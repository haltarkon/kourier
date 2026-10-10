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
use crate::transfer::TransferPhase;

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
    sent.borrow_mut().clear();
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

#[test]
fn the_link_page_shows_the_transfer_of_its_own_account_only() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    assert!(src.link_transfer().is_none(), "no transfer reported yet");

    // Another account's transfer is not this page's business.
    let ch = src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert!(ch.transfer);
    assert!(src.link_transfer().is_none());

    let ch = src.on_event(&transfer(new, HistoryTransfer::Waiting));
    assert!(ch.transfer);
    let v = src.link_transfer().expect("the linking account's transfer");
    assert_eq!((v.account, v.phase), (new, TransferPhase::Waiting));
    assert_eq!(v.skip, "Continue Without History");

    // The same state again: nothing to redraw, nothing sent.
    let ch = src.on_event(&transfer(new, HistoryTransfer::Waiting));
    assert!(!ch.transfer);

    src.on_event(&transfer(new, downloading(12_300_000, Some(45_600_000))));
    let v = src.link_transfer().expect("download");
    assert_eq!(v.title, "Downloading Message History");
    assert_eq!(v.detail, "12.3 MB of 45.6 MB");
    assert!(v.progress > 0.26 && v.progress < 0.28);

    // The bytes step back (the backend restarted and counts from the file on disk).
    src.on_event(&transfer(new, downloading(4_000_000, Some(45_600_000))));
    let v = src.link_transfer().expect("download");
    assert_eq!(v.detail, "4.0 MB of 45.6 MB");
    assert!(v.progress > 0.08 && v.progress < 0.09);

    src.on_event(&transfer(new, HistoryTransfer::Reading));
    let v = src.link_transfer().expect("read");
    assert_eq!(v.phase, TransferPhase::Reading);
    assert!(v.skip.is_empty());
    assert!(!src.skip_transfer(new), "a read cannot be skipped");
    assert!(commands(&sent).is_empty(), "nothing was sent so far");

    // The end: the page goes back to the login's own steps, and the notice waits.
    let ch = src.on_event(&transfer(new, HistoryTransfer::Done));
    assert!(ch.transfer);
    assert!(src.link_transfer().is_none());
    assert_eq!(commands(&sent), [("dismiss", 5)]);
    assert_eq!(
        src.take_transfer_notices(),
        ["The message history was transferred."]
    );
    assert!(src.take_transfer_notices().is_empty(), "taken once");
    assert_eq!(
        src.login_view().map(|v| v.kind),
        Some(LoginViewKind::Progress),
        "the link finishes as it always did"
    );
}

#[test]
fn a_skip_from_the_link_page_ends_without_a_notice_and_is_dismissed() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    src.on_event(&transfer(new, HistoryTransfer::Waiting));
    assert!(src.skip_transfer(new));
    assert_eq!(commands(&sent), [("skip", 5)]);
    assert!(
        src.link_transfer().is_some(),
        "the state stays until the backend reports the end"
    );
    // The answer to a skip is the next event.
    let ch = src.on_event(&transfer(new, HistoryTransfer::Skipped));
    assert!(ch.transfer);
    assert!(src.link_transfer().is_none());
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
    assert!(src.link_transfer().is_none(), "there is no link page");
    let b = shown(&src).expect("banner");
    assert_eq!((b.account, b.kind), (ME, BannerKind::Transfer));
    assert!(b.action.is_empty() && b.url.is_empty());
    let v = b.transfer.expect("the same view the link page shows");
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
    assert!(v.text.contains("Update Kourier first"));
}

#[test]
fn an_account_removed_while_its_transfer_is_shown_takes_it_along() {
    let (mut src, sent) = loaded();
    let new = AccountId(5);
    linking(&mut src, &sent, new);
    src.on_event(&transfer(new, HistoryTransfer::Waiting));
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Download)));
    assert!(src.link_transfer().is_some());
    assert!(shown(&src).is_some());

    let ch = src.on_event(&UiEvent::AccountRemoved { account: new });
    assert!(ch.transfer);
    assert!(src.link_transfer().is_none());
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
    assert!(shown(&src).is_none(), "the page shows it large: not twice");
    // Back, or Cancel: the account is linked already, so there is no login to cancel.
    src.cancel_login();
    assert!(
        commands(&sent).is_empty(),
        "no CancelLogin for a linked account"
    );
    assert!(src.link_transfer().is_none());
    assert_eq!(
        shown(&src).map(|b| b.kind),
        Some(BannerKind::Transfer),
        "the transfer goes on in the banner"
    );
    // Before any transfer state the same press does cancel the login.
    linking(&mut src, &sent, AccountId(6));
    src.cancel_login();
    assert_eq!(commands(&sent), [("cancel-login", 6)]);
    // And once the transfer is over the rule is the login's own again (the core ignores a
    // cancel for an account it has linked).
    linking(&mut src, &sent, AccountId(7));
    src.on_event(&transfer(AccountId(7), HistoryTransfer::Reading));
    src.on_event(&transfer(AccountId(7), HistoryTransfer::Done));
    sent.borrow_mut().clear();
    src.cancel_login();
    assert_eq!(commands(&sent), [("cancel-login", 7)]);
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
    assert_eq!(
        src.link_transfer().map(|v| v.account),
        Some(AccountId(2)),
        "the phone is asked for the history"
    );
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
fn review_the_link_page_of_a_relink_does_not_show_a_transfer_the_backend_forgot() {
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    src.on_event(&UiEvent::ConnState {
        account: ME,
        state: logged_out(),
    });
    // "Link Again": the page must show the codes of this login. A relink has no transfer,
    // and the one the book still holds was deleted by the backend with the login.
    src.relink(ME).expect("relink");
    assert!(
        src.link_transfer().is_none(),
        "the link page shows the old transfer in place of the relink's steps"
    );
    // Whatever is heard of a transfer of this account while its relink is on the page
    // (nothing should be: a relink has none), the page keeps to the login's steps and its
    // "Cancel" reaches the backend.
    src.on_event(&transfer(ME, HistoryTransfer::Waiting));
    assert!(src.link_transfer().is_none());
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
    // What happens today (not asserted as right or wrong: the question does not say it):
    // the link of a second account is in progress (wide window: its page beside the list),
    // and "Try Again…" on the banner of the first account is confirmed.
    let (mut src, sent) = loaded();
    src.on_event(&transfer(ME, failed(HistoryTransferFailure::Corrupt)));
    linking(&mut src, &sent, AccountId(6));
    src.ask_replace(ME).expect("question");
    src.confirm_replace(ME).expect("link page");
    assert_eq!(
        commands(&sent),
        [("cancel-login", 6), ("remove", 1), ("add", 0)]
    );
}
