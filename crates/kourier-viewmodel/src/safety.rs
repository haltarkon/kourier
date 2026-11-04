//! The safety-number level of the details panel (P5a; architecture §8.3): what "Verify Safety
//! Number" shows of one person, built from the backend's [`SafetyAnswer`]. Qt-free: the digit
//! groups, the state line, the buttons and every text live here; the front end draws them and
//! sends a click back by its [`SafetyAction`]. The KEY a click acts on stays in Rust
//! ([`SafetyView::key`]): the front end never round-trips it through its own text.

pub use kourier_model::VerifyState;
use kourier_model::{AccountId, IdentityFingerprint, SafetyAnswer, SafetyUnavailable, UserId};

use crate::i18n::{trc, trfc};

/// How many digits one group of the number holds (Signal: 12 groups of 5).
pub const GROUP_DIGITS: usize = 5;

/// What a button of the level does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SafetyAction {
    /// "Mark as Verified": `UiCommand::SetVerified { verified: true }` with the shown key.
    Mark,
    /// "Clear Verification": `UiCommand::SetVerified { verified: false }` with the shown key.
    Clear,
    /// "Confirm New Safety Number": `UiCommand::ConfirmIdentity` with the shown key (BQ-2b;
    /// never a Clear: a Clear always tells the other devices).
    Confirm,
}

impl SafetyAction {
    /// The name a front end passes back for the button (`mark`, `clear`, `confirm`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Mark => "mark",
            Self::Clear => "clear",
            Self::Confirm => "confirm",
        }
    }

    /// The action named `name` ([`SafetyAction::name`]); `None` for any other text.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Mark, Self::Clear, Self::Confirm]
            .into_iter()
            .find(|a| a.name() == name)
    }
}

/// One button of the level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafetyButton {
    /// What it does.
    pub action: SafetyAction,
    /// Its label.
    pub label: String,
    /// The primary one (drawn as such).
    pub primary: bool,
}

/// What the safety-number level shows ([`crate::source::UiSource::take_safety`]).
#[derive(Clone, PartialEq, Eq)]
pub struct SafetyView {
    /// The account the person is seen through.
    pub account: AccountId,
    /// The person.
    pub user: UserId,
    /// The page title.
    pub title: String,
    /// The pane's accessible name ("Safety number with Ada").
    pub accessible_title: String,
    /// The 12 groups of 5 digits, in order; empty when there is no number.
    pub groups: Vec<String>,
    /// What a screen reader reads for the digits ("Safety number: 30035 44776 …"); empty
    /// without a number.
    pub accessible_digits: String,
    /// The bytes the QR code carries (Signal: the `ScannableFingerprint` protobuf); empty
    /// without a number. Handed to the front end as BYTES, never as a string (critic M6).
    pub scannable: Vec<u8>,
    /// The person's key the number was computed from: a click acts on this key only (the
    /// backend refuses another). `None` without a number.
    pub key: Option<IdentityFingerprint>,
    /// The stored state; `None` without a number.
    pub state: Option<VerifyState>,
    /// The hint under the number; empty without one.
    pub hint: String,
    /// The state line ("Verified", "Your safety number with Ada changed since you verified
    /// it."); empty when there is none.
    pub state_line: String,
    /// The buttons, the primary one first; none without a number.
    pub buttons: Vec<SafetyButton>,
    /// Why there is no number (one sentence); empty when there is one.
    pub reason: String,
    /// The line under the buttons ([`refused_text`], [`not_saved_text`], [`swapped_text`]); it stays
    /// until the next click or the close of the level; empty otherwise.
    pub notice: String,
}

crate::safe_debug!(SafetyView { account, state });

/// The level of `user`, named `name`, from the backend's `answer`. `paused`: a send to them
/// waits for a confirm (BQ-3, critic F2): a person in state Default then also gets "Confirm New
/// Safety Number" (it releases the send without verifying anyone).
pub fn safety_view(
    account: AccountId,
    user: UserId,
    name: &str,
    answer: &SafetyAnswer,
    paused: bool,
) -> SafetyView {
    let title = trc("@title", "Verify Safety Number").into_owned();
    let accessible_title = trfc(
        "@info accessible the details panel at a person's safety number, %1 the person",
        "Safety number with %1",
        &[name],
    );
    let mut view = SafetyView {
        account,
        user,
        title,
        accessible_title,
        groups: Vec::new(),
        accessible_digits: String::new(),
        scannable: Vec::new(),
        key: None,
        state: None,
        hint: String::new(),
        state_line: String::new(),
        buttons: Vec::new(),
        reason: String::new(),
        notice: String::new(),
    };
    let n = match answer {
        Ok(n) => n,
        Err(why) => {
            view.reason = reason_text(*why);
            return view;
        }
    };
    view.groups = groups(n.digits.expose());
    view.accessible_digits = trfc(
        "@info accessible the digits of a safety number, %1 the groups of digits",
        "Safety number: %1",
        &[&view.groups.join(" ")],
    );
    view.scannable = n.scannable.expose().clone();
    view.key = Some(n.key.clone());
    view.state = Some(n.state);
    view.hint = trfc(
        "@info",
        "To verify end-to-end encryption with %1, compare the numbers above with their device. They can also scan your code on their device.",
        &[name],
    );
    let button = |action: SafetyAction, primary: bool| SafetyButton {
        action,
        label: match action {
            SafetyAction::Mark => trc("@action:button", "Mark as Verified"),
            SafetyAction::Clear => trc("@action:button", "Clear Verification"),
            SafetyAction::Confirm => trc("@action:button", "Confirm New Safety Number"),
        }
        .into_owned(),
        primary,
    };
    match n.state {
        VerifyState::Verified => {
            view.state_line = verified_text();
            view.buttons = vec![button(SafetyAction::Clear, true)];
        }
        VerifyState::Changed => {
            view.state_line = trfc(
                "@info:status",
                "Your safety number with %1 changed since you verified it.",
                &[name],
            );
            view.buttons = vec![
                button(SafetyAction::Confirm, true),
                button(SafetyAction::Mark, false),
            ];
        }
        VerifyState::Default if paused => {
            view.state_line = changed_text();
            view.buttons = vec![
                button(SafetyAction::Confirm, true),
                button(SafetyAction::Mark, false),
            ];
        }
        VerifyState::Default => {
            view.buttons = vec![button(SafetyAction::Mark, true)];
        }
    }
    view
}

/// The 60 digits as groups of [`GROUP_DIGITS`] (any other length is cut the same way).
pub fn groups(digits: &str) -> Vec<String> {
    let chars: Vec<char> = digits.chars().filter(char::is_ascii_digit).collect();
    chars
        .chunks(GROUP_DIGITS)
        .map(|c| c.iter().collect())
        .collect()
}

/// The sentence of a person without a number.
pub fn reason_text(why: SafetyUnavailable) -> String {
    match why {
        SafetyUnavailable::NoKey => trc(
            "@info",
            "A safety number will be created with this person after you exchange messages with them.",
        ),
        SafetyUnavailable::NotRunning => trc(
            "@info",
            "The safety number can be shown once this account is connected.",
        ),
        // The row is not offered for the own page or a network without numbers; a level that
        // still gets such an answer says it cannot be verified.
        SafetyUnavailable::NoAccountId | SafetyUnavailable::OwnId | SafetyUnavailable::Unsupported => {
            trc(
                "@info",
                "This person cannot be verified yet: Kourier does not know their Signal account.",
            )
        }
    }
    .into_owned()
}

/// The line under the buttons after the backend REFUSED a Mark, Clear or Confirm because the
/// number changed after it was shown (the level asks again and shows the new one). A Mark and
/// a Clear share one text; a Confirm has its own (U1: it must not say "mark").
pub fn refused_text(action: SafetyAction) -> String {
    match action {
        SafetyAction::Confirm => trc(
            "@info:status a refused confirm",
            "The safety number changed. Look at the new one before you confirm it.",
        ),
        SafetyAction::Mark | SafetyAction::Clear => trc(
            "@info:status a refused mark",
            "The safety number changed. Look at the new one before you mark it.",
        ),
    }
    .into_owned()
}

/// The line under the buttons when a Mark, Clear or Confirm failed for any other reason than
/// the refusal of a stale key (the account is not running, the store failed): the change was
/// not made, and nothing is said about the number (U1: a false "the number changed" on this
/// page would teach the user to ignore the real one).
pub fn not_saved_text() -> String {
    trc("@info:status", "The change could not be saved.").into_owned()
}

/// The line kept under the buttons when the number of an OPEN level turned into another one
/// without a click (the person's key changed while it was on screen, U2): the buttons act on
/// the new number, which nobody has compared yet.
pub fn swapped_text() -> String {
    refused_text(SafetyAction::Mark)
}

/// The quiet state line of a verified person (under the row, and on the level).
pub fn verified_text() -> String {
    trc("@info:status under the button, state", "Verified").into_owned()
}

/// The quiet state line of a person whose number changed (under the row).
pub fn changed_text() -> String {
    trc(
        "@info:status under the button, state",
        "Safety number changed",
    )
    .into_owned()
}

#[cfg(test)]
mod tests {
    use kourier_model::{Redacted, SafetyNumber};

    use super::*;

    fn number(state: VerifyState) -> SafetyAnswer {
        Ok(SafetyNumber {
            digits: Redacted::new(
                "300354477612345678901234567890123456789012345678901234567890".into(),
            ),
            scannable: Redacted::new(vec![0x08, 0x02, 0xff]),
            key: IdentityFingerprint(vec![5; 33]),
            state,
        })
    }

    fn view(answer: &SafetyAnswer, paused: bool) -> SafetyView {
        safety_view(
            AccountId(1),
            UserId::parse("aci:a").expect("id"),
            "Ada",
            answer,
            paused,
        )
    }

    fn actions(v: &SafetyView) -> Vec<(SafetyAction, bool)> {
        v.buttons.iter().map(|b| (b.action, b.primary)).collect()
    }

    /// The number is shown as 12 groups of 5 digits, in order, and read as one sentence.
    #[test]
    fn safety_view_groups() {
        let v = view(&number(VerifyState::Default), false);
        assert_eq!(v.groups.len(), 12);
        assert!(v.groups.iter().all(|g| g.len() == 5));
        assert_eq!(v.groups[0], "30035");
        assert_eq!(v.groups[1], "44776");
        assert_eq!(v.groups.concat().len(), 60);
        assert!(
            v.accessible_digits.ends_with(
                "30035 44776 12345 67890 12345 67890 12345 67890 12345 67890 12345 67890"
            )
        );
        assert_eq!(v.scannable, vec![0x08, 0x02, 0xff], "the bytes as they are");
        assert!(
            v.key == Some(IdentityFingerprint(vec![5; 33])),
            "the key of the answer"
        );
    }

    /// Each state words its own line and offers its own buttons; Changed is never worded as
    /// Default, and Confirm is a button of its own (never a Clear).
    #[test]
    fn safety_view_texts_per_state() {
        let d = view(&number(VerifyState::Default), false);
        assert_eq!(actions(&d), vec![(SafetyAction::Mark, true)]);
        assert!(d.state_line.is_empty());
        assert!(d.hint.contains("Ada"));
        let v = view(&number(VerifyState::Verified), false);
        assert_eq!(actions(&v), vec![(SafetyAction::Clear, true)]);
        assert_eq!(v.state_line, "Verified");
        let c = view(&number(VerifyState::Changed), false);
        assert_eq!(
            actions(&c),
            vec![(SafetyAction::Confirm, true), (SafetyAction::Mark, false)]
        );
        assert_eq!(
            c.state_line,
            "Your safety number with Ada changed since you verified it."
        );
        assert_eq!(c.buttons[0].label, "Confirm New Safety Number");
        // A Default person whose send waits (BQ-3, F2) can release it.
        let p = view(&number(VerifyState::Default), true);
        assert_eq!(
            actions(&p),
            vec![(SafetyAction::Confirm, true), (SafetyAction::Mark, false)]
        );
        assert_eq!(p.state_line, "Safety number changed");
        // A paused Verified person keeps Clear only (a verified key does not pause).
        assert_eq!(
            actions(&view(&number(VerifyState::Verified), true)),
            vec![(SafetyAction::Clear, true)]
        );
    }

    /// Without a number: the reason only, no digits, no QR, no key, no button.
    #[test]
    fn safety_view_without_a_number() {
        for (why, start) in [
            (SafetyUnavailable::NoKey, "A safety number will be created"),
            (
                SafetyUnavailable::NotRunning,
                "The safety number can be shown once",
            ),
            (
                SafetyUnavailable::NoAccountId,
                "This person cannot be verified yet",
            ),
        ] {
            let v = view(&Err(why), false);
            assert!(v.reason.starts_with(start), "{why:?}");
            assert!(v.groups.is_empty() && v.scannable.is_empty() && v.key.is_none());
            assert!(v.buttons.is_empty() && v.hint.is_empty() && v.state.is_none());
        }
        assert_eq!(
            view(&Err(SafetyUnavailable::NoKey), false).title,
            "Verify Safety Number"
        );
        assert_eq!(
            view(&Err(SafetyUnavailable::NoKey), false).accessible_title,
            "Safety number with Ada"
        );
    }
}
