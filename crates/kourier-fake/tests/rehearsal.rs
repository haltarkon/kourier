//! The rehearsal network (T1a, A1 and A2): the fake that plays WhatsApp for the run tool's
//! rehearsal refuses a pairing whose number misses the expected tail like the real backend
//! (W2c), pushes its history over time, goes online without replaying it after a restart, and
//! puts one text on cue into the first chat of a kind of the account that runs.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEvent, Command, ContextParams, DbKey, Envelope, Network,
    StartMode, SystemClock,
};
use kourier_fake::rehearsal::{self, Timings};
use kourier_fake::{Arrival, ArriveError, FakeNetwork};
use kourier_model::{
    AccountId, AccountUuid, ConnState, ConvKey, ErrorKind, LoginStepKind, Redacted, UserId,
};
use tokio_util::sync::CancellationToken;

/// Fast waits: the codes and the pauses of the rehearsal take milliseconds here.
const FAST: Timings = Timings {
    code: Duration::from_millis(25),
    pause: Duration::from_millis(25),
};

fn fast() -> FakeNetwork {
    FakeNetwork::rehearsal_with(FAST)
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
        clock: Arc::new(SystemClock),
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

async fn start_qr(run: &Running) {
    send(
        run,
        Command::StartLogin {
            flow: "qr".into(),
            relink: None,
            taken: Vec::new(),
        },
    )
    .await;
}

/// What a core sees of the actor, in order.
#[derive(Clone, Debug, PartialEq)]
enum Seen {
    Qr,
    Failed {
        at: String,
        kind: ErrorKind,
    },
    LoginDone,
    Ready,
    /// A conversation the network states (a chat appears in the list).
    Chat(ConvKey),
    /// A history batch: its conversation, how many messages, whether more follow.
    Batch(ConvKey, usize, bool),
    /// A live message: conversation, author, text.
    Message(ConvKey, UserId, String),
}

fn seen_of(events: &[BackendEvent]) -> Vec<Seen> {
    events
        .iter()
        .filter_map(|e| match e {
            BackendEvent::Login(s) => match &s.kind {
                LoginStepKind::ShowQr { .. } => Some(Seen::Qr),
                LoginStepKind::Failed { error } => Some(Seen::Failed {
                    at: s.step_id.clone(),
                    kind: error.kind.clone(),
                }),
                LoginStepKind::Done => Some(Seen::LoginDone),
                _ => None,
            },
            BackendEvent::AccountReady { .. } => Some(Seen::Ready),
            BackendEvent::ConversationUpsert(patch) => Some(Seen::Chat(patch.key.clone())),
            BackendEvent::HistoryBatch {
                conv,
                messages,
                has_more,
                ..
            } => Some(Seen::Batch(conv.clone(), messages.len(), *has_more)),
            BackendEvent::Message(m) => match &m.origin {
                kourier_model::Origin::Remote { author, .. } => Some(Seen::Message(
                    m.conv.clone(),
                    author.clone(),
                    body_text(&m.body),
                )),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

fn body_text(body: &kourier_model::Body) -> String {
    match body {
        kourier_model::Body::Text(t) => t.body.clone(),
        other => format!("{other:?}"),
    }
}

async fn next(run: &mut Running) -> Vec<Seen> {
    let env: Envelope = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
        .await
        .expect("timely")
        .expect("open");
    if let Some(a) = env.ack {
        a.complete(Ok(()));
    }
    seen_of(&env.events)
}

/// Reads until `f` holds for something seen; returns everything seen on the way.
async fn until(run: &mut Running, mut f: impl FnMut(&Seen) -> bool) -> Vec<Seen> {
    let mut all = Vec::new();
    loop {
        let seen = next(run).await;
        let hit = seen.iter().any(&mut f);
        all.extend(seen);
        if hit {
            return all;
        }
    }
}

/// Reads and acks everything the actor says until its script is over (every delivery acked).
async fn until_finished(run: &mut Running, fake: &FakeNetwork, uuid: AccountUuid) -> Vec<Seen> {
    let control = fake.control();
    let mut all = Vec::new();
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                () = control.wait_for(|c| c.is_finished(uuid)) => break,
                env = run.ends.events.recv() => {
                    let env = env.expect("open");
                    if let Some(a) = env.ack {
                        a.complete(Ok(()));
                    }
                    all.extend(seen_of(&env.events));
                }
            }
        }
    })
    .await;
    waited.expect("the script finished in time");
    // What was queued behind the last acknowledgement.
    while let Ok(env) = run.ends.events.try_recv() {
        if let Some(a) = env.ack {
            a.complete(Ok(()));
        }
        all.extend(seen_of(&env.events));
    }
    all
}

async fn online(run: &mut Running) {
    tokio::time::timeout(
        Duration::from_secs(10),
        run.ends
            .conn
            .wait_for(|c| matches!(c, ConnState::Online { .. })),
    )
    .await
    .expect("timely")
    .expect("online");
}

fn peer() -> UserId {
    UserId::from_parts("pn", rehearsal::PEER_NUMBER).expect("id")
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{}", rehearsal::GROUP_NUMBER).into())
}

/// Links the rehearsal account with the right tail and lets its script run to the end.
async fn linked_and_finished(
    fake: &FakeNetwork,
    dir: &std::path::Path,
    uuid: AccountUuid,
    rt: &tokio::runtime::Runtime,
) -> (Running, Vec<Seen>) {
    let mut run = spawn(
        fake,
        dir,
        uuid,
        StartMode::Fresh,
        Some(rehearsal::RIGHT_TAIL),
        rt,
    );
    start_qr(&run).await;
    let mut seen = until_finished(&mut run, fake, uuid).await;
    seen.extend(next_settled(&mut run).await);
    (run, seen)
}

/// Whatever else is already waiting (acked), without waiting for more.
async fn next_settled(run: &mut Running) -> Vec<Seen> {
    let mut all = Vec::new();
    tokio::time::sleep(Duration::from_millis(50)).await;
    while let Ok(env) = run.ends.events.try_recv() {
        if let Some(a) = env.ack {
            a.complete(Ok(()));
        }
        all.extend(seen_of(&env.events));
    }
    all
}

#[test]
fn the_rehearsal_refuses_a_pairing_whose_number_misses_the_tail() {
    let rt = runtime();
    let fake = fast();
    rt.block_on(async {
        // The wrong phone, and the tail no number has (a start without typed digits).
        for tail in [rehearsal::WRONG_TAIL, "000000000000000"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let uuid = AccountUuid::new_random();
            let mut run = spawn(&fake, dir.path(), uuid, StartMode::Fresh, Some(tail), &rt);
            start_qr(&run).await;
            let seen = until(&mut run, |s| matches!(s, Seen::Failed { .. })).await;
            let qrs = seen.iter().filter(|s| **s == Seen::Qr).count();
            assert_eq!(qrs, 2, "two codes of the rehearsal: {seen:?}");
            assert_eq!(
                seen.last(),
                Some(&Seen::Failed {
                    at: "qr".into(),
                    kind: ErrorKind::NotTheExpectedAccount
                }),
                "the login ends at the QR step, refused: {seen:?}"
            );
            // No account came of it: nothing more is said, no device is on the server.
            assert!(next_settled(&mut run).await.is_empty());
            assert!(!seen.contains(&Seen::Ready));
            assert!(!fake.control().has_device(uuid), "the tail {tail}");
            stop(run).await;
        }

        // The right phone links.
        let dir = tempfile::tempdir().expect("tempdir");
        let uuid = AccountUuid::new_random();
        let mut run = spawn(
            &fake,
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(rehearsal::RIGHT_TAIL),
            &rt,
        );
        start_qr(&run).await;
        let seen = until(&mut run, |s| *s == Seen::Ready).await;
        assert_eq!(
            seen.iter().filter(|s| **s == Seen::Qr).count(),
            2,
            "{seen:?}"
        );
        assert!(fake.control().has_device(uuid));
        stop(run).await;
    });
}

#[test]
fn a_resumed_rehearsal_account_goes_online_without_replaying_its_history() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (run, first) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let batches = |seen: &[Seen]| seen.iter().filter(|s| matches!(s, Seen::Batch(..))).count();
        // Four batches of messages, one end for each chat.
        assert_eq!(batches(&first), 6, "{first:?}");
        stop(run).await;

        // The next start of the same account, in the same process or another: online, nothing
        // delivered again.
        let mut run = spawn(&fake, dir.path(), uuid, StartMode::Resume, None, &rt);
        send(&run, Command::Connect).await;
        online(&mut run).await;
        let mut again = next_settled(&mut run).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        again.extend(next_settled(&mut run).await);
        assert_eq!(batches(&again), 0, "the history came again: {again:?}");
        assert!(!again.contains(&Seen::Ready) && !again.contains(&Seen::LoginDone));
        stop(run).await;
    });
}

/// The conversations of a push come with its chunks, one after the other: the direct chat before
/// its first batch, the group's chat with ITS first batch and not before the direct chat's batch
/// was delivered. The run tool's `history-interrupt` needs the number of conversations to grow
/// while the push runs. Mutant: both chats in the first delivery.
#[test]
fn the_rehearsals_chats_arrive_with_their_chunks() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (run, seen) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let position = |wanted: &Seen| seen.iter().position(|s| s == wanted);
        let direct = ConvKey::Direct(peer());
        let first_batch = |conv: &ConvKey| {
            seen.iter()
                .position(|s| matches!(s, Seen::Batch(c, n, _) if c == conv && *n > 0))
        };
        let (chat_direct, chat_group) = (
            position(&Seen::Chat(direct.clone())).expect("the direct chat is stated"),
            position(&Seen::Chat(group())).expect("the group's chat is stated"),
        );
        let (batch_direct, batch_group) = (
            first_batch(&direct).expect("the direct chat's batch"),
            first_batch(&group()).expect("the group's batch"),
        );
        assert!(chat_direct < batch_direct, "{seen:?}");
        assert!(
            batch_direct < chat_group,
            "the group's chat stood before the direct chat's chunk: {seen:?}"
        );
        assert!(chat_group < batch_group, "{seen:?}");
        stop(run).await;
    });
}

#[test]
fn arrive_puts_one_text_into_the_first_chat_of_that_kind() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (mut run, _) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let control = fake.control();

        control.arrive(Arrival::Group).expect("a group chat");
        let seen = until(&mut run, |s| matches!(s, Seen::Message(..))).await;
        assert_eq!(
            seen.last(),
            Some(&Seen::Message(
                group(),
                peer(),
                "Rehearsal arrival 1".into()
            ))
        );

        control.arrive(Arrival::Direct).expect("a direct chat");
        let seen = until(&mut run, |s| matches!(s, Seen::Message(..))).await;
        assert_eq!(
            seen.last(),
            Some(&Seen::Message(
                ConvKey::Direct(peer()),
                peer(),
                "Rehearsal arrival 2".into()
            )),
            "the direct chat gets its own: {seen:?}"
        );
        // Exactly one message each, nothing else arrives by itself.
        assert!(next_settled(&mut run).await.is_empty());
        stop(run).await;
    });
}

#[test]
fn arrive_without_a_running_account_is_refused() {
    let rt = runtime();
    let fake = fast();
    let control = fake.control();
    // No account at all.
    assert_eq!(
        control.arrive(Arrival::Direct),
        Err(ArriveError::NoRunningAccount)
    );
    rt.block_on(async {
        // An account that runs but has no chat yet (it is not linked).
        let dir = tempfile::tempdir().expect("tempdir");
        let uuid = AccountUuid::new_random();
        let run = spawn(
            &fake,
            dir.path(),
            uuid,
            StartMode::Fresh,
            Some(rehearsal::RIGHT_TAIL),
            &rt,
        );
        // (The actor opens its door a moment after `spawn` returns.)
        let opened = tokio::time::timeout(Duration::from_secs(10), async {
            while control.arrive(Arrival::Direct) == Err(ArriveError::NoRunningAccount) {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await;
        assert!(opened.is_ok(), "the account's door opened");
        assert_eq!(
            control.arrive(Arrival::Direct),
            Err(ArriveError::NoChat(Arrival::Direct))
        );
        assert_eq!(
            control.arrive(Arrival::Group),
            Err(ArriveError::NoChat(Arrival::Group))
        );
        // It stops: the door closes with it.
        stop(run).await;
        assert_eq!(
            control.arrive(Arrival::Group),
            Err(ArriveError::NoRunningAccount)
        );
    });
}

/// W5a D16: the rehearsal's chats state what the WhatsApp backend states for chats the phone started
/// (`scripts::whatsapp_features`, the one source `kourier-whatsapp` compares with), never Signal's; and the fake
/// playing WhatsApp gives its own messages WhatsApp's ids: `3EB0` and 18 upper-case hex digits, 22 characters.
#[test]
fn the_rehearsal_states_whatsapps_features_and_whatsapps_ids() {
    use kourier_backend_api::Network as _;
    use kourier_fake::Step;
    use kourier_fake::scripts::whatsapp_features;

    let script = rehearsal::script(FAST).expect("the rehearsal's script");
    let mut stated = Vec::new();
    for step in &script.steps {
        let Step::Deliver(delivery) = step else {
            continue;
        };
        for event in &delivery.events {
            if let BackendEvent::ConversationUpsert(patch) = event
                && let Some(features) = &patch.features
            {
                stated.push((matches!(patch.key, ConvKey::Group(_)), features.clone()));
            }
        }
    }
    assert!(
        stated.iter().any(|(group, _)| *group) && stated.iter().any(|(group, _)| !*group),
        "the rehearsal states no direct chat or no group"
    );
    for (group, features) in stated {
        assert_eq!(
            features,
            whatsapp_features(group),
            "a rehearsal chat states other features than the WhatsApp backend"
        );
    }

    let fake = fast();
    let id = fake.allocate_message_id(&[], &ConvKey::Direct(peer()), 1_760_000_123_456);
    assert_eq!(
        id.local.len(),
        22,
        "the fake playing WhatsApp gives no 22-character id"
    );
    assert!(
        id.local.starts_with("3EB0")
            && id.local[4..]
                .chars()
                .all(|c| c.is_ascii_digit() || ('A'..='F').contains(&c)),
        "the fake playing WhatsApp gives no id of WhatsApp's shape"
    );
    assert_eq!(id.sent_at, 1_760_000_123_456);
}

// ---- T2 T-5: the acts of the rehearsal's phone on cue ----------------------------------------

use kourier_backend_api::IncomingMessage;
use kourier_fake::PhoneAct;
use kourier_model::{CallDirection, CallState, Origin, Patch, TargetAuthor};

/// The raw events the actor has queued (acked), after a short fixed wait. Only for a check that
/// nothing came, and for a drain that asserts on nothing: a test that expects an event waits for
/// it ([`act_events`]), because under load the actor commits it later than any fixed wait.
async fn events_now(run: &mut Running) -> Vec<BackendEvent> {
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut all = Vec::new();
    while let Ok(env) = run.ends.events.try_recv() {
        if let Some(a) = env.ack {
            a.complete(Ok(()));
        }
        all.extend(env.events);
    }
    all
}

/// Waits until `f` finds an event, reading and acking what the actor says.
async fn event_until(
    run: &mut Running,
    within: Duration,
    mut f: impl FnMut(&BackendEvent) -> bool,
) -> Vec<BackendEvent> {
    let mut all = Vec::new();
    let waited = tokio::time::timeout(within, async {
        loop {
            let env = run.ends.events.recv().await.expect("open");
            if let Some(a) = env.ack {
                a.complete(Ok(()));
            }
            let hit = env.events.iter().any(&mut f);
            all.extend(env.events);
            if hit {
                break;
            }
        }
    })
    .await;
    waited.expect("the event came in time");
    all
}

/// How long a test waits for what an act of the phone says. The actor plays the acts in the
/// order they were sent and commits each one when it gets to it; under load that takes seconds.
const ACT_WITHIN: Duration = Duration::from_secs(20);

/// What an act of the phone says: waits until `f` finds an event, so the events of the act
/// are in, then adds whatever else is already queued (it waits for nothing more).
async fn act_events(run: &mut Running, f: impl FnMut(&BackendEvent) -> bool) -> Vec<BackendEvent> {
    let mut all = event_until(run, ACT_WITHIN, f).await;
    while let Ok(env) = run.ends.events.try_recv() {
        if let Some(a) = env.ack {
            a.complete(Ok(()));
        }
        all.extend(env.events);
    }
    all
}

fn direct() -> ConvKey {
    ConvKey::Direct(peer())
}

fn call_of(event: &BackendEvent) -> Option<(&IncomingMessage, &kourier_model::CallLine)> {
    match event {
        BackendEvent::Message(m) => m.body.call_line().map(|line| (m, line)),
        _ => None,
    }
}

/// The phone's own settings, on cue: the pin list is the network's (the newest pin first, the whole
/// list each time), the archive is one flag of the chat, a mute is "for ever" and its removal clears
/// it. None of it is a frame of the run tool's budget (the fake took nothing from the app).
/// Mutants: the pin list in the other order; an unmute that stays muted.
#[test]
fn the_phone_pins_archives_and_mutes_a_chat_on_cue() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (mut run, _) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let control = fake.control();
        let (d, g) = (direct(), group());

        let is_pin = |e: &BackendEvent| matches!(e, BackendEvent::PinnedConversations(_));
        let is_upsert = |e: &BackendEvent| matches!(e, BackendEvent::ConversationUpsert(_));
        let pins = |events: &[BackendEvent]| {
            events
                .iter()
                .filter_map(|e| match e {
                    BackendEvent::PinnedConversations(list) => Some(list.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        control.phone(Arrival::Direct, PhoneAct::Pin).expect("pin");
        assert_eq!(
            pins(&act_events(&mut run, is_pin).await),
            vec![vec![d.clone()]]
        );
        control.phone(Arrival::Group, PhoneAct::Pin).expect("pin");
        assert_eq!(
            pins(&act_events(&mut run, is_pin).await),
            vec![vec![g.clone(), d.clone()]],
            "the newest pin is first"
        );
        control
            .phone(Arrival::Direct, PhoneAct::Unpin)
            .expect("unpin");
        assert_eq!(
            pins(&act_events(&mut run, is_pin).await),
            vec![vec![g.clone()]]
        );

        let patches = |events: Vec<BackendEvent>| {
            events
                .into_iter()
                .filter_map(|e| match e {
                    BackendEvent::ConversationUpsert(p) => Some(p),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        control
            .phone(Arrival::Group, PhoneAct::Archive)
            .expect("archive");
        let seen = patches(act_events(&mut run, is_upsert).await);
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].key, g);
        assert_eq!(seen[0].archived, Some(true));
        assert_eq!(seen[0].muted_until, Patch::Unchanged, "only the archive");
        control
            .phone(Arrival::Group, PhoneAct::Unarchive)
            .expect("unarchive");
        assert_eq!(
            patches(act_events(&mut run, is_upsert).await)[0].archived,
            Some(false)
        );

        control
            .phone(Arrival::Direct, PhoneAct::Mute)
            .expect("mute");
        let seen = patches(act_events(&mut run, is_upsert).await);
        assert_eq!(
            (seen[0].key.clone(), &seen[0].muted_until),
            (d.clone(), &Patch::Set(i64::MAX))
        );
        assert_eq!(seen[0].archived, None, "only the mute");
        control
            .phone(Arrival::Direct, PhoneAct::Unmute)
            .expect("unmute");
        assert_eq!(
            patches(act_events(&mut run, is_upsert).await)[0].muted_until,
            Patch::Clear
        );

        assert_eq!(control.frames(), 0, "the phone's acts are no frames");
        stop(run).await;
    });
}

/// A call into the 1:1 chat rings (the other person's signal), and the phone's own log says a
/// moment later that it was answered there (an own-device signal); a group has no call to play.
/// Mutants: the answer said by the caller; no ring before the answer.
#[test]
fn a_call_rings_and_is_answered_on_the_phone() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (mut run, _) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let control = fake.control();
        assert_eq!(
            control.phone(Arrival::Group, PhoneAct::Call),
            Err(ArriveError::Unsupported(PhoneAct::Call, Arrival::Group))
        );
        control
            .phone(Arrival::Direct, PhoneAct::Call)
            .expect("call");
        let started = std::time::Instant::now();
        let events = event_until(&mut run, Duration::from_secs(10), |e| {
            call_of(e).is_some_and(|(_, line)| line.state == CallState::Answered)
        })
        .await;
        let lines: Vec<_> = events.iter().filter_map(call_of).collect();
        assert_eq!(lines.len(), 2, "a ring and its answer: {events:?}");
        let (ring_msg, ring) = lines[0];
        assert!(matches!(ring.state, CallState::Ringing { .. }), "{ring:?}");
        assert_eq!(ring.direction, CallDirection::Incoming);
        assert!(!ring.group);
        assert_eq!(ring_msg.conv, direct());
        assert!(
            matches!(&ring_msg.origin, Origin::Remote { author, .. } if *author == peer()),
            "the other person rings"
        );
        let (answer_msg, answer) = lines[1];
        assert_eq!(answer.state, CallState::Answered);
        assert!(
            matches!(answer_msg.origin, Origin::OwnOtherDevice { .. }),
            "the phone's own signal: {:?}",
            answer_msg.origin
        );
        assert_eq!(answer_msg.local, ring_msg.local, "one call, one line");
        assert!(
            started.elapsed() >= rehearsal::CALL_RINGS_FOR,
            "the ring lasted {:?}",
            started.elapsed()
        );
        assert_eq!(control.frames(), 0);
        stop(run).await;
    });
}

/// The other person's react, edit and delete are on the text that arrived LAST in the chat, also
/// when the cue comes right behind the arrival's; nothing is played for a chat that had none this
/// start. Mutants: the pre-check of the cue behind an arrival removed; the newest text not updated
/// by a second arrival.
#[test]
fn the_other_person_reacts_edits_and_deletes_the_text_that_arrived_last() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (mut run, _) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let control = fake.control();
        for act in [
            PhoneAct::PeerReact,
            PhoneAct::PeerEdit,
            PhoneAct::PeerDelete,
        ] {
            assert_eq!(
                control.phone(Arrival::Direct, act),
                Err(ArriveError::NothingArrived(act)),
                "no text has arrived yet"
            );
        }
        assert!(events_now(&mut run).await.is_empty(), "nothing was played");

        // Two arrivals, the cues sent right behind the second one without waiting for it.
        control.arrive(Arrival::Direct).expect("first text");
        control.arrive(Arrival::Direct).expect("second text");
        control
            .phone(Arrival::Direct, PhoneAct::PeerReact)
            .expect("react");
        control
            .phone(Arrival::Direct, PhoneAct::PeerEdit)
            .expect("edit");
        control
            .phone(Arrival::Direct, PhoneAct::PeerDelete)
            .expect("delete");
        // The actor plays the two arrivals and the three cues in the order they were sent: the
        // delete is the last, so with it the others are in.
        let events = act_events(&mut run, |e| matches!(e, BackendEvent::Deleted { .. })).await;
        let second = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Message(m) => Some(m.local.to_string()),
                _ => None,
            })
            .nth(1)
            .expect("the second arrival");
        let on_second = |t: &kourier_model::MsgTarget| {
            t.conv.as_ref() == Some(&direct())
                && t.author == TargetAuthor::User(peer())
                && *t.local == second
        };
        let mut seen = (0, 0, 0);
        for event in &events {
            match event {
                BackendEvent::Reaction {
                    target, by, remove, ..
                } => {
                    assert!(
                        on_second(target) && *by == Some(peer()) && !remove,
                        "{event:?}"
                    );
                    seen.0 += 1;
                }
                BackendEvent::Edited {
                    target,
                    by,
                    new_local,
                    body,
                    ..
                } => {
                    assert!(on_second(target) && *by == Some(peer()), "{event:?}");
                    assert!(new_local.is_none(), "WhatsApp keeps the id of an edit");
                    assert_eq!(body_text(body), "Rehearsal arrival 2 edited");
                    seen.1 += 1;
                }
                BackendEvent::Deleted {
                    target,
                    by,
                    for_everyone,
                    ..
                } => {
                    assert!(on_second(target) && *by == Some(peer()) && *for_everyone);
                    seen.2 += 1;
                }
                _ => {}
            }
        }
        assert_eq!(seen, (1, 1, 1), "{events:?}");
        assert_eq!(control.frames(), 0);
        stop(run).await;
    });
}

// ---- The two count lines of a frame (T2 F1) --------------------------------------------------------------------

/// The events of the process, as `message=.. name=value` text (the run tool's lines are read from stderr; this is
/// the same words before the formatter). Installed once for the process: the actor runs on a thread of its own.
mod logged {
    use std::fmt::Debug;
    use std::sync::{Mutex, OnceLock, PoisonError};

    use tracing::field::{Field, Visit};
    use tracing::{Event, Metadata, Subscriber, span};

    static LINES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

    struct Collect;

    impl Subscriber for Collect {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &span::Attributes<'_>) -> span::Id {
            span::Id::from_u64(1)
        }
        fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
        fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
        fn event(&self, event: &Event<'_>) {
            struct All(Vec<String>);
            impl Visit for All {
                fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
                    self.0.push(format!("{}={value:?}", field.name()));
                }
            }
            let mut all = All(Vec::new());
            event.record(&mut all);
            LINES
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(all.0.join(" "));
        }
        fn enter(&self, _: &span::Id) {}
        fn exit(&self, _: &span::Id) {}
    }

    /// Installs the collector (once; a second call in the same process finds it there).
    pub(super) fn install() {
        let _ = tracing::subscriber::set_global_default(Collect);
    }

    /// The events so far whose text holds `needle`.
    pub(super) fn containing(needle: &str) -> Vec<String> {
        LINES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|l| l.contains(needle))
            .cloned()
            .collect()
    }
}

/// One transmit into the 1:1 chat, answered.
async fn transmit_into(
    run: &Running,
    local: &str,
    kind: kourier_backend_api::OutgoingKind,
) -> kourier_backend_api::TransmitOutcome {
    let req = kourier_backend_api::OutgoingRequest {
        conv: direct(),
        local: local.into(),
        sent_at: 5,
        kind,
        only: None,
        is_recipient_update: false,
    };
    let (reply, answer) = tokio::sync::oneshot::channel();
    send(
        run,
        Command::Transmit {
            local: kourier_model::MessageId(1),
            req: Box::new(req),
            reply,
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), answer)
        .await
        .expect("timely")
        .expect("answered")
}

/// T2 F8: the fake HOLDS BACK the ack of the run tool's resend text the first time the server takes a new copy of it
/// (the frame is written and counted, no answer ever comes), so the kill comes before the ack; the same id again (the
/// next start's resend) is a repeat the server drops and it is answered; another text, and the resend text under a new
/// id, are other messages (the second is held again). Mutants (the walk needs a real resend to pass): the hold dropped
/// (the first answer comes at once); the repeat treated as a new copy (it is held again and not announced).
#[test]
fn the_rehearsals_fake_holds_the_ack_of_the_resend_text_once_and_drops_its_repeat() {
    logged::install();
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (run, _) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let control = fake.control();
        let (written, answered, repeated) = (
            || logged::containing("a send frame was written"),
            || logged::containing("a send frame was answered"),
            || logged::containing("a send frame repeated a message the server already holds"),
        );
        let text = |body: &str| kourier_backend_api::OutgoingKind::Message {
            body: kourier_model::Body::Text(kourier_model::Text::plain(body)),
            reply_to: None,
            expiry: None,
        };
        let resend = || text(rehearsal::HELD_ACK_TEXT);
        // The first frame: taken by the server (written, counted), never answered.
        let (reply, mut answer) = tokio::sync::oneshot::channel();
        send(
            &run,
            Command::Transmit {
                local: kourier_model::MessageId(1),
                req: Box::new(kourier_backend_api::OutgoingRequest {
                    conv: direct(),
                    local: "3EB0F1E2D3C4B5A69788".into(),
                    sent_at: 5,
                    kind: resend(),
                    only: None,
                    is_recipient_update: false,
                }),
                reply,
            },
        )
        .await;
        for _ in 0..200 {
            if control.frames() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(control.frames(), 1, "the fake did not take the frame");
        assert!(
            tokio::time::timeout(Duration::from_millis(600), &mut answer)
                .await
                .is_err(),
            "the ack of the resend text was not held back"
        );
        assert_eq!(written().len(), 1, "{:?}", written());
        assert!(answered().is_empty(), "{:?}", answered());
        assert!(repeated().is_empty(), "{:?}", repeated());
        // The same id again: the server holds it, drops the repeat and answers (acked).
        let again = transmit_into(&run, "3EB0F1E2D3C4B5A69788", resend()).await;
        assert!(
            matches!(again, kourier_backend_api::TransmitOutcome::Sent { .. }),
            "{again:?}"
        );
        assert_eq!(written().len(), 2, "{:?}", written());
        assert_eq!(repeated().len(), 1, "{:?}", repeated());
        assert_eq!(answered().len(), 1, "{:?}", answered());
        assert!(answered()[0].contains("outcome=acked"), "{:?}", answered());
        // Another text is another message: acked at once, no repeat.
        let other = transmit_into(&run, "3EB0A1B2C3D4E5F60718", text("synthetic other")).await;
        assert!(matches!(
            other,
            kourier_backend_api::TransmitOutcome::Sent { .. }
        ));
        assert_eq!(repeated().len(), 1, "{:?}", repeated());
        assert_eq!(answered().len(), 2, "{:?}", answered());
        // The resend text under a NEW id is a new message, and its ack is held back again: a resend that lost its
        // id is not answered, which is how the walk sees it.
        let (reply, mut answer) = tokio::sync::oneshot::channel();
        send(
            &run,
            Command::Transmit {
                local: kourier_model::MessageId(3),
                req: Box::new(kourier_backend_api::OutgoingRequest {
                    conv: direct(),
                    local: "3EB0C9D8E7F605142332".into(),
                    sent_at: 6,
                    kind: resend(),
                    only: None,
                    is_recipient_update: false,
                }),
                reply,
            },
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(600), &mut answer)
                .await
                .is_err(),
            "the resend text under a new id was answered"
        );
        assert_eq!(repeated().len(), 1, "{:?}", repeated());
        assert_eq!(answered().len(), 2, "{:?}", answered());
        for line in written().iter().chain(repeated().iter()) {
            assert!(
                !line.contains("3EB0") && !line.contains("Kourier test run"),
                "{line}"
            );
        }
        stop(run).await;
    });
}

/// T2 F1: the fake that plays WhatsApp says, for every frame it takes from the app (a send, a reaction, a timer
/// set), the two count lines the real backend says at its write point, `a send frame was written kind=..` and `a send
/// frame was answered outcome=acked code=0`, and none for what is no frame (the phone's acts): the run tool's count of
/// these lines equals `FakeControl::frames`. Mutants: the `written` line dropped; the line said for a phone's act; the
/// timer set not said.
#[test]
fn the_rehearsals_fake_says_the_two_count_lines_for_every_frame_it_takes() {
    logged::install();
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let uuid = AccountUuid::new_random();
    let fake = fast();
    rt.block_on(async {
        let (mut run, _) = linked_and_finished(&fake, dir.path(), uuid, &rt).await;
        let control = fake.control();
        let (written, answered) = (
            || logged::containing("a send frame was written"),
            || logged::containing("a send frame was answered"),
        );
        assert!(
            written().is_empty() && answered().is_empty(),
            "{:?}",
            written()
        );
        // The phone's acts are no frames and say nothing.
        control.phone(Arrival::Direct, PhoneAct::Pin).expect("pin");
        let _ = events_now(&mut run).await;
        assert!(written().is_empty(), "{:?}", written());
        // A send, a reaction and a timer set.
        let sent = transmit_into(
            &run,
            "3EB0F1E2D3C4B5A69788",
            kourier_backend_api::OutgoingKind::Message {
                body: kourier_model::Body::Text(kourier_model::Text::plain("synthetic hello")),
                reply_to: None,
                expiry: None,
            },
        )
        .await;
        assert!(matches!(
            sent,
            kourier_backend_api::TransmitOutcome::Sent { .. }
        ));
        let reacted = transmit_into(
            &run,
            "3EB0A1B2C3D4E5F60718",
            kourier_backend_api::OutgoingKind::React {
                target: kourier_model::MsgTarget {
                    conv: Some(direct()),
                    author: kourier_model::TargetAuthor::Own,
                    local: "3EB0F1E2D3C4B5A69788".into(),
                    alternates: Vec::new(),
                },
                emoji: "\u{1F44D}".into(),
                remove: false,
            },
        )
        .await;
        assert!(matches!(
            reacted,
            kourier_backend_api::TransmitOutcome::Sent { .. }
        ));
        let (reply, answer) = tokio::sync::oneshot::channel();
        send(
            &run,
            Command::ChangeConversation {
                conv: direct(),
                change: kourier_model::ConversationChange::SetTimer {
                    timer: Some(Duration::from_secs(86_400)),
                },
                id: Some(kourier_backend_api::AllocatedId {
                    local: "3EB0C9D8E7F605142332".into(),
                    sent_at: 6,
                }),
                reply,
            },
        )
        .await;
        answer.await.expect("answered").expect("accepted");
        assert_eq!(control.frames(), 3);
        let kinds: Vec<String> = written()
            .iter()
            .map(|l| {
                l.split_whitespace()
                    .find_map(|w| w.strip_prefix("kind="))
                    .unwrap_or("?")
                    .to_owned()
            })
            .collect();
        assert_eq!(kinds, ["message", "reaction", "timer"], "{:?}", written());
        let answered = answered();
        assert_eq!(answered.len(), 3, "{answered:?}");
        assert!(
            answered.iter().all(|l| l.contains("outcome=acked")
                && l.contains("code=0")
                && l.contains("again=no")),
            "{answered:?}"
        );
        // Nothing but the fixed words, one kind or outcome and a code.
        for line in written().iter().chain(answered.iter()) {
            assert!(
                !line.contains("3EB0") && !line.contains("synthetic"),
                "{line}"
            );
        }
        stop(run).await;
    });
}
