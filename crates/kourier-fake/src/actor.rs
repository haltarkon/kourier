//! The fake account actor: its own thread, a current-thread runtime and `LocalSet`, a real
//! `backend.db` with `core_outbox`, stage 1 (inbox) and stage 2 (events) transactions, and the
//! `kourier-db` forwarder (§5.6, §6.3) — the durability shape of a real backend.

use std::cell::Cell;
use std::convert::Infallible;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendEvent, Clock, Command, DurableCommand, Envelope, Exit, OutgoingRequest,
    Relink, StartMode, TransmitOutcome, decode_outbox_row, encode_outbox_row,
};
use kourier_db::rusqlite::OptionalExtension;
use kourier_db::{
    Batch, Forwarder, ForwarderConfig, Migrations, Refusal, ZoneDb, failpoint, migration, outbox,
};
use kourier_model::{
    AccountProgress, AccountUuid, BackendError, ConnState, ConvKey, ConversationChange,
    ConversationPatch, DeviceLogout, ErrorKind, HistoryTransfer, HistoryTransferFailure,
    InputField, InputKind, LocalMsgId, LoginStep, LoginStepKind, LogoutReport, Member, OwnDevice,
    Patch, RecipientState, Redacted, UserId,
};
use tokio::sync::{Notify, mpsc, watch};

use crate::conformance;
use crate::flavor::Flavor;
use crate::network::{FakeControl, TransmitRecord};
use crate::script::{LoginScript, Script, Step, TransferScript};

/// The fake's `backend.db` schema component.
pub const MIGRATIONS: Migrations = Migrations::new(
    "fake",
    &[
        migration!(1, "0001_fake"),
        migration!(2, "0002_fake_transmit"),
        migration!(3, "0003_fake_job_payload"),
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
    let rows: Vec<(String, i64, String, bool)> = conn
        .prepare(
            "SELECT local, sent_at, recipient, recipient_update FROM fake_transmits ORDER BY id",
        )
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<kourier_db::rusqlite::Result<Vec<_>>>()
        })
        .map_err(internal)?;
    rows.into_iter()
        .map(|(local, sent_at, recipient, recipient_update)| {
            Ok(ServerDelivery {
                local,
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

enum Stop {
    Now(Exit),
    Drain(Exit),
}

enum LoginCmd {
    Start(Option<Relink>),
    Input(u64),
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
        match Actor::open(ctx, script, pushed, control, flavor) {
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
}

impl Actor {
    fn open(
        ctx: AccountContext,
        script: Arc<Script>,
        pushed: bool,
        control: Arc<FakeControl>,
        flavor: Flavor,
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
            linking: Cell::new(ctx.start == StartMode::Fresh && !linked),
            blob_dir: ctx.blob_dir,
            pushed,
            skip_pending: Cell::new(false),
            skip: Notify::new(),
            walking: Cell::new(false),
        };
        Ok((actor, ctx.commands))
    }

    async fn run(self, commands: mpsc::Receiver<Command>) -> Exit {
        tracing::debug!(uuid = %self.uuid.short(), flavor = ?self.flavor, script = %self.script.name, "fake actor running");
        if self.start == StartMode::Rebuild
            && let Err(e) = self.rebuild()
        {
            return Exit::Failed(e);
        }
        let (login_tx, login_rx) = mpsc::unbounded_channel();
        let stop = tokio::select! {
            s = self.forwarder() => s,
            s = self.runner(login_rx) => s,
            s = self.commands(commands, login_tx) => s,
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
        let payload = keep
            .then(|| serde_json::to_string(&command))
            .transpose()
            .map_err(internal)?;
        let now = self.now();
        let mut added = false;
        self.commit_events(&[BackendEvent::CommandPersisted { cmd }], |tx| {
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
    fn transmit(&self, req: &OutgoingRequest) -> Result<TransmitOutcome, BackendError> {
        // Like a real backend before its upload: every attachment's blob must read back whole.
        if let kourier_backend_api::OutgoingKind::Message {
            body: kourier_model::Body::Media { attachments, .. },
            ..
        } = &req.kind
        {
            for a in attachments {
                if let Err(e) = crate::attachments::read_outgoing(&self.blob_dir, a) {
                    return Ok(TransmitOutcome::Failed(e));
                }
            }
        }
        // Like a real backend, an edit, delete or reaction is checked before it is sent.
        if let Some(e) = invalid_change(&req.kind) {
            return Ok(TransmitOutcome::Failed(e));
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
        let outcome = self
            .db
            .zone(|tx| -> kourier_db::Result<TransmitOutcome> {
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
                        return Ok(TransmitOutcome::Failed(r.error.clone()));
                    }
                }
                let (mut delivered, mut failed) = (Vec::new(), Vec::new());
                'recipients: for u in recipients {
                    let have: i64 = tx.query_row(
                        "SELECT count(*) FROM fake_transmits WHERE local = ?1 AND recipient = ?2",
                        (req.local.as_ref(), u.as_str()),
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
                            "INSERT INTO fake_transmits(local, sent_at, recipient, recipient_update, at)
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                            (req.local.as_ref(), req.sent_at, u.as_str(), req.is_recipient_update, now),
                        )?;
                    }
                    delivered.push((u, RecipientState::Sent { sealed: true }));
                }
                Ok(if failed.is_empty() {
                    TransmitOutcome::Sent {
                        server_at: Some(now),
                        per_recipient: delivered,
                    }
                } else {
                    TransmitOutcome::Partial { delivered, failed }
                })
            })
            .map_err(internal)?;
        failpoint::hit(FAILPOINT_AFTER_SERVER_ACCEPT);
        Ok(outcome)
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
        let stop = self.serve(rx, login, &mut downloads).await;
        // A file that is being written is finished before the actor goes: its target may
        // be asked for again by the next actor, and its directory may be removed.
        while downloads.join_next().await.is_some() {}
        stop
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
                | Command::ConfirmIdentity { .. }
                | Command::RefreshContact { .. }
                | Command::FetchHistory { .. } => {}
                Command::StartLogin { relink, .. } => {
                    let _ = login.send(LoginCmd::Start(relink));
                }
                Command::SubmitLoginInput { txn, .. } => {
                    let _ = login.send(LoginCmd::Input(txn));
                }
                Command::CancelLogin => {
                    let _ = login.send(LoginCmd::Cancel);
                }
                Command::Transmit { local, req, reply } => {
                    let outcome = match self.transmit(&req) {
                        Ok(o) => o,
                        Err(e) => return Stop::Now(Exit::Failed(e)),
                    };
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
                    self.record_transmit(local, *req, &outcome);
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
                                let _ = reply.send(outcome);
                            });
                        }
                        None => {
                            let _ = reply.send(outcome);
                        }
                    }
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
                } => {
                    let command = DurableCommand::ClearConversation {
                        conv,
                        newest,
                        newest_non_expiring,
                        remove,
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
                    if self.script.reject_changes {
                        let _ = reply.send(Err(BackendError::new(
                            ErrorKind::InvalidInput,
                            "transaction rejected",
                        )));
                    } else {
                        let res = self.change_conversation(conv, &change, id);
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
                Command::Resolve { reply, .. } => {
                    let _ = reply.send(Err(BackendError::unsupported("not scripted")));
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
        let mut patch = patch_for(conv.clone(), change);
        let ConversationChange::SetTimer { timer } = change else {
            return self.commit_events(&[BackendEvent::ConversationUpsert(patch)], |_| Ok(()));
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
        let relink = loop {
            match rx.recv().await {
                Some(LoginCmd::Start(r)) => break r,
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
                            Some(LoginCmd::Input(t)) if t == txn => break,
                            Some(LoginCmd::Cancel) | None => return Ok(false),
                            Some(_) => continue,
                        }
                    }
                }
                let mut scanned = false;
                for i in 0..*codes {
                    txn += 1;
                    let data = format!("fake-link://{}/{i}", self.uuid.short());
                    self.step_login(
                        "qr",
                        txn,
                        LoginStepKind::ShowQr {
                            data: Redacted::new(data),
                            valid_for: Some(*valid_for),
                        },
                    )
                    .await;
                    if *scanned_at == Some(i) {
                        scanned = true;
                        break;
                    }
                    tokio::select! {
                        _ = self.clock.sleep(*valid_for) => {}
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
                let identity = linked_as
                    .clone()
                    .unwrap_or_else(|| self.script.own_ids.clone());
                self.link(relink.as_ref(), &identity).await
            }
        }
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
        self.commit_events(&[ready], |tx| {
            tx.execute(
                "INSERT OR REPLACE INTO fake_identity(id, own_ids, device, display_name)
                 VALUES (1, ?1, ?2, ?3)",
                (&own, device, &name),
            )?;
            if let (Some(cursor), Some(events)) = (cursor, &played) {
                Self::set_in(tx, "cursor", cursor)?;
                Self::note_timer_versions(tx, events)?;
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
                // Stage 1: the delivery is durable in the inbox together with the cursor.
                let row = encode_outbox_row(&d.events).map_err(internal)?;
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

/// The confirmation the network would send for a roaming change (§4.1 principle 4).
fn patch_for(conv: ConvKey, change: &ConversationChange) -> ConversationPatch {
    let mut p = ConversationPatch::new(conv);
    match change {
        ConversationChange::SetArchived { archived } => p.archived = Some(*archived),
        ConversationChange::SetPinned { pinned } => {
            p.pinned_order = if *pinned { Patch::Set(0) } else { Patch::Clear };
        }
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
