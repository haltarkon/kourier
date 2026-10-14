//! Groups v2 receive against `@signalapp/mock-server` (docs/architecture.md §6.6, §6.8, §11 S10):
//! a group from the phone's Storage Service reaches the core with its title, members and avatar;
//! members' messages arrive in the group conversation with their authors (pairwise and over
//! sender keys, with the distribution message processed first); a member removal produces a
//! system message, also across a kill after the backend commit; messages from non-members and
//! from a group we are not in are dropped; a failing group fetch parks only that group's lane,
//! while a 1:1 message keeps flowing; and the 1:1 retry requests both ways (a forced decryption
//! failure leads to a retry request and the resent message replaces the placeholder; an incoming
//! retry request is answered from the Message Send Log).
//!
//! The backend runs as the app runs it (`SignalNetwork::spawn`); the test plays the core
//! (`support::fake_core`). Skips without Node ≥ 24 or the harness's `node_modules`
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kourier_backend_api::{
    BackendEnv, BackendEvent, Command, Exit, InitialSync, StartMode, TransmitOutcome,
};
use kourier_model::{LoginStepKind, UserId};
use kourier_signal::SignalNetwork;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use support::fake_core::{AccountFiles, FakeCore, Sink, SinkMessage, WAIT, local_env};
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, ModifyGroup, Primary,
    SendGroupText, SendText, StorageAccount, StorageContact, StorageGroup, StorageSpec,
};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// Set (to a JSON [`ChildSpec`]) in the helper process of the kill test.
const CHILD_ENV: &str = "KOURIER_SIGNAL_GROUP_CHILD";
/// Prefix of the helper's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-GROUP-CHILD";
/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(1500);

/// A small avatar (the core stores bytes; it never decodes images here).
const GROUP_AVATAR: &[u8] = b"\x89PNG\r\n\x1a\nClimbing club avatar bytes";

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// Our phone ("Myself"), three members (Ada, Bo, Cal) and a non-member (Dag).
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    ada: Primary,
    bo: Primary,
    cal: Primary,
    dag: Primary,
    sink: Sink,
    files: AccountFiles,
}

impl World {
    async fn start() -> Option<Self> {
        init_tracing();
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = local_env(harness.handshake(), tmp.path()).expect("local env");
        let make = |name: &str| {
            let harness = &harness;
            let name = name.to_owned();
            async move {
                harness
                    .create_primary(&CreatePrimary::named(name))
                    .await
                    .expect("primary")
            }
        };
        let ada = make("Ada").await;
        let bo = make("Bo").await;
        let cal = make("Cal").await;
        let dag = make("Dag").await;
        let phone = harness
            .create_primary(&CreatePrimary {
                contacts: vec![
                    ada.device_ref.clone(),
                    bo.device_ref.clone(),
                    cal.device_ref.clone(),
                ],
                ..CreatePrimary::named("Myself")
            })
            .await
            .expect("phone");
        let sink = Sink::open(&tmp.path().join("sink.db")).expect("sink");
        let files = AccountFiles::new(tmp.path()).expect("account dir");
        Some(Self {
            harness,
            tmp,
            env,
            phone,
            ada,
            bo,
            cal,
            dag,
            sink,
            files,
        })
    }

    fn user(primary: &Primary) -> String {
        format!("aci:{}", primary.aci)
    }

    /// The core's conversation key of `group` (`group:gv2:<base64url group id>`).
    fn conv_key(group: &GroupInfo) -> String {
        let id = STANDARD.decode(&group.id).expect("group id");
        format!("group:gv2:{}", URL_SAFE_NO_PAD.encode(id))
    }

    /// Links a fresh account; the storage state (so a `groups` list is synced) is set first.
    async fn link(&self, storage: &StorageSpec) -> (FakeCore, DeviceInfo) {
        self.harness
            .set_storage_state(storage)
            .await
            .expect("storage state");
        let mut core = FakeCore::start_full(
            SignalNetwork::new(),
            &self.env,
            &self.files,
            StartMode::Fresh,
            &self.sink,
            InitialSync::ContactsAndStorage,
        )
        .expect("spawn the actor");
        let mut device = None;
        let (_url, steps) = core
            .link(|url| {
                let device = &mut device;
                async move {
                    *device = Some(
                        self.harness
                            .complete_provision(&url, &self.phone.device_ref, WAIT)
                            .await
                            .expect("completeProvision"),
                    );
                }
            })
            .await;
        assert!(matches!(
            steps.last().map(|s| &s.kind),
            Some(LoginStepKind::Done)
        ));
        (core, device.expect("linked device"))
    }

    /// The merged `ConversationUpsert` fields for `group` (field by field, in commit order).
    fn upsert(&self, group: &GroupInfo) -> serde_json::Map<String, Value> {
        let key = Self::conv_key(group);
        let mut merged = serde_json::Map::new();
        for patch in self.sink.event_values("ConversationUpsert") {
            if patch["key"] == key.as_str()
                && let Value::Object(fields) = patch
            {
                merged.extend(fields);
            }
        }
        merged
    }

    /// The group's messages in the sink, in commit order.
    fn messages(&self, group: &GroupInfo) -> Vec<SinkMessage> {
        self.sink.messages_in(&Self::conv_key(group))
    }

    /// Waits until a message with `local` (its sent timestamp) is in the group conversation.
    async fn wait_group_message(&self, group: &GroupInfo, author: &str, local: u64) -> SinkMessage {
        let key = Self::conv_key(group);
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Some(message) = self
                .messages(group)
                .into_iter()
                .find(|m| m.local == local.to_string() && m.conv == key && m.author == author)
            {
                return message;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for message {local} in {key}"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// The system-message bodies in the group conversation (`<System>` + the event JSON).
    fn system_messages(&self, group: &GroupInfo) -> Vec<Value> {
        self.messages(group)
            .into_iter()
            .filter_map(|m| m.body.strip_prefix("<System>").map(str::to_owned))
            .filter_map(|json| serde_json::from_str(&json).ok())
            .collect()
    }
}

/// The group members (title, avatar) the done-when checks, created by the phone with Ada and Bo.
async fn make_group(world: &World) -> GroupInfo {
    world
        .harness
        .create_group(&CreateGroup {
            from: world.phone.device_ref.clone(),
            title: "Climbing club".to_owned(),
            members: vec![world.ada.device_ref.clone(), world.bo.device_ref.clone()],
            avatar: Some(STANDARD.encode(GROUP_AVATAR)),
            timer: Some(3600),
            ..CreateGroup::default()
        })
        .await
        .expect("create group")
}

fn storage_with(world: &World, group: &GroupInfo) -> StorageSpec {
    StorageSpec {
        primary: world.phone.device_ref.clone(),
        groups: vec![StorageGroup {
            id: group.id.clone(),
            ..StorageGroup::default()
        }],
        ..StorageSpec::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_reaches_the_core_with_title_members_and_avatar() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let (mut core, _device) = world.link(&storage_with(&world, &group)).await;
    core.wait_online().await;

    // The storage sync queued a group refresh; wait until it applied.
    let deadline = tokio::time::Instant::now() + WAIT;
    let fields = loop {
        let fields = world.upsert(&group);
        if fields.contains_key("title") {
            break fields;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "group never reached the core"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };

    assert_eq!(fields["title"]["Set"], "Climbing club");
    assert_eq!(
        fields["timer"]["Set"]["secs"], 3600,
        "the disappearing timer"
    );
    // Members: us (admin), Ada and Bo.
    let members = fields["members"]["upsert"].as_array().expect("members");
    let mut users: Vec<&str> = members
        .iter()
        .map(|m| m["user"].as_str().unwrap())
        .collect();
    users.sort_unstable();
    let mut expected = [
        World::user(&world.phone),
        World::user(&world.ada),
        World::user(&world.bo),
    ];
    expected.sort();
    assert_eq!(
        users,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(fields["members"]["full"].as_bool().unwrap(), "a full list");
    let creator = members
        .iter()
        .find(|m| m["user"] == World::user(&world.phone).as_str())
        .unwrap();
    assert_eq!(creator["role"], "Admin", "the creator is an administrator");

    // The avatar was downloaded into the account's blob store.
    let avatar = fields["avatar"]["Set"].as_object().expect("avatar set");
    let path = avatar["path"].as_str().expect("avatar path");
    let file = world.files.dir.join("blobs").join(path);
    assert!(file.is_file(), "the avatar file exists at {file:?}");

    // No system message on the first fetch (Desktop drops the "you joined" history).
    assert!(
        world.system_messages(&group).is_empty(),
        "no system messages on the first fetch"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Design §3 S4: the Storage Service's pinned list keeps a group the core has, in the
/// record's order beside a contact. On a fresh link the first sync finds the group before its
/// first fetch (the core does not have it) and leaves it out, as a pin of it would be a bare
/// chat; the group's first fetch then pins it from the stored account record, with NO second
/// account record from the phone (it may not write one for days).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pinned_group_stays_pinned_after_a_storage_sync() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let storage = StorageSpec {
        primary: world.phone.device_ref.clone(),
        contacts: vec![StorageContact {
            pinned: Some(true),
            ..StorageContact::of(&world.ada.device_ref)
        }],
        groups: vec![StorageGroup {
            id: group.id.clone(),
            pinned: Some(true),
            ..StorageGroup::default()
        }],
        account: Some(StorageAccount {
            link_previews: Some(false),
            ..StorageAccount::default()
        }),
        ..StorageSpec::default()
    };
    let (mut core, _device) = world.link(&storage).await;
    core.wait_online().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !world.upsert(&group).contains_key("title") {
        assert!(tokio::time::Instant::now() < deadline, "group fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ada = format!("direct:{}", World::user(&world.ada));
    let key = World::conv_key(&group);
    let pins = world.sink.event_values("PinnedConversations");
    assert_eq!(
        pins.first(),
        Some(&serde_json::json!([ada])),
        "before its first fetch the group is not named"
    );
    // The first fetch of the group pins it, in the record's order.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let pins = world.sink.event_values("PinnedConversations");
        if pins.last() == Some(&serde_json::json!([ada, key])) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group stays pinned after its first fetch: {:?}",
            pins.last().map(|p| p.as_array().map(Vec::len))
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn member_messages_arrive_with_authors_pairwise_and_by_sender_key() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let (mut core, device) = world.link(&storage_with(&world, &group)).await;
    core.wait_online().await;
    // Let the first fetch land so later messages do not trigger another.
    let deadline = tokio::time::Instant::now() + WAIT;
    while !world.upsert(&group).contains_key("title") {
        assert!(tokio::time::Instant::now() < deadline, "group fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // A pairwise group message from Ada.
    let pairwise = world
        .harness
        .send_group_text(&SendGroupText {
            from: world.ada.aci.clone(),
            group: group.id.clone(),
            to: vec![device.device_ref.clone()],
            text: "pairwise hi".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("pairwise group text");
    let message = world
        .wait_group_message(&group, &World::user(&world.ada), pairwise.timestamp)
        .await;
    assert_eq!(message.body, "pairwise hi");
    assert_eq!(message.device, Some(world.ada.device_id));

    // Two sender-key messages from Bo (the distribution message is delivered first).
    for text in ["sender key one", "sender key two"] {
        let sent = world
            .harness
            .send_group_text(&SendGroupText {
                from: world.bo.aci.clone(),
                group: group.id.clone(),
                to: vec![device.device_ref.clone()],
                text: text.to_owned(),
                sender_key: true,
                ..SendGroupText::default()
            })
            .await
            .expect("sender key group text");
        let message = world
            .wait_group_message(&group, &World::user(&world.bo), sent.timestamp)
            .await;
        assert_eq!(message.body, text);
    }

    // Each message exactly once.
    world
        .harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("deliveries settled");
    assert_eq!(
        world
            .messages(&group)
            .iter()
            .filter(|m| !m.body.starts_with("<System>"))
            .count(),
        3,
        "three member messages, no duplicates"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn non_member_and_unknown_group_messages_are_dropped() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let (mut core, device) = world.link(&storage_with(&world, &group)).await;
    core.wait_online().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !world.upsert(&group).contains_key("title") {
        assert!(tokio::time::Instant::now() < deadline, "group fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Dag is not a member: his group message is dropped.
    world
        .harness
        .send_group_text(&SendGroupText {
            from: world.dag.aci.clone(),
            group: group.id.clone(),
            to: vec![device.device_ref.clone()],
            text: "let me in".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("non-member group text");

    // A later message from a member must land, so we know the non-member one was processed.
    let ok = world
        .harness
        .send_group_text(&SendGroupText {
            from: world.ada.aci.clone(),
            group: group.id.clone(),
            to: vec![device.device_ref.clone()],
            text: "members only".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("member group text");
    world
        .wait_group_message(&group, &World::user(&world.ada), ok.timestamp)
        .await;
    world
        .harness
        .wait_for_deliveries(&device.device_ref, WAIT)
        .await
        .expect("deliveries settled");

    assert_eq!(
        world
            .messages(&group)
            .iter()
            .filter(|m| !m.body.starts_with("<System>"))
            .count(),
        1,
        "only the member's message is in the group"
    );
    assert_eq!(
        world.sink.messages_from(&World::user(&world.dag)),
        0,
        "nothing from the non-member anywhere"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_produces_a_system_message() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let (mut core, device) = world.link(&storage_with(&world, &group)).await;
    core.wait_online().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !world.upsert(&group).contains_key("title") {
        assert!(tokio::time::Instant::now() < deadline, "group fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The phone (on another device) removes Ada and sends us the update.
    let changed = world
        .harness
        .modify_group(&ModifyGroup {
            from: world.phone.device_ref.clone(),
            group: group.id.clone(),
            remove_members: vec![world.ada.device_ref.clone()],
            to: vec![device.device_ref.clone()],
            ..ModifyGroup::default()
        })
        .await
        .expect("remove Ada");
    assert_eq!(changed.revision, 1);

    let deadline = tokio::time::Instant::now() + WAIT;
    let removed = loop {
        let events: Vec<Value> = world.system_messages(&group);
        if let Some(removed) = events.iter().find(|e| e.get("MembersRemoved").is_some()) {
            break removed.clone();
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no removal system message; events {events:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let payload = &removed["MembersRemoved"];
    assert_eq!(
        payload["users"][0],
        World::user(&world.ada),
        "Ada was removed"
    );
    // The phone (our own account) removed her: `by` is None (the viewmodel shows "You").
    assert!(payload["by"].is_null(), "removed by us");

    // The member list the core now has no longer includes Ada.
    let members = world.upsert(&group)["members"]["upsert"]
        .as_array()
        .expect("members")
        .iter()
        .map(|m| m["user"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        !members.contains(&World::user(&world.ada)),
        "Ada is no longer a member: {members:?}"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    uuid: String,
    key: Vec<u8>,
    env: BackendEnv,
    sink: PathBuf,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_survives_a_kill_after_the_backend_commit() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let (mut core, device) = world.link(&storage_with(&world, &group)).await;
    core.wait_online().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !world.upsert(&group).contains_key("title") {
        assert!(tokio::time::Instant::now() < deadline, "group fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // The phone removes Bo while nobody is connected; the update waits in the server's queue.
    world
        .harness
        .modify_group(&ModifyGroup {
            from: world.phone.device_ref.clone(),
            group: group.id.clone(),
            remove_members: vec![world.bo.device_ref.clone()],
            to: vec![device.device_ref.clone()],
            ..ModifyGroup::default()
        })
        .await
        .expect("remove Bo");

    // The next start processes it in a helper armed to abort right after the stage-2 zone that
    // applied the group and its system messages committed (events still in core_outbox).
    let child = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
    };
    let failpoint = format!(
        "{}:1",
        kourier_signal::failpoints::AFTER_BACKEND_EFFECTS_COMMIT
    );
    let mut process = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "child_actor_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, serde_json::to_string(&child).expect("spec"))
        .env("KOURIER_FAILPOINT", &failpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the helper process");
    let mut lines = BufReader::new(process.stdout.take().expect("stdout")).lines();
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    let status = tokio::time::timeout(WAIT, process.wait())
        .await
        .expect("the helper process ended")
        .expect("helper status");
    assert_eq!(
        status.signal(),
        Some(6),
        "the helper aborted at {failpoint} ({status})"
    );
    // The backend committed the new revision but the core has not been told yet.
    assert!(
        world.files.count_rows("core_outbox").expect("outbox") > 0,
        "the events wait in core_outbox"
    );

    // Restart: the forwarder ships the events the dead process committed; the system message
    // is not lost.
    let mut core =
        FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("start");
    core.send(Command::Connect).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let events = world.system_messages(&group);
        if events.iter().any(|e| {
            e.get("MembersRemoved")
                .is_some_and(|r| r["users"][0] == World::user(&world.bo))
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the removal system message was lost across the kill"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(world.files.count_rows("core_outbox").expect("outbox"), 0);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Link feedback L3: a fresh link reports the groups the server has not given yet (the link's
/// `Done` does not wait for them), and once the group's state arrived, a last report says
/// `finished`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_link_reports_its_groups_still_loading_and_then_that_it_finished() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    world
        .harness
        .fail_group_fetch(&group.id, true)
        .await
        .expect("fail group fetch");
    let (mut core, _device) = world.link(&storage_with(&world, &group)).await;
    let mut seen = core.progress_seen.clone();
    let deadline = tokio::time::Instant::now() + WAIT;
    while !seen.iter().any(|p| p.groups_left == 1) {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::AccountProgress(p)) => {
                assert!(!p.finished, "finished while the group still loads");
                seen.push(p);
            }
            Some(_) => {}
            None => panic!("no report of the group still loading: {seen:?}"),
        }
    }
    world
        .harness
        .fail_group_fetch(&group.id, false)
        .await
        .expect("unfail group fetch");
    let deadline = tokio::time::Instant::now() + 2 * WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::AccountProgress(p)) if p.finished => break,
            Some(_) => {}
            None => panic!("no finished report after the group was fetched"),
        }
    }
    assert!(
        world.upsert(&group).contains_key("title"),
        "the group was fetched"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Link feedback L3 (F3, F5): a start that finds a group still without its state reports it
/// as the link's tail within a day of the link, and says nothing for an older account (a
/// group the server never gives is no news every day: no banner, no question on quitting).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_reports_a_stranded_group_only_within_a_day_of_the_link() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    world
        .harness
        .fail_group_fetch(&group.id, true)
        .await
        .expect("fail group fetch");
    let (core, _device) = world.link(&storage_with(&world, &group)).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    // The link was cut off with the group still loading (the core has it).
    world
        .files
        .execute("UPDATE groups SET features_hash = COALESCE(features_hash, 42)")
        .expect("the core has the group");
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM groups WHERE state IS NULL")
            .expect("groups"),
        Some(1),
        "the group waits for its state"
    );

    // Just linked: the start reports the group again.
    let mut core =
        FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("start");
    core.send(Command::Connect).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::AccountProgress(p)) if p.groups_left == 1 => {
                assert!(!p.finished);
                break;
            }
            Some(_) => {}
            None => panic!("a start within a day of the link did not report the group"),
        }
    }
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // A month later the same group is old news: nothing is reported, the refresh goes on.
    world
        .files
        .execute("UPDATE account SET linked_at = linked_at - 2592000000")
        .expect("an old account");
    let mut core =
        FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("start");
    core.send(Command::Connect).await;
    core.wait_online().await;
    while let Some(event) = core.next_ephemeral(Duration::from_secs(2)).await {
        assert!(
            !matches!(event, BackendEvent::AccountProgress(_)),
            "an old account's stranded group was reported: {event:?}"
        );
    }
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_group_fetch_parks_only_that_group() {
    let Some(world) = World::start().await else {
        return;
    };
    let group = make_group(&world).await;
    let (mut core, device) = world.link(&storage_with(&world, &group)).await;
    core.wait_online().await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !world.upsert(&group).contains_key("title") {
        assert!(tokio::time::Instant::now() < deadline, "group fetched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The phone adds Cal (a member our backend does not yet know) and sends us the update as a
    // transcript, bumping the revision; then every group fetch fails.
    world
        .harness
        .modify_group(&ModifyGroup {
            from: world.phone.device_ref.clone(),
            group: group.id.clone(),
            add_members: vec![world.cal.device_ref.clone()],
            ..ModifyGroup::default()
        })
        .await
        .expect("add Cal");
    world
        .harness
        .fail_group_fetch(&group.id, true)
        .await
        .expect("fail group fetch");

    // Cal — a member only under the new revision — sends a group message. Our backend must fetch
    // (Cal is not a member under the last known state), the fetch fails, and the lane parks.
    let parked = world
        .harness
        .send_group_text(&SendGroupText {
            from: world.cal.aci.clone(),
            group: group.id.clone(),
            to: vec![device.device_ref.clone()],
            text: "from the new member".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("parked group text");

    // A 1:1 message from Ada keeps flowing while the group lane is parked.
    let direct = world
        .harness
        .send_text(&SendText {
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&world.ada.aci, &device.device_ref, "direct while parked")
        })
        .await
        .expect("direct text");
    world
        .sink
        .wait_message(
            &World::user(&world.ada),
            &direct.timestamp.to_string(),
            WAIT,
        )
        .await
        .expect("the 1:1 message reached the core while the group was parked");

    // The parked group message has not been applied.
    tokio::time::sleep(QUIET).await;
    assert!(
        world
            .messages(&group)
            .iter()
            .all(|m| m.local != parked.timestamp.to_string()),
        "the group message is parked, not applied"
    );

    // Once the fetch works again, the parked lane catches up.
    world
        .harness
        .fail_group_fetch(&group.id, false)
        .await
        .expect("unfail group fetch");
    world
        .wait_group_message(&group, &World::user(&world.cal), parked.timestamp)
        .await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_decryption_failure_leads_to_a_retry_request_and_the_resend_replaces_the_placeholder() {
    let Some(world) = World::start().await else {
        return;
    };
    // No group needed: a 1:1 decryption failure. Storage with no groups.
    let storage = StorageSpec {
        primary: world.phone.device_ref.clone(),
        ..StorageSpec::default()
    };
    let (mut core, device) = world.link(&storage).await;
    core.wait_online().await;

    // Ada sends us a message we cannot decrypt, with a Resendable hint and a 1:1 context.
    let timestamp = 1_800_000_000_000;
    world
        .harness
        .send_undecryptable(
            &world.ada.aci,
            &device.device_ref,
            "the lost message",
            "resendable",
            timestamp,
        )
        .await
        .expect("sendUndecryptable");

    // A placeholder with the would-be net id (Ada, timestamp) appears in the 1:1 conversation.
    let author = World::user(&world.ada);
    let placeholder = world
        .sink
        .wait_message(&author, &timestamp.to_string(), WAIT)
        .await
        .expect("the undecryptable message left a placeholder");
    assert_eq!(
        placeholder.conv,
        format!("direct:{author}"),
        "placeholder in the 1:1 chat"
    );
    assert!(
        placeholder.body.starts_with("<Placeholder>"),
        "an Undecryptable placeholder: {}",
        placeholder.body
    );
    assert!(
        placeholder.body.contains("\"resendable\":true"),
        "resendable: {}",
        placeholder.body
    );

    // Kourier sent Ada a retry request for that message.
    let request = world
        .harness
        .wait_for_decryption_error(&world.ada.aci, None, WAIT)
        .await
        .expect("Ada received a retry request");
    assert_eq!(request.timestamp, timestamp);
    // The request names the device the undecryptable message was sent from (Ada's).
    assert_eq!(request.sender_device, world.ada.device_id);

    // Ada "resends": a normal message with the same timestamp. It replaces the placeholder.
    world
        .harness
        .send_text(&SendText {
            await_delivery: true,
            timeout: Some(60_000),
            timestamp: Some(timestamp),
            ..SendText::new(&world.ada.aci, &device.device_ref, "the lost message")
        })
        .await
        .expect("resend");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let message = world
            .sink
            .message(&author, &timestamp.to_string())
            .expect("message row");
        if message.body == "the lost message" {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the resend never replaced the placeholder (still {})",
            message.body
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The net id was delivered twice (the placeholder, then the resend that replaced it) and
    // left a single row.
    let deliveries = world.sink.deliveries(&author, &timestamp.to_string());
    assert!(deliveries >= 1, "the net id was delivered ({deliveries})");
    assert_eq!(
        world
            .sink
            .messages_in(&format!("direct:{author}"))
            .iter()
            .filter(|m| m.local == timestamp.to_string())
            .count(),
        1,
        "a single row for the net id"
    );
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_incoming_retry_request_is_answered_from_the_message_send_log() {
    let Some(world) = World::start().await else {
        return;
    };
    let storage = StorageSpec {
        primary: world.phone.device_ref.clone(),
        ..StorageSpec::default()
    };
    let (mut core, device) = world.link(&storage).await;
    core.wait_online().await;

    // Our phone accepts the chat with Ada so Kourier may send (§6.7 M1 rule), then Kourier sends
    // Ada a 1:1 text (logged in the Message Send Log).
    world
        .harness
        .send_message_request_response(
            &world.phone.device_ref,
            &device.device_ref,
            &world.ada.aci,
            "accept",
        )
        .await
        .expect("accept Ada");
    world
        .files
        .wait_for_i64(
            "chat accepted",
            &format!(
                "SELECT profile_sharing FROM peers WHERE service_id = '{}'",
                world.ada.aci
            ),
            |v| v == Some(1),
        )
        .await;
    let (_sent_at, outcome) = core
        .transmit_text(
            &UserId::parse(&format!("aci:{}", world.ada.aci)).expect("user"),
            "hello Ada",
        )
        .await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "the message was sent: {outcome:?}"
    );
    let sent = world
        .harness
        .wait_for_message(&world.ada.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Ada got the message");
    assert_eq!(sent.text, "hello Ada");
    let timestamp = sent.timestamp;

    // The message is in the Message Send Log.
    world
        .files
        .wait_for_i64(
            "logged in the MSL",
            &format!("SELECT count(*) FROM msl_payloads WHERE timestamp = {timestamp}"),
            |v| v.is_some_and(|n| n >= 1),
        )
        .await;

    // Ada could not decrypt it and asks for a resend.
    world
        .harness
        .send_retry_request(
            &world.ada.aci,
            &device.device_ref,
            timestamp,
            device.device_id,
        )
        .await
        .expect("sendRetryRequest");

    // Kourier resends the logged content; Ada receives "hello Ada" again at the same timestamp.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if let Ok(again) = world
            .harness
            .wait_for_message(&world.ada.aci, Some(&device.device_ref), QUIET)
            .await
        {
            assert_eq!(again.text, "hello Ada");
            assert_eq!(
                again.timestamp, timestamp,
                "resent at the original timestamp"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the retry request was not answered from the send log"
        );
    }
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The helper process of the kill test.
#[test]
fn child_actor_process() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
    init_tracing();
    let spec: ChildSpec = serde_json::from_str(&spec).expect("child spec");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async move {
        let files = AccountFiles {
            dir: spec.dir,
            uuid: spec.uuid.parse().expect("uuid"),
            key: spec.key.try_into().expect("32-byte key"),
        };
        let sink = Sink::open(&spec.sink).expect("child sink");
        let core = FakeCore::start(&spec.env, &files, StartMode::Resume, &sink).expect("spawn");
        core.send(Command::Connect).await;
        tokio::time::sleep(Duration::from_secs(120)).await;
        println!("{CHILD_LINE} the failpoint did not fire");
        let _ = core.shutdown().await;
    });
}
