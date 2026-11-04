//! The rehearsal network of Telegram (KT-1b, D-2 F5): the fake that plays Telegram for the run
//! tool's rehearsal shows a first QR code for fifteen seconds that nobody scans, links by the
//! second one with the account's password asked after the scan and the number checked after the
//! password, walks the `phone` flow of its own number with the public constants, serves a direct
//! chat and a basic group with one history batch each while it says `Syncing`, goes online only
//! after the acks, replays nothing when it resumes, and puts an arrival on cue with Telegram's id
//! shape. Nothing of Telegram is contacted and `kourier-fake` holds no Telegram code: every value
//! here is made up.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEvent, Clock, Command, ContextParams, DbKey, Envelope,
    FakeClock, Network, StartMode, SystemClock,
};
use kourier_fake::telegram::{self, Timings};
use kourier_fake::{Arrival, FakeNetwork, Flavor};
use kourier_model::{
    AccountId, AccountUuid, CodeDelivery, CodeResend, CodeVia, ConnState, ConvKey, ErrorKind,
    InputField, InputKind, LoginStepKind, MemberRole, NetworkKind, Patch, Redacted, UserId,
};
use tokio_util::sync::CancellationToken;

/// Fast waits for the tests that run on the system clock.
const FAST: Timings = Timings {
    first_code: Duration::from_millis(25),
    code: Duration::from_millis(25),
    resend: Duration::from_millis(25),
};

fn fast() -> FakeNetwork {
    FakeNetwork::rehearsal_telegram_with(FAST)
}

fn peer() -> UserId {
    UserId::from_parts("tg", telegram::PEER_TG_ID).expect("id")
}

fn own() -> Vec<UserId> {
    vec![
        UserId::from_parts("tg", telegram::OWN_TG_ID).expect("id"),
        UserId::from_parts("pn", telegram::OWN_NUMBER).expect("id"),
    ]
}

fn group() -> ConvKey {
    ConvKey::Group(format!("tg:{}", telegram::GROUP_CHAT_ID).into())
}

struct Running {
    ends: kourier_backend_api::CoreEnds,
    handle: kourier_backend_api::AccountHandle,
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
}

fn spawn(
    fake: &FakeNetwork,
    clock: Arc<dyn Clock>,
    dir: &std::path::Path,
    uuid: AccountUuid,
    start: StartMode,
    tail: Option<&str>,
    rt: &tokio::runtime::Runtime,
) -> Running {
    let (ctx, ends) = AccountContext::new(ContextParams {
        account: AccountId(1),
        uuid,
        start,
        backend_db: dir.join("backend.db"),
        db_key: DbKey::new([3; 32]),
        blob_dir: dir.join("blobs"),
        net_rt: rt.handle().clone(),
        cancel: CancellationToken::new(),
        clock,
        config: BackendConfig {
            expect_number_suffix: tail.map(|t| Redacted::new(t.to_owned())),
            ..BackendConfig::default()
        },
        ui: Default::default(),
        local_prefs: Default::default(),
    });
    let handle = fake.spawn(ctx).expect("spawn");
    Running { ends, handle }
}

async fn stop(mut run: Running) {
    let _ = run.ends.commands.send(Command::Shutdown).await;
    let _ = run.handle.exited.await;
    if let Some(t) = run.handle.thread.take() {
        t.join().expect("join");
    }
}

async fn send(run: &Running, command: Command) {
    run.ends.commands.send(command).await.expect("send");
}

async fn start(run: &Running, flow: &str) {
    send(
        run,
        Command::StartLogin {
            flow: flow.into(),
            relink: None,
            taken: Vec::new(),
        },
    )
    .await;
}

async fn submit(run: &Running, txn: u64, values: &[(&str, &str)]) {
    send(
        run,
        Command::SubmitLoginInput {
            txn,
            values: values
                .iter()
                .map(|(k, v)| ((*k).to_owned(), Redacted::new((*v).to_owned())))
                .collect(),
        },
    )
    .await;
}

/// The next envelope, acked.
async fn next(run: &mut Running) -> Vec<BackendEvent> {
    let env = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
        .await
        .expect("timely")
        .expect("open");
    if let Some(a) = env.ack {
        a.complete(Ok(()));
    }
    env.events
}

/// The next login step the actor shows (events of other kinds are skipped).
async fn step(run: &mut Running) -> (String, u64, LoginStepKind) {
    loop {
        for e in next(run).await {
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
}

/// Waits until the actor sleeps on the account clock (a wait is armed).
async fn until_sleeping(clock: &FakeClock) {
    while clock.sleepers() == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn only_field(kind: LoginStepKind) -> InputField {
    match kind {
        LoginStepKind::Input { mut fields } => {
            assert_eq!(fields.len(), 1, "one field per step");
            fields.remove(0)
        }
        other => panic!("not an input step: {other:?}"),
    }
}

fn failure(step: (String, u64, LoginStepKind)) -> (String, ErrorKind) {
    match step.2 {
        LoginStepKind::Failed { error } => (step.0, error.kind),
        other => panic!("not a failure: {other:?}"),
    }
}

/// The login steps up to `done`, and the ids `AccountReady` carried.
async fn until_linked(run: &mut Running) -> (Vec<String>, Vec<UserId>) {
    let (mut steps, mut done, mut ids) = (Vec::new(), false, None);
    while !(done && ids.is_some()) {
        for e in next(run).await {
            match e {
                BackendEvent::AccountReady { own_ids, .. } => ids = Some(own_ids),
                BackendEvent::Login(s) => {
                    done |= matches!(s.kind, LoginStepKind::Done);
                    steps.push(s.step_id);
                }
                _ => {}
            }
        }
    }
    (steps, ids.expect("ready"))
}

/// Links by the QR flow on the system clock (the password typed at its step) and reads every
/// event until the script is over.
async fn link_and_finish(
    fake: &FakeNetwork,
    run: &mut Running,
    uuid: AccountUuid,
    password: Option<&str>,
) -> Vec<BackendEvent> {
    let control = fake.control();
    start(run, "qr").await;
    let mut all = Vec::new();
    loop {
        let env = tokio::time::timeout(Duration::from_secs(20), async {
            tokio::select! {
                () = control.wait_for(|c| c.is_finished(uuid)) => None,
                env = run.ends.events.recv() => Some(env.expect("open")),
            }
        })
        .await
        .expect("the script finished in time");
        let Some(env) = env else { break };
        if let Some(a) = env.ack {
            a.complete(Ok(()));
        }
        for e in &env.events {
            if let (BackendEvent::Login(s), Some(password)) = (e, password)
                && s.step_id == "password"
            {
                submit(run, s.txn, &[("password", password)]).await;
            }
        }
        all.extend(env.events);
    }
    // What was queued behind the last acknowledgement.
    while let Ok(Envelope { events, ack }) = run.ends.events.try_recv() {
        if let Some(a) = ack {
            a.complete(Ok(()));
        }
        all.extend(events);
    }
    all
}

fn delivery(wait: Duration) -> CodeDelivery {
    CodeDelivery {
        via: CodeVia::App,
        resend: Some(CodeResend {
            after: wait,
            via: CodeVia::Sms,
        }),
    }
}

#[test]
fn the_rehearsal_is_telegram_without_the_warning_and_states_no_roaming_setting() {
    let net = FakeNetwork::rehearsal_telegram();
    assert_eq!(net.kind(), NetworkKind::Telegram);
    let mut want = Flavor::SignalLike.playing_descriptor(NetworkKind::Telegram);
    assert!(want.risk_notice.is_some(), "the real thing asks first");
    want.risk_notice = None;
    assert_eq!(
        *net.descriptor(),
        want,
        "Telegram's descriptor but for the warning page (the UI's own, tested there)"
    );
    assert!(net.descriptor().receipts_always_sent);
    assert_eq!(net.descriptor().shutdown_grace, None);
    // The warning page is not there to tick, and the plain playing fake still has it.
    let playing = FakeNetwork::playing(NetworkKind::Telegram, telegram::script(FAST).expect("s"));
    assert!(playing.descriptor().risk_notice.is_some());
    assert_eq!(
        FakeNetwork::rehearsal_telegram_with(FAST).kind(),
        NetworkKind::Telegram
    );
}

#[test]
fn the_first_code_is_never_scanned_within_fifteen_seconds_and_the_second_links_with_the_password() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let started = clock.now_ms();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::rehearsal_telegram();
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            Arc::new(clock.clone()),
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(telegram::RIGHT_TAIL),
            &rt,
        );
        start(&run, "qr").await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("qr", 1));
        match kind {
            LoginStepKind::ShowQr { valid_for, .. } => {
                assert_eq!(valid_for, Some(Duration::from_secs(15)), "the first code");
            }
            other => panic!("{other:?}"),
        }
        // The account clock holds the first code for 15 s; a moment short of it nothing is said.
        until_sleeping(&clock).await;
        assert_eq!(clock.deadlines(), [started + 15_000]);
        clock.advance(Duration::from_millis(14_999));
        let early = heard(&mut run).await;
        assert!(early.is_none(), "nobody scans the first code: {early:?}");

        // Then the second code, of three seconds; the phone scans it at once: the account's
        // password comes right after, with its hint.
        clock.advance(Duration::from_millis(1));
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("qr", 2));
        match kind {
            LoginStepKind::ShowQr { valid_for, .. } => {
                assert_eq!(valid_for, Some(Duration::from_secs(3)), "the second code");
            }
            other => panic!("{other:?}"),
        }
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("password", 3), "right after the scan");
        let field = only_field(kind);
        assert_eq!(field.kind, InputKind::Secret);
        assert_eq!(
            field.note.and_then(|n| n.explanation).as_deref(),
            Some(telegram::PASSWORD_HINT)
        );
        submit(&run, txn, &[("password", "wrong")]).await;
        let (id, txn, _) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("password-again", 4));
        submit(&run, txn, &[("password", telegram::PASSWORD)]).await;
        let (steps, ids) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"]);
        assert_eq!(ids, own(), "the user id first, the number after it");
        assert!(
            fake.control().has_device(uuid),
            "the account is on the server"
        );
        stop(run).await;
    });
}

#[test]
fn a_wrong_phone_is_refused_at_the_qr_step_after_the_password() {
    let rt = runtime();
    let fake = fast();
    rt.block_on(async {
        for tail in [telegram::WRONG_TAIL, "000000000000000"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let uuid = AccountUuid::new_random();
            let mut run = spawn(
                &fake,
                Arc::new(SystemClock),
                dir.path(),
                uuid,
                StartMode::Fresh,
                Some(tail),
                &rt,
            );
            start(&run, "qr").await;
            assert_eq!(step(&mut run).await.0, "qr");
            assert_eq!(step(&mut run).await.0, "qr");
            let (id, txn, _) = step(&mut run).await;
            assert_eq!(id, "password", "the password comes before the check");
            submit(&run, txn, &[("password", telegram::PASSWORD)]).await;
            let refused = step(&mut run).await;
            assert_eq!(refused.0, "qr", "the refusal is at the QR step");
            assert_eq!(failure(refused).1, ErrorKind::NotTheExpectedAccount);
            let more = heard(&mut run).await;
            assert!(more.is_none(), "no account came of it: {more:?}");
            assert!(!fake.control().has_device(uuid), "the tail {tail}");
            stop(run).await;
        }
    });
}

#[test]
fn the_phone_flow_of_the_own_number_walks_every_step_with_the_public_constants() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::rehearsal_telegram();
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            Arc::new(clock.clone()),
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            Some(telegram::RIGHT_TAIL),
            &rt,
        );
        start(&run, "phone").await;
        let (id, txn, _) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("phone", 1));
        // The whole number with a plus, as a person types it.
        submit(
            &run,
            txn,
            &[("phone", &format!("+{}", telegram::OWN_NUMBER))],
        )
        .await;

        let (id, txn, kind) = step(&mut run).await;
        assert_eq!(id, "code");
        let field = only_field(kind);
        assert_eq!(
            field.code,
            Some(delivery(Timings::REAL.resend)),
            "through the app, a new one by SMS after the whole wait"
        );
        // An early ask: the same code step, the wait that is left.
        clock.advance(Duration::from_secs(10));
        submit(&run, txn, &[(kourier_model::RESEND_FIELD, "1")]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!(id, "code");
        assert_eq!(
            only_field(kind).code,
            Some(delivery(Duration::from_secs(20))),
            "20 s are left of the 30"
        );
        // After the wait: a new code, by SMS, and no further one.
        clock.advance(Duration::from_secs(20));
        submit(&run, txn, &[(kourier_model::RESEND_FIELD, "1")]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!(id, "code");
        assert_eq!(
            only_field(kind).code,
            Some(CodeDelivery {
                via: CodeVia::Sms,
                resend: None
            })
        );
        submit(&run, txn, &[("code", telegram::CODE)]).await;

        let (id, txn, kind) = step(&mut run).await;
        assert_eq!(id, "password");
        assert_eq!(
            only_field(kind).note.and_then(|n| n.explanation).as_deref(),
            Some(telegram::PASSWORD_HINT)
        );
        submit(&run, txn, &[("password", telegram::PASSWORD)]).await;

        let (id, txn, _) = step(&mut run).await;
        assert_eq!(id, "email");
        submit(&run, txn, &[("email", telegram::EMAIL_ADDRESS)]).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!(id, "email-code");
        assert_eq!(
            only_field(kind).code,
            None,
            "an e-mail code has no delivery"
        );
        submit(&run, txn, &[("code", telegram::EMAIL_CODE)]).await;

        let (steps, ids) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"]);
        assert_eq!(ids, own());
        stop(run).await;
    });
}

#[test]
fn the_phone_flow_refuses_the_wrong_tail_before_the_code_and_a_stranger_at_registration() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let fake = FakeNetwork::rehearsal_telegram();
    rt.block_on(async {
        // The own number, but the run tool expects another tail: refused at `phone`, no code.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut run = spawn(
            &fake,
            Arc::new(clock.clone()),
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            Some(telegram::WRONG_TAIL),
            &rt,
        );
        start(&run, "phone").await;
        let (_, txn, _) = step(&mut run).await;
        submit(&run, txn, &[("phone", telegram::OWN_NUMBER)]).await;
        let refused = step(&mut run).await;
        assert_eq!(refused.0, "phone");
        assert_eq!(failure(refused).1, ErrorKind::NotTheExpectedAccount);
        stop(run).await;

        // The other person's number, with a tail it ends in: Telegram has no account for it here.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut run = spawn(
            &fake,
            Arc::new(clock.clone()),
            dir.path(),
            AccountUuid::new_random(),
            StartMode::Fresh,
            Some(&telegram::PEER_NUMBER[4..]),
            &rt,
        );
        start(&run, "phone").await;
        let (_, txn, _) = step(&mut run).await;
        submit(&run, txn, &[("phone", telegram::PEER_NUMBER)]).await;
        let refused = step(&mut run).await;
        assert_eq!(refused.0, "registration");
        assert_eq!(failure(refused).1, ErrorKind::Unsupported);
        stop(run).await;
    });
}

/// The chats come while the account says `Syncing`: the direct chat with its author and ONE batch,
/// the group with its members and ONE batch, in the order the real start sync commits them; the
/// account goes `Online` only after the core acked them. Mutants: no `Syncing` step; no
/// `CatchUpDone`; a second batch for a chat.
#[test]
fn the_chats_arrive_with_one_batch_each_while_syncing_and_the_account_goes_online_after_the_acks() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = fast();
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(telegram::RIGHT_TAIL),
            &rt,
        );
        start(&run, "qr").await;
        // Read and ack everything until the group's batch; hold THAT envelope's ack.
        let mut all = Vec::new();
        let held = loop {
            let env = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
                .await
                .expect("timely")
                .expect("open");
            for e in &env.events {
                if let BackendEvent::Login(s) = e
                    && s.step_id == "password"
                {
                    submit(&run, s.txn, &[("password", telegram::PASSWORD)]).await;
                }
            }
            let last = env
                .events
                .iter()
                .any(|e| matches!(e, BackendEvent::HistoryBatch { conv, .. } if *conv == group()));
            all.extend(env.events);
            if last {
                break env.ack;
            }
            if let Some(a) = env.ack {
                a.complete(Ok(()));
            }
        };
        // The core has not acked the chats: the account is still syncing.
        let state = run.ends.conn.borrow().clone();
        assert!(
            matches!(state, ConnState::Syncing { .. }),
            "syncing until the chats are acked, not {state:?}"
        );
        if let Some(a) = held {
            a.complete(Ok(()));
        }
        tokio::time::timeout(
            Duration::from_secs(10),
            run.ends
                .conn
                .wait_for(|c| matches!(c, ConnState::Online { .. })),
        )
        .await
        .expect("timely")
        .expect("online after the acks");

        let batches: Vec<_> = all
            .iter()
            .filter_map(|e| match e {
                BackendEvent::HistoryBatch {
                    conv,
                    messages,
                    has_more,
                    progress,
                } => Some((conv.clone(), messages.clone(), *has_more, *progress)),
                _ => None,
            })
            .collect();
        assert_eq!(batches.len(), 2, "ONE batch per chat: {batches:?}");
        let direct = ConvKey::Direct(peer());
        assert_eq!(batches[0].0, direct, "the direct chat first");
        assert_eq!(batches[1].0, group());
        for (_, _, has_more, progress) in &batches {
            assert!(!has_more && progress.is_none(), "the TG-4 shape");
        }

        // The direct chat: three messages of the other person, the oldest read, two unread; the
        // group: two, both unread. Ids are Telegram's (a server id shifted by 20 bits), text is
        // the stems, never notified.
        let shape = |conv: &ConvKey, texts: &str, read: &[bool]| {
            let (_, messages, _, _) = batches.iter().find(|b| b.0 == *conv).expect("batch");
            assert_eq!(messages.len(), read.len());
            let mut last = 0_i64;
            for (i, m) in messages.iter().enumerate() {
                let id: i64 = m.local.parse().expect("decimal");
                assert_eq!(id % (1 << 20), 0, "a server id shifted by 20 bits: {id}");
                assert!(id > last, "ids rise");
                last = id;
                assert!(
                    id < telegram::arrival_id(1).parse::<i64>().expect("decimal"),
                    "an arrival is newer than any history"
                );
                assert_eq!(m.read, read[i], "{texts} {i}");
                assert!(m.silent, "history is never news");
                match &m.origin {
                    kourier_model::Origin::Remote { author, .. } => assert_eq!(*author, peer()),
                    other => panic!("{other:?}"),
                }
                assert_eq!(
                    format!("{:?}", m.body),
                    format!("{:?}", kourier_model::Body::text(format!("{texts} {i}")))
                );
            }
        };
        shape(&direct, telegram::DIRECT_WORDS, &[true, false, false]);
        shape(&group(), telegram::GROUP_WORDS, &[false, false]);

        // The chats and the author before their batches, with what Telegram states of a chat.
        let position = |f: &dyn Fn(&BackendEvent) -> bool| all.iter().position(f).expect("event");
        let chat = |key: ConvKey| {
            position(&move |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == key))
        };
        let batch = |key: ConvKey| {
            position(&move |e| matches!(e, BackendEvent::HistoryBatch { conv, .. } if *conv == key))
        };
        let contact =
            position(&|e| matches!(e, BackendEvent::ContactUpsert(p) if p.user == peer()));
        assert!(contact < chat(direct.clone()));
        assert!(chat(direct.clone()) < batch(direct.clone()));
        assert!(batch(direct.clone()) < chat(group()));
        assert!(chat(group()) < batch(group()));
        for event in &all {
            let BackendEvent::ConversationUpsert(patch) = event else {
                continue;
            };
            let is_group = patch.key == group();
            assert_eq!(
                patch.features,
                Some(telegram::telegram_features(is_group)),
                "a chat states what the Telegram backend states"
            );
            if is_group {
                assert_eq!(patch.title, Patch::Set(telegram::GROUP_NAME.to_owned()));
                let members = patch.members.as_ref().expect("members");
                let roles: Vec<(UserId, MemberRole)> = members
                    .upsert
                    .iter()
                    .map(|m| (m.user.clone(), m.role))
                    .collect();
                assert_eq!(
                    roles,
                    [
                        (peer(), MemberRole::Admin),
                        (own()[0].clone(), MemberRole::Member)
                    ]
                );
            }
        }
        // The other person: a name and the number they show.
        let named = all
            .iter()
            .find_map(|e| match e {
                BackendEvent::ContactUpsert(p) if p.user == peer() => Some(p.clone()),
                _ => None,
            })
            .expect("contact");
        assert_eq!(
            named.profile_name,
            Patch::Set(telegram::PEER_NAME.to_owned())
        );
        assert_eq!(named.number_shown, Some(true));
        let numbers: Vec<String> = named
            .identifiers
            .expect("identifiers")
            .into_iter()
            .map(|i| i.value)
            .collect();
        assert_eq!(numbers, [format!("+{}", telegram::PEER_NUMBER)]);
        // The fake states no setting the network cannot write yet.
        assert!(
            !all.iter()
                .any(|e| matches!(e, BackendEvent::AccountCaps(_))),
            "no roaming statement"
        );
        stop(run).await;
    });
}

#[test]
fn a_resumed_telegram_rehearsal_goes_online_and_replays_nothing() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(telegram::RIGHT_TAIL),
            &rt,
        );
        let first = link_and_finish(&fake, &mut run, uuid, Some(telegram::PASSWORD)).await;
        let batches = |events: &[BackendEvent]| {
            events
                .iter()
                .filter(|e| matches!(e, BackendEvent::HistoryBatch { .. }))
                .count()
        };
        assert_eq!(batches(&first), 2, "{first:?}");
        stop(run).await;

        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Resume,
            None,
            &rt,
        );
        send(&run, Command::Connect).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            run.ends
                .conn
                .wait_for(|c| matches!(c, ConnState::Online { .. })),
        )
        .await
        .expect("timely")
        .expect("online");
        let mut again = Vec::new();
        for _ in 0..3 {
            again.extend(heard(&mut run).await.unwrap_or_default());
        }
        assert_eq!(batches(&again), 0, "the history came again: {again:?}");
        assert!(
            !again
                .iter()
                .any(|e| matches!(e, BackendEvent::AccountReady { .. })),
            "not linked again"
        );
        stop(run).await;
    });
}

/// An arrival of the fake that plays Telegram has Telegram's id shape (a server id shifted by 20
/// bits, in decimal, newer than the history) and the shared words; the fake that plays WhatsApp
/// keeps its stanza-id shape. Mutant: the WhatsApp prefix for every kind; Telegram's shape for
/// every kind.
#[test]
fn an_arrival_has_telegrams_id_shape_and_whatsapps_keeps_its_own() {
    use kourier_fake::rehearsal;

    let rt = runtime();
    rt.block_on(async {
        // Telegram.
        let dir = tempfile::tempdir().expect("tempdir");
        let uuid = AccountUuid::new_random();
        let fake = fast();
        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(telegram::RIGHT_TAIL),
            &rt,
        );
        link_and_finish(&fake, &mut run, uuid, Some(telegram::PASSWORD)).await;
        let control = fake.control();
        control.arrive(Arrival::Group).expect("a group chat");
        let message = arrived(&mut run).await;
        assert_eq!(message.conv, group());
        assert_eq!(&*message.local, telegram::arrival_id(1));
        control.arrive(Arrival::Direct).expect("a direct chat");
        let message = arrived(&mut run).await;
        assert_eq!(message.conv, ConvKey::Direct(peer()));
        assert_eq!(&*message.local, telegram::arrival_id(2));
        let id: i64 = message.local.parse().expect("decimal");
        assert_eq!(id % (1 << 20), 0);
        assert_eq!(
            format!("{:?}", message.body),
            format!(
                "{:?}",
                kourier_model::Body::text(format!("{} 2", telegram::ARRIVAL_WORDS))
            )
        );
        stop(run).await;

        // WhatsApp keeps its own.
        let dir = tempfile::tempdir().expect("tempdir");
        let uuid = AccountUuid::new_random();
        let fake = FakeNetwork::rehearsal_with(rehearsal::Timings {
            code: Duration::from_millis(25),
            pause: Duration::from_millis(25),
        });
        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(rehearsal::RIGHT_TAIL),
            &rt,
        );
        link_and_finish(&fake, &mut run, uuid, None).await;
        fake.control().arrive(Arrival::Direct).expect("a chat");
        let message = arrived(&mut run).await;
        assert_eq!(
            &*message.local,
            format!("{}{:08}", rehearsal::ARRIVAL_ID_PREFIX, 1),
            "WhatsApp's stanza-id shape"
        );
        stop(run).await;
    });
}

/// The live message the next envelopes bring.
async fn arrived(run: &mut Running) -> kourier_backend_api::IncomingMessage {
    loop {
        for e in next(run).await {
            if let BackendEvent::Message(m) = e {
                return m;
            }
        }
    }
}
