//! THE QUEUE (TH6a, schema 4, `migrations/0004_send.sql`): the rows of `send_task` and `send_receiver`, the zones that
//! move them, what may run next, the two budgets and the sweep. Nothing here seals, opens or touches a socket: a zone
//! takes bytes that are sealed already (the nonces already hashed) and commits them, and the frames go after the
//! commit.
//!
//! ```text
//!               ZONE PREPARE                 (no reflect: starts at `reflected`)
//!  (Transmit | job) ──────────────────▶ prepared
//!     prepared  ─ reflect-ack ─ ZONE REFLECTED ─▶ reflected
//!     reflected ─ box acks ─ ZONE ACKS ─▶ reflected (some pending)
//!                                      ─▶ acked     (none pending, has_update: the sealed update and its nonce)
//!                                      ─▶ done      (none pending, no update; a job's row is DELETED)
//!     acked ─ update's reflect-ack ─ ZONE DONE ─▶ done (sealed columns NULL; a job's row is DELETED)
//! ```
//!
//! THE ZONES. Every multi-row write is ONE [`db::zone`] with its failpoints; a kill leaves the state before or the
//! complete state after (`tests/kill.rs`). [`prepare_send`] takes the caller's zone, so a later step can enqueue a job
//! inside the zone that received its cause.
//!
//! THE BUDGETS. Two on the same columns: an un-attached `transmit` row (deleted by the user, or its core budget
//! ended) must never stop a `job`, whose zone is the receive path's. `bytes` is what a row HOLDS: every zone that
//! empties a BLOB lowers it by the lengths it nulls, and the update's write raises it ([`refresh_bytes`]). A row over its
//! budget is refused with [`StoreError::QueueFull`] and the caller's zone rolls back; only a `transmit` row's PREPARE may
//! first evict the oldest `transmit` rows nothing owes any more ([`EVICT_AFTER_MS`]), in the same zone.
//!
//! Nothing in a real build calls these yet, so each stays reachable as a doc-hidden door in `store` (rule 13).

use std::fmt;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use kourier_db::rusqlite::{Connection, OptionalExtension as _, Row, params};
use kourier_db::{Tx, ZoneDb};

use crate::db::{self, Points};
use crate::error::{Result, StoreError, sql};
use crate::failpoints as fp;
use crate::store::{NonceScope, add_nonces, from_sql_u64, to_sql_u64};

const PREPARE_POINTS: Points = Points {
    inside: fp::INSIDE_SEND_PREPARE,
    after: fp::AFTER_SEND_PREPARE,
};

const REFLECTED_POINTS: Points = Points {
    inside: fp::INSIDE_SEND_REFLECTED,
    after: fp::AFTER_SEND_REFLECTED,
};

const ACKS_POINTS: Points = Points {
    inside: fp::INSIDE_SEND_ACKS,
    after: fp::AFTER_SEND_ACKS,
};

const DONE_POINTS: Points = Points {
    inside: fp::INSIDE_SEND_DONE,
    after: fp::AFTER_SEND_DONE,
};

const SWEEP_POINTS: Points = Points {
    inside: fp::INSIDE_SEND_SWEEP,
    after: fp::AFTER_SEND_SWEEP,
};

const GIVE_UP_POINTS: Points = Points {
    inside: fp::INSIDE_SEND_GIVE_UP,
    after: fp::AFTER_SEND_GIVE_UP,
};

/// The rule name of the WARN line of a row its budget refused (one line per refused zone, never a row's content).
pub const RULE_QUEUE_FULL: &str = "send-queue-full";
/// The rule name of the WARN line of the `transmit` rows a PREPARE evicted to make room, with their count.
pub const RULE_TRANSMIT_EVICTED: &str = "send-transmit-evicted";
/// The rule name of the WARN line of the sweep's open `job` rows, with their count.
pub const RULE_JOB_EXPIRED: &str = "send-job-expired";
/// The rule name of the WARN line of the sweep's open `transmit` rows, with their count.
pub const RULE_TRANSMIT_EXPIRED: &str = "send-transmit-expired";
/// The rule name of the WARN line of a job given up after its failed runs.
pub const RULE_JOB_GIVEN_UP: &str = "send-job-given-up";

/// What one origin's open rows may hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendBudget {
    /// The most open rows.
    pub tasks: i64,
    /// The most bytes those rows hold (`reflect_env`, `update_env` and the boxes still pending).
    pub bytes: i64,
}

impl SendBudget {
    /// Whether a row of `bytes` is taken beside `tasks` open rows holding `held` bytes.
    fn takes(self, tasks: i64, held: i64, bytes: i64) -> bool {
        tasks < self.tasks && held.saturating_add(bytes) <= self.bytes
    }
}

/// The budget of `transmit` rows; only a `Transmit` is refused by it.
pub const TRANSMIT_BUDGET: SendBudget = SendBudget {
    tasks: 2_000,
    bytes: 64 * 1024 * 1024,
};

/// The budget of `job` rows; only a job is refused by it.
pub const JOB_BUDGET: SendBudget = SendBudget {
    tasks: 2_000,
    bytes: 16 * 1024 * 1024,
};

/// How old an un-attached `prepared` or `reflected` `transmit` row must be before a PREPARE that finds the `transmit`
/// budget spent may take it (24 hours of the local clock, `queued_at`). It is the core's own budget for a message: the
/// core fails a message it has not sent within 24 hours, so the row no longer has an owner, and a later Retry of the
/// same message prepares anew under the same id (limit 2 of THE SEND CORE).
pub const EVICT_AFTER_MS: i64 = 24 * 60 * 60 * 1000;
/// A `done` row is the ledger's for 14 days.
const DONE_KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// An open `transmit` row stands for 30 days (the core's own budget ends long before).
const TRANSMIT_KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// An open `job` row stands for 14 days.
const JOB_KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// The `origin` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOrigin {
    /// The core's outbox owns the row.
    Transmit,
    /// The backend's own: a delivery receipt, an FS reject, a creator's answer.
    Job,
}

impl SendOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::Transmit => "transmit",
            Self::Job => "job",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "transmit" => Some(Self::Transmit),
            "job" => Some(Self::Job),
            _ => None,
        }
    }

    fn budget(self) -> SendBudget {
        match self {
            Self::Transmit => TRANSMIT_BUDGET,
            Self::Job => JOB_BUDGET,
        }
    }
}

/// The `state` column of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendState {
    /// PREPARE committed; the reflect is owed.
    Prepared,
    /// The reflect-ack is durable, or the type does not reflect; boxes are owed.
    Reflected,
    /// Every box is durable; the device group's update is owed.
    Acked,
    /// Nothing is owed; the row is the ledger.
    Done,
}

impl SendState {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "prepared" => Some(Self::Prepared),
            "reflected" => Some(Self::Reflected),
            "acked" => Some(Self::Acked),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// The `state` column of a receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverState {
    /// Its box is owed.
    Pending,
    /// The chat server's message-ack is durable.
    Acked,
    /// No box: the Identity Blocked Steps blocked it.
    Blocked,
    /// No box: the ID is invalid or revoked.
    Invalid,
    /// No box: the file holds no key of it.
    NoKey,
    /// No box: left out for this type by the receiver's features.
    Omitted,
}

impl ReceiverState {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "acked" => Some(Self::Acked),
            "blocked" => Some(Self::Blocked),
            "invalid" => Some(Self::Invalid),
            "no-key" => Some(Self::NoKey),
            "omitted" => Some(Self::Omitted),
            _ => None,
        }
    }
}

/// Whose row a new task is.
#[derive(Clone, PartialEq, Eq)]
pub enum NewOrigin {
    /// The core's: `local` is `msg_id`'s text form of the message id, the ledger's key.
    Transmit {
        /// The 16 lowercase hex digits (the schema's CHECK).
        local: String,
    },
    /// The backend's own.
    Job,
}

/// What a new receiver gets.
#[derive(Clone, PartialEq, Eq)]
pub enum NewFate {
    /// The whole box as the chat server gets it, its nonce inside: 88 to 8,170 bytes (the schema's CHECK).
    Send {
        /// The sealed box.
        payload: Vec<u8>,
    },
    /// See [`ReceiverState::Blocked`].
    Blocked,
    /// See [`ReceiverState::Invalid`].
    Invalid,
    /// See [`ReceiverState::NoKey`].
    NoKey,
    /// See [`ReceiverState::Omitted`].
    Omitted,
}

impl NewFate {
    fn state(&self) -> &'static str {
        match self {
            Self::Send { .. } => "pending",
            Self::Blocked => "blocked",
            Self::Invalid => "invalid",
            Self::NoKey => "no-key",
            Self::Omitted => "omitted",
        }
    }

    fn payload(&self) -> Option<&[u8]> {
        match self {
            Self::Send { payload } => Some(payload),
            Self::Blocked | Self::Invalid | Self::NoKey | Self::Omitted => None,
        }
    }
}

/// One receiver of a new task.
#[derive(Clone, PartialEq, Eq)]
pub struct NewReceiver {
    /// The Threema ID (8 characters; personal data).
    pub identity: String,
    /// What it gets.
    pub fate: NewFate,
}

/// ALL A PREPARE WRITES, sealed already. `Debug` prints counts.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedRows {
    /// Whose row it is.
    pub origin: NewOrigin,
    /// A short name of the code's: `[a-z0-9-]`, 1 to 32 characters (the schema's CHECK).
    pub kind: &'static str,
    /// `MessageId.0`.
    pub message_id: u64,
    /// The prost bytes of the `d2d.ConversationId`, 1 to 256 bytes.
    pub conversation: Vec<u8>,
    /// The message's own time (ms).
    pub created_at: i64,
    /// Whether the end of the boxes owes the device group a Sent update.
    pub has_update: bool,
    /// When this zone runs (ms, the local clock).
    pub queued_at: i64,
    /// The sealed `OutgoingMessage`, 40 to 65,516 bytes; `None` for a type that does not reflect: the row starts at
    /// `reflected`.
    pub reflect_env: Option<Vec<u8>>,
    /// Every receiver, with what it gets.
    pub receivers: Vec<NewReceiver>,
    /// The hash of every CSP nonce a box here uses, written in this zone: before its first use.
    pub csp_nonces: Vec<[u8; 32]>,
    /// The hash of the D2D nonce of `reflect_env`, written in this zone: before its first use.
    pub d2d_nonces: Vec<[u8; 32]>,
}

impl SealedRows {
    /// The `bytes` column: `reflect_env` and every payload.
    fn bytes(&self) -> usize {
        self.receivers
            .iter()
            .filter_map(|r| r.fate.payload().map(<[u8]>::len))
            .sum::<usize>()
            + self.reflect_env.as_ref().map_or(0, Vec::len)
    }
}

impl fmt::Debug for SealedRows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedRows")
            .field("kind", &self.kind)
            .field("has_update", &self.has_update)
            .field("receivers", &self.receivers.len())
            .field("csp_nonces", &self.csp_nonces.len())
            .field("d2d_nonces", &self.d2d_nonces.len())
            .finish_non_exhaustive()
    }
}

/// A stored task. `Debug` prints no ID, no id of a message, no conversation and no sealed byte.
#[derive(Clone, PartialEq, Eq)]
pub struct SendTask {
    /// Queue order, never reused.
    pub id: i64,
    /// Whose row it is.
    pub origin: SendOrigin,
    /// The code's short name.
    pub kind: String,
    /// The ledger's key; `Some` exactly for a `transmit` row.
    pub local: Option<String>,
    /// `MessageId.0`.
    pub message_id: u64,
    /// The prost bytes of the `d2d.ConversationId`.
    pub conversation: Vec<u8>,
    /// The message's own time (ms).
    pub created_at: i64,
    /// Where it stands.
    pub state: SendState,
    /// Whether the end of the boxes owes the device group a Sent update.
    pub has_update: bool,
    /// The update's reflect-ack time (ms): `TransmitOutcome::Sent.server_at`.
    pub sent_at: Option<i64>,
    /// What the row holds: `reflect_env`, `update_env` and every payload still pending (the budgets' sum).
    pub bytes: i64,
    /// The failed runs ([`count_failed_run`]).
    pub attempts: u32,
    /// When PREPARE ran (ms, the local clock).
    pub queued_at: i64,
    /// When the row reached `done` (ms, the local clock).
    pub finished_at: Option<i64>,
    /// The sealed `OutgoingMessage`, while its reflect-ack is not durable.
    pub reflect_env: Option<Vec<u8>>,
    /// The sealed `OutgoingMessageUpdate`, while its reflect-ack is not durable.
    pub update_env: Option<Vec<u8>>,
}

impl fmt::Debug for SendTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendTask")
            .field("id", &self.id)
            .field("origin", &self.origin)
            .field("kind", &self.kind)
            .field("state", &self.state)
            .field("has_update", &self.has_update)
            .field("bytes", &self.bytes)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

/// A stored receiver. `Debug` prints its state alone.
#[derive(Clone, PartialEq, Eq)]
pub struct SendReceiver {
    /// The Threema ID (personal data).
    pub identity: String,
    /// Where it stands.
    pub state: ReceiverState,
    /// The sealed box, exactly while it is [`ReceiverState::Pending`].
    pub payload: Option<Vec<u8>>,
}

impl fmt::Debug for SendReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendReceiver")
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

/// The sealed update the last ACKS zone writes.
#[derive(Clone, Copy)]
pub struct SealedUpdate<'a> {
    /// The sealed `OutgoingMessageUpdate`, 40 to 65,516 bytes.
    pub envelope: &'a [u8],
    /// The hash of its D2D nonce, written in the same zone: before its first use.
    pub nonce: [u8; 32],
}

/// Where a zone left the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendEnd {
    /// Receivers are still pending: the row stays `reflected`.
    Open {
        /// How many.
        pending: u32,
    },
    /// Every box is durable and the sealed update is written: `acked`.
    Acked,
    /// Nothing is owed: `done`, the ledger's row.
    Done,
    /// Nothing is owed and the row was a job's: it is gone, with its receivers.
    Deleted,
}

/// What may run now, for [`next_send`].
#[derive(Debug, Clone, Copy)]
pub struct Runnable<'a> {
    /// The tasks a `Transmit` is attached to.
    pub attached: &'a [i64],
    /// The tasks that are cooling: none of them runs, whatever else holds.
    pub cooling: &'a [i64],
    /// Whether the account is `Online` (a job's gate).
    pub online: bool,
}

/// What [`sweep_sends`] deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    /// `done` rows past 14 days: the ledger's forgetting, quiet.
    pub done: u32,
    /// Open `transmit` rows past 30 days ([`RULE_TRANSMIT_EXPIRED`]).
    pub transmit: u32,
    /// Open `job` rows past 14 days ([`RULE_JOB_EXPIRED`]).
    pub jobs: u32,
}

/// The zones a test may make fail, by their `INSIDE` points.
const FAILABLE: [&str; 6] = [
    fp::INSIDE_SEND_PREPARE,
    fp::INSIDE_SEND_REFLECTED,
    fp::INSIDE_SEND_ACKS,
    fp::INSIDE_SEND_DONE,
    fp::INSIDE_SEND_SWEEP,
    fp::INSIDE_SEND_GIVE_UP,
];
/// The test door's zone ([`fail_send_zone`]): one more than its place in [`FAILABLE`], 0 for none.
static FAIL_ZONE: AtomicUsize = AtomicUsize::new(0);
/// The SQLite extended result code the door's zone fails with.
static FAIL_CODE: AtomicI32 = AtomicI32::new(0);

/// Makes the send zone of `point` (its `INSIDE_*` failpoint) fail with the SQLite extended result code `code`, after
/// its writes and before its commit, every time: both kinds of failure can be tested (a content code such as 275,
/// `SQLITE_CONSTRAINT_CHECK`, or the store's such as 13, `SQLITE_FULL`). An empty `point` turns it off; every call
/// replaces the one before. Nothing in the application calls it.
///
/// # Panics
///
/// With a `point` that is no send zone's (a test's mistake).
#[doc(hidden)]
pub fn fail_send_zone(point: &str, code: i32) {
    let zone = FAILABLE
        .iter()
        .position(|z| *z == point)
        .map_or(0, |at| at + 1);
    assert!(
        zone != 0 || point.is_empty(),
        "the door takes the INSIDE point of a send zone"
    );
    // The code first: a reader that sees the zone sees its code.
    FAIL_CODE.store(code, Ordering::SeqCst);
    FAIL_ZONE.store(zone, Ordering::SeqCst);
}

/// The door's failure of the zone of `point`, if it is the one set.
fn door_error(point: &str) -> Option<StoreError> {
    let zone = FAIL_ZONE.load(Ordering::SeqCst);
    (zone != 0 && FAILABLE[zone - 1] == point).then(|| StoreError::Sql {
        op: "a send zone failed",
        code: Some(FAIL_CODE.load(Ordering::SeqCst)),
    })
}

/// ONE send zone: [`db::zone`] with the test door behind `f`'s writes.
fn send_zone<T>(
    db: &ZoneDb,
    points: Points,
    f: impl FnOnce(&mut Tx<'_>) -> Result<T>,
) -> Result<T> {
    db::zone(db, points, |tx| {
        let out = f(tx)?;
        door_error(points.inside).map_or(Ok(out), Err)
    })
}

/// The budget's look at one origin's open rows: a walk of the covering `send_task_open` (the four short columns it names;
/// no BLOB and no table row is read).
const OPEN_OF_ORIGIN: &str =
    "SELECT count(*), coalesce(sum(bytes), 0) FROM send_task WHERE state <> 'done' AND origin = ?1";

/// The eviction's search: the `transmit` rows nothing owes any more, oldest first (`queued_at`, then `id`). `state <> 'done'`
/// is what lets SQLite use `send_task_open` (it cannot prove that the `IN` implies it), and the index covers every column
/// named here, so no row of the table is read; the order is one sort of what is found.
const EVICTABLE: &str = "SELECT id, bytes FROM send_task WHERE state <> 'done' AND origin = 'transmit' \
     AND state IN ('prepared', 'reflected') AND queued_at <= ?1 \
     AND id NOT IN (SELECT value FROM json_each(?2)) ORDER BY queued_at, id";

/// `attached` as the JSON array `json_each` reads (the clear's form: one bound text, no statement per length of list).
fn attached_json(attached: &[i64]) -> String {
    let ids = attached
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    format!("[{ids}]")
}

/// What one origin's open rows hold: how many, and the sum of their `bytes`.
fn open_of(conn: &Connection, origin: SendOrigin) -> Result<(i64, i64)> {
    conn.prepare_cached(OPEN_OF_ORIGIN)
        .and_then(|mut stmt| {
            stmt.query_row(params![origin.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        })
        .map_err(sql("read the send budget failed"))
}

/// EVICTION UNDER PRESSURE (decision S-M1): the `transmit` budget is spent, so the oldest `transmit` rows in `prepared` or
/// `reflected` that no `Transmit` is attached to and that are at least [`EVICT_AFTER_MS`] old go, with their receivers
/// (the foreign key's cascade), one or more until a row of `bytes` fits. Never an `acked` row (it finishes by itself),
/// a `done` row, an attached row (in flight) or a job. Plans the rows from the sums first and deletes nothing when even all
/// of them would not make room. Returns how many rows went.
fn evict_for_room(
    tx: &Tx<'_>,
    budget: SendBudget,
    (tasks, held): (i64, i64),
    bytes: i64,
    now_ms: i64,
    attached: &[i64],
) -> Result<usize> {
    const OP: &str = "evict the old send rows failed";
    let mut chosen = Vec::new();
    {
        let mut stmt = tx.prepare_cached(EVICTABLE).map_err(sql(OP))?;
        let mut rows = stmt
            .query(params![
                now_ms.saturating_sub(EVICT_AFTER_MS),
                attached_json(attached)
            ])
            .map_err(sql(OP))?;
        let (mut tasks, mut held) = (tasks, held);
        while !budget.takes(tasks, held, bytes) {
            let Some(r) = rows.next().map_err(sql(OP))? else {
                return Ok(0);
            };
            chosen.push(r.get::<_, i64>(0).map_err(sql(OP))?);
            tasks -= 1;
            held = held.saturating_sub(r.get::<_, i64>(1).map_err(sql(OP))?);
        }
    }
    let mut delete = tx
        .prepare_cached("DELETE FROM send_task WHERE id = ?1")
        .map_err(sql(OP))?;
    for id in &chosen {
        delete.execute(params![id]).map_err(sql(OP))?;
    }
    Ok(chosen.len())
}

/// Refuses a row of `bytes` that does not fit `origin`'s budget. A `transmit` row first tries
/// [`evict_for_room`] in the same zone and looks again at what the file holds; one WARN by
/// [`RULE_TRANSMIT_EVICTED`] with the count goes out when the zone commits.
fn check_budget(
    tx: &mut Tx<'_>,
    origin: SendOrigin,
    bytes: i64,
    now_ms: i64,
    attached: &[i64],
) -> Result<()> {
    let budget = origin.budget();
    let (tasks, held) = open_of(tx, origin)?;
    if budget.takes(tasks, held, bytes) {
        return Ok(());
    }
    if origin == SendOrigin::Transmit {
        let evicted = evict_for_room(tx, budget, (tasks, held), bytes, now_ms, attached)?;
        if evicted > 0 {
            let (now_tasks, now_held) = open_of(tx, origin)?;
            if budget.takes(now_tasks, now_held, bytes) {
                tx.on_commit(move || {
                    tracing::warn!(
                        "send rows evicted to make room: {evicted} by {RULE_TRANSMIT_EVICTED}"
                    );
                });
                return Ok(());
            }
            // The plan said there was room and the file says there is none: refuse, and the caller's zone takes the
            // deletes back with it.
        }
    }
    tracing::warn!(
        "a send row refused by {RULE_QUEUE_FULL}: {tasks} {} rows open",
        origin.as_str()
    );
    Err(StoreError::QueueFull)
}

/// THE ROWS OF ONE PREPARE inside the caller's zone: the budget's check, the task (`prepared`, or `reflected` for a
/// type that does not reflect), its receivers and the hashed nonces of both stores. Returns the task's id. A plain
/// `INSERT`, never a replace: the caller read the ledger, and a `local` the file already holds fails by
/// `send_by_local`. A row the schema refuses fails the zone by name and names no input; a row over its budget fails it
/// with [`StoreError::QueueFull`]. `attached` are the tasks a `Transmit` is attached to: a `transmit` row whose budget is
/// spent may evict old rows in this zone (S-M1, [`EVICT_AFTER_MS`]) and never one of them; a job evicts nothing, and
/// passes none.
#[doc(hidden)]
pub fn prepare_send(tx: &mut Tx<'_>, rows: &SealedRows, attached: &[i64]) -> Result<i64> {
    const OP: &str = "prepare a send failed";
    let bytes = i64::try_from(rows.bytes()).map_err(|_| StoreError::Failed(OP))?;
    let (origin, local) = match &rows.origin {
        NewOrigin::Transmit { local } => (SendOrigin::Transmit, Some(local.as_str())),
        NewOrigin::Job => (SendOrigin::Job, None),
    };
    check_budget(tx, origin, bytes, rows.queued_at, attached)?;
    let state = if rows.reflect_env.is_some() {
        "prepared"
    } else {
        "reflected"
    };
    tx.execute(
        "INSERT INTO send_task (origin, kind, local, message_id, conversation, created_at, state, \
           has_update, bytes, queued_at, reflect_env) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            origin.as_str(),
            rows.kind,
            local,
            to_sql_u64(rows.message_id),
            rows.conversation,
            rows.created_at,
            state,
            rows.has_update,
            bytes,
            rows.queued_at,
            rows.reflect_env,
        ],
    )
    .map_err(sql(OP))?;
    let task = tx.last_insert_rowid();
    let mut receiver = tx
        .prepare_cached(
            "INSERT INTO send_receiver (task, identity, state, payload) VALUES (?1, ?2, ?3, ?4)",
        )
        .map_err(sql(OP))?;
    for r in &rows.receivers {
        receiver
            .execute(params![task, r.identity, r.fate.state(), r.fate.payload()])
            .map_err(sql(OP))?;
    }
    add_nonces(tx, NonceScope::Csp, &rows.csp_nonces)?;
    add_nonces(tx, NonceScope::D2d, &rows.d2d_nonces)?;
    Ok(task)
}

/// ZONE PREPARE: [`prepare_send`] as ONE zone (`INSIDE_SEND_PREPARE` / `AFTER_SEND_PREPARE`). Its commit is the point
/// of no return; an eviction of S-M1 is in the same zone, so the evicted rows and the new one commit together or not at
/// all.
#[doc(hidden)]
pub fn prepare_send_zone(db: &ZoneDb, rows: &SealedRows, attached: &[i64]) -> Result<i64> {
    send_zone(db, PREPARE_POINTS, |tx| prepare_send(tx, rows, attached))
}

/// ZONE PREPARE around the seal: `seal` runs in the zone and sees its view of the nonce stores, and nothing commits
/// unless it and the rows it makes hold. (A refused row has been sealed by then: the budget answers after the seal, CPU
/// only: a limit of THE SEND CORE.)
pub(crate) fn prepare_sealed_zone(
    db: &ZoneDb,
    attached: &[i64],
    seal: impl FnOnce(&Tx<'_>) -> Result<SealedRows>,
) -> Result<i64> {
    send_zone(db, PREPARE_POINTS, |tx| {
        let rows = seal(tx)?;
        prepare_send(tx, &rows, attached)
    })
}

/// THE ROW'S `bytes` BY WHAT IT HOLDS (S-M1 (a)): `reflect_env`, `update_env` and every receiver's box, from the lengths
/// stored in the file, in the same zone as the write that emptied or filled a BLOB (SQLite reads the length of a BLOB
/// from its record, not its pages). Every zone that nulls a BLOB or writes the update ends with it, so after each zone
/// `bytes = length(reflect_env) + length(update_env) + sum(length(payload))` with a NULL counting 0, and the budget's sum
/// counts what the rows hold and not what they once held. Fails a row that is not there.
fn refresh_bytes(tx: &Tx<'_>, task: i64) -> Result<()> {
    const OP: &str = "refresh a send row's bytes failed";
    let changed = tx
        .execute(
            "UPDATE send_task SET bytes = coalesce(length(reflect_env), 0) + coalesce(length(update_env), 0) \
               + (SELECT coalesce(sum(length(payload)), 0) FROM send_receiver WHERE task = send_task.id) \
             WHERE id = ?1",
            params![task],
        )
        .map_err(sql(OP))?;
    if changed != 1 {
        return Err(StoreError::Failed(OP));
    }
    Ok(())
}

/// ZONE REFLECTED (`INSIDE_SEND_REFLECTED` / `AFTER_SEND_REFLECTED`): the reflect-ack is durable, the row is
/// `reflected` and its sealed reflect is gone, and its `bytes` lower by that reflect. A row that is not `prepared` fails
/// the zone.
#[doc(hidden)]
pub fn mark_send_reflected(db: &ZoneDb, task: i64) -> Result<()> {
    const OP: &str = "mark a send reflected failed";
    send_zone(db, REFLECTED_POINTS, |tx| {
        let changed = tx
            .execute(
                "UPDATE send_task SET state = 'reflected', reflect_env = NULL \
                 WHERE id = ?1 AND state = 'prepared'",
                params![task],
            )
            .map_err(sql(OP))?;
        if changed != 1 {
            return Err(StoreError::Failed(OP));
        }
        refresh_bytes(tx, task)
    })
}

/// The receivers of one pass: `pending` -> `acked`, their boxes emptied. One that is not pending stays as it is.
fn ack_receivers(tx: &Tx<'_>, task: i64, acked: &[&str]) -> Result<()> {
    const OP: &str = "mark send acks failed";
    let mut ack = tx
        .prepare_cached(
            "UPDATE send_receiver SET state = 'acked', payload = NULL \
             WHERE task = ?1 AND identity = ?2 AND state = 'pending'",
        )
        .map_err(sql(OP))?;
    for identity in acked {
        ack.execute(params![task, identity]).map_err(sql(OP))?;
    }
    Ok(())
}

/// Where the pass leaves the row: still `reflected` while a receiver is pending; else `acked` with the sealed update
/// and its nonce, or, with no update owed, `done` (a `transmit` row) or gone (a job's). A row that is not `reflected`
/// fails, and so does an owed update that is missing.
fn end_of_acks(
    tx: &Tx<'_>,
    task: i64,
    update: Option<SealedUpdate<'_>>,
    now_ms: i64,
) -> Result<SendEnd> {
    const OP: &str = "mark send acks failed";
    let row: Option<(String, String, bool)> = tx
        .query_row(
            "SELECT origin, state, has_update FROM send_task WHERE id = ?1",
            params![task],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(sql(OP))?;
    let Some((origin, state, has_update)) = row else {
        return Err(StoreError::Failed(OP));
    };
    if SendState::parse(&state) != Some(SendState::Reflected) {
        return Err(StoreError::Failed(OP));
    }
    let pending: u32 = tx
        .query_row(
            "SELECT count(*) FROM send_receiver WHERE task = ?1 AND state = 'pending'",
            params![task],
            |r| r.get(0),
        )
        .map_err(sql(OP))?;
    if pending > 0 {
        return Ok(SendEnd::Open { pending });
    }
    if has_update {
        let update = update.ok_or(StoreError::Failed(OP))?;
        tx.execute(
            "UPDATE send_task SET state = 'acked', update_env = ?2 WHERE id = ?1",
            params![task, update.envelope],
        )
        .map_err(sql(OP))?;
        add_nonces(tx, NonceScope::D2d, &[update.nonce])?;
        return Ok(SendEnd::Acked);
    }
    match SendOrigin::parse(&origin) {
        Some(SendOrigin::Job) => {
            tx.execute("DELETE FROM send_task WHERE id = ?1", params![task])
                .map_err(sql(OP))?;
            Ok(SendEnd::Deleted)
        }
        Some(SendOrigin::Transmit) => {
            tx.execute(
                "UPDATE send_task SET state = 'done', finished_at = ?2 WHERE id = ?1",
                params![task, now_ms],
            )
            .map_err(sql(OP))?;
            Ok(SendEnd::Done)
        }
        None => Err(StoreError::Unreadable("a send row unreadable")),
    }
}

/// ZONE ACKS (`INSIDE_SEND_ACKS` / `AFTER_SEND_ACKS`), once per pass: the acks of `acked` and where they leave the row
/// ([`SendEnd`]), together. `acked` may be empty: a notes group, or a row whose kill fell between REFLECTED and this
/// zone with nothing pending. `update` is the sealed `Sent` update; it is written only by the pass that leaves nothing
/// pending, and its absence then fails the zone for a row that owes one. The row's `bytes` falls by the acked boxes and
/// rises by the update ([`refresh_bytes`]).
#[doc(hidden)]
pub fn mark_send_acks(
    db: &ZoneDb,
    task: i64,
    acked: &[&str],
    update: Option<SealedUpdate<'_>>,
    now_ms: i64,
) -> Result<SendEnd> {
    send_zone(db, ACKS_POINTS, |tx| {
        ack_receivers(tx, task, acked)?;
        let end = end_of_acks(tx, task, update, now_ms)?;
        if end != SendEnd::Deleted {
            refresh_bytes(tx, task)?;
        }
        Ok(end)
    })
}

/// ZONE DONE (`INSIDE_SEND_DONE` / `AFTER_SEND_DONE`): the update's reflect-ack is durable. A `transmit` row is `done`
/// with `sent_at` (the ack's time, the core's `server_at`) and its sealed columns gone; a job's row is deleted with
/// its receivers. A row that is not `acked` fails the zone. A `done` row holds nothing: its `bytes` is 0.
#[doc(hidden)]
pub fn mark_send_done(db: &ZoneDb, task: i64, sent_at: i64, now_ms: i64) -> Result<SendEnd> {
    const OP: &str = "finish a send failed";
    send_zone(db, DONE_POINTS, |tx| {
        let gone = tx
            .execute(
                "DELETE FROM send_task WHERE id = ?1 AND state = 'acked' AND origin = 'job'",
                params![task],
            )
            .map_err(sql(OP))?;
        if gone == 1 {
            return Ok(SendEnd::Deleted);
        }
        let changed = tx
            .execute(
                "UPDATE send_task SET state = 'done', reflect_env = NULL, update_env = NULL, \
                   sent_at = ?2, finished_at = ?3 \
                 WHERE id = ?1 AND state = 'acked' AND origin = 'transmit'",
                params![task, sent_at, now_ms],
            )
            .map_err(sql(OP))?;
        if changed != 1 {
            return Err(StoreError::Failed(OP));
        }
        refresh_bytes(tx, task)?;
        Ok(SendEnd::Done)
    })
}

/// One more FAILED RUN of `task` (the socket ended by a missing ack or by the server's close with frames of the row
/// unanswered), and the count so far. One statement, so no failpoint pair: a restart loses a row's cooling, not this
/// count. A row that is not there fails.
#[doc(hidden)]
pub fn count_failed_run(db: &ZoneDb, task: i64) -> Result<u32> {
    const OP: &str = "count a failed send run failed";
    db.zone(|tx| {
        tx.query_row(
            "UPDATE send_task SET attempts = attempts + 1 WHERE id = ?1 RETURNING attempts",
            params![task],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql(OP))?
        .ok_or(StoreError::Failed(OP))
    })
}

/// THE GIVE-UP of a job (decision R2), as ONE zone (`INSIDE_SEND_GIVE_UP` / `AFTER_SEND_GIVE_UP`): the row and, by its
/// cascade, its receivers go, whatever its state; one WARN by [`RULE_JOB_GIVEN_UP`] at the commit. The nonces stay, they
/// are permanent. A `transmit` row, or a row that is not there, fails: the core owns a `transmit` row.
#[doc(hidden)]
pub fn give_up_send_job(db: &ZoneDb, task: i64) -> Result<()> {
    const OP: &str = "give up a send job failed";
    send_zone(db, GIVE_UP_POINTS, |tx| {
        let changed = tx
            .execute(
                "DELETE FROM send_task WHERE id = ?1 AND origin = 'job'",
                params![task],
            )
            .map_err(sql(OP))?;
        if changed != 1 {
            return Err(StoreError::Failed(OP));
        }
        tx.on_commit(|| tracing::warn!("send rows given up: 1 by {RULE_JOB_GIVEN_UP}"));
        Ok(())
    })
}

const TASK_COLUMNS: &str = "id, origin, kind, local, message_id, conversation, created_at, state, \
    has_update, sent_at, bytes, attempts, queued_at, finished_at, reflect_env, update_env";

/// A row of [`TASK_COLUMNS`].
fn task_of(r: &Row<'_>) -> Result<SendTask> {
    const OP: &str = "read a send row failed";
    const BAD: StoreError = StoreError::Unreadable("a send row unreadable");
    let origin: String = r.get(1).map_err(sql(OP))?;
    let state: String = r.get(7).map_err(sql(OP))?;
    Ok(SendTask {
        id: r.get(0).map_err(sql(OP))?,
        origin: SendOrigin::parse(&origin).ok_or(BAD)?,
        kind: r.get(2).map_err(sql(OP))?,
        local: r.get(3).map_err(sql(OP))?,
        message_id: from_sql_u64(r.get(4).map_err(sql(OP))?),
        conversation: r.get(5).map_err(sql(OP))?,
        created_at: r.get(6).map_err(sql(OP))?,
        state: SendState::parse(&state).ok_or(BAD)?,
        has_update: r.get(8).map_err(sql(OP))?,
        sent_at: r.get(9).map_err(sql(OP))?,
        bytes: r.get(10).map_err(sql(OP))?,
        attempts: r.get(11).map_err(sql(OP))?,
        queued_at: r.get(12).map_err(sql(OP))?,
        finished_at: r.get(13).map_err(sql(OP))?,
        reflect_env: r.get(14).map_err(sql(OP))?,
        update_env: r.get(15).map_err(sql(OP))?,
    })
}

/// The one row `tail` selects (`WHERE ..` over [`TASK_COLUMNS`], one parameter).
fn one_task(
    conn: &Connection,
    tail: &str,
    value: impl kourier_db::rusqlite::ToSql,
) -> Result<Option<SendTask>> {
    const OP: &str = "read a send row failed";
    let mut stmt = conn
        .prepare_cached(&format!("SELECT {TASK_COLUMNS} FROM send_task {tail}"))
        .map_err(sql(OP))?;
    let mut rows = stmt.query(params![value]).map_err(sql(OP))?;
    rows.next().map_err(sql(OP))?.map(task_of).transpose()
}

/// The task `id`.
#[doc(hidden)]
pub fn send_task(conn: &Connection, id: i64) -> Result<Option<SendTask>> {
    one_task(conn, "WHERE id = ?1", id)
}

/// THE LEDGER'S READ: the task of `local`, whatever its state (one search of `send_by_local`).
#[doc(hidden)]
pub fn send_ledger(conn: &Connection, local: &str) -> Result<Option<SendTask>> {
    one_task(conn, "WHERE local = ?1", local)
}

/// Every task, in queue order, with its sealed bytes.
#[doc(hidden)]
pub fn send_tasks(conn: &Connection) -> Result<Vec<SendTask>> {
    const OP: &str = "read the send rows failed";
    let mut stmt = conn
        .prepare_cached(&format!("SELECT {TASK_COLUMNS} FROM send_task ORDER BY id"))
        .map_err(sql(OP))?;
    let mut rows = stmt.query([]).map_err(sql(OP))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql(OP))? {
        out.push(task_of(r)?);
    }
    Ok(out)
}

/// The look of [`send_receiver_states`]: two short columns, the `payload` BLOB not named.
const RECEIVER_STATES: &str =
    "SELECT identity, state FROM send_receiver WHERE task = ?1 ORDER BY identity";

/// The receivers of `task`, by ID, with their sealed boxes.
#[doc(hidden)]
pub fn send_receivers(conn: &Connection, task: i64) -> Result<Vec<SendReceiver>> {
    const OP: &str = "read the send receivers failed";
    let mut stmt = conn
        .prepare_cached(
            "SELECT identity, state, payload FROM send_receiver WHERE task = ?1 ORDER BY identity",
        )
        .map_err(sql(OP))?;
    let mut rows = stmt.query(params![task]).map_err(sql(OP))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql(OP))? {
        let state: String = r.get(1).map_err(sql(OP))?;
        out.push(SendReceiver {
            identity: r.get(0).map_err(sql(OP))?,
            state: ReceiverState::parse(&state)
                .ok_or(StoreError::Unreadable("a send row unreadable"))?,
            payload: r.get(2).map_err(sql(OP))?,
        });
    }
    Ok(out)
}

/// The receivers of `task` by ID and state, with no box: what the answers to the core read (S-L1: a row of 256 receivers
/// all pending holds about 2 MiB of boxes an answer does not look at). Where a box is needed, [`send_receivers`].
pub(crate) fn send_receiver_states(
    conn: &Connection,
    task: i64,
) -> Result<Vec<(String, ReceiverState)>> {
    const OP: &str = "read the send receiver states failed";
    let mut stmt = conn.prepare_cached(RECEIVER_STATES).map_err(sql(OP))?;
    let mut rows = stmt.query(params![task]).map_err(sql(OP))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(sql(OP))? {
        let state: String = r.get(1).map_err(sql(OP))?;
        out.push((
            r.get(0).map_err(sql(OP))?,
            ReceiverState::parse(&state).ok_or(StoreError::Unreadable("a send row unreadable"))?,
        ));
    }
    Ok(out)
}

/// The walk of [`next_send`]: the open rows in queue order, their short columns only (`send_task_open` covers it: a walk
/// of index pages, no table row).
const OPEN_IN_ORDER: &str =
    "SELECT id, origin, state FROM send_task WHERE state <> 'done' ORDER BY id";

/// THE NEXT ROW TO RUN, one at a time per account: the first in queue order that is not cooling and that
/// - is `acked`, attached or not, a job's too: every box is durable and only the device group's update is owed, so the
///   core has nothing to decide (a retried `Transmit` then finds `done` and gets the real `server_at`);
/// - or is a `transmit` row with a `Transmit` attached: the core decides, and a message the user deleted while offline
///   never goes;
/// - or is a `job` row while the account is `Online`.
///
/// The caller asks only after the chat server's login-ack.
#[doc(hidden)]
pub fn next_send(conn: &Connection, can: &Runnable<'_>) -> Result<Option<SendTask>> {
    const OP: &str = "read the next send row failed";
    const BAD: StoreError = StoreError::Unreadable("a send row unreadable");
    let mut stmt = conn.prepare_cached(OPEN_IN_ORDER).map_err(sql(OP))?;
    let mut rows = stmt.query([]).map_err(sql(OP))?;
    while let Some(r) = rows.next().map_err(sql(OP))? {
        let id: i64 = r.get(0).map_err(sql(OP))?;
        if can.cooling.contains(&id) {
            continue;
        }
        let origin: String = r.get(1).map_err(sql(OP))?;
        let state: String = r.get(2).map_err(sql(OP))?;
        let runs = SendState::parse(&state).ok_or(BAD)? == SendState::Acked
            || match SendOrigin::parse(&origin).ok_or(BAD)? {
                SendOrigin::Transmit => can.attached.contains(&id),
                SendOrigin::Job => can.online,
            };
        if runs {
            return send_task(conn, id);
        }
    }
    Ok(None)
}

const SWEEP_DONE: &str = "DELETE FROM send_task WHERE state = 'done' AND finished_at < ?1";
const SWEEP_OPEN: &str =
    "DELETE FROM send_task WHERE state <> 'done' AND origin = ?1 AND queued_at < ?2";

/// What the user's clear takes of the queue (decision R5). `state <> 'done'` is what lets SQLite use `send_task_open`: it
/// cannot prove that the `IN` implies it.
const CLEAR_UNSENT: &str = "DELETE FROM send_task WHERE state <> 'done' AND origin = 'transmit' \
     AND state IN ('prepared', 'reflected') AND conversation = ?1 AND created_at <= ?2 \
     AND id NOT IN (SELECT value FROM json_each(?3))";

/// THE CLEAR'S PART OF THE QUEUE (decision R5), inside the caller's zone: a chat's unsent `transmit` rows at or below
/// `before_ms` go, with their sealed bytes and receivers. The rows are sealed, and the key is in the same file, so a clear
/// owes the user this. An attached row stays: it is in flight, past its point of no return. An `acked` row holds no body and
/// finishes by itself; a `done` row is the ledger's (ids and receivers, no body, for its 14 days); a job is not the user's
/// message. A row taken here and sent again by the core is prepared anew under the same id: receivers drop it by id, and the
/// phone sees a second `OutgoingMessage`. Returns how many rows went.
pub(crate) fn clear_unsent(
    tx: &Tx<'_>,
    conversation: &[u8],
    before_ms: i64,
    attached: &[i64],
) -> Result<usize> {
    const OP: &str = "clear the unsent rows of a chat failed";
    tx.execute(
        CLEAR_UNSENT,
        params![conversation, before_ms, attached_json(attached)],
    )
    .map_err(sql(OP))
}

/// THE SWEEP, as ONE zone (`INSIDE_SEND_SWEEP` / `AFTER_SEND_SWEEP`): `done` rows past 14 days (a very late retry then
/// prepares anew under the same id, and receivers drop it by id), open `transmit` rows past 30 days, open `job` rows
/// past 14 days, each with its receivers; a row exactly at its age stays. The caller runs it at every socket's first
/// clock-proved `ServerInfo`: never under a clock nothing proved, and a long-running desktop keeps no spent budget
/// until a restart. Each delete finds its rows by a walk of the covering partial index `send_task_open` (the plan test).
#[doc(hidden)]
pub fn sweep_sends(db: &ZoneDb, now_ms: i64) -> Result<Swept> {
    const OP: &str = "sweep the send rows failed";
    let count = |n: usize| u32::try_from(n).map_err(|_| StoreError::Failed(OP));
    send_zone(db, SWEEP_POINTS, |tx| {
        let done = tx
            .execute(SWEEP_DONE, params![now_ms.saturating_sub(DONE_KEEP_MS)])
            .map_err(sql(OP))?;
        let transmit = tx
            .execute(
                SWEEP_OPEN,
                params![
                    SendOrigin::Transmit.as_str(),
                    now_ms.saturating_sub(TRANSMIT_KEEP_MS)
                ],
            )
            .map_err(sql(OP))?;
        let jobs = tx
            .execute(
                SWEEP_OPEN,
                params![SendOrigin::Job.as_str(), now_ms.saturating_sub(JOB_KEEP_MS)],
            )
            .map_err(sql(OP))?;
        let swept = Swept {
            done: count(done)?,
            transmit: count(transmit)?,
            jobs: count(jobs)?,
        };
        for (rule, n) in [
            (RULE_TRANSMIT_EXPIRED, swept.transmit),
            (RULE_JOB_EXPIRED, swept.jobs),
        ] {
            if n > 0 {
                tx.on_commit(move || {
                    tracing::warn!("send rows dropped by the sweep: {n} by {rule}");
                });
            }
        }
        Ok(swept)
    })
}

#[cfg(test)]
mod tests {
    use kourier_db::DbKey;

    use super::*;
    use crate::store::open;

    /// A file at the latest schema, in `dir`.
    fn store(dir: &tempfile::TempDir) -> ZoneDb {
        open(
            &dir.path().join("backend.db"),
            &DbKey::from_bytes([0x5f; 32]),
        )
        .expect("a migrated file")
    }

    /// What SQLite plans for `statement`, with as many values as it has parameters.
    fn plan(db: &ZoneDb, statement: &str) -> Vec<String> {
        db.read(|conn: &Connection| {
            let mut stmt = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {statement}"))
                .map_err(sql("plan"))?;
            let values = std::iter::repeat_n(0_i64, stmt.parameter_count());
            stmt.query_map(kourier_db::rusqlite::params_from_iter(values), |r| {
                r.get::<_, String>(3)
            })
            .map_err(sql("plan"))?
            .collect::<std::result::Result<Vec<String>, _>>()
            .map_err(sql("plan"))
        })
        .expect("the plan")
    }

    /// Every statement that runs often walks a partial index and never the table: the budget at every PREPARE, the walk
    /// of `next_send` at every pump, the eviction's search and the sweep's search of open rows read index pages only
    /// (`send_task_open` COVERS them: S-L1, a walk of the whole partial index, not one search); the clear needs
    /// `conversation` and `created_at`, which the index lacks, so it reads the index and then the table's rows it names
    /// (every delete reaches the receivers through their primary key, the cascade's one search). The `done` rows of up to
    /// 14 days and the sealed BLOBs of the open rows are not read on the way. (SQLite's words, on the schema's own
    /// indexes.)
    #[test]
    fn the_statements_that_run_often_walk_a_partial_index_and_never_the_table() {
        const COVERING: &str = "SCAN send_task USING COVERING INDEX send_task_open";
        const OPEN: &str = "SCAN send_task USING INDEX send_task_open";
        const CASCADE: &str =
            "SEARCH send_receiver USING COVERING INDEX sqlite_autoindex_send_receiver_1 (task=?)";
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = store(&dir);
        assert_eq!(plan(&db, OPEN_OF_ORIGIN), [COVERING], "the budget");
        assert_eq!(
            plan(&db, OPEN_IN_ORDER),
            [COVERING],
            "the walk of next_send, in queue order with no sort"
        );
        // Between them the list of attached rows, read once: the virtual table's own plan is not this statement's; the
        // oldest-first order is one sort of the rows found.
        let evict = plan(&db, EVICTABLE);
        assert_eq!(
            evict.first().map(String::as_str),
            Some(COVERING),
            "the eviction's search"
        );
        assert_eq!(
            evict.last().map(String::as_str),
            Some("USE TEMP B-TREE FOR ORDER BY"),
            "the eviction's order"
        );
        let clear = plan(&db, CLEAR_UNSENT);
        assert_eq!(clear.first().map(String::as_str), Some(OPEN), "the clear");
        assert_eq!(clear.last().map(String::as_str), Some(CASCADE), "the clear");
        assert_eq!(
            plan(&db, SWEEP_OPEN),
            [COVERING, CASCADE],
            "the sweep's open rows"
        );
        assert_eq!(
            plan(&db, SWEEP_DONE),
            [
                "SEARCH send_task USING COVERING INDEX send_task_done (finished_at<?)",
                CASCADE
            ],
            "the sweep's done rows"
        );
    }

    /// THE ANSWERS' READ NAMES NO BOX (S-L1): `send_receiver_states` is the same receivers as `send_receivers`, state for
    /// state, and its statement never reads the table's `payload` column (index 3), where `send_receivers` does: the
    /// bytecode shows it, not a guess. A row of 256 receivers all pending is ~2 MiB of boxes an answer does not look at.
    #[test]
    fn the_receiver_states_read_is_the_same_receivers_and_reads_no_box() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = store(&dir);
        db.zone(|tx| {
            tx.execute(
                "INSERT INTO send_task (origin, kind, message_id, conversation, created_at, state, has_update, bytes, \
                   queued_at) VALUES ('job', 'text', 1, X'0A', 0, 'reflected', 0, 0, 1)",
                [],
            )
            .map_err(sql("seed"))?;
            for (identity, state, payload) in [
                ("RCVR0001", "pending", Some(vec![1_u8; 88])),
                ("RCVR0002", "acked", None),
                ("RCVR0003", "blocked", None),
                ("RCVR0004", "invalid", None),
                ("RCVR0005", "no-key", None),
                ("RCVR0006", "omitted", None),
            ] {
                tx.execute(
                    "INSERT INTO send_receiver (task, identity, state, payload) VALUES (1, ?1, ?2, ?3)",
                    params![identity, state, payload],
                )
                .map_err(sql("seed"))?;
            }
            Ok::<(), StoreError>(())
        })
        .expect("the rows");
        let (states, boxes) = db
            .read(|conn: &Connection| {
                Ok::<_, StoreError>((send_receiver_states(conn, 1)?, send_receivers(conn, 1)?))
            })
            .expect("both reads");
        assert_eq!(states.len(), 6, "every receiver");
        assert!(
            states
                .iter()
                .map(|(identity, state)| (identity.as_str(), *state))
                .eq(boxes.iter().map(|r| (r.identity.as_str(), r.state))),
            "the same receivers in the same order with the same states"
        );
        // EXPLAIN's `Column` opcodes: the one that reads column 3 of a cursor is the `payload`.
        let reads_payload = |statement: &str| {
            db.read(|conn: &Connection| {
                let mut stmt = conn
                    .prepare(&format!("EXPLAIN {statement}"))
                    .map_err(sql("explain"))?;
                let columns = stmt
                    .query_map([1_i64], |r| {
                        Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?))
                    })
                    .map_err(sql("explain"))?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(sql("explain"))?;
                Ok::<_, StoreError>(columns.iter().any(|(op, p2)| op == "Column" && *p2 == 3))
            })
            .expect("the bytecode")
        };
        assert!(
            !reads_payload(RECEIVER_STATES),
            "the states' statement reads no box"
        );
        assert!(
            reads_payload(
                "SELECT identity, state, payload FROM send_receiver WHERE task = ?1 ORDER BY identity"
            ),
            "the test's own control: the boxes' statement does"
        );
    }

    /// The ledger's read is one search of `send_by_local`.
    #[test]
    fn the_ledgers_read_is_one_search_of_its_index() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = store(&dir);
        assert_eq!(
            plan(
                &db,
                &format!("SELECT {TASK_COLUMNS} FROM send_task WHERE local = ?1")
            ),
            ["SEARCH send_task USING INDEX send_by_local (local=?)"]
        );
    }

    /// The two kinds of failure the door can make, as the crate's own `is_content` reads them: a CHECK's code is the
    /// content's, a full disk the store's; a full queue is not the content's, so a zone it rolls back acks nothing.
    #[test]
    fn the_doors_two_codes_are_the_two_kinds_and_a_full_queue_is_no_content() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = store(&dir);
        for (code, content) in [(275, true), (13, false)] {
            fail_send_zone(fp::INSIDE_SEND_SWEEP, code);
            let error = sweep_sends(&db, 0).expect_err("the door fails the zone");
            assert_eq!(error.is_content(), content, "code {code}");
        }
        fail_send_zone("", 0);
        sweep_sends(&db, 0).expect("the door is off");
        assert!(!StoreError::QueueFull.is_content());
    }
}
