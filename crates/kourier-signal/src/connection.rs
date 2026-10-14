//! The authenticated connection (§4.5, §6.4, §6.6): connect with backoff, the receive loop
//! (stage-1 batches with ack after commit and backpressure, the catch-up boundary, `Syncing` →
//! `Online` once stage 2 and the core are through it),
//! and the post-connect steps (one-time prekeys, the job runner with remote config, capabilities
//! and the post-link sync requests, and `Done`, §6.5 steps 7-8, §6.12).
//!
//! Staying connected (S15; Desktop's `SocketManager.preload.ts` and
//! `WebsocketResources.preload.ts`):
//! - a closed or failed socket is reopened after a Fibonacci delay with jitter ([`backoff`]),
//!   at once when the system says the network changed or the machine woke up ([`Nudge`]),
//!   also when it said so while an attempt that then failed was on its way;
//! - a keepalive every 30 s proves the server still answers; a socket that does not answer, or
//!   whose last sign of life is more than 5 minutes old (a suspend), is closed and replaced
//!   ([`health`]); a hint from the system asks at once, with a short timeout, whatever
//!   keepalive is still unanswered;
//! - close 4409 (another client connected with our credentials) is not answered with a
//!   reconnect: the task ends [`ConnectionEnd::Held`] and waits for the user;
//! - 401, 403 or a refusal of our PNI keys means this device is no longer linked:
//!   [`ConnectionEnd::LoggedOut`], and the actor runs the unlink procedure (§4.3 step 5);
//! - a local clock further from the server's than the remote config allows closes the socket
//!   and keeps it closed until the clock is right (`NeedsUserAction(FixClock)`, §9.6); after
//!   a resume the server is asked for its time once more.

pub(crate) mod backoff;
mod health;

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, InitialSync};
use kourier_model::{BackendError, ConnState, LoginStep, LoginStepKind, UserAction};
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, spawn_local};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use self::backoff::Backoff;
use self::health::ServerClock;
use crate::actor::Shared;
use crate::db::{self, Account};
use crate::error::{Error, Result};
use crate::jobs::{self, JobKind};
use crate::net::{
    ChatEvent, ChatRequest, ChatResponse, Connected, SignalTransport, StopReason, TransportError,
};
use crate::receive::stage1::{self, MAX_BATCH, Stage1};
use crate::receive::{FAILPOINT_AFTER_INBOX_COMMIT, FAILPOINT_BEFORE_ACK, stage2};
use crate::{keys, maintenance};

pub(crate) use self::health::{CLOCK_RECHECK, KEEPALIVE_INTERVAL};

/// How long stage 1 waits for more envelopes after the first of a batch (Desktop's
/// `decryptAndCacheBatcher`).
const BATCH_WAIT: Duration = Duration::from_millis(75);

/// Delays between attempts of the post-connect steps on one socket.
const POST_CONNECT_RETRY_SECS: [u64; 5] = [2, 5, 15, 30, 60];

/// How a connection task ended.
#[derive(Debug)]
pub(crate) enum ConnectionEnd {
    /// Stopped on request.
    Stopped,
    /// Stopped by a condition only the user can lift (4409: another client took the
    /// connection; the server no longer accepts this version). The connection state says
    /// which; system hints must not reconnect, `Command::Connect` does.
    Held,
    /// The server says this device is no longer linked (§4.3 step 5).
    LoggedOut(String),
    /// A fatal error (the supervisor restarts the actor).
    Failed(BackendError),
}

/// What the actor tells a running connection task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nudge {
    /// `Command::Connect` while the task runs: stop waiting, connect now.
    Connect,
    /// The network changed (NetworkManager): a waiting reconnect starts now, an open socket is
    /// asked whether it still lives.
    NetworkChanged,
    /// The machine woke up (logind): as above; a socket silent for more than 5 minutes is
    /// replaced without asking, and a wait for the right clock connects once to hear the
    /// server's time anew.
    Resumed,
}

/// Why the receive loop of one socket ended.
enum LoopEnd {
    Stop,
    /// The socket closed; `healthy` when it had delivered its backlog (`QueueEmpty`) first.
    Closed {
        reason: StopReason,
        healthy: bool,
    },
    /// The socket's last sign of life is too old (the machine slept): replace it now.
    Stale,
    /// The server no longer answers on this socket (keepalive failed or timed out).
    Dead {
        healthy: bool,
    },
    /// Our clock is too far from the server's.
    Skewed(ServerClock),
    /// Something else found this device unlinked (the refusal of our PNI keys).
    Unlinked(String),
    Fatal(Error),
}

/// How a wait between two connect attempts ended.
enum Waited {
    Stop,
    /// The delay passed.
    Elapsed,
    /// The actor said to connect now.
    Nudged,
}

/// Connects, receives and reconnects until stopped, held or unlinked.
pub(crate) async fn run<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    stop: CancellationToken,
    mut nudges: mpsc::UnboundedReceiver<Nudge>,
) -> ConnectionEnd {
    let mut backoff = Backoff::new();
    loop {
        let Some(account) = shared.account() else {
            return ConnectionEnd::Failed(BackendError::internal("connecting without an account"));
        };
        shared.set_conn(ConnState::Connecting);
        let connected = tokio::select! {
            () = stop.cancelled() => return stopped(&shared),
            connected = shared.net.connect_auth(account.credentials()) => connected,
        };
        // What the actor hinted at while this attempt ran: a connection that came about has
        // answered it; an attempt that failed has not (below).
        let mut hinted = false;
        while nudges.try_recv().is_ok() {
            hinted = true;
        }
        let Connected { socket, events } = match connected {
            Ok(connected) => connected,
            Err(
                e @ (TransportError::DeviceDeregistered
                | TransportError::Rejected { status: 401 | 403 }),
            ) => return unlinked(&shared, e.to_string()),
            Err(TransportError::AppExpired) => {
                shared.set_conn(ConnState::NeedsUserAction(UserAction::UpdateApp));
                return ConnectionEnd::Held;
            }
            Err(e) if hinted && !answered_by_server(&e) => {
                // The network changed (or the machine woke up, or the user asked) while this
                // attempt was on its way, and it never got an answer: it says nothing about
                // the network as it is now. Waiting out a delay here (up to 26 minutes in the
                // long tail) would lose the hint; the next attempt starts at once, with fresh
                // delays behind it. One retry per burst of hints: the queue is empty now.
                tracing::info!(error = %e, "connect failed while a hint arrived; trying again now");
                backoff.reset();
                continue;
            }
            Err(e) => {
                // Below HTTP: the server was not reached at all (see `Backoff`).
                let unreachable =
                    matches!(e, TransportError::Unreachable(_) | TransportError::Timeout);
                let mut delay = backoff.next(unreachable);
                if let TransportError::RetryLater { retry_after_secs } = e {
                    delay = delay.max(Duration::from_secs(u64::from(retry_after_secs)));
                }
                tracing::warn!(error = %e, retry_in_s = delay.as_secs(), "connect failed");
                match wait(&shared, &stop, &mut nudges, delay).await {
                    Waited::Stop => return stopped(&shared),
                    Waited::Elapsed => {}
                    Waited::Nudged => backoff.reset(),
                }
                continue;
            }
        };
        let socket = Arc::new(socket);
        *shared.socket.borrow_mut() = Some(socket.clone());
        shared.set_conn(ConnState::Syncing { fraction: None });
        let post = spawn_local(post_connect(shared.clone(), socket.clone()));
        let end = receive_loop(&shared, account, events, &stop, &mut nudges, &socket).await;
        post.abort();
        *shared.socket.borrow_mut() = None;
        // Closes what is still open (a stale or dead socket, a stop); nothing to do for a
        // socket the server closed.
        shared.net.disconnect(&socket).await;
        let (healthy, immediate) = match end {
            LoopEnd::Stop => return stopped(&shared),
            LoopEnd::Fatal(e) => {
                tracing::error!(error = %e, "receive loop failed");
                return ConnectionEnd::Failed(e.to_backend_error());
            }
            LoopEnd::Unlinked(reason) => return unlinked(&shared, reason),
            LoopEnd::Closed {
                reason: StopReason::ConnectedElsewhere,
                ..
            } => {
                // Another client took our credentials: no automatic reconnect (§6.4), or the
                // two would take the connection from each other for ever.
                tracing::warn!("another client connected with this device's credentials (4409)");
                shared.set_conn(ConnState::NeedsUserAction(UserAction::Reconnect));
                return ConnectionEnd::Held;
            }
            LoopEnd::Skewed(clock) => {
                if !wait_for_clock(&shared, &stop, &mut nudges, clock).await {
                    return stopped(&shared);
                }
                (true, true)
            }
            LoopEnd::Stale => {
                tracing::info!("chat socket stale (the machine slept); reconnecting now");
                (true, true)
            }
            LoopEnd::Dead { healthy } => {
                tracing::warn!("chat socket did not answer its keepalive; reconnecting");
                (healthy, false)
            }
            LoopEnd::Closed {
                // 4401: the server wants a fresh login on this connection (our credentials
                // changed or went away). The next connect says which: it is refused with 401
                // or 403 when the device was unlinked.
                reason: StopReason::ConnectionInvalidated,
                healthy,
            } => {
                tracing::info!("the server invalidated the connection (4401); reconnecting");
                (healthy, false)
            }
            LoopEnd::Closed { reason, healthy } => {
                tracing::info!(?reason, "chat socket closed");
                (healthy, false)
            }
        };
        // A socket that closes before delivering its backlog counts as a failed attempt, so a
        // server that accepts and drops us is not hammered.
        if healthy {
            backoff.reset();
        }
        if immediate {
            continue;
        }
        let delay = backoff.next(false);
        tracing::info!(retry_in_s = delay.as_secs(), "reconnecting after a delay");
        match wait(&shared, &stop, &mut nudges, delay).await {
            Waited::Stop => return stopped(&shared),
            Waited::Elapsed => {}
            Waited::Nudged => backoff.reset(),
        }
    }
}

/// Whether a failed connect attempt was the server's own answer (a refused upgrade, "retry
/// later"): the network works, so a hint that came meanwhile is no reason to ask again before
/// the delay.
fn answered_by_server(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::Rejected { .. } | TransportError::RetryLater { .. }
    )
}

/// Waits `delay` before the next connect attempt (`Offline` with the retry time), unless the
/// actor says the network changed, the machine woke up or the user asked: then at once.
async fn wait<T: SignalTransport>(
    shared: &Shared<T>,
    stop: &CancellationToken,
    nudges: &mut mpsc::UnboundedReceiver<Nudge>,
    delay: Duration,
) -> Waited {
    shared.set_conn(ConnState::Offline {
        retry_at_ms: Some(retry_at(shared, delay)),
    });
    tokio::select! {
        () = stop.cancelled() => Waited::Stop,
        () = shared.clock.sleep(delay) => Waited::Elapsed,
        nudge = nudges.recv() => match nudge {
            Some(nudge) => {
                tracing::info!(?nudge, "reconnecting now");
                Waited::Nudged
            }
            // The actor is gone; `stop` follows.
            None => Waited::Stop,
        },
    }
}

/// The local clock is too far from the server's (§6.4, §9.6): reports it and stays
/// disconnected, looking again every `Tuning::clock_recheck` (Desktop: 10 s) and whenever the
/// actor nudges, until the clock is within the limit. `false` when stopped meanwhile. The wait
/// between two looks is real time, not the clock under suspicion.
///
/// The server's time is carried forward from the last connect by the monotonic clock, which
/// stands still while the machine sleeps: after a resume the estimate is behind by the length
/// of the sleep, and a clock set right would still look wrong (for good, after a long sleep).
/// A resume therefore ends the wait (`true`): the next connect brings the server's time anew
/// and comes back here if the clock is still off. Desktop keeps its estimate
/// (`SocketManager.preload.ts` `#getSkew`, "accidental interactions with OS sleep").
async fn wait_for_clock<T: SignalTransport>(
    shared: &Shared<T>,
    stop: &CancellationToken,
    nudges: &mut mpsc::UnboundedReceiver<Nudge>,
    clock: ServerClock,
) -> bool {
    let max = max_skew(shared);
    let mut reported = None;
    loop {
        if !clock.is_skewed(shared.now_ms(), max) {
            tracing::info!("the clock is right again; reconnecting");
            return true;
        }
        // Rounded to seconds: the estimate drifts by milliseconds, the banner need not.
        let skew_ms = clock.skew_ms(shared.now_ms()) / 1000 * 1000;
        if reported != Some(skew_ms) {
            tracing::warn!(
                skew_s = skew_ms / 1000,
                max_s = max.as_secs(),
                "the system clock is too far from the server's; staying disconnected"
            );
            reported = Some(skew_ms);
            shared.set_conn(ConnState::NeedsUserAction(UserAction::FixClock { skew_ms }));
        }
        tokio::select! {
            () = stop.cancelled() => return false,
            () = tokio::time::sleep(shared.tuning.clock_recheck) => {}
            nudge = nudges.recv() => match nudge {
                Some(Nudge::Resumed) => {
                    tracing::info!("woke up with a wrong clock; asking the server for its time");
                    return true;
                }
                // Look again now.
                Some(Nudge::Connect | Nudge::NetworkChanged) => {}
                None => return false,
            },
        }
    }
}

/// The largest clock difference the remote config allows.
fn max_skew<T: SignalTransport>(shared: &Shared<T>) -> Duration {
    let configured = shared
        .db
        .read(|c| maintenance::remote_config_value(c, health::MAX_SKEW_KEY))
        .unwrap_or(None);
    health::max_skew(configured.as_deref())
}

/// When the next connect attempt starts, in ms since the epoch (for `ConnState::Offline`).
fn retry_at<T: SignalTransport>(shared: &Shared<T>, delay: Duration) -> u64 {
    shared
        .now_ms()
        .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
}

fn stopped<T: SignalTransport>(shared: &Shared<T>) -> ConnectionEnd {
    shared.set_conn(ConnState::Offline { retry_at_ms: None });
    ConnectionEnd::Stopped
}

/// This device is no longer linked; `reason` is log-safe.
fn unlinked<T: SignalTransport>(shared: &Shared<T>, reason: String) -> ConnectionEnd {
    tracing::warn!(%reason, "the server no longer knows this device: unlinked");
    shared.set_conn(ConnState::LoggedOut {
        reason: reason.clone(),
    });
    ConnectionEnd::LoggedOut(reason)
}

/// Inbox rows waiting for stage 2 above which stage 1 stops taking envelopes (§6.6): the
/// listener channel is unbounded, so backpressure is applied by delaying acks.
pub(crate) const MAX_BACKLOG: i64 = 2_000;

/// Where the current connection is on its way from `Syncing` to `Online` (§4.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatchUp {
    /// No `QueueEmpty` yet on this socket.
    Backlog,
    /// `QueueEmpty` came at this arrival counter; stage 2 has rows up to it left.
    Stage2 { boundary: u64 },
    /// Stage 2 is through the boundary; the core has the `core_outbox` rows written until then
    /// left: seq ≤ `target` and `created_at` ≤ `cutoff`. Both bounds are needed because seqs
    /// restart at 1 once the table is empty (no AUTOINCREMENT): a row written later with a
    /// reused seq has a newer `created_at` and does not hold `Online` back.
    Core { target: i64, cutoff: i64 },
    /// Everything up to the boundary is committed in the core.
    Online,
    /// The forwarder is gone (the core stopped listening): nothing will be acked any more.
    Stalled,
}

/// The keepalive of one socket (§6.4): when it last showed life, when the next request is due,
/// and the request in flight.
struct Keepalive<T: SignalTransport> {
    /// Wall clock (ms) of the connect or of the last answered keepalive.
    last_alive_ms: u64,
    /// Fires when the next keepalive is due.
    due: kourier_backend_api::Sleep,
    /// The keepalive in flight.
    probe: Option<JoinHandle<std::result::Result<ChatResponse, TransportError>>>,
    /// When the keepalive in flight gives up.
    probe_deadline: Option<Instant>,
    interval: Duration,
    socket: Arc<T::Socket>,
}

impl<T: SignalTransport> Keepalive<T> {
    fn new(shared: &Shared<T>, socket: &Arc<T::Socket>) -> Self {
        let interval = shared.tuning.keepalive_interval;
        Self {
            last_alive_ms: shared.now_ms(),
            due: shared.clock.sleep(interval),
            probe: None,
            probe_deadline: None,
            interval,
            socket: socket.clone(),
        }
    }

    /// Sends a keepalive that must be answered within `timeout`, unless the socket is stale
    /// (`true`: close it without asking, Desktop's "disconnecting due to stale state") or one
    /// that gives up no later is on its way.
    ///
    /// A keepalive in flight that would wait longer is replaced: a check the system asked for
    /// (a network change, a resume) must not sit behind a routine keepalive that may wait half
    /// a minute on a dead socket (Desktop's `forceKeepAlive` sends a new request whatever is
    /// in flight). The answer of the replaced one is ignored.
    fn check(&mut self, shared: &Shared<T>, timeout: Duration) -> bool {
        if health::is_stale(self.last_alive_ms, shared.now_ms()) {
            return true;
        }
        let deadline = Instant::now() + timeout;
        if self.probe_deadline.is_none_or(|current| deadline < current) {
            if let Some(replaced) = self.probe.take() {
                replaced.abort();
            }
            let (net, socket) = (shared.net.clone(), self.socket.clone());
            self.probe = Some(spawn_local(async move {
                net.request(&socket, ChatRequest::keepalive(timeout)).await
            }));
            self.probe_deadline = Some(deadline);
        }
        // The next regular one counts from this one's answer.
        self.due = Box::pin(std::future::pending());
        false
    }

    /// The answer of the keepalive in flight; `false` when the socket is to be closed
    /// (Desktop closes on any status outside 2xx as well as on silence).
    fn answered(
        &mut self,
        shared: &Shared<T>,
        answer: std::result::Result<
            std::result::Result<ChatResponse, TransportError>,
            tokio::task::JoinError,
        >,
    ) -> bool {
        self.probe = None;
        self.probe_deadline = None;
        match answer {
            Ok(Ok(response)) if response.status.is_success() => {
                self.last_alive_ms = shared.now_ms();
                self.due = shared.clock.sleep(self.interval);
                true
            }
            Ok(Ok(response)) => {
                tracing::warn!(status = response.status.as_u16(), "keepalive refused");
                false
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "keepalive failed");
                false
            }
            Err(_) => false,
        }
    }
}

impl<T: SignalTransport> Drop for Keepalive<T> {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.take() {
            probe.abort();
        }
    }
}

/// The keepalive in flight as a future that never completes when there is none.
async fn probe_answer(
    probe: Option<&mut JoinHandle<std::result::Result<ChatResponse, TransportError>>>,
) -> std::result::Result<std::result::Result<ChatResponse, TransportError>, tokio::task::JoinError>
{
    match probe {
        Some(probe) => probe.await,
        None => std::future::pending().await,
    }
}

/// The events of one socket until it closes or the task is stopped.
async fn receive_loop<T: SignalTransport>(
    shared: &Shared<T>,
    mut account: Account,
    mut events: mpsc::UnboundedReceiver<ChatEvent>,
    stop: &CancellationToken,
    nudges: &mut mpsc::UnboundedReceiver<Nudge>,
    socket: &Arc<T::Socket>,
) -> LoopEnd {
    // The boundary is per connection: until this socket's QueueEmpty, everything is catch-up.
    shared.lanes.set_boundary(None);
    let mut catch_up = CatchUp::Backlog;
    let mut acked = shared.acked.clone();
    let mut progress = shared.lanes.progress();
    let mut unlinked = shared.unlinked();
    if let Some(reason) = unlinked.borrow_and_update().clone() {
        // Found between two sockets: no receive loop was there to hear it.
        return LoopEnd::Unlinked(reason);
    }
    let mut keepalive = Keepalive::new(shared, socket);
    let mut pending: Option<ChatEvent> = None;
    // A history transfer that is not over holds receiving back (`transfer.rs`): envelopes
    // and the end of the server's queue are parked here in the order they came, and no
    // envelope is acknowledged, so the server keeps every one. Everything else of the
    // socket (its close, the keepalive, the server's clock) goes on.
    let mut hold = shared.transfer.watch();
    let mut parked: std::collections::VecDeque<ChatEvent> = std::collections::VecDeque::new();
    loop {
        let held = *hold.borrow_and_update();
        let event = match pending.take().or_else(|| {
            // What waited for the transfer comes first, in its order.
            if held { None } else { parked.pop_front() }
        }) {
            Some(event) => Some(event),
            None => tokio::select! {
                () = stop.cancelled() => return LoopEnd::Stop,
                event = events.recv() => event,
                changed = hold.changed() => {
                    if changed.is_ok() && !*hold.borrow() && !parked.is_empty() {
                        tracing::info!(
                            parked = parked.len(),
                            "the history transfer is over; taking what the socket delivered meanwhile"
                        );
                    }
                    // A closed watch never changes again: nothing to wait for, no busy loop.
                    if changed.is_err() {
                        return LoopEnd::Stop;
                    }
                    continue;
                }
                changed = progress.changed(), if matches!(catch_up, CatchUp::Stage2 { .. }) => {
                    if changed.is_ok()
                        && let Err(e) = check_online(shared, &mut catch_up)
                    {
                        return LoopEnd::Fatal(e);
                    }
                    continue;
                }
                changed = acked.changed(), if matches!(catch_up, CatchUp::Core { .. }) => {
                    if changed.is_err() {
                        // The forwarder is gone (the core stopped listening): nothing will be
                        // acked any more, and a closed watch would wake this branch forever.
                        tracing::warn!("forwarder stopped; staying in Syncing");
                        catch_up = CatchUp::Stalled;
                        continue;
                    }
                    if let Err(e) = check_online(shared, &mut catch_up) {
                        return LoopEnd::Fatal(e);
                    }
                    continue;
                }
                changed = unlinked.changed() => {
                    let reason = unlinked.borrow_and_update().clone();
                    match (changed, reason) {
                        (Ok(()), Some(reason)) => return LoopEnd::Unlinked(reason),
                        // No sender or no reason: nothing to act on, and no busy loop either.
                        (Ok(()), None) => continue,
                        (Err(_), _) => return LoopEnd::Stop,
                    }
                }
                nudge = nudges.recv() => {
                    match nudge {
                        // Connected already.
                        Some(Nudge::Connect) => {}
                        Some(Nudge::NetworkChanged | Nudge::Resumed) => {
                            if keepalive.check(shared, health::CHECK_TIMEOUT) {
                                return LoopEnd::Stale;
                            }
                        }
                        None => return LoopEnd::Stop,
                    }
                    continue;
                }
                () = &mut keepalive.due => {
                    // A timer that fires late (it does not run while the machine sleeps) finds
                    // the socket stale.
                    if keepalive.check(shared, health::KEEPALIVE_TIMEOUT) {
                        return LoopEnd::Stale;
                    }
                    continue;
                }
                answer = probe_answer(keepalive.probe.as_mut()) => {
                    if !keepalive.answered(shared, answer) {
                        return LoopEnd::Dead { healthy: catch_up != CatchUp::Backlog };
                    }
                    continue;
                }
            },
        };
        let event = match event {
            Some(event @ (ChatEvent::Envelope(_) | ChatEvent::QueueEmpty)) if held => {
                parked.push_back(event);
                continue;
            }
            event => event,
        };
        let step = match event {
            None => {
                return LoopEnd::Closed {
                    reason: StopReason::Closed("event stream ended".to_owned()),
                    healthy: catch_up != CatchUp::Backlog,
                };
            }
            Some(ChatEvent::Envelope(first)) => {
                let arrived = Instant::now();
                let mut batch = vec![first];
                let deadline = arrived + BATCH_WAIT;
                while batch.len() < MAX_BATCH {
                    // Parked envelopes are older than anything still in the channel.
                    let next = match parked.pop_front() {
                        Some(event) => Ok(Some(event)),
                        None => timeout_at(deadline, events.recv()).await,
                    };
                    match next {
                        Ok(Some(ChatEvent::Envelope(envelope))) => batch.push(envelope),
                        Ok(Some(other)) => {
                            pending = Some(other);
                            break;
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
                match backpressure(shared, &mut progress, stop).await {
                    Ok(true) => {}
                    Ok(false) => return LoopEnd::Stop,
                    Err(e) => return LoopEnd::Fatal(e),
                }
                let committed = receive_batch(
                    shared,
                    &mut account,
                    batch,
                    arrived.into_std(),
                    catch_up == CatchUp::Backlog,
                );
                // Let the stage-2 lanes and the forwarder run between batches.
                tokio::task::yield_now().await;
                committed
            }
            Some(ChatEvent::QueueEmpty) => queue_empty(shared, &mut catch_up),
            Some(ChatEvent::ServerTimestamp(server_ms)) => {
                let clock = ServerClock::now(server_ms);
                let skew_ms = clock.skew_ms(shared.now_ms());
                tracing::debug!(skew_ms, "server timestamp");
                let skewed = clock.is_skewed(shared.now_ms(), max_skew(shared));
                // The one place where the server confirms this computer's clock.
                shared.clock_confirmed.set(!skewed);
                if skewed {
                    return LoopEnd::Skewed(clock);
                }
                Ok(())
            }
            Some(ChatEvent::Alerts(alerts)) => {
                if !alerts.is_empty() {
                    tracing::info!(count = alerts.len(), "server alerts");
                }
                Ok(())
            }
            Some(ChatEvent::Stopped(reason)) => {
                return LoopEnd::Closed {
                    reason,
                    healthy: catch_up != CatchUp::Backlog,
                };
            }
        };
        if let Err(e) = step.and_then(|()| check_online(shared, &mut catch_up)) {
            return LoopEnd::Fatal(e);
        }
    }
}

/// Stage-1 backpressure (§6.6): waits while stage 2 has more than [`MAX_BACKLOG`] rows to
/// handle; the envelopes keep queueing in the unbounded listener channel, unacked. `false`
/// when stopped meanwhile.
async fn backpressure<T: SignalTransport>(
    shared: &Shared<T>,
    progress: &mut watch::Receiver<u64>,
    stop: &CancellationToken,
) -> Result<bool> {
    let mut waited = false;
    loop {
        let backlog = shared.db.read(stage2::backlog)?;
        if backlog <= MAX_BACKLOG {
            if waited {
                tracing::debug!(backlog, "stage 2 caught up; stage 1 resumes");
            }
            return Ok(true);
        }
        if !waited {
            tracing::debug!(backlog, "stage 2 backlog full; acks wait");
            waited = true;
        }
        tokio::select! {
            () = stop.cancelled() => return Ok(false),
            changed = progress.changed() => {
                if changed.is_err() {
                    return Ok(true);
                }
            }
        }
    }
}

/// Stage 1 for one batch and the acks after its commit; stage 2 picks the rows up in its lanes.
/// A number change in the batch (§6.6) replaces `account`, here and for every other task.
/// `arrived` is when the first envelope of the batch reached the actor, `backlog` whether the
/// account is still reading the server's queue: both go into the measuring mark
/// ([`crate::PERF_STAGE1_STORED`]).
fn receive_batch<T: SignalTransport>(
    shared: &Shared<T>,
    account: &mut Account,
    batch: Vec<crate::net::IncomingEnvelope>,
    arrived: std::time::Instant,
    backlog: bool,
) -> Result<()> {
    let count = batch.len();
    #[cfg(test)]
    {
        let delay = shared.stage1_delay.get();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
    }
    let committed = stage1::commit_batch(
        &shared.db,
        &Stage1 {
            account,
            trust_roots: &shared.trust_roots,
            now: shared.now(),
            wake: &shared.wake,
        },
        batch,
    )?;
    if let Some(changed) = committed.account {
        *shared.account.borrow_mut() = Some(changed.clone());
        *account = changed;
        // The upload of fresh PNI prekeys the change queued.
        shared.jobs_wake.notify_one();
    }
    shared.lanes.wake();
    // The measuring mark of "receive → notification" (§1.3): the batch is durable in the
    // stage-1 inbox. Numbers only; nobody listens unless the target is turned on.
    tracing::trace!(
        target: crate::PERF_TARGET,
        name = crate::PERF_STAGE1_STORED,
        envelopes = count as u64,
        waited_us = u64::try_from(arrived.elapsed().as_micros()).unwrap_or(u64::MAX),
        backlog = u64::from(backlog),
    );
    kourier_db::failpoint::hit(FAILPOINT_AFTER_INBOX_COMMIT);
    for (ack, status) in committed.acks {
        kourier_db::failpoint::hit(FAILPOINT_BEFORE_ACK);
        if let Err(e) = ack.respond(status) {
            // The socket is gone: the server redelivers, stage 1 drops the duplicate.
            tracing::debug!(error = %e, "ack not sent");
        }
    }
    tracing::debug!(envelopes = count, "stage-1 batch committed and acked");
    Ok(())
}

/// The server delivered its backlog: record the boundary (§4.5); the account stays `Syncing`
/// until stage 2 and the core are through it.
fn queue_empty<T: SignalTransport>(shared: &Shared<T>, catch_up: &mut CatchUp) -> Result<()> {
    let counter = shared.db.zone(|tx| -> Result<u64> {
        let counter = db::arrival_counter(tx)?;
        db::set_catch_up_boundary(tx, counter)?;
        Ok(counter)
    })?;
    shared.lanes.set_boundary(Some(counter));
    shared.lanes.wake();
    tracing::debug!(boundary = counter, "queue empty");
    *catch_up = CatchUp::Stage2 { boundary: counter };
    Ok(())
}

/// Moves the catch-up state forward: stage 2 through the boundary (no unhandled inbox row up
/// to it), then every `core_outbox` row written until then acked by the core (rows are
/// deleted only after the ack) → `Online`.
fn check_online<T: SignalTransport>(shared: &Shared<T>, catch_up: &mut CatchUp) -> Result<()> {
    if let CatchUp::Stage2 { boundary } = *catch_up {
        if shared.db.read(|c| stage2::pending_through(c, boundary))? > 0 {
            return Ok(());
        }
        // Every row up to the boundary has its events in core_outbox now.
        let (target, cutoff) = shared.db.read(|c| -> Result<(i64, i64)> {
            Ok(c.query_row(
                "SELECT coalesce(max(seq), 0), coalesce(max(created_at), 0) FROM core_outbox",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })?;
        *catch_up = CatchUp::Core { target, cutoff };
    }
    if let CatchUp::Core { target, cutoff } = *catch_up {
        let waiting: i64 = shared.db.read(|c| -> Result<i64> {
            Ok(c.query_row(
                "SELECT count(*) FROM core_outbox WHERE seq <= ?1 AND created_at <= ?2",
                [target, cutoff],
                |r| r.get(0),
            )?)
        })?;
        if waiting == 0 {
            *catch_up = CatchUp::Online;
            shared.set_conn(ConnState::Online { detail: None });
            // Receipts and read syncs wait for the catch-up (§6.7 step 6).
            shared.jobs_wake.notify_one();
        }
    }
    Ok(())
}

/// After each connect: one-time prekeys (the upload that completes a link, §6.5 step 7) and the
/// connect-time jobs, then the job runner (§6.12) with the post-link gate. A failed step is
/// retried with a backoff while this socket stays up (a login must not wait for a reconnect),
/// and from the start at the next connect; the task is aborted when the socket closes.
async fn post_connect<T: SignalTransport>(shared: Rc<Shared<T>>, socket: Arc<T::Socket>) {
    let mut attempt = 0usize;
    let retry_delay = |attempt: &mut usize| {
        let delay = Duration::from_secs(
            POST_CONNECT_RETRY_SECS[(*attempt).min(POST_CONNECT_RETRY_SECS.len() - 1)],
        );
        *attempt += 1;
        delay
    };
    while let Err(e) = connect_steps(&shared, &socket).await {
        let delay = retry_delay(&mut attempt);
        tracing::warn!(error = %e, retry_in_s = delay.as_secs(), "post-connect steps failed");
        shared.clock.sleep(delay).await;
    }
    let gate = &shared;
    loop {
        match jobs::run(&shared, &socket, || finish_link(gate)).await {
            Ok(never) => match never {},
            Err(Error::CoreGone) => return,
            Err(e) => {
                let delay = retry_delay(&mut attempt);
                tracing::error!(error = %e, retry_in_s = delay.as_secs(), "job runner failed");
                shared.clock.sleep(delay).await;
            }
        }
    }
}

/// One-time keys, then the jobs every connect schedules (remote config, KEYS if needed).
async fn connect_steps<T: SignalTransport>(
    shared: &Shared<T>,
    socket: &Arc<T::Socket>,
) -> Result<()> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    keys::ensure_one_time_keys(&shared.net, socket, &shared.db, &account, shared.now()).await?;
    let now = shared.now();
    shared
        .db
        .zone(|tx| maintenance::schedule_at_connect(tx, now))?;
    Ok(())
}

/// The post-link gate (§6.5 step 8), checked whenever the job runner is idle. The link is
/// complete once its sync request jobs are done (sent, or dropped after their retry budget);
/// then `post_link_sync = 'done'` and, while a login waits, `Done` after the core acked
/// everything written so far (so it has committed `AccountReady` and what followed).
///
/// `InitialSync::LinkOnly` stops there by definition. `ContactsAndStorage` (the app's default)
/// also waits until the phone's contact sync was applied (or `contact_sync::LINK_WAIT` passed
/// without one) and the first Storage Service sync finished (or was given up), so the UI opens
/// on named contacts; after an imported history archive it does not wait for the Storage
/// Service sync (the archive named every chat; Desktop does the same).
///
/// The job runner looks here after every batch too (`jobs::run`): nothing else in the queue
/// (profile fetches, group refreshes, receipts) holds `Done`.
async fn finish_link<T: SignalTransport>(shared: &Shared<T>) -> Result<()> {
    let Some(account) = shared.account() else {
        return Ok(());
    };
    let result = finish_link_steps(shared, account).await;
    // How far the link's work is (`progress.rs`): after the gate, so a `Done` sent here is
    // already counted out.
    crate::progress::report(shared).await?;
    result
}

async fn finish_link_steps<T: SignalTransport>(shared: &Shared<T>, account: Account) -> Result<()> {
    // A history transfer comes before the link is done (Desktop shows its import inside the
    // install screen): the contact sync's answer and the Storage Service sync wait for it,
    // and so does `Done`. Its end wakes the job runner, which comes back here.
    if shared.transfer.holds() {
        return Ok(());
    }
    if !account.post_link_done {
        if shared.db.read(|c| jobs::count(c, JobKind::SyncRequest))? > 0 {
            return Ok(());
        }
        match shared.config.initial_sync {
            InitialSync::LinkOnly => {}
            InitialSync::ContactsAndStorage => {
                let imported = shared.transfer.imported();
                if !shared.db.read(|c| initial_sync_done(c, imported))? {
                    return Ok(());
                }
            }
        }
        shared.db.zone(|tx| -> Result<()> {
            tx.execute(
                "UPDATE account SET post_link_sync = 'done' WHERE id = 1",
                [],
            )?;
            Ok(())
        })?;
        if let Some(stored) = shared.account.borrow_mut().as_mut() {
            stored.post_link_done = true;
        }
        tracing::info!("post-link sync requests sent");
    }
    if shared.login_pending.get() {
        // `Done` only after the core committed `AccountReady` (and what followed), so the UI
        // never leaves the login page for an account the core does not have yet. A connection
        // lost in between finishes this on the next connect.
        shared.wait_acked(crate::outbox::max_seq(&shared.db)?).await;
        shared
            .emit(BackendEvent::Login(LoginStep {
                step_id: "done".to_owned(),
                txn: 1,
                kind: LoginStepKind::Done,
            }))
            .await?;
        shared.login_pending.set(false);
        tracing::info!("link finished");
    }
    Ok(())
}

/// Failed attempts of the first contact or Storage Service sync after which the link stops
/// waiting for it (the job keeps retrying in the background: the Storage Service sync has no
/// expiry and a contact-sync download retries for a day, far too long for a login screen).
const LINK_SYNC_ATTEMPTS: i64 = 3;

/// Contact sync applied (or waited for long enough) and no Storage Service sync (unless
/// `imported`: a history archive was imported) or contact sync left to run, except ones that
/// already failed [`LINK_SYNC_ATTEMPTS`] times.
fn initial_sync_done(conn: &kourier_db::rusqlite::Connection, imported: bool) -> Result<bool> {
    let contacts: bool = conn.query_row(
        "SELECT contact_sync_done = 1 FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let waiting: i64 = conn.query_row(
        "SELECT count(*) FROM jobs WHERE kind IN (?1, ?2) AND attempts < ?3",
        (
            JobKind::ContactSync.as_str(),
            if imported {
                JobKind::ContactSync.as_str()
            } else {
                JobKind::StorageSync.as_str()
            },
            LINK_SYNC_ATTEMPTS,
        ),
        |r| r.get(0),
    )?;
    Ok(contacts && waiting == 0)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;

    use bytes::Bytes;
    use kourier_backend_api::{BackendEvent, Clock as _, Envelope, FakeClock};
    use libsignal_net::chat::fake::{FakeChatRemote, ReceiveRequestError};
    use libsignal_net::chat::{RequestProto, ResponseProto};
    use tokio::task::{LocalSet, spawn_local};

    use super::*;
    use crate::actor::testing;
    use crate::net::ChatSocket as _;
    use crate::receive::tests::{Phone, fixture, text_content};

    const T: u64 = 1_700_000_000_000;

    /// The clock libsignal-net's fake chat server states in its connect response.
    const FAKE_SERVER_MS: u64 = 500;

    fn push(id: u64, path: &str, body: &[u8]) -> RequestProto {
        RequestProto {
            verb: Some("PUT".to_owned()),
            path: Some(path.to_owned()),
            body: Some(Bytes::copy_from_slice(body)),
            headers: vec![format!("X-Signal-Timestamp: {T}")],
            id: Some(id),
        }
    }

    /// Plays the core: commits nothing, acks every durable envelope, counts deliveries of each
    /// message id.
    fn fake_core(
        mut core: mpsc::Receiver<Envelope>,
    ) -> (
        Rc<RefCell<HashMap<String, u32>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = Rc::new(RefCell::new(HashMap::new()));
        let record = seen.clone();
        let task = spawn_local(async move {
            while let Some(envelope) = core.recv().await {
                let Some(ack) = envelope.ack else { continue };
                for event in envelope.events {
                    if let BackendEvent::Message(message) = event {
                        *record
                            .borrow_mut()
                            .entry(message.local.to_string())
                            .or_insert(0) += 1;
                    }
                }
                ack.complete(Ok(()));
            }
        });
        (seen, task)
    }

    /// §10.1 "Signal transport": 1,000 envelopes queued on libsignal-net's fake chat server
    /// before a slow stage 1 takes any (the listener runs on the network runtime's threads and
    /// may not block): every envelope is acked, after its commit, none twice, none lost, and
    /// the account goes Online once the core has all of them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_thousand_queued_envelopes_with_a_slow_stage_one_are_neither_dropped_nor_stalled() {
        LocalSet::new()
            .run_until(async {
                let fixture = fixture();
                let mut phone = Phone::new(&fixture);
                let envelopes: Vec<Vec<u8>> = (0..1000u64)
                    .map(|i| {
                        phone.envelope(&fixture, &text_content(&format!("m{i}"), T + i), T + i)
                    })
                    .collect();
                let mut test =
                    testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                // The fake server's connect response states its clock: ours agrees.
                test.shared.clock = Arc::new(FakeClock::new(FAKE_SERVER_MS));
                // A slow decrypt: every stage-1 batch blocks the actor thread for 20 ms.
                test.shared.stage1_delay.set(Duration::from_millis(20));
                let testing::TestShared {
                    shared,
                    core,
                    acked,
                    mut conn,
                } = test;
                let shared = Rc::new(shared);
                let stop = CancellationToken::new();
                let forwarder = spawn_local(crate::outbox::forward(
                    shared.db.clone(),
                    shared.events.clone(),
                    shared.wake.clone(),
                    acked,
                    stop.clone(),
                ));
                let driver = spawn_local(stage2::drive(shared.clone(), stop.clone()));
                let (seen, core_task) = fake_core(core);
                let (socket, remote, events) =
                    crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
                let socket = Arc::new(socket);
                for (id, bytes) in (0u64..).zip(&envelopes) {
                    remote
                        .send_request(push(id, "/api/v1/message", bytes))
                        .unwrap();
                }
                remote
                    .send_request(push(1000, "/api/v1/queue/empty", b""))
                    .unwrap();
                let (_nudge, mut nudges) = mpsc::unbounded_channel();
                let receive = {
                    let (shared, stop) = (shared.clone(), stop.clone());
                    let (account, socket) = (fixture.account.clone(), socket.clone());
                    spawn_local(async move {
                        receive_loop(&shared, account, events, &stop, &mut nudges, &socket).await
                    })
                };

                // Every envelope is answered, in time (the fake server panics after 3 s of
                // silence).
                let mut responses = 0;
                while responses < 1000 {
                    match remote.receive_request().await {
                        Err(ReceiveRequestError::GotResponse) => responses += 1,
                        other => panic!("unexpected from the client: {other:?}"),
                    }
                }
                tokio::time::timeout(Duration::from_secs(30), conn.wait_for(ConnState::is_online))
                    .await
                    .expect("Online in time")
                    .expect("conn watch");
                {
                    let seen = seen.borrow();
                    assert_eq!(seen.len(), 1000, "no message lost");
                    assert!(seen.values().all(|n| *n == 1), "no message twice");
                }
                let left = |table: &str| {
                    shared
                        .db
                        .read(|c| -> crate::error::Result<i64> {
                            Ok(
                                c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
                                    r.get(0)
                                })?,
                            )
                        })
                        .unwrap()
                };
                assert_eq!(left("inbox"), 0);
                assert_eq!(left("core_outbox"), 0);
                assert_eq!(shared.db.read(db::arrival_counter).unwrap(), 1000);

                stop.cancel();
                assert!(matches!(receive.await.unwrap(), LoopEnd::Stop));
                driver.await.unwrap();
                forwarder.await.unwrap();
                drop(shared);
                core_task.await.unwrap();
                socket.disconnect().await;
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stage_one_waits_while_the_stage_two_backlog_is_full() {
        LocalSet::new()
            .run_until(async {
                let fixture = fixture();
                let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                let shared = Rc::new(test.shared);
                shared
                    .db
                    .zone(|tx| -> crate::error::Result<()> {
                        for arrival in 1..=MAX_BACKLOG + 1 {
                            tx.execute(
                                "INSERT INTO inbox(arrival, received_at, kind, lane, our_kind,
                                   sender, sender_device, timestamp, sealed)
                                 VALUES (?1, 0, 'delivery_receipt', 'misc', 'aci', 'x', 1, 0, 0)",
                                [arrival],
                            )?;
                        }
                        Ok(())
                    })
                    .unwrap();
                let stop = CancellationToken::new();
                let waiting = {
                    let shared = shared.clone();
                    let stop = stop.clone();
                    spawn_local(async move {
                        let mut progress = shared.lanes.progress();
                        backpressure(&shared, &mut progress, &stop).await
                    })
                };
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert!(!waiting.is_finished(), "stage 1 waits");
                // Stage 2 makes progress (the lanes handle the receipts).
                let driver = spawn_local(stage2::drive(shared.clone(), stop.clone()));
                let resumed = tokio::time::timeout(Duration::from_secs(30), waiting)
                    .await
                    .expect("resumed in time")
                    .unwrap();
                assert!(resumed.unwrap());
                stop.cancel();
                driver.await.unwrap();
            })
            .await;
    }

    /// `Syncing` → `Online` once the rows written before stage 2 reached the boundary are
    /// acked, even when `core_outbox` emptied meanwhile and later rows arrived.
    #[tokio::test(flavor = "current_thread")]
    async fn online_waits_for_the_boundary_rows_only_even_when_seqs_restart() {
        LocalSet::new()
            .run_until(async {
                let fixture = fixture();
                let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                let shared = test.shared;
                let row = |created_at: i64| {
                    shared
                        .db
                        .zone(|tx| tx.outbox_append(b"row", created_at).map_err(Error::from))
                        .unwrap()
                };
                let delete_all = || {
                    shared
                        .db
                        .zone(|tx| -> Result<()> {
                            tx.execute("DELETE FROM core_outbox", [])?;
                            Ok(())
                        })
                        .unwrap();
                };
                assert_eq!(row(100), 1);
                assert_eq!(row(100), 2);
                let mut catch_up = CatchUp::Stage2 { boundary: 0 };
                check_online(&shared, &mut catch_up).unwrap();
                assert_eq!(
                    catch_up,
                    CatchUp::Core {
                        target: 2,
                        cutoff: 100
                    },
                    "stage 2 is through; the core has two rows left"
                );
                assert!(!test.conn.borrow().is_online());

                // The core acked both; the table emptied, and live traffic wrote a row before
                // this check ran (seqs are never reused since kourier-db's AUTOINCREMENT
                // migration; the check must not depend on either).
                delete_all();
                assert_eq!(row(200), 3, "seqs are never reused");
                check_online(&shared, &mut catch_up).unwrap();
                assert_eq!(catch_up, CatchUp::Online);
                assert!(test.conn.borrow().is_online());

                // A boundary row still unacked holds Online back.
                delete_all();
                row(300);
                let mut catch_up = CatchUp::Stage2 { boundary: 0 };
                check_online(&shared, &mut catch_up).unwrap();
                check_online(&shared, &mut catch_up).unwrap();
                assert!(matches!(catch_up, CatchUp::Core { .. }));
            })
            .await;
    }

    /// `ContactsAndStorage` waits for the contact sync and the first Storage Service sync, but
    /// not for ever: a job that keeps failing transiently stops holding the login after
    /// [`LINK_SYNC_ATTEMPTS`] attempts (it keeps retrying in the background).
    #[test]
    fn the_link_gate_waits_for_both_syncs_but_not_for_failing_ones() {
        let fixture = fixture();
        let done = || fixture.db.read(|c| initial_sync_done(c, false)).unwrap();
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE account SET contact_sync_done = 0", [])?;
                jobs::enqueue(tx, JobKind::StorageSync, "", 0, None)?;
                Ok(())
            })
            .unwrap();
        assert!(!done(), "no contact sync yet");
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute("UPDATE account SET contact_sync_done = 1", [])?;
                Ok(())
            })
            .unwrap();
        assert!(!done(), "the storage sync is still queued");
        assert!(
            fixture.db.read(|c| initial_sync_done(c, true)).unwrap(),
            "after an imported archive the storage sync does not hold the link"
        );
        for attempt in 1..=LINK_SYNC_ATTEMPTS {
            fixture
                .db
                .zone(|tx| -> Result<()> {
                    let job = jobs::next_due(tx, i64::MAX, true, false)?.expect("storage sync");
                    jobs::retry(tx, &job, 0, "server answered 503", false, None)?;
                    Ok(())
                })
                .unwrap();
            assert_eq!(done(), attempt == LINK_SYNC_ATTEMPTS, "attempt {attempt}");
        }
        assert_eq!(
            fixture
                .db
                .read(|c| jobs::count(c, JobKind::StorageSync))
                .unwrap(),
            1,
            "still retried"
        );
    }

    /// One receive loop on libsignal-net's fake chat server, with a clock the test moves.
    struct Loop {
        shared: Rc<Shared<testing::NoTransport>>,
        clock: FakeClock,
        remote: FakeChatRemote,
        nudge: mpsc::UnboundedSender<Nudge>,
        stop: CancellationToken,
        task: tokio::task::JoinHandle<LoopEnd>,
        _core: mpsc::Receiver<Envelope>,
    }

    impl Loop {
        /// Starts the loop with the local clock at `clock_ms` and a keepalive every
        /// `keepalive` of that clock.
        fn start(clock_ms: u64, keepalive: Duration) -> Self {
            let fixture = fixture();
            let mut test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
            let clock = FakeClock::new(clock_ms);
            test.shared.clock = Arc::new(clock.clone());
            test.shared.tuning.keepalive_interval = keepalive;
            let shared = Rc::new(test.shared);
            let (socket, remote, events) =
                crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
            let socket = Arc::new(socket);
            let (nudge, mut nudges) = mpsc::unbounded_channel();
            let stop = CancellationToken::new();
            let task = {
                let (shared, stop) = (shared.clone(), stop.clone());
                let account = fixture.account.clone();
                spawn_local(async move {
                    // The fixture (and its database directory) lives as long as the loop.
                    let _fixture = fixture;
                    receive_loop(&shared, account, events, &stop, &mut nudges, &socket).await
                })
            };
            Self {
                shared,
                clock,
                remote,
                nudge,
                stop,
                task,
                _core: test.core,
            }
        }

        /// The next request the client sent to the fake server.
        async fn request(&self) -> RequestProto {
            self.remote
                .receive_request()
                .await
                .expect("a request")
                .expect("the socket is open")
        }

        fn answer(&self, request: &RequestProto, status: u32) {
            self.remote
                .send_response(ResponseProto {
                    id: request.id,
                    status: Some(status),
                    message: None,
                    headers: Vec::new(),
                    body: None,
                })
                .expect("the socket is open");
        }

        /// Whether the client sends nothing for a moment.
        async fn silent(&self) -> bool {
            tokio::time::timeout(Duration::from_millis(150), self.remote.receive_request())
                .await
                .is_err()
        }

        /// How the loop ended, within a few seconds.
        async fn end(self) -> LoopEnd {
            tokio::time::timeout(Duration::from_secs(10), self.task)
                .await
                .expect("the loop ended")
                .expect("the loop did not panic")
        }

        /// Lets the loop run until it has armed what the test waits for.
        async fn settle(&self) {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    fn is_keepalive(request: &RequestProto) -> bool {
        request.verb.as_deref() == Some("GET")
            && request.path.as_deref() == Some(crate::net::KEEPALIVE_PATH)
    }

    /// §6.4: a keepalive goes out one interval after the connect and after every answer; a
    /// server that answers anything but success is taken for dead and the socket is replaced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keepalives_follow_each_answer_and_a_refusal_ends_the_socket() {
        LocalSet::new()
            .run_until(async {
                let interval = Duration::from_secs(30);
                let run = Loop::start(FAKE_SERVER_MS, interval);
                run.settle().await;
                // Nothing before the interval is over.
                run.clock.advance(interval - Duration::from_millis(1));
                run.settle().await;
                assert!(!run.task.is_finished());
                run.clock.advance(Duration::from_millis(1));
                let first = run.request().await;
                assert!(is_keepalive(&first), "{first:?}");
                // No second keepalive while the first is unanswered, however late it is.
                run.clock.advance(interval);
                assert!(run.silent().await, "one keepalive at a time");
                run.answer(&first, 200);
                assert!(run.silent().await, "the next one counts from the answer");
                run.clock.advance(interval);
                let second = run.request().await;
                assert!(is_keepalive(&second));
                assert_ne!(second.id, first.id);
                run.answer(&second, 500);
                assert!(matches!(run.end().await, LoopEnd::Dead { healthy: false }));
            })
            .await;
    }

    /// §11 S15 "resume after 10 min suspend": timers do not run while the machine sleeps, the
    /// wall clock does. On `Resumed` a socket silent for more than five minutes is given up
    /// without asking, so the reconnect starts at once; after a short sleep the socket is
    /// asked, and lives on when it answers.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_resume_replaces_a_stale_socket_and_asks_a_fresh_one() {
        LocalSet::new()
            .run_until(async {
                let interval = Duration::from_secs(30);
                // A short sleep: the socket is asked, with the short timeout of a check.
                let run = Loop::start(FAKE_SERVER_MS, interval);
                run.settle().await;
                run.clock.set(FAKE_SERVER_MS + 60_000);
                run.nudge.send(Nudge::Resumed).unwrap();
                let check = run.request().await;
                assert!(is_keepalive(&check));
                run.answer(&check, 200);
                run.settle().await;
                assert!(
                    !run.task.is_finished(),
                    "an answered check keeps the socket"
                );
                // A network change asks as well.
                run.nudge.send(Nudge::NetworkChanged).unwrap();
                let check = run.request().await;
                assert!(is_keepalive(&check));
                run.answer(&check, 200);
                run.settle().await;
                // `Connect` while connected changes nothing.
                run.nudge.send(Nudge::Connect).unwrap();
                run.settle().await;
                assert!(!run.task.is_finished());

                // Ten minutes asleep: no keepalive is sent, the socket is replaced.
                let asleep = run.clock.now_ms() + 10 * 60 * 1000;
                run.clock.set(asleep);
                run.nudge.send(Nudge::Resumed).unwrap();
                assert!(matches!(run.end().await, LoopEnd::Stale));

                // Without a hint from the system, the late keepalive timer finds the same.
                let run = Loop::start(FAKE_SERVER_MS, interval);
                run.settle().await;
                run.clock.set(FAKE_SERVER_MS + 10 * 60 * 1000);
                assert!(matches!(run.end().await, LoopEnd::Stale));
            })
            .await;
    }

    /// §6.4: a check the system asked for (the network changed, the machine woke up) does not
    /// wait behind a routine keepalive that is still unanswered: on a socket that died without
    /// a close, that one would hold the reconnect back for its whole 30 s. The check goes out
    /// at once with its own short timeout; its answer keeps the socket, its silence ends it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hint_asks_again_while_a_routine_keepalive_is_unanswered() {
        LocalSet::new()
            .run_until(async {
                let interval = Duration::from_secs(30);
                let run = Loop::start(FAKE_SERVER_MS, interval);
                run.settle().await;
                run.clock.advance(interval);
                let routine = run.request().await;
                assert!(is_keepalive(&routine), "{routine:?}");

                // The routine keepalive is unanswered when the network changes.
                run.nudge.send(Nudge::NetworkChanged).unwrap();
                let check = run.request().await;
                assert!(is_keepalive(&check), "{check:?}");
                assert_ne!(check.id, routine.id, "a new request, not the old one");
                // A second hint finds the short check on its way and adds nothing.
                run.nudge.send(Nudge::Resumed).unwrap();
                assert!(run.silent().await, "one check at a time");
                // The check's answer keeps the socket; the late answer of the replaced
                // keepalive changes nothing.
                run.answer(&check, 200);
                run.answer(&routine, 500);
                run.settle().await;
                assert!(
                    !run.task.is_finished(),
                    "an answered check keeps the socket"
                );
                assert!(
                    run.silent().await,
                    "the next keepalive counts from the answer"
                );

                // The next routine keepalive, unanswered again; so is the check of a hint.
                run.clock.advance(interval);
                let routine = run.request().await;
                assert!(is_keepalive(&routine));
                let asked = std::time::Instant::now();
                run.nudge.send(Nudge::NetworkChanged).unwrap();
                let check = run.request().await;
                assert_ne!(check.id, routine.id);
                assert!(matches!(run.end().await, LoopEnd::Dead { healthy: false }));
                let took = asked.elapsed();
                assert!(
                    took >= health::CHECK_TIMEOUT - Duration::from_millis(500)
                        && took < health::CHECK_TIMEOUT + Duration::from_secs(3),
                    "given up after the check's timeout, not the keepalive's 30 s: {took:?}"
                );
            })
            .await;
    }

    /// §4.7, §6.4: a hint from the system that arrives while a connect attempt is on its way
    /// is not lost when that attempt fails: the failure belongs to the network as it was, so
    /// the next attempt starts at once instead of after the delay the failures had grown to.
    /// Without a hint, a failed attempt waits its delay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_hint_during_a_failing_connect_attempt_is_not_lost() {
        use std::sync::atomic::Ordering;
        LocalSet::new()
            .run_until(async {
                let fixture = fixture();
                let mut test =
                    testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                let clock = FakeClock::new(T);
                test.shared.clock = Arc::new(clock.clone());
                let shared = Rc::new(test.shared);
                let conn = test.conn;
                let transport = shared.net.transport();
                transport.auth_held.store(true, Ordering::SeqCst);
                let attempts = || transport.auth_attempts.load(Ordering::SeqCst);
                let until = |what: &'static str, n: usize| {
                    let conn = conn.clone();
                    async move {
                        let started = std::time::Instant::now();
                        while transport.auth_attempts.load(Ordering::SeqCst) != n {
                            assert!(
                                started.elapsed() < Duration::from_secs(5),
                                "{what}: {} attempts, {:?}",
                                transport.auth_attempts.load(Ordering::SeqCst),
                                *conn.borrow()
                            );
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                    }
                };
                let settle = || async {
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                };
                let retry_at = || match &*conn.borrow() {
                    ConnState::Offline { retry_at_ms } => *retry_at_ms,
                    _ => None,
                };
                let (nudge, nudges) = mpsc::unbounded_channel();
                let stop = CancellationToken::new();
                let task = spawn_local(run(shared.clone(), stop.clone(), nudges));

                // The first attempt fails with no hint: the first delay (1 s) is waited out.
                settle().await;
                until("the first attempt", 1).await;
                transport.auth_fail.notify_one();
                settle().await;
                assert_eq!(retry_at(), Some(T + 1_000), "{:?}", *conn.borrow());
                assert_eq!(attempts(), 1, "no attempt before the delay is over");
                clock.advance(Duration::from_secs(1));
                settle().await;
                until("the second attempt", 2).await;
                // The second one fails too: 2 s now. Three more failures grow it further.
                for (n, delay_s) in [(2, 2), (3, 3), (4, 5), (5, 8)] {
                    transport.auth_fail.notify_one();
                    settle().await;
                    let at = retry_at().expect("waiting for the next attempt");
                    assert!(
                        at >= clock.now_ms() + delay_s * 1_000
                            && at <= clock.now_ms() + delay_s * 1_000 + 5_000,
                        "attempt {n} waits {delay_s} s (plus jitter above 5 s)"
                    );
                    assert_eq!(attempts(), n);
                    clock.advance(Duration::from_secs(delay_s + 5));
                    settle().await;
                    until("the next attempt", n + 1).await;
                }

                // The network changes while the sixth attempt is on its way; then it fails.
                assert_eq!(attempts(), 6);
                nudge.send(Nudge::NetworkChanged).unwrap();
                nudge.send(Nudge::NetworkChanged).unwrap();
                settle().await;
                transport.auth_fail.notify_one();
                settle().await;
                // The seventh starts at once: the clock has not moved.
                until("the attempt after the hint", 7).await;
                // It fails without a hint: one retry per burst, and the delays start over.
                transport.auth_fail.notify_one();
                settle().await;
                assert_eq!(retry_at(), Some(clock.now_ms() + 1_000));
                assert_eq!(attempts(), 7);

                // An attempt the server itself refused is another matter: the network works,
                // and a hint that came meanwhile does not hurry the next one.
                assert!(answered_by_server(&TransportError::Rejected {
                    status: 503
                }));
                assert!(answered_by_server(&TransportError::RetryLater {
                    retry_after_secs: 30
                }));
                assert!(!answered_by_server(&TransportError::Timeout));
                assert!(!answered_by_server(&TransportError::Unreachable(
                    "no route".to_owned()
                )));
                assert!(!answered_by_server(&TransportError::Disconnected));

                stop.cancel();
                let end = tokio::time::timeout(Duration::from_secs(5), task)
                    .await
                    .expect("stopped")
                    .expect("no panic");
                assert!(matches!(end, ConnectionEnd::Stopped));
                // Lets the attempt still held on the network runtime end.
                transport.auth_fail.notify_one();
                drop(fixture);
            })
            .await;
    }

    /// §9.6: while the clock is wrong the device stays disconnected and looks at the clock
    /// again on every hint; a resume ends the wait, because the server's time carried forward
    /// by the monotonic clock is behind by the length of the sleep (a clock set right after
    /// two days asleep would look two days ahead for ever).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wrong_clock_waits_for_the_clock_and_asks_the_server_again_after_a_resume() {
        const HOUR: u64 = 60 * 60 * 1000;
        LocalSet::new()
            .run_until(async {
                let waiting = |local_ms: u64| {
                    let fixture = fixture();
                    let mut test =
                        testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                    let clock = FakeClock::new(local_ms);
                    test.shared.clock = Arc::new(clock.clone());
                    // Only hints make it look again.
                    test.shared.tuning.clock_recheck = Duration::from_secs(1_000_000);
                    let shared = Rc::new(test.shared);
                    let (nudge, mut nudges) = mpsc::unbounded_channel();
                    let stop = CancellationToken::new();
                    let task = {
                        let (shared, stop) = (shared.clone(), stop.clone());
                        spawn_local(async move {
                            let _fixture = fixture;
                            let server = ServerClock::now(FAKE_SERVER_MS);
                            wait_for_clock(&shared, &stop, &mut nudges, server).await
                        })
                    };
                    (clock, nudge, stop, task, test.conn, test.core)
                };
                let settle = || async {
                    for _ in 0..20 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::sleep(Duration::from_millis(30)).await;
                };
                let ended = |task: tokio::task::JoinHandle<bool>| async {
                    tokio::time::timeout(Duration::from_secs(5), task)
                        .await
                        .expect("the wait ended")
                        .expect("the wait did not panic")
                };
                let fix_clock = |conn: &watch::Receiver<ConnState>| {
                    matches!(
                        &*conn.borrow(),
                        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms })
                            if (*skew_ms - 25 * 3_600_000).abs() < 10_000
                    )
                };

                // 25 hours ahead: reported, and a network change or a `Connect` only looks
                // again (no connect while the clock is wrong).
                let (clock, nudge, _stop, task, conn, _core) = waiting(FAKE_SERVER_MS + 25 * HOUR);
                settle().await;
                assert!(fix_clock(&conn), "{:?}", *conn.borrow());
                nudge.send(Nudge::NetworkChanged).unwrap();
                nudge.send(Nudge::Connect).unwrap();
                settle().await;
                assert!(!task.is_finished(), "still waiting for the clock");
                // The user sets the clock: the next look finds it right.
                clock.set(FAKE_SERVER_MS);
                nudge.send(Nudge::NetworkChanged).unwrap();
                assert!(ended(task).await, "connect again");

                // Still wrong at a resume: the wait ends all the same, for a fresh server time.
                let (_clock, nudge, _stop, task, conn, _core) = waiting(FAKE_SERVER_MS + 25 * HOUR);
                settle().await;
                assert!(fix_clock(&conn));
                nudge.send(Nudge::Resumed).unwrap();
                assert!(ended(task).await, "connect again to hear the server's time");

                // Stopped while waiting.
                let (_clock, _nudge, stop, task, _conn, _core) =
                    waiting(FAKE_SERVER_MS + 25 * HOUR);
                settle().await;
                stop.cancel();
                assert!(!ended(task).await);
            })
            .await;
    }

    /// §6.4, §9.6: the server's clock arrives with the connect response; a local clock more
    /// than the allowed skew away from it ends the socket (ahead or behind), one within it
    /// does not, and the remote config can narrow the limit.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_clock_too_far_from_the_servers_ends_the_socket() {
        LocalSet::new()
            .run_until(async {
                const HOUR: u64 = 60 * 60 * 1000;
                let far = Duration::from_secs(1_000_000);
                let ahead = Loop::start(FAKE_SERVER_MS + 25 * HOUR, far);
                let LoopEnd::Skewed(clock) = ahead.end().await else {
                    panic!("a clock 25 h ahead is skewed");
                };
                let skew = clock.skew_ms(FAKE_SERVER_MS + 25 * HOUR);
                let hours = i64::try_from(25 * HOUR).unwrap();
                assert!((skew - hours).abs() < 10_000, "positive: ahead ({skew})");

                // 23 hours ahead is within the default of a day.
                let fine = Loop::start(FAKE_SERVER_MS + 23 * HOUR, far);
                fine.settle().await;
                assert!(!fine.task.is_finished());
                fine.stop.cancel();
                assert!(matches!(fine.end().await, LoopEnd::Stop));

                // The remote config allows two hours: three are too many.
                let fixture = fixture();
                fixture
                    .db
                    .zone(|tx| -> Result<()> {
                        tx.execute(
                            "INSERT INTO remote_config(name, value) VALUES (?1, '7200')",
                            [health::MAX_SKEW_KEY],
                        )?;
                        Ok(())
                    })
                    .unwrap();
                let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                assert_eq!(max_skew(&test.shared), Duration::from_secs(7200));
                let clock = ServerClock::now(FAKE_SERVER_MS);
                assert!(clock.is_skewed(FAKE_SERVER_MS + 3 * HOUR, max_skew(&test.shared)));
                assert!(!clock.is_skewed(FAKE_SERVER_MS + HOUR, max_skew(&test.shared)));
            })
            .await;
    }

    /// A device found unlinked outside the connection task (the refusal of our PNI prekeys)
    /// ends the socket, also when it was found between two sockets.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unlink_found_elsewhere_ends_the_socket() {
        LocalSet::new()
            .run_until(async {
                let far = Duration::from_secs(1_000_000);
                let run = Loop::start(FAKE_SERVER_MS, far);
                run.settle().await;
                assert!(!run.task.is_finished());
                run.shared.unlink("keys refused");
                let shared = run.shared.clone();
                let LoopEnd::Unlinked(reason) = run.end().await else {
                    panic!("unlinked");
                };
                assert_eq!(reason, "keys refused");

                // The next socket of the same actor does not live either.
                let (socket, _remote, events) =
                    crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
                let (_nudge, mut nudges) = mpsc::unbounded_channel();
                let account = shared.account().unwrap();
                let end = receive_loop(
                    &shared,
                    account,
                    events,
                    &CancellationToken::new(),
                    &mut nudges,
                    &Arc::new(socket),
                )
                .await;
                assert!(matches!(end, LoopEnd::Unlinked(_)));
            })
            .await;
    }
    /// What a measuring build reads from stage 1 (§1.3 "receive → notification"): one event
    /// on the perf target per stored batch, a fixed name and numbers, nothing else.
    #[derive(Clone, Default)]
    struct PerfMarks(std::sync::Arc<std::sync::Mutex<Vec<PerfMark>>>);

    #[derive(Clone, Debug, Default, PartialEq)]
    struct PerfMark {
        name: String,
        numbers: std::collections::BTreeMap<String, u64>,
        texts: Vec<String>,
    }

    impl tracing::field::Visit for PerfMark {
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.numbers.insert(field.name().to_owned(), value);
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "name" {
                value.clone_into(&mut self.name);
            } else {
                self.texts.push(field.name().to_owned());
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {
            self.texts.push(field.name().to_owned());
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for PerfMarks {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if event.metadata().target() == crate::PERF_TARGET {
                let mut mark = PerfMark::default();
                event.record(&mut mark);
                self.0.lock().unwrap().push(mark);
            }
        }
    }

    #[test]
    fn a_stored_batch_leaves_a_numeric_mark_on_the_perf_target() {
        use tracing_subscriber::layer::SubscriberExt as _;
        let marks = PerfMarks::default();
        let subscriber = tracing_subscriber::registry().with(marks.clone());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tracing::subscriber::with_default(subscriber, || {
            runtime.block_on(async {
                let fixture = fixture();
                let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
                let mut account = fixture.account.clone();
                let envelope = |bytes: &'static [u8]| crate::net::IncomingEnvelope {
                    bytes: Bytes::from_static(bytes),
                    server_delivery_timestamp: 0,
                    ack: crate::net::ServerAck::new(|_| Ok(())),
                };
                // Two envelopes that arrived 80 ms ago (the batching wait), live.
                let arrived = std::time::Instant::now() - Duration::from_millis(80);
                receive_batch(
                    &test.shared,
                    &mut account,
                    vec![envelope(b"not an envelope"), envelope(b"")],
                    arrived,
                    false,
                )
                .unwrap();
                // One from the server's backlog.
                receive_batch(
                    &test.shared,
                    &mut account,
                    vec![envelope(b"x")],
                    std::time::Instant::now(),
                    true,
                )
                .unwrap();
            });
        });
        let marks = marks.0.lock().unwrap().clone();
        assert_eq!(marks.len(), 2, "one mark per batch: {marks:?}");
        assert_eq!(marks[0].name, crate::PERF_STAGE1_STORED);
        assert_eq!(marks[0].numbers["envelopes"], 2);
        assert_eq!(marks[0].numbers["backlog"], 0);
        assert!(
            marks[0].numbers["waited_us"] >= 80_000,
            "from the arrival of the first envelope: {marks:?}"
        );
        assert_eq!(marks[1].numbers["envelopes"], 1);
        assert_eq!(marks[1].numbers["backlog"], 1);
        for mark in &marks {
            assert!(mark.texts.is_empty(), "numbers only: {mark:?}");
            assert_eq!(
                mark.numbers.keys().map(String::as_str).collect::<Vec<_>>(),
                ["backlog", "envelopes", "waited_us"]
            );
        }
    }
}
