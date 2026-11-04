//! The fake plays Telegram (KT-1b): its descriptor is what the UI needs, the `phone` login flow
//! walks the number, the code (its delivery, its wait, a new code), the two-step password and the
//! login e-mail in TDLib's order and keyed on the TYPED digits, a QR scan is followed by the password
//! and only then by the check of the number, and the login-start line has the words the real
//! backend prints. Nothing of Telegram is contacted and `kourier-fake` holds no Telegram code: every
//! number, code and password here is made up.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEvent, Command, ContextParams, DbKey, Envelope,
    FakeClock, Network, StartMode,
};
use kourier_fake::{FakeNetwork, Flavor, LoginScript, PhoneAccount, PhoneLoginScript, Script};
use kourier_model::{
    AccountId, AccountUuid, BackendError, CodeDelivery, CodeResend, CodeVia, ErrorKind,
    HistorySource, InputField, InputKind, LoginStepKind, NetworkKind, RESEND_FIELD, Redacted,
    UserId,
};
use tokio_util::sync::CancellationToken;

/// The account's number (the one the phone scans in), its expected tail, a plain account without a
/// password, a number in no row and the tail of none of them. No value is a substring of another.
const OWN: &str = "999000518462";
const OWN_TAIL: &str = "00518462";
const PLAIN: &str = "999000611233";
const FLOODED: &str = "999000264075";
const GHOST: &str = "999000377108";
const CODE: &str = "24680";
const PASSWORD: &str = "synthetic password";
const HINT: &str = "synthetic hint";
const EMAIL_CODE: &str = "13579";
const ADDRESS: &str = "someone@example.invalid";

fn me() -> UserId {
    "tg:100200300".parse().expect("id")
}

fn telegram(script: Script) -> FakeNetwork {
    FakeNetwork::playing(NetworkKind::Telegram, script)
}

/// The code arrived in the app; a new one may be asked for after `wait`, by SMS.
fn delivery(wait: Duration) -> CodeDelivery {
    CodeDelivery {
        via: CodeVia::App,
        resend: Some(CodeResend {
            after: wait,
            via: CodeVia::Sms,
        }),
    }
}

fn flood() -> BackendError {
    BackendError::new(
        ErrorKind::RateLimited {
            retry_after: Some(Duration::from_secs(3600)),
        },
        "too many attempts",
    )
}

/// The table: OWN has everything (password with a hint, then a login e-mail), PLAIN only a code,
/// FLOODED is refused at `phone`. `GHOST` is in no row.
fn phone_script(name: &str) -> Script {
    Script::new(name, vec![me()])
        .with_number(OWN)
        .with_phone_login(PhoneLoginScript::new(vec![
            PhoneAccount::new(OWN, CODE, delivery(Duration::from_secs(60)))
                .with_password(PASSWORD, Some(HINT))
                .with_email(EMAIL_CODE),
            PhoneAccount::new(
                PLAIN,
                CODE,
                CodeDelivery {
                    via: CodeVia::Sms,
                    resend: None,
                },
            ),
            PhoneAccount::new(FLOODED, CODE, delivery(Duration::ZERO)).refusing(flood()),
        ]))
}

/// The QR login of the Telegram tests: no device name, two codes of 20 s, the phone scans the first
/// at once. The account is OWN (which has a password).
fn qr_script(name: &str, honour_tail: bool) -> Script {
    let mut script = phone_script(name);
    script.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 2,
        valid_for: Duration::from_secs(20),
        scanned_at: Some(0),
        linked_as: None,
    };
    script.honour_number_tail = honour_tail;
    script
}

struct Running {
    ends: kourier_backend_api::CoreEnds,
    _handle: kourier_backend_api::AccountHandle,
    uuid: AccountUuid,
    _dir: tempfile::TempDir,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

/// An account of `fake` on `clock`, which expects a number ending in `tail` (the run tool's).
fn spawn(
    fake: &FakeNetwork,
    clock: &FakeClock,
    rt: &tokio::runtime::Runtime,
    tail: Option<&str>,
) -> Running {
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start: StartMode::Fresh,
        backend_db: dir.path().join("backend.db"),
        db_key: DbKey::new([3; 32]),
        blob_dir: dir.path().join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock: Arc::new(clock.clone()),
        config: BackendConfig {
            expect_number_suffix: tail.map(|t| Redacted::new(t.to_owned())),
            ..BackendConfig::default()
        },
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = fake.spawn(ctx).expect("spawn");
    Running {
        ends,
        _handle: handle,
        uuid,
        _dir: dir,
    }
}

async fn next(run: &mut Running) -> Envelope {
    let env = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
        .await
        .expect("timely")
        .expect("open");
    if let Some(a) = env.ack {
        a.complete(Ok(()));
    }
    Envelope {
        events: env.events,
        ack: None,
    }
}

/// The next login step the actor shows (ephemeral events of other kinds are skipped).
async fn step(run: &mut Running) -> (String, u64, LoginStepKind) {
    loop {
        let env = next(run).await;
        for e in env.events {
            if let BackendEvent::Login(s) = e {
                return (s.step_id, s.txn, s.kind);
            }
        }
    }
}

/// What the actor says within a moment of real time, if anything.
async fn heard(run: &mut Running) -> Option<Vec<BackendEvent>> {
    tokio::time::timeout(Duration::from_millis(150), next(run))
        .await
        .ok()
        .map(|env| env.events)
}

/// The login steps up to the `done` step, and whether `AccountReady` came with them.
async fn until_linked(run: &mut Running) -> (Vec<String>, bool) {
    let (mut steps, mut done, mut ready) = (Vec::new(), false, false);
    while !(done && ready) {
        for e in next(run).await.events {
            match e {
                BackendEvent::AccountReady { .. } => ready = true,
                BackendEvent::Login(s) => {
                    done |= matches!(s.kind, LoginStepKind::Done);
                    steps.push(s.step_id);
                }
                _ => {}
            }
        }
    }
    (steps, ready)
}

async fn start(run: &Running, flow: &str) {
    run.ends
        .commands
        .send(Command::StartLogin {
            flow: flow.into(),
            relink: None,
            taken: Vec::new(),
        })
        .await
        .expect("send");
}

async fn submit(run: &Running, txn: u64, values: &[(&str, &str)]) {
    run.ends
        .commands
        .send(Command::SubmitLoginInput {
            txn,
            values: values
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Redacted::new((*v).to_owned())))
                .collect(),
        })
        .await
        .expect("send");
}

async fn cancel(run: &Running) {
    run.ends
        .commands
        .send(Command::CancelLogin)
        .await
        .expect("send");
}

/// The one field of an input step.
fn only_field(kind: LoginStepKind) -> InputField {
    match kind {
        LoginStepKind::Input { mut fields } => {
            assert_eq!(fields.len(), 1, "one field per step");
            fields.remove(0)
        }
        other => panic!("not an input step: {other:?}"),
    }
}

/// The failure of a step: where it failed and with which kind.
fn failure(step: (String, u64, LoginStepKind)) -> (String, ErrorKind) {
    match step.2 {
        LoginStepKind::Failed { error } => (step.0, error.kind),
        other => panic!("not a failure: {other:?}"),
    }
}

/// Types a number at the `phone` step of a fresh account and returns what comes next.
async fn type_number(
    fake: &FakeNetwork,
    clock: &FakeClock,
    rt: &tokio::runtime::Runtime,
    tail: Option<&str>,
    typed: &str,
) -> (Running, (String, u64, LoginStepKind)) {
    let mut run = spawn(fake, clock, rt, tail);
    start(&run, "phone").await;
    let (id, txn, _) = step(&mut run).await;
    assert_eq!((id.as_str(), txn), ("phone", 1));
    submit(&run, 1, &[("phone", typed)]).await;
    let first = step(&mut run).await;
    (run, first)
}

#[test]
fn the_fake_playing_telegram_says_what_the_ui_needs() {
    let net = telegram(Script::new("tg", vec![me()]));
    assert_eq!(net.kind(), NetworkKind::Telegram);
    let d = net.descriptor();
    assert_eq!(d.display_name, "Telegram");
    assert_eq!(d.icon_name, "telegram");
    let flows: Vec<(&str, &str)> = d
        .login_flows
        .iter()
        .map(|f| (f.id.as_str(), f.label.as_str()))
        .collect();
    assert_eq!(
        flows,
        [("qr", "Scan a QR Code"), ("phone", "Phone Number")],
        "the QR code first"
    );
    assert!(d.start_chat.is_empty(), "Kourier starts no chat there");
    assert_eq!(d.history, HistorySource::OnDemand);
    assert!(!d.history_transfer);
    assert!(d.experimental);
    let notice = d.risk_notice.as_ref().expect("the warning asks first");
    assert_eq!(notice.title, "Before You Add Telegram");
    assert!(
        notice.body.starts_with("Kourier is not made by Telegram."),
        "{}",
        notice.body
    );
    assert_eq!(notice.body.matches("\n\n").count(), 2, "three paragraphs");
    assert_eq!(notice.url, None);
    assert!(
        d.receipts_always_sent,
        "Telegram's rules: receipts and typing are always sent"
    );
    assert!(!d.safety_numbers && !d.contact_cards && d.cleaned_pictures.is_none());
    assert_eq!(
        d.shutdown_grace, None,
        "the fake stops at once: it states no grace, whatever the real backend states"
    );
    // The ids stay the Signal-like ones: the same script gives the same chats.
    assert_eq!(d.msg_id_scope, Flavor::SignalLike.descriptor().msg_id_scope);
    assert_eq!(d.edit_target, Flavor::SignalLike.descriptor().edit_target);
    assert_eq!(d.outbox, Flavor::SignalLike.descriptor().outbox);

    // No other fake says it: the flag is Telegram's.
    let others = [
        FakeNetwork::new(Flavor::SignalLike, Script::new("p", vec![me()])),
        FakeNetwork::playing(NetworkKind::WhatsApp, Script::new("w", vec![me()])),
        FakeNetwork::playing(NetworkKind::Threema, Script::new("t", vec![me()])),
    ];
    for other in &others {
        assert!(
            !other.descriptor().receipts_always_sent,
            "{:?}",
            other.kind()
        );
    }
}

#[test]
fn the_phone_flow_walks_number_code_password_email_and_links() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = telegram(phone_script("walk"));
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt, None);
        start(&run, "phone").await;
        let mut seen: Vec<(String, u64)> = Vec::new();

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        let field = only_field(kind);
        assert_eq!((field.id.as_str(), field.kind), ("phone", InputKind::Phone));
        // The number as a person types it: the separators do not matter.
        submit(&run, txn, &[("phone", "+999 000-518.462")]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        let field = only_field(kind);
        assert_eq!((field.id.as_str(), field.kind), ("code", InputKind::Text));
        assert_eq!(
            field.code,
            Some(delivery(Duration::from_secs(60))),
            "the code step states how it came and the wait for a new one"
        );
        submit(&run, txn, &[("code", "00000")]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        let field = only_field(kind);
        assert_eq!(field.id, "code", "the same field again");
        assert!(field.code.is_some(), "the retry states the delivery too");
        submit(&run, txn, &[("code", CODE)]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        let field = only_field(kind);
        assert_eq!(
            (field.id.as_str(), field.kind),
            ("password", InputKind::Secret)
        );
        let hint = field.note.as_ref().and_then(|n| n.explanation.as_deref());
        assert_eq!(hint, Some(HINT), "the owner's hint is the field's note");
        assert_eq!(field.code, None, "only the code field states a delivery");
        submit(&run, txn, &[("password", "wrong")]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        let field = only_field(kind);
        assert_eq!(field.id, "password");
        assert_eq!(
            field.note.and_then(|n| n.explanation).as_deref(),
            Some(HINT),
            "the retry shows the hint again"
        );
        submit(&run, txn, &[("password", PASSWORD)]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        assert_eq!(only_field(kind).id, "email");
        submit(&run, txn, &[("email", "nobody")]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        assert_eq!(only_field(kind).id, "email", "the same field again");
        submit(&run, txn, &[("email", ADDRESS)]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        let field = only_field(kind);
        assert_eq!(field.id, "code", "the e-mail's code is a `code` field");
        assert_eq!(field.code, None, "an e-mail code states no delivery");
        submit(&run, txn, &[("code", "00000")]).await;

        let (id, txn, kind) = step(&mut run).await;
        seen.push((id, txn));
        assert_eq!(only_field(kind).id, "code");
        submit(&run, txn, &[("code", EMAIL_CODE)]).await;

        let (steps, ready) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"]);
        assert!(ready, "one AccountReady");

        let ids: Vec<&str> = seen.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "phone",
                "code",
                "code-again",
                "password",
                "password-again",
                "email",
                "email-again",
                "email-code",
                "email-code-again"
            ]
        );
        let txns: Vec<u64> = seen.iter().map(|(_, t)| *t).collect();
        assert_eq!(txns, (1..=9).collect::<Vec<u64>>(), "txn 1..n with no gap");
        // The typed values reached the fake's record, in order (the number as typed).
        let inputs = fake.control().login_inputs(run.uuid);
        assert_eq!(
            inputs[0],
            [("phone".to_owned(), "+999 000-518.462".to_owned())]
        );
        assert_eq!(inputs.len(), 9);
    });
}

#[test]
fn an_account_without_a_password_goes_from_the_code_to_the_link() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = telegram(phone_script("plain"));
    rt.block_on(async {
        let (mut run, (id, txn, kind)) = type_number(&fake, &clock, &rt, None, PLAIN).await;
        assert_eq!(id, "code");
        let field = only_field(kind);
        assert_eq!(
            field.code,
            Some(CodeDelivery {
                via: CodeVia::Sms,
                resend: None
            }),
            "this account's network offers no next way"
        );
        submit(&run, txn, &[("code", CODE)]).await;
        let (steps, ready) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"], "no password, no e-mail");
        assert!(ready);
    });
}

#[test]
fn a_number_without_the_expected_tail_is_refused_before_the_code() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let mut script = phone_script("tail");
    script.honour_number_tail = true;
    let fake = telegram(script);
    rt.block_on(async {
        // PLAIN is a number of the table that does not end in the expected tail.
        let (mut run, first) = type_number(&fake, &clock, &rt, Some(OWN_TAIL), PLAIN).await;
        assert_eq!(
            failure(first),
            ("phone".to_owned(), ErrorKind::NotTheExpectedAccount),
            "the refusal comes at `phone`, where the code would have been asked for"
        );
        let more = heard(&mut run).await;
        assert!(more.is_none(), "no code step, no account: {more:?}");

        // The right number passes the check and gets its code.
        let (_run, (id, _, _)) = type_number(&fake, &clock, &rt, Some(OWN_TAIL), OWN).await;
        assert_eq!(id, "code");

        // A script that does not honour the tail ignores it, as the other fakes do.
        let lax = telegram(phone_script("lax"));
        let (_run, (id, _, _)) = type_number(&lax, &clock, &rt, Some(OWN_TAIL), PLAIN).await;
        assert_eq!(id, "code");
    });
}

#[test]
fn the_outcome_is_the_numbers_not_the_attempts() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = telegram(phone_script("rows"));
    rt.block_on(async {
        // A number in no row: Telegram has no account for it. The step is `registration`.
        let (mut run, first) = type_number(&fake, &clock, &rt, None, GHOST).await;
        assert_eq!(
            failure(first),
            ("registration".to_owned(), ErrorKind::Unsupported)
        );
        assert!(heard(&mut run).await.is_none(), "the login ended there");

        // A row with a refusal fails at `phone` with that row's error ...
        let (_run, first) = type_number(&fake, &clock, &rt, None, FLOODED).await;
        let (id, error) = match first {
            (id, _, LoginStepKind::Failed { error }) => (id, error),
            other => panic!("{other:?}"),
        };
        assert_eq!(id, "phone");
        assert_eq!(error, flood());

        // ... a number that is right in the next attempt gets its code ...
        let (_run, (id, _, _)) = type_number(&fake, &clock, &rt, None, OWN).await;
        assert_eq!(id, "code");

        // ... and the refused one is refused again: the attempt number decides nothing.
        let (_run, first) = type_number(&fake, &clock, &rt, None, FLOODED).await;
        assert_eq!(failure(first).0, "phone");

        // Text that is no phone number fails as such, at `phone`.
        for typed in ["not a number", "999x000", "+"] {
            let (_run, first) = type_number(&fake, &clock, &rt, None, typed).await;
            assert_eq!(
                failure(first),
                ("phone".to_owned(), ErrorKind::InvalidInput),
                "{typed:?}"
            );
        }
    });
}

#[test]
fn a_new_code_comes_only_after_its_wait_and_each_step_states_what_is_left() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = telegram(phone_script("resend"));
    rt.block_on(async {
        let (mut run, (id, txn, kind)) = type_number(&fake, &clock, &rt, None, OWN).await;
        assert_eq!((id.as_str(), txn), ("code", 2));
        assert_eq!(
            only_field(kind).code,
            Some(delivery(Duration::from_secs(60)))
        );

        // 20 s later the person asks: not yet. The same code step comes again, the same way, and
        // says what is LEFT of the wait (40 s), not the whole of it.
        clock.advance(Duration::from_secs(20));
        submit(&run, 2, &[(RESEND_FIELD, "1")]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("code", 3));
        assert_eq!(
            only_field(kind).code,
            Some(delivery(Duration::from_secs(40))),
            "the same delivery, 40 s left"
        );

        // A wrong code meanwhile shows the retry with the wait that is left.
        clock.advance(Duration::from_secs(15));
        submit(&run, 3, &[("code", "00000")]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("code-again", 4));
        assert_eq!(
            only_field(kind).code,
            Some(delivery(Duration::from_secs(25)))
        );

        // At 60 s from the first showing the wait is over: the new code comes, by SMS, and it is
        // the last (no further resend).
        clock.advance(Duration::from_secs(25));
        submit(&run, 4, &[(RESEND_FIELD, "1")]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("code", 5));
        assert_eq!(
            only_field(kind).code,
            Some(CodeDelivery {
                via: CodeVia::Sms,
                resend: None
            })
        );

        // Asking again changes nothing; a wrong code now says the retry with the same way.
        submit(&run, 5, &[(RESEND_FIELD, "1")]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("code", 6));
        assert_eq!(only_field(kind).code.expect("a delivery").resend, None);
        submit(&run, 6, &[("code", "00000")]).await;
        let (id, _, kind) = step(&mut run).await;
        assert_eq!(id, "code-again");
        assert_eq!(only_field(kind).code.expect("a delivery").via, CodeVia::Sms);
    });
}

#[test]
fn a_zero_wait_is_stated_at_once_and_a_new_code_may_be_asked_for_at_once() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let mut script = phone_script("zero");
    script.phone_login = Some(PhoneLoginScript::new(vec![PhoneAccount::new(
        OWN,
        CODE,
        delivery(Duration::ZERO),
    )]));
    let fake = telegram(script);
    rt.block_on(async {
        let (mut run, (_, txn, kind)) = type_number(&fake, &clock, &rt, None, OWN).await;
        assert_eq!(
            only_field(kind)
                .code
                .and_then(|c| c.resend)
                .map(|r| r.after),
            Some(Duration::ZERO),
            "the page shows the button at once"
        );
        submit(&run, txn, &[(RESEND_FIELD, "1")]).await;
        let (_, _, kind) = step(&mut run).await;
        let delivery = only_field(kind).code.expect("a delivery");
        assert_eq!((delivery.via, delivery.resend), (CodeVia::Sms, None));
    });
}

#[test]
fn the_qr_flow_asks_the_password_after_the_scan_and_checks_the_number_after_it() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    rt.block_on(async {
        // The account the phone scans in has a password: it is asked right after the scan, a wrong
        // one is asked again, and only then the number is checked: the wrong phone is refused AT
        // `qr`, after the password (TDLib's order: the check runs once the login is Ready).
        let wrong = telegram(qr_script("qr-wrong", true));
        let mut run = spawn(&wrong, &clock, &rt, Some("00264075"));
        start(&run, "qr").await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("qr", 1));
        assert!(matches!(kind, LoginStepKind::ShowQr { .. }));
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("password", 2), "right after the scan");
        let field = only_field(kind);
        assert_eq!(field.kind, InputKind::Secret);
        assert_eq!(
            field.note.and_then(|n| n.explanation).as_deref(),
            Some(HINT)
        );
        submit(&run, txn, &[("password", "wrong")]).await;
        let (id, txn, _) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("password-again", 3));
        submit(&run, txn, &[("password", PASSWORD)]).await;
        let refused = step(&mut run).await;
        assert_eq!(
            (refused.0.as_str(), refused.1),
            ("qr", 4),
            "the refusal is the step after the password"
        );
        assert_eq!(failure(refused).1, ErrorKind::NotTheExpectedAccount);
        let more = heard(&mut run).await;
        assert!(more.is_none(), "nothing is linked: {more:?}");

        // The right phone links after the password.
        let right = telegram(qr_script("qr-right", true));
        let mut run = spawn(&right, &clock, &rt, Some(OWN_TAIL));
        start(&run, "qr").await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "qr");
        let (id, txn, _) = step(&mut run).await;
        assert_eq!(id, "password");
        submit(&run, txn, &[("password", PASSWORD)]).await;
        let (steps, ready) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"]);
        assert!(ready);

        // An account without a password goes straight from the scan to the link.
        let mut script = qr_script("qr-plain", false);
        script.number = Some(PLAIN.to_owned());
        let plain = telegram(script);
        let mut run = spawn(&plain, &clock, &rt, None);
        start(&run, "qr").await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "qr");
        let (steps, ready) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"], "no password step");
        assert!(ready);
    });
}

#[test]
fn a_cancel_ends_the_phone_login_at_every_wait() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = telegram(phone_script("cancel"));
    rt.block_on(async {
        // At the number ...
        let mut run = spawn(&fake, &clock, &rt, None);
        start(&run, "phone").await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "phone");
        cancel(&run).await;
        assert!(heard(&mut run).await.is_none(), "a cancel says nothing");

        // ... at the code ...
        let (mut run, (id, _, _)) = type_number(&fake, &clock, &rt, None, OWN).await;
        assert_eq!(id, "code");
        cancel(&run).await;
        assert!(heard(&mut run).await.is_none(), "a cancel says nothing");
        clock.advance(Duration::from_secs(600));
        assert!(
            heard(&mut run).await.is_none(),
            "time brings nothing either"
        );

        // ... and at the password.
        let (mut run, (_, txn, _)) = type_number(&fake, &clock, &rt, None, OWN).await;
        submit(&run, txn, &[("code", CODE)]).await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "password");
        cancel(&run).await;
        assert!(heard(&mut run).await.is_none(), "a cancel says nothing");
    });
}

#[test]
fn an_input_for_another_step_is_ignored() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = telegram(phone_script("stale"));
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt, None);
        start(&run, "phone").await;
        let (id, txn, _) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("phone", 1));
        // An answer to a step that is not on screen moves nothing (the real machine's rule, L7).
        submit(&run, 7, &[("phone", OWN)]).await;
        assert!(heard(&mut run).await.is_none(), "a stale answer");
        submit(&run, 1, &[("phone", OWN)]).await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "code");
    });
}

#[test]
fn a_phone_login_the_script_does_not_play_is_refused_as_unsupported() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    // No phone script: that flow is not there.
    let fake = telegram(Script::new("none", vec![me()]));
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt, None);
        start(&run, "phone").await;
        let (id, _, kind) = step(&mut run).await;
        assert_eq!(id, "failed");
        assert!(
            matches!(&kind, LoginStepKind::Failed { error } if error.kind == ErrorKind::Unsupported),
            "{kind:?}"
        );
    });
}

/// The words of the login-start line are pinned by the source (the fake crate has no log capture;
/// the real backend's test reads the line from its own log): the fake playing Telegram prints
/// `a telegram login starts` with `expected_number_check`, every other fake the line WhatsApp's run
/// tool waits for. `kourier-telegram` prints the very same words (KT-1b C4).
#[test]
fn the_login_start_line_of_the_fake_playing_telegram_has_the_pinned_words() {
    let flat = |src: &str| src.split_whitespace().collect::<Vec<_>>().join(" ");
    let phone = flat(include_str!("../src/actor/phone.rs"));
    assert!(
        phone.contains(
            r#"kourier_model::NetworkKind::Telegram => { tracing::info!(expected_number_check = check, "a telegram login starts"); }"#
        ),
        "the Telegram arm of the login-start line changed"
    );
    assert!(
        phone.contains(
            r#"_ => tracing::info!(expected_number_check = check, "a whatsapp login starts"),"#
        ),
        "the line WhatsApp's run tool waits for changed"
    );
    assert!(
        phone.contains(r#"if tail_set { "on" } else { "off" }"#),
        "the words of the check are `on` and `off`"
    );
    assert!(
        !phone.contains("flow ="),
        "the line has no `flow` field (the real backend's has none)"
    );
    // The line is printed at the start of every login, by the actor.
    let actor = flat(include_str!("../src/actor.rs"));
    assert!(
        actor.contains(
            "self.log_login_start(); self.control.login(self.uuid, flow.clone(), taken);"
        ),
        "the actor no longer prints the line at `StartLogin`"
    );
}
