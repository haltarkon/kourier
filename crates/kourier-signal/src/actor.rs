//! The per-account actor (§2.5, §6.3; Desktop's `SD/ts/background.preload.ts` wiring): one OS
//! thread with a current-thread runtime and a `LocalSet`, owning the `backend.db` connection,
//! the command loop, the login task, the connection task and the `core_outbox` forwarder.
//! Network futures run on the shared runtime (`AccountContext::net_rt`, see [`Net`]).

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEvent, Clock, Command, Envelope, Exit, StartMode,
    TransmitOutcome,
};
use kourier_db::ZoneDb;
use kourier_model::{
    BackendError, ConnState, DeviceLogout, LoginStep, LoginStepKind, LogoutReport,
};
use libsignal_protocol::PublicKey;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::{JoinError, JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::Tuning;
use crate::connection::{self, ConnectionEnd, Nudge};
use crate::db::{self, Account};
use crate::error::{Error, Result};
use crate::net::{Connected, Net, SignalTransport, TransportError};
use crate::provisioning::link::{self, LinkEnd, LoginInput};
use crate::receive::stage2::{self, Lanes};
use crate::send::{self, SendContext, SendLocks, UnauthSocket};
use crate::{commands, maintenance, outbox};

/// How long `Shutdown` lets the forwarder drain `core_outbox` before the actor exits (the core
/// waits 5 s in total, §4.7).
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);

/// How long an unlinked device's forwarder may take to hand the core what it still has, before
/// the core wipes `backend.db` with it (§4.3 step 5).
const UNLINK_DRAIN: Duration = Duration::from_secs(30);

/// How long, of that, stage 2 may take to finish the rows already in the inbox.
const UNLINK_STAGE2: Duration = Duration::from_secs(10);

/// How long `Command::Logout` waits for the server to answer `DELETE /v1/devices/<id>`; the
/// core gives a logout 10 s in all (§4.3 step 7).
const LOGOUT_REQUEST: Duration = Duration::from_secs(5);

/// How long `Command::Logout` may spend on the network in all: letting a login that is still
/// running come to its end, a socket of its own when none is open, and the request. With
/// [`SHUTDOWN_DRAIN`] behind it this stays inside the core's 10 s.
const LOGOUT_NETWORK: Duration = Duration::from_secs(7);

/// How long, of that, a login that is still running gets to end where it knows what the
/// server holds (the answer to a link request that is on its way).
const LOGOUT_LOGIN_SETTLE: Duration = Duration::from_secs(4);

/// How long `Shutdown` gives a login that is still running to end and, when the server had
/// linked a device for it, to remove that device again. With [`SHUTDOWN_DRAIN`] behind it
/// this is the 5 s the core waits for a shutdown (§4.7). At the QR code nothing of it is
/// used: the login ends at once.
const SHUTDOWN_LOGIN: Duration = Duration::from_secs(3);

/// State shared by the actor's tasks (all on the actor thread, hence `Rc`/`RefCell`).
pub(crate) struct Shared<T: SignalTransport> {
    /// The transport and the network runtime.
    pub(crate) net: Net<T>,
    /// `backend.db`.
    pub(crate) db: Rc<ZoneDb>,
    /// Envelopes to the core (forwarder and ephemeral events).
    pub(crate) events: mpsc::Sender<Envelope>,
    /// Time source.
    pub(crate) clock: Arc<dyn Clock>,
    /// Runtime configuration.
    pub(crate) config: BackendConfig,
    /// Sealed-sender trust roots of the environment.
    pub(crate) trust_roots: Vec<PublicKey>,
    /// The environment's zkgroup `ServerPublicParams` (group credentials, notary signatures,
    /// endorsements); `None` when they do not parse.
    pub(crate) zk_params: Option<zkgroup::ServerPublicParams>,
    /// Wakes the forwarder after a commit that wrote `core_outbox` rows.
    pub(crate) wake: Rc<Notify>,
    /// The last `core_outbox` seq the core acked.
    pub(crate) acked: watch::Receiver<i64>,
    /// The linked account, once there is one.
    pub(crate) account: RefCell<Option<Account>>,
    /// The authenticated socket while connected.
    pub(crate) socket: RefCell<Option<Arc<T::Socket>>>,
    /// A login linked the account and still owes the core its `Done` step (§6.5 step 8).
    pub(crate) login_pending: Cell<bool>,
    /// Wakes an idle job runner when jobs are added while it sleeps (§6.12). Every job of S6 is
    /// added before the runner starts; command jobs (receipts, read syncs) and stage 2 (delivery
    /// receipts) notify it.
    pub(crate) jobs_wake: Rc<Notify>,
    /// Timings tests may shorten (QR rotation, keepalive, clock recheck).
    pub(crate) tuning: Tuning,
    /// Stage 2: lanes, the catch-up boundary, progress (§6.6).
    pub(crate) lanes: Lanes,
    /// Sends to one recipient run one at a time (§13 phase D).
    pub(crate) send_locks: SendLocks,
    /// The unauthenticated socket for sealed sends and profile fetches (§6.4).
    pub(crate) unauth: UnauthSocket<T>,
    /// `accounts/<uuid>/blobs`: avatars go here through `kourier-blob` (§5.8).
    pub(crate) blob_dir: PathBuf,
    /// The history transfer of a fresh link: its directory, whether it holds receiving back,
    /// the user's skip (`transfer.rs`).
    pub(crate) transfer: crate::transfer::TransferShared,
    /// Whether the work of a new link is reported to the core, and what was reported
    /// (`progress.rs`, `BackendEvent::AccountProgress`).
    pub(crate) settling: crate::progress::Settling,
    /// Test hook: a blocking delay per stage-1 batch (a slow decrypt).
    #[cfg(test)]
    pub(crate) stage1_delay: Cell<Duration>,
    /// The token of the newest send challenge the server set (428, §6.4), until its answer
    /// was accepted. The core hands the token of the attempt it paused on back with the answer;
    /// this one is newer when the server asked again meanwhile.
    pub(crate) challenge: RefCell<Option<kourier_model::Redacted<String>>>,
    /// The send challenge a job met and announced to the core, while it stands
    /// ([`crate::challenge::announce`]): further jobs and tries that meet it announce nothing,
    /// and jobs that would meet it are held. Forgotten when its answer was accepted and at
    /// a fresh `Online` that ends the core's own notice ([`Self::forget_job_challenge`], §4.8);
    /// lost with a restart, whose first `Online` releases what was held for it.
    pub(crate) job_challenge: Cell<Option<crate::challenge::Standing>>,
    /// This device is no longer linked: stage 2 and the jobs still running start no network
    /// work (§4.3 step 5).
    pub(crate) unlinking: Cell<bool>,
    /// The server stated its time on the connection that stands and this computer's clock was
    /// within the tolerance (`connection`, `ChatEvent::ServerTimestamp`); `false` before
    /// any connection, from a statement the clock failed and from any state of the
    /// connection other than syncing or online (`set_conn`). The delivery of a history
    /// import drops a row that is due only while this is set (`transfer/deliver.rs`).
    pub(crate) clock_confirmed: Cell<bool>,
    /// The last timestamp [`Self::unique_timestamp`] handed out.
    last_timestamp: Cell<u64>,
    /// Connection state for the core.
    conn: watch::Sender<ConnState>,
    /// Why this device counts as unlinked, set by whoever finds out outside the connection
    /// task (the refusal of our PNI prekeys); the connection task ends on it.
    unlinked: watch::Sender<Option<String>>,
}

/// What [`Shared::new`] needs.
pub(crate) struct SharedParts<T: SignalTransport> {
    pub(crate) net: Net<T>,
    pub(crate) db: Rc<ZoneDb>,
    pub(crate) events: mpsc::Sender<Envelope>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) config: BackendConfig,
    pub(crate) trust_roots: Vec<PublicKey>,
    pub(crate) zk_params: Vec<u8>,
    pub(crate) wake: Rc<Notify>,
    pub(crate) acked: watch::Receiver<i64>,
    pub(crate) account: Option<Account>,
    pub(crate) tuning: Tuning,
    pub(crate) conn: watch::Sender<ConnState>,
    pub(crate) blob_dir: PathBuf,
    pub(crate) transfer: crate::transfer::TransferShared,
}

impl<T: SignalTransport> Shared<T> {
    /// The shared state of a starting actor.
    pub(crate) fn new(parts: SharedParts<T>) -> Self {
        let SharedParts {
            net,
            db,
            events,
            clock,
            config,
            trust_roots,
            zk_params,
            wake,
            acked,
            account,
            tuning,
            conn,
            blob_dir,
            transfer,
        } = parts;
        let zk_params = zkgroup::deserialize::<zkgroup::ServerPublicParams>(&zk_params).ok();
        if zk_params.is_none() {
            tracing::warn!("the environment's zkgroup params do not parse; groups stay unread");
        }
        Self {
            net,
            db,
            events,
            clock,
            config,
            trust_roots,
            zk_params,
            wake,
            acked,
            account: RefCell::new(account),
            socket: RefCell::new(None),
            login_pending: Cell::new(false),
            jobs_wake: Rc::new(Notify::new()),
            tuning,
            lanes: Lanes::production(),
            send_locks: SendLocks::default(),
            unauth: UnauthSocket::default(),
            blob_dir,
            transfer,
            settling: crate::progress::Settling::default(),
            #[cfg(test)]
            stage1_delay: Cell::new(Duration::ZERO),
            challenge: RefCell::new(None),
            job_challenge: Cell::new(None),
            unlinking: Cell::new(false),
            clock_confirmed: Cell::new(false),
            last_timestamp: Cell::new(0),
            conn,
            unlinked: watch::Sender::new(None),
        }
    }

    /// Reports that this device is no longer linked (`reason` is log-safe); the connection
    /// task ends with it and the actor runs the unlink procedure (§4.3 step 5).
    pub(crate) fn unlink(&self, reason: &str) {
        tracing::warn!(reason, "this device is no longer linked");
        self.unlinked.send_replace(Some(reason.to_owned()));
    }

    /// Sees [`Self::unlink`].
    pub(crate) fn unlinked(&self) -> watch::Receiver<Option<String>> {
        self.unlinked.subscribe()
    }

    /// Remembers the challenge of a send the server refused with 428, so that its answer can
    /// name it ([`crate::challenge`]).
    pub(crate) fn note_challenge(&self, outcome: &TransmitOutcome) {
        use kourier_model::ErrorKind;
        let token = match outcome {
            TransmitOutcome::Failed(error) => match &error.kind {
                ErrorKind::Challenge { token, .. } => Some(token),
                _ => None,
            },
            TransmitOutcome::Partial { failed, .. } => {
                failed.iter().find_map(|(_, error)| match &error.kind {
                    ErrorKind::Challenge { token, .. } => Some(token),
                    _ => None,
                })
            }
            TransmitOutcome::Sent { .. } => None,
        };
        if let Some(token) = token.filter(|token| !token.expose().is_empty()) {
            *self.challenge.borrow_mut() = Some(token.clone());
        }
    }

    /// Sends an ephemeral event (login steps) to the core.
    pub(crate) async fn emit(&self, event: BackendEvent) -> Result<()> {
        self.events
            .send(Envelope::ephemeral(event))
            .await
            .map_err(|_| Error::CoreGone)
    }

    /// Milliseconds since the epoch.
    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// [`Self::now_ms`] as a column value.
    pub(crate) fn now(&self) -> i64 {
        i64::try_from(self.now_ms()).unwrap_or(i64::MAX)
    }

    /// A send timestamp no earlier than now and later than every one handed out before, for
    /// messages of our own that need distinct timestamps (sync requests sent back to back).
    pub(crate) fn unique_timestamp(&self) -> u64 {
        let timestamp = self
            .now_ms()
            .max(self.last_timestamp.get().saturating_add(1));
        self.last_timestamp.set(timestamp);
        timestamp
    }

    /// A copy of the account, if linked.
    pub(crate) fn account(&self) -> Option<Account> {
        self.account.borrow().clone()
    }

    /// What a send on the authenticated `socket` needs.
    pub(crate) fn send_context<'a>(
        &'a self,
        account: &'a Account,
        socket: &'a Arc<T::Socket>,
    ) -> SendContext<'a, T> {
        SendContext {
            net: &self.net,
            socket,
            unauth: &self.unauth,
            db: &self.db,
            account,
            locks: &self.send_locks,
            trust_roots: &self.trust_roots,
            clock: &*self.clock,
            jobs_wake: &self.jobs_wake,
            outbox_wake: &self.wake,
        }
    }

    /// The account is `Online` (through its catch-up).
    pub(crate) fn is_online(&self) -> bool {
        self.conn.borrow().is_online()
    }

    /// Sees every change of the connection state (a socket that opened or closed).
    pub(crate) fn conn_watch(&self) -> watch::Receiver<ConnState> {
        self.conn.subscribe()
    }

    /// Publishes a connection state (log-safe: only the variant name is logged).
    pub(crate) fn set_conn(&self, state: ConnState) {
        if *self.conn.borrow() != state {
            tracing::info!(state = state.name(), "connection state");
            if !matches!(state, ConnState::Syncing { .. } | ConnState::Online { .. }) {
                // A connection that is gone confirms nothing: the clock may change before
                // the next one (a suspend), and the server's next statement confirms it.
                self.clock_confirmed.set(false);
            }
            if state.is_online() {
                // A challenge announced during the catch-up that ends here is on show in the
                // core from now on; every other one ends for the core with this `Online`.
                match self
                    .job_challenge
                    .get()
                    .and_then(crate::challenge::Standing::at_online)
                {
                    Some(standing) => self.job_challenge.set(Some(standing)),
                    None => self.forget_job_challenge(),
                }
            }
            self.conn.send_replace(state);
        }
    }

    /// The send challenge a job announced has ended without an answer, as far as anyone can
    /// tell: the core ended its notice at this fresh `Online` (§4.8), or the actor was
    /// restarted and neither the standing challenge nor its token were kept. The jobs held
    /// for it in `backend.db` get their own times back, so that the hold ends with the
    /// notice: the first of them asks the server once, and a challenge that still stands is
    /// announced again with its new token and holds the others again
    /// ([`crate::challenge`]).
    pub(crate) fn forget_job_challenge(&self) {
        self.job_challenge.set(None);
        let now = self.now();
        match self.db.zone(|tx| crate::jobs::release_challenged(tx, now)) {
            Ok(0) => {}
            Ok(released) => {
                tracing::info!(released, "jobs held for a send challenge are tried again");
            }
            Err(e) => {
                tracing::warn!(error = %e, "releasing the jobs held for a send challenge failed");
            }
        }
    }

    /// Until when (ms) jobs that send to other accounts wait for the send challenge a job
    /// met, if it still stands at `now`.
    pub(crate) fn challenge_until(&self, now: i64) -> Option<i64> {
        self.job_challenge
            .get()
            .map(|standing| standing.until)
            .filter(|until| *until > now)
    }

    /// Waits until the core acked every `core_outbox` row up to `seq` (rows are deleted only
    /// after the ack), or the forwarder is gone.
    pub(crate) async fn wait_acked(&self, seq: i64) {
        let mut acked = self.acked.clone();
        loop {
            let waiting = self.db.read(|c| -> Result<i64> {
                Ok(c.query_row(
                    "SELECT count(*) FROM core_outbox WHERE seq <= ?1",
                    [seq],
                    |r| r.get(0),
                )?)
            });
            match waiting {
                Ok(0) => return,
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, "reading core_outbox failed");
                    return;
                }
            }
            if acked.changed().await.is_err() {
                return;
            }
        }
    }
}

/// How a login task ended.
type LoginResult = std::result::Result<Account, LinkEnd>;

/// A running login flow.
struct Login {
    inputs: mpsc::UnboundedSender<LoginInput>,
    task: JoinHandle<LoginResult>,
    /// Asks the task to end (`CancelLogin`, `Logout`, `Shutdown`). It is never aborted while
    /// its link request may be on its way, unless that request stays unanswered for too long
    /// ([`Actor::settle_login`]): only the answer names the device the server linked, and a
    /// device nobody knows the id and password of could never be removed again
    /// ([`crate::provisioning::link`]).
    stop: CancellationToken,
}

/// A running connection task.
struct Connection {
    stop: CancellationToken,
    task: JoinHandle<ConnectionEnd>,
    /// Hints for the task: connect now, check the socket.
    nudges: mpsc::UnboundedSender<Nudge>,
}

/// The running history transfer of a fresh link (`transfer::run`).
struct Transfer {
    /// Stops it where it is; the stored state stays, the next start goes on from it.
    stop: CancellationToken,
    task: JoinHandle<()>,
}

/// What woke the command loop.
enum Next {
    Cancelled,
    Command(Option<Command>),
    Login(std::result::Result<LoginResult, JoinError>),
    Connection(std::result::Result<ConnectionEnd, JoinError>),
}

/// The actor: the command loop and its tasks.
struct Actor<T: SignalTransport> {
    shared: Rc<Shared<T>>,
    commands: mpsc::Receiver<Command>,
    cancel: CancellationToken,
    login: Option<Login>,
    connection: Option<Connection>,
    /// The connection ended on something only the user can lift (4409, an expired version, a
    /// `Disconnect`): system hints do not reconnect, `Command::Connect` does (§6.4).
    held: bool,
    /// The history transfer, while this actor has run one.
    transfer: Option<Transfer>,
    /// The start found a transfer that is over and could not truncate the WAL at once.
    scrub_owed: bool,
    forwarder: JoinHandle<()>,
    forwarder_stop: CancellationToken,
    stage2: JoinHandle<()>,
    stage2_stop: CancellationToken,
}

/// The actor thread's body: builds the runtime, opens `backend.db` and runs the command loop
/// until shutdown, logout or a fatal error.
pub(crate) fn run<T: SignalTransport>(
    ctx: AccountContext,
    transport: Arc<T>,
    trust_roots: Vec<PublicKey>,
    zk_params: Vec<u8>,
    tuning: Tuning,
) -> Exit {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            return Exit::Failed(BackendError::internal(format!(
                "building the account runtime: {e}"
            )));
        }
    };
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        match Actor::start(ctx, transport, trust_roots, zk_params, tuning) {
            Ok(actor) => actor.run().await,
            Err(e) => {
                tracing::error!(error = %e, "account actor failed to start");
                Exit::Failed(e.to_backend_error())
            }
        }
    })
}

/// `Option<JoinHandle>` as a future that never completes when there is no task.
async fn join_opt<R>(task: Option<&mut JoinHandle<R>>) -> std::result::Result<R, JoinError> {
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

impl<T: SignalTransport> Actor<T> {
    fn start(
        ctx: AccountContext,
        transport: Arc<T>,
        trust_roots: Vec<PublicKey>,
        zk_params: Vec<u8>,
        tuning: Tuning,
    ) -> Result<Self> {
        let AccountContext {
            account: _,
            uuid,
            start,
            backend_db,
            db_key,
            blob_dir,
            net_rt,
            commands,
            events,
            conn,
            ui: _,
            local_prefs: _,
            cancel,
            clock,
            config,
        } = ctx;
        let db = Rc::new(db::open(&backend_db, &db_key)?);
        let account = db.read(Account::load)?;
        if start == StartMode::Fresh && account.is_some() {
            return Err(Error::Internal("a fresh start found an existing account"));
        }
        // A linked account talks only to the server that linked it (§6.5 step 6): checked
        // before any job is queued and long before anything connects.
        if let Some(bound) = db.read(db::bound_env)? {
            crate::net::env::check_binding(bound.as_deref(), &config.env)?;
        }
        tracing::info!(
            account = uuid.short(),
            ?start,
            linked = account.is_some(),
            "account actor starting"
        );
        // The history transfer of a fresh link (§14.2 item 14): one that is unfinished goes
        // on and holds receiving back; anything else leaves no archive file behind.
        let transfer_dir = crate::transfer::dir_beside(&backend_db);
        if start == StartMode::Rebuild {
            // A rebuilt core.db has nothing of an import that was being delivered: the
            // record of its stage is taken back and the archive is read again, so the new
            // core gets the whole import and not its tail (`transfer/deliver.rs`).
            let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
            if db.zone(|tx| crate::transfer::deliver::rebuild(tx, now))? {
                tracing::info!("the history import starts over for the rebuilt core");
            }
        }
        let stored_transfer = db.read(crate::transfer::load)?;
        // A transfer that is over holds no key in its row, but a process that was killed
        // between that write and its checkpoint left the key in the WAL: truncated here,
        // before anything else is written (`transfer.rs`, "Secrets"). A reader that holds
        // it back now leaves it to the task `run` starts.
        let scrub_owed = stored_transfer
            .as_ref()
            .is_some_and(|stored| stored.phase.is_over())
            && !crate::transfer::checkpoint(&db);
        crate::transfer::clean_at_start(&transfer_dir, stored_transfer.as_ref());
        let transfer_active = account.is_some()
            && stored_transfer
                .as_ref()
                .is_some_and(|stored| !stored.phase.is_over());
        if transfer_active {
            tracing::info!(state = ?stored_transfer.as_ref().map(|s| s.phase), "history transfer unfinished: it goes on");
        }
        let wake = Rc::new(Notify::new());
        if let Some(account) = &account {
            let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
            // Capabilities at every start of a linked account (§6.12).
            db.zone(|tx| maintenance::enqueue_at_start(tx, now))?;
            if start == StartMode::Rebuild {
                // A rebuilt core.db learns the account and its limits again (§4.3 step 8),
                // then contacts and settings through fresh syncs (S9); groups join in S10.
                maintenance::rebuild_events(&db, account, &blob_dir, now, &wake)?;
            } else {
                // Features this version computes differently (S11 group send, S12 files).
                db.zone(|tx| -> Result<()> {
                    // What the account can do, for the core, at every start
                    // (`append_account_caps`; a rebuild states it with `AccountReady`).
                    maintenance::append_account_caps(tx, now, &wake)?;
                    let events = crate::features::refresh_features(tx, account, now)?;
                    if !events.is_empty() {
                        outbox::append_events(tx, &events, now, &wake)?;
                    }
                    Ok(())
                })?;
            }
        }
        let (acked_tx, acked) = watch::channel(0);
        let forwarder_stop = cancel.child_token();
        let forwarder = spawn_local(outbox::forward(
            db.clone(),
            events.clone(),
            wake.clone(),
            acked_tx,
            forwarder_stop.clone(),
        ));
        // The stored remote config steers the transport from the first connect on (§6.4).
        if account.is_some() {
            transport.remote_config_changed(&db.read(maintenance::load_remote_config)?);
        }
        let shared = Rc::new(Shared::new(SharedParts {
            net: Net::new(transport, net_rt),
            db,
            events,
            clock,
            config,
            trust_roots,
            zk_params,
            wake,
            acked,
            account,
            tuning,
            conn,
            blob_dir,
            transfer: crate::transfer::TransferShared::new(transfer_dir, transfer_active),
        }));
        // Rows a previous run committed in stage 1 but did not finish (§5.6 "restart") are
        // picked up by the stage-2 driver right away, before and while connecting.
        let stage2_stop = cancel.child_token();
        let stage2 = spawn_local(stage2::drive(shared.clone(), stage2_stop.clone()));
        let mut actor = Self {
            shared,
            commands,
            cancel,
            login: None,
            connection: None,
            held: false,
            transfer: None,
            scrub_owed,
            forwarder,
            forwarder_stop,
            stage2,
            stage2_stop,
        };
        if transfer_active {
            actor.start_transfer();
        }
        // A link's tail cut off by a quit: its groups still load, and the UI shows how many.
        // Only within a day of the link: an old account's stranded group is no news.
        let now = actor.shared.now();
        match actor
            .shared
            .db
            .read(|conn| crate::progress::groups_loading(conn, now))
        {
            Ok(true) => actor.shared.settling.begin(),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = %e, "reading the groups still loading failed"),
        }
        Ok(actor)
    }

    /// Starts the task that runs the stored history transfer to its end. Its long-poll
    /// waits for the authenticated socket; its download and its read need none.
    fn start_transfer(&mut self) {
        if self.transfer.is_some() {
            return;
        }
        let stop = self.cancel.child_token();
        let task = spawn_local(crate::transfer::run(self.shared.clone(), stop.clone()));
        self.transfer = Some(Transfer { stop, task });
        self.shared.settling.begin();
    }

    /// Stops the history transfer where it is (the stored state stays) and waits for its
    /// task.
    async fn stop_transfer(&mut self) {
        if let Some(transfer) = self.transfer.take() {
            transfer.stop.cancel();
            let _ = transfer.task.await;
        }
    }

    /// `Command::SkipHistoryTransfer`: ends a transfer that waits for the phone or downloads.
    fn skip_transfer(&self) {
        let skippable = self.shared.db.read(crate::transfer::load).map(|stored| {
            stored.is_some_and(|stored| {
                matches!(
                    stored.phase,
                    crate::transfer::Phase::Waiting | crate::transfer::Phase::Downloading
                )
            })
        });
        match skippable {
            Ok(true) if self.transfer.is_some() => {
                tracing::info!("the user skips the history transfer");
                self.shared.transfer.skip();
            }
            Ok(_) => tracing::debug!("no history transfer to skip"),
            Err(e) => tracing::warn!(error = %e, "reading the history transfer failed"),
        }
    }

    async fn run(mut self) -> Exit {
        if std::mem::take(&mut self.scrub_owed) {
            let shared = self.shared.clone();
            spawn_local(async move { crate::transfer::scrub(&shared).await });
        }
        // A transfer that is over is said again at every start, until it is dismissed: the
        // core keeps none of it, and a UI that was not there when it ended must learn how
        // it ended. One that is not over is reported by its task.
        if !self.shared.transfer.holds() {
            crate::transfer::report_stored(&self.shared).await;
        }
        loop {
            let next = tokio::select! {
                () = self.cancel.cancelled() => Next::Cancelled,
                command = self.commands.recv() => Next::Command(command),
                done = join_opt(self.login.as_mut().map(|l| &mut l.task)) => Next::Login(done),
                end = join_opt(self.connection.as_mut().map(|c| &mut c.task)) => Next::Connection(end),
            };
            match next {
                Next::Cancelled | Next::Command(None) => {
                    return self.shutdown(Exit::Shutdown).await;
                }
                Next::Command(Some(command)) => {
                    if let Some(exit) = self.command(command).await {
                        return exit;
                    }
                }
                Next::Login(done) => {
                    let stopped = self
                        .login
                        .take()
                        .is_some_and(|login| login.stop.is_cancelled());
                    self.login_finished(done, stopped).await;
                }
                Next::Connection(end) => {
                    self.connection = None;
                    match end {
                        Ok(ConnectionEnd::Stopped) => {}
                        Ok(ConnectionEnd::Held) => self.held = true,
                        Ok(ConnectionEnd::LoggedOut(reason)) => {
                            return self.unlinked(reason, UNLINK_DRAIN).await;
                        }
                        Ok(ConnectionEnd::Failed(error)) => {
                            return self.shutdown(Exit::Failed(error)).await;
                        }
                        Err(e) => {
                            let exit = Exit::Panicked(format!("connection task: {e}"));
                            return self.shutdown(exit).await;
                        }
                    }
                }
            }
        }
    }

    /// Handles one command; `Some(exit)` ends the actor.
    async fn command(&mut self, command: Command) -> Option<Exit> {
        tracing::debug!(command = command.name(), "command");
        match command {
            Command::Shutdown => return Some(self.shutdown(Exit::Shutdown).await),
            Command::Connect => {
                // The user's (or the core's) explicit wish: also after 4409 (§6.4).
                self.held = false;
                self.nudge(Nudge::Connect);
                // Sent after the first UI frame: a UI that came up later learns where a
                // history transfer stands, also one that is over (until it is dismissed).
                crate::transfer::report_stored(&self.shared).await;
            }
            Command::SkipHistoryTransfer => self.skip_transfer(),
            Command::DismissHistoryTransfer => crate::transfer::dismiss_stored(&self.shared),
            Command::NetworkChanged => self.hint(Nudge::NetworkChanged),
            Command::Resumed => self.hint(Nudge::Resumed),
            Command::Disconnect => self.disconnect().await,
            Command::StartLogin {
                flow,
                relink,
                taken,
            } => self.start_login(&flow, relink, taken).await,
            Command::SubmitLoginInput { txn, values } => match &self.login {
                Some(login) => {
                    let _ = login.inputs.send(LoginInput { txn, values });
                }
                None => tracing::debug!("login input without a login"),
            },
            Command::CancelLogin => {
                // The task ends by itself, at once unless its link request is on its way
                // (`Login::stop`); the loop takes its end, and the `Logout` the core sends
                // right behind removes a device that was linked all the same.
                if let Some(login) = &self.login {
                    login.stop.cancel();
                    tracing::info!("login cancelled");
                }
            }
            Command::Transmit { local, req, reply } => self.transmit(local, *req, reply),
            Command::Logout => return Some(self.logout().await),
            Command::ChangeConversation {
                conv,
                change: kourier_model::ConversationChange::SetTimer { timer },
                id,
                reply,
            } => {
                let _ = reply.send(self.set_timer(&conv, timer, id.as_ref()));
            }
            Command::ChangeConversation { reply, .. }
            | Command::ChangeContact { reply, .. }
            | Command::ChangeAccountSettings { reply, .. }
            | Command::ChangeProfile { reply, .. } => {
                let _ = reply.send(Err(BackendError::unsupported(
                    "Storage Service writes are not in M1",
                )));
            }
            Command::Resolve { reply, .. } => {
                let _ = reply.send(Err(BackendError::unsupported("resolve arrives later")));
            }
            Command::ListOwnDevices { reply } => {
                let _ = reply.send(Err(BackendError::unsupported("device list arrives later")));
            }
            Command::SendReceipts {
                cmd,
                conv,
                targets,
                kind,
            } => self.durable(cmd, |tx, account, now, wake| {
                commands::persist_receipts(tx, account, cmd, &conv, &targets, kind, now, wake)
            }),
            Command::SyncReadMarker {
                cmd, up_to, read, ..
            } => self.durable(cmd, |tx, _, now, wake| {
                commands::persist_read_sync(tx, &up_to, &read, cmd, now, wake)
            }),
            Command::SetTyping { conv, on } => {
                spawn_local(commands::send_typing(self.shared.clone(), conv, on));
            }
            Command::ConfirmIdentity { user } => {
                if let Ok(crate::service_id::SignalUser::Aci(aci)) =
                    crate::service_id::parse_user_id(&user)
                    && let Err(e) = self
                        .shared
                        .db
                        .zone(|tx| commands::confirm_identity(tx, aci))
                {
                    tracing::error!(error = %e, "confirming an identity failed");
                }
            }
            Command::DownloadAttachment {
                attachment,
                request,
                remote,
                dest,
                net_ids,
            } => {
                // The names check a backfilled pointer without a digest by the plaintext
                // hash the attachment's names state (`Pointer::from_blob_named`).
                spawn_local(crate::attachment::run_command(
                    self.shared.clone(),
                    attachment,
                    request,
                    remote,
                    dest,
                    net_ids.into_inner(),
                ));
            }
            Command::RefreshContact { user } => {
                // A profile fetch for one contact (§6.10), through the job queue.
                if let Ok(crate::service_id::SignalUser::Aci(aci)) =
                    crate::service_id::parse_user_id(&user)
                {
                    let sid = libsignal_core::ServiceId::from(aci).service_id_string();
                    let now = self.shared.now();
                    match self
                        .shared
                        .db
                        .zone(|tx| crate::profile::request(tx, &sid, now))
                    {
                        Ok(_) => self.shared.jobs_wake.notify_one(),
                        Err(e) => tracing::error!(error = %e, "queueing a profile fetch failed"),
                    }
                }
            }
            Command::ClearConversation {
                cmd,
                conv,
                newest,
                newest_non_expiring,
                remove,
            } => self.durable(cmd, |tx, account, now, wake| {
                crate::delete_sync::persist_clear(
                    tx,
                    account,
                    cmd,
                    &conv,
                    &newest,
                    &newest_non_expiring,
                    remove,
                    now,
                    wake,
                )
            }),
            Command::DeleteAttachment {
                cmd,
                conv,
                target,
                index: _,
                net_ids,
            } => self.durable(cmd, |tx, account, now, wake| {
                crate::delete_sync::persist_attachment(
                    tx,
                    account,
                    cmd,
                    &conv,
                    &target,
                    net_ids.expose(),
                    now,
                    wake,
                )
            }),
            Command::RequestAttachments { cmd, conv, target } => {
                self.durable(cmd, |tx, account, now, wake| {
                    crate::backfill::persist(tx, account, cmd, &conv, &target, now, wake)
                });
            }
            other @ Command::FetchHistory { .. } => {
                tracing::debug!(command = other.name(), "command not handled yet");
            }
            Command::SubmitChallenge {
                token,
                answer,
                reply,
            } => {
                let shared = self.shared.clone();
                spawn_local(async move {
                    // The core may have stopped waiting; the answer went out either way.
                    let _ = reply.send(crate::challenge::submit(&shared, token, &answer).await);
                });
            }
        }
        None
    }

    /// Persists a durable command's job and its `CommandPersisted` in one zone (§4.8), then
    /// wakes the job runner. Without a linked account nothing is confirmed: the core resends the
    /// command at its next start.
    fn durable(
        &self,
        cmd: kourier_model::CmdId,
        persist: impl FnOnce(&mut kourier_db::Tx<'_>, &Account, i64, &Rc<Notify>) -> Result<bool>,
    ) {
        let Some(account) = self.shared.account() else {
            tracing::debug!(cmd = cmd.0, "durable command before linking ignored");
            return;
        };
        let now = self.shared.now();
        let wake = self.shared.wake.clone();
        match self.shared.db.zone(|tx| persist(tx, &account, now, &wake)) {
            Ok(added) => {
                kourier_db::failpoint::hit(commands::FAILPOINT_AFTER_COMMAND_PERSISTED);
                if added {
                    self.shared.jobs_wake.notify_one();
                }
            }
            Err(e) => tracing::error!(cmd = cmd.0, error = %e, "persisting a command failed"),
        }
    }

    /// `ConversationChange::SetTimer`: a 1:1 chat's disappearing-message timer
    /// ([`crate::timer::change`]). A group's timer is part of the group state, which Kourier
    /// cannot write in M1 (§6.8).
    fn set_timer(
        &self,
        conv: &kourier_model::ConvKey,
        timer: Option<Duration>,
        id: Option<&kourier_backend_api::AllocatedId>,
    ) -> std::result::Result<(), BackendError> {
        let kourier_model::ConvKey::Direct(user) = conv else {
            return Err(
                BackendError::unsupported("group timers need group changes (not in M1)")
                    .with_user_message("Change this group's timer on your phone."),
            );
        };
        let account = self
            .shared
            .account()
            .ok_or_else(|| BackendError::unsupported("not linked"))?;
        let now = self.shared.now();
        let wake = self.shared.wake.clone();
        let queued = self
            .shared
            .db
            .zone(|tx| crate::timer::change(tx, &account, user, timer, id, now, &wake))
            .map_err(crate::timer::ChangeError::into_backend)?;
        if queued {
            self.shared.jobs_wake.notify_one();
        }
        Ok(())
    }

    fn connect(&mut self) {
        if self.connection.is_some() {
            return;
        }
        if self.shared.account.borrow().is_none() {
            tracing::debug!("connect before linking ignored");
            return;
        }
        let stop = self.cancel.child_token();
        let (nudges, hints) = mpsc::unbounded_channel();
        let task = spawn_local(connection::run(self.shared.clone(), stop.clone(), hints));
        self.connection = Some(Connection { stop, task, nudges });
    }

    /// Passes `nudge` to the running connection task, or starts one.
    fn nudge(&mut self, nudge: Nudge) {
        match &self.connection {
            // A closed channel is a task that just ended; the loop picks its end up next.
            Some(connection) => drop(connection.nudges.send(nudge)),
            None => self.connect(),
        }
    }

    /// A hint from the system (§4.7): the network changed or the machine woke up. Routes and
    /// DNS answers may be others now, both sockets may be dead without knowing it; a connection
    /// that waits for the user stays as it is.
    fn hint(&mut self, nudge: Nudge) {
        self.shared.net.transport().network_changed();
        // Opened lazily again, on the network as it is now.
        self.shared.unauth.reset(&self.shared.net);
        if self.held {
            tracing::debug!(?nudge, "the connection waits for the user; hint ignored");
            return;
        }
        self.nudge(nudge);
    }

    async fn disconnect(&mut self) {
        // Stays disconnected until `Connect`, whatever the system hints.
        self.held = true;
        if let Some(connection) = self.connection.take() {
            connection.stop.cancel();
            let _ = connection.task.await;
        }
        self.shared
            .set_conn(ConnState::Offline { retry_at_ms: None });
    }

    /// `Command::Logout` (§4.3 steps 2 and 7): the user removes the account, or the core turns
    /// away an actor whose login it cancelled or refused. Tells the server to forget this
    /// device (`DELETE /v1/devices/<id>`, Desktop's `unlink`), best effort, then stops like a
    /// device that was unlinked from the phone. The core deletes the account next, so nothing
    /// is waited for longer than its 10 s allow.
    ///
    /// Which device there is to forget:
    /// - a login still running is asked to stop and awaited ([`LOGOUT_LOGIN_SETTLE`]): at the
    ///   QR code it ends at once and nothing was registered; with its link request on the way
    ///   it ends with the device the server linked;
    /// - a login that finished left the account, whether or not the authenticated socket is up
    ///   yet (the request then goes out on a socket opened for it);
    /// - no login and no account: there is no device, and nothing to tell the server.
    async fn logout(&mut self) -> Exit {
        let now = tokio::time::Instant::now();
        let deadline = now + LOGOUT_NETWORK;
        let device = match self.settle_login(now + LOGOUT_LOGIN_SETTLE).await {
            Some(Ok(Ok(account))) => Some(account),
            Some(Ok(Err(LinkEnd::Unwanted { device, .. }))) => Some(*device),
            Some(Ok(Err(LinkEnd::Stopped | LinkEnd::Failed(_)))) | None => None,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "logout: the login task failed");
                None
            }
        };
        // Read before the device goes: the wipe that follows takes the row with it. A device
        // that was never stored (a login stopped at its answer) has no name here.
        let device_name = self
            .shared
            .db
            .read(|c| -> Result<Option<String>> {
                use kourier_db::rusqlite::OptionalExtension as _;
                Ok(
                    c.query_row("SELECT device_name FROM account WHERE id = 1", [], |r| {
                        r.get::<_, Option<String>>(0)
                    })
                    .optional()?
                    .flatten(),
                )
            })
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "logout: the device name could not be read");
                None
            });
        let outcome = match device.or_else(|| self.shared.account()) {
            Some(account) => self.remove_device(&account, deadline).await,
            None => {
                tracing::info!("logout before a device was linked: nothing to remove");
                DeviceLogout::NoDevice
            }
        };
        // The core tells the user, once, whether the phone still lists the device. A core
        // that is gone has nobody to tell.
        let _ = self
            .shared
            .emit(BackendEvent::Logout(LogoutReport {
                outcome,
                device_name,
            }))
            .await;
        self.unlinked("logged out on request".to_owned(), SHUTDOWN_DRAIN)
            .await
    }

    /// Asks the login that is still running, if there is one, to stop and waits for its end
    /// until `deadline`: at the QR code it ends at once, and a link request that is on its way
    /// is answered first ([`Login::stop`]). `None`: no login was running, or its link request
    /// was still unanswered at `deadline` and the task was dropped; a device the server links
    /// for that request stays on the account, because nobody will learn its id (warned).
    async fn settle_login(
        &mut self,
        deadline: tokio::time::Instant,
    ) -> Option<std::result::Result<LoginResult, JoinError>> {
        let mut login = self.login.take()?;
        login.stop.cancel();
        match tokio::time::timeout_at(deadline, &mut login.task).await {
            Ok(done) => Some(done),
            Err(_) => {
                login.task.abort();
                tracing::warn!(
                    "a link request was still unanswered when its login had to end; if a new \
                     device shows on the phone, unlink it there"
                );
                None
            }
        }
    }

    /// Tells the server to forget the device of `account` (`DELETE /v1/devices/<id>`), best
    /// effort and done by `deadline`: on the authenticated socket when one is open, else on
    /// one opened for this request. A link that finished a moment ago has no socket yet, an
    /// account that is offline or held has none either, and a device that stays linked with
    /// nobody behind it can only be removed on the phone.
    async fn remove_device(
        &mut self,
        account: &Account,
        deadline: tokio::time::Instant,
    ) -> DeviceLogout {
        let open = self.shared.socket.borrow().clone();
        // `own` is the event stream of a socket opened here: kept until the request is
        // answered, and the sign that the socket is ours to close.
        let (socket, own) = match open {
            Some(socket) => (socket, None),
            None => {
                // The connection task may be between two attempts, or in one: it must not
                // open a second socket for this device beside ours (4409).
                if let Some(connection) = self.connection.take() {
                    connection.stop.cancel();
                    let _ = connection.task.await;
                }
                let connecting = self.shared.net.connect_auth(account.credentials());
                match tokio::time::timeout_at(deadline, connecting).await {
                    Ok(Ok(Connected { socket, events })) => (Arc::new(socket), Some(events)),
                    Ok(Err(
                        e @ (TransportError::DeviceDeregistered
                        | TransportError::Rejected { status: 401 | 403 }),
                    )) => {
                        // What the connection task takes as "unlinked" (`connection::run`):
                        // the phone removed this device while it was away.
                        tracing::info!(
                            reason = %e,
                            "the server no longer knows this device: nothing to remove"
                        );
                        return DeviceLogout::Removed;
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            error = %e,
                            "logout: the server could not be reached to remove this device; \
                             unlink it on the phone"
                        );
                        return DeviceLogout::NotReached;
                    }
                    Err(_) => {
                        tracing::warn!(
                            "logout: no connection in time to remove this device; unlink it \
                             on the phone"
                        );
                        return DeviceLogout::NotReached;
                    }
                }
            }
        };
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let path = format!("/v1/devices/{}", u32::from(account.device_id));
        let request = crate::net::ChatRequest::new(http::Method::DELETE, &path)
            .map(|request| request.with_timeout(left.min(LOGOUT_REQUEST)));
        let answer = match request {
            Ok(request) => self.shared.net.request(&socket, request).await,
            Err(e) => Err(e),
        };
        let outcome = match answer {
            Ok(response) => {
                let outcome = device_logout_of(response.status);
                if outcome == DeviceLogout::Removed {
                    tracing::info!(
                        status = response.status.as_u16(),
                        "this device was removed from the account"
                    );
                } else {
                    tracing::warn!(
                        status = response.status.as_u16(),
                        "logout: the server did not remove this device; unlink it on the phone"
                    );
                }
                outcome
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "logout: the server could not be told to remove this device; unlink it on \
                     the phone"
                );
                DeviceLogout::NotReached
            }
        };
        if own.is_some() {
            self.shared.net.disconnect(&socket).await;
        }
        outcome
    }

    /// The unlink procedure (§4.3 step 5; Signal obligation 8): this device's credentials are
    /// void. All network work stops; stage 2 finishes the rows already decrypted into the inbox
    /// with what it has (no socket: a group message is applied with the last known group state
    /// or stays behind); the forwarder hands the core everything written so far (≤ `drain`);
    /// then the actor exits with `Exit::LoggedOut` and the core wipes `backend.db`, the keys
    /// and the account secret. Conversations and messages stay in `core.db`.
    async fn unlinked(&mut self, reason: String, drain: Duration) -> Exit {
        let deadline = tokio::time::Instant::now() + drain;
        self.shared.unlinking.set(true);
        self.shared.set_conn(ConnState::LoggedOut {
            reason: reason.clone(),
        });
        // A history transfer ends with the login it belonged to. Its key and the archive's
        // place leave the database files here, not only with the core's wipe of them (the
        // row is deleted and the WAL truncated), and so does the archive file.
        self.stop_transfer().await;
        crate::transfer::forget(&self.shared).await;
        let dir = self.shared.transfer.dir().to_owned();
        let _ = tokio::task::spawn_blocking(move || crate::transfer::remove_dir(&dir)).await;
        if let Some(login) = self.login.take() {
            login.task.abort();
        }
        if let Some(connection) = self.connection.take() {
            connection.stop.cancel();
            let _ = connection.task.await;
        }
        self.shared.unauth.close(&self.shared.net).await;
        let stage2 = deadline.min(tokio::time::Instant::now() + UNLINK_STAGE2);
        let settled = tokio::time::timeout_at(stage2, stage2::settle(&self.shared))
            .await
            .is_ok();
        self.stage2_stop.cancel();
        let _ = (&mut self.stage2).await;
        let left = self.shared.db.read(stage2::backlog).unwrap_or(-1);
        if !settled || left > 0 {
            // Rows that need the network they no longer have (a group never fetched); they go
            // with the wipe, as the envelopes still queued on the server do.
            tracing::warn!(rows = left, "unlinked with undelivered inbox rows");
        }
        if let Ok(target) = outbox::max_seq(&self.shared.db)
            && target > 0
            && tokio::time::timeout_at(deadline, self.shared.wait_acked(target))
                .await
                .is_err()
        {
            tracing::warn!("unlinked before the core took every event");
        }
        self.forwarder_stop.cancel();
        let _ = (&mut self.forwarder).await;
        let exit = Exit::LoggedOut { reason };
        tracing::info!(?exit, "account actor stopped");
        exit
    }

    /// `Command::StartLogin`. A login that was cancelled a moment ago may still be ending
    /// (`CancelLogin` only asks it to stop): it is brought to its end first, as the loop would
    /// do, so that the new login is not taken for a second one and lost.
    async fn start_login(
        &mut self,
        flow: &str,
        relink: Option<kourier_backend_api::Relink>,
        taken: Vec<kourier_backend_api::TakenIdentity>,
    ) {
        if self
            .login
            .as_ref()
            .is_some_and(|login| login.stop.is_cancelled())
        {
            let deadline = tokio::time::Instant::now() + LOGOUT_LOGIN_SETTLE;
            if let Some(done) = self.settle_login(deadline).await {
                self.login_finished(done, true).await;
            }
        }
        let refuse = |detail: &str| BackendError::unsupported(detail.to_owned());
        let error = if flow != "qr" {
            Some(refuse("only the qr login flow exists for Signal"))
        } else if self.shared.account.borrow().is_some() {
            Some(BackendError::new(
                kourier_model::ErrorKind::InvalidInput,
                "this account is linked already",
            ))
        } else if self.login.is_some() {
            tracing::debug!("login already running");
            return;
        } else {
            None
        };
        if let Some(error) = error {
            self.login_failed(error).await;
            return;
        }
        // A new link: an import this run remembers belongs to an earlier one, and this link's
        // `Done` waits for the Storage Service sync unless its own transfer ends `done`.
        self.shared.transfer.forget_import();
        let (inputs, rx) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let task = spawn_local(link::link(
            self.shared.clone(),
            relink,
            taken,
            rx,
            stop.clone(),
        ));
        self.login = Some(Login { inputs, task, stop });
    }

    /// A login task ended. `stopped`: it had been asked to (`CancelLogin`), so the core no
    /// longer follows it: nothing is reported, and an account that was stored all the same is
    /// only kept for the `Logout` that follows (`Command::CancelLogin`).
    async fn login_finished(
        &mut self,
        done: std::result::Result<LoginResult, JoinError>,
        stopped: bool,
    ) {
        match done {
            Ok(Ok(account)) if stopped => {
                tracing::info!("a cancelled login had linked already; waiting for the logout");
                *self.shared.account.borrow_mut() = Some(account);
            }
            Ok(Ok(account)) => {
                *self.shared.account.borrow_mut() = Some(account);
                self.shared.lanes.wake();
                self.shared.login_pending.set(true);
                self.shared.settling.begin();
                // The link stored a history transfer (the phone sent its key): it runs
                // from here, and receiving waits for its end (`transfer.rs`).
                if self.shared.transfer.holds() {
                    self.start_transfer();
                }
                let progress = LoginStepKind::Progress {
                    label: "Syncing contacts and groups".to_owned(),
                    fraction: None,
                };
                let _ = self
                    .shared
                    .emit(BackendEvent::Login(LoginStep {
                        step_id: "syncing".to_owned(),
                        txn: 1,
                        kind: progress,
                    }))
                    .await;
                // §6.5 steps 7-8 run on the connection (see `connection::post_connect`).
                self.connect();
            }
            Ok(Err(LinkEnd::Stopped)) => tracing::debug!("the login stopped"),
            Ok(Err(LinkEnd::Unwanted { device, error })) => {
                // Linked on the server, kept nowhere: without this request the device would
                // stay on the user's phone with nobody behind it.
                let deadline = tokio::time::Instant::now() + LOGOUT_NETWORK;
                self.remove_device(&device, deadline).await;
                if let Some(error) = error {
                    self.login_failed(error).await;
                }
            }
            Ok(Err(LinkEnd::Failed(_))) if stopped => tracing::debug!("the cancelled login failed"),
            Ok(Err(LinkEnd::Failed(error))) => self.login_failed(error).await,
            Err(e) if e.is_cancelled() => {}
            Err(e) => {
                self.login_failed(BackendError::internal(format!("login task: {e}")))
                    .await;
            }
        }
    }

    async fn login_failed(&self, error: BackendError) {
        tracing::warn!(error = %error, "login failed");
        let _ = self
            .shared
            .emit(BackendEvent::Login(LoginStep {
                step_id: "failed".to_owned(),
                txn: 1,
                kind: LoginStepKind::Failed { error },
            }))
            .await;
    }

    fn transmit(
        &self,
        local: kourier_model::LocalMsgId,
        req: kourier_backend_api::OutgoingRequest,
        reply: tokio::sync::oneshot::Sender<TransmitOutcome>,
    ) {
        let shared = self.shared.clone();
        spawn_local(async move {
            // Clone out of the cells first: no `RefCell` borrow may live across an await.
            let socket = shared.socket.borrow().clone();
            let outcome = match (shared.account(), socket) {
                (Some(account), Some(socket)) => {
                    let uploader = send::media::Uploader {
                        shared: &shared,
                        socket: &socket,
                        local,
                        net_local: &req.local,
                    };
                    match &req.conv {
                        kourier_model::ConvKey::Group(_) => {
                            send::group::transmit(&shared, &socket, &account, &req, &uploader).await
                        }
                        _ => {
                            let ctx = shared.send_context(&account, &socket);
                            send::transmit(&ctx, &req, &uploader).await
                        }
                    }
                }
                (None, _) => TransmitOutcome::Failed(BackendError::unsupported("not linked")),
                (Some(_), None) => {
                    TransmitOutcome::Failed(BackendError::transient("not connected"))
                }
            };
            // A challenge is answered with its token (`Command::SubmitChallenge`).
            shared.note_challenge(&outcome);
            // The core may have stopped waiting; the send is on the wire either way (§4.7).
            let _ = reply.send(outcome);
        });
    }

    /// Stops every task, lets the forwarder drain briefly, then reports `exit`.
    ///
    /// A login that is still running ends here for good: the core does not take a link up
    /// again after a stop, it discards an unfinished one at its next start (§4.3). So the
    /// login is stopped like a cancelled one ([`SHUTDOWN_LOGIN`]), and a device the server
    /// had linked for it, which is stored nowhere, is removed from the account instead of
    /// staying on the user's phone. An account the login had stored already stays: the core
    /// gets its `AccountReady` from the forwarder.
    async fn shutdown(&mut self, exit: Exit) -> Exit {
        let deadline = tokio::time::Instant::now() + SHUTDOWN_LOGIN;
        if let Some(Ok(Err(LinkEnd::Unwanted { device, .. }))) = self.settle_login(deadline).await {
            self.remove_device(&device, deadline).await;
        }
        // Stopped where it is: the next start goes on from the stored state.
        self.stop_transfer().await;
        if let Some(connection) = self.connection.take() {
            connection.stop.cancel();
            let _ = connection.task.await;
        }
        self.shared.unauth.close(&self.shared.net).await;
        // Lane tasks may still finish their current step; the driver starts no new ones.
        self.stage2_stop.cancel();
        let _ = (&mut self.stage2).await;
        if let Ok(target) = outbox::max_seq(&self.shared.db)
            && target > 0
        {
            let _ = tokio::time::timeout(SHUTDOWN_DRAIN, self.shared.wait_acked(target)).await;
        }
        self.forwarder_stop.cancel();
        let _ = (&mut self.forwarder).await;
        if !matches!(exit, Exit::LoggedOut { .. }) {
            self.shared
                .set_conn(ConnState::Offline { retry_at_ms: None });
        }
        tracing::info!(?exit, "account actor stopped");
        exit
    }
}

/// What the server's answer to `DELETE /v1/devices/<id>` means for the user (link feedback L4):
/// a success, or an answer that says the server no longer knows this device or its credentials
/// (401, 403, 404: the phone removed it already, or a second instance raced), leaves nothing
/// to remove on the phone; anything else (a server error, 429 or 413 for a request that was
/// refused, any other refusal) did not remove it. (A timeout or a transport error has no
/// answer and is `NotReached` where the request is made.)
fn device_logout_of(status: http::StatusCode) -> DeviceLogout {
    match status.as_u16() {
        200..=299 | 401 | 403 | 404 => DeviceLogout::Removed,
        _ => DeviceLogout::NotReached,
    }
}

/// Test support: a [`Shared`] without a network, for driving stage 1, stage 2 and the
/// forwarder by hand.
#[cfg(test)]
pub(crate) mod testing {
    use kourier_backend_api::SystemClock;

    use super::*;
    use crate::net::libsignal::LibsignalSocket;
    use crate::net::{
        AuthCredentials, ChatEvent, Connected, HttpsRequest, HttpsResponse, ProvisioningEvent,
        TransportError,
    };

    /// A transport that never connects (tests feed sockets by hand). It counts the attempts
    /// to open the authenticated socket, and a test may hold each one until it lets it fail.
    #[derive(Default)]
    pub(crate) struct NoTransport {
        /// `connect_auth` calls so far.
        pub(crate) auth_attempts: std::sync::atomic::AtomicUsize,
        /// While set, a `connect_auth` fails only once [`Self::auth_fail`] was notified.
        pub(crate) auth_held: std::sync::atomic::AtomicBool,
        /// Lets one held `connect_auth` fail.
        pub(crate) auth_fail: Notify,
    }

    impl SignalTransport for NoTransport {
        type Socket = LibsignalSocket;

        async fn connect_auth(
            &self,
            _credentials: &AuthCredentials,
        ) -> std::result::Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
            use std::sync::atomic::Ordering;
            self.auth_attempts.fetch_add(1, Ordering::SeqCst);
            if self.auth_held.load(Ordering::SeqCst) {
                self.auth_fail.notified().await;
            }
            Err(TransportError::Disconnected)
        }

        async fn connect_unauth(
            &self,
        ) -> std::result::Result<Connected<LibsignalSocket, ChatEvent>, TransportError> {
            Err(TransportError::Disconnected)
        }

        async fn connect_provisioning(
            &self,
        ) -> std::result::Result<Connected<LibsignalSocket, ProvisioningEvent>, TransportError>
        {
            Err(TransportError::Disconnected)
        }

        async fn https(
            &self,
            _request: HttpsRequest,
        ) -> std::result::Result<HttpsResponse, TransportError> {
            Err(TransportError::Disconnected)
        }

        async fn https_download(
            &self,
            _request: HttpsRequest,
            _sink: crate::net::DownloadSink,
        ) -> std::result::Result<http::StatusCode, TransportError> {
            Err(TransportError::Disconnected)
        }

        async fn https_upload(
            &self,
            _request: crate::net::UploadRequest,
        ) -> std::result::Result<HttpsResponse, TransportError> {
            Err(TransportError::Disconnected)
        }

        fn network_changed(&self) {}
    }

    /// A [`Shared`] (not yet in an `Rc`, so a test can still swap its lanes) and its far ends.
    pub(crate) struct TestShared {
        /// The shared state.
        pub(crate) shared: Shared<NoTransport>,
        /// What the actor sends to the core.
        pub(crate) core: mpsc::Receiver<Envelope>,
        /// The forwarder's end of `Shared::acked`.
        pub(crate) acked: watch::Sender<i64>,
        /// Published connection states.
        pub(crate) conn: watch::Receiver<ConnState>,
    }

    /// A linked account's shared state over `db`, on the current runtime.
    pub(crate) fn shared(
        db: Rc<ZoneDb>,
        account: Account,
        trust_roots: Vec<PublicKey>,
    ) -> TestShared {
        let (events, core) = mpsc::channel(64);
        let (acked_tx, acked) = watch::channel(0);
        let (conn_tx, conn) = watch::channel(ConnState::Offline { retry_at_ms: None });
        let shared = Shared::new(SharedParts {
            net: Net::new(
                Arc::new(NoTransport::default()),
                tokio::runtime::Handle::current(),
            ),
            db,
            events,
            clock: Arc::new(SystemClock),
            config: BackendConfig::default(),
            trust_roots,
            zk_params: Vec::new(),
            wake: Rc::new(Notify::new()),
            acked,
            account: Some(account),
            tuning: Tuning::default(),
            conn: conn_tx,
            blob_dir: std::env::temp_dir().join("kourier-signal-test-blobs-unused"),
            transfer: crate::transfer::TransferShared::new(
                std::env::temp_dir().join("kourier-signal-test-transfer-unused"),
                false,
            ),
        });
        TestShared {
            shared,
            core,
            acked: acked_tx,
            conn,
        }
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;

    /// A login that starts is a new link: what an earlier transfer of this actor imported
    /// (`TransferShared::imported`, which lets a link's `Done` skip the Storage Service sync)
    /// is forgotten, so a later link on the same actor waits for the sync again (link
    /// feedback L2). A login that is refused (the account is linked already) forgets
    /// nothing: the link that is complete keeps what its transfer said.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_login_that_starts_forgets_an_earlier_import() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                let shared = Rc::new(test.shared);
                let (_commands_tx, commands) = mpsc::channel(8);
                let mut actor = Actor {
                    shared: shared.clone(),
                    commands,
                    cancel: CancellationToken::new(),
                    login: None,
                    connection: None,
                    held: false,
                    transfer: None,
                    scrub_owed: false,
                    forwarder: spawn_local(async {}),
                    forwarder_stop: CancellationToken::new(),
                    stage2: spawn_local(async {}),
                    stage2_stop: CancellationToken::new(),
                };

                // Linked, its transfer imported the archive; a second login is refused.
                shared.transfer.mark_imported();
                actor.start_login("qr", None, Vec::new()).await;
                assert!(actor.login.is_none(), "the account is linked already");
                assert!(
                    shared.transfer.imported(),
                    "a refused login forgets nothing"
                );

                // No account (it went before the next link): the login starts and forgets.
                *shared.account.borrow_mut() = None;
                actor.start_login("qr", None, Vec::new()).await;
                assert!(actor.login.is_some(), "the login started");
                assert!(
                    !shared.transfer.imported(),
                    "a new link must not inherit the earlier import"
                );

                let login = actor.login.take().expect("login");
                login.stop.cancel();
                let _ = login.task.await;
            })
            .await;
    }
}

#[cfg(test)]
mod logout_tests {
    use super::*;

    fn of(status: u16) -> DeviceLogout {
        device_logout_of(http::StatusCode::from_u16(status).expect("a status"))
    }

    /// Link feedback L4: the answer of the server to the device's removal. What says the
    /// device is gone (or never was) leaves nothing to remove on the phone; a server that
    /// failed, was busy or refused the request did not remove it, and the user is told to.
    #[test]
    fn the_servers_answer_to_the_removal_says_whether_the_device_is_gone() {
        // Removed.
        for status in [200, 204] {
            assert_eq!(of(status), DeviceLogout::Removed, "{status}");
        }
        // The server no longer knows this device or its credentials (the phone removed the
        // device while the socket was still up).
        for status in [401, 403, 404] {
            assert_eq!(of(status), DeviceLogout::Removed, "{status}");
        }
        // The server could not be asked, or refused: still listed on the phone, maybe.
        for status in [400, 409, 413, 422, 423, 429, 500, 502, 503, 504] {
            assert_eq!(of(status), DeviceLogout::NotReached, "{status}");
        }
    }
}
