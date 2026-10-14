//! "Remove Account…" and "Unlink…" of an account (Settings → "Accounts", the drawer's menu of an
//! account; link feedback L4): the labels, the question put before either happens, and what the
//! user is told once afterwards about the device on the network.
//!
//! The questions reuse [`ReplaceQuestion`] (title, the account line, paragraphs, two buttons), so
//! the one dialog of the front-end shows all three with the same guard (`armDelay`, Cancel the
//! default, never confirmed by Enter).

use kourier_model::{AccountId, DeviceLogout, LogoutReport};

use crate::i18n::{trc, trfc};
use crate::transfer::ReplaceQuestion;

/// Which of the two account actions a question is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AccountAction {
    /// "Remove Account…": the device logs out on the network and everything of the account on
    /// this computer is deleted.
    Remove,
    /// "Unlink…": the device logs out on the network and the chats stay readable here.
    Unlink,
}

/// Label of the action "Remove Account…".
pub fn remove_action_label() -> String {
    trc(
        "@action:button unlink this computer from an account and delete the account's data here",
        "Remove Account…",
    )
    .into_owned()
}

/// Label of the action "Unlink…".
pub fn unlink_action_label() -> String {
    trc(
        "@action:button unlink this computer from an account, keeping its chats readable here",
        "Unlink…",
    )
    .into_owned()
}

/// The question before "Remove Account…" of `account`, described by `about`
/// ([`crate::transfer::account_line`]). `linked`: this computer is still linked to the account;
/// a logged-out account is not (the phone has unlinked it already), so the question does not
/// say Kourier unlinks anything: it only deletes what the account has here.
pub fn remove_question(account: AccountId, about: String, linked: bool) -> ReplaceQuestion {
    ReplaceQuestion {
        account,
        title: trc(
            "@title:dialog question before an account is removed from this computer",
            "Remove Account?",
        )
        .into_owned(),
        about,
        paragraphs: vec![
            if linked {
                trc(
                    "@info question before an account is removed from this computer",
                    "Kourier unlinks this computer from your account and deletes this account's chats, messages and files from this computer.",
                )
                .into_owned()
            } else {
                trc(
                    "@info question before an account that is not linked any more is removed from this computer",
                    "Kourier deletes this account's chats, messages and files from this computer.",
                )
                .into_owned()
            },
            trc(
                "@info question before an account is removed from this computer",
                "Your account, and the messages on your phone and your other devices, stay as they are.",
            )
            .into_owned(),
            trc(
                "@info question before an account is removed from this computer",
                "To use this account here again, link it again with your phone.",
            )
            .into_owned(),
        ],
        confirm: trc(
            "@action:button remove an account from this computer",
            "Remove Account",
        )
        .into_owned(),
        cancel: trc("@action:button", "Cancel").into_owned(),
    }
}

/// The question before "Unlink…" of `account`, described by `about`.
pub fn unlink_question(account: AccountId, about: String) -> ReplaceQuestion {
    ReplaceQuestion {
        account,
        title: trc(
            "@title:dialog question before this computer is unlinked from an account",
            "Unlink This Computer?",
        )
        .into_owned(),
        about,
        paragraphs: vec![
            trc(
                "@info question before this computer is unlinked from an account",
                "Kourier unlinks this computer from your account. Your chats stay readable on this computer.",
            )
            .into_owned(),
            trc(
                "@info question before this computer is unlinked from an account",
                "New messages stop arriving here until you link the account again with your phone.",
            )
            .into_owned(),
        ],
        confirm: trc(
            "@action:button unlink this computer from an account",
            "Unlink",
        )
        .into_owned(),
        cancel: trc("@action:button", "Cancel").into_owned(),
    }
}

/// What the user is told once after "Remove Account…" or "Unlink…".
#[derive(Clone, PartialEq, Eq)]
pub struct LogoutNotice {
    /// The account it is about.
    pub account: AccountId,
    /// What happened here, one sentence (names the account: plain text, never logged).
    pub text: String,
    /// When the network could not be told: what to do on the phone (names the device);
    /// empty when nothing is left to do. A front-end keeps a notice with this text on screen
    /// until the user closes it.
    pub on_phone: String,
    /// The title of a notice that stays ([`Self::on_phone`] not empty).
    pub title: String,
}

crate::safe_debug!(LogoutNotice { account });

/// How the notice tells an account from another that looks the same.
#[derive(Clone, Debug, Default)]
pub struct LinkInfo {
    /// Another account of the profile has this account's identity or name and number.
    pub twin: bool,
    /// "device 3 · linked 12 Sep" ([`crate::transfer::device_label`]); empty when not known.
    pub label: String,
    /// The day the device was linked, as the phone lists it; empty when not known.
    pub date: String,
}

/// The notice about the device of `account` (`name`: how the account is told apart, its
/// network's name `network`) after it was removed (`removed`) or unlinked. `link` tells two
/// accounts that look the same apart, in the notice and in the instruction for the phone: the
/// phone lists a device by its name and the day it was linked, so for twins the instruction
/// names both and never says "this computer", which would be both entries.
pub fn logout_notice(
    account: AccountId,
    removed: bool,
    report: &LogoutReport,
    name: &str,
    network: &str,
    link: &LinkInfo,
) -> LogoutNotice {
    let named = if link.twin && !link.label.is_empty() {
        crate::transfer::with_device(name, &link.label)
    } else {
        name.to_owned()
    };
    let text = if removed {
        trfc(
            "@info:status after an account was removed, %1 the account's name",
            "The account “%1” was removed from this computer.",
            &[&named],
        )
    } else {
        trfc(
            "@info:status after this computer was unlinked from an account, %1 the account's name",
            "This computer was unlinked from the account “%1”. Its chats stay readable here.",
            &[&named],
        )
    };
    let device = report
        .device_name
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let on_phone = match report.outcome {
        DeviceLogout::Removed | DeviceLogout::NoDevice => String::new(),
        DeviceLogout::NotReached => match (device, link.twin && !link.date.is_empty()) {
            (Some(device), true) => trfc(
                "@info %1 the messenger (Signal), %2 the name of this computer among the phone's linked devices, %3 the date it was linked",
                "%1 could not be reached: remove “%2” linked on %3 from the linked devices on your phone.",
                &[network, device, &link.date],
            ),
            (None, true) => trfc(
                "@info %1 the messenger (Signal), %2 the date this computer was linked; the name it was linked with is not known",
                "%1 could not be reached: remove the device linked on %2 from the linked devices on your phone.",
                &[network, &link.date],
            ),
            (Some(device), false) => trfc(
                "@info %1 the messenger (Signal), %2 the name of this computer among the phone's linked devices",
                "%1 could not be reached: remove “%2” from the linked devices on your phone.",
                &[network, device],
            ),
            (None, false) => trfc(
                "@info %1 the messenger (Signal); the name this computer was linked with is not known",
                "%1 could not be reached: remove this computer from the linked devices on your phone.",
                &[network],
            ),
        },
    };
    let title = if on_phone.is_empty() {
        String::new()
    } else {
        trc(
            "@title:dialog the network could not be told that this computer left an account",
            "Remove This Computer on Your Phone",
        )
        .into_owned()
    };
    LogoutNotice {
        account,
        text,
        on_phone,
        title,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: AccountId = AccountId(7);

    fn one() -> LinkInfo {
        LinkInfo::default()
    }

    fn twin() -> LinkInfo {
        LinkInfo {
            twin: true,
            label: "device 3 · linked 2 Oct".into(),
            date: "2 Oct".into(),
        }
    }

    #[test]
    fn a_confirmed_removal_says_what_happened_and_asks_nothing_more() {
        let n = logout_notice(
            A,
            true,
            &LogoutReport::new(DeviceLogout::Removed),
            "Ann",
            "Signal",
            &one(),
        );
        // The account, not a person: "Ann was removed" would read as if Ann were.
        assert_eq!(n.text, "The account “Ann” was removed from this computer.");
        assert!(n.on_phone.is_empty() && n.title.is_empty());
        let n = logout_notice(
            A,
            true,
            &LogoutReport::new(DeviceLogout::NoDevice),
            "Ann",
            "Signal",
            &one(),
        );
        assert!(n.on_phone.is_empty());
    }

    #[test]
    fn a_removal_the_server_did_not_confirm_says_to_remove_the_device_on_the_phone() {
        let report = LogoutReport {
            outcome: DeviceLogout::NotReached,
            device_name: Some("desk".into()),
        };
        let n = logout_notice(A, true, &report, "Ann", "Signal", &one());
        assert_eq!(
            n.on_phone,
            "Signal could not be reached: remove “desk” from the linked devices on your phone."
        );
        assert_eq!(n.title, "Remove This Computer on Your Phone");
        // A name the backend does not know: the second text, without one.
        let n = logout_notice(
            A,
            false,
            &LogoutReport::new(DeviceLogout::NotReached),
            "Ann",
            "Signal",
            &one(),
        );
        assert_eq!(
            n.on_phone,
            "Signal could not be reached: remove this computer from the linked devices on your phone."
        );
        assert_eq!(
            n.text,
            "This computer was unlinked from the account “Ann”. Its chats stay readable here."
        );
    }

    /// Two entries on the phone are this computer: the instruction names the one to remove by
    /// what the phone lists (its name and the day it was linked), never "this computer".
    #[test]
    fn the_instruction_for_two_links_of_one_account_names_the_device_and_the_day() {
        let report = LogoutReport {
            outcome: DeviceLogout::NotReached,
            device_name: Some("desk".into()),
        };
        let n = logout_notice(A, true, &report, "Ann", "Signal", &twin());
        assert_eq!(
            n.on_phone,
            "Signal could not be reached: remove “desk” linked on 2 Oct from the linked devices on your phone."
        );
        assert_eq!(
            n.text,
            "The account “Ann · device 3 · linked 2 Oct” was removed from this computer."
        );
        // A link from before the name was kept: the day alone, still not "this computer".
        let n = logout_notice(
            A,
            true,
            &LogoutReport::new(DeviceLogout::NotReached),
            "Ann",
            "Signal",
            &twin(),
        );
        assert_eq!(
            n.on_phone,
            "Signal could not be reached: remove the device linked on 2 Oct from the linked devices on your phone."
        );
        assert!(!n.on_phone.contains("this computer"));
    }

    #[test]
    fn the_questions_name_what_they_do_and_never_print_their_text() {
        let q = remove_question(A, "Ann · +1".into(), true);
        assert_eq!(q.title, "Remove Account?");
        assert_eq!(q.confirm, "Remove Account");
        assert_eq!(q.paragraphs.len(), 3);
        assert!(q.paragraphs[0].starts_with("Kourier unlinks this computer"));
        let q = unlink_question(A, "Ann · +1".into());
        assert_eq!(q.confirm, "Unlink");
        assert!(!format!("{q:?}").contains("Ann"));
        let n = logout_notice(
            A,
            true,
            &LogoutReport::new(DeviceLogout::Removed),
            "Ann",
            "Signal",
            &one(),
        );
        assert!(!format!("{n:?}").contains("Ann"));
    }

    /// The phone has unlinked a logged-out account already: its removal only deletes what it has
    /// here, and the question does not claim to unlink anything.
    #[test]
    fn the_question_for_an_account_that_is_not_linked_does_not_claim_to_unlink() {
        let q = remove_question(A, "Ann · +1".into(), false);
        assert_eq!(q.paragraphs.len(), 3);
        assert_eq!(
            q.paragraphs[0],
            "Kourier deletes this account's chats, messages and files from this computer."
        );
        assert!(q.paragraphs.iter().all(|p| !p.contains("unlink")), "{q:?}");
    }
}
