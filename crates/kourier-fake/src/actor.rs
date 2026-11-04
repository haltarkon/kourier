//! The fake account actor: its own thread, a current-thread runtime and `LocalSet`, a real
//! `backend.db` with `core_outbox`, stage 1 (inbox) and stage 2 (events) transactions, and the
//! `kourier-db` forwarder (§5.6, §6.3) — the durability shape of a real backend.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::{
    AbortAnswer, AccountContext, AssignedId, BackendEvent, Clock, Command, DurableCommand,
    Envelope, Exit, OutgoingRequest, Relink, StartMode, TransmitOutcome, decode_outbox_row,
    encode_outbox_row,
};
use kourier_db::rusqlite::OptionalExtension;
use kourier_db::{
    Batch, Forwarder, ForwarderConfig, Migrations, Refusal, ZoneDb, failpoint, migration, outbox,
};
use kourier_model::{
    AccountCaps, AccountProgress, AccountUuid, BackendError, ConnState, ConvKey,
    ConversationChange, ConversationPatch, DeviceLogout, ErrorKind, HistoryTransfer,
    HistoryTransferFailure, InputField, InputKind, LocalMsgId, LoginStep, LoginStepKind,
    LogoutReport, Member, NetworkKind, OwnDevice, Patch, RecipientState, Redacted, UserId,
};
use tokio::sync::{Notify, mpsc, watch};

use crate::conformance;
use crate::flavor::Flavor;
use crate::network::{AbortRecord, FakeControl, TransmitRecord};
use crate::rehearsal::{Injection, KnownChat, chats_of};
use crate::script::{
    FAKE_EMOJI, LoginScript, PIN_LIMIT, PairCodeScript, Script, Step, TransferScript,
};
use crate::upload;

mod cue;
mod phone;

/// The fake's `backend.db` schema component.
pub const MIGRATIONS: Migrations = Migrations::new(
    "fake",
    &[
        migration!(1, "0001_fake"),
        migration!(2, "0002_fake_transmit"),
        migration!(3, "0003_fake_job_payload"),
        migration!(4, "0004_fake_transmit_conv"),
    ],
);

/// Kill point after a stage-1 (inbox) commit (§10.4).
pub const FAILPOINT_AFTER_INBOX_COMMIT: &str = "after_inbox_commit";
/// Kill point between the stage-1 commit and the server ack (§10.4).
pub const FAILPOINT_BEFORE_ACK: &str = "before_ack";
/// Kill point after a stage-2 or job transaction committed its events into `core_outbox`.
pub const FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT: &str = "after_backend_effects_commit";
/// Kill point after the fake server accepted a transmit, before the outcome reaches the core
/// (§10.4 `after_server_accept`; the fake records acceptance and send progress in one
/// transaction, so a resend skips the recipients that have the message).
pub const FAILPOINT_AFTER_SERVER_ACCEPT: &str = "after_server_accept";

/// One message copy the fake server accepted (see [`server_log`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerDelivery {
    /// The network-local id of the message.
    pub local: String,
    /// The string form of the conversation the copy was sent in (`ConvKey::encode`); empty for
    /// a copy an earlier build of the fake logged.
    pub conv: String,
    /// Its `sent_at`.
    pub sent_at: i64,
    /// Who got it.
    pub recipient: UserId,
    /// It was a follow-up send to more recipients (`is_recipient_update`).
    pub recipient_update: bool,
}

/// Every message copy the fake server accepted, in order, from the `backend.db` at `path` (for
/// tests that inspect a killed process's account; the database must not be open elsewhere for
/// writing).
pub fn server_log(
    path: &Path,
    key: &kourier_db::DbKey,
) -> Result<Vec<ServerDelivery>, BackendError> {
    let conn = kourier_db::open_read_only(path, key).map_err(internal)?;
    let rows: Vec<(String, String, i64, String, bool)> = conn
        .prepare(
            "SELECT local, conv, sent_at, recipient, recipient_update FROM fake_transmits
             ORDER BY id",
        )
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<kourier_db::rusqlite::Result<Vec<_>>>()
        })
        .map_err(internal)?;
    rows.into_iter()
        .map(|(local, conv, sent_at, recipient, recipient_update)| {
            Ok(ServerDelivery {
                local,
                conv,
                sent_at,
                recipient: UserId::parse(&recipient).map_err(internal)?,
                recipient_update,
            })
        })
        .collect()
}

/// How long a logout or unlink waits for the forwarder to drain (§4.3 step 5).
/// One durable command in the fake's job table (see [`job_log`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    /// The command's id (`pending_commands.id` of the core).
    pub cmd: i64,
    /// `signal` or `job` for receipts and read syncs (by flavor), `clear` for a
    /// `ClearConversation`, `attachment` for a `DeleteAttachment`.
    pub kind: String,
    /// What the command asked for, for the deletes for me made on this device.
    pub command: Option<DurableCommand>,
}

/// The fake's job table, in command order, from the `backend.db` at `path` (for tests that
/// inspect a killed process's account; the database must not be open elsewhere for writing).
pub fn job_log(path: &Path, key: &kourier_db::DbKey) -> Result<Vec<Job>, BackendError> {
    let conn = kourier_db::open_read_only(path, key).map_err(internal)?;
    let rows: Vec<(i64, String, Option<String>)> = conn
        .prepare("SELECT cmd, kind, payload FROM jobs ORDER BY cmd")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<kourier_db::rusqlite::Result<Vec<_>>>()
        })
        .map_err(internal)?;
    rows.into_iter()
        .map(|(cmd, kind, payload)| {
            Ok(Job {
                cmd,
                kind,
                command: payload
                    .map(|p| serde_json::from_str(&p).map_err(internal))
                    .transpose()?,
            })
        })
        .collect()
}

const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

/// What the fake server did with the copies of a request (T2 F8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Copies {
    /// At least one recipient got a new copy.
    New,
    /// Every recipient already held this id: a repeat the server drops (the resend after a kill, with its id).
    Repeat,
    /// Nothing was taken (a refusal, or no recipient).
    Nothing,
}

enum Stop {
    Now(Exit),
    Drain(Exit),
}

enum LoginCmd {
    /// `StartLogin`: the flow id the core asked for and the relink, if it is one.
    Start {
        flow: String,
        relink: Option<Relink>,
    },
    /// `SubmitLoginInput`: the step's `txn` and the typed values (also recorded for the test). A
    /// scripted flow that looks at what was typed (the `phone` flow) reads them here.
    Input(u64, Vec<(String, Redacted<String>)>),
    Cancel,
}

/// How many downloads the fake runs at the same time.
const PARALLEL_DOWNLOADS: usize = 4;

/// One finished download: the attachment, the request and what `AttachmentDone` reports.
type Downloaded = (
    kourier_model::AttachmentId,
    kourier_model::DownloadId,
    Result<kourier_model::AttachmentMeta, BackendError>,
);

/// The downloads in flight.
type Downloads = tokio::task::JoinSet<Downloaded>;

/// The upload phases in flight ([`upload`]).
type Uploads = tokio::task::JoinSet<upload::Finished>;

fn internal(e: impl std::fmt::Display) -> BackendError {
    BackendError::internal(format!("fake backend: {e}"))
}

/// Events that are mirrored state: re-emitted after a `core.db` rebuild (§4.3 step 8).
fn is_mirrored(e: &BackendEvent) -> bool {
    matches!(
        e,
        BackendEvent::AccountSettings(_)
            | BackendEvent::AccountCaps(_)
            | BackendEvent::ContactUpsert(_)
            | BackendEvent::IdentityMerged { .. }
            | BackendEvent::ConversationUpsert(_)
            | BackendEvent::PinnedConversations(_)
            | BackendEvent::ConversationRemoved { .. }
    )
}

/// The thread body (see [`kourier_backend_api::spawn_account_thread`]).
pub(crate) fn run(
    ctx: AccountContext,
    script: Arc<Script>,
    pushed: bool,
    control: Arc<FakeControl>,
    flavor: Flavor,
    kind: NetworkKind,
) -> Exit {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return Exit::Failed(internal(e)),
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        match Actor::open(ctx, script, pushed, control, flavor, kind) {
            Ok((actor, commands)) => actor.run(commands).await,
            Err(e) => Exit::Failed(e),
        }
    })
}

struct Actor {
    db: Rc<ZoneDb>,
    wake: Notify,
    events: mpsc::Sender<Envelope>,
    conn: watch::Sender<ConnState>,
    go: watch::Sender<bool>,
    clock: Arc<dyn Clock>,
    script: Arc<Script>,
    control: Arc<FakeControl>,
    uuid: AccountUuid,
    start: StartMode,
    flavor: Flavor,
    /// The network this fake plays (`NetworkKind::Fake` for none): a fake that plays Threema
    /// shows the emoji step after the scan of a QR login.
    kind: NetworkKind,
    /// A link is in progress and the core has not acked `AccountReady` yet: the conformance
    /// check applies the linking rule (§4.3).
    linking: Cell<bool>,
    /// The account's blob directory (outgoing attachments are read from it).
    blob_dir: std::path::PathBuf,
    /// The script was pushed for this very start. One the account ran before is not played
    /// again by a relink: the conversations and messages it delivered are still in the core,
    /// and an account whose script ends unlinked would be unlinked again at once.
    pushed: bool,
    /// The user's skip of the history transfer, until the walk has looked at it.
    skip_pending: Cell<bool>,
    /// Wakes the walk of a history transfer (a skip arrived).
    skip: Notify,
    /// A history transfer is being walked ([`Actor::walk_transfer`]); otherwise a stored
    /// state that is not over just stands ([`Step::Transfer`]).
    walking: Cell<bool>,
    /// The messages in their upload phase ([`Script::uploads`]): what an abort asks.
    holds: RefCell<HashMap<LocalMsgId, Rc<upload::Hold>>>,
    /// The tail of the number the link must find (`BackendConfig::expect_number_suffix`), honoured
    /// only by a script that says so ([`Script::honour_number_tail`]).
    number_tail: Option<Redacted<String>>,
    /// The chats the script has delivered so far, with the member who writes into each: what
    /// [`FakeControl::arrive`] picks from.
    known: Arc<Mutex<Vec<KnownChat>>>,
    /// The account's `AccountCaps` have been stated (by the script or by [`Actor::with_roaming_statement`]):
    /// until then the next delivery carries the fake's statement of the roaming settings.
    caps_stated: Cell<bool>,
}

impl Actor {
    fn open(
        ctx: AccountContext,
        script: Arc<Script>,
        pushed: bool,
        control: Arc<FakeControl>,
        flavor: Flavor,
        kind: NetworkKind,
    ) -> Result<(Self, mpsc::Receiver<Command>), BackendError> {
        if let Some(dir) = ctx.backend_db.parent() {
            std::fs::create_dir_all(dir).map_err(internal)?;
        }
        let key = kourier_db::DbKey::from_bytes(*ctx.db_key.expose());
        let mut conn = kourier_db::open(&ctx.backend_db, &key).map_err(internal)?;
        kourier_db::migrate(&mut conn, &outbox::MIGRATIONS).map_err(internal)?;
        kourier_db::migrate(&mut conn, &MIGRATIONS).map_err(internal)?;
        control.spawned(ctx.uuid, ctx.start);
        let db = Rc::new(ZoneDb::new(conn));
        let linked = db
            .read(|c| -> kourier_db::Result<bool> {
                Ok(c.query_row(
                    "SELECT value FROM fake_state WHERE key = 'linked'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .is_some_and(|v| v != 0))
            })
            .map_err(internal)?;
        let actor = Self {
            db,
            wake: Notify::new(),
            events: ctx.events,
            conn: ctx.conn,
            go: watch::channel(false).0,
            clock: ctx.clock,
            script,
            control,
            uuid: ctx.uuid,
            start: ctx.start,
            flavor,
            kind,
            linking: Cell::new(ctx.start == StartMode::Fresh && !linked),
            blob_dir: ctx.blob_dir,
            pushed,
            skip_pending: Cell::new(false),
            skip: Notify::new(),
            walking: Cell::new(false),
            holds: RefCell::new(HashMap::new()),
            number_tail: ctx.config.expect_number_suffix,
            known: Arc::new(Mutex::new(Vec::new())),
            caps_stated: Cell::new(false),
        };
        // A resumed account has stated them in an earlier run (they are in its mirror).
        actor.caps_stated.set(actor.stated_caps()?.is_some());
        Ok((actor, ctx.commands))
    }

    async fn run(self, commands: mpsc::Receiver<Command>) -> Exit {
        tracing::debug!(uuid = %self.uuid.short(), flavor = ?self.flavor, script = %self.script.name, "fake actor running");
        // The run tool's rehearsal plays WhatsApp: say what the real backend says when the one-start
        // call switch is on, so the tool's proof of that switch is walked in the rehearsal too.
        if self.plays_the_rehearsal() {
            crate::rehearsal::say_call_builtin();
        }
        if self.start == StartMode::Rebuild
            && let Err(e) = self.rebuild()
        {
            return Exit::Failed(e);
        }
        let (login_tx, login_rx) = mpsc::unbounded_channel();
        // The door of a live account: `FakeControl::arrive` hands an arrival to this actor.
        let (door_tx, door_rx) = mpsc::unbounded_channel();
        let _door = match self.refresh_known() {
            Ok(()) => self
                .control
                .open_door(self.uuid, door_tx, self.known.clone()),
            Err(e) => return Exit::Failed(e),
        };
        let stop = tokio::select! {
            s = self.forwarder() => s,
            s = self.runner(login_rx) => s,
            s = self.commands(commands, login_tx) => s,
            s = self.arrivals(door_rx) => s,
        };
        match stop {
            Stop::Now(exit) => exit,
            Stop::Drain(exit) => {
                let fwd = Forwarder::new(self.db.clone(), ForwarderConfig::default());
                let drained =
                    tokio::time::timeout(DRAIN_TIMEOUT, fwd.drain_with(|b| self.ship(b))).await;
                if !matches!(drained, Ok(Ok(()))) {
                    tracing::warn!(uuid = %self.uuid.short(), "core_outbox not drained before exit");
                }
                exit
            }
        }
    }

    // ---------- durable state ----------

    fn get(&self, key: &str) -> Result<i64, BackendError> {
        self.db
            .read(|c| -> kourier_db::Result<Option<i64>> {
                Ok(
                    c.query_row("SELECT value FROM fake_state WHERE key = ?1", [key], |r| {
                        r.get(0)
                    })
                    .optional()?,
                )
            })
            .map(|v| v.unwrap_or(0))
            .map_err(internal)
    }

    fn linked(&self) -> Result<bool, BackendError> {
        Ok(self.get("linked")? != 0)
    }

    fn cursor(&self) -> Result<usize, BackendError> {
        Ok(usize::try_from(self.get("cursor")?).unwrap_or(0))
    }

    fn set_in(tx: &kourier_db::Tx<'_>, key: &str, value: i64) -> kourier_db::Result<()> {
        tx.execute(
            "INSERT INTO fake_state(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            (key, value),
        )?;
        Ok(())
    }

    /// Raises the timer versions of conversations to what `events` state: our own timer
    /// changes count on from the version the network last gave
    /// ([`change_conversation`](Self::change_conversation)).
    fn note_timer_versions(
        tx: &kourier_db::Tx<'_>,
        events: &[BackendEvent],
    ) -> kourier_db::Result<()> {
        for (key, version) in timer_versions(events) {
            let current: Option<i64> = tx
                .query_row("SELECT value FROM fake_state WHERE key = ?1", [&key], |r| {
                    r.get(0)
                })
                .optional()?;
            if current.is_none_or(|c| c < version) {
                Self::set_in(tx, &key, version)?;
            }
        }
        Ok(())
    }

    /// Keeps the account's pin list: the pins `events` state, as `pin:<conversation>` keys
    /// whose value is the place in the list (a `PinnedConversations` list replaces them all,
    /// a patch with a `pinned_order` sets or clears one). It is what the network has told the core,
    /// so [`pinned`](Self::pinned) is the list the core shows.
    fn note_pins(tx: &kourier_db::Tx<'_>, events: &[BackendEvent]) -> kourier_db::Result<()> {
        for e in events {
            match e {
                BackendEvent::ConversationUpsert(p) => match p.pinned_order {
                    Patch::Set(place) => Self::set_in(tx, &pin_key(&p.key), i64::from(place))?,
                    Patch::Clear => {
                        tx.execute("DELETE FROM fake_state WHERE key = ?1", [pin_key(&p.key)])?;
                    }
                    Patch::Unchanged => {}
                },
                BackendEvent::PinnedConversations(keys) => {
                    // (`PIN_PREFIX`)
                    tx.execute("DELETE FROM fake_state WHERE key LIKE 'pin:%'", [])?;
                    for (place, key) in keys.iter().enumerate() {
                        let place = i64::try_from(place).unwrap_or(i64::MAX);
                        Self::set_in(tx, &pin_key(key), place)?;
                    }
                }
                BackendEvent::ConversationRemoved { conv } => {
                    tx.execute("DELETE FROM fake_state WHERE key = ?1", [pin_key(conv)])?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The account's pinned chats in the order the network lists them.
    fn pinned(&self) -> Result<Vec<ConvKey>, BackendError> {
        let keys: Vec<String> = self
            .db
            .read(|c| -> kourier_db::Result<Vec<String>> {
                // (`PIN_PREFIX`)
                let mut stmt = c.prepare(
                    "SELECT key FROM fake_state WHERE key LIKE 'pin:%' ORDER BY value, key",
                )?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .map_err(internal)?;
        keys.iter()
            .map(|k| ConvKey::parse(&k[PIN_PREFIX.len()..]).map_err(internal))
            .collect()
    }

    fn advance(&self, cursor: usize) -> Result<(), BackendError> {
        let next = i64::try_from(cursor + 1).unwrap_or(i64::MAX);
        self.db
            .zone(|tx| Self::set_in(tx, "cursor", next))
            .map_err(internal)?;
        self.control.reached(self.uuid, cursor + 1);
        Ok(())
    }

    /// A durable command (§4.8): its job (unique on `cmd`, so a command the core sends again
    /// is one job) and `CommandPersisted` in one transaction. `payload`: what it asked for,
    /// kept for the commands tests look into.
    fn persist_command(
        &self,
        cmd: kourier_model::CmdId,
        kind: &str,
        command: DurableCommand,
        keep: bool,
    ) -> Result<(), BackendError> {
        self.persist_command_with(cmd, kind, command, keep, Vec::new())
    }

    /// [`persist_command`](Self::persist_command) for a command whose effect the network states
    /// with it: `effects` are committed in the same transaction, ahead of `CommandPersisted`.
    fn persist_command_with(
        &self,
        cmd: kourier_model::CmdId,
        kind: &str,
        command: DurableCommand,
        keep: bool,
        mut effects: Vec<BackendEvent>,
    ) -> Result<(), BackendError> {
        let payload = keep
            .then(|| serde_json::to_string(&command))
            .transpose()
            .map_err(internal)?;
        let now = self.now();
        let mut added = false;
        effects.push(BackendEvent::CommandPersisted { cmd });
        self.commit_events(&effects, |tx| {
            added = tx.execute(
                "INSERT OR IGNORE INTO jobs(cmd, kind, created_at, payload)
                 VALUES (?1, ?2, ?3, ?4)",
                (cmd.0, kind, now, &payload),
            )? > 0;
            Ok(())
        })?;
        if added {
            self.control.persisted(self.uuid, command);
        }
        Ok(())
    }

    /// Reports a request for lost files sent and commits the phone's scripted answer
    /// ([`crate::script::BackfillScript`]).
    fn answer_backfill(
        &self,
        target: kourier_model::MsgTarget,
        request: kourier_model::CmdId,
    ) -> Result<(), BackendError> {
        use crate::script::BackfillScript;
        use kourier_backend_api::{RestoreSlot, RestoreUpdate};
        let at = self.now();
        let mut events = vec![BackendEvent::AttachmentRestore {
            target: target.clone(),
            update: RestoreUpdate::Sent,
            at,
            cmd: Some(request),
        }];
        let answer = match &self.script.backfill {
            None | Some(BackfillScript::Silent) => None,
            Some(BackfillScript::NotFound) => Some(RestoreUpdate::MessageNotFound),
            Some(BackfillScript::Files(slots)) => Some(RestoreUpdate::Answer {
                attachments: slots
                    .iter()
                    .enumerate()
                    .map(|(i, slot)| match slot {
                        Some(p) => RestoreSlot::File {
                            remote: p.to_blob(),
                            // A pointer the phone re-uploaded states the digest of its blob:
                            // the core takes no pointer it could not check at the download.
                            net_ids: Redacted::new(vec![format!("digest:fake-{i}")]),
                            size: p
                                .plaintext()
                                .and_then(Result::ok)
                                .map_or(0, |b| b.len() as u64),
                        },
                        None => RestoreSlot::Gone,
                    })
                    .collect(),
                long_text: None,
            }),
        };
        events.extend(answer.map(|update| BackendEvent::AttachmentRestore {
            target,
            update,
            at,
            cmd: None,
        }));
        self.commit_events(&events, |_| Ok(()))
    }

    /// Increments a durable counter and returns the new value.
    fn bump(&self, key: &str) -> Result<i64, BackendError> {
        self.db
            .zone(|tx| -> kourier_db::Result<i64> {
                let v: i64 = tx
                    .query_row("SELECT value FROM fake_state WHERE key = ?1", [key], |r| {
                        r.get(0)
                    })
                    .optional()?
                    .unwrap_or(0)
                    + 1;
                Self::set_in(tx, key, v)?;
                Ok(v)
            })
            .map_err(internal)
    }

    fn now(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX)
    }

    /// Appends `events` (one `core_outbox` row) and the mirrored ones, in one transaction with
    /// `extra` (§4.1 principle 6), then wakes the forwarder.
    fn commit_events(
        &self,
        events: &[BackendEvent],
        extra: impl FnOnce(&kourier_db::Tx<'_>) -> kourier_db::Result<()>,
    ) -> Result<(), BackendError> {
        let row = encode_outbox_row(events).map_err(internal)?;
        let mirrored: Vec<String> = events
            .iter()
            .filter(|e| is_mirrored(e))
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()
            .map_err(internal)?;
        let now = self.now();
        self.db
            .zone(|tx| -> kourier_db::Result<()> {
                extra(tx)?;
                Self::note_timer_versions(tx, events)?;
                Self::note_pins(tx, events)?;
                tx.outbox_append(&row, now)?;
                for m in &mirrored {
                    tx.execute("INSERT INTO mirror(event) VALUES (?1)", [m])?;
                }
                Ok(())
            })
            .map_err(internal)?;
        failpoint::hit(FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
        self.wake.notify_one();
        Ok(())
    }

    /// Re-emits the account and its mirrored state after a `core.db` rebuild.
    fn rebuild(&self) -> Result<(), BackendError> {
        let Some(ready) = self.identity_event()? else {
            return Ok(());
        };
        let mirror: Vec<String> = self
            .db
            .read(|c| -> kourier_db::Result<Vec<String>> {
                let mut stmt = c.prepare("SELECT event FROM mirror ORDER BY seq")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .map_err(internal)?;
        let mut events = vec![ready];
        for m in mirror {
            events.push(serde_json::from_str(&m).map_err(internal)?);
        }
        let rows = events
            .chunks(100)
            .map(encode_outbox_row)
            .collect::<Result<Vec<_>, _>>()
            .map_err(internal)?;
        let now = self.now();
        self.db
            .zone(|tx| -> kourier_db::Result<()> {
                for r in &rows {
                    tx.outbox_append(r, now)?;
                }
                Ok(())
            })
            .map_err(internal)?;
        failpoint::hit(FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
        self.wake.notify_one();
        Ok(())
    }

    fn identity_event(&self) -> Result<Option<BackendEvent>, BackendError> {
        let row: Option<(String, Option<u32>, Option<String>)> = self
            .db
            .read(|c| -> kourier_db::Result<_> {
                Ok(c.query_row(
                    "SELECT own_ids, device, display_name FROM fake_identity WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?)
            })
            .map_err(internal)?;
        let Some((own, device, name)) = row else {
            return Ok(None);
        };
        let own_ids: Vec<UserId> = serde_json::from_str(&own).map_err(internal)?;
        Ok(Some(BackendEvent::AccountReady {
            own_ids,
            device,
            display_name: name.map(Redacted::new),
            number: self.script.number.clone().map(Redacted::new),
        }))
    }

    // ---------- forwarder ----------

    async fn forwarder(&self) -> Stop {
        let cfg = ForwarderConfig {
            max_rows: 100,
            retry_min: Duration::from_millis(20),
            retry_max: Duration::from_secs(1),
        };
        let fwd = Forwarder::new(self.db.clone(), cfg);
        let result: kourier_db::Result<Infallible> =
            fwd.run_with(&self.wake, |b| self.ship(b)).await;
        match result {
            Ok(never) => match never {},
            Err(e) => Stop::Now(Exit::Failed(internal(e))),
        }
    }

    /// Ships one batch as one envelope and resolves after the core's commit. A row that does
    /// not decode, or that the core refuses for good, is a [`Refusal::Reject`]: the forwarder
    /// quarantines it instead of blocking the account.
    async fn ship(&self, batch: Batch) -> Result<(), Refusal> {
        let mut events = Vec::new();
        for row in batch.rows() {
            events.extend(
                decode_outbox_row(&row.envelope).map_err(|e| Refusal::Reject(e.to_string()))?,
            );
        }
        if let Some(d) = self.script.forward_delay {
            self.clock.sleep(d).await;
        }
        let ready = events
            .iter()
            .any(|e| matches!(e, BackendEvent::AccountReady { .. }));
        let (env, waiter) = Envelope::durable(events);
        let linking = self.linking.get().then(|| env.validate_linking());
        if let Some(Err(v)) = linking {
            self.control.violation(v.to_string());
        } else if let Err(v) = conformance::check_envelope(&env, &self.script.own_ids) {
            self.control.violation(v.to_string());
        }
        self.events
            .send(env)
            .await
            .map_err(|_| Refusal::Retry("the core is gone".to_owned()))?;
        match waiter.wait().await {
            Ok(()) => {
                if ready {
                    self.linking.set(false);
                }
                Ok(())
            }
            Err(e) if e.retryable => Err(Refusal::Retry(e.detail)),
            Err(e) => Err(Refusal::Reject(e.detail)),
        }
    }

    // ---------- own sends ----------

    /// The recipients of `conv` as this backend knows them: the peer of a direct chat, else the
    /// members from the mirrored conversation patches; never our own ids.
    fn recipients(&self, conv: &ConvKey) -> Result<Vec<UserId>, BackendError> {
        let own = &self.script.own_ids;
        if let ConvKey::Direct(peer) = conv {
            return Ok(if own.contains(peer) {
                Vec::new()
            } else {
                vec![peer.clone()]
            });
        }
        let mirror: Vec<String> = self
            .db
            .read(|c| -> kourier_db::Result<Vec<String>> {
                let mut stmt = c.prepare("SELECT event FROM mirror ORDER BY seq")?;
                let rows = stmt.query_map([], |r| r.get(0))?;
                Ok(rows.collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .map_err(internal)?;
        let mut members: Vec<Member> = Vec::new();
        for m in mirror {
            if let BackendEvent::ConversationUpsert(p) =
                serde_json::from_str(&m).map_err(internal)?
                && p.key == *conv
                && let Some(list) = &p.members
            {
                list.apply(&mut members);
            }
        }
        Ok(members
            .into_iter()
            .map(|m| m.user)
            .filter(|u| !own.contains(u))
            .collect())
    }

    /// One transmit attempt against the fake server: scripted rules decide failures (their
    /// counts are durable); accepted copies go to `fake_transmits`, which is also the send
    /// progress, so recipients that have the message are never sent it again.
    fn transmit(&self, req: &OutgoingRequest) -> Result<(TransmitOutcome, Copies), BackendError> {
        // Like a real backend before its upload: every attachment's blob must read back whole.
        if let kourier_backend_api::OutgoingKind::Message {
            body: kourier_model::Body::Media { attachments, .. },
            ..
        } = &req.kind
        {
            for a in attachments {
                if let Err(e) = crate::attachments::read_outgoing(&self.blob_dir, a) {
                    return Ok((TransmitOutcome::Failed(e), Copies::Nothing));
                }
            }
        }
        // Like a real backend, an edit, delete or reaction is checked before it is sent.
        if let Some(e) = invalid_change(&req.kind) {
            return Ok((TransmitOutcome::Failed(e), Copies::Nothing));
        }
        let recipients = match (&req.only, &req.kind) {
            (Some(only), _) => only.clone(),
            // A delete for me goes to our other devices only (Signal `deleteForMe` sync).
            (
                None,
                kourier_backend_api::OutgoingKind::Delete {
                    for_everyone: false,
                    ..
                },
            ) => self.script.own_ids.iter().take(1).cloned().collect(),
            (None, _) => self.recipients(&req.conv)?,
        };
        let rules = &self.script.transmit;
        let now = self.now();
        // A Telegram-like server names the message and tells the account so (KT-2).
        let assigned = self.assigned(req);
        let echo = assigned
            .as_ref()
            .map(|a| self.echo_row(req, a, now))
            .transpose()?;
        let outcome = self
            .db
            .zone(|tx| -> kourier_db::Result<(TransmitOutcome, Copies)> {
                let used = |tx: &kourier_db::Tx<'_>, i: usize| -> kourier_db::Result<u32> {
                    let v: Option<i64> = tx
                        .query_row(
                            "SELECT value FROM fake_state WHERE key = ?1",
                            [format!("transmit-rule:{i}")],
                            |r| r.get(0),
                        )
                        .optional()?;
                    Ok(u32::try_from(v.unwrap_or(0)).unwrap_or(u32::MAX))
                };
                // A solved challenge ends every challenge rule (the server stops answering 428).
                let solved: bool = tx
                    .query_row(
                        "SELECT value FROM fake_state WHERE key = 'challenge-solved'",
                        [],
                        |r| r.get::<_, i64>(0),
                    )
                    .optional()?
                    .is_some_and(|v| v != 0);
                let applies = |r: &crate::script::TransmitRule| {
                    r.conv.as_ref().is_none_or(|c| *c == req.conv)
                        && r.kind.as_deref().is_none_or(|k| k == req.kind.name())
                        && !(solved && matches!(r.error.kind, ErrorKind::Challenge { .. }))
                };
                // The fake handed out its challenge token: the answer must come back with it.
                let issue = |tx: &kourier_db::Tx<'_>, e: &BackendError| {
                    if matches!(e.kind, ErrorKind::Challenge { .. }) {
                        Self::set_in(tx, "challenge-issued", 1)
                    } else {
                        Ok(())
                    }
                };
                for (i, r) in rules.iter().enumerate() {
                    if r.member.is_none() && applies(r) && used(tx, i)? < r.times {
                        Self::set_in(tx, &format!("transmit-rule:{i}"), i64::from(used(tx, i)? + 1))?;
                        issue(tx, &r.error)?;
                        return Ok((TransmitOutcome::Failed(r.error.clone()), Copies::Nothing));
                    }
                }
                let (mut delivered, mut failed) = (Vec::new(), Vec::new());
                let (mut inserted, mut held_already) = (0usize, 0usize);
                // The server's key of a copy is `(local, recipient, conversation)`: ids of a
                // network with per-conversation ids may repeat in another chat of one
                // person, and that copy is a new message (a row of an older build, with no
                // conversation, counts for any chat).
                let conv = req.conv.encode();
                'recipients: for u in recipients {
                    let have: i64 = tx.query_row(
                        "SELECT count(*) FROM fake_transmits
                         WHERE local = ?1 AND recipient = ?2 AND conv IN (?3, '')",
                        (req.local.as_ref(), u.as_str(), conv.as_str()),
                        |r| r.get(0),
                    )?;
                    if have == 0 {
                        for (i, r) in rules.iter().enumerate() {
                            if r.member.as_ref() == Some(&u) && applies(r) && used(tx, i)? < r.times {
                                Self::set_in(tx, &format!("transmit-rule:{i}"), i64::from(used(tx, i)? + 1))?;
                                issue(tx, &r.error)?;
                                failed.push((u, r.error.clone()));
                                continue 'recipients;
                            }
                        }
                        tx.execute(
                            "INSERT INTO fake_transmits(local, sent_at, recipient, recipient_update, at, conv)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            (
                                req.local.as_ref(),
                                req.sent_at,
                                u.as_str(),
                                req.is_recipient_update,
                                now,
                                conv.as_str(),
                            ),
                        )?;
                        inserted += 1;
                    } else {
                        held_already += 1;
                    }
                    delivered.push((u, RecipientState::Sent { sealed: true }));
                }
                let copies = if inserted > 0 {
                    Copies::New
                } else if held_already > 0 {
                    Copies::Repeat
                } else {
                    Copies::Nothing
                };
                let outcome = if !failed.is_empty() {
                    TransmitOutcome::Partial { delivered, failed }
                } else if let Some(assigned) = &assigned {
                    if let Some(row) = &echo
                        && Self::first_echo(tx, req)?
                    {
                        tx.outbox_append(row, now)?;
                    }
                    TransmitOutcome::SentAs {
                        assigned: assigned.clone(),
                        server_at: Some(now),
                        per_recipient: delivered,
                    }
                } else {
                    TransmitOutcome::Sent {
                        server_at: Some(now),
                        per_recipient: delivered,
                    }
                };
                Ok((outcome, copies))
            })
            .map_err(internal)?;
        failpoint::hit(FAILPOINT_AFTER_SERVER_ACCEPT);
        Ok(outcome)
    }

    /// What the server of a Telegram-like account calls a new message (KT-2): its `sent_at` in
    /// decimal, which is also its position. DERIVED from the request, so a resend under the
    /// allocated id gets the same answer without the fake keeping a map, and strictly rising per
    /// account because the core allocates its `sent_at` so (`outbox::allocate`). `None` for a
    /// flavour whose server names nothing and for a request that is no message: an edit, a delete
    /// and a reaction name a message that has its id, under the allocated or the assigned one (the
    /// fake looks at neither) and are answered `Sent`.
    fn assigned(&self, req: &OutgoingRequest) -> Option<AssignedId> {
        (self.flavor == Flavor::TelegramLike
            && matches!(req.kind, kourier_backend_api::OutgoingKind::Message { .. }))
        .then(|| AssignedId {
            local: req.sent_at.to_string().into(),
            seq: req.sent_at,
        })
    }

    /// The echo of an accepted message, as the `core_outbox` row it is committed in: what a
    /// Telegram-like server tells the sender's account of the message it just took, one own row
    /// under the assigned id with its position, the request's content and the id it was allocated
    /// under (unless the script's echo knows no binding), and the `server_at` of the answer, so the
    /// order the two arrive in changes nothing.
    fn echo_row(
        &self,
        req: &OutgoingRequest,
        assigned: &AssignedId,
        server_at: i64,
    ) -> Result<Vec<u8>, BackendError> {
        let kourier_backend_api::OutgoingKind::Message { body, reply_to, .. } = &req.kind else {
            return Err(internal("an echo of something that is no message"));
        };
        let mut echo = kourier_backend_api::IncomingMessage::new(
            req.conv.clone(),
            assigned.local.clone(),
            kourier_model::Origin::OwnThisDevice,
            body.clone(),
            req.sent_at,
            server_at,
        );
        echo.reply_to.clone_from(reply_to);
        echo.server_at = Some(server_at);
        echo.net_seq = Some(assigned.seq);
        echo.allocated_as = (!self.script.echo_without_binding).then(|| req.local.clone());
        encode_outbox_row(&[BackendEvent::Message(echo)]).map_err(internal)
    }

    /// Whether this is the first time the server takes `req`'s message: the echo is committed ONCE
    /// per `(conversation, local)`, in the transaction of the accept like any backend's effects
    /// (§4.1 principle 6). A resend, a follow-up to more recipients and the retry of a partial send
    /// tell the core nothing more; a durable mark in `fake_state` holds that, as it holds the
    /// counts of the transmit rules, and a chat with nobody else in it, which has no copy to
    /// remember, still gets exactly one.
    fn first_echo(tx: &kourier_db::Tx<'_>, req: &OutgoingRequest) -> kourier_db::Result<bool> {
        let mark = format!("echo:{}:{}", req.conv.encode(), req.local);
        Ok(tx.execute(
            "INSERT OR IGNORE INTO fake_state(key, value) VALUES (?1, 1)",
            [mark],
        )? > 0)
    }

    fn record_transmit(
        &self,
        message: LocalMsgId,
        req: OutgoingRequest,
        outcome: &TransmitOutcome,
    ) {
        self.control.transmitted(
            self.uuid,
            TransmitRecord {
                message,
                req,
                at_ms: self.clock.now_ms(),
                outcome: outcome.clone(),
            },
        );
    }

    fn outbox_empty(&self) -> Result<bool, BackendError> {
        self.db
            .read(|c| -> kourier_db::Result<bool> {
                let inbox: i64 = c.query_row("SELECT count(*) FROM inbox", [], |r| r.get(0))?;
                Ok(outbox::len(c)? == 0 && inbox == 0)
            })
            .map_err(internal)
    }

    /// Waits until every committed event was acked by the core.
    async fn wait_acked(&self) -> Result<(), BackendError> {
        while !self.outbox_empty()? {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        Ok(())
    }

    // ---------- commands ----------

    async fn commands(
        &self,
        rx: mpsc::Receiver<Command>,
        login: mpsc::UnboundedSender<LoginCmd>,
    ) -> Stop {
        let mut downloads = Downloads::new();
        // The upload phases end with the actor (their `Transmit` replies are dropped, as a
        // backend that goes away drops them).
        let mut uploads = Uploads::new();
        let stop = self.serve(rx, login, &mut downloads, &mut uploads).await;
        // A file that is being written is finished before the actor goes: its target may
        // be asked for again by the next actor, and its directory may be removed.
        while downloads.join_next().await.is_some() {}
        stop
    }

    /// The fake server has taken (or refused) the request: records it and gives the core the
    /// outcome, at once or, in a conversation with a late reply or a slow send, after the delay
    /// (the server has the request, the core hears later).
    ///
    /// A message the server named (`SentAs`) has its echo committed with the accept
    /// ([`Self::echo_row`]). By default the core has the echo, acked, BEFORE it hears the outcome
    /// ("echo first"); a script that holds the forwarder back ([`Script::forward_delay`]) gets the
    /// other order, the outcome at once and the echo behind it.
    async fn answer(
        &self,
        local: LocalMsgId,
        req: Box<OutgoingRequest>,
        (outcome, copies): (TransmitOutcome, Copies),
        reply: tokio::sync::oneshot::Sender<TransmitOutcome>,
    ) -> Result<(), BackendError> {
        if matches!(outcome, TransmitOutcome::SentAs { .. }) {
            self.wake.notify_one();
            if self.script.forward_delay.is_none() {
                self.wait_acked().await?;
            }
        }
        let delay = self
            .script
            .reply_delays
            .iter()
            .find(|(c, _)| *c == req.conv)
            .map(|(_, d)| *d)
            .or(self.script.slow_sends.map(|(_, d)| d));
        // A slow send reports its phase now and again every second while it
        // lasts, as a real backend does (the first report can come before the
        // UI has the row).
        let phase = self
            .script
            .slow_sends
            .and_then(|(p, _)| p)
            .filter(|_| delay.is_some())
            .map(|p| BackendEvent::TransmitProgress {
                local,
                progress: 0.0,
                phase: Some(p),
            });
        if let Some(e) = &phase {
            self.send_ephemeral(e.clone()).await;
        }
        let late = delay.map(|d| self.clock.sleep(d));
        // The run tool's rehearsal counts the two lines the real backend says at its write point (T2 F1).
        let counted = self.plays_the_rehearsal();
        let (outcome_word, outcome_code, outcome_again) =
            crate::rehearsal::answered_words(&outcome);
        if counted {
            crate::rehearsal::say_frame_written(crate::rehearsal::frame_word(&req.kind));
            if copies == Copies::Repeat {
                crate::rehearsal::say_repeat_dropped();
            }
        }
        // T2 F8: the rehearsal's resend text is taken by the server and its ack is held back for good, so the run tool
        // kills the app before the ack and the next start's resend (the same id) is real.
        let held =
            counted && copies == Copies::New && crate::rehearsal::holds_the_ack_of(&req.kind);
        self.record_transmit(local, *req, &outcome);
        if held {
            tokio::spawn(async move {
                let _the_core_waits = reply;
                std::future::pending::<()>().await;
            });
            return Ok(());
        }
        match late {
            // A late answer: the server has the request, the core hears later.
            Some(sleep) => {
                let events = self.events.clone();
                tokio::spawn(async move {
                    let again = async {
                        let Some(e) = phase else {
                            return std::future::pending().await;
                        };
                        loop {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            let _ = events.send(Envelope::ephemeral(e.clone())).await;
                        }
                    };
                    tokio::select! {
                        () = sleep => {}
                        () = again => {}
                    }
                    if counted {
                        crate::rehearsal::say_frame_answered(
                            outcome_word,
                            outcome_code,
                            &outcome_again,
                        );
                    }
                    let _ = reply.send(outcome);
                });
            }
            None => {
                if counted {
                    crate::rehearsal::say_frame_answered(
                        outcome_word,
                        outcome_code,
                        &outcome_again,
                    );
                }
                let _ = reply.send(outcome);
            }
        }
        Ok(())
    }

    /// Whether this fake plays WhatsApp for the run tool's rehearsal (the script honours the number tail and the
    /// network is not Telegram): only then it says the two count lines of the real backend's write point.
    fn plays_the_rehearsal(&self) -> bool {
        self.script.honour_number_tail && self.kind != kourier_model::NetworkKind::Telegram
    }

    /// A message's upload phase ended: the fake server takes it (and the transmit is
    /// recorded), or, when an abort stopped the phase, nothing reaches the server and the
    /// attempt fails.
    async fn finish_upload(
        &self,
        (local, req, reply, ended): upload::Finished,
    ) -> Result<(), BackendError> {
        self.holds.borrow_mut().remove(&local);
        match ended {
            upload::Ended::Stopped => {
                let _ = reply.send(TransmitOutcome::Failed(upload::stopped()));
            }
            upload::Ended::Over => {
                let outcome = self.transmit(&req)?;
                self.answer(local, req, outcome, reply).await?;
            }
        }
        Ok(())
    }

    /// Commits the answers of finished downloads, as many as are ready, in one transaction.
    fn finish_downloads(
        &self,
        first: Downloaded,
        downloads: &mut Downloads,
    ) -> Result<(), BackendError> {
        let mut done = vec![first];
        while let Some(Ok(next)) = downloads.try_join_next() {
            done.push(next);
        }
        let events: Vec<BackendEvent> = done
            .into_iter()
            .map(
                |(attachment, request, result)| BackendEvent::AttachmentDone {
                    attachment,
                    request: Some(request),
                    result,
                },
            )
            .collect();
        // One `core_outbox` row each (an envelope is refused or taken as a whole, and the
        // answers have nothing to do with each other), one transaction for all of them.
        let rows: Vec<Vec<u8>> = events
            .iter()
            .map(|e| encode_outbox_row(std::slice::from_ref(e)))
            .collect::<Result<_, _>>()
            .map_err(internal)?;
        let now = self.now();
        self.db
            .zone(|tx| -> kourier_db::Result<()> {
                for row in &rows {
                    tx.outbox_append(row, now)?;
                }
                Ok(())
            })
            .map_err(internal)?;
        failpoint::hit(FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
        self.wake.notify_one();
        Ok(())
    }

    async fn serve(
        &self,
        mut rx: mpsc::Receiver<Command>,
        login: mpsc::UnboundedSender<LoginCmd>,
        downloads: &mut Downloads,
        uploads: &mut Uploads,
    ) -> Stop {
        loop {
            // Downloads run beside the command loop, a few at a time, like a real backend's:
            // the files are written on the blocking pool, and neither the script nor the
            // forwarder waits for a file.
            let cmd = tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(cmd) => cmd,
                    None => break,
                },
                Some(done) = downloads.join_next(), if !downloads.is_empty() => {
                    let Ok(done) = done else { continue };
                    self.send_ephemeral(BackendEvent::DownloadProgress {
                        attachment: done.0,
                        done: done.2.as_ref().map_or(0, |m| m.size),
                        total: done.2.as_ref().map_or(1, |m| m.size),
                    })
                    .await;
                    if let Err(e) = self.finish_downloads(done, downloads) {
                        return Stop::Now(Exit::Failed(e));
                    }
                    continue;
                }
                Some(done) = uploads.join_next(), if !uploads.is_empty() => {
                    let Ok(done) = done else { continue };
                    if let Err(e) = self.finish_upload(done).await {
                        return Stop::Now(Exit::Failed(e));
                    }
                    continue;
                }
            };
            self.control.command(self.uuid, cmd.name());
            match cmd {
                Command::Connect => {
                    self.go.send_replace(true);
                    // Where the history transfer stands is said again at every connect,
                    // also an end, until it is dismissed.
                    self.report_transfer().await;
                }
                Command::Disconnect => {
                    self.go.send_replace(false);
                    self.conn
                        .send_replace(ConnState::Offline { retry_at_ms: None });
                }
                Command::Shutdown => return Stop::Now(Exit::Shutdown),
                Command::Logout => {
                    // A real backend tells its server to forget the device, if it has one,
                    // and says what became of it right before its exit.
                    if let Some(delay) = self.script.logout_delay {
                        self.clock.sleep(delay).await;
                    }
                    let outcome = if !self.linked().unwrap_or(false) {
                        DeviceLogout::NoDevice
                    } else if self.script.logout_unreachable {
                        DeviceLogout::NotReached
                    } else {
                        self.control.device(self.uuid, false);
                        DeviceLogout::Removed
                    };
                    for _ in 0..self.script.logout_backlog.min(6) {
                        self.send_ephemeral(BackendEvent::AccountProgress(
                            kourier_model::AccountProgress::finished(),
                        ))
                        .await;
                    }
                    self.send_ephemeral(BackendEvent::Logout(LogoutReport {
                        outcome,
                        device_name: self.script.device_name.clone(),
                    }))
                    .await;
                    let exit = Exit::LoggedOut {
                        reason: "logged out".into(),
                    };
                    return if self.script.logout_backlog > 0 {
                        // The exit is ready while the report is still queued.
                        Stop::Now(exit)
                    } else {
                        Stop::Drain(exit)
                    };
                }
                Command::NetworkChanged
                | Command::Resumed
                | Command::SetTyping { .. }
                | Command::RefreshContact { .. }
                | Command::FetchHistory { .. } => {}
                Command::StartLogin {
                    flow,
                    relink,
                    taken,
                } => {
                    // The line the real login prints, for a script that honours the number tail
                    // (the rehearsals'): the run tool says "scan now" only after it has read this
                    // token (L2); `phone.rs` has the words.
                    self.log_login_start();
                    self.control.login(self.uuid, flow.clone(), taken);
                    let _ = login.send(LoginCmd::Start { flow, relink });
                }
                Command::SubmitLoginInput { txn, values } => {
                    self.control.login_input(
                        self.uuid,
                        values
                            .iter()
                            .map(|(k, v)| (k.clone(), v.expose().clone()))
                            .collect(),
                    );
                    let _ = login.send(LoginCmd::Input(txn, values));
                }
                Command::CancelLogin => {
                    let _ = login.send(LoginCmd::Cancel);
                }
                Command::Transmit { local, req, reply } => {
                    // A message with files in a conversation whose script has an upload phase
                    // is registered NOW (an abort right behind it must find it); the server
                    // takes it when the phase is over.
                    if let Some((files, script)) = upload::plan(&self.script.uploads, &req) {
                        let hold = upload::Hold::new(&script);
                        self.holds.borrow_mut().insert(local, hold.clone());
                        uploads.spawn_local(upload::run(
                            self.events.clone(),
                            self.clock.clone(),
                            local,
                            files,
                            script,
                            hold,
                            req,
                            reply,
                        ));
                        continue;
                    }
                    let outcome = match self.transmit(&req) {
                        Ok(o) => o,
                        Err(e) => return Stop::Now(Exit::Failed(e)),
                    };
                    if let Err(e) = self.answer(local, req, outcome, reply).await {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                // Answered at once from the message's stop state: only a message in its upload
                // phase ([`Script::uploads`]) can be stopped.
                Command::AbortTransmit { local, reply } => {
                    let answer = self
                        .holds
                        .borrow()
                        .get(&local)
                        .map_or(AbortAnswer::TooLate, |hold| hold.abort());
                    self.control.aborted(
                        self.uuid,
                        AbortRecord {
                            message: local,
                            answer,
                        },
                    );
                    // A command loop that is inside an emit (see `Script::abort_backlog`): the
                    // answer comes after the queue was read.
                    for _ in 0..self.script.abort_backlog {
                        self.send_ephemeral(BackendEvent::AccountProgress(
                            kourier_model::AccountProgress::finished(),
                        ))
                        .await;
                    }
                    let _ = reply.send(answer);
                }
                Command::SendReceipts {
                    cmd,
                    conv,
                    targets,
                    kind,
                } => {
                    let command = DurableCommand::SendReceipts {
                        conv,
                        targets,
                        kind,
                    };
                    if let Err(e) = self.persist_command(cmd, self.job_kind(), command, false) {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::SyncReadMarker {
                    cmd,
                    conv,
                    up_to,
                    read,
                } => {
                    let command = DurableCommand::SyncReadMarker { conv, up_to, read };
                    if let Err(e) = self.persist_command(cmd, self.job_kind(), command, false) {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                // The deletes for me made on this device: a real backend would queue a sync
                // message for the account's other devices. The fake keeps what was asked.
                Command::ClearConversation {
                    cmd,
                    conv,
                    newest,
                    newest_non_expiring,
                    remove,
                    before_ms,
                    system_before_ms,
                } => {
                    let command = DurableCommand::ClearConversation {
                        conv,
                        newest,
                        newest_non_expiring,
                        remove,
                        before_ms,
                        system_before_ms,
                    };
                    if let Err(e) = self.persist_command(cmd, "clear", command, true) {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::DeleteAttachment {
                    cmd,
                    conv,
                    target,
                    index,
                    net_ids,
                } => {
                    let command = DurableCommand::DeleteAttachment {
                        conv,
                        target,
                        index,
                        net_ids,
                    };
                    if let Err(e) = self.persist_command(cmd, "attachment", command, true) {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::RequestAttachments { cmd, conv, target } => {
                    // The request is kept like any durable command, reported sent, and the
                    // phone answers as the script's backfill knob says (none: never).
                    let command = DurableCommand::RequestAttachments {
                        conv,
                        target: target.clone(),
                    };
                    if let Err(e) = self.persist_command(cmd, "restore", command, true) {
                        return Stop::Now(Exit::Failed(e));
                    }
                    if let Err(e) = self.answer_backfill(target, cmd) {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::DownloadAttachment {
                    attachment,
                    request,
                    remote,
                    dest,
                    ..
                } => {
                    // Progress is ephemeral; the result is durable and committed only after the
                    // file is complete (rename + fsync in the blob writer).
                    self.send_ephemeral(BackendEvent::DownloadProgress {
                        attachment,
                        done: 0,
                        total: 1,
                    })
                    .await;
                    if let Some(delay) = self.script.download_delay {
                        // A slow download: the file is complete only after this long.
                        self.clock.sleep(delay).await;
                    }
                    // At most a few at a time; the rest wait for a free place here.
                    while downloads.len() >= PARALLEL_DOWNLOADS {
                        let Some(Ok(done)) = downloads.join_next().await else {
                            break;
                        };
                        if let Err(e) = self.finish_downloads(done, downloads) {
                            return Stop::Now(Exit::Failed(e));
                        }
                    }
                    downloads.spawn_blocking(move || {
                        let result = crate::attachments::download(&remote, &dest);
                        (attachment, request, result)
                    });
                }
                Command::ChangeConversation {
                    conv,
                    change,
                    id,
                    reply,
                } => {
                    self.control.change(self.uuid, change.clone());
                    // A timer set is a frame of the run tool's budget: the two count lines (T2 F1).
                    let timer = self.plays_the_rehearsal()
                        && matches!(change, ConversationChange::SetTimer { .. });
                    if timer {
                        crate::rehearsal::say_frame_written("timer");
                    }
                    if self.script.reject_changes {
                        if timer {
                            // The core never sends a timer set again: the person asks again (T2 G1).
                            crate::rehearsal::say_frame_answered("nacked", 0, "no");
                        }
                        let _ = reply.send(Err(BackendError::new(
                            ErrorKind::InvalidInput,
                            "transaction rejected",
                        )));
                    } else {
                        let res = self.change_conversation(conv, &change, id);
                        if timer {
                            let (word, code) = if res.is_ok() {
                                ("acked", 0)
                            } else {
                                ("nacked", 0)
                            };
                            crate::rehearsal::say_frame_answered(word, code, "no");
                        }
                        let _ = reply.send(res);
                    }
                }
                Command::SubmitChallenge {
                    token,
                    answer,
                    reply,
                } => {
                    let _ = reply.send(self.submit_challenge(token.as_ref(), &answer));
                }
                Command::SkipHistoryTransfer => {
                    if let Err(e) = self.skip_transfer().await {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::DismissHistoryTransfer => {
                    if let Err(e) = self.dismiss_transfer() {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::ChangeContact { reply, .. }
                | Command::ChangeAccountSettings { reply, .. }
                | Command::ChangeProfile { reply, .. } => {
                    let _ = reply.send(Err(BackendError::unsupported("not scripted")));
                }
                // The account's switch of the roaming settings: the fake re-states its caps with
                // the flag as asked (a fake that states none refuses, like a network that cannot).
                Command::SetRoamingWrites { on, reply } => {
                    let _ = reply.send(self.set_roaming_writes(on));
                }
                // The core's own durable change of a chat (the clearing of a mark when it is
                // read): kept as a job like the other durable commands and acknowledged. While the
                // account's switch is on the network applies it, like the plain change, in the same
                // transaction; with the switch off (or none) it is acknowledged and dropped. A
                // command the core sends again is one job and applies once.
                Command::ChangeConversationDurable {
                    cmd,
                    conv,
                    change,
                    asked_at,
                } => {
                    let effects = match self.durable_effects(cmd, &conv, &change) {
                        Ok(effects) => effects,
                        Err(e) => return Stop::Now(Exit::Failed(e)),
                    };
                    let command = DurableCommand::ChangeConversation {
                        conv,
                        change,
                        asked_at,
                    };
                    if let Err(e) = self.persist_command_with(cmd, "change", command, true, effects)
                    {
                        return Stop::Now(Exit::Failed(e));
                    }
                }
                Command::Resolve { reply, .. } => {
                    let _ = reply.send(Err(BackendError::unsupported("not scripted")));
                }
                Command::SafetyNumber { user, reply } => {
                    let _ = reply.send(Ok(self.safety_number(&user)));
                }
                Command::ConfirmIdentity { user, key, reply } => {
                    let _ = reply.send(self.confirm_identity(user, &key));
                }
                Command::SetVerified {
                    user,
                    verified,
                    key,
                    reply,
                } => {
                    let _ = reply.send(self.set_verified(user, verified, &key));
                }
                Command::ListOwnDevices { reply } => {
                    let _ = reply.send(Ok(vec![OwnDevice {
                        id: self.script.device.unwrap_or(1),
                        name: Some("Kourier".into()),
                        created_at: None,
                        last_seen: None,
                        this_device: true,
                    }]));
                }
            }
        }
        // The core dropped its command sender: it is gone.
        Stop::Now(Exit::Shutdown)
    }

    /// The job kind of receipts and read syncs (by flavor, as before the job table kept
    /// payloads).
    fn job_kind(&self) -> &'static str {
        if matches!(self.flavor, Flavor::SignalLike) {
            "signal"
        } else {
            "job"
        }
    }

    // ---------- roaming settings ----------

    /// The newest `AccountCaps` this account has stated (the script's or the fake's own), from the
    /// mirror.
    fn stated_caps(&self) -> Result<Option<AccountCaps>, BackendError> {
        let row: Option<String> = self
            .db
            .read(|c| -> kourier_db::Result<Option<String>> {
                Ok(c.query_row(
                    "SELECT event FROM mirror WHERE event LIKE '{\"AccountCaps\":%'
                     ORDER BY seq DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?)
            })
            .map_err(internal)?;
        row.map(|json| match serde_json::from_str(&json) {
            Ok(BackendEvent::AccountCaps(caps)) => Ok(caps),
            Ok(_) => Err(internal("a mirrored caps event is no caps")),
            Err(e) => Err(internal(e)),
        })
        .transpose()
    }

    /// The account's switch is on: the stated flag is `Some(true)`.
    fn roaming_on(&self) -> Result<bool, BackendError> {
        Ok(self
            .stated_caps()?
            .and_then(|caps| caps.roaming_writes())
            .unwrap_or(false))
    }

    /// What the fake states of the roaming settings ([`Script::roaming_writes`]) added to the
    /// events of a delivery: a delivered `AccountCaps` that does not state the flag itself gets
    /// the flag and the pin limit, and the first delivery of an account that has stated no caps yet
    /// gets a statement of its own in front. The flag is the one the account has now (a switch
    /// turned since stays turned), else the script's.
    fn with_roaming_statement<'a>(
        &self,
        events: &'a [BackendEvent],
    ) -> Result<Cow<'a, [BackendEvent]>, BackendError> {
        let Some(default) = self.script.roaming_writes else {
            return Ok(Cow::Borrowed(events));
        };
        let carries_caps = events
            .iter()
            .any(|e| matches!(e, BackendEvent::AccountCaps(_)));
        if !carries_caps && self.caps_stated.get() {
            return Ok(Cow::Borrowed(events));
        }
        let on = self
            .stated_caps()?
            .and_then(|caps| caps.roaming_writes())
            .unwrap_or(default);
        let mut out = events.to_vec();
        for e in &mut out {
            if let BackendEvent::AccountCaps(caps) = e
                && caps.roaming_writes().is_none()
            {
                state_roaming(caps, on);
            }
        }
        if !carries_caps {
            let mut caps = AccountCaps::default();
            state_roaming(&mut caps, on);
            out.insert(0, BackendEvent::AccountCaps(caps));
        }
        self.caps_stated.set(true);
        Ok(Cow::Owned(out))
    }

    /// What the account states of the roaming settings again after a link that resumes a script
    /// that was played (`played`: the events it delivered): the unlink wiped `backend.db`, and with
    /// it the caps the account had stated, while the core keeps its stored statement and lets a click
    /// through. It is the newest `AccountCaps` the script played, the flag and the pin limit added
    /// as a delivery would have added them ([`with_roaming_statement`](Self::with_roaming_statement),
    /// with the script's flag: a switch turned since is not remembered), or an `AccountCaps` of the
    /// statement alone when the script played none. `None`: the script states nothing
    /// ([`Script::roaming_writes`] is `None` and no played caps state the flag).
    fn restated_roaming(&self, played: &[BackendEvent]) -> Option<AccountCaps> {
        let newest = played.iter().rev().find_map(|e| match e {
            BackendEvent::AccountCaps(caps) => Some(caps.clone()),
            _ => None,
        });
        if let Some(caps) = newest
            .as_ref()
            .filter(|caps| caps.roaming_writes().is_some())
        {
            return Some(caps.clone());
        }
        let on = self.script.roaming_writes?;
        let mut caps = newest.unwrap_or_default();
        state_roaming(&mut caps, on);
        Some(caps)
    }

    /// `Command::SetRoamingWrites`: the account's caps again, with the flag as asked.
    fn set_roaming_writes(&self, on: bool) -> Result<(), BackendError> {
        let Some(mut caps) = self
            .stated_caps()?
            .filter(|caps| caps.roaming_writes().is_some())
        else {
            return Err(BackendError::unsupported("not scripted"));
        };
        caps.flags
            .insert(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), on);
        self.commit_events(&[BackendEvent::AccountCaps(caps)], |_| Ok(()))
    }

    /// The events a roaming change implies, as the network states them (§4.1 principle 4):
    /// the patch of the change, and for the pin list what a network that keeps one does with it
    /// (a new pin at its end). A pin goes to the END of the account's list (the whole list is
    /// stated) and unarchives the chat; archiving takes the chat out of the list.
    fn roaming_events(
        &self,
        conv: &ConvKey,
        change: &ConversationChange,
    ) -> Result<Vec<BackendEvent>, BackendError> {
        match change {
            ConversationChange::SetPinned { pinned: true } => {
                let mut list = self.pinned()?;
                if !list.contains(conv) {
                    let full = self
                        .stated_caps()?
                        .and_then(|caps| caps.max_pinned_chats)
                        .is_some_and(|limit| list.len() >= usize::try_from(limit).unwrap_or(0));
                    if full {
                        return Err(BackendError::new(
                            ErrorKind::InvalidInput,
                            "too many pinned chats",
                        ));
                    }
                    list.push(conv.clone());
                }
                let mut unarchive = ConversationPatch::new(conv.clone());
                unarchive.archived = Some(false);
                Ok(vec![
                    BackendEvent::PinnedConversations(list),
                    BackendEvent::ConversationUpsert(unarchive),
                ])
            }
            ConversationChange::SetPinned { pinned: false } => {
                let mut list = self.pinned()?;
                list.retain(|k| k != conv);
                Ok(vec![BackendEvent::PinnedConversations(list)])
            }
            ConversationChange::SetArchived { archived: true } => {
                let mut events = vec![BackendEvent::ConversationUpsert(patch_for(
                    conv.clone(),
                    change,
                ))];
                let mut list = self.pinned()?;
                let before = list.len();
                list.retain(|k| k != conv);
                if list.len() != before {
                    events.push(BackendEvent::PinnedConversations(list));
                }
                Ok(events)
            }
            _ => Ok(vec![BackendEvent::ConversationUpsert(patch_for(
                conv.clone(),
                change,
            ))]),
        }
    }

    /// What the network states when it applies a durable change the core sent: the change's
    /// events while the account's switch is on and the command is new to the fake, none otherwise
    /// (a change that cannot be made is acknowledged and dropped, never resent for ever).
    fn durable_effects(
        &self,
        cmd: kourier_model::CmdId,
        conv: &ConvKey,
        change: &ConversationChange,
    ) -> Result<Vec<BackendEvent>, BackendError> {
        if !change.is_roaming() || !self.roaming_on()? {
            return Ok(Vec::new());
        }
        let known: bool = self
            .db
            .read(|c| -> kourier_db::Result<bool> {
                Ok(
                    c.query_row("SELECT 1 FROM jobs WHERE cmd = ?1", [cmd.0], |_| Ok(()))
                        .optional()?
                        .is_some(),
                )
            })
            .map_err(internal)?;
        if known {
            return Ok(Vec::new());
        }
        // (A change the network refuses, a pin over the limit, is dropped like the others.)
        Ok(self.roaming_events(conv, change).unwrap_or_default())
    }

    /// The confirmation of a roaming change (§4.1 principle 4): the patch, and for a timer
    /// change also its version (a durable counter per conversation, as Signal's
    /// `expireTimerVersion`) and the timeline row of our own change under the id the core
    /// allocated for the update (§4.2 `ChangeConversation`). One delivery.
    fn change_conversation(
        &self,
        conv: ConvKey,
        change: &ConversationChange,
        id: Option<kourier_backend_api::AllocatedId>,
    ) -> Result<(), BackendError> {
        if change.is_roaming() && !self.roaming_on()? {
            // The backend is the gate: the core checks the account's statement first, and a
            // click that gets here anyway is refused.
            return Err(BackendError::unsupported(
                "this account does not write its chat settings to the network",
            ));
        }
        let mut patch = patch_for(conv.clone(), change);
        let ConversationChange::SetTimer { timer } = change else {
            let events = self.roaming_events(&conv, change)?;
            return self.commit_events(&events, |_| Ok(()));
        };
        let Some(id) = id else {
            return Err(BackendError::new(
                ErrorKind::InvalidInput,
                "a timer change needs the id of its update",
            ));
        };
        let version = self.bump(&timer_version_key(&conv))?;
        patch.timer_version = Some(u32::try_from(version).unwrap_or(u32::MAX));
        let row = kourier_backend_api::IncomingMessage::new(
            conv,
            id.local,
            kourier_model::Origin::OwnThisDevice,
            kourier_model::Body::System(kourier_model::SystemEvent::TimerChanged {
                by: None,
                timer: *timer,
            }),
            id.sent_at,
            self.now(),
        );
        self.commit_events(
            &[
                BackendEvent::ConversationUpsert(patch),
                BackendEvent::Message(row),
            ],
            |_| Ok(()),
        )
    }

    /// The fake server's answer to a solved challenge: accepted when the answer is the
    /// scripted one (any, when none is scripted) and the token is the one the fake handed out
    /// with a refused transmit (the core must give back the token of the attempt it paused
    /// on; there is none when the challenge was only announced through the connection state).
    /// Accepting ends the challenge rules and, when the connection state shows a challenge,
    /// goes `Online`.
    fn submit_challenge(
        &self,
        token: Option<&Redacted<String>>,
        answer: &Redacted<String>,
    ) -> Result<(), BackendError> {
        let refuse = |detail: &str| Err(BackendError::new(ErrorKind::InvalidInput, detail));
        if token.is_some_and(|t| t.expose() != crate::script::CHALLENGE_TOKEN) {
            return refuse("unknown challenge token");
        }
        let open = self.get("challenge-issued")? != 0 && self.get("challenge-solved")? == 0;
        if open && token.is_none() {
            return refuse("the challenge token is missing");
        }
        if self
            .script
            .challenge_answer
            .as_ref()
            .is_some_and(|expected| expected != answer.expose())
        {
            return refuse("challenge answer rejected");
        }
        self.db
            .zone(|tx| Self::set_in(tx, "challenge-solved", 1))
            .map_err(internal)?;
        let waiting = matches!(
            &*self.conn.borrow(),
            ConnState::NeedsUserAction(kourier_model::UserAction::SolveChallenge { .. })
        );
        if waiting {
            self.conn.send_replace(ConnState::Online { detail: None });
        }
        Ok(())
    }

    // ---------- login ----------

    async fn send_ephemeral(&self, e: BackendEvent) {
        let _ = self.events.send(Envelope::ephemeral(e)).await;
    }

    async fn step_login(&self, step_id: &str, txn: u64, kind: LoginStepKind) {
        self.send_ephemeral(BackendEvent::Login(LoginStep {
            step_id: step_id.into(),
            txn,
            kind,
        }))
        .await;
    }

    /// Runs the scripted login; `Ok(false)` when it did not link (the core stops the actor).
    async fn login(
        &self,
        rx: &mut mpsc::UnboundedReceiver<LoginCmd>,
    ) -> Result<bool, BackendError> {
        let (flow, relink) = loop {
            match rx.recv().await {
                Some(LoginCmd::Start { flow, relink }) => break (flow, relink),
                Some(_) => continue,
                None => return Ok(false),
            }
        };
        if relink.is_some()
            && self
                .control
                .relink_fails(self.uuid, self.script.fail_relinks)
        {
            // The phone refused, or the codes ran out: the account stays logged out.
            let error = BackendError::new(ErrorKind::Unauthorized, "the relink was refused");
            self.step_login("failed", 98, LoginStepKind::Failed { error })
                .await;
            return Ok(false);
        }
        // The flows a script plays: the QR flow always, the pair-code flow when it has one. A
        // flow the network does not have is refused like the network refuses it.
        if flow == "pair-code"
            && let Some(pair) = &self.script.pair_code
        {
            return self.pair_code(pair, relink.as_ref(), rx).await;
        }
        if flow == "phone"
            && let Some(phone) = &self.script.phone_login
        {
            return self.phone_login(phone, relink.as_ref(), rx).await;
        }
        if flow != "qr" {
            let error = BackendError::new(ErrorKind::Unsupported, "this login flow is not offered");
            self.step_login("failed", 97, LoginStepKind::Failed { error })
                .await;
            return Ok(false);
        }
        match &self.script.login {
            LoginScript::Auto => self.link(relink.as_ref(), &self.script.own_ids).await,
            LoginScript::Fail { detail } => {
                let error = BackendError::new(ErrorKind::Unauthorized, detail.clone());
                self.step_login("failed", 1, LoginStepKind::Failed { error })
                    .await;
                Ok(false)
            }
            LoginScript::Qr {
                ask_device_name,
                codes,
                valid_for,
                scanned_at,
                linked_as,
            } => {
                let mut txn = 0;
                if *ask_device_name {
                    txn += 1;
                    let field = InputField {
                        id: "device-name".into(),
                        label: "Device name".into(),
                        kind: InputKind::Text,
                        default: Some("kourier".into()),
                        required: true,
                        note: None,
                        code: None,
                    };
                    self.step_login(
                        "device-name",
                        txn,
                        LoginStepKind::Input {
                            fields: vec![field],
                        },
                    )
                    .await;
                    loop {
                        match rx.recv().await {
                            Some(LoginCmd::Input(t, _)) if t == txn => break,
                            Some(LoginCmd::Cancel) | None => return Ok(false),
                            Some(_) => continue,
                        }
                    }
                }
                let mut scanned = false;
                for i in 0..*codes {
                    txn += 1;
                    let data = format!("fake-link://{}/{i}", self.uuid.short());
                    // The first code may live longer than the others (`Script::first_qr_for`).
                    let lives = match (i, self.script.first_qr_for) {
                        (0, Some(first)) => first,
                        _ => *valid_for,
                    };
                    self.step_login(
                        "qr",
                        txn,
                        LoginStepKind::ShowQr {
                            data: Redacted::new(data),
                            valid_for: Some(lives),
                        },
                    )
                    .await;
                    if *scanned_at == Some(i) {
                        scanned = true;
                        break;
                    }
                    tokio::select! {
                        _ = self.clock.sleep(lives) => {}
                        c = rx.recv() => {
                            if matches!(c, Some(LoginCmd::Cancel) | None) {
                                return Ok(false);
                            }
                        }
                    }
                }
                if !scanned {
                    let error = BackendError::new(ErrorKind::Expired, "the QR codes expired");
                    self.step_login("failed", txn + 1, LoginStepKind::Failed { error })
                        .await;
                    return Ok(false);
                }
                if self.kind == NetworkKind::Threema {
                    // Threema's link goes on from the scan to the emoji the phone shows too: the
                    // person compares them and confirms on the phone (no button here), so the
                    // fake waits for the phone's confirmation (`Script::emoji_for` of the
                    // account clock) and only a cancel ends the wait early. Any other command
                    // (a stray `SubmitLoginInput`, say) is ignored; the QR wait above ends its
                    // rotation on one.
                    txn += 1;
                    let emoji = FAKE_EMOJI.iter().map(|glyph| (*glyph).to_owned()).collect();
                    self.step_login("emoji", txn, LoginStepKind::ConfirmEmoji { emoji })
                        .await;
                    let mut confirmed = self.clock.sleep(self.script.emoji_for);
                    loop {
                        tokio::select! {
                            _ = &mut confirmed => break,
                            c = rx.recv() => {
                                if matches!(c, Some(LoginCmd::Cancel) | None) {
                                    return Ok(false);
                                }
                            }
                        }
                    }
                }
                // TDLib asks the scanned-in account's two-step password right after the scan, and
                // checks the number only after `Ready`: the password first (Telegram's order).
                if let Some(password) = self.qr_password()
                    && !self.password_steps(rx, &mut txn, password).await
                {
                    return Ok(false);
                }
                if self.number_refused() {
                    // The real backend's wrong-phone refusal (W2c): the login ends at the QR
                    // step, nothing is linked, no `AccountReady`, no device.
                    let error = BackendError::new(
                        ErrorKind::NotTheExpectedAccount,
                        "the account is not the expected one",
                    );
                    self.step_login("qr", txn + 1, LoginStepKind::Failed { error })
                        .await;
                    return Ok(false);
                }
                let identity = linked_as
                    .clone()
                    .unwrap_or_else(|| self.script.own_ids.clone());
                self.link(relink.as_ref(), &identity).await
            }
        }
    }

    /// The `pair-code` flow (WhatsApp's "link with phone number"): the phone number is asked
    /// for, the network may refuse it, the code is shown, and the phone types it.
    async fn pair_code(
        &self,
        pair: &PairCodeScript,
        relink: Option<&Relink>,
        rx: &mut mpsc::UnboundedReceiver<LoginCmd>,
    ) -> Result<bool, BackendError> {
        let attempt = self.control.pair_attempt();
        let field = InputField {
            id: "phone".into(),
            label: "Phone number".into(),
            kind: InputKind::Phone,
            default: None,
            required: true,
            note: None,
            code: None,
        };
        self.step_login(
            "phone",
            1,
            LoginStepKind::Input {
                fields: vec![field],
            },
        )
        .await;
        loop {
            match rx.recv().await {
                Some(LoginCmd::Input(1, _)) => break,
                Some(LoginCmd::Cancel) | None => return Ok(false),
                Some(_) => continue,
            }
        }
        if let Some(error) = pair.refusals.get(attempt) {
            // A failed step carries the step it failed at (the vocabulary of the link page).
            self.step_login(
                "phone",
                2,
                LoginStepKind::Failed {
                    error: error.clone(),
                },
            )
            .await;
            return Ok(false);
        }
        self.step_login(
            "code",
            2,
            LoginStepKind::ShowCode {
                code: Redacted::new(pair.code.clone()),
            },
        )
        .await;
        let entered = match pair.entered_after {
            Some(after) => after.min(pair.valid_for),
            None => pair.valid_for,
        };
        tokio::select! {
            _ = self.clock.sleep(entered) => {}
            c = rx.recv() => {
                if matches!(c, Some(LoginCmd::Cancel) | None) {
                    return Ok(false);
                }
            }
        }
        if pair
            .entered_after
            .is_none_or(|after| after > pair.valid_for)
        {
            let error = BackendError::new(ErrorKind::Expired, "the pairing code expired");
            self.step_login("code", 3, LoginStepKind::Failed { error })
                .await;
            return Ok(false);
        }
        self.link(relink, &self.script.own_ids).await
    }

    /// Finishes a link: refuses another identity on relink (before "linking", §4.3 step 6),
    /// stores the identity and emits `AccountReady` in one transaction.
    async fn link(
        &self,
        relink: Option<&Relink>,
        identity: &[UserId],
    ) -> Result<bool, BackendError> {
        if let Some(r) = relink
            && !r.expect_own.is_empty()
            && !identity.iter().any(|i| r.expect_own.contains(i))
        {
            let error = BackendError::new(
                ErrorKind::InvalidInput,
                "this is a different account; add it as a new account",
            );
            self.step_login("failed", 99, LoginStepKind::Failed { error })
                .await;
            return Ok(false);
        }
        self.step_login(
            "linking",
            0,
            LoginStepKind::Progress {
                label: "Linking".into(),
                fraction: None,
            },
        )
        .await;
        if self.script.online_while_linking {
            // The socket is up before the core has seen who we are.
            self.conn.send_replace(ConnState::Online { detail: None });
        }
        let own = serde_json::to_string(identity).map_err(internal)?;
        let ready = BackendEvent::AccountReady {
            own_ids: identity.to_vec(),
            device: self.script.device,
            display_name: self.script.display_name.clone().map(Redacted::new),
            number: self.script.number.clone().map(Redacted::new),
        };
        if !self.script.early_events.is_empty() {
            // Contract-breaking scripts only: durable events before `AccountReady`.
            self.commit_events(&self.script.early_events, |_| Ok(()))?;
        }
        let (device, name) = (self.script.device, self.script.display_name.clone());
        let transfer = self
            .script
            .transfer
            .as_ref()
            .filter(|t| relink.is_none() && !t.stages.is_empty());
        // A relink of an account that ran this script before: what the script delivered is
        // still in the core, so it is not played again (the unlink wiped `backend.db`, and
        // with it the cursor). The script goes on where the account left it: the steps it
        // had not reached when it was unlinked still come. The control remembers how far it
        // got; an account that ran in another process counts as done with its script. What
        // the server would still know of the part that was played is restored: the timer
        // versions its patches stated.
        let resume: Option<usize> = (relink.is_some() && !self.pushed).then(|| {
            let end = self.script.steps.len();
            self.control
                .script_progress(self.uuid)
                .map_or(end, |at| at.min(end))
        });
        let played: Option<Vec<BackendEvent>> = resume.map(|at| {
            self.script
                .steps
                .iter()
                .take(at)
                .filter_map(|s| match s {
                    Step::Deliver(d) => Some(d.events.clone()),
                    _ => None,
                })
                .flatten()
                .collect()
        });
        let cursor = resume.map(|at| i64::try_from(at).unwrap_or(i64::MAX));
        // The statement of the roaming settings a resumed account makes again, as the fake's own
        // memory (the mirror): no event goes to the core, which has the account's stored caps, and a
        // rebuild of the core's database restates it with the rest of the mirror (F8).
        let restated = played
            .as_deref()
            .and_then(|events| self.restated_roaming(events))
            .map(|caps| serde_json::to_string(&BackendEvent::AccountCaps(caps)))
            .transpose()
            .map_err(internal)?;
        self.commit_events(&[ready], |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO fake_identity(id, own_ids, device, display_name)
                 VALUES (1, ?1, ?2, ?3)",
                (&own, device, &name),
            )?;
            if let (Some(cursor), Some(events)) = (cursor, &played) {
                Self::set_in(tx, "cursor", cursor)?;
                Self::note_timer_versions(tx, events)?;
                Self::note_pins(tx, events)?;
            }
            if let Some(caps) = &restated {
                tx.execute("INSERT INTO mirror(event) VALUES (?1)", [caps])?;
            }
            // The history transfer of a fresh link is stored with the account it belongs
            // to, at its first stage ("Link Again" has none).
            if let Some(first) = transfer.and_then(|t| t.stages.first().map(|s| s.0)) {
                Self::store_transfer(tx, first)?;
                Self::set_in(tx, TRANSFER_STAGE, 0)?;
                Self::set_in(tx, TRANSFER_STAGE_SET, 1)?;
            }
            Self::set_in(tx, "linked", 1)
        })?;
        if restated.is_some() {
            // Stated now: no later delivery states the roaming settings in front of its events.
            self.caps_stated.set(true);
        }
        // No await since `AccountReady`: the forwarder finds these rows with it.
        for e in &self.script.with_ready {
            self.commit_events(std::slice::from_ref(e), |_| Ok(()))?;
        }
        self.control.device(self.uuid, true);
        if let Some(transfer) = transfer {
            // The transfer comes before the link is done. The core has the account first
            // (it passes a skip on only to an account it has), and until the transfer ends
            // nothing of the script is delivered.
            self.wait_acked().await?;
            self.conn
                .send_replace(ConnState::Syncing { fraction: None });
            self.walk_transfer(transfer).await?;
        }
        self.step_login("done", 0, LoginStepKind::Done).await;
        // A linked device is connected.
        self.go.send_replace(true);
        Ok(true)
    }

    // ---------- history transfer ----------

    /// Stores `state` as where the account's history transfer stands.
    fn store_transfer(tx: &kourier_db::Tx<'_>, state: HistoryTransfer) -> kourier_db::Result<()> {
        let (kind, done, total) = encode_transfer(state);
        Self::set_in(tx, TRANSFER_KIND, kind)?;
        Self::set_in(tx, TRANSFER_DONE, done)?;
        Self::set_in(tx, TRANSFER_TOTAL, total)?;
        // (A state stored outside a walk has no stage to go on from.)
        Self::set_in(tx, TRANSFER_STAGE_SET, 0)
    }

    /// Where the stored history transfer stands; `None` without one (never started, or
    /// dismissed).
    fn stored_transfer(&self) -> Result<Option<HistoryTransfer>, BackendError> {
        Ok(decode_transfer(
            self.get(TRANSFER_KIND)?,
            self.get(TRANSFER_DONE)?,
            self.get(TRANSFER_TOTAL)?,
        ))
    }

    fn transfer_running(&self) -> Result<bool, BackendError> {
        Ok(self.stored_transfer()?.is_some_and(|s| !s.is_over()))
    }

    /// Says where the history transfer stands, if there is one.
    async fn report_transfer(&self) {
        match self.stored_transfer() {
            Ok(Some(state)) => {
                self.send_ephemeral(BackendEvent::HistoryTransfer(state))
                    .await;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "reading the history transfer failed"),
        }
    }

    /// Stores and reports a state of the walk (`stage`: its place among the stages).
    async fn set_transfer(
        &self,
        state: HistoryTransfer,
        stage: Option<usize>,
    ) -> Result<(), BackendError> {
        self.db
            .zone(|tx| -> kourier_db::Result<()> {
                Self::store_transfer(tx, state)?;
                if let Some(stage) = stage {
                    Self::set_in(tx, TRANSFER_STAGE, i64::try_from(stage).unwrap_or(i64::MAX))?;
                    Self::set_in(tx, TRANSFER_STAGE_SET, 1)?;
                }
                Ok(())
            })
            .map_err(internal)?;
        self.send_ephemeral(BackendEvent::HistoryTransfer(state))
            .await;
        Ok(())
    }

    /// Walks `transfer` from the stored stage to its end. A skip ends a stage that can be
    /// skipped as `Skipped`; it is looked at before every step into the next stage, so one
    /// that arrived while a download ran is not lost to the read that follows.
    async fn walk_transfer(&self, transfer: &TransferScript) -> Result<(), BackendError> {
        self.walking.set(true);
        let mut stage = usize::try_from(self.get(TRANSFER_STAGE)?).unwrap_or(0);
        let end = loop {
            let Some((state, hold)) = transfer.stages.get(stage) else {
                break transfer.end;
            };
            if !state.can_skip() {
                // A skip cannot end this stage, and none is kept for a later one.
                self.skip_pending.set(false);
            } else if self.skip_pending.replace(false) {
                break HistoryTransfer::Skipped;
            }
            self.set_transfer(*state, Some(stage)).await?;
            let counted = transfer
                .import
                .filter(|_| *state == HistoryTransfer::Reading);
            let held = async {
                match (hold, counted) {
                    (Some(d), Some(total)) => {
                        // The delivery's numbers, a quarter at a time.
                        for k in 0..4 {
                            self.report_import(total * k / 4, total).await;
                            self.clock.sleep(*d / 4).await;
                        }
                    }
                    (Some(d), None) => self.clock.sleep(*d).await,
                    (None, counted) => {
                        if let Some(total) = counted {
                            let share = total * TransferScript::HELD_SHARE / 1000;
                            self.report_import(share, total).await;
                        }
                        std::future::pending().await
                    }
                }
            };
            tokio::pin!(held);
            loop {
                tokio::select! {
                    () = &mut held => break,
                    () = self.skip.notified() => {
                        if state.can_skip() && self.skip_pending.get() {
                            break;
                        }
                    }
                }
            }
            if state.can_skip() && self.skip_pending.replace(false) {
                break HistoryTransfer::Skipped;
            }
            stage += 1;
        };
        self.walking.set(false);
        // An end that is not an end would hold the account back for good.
        let end = if end.is_over() {
            end
        } else {
            HistoryTransfer::Skipped
        };
        self.set_transfer(end, None).await?;
        if transfer.import.is_some() {
            self.send_ephemeral(BackendEvent::AccountProgress(AccountProgress::finished()))
                .await;
        }
        Ok(())
    }

    /// Says how many of the history's `total` messages are delivered.
    async fn report_import(&self, done: u64, total: u64) {
        self.send_ephemeral(BackendEvent::AccountProgress(AccountProgress {
            messages_done: done,
            messages_total: total,
            ..AccountProgress::default()
        }))
        .await;
    }

    /// `Command::SkipHistoryTransfer`: ends a wait or a download as skipped; nothing happens
    /// in any other state.
    async fn skip_transfer(&self) -> Result<(), BackendError> {
        if !self.stored_transfer()?.is_some_and(|s| s.can_skip()) {
            return Ok(());
        }
        if self.walking.get() {
            // The walk ends its stage and reports.
            self.skip_pending.set(true);
            self.skip.notify_one();
            return Ok(());
        }
        self.set_transfer(HistoryTransfer::Skipped, None).await?;
        // Receiving is no longer held back.
        if matches!(*self.conn.borrow(), ConnState::Syncing { .. }) {
            self.conn.send_replace(ConnState::Online { detail: None });
        }
        Ok(())
    }

    /// `Command::DismissHistoryTransfer`: forgets a transfer that is over, so it is not
    /// reported again; nothing happens while one runs.
    fn dismiss_transfer(&self) -> Result<(), BackendError> {
        if !self.stored_transfer()?.is_some_and(|s| s.is_over()) {
            return Ok(());
        }
        self.db
            .zone(|tx| Self::set_in(tx, TRANSFER_KIND, 0))
            .map_err(internal)
    }

    // ---------- script ----------

    /// Recomputes the chats the script has delivered (every `Deliver` step before the cursor):
    /// at the start of an actor (a resumed account has them already), after a login (a relink
    /// plays the cursor forward) and after each delivery.
    fn refresh_known(&self) -> Result<(), BackendError> {
        let cursor = self.cursor()?;
        let delivered = self
            .script
            .steps
            .iter()
            .take(cursor)
            .filter_map(|s| match s {
                Step::Deliver(d) => Some(d.events.iter()),
                _ => None,
            })
            .flatten();
        let chats = chats_of(delivered, &self.script.own_ids);
        *self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = chats;
        Ok(())
    }

    /// Adds the chats a delivery creates to the known ones (one delivery's events, not a rescan of
    /// the script: a fixture's script has thousands of steps).
    fn note_chats(&self, events: &[BackendEvent]) {
        let fresh = chats_of(events, &self.script.own_ids);
        let mut known = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for chat in fresh {
            if !known.iter().any(|k| k.conv == chat.conv) {
                known.push(chat);
            }
        }
    }

    /// Takes the arrivals [`FakeControl::arrive`] sends: one text from the other member into the
    /// chat it chose, committed as a live message of the network (a new, unread row); and the
    /// acts [`FakeControl::phone`] sends ([`Actor::cue`]), in the order they were sent.
    async fn arrivals(&self, mut rx: mpsc::UnboundedReceiver<Injection>) -> Stop {
        while let Some(arrival) = rx.recv().await {
            let done = match arrival.act {
                None => self.arrive(arrival),
                Some(act) => self.cue(act, arrival).await,
            };
            if let Err(e) = done {
                return Stop::Now(Exit::Failed(e));
            }
        }
        std::future::pending().await
    }

    fn arrive(&self, arrival: Injection) -> Result<(), BackendError> {
        let n = self.bump("arrivals")?;
        let now = self.now();
        // The fake playing Telegram gives its arrivals Telegram's id shape (a server id shifted
        // by 20 bits, in decimal); every other fake keeps WhatsApp's stanza-id shape.
        let local = if self.kind == NetworkKind::Telegram {
            crate::telegram::arrival_id(n)
        } else {
            format!("{}{n:08}", crate::rehearsal::ARRIVAL_ID_PREFIX)
        };
        let conv = arrival.conv.clone();
        let message = kourier_backend_api::IncomingMessage::new(
            arrival.conv,
            local.clone(),
            kourier_model::Origin::Remote {
                author: arrival.author,
                device: Some(1),
            },
            kourier_model::Body::text(format!("{} {n}", crate::rehearsal::ARRIVAL_WORDS)),
            now,
            now,
        );
        self.commit_events(&[BackendEvent::Message(message)], |_| Ok(()))?;
        self.note_arrived(&conv, local, n);
        Ok(())
    }

    /// `Command::SafetyNumber` ([`crate::Script::safety`]): the held key's number. A script
    /// without safety numbers is a network without them; one that has them but not for
    /// `user` knows no key of that person.
    fn safety_number(&self, user: &UserId) -> kourier_model::SafetyAnswer {
        use kourier_model::SafetyUnavailable;
        if self.script.safety.is_empty() {
            return Err(SafetyUnavailable::Unsupported);
        }
        if self.script.own_ids.contains(user) {
            return Err(SafetyUnavailable::OwnId);
        }
        self.control
            .with_safety(self.uuid, &self.script, user, crate::safety::Held::ask)
            .flatten()
            .ok_or(SafetyUnavailable::NoKey)
    }

    /// `Command::SetVerified`: refused for a key the fake does not hold; else the state, the
    /// contact's trust and key, the quiet notice in the 1:1 chat when the state changed (only
    /// a chat the fake delivered: a notice opens no chat) and `IdentityTrusted` when it was
    /// `Changed`, in one commit.
    fn set_verified(
        &self,
        user: UserId,
        verified: bool,
        key: &kourier_model::IdentityFingerprint,
    ) -> Result<(), BackendError> {
        self.control
            .note_verified(self.uuid, user.clone(), verified);
        let Some(applied) = self
            .control
            .with_safety(self.uuid, &self.script, &user, |h| h.set(verified, key))
        else {
            return Err(BackendError::unsupported("not scripted"));
        };
        let Some(applied) = applied else {
            return Err(BackendError::new(
                kourier_model::ErrorKind::InvalidInput,
                "the safety number changed",
            ));
        };
        self.commit_applied(user, verified, &applied)
    }

    /// `Command::ConfirmIdentity` (BQ-2b, the lead's RC6): a scripted person in state Changed
    /// gets the SHOWN key as Default (the same commit as a Clear of it) or, for another key,
    /// a refusal (`InvalidInput`, nothing changes, so the core keeps the send paused); anyone
    /// else (not scripted, or not Changed) is an approval only, answered `Ok`.
    fn confirm_identity(
        &self,
        user: UserId,
        key: &kourier_model::IdentityFingerprint,
    ) -> Result<(), BackendError> {
        use crate::safety::Confirm;
        match self
            .control
            .with_safety(self.uuid, &self.script, &user, |h| h.confirm(key))
        {
            None | Some(Confirm::Approved) => Ok(()),
            Some(Confirm::Refused) => Err(BackendError::new(
                kourier_model::ErrorKind::InvalidInput,
                "the safety number changed",
            )),
            Some(Confirm::Applied(applied)) => self.commit_applied(user, false, &applied),
        }
    }

    /// The commit of a state the fake took (a Mark, a Clear or a Confirm): the contact's trust
    /// and key, the quiet notice in a delivered 1:1 chat when the state changed, and
    /// `IdentityTrusted` when it was `Changed`.
    fn commit_applied(
        &self,
        user: UserId,
        verified: bool,
        applied: &crate::safety::Applied,
    ) -> Result<(), BackendError> {
        use kourier_model::{Body, ContactPatch, Patch, SystemEvent, VerifyState};
        let mut patch = ContactPatch::new(user.clone());
        patch.trust = Some(crate::safety::trust_of(applied.now));
        patch.identity_fingerprint = Patch::Set(applied.key.clone());
        let mut events = vec![BackendEvent::ContactUpsert(patch)];
        let direct = ConvKey::Direct(user.clone());
        let has_chat = self
            .known
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|k| k.conv == direct);
        if applied.was != applied.now && has_chat {
            let n = self.bump("verified")?;
            let now = self.now();
            let mut message = kourier_backend_api::IncomingMessage::new(
                direct,
                format!("verified:{n:08}"),
                kourier_model::Origin::Remote {
                    author: user.clone(),
                    device: None,
                },
                Body::System(SystemEvent::VerifiedChanged {
                    user: user.clone(),
                    verified,
                    other_device: false,
                }),
                now,
                now,
            );
            message.silent = true;
            events.push(BackendEvent::Message(message));
        }
        if applied.was == VerifyState::Changed {
            events.push(BackendEvent::Notice(
                kourier_backend_api::BackendNotice::IdentityTrusted { user },
            ));
        }
        self.commit_events(&events, |_| Ok(()))
    }

    /// Whether the pairing of this QR login is refused for its number: the script honours the
    /// expected tail, one is set, and the account's number does not end in it (an empty tail or
    /// an account without a number fails closed, as the real backend's does).
    fn number_refused(&self) -> bool {
        self.tail_refuses(self.script.number.as_deref().unwrap_or_default())
    }

    /// Stage 2 of every inbox row: the events go to `core_outbox` and the row is deleted, in one
    /// transaction (§5.6 invariant 2).
    fn flush_inbox(&self) -> Result<(), BackendError> {
        let rows: Vec<(i64, Vec<u8>)> = self
            .db
            .read(|c| -> kourier_db::Result<_> {
                let mut stmt = c.prepare("SELECT seq, events FROM inbox ORDER BY seq")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                Ok(rows.collect::<kourier_db::rusqlite::Result<Vec<_>>>()?)
            })
            .map_err(internal)?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut mirrored = Vec::new();
        let mut delivered = Vec::new();
        for (_, bytes) in &rows {
            for e in decode_outbox_row(bytes).map_err(internal)? {
                if is_mirrored(&e) {
                    mirrored.push(serde_json::to_string(&e).map_err(internal)?);
                }
                delivered.push(e);
            }
        }
        let now = self.now();
        self.db
            .zone(|tx| -> kourier_db::Result<()> {
                Self::note_timer_versions(tx, &delivered)?;
                Self::note_pins(tx, &delivered)?;
                for (seq, bytes) in &rows {
                    tx.outbox_append(bytes, now)?;
                    tx.execute("DELETE FROM inbox WHERE seq = ?1", [seq])?;
                }
                for m in &mirrored {
                    tx.execute("INSERT INTO mirror(event) VALUES (?1)", [m])?;
                }
                Ok(())
            })
            .map_err(internal)?;
        failpoint::hit(FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
        self.wake.notify_one();
        Ok(())
    }

    async fn wait_go(&self, go: &mut watch::Receiver<bool>) {
        while !*go.borrow_and_update() {
            if go.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }

    async fn runner(&self, mut login: mpsc::UnboundedReceiver<LoginCmd>) -> Stop {
        match self.run_script(&mut login).await {
            Ok(Some(stop)) => stop,
            Ok(None) => std::future::pending().await,
            Err(e) => Stop::Now(Exit::Failed(e)),
        }
    }

    async fn run_script(
        &self,
        login: &mut mpsc::UnboundedReceiver<LoginCmd>,
    ) -> Result<Option<Stop>, BackendError> {
        // Inbox rows whose stage 2 did not commit before a crash go first.
        self.flush_inbox()?;
        let mut go = self.go.subscribe();
        let resumed = self.linked()?;
        if !resumed {
            if !self.login(login).await? {
                return Ok(None);
            }
            self.refresh_known()?;
        } else {
            // A start says where the history transfer stands, also one that is over.
            self.report_transfer().await;
            self.wait_go(&mut go).await;
            // A transfer that was cut off by the stop goes on from its stage, before
            // anything else is delivered.
            if let (Some(transfer), true) = (
                self.script.transfer.as_ref(),
                self.get(TRANSFER_STAGE_SET)? != 0 && self.transfer_running()?,
            ) {
                self.conn
                    .send_replace(ConnState::Syncing { fraction: None });
                self.walk_transfer(transfer).await?;
            }
        }
        self.conn.send_replace(ConnState::Connecting);
        let cursor = self.cursor()?;
        if !matches!(self.script.steps.get(cursor), Some(Step::Conn(_))) {
            if let (true, Some(wait)) = (resumed, self.script.catch_up_on_resume) {
                self.conn
                    .send_replace(ConnState::Syncing { fraction: None });
                self.wait_acked().await?;
                self.clock.sleep(wait).await;
            }
            self.conn.send_replace(ConnState::Online { detail: None });
        }
        // What the network states at every connect, once the connection is up.
        for (delay, event) in &self.script.after_connect {
            self.clock.sleep(*delay).await;
            self.send_ephemeral(event.clone()).await;
        }
        loop {
            let cursor = self.cursor()?;
            let Some(step) = self.script.steps.get(cursor) else {
                break;
            };
            self.wait_go(&mut go).await;
            if let Some(stop) = self.step(cursor, step).await? {
                return Ok(Some(stop));
            }
            // Let the forwarder and the command loop run between steps.
            tokio::task::yield_now().await;
        }
        self.wait_acked().await?;
        self.control.mark_finished(self.uuid);
        Ok(None)
    }

    async fn step(&self, cursor: usize, step: &Step) -> Result<Option<Stop>, BackendError> {
        match step {
            Step::Deliver(d) => {
                // Stage 1: the delivery is durable in the inbox together with the cursor. (The
                // script's `AccountCaps` and the first delivery carry what the fake states of
                // the roaming settings.)
                let events = self.with_roaming_statement(&d.events)?;
                let row = encode_outbox_row(&events).map_err(internal)?;
                let next = i64::try_from(cursor + 1).unwrap_or(i64::MAX);
                let step_no = i64::try_from(cursor).unwrap_or(i64::MAX);
                self.db
                    .zone(|tx| -> kourier_db::Result<()> {
                        tx.execute(
                            "INSERT INTO inbox(step, events) VALUES (?1, ?2)",
                            (step_no, &row),
                        )?;
                        Self::set_in(tx, "cursor", next)
                    })
                    .map_err(internal)?;
                self.control.reached(self.uuid, cursor + 1);
                self.note_chats(&d.events);
                failpoint::hit(FAILPOINT_AFTER_INBOX_COMMIT);
                // The server ack would leave here.
                failpoint::hit(FAILPOINT_BEFORE_ACK);
                self.flush_inbox()?;
            }
            Step::Ephemeral(e) => {
                self.send_ephemeral(e.as_ref().clone()).await;
                self.advance(cursor)?;
            }
            Step::Conn(s) => {
                self.conn.send_replace(s.clone());
                self.advance(cursor)?;
            }
            Step::CatchUpDone => {
                self.wait_acked().await?;
                self.conn.send_replace(ConnState::Online { detail: None });
                self.advance(cursor)?;
            }
            Step::WaitAcked => {
                self.wait_acked().await?;
                self.advance(cursor)?;
            }
            Step::Sleep(d) => {
                self.clock.sleep(*d).await;
                self.advance(cursor)?;
            }
            Step::Panic { times } => {
                let n = self.bump(&format!("panic:{cursor}"))?;
                if n <= i64::from(*times) {
                    panic!("scripted panic {n} of {times}");
                }
                self.advance(cursor)?;
            }
            Step::Fail(e) => {
                self.advance(cursor)?;
                return Ok(Some(Stop::Now(Exit::Failed(e.clone()))));
            }
            Step::Transfer(state) => {
                let next = i64::try_from(cursor + 1).unwrap_or(i64::MAX);
                self.db
                    .zone(|tx| -> kourier_db::Result<()> {
                        Self::store_transfer(tx, *state)?;
                        Self::set_in(tx, "cursor", next)
                    })
                    .map_err(internal)?;
                self.control.reached(self.uuid, cursor + 1);
                self.send_ephemeral(BackendEvent::HistoryTransfer(*state))
                    .await;
            }
            Step::Unlink { reason } => {
                self.advance(cursor)?;
                if cursor + 1 >= self.script.steps.len() {
                    // The script ends here: what it delivered is with the core.
                    self.wait_acked().await?;
                    self.control.mark_finished(self.uuid);
                }
                self.conn.send_replace(ConnState::LoggedOut {
                    reason: reason.clone(),
                    cause: Default::default(),
                    note: Default::default(),
                });
                self.control.device(self.uuid, false);
                return Ok(Some(Stop::Drain(Exit::LoggedOut {
                    reason: reason.clone(),
                })));
            }
        }
        Ok(None)
    }
}

/// `fake_state` keys of the history transfer: the kind of its state (0: none), the bytes of
/// a download, its stated size plus one (0: not stated), the stage of the walk, and whether
/// the stored state came from a walk (a state from [`Step::Transfer`] has no stage).
const TRANSFER_KIND: &str = "transfer";
const TRANSFER_DONE: &str = "transfer_done";
const TRANSFER_TOTAL: &str = "transfer_total";
const TRANSFER_STAGE: &str = "transfer_stage";
const TRANSFER_STAGE_SET: &str = "transfer_walked";

const FAILURES: [HistoryTransferFailure; 7] = [
    HistoryTransferFailure::LinkAgain,
    HistoryTransferFailure::UpdateApp,
    HistoryTransferFailure::Corrupt,
    HistoryTransferFailure::TooLarge,
    HistoryTransferFailure::Download,
    HistoryTransferFailure::Internal,
    // Appended: the stored index of the kinds before it stays what it was.
    HistoryTransferFailure::Incomplete,
];

/// A transfer state as three numbers of `fake_state`.
fn encode_transfer(state: HistoryTransfer) -> (i64, i64, i64) {
    let int = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    match state {
        HistoryTransfer::Waiting => (1, 0, 0),
        HistoryTransfer::Downloading { done, total } => {
            (2, int(done), total.map_or(0, |t| int(t).saturating_add(1)))
        }
        HistoryTransfer::Reading => (3, 0, 0),
        HistoryTransfer::Done => (4, 0, 0),
        HistoryTransfer::Skipped => (5, 0, 0),
        HistoryTransfer::Failed(kind) => {
            let at = FAILURES.iter().position(|k| *k == kind).unwrap_or(0);
            (10 + int(at as u64), 0, 0)
        }
    }
}

fn decode_transfer(kind: i64, done: i64, total: i64) -> Option<HistoryTransfer> {
    let uint = |v: i64| u64::try_from(v).unwrap_or(0);
    Some(match kind {
        1 => HistoryTransfer::Waiting,
        2 => HistoryTransfer::Downloading {
            done: uint(done),
            total: (total > 0).then(|| uint(total - 1)),
        },
        3 => HistoryTransfer::Reading,
        4 => HistoryTransfer::Done,
        5 => HistoryTransfer::Skipped,
        k if k >= 10 => HistoryTransfer::Failed(*FAILURES.get(usize::try_from(k - 10).ok()?)?),
        _ => return None,
    })
}

/// The `fake_state` keys of the pin list start with this; the value is the place in the list.
const PIN_PREFIX: &str = "pin:";

/// The `fake_state` key of a pinned conversation ([`Actor::note_pins`]).
fn pin_key(conv: &ConvKey) -> String {
    format!("{PIN_PREFIX}{}", conv.encode())
}

/// The `fake_state` key of a conversation's timer version (Signal's `expireTimerVersion`).
fn timer_version_key(conv: &ConvKey) -> String {
    format!("timer-version:{}", conv.encode())
}

/// The timer versions the patches among `events` state, by [`timer_version_key`].
fn timer_versions(events: &[BackendEvent]) -> Vec<(String, i64)> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ConversationUpsert(p) => p
                .timer_version
                .map(|v| (timer_version_key(&p.key), i64::from(v))),
            _ => None,
        })
        .collect()
}

/// Why the fake server refuses an edit, delete or reaction (a real backend's checks): edits
/// and deletes for me only of our own messages, edits only to non-empty text or to a message
/// with attachments (its caption; every attachment must come with a pointer or a file to send
/// again), reactions only with an emoji.
fn invalid_change(kind: &kourier_backend_api::OutgoingKind) -> Option<BackendError> {
    use kourier_backend_api::OutgoingKind;
    use kourier_model::{Body, TargetAuthor};
    let refuse = |detail: &str| Some(BackendError::new(ErrorKind::InvalidInput, detail));
    match kind {
        OutgoingKind::Message { .. } => None,
        OutgoingKind::Edit { target, .. } if target.author != TargetAuthor::Own => {
            refuse("only own messages can be edited")
        }
        OutgoingKind::Edit { body, .. } => match body {
            Body::Text(t) if !t.body.trim().is_empty() => None,
            Body::Media { attachments, .. } if attachments.is_empty() => {
                refuse("an edit of a media message needs its attachments")
            }
            Body::Media { attachments, .. }
                if attachments
                    .iter()
                    .any(|a| a.long_text || (a.remote.is_none() && a.local.is_none())) =>
            {
                refuse("an attachment of the edit has neither a pointer nor a file")
            }
            Body::Media { .. } => None,
            _ => refuse("an edit needs text"),
        },
        OutgoingKind::Delete { target, .. } if target.author == TargetAuthor::Unknown => {
            refuse("a delete needs its target's author")
        }
        OutgoingKind::Delete { .. } => None,
        OutgoingKind::React { emoji, .. } if emoji.trim().is_empty() => refuse("an empty reaction"),
        OutgoingKind::React { .. } => None,
    }
}

/// States the roaming flag `on` in `caps`, and the pin limit unless it states one.
fn state_roaming(caps: &mut AccountCaps, on: bool) {
    caps.flags
        .insert(AccountCaps::FLAG_ROAMING_WRITES.to_owned(), on);
    caps.max_pinned_chats.get_or_insert(PIN_LIMIT);
}

/// The confirmation the network would send for a roaming change (§4.1 principle 4).
fn patch_for(conv: ConvKey, change: &ConversationChange) -> ConversationPatch {
    let mut p = ConversationPatch::new(conv);
    match change {
        ConversationChange::SetArchived { archived } => p.archived = Some(*archived),
        // The pin list is the account's, not the chat's: `Actor::roaming_events` states it.
        ConversationChange::SetPinned { .. } => {}
        ConversationChange::SetMuted { until } => {
            p.muted_until = until.map_or(Patch::Clear, Patch::Set);
        }
        ConversationChange::SetBlocked { blocked } => p.blocked = Some(*blocked),
        ConversationChange::SetMarkedUnread { marked_unread } => {
            p.marked_unread = Some(*marked_unread);
        }
        ConversationChange::SetTimer { timer } => {
            p.timer = timer.map_or(Patch::Clear, Patch::Set);
        }
        ConversationChange::AcceptRequest => p.request_pending = Some(false),
        ConversationChange::Leave => {}
    }
    p
}
