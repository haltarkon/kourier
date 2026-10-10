//! The clock of fixture mode (`--fixture`, §10.2): the fixture's messages carry fixed dates, so
//! the core's "now" has to be near them. With the system clock every fixture message would be
//! older than the edit and delete windows (no message could be edited or deleted for everyone)
//! and its timers long over.
//!
//! This clock starts at a given instant and then runs in real time: timers and sleeps behave
//! as with the real clock, only the date is the fixture's.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kourier_backend_api::{Clock, Sleep};

/// A real-time clock shifted so that it read `start_ms` when it was made.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixtureClock {
    /// What to add to the system clock (ms).
    offset_ms: i64,
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
            offset_ms: start_ms.saturating_sub(system_ms()),
        }
    }
}

impl Clock for FixtureClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(system_ms().saturating_add(self.offset_ms)).unwrap_or(0)
    }

    fn sleep_until(&self, deadline_ms: u64) -> Sleep {
        let clock = *self;
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
