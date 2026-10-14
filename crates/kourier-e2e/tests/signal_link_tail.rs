//! The link's tail with a big history transfer, measured (link feedback L2, 2026-10-12):
//! the phases of a fresh link until the login's `Done`, with the app's own initial sync
//! (`InitialSync::ContactsAndStorage`): people with profiles (and avatars) on the mock, groups
//! on the mock, the phone's Storage Service and contact sync, an archive that names them all.
//! Optionally through a TCP proxy that adds a round-trip time (`TAIL_RTT_MS`) to everything
//! the app sends to the mock. Not run by CI (`#[ignore]`: ~15 s of release time, ~10 s of it
//! the mock creating groups).
//!
//! It asserts that `Done` comes within `TAIL_BUDGET_MS` (5,000 by default) of the transfer's
//! end. Before L2 the first Storage Service sync's one `IdentityMerged` per numbered contact
//! walked all messages per merge and the gate waited for the whole job queue: 11 s at the
//! defaults (300 numbered contacts, 20,000 messages), 39 s with 60,000 messages, 19 s at a
//! 150 ms round trip; after it 0.1, 0.1 and 0.3 s.
//!
//! Run: TAIL_LOG=<file> [TAIL_RTT_MS=150] [TAIL_MESSAGES=60000] [TAIL_NO_E164=1]
//!   KOURIER_REQUIRE_MOCK_HARNESS=1 cargo test --release -p kourier-e2e --test signal_link_tail
//!   -- --ignored --nocapture --test-threads 1

mod support;

use std::io::{Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendConfig, BackendEnv, InitialSync, NetworkRegistry, SystemClock};
use kourier_core::facade::{ConversationListQuery, Query, QueryData, UiCommand, UiEvent};
use kourier_core::testing::DirVault;
use kourier_core::{Core, CoreConfig};
use kourier_model::{AccountId, HistoryTransfer, LoginStepKind, NetworkKind, Redacted};
use kourier_signal::SignalNetwork;
use kourier_signal::archive_testing::{
    ChatSpec, ChatWith, ContactSpec, GroupSpec, HistorySpec, Party, SnapshotSpec, TextSpec, Who,
    write_history_archive,
};
use support::harness::{CreateGroup, CreatePrimary, StorageContact, StorageGroup, StorageSpec};
use support::{CORE_KEY, World};

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("ms")
}

/// A 1x1 PNG.
const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// The log file, shared by the subscriber and the marks.
#[derive(Clone)]
struct SharedFile(Arc<Mutex<std::fs::File>>);

impl std::io::Write for SharedFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// The timeline of the run: (seconds since the start, what).
#[derive(Clone)]
struct Marks(Arc<Mutex<Vec<(f64, String)>>>, SharedFile);

impl Marks {
    fn mark(&self, t0: Instant, what: impl Into<String>) {
        let what = what.into();
        let at = t0.elapsed().as_secs_f64();
        let _ = writeln!(self.1.clone(), "{at:.6}s MARK {what}");
        self.0.lock().unwrap().push((at, what));
    }
}

/// Forwards one direction of a connection with a one-way delay of `delay`: every chunk read
/// is written `delay` after it was read, in order.
fn pipe(mut from: TcpStream, mut to: TcpStream, delay: Duration) {
    let (tx, rx) = std::sync::mpsc::channel::<(Instant, Vec<u8>)>();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx
                        .send((Instant::now() + delay, buf[..n].to_vec()))
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    for (due, chunk) in rx {
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        if to.write_all(&chunk).is_err() {
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
    let _ = reader.join();
}

/// A TCP proxy on 127.0.0.1 to `target` that adds `rtt` (half each way). Returns its port.
fn latency_proxy(target: u16, rtt: Duration) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("proxy bind");
    let port = listener.local_addr().expect("addr").port();
    let one_way = rtt / 2;
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(client) = conn else { continue };
            let Ok(server) = TcpStream::connect(("127.0.0.1", target)) else {
                continue;
            };
            let _ = client.set_nodelay(true);
            let _ = server.set_nodelay(true);
            let (c2, s2) = (
                client.try_clone().expect("clone"),
                server.try_clone().expect("clone"),
            );
            std::thread::spawn(move || pipe(client, server, one_way));
            std::thread::spawn(move || pipe(s2, c2, one_way));
        }
    });
    port
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement (release build, ~15 s): see the file's header"]
async fn tail_of_a_big_link() {
    let log_path = std::env::var("TAIL_LOG").unwrap_or_else(|_| "/dev/null".to_owned());
    let file = SharedFile(Arc::new(Mutex::new(
        std::fs::File::create(&log_path).expect("log file"),
    )));
    let t0 = Instant::now();
    let marks = Marks(Arc::default(), file.clone());
    let writer = file.clone();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            std::env::var("TAIL_FILTER").unwrap_or_else(|_| {
                "warn,tail=info,kourier_signal=info,kourier_core=info".to_owned()
            }),
        ))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .try_init();
    let people_n = env_num("TAIL_PEOPLE", 300) as usize;
    let groups_n = env_num("TAIL_GROUPS", 50) as usize;
    let members_n = env_num("TAIL_MEMBERS", 30) as usize;
    let messages_n = env_num("TAIL_MESSAGES", 20_000) as usize;
    let rtt = Duration::from_millis(env_num("TAIL_RTT_MS", 0));
    let extra_storage = env_num("TAIL_EXTRA_STORAGE", 0) as u32;

    let Some(world) = World::start().await else {
        return;
    };
    marks.mark(t0, format!(
        "setup begins: people {people_n}, groups {groups_n} x {members_n}, messages {messages_n}, rtt {} ms, extra storage contacts {extra_storage}",
        rtt.as_millis()
    ));

    // People with profiles (2 of 3 with an avatar) on the mock.
    let mut people = Vec::with_capacity(people_n);
    for i in 0..people_n {
        let spec = CreatePrimary {
            avatar: (i % 3 != 2).then(|| PNG_B64.to_owned()),
            has_e164: (env_num("TAIL_NO_E164", 0) == 1).then_some(false),
            ..CreatePrimary::named(format!("Person {i}"))
        };
        people.push(world.harness.create_primary(&spec).await.expect("person"));
    }
    marks.mark(t0, "people created");
    // Our phone, with all of them in its contact sync.
    let me = world
        .harness
        .create_primary(&CreatePrimary {
            contacts: people.iter().map(|p| p.device_ref.clone()).collect(),
            ..CreatePrimary::named("Me")
        })
        .await
        .expect("me");
    // Groups the phone created, each with `members_n` people.
    let mut groups = Vec::with_capacity(groups_n);
    let mut group_members: Vec<Vec<usize>> = Vec::with_capacity(groups_n);
    for g in 0..groups_n {
        let members: Vec<usize> = (0..members_n.min(people_n))
            .map(|k| (g * 7 + k * 11) % people_n)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let group = world
            .harness
            .create_group(&CreateGroup {
                from: me.device_ref.clone(),
                title: format!("Group {g}"),
                members: members
                    .iter()
                    .map(|&i| people[i].device_ref.clone())
                    .collect(),
                ..CreateGroup::default()
            })
            .await
            .expect("group");
        groups.push(group);
        group_members.push(members);
    }
    marks.mark(t0, "groups created");
    // The phone's Storage Service: every person and group.
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: me.device_ref.clone(),
            contacts: people
                .iter()
                .map(|p| StorageContact::of(p.device_ref.clone()))
                .collect(),
            groups: groups
                .iter()
                .map(|g| StorageGroup {
                    id: g.id.clone(),
                    ..StorageGroup::default()
                })
                .collect(),
            extra_contacts: (extra_storage > 0).then_some(extra_storage),
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    marks.mark(t0, "storage state set");

    // The archive.
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let base = now_ms() - 30 * 24 * 3_600_000;
    let contacts: Vec<ContactSpec> = people
        .iter()
        .map(|p| ContactSpec {
            aci: Some(p.aci.parse().expect("ACI")),
            profile_name: format!("Archived {}", p.profile_name),
            profile_sharing: true,
            profile_key: Some(
                b64.decode(&p.profile_key)
                    .expect("key")
                    .try_into()
                    .expect("32"),
            ),
            ..ContactSpec::default()
        })
        .collect();
    let group_specs: Vec<GroupSpec> = groups
        .iter()
        .zip(&group_members)
        .map(|(g, members)| GroupSpec {
            master_key: b64.decode(&g.master_key).expect("master key"),
            whitelisted: true,
            blocked: false,
            snapshot: Some(SnapshotSpec {
                title: Some(g.title.clone()),
                members: std::iter::once((Party::Own, true))
                    .chain(members.iter().map(|&i| (Party::Contact(i), false)))
                    .collect(),
                ..SnapshotSpec::default()
            }),
        })
        .collect();
    let direct_share = if groups_n == 0 {
        messages_n
    } else {
        messages_n * 3 / 4
    };
    let per_direct = (direct_share / people_n.max(1)).max(1);
    let per_group = (messages_n - direct_share)
        .checked_div(groups_n)
        .unwrap_or(0);
    let mut sent = base;
    let mut chats = Vec::new();
    for i in 0..people_n {
        let texts = (0..per_direct)
            .map(|k| {
                sent += 1000;
                TextSpec {
                    outgoing: k % 3 == 0,
                    body: format!("Message {k} of a chat, some sixty characters of plain text."),
                    sent_at: sent,
                    received_at: sent + 2,
                    read: true,
                    statuses: Vec::new(),
                    ..TextSpec::default()
                }
            })
            .collect();
        chats.push(ChatSpec {
            with: ChatWith::Contact(i),
            texts,
            ..ChatSpec::default()
        });
    }
    for (g, members) in group_members.iter().enumerate() {
        let texts = (0..per_group)
            .map(|k| {
                sent += 1000;
                TextSpec {
                    author: Some(Who::Contact(members[k % members.len()])),
                    body: format!("Group message {k}, some sixty characters of plain text."),
                    sent_at: sent,
                    received_at: sent + 2,
                    read: true,
                    ..TextSpec::default()
                }
            })
            .collect();
        chats.push(ChatSpec {
            with: ChatWith::Group(g),
            texts,
            ..ChatSpec::default()
        });
    }
    let spec = HistorySpec {
        contacts,
        groups: group_specs,
        chats,
        ..HistorySpec::default()
    };
    marks.mark(t0, "archive spec built");

    // The app, as the real one: transfer offered, contacts and storage before Done.
    let env = match &world.env {
        BackendEnv::LocalTest(local) if !rtt.is_zero() => {
            let mut local = local.clone();
            local.port = latency_proxy(local.port, rtt);
            BackendEnv::LocalTest(local)
        }
        other => other.clone(),
    };
    let data = world.tmp.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let network = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(600), Duration::from_secs(1));
    let networks = NetworkRegistry::new()
        .with(Arc::new(network))
        .expect("registry");
    let mut cfg = CoreConfig::new(
        data.join("core.db"),
        kourier_db::DbKey::from(CORE_KEY),
        networks,
        Arc::new(DirVault::new(&data)),
        Arc::new(SystemClock),
        tokio::runtime::Handle::current(),
    );
    cfg.backend = BackendConfig {
        env,
        initial_sync: InitialSync::ContactsAndStorage,
        ..BackendConfig::default()
    };
    let core = Core::start(cfg).await.expect("core starts");
    let handle = core.handle();
    let mut events = handle.subscribe();
    handle.send(UiCommand::UiReady).expect("UiReady");

    // Begin the link the way the link page does.
    handle
        .send(UiCommand::AddAccount {
            network: NetworkKind::Signal,
            flow: "qr".to_owned(),
        })
        .expect("add");
    let mut account: Option<AccountId> = None;
    let url = loop {
        match tokio::time::timeout(Duration::from_secs(60), events.recv())
            .await
            .expect("link events")
        {
            Ok(UiEvent::AccountAdded { account: a }) => account = Some(a),
            Ok(UiEvent::LoginStep { account: a, step }) if Some(a) == account => match &step.kind {
                LoginStepKind::Input { fields } => handle
                    .send(UiCommand::SubmitLoginInput {
                        account: a,
                        txn: step.txn,
                        values: vec![(
                            fields[0].id.clone(),
                            Redacted::new("Kourier tail".to_owned()),
                        )],
                    })
                    .expect("input"),
                LoginStepKind::ShowQr { data, .. } => break data.expose().clone(),
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            _ => {}
        }
    };
    let account = account.expect("account");
    let key = world
        .harness
        .set_ephemeral_backup_key(&me.device_ref)
        .await
        .expect("setEphemeralBackupKey");

    // Follow the UI events from here, on a task of their own.
    let follower = {
        let marks = marks.clone();
        let mut events = events;
        tokio::spawn(async move {
            let mut progress = 0usize;
            let mut chat_ends = 0usize;
            let mut seen_states = std::collections::BTreeSet::new();
            loop {
                match events.recv().await {
                    Ok(UiEvent::LoginStep { account: a, step }) if a == account => {
                        match &step.kind {
                            LoginStepKind::Done => {
                                marks.mark(t0, "UI: login Done");
                                return (progress, chat_ends);
                            }
                            LoginStepKind::Progress { label, fraction } => {
                                marks.mark(t0, format!("UI: login progress {label:?} {fraction:?}"))
                            }
                            LoginStepKind::Failed { error } => {
                                marks.mark(t0, format!("UI: login FAILED {error:?}"));
                                return (progress, chat_ends);
                            }
                            other => marks.mark(t0, format!("UI: login step {other:?}")),
                        }
                    }
                    Ok(UiEvent::HistoryTransfer { account: a, state }) if a == account => {
                        let name = match state {
                            HistoryTransfer::Downloading { .. } => "Downloading".to_owned(),
                            other => format!("{other:?}"),
                        };
                        if seen_states.insert(name.clone()) {
                            marks.mark(t0, format!("UI: transfer {name}"));
                        }
                    }
                    Ok(UiEvent::HistoryProgress { has_more, .. }) => {
                        if progress == 0 {
                            marks.mark(t0, "UI: first HistoryProgress");
                        }
                        progress += 1;
                        chat_ends += usize::from(!has_more);
                    }
                    Ok(UiEvent::ConnState { account: a, state }) if a == account => {
                        marks.mark(t0, format!("UI: conn {state:?}"));
                    }
                    Ok(UiEvent::CatchUpSummary {
                        messages,
                        conversations,
                        ..
                    }) => marks.mark(
                        t0,
                        format!("UI: catch-up summary {messages} msgs / {conversations} chats"),
                    ),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        marks.mark(t0, format!("UI: lagged {n}"));
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        return (progress, chat_ends);
                    }
                }
            }
        })
    };
    // How many chats the core lists, once a second.
    let sampler = {
        let marks = marks.clone();
        let handle = handle.clone();
        let total = people_n + groups_n;
        tokio::spawn(async move {
            let mut last = usize::MAX;
            loop {
                let query = ConversationListQuery {
                    account: Some(account),
                    limit: 2000,
                    ..ConversationListQuery::default()
                };
                if let Ok(QueryData::Conversations(page)) =
                    handle.query(Query::Conversations(query)).await
                {
                    let n = page.rows.len();
                    if n != last {
                        marks.mark(t0, format!("core lists {n} chats (of {total})"));
                        last = n;
                    }
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        })
    };

    marks.mark(t0, "phone scans");
    let device = world
        .harness
        .complete_provision(&url, &me.device_ref, Duration::from_secs(120))
        .await
        .expect("completeProvision");
    assert!(device.sent_transfer_key, "the phone sent its transfer key");
    marks.mark(t0, "phone: provisioned");
    let path = world.tmp.path().join("phone-archive");
    let bytes = write_history_archive(&path, &key, device.aci.parse().expect("ACI"), &spec)
        .expect("archive");
    marks.mark(t0, format!("phone: archive written, {bytes} bytes"));
    let place = world
        .harness
        .store_transfer_archive(&path)
        .await
        .expect("storeTransferArchive");
    world
        .harness
        .provide_transfer_archive(&device.device_ref, &place)
        .await
        .expect("provideTransferArchive");
    marks.mark(t0, "phone: archive uploaded");

    let (progress, chat_ends) = tokio::time::timeout(Duration::from_secs(3600), follower)
        .await
        .expect("Done within an hour")
        .expect("follower");
    marks.mark(
        t0,
        format!("history progress events {progress}, chat ends {chat_ends}"),
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    sampler.abort();
    core.shutdown().await;
    marks.mark(t0, "core stopped");
    println!("TIMELINE");
    for (at, what) in marks.0.lock().unwrap().iter() {
        println!("{at:9.2}  {what}");
    }
    let at = |what: &str| {
        marks
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|(_, w)| w == what)
            .map(|(at, _)| *at)
    };
    let transfer_done = at("UI: transfer Done").expect("the transfer ended");
    let login_done = at("UI: login Done").expect("the login ended");
    drop(world.tmp);
    world.harness.shutdown().await.expect("harness shutdown");
    let budget = env_num("TAIL_BUDGET_MS", 5_000) as f64 / 1000.0;
    let tail = login_done - transfer_done;
    println!("TAIL {tail:.2} s after the transfer's end (budget {budget:.2} s)");
    assert!(
        tail <= budget,
        "the login ended {tail:.2} s after the transfer (budget {budget:.2} s): the post-link \
         gate waits for something it does not need (jobs, the Storage Service sync, the core's \
         merges)"
    );
}
