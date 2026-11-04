//! What TDLib's connection and freeze updates mean for the account (TG-2 brief §3.2, §3.3; critic
//! M4, M7). One table, one place; the session (C8) applies it.
//!
//! | TDLib | account shows |
//! |---|---|
//! | `connectionStateWaitingForNetwork` | `Offline` (no retry time: TDLib's own back-off is not exposed) |
//! | `connectionStateConnectingToProxy`, `connectionStateConnecting` | `Connecting` (behind the gate the proxy step is the normal one) |
//! | `connectionStateUpdating` | `Syncing` (TDLib downloads what came while the app was offline) |
//! | `connectionStateReady` | `Online` (T3/TD-3 later hold it until the reconcile is durable) |
//! | an unknown connection state | nothing (the last state stays) |
//! | `updateFreezeState{is_frozen: true}` | `AccountHold(Some(Frozen, no end))`: receiving goes on, sending waits (his U-5 answer) |
//! | `updateFreezeState{is_frozen: false}` | `AccountHold(None)` |
//! | TDLib's OWN logout after Ready, a resumed account TDLib no longer knows | `LoggedOut{cause: Unknown}` and `Exit::LoggedOut`, never a `BackendEvent::Logout` (that one answers the core's `Logout` only, M7) |
//!
//! The hold rule (networks.md, M4): a backend that can hold states `AccountHold` (Some or None) at
//! EVERY connect before its first `Syncing` or `Online`. TDLib sends `updateFreezeState` only when
//! a freeze is known, so [`Holds`] states `None` then unless a freeze was seen; a freeze seen once
//! is stated again at every later connect until TDLib lifts it. TDLib's freezing and deletion dates
//! are not a hold's end (the deletion date would LIFT the hold then): `until_ms` is `None`. The
//! appeal link is not kept (the appeal is made in the Telegram app).
//!
//! Every `reason` is a fixed phrase.

use kourier_model::{AccountHold, ConnState, HoldReason, LogoutCause, NetworkNote};

use crate::tdapi::{ConnectionState, FreezeState};

/// The reason of a logout TDLib did itself after Ready (L5: not after Kourier's own `logOut`).
pub(crate) const TELEGRAM_ENDED: &str = "Telegram ended this session";
/// The reason of a resumed account whose TDLib is not logged in (its `tdlib/` lost or reset, or a
/// login that never finished: L-8).
pub(crate) const SESSION_GONE: &str = "Telegram's session of this account is gone";

/// The account's state for a connection state; `None` keeps the last one.
pub(crate) fn conn_of(state: ConnectionState) -> Option<ConnState> {
    match state {
        ConnectionState::WaitingForNetwork => Some(ConnState::Offline { retry_at_ms: None }),
        ConnectionState::ConnectingToProxy | ConnectionState::Connecting => {
            Some(ConnState::Connecting)
        }
        ConnectionState::Updating => Some(ConnState::Syncing { fraction: None }),
        ConnectionState::Ready => Some(ConnState::Online { detail: None }),
        ConnectionState::Unknown => None,
    }
}

/// The hold of a freeze update: `Some` while frozen, `None` once lifted.
pub(crate) fn hold_of(freeze: &FreezeState) -> Option<AccountHold> {
    freeze.is_frozen.then_some(AccountHold {
        until_ms: None,
        reason: HoldReason::Frozen,
    })
}

/// A logout with a fixed reason (M7: the state and `Exit::LoggedOut`, never a `Logout` event).
pub(crate) fn logged_out(reason: &'static str) -> ConnState {
    ConnState::LoggedOut {
        reason: reason.to_owned(),
        cause: LogoutCause::Unknown,
        note: NetworkNote::default(),
    }
}

/// The hold rule of one account across its connects (M4). Each method returns the
/// `AccountHold` event to send now, if any (`Some(None)` = lift / no hold).
#[derive(Debug, Default)]
pub(crate) struct Holds {
    /// The last freeze TDLib said.
    hold: Option<AccountHold>,
    /// This connect already stated its hold.
    stated: bool,
}

impl Holds {
    /// A helper (re)start: the next `Syncing`/`Online` states the hold again.
    pub(crate) fn on_connect(&mut self) {
        self.stated = false;
    }

    /// A freeze update: stated at once (it also counts as this connect's statement).
    pub(crate) fn on_freeze(&mut self, freeze: &FreezeState) -> Option<Option<AccountHold>> {
        self.hold = hold_of(freeze);
        self.stated = true;
        Some(self.hold)
    }

    /// Before `next` is shown: the hold, once per connect, before the first `Syncing`/`Online`.
    pub(crate) fn before(&mut self, next: &ConnState) -> Option<Option<AccountHold>> {
        let connected = matches!(next, ConnState::Syncing { .. } | ConnState::Online { .. });
        if connected && !self.stated {
            self.stated = true;
            Some(self.hold)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn freeze(is_frozen: bool) -> FreezeState {
        FreezeState {
            is_frozen,
            freezing_date: 1_700_000_000,
            deletion_date: 1_800_000_000,
            appeal_link: "https://example.invalid/appeal".to_owned(),
        }
    }

    /// §3.3's table, row by row.
    #[test]
    fn connection_states_map_as_the_table() {
        assert_eq!(
            conn_of(ConnectionState::WaitingForNetwork),
            Some(ConnState::Offline { retry_at_ms: None })
        );
        assert_eq!(
            conn_of(ConnectionState::ConnectingToProxy),
            Some(ConnState::Connecting)
        );
        assert_eq!(
            conn_of(ConnectionState::Connecting),
            Some(ConnState::Connecting)
        );
        assert_eq!(
            conn_of(ConnectionState::Updating),
            Some(ConnState::Syncing { fraction: None })
        );
        assert_eq!(
            conn_of(ConnectionState::Ready),
            Some(ConnState::Online { detail: None })
        );
        assert_eq!(conn_of(ConnectionState::Unknown), None);
    }

    /// A freeze is a Frozen hold with no end (the dates are not its end, the link is not kept);
    /// its lift is `None`.
    #[test]
    fn a_freeze_is_a_hold_and_its_end_lifts_it() {
        assert_eq!(
            hold_of(&freeze(true)),
            Some(AccountHold {
                until_ms: None,
                reason: HoldReason::Frozen,
            })
        );
        assert_eq!(hold_of(&freeze(false)), None);
        assert!(matches!(
            logged_out(TELEGRAM_ENDED),
            ConnState::LoggedOut {
                cause: LogoutCause::Unknown,
                ..
            }
        ));
    }

    /// M4: every connect states the hold once before its first Syncing/Online: `None` when no
    /// freeze was seen, the freeze when one was (also after a restart), and a freeze update is
    /// stated at once.
    #[test]
    fn the_hold_is_stated_before_the_first_online_of_every_connect() {
        let mut holds = Holds::default();
        holds.on_connect();
        assert_eq!(holds.before(&ConnState::Connecting), None);
        assert_eq!(
            holds.before(&ConnState::Online { detail: None }),
            Some(None)
        );
        assert_eq!(
            holds.before(&ConnState::Online { detail: None }),
            None,
            "once"
        );

        let frozen = Some(AccountHold {
            until_ms: None,
            reason: HoldReason::Frozen,
        });
        assert_eq!(holds.on_freeze(&freeze(true)), Some(frozen));
        holds.on_connect();
        assert_eq!(
            holds.before(&ConnState::Syncing { fraction: None }),
            Some(frozen),
            "a restart states the freeze again"
        );
        // A freeze that comes before the first Online of a connect is that connect's statement.
        holds.on_connect();
        assert_eq!(holds.on_freeze(&freeze(false)), Some(None));
        assert_eq!(holds.before(&ConnState::Online { detail: None }), None);
    }
}
