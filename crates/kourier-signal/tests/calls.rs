//! Calls against the mock Signal server: every signal a device that takes no calls gets for one
//! becomes exactly the call line the contract names (`IncomingMessage::call`), and Kourier sends
//! NOTHING for it.
//!
//! The account actor runs under the fake core of `support`; what the backend reports lands in
//! the sink. The other side is the mock's phone ("Myself"), three contacts who are also the
//! members of one group, and a stranger nobody accepted.
//!
//! "Sends nothing" is measured at the server ([`World::watch`], [`World::silent_since`]): the
//! number of requests any client made to send a message to anybody (the 1:1 route that carries
//! messages, receipts, typing, call signalling and the sync messages to our own account, and
//! every multi-recipient send) is the same before a call signal and after the backend handled
//! it and a quiet time passed; in `backend.db` no job and no owed receipt was queued. Kourier
//! is the only client of the mock in these tests. A control at the end shows the same measure
//! does see a delivery receipt.
//!
//! Skipped (with a notice) when the mock harness is not installed; with
//! `KOURIER_REQUIRE_MOCK_HARNESS=1` a missing harness is a failure.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    dead_code,
    reason = "the shared support modules serve several test files"
)]

mod support;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kourier_backend_api::{BackendEnv, Command, Exit, InitialSync, StartMode};
use kourier_model::{ConvKey, LoginStepKind, UserId};
use kourier_signal::SignalNetwork;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT};
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, Primary, StorageContact,
    StorageGroup, StorageSpec,
};

/// How long the server is watched for a send after a call signal was handled. An owed
/// delivery receipt goes out 500 ms after it is queued; a reply to a call would go out at once.
const QUIET: Duration = Duration::from_millis(1500);

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// What the server and `backend.db` showed before a call signal.
struct Watch {
    /// Requests to send a message that clients made so far.
    send_requests: u64,
    /// The newest job.
    last_job: i64,
}

struct World {
    harness: Harness,
    _tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    members: [Primary; 3],
    stranger: Primary,
    group: GroupInfo,
    sink: Sink,
    files: AccountFiles,
}

impl World {
    async fn start() -> Option<Self> {
        init_tracing();
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = support::fake_core::local_env(harness.handshake(), tmp.path()).expect("env");
        let mut members = Vec::new();
        for name in ["Ada", "Bo", "Cal"] {
            members.push(
                harness
                    .create_primary(&CreatePrimary::named(name))
                    .await
                    .expect("member"),
            );
        }
        let members: [Primary; 3] = members.try_into().expect("three members");
        let stranger = harness
            .create_primary(&CreatePrimary::named("Dan"))
            .await
            .expect("stranger");
        let phone = harness
            .create_primary(&CreatePrimary {
                contacts: members.iter().map(|m| m.device_ref.clone()).collect(),
                ..CreatePrimary::named("Myself")
            })
            .await
            .expect("phone");
        let group = harness
            .create_group(&CreateGroup {
                from: phone.device_ref.clone(),
                title: "Climbing club".to_owned(),
                members: members.iter().map(|m| m.device_ref.clone()).collect(),
                ..CreateGroup::default()
            })
            .await
            .expect("create group");
        let sink = Sink::open(&tmp.path().join("sink.db")).expect("sink");
        let files = AccountFiles::new(tmp.path()).expect("account dir");
        Some(Self {
            harness,
            _tmp: tmp,
            env,
            phone,
            members,
            stranger,
            group,
            sink,
            files,
        })
    }

    fn ada(&self) -> &Primary {
        &self.members[0]
    }

    fn bo(&self) -> &Primary {
        &self.members[1]
    }

    fn group_conv(&self) -> Value {
        let id = STANDARD.decode(&self.group.id).expect("group id");
        key_json(&ConvKey::Group(
            format!("gv2:{}", URL_SAFE_NO_PAD.encode(id)).into(),
        ))
    }

    fn user(primary: &Primary) -> String {
        format!("aci:{}", primary.aci)
    }

    fn direct(primary: &Primary) -> Value {
        key_json(&ConvKey::Direct(
            UserId::parse(&Self::user(primary)).expect("user id"),
        ))
    }

    /// Links a fresh account whose phone has the group and its members as accepted contacts.
    async fn link(&self) -> (FakeCore, DeviceInfo) {
        self.harness
            .set_storage_state(&StorageSpec {
                primary: self.phone.device_ref.clone(),
                contacts: self
                    .members
                    .iter()
                    .map(|m| StorageContact::of(m.device_ref.clone()))
                    .collect(),
                groups: vec![StorageGroup {
                    id: self.group.id.clone(),
                    ..StorageGroup::default()
                }],
                ..StorageSpec::default()
            })
            .await
            .expect("storage state");
        let mut core = self.actor(StartMode::Fresh);
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
        core.wait_online().await;
        self.files
            .wait_for_i64(
                "the group's state",
                "SELECT count(*) FROM groups WHERE revision IS NOT NULL
                   AND endorsements_expire_at IS NOT NULL AND membership = 'member'",
                |v| v == Some(1),
            )
            .await;
        self.files
            .wait_for_i64(
                "accepted contacts",
                "SELECT count(*) FROM peers WHERE profile_sharing = 1 OR system_contact = 1",
                |v| v.is_some_and(|n| n >= 3),
            )
            .await;
        (core, device.expect("linked device"))
    }

    fn actor(&self, mode: StartMode) -> FakeCore {
        FakeCore::start_full(
            SignalNetwork::new(),
            &self.env,
            &self.files,
            mode,
            &self.sink,
            InitialSync::ContactsAndStorage,
        )
        .expect("spawn the actor")
    }

    async fn send_requests(&self) -> u64 {
        self.harness
            .send_stats()
            .await
            .expect("sendStats")
            .send_requests
    }

    fn sql(&self, sql: &str) -> i64 {
        self.files.peek_i64(sql).expect("backend.db").unwrap_or(0)
    }

    /// Waits until the backend has stopped sending (what linking and earlier steps left to
    /// do) and has handled everything it received, then notes what the server has seen.
    async fn watch(&self) -> Watch {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let before = self.send_requests().await;
            tokio::time::sleep(Duration::from_millis(700)).await;
            let idle = self.sql("SELECT count(*) FROM inbox") == 0
                && self.sql("SELECT count(*) FROM delivery_receipts") == 0;
            if idle && self.send_requests().await == before {
                return Watch {
                    send_requests: before,
                    last_job: self.sql("SELECT coalesce(max(id), 0) FROM jobs"),
                };
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the backend never came to rest"
            );
        }
    }

    /// Asserts that since `watch` no client asked the server to send anything to anybody, and
    /// that the backend queued no job and owes no receipt. Call it once the backend has
    /// handled the signal (its event is in the sink, or [`World::handled`]).
    async fn silent_since(&self, watch: &Watch, what: &str) {
        self.sent_nothing_since(watch, what).await;
        assert_eq!(
            self.sql(&format!(
                "SELECT count(*) FROM jobs WHERE id > {}",
                watch.last_job
            )),
            0,
            "{what}: a job was queued"
        );
    }

    /// The server's half of [`World::silent_since`], and no owed receipt: for a watch that
    /// spans a restart, whose own jobs (capabilities, remote config) are no sends.
    async fn sent_nothing_since(&self, watch: &Watch, what: &str) {
        self.handled().await;
        tokio::time::sleep(QUIET).await;
        assert_eq!(
            self.send_requests().await,
            watch.send_requests,
            "{what}: Kourier sent something"
        );
        assert_eq!(
            self.sql("SELECT count(*) FROM delivery_receipts"),
            0,
            "{what}: a delivery receipt is owed"
        );
    }

    /// Waits until stage 2 has handled every envelope the device acknowledged.
    async fn handled(&self) {
        self.files
            .wait_for_i64("an empty inbox", "SELECT count(*) FROM inbox", |n| {
                n == Some(0)
            })
            .await;
    }

    /// Call signalling from `from` to our device, delivered before this returns. Returns the
    /// envelope's server timestamp: the time a line takes for what the other side says. (The
    /// sender's own timestamp is read from the harness's clock a moment earlier and can be
    /// a millisecond less.)
    async fn call(&self, from: &Primary, device: &DeviceInfo, mut args: Value) -> u64 {
        args["from"] = json!(from.aci);
        args["to"] = json!(device.device_ref);
        args["awaitDelivery"] = json!(true);
        args["timeout"] = json!(60_000);
        self.harness
            .send_call(args)
            .await
            .expect("sendCall")
            .server_timestamp
            .expect("sendCall states the envelope's server timestamp")
    }

    /// What our phone syncs about a call, delivered before this returns.
    async fn synced(&self, device: &DeviceInfo, call_event: Value) -> u64 {
        self.harness
            .send_sync(json!({
                "from": self.phone.aci, "to": device.device_ref, "awaitDelivery": true,
                "timeout": 60_000, "callEvent": call_event,
            }))
            .await
            .expect("sendSync callEvent")
            .timestamp
    }

    /// The `n`th signal the backend reported for the line `local` under `author` (`""` for
    /// our own devices), once there are that many.
    async fn signal(&self, author: &str, local: &str, n: usize) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let values = self.sink.message_values(author, local);
            if values.len() >= n {
                return values[n - 1].clone();
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "signal {n} of {local} never reached the core"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// How many signals the backend reported for the line `local` under `author`.
    fn signals(&self, author: &str, local: &str) -> usize {
        self.sink.message_values(author, local).len()
    }

    async fn shutdown(self, core: FakeCore) {
        assert_eq!(core.shutdown().await, Exit::Shutdown);
        self.harness.shutdown().await.expect("harness shutdown");
    }
}

/// A conversation key as the sink stores it (serde JSON of `ConvKey`).
fn key_json(key: &ConvKey) -> Value {
    serde_json::to_value(key).expect("conv key json")
}

/// The call line of a reported signal.
fn line(message: &Value) -> &Value {
    let line = &message["body"]["System"]["Call"];
    assert!(line.is_object(), "not a call line: {message}");
    line
}

/// The time a ringing signal says the ring has left (`deadline - at`).
fn time_left(message: &Value) -> i64 {
    let deadline = line(message)["state"]["Ringing"]["deadline"]
        .as_i64()
        .unwrap_or_else(|| panic!("not ringing: {message}"));
    deadline - message["sent_at"].as_i64().expect("sent_at")
}

/// The ring id an era id maps to, computed here: the number of 16 hex digits, else the first
/// eight bytes of the era's SHA-256, little-endian; as the unsigned decimal of a call id.
fn era_call_id(era: &str) -> String {
    if era.len() == 16
        && let Ok(number) = u64::from_str_radix(era, 16)
    {
        return if number == 0 {
            u64::MAX.to_string()
        } else {
            number.to_string()
        };
    }
    let digest = Sha256::digest(era.as_bytes());
    let mut id: u64 = 0;
    for (i, byte) in digest[..8].iter().enumerate() {
        id |= u64::from(*byte) << (8 * i);
    }
    id.to_string()
}

/// 1:1 calls: the Offer rings for what is left of 60 s by the server's clock, the caller's
/// Hangup NORMAL is a missed call and no other Hangup is anything, the phone's sync says
/// answered, declined, outgoing and deleted, a stranger's call never rings; and after each
/// of them the server has seen nothing from Kourier.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_to_one_calls_are_reported_and_never_answered() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let ada = world.ada();
    let ada_user = World::user(ada);
    let conv = World::direct(ada);
    let remote = json!({ "Remote": { "author": ada_user, "device": 1 } });
    let own = json!({ "OwnOtherDevice": { "device": 1 } });

    // -- a fresh Offer rings: voice, by the caller, a minute left
    let watch = world.watch().await;
    let at = world
        .call(ada, &device, json!({ "offer": { "id": 7001 } }))
        .await;
    let ring = world.signal(&ada_user, "call:7001", 1).await;
    assert_eq!(ring["conv"], conv, "{ring}");
    assert_eq!(ring["origin"], remote, "{ring}");
    assert_eq!(ring["sent_at"], at);
    assert_eq!(line(&ring)["direction"], "Incoming");
    assert_eq!(line(&ring)["group"], false);
    assert_eq!(line(&ring)["media"], "Voice");
    assert_eq!(line(&ring)["by"], ada_user);
    let left = time_left(&ring);
    assert!(
        (58_000..=60_000).contains(&left),
        "a minute minus the delivery: {left}"
    );
    assert_eq!(ring["silent"], false);
    assert_eq!(ring["catch_up"], false, "{ring}");
    assert!(ring["received_at"].as_i64().is_some_and(|t| t > 0));
    world.silent_since(&watch, "an Offer").await;

    // -- the caller gives up: missed
    let watch = world.watch().await;
    world
        .call(
            ada,
            &device,
            json!({ "hangup": { "id": 7001, "type": "normal" } }),
        )
        .await;
    let missed = world.signal(&ada_user, "call:7001", 2).await;
    assert_eq!(missed["origin"], remote);
    assert_eq!(line(&missed)["state"], "Missed", "{missed}");
    assert_eq!(line(&missed)["by"], ada_user);
    assert!(line(&missed)["media"].is_null(), "a hang-up names no kind");
    world.silent_since(&watch, "a Hangup NORMAL").await;

    // -- an Offer that waited 59 s on the server has a second left; the phone answers it
    let watch = world.watch().await;
    world
        .call(
            ada,
            &device,
            json!({ "offer": { "id": 7002, "video": true }, "ageMs": 59_000 }),
        )
        .await;
    let late = world.signal(&ada_user, "call:7002", 1).await;
    assert_eq!(line(&late)["media"], "Video");
    let left = time_left(&late);
    assert!((1..=1_000).contains(&left), "59 s old: {left}");
    world
        .silent_since(&watch, "an Offer that is 59 s old")
        .await;

    let watch = world.watch().await;
    let answered_at = 1_800_000_000_123u64;
    world
        .synced(
            &device,
            json!({
                "conversation": { "aci": ada.aci }, "callId": 7002, "timestamp": answered_at,
                "type": "video", "direction": "incoming", "event": "accepted",
            }),
        )
        .await;
    let answered = world.signal("", "call:7002", 1).await;
    assert_eq!(answered["conv"], conv);
    assert_eq!(answered["origin"], own, "{answered}");
    assert_eq!(answered["sent_at"], answered_at, "the phone's time");
    assert_eq!(line(&answered)["direction"], "Incoming");
    assert_eq!(line(&answered)["state"], "Answered");
    assert_eq!(line(&answered)["media"], "Video");
    assert_eq!(line(&answered)["by"], ada_user);
    world
        .silent_since(&watch, "the phone's INCOMING ACCEPTED")
        .await;

    // -- an Offer older than the ring time has no time left (the core makes it missed)
    let watch = world.watch().await;
    world
        .call(
            ada,
            &device,
            json!({ "offer": { "id": 7003 }, "ageMs": 61_000 }),
        )
        .await;
    let expired = world.signal(&ada_user, "call:7003", 1).await;
    assert!(time_left(&expired) <= 0, "61 s old: {expired}");
    assert_eq!(expired["silent"], false);
    world.silent_since(&watch, "an expired Offer").await;

    // -- a Hangup that says what another of our devices did is not reported, nor is any
    //    other call signalling, nor a call message for another of our devices
    let our_device = u64::from(device.device_id);
    let ignored = [
        json!({ "hangup": { "id": 7003, "type": "accepted", "deviceId": 1 } }),
        json!({ "hangup": { "id": 7003, "type": "declined", "deviceId": 1 } }),
        json!({ "hangup": { "id": 7003, "type": "busy", "deviceId": 1 } }),
        json!({ "hangup": { "id": 7003, "type": "needPermission" } }),
        json!({ "hangup": { "id": 7003, "type": 9 } }),
        json!({ "answer": { "id": 7003 } }),
        json!({ "busy": { "id": 7003 } }),
        json!({ "iceUpdate": { "id": 7003 } }),
        json!({ "offer": { "id": 7004 }, "destinationDeviceId": our_device + 1 }),
        json!({ "hangup": { "id": 7003 }, "destinationDeviceId": our_device + 1 }),
    ];
    for args in ignored {
        let watch = world.watch().await;
        let what = args.to_string();
        world.call(ada, &device, args).await;
        world.silent_since(&watch, &what).await;
        assert_eq!(world.signals(&ada_user, "call:7003"), 1, "{what}");
        assert_eq!(world.signals(&ada_user, "call:7004"), 0, "{what}");
    }
    // A call message addressed to this very device is taken.
    let watch = world.watch().await;
    world
        .call(
            ada,
            &device,
            json!({ "offer": { "id": 7004 }, "destinationDeviceId": our_device }),
        )
        .await;
    world.signal(&ada_user, "call:7004", 1).await;
    world.silent_since(&watch, "an Offer for this device").await;

    // -- a caller nobody accepted: a line with no ringing phase, never announced
    let dan = &world.stranger;
    let dan_user = World::user(dan);
    let watch = world.watch().await;
    world
        .call(dan, &device, json!({ "offer": { "id": 7005 } }))
        .await;
    let request = world.signal(&dan_user, "call:7005", 1).await;
    assert_eq!(request["conv"], World::direct(dan));
    assert_eq!(time_left(&request), 0, "no ringing phase: {request}");
    assert_eq!(request["silent"], true);
    assert_eq!(line(&request)["by"], dan_user);
    world.silent_since(&watch, "a stranger's Offer").await;
    let watch = world.watch().await;
    world
        .call(dan, &device, json!({ "hangup": { "id": 7005 } }))
        .await;
    let gone = world.signal(&dan_user, "call:7005", 2).await;
    assert_eq!(line(&gone)["state"], "Missed");
    assert_eq!(gone["silent"], true, "{gone}");
    world.silent_since(&watch, "a stranger's Hangup").await;

    // -- the rest of the phone's syncs
    for (id, kind, direction, event, state, media, by, line_direction) in [
        (
            7006u64,
            "audio",
            "incoming",
            "notAccepted",
            "Declined",
            "Voice",
            Some(&ada_user),
            "Incoming",
        ),
        (
            7007, "video", "outgoing", "accepted", "Answered", "Video", None, "Outgoing",
        ),
        (
            7008,
            "audio",
            "outgoing",
            "notAccepted",
            "Unanswered",
            "Voice",
            None,
            "Outgoing",
        ),
    ] {
        let watch = world.watch().await;
        world
            .synced(
                &device,
                json!({
                    "conversation": { "aci": ada.aci }, "callId": id, "timestamp": answered_at + id,
                    "type": kind, "direction": direction, "event": event,
                }),
            )
            .await;
        let local = format!("call:{id}");
        let synced = world.signal("", &local, 1).await;
        let what = format!("{direction} {event}: {synced}");
        assert_eq!(synced["conv"], conv, "{what}");
        assert_eq!(synced["origin"], own, "{what}");
        assert_eq!(line(&synced)["direction"], line_direction, "{what}");
        assert_eq!(line(&synced)["state"], state, "{what}");
        assert_eq!(line(&synced)["media"], media, "{what}");
        assert_eq!(line(&synced)["group"], false, "{what}");
        match by {
            Some(user) => assert_eq!(line(&synced)["by"], **user, "{what}"),
            None => assert!(line(&synced)["by"].is_null(), "{what}"),
        }
        world.silent_since(&watch, &what).await;
    }

    // DELETE: the line goes, by our own account, for us only.
    let watch = world.watch().await;
    world
        .synced(
            &device,
            json!({
                "conversation": { "aci": ada.aci }, "callId": 7006, "timestamp": answered_at,
                "type": "audio", "direction": "incoming", "event": "delete",
            }),
        )
        .await;
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| e["target"]["local"] == "call:7006")
        .await;
    assert_eq!(deleted["target"]["conv"], conv, "{deleted}");
    assert_eq!(deleted["target"]["author"], "Own");
    assert_eq!(deleted["for_everyone"], false);
    assert!(deleted["by"].is_null());
    world.silent_since(&watch, "the phone's DELETE").await;

    // Call links, OBSERVED, and an event whose type does not fit its conversation: nothing.
    let deletes = world.sink.event_values("Deleted").len();
    for (id, kind, event) in [
        (7010u64, "adHoc", "accepted"),
        (7011, "audio", "observed"),
        (7012, "group", "accepted"),
    ] {
        let watch = world.watch().await;
        world
            .synced(
                &device,
                json!({
                    "conversation": { "aci": ada.aci }, "callId": id, "timestamp": answered_at,
                    "type": kind, "direction": "incoming", "event": event,
                }),
            )
            .await;
        world.silent_since(&watch, kind).await;
        for local in [format!("call:{id}"), format!("gcall:{id}")] {
            assert_eq!(world.signals("", &local), 0, "{kind} {event}");
        }
    }
    assert_eq!(world.sink.event_values("Deleted").len(), deletes);

    // -- a `groupCallUpdate` outside a group means nothing: no line and no "Call" bubble
    let before = world.sink.messages_from(&ada_user);
    let watch = world.watch().await;
    let update = world
        .harness
        .send_data(json!({
            "from": ada.aci, "to": device.device_ref, "awaitDelivery": true, "sealed": true,
            "groupCallUpdate": { "eraId": "0123456789abcdef" },
        }))
        .await
        .expect("sendData")
        .timestamp;
    world.silent_since(&watch, "a 1:1 groupCallUpdate").await;
    assert_eq!(world.sink.messages_from(&ada_user), before);
    assert!(
        world
            .sink
            .message_values(&ada_user, &update.to_string())
            .is_empty()
    );

    // -- the control: the same watch does see what Kourier sends. A sealed text from an
    //    accepted contact is owed a delivery receipt, and the server sees it go out.
    let watch = world.watch().await;
    let text = world
        .harness
        .send_data(json!({
            "from": ada.aci, "to": device.device_ref, "awaitDelivery": true, "sealed": true,
            "text": "are you there?", "withProfileKey": true,
        }))
        .await
        .expect("sendData")
        .timestamp;
    world
        .sink
        .wait_message_value(&ada_user, &text.to_string())
        .await;
    let receipt = world
        .harness
        .wait_for_receipt(&ada.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("the delivery receipt of a text");
    assert_eq!(receipt.timestamps, vec![text]);
    assert!(
        world.send_requests().await > watch.send_requests,
        "the server counts a receipt as a send"
    );
    world.shutdown(core).await;
}

/// Group calls: a ring (also one that comes with an Offer) and its cancellation, a member's
/// group-call update, our own from another device, and the phone's syncs meet in the line of
/// the call's id; rings that are none, unknown groups and non-members give nothing; and after
/// each of them the server has seen nothing from Kourier, not even a delivery receipt for a
/// sealed group-call update.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_calls_are_reported_and_never_answered() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let (ada, bo) = (world.ada(), world.bo());
    let (ada_user, bo_user) = (World::user(ada), World::user(bo));
    let conv = world.group_conv();
    let own = json!({ "OwnOtherDevice": { "device": 1 } });

    // -- a ring, sent as Desktop's own fixture sends it: with an Offer beside it. It is a
    //    group ring only. The ring id is negative: the call id is its unsigned form.
    let ring_id: i64 = -5;
    let ring_call = ring_id.cast_unsigned().to_string();
    let ring_local = format!("gcall:{ring_call}");
    let watch = world.watch().await;
    let at = world
        .call(
            ada,
            &device,
            json!({
                "offer": { "id": 9001, "video": true },
                "ring": { "group": world.group.id, "ringId": ring_id },
            }),
        )
        .await;
    let ring = world.signal(&ada_user, &ring_local, 1).await;
    assert_eq!(ring["conv"], conv, "{ring}");
    assert_eq!(
        ring["origin"],
        json!({ "Remote": { "author": ada_user, "device": 1 } })
    );
    assert_eq!(ring["sent_at"], at);
    assert_eq!(line(&ring)["group"], true);
    assert_eq!(line(&ring)["direction"], "Incoming");
    assert_eq!(line(&ring)["by"], ada_user);
    assert!(line(&ring)["media"].is_null(), "a group call has no kind");
    let left = time_left(&ring);
    assert!((58_000..=60_000).contains(&left), "{left}");
    assert_eq!(ring["catch_up"], false);
    world.silent_since(&watch, "a group ring").await;
    assert_eq!(
        world.signals(&ada_user, "call:9001"),
        0,
        "no 1:1 call of the ringer beside the ring"
    );

    // -- the ringer takes it back: missed
    let watch = world.watch().await;
    world
        .call(
            ada,
            &device,
            json!({ "ring": { "group": world.group.id, "ringId": ring_id, "cancelled": true } }),
        )
        .await;
    let cancelled = world.signal(&ada_user, &ring_local, 2).await;
    assert_eq!(line(&cancelled)["state"], "Missed", "{cancelled}");
    assert_eq!(line(&cancelled)["group"], true);
    world.silent_since(&watch, "a cancelled group ring").await;

    // -- a ring that waited on the server
    let watch = world.watch().await;
    world
        .call(
            bo,
            &device,
            json!({ "ring": { "group": world.group.id, "ringId": 9002 }, "ageMs": 59_000 }),
        )
        .await;
    let late = world.signal(&bo_user, "gcall:9002", 1).await;
    assert!((1..=1_000).contains(&time_left(&late)), "{late}");
    world
        .silent_since(&watch, "a group ring that is 59 s old")
        .await;

    // -- no line: an opaque payload that is no ring (garbage, empty), a group we do not
    //    know, a ringer who is no member
    let unknown_group = STANDARD.encode([0x5au8; 32]);
    let nothing = [
        (
            ada,
            json!({ "opaque": STANDARD.encode(b"\xff\xff\xff\xff not a protobuf") }),
        ),
        (ada, json!({ "opaque": "" })),
        (
            ada,
            json!({ "ring": { "groupId": unknown_group, "ringId": 9003 } }),
        ),
        (
            &world.stranger,
            json!({ "ring": { "group": world.group.id, "ringId": 9003 } }),
        ),
    ];
    for (from, args) in nothing {
        let watch = world.watch().await;
        let what = args.to_string();
        world.call(from, &device, args).await;
        world.silent_since(&watch, &what).await;
        for author in [&ada_user, &World::user(&world.stranger)] {
            assert_eq!(world.signals(author, "gcall:9003"), 0, "{what}");
        }
    }

    // -- a member's group-call update: the line of the call its era names, started, by its
    //    sender; sent sealed, and still no delivery receipt goes out for it
    let era = "0123456789abcdef";
    let era_local = format!("gcall:{}", era_call_id(era));
    assert_eq!(era_local, "gcall:81985529216486895");
    let before = world.sink.messages_from(&bo_user);
    let watch = world.watch().await;
    let update = world
        .harness
        .send_data(json!({
            "from": bo.aci, "to": [device.device_ref], "group": world.group.id,
            "awaitDelivery": true, "sealed": true, "groupCallUpdate": { "eraId": era },
        }))
        .await
        .expect("sendData")
        // A group-call update is a message of its sender: the line takes the message's own
        // timestamp (unlike a ring, which takes the envelope's server time).
        .timestamp;
    let started = world.signal(&bo_user, &era_local, 1).await;
    assert_eq!(started["conv"], conv, "{started}");
    assert_eq!(started["sent_at"], update);
    assert_eq!(line(&started)["state"], "Started");
    assert_eq!(line(&started)["group"], true);
    assert_eq!(line(&started)["direction"], "Incoming");
    assert_eq!(line(&started)["by"], bo_user);
    world
        .silent_since(&watch, "a sealed group-call update")
        .await;
    assert_eq!(
        world.sink.messages_from(&bo_user),
        before + 1,
        "one line and no bubble"
    );
    assert!(
        world
            .sink
            .message_values(&bo_user, &update.to_string())
            .is_empty(),
        "no message under the update's own id"
    );

    // An era id that is no 16-digit hex number maps through SHA-256; a second update of the
    // same call is a second signal of the same line.
    let hashed = "lobby-2026";
    let hashed_local = format!("gcall:{}", era_call_id(hashed));
    for n in 1..=2 {
        let watch = world.watch().await;
        world
            .harness
            .send_data(json!({
                "from": ada.aci, "to": [device.device_ref], "group": world.group.id,
                "awaitDelivery": true, "groupCallUpdate": { "eraId": hashed },
            }))
            .await
            .expect("sendData");
        let again = world.signal(&ada_user, &hashed_local, n).await;
        assert_eq!(line(&again)["state"], "Started");
        world.silent_since(&watch, "a group-call update").await;
    }

    // Without an era id (Android, after a declined ring): nothing.
    let before = world.sink.messages_from(&ada_user);
    let watch = world.watch().await;
    world
        .harness
        .send_data(json!({
            "from": ada.aci, "to": [device.device_ref], "group": world.group.id,
            "awaitDelivery": true, "groupCallUpdate": {},
        }))
        .await
        .expect("sendData");
    world
        .silent_since(&watch, "a group-call update without an era")
        .await;
    assert_eq!(world.sink.messages_from(&ada_user), before);

    // -- the user joins on the phone: its transcript of the update says answered and names
    //    nobody
    let watch = world.watch().await;
    let joined = world
        .harness
        .send_data(json!({
            "from": world.phone.aci, "to": device.device_ref, "group": world.group.id,
            "awaitDelivery": true, "groupCallUpdate": { "eraId": era }, "sync": {},
        }))
        .await
        .expect("sendData")
        .timestamp;
    let answered = world.signal("", &era_local, 1).await;
    assert_eq!(answered["conv"], conv, "{answered}");
    assert_eq!(answered["origin"], own);
    assert_eq!(answered["sent_at"], joined);
    assert_eq!(line(&answered)["state"], "Answered");
    assert_eq!(line(&answered)["direction"], "Incoming");
    assert_eq!(line(&answered)["group"], true);
    assert!(line(&answered)["by"].is_null());
    world
        .silent_since(&watch, "our own group-call update")
        .await;
    assert!(
        !world
            .sink
            .event_names()
            .contains(&"OwnSendState".to_owned()),
        "a call line is no message of ours"
    );

    // -- the phone's syncs name the ring id as an unsigned call id
    for (n, event, state) in [
        (1u64, "accepted", "Answered"),
        (2, "notAccepted", "Declined"),
    ] {
        let watch = world.watch().await;
        world
            .synced(
                &device,
                json!({
                    "conversation": { "group": world.group.id }, "callId": ring_call,
                    "timestamp": 1_800_000_000_000u64 + n, "type": "group",
                    "direction": "incoming", "event": event,
                }),
            )
            .await;
        let synced = world
            .signal("", &ring_local, usize::try_from(n).unwrap())
            .await;
        assert_eq!(synced["conv"], conv, "{synced}");
        assert_eq!(synced["origin"], own);
        assert_eq!(line(&synced)["state"], state, "{synced}");
        assert_eq!(line(&synced)["group"], true);
        assert_eq!(line(&synced)["direction"], "Incoming");
        assert!(line(&synced)["by"].is_null(), "the sync names nobody");
        assert!(line(&synced)["media"].is_null());
        world.silent_since(&watch, event).await;
    }
    let watch = world.watch().await;
    world
        .synced(
            &device,
            json!({
                "conversation": { "group": world.group.id }, "callId": ring_call,
                "timestamp": 1_800_000_000_009u64, "type": "group",
                "direction": "incoming", "event": "delete",
            }),
        )
        .await;
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| e["target"]["local"] == ring_local)
        .await;
    assert_eq!(deleted["target"]["conv"], conv);
    assert_eq!(deleted["for_everyone"], false);
    world.silent_since(&watch, "a group DELETE").await;

    // A group the backend does not know, and a 1:1 event that names a group: nothing.
    for (conversation, kind) in [
        (json!({ "raw": unknown_group }), "group"),
        (json!({ "group": world.group.id }), "audio"),
    ] {
        let watch = world.watch().await;
        world
            .synced(
                &device,
                json!({
                    "conversation": conversation, "callId": 9010, "timestamp": 1_800_000_000_010u64,
                    "type": kind, "direction": "incoming", "event": "accepted",
                }),
            )
            .await;
        world.silent_since(&watch, kind).await;
        assert_eq!(world.signals("", "gcall:9010"), 0);
        assert_eq!(world.signals("", "call:9010"), 0);
    }
    world.shutdown(core).await;
}

/// A call that came while Kourier was closed: its Offer is from before the catch-up boundary
/// (`catch_up`, as for a message), and its age is the time it really waited on the server,
/// so the ring has that much less left. Nothing is sent for it either.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_from_while_kourier_was_closed_is_a_catch_up_signal_with_its_age() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let ada = world.ada();
    let ada_user = World::user(ada);
    let watch = world.watch().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // The Offer waits in the server's queue for three seconds.
    world
        .harness
        .send_call(json!({ "from": ada.aci, "to": device.device_ref, "offer": { "id": 7100 } }))
        .await
        .expect("sendCall");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut core = world.actor(StartMode::Resume);
    core.send(Command::Connect).await;
    core.wait_online().await;
    let ring = world.signal(&ada_user, "call:7100", 1).await;
    assert_eq!(ring["catch_up"], true, "{ring}");
    let left = time_left(&ring);
    assert!(
        (50_000..=57_100).contains(&left),
        "a minute minus three seconds in the queue: {left}"
    );
    world
        .sent_nothing_since(&watch, "an Offer from the offline queue")
        .await;
    world.shutdown(core).await;
}
