//! `CoreSource`: the rows and banners of blended chats against the simulated core of
//! [`super::tests`]: the badge number, the account chip and the pinned block that the account
//! table gives every row once per call, and the banners of accounts whose chats are not in the
//! list (the hidden ones speak only when they need the user).

use kourier_core::facade::UiEvent;
use kourier_model::{
    AccountId, ConnState, HistoryTransfer, LogoutCause, NetworkKind, NetworkNote, UserAction,
};

use super::choice_tests::{answer, chat, hiding, uuid};
use super::tests::{account, source};
use super::*;
use crate::conversations::{BannerKind, hidden_unread};
use crate::shown_accounts::ShownAccounts;
use crate::transfer::TransferPhase;

/// An account of `network` named `name` (`None`: the profile has no name), id and uuid `n`.
fn acct(n: i64, network: NetworkKind, name: Option<&str>) -> CoreAccount {
    CoreAccount {
        id: AccountId(n),
        uuid: uuid(n as u128),
        network,
        display_name: name.map(Into::into),
        conn: ConnState::Online { detail: None },
        ..account()
    }
}

/// A source that knows these accounts and lists `rows`.
fn blended(accounts: &[CoreAccount], rows: &[CoreConversation]) -> CoreSource {
    let (mut src, sent) = source();
    src.reload();
    answer(&mut src, &sent, rows, Some(accounts));
    src
}

/// Signal "Home", Signal "Work", Threema "Desk": ids 1, 2, 3 (the places 1, 2, 0).
fn trio() -> Vec<CoreAccount> {
    vec![
        acct(1, NetworkKind::Signal, Some("Home")),
        acct(2, NetworkKind::Signal, Some("Work")),
        acct(3, NetworkKind::Threema, Some("Desk")),
    ]
}

/// One chat in each account of [`trio`], chat id = account id, newest first by id.
fn trio_chats() -> Vec<CoreConversation> {
    vec![
        chat(1, 1, "Dogs club", 30, false, 0),
        chat(2, 2, "Dogs walk", 20, false, 0),
        chat(3, 3, "Dogs park", 10, false, 0),
    ]
}

fn hiding_all(ns: &[u128]) -> ConversationFilter {
    ConversationFilter {
        hidden: ns.iter().map(|&n| uuid(n)).collect(),
        ..ConversationFilter::default()
    }
}

fn numbers(rows: &[ConversationRow]) -> Vec<(i64, u32)> {
    rows.iter().map(|r| (r.id.0, r.badge_number)).collect()
}

fn chips(rows: &[ConversationRow]) -> Vec<(i64, &str)> {
    rows.iter()
        .map(|r| (r.id.0, r.account_badge.as_str()))
        .collect()
}

#[test]
fn the_badge_number_is_the_place_among_its_networks_accounts() {
    let src = blended(&trio(), &trio_chats());
    // Signal 1, Signal 2, Threema alone.
    assert_eq!(
        numbers(&src.conversations(&ConversationFilter::default())),
        [(1, 1), (2, 2), (3, 0)]
    );
    // Hiding Signal 1 does not renumber: Signal 2 stays 2 (the place in the panel).
    assert_eq!(numbers(&src.conversations(&hiding(1))), [(2, 2), (3, 0)]);
    // Every row of a list with 2+ accounts carries its network icon.
    let rows = src.conversations(&ConversationFilter::default());
    assert_eq!(
        rows.iter()
            .map(|r| r.badge_icon.as_str())
            .collect::<Vec<_>>(),
        ["signal", "signal", "threema"]
    );
}

#[test]
fn the_number_stays_with_one_shown_account() {
    let src = blended(&trio(), &trio_chats());
    let rows = src.conversations(&hiding_all(&[1, 3]));
    assert_eq!(numbers(&rows), [(2, 2)], "still the 2nd Signal account");
    assert_eq!(
        rows[0].badge_icon, "signal",
        "the icon stays with 2+ accounts"
    );
}

#[test]
fn one_account_has_neither_number_nor_icon_nor_chip() {
    let src = blended(&[acct(1, NetworkKind::Signal, Some("Home"))], &trio_chats());
    let rows = src.conversations(&ConversationFilter::default());
    assert!(
        rows.iter()
            .all(|r| r.badge_number == 0 && r.badge_icon.is_empty() && r.account_badge.is_empty())
    );
}

#[test]
fn the_chip_needs_two_shown_accounts() {
    let src = blended(&trio(), &trio_chats());
    assert_eq!(
        chips(&src.conversations(&ConversationFilter::default())),
        [(1, "Home"), (2, "Work"), (3, "Desk")]
    );
    // Two shown: both carry theirs.
    assert_eq!(
        chips(&src.conversations(&hiding(3))),
        [(1, "Home"), (2, "Work")]
    );
    // One shown, though three accounts exist: no chip (the title says whose chats these are).
    assert_eq!(chips(&src.conversations(&hiding_all(&[1, 3]))), [(2, "")]);
}

#[test]
fn a_hidden_accounts_match_carries_its_chip() {
    let src = blended(&trio(), &trio_chats());
    let search = |hidden: &[u128]| ConversationFilter {
        query: "dogs".into(),
        ..hiding_all(hidden)
    };
    // One account shown (2), the others hidden: the matches of three accounts are listed, so
    // EVERY row names its account, the shown one's too (his Q2, the lead's S8).
    assert_eq!(
        chips(&src.conversations(&search(&[1, 3]))),
        [(1, "Home"), (2, "Work"), (3, "Desk")]
    );
    // Two shown, one hidden: all three marked.
    assert_eq!(
        chips(&src.conversations(&search(&[3]))),
        [(1, "Home"), (2, "Work"), (3, "Desk")]
    );
}

#[test]
fn a_search_over_two_accounts_names_every_row() {
    // One account shown (2): a search whose matches are of two or more accounts names the
    // account on every row, the shown account's too (S8: "each with its account's name on
    // the row"); the matches of one account alone stay as the rules of the list say.
    let src = blended(&trio(), &trio_chats());
    let search = |query: &str| ConversationFilter {
        query: query.into(),
        ..hiding_all(&[1, 3])
    };
    assert_eq!(
        chips(&src.conversations(&search("dogs"))),
        [(1, "Home"), (2, "Work"), (3, "Desk")]
    );
    // Only the shown account's chat matches: one account, no chip (the title names it).
    assert_eq!(chips(&src.conversations(&search("walk"))), [(2, "")]);
    // Only a hidden account's chat matches: it carries its chip.
    assert_eq!(chips(&src.conversations(&search("park"))), [(3, "Desk")]);
    // The shown and one hidden account: both named.
    let two = ConversationFilter {
        query: "dogs".into(),
        ..hiding_all(&[3])
    };
    assert_eq!(
        chips(&src.conversations(&two)),
        [(1, "Home"), (2, "Work"), (3, "Desk")]
    );
    // No search: nothing changes (one shown, no chip).
    assert_eq!(chips(&src.conversations(&hiding_all(&[1, 3]))), [(2, "")]);
}

#[test]
fn a_chip_falls_back_to_the_network_name() {
    let accounts = [
        acct(1, NetworkKind::Signal, None),
        acct(2, NetworkKind::Threema, Some("Desk")),
    ];
    let src = blended(&accounts, &trio_chats()[..2]);
    assert_eq!(
        chips(&src.conversations(&ConversationFilter::default())),
        [(1, "Signal"), (2, "Desk")],
        "a shown account never loses its chip"
    );
}

#[test]
fn two_accounts_with_one_name_get_their_number() {
    let accounts = [
        acct(1, NetworkKind::Signal, Some("Andrii")),
        acct(2, NetworkKind::Signal, Some("Andrii")),
        acct(3, NetworkKind::Threema, Some("Andrii")),
    ];
    let src = blended(&accounts, &trio_chats());
    // The two Signal accounts are told apart by their place; the Threema one is the only
    // account of its network (its icon already tells it apart) and keeps the plain name.
    assert_eq!(
        chips(&src.conversations(&ConversationFilter::default())),
        [(1, "Andrii · 1"), (2, "Andrii · 2"), (3, "Andrii")]
    );
    // Hidden or not, the number is the place in the panel.
    assert_eq!(
        chips(&src.conversations(&hiding(1))),
        [(2, "Andrii · 2"), (3, "Andrii")]
    );
}

#[test]
fn pinned_chats_stand_account_by_account() {
    let pinned = |id: i64, acct: i64, pin: u32, last: i64| CoreConversation {
        pinned_order: Some(pin),
        ..chat(id, acct, "Pinned", last, false, 0)
    };
    let rows = [
        // Account 2 (second in the panel): its phone's pins 0 and 1.
        pinned(10, 2, 0, 5),
        pinned(11, 2, 1, 4),
        // Account 1 (first in the panel): one pin at position 7.
        pinned(12, 1, 7, 3),
        // Not pinned: newest first after the block, of every account.
        chat(13, 1, "Plain", 100, false, 0),
        chat(14, 2, "Plain", 200, false, 0),
    ];
    let src = blended(&trio(), &rows);
    assert_eq!(
        super::choice_tests::ids(&src.conversations(&ConversationFilter::default())),
        [12, 10, 11, 14, 13],
        "account 1's pins first, then account 2's in its own pin order"
    );
    // Hiding an account takes its pins away; the other keeps its order.
    assert_eq!(
        super::choice_tests::ids(&src.conversations(&hiding(1))),
        [10, 11, 14]
    );
}

#[test]
fn the_rail_counts_hidden_unread() {
    let mut rows = trio_chats();
    rows[0].unread = 2;
    rows[1].unread = 5;
    let mut muted = chat(4, 2, "Muted", 5, false, 9);
    muted.muted_until = Some(i64::MAX);
    let archived = chat(5, 2, "Archived", 4, true, 7);
    let mut marked = chat(6, 3, "Marked", 3, false, 0);
    marked.marked_unread = true;
    rows.extend([muted, archived, marked]);
    let src = blended(&trio(), &rows);
    let mut panel = src.accounts();
    let mut shown = ShownAccounts::default();
    shown.click(&panel, AccountId(1));
    shown.fill_rows(&mut panel);
    // The pills of the hidden accounts 2 and 3: the 5 of account 2 (a muted and an archived chat
    // count nowhere), nothing for the chat of account 3 that is only marked unread.
    assert_eq!(
        panel.iter().map(|a| a.unread).collect::<Vec<_>>(),
        [2, 5, 0]
    );
    assert_eq!(hidden_unread(&panel), 5);
    shown.show_all();
    shown.fill_rows(&mut panel);
    assert_eq!(hidden_unread(&panel), 0, "nothing is hidden");
}

// ---------- banners ----------

fn conn(src: &mut CoreSource, account: i64, state: ConnState) {
    src.on_event(&UiEvent::ConnState {
        account: AccountId(account),
        state,
    });
}

fn waiting(src: &mut CoreSource, account: i64) {
    src.on_event(&UiEvent::HistoryTransfer {
        account: AccountId(account),
        state: HistoryTransfer::Waiting,
    });
}

fn logged_out() -> ConnState {
    ConnState::LoggedOut {
        reason: "unlinked".into(),
        cause: LogoutCause::default(),
        note: NetworkNote::default(),
    }
}

/// Home (1) and Work (2), both online.
fn pair() -> CoreSource {
    blended(
        &[
            acct(1, NetworkKind::Signal, Some("Home")),
            acct(2, NetworkKind::Threema, Some("Work")),
        ],
        &[],
    )
}

#[test]
fn a_transfer_of_a_hidden_account_shows() {
    let mut src = pair();
    waiting(&mut src, 2);
    let b = src.banners(&hiding(2));
    let t = b.transfer.expect("the hidden account's transfer shows");
    assert_eq!((t.account, t.kind), (AccountId(2), BannerKind::Transfer));
    assert!(
        t.text.starts_with("Work: "),
        "it says whose it is: {}",
        t.text
    );
    assert!(b.state.is_none());
    // The shown accounts have none: the placeholder of an empty list says nothing about it.
    assert!(b.shown_transfer.is_none());
    // Shown, it is on both.
    let b = src.banners(&ConversationFilter::default());
    assert_eq!(
        (
            b.transfer.map(|t| t.account),
            b.shown_transfer.map(|t| t.account)
        ),
        (Some(AccountId(2)), Some(AccountId(2)))
    );
}

#[test]
fn a_hidden_offline_account_with_a_waiting_transfer_shows_the_transfer() {
    // The one banner an account has is the more pressing of its state and its transfer, and a
    // running transfer ranks below Offline: the hidden account's transfer is taken directly.
    let mut src = pair();
    conn(&mut src, 2, ConnState::Offline { retry_at_ms: None });
    waiting(&mut src, 2);
    let b = src.banners(&hiding(2));
    let t = b
        .transfer
        .expect("the transfer shows though the account is offline");
    assert_eq!((t.account, t.kind), (AccountId(2), BannerKind::Transfer));
    assert!(t.transfer.is_some());
    assert!(b.state.is_none(), "its Offline state stays hidden");
}

#[test]
fn the_history_phases_of_a_hidden_account_show() {
    let mut src = blended(
        &[
            acct(1, NetworkKind::Fake, Some("Home")),
            acct(2, NetworkKind::Fake, Some("Work")),
        ],
        &[],
    );
    // WhatsApp-style pushed history of the hidden account: its network has no history transfer.
    for n in &mut src.networks {
        n.history_transfer = false;
    }
    src.on_event(&UiEvent::HistoryProgress {
        account: AccountId(2),
        conv: ConversationId(1),
        progress: None,
        has_more: true,
    });
    let t = src
        .banners(&hiding(2))
        .transfer
        .expect("the history phase shows");
    assert_eq!(t.kind, BannerKind::History);
    assert_eq!(t.transfer.map(|v| v.phase), Some(TransferPhase::History));
    // Updating contacts and groups after a link.
    let mut src = pair();
    src.on_event(&UiEvent::AccountProgress {
        account: AccountId(2),
        progress: kourier_model::AccountProgress {
            profiles_left: 20,
            groups_left: 3,
            ..kourier_model::AccountProgress::default()
        },
    });
    let t = src
        .banners(&hiding(2))
        .transfer
        .expect("the update phase shows");
    assert_eq!(t.kind, BannerKind::Updating);
}

#[test]
fn a_hidden_accounts_state_shows_when_it_needs_the_user() {
    let needs: [(&str, ConnState, BannerKind); 3] = [
        ("logged out", logged_out(), BannerKind::LoggedOut),
        (
            "clock",
            ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: 7_200_000 }),
            BannerKind::Clock,
        ),
        (
            "challenge",
            ConnState::NeedsUserAction(UserAction::SolveChallenge {
                url: "https://example.invalid/c".into(),
            }),
            BannerKind::Challenge,
        ),
    ];
    for (what, state, kind) in needs {
        let mut src = pair();
        conn(&mut src, 2, state);
        let b = src
            .banners(&hiding(2))
            .state
            .unwrap_or_else(|| panic!("{what}: a hidden account shows its banner"));
        assert_eq!((b.account, b.kind), (AccountId(2), kind), "{what}");
        assert!(b.text.starts_with("Work: "), "{what}: {}", b.text);
    }
}

#[test]
fn a_logged_out_hidden_account_shows_its_banner() {
    let mut src = pair();
    conn(&mut src, 2, logged_out());
    let b = src.banners(&hiding(2)).state.expect("its banner");
    assert_eq!(b.kind, BannerKind::LoggedOut);
    assert!(!b.action.is_empty(), "with its \"Link Again\"");
    // Shown or hidden, the same banner (nothing else is going on).
    assert_eq!(
        src.banners(&ConversationFilter::default())
            .state
            .map(|b| b.kind),
        Some(BannerKind::LoggedOut)
    );
}

#[test]
fn an_offline_hidden_account_shows_no_banner() {
    for (what, state) in [
        ("offline", ConnState::Offline { retry_at_ms: None }),
        ("connecting", ConnState::Connecting),
        ("syncing", ConnState::Syncing { fraction: None }),
    ] {
        let mut src = pair();
        conn(&mut src, 2, state.clone());
        assert!(
            src.banners(&hiding(2)).state.is_none(),
            "{what}: a hidden account's passing state is not shown"
        );
        // The same state of a SHOWN account is: the set decides.
        assert!(
            src.banners(&ConversationFilter::default()).state.is_some(),
            "{what}"
        );
    }
    // A lost part of the pushed history is a quiet line of a shown account only.
    let mut src = pair();
    src.on_event(&UiEvent::Notice {
        account: AccountId(2),
        notice: kourier_core::facade::BackendNotice::HistoryLost { count: 2 },
    });
    assert_eq!(
        src.banners(&ConversationFilter::default())
            .state
            .map(|b| b.kind),
        Some(BannerKind::HistoryLost)
    );
    assert!(src.banners(&hiding(2)).state.is_none());
}

#[test]
fn a_shown_accounts_banner_is_as_before_and_the_wrappers_keep_their_meaning() {
    let mut src = pair();
    conn(&mut src, 1, ConnState::Offline { retry_at_ms: None });
    waiting(&mut src, 2);
    let b = src.banners(&ConversationFilter::default());
    assert_eq!(
        b.state.as_ref().map(|b| (b.account, b.kind)),
        src.banner(None).as_ref().map(|b| (b.account, b.kind))
    );
    assert_eq!(
        b.transfer.as_ref().map(|b| (b.account, b.kind)),
        src.transfer_banner(None).map(|b| (b.account, b.kind))
    );
    // Hiding the account whose state it is takes that state away (Offline passes).
    assert!(src.banners(&hiding(1)).state.is_none());
}

#[test]
fn the_placeholder_reads_only_a_shown_accounts_transfer() {
    // A shown account with no chats and a hidden one whose transfer runs: the empty list must
    // not say that a history is coming into it.
    let mut src = pair();
    waiting(&mut src, 2);
    let b = src.banners(&hiding(2));
    assert!(b.transfer.is_some());
    assert!(b.shown_transfer.is_none());
    // When a shown account has one too, that one is the placeholder's.
    waiting(&mut src, 1);
    let b = src.banners(&hiding(2));
    assert_eq!(
        b.shown_transfer.map(|t| t.account),
        Some(AccountId(1)),
        "the shown account's own"
    );
}
