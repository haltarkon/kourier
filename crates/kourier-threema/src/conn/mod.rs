//! The account's connection (TH3; Signal's shape, `kourier-signal/src/connection.rs`): ONE task on
//! the actor's thread that connects to the mediator through the parts' WebSocket connector, runs
//! libthreema's `D2mProtocol` and the `CspProtocol` inside its Proxy payloads, keeps the socket
//! alive, and after a close decides with the close table what comes next (connect again after the
//! backoff, hold for the user, or the end of the link).
//!
//! - THE STATES (brief §2.3, critic H2): `Connecting` while the socket and the handshakes run;
//!   `Syncing` at the CSP login-ack; `Online { "follower" | "leader" }` once the mediator's
//!   `ReflectionQueueDry` came AND the core acked every `core_outbox` row written before it (THE
//!   CATCH-UP BOUNDARY, `docs/architecture.md` §4.5, TH5 A5b). Dry is COUNTED after the pass's zone
//!   and only once nothing waits in step 0's hold (a Dry that came during a join is counted after
//!   zone 2 and the release of what waited): then the boundary is `outbox::max_seq`, and the
//!   forwarder's `acked` ([`Setup::acked`]) reaching it publishes `Online` (at once when it already
//!   has: an empty backlog). Dry and the promotion usually come BEFORE the login-ack (the mediator
//!   answers the held CSP hello after them): they are remembered, and the login-ack publishes what
//!   they decided. The boundary is the SOCKET's: a new socket (a reconnect, after a `Disconnect` or a
//!   socket loss) starts without one, so its backlog is a catch-up again; a core that never acks
//!   keeps `Syncing`. `catch_up` of a reflected message: `true` until Dry is counted (so also for
//!   everything that waited in step 0's hold, Dry or not), `false` after.
//! - THE REFLECTION LOOP (TH5 A5a, `crate::reflect`): the socket's queue is DRAINED with `try_recv`
//!   up to [`reflect::BATCH`] datagrams per pass (decision H1), so a backlog is a batch; the pass's
//!   reflected envelopes are applied in ONE zone and only then acked, one `reflected-ack` per id in
//!   arrival order; Dry is applied after the zone. An EPHEMERAL one (by its flag) has no zone, no nonce and
//!   no ack (C5, M5): TH5b B3 (R12) opens it with `reflect::open_ephemeral` and sends an incoming typing
//!   indicator to the core as ONE `Envelope::ephemeral(Typing)` with `try_send` on [`Setup::events`] (typing
//!   is lossy: a full or closed channel drops it, the step never waits for the core). STEP 0 (B1): while the file is not join-ready (a join between its first
//!   `ServerInfo` and zone 2) the envelopes wait in memory (at most 30 or 2 MiB; over that the socket
//!   closes 1000 and the mediator delivers them again) and are applied right after zone 2 commits
//!   ([`Setup::ready`]). Once the socket's end is set the batch is applied but nothing is acked (L13); a
//!   close the drain found among the datagrams is the same (L-2: the batch is applied, no data frame goes out
//!   after the server's close). A socket that ended over the hold's bound reconnects as soon as the file is
//!   ready, or after one backoff step, never in a tight loop (S10).
//!   Nothing asks the chat server for messages ([`csp::CspSend`]).
//! - EVERY TIMER IS THE ACTOR'S CLOCK (`ctx.clock`): the deadlines of the connect and the handshakes,
//!   the keep-alive, the backoff. No tokio sleep and no `Instant` here.
//! - THE HANDSHAKE'S DEADLINES, BY PHASE (critic M10, TH3 fix P-L7): [`HANDSHAKE_WITHIN`] (20 s)
//!   covers DNS + TCP + TLS + the upgrade + the D2M handshake up to `ServerInfo`; from `ServerInfo`
//!   on the CSP login has its own [`LOGIN_WITHIN`] (20 s) that EVERY frame heard pushes out (a
//!   reflection backlog in progress is progress; the mediator may answer the held CSP hello only
//!   after it, [U: real server] RT1).
//! - THE JOIN'S FIRST SOCKET (TH4 J5a, brief §2.6, decisions H1 and M1): a task started with
//!   [`Setup::join`] reports the `device_slot_state` of the FIRST `ServerInfo` on the hook BEFORE the
//!   slot is marked existing ([`SlotReport`]); anything but `NEW` (spec-impossible for an expected
//!   NEW, a belt) closes with 1000 and ends the task [`End::JoinRefused`]. On that socket L13's
//!   crash-window retry is OFF: a 4115, any 41xx, a timeout or any other end before `ServerInfo`
//!   reports [`SlotReport::Failed`] and ends the task the same way. The hook is gone after the first
//!   `ServerInfo`: a later reconnect is TH3's business.
//! - THE SEND QUEUE'S TURN (TH6a S5a, `crate::send`): the socket runs ONE row at a time, [`Session::pump`] at the end of
//!   every pass of [`Session::step`], after both protocols have said what they had. It hands the row's machine the
//!   reflect-acks and message-acks of the pass, runs the zones they allow and writes what those zones allow: a frame goes
//!   only after the zone that holds its bytes or proves its turn committed. A pass whose zone commits while the socket
//!   is going writes nothing (L4: the zone is what the server said, the frame would come after the server's close, or
//!   on a socket that broke). A `transmit` row runs from the login-ack while a `Transmit` is attached, a `job` row from
//!   `Online`, an `acked` row by itself from the login-ack (`queue::next_send`), and none of it needs the leader or the
//!   chat server's unblock: a follower sends. A zone that the STORE fails closes 1000 (`Outcome::Store`); one that fails
//!   by its CONTENT cools that row and later rows go.
//! - WHEN AN ACK STAYS AWAY (TH6a S5b, decisions C11, R2, L1; `crate::send::shared`): frames of the running row unanswered
//!   for [`ACK_WITHIN`] of the actor's clock, from the last of them written or the last ack of it heard, close 1000
//!   (`Outcome::NoAck`), and the next socket writes the same bytes. An echo pushes nothing: an account whose keep-alive
//!   answers and whose row does not is what this catches. A reflection before Dry does: that is the backlog the reflect-ack
//!   comes behind. The socket's end decides the rest ([`Session::socket_ended`]): frames unanswered at a missing ack or at
//!   the server's close are a failed run of the row, and from the second the row cools, the socket stays up for what else
//!   there is and wakes when the row may run again.
//! - A WRITE IS BOUNDED (C14, L5; [`Session::send`]): a stalled write ends the socket after [`SEND_WITHIN`] of the actor's
//!   clock, closed 1000, as `Outcome::Dead`: a stalled write says nothing about a row, so nothing cools.
//! - THE QUEUE'S SWEEP (C15): at EVERY socket's `ServerInfo` whose clock check passed, never under a clock nothing proved, so
//!   a long-running desktop keeps no spent budget until a restart.
//! - THE OWN DEVICE'S DROP (RP-b, [`drop_own`]): at a logout the actor asks the running connection
//!   ([`Task::drop_own`]): a socket past `ServerInfo` writes `DropDevice` of the own id and sends no frame of the
//!   send queue after it; the ack of that id, or the server's 4113 / 4114 after the ask, ends the task
//!   [`End::Dropped`]. A socket before `ServerInfo`, a connection between sockets or one that ends otherwise
//!   after the ask is closed with 1000 and connects no more ([`End::Stopped`]): the actor drops the device on
//!   a D2M-only socket of its own ([`drop_alone`]). Nothing here registers a slot for it.
//! - Logged: names, codes and kinds only; never a key, an ID, a label, a URL or the server's close
//!   reason.

mod csp;
mod d2m;
mod drop_own;
mod keepalive;
pub(crate) mod skew;
mod state;

use std::cell::Cell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{Clock, Envelope};
use kourier_db::ZoneDb;
use kourier_model::{AccountUuid, BackendError, ConnState, ErrorKind};
use libthreema::csp::payload::IncomingPayload as CspIncoming;
use libthreema::csp::{CspProtocol, CspStateUpdate};
use libthreema::d2m::payload::{
    IncomingPayload as D2mIncoming, OutgoingPayload as D2mOutgoing, Reflect, ReflectFlags,
    Reflected, ReflectedAck,
};
use libthreema::d2m::{D2mProtocol, D2mProtocolError, D2mStateUpdate};
use libthreema::protobuf::d2m::DeviceSlotState;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{JoinHandle, spawn_local};
use tokio_util::sync::CancellationToken;

use crate::close::{Backoff, CloseError};
use crate::error::StoreError;
use crate::reflect;
use crate::send::attempt;
use crate::send::machine::{Do, Machine, ReflectOf, Zone};
use crate::send::queue::{Runnable, SealedUpdate, SendOrigin, SendReceiver, SendTask};
use crate::send::shared::{COOL_FIRST, JOB_GIVES_UP_AT, Shared, cooling_after};
use crate::store::{self, DeferredBounds, Identity, SlotState};
use crate::{NetError, ServerParams, Transports, WsEvent, WsKind, WsRequest, WsSender};

use csp::CspSend;
pub(crate) use drop_own::{Alone, Dropped, alone as drop_alone, deadline as drop_deadline};
use keepalive::{Due, Keepalive};
pub(crate) use state::publish;
use state::{Next, Outcome};

/// The connect (DNS, TCP, TLS, the upgrade) and the D2M handshake up to `ServerInfo` must finish
/// within this much of the actor's clock (Kourier's number; critic M10).
const HANDSHAKE_WITHIN: Duration = Duration::from_secs(20);
/// From `ServerInfo` on, the CSP login must finish within this much after the last frame heard
/// (TH3 fix P-L7: every frame of the server pushes the deadline out).
const LOGIN_WITHIN: Duration = Duration::from_secs(20);
/// How long a stopping connection waits for its close frame to be written.
const CLOSE_WITHIN: Duration = Duration::from_secs(1);
/// Frames of the running send row may stand unanswered this long after the last of them written or the last ack of it
/// heard (decision C11; Kourier's number, WhatsApp's is 75 s).
const ACK_WITHIN: Duration = Duration::from_secs(60);
/// How long one frame may take to write (decision C14; Kourier's number).
const SEND_WITHIN: Duration = Duration::from_secs(10);
/// The rule name of the WARN line of a send row that fails by its content: it cools.
const RULE_ZONE_FAILED: &str = "send-zone-failed";
/// The rule name of the WARN line of message-acks no row waits for.
const RULE_ACK_UNKNOWN: &str = "send-ack-unknown";
/// The rule name of the WARN line of reflect-acks no row waits for.
const RULE_REFLECT_ACK_UNKNOWN: &str = "send-reflect-ack-unknown";

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// `Offline` without a retry (the actor's `Disconnect`).
pub(crate) fn publish_offline(tx: &watch::Sender<ConnState>) {
    publish(tx, ConnState::Offline { retry_at_ms: None });
}

/// What a connection is made with (the actor's, for the actor's lifetime).
pub(crate) struct Setup {
    /// The parts' transports, made once per actor.
    pub(crate) transports: Transports,
    /// The servers.
    pub(crate) server: ServerParams,
    /// The stored identity (keys in `Zeroizing`).
    pub(crate) identity: Rc<Identity>,
    /// What the slot is believed to be, kept by the ACTOR across its connection tasks (P-M1).
    pub(crate) slots: Slots,
    /// The actor's clock.
    pub(crate) clock: Arc<dyn Clock>,
    /// The account's state channel to the core.
    pub(crate) state: watch::Sender<ConnState>,
    /// The account's `backend.db` (the slot state is written at `ServerInfo`).
    pub(crate) db: Rc<ZoneDb>,
    /// The account (its "Connect Anyway" flag, [`skew`]).
    pub(crate) account: AccountUuid,
    /// TH4: set when this task is the JOIN's connection (module doc): the first `ServerInfo` (or the
    /// first socket's end before one) is reported here.
    pub(crate) join: Option<JoinHook>,
    /// Whether the file is join-ready (TH4's `join_ready` marker): `true` for an account that started
    /// linked; a join's actor sets it right after zone 2 commits. Until then reflected envelopes wait
    /// in memory (step 0, decision B1).
    pub(crate) ready: watch::Receiver<bool>,
    /// Wakes the actor's forwarder after a reflected batch with events committed (`core_outbox`, TH5 A6).
    pub(crate) wake: Rc<Notify>,
    /// The forwarder's last settled `core_outbox` seq (the actor's `acked` watch, `outbox::forward`):
    /// what the catch-up boundary waits for (TH5 A5b).
    pub(crate) acked: watch::Receiver<i64>,
    /// The start's age bound of `deferred` is still due (round B, S9 + S11; the actor's, `true` at its
    /// start): taken by the first `ServerInfo` whose clock check passed, which runs it once.
    pub(crate) expire_due: Rc<Cell<bool>>,
    /// The actor's events channel to the core (a clone of its sender): the EPHEMERAL typing of R12 goes here
    /// with `try_send`, never through `core_outbox` (TH5b B3).
    pub(crate) events: mpsc::Sender<Envelope>,
    /// What the actor and the socket share of the send queue (TH6a).
    pub(crate) send: Rc<Shared>,
}

/// Where the join's first `ServerInfo` is reported (TH4 J5a).
pub(crate) type JoinHook = oneshot::Sender<SlotReport>;

/// What the join's first socket said of the device's slot.
#[derive(Debug)]
pub(crate) enum SlotReport {
    /// The first `ServerInfo` said `NEW`: the mediator registered the device just now.
    New,
    /// The first `ServerInfo` said something else (EXISTING, or no state): the spec rules it out for
    /// an expected NEW (`TP/md-d2m.proto`: a mismatch is a 4115 before `ServerInfo`); a belt.
    NotNew,
    /// The first socket ended before any `ServerInfo`: the error of the join's `Failed` step.
    Failed(BackendError),
}

/// The device slot's state as the actor knows it, shared by every connection task of ONE actor
/// (TH3 fix P-M1: a second task, after a `Disconnect` or a task that panicked, expects what the first
/// one learnt, so a reconnect never costs a 4115 round trip and the UPDATE is not repeated).
#[derive(Clone)]
pub(crate) struct Slots {
    /// What the slot is believed to be: the stored state, `existing` after any `ServerInfo` (kept in
    /// memory even when the write fails, critic L11).
    slot: Rc<Cell<SlotState>>,
    /// Whether the FILE still says `new` (the L13 retry sets `existing` in memory before its
    /// `ServerInfo`, so the write cannot hang on the memory's state).
    file_new: Rc<Cell<bool>>,
}

impl Slots {
    /// The slot state of `identity`, as the file has it.
    pub(crate) fn new(identity: &Identity) -> Self {
        Self {
            slot: Rc::new(Cell::new(identity.slot_state)),
            file_new: Rc::new(Cell::new(identity.slot_state == SlotState::New)),
        }
    }

    /// Whether the FILE still says `new`: no `ServerInfo` has been seen since the identity was written (RP-b, P-M2: a join's
    /// hello may still be in flight at the mediator, so a first 4115 of the drop's socket is not final).
    pub(crate) fn file_says_new(&self) -> bool {
        self.file_new.get()
    }
}

/// What the actor tells a running connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Nudge {
    /// `Command::Connect`: connect now when waiting, try once when held.
    Connect,
    /// `Command::NetworkChanged`.
    NetworkChanged,
    /// `Command::Resumed`.
    Resumed,
}

/// How a connection task ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// The actor stopped it.
    Stopped,
    /// The link is over (`LoggedOut` was published): the actor ends.
    LoggedOut {
        /// Log-safe reason.
        reason: &'static str,
    },
    /// The join's first socket did not register the device (reported on the hook first).
    JoinRefused,
    /// RP-b: the mediator dropped the own device at the actor's ask ([`Task::drop_own`]).
    Dropped,
}

/// The running connection, as the actor holds it.
pub(crate) struct Task {
    nudges: mpsc::UnboundedSender<Nudge>,
    stop: CancellationToken,
    /// RP-b: the actor asks for the own device's drop ([`Task::drop_own`]).
    drop_ask: CancellationToken,
    handle: JoinHandle<End>,
}

impl Task {
    /// Starts the connection on the actor's `LocalSet`: it connects at once.
    pub(crate) fn start(setup: Setup) -> Self {
        let (nudges, rx) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let drop_ask = CancellationToken::new();
        let handle = spawn_local(run(setup, rx, stop.clone(), drop_ask.clone()));
        Self {
            nudges,
            stop,
            drop_ask,
            handle,
        }
    }

    /// Hands a nudge to the connection (a connection that ended takes none).
    pub(crate) fn nudge(&self, nudge: Nudge) {
        let _ = self.nudges.send(nudge);
    }

    /// Stops the connection (an open socket is closed with 1000) and waits for its end.
    pub(crate) async fn stop(self) -> Result<End, Panicked> {
        self.stop.cancel();
        joined(self.handle.await)
    }

    /// RP-b (module doc): asks the connection to drop the own device on its socket and waits for its end until
    /// [`drop_own::wait_end`] of `by_ms` on `clock` (the closing second stays inside `by_ms`) or the actor's `cancel`.
    /// `Some`: the answer (`Removed`: the mediator dropped it; `NotReached`: the time ran out or the actor was cancelled,
    /// the connection stopped); `None`: no socket past `ServerInfo` could ask, the connection is over and the caller
    /// drops the device on a socket of its own.
    pub(crate) async fn drop_own(
        mut self,
        clock: &dyn Clock,
        by_ms: u64,
        cancel: &CancellationToken,
    ) -> Option<Dropped> {
        self.drop_ask.cancel();
        let ended = tokio::select! {
            biased;
            end = &mut self.handle => Some(joined(end)),
            () = cancel.cancelled() => None,
            () = clock.sleep_until(drop_own::wait_end(by_ms)) => {
                tracing::info!("the mediator did not answer the drop in time");
                None
            }
        };
        match ended {
            Some(Ok(End::Dropped)) => Some(Dropped::Removed),
            Some(Ok(End::Stopped | End::JoinRefused | End::LoggedOut { .. }) | Err(Panicked)) => {
                None
            }
            None => {
                self.stop.cancel();
                let _ = joined(self.handle.await);
                Some(Dropped::NotReached)
            }
        }
    }

    /// Waits until the connection ends by itself (a logout verdict, or a panic of the task).
    pub(crate) async fn ended(task: &mut Option<Self>) -> Result<End, Panicked> {
        match task {
            Some(t) => {
                let end = joined((&mut t.handle).await);
                *task = None;
                end
            }
            None => std::future::pending().await,
        }
    }
}

/// The connection task panicked (TH3 fix P-L8): the actor's failure, so the core's restart policy
/// hears of it; never a silent stop that leaves the last state standing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Panicked;

/// The task's result. The task is never aborted, so a `JoinError` is its panic; logged once, with
/// no content (the panic's message stays with the panic hook).
fn joined(result: Result<End, tokio::task::JoinError>) -> Result<End, Panicked> {
    result.map_err(|_| {
        tracing::error!("the connection task panicked");
        Panicked
    })
}

/// The connection's loop: one socket, the verdict, the wait or the hold, until the actor stops it
/// or the link ends.
async fn run(
    mut setup: Setup,
    mut nudges: mpsc::UnboundedReceiver<Nudge>,
    stop: CancellationToken,
    drop_ask: CancellationToken,
) -> End {
    let mut backoff = Backoff::default();
    // The ACTOR's slot state (P-M1), not this task's.
    let (slot, file_new) = (setup.slots.slot.clone(), setup.slots.file_new.clone());
    // TH4: the join's hook, taken by the first `ServerInfo`.
    let mut hook = setup.join.take();
    // S10: what a socket that ended over step 0's bound waits for.
    let mut file_ready = setup.ready.clone();
    loop {
        publish(&setup.state, ConnState::Connecting);
        let expected = slot.get();
        let joining = hook.is_some();
        let halt = Halt {
            stop: &stop,
            drop_ask: &drop_ask,
        };
        let mut outcome = one_socket(
            &setup,
            &slot,
            &file_new,
            &mut hook,
            &mut nudges,
            halt,
            &mut backoff,
        )
        .await;
        // Critic M1: never on the join's first socket (its 4115 is the join's answer). RP-b: never after a drop's ask.
        if !joining && !drop_ask.is_cancelled() && crash_window(expected, slot.get(), &outcome) {
            // Critic L13: the file said `new`, but the slot exists (a kill between a `ServerInfo`
            // and its write): ONE more connect at once with `existing`, no backoff step, no count.
            // A second 4115 is the verdict.
            tracing::info!("the slot exists although the file says new: one more connect");
            slot.set(SlotState::Existing);
            outcome = one_socket(
                &setup,
                &slot,
                &file_new,
                &mut hook,
                &mut nudges,
                halt,
                &mut backoff,
            )
            .await;
        }
        setup.send.set_live(false);
        // RP-b: the mediator dropped the device at the actor's ask.
        if outcome == Outcome::Dropped {
            publish(&setup.state, ConnState::Offline { retry_at_ms: None });
            return End::Dropped;
        }
        if let Some(hook) = hook.take() {
            // The join's first socket ended before any `ServerInfo` (M1): the join fails at once; no
            // retry, no backoff. A stop of the actor reports nothing (the hook is dropped).
            publish(&setup.state, ConnState::Offline { retry_at_ms: None });
            if outcome == Outcome::Stopped {
                return End::Stopped;
            }
            tracing::info!("the joined device was not registered by the mediator");
            let _ = hook.send(SlotReport::Failed(join_failure(&outcome)));
            return End::JoinRefused;
        }
        if outcome == Outcome::JoinRefused {
            publish(&setup.state, ConnState::Offline { retry_at_ms: None });
            return End::JoinRefused;
        }
        // RP-b: after a drop's ask a socket that ended otherwise is not followed by another; the actor drops the
        // device on a socket of its own (no verdict of the table: the account is logging out).
        if drop_ask.is_cancelled() {
            publish(&setup.state, ConnState::Offline { retry_at_ms: None });
            return End::Stopped;
        }
        let held_full = outcome == Outcome::HeldFull;
        match state::next(&outcome, &mut backoff) {
            Next::Stopped => {
                publish(&setup.state, ConnState::Offline { retry_at_ms: None });
                return End::Stopped;
            }
            Next::LoggedOut { reason, cause } => {
                publish(
                    &setup.state,
                    ConnState::LoggedOut {
                        reason: reason.to_owned(),
                        cause,
                        note: Default::default(),
                    },
                );
                return End::LoggedOut { reason };
            }
            Next::After(delay) => {
                let at = setup.clock.now_ms().saturating_add(ms(delay));
                publish(
                    &setup.state,
                    ConnState::Offline {
                        retry_at_ms: Some(at),
                    },
                );
                // Any nudge connects at once (the count stays).
                let stopped = tokio::select! {
                    biased;
                    () = stop.cancelled() => true,
                    // RP-b: no socket to drop on; the actor opens its own.
                    () = drop_ask.cancelled() => true,
                    () = setup.clock.sleep_until(at) => false,
                    nudge = nudges.recv() => nudge.is_none(),
                    // S10: the hold overflowed because the file was not ready: connect when it is
                    // (at once when it already is), else after the one backoff step above.
                    () = ready_file(&mut file_ready), if held_full => false,
                };
                if stopped {
                    publish(&setup.state, ConnState::Offline { retry_at_ms: None });
                    return End::Stopped;
                }
            }
            Next::Hold { action, hints } => {
                publish(&setup.state, ConnState::NeedsUserAction(action));
                loop {
                    let nudge = tokio::select! {
                        biased;
                        () = stop.cancelled() => None,
                        () = drop_ask.cancelled() => None,
                        nudge = nudges.recv() => nudge,
                    };
                    match nudge {
                        None => {
                            publish(&setup.state, ConnState::Offline { retry_at_ms: None });
                            return End::Stopped;
                        }
                        // The user's own word starts a NEW series of attempts (P-L6: "user
                        // interaction should be required to continue reconnecting"); a hint of
                        // the system makes ONE attempt, and a failure holds again at once.
                        Some(Nudge::Connect) => {
                            backoff.worked();
                            break;
                        }
                        Some(Nudge::NetworkChanged | Nudge::Resumed) if hints => break,
                        Some(Nudge::NetworkChanged | Nudge::Resumed) => {}
                    }
                }
            }
        }
    }
}

/// The actor's two words to a connection: stop, and (RP-b) drop the own device.
#[derive(Clone, Copy)]
struct Halt<'a> {
    stop: &'a CancellationToken,
    drop_ask: &'a CancellationToken,
}

/// S10: resolves when the file is join-ready; never when the actor (the sender) is gone, so the backoff
/// step decides then.
async fn ready_file(ready: &mut watch::Receiver<bool>) {
    if ready.wait_for(|ready| *ready).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Whether `outcome` is the crash window of critic L13: the connect expected a NEW slot, the
/// mediator said 4115 before any `ServerInfo` (the slot in memory is still `New`).
fn crash_window(expected: SlotState, now: SlotState, outcome: &Outcome) -> bool {
    expected == SlotState::New
        && now == SlotState::New
        && matches!(outcome, Outcome::Closed { code: 4115, .. })
}

/// The `Failed` step of a join whose first socket ended before any `ServerInfo` (brief §2.9): 4111
/// `TooManyDevices`, 4110 (the protocol version) and a server Kourier cannot use `Unsupported`, another
/// 41xx `Unauthorized`, anything else (a timeout, a refused connect, 4000, 1006) `Transient`.
fn join_failure(outcome: &Outcome) -> BackendError {
    match outcome {
        Outcome::Closed { code: 4111, .. } => BackendError::new(
            ErrorKind::TooManyDevices { max: None },
            "the device group has no free slot",
        ),
        // 4110: the mediator does not take Kourier's protocol version (P7; TH3 holds `UpdateApp` for it).
        Outcome::Closed { code: 4110, .. }
        | Outcome::NotReached(NetError::Untrusted(_))
        | Outcome::Protocol {
            unsupported: true, ..
        } => BackendError::unsupported("the mediator is not one Kourier can use"),
        Outcome::Closed {
            code: 4100..=4199, ..
        } => BackendError::new(ErrorKind::Unauthorized, "the mediator refused the device"),
        _ => BackendError::new(
            ErrorKind::Transient { retry_after: None },
            "the mediator did not register the device",
        ),
    }
}

/// One socket from the connect to its end. `join`: the join's hook while no `ServerInfo` came. `backoff` hears when the end
/// was a send row's and not the connection's ([`Session::socket_ended`]).
async fn one_socket(
    setup: &Setup,
    slot: &Rc<Cell<SlotState>>,
    file_new: &Rc<Cell<bool>>,
    join: &mut Option<JoinHook>,
    nudges: &mut mpsc::UnboundedReceiver<Nudge>,
    halt: Halt<'_>,
    backoff: &mut Backoff,
) -> Outcome {
    let stop = halt.stop;
    let clock = &*setup.clock;
    let handshake_by = clock.now_ms().saturating_add(ms(HANDSHAKE_WITHIN));
    let expected = slot.get();
    let contexts = d2m::context(&setup.identity, &setup.server, expected)
        .and_then(|d| csp::context(&setup.identity, &setup.server).map(|c| (d, c)))
        .and_then(|(d, c)| reflect::Keys::of(&setup.identity).map(|k| (d, c, k)));
    let (d2m_context, csp_context, keys) = match contexts {
        Ok(contexts) => contexts,
        Err(field) => {
            // The stored identity or the parts do not convert: nothing can connect (a bug, or a
            // file from a newer Kourier). Held until the user acts.
            tracing::error!(field, "the connection could not be set up");
            return Outcome::Protocol {
                unsupported: true,
                up_for: Duration::ZERO,
            };
        }
    };
    let (d2m, url) = D2mProtocol::new(d2m_context);
    let (csp, hello) = CspProtocol::new(csp_context);
    let connect = setup.transports.ws.connect(WsRequest {
        url,
        kind: WsKind::Mediator,
    });
    let (tx, rx) = tokio::select! {
        biased;
        () = stop.cancelled() => return Outcome::Stopped,
        // RP-b: no socket yet to drop on; the actor opens its own.
        () = halt.drop_ask.cancelled() => return Outcome::Stopped,
        () = clock.sleep_until(handshake_by) => {
            tracing::info!("the mediator was not reached in time");
            return Outcome::TimedOut;
        }
        opened = connect => match opened {
            Ok(halves) => halves,
            Err(error) => {
                tracing::info!(%error, "the mediator was not reached");
                return Outcome::NotReached(error);
            }
        },
    };
    let mut s = Session {
        clock: setup.clock.clone(),
        state: setup.state.clone(),
        d2m,
        csp,
        tx,
        broken: false,
        server_info_at: None,
        logged_in: false,
        dry_seen: false,
        boundary: None,
        acked: setup.acked.clone(),
        leader: false,
        close_error: None,
        alive: Keepalive::opened(clock.now_ms()),
        deadline_ms: handshake_by,
        end: None,
        slot: slot.clone(),
        file_new: file_new.clone(),
        db: setup.db.clone(),
        account: setup.account,
        keys,
        ready: setup.ready.clone(),
        wake: setup.wake.clone(),
        held: Vec::new(),
        held_bytes: 0,
        closing: false,
        clock_proved: false,
        expire_due: setup.expire_due.clone(),
        events: setup.events.clone(),
        shared: setup.send.clone(),
        machine: None,
        next_reflect_id: 1,
        ack_by: None,
        queue_off: false,
        stop: stop.clone(),
        own_device: setup.identity.d2m_device_id,
        dropping: false,
    };
    // The CSP client-hello goes out at once in a Proxy (allowed before `ServerInfo`; Desktop does).
    s.proxy(hello.0).await;
    let outcome = drive(&mut s, &rx, setup, join, nudges, halt).await;
    s.socket_ended(&outcome, backoff);
    outcome
}

/// The open socket's loop, up to its end.
async fn drive(
    s: &mut Session,
    rx: &async_channel::Receiver<WsEvent>,
    setup: &Setup,
    join: &mut Option<JoinHook>,
    nudges: &mut mpsc::UnboundedReceiver<Nudge>,
    halt: Halt<'_>,
) -> Outcome {
    let (stop, drop_ask) = (halt.stop, halt.drop_ask);
    let clock = &*setup.clock;
    let mut ready = setup.ready.clone();
    // The catch-up boundary (A5b): the forwarder's settled seq; `false` once the forwarder is gone (a
    // core that never acks keeps `Syncing`, the honest state).
    let mut acked = setup.acked.clone();
    acked.borrow_and_update();
    let mut forwarder = true;
    loop {
        s.step(join).await;
        if let Some(outcome) = s.end.take() {
            return outcome;
        }
        let wake = if s.logged_in {
            s.wake_at()
        } else {
            s.deadline_ms
        };
        tokio::select! {
            biased;
            () = stop.cancelled() => {
                s.close(1000).await;
                return Outcome::Stopped;
            }
            // RP-b: the actor asks for the own device's drop (once per socket).
            () = drop_ask.cancelled(), if !s.dropping => s.drop_asked().await,
            event = rx.recv() => match event {
                Ok(WsEvent::Binary(datagram)) => {
                    s.alive.heard(s.now());
                    s.login_progress();
                    // H1: what else is queued, up to a batch, goes into the same pass; a close found
                    // here is handled after the pass (its batch applied first, L-2: and not acked, the
                    // server's close came before: no data frame after it).
                    let (datagrams, then) = drain(rx, datagram);
                    s.datagrams(datagrams).await;
                    if let Some(code) = then {
                        s.closing = true;
                        s.step(join).await;
                        if let Some(outcome) = s.end.take() {
                            return outcome;
                        }
                        return s.closed(code);
                    }
                }
                Ok(WsEvent::Closed { code, reason: _ }) => return s.closed(code),
                // The socket ended without a close frame.
                Err(_) => return s.closed(1006),
            },
            // Step 0 (B1): zone 2 committed: the next pass applies what waited, then counts a Dry
            // that came meanwhile (A5b).
            is = ready.wait_for(|ready| *ready), if !s.held.is_empty() || s.dry_seen => {
                if is.is_err() {
                    // The actor is gone: nothing that waits is acked (the mediator delivers it again),
                    // and no Dry is counted.
                    s.held.clear();
                    s.held_bytes = 0;
                    s.dry_seen = false;
                }
            }
            // The catch-up boundary (A5b): the core settled more rows; `Online` once it reached it.
            changed = acked.changed(), if forwarder && s.awaits_core() => {
                if changed.is_err() {
                    forwarder = false;
                } else {
                    s.publish();
                }
            }
            nudge = nudges.recv() => match nudge {
                None => {
                    s.close(1000).await;
                    return Outcome::Stopped;
                }
                Some(nudge) => s.hint(nudge).await,
            },
            // TH6a: a row may run: the next pass looks (`Session::pump`). The timer below wakes it for a row that thawed.
            () = setup.send.kicked() => {}
            () = clock.sleep_until(wake) => s.timer().await,
        }
    }
}

/// The first datagram of a pass and what else the socket has queued, up to [`reflect::BATCH`] in all
/// (decision H1: no wait); the close code of an end found while draining (1006: no close frame).
fn drain(rx: &async_channel::Receiver<WsEvent>, first: Vec<u8>) -> (Vec<Vec<u8>>, Option<u16>) {
    let mut datagrams = vec![first];
    while datagrams.len() < reflect::BATCH {
        match rx.try_recv() {
            Ok(WsEvent::Binary(datagram)) => datagrams.push(datagram),
            Ok(WsEvent::Closed { code, reason: _ }) => return (datagrams, Some(code)),
            Err(async_channel::TryRecvError::Empty) => break,
            Err(async_channel::TryRecvError::Closed) => return (datagrams, Some(1006)),
        }
    }
    (datagrams, None)
}

/// What one pass of [`Session::step`] collected from the mediator.
#[derive(Default)]
struct Pass {
    /// Proxy bytes (the CSP leg).
    proxied: Vec<Vec<u8>>,
    /// The non-ephemeral reflected envelopes, in arrival order.
    reflected: Vec<Reflected>,
    /// `ReflectionQueueDry` came (applied after the batch's zone, H1).
    dry: bool,
    /// The reflect-acks: the id and the time the mediator stored the reflect (ms), in arrival order.
    reflect_acks: Vec<(u32, u64)>,
    /// The chat server's message-acks: the receiver's ID and the message id, in arrival order.
    message_acks: Vec<(String, u64)>,
    /// RP-b: the mediator acked the drop of the own device.
    dropped: bool,
}

/// One open socket: both protocols, what they said, the keep-alive.
struct Session {
    clock: Arc<dyn Clock>,
    state: watch::Sender<ConnState>,
    d2m: D2mProtocol,
    csp: CspProtocol,
    tx: Box<dyn WsSender>,
    /// A send failed: the socket is going; its close event says how. Or a write was given up ([`Session::send`]): no frame
    /// goes after it, and [`Session::end`] says how.
    broken: bool,
    /// When `ServerInfo` came (the socket's uptime counts from it).
    server_info_at: Option<u64>,
    /// The CSP login-ack came.
    logged_in: bool,
    /// The mediator's `ReflectionQueueDry` came and is not counted yet: it waits for step 0's hold
    /// (the file join-ready and nothing held), A5b.
    dry_seen: bool,
    /// THE CATCH-UP BOUNDARY of this socket (A5b): `outbox::max_seq` when Dry was counted; `None`
    /// before. `Online` once the forwarder's `acked` reached it.
    boundary: Option<i64>,
    /// The forwarder's last settled seq ([`Setup::acked`]).
    acked: watch::Receiver<i64>,
    /// The mediator promoted this device to leader.
    leader: bool,
    /// The chat server's `close-error`, kept for the verdict (it decides alone).
    close_error: Option<CloseError>,
    alive: Keepalive,
    /// When the handshake times out (P-L7): [`HANDSHAKE_WITHIN`] after the connect started; from
    /// `ServerInfo` on [`LOGIN_WITHIN`] after the last frame heard. Not used once logged in.
    deadline_ms: u64,
    /// Set when the client ends the socket itself.
    end: Option<Outcome>,
    /// What the next connect expects of the slot.
    slot: Rc<Cell<SlotState>>,
    /// The file still says `new`.
    file_new: Rc<Cell<bool>>,
    db: Rc<ZoneDb>,
    account: AccountUuid,
    /// DGK and the own ID: what opens and hashes the reflected envelopes.
    keys: reflect::Keys,
    /// Whether the file is join-ready ([`Setup::ready`]).
    ready: watch::Receiver<bool>,
    /// Wakes the forwarder ([`Setup::wake`]).
    wake: Rc<Notify>,
    /// Step 0 (B1): the reflected envelopes that wait for zone 2, in arrival order, and their bytes.
    held: Vec<Reflected>,
    held_bytes: usize,
    /// The drain found the server's close among the datagrams (L-2): the pass in hand is applied, but
    /// no ack goes out after it (RFC 6455 5.5.1: no data after the close handshake started).
    closing: bool,
    /// The mediator's `ServerInfo` proved the local clock (within the skew bound; not "Connect Anyway"):
    /// only then do this socket's batch zones apply `deferred`'s age bound (round B, S9).
    clock_proved: bool,
    /// [`Setup::expire_due`].
    expire_due: Rc<Cell<bool>>,
    /// [`Setup::events`].
    events: mpsc::Sender<Envelope>,
    /// [`Setup::send`].
    shared: Rc<Shared>,
    /// The row that runs on this socket; none of it survives the socket but what its zones wrote.
    machine: Option<Machine>,
    /// The next `Reflect`'s id: unique per connection, so it starts over with every socket.
    next_reflect_id: u32,
    /// When the frames of the running row must have been answered ([`ACK_WITHIN`]); `None`: none stand unanswered.
    ack_by: Option<u64>,
    /// The queue could not be read by its content: this socket sends nothing more.
    queue_off: bool,
    /// The actor's stop: a stalled write does not hold it up.
    stop: CancellationToken,
    /// The own device's D2M id (RP-b: the one `DropDevice` names).
    own_device: u64,
    /// RP-b: the actor asked for the own device's drop on this socket.
    dropping: bool,
}

impl Session {
    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// A frame of the server came: during the CSP login (after `ServerInfo`, before the login-ack)
    /// that is progress, and the login's deadline moves to [`LOGIN_WITHIN`] from now (P-L7).
    fn login_progress(&mut self) {
        if self.server_info_at.is_some() && !self.logged_in {
            self.deadline_ms = self.now().saturating_add(ms(LOGIN_WITHIN));
        }
    }

    fn up_for(&self) -> Duration {
        self.server_info_at.map_or(Duration::ZERO, |at| {
            Duration::from_millis(self.now().saturating_sub(at))
        })
    }

    /// One frame, bounded by [`SEND_WITHIN`] of the actor's clock (C14, L5) and by the actor's stop. The send comes first
    /// in the `biased` select, so a write that is ready registers no sleep. Past the bound the socket is thrown away at
    /// once: closed 1000, `Outcome::Dead`. Dropping the pending send is safe for what it leaves behind: `LiveSender` holds
    /// its sink's lock only inside that future, a frame may stay half in the sink's buffer, and the sink goes with this
    /// session (its read pump aborted), so nothing of a torn frame can be read back, and a row writes its stored bytes
    /// again on a new socket.
    async fn send(&mut self, datagram: Vec<u8>) {
        if self.broken {
            return;
        }
        tokio::select! {
            biased;
            sent = self.tx.send(datagram) => {
                if sent.is_err() {
                    self.broken = true;
                }
            }
            () = self.stop.cancelled() => self.write_given_up(Outcome::Stopped).await,
            () = self.clock.sleep(SEND_WITHIN) => {
                tracing::info!("a write stalled");
                self.write_given_up(Outcome::Dead { up_for: self.up_for() }).await;
            }
        }
    }

    /// A write will not end: no frame goes after it, the socket closes 1000 and ends as `outcome`.
    async fn write_given_up(&mut self, outcome: Outcome) {
        self.broken = true;
        self.close(1000).await;
        if self.end.is_none() {
            self.end = Some(outcome);
        }
    }

    /// CSP bytes in one D2M Proxy payload.
    async fn proxy(&mut self, bytes: Vec<u8>) {
        match self.d2m.create_payload(D2mOutgoing::Proxy(bytes)) {
            Ok(instruction) => {
                if let Some(datagram) = instruction.outgoing_datagram {
                    self.send(datagram.0).await;
                }
            }
            Err(error) => self.d2m_failed(&error).await,
        }
    }

    async fn csp_send(&mut self, payload: CspSend) {
        match self.csp.create_payload(&payload.payload()) {
            Ok(instruction) => {
                if let Some(frame) = instruction.outgoing_frame {
                    self.proxy(frame.0).await;
                }
            }
            Err(_) => self.protocol_error(false, "csp encode").await,
        }
    }

    /// The client closes the socket (bounded by [`CLOSE_WITHIN`] of the actor's clock).
    async fn close(&mut self, code: u16) {
        let close = self.tx.close(code, String::new());
        let clock = self.clock.clone();
        tokio::select! {
            _ = close => {}
            () = clock.sleep(CLOSE_WITHIN) => {}
        }
    }

    /// M9: the server's protocol version is one Kourier does not speak (4110), or it sent what
    /// libthreema refuses (4010). The error's operation is logged, never its text.
    async fn protocol_error(&mut self, unsupported: bool, what: &'static str) {
        if self.end.is_some() {
            return;
        }
        tracing::warn!(what, unsupported, "the server's message was refused");
        self.close(if unsupported { 4110 } else { 4010 }).await;
        self.end = Some(Outcome::Protocol {
            unsupported,
            up_for: self.up_for(),
        });
    }

    async fn d2m_failed(&mut self, error: &D2mProtocolError) {
        let unsupported = matches!(error, D2mProtocolError::UnsupportedVersion(_));
        self.protocol_error(unsupported, "d2m").await;
    }

    /// The binary frames of one pass from the mediator.
    async fn datagrams(&mut self, datagrams: Vec<Vec<u8>>) {
        if let Err(error) = self.d2m.add_datagrams(datagrams) {
            self.d2m_failed(&error).await;
        }
    }

    /// The server closed the socket with `code`. RP-b: after the drop's ask a 4113 or 4114 is the drop (the device is
    /// gone from the mediator).
    fn closed(&self, code: u16) -> Outcome {
        // The code and the spec's words for it; never the server's close reason.
        let meaning = crate::close::sentence(code).unwrap_or("a code the spec does not name");
        tracing::info!(code, meaning, "the mediator closed the connection");
        if self.dropping && matches!(code, 4113 | 4114) {
            return Outcome::Dropped;
        }
        Outcome::Closed {
            code,
            close_error: self.close_error,
            up_for: self.up_for(),
        }
    }

    /// What the socket's timer waits for once logged in: the keep-alive, an awaited ack, or the next row that thaws.
    fn wake_at(&self) -> u64 {
        [
            self.alive.deadline(),
            self.ack_by,
            self.shared.next_thaw(self.now()),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(u64::MAX)
    }

    /// The timer of the actor's clock fired: the handshake's deadline, an awaited ack, or the keep-alive. A row that thawed
    /// needs nothing here: the next pass looks.
    async fn timer(&mut self) {
        let now = self.now();
        if !self.logged_in {
            tracing::info!(
                login = self.server_info_at.is_some(),
                "the handshake did not finish in time"
            );
            self.close(1000).await;
            self.end = Some(Outcome::TimedOut);
            return;
        }
        if self.ack_by.is_some_and(|by| by <= now) {
            tracing::info!("an awaited ack of the send queue did not come");
            self.close(1000).await;
            self.end = Some(Outcome::NoAck {
                up_for: self.up_for(),
            });
            return;
        }
        match self.alive.due(now) {
            Due::Nothing => {}
            Due::Echo => {
                self.csp_send(CspSend::EchoRequest(now.to_be_bytes().to_vec()))
                    .await;
            }
            Due::Dead => {
                tracing::info!("an echo stayed unanswered");
                self.close(1000).await;
                self.end = Some(Outcome::Dead {
                    up_for: self.up_for(),
                });
            }
        }
    }

    /// A nudge while the socket is open. Before the login the handshake's deadline decides; a
    /// `Connect` kicks the send queue (it is connected); `NetworkChanged` probes; `Resumed` probes, or
    /// replaces a socket the server was silent on for over five minutes (brief §2.2).
    async fn hint(&mut self, nudge: Nudge) {
        if !self.logged_in || self.end.is_some() {
            return;
        }
        let now = self.now();
        match nudge {
            // Connected already; it is the queue that takes another look.
            Nudge::Connect => self.shared.kick(),
            Nudge::Resumed if self.alive.stale(now) => {
                tracing::info!("the socket was silent too long: replaced");
                self.close(1000).await;
                self.end = Some(Outcome::Stale);
            }
            Nudge::NetworkChanged | Nudge::Resumed => {
                self.alive.probe(now);
                self.csp_send(CspSend::EchoRequest(now.to_be_bytes().to_vec()))
                    .await;
            }
        }
    }

    /// RP-b: the actor asks for the own device's drop. Past `ServerInfo` the socket writes `DropDevice` of the own id
    /// ([`drop_own::frame`]) and waits for its answer; before it (or on a socket that is going) it closes with 1000 and the
    /// actor drops the device on a socket of its own.
    async fn drop_asked(&mut self) {
        self.dropping = true;
        if self.server_info_at.is_none() || self.end.is_some() || self.closing || self.broken {
            tracing::info!("no socket to drop the own device on: closed");
            self.close(1000).await;
            if self.end.is_none() {
                self.end = Some(Outcome::Stopped);
            }
            return;
        }
        let Some(datagram) = drop_own::frame(&mut self.d2m, self.own_device) else {
            self.close(1000).await;
            self.end = Some(Outcome::Stopped);
            return;
        };
        self.send(datagram).await;
        // S-5: a write that failed or stalled marks the socket broken or ends it; only a written frame is logged as one.
        if self.broken || self.end.is_some() {
            tracing::info!("the own device's drop could not be written to the live socket");
        } else {
            tracing::info!("the own device's drop went on the live socket");
        }
    }

    /// Lets both protocols work on what arrived until neither has anything left. `join`: the join's
    /// hook, taken at the first `ServerInfo`.
    async fn step(&mut self, join: &mut Option<JoinHook>) {
        loop {
            let mut moved = false;
            let mut pass = Pass::default();
            while self.end.is_none() {
                match self.d2m.poll() {
                    Ok(Some(instruction)) => {
                        if let Some(D2mStateUpdate::PostHandshake(info)) = instruction.state_update
                        {
                            // The slot state FIRST (brief §2.4: before the clock rule closes,
                            // before `Syncing`), then the clock.
                            self.server_info_at = Some(self.now());
                            // The CSP login's own deadline starts here (P-L7).
                            self.deadline_ms = self.now().saturating_add(ms(LOGIN_WITHIN));
                            // TH4: the join's first `ServerInfo` is reported BEFORE the slot is
                            // marked existing (brief §2.6); not `NEW` ends the socket.
                            if let Some(hook) = join.take() {
                                self.join_report(hook, info.device_slot_state).await;
                                if self.end.is_some() {
                                    break;
                                }
                            }
                            self.slot_exists();
                            // The clock rule at a join's first `ServerInfo` (critic L8): the device
                            // IS registered now (reported `NEW` above), so a skew hold does not stop
                            // the join: the actor writes zone 2, waits until the core applied the
                            // first row, sends `Registered` and ends with `Done` after every row
                            // (`actor.rs`, THE WHOLE JOIN), and the account then holds for the clock
                            // (`FixClock`) as any TH3 connection.
                            self.clock_proved =
                                self.check_clock(info.server_time_ms, "mediator").await;
                            if self.end.is_some() {
                                break;
                            }
                            // Round B, S9 + S11: the start's age bound of `deferred`, once per actor, at
                            // the first `ServerInfo` that proved the clock (never under "Connect Anyway").
                            if self.clock_proved && self.expire_due.replace(false) {
                                let now = i64::try_from(self.now()).unwrap_or(i64::MAX);
                                reflect::remap::expire_at_a_proved_clock(&self.db, now);
                            }
                            // C15: the queue's sweep at every socket's first `ServerInfo` that proved the clock.
                            if self.clock_proved {
                                self.sweep_queue();
                            }
                        }
                        if let Some(payload) = instruction.incoming_payload {
                            self.d2m_payload(payload, &mut pass);
                        }
                        if let Some(datagram) = instruction.outgoing_datagram {
                            self.send(datagram.0).await;
                            moved = true;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => self.d2m_failed(&error).await,
                }
            }
            // RP-b: the mediator dropped the own device: the socket is over (the server's 4113 may follow).
            if pass.dropped && self.end.is_none() {
                tracing::info!("the mediator acked the own device's drop");
                if !self.closing {
                    self.close(1000).await;
                }
                self.end = Some(Outcome::Dropped);
            }
            // L1: the backlog is why an ack may be late: a reflect-ack comes behind what the mediator queued before it.
            let backlog = !pass.reflected.is_empty() && self.boundary.is_none();
            // THE REFLECTION LOOP (module doc): the batch's zone, then its acks, then Dry.
            if !pass.reflected.is_empty() || (!self.held.is_empty() && *self.ready.borrow()) {
                moved |= self.reflected(pass.reflected).await;
            }
            if backlog && self.ack_by.is_some() {
                self.ack_heard();
            }
            self.dry_seen |= pass.dry;
            // A5b: Dry is counted once nothing waits in step 0 (the file join-ready, the hold
            // released into the zones above).
            if self.dry_seen && self.held.is_empty() && *self.ready.borrow() && self.end.is_none() {
                self.dry_seen = false;
                self.count_dry().await;
            }
            for bytes in &pass.proxied {
                if self.end.is_none() && self.csp.add_chunks(&[bytes]).is_err() {
                    self.protocol_error(false, "csp chunks").await;
                }
            }
            let mut frames = Vec::new();
            let mut answers = Vec::new();
            while self.end.is_none() {
                match self.csp.poll() {
                    Ok(Some(instruction)) => {
                        if let Some(CspStateUpdate::PostHandshake(ack)) = instruction.state_update {
                            // The clock again, at the chat server's login-ack.
                            let _ = self.check_clock(ack.server_time_ms, "chat server").await;
                            if self.end.is_some() {
                                break;
                            }
                            self.logged_in = true;
                            self.shared.set_live(true);
                            self.alive.start(self.now());
                            answers.push(CspSend::SetConnectionIdleTimeout(keepalive::IDLE_S));
                            self.publish();
                        }
                        frames.extend(instruction.outgoing_frame.map(|f| f.0));
                        if let Some(payload) = instruction.incoming_payload {
                            answers.extend(self.csp_payload(payload, &mut pass.message_acks));
                        }
                    }
                    Ok(None) => break,
                    Err(_) => self.protocol_error(false, "csp").await,
                }
            }
            for frame in frames {
                self.proxy(frame).await;
                moved = true;
            }
            for answer in answers {
                self.csp_send(answer).await;
                moved = true;
            }
            moved |= self.pump(pass.reflect_acks, pass.message_acks).await;
            if !moved || self.end.is_some() || self.broken {
                return;
            }
        }
    }

    /// The queue's sweep (C15). A failure is one WARN: the next socket sweeps again.
    fn sweep_queue(&self) {
        let now = i64::try_from(self.now()).unwrap_or(i64::MAX);
        if store::sweep_sends(&self.db, now).is_err() {
            tracing::warn!("the send queue's sweep failed; it runs again at the next socket");
        }
    }

    /// The join's first `ServerInfo` said `state`: reported on `hook`; anything but `NEW` closes with
    /// 1000 (the spec: "close the D2M connection (normally)", `TP/md-d2d-join.proto` step 6).
    async fn join_report(&mut self, hook: JoinHook, state: Option<DeviceSlotState>) {
        if state == Some(DeviceSlotState::New) {
            let _ = hook.send(SlotReport::New);
            return;
        }
        tracing::warn!("the mediator did not register the joined device as new");
        let _ = hook.send(SlotReport::NotNew);
        self.close(1000).await;
        self.end = Some(Outcome::JoinRefused);
    }

    /// `ServerInfo` came: the slot exists. Written while the file says `new`; a failed write is a
    /// warning and the slot is `existing` in memory anyway (critic L11; the next `ServerInfo`
    /// writes again, and a new start heals it through the one retry of L13).
    fn slot_exists(&self) {
        self.slot.set(SlotState::Existing);
        if !self.file_new.get() {
            return;
        }
        match store::set_slot_existing(&self.db) {
            Ok(()) => self.file_new.set(false),
            Err(_) => tracing::warn!("the slot state could not be saved"),
        }
    }

    /// The clock rule ([`skew`]): more than 20 minutes from `server_ms` closes with 1000 and holds
    /// for the user, unless the account's "Connect Anyway" is up (then it is logged only). Whether the
    /// clock is PROVED: within the bound (round B, S9: `false` under "Connect Anyway" too).
    async fn check_clock(&mut self, server_ms: u64, server: &'static str) -> bool {
        let skew_ms = skew::skew_ms(self.now(), server_ms);
        if !skew::too_far(skew_ms) {
            return true;
        }
        let minutes = skew_ms / 60_000;
        if skew::anyway(self.account) {
            tracing::info!(server, minutes, "the clock is off; connected anyway");
            return false;
        }
        tracing::warn!(server, minutes, "the clock is off by more than 20 minutes");
        self.close(1000).await;
        self.end = Some(Outcome::ClockSkew { skew_ms });
        false
    }

    /// The reflected envelopes of one pass (module doc): held while the file is not join-ready
    /// (step 0), else applied with what waited, in zones of at most [`reflect::BATCH`], each zone's
    /// acks sent after its commit. Whether anything was sent.
    async fn reflected(&mut self, batch: Vec<Reflected>) -> bool {
        if !*self.ready.borrow() {
            self.hold(batch).await;
            return false;
        }
        let mut rest = std::mem::take(&mut self.held);
        self.held_bytes = 0;
        // A5b: a message is a catch-up one until Dry is COUNTED (the contract's "arrival <= boundary, or
        // boundary not yet known"): every row of these zones is below the boundary taken after them. A
        // Dry is counted only after what waited in step 0 is applied, so what waited is catch-up too,
        // Dry or not (the release is one pass, and Dry in a pass is counted after its zone, brief
        // §2.7 item 9).
        let catch_up = self.boundary.is_none();
        rest.extend(batch);
        let mut moved = false;
        while !rest.is_empty() {
            let tail = rest.split_off(rest.len().min(reflect::BATCH));
            let chunk = std::mem::replace(&mut rest, tail);
            let applied = {
                let ctx = reflect::Ctx {
                    db: &self.db,
                    now_ms: i64::try_from(self.now()).unwrap_or(i64::MAX),
                    // S9: the age bound only once the mediator proved the clock.
                    bounds: if self.clock_proved {
                        DeferredBounds::REAL
                    } else {
                        DeferredBounds::REAL.without_age()
                    },
                    own: self.keys.own(),
                    wake: &self.wake,
                    catch_up,
                };
                reflect::apply(&ctx, reflect::Batch::open(&self.keys, chunk))
            };
            // L13: once the socket's end is set, the batch is applied but nothing is acked; L-2: the
            // same when the server's close was found with the batch.
            if self.end.is_none() && !self.closing {
                for reflect_id in applied.acks {
                    self.ack(reflect_id).await;
                    moved = true;
                    // K3 of the kill loops (`tests/connection/kill.rs`): after some acks, before the rest.
                    kourier_db::failpoint::hit(crate::failpoints::AFTER_REFLECTED_ACK);
                }
            }
            if applied.io_failed {
                // H3: the store fails; the rest is not acked, the mediator delivers it again.
                if self.end.is_none() {
                    self.close(1000).await;
                    self.end = Some(Outcome::Store {
                        up_for: self.up_for(),
                    });
                }
                return moved;
            }
        }
        moved
    }

    /// Step 0 (B1): `batch` waits for zone 2; over 30 envelopes or 2 MiB the socket closes 1000 (the
    /// mediator delivers them again on the next socket).
    async fn hold(&mut self, batch: Vec<Reflected>) {
        for reflected in batch {
            self.held_bytes = self.held_bytes.saturating_add(reflected.envelope.len());
            self.held.push(reflected);
        }
        if self.held.len() <= reflect::BATCH && self.held_bytes <= reflect::HOLD_BYTES {
            return;
        }
        self.held.clear();
        self.held_bytes = 0;
        if self.end.is_some() {
            return;
        }
        tracing::info!(
            "more reflections came before the link was ready than can wait; reconnecting"
        );
        self.close(1000).await;
        self.end = Some(Outcome::HeldFull);
    }

    /// Counts `ReflectionQueueDry` (A5b): the boundary is the highest `core_outbox` seq now (every row
    /// written before Dry, zone 2 of a join and what waited in step 0 included), and the state is
    /// published (`Online` at once when the forwarder already settled it, e.g. no rows at all). A
    /// failed read of the file ends the socket like a failed zone (H3's I/O branch: a new socket takes
    /// a new boundary).
    async fn count_dry(&mut self) {
        match crate::outbox::max_seq(&self.db) {
            Ok(seq) => {
                self.boundary = Some(seq);
                self.publish();
            }
            Err(_) => {
                tracing::warn!("the catch-up boundary could not be read; reconnecting");
                self.close(1000).await;
                self.end = Some(Outcome::Store {
                    up_for: self.up_for(),
                });
            }
        }
    }

    /// Whether the account waits for the core's ack of the boundary (`Syncing` with Dry counted).
    fn awaits_core(&self) -> bool {
        self.logged_in && self.boundary.is_some_and(|b| *self.acked.borrow() < b)
    }

    /// One `reflected-ack`.
    async fn ack(&mut self, reflect_id: u32) {
        let ack = D2mOutgoing::ReflectedAck(ReflectedAck { reflect_id });
        match self.d2m.create_payload(ack) {
            Ok(instruction) => {
                if let Some(datagram) = instruction.outgoing_datagram {
                    self.send(datagram.0).await;
                }
            }
            Err(error) => self.d2m_failed(&error).await,
        }
    }

    /// One D2M payload, collected into the pass.
    fn d2m_payload(&mut self, payload: D2mIncoming, pass: &mut Pass) {
        match payload {
            D2mIncoming::Proxy(bytes) => pass.proxied.push(bytes),
            D2mIncoming::ReflectionQueueDry => pass.dry = true,
            D2mIncoming::RolePromotedToLeader => {
                // Observed and reported, nothing else (brief §2.9): no request for messages.
                self.leader = true;
                self.publish();
            }
            // C5, M5: an EPHEMERAL one (its flag) has no zone, no nonce and no ack (the spec forbids
            // the ack). R12: an incoming typing goes to the core at once, lossy (`try_send`: the step
            // never waits for the core; a full or closed channel drops it).
            D2mIncoming::Reflected(r) if reflect::is_ephemeral(&r) => {
                if let Some(typing) = reflect::open_ephemeral(&self.keys, &r)
                    && self.events.try_send(Envelope::ephemeral(typing)).is_err()
                {
                    tracing::debug!(
                        "a typing indicator was dropped: the core's channel is full or closed"
                    );
                }
            }
            D2mIncoming::Reflected(r) => pass.reflected.push(r),
            D2mIncoming::ReflectAck(ack) => pass.reflect_acks.push((ack.reflect_id, ack.timestamp)),
            // RP-b: the ack of the own device's drop, after the actor's ask; any other is left alone.
            D2mIncoming::DropDeviceAck(device) if self.dropping && device.0 == self.own_device => {
                pass.dropped = true;
            }
            other => tracing::debug!(
                payload = other.variant_name(),
                "a mediator payload was not asked for"
            ),
        }
    }

    /// One CSP payload; what to answer. A message-ack is collected for the pass's [`Session::pump`].
    fn csp_payload(
        &mut self,
        payload: CspIncoming,
        message_acks: &mut Vec<(String, u64)>,
    ) -> Option<CspSend> {
        match payload {
            CspIncoming::EchoRequest(echo) => Some(CspSend::EchoResponse(echo.0)),
            CspIncoming::EchoResponse(echo) => {
                self.alive.answered();
                let sent = echo
                    .0
                    .as_slice()
                    .try_into()
                    .map(u64::from_be_bytes)
                    .unwrap_or_default();
                tracing::debug!(rtt_ms = self.now().saturating_sub(sent), "echo answered");
                None
            }
            CspIncoming::CloseError(error) => {
                // Its message is the server's text: never logged.
                self.close_error = Some(CloseError {
                    can_reconnect: error.can_reconnect,
                });
                None
            }
            CspIncoming::MessageAck(ack) => {
                message_acks.push((ack.sender_identity.as_str().to_owned(), ack.message_id.0));
                None
            }
            // [U: real server] it cannot come without a request for messages; if it does it is
            // NOT acked (TH6 owns the leader's receive).
            other => {
                tracing::debug!(
                    payload = other.variant_name(),
                    "a chat server payload was left alone"
                );
                None
            }
        }
    }

    /// THE SEND QUEUE'S TURN at the end of a pass (module doc): what the servers said of the running row, then, while no
    /// row runs, the next one. Whether a frame went. The zones run whatever the socket is about to do; the frames do not
    /// ([`Session::may_write`], L4).
    async fn pump(
        &mut self,
        reflect_acks: Vec<(u32, u64)>,
        message_acks: Vec<(String, u64)>,
    ) -> bool {
        let mut moved = self.heard(reflect_acks, message_acks).await;
        while self.machine.is_none() && self.logged_in && !self.queue_off && self.may_write() {
            let Some((task, receivers)) = self.next_row().await else {
                break;
            };
            let (id, origin) = (task.id, task.origin);
            let only = self.shared.only_of(id);
            let Some((machine, first)) = Machine::start(task, receivers, only.as_deref()) else {
                self.row_failed(id, origin);
                continue;
            };
            self.machine = Some(machine);
            moved |= self.perform(first).await;
        }
        // No row can be named on this socket any more, so nothing attached would ever be answered.
        if self.queue_off {
            attempt::all_gone(&self.db, &self.shared);
        }
        if !self.machine.as_ref().is_some_and(Machine::unanswered) {
            self.ack_by = None;
        }
        moved
    }

    /// A frame of the running row went, or an ack of it came: [`ACK_WITHIN`] starts again.
    fn ack_heard(&mut self) {
        self.ack_by = Some(self.now().saturating_add(ms(ACK_WITHIN)));
    }

    /// The reflect-acks and message-acks of one pass, in that order: a row's reflect-ack and its boxes' acks are never
    /// in one pass (the boxes go after the zone of the reflect-ack).
    async fn heard(
        &mut self,
        reflect_acks: Vec<(u32, u64)>,
        message_acks: Vec<(String, u64)>,
    ) -> bool {
        let mut moved = false;
        let mut unknown = 0_usize;
        for (id, at_ms) in reflect_acks {
            match self.machine.as_mut().and_then(|m| m.reflect_ack(id, at_ms)) {
                Some(zone) => {
                    self.ack_heard();
                    moved |= self.perform(vec![Do::Zone(zone)]).await;
                }
                None => unknown += 1,
            }
        }
        if unknown > 0 {
            tracing::warn!("reflect-acks ignored: {unknown} by {RULE_REFLECT_ACK_UNKNOWN}");
        }
        if message_acks.is_empty() {
            return moved;
        }
        let total = message_acks.len();
        let (zone, unknown) = match self.machine.as_mut() {
            Some(machine) => machine.message_acks(message_acks),
            None => (None, total),
        };
        if unknown > 0 {
            tracing::warn!("message-acks ignored: {unknown} by {RULE_ACK_UNKNOWN}");
        }
        if let Some(zone) = zone {
            self.ack_heard();
            moved |= self.perform(vec![Do::Zone(zone)]).await;
        }
        moved
    }

    /// The next row that may run and its receivers. A read the store fails ends the socket; one that fails by its content
    /// stops the queue on this socket, with no way to tell which row it was.
    async fn next_row(&mut self) -> Option<(SendTask, Vec<SendReceiver>)> {
        let attached = self.shared.attached();
        let cooling = self.shared.cooling(self.now());
        let can = Runnable {
            attached: &attached,
            cooling: &cooling,
            online: self.caught_up(),
        };
        let row = self.db.read(|conn: &kourier_db::rusqlite::Connection| {
            let Some(task) = store::next_send(conn, &can)? else {
                return Ok::<_, StoreError>(None);
            };
            let receivers = store::send_receivers(conn, task.id)?;
            Ok(Some((task, receivers)))
        });
        match row {
            Ok(row) => row,
            Err(error) if error.is_content() => {
                tracing::warn!("the send queue stops on this socket: 1 by {RULE_ZONE_FAILED}");
                self.queue_off = true;
                None
            }
            Err(_) => {
                self.store_failed().await;
                None
            }
        }
    }

    /// Does what the running row's machine said, in order; a zone's end brings its next steps. Whether a frame went.
    async fn perform(&mut self, first: Vec<Do>) -> bool {
        let mut moved = false;
        let mut todo = VecDeque::from(first);
        while let Some(one) = todo.pop_front() {
            match one {
                Do::Reflect { envelope, of } => {
                    if self.may_write() {
                        self.write_reflect(envelope, of).await;
                        self.ack_heard();
                        moved = true;
                    }
                }
                Do::Box(bytes) => match CspSend::boxed(bytes) {
                    Some(frame) if self.may_write() => {
                        self.csp_send(frame).await;
                        self.ack_heard();
                        moved = true;
                        if !self.broken {
                            kourier_db::failpoint::hit(crate::failpoints::AFTER_SEND_BOX);
                        }
                    }
                    Some(_) => {
                        if let Some(machine) = self.machine.as_mut() {
                            machine.box_not_written();
                        }
                    }
                    None => {
                        self.running_failed();
                        todo.clear();
                    }
                },
                Do::Zone(zone) => match self.run_zone(zone) {
                    Ok(next) => todo.extend(next),
                    Err(error) => {
                        self.zone_failed(&error).await;
                        todo.clear();
                    }
                },
                Do::Finished => {
                    if let Some(machine) = self.machine.take() {
                        attempt::row_finished(&self.db, &self.shared, machine.task().id);
                    }
                }
                Do::Cool => self.running_failed(),
            }
        }
        moved
    }

    /// Whether a frame of the send queue may go on this socket (L4; L-2: no data frame after the server's close; RP-b:
    /// none after the own device's drop went).
    fn may_write(&self) -> bool {
        self.end.is_none() && !self.closing && !self.broken && !self.dropping
    }

    /// One `Reflect` of the running row under a new id (flags 0: never ephemeral, the mediator owes an ack).
    async fn write_reflect(&mut self, envelope: Vec<u8>, of: ReflectOf) {
        let reflect_id = self.next_reflect_id;
        self.next_reflect_id = reflect_id.wrapping_add(1);
        if let Some(machine) = self.machine.as_mut() {
            machine.reflect_written(reflect_id);
        }
        let reflect = D2mOutgoing::Reflect(Reflect {
            flags: ReflectFlags(0),
            reflect_id,
            envelope,
        });
        match self.d2m.create_payload(reflect) {
            Ok(instruction) => {
                if let Some(datagram) = instruction.outgoing_datagram {
                    self.send(datagram.0).await;
                }
            }
            Err(error) => {
                self.d2m_failed(&error).await;
                return;
            }
        }
        if !self.broken {
            kourier_db::failpoint::hit(match of {
                ReflectOf::Message => crate::failpoints::AFTER_SEND_REFLECT,
                ReflectOf::Update => crate::failpoints::AFTER_SEND_UPDATE,
            });
        }
    }

    /// One zone of the running row: it commits, then the machine says what its end allows.
    fn run_zone(&mut self, zone: Zone) -> Result<Vec<Do>, StoreError> {
        let Some(machine) = self.machine.as_mut() else {
            return Ok(Vec::new());
        };
        let task = machine.task().id;
        let now = i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX);
        match zone {
            Zone::Reflected => {
                store::mark_send_reflected(&self.db, task)?;
                Ok(machine.reflected())
            }
            Zone::Acks { acked, seal } => {
                let sealed = if seal {
                    let row = machine.task();
                    Some(self.db.read(|conn: &kourier_db::rusqlite::Connection| {
                        store::seal_sent_update(conn, row)
                    })?)
                } else {
                    None
                };
                let update = sealed.as_ref().map(|(envelope, nonce)| SealedUpdate {
                    envelope,
                    nonce: *nonce,
                });
                let names: Vec<&str> = acked.iter().map(String::as_str).collect();
                let end = store::mark_send_acks(&self.db, task, &names, update, now)?;
                Ok(machine.acks_end(end, sealed.map(|(envelope, _)| envelope)))
            }
            Zone::Done { sent_at } => {
                store::mark_send_done(&self.db, task, sent_at, now)?;
                Ok(machine.done())
            }
        }
    }

    /// A send zone failed (decision R2): by the store's fault the socket ends, the row stands for the next one; by the
    /// content's the row cools and later rows go.
    async fn zone_failed(&mut self, error: &StoreError) {
        if error.is_content() {
            self.running_failed();
            return;
        }
        self.machine = None;
        self.store_failed().await;
    }

    /// The store failed under the queue (H3's I/O branch).
    async fn store_failed(&mut self) {
        tracing::warn!("the send queue's store failed; reconnecting");
        if self.end.is_none() {
            self.close(1000).await;
            self.end = Some(Outcome::Store {
                up_for: self.up_for(),
            });
        }
    }

    /// The running row cannot go on by its content.
    fn running_failed(&mut self) {
        if let Some(machine) = self.machine.take() {
            let task = machine.task();
            self.row_failed(task.id, task.origin);
        }
    }

    /// Row `task` cannot go on by its content (decision R2): no close, and it cools at once, even the first time: on this
    /// socket it would run again in the same pass, and fail the same way.
    fn row_failed(&self, task: i64, origin: SendOrigin) {
        tracing::warn!("send rows cooling: 1 by {RULE_ZONE_FAILED}");
        self.failed_run(task, origin, Some(COOL_FIRST));
        attempt::row_stopped(&self.db, &self.shared, task);
    }

    /// The socket is over (decision R2): when `outcome` says something about the row and frames of it stand unanswered,
    /// that was a failed run of the row. The machine goes with the socket either way. The close is then the row's and the
    /// connection worked, so `backoff` starts its count again: else a queue of rows the server refuses, two closes each,
    /// would add up to the account's hold by the sixth. [`Backoff`]'s doc warns of the loop this could be, a payload refused
    /// at every start; the row's cooling is what ends it, as the next socket does not run it. EVERY attempt is answered here
    /// from what the rows say by now: nothing stays attached past a socket.
    fn socket_ended(&mut self, outcome: &Outcome, backoff: &mut Backoff) {
        if let Some(machine) = self.machine.take()
            && machine.unanswered()
            && outcome.is_failed_run()
        {
            let task = machine.task();
            self.failed_run(task.id, task.origin, None);
            backoff.worked();
        }
        attempt::all_gone(&self.db, &self.shared);
    }

    /// One more failed run of row `task`: durable, then a job's give-up or the row's cooling, at least `floor`. A count the
    /// store refuses is one WARN and the row cools by `floor`: the next zone tells whether the store is gone.
    fn failed_run(&self, task: i64, origin: SendOrigin, floor: Option<Duration>) {
        let runs = store::count_failed_run(&self.db, task).unwrap_or_else(|_| {
            tracing::warn!("a failed run of a send row could not be counted");
            0
        });
        tracing::info!(runs, "a run of a send row failed");
        if origin == SendOrigin::Job
            && runs >= JOB_GIVES_UP_AT
            && store::give_up_send_job(&self.db, task)
                .inspect_err(|_| tracing::warn!("a send job could not be given up; it cools"))
                .is_ok()
        {
            return;
        }
        if let Some(cooling) = cooling_after(runs).or(floor) {
            self.shared
                .cool(task, self.now().saturating_add(ms(cooling)));
        }
    }

    /// Whether the socket caught up: Dry is counted and the forwarder settled every row up to the boundary (A5b).
    fn caught_up(&self) -> bool {
        self.boundary.is_some_and(|b| *self.acked.borrow() >= b)
    }

    /// The state the core sees for this socket (nothing before the CSP login): `Online` once it
    /// caught up, else `Syncing`.
    fn publish(&self) {
        if !self.logged_in {
            return;
        }
        let new = if self.caught_up() {
            ConnState::Online {
                detail: Some(if self.leader { "leader" } else { "follower" }.to_owned()),
            }
        } else {
            ConnState::Syncing { fraction: None }
        };
        publish(&self.state, new);
    }
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt as _;

    use super::*;

    /// S10: `ready_file` resolves at once when the file is ready, as soon as it becomes ready, and never
    /// when the actor (the sender) is gone: the backoff step decides then.
    #[test]
    fn ready_file_waits_for_the_flag_and_never_ends_without_its_sender() {
        let (tx, mut ready) = watch::channel(false);
        assert!(
            ready_file(&mut ready).now_or_never().is_none(),
            "resolved before the file was ready"
        );
        tx.send_replace(true);
        assert!(
            ready_file(&mut ready).now_or_never().is_some(),
            "did not resolve for a ready file"
        );
        let (tx, mut ready) = watch::channel(false);
        drop(tx);
        assert!(
            ready_file(&mut ready).now_or_never().is_none(),
            "resolved although the actor is gone"
        );
        let (tx, mut ready) = watch::channel(true);
        drop(tx);
        assert!(
            ready_file(&mut ready).now_or_never().is_some(),
            "a ready file whose actor ended did not resolve"
        );
    }

    /// Step 0's 2 MiB hold bound (storage review: its mutant survives) cannot be tripped by any input today:
    /// at most [`reflect::BATCH`] envelopes are held (the count closes the socket first) and a mediator
    /// message is at most 65,536 bytes, so the held bytes stay under 1.875 MiB. The bound is a belt; when
    /// this fails (a bigger batch or datagram) it can be reached and needs its own test like the count.
    #[test]
    fn the_hold_byte_bound_is_above_what_the_count_bound_lets_through() {
        const MEDIATOR_MESSAGE_MAX: usize = 65_536;
        const {
            assert!(reflect::BATCH * MEDIATOR_MESSAGE_MAX < reflect::HOLD_BYTES);
        }
    }
}
