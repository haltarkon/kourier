//! One supervisor task per account (§4.3): spawns the backend actor, runs its ingest (one
//! `core.db` transaction per envelope, then the ack), forwards commands, and handles the
//! lifecycle: login, unlink wipe, relink, removal, and restarts with capped exponential backoff
//! on the injected [`Clock`](kourier_backend_api::Clock).
//!
//! Reconnects are the backend's (§4.7); the supervisor passes the desktop's hints on. A network
//! change comes as a burst of signals and a resume can come with one, so hints are collected
//! for [`HINT_SETTLE`] and reach the backend as one command (`Resumed` if a resume was among
//! them, else `NetworkChanged`), never while a login runs. A resume also makes the core's own
//! timers look at the clock again: timers do not run while the machine sleeps.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendEvent, BackendNotice, Command, ContextParams, DurableCommand, Envelope,
    Exit, Network, Relink, StartMode, UiPresence,
};
use kourier_db::{Priority, failpoint};
use kourier_model::{
    AccountId, AccountState, AccountUuid, CmdId, ConnState, ConversationId, LoginStepKind,
    MsgIdScope, PersistError, Redacted, UserId,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::attachments::{self, DownloadJob, derive};
use crate::dispatch::{Dispatcher, Signal, Wake};
use crate::error::{CoreError, Result};
use crate::facade::{ActionSubject, Progress, UiEvent};
use crate::ingest::{self, Changes};
use crate::runtime::Shared;
use crate::store::lane;
use crate::vault::VaultError;
use crate::{local, query};

/// Kill point between the ingest commit and the ack (§10.4): the backend forwards again.
pub const FAILPOINT_BEFORE_ACK: &str = "core_ingest_before_ack";
/// Kill point after a `pending_commands` row committed, before the command is sent (§10.4).
pub const FAILPOINT_AFTER_PENDING_COMMAND: &str = "after_pending_command";
/// Kill point after a download was committed (`attachment_downloads`), before its command is
/// handed to the actor (§10.4): the next start resends it with the same target.
pub const FAILPOINT_BEFORE_DOWNLOAD_SEND: &str = "core_before_download_send";

/// Splits an envelope into writer jobs (§4.9, §4.10): consecutive live events form one
/// high-priority job; each history batch (≤ 500 messages) is its own low-priority job, so the
/// writer interleaves other accounts' live traffic between the chunks of a large import. The
/// ack waits for the last job; a crash in between replays the envelope into idempotent ingest.
pub(crate) fn segments(events: Vec<BackendEvent>) -> Vec<(Priority, Vec<BackendEvent>)> {
    let mut out: Vec<(Priority, Vec<BackendEvent>)> = Vec::new();
    for e in events {
        let history = matches!(e, BackendEvent::HistoryBatch { .. });
        match out.last_mut() {
            Some((Priority::High, seg)) if !history => seg.push(e),
            _ => out.push((
                if history {
                    Priority::Low
                } else {
                    Priority::High
                },
                vec![e],
            )),
        }
    }
    out
}

/// Restart policy of crashed backends (§4.3 step 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RestartPolicy {
    /// First delay.
    pub min: Duration,
    /// Delay cap.
    pub max: Duration,
    /// Give up after this many failures within `window`.
    pub max_failures: usize,
    /// The failure window.
    pub window: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            min: Duration::from_secs(1),
            max: Duration::from_secs(300),
            max_failures: 5,
            window: Duration::from_secs(600),
        }
    }
}

impl RestartPolicy {
    /// The delay before restart `attempt` (1-based): `min · 2^(attempt-1)` plus up to 25 %
    /// jitter (a stable function of `seed`), capped at `max`.
    pub fn delay(&self, attempt: u32, seed: u64) -> Duration {
        let base = self
            .min
            .saturating_mul(1u32 << attempt.saturating_sub(1).min(20))
            .min(self.max);
        let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
        let h = kourier_model::stable_hash(
            &[seed.to_le_bytes(), u64::from(attempt).to_le_bytes()].concat(),
        );
        let jitter = base_ms / 4 * (h % 1001) / 1000;
        Duration::from_millis(base_ms.saturating_add(jitter)).min(self.max)
    }
}

/// Lifecycle timeouts (§4.3, §4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// After `Shutdown`, wait this long before cancelling the actor's token (§4.7: 5 s).
    pub shutdown_grace: Duration,
    /// Best-effort `Logout` before removal (§4.3 step 7: 10 s).
    pub logout: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            shutdown_grace: Duration::from_secs(5),
            logout: Duration::from_secs(10),
        }
    }
}

/// The `tracing` target of the core's timing marks (§1.3 "how measured"). Each mark is one
/// event at `TRACE` level with a `name` and numbers only, never content:
///
/// - `recv.commit`: one envelope of durable events was committed and acked. `account`,
///   `events`, `messages` (new incoming messages in it), `notify` (of those, how many may
///   be notified), `catch_up` (how many belong to a catch-up), `took_us` (from the envelope
///   reaching the core to its commit, the ack included);
/// - `recv.catchup`: the backend left `Syncing`. `account`, `messages` and `conversations`
///   (what the catch-up brought), `took_ms` (how long `Syncing` lasted), `per_second`.
///
/// Nothing subscribes by default, so a mark costs one disabled-callsite check. A measuring
/// build enables the target and stamps each event on arrival (`kourier_app::trace`).
pub const PERF_TARGET: &str = "kourier::perf";

/// How long connectivity hints are collected before one command goes to the backend: the
/// signals of one network change (NetworkManager walks through several states) and of a
/// resume arrive within this.
pub const HINT_SETTLE: Duration = Duration::from_secs(1);

/// A connectivity hint from the desktop (§4.7). Ordered: a resume covers a network change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Hint {
    /// NetworkManager reported a change.
    NetworkChanged,
    /// logind reported a resume from suspend.
    Resumed,
}

/// Drops failure times that lie outside `window` before `now`, and those after `now` (the
/// clock was set back, §9.6: they would stay "recent" until the clock caught up and could halt
/// an account for failures of another day).
pub(crate) fn recent_failures(failures: &mut VecDeque<u64>, now: u64, window: u64) {
    failures.retain(|t| *t <= now && now - *t <= window);
}

/// Messages from the core task to a supervisor.
#[derive(Debug)]
pub(crate) enum Control {
    LoginInput {
        txn: u64,
        values: Vec<(String, Redacted<String>)>,
    },
    CancelLogin,
    Relink {
        flow: String,
    },
    Remove,
    Restart,
    /// Forwarded to a running actor whose account is linked; dropped otherwise (reply senders
    /// then report "gone").
    Backend(Command),
    /// A connectivity hint: collected and passed to a running, linked actor as one command.
    Hint(Hint),
    /// Durable commands; resent from `pending_commands` at the next start if not running, or
    /// when the login that runs is done.
    Durable(Vec<(CmdId, DurableCommand)>),
    /// Downloads a user asked for; resent from `attachment_downloads` at the next start if not
    /// running.
    Downloads(Vec<DownloadJob>),
    /// The outbox changed or a message was retried (§4.8); the next start scans it anyway.
    Outbox(Signal),
    /// The user accepted a safety-number change: tell the backend, resume paused queues.
    ConfirmIdentity(UserId),
    /// The user's answer to a challenge: handed to the running actor with the token of the
    /// challenge the outbox waits on; dropped without an actor (the reply then reports "gone").
    SubmitChallenge {
        answer: Redacted<String>,
        reply: kourier_backend_api::Reply<()>,
    },
    Shutdown(oneshot::Sender<()>),
}

/// What a supervisor does next.
#[derive(Debug)]
pub(crate) enum Plan {
    /// Spawn fresh and log in.
    Login {
        flow: String,
        relink: Option<Relink>,
    },
    /// Spawn with stored credentials.
    Run(StartMode),
    /// Logged out: wait for relink or removal.
    Idle,
    /// Gave up after repeated failures: wait for `Restart` or removal.
    Halted,
}

enum Next {
    Plan(Plan),
    Stop,
}

/// The core's ends of one running actor.
struct Running {
    commands: mpsc::Sender<Command>,
    events: mpsc::Receiver<Envelope>,
    conn: watch::Receiver<ConnState>,
    conn_open: bool,
    ui: watch::Sender<UiPresence>,
    exited: oneshot::Receiver<Exit>,
    exit_done: bool,
    thread: Option<std::thread::JoinHandle<()>>,
    cancel: CancellationToken,
    /// Unanswered downloads were sent again (once per actor, at the first `Online`).
    downloads_resumed: bool,
    /// The newest download request of an earlier actor: requests above it belong to this one.
    resume_up_to: i64,
}

/// How long a command may wait for room in the actor's queue (§4.2: bounded, 64).
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

impl Running {
    /// Sends a command; an actor that does not read its queue for [`SEND_TIMEOUT`] loses it
    /// (durable commands are resent from `pending_commands` at the next start).
    async fn send(&self, cmd: Command) {
        let name = cmd.name();
        match tokio::time::timeout(SEND_TIMEOUT, self.commands.send(cmd)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => tracing::debug!(command = name, "account actor is gone; command dropped"),
            Err(_) => tracing::warn!(
                command = name,
                "account actor does not read commands; dropped"
            ),
        }
    }
}

enum Flow {
    Continue,
    LoginFailed(String),
}

/// Refuses an envelope of an actor whose account the core did not accept.
fn refuse(env: Envelope) {
    if let Some(ack) = env.ack {
        ack.complete(Err(PersistError {
            retryable: false,
            detail: "the account is not linked".into(),
        }));
    }
}

/// How the envelopes of an actor are treated while the core shuts down.
fn on_shutdown(linked: bool, relinking: bool) -> Ingest {
    if linked {
        Ingest::Linked
    } else {
        Ingest::Linking { relinking }
    }
}

/// What [`Supervisor::stop`] does with the envelopes an actor still sends while it stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ingest {
    /// The core accepted the actor's identity: they are applied.
    Linked,
    /// The core is shutting down while a login runs: the rules of the running loop hold. A
    /// link that finishes on the way out is kept (the account starts linked next time), a
    /// relink only when it is the same account.
    Linking {
        /// The login is a relink.
        relinking: bool,
    },
    /// The login failed, was cancelled or was refused as another account, or the account is
    /// being removed: nothing is taken in.
    Refused,
}

/// The supervisor of one account.
pub(crate) struct Supervisor {
    sh: Arc<Shared>,
    account: AccountId,
    uuid: AccountUuid,
    network: Arc<dyn Network>,
    scope: MsgIdScope,
    ctl: mpsc::UnboundedReceiver<Control>,
    /// Failure times within the restart window.
    failures: VecDeque<u64>,
    /// Restart attempt (1-based) of the current failure streak; a streak ends when an actor
    /// stayed up for a whole window, so the backoff starts over.
    attempt: u32,
    catch_up: (u32, BTreeSet<ConversationId>),
    typing: HashSet<(ConversationId, UserId)>,
    syncing: bool,
    /// The outbox scheduler (§4.8).
    dispatch: Dispatcher,
    /// Safety-number confirmations given while no actor ran: sent at the next start.
    confirm_later: Vec<UserId>,
    /// Commands waiting for room in the actor's queue (downloads): handed over as the actor
    /// takes them, so a burst never blocks the envelope loop. Dropped when the actor stops (the
    /// next start resends from the rows).
    out: VecDeque<Command>,
    /// The connectivity hint being collected and when it goes to the backend. The sleep is in
    /// a `Mutex` only so the supervisor stays `Sync` (it is polled through `&mut`).
    hint: Option<(Hint, std::sync::Mutex<kourier_backend_api::Sleep>)>,
    /// Something that was applied took rows out of the outbox ([`Changes::outbox`]): the
    /// dispatcher looks again before the loop waits.
    outbox_dirty: bool,
    /// When the backend's current `Syncing` began (the `recv.catchup` mark).
    syncing_since: Option<std::time::Instant>,
}

impl Supervisor {
    pub(crate) fn new(
        sh: Arc<Shared>,
        account: AccountId,
        uuid: AccountUuid,
        network: Arc<dyn Network>,
        ctl: mpsc::UnboundedReceiver<Control>,
    ) -> Self {
        let scope = network.descriptor().msg_id_scope;
        Self {
            sh,
            account,
            uuid,
            network,
            scope,
            ctl,
            failures: VecDeque::new(),
            attempt: 0,
            catch_up: (0, BTreeSet::new()),
            typing: HashSet::new(),
            syncing: false,
            dispatch: Dispatcher::new(account, scope),
            confirm_later: Vec::new(),
            out: VecDeque::new(),
            hint: None,
            outbox_dirty: false,
            syncing_since: None,
        }
    }

    /// Notes a connectivity hint: the first one of a burst starts the settle time, later ones
    /// only raise what will be sent.
    fn note_hint(&mut self, hint: Hint) {
        match &mut self.hint {
            Some((pending, _)) => *pending = (*pending).max(hint),
            None => {
                let settle = u64::try_from(HINT_SETTLE.as_millis()).unwrap_or(u64::MAX);
                let at = self.sh.clock.now_ms().saturating_add(settle);
                let sleep = self.sh.clock.sleep_until(at);
                self.hint = Some((hint, std::sync::Mutex::new(sleep)));
            }
        }
    }

    /// Completes when the collected hint is due (never without one).
    async fn hint_due(hint: &mut Option<(Hint, std::sync::Mutex<kourier_backend_api::Sleep>)>) {
        match hint.as_mut() {
            Some((_, sleep)) => {
                sleep
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .await
            }
            None => std::future::pending().await,
        }
    }

    fn emit(&self, e: UiEvent) {
        // No receiver (headless tests, shutdown) is fine.
        let _ = self.sh.events.send(e);
    }

    async fn set_conn(&mut self, state: ConnState) {
        let was_syncing = std::mem::replace(
            &mut self.syncing,
            matches!(state, ConnState::Syncing { .. }),
        );
        // Connected and caught up: the missed calls that waited for it (`crate::calls`) go
        // into the summary, those that are still missed.
        let online = matches!(state, ConnState::Online { .. });
        let held = self.sh.calls_online(self.account, online);
        for conv in self.sh.still_missed(held).await {
            self.catch_up.0 = self.catch_up.0.saturating_add(1);
            self.catch_up.1.insert(conv);
        }
        self.sh
            .conn_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(self.account, state.clone());
        self.note_clock(&state);
        self.emit(UiEvent::ConnState {
            account: self.account,
            state,
        });
        if self.syncing && !was_syncing {
            self.syncing_since = Some(std::time::Instant::now());
        }
        if (was_syncing && !self.syncing) || (online && self.catch_up.0 > 0) {
            let (n, convs) = std::mem::take(&mut self.catch_up);
            if let Some(since) = self.syncing_since.take() {
                let took = since.elapsed();
                let per_second = f64::from(n) / took.as_secs_f64().max(0.001);
                tracing::trace!(
                    target: PERF_TARGET,
                    name = "recv.catchup",
                    account = self.account.0,
                    messages = n,
                    conversations = convs.len(),
                    took_ms = u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
                    per_second = per_second.round(),
                );
            }
            if n > 0 {
                self.emit(UiEvent::CatchUpSummary {
                    account: self.account,
                    messages: n,
                    conversations: u32::try_from(convs.len()).unwrap_or(u32::MAX),
                });
            }
        }
    }

    /// §9.6: the sweeper leaves the account alone from the moment its backend finds the clock
    /// wrong until a server took its connection again. Stored, so it holds across a restart.
    fn note_clock(&self, state: &ConnState) {
        let wrong = if crate::expiry::clock_is_wrong(state) {
            true
        } else if crate::expiry::clock_is_accepted(state) {
            false
        } else {
            return;
        };
        let changed = {
            let mut held = self
                .sh
                .clock_wrong
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if wrong {
                held.insert(self.account)
            } else {
                held.remove(&self.account)
            }
        };
        // The first verdict of this run (after the hold above is in place, so the sweeper
        // never sees the account in neither set while its clock is wrong).
        self.clock_spoken_for();
        if !changed {
            return;
        }
        tracing::info!(
            account = self.account.0,
            wrong,
            "the backend's verdict on the local clock changed"
        );
        self.sh.expiry_wake.notify_one();
        let (sh, account) = (self.sh.clone(), self.account);
        tokio::spawn(async move {
            // What is stored is the verdict at the time of the write, not of this call: two
            // changes in a row may reach the writer in either order, and both then store the
            // latest.
            let held = sh.clone();
            let stored = sh
                .store
                .write(Priority::High, lane(account), move |c| {
                    let wrong = held
                        .clock_wrong
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains(&account);
                    local::set_clock_wrong(c, account, wrong)
                })
                .await;
            if let Err(e) = stored {
                tracing::warn!(account = account.0, error = %e, "storing the clock verdict failed");
            }
        });
    }

    /// The backend gave its verdict on the clock, or no backend is left to give one: the
    /// account no longer waits for it (§9.6, [`Shared::clock_unverified`]).
    fn clock_spoken_for(&self) {
        let waited = self
            .sh
            .clock_unverified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.account);
        if waited {
            self.sh.expiry_wake.notify_one();
        }
    }

    /// Runs until the account is removed or the core shuts down.
    pub(crate) async fn run(mut self, mut plan: Plan) {
        loop {
            let next = match plan {
                Plan::Idle | Plan::Halted => self.wait(plan).await,
                p @ (Plan::Login { .. } | Plan::Run(_)) => self.incarnation(p).await,
            };
            match next {
                Next::Plan(p) => plan = p,
                Next::Stop => break,
            }
        }
        // Removed: no entry stays behind. Shutting down: the hold stays to the end. The
        // sweeper outlives this task by a moment, and an account whose clock nobody
        // confirmed in this run must not be swept because its backend was told to stop.
        if !self.sh.stopping() {
            self.clock_spoken_for();
        }
        self.sh
            .controls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.account);
        self.sh
            .conn_states
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.account);
    }

    /// No actor runs: wait for a relink (`Idle`), a restart (`Halted`), removal or shutdown.
    async fn wait(&mut self, plan: Plan) -> Next {
        // Nobody is left who could object to the clock.
        self.clock_spoken_for();
        loop {
            match self.ctl.recv().await {
                None => return Next::Stop,
                Some(Control::Shutdown(tx)) => {
                    let _ = tx.send(());
                    return Next::Stop;
                }
                Some(Control::Remove) => {
                    self.remove(None).await;
                    return Next::Stop;
                }
                Some(Control::Relink { flow }) if matches!(plan, Plan::Idle) => {
                    match self.prepare_relink(flow).await {
                        Ok(p) => return Next::Plan(p),
                        Err(e) => self.emit(UiEvent::AccountError {
                            account: self.account,
                            detail: e.to_string(),
                            restartable: false,
                        }),
                    }
                }
                Some(Control::Restart) if matches!(plan, Plan::Halted) => {
                    self.failures.clear();
                    self.attempt = 0;
                    return Next::Plan(Plan::Run(StartMode::Resume));
                }
                Some(other) => self.without_actor(other, "no actor running").await,
            }
        }
    }

    /// "Link again" (§4.3 step 6): a clean slate of credentials for the same account.
    async fn prepare_relink(&mut self, flow: String) -> Result<Plan> {
        let info = {
            let account = self.account;
            self.sh
                .store
                .read(move |c| crate::sql::account_info(c, account))
                .await?
                .ok_or(CoreError::UnknownAccount(self.account))?
        };
        if !matches!(info.state, AccountState::LoggedOut { .. }) {
            return Err(CoreError::InvalidState(
                "only a logged-out account can be relinked",
            ));
        }
        self.sh.vault.wipe_credentials(self.uuid).await?;
        self.sh.vault.create(self.uuid, self.network.kind()).await?;
        Ok(Plan::Login {
            flow,
            relink: Some(Relink {
                account: self.account,
                expect_own: info.own_ids,
            }),
        })
    }

    async fn incarnation(&mut self, plan: Plan) -> Next {
        let (start, login, relinking) = match plan {
            Plan::Login { flow, relink } => {
                let relinking = relink.is_some();
                (StartMode::Fresh, Some((flow, relink)), relinking)
            }
            Plan::Run(mode) => (mode, None, false),
            Plan::Idle | Plan::Halted => return Next::Plan(plan),
        };
        let mut linked = login.is_none();
        let mut run = match self.spawn(start).await {
            Ok(run) => run,
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "starting the account failed");
                if linked
                    && matches!(
                        e,
                        CoreError::Vault(VaultError::Missing | VaultError::SecretMissing)
                    )
                {
                    // The credentials are gone (an unlink wipe interrupted before `LoggedOut`
                    // was recorded, a lost keyring item): no restart can help, the user relinks.
                    self.logged_out("the account's keys are missing").await;
                    return Next::Plan(Plan::Idle);
                }
                if linked && matches!(e, CoreError::Vault(VaultError::WrongKey)) {
                    // The keys are there and this profile's key does not open them. That is
                    // not a logout: nothing is wiped, and no restart changes the key.
                    self.emit(UiEvent::AccountError {
                        account: self.account,
                        detail: "the account's keys do not open with this profile's key; nothing was deleted".into(),
                        restartable: true,
                    });
                    return Next::Plan(Plan::Halted);
                }
                return if linked {
                    self.backoff(start).await
                } else {
                    self.login_failed(relinking, e.to_string()).await
                };
            }
        };
        self.dispatch.start(&self.sh, linked).await;
        self.out.clear();
        self.hint = None;
        let account = self.account;
        run.resume_up_to = self
            .sh
            .store
            .read(move |c| ingest::newest_download(c, account))
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(account = account.0, error = %e, "reading pending downloads failed");
                0
            });
        // Subscribe before reading the current values, so a change in between is not lost.
        let mut ui_ready = self.sh.ui_ready.subscribe();
        let mut presence = self.sh.presence.subscribe();
        presence.mark_unchanged();
        self.push_presence(&run);
        match login {
            Some((flow, relink)) => {
                // A new link starts from scratch: confirmations for the old one do not apply.
                self.confirm_later.clear();
                run.send(Command::StartLogin { flow, relink }).await;
            }
            None => {
                if *ui_ready.borrow_and_update() {
                    run.send(Command::Connect).await;
                }
                self.resend_pending(&run).await;
                for user in std::mem::take(&mut self.confirm_later) {
                    run.send(Command::ConfirmIdentity { user }).await;
                }
            }
        }
        loop {
            tokio::select! {
                // Connection state first: a backend publishes `Syncing` before the catch-up
                // envelopes it sends, and the summary rule (§4.5) depends on seeing it first.
                biased;
                r = run.conn.changed(), if run.conn_open => {
                    if r.is_err() {
                        run.conn_open = false;
                    } else {
                        let state = run.conn.borrow_and_update().clone();
                        // The backend's own state, not the one shown (a challenge shows over
                        // `Online`, which is a verdict on the clock all the same).
                        self.note_clock(&state);
                        let resume = self.dispatch.conn(&state);
                        let online = state.is_online();
                        let shown = self.dispatch.shown(state);
                        self.set_conn(shown).await;
                        if online && linked && !run.downloads_resumed {
                            run.downloads_resumed = true;
                            self.resume_downloads(run.resume_up_to).await;
                        }
                        if resume {
                            self.kick(&run).await;
                        }
                    }
                }
                ev = run.events.recv() => match ev {
                    Some(env) => {
                        let was_linked = linked;
                        if let Flow::LoginFailed(detail) = self.envelope(env, &mut linked, relinking).await {
                            // The core turned the link away (a relink as another account),
                            // or the login failed in the backend. In the first case the
                            // device exists on the network and is logged out again; in the
                            // second the backend has none and just stops.
                            self.log_out(&mut run, Ingest::Refused).await;
                            return self.login_failed(relinking, detail).await;
                        }
                        if linked && !was_linked {
                            self.now_linked(&mut run).await;
                        }
                        if std::mem::take(&mut self.outbox_dirty) {
                            // What was applied removed outbox rows (a tombstone, an echo):
                            // the rows behind them start now, not when the timer armed for
                            // a removed row fires.
                            self.kick(&run).await;
                        }
                    }
                    None => {
                        let exit = if run.exit_done {
                            Exit::Panicked("exit already reported".into())
                        } else {
                            run.exit_done = true;
                            Exit::from_channel((&mut run.exited).await)
                        };
                        self.join(&mut run).await;
                        return self.after_exit(exit, linked, relinking, start).await;
                    }
                },
                exit = &mut run.exited, if !run.exit_done => {
                    run.exit_done = true;
                    let exit = Exit::from_channel(exit);
                    self.drain(&mut run, &mut linked, relinking).await;
                    self.join(&mut run).await;
                    return self.after_exit(exit, linked, relinking, start).await;
                }
                c = self.ctl.recv() => match c {
                    None => {
                        self.stop(&mut run, on_shutdown(linked, relinking)).await;
                        return Next::Stop;
                    }
                    Some(Control::Shutdown(tx)) => {
                        self.stop(&mut run, on_shutdown(linked, relinking)).await;
                        let _ = tx.send(());
                        return Next::Stop;
                    }
                    Some(Control::Remove) => {
                        self.remove(Some((run, linked))).await;
                        return Next::Stop;
                    }
                    Some(Control::LoginInput { txn, values }) => {
                        run.send(Command::SubmitLoginInput { txn, values }).await;
                    }
                    Some(Control::CancelLogin) if !linked => {
                        run.send(Command::CancelLogin).await;
                        // The link may have finished on the network a moment ago, with
                        // its `AccountReady` still on the way here: the device would stay
                        // linked to the user's account with nobody behind it. A backend
                        // that has a device logs it out; one that has none just stops.
                        self.log_out(&mut run, Ingest::Refused).await;
                        return self.login_failed(relinking, "login cancelled".into()).await;
                    }
                    // An actor the core has not accepted gets nothing that would go out in
                    // the account's name: a relink can turn out to be another account, and a
                    // roaming change or a typing notice would then be that account's. The
                    // command's reply is dropped, which its sender reads as "the account is
                    // not running".
                    Some(Control::Backend(cmd)) if linked => run.send(cmd).await,
                    Some(Control::Backend(cmd)) => {
                        tracing::debug!(
                            account = self.account.0,
                            command = cmd.name(),
                            "not linked yet; command dropped"
                        );
                    }
                    Some(Control::Hint(hint)) => {
                        let resumed = hint == Hint::Resumed
                            && !matches!(self.hint, Some((Hint::Resumed, _)));
                        self.note_hint(hint);
                        if resumed {
                            // Our own backoff sleep did not run while the machine slept
                            // (once per burst of hints).
                            self.dispatch.rearm();
                            self.kick(&run).await;
                        }
                    }
                    Some(Control::Durable(cmds)) if linked => {
                        for (id, cmd) in cmds {
                            run.send(cmd.into_command(id)).await;
                        }
                    }
                    // Their rows are stored: `now_linked` sends them once the account is.
                    Some(Control::Durable(_)) => {}
                    Some(Control::Downloads(jobs)) if linked => self.queue_downloads(jobs).await,
                    // Their rows are stored (`Downloading`), so they count among the unanswered
                    // downloads that are sent once the account is linked and online. A
                    // relink can turn out to be another account, which must not ask its
                    // server for this one's files.
                    Some(Control::Downloads(jobs)) => {
                        let newest = jobs.iter().map(|j| j.request.0).max().unwrap_or(0);
                        run.resume_up_to = run.resume_up_to.max(newest);
                    }
                    Some(Control::Outbox(signal)) => {
                        if self.dispatch.signal(signal) {
                            let state = run.conn.borrow().clone();
                            self.set_conn(state).await;
                        }
                        self.kick(&run).await;
                    }
                    Some(Control::ConfirmIdentity(user)) => {
                        // A link in progress starts from an empty identity store: there is
                        // nothing for its backend to confirm, and what it is linking may be
                        // another account. The paused queues are released all the same; the
                        // next attempt after the link meets the backend's own view.
                        if linked {
                            run.send(Command::ConfirmIdentity { user: user.clone() }).await;
                        }
                        self.confirmed(&user).await;
                        self.kick(&run).await;
                    }
                    Some(Control::SubmitChallenge { answer, reply }) if linked => {
                        let token = self.dispatch.challenge_token();
                        run.send(Command::SubmitChallenge { token, answer, reply }).await;
                    }
                    // No send of this actor was challenged (it has sent nothing), and the
                    // answer must not go out as whoever it is linking. The reply is dropped,
                    // which the sender reads as "the account is not running".
                    Some(Control::SubmitChallenge { .. }) => {
                        tracing::debug!(
                            account = self.account.0,
                            "not linked yet; challenge answer dropped"
                        );
                    }
                    // `RestartAccount` for an account whose actor runs: the user's "Reconnect"
                    // (`ConnState::NeedsUserAction(Reconnect)`, Signal's 4409). A backend that
                    // waits for the user connects only on `Connect` (§6.4); one that is
                    // connected or connecting already ignores it.
                    Some(Control::Restart) if linked => run.send(Command::Connect).await,
                    Some(other) => {
                        tracing::debug!(account = self.account.0, control = ?other, "not applicable while running");
                    }
                },
                w = self.dispatch.wake() => self.outbox_wake(w, &run).await,
                () = Self::hint_due(&mut self.hint) => {
                    // A login connects by itself; a hint must not start anything before it.
                    if let Some((hint, _)) = self.hint.take()
                        && linked
                    {
                        run.send(match hint {
                            Hint::NetworkChanged => Command::NetworkChanged,
                            Hint::Resumed => Command::Resumed,
                        })
                        .await;
                    }
                }
                // An owned permit on a clone: no borrow of `run` outlives the select.
                permit = run.commands.clone().reserve_owned(), if !self.out.is_empty() => match permit {
                    Ok(permit) => {
                        if let Some(cmd) = self.out.pop_front() {
                            permit.send(cmd);
                        }
                    }
                    // The actor is gone; its next start resends from the rows.
                    Err(_) => self.out.clear(),
                },
                r = ui_ready.changed() => {
                    if r.is_ok() && *ui_ready.borrow_and_update() {
                        run.send(Command::Connect).await;
                    }
                }
                r = presence.changed() => {
                    if r.is_ok() {
                        self.push_presence(&run);
                    }
                }
            }
        }
    }

    /// The core accepted the actor's `AccountReady` (a login or relink finished): what waited
    /// for the account goes out now. Nothing of it was handed to the actor before, so a relink
    /// that turned out to be another account (refused in [`Self::envelope`]) sent nothing.
    async fn now_linked(&mut self, run: &mut Running) {
        self.dispatch.linked();
        // Durable commands still stored for the account. A logout deletes them
        // (`local::logged_out`), so after a relink there are none unless that bookkeeping did
        // not complete; then they are the same account's and go out now.
        self.resend_pending(run).await;
        // By the states seen so far: an `Online` that came before the link was not acted on.
        // Downloads asked for while the link ran are among those resumed (`resume_up_to`).
        if self.dispatch.online() && !run.downloads_resumed {
            run.downloads_resumed = true;
            self.resume_downloads(run.resume_up_to).await;
        }
        self.kick(run).await;
    }

    /// Starts due outbox heads and publishes what that changed.
    async fn kick(&mut self, run: &Running) {
        match self.dispatch.kick(&self.sh, &run.commands).await {
            Ok(kicked) => {
                for failed in kicked.failed {
                    self.report_failed(failed);
                }
                self.publish(kicked.changes).await;
            }
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "starting outbox attempts failed")
            }
        }
    }

    /// Tells the UI about an edit, delete or reaction that was rolled back.
    fn report_failed(&self, failed: crate::outbox::FailedChange) {
        self.emit(UiEvent::ActionFailed {
            account: Some(self.account),
            action: failed.action,
            error: Arc::new(CoreError::from(failed.error)),
            subject: ActionSubject::Message(failed.message),
        });
    }

    /// A backoff elapsed, an attempt finished or a challenge ran out: record it, then start
    /// what is due.
    async fn outbox_wake(&mut self, w: Wake, run: &Running) {
        if matches!(w, Wake::ChallengeOver) {
            // The pause is lifted: the backend's own state shows again; if the network still
            // wants its challenge, the first send says so and the account pauses anew.
            let state = self.dispatch.shown_now();
            self.set_conn(state).await;
        }
        if let Wake::Done(done) = w {
            match self.dispatch.done(&self.sh, done).await {
                Ok(effect) => {
                    if let Some(state) = effect.conn {
                        self.set_conn(state).await;
                    }
                    if let Some(notice) = effect.notice {
                        self.emit(UiEvent::Notice {
                            account: self.account,
                            notice,
                        });
                    }
                    if let Some(failed) = effect.failed {
                        self.report_failed(failed);
                    }
                    self.publish(effect.changes).await;
                }
                // The row was queued again after a backoff (or, if even that failed, resumes at
                // the next actor start).
                Err(e) => {
                    tracing::warn!(account = self.account.0, error = %e, "recording a transmit outcome failed")
                }
            }
        }
        self.kick(run).await;
    }

    /// A control message while no actor runs (waiting for a relink or restart, or in the restart
    /// backoff). Outbox signals still reach the dispatcher (a retry lifts its conversation's
    /// identity pause, which survives actor restarts); a safety-number confirmation resumes the
    /// paused queues now and reaches the backend at the next start. Anything else is dropped.
    async fn without_actor(&mut self, c: Control, why: &'static str) {
        match c {
            Control::Outbox(signal) => {
                self.dispatch.signal(signal);
            }
            Control::ConfirmIdentity(user) => {
                self.confirmed(&user).await;
                if !self.confirm_later.contains(&user) {
                    self.confirm_later.push(user);
                }
            }
            // Nothing to tell: the next actor connects when it starts.
            Control::Hint(_) => {}
            other => {
                tracing::debug!(account = self.account.0, control = ?other, why, "ignored");
            }
        }
    }

    /// `ConfirmIdentity`: the conversations paused on `user` show their heads queued again.
    async fn confirmed(&mut self, user: &UserId) {
        let heads = self.dispatch.confirmed(user);
        if heads.is_empty() {
            return;
        }
        let (account, scope) = (self.account, self.scope);
        let now = i64::try_from(self.sh.clock.now_ms()).unwrap_or(i64::MAX);
        let res = self
            .sh
            .store
            .write(Priority::High, lane(account), move |c| {
                crate::outbox::requeue(c, account, scope, now, &heads)
            })
            .await;
        match res {
            Ok(ch) => self.publish(ch).await,
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "requeueing paused messages failed")
            }
        }
    }

    fn push_presence(&self, run: &Running) {
        let p = self.sh.presence.borrow().clone();
        let visible = p
            .visible
            .filter(|(a, _, _)| *a == self.account)
            .map(|(_, _, key)| key);
        run.ui.send_replace(UiPresence {
            focused: p.focused,
            visible,
            idle: false,
        });
    }

    /// Unwraps the key and spawns the actor.
    async fn spawn(&mut self, start: StartMode) -> Result<Running> {
        let key = self.sh.vault.unwrap(self.uuid).await?;
        let account = self.account;
        let prefs = self
            .sh
            .store
            .read(move |c| local::local_prefs(c, account))
            .await?;
        let cancel = self.sh.cancel.child_token();
        let (ctx, ends) = AccountContext::new(ContextParams {
            account: self.account,
            uuid: self.uuid,
            start,
            backend_db: self.sh.vault.backend_db(self.uuid),
            db_key: key,
            blob_dir: self.sh.vault.blob_dir(self.uuid),
            net_rt: self.sh.net_rt.clone(),
            cancel: cancel.clone(),
            clock: self.sh.clock.clone(),
            config: self.sh.backend.clone(),
            ui: UiPresence::default(),
            local_prefs: prefs,
        });
        let handle = self.network.spawn(ctx)?;
        tracing::info!(account = self.account.0, uuid = %self.uuid.short(), ?start, "account actor started");
        Ok(Running {
            commands: ends.commands,
            events: ends.events,
            conn: ends.conn,
            conn_open: true,
            ui: ends.ui,
            exited: handle.exited,
            exit_done: false,
            thread: handle.thread,
            cancel,
            downloads_resumed: false,
            resume_up_to: 0,
        })
    }

    /// Hands downloads to the actor (through [`Self::out`]) after creating their blob shard
    /// directories.
    async fn queue_downloads(&mut self, jobs: Vec<DownloadJob>) {
        if jobs.is_empty() {
            return;
        }
        let dir = self.sh.vault.blob_dir(self.uuid);
        let d = dir.clone();
        let paths: Vec<_> = jobs.iter().map(|j| j.path.clone()).collect();
        let prepared = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            let store = attachments::core_store(&d)?;
            for p in paths {
                if let Some(id) = attachments::core_id(&p) {
                    store.prepare(&id)?;
                }
            }
            Ok(())
        })
        .await;
        if !matches!(prepared, Ok(Ok(()))) {
            tracing::warn!(
                account = self.account.0,
                "creating blob directories failed; downloads wait for the next start"
            );
            return;
        }
        failpoint::hit(FAILPOINT_BEFORE_DOWNLOAD_SEND);
        self.out.extend(jobs.iter().map(|j| j.command(&dir)));
    }

    /// Downloads an earlier actor left unanswered (requests up to `up_to`) are sent again once
    /// the backend reached `Online` (§4.5: the catch-up boundary): by then every
    /// `AttachmentDone` it had recorded is ingested, so only downloads that really got no answer
    /// run again. Requests of this actor are not repeated.
    async fn resume_downloads(&mut self, up_to: i64) {
        if up_to == 0 {
            return;
        }
        let (account, scope) = (self.account, self.scope);
        let now = i64::try_from(self.sh.clock.now_ms()).unwrap_or(i64::MAX);
        match self
            .sh
            .store
            .write(Priority::High, lane(account), move |c| {
                ingest::resume_downloads(c, account, scope, now, up_to)
            })
            .await
        {
            Ok((jobs, ch)) => {
                self.publish(ch).await;
                self.queue_downloads(jobs).await;
            }
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "resuming downloads failed")
            }
        }
    }

    async fn resend_pending(&mut self, run: &Running) {
        let account = self.account;
        match self
            .sh
            .store
            .read(move |c| local::pending_commands(c, account))
            .await
        {
            Ok(cmds) => {
                for (id, cmd) in cmds {
                    run.send(cmd.into_command(id)).await;
                }
            }
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "reading pending commands failed")
            }
        }
    }

    /// One envelope: validate, apply in one transaction (or one per history chunk), ack. While
    /// a link is in progress, durable envelopes must start with `AccountReady` (contract rule).
    async fn envelope(&mut self, env: Envelope, linked: &mut bool, relinking: bool) -> Flow {
        let valid = if *linked {
            env.validate()
        } else {
            env.validate_linking()
        };
        if let Err(v) = valid {
            tracing::warn!(account = self.account.0, error = %v, "backend broke the contract; envelope refused");
            if let Some(ack) = env.ack {
                ack.complete(Err(PersistError {
                    retryable: false,
                    detail: v.to_string(),
                }));
            }
            return Flow::Continue;
        }
        let Envelope { events, ack } = env;
        let Some(ack) = ack else {
            for e in events {
                if let Some(detail) = self.ephemeral(e, *linked).await {
                    return Flow::LoginFailed(detail);
                }
            }
            return Flow::Continue;
        };
        if relinking
            && !*linked
            && let Some(detail) = self.relink_mismatch(&events).await
        {
            ack.complete(Err(PersistError {
                retryable: false,
                detail: detail.clone(),
            }));
            return Flow::LoginFailed(detail);
        }
        let (arrived, count) = (std::time::Instant::now(), events.len());
        let (ch, result) = self.apply(events).await;
        match result {
            Ok(()) => {
                failpoint::hit(FAILPOINT_BEFORE_ACK);
                ack.complete(Ok(()));
                tracing::trace!(
                    target: PERF_TARGET,
                    name = "recv.commit",
                    account = self.account.0,
                    events = count,
                    messages = ch.incoming.len(),
                    notify = ch.notify.len(),
                    catch_up = ch.catch_up.len(),
                    took_us = u64::try_from(arrived.elapsed().as_micros()).unwrap_or(u64::MAX),
                );
                if ch.account_ready && !*linked {
                    *linked = true;
                    self.attempt = 0;
                }
            }
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "ingest failed; the backend forwards again");
                ack.complete(Err(PersistError {
                    retryable: !matches!(e, CoreError::Json(_)),
                    detail: e.to_string(),
                }));
            }
        }
        // Segments committed before a failing one stay committed; their redelivery is a no-op
        // for ingest, so their invalidations and notifications go out now or never.
        self.publish(ch).await;
        Flow::Continue
    }

    /// A relinked identity must be the account's primary own id (§4.3 step 6): the first of
    /// `own_ids` (Signal ACI, WhatsApp LID, Threema id). Sharing a secondary id is not enough:
    /// a phone number (Signal PNI, WhatsApp PN) can move to another account, and Desktop
    /// compares the ACI (`SD/ts/util/isRelinkingToSameAccount.std.ts`). Fails closed.
    async fn relink_mismatch(&self, events: &[BackendEvent]) -> Option<String> {
        let ready = events.iter().find_map(|e| match e {
            BackendEvent::AccountReady { own_ids, .. } => Some(own_ids.clone()),
            _ => None,
        })?;
        let account = self.account;
        let refused = || Some("this is a different account; add it as a new account".into());
        let known = match self
            .sh
            .store
            .read(move |c| local::own_ids(c, account))
            .await
        {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(account = self.account.0, error = %e, "reading own ids failed; relink refused");
                return refused();
            }
        };
        match (known.first(), ready.first()) {
            (None, _) => None,
            (Some(old), Some(new)) if old == new => None,
            _ => refused(),
        }
    }

    /// Applies durable events: live events in one high-priority job, each history batch in its
    /// own low-priority job (≤ 500 rows, §4.10), so other accounts' live traffic interleaves.
    /// Returns what the committed jobs changed, and the first failure (later jobs are skipped).
    async fn apply(&self, events: Vec<BackendEvent>) -> (Changes, Result<()>) {
        let mut all = Changes::default();
        for (prio, events) in segments(events) {
            let (account, scope) = (self.account, self.scope);
            let now = i64::try_from(self.sh.clock.now_ms()).unwrap_or(i64::MAX);
            let result = self
                .sh
                .store
                .write(prio, lane(account), move |conn| {
                    ingest::apply_events(conn, account, scope, now, events)
                })
                .await;
            match result {
                Ok(ch) => all.absorb(ch),
                Err(e) => return (all, Err(e)),
            }
        }
        (all, Ok(()))
    }

    /// Turns committed changes into UI events (§4.9 last row).
    async fn publish(&mut self, ch: Changes) {
        let account = self.account;
        self.sh.forget_media(&ch);
        self.outbox_dirty |= ch.outbox;
        if ch.expiring {
            self.sh.expiry_wake.notify_one();
        }
        if ch.ringing {
            self.sh.calls_wake.notify_one();
        }
        if ch.accounts {
            self.emit(UiEvent::AccountsChanged);
        }
        for (from, into) in &ch.merged {
            self.emit(UiEvent::ConversationMerged {
                from: *from,
                into: *into,
            });
        }
        if !ch.removed.is_empty() {
            self.emit(UiEvent::ConversationsRemoved {
                ids: ch.removed.iter().copied().collect(),
            });
        }
        if !ch.conversations.is_empty() {
            self.emit(UiEvent::ConversationsChanged {
                ids: ch.conversations.iter().copied().collect(),
            });
        }
        for (conv, ids) in &ch.messages {
            if !ch.timelines.contains(conv) {
                self.emit(UiEvent::MessagesChanged {
                    conv: *conv,
                    ids: ids.iter().copied().collect(),
                });
            }
        }
        for conv in &ch.timelines {
            self.emit(UiEvent::TimelineInvalidated { conv: *conv });
        }
        if ch.contacts {
            self.emit(UiEvent::ContactsChanged { account });
        }
        for (conv, user, _device) in ch.incoming {
            if self.typing.remove(&(conv, user.clone())) {
                self.emit(UiEvent::Typing {
                    account,
                    conv,
                    user,
                    on: false,
                    ttl: Duration::ZERO,
                });
            }
        }
        self.sh.notify(account, &ch.notify);
        self.sh.hold_calls(account, ch.held_calls);
        for conv in ch.catch_up {
            self.catch_up.0 = self.catch_up.0.saturating_add(1);
            self.catch_up.1.insert(conv);
        }
        if !self.syncing && self.catch_up.0 > 0 {
            // Catch-up rows that arrive outside Syncing (boundary already passed) are summarized
            // at once.
            let (n, convs) = std::mem::take(&mut self.catch_up);
            self.emit(UiEvent::CatchUpSummary {
                account,
                messages: n,
                conversations: u32::try_from(convs.len()).unwrap_or(u32::MAX),
            });
        }
        for notice in ch.notices {
            // A challenge a job of the backend met: the account state a challenged send
            // shows (§4.8), so the UI needs no second way to offer the answer.
            if let BackendNotice::ChallengeRequired { url, retry_after } = &notice {
                let shown = self
                    .dispatch
                    .announce_challenge(&self.sh, url.clone(), *retry_after);
                // Kept across a restart, like the challenge of a send.
                self.dispatch.sync_challenge(&self.sh).await;
                self.set_conn(shown).await;
            }
            self.emit(UiEvent::Notice { account, notice });
        }
        for (conv, progress, has_more) in ch.history {
            self.emit(UiEvent::HistoryProgress {
                account,
                conv,
                progress,
                has_more,
            });
        }
        let dir = self.sh.vault.blob_dir(self.uuid);
        if !ch.unlink.is_empty() {
            let (d, paths) = (dir.clone(), ch.unlink);
            let _ = tokio::task::spawn_blocking(move || attachments::unlink(&d, &paths)).await;
        }
        if !ch.derive.is_empty() {
            derive::spawn(&self.sh, account, self.scope, dir, ch.derive);
        }
        self.queue_downloads(ch.downloads).await;
    }

    /// Ephemeral events; returns a failure detail when a fresh login failed.
    async fn ephemeral(&mut self, e: BackendEvent, linked: bool) -> Option<String> {
        let account = self.account;
        match e {
            BackendEvent::Login(step) => {
                let failed = match &step.kind {
                    LoginStepKind::Failed { error } => Some(error.detail.clone()),
                    _ => None,
                };
                self.emit(UiEvent::LoginStep { account, step });
                if !linked {
                    return failed;
                }
            }
            BackendEvent::Typing {
                conv, by, on, ttl, ..
            } => {
                let key = conv;
                let resolved = self
                    .sh
                    .store
                    .read(move |c| query::conversation_by_key(c, account, &key))
                    .await;
                if let Ok(Some(conv)) = resolved {
                    if on {
                        self.typing.insert((conv, by.clone()));
                    } else {
                        self.typing.remove(&(conv, by.clone()));
                    }
                    self.emit(UiEvent::Typing {
                        account,
                        conv,
                        user: by,
                        on,
                        ttl,
                    });
                }
            }
            BackendEvent::AttachmentProgress {
                local,
                index,
                done,
                total,
            } => self.emit(UiEvent::Progress(Progress::Upload {
                message: local,
                index,
                done,
                total,
            })),
            BackendEvent::DownloadProgress {
                attachment,
                done,
                total,
            } => self.emit(UiEvent::Progress(Progress::Download {
                attachment,
                done,
                total,
            })),
            BackendEvent::TransmitProgress { local, progress } => {
                self.emit(UiEvent::Progress(Progress::Transmit {
                    message: local,
                    progress,
                }));
            }
            BackendEvent::HistoryTransfer(state) => {
                self.emit(UiEvent::HistoryTransfer { account, state });
            }
            _ => {}
        }
        None
    }

    /// Applies what the actor still sends after it reported its exit, until its sender closes
    /// (bounded by the shutdown grace). The rules of the running loop hold: an actor that is
    /// still linking is ingested only from its `AccountReady` on, a relink only when it is the
    /// same account, and after a refusal nothing more.
    async fn drain(&mut self, run: &mut Running, linked: &mut bool, relinking: bool) {
        let grace = self.sh.timeouts.shutdown_grace;
        let deadline = tokio::time::Instant::now() + grace;
        let mut refused = false;
        while let Ok(Some(env)) = tokio::time::timeout_at(deadline, run.events.recv()).await {
            if refused {
                refuse(env);
            } else if let Flow::LoginFailed(_) = self.envelope(env, linked, relinking).await {
                refused = true;
            }
        }
    }

    /// `Shutdown`, then the grace period, then the cancel token. What the actor still sends
    /// meanwhile is treated as `ingest` says. An actor the core refused is not ingested: its
    /// forwarder sends the refused `AccountReady` again on its own when the refused batch had
    /// more rows, and applying that on the way out would make the account the one that was
    /// just refused.
    async fn stop(&mut self, run: &mut Running, mut ingest: Ingest) {
        run.send(Command::Shutdown).await;
        let grace = self.sh.timeouts.shutdown_grace;
        let mut deadline = tokio::time::Instant::now() + grace;
        let mut cancelled = false;
        while !run.exit_done {
            tokio::select! {
                _ = &mut run.exited => {
                    run.exit_done = true;
                }
                ev = run.events.recv() => match ev {
                    Some(env) => ingest = self.ingest_stopping(env, ingest).await,
                    None => {
                        // Sender gone: the exit report follows (or the actor died).
                        let _ = tokio::time::timeout(grace, &mut run.exited).await;
                        run.exit_done = true;
                    }
                },
                _ = tokio::time::sleep_until(deadline) => {
                    if cancelled {
                        tracing::warn!(account = self.account.0, "actor ignored its cancel token; abandoned");
                        break;
                    }
                    tracing::warn!(account = self.account.0, "actor ignored Shutdown; cancelling");
                    run.cancel.cancel();
                    cancelled = true;
                    deadline = tokio::time::Instant::now() + grace;
                }
            }
        }
        run.cancel.cancel();
        self.join(run).await;
    }

    /// One envelope of an actor that is stopping; returns how the next one is treated.
    async fn ingest_stopping(&mut self, env: Envelope, ingest: Ingest) -> Ingest {
        match ingest {
            Ingest::Refused => {
                refuse(env);
                Ingest::Refused
            }
            Ingest::Linked => {
                let mut linked = true;
                let _ = self.envelope(env, &mut linked, false).await;
                Ingest::Linked
            }
            Ingest::Linking { relinking } => {
                let mut linked = false;
                match self.envelope(env, &mut linked, relinking).await {
                    Flow::LoginFailed(_) => Ingest::Refused,
                    Flow::Continue if linked => Ingest::Linked,
                    Flow::Continue => ingest,
                }
            }
        }
    }

    /// Joins the actor's OS thread off the runtime (bounded).
    async fn join(&self, run: &mut Running) {
        if let Some(t) = run.thread.take() {
            let grace = self.sh.timeouts.shutdown_grace;
            let joined =
                tokio::time::timeout(grace, tokio::task::spawn_blocking(move || t.join())).await;
            if joined.is_err() {
                tracing::warn!(
                    account = self.account.0,
                    "account thread did not finish; detached"
                );
            }
        }
    }

    async fn after_exit(
        &mut self,
        exit: Exit,
        linked: bool,
        relinking: bool,
        start: StartMode,
    ) -> Next {
        // A panic payload is free text from anywhere in the backend: never logged (§9.1).
        let (kind, detail) = match &exit {
            Exit::Shutdown => ("shutdown", None),
            Exit::LoggedOut { reason } => ("logged out", Some(reason.as_str())),
            Exit::Failed(e) => ("failed", Some(e.detail.as_str())),
            Exit::Panicked(_) => ("panicked", None),
        };
        tracing::info!(
            account = self.account.0,
            exit = kind,
            detail,
            "account actor exited"
        );
        self.set_conn(ConnState::Offline { retry_at_ms: None })
            .await;
        if !linked {
            let detail = match &exit {
                Exit::Failed(e) => e.detail.clone(),
                Exit::Panicked(_) => "the backend crashed while linking".into(),
                Exit::LoggedOut { reason } => reason.clone(),
                Exit::Shutdown => "the backend stopped while linking".into(),
            };
            return self.login_failed(relinking, detail).await;
        }
        match exit {
            Exit::LoggedOut { reason } => {
                self.logged_out(&reason).await;
                Next::Plan(Plan::Idle)
            }
            // The backend cannot run this account the way it was started (Signal: the
            // account belongs to another server than the one this start names, §9.2). It
            // would say the same at every restart, so there is none; its words go to the
            // user, and nothing of the account is touched.
            Exit::Failed(e) if matches!(e.kind, kourier_model::ErrorKind::Unsupported) => {
                tracing::error!(
                    account = self.account.0,
                    detail = %e.detail,
                    "the backend refuses this account as started; not restarting"
                );
                self.emit(UiEvent::AccountError {
                    account: self.account,
                    detail: e.detail,
                    restartable: false,
                });
                Next::Plan(Plan::Halted)
            }
            // A backend exits with Shutdown only when told to; anything unasked is a failure.
            Exit::Shutdown | Exit::Failed(_) | Exit::Panicked(_) => {
                let mode = if start == StartMode::Fresh {
                    StartMode::Resume
                } else {
                    start
                };
                self.backoff(mode).await
            }
        }
    }

    /// Records a failure and waits before the restart; gives up after too many (§4.3 step 4).
    async fn backoff(&mut self, mode: StartMode) -> Next {
        let now = self.sh.clock.now_ms();
        let window = u64::try_from(self.sh.restart.window.as_millis()).unwrap_or(u64::MAX);
        recent_failures(&mut self.failures, now, window);
        // A quiet window since the last failure ends the streak (else a backend that fails once
        // a day would reach the delay cap after a week).
        if self.failures.is_empty() {
            self.attempt = 0;
        }
        self.failures.push_back(now);
        if self.failures.len() >= self.sh.restart.max_failures {
            tracing::warn!(
                account = self.account.0,
                failures = self.failures.len(),
                "giving up on the account"
            );
            self.emit(UiEvent::AccountError {
                account: self.account,
                detail: format!(
                    "the {} backend failed {} times in a row",
                    self.network.kind(),
                    self.failures.len()
                ),
                restartable: true,
            });
            return Next::Plan(Plan::Halted);
        }
        self.attempt = self.attempt.saturating_add(1);
        let seed = kourier_model::stable_hash(self.uuid.0.as_bytes());
        let delay = self.sh.restart.delay(self.attempt, seed);
        let wake = now.saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX));
        self.set_conn(ConnState::Offline {
            retry_at_ms: Some(wake),
        })
        .await;
        let mut sleep = self.sh.clock.sleep_until(wake);
        loop {
            tokio::select! {
                _ = &mut sleep => return Next::Plan(Plan::Run(mode)),
                c = self.ctl.recv() => match c {
                    None => return Next::Stop,
                    Some(Control::Shutdown(tx)) => {
                        let _ = tx.send(());
                        return Next::Stop;
                    }
                    Some(Control::Remove) => {
                        self.remove(None).await;
                        return Next::Stop;
                    }
                    Some(Control::Restart) => return Next::Plan(Plan::Run(mode)),
                    // The sleep did not run while the machine slept: wait for what is left.
                    Some(Control::Hint(Hint::Resumed)) => {
                        sleep = self.sh.clock.sleep_until(wake);
                    }
                    Some(other) => self.without_actor(other, "restarting").await,
                },
            }
        }
    }

    /// A fresh link failed or was cancelled: the account goes away (§4.3 step 2). A relink that
    /// failed leaves the account logged out with no credentials.
    async fn login_failed(&mut self, relinking: bool, detail: String) -> Next {
        tracing::info!(
            account = self.account.0,
            relinking,
            "login did not complete"
        );
        if relinking {
            if let Err(e) = self.sh.vault.wipe_credentials(self.uuid).await {
                tracing::warn!(account = self.account.0, error = %e, "wiping relink credentials failed");
            }
            self.emit(UiEvent::AccountError {
                account: self.account,
                detail,
                restartable: false,
            });
            self.emit(UiEvent::AccountsChanged);
            return Next::Plan(Plan::Idle);
        }
        self.discard().await;
        Next::Stop
    }

    /// Deletes an account that never finished linking: secret, directory, row.
    async fn discard(&mut self) {
        if let Err(e) = self.sh.vault.shred(self.uuid).await {
            tracing::debug!(account = self.account.0, error = %e, "shred of an unlinked account");
        }
        if let Err(e) = self.sh.vault.remove_dir(self.uuid).await {
            tracing::warn!(account = self.account.0, error = %e, "removing the account directory failed");
        }
        let account = self.account;
        if let Err(e) = self
            .sh
            .store
            .write(Priority::High, lane(account), move |c| {
                local::delete_account(c, account)
            })
            .await
        {
            tracing::warn!(account = self.account.0, error = %e, "deleting the account row failed");
        }
        self.emit(UiEvent::AccountRemoved { account });
        self.emit(UiEvent::AccountsChanged);
    }

    /// Unlinked (§4.3 step 5): wipe the credentials; history stays readable.
    async fn logged_out(&mut self, reason: &str) {
        if let Err(e) = self.sh.vault.wipe_credentials(self.uuid).await {
            tracing::warn!(account = self.account.0, error = %e, "wiping credentials failed");
        }
        let (account, reason_owned) = (self.account, reason.to_owned());
        if let Err(e) = self
            .sh
            .store
            .write(Priority::High, lane(account), move |c| {
                local::logged_out(c, account, &reason_owned)
            })
            .await
        {
            tracing::warn!(account = self.account.0, error = %e, "recording the logout failed");
        }
        self.set_conn(ConnState::LoggedOut {
            reason: reason.to_owned(),
        })
        .await;
        self.emit(UiEvent::AccountsChanged);
    }

    /// `Logout` for a running actor, best effort (§4.3 step 7: at most [`Timeouts::logout`]):
    /// a backend that has a device on the network tells the network to forget it and exits
    /// `LoggedOut`; then the actor is stopped for sure. What it still sends meanwhile is
    /// treated as `ingest` says. Used when the account is removed, and when the core turns
    /// away an actor that may have finished its link (a cancelled login, a refused relink):
    /// the core has not accepted that device, and nobody else would ever log it out.
    async fn log_out(&mut self, run: &mut Running, mut ingest: Ingest) {
        run.send(Command::Logout).await;
        let deadline = tokio::time::Instant::now() + self.sh.timeouts.logout;
        let mut events_open = true;
        while !run.exit_done {
            tokio::select! {
                _ = &mut run.exited => run.exit_done = true,
                ev = run.events.recv(), if events_open => match ev {
                    Some(env) => ingest = self.ingest_stopping(env, ingest).await,
                    None => events_open = false,
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
        if !run.exit_done {
            // The account goes with everything it has: what arrives now changes nothing.
            self.stop(run, Ingest::Refused).await;
        } else {
            run.cancel.cancel();
            self.join(run).await;
        }
    }

    /// Remove (§4.3 step 7): best-effort logout, shutdown, shred, row, WAL truncate, directory.
    /// `run`: the running actor and whether the core accepted its identity. What a linked
    /// actor still sends during its logout is applied (its forwarder drains); an actor that is
    /// still linking is not ingested: its `AccountReady` would make the account `Active` (and,
    /// in a relink, whoever it linked as) while it is being removed, and would stay so if the
    /// shred then failed.
    async fn remove(&mut self, run: Option<(Running, bool)>) {
        let account = self.account;
        let _ = self
            .sh
            .store
            .write(Priority::High, lane(account), move |c| {
                local::set_state(c, account, &AccountState::Removing)
            })
            .await;
        self.emit(UiEvent::AccountsChanged);
        if let Some((mut run, linked)) = run {
            let ingest = if linked {
                Ingest::Linked
            } else {
                Ingest::Refused
            };
            self.log_out(&mut run, ingest).await;
        }
        if let Err(e) = self.sh.vault.shred(self.uuid).await {
            // Without the shred the removal promise (§5.2) does not hold: the row stays
            // `Removing` (hidden from the lists) and startup tries again.
            tracing::warn!(account = self.account.0, error = %e, "shredding the account secret failed; removal retried at the next start");
            self.emit(UiEvent::AccountError {
                account,
                detail: format!("the account could not be removed completely ({e}); it is retried at the next start"),
                restartable: false,
            });
            self.emit(UiEvent::AccountsChanged);
            return;
        }
        if let Err(e) = self
            .sh
            .store
            .write(Priority::High, lane(account), move |c| {
                local::delete_account(c, account)
            })
            .await
        {
            tracing::warn!(account = self.account.0, error = %e, "deleting the account row failed");
        }
        if let Err(e) = self.sh.vault.remove_dir(self.uuid).await {
            tracing::warn!(account = self.account.0, error = %e, "removing the account directory failed");
        }
        self.emit(UiEvent::AccountRemoved { account });
        self.emit(UiEvent::AccountsChanged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kourier_model::{CmdId, ConvKey};

    fn history() -> BackendEvent {
        BackendEvent::HistoryBatch {
            conv: ConvKey::Group("wag:1".into()),
            messages: Vec::new(),
            has_more: false,
            progress: None,
        }
    }

    fn live(n: i64) -> BackendEvent {
        BackendEvent::CommandPersisted { cmd: CmdId(n) }
    }

    #[test]
    fn history_batches_become_their_own_low_priority_jobs() {
        let segs = segments(vec![live(1), live(2), history(), history(), live(3)]);
        let shape: Vec<(Priority, usize)> = segs.iter().map(|(p, e)| (*p, e.len())).collect();
        assert_eq!(
            shape,
            [
                (Priority::High, 2),
                (Priority::Low, 1),
                (Priority::Low, 1),
                (Priority::High, 1)
            ]
        );
        assert!(segments(Vec::new()).is_empty());
    }

    /// The failure window after a clock change (§9.6): failures recorded on a clock that
    /// was ahead do not count as recent once it is set back, and old ones leave as before.
    #[test]
    fn failures_from_a_clock_that_was_ahead_do_not_count() {
        let window = 600_000;
        let mut f: VecDeque<u64> = [1_000, 500_000, 590_000].into();
        recent_failures(&mut f, 601_001, window);
        assert_eq!(f, [500_000, 590_000], "the oldest left the window");
        // The clock was a day ahead when these were recorded, then it was fixed.
        let day = 86_400_000;
        let mut f: VecDeque<u64> = [day + 1, day + 2, day + 3, 5_000].into();
        recent_failures(&mut f, 6_000, window);
        assert_eq!(f, [5_000]);
        recent_failures(&mut f, 5_000, window);
        assert_eq!(f, [5_000], "a failure right now stays");
        assert!(
            Hint::Resumed > Hint::NetworkChanged,
            "a resume covers a change"
        );
    }

    #[test]
    fn backoff_doubles_with_bounded_jitter_and_a_cap() {
        let p = RestartPolicy::default();
        for attempt in 1..=12u32 {
            let d = p.delay(attempt, 42).as_millis();
            let base = (1000u128 << (attempt - 1)).min(300_000);
            assert!(
                d >= base && d <= (base + base / 4).min(300_000),
                "{attempt}: {d}"
            );
        }
        assert_eq!(p.delay(40, 7), p.max);
        assert_eq!(p.delay(3, 9), p.delay(3, 9), "deterministic for a seed");
    }
}
