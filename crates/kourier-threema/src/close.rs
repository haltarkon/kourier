//! What a closed mediator connection means: every close code of the device-to-mediator protocol and
//! a chat server's `close-error` mapped to what the connection does next (TH2; the actor of TH3
//! uses it, the offline fake's close codes are tested against it).
//!
//! The table is the spec's (`threema-protocols/src/md-d2m.struct.yml`, "Close Codes" and
//! "Reconnect policy"):
//!
//! - `1xxx` and `40xx` allow an automatic reconnect, with linear backoff; when the connection fails
//!   again and again, the user is asked before the next attempt ([`Backoff`]).
//! - `41xx` never reconnects by itself: the user acts first. 4110 (unsupported protocol version)
//!   asks for a newer Kourier, 4111 (device limit) for a free slot on the phone, 4112 (the same
//!   device connected again) for a "Reconnect". 4113 (dropped by another device), 4114 (dropped
//!   because the reflection queue overflowed) and 4115 (device slot state mismatch) end the link:
//!   the slot is gone or no longer this device's, so only linking again helps.
//! - Any other code: a warning in the log, and an automatic reconnect.
//! - A `close-error` of the chat server decides alone: its `can-reconnect` byte (0 = no) wins over
//!   the close code that follows it.
//!
//! Pure: no I/O, no clock, no log. `LogoutCause` for 4114/4115 stays `Unknown` until the contract
//! gains words for them (lane decision L13); [`LoggedOut`] carries the code until then.

use std::time::Duration;

/// Why a link ended: the three codes after which this device is no longer in the device group as
/// it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoggedOut {
    /// 4113: another device of the group (the phone) dropped this one.
    DroppedByOtherDevice,
    /// 4114: the mediator dropped this device because its reflection queue reached the limit (a
    /// long pause).
    QueueOverflow,
    /// 4115: the device slot is not in the state the client expected (it vanished, or another
    /// device holds it): link again.
    SlotStateMismatch,
}

impl LoggedOut {
    /// The close code.
    pub fn code(self) -> u16 {
        match self {
            Self::DroppedByOtherDevice => 4113,
            Self::QueueOverflow => 4114,
            Self::SlotStateMismatch => 4115,
        }
    }
}

/// What the user must do before the next connection attempt (no automatic reconnect).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// 4110: the mediator does not speak the protocol version Kourier chose: update Kourier.
    UpdateApp,
    /// 4111: every device slot of the group is used: free one on the phone.
    TooManyDevices,
    /// 4112: the same device connected again elsewhere and this connection was ended: reconnect
    /// only on the user's word (two Kourier processes of one profile would otherwise take turns).
    Reconnect,
    /// The chat server said `close-error` with `can-reconnect = 0`.
    ChatServerRefused,
    /// Another `41xx` code the table does not know: the class forbids a reconnect (warn).
    Unknown(u16),
}

/// What a closed connection means for the next step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Reconnect by itself, after [`Backoff`]'s delay. `warn` for a code the table does not know
    /// (the spec: "a warning in the log, but automatic reconnects are allowed").
    Reconnect {
        /// The code was not one of the spec's.
        warn: bool,
    },
    /// Wait for the user.
    Stop(Stop),
    /// The link is over: link again.
    LoggedOut(LoggedOut),
}

/// A chat server's `close-error` (`csp.struct.yml` `close-error`): only its `can-reconnect` byte
/// matters here (its message is the server's text, never logged).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseError {
    /// `can-reconnect != 0`.
    pub can_reconnect: bool,
}

impl CloseError {
    /// From the wire byte: 0 means "may not reconnect automatically", any other value may.
    pub fn from_byte(can_reconnect: u8) -> Self {
        Self {
            can_reconnect: can_reconnect != 0,
        }
    }
}

/// The verdict for a connection that closed with `code`, after the chat server's `close-error`
/// when one came before the close.
pub fn verdict(code: u16, close_error: Option<CloseError>) -> Verdict {
    if let Some(e) = close_error {
        // "the reconnect policy solely depends on the close-error.can-reconnect field and the
        // (following) Close Code must be ignored".
        return if e.can_reconnect {
            Verdict::Reconnect { warn: false }
        } else {
            Verdict::Stop(Stop::ChatServerRefused)
        };
    }
    match code {
        // The spec's 1xxx (1000, 1001, 1011) and every other WebSocket code (1006 is what a
        // dropped connection reads as locally): reconnect.
        1000..=1999 => Verdict::Reconnect { warn: false },
        // Chat server (4000, 4001, 4009) and mediator (4010 protocol error, 4011 transaction TTL,
        // 4012 unknown message acked, 4013 idle) codes of the 40xx class, known or not.
        4000..=4099 => Verdict::Reconnect { warn: false },
        4110 => Verdict::Stop(Stop::UpdateApp),
        4111 => Verdict::Stop(Stop::TooManyDevices),
        4112 => Verdict::Stop(Stop::Reconnect),
        4113 => Verdict::LoggedOut(LoggedOut::DroppedByOtherDevice),
        4114 => Verdict::LoggedOut(LoggedOut::QueueOverflow),
        4115 => Verdict::LoggedOut(LoggedOut::SlotStateMismatch),
        4100..=4199 => Verdict::Stop(Stop::Unknown(code)),
        _ => Verdict::Reconnect { warn: true },
    }
}

/// The spec's own words for a close code (`threema-protocols/src/md-d2m.struct.yml`, "Close Codes"),
/// fixed English kept in the crate (critic H3: never put into a `NetworkNote`; the words the user
/// reads are the viewmodel's). Log-safe: it is the spec's text, never the server's close reason.
/// `None` for a code the spec does not name.
pub fn sentence(code: u16) -> Option<&'static str> {
    Some(match code {
        1000 => "Normal closure, connection successfully completed",
        1001 => "Server is shutting down",
        1011 => "Server terminated the connection due to an internal error",
        4000 => "Chat server connection closed",
        4001 => "Chat server connection could not be established",
        4009 => "Internal error related to chat server connection",
        4010 => "Protocol error",
        4011 => "Transaction TTL reached",
        4012 => "Unknown message acked",
        4013 => "Client idle timeout exceeded",
        4110 => "Unsupported protocol version",
        4111 => "Device limit reached",
        4112 => "Duplicate connection",
        4113 => "Dropped by other device",
        4114 => "Dropped by server because the reflection queue length limit was reached",
        4115 => "Device slot state mismatch",
        _ => return None,
    })
}

/// Linear backoff with a stop after repeated failures (the spec: "When automatically
/// reconnecting, linear backoff should be applied. In case the connection fails repeatedly, user
/// interaction should be required"). The step, the number of attempts and the time a connection
/// must stay up to count as having worked are Kourier's choice (the spec fixes none).
///
/// A connection WORKED when it stayed up [`Backoff::WORKED_AFTER`] (the count starts again), or when
/// the caller says so with [`Backoff::worked`]. The end of the handshake proves nothing: a
/// deterministic failure right after the login (a payload the server refuses at every start) would
/// reset the count every time and Kourier would reconnect every few seconds for ever. For the same
/// reason `ReflectionQueueDry`, which the mediator sends at once after `ServerInfo`, is no proof
/// either; use `worked` for a later, stronger sign.
#[derive(Clone, Debug, Default)]
pub struct Backoff {
    failures: u32,
}

/// What to do after a close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Connect again after this delay.
    After(Duration),
    /// Ask the user first: the verdict forbids a reconnect, or the attempts ran out.
    AskUser,
}

impl Backoff {
    /// The delay grows by this much per failure in a row.
    pub const STEP: Duration = Duration::from_secs(5);
    /// After this many failures in a row the user is asked.
    pub const MAX_FAILURES: u32 = 10;
    /// A connection that stayed up this long worked: the count starts again.
    pub const WORKED_AFTER: Duration = Duration::from_secs(60);

    /// The caller knows the connection worked (a sign stronger than the handshake or the first
    /// `ReflectionQueueDry`, see the type's doc): the count starts again.
    pub fn worked(&mut self) {
        self.failures = 0;
    }

    /// A connection that had been up for `up_for` (`Duration::ZERO` when it never got up) closed
    /// with `verdict`: when to try again.
    pub fn closed(&mut self, verdict: Verdict, up_for: Duration) -> Next {
        if up_for >= Self::WORKED_AFTER {
            self.worked();
        }
        match verdict {
            Verdict::Reconnect { .. } => {
                self.failures = self.failures.saturating_add(1);
                if self.failures > Self::MAX_FAILURES {
                    Next::AskUser
                } else {
                    Next::After(Self::STEP.saturating_mul(self.failures))
                }
            }
            Verdict::Stop(_) | Verdict::LoggedOut(_) => Next::AskUser,
        }
    }

    /// Failures in a row so far.
    pub fn failures(&self) -> u32 {
        self.failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every code of the spec's table, with the expectation written out (not computed).
    #[test]
    fn every_code_of_the_spec_maps_as_its_class_says() {
        let reconnect = Verdict::Reconnect { warn: false };
        let table: [(u16, Verdict); 16] = [
            (1000, reconnect),
            (1001, reconnect),
            (1011, reconnect),
            (4000, reconnect),
            (4001, reconnect),
            (4009, reconnect),
            (4010, reconnect),
            (4011, reconnect),
            (4012, reconnect),
            (4013, reconnect),
            (4110, Verdict::Stop(Stop::UpdateApp)),
            (4111, Verdict::Stop(Stop::TooManyDevices)),
            (4112, Verdict::Stop(Stop::Reconnect)),
            (4113, Verdict::LoggedOut(LoggedOut::DroppedByOtherDevice)),
            (4114, Verdict::LoggedOut(LoggedOut::QueueOverflow)),
            (4115, Verdict::LoggedOut(LoggedOut::SlotStateMismatch)),
        ];
        for (code, want) in table {
            assert_eq!(verdict(code, None), want, "close code {code}");
        }
        for out in [
            LoggedOut::DroppedByOtherDevice,
            LoggedOut::QueueOverflow,
            LoggedOut::SlotStateMismatch,
        ] {
            assert_eq!(
                verdict(out.code(), None),
                Verdict::LoggedOut(out),
                "code() of {out:?}"
            );
        }
    }

    #[test]
    fn codes_outside_the_table_follow_their_class_or_warn() {
        // Other 1xxx and 40xx: reconnect, no warning (the class allows it).
        for code in [1006, 1002, 4002, 4099] {
            assert_eq!(
                verdict(code, None),
                Verdict::Reconnect { warn: false },
                "{code}"
            );
        }
        // An unknown 41xx: the class forbids a reconnect.
        for code in [4100, 4116, 4199] {
            assert_eq!(
                verdict(code, None),
                Verdict::Stop(Stop::Unknown(code)),
                "{code}"
            );
        }
        // Anything else: warn and reconnect.
        for code in [0, 999, 2000, 3000, 4500, 4200, 4999, 5000, u16::MAX] {
            assert_eq!(
                verdict(code, None),
                Verdict::Reconnect { warn: true },
                "{code}"
            );
        }
    }

    /// The spec's sentence for each of its codes (literals, H3); nothing for another code.
    #[test]
    fn every_code_of_the_spec_has_its_sentence_and_no_other_code_has_one() {
        let table: [(u16, &str); 16] = [
            (1000, "Normal closure, connection successfully completed"),
            (1001, "Server is shutting down"),
            (
                1011,
                "Server terminated the connection due to an internal error",
            ),
            (4000, "Chat server connection closed"),
            (4001, "Chat server connection could not be established"),
            (4009, "Internal error related to chat server connection"),
            (4010, "Protocol error"),
            (4011, "Transaction TTL reached"),
            (4012, "Unknown message acked"),
            (4013, "Client idle timeout exceeded"),
            (4110, "Unsupported protocol version"),
            (4111, "Device limit reached"),
            (4112, "Duplicate connection"),
            (4113, "Dropped by other device"),
            (
                4114,
                "Dropped by server because the reflection queue length limit was reached",
            ),
            (4115, "Device slot state mismatch"),
        ];
        for (code, want) in table {
            assert_eq!(sentence(code), Some(want), "close code {code}");
        }
        for code in [0, 1002, 1006, 4002, 4099, 4100, 4116, 4199, 4999, u16::MAX] {
            assert_eq!(sentence(code), None, "close code {code}");
        }
    }

    /// A `close-error` decides alone, whatever code follows it.
    #[test]
    fn a_close_error_wins_over_the_code_that_follows() {
        let no = Some(CloseError::from_byte(0));
        let yes = Some(CloseError::from_byte(1));
        for code in [1000, 4000, 4010, 4110, 4112, 4113, 4115, 5000] {
            assert_eq!(
                verdict(code, no),
                Verdict::Stop(Stop::ChatServerRefused),
                "{code} after can-reconnect 0"
            );
            assert_eq!(
                verdict(code, yes),
                Verdict::Reconnect { warn: false },
                "{code} after can-reconnect 1"
            );
        }
        assert_eq!(
            CloseError::from_byte(0xff),
            CloseError {
                can_reconnect: true
            },
            "any non-zero byte may reconnect"
        );
    }

    #[test]
    fn backoff_is_linear_resets_when_it_worked_and_asks_after_the_last_attempt() {
        let mut b = Backoff::default();
        let again = Verdict::Reconnect { warn: false };
        let never_up = Duration::ZERO;
        for n in 1..=Backoff::MAX_FAILURES {
            assert_eq!(
                b.closed(again, never_up),
                Next::After(Backoff::STEP * n),
                "attempt {n}"
            );
        }
        assert_eq!(
            b.closed(again, never_up),
            Next::AskUser,
            "after the last attempt"
        );
        b.worked();
        assert_eq!(b.failures(), 0);
        assert_eq!(
            b.closed(Verdict::Reconnect { warn: true }, never_up),
            Next::After(Backoff::STEP),
            "a fresh count"
        );
        for v in [
            Verdict::Stop(Stop::UpdateApp),
            Verdict::LoggedOut(LoggedOut::QueueOverflow),
        ] {
            let mut b = Backoff::default();
            assert_eq!(b.closed(v, never_up), Next::AskUser, "{v:?}");
        }
    }

    /// G8: a connection that logs in and is closed with 4010 a moment later, ten times in a row,
    /// ends in `AskUser`: the handshake is no proof that the connection worked.
    #[test]
    fn ten_logins_that_fail_at_once_end_in_ask_user() {
        let mut b = Backoff::default();
        let protocol_error = verdict(4010, None);
        for n in 1..=Backoff::MAX_FAILURES {
            // The handshake ended and the login went through; the close came 2 s later.
            assert_eq!(
                b.closed(protocol_error, Duration::from_secs(2)),
                Next::After(Backoff::STEP * n),
                "cycle {n}"
            );
        }
        assert_eq!(
            b.closed(protocol_error, Duration::from_secs(2)),
            Next::AskUser,
            "the eleventh"
        );
    }

    /// A connection that stayed up the minimum time worked: the count starts again; one
    /// millisecond less and it did not.
    #[test]
    fn a_connection_that_stayed_up_a_minute_resets_the_count() {
        let mut b = Backoff::default();
        let again = Verdict::Reconnect { warn: false };
        for _ in 0..5 {
            b.closed(again, Duration::from_secs(1));
        }
        assert_eq!(b.failures(), 5);
        let just_short = Backoff::WORKED_AFTER - Duration::from_millis(1);
        assert_eq!(
            b.closed(again, just_short),
            Next::After(Backoff::STEP * 6),
            "a millisecond short of the minimum"
        );
        assert_eq!(
            b.closed(again, Backoff::WORKED_AFTER),
            Next::After(Backoff::STEP),
            "at the minimum"
        );
        assert_eq!(b.failures(), 1);
        // Cycles of 61 s never run out, however many.
        for _ in 0..(3 * Backoff::MAX_FAILURES) {
            assert_eq!(
                b.closed(again, Duration::from_secs(61)),
                Next::After(Backoff::STEP),
                "a long-lived connection"
            );
        }
    }
}
