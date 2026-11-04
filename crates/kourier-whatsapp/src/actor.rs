//! The account actor (W2, chunk C4a): it opens the account's store, serves the core's commands,
//! drives the login machine and the connection over a [`Session`], and ships `AccountReady` and
//! whatever follows to the core through `core_outbox`.
//!
//! Start rules (the Signal actor's, `kourier-signal/src/actor.rs`):
//! - `Fresh` with a device row in the file is refused: a fresh link never lands on an existing
//!   device (it would silently replace the keys of a linked account).
//! - `Resume` and `Rebuild` without the file are refused: `kourier_db::open` creates a missing file,
//!   and a resumed account must never become a new device by accident.
//! - A key that does not open the file ends the actor with `Exit::Failed`, with the file untouched
//!   (architecture §5.2: a start that cannot open an account must not decide its fate).
//! - `Rebuild` (the core's `core.db` was rebuilt) states `AccountReady` again from the stored
//!   device, through `core_outbox` (critic H6), BEFORE the forwarder and stage 2 are started, so
//!   nothing this run writes leads it (fix F10); rows a previous run left keep their lower `seq`. The
//!   same zone forgets every chat's stated features (the rebuilt core lost them), and the start sweep
//!   of every start that is not a fresh link states what is missing or stale (W5a, D9, critic M4).
//!
//! The client:
//! - It is made by the [`SessionFactory`] at the first `Connect` of an account with a paired device,
//!   or at `StartLogin`: never at the actor's start (decision L2/M4), and not at all while the
//!   environment is not Production or a proxy is set (nothing may be connected around the
//!   configuration, critic L-b and L7).
//! - `Disconnect` pauses it and `Connect` resumes it (critic M3). A terminal verdict (a ban, an
//!   outdated version, a replaced session, a refusal) stops the client AND waits for it to release
//!   the store; only then can a new `Connect` make another, so two clients never share one store.
//!   System hints (`NetworkChanged`, `Resumed`) never revive a held account (Signal's `held`).
//! - A write the account cannot go on without failing (the hook's inbox commit, the handler's
//!   `core_outbox` write) ends that client for good (`signal_shutdown_sync` is terminal, decision D5):
//!   the client announces it as `WaSignal::WriteFailed`, the actor stops it, waits for the store's
//!   release, shows `Offline` with the time of the next try and makes a NEW client after a backoff
//!   (5 s, doubling to 5 min; a client that stayed up for two minutes resets it), so a full disk
//!   never loops hot. `Disconnect`, `Connect` and `Logout` end the wait. A stop whose release did not
//!   come in time is remembered (`unreleased`): no client is made while the store is still held, the
//!   wait looks again every 5 s (fix F9: never two clients on one store, also in the automatic rebuild).
//! - `Online` is the catch-up machine's ([`crate::catchup`], decision D2): a connection is `Syncing`
//!   until the offline drain is durable (Kourier's own boundary), stage 2 is through it and the core
//!   acknowledged what it wrote; only `Connected` AND that make `Online`, in either order. What the
//!   core is told waits for nobody else: the forwarder ships rows in `seq` order from the start, and
//!   `AccountReady` leads them: a link HOLDS the receive path ([`crate::hold`]) from `StartLogin` until
//!   `AccountReady` is recorded - stage 2 does not step and the handler's write waits - so nothing of the
//!   new connection can be appended below it, however the actor's turn and the connection interleave
//!   (fix F5; the core refuses a row that leads a link for good). A `Rebuild` states it before the
//!   forwarder starts (fix F10).
//! - A logout the server announces ends the actor with `Exit::LoggedOut`; one Kourier asked for is
//!   not an event (translate drops it), and `Command::Logout` reports `LogoutReport` exactly once.
//! - The reach-out lock is stated at every connect: pulled at the first connect of a run (retried
//!   with a back-off while the answer does not come, saying nothing meanwhile), stated again from
//!   what is known at the later connects, and changed by the network's pushes (critic M1, L-e).
//!   The typed facts that today's contract has no event for go to [`Facts`].
//!
//! Shutdown order, on every path: the pair-code request is withdrawn, the client is stopped (its
//! store released; at a clean quit a start-up resync that runs gets a moment to end BESIDE the waits for announced chunks
//! and for acks (one deadline, F7), then the client is paused first
//! and, if the account was quiet, the app-state dirty flags that nothing owes a resync are cleared: rule (b) in
//! `crate::appstate`), stage 2 stops (a step in flight
//! finishes), the history driver stops (a row in flight finishes, its feeder's thread is joined) and the history
//! stage is closed (at a logged-out exit it is deleted as well: a quit, a failure and a removal keep it), the
//! outbox is given a moment to hand the core what it still has, the forwarder stops, then the owner is closed
//! (queued jobs finish), then the `Exit` is reported - so the writer thread never outlives the actor, and a
//! stopped client never finds its store closed.

use std::future::{Future, pending};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEnv, BackendEvent, Clock, Command, Envelope, Exit, Sleep,
    StartMode,
};
use kourier_db::DbKey;
use kourier_model::{
    AccountHold, AttachmentId, BackendError, ConnState, DeviceLogout, DownloadId, LoginStep,
    LoginStepKind, LogoutReport,
};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use wacore::store::traits::DeviceStore;

use crate::NetParts;
use crate::appstate::startup::{self, Resyncer};
use crate::appstate::{self, Asking, Quiet};
use crate::blocklist;
use crate::catchup::{self, CatchUp, Phase, Shown};
use crate::events::EventWriter;
use crate::facts::{Facts, LogFacts};
use crate::groups;
use crate::history::capture::Capture;
use crate::history::driver::{self, Driver};
use crate::history::lost;
use crate::history::stage::LazyStage;
use crate::inbox::{self, Inbox};
use crate::link::{Gate, Login, Out, Ready, Rules};
use crate::map::OwnIds;
use crate::merges;
use crate::outbox;
use crate::send;
use crate::send::hold::{HoldView, Recheck};
use crate::session::{RealFactory, Receive, Released, Session, SessionFactory, Wiring};
use crate::stage2::{Stage2, Timing};
use crate::states::{self, WaState};
use crate::store::{KourierStore, StoreOwner};
use crate::tap::Tap;
use crate::translate::{HoldState, Typing, WaSignal};

/// What a request W1 and W2 do not carry out yet is told.
const NOT_DONE: &str = "WhatsApp does not do this yet";
/// How long a "typing" lasts without a refresh before the core clears it (an estimate in the range
/// of WhatsApp Web's own timeout; the contact's next chat state refreshes it).
pub(crate) const TYPING_TTL: Duration = Duration::from_secs(25);
/// What a download request is answered with until W7: the file cannot be fetched yet, but may be later.
const DOWNLOAD_NOT_DONE: &str = "downloads from WhatsApp are not done yet";

/// How long a stopping actor lets the forwarder hand the core what it still has.
#[cfg(not(test))]
const SHUTDOWN_DRAIN: Duration = Duration::from_secs(2);
#[cfg(test)]
const SHUTDOWN_DRAIN: Duration = Duration::from_millis(300);

/// How long a stopping actor waits for the logout of a refused pairing that is still under way (the
/// logout itself is bounded by the session; a quit must not wait for all of it).
#[cfg(not(test))]
const REFUSED_LOGOUT_GRACE: Duration = Duration::from_secs(5);
#[cfg(test)]
const REFUSED_LOGOUT_GRACE: Duration = Duration::from_millis(300);

/// How long a normal quit waits for the acks of frames already written (D19, critic M1): the attempts end, and
/// their outcomes are in the ledger, before the client stops. An attempt still before its frame does not count: the
/// quit takes it at once (fix F16, `send::stop`). It runs beside the wait for announced history chunks
/// (`lost::QUIT_WAIT`, 15 s), so the quit stays inside the network's `shutdown_grace` (20 s).
#[cfg(not(test))]
const QUIT_ACK_WAIT: Duration = Duration::from_secs(10);
#[cfg(test)]
const QUIT_ACK_WAIT: Duration = Duration::from_secs(5);

/// The first and the longest wait before the reach-out lock is asked for again.
const HOLD_RETRY_MIN: Duration = Duration::from_secs(5);
const HOLD_RETRY_MAX: Duration = Duration::from_secs(300);

/// How long the first `Connected` of a run waits for the reach-out lock before it shows `Syncing` (D8, brief
/// §2.7): the contract wants the hold stated before the first `Syncing`, and the core may send from `Syncing` on.
/// A pull that takes longer goes on off the loop; sends wait for it ([`send::attempt::Gate::Unknown`]).
const HOLD_FIRST_WAIT: Duration = Duration::from_secs(10);

/// THE END OF "UNKNOWN" (D8, critic H3): after the pull has gone unanswered this long since the run first asked,
/// "unknown" counts as "no hold" for the send gate (D7 and the 463 floor guard what remains); the pull's retry goes
/// on, and an answer still states the hold.
const HOLD_UNKNOWN_END: Duration = Duration::from_secs(600);

/// The first and the longest wait before a new client is made after a failed write (decision D5),
/// and how long a client must have stayed up for its failure to count as a first one again.
const REBUILD_MIN: Duration = Duration::from_secs(5);
const REBUILD_MAX: Duration = Duration::from_secs(300);
const STABLE_RUN: Duration = Duration::from_secs(120);

/// The actor of a production account. `parts` is what the real clients are made from, one pair per
/// client, when the actor makes one (the first `Connect` or `StartLogin`).
pub(crate) fn run(ctx: AccountContext, parts: NetParts) -> Exit {
    run_with(ctx, Box::new(RealFactory::new(parts)), Box::new(LogFacts))
}

/// The actor over a [`SessionFactory`] and a [`Facts`] sink: the production path with the real
/// ones, the tests with doubles.
pub(crate) fn run_with(
    ctx: AccountContext,
    factory: Box<dyn SessionFactory>,
    facts: Box<dyn Facts>,
) -> Exit {
    let AccountContext {
        uuid,
        start,
        backend_db,
        db_key,
        blob_dir,
        commands,
        events,
        conn,
        cancel,
        clock,
        config,
        ..
    } = ctx;
    let key = DbKey::from_bytes(*db_key.expose());
    drop(db_key);
    if start != StartMode::Fresh && !backend_db.exists() {
        return Exit::Failed(BackendError::internal(
            "the account's database is missing; it is not made again on a resume",
        ));
    }
    // An upload that was running when the process died left its sealed temporary file; its key died with the
    // process (W5b, G3).
    let upload_tmp = send::tmp::dir_beside(&backend_db);
    send::tmp::clean_at_start(&upload_tmp);
    let owner = match StoreOwner::open(&backend_db, &key, &format!("wa-db-{}", uuid.short())) {
        Ok(owner) => owner,
        Err(e) => return Exit::Failed(BackendError::internal(e.to_string())),
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => {
            owner.close();
            return Exit::Failed(BackendError::internal("the actor's runtime did not start"));
        }
    };
    let store = owner.store();
    // Touches no file: the stage is made by the first captured chunk (or found, with its backlog).
    let stage = Arc::new(LazyStage::new(&backend_db, &key));
    if start == StartMode::Fresh {
        // A fresh link never inherits a stage: one left by a logout whose removal did not finish is not this
        // account's (W4 C18).
        stage.clear_leftover();
    }
    let exit = runtime.block_on(async {
        if start == StartMode::Fresh {
            match store.exists().await {
                Ok(false) => {}
                Ok(true) => {
                    return Exit::Failed(BackendError::internal(
                        "a fresh start found a device in the account's database",
                    ));
                }
                Err(_) => {
                    return Exit::Failed(BackendError::internal("the account's database failed"));
                }
            }
        }
        // `Rebuild` states `AccountReady` again BEFORE the actor exists: its forwarder and stage 2
        // start in `Actor::new`, so nothing the new run writes can lead it (fix F10).
        if start == StartMode::Rebuild
            && let Err(error) = state_ready_again(&store, clock.now_ms()).await
        {
            return Exit::Failed(error);
        }
        // The start-up resync (W6a1, D2, critic B1): its reasons are the dirty set READ NOW, before the client or
        // the handler exist, so nothing this run writes can be one. A link asks nothing of Kourier's own: the books
        // are stamped in the zone that records `AccountReady` for the pairing (`outbox::append_ready`, fix F6), not
        // here, where the link page may have been open for hours before the scan.
        let mut resync_reasons = Vec::new();
        if start != StartMode::Fresh {
            match startup::open(&store).await {
                Ok(reasons) => resync_reasons = reasons,
                Err(e) => {
                    tracing::warn!(error = %e, "the dirty app-state collections could not be read");
                    // Nothing is known: every flag stands until a start that can read them.
                    let all: Vec<String> = appstate::EVENTS_WRITTEN
                        .iter()
                        .map(|name| (*name).to_owned())
                        .collect();
                    store.activity().owe(&all);
                }
            }
        }
        // The history the phone announced in an earlier run and this build never captured is counted lost now,
        // before the client or stage 2 exist: what is found is none of this run's (W4 C20).
        let mut lost_notice = None;
        if start != StartMode::Fresh {
            let now = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
            match lost::count_at_start(&store, &stage, now).await {
                Ok(0) => {}
                Ok(counted) => {
                    tracing::warn!(counted, "history chunks the phone announced never arrived")
                }
                Err(e) => {
                    tracing::warn!(error = %e, "the lost history chunks could not be counted")
                }
            }
            // The send outcomes are needed only between a reply and the core's commit of it (D4): the rows older
            // than 14 days go at every start.
            match store
                .write("forget old send outcomes", move |tx| {
                    send::ledger::sweep(tx, now)
                })
                .await
            {
                Ok(0) => {}
                Ok(gone) => tracing::debug!(gone, "old send outcomes were forgotten"),
                Err(e) => {
                    tracing::warn!(error = %e, "the old send outcomes could not be forgotten")
                }
            }
            // The upload records of sent files are kept 30 days (W5b, G4): the caption edit and a later re-upload
            // request read them; older ones go at every start.
            match store
                .write("forget old upload records", move |tx| {
                    send::record::sweep(tx, now)
                })
                .await
            {
                Ok(0) => {}
                Ok(gone) => tracing::debug!(gone, "old upload records were forgotten"),
                Err(e) => {
                    tracing::warn!(error = %e, "the old upload records could not be forgotten")
                }
            }
            // A receipt job that found no connection for 14 days never will (fix F14).
            match store
                .write("forget old receipt jobs", move |tx| {
                    send::receipts::sweep(tx, now)
                })
                .await
            {
                Ok(0) => {}
                Ok(gone) => tracing::debug!(gone, "old receipt jobs were forgotten"),
                Err(e) => {
                    tracing::warn!(error = %e, "the old receipt jobs could not be forgotten")
                }
            }
            // Which chat a call's line was filed under is kept 30 days (W6a1, A6, critic L6): the phone's call log
            // names the call again only while it is recent; older rows go at every start.
            match store
                .write("forget old call lines", move |tx| {
                    crate::store::call_lines::sweep(tx, now)
                })
                .await
            {
                Ok(0) => {}
                Ok(gone) => tracing::debug!(gone, "old call lines were forgotten"),
                Err(e) => {
                    tracing::warn!(error = %e, "the old call lines could not be forgotten")
                }
            }
            // The chats whose features were never stated or are stale (a new version, a fact that changed, a
            // `Rebuild`) are stated before anything of this run is written (W5a, D9, critic M4).
            match outbox::append_statements(&store, now).await {
                Ok(0) => {}
                Ok(stated) => tracing::debug!(stated, "chat features were stated at the start"),
                Err(e) => {
                    tracing::warn!(error = %e, "the chats' features could not be stated")
                }
            }
            // The person is told once per start, until the core has taken the notice (R-Q2). Its row is appended
            // before the forwarder exists, so it ships with the rows a previous run left.
            match lost::append_notice(&store, now).await {
                Ok(told) => lost_notice = told,
                Err(e) => {
                    tracing::warn!(error = %e, "the lost history could not be put to the core")
                }
            }
        }
        let mut actor = Actor::new(Parts {
            commands,
            events,
            conn,
            cancel,
            clock,
            config,
            store,
            stage,
            factory,
            facts,
            lost_notice,
            blob_dir,
            upload_tmp,
            resync_reasons,
        });
        let exit = actor.event_loop().await;
        actor.finish(exit).await
    });
    // The store is closed on every path, before the exit is reported.
    owner.close();
    exit
}

/// What the actor is made of.
struct Parts {
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Envelope>,
    conn: watch::Sender<ConnState>,
    cancel: CancellationToken,
    clock: Arc<dyn Clock>,
    config: BackendConfig,
    store: KourierStore,
    /// The history stage of the account (W4), opened when the first chunk is captured or one is on disk.
    stage: Arc<LazyStage>,
    factory: Box<dyn SessionFactory>,
    facts: Box<dyn Facts>,
    /// The notice of the lost history this start put to the core, until the core has taken it.
    lost_notice: Option<lost::Told>,
    /// The core's blob directory: the files a message sends (W5b).
    blob_dir: PathBuf,
    /// The account's `upload-tmp` directory (W5b, G3), emptied before the actor was made.
    upload_tmp: PathBuf,
    /// The collections the store held dirty when it opened (`startup::open`): the start-up resync's reasons.
    resync_reasons: Vec<String>,
}

/// The client of the account and what the actor knows about it.
struct Client {
    session: Arc<dyn Session>,
    /// The client's signals; `None` once its sender is gone.
    signals: Option<mpsc::UnboundedReceiver<WaSignal>>,
    /// `Disconnect` paused it.
    paused: bool,
}

/// The reach-out lock as the actor knows it.
struct HoldBook {
    /// Whether an answer to the pull came in this run.
    pulled: bool,
    /// What the actor knows of the lock (the last statement: a hold, none, or unknown; when the run first asked,
    /// the clock THE END OF "UNKNOWN" (D8) counts from), SHARED with the attempts and the typing states, which
    /// look at it again right before they write (fix F6).
    view: HoldView,
    /// The pull in flight (it runs off the loop: only the first `Connected` waits for it, and only up to
    /// [`HOLD_FIRST_WAIT`]).
    pull: Option<tokio::task::JoinHandle<Result<HoldState, BackendError>>>,
    /// How many pushes of the lock this run has seen, and how many it had seen when the pull in flight was asked: a
    /// pull answer is older than a push that came after it was asked, and must not overwrite it (fix F6).
    pushes: u64,
    asked_pushes: u64,
    /// Whether the end of "unknown" was logged in this run (once).
    unknown_ended: bool,
    /// The wait before the pull is asked again after it failed.
    retry: Option<Sleep>,
    backoff: Duration,
}

/// The new client after a failed write: the wait that is running and the wait the next failure gets.
struct Rebuild {
    /// Set while the account waits for the next try; no client exists then.
    sleep: Option<Sleep>,
    /// The wait for the next failure (doubles with each failure that follows a short run).
    next: Duration,
    /// When the client that runs was started (the actor's clock, ms); `None` without one.
    started_ms: Option<u64>,
}

/// What woke the loop.
enum Next {
    Cancelled,
    Command(Option<Command>),
    Signal(Option<WaSignal>),
    Acked(bool),
    HoldRetry,
    /// The pull of the reach-out lock answered (or its task ended).
    HoldPulled(Result<HoldState, BackendError>),
    /// The wait after a failed write is over: a new client may be made.
    Rebuild,
    /// Stage 2 committed a step.
    Stage2Progress,
    /// The catch-up machine's wait is over (upstream's completion did not come, or a read failed).
    CatchUpTimer,
    /// The logout of a refused pairing ended.
    RefusedLogout(DeviceLogout),
}

/// The logout of a refused pairing while it is under way: the session's one shared answer, awaited
/// by the loop as one more thing to wake it, never inside the handling of a signal.
type PendingLogout = Pin<Box<dyn Future<Output = DeviceLogout> + Send>>;

struct Actor {
    commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Envelope>,
    conn: watch::Sender<ConnState>,
    cancel: CancellationToken,
    clock: Arc<dyn Clock>,
    config: BackendConfig,
    store: KourierStore,
    factory: Box<dyn SessionFactory>,
    facts: Box<dyn Facts>,
    client: Option<Client>,
    login: Option<Login>,
    /// A terminal verdict, `Disconnect` or a client that could not start: system hints do not
    /// reconnect, `Command::Connect` does.
    held: bool,
    /// The connection is up (for the reach-out lock's retry).
    online: bool,
    hold: HoldBook,
    /// The receive path every client of the account is wired to.
    receive: Receive,
    /// The wait for a new client after a failed write.
    rebuild: Rebuild,
    /// Where the connection is on its way from `Syncing` to `Online` (W3a; it owns `Online`).
    catch: CatchUp,
    catch_timing: catchup::Timing,
    /// The machine's wait: upstream's completion (`catch_timing.backlog_wait`) or a new look after a
    /// failed read (`catch_timing.recheck`).
    catch_timer: Option<Sleep>,
    /// Stage 2's committed steps, which move the machine through the boundary.
    stage2_progress: watch::Receiver<u64>,
    /// The history driver (W4): delivers the captured chunks to the core, row by row, behind the live path's
    /// own rows. Stopped in `finish` with stage 2.
    history: Driver,
    /// What wakes the driver besides a captured chunk and the forwarder's acks: the link hold opening.
    history_wake: Arc<Notify>,
    /// The history stage, for its removal when the account is logged out (`finish`).
    stage: Arc<LazyStage>,
    /// The notice of the lost history this start put to the core: the person counts as told when the core took its row.
    lost_notice: Option<lost::Told>,
    /// When this run began (the clock's wall time, ms): a quit waits only for the chunks announced since, the only
    /// downloads that can still be running (`lost::wait_for_captures`).
    started_ms: i64,
    /// When this run linked the account (its `AccountReady` recorded, the clock's ms): the token gate counts its 30
    /// minutes from it before the phone's first history notice is recorded (`send::tokens`, D12).
    linked_ms: Option<i64>,
    /// The `core_outbox` seq of the `AccountReady` the login waits for the core to commit.
    ready_seq: Option<i64>,
    wake: Arc<Notify>,
    acked: watch::Receiver<i64>,
    acked_open: bool,
    forwarder: tokio::task::JoinHandle<()>,
    forwarder_stop: CancellationToken,
    /// Stage 2 of the receive path (inbox rows -> `core_outbox`), started with the actor so rows a
    /// previous run left are handled at once. Stopped in `finish`, after the client, before the
    /// outbox gets its last moment.
    stage2: tokio::task::JoinHandle<()>,
    stage2_stop: CancellationToken,
    /// The core is gone or a write the account cannot go on without failed: the loop ends with
    /// this.
    stop_with: Option<Exit>,
    /// The logout of a refused pairing, while it is under way (the client is stopped when it ends).
    refused_logout: Option<PendingLogout>,
    /// The session whose `stop` did not see the store released in time: no client is made until it has
    /// (fix F9).
    unreleased: Option<Arc<dyn Session>>,
    /// The `Transmit` attempts under way (`send::attempt`), each until its late step is over; a normal quit waits
    /// for them within [`QUIT_ACK_WAIT`] (D19), after telling the ones still before their frame to end (fix F16); what
    /// is left after the wait is ended before the client stops.
    attempts: tokio::task::JoinSet<()>,
    /// The stop states of the attempts under way, by message (W5b, G2): filled before an attempt is spawned, emptied by
    /// its guard.
    transmits: send::stop::Transmits,
    /// The core's blob directory and the account's `upload-tmp` (W5b: an attempt's file work).
    blob_dir: PathBuf,
    upload_tmp: PathBuf,
    /// The messages whose POST may still run after their attempt answered (W5b fix F5).
    lingering: send::upload::Lingering,
    /// The session receipts go out over (`send::receipts::run`): `Some` while a connection is up and not paused.
    receipt_live: watch::Sender<Option<Arc<dyn Session>>>,
    /// Wakes the receipt runner after a job was written.
    receipt_wake: Arc<Notify>,
    /// Typing states on their way (fire and forget), dropped with the actor.
    chatter: tokio::task::JoinSet<()>,
    /// The start-up resync (W6a1): the reasons not asked yet in this start and the one task that asks them, started
    /// when the connection is `Online`, never on this thread (`Session::resync` runs on the client's runtime).
    resync: Resyncer,
    /// The phone's blocklist (W6a2, B3, D22): read once per start by one task, started when the connection is `Online`
    /// and never before; asked again only at a later `Online` of this start when no list came back.
    blocklist: blocklist::Reader,
    /// The group fetch policy (W6a2, B6, D9, M1): the start's two IQs and the single queries after a notice, one task per
    /// connection started when it is `Online` and never before, all under the account's one bucket (which lives as long as
    /// this actor).
    groups: groups::fetch::Fetcher,
    /// The merge sweep's task (W6a2, B9), started with the groups' when the connection comes `Online`.
    merges: merges::Sweeper,
}

impl Actor {
    fn new(parts: Parts) -> Self {
        let Parts {
            commands,
            events,
            conn,
            cancel,
            clock,
            config,
            store,
            stage,
            factory,
            facts,
            lost_notice,
            blob_dir,
            upload_tmp,
            resync_reasons,
        } = parts;
        let started_ms = i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
        let wake = Arc::new(Notify::new());
        let (acked_tx, acked) = watch::channel(0);
        let forwarder_stop = cancel.child_token();
        // Rows a previous run left (an `AccountReady` whose ack never came) ship from the start.
        let forwarder = tokio::spawn(outbox::forward(
            store.clone(),
            events.clone(),
            wake.clone(),
            acked_tx,
            forwarder_stop.clone(),
        ));
        // Its own token: a shutdown cancels `cancel` first, and the stage must still be there when
        // `finish` lets the outbox hand over what it has.
        let stage2_stop = CancellationToken::new();
        let stage2_state = Arc::new(Stage2::new(
            store.clone(),
            clock.clone(),
            wake.clone(),
            Timing::default(),
        ));
        let stage2 = tokio::spawn(stage2_state.clone().drive(stage2_stop.clone()));
        let stage2_progress = stage2_state.progress();
        let writer = {
            let writer = EventWriter::new(store.clone(), clock.clone(), wake.clone());
            // The zone that states a group's members lets its waiting admin deletes go: stage 2 maps them then.
            writer.wakes_stage2_with(stage2_state.waker());
            writer
        };
        // A NEW captured chunk wakes the driver; so does the link hold opening (`sync_receive_hold`).
        let history_wake = Arc::new(Notify::new());
        let history = Driver::spawn(driver::Parts {
            store: store.clone(),
            stage: stage.clone(),
            stage2: stage2_state.clone(),
            clock: clock.clone(),
            wake: history_wake.clone(),
            outbox_wake: wake.clone(),
            merges: writer.merges(),
            acked: acked.clone(),
            timing: driver::Timing::DEFAULT,
        });
        // The receipt runner (D10): it sends the jobs while `receipt_live` names a live session.
        let (receipt_live, live) = watch::channel(None);
        let receipt_wake = Arc::new(Notify::new());
        let own_of = stage2_state.clone();
        tokio::spawn(send::receipts::run(
            store.clone(),
            live,
            receipt_wake.clone(),
            move || {
                let stage2 = own_of.clone();
                async move { stage2.own().await }
            },
            cancel.child_token(),
        ));
        let receive = Receive {
            inbox: Inbox::new(store.clone(), clock.clone()),
            writer,
            stage2: stage2_state,
            capture: Capture::new(stage.clone(), history_wake.clone()),
            acks: Arc::default(),
        };
        Self {
            commands,
            events,
            conn,
            cancel,
            clock,
            config,
            store,
            factory,
            facts,
            client: None,
            login: None,
            held: false,
            online: false,
            hold: HoldBook {
                pulled: false,
                view: HoldView::new(HOLD_UNKNOWN_END),
                pull: None,
                pushes: 0,
                asked_pushes: 0,
                unknown_ended: false,
                retry: None,
                backoff: HOLD_RETRY_MIN,
            },
            receive,
            rebuild: Rebuild {
                sleep: None,
                next: REBUILD_MIN,
                started_ms: None,
            },
            catch: CatchUp::new(),
            catch_timing: catchup::Timing::default(),
            catch_timer: None,
            stage2_progress,
            history,
            history_wake,
            stage,
            lost_notice,
            started_ms,
            linked_ms: None,
            ready_seq: None,
            wake,
            acked,
            acked_open: true,
            forwarder,
            forwarder_stop,
            stage2,
            stage2_stop,
            stop_with: None,
            refused_logout: None,
            unreleased: None,
            attempts: tokio::task::JoinSet::new(),
            transmits: send::stop::Transmits::default(),
            blob_dir,
            upload_tmp,
            lingering: send::upload::Lingering::default(),
            receipt_live,
            receipt_wake,
            chatter: tokio::task::JoinSet::new(),
            resync: Resyncer::new(resync_reasons),
            blocklist: blocklist::Reader::default(),
            groups: groups::fetch::Fetcher::default(),
            merges: merges::Sweeper::default(),
        }
    }

    // ----- the loop -----

    async fn event_loop(&mut self) -> Exit {
        loop {
            if let Some(exit) = self.stop_with.take() {
                return exit;
            }
            self.sync_receipt_live();
            let next = tokio::select! {
                () = self.cancel.cancelled() => Next::Cancelled,
                command = self.commands.recv() => Next::Command(command),
                signal = next_signal(&mut self.client) => Next::Signal(signal),
                changed = self.acked.changed(), if self.acked_open => Next::Acked(changed.is_ok()),
                () = timer(&mut self.hold.retry) => Next::HoldRetry,
                pulled = hold_pull(&mut self.hold.pull) => Next::HoldPulled(pulled),
                () = timer(&mut self.rebuild.sleep) => Next::Rebuild,
                // The sender lives in `self.receive` as long as this actor does.
                _ = self.stage2_progress.changed(), if self.catch.waits_for_stage2() => {
                    Next::Stage2Progress
                }
                () = timer(&mut self.catch_timer) => Next::CatchUpTimer,
                outcome = refused_logout(&mut self.refused_logout) => Next::RefusedLogout(outcome),
            };
            match next {
                // The core dropped its end: the same as a shutdown.
                Next::Cancelled | Next::Command(None) | Next::Command(Some(Command::Shutdown)) => {
                    return Exit::Shutdown;
                }
                Next::Command(Some(command)) => {
                    if let Some(exit) = self.command(command).await {
                        return exit;
                    }
                }
                Next::Signal(Some(signal)) => {
                    if let Some(exit) = self.signal(signal).await {
                        return exit;
                    }
                }
                Next::Signal(None) => {
                    if let Some(client) = &mut self.client {
                        client.signals = None;
                    }
                }
                Next::Acked(open) => {
                    if open {
                        self.acked_by_core().await;
                        self.catch_up().await;
                    } else {
                        self.acked_open = false;
                    }
                }
                Next::Stage2Progress => self.catch_up().await,
                Next::CatchUpTimer => {
                    self.catch_timer = None;
                    self.catch_timeout().await;
                }
                Next::HoldRetry => {
                    self.hold.retry = None;
                    if self.online {
                        self.start_pull();
                    }
                }
                Next::HoldPulled(pulled) => {
                    self.hold.pull = None;
                    self.hold_pulled(pulled).await;
                }
                Next::Rebuild => {
                    self.rebuild.sleep = None;
                    self.rebuild_client().await;
                }
                Next::RefusedLogout(outcome) => self.refused_logout_ended(outcome).await,
            }
        }
    }

    /// One command. `Some` ends the actor.
    async fn command(&mut self, command: Command) -> Option<Exit> {
        match command {
            Command::Logout => return Some(self.logout().await),
            Command::Connect => self.connect().await,
            Command::Disconnect => self.disconnect().await,
            Command::NetworkChanged | Command::Resumed => {
                if !self.held
                    && let Some(client) = &self.client
                {
                    client.session.reconnect_now().await;
                }
            }
            Command::StartLogin {
                flow,
                relink,
                taken,
            } => self.start_login(&flow, relink, taken).await,
            Command::SubmitLoginInput { txn, values } => {
                if let (Some(login), Some(client)) = (&mut self.login, &self.client) {
                    let session = client.session.clone();
                    let outs = login.input(&*session, txn, &values).await;
                    self.outs(outs).await;
                }
            }
            Command::CancelLogin => {
                if let (Some(login), Some(client)) = (&mut self.login, &self.client) {
                    let session = client.session.clone();
                    let outs = login.cancel(&*session).await;
                    self.outs(outs).await;
                }
            }
            Command::Transmit { local, req, reply } => self.transmit(local, *req, reply).await,
            // The X on an upload (W5b, G2): answered at once from the attempt's stop state, with no `.await` before
            // the reply; an attempt that is past its point of no return, or none, is too late.
            Command::AbortTransmit { local, reply } => {
                let _ = reply.send(self.transmits.abort(local));
            }
            // The 1:1 timer set is the one conversation change W6a2 writes (D22); the rest is W6b's.
            Command::ChangeConversation {
                conv,
                change,
                id,
                reply,
            } => self.change_conversation(conv, change, id, reply).await,
            Command::ChangeContact { reply, .. }
            | Command::ChangeAccountSettings { reply, .. }
            | Command::ChangeProfile { reply, .. }
            // A10a: WhatsApp states no `roaming_writes` flag yet (W6 does), so the switch is refused.
            | Command::SetRoamingWrites { reply, .. }
            | Command::SubmitChallenge { reply, .. } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            Command::Resolve { reply, .. } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            Command::ListOwnDevices { reply, .. } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            // WhatsApp's security code is a later item: no safety numbers (P5a).
            Command::SafetyNumber { reply, .. } => {
                let _ = reply.send(Ok(Err(kourier_model::SafetyUnavailable::Unsupported)));
            }
            Command::SetVerified { reply, .. } => {
                let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            }
            // Nothing pauses on a WhatsApp identity yet: an approval with nothing to do.
            Command::ConfirmIdentity { reply, .. } => {
                let _ = reply.send(Ok(()));
            }
            Command::DownloadAttachment {
                attachment,
                request,
                ..
            } => self.answer_download(attachment, request).await,
            // Receipts (D10, `send::receipts`): the job and its confirmation in one zone; the runner sends it.
            Command::SendReceipts {
                cmd,
                conv,
                targets,
                kind,
            } => {
                let now = self.now_ms();
                match send::receipts::persist(&self.store, cmd, conv, targets, kind, now).await {
                    Ok(persisted) => {
                        self.wake.notify_one();
                        if persisted == send::receipts::Persisted::Job {
                            self.receipt_wake.notify_one();
                        } else {
                            tracing::debug!(?persisted, "a receipt command needs no job");
                        }
                    }
                    // The row stays pending: the core sends the command again at the next start.
                    Err(e) => tracing::warn!(error = %e, "a receipt job could not be written"),
                }
            }
            // The user read a chat here (D6): the history states no unread hint for it afterwards, and the command
            // is confirmed in the same zone (D10): the receipts carry the reads of live messages; reads of history
            // rows reach the phone only with W6b's app-state `markChatAsRead`.
            Command::SyncReadMarker { cmd, conv, .. } => {
                let now = self.now_ms();
                match send::receipts::confirm_read_marker(&self.store, cmd, &conv, now).await {
                    Ok(()) => self.wake.notify_one(),
                    Err(e) => {
                        tracing::warn!(error = %e, "a read of a chat was not noted for the history")
                    }
                }
            }
            // A10a: the core's own durable change of a chat (the clearing of a mark when a chat is read) is queued
            // only for an account whose caps state the roaming flag, which WhatsApp does not until W6; a row left
            // over is confirmed and dropped, or the core would send it again at every start. No content in the line.
            Command::ChangeConversationDurable { cmd, .. } => {
                tracing::debug!(cmd = cmd.0, "a durable conversation change is dropped");
                let now = self.now_ms();
                match send::receipts::confirm(&self.store, cmd, now).await {
                    Ok(()) => self.wake.notify_one(),
                    Err(e) => {
                        tracing::warn!(error = %e, "a durable conversation change was not confirmed")
                    }
                }
            }
            // WhatsApp media have no ids to tell the other devices (D10): confirmed.
            Command::DeleteAttachment { cmd, .. } => {
                let now = self.now_ms();
                match send::receipts::confirm(&self.store, cmd, now).await {
                    Ok(()) => self.wake.notify_one(),
                    Err(e) => {
                        tracing::warn!(error = %e, "a delete of an attachment was not confirmed")
                    }
                }
            }
            Command::SetTyping { conv, on } => self.set_typing(conv, on).await,
            // THE EXPLICIT LIST (brief §2.7, decisions D10; rule 2 of upload-progress: no wildcard for a command with
            // an answer): every other command, by name, with why nothing is done for it here. A durable one stays
            // pending (no `CommandPersisted`) until the step that carries it out (P5a's `SafetyNumber`, `SetVerified`
            // and `ConfirmIdentity` are answered above).
            // Read by the loop before a command reaches here (`Next::Command(Some(Command::Shutdown))`).
            Command::Shutdown => {}
            // Confirmed here (W6a1, D14): the chat's upload records go and `CommandPersisted` is appended, in ONE
            // zone. The phone is not told until W6b (`clearChat`/`deleteChat` behind the account's switch).
            Command::ClearConversation {
                cmd,
                conv,
                before_ms,
                system_before_ms,
                ..
            } => {
                let now = self.now_ms();
                // The chat's cut (F3): what the core keeps for its own history rows, the larger of the two times.
                let cut = before_ms.max(system_before_ms);
                match send::receipts::confirm_clear(&self.store, cmd, &conv, cut, now).await {
                    Ok(()) => self.wake.notify_one(),
                    Err(e) => {
                        tracing::warn!(error = %e, "a clear of a chat was not confirmed")
                    }
                }
            }
            // Durable, W7's re-upload request (the media retry): the row stays pending for it.
            Command::RequestAttachments { .. } => {}
            // The history comes from the phone's push (W4), not on demand: the descriptor says so.
            Command::FetchHistory { .. } => {}
            // Profiles come with the contacts' app state and pushes: nothing is asked of the network for one (W6a2, B8b). What
            // the backend holds of the person is stated again from its books.
            Command::RefreshContact { user } => {
                let now = self.now_ms();
                match outbox::append_restated_contact(&self.store, user, now).await {
                    Ok(Some(_)) => self.wake.notify_one(),
                    Ok(None) => {}
                    Err(e) => tracing::warn!(error = %e, "a contact could not be restated"),
                }
            }
            // The phone's history push cannot be skipped or dismissed from here (W4 states no transfer).
            Command::SkipHistoryTransfer | Command::DismissHistoryTransfer => {}
        }
        self.stop_with.take()
    }

    /// The wall time in ms, as the store's rows keep it.
    fn now_ms(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX)
    }

    /// The session of a live, unpaused connection, for the receipt runner (unchanged: nothing is said).
    fn sync_receipt_live(&self) {
        let live = self
            .client
            .as_ref()
            .filter(|client| self.online && !client.paused)
            .map(|client| client.session.clone());
        self.receipt_live.send_if_modified(|current| {
            let same = match (&*current, &live) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            };
            if !same {
                *current = live;
            }
            !same
        });
    }

    /// `SetTyping` (brief §2.6): fire and forget over a live connection, NEVER while the reach-out lock is not
    /// open (D8: the peer would see typing and then nothing for days); D7's started fact is asked off the loop.
    async fn set_typing(&mut self, conv: kourier_model::ConvKey, on: bool) {
        let Some(session) = self
            .client
            .as_ref()
            .filter(|client| self.online && !client.paused)
            .map(|client| client.session.clone())
        else {
            return;
        };
        if self.send_gate() != send::attempt::Gate::Open {
            tracing::debug!("no typing state while the reach-out lock is not open");
            return;
        }
        let own = self.receive.stage2.own().await;
        while self.chatter.try_join_next().is_some() {}
        self.chatter.spawn(send::receipts::typing(
            self.store.clone(),
            session,
            own,
            conv,
            on,
            self.recheck(),
        ));
    }

    /// `Transmit` (W5a, `send::attempt`): the attempt runs off the loop (its ack wait takes up to
    /// `ACK_WAIT`), on this runtime, held in `attempts` so it ends with the actor; it replies with its answer
    /// after the outcome is durable. The local of the request is the stanza id (D2). Its stop state is registered
    /// HERE, synchronously, before the spawn (W5b, G2): an `AbortTransmit` right behind the `Transmit` finds it. Only
    /// a new message with files can be stopped ([`stoppable`]).
    async fn transmit(
        &mut self,
        local: kourier_model::LocalMsgId,
        req: kourier_backend_api::OutgoingRequest,
        reply: tokio::sync::oneshot::Sender<kourier_backend_api::TransmitOutcome>,
    ) {
        while self.attempts.try_join_next().is_some() {}
        let stop = self.transmits.register(
            local,
            if stoppable(&req.kind) {
                send::stop::Stop::open()
            } else {
                send::stop::Stop::closed()
            },
        );
        let parts = self.attempt_parts(stop).await;
        self.attempts
            .spawn(send::attempt::transmit(parts, req, reply));
    }

    /// What one attempt that writes a frame needs, `stop` its place in the actor's `Transmits`.
    async fn attempt_parts(&mut self, stop: send::stop::Registered) -> send::attempt::Parts {
        send::attempt::Parts {
            store: self.store.clone(),
            session: self.client.as_ref().map(|client| client.session.clone()),
            acks: self.receive.acks.clone(),
            own: self.receive.stage2.own().await,
            now_ms: i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX),
            hold: self.recheck(),
            linked_ms: self.linked_ms,
            stop,
            files: send::attempt::Files {
                events: self.events.clone(),
                blob_dir: self.blob_dir.clone(),
                tmp_dir: self.upload_tmp.clone(),
                lingering: self.lingering.clone(),
            },
        }
    }

    /// `ChangeConversation` (W6a2 B7b, D22): the 1:1 timer set is judged ([`send::timer::request`]) and, if it is one the
    /// backend writes, runs as an attempt beside the sends (the quit waits for it the same way); the answer is the
    /// attempt's. Any other change is W6b's: refused as not done.
    async fn change_conversation(
        &mut self,
        conv: kourier_model::ConvKey,
        change: kourier_model::ConversationChange,
        id: Option<kourier_backend_api::AllocatedId>,
        reply: kourier_backend_api::Reply<()>,
    ) {
        let kourier_model::ConversationChange::SetTimer { timer } = change else {
            let _ = reply.send(Err(BackendError::unsupported(NOT_DONE)));
            return;
        };
        let set = match send::timer::request(conv, timer, id) {
            Ok(set) => set,
            Err(refusal) => {
                let _ = reply.send(Err(refusal));
                return;
            }
        };
        while self.attempts.try_join_next().is_some() {}
        let stop = self.transmits.register_change();
        let writer = self.receive.writer.clone();
        let parts = self.attempt_parts(stop).await;
        self.attempts.spawn(async move {
            let _ = reply.send(send::timer::set(parts, writer, set).await);
        });
    }

    /// `DownloadAttachment`: nothing downloads before W7, and a request that is never answered would leave the
    /// attachment waiting. The answer is a failure the user can try again after W7 (a transient error: the
    /// core marks the attachment `Failed { retryable: true }`), in one `core_outbox` zone (the core resends an
    /// unanswered request at its next start, so a kill before the commit loses nothing).
    async fn answer_download(&mut self, attachment: AttachmentId, request: DownloadId) {
        let answer = BackendEvent::AttachmentDone {
            attachment,
            request: Some(request),
            result: Err(BackendError::transient(DOWNLOAD_NOT_DONE)),
        };
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        match outbox::append_download_answer(&self.store, answer, now).await {
            Ok(_) => self.wake.notify_one(),
            Err(e) => tracing::warn!(error = %e, "a download request could not be answered"),
        }
    }

    // ----- the connection -----

    /// `Connect`: resumes a paused client, or makes one for an account with a paired device. An
    /// account without a device, and one whose login is running, has nothing to connect.
    async fn connect(&mut self) {
        if self.login_active() {
            return;
        }
        self.held = false;
        // A person who asks for the connection does not wait out a backoff.
        self.rebuild.sleep = None;
        if let Some(client) = &mut self.client {
            if client.paused {
                client.paused = false;
                client.session.resume().await;
                self.set_conn(ConnState::Connecting);
            }
            return;
        }
        if !self.has_paired_device().await {
            return;
        }
        if !self.previous_released().await {
            // The last client still holds the store (its stop timed out): the connection follows
            // when it lets go.
            tracing::warn!("the previous client has not released the store; waiting");
            self.schedule_rebuild(REBUILD_MIN);
            return;
        }
        if let Err(e) = self.start_client(None).await {
            tracing::warn!(error = %e, "the client did not start");
            self.held = true;
        }
    }

    async fn disconnect(&mut self) {
        self.held = true;
        if let Some(client) = &mut self.client {
            client.paused = true;
            client.session.pause().await;
            // The socket is closed: no ack of its frames will come (D5).
            self.receive.acks.connection_ended();
            self.online = false;
            self.reset_catch_up(true).await;
            self.set_conn(ConnState::Offline { retry_at_ms: None });
        } else if self.rebuild.sleep.take().is_some() {
            // The account waited to try again: it does not, and the time of the try is gone.
            self.set_conn(ConnState::Offline { retry_at_ms: None });
        }
    }

    /// Makes the client and starts it. On failure nothing is left running.
    async fn start_client(&mut self, gate: Option<Arc<Gate>>) -> Result<(), BackendError> {
        check_config(&self.config)?;
        // Never two clients on one store (fix F9): the callers that can wait do (`connect`,
        // `rebuild_client`); a login or a logout that meets a client still letting go fails.
        if !self.previous_released().await {
            return Err(BackendError::transient(
                "the previous client has not released the account's database yet",
            ));
        }
        // A new client begins at the backlog (§4.5): until its drain's boundary is taken, what it
        // commits is catch-up. Nothing of this client can be committed yet, so this is exact.
        self.reset_catch_up(true).await;
        // The handler tells our own acts (a group notice we caused) by the ids it cannot look up
        // itself: read them from the device row now, before the client can dispatch anything.
        let _ = self.receive.stage2.own().await;
        let (tap, signals) = Tap::channel(gate);
        let session = self.factory.make(Wiring {
            tap,
            store: self.store.clone(),
            clock: self.clock.clone(),
            receive: self.receive.clone(),
        });
        self.set_conn(ConnState::Connecting);
        if let Err(e) = session.start().await {
            if session.stop().await == Released::TimedOut {
                self.unreleased = Some(session);
            }
            self.online = false;
            self.set_conn(ConnState::Offline { retry_at_ms: None });
            return Err(e);
        }
        self.client = Some(Client {
            session,
            signals: Some(signals),
            paused: false,
        });
        self.rebuild.started_ms = Some(self.clock.now_ms());
        Ok(())
    }

    /// A write the account cannot go on without failed: the client ended itself (terminal, D5). It is
    /// stopped here (the store is released before anything can make another), the connection shows
    /// the time of the next try, and a NEW client is made when the backoff is over. A failure after a
    /// short run doubles the wait (5 s, 10 s, ... 5 min); after a run of two minutes it starts again
    /// at 5 s.
    async fn write_failed(&mut self) {
        let now = self.clock.now_ms();
        let ran = self
            .rebuild
            .started_ms
            .take()
            .map_or(Duration::ZERO, |started| {
                Duration::from_millis(now.saturating_sub(started))
            });
        let wait = if ran >= STABLE_RUN {
            REBUILD_MIN
        } else {
            self.rebuild.next
        };
        self.rebuild.next = (wait * 2).min(REBUILD_MAX);
        tracing::error!(
            wait_secs = wait.as_secs(),
            "a write failed; the client is replaced after a backoff"
        );
        self.stop_client().await;
        if self.held || self.login_blocks_a_client() || !self.has_paired_device().await {
            // Paused, a link that this very failure ended, or nothing paired to connect: nothing
            // is made again by itself.
            return;
        }
        self.schedule_rebuild(wait);
    }

    /// Shows `Offline` with the time of the next try and sets the wait after which a new client may
    /// be made.
    fn schedule_rebuild(&mut self, wait: Duration) {
        let retry_at_ms = self
            .clock
            .now_ms()
            .saturating_add(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX));
        self.set_conn(ConnState::Offline {
            retry_at_ms: Some(retry_at_ms),
        });
        self.rebuild.sleep = Some(self.clock.sleep(wait));
    }

    /// Whether no earlier client holds the store any more (fix F9). A stop that timed out left the
    /// session behind; it is asked once, without waiting, and forgotten when its release came.
    async fn previous_released(&mut self) -> bool {
        let Some(session) = &self.unreleased else {
            return true;
        };
        if session.release_wait(Duration::ZERO).await == Released::Yes {
            self.unreleased = None;
            true
        } else {
            false
        }
    }

    /// The backoff after a failed write is over: the same as a `Connect`, except that it does not
    /// lift a hold (a `Disconnect` or a terminal verdict meanwhile keeps the account quiet).
    async fn rebuild_client(&mut self) {
        if self.held || self.client.is_some() || self.login_blocks_a_client() {
            return;
        }
        if !self.has_paired_device().await {
            return;
        }
        if !self.previous_released().await {
            // The last client still holds the store (its stop timed out): look again later.
            tracing::warn!("the previous client has not released the store; waiting");
            self.schedule_rebuild(REBUILD_MIN);
            return;
        }
        if let Err(e) = self.start_client(None).await {
            tracing::warn!(error = %e, "the client did not start");
            self.held = true;
        }
    }

    /// The pairing was refused: the logout of the companion is under way (the client's event handler
    /// started it the moment the gate refused). The session's `logout` is one shared answer, so
    /// asking it again here only waits for that one; the loop waits for it as one more event and
    /// stops the client when it ends (F3, review F6: never inside a signal's handling).
    fn await_refused_logout(&mut self) {
        let Some(client) = &self.client else {
            return;
        };
        let session = client.session.clone();
        self.refused_logout = Some(Box::pin(async move { session.logout().await }));
    }

    /// The logout of a refused pairing ended: the outcome is kept for the core's `Logout`, and the
    /// client, which the logout has shut down, is stopped and releases the store.
    async fn refused_logout_ended(&mut self, outcome: DeviceLogout) {
        self.refused_logout = None;
        if let Some(login) = &mut self.login {
            login.logout_reached(outcome);
        }
        self.stop_client().await;
    }

    /// Waits for the logout of a refused pairing that is still under way, up to `limit` (`None`: as
    /// long as the session's own bound), and settles it.
    async fn settle_refused_logout(&mut self, limit: Option<Duration>) {
        let Some(pending) = self.refused_logout.take() else {
            return;
        };
        let outcome = match limit {
            None => Some(pending.await),
            Some(limit) => tokio::time::timeout(limit, pending).await.ok(),
        };
        match outcome {
            Some(outcome) => self.refused_logout_ended(outcome).await,
            None => {
                // Not ended in time: the companion may stay on the phone's list.
                if let Some(login) = &mut self.login {
                    login.logout_reached(DeviceLogout::NotReached);
                }
                self.stop_client().await;
            }
        }
    }

    /// Stops the client and waits for the store to be released. A release that does not come in
    /// time is logged and never turns a finished logout into a failure (critic M9).
    async fn stop_client(&mut self) {
        self.online = false;
        self.close_login_gate();
        // A resync that runs ends with the client; its collections keep their flags.
        self.resync.abort().await;
        self.blocklist.abort().await;
        self.groups.abort().await;
        self.merges.abort().await;
        self.reset_catch_up(true).await;
        if let Some(client) = self.client.take() {
            let released = client.session.stop().await;
            // No ack of its frames will come: the sends that wait hear it (D5).
            self.receive.acks.connection_ended();
            if released == Released::TimedOut {
                tracing::warn!("the client did not release the store in time");
                self.unreleased = Some(client.session);
            }
        }
    }

    /// A client that is being stopped pairs nothing any more: the gate of the login it belongs to
    /// refuses every pairing from now on (W2c, L-a), so a `<pair-success>` that lands while the client
    /// is still shutting down is claimed and refused instead of completed.
    fn close_login_gate(&self) {
        if let Some(login) = &self.login {
            login.gate().close();
        }
    }

    /// [`stop_client`](Self::stop_client) for a clean quit (rule (b), `crate::appstate`): the client
    /// is paused first (its socket closes, so no sync starts or waits for the network), the account
    /// is looked at for quiet, the client is stopped and its store released, the look is repeated, and
    /// only a quiet stop clears the dirty flags of the app-state collections. Not quiet = the flags
    /// stay = the next start rebuilds those collections. Counts only in the log.
    ///
    /// ONE QUIT BUDGET (fix F7, DUR-7): the network gives a quit `SHUTDOWN_GRACE` (20 s). The waits of [`Self::finish`]
    /// (announced chunks, acks, the resync) ran beside each other for at most 15 s; the store's release can take
    /// `RELEASE_WAIT` (5 s); the quiet look gets what is left of the grace (at most its own bound), so the three never
    /// add up past it. A look that finds no time left keeps the flags (the next start asks again): with less than one
    /// look window left before the release (the joined waits took their 15 s) there is no look at all (fix G6,
    /// [`Quiet::within`]), so the worst case ends inside the grace. `quit_started` is when the quit began, on the
    /// monotonic clock and on the actor's own clock (the waits ran on it); the larger elapsed counts.
    async fn stop_client_quietly(&mut self, quit_started: (tokio::time::Instant, u64)) {
        self.online = false;
        self.close_login_gate();
        // The resync had its moment beside the quit's other waits; one still there (a quit that skipped them) is ended
        // now, its flag standing.
        self.resync.abort().await;
        self.blocklist.abort().await;
        self.groups.abort().await;
        self.merges.abort().await;
        self.reset_catch_up(true).await;
        let Some(client) = self.client.take() else {
            return;
        };
        let spent = quit_started.0.elapsed().max(Duration::from_millis(
            self.clock.now_ms().saturating_sub(quit_started.1),
        ));
        let left = crate::network::SHUTDOWN_GRACE
            .saturating_sub(crate::session::RELEASE_WAIT)
            .saturating_sub(spent);
        let (released, outcome) = match Quiet::within(left) {
            Some(quiet) => {
                appstate::stop_quietly(
                    &self.store,
                    client.session.as_ref(),
                    quiet,
                    client.paused,
                    appstate::EVENTS_WRITTEN,
                )
                .await
            }
            None => {
                tracing::warn!(
                    "the quit had no time left for the quiet look; the app-state flags stay and the next start asks again"
                );
                appstate::stop_without_look(client.session.as_ref(), client.paused).await
            }
        };
        self.receive.acks.connection_ended();
        if released == Released::TimedOut {
            tracing::warn!("the client did not release the store in time");
            self.unreleased = Some(client.session);
        }
        tracing::debug!(?outcome, "the clean stop looked at the app-state flags");
    }

    fn set_conn(&self, state: ConnState) {
        // Group fetches are asked only while `Online` (D9, M1): the policy's task is told the moment that ends.
        if !matches!(state, ConnState::Online { .. }) {
            self.groups.offline();
        }
        self.conn.send_if_modified(|current| {
            if *current == state {
                false
            } else {
                *current = state;
                true
            }
        });
    }

    fn login_active(&self) -> bool {
        self.login.as_ref().is_some_and(|login| !login.is_over())
    }

    /// A login that a new client must not be made around: one that has no account yet. After the
    /// core acked `AccountReady` only the first `Connected` is missing, and a client that ended in
    /// the meantime is made again (fix F8).
    fn login_blocks_a_client(&self) -> bool {
        self.login
            .as_ref()
            .is_some_and(|login| !login.is_over() && !login.is_linked())
    }

    /// Whether the store holds a device that was paired with a phone (an unpaired one is what a
    /// login that did not finish leaves).
    async fn has_paired_device(&self) -> bool {
        matches!(self.store.load().await, Ok(Some(device)) if device.pn.is_some())
    }

    // ----- signals -----

    /// One signal of the client. `Some` ends the actor.
    async fn signal(&mut self, signal: WaSignal) -> Option<Exit> {
        if let WaSignal::Typing(typing) = signal {
            self.typing(typing).await;
            return self.stop_with.take();
        }
        let now = self.clock.now_ms();
        let session = self.client.as_ref().map(|client| client.session.clone());
        let mut exit = None;
        let mut terminal = false;
        if matches!(signal, WaSignal::Connected) {
            // The hold is stated BEFORE the connection shows `Syncing` (D8, the contract's K-A fix C1).
            self.connected().await;
        }
        if let Some(state) = states::state_of(&signal) {
            self.online = matches!(
                state.conn,
                ConnState::Syncing { .. } | ConnState::Online { .. }
            );
            if !self.online {
                // The connection is down or stopped: the next one starts over (§4.5). The handler
                // forgot the stored boundary in the writer's order (fix F11).
                self.reset_catch_up(false).await;
            }
            self.set_conn(state.conn.clone());
            self.facts.state(&state);
            terminal = state.terminal;
            if let ConnState::LoggedOut { reason, .. } = &state.conn {
                exit = Some(Exit::LoggedOut {
                    reason: reason.clone(),
                });
            } else if terminal {
                // The account waits for the person to ask again.
                self.held = true;
            }
        }
        if let WaSignal::WillRetry(why) = &signal {
            tracing::info!(note = states::retry_note(*why), "whatsapp connection");
        }
        if let WaSignal::Hold(state) = &signal
            && let Some(hold) = states::hold_push(state)
        {
            self.hold.view.set(hold);
            self.hold.pushes += 1;
            self.state_hold(hold).await;
        }
        self.catch_signal(&signal).await;
        if let (Some(login), Some(session)) = (&mut self.login, &session) {
            let outs = login.signal(&**session, &signal, now).await;
            self.outs(outs).await;
        }
        if matches!(signal, WaSignal::WriteFailed) {
            self.write_failed().await;
        }
        if terminal && exit.is_none() {
            // A terminal verdict: the client is stopped and has released the store before anything
            // can make a new one.
            self.stop_client().await;
        }
        exit.or_else(|| self.stop_with.take())
    }

    /// Someone types in a chat (or stopped). EPHEMERAL: an envelope with no ack, never a `core_outbox`
    /// row (a typing indicator is stale in seconds; W3b rule (a) covers durable facts only). Dropped when
    /// the client is paused or gone, while the link hold is closed (nothing of a connection that has no
    /// account may reach the core, `crate::hold`) and when the sender is one of our own ids (an echo of
    /// our other device, not news). The pairing gate already drops everything after a refusal
    /// ([`crate::tap::Tap::deliver`]).
    async fn typing(&mut self, typing: Typing) {
        if !self.client.as_ref().is_some_and(|client| !client.paused)
            || self.store.hold().is_closed()
        {
            return;
        }
        if self.receive.stage2.own().await.is_own(&typing.by) {
            return;
        }
        self.emit(Envelope::ephemeral(BackendEvent::Typing {
            conv: typing.conv,
            by: typing.by,
            device: typing.device,
            on: typing.on,
            ttl: TYPING_TTL,
        }))
        .await;
    }

    // ----- the catch-up machine -----

    /// The connection or the client ended (or a client is about to begin): the machine starts over
    /// and the stored boundary is forgotten, so a boundary of an earlier drain can never be read as
    /// this one's (§4.5: every new connection starts at the backlog). Best effort: a failed write is
    /// logged, and the handler forgets it in the writer's order as well.
    ///
    /// `forget` says whether the stored boundary is cleared HERE: for a client that is made or paused
    /// (nothing of it can be in flight), yes; for the end of a connection that a signal announced, no:
    /// the handler queued the forgetting in the writer's order when it saw the event, and a write
    /// from here could land after the boundary a fast reconnect's drain already took (fix F11).
    async fn reset_catch_up(&mut self, forget: bool) {
        self.catch.reset();
        self.catch_timer = None;
        if forget && let Err(e) = catchup::forget_boundary(&self.store).await {
            tracing::warn!(error = %e, "the catch-up boundary could not be cleared");
        }
        // Every row of an earlier connection is below this number: the ledger sweep's bound once
        // this connection has drained durably (decision D7). Unknown = no sweep.
        match catchup::counter(&self.store).await {
            Ok(counter) => self.catch.cover(counter),
            Err(e) => tracing::warn!(error = %e, "the arrival counter could not be read"),
        }
    }

    /// `Connected` and the offline signals move the machine (only those of a client that runs: a
    /// paused one may still deliver what was in flight).
    async fn catch_signal(&mut self, signal: &WaSignal) {
        if !self.client.as_ref().is_some_and(|client| !client.paused) {
            return;
        }
        // Upstream reconnects by itself (its keepalive watchdog, a critical app-state sync that got no
        // answer) without a `Disconnected`, and reports ONE end per drain: a second `Connected`, or a
        // preview or a completion after the drain's end, is a NEW connection's (fix F4). The machine
        // starts over without touching the store (the handler wrote the boundary in order, F11); no
        // ledger sweep is made for that connection.
        let new_connection = match signal {
            WaSignal::Connected => self.catch.has_connected(),
            WaSignal::OfflinePreview { .. } | WaSignal::OfflineDone { .. } => {
                self.catch.drain_ended()
            }
            _ => false,
        };
        if new_connection {
            tracing::info!("a new connection began without a disconnect; the catch-up starts over");
            self.catch.reset();
            self.catch_timer = None;
        }
        match signal {
            WaSignal::Connected => self.catch.connected(),
            WaSignal::OfflinePreview { total } => {
                self.catch.previewed(*total);
                self.catch_timer = None;
            }
            WaSignal::OfflineInterrupted => {
                self.catch.interrupted();
                self.catch_timer = None;
            }
            WaSignal::OfflineDone { count } => self.offline_done(*count).await,
            _ => return,
        }
        self.catch_up().await;
    }

    /// Upstream says the drain is complete. The boundary is Kourier's own (decision D5): the handler
    /// took it in the writer's order when it saw the event, so it is read here; without one (that
    /// write failed, or no handler is behind this client) the machine takes it now.
    async fn offline_done(&mut self, count: i32) {
        self.catch.drain_over(count);
        if self.catch.phase() != Phase::Backlog {
            // The backstop took the boundary already; upstream's completion came late.
            return;
        }
        let taken = match catchup::boundary(&self.store).await {
            Ok(Some(boundary)) => Ok(boundary),
            Ok(None) => catchup::take_boundary(&self.store).await,
            Err(e) => Err(e),
        };
        match taken {
            Ok(boundary) => self.catch.boundary_taken(boundary),
            Err(e) => tracing::warn!(error = %e, "the catch-up boundary could not be taken"),
        }
    }

    /// Moves the machine as far as the store lets it and shows the result. Stage 2's progress, the
    /// core's acknowledgements, the signals and the machine's timer all end here.
    async fn catch_up(&mut self) {
        let running = self.client.as_ref().is_some_and(|client| !client.paused);
        if !running || !self.catch.is_up() {
            self.catch_timer = None;
            return;
        }
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        let advanced = self.catch.advance(&self.store, now).await;
        if let Some(covers) = self.catch.take_sweep() {
            // A low-priority job that nobody waits for: the ledger of what the earlier connections
            // committed is no longer needed for the redeliveries of this drain.
            let store = self.store.clone();
            tokio::spawn(async move {
                match inbox::sweep_ledger(&store, covers, now).await {
                    Ok(swept) => tracing::debug!(swept, "the inbox ledger was swept"),
                    Err(e) => tracing::warn!(error = %e, "the inbox ledger could not be swept"),
                }
            });
        }
        match self.catch.shown() {
            Shown::Nothing => {}
            Shown::Syncing => self.set_conn(ConnState::Syncing { fraction: None }),
            Shown::Online => {
                let online = ConnState::Online { detail: None };
                let came_online = *self.conn.borrow() != online;
                if came_online {
                    self.facts.state(&WaState::plain(online.clone()));
                }
                self.set_conn(online);
                if came_online {
                    self.start_resync().await;
                    self.start_blocklist().await;
                    self.start_groups();
                    self.start_merges();
                }
            }
        }
        self.catch_timer = match advanced {
            Err(e) => {
                tracing::warn!(error = %e, "the catch-up check failed; looking again later");
                Some(self.clock.sleep(self.catch_timing.recheck))
            }
            // Waiting for upstream's completion: one wait for the whole drain, not one per signal.
            Ok(()) if self.catch.phase() == Phase::Backlog => self
                .catch_timer
                .take()
                .or_else(|| Some(self.clock.sleep(self.catch_timing.backlog_wait))),
            Ok(()) => None,
        };
    }

    /// The connection came `Online`: the start-up resync's reasons are asked, one collection after another, by ONE task
    /// on the client's runtime (D2); nothing is started for a paused client or when none is left.
    async fn start_resync(&mut self) {
        let Some(session) = self
            .client
            .as_ref()
            .filter(|client| !client.paused)
            .map(|client| client.session.clone())
        else {
            return;
        };
        let store = self.store.clone();
        let clock = self.clock.clone();
        let writer = self.receive.writer.clone();
        self.resync
            .start(move |reasons| async move {
                startup::run(
                    &store,
                    &writer,
                    &Asking(session),
                    clock.as_ref(),
                    Quiet::STANDARD,
                    appstate::EVENTS_WRITTEN,
                    reasons,
                )
                .await
            })
            .await;
    }

    /// The connection came `Online`: the phone's blocklist is read, once per start and at most once per 24 hours, by ONE
    /// task on the actor's runtime (D22, Q1); nothing is started for a paused client or when this start has the list.
    async fn start_blocklist(&mut self) {
        let Some(session) = self
            .client
            .as_ref()
            .filter(|client| !client.paused)
            .map(|client| client.session.clone())
        else {
            return;
        };
        let writer = self.receive.writer.clone();
        let own = self.receive.stage2.own_known();
        let (store, clock) = (self.store.clone(), self.clock.clone());
        self.blocklist
            .start(move || async move {
                blocklist::read(session.as_ref(), &store, &clock, &writer, own).await
            })
            .await;
    }

    /// The connection came `Online`: the group fetch policy's task is started (D9, M1); nothing for a paused client, and a
    /// task that still runs goes on by itself.
    fn start_groups(&mut self) {
        let Some(session) = self
            .client
            .as_ref()
            .filter(|client| !client.paused)
            .map(|client| client.session.clone())
        else {
            return;
        };
        self.groups.start(
            session,
            self.receive.writer.clone(),
            self.receive.stage2.clone(),
            self.clock.clone(),
        );
    }

    /// The connection came `Online`: the merge sweep's task is started, or woken when it runs (W6a2, B9). It needs no session:
    /// it reads the store and states through the holding writer; a paused client has a store that is still ours, but a
    /// client that is not running states nothing new, so none is started for it either.
    fn start_merges(&mut self) {
        if self.client.as_ref().is_none_or(|client| client.paused) {
            return;
        }
        self.merges.start(
            self.store.clone(),
            self.receive.writer.clone(),
            self.receive.stage2.clone(),
            self.receive.writer.merges(),
        );
    }

    /// The machine's timer ran out. In the backlog phase upstream's completion did not come: the
    /// boundary is taken now, so the account does not stay `Syncing` with every message hidden as
    /// catch-up.
    async fn catch_timeout(&mut self) {
        if self.catch.phase() == Phase::Backlog && self.catch.is_up() {
            tracing::warn!("the offline sync did not complete in time; going on without it");
            match catchup::take_boundary(&self.store).await {
                Ok(boundary) => self.catch.boundary_taken(boundary),
                Err(e) => tracing::warn!(error = %e, "the catch-up boundary could not be taken"),
            }
        }
        self.catch_up().await;
    }

    /// The first `Connected` of a run asks for the reach-out lock and waits for the answer up to
    /// [`HOLD_FIRST_WAIT`]; the later ones state what is known (critic M1, L-e). Either way the statement comes
    /// before the caller shows `Syncing` (D8).
    async fn connected(&mut self) {
        if self.hold.pulled {
            if let Some(last) = self.hold.view.last() {
                self.state_hold(last).await;
            }
            return;
        }
        self.start_pull();
        if self.hold.pull.is_none() {
            return;
        }
        let wait = self.clock.sleep(HOLD_FIRST_WAIT);
        let pulled = tokio::select! {
            () = self.cancel.cancelled() => return,
            () = wait => None,
            pulled = hold_pull(&mut self.hold.pull) => Some(pulled),
        };
        match pulled {
            Some(pulled) => {
                self.hold.pull = None;
                self.hold_pulled(pulled).await;
            }
            // The pull goes on off the loop; until it answers, sends wait (`send_gate`).
            None => tracing::warn!(
                "the reach-out lock did not answer in time; the connection goes on without it"
            ),
        }
    }

    /// Asks for the reach-out lock off the loop, unless a pull is in flight or there is no client.
    fn start_pull(&mut self) {
        if self.hold.pull.is_some() {
            return;
        }
        let Some(session) = self.client.as_ref().map(|client| client.session.clone()) else {
            return;
        };
        self.hold.view.asked(self.clock.now_ms());
        self.hold.asked_pushes = self.hold.pushes;
        self.hold.pull = Some(tokio::spawn(async move { session.fetch_hold().await }));
    }

    async fn hold_pulled(&mut self, pulled: Result<HoldState, BackendError>) {
        match pulled {
            Ok(state) => {
                self.hold.pulled = true;
                self.hold.retry = None;
                self.hold.backoff = HOLD_RETRY_MIN;
                if self.hold.pushes != self.hold.asked_pushes {
                    // A push came after this pull was asked: the push is the newer word of the server (fix F6).
                    tracing::debug!("a reach-out pull older than a push was dropped");
                    return;
                }
                let hold = states::hold_pull(&state);
                self.hold.view.set(hold);
                self.state_hold(hold).await;
            }
            Err(e) => {
                // Nothing is stated until the answer comes: a hold without an end must not stand
                // for ever, and "no hold" must not be said without knowing.
                tracing::warn!(error = %e, "the reach-out lock could not be read; trying again later");
                self.hold.retry = Some(self.clock.sleep(self.hold.backoff));
                self.hold.backoff = (self.hold.backoff * 2).min(HOLD_RETRY_MAX);
            }
        }
    }

    /// States the hold to the core (the contract's ephemeral `AccountHold`) and logs it by kind.
    async fn state_hold(&mut self, hold: Option<AccountHold>) {
        self.facts.hold(hold);
        self.emit(Envelope::ephemeral(BackendEvent::AccountHold(hold)))
            .await;
    }

    /// Whether a send may write a frame now (D8): not while the lock of this run is unknown (until
    /// [`HOLD_UNKNOWN_END`] after the run first asked) and not while a hold is known. Receipts do not ask.
    fn send_gate(&mut self) -> send::attempt::Gate {
        let gate = self.hold.view.gate(self.clock.now_ms());
        if gate == send::attempt::Gate::Open
            && self.hold.view.last().is_none()
            && !self.hold.unknown_ended
        {
            self.hold.unknown_ended = true;
            tracing::warn!(
                "the reach-out lock stayed unknown for 10 minutes; sends go on without it"
            );
        }
        gate
    }

    /// The gate asked again, for an attempt or a typing state that is about to write (fix F6): the shared view and
    /// this actor's clock, nothing else.
    fn recheck(&self) -> Recheck {
        let view = self.hold.view.clone();
        let clock = self.clock.clone();
        Recheck::new(move || view.gate(clock.now_ms()))
    }

    // ----- login -----

    async fn start_login(
        &mut self,
        flow: &str,
        relink: Option<kourier_backend_api::Relink>,
        taken: Vec<kourier_backend_api::TakenIdentity>,
    ) {
        if self.login_active() {
            return;
        }
        // A refused pairing's client is still on its way out: a new login never meets it.
        self.settle_refused_logout(None).await;
        let rules = Rules::new(taken, relink, self.config.expect_number_suffix.clone());
        // The one fixed token that says whether the number is checked (no digits, W2c F2): a run
        // tool reads it before it asks for a scan.
        tracing::info!(
            expected_number_check = rules.number_check(),
            "a whatsapp login starts"
        );
        let mut login = match Login::new(flow, rules) {
            Ok(login) => login,
            Err(error) => {
                self.emit_step(LoginStep {
                    step_id: "failed".to_owned(),
                    txn: 1,
                    kind: LoginStepKind::Failed { error },
                })
                .await;
                return;
            }
        };
        let outs = if self.has_paired_device().await {
            login.fail_start(BackendError::internal("this account is already linked"))
        } else {
            // Nothing of the new connection may reach `core_outbox` before `AccountReady` does:
            // the hold closes before the client exists (fix F5) and opens when the login has an
            // account, or ended without one (`outs`).
            self.store.hold().set_closed(true);
            match self.start_client(Some(login.gate())).await {
                Ok(()) => login.begin(),
                Err(error) => login.fail_start(error),
            }
        };
        self.login = Some(login);
        self.outs(outs).await;
    }

    /// What the login machine handed back.
    async fn outs(&mut self, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Step(step) => self.emit_step(step).await,
                Out::Ready(ready) => self.append_ready(&ready).await,
                Out::Stop => self.stop_client().await,
                Out::Refused => self.await_refused_logout(),
            }
        }
        // A finished login is forgotten, except a refused pairing: the core's `Logout` that follows
        // asks what its logout reached.
        if self
            .login
            .as_ref()
            .is_some_and(|login| login.is_over() && !login.is_refused())
        {
            self.login = None;
        }
        self.sync_receive_hold();
    }

    /// The receive hold follows the login (fix F5, `crate::hold`): closed while it has no account
    /// (before the pairing, or a pairing that was refused), open otherwise. Opening wakes stage 2, which
    /// left its rows alone meanwhile.
    fn sync_receive_hold(&self) {
        let closed = self.login.as_ref().is_some_and(Login::holds_receive);
        self.store.hold().set_closed(closed);
        if !closed {
            self.receive.stage2.wake();
            self.history_wake.notify_one();
        }
    }

    async fn emit_step(&mut self, step: LoginStep) {
        self.emit(Envelope::ephemeral(BackendEvent::Login(step)))
            .await;
    }

    async fn emit(&mut self, envelope: Envelope) {
        let sent = tokio::select! {
            () = self.cancel.cancelled() => return,
            sent = self.events.send(envelope) => sent,
        };
        if sent.is_err() && self.stop_with.is_none() {
            // The core is gone: the same as a shutdown.
            self.stop_with = Some(Exit::Shutdown);
        }
    }

    /// `AccountReady` into `core_outbox` in one zone; the forwarder ships it.
    async fn append_ready(&mut self, ready: &Ready) {
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        match outbox::append_ready(&self.store, ready, now).await {
            Ok(seq) => {
                self.ready_seq = Some(seq);
                self.linked_ms = Some(now);
                // A fresh link: the device row did not exist when the client was made.
                self.receive.stage2.set_own(OwnIds {
                    lid: ready
                        .own_ids
                        .iter()
                        .find(|id| id.scheme() == "lid")
                        .cloned(),
                    pn: ready.own_ids.iter().find(|id| id.scheme() == "pn").cloned(),
                });
                self.wake.notify_one();
            }
            Err(e) => {
                tracing::error!(error = %e, "AccountReady could not be recorded");
                self.stop_with = Some(Exit::Failed(BackendError::internal(
                    "the linked account could not be recorded",
                )));
            }
        }
    }

    /// The core committed rows: when that includes `AccountReady`, the login may finish.
    async fn acked_by_core(&mut self) {
        self.lost_notice_taken().await;
        let Some(seq) = self.ready_seq else { return };
        if *self.acked.borrow() < seq {
            return;
        }
        self.ready_seq = None;
        if let Some(login) = &mut self.login {
            let outs = login.acked();
            self.outs(outs).await;
        }
    }

    /// The core committed the row of the lost-history notice: the person has been told (W4 C20b). A kill before this
    /// leaves the books as they were, so the next start tells again.
    async fn lost_notice_taken(&mut self) {
        let Some(notice) = self.lost_notice else {
            return;
        };
        if *self.acked.borrow() < notice.seq {
            return;
        }
        self.lost_notice = None;
        if let Err(e) = lost::mark_told(&self.store, notice.count).await {
            tracing::warn!(error = %e, "the lost history could not be marked told");
        }
    }

    // ----- logout and the end -----

    /// `Command::Logout`: the device is removed from the account if there is one, the core is told
    /// once what that reached, and the actor ends logged out.
    async fn logout(&mut self) -> Exit {
        self.rebuild.sleep = None;
        // A refused pairing's logout may still be under way: its outcome is what this reports.
        self.settle_refused_logout(None).await;
        let outcome = if let Some(reported) = self.login.as_ref().and_then(Login::logout_report) {
            // A refused pairing was logged out the moment it was refused.
            reported
        } else if self.login_active() {
            // Nothing was paired: the login is withdrawn (and its gate refuses what may still arrive).
            self.close_login_gate();
            if let Some(client) = &self.client {
                client.session.cancel_pair_code().await;
            }
            DeviceLogout::NoDevice
        } else if !self.has_paired_device().await {
            DeviceLogout::NoDevice
        } else {
            self.logout_paired_device().await
        };
        let report = LogoutReport {
            outcome,
            device_name: Some(self.factory.device_name()),
        };
        self.emit(Envelope::ephemeral(BackendEvent::Logout(report)))
            .await;
        Exit::LoggedOut {
            reason: "logged out on request".to_owned(),
        }
    }

    async fn logout_paired_device(&mut self) -> DeviceLogout {
        if self.client.is_none() {
            // The request to remove the companion needs a connection: the client is made for it
            // (the session's logout holds the short wait for the connection, brief 3.6).
            if let Err(e) = self.start_client(None).await {
                tracing::warn!(error = %e, "no client to remove the device with");
                return DeviceLogout::NotReached;
            }
        }
        match &self.client {
            Some(client) => client.session.logout().await,
            None => DeviceLogout::NotReached,
        }
    }

    /// The shutdown order (module doc), then the exit.
    async fn finish(&mut self, exit: Exit) -> Exit {
        let quit_started = (tokio::time::Instant::now(), self.clock.now_ms());
        // A quit gives the chunks the phone announced, whose download is still running, up to `lost::QUIT_WAIT` to be
        // captured, and only while there is such a chunk: the client is still up, the phone sends the history once.
        // FIRST, so the rest of the order keeps its own bounds inside the network's stop grace (W4 C20).
        // Beside it, the sends whose frames are in the air get their acks, and their outcomes reach the ledger,
        // before the client stops (D19): the next start answers them from the ledger with no frame.
        if matches!(exit, Exit::Shutdown) && !self.login_active() {
            let captures = lost::wait_for_captures(
                &self.store,
                &self.stage,
                &self.clock,
                self.started_ms,
                lost::quit_wait(),
            );
            // A start-up resync that is still running gets its moment BESIDE these waits, not after them (fix F7, DUR-7,
            // M7): the three share one deadline, so the quit costs the longest of them (15 s at most), and a replay
            // cut by it is safe: its collection keeps its flag and the next start asks again.
            let resync = self.resync.finish(startup::STOP_WAIT);
            // The attempts still before their frame (preparing, uploading, waiting for the hold) end now, `Transient`, and
            // are not waited for; the ones past their point of no return and the frameless kinds are (fix F16, Q-B16).
            let (ended, kept) = self.transmits.quit_all();
            if ended + kept > 0 {
                tracing::debug!(ended, kept, "the quit told the sends under way to end");
            }
            let (captured, acks, ()) =
                tokio::join!(captures, quit_acks(&mut self.attempts), resync);
            match acks {
                QuitAcks::Nothing | QuitAcks::Answered => {}
                QuitAcks::TimedOut => tracing::warn!(
                    "the quit did not wait longer for the acks of sends in the air; they are sent again at the next start"
                ),
            }
            match captured {
                lost::Waited::Nothing => {}
                lost::Waited::Captured => {
                    tracing::info!("the announced history chunks arrived before the quit")
                }
                lost::Waited::TimedOut => tracing::warn!(
                    "the quit did not wait longer for the announced history chunks; they count as lost at the next start"
                ),
            }
        }
        // A refused pairing's logout gets a moment to reach the phone's list before the client goes.
        self.settle_refused_logout(Some(REFUSED_LOGOUT_GRACE)).await;
        if self.login_active()
            && let Some(client) = &self.client
        {
            client.session.cancel_pair_code().await;
        }
        if matches!(exit, Exit::Shutdown) && !self.login_active() {
            self.stop_client_quietly(quit_started).await;
        } else {
            self.stop_client().await;
        }
        self.stage2_stop.cancel();
        let _ = (&mut self.stage2).await;
        // The history driver: a row in flight finishes and its feeder's thread is joined, then the stage
        // file is closed (its queued jobs finish; a capture that comes later fails and its receipt is withheld).
        self.history.stop().await;
        let capture = self.receive.capture.clone();
        let _ = tokio::task::spawn_blocking(move || capture.close()).await;
        // A logged-out account keeps no history stage: its key goes with the core's wipe, and the chunks in it are
        // the account's messages (W4 C18). A quit, a failure and a removal that is not a logout leave it.
        if matches!(exit, Exit::LoggedOut { .. }) {
            let stage = self.stage.clone();
            let _ = tokio::task::spawn_blocking(move || stage.remove()).await;
        }
        if let Ok(max) = outbox::max_seq(&self.store).await {
            let _ = tokio::time::timeout(
                SHUTDOWN_DRAIN,
                outbox::wait_acked(&self.store, self.acked.clone(), max),
            )
            .await;
        }
        self.forwarder_stop.cancel();
        let _ = (&mut self.forwarder).await;
        exit
    }
}

/// What the quit's wait for the sends in the air found (D19).
enum QuitAcks {
    /// No attempt was under way.
    Nothing,
    /// Every attempt ended (answered, or its connection ended).
    Answered,
    /// [`QUIT_ACK_WAIT`] passed first: the rest are ended at once (the core reads a dropped reply as transient and
    /// resends at the next start under the same id).
    TimedOut,
}

/// Whether an attempt of `kind` can be stopped (W5b, G2): a new message with files, whose upload comes first. A text
/// has no upload to report (the core never offers its X); an edit, a delete and a reaction keep `Stop::closed()`.
fn stoppable(kind: &kourier_backend_api::OutgoingKind) -> bool {
    matches!(
        kind,
        kourier_backend_api::OutgoingKind::Message {
            body: kourier_model::Body::Media { .. },
            ..
        }
    )
}

/// Waits for the attempts under way, within [`QUIT_ACK_WAIT`] (D19), and ends the rest (fix F16).
async fn quit_acks(attempts: &mut tokio::task::JoinSet<()>) -> QuitAcks {
    if attempts.is_empty() {
        return QuitAcks::Nothing;
    }
    let all = async { while attempts.join_next().await.is_some() {} };
    let waited = match tokio::time::timeout(QUIT_ACK_WAIT, all).await {
        Ok(()) => QuitAcks::Answered,
        Err(_) => QuitAcks::TimedOut,
    };
    // What is left after the wait ends HERE, before the client stops (fix F16): an upload task of the client's runtime
    // would hold the client into the store's release wait (5 s), and a dropped reply is what the core resends anyway.
    attempts.abort_all();
    while attempts.join_next().await.is_some() {}
    waited
}

/// `Rebuild`: the core forgot the account, so the stored device is stated again, through
/// `core_outbox`, before anything else of this run is written (fix F10). A store without a paired
/// device states nothing. Rows a PREVIOUS run left in `core_outbox` keep their lower `seq` (they
/// cannot be moved behind it): they are what the old core never acknowledged, applied to the rebuilt
/// one, which takes them in its idempotent ingest.
async fn state_ready_again(store: &KourierStore, now_ms: u64) -> Result<(), BackendError> {
    let device = match store.load().await {
        Ok(Some(device)) => device,
        Ok(None) => return Ok(()),
        Err(e) => {
            tracing::error!(error = %e, "the stored device could not be read");
            return Ok(());
        }
    };
    let Some(pn) = device.pn.as_ref() else {
        return Ok(());
    };
    let (Ok(pn_id), lid_id) = (
        kourier_model::UserId::from_parts("pn", pn.user.as_str()),
        device
            .lid
            .as_ref()
            .and_then(|lid| kourier_model::UserId::from_parts("lid", lid.user.as_str()).ok()),
    ) else {
        return Ok(());
    };
    let ready = Ready::of(pn_id, lid_id, pn.device);
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    match outbox::append_ready_again(store, &ready, now).await {
        Ok(_) => Ok(()),
        Err(e) => {
            tracing::error!(error = %e, "AccountReady could not be recorded");
            Err(BackendError::internal(
                "the linked account could not be recorded",
            ))
        }
    }
}

/// The environment and the proxy of the configuration, which the client cannot honour: nothing is
/// connected around them.
fn check_config(config: &BackendConfig) -> Result<(), BackendError> {
    if config.env != BackendEnv::Production {
        return Err(BackendError::unsupported(
            "WhatsApp runs only against its real service",
        ));
    }
    if config.proxy.is_some() {
        return Err(BackendError::unsupported(
            "WhatsApp cannot use the proxy yet",
        ));
    }
    Ok(())
}

/// The client's next signal; never ready while there is no client or its channel is gone.
async fn next_signal(client: &mut Option<Client>) -> Option<WaSignal> {
    match client.as_mut().and_then(|client| client.signals.as_mut()) {
        Some(signals) => signals.recv().await,
        None => pending().await,
    }
}

/// The logout of a refused pairing; never ready while none is under way.
async fn refused_logout(pending: &mut Option<PendingLogout>) -> DeviceLogout {
    match pending {
        Some(pending) => pending.await,
        None => std::future::pending().await,
    }
}

/// A timer of the loop (the reach-out lock's retry, the wait for a new client); never ready while
/// none is set.
/// The answer of the reach-out pull in flight; never, without one. A pull whose task ended without an answer (a
/// panic, the runtime going down) is a failed pull.
async fn hold_pull(
    pull: &mut Option<tokio::task::JoinHandle<Result<HoldState, BackendError>>>,
) -> Result<HoldState, BackendError> {
    match pull {
        Some(handle) => handle
            .await
            .unwrap_or_else(|_| Err(BackendError::transient("the pull of the lock ended"))),
        None => pending().await,
    }
}

async fn timer(timer: &mut Option<Sleep>) {
    match timer {
        Some(sleep) => sleep.await,
        None => pending().await,
    }
}

#[cfg(test)]
mod tests;
