//! Reconnect delays of the chat sockets (§6.4; Desktop's `SD/ts/util/BackOff.std.ts` and its use
//! in `SocketManager.preload.ts`: Fibonacci seconds with up to 5 s of jitter, a longer tail
//! while offline).

use std::time::Duration;

/// Delays after consecutive failures (`FIBONACCI_TIMEOUTS`), in seconds.
const FIBONACCI_SECS: [u64; 9] = [1, 2, 3, 5, 8, 13, 21, 34, 55];

/// How the delays go on while the server cannot be reached at all
/// (`EXTENDED_FIBONACCI_TIMEOUTS`, up to about 26 minutes), in seconds.
const OFFLINE_SECS: [u64; 7] = [89, 144, 233, 377, 610, 987, 1597];

/// Most random time added to a delay (`JITTER`), so that the clients of a server that comes
/// back do not all return in the same second. Not added to delays that are not longer than it.
const JITTER: Duration = Duration::from_secs(5);

/// The reconnect delays of one socket.
///
/// Desktop switches to the long tail when the browser reports "offline". Kourier has no such
/// signal (NetworkManager's hint says only that something changed), so the tail is used while
/// every failure since the last reset was below HTTP (no route, timeout): the server was never
/// reached, which is what offline means here. A failure the server itself answered keeps the
/// delays at 55 s. A network change or a resume resets everything ([`Self::reset`]).
#[derive(Debug)]
pub(crate) struct Backoff {
    /// Consecutive failures since the last reset.
    failures: usize,
    /// Some failure since the last reset was answered by the server.
    reached: bool,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

impl Backoff {
    /// No failures yet.
    pub(crate) const fn new() -> Self {
        Self {
            failures: 0,
            reached: false,
        }
    }

    /// Forgets the failures (connected and caught up, the network changed, the user asked).
    pub(crate) fn reset(&mut self) {
        *self = Self::new();
    }

    /// The delay before the next attempt after one more failure. `unreachable`: the failure was
    /// below HTTP. `random` is uniform in `0.0..1.0`.
    pub(crate) fn next_with(&mut self, unreachable: bool, random: f64) -> Duration {
        if !unreachable {
            self.reached = true;
        }
        let secs = match FIBONACCI_SECS.get(self.failures) {
            Some(secs) => *secs,
            None if self.reached => FIBONACCI_SECS[FIBONACCI_SECS.len() - 1],
            None => {
                let tail = self.failures - FIBONACCI_SECS.len();
                OFFLINE_SECS[tail.min(OFFLINE_SECS.len() - 1)]
            }
        };
        self.failures = self.failures.saturating_add(1);
        let delay = Duration::from_secs(secs);
        if JITTER < delay {
            delay + JITTER.mul_f64(random.clamp(0.0, 1.0))
        } else {
            delay
        }
    }

    /// [`Self::next_with`] with fresh randomness.
    pub(crate) fn next(&mut self, unreachable: bool) -> Duration {
        self.next_with(unreachable, rand::random::<f64>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(backoff: &mut Backoff, unreachable: bool) -> u64 {
        backoff.next_with(unreachable, 0.0).as_secs()
    }

    #[test]
    fn delays_follow_desktops_fibonacci_and_stop_at_55_seconds() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..12).map(|_| secs(&mut backoff, false)).collect();
        assert_eq!(delays, [1, 2, 3, 5, 8, 13, 21, 34, 55, 55, 55, 55]);
        backoff.reset();
        assert_eq!(secs(&mut backoff, false), 1, "a reset starts over");
    }

    #[test]
    fn delays_grow_on_while_the_server_is_never_reached() {
        let mut backoff = Backoff::new();
        let delays: Vec<u64> = (0..18).map(|_| secs(&mut backoff, true)).collect();
        assert_eq!(
            delays,
            [
                1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 1597, 1597
            ]
        );
        // One answer from the server: it is there, so no more than 55 s between attempts.
        assert_eq!(secs(&mut backoff, false), 55);
        assert_eq!(secs(&mut backoff, true), 55, "until the next reset");
    }

    #[test]
    fn jitter_is_added_only_to_delays_longer_than_it() {
        let mut backoff = Backoff::new();
        let delays: Vec<Duration> = (0..6).map(|_| backoff.next_with(false, 0.5)).collect();
        assert_eq!(delays[0], Duration::from_secs(1));
        assert_eq!(
            delays[3],
            Duration::from_secs(5),
            "5 s is not longer than the jitter"
        );
        assert_eq!(delays[4], Duration::from_millis(8_000 + 2_500));
        assert_eq!(delays[5], Duration::from_millis(13_000 + 2_500));
        // Never more than 5 s on top, whatever the random source gives.
        let mut backoff = Backoff::new();
        for _ in 0..40 {
            let plain = Backoff {
                failures: backoff.failures,
                reached: backoff.reached,
            }
            .next_with(true, 0.0);
            let jittered = backoff.next(true);
            assert!(
                jittered >= plain && jittered <= plain + JITTER,
                "{jittered:?}"
            );
        }
    }
}
