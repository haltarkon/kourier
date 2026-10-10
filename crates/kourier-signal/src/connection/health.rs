//! Is the authenticated socket still alive, and is our clock right (§6.4, §9.6; Desktop's
//! `WebsocketResources.preload.ts:503-649` `KeepAlive` and `SocketManager.preload.ts:66-69,
//! 896-984` clock skew).
//!
//! libsignal-net pings the websocket itself. The keepalive request on top of it is what tells
//! that the *server* still answers on this socket, and its bookkeeping is what notices a
//! suspend: timers do not run while the machine sleeps, the wall clock does.

use std::time::{Duration, Instant};

/// A keepalive goes out this long after the last one was answered (`KEEPALIVE_INTERVAL_MS`).
pub(crate) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

/// A keepalive not answered within this closes the socket (`KEEPALIVE_TIMEOUT_MS`).
pub(crate) const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);

/// The same for a keepalive sent because the system said something changed (a resume, another
/// network): the answer of a live socket takes milliseconds, and the user is looking at the
/// window. Desktop uses 5 s when it believes it is offline (`OFFLINE_KEEPALIVE_TIMEOUT_MS`) and
/// 30 s otherwise; Kourier cannot tell the two apart and takes the short one, at the price of
/// a reconnect too many on a very slow link.
pub(crate) const CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// A socket whose last sign of life is older than this is closed without asking
/// (`STALE_THRESHOLD_MS`): the machine slept, and the server dropped the connection long ago.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(5 * 60);

/// Whether a socket last known alive at `last_alive_ms` is stale at `now_ms` (wall clock).
pub(crate) fn is_stale(last_alive_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_alive_ms) > millis(STALE_AFTER)
}

/// The remote-config entry with the largest clock difference the server accepts, in seconds.
pub(crate) const MAX_SKEW_KEY: &str = "client.maxAllowedClockSkewSeconds";

/// Without that entry (`MAX_ALLOWED_SKEW_FALLBACK`).
const MAX_SKEW_DEFAULT: Duration = Duration::from_secs(24 * 60 * 60);

/// The least the entry can ask for (`MAX_ALLOWED_SKEW_MIN`).
const MAX_SKEW_MIN: Duration = Duration::from_secs(60 * 60);

/// How often a wrong clock is looked at again (`SKEW_RECHECK_INTERVAL`).
pub(crate) const CLOCK_RECHECK: Duration = Duration::from_secs(10);

/// The largest clock difference tolerated, from the remote-config value (`getMaxAllowedSkew`):
/// a day without one or with one that does not parse, never less than an hour.
pub(crate) fn max_skew(configured: Option<&str>) -> Duration {
    configured
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(MAX_SKEW_DEFAULT, |secs| {
            Duration::from_secs(secs).max(MAX_SKEW_MIN)
        })
}

/// `d` in ms.
fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The server's time as a connect response stated it, and when we heard it.
///
/// Timestamps on messages cannot be more right than the clock that made them: with a clock far
/// off, sent messages sort wrongly on every device, sealed-sender certificates look expired,
/// and disappearing timers fire at the wrong moment. Desktop therefore refuses to stay
/// connected, and so does Kourier (§6.4).
#[derive(Clone, Copy, Debug)]
pub(crate) struct ServerClock {
    server_ms: u64,
    heard: Instant,
}

impl ServerClock {
    /// The server said `server_ms` just now.
    pub(crate) fn now(server_ms: u64) -> Self {
        Self {
            server_ms,
            heard: Instant::now(),
        }
    }

    /// The server's time now, by our monotonic clock.
    fn estimate(&self) -> u64 {
        self.server_ms.saturating_add(millis(self.heard.elapsed()))
    }

    /// How far the local wall clock `local_ms` is ahead of the server (negative: behind), in ms.
    pub(crate) fn skew_ms(&self, local_ms: u64) -> i64 {
        let diff = i128::from(local_ms) - i128::from(self.estimate());
        i64::try_from(diff).unwrap_or(if diff < 0 { i64::MIN } else { i64::MAX })
    }

    /// Whether the local clock is further off than `max`.
    pub(crate) fn is_skewed(&self, local_ms: u64, max: Duration) -> bool {
        self.skew_ms(local_ms).unsigned_abs() > millis(max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: u64 = 60 * 60 * 1000;

    #[test]
    fn a_socket_is_stale_five_minutes_after_its_last_sign_of_life() {
        let alive = 1_700_000_000_000;
        assert!(!is_stale(alive, alive));
        assert!(!is_stale(alive, alive + 5 * 60 * 1000));
        assert!(is_stale(alive, alive + 5 * 60 * 1000 + 1));
        assert!(
            is_stale(alive, alive + 10 * 60 * 1000),
            "a 10-minute suspend"
        );
        // A clock set back is not a suspend.
        assert!(!is_stale(alive, alive - HOUR_MS));
    }

    #[test]
    fn the_allowed_skew_is_a_day_unless_the_config_says_more_than_an_hour() {
        assert_eq!(max_skew(None), Duration::from_secs(24 * 3600));
        assert_eq!(max_skew(Some("7200")), Duration::from_secs(7200));
        assert_eq!(max_skew(Some(" 172800 ")), Duration::from_secs(172_800));
        assert_eq!(
            max_skew(Some("60")),
            Duration::from_secs(3600),
            "at least an hour"
        );
        assert_eq!(max_skew(Some("soon")), Duration::from_secs(24 * 3600));
        assert_eq!(max_skew(Some("-5")), Duration::from_secs(24 * 3600));
    }

    #[test]
    fn skew_is_local_minus_server_and_judged_against_the_limit() {
        let server = 1_700_000_000_000;
        let clock = ServerClock::now(server);
        let day = Duration::from_secs(24 * 3600);
        // The estimate moves with the monotonic clock: allow the test's own run time.
        let near = |skew: i64, expected: i64| (expected - skew).abs() < 5_000;
        assert!(near(clock.skew_ms(server), 0));
        assert!(near(clock.skew_ms(server + 25 * HOUR_MS), 25 * 3_600_000));
        assert!(near(clock.skew_ms(server - 25 * HOUR_MS), -25 * 3_600_000));
        assert!(!clock.is_skewed(server + 23 * HOUR_MS, day));
        assert!(clock.is_skewed(server + 25 * HOUR_MS, day), "ahead");
        assert!(clock.is_skewed(server - 25 * HOUR_MS, day), "behind");
        assert!(clock.is_skewed(0, day), "a clock at the epoch");
        assert!(clock.is_skewed(server + 2 * HOUR_MS, Duration::from_secs(3600)));
    }
}
