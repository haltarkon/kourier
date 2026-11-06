//! What the end of one socket means for the account: the close table of [`crate::close`] turned
//! into the next step ([`Next`]) and the [`ConnState`] the core sees, and the one rule of
//! publishing it ([`publish`]: nothing replaces a `LoggedOut` but another `LoggedOut`, Signal's
//! `set_conn`). Threema's own sentence for a code stays in the crate's verdict (critic H3): every
//! note is `NetworkNote::default()`. Pure but for the log line of an unknown code.

use std::time::Duration;

use kourier_model::{ConnState, LogoutCause, UserAction};
use tokio::sync::watch;

use crate::NetError;
use crate::close::{Backoff, CloseError, LoggedOut, Next as Pace, Stop, Verdict, verdict};

/// Critic T-H1: how long a socket that stepped aside for the phone waits before the next (the critic's 2 min).
pub(crate) const YIELD_PAUSE: Duration = Duration::from_secs(2 * 60);

/// How one socket ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The actor stopped it (Disconnect, Logout, Shutdown): closed with 1000.
    Stopped,
    /// The connector failed before a socket was open.
    NotReached(NetError),
    /// The connect and the handshake did not finish within 20 s of the actor's clock (closed with
    /// 1000).
    TimedOut,
    /// At `Resumed`, the server had been silent for over five minutes: the client closed with
    /// 1000 to connect again at once (not counted in the backoff).
    Stale,
    /// An echo stayed unanswered, or a write stalled past its bound (closed with 1000).
    Dead {
        /// How long the socket had been up since `ServerInfo`.
        up_for: Duration,
    },
    /// The chat server and the mediator answer, but an ack of the running send row stayed away for `ACK_WITHIN` (closed
    /// with 1000): the row stands and a later socket writes what is owed.
    NoAck {
        /// How long the socket had been up since `ServerInfo`.
        up_for: Duration,
    },
    /// libthreema refused what the server sent: the client closed with 4110 (`unsupported`, the
    /// server's version) or 4010 (critic M9).
    Protocol {
        /// The server's protocol version is one Kourier does not speak.
        unsupported: bool,
        /// How long the socket had been up since `ServerInfo`.
        up_for: Duration,
    },
    /// A server's time was more than 20 minutes from the actor's clock (closed with 1000; C2).
    ClockSkew {
        /// The local clock minus the server's (ms, positive: local ahead).
        skew_ms: i64,
    },
    /// Step 0 of the reflection loop (TH5 decision B1): more reflected envelopes came before the file
    /// was join-ready than are held (30 or 2 MiB): the client closed with 1000 to connect again when
    /// the file is ready, or after one backoff step (S10; not counted in the backoff); the mediator
    /// delivers them again.
    HeldFull,
    /// A reflected batch's zone failed in the store itself (decision H3: an I/O error): nothing more
    /// was acked, the client closed with 1000; the mediator delivers the rest again.
    Store {
        /// How long the socket had been up since `ServerInfo`.
        up_for: Duration,
    },
    /// The join's first `ServerInfo` did not say `NEW` (TH4): closed with 1000; the task ends
    /// (`conn::End::JoinRefused`) before the table is asked.
    JoinRefused,
    /// RP-b: the mediator dropped the own device at the actor's ask (its ack, or its 4113 / 4114 after the ask); the
    /// task ends (`conn::End::Dropped`) before the table is asked.
    Dropped,
    /// THE H1 GUARD (TH6 C1, `TP/csp.struct.yml:688-689`): the chat server sent a message on a socket that may not take
    /// one (not the leader, or its unblock has not gone): the client closed with 1000 and acked nothing; the server keeps
    /// the message for the device that leads. Not the row's failure: through the backoff as a lost network.
    Unasked {
        /// How long the socket had been up since `ServerInfo`.
        up_for: Duration,
    },
    /// TH6 FIX-B, P-L6: the leader recycled the socket because boxes were refused for room (critic T-H2) although NOT ONE
    /// message was acked on it (everything it took was held): the client closed with 1000, and the next socket comes
    /// after one backoff step ([`Backoff::STEP`]), not counted, as [`Outcome::HeldFull`]'s: a backlog the leader cannot
    /// get through must not make it reconnect in a tight loop. With an ack on the socket the recycle is [`Outcome::Stale`].
    NoProgress,
    /// TH6 L6, critic T-H1's STEP-ASIDE: the leader socket held a message only the phone resolves and the mediator showed
    /// another device connected: the client closed with 1000 ONCE so the mediator can promote that device. Not a failed
    /// run, not counted; the next socket after [`YIELD_PAUSE`] (a hint connects at once, as after any wait).
    Yield,
    /// The server closed the socket (1006: it ended without a close frame).
    Closed {
        /// The close code.
        code: u16,
        /// The chat server's `close-error` before the close, if one came.
        close_error: Option<CloseError>,
        /// How long the socket had been up since `ServerInfo`.
        up_for: Duration,
    },
}

/// What the connection does next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Stopped by the actor: nothing.
    Stopped,
    /// Connect again after this delay (`Offline { retry_at_ms }`).
    After(Duration),
    /// Wait for the user (`NeedsUserAction`); `hints`: a `NetworkChanged` or `Resumed` makes one
    /// attempt (QA: after the attempts ran out; an untrusted server, another network may not
    /// intercept), else only `Connect` does.
    Hold {
        /// What the user is asked.
        action: UserAction,
        /// Whether a hint tries once.
        hints: bool,
    },
    /// The link is over.
    LoggedOut {
        /// Log-safe reason (the code).
        reason: &'static str,
        /// Why, for the UI.
        cause: LogoutCause,
    },
}

impl Outcome {
    /// Whether this end says something about the row whose frames stood unanswered (decision R2): a missing ack, or a
    /// close that came FROM THE SERVER, is a failed run of it (`crate::send::shared`). A lost network says nothing about
    /// the row, and a phone's Wi-Fi change must not delay a send by minutes: `Dead` (a write that stalled too), 1006 (the
    /// socket ended without a close frame, nobody closed it), `NotReached`, `TimedOut`, `Stale`. The rest is Kourier's own
    /// end. No wildcard: a new way to end decides here.
    pub(crate) fn is_failed_run(&self) -> bool {
        match self {
            Self::NoAck { .. } => true,
            Self::Closed { code, .. } => *code != 1006,
            Self::Stopped
            | Self::NotReached(_)
            | Self::TimedOut
            | Self::Stale
            | Self::Dead { .. }
            | Self::Protocol { .. }
            | Self::ClockSkew { .. }
            | Self::HeldFull
            | Self::NoProgress
            | Self::Store { .. }
            | Self::JoinRefused
            | Self::Dropped
            | Self::Unasked { .. }
            | Self::Yield => false,
        }
    }
}

/// The next step after `outcome`, counting it in `backoff`.
pub(crate) fn next(outcome: &Outcome, backoff: &mut Backoff) -> Next {
    let again = Verdict::Reconnect { warn: false };
    let (verdict, up_for) = match outcome {
        // A refused join and a dropped device end the task before the table (`conn::run`).
        Outcome::Stopped | Outcome::JoinRefused | Outcome::Dropped => return Next::Stopped,
        Outcome::Stale => return Next::After(Duration::ZERO),
        // T-H1: the phone gets its moment to take the lead; not counted.
        Outcome::Yield => return Next::After(YIELD_PAUSE),
        // S10: not a tight loop while zone 2 is pending: one backoff step at most (`conn::run` also
        // connects as soon as the file is ready); not counted.
        Outcome::HeldFull => return Next::After(Backoff::STEP),
        // P-L6: a recycle with nothing acked on the socket: the same one step, not counted.
        Outcome::NoProgress => return Next::After(Backoff::STEP),
        // C2: held; a `Connect` or a hint measures again (a clock NTP fixed after a wake).
        Outcome::ClockSkew { skew_ms } => {
            return Next::Hold {
                action: UserAction::FixClock { skew_ms: *skew_ms },
                hints: true,
            };
        }
        Outcome::NotReached(NetError::Untrusted(kind)) => {
            tracing::warn!(?kind, "{}", super::lines::E3_UNTRUSTED);
            return Next::Hold {
                action: UserAction::UpdateApp,
                hints: true,
            };
        }
        Outcome::NotReached(NetError::CertificateDates) => {
            // C8: a wrong local clock causes it as well as an expired certificate.
            tracing::warn!("the server's certificate is outside its dates");
            (again, Duration::ZERO)
        }
        Outcome::NotReached(NetError::Failed(_) | NetError::Refused(_)) | Outcome::TimedOut => {
            (again, Duration::ZERO)
        }
        Outcome::Dead { up_for }
        | Outcome::NoAck { up_for }
        | Outcome::Store { up_for }
        | Outcome::Unasked { up_for } => (again, *up_for),
        Outcome::Protocol {
            unsupported: true, ..
        } => {
            return Next::Hold {
                action: UserAction::UpdateApp,
                hints: false,
            };
        }
        Outcome::Protocol {
            unsupported: false,
            up_for,
        } => (again, *up_for),
        Outcome::Closed {
            code,
            close_error,
            up_for,
        } => {
            let v = verdict(*code, *close_error);
            if v == (Verdict::Reconnect { warn: true }) {
                // The spec: "a warning in the log". The code only, never the reason.
                tracing::warn!(
                    code,
                    "the mediator closed with a code Kourier does not know"
                );
            }
            (v, *up_for)
        }
    };
    let pace = backoff.closed(verdict, up_for);
    match verdict {
        Verdict::LoggedOut(why) => logged_out(why),
        Verdict::Stop(stop) => Next::Hold {
            action: match stop {
                Stop::UpdateApp => UserAction::UpdateApp,
                // C3, C4: the contract has no closer variant until L13's commit after P5.
                Stop::TooManyDevices
                | Stop::Reconnect
                | Stop::ChatServerRefused
                | Stop::Unknown(_) => UserAction::Reconnect,
            },
            hints: false,
        },
        Verdict::Reconnect { .. } => match pace {
            Pace::After(delay) => Next::After(delay),
            // QA: about ten failures in a row: ask; a hint still tries once.
            Pace::AskUser => Next::Hold {
                action: UserAction::Reconnect,
                hints: true,
            },
        },
    }
}

/// C5: 4113 is "removed on the phone"; 4114 and 4115 have no cause of their own until L13.
fn logged_out(why: LoggedOut) -> Next {
    match why {
        LoggedOut::DroppedByOtherDevice => Next::LoggedOut {
            reason: "4113 dropped by another device",
            cause: LogoutCause::RemovedOnPhone,
        },
        LoggedOut::QueueOverflow => Next::LoggedOut {
            reason: "4114 dropped by the mediator",
            cause: LogoutCause::Unknown,
        },
        LoggedOut::SlotStateMismatch => Next::LoggedOut {
            reason: "4115 device slot state mismatch",
            cause: LogoutCause::Unknown,
        },
    }
}

/// Publishes `new` unless the account is logged out already (a `LoggedOut` is final but for
/// another `LoggedOut`) or the state is the same.
pub(crate) fn publish(tx: &watch::Sender<ConnState>, new: ConnState) {
    tx.send_if_modified(|state| {
        let final_already = matches!(state, ConnState::LoggedOut { .. })
            && !matches!(new, ConnState::LoggedOut { .. });
        if final_already || *state == new {
            return false;
        }
        *state = new;
        true
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_logged_out_state_is_final_but_for_another_logout() {
        let (tx, rx) = watch::channel(ConnState::Connecting);
        let out = ConnState::LoggedOut {
            reason: "x".to_owned(),
            cause: LogoutCause::Unknown,
            note: Default::default(),
        };
        publish(&tx, out.clone());
        publish(&tx, ConnState::Offline { retry_at_ms: None });
        assert_eq!(*rx.borrow(), out);
    }

    #[test]
    fn an_untrusted_server_or_an_old_version_holds_and_4113_ends_the_link() {
        let mut b = Backoff::default();
        assert_eq!(
            next(
                &Outcome::NotReached(NetError::Untrusted(crate::UntrustedKind::PinMismatch)),
                &mut b
            ),
            Next::Hold {
                action: UserAction::UpdateApp,
                hints: true
            }
        );
        assert_eq!(
            next(
                &Outcome::Protocol {
                    unsupported: true,
                    up_for: Duration::ZERO
                },
                &mut b
            ),
            Next::Hold {
                action: UserAction::UpdateApp,
                hints: false
            }
        );
        assert_eq!(
            next(
                &Outcome::Closed {
                    code: 4113,
                    close_error: None,
                    up_for: Duration::ZERO
                },
                &mut b
            ),
            Next::LoggedOut {
                reason: "4113 dropped by another device",
                cause: LogoutCause::RemovedOnPhone
            }
        );
        assert_eq!(
            next(&Outcome::TimedOut, &mut Backoff::default()),
            Next::After(Backoff::STEP)
        );
    }

    /// R2, arm by arm, as literals: what is a failed run of the row whose frames stood unanswered.
    #[test]
    fn a_missing_ack_or_a_servers_close_is_a_failed_run_and_a_lost_network_is_not() {
        let up_for = Duration::from_secs(1);
        let closed = |code| Outcome::Closed {
            code,
            close_error: None,
            up_for,
        };
        for failed in [
            Outcome::NoAck { up_for },
            closed(1000),
            closed(1001),
            closed(1005),
            closed(1011),
            closed(4000),
            closed(4010),
            closed(4112),
            Outcome::Closed {
                code: 4000,
                close_error: Some(CloseError {
                    can_reconnect: false,
                }),
                up_for,
            },
        ] {
            assert!(failed.is_failed_run(), "{failed:?}");
        }
        for not in [
            Outcome::Stopped,
            Outcome::NotReached(NetError::Failed("x")),
            Outcome::TimedOut,
            Outcome::Stale,
            Outcome::Dead { up_for },
            closed(1006),
            Outcome::Protocol {
                unsupported: false,
                up_for,
            },
            Outcome::ClockSkew { skew_ms: 1 },
            Outcome::HeldFull,
            Outcome::NoProgress,
            Outcome::Store { up_for },
            Outcome::JoinRefused,
            Outcome::Dropped,
            Outcome::Unasked { up_for },
            Outcome::Yield,
        ] {
            assert!(!not.is_failed_run(), "{not:?}");
        }
        // RP-b: a dropped device ends the task like a stop; nothing reconnects.
        assert_eq!(
            next(&Outcome::Dropped, &mut Backoff::default()),
            Next::Stopped
        );
    }

    /// `NoAck` goes through the backoff like any socket that was up: after a minute up the count starts again, so cycles
    /// of it never hold the account by themselves; under a minute it counts.
    #[test]
    fn a_missing_ack_goes_through_the_backoff() {
        let mut b = Backoff::default();
        let cycle = Outcome::NoAck {
            up_for: Duration::from_secs(60),
        };
        for _ in 0..30 {
            assert_eq!(next(&cycle, &mut b), Next::After(Duration::from_secs(5)));
        }
        let short = Outcome::NoAck {
            up_for: Duration::from_secs(1),
        };
        assert_eq!(next(&short, &mut b), Next::After(Duration::from_secs(10)));
    }

    /// TH6 C1: an unasked chat server message goes through the backoff as a lost network does: counted under a minute up
    /// (5 s, then 10 s), and after a minute up the count starts again.
    #[test]
    fn an_unasked_message_goes_through_the_backoff_like_a_dead_socket() {
        let mut b = Backoff::default();
        let mut dead = Backoff::default();
        let short = Duration::from_secs(1);
        for want in [5, 10] {
            assert_eq!(
                next(&Outcome::Unasked { up_for: short }, &mut b),
                Next::After(Duration::from_secs(want))
            );
            assert_eq!(
                next(&Outcome::Dead { up_for: short }, &mut dead),
                Next::After(Duration::from_secs(want))
            );
        }
        let long = Outcome::Unasked {
            up_for: Duration::from_secs(60),
        };
        assert_eq!(next(&long, &mut b), Next::After(Duration::from_secs(5)));
    }

    /// TH6 L6 (critic T-H1): a socket that stepped aside waits two minutes, every time, and does not move the count of
    /// failures.
    #[test]
    fn a_step_aside_waits_two_minutes_and_does_not_count() {
        let mut b = Backoff::default();
        for _ in 0..20 {
            assert_eq!(
                next(&Outcome::Yield, &mut b),
                Next::After(Duration::from_secs(120))
            );
        }
        assert_eq!(
            next(&Outcome::TimedOut, &mut b),
            Next::After(Backoff::STEP),
            "a step-aside moved the count of failures"
        );
    }

    /// S10: a socket that ended over step 0's bound reconnects after ONE backoff step at most (the
    /// connection also connects as soon as the file is ready), never at once in a tight loop, and the
    /// count of failures does not move; a stale socket still reconnects at once.
    #[test]
    fn a_held_full_socket_waits_one_backoff_step_and_does_not_count() {
        let mut b = Backoff::default();
        for _ in 0..20 {
            assert_eq!(
                next(&Outcome::HeldFull, &mut b),
                Next::After(Duration::from_secs(5)),
                "the wait of a socket that overflowed step 0"
            );
        }
        assert_eq!(
            next(&Outcome::TimedOut, &mut b),
            Next::After(Backoff::STEP),
            "a held-full socket moved the count of failures"
        );
        assert_eq!(
            next(&Outcome::Stale, &mut Backoff::default()),
            Next::After(Duration::ZERO)
        );
    }

    /// P-L6: a recycle for room with nothing acked on the socket waits ONE backoff step, as a held-full one does, and
    /// the count of failures does not move; with an ack on the socket the recycle stays at once (`Outcome::Stale`).
    #[test]
    fn a_recycle_with_nothing_acked_waits_one_backoff_step_and_does_not_count() {
        let mut b = Backoff::default();
        for _ in 0..20 {
            assert_eq!(
                next(&Outcome::NoProgress, &mut b),
                Next::After(Backoff::STEP),
                "the wait of a socket that recycled with nothing acked"
            );
        }
        assert_eq!(
            next(&Outcome::TimedOut, &mut b),
            Next::After(Backoff::STEP),
            "a no-progress recycle moved the count of failures"
        );
        assert_eq!(
            next(&Outcome::Stale, &mut Backoff::default()),
            Next::After(Duration::ZERO)
        );
    }
}
