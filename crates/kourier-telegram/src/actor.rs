//! The account actor: it opens the account's store by the start rules, serves the core's commands
//! and ends cleanly. A login or a `Connect` starts the account's gate and helper through the
//! supervisor and runs a live [`Session`] over it (TG-2 C8b): the session's handlers decide, this
//! loop carries their [`Act`]s out. The application's dialer still refuses every connection, so no
//! build of TG-2 reaches Telegram.
//!
//! Start rules (WhatsApp's, `kourier-whatsapp/src/actor.rs`; TG-1 plan §2.8):
//! - `Resume` and `Rebuild` without the file are refused: `kourier_db::open` creates a missing
//!   file, and a resumed account must never become a new one by accident.
//! - A key that does not open the file, or a file of another network, ends the actor with
//!   `Exit::Failed`, the file untouched (`db::open` refuses both before any migration).
//! - `Fresh` with an account row in the file is refused: a fresh login never lands on an
//!   existing account.
//! - A stored environment that differs from the parts' one ends the actor with `Exit::Failed`
//!   before any gate or helper, both ways (critic H2: a test-server account is never opened by a
//!   normal profile, and a production account never by the run tool's).
//! - Rows left in `core_outbox` (an `AccountReady` whose ack a crash lost) ship first.
//!
//! The loop (critic H1): `wake` is the one `select!` (cancel, the helper's inbound frames, the
//! answer of the ONE request in flight, the watchdog's probe, the commands, the restart's due
//! time, the QR bound, the watchdog's silence); it RETURNS a [`Wake`], and the handlers run after
//! it with `&mut self`. A request in flight is STATE (`Live::pending`), a future built from a
//! CLONE of the helper's link, so a command that wins the `select!` never drops an answer.
//!
//! Every `Command` variant has its OWN explicit arm (no wildcard): a new variant fails to compile
//! here, and is answered on purpose. A command with a reply is always answered. Nothing of a
//! command's content (a login input, a conversation, an identifier) is logged: only fixed texts.

use kourier_backend_api::{
    AbortAnswer, AccountContext, BackendEvent, BackendNotice, Clock, Command, Envelope, Exit,
    Relink, Sleep, StartMode, TakenIdentity, TransmitOutcome, UiPresence, clock::duration_ms,
};
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_db::DbKey;
use kourier_db::rusqlite::Connection;
use kourier_model::{
    AttachmentId, BackendError, CmdId, ConnState, ConvKey, DeviceLogout, DownloadId, LoginStep,
    LoginStepKind, LogoutReport, MsgTarget, ReceiptKind, Redacted,
};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::db;
use crate::error::LinkError;
use crate::keys::tdlib_key_base64;
use crate::link::Inbound;
use crate::login::{Flow, Login, Rules, step, wipe};
use crate::outbox::{self, Sent, Settled, ShipEnd, Shipped, Shipper};
use crate::parts::{TgEnv, TgParts};
use crate::pin::PIN;
use crate::presence::{self, Hint, Hints, Presence};
use crate::receive::chats::chat_id_of;
use crate::session::{Act, Asked, LaneAsk, Session, persisted};
use crate::store;
use crate::supervisor::{self, AccountStart, Backoff, Helper, Retry, Timings};
use crate::tdapi::{self, AuthorizationState, Update};

/// What a request this build does not carry out yet is told.
const NOT_DONE: &str = "Telegram does not do this yet";
/// What a login is told by parts without API credentials.
pub const NO_API_KEY: &str = "this build has no Telegram API key";
/// What a login is told when the helper cannot open the pinned TDLib.
pub(crate) const NO_LIBRARY: &str = "the Telegram library is not available in this build";
/// The notice of five helper aborts within ten minutes (once per burst); also the end of a login
/// whose restart budget is used up (critic B1).
pub(crate) const KEEPS_STOPPING: &str = "Telegram's library keeps stopping";
/// What a login is told by a helper that is not this build's (another TDLib, no lock-down): a
/// property of the installation, so unsupported, never retried (TG-1b fix G15).
pub(crate) const WRONG_HELPER: &str = "the Telegram library does not match this build";
/// The notice while TDLib's database of the account is still in use by another helper (the
/// second try in a row; the start goes on after its back-off, TG-1b fix G12). A NOTICE since TG-2
/// (critic L2): a `Failed` step would end the login (B1).
pub(crate) const DATABASE_IN_USE: &str =
    "Telegram's data is still in use by another process; trying again";
/// What a login of a flow the descriptor does not offer is told.
pub(crate) const NO_SUCH_FLOW: &str = "this way of logging in to Telegram is not offered";
/// The end of a resume whose stored environment is Telegram's test servers while the parts'
/// one is production (critic H2).
pub(crate) const TEST_ACCOUNT: &str = "this account belongs to Telegram's test servers";
/// The end of a resume whose stored environment is production while the parts' one is the test
/// servers (critic H2).
pub(crate) const PRODUCTION_ACCOUNT: &str = "this account belongs to Telegram's production servers";
/// The error of a resume whose stored environment `stored` differs from the parts' one (critic
/// H2). UNSUPPORTED with a user message (fix J3, Signal's rule for a wrong environment, §4.3
/// step 4): the core halts the account with these words at once; a restart would only say the
/// same again.
/// The group commit's frame bound (TG-3 C7b, brief §4.3 [I: TP measures]).
pub(crate) const COMMIT_FRAMES: usize = 64;
/// The group commit's time bound since the first staged item, in ms ([I: TP measures]).
pub(crate) const COMMIT_AFTER_MS: u64 = 50;

/// The answer of every download until T6 (L-9): a fixed phrase.
pub(crate) const DOWNLOAD_NOT_DONE: &str = "downloads from Telegram are not done yet";

/// The durable, retryable `AttachmentDone` of one download request (L-9; WhatsApp's
/// `answer_download`).
pub(crate) fn download_answer(attachment: AttachmentId, request: DownloadId) -> Act {
    Act::Rows(Box::new(store::Staged {
        rows: vec![store::Row {
            events: vec![store::Event {
                event: BackendEvent::AttachmentDone {
                    attachment,
                    request: Some(request),
                    result: Err(BackendError::transient(DOWNLOAD_NOT_DONE)),
                },
                gate: None,
            }],
            delivered: Vec::new(),
        }],
        ..store::Staged::default()
    }))
}

pub(crate) fn wrong_environment(stored: TgEnv) -> BackendError {
    match stored {
        TgEnv::Test => BackendError::unsupported(TEST_ACCOUNT).with_user_message(
            "This Telegram account was linked on Telegram's test servers, and Kourier was \
             started for Telegram's production servers. Nothing was changed. Start Kourier \
             for the test servers again.",
        ),
        TgEnv::Production => BackendError::unsupported(PRODUCTION_ACCOUNT).with_user_message(
            "This Telegram account was linked on Telegram's production servers, and Kourier \
             was started for Telegram's test servers. Nothing was changed. Start Kourier for \
             the production servers again.",
        ),
    }
}

/// How long a logout waits for TDLib to close after `logOut` (critic H4; the core's whole logout
/// window is 10 s).
pub(crate) const LOGOUT_WAIT: Duration = Duration::from_secs(4);
/// How long a logout then waits for TDLib to close after `destroy` (critic H4).
pub(crate) const DESTROY_WAIT: Duration = Duration::from_secs(2);
/// How long a stop waits for TDLib's answer to `online false` (critic H6; best effort).
pub(crate) const OFFLINE_WAIT: Duration = Duration::from_secs(1);
/// How long a clean stop drains TDLib after its own `close` for `authorizationStateClosed`
/// (decision Q-D3; inside the core's shutdown window). On the bound the run stays unclean.
pub(crate) const CLOSE_DRAIN: Duration = Duration::from_secs(3);

/// How many read markers wait for a helper (TF6); the rest stay pending in the core.
const HELD_MARKERS: usize = 256;

/// The actor of one account. `parts` is what a helper is made from.
pub(crate) fn run(ctx: AccountContext, parts: TgParts) -> Exit {
    run_with(ctx, parts, Timings::default())
}

/// [`run`] with the supervisor's times (tests scale them down).
pub(crate) fn run_with(ctx: AccountContext, parts: TgParts, timings: Timings) -> Exit {
    let AccountContext {
        uuid,
        start,
        backend_db,
        db_key,
        net_rt,
        commands,
        events,
        conn: conn_state,
        cancel,
        clock,
        config,
        mut ui,
        ..
    } = ctx;
    let key = DbKey::from_bytes(*db_key.expose());
    drop(db_key);
    let tdlib_key = tdlib_key_base64(&key, uuid);
    if start != StartMode::Fresh && !backend_db.exists() {
        return Exit::Failed(BackendError::internal(
            "the account's database is missing; it is not made again on a resume",
        ));
    }
    let conn = match db::open(&backend_db, &key) {
        Ok(conn) => Rc::new(conn),
        Err(e) => return Exit::Failed(BackendError::internal(e.to_string())),
    };
    drop(key);
    let stored = match db::account(&conn) {
        Ok(row) => row,
        Err(_) => return Exit::Failed(BackendError::internal("the account's database failed")),
    };
    if start == StartMode::Fresh {
        match &stored {
            None => {}
            Some(_) => {
                return Exit::Failed(BackendError::internal(
                    "a fresh start found an account in the account's database",
                ));
            }
        }
    }
    // A stored environment binds (critic H2): a mismatch refuses the account before any gate or
    // helper exists, both ways. A new account takes the parts' one (written at its first login).
    if let Some(row) = &stored
        && row.env != parts.env()
    {
        return Exit::Failed(wrong_environment(row.env));
    }
    let env = stored.as_ref().map_or_else(|| parts.env(), |row| row.env);
    let has_row = stored.is_some();
    let user_id = stored.and_then(|row| row.user_id);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return Exit::Failed(BackendError::internal("the actor's runtime did not start")),
    };
    // What the UI shows at the start (H6, T-3): read now, every change wakes the loop.
    let mut presence = Presence::default();
    presence.on_ui(presence::online_of(&ui.borrow_and_update()), clock.now_ms());
    let shipper = Shipper::new(conn.clone());
    let mut actor = Actor {
        commands,
        events,
        cancel,
        conn,
        shipper,
        group: Group::default(),
        synced_through: None,
        parts,
        uuid,
        backend_db,
        net_rt,
        tdlib_key,
        env,
        timings,
        told_lockdown: false,
        conn_state,
        clock,
        backoff: Backoff::default(),
        retry: None,
        suffix: config.expect_number_suffix,
        has_row,
        user_id,
        session: None,
        live: None,
        was_authorized: false,
        later: VecDeque::new(),
        held: VecDeque::new(),
        ui,
        ui_open: true,
        presence,
        hints: Hints::default(),
    };
    let exit = runtime.block_on(actor.event_loop());
    // The store closes here, before the exit is reported.
    drop(actor);
    exit
}

/// A request's answer in flight: a future that owns a clone of the helper's link (H1), polled by
/// the actor's `select!`. Never `Send`: the actor runs on its own current-thread runtime.
type Answer = Pin<Box<dyn Future<Output = Result<Value, LinkError>>>>;

/// The group commit (TG-3 C7b, brief §4.3): the receive's staged items, written in ONE
/// transaction when [`COMMIT_FRAMES`] frames were taken or [`COMMIT_AFTER_MS`] passed since the
/// first staged item ([`Group::full`], before the next frame), or when the inbound queue is empty
/// (the `select!`'s order: no frame and no answer is ready).
#[derive(Debug, Default)]
struct Group {
    staged: store::Staged,
    /// Inbound frames taken since the first staged item.
    frames: usize,
    /// When the first staged item came (the clock's ms).
    since_ms: Option<u64>,
}

impl Group {
    fn is_empty(&self) -> bool {
        self.staged.is_empty()
    }

    /// One inbound frame was taken (it counts only while something is staged).
    fn on_frame(&mut self) {
        if !self.is_empty() {
            self.frames += 1;
        }
    }

    /// Adds `staged` (rows in order; chat facts, gaps and parked facts after the ones before).
    fn stage(&mut self, staged: store::Staged, now_ms: u64) {
        if staged.is_empty() {
            return;
        }
        if self.is_empty() {
            self.frames = 0;
            self.since_ms = Some(now_ms);
        }
        let store::Staged {
            chats,
            rows,
            gaps,
            parked,
            clean_end,
        } = staged;
        self.staged.chats.extend(chats);
        self.staged.rows.extend(rows);
        self.staged.gaps.extend(gaps);
        self.staged.parked.extend(parked);
        self.staged.clean_end |= clean_end;
    }

    /// The commit may not wait for the queue to empty: 64 frames, or 50 ms since the first item.
    fn full(&self, now_ms: u64) -> bool {
        !self.is_empty()
            && (self.frames >= COMMIT_FRAMES
                || self
                    .since_ms
                    .is_some_and(|since| now_ms.saturating_sub(since) >= COMMIT_AFTER_MS))
    }

    /// Everything staged, for one commit; the group starts over.
    fn take(&mut self) -> store::Staged {
        self.frames = 0;
        self.since_ms = None;
        std::mem::take(&mut self.staged)
    }
}

/// A helper that runs, and what the loop polls of it.
struct Live {
    helper: Helper,
    /// The account's TDLib files directory (emptied at every stop).
    files_dir: PathBuf,
    /// When this helper's start began (the clock's ms): its life for the back-off.
    began_ms: u64,
    /// The ONE request in flight, as the session handed it out (`Act::Send`).
    pending: Option<(Asked, Answer)>,
    /// The sync lane's ONE request in flight (`Act::Fetch`, TG-3 C7a): never the login's slot.
    fetch: Option<(LaneAsk, Answer)>,
    /// The watchdog's `testCallEmpty` in flight (TG-1b M4, lead L-6).
    probe: Option<Answer>,
    /// When the next probe is asked.
    probe_at: Instant,
    /// When the helper's last frame came (a heartbeat counts).
    last_frame: Instant,
    /// The online status's and the network hints' requests (C8c), one after the other; their
    /// answers are not read (fire and forget, never the login's pending slot).
    side: Option<Answer>,
}

/// What the actor holds.
struct Actor {
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Envelope>,
    cancel: CancellationToken,
    /// The account's `backend.db`, open for the actor's life (shared with the shipping arm's
    /// forwarder; the actor's thread owns both).
    conn: Rc<Connection>,
    /// The shipping arm (TG-3 C7b, lead L-4): receive rows reach the core while inbound flows.
    shipper: Shipper,
    /// The group commit (TG-3 C7b): what the receive staged since the last commit.
    group: Group,
    /// The start sync's barrier (lead L-6): `Session::on_synced` once no `core_outbox` row at or
    /// below this `seq` is left (`Some(0)`: nothing to wait for).
    synced_through: Option<i64>,
    /// The helper parts; a login or a `Connect` starts a helper from them.
    parts: TgParts,
    uuid: kourier_model::AccountUuid,
    backend_db: PathBuf,
    net_rt: tokio::runtime::Handle,
    /// TDLib's database key (base64), zeroized on drop.
    tdlib_key: Zeroizing<String>,
    env: TgEnv,
    timings: Timings,
    /// The helper's lock-down word was logged (once per actor).
    told_lockdown: bool,
    /// The account's connection state.
    conn_state: watch::Sender<ConnState>,
    clock: Arc<dyn Clock>,
    /// The helper's restart back-off.
    backoff: Backoff,
    /// A helper start to try again at the clock's ms (the session waits meanwhile).
    retry: Option<u64>,
    /// `BackendConfig::expect_number_suffix` (the run tool's expected tail).
    suffix: Option<Redacted<String>>,
    /// The account row exists (written by the first login of a `Fresh` actor).
    has_row: bool,
    /// The account's TDLib user id: the row's, or the one a login's ack set.
    user_id: Option<i64>,
    /// The live session (a login or a linked account); kept while a restart waits.
    session: Option<Session>,
    live: Option<Live>,
    /// A login cancelled after TDLib said `Ready` (a Telegram session exists: the `Logout` that
    /// follows must `logOut`, L5).
    was_authorized: bool,
    /// Commands that came while an `AccountReady` shipped, handled next (in order).
    later: VecDeque<Kind>,
    /// Read markers that came while no helper was live (the back-off, a start that failed; TF6):
    /// served, in order, once a helper is up on a linked session. At most [`HELD_MARKERS`]; past
    /// it a marker stays pending in the core, which sends it again at the next start.
    held: VecDeque<Kind>,
    /// What the UI shows (`AccountContext::ui`): the online status (H6, T-3).
    ui: watch::Receiver<UiPresence>,
    /// The core's end of `ui` is there (a dropped sender is read once, then never polled again).
    ui_open: bool,
    presence: Presence,
    hints: Hints,
}

/// What a command is, once the ones that carry a reply are answered (and the rest ignored). It
/// carries what the actor needs; it has no `Debug` (an input value may be a password).
enum Kind {
    Shutdown,
    Logout,
    StartLogin {
        flow: String,
        relink: Option<Relink>,
        taken: Vec<TakenIdentity>,
    },
    CancelLogin,
    /// `SubmitLoginInput`.
    Input {
        txn: u64,
        values: Vec<(String, Redacted<String>)>,
    },
    Connect,
    Disconnect,
    /// `NetworkChanged` or `Resumed` (C8c sets TDLib's network type).
    Hint(Hint),
    /// `SyncReadMarker` (critic H3): `viewMessages(force_read: true)` on the sync lane.
    ReadMarker {
        cmd: CmdId,
        conv: ConvKey,
        up_to: MsgTarget,
    },
    /// A durable command confirmed with nothing to send (`SendReceipts{Read}`, Q-L3: Telegram has
    /// ONE read act, the marker's).
    Persist(CmdId),
    /// `DownloadAttachment` (L-9): answered with a durable retryable failure until T6.
    Download {
        attachment: AttachmentId,
        request: DownloadId,
    },
    /// Answered or ignored already.
    Other,
}

/// Sorts a command (see [`Kind`]). EVERY `Command` variant has its own explicit arm here (no
/// wildcard): a new variant fails to compile, and is answered on purpose. It is used by the
/// actor's loop, by a start (G14) and by a shipping `AccountReady`, so a command is answered the
/// same way whenever it comes.
fn sort(command: Command) -> Kind {
    match command {
        Command::Shutdown => Kind::Shutdown,
        Command::Logout => Kind::Logout,
        Command::StartLogin {
            flow,
            relink,
            taken,
        } => Kind::StartLogin {
            flow,
            relink,
            taken,
        },
        Command::CancelLogin => Kind::CancelLogin,
        Command::Connect => Kind::Connect,
        Command::Disconnect => Kind::Disconnect,
        Command::NetworkChanged => Kind::Hint(Hint::NetworkChanged),
        Command::Resumed => Kind::Hint(Hint::Resumed),
        Command::SubmitLoginInput { txn, values } => Kind::Input { txn, values },
        Command::Transmit { reply, .. } => {
            let _ = reply.send(TransmitOutcome::Failed(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        // Nothing uploads, so nothing can be stopped.
        Command::AbortTransmit { reply, .. } => {
            let _ = reply.send(AbortAnswer::TooLate);
            Kind::Other
        }
        // Q-L3: Telegram has one read act (the marker's `viewMessages`); a read receipt is that act
        // already, confirmed with no request. `Viewed` waits for T4 (its row stays pending: no
        // `CommandPersisted` for work that was not done).
        Command::SendReceipts { cmd, kind, .. } => match kind {
            ReceiptKind::Read => Kind::Persist(cmd),
            // A delivery receipt is TDLib's own (it acknowledges what it receives); `Viewed`
            // waits for T4.
            ReceiptKind::Delivered | ReceiptKind::Viewed => Kind::Other,
        },
        Command::SyncReadMarker {
            cmd, conv, up_to, ..
        } => Kind::ReadMarker { cmd, conv, up_to },
        // Durable commands about Telegram conversations T3 does not carry out: their rows stay
        // pending (no `CommandPersisted` for work that was not done).
        Command::ClearConversation { .. } => Kind::Other,
        Command::DeleteAttachment { .. } => Kind::Other,
        Command::RequestAttachments { .. } => Kind::Other,
        Command::SetTyping { .. } => Kind::Other,
        // L-9: TG-3 hands the core media POINTERS; downloads come with T6. Every request is
        // answered (the core removes what it left at `dest` then) with a durable RETRYABLE
        // failure, WhatsApp's `answer_download`: the core asks again later.
        Command::DownloadAttachment {
            attachment,
            request,
            ..
        } => Kind::Download {
            attachment,
            request,
        },
        Command::ChangeConversation { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        // A10a: the core's own durable change of a chat (the clearing of a mark when a chat is read) is queued
        // only for an account whose caps state the roaming flag, which Telegram does not; a row left over is
        // confirmed and dropped, or the core would send it again at every start.
        Command::ChangeConversationDurable { cmd, .. } => {
            tracing::debug!("a durable conversation change is dropped");
            Kind::Persist(cmd)
        }
        // A10a: Telegram states no `roaming_writes` flag, so the switch is refused.
        Command::SetRoamingWrites { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        Command::ChangeContact { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        Command::ChangeAccountSettings { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        Command::ChangeProfile { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        // Telegram has no safety numbers (P5a): nothing pauses on a Telegram identity, so a
        // confirmation is an approval with nothing to do and a verification is not offered.
        Command::ConfirmIdentity { reply, .. } => {
            let _ = reply.send(Ok(()));
            Kind::Other
        }
        Command::SafetyNumber { reply, .. } => {
            let _ = reply.send(Ok(Err(kourier_model::SafetyUnavailable::Unsupported)));
            Kind::Other
        }
        Command::SetVerified { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        Command::FetchHistory { .. } => Kind::Other,
        Command::Resolve { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        Command::ListOwnDevices { reply } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        Command::RefreshContact { .. } => Kind::Other,
        Command::SubmitChallenge { reply, .. } => {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            Kind::Other
        }
        // No history transfer exists on Telegram.
        Command::SkipHistoryTransfer => Kind::Other,
        Command::DismissHistoryTransfer => Kind::Other,
    }
}

/// What woke the actor's loop (H1: the `select!` returns it, the handlers run after it).
enum Wake {
    Cancelled,
    /// A command (`None`: the core dropped its end).
    Command(Option<Command>),
    /// The restart's back-off is over.
    Due,
    /// A frame of the live helper (`None`: its reader ended).
    Inbound(Option<Inbound>),
    /// The answer of the request in flight.
    Answer(Asked, Result<Value, LinkError>),
    /// The session's deadline (the QR bound).
    Tick,
    /// No frame of the live helper for two heartbeats (M4).
    Silent,
    /// The watchdog's probe is due.
    ProbeTime,
    /// The watchdog's probe was answered.
    Probe(Result<Value, LinkError>),
    /// The UI's presence changed (`false`: the core dropped its end).
    Ui(bool),
    /// The online status's debounce is due (H6).
    PresenceTime,
    /// The side requests (online status, network type) were answered.
    Side,
    /// The sync lane's request in flight was answered (TG-3 C7a).
    Fetched(LaneAsk, Result<Value, LinkError>),
    /// The sync lane's pace or 429 pause is over.
    LaneDue,
    /// The group commit is due: 64 frames taken, 50 ms since the first staged item, or the
    /// inbound queue is empty (TG-3 C7b).
    Commit,
    /// The shipping arm's batch was answered (or its back-off ended; lead L-4).
    Acked(Sent),
}

impl Actor {
    async fn event_loop(&mut self) -> Exit {
        // Rows a crash left (an `AccountReady` whose ack never came) ship before anything else.
        if let Some(shipped) = self.ship_rows().await
            && let Err(Some(exit)) = self.after_ship(shipped, None)
        {
            return exit;
        }
        loop {
            if !self.held.is_empty()
                && self.live.is_some()
                && self.session.as_ref().is_some_and(Session::is_linked)
            {
                let held = std::mem::take(&mut self.held);
                self.later.extend(held);
            }
            while let Some(kind) = self.later.pop_front() {
                if let Some(exit) = self.kind(kind).await {
                    return exit;
                }
            }
            self.sync_presence();
            let wake = self.wake().await;
            if let Some(exit) = self.handle(wake).await {
                return exit;
            }
        }
    }

    /// The one `select!` of the loop: it only says what woke it.
    async fn wake(&mut self) -> Wake {
        let due = optional_sleep(&*self.clock, self.retry);
        let tick = optional_sleep(
            &*self.clock,
            self.session.as_ref().and_then(Session::deadline_ms),
        );
        let presence_at = optional_sleep(&*self.clock, self.presence.due_ms());
        let lane_due = optional_sleep(
            &*self.clock,
            if self.live.is_some() {
                self.session.as_ref().and_then(Session::lane_due_ms)
            } else {
                None
            },
        );
        let beat = self.timings.beat;
        let staged = !self.group.is_empty();
        let commit_full = self.group.full(self.clock.now_ms());
        let Self {
            cancel,
            commands,
            live,
            ui,
            ui_open,
            shipper,
            ..
        } = self;
        let shipping = shipper.future();
        let acked = async move {
            match shipping {
                Some(fut) => fut.as_mut().await,
                None => std::future::pending().await,
            }
        };
        let ui_open = *ui_open;
        let (inbound, pending, fetch, probe, side, silent_at, probe_at) = match live.as_mut() {
            Some(Live {
                helper,
                pending,
                fetch,
                probe,
                side,
                last_frame,
                probe_at,
                ..
            }) => {
                let probe_due = if probe.is_none() {
                    Some(*probe_at)
                } else {
                    None
                };
                (
                    Some(helper.inbound()),
                    pending.as_mut(),
                    fetch.as_mut(),
                    probe.as_mut(),
                    side.as_mut(),
                    Some(*last_frame + beat * 2),
                    probe_due,
                )
            }
            None => (None, None, None, None, None, None, None),
        };
        let inbound = async move {
            match inbound {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        let answer = async move {
            match pending {
                Some((asked, fut)) => {
                    let result = fut.as_mut().await;
                    (*asked, result)
                }
                None => std::future::pending().await,
            }
        };
        let fetched = async move {
            match fetch {
                Some((ask, fut)) => {
                    let result = fut.as_mut().await;
                    (ask.clone(), result)
                }
                None => std::future::pending().await,
            }
        };
        let probed = async move {
            match probe {
                Some(fut) => fut.as_mut().await,
                None => std::future::pending().await,
            }
        };
        let sided = async move {
            match side {
                Some(fut) => {
                    let _ = fut.as_mut().await;
                }
                None => std::future::pending().await,
            }
        };
        let silent = optional_instant(silent_at);
        let probe_time = optional_instant(probe_at);
        tokio::select! {
            biased;
            () = cancel.cancelled() => Wake::Cancelled,
            // A full group commit goes before the next frame (64 frames or 50 ms).
            () = std::future::ready(()), if commit_full => Wake::Commit,
            // The core's ack frees the arm for the next batch: before the frames, so a steady
            // stream of updates never starves the shipping.
            sent = acked => Wake::Acked(sent),
            // Frames before answers: TDLib sends the next state BEFORE the answer of the request
            // that caused it, and the two travel apart (the update queue, the answer's oneshot).
            frame = inbound => Wake::Inbound(frame),
            (asked, result) = answer => Wake::Answer(asked, result),
            (ask, result) = fetched => Wake::Fetched(ask, result),
            result = probed => Wake::Probe(result),
            // No frame and no answer is ready: the inbound queue is empty, the commit is due.
            () = std::future::ready(()), if staged => Wake::Commit,
            command = commands.recv() => Wake::Command(command),
            changed = ui.changed(), if ui_open => Wake::Ui(changed.is_ok()),
            () = sided => Wake::Side,
            () = presence_at => Wake::PresenceTime,
            () = lane_due => Wake::LaneDue,
            () = due => Wake::Due,
            () = tick => Wake::Tick,
            () = silent => Wake::Silent,
            () = probe_time => Wake::ProbeTime,
        }
    }

    /// One wake. `Some` ends the actor.
    async fn handle(&mut self, wake: Wake) -> Option<Exit> {
        match wake {
            Wake::Cancelled => {
                self.stop_live(true).await;
                Some(Exit::Shutdown)
            }
            // The core dropped its end: the same as a shutdown.
            Wake::Command(None) => self.kind(Kind::Shutdown).await,
            Wake::Command(Some(command)) => self.kind(sort(command)).await,
            Wake::Due => {
                self.retry = None;
                if self.live.is_none() && self.session.as_ref().is_some_and(|s| !s.is_quiet()) {
                    self.bring_up().await
                } else {
                    None
                }
            }
            Wake::Inbound(frame) => {
                self.group.on_frame();
                self.inbound(frame).await
            }
            Wake::Commit => self.commit_or_restart().await,
            Wake::Acked(sent) => self.acked(sent).await,
            Wake::Answer(asked, result) => {
                if let Some(live) = self.live.as_mut() {
                    live.pending = None;
                }
                let now = self.clock.now_ms();
                let acts = match self.session.as_mut() {
                    Some(session) => session.on_answer(asked, result, now),
                    None => Vec::new(),
                };
                self.run_acts(acts).await
            }
            Wake::Fetched(ask, result) => {
                if let Some(live) = self.live.as_mut() {
                    live.fetch = None;
                }
                let now = self.clock.now_ms();
                let acts = match self.session.as_mut() {
                    Some(session) => session.on_fetched(ask, result, now),
                    None => Vec::new(),
                };
                self.run_acts(acts).await
            }
            Wake::LaneDue => {
                let now = self.clock.now_ms();
                let acts: Vec<Act> = match self.session.as_mut() {
                    Some(session) => session.pump_lane(now).into_iter().collect(),
                    None => Vec::new(),
                };
                self.run_acts(acts).await
            }
            Wake::Tick => {
                let now = self.clock.now_ms();
                let acts = match self.session.as_mut() {
                    Some(session) => session.on_tick(now),
                    None => Vec::new(),
                };
                self.run_acts(acts).await
            }
            Wake::Silent => {
                tracing::debug!(
                    "the TDLib helper sent nothing for two heartbeats; it is restarted"
                );
                let acts = self.restart(Retry::Abort, true).await;
                self.run_acts(acts).await
            }
            Wake::ProbeTime => {
                if let Some(live) = self.live.as_mut() {
                    let link = live.helper.link().clone();
                    let client = live.helper.client;
                    live.probe = Some(Box::pin(async move {
                        link.request(client, json!({"@type": "testCallEmpty"}))
                            .await
                    }));
                }
                None
            }
            Wake::Probe(result) => {
                let every = self.timings.probe_every;
                if let Some(live) = self.live.as_mut() {
                    live.probe = None;
                    live.probe_at = Instant::now() + every;
                }
                match result {
                    // An error answer is an answer; only silence and the link's end are not.
                    Err(LinkError::Timeout) => {
                        tracing::debug!(
                            "TDLib did not answer the watchdog's probe; it is restarted"
                        );
                        let acts = self.restart(Retry::Abort, true).await;
                        self.run_acts(acts).await
                    }
                    Err(LinkError::Closed) => {
                        let acts = self.restart(Retry::Abort, false).await;
                        self.run_acts(acts).await
                    }
                    _ => None,
                }
            }
            Wake::Ui(open) => {
                let online = if open {
                    presence::online_of(&self.ui.borrow_and_update())
                } else {
                    // The core is going away: nothing shows this account any more.
                    self.ui_open = false;
                    false
                };
                self.presence.on_ui(online, self.clock.now_ms());
                None
            }
            // `sync_presence` runs before the next wake.
            Wake::PresenceTime => None,
            Wake::Side => {
                if let Some(live) = self.live.as_mut() {
                    live.side = None;
                }
                None
            }
        }
    }

    /// Tells TDLib the online status the debounce wants now (H6): only a linked session past
    /// `Ready` on a live helper (never before Ready). Called before every wake.
    fn sync_presence(&mut self) {
        let ready = self.live.is_some()
            && self
                .session
                .as_ref()
                .is_some_and(|s| s.is_linked() && s.authorized());
        self.presence.set_ready(ready);
        if let Some(online) = self.presence.poll(self.clock.now_ms()) {
            self.side_request(presence::online_request(online));
        }
    }

    /// A request whose answer is not read (the online status, the network type), after the side
    /// requests before it; never the login's pending slot. Without a helper it is not sent.
    fn side_request(&mut self, request: Value) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let link = live.helper.link().clone();
        let client = live.helper.client;
        let before = live.side.take();
        live.side = Some(Box::pin(async move {
            if let Some(before) = before {
                let _ = before.await;
            }
            link.request(client, request).await
        }));
    }

    /// A frame of the live helper.
    async fn inbound(&mut self, frame: Option<Inbound>) -> Option<Exit> {
        if let Some(live) = self.live.as_mut() {
            live.last_frame = Instant::now();
        }
        match frame {
            Some(Inbound::Update(json)) => {
                let json = Zeroizing::new(json);
                let now = self.clock.now_ms();
                let acts = match self.session.as_mut() {
                    Some(session) => session.on_inbound(&json, now),
                    None => Vec::new(),
                };
                drop(json);
                self.run_acts(acts).await
            }
            Some(Inbound::Heartbeat | Inbound::Hello(_) | Inbound::GateProxy { .. }) => None,
            Some(Inbound::Refused(_)) => {
                tracing::debug!("the TDLib helper refused a request");
                None
            }
            // The helper is going away (B1: during a login no step; the back-off restart, then
            // TDLib's persisted state is shown again).
            Some(Inbound::Fatal(_) | Inbound::Bye(_) | Inbound::Garbled | Inbound::Closed)
            | None => {
                tracing::debug!("the TDLib helper ended; it is started again after the back-off");
                let acts = self.restart(Retry::Abort, false).await;
                self.run_acts(acts).await
            }
        }
    }

    /// One command, sorted. `Some` ends the actor.
    async fn kind(&mut self, kind: Kind) -> Option<Exit> {
        match kind {
            Kind::Shutdown => {
                // A clean stop (TG-3 C10a, brief §4.5, decision Q-D3): with a live helper TDLib is
                // DRAINED first (`close`, its last updates into the receive path, until `Closed`
                // or [`CLOSE_DRAIN`]); then the LAST commit says the run ended cleanly
                // (`run_state.clean_end = 1`) in the transaction of its rows, ONLY when `Closed`
                // was seen (on the bound the next start's check gaps run). Nothing is read from
                // the helper after it.
                let closed = self.live.is_some() && self.drain_close().await;
                // Messages still waiting for their chat, or dropped ones no gap row covers (TF3),
                // keep the run unclean: the next start's checks must run.
                let settled = !self
                    .session
                    .as_ref()
                    .is_some_and(Session::receive_unsettled);
                self.commit_with(closed && settled).await;
                self.stop_live(false).await;
                Some(Exit::Shutdown)
            }
            Kind::Logout => Some(self.logout().await),
            Kind::StartLogin {
                flow,
                relink,
                taken,
            } => self.start_login(&flow, relink, taken).await,
            Kind::CancelLogin => {
                self.cancel_login();
                None
            }
            Kind::Input { txn, values } => {
                // While a restart waits the step is shown again with a new txn: this one is stale.
                if self.live.is_none() {
                    wipe(values);
                    return None;
                }
                let now = self.clock.now_ms();
                let acts = match self.session.as_mut() {
                    Some(session) => session.on_input(txn, values, now),
                    None => {
                        wipe(values);
                        Vec::new()
                    }
                };
                self.run_acts(acts).await
            }
            Kind::Connect => self.connect().await,
            Kind::Disconnect => {
                self.stop_live(false).await;
                self.session = None;
                self.retry = None;
                self.conn_state
                    .send_replace(ConnState::Offline { retry_at_ms: None });
                None
            }
            // TDLib's network type (critic M8): only while a helper runs; `NetworkChanged` only while
            // TDLib is not connected; at most once per 10 s.
            Kind::Hint(hint) => {
                let now = self.clock.now_ms();
                let last = self.session.as_ref().and_then(Session::last_connection);
                if self.live.is_some() && self.hints.on_hint(hint, last, now) {
                    self.side_request(presence::network_type_request());
                }
                None
            }
            Kind::ReadMarker { cmd, conv, up_to } => self.read_marker(cmd, &conv, &up_to).await,
            Kind::Persist(cmd) => self.run_acts(vec![persisted(&[cmd])]).await,
            Kind::Download {
                attachment,
                request,
            } => {
                self.run_acts(vec![download_answer(attachment, request)])
                    .await
            }
            Kind::Other => None,
        }
    }

    /// `SyncReadMarker` (critic H3, Q-L3): the delivered ids of the chat up to the marker (with
    /// their direction) go to the session's read act, which tells TDLib the incoming ones. Without a linked session on a live helper the command stays
    /// pending (the core sends it again at the next start); a conversation or a target that is
    /// no Telegram chat or message has nothing to tell and is confirmed.
    async fn read_marker(&mut self, cmd: CmdId, conv: &ConvKey, up_to: &MsgTarget) -> Option<Exit> {
        let ready = self.live.is_some() && self.session.as_ref().is_some_and(Session::is_linked);
        if !ready {
            // No live helper (the back-off): the marker is served when one is back (TF6). The core
            // sends pending commands again only at the backend's start, not at a helper restart.
            if self.held.len() < HELD_MARKERS {
                self.held.push_back(Kind::ReadMarker {
                    cmd,
                    conv: conv.clone(),
                    up_to: up_to.clone(),
                });
            }
            tracing::debug!("a read marker waits for the helper");
            return None;
        }
        let target = chat_id_of(conv).zip(up_to.local.parse::<i64>().ok().filter(|&id| id > 0));
        let Some((chat_id, max_id)) = target else {
            return self.run_acts(vec![persisted(&[cmd])]).await;
        };
        let rows =
            match store::delivered_ids(&self.conn, chat_id, max_id, store::DELIVERED_KEEP_PER_CHAT)
            {
                Ok(rows) => rows,
                Err(_) => {
                    tracing::debug!("the delivered messages of a read marker could not be read");
                    return None;
                }
            };
        // The session tells TDLib only the INCOMING ids (U2, honesty HIGH-1) and stores their
        // highest as the read position: it gets the direction of every id.
        let now = self.clock.now_ms();
        let acts = match self.session.as_mut() {
            Some(session) => session.read_marker(chat_id, rows, cmd, now),
            None => Vec::new(),
        };
        self.run_acts(acts).await
    }

    /// `StartLogin`: a new login session over a new helper. A login that runs ignores another.
    async fn start_login(
        &mut self,
        flow: &str,
        relink: Option<Relink>,
        taken: Vec<TakenIdentity>,
    ) -> Option<Exit> {
        if self.session.as_ref().is_some_and(|s| !s.is_quiet()) {
            tracing::debug!("a Telegram login while another runs was ignored");
            return None;
        }
        // A helper an ended or cancelled login left goes first.
        self.stop_live(true).await;
        self.retry = None;
        let Some(flow) = Flow::parse(flow) else {
            let step = LoginStep {
                step_id: step::QR.to_owned(),
                txn: 1,
                kind: LoginStepKind::Failed {
                    error: BackendError::unsupported(NO_SUCH_FLOW),
                },
            };
            self.emit(Envelope::ephemeral(BackendEvent::Login(step)))
                .await;
            return None;
        };
        let rules = Rules::new(taken, relink, self.suffix.clone());
        // The one fixed token that says whether the number is checked (no digits): a run tool
        // reads it before it asks for a number or a scan. The fake that plays Telegram prints the
        // same words (`kourier-fake` `actor/phone.rs`; `tests/source.rs` compares the two).
        tracing::info!(
            expected_number_check = rules.number_check(),
            "a telegram login starts"
        );
        let mut session = Session::login(Login::new(flow, rules));
        // No API pair: answered before any row, gate, helper, directory or library (TG-1b G1).
        if self.parts.api().is_none() {
            tracing::debug!("a Telegram login is not available in this build");
            let acts = session.fail_login(BackendError::unsupported(NO_API_KEY));
            self.session = Some(session);
            return self.run_acts(acts).await;
        }
        // The first login of a `Fresh` actor writes the row (the environment binds) BEFORE the
        // helper starts.
        if !self.has_row {
            let row = db::AccountRow {
                env: self.env,
                user_id: None,
                pin: PIN.commit.to_owned(),
                created_ms: i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX),
            };
            if db::create_account(&self.conn, &row).is_err() {
                let acts =
                    session.fail_login(BackendError::internal("the account's database failed"));
                self.session = Some(session);
                return self.run_acts(acts).await;
            }
            self.has_row = true;
        }
        self.session = Some(session);
        self.bring_up().await
    }

    /// `CancelLogin`: the login session goes (no step); its helper stays for the `Logout` that
    /// follows at once (L5: its `logOut` also ends a login TDLib already finished).
    fn cancel_login(&mut self) {
        if self.session.as_ref().is_some_and(Session::is_linked) {
            return;
        }
        if let Some(session) = self.session.take() {
            // Across restarts too (J6): a Telegram session exists once TDLib said Ready.
            self.was_authorized |= session.ever_authorized();
        }
        if let Some(live) = self.live.as_mut() {
            live.pending = None;
        }
        if self.retry.take().is_some() {
            self.not_waiting();
        }
    }

    /// `Connect` of a linked account: a resumed session over a new helper.
    async fn connect(&mut self) -> Option<Exit> {
        if self.session.is_some() || !self.has_row {
            return None;
        }
        if self.user_id.is_none() {
            tracing::debug!("a Connect of an account whose login never finished was ignored");
            return None;
        }
        self.session = Some(Session::resume(self.user_id));
        self.retry = None;
        self.bring_up().await
    }

    /// The account waits for no restart any more.
    fn not_waiting(&self) {
        if matches!(
            *self.conn_state.borrow(),
            ConnState::Offline {
                retry_at_ms: Some(_)
            }
        ) {
            self.conn_state
                .send_replace(ConnState::Offline { retry_at_ms: None });
        }
    }

    /// Starts a helper for the session, up to its first state, then hands that state (and the
    /// start's frames, M3) to the session. `Some` ends the actor.
    ///
    /// The start runs under a child token of the actor's, and the commands are served meanwhile
    /// (G14): a `Shutdown` or a `Logout` cancels it (the helper is killed and reaped at once, the
    /// actor ends), a `CancelLogin` or a `Disconnect` cancels it with no step, another login, an
    /// input (its step is shown again after the start, txn + 1) and a hint are dropped, and every
    /// command with a reply is answered at once.
    async fn bring_up(&mut self) -> Option<Exit> {
        let began = self.clock.now_ms();
        let child = self.cancel.child_token();
        let mut interrupt: Option<Kind> = None;
        let (started, lockdown) = {
            let start = start_helper(
                StartInputs {
                    parts: &self.parts,
                    uuid: self.uuid,
                    backend_db: &self.backend_db,
                    key: &self.tdlib_key,
                    env: self.env,
                    timings: &self.timings,
                    net_rt: &self.net_rt,
                },
                &child,
            );
            tokio::pin!(start);
            loop {
                tokio::select! {
                    biased;
                    done = &mut start => break done,
                    command = self.commands.recv(), if interrupt.is_none() => {
                        // The core dropped its end: the same as a shutdown.
                        match command.map_or(Kind::Shutdown, sort) {
                            Kind::Other
                            | Kind::Connect
                            | Kind::Hint(_)
                            | Kind::StartLogin { .. } => {}
                            // Durable: handled after the start, in order.
                            durable @ (Kind::ReadMarker { .. }
                            | Kind::Persist(_)
                            | Kind::Download { .. }) => {
                                self.later.push_back(durable);
                            }
                            // Dropped unread: zeroed (J14).
                            Kind::Input { values, .. } => wipe(values),
                            stop @ (Kind::Shutdown
                            | Kind::Logout
                            | Kind::CancelLogin
                            | Kind::Disconnect) => {
                                interrupt = Some(stop);
                                child.cancel();
                            }
                        }
                    }
                }
            }
        };
        if let Some(word) = lockdown
            && !self.told_lockdown
        {
            self.told_lockdown = true;
            tracing::info!(lockdown = word, "telegram helper lock-down");
        }
        match interrupt {
            Some(Kind::Shutdown) => return Some(Exit::Shutdown),
            Some(Kind::Logout) => return Some(self.logout().await),
            Some(Kind::CancelLogin) => {
                self.cancel_login();
                self.not_waiting();
                return None;
            }
            Some(Kind::Disconnect) => {
                self.session = None;
                self.conn_state
                    .send_replace(ConnState::Offline { retry_at_ms: None });
                return None;
            }
            _ => {}
        }
        match started {
            Started::Cancelled => Some(Exit::Shutdown),
            Started::Up(helper, files_dir) => {
                let mut helper = *helper;
                // A start that worked starts the back-off over (G13).
                self.backoff.reached();
                self.not_waiting();
                let first = helper.take_first();
                let now = Instant::now();
                self.live = Some(Live {
                    helper,
                    files_dir,
                    began_ms: began,
                    pending: None,
                    fetch: None,
                    probe: None,
                    probe_at: now + self.timings.probe_every,
                    last_frame: now,
                    side: None,
                });
                let now_ms = self.clock.now_ms();
                // A helper run begins: how the last one ended decides the unclean-end rule (B1).
                let after_unclean = match store::start_run(&self.conn) {
                    Ok(previous) => previous == store::PreviousRun::Unclean,
                    Err(_) => {
                        tracing::debug!("the run state could not be written");
                        true
                    }
                };
                // The machines start from the store (C8): cursors, read positions, delivered.
                let seed = store::seed(&self.conn).unwrap_or_else(|_| {
                    tracing::debug!("the chat state could not be read; the run starts unseeded");
                    store::Seed::default()
                });
                let acts = match (self.session.as_mut(), first) {
                    (Some(session), Some(first)) => {
                        #[cfg(test)]
                        session.set_learn_bound(self.timings.learn_bound);
                        let mut acts = session.begin_run_seeded(now_ms, after_unclean, &seed);
                        acts.extend(session.on_first(first, now_ms));
                        acts
                    }
                    _ => Vec::new(),
                };
                if self.session.is_none() {
                    self.stop_live(false).await;
                    return None;
                }
                self.run_acts(acts).await
            }
            Started::Answer(error) => {
                self.not_waiting();
                tracing::debug!("a Telegram login is not available in this build");
                let in_login = self.session.as_ref().is_some_and(Session::in_login);
                if in_login {
                    let acts = self
                        .session
                        .as_mut()
                        .map_or_else(Vec::new, |s| s.fail_login(error));
                    self.run_acts(acts).await
                } else {
                    // A linked account whose helper cannot start (no API pair, no library,
                    // another helper): told once; a later `Connect` tries again.
                    self.session = None;
                    self.conn_state
                        .send_replace(ConnState::Offline { retry_at_ms: None });
                    self.emit(Envelope::ephemeral(BackendEvent::Notice(
                        BackendNotice::Error(error),
                    )))
                    .await;
                    None
                }
            }
            Started::Again(retry) => {
                // The helper's life: from this start to its failure, on the actor's clock.
                let life = Duration::from_millis(self.clock.now_ms().saturating_sub(began));
                let acts = self.schedule(retry, life).await;
                self.run_acts(acts).await
            }
        }
    }

    /// The live helper is gone or given up (`kill`: at once, no `Q` wait): it is stopped, and the
    /// session's helper is started again after the back-off. The acts: a login whose restart
    /// budget is used up ends `Failed` (B1).
    async fn restart(&mut self, retry: Retry, kill: bool) -> Vec<Act> {
        self.synced_through = None;
        let now = self.clock.now_ms();
        let began = self.live.as_ref().map_or(now, |live| live.began_ms);
        self.stop_live(kill).await;
        if self.session.as_ref().is_none_or(Session::is_quiet) {
            return Vec::new();
        }
        let life = Duration::from_millis(now.saturating_sub(began));
        self.schedule(retry, life).await
    }

    /// The next start after the back-off (`Offline { retry_at_ms }`; five aborts in ten minutes
    /// tell the person once, or end a login: B1).
    async fn schedule(&mut self, retry: Retry, life: Duration) -> Vec<Act> {
        let now = self.clock.now_ms();
        let (wait, tell) = self.backoff.failed(retry, life, now);
        if let Some(session) = self.session.as_mut() {
            session.on_restart();
        }
        if tell && self.session.as_ref().is_some_and(Session::in_login) {
            self.retry = None;
            self.not_waiting();
            return self.session.as_mut().map_or_else(Vec::new, |s| {
                s.fail_login(BackendError::transient(KEEPS_STOPPING))
            });
        }
        let at = now.saturating_add(duration_ms(wait));
        self.retry = Some(at);
        self.conn_state.send_replace(ConnState::Offline {
            retry_at_ms: Some(at),
        });
        tracing::debug!(
            wait_ms = duration_ms(wait),
            "the TDLib helper is started again after the back-off"
        );
        if tell {
            self.emit(Envelope::ephemeral(BackendEvent::Notice(
                BackendNotice::Error(BackendError::transient(KEEPS_STOPPING)),
            )))
            .await;
        }
        // A database that is still in use is told at the second try in a row (G12; a notice
        // since TG-2, L2); the restarts go on.
        if retry == Retry::Transient && self.backoff.transients() == IN_USE_ANSWER_AT {
            self.emit(Envelope::ephemeral(BackendEvent::Notice(
                BackendNotice::Error(BackendError::transient(DATABASE_IN_USE)),
            )))
            .await;
        }
        Vec::new()
    }

    /// Carries the session's acts out, in order. `Some` ends the actor.
    async fn run_acts(&mut self, acts: Vec<Act>) -> Option<Exit> {
        let mut queue: VecDeque<Act> = acts.into();
        while let Some(act) = queue.pop_front() {
            match act {
                Act::Step(step) => {
                    self.emit(Envelope::ephemeral(BackendEvent::Login(step)))
                        .await;
                }
                Act::Send { asked, json } => self.send(asked, json),
                Act::AppendReady { user_id, event } => match self.ready(user_id, &event).await {
                    Ok(more) => queue.extend(more),
                    // A `Shutdown` or a `Logout` came while it shipped: `later` holds it.
                    Err(None) => return None,
                    Err(Some(exit)) => {
                        self.stop_live(true).await;
                        return Some(exit);
                    }
                },
                Act::Conn(state) => {
                    self.conn_state.send_replace(state);
                }
                Act::Hold(hold) => {
                    self.emit(Envelope::ephemeral(BackendEvent::AccountHold(hold)))
                        .await;
                }
                Act::Restart => {
                    let more = self.restart(Retry::Abort, false).await;
                    queue.extend(more);
                }
                // The group commit: staged now, written at `Wake::Commit`.
                Act::Rows(staged) => self.stage(*staged),
                Act::Synced => {
                    if !self.commit().await {
                        // The start sync's rows were not written: the run ends (TF1).
                        let more = self.dropped_batch().await;
                        queue.extend(more);
                        continue;
                    }
                    self.synced_through = match outbox::newest_seq(&self.conn) {
                        Ok(seq) => Some(seq.unwrap_or(0)),
                        Err(_) => {
                            tracing::debug!("the start sync's rows could not be read");
                            Some(0)
                        }
                    };
                    queue.extend(self.check_synced());
                }
                Act::Ephemeral(event) => {
                    self.emit(Envelope::ephemeral(*event)).await;
                }
                Act::Fetch { ask, json } => self.send_fetch(ask, json),
                Act::End(exit) => {
                    self.stop_live(false).await;
                    self.session = None;
                    // A remote logout (M7): TDLib's unfinished logout must not greet the next
                    // relink (J8); the core wipes the credentials after this exit.
                    if matches!(exit, Exit::LoggedOut { .. }) {
                        self.remove_tdlib();
                    }
                    return Some(exit);
                }
            }
        }
        None
    }

    /// Adds a receive commit's items to the group commit.
    fn stage(&mut self, staged: store::Staged) {
        self.group.stage(staged, self.clock.now_ms());
    }

    /// The group commit (TG-3 C7b): the chat machine's held patches first (run B Q-B2: once per
    /// commit batch), then everything staged in ONE `store::commit_rows`, then the shipping arm
    /// is kicked. `false`: the commit FAILED and its items are dropped; the machines' memory is
    /// ahead of the file then, and TDLib has no ack (what it handed out is gone from its stream),
    /// so the caller ends the helper run ([`Self::dropped_batch`], fix round TF1).
    async fn commit(&mut self) -> bool {
        self.commit_with(false).await
    }

    /// [`Self::commit`] for the wake that has nothing else to do with a failure: a dropped batch
    /// restarts the helper at once and the acts of that restart are carried out.
    async fn commit_or_restart(&mut self) -> Option<Exit> {
        if self.commit().await {
            return None;
        }
        let acts = self.dropped_batch().await;
        self.run_acts(acts).await
    }

    /// A receive commit failed (TF1, review dF1): the receive machines moved on while the file
    /// did not, and TDLib will not say it again. The helper run ends like any helper failure
    /// (`Retry::Abort`: the back-off): nothing more is taken from this helper, the machines are
    /// seeded from the store at the next start, whose `start_run` reads `Unclean` (`clean_end`
    /// was never set), so the start decision, B1's check gap and the id trigger find again what
    /// TDLib saved. Only the receive machine is forgotten here, so the stop's own commit (the
    /// machine's held patches) writes nothing of the state that is ahead.
    async fn dropped_batch(&mut self) -> Vec<Act> {
        tracing::warn!(
            "received Telegram rows could not be written; the TDLib helper is restarted"
        );
        if let Some(session) = self.session.as_mut() {
            session.drop_receive();
        }
        self.restart(Retry::Abort, false).await
    }

    /// [`Self::commit`]; with `clean_end` the commit is a clean stop's last one (it is made even
    /// when nothing else is staged). `false`: the write failed (nothing of `clean_end` is kept
    /// either: the run stays unclean).
    async fn commit_with(&mut self, clean_end: bool) -> bool {
        let flushed = match self.session.as_mut() {
            Some(session) => session.flush_receive(),
            None => Vec::new(),
        };
        for act in flushed {
            match act {
                Act::Rows(staged) => self.stage(*staged),
                Act::Fetch { ask, json } => self.send_fetch(ask, json),
                Act::Ephemeral(event) => self.emit(Envelope::ephemeral(*event)).await,
                other => tracing::debug!(act = ?other, "a flush handed out an act it never makes"),
            }
        }
        let mut staged = self.group.take();
        staged.clean_end |= clean_end;
        let mut written = true;
        if !staged.is_empty() {
            let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
            written = store::commit_rows(&self.conn, &staged, now).is_ok();
        }
        self.kick_ship();
        written
    }

    /// Hands the next batch to the shipping arm when nothing is in flight.
    fn kick_ship(&mut self) {
        if self.shipper.kick(&self.conn, &self.events).is_err() {
            tracing::debug!("the shipping arm could not read core_outbox");
        }
    }

    /// The shipping arm's future came to `sent` (L-4): settle it, ship the next batch, and tell
    /// the session when the start sync's rows are acked (L-6).
    async fn acked(&mut self, sent: Sent) -> Option<Exit> {
        match self.shipper.settle(sent) {
            Settled::CoreGone => {
                self.stop_live(true).await;
                return Some(Exit::Shutdown);
            }
            Settled::More | Settled::Store => {}
        }
        self.kick_ship();
        let acts = self.check_synced();
        self.run_acts(acts).await
    }

    /// `Session::on_synced` once the start sync's barrier holds (no row at or below it is left).
    fn check_synced(&mut self) -> Vec<Act> {
        let Some(through) = self.synced_through else {
            return Vec::new();
        };
        let left = match outbox::oldest_seq(&self.conn) {
            Ok(oldest) => oldest.is_some_and(|oldest| oldest <= through),
            Err(_) => true,
        };
        if left {
            return Vec::new();
        }
        self.synced_through = None;
        // A quiet point, once per helper run (TF5, review dF5): `delivered` keeps what rule L-5
        // keeps, so the next start's seed stays small.
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        if store::sweep(&self.conn, now).is_err() {
            tracing::debug!("the delivered rows could not be swept");
        }
        self.session
            .as_mut()
            .map_or_else(Vec::new, Session::on_synced)
    }

    /// Hands a request to the live helper as the ONE pending answer (H1). Without a helper the
    /// request goes with it (the restart's `on_restart` forgets what was in flight).
    fn send(&mut self, asked: Asked, json: Zeroizing<String>) {
        let Some(live) = self.live.as_mut() else {
            drop(json);
            return;
        };
        let link = live.helper.link().clone();
        let client = live.helper.client;
        live.pending = Some((
            asked,
            Box::pin(async move { link.request_secret(client, json).await }),
        ));
    }

    /// Hands a sync request to the live helper as the sync lane's ONE request in flight (never
    /// the login's `pending`). Without a helper it is dropped (the restart's `on_restart` gave the
    /// lane its page back).
    fn send_fetch(&mut self, ask: LaneAsk, json: Value) {
        let Some(live) = self.live.as_mut() else {
            return;
        };
        let link = live.helper.link().clone();
        let client = live.helper.client;
        live.fetch = Some((
            ask,
            Box::pin(async move { link.request(client, json).await }),
        ));
    }

    /// `Act::AppendReady`: the row and the user id in one transaction, then shipped; the acts of
    /// the ack (or of the failure). `Err(None)`: a `Shutdown` or `Logout` came while it shipped;
    /// `Err(Some)`: the actor ends.
    async fn ready(
        &mut self,
        user_id: i64,
        event: &BackendEvent,
    ) -> Result<Vec<Act>, Option<Exit>> {
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let seq = match outbox::append_ready(&self.conn, user_id, event, now) {
            Ok(seq) => seq,
            Err(_) => {
                tracing::debug!("the AccountReady of a Telegram login could not be written");
                return Ok(self
                    .session
                    .as_mut()
                    .map_or_else(Vec::new, Session::on_ready_failed));
            }
        };
        match self.ship_rows().await {
            Some(shipped) => self.after_ship(shipped, Some(seq)),
            None => Err(None),
        }
    }

    /// What a shipping pass means for the `AccountReady` of `seq` (if one waits).
    fn after_ship(&mut self, shipped: Shipped, seq: Option<i64>) -> Result<Vec<Act>, Option<Exit>> {
        match shipped.end {
            ShipEnd::CoreGone | ShipEnd::Cancelled => return Err(Some(Exit::Shutdown)),
            ShipEnd::Drained | ShipEnd::Store(_) => {}
        }
        let Some(seq) = seq else {
            return Ok(Vec::new());
        };
        if shipped.acked.is_some_and(|acked| acked >= seq) {
            let acts = self
                .session
                .as_mut()
                .map_or_else(Vec::new, Session::on_acked);
            if let Some(session) = self.session.as_ref()
                && session.is_linked()
            {
                self.user_id = session.user_id();
            }
            Ok(acts)
        } else if shipped.quarantined > 0 || matches!(shipped.end, ShipEnd::Store(_)) {
            Ok(self
                .session
                .as_mut()
                .map_or_else(Vec::new, Session::on_ready_failed))
        } else {
            Ok(Vec::new())
        }
    }

    /// Ships every waiting `core_outbox` row (`outbox::ship_pending`) while the commands are
    /// served: one with a reply is answered at once, the rest wait in `later` (in order). A
    /// `Shutdown` or a `Logout` drops the pass (`None`; cancel-safe: an unacked row stays and the
    /// next pass sends it again, and the core takes a repeated `AccountReady`, H1).
    async fn ship_rows(&mut self) -> Option<Shipped> {
        // The arm's batch in flight ships again in this pass (the core takes the repeat).
        self.shipper.abandon();
        let shipped = self.ship_blocking().await;
        self.kick_ship();
        shipped
    }

    /// [`Actor::ship_rows`]'s pass itself.
    async fn ship_blocking(&mut self) -> Option<Shipped> {
        let Self {
            conn,
            events,
            commands,
            cancel,
            later,
            ..
        } = self;
        let ship = outbox::ship_pending(conn, events, cancel);
        tokio::pin!(ship);
        loop {
            tokio::select! {
                biased;
                shipped = &mut ship => return Some(shipped),
                command = commands.recv() => match command.map_or(Kind::Shutdown, sort) {
                    Kind::Other => {}
                    stop @ (Kind::Shutdown | Kind::Logout) => {
                        later.push_back(stop);
                        return None;
                    }
                    kind => later.push_back(kind),
                },
            }
        }
    }

    /// A clean stop's drain (decision Q-D3): the account is told offline first (as
    /// [`Self::stop_live`] does, which then sends nothing more), then TDLib is asked to `close`
    /// over the link, and every frame is read into the receive path (its rows staged for the
    /// last commit) until TDLib's `authorizationStateClosed` (`true`) or [`CLOSE_DRAIN`], the
    /// helper's end or a cancel (`false`). While it drains, an authorization state is no restart
    /// and no logout (the actor knows it stops), and only rows and ephemeral events are carried
    /// out: a fetch is not sent (an open gap stays open for the next run).
    async fn drain_close(&mut self) -> bool {
        let cancel = self.cancel.clone();
        let Some(live) = self.live.as_mut() else {
            return false;
        };
        let link = live.helper.link().clone();
        let client = live.helper.client;
        if self.presence.on_stop(self.clock.now_ms()) {
            let offline = link.request(client, presence::online_request(false));
            tokio::select! {
                biased;
                () = cancel.cancelled() => {}
                () = tokio::time::sleep(OFFLINE_WAIT) => {}
                _ = offline => {}
            }
        }
        let close = link.request(client, json!({"@type": "close"}));
        tokio::pin!(close);
        let deadline = Instant::now() + self.timings.close_drain;
        let mut asking = true;
        loop {
            let Some(live) = self.live.as_mut() else {
                return false;
            };
            let frame = tokio::select! {
                biased;
                () = cancel.cancelled() => return false,
                () = tokio::time::sleep_until(deadline) => {
                    tracing::debug!("TDLib did not close in time; the run stays unclean");
                    return false;
                }
                answer = &mut close, if asking => {
                    asking = false;
                    if matches!(answer, Err(LinkError::Closed)) {
                        return false;
                    }
                    continue;
                }
                frame = live.helper.inbound().recv() => frame,
            };
            let json = match frame {
                Some(Inbound::Update(json)) => Zeroizing::new(json),
                Some(Inbound::Closed | Inbound::Bye(_) | Inbound::Fatal(_) | Inbound::Garbled)
                | None => return false,
                Some(_) => continue,
            };
            match tdapi::parse_update(&json) {
                Some(Update::AuthorizationState(AuthorizationState::Closed)) => return true,
                // Closing (or any other state while it closes): no restart, no logout.
                Some(Update::AuthorizationState(_)) => continue,
                _ => {}
            }
            let now = self.clock.now_ms();
            let acts = match self.session.as_mut() {
                Some(session) => session.on_inbound(&json, now),
                None => Vec::new(),
            };
            drop(json);
            for act in acts {
                match act {
                    Act::Rows(staged) => self.stage(*staged),
                    Act::Ephemeral(event) => self.emit(Envelope::ephemeral(*event)).await,
                    other => tracing::debug!(act = ?other, "an act is dropped while TDLib closes"),
                }
            }
        }
    }

    /// Stops the live helper (`kill`: at once) and empties its files directory. A helper that
    /// shows the account online is told `false` first (H6; best effort, [`OFFLINE_WAIT`], not at a
    /// kill); the files directory's size is logged as a number before it is emptied (L-9).
    async fn stop_live(&mut self, kill: bool) {
        // What the receive staged is written before the helper goes (the next run's cursor).
        self.commit().await;
        let Some(live) = self.live.take() else {
            return;
        };
        let was_online = self.presence.on_stop(self.clock.now_ms());
        let Live {
            helper, files_dir, ..
        } = live;
        if was_online && !kill {
            let link = helper.link().clone();
            let offline = link.request(helper.client, presence::online_request(false));
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => {}
                () = tokio::time::sleep(OFFLINE_WAIT) => {}
                _ = offline => {}
            }
        }
        let _report = if kill {
            let now = CancellationToken::new();
            now.cancel();
            helper.stop(&self.timings, &now).await
        } else {
            helper.stop(&self.timings, &self.cancel).await
        };
        let bytes = dir_bytes(&files_dir);
        tracing::debug!(bytes, "the TDLib files directory is emptied");
        let _ = supervisor::empty_files_dir(&files_dir);
    }

    /// `Logout` (critic H4: under 8 s in all, inside the core's 10 s window): with a live helper
    /// `logOut` and up to 4 s for TDLib's `Closed`, else `destroy` and up to 2 s; the report is
    /// sent BEFORE the helper's stop; then the helper is killed and `tdlib/` and the files
    /// directory are removed (the vault's wipe of `tdlib/` is the belt when the core's kill comes
    /// first).
    async fn logout(&mut self) -> Exit {
        let linked = self.user_id.is_some()
            || self.was_authorized
            || self.session.as_ref().is_some_and(Session::ever_authorized);
        if let Some(session) = self.session.as_mut() {
            session.logout_asked();
        }
        let outcome = match self.live.as_mut() {
            Some(live) => {
                let closed = network_logout(&mut live.helper, &self.cancel).await;
                match (linked, closed) {
                    (true, true) => DeviceLogout::Removed,
                    // Local data gone, the session stays in his list (offline: every TG-2 build).
                    (true, false) => DeviceLogout::NotReached,
                    (false, _) => DeviceLogout::NoDevice,
                }
            }
            None if linked => DeviceLogout::NotReached,
            None => DeviceLogout::NoDevice,
        };
        self.emit(Envelope::ephemeral(BackendEvent::Logout(
            LogoutReport::new(outcome),
        )))
        .await;
        self.stop_live(true).await;
        self.session = None;
        self.remove_tdlib();
        Exit::LoggedOut {
            reason: "logged out on request".to_owned(),
        }
    }

    /// Removes the account's `tdlib/` directory and its files directory (a logout).
    fn remove_tdlib(&self) {
        if let Some(dir) = self.backend_db.parent() {
            let _ = supervisor::empty_files_dir(&dir.join("tdlib"));
        }
        if let Some(root) = self.parts.files_root() {
            let _ = supervisor::empty_files_dir(&root.join(supervisor::files_dir_name(self.uuid)));
        }
    }

    async fn emit(&mut self, envelope: Envelope) {
        tokio::select! {
            () = self.cancel.cancelled() => {}
            // A core that is gone sees no event; its closed command channel ends the actor.
            _ = self.events.send(envelope) => {}
        }
    }
}

/// `logOut`, then (if TDLib did not close within [`LOGOUT_WAIT`]) `destroy` for up to
/// [`DESTROY_WAIT`]. `true`: TDLib closed after the `logOut` (the session is gone on Telegram's
/// side too). A second `logOut` answered "already logging out" counts as asked.
async fn network_logout(helper: &mut Helper, cancel: &CancellationToken) -> bool {
    let link = helper.link().clone();
    let client = helper.client;
    let asked = link.request(client, json!({"@type": "logOut"}));
    if wait_closed(helper, asked, LOGOUT_WAIT, cancel).await {
        return true;
    }
    tracing::debug!("TDLib's logout did not end in time; its local data is destroyed");
    let destroyed = link.request(client, json!({"@type": "destroy"}));
    let _ = wait_closed(helper, destroyed, DESTROY_WAIT, cancel).await;
    false
}

/// Waits up to `bound` for TDLib's `authorizationStateClosed` while `request` is answered (its
/// answer itself is not the end). `false` at the bound, at the helper's end or at a cancel.
async fn wait_closed(
    helper: &mut Helper,
    request: impl Future<Output = Result<Value, LinkError>>,
    bound: Duration,
    cancel: &CancellationToken,
) -> bool {
    let deadline = Instant::now() + bound;
    tokio::pin!(request);
    let mut asking = true;
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return false,
            () = tokio::time::sleep_until(deadline) => return false,
            answer = &mut request, if asking => {
                asking = false;
                if matches!(answer, Err(LinkError::Closed)) {
                    return false;
                }
            }
            frame = helper.inbound().recv() => match frame {
                Some(Inbound::Update(json)) => {
                    let json = Zeroizing::new(json);
                    if matches!(
                        tdapi::parse_update(&json),
                        Some(Update::AuthorizationState(AuthorizationState::Closed))
                    ) {
                        return true;
                    }
                }
                Some(Inbound::Closed | Inbound::Bye(_) | Inbound::Fatal(_) | Inbound::Garbled)
                | None => return false,
                Some(_) => {}
            },
        }
    }
}

/// The clock's sleep until `at`, if any.
fn optional_sleep(clock: &dyn Clock, at: Option<u64>) -> impl Future<Output = ()> + use<> {
    let sleep: Option<Sleep> = at.map(|at| clock.sleep_until(at));
    async move {
        match sleep {
            Some(sleep) => sleep.await,
            None => std::future::pending().await,
        }
    }
}

/// The bytes of the regular files under `dir` (not following links; an unreadable entry counts
/// 0). TDLib's files directory lives on the tmpfs media cache: its size is logged as a number at
/// every stop (L-9; T6 owns the cap).
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

/// tokio's sleep until `at`, if any (the watchdog's own times, as in `Helper::watch`).
async fn optional_instant(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The second try in a row of a database that is in use is the one the person is told of.
const IN_USE_ANSWER_AT: usize = 2;

/// What one start needs from the actor (borrowed, so the actor's command channel stays free to
/// be served while it runs).
struct StartInputs<'a> {
    parts: &'a TgParts,
    uuid: kourier_model::AccountUuid,
    backend_db: &'a Path,
    key: &'a Zeroizing<String>,
    env: TgEnv,
    timings: &'a Timings,
    net_rt: &'a tokio::runtime::Handle,
}

/// How one start ended.
enum Started {
    /// The helper runs and reached its first state; its files directory. (Boxed: the helper is
    /// the largest variant by far, clippy's `large_enum_variant`.)
    Up(Box<Helper>, PathBuf),
    /// The session's answer (a property of the build or a refusal: never retried).
    Answer(BackendError),
    /// Tried again after the back-off (`Abort` or `Transient`).
    Again(Retry),
    /// The start's token was cancelled meanwhile (the helper, if any, is killed and reaped).
    Cancelled,
}

/// Starts the account's gate and helper up to TDLib's first authorization state; the lock-down
/// word of the helper that got there. A cancelled `cancel` kills and reaps the helper at once.
async fn start_helper(
    i: StartInputs<'_>,
    cancel: &CancellationToken,
) -> (Started, Option<&'static str>) {
    let Some(api) = i.parts.api().cloned() else {
        return (Started::Answer(BackendError::unsupported(NO_API_KEY)), None);
    };
    let Some(root) = i.parts.files_root().cloned() else {
        return (
            Started::Answer(BackendError::internal(
                "this build has no place for Telegram's files",
            )),
            None,
        );
    };
    let Ok((database_dir, files_dir)) = supervisor::account_dirs(i.backend_db, &root, i.uuid)
    else {
        return (
            Started::Answer(BackendError::internal(
                "the account's TDLib directories could not be made",
            )),
            None,
        );
    };
    // His answer U-4: `Kourier (<host name>)`, from the host name the application gave.
    let device_model = i.parts.device_model();
    let account = AccountStart {
        uuid: i.uuid,
        key: i.key,
        database_dir,
        files_dir: files_dir.clone(),
        env: i.env,
        api,
        device_model: &device_model,
    };
    let started = supervisor::start(i.parts, &account, i.net_rt, i.timings, cancel).await;
    let answer = match started {
        Ok(helper) => {
            let lockdown = Some(helper.lockdown.as_str());
            if cancel.is_cancelled() {
                helper.stop(i.timings, cancel).await;
                let _ = supervisor::empty_files_dir(&files_dir);
                return (Started::Cancelled, lockdown);
            }
            tracing::debug!("the TDLib helper reached its first state");
            return (Started::Up(Box::new(helper), files_dir), lockdown);
        }
        Err(supervisor::StartError::Cancelled) => Started::Cancelled,
        Err(e) => match e.retry() {
            retry @ (Retry::Abort | Retry::Transient) => {
                tracing::debug!(class = %e, "the TDLib start failed; it is tried again");
                Started::Again(retry)
            }
            Retry::Never if e.is_missing_library() => {
                Started::Answer(BackendError::unsupported(NO_LIBRARY))
            }
            Retry::Never if e.is_wrong_helper() => {
                Started::Answer(BackendError::unsupported(WRONG_HELPER))
            }
            Retry::Never => {
                tracing::debug!(class = %e, "the TDLib start failed");
                Started::Answer(BackendError::transient(e.to_string()))
            }
        },
    };
    let _ = supervisor::empty_files_dir(&files_dir);
    if cancel.is_cancelled() {
        return (Started::Cancelled, None);
    }
    (answer, None)
}

#[cfg(test)]
mod tests;
