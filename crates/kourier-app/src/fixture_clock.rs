//! The clock of fixture mode (`--fixture`, §10.2): the fixture's messages carry fixed dates, so
//! the core's "now" has to be near them. With the system clock every fixture message would be
//! older than the edit and delete windows (no message could be edited or deleted for everyone)
//! and its timers long over.
//!
//! This clock starts at a given instant and then runs in real time: timers and sleeps behave
//! as with the real clock, only the date is the fixture's. A page whose picture shows the time
//! since a message was sent (the line under an upload) FREEZES it at a chosen instant while the
//! messages are sent and thaws it afterwards ([`FixtureClock::freeze_at`]), so the digits do not
//! depend on how long the start took.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kourier_backend_api::{Clock, Sleep};

/// The frozen reading when the clock is not frozen.
const RUNNING: i64 = i64::MIN;

/// A real-time clock shifted so that it read `start_ms` when it was made. Clones share the one
/// clock (the app's core reads one, the fixture's seeding holds another).
#[derive(Clone, Debug)]
pub(crate) struct FixtureClock {
    state: Arc<State>,
}

#[derive(Debug)]
struct State {
    /// What to add to the system clock (ms) while it runs.
    offset_ms: AtomicI64,
    /// The reading while it is frozen, else [`RUNNING`].
    frozen_ms: AtomicI64,
}

fn system_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

impl FixtureClock {
    /// A clock that reads `start_ms` (ms since the Unix epoch) now.
    pub(crate) fn starting_at(start_ms: i64) -> Self {
        Self {
            state: Arc::new(State {
                offset_ms: AtomicI64::new(start_ms.saturating_sub(system_ms())),
                frozen_ms: AtomicI64::new(RUNNING),
            }),
        }
    }

    /// Stops the clock at `at_ms`: every reading is that instant until [`Self::thaw`].
    /// (A sleep that was started meanwhile still waits its real time.)
    pub(crate) fn freeze_at(&self, at_ms: i64) {
        self.state.frozen_ms.store(at_ms, Ordering::SeqCst);
    }

    /// Lets a frozen clock run on from the instant it stands at (no jump; nothing happens to a
    /// running one).
    pub(crate) fn thaw(&self) {
        let at = self.state.frozen_ms.load(Ordering::SeqCst);
        if at != RUNNING {
            // The offset first, so a reader between the two stores sees the frozen reading or
            // the same instant, never an old one.
            self.state
                .offset_ms
                .store(at.saturating_sub(system_ms()), Ordering::SeqCst);
            self.state.frozen_ms.store(RUNNING, Ordering::SeqCst);
        }
    }

    fn read(&self) -> i64 {
        match self.state.frozen_ms.load(Ordering::SeqCst) {
            RUNNING => system_ms().saturating_add(self.state.offset_ms.load(Ordering::SeqCst)),
            at => at,
        }
    }
}

impl Clock for FixtureClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.read()).unwrap_or(0)
    }

    fn sleep_until(&self, deadline_ms: u64) -> Sleep {
        let clock = self.clone();
        // Lazy, like the system clock: the tokio timer is created on first poll.
        Box::pin(async move {
            let wait = deadline_ms.saturating_sub(clock.now_ms());
            tokio::time::sleep(Duration::from_millis(wait)).await;
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_starts_at_the_given_instant_and_runs_on() {
        let start = 1_760_000_000_000;
        let clock = FixtureClock::starting_at(start);
        let a = clock.now_ms();
        assert!((start as u64..start as u64 + 2_000).contains(&a), "{a}");
        std::thread::sleep(Duration::from_millis(20));
        assert!(clock.now_ms() >= a + 15, "it runs in real time");
    }

    #[test]
    fn a_frozen_clock_stands_still_and_runs_on_from_there() {
        let start = 1_760_000_000_000;
        let clock = FixtureClock::starting_at(start);
        let twin = clock.clone();
        let at = start - 90_000;
        clock.freeze_at(at);
        std::thread::sleep(Duration::from_millis(30));
        // Every handle reads the frozen instant, however long it stands.
        assert_eq!(clock.now_ms(), at as u64);
        assert_eq!(twin.now_ms(), at as u64);
        twin.thaw();
        let a = clock.now_ms();
        assert!((at as u64..at as u64 + 1_000).contains(&a), "no jump: {a}");
        std::thread::sleep(Duration::from_millis(20));
        assert!(clock.now_ms() >= a + 15, "it runs in real time again");
        // A thaw of a running clock changes nothing.
        let b = clock.now_ms();
        clock.thaw();
        assert!(clock.now_ms() >= b && clock.now_ms() < b + 1_000);
    }

    #[tokio::test]
    async fn sleeps_are_measured_on_its_own_time() {
        let clock = FixtureClock::starting_at(1_000_000);
        let started = std::time::Instant::now();
        // A deadline in the past (of this clock) is due at once, whatever the system date is.
        clock.sleep_until(999_000).await;
        clock.sleep_until(clock.now_ms() + 30).await;
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(25) && took < Duration::from_secs(5),
            "{took:?}"
        );
    }
}
