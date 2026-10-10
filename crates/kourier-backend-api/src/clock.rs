//! Wall-clock time for the core and backends (architecture §9.6). Every timer (outbox backoff,
//! supervisor restarts, typing, disappearing sweeper, key maintenance) takes a [`Clock`], so
//! tests drive time with [`FakeClock`] instead of sleeping.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The future returned by [`Clock::sleep_until`].
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A source of wall-clock time and timers. Dyn-compatible: shared as `Arc<dyn Clock>`.
pub trait Clock: Send + Sync + 'static {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;

    /// Completes once [`Clock::now_ms`] has reached `deadline_ms` (at once if it has).
    /// [`SystemClock`] needs a tokio runtime with the time driver enabled.
    fn sleep_until(&self, deadline_ms: u64) -> Sleep;

    /// Completes after `d` of this clock's time.
    fn sleep(&self, d: Duration) -> Sleep {
        self.sleep_until(self.now_ms().saturating_add(duration_ms(d)))
    }
}

/// `d` in whole milliseconds, saturating.
pub fn duration_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The real clock: `SystemTime` for `now_ms`, tokio timers for sleeps.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, duration_ms)
    }

    fn sleep_until(&self, deadline_ms: u64) -> Sleep {
        // Lazy: the tokio timer is created on first poll, inside the runtime, so the future
        // may be built anywhere.
        Box::pin(async move {
            let wait = deadline_ms.saturating_sub(SystemClock.now_ms());
            tokio::time::sleep(Duration::from_millis(wait)).await;
        })
    }
}

/// A manually driven clock for tests. Time only moves with [`FakeClock::set`],
/// [`FakeClock::advance`] or [`FakeClock::advance_to_next`]; sleeps whose deadline is reached
/// are woken then. Clones share the same time.
#[derive(Clone, Debug)]
pub struct FakeClock {
    inner: Arc<FakeInner>,
}

#[derive(Debug)]
struct FakeInner {
    now: AtomicU64,
    next_id: AtomicU64,
    sleepers: Mutex<BTreeMap<u64, Sleeper>>,
}

#[derive(Debug)]
struct Sleeper {
    deadline: u64,
    waker: Waker,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panicking test thread must not wedge every other user of the clock.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeClock {
    /// A clock that reads `start_ms`.
    pub fn new(start_ms: u64) -> Self {
        Self {
            inner: Arc::new(FakeInner {
                now: AtomicU64::new(start_ms),
                next_id: AtomicU64::new(0),
                sleepers: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// Sets the time (it may jump backwards, like a wall clock) and wakes due sleeps.
    pub fn set(&self, ms: u64) {
        self.inner.now.store(ms, Ordering::SeqCst);
        let due: Vec<Waker> = {
            let mut sleepers = lock(&self.inner.sleepers);
            let ids: Vec<u64> = sleepers
                .iter()
                .filter(|(_, s)| s.deadline <= ms)
                .map(|(id, _)| *id)
                .collect();
            ids.iter()
                .filter_map(|id| sleepers.remove(id))
                .map(|s| s.waker)
                .collect()
        };
        due.into_iter().for_each(Waker::wake);
    }

    /// Moves time forward by `d`; returns the new time.
    pub fn advance(&self, d: Duration) -> u64 {
        let now = self.now_ms().saturating_add(duration_ms(d));
        self.set(now);
        now
    }

    /// Number of sleeps that are waiting (polled and not yet due).
    pub fn sleepers(&self) -> usize {
        lock(&self.inner.sleepers).len()
    }

    /// The earliest deadline of a waiting sleep.
    pub fn next_deadline(&self) -> Option<u64> {
        lock(&self.inner.sleepers)
            .values()
            .map(|s| s.deadline)
            .min()
    }

    /// The deadlines of every waiting sleep, earliest first (tests that check what a timer
    /// was armed for).
    pub fn deadlines(&self) -> Vec<u64> {
        let mut all: Vec<u64> = lock(&self.inner.sleepers)
            .values()
            .map(|s| s.deadline)
            .collect();
        all.sort_unstable();
        all
    }

    /// Jumps to the earliest waiting deadline (never backwards) and wakes it; returns the new
    /// time, or `None` when nothing waits.
    pub fn advance_to_next(&self) -> Option<u64> {
        let next = self.next_deadline()?.max(self.now_ms());
        self.set(next);
        Some(next)
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.inner.now.load(Ordering::SeqCst)
    }

    fn sleep_until(&self, deadline_ms: u64) -> Sleep {
        Box::pin(FakeSleep {
            inner: self.inner.clone(),
            deadline: deadline_ms,
            id: None,
        })
    }
}

struct FakeSleep {
    inner: Arc<FakeInner>,
    deadline: u64,
    id: Option<u64>,
}

impl Future for FakeSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        let mut sleepers = lock(&this.inner.sleepers);
        // Checked under the lock: `set` stores the time before it takes the lock, so a sleep
        // either sees the new time here or is found and woken by `set`.
        if this.inner.now.load(Ordering::SeqCst) >= this.deadline {
            if let Some(id) = this.id.take() {
                sleepers.remove(&id);
            }
            return Poll::Ready(());
        }
        let next_id = &this.inner.next_id;
        let id = *this
            .id
            .get_or_insert_with(|| next_id.fetch_add(1, Ordering::Relaxed));
        sleepers.insert(
            id,
            Sleeper {
                deadline: this.deadline,
                waker: cx.waker().clone(),
            },
        );
        Poll::Pending
    }
}

impl Drop for FakeSleep {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            lock(&self.inner.sleepers).remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::task::Wake;

    struct CountingWaker(AtomicUsize);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn fake_sleep_completes_only_when_time_is_reached() {
        let clock = FakeClock::new(1_000);
        let counter = Arc::new(CountingWaker(AtomicUsize::new(0)));
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut sleep = clock.sleep(Duration::from_millis(500));
        assert!(sleep.as_mut().poll(&mut cx).is_pending());
        assert_eq!((clock.sleepers(), clock.next_deadline()), (1, Some(1_500)));
        assert_eq!(clock.advance(Duration::from_millis(499)), 1_499);
        assert_eq!(counter.0.load(Ordering::SeqCst), 0);
        assert!(sleep.as_mut().poll(&mut cx).is_pending());
        clock.advance(Duration::from_millis(1));
        assert_eq!(counter.0.load(Ordering::SeqCst), 1, "woken when due");
        assert_eq!(clock.sleepers(), 0);
        assert!(sleep.as_mut().poll(&mut cx).is_ready());
    }

    #[test]
    fn past_deadline_is_ready_and_dropped_sleeps_deregister() {
        let clock = FakeClock::new(50);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(clock.sleep_until(10).as_mut().poll(&mut cx).is_ready());
        let mut a = clock.sleep_until(100);
        let mut b = clock.sleep_until(70);
        assert!(a.as_mut().poll(&mut cx).is_pending() && b.as_mut().poll(&mut cx).is_pending());
        assert!(
            a.as_mut().poll(&mut cx).is_pending(),
            "re-poll keeps one registration"
        );
        assert_eq!(clock.sleepers(), 2);
        assert_eq!(clock.deadlines(), [70, 100]);
        assert_eq!(clock.advance_to_next(), Some(70));
        assert!(b.as_mut().poll(&mut cx).is_ready());
        drop(a);
        assert_eq!((clock.sleepers(), clock.advance_to_next()), (0, None));
        let shared = clock.clone();
        shared.set(5);
        assert_eq!(clock.now_ms(), 5, "clones share time; it may go backwards");
    }

    #[test]
    fn system_clock_is_wall_time_and_sleeps() {
        let now = SystemClock.now_ms();
        assert!(now > 1_700_000_000_000, "{now}");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("runtime");
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        rt.block_on(clock.sleep(Duration::from_millis(5)));
        assert!(SystemClock.now_ms() >= now + 5);
        assert_eq!(duration_ms(Duration::MAX), u64::MAX);
    }
}
