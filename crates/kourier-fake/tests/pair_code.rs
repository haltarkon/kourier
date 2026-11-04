//! The fake plays WhatsApp (K-A): its descriptor is what the UI needs, and the `pair-code`
//! login flow (WhatsApp's "link with phone number") runs on the account clock: a phone number
//! is asked for, the network may refuse it, the code is shown, the phone enters it and the
//! account links. A flow the script does not play is refused. The fake plays Threema too (TH4
//! KH1c): its descriptor, and the emoji step that follows the scan of its QR code.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendEvent, Command, ContextParams, DbKey, Envelope, FakeClock, Network,
    StartMode,
};
use kourier_fake::{EMOJI_FOR, FakeNetwork, Flavor, LoginScript, PairCodeScript, Script};
use kourier_model::{
    AccountId, AccountUuid, BackendError, ErrorKind, HistorySource, InputKind, LoginStepKind,
    NetworkKind, Redacted, UserId,
};
use tokio_util::sync::CancellationToken;

fn me() -> UserId {
    "aci:00000000-0000-4000-8000-0000000000aa"
        .parse()
        .expect("id")
}

fn whatsapp(script: Script) -> FakeNetwork {
    FakeNetwork::playing(NetworkKind::WhatsApp, script)
}

#[test]
fn the_fake_playing_whatsapp_says_what_the_ui_needs_and_the_plain_fake_does_not() {
    let net = whatsapp(Script::new("wa", vec![me()]));
    assert_eq!(net.kind(), NetworkKind::WhatsApp);
    let d = net.descriptor();
    assert_eq!(d.display_name, "WhatsApp");
    let flows: Vec<&str> = d.login_flows.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(flows, ["qr", "pair-code"]);
    assert_eq!(d.history, HistorySource::PushedOnLink);
    assert!(!d.history_transfer, "WhatsApp pushes batches, no archive");
    assert!(d.experimental && d.risk_notice.is_some());
    assert!(d.start_chat.is_empty(), "Kourier never starts a chat there");
    // The ids stay the Signal-like ones: the same script gives the same chats.
    assert_eq!(d.msg_id_scope, Flavor::SignalLike.descriptor().msg_id_scope);

    let plain = FakeNetwork::new(Flavor::SignalLike, Script::new("p", vec![me()]));
    assert_eq!(plain.kind(), NetworkKind::Fake);
    let p = plain.descriptor();
    assert!(!p.experimental && !p.history_transfer && p.risk_notice.is_none());
    assert_eq!(p.login_flows.len(), 1);
    assert!(
        plain
            .with_history_transfer(true)
            .descriptor()
            .history_transfer
    );
}

#[test]
#[should_panic(expected = "the fake plays WhatsApp, Threema and Telegram only, not Signal")]
fn the_fake_plays_no_other_network() {
    let _ = FakeNetwork::playing(NetworkKind::Signal, Script::new("s", vec![me()]));
}

fn threema(script: Script) -> FakeNetwork {
    FakeNetwork::playing(NetworkKind::Threema, script)
}

/// The QR login of the Threema tests: no device name, two codes of 20 s, the phone scans the FIRST
/// one at once (nothing waits on the clock before the emoji).
fn scanned_at_once(name: &str) -> Script {
    let mut script = Script::new(name, vec![me()]);
    script.login = LoginScript::Qr {
        ask_device_name: false,
        codes: 2,
        valid_for: Duration::from_secs(20),
        scanned_at: Some(0),
        linked_as: None,
    };
    script
}

/// Waits until the actor sleeps on the account clock (the emoji wait is armed).
async fn until_sleeping(clock: &FakeClock) {
    while clock.sleepers() == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

/// What the actor says within a moment of real time, if anything (the account clock does not move
/// in that moment: a step that waits on it cannot come).
async fn heard(run: &mut Running) -> Option<Vec<BackendEvent>> {
    tokio::time::timeout(Duration::from_millis(150), next(run))
        .await
        .ok()
        .map(|env| env.events)
}

/// The login steps up to the `done` step, and whether `AccountReady` came with them: the durable
/// event travels through the forwarder, so it may come on either side of the step.
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

/// The three glyphs of the fake's emoji step, written out as code points here and not taken from
/// the crate's constant: the black cat is the list's sequence with a zero-width joiner.
fn the_three_emoji() -> Vec<String> {
    ["\u{1F435}", "\u{1F408}\u{200D}\u{2B1B}", "\u{1F33B}"]
        .map(str::to_owned)
        .to_vec()
}

#[test]
fn playing_threema() {
    let net = threema(Script::new("th", vec![me()]));
    assert_eq!(net.kind(), NetworkKind::Threema);
    let d = net.descriptor();
    assert_eq!(d.display_name, "Threema");
    assert_eq!(d.icon_name, "threema");
    let flows: Vec<(&str, &str)> = d
        .login_flows
        .iter()
        .map(|f| (f.id.as_str(), f.label.as_str()))
        .collect();
    assert_eq!(
        flows,
        [("qr", "Link with a QR code")],
        "one flow: the emoji are a step of it"
    );
    assert!(d.start_chat.is_empty(), "a chat from an ID comes later");
    assert_eq!(d.history, HistorySource::None);
    assert!(!d.history_transfer);
    assert!(d.experimental);
    let notice = d.risk_notice.as_ref().expect("the warning asks first");
    assert_eq!(notice.title, "Before You Turn On Threema");
    assert_eq!(
        notice.body,
        "Kourier is not an official Threema app. It links as another device of your Threema account, and Threema may end that link."
    );
    assert_eq!(notice.url, None);
    assert!(!d.safety_numbers && !d.contact_cards && d.cleaned_pictures.is_none());
    // The ids stay the Signal-like ones: the same script gives the same chats.
    assert_eq!(d.msg_id_scope, Flavor::SignalLike.descriptor().msg_id_scope);
    assert_eq!(d.edit_target, Flavor::SignalLike.descriptor().edit_target);

    // The link: the code, the phone scans it, THEN the emoji, then the account links.
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = threema(scanned_at_once("th-link"));
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt);
        start(&run, "qr").await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("qr", 1));
        assert!(matches!(kind, LoginStepKind::ShowQr { .. }));
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("emoji", 2), "after the scan");
        match kind {
            LoginStepKind::ConfirmEmoji { emoji } => assert_eq!(emoji, the_three_emoji()),
            other => panic!("{other:?}"),
        }
        // The phone confirms 2 s later on the account clock: not a moment before.
        until_sleeping(&clock).await;
        clock.advance(EMOJI_FOR - Duration::from_secs(1));
        let early = heard(&mut run).await;
        assert!(
            early.is_none(),
            "the emoji stand for two seconds: {early:?}"
        );
        clock.advance(Duration::from_secs(1));
        let (steps, ready) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"]);
        assert!(ready, "the account linked");
    });
}

#[test]
fn only_the_fake_playing_threema_shows_the_emoji_step() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let others = [
        whatsapp(scanned_at_once("wa")),
        FakeNetwork::new(Flavor::SignalLike, scanned_at_once("plain")),
        FakeNetwork::new(Flavor::ThreemaLike, scanned_at_once("threema-like")),
    ];
    rt.block_on(async {
        for fake in &others {
            let mut run = spawn(fake, &clock, &rt);
            start(&run, "qr").await;
            let (id, _, _) = step(&mut run).await;
            assert_eq!(id, "qr");
            let (steps, ready) = until_linked(&mut run).await;
            assert_eq!(steps, ["linking", "done"], "{:?}", fake.kind());
            assert!(ready);
        }
    });
}

#[test]
fn the_emoji_step_stands_as_long_as_the_script_says_and_a_cancel_ends_it() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let held = Duration::from_secs(600);
    let fake = threema(scanned_at_once("th-held").with_emoji_for(held));
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt);
        start(&run, "qr").await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "qr");
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "emoji");
        until_sleeping(&clock).await;
        // Past the default two seconds and a long way on: the page stays where it is.
        clock.advance(EMOJI_FOR * 10);
        let more = heard(&mut run).await;
        assert!(more.is_none(), "held at the emoji: {more:?}");
        // A command that is no cancel does not end the wait either.
        run.ends
            .commands
            .send(Command::SubmitLoginInput {
                txn: 2,
                values: Vec::new(),
            })
            .await
            .expect("send");
        let more = heard(&mut run).await;
        assert!(more.is_none(), "an input is not the phone's word: {more:?}");
        run.ends
            .commands
            .send(Command::CancelLogin)
            .await
            .expect("send");
        // The cancel ended the link: the time that is left brings nothing. (The cancel reaches the
        // login through the actor's command loop: let it arrive before the clock moves, or the
        // wait may end on the clock first.)
        let more = heard(&mut run).await;
        assert!(more.is_none(), "a cancel says nothing: {more:?}");
        clock.advance(held);
        let more = heard(&mut run).await;
        assert!(more.is_none(), "a cancelled link links nothing: {more:?}");
    });
}

struct Running {
    ends: kourier_backend_api::CoreEnds,
    _handle: kourier_backend_api::AccountHandle,
    uuid: AccountUuid,
    _dir: tempfile::TempDir,
}

fn spawn(fake: &FakeNetwork, clock: &FakeClock, rt: &tokio::runtime::Runtime) -> Running {
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
        config: Default::default(),
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

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
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

async fn type_number(run: &Running, txn: u64) {
    run.ends
        .commands
        .send(Command::SubmitLoginInput {
            txn,
            values: vec![("phone".into(), Redacted::new("+380671234567".to_owned()))],
        })
        .await
        .expect("send");
}

#[test]
fn the_pair_code_flow_asks_for_the_number_shows_the_code_and_links() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake =
        whatsapp(Script::new("pair", vec![me()]).with_pair_code(PairCodeScript::new("ABCD-1234")));
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt);
        start(&run, "pair-code").await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("phone", 1));
        match kind {
            LoginStepKind::Input { fields } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(
                    (fields[0].id.as_str(), fields[0].kind),
                    ("phone", InputKind::Phone)
                );
            }
            other => panic!("{other:?}"),
        }
        type_number(&run, txn).await;
        let (id, _, kind) = step(&mut run).await;
        assert_eq!(id, "code");
        assert!(matches!(&kind, LoginStepKind::ShowCode { code } if code.expose() == "ABCD-1234"));
        // The phone types the code ten seconds later on the account clock.
        while clock.sleepers() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        clock.advance(Duration::from_secs(10));
        let mut ready = false;
        for _ in 0..10 {
            let env = next(&mut run).await;
            ready |= env
                .events
                .iter()
                .any(|e| matches!(e, BackendEvent::AccountReady { .. }));
            if ready {
                break;
            }
        }
        assert!(ready, "the account linked");
        assert_eq!(fake.control().login_flows(run.uuid), ["pair-code"]);
        assert_eq!(
            fake.control().login_inputs(run.uuid),
            [vec![("phone".to_owned(), "+380671234567".to_owned())]]
        );
    });
}

#[test]
fn a_refused_number_fails_that_attempt_and_the_next_one_gets_a_code() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let throttled = BackendError::new(
        ErrorKind::RateLimited {
            retry_after: Some(Duration::from_secs(60)),
        },
        "too many codes",
    );
    let fake = whatsapp(
        Script::new("refused", vec![me()])
            .with_pair_code(PairCodeScript::new("WXYZ-5678").refusing(vec![throttled.clone()])),
    );
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt);
        start(&run, "pair-code").await;
        let (_, txn, _) = step(&mut run).await;
        type_number(&run, txn).await;
        let (id, _, kind) = step(&mut run).await;
        assert_eq!(id, "phone", "a failed step carries the step it failed at");
        assert!(matches!(&kind, LoginStepKind::Failed { error } if *error == throttled));
        // Try again (the core starts a new actor for the new attempt): accepted this time.
        let mut again = spawn(&fake, &clock, &rt);
        start(&again, "pair-code").await;
        let (_, txn, _) = step(&mut again).await;
        type_number(&again, txn).await;
        let (id, _, kind) = step(&mut again).await;
        assert_eq!(id, "code");
        assert!(matches!(&kind, LoginStepKind::ShowCode { .. }));
    });
}

#[test]
fn a_flow_the_script_does_not_play_is_refused_as_unsupported() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    // No pair-code script: that flow is not there. And "nonsense" never is.
    let fake = whatsapp(Script::new("none", vec![me()]));
    rt.block_on(async {
        for flow in ["pair-code", "nonsense"] {
            let mut run = spawn(&fake, &clock, &rt);
            start(&run, flow).await;
            let (id, _, kind) = step(&mut run).await;
            assert_eq!(id, "failed", "{flow}");
            assert!(
                matches!(&kind, LoginStepKind::Failed { error } if error.kind == ErrorKind::Unsupported),
                "{flow}: {kind:?}"
            );
        }
    });
}

#[test]
fn a_code_the_phone_never_enters_expires() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = whatsapp(
        Script::new("expires", vec![me()])
            .with_pair_code(PairCodeScript::new("QQQQ-0000").never_entered()),
    );
    rt.block_on(async {
        let mut run = spawn(&fake, &clock, &rt);
        start(&run, "pair-code").await;
        let (_, txn, _) = step(&mut run).await;
        type_number(&run, txn).await;
        let (id, _, _) = step(&mut run).await;
        assert_eq!(id, "code");
        while clock.sleepers() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Not yet: the code is valid for three minutes.
        clock.advance(Duration::from_secs(179));
        tokio::time::sleep(Duration::from_millis(50)).await;
        clock.advance(Duration::from_secs(1));
        let (id, _, kind) = step(&mut run).await;
        assert_eq!(id, "code", "a failed step carries the step it failed at");
        assert!(
            matches!(&kind, LoginStepKind::Failed { error } if error.kind == ErrorKind::Expired)
        );
    });
}
