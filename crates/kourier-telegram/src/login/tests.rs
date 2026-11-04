//! The login machine on synthetic states and inputs (TG-2 C6; critic B1, M6, L7, M10). Synthetic
//! values only (the number `+999068204719`, the peer `+999075390162`, the code `12345`, the
//! password `synthetic-password-3141`, the hint `synthetic hint`, `ada@example.invalid`); nothing
//! here prints a typed value: the assertion messages name cases, never values.

use serde_json::json;

use super::*;

const OWN_DIGITS: &str = "999068204719";
const PEER_DIGITS: &str = "999075390162";
const CODE: &str = "12345";
const PASSWORD: &str = "synthetic-password-3141";
const EMAIL: &str = "ada@example.invalid";

fn uid(id: &str) -> UserId {
    UserId::parse(id).expect("a synthetic id")
}

fn taken(ids: &[(&str, bool)]) -> Vec<TakenIdentity> {
    ids.iter()
        .map(|&(id, logged_out)| TakenIdentity {
            user: uid(id),
            logged_out,
        })
        .collect()
}

fn rules(taken_ids: &[(&str, bool)]) -> Rules {
    Rules::new(taken(taken_ids), None, None)
}

fn relink_rules(expect_own: &[&str]) -> Rules {
    Rules {
        taken: Vec::new(),
        expect_own: expect_own.iter().map(|id| uid(id)).collect(),
        relink: true,
        suffix: None,
    }
}

fn tail_rules(tail: &str) -> Rules {
    Rules::new(Vec::new(), None, Some(Redacted::new(tail.to_owned())))
}

fn state(v: Value) -> AuthorizationState {
    serde_json::from_value(v).expect("a synthetic state")
}

fn wait_phone() -> AuthorizationState {
    state(json!({"@type": "authorizationStateWaitPhoneNumber"}))
}

fn wait_code(code_type: Value, next_type: Value, timeout: i32) -> AuthorizationState {
    state(json!({
        "@type": "authorizationStateWaitCode",
        "code_info": {
            "@type": "authenticationCodeInfo",
            "phone_number": format!("+{OWN_DIGITS}"),
            "type": code_type,
            "next_type": next_type,
            "timeout": timeout,
        }
    }))
}

fn wait_code_telegram() -> AuthorizationState {
    wait_code(
        json!({"@type": "authenticationCodeTypeTelegramMessage", "length": 5}),
        Value::Null,
        0,
    )
}

fn wait_password(hint: &str) -> AuthorizationState {
    state(json!({
        "@type": "authorizationStateWaitPassword",
        "password_hint": hint,
        "has_recovery_email_address": false,
        "recovery_email_address_pattern": "",
    }))
}

fn ready() -> AuthorizationState {
    state(json!({"@type": "authorizationStateReady"}))
}

fn values(pairs: &[(&str, &str)]) -> Vec<(String, Redacted<String>)> {
    pairs
        .iter()
        .map(|&(k, v)| (k.to_owned(), Redacted::new(v.to_owned())))
        .collect()
}

fn auth_error(class: TdClass) -> LinkError {
    LinkError::Auth {
        code: 400,
        class,
        retry_after_s: None,
    }
}

/// The steps among `outs`: (step id, txn).
fn steps(outs: &[Out]) -> Vec<(String, u64)> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Step(s) => Some((s.step_id.clone(), s.txn)),
            _ => None,
        })
        .collect()
}

/// The requests among `outs`, parsed (the test reads what the machine would send).
fn requests(outs: &[Out]) -> Vec<(ReqKind, Value)> {
    outs.iter()
        .filter_map(|o| match o {
            Out::Request(r) => Some((
                r.kind,
                serde_json::from_str(r.json.as_str()).expect("a request is JSON"),
            )),
            _ => None,
        })
        .collect()
}

/// The one step among `outs`.
fn only_step(outs: &[Out]) -> &LoginStep {
    let all: Vec<&LoginStep> = outs
        .iter()
        .filter_map(|o| match o {
            Out::Step(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(all.len(), 1, "exactly one step");
    all[0]
}

fn failed_kind(step: &LoginStep) -> ErrorKind {
    match &step.kind {
        LoginStepKind::Failed { error } => error.kind.clone(),
        _ => panic!("the step {} is not Failed", step.step_id),
    }
}

fn input_fields(step: &LoginStep) -> &[InputField] {
    match &step.kind {
        LoginStepKind::Input { fields } => fields,
        _ => panic!("the step {} is no input step", step.step_id),
    }
}

/// A phone login at the code step: the number sent and answered `ok`, the code step shown.
fn at_code(code_state: AuthorizationState) -> Login {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(wait_phone(), 0);
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("phone", "+999068204719")]), 0);
    assert_eq!(requests(&outs).len(), 1, "the number is sent");
    login.on_answer(ReqKind::SetPhone, Ok(json!({"@type": "ok"})), 0);
    login.on_state(code_state, 1_000);
    login
}

fn at_password(hint: &str) -> Login {
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    login.on_input(txn, values(&[("code", CODE)]), 2_000);
    login.on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 2_000);
    login.on_state(wait_password(hint), 2_000);
    login
}

fn me(id: i64, phone: &str) -> Value {
    json!({
        "@type": "user",
        "id": id,
        "first_name": "Ada",
        "last_name": "Test",
        "phone_number": phone,
    })
}

/// A login at `linking` with `getMe` asked.
fn at_linking(rules: Rules) -> Login {
    let mut login = Login::new(Flow::Qr, rules);
    login.on_state(wait_phone(), 0);
    login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), 0);
    let outs = login.on_state(ready(), 10);
    assert_eq!(steps(&outs), vec![(step::LINKING.to_owned(), login.txn())]);
    assert_eq!(requests(&outs)[0].0, ReqKind::GetMe, "getMe after Ready");
    login
}

// ---- QR -----------------------------------------------------------------------------------

/// The QR request carries the `tg:` ids of the profile's other Telegram accounts as numbers, and
/// only those (a `pn:` id, a foreign scheme are not user ids); no step comes before the link.
#[test]
fn qr_asks_with_the_other_accounts_telegram_ids() {
    let mut login = Login::new(
        Flow::Qr,
        rules(&[
            ("tg:111", false),
            ("pn:999075390162", false),
            ("tg:222", true),
            ("lid:5", false),
        ]),
    );
    let outs = login.on_state(wait_phone(), 0);
    assert!(steps(&outs).is_empty(), "no step before the first link");
    let reqs = requests(&outs);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, ReqKind::RequestQr);
    assert_eq!(
        reqs[0].1,
        json!({"@type": "requestQrCodeAuthentication", "other_user_ids": [111, 222]})
    );
    assert_eq!(login.pending(), Some(ReqKind::RequestQr));
}

/// M6: the QR request is built once per login; a second WaitPhoneNumber ends the login Expired.
#[test]
fn a_second_wait_phone_number_in_the_qr_flow_ends_the_login_and_asks_nothing() {
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), 0);
    login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), 0);
    let outs = login.on_state(wait_phone(), 5);
    assert!(requests(&outs).is_empty(), "no second QR request");
    let step = only_step(&outs);
    assert_eq!(step.step_id, step::QR);
    assert_eq!(failed_kind(step), ErrorKind::Expired);
    assert!(login.is_ended());
}

/// Every new link is a new `qr` step (txn + 1) holding that link; the eleventh ends the login.
#[test]
fn every_new_link_is_a_new_qr_step_and_the_tenth_is_the_last() {
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), 0);
    login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), 0);
    for n in 1..=QR_MAX_LINKS {
        let link = format!("tg://login?token=c3ludGhldGlj{n}");
        let outs = login.on_state(
            state(json!({"@type": "authorizationStateWaitOtherDeviceConfirmation", "link": link})),
            u64::from(n) * 1_000,
        );
        let step = only_step(&outs);
        assert_eq!(step.step_id, step::QR, "link {n}");
        assert_eq!(step.txn, u64::from(n), "link {n}");
        match &step.kind {
            LoginStepKind::ShowQr { data, valid_for } => {
                assert!(data.expose() == &link, "link {n} is shown as given");
                assert_eq!(*valid_for, None);
            }
            _ => panic!("link {n} is no ShowQr"),
        }
    }
    let outs = login.on_state(
        state(json!({"@type": "authorizationStateWaitOtherDeviceConfirmation", "link": "tg://login?token=eA"})),
        20_000,
    );
    assert_eq!(failed_kind(only_step(&outs)), ErrorKind::Expired);
    assert!(login.is_ended());
}

/// M6: five minutes after the request the QR login ends Expired; later links show nothing.
#[test]
fn the_qr_login_ends_after_five_minutes() {
    let t0 = 1_000_000;
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), t0);
    assert_eq!(login.deadline_ms(), Some(t0 + QR_BOUND_MS));
    assert!(login.on_tick(t0 + QR_BOUND_MS - 1).is_empty());
    let outs = login.on_tick(t0 + QR_BOUND_MS);
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), failed_kind(step)),
        (step::QR, ErrorKind::Expired)
    );
    assert_eq!(login.deadline_ms(), None);
    let later = login.on_state(
        state(json!({"@type": "authorizationStateWaitOtherDeviceConfirmation", "link": "tg://login?token=eA"})),
        t0 + QR_BOUND_MS + 1,
    );
    assert!(later.is_empty(), "nothing after the end");
}

// ---- phone ---------------------------------------------------------------------------------

/// A typed number of another account of the profile is refused BEFORE any request (the code
/// would go to that account), for both `logged_out` values.
#[test]
fn a_typed_number_of_another_account_is_refused_before_any_request() {
    for logged_out in [false, true] {
        let mut login = Login::new(Flow::Phone, rules(&[("pn:999068204719", logged_out)]));
        let shown = login.on_state(wait_phone(), 0);
        let phone = only_step(&shown);
        assert_eq!((phone.step_id.as_str(), phone.txn), (step::PHONE, 1));
        assert_eq!(input_fields(phone)[0].kind, InputKind::Phone);
        let outs = login.on_input(1, values(&[("phone", "+999 068 204 719")]), 0);
        assert!(requests(&outs).is_empty(), "no request ({logged_out})");
        let step = only_step(&outs);
        assert_eq!(step.step_id, step::PHONE);
        assert_eq!(failed_kind(step), ErrorKind::AlreadyAdded { logged_out });
    }
}

/// A relink whose account has a `pn:` id refuses another number before any request; its own
/// number goes on.
#[test]
fn a_relink_to_another_number_is_refused_before_any_request() {
    let mut login = Login::new(Flow::Phone, relink_rules(&["tg:7", "pn:999075390162"]));
    login.on_state(wait_phone(), 0);
    let outs = login.on_input(1, values(&[("phone", "+999068204719")]), 0);
    assert!(requests(&outs).is_empty());
    assert_eq!(failed_kind(only_step(&outs)), ErrorKind::InvalidInput);

    let mut login = Login::new(Flow::Phone, relink_rules(&["tg:7", "pn:999075390162"]));
    login.on_state(wait_phone(), 0);
    let outs = login.on_input(1, values(&[("phone", "+999075390162")]), 0);
    assert_eq!(requests(&outs).len(), 1, "the account's own number is sent");
}

/// The run tool's tail: a match is sent, a mismatch and an empty tail are refused first.
#[test]
fn a_number_without_the_expected_tail_is_refused_before_any_request() {
    for (tail, sent) in [
        ("4719", true),
        ("204719", true),
        ("4718", false),
        ("", false),
    ] {
        let mut login = Login::new(Flow::Phone, tail_rules(tail));
        login.on_state(wait_phone(), 0);
        let outs = login.on_input(1, values(&[("phone", "+999068204719")]), 0);
        assert_eq!(
            !requests(&outs).is_empty(),
            sent,
            "tail of {} digits",
            tail.len()
        );
        if !sent {
            assert_eq!(
                failed_kind(only_step(&outs)),
                ErrorKind::NotTheExpectedAccount
            );
        }
    }
}

/// A text that is no number fails at `phone` and sends nothing.
#[test]
fn a_malformed_number_fails_at_phone_and_sends_nothing() {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(wait_phone(), 0);
    let outs = login.on_input(1, values(&[("phone", "+999 06a8")]), 0);
    assert!(requests(&outs).is_empty());
    assert_eq!(failed_kind(only_step(&outs)), ErrorKind::InvalidInput);
}

/// The number request: `+<digits>` and every setting off, no Firebase, no tokens.
#[test]
fn the_phone_request_carries_every_setting_false() {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(wait_phone(), 0);
    let outs = login.on_input(1, values(&[("phone", "00 999 068 204 719")]), 0);
    let reqs = requests(&outs);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, ReqKind::SetPhone);
    assert!(
        reqs[0].1
            == json!({
                "@type": "setAuthenticationPhoneNumber",
                "phone_number": "+999068204719",
                "settings": {
                    "@type": "phoneNumberAuthenticationSettings",
                    "allow_flash_call": false,
                    "allow_missed_call": false,
                    "is_current_phone_number": false,
                    "has_unknown_phone_number": false,
                    "allow_sms_retriever_api": false,
                    "firebase_authentication_settings": null,
                    "authentication_tokens": [],
                }
            }),
        "the number request differs"
    );
}

// ---- code ----------------------------------------------------------------------------------

/// The code step's label names how the code came; a type Kourier cannot take ends the login.
#[test]
fn the_code_step_names_its_delivery() {
    for (ty, label, via, hint) in [
        (
            json!({"@type": "authenticationCodeTypeTelegramMessage", "length": 5}),
            "Code (sent to your Telegram app)",
            CodeVia::App,
            None,
        ),
        (
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            "Code (sent by SMS)",
            CodeVia::Sms,
            None,
        ),
        (
            json!({"@type": "authenticationCodeTypeSmsWord", "first_letter": "s"}),
            "Code (sent by SMS)",
            CodeVia::SmsWord,
            Some("s"),
        ),
        (
            json!({"@type": "authenticationCodeTypeSmsPhrase", "first_word": "synthetic"}),
            "Code (sent by SMS)",
            CodeVia::SmsPhrase,
            Some("synthetic"),
        ),
        (
            json!({"@type": "authenticationCodeTypeCall", "length": 5}),
            "Code (from the call)",
            CodeVia::Call,
            None,
        ),
        (
            json!({"@type": "authenticationCodeTypeFragment", "url": "https://example.invalid/", "length": 5}),
            "Code (sent to fragment.com)",
            CodeVia::Fragment,
            None,
        ),
        (
            json!({"@type": "authenticationCodeTypeSomethingNew"}),
            "Code",
            CodeVia::Other,
            None,
        ),
    ] {
        let login_outs = {
            let mut login = Login::new(Flow::Phone, rules(&[]));
            login.on_state(wait_code(ty, Value::Null, 0), 0)
        };
        let step = only_step(&login_outs);
        assert_eq!(step.step_id, step::CODE, "{label}");
        let fields = input_fields(step);
        assert_eq!(fields.len(), 1);
        assert_eq!(
            (fields[0].id.as_str(), fields[0].label.as_str()),
            ("code", label)
        );
        assert_eq!(fields[0].kind, InputKind::Text);
        assert!(fields[0].required);
        // The typed delivery the UI words (KT-1b): the way it came, no resend without a next
        // type; an SMS word or phrase puts its first letter or word in the note.
        assert_eq!(
            fields[0].code,
            Some(CodeDelivery { via, resend: None }),
            "{label}"
        );
        let note = fields[0]
            .note
            .as_ref()
            .and_then(|n| n.explanation.as_deref());
        assert_eq!(note, hint, "{label}");
    }
    for ty in [
        json!({"@type": "authenticationCodeTypeFlashCall", "pattern": "999*"}),
        json!({"@type": "authenticationCodeTypeMissedCall", "phone_number_prefix": "999", "length": 4}),
        json!({"@type": "authenticationCodeTypeFirebaseAndroid"}),
        json!({"@type": "authenticationCodeTypeFirebaseIos"}),
    ] {
        let mut login = Login::new(Flow::Phone, rules(&[]));
        let outs = login.on_state(wait_code(ty, Value::Null, 0), 0);
        let step = only_step(&outs);
        assert_eq!(
            (step.step_id.as_str(), failed_kind(step)),
            (step::CODE, ErrorKind::Unsupported)
        );
    }
}

/// A resend is asked only when TDLib names a next type AND its timeout has passed since the
/// step; otherwise the same step comes again and nothing is sent.
#[test]
fn resend_only_when_telegram_allows_it() {
    // No next type: never.
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("resend", "1")]), 1_000_000);
    assert!(requests(&outs).is_empty());
    assert_eq!(steps(&outs), vec![(step::CODE.to_owned(), txn + 1)]);

    // A next type and 30 s: not at 29 s after the step (shown at 1 000 ms), then yes.
    let mut login = at_code(wait_code(
        json!({"@type": "authenticationCodeTypeTelegramMessage", "length": 5}),
        json!({"@type": "authenticationCodeTypeSms", "length": 5}),
        30,
    ));
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("resend", "1")]), 30_999);
    assert!(requests(&outs).is_empty(), "before the timeout");
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("resend", "1")]), 31_000);
    let reqs = requests(&outs);
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].1,
        json!({"@type": "resendAuthenticationCode", "reason": {"@type": "resendCodeReasonUserRequest"}})
    );
}

/// What the code field of `step` states: the way it came and the new code's wait and way.
fn delivery_of(step: &LoginStep) -> (CodeVia, Option<(Duration, CodeVia)>) {
    let fields = input_fields(step);
    assert_eq!(fields.len(), 1);
    let delivery = fields[0].code.expect("the code field states its delivery");
    (delivery.via, delivery.resend.map(|r| (r.after, r.via)))
}

/// KT-1b: the code field states how the code came and the wait that REMAINS before a new one may be
/// asked for, at EVERY showing of the step (the first, a wrong entry's `code-again`, an early ask's
/// same step again), so a page that restarts its timer at each step is never late; 0 at or past the
/// time. A refused resend (J2) leaves none, and so does a next type Kourier cannot take. Mutants:
/// the whole timeout at every showing; a resend from any next type.
#[test]
fn the_code_step_states_its_delivery_and_when_a_new_code_may_come() {
    let secs = Duration::from_secs;
    let app_then_sms = || {
        wait_code(
            json!({"@type": "authenticationCodeTypeTelegramMessage", "length": 5}),
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            30,
        )
    };
    // Shown at 1 000 ms with a wait of 30 s: due at 31 000 ms.
    let mut login = Login::new(Flow::Phone, rules(&[]));
    let outs = login.on_state(app_then_sms(), 1_000);
    assert_eq!(
        delivery_of(only_step(&outs)),
        (CodeVia::App, Some((secs(30), CodeVia::Sms))),
        "the whole wait at the first showing"
    );
    // A wrong code at 11 000 ms: `code-again` states the 20 s that are left.
    let txn = login.txn();
    login.on_input(txn, values(&[("code", "54321")]), 10_500);
    let outs = login.on_answer(
        ReqKind::CheckCode,
        Err(auth_error(TdClass::PhoneCodeInvalid)),
        11_000,
    );
    let again = only_step(&outs);
    assert_eq!(again.step_id, step::CODE_AGAIN);
    assert_eq!(
        delivery_of(again),
        (CodeVia::App, Some((secs(20), CodeVia::Sms)))
    );
    // An early ask at 21 000 ms sends nothing and shows the same step with 10 s left.
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("resend", "1")]), 21_000);
    assert!(requests(&outs).is_empty());
    let same = only_step(&outs);
    assert_eq!(same.step_id, step::CODE);
    assert_eq!(
        delivery_of(same),
        (CodeVia::App, Some((secs(10), CodeVia::Sms)))
    );
    // At the time: the resend is built. Telegram refuses it (J2): the step again, no new code offered.
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("resend", "1")]), 31_000);
    assert_eq!(requests(&outs)[0].0, ReqKind::Resend);
    let outs = login.on_answer(ReqKind::Resend, Err(LinkError::Td { code: 406 }), 31_100);
    assert_eq!(
        delivery_of(only_step(&outs)),
        (CodeVia::App, None),
        "a refused resend leaves none"
    );

    // A resend Telegram takes brings a new code state with its own way and wait: shown at 32 000.
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(app_then_sms(), 1_000);
    let txn = login.txn();
    login.on_input(txn, values(&[("resend", "1")]), 31_500);
    login.on_answer(ReqKind::Resend, Ok(json!({"@type": "ok"})), 31_600);
    let outs = login.on_state(
        wait_code(
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            json!({"@type": "authenticationCodeTypeCall", "length": 5}),
            60,
        ),
        32_000,
    );
    assert_eq!(
        delivery_of(only_step(&outs)),
        (CodeVia::Sms, Some((secs(60), CodeVia::Call)))
    );

    // At or past the time the wait is 0: the button may show at once. Zero from TDLib is zero.
    let mut login = Login::new(Flow::Phone, rules(&[]));
    let outs = login.on_state(
        wait_code(
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            json!({"@type": "authenticationCodeTypeCall", "length": 5}),
            0,
        ),
        1_000,
    );
    assert_eq!(
        delivery_of(only_step(&outs)),
        (CodeVia::Sms, Some((Duration::ZERO, CodeVia::Call)))
    );
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(app_then_sms(), 1_000);
    let txn = login.txn();
    login.on_input(txn, values(&[("code", "54321")]), 90_000);
    let outs = login.on_answer(
        ReqKind::CheckCode,
        Err(auth_error(TdClass::PhoneCodeInvalid)),
        90_000,
    );
    assert_eq!(
        delivery_of(only_step(&outs)),
        (CodeVia::App, Some((Duration::ZERO, CodeVia::Sms))),
        "past the time: 0, never a negative wait"
    );

    // A next type Kourier cannot take offers no new code, and an ask for one builds no request
    // even after the time (the login would end in `Unsupported`); an unknown next type is a way
    // with no word.
    for next in [
        json!({"@type": "authenticationCodeTypeFlashCall", "pattern": "999*"}),
        json!({"@type": "authenticationCodeTypeMissedCall", "phone_number_prefix": "999", "length": 4}),
        json!({"@type": "authenticationCodeTypeFirebaseAndroid"}),
        json!({"@type": "authenticationCodeTypeFirebaseIos"}),
    ] {
        let mut login = Login::new(Flow::Phone, rules(&[]));
        let outs = login.on_state(
            wait_code(
                json!({"@type": "authenticationCodeTypeSms", "length": 5}),
                next.clone(),
                0,
            ),
            1_000,
        );
        assert_eq!(
            delivery_of(only_step(&outs)),
            (CodeVia::Sms, None),
            "{next}"
        );
        let txn = login.txn();
        let outs = login.on_input(txn, values(&[("resend", "1")]), 99_000);
        assert!(requests(&outs).is_empty(), "{next}: no resend is built");
        assert_eq!(only_step(&outs).step_id, step::CODE);
    }
    let mut login = Login::new(Flow::Phone, rules(&[]));
    let outs = login.on_state(
        wait_code(
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            json!({"@type": "authenticationCodeTypeSomethingNew"}),
            0,
        ),
        1_000,
    );
    assert_eq!(
        delivery_of(only_step(&outs)),
        (CodeVia::Sms, Some((Duration::ZERO, CodeVia::Other)))
    );

    // The first letter of an SMS word stays the note at every showing of the step.
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(
        wait_code(
            json!({"@type": "authenticationCodeTypeSmsWord", "first_letter": "k"}),
            Value::Null,
            0,
        ),
        1_000,
    );
    let txn = login.txn();
    login.on_input(txn, values(&[("code", "54321")]), 2_000);
    let outs = login.on_answer(
        ReqKind::CheckCode,
        Err(auth_error(TdClass::PhoneCodeInvalid)),
        2_000,
    );
    let note = input_fields(only_step(&outs))[0]
        .note
        .as_ref()
        .and_then(|n| n.explanation.as_deref());
    assert_eq!(note, Some("k"));
    // Foreign text: one plain line (a hostile word is cut like the hint).
    let mut login = Login::new(Flow::Phone, rules(&[]));
    let outs = login.on_state(
        wait_code(
            json!({"@type": "authenticationCodeTypeSmsPhrase", "first_word": "a\u{202e}b\nc"}),
            Value::Null,
            0,
        ),
        1_000,
    );
    let note = input_fields(only_step(&outs))[0]
        .note
        .as_ref()
        .and_then(|n| n.explanation.as_deref());
    assert_eq!(note, Some("ab c"), "no bidi control, one line");
}

/// B1: a wrong code shows the code step again as `code-again` (txn + 1, the same field) and
/// builds no request; the next code is sent from that step.
#[test]
fn a_wrong_code_reshows_the_code_step_as_code_again_and_builds_no_request() {
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    let outs = login.on_input(txn, values(&[("code", "54321")]), 2_000);
    assert!(
        requests(&outs)[0].1 == json!({"@type": "checkAuthenticationCode", "code": "54321"}),
        "the code request differs"
    );
    let outs = login.on_answer(
        ReqKind::CheckCode,
        Err(auth_error(TdClass::PhoneCodeInvalid)),
        2_100,
    );
    assert!(requests(&outs).is_empty(), "no request on a wrong code");
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), step.txn),
        (step::CODE_AGAIN, txn + 1)
    );
    assert_eq!(input_fields(step)[0].id, "code");
    assert!(!login.is_ended(), "a wrong code does not end the login");
    let outs = login.on_input(txn + 1, values(&[("code", CODE)]), 3_000);
    assert_eq!(requests(&outs)[0].0, ReqKind::CheckCode);
    for class in [TdClass::PhoneCodeEmpty, TdClass::CodeInvalid] {
        let mut login = at_code(wait_code_telegram());
        let txn = login.txn();
        login.on_input(txn, values(&[("code", CODE)]), 2_000);
        let outs = login.on_answer(ReqKind::CheckCode, Err(auth_error(class)), 2_000);
        assert_eq!(only_step(&outs).step_id, step::CODE_AGAIN, "{class:?}");
    }
}

/// An expired code ends the login at `code` (the new login asks for a new code); a later input
/// is ignored.
#[test]
fn an_expired_code_ends_the_login_at_code() {
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    login.on_input(txn, values(&[("code", CODE)]), 2_000);
    let outs = login.on_answer(
        ReqKind::CheckCode,
        Err(auth_error(TdClass::PhoneCodeExpired)),
        2_000,
    );
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), failed_kind(step)),
        (step::CODE, ErrorKind::Expired)
    );
    assert!(
        login
            .on_input(login.txn(), values(&[("code", CODE)]), 3_000)
            .is_empty()
    );
}

// ---- password, e-mail ----------------------------------------------------------------------

/// The password step: one Secret field; Telegram's hint is the field's NOTE (one line), never in
/// its label; an empty hint gives no note; `Debug` of the step holds no hint.
#[test]
fn the_password_step_is_secret_and_carries_the_hint_only_as_its_note() {
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    login.on_input(txn, values(&[("code", CODE)]), 2_000);
    login.on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 2_000);
    let outs = login.on_state(wait_password("synthetic\n  hint\t"), 2_000);
    let step = only_step(&outs);
    assert_eq!(step.step_id, step::PASSWORD);
    let field = &input_fields(step)[0];
    assert_eq!(
        (field.id.as_str(), field.kind),
        ("password", InputKind::Secret)
    );
    assert!(
        !field.label.contains("synthetic"),
        "the hint is not in the label"
    );
    assert_eq!(
        field.note,
        Some(NetworkNote {
            explanation: Some("synthetic hint".to_owned()),
            appeal_url: None,
        })
    );
    assert!(
        !format!("{outs:?}").contains("synthetic"),
        "Debug shows no hint"
    );

    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    login.on_input(txn, values(&[("code", CODE)]), 2_000);
    login.on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 2_000);
    let outs = login.on_state(wait_password(" \n "), 2_000);
    assert_eq!(input_fields(only_step(&outs))[0].note, None);
}

/// B1: a wrong password shows `password-again` with the same note; the password request is the
/// typed value, JSON-escaped (M10).
#[test]
fn a_wrong_password_reshows_password_again() {
    let mut login = at_password("synthetic hint");
    let txn = login.txn();
    let typed = "synthetic \"pass\\word\"\nü 3141";
    let outs = login.on_input(txn, values(&[("password", typed)]), 3_000);
    let reqs = requests(&outs);
    assert_eq!(reqs[0].0, ReqKind::CheckPassword);
    assert!(
        reqs[0].1 == json!({"@type": "checkAuthenticationPassword", "password": typed}),
        "the password comes back as typed"
    );
    let outs = login.on_answer(
        ReqKind::CheckPassword,
        Err(auth_error(TdClass::PasswordHashInvalid)),
        3_000,
    );
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), step.txn),
        (step::PASSWORD_AGAIN, txn + 1)
    );
    assert!(input_fields(step)[0].note.is_some(), "the hint stays");
    assert!(!login.is_ended());
    let outs = login.on_input(txn + 1, values(&[("password", PASSWORD)]), 4_000);
    assert_eq!(requests(&outs).len(), 1);
}

/// His U-3 answer: the e-mail address and its code are typed in Kourier; wrong ones show the
/// retry ids; a refused address ends the login.
#[test]
fn email_steps() {
    let mut login = at_code(wait_code_telegram());
    let outs = login.on_state(
        state(json!({"@type": "authorizationStateWaitEmailAddress", "allow_apple_id": true, "allow_google_id": true})),
        2_000,
    );
    let step = only_step(&outs);
    assert_eq!(step.step_id, step::EMAIL);
    assert_eq!(input_fields(step)[0].id, "email");
    let txn = step.txn;
    let outs = login.on_input(txn, values(&[("email", EMAIL)]), 2_000);
    assert_eq!(
        requests(&outs)[0].1,
        json!({"@type": "setAuthenticationEmailAddress", "email_address": EMAIL})
    );
    let outs = login.on_answer(
        ReqKind::SetEmail,
        Err(auth_error(TdClass::EmailInvalid)),
        2_000,
    );
    assert_eq!(only_step(&outs).step_id, step::EMAIL_AGAIN);
    let txn = login.txn();
    login.on_input(txn, values(&[("email", EMAIL)]), 2_500);
    login.on_answer(ReqKind::SetEmail, Ok(json!({"@type": "ok"})), 2_500);
    let outs = login.on_state(
        state(json!({
            "@type": "authorizationStateWaitEmailCode",
            "code_info": {"@type": "emailAddressAuthenticationCodeInfo", "email_address_pattern": "a*@example.invalid", "length": 6},
        })),
        3_000,
    );
    let step = only_step(&outs);
    assert_eq!(step.step_id, step::EMAIL_CODE);
    let txn = step.txn;
    let outs = login.on_input(txn, values(&[("code", CODE)]), 3_000);
    assert_eq!(
        requests(&outs)[0].1,
        json!({"@type": "checkAuthenticationEmailCode", "code": {"@type": "emailAddressAuthenticationCode", "code": CODE}})
    );
    let outs = login.on_answer(
        ReqKind::CheckEmailCode,
        Err(auth_error(TdClass::EmailCodeInvalid)),
        3_000,
    );
    assert_eq!(only_step(&outs).step_id, step::EMAIL_CODE_AGAIN);

    let mut login = at_code(wait_code_telegram());
    login.on_state(
        state(json!({"@type": "authorizationStateWaitEmailAddress"})),
        2_000,
    );
    let txn = login.txn();
    login.on_input(txn, values(&[("email", EMAIL)]), 2_000);
    let outs = login.on_answer(
        ReqKind::SetEmail,
        Err(auth_error(TdClass::EmailNotAllowed)),
        2_000,
    );
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), failed_kind(step)),
        (step::EMAIL, ErrorKind::Unsupported)
    );
}

// ---- refusals ------------------------------------------------------------------------------

/// Registration is refused at `registration` and nothing is built for it (never `registerUser`).
#[test]
fn registration_is_refused_and_register_user_is_never_built() {
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    login.on_input(txn, values(&[("code", CODE)]), 2_000);
    let mut outs = login.on_state(
        state(json!({"@type": "authorizationStateWaitRegistration", "terms_of_service": {"@type": "termsOfService"}})),
        2_000,
    );
    outs.extend(login.on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 2_000));
    assert!(requests(&outs).is_empty(), "nothing is sent");
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), failed_kind(step)),
        (step::REGISTRATION, ErrorKind::Unsupported)
    );
    assert!(login.is_ended());
}

/// A purchase to log in is refused at `purchase`; nothing is sent.
#[test]
fn premium_purchase_is_refused() {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    let outs = login.on_state(
        state(json!({"@type": "authorizationStateWaitPremiumPurchase", "store_product_id": "x"})),
        0,
    );
    assert!(requests(&outs).is_empty());
    let step = only_step(&outs);
    assert_eq!(
        (step.step_id.as_str(), failed_kind(step)),
        (step::PURCHASE, ErrorKind::Unsupported)
    );
}

/// KT-1b: the three things Telegram can ask for that Kourier does not do fail at a step id of their
/// own, wherever the login stood, so the UI words each (it keys on these three strings and on the
/// `Unsupported` kind). `update` comes from TDLib's `UPDATE_APP_TO_LOGIN` to any request (the number,
/// the code, a resend). Mutant: a refusal back at `phone`.
#[test]
fn registration_purchase_and_update_fail_at_their_own_steps() {
    assert_eq!(
        (step::REGISTRATION, step::PURCHASE, step::UPDATE),
        ("registration", "purchase", "update"),
        "the words the UI keys on"
    );
    // Registration and a purchase are states TDLib reaches; from any flow.
    for flow in [Flow::Phone, Flow::Qr] {
        let mut login = Login::new(flow, rules(&[]));
        let outs = login.on_state(
            state(json!({"@type": "authorizationStateWaitRegistration", "terms_of_service": {"@type": "termsOfService"}})),
            0,
        );
        assert_eq!(only_step(&outs).step_id, step::REGISTRATION, "{flow:?}");
        let mut login = Login::new(flow, rules(&[]));
        let outs = login.on_state(
            state(
                json!({"@type": "authorizationStateWaitPremiumPurchase", "store_product_id": "x"}),
            ),
            0,
        );
        assert_eq!(only_step(&outs).step_id, step::PURCHASE, "{flow:?}");
    }
    // An update TDLib needs: the answer to the number, the code and a resend.
    let at_phone = || {
        let mut login = Login::new(Flow::Phone, rules(&[]));
        login.on_state(wait_phone(), 0);
        let txn = login.txn();
        let outs = login.on_input(txn, values(&[("phone", "+999068204719")]), 0);
        assert_eq!(requests(&outs)[0].0, ReqKind::SetPhone);
        login
    };
    let mut login = at_phone();
    let outs = login.on_answer(ReqKind::SetPhone, Err(auth_error(TdClass::UpdateNeeded)), 1);
    let failed = only_step(&outs);
    assert_eq!(
        (failed.step_id.as_str(), failed_kind(failed)),
        (step::UPDATE, ErrorKind::Unsupported)
    );
    assert!(login.is_ended());
    for (kind, typed) in [
        (ReqKind::CheckCode, ("code", CODE)),
        (ReqKind::Resend, ("resend", "1")),
    ] {
        let mut login = at_code(wait_code(
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            json!({"@type": "authenticationCodeTypeCall", "length": 5}),
            0,
        ));
        let txn = login.txn();
        let outs = login.on_input(txn, values(&[typed]), 2_000);
        assert_eq!(requests(&outs)[0].0, kind);
        let outs = login.on_answer(kind, Err(auth_error(TdClass::UpdateNeeded)), 2_000);
        let failed = only_step(&outs);
        assert_eq!(
            (failed.step_id.as_str(), failed_kind(failed)),
            (step::UPDATE, ErrorKind::Unsupported),
            "{kind:?}"
        );
    }
}

/// After Ready, `getMe`'s ids are checked again (taken by either id, a relink to another
/// account, the tail): a `Failed` step and `Refused`, never `Ready`.
#[test]
fn ready_checks_both_ids_after_me() {
    let cases: Vec<(Rules, Refusal)> = vec![
        (
            rules(&[("tg:100", false)]),
            Refusal::AlreadyAdded { logged_out: false },
        ),
        (
            rules(&[("pn:999068204719", true)]),
            Refusal::AlreadyAdded { logged_out: true },
        ),
        (
            relink_rules(&["tg:7", "pn:999075390162"]),
            Refusal::NotTheRelinkedAccount,
        ),
        (tail_rules("4718"), Refusal::NotTheExpectedNumber),
    ];
    for (rules, want) in cases {
        let mut login = at_linking(rules);
        let outs = login.on_answer(ReqKind::GetMe, Ok(me(100, OWN_DIGITS)), 20);
        assert!(
            !outs.iter().any(|o| matches!(o, Out::Ready(_))),
            "{want:?}: no Ready"
        );
        assert!(
            outs.iter()
                .any(|o| matches!(o, Out::Refused(r) if *r == want)),
            "{want:?}: refused"
        );
        assert_eq!(
            only_step(&outs).step_id,
            step::QR,
            "{want:?}: at the flow's step"
        );
        assert_eq!(failed_kind(only_step(&outs)), want.error().kind);
    }
    // A relink of the same account by its tg id passes; an answer that is no user is refused.
    let mut login = at_linking(relink_rules(&["tg:100"]));
    let outs = login.on_answer(ReqKind::GetMe, Ok(me(100, PEER_DIGITS)), 20);
    assert!(matches!(outs.as_slice(), [Out::Ready(_)]));
    let mut login = at_linking(rules(&[]));
    let outs = login.on_answer(ReqKind::GetMe, Ok(json!({"@type": "chat", "id": 100})), 20);
    assert!(
        outs.iter()
            .any(|o| matches!(o, Out::Refused(Refusal::Unreadable)))
    );
}

/// Own ids are `[tg:<id>, pn:<digits>]`, the number `+<digits>`, the name "Ada Test"; `done`
/// comes only after the core's ack; without a number only the `tg:` id.
#[test]
fn own_ids_are_tg_then_pn_and_done_waits_for_the_ack() {
    let mut login = at_linking(rules(&[]));
    let outs = login.on_answer(ReqKind::GetMe, Ok(me(100, OWN_DIGITS)), 20);
    let [Out::Ready(ready)] = outs.as_slice() else {
        panic!("one Ready");
    };
    assert_eq!(ready.own_ids, vec![uid("tg:100"), uid("pn:999068204719")]);
    assert!(ready.number.as_ref().map(|n| n.expose().as_str()) == Some("+999068204719"));
    assert!(ready.display_name.as_ref().map(|n| n.expose().as_str()) == Some("Ada Test"));
    match ready.event() {
        BackendEvent::AccountReady {
            own_ids, device, ..
        } => {
            assert_eq!((own_ids.len(), device), (2, None));
        }
        _ => panic!("AccountReady"),
    }
    assert!(!login.is_ended(), "not done before the ack");
    let outs = login.acked();
    assert_eq!(only_step(&outs).step_id, step::DONE);
    assert!(matches!(only_step(&outs).kind, LoginStepKind::Done));
    assert!(login.is_ended());
    assert!(login.acked().is_empty(), "done once");

    let mut login = at_linking(rules(&[]));
    let outs = login.on_answer(ReqKind::GetMe, Ok(me(100, "")), 20);
    let [Out::Ready(ready)] = outs.as_slice() else {
        panic!("one Ready");
    };
    assert_eq!(
        (ready.own_ids.clone(), ready.number.clone()),
        (vec![uid("tg:100")], None)
    );
}

// ---- errors, aborts, txn -------------------------------------------------------------------

/// Any 429 and PHONE_NUMBER_FLOOD end the login RateLimited, with the wait when TDLib named one.
#[test]
fn a_429_is_rate_limited_with_its_wait() {
    for (class, wait, want) in [
        (
            TdClass::TooManyRequests,
            Some(37),
            Some(Duration::from_secs(37)),
        ),
        (TdClass::TooManyRequests, None, None),
        (TdClass::PhoneNumberFlood, None, None),
    ] {
        let mut login = Login::new(Flow::Phone, rules(&[]));
        login.on_state(wait_phone(), 0);
        login.on_input(1, values(&[("phone", "+999068204719")]), 0);
        let err = LinkError::Auth {
            code: 429,
            class,
            retry_after_s: wait,
        };
        let outs = login.on_answer(ReqKind::SetPhone, Err(err), 0);
        let step = only_step(&outs);
        assert_eq!(step.step_id, step::PHONE);
        assert_eq!(
            failed_kind(step),
            ErrorKind::RateLimited { retry_after: want }
        );
    }
}

/// B1: a helper that died or did not answer is an Abort with no step; the state TDLib comes back
/// in shows its step again (txn + 1) without a request.
#[test]
fn a_helper_abort_during_a_login_shows_no_failed_step() {
    for err in [LinkError::Timeout, LinkError::Closed] {
        let mut login = at_code(wait_code_telegram());
        let txn = login.txn();
        login.on_input(txn, values(&[("code", CODE)]), 2_000);
        let outs = login.on_answer(ReqKind::CheckCode, Err(err), 2_000);
        assert!(matches!(outs.as_slice(), [Out::Abort]), "{err:?}");
        assert!(!login.is_ended());
        let outs = login.on_state(wait_code_telegram(), 9_000);
        assert!(requests(&outs).is_empty());
        assert_eq!(steps(&outs), vec![(step::CODE.to_owned(), txn + 1)]);
    }
}

/// `Another authorization query` and `Call to .. unexpected` re-read the state and show it.
#[test]
fn another_query_rereads_the_state() {
    for class in [TdClass::AnotherQuery, TdClass::Unexpected] {
        let mut login = at_code(wait_code_telegram());
        let txn = login.txn();
        login.on_input(txn, values(&[("code", CODE)]), 2_000);
        let outs = login.on_answer(ReqKind::CheckCode, Err(auth_error(class)), 2_000);
        let reqs = requests(&outs);
        assert_eq!(reqs.len(), 1, "{class:?}");
        assert_eq!(reqs[0].1, json!({"@type": "getAuthorizationState"}));
        let answer = json!({
            "@type": "authorizationStateWaitCode",
            "code_info": {"@type": "authenticationCodeInfo", "type": {"@type": "authenticationCodeTypeSms", "length": 5}},
        });
        let outs = login.on_answer(ReqKind::GetState, Ok(answer), 2_100);
        assert_eq!(
            steps(&outs),
            vec![(step::CODE.to_owned(), txn + 1)],
            "{class:?}"
        );
    }
}

/// TDLib's own LOGOUT during a login ends it; one Kourier asked for does not. A close without a
/// logout (Closing, Closed) is an Abort, not a failure (fix J5): the helper restarts and the
/// persisted step is shown again.
#[test]
fn tdlibs_own_logout_during_a_login_fails_it_but_a_close_restarts_and_an_asked_one_is_nothing() {
    let mut login = at_code(wait_code_telegram());
    let outs = login.on_state(
        state(json!({"@type": "authorizationStateLoggingOut"})),
        2_000,
    );
    let step = only_step(&outs);
    assert_eq!(step.step_id, step::CODE);
    assert!(matches!(failed_kind(step), ErrorKind::Transient { .. }));
    assert!(login.is_ended());

    for s in ["authorizationStateClosing", "authorizationStateClosed"] {
        let mut login = at_code(wait_code_telegram());
        let txn = login.txn();
        let outs = login.on_state(state(json!({"@type": s})), 2_000);
        assert!(matches!(outs.as_slice(), [Out::Abort]), "{s}: an Abort");
        assert!(!login.is_ended(), "{s}: the login goes on");
        login.on_restart();
        let outs = login.on_state(wait_code_telegram(), 9_000);
        assert!(requests(&outs).is_empty(), "{s}: no request");
        assert_eq!(steps(&outs), vec![(step::CODE.to_owned(), txn + 1)], "{s}");
    }

    for s in [
        "authorizationStateLoggingOut",
        "authorizationStateClosing",
        "authorizationStateClosed",
    ] {
        let mut login = at_code(wait_code_telegram());
        login.logout_asked();
        assert!(
            login.on_state(state(json!({"@type": s})), 2_000).is_empty(),
            "{s} asked"
        );
    }
}

/// `txn` rises by one with every step, whatever its kind.
#[test]
fn txn_rises_with_every_step() {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    let mut seen = Vec::new();
    seen.extend(steps(&login.on_state(wait_phone(), 0)));
    seen.extend(steps(&login.on_input(
        1,
        values(&[("phone", "+999068204719")]),
        0,
    )));
    login.on_answer(ReqKind::SetPhone, Ok(json!({"@type": "ok"})), 0);
    seen.extend(steps(&login.on_state(wait_code_telegram(), 0)));
    seen.extend(steps(&login.on_input(2, values(&[("code", CODE)]), 0)));
    seen.extend(steps(&login.on_answer(
        ReqKind::CheckCode,
        Err(auth_error(TdClass::PhoneCodeInvalid)),
        0,
    )));
    seen.extend(steps(&login.on_state(wait_password(""), 0)));
    seen.extend(steps(&login.on_state(ready(), 0)));
    let txns: Vec<u64> = seen.iter().map(|(_, t)| *t).collect();
    assert_eq!(txns, vec![1, 2, 3, 4, 5]);
    let ids: Vec<&str> = seen.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["phone", "code", "code-again", "password", "linking"]
    );
}

/// L7: an input for an older step is ignored, and one step's input is sent once.
#[test]
fn a_stale_txn_is_ignored_and_one_steps_input_is_sent_once() {
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    assert!(
        login
            .on_input(txn - 1, values(&[("code", CODE)]), 2_000)
            .is_empty(),
        "stale"
    );
    assert!(
        login
            .on_input(txn + 1, values(&[("code", CODE)]), 2_000)
            .is_empty(),
        "future"
    );
    assert_eq!(
        requests(&login.on_input(txn, values(&[("code", CODE)]), 2_000)).len(),
        1
    );
    login.on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 2_000);
    assert!(
        login
            .on_input(txn, values(&[("code", CODE)]), 2_000)
            .is_empty(),
        "sent once"
    );
}

/// H1: an input that comes while another request waits is held and sent when that answer
/// came (TDLib's WaitCode comes before the number's `ok`); a stale answer is ignored.
#[test]
fn an_input_while_a_request_waits_is_sent_after_its_answer() {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(wait_phone(), 0);
    login.on_input(1, values(&[("phone", "+999068204719")]), 0);
    assert_eq!(login.pending(), Some(ReqKind::SetPhone));
    login.on_state(wait_code_telegram(), 10);
    let outs = login.on_input(login.txn(), values(&[("code", CODE)]), 20);
    assert!(outs.is_empty(), "held while the number's answer waits");
    assert!(
        login
            .on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 25)
            .is_empty(),
        "stale"
    );
    let outs = login.on_answer(ReqKind::SetPhone, Ok(json!({"@type": "ok"})), 30);
    let reqs = requests(&outs);
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0].0, ReqKind::CheckCode);
    assert_eq!(login.pending(), Some(ReqKind::CheckCode));
}

/// No request the machine builds is one the helper forbids, and no forbidden name appears as a
/// string in the machine's source.
#[test]
fn no_request_the_machine_builds_is_forbidden() {
    for kind in ReqKind::ALL {
        assert!(
            !kourier_tdlib::serve::is_forbidden_request(kind.type_name()),
            "{kind:?} is forbidden"
        );
    }
    let source = include_str!("../login.rs");
    for name in kourier_tdlib::serve::FORBIDDEN_REQUESTS {
        let quoted = ["\"", name, "\""].concat();
        assert!(!source.contains(&quoted), "the machine names {name}");
    }
}

/// Nothing typed reaches a `Debug` of what the machine hands out (the session may log kinds).
#[test]
fn nothing_typed_reaches_a_debug_of_the_outs() {
    let mut all = Vec::new();
    let mut login = Login::new(Flow::Phone, rules(&[]));
    all.extend(login.on_state(wait_phone(), 0));
    all.extend(login.on_input(1, values(&[("phone", "+999068204719")]), 0));
    all.extend(login.on_answer(ReqKind::SetPhone, Ok(json!({"@type": "ok"})), 0));
    all.extend(login.on_state(wait_code_telegram(), 0));
    all.extend(login.on_input(login.txn(), values(&[("code", CODE)]), 0));
    all.extend(login.on_answer(ReqKind::CheckCode, Ok(json!({"@type": "ok"})), 0));
    all.extend(login.on_state(wait_password("synthetic hint"), 0));
    all.extend(login.on_input(login.txn(), values(&[("password", PASSWORD)]), 0));
    all.extend(login.on_answer(ReqKind::CheckPassword, Ok(json!({"@type": "ok"})), 0));
    all.extend(login.on_state(ready(), 0));
    all.extend(login.on_answer(ReqKind::GetMe, Ok(me(100, OWN_DIGITS)), 0));
    let text = format!("{all:?} {login:?}");
    for secret in ["068204719", CODE, PASSWORD, "synthetic hint", "Ada"] {
        assert!(!text.contains(secret), "a typed value reached Debug");
    }
}

// ---- fixes J1, J2, J4, J9, J13 (the TG-2 reviews) -----------------------------------------

fn qr_link(token: &str) -> AuthorizationState {
    state(json!({
        "@type": "authorizationStateWaitOtherDeviceConfirmation",
        "link": format!("tg://login?token={token}"),
    }))
}

fn wait_email_code() -> AuthorizationState {
    state(json!({
        "@type": "authorizationStateWaitEmailCode",
        "code_info": {"@type": "emailAddressAuthenticationCodeInfo", "email_address_pattern": "a*@example.invalid", "length": 6},
    }))
}

/// J1: the QR bound (M6) covers the WAIT FOR A SCAN only. Once TDLib leaves the QR state for a
/// password, an e-mail step or `Ready`, no deadline runs: a tick past the old bound ends nothing,
/// a restart does not bring the bound back, and the password still logs in.
#[test]
fn a_scanned_qr_login_is_no_longer_bound_by_the_wait_for_a_scan() {
    let t0 = 1_000_000;
    let after: Vec<(&str, AuthorizationState)> = vec![
        (step::PASSWORD, wait_password("synthetic hint")),
        (
            step::EMAIL,
            state(json!({"@type": "authorizationStateWaitEmailAddress"})),
        ),
        (step::EMAIL_CODE, wait_email_code()),
        (step::LINKING, ready()),
    ];
    for (id, next) in after {
        let mut login = Login::new(Flow::Qr, rules(&[]));
        login.on_state(wait_phone(), t0);
        login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), t0);
        login.on_state(qr_link("eA"), t0 + 1);
        assert_eq!(
            login.deadline_ms(),
            Some(t0 + QR_BOUND_MS),
            "{id}: a link waits for a scan"
        );
        let outs = login.on_state(next, t0 + QR_BOUND_MS - 10_000);
        assert_eq!(steps(&outs)[0].0, id, "{id}: the step after the scan");
        assert_eq!(login.deadline_ms(), None, "{id}: the bound is off");
        assert!(
            login.on_tick(t0 + QR_BOUND_MS + 60_000).is_empty(),
            "{id}: a tick past the bound ends nothing"
        );
        login.on_restart();
        assert_eq!(
            login.deadline_ms(),
            None,
            "{id}: a restart revives no bound"
        );
        assert!(login.on_tick(t0 + 2 * QR_BOUND_MS).is_empty(), "{id}");
        assert!(!login.is_ended(), "{id}");
    }

    // QR, scan, 2FA: the password typed after the bound logs in.
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), t0);
    login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), t0);
    login.on_state(qr_link("eA"), t0 + 1);
    login.on_state(wait_password("synthetic hint"), t0 + QR_BOUND_MS - 10_000);
    assert!(login.on_tick(t0 + QR_BOUND_MS).is_empty());
    let txn = login.txn();
    let outs = login.on_input(
        txn,
        values(&[("password", PASSWORD)]),
        t0 + QR_BOUND_MS + 5_000,
    );
    assert_eq!(requests(&outs)[0].0, ReqKind::CheckPassword);
    // A link after the scan does not start the bound again.
    login.on_answer(
        ReqKind::CheckPassword,
        Err(auth_error(TdClass::PasswordHashInvalid)),
        t0 + QR_BOUND_MS + 6_000,
    );
    assert_eq!(login.deadline_ms(), None);
}

/// J2: a resend Telegram refuses (406 SEND_CODE_UNAVAILABLE, TDLib's own 400 "can't be resent",
/// any answer of no terminal class) leaves the code already sent valid: the code step comes
/// again (txn + 1) with no request and no further resend offered, and the code logs in. A flood,
/// an expired code, a banned number, an application that must update, an e-mail address that is
/// not allowed (the next test), a request in the wrong state and the helper's end keep their own
/// readings.
#[test]
fn a_refused_resend_reshows_the_code_step_and_the_code_still_logs_in() {
    let sms_then_call = || {
        wait_code(
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            json!({"@type": "authenticationCodeTypeCall", "length": 5}),
            0,
        )
    };
    for err in [
        LinkError::Td { code: 406 },
        LinkError::Td { code: 400 },
        LinkError::Frame,
        auth_error(TdClass::EmailInvalid),
    ] {
        let mut login = at_code(sms_then_call());
        let txn = login.txn();
        let outs = login.on_input(txn, values(&[("resend", "1")]), 5_000);
        assert_eq!(requests(&outs).len(), 1, "{err:?}: the resend is built");
        let outs = login.on_answer(ReqKind::Resend, Err(err), 5_000);
        assert!(requests(&outs).is_empty(), "{err:?}: no request");
        assert_eq!(
            steps(&outs),
            vec![(step::CODE.to_owned(), txn + 1)],
            "{err:?}: the code step again"
        );
        assert!(!login.is_ended(), "{err:?}");
        let outs = login.on_input(txn + 1, values(&[("resend", "1")]), 99_000);
        assert!(requests(&outs).is_empty(), "{err:?}: no further resend");
        assert_eq!(
            steps(&outs),
            vec![(step::CODE.to_owned(), txn + 2)],
            "{err:?}"
        );
        let outs = login.on_input(txn + 2, values(&[("code", CODE)]), 99_000);
        assert_eq!(requests(&outs)[0].0, ReqKind::CheckCode, "{err:?}");
    }

    for (class, kind, at) in [
        (
            TdClass::TooManyRequests,
            ErrorKind::RateLimited { retry_after: None },
            step::CODE,
        ),
        (
            TdClass::PhoneNumberFlood,
            ErrorKind::RateLimited { retry_after: None },
            step::CODE,
        ),
        (TdClass::PhoneCodeExpired, ErrorKind::Expired, step::CODE),
    ] {
        let mut login = at_code(sms_then_call());
        let txn = login.txn();
        login.on_input(txn, values(&[("resend", "1")]), 5_000);
        let outs = login.on_answer(ReqKind::Resend, Err(auth_error(class)), 5_000);
        let step = only_step(&outs);
        assert_eq!(
            (step.step_id.as_str(), failed_kind(step)),
            (at, kind),
            "{class:?}"
        );
        assert!(login.is_ended(), "{class:?}");
    }

    let mut login = at_code(sms_then_call());
    let txn = login.txn();
    login.on_input(txn, values(&[("resend", "1")]), 5_000);
    let outs = login.on_answer(
        ReqKind::Resend,
        Err(auth_error(TdClass::AnotherQuery)),
        5_000,
    );
    assert_eq!(
        requests(&outs)[0].1,
        json!({"@type": "getAuthorizationState"})
    );
    let mut login = at_code(sms_then_call());
    let txn = login.txn();
    login.on_input(txn, values(&[("resend", "1")]), 5_000);
    let outs = login.on_answer(ReqKind::Resend, Err(LinkError::Closed), 5_000);
    assert!(matches!(outs.as_slice(), [Out::Abort]));
}

/// J2, the lead's refinement: a resend refused as a banned number, an application that must update
/// or an e-mail address that is not allowed is TERMINAL like a flood or an expired code. The login
/// ends `Failed` at the same step and with the same kind as the same answer to the code check
/// gives, and neither the code step comes again nor a request is built.
#[test]
fn a_resend_refused_for_a_banned_number_ends_the_login() {
    let sms_then_call = || {
        wait_code(
            json!({"@type": "authenticationCodeTypeSms", "length": 5}),
            json!({"@type": "authenticationCodeTypeCall", "length": 5}),
            0,
        )
    };
    for (class, at, kind) in [
        (
            TdClass::PhoneNumberBanned,
            step::PHONE,
            ErrorKind::Unauthorized,
        ),
        (TdClass::UpdateNeeded, step::UPDATE, ErrorKind::Unsupported),
        (
            TdClass::EmailNotAllowed,
            step::EMAIL,
            ErrorKind::Unsupported,
        ),
    ] {
        // The reading of the same answer to a request that is no resend.
        let mut other = at_code(sms_then_call());
        let txn = other.txn();
        let outs = other.on_input(txn, values(&[("code", CODE)]), 5_000);
        assert_eq!(requests(&outs)[0].0, ReqKind::CheckCode, "{class:?}");
        let outs = other.on_answer(ReqKind::CheckCode, Err(auth_error(class)), 5_000);
        let step = only_step(&outs);
        assert_eq!(
            (step.step_id.as_str(), failed_kind(step)),
            (at, kind.clone()),
            "{class:?}: the reading of any other request"
        );

        let mut login = at_code(sms_then_call());
        let txn = login.txn();
        let outs = login.on_input(txn, values(&[("resend", "1")]), 5_000);
        assert_eq!(requests(&outs).len(), 1, "{class:?}: the resend is built");
        let outs = login.on_answer(ReqKind::Resend, Err(auth_error(class)), 5_000);
        assert!(requests(&outs).is_empty(), "{class:?}: no request");
        let step = only_step(&outs);
        assert_eq!(
            (step.step_id.as_str(), failed_kind(step)),
            (at, kind),
            "{class:?}: the same step and kind as for any other request"
        );
        assert!(login.is_ended(), "{class:?}");
    }
}

/// J4: a helper that dies before the FIRST QR link comes back in `WaitPhoneNumber` (TDLib saved
/// no QR state yet): ONE new request, no step, and the bound of the first request still runs. A
/// second death before a link, a death after a link and a scanned login ask nothing more.
#[test]
fn a_helper_death_before_the_first_qr_link_allows_one_new_request() {
    let t0 = 1_000_000;
    let mut login = Login::new(Flow::Qr, rules(&[]));
    let outs = login.on_state(wait_phone(), t0);
    assert_eq!(requests(&outs).len(), 1);
    let outs = login.on_answer(ReqKind::RequestQr, Err(LinkError::Closed), t0 + 10);
    assert!(matches!(outs.as_slice(), [Out::Abort]));
    login.on_restart();
    let outs = login.on_state(wait_phone(), t0 + 3_000);
    let reqs = requests(&outs);
    assert_eq!(reqs.len(), 1, "one new request");
    assert_eq!(reqs[0].0, ReqKind::RequestQr);
    assert!(steps(&outs).is_empty(), "no step");
    assert!(!login.is_ended());
    assert_eq!(
        login.deadline_ms(),
        Some(t0 + QR_BOUND_MS),
        "the bound of the first request runs on"
    );
    // The link comes: a qr step, as ever.
    login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), t0 + 3_100);
    let outs = login.on_state(qr_link("eA"), t0 + 3_200);
    assert_eq!(steps(&outs), vec![(step::QR.to_owned(), 1)]);

    // A second death before any link: no third request.
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), t0);
    login.on_restart();
    login.on_state(wait_phone(), t0 + 1_000);
    login.on_restart();
    let outs = login.on_state(wait_phone(), t0 + 2_000);
    assert!(requests(&outs).is_empty(), "no third request");
    assert_eq!(failed_kind(only_step(&outs)), ErrorKind::Expired);

    // A death AFTER a link: TDLib persisted the QR state, a WaitPhoneNumber is the end.
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), t0);
    login.on_answer(ReqKind::RequestQr, Ok(json!({"@type": "ok"})), t0);
    login.on_state(qr_link("eA"), t0 + 1);
    login.on_restart();
    let outs = login.on_state(wait_phone(), t0 + 2_000);
    assert!(requests(&outs).is_empty(), "no request after a link");
    assert_eq!(failed_kind(only_step(&outs)), ErrorKind::Expired);
}

/// J9 (MU2): an input held behind the number's answer belongs to the step it was typed at; a
/// newer step (a resend's new `WaitCode` before the number's `ok`) drops it, so the `ok` hands
/// out no `checkAuthenticationCode` for a code Telegram no longer waits for.
#[test]
fn a_held_input_of_an_older_step_is_dropped_when_a_newer_step_comes() {
    let mut login = Login::new(Flow::Phone, rules(&[]));
    login.on_state(wait_phone(), 0);
    login.on_input(1, values(&[("phone", "+999068204719")]), 0);
    assert_eq!(login.pending(), Some(ReqKind::SetPhone));
    login.on_state(wait_code_telegram(), 10);
    let outs = login.on_input(login.txn(), values(&[("code", CODE)]), 20);
    assert!(outs.is_empty(), "held while the number's answer waits");
    let outs = login.on_state(wait_code_telegram(), 30);
    assert_eq!(steps(&outs), vec![(step::CODE.to_owned(), 3)]);
    let outs = login.on_answer(ReqKind::SetPhone, Ok(json!({"@type": "ok"})), 40);
    assert!(
        requests(&outs).is_empty(),
        "the number's ok sent the code of the older step"
    );
    assert_eq!(login.pending(), None);
    // The newer step's own input goes out.
    let outs = login.on_input(3, values(&[("code", CODE)]), 50);
    assert_eq!(requests(&outs)[0].0, ReqKind::CheckCode);
}

/// J13: Telegram's own text (the 2FA hint, the name) loses the bidirectional controls that could
/// reorder what follows and is cut at 400 characters, like WhatsApp's explanation.
#[test]
fn the_hint_and_the_name_lose_bidi_controls_and_are_cut() {
    let bidi = [
        '\u{061C}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}',
        '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}',
    ];
    let mut hint = String::from("pet");
    hint.extend(bidi);
    hint.push_str("name ");
    hint.push_str(&"x".repeat(500));
    let note = note_of(&hint).and_then(|n| n.explanation).expect("a note");
    assert!(!note.chars().any(is_bidi_control), "a bidi control stayed");
    assert_eq!(note.chars().count(), 400, "not cut at 400 characters");
    assert!(note.starts_with("petname x"));
    assert!(
        note_of("\u{202E}\u{2066}").is_none(),
        "only controls: no note"
    );

    let mut first = String::from("Ad");
    first.extend(bidi);
    first.push('a');
    let user = User {
        id: 100,
        first_name: first,
        last_name: "T".repeat(500),
        phone_number: String::new(),
    };
    let ready = rules(&[]).decide(&user).expect("accepted");
    let name = ready.display_name.expect("a name");
    let name = name.expose();
    assert!(!name.chars().any(is_bidi_control), "a bidi control stayed");
    assert_eq!(name.chars().count(), 400, "not cut at 400 characters");
    assert!(name.starts_with("Ada TT"));
}

/// J14: a typed value the login does not use goes through [`wipe`] (zeroed on drop): a stale
/// `txn`, a step whose input was already sent, an ended login, the fields left after the one
/// taken, and an input with nothing on screen to answer.
#[test]
fn every_typed_value_the_login_drops_is_wiped() {
    // A leftover: `resend` taken, the `code` beside it is left.
    let mut login = at_code(wait_code(
        json!({"@type": "authenticationCodeTypeSms", "length": 5}),
        json!({"@type": "authenticationCodeTypeCall", "length": 5}),
        0,
    ));
    let txn = login.txn();
    let before = wiped();
    login.on_input(txn, values(&[("resend", "1"), ("code", CODE)]), 5_000);
    assert!(wiped() > before, "the field left after the taken one");

    // Sent once: the second input of the same step.
    let mut login = at_code(wait_code_telegram());
    let txn = login.txn();
    login.on_input(txn, values(&[("code", CODE)]), 2_000);
    let before = wiped();
    login.on_input(txn, values(&[("code", CODE)]), 2_100);
    assert!(wiped() > before, "the second input of one step");

    // Stale and future txn.
    let before = wiped();
    login.on_input(txn - 1, values(&[("code", CODE)]), 2_200);
    login.on_input(txn + 7, values(&[("code", CODE)]), 2_300);
    assert!(wiped() >= before + 2, "a stale or future txn");

    // Nothing on screen to answer (the QR step).
    let mut login = Login::new(Flow::Qr, rules(&[]));
    login.on_state(wait_phone(), 0);
    login.on_state(qr_link("eA"), 1);
    let before = wiped();
    login.on_input(login.txn(), values(&[("code", CODE)]), 2);
    assert!(wiped() > before, "an input at a QR step");

    // An ended login.
    let mut login = at_code(wait_code_telegram());
    let _ = login.fail(BackendError::internal("synthetic"));
    let before = wiped();
    login.on_input(login.txn(), values(&[("code", CODE)]), 3_000);
    assert!(wiped() > before, "an input after the end");
}
