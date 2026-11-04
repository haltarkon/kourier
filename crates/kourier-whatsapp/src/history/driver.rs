//! The driver (W4 C16): the account's task that delivers the stage to the core.
//!
//! It runs on the actor's runtime beside stage 2 and the forwarder, and does nothing but wait until there is
//! something to deliver:
//!
//! 1. START. It begins when the stage file is on disk (a chunk was captured, now or in an earlier run) and the
//!    stored device is known (the stage is named by the account's own id, decided once: [`decided_owner`]). The stage is opened
//!    on a blocking thread, the feeder (its own thread for reading chunks) and the pump are made, and the
//!    pump counts the start of the application ([`Pump::start`]). An account that never receives a history
//!    never opens a stage and never wakes: the driver sleeps on its wake until a capture makes the file.
//! 2. STEPS. [`Pump::step`] in a loop: a row appended wakes the forwarder and the next step follows at once
//!    (after a yield, so the actor's loop and stage 2 get their turns); rows that wait for the core wait for
//!    the forwarder's acknowledgement, at most as long as the pump says, then step again (the idle time is
//!    counted by looks); nothing to do waits for a new chunk. While a chat has its end owed (`more = 1`) or an
//!    announced chunk is unresolved a look is taken every [`Timing::poll`], because nothing wakes the driver when
//!    stage 2 records a notice, or the time passes, that makes the end decidable (the poll stops when neither
//!    holds: a push whose last chunk is lost or bad ends by [`super::progress::Facts::account_over`], W4 fix F6).
//! 3. THE LINK HOLD. While [`crate::hold`] is closed (a login has no account yet) nothing is appended: the
//!    driver waits for the actor, which wakes it when the hold opens. The core refuses a row that leads a
//!    link, so `AccountReady` must have the lowest `seq`.
//! 4. THE END. [`Driver::stop`] ends the loop (a step in flight finishes) and drops the pump on a blocking
//!    thread: the feeder's thread is joined there, at most one chunk read later.
//!
//! A failed step (the store or the stage did not answer, or the pump gave a delivery up as out of place) is
//! logged by kind, never with an id, and tried again after a pause that doubles up to five minutes and starts over
//! at the first step that works.
//!
//! A stage the open replaced (a foreign owner, a key that did not open it), a file that vanished and was made again,
//! and a stage a capture made over books that are not empty all leave books that point into another stage. Every file
//! has an incarnation id and the books record the one they point into: [`retention::adopt`] starts them over, in the
//! zone that writes the new id, before anything is delivered from the new stage, whoever opened it and however the run
//! that did ended (W4 fix HIGH-1).
//!
//! THE TAKE-UP ([`deliver::take_up`]): once the driver has begun, before its first step, the messages an EARLIER
//! build's map deferred (`history_deferred`, a lower `MAP_VERSION`) that this build can show are delivered, one
//! silent row of a chat at a time, never more than the delivery's window of them unanswered. A failure waits for
//! the next start (they are kept, not lost). A build with the same map finds nothing and the pass ends at once.
//!
//! THE CALL-LOG TAKE-UP ([`deliver::take_up_calls`], W6a1 D13): the call-log records the push kept (the kind of
//! `history_deferred` that holds them) become quiet call lines in the chats of their calls ONCE THE PUSH IS OVER: when a
//! step finds nothing to deliver (`Step::Quiet`) and no chat owes its end and no announced chunk is still to come (the
//! same look that keeps the 30 s poll alive). A chunk that arrives later is taken up the same way at the next quiet
//! step; each record once (it is marked, not deleted). A failure waits for the next quiet step or start.
//!
//! THE ADDRESS-BOOK TAKE-UP ([`contacts::take_up`], W6a2 B8b): the names the push kept (`InlineContact`s, kind 3) go into the
//! address-book mirror at the same quiet step, each once (the kept entry is deleted in the zone that takes it); the names of
//! the people the core knows are stated, the rest wait in the mirror.
//!
//! HOUSEKEEPING ([`retention`]): a chunk whose rows all settled is retired (its bytes dropped) after each wait for the
//! core's answers and when the delivery is quiet; a `bad` chunk is forgotten after 14 days, at the start and when a
//! run of rows ends.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::Clock;
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use wacore::store::device::Device;
use wacore::store::traits::DeviceStore as _;
use wacore_binary::Jid;

use super::capture::decided_owner;
use super::contacts;
use super::deliver::{self, Own};
use super::feeder::Feeder;
use super::live;
use super::lost;
use super::pump::{Pump, Step};
use super::retention;
use super::settle::Limits;
use super::stage::{LazyStage, Origin, StageHandle};
use crate::map::Ctx;
use crate::outbox;
use crate::stage2::Stage2;
use crate::store::KourierStore;

/// The driver's waits.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// The delivery's bounds ([`Limits::DEFAULT`] outside the tests).
    pub(crate) limits: Limits,
    /// How often a look is taken while the end of a chat is owed and nothing else wakes the driver.
    pub(crate) poll: Duration,
    /// The first and the longest pause after a failed step or start.
    pub(crate) pause: Duration,
    pub(crate) pause_max: Duration,
}

impl Timing {
    pub(crate) const DEFAULT: Self = Self {
        limits: Limits::DEFAULT,
        poll: Duration::from_secs(30),
        pause: Duration::from_secs(5),
        pause_max: Duration::from_secs(300),
    };
}

impl Default for Timing {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What the driver is made of.
pub(crate) struct Parts {
    pub(crate) store: KourierStore,
    pub(crate) stage: Arc<LazyStage>,
    /// Where our own ids are known from (the map needs them).
    pub(crate) stage2: Arc<Stage2>,
    pub(crate) clock: Arc<dyn Clock>,
    /// Notified by a NEW captured chunk and by the actor when the link hold opens.
    pub(crate) wake: Arc<Notify>,
    /// Wakes the forwarder when a row was appended.
    pub(crate) outbox_wake: Arc<Notify>,
    /// Wakes the merge sweep: the push brought people the core knows now, and the store may hold their pairs (W6a2, B9).
    pub(crate) merges: Arc<Notify>,
    /// The forwarder's acknowledgements: a change may settle a waiting row.
    pub(crate) acked: watch::Receiver<i64>,
    pub(crate) timing: Timing,
}

/// The running driver of one account.
pub(crate) struct Driver {
    task: JoinHandle<()>,
    stop: CancellationToken,
}

impl Driver {
    /// Starts the task on the current runtime.
    pub(crate) fn spawn(parts: Parts) -> Self {
        let stop = CancellationToken::new();
        let task = tokio::spawn(run(parts, stop.clone()));
        Self { task, stop }
    }

    /// Ends the task (a step in flight finishes first) and waits until its feeder's thread is joined.
    pub(crate) async fn stop(&mut self) {
        self.stop.cancel();
        let _ = (&mut self.task).await;
    }
}

async fn run(parts: Parts, stop: CancellationToken) {
    let mut waits = Waits {
        wake: parts.wake.clone(),
        acked: parts.acked.clone(),
        acked_open: true,
        stop: stop.clone(),
        clock: parts.clock.clone(),
    };
    let Some(Started {
        mut pump,
        stage,
        own,
    }) = start(&parts, &mut waits).await
    else {
        return;
    };
    steps(&parts, &mut waits, &mut pump, (&stage, own)).await;
    // Dropping the pump joins the feeder's thread: not on the runtime's thread.
    let _ = tokio::task::spawn_blocking(move || drop(pump)).await;
}

/// The pump and the stage it reads, once the driver has begun.
struct Started {
    pump: Pump<Feeder>,
    stage: StageHandle,
    /// Our own ids, for the secrets of the messages of a take-up.
    own: Own,
}

/// What a start found.
enum Begun {
    Started(Box<Started>),
    /// Nothing to start from yet (no device): wait.
    Waits,
    /// A read, an open or the reset of a replaced stage failed: pause, then again.
    Failed,
}

/// Waits until the driver can begin: the hold open, the stage file on disk, the device stored. `None`: stopped,
/// or parked.
async fn start(parts: &Parts, waits: &mut Waits) -> Option<Started> {
    let mut pause = Pause::new(parts.timing);
    loop {
        if waits.stopped() {
            return None;
        }
        if !parts.store.hold().is_closed() && parts.stage.on_disk() {
            match begin(parts).await {
                Begun::Started(started) => return Some(*started),
                Begun::Waits => {
                    if !waits.wait(Some(parts.timing.poll), false).await {
                        return None;
                    }
                }
                Begun::Failed => {
                    if !waits.sleep(pause.next()).await {
                        return None;
                    }
                }
            }
            continue;
        }
        if !waits.wait(None, false).await {
            return None;
        }
    }
}

async fn begin(parts: &Parts) -> Begun {
    let device = match parts.store.load().await {
        Ok(Some(device)) => device,
        Ok(None) => return Begun::Waits,
        Err(_) => return Begun::Failed,
    };
    let owner = match decided_owner(&parts.store).await {
        Ok(Some(owner)) => owner,
        Ok(None) => return Begun::Waits,
        Err(_) => return Begun::Failed,
    };
    let stage = Arc::clone(&parts.stage);
    let handle = match tokio::task::spawn_blocking(move || stage.handle(&owner)).await {
        Ok(Ok(handle)) => handle,
        _ => return Begun::Failed,
    };
    // The books point into THIS stage file or start over, in one zone, BEFORE anything is delivered from it (an id the
    // books hold changes nothing). The origin is taken only when that worked, so a failed adoption is made again.
    let Some(incarnation) = parts.stage.incarnation() else {
        return Begun::Failed;
    };
    let origin = parts.stage.origin();
    let known_lost = match origin {
        Some(Origin::Foreign { lost }) => Some(lost),
        _ => None,
    };
    match retention::adopt(&parts.store, incarnation, known_lost).await {
        Ok(retention::Adopted::Reset) => {
            let kind = match origin {
                Some(Origin::Foreign { .. }) => "foreign owner",
                Some(Origin::Unreadable) => "unreadable, set aside",
                _ => "the file was not the one the books describe",
            };
            tracing::warn!(kind, "the history stage was replaced; its books were reset");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, "the history books could not be pointed at the stage");
            return Begun::Failed;
        }
    }
    parts.stage.take_origin();
    let ctx = Ctx {
        own: parts.stage2.own().await,
    };
    let feeder = Feeder::new(
        handle.clone(),
        parts.store.clone(),
        ctx,
        Arc::clone(&parts.clock),
    );
    let own = own_of(&device);
    match Pump::start(
        parts.store.clone(),
        feeder,
        own.clone(),
        Arc::clone(&parts.clock),
        parts.timing.limits,
    )
    .await
    {
        Ok(pump) => Begun::Started(Box::new(Started {
            pump,
            stage: handle,
            own,
        })),
        Err(_) => Begun::Failed,
    }
}

/// Our own ids for the secrets of a message of ours: the users of the device's two addresses.
fn own_of(device: &Device) -> Own {
    // An address with no user (a pair-success without a `lid` stores one) names nobody: LOW-2.
    Own {
        pn: device
            .pn
            .as_ref()
            .filter(|jid| !jid.user.is_empty())
            .map(|jid| Jid::pn(jid.user.as_str())),
        lid: device
            .lid
            .as_ref()
            .filter(|jid| !jid.user.is_empty())
            .map(|jid| Jid::lid(jid.user.as_str())),
    }
}

/// The steps (module docs, 2 and 3).
async fn steps(
    parts: &Parts,
    waits: &mut Waits,
    pump: &mut Pump<Feeder>,
    (stage, own): (&StageHandle, Own),
) {
    let mut pause = Pause::new(parts.timing);
    // A start finds what an earlier run could not finish: chunks to retire (and the floor of the live notes, which
    // the forgetting below reads), bad ones that have aged out.
    retire(parts, stage).await;
    // Forgets the notes of live messages that are too old to matter: at the start, and when a run of rows ends.
    forget_old_live(parts).await;
    forget_bad(parts, stage).await;
    // What an earlier map could not show goes to the core before the stage's rows.
    if !take_up(parts, waits, own.clone(), Taking::Messages).await {
        return;
    }
    let mut busy = false;
    // A chunk that comes after its notice was counted lost takes the count back, right behind its capture (the wake
    // that brought the driver here): looked at again at every step while a notice still counted lost waits.
    let mut late_chunks = true;
    loop {
        if waits.stopped() {
            return;
        }
        if late_chunks {
            late_chunks = take_back_late(parts, stage).await;
        }
        if parts.store.hold().is_closed() {
            if !waits.wait(None, false).await {
                return;
            }
            continue;
        }
        let waited = match pump.step().await {
            Ok(Step::Progress { seq }) => {
                pause.reset();
                busy = true;
                if seq.is_some() {
                    parts.outbox_wake.notify_one();
                }
                // The actor's loop and stage 2 take their turn between two rows.
                tokio::task::yield_now().await;
                continue;
            }
            Ok(Step::Wait { left }) => {
                let waited = waits.wait(Some(left), true).await;
                // The core's answers settle rows: a chunk may have none left.
                if waited {
                    retire(parts, stage).await;
                }
                waited
            }
            Ok(Step::Quiet) => {
                retire(parts, stage).await;
                if std::mem::take(&mut busy) {
                    forget_old_live(parts).await;
                    forget_bad(parts, stage).await;
                }
                let again = looks_again(&parts.store).await;
                // The push is over (module docs): the call log it kept goes to the chats of its calls.
                if !again && !take_up(parts, waits, own.clone(), Taking::Calls).await {
                    return;
                }
                // And the address book it kept goes into the mirror, its names to the people the core knows (B8b).
                if !again && !take_up(parts, waits, own.clone(), Taking::Contacts).await {
                    return;
                }
                // The people it delivered are known to the core now: the pairs the store holds for them are due (B9).
                if !again {
                    parts.merges.notify_one();
                }
                waits.wait(again.then_some(parts.timing.poll), false).await
            }
            Err(e) => {
                // The error names the operation, never an input.
                tracing::error!(error = %e, "the history delivery failed a step; trying again");
                waits.sleep(pause.next()).await
            }
        };
        if !waited {
            return;
        }
    }
}

/// What a take-up takes up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Taking {
    /// The messages an earlier map deferred ([`deliver::take_up`]).
    Messages,
    /// The call-log records the push kept ([`deliver::take_up_calls`]).
    Calls,
    /// The address-book names the push kept ([`contacts::take_up`], W6a2 B8b).
    Contacts,
}

/// What one pass of a take-up did, for the window and the log.
struct Pass {
    seq: Option<i64>,
    /// Messages or lines handed to the core.
    delivered: u64,
    /// Messages the contract refused, records that gave no line.
    dropped: u64,
}

/// The take-up of what a pass finds due (module docs). `false`: stopped.
async fn take_up(parts: &Parts, waits: &mut Waits, own: Own, taking: Taking) -> bool {
    let ctx = Ctx {
        own: parts.stage2.own().await,
    };
    // Rows appended and not yet answered: the next waits for the oldest once the window is full.
    let mut unanswered: VecDeque<i64> = VecDeque::new();
    let (mut delivered, mut dropped) = (0u64, 0u64);
    loop {
        if waits.stopped() {
            return false;
        }
        while unanswered.len() >= parts.timing.limits.window.max(1) {
            let Some(&oldest) = unanswered.front() else {
                break;
            };
            if !waits.answered(&parts.store, oldest).await {
                return false;
            }
            unanswered.pop_front();
        }
        // After the wait for the core: the hold may have closed while the window was full.
        if parts.store.hold().is_closed() {
            if !waits.wait(None, false).await {
                return false;
            }
            continue;
        }
        let now = i64::try_from(parts.clock.now_ms()).unwrap_or(i64::MAX);
        let pass = match taking {
            Taking::Messages => deliver::take_up(&parts.store, own.clone(), ctx.clone(), now)
                .await
                .map(|done| {
                    done.map(|done| Pass {
                        seq: done.seq,
                        delivered: u64::from(done.delivered),
                        dropped: u64::from(done.dropped),
                    })
                }),
            Taking::Calls => deliver::take_up_calls(&parts.store, ctx.own.clone(), now)
                .await
                .map(|done| {
                    done.map(|done| Pass {
                        seq: done.seq,
                        delivered: done.counts.lines as u64,
                        dropped: (done.counts.silent + done.counts.unplaced + done.counts.refused)
                            as u64
                            + u64::from(done.unread),
                    })
                }),
            Taking::Contacts => contacts::take_up(&parts.store, now).await.map(|done| {
                done.map(|done| Pass {
                    seq: done.seq,
                    delivered: u64::from(done.taken),
                    dropped: u64::from(done.unread),
                })
            }),
        };
        match pass {
            Ok(None) => break,
            Ok(Some(done)) => {
                delivered += done.delivered;
                dropped += done.dropped;
                if let Some(seq) = done.seq {
                    unanswered.push_back(seq);
                    parts.outbox_wake.notify_one();
                }
                // The actor's loop and stage 2 take their turn between two rows.
                tokio::task::yield_now().await;
            }
            Err(e) => {
                // They are kept: the next start (or the next quiet step) tries again.
                tracing::error!(error = %e, "deferred history could not be taken up; trying again later");
                return true;
            }
        }
    }
    if delivered > 0 || dropped > 0 {
        match taking {
            Taking::Messages => tracing::info!(
                delivered,
                dropped,
                "history the map could not show earlier was taken up"
            ),
            Taking::Calls => tracing::info!(
                lines = delivered,
                without_line = dropped,
                "the call log the history push kept was taken up"
            ),
            Taking::Contacts => tracing::info!(
                contacts = delivered,
                unread = dropped,
                "the address book the history push kept was taken up"
            ),
        }
    }
    true
}

/// Takes back the lost count of the chunks that were captured after their notice was counted lost
/// ([`lost::take_back`]). `true`: look again at the next step (a notice counted lost still waits for its chunk, or the
/// look failed).
async fn take_back_late(parts: &Parts, stage: &StageHandle) -> bool {
    match lost::take_back(&parts.store, stage).await {
        Ok(taken) => {
            if taken.back > 0 {
                tracing::info!(
                    chunks = taken.back,
                    "history chunks counted lost arrived after all; the count was taken back"
                );
            }
            taken.waiting > 0
        }
        Err(e) => {
            tracing::warn!(error = %e, "the lost history count could not be corrected");
            true
        }
    }
}

/// Retires the chunks whose rows have all settled ([`retention::retire`]); a failure is a warning, the next look
/// does it again.
async fn retire(parts: &Parts, stage: &StageHandle) {
    if let Err(e) = retention::retire(&parts.store, stage).await {
        tracing::warn!(error = %e, "delivered history chunks could not be retired");
    }
    // The floor of the live notes follows what waits for its delivery (W4 fix F5).
    if let Err(e) = retention::set_live_floor(&parts.store, stage).await {
        tracing::warn!(error = %e, "the floor of the live notes could not be set");
    }
}

/// Forgets the `bad` chunks that have been kept for 14 days ([`retention::forget_bad`]).
async fn forget_bad(parts: &Parts, stage: &StageHandle) {
    let now = i64::try_from(parts.clock.now_ms()).unwrap_or(i64::MAX);
    match retention::forget_bad(stage, now).await {
        Ok(ids) => {
            if let Err(e) = retention::settle_notices(&parts.store, ids).await {
                tracing::warn!(error = %e, "the notices of forgotten history chunks could not be settled");
            }
        }
        Err(e) => tracing::warn!(error = %e, "old bad history chunks could not be forgotten"),
    }
}

/// Forgets the notes of live messages and names that nothing needs any more ([`live::tidy`]: a day before the oldest
/// chunk that waits for its delivery, else before now; the hint and a name are decided at the delivery).
async fn forget_old_live(parts: &Parts) {
    let now = i64::try_from(parts.clock.now_ms()).unwrap_or(i64::MAX);
    if let Err(e) = parts
        .store
        .write("forget old live messages", move |tx| live::tidy(tx, now))
        .await
    {
        tracing::warn!(error = %e, "the notes of live messages could not be pruned");
    }
}

/// Whether a look is taken now and then although nothing else wakes the driver (W4 fix F6): a chat still waits for
/// its end, or an announced chunk is unresolved (nothing wakes the driver when stage 2 records a notice or the time
/// passes that makes the end decidable). It stops when no notice is unresolved and no end is owed.
async fn looks_again(store: &KourierStore) -> bool {
    store
        .read(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM history_chats WHERE more = 1) \
                     OR (SELECT count(*) FROM history_announced WHERE lost = 0) \
                        > (SELECT count(*) FROM history_chunks)",
                [],
                |r| r.get::<_, bool>(0),
            )
            .map_err(crate::error::sql("look for a push that has not ended"))
        })
        .await
        .unwrap_or(false)
}

/// The pause after a failure: doubles up to the longest, starts over after a step that worked.
struct Pause {
    first: Duration,
    longest: Duration,
    next: Duration,
}

impl Pause {
    fn new(timing: Timing) -> Self {
        Self {
            first: timing.pause,
            longest: timing.pause_max,
            next: timing.pause,
        }
    }

    fn next(&mut self) -> Duration {
        let now = self.next;
        self.next = (self.next * 2).min(self.longest);
        now
    }

    fn reset(&mut self) {
        self.next = self.first;
    }
}

/// What the driver waits on.
struct Waits {
    wake: Arc<Notify>,
    acked: watch::Receiver<i64>,
    acked_open: bool,
    stop: CancellationToken,
    clock: Arc<dyn Clock>,
}

impl Waits {
    fn stopped(&self) -> bool {
        self.stop.is_cancelled()
    }

    /// Waits for a wake, a stop, `limit` of the clock and, when `for_acks`, an acknowledgement of the forwarder.
    /// `false`: stopped.
    async fn wait(&mut self, limit: Option<Duration>, for_acks: bool) -> bool {
        let Self {
            wake,
            acked,
            acked_open,
            stop,
            clock,
        } = self;
        let nap = async {
            match limit {
                Some(limit) => clock.sleep(limit).await,
                None => std::future::pending().await,
            }
        };
        let changed = async {
            if for_acks && *acked_open {
                acked.changed().await.is_ok()
            } else {
                std::future::pending().await
            }
        };
        let mut gone = false;
        tokio::select! {
            () = stop.cancelled() => return false,
            () = wake.notified() => {}
            () = nap => {}
            open = changed => gone = !open,
        }
        if gone {
            *acked_open = false;
        }
        !stop.is_cancelled()
    }

    /// Waits until the core answered (or the poison rule settled) every row up to `seq`. `false`: stopped.
    async fn answered(&self, store: &KourierStore, seq: i64) -> bool {
        tokio::select! {
            () = self.stop.cancelled() => false,
            () = outbox::wait_acked(store, self.acked.clone(), seq) => !self.stop.is_cancelled(),
        }
    }

    /// Sleeps `d` of the clock, or until stopped. `false`: stopped.
    async fn sleep(&mut self, d: Duration) -> bool {
        tokio::select! {
            () = self.stop.cancelled() => false,
            () = self.clock.sleep(d) => true,
        }
    }
}

#[cfg(test)]
mod tests;
