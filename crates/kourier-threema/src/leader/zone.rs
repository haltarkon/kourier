//! THE LEADER'S STORE (TH6 L1; schema 5, `migrations/0005_leader.sql`; brief §2.5, §2.7, §2.8): the rows of
//! `csp_inbound` and the three zones that move them. Nothing here opens, seals or touches a socket: a zone takes what is
//! decided, opened and sealed already (the nonces hashed through `reflect::Keys::hash`) and commits it; the frames go
//! after the commit.
//!
//! ```text
//!  a kept message ── ZONE RECEIVE ──▶ pending   (its events, nonces, jobs and the SEALED reflect, in one zone)
//!  pending ── reflect-ack ── ZONE REFLECTED ──▶ reflected   (the sealed bytes dropped; the receipts as jobs)
//!  pending   ── the phone reflects it (C10, the follower's zone) ──▶ gone (its nonce stays)
//!  reflected past 14 days, pending past 30 days WITH its CSP nonce ── THE SWEEP ──▶ gone
//! ```
//!
//! - ZONE RECEIVE ([`receive_inbound`]; `INSIDE_INBOUND_RECEIVE` / `AFTER_INBOUND_RECEIVE`): up to a batch of messages
//!   in arrival order, ONE zone. A spec DISCARD writes the CSP nonce where the spec marks it and the jobs its steps send;
//!   a KEPT message writes, in this order, the `ContactSync` of a committed transaction and then its `IncomingMessage`
//!   through the follower's own mapper (`reflect::map_one`: BLOB writes, a `deferred` row of source `csp`), its jobs
//!   (the creator's answer), its CSP nonce, the D2D nonce of its reflect and its `csp_inbound` row `pending` with the
//!   sealed reflect; the batch's EVENTS, in arrival order, go into `core_outbox` at the end of the same zone (rows of at
//!   most 500). All or nothing: a kill leaves none of it or all of it (a second zone for the events dies in the kill
//!   loop, mutant M3).
//! - ZONE REFLECTED ([`mark_inbound_reflected`]; `INSIDE_INBOUND_REFLECTED` / `AFTER_INBOUND_REFLECTED`): the rows whose
//!   reflect-ack came, `reflected` with the zone's local time, the sealed reflect and the nonce's hash dropped, and ONE
//!   delivery-receipt job per sender over the rows that owe one (call C3: coalesced, at most [`RECEIPT_IDS`] ids a job,
//!   in arrival order), in the same zone, BEFORE the CSP ack (the spec's persistent task, P9).
//! - THE SWEEP ([`sweep_inbound`]; `INSIDE_INBOUND_SWEEP` / `AFTER_INBOUND_SWEEP`), beside the send queue's at every
//!   socket's first clock-proved `ServerInfo`: `reflected` rows past 14 days (the step-14 memory's span), `pending` rows
//!   past 30 days AND their CSP nonces (critic T-L5, decided (b) before 0005 froze): a message the chat server still
//!   keeps then comes again as new and is reflected, where a kept nonce would have it acked unreflected at step 2. A
//!   row exactly at its age stays. One WARN `inbound-pending-expired` with the count; the `reflected` rows go quietly.
//! - STEP 0 ([`inbound_row`]): the row of (sender, message id), ONE search of the `UNIQUE` index, read BEFORE the nonce
//!   check of every incoming message.
//! - C10 ([`forget_pending`]): the follower's zone deletes a `pending` row when the phone reflects an `IncomingMessage`
//!   of the same (sender, message id): the phone took it over (its nonce stays: the phone acks it).
//!
//! INSIDE RECEIVE, the zone guards itself against what a decision made before it could not see (a batch is decided
//! before its zone runs): a kept message whose (sender, id) has a row by then is written not at all ([`Received::Known`]:
//! hold it, the next socket resumes it at step 0); one whose CSP nonce is in the store by then is the spec's step 2
//! ([`Received::NonceUsed`]: a discard); a reflect whose D2D nonce is in the store fails the zone (a drawn nonce is never
//! used twice: `add_nonces` is `INSERT OR IGNORE` and would hide it). A job the full queue refuses fails the zone with
//! `QueueFull` (not the content's: the caller holds, S-L2).
//!
//! T-M4's inbound cooling keeps NO durable count (decided here, with the critic's "in memory per actor"): nothing
//! durable hangs on it (an inbound row is never given up: the guard forbids an ack unreflected, so only the phone or the
//! sweep ends it), and a restart that tries a poison row twice more is what the cooling wants (the mediator may have
//! recovered). A later step that needs it adds a column in a new migration.
//!
//! Nothing in a real build reaches these yet (L4b's socket stage does), so the zones, the step-0 read and the door are
//! doc-hidden doors in `store` (rule 13). Nothing here logs a body, an ID, a nonce or a key: rule names and counts.

use std::fmt;
use std::rc::Rc;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

use kourier_backend_api::BackendEvent;
use kourier_db::rusqlite::{Connection, OptionalExtension as _, Row, params};
use kourier_db::{Tx, ZoneDb};
use libthreema::common::ThreemaId;
use libthreema::protobuf::d2d;
use prost::Message as _;
use tokio::sync::Notify;
use zeroize::Zeroizing;

use crate::db::{self, Points};
use crate::error::{Result, StoreError, sql};
use crate::failpoints as fp;
use crate::outbox;
use crate::reflect::{self, Counts, One};
use crate::send::{NewJob, enqueue_job};
use crate::store::{
    self, DeferredBounds, DeferredSource, Dropped, NonceScope, from_sql_u64, to_sql_u64,
};

const RECEIVE_POINTS: Points = Points {
    inside: fp::INSIDE_INBOUND_RECEIVE,
    after: fp::AFTER_INBOUND_RECEIVE,
};

const REFLECTED_POINTS: Points = Points {
    inside: fp::INSIDE_INBOUND_REFLECTED,
    after: fp::AFTER_INBOUND_REFLECTED,
};

const SWEEP_POINTS: Points = Points {
    inside: fp::INSIDE_INBOUND_SWEEP,
    after: fp::AFTER_INBOUND_SWEEP,
};

/// The most message ids of one delivery-receipt job (a receipt carries a list; one RECEIVE batch holds at most 30).
pub const RECEIPT_IDS: usize = 30;
/// A `reflected` row is the step-14 memory for 14 days.
const REFLECTED_KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// A `pending` row (and its CSP nonce) stands for 30 days.
const PENDING_KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;
/// The rule name of the sweep's WARN line of `pending` rows past their age, with their count.
pub const RULE_PENDING_EXPIRED: &str = "inbound-pending-expired";

const OP_READ: &str = "read the received rows failed";
const OP_RECEIVE: &str = "store a received message failed";
const OP_REFLECTED: &str = "mark a received message reflected failed";
const OP_SWEEP: &str = "sweep the received rows failed";
const OP_FORGET: &str = "forget a received message failed";
/// The zone's own refusal: a reflect's D2D nonce is in the store already (a drawn nonce is never used twice).
const OP_D2D_USED: &str = "a received message's reflect nonce is used";
const OP_OWN: &str = "the own ID of a receive zone";

/// The `state` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundState {
    /// Written by ZONE RECEIVE with the sealed reflect; its reflect-ack not durable.
    Pending,
    /// The reflect-ack is durable (ZONE REFLECTED); the step-14 memory.
    Reflected,
}

impl InboundState {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "reflected" => Some(Self::Reflected),
            _ => None,
        }
    }
}

/// One stored row. `Debug` prints the id, the state and the two marks: no ID, no message id, no nonce, no bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct InboundRow {
    /// Arrival order; the reflect-ack's owner on the socket.
    pub id: i64,
    /// The sender's Threema ID.
    pub sender: String,
    /// The message id.
    pub message_id: u64,
    /// Where it stands.
    pub state: InboundState,
    /// A CSP message-ack is owed.
    pub ack: bool,
    /// ZONE REFLECTED enqueues a delivery receipt for it.
    pub receipt: bool,
    /// When ZONE RECEIVE ran (ms, the local clock).
    pub received_at: i64,
    /// When ZONE REFLECTED ran (ms, the local clock); `None` while `pending`.
    pub reflected_at: Option<i64>,
    /// The CSP nonce as `csp_nonce` holds it; `None` once `reflected`.
    pub nonce_hash: Option<[u8; 32]>,
    /// The sealed reflect, to write again byte for byte; `None` once `reflected`.
    pub reflect_env: Option<Zeroizing<Vec<u8>>>,
}

impl fmt::Debug for InboundRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundRow")
            .field("id", &self.id)
            .field("state", &self.state)
            .field("ack", &self.ack)
            .field("receipt", &self.receipt)
            .finish_non_exhaustive()
    }
}

/// What every message of one RECEIVE zone shares: the own ID (which mention is ours), the zone's local time
/// (`received_at`, a `deferred` row's age, the jobs' clock), the catch-up mark of its events (call C4), the bounds of
/// `deferred` and the forwarder's wake.
pub struct Receiving<'a> {
    /// The own Threema ID.
    pub own: &'a str,
    /// The zone's local time (ms).
    pub now_ms: i64,
    /// The events are catch-up (the CSP backlog before `QueueSendComplete`).
    pub catch_up: bool,
    /// The bounds of `deferred` (without the age under a clock nothing proved).
    pub bounds: DeferredBounds,
    /// Wakes the forwarder once the zone committed.
    pub wake: &'a Rc<Notify>,
}

/// A message the spec discards (steps 2-21, the Common Group Receive Steps): acked after the zone, nothing reflected.
pub struct Discarded<'a> {
    /// The CSP nonce, hashed, where the spec marks it (past step 7 with a replay-protected type); `None` before that.
    pub nonce_hash: Option<[u8; 32]>,
    /// The jobs its steps send (a group-sync-request, an empty group-setup, a group-leave, the FS reject).
    pub jobs: &'a [NewJob],
    /// TH6 L5: the plaintext `d2d.Envelope { contact_sync }` of a transaction committed for this message (its steps
    /// discarded it after the change: the phone has the change, so the file gets it too), applied first.
    pub contact: Option<&'a [u8]>,
}

/// A message the leader KEEPS: mapped, reflected, then acked.
pub struct Kept<'a> {
    /// The sender's Threema ID from the clear head.
    pub sender: &'a str,
    /// The message id from the clear head.
    pub message_id: u64,
    /// The `d2d.Envelope { incoming_message }` the leader reflects, as PLAINTEXT (the mapper reads it, a `deferred`
    /// row keeps it): 1 to 65,536 bytes.
    pub message: &'a [u8],
    /// The plaintext `d2d.Envelope { contact_sync }` of a COMMITTED transaction for this message (an implicit contact,
    /// a nickname, the acquaintance lifted), applied first through the same mapper as the phone's reflection of it.
    pub contact: Option<&'a [u8]>,
    /// The message's time (ms; critic T-L3: the metadata's, else the head's x 1000, clamped to now): `server_at` of its
    /// events and of a `deferred` row.
    pub server_at: u64,
    /// The CSP flags (a `deferred` row's `flags`).
    pub flags: u8,
    /// The CSP nonce, hashed.
    pub nonce_hash: [u8; 32],
    /// The D2D nonce of the sealed reflect, hashed.
    pub d2d_hash: [u8; 32],
    /// The sealed reflect: 40 to 65,516 bytes (the schema's CHECK).
    pub reflect_env: &'a [u8],
    /// A CSP message-ack is owed (no flag `0x04`).
    pub ack: bool,
    /// A delivery receipt is owed at ZONE REFLECTED.
    pub receipt: bool,
    /// The jobs of keeping it (the creator's answer).
    pub jobs: &'a [NewJob],
}

/// One message of a RECEIVE zone.
pub enum Inbound<'a> {
    /// Discarded by the spec.
    Discard(Discarded<'a>),
    /// Kept.
    Keep(Kept<'a>),
}

/// What became of one message of a committed RECEIVE zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Received {
    /// Discarded: its nonce and jobs (if any) are in; ack it.
    Discarded,
    /// Kept: its `csp_inbound` row, `pending`; reflect it.
    Kept(i64),
    /// Kept, but its (sender, message id) had a row by the time the zone ran (a copy earlier in the same batch, or a
    /// race the decision could not see): nothing of it was written. Neither reflected nor acked: the next socket
    /// resumes it at step 0.
    Known,
    /// Kept, but its CSP nonce was in the store by the time the zone ran (another message of the batch used it):
    /// nothing of it was written; the spec's step 2, a discard to ack.
    NonceUsed,
}

/// A row ZONE REFLECTED moved: what its CSP ack names, in the order given.
#[derive(Clone, PartialEq, Eq)]
pub struct InboundAck {
    /// The row.
    pub id: i64,
    /// The sender's Threema ID (the ack names it).
    pub sender: String,
    /// The message id (the ack names it).
    pub message_id: u64,
    /// Whether an ack is owed.
    pub ack: bool,
}

impl fmt::Debug for InboundAck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InboundAck")
            .field("id", &self.id)
            .field("ack", &self.ack)
            .finish_non_exhaustive()
    }
}

/// What [`sweep_inbound`] deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InboundSwept {
    /// `reflected` rows past 14 days: the step-14 memory's forgetting, quiet.
    pub reflected: u32,
    /// `pending` rows past 30 days, with their CSP nonces ([`RULE_PENDING_EXPIRED`]).
    pub pending: u32,
}

/// The zones a test may make fail, by their `INSIDE` points.
const FAILABLE: [&str; 3] = [
    fp::INSIDE_INBOUND_RECEIVE,
    fp::INSIDE_INBOUND_REFLECTED,
    fp::INSIDE_INBOUND_SWEEP,
];
/// The test door's zone ([`fail_inbound_zone`]): one more than its place in [`FAILABLE`], 0 for none.
static FAIL_ZONE: AtomicUsize = AtomicUsize::new(0);
/// The SQLite extended result code the door's zone fails with.
static FAIL_CODE: AtomicI32 = AtomicI32::new(0);

/// Makes the leader's zone of `point` (its `INSIDE_*` failpoint) fail with the SQLite extended result code `code`,
/// after its writes and before its commit, every time: a content code such as 275 (`SQLITE_CONSTRAINT_CHECK`) or the
/// store's such as 13 (`SQLITE_FULL`). An empty `point` turns it off; every call replaces the one before. Nothing in
/// the application calls it.
///
/// # Panics
///
/// With a `point` that is no leader zone's (a test's mistake).
#[doc(hidden)]
pub fn fail_inbound_zone(point: &str, code: i32) {
    let zone = FAILABLE
        .iter()
        .position(|z| *z == point)
        .map_or(0, |at| at + 1);
    assert!(
        zone != 0 || point.is_empty(),
        "the door takes the INSIDE point of a leader zone"
    );
    // The code first: a reader that sees the zone sees its code.
    FAIL_CODE.store(code, Ordering::SeqCst);
    FAIL_ZONE.store(zone, Ordering::SeqCst);
}

fn door_error(point: &str) -> Option<StoreError> {
    let zone = FAIL_ZONE.load(Ordering::SeqCst);
    (zone != 0 && FAILABLE[zone - 1] == point).then(|| StoreError::Sql {
        op: "a leader zone failed",
        code: Some(FAIL_CODE.load(Ordering::SeqCst)),
    })
}

/// ONE leader zone: [`db::zone`] with the test door behind `f`'s writes.
fn inbound_zone<T>(
    db: &ZoneDb,
    points: Points,
    f: impl FnOnce(&mut Tx<'_>) -> Result<T>,
) -> Result<T> {
    db::zone(db, points, |tx| {
        let out = f(tx)?;
        door_error(points.inside).map_or(Ok(out), Err)
    })
}

/// Every column, in the order [`row_of`] reads them.
const COLUMNS: &str = "id, sender, message_id, state, ack, receipt, received_at, reflected_at, nonce_hash, reflect_env";

/// STEP 0's read: ONE search of the `UNIQUE (sender, message_id)` index (a unit test pins the plan).
const STEP_0: &str = "SELECT id, sender, message_id, state, ack, receipt, received_at, reflected_at, nonce_hash, \
     reflect_env FROM csp_inbound WHERE sender = ?1 AND message_id = ?2";

/// ZONE RECEIVE's insert (never `OR REPLACE`: the step-0 read comes first).
const INSERT: &str = "INSERT INTO csp_inbound (sender, message_id, state, ack, receipt, received_at, \
     reflected_at, nonce_hash, reflect_env) VALUES (?1, ?2, 'pending', ?3, ?4, ?5, NULL, ?6, ?7)";

/// ZONE REFLECTED's read of a row it moves (by its primary key).
const PENDING_OF: &str =
    "SELECT sender, message_id, ack, receipt FROM csp_inbound WHERE id = ?1 AND state = 'pending'";

/// ZONE REFLECTED's move.
const MARK_REFLECTED: &str = "UPDATE csp_inbound SET state = 'reflected', reflected_at = ?2, nonce_hash = NULL, \
     reflect_env = NULL WHERE id = ?1 AND state = 'pending'";

/// C10's delete: one search of the `UNIQUE` index.
const FORGET_PENDING: &str =
    "DELETE FROM csp_inbound WHERE sender = ?1 AND message_id = ?2 AND state = 'pending'";

/// The sweep's nonces: the CSP nonces of the `pending` rows past their age, each one search of `csp_nonce`'s key; the
/// rows found by one search of `csp_inbound_pending`.
const SWEEP_NONCES: &str = "DELETE FROM csp_nonce WHERE hash IN (SELECT nonce_hash FROM csp_inbound \
     WHERE state = 'pending' AND received_at < ?1)";
/// The sweep's `pending` rows: one search of `csp_inbound_pending`.
const SWEEP_PENDING: &str = "DELETE FROM csp_inbound WHERE state = 'pending' AND received_at < ?1";
/// The sweep's `reflected` rows: one search of `csp_inbound_done`.
const SWEEP_REFLECTED: &str =
    "DELETE FROM csp_inbound WHERE state = 'reflected' AND reflected_at < ?1";

fn row_of(r: &Row<'_>) -> kourier_db::rusqlite::Result<(InboundRowRaw, Option<Vec<u8>>)> {
    Ok((
        InboundRowRaw {
            id: r.get(0)?,
            sender: r.get(1)?,
            message_id: r.get(2)?,
            state: r.get(3)?,
            ack: r.get(4)?,
            receipt: r.get(5)?,
            received_at: r.get(6)?,
            reflected_at: r.get(7)?,
            nonce_hash: r.get(8)?,
        },
        r.get(9)?,
    ))
}

/// A row as SQLite gives it, before its checks.
struct InboundRowRaw {
    id: i64,
    sender: String,
    message_id: i64,
    state: String,
    ack: bool,
    receipt: bool,
    received_at: i64,
    reflected_at: Option<i64>,
    nonce_hash: Option<Vec<u8>>,
}

fn checked(raw: InboundRowRaw, reflect_env: Option<Vec<u8>>) -> Result<InboundRow> {
    const UNREADABLE: StoreError = StoreError::Unreadable("a received row unreadable");
    let nonce_hash = match raw.nonce_hash {
        None => None,
        Some(bytes) => Some(<[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| UNREADABLE)?),
    };
    Ok(InboundRow {
        id: raw.id,
        sender: raw.sender,
        message_id: from_sql_u64(raw.message_id),
        state: InboundState::parse(&raw.state).ok_or(UNREADABLE)?,
        ack: raw.ack,
        receipt: raw.receipt,
        received_at: raw.received_at,
        reflected_at: raw.reflected_at,
        nonce_hash,
        reflect_env: reflect_env.map(Zeroizing::new),
    })
}

/// STEP 0 (P1): the row of (`sender`, `message_id`), if any. Inside a zone too.
#[doc(hidden)]
pub fn inbound_row(conn: &Connection, sender: &str, message_id: u64) -> Result<Option<InboundRow>> {
    let raw = conn
        .prepare_cached(STEP_0)
        .and_then(|mut stmt| {
            stmt.query_row(params![sender, to_sql_u64(message_id)], row_of)
                .optional()
        })
        .map_err(sql(OP_READ))?;
    raw.map(|(raw, env)| checked(raw, env)).transpose()
}

/// Every row, by id (the tests' look at the table).
#[doc(hidden)]
pub fn inbound_rows(conn: &Connection) -> Result<Vec<InboundRow>> {
    let mut stmt = conn
        .prepare(&format!("SELECT {COLUMNS} FROM csp_inbound ORDER BY id"))
        .map_err(sql(OP_READ))?;
    let raws = stmt
        .query_map([], row_of)
        .map_err(sql(OP_READ))?
        .collect::<kourier_db::rusqlite::Result<Vec<_>>>()
        .map_err(sql(OP_READ))?;
    raws.into_iter()
        .map(|(raw, env)| checked(raw, env))
        .collect()
}

/// ZONE RECEIVE (module doc): `messages` in arrival order, ONE zone; what became of each, in the same order. `Err` rolls
/// everything back: the store's failure (nothing more is acked, the socket ends), the content's (a plaintext that is no
/// `d2d.Envelope`, a row the schema refuses, a used D2D nonce: the caller redoes the batch one message per zone and
/// holds the one that fails, C6), `QueueFull` (a job the full queue refuses: the caller holds, S-L2).
#[doc(hidden)]
pub fn receive_inbound(
    db: &ZoneDb,
    at: &Receiving<'_>,
    messages: &[Inbound<'_>],
) -> Result<Vec<Received>> {
    let own = ThreemaId::try_from(at.own).map_err(|_| StoreError::Failed(OP_OWN))?;
    let ctx = reflect::Ctx {
        db,
        now_ms: at.now_ms,
        bounds: at.bounds,
        own,
        wake: at.wake,
        pictures: None,
        catch_up: at.catch_up,
    };
    let (received, counts) = inbound_zone(db, RECEIVE_POINTS, |tx| {
        let mut counts = Counts::default();
        let mut dropped = Dropped::default();
        let mut events = Vec::new();
        let mut received = Vec::with_capacity(messages.len());
        for message in messages {
            received.push(match message {
                Inbound::Discard(discard) => {
                    if let Some(plaintext) = discard.contact {
                        let envelope = d2d::Envelope::decode(plaintext)
                            .map_err(|_| StoreError::Unreadable("a received message's envelope"))?;
                        let mut notice_tag = [0; 8];
                        if let Some(hash) = &discard.nonce_hash {
                            notice_tag.copy_from_slice(&hash[..8]);
                        }
                        let one = One {
                            envelope: &envelope,
                            plaintext,
                            server_at: u64::try_from(at.now_ms).unwrap_or_default(),
                            flags: 0,
                            notice_tag,
                        };
                        events.extend(reflect::map_one(
                            tx,
                            &ctx,
                            &one,
                            DeferredSource::Csp,
                            &mut counts,
                            &mut dropped,
                        )?);
                    }
                    if let Some(hash) = &discard.nonce_hash {
                        store::add_nonces(tx, NonceScope::Csp, std::slice::from_ref(hash))?;
                    }
                    for job in discard.jobs {
                        enqueue_job(tx, job, at.now_ms)?;
                    }
                    Received::Discarded
                }
                Inbound::Keep(kept) => {
                    keep(tx, &ctx, kept, &mut events, &mut counts, &mut dropped)?
                }
            });
        }
        // The batch's events in arrival order, in rows of at most 500, in THIS zone: durable before any reflect.
        outbox::append_in_rows(tx, &events, at.now_ms, at.wake)?;
        store::warn_dropped(tx, dropped);
        Ok((received, counts))
    })?;
    counts.warn_received();
    Ok(received)
}

/// One kept message inside ZONE RECEIVE (module doc, the order of its writes).
fn keep(
    tx: &mut Tx<'_>,
    ctx: &reflect::Ctx<'_>,
    kept: &Kept<'_>,
    events: &mut Vec<BackendEvent>,
    counts: &mut Counts,
    dropped: &mut Dropped,
) -> Result<Received> {
    if inbound_row(tx, kept.sender, kept.message_id)?.is_some() {
        tracing::debug!("a received message is in the file already; held for step 0");
        return Ok(Received::Known);
    }
    if store::contains_nonce(tx, NonceScope::Csp, &kept.nonce_hash)? {
        tracing::debug!("a received message's nonce is used; discarded");
        return Ok(Received::NonceUsed);
    }
    if store::contains_nonce(tx, NonceScope::D2d, &kept.d2d_hash)? {
        return Err(StoreError::Failed(OP_D2D_USED));
    }
    let mut notice_tag = [0; 8];
    notice_tag.copy_from_slice(&kept.nonce_hash[..8]);
    for plaintext in kept.contact.into_iter().chain([kept.message]) {
        let envelope = d2d::Envelope::decode(plaintext)
            .map_err(|_| StoreError::Unreadable("a received message's envelope"))?;
        let one = One {
            envelope: &envelope,
            plaintext,
            server_at: kept.server_at,
            flags: u16::from(kept.flags),
            notice_tag,
        };
        events.extend(reflect::map_one(
            tx,
            ctx,
            &one,
            DeferredSource::Csp,
            counts,
            dropped,
        )?);
    }
    for job in kept.jobs {
        enqueue_job(tx, job, ctx.now_ms)?;
    }
    store::add_nonces(tx, NonceScope::Csp, std::slice::from_ref(&kept.nonce_hash))?;
    store::add_nonces(tx, NonceScope::D2d, std::slice::from_ref(&kept.d2d_hash))?;
    tx.execute(
        INSERT,
        params![
            kept.sender,
            to_sql_u64(kept.message_id),
            kept.ack,
            kept.receipt,
            ctx.now_ms,
            &kept.nonce_hash[..],
            kept.reflect_env,
        ],
    )
    .map_err(sql(OP_RECEIVE))?;
    Ok(Received::Kept(tx.last_insert_rowid()))
}

/// ZONE REFLECTED (module doc): the rows `ids` (their reflect-acks came) in the order given, ONE zone at the local time
/// `now_ms`; `receipt(sender, ids)` makes the delivery-receipt job of one sender (the body is the send side's). A row
/// that is not `pending` (the phone's reflection took it, C10) is left and not returned: nothing is acked for it. `Err`
/// rolls back: the rows stay `pending` and no ack goes (the store's failure ends the socket; `QueueFull` holds the acks,
/// S-L2; the content's: the caller redoes it one row per zone).
#[doc(hidden)]
pub fn mark_inbound_reflected(
    db: &ZoneDb,
    ids: &[i64],
    now_ms: i64,
    receipt: &dyn Fn(&str, &[u64]) -> NewJob,
) -> Result<Vec<InboundAck>> {
    inbound_zone(db, REFLECTED_POINTS, |tx| {
        let mut moved = Vec::with_capacity(ids.len());
        // The senders in the order first seen, each with its receipt's ids in row order.
        let mut owed: Vec<(String, Vec<u64>)> = Vec::new();
        for &id in ids {
            let row: Option<(String, i64, bool, bool)> = tx
                .prepare_cached(PENDING_OF)
                .and_then(|mut stmt| {
                    stmt.query_row(params![id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })
                    .optional()
                })
                .map_err(sql(OP_REFLECTED))?;
            let Some((sender, message_id, ack, wants_receipt)) = row else {
                tracing::debug!(
                    "a reflect-ack names a received row that is not pending; nothing acked"
                );
                continue;
            };
            let n = tx
                .execute(MARK_REFLECTED, params![id, now_ms])
                .map_err(sql(OP_REFLECTED))?;
            if n != 1 {
                return Err(StoreError::Failed(OP_REFLECTED));
            }
            let message_id = from_sql_u64(message_id);
            if wants_receipt {
                match owed.iter_mut().find(|(s, _)| *s == sender) {
                    Some((_, list)) => list.push(message_id),
                    None => owed.push((sender.clone(), vec![message_id])),
                }
            }
            moved.push(InboundAck {
                id,
                sender,
                message_id,
                ack,
            });
        }
        for (sender, list) in &owed {
            for chunk in list.chunks(RECEIPT_IDS) {
                enqueue_job(tx, &receipt(sender, chunk), now_ms)?;
            }
        }
        Ok(moved)
    })
}

/// THE SWEEP as ONE zone (module doc): `reflected` rows past 14 days, `pending` rows past 30 days with their CSP nonces;
/// a row exactly at its age stays. The caller runs it under a proved clock only.
#[doc(hidden)]
pub fn sweep_inbound(db: &ZoneDb, now_ms: i64) -> Result<InboundSwept> {
    let count = |n: usize| u32::try_from(n).map_err(|_| StoreError::Failed(OP_SWEEP));
    inbound_zone(db, SWEEP_POINTS, |tx| {
        let pending_before = now_ms.saturating_sub(PENDING_KEEP_MS);
        tx.execute(SWEEP_NONCES, params![pending_before])
            .map_err(sql(OP_SWEEP))?;
        let pending = tx
            .execute(SWEEP_PENDING, params![pending_before])
            .map_err(sql(OP_SWEEP))?;
        let reflected = tx
            .execute(
                SWEEP_REFLECTED,
                params![now_ms.saturating_sub(REFLECTED_KEEP_MS)],
            )
            .map_err(sql(OP_SWEEP))?;
        let swept = InboundSwept {
            reflected: count(reflected)?,
            pending: count(pending)?,
        };
        let n = swept.pending;
        if n > 0 {
            tx.on_commit(move || {
                tracing::warn!(
                    rule = RULE_PENDING_EXPIRED,
                    dropped = n,
                    "received rows dropped by the sweep: {n} by {RULE_PENDING_EXPIRED}"
                );
            });
        }
        Ok(swept)
    })
}

/// C10, inside the follower's zone: the phone reflected the `IncomingMessage` of (`sender`, `message_id`) as its
/// leader's, so a `pending` row of it is deleted (its nonce stays: the phone acks it). A `reflected` row stays (the
/// step-14 memory). Returns whether a row went.
pub(crate) fn forget_pending(tx: &Tx<'_>, sender: &str, message_id: u64) -> Result<bool> {
    let n = tx
        .execute(FORGET_PENDING, params![sender, to_sql_u64(message_id)])
        .map_err(sql(OP_FORGET))?;
    if n > 0 {
        tracing::debug!("a received message the phone took over is forgotten");
    }
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use kourier_db::DbKey;

    use super::*;
    use crate::store::open;

    fn store(dir: &tempfile::TempDir) -> ZoneDb {
        open(
            &dir.path().join("backend.db"),
            &DbKey::from_bytes([0x6a; 32]),
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

    /// Step 0 runs for EVERY incoming message and C10 for every reflected `IncomingMessage`: each is ONE search of the
    /// `UNIQUE (sender, message_id)` index; ZONE REFLECTED reads and moves its rows by the primary key; the sweep finds
    /// its rows through the two partial indexes and the CSP nonces through `csp_nonce`'s key, never a scan of a table.
    /// (SQLite's words, on the schema's own indexes.)
    #[test]
    fn the_leaders_statements_search_an_index_and_never_scan() {
        const UNIQUE: &str = "SEARCH csp_inbound USING INDEX sqlite_autoindex_csp_inbound_1 (sender=? AND message_id=?)";
        const BY_ID: &str = "SEARCH csp_inbound USING INTEGER PRIMARY KEY (rowid=?)";
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = store(&dir);
        assert_eq!(plan(&db, STEP_0), [UNIQUE], "step 0");
        assert_eq!(plan(&db, FORGET_PENDING), [UNIQUE], "C10's delete");
        assert_eq!(plan(&db, PENDING_OF), [BY_ID], "ZONE REFLECTED's read");
        assert_eq!(plan(&db, MARK_REFLECTED), [BY_ID], "ZONE REFLECTED's move");
        assert_eq!(
            plan(&db, SWEEP_PENDING),
            ["SEARCH csp_inbound USING INDEX csp_inbound_pending (received_at<?)"],
            "the sweep of pending rows"
        );
        assert_eq!(
            plan(&db, SWEEP_REFLECTED),
            ["SEARCH csp_inbound USING INDEX csp_inbound_done (reflected_at<?)"],
            "the sweep of reflected rows"
        );
        let nonces = plan(&db, SWEEP_NONCES);
        assert!(
            nonces
                .iter()
                .any(|l| l.contains("csp_inbound USING INDEX csp_inbound_pending (received_at<?)"))
                && nonces
                    .iter()
                    .any(|l| l.contains("csp_nonce USING PRIMARY KEY (hash=?)"))
                && !nonces.iter().any(|l| l.starts_with("SCAN")),
            "the sweep of the nonces: {nonces:?}"
        );
    }

    /// C10: the follower's delete takes a `pending` row of (sender, id) and nothing else: a `reflected` row (the step-14
    /// memory), another sender's row and another id stay; the CSP nonce of the forgotten row stays (the phone acks it).
    #[test]
    fn c10_forgets_only_the_pending_row_of_its_sender_and_id() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = store(&dir);
        let pending = |sender: &str, id: i64| {
            format!(
                "INSERT INTO csp_inbound (sender, message_id, state, ack, receipt, received_at, nonce_hash, reflect_env) \
                 VALUES ('{sender}', {id}, 'pending', 1, 1, 1, X'{}', zeroblob(40));",
                "C7".repeat(32)
            )
        };
        let gone = db
            .zone(|tx| {
                tx.execute_batch(&format!(
                    "{}{}{}INSERT INTO csp_inbound (sender, message_id, state, ack, receipt, received_at, \
                     reflected_at) VALUES ('L1C10ID1', 8, 'reflected', 1, 1, 1, 2); \
                     INSERT INTO csp_nonce (hash) VALUES (X'{}');",
                    pending("L1C10ID1", 7),
                    pending("L1C10ID2", 9),
                    pending("L1C10ID1", -1),
                    "C7".repeat(32)
                ))
                .map_err(sql("seed"))?;
                Ok::<_, StoreError>([
                    forget_pending(tx, "L1C10ID1", 7)?,
                    forget_pending(tx, "L1C10ID1", 7)?,
                    forget_pending(tx, "L1C10ID1", 8)?,
                    forget_pending(tx, "L1C10ID2", 7)?,
                ])
            })
            .expect("the zone");
        assert_eq!(gone, [true, false, false, false]);
        let left: Vec<(String, u64, InboundState)> = db
            .read(inbound_rows)
            .expect("the rows")
            .into_iter()
            .map(|r| (r.sender, r.message_id, r.state))
            .collect();
        assert_eq!(
            left,
            [
                ("L1C10ID2".to_owned(), 9, InboundState::Pending),
                ("L1C10ID1".to_owned(), u64::MAX, InboundState::Pending),
                ("L1C10ID1".to_owned(), 8, InboundState::Reflected),
            ]
        );
        assert!(
            db.read(|c| store::contains_nonce(c, NonceScope::Csp, &[0xC7; 32]))
                .expect("the nonce"),
            "C10 took the nonce"
        );
    }

    /// The `Debug` of a row and of an ack prints neither an ID, a message id, a nonce nor a byte of the reflect.
    #[test]
    fn debug_prints_no_id_no_message_id_and_no_bytes() {
        let row = InboundRow {
            id: 3,
            sender: "L1DBGID7".to_owned(),
            message_id: 0x1122_3344_5566_7788,
            state: InboundState::Pending,
            ack: true,
            receipt: false,
            received_at: 5,
            reflected_at: None,
            nonce_hash: Some([0xab; 32]),
            reflect_env: Some(Zeroizing::new(vec![0xcd; 40])),
        };
        let ack = InboundAck {
            id: 3,
            sender: "L1DBGID7".to_owned(),
            message_id: 0x1122_3344_5566_7788,
            ack: true,
        };
        for printed in [format!("{row:?}"), format!("{ack:?}")] {
            for needle in ["L1DBGID7", "1234605616436508552", "171", "205", "ab", "cd"] {
                assert!(!printed.contains(needle), "Debug printed {needle}");
            }
        }
    }
}
