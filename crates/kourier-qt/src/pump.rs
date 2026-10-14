//! The `UiEvent` pump (§8.5): core events cross from tokio to the Qt thread through one queue,
//! with at most one queued drain closure at a time.
//!
//! A producer task on the app's runtime receives `UiEvent`s from the core's broadcast channel
//! and pushes them here; the first push after the queue went idle asks for a drain
//! ([`PumpQueue::push`] returns `true`), which the task posts with `CxxQtThread::queue`. The
//! drain runs on the Qt thread for at most [`DRAIN_BUDGET`], hands each item to the source
//! (which coalesces invalidations into at most one follow-up query per model), refreshes the
//! models once, and asks to be re-posted while items remain. A lagged channel becomes one
//! [`PumpItem::Lagged`]: the source then reloads every visible model (Delta Chat pattern).
//!
//! This module is Qt-free so the scheduling logic is tested with plain threads.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kourier_app::facade::UiEvent;

/// How long one drain may run on the Qt thread before it yields to the event loop.
pub const DRAIN_BUDGET: Duration = Duration::from_millis(8);

/// How long the producer task waits after it queued an event before it posts the drain, in ms
/// (0: it does not). A test sets it to widen the time between the core's `emit` and the front-end's
/// reaction, which is what a screenshot has to wait for (`App::screenshot_holds`).
static TEST_DELAY_MS: AtomicU64 = AtomicU64::new(0);

/// TEST ONLY: the Qt thread hears of a queued event `delay` late (the queue holds it
/// meanwhile, the next ones wait in the channel).
pub fn set_test_delay(delay: Duration) {
    TEST_DELAY_MS.store(
        u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
}

/// The delay of [`set_test_delay`].
pub(crate) fn test_delay() -> Duration {
    Duration::from_millis(TEST_DELAY_MS.load(Ordering::Relaxed))
}

/// One thing for the Qt thread.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "nearly every item is an event; boxing would add an allocation per event"
)]
pub enum PumpItem {
    /// A core event.
    Event(UiEvent),
    /// The receiver lagged and lost this many events: reload what is visible.
    Lagged(u64),
}

/// What a drain run left behind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drained {
    /// Items remain: post another drain (the scheduled flag stays set).
    More,
    /// The queue is empty and the flag is cleared: the next push schedules.
    Idle,
}

/// The queue between the producer task and the Qt thread.
#[derive(Debug, Default)]
pub struct PumpQueue {
    items: Mutex<VecDeque<PumpItem>>,
    scheduled: AtomicBool,
}

impl PumpQueue {
    /// An empty queue.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adds an item. Returns `true` when the caller must post a drain (none is pending).
    /// Consecutive lag notices merge into one.
    pub fn push(&self, item: PumpItem) -> bool {
        {
            let mut items = self.items.lock().unwrap_or_else(PoisonError::into_inner);
            match (items.back_mut(), item) {
                (Some(PumpItem::Lagged(n)), PumpItem::Lagged(m)) => *n = n.saturating_add(m),
                (_, item) => items.push_back(item),
            }
        }
        !self.scheduled.swap(true, Ordering::AcqRel)
    }

    /// Items waiting.
    pub fn len(&self) -> usize {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Whether nothing is waiting.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether a drain is posted or running.
    pub fn is_scheduled(&self) -> bool {
        self.scheduled.load(Ordering::Acquire)
    }

    fn pop(&self) -> Option<PumpItem> {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
    }

    /// Runs on the Qt thread: hands items to `f` until the queue is empty or `budget` is
    /// spent (at least one item is taken per run).
    pub fn drain(&self, budget: Duration, mut f: impl FnMut(PumpItem)) -> Drained {
        let start = Instant::now();
        loop {
            match self.pop() {
                Some(item) => f(item),
                None => {
                    self.scheduled.store(false, Ordering::Release);
                    // A push between the empty pop and the store saw the flag still set and
                    // did not schedule: take the duty back if there is something now.
                    if !self.is_empty() && !self.scheduled.swap(true, Ordering::AcqRel) {
                        return Drained::More;
                    }
                    return Drained::Idle;
                }
            }
            if start.elapsed() >= budget {
                return if self.is_empty() {
                    self.scheduled.store(false, Ordering::Release);
                    if !self.is_empty() && !self.scheduled.swap(true, Ordering::AcqRel) {
                        Drained::More
                    } else {
                        Drained::Idle
                    }
                } else {
                    Drained::More
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn ev() -> PumpItem {
        PumpItem::Event(UiEvent::AccountsChanged)
    }

    #[test]
    fn one_drain_is_posted_per_burst() {
        let q = PumpQueue::new();
        assert!(q.push(ev()), "the first push schedules");
        for _ in 0..999 {
            assert!(!q.push(ev()), "a drain is already pending");
        }
        let mut n = 0;
        assert_eq!(q.drain(Duration::from_secs(10), |_| n += 1), Drained::Idle);
        assert_eq!(n, 1000);
        assert!(!q.is_scheduled());
        assert!(q.push(ev()), "idle again: the next push schedules");
    }

    #[test]
    fn the_budget_splits_long_bursts_and_lags_merge() {
        let q = PumpQueue::new();
        q.push(ev());
        for _ in 0..10 {
            q.push(PumpItem::Lagged(2));
        }
        q.push(ev());
        assert_eq!(q.len(), 3, "consecutive lag notices merge");
        let mut seen = Vec::new();
        // A zero budget takes one item per run and asks for more.
        let first = q.drain(Duration::ZERO, |i| seen.push(i));
        assert_eq!(first, Drained::More);
        assert!(q.is_scheduled(), "the flag stays set while items remain");
        while q.drain(Duration::ZERO, |i| seen.push(i)) == Drained::More {}
        assert_eq!(seen.len(), 3);
        assert!(matches!(seen[1], PumpItem::Lagged(20)));
        assert!(!q.is_scheduled());
    }

    /// A producer thread races the drains: no item is lost and no drain is left unposted.
    #[test]
    fn no_lost_wakeups_under_contention() {
        const N: usize = 20_000;
        let q = PumpQueue::new();
        let posts = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let producer = {
            let (q, posts, tx) = (q.clone(), posts.clone(), tx.clone());
            std::thread::spawn(move || {
                for _ in 0..N {
                    if q.push(ev()) {
                        posts.fetch_add(1, Ordering::SeqCst);
                        let _ = tx.send(());
                    }
                }
            })
        };
        drop(tx);
        let mut handled = 0;
        // The "Qt thread": one drain per posted closure, re-posting itself on `More`.
        while handled < N {
            rx.recv_timeout(Duration::from_secs(10))
                .expect("a drain was posted for every pending item");
            while q.drain(Duration::from_micros(50), |_| handled += 1) == Drained::More {}
        }
        producer.join().expect("producer");
        assert_eq!(handled, N);
        assert!(q.is_empty());
        assert!(posts.load(Ordering::SeqCst) >= 1);
    }
}
