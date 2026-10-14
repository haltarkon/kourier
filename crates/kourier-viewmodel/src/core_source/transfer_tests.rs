//! `CoreSource`: the history transfer of a newly linked account (architecture §14.2 item 14)
//! on the link page and in the account banner, its ends, and the question before an account
//! is removed and linked again. Against the simulated core of [`super::tests`].

use kourier_core::facade::{Query, QueryData, UiCommand, UiEvent};
use kourier_model::{
    AccountId, AccountState, AccountUuid, ConnState, HistoryTransfer, HistoryTransferFailure,
    LoginStep, LoginStepKind, NetworkKind, UserAction,
};

use super::tests::{Sent, Store, account, result, settle, source, take_queries};
use super::*;
use crate::conversations::BannerKind;
use crate::transfer::{TransferPhase, TransferView};

const ME: AccountId = AccountId(1);

fn transfer(account: AccountId, state: HistoryTransfer) -> UiEvent {
    UiEvent::HistoryTransfer { account, state }
}

fn downloading(done: u64, total: Option<u64>) -> HistoryTransfer {
    HistoryTransfer::Downloading { done, total }
}

fn failed(kind: HistoryTransferFailure) -> HistoryTransfer {
    HistoryTransfer::Failed(kind)
}

/// A source that knows the account [`ME`] (the test network, online).
fn loaded() -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    settle(&mut src, &sent, &Store::new(&[(1, 2)]));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::Online { detail: None },
    });
    sent.borrow_mut().clear();
    (src, sent)
}

/// A source with these accounts.
fn with_accounts(accounts: Vec<kourier_core::facade::AccountRow>) -> (CoreSource, Sent) {
    let (mut src, sent) = source();
    src.reload();
    let store = Store::new(&[(1, 2)]);
    for (t, q) in take_queries(&sent) {
        let data = match q {
            Query::Accounts => QueryData::Accounts(accounts.clone()),
            other => store.answer(&other),
        };
        src.on_event(&result(t, data));
    }
    sent.borrow_mut().clear();
    (src, sent)
}

/// The commands sent so far that are not queries, by name and account.
fn commands(sent: &Sent) -> Vec<(&'static str, i64)> {
    let out = sent
        .borrow()
        .iter()
        .filter_map(|c| match c {
            UiCommand::SkipHistoryTransfer { account } => Some(("skip", account.0)),
            UiCommand::DismissHistoryTransfer { account } => Some(("dismiss", account.0)),
            UiCommand::RemoveAccount { account } => Some(("remove", account.0)),
            UiCommand::AddAccount { .. } => Some(("add", 0)),
            UiCommand::CancelLogin { account } => Some(("cancel-login", account.0)),
            UiCommand::Relink { account, .. } => Some(("relink", account.0)),
            UiCommand::Unlink { account } => Some(("unlink", account.0)),
            _ => None,
        })
        .collect();
    sent.borrow_mut().clear();
    out
}

/// Starts linking a new account of the test network; the core names it `account`.
fn linking(src: &mut CoreSource, sent: &Sent, account: AccountId) {
    src.start_login(NetworkKind::Fake).expect("start");
    src.on_event(&UiEvent::AccountAdded { account });
    src.on_event(&UiEvent::LoginStep {
        account,
        step: LoginStep {
            step_id: "linking".into(),
            txn: 0,
            kind: LoginStepKind::Progress {
                label: "Linking".into(),
                fraction: None,
            },
        },
    });
    // The queries stay to be answered ([`known`]); commands are forgotten.
    sent.borrow_mut()
        .retain(|c| matches!(c, UiCommand::Query { .. }));
}

/// What the main window shows above the list for all accounts when at most ONE banner is
/// expected: the state of an account or a history transfer, never both.
fn shown(src: &CoreSource) -> Option<Banner> {
    let (state, transfer) = (src.banner(None), src.transfer_banner(None));
    assert!(
        state.as_ref().is_none_or(|b| b.transfer.is_none()
            && !matches!(b.kind, BannerKind::Transfer | BannerKind::TransferFailed)),
        "a transfer is never the state's banner"
    );
    assert!(
        transfer.as_ref().is_none_or(|b| b.transfer.is_some()),
        "the transfer's banner carries its view"
    );
    assert!(
        state.is_none() || transfer.is_none(),
        "two banners where one was expected: {state:?} and {transfer:?}"
    );
    state.or(transfer)
}

/// Answers the accounts query with `ids`, Active (the core took them).
fn known(src: &mut CoreSource, sent: &Sent, ids: &[AccountId]) {
    let rows: Vec<_> = ids
        .iter()
        .map(|id| kourier_core::facade::AccountRow {
            id: *id,
            uuid: AccountUuid::new_random(),
            conn: ConnState::Online { detail: None },
            ..account()
        })
        .collect();
    src.on_event(&UiEvent::AccountsChanged);
    // Answered until no accounts query follows up (the other queries are dropped).
    loop {
        let accounts: Vec<_> = take_queries(sent)
            .into_iter()
            .filter(|(_, q)| matches!(q, Query::Accounts))
            .collect();
        if accounts.is_empty() {
            break;
        }
        for (t, _) in accounts {
            src.on_event(&result(t, QueryData::Accounts(rows.clone())));
        }
    }
}

/// The transfer of `account` as its banner shows it.
fn banner_view(src: &CoreSource, account: AccountId) -> Option<TransferView> {
    src.transfer_banner(Some(account)).and_then(|b| b.transfer)
}

/// Link feedback L3: the link page ends ("Linked") as soon as the backend reports the
/// transfer of the account it links (it comes only after the link); the transfer goes on in
/// the banner (the link page has no view of a transfer).
#[test]
fn the_link_page_ends_at_its_accounts_transfer_and_the_banner_shows_it() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);

    // Another account's transfer is not this page's business.
    let ch = src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert!(ch.transfer && !ch.login);
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Progress)
    );

    let ch = src.on_event(&transfer(new, HistoryTransfer::Waiting));
    assert!(ch.transfer && ch.login);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    known(&mut src, &sent, &[ME, new]);
    let v = banner_view(&src, new).expect("the banner shows the transfer");
    assert_eq!((v.account, v.phase), (new, TransferPhase::Waiting));
    assert_eq!(v.skip, "Continue Without History");

    // The same state again: nothing to redraw, nothing sent.
    let ch = src.on_event(&transfer(new, HistoryTransfer::Waiting));
    assert!(!ch.transfer && !ch.login);

    src.on_event(&transfer(new, downloading(12_300_000, Some(45_600_000))));
    let v = banner_view(&src, new).expect("download");
    assert!(
        v.status.ends_with("Downloading message history…"),
        "{}",
        v.status
    );
    assert_eq!(v.detail, "12.3 MB of 45.6 MB");
    assert!(v.progress > 0.26 && v.progress < 0.28);

    // The bytes step back (the backend restarted and counts from the file on disk).
    src.on_event(&transfer(new, downloading(4_000_000, Some(45_600_000))));
    let v = banner_view(&src, new).expect("download");
    assert_eq!(v.detail, "4.0 MB of 45.6 MB");
    assert!(v.progress > 0.08 && v.progress < 0.09);

    src.on_event(&transfer(new, HistoryTransfer::Reading));
    let v = banner_view(&src, new).expect("read");
    assert_eq!(v.phase, TransferPhase::Reading);
    assert!(v.skip.is_empty());
    assert!(!src.skip_transfer(new), "a read cannot be skipped");
    assert!(commands(&sent).is_empty(), "nothing was sent so far");

    // The end: the notice waits, and the backend's own `Done` of the login changes nothing.
    let ch = src.on_event(&transfer(new, HistoryTransfer::Done));
    assert!(ch.transfer);
    assert!(banner_view(&src, new).is_none());
    assert_eq!(commands(&sent), [("dismiss", 5)]);
    assert_eq!(
        src.take_transfer_notices(),
        ["Me: The message history was transferred."],
        "two accounts: the notice names its own"
    );
    assert!(src.take_transfer_notices().is_empty(), "taken once");
    let ch = src.on_event(&UiEvent::LoginStep {
        account: new,
        step: LoginStep {
            step_id: "syncing".into(),
            txn: 0,
            kind: LoginStepKind::Progress {
                label: "Syncing contacts and groups".into(),
                fraction: None,
            },
        },
    });
    assert!(!ch.login, "a later step finds the page ended");
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
}

/// Link feedback L3, a link without a transfer (the phone declined it): the page ends when
/// the core's list has the account Active (it took `AccountReady`), long before the
/// backend's `Done`, and leaving it cancels nothing.
#[test]
fn without_a_transfer_the_link_page_ends_when_the_core_has_the_account() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    known(&mut src, &sent, &[ME]);
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Progress),
        "not in the core's list yet"
    );
    known(&mut src, &sent, &[ME, new]);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    src.cancel_login();
    assert!(commands(&sent).is_empty(), "nothing to cancel");
}

fn left(profiles: u32, groups: u32) -> UiEvent {
    UiEvent::AccountProgress {
        account: AccountId(5),
        progress: kourier_model::AccountProgress {
            profiles_left: profiles,
            groups_left: groups,
            ..kourier_model::AccountProgress::default()
        },
    }
}

/// Link feedback L3: after the link page ended, the profiles and groups a fresh link still
/// fetches show in the banner with their counts, after the transfer's read and in its
/// place when there is none, until the backend says it finished; an account that is offline
/// says so instead, and a logged-out one shows none.
#[test]
fn the_banner_counts_the_profiles_and_groups_a_fresh_link_still_fetches() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    // Under the link page nothing shows.
    let ch = src.on_event(&left(120, 8));
    assert!(ch.transfer);
    assert!(banner_view(&src, new).is_none(), "the link page is up");
    known(&mut src, &sent, &[ME, new]);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    let b = src.transfer_banner(Some(new)).expect("the update shows");
    assert_eq!(b.kind, BannerKind::Updating);
    let v = b.transfer.expect("its view");
    assert_eq!(v.phase, TransferPhase::Updating);
    assert_eq!(
        v.status, "Me: Updating contacts and groups: 120 profiles, 8 groups left",
        "two accounts: it names its own"
    );
    assert!(src.banner(None).is_none(), "not a state of the account");

    // A transfer that runs goes first; after it, the update again.
    src.on_event(&transfer(new, HistoryTransfer::Reading));
    assert_eq!(
        banner_view(&src, new).map(|v| v.phase),
        Some(TransferPhase::Reading)
    );
    src.on_event(&transfer(new, HistoryTransfer::Done));
    src.on_event(&left(30, 2));
    let v = banner_view(&src, new).expect("the update again");
    assert_eq!(v.phase, TransferPhase::Updating);
    assert!((v.progress - 0.75).abs() < 1e-9, "{}", v.progress);

    // Getting new messages says less than the update; being offline says more.
    src.on_event(&UiEvent::ConnState {
        account: new,
        state: ConnState::Syncing { fraction: None },
    });
    assert_eq!(
        src.transfer_banner(Some(new)).map(|b| b.kind),
        Some(BannerKind::Updating)
    );
    src.on_event(&UiEvent::ConnState {
        account: new,
        state: ConnState::Offline { retry_at_ms: None },
    });
    assert!(src.transfer_banner(Some(new)).is_none());
    assert_eq!(
        src.banner(Some(new)).map(|b| b.kind),
        Some(BannerKind::Offline)
    );
    src.on_event(&UiEvent::ConnState {
        account: new,
        state: ConnState::Online { detail: None },
    });

    // Finished: nothing is left to show.
    let ch = src.on_event(&UiEvent::AccountProgress {
        account: new,
        progress: kourier_model::AccountProgress::finished(),
    });
    assert!(ch.transfer);
    assert!(src.transfer_banner(None).is_none());

    // A logout stops the work; nothing shows of it.
    src.on_event(&left(5, 0));
    assert!(src.transfer_banner(Some(new)).is_some());
    src.on_event(&UiEvent::ConnState {
        account: new,
        state: logged_out(),
    });
    assert!(src.transfer_banner(Some(new)).is_none());
}

/// Link feedback L3, F1: the one `finished` report may be lost with a lagged channel (the
/// reload that follows): the update's banner and the question on quitting must not stay up for
/// the rest of the session. A report that changed afterwards brings the numbers back.
#[test]
fn a_reload_forgets_the_numbers_so_a_lost_finished_leaves_nothing_up() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    known(&mut src, &sent, &[ME, new]);
    src.on_event(&left(30, 2));
    assert_eq!(
        src.transfer_banner(Some(new)).map(|b| b.kind),
        Some(BannerKind::Updating)
    );
    assert!(src.quit_question().is_some());

    // The channel lagged: the `finished` that came in between is gone.
    let ch = src.reload();
    assert!(ch.transfer && ch.accounts, "what was shown must be redrawn");
    known(&mut src, &sent, &[ME, new]);
    assert!(src.transfer_banner(Some(new)).is_none(), "no update shown");
    assert!(src.quit_question().is_none(), "nothing runs");
    assert!(!src.reload().transfer, "nothing was left to forget");

    // The next report brings the work back.
    src.on_event(&left(29, 2));
    assert_eq!(
        src.transfer_banner(Some(new)).map(|b| b.kind),
        Some(BannerKind::Updating)
    );
    assert!(src.quit_question().is_some());
}

#[test]
fn a_skip_from_the_banner_ends_without_a_notice_and_is_dismissed() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    src.on_event(&transfer(new, HistoryTransfer::Waiting));
    known(&mut src, &sent, &[ME, new]);
    assert!(src.skip_transfer(new));
    assert_eq!(commands(&sent), [("skip", 5)]);
    assert!(
        banner_view(&src, new).is_some(),
        "the state stays until the backend reports the end"
    );
    // The answer to a skip is the next event.
    let ch = src.on_event(&transfer(new, HistoryTransfer::Skipped));
    assert!(ch.transfer);
    assert!(banner_view(&src, new).is_none());
    assert!(src.take_transfer_notices().is_empty(), "our own skip");
    assert_eq!(commands(&sent), [("dismiss", 5)]);
    // Skipping what does not run sends nothing.
    assert!(!src.skip_transfer(new));
    assert!(!src.skip_transfer(AccountId(77)));
    assert!(commands(&sent).is_empty());
}

#[test]
fn an_end_reported_three_times_is_said_once_and_dismissed_each_time() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    // The phone's user declined: this UI did not skip.
    for n in 0..3 {
        let ch = src.on_event(&transfer(ME, HistoryTransfer::Skipped));
        assert_eq!(ch.transfer, n == 0, "report {n}");
    }
    assert_eq!(
        src.take_transfer_notices(),
        ["The message history was not transferred."]
    );
    assert_eq!(
        commands(&sent),
        [("dismiss", 1), ("dismiss", 1), ("dismiss", 1)],
        "one dismissal per report: an earlier one may have crossed the report"
    );
    assert!(shown(&src).is_none());
}

#[test]
fn after_a_restart_the_banner_shows_the_transfer_and_skips_it() {
    // No login runs: the application started in the middle of a transfer.
    let (mut src, sent) = loaded();
    assert!(shown(&src).is_none());
    let ch = src.on_event(&transfer(ME, downloading(1_000_000, Some(4_000_000))));
    assert!(ch.transfer);
    let b = shown(&src).expect("banner");
    assert_eq!((b.account, b.kind), (ME, BannerKind::Transfer));
    assert!(b.action.is_empty() && b.url.is_empty());
    let v = b.transfer.expect("the transfer's view");
    assert_eq!(v.phase, TransferPhase::Downloading);
    assert_eq!(v.status, "Downloading message history…");
    assert_eq!(b.text, v.status);
    assert_eq!(v.detail, "1.0 MB of 4.0 MB");
    assert_eq!(v.progress, 0.25);
    assert_eq!(v.skip, "Continue Without History");

    assert!(src.skip_transfer(ME));
    assert_eq!(commands(&sent), [("skip", 1)]);
    src.on_event(&transfer(ME, HistoryTransfer::Skipped));
    assert!(shown(&src).is_none());
    assert!(src.take_transfer_notices().is_empty());
    assert_eq!(commands(&sent), [("dismiss", 1)]);
}

#[test]
fn which_banner_wins_beside_a_transfer() {
    let (mut src, _) = loaded();
    let conn = |src: &mut CoreSource, state| {
        src.on_event(&UiEvent::ConnState { account: ME, state });
    };
    let kind = |src: &CoreSource| shown(src).map(|b| b.kind);
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    // The two states that pass by themselves say less than the transfer.
    conn(&mut src, ConnState::Connecting);
    assert_eq!(kind(&src), Some(BannerKind::Transfer));
    conn(&mut src, ConnState::Syncing { fraction: None });
    assert_eq!(kind(&src), Some(BannerKind::Transfer));
    // Whatever cuts the account off, or needs the user now, goes first.
    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    assert_eq!(kind(&src), Some(BannerKind::Offline));
    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: 7_200_000 }),
    );
    assert_eq!(kind(&src), Some(BannerKind::Clock));
    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::SolveChallenge {
            url: "https://challenge.invalid/x".into(),
        }),
    );
    assert_eq!(kind(&src), Some(BannerKind::Challenge));
    // A logged-out account has no transfer left: its backend forgot it with the login.
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    assert_eq!(kind(&src), Some(BannerKind::LoggedOut));
    assert!(shown(&src).expect("banner").transfer.is_none());

    // A failure waits for the user: it goes before "offline", which passes by itself,
    // and behind the states that stop messages now.
    conn(&mut src, ConnState::Online { detail: None });
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Corrupt)));
    assert_eq!(kind(&src), Some(BannerKind::TransferFailed));
    conn(&mut src, ConnState::Offline { retry_at_ms: None });
    assert_eq!(kind(&src), Some(BannerKind::TransferFailed));
    conn(&mut src, ConnState::Syncing { fraction: None });
    assert_eq!(kind(&src), Some(BannerKind::TransferFailed));
    conn(
        &mut src,
        ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: 7_200_000 }),
    );
    assert_eq!(kind(&src), Some(BannerKind::Clock));
    conn(
        &mut src,
        ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    );
    assert_eq!(kind(&src), Some(BannerKind::LoggedOut));
    assert!(!BannerKind::Transfer.passing() && !BannerKind::TransferFailed.passing());
}

#[test]
fn a_failure_stays_until_it_is_dismissed_and_the_dismissal_is_sent_once() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Reading));
    let end = failed(HistoryTransferFailure::Corrupt);
    assert!(src.on_event(&transfer(ME, end)).transfer);
    // Reported again at a connect and at the next start: it stays, nothing is sent.
    for _ in 0..3 {
        assert!(!src.on_event(&transfer(ME, end)).transfer);
    }
    assert!(commands(&sent).is_empty());
    assert!(
        src.take_transfer_notices().is_empty(),
        "a banner, not a notice"
    );
    let b = shown(&src).expect("banner");
    assert_eq!(b.kind, BannerKind::TransferFailed);
    let v = b.transfer.expect("view");
    assert_eq!(v.phase, TransferPhase::Failed);
    assert_eq!(
        b.text,
        "The message history arrived damaged and was not imported. You can transfer it again."
    );
    assert_eq!(
        (v.retry.as_str(), v.dismiss.as_str()),
        ("Try Again…", "Dismiss")
    );

    assert!(src.dismiss_transfer(ME));
    assert!(!src.dismiss_transfer(ME), "a second press does nothing");
    assert_eq!(commands(&sent), [("dismiss", 1)]);
    assert!(shown(&src).is_none());
    // A report that was on its way when the dismissal went out: no banner, and the
    // backend is told again.
    assert!(!src.on_event(&transfer(ME, end)).transfer);
    assert!(shown(&src).is_none());
    assert_eq!(commands(&sent), [("dismiss", 1)]);
    // Dismissing where nothing failed sends nothing.
    assert!(!src.dismiss_transfer(AccountId(9)));
    assert!(commands(&sent).is_empty());
}

#[test]
fn an_old_version_gets_no_try_again() {
    let (mut src, _) = loaded();
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::UpdateApp)));
    let v = shown(&src).and_then(|b| b.transfer).expect("view");
    assert!(v.retry.is_empty());
    assert_eq!(v.dismiss, "Dismiss");
    assert!(v.status.contains("Update Kourier first"));
}

#[test]
fn an_account_removed_while_its_transfer_is_shown_takes_it_along() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    src.on_event(&transfer(new, HistoryTransfer::Waiting));
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Download)));
    known(&mut src, &sent, &[ME, new]);
    assert!(banner_view(&src, new).is_some());
    assert!(banner_view(&src, ME).is_some());

    let ch = src.on_event(&UiEvent::AccountRemoved { account: new });
    assert!(ch.transfer);
    assert!(banner_view(&src, new).is_none());
    assert!(!src.skip_transfer(new), "nothing left to skip");
    let ch = src.on_event(&UiEvent::AccountRemoved { account: ME });
    assert!(ch.transfer);
    assert!(!src.dismiss_transfer(ME));
    // (The account row itself goes with the accounts query that follows.)
    assert!(
        commands(&sent).is_empty(),
        "nothing is sent for an account that is gone"
    );
    // An account that showed nothing changes nothing.
    assert!(
        !src.on_event(&UiEvent::AccountRemoved {
            account: AccountId(8)
        })
        .transfer
    );
}

#[test]
fn leaving_the_link_page_during_a_transfer_abandons_nothing() {
    let (mut src, sent) = loaded();
    linking(&mut src, &sent, ME);
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    // The page ended at the first report (link feedback L3): the banner shows the transfer.
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    assert_eq!(
        shown(&src).map(|b| b.kind),
        Some(BannerKind::Transfer),
        "the transfer goes on in the banner"
    );
    // Leaving the "Linked" page: the account is linked, so there is no login to cancel.
    src.cancel_login();
    assert!(
        commands(&sent).is_empty(),
        "no CancelLogin for a linked account"
    );
    // Before any transfer state the same press does cancel the login.
    linking(&mut src, &sent, AccountId(6));
    src.cancel_login();
    assert_eq!(commands(&sent), [("cancel-login", 6)]);
    // A transfer that ran and ended: the page ended with its first report, and leaving it
    // sends nothing either.
    linking(&mut src, &sent, AccountId(7));
    src.on_event(&transfer(AccountId(7), HistoryTransfer::Reading));
    src.on_event(&transfer(AccountId(7), HistoryTransfer::Done));
    sent.borrow_mut().clear();
    src.cancel_login();
    assert!(commands(&sent).is_empty());
}

#[test]
fn with_several_accounts_the_banner_and_the_notice_name_theirs() {
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let (mut src, _) = with_accounts(vec![me, work]);
    src.on_event(&transfer(AccountId(2), HistoryTransfer::Reading));
    let b = shown(&src).expect("banner");
    assert_eq!((b.account, b.kind), (AccountId(2), BannerKind::Transfer));
    assert!(b.text.starts_with("Work: Importing messages…"));
    assert_eq!(b.transfer.expect("view").status, b.text);
    assert!(
        src.banner(Some(ME)).is_none() && src.transfer_banner(Some(ME)).is_none(),
        "the other account shows nothing"
    );
    assert_eq!(
        src.transfer_banner(Some(AccountId(2))).map(|b| b.account),
        Some(AccountId(2)),
        "and its own account alone shows it too"
    );
    src.on_event(&transfer(AccountId(2), HistoryTransfer::Done));
    assert_eq!(
        src.take_transfer_notices(),
        ["Work: The message history was transferred."]
    );
}

#[test]
fn only_an_account_whose_network_brings_history_offers_the_transfer() {
    let signal = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        network: NetworkKind::Signal,
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let gone = kourier_core::facade::AccountRow {
        id: AccountId(3),
        uuid: AccountUuid::new_random(),
        state: AccountState::LoggedOut {
            reason: "unlinked".into(),
        },
        ..account()
    };
    let (mut src, _) = with_accounts(vec![account(), signal, gone]);
    let offers = |src: &CoreSource| -> Vec<(i64, bool)> {
        src.accounts()
            .iter()
            .map(|a| (a.id.0, a.offers_history))
            .collect()
    };
    // The test network's descriptor says "history is pushed on link"; Signal's does not
    // yet. A logged-out account can be replaced like any other.
    assert_eq!(offers(&src), [(1, true), (2, false), (3, true)]);
    // Not while a transfer of the account runs.
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert_eq!(offers(&src), [(1, false), (2, false), (3, true)]);
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Download)));
    assert_eq!(offers(&src), [(1, true), (2, false), (3, true)]);
}

#[test]
fn only_a_confirmed_question_removes_the_account_and_links_it_again() {
    let (mut src, sent) = loaded();
    // Without a question nothing can be confirmed.
    assert_eq!(src.confirm_replace(ME), Err(SourceError::Unavailable));
    assert!(commands(&sent).is_empty());

    let q = src.ask_replace(ME).expect("question");
    assert_eq!((q.account, q.about.as_str()), (ME, "Me · Test network"));
    assert_eq!(q.confirm, "Remove and Link Again");
    assert!(commands(&sent).is_empty(), "asking sends nothing");

    // Cancel (or Escape): nothing happens, and the question is gone.
    src.cancel_replace();
    assert_eq!(src.confirm_replace(ME), Err(SourceError::Unavailable));
    assert!(commands(&sent).is_empty());

    // A confirmation that names another account than the question (a dialog that outlived
    // its question) removes nothing, and the question stays open.
    src.ask_replace(ME).expect("question");
    assert_eq!(
        src.confirm_replace(AccountId(9)),
        Err(SourceError::Unavailable)
    );
    assert!(commands(&sent).is_empty());

    // Confirmed: remove, then add an account of the same network.
    let view = src.confirm_replace(ME).expect("link page");
    assert_eq!(view.kind, LoginViewKind::Waiting);
    assert_eq!(view.title, "Link Test network");
    assert_eq!(commands(&sent), [("remove", 1), ("add", 0)]);
    // A second activation of the same question does nothing.
    assert_eq!(src.confirm_replace(ME), Err(SourceError::Unavailable));
    assert!(commands(&sent).is_empty());

    // The new account's login is the one the link page shows.
    src.on_event(&UiEvent::AccountAdded {
        account: AccountId(2),
    });
    src.on_event(&UiEvent::AccountRemoved { account: ME });
    src.on_event(&transfer(AccountId(2), HistoryTransfer::Waiting));
    known(&mut src, &sent, &[AccountId(2)]);
    assert_eq!(
        banner_view(&src, AccountId(2)).map(|v| v.account),
        Some(AccountId(2)),
        "the phone is asked for the history"
    );
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
}

#[test]
fn a_question_about_an_account_that_went_away_removes_nothing() {
    let (mut src, sent) = loaded();
    assert!(src.ask_replace(AccountId(42)).is_none(), "no such account");
    src.ask_replace(ME).expect("question");
    src.on_event(&UiEvent::AccountRemoved { account: ME });
    assert_eq!(src.confirm_replace(ME), Err(SourceError::Unavailable));
    assert!(commands(&sent).is_empty());
}

#[test]
fn try_again_of_a_failed_transfer_asks_the_same_question() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::LinkAgain)));
    let v = shown(&src).and_then(|b| b.transfer).expect("view");
    assert_eq!(v.retry, "Try Again…");
    let q = src.ask_replace(v.account).expect("question");
    assert_eq!(q.title, "Remove and Link Again?");
    assert!(commands(&sent).is_empty());
    src.confirm_replace(v.account).expect("link page");
    assert_eq!(commands(&sent), [("remove", 1), ("add", 0)]);
    // The removed account takes its banner along.
    src.on_event(&UiEvent::AccountRemoved { account: ME });
    assert!(shown(&src).is_none());
}

#[test]
fn the_question_tells_accounts_apart_by_name_and_number() {
    let numbered = kourier_core::facade::AccountRow {
        number: Some("+380 44 123 4567".into()),
        display_name: Some("Olha".into()),
        ..account()
    };
    let (mut src, _) = with_accounts(vec![numbered]);
    let q = src.ask_replace(ME).expect("question");
    assert_eq!(q.about, "Olha · +380 44 123 4567");
}

// ---- The independent review (target/h3-context/review.md) ---------------------------
// Its tests, kept under their names. The one that is still ignored shows what was left as
// it is, with the reason.

fn logged_out() -> ConnState {
    ConnState::LoggedOut {
        reason: "unlinked".into(),
    }
}

#[test]
fn review_cancelling_a_relink_is_sent_also_for_an_account_that_once_had_a_transfer() {
    let (mut src, sent) = loaded();
    // The end of the transfer of the first link: announced, dismissed, nothing is shown.
    src.on_event(&transfer(ME, HistoryTransfer::Done));
    assert_eq!(commands(&sent), [("dismiss", 1)]);
    // Later the phone unlinks this device; the user presses "Link Again", then "Cancel".
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: logged_out(),
    });
    src.relink(ME).expect("relink");
    assert_eq!(commands(&sent), [("relink", 1)]);
    src.cancel_login();
    assert_eq!(
        commands(&sent),
        [("cancel-login", 1)],
        "the relink that runs in the backend (its codes, its socket) is abandoned"
    );
}

#[test]
fn review_a_transfer_the_backend_forgot_with_the_login_does_not_come_back_after_a_relink() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert_eq!(shown(&src).map(|b| b.kind), Some(BannerKind::Transfer));
    // The phone unlinks the device in the middle: the backend deletes the transfer with
    // the login (`transfer::forget`) and reports nothing about it.
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: logged_out(),
    });
    assert_eq!(shown(&src).map(|b| b.kind), Some(BannerKind::LoggedOut));
    // "Link Again" (a relink has no transfer), linked, online.
    src.relink(ME).expect("relink");
    src.on_event(&UiEvent::LoginStep {
        account: ME,
        step: LoginStep {
            step_id: "done".into(),
            txn: 0,
            kind: LoginStepKind::Done,
        },
    });
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::Online { detail: None },
    });
    sent.borrow_mut().clear();
    assert_eq!(
        shown(&src).map(|b| b.kind),
        None,
        "no transfer runs: nothing may say \"Waiting for your phone…\" with a button that ends nothing"
    );
    assert!(
        src.accounts().iter().all(|a| a.offers_history),
        "and the account offers the transfer again"
    );
}

#[test]
fn the_page_of_a_relink_keeps_to_its_steps_whatever_is_heard_of_a_transfer_the_backend_forgot() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: logged_out(),
    });
    // "Link Again": the page shows the codes of this login. A relink has no transfer, and
    // the one the book still holds was deleted by the backend with the login.
    src.relink(ME).expect("relink");
    // Whatever is heard of a transfer of this account while its relink is on the page
    // (nothing should be: a relink has none), the page keeps to the login's steps and its
    // "Cancel" reaches the backend.
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert_ne!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Done),
        "the relink's page ends at its transfer"
    );
    sent.borrow_mut().clear();
    src.cancel_login();
    assert_eq!(commands(&sent), [("cancel-login", 1)]);
}

#[test]
fn an_account_found_logged_out_at_the_start_has_no_running_transfer() {
    // The state event of the logout was not seen (it came while the application was
    // closed, or the event channel lagged): the accounts answer says it.
    let (mut src, sent) = loaded();
    for state in [
        HistoryTransfer::Waiting,
        downloading(1, Some(2)),
        HistoryTransfer::Reading,
    ] {
        src.on_event(&transfer(ME, state));
        assert!(src.skip_transfer(ME) || state == HistoryTransfer::Reading);
        let gone = kourier_core::facade::AccountRow {
            state: AccountState::LoggedOut {
                reason: "unlinked".into(),
            },
            ..account()
        };
        src.reload();
        let store = Store::new(&[(1, 2)]);
        let mut changed = false;
        for (t, q) in take_queries(&sent) {
            let data = match q {
                Query::Accounts => QueryData::Accounts(vec![gone.clone()]),
                other => store.answer(&other),
            };
            changed |= src.on_event(&result(t, data)).transfer;
        }
        assert!(
            changed,
            "{state:?}: what the window shows of the transfer changed"
        );
        sent.borrow_mut().clear();
        assert!(!src.skip_transfer(ME), "{state:?}: nothing left to skip");
        assert!(commands(&sent).is_empty());
        // Linked again, online: nothing of the old transfer is back.
        src.on_event(&UiEvent::ConnState {
            account: ME,
            state: ConnState::Online { detail: None },
        });
        let again = kourier_core::facade::AccountRow {
            conn: ConnState::Online { detail: None },
            ..account()
        };
        src.reload();
        for (t, q) in take_queries(&sent) {
            let data = match q {
                Query::Accounts => QueryData::Accounts(vec![again.clone()]),
                other => store.answer(&other),
            };
            src.on_event(&result(t, data));
        }
        assert!(shown(&src).is_none(), "{state:?}");
        assert!(src.accounts().iter().all(|a| a.offers_history));
    }
    // A failure nobody dismissed is still shown after the logout is over: it happened.
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Corrupt)));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: logged_out(),
    });
    assert_eq!(shown(&src).map(|b| b.kind), Some(BannerKind::LoggedOut));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::Online { detail: None },
    });
    assert_eq!(
        shown(&src).map(|b| b.kind),
        Some(BannerKind::TransferFailed)
    );
}

#[test]
fn review_a_failed_transfer_is_not_hidden_behind_another_accounts_state() {
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        state: AccountState::LoggedOut {
            reason: "unlinked".into(),
        },
        ..account()
    };
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let (mut src, sent) = with_accounts(vec![me, work]);
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Download)));
    // The user leaves "Work" logged out for weeks. What the window shows (all accounts):
    // the state of "Work" AND the failure of "Me", each with its own actions.
    let state = src.banner(None).expect("the state's banner");
    assert_eq!(
        (state.account, state.kind),
        (AccountId(2), BannerKind::LoggedOut)
    );
    assert!(state.transfer.is_none());
    let shown = src.transfer_banner(None).expect("the transfer's banner");
    assert_eq!(
        (shown.account, shown.kind),
        (ME, BannerKind::TransferFailed),
        "the failure, with its \"Try Again…\" and \"Dismiss\", must be reachable somewhere"
    );
    let v = shown.transfer.expect("view");
    assert_eq!(
        (v.account, v.retry.as_str(), v.dismiss.as_str()),
        (ME, "Try Again…", "Dismiss")
    );
    assert!(
        v.status
            .starts_with("Me: The message history could not be downloaded.")
    );
    // "Dismiss" reaches the account of the failure, and the other banner stays.
    assert!(src.dismiss_transfer(v.account));
    assert_eq!(commands(&sent), [("dismiss", 1)]);
    assert!(src.transfer_banner(None).is_none());
    assert_eq!(
        src.banner(None).map(|b| b.kind),
        Some(BannerKind::LoggedOut)
    );
}

#[test]
fn review_the_skip_of_a_running_transfer_stays_in_reach_beside_another_accounts_state() {
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        conn: ConnState::Offline { retry_at_ms: None },
        ..account()
    };
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Syncing { fraction: None },
        ..account()
    };
    let (mut src, sent) = with_accounts(vec![me, work]);
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    // "Work" is offline and says so; the transfer of "Me" shows beside it. ("Me" is also
    // "getting new messages": its transfer says more, so that line is not shown.)
    let state = src.banner(None).expect("the state's banner");
    assert_eq!(
        (state.account, state.kind),
        (AccountId(2), BannerKind::Offline)
    );
    let shown = src.transfer_banner(None).expect("the transfer's banner");
    assert_eq!((shown.account, shown.kind), (ME, BannerKind::Transfer));
    let v = shown.transfer.expect("view");
    assert_eq!(v.skip, "Continue Without History");
    assert!(src.skip_transfer(v.account));
    assert_eq!(commands(&sent), [("skip", 1)]);
}

#[test]
fn one_account_never_shows_its_state_and_its_transfer_together() {
    // Within ONE account the order of `BannerKind` decides, as before: the account banner
    // has one line for it. (`shown` fails when both sorts are there.)
    let (mut src, _) = loaded();
    for transfer_state in [
        HistoryTransfer::Waiting,
        failed(HistoryTransferFailure::Corrupt),
    ] {
        src.on_event(&transfer(ME, transfer_state));
        for conn in [
            ConnState::Connecting,
            ConnState::Syncing { fraction: None },
            ConnState::Offline { retry_at_ms: None },
            ConnState::NeedsUserAction(UserAction::FixClock { skew_ms: 7_200_000 }),
            ConnState::Online { detail: None },
        ] {
            src.on_event(&UiEvent::ConnState {
                account: ME,
                state: conn,
            });
            assert!(shown(&src).is_some(), "{transfer_state:?}");
        }
    }
}

#[test]
fn among_the_transfers_of_several_accounts_a_failed_one_goes_first() {
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let (mut src, _) = with_accounts(vec![me, work]);
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    src.on_event(&transfer(
        AccountId(2),
        failed(HistoryTransferFailure::Corrupt),
    ));
    assert!(src.banner(None).is_none(), "no account has a state to show");
    let first = src.transfer_banner(None).expect("banner");
    assert_eq!(
        (first.account, first.kind),
        (AccountId(2), BannerKind::TransferFailed)
    );
    // Dismissed, the running one of the other account shows with its skip; and each
    // account alone (picked in the drawer) shows its own.
    assert_eq!(
        src.transfer_banner(Some(ME)).map(|b| b.kind),
        Some(BannerKind::Transfer)
    );
    assert!(src.dismiss_transfer(AccountId(2)));
    let next = src.transfer_banner(None).expect("banner");
    assert_eq!((next.account, next.kind), (ME, BannerKind::Transfer));
}

#[test]
#[ignore = "review, left as it is: the end of a skip this UI sent is announced as \"not transferred\" when the application was restarted before the backend reported it (the mark is memory only; keeping it needs a state file of the UI)"]
fn review_our_own_skip_is_not_announced_after_a_restart() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert!(src.skip_transfer(ME));
    assert_eq!(commands(&sent), [("skip", 1)]);
    // The application is closed before the end arrives; the next start has a new source.
    let (mut src, _) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Skipped));
    assert!(
        src.take_transfer_notices().is_empty(),
        "the user pressed \"Continue Without History\" himself"
    );
}

#[test]
fn review_states_in_any_order_are_taken_as_the_state_now() {
    let (mut src, sent) = loaded();
    // Reading right after Waiting (the download was not reported), then the size of a
    // download appears and disappears, the bytes step back, and it waits again.
    for (state, phase, skip) in [
        (HistoryTransfer::Waiting, TransferPhase::Waiting, true),
        (HistoryTransfer::Reading, TransferPhase::Reading, false),
        (downloading(9, None), TransferPhase::Downloading, true),
        (downloading(9, Some(10)), TransferPhase::Downloading, true),
        (downloading(3, None), TransferPhase::Downloading, true),
        (HistoryTransfer::Reading, TransferPhase::Reading, false),
        (HistoryTransfer::Waiting, TransferPhase::Waiting, true),
    ] {
        // Each state three times: once it changes something, then never.
        let changed: Vec<bool> = (0..3)
            .map(|_| src.on_event(&transfer(ME, state)).transfer)
            .collect();
        assert_eq!(changed, [true, false, false], "{state:?}");
        let v = shown(&src).and_then(|b| b.transfer).expect("view");
        assert_eq!((v.phase, !v.skip.is_empty()), (phase, skip), "{state:?}");
        assert_eq!(src.skip_transfer(ME), skip, "{state:?}");
    }
    assert!(src.take_transfer_notices().is_empty());
    assert!(
        commands(&sent).iter().all(|c| c.0 == "skip"),
        "no dismissal while a transfer runs"
    );
}

#[test]
fn review_an_end_found_at_every_start_is_said_once_per_start_and_a_failure_survives_them() {
    // Three starts of the application, each with a new source; the backend repeats the end
    // at the start and at the connect.
    for end in [HistoryTransfer::Done, HistoryTransfer::Skipped] {
        let (mut src, sent) = loaded();
        for _ in 0..2 {
            src.on_event(&transfer(ME, end));
        }
        assert_eq!(src.take_transfer_notices().len(), 1, "{end:?}");
        assert_eq!(commands(&sent), [("dismiss", 1), ("dismiss", 1)], "{end:?}");
        assert!(shown(&src).is_none());
    }
    // A failure nobody dismissed: every start shows it again, and nothing is sent.
    for _ in 0..3 {
        let (mut src, sent) = loaded();
        for _ in 0..2 {
            src.on_event(&transfer(ME, failed(HistoryTransferFailure::TooLarge)));
        }
        assert_eq!(
            shown(&src).map(|b| b.kind),
            Some(BannerKind::TransferFailed)
        );
        assert!(commands(&sent).is_empty());
        assert!(src.take_transfer_notices().is_empty());
    }
}

#[test]
fn review_two_accounts_with_transfers_keep_their_own_ends_and_skips() {
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let (mut src, sent) = with_accounts(vec![me, work]);
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    src.on_event(&transfer(AccountId(2), downloading(1, Some(2))));
    assert!(src.skip_transfer(AccountId(2)));
    // The first account's phone declines, the second ends by our skip.
    src.on_event(&transfer(ME, HistoryTransfer::Skipped));
    src.on_event(&transfer(AccountId(2), HistoryTransfer::Skipped));
    assert_eq!(
        src.take_transfer_notices(),
        ["Me: The message history was not transferred."]
    );
    assert_eq!(
        commands(&sent),
        [("skip", 2), ("dismiss", 1), ("dismiss", 2)]
    );
}

#[test]
fn review_a_confirmation_cannot_reach_another_account_than_the_one_asked_about() {
    let work = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let (mut src, sent) = with_accounts(vec![account(), work]);
    // The question about the first account is open; a second question is asked for (the
    // window shows only one): the open dialog's confirmation is void, nothing is removed.
    src.ask_replace(ME).expect("question");
    src.ask_replace(AccountId(2)).expect("question");
    assert_eq!(src.confirm_replace(ME), Err(SourceError::Unavailable));
    assert!(commands(&sent).is_empty());
    // The account asked about last is removed elsewhere meanwhile: void as well.
    src.on_event(&UiEvent::AccountRemoved {
        account: AccountId(2),
    });
    assert_eq!(
        src.confirm_replace(AccountId(2)),
        Err(SourceError::Unavailable)
    );
    assert!(commands(&sent).is_empty());
}

#[test]
fn review_confirming_while_another_link_runs_abandons_that_link() {
    // The link of a second account is in progress (wide window: its page beside the list),
    // and "Try Again…" on the banner of the first account is confirmed.
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Corrupt)));
    linking(&mut src, &sent, AccountId(6));
    let q = src.ask_replace(ME).expect("question");
    // F3: the question says that the link in progress is cancelled.
    assert_eq!(
        q.paragraphs.last().map(String::as_str),
        Some(
            "Linking another Test network account has not finished. Removing this account now cancels that link. You can link that account again later."
        )
    );
    src.confirm_replace(ME).expect("link page");
    assert_eq!(
        commands(&sent),
        [("cancel-login", 6), ("remove", 1), ("add", 0)]
    );
}

#[test]
fn transfer_history_is_not_offered_while_the_account_still_links() {
    // `buttons.md` F4: after its transfer a link goes on (contacts, groups, profiles) until
    // its `Done`; offering the transfer then would delete what just arrived.
    let offers = |src: &CoreSource, id: AccountId| {
        src.accounts()
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.offers_history)
    };
    let new = AccountId(2);
    let row = kourier_core::facade::AccountRow {
        id: new,
        uuid: AccountUuid::new_random(),
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    // The core has announced the account (rows are asked for at `AccountAdded`).
    let (mut src, sent) = with_accounts(vec![me.clone(), row.clone()]);
    let step = |kind| UiEvent::LoginStep {
        account: new,
        step: LoginStep {
            step_id: "linking".into(),
            txn: 0,
            kind,
        },
    };
    let progress = || {
        step(LoginStepKind::Progress {
            label: "Syncing contacts and groups".into(),
            fraction: None,
        })
    };
    let answer_accounts = |src: &mut CoreSource| {
        for (t, q) in take_queries(&sent) {
            if matches!(q, Query::Accounts) {
                src.on_event(&result(
                    t,
                    QueryData::Accounts(vec![me.clone(), row.clone()]),
                ));
            }
        }
    };
    linking(&mut src, &sent, new);
    assert_eq!(offers(&src, new), Some(false), "at the QR");
    src.on_event(&transfer(new, HistoryTransfer::Reading));
    assert_eq!(offers(&src, new), Some(false), "during the transfer");
    src.on_event(&transfer(new, HistoryTransfer::Done));
    src.on_event(&progress());
    assert_eq!(offers(&src, new), Some(false), "in the tail, the page up");
    // The page is left in the tail (its Cancel): the link goes on in the background.
    src.cancel_login();
    src.on_event(&progress());
    assert_eq!(offers(&src, new), Some(false), "in the tail, the page left");
    assert_eq!(offers(&src, ME), Some(true), "the other account offers it");
    src.on_event(&step(LoginStepKind::Done));
    answer_accounts(&mut src);
    assert_eq!(offers(&src, new), Some(true), "once the link is done");
    // A login that started again and was logged out by the phone has nothing to protect.
    src.on_event(&progress());
    assert_eq!(offers(&src, new), Some(false));
    src.on_event(&UiEvent::ConnState {
        account: new,
        state: logged_out(),
    });
    assert_eq!(offers(&src, new), Some(true), "a logged-out account");
    // A removed account forgets its unfinished link.
    src.on_event(&UiEvent::AccountRemoved { account: new });
    assert!(!src.unfinished_links.contains(&new));
}

#[test]
fn an_action_that_would_cancel_a_link_in_progress_asks_first() {
    // `buttons.md` F3: Settings, "Link Again" or "Remove and Link Again" cancel the login of
    // the link page; the window asks first when that loses a link.
    // The first account is logged out: "Link Again" is what the banner offers for it.
    let (mut src, sent) = with_accounts(vec![logged_out_account()]);
    assert!(src.abandon_question().is_none(), "no login");
    assert_eq!(
        src.ask_replace(ME).expect("question").paragraphs.len(),
        3,
        "no link to mention"
    );
    src.cancel_replace();
    linking(&mut src, &sent, AccountId(6));
    let q = src.abandon_question().expect("at the QR code");
    assert_eq!(
        (q.title.as_str(), q.confirm.as_str(), q.cancel.as_str()),
        ("Stop Linking?", "Stop Linking", "Continue Linking")
    );
    // The text names what cancels the link. The dialog's default button says "Continue
    // Linking", so the text must not say that going on cancels it (user review of L1).
    assert_eq!(
        q.text,
        "Linking a Test network account has not finished. Opening this now cancels the link. You can link the account again later."
    );
    assert!(!q.text.contains("Going on"), "{}", q.text);
    // Linked (its transfer runs or ran): leaving the page cancels nothing.
    src.on_event(&transfer(AccountId(6), HistoryTransfer::Waiting));
    assert!(src.abandon_question().is_none(), "transfer waiting");
    src.on_event(&transfer(AccountId(6), HistoryTransfer::Reading));
    src.on_event(&transfer(AccountId(6), HistoryTransfer::Done));
    assert!(src.abandon_question().is_none(), "in the tail");
    // Asking does not cancel; the answer does.
    assert!(!commands(&sent).iter().any(|c| c.0 == "cancel-login"));
    // "Link Again" can be stopped at any step.
    src.relink(ME).expect("relink");
    assert!(src.abandon_question().is_some(), "a relink");
    src.cancel_login();
    // (The page of 6 ended at its transfer, link feedback L3: no cancel goes out for it.)
    assert_eq!(commands(&sent), [("relink", 1), ("cancel-login", 1)]);
    assert!(src.abandon_question().is_none(), "after the cancel");
}

/// The first account of the tests, logged out by the phone: its banner offers "Link Again".
fn logged_out_account() -> kourier_core::facade::AccountRow {
    kourier_core::facade::AccountRow {
        state: AccountState::LoggedOut {
            reason: "unlinked".into(),
        },
        conn: logged_out(),
        ..account()
    }
}

/// Answers the accounts query the source asked for with `rows` (and the others with nothing).
fn answer_accounts(src: &mut CoreSource, sent: &Sent, rows: &[kourier_core::facade::AccountRow]) {
    for (t, q) in take_queries(sent) {
        if matches!(q, Query::Accounts) {
            src.on_event(&result(t, QueryData::Accounts(rows.to_vec())));
        }
    }
}

fn progress_step(account: AccountId, label: &str) -> UiEvent {
    UiEvent::LoginStep {
        account,
        step: LoginStep {
            step_id: "syncing".into(),
            txn: 0,
            kind: LoginStepKind::Progress {
                label: label.into(),
                fraction: None,
            },
        },
    }
}

#[test]
fn a_fresh_link_without_a_transfer_asks_nothing_once_the_core_has_the_account() {
    // The phone declined the history: no transfer is ever reported. The core takes
    // `AccountReady` (the account turns Active) and the link page shows the tail ("Syncing
    // contacts and groups") for minutes. A cancel is dropped for a linked account
    // (`supervisor.rs` `Control::CancelLogin if !linked`), so a question "Stop Linking?" there
    // would promise a cancel that never happens (user and correctness reviews of L1).
    let new = AccountId(2);
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let row = |state| kourier_core::facade::AccountRow {
        id: new,
        uuid: AccountUuid::new_random(),
        state,
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let (mut src, sent) = with_accounts(vec![me.clone()]);
    src.start_login(NetworkKind::Fake).expect("start");
    // The core announces the account (state Linking) and the source asks for the rows.
    src.on_event(&UiEvent::AccountAdded { account: new });
    answer_accounts(&mut src, &sent, &[me.clone(), row(AccountState::Linking)]);
    src.on_event(&progress_step(new, "Linking"));
    assert!(src.abandon_question().is_some(), "at the QR code");
    assert!(
        src.abandon_question().is_some(),
        "linking, the core has not the account"
    );
    // `AccountReady` is taken: the core's account list says Active.
    src.on_event(&UiEvent::AccountsChanged);
    answer_accounts(&mut src, &sent, &[me.clone(), row(AccountState::Active)]);
    assert!(
        src.abandon_question().is_none(),
        "the account is Active: leaving the page cancels nothing"
    );
    src.on_event(&progress_step(new, "Syncing contacts and groups"));
    assert!(src.abandon_question().is_none(), "in the tail");
    assert!(
        !commands(&sent).iter().any(|c| c.0 == "cancel-login"),
        "asking cancels nothing"
    );
}

#[test]
fn a_link_again_asks_nothing_once_the_core_has_the_account_again() {
    // "Link Again" of a logged-out account: the account is LoggedOut while the phone
    // scans, Active from `AccountReady` on (the tail follows).
    let (mut src, sent) = with_accounts(vec![logged_out_account()]);
    src.relink(ME).expect("relink");
    assert!(src.abandon_question().is_some(), "at the QR code");
    src.on_event(&progress_step(ME, "Linking"));
    assert!(src.abandon_question().is_some(), "linking");
    src.on_event(&UiEvent::AccountsChanged);
    let active = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    answer_accounts(&mut src, &sent, &[active]);
    // The row says Active, the live state still says logged out: the page stays.
    assert!(
        src.abandon_question().is_some(),
        "the logout is the newer word"
    );
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Progress)
    );
    // The relinked account connects: the core has it, and the page ends (link feedback L3).
    let ch = src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::Connecting,
    });
    assert!(ch.login);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    src.on_event(&progress_step(ME, "Syncing contacts and groups"));
    assert!(
        src.abandon_question().is_none(),
        "the relinked account is Active: nothing to cancel"
    );
}

fn failed_step(account: AccountId) -> UiEvent {
    UiEvent::LoginStep {
        account,
        step: LoginStep {
            step_id: "already-added".into(),
            txn: 0,
            kind: LoginStepKind::Failed {
                error: kourier_model::BackendError::new(
                    kourier_model::ErrorKind::AlreadyAdded { logged_out: false },
                    "already".to_owned(),
                ),
            },
        },
    }
}

fn done_step(account: AccountId) -> UiEvent {
    UiEvent::LoginStep {
        account,
        step: LoginStep {
            step_id: "done".into(),
            txn: 0,
            kind: LoginStepKind::Done,
        },
    }
}

/// Link feedback L3: the backend's own `Done` of a second link of an account that is here
/// can come before the core refuses it: the refusal still shows (`link_actions_ui`
/// `already`). Once the page ended because the core has the account, a later failure of the
/// backend's tail does not take the page back.
#[test]
fn a_refusal_after_the_backends_done_shows_but_not_after_the_core_took_the_account() {
    let me = kourier_core::facade::AccountRow {
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let new = AccountId(2);
    let (mut src, sent) = with_accounts(vec![me.clone()]);
    src.start_login(NetworkKind::Fake).expect("start");
    src.on_event(&UiEvent::AccountAdded { account: new });
    src.on_event(&done_step(new));
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    let ch = src.on_event(&failed_step(new));
    assert!(ch.login);
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Failed)
    );

    // The core took the account: the page ended, a later failure changes nothing.
    let other = AccountId(3);
    src.start_login(NetworkKind::Fake).expect("start again");
    src.on_event(&UiEvent::AccountAdded { account: other });
    src.on_event(&transfer(other, HistoryTransfer::Waiting));
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    let ch = src.on_event(&failed_step(other));
    assert!(!ch.login);
    assert_eq!(src.login_view().map(|v| v.kind), Some(LoginViewKind::Done));
    drop(sent);
}

/// F8: closing the window while a transfer runs or a new link still fetches its contacts
/// and groups asks first; after it (or after a failure) it quits at once.
#[test]
fn quitting_asks_while_a_transfer_or_the_update_after_a_link_runs() {
    let (mut src, sent) = loaded();
    assert!(src.quit_question().is_none(), "nothing runs");
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    src.on_event(&transfer(new, HistoryTransfer::Waiting));
    known(&mut src, &sent, &[ME, new]);
    let q = src
        .quit_question()
        .expect("a transfer that waits for the phone");
    assert_eq!(q.title, "Quit While Importing History?");
    assert_eq!(
        q.text,
        "Kourier is still importing your message history. It goes on at the next start."
    );
    assert_eq!((q.confirm.as_str(), q.cancel.as_str()), ("Quit", "Cancel"));
    src.on_event(&transfer(new, HistoryTransfer::Reading));
    assert_eq!(
        src.quit_question().map(|q| q.title),
        Some("Quit While Importing History?".to_owned())
    );
    // The update after it: asked with its own words, also while a transfer is reported.
    src.on_event(&left(30, 2));
    assert_eq!(
        src.quit_question().map(|q| q.title),
        Some("Quit While Importing History?".to_owned()),
        "the transfer goes first"
    );
    src.on_event(&transfer(new, HistoryTransfer::Done));
    let q = src.quit_question().expect("the update after the transfer");
    assert_eq!(q.title, "Quit While Updating Contacts and Groups?");
    assert_eq!(
        q.text,
        "Kourier is still fetching your contacts and groups. It goes on at the next start."
    );
    src.on_event(&UiEvent::AccountProgress {
        account: new,
        progress: kourier_model::AccountProgress::finished(),
    });
    assert!(src.quit_question().is_none(), "finished");
    // A failure that waits to be dismissed is not work that goes on.
    src.on_event(&transfer(
        new,
        HistoryTransfer::Failed(HistoryTransferFailure::Download),
    ));
    assert!(src.quit_question().is_none(), "a failure");
    // A logged-out account does nothing more, whatever was last heard of its work.
    src.on_event(&transfer(new, HistoryTransfer::Waiting));
    src.on_event(&left(5, 0));
    assert!(src.quit_question().is_some());
    src.on_event(&UiEvent::ConnState {
        account: new,
        state: logged_out(),
    });
    assert!(src.quit_question().is_none(), "logged out");
}

// ---------- "Remove Account…" and "Unlink…" (link feedback L4) ----------

#[test]
fn a_plain_removal_needs_its_own_question_and_cancels_no_other_login() {
    let (mut src, sent) = loaded();
    // Without a question nothing is removed, and neither by another action's question.
    assert!(!src.confirm_account_action(ME, AccountAction::Remove));
    src.ask_account_action(ME, AccountAction::Unlink)
        .expect("unlink question");
    assert!(!src.confirm_account_action(ME, AccountAction::Remove));
    // The question of "Transfer History from Phone…" is not this one either.
    src.ask_replace(ME).expect("replace question");
    assert!(!src.confirm_account_action(ME, AccountAction::Remove));
    assert_eq!(commands(&sent), []);
    // A link of another account waits at its QR code: the removal leaves it alone.
    linking(&mut src, &sent, AccountId(6));
    let q = src
        .ask_account_action(ME, AccountAction::Remove)
        .expect("question");
    assert_eq!(q.title, "Remove Account?");
    assert_eq!(q.about, "Me · Test network");
    // Asking closed the replace question: it cannot be confirmed behind this one's back.
    assert!(src.confirm_replace(ME).is_err());
    assert!(src.confirm_account_action(ME, AccountAction::Remove));
    assert_eq!(commands(&sent), [("remove", 1)]);
    // Taken once.
    assert!(!src.confirm_account_action(ME, AccountAction::Remove));
    assert_eq!(commands(&sent), []);
}

#[test]
fn an_unlink_needs_its_own_question_and_cancels_no_other_login() {
    let (mut src, sent) = loaded();
    linking(&mut src, &sent, AccountId(6));
    src.ask_account_action(ME, AccountAction::Remove)
        .expect("remove question");
    assert!(!src.confirm_account_action(ME, AccountAction::Unlink));
    let q = src
        .ask_account_action(ME, AccountAction::Unlink)
        .expect("question");
    assert_eq!(q.confirm, "Unlink");
    assert!(src.confirm_account_action(ME, AccountAction::Unlink));
    assert_eq!(commands(&sent), [("unlink", 1)]);
    assert!(!src.confirm_account_action(ME, AccountAction::Unlink));
    // Cancelled: nothing.
    src.ask_account_action(ME, AccountAction::Unlink)
        .expect("question");
    src.cancel_account_action();
    assert!(!src.confirm_account_action(ME, AccountAction::Unlink));
    assert_eq!(commands(&sent), []);
}

#[test]
fn a_logged_out_account_offers_removal_but_no_unlink() {
    let (mut src, sent) = loaded();
    assert!(src.accounts().iter().all(|a| a.offers_unlink));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    });
    assert!(src.accounts().iter().all(|a| !a.offers_unlink));
    assert!(src.ask_account_action(ME, AccountAction::Unlink).is_none());
    src.ask_account_action(ME, AccountAction::Remove)
        .expect("question");
    assert!(src.confirm_account_action(ME, AccountAction::Remove));
    assert_eq!(commands(&sent), [("remove", 1)]);
}

#[test]
fn an_unlink_asked_before_a_logout_sends_nothing_after_it() {
    let (mut src, sent) = loaded();
    src.ask_account_action(ME, AccountAction::Unlink)
        .expect("question");
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    });
    assert!(!src.confirm_account_action(ME, AccountAction::Unlink));
    assert_eq!(commands(&sent), []);
}

#[test]
fn a_removed_account_closes_its_question() {
    let (mut src, sent) = loaded();
    src.ask_account_action(ME, AccountAction::Remove)
        .expect("question");
    src.on_event(&UiEvent::AccountRemoved { account: ME });
    assert!(!src.confirm_account_action(ME, AccountAction::Remove));
    assert_eq!(commands(&sent), []);
}

fn label(
    name: Option<&str>,
    twin: bool,
    device: Option<u32>,
    linked_at: i64,
) -> kourier_core::facade::AccountLabel {
    kourier_core::facade::AccountLabel {
        network: NetworkKind::Fake,
        display_name: name.map(Into::into),
        number: None,
        own_device: device,
        linked_at,
        twin,
    }
}

fn outcome(
    seq: u64,
    removed: bool,
    report: kourier_model::LogoutReport,
    who: kourier_core::facade::AccountLabel,
) -> UiEvent {
    UiEvent::LogoutOutcome {
        seq,
        account: ME,
        removed,
        report,
        who,
    }
}

#[test]
fn the_outcome_of_a_removal_is_told_once() {
    use kourier_model::{DeviceLogout, LogoutReport};
    let (mut src, _sent) = loaded();
    let event = outcome(
        1,
        true,
        LogoutReport::new(DeviceLogout::NotReached),
        label(Some("Me"), false, Some(2), 0),
    );
    let ch = src.on_event(&event);
    assert!(ch.account_notice);
    let notices = src.take_logout_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].text,
        "The account “Me” was removed from this computer."
    );
    assert_eq!(
        notices[0].on_phone,
        "Test network could not be reached: remove this computer from the linked devices on your phone."
    );
    assert!(src.take_logout_notices().is_empty());
    // The core tells an outcome it gave before `UiReady` once more at it: not shown twice.
    let again = src.on_event(&event);
    assert!(!again.account_notice);
    assert!(src.take_logout_notices().is_empty());
    src.on_event(&UiEvent::AccountRemoved { account: ME });
    assert!(src.take_logout_notices().is_empty());
}

/// An outcome told live after the front-end subscribed can have a higher number than the ones
/// only the replay at `UiReady` brings: those are still told, each once.
#[test]
fn a_replay_after_a_newer_live_outcome_still_tells_the_older_ones() {
    use kourier_model::{DeviceLogout, LogoutReport};
    let (mut src, _sent) = loaded();
    let ev = |seq| {
        outcome(
            seq,
            true,
            LogoutReport::new(DeviceLogout::NotReached),
            label(Some("Me"), false, Some(2), 0),
        )
    };
    src.on_event(&ev(3));
    for seq in [1, 2, 3] {
        src.on_event(&ev(seq));
    }
    assert_eq!(
        src.take_logout_notices().len(),
        3,
        "1 and 2 once each, 3 once"
    );
}

/// F1: a removal the start finished tells its outcome before the account list is loaded and
/// after the row is gone; the notice still names the account, from the event.
#[test]
fn a_notice_names_its_account_from_the_event_not_from_the_list() {
    use kourier_model::{DeviceLogout, LogoutReport};
    let (mut src, _sent) = source();
    // Nothing loaded: no account list at all.
    assert!(src.accounts().is_empty());
    src.on_event(&outcome(
        4,
        true,
        LogoutReport::new(DeviceLogout::Removed),
        label(Some("Ann"), false, Some(2), 0),
    ));
    let notices = src.take_logout_notices();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].text,
        "The account “Ann” was removed from this computer."
    );
    // A profile without a name is told by its network, never by nothing.
    src.on_event(&outcome(
        5,
        false,
        LogoutReport::new(DeviceLogout::Removed),
        label(None, false, None, 0),
    ));
    assert_eq!(
        src.take_logout_notices()[0].text,
        "This computer was unlinked from the account “Test network”. Its chats stay readable here."
    );
}

#[test]
fn two_links_of_one_identity_are_told_apart_by_their_device_and_day() {
    // D10: a profile from before one identity was one account holds two that look the same.
    // The day is what the phone lists beside each linked device (the clock of these tests is
    // 2025-10-09, UTC).
    let first = kourier_core::facade::AccountRow {
        own_device: Some(2),
        linked_at: 1_757_678_400_000, // 2025-09-12
        conn: ConnState::Online { detail: None },
        ..account()
    };
    let twin = kourier_core::facade::AccountRow {
        id: AccountId(2),
        uuid: AccountUuid::new_random(),
        own_device: Some(3),
        linked_at: 1_709_640_000_000, // 2024-03-05, another year
        ..first.clone()
    };
    let other = kourier_core::facade::AccountRow {
        id: AccountId(3),
        uuid: AccountUuid::new_random(),
        display_name: Some("Work".into()),
        own_device: Some(4),
        ..first.clone()
    };
    let (mut src, sent) = with_accounts(vec![first, twin, other]);
    let devices: Vec<_> = src.accounts().into_iter().map(|a| a.device).collect();
    assert_eq!(
        devices,
        [
            "device 2 · linked 12 Sep",
            "device 3 · linked 5 Mar 2024",
            ""
        ]
    );
    let q = src
        .ask_account_action(AccountId(2), AccountAction::Remove)
        .expect("question");
    assert_eq!(q.about, "Me · Test network · device 3 · linked 5 Mar 2024");
    assert!(src.confirm_account_action(AccountId(2), AccountAction::Remove));
    assert_eq!(commands(&sent), [("remove", 2)]);
    // An account that looks like no other says no device.
    let q = src
        .ask_account_action(AccountId(3), AccountAction::Remove)
        .expect("question");
    assert_eq!(q.about, "Work · Test network");
}

/// F2: when the phone cannot be told, the instruction for two links of one account names the
/// entry by its name and its day, never "this computer".
#[test]
fn the_instruction_for_a_twin_names_its_device_and_day() {
    use kourier_model::{DeviceLogout, LogoutReport};
    let (mut src, _sent) = source();
    let report = LogoutReport {
        outcome: DeviceLogout::NotReached,
        device_name: Some("workstation".into()),
    };
    src.on_event(&outcome(
        1,
        true,
        report,
        label(Some("Me"), true, Some(3), 1_757_678_400_000),
    ));
    let n = &src.take_logout_notices()[0];
    assert_eq!(
        n.on_phone,
        "Test network could not be reached: remove “workstation” linked on 12 Sep from the linked devices on your phone."
    );
    assert_eq!(
        n.text,
        "The account “Me · device 3 · linked 12 Sep” was removed from this computer."
    );
}

/// Wording: a logged-out account has no link to cut, and the question must not say Kourier
/// unlinks anything.
#[test]
fn removing_a_logged_out_account_does_not_claim_to_unlink() {
    let (mut src, _sent) = loaded();
    let q = src
        .ask_account_action(ME, AccountAction::Remove)
        .expect("question");
    assert!(q.paragraphs[0].starts_with("Kourier unlinks this computer"));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: ConnState::LoggedOut {
            reason: "unlinked".into(),
        },
    });
    let q = src
        .ask_account_action(ME, AccountAction::Remove)
        .expect("question");
    assert_eq!(
        q.paragraphs[0],
        "Kourier deletes this account's chats, messages and files from this computer."
    );
}

/// F3: "Remove and Link Again?" is asked only where "Transfer History from Phone…" is offered:
/// not while the account's history is being imported, and not for a network without it. A menu
/// item that is hidden but still reachable by the keyboard must not get there.
#[test]
fn the_replace_question_is_not_asked_while_a_transfer_runs() {
    let (mut src, _sent) = loaded();
    assert!(src.ask_replace(ME).is_some(), "offered at rest");
    src.cancel_replace();
    src.on_event(&transfer(ME, downloading(1, Some(10))));
    assert!(
        !src.accounts()[0].offers_history,
        "hidden while the transfer runs"
    );
    assert!(src.ask_replace(ME).is_none(), "and not to be asked");
}

/// L5 (`buttons.md` §3): "Continue Without History" pressed twice (a double click, or a
/// press and a key) sends ONE skip; the backend's next report (the same state again after a
/// restart that lost the skip) lets the user press it again, and so does the time (the
/// window is 2 s: `transfer.rs` `a_second_press_is_dropped_only_within_the_double_click_time`
/// has the clock).
#[test]
fn a_skip_pressed_twice_is_sent_once() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, downloading(1_000, Some(4_000))));
    assert!(src.skip_transfer(ME));
    assert!(!src.skip_transfer(ME), "the second press");
    assert_eq!(commands(&sent), [("skip", 1)]);
    // The backend restarted before it took the skip: the state is reported again.
    src.on_event(&transfer(ME, downloading(1_000, Some(4_000))));
    assert!(
        src.skip_transfer(ME),
        "a skip the backend lost can be sent again"
    );
    assert!(!src.skip_transfer(ME));
    assert_eq!(commands(&sent), [("skip", 1)]);
    src.on_event(&transfer(ME, HistoryTransfer::Skipped));
    assert!(src.take_transfer_notices().is_empty(), "our own skip");
    assert_eq!(commands(&sent), [("dismiss", 1)]);
}
