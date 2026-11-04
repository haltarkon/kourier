//! What a [`WaSignal`] means for the account (W2, chunk C1c): the connection state with the typed
//! facts behind it, and the reach-out lock. One table (decision L5), one place.
//!
//! The states are the account contract's (K-A): a ban is `ConnState::Restricted { kind:
//! Restriction::Banned(BanReason), note, .. }`, a logout `ConnState::LoggedOut { cause:
//! LogoutCause, note, .. }`, the `note` ([`NetworkNote`]) carries WhatsApp's own sentence and its
//! page for the person, and the reach-out lock is the contract's [`AccountHold`]. What stays
//! internal is the translate layer's vocabulary ([`WaBan`], [`LogoutShape`]) and [`WaState`]'s
//! `terminal` flag (the client stopped for good), which the contract has no place for.
//!
//! The table (rows of the brief's 3.5 with the critic's changes; the "source" column is where the
//! guess comes from, a wrong guess changes a sentence, never what happens):
//!
//! | signal | account shows | note |
//! |---|---|---|
//! | `Connected` | `Syncing` | the catch-up machine (`catchup.rs`, W3a) owns `Online`: it shows it once the drain is durable and the core has it |
//! | `Disconnected`, `WillRetry` | `Offline` | upstream's backoff is not exposed: no retry time |
//! | `Banned` | `Restricted` until the ban ends, kind `Banned(reason)` (below), **terminal** | a new client after the person asks (Q1 answered: wait for Reconnect) |
//! | `Outdated` (405) | `NeedsUserAction(UpdateApp)`, terminal | the one typed "update" signal |
//! | `Replaced` (conflict replaced, 409) | `NeedsUserAction(Reconnect)`, terminal | another session took over |
//! | `Refused` (400, 409, 413 ...) | `NeedsUserAction(Reconnect)`, terminal | upstream stopped for good; looping against a server that refused is how bans start |
//! | `LoggedOut`, device removed or 516 | `LoggedOut`, cause `RemovedOnPhone` | upstream's `conflict device_removed` (whatsmeow `connectionevents.go:40`) |
//! | `LoggedOut`, 401 | cause `Unknown` | removed while offline, or unused for too long: not told apart |
//! | `LoggedOut`, 403 | `AccountGone`, or `Unofficial` when the lock's `violation_reason` says so | upstream: "locked server-side"; whatsmeow #561 and the research note say "unofficial app" |
//! | `LoggedOut`, 406 | `AccountGone` | whatsmeow "BANNED" |
//! | `LocalLogout` | nothing | a logout Kourier asked for is not an event (critic H8) |
//!
//! The ban codes (`TempBanReason`) map to `BanReason`: 101 `TooManyStrangers`, 102 `ManyBlocked`, 103
//! `TooManyGroups`, 104 `SameMessageToMany`, 106 `BroadcastList`, any other code `Other(code)`.
//!
//! `PhoneInactive` has no upstream signal known today: this file never produces it (evidence from
//! the first real run may add it).
//!
//! The note: a ban carries WhatsApp's sentence (`message`) and its page (`url`, `https` only); a
//! logout carries the sentence (header and subtext) and no page, because upstream's logout has none.
//! Both were cleaned in `translate.rs` (plain text, one line, bounded; a link that is not a bounded
//! `https://` address is dropped there), and what it did not keep is [`NetworkNote::default`].
//!
//! Every `reason` string is log-safe: a code or a fixed phrase, never the server's text and never
//! a number that names anybody. The `note` is the one foreign text and is never logged:
//! `NetworkNote`'s `Debug` says only which parts are there.

use kourier_model::{
    AccountHold, BanReason, ConnState, HoldReason, LogoutCause, NetworkNote, Restriction,
    UserAction,
};

use crate::translate::{Banned, HoldState, Logout, LogoutShape, RetryWhy, WaBan, WaSignal};

/// What the account shows after a signal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WaState {
    /// The connection state, with a ban's kind and a logout's cause and the network's note.
    pub conn: ConnState,
    /// The client stopped for good: a new client is needed to try again, and only after the person
    /// asked for it (critic M3). System hints never revive it.
    pub terminal: bool,
}

impl WaState {
    pub(crate) fn plain(conn: ConnState) -> Self {
        Self {
            conn,
            terminal: false,
        }
    }

    fn stopped(action: UserAction) -> Self {
        Self {
            terminal: true,
            ..Self::plain(ConnState::NeedsUserAction(action))
        }
    }
}

/// The state a signal brings, or `None` when it does not change the connection (the login
/// signals, the reach-out lock, a logout Kourier asked for itself).
pub(crate) fn state_of(signal: &WaSignal) -> Option<WaState> {
    Some(match signal {
        WaSignal::Connected => WaState::plain(ConnState::Syncing { fraction: None }),
        WaSignal::Disconnected { .. } | WaSignal::WillRetry(_) => {
            WaState::plain(ConnState::Offline { retry_at_ms: None })
        }
        WaSignal::Outdated => WaState::stopped(UserAction::UpdateApp),
        WaSignal::Replaced | WaSignal::Refused { .. } => WaState::stopped(UserAction::Reconnect),
        WaSignal::Banned(b) => restricted(b),
        WaSignal::LoggedOut(l) => logged_out(l),
        _ => return None,
    })
}

fn restricted(b: &Banned) -> WaState {
    WaState {
        conn: ConnState::Restricted {
            until_ms: Some(b.until_ms),
            reason: format!("temporary ban {}", b.ban.code()),
            kind: Restriction::Banned(ban_reason(b.ban)),
            // WhatsApp's own sentence and page, as `translate.rs` kept them.
            note: NetworkNote {
                explanation: b.explanation.clone(),
                appeal_url: b.appeal_url.clone(),
            },
        },
        terminal: true,
    }
}

fn logged_out(l: &Logout) -> WaState {
    WaState {
        conn: ConnState::LoggedOut {
            reason: logout_reason(l.shape).to_owned(),
            cause: logout_cause(l),
            // Upstream's logout has a header and a subtext and no page.
            note: NetworkNote {
                explanation: l.explanation.clone(),
                appeal_url: None,
            },
        },
        terminal: true,
    }
}

/// The ban a wire code stands for (the codes are in the module doc).
fn ban_reason(ban: WaBan) -> BanReason {
    match ban {
        WaBan::TooManyStrangers => BanReason::TooManyStrangers,
        WaBan::ManyBlocked => BanReason::ManyBlocked,
        WaBan::TooManyGroups => BanReason::TooManyGroups,
        WaBan::SameMessageToMany => BanReason::SameMessageToMany,
        WaBan::BroadcastList => BanReason::BroadcastList,
        WaBan::Other(code) => BanReason::Other(code),
    }
}

/// The fixed, log-safe phrase for the `reason` of a logout.
fn logout_reason(shape: LogoutShape) -> &'static str {
    match shape {
        LogoutShape::RemovedByPhone => "device removed",
        LogoutShape::Unauthorized => "unauthorized (401)",
        LogoutShape::Locked => "account locked (403)",
        LogoutShape::Refused406 => "account refused (406)",
        LogoutShape::Other => "logged out",
    }
}

/// The cause of a logout (decision L5; the table in the module doc).
pub(crate) fn logout_cause(l: &Logout) -> LogoutCause {
    match l.shape {
        LogoutShape::RemovedByPhone => LogoutCause::RemovedOnPhone,
        LogoutShape::Unauthorized | LogoutShape::Other => LogoutCause::Unknown,
        LogoutShape::Locked => {
            // `violation_reason` is not a closed set; "other_harm" is the only value seen. A lock
            // that names an unofficial client is the one cause WhatsApp states about Kourier's kind
            // of program, so it gets its own sentence.
            if l.violation
                .as_deref()
                .is_some_and(|v| v.contains("unofficial"))
            {
                LogoutCause::Unofficial
            } else {
                LogoutCause::AccountGone
            }
        }
        LogoutShape::Refused406 => LogoutCause::AccountGone,
    }
}

/// What the supervision logs for a retry (a fixed phrase; the rate limit is the one that matters).
pub(crate) fn retry_note(why: RetryWhy) -> &'static str {
    match why {
        RetryWhy::ServerError => "server error, retrying",
        RetryWhy::Unavailable => "server unavailable, retrying",
        RetryWhy::RateLimited => "rate limited, retrying",
        RetryWhy::Unreachable => "could not connect, retrying",
    }
}

/// The reach-out lock a PUSH states (`ReachoutTimelockUpdate`): `Some(Some(hold))` for a lock,
/// `Some(None)` for a lifted one, and `None` when the update asserts nothing, so nothing is told.
pub(crate) fn hold_push(state: &HoldState) -> Option<Option<AccountHold>> {
    match state {
        HoldState::Active { until_ms, .. } => Some(Some(AccountHold {
            until_ms: *until_ms,
            reason: HoldReason::ReachOutLocked,
        })),
        HoldState::Lifted => Some(None),
        HoldState::Unasserted => None,
    }
}

/// The reach-out lock the PULL after a connect found: exactly one statement at every connect
/// (critic M1), `None` included. Only an active lock is a hold; a lifted or unasserted answer is
/// "no hold", so a hold without an end cannot stand for ever.
pub(crate) fn hold_pull(state: &HoldState) -> Option<AccountHold> {
    hold_push(state).flatten()
}

#[cfg(test)]
mod tests;
