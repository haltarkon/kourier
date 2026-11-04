//! `CoreSource`: experimental networks (WhatsApp) that are turned off in this Kourier, and the
//! states a network reports honestly (restrictions, holds, history), against the simulated
//! core of [`super::tests`].

use kourier_core::facade::UiEvent;
use kourier_model::{AccountId, ConnState, NetworkKind};

use super::tests::{Sent, Store, account, settle, source_with, test_networks};
use super::*;
use crate::conversations::BannerKind;
use crate::login::{NetworkEntry, network_choices};

const CONV: ConversationId = ConversationId(1);

/// The welcome page's rows with WhatsApp compiled in: `on` says whether it was switched on at
/// this start.
pub(super) fn networks(whatsapp_on: bool) -> Vec<NetworkChoice> {
    let mut entries = vec![
        NetworkEntry::plain(NetworkKind::Signal),
        NetworkEntry {
            history_transfer: true,
            ..NetworkEntry::plain(NetworkKind::Fake)
        },
    ];
    entries.push(NetworkEntry {
        experimental: true,
        on: whatsapp_on,
        ..NetworkEntry::plain(NetworkKind::WhatsApp)
    });
    network_choices(&entries, false)
}

/// A store whose only account is a WhatsApp one.
pub(super) fn whatsapp_store() -> Store {
    let mut store = Store::new(&[(1, 3)]);
    store.accounts = vec![kourier_core::facade::AccountRow {
        network: NetworkKind::WhatsApp,
        display_name: Some("Me".into()),
        conn: ConnState::Offline { retry_at_ms: None },
        ..account()
    }];
    store
}

/// A source with a WhatsApp account and conversation 1 open.
fn with_whatsapp(whatsapp_on: bool) -> (CoreSource, Sent) {
    let (mut src, sent) = source_with(networks(whatsapp_on));
    let store = whatsapp_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    (src, sent)
}

/// The order of the banners of ONE account is the order of `BannerKind`: a paused account is
/// more pressing than anything that passes by itself and than a failed history transfer, and
/// less than an account the person must relink or restart.
#[test]
fn a_network_turned_off_has_its_place_in_the_order_of_the_banners() {
    assert!(BannerKind::NetworkOff > BannerKind::Offline);
    assert!(BannerKind::NetworkOff > BannerKind::TransferFailed);
    assert!(BannerKind::NetworkOff > BannerKind::Syncing);
    assert!(BannerKind::NetworkOff < BannerKind::Error);
    assert!(BannerKind::NetworkOff < BannerKind::LoggedOut);
    assert!(
        !BannerKind::NetworkOff.passing(),
        "it asks the person to act"
    );
    assert_eq!(BannerKind::NetworkOff.as_str(), "networkOff");
}

/// An account of a network that is off stays readable and says so: its banner replaces
/// "offline" (which is what the core shows an account without a backend) and offers "Turn On";
/// the composer of its chats is off with the reason.
#[test]
fn an_account_of_a_network_turned_off_says_so_and_its_chats_stay_readable() {
    let (src, _) = with_whatsapp(false);
    let b = src.banner(None).expect("the paused account's banner");
    assert_eq!(
        (b.kind, b.text.as_str(), b.action.as_str()),
        (
            BannerKind::NetworkOff,
            "WhatsApp is turned off. Turn it on to send and receive.",
            "Turn On and Restart"
        )
    );
    assert_eq!(b.account, AccountId(1));
    let header = src.chat(CONV).expect("the chat opens");
    assert_eq!(
        header.cannot_send,
        "You can read this chat. WhatsApp is turned off: turn it on to write here."
    );
    assert!(
        header.timers.is_empty(),
        "no timer menu for a chat that cannot send"
    );
    assert!(
        !src.timeline(CONV).is_empty(),
        "the messages are still there"
    );
}

/// The same account with the network on shows what the connection shows.
#[test]
fn the_same_account_with_its_network_on_shows_its_connection() {
    let (src, _) = with_whatsapp(true);
    let b = src.banner(None).expect("offline banner");
    assert_eq!(b.kind, BannerKind::Offline);
    assert!(src.chat(CONV).expect("chat").cannot_send.is_empty());
}

/// A network that is not experimental is never "off": a test network or Signal that is not
/// in the build is no pause.
#[test]
fn only_an_experimental_network_can_be_turned_off() {
    let (mut src, sent) = source_with(test_networks());
    let store = Store::new(&[(1, 3)]);
    src.reload();
    settle(&mut src, &sent, &store);
    // The Fake account is not experimental: its banner is its connection's.
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::Connecting)
    );
}

/// What the person must fix first keeps its place: a logged-out account shows "link again"
/// kind but not the button (linking needs the network on); "Turn On" is the way back.
#[test]
fn a_logged_out_account_of_a_network_turned_off_keeps_its_place_without_its_button() {
    let (mut src, _) = with_whatsapp(false);
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::LoggedOut {
            reason: "unlinked".into(),
            cause: Default::default(),
            note: Default::default(),
        },
    });
    let b = src.banner(None).expect("logged out banner");
    assert_eq!(b.kind, BannerKind::LoggedOut);
    assert!(b.action.is_empty(), "Link Again needs the network: {b:?}");
}

/// "Unlink…" asks the network, which is off: not offered (the core would refuse it) and a
/// question for it does not open. "Remove Account…" stays and says the phone is not told.
#[test]
fn unlink_is_not_offered_while_the_network_is_off_and_remove_says_the_phone_keeps_it() {
    let (mut src, sent) = with_whatsapp(false);
    let rows = src.accounts();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].offers_unlink, "no Unlink for a paused account");
    assert!(
        src.ask_account_action(AccountId(1), AccountAction::Unlink)
            .is_none()
    );

    let q = src
        .ask_account_action(AccountId(1), AccountAction::Remove)
        .expect("Remove is still offered");
    assert_eq!(q.title, "Remove Account?");
    assert!(
        q.paragraphs[0].starts_with("WhatsApp is turned off, so Kourier cannot tell your phone."),
        "{:?}",
        q.paragraphs
    );
    assert!(
        q.paragraphs[1].contains("stays in the linked devices of your phone"),
        "{:?}",
        q.paragraphs
    );
    sent.borrow_mut().clear();
    assert!(src.confirm_account_action(AccountId(1), AccountAction::Remove));
    assert!(
        sent.borrow()
            .iter()
            .any(|c| matches!(c, UiCommand::RemoveAccount { account } if *account == AccountId(1))),
        "{:?}",
        sent.borrow()
    );
}

/// With the network on the old questions stand: Unlink is offered and Remove tells the phone.
#[test]
fn with_the_network_on_unlink_and_the_old_remove_question_stand() {
    let (mut src, _) = with_whatsapp(true);
    assert!(src.accounts()[0].offers_unlink);
    let q = src
        .ask_account_action(AccountId(1), AccountAction::Remove)
        .expect("question");
    assert!(
        q.paragraphs[0].starts_with("Kourier unlinks this computer from your account"),
        "{:?}",
        q.paragraphs
    );
}

// ---------- Telegram: a build that cannot log in to it (KT-1b, his Q4 and U-1) ----------

/// The welcome page's rows with Telegram listed: `on` says it was registered at this start,
/// `missing` that this build cannot log in to it (the application lists such a network whether
/// the person turned it on or not).
pub(super) fn telegram_networks(
    on: bool,
    missing: Option<crate::login::NetworkMissing>,
) -> Vec<NetworkChoice> {
    network_choices(
        &[
            NetworkEntry::plain(NetworkKind::Signal),
            NetworkEntry {
                experimental: true,
                on,
                missing,
                history: kourier_model::HistorySource::OnDemand,
                receipts_always_sent: true,
                ..NetworkEntry::plain(NetworkKind::Telegram)
            },
        ],
        false,
    )
}

/// A store whose only account is a Telegram one.
pub(super) fn telegram_store() -> Store {
    let mut store = Store::new(&[(1, 3)]);
    store.accounts = vec![kourier_core::facade::AccountRow {
        network: NetworkKind::Telegram,
        display_name: Some("Me".into()),
        conn: ConnState::Offline { retry_at_ms: None },
        ..account()
    }];
    store
}

/// A source with a Telegram account and conversation 1 open.
pub(super) fn with_telegram(
    on: bool,
    missing: Option<crate::login::NetworkMissing>,
) -> (CoreSource, Sent) {
    let (mut src, sent) = source_with(telegram_networks(on, missing));
    let store = telegram_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    (src, sent)
}

/// A build that cannot log in to Telegram says what is missing on EVERY Telegram account, whether
/// the person turned the network on or off (turning it on would not help: no button), and the
/// composer of its chats is closed with the reason. Mutant: drop the missing branch of
/// `own_banner_bare` (the off account says "turned off", the on one "offline").
#[test]
fn an_account_of_a_network_the_build_cannot_log_in_to_says_what_is_missing_on_or_off() {
    use crate::login::NetworkMissing::{ApiKey, Library};
    for on in [true, false] {
        for (missing, text) in [
            (
                ApiKey,
                "This build of Kourier has no Telegram API key, so it cannot connect to Telegram. The chats stay readable.",
            ),
            (
                Library,
                "The Telegram part of Kourier is missing from this installation. The chats stay readable.",
            ),
        ] {
            let (src, _) = with_telegram(on, Some(missing));
            let b = src.banner(None).expect("the missing network's banner");
            assert_eq!(
                (b.kind, b.text.as_str(), b.action.as_str()),
                (BannerKind::NetworkOff, text, ""),
                "on: {on}, {missing:?}"
            );
            assert_eq!(b.account, AccountId(1));
            let header = src.chat(CONV).expect("the chat opens");
            assert_eq!(
                header.cannot_send,
                "You can read this chat. This build of Kourier cannot connect to Telegram.",
                "on: {on}"
            );
            assert!(header.timers.is_empty(), "no timer menu for a closed chat");
            assert!(!src.timeline(CONV).is_empty(), "the chats stay readable");
        }
    }
    // The controls: a Telegram that can log in keeps the words of an off and an on account.
    let (off, _) = with_telegram(false, None);
    let b = off.banner(None).expect("off banner");
    assert_eq!(
        (b.kind, b.text.as_str(), b.action.as_str()),
        (
            BannerKind::NetworkOff,
            "Telegram is turned off. Turn it on to send and receive.",
            "Turn On and Restart"
        )
    );
    let (on, _) = with_telegram(true, None);
    assert_eq!(
        on.banner(None).map(|b| b.kind),
        Some(BannerKind::Offline),
        "an account that can connect shows its connection"
    );
    assert!(on.chat(CONV).expect("chat").cannot_send.is_empty());
}

/// The missing part is not a ban: a hold of the account shows nothing (it has no backend), a
/// logged-out account keeps its place without its button, and WhatsApp's accounts beside it are
/// untouched.
#[test]
fn a_missing_network_has_no_hold_and_keeps_the_logged_out_place_without_its_button() {
    let (mut src, _) = with_telegram(true, Some(crate::login::NetworkMissing::ApiKey));
    // Missing is not "turned off": the two have their own words and the missing one no button.
    assert!(src.network_missing(NetworkKind::Telegram).is_some());
    assert!(!src.network_off(NetworkKind::Telegram));
    assert!(src.network_paused(NetworkKind::Telegram));
    hold(&mut src, locked_until(NOW + 3_600_000));
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::NetworkOff),
        "paused, not held"
    );
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state: ConnState::LoggedOut {
            reason: "unlinked".into(),
            cause: Default::default(),
            note: Default::default(),
        },
    });
    let b = src.banner(None).expect("logged out banner");
    assert_eq!(b.kind, BannerKind::LoggedOut);
    assert!(b.action.is_empty(), "Link Again needs the network: {b:?}");
    // A WhatsApp account of a build where only Telegram is missing is not touched.
    let mut rows = networks(false);
    rows.extend(telegram_networks(true, Some(crate::login::NetworkMissing::Library)).split_off(1));
    let (src, _) = source_with(rows);
    assert!(
        src.network_missing(NetworkKind::WhatsApp).is_none()
            && src.network_off(NetworkKind::WhatsApp)
    );
}

/// "Unlink…" asks a network that cannot be reached: not offered for a missing network either, and
/// "Remove Account…" says why the session is not ended (this build cannot connect), not that the
/// network is "turned off", and does not speak of a phone (fix round F7: Telegram has sessions).
/// Mutant: the `missing` branch of the second paragraph removed (the phone sentence again).
#[test]
fn unlink_is_not_offered_to_a_missing_network_and_remove_says_why_the_phone_is_not_told() {
    let (mut src, _) = with_telegram(false, Some(crate::login::NetworkMissing::ApiKey));
    assert!(
        !src.accounts()[0].offers_unlink,
        "no Unlink for a paused account"
    );
    let q = src
        .ask_account_action(AccountId(1), AccountAction::Remove)
        .expect("Remove is still offered");
    assert!(
        q.paragraphs[0].starts_with(
            "This build of Kourier cannot connect to Telegram, so it cannot end this session."
        ),
        "{:?}",
        q.paragraphs
    );
    assert!(!q.paragraphs[0].contains("turned off"));
    // Telegram has sessions, not a phone that holds the list of devices: no word of a phone in
    // what is said about the network (the last paragraph, about the data, names it for the
    // messages and stays), and the session is said to stay listed in the other apps.
    assert_eq!(
        q.paragraphs[1],
        "This session stays listed in your other Telegram apps until you end it there."
    );
    for p in &q.paragraphs[..2] {
        assert!(!p.contains("phone"), "{p}");
    }
}

/// "Remove Account…" of a Telegram account that is only turned OFF (the build can connect) speaks
/// of a session too, never of a phone (post-rebase fix a): the words of the missing case with
/// the reason "turned off", the same second sentence, and the Telegram-less control (WhatsApp's
/// off account keeps its phone sentences: `an_account_of_a_network_that_is_off...` above).
/// Mutants: `sessions` false for an account that is not missing (the phone sentences); the
/// second paragraph keyed on `missing` alone.
#[test]
fn remove_of_a_turned_off_telegram_speaks_of_a_session_and_not_of_a_phone() {
    let (mut src, _) = with_telegram(false, None);
    assert!(src.network_missing(NetworkKind::Telegram).is_none());
    assert!(src.network_off(NetworkKind::Telegram));
    let q = src
        .ask_account_action(AccountId(1), AccountAction::Remove)
        .expect("Remove is still offered");
    assert_eq!(
        q.paragraphs[0],
        "Telegram is turned off, so Kourier cannot end this session. Kourier deletes this account's chats, messages and files from this computer."
    );
    // The very sentence the missing case has.
    assert_eq!(
        q.paragraphs[1],
        "This session stays listed in your other Telegram apps until you end it there."
    );
    for p in &q.paragraphs[..2] {
        assert!(!p.contains("phone"), "{p}");
    }
    // The missing case keeps its own first words and the same second sentence.
    let (mut src, _) = with_telegram(false, Some(crate::login::NetworkMissing::Library));
    let m = src
        .ask_account_action(AccountId(1), AccountAction::Remove)
        .expect("Remove is still offered");
    assert!(m.paragraphs[0].starts_with("This build of Kourier cannot connect to Telegram"));
    assert_eq!(m.paragraphs[1], q.paragraphs[1]);
}

// ---------- what the network says: bans, logouts and its own words ----------

use kourier_model::{BanReason, LogoutCause, NetworkNote, Restriction};

use super::tests::NOW;

pub(super) fn conn(src: &mut CoreSource, state: ConnState) {
    src.on_event(&UiEvent::ConnState {
        account: AccountId(1),
        state,
    });
}

fn banned(reason: BanReason, until_ms: Option<u64>, note: NetworkNote) -> ConnState {
    ConnState::Restricted {
        until_ms,
        // The log-safe detail: the banner never shows it.
        reason: "SECRET raw detail of the backend".into(),
        kind: Restriction::Banned(reason),
        note,
    }
}

/// A ban says why, in the viewmodel's words, and until when (the calendar is UTC and "now" is
/// 08:53 on the day the banner is read).
#[test]
fn a_ban_names_its_reason_and_its_end() {
    let (mut src, _) = with_whatsapp(true);
    let end = u64::try_from(NOW + 3 * 3_600_000).expect("time");
    let table = [
        (
            BanReason::TooManyStrangers,
            "WhatsApp banned this account until today at 11:53: you sent too many messages to people who have not saved your number.",
        ),
        (
            BanReason::ManyBlocked,
            "WhatsApp banned this account until today at 11:53: too many people blocked you.",
        ),
        (
            BanReason::TooManyGroups,
            "WhatsApp banned this account until today at 11:53: you created too many groups with people who have not saved your number.",
        ),
        (
            BanReason::SameMessageToMany,
            "WhatsApp banned this account until today at 11:53: you sent the same message to too many people.",
        ),
        (
            BanReason::BroadcastList,
            "WhatsApp banned this account until today at 11:53: you sent too many messages to a broadcast list.",
        ),
        (
            BanReason::Other(7),
            "WhatsApp banned this account until today at 11:53.",
        ),
    ];
    for (reason, text) in table {
        conn(&mut src, banned(reason, Some(end), NetworkNote::default()));
        let b = src.banner(None).expect("ban banner");
        assert_eq!((b.kind, b.text.as_str()), (BannerKind::Restricted, text));
        assert!(b.action.is_empty(), "nothing to press during a ban");
        assert!(
            !b.text.contains("SECRET"),
            "the backend's detail stays in the log"
        );
    }
    // Without a date no date is made up.
    conn(
        &mut src,
        banned(BanReason::ManyBlocked, None, NetworkNote::default()),
    );
    assert_eq!(
        src.banner(None).expect("ban").text,
        "WhatsApp banned this account for now: too many people blocked you."
    );
    // A restriction the backend could not describe is told in general, never in its words.
    conn(
        &mut src,
        ConnState::Restricted {
            until_ms: Some(end + 24 * 3_600_000),
            reason: "SECRET raw detail of the backend".into(),
            kind: Restriction::Unknown,
            note: NetworkNote::default(),
        },
    );
    assert_eq!(
        src.banner(None).expect("restricted").text,
        "WhatsApp restricted this account until tomorrow at 11:53."
    );
}

/// A ban that is over does not reconnect by itself: the banner offers "Reconnect", which is
/// the restart of the account.
#[test]
fn a_ban_that_has_ended_waits_for_the_person_to_reconnect() {
    let (mut src, sent) = with_whatsapp(true);
    let past = u64::try_from(NOW - 1_000).expect("time");
    conn(
        &mut src,
        banned(
            BanReason::TooManyStrangers,
            Some(past),
            NetworkNote::default(),
        ),
    );
    let b = src.banner(None).expect("ended");
    assert_eq!(
        (b.kind, b.text.as_str(), b.action.as_str()),
        (
            BannerKind::Error,
            "The WhatsApp ban on this account has ended. Reconnect to send and receive again.",
            "Reconnect"
        )
    );
    sent.borrow_mut().clear();
    assert!(src.restart_account(AccountId(1)));
    assert!(
        sent.borrow().iter().any(
            |c| matches!(c, UiCommand::RestartAccount { account } if *account == AccountId(1))
        )
    );
}

/// What the network says itself is plain text on one cut line, and its page is opened only
/// when it is an `https://` address.
#[test]
fn the_networks_own_words_are_plain_text_and_its_page_is_https_only() {
    let (mut src, _) = with_whatsapp(true);
    let end = u64::try_from(NOW + 3_600_000).expect("time");
    let note = |text: &str, url: &str| NetworkNote {
        explanation: Some(text.to_owned()),
        appeal_url: Some(url.to_owned()),
    };
    conn(
        &mut src,
        banned(
            BanReason::TooManyStrangers,
            Some(end),
            note(
                "Line one\n<b>line</b> two",
                "https://faq.example.invalid/ban",
            ),
        ),
    );
    let b = src.banner(None).expect("ban");
    assert_eq!(b.explanation, "Line one <b>line</b> two");
    assert_eq!(b.appeal_url, "https://faq.example.invalid/ban");
    assert_eq!(b.appeal_label, "Open WhatsApp's Page");
    for bad in [
        "http://faq.example.invalid/ban",
        "file:///etc/passwd",
        "https://",
        "https://a b",
        "sgnl://x",
    ] {
        conn(
            &mut src,
            banned(BanReason::TooManyStrangers, Some(end), note("x", bad)),
        );
        let b = src.banner(None).expect("ban");
        assert!(
            b.appeal_url.is_empty() && b.appeal_label.is_empty(),
            "{bad} must not become a button: {b:?}"
        );
    }
    // A long text is cut.
    conn(
        &mut src,
        banned(
            BanReason::TooManyStrangers,
            Some(end),
            note(&"word ".repeat(500), ""),
        ),
    );
    let shown = src.banner(None).expect("ban").explanation;
    assert!(
        shown.chars().count() <= 401 && shown.ends_with('…'),
        "{shown:?}"
    );
}

/// A logout says why when the network did; only a cause that can be linked again offers
/// "Link Again".
#[test]
fn a_logout_says_why_and_offers_link_again_only_where_it_can_work() {
    let (mut src, _) = with_whatsapp(true);
    let out = |cause| ConnState::LoggedOut {
        reason: "SECRET".into(),
        cause,
        note: NetworkNote::default(),
    };
    let table = [
        (
            LogoutCause::Unknown,
            "This device is no longer linked. Link it again to send and receive messages.",
            "Link Again",
        ),
        (
            LogoutCause::RemovedOnPhone,
            "This computer was removed from the linked devices on your phone. Link it again to send and receive messages.",
            "Link Again",
        ),
        (
            LogoutCause::PhoneInactive,
            "WhatsApp unlinked this computer because your phone was not used for a long time. Link it again to send and receive messages.",
            "Link Again",
        ),
        (
            LogoutCause::AccountGone,
            "WhatsApp closed or locked this account.",
            "",
        ),
        (
            LogoutCause::Unofficial,
            "WhatsApp logged this account out because Kourier is not an official app.",
            "",
        ),
    ];
    for (cause, text, action) in table {
        conn(&mut src, out(cause));
        let b = src.banner(None).expect("logged out");
        assert_eq!(
            (b.kind, b.text.as_str(), b.action.as_str()),
            (BannerKind::LoggedOut, text, action),
            "{cause:?}"
        );
    }
    // The network's words ride along.
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "SECRET".into(),
            cause: LogoutCause::AccountGone,
            note: NetworkNote {
                explanation: Some("Your account is locked.".into()),
                appeal_url: Some("https://faq.example.invalid/locked".into()),
            },
        },
    );
    let b = src.banner(None).expect("locked");
    assert_eq!(b.explanation, "Your account is locked.");
    assert_eq!(b.appeal_url, "https://faq.example.invalid/locked");
}

/// A source whose clock the test sets, with a WhatsApp account (network on) and the chat open.
pub(super) fn with_clock() -> (CoreSource, Sent, std::rc::Rc<std::cell::Cell<i64>>) {
    use std::cell::Cell;
    use std::rc::Rc;
    let clock = Rc::new(Cell::new(NOW));
    let sent: Sent = Rc::default();
    let (s2, c2) = (sent.clone(), clock.clone());
    let mut src = CoreSource::new(
        Box::new(move |c: UiCommand| {
            s2.borrow_mut().push(c);
            true
        }),
        CoreSourceConfig {
            now: Box::new(move || c2.get()),
            calendar: Box::new(crate::time::FixedOffset(0)),
            networks: networks(true),
            hostname: "workstation".into(),
            demo: None,
        },
    );
    let store = whatsapp_store();
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    (src, sent, clock)
}

/// The banner of a ban changes when the ban ends, with no event from the core: the tick sees
/// the instant pass, and the timer runs only while there is one to wait for.
#[test]
fn the_banner_of_a_ban_changes_by_itself_when_the_ban_ends() {
    let (mut src, _, clock) = with_clock();
    assert!(!src.has_timers(), "nothing to wait for yet");
    let end = NOW + 5 * 60_000;
    conn(
        &mut src,
        banned(
            BanReason::TooManyStrangers,
            Some(u64::try_from(end).expect("time")),
            NetworkNote::default(),
        ),
    );
    assert!(src.has_timers(), "a ban with an end wakes the tick");
    // The first tick only notes the time.
    clock.set(NOW + 1_000);
    assert!(!src.tick().accounts);
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::Restricted)
    );
    clock.set(end - 1_000);
    assert!(!src.tick().accounts, "not yet");
    clock.set(end + 1_000);
    assert!(src.tick().accounts, "the ban ended between two ticks");
    let b = src.banner(None).expect("ended");
    assert_eq!(
        (b.kind, b.action.as_str()),
        (BannerKind::Error, "Reconnect")
    );
    assert!(!src.has_timers(), "nothing left to wait for");
    clock.set(end + 2_000);
    assert!(!src.tick().accounts, "it is told once");
}

// ---------- a send hold: the connection is up, what the user sends waits ----------

use kourier_model::{AccountHold, HoldReason};

fn hold(src: &mut CoreSource, hold: Option<AccountHold>) -> Changes {
    src.on_event(&UiEvent::AccountHold {
        account: AccountId(1),
        hold,
    })
}

fn locked_until(ms: i64) -> Option<AccountHold> {
    Some(AccountHold {
        until_ms: Some(u64::try_from(ms).expect("time")),
        reason: HoldReason::ReachOutLocked,
    })
}

pub(super) fn online(src: &mut CoreSource) {
    conn(src, ConnState::Online { detail: None });
}

/// A hold says what is held and until when, in the viewmodel's words, with no button (nothing
/// to press: Kourier keeps the messages), in the place of a restriction in the order of the
/// banners, and only while the account has a backend to hold.
#[test]
fn a_send_hold_names_what_is_held_and_its_end_in_the_banner() {
    let (mut src, _) = with_whatsapp(true);
    online(&mut src);
    assert!(src.banner(None).is_none(), "no hold, nothing to say");
    let end = NOW + 3 * 3_600_000;
    assert!(hold(&mut src, locked_until(end)).accounts);
    let b = src.banner(None).expect("hold banner");
    assert_eq!(b.kind, BannerKind::Restricted);
    assert_eq!(
        b.text,
        "WhatsApp restricts this account until today at 11:53: messages to people who have not written to you may be refused. Kourier holds your messages until then."
    );
    assert!(b.action.is_empty(), "nothing to press");
    // The same event again changes nothing on screen.
    assert!(!hold(&mut src, locked_until(end)).accounts);
    // No date is made up.
    hold(
        &mut src,
        Some(AccountHold {
            until_ms: None,
            reason: HoldReason::ReachOutLocked,
        }),
    );
    assert_eq!(
        src.banner(None).expect("hold banner").text,
        "WhatsApp restricts this account for now: messages to people who have not written to you may be refused. Kourier holds your messages until it ends."
    );
    // A reason the backend could not give is told in general.
    hold(
        &mut src,
        Some(AccountHold {
            until_ms: Some(u64::try_from(end).expect("time")),
            reason: HoldReason::Unknown,
        }),
    );
    assert_eq!(
        src.banner(None).expect("hold banner").text,
        "WhatsApp is not taking the messages you send until today at 11:53. Kourier holds them until then."
    );
    // The hold counts more than a connection that is only down (what the person can do
    // about their sends is the pressing thing) ...
    hold(&mut src, locked_until(end));
    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::Restricted)
    );
    // ... and less than a ban, which says more, with the connection down.
    conn(
        &mut src,
        banned(
            BanReason::ManyBlocked,
            Some(u64::try_from(end).expect("time")),
            NetworkNote::default(),
        ),
    );
    assert!(
        src.banner(None).expect("ban").text.contains("banned"),
        "the ban's words, not the hold's"
    );
    // Lifted: the banner goes with it.
    online(&mut src);
    hold(&mut src, None);
    assert!(src.banner(None).is_none());
}

/// A frozen account (Telegram, S-2) has a sentence of its own (KT-1b): one with and without an
/// end (Telegram announces none), the way out named (the appeal is made in Telegram's own app),
/// nothing to press. The other reasons keep their sentences.
#[test]
fn a_frozen_account_has_its_own_sentence() {
    let (mut src, _) = with_telegram(true, None);
    online(&mut src);
    let end = NOW + 3 * 3_600_000;
    let banner_of = |src: &mut CoreSource, reason: HoldReason, until_ms: Option<u64>| {
        hold(src, Some(AccountHold { until_ms, reason }));
        let b = src.banner(None).expect("hold banner");
        (b.kind, b.text, b.action.is_empty())
    };
    let until = Some(u64::try_from(end).expect("time"));
    const FROZEN: &str = "Telegram has frozen this account. You can read your chats; Kourier holds what you send until Telegram lifts it. To appeal, open the Telegram app.";
    for until_ms in [None, until] {
        let (kind, text, no_button) = banner_of(&mut src, HoldReason::Frozen, until_ms);
        assert_eq!(kind, BannerKind::Restricted);
        assert_eq!(text, FROZEN, "with or without an end");
        assert!(
            no_button,
            "nothing to press: the appeal is in the network's own app"
        );
    }
    // The other reasons keep their words.
    let (_, unknown, _) = banner_of(&mut src, HoldReason::Unknown, None);
    assert_eq!(
        unknown,
        "Telegram is not taking the messages you send for now. Kourier holds them until it ends."
    );
    let (_, locked, _) = banner_of(&mut src, HoldReason::ReachOutLocked, None);
    assert!(
        locked.contains("restricts this account for now"),
        "{locked}"
    );
}

/// A hold on an account whose network is turned off is not shown: nothing is held there, the
/// account is paused ("turned off" says it).
#[test]
fn a_paused_account_says_it_is_turned_off_not_that_it_is_held() {
    let (mut src, _) = with_whatsapp(false);
    hold(&mut src, locked_until(NOW + 3_600_000));
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::NetworkOff)
    );
}

/// The line under a message waiting for the hold names the network and the end, with the
/// connection up; with the connection down the connection is what is missing; past the end
/// (the core lifts a hold at its end) the line is the plain one again; lifted, the message is
/// simply being sent. A rebuilt timeline says what the tick's lines say.
#[test]
fn a_message_held_back_says_who_restricted_the_account_and_until_when() {
    let (mut src, sent, clock) = with_clock();
    let mut store = whatsapp_store();
    let mut conv = Store::new(&[(1, 3)]).convs.remove(&1).expect("conv");
    super::tests::make_pending(&mut conv[2], false, None);
    let id = conv[2].id;
    store.convs.insert(1, conv);
    src.reload();
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    online(&mut src);
    let line = |src: &CoreSource| {
        let lines = src.wait_lines(CONV);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].0, id);
        // The timeline of the same moment says the same.
        let rows = src.timeline(CONV);
        let r = rows
            .iter()
            .find(|r| r.key == crate::timeline::RowKey::Message(id))
            .expect("row");
        assert_eq!(
            (r.wait.clone(), r.wait_timer.clone()),
            (lines[0].1.reason.clone(), lines[0].1.timer.clone())
        );
        lines[0].1.text()
    };
    assert_eq!(line(&src), "Sending… 0:42");
    let end = NOW + 3 * 3_600_000;
    let ch = hold(&mut src, locked_until(end));
    assert!(ch.timeline, "the open chat's lines are rebuilt");
    assert_eq!(
        line(&src),
        "Waiting: WhatsApp restricted this account until today at 11:53"
    );
    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    assert_eq!(line(&src), "Waiting for the connection · 0:42");
    online(&mut src);
    // The end passes before the core says so: no date that is gone.
    clock.set(end + 1_000);
    assert!(line(&src).starts_with("Sending…"), "{}", line(&src));
    clock.set(NOW);
    hold(&mut src, None);
    assert_eq!(line(&src), "Sending… 0:42");
}

/// A ban names its end under the messages it holds too; once it has ended the messages wait
/// for the person to reconnect, as the banner says.
#[test]
fn a_ban_names_its_end_under_the_held_message_and_an_ended_one_waits_for_reconnect() {
    let (mut src, sent, clock) = with_clock();
    let mut store = whatsapp_store();
    let mut conv = Store::new(&[(1, 3)]).convs.remove(&1).expect("conv");
    super::tests::make_pending(&mut conv[2], false, None);
    store.convs.insert(1, conv);
    src.reload();
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    let end = NOW + 5 * 60_000;
    conn(
        &mut src,
        banned(
            BanReason::TooManyStrangers,
            Some(u64::try_from(end).expect("time")),
            NetworkNote::default(),
        ),
    );
    let text = |src: &CoreSource| src.wait_lines(CONV).remove(0).1.text();
    assert_eq!(
        text(&src),
        "Waiting: WhatsApp restricted this account until today at 08:58"
    );
    clock.set(NOW + 1_000);
    src.tick();
    clock.set(end + 1_000);
    assert!(src.tick().accounts);
    assert!(
        text(&src).starts_with("Waiting for you to reconnect"),
        "{}",
        text(&src)
    );
}

/// The banner and the line change at the hold's end with no event: the tick sees the instant
/// pass, and the timer runs only while there is an end ahead.
#[test]
fn a_send_hold_that_runs_out_changes_the_banner_by_itself() {
    let (mut src, _, clock) = with_clock();
    online(&mut src);
    assert!(!src.has_timers());
    let end = NOW + 5 * 60_000;
    hold(&mut src, locked_until(end));
    assert!(src.has_timers(), "an end ahead wakes the tick");
    clock.set(NOW + 1_000);
    assert!(!src.tick().accounts, "the first tick only notes the time");
    clock.set(end + 1_000);
    assert!(src.tick().accounts, "the end passed between two ticks");
    assert!(src.banner(None).is_none(), "no date that is gone");
    assert!(!src.has_timers());
}

// ---------- a history the network pushes: a quiet banner, bounded ----------

use crate::transfer::TransferPhase;

pub(super) fn fill(
    src: &mut CoreSource,
    conv: i64,
    progress: Option<f32>,
    has_more: bool,
) -> Changes {
    src.on_event(&UiEvent::HistoryProgress {
        account: AccountId(1),
        conv: ConversationId(conv),
        progress,
        has_more,
    })
}

/// A network whose history is pushed to this computer (no `history_transfer`) shows a quiet
/// line while any chat of the account fills, with the percentage when the network gives one,
/// and nothing when the last chat is done. It is a passing banner of the transfer sort, so
/// the state banner of the account (Online: none) is not touched.
#[test]
fn a_history_the_network_pushes_shows_a_quiet_banner_while_chats_fill() {
    let (mut src, _, _) = with_clock();
    online(&mut src);
    assert!(src.transfer_banner(None).is_none());
    let ch = fill(&mut src, 1, None, true);
    assert!(ch.transfer && ch.accounts, "the banner appears");
    let b = src.transfer_banner(None).expect("history banner");
    assert_eq!(b.kind, BannerKind::History);
    assert!(b.kind.passing());
    assert_eq!(b.text, "Getting your message history from your phone…");
    assert!(b.action.is_empty());
    let t = b.transfer.expect("the view");
    assert_eq!(t.phase, TransferPhase::History);
    assert!(t.skip.is_empty() && t.retry.is_empty() && t.dismiss.is_empty());
    assert!(t.progress < 0.0, "no bar without a fraction");
    // The network gives a fraction: the line reads it.
    assert!(fill(&mut src, 1, Some(0.4), true).transfer);
    let b = src.transfer_banner(None).expect("history banner");
    assert_eq!(b.text, "Getting your message history from your phone: 40 %");
    assert!((b.transfer.expect("view").progress - 0.4).abs() < 1e-6);
    // The same word again changes nothing on screen.
    assert!(!fill(&mut src, 1, Some(0.4), true).transfer);
    // A second chat; the first one finishing does not end the banner, the last one does.
    fill(&mut src, 2, Some(0.5), true);
    fill(&mut src, 1, None, false);
    assert!(src.transfer_banner(None).is_some(), "chat 2 still fills");
    let ch = fill(&mut src, 2, None, false);
    assert!(ch.transfer && ch.accounts);
    assert!(src.transfer_banner(None).is_none());
    // A chat that was never filling ends nothing.
    assert!(!fill(&mut src, 3, None, false).transfer);
    assert!(src.banner(None).is_none(), "the state banner is its own");
}

/// A network whose history comes with a transfer at the link (Signal's archive) has the
/// banners of that transfer, not this one; its chats are held for Clear and Delete as before.
#[test]
fn an_account_with_a_history_transfer_shows_no_pushed_history_banner() {
    let (mut src, sent) = source_with(networks(true));
    let store = Store::new(&[(1, 3)]);
    src.reload();
    settle(&mut src, &sent, &store);
    src.open_conversation(Some(CONV));
    settle(&mut src, &sent, &store);
    let ch = fill(&mut src, 1, None, true);
    assert!(!ch.transfer && !ch.accounts);
    assert!(src.transfer_banner(None).is_none());
    assert!(
        src.quit_question().is_none(),
        "no pushed history to lose: the quit question is the transfer's, and none runs"
    );
    assert_eq!(
        src.chat(CONV).expect("header").still_filling,
        "Available once this chat's history has arrived",
        "the hold on Clear and Delete is the same"
    );
}

/// A phone that goes away leaves no end to wait for: with no batch for five minutes the banner
/// and the hold on Clear and Delete let go by themselves (the tick), and a batch in between
/// starts the five minutes again. The timer runs only while a history is waited for.
#[test]
fn a_pushed_history_that_goes_quiet_ends_after_a_while() {
    let (mut src, _, clock) = with_clock();
    online(&mut src);
    assert!(!src.has_timers());
    fill(&mut src, 1, Some(0.1), true);
    assert!(src.has_timers());
    assert!(!src.chat(CONV).expect("header").still_filling.is_empty());
    let idle = HISTORY_FILL_IDLE_MS;
    clock.set(NOW + idle - 1);
    let ch = src.tick();
    assert!(!ch.transfer && src.transfer_banner(None).is_some());
    // A batch: five minutes again.
    fill(&mut src, 1, Some(0.2), true);
    clock.set(NOW + idle + 10);
    let ch = src.tick();
    assert!(!ch.transfer && src.transfer_banner(None).is_some());
    clock.set(NOW + idle - 1 + idle);
    let ch = src.tick();
    assert!(
        ch.transfer && ch.accounts && ch.header,
        "the banner and the open chat's menu change"
    );
    assert!(src.transfer_banner(None).is_none());
    assert_eq!(src.chat(CONV).expect("header").still_filling, "");
    assert!(!src.has_timers(), "nothing left to wait for");
    assert!(!src.tick().transfer, "it ends once");
}

/// The banner does not outlive its account's state: a connection that goes ends the push
/// (nothing is coming on it), a logout and a removal too, and a batch that arrives for an
/// account that is down starts nothing.
#[test]
fn a_pushed_history_ends_with_the_connection_the_logout_and_the_removal() {
    let (mut src, sent, _) = with_clock();
    online(&mut src);
    fill(&mut src, 1, None, true);
    assert!(src.transfer_banner(None).is_some());
    // The connection goes: the push stops with it.
    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    assert!(src.transfer_banner(None).is_none());
    assert_eq!(src.chat(CONV).expect("header").still_filling, "");
    assert!(!src.has_timers());
    // Connected again, the phone pushes again.
    online(&mut src);
    fill(&mut src, 1, None, true);
    assert!(src.transfer_banner(None).is_some());
    // Logged out.
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "test".into(),
            cause: LogoutCause::RemovedOnPhone,
            note: NetworkNote::default(),
        },
    );
    assert!(src.transfer_banner(None).is_none());
    online(&mut src);
    fill(&mut src, 1, None, true);
    assert!(src.transfer_banner(None).is_some());
    // The account is removed: the next list of accounts does not have it.
    let mut store = whatsapp_store();
    store.accounts = vec![kourier_core::facade::AccountRow {
        id: AccountId(2),
        ..store.accounts[0].clone()
    }];
    src.on_event(&UiEvent::AccountsChanged);
    settle(&mut src, &sent, &store);
    assert!(src.fills.is_empty() && src.filling.is_empty());
}

// ---------- a history that lost a part: the quiet line that stays ----------

/// The backend tells that `count` chunks of the pushed history never arrived.
fn lost(src: &mut CoreSource, count: u32) -> Changes {
    src.on_event(&UiEvent::Notice {
        account: AccountId(1),
        notice: BackendNotice::HistoryLost { count },
    })
}

/// A notice that part of the pushed history is lost shows as a quiet line of the account's
/// banner: its own sentence (no count: a chunk is the network's unit), no button, and it
/// stays (it is no state that passes). It says nothing for a count of 0, and the same notice
/// again changes nothing on screen.
#[test]
fn a_lost_part_of_the_history_is_a_quiet_line_that_stays() {
    let (mut src, _, _) = with_clock();
    online(&mut src);
    assert!(src.banner(None).is_none());
    assert!(!lost(&mut src, 0).accounts, "a count of 0 says nothing");
    assert!(src.banner(None).is_none());
    assert!(lost(&mut src, 2).accounts, "the line appears");
    let b = src.banner(None).expect("the quiet line");
    assert_eq!(b.kind, BannerKind::HistoryLost);
    assert_eq!(b.kind.as_str(), "historyLost");
    assert_eq!(
        b.text,
        "Part of your message history did not arrive from your phone."
    );
    assert!(b.action.is_empty() && b.url.is_empty() && b.transfer.is_none());
    assert!(!b.kind.passing(), "it stays for the session");
    assert_eq!(b.account, AccountId(1));
    assert!(!lost(&mut src, 2).accounts, "the same notice again");
    assert!(!lost(&mut src, 5).accounts, "another count, the same line");
    assert!(src.transfer_banner(None).is_none(), "it is no transfer");
    assert!(!src.has_timers(), "nothing to wait for");
}

/// It is the lowest of all the banners: every state, the quiet line of the history still
/// arriving and a running or failed transfer stand above it, and it is there again when they
/// are gone; with several accounts it says which one it is about.
#[test]
fn the_lost_history_line_gives_way_to_every_other_banner() {
    assert!(BannerKind::HistoryLost < BannerKind::Connecting);
    let (mut src, _, _) = with_clock();
    online(&mut src);
    lost(&mut src, 1);
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::HistoryLost)
    );
    // A state above it.
    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    assert_eq!(src.banner(None).map(|b| b.kind), Some(BannerKind::Offline));
    conn(&mut src, ConnState::Connecting);
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::Connecting)
    );
    online(&mut src);
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::HistoryLost)
    );
    // The history still arriving: its own quiet line is the account's banner meanwhile.
    fill(&mut src, 1, Some(0.5), true);
    assert!(
        src.banner(None).is_none(),
        "the account shows its transfer sort"
    );
    assert_eq!(
        src.transfer_banner(None).map(|b| b.kind),
        Some(BannerKind::History)
    );
    fill(&mut src, 1, None, false);
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::HistoryLost)
    );
    assert!(src.transfer_banner(None).is_none());
}

/// The line is about the link that lost the part: a logout, a removal or an account that is
/// not linked any more takes it away, and a new link does not bring it back.
#[test]
fn the_lost_history_line_goes_with_the_link() {
    let (mut src, sent, _) = with_clock();
    online(&mut src);
    lost(&mut src, 1);
    assert!(src.banner(None).is_some());
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "test".into(),
            cause: LogoutCause::RemovedOnPhone,
            note: NetworkNote::default(),
        },
    );
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::LoggedOut)
    );
    online(&mut src);
    assert!(
        src.banner(None).is_none(),
        "the old link's loss is not shown again"
    );
    // The account is removed: the next list of accounts does not have it.
    lost(&mut src, 1);
    assert!(src.banner(None).is_some());
    let mut store = whatsapp_store();
    store.accounts = vec![kourier_core::facade::AccountRow {
        id: AccountId(2),
        ..store.accounts[0].clone()
    }];
    src.on_event(&UiEvent::AccountsChanged);
    settle(&mut src, &sent, &store);
    assert!(src.lost_history.is_empty());
    // A removal says so too, before the list is read again.
    lost(&mut src, 1);
    src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(1),
    });
    assert!(src.lost_history.is_empty());
}
