//! The account actor: the connection (TH3) and the device join (TH4). Signal's shape (`kourier-signal/src/actor.rs`): one
//! named thread (`spawn_account_thread`), a current-thread runtime and a `LocalSet`, the account's
//! `backend.db` as an `Rc<ZoneDb>` and `kourier_db::Forwarder` over it; WhatsApp's rules
//! (`kourier-whatsapp/src/actor.rs`).
//!
//! Start rules:
//! - `Resume` and `Rebuild` without the file are refused: opening creates a missing file, and a
//!   resumed account must never become a new device by accident.
//! - A key that does not open the file ends the actor with `Exit::Failed`, the file untouched.
//! - `Fresh` over a stored identity is refused: a fresh link never lands on a linked device.
//! - `Rebuild` states `AccountReady` again from the stored identity, then the settings, contacts and
//!   groups of a joined file (`store::ready_join`, the join's zone 2), through `core_outbox`, BEFORE
//!   the forwarder starts, so nothing of this run leads it; rows a previous run left keep their
//!   lower `seq`.
//! - A linked file (`Resume`, `Rebuild`) then maps its `deferred` rows of an older `MAP_VERSION` again
//!   (TH5 A8, `reflect::remap`: pages of 30 rows, each its own zone, their messages as HISTORY rows, round
//!   B S6), before the forwarder and before any connection, so they keep arrival order and sit below the
//!   first socket's catch-up boundary. A re-map that the STORE stops (a full disk, an I/O error) FAILS the
//!   start (TH5b S-M1, `Exit::Failed`, "the deferred rows could not be mapped again"; the rows stay, no task
//!   and no socket were made), and the core's restart with its back-off maps them before anything is
//!   applied live. The start's AGE BOUND of `deferred` runs later: once per actor, at the first `ServerInfo`
//!   whose clock check passed (round B, S9 + S11: never before the first socket, never under "Connect
//!   Anyway"), so a clock set far ahead cannot empty the table.
//!
//! THE CONNECTION (TH3, `crate::conn`): an account with a stored identity connects at `Connect`.
//! The parts' maker is called ONCE per actor, at that first `Connect` (on this thread); an account
//! without an identity (a `Fresh` start before TH4's link) stays `Offline` and makes nothing.
//! `Disconnect` stops the connection (closed with 1000) and the system's hints do nothing until the
//! next `Connect`; `NetworkChanged` and `Resumed` are handed to a running connection; `Logout` and
//! `Shutdown` stop it first. A logout verdict of the connection (4113-4115) ends the actor
//! `LoggedOut`; a PANIC of the connection task ends it `Failed` (P-L8). The slot state is the
//! actor's (`slots`), not the task's (P-M1).
//!
//! THE LOGOUT (RP-b, `conn::drop_own`; Desktop's unlink): `Logout` with a stored identity (a linked one, or zone 1's
//! of a join that did not finish: a cancel, a failure after the ClientHello; the core sends `Logout` after each of those
//! two, BEFORE it deletes the `Linking` account's directory) DROPS THE OWN DEVICE on the mediator within `DROP_WITHIN`
//! (8 s) of the actor's clock, taken FIRST in `logout()` (P-L1: before the join's stop), inside the core's 10 s; every
//! wait ends one closing second earlier: on the running connection's socket past `ServerInfo` (`DropDevice` of the own
//! id), else on a D2M-only socket of its own that expects the slot EXISTING and has no CSP leg, so a slot that is gone
//! is never made again (4115). The ack of the own id, the server's 4113 / 4114, or a 4115 is `Removed` (a 4115 while the
//! file still says `new` only after ONE more socket about 1 s later: P-M2); no answer in time, a refused connect, another
//! close, or a configuration the actor cannot honour (no socket at all) is `NotReached`; no identity is `NoDevice`. The
//! report names the stored label. NOT covered: a kill, a panic of the connection task or an actor exit between the join's
//! ClientHello and the core's commit of `AccountReady` sends no `Logout` (the core discards the `Linking` row at its next
//! start with no actor: `kourier-core/src/runtime.rs`, `supervisor.rs` `after_exit`): that orphan stays.
//!
//! THE JOIN (TH4 J3, `crate::join`): `StartLogin { "qr" }` starts ONE join task when the parts'
//! allow-list holds an ID (B1's gate: an EMPTY list fails at step `qr` with `NotTheExpectedAccount`
//! BEFORE the parts' maker is called: no transport, no socket, no QR) and the file holds no
//! identity. The transports are the actor's, made once (as `Connect` makes them). The steps are
//! EPHEMERAL envelopes with a `txn` that rises per step: `qr` (`ShowQr`, one code that does not
//! rotate), `emoji` (`ConfirmEmoji`, the three glyphs of the path hash), `linking` (`Progress`), and a
//! `Failed` step at the step the join failed at. `CancelLogin`, `Logout` and `Shutdown` stop a
//! running join first (the rendezvous closed with 1000). `Connect`, `NetworkChanged` and `Resumed`
//! during a join do nothing until the import (no identity yet).
//!
//! THE WHOLE JOIN (TH4 J5a, decisions H1, M1, C3; WhatsApp's `Done` rule, `kourier-whatsapp/src/link.rs`):
//! at `EssentialData`, in this order: (1) the data's checks (`essential::Import::check`), (2) the ID
//! against the login's rules (relink, taken, allow-list); a refusal stops the join with 4100 and fails
//! at `linking` with nothing written; (3) ZONE 1 (`store::import_join`: identity with slot `new`, the
//! data; NO `core_outbox` row), then `identity`, `linked_label` and a reset `slots`; (4) the connection
//! with the JOIN HOOK (`conn::Setup::join`); (5) the first `ServerInfo` must say `NEW` (a 41xx close,
//! no `ServerInfo` within 20 s, anything but `NEW`: `Failed` at `linking`, no zone 2, the join stopped
//! with 4100; the core deletes the `Linking` account); (6) ZONE 2 (`store::ready_join`: `AccountReady`
//! first, the marker) -> the `seq` of its first and of its last row; (7) the core must have APPLIED row 1
//! (`AccountReady`): the forwarder's `acked` watch reaches the first `seq` AND `outbox::refused` says the
//! row was not quarantined (S3, P6: nothing is told to the phone before the core owns the account); a
//! REFUSED row 1 ends the link at once (the rendezvous closed 4100, the connection stopped, NO `Registered`,
//! NO `Done`) with a `Failed { Internal }` at `linking` (TH5 L1: a core that already failed the login reads
//! no more; one that refused the row for another reason ends the login on it); (8) `Registered` to the phone
//! (`Join::register`: `Sent` or `NotSent`, P5; a phone that left first is `NotSent`: one WARN, the login
//! goes on, the device IS registered on the mediator); (9) `Done` ONLY when `acked` reaches the LAST
//! `seq` (after (6) the connection applies the reflected envelopes it held since `ServerInfo`: TH5's
//! step 0, `conn::Setup::ready`). Before the ack of row 1 a failure is a `Failed` step (and stops the join's connection, P1); a
//! phone that leaves between zone 2 and the ack of row 1 does not fail the link. After `Done` a failure is
//! a connection outcome. A join that ended without `Done` past zone 1 (failed, refused, cancelled) is over
//! for this actor: `Connect` does nothing (S11). The join publishes no `ConnState`: the connection does.
//!
//! A `Rebuild` or `Resume` of a file with an identity but WITHOUT zone 2's marker (S4) is a join that
//! stopped between its zones: the actor ends `Exit::LoggedOut` at its start ("the link was never
//! finished"), states nothing and connects nothing (the core shows the account as logged out; the
//! account is not linked).
//!
//! THE CLEAR (TH5b R1): the user's Clear Chat or Delete Chat (`Command::ClearConversation`) deletes every
//! `deferred` row of that chat and confirms the command with `CommandPersisted` in ONE zone
//! (`store::clear_deferred`, failpoints `INSIDE_CLEAR` / `AFTER_CLEAR`), so a later build's re-map never shows a
//! message the user cleared. A clear that waited pending across an upgrade is handled after the start's
//! re-map (a KNOWN LIMIT at EVERY later raise of `MAP_VERSION`, not of dev files only, TH5b S-L2: the
//! re-map runs before the actor reads a command, and the core resends the pending commands only after
//! `Connect`, `kourier-core/src/supervisor.rs`, so the re-map may already have mapped that chat's rows; a
//! deferred call row is a message, and the core's cut refuses a history row only at or below the newest
//! `sent_at` that went. A real fix needs the core: the pending clears handed to the actor before its
//! re-map, or a durable "cleared at" the backend reads; a follow-up for the main lead).
//!
//! THE SEND CORE'S DOOR (TH6a, decision C3): `Command::Transmit` stays `Failed(unsupported)` unless the parts say
//! `send_open` (`ThreemaParts::with_send_open`, doc-hidden, which only this crate's tests call: rules 13 and 15), so nothing
//! in a real build reaches the wire through it and every chat keeps `CannotSend::NotYetBuilt`. Behind the door
//! `send::attempt` carries it out. `AbortTransmit` is `TooLate` either way: nothing uploads, and PREPARE is the point of no
//! return. THE CLEAR'S share of the queue: the chat's unsent rows at or below `before_ms` go in the clear's one zone, an
//! attached row stays (decision R5).
//!
//! THE COMMAND MATCH LISTS EVERY `Command` VARIANT, with no wildcard arm, and NAMES EVERY FIELD of
//! it (`field: _`, never `..`; lane decision M3, rules 7 of `tests/builder.rs`): a new variant of the
//! contract does not compile here until it has an arm, a field a variant gains later (a `reply`) does
//! not compile until its arm says what becomes of it, and every variant that carries a reply answers
//! it (an unsupported request is an answer).
//!
//! Shutdown order: the forwarder gets [`SHUTDOWN_DRAIN`] to hand the core what it still has, then it
//! stops, then the store is closed, then the `Exit` is reported.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AbortAnswer, AccountContext, BackendConfig, BackendEnv, BackendEvent, Clock, Command, Envelope,
    Exit, Relink, StartMode, TakenIdentity, TransmitOutcome,
};
use kourier_db::{DbKey, ZoneDb};
use kourier_model::{
    AccountUuid, AttachmentId, BackendError, CmdId, ConnState, ConvKey, DeviceLogout, DownloadId,
    ErrorKind, LoginStep, LoginStepKind, LogoutReport, Redacted,
};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::conn::{self, End, JoinHook, Nudge, Panicked, SlotReport, Task};
use crate::error::StoreError;
use crate::join::essential::{Bounds, Import};
use crate::join::rules::Rules;
use crate::join::{self, Join, JoinEvent, Received, Registration};
use crate::send::attempt;
use crate::send::shared::Shared;
use crate::{ThreemaParts, Transports, db, outbox, store};

/// What the core is told when it starts (`Resume`, `Rebuild`) a file whose join never reached its zone 2
/// (S4).
const UNFINISHED_LINK: &str = "the link was never finished";
/// What a request TH1 does not carry out is told.
const NOT_DONE: &str = "Threema does not do this yet";
/// What the core is told when the start's re-map stopped on the store's failure (TH5b S-M1): the start
/// fails, the core restarts the actor with its back-off.
const REMAP_STOPPED: &str = "the deferred rows could not be mapped again";
/// The close code of a join the person cancelled or the actor stops (`TP/md-d2d-rendezvous.proto`).
const JOIN_CANCELLED: u16 = 1000;
/// The close code of a join the actor ends after the nomination (an upper-layer error).
const JOIN_REFUSED: u16 = 4100;

/// How long a stopping actor lets the forwarder hand the core what it still has (the core waits
/// 5 s for a shutdown in all, §4.7).
#[cfg(not(test))]
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
#[cfg(test)]
const SHUTDOWN_DRAIN: Duration = Duration::from_millis(300);

/// The actor thread's body: builds the runtime, opens `backend.db` and serves commands until a
/// shutdown, a logout or a failure.
pub(crate) fn run(ctx: AccountContext, parts: ThreemaParts) -> Exit {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return Exit::Failed(BackendError::internal("the actor's runtime did not start")),
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        match Actor::start(ctx, parts) {
            Ok(mut actor) => {
                let exit = actor.event_loop().await;
                actor.finish(exit).await
            }
            // S4: the file is a join that stopped between its zones; nothing was stated or connected.
            Err(NotStarted::Unfinished) => {
                tracing::warn!("the account's link was never finished; the account is not linked");
                Exit::LoggedOut {
                    reason: UNFINISHED_LINK.to_owned(),
                }
            }
            Err(NotStarted::Failed(error)) => {
                tracing::error!(kind = ?error.kind, "the threema account actor did not start");
                Exit::Failed(error)
            }
        }
    })
}

struct Actor {
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Envelope>,
    cancel: CancellationToken,
    config: BackendConfig,
    db: Rc<ZoneDb>,
    clock: Arc<dyn Clock>,
    /// Wakes the forwarder after a zone that appended an event committed.
    wake: Rc<Notify>,
    /// The parts a client is made from (at the first `Connect` of an account with an identity).
    parts: ThreemaParts,
    /// The transports, made once per actor.
    transports: Option<Transports>,
    /// The stored identity (keys in `Zeroizing`), kept in memory for the connection.
    identity: Option<Rc<store::Identity>>,
    /// The account's state channel to the core.
    conn_state: watch::Sender<ConnState>,
    /// The device slot's state, kept across the connection tasks of this actor (P-M1); made at the
    /// first `Connect` from the stored identity.
    slots: Option<conn::Slots>,
    /// The running connection.
    task: Option<Task>,
    /// The running join (TH4) and its steps.
    join: Option<Join>,
    login: Steps,
    /// Where the join stands after its zone 1 (`None`: before it, or no join).
    linking: Option<Linking>,
    /// A join that got past zone 1 ended without `Done` (it failed, the core refused its first row, or
    /// it was cancelled): the file's identity is not a linked account, `Connect` does nothing (S11).
    link_ended: bool,
    /// The label the stored device was linked with (`None`: no device in the file).
    linked_label: Option<String>,
    acked: watch::Receiver<i64>,
    forwarder: JoinHandle<()>,
    forwarder_stop: CancellationToken,
    /// The core dropped its events channel: the actor ends as at a shutdown.
    core_gone: bool,
    /// The account's uuid (its "Connect Anyway" flag of the clock rule).
    account: AccountUuid,
    /// Whether the file is join-ready (TH5 step 0, decision B1): `true` for a linked file at the
    /// start, set by a join right after its zone 2 commits; the connection holds reflected envelopes
    /// in memory until then.
    ready: watch::Sender<bool>,
    /// The start's age bound of `deferred` is still due (round B, S9 + S11): the first connection's
    /// `ServerInfo` that proves the clock runs it, once per actor (`conn::Setup::expire_due`).
    expire_due: Rc<Cell<bool>>,
    /// What the actor and its socket share of the send queue (TH6a).
    send: Rc<Shared>,
}

impl Actor {
    /// The start rules (module doc), then the forwarder.
    fn start(ctx: AccountContext, parts: ThreemaParts) -> Result<Self, NotStarted> {
        let AccountContext {
            start,
            backend_db,
            db_key,
            commands,
            events,
            cancel,
            clock,
            config,
            conn: conn_state,
            uuid: account,
            ..
        } = ctx;
        if start != StartMode::Fresh && !backend_db.exists() {
            return Err(BackendError::internal(
                "the account's database is missing; it is not made again on a resume",
            )
            .into());
        }
        let key = DbKey::from_bytes(*db_key.expose());
        drop(db_key);
        let db = Rc::new(
            db::open(&backend_db, &key).map_err(|e| BackendError::internal(e.to_string()))?,
        );
        drop(key);
        let identity = db
            .read(store::load_identity)
            .map_err(|_| BackendError::internal("the account's identity could not be read"))?;
        if start == StartMode::Fresh && identity.is_some() {
            return Err(BackendError::internal(
                "a fresh start found a linked device in the account's database",
            )
            .into());
        }
        // S4: an identity without zone 2's marker is a join that stopped between its zones (or a file
        // whose core.db was lost then): it is never stated to the core as a linked account.
        if start != StartMode::Fresh && identity.is_some() {
            let ready = db
                .read(store::is_join_ready)
                .map_err(|_| BackendError::internal("the account's link could not be read"))?;
            if !ready {
                return Err(NotStarted::Unfinished);
            }
        }
        let wake = Rc::new(Notify::new());
        let linked_label = identity.as_ref().map(|i| i.device_label.clone());
        if start == StartMode::Rebuild && identity.is_some() {
            state_ready_again(&db, &*clock, &wake)?;
        }
        // A started identity passed the marker check above (S4): its file is join-ready.
        let (ready, _) = watch::channel(identity.is_some());
        let identity = identity.map(Rc::new);
        // TH5 A8 (decision M3): the deferred rows of an older MAP_VERSION are mapped again, after a
        // Rebuild's `AccountReady` and before any connection (`reflect::remap`'s doc says why). The core's
        // cancel token is checked between its pages (round A, S5: a cancelled one is a clean stop, the event
        // loop answers it at once). TH5b S-M1: a re-map the STORE stopped FAILS the start (the rows stay, no
        // socket is made, the core starts the actor again after its back-off), so no delta is applied live
        // before the older rows are mapped. It runs BEFORE the forwarder is spawned, so a failed start leaves
        // no task on the LocalSet; for a start that goes on the order changes nothing (`start` never yields:
        // the forwarder runs only once it returned, and it reads `core_outbox` from its first row).
        if let Some(identity) = &identity {
            let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
            crate::reflect::remap::at_start(&db, &identity.threema_id, now, &wake, &cancel)
                .map_err(|_: StoreError| BackendError::internal(REMAP_STOPPED))?;
        }
        let (acked_tx, acked) = watch::channel(0);
        let forwarder_stop = CancellationToken::new();
        let forwarder = spawn_local(outbox::forward(
            db.clone(),
            events.clone(),
            wake.clone(),
            acked_tx,
            forwarder_stop.clone(),
        ));
        Ok(Self {
            commands,
            events,
            cancel,
            config,
            db,
            clock,
            wake,
            parts,
            transports: None,
            identity,
            conn_state,
            slots: None,
            task: None,
            join: None,
            login: Steps::default(),
            linking: None,
            link_ended: false,
            linked_label,
            acked,
            forwarder,
            forwarder_stop,
            core_gone: false,
            account,
            ready,
            expire_due: Rc::new(Cell::new(true)),
            send: Rc::new(Shared::default()),
        })
    }

    async fn event_loop(&mut self) -> Exit {
        loop {
            if self.core_gone {
                return Exit::Shutdown;
            }
            let command = tokio::select! {
                () = self.cancel.cancelled() => return Exit::Shutdown,
                end = Task::ended(&mut self.task) => match end {
                    Ok(End::LoggedOut { reason }) => {
                        return Exit::LoggedOut { reason: reason.to_owned() };
                    }
                    // A refused join reported on its hook first (`link_next`); a drop is asked only by a logout.
                    Ok(End::Stopped | End::JoinRefused | End::Dropped) => continue,
                    // A panicked connection is the actor's failure (P-L8): the core's restart
                    // policy hears of it, and the last state does not stand for a dead socket.
                    Err(Panicked) => return Exit::Failed(task_failed()),
                },
                next = Join::next(&mut self.join) => {
                    match next {
                        join::Next::Event(event) => self.join_event(event).await,
                        join::Next::Ended(ended) => self.join_ended(ended).await,
                    }
                    continue;
                }
                next = link_next(&mut self.linking, &mut self.acked) => {
                    match next {
                        LinkNext::Report(report) => self.server_info(report).await,
                        LinkNext::Settled => self.owned().await,
                        LinkNext::Acked => self.linked().await,
                    }
                    continue;
                }
                command = self.commands.recv() => command,
            };
            // The core dropped its end: the same as a shutdown.
            let Some(command) = command else {
                return Exit::Shutdown;
            };
            if let Some(exit) = self.command(command).await {
                return exit;
            }
        }
    }

    /// One command; `Some` ends the actor. EVERY variant has its own arm (module doc).
    async fn command(&mut self, command: Command) -> Option<Exit> {
        match command {
            Command::Shutdown => return Some(Exit::Shutdown),
            Command::Logout => return Some(self.logout().await),
            Command::Connect => self.connect(),
            // The connection stops; a hint does nothing until the next `Connect`.
            Command::Disconnect => {
                if let Some(task) = self.task.take() {
                    match task.stop().await {
                        Ok(End::LoggedOut { reason }) => {
                            return Some(Exit::LoggedOut {
                                reason: reason.to_owned(),
                            });
                        }
                        Ok(End::Stopped | End::JoinRefused | End::Dropped) => {}
                        Err(Panicked) => return Some(Exit::Failed(task_failed())),
                    }
                }
                conn::publish_offline(&self.conn_state);
            }
            // Handed to a running connection; without one a hint never connects (only `Connect`).
            Command::NetworkChanged => self.nudge(Nudge::NetworkChanged),
            Command::Resumed => self.nudge(Nudge::Resumed),
            // `relink` and `taken` become the login's `Rules`, checked at `EssentialData` (brief §2.7).
            Command::StartLogin {
                flow,
                relink,
                taken,
            } => self.start_login(&flow, relink, taken).await,
            // No step of a Threema join expects input.
            Command::SubmitLoginInput { txn: _, values: _ } => {}
            // The join stops (closed with 1000); before the import nothing was written. The
            // core's `Logout` follows.
            Command::CancelLogin => {
                self.stop_join(JOIN_CANCELLED).await;
                // After zone 1 the join's connection goes too: no zone 2 after a cancel, and no `Done`
                // after zone 2 (the wait for the core's ack is dropped).
                //
                // M3, closed by RP-b (architecture §4.3 step 2: "a link that finished just then does not
                // leave a device on the server"): a cancel after the join's ClientHello reached the
                // mediator leaves the new device's slot there UNTIL the core's `Logout` that follows:
                // the file keeps zone 1's identity row, and the logout drops the device from it (THE
                // LOGOUT, module doc) and reports `Removed`. Pinned by `tests/join/refused.rs`.
                if self.linking.take().is_some() {
                    self.link_ended = true;
                    if let Some(task) = self.task.take() {
                        let _ = task.stop().await;
                    }
                }
            }
            // TH6a: only under the parts' test door does a `Transmit` reach the send core (`send_open`, module doc).
            Command::Transmit {
                local: _,
                req,
                reply,
            } => {
                if self.parts.send_open() {
                    attempt::transmit(&self.db, &self.send, self.clock.now_ms(), *req, reply);
                } else {
                    let _ =
                        reply.send(TransmitOutcome::Failed(BackendError::unsupported(NOT_DONE)));
                }
            }
            // Nothing uploads, so nothing can be stopped.
            Command::AbortTransmit { local: _, reply } => {
                let _ = reply.send(AbortAnswer::TooLate);
            }
            Command::ChangeConversation {
                conv: _,
                change: _,
                id: _,
                reply,
            }
            | Command::ChangeContact {
                user: _,
                change: _,
                reply,
            }
            | Command::ChangeAccountSettings { patch: _, reply }
            | Command::ChangeProfile { patch: _, reply }
            // A10a: Threema states no `roaming_writes` flag, so the switch is refused.
            | Command::SetRoamingWrites { on: _, reply }
            | Command::SubmitChallenge {
                token: _,
                answer: _,
                reply,
            } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            Command::Resolve { id: _, reply } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            Command::ListOwnDevices { reply } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            // No safety numbers on Threema in Kourier yet (P5a): nothing pauses on a Threema
            // identity, so a confirmation is an approval with nothing to do, and neither a safety
            // number nor a verification is offered.
            Command::ConfirmIdentity {
                user: _,
                key: _,
                reply,
            } => {
                let _ = reply.send(Ok(()));
            }
            Command::SafetyNumber { user: _, reply } => {
                let _ = reply.send(Ok(Err(kourier_model::SafetyUnavailable::Unsupported)));
            }
            Command::SetVerified {
                user: _,
                verified: _,
                key: _,
                reply,
            } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            // A10a: the core's own durable change of a chat (the clearing of a mark when a chat is
            // read) is queued only for an account whose caps state the roaming flag, which Threema
            // does not; a row left over is confirmed here (and dropped), or the core would send it
            // again at every start. (The clear below is the other durable command confirmed here.)
            Command::ChangeConversationDurable {
                cmd,
                conv: _,
                change: _,
                asked_at: _,
            } => self.confirm_dropped(cmd),
            // TH5b R1: the user's Clear Chat / Delete Chat. Threema's D2D syncs no clear, so nothing goes
            // to the other devices; the chat's `deferred` rows go and the command is confirmed in ONE zone
            // (`store::clear_deferred`), whatever the anchors or `remove` say. TH6a R5: the same zone takes the
            // chat's unsent rows of the send queue at or below `before_ms`.
            Command::ClearConversation {
                cmd,
                conv,
                newest: _,
                newest_non_expiring: _,
                remove: _,
                before_ms,
                system_before_ms: _,
            } => self.clear_conversation(cmd, &conv, before_ms),
            // Durable commands: nothing carries them out yet, so none is confirmed and the core's
            // row stays pending (it is sent again at a later start).
            Command::SendReceipts {
                cmd: _,
                conv: _,
                targets: _,
                kind: _,
            }
            | Command::SyncReadMarker {
                cmd: _,
                conv: _,
                up_to: _,
                read: _,
            }
            | Command::DeleteAttachment {
                cmd: _,
                conv: _,
                target: _,
                index: _,
                net_ids: _,
            }
            | Command::RequestAttachments {
                cmd: _,
                conv: _,
                target: _,
            } => {}
            // The core waits for the answer to every download it asks for: it is told the request
            // cannot be carried out (the contract's failure form), durably.
            Command::DownloadAttachment {
                attachment,
                request,
                remote: _,
                dest: _,
                net_ids: _,
            } => self.answer_download(attachment, request),
            // No message of a Threema account exists before the receive path (TH5): no history to
            // fetch or transfer, no contact to act on.
            Command::SetTyping { conv: _, on: _ }
            | Command::FetchHistory {
                conv: _,
                before: _,
                limit: _,
            }
            | Command::RefreshContact { user: _ }
            | Command::SkipHistoryTransfer
            | Command::DismissHistoryTransfer => {}
        }
        None
    }

    /// `Connect`: an account with a stored identity connects (the transports made at the first);
    /// a running connection connects now (waiting) or tries once (held). Without an identity, or
    /// under a configuration it cannot honour, the account stays `Offline` and nothing is made.
    fn connect(&mut self) {
        if let Err(error) = check_config(&self.config) {
            tracing::warn!(kind = ?error.kind, "the configuration cannot be honoured");
            return;
        }
        let Some(identity) = self.identity.clone() else {
            return;
        };
        // S11: a join that ended without `Done` past zone 1 leaves an identity the core is about to
        // discard; connecting would register the device on the mediator (the very thing the failure
        // or the cancel was to avoid).
        if self.link_ended {
            tracing::debug!("a Connect after a link that did not finish was ignored");
            return;
        }
        if let Some(task) = &self.task {
            task.nudge(Nudge::Connect);
            return;
        }
        self.start_task(identity, None);
    }

    /// Starts the connection of `identity` (the transports and the slots made at the first);
    /// `join`: the join's hook (TH4).
    fn start_task(&mut self, identity: Rc<store::Identity>, join: Option<JoinHook>) {
        let transports = self
            .transports
            .get_or_insert_with(|| self.parts.make())
            .clone();
        let slots = self
            .slots
            .get_or_insert_with(|| conn::Slots::new(&identity))
            .clone();
        self.task = Some(Task::start(conn::Setup {
            transports,
            server: self.parts.server().clone(),
            identity,
            slots,
            clock: self.clock.clone(),
            state: self.conn_state.clone(),
            db: self.db.clone(),
            account: self.account,
            join,
            ready: self.ready.subscribe(),
            wake: self.wake.clone(),
            acked: self.acked.clone(),
            expire_due: self.expire_due.clone(),
            events: self.events.clone(),
            send: self.send.clone(),
        }));
    }

    fn nudge(&self, nudge: Nudge) {
        if let Some(task) = &self.task {
            task.nudge(nudge);
        }
    }

    /// `DownloadAttachment`: the core waits for an `AttachmentDone` for every request, and nothing
    /// downloads before the receive path (TH5), so the answer is the contract's failure form, an
    /// unsupported error, written to `core_outbox` in one zone (the core sends an unanswered request
    /// again at its next start, so a kill before the commit loses nothing).
    fn answer_download(&mut self, attachment: AttachmentId, request: DownloadId) {
        let answer = BackendEvent::AttachmentDone {
            attachment,
            request: Some(request),
            result: Err(BackendError::unsupported(NOT_DONE)),
        };
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let appended = self
            .db
            .zone(|tx| outbox::append_events(tx, std::slice::from_ref(&answer), now, &self.wake));
        if appended.is_err() {
            tracing::warn!("a download request could not be answered");
        }
    }

    /// `ChangeConversationDurable`: confirmed with `CommandPersisted` in one zone and dropped (no
    /// content in the log line). A zone that fails leaves the core's row pending, and it is sent
    /// again at the next start.
    fn confirm_dropped(&mut self, cmd: CmdId) {
        tracing::debug!(cmd = cmd.0, "a durable conversation change is dropped");
        let confirmed = BackendEvent::CommandPersisted { cmd };
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let appended = self.db.zone(|tx| {
            outbox::append_events(tx, std::slice::from_ref(&confirmed), now, &self.wake)
        });
        if appended.is_err() {
            tracing::warn!("a durable conversation change could not be confirmed");
        }
    }

    /// `ClearConversation` (TH5b R1): the chat's `deferred` rows deleted and `CommandPersisted` written in ONE
    /// zone (`store::clear_deferred`), with the chat's unsent send rows at or below `before_ms` that no `Transmit` is
    /// attached to (TH6a R5). A zone that fails leaves the core's command pending (it is sent again at the next start);
    /// the log lines carry the count only, never the chat.
    fn clear_conversation(&mut self, cmd: CmdId, conv: &ConvKey, before_ms: Option<i64>) {
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let attached = self.send.attached();
        match store::clear_deferred(&self.db, conv, before_ms, &attached, cmd, now, &self.wake) {
            Ok(deleted) => tracing::debug!(
                cmd = cmd.0,
                deleted,
                "a cleared chat's deferred rows are deleted: {deleted}"
            ),
            Err(_) => {
                tracing::warn!("a cleared chat could not be confirmed; the core sends it again")
            }
        }
    }

    /// `StartLogin`: the refusals at step `qr` (module doc; every one BEFORE the parts' maker is
    /// called), else the login's rules (the relink, the profile's other accounts, the allow-list: J5a
    /// checks the joined ID with them), the transports (once per actor) and the join task.
    async fn start_login(&mut self, flow: &str, relink: Option<Relink>, taken: Vec<TakenIdentity>) {
        // P9: also while a join is past zone 1 (`linking`): the login's counter and rules are the
        // running join's, and its `Done` is still to come.
        if self.join.is_some() || self.linking.is_some() {
            tracing::warn!("a login started while a join runs: ignored");
            return;
        }
        self.login = Steps::default();
        let refusal = match check_config(&self.config) {
            Err(error) => Some(error),
            Ok(()) if flow != "qr" => {
                Some(BackendError::unsupported("Threema has no such login flow"))
            }
            // B1's gate: only a prepared run profile names IDs; nothing is made without one.
            Ok(()) if self.parts.allowed().is_empty() => Some(BackendError::new(
                ErrorKind::NotTheExpectedAccount,
                "no Threema ID is on the list of this profile",
            )),
            // A link never lands on a linked device (a relink starts a fresh actor on an empty
            // file, `context.rs`).
            Ok(()) if self.identity.is_some() => Some(BackendError::internal(
                "the account's database holds a linked device",
            )),
            Ok(()) => None,
        };
        if let Some(error) = refusal {
            self.step("qr", LoginStepKind::Failed { error }).await;
            return;
        }
        self.login.rules = Some(Rules::new(taken, relink, self.parts.allowed().clone()));
        let transports = self
            .transports
            .get_or_insert_with(|| self.parts.make())
            .clone();
        self.join = Some(Join::start(join::Setup {
            ws: transports.ws,
            rendezvous_url: self.parts.server().rendezvous_url.clone(),
            clock: self.clock.clone(),
        }));
    }

    /// One event of the running join: its step, or the end of the login.
    async fn join_event(&mut self, event: JoinEvent) {
        match event {
            JoinEvent::Qr(text) => {
                let data = Redacted::new(text.as_str().to_owned());
                let kind = LoginStepKind::ShowQr {
                    data,
                    valid_for: None,
                };
                self.step("qr", kind).await;
            }
            JoinEvent::Emoji(glyphs) => {
                let emoji = glyphs.iter().map(|g| (*g).to_owned()).collect();
                self.step("emoji", LoginStepKind::ConfirmEmoji { emoji })
                    .await;
            }
            JoinEvent::Begun => {
                let kind = LoginStepKind::Progress {
                    label: "Linking".to_owned(),
                    fraction: None,
                };
                self.step("linking", kind).await;
            }
            JoinEvent::Essential(received) => self.essential(received).await,
            // Zone 2 is written and the core's apply of row 1 is awaited: the phone left, but the device
            // IS registered on the mediator and the core is about to own the account, so the link goes
            // on (`owned`: `Registered` is `NotSent`, `Done` follows the ack).
            JoinEvent::Failed(_) if matches!(self.linking, Some(Linking::Owned(_))) => {
                tracing::info!("the phone ended the join after zone 2");
                if let Some(join) = self.join.take() {
                    let _ = join.stop(JOIN_CANCELLED).await;
                }
            }
            // Before zone 2 the link fails as a whole (its connection goes too).
            JoinEvent::Failed(error) if self.linking.is_some() => self.link_failed(error).await,
            JoinEvent::Failed(error) => {
                if let Some(join) = self.join.take() {
                    let _ = join.stop(JOIN_CANCELLED).await;
                }
                let at = self.login.at;
                self.step(at, LoginStepKind::Failed { error }).await;
            }
        }
    }

    /// `EssentialData` came: steps (1)-(4) of the module doc's whole join.
    async fn essential(&mut self, received: Box<Received>) {
        // (1) The data's own checks; the device ids drawn.
        let import = match Import::check(received, Bounds::REAL, &mut rand::rng()) {
            Ok(import) => import,
            Err(refused) => return self.link_failed(refused.error()).await,
        };
        // (2) The ID against the login's rules (relink, taken, allow-list).
        let checked = match &self.login.rules {
            Some(rules) => rules.check(&import.threema_id).map_err(|r| r.error()),
            None => Err(BackendError::internal("the login has no rules")),
        };
        if let Err(error) = checked {
            return self.link_failed(error).await;
        }
        // (3) ZONE 1: the import, no core_outbox row (H1).
        let label = crate::network::device_label_of(self.parts.host_name());
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        if store::import_join(&self.db, &import, &label, now).is_err() {
            tracing::error!("the join could not be stored");
            let error = BackendError::internal("the join could not be stored");
            return self.link_failed(error).await;
        }
        let identity = Rc::new(import.identity(label.clone(), now));
        drop(import);
        self.identity = Some(identity.clone());
        self.linked_label = Some(label);
        // A new identity resets the slots (th3-build "What TH4 must know").
        self.slots = None;
        // (4) The connection with the join hook.
        let (hook, report) = oneshot::channel();
        self.start_task(identity, Some(hook));
        self.linking = Some(Linking::Server(report));
    }

    /// (5)-(6): the join's first `ServerInfo` (or the first socket's end before one): zone 2, then the
    /// wait for the core to apply row 1 (`owned`).
    async fn server_info(&mut self, report: SlotReport) {
        self.linking = None;
        match report {
            SlotReport::New => {}
            SlotReport::NotNew => {
                let error = BackendError::new(
                    ErrorKind::Unauthorized,
                    "the mediator did not register the device as new",
                );
                return self.link_failed(error).await;
            }
            SlotReport::Failed(error) => return self.link_failed(error).await,
        }
        // (6) ZONE 2: only now the core hears of the account (H1). A failure stops the join's
        // connection too (P1: `link_failed`).
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let ready = match store::ready_join(&self.db, now, &self.wake) {
            Ok(ready) => ready,
            Err(_) => {
                tracing::error!("the joined account could not be stated");
                let error = BackendError::internal("the joined account could not be recorded");
                return self.link_failed(error).await;
            }
        };
        // TH5 step 0 (B1): the reflected envelopes the connection held since `ServerInfo` are applied
        // now, so their rows sort after `AccountReady`.
        self.ready.send_replace(true);
        // (7) NOTHING goes to the phone before the core applied row 1 (S3). [U: what the phone does]
        // after a kill from here to `Registered` (H1, RT1): the device is on the mediator, the core
        // shreds the `Linking` account at its next start.
        self.linking = Some(Linking::Owned(ready));
    }

    /// (7)-(8): the forwarder settled row 1 (`AccountReady`): applied by the core, or refused for good
    /// (P6). Applied: `Registered` to the phone, then `Done` waits for the last row.
    async fn owned(&mut self) {
        let Some(Linking::Owned(ready)) = self.linking.take() else {
            return;
        };
        match outbox::refused(&self.db, ready.first) {
            Ok(false) => {}
            // The core refused the row: the phone is told nothing, no `Registered`, no `Done`. TH5 L1
            // (decision C8): the login gets its END here whatever the core's reason. A core that already
            // failed the login (a login refusal: AlreadyAdded, a relink mismatch) reads no more events; one
            // that refused for another reason (a contract error) ends the `Linking` account on this
            // `Failed` (a failed step while not linked is `LoginFailed`).
            Ok(true) => {
                tracing::warn!("the core refused the joined account's first row; the link ends");
                let error = BackendError::internal("the core refused the account");
                return self.link_failed(error).await;
            }
            Err(_) => {
                let error = BackendError::internal("the core's answer could not be read");
                return self.link_failed(error).await;
            }
        }
        // (8) `Registered`, then the rendezvous closed with 1000. A phone that left first leaves the
        // login as it is: the device IS registered on the mediator and the core owns the account (P5).
        let sent = match self.join.take() {
            Some(join) => matches!(join.register().await, Ok(Registration::Sent)),
            None => false,
        };
        if !sent {
            tracing::warn!("the phone was gone before Registered could be sent; the link goes on");
        }
        // (9) `Done` waits for the core's ack of the last row (C3).
        self.linking = Some(Linking::Ack(ready.last));
    }

    /// (9): the core settled zone 2's last row: the login is done.
    async fn linked(&mut self) {
        self.linking = None;
        self.step("done", LoginStepKind::Done).await;
    }

    /// A failure of the join before the core applied row 1: the join stopped with 4100, the join's
    /// connection stopped (P1), a `Failed` step at `linking` (the core deletes the `Linking` account).
    async fn link_failed(&mut self, error: BackendError) {
        self.end_link().await;
        self.step("linking", LoginStepKind::Failed { error }).await;
    }

    /// The end of a link that is not `Done`, no step sent: the join stopped with 4100, the connection
    /// of the join stopped (whatever the failure was, an actor with a join has no other connection
    /// before `Done`), and a join past zone 1 is over for this actor (S11).
    async fn end_link(&mut self) {
        self.linking = None;
        self.stop_join(JOIN_REFUSED).await;
        if let Some(task) = self.task.take() {
            let _ = task.stop().await;
        }
        if self.identity.is_some() {
            self.link_ended = true;
        }
    }

    /// The join task ended without a `Failed` (a panic, or a bug): the login fails where it is.
    async fn join_ended(&mut self, ended: Result<(), join::Panicked>) {
        let error = match ended {
            Ok(()) => BackendError::internal("the join ended without a result"),
            Err(join::Panicked) => BackendError::internal("the join task failed"),
        };
        match self.linking {
            // Zone 2 is written: the same as a phone that left (`join_event`).
            Some(Linking::Owned(_)) => {
                tracing::info!("the join ended after zone 2");
            }
            Some(_) => self.link_failed(error).await,
            None => {
                let at = self.login.at;
                self.step(at, LoginStepKind::Failed { error }).await;
            }
        }
    }

    /// Stops a running join (its socket closed with `code`); nothing without one.
    async fn stop_join(&mut self, code: u16) {
        if let Some(join) = self.join.take() {
            let _ = join.stop(code).await;
        }
    }

    /// One login step, ephemeral, with the next `txn`; `at` is where a failure is reported.
    async fn step(&mut self, step_id: &'static str, kind: LoginStepKind) {
        self.login.txn += 1;
        self.login.at = step_id;
        let step = LoginStep {
            step_id: step_id.to_owned(),
            txn: self.login.txn,
            kind,
        };
        self.emit(Envelope::ephemeral(BackendEvent::Login(step)))
            .await;
    }

    /// `Command::Logout`: the own device is dropped (THE LOGOUT, module doc), the core is told once what became of it,
    /// then the actor ends logged out.
    async fn logout(&mut self) -> Exit {
        // P-L1: the drop's deadline counts from the logout, BEFORE the join's stop (up to two seconds on a stalled rendezvous).
        let by_ms = conn::drop_deadline(self.clock.now_ms());
        self.stop_join(JOIN_CANCELLED).await;
        let outcome = match self.identity.clone() {
            Some(identity) => self.drop_own(&identity, by_ms).await,
            None => {
                if let Some(task) = self.task.take() {
                    let _ = task.stop().await;
                }
                DeviceLogout::NoDevice
            }
        };
        let report = LogoutReport {
            outcome,
            device_name: self.linked_label.clone(),
        };
        self.emit(Envelope::ephemeral(BackendEvent::Logout(report)))
            .await;
        Exit::LoggedOut {
            reason: "logged out on request".to_owned(),
        }
    }

    /// RP-b: the own device's drop (THE LOGOUT, module doc): on the running connection's socket when it is past
    /// `ServerInfo`, else on a D2M-only socket of its own; both within `by_ms` of the actor's clock. A configuration the
    /// actor cannot honour opens no socket (`connect`'s rule).
    async fn drop_own(&mut self, identity: &store::Identity, by_ms: u64) -> DeviceLogout {
        if let Err(error) = check_config(&self.config) {
            tracing::warn!(kind = ?error.kind, "the configuration cannot be honoured; the device was not dropped");
            if let Some(task) = self.task.take() {
                let _ = task.stop().await;
            }
            return DeviceLogout::NotReached;
        }
        let live = match self.task.take() {
            Some(task) => task.drop_own(&*self.clock, by_ms, &self.cancel).await,
            None => None,
        };
        let dropped = match live {
            Some(dropped) => dropped,
            None => {
                // P-M2: whether the file still says `new`, read after the live socket's end (it may have seen a
                // `ServerInfo`): the actor's slots, else the stored identity's own state.
                let file_new = match &self.slots {
                    Some(slots) => slots.file_says_new(),
                    None => identity.slot_state == store::SlotState::New,
                };
                let transports = self
                    .transports
                    .get_or_insert_with(|| self.parts.make())
                    .clone();
                conn::drop_alone(conn::Alone {
                    transports: &transports,
                    server: self.parts.server(),
                    identity,
                    clock: &*self.clock,
                    by_ms,
                    file_new,
                    stop: &self.cancel,
                })
                .await
            }
        };
        match dropped {
            conn::Dropped::Removed => DeviceLogout::Removed,
            conn::Dropped::NotReached => DeviceLogout::NotReached,
        }
    }

    async fn emit(&mut self, envelope: Envelope) {
        let sent = tokio::select! {
            () = self.cancel.cancelled() => return,
            sent = self.events.send(envelope) => sent,
        };
        if sent.is_err() {
            self.core_gone = true;
        }
    }

    /// The shutdown order (module doc), then the exit.
    async fn finish(mut self, exit: Exit) -> Exit {
        // A running join first, then the connection: each socket is closed with 1000.
        self.stop_join(JOIN_CANCELLED).await;
        if let Some(task) = self.task.take() {
            let _ = task.stop().await;
        }
        if let Ok(target) = outbox::max_seq(&self.db)
            && target > 0
        {
            let mut acked = self.acked.clone();
            let _ =
                tokio::time::timeout(SHUTDOWN_DRAIN, acked.wait_for(|seq| *seq >= target)).await;
        }
        self.forwarder_stop.cancel();
        let _ = self.forwarder.await;
        // The forwarder held the other handle of the store: it is closed here, before the exit.
        drop(self.db);
        exit
    }
}

/// Where a join stands after its zone 1 (TH4 J5a).
enum Linking {
    /// The join's connection runs: the first `ServerInfo`'s report is awaited.
    Server(oneshot::Receiver<SlotReport>),
    /// Zone 2 is written: the forwarder's settling of its FIRST row is awaited (S3); the phone has
    /// heard nothing yet.
    Owned(store::Ready),
    /// The core applied row 1 and `Registered` is handled: the settling of the LAST row (this `seq`)
    /// is awaited.
    Ack(i64),
}

/// What [`link_next`] gives the actor.
enum LinkNext {
    Report(SlotReport),
    /// Row 1 of zone 2 is settled (applied or refused: `outbox::refused` tells).
    Settled,
    /// The last row of zone 2 is settled.
    Acked,
}

/// The next step of a join after its zone 1; pends without one (and once the forwarder is gone).
async fn link_next(linking: &mut Option<Linking>, acked: &mut watch::Receiver<i64>) -> LinkNext {
    match linking {
        Some(Linking::Server(report)) => LinkNext::Report(report.await.unwrap_or_else(|_| {
            SlotReport::Failed(BackendError::transient(
                "the connection ended before the mediator answered",
            ))
        })),
        Some(Linking::Owned(ready)) => {
            let seq = ready.first;
            if acked.wait_for(|settled| *settled >= seq).await.is_err() {
                return std::future::pending().await;
            }
            LinkNext::Settled
        }
        Some(Linking::Ack(seq)) => {
            let seq = *seq;
            if acked.wait_for(|settled| *settled >= seq).await.is_err() {
                return std::future::pending().await;
            }
            LinkNext::Acked
        }
        None => std::future::pending().await,
    }
}

/// Why the actor did not start.
enum NotStarted {
    /// The start failed (the core's restart policy hears of it).
    Failed(BackendError),
    /// The file holds an identity but no zone-2 marker (S4): the account is not linked.
    Unfinished,
}

impl From<BackendError> for NotStarted {
    fn from(error: BackendError) -> Self {
        Self::Failed(error)
    }
}

/// The current login: the last `txn`, the step a failure is reported at, and the rules its joined ID
/// is checked by (set when the join starts; `None` before and after a refused `StartLogin`).
struct Steps {
    txn: u64,
    at: &'static str,
    /// The joined ID is checked with it at `EssentialData` (`Rules::check`).
    rules: Option<Rules>,
}

impl Default for Steps {
    fn default() -> Self {
        Self {
            txn: 0,
            at: "qr",
            rules: None,
        }
    }
}

/// The error of a connection task that panicked (log-safe: no content).
fn task_failed() -> BackendError {
    BackendError::internal("the connection task failed")
}

/// `Rebuild`: the core forgot the account, so what the file mirrors is stated again through
/// `core_outbox` BEFORE anything else: `AccountReady` (L10 option B: no device id is reported to the
/// contract), the settings, the contacts and the groups of a joined file (architecture: Rebuild
/// re-states mirrored contacts, groups, settings). The same rows through the same zone as the join's
/// zone 2 (`store::ready_join` = `join::events` over `store::load_snapshot`).
fn state_ready_again(
    db: &ZoneDb,
    clock: &dyn Clock,
    wake: &Rc<Notify>,
) -> Result<(), BackendError> {
    let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
    store::ready_join(db, now, wake)
        .map(drop)
        .map_err(|_: StoreError| BackendError::internal("the linked account could not be recorded"))
}

/// The environment and the proxy of the configuration: Threema's own service only, without a proxy,
/// until TH3 knows how to honour them; nothing is connected around them.
fn check_config(config: &BackendConfig) -> Result<(), BackendError> {
    if config.env != BackendEnv::Production {
        return Err(BackendError::unsupported(
            "Threema runs only against its real service",
        ));
    }
    if config.proxy.is_some() {
        return Err(BackendError::unsupported(
            "Threema cannot use the proxy yet",
        ));
    }
    Ok(())
}
