//! `CoreSource`: Telegram's login page (KT-1b): which steps offer the other flow, what a banned
//! number or a refused number offers instead, and what of a typed step is put back (never a
//! secret).

use std::time::Duration;

use kourier_core::facade::{UiCommand, UiEvent};
use kourier_model::{
    AccountId, BackendError, CodeDelivery, CodeResend, CodeVia, ErrorKind, InputField, InputKind,
    LoginStep, LoginStepKind, NetworkKind, RESEND_FIELD, Redacted,
};

use super::tests::{Sent, source_with};
use super::*;
use crate::login::{NetworkEntry, NetworkMissing, network_choices};

const TG: AccountId = AccountId(7);

fn telegram_entry() -> NetworkEntry {
    NetworkEntry {
        kind: NetworkKind::Telegram,
        flows: vec![
            ("qr".into(), "Scan a QR Code".into()),
            ("phone".into(), "Phone Number".into()),
        ],
        experimental: true,
        risk_notice: Some(("t".into(), "b".into())),
        history_transfer: false,
        history: kourier_model::HistorySource::OnDemand,
        on: true,
        receipts_always_sent: true,
        missing: None,
    }
}

/// A source whose welcome page offers Telegram (on) with both ways of linking, and a login of it
/// that runs `flow` and has its account `TG`.
fn started(flow: &str) -> (CoreSource, Sent) {
    let (mut src, sent) = source_with(network_choices(
        &[NetworkEntry::plain(NetworkKind::Signal), telegram_entry()],
        false,
    ));
    src.start_login(NetworkKind::Telegram).expect("start");
    src.login.as_mut().expect("login").flow = flow.into();
    src.on_event(&UiEvent::AccountAdded { account: TG });
    (src, sent)
}

fn step(id: &str, txn: u64, kind: LoginStepKind) -> UiEvent {
    UiEvent::LoginStep {
        account: TG,
        step: LoginStep {
            step_id: id.into(),
            txn,
            kind,
        },
    }
}

fn field(id: &str, kind: InputKind, code: Option<CodeDelivery>) -> InputField {
    InputField {
        id: id.into(),
        label: format!("Backend label of {id}"),
        kind,
        default: None,
        required: true,
        note: None,
        code,
    }
}

fn input(id: &str, txn: u64, fields: Vec<InputField>) -> UiEvent {
    step(id, txn, LoginStepKind::Input { fields })
}

fn app_code(resend_secs: Option<u64>) -> CodeDelivery {
    CodeDelivery {
        via: CodeVia::App,
        resend: resend_secs.map(|s| CodeResend {
            after: Duration::from_secs(s),
            via: CodeVia::Sms,
        }),
    }
}

fn failed(id: &str, kind: ErrorKind) -> UiEvent {
    step(
        id,
        9,
        LoginStepKind::Failed {
            error: BackendError::new(kind, "refused"),
        },
    )
}

fn other_flow(src: &CoreSource) -> (String, String) {
    let v = src.login_view().expect("view");
    (v.other_flow, v.other_flow_label)
}

/// The other flow is a way on at a code to scan, a number to type and a code to enter (also
/// after a wrong one), and NOT at the password and the e-mail of the login or their retries: the
/// account is known by then (a QR scan or the code was accepted), and a switch would cancel a link
/// nearly made. Mutant: the step test of `with_flows` removed (every input step offers it).
#[test]
fn no_other_flow_is_offered_at_a_password_or_email_step_but_still_at_the_ones_before() {
    // The QR flow offers the phone flow.
    let (mut src, _) = started("qr");
    src.on_event(&step(
        "qr",
        1,
        LoginStepKind::ShowQr {
            data: Redacted::new("payload".into()),
            valid_for: None,
        },
    ));
    assert_eq!(
        other_flow(&src),
        ("phone".into(), "Link with Phone Number Instead".into())
    );
    // The phone flow: the number, the code and its retry offer the QR flow.
    let (mut src, _) = started("phone");
    src.on_event(&input(
        "phone",
        2,
        vec![field("phone", InputKind::Phone, None)],
    ));
    assert_eq!(
        other_flow(&src),
        ("qr".into(), "Scan a QR Code Instead".into())
    );
    for (id, txn) in [("code", 3), ("code-again", 4)] {
        src.on_event(&input(
            id,
            txn,
            vec![field("code", InputKind::Text, Some(app_code(None)))],
        ));
        assert_eq!(other_flow(&src).0, "qr", "{id}");
    }
    // Where a switch would cancel a link nearly made, in either flow.
    for flow in ["qr", "phone"] {
        let (mut src, _) = started(flow);
        for (id, f) in [
            ("password", field("password", InputKind::Secret, None)),
            ("password-again", field("password", InputKind::Secret, None)),
            ("email", field("email", InputKind::Text, None)),
            ("email-again", field("email", InputKind::Text, None)),
            ("email-code", field("code", InputKind::Text, None)),
            ("email-code-again", field("code", InputKind::Text, None)),
        ] {
            src.on_event(&input(id, 5, vec![f]));
            let v = src.login_view().expect("view");
            assert_eq!(v.kind, LoginViewKind::Input, "{id}");
            assert_eq!(
                (v.other_flow.as_str(), v.other_flow_label.as_str()),
                ("", ""),
                "{id} in the {flow} flow"
            );
        }
    }
}

/// A banned number, a number without an account and a login that needs a newer Kourier go back
/// with "OK": no Try Again and no other flow (asking again fails the same, and so does the other
/// way). A purchase asked for before a code is sent offers no Try Again either, but keeps the QR
/// flow, which sends no code (fix round F2). A code Kourier cannot take, and an e-mail address
/// Telegram does not accept, keep both. WhatsApp's own `(phone, Unauthorized)` keeps both too.
/// Mutants: the Telegram guard on the banned number dropped; `purchase` back in the "OK" arm.
#[test]
fn a_refusal_no_new_attempt_mends_offers_ok_and_no_other_way() {
    for id in ["phone", "registration", "update"] {
        let kind = if id == "phone" {
            ErrorKind::Unauthorized
        } else {
            ErrorKind::Unsupported
        };
        let (mut src, _) = started("phone");
        src.on_event(&failed(id, kind));
        let v = src.login_view().expect("view");
        assert_eq!(v.kind, LoginViewKind::Failed, "{id}");
        assert!(!v.retry, "no Try Again at {id}");
        assert_eq!(v.leave, "OK", "{id}");
        assert_eq!(other_flow(&src), ("".into(), "".into()), "{id}");
    }
    // A purchase before the code is sent: no Try Again, no "OK", and the QR flow avoids the
    // purchase, so the switch stays.
    let (mut src, _) = started("phone");
    src.on_event(&failed("purchase", ErrorKind::Unsupported));
    let v = src.login_view().expect("view");
    assert_eq!(v.kind, LoginViewKind::Failed);
    assert!(!v.retry && v.leave.is_empty(), "{v:?}");
    assert_eq!(
        other_flow(&src),
        ("qr".into(), "Scan a QR Code Instead".into())
    );
    // An e-mail address Telegram does not accept: Try Again and the QR flow (no login e-mail is
    // asked after a scan).
    let (mut src, _) = started("phone");
    src.on_event(&failed("email", ErrorKind::Unsupported));
    let v = src.login_view().expect("view");
    assert!(v.retry && v.leave.is_empty());
    assert_eq!(
        other_flow(&src),
        ("qr".into(), "Scan a QR Code Instead".into())
    );
    // A code Kourier cannot take: Try Again, and the QR flow is the way on.
    let (mut src, _) = started("phone");
    src.on_event(&failed("code", ErrorKind::Unsupported));
    let v = src.login_view().expect("view");
    assert!(v.retry && v.leave.is_empty());
    assert_eq!(
        other_flow(&src),
        ("qr".into(), "Scan a QR Code Instead".into())
    );
    // WhatsApp's `(phone, Unauthorized)` is the phone's rejection: it keeps both.
    let mut wa_entry = telegram_entry();
    wa_entry.kind = NetworkKind::WhatsApp;
    wa_entry.flows = vec![
        ("qr".into(), "QR code".into()),
        ("pair-code".into(), "Phone number".into()),
    ];
    let (mut wa, _) = source_with(network_choices(
        &[NetworkEntry::plain(NetworkKind::Signal), wa_entry],
        false,
    ));
    wa.start_login(NetworkKind::WhatsApp).expect("start");
    wa.login.as_mut().expect("login").flow = "pair-code".into();
    wa.on_event(&UiEvent::AccountAdded { account: TG });
    wa.on_event(&failed("phone", ErrorKind::Unauthorized));
    let v = wa.login_view().expect("view");
    assert_eq!(v.error, "The phone rejected the link.");
    assert!(v.retry && v.leave.is_empty());
    assert_eq!(other_flow(&wa).0, "qr");
}

/// What was typed comes back into a step of the same id, never a secret: the wrong password of
/// `password-again` is NOT put back into the field of the next `password-again` (the second wrong
/// try used to refill the first), a code (not secret) is, and the request for a new code (a value
/// that is no field of its step) is nothing to put back. Mutant: every typed value is kept.
#[test]
fn a_typed_password_is_not_kept() {
    let (mut src, sent) = started("phone");
    let pw = || vec![field("password", InputKind::Secret, None)];
    src.on_event(&input("password", 5, pw()));
    src.submit_login(vec![("password".into(), "first wrong".into())])
        .expect("submit");
    src.on_event(&input("password-again", 6, pw()));
    assert_eq!(src.login_view().expect("view").fields[0].value, "");
    // The second wrong try: the same id again, the field stays empty.
    src.submit_login(vec![("password".into(), "second wrong".into())])
        .expect("submit");
    src.on_event(&input("password-again", 7, pw()));
    let v = src.login_view().expect("view");
    assert_eq!(v.fields[0].value, "", "a wrong password is not put back");
    assert_eq!(v.fields[0].kind, "secret");
    let typed = src.login.as_ref().expect("login").typed.as_ref();
    assert!(
        typed.is_some_and(|t| t.values.is_empty()),
        "no secret stays in memory at all"
    );
    // The password itself reached the core, as typed.
    let sent_values: Vec<(String, String)> = sent
        .borrow()
        .iter()
        .filter_map(|c| match c {
            UiCommand::SubmitLoginInput { values, .. } => Some(values.clone()),
            _ => None,
        })
        .flatten()
        .map(|(k, v)| (k, v.expose().clone()))
        .collect();
    assert_eq!(
        sent_values,
        [
            ("password".to_owned(), "first wrong".to_owned()),
            ("password".to_owned(), "second wrong".to_owned())
        ]
    );
    // A wrong code (not a secret) IS put back, so a digit can be corrected.
    let (mut src, _) = started("phone");
    let code = || vec![field("code", InputKind::Text, Some(app_code(Some(30))))];
    src.on_event(&input("code-again", 3, code()));
    src.submit_login(vec![("code".into(), "12344".into())])
        .expect("submit");
    src.on_event(&input("code-again", 4, code()));
    assert_eq!(src.login_view().expect("view").fields[0].value, "12344");
    // A request for a new code carries no field of the step: nothing is kept, and the code typed
    // before it is forgotten with it (the step is shown again as it was).
    src.submit_login(vec![(RESEND_FIELD.into(), "1".into())])
        .expect("submit");
    let typed = src.login.as_ref().expect("login").typed.as_ref();
    assert!(typed.is_some_and(|t| t.values.is_empty()));
    src.on_event(&input("code-again", 5, code()));
    assert_eq!(src.login_view().expect("view").fields[0].value, "");
}

/// A Telegram this build cannot log in to starts no login (its row is for Settings), and its
/// reason stays on the row for the page.
#[test]
fn a_missing_telegram_starts_no_login() {
    let entry = NetworkEntry {
        missing: Some(NetworkMissing::ApiKey),
        ..telegram_entry()
    };
    let (mut src, sent) = source_with(network_choices(
        &[NetworkEntry::plain(NetworkKind::Signal), entry],
        false,
    ));
    assert!(src.start_login(NetworkKind::Telegram).is_err());
    assert!(sent.borrow().is_empty(), "nothing was sent");
    let row = src
        .networks()
        .into_iter()
        .find(|n| n.kind == NetworkKind::Telegram)
        .expect("the row");
    assert_eq!(row.missing, Some(NetworkMissing::ApiKey));
}
