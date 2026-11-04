//! T-2: the login machine over a [`FakeSession`] (no client, no socket). Numbers and ids here are
//! the fictitious 555-01xx range; nothing names a person.

use std::time::Duration;

use kourier_backend_api::{Relink, TakenIdentity};
use kourier_model::{
    AccountId, DeviceLogout, ErrorKind, LoginStep, LoginStepKind, Redacted, UserId,
};

use super::*;
use crate::session::fake::{Call, FakeSession};
use crate::translate::{Banned, Logout, LogoutShape, WaBan};

const PN: &str = "15550100123";
const LID: &str = "99900100123";
const OTHER_PN: &str = "15550100456";
const OTHER_LID: &str = "99900100456";

fn paired(pn: &str, lid: &str) -> Paired {
    Paired {
        pn: Redacted::new(pn.to_owned()),
        lid: Redacted::new(lid.to_owned()),
        device: 7,
    }
}

fn pn(digits: &str) -> UserId {
    UserId::from_parts("pn", digits).expect("id")
}

fn lid(digits: &str) -> UserId {
    UserId::from_parts("lid", digits).expect("id")
}

fn taken(user: UserId, logged_out: bool) -> TakenIdentity {
    TakenIdentity { user, logged_out }
}

fn relink(own: Vec<UserId>) -> Relink {
    Relink {
        account: AccountId(1),
        expect_own: own,
    }
}

fn rules() -> Rules {
    Rules::new(Vec::new(), None, None)
}

fn qr_code(code: &str, secs: u64) -> WaSignal {
    WaSignal::QrCode {
        code: Redacted::new(code.to_owned()),
        valid_for: Duration::from_secs(secs),
    }
}

fn typed(number: &str) -> Vec<(String, Redacted<String>)> {
    vec![("phone".to_owned(), Redacted::new(number.to_owned()))]
}

/// The steps among `outs`, in order.
fn steps(outs: &[Out]) -> Vec<&LoginStep> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Step(s) => Some(s),
            _ => None,
        })
        .collect()
}

/// The error of the one `Failed` step among `outs`, with the step it failed at.
fn failure(outs: &[Out]) -> (&str, &BackendError) {
    let failed: Vec<_> = steps(outs)
        .into_iter()
        .filter_map(|s| match &s.kind {
            LoginStepKind::Failed { error } => Some((s.step_id.as_str(), error)),
            _ => None,
        })
        .collect();
    assert_eq!(failed.len(), 1, "exactly one failed step in {outs:?}");
    failed[0]
}

fn stops(outs: &[Out]) -> usize {
    outs.iter().filter(|o| matches!(o, Out::Stop)).count()
}

fn refusals(outs: &[Out]) -> usize {
    outs.iter().filter(|o| matches!(o, Out::Refused)).count()
}

fn readies(outs: &[Out]) -> Vec<&Ready> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Ready(r) => Some(r),
            _ => None,
        })
        .collect()
}

/// A phone-number login whose `Input` step was shown; returns it with the step's `txn`.
fn phone_login(rules: Rules) -> (Login, u64) {
    let mut login = Login::new("pair-code", rules).expect("flow");
    let outs = login.begin();
    let txn = steps(&outs)[0].txn;
    (login, txn)
}

fn qr_login(rules: Rules) -> Login {
    let mut login = Login::new("qr", rules).expect("flow");
    assert!(login.begin().is_empty());
    login
}

// ----- the number -----

#[test]
fn a_typed_number_becomes_digits_and_the_obvious_mistakes_are_refused() {
    for (typed, digits) in [
        ("15550100123", "15550100123"),
        ("+1 555 010 0123", "15550100123"),
        ("+1 (555) 010-0123", "15550100123"),
        ("001 555.010.0123", "15550100123"),
        ("  +15550100123  ", "15550100123"),
        ("1555010", "1555010"),
        ("123456789012345", "123456789012345"),
    ] {
        assert_eq!(normalize_number(typed).as_deref(), Ok(digits), "{typed}");
    }
    for (typed, problem) in [
        ("", NumberProblem::TooShort),
        ("+", NumberProblem::TooShort),
        ("155501", NumberProblem::TooShort),
        ("1234567890123456", NumberProblem::TooLong),
        ("05550100123", NumberProblem::LeadingZero),
        // A `+` is the international prefix already: a `00` after it is a missing country code.
        ("+001 555 010 0123", NumberProblem::LeadingZero),
        ("1555010012a", NumberProblem::Characters),
        ("1555-010-0123;", NumberProblem::Characters),
        ("1555\u{202e}0100123", NumberProblem::Characters),
    ] {
        assert_eq!(normalize_number(typed), Err(problem), "{typed:?}");
    }
}

#[tokio::test]
async fn the_phone_way_asks_for_the_number_in_one_input_step() {
    let mut login = Login::new("pair-code", rules()).expect("flow");
    let outs = login.begin();
    let shown = steps(&outs);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].step_id, "phone");
    let LoginStepKind::Input { fields } = &shown[0].kind else {
        panic!("an input step, got {:?}", shown[0].kind);
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].id, "phone");
    assert_eq!(fields[0].kind, kourier_model::InputKind::Phone);
    assert!(fields[0].required);

    // The QR codes the client rotates in anyway are not shown on this way.
    let session = FakeSession::new();
    assert!(
        login
            .signal(&session, &qr_code("ref", 60), 0)
            .await
            .is_empty()
    );
    assert!(
        login
            .signal(
                &session,
                &WaSignal::QrExhausted {
                    socket_closed: false
                },
                0
            )
            .await
            .is_empty()
    );
    assert!(!login.is_over());
}

#[tokio::test]
async fn an_unknown_flow_is_unsupported() {
    let e = Login::new("id-backup", rules()).expect_err("no such flow");
    assert_eq!(e.kind, ErrorKind::Unsupported);
}

#[tokio::test]
async fn a_good_number_is_asked_for_once_as_digits_and_a_stale_answer_is_ignored() {
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());

    // An answer to another step instance does nothing.
    assert!(
        login
            .input(&session, txn + 5, &typed("+1 555 010 0123"))
            .await
            .is_empty()
    );
    assert_eq!(session.pair_requests(), 0);

    let outs = login
        .input(&session, txn, &typed("+1 (555) 010-0123"))
        .await;
    assert!(outs.is_empty(), "the code arrives as a signal: {outs:?}");
    assert_eq!(session.calls(), vec![Call::PairWithCode(PN.to_owned())]);

    // The same answer twice does not ask twice.
    assert!(login.input(&session, txn, &typed(PN)).await.is_empty());
    assert_eq!(session.pair_requests(), 1);
}

#[tokio::test]
async fn a_number_that_is_no_number_fails_the_phone_step_and_asks_for_nothing() {
    for bad in [
        "",
        "123456",
        "1234567890123456",
        "05550100123",
        "1555x0100123",
    ] {
        let session = FakeSession::new();
        let (mut login, txn) = phone_login(rules());
        let outs = login.input(&session, txn, &typed(bad)).await;
        let (at, error) = failure(&outs);
        assert_eq!(
            (at, &error.kind),
            ("phone", &ErrorKind::InvalidInput),
            "{bad:?}"
        );
        assert_eq!(stops(&outs), 1);
        assert!(login.is_over());
        assert_eq!(session.pair_requests(), 0, "{bad:?}");
    }
    // An answer with no phone field at all.
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());
    let outs = login.input(&session, txn, &[]).await;
    assert_eq!(failure(&outs).1.kind, ErrorKind::InvalidInput);
}

#[tokio::test]
async fn another_accounts_number_is_refused_before_any_code_is_asked_for() {
    let session = FakeSession::new();
    let rules = Rules::new(vec![taken(pn(PN), true)], None, None);
    let (mut login, txn) = phone_login(rules);
    let outs = login.input(&session, txn, &typed("+1 555 010 0123")).await;
    let (at, error) = failure(&outs);
    assert_eq!(at, "phone");
    assert_eq!(error.kind, ErrorKind::AlreadyAdded { logged_out: true });
    assert_eq!(stops(&outs), 1);
    assert_eq!(session.pair_requests(), 0, "no code was asked for");

    // A different number is not refused by it.
    let session = FakeSession::new();
    let rules = Rules::new(vec![taken(pn(OTHER_PN), false)], None, None);
    let (mut login, txn) = phone_login(rules);
    assert!(login.input(&session, txn, &typed(PN)).await.is_empty());
    assert_eq!(session.pair_requests(), 1);
}

/// Review F1 of W2a, closed by W2b (B4): the core hands `StartLogin.taken` EVERY own id of the other
/// accounts (`ingest::other_identities`; its test is `remove_unlink::a_login_is_told_every_own_id_of_the_other_accounts`),
/// so an account that is `[lid, pn]` puts its phone number into the list and a typed number of it is
/// refused BEFORE the code is asked for (a code pushed to the other account's phone). Given the LID
/// alone (a list made as before W2b), the code is asked for and only the pairing's own check, which
/// reads both ids, refuses and logs the companion out again: that second line stays.
#[tokio::test]
async fn every_own_id_in_taken_refuses_a_number_before_the_code() {
    // Account A is [lid, pn] and the core hands both: refused before any code.
    let every_id = vec![taken(lid(LID), false), taken(pn(PN), false)];
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(every_id, None, None));
    let outs = login.input(&session, txn, &typed(PN)).await;
    let (at, error) = failure(&outs);
    assert_eq!(at, "phone");
    assert_eq!(error.kind, ErrorKind::AlreadyAdded { logged_out: false });
    assert_eq!(session.pair_requests(), 0, "no code was asked for");

    // The LID alone (what the core handed before W2b): the number is not refused, the code is asked
    // for, and the pairing refuses it (both ids are checked there) and records nothing.
    let lid_only = vec![taken(lid(LID), false)];
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(lid_only, None, None));
    let outs = login.input(&session, txn, &typed(PN)).await;
    assert!(outs.is_empty(), "the number is not refused: {outs:?}");
    assert_eq!(
        session.pair_requests(),
        1,
        "the second line is what refuses"
    );
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    let (at, error) = failure(&outs);
    assert_eq!(at, "code");
    assert_eq!(error.kind, ErrorKind::AlreadyAdded { logged_out: false });
    assert_eq!(refusals(&outs), 1);
    assert!(readies(&outs).is_empty());
}

#[tokio::test]
async fn a_relink_asks_for_the_accounts_own_number_only() {
    let own = relink(vec![lid(LID), pn(PN)]);
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(Vec::new(), Some(own.clone()), None));
    let outs = login.input(&session, txn, &typed(OTHER_PN)).await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("phone", &ErrorKind::InvalidInput));
    assert_eq!(session.pair_requests(), 0, "no code to a stranger's phone");

    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(Vec::new(), Some(own), None));
    assert!(login.input(&session, txn, &typed(PN)).await.is_empty());
    assert_eq!(session.pair_requests(), 1);

    // An account with no number among its ids is checked at the pairing, not here.
    let session = FakeSession::new();
    let lid_only = relink(vec![lid(LID)]);
    let (mut login, txn) = phone_login(Rules::new(Vec::new(), Some(lid_only), None));
    assert!(
        login
            .input(&session, txn, &typed(OTHER_PN))
            .await
            .is_empty()
    );
    assert_eq!(session.pair_requests(), 1);
}

#[tokio::test]
async fn the_expected_tail_is_checked_before_the_code_is_asked_for() {
    let tail = || Some(Redacted::new("0123".to_owned()));
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(Vec::new(), None, tail()));
    let outs = login.input(&session, txn, &typed(OTHER_PN)).await;
    assert_eq!(failure(&outs).1.kind, ErrorKind::NotTheExpectedAccount);
    assert_eq!(session.pair_requests(), 0);

    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(Vec::new(), None, tail()));
    assert!(login.input(&session, txn, &typed(PN)).await.is_empty());
    assert_eq!(session.pair_requests(), 1);

    // An empty tail matches nothing (fails closed).
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(Rules::new(
        Vec::new(),
        None,
        Some(Redacted::new("no digits".to_owned())),
    ));
    let outs = login.input(&session, txn, &typed(PN)).await;
    assert_eq!(failure(&outs).1.kind, ErrorKind::NotTheExpectedAccount);
    assert_eq!(session.pair_requests(), 0);
}

// ----- the pair code -----

#[tokio::test]
async fn the_code_is_shown_when_the_client_has_made_it_and_again_for_each_new_one() {
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());
    login.input(&session, txn, &typed(PN)).await;
    let code = |c: &str| WaSignal::PairCode {
        code: Redacted::new(c.to_owned()),
        valid_for: Duration::from_secs(180),
    };
    let first = login.signal(&session, &code("AAAA1111"), 0).await;
    let second = login.signal(&session, &code("BBBB2222"), 0).await;
    let (a, b) = (steps(&first), steps(&second));
    assert_eq!((a.len(), b.len()), (1, 1));
    assert_eq!(a[0].step_id, "code");
    assert_eq!(
        a[0].kind,
        LoginStepKind::ShowCode {
            code: Redacted::new("AAAA1111".to_owned())
        }
    );
    assert!(b[0].txn > a[0].txn);
    // A code nobody asked for (the QR way) is not shown.
    let mut qr = qr_login(rules());
    assert!(qr.signal(&session, &code("CCCC3333"), 0).await.is_empty());
}

#[tokio::test]
async fn an_expired_code_is_asked_for_again_with_the_same_number_at_most_three_times() {
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());
    login.input(&session, txn, &typed(PN)).await;
    let refresh = WaSignal::PairCodeRefresh {
        force_manual: false,
    };
    for n in 1..=MAX_CODE_REFRESHES {
        assert!(login.signal(&session, &refresh, 0).await.is_empty());
        assert_eq!(session.pair_requests(), 1 + usize::from(n));
    }
    assert_eq!(
        session.calls().last(),
        Some(&Call::PairWithCode(PN.to_owned()))
    );
    // The fourth refresh ends the login: no request, an expired code.
    let outs = login.signal(&session, &refresh, 0).await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("code", &ErrorKind::Expired));
    assert_eq!(session.pair_requests(), 1 + usize::from(MAX_CODE_REFRESHES));
    assert_eq!(stops(&outs), 1);
}

#[tokio::test]
async fn a_refresh_the_person_must_ask_for_ends_the_login_without_a_request() {
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());
    login.input(&session, txn, &typed(PN)).await;
    let outs = login
        .signal(
            &session,
            &WaSignal::PairCodeRefresh { force_manual: true },
            0,
        )
        .await;
    assert_eq!(failure(&outs).1.kind, ErrorKind::Expired);
    assert_eq!(session.pair_requests(), 1);
}

fn refused(code: i32, backoff: Option<u64>) -> PairCodeFailure {
    PairCodeFailure::Refused {
        code,
        backoff: backoff.map(Duration::from_secs),
    }
}

#[tokio::test]
async fn a_refused_code_request_is_told_by_its_status() {
    let secs = |s| Some(Duration::from_secs(s));
    let cases: Vec<(PairCodeFailure, ErrorKind)> = vec![
        (
            refused(429, secs(30).map(|d| d.as_secs())),
            ErrorKind::RateLimited {
                retry_after: secs(30),
            },
        ),
        (
            refused(429, None),
            ErrorKind::RateLimited { retry_after: None },
        ),
        // 400 is a malformed number OR a throttle: with a back-off it is the throttle, without one
        // it is a number to check (critic M7).
        (refused(400, None), ErrorKind::InvalidInput),
        (
            refused(400, Some(60)),
            ErrorKind::RateLimited {
                retry_after: secs(60),
            },
        ),
        (refused(452, None), ErrorKind::Unsupported),
        (
            refused(403, None),
            ErrorKind::Transient { retry_after: None },
        ),
        (
            refused(500, Some(5)),
            ErrorKind::Transient {
                retry_after: secs(5),
            },
        ),
        (
            refused(418, None),
            ErrorKind::Transient { retry_after: None },
        ),
        (
            PairCodeFailure::NoAnswer,
            ErrorKind::Transient { retry_after: None },
        ),
        (PairCodeFailure::InvalidNumber, ErrorKind::InvalidInput),
    ];
    for (failure_of_request, kind) in cases {
        // From the number's answer ...
        let session = FakeSession::new();
        session.script_pair(Err(failure_of_request));
        let (mut login, txn) = phone_login(rules());
        let outs = login.input(&session, txn, &typed(PN)).await;
        let (at, error) = failure(&outs);
        assert_eq!(
            (at, &error.kind),
            ("phone", &kind),
            "{failure_of_request:?}"
        );
        assert_eq!(stops(&outs), 1);
        assert!(login.is_over());

        // ... and from a refresh.
        let session = FakeSession::new();
        let (mut login, txn) = phone_login(rules());
        login.input(&session, txn, &typed(PN)).await;
        session.script_pair(Err(failure_of_request));
        let outs = login
            .signal(
                &session,
                &WaSignal::PairCodeRefresh {
                    force_manual: false,
                },
                0,
            )
            .await;
        let (at, error) = failure(&outs);
        assert_eq!((at, &error.kind), ("code", &kind), "{failure_of_request:?}");
    }
}

#[tokio::test]
async fn a_request_that_lost_its_flow_is_no_failure() {
    for lost in [
        PairCodeFailure::AlreadyOutstanding,
        PairCodeFailure::Cancelled,
    ] {
        let session = FakeSession::new();
        session.script_pair(Err(lost));
        let (mut login, txn) = phone_login(rules());
        assert!(login.input(&session, txn, &typed(PN)).await.is_empty());
        assert!(!login.is_over(), "{lost:?}");
    }
}

#[tokio::test]
async fn the_event_that_repeats_a_refusal_is_not_a_second_report() {
    let session = FakeSession::new();
    session.script_pair(Err(refused(429, Some(30))));
    let (mut login, txn) = phone_login(rules());
    let first = login.input(&session, txn, &typed(PN)).await;
    assert_eq!(steps(&first).len(), 1);
    // Upstream dispatches the failure as an event as well; it arrives after the call's own error.
    let event = WaSignal::PairCodeRefused {
        refusal: Some(PairRefusal::RateOverlimit),
        backoff: Some(Duration::from_secs(30)),
    };
    assert!(login.signal(&session, &event, 0).await.is_empty());

    // Nor does it end a login that waits for its code: the call's own answer is the only report.
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());
    login.input(&session, txn, &typed(PN)).await;
    assert!(login.signal(&session, &event, 0).await.is_empty());
    assert!(!login.is_over());
}

#[tokio::test]
async fn cancelling_a_code_request_withdraws_it_once() {
    let session = FakeSession::new();
    let (mut login, txn) = phone_login(rules());
    login.input(&session, txn, &typed(PN)).await;
    let outs = login.cancel(&session).await;
    assert_eq!(outs, vec![Out::Stop]);
    assert_eq!(session.count(&Call::CancelPairCode), 1);
    assert!(login.is_over());
    // Nothing happens twice.
    assert!(login.cancel(&session).await.is_empty());
    assert_eq!(session.count(&Call::CancelPairCode), 1);
    // The QR way has no code to withdraw.
    let session = FakeSession::new();
    let mut qr = qr_login(rules());
    assert_eq!(qr.cancel(&session).await, vec![Out::Stop]);
    assert_eq!(session.count(&Call::CancelPairCode), 0);
}

// ----- the QR code -----

#[tokio::test]
async fn the_qr_way_shows_one_step_per_code_and_the_txn_rises() {
    let session = FakeSession::new();
    let mut login = qr_login(rules());
    let first = login.signal(&session, &qr_code("ref-1", 60), 0).await;
    let second = login.signal(&session, &qr_code("ref-2", 20), 0).await;
    let (a, b) = (steps(&first), steps(&second));
    assert_eq!((a.len(), b.len()), (1, 1));
    assert_eq!(a[0].step_id, "qr");
    assert_eq!(
        a[0].kind,
        LoginStepKind::ShowQr {
            data: Redacted::new("ref-1".to_owned()),
            valid_for: Some(Duration::from_secs(60)),
        }
    );
    assert_eq!(
        b[0].kind,
        LoginStepKind::ShowQr {
            data: Redacted::new("ref-2".to_owned()),
            valid_for: Some(Duration::from_secs(20)),
        }
    );
    assert!(b[0].txn > a[0].txn);
    assert_eq!(
        session.calls(),
        Vec::new(),
        "a QR code asks nothing of the client"
    );
}

#[tokio::test]
async fn codes_that_run_out_expire_the_qr_login_and_an_old_phone_is_unsupported() {
    let session = FakeSession::new();
    let mut login = qr_login(rules());
    login.signal(&session, &qr_code("ref", 60), 0).await;
    let outs = login
        .signal(
            &session,
            &WaSignal::QrExhausted {
                socket_closed: true,
            },
            0,
        )
        .await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("qr", &ErrorKind::Expired));
    assert_eq!(stops(&outs), 1);
    // Over: a late code shows nothing.
    assert!(
        login
            .signal(&session, &qr_code("late", 20), 0)
            .await
            .is_empty()
    );

    let mut login = qr_login(rules());
    let outs = login
        .signal(&session, &WaSignal::QrScannedWithoutMultidevice, 0)
        .await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("qr", &ErrorKind::Unsupported));
}

#[tokio::test]
async fn a_passkey_request_is_refused_at_the_passkey_step() {
    let session = FakeSession::new();
    let mut login = qr_login(rules());
    let outs = login.signal(&session, &WaSignal::PasskeyRequested, 0).await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("passkey", &ErrorKind::Unsupported));
    assert_eq!(stops(&outs), 1);
    assert_eq!(
        session.calls(),
        Vec::new(),
        "nothing was linked, nothing to log out"
    );
}

// ----- the pairing -----

#[tokio::test]
async fn a_pairing_that_passes_hands_back_the_account_lid_first_and_logs_nothing_out() {
    let session = FakeSession::new();
    let mut login = qr_login(rules());
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    let ready = readies(&outs);
    assert_eq!(ready.len(), 1);
    assert_eq!(
        ready[0].own_ids,
        vec![lid(LID), pn(PN)],
        "[lid, pn]: decision L4"
    );
    assert_eq!(ready[0].device, 7);
    assert_eq!(ready[0].number.expose(), &format!("+{PN}"));
    let shown = steps(&outs);
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0].step_id, "linking");
    assert!(matches!(shown[0].kind, LoginStepKind::Progress { .. }));
    assert_eq!(stops(&outs), 0);
    assert_eq!(session.count(&Call::Logout), 0);
    assert!(!login.is_over());
    assert_eq!(login.logout_report(), None);
    assert!(!login.gate().is_refused());
}

#[tokio::test]
async fn the_login_is_done_only_after_the_core_acked_and_the_first_connected_in_either_order() {
    for ack_first in [true, false] {
        let session = FakeSession::new();
        let mut login = qr_login(rules());
        // A `Connected` of the pairing socket's past, before the pairing, counts for nothing.
        assert!(
            login
                .signal(&session, &WaSignal::Connected, 0)
                .await
                .is_empty()
        );
        login
            .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
            .await;

        let (first, second) = if ack_first {
            (
                login.acked(),
                login.signal(&session, &WaSignal::Connected, 0).await,
            )
        } else {
            (
                login.signal(&session, &WaSignal::Connected, 0).await,
                login.acked(),
            )
        };
        assert!(
            first.is_empty(),
            "one half is not enough (ack first: {ack_first})"
        );
        let done = steps(&second);
        assert_eq!(done.len(), 1, "ack first: {ack_first}");
        assert_eq!(
            (done[0].step_id.as_str(), &done[0].kind),
            ("done", &LoginStepKind::Done)
        );
        assert!(login.is_over());
        // Nothing after Done.
        assert!(
            login
                .signal(&session, &WaSignal::Connected, 0)
                .await
                .is_empty()
        );
        assert!(login.acked().is_empty());
    }
}

#[tokio::test]
async fn an_ack_alone_or_a_connect_alone_never_finishes_a_login() {
    let session = FakeSession::new();
    let mut login = qr_login(rules());
    login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert!(login.acked().is_empty());
    assert!(login.acked().is_empty(), "the same ack twice is still half");
    assert!(!login.is_over());
}

/// Review F5: `Ready` carries the ids and the number; what any `?outs` of a debugging session prints
/// is the id schemes and the device only.
#[test]
fn the_debug_of_a_ready_account_names_no_id_and_no_number() {
    let ready = Ready::of(pn(PN), Some(lid(LID)), 7);
    assert_eq!(ready.number.expose(), &format!("+{PN}"));
    let outs = vec![Out::Ready(ready)];
    let shown = format!("{outs:?}");
    for secret in [PN, LID, "+1555"] {
        assert!(!shown.contains(secret), "{shown}");
    }
    for known in ["lid", "pn", "device: 7"] {
        assert!(shown.contains(known), "{shown}");
    }
}

#[tokio::test]
async fn an_account_of_the_profile_fails_the_step_and_the_machine_awaits_no_logout() {
    // Taken by its phone-number id, and by its LID: both stand for the same account.
    for user in [pn(PN), lid(LID)] {
        let session = FakeSession::new();
        let rules = Rules::new(vec![taken(user, true)], None, None);
        let mut login = qr_login(rules);
        login.signal(&session, &qr_code("ref", 60), 0).await;
        let outs = login
            .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
            .await;
        let (at, error) = failure(&outs);
        assert_eq!(at, "qr");
        assert_eq!(error.kind, ErrorKind::AlreadyAdded { logged_out: true });
        // F3: the client is NOT stopped (its logout is under way, started by the event handler) and
        // the machine itself asked the session for nothing: it awaits no logout.
        assert_eq!((refusals(&outs), stops(&outs)), (1, 0));
        assert!(readies(&outs).is_empty(), "nothing for the core's outbox");
        assert_eq!(
            session.count(&Call::Logout),
            0,
            "the machine awaits no logout"
        );
        assert!(login.is_refused());
        assert_eq!(
            login.logout_report(),
            None,
            "pending until the actor hands it in"
        );
        assert!(login.gate().is_refused());
        assert!(login.is_over());
        // The outcome is picked up later and kept.
        login.logout_reached(DeviceLogout::Removed);
        assert_eq!(login.logout_report(), Some(DeviceLogout::Removed));
        // The connect after the 515 and anything else is not heard.
        assert!(
            login
                .signal(&session, &WaSignal::Connected, 0)
                .await
                .is_empty()
        );
        assert!(login.acked().is_empty());
        assert_eq!(session.count(&Call::Logout), 0);
    }
}

#[tokio::test]
async fn a_relink_by_another_identity_is_refused_and_awaits_no_logout() {
    let session = FakeSession::new();
    let rules = Rules::new(
        Vec::new(),
        Some(relink(vec![lid(OTHER_LID), pn(OTHER_PN)])),
        None,
    );
    let mut login = qr_login(rules);
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("qr", &ErrorKind::InvalidInput));
    assert!(readies(&outs).is_empty());
    assert_eq!(refusals(&outs), 1);
    assert_eq!(session.count(&Call::Logout), 0);
}

#[tokio::test]
async fn a_relink_after_a_number_change_passes_by_the_lid() {
    // The phone has a new number; the LID is the account's.
    let session = FakeSession::new();
    let rules = Rules::new(Vec::new(), Some(relink(vec![lid(LID), pn(OTHER_PN)])), None);
    let mut login = qr_login(rules);
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert_eq!(readies(&outs).len(), 1);
    assert_eq!(session.count(&Call::Logout), 0);
    // And a relink that names no ids checks nothing (the core knows none).
    let mut login = qr_login(Rules::new(Vec::new(), Some(relink(Vec::new())), None));
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert_eq!(readies(&outs).len(), 1);
}

#[tokio::test]
async fn the_tail_is_checked_after_a_pairing_by_qr_and_a_wrong_account_is_refused() {
    let session = FakeSession::new();
    let rules = Rules::new(Vec::new(), None, Some(Redacted::new("9999".to_owned())));
    let mut login = qr_login(rules);
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert_eq!(failure(&outs).1.kind, ErrorKind::NotTheExpectedAccount);
    assert!(readies(&outs).is_empty());
    assert_eq!(refusals(&outs), 1);
    assert_eq!(session.count(&Call::Logout), 0);

    let session = FakeSession::new();
    let rules = Rules::new(Vec::new(), None, Some(Redacted::new("0123".to_owned())));
    let mut login = qr_login(rules);
    let outs = login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert_eq!(readies(&outs).len(), 1);
    assert_eq!(session.count(&Call::Logout), 0);
}

#[tokio::test]
async fn the_checks_run_in_order_relink_then_profile_then_tail() {
    let a = paired(PN, LID);
    let relink_wrong = Some(relink(vec![lid(OTHER_LID)]));
    let taken_it = vec![taken(lid(LID), false)];
    let tail = Some(Redacted::new("9999".to_owned()));

    let all = Rules::new(taken_it.clone(), relink_wrong.clone(), tail.clone());
    assert_eq!(
        all.decide(&a),
        Decision::Refuse(Refusal::NotTheRelinkedAccount)
    );
    let no_relink = Rules::new(taken_it, None, tail.clone());
    assert_eq!(
        no_relink.decide(&a),
        Decision::Refuse(Refusal::AlreadyAdded { logged_out: false })
    );
    let tail_only = Rules::new(Vec::new(), None, tail);
    assert_eq!(
        tail_only.decide(&a),
        Decision::Refuse(Refusal::NotTheExpectedNumber)
    );
    assert!(matches!(rules().decide(&a), Decision::Accept(_)));
    // Ids that cannot be ids are refused, not linked.
    let odd = paired("12 34", LID);
    assert_eq!(rules().decide(&odd), Decision::Refuse(Refusal::Unreadable));
}

#[tokio::test]
async fn a_logout_that_did_not_reach_the_server_is_reported_as_not_reached() {
    // The report says `Removed` only for a confirmed removal; every other end of the logout is
    // `NotReached`, and the first answer handed in is the one kept.
    for (reached, reported) in [
        (DeviceLogout::Removed, DeviceLogout::Removed),
        (DeviceLogout::NotReached, DeviceLogout::NotReached),
        (DeviceLogout::NoDevice, DeviceLogout::NotReached),
    ] {
        let session = FakeSession::new();
        let rules = Rules::new(vec![taken(lid(LID), false)], None, None);
        let mut login = qr_login(rules);
        login
            .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
            .await;
        assert_eq!(login.logout_report(), None, "pending");
        login.logout_reached(reached);
        assert_eq!(login.logout_report(), Some(reported), "{reached:?}");
        assert_eq!(session.count(&Call::Logout), 0);
    }
}

#[test]
fn the_gate_sets_its_flag_on_a_refusal_and_never_on_an_accept() {
    let gate = Gate::new(Rules::new(vec![taken(lid(LID), false)], None, None));
    assert!(!gate.is_refused());
    assert!(matches!(
        gate.decide(&paired(OTHER_PN, OTHER_LID)),
        Decision::Accept(_)
    ));
    assert!(!gate.is_refused());
    assert!(matches!(gate.decide(&paired(PN, LID)), Decision::Refuse(_)));
    assert!(gate.is_refused());
    // For good: a later accept does not clear it.
    assert!(matches!(
        gate.decide(&paired(OTHER_PN, OTHER_LID)),
        Decision::Accept(_)
    ));
    assert!(gate.is_refused());
}

// ----- cancel, and verdicts of the connection -----

#[tokio::test]
async fn a_pairing_that_stands_is_not_undone_by_cancel() {
    let session = FakeSession::new();
    let mut login = qr_login(rules());
    login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert!(
        login.cancel(&session).await.is_empty(),
        "the core's Logout follows and is the actor's"
    );
    assert_eq!(session.calls(), Vec::new());
}

fn banned(until_ms: u64) -> WaSignal {
    WaSignal::Banned(Banned {
        ban: WaBan::SameMessageToMany,
        until_ms,
        explanation: None,
        appeal_url: None,
    })
}

#[tokio::test]
async fn a_verdict_of_the_connection_ends_a_login_that_has_not_finished() {
    let session = FakeSession::new();
    let cases: Vec<(WaSignal, ErrorKind)> = vec![
        (WaSignal::Outdated, ErrorKind::Unsupported),
        (
            WaSignal::Replaced,
            ErrorKind::Transient { retry_after: None },
        ),
        (
            WaSignal::Refused { code: 413 },
            ErrorKind::Transient { retry_after: None },
        ),
        (
            banned(70_000),
            ErrorKind::RateLimited {
                retry_after: Some(Duration::from_secs(60)),
            },
        ),
        (
            WaSignal::LoggedOut(Logout {
                shape: LogoutShape::Unauthorized,
                violation: None,
                explanation: None,
            }),
            ErrorKind::LoggedOut,
        ),
    ];
    for (signal, kind) in cases {
        let mut login = qr_login(rules());
        let outs = login.signal(&session, &signal, 10_000).await;
        let (at, error) = failure(&outs);
        assert_eq!((at, &error.kind), ("qr", &kind), "{signal:?}");
        assert_eq!(stops(&outs), 1);
    }
    // A reconnect the client handles by itself, a lock and a logout of our own say nothing.
    let mut login = qr_login(rules());
    for quiet in [
        WaSignal::Disconnected { clean: false },
        WaSignal::WillRetry(crate::translate::RetryWhy::ServerError),
        WaSignal::LocalLogout,
    ] {
        assert!(
            login.signal(&session, &quiet, 0).await.is_empty(),
            "{quiet:?}"
        );
    }
    assert!(!login.is_over());
    // The step a verdict fails is the one on screen: the code, once a number was typed.
    let (mut login, txn) = phone_login(rules());
    login.input(&session, txn, &typed(PN)).await;
    let outs = login.signal(&session, &WaSignal::Outdated, 0).await;
    assert_eq!(failure(&outs).0, "code");
}

// ----- the pre-pair refusal (W2c) -----

fn tail_rules(tail: &str) -> Rules {
    Rules::new(Vec::new(), None, Some(Redacted::new(tail.to_owned())))
}

/// F2: the token a run tool reads before it says "scan now". `on` whenever a tail was set (an
/// empty one fails closed, so it still checks), `off` without one; it is a fixed word, never the
/// tail. Mutant: `off` for a set tail (the check would be reported away).
#[test]
fn the_number_check_is_reported_as_a_fixed_token() {
    assert_eq!(Rules::new(Vec::new(), None, None).number_check(), "off");
    assert_eq!(tail_rules("9999").number_check(), "on");
    assert_eq!(tail_rules("").number_check(), "on");
}

/// T-U5: a pairing the claim refused ends the login with the refusal's own failure and stops the
/// client. It is NOT a refused pairing in the W2a sense: nothing was paired, so there is no logout
/// to await and the core's `Logout` that follows finds no device. Mutant: `Out::Refused` handed back.
#[tokio::test]
async fn a_claimed_refusal_ends_the_login_without_a_device_to_log_out() {
    let session = FakeSession::new();
    let mut login = qr_login(tail_rules("9999"));
    let outs = login
        .signal(&session, &WaSignal::PairClaimed(Some(paired(PN, LID))), 0)
        .await;
    let (at, error) = failure(&outs);
    assert_eq!((at, &error.kind), ("qr", &ErrorKind::NotTheExpectedAccount));
    assert_eq!(
        error.user_message.as_deref(),
        Some("This is not the account Kourier was told to link.")
    );
    assert_eq!(stops(&outs), 1);
    assert_eq!(refusals(&outs), 0, "a claimed refusal awaits no logout");
    assert!(readies(&outs).is_empty());
    assert!(login.is_over());
    assert!(!login.is_refused());
    assert_eq!(login.logout_report(), None);
    assert!(!login.holds_receive(), "the hold opens: the login is over");
    assert_eq!(session.count(&Call::Logout), 0);
    // The client is on its way out: whatever else pairs on it is refused.
    assert!(matches!(
        login.gate().claim(&[Some(paired(OTHER_PN, OTHER_LID))]),
        Verdict::Refuse(Refused {
            token: "no-login",
            ..
        })
    ));
}

/// The cause is the real one: the same rules judge the ids again, in the machine.
#[tokio::test]
async fn a_claimed_refusal_keeps_its_cause() {
    let session = FakeSession::new();
    let cases: Vec<(Rules, Option<Paired>, ErrorKind)> = vec![
        (
            Rules::new(vec![taken(lid(LID), true)], None, None),
            Some(paired(PN, LID)),
            ErrorKind::AlreadyAdded { logged_out: true },
        ),
        (
            Rules::new(
                Vec::new(),
                Some(relink(vec![lid(OTHER_LID), pn(OTHER_PN)])),
                None,
            ),
            Some(paired(PN, LID)),
            ErrorKind::InvalidInput,
        ),
        (rules(), None, ErrorKind::InvalidInput),
    ];
    for (rules, ids, kind) in cases {
        let mut login = qr_login(rules);
        let outs = login.signal(&session, &WaSignal::PairClaimed(ids), 0).await;
        let (at, error) = failure(&outs);
        assert_eq!((at, &error.kind), ("qr", &kind));
        assert_eq!(stops(&outs), 1);
    }
}

/// F14: a claim before the number was typed (phone flow) fails at `code`: at `phone` the page would
/// tell the person to check a number they never typed. After the number it is the code on screen.
/// Mutant: the step is always the one on screen.
#[tokio::test]
async fn a_claimed_refusal_before_the_number_does_not_blame_the_number() {
    let session = FakeSession::new();
    let (mut login, _txn) = phone_login(tail_rules("9999"));
    let outs = login
        .signal(&session, &WaSignal::PairClaimed(Some(paired(PN, LID))), 0)
        .await;
    assert_eq!(failure(&outs).0, "code");
    assert_eq!(stops(&outs), 1);

    let (mut login, txn) = phone_login(tail_rules("9999"));
    // The typed number ends in the tail, so the code is asked for; then a pairing by another number.
    login.input(&session, txn, &typed("15550109999")).await;
    let outs = login
        .signal(&session, &WaSignal::PairClaimed(Some(paired(PN, LID))), 0)
        .await;
    assert_eq!(failure(&outs).0, "code");
    assert_eq!(refusals(&outs), 0);
}

/// A claim that arrives when no login can take it is ignored: after a pairing stood, after the login
/// ended, after a refusal.
#[tokio::test]
async fn a_claimed_refusal_is_ignored_when_the_login_is_past_the_pairing() {
    let session = FakeSession::new();
    let claimed = WaSignal::PairClaimed(Some(paired(OTHER_PN, OTHER_LID)));
    // A pairing stands.
    let mut login = qr_login(rules());
    login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert!(login.signal(&session, &claimed, 0).await.is_empty());
    assert!(login.is_linked() || !login.is_over());
    // The login ended.
    let mut login = qr_login(rules());
    login.cancel(&session).await;
    assert!(login.signal(&session, &claimed, 0).await.is_empty());
    // A pairing was refused by the second line.
    let mut login = qr_login(tail_rules("9999"));
    login
        .signal(&session, &WaSignal::PairSuccess(paired(PN, LID)), 0)
        .await;
    assert!(login.is_refused());
    assert!(login.signal(&session, &claimed, 0).await.is_empty());
}

/// T-U4/F3: EVERY `pair-success` child is judged, and anything but exactly one child is refused.
/// Upstream completes the first child it handles, so a later child the rules accept must not hide an
/// earlier stranger's. Mutants: only the first child judged; only the last.
#[test]
fn every_pair_success_child_is_judged_and_exactly_one_is_allowed() {
    let rules = || Rules::new(vec![taken(lid(OTHER_LID), false)], None, None);
    let good = paired(PN, LID);
    let stranger = paired(OTHER_PN, OTHER_LID);
    /// (name, the children's ids, the refusal's word and the ids it tells the login: none = a pass)
    type Case = (
        &'static str,
        Vec<Option<Paired>>,
        Option<(&'static str, Option<Paired>)>,
    );
    let cases: Vec<Case> = vec![
        ("one good child", vec![Some(good.clone())], None),
        (
            "stranger then good",
            vec![Some(stranger.clone()), Some(good.clone())],
            Some(("taken", Some(stranger.clone()))),
        ),
        (
            "good then stranger",
            vec![Some(good.clone()), Some(stranger.clone())],
            Some(("taken", Some(stranger.clone()))),
        ),
        (
            "two good children",
            vec![Some(good.clone()), Some(good.clone())],
            Some(("unreadable", None)),
        ),
        ("no ids", vec![None], Some(("unreadable", None))),
        (
            "good then no ids",
            vec![Some(good.clone()), None],
            Some(("unreadable", None)),
        ),
        ("no child at all", vec![], Some(("unreadable", None))),
    ];
    for (name, pairs, expect) in cases {
        let gate = Gate::new(rules());
        match (gate.claim(&pairs), expect) {
            (Verdict::Pass, None) => assert!(!gate.is_refused(), "{name}"),
            (Verdict::Refuse(refused), Some((token, told))) => {
                assert_eq!(refused.token, token, "{name}");
                assert_eq!(refused.tell, Tell::Login(told), "{name}");
                assert!(gate.is_refused(), "{name}: the flag");
            }
            (got, want) => panic!("{name}: {got:?}, wanted {want:?}"),
        }
    }
}

/// Each refusal has its own word for the one log line (never an id), and the relink and the taken
/// checks run on the stanza's ids exactly as they do on the event's.
#[test]
fn each_refusal_has_its_own_log_word() {
    let a = paired(PN, LID);
    let word = |rules: Rules, pairs: Vec<Option<Paired>>| match Gate::new(rules).claim(&pairs) {
        Verdict::Refuse(refused) => refused.token,
        Verdict::Pass => "pass",
    };
    assert_eq!(
        word(tail_rules("9999"), vec![Some(a.clone())]),
        "expected-number"
    );
    assert_eq!(
        word(
            Rules::new(vec![taken(pn(PN), false)], None, None),
            vec![Some(a.clone())]
        ),
        "taken"
    );
    // Taken by the LID alone: the LID decides.
    assert_eq!(
        word(
            Rules::new(vec![taken(lid(LID), false)], None, None),
            vec![Some(a.clone())]
        ),
        "taken"
    );
    assert_eq!(
        word(
            Rules::new(Vec::new(), Some(relink(vec![lid(OTHER_LID)])), None),
            vec![Some(a.clone())]
        ),
        "relink"
    );
    assert_eq!(
        word(rules(), vec![Some(paired("12 34", LID))]),
        "unreadable"
    );
    assert_eq!(word(rules(), vec![Some(a)]), "pass");
}

/// A login that ends without a device closes its gate, by whichever way it ends; one that was
/// refused by the second line does not need to. Mutants: `fail` does not close; `cancel` does not.
#[tokio::test]
async fn a_login_that_ends_without_a_device_closes_its_gate() {
    let session = FakeSession::new();
    let open =
        |login: &Login| matches!(login.gate().claim(&[Some(paired(PN, LID))]), Verdict::Pass);
    assert!(
        open(&qr_login(rules())),
        "an open gate lets a good pairing through"
    );
    // Failed (the codes ran out).
    let mut login2 = qr_login(rules());
    login2
        .signal(
            &session,
            &WaSignal::QrExhausted {
                socket_closed: true,
            },
            0,
        )
        .await;
    assert!(login2.is_over());
    assert!(!open(&login2));
    // Cancelled.
    let mut login3 = qr_login(rules());
    login3.cancel(&session).await;
    assert!(!open(&login3));
    // Started and failed at once.
    let mut login4 = Login::new("qr", rules()).expect("flow");
    login4.fail_start(BackendError::internal("no client"));
    assert!(!open(&login4));
}
