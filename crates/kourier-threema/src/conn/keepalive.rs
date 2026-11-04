//! The chat server's keep-alive (decision C9, Desktop's numbers: `connection.ts:173-178`): after the
//! login the client asks for an idle timeout of [`IDLE_S`] SECONDS (the spec's unit,
//! `csp.struct.yml:1001-1016`; libthreema's doc says minutes) and sends a CSP echo request every
//! [`ECHO_EVERY`]; an echo not answered within [`ANSWER_WITHIN`] means the socket is dead. Only CSP
//! frames reset the chat server's idle timer (D2M traffic never reaches it), so the echo is what
//! keeps the CSP leg alive; it is a D2M frame too, so the mediator's own timer is reset as well.
//!
//! The system's hints (brief §2.2): `NetworkChanged` on an open socket sends one echo, a PROBE,
//! whose answer is due within [`PROBE_WITHIN`]; `Resumed` does the same, unless the last frame
//! from the server is older than [`STALE_AFTER`]: then the socket is replaced without a probe
//! (Signal's rule, `kourier-signal/src/connection.rs`; after a long sleep the link is gone anyway).
//!
//! Pure: times in ms of the actor's clock, no I/O.

use std::time::Duration;

/// The idle timeout the client asks the chat server for, in SECONDS.
pub(crate) const IDLE_S: u16 = 30;
/// How often an echo request goes out.
pub(crate) const ECHO_EVERY: Duration = Duration::from_secs(10);
/// How long an echo may stay unanswered.
pub(crate) const ANSWER_WITHIN: Duration = Duration::from_secs(10);
/// How long a hint's probe may stay unanswered.
pub(crate) const PROBE_WITHIN: Duration = Duration::from_secs(5);
/// At `Resumed`, a socket that heard nothing from the server for longer than this is replaced.
pub(crate) const STALE_AFTER: Duration = Duration::from_secs(5 * 60);

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What is due at a wake-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Due {
    /// Nothing yet.
    Nothing,
    /// Send an echo request (its payload is the time it went out).
    Echo,
    /// The last echo was not answered in time: the socket is dead.
    Dead,
}

/// The echo schedule of one socket.
#[derive(Clone, Debug, Default)]
pub(crate) struct Keepalive {
    /// When the next echo goes out (`None` before the login).
    next_echo_ms: Option<u64>,
    /// When the unanswered echo's answer is due.
    answer_due_ms: Option<u64>,
    /// When the last frame from the server came (the socket's open time before the first).
    heard_ms: u64,
}

impl Keepalive {
    /// A socket opened at `now`.
    pub(crate) fn opened(now: u64) -> Self {
        Self {
            heard_ms: now,
            ..Self::default()
        }
    }

    /// A frame from the server came at `now`.
    pub(crate) fn heard(&mut self, now: u64) {
        self.heard_ms = self.heard_ms.max(now);
    }

    /// Whether the server was silent for longer than [`STALE_AFTER`] at `now`.
    pub(crate) fn stale(&self, now: u64) -> bool {
        now.saturating_sub(self.heard_ms) > ms(STALE_AFTER)
    }

    /// A hint's probe goes out at `now`: an echo whose answer is due within [`PROBE_WITHIN`] (or
    /// sooner, when an earlier echo's answer is due before that). The schedule stays.
    pub(crate) fn probe(&mut self, now: u64) {
        let due = now.saturating_add(ms(PROBE_WITHIN));
        self.answer_due_ms = Some(self.answer_due_ms.map_or(due, |t| t.min(due)));
    }

    /// The login finished at `now`: the first echo goes out [`ECHO_EVERY`] later.
    pub(crate) fn start(&mut self, now: u64) {
        self.next_echo_ms = Some(now.saturating_add(ms(ECHO_EVERY)));
        self.answer_due_ms = None;
    }

    /// The earliest time something is due (`None` before the login).
    pub(crate) fn deadline(&self) -> Option<u64> {
        match (self.next_echo_ms, self.answer_due_ms) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// What is due at `now`. An `Echo` is taken as sent at `now`.
    pub(crate) fn due(&mut self, now: u64) -> Due {
        if self.answer_due_ms.is_some_and(|t| t <= now) {
            return Due::Dead;
        }
        match self.next_echo_ms {
            Some(t) if t <= now => {
                self.next_echo_ms = Some(now.saturating_add(ms(ECHO_EVERY)));
                // An earlier answer still due (a hint's probe) is kept (P-L1): the regular echo
                // must not push a probe's 5 s out to 10 s.
                let due = now.saturating_add(ms(ANSWER_WITHIN));
                self.answer_due_ms = Some(self.answer_due_ms.map_or(due, |d| d.min(due)));
                Due::Echo
            }
            _ => Due::Nothing,
        }
    }

    /// An echo response came.
    pub(crate) fn answered(&mut self) {
        self.answer_due_ms = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_echo_every_ten_seconds_and_dead_when_one_stays_unanswered() {
        let mut k = Keepalive::default();
        assert_eq!(k.deadline(), None, "nothing before the login");
        k.start(1_000);
        assert_eq!(k.deadline(), Some(11_000));
        assert_eq!(k.due(10_999), Due::Nothing);
        assert_eq!(k.due(11_000), Due::Echo);
        assert_eq!(k.deadline(), Some(21_000), "the next echo and the answer");
        k.answered();
        assert_eq!(k.due(21_000), Due::Echo, "answered: the next echo");
        assert_eq!(k.due(31_000), Due::Dead, "not answered within 10 s");
        assert_eq!(IDLE_S, 30, "seconds, Desktop's number");
    }

    #[test]
    fn a_probe_is_due_in_five_seconds_and_silence_over_five_minutes_is_stale() {
        let mut k = Keepalive::opened(1_000);
        k.start(1_000);
        k.probe(2_000);
        assert_eq!(k.deadline(), Some(7_000), "the probe's answer");
        assert_eq!(k.due(7_000), Due::Dead, "the probe stayed unanswered");
        let mut k = Keepalive::opened(1_000);
        k.start(1_000);
        assert_eq!(k.due(11_000), Due::Echo);
        k.probe(18_000);
        assert_eq!(k.deadline(), Some(21_000), "an earlier answer stays due");
        k.answered();
        assert_eq!(k.deadline(), Some(21_000), "the next echo");
        assert!(!k.stale(301_000), "exactly five minutes");
        assert!(k.stale(301_001), "five minutes and a ms");
        k.heard(200_000);
        assert!(!k.stale(500_000));
        assert!(k.stale(500_001));
    }

    /// P-L1 (the protocol review's F2): a regular echo does not move an earlier probe deadline. The
    /// probe at +18 s is due at +23 s; the regular echo at +20 s (answer due +30 s) leaves it.
    #[test]
    fn a_regular_echo_keeps_a_probes_earlier_deadline() {
        let mut k = Keepalive::opened(0);
        k.start(0);
        assert_eq!(k.due(10_000), Due::Echo);
        k.answered();
        k.probe(18_000);
        assert_eq!(k.deadline(), Some(20_000), "the next echo");
        assert_eq!(k.due(20_000), Due::Echo);
        assert_eq!(k.deadline(), Some(23_000), "the probe's answer stays due");
        assert_eq!(k.due(23_000), Due::Dead, "dead at the probe's deadline");
        // Without a probe the echo's answer is due 10 s after it went out.
        let mut k = Keepalive::opened(0);
        k.start(0);
        assert_eq!(k.due(10_000), Due::Echo);
        assert_eq!(k.deadline(), Some(20_000));
        assert_eq!(k.due(20_000), Due::Dead);
    }
}
