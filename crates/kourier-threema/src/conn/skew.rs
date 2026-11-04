//! The clock rule (brief §2.7, decision C2): the mediator's `ServerInfo` and the chat server's
//! login-ack each carry the server's time (patch P10); the skew is the ACTOR'S clock minus it
//! (positive: the local clock is ahead, the contract's `UserAction::FixClock`). More than 20 minutes
//! either way (`md-d2m.proto`: "more than 20 minutes") closes the connection and asks the user to fix
//! the clock; exactly 20 minutes is fine.
//!
//! "Connect Anyway, until restart" (his Q4, decision C2): a PROCESS-WIDE flag per account uuid that
//! holds until the Kourier process ends, not until the actor ends (the core restarts a failed
//! actor). A plain `Connect` never sets it: it measures again. NOTHING in TH3 sets it but the
//! crate's tests ([`crate::connect_anyway`]); KH1f wires the clock warning's button to it.

use std::collections::BTreeSet;
use std::sync::{Mutex, PoisonError};

use kourier_model::AccountUuid;

/// The most the two clocks may differ (ms).
pub(crate) const MAX_SKEW_MS: u64 = 20 * 60 * 1_000;

/// The accounts whose clock warning was answered with "Connect Anyway" in this process.
static ANYWAY: Mutex<BTreeSet<AccountUuid>> = Mutex::new(BTreeSet::new());

/// The skew of the local clock against the server's time (ms, positive: local ahead).
pub(crate) fn skew_ms(local_ms: u64, server_ms: u64) -> i64 {
    let local = i128::from(local_ms);
    let server = i128::from(server_ms);
    i64::try_from(local - server).unwrap_or(if local > server { i64::MAX } else { i64::MIN })
}

/// Whether `skew` is more than the protocol allows.
pub(crate) fn too_far(skew: i64) -> bool {
    skew.unsigned_abs() > MAX_SKEW_MS
}

/// Sets the account's "Connect Anyway" until the process ends.
pub(crate) fn set_anyway(account: AccountUuid) {
    ANYWAY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(account);
}

/// Whether the account's clock warning was answered with "Connect Anyway" in this process.
pub(crate) fn anyway(account: AccountUuid) -> bool {
    ANYWAY
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&account)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Critic H1: literal numbers for both signs (a server 21 minutes AHEAD is a local clock 21
    /// minutes behind: negative).
    #[test]
    fn the_skew_is_the_local_clock_minus_the_servers_time() {
        assert_eq!(skew_ms(1_000_000_000_000, 1_000_001_260_000), -1_260_000);
        assert_eq!(skew_ms(1_000_001_260_000, 1_000_000_000_000), 1_260_000);
        assert_eq!(skew_ms(5, 5), 0);
        assert_eq!(skew_ms(0, u64::MAX), i64::MIN);
        assert_eq!(skew_ms(u64::MAX, 0), i64::MAX);
    }

    #[test]
    fn exactly_twenty_minutes_is_fine_one_ms_more_is_not() {
        assert!(!too_far(1_200_000));
        assert!(!too_far(-1_200_000));
        assert!(too_far(1_200_001));
        assert!(too_far(-1_200_001));
        assert!(too_far(i64::MIN));
        assert!(!too_far(0));
    }

    #[test]
    fn connect_anyway_is_per_account() {
        let a = AccountUuid::new_random();
        let b = AccountUuid::new_random();
        assert!(!anyway(a));
        set_anyway(a);
        assert!(anyway(a));
        assert!(!anyway(b));
    }
}
