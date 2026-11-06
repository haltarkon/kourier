//! The rehearsal network of Threema (TR T8): the fake that plays Threema for the run tool's
//! rehearsal shows ONE QR code that the phone scans at once, then the three emoji for the time its
//! script says, links as the synthetic own ID, serves a direct chat and a group with NO history
//! while it says `Syncing`, goes online only after the acks, replays nothing when it resumes, and
//! puts an arrival on cue. It judges no Threema ID (decision M7). Nothing of Threema is contacted
//! and `kourier-fake` holds no Threema code: every value here is made up.

use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{
    AccountContext, BackendConfig, BackendEvent, Clock, Command, ContextParams, DbKey, Envelope,
    FakeClock, Network, StartMode, SystemClock,
};
use kourier_fake::threema::{self, Timings};
use kourier_fake::{Arrival, FAKE_EMOJI, FakeNetwork, Flavor};
use kourier_model::{
    AccountId, AccountUuid, ConnState, ConvKey, LoginStepKind, MemberRole, NetworkKind, Patch,
    UserId,
};
use tokio_util::sync::CancellationToken;

/// Fast waits for the tests that run on the system clock.
const FAST: Timings = Timings {
    code: Duration::from_millis(25),
    emoji: Duration::from_millis(25),
};

fn fast() -> FakeNetwork {
    FakeNetwork::rehearsal_threema_with(FAST)
}

fn peer() -> UserId {
    UserId::from_parts("threema", threema::PEER_ID).expect("id")
}

fn own() -> UserId {
    UserId::from_parts("threema", threema::OWN_ID).expect("id")
}

fn group() -> ConvKey {
    ConvKey::Group(format!("threema:{}.{}", threema::PEER_ID, threema::GROUP_ID_HEX).into())
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
        config: BackendConfig::default(),
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

async fn start(run: &Running) {
    run.ends
        .commands
        .send(Command::StartLogin {
            flow: "qr".into(),
            relink: None,
            taken: Vec::new(),
        })
        .await
        .expect("send");
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

/// Links on the system clock and reads every event until the script is over.
async fn link_and_finish(
    fake: &FakeNetwork,
    run: &mut Running,
    uuid: AccountUuid,
) -> Vec<BackendEvent> {
    let control = fake.control();
    start(run).await;
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

async fn arrived(run: &mut Running) -> kourier_backend_api::IncomingMessage {
    loop {
        for e in next(run).await {
            if let BackendEvent::Message(m) = e {
                return m;
            }
        }
    }
}

#[test]
fn the_rehearsal_is_threema_without_the_warning_and_states_no_roaming_setting() {
    let net = FakeNetwork::rehearsal_threema();
    assert_eq!(net.kind(), NetworkKind::Threema);
    let mut want = Flavor::SignalLike.playing_descriptor(NetworkKind::Threema);
    assert!(want.risk_notice.is_some(), "the real thing asks first");
    want.risk_notice = None;
    assert_eq!(
        *net.descriptor(),
        want,
        "Threema's descriptor but for the warning page (the UI's own, tested there)"
    );
    // The warning page is not there to tick, and the plain playing fake still has it.
    let playing = FakeNetwork::playing(NetworkKind::Threema, threema::script(FAST).expect("s"));
    assert!(playing.descriptor().risk_notice.is_some());
    assert_eq!(
        threema::script(Timings::REAL)
            .expect("script")
            .roaming_writes,
        None
    );
}

/// ONE code, scanned at once; the three emoji stand for the script's time on the account clock and
/// nothing goes on a moment short of it; then `linking`, the own ID in `AccountReady` and `done`;
/// the account is on the server. Mutants: two codes; the emoji step skipped; the wait not on the
/// account clock.
#[test]
fn one_code_is_scanned_at_once_the_emoji_stand_and_the_link_ends_as_the_synthetic_own_id() {
    let rt = runtime();
    let clock = FakeClock::new(1_000_000);
    let started = clock.now_ms();
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = FakeNetwork::rehearsal_threema();
    let uuid = AccountUuid::new_random();
    rt.block_on(async {
        let mut run = spawn(
            &fake,
            Arc::new(clock.clone()),
            dir.path(),
            uuid,
            StartMode::Fresh,
            &rt,
        );
        start(&run).await;
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("qr", 1));
        assert!(
            matches!(kind, LoginStepKind::ShowQr { .. }),
            "the code: {kind:?}"
        );
        // The phone scans at once: the next step is the emoji, with the three the fake shows.
        let (id, txn, kind) = step(&mut run).await;
        assert_eq!((id.as_str(), txn), ("emoji", 2), "no second code");
        match kind {
            LoginStepKind::ConfirmEmoji { emoji } => {
                assert_eq!(emoji, FAKE_EMOJI.map(str::to_owned));
            }
            other => panic!("{other:?}"),
        }
        // The account clock holds the emoji for 20 s; a moment short of it nothing is said.
        until_sleeping(&clock).await;
        assert_eq!(clock.deadlines(), [started + 20_000]);
        clock.advance(Duration::from_millis(19_999));
        let early = heard(&mut run).await;
        assert!(early.is_none(), "the phone has not confirmed: {early:?}");
        clock.advance(Duration::from_millis(1));
        let (steps, ids) = until_linked(&mut run).await;
        assert_eq!(steps, ["linking", "done"]);
        assert_eq!(ids, [own()]);
        assert!(
            fake.control().has_device(uuid),
            "the account is on the server"
        );
        stop(run).await;
    });
}

/// The chats come while the account says `Syncing`: the direct chat with its author, the group with
/// its members and title, both with NO history batch and with what the Threema backend states of a
/// chat; the account goes `Online` only after the core acked them. Mutants: no `Syncing` step; no
/// `CatchUpDone`; a history batch for a chat.
#[test]
fn the_chats_arrive_without_history_while_syncing_and_the_account_goes_online_after_the_acks() {
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
            &rt,
        );
        start(&run).await;
        // Read and ack everything until the group's chat; hold THAT envelope's ack.
        let mut all = Vec::new();
        let held = loop {
            let env = tokio::time::timeout(Duration::from_secs(10), run.ends.events.recv())
                .await
                .expect("timely")
                .expect("open");
            let last = env
                .events
                .iter()
                .any(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == group()));
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

        assert!(
            !all.iter().any(|e| matches!(
                e,
                BackendEvent::HistoryBatch { .. } | BackendEvent::Message(_)
            )),
            "Threema pushes no history: {all:?}"
        );
        // The author, then the direct chat, then the group, in the order of the join's second zone.
        let direct = ConvKey::Direct(peer());
        let position = |f: &dyn Fn(&BackendEvent) -> bool| all.iter().position(f).expect("event");
        let chat = |key: ConvKey| {
            position(&move |e| matches!(e, BackendEvent::ConversationUpsert(p) if p.key == key))
        };
        let contact =
            position(&|e| matches!(e, BackendEvent::ContactUpsert(p) if p.user == peer()));
        assert!(contact < chat(direct.clone()));
        assert!(chat(direct) < chat(group()));
        for event in &all {
            let BackendEvent::ConversationUpsert(patch) = event else {
                continue;
            };
            assert_eq!(
                patch.features,
                Some(threema::threema_features()),
                "a chat states what the Threema backend states"
            );
            if patch.key == group() {
                assert_eq!(patch.title, Patch::Set(threema::GROUP_NAME.to_owned()));
                let members = patch.members.as_ref().expect("members");
                let roles: Vec<(UserId, MemberRole)> = members
                    .upsert
                    .iter()
                    .map(|m| (m.user.clone(), m.role))
                    .collect();
                assert_eq!(
                    roles,
                    [(peer(), MemberRole::Admin), (own(), MemberRole::Member)]
                );
            }
        }
        let named = all
            .iter()
            .find_map(|e| match e {
                BackendEvent::ContactUpsert(p) if p.user == peer() => Some(p.clone()),
                _ => None,
            })
            .expect("contact");
        assert_eq!(
            named.profile_name,
            Patch::Set(threema::PEER_NAME.to_owned())
        );
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
fn a_resumed_threema_rehearsal_goes_online_and_replays_nothing() {
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
            &rt,
        );
        let first = link_and_finish(&fake, &mut run, uuid).await;
        let chats = |events: &[BackendEvent]| {
            events
                .iter()
                .filter(|e| matches!(e, BackendEvent::ConversationUpsert(_)))
                .count()
        };
        assert_eq!(chats(&first), 2, "{first:?}");
        stop(run).await;

        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Resume,
            &rt,
        );
        run.ends
            .commands
            .send(Command::Connect)
            .await
            .expect("send");
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
        assert_eq!(chats(&again), 0, "the chats came again: {again:?}");
        assert!(
            !again
                .iter()
                .any(|e| matches!(e, BackendEvent::AccountReady { .. })),
            "not linked again"
        );
        stop(run).await;
    });
}

/// An arrival goes into the first chat of its kind of the RUNNING account as a live unread text
/// of the other member, with the shared words and the id every fake but Telegram's gives. Mutants:
/// the author is the account; a group arrival goes into the direct chat.
#[test]
fn an_arrival_is_a_live_text_of_the_other_member_in_the_first_chat_of_its_kind() {
    let rt = runtime();
    rt.block_on(async {
        let dir = tempfile::tempdir().expect("tempdir");
        let uuid = AccountUuid::new_random();
        let fake = fast();
        let mut run = spawn(
            &fake,
            Arc::new(SystemClock),
            dir.path(),
            uuid,
            StartMode::Fresh,
            &rt,
        );
        link_and_finish(&fake, &mut run, uuid).await;
        let control = fake.control();
        control.arrive(Arrival::Group).expect("a group chat");
        let message = arrived(&mut run).await;
        assert_eq!(message.conv, group());
        control.arrive(Arrival::Direct).expect("a direct chat");
        let message = arrived(&mut run).await;
        assert_eq!(message.conv, ConvKey::Direct(peer()));
        match &message.origin {
            kourier_model::Origin::Remote { author, .. } => assert_eq!(*author, peer()),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            format!("{:?}", message.body),
            format!(
                "{:?}",
                kourier_model::Body::text(format!("{} 2", threema::ARRIVAL_WORDS))
            )
        );
        stop(run).await;
    });
}
