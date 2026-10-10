//! S13 against the mock Signal server (architecture §6.7, §6.13): edits, deletes for everyone
//! and for me, reactions, quotes, 1:1 timers, placeholders and the sync messages that go with
//! them, in both directions and in both kinds of chat.
//!
//! The account actor runs under the fake core of `support`: every `Transmit` is one attempt,
//! and what the backend reports lands in the sink. The other side is the mock's phone ("Myself")
//! and three contacts who are also the members of one group.
//!
//! Skipped (with a notice) when the mock harness is not installed; with
//! `KOURIER_REQUIRE_MOCK_HARNESS=1` a missing harness is a failure.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    dead_code,
    reason = "the shared support modules serve several test files"
)]

mod support;

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kourier_backend_api::{
    AllocatedId, BackendEnv, Command, Exit, InitialSync, Network as _, OutgoingKind,
    OutgoingRequest, StartMode, TransmitOutcome,
};
use kourier_model::{
    Body, ConvKey, ConversationChange, ErrorKind, LoginStepKind, MsgTarget, QuoteSnapshot, ReplyTo,
    TargetAuthor, Text, UserId,
};
use kourier_signal::SignalNetwork;
use serde_json::{Value, json};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT};
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, Primary, ReceivedMessage,
    StorageContact, StorageGroup, StorageSpec,
};

/// How long "nothing more arrives" is watched.
const QUIET: Duration = Duration::from_millis(1500);

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// A message id allocated like the core does: the time now, never twice the same.
fn allocate(conv: &ConvKey) -> AllocatedId {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(0);
    let mut unique = now;
    let _ = LAST.try_update(Ordering::SeqCst, Ordering::SeqCst, |last| {
        unique = now.max(last + 1);
        Some(unique)
    });
    SignalNetwork::new().allocate_message_id(&[], conv, unique)
}

/// A request of `kind` in `conv` with a fresh id.
fn request(conv: &ConvKey, kind: OutgoingKind) -> OutgoingRequest {
    let id = allocate(conv);
    OutgoingRequest {
        conv: conv.clone(),
        local: id.local,
        sent_at: id.sent_at,
        kind,
        only: None,
        is_recipient_update: false,
    }
}

fn text(conv: &ConvKey, body: &str) -> OutgoingRequest {
    request(
        conv,
        OutgoingKind::Message {
            body: Body::text(body),
            reply_to: None,
            expiry: None,
        },
    )
}

/// A reference to a message of `author` (`None`: our own) sent at `timestamp`.
fn target(conv: &ConvKey, author: Option<&UserId>, timestamp: u64) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author: author.map_or(TargetAuthor::Own, |user| TargetAuthor::User(user.clone())),
        local: timestamp.to_string().into(),
        alternates: Vec::new(),
    }
}

fn sent_at(req: &OutgoingRequest) -> u64 {
    u64::try_from(req.sent_at).expect("timestamp")
}

/// A binary ACI as the harness prints it (base64 of the 16 uuid bytes).
fn aci_b64(aci: &str) -> String {
    let uuid = uuid::Uuid::parse_str(aci).expect("aci");
    STANDARD.encode(uuid.as_bytes())
}

/// Whether the JSON string `value` is the Signal message id `timestamp`.
fn is_id(value: &Value, timestamp: u64) -> bool {
    value.as_str().is_some_and(|id| id.parse() == Ok(timestamp))
}

/// A conversation key as the sink stores it (serde JSON of `ConvKey`).
fn key_json(key: &ConvKey) -> Value {
    serde_json::to_value(key).expect("conv key json")
}

fn assert_sent(outcome: &TransmitOutcome, recipients: usize) {
    let TransmitOutcome::Sent { per_recipient, .. } = outcome else {
        panic!("not sent: {outcome:?}");
    };
    assert_eq!(per_recipient.len(), recipients, "{outcome:?}");
}

/// `Command::ChangeConversation { SetTimer }` for `conv` with a fresh id; returns the id and the
/// backend's answer.
async fn set_timer(
    core: &FakeCore,
    conv: &ConvKey,
    secs: Option<u64>,
) -> (AllocatedId, Result<(), kourier_model::BackendError>) {
    let id = allocate(conv);
    let (reply, answer) = tokio::sync::oneshot::channel();
    core.send(Command::ChangeConversation {
        conv: conv.clone(),
        change: ConversationChange::SetTimer {
            timer: secs.map(Duration::from_secs),
        },
        id: Some(id.clone()),
        reply,
    })
    .await;
    (id, answer.await.expect("answer"))
}

/// Our phone ("Myself"), three contacts, and the group "Climbing club" with all of them.
struct World {
    harness: Harness,
    _tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    members: [Primary; 3],
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
            group,
            sink,
            files,
        })
    }

    fn ada(&self) -> &Primary {
        &self.members[0]
    }

    fn group_conv(&self) -> ConvKey {
        let id = STANDARD.decode(&self.group.id).expect("group id");
        ConvKey::Group(format!("gv2:{}", URL_SAFE_NO_PAD.encode(id)).into())
    }

    fn user(primary: &Primary) -> UserId {
        UserId::parse(&format!("aci:{}", primary.aci)).expect("user id")
    }

    fn direct(primary: &Primary) -> ConvKey {
        ConvKey::Direct(Self::user(primary))
    }

    /// Links a fresh account whose phone has the group and its members as accepted contacts,
    /// and waits until the group can be written to.
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
                "group state with endorsements",
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

    /// A text from `from` to our device; returns its timestamp once the device has it.
    async fn says(&self, from: &Primary, device: &DeviceInfo, text: &str) -> u64 {
        self.harness
            .send_data(json!({
                "from": from.aci, "to": device.device_ref, "text": text,
                "withProfileKey": true, "awaitDelivery": true, "timeout": 60_000,
            }))
            .await
            .expect("sendData")
            .timestamp
    }

    /// The next `DataMessage` of `kind` that `who` got from our device, 1:1 or in the group.
    async fn got(
        &self,
        who: &Primary,
        device: &DeviceInfo,
        kind: &str,
        in_group: bool,
    ) -> ReceivedMessage {
        self.harness
            .wait_for_data(
                &who.aci,
                kind,
                Some(&device.device_ref),
                in_group.then_some(self.group.id.as_str()),
                WAIT,
            )
            .await
            .unwrap_or_else(|e| panic!("{} got no {kind}: {e}", who.profile_name))
    }

    /// Asserts `who` gets no further `DataMessage` or edit from our device for a while.
    async fn nothing_more(&self, who: &Primary, device: &DeviceInfo) {
        let message = self
            .harness
            .wait_for_message(&who.aci, Some(&device.device_ref), QUIET)
            .await;
        assert!(message.is_err(), "another message: {message:?}");
        let edit = self
            .harness
            .wait_for_edit(
                &who.aci,
                Some(&device.device_ref),
                None,
                Duration::from_millis(100),
            )
            .await;
        assert!(edit.is_err(), "another edit: {edit:?}");
    }

    /// The next sync transcript (`sent`) the phone got from our device.
    async fn transcript(&self, device: &DeviceInfo) -> Value {
        let sync = self
            .harness
            .wait_for_sync_message(&self.phone.aci, Some(&device.device_ref), "sent", WAIT)
            .await
            .expect("sync transcript");
        sync.sync_message["content"]["sent"].clone()
    }

    async fn shutdown(self, core: FakeCore) {
        assert_eq!(core.shutdown().await, Exit::Shutdown);
        self.harness.shutdown().await.expect("harness shutdown");
    }
}

/// 1:1, Kourier → contact and phone: a reaction and its removal, two edits in a row, a reply
/// with a quote and its edit (the quote goes along), a delete for everyone and a delete for
/// me; each with the fields Desktop sets, sealed, logged, with its sync transcript, and never
/// twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_to_one_modifiers_reach_the_contact_and_the_phone() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let ada = world.ada();
    let conv = World::direct(ada);
    let ada_user = World::user(ada);

    let hers = world.says(ada, &device, "Is the gym open?").await;
    world
        .sink
        .wait_message(ada_user.as_str(), &hers.to_string(), WAIT)
        .await
        .expect("her message");

    // The features the core got with her first message: everything S13 turns on (values:
    // `features` unit tests).
    let features = world
        .sink
        .wait_event_value("ConversationUpsert", |p| {
            p["key"] == key_json(&conv) && !p["features"].is_null()
        })
        .await["features"]
        .clone();
    assert_eq!(features["reply"], "Full", "{features}");
    assert_eq!(features["reactions"], "Full");
    assert_eq!(features["edit"]["max_age_secs"], 86_400);
    // What a receiver enforces (iOS drops edits after the hundredth), not the composers' 10.
    assert_eq!(features["edit"]["max_count"], 100);
    // Files pack into albums by format: what Desktop, Android and iOS all draw (pictures and
    // MP4 videos); every other file travels alone, a HEIC picture and a QuickTime video too.
    assert_eq!(
        features["file_packing"]["albums"][0]["max"], 31,
        "{features}"
    );
    assert_eq!(
        features["file_packing"]["albums"][0]["kinds"],
        serde_json::json!([]),
        "{features}"
    );
    assert_eq!(
        features["file_packing"]["albums"][0]["types"],
        serde_json::json!([
            "image/bmp",
            "image/gif",
            "image/jpeg",
            "image/png",
            "image/webp",
            "video/mp4"
        ]),
        "{features}"
    );
    // Any timer from a second to the largest one Android reads as it was sent (a signed
    // 32-bit number of seconds), besides the offered list. Four weeks is no limit.
    assert_eq!(
        features["disappearing_custom"]["AfterRead"],
        serde_json::json!({ "min_secs": 1, "max_secs": 2_147_483_647, "step_secs": 1 }),
        "{features}"
    );
    assert_eq!(features["file_packing"]["reply_needs_text"], false);
    assert_eq!(features["delete_for_everyone"]["max_age_secs"], 86_400);
    assert_eq!(features["delete_for_me"], true);
    assert_eq!(features["edit_receive"]["max_age_secs"], 172_800);
    assert!(
        features["disappearing"]["AfterRead"].is_array(),
        "{features}"
    );

    // -- a reaction to her message, then taken back
    let react = |emoji: &str, remove: bool| {
        request(
            &conv,
            OutgoingKind::React {
                target: target(&conv, Some(&ada_user), hers),
                emoji: emoji.to_owned(),
                remove,
            },
        )
    };
    let reaction = react("👍", false);
    assert_sent(&core.transmit(reaction.clone()).await, 1);
    let got = world.got(ada, &device, "reaction", false).await;
    assert!(got.sealed, "sealed like a message");
    assert_eq!(got.timestamp, sent_at(&reaction));
    let sent = &got.data_message["reaction"];
    assert_eq!(sent["emoji"], "👍", "{sent}");
    assert_ne!(sent["remove"], true);
    assert_eq!(sent["targetSentTimestamp"], hers);
    assert_eq!(
        sent["targetAuthorAciBinary"],
        aci_b64(&ada.aci),
        "her message"
    );
    assert!(got.data_message["profileKey"].is_string());
    assert_eq!(got.data_message["expireTimer"], 0, "the chat's timer: off");
    assert_eq!(got.data_message["expireTimerVersion"], 1);
    assert!(got.data_message["body"].is_null());
    let transcript = world.transcript(&device).await;
    assert_eq!(transcript["timestamp"], sent_at(&reaction));
    assert_eq!(transcript["message"]["reaction"]["emoji"], "👍");
    assert_eq!(
        transcript["destinationServiceIdBinary"],
        aci_b64(&ada.aci),
        "{transcript}"
    );
    assert!(transcript["expirationStartTimestamp"].is_null());
    // The same request again (the core retries after a crash): nobody gets it twice.
    assert_sent(&core.transmit(reaction.clone()).await, 1);
    world.nothing_more(ada, &device).await;

    // Only one emoji is a reaction (the receivers drop anything else): refused, nothing sent.
    for not_an_emoji in ["", "nice", "👍👍"] {
        match core.transmit(react(not_an_emoji, false)).await {
            TransmitOutcome::Failed(e) => assert_eq!(e.kind, ErrorKind::InvalidInput),
            other => panic!("{not_an_emoji:?}: {other:?}"),
        }
    }
    world.nothing_more(ada, &device).await;

    let removal = react("👍", true);
    assert_sent(&core.transmit(removal).await, 1);
    let got = world.got(ada, &device, "reaction", false).await;
    assert_eq!(got.data_message["reaction"]["remove"], true);
    assert_eq!(
        got.data_message["reaction"]["emoji"], "👍",
        "the emoji that goes"
    );
    world.transcript(&device).await;

    // -- our message, edited twice: each edit names the revision before it
    let original = text(&conv, "Yes, untli nine");
    assert_sent(&core.transmit(original.clone()).await, 1);
    assert_eq!(
        world.got(ada, &device, "message", false).await.text,
        "Yes, untli nine"
    );
    world.transcript(&device).await;
    let edit = |target_at: u64, body: &str| {
        request(
            &conv,
            OutgoingKind::Edit {
                target: target(&conv, None, target_at),
                body: Body::text(body),
            },
        )
    };
    let first = edit(sent_at(&original), "Yes, until nine");
    assert_sent(&core.transmit(first.clone()).await, 1);
    let got = world
        .harness
        .wait_for_edit(&ada.aci, Some(&device.device_ref), None, WAIT)
        .await
        .expect("the edit");
    assert!(got.sealed);
    assert_eq!(got.target_timestamp, sent_at(&original));
    assert_eq!(got.timestamp, sent_at(&first), "its own timestamp");
    assert_eq!(got.text, "Yes, until nine");
    assert!(got.data_message["profileKey"].is_string());
    assert_eq!(got.data_message["expireTimerVersion"], 1);
    let transcript = world.transcript(&device).await;
    assert_eq!(transcript["timestamp"], sent_at(&first));
    assert!(
        transcript["message"].is_null(),
        "an edit transcript: {transcript}"
    );
    assert_eq!(
        transcript["editMessage"]["targetSentTimestamp"],
        sent_at(&original)
    );
    assert_eq!(
        transcript["editMessage"]["dataMessage"]["body"],
        "Yes, until nine"
    );
    let second = edit(sent_at(&first), "Yes, until nine tonight");
    assert_sent(&core.transmit(second.clone()).await, 1);
    let got = world
        .harness
        .wait_for_edit(&ada.aci, Some(&device.device_ref), None, WAIT)
        .await
        .expect("the second edit");
    assert_eq!(
        got.target_timestamp,
        sent_at(&first),
        "the previous revision"
    );
    world.transcript(&device).await;

    // -- a reply with a quote; its edit carries the quote again
    let reply = request(
        &conv,
        OutgoingKind::Message {
            body: Body::text("Until nine"),
            reply_to: Some(ReplyTo {
                target: target(&conv, Some(&ada_user), hers),
                author: Some(ada_user.clone()),
                snapshot: Some(QuoteSnapshot {
                    text: Some(Text::plain("Is the gym open?")),
                    ..QuoteSnapshot::default()
                }),
            }),
            expiry: None,
        },
    );
    assert_sent(&core.transmit(reply.clone()).await, 1);
    let got = world.got(ada, &device, "message", false).await;
    let quote = &got.data_message["quote"];
    assert_eq!(quote["id"], hers, "{quote}");
    assert_eq!(quote["authorAciBinary"], aci_b64(&ada.aci));
    assert_eq!(quote["text"], "Is the gym open?");
    assert_eq!(quote["type"], 0, "NORMAL");
    assert_eq!(
        world.transcript(&device).await["message"]["quote"]["id"],
        hers
    );
    let reply_edit = edit(sent_at(&reply), "Until nine, I think");
    assert_sent(&core.transmit(reply_edit).await, 1);
    let got = world
        .harness
        .wait_for_edit(&ada.aci, Some(&device.device_ref), None, WAIT)
        .await
        .expect("the reply's edit");
    assert_eq!(
        got.data_message["quote"]["id"], hers,
        "receivers take the quote of the edit: {}",
        got.data_message
    );
    assert_eq!(got.data_message["quote"]["text"], "Is the gym open?");
    world.transcript(&device).await;

    // -- a reply sent from our phone and edited here: the edit carries the phone's quote
    let phone_reply = world
        .harness
        .send_data(json!({
            "from": world.phone.aci, "to": device.device_ref, "awaitDelivery": true,
            "text": "Sent from my phone", "withProfileKey": true,
            "quote": { "id": hers, "author": ada.aci, "text": "Is the gym open?" },
            "sync": { "destination": ada.aci },
        }))
        .await
        .expect("the phone's reply")
        .timestamp;
    world
        .sink
        .wait_message("", &phone_reply.to_string(), WAIT)
        .await
        .expect("the phone's reply reached the core");
    let phone_reply_edit = edit(phone_reply, "Sent from my phone, fixed here");
    assert_sent(&core.transmit(phone_reply_edit).await, 1);
    let got = world
        .harness
        .wait_for_edit(&ada.aci, Some(&device.device_ref), None, WAIT)
        .await
        .expect("the edit of the phone's reply");
    assert_eq!(got.target_timestamp, phone_reply);
    assert_eq!(
        got.data_message["quote"]["id"], hers,
        "the quote the phone sent goes along: {}",
        got.data_message
    );
    assert_eq!(got.data_message["quote"]["text"], "Is the gym open?");
    assert_eq!(
        world.transcript(&device).await["editMessage"]["dataMessage"]["quote"]["id"],
        hers
    );

    // -- delete for everyone: the target and the profile key, no timer
    let delete = request(
        &conv,
        OutgoingKind::Delete {
            target: target(&conv, None, sent_at(&original)),
            for_everyone: true,
        },
    );
    assert_sent(&core.transmit(delete.clone()).await, 1);
    let got = world.got(ada, &device, "delete", false).await;
    assert!(got.sealed);
    assert_eq!(
        got.data_message["delete"]["targetSentTimestamp"],
        sent_at(&original)
    );
    assert!(
        got.data_message["expireTimer"].is_null(),
        "{}",
        got.data_message
    );
    assert!(got.data_message["profileKey"].is_string());
    let transcript = world.transcript(&device).await;
    assert_eq!(
        transcript["message"]["delete"]["targetSentTimestamp"],
        sent_at(&original)
    );
    // Somebody else's message cannot be deleted for everyone in a 1:1 chat.
    let foreign = request(
        &conv,
        OutgoingKind::Delete {
            target: target(&conv, Some(&ada_user), hers),
            for_everyone: true,
        },
    );
    match core.transmit(foreign).await {
        TransmitOutcome::Failed(e) => assert_eq!(e.kind, ErrorKind::InvalidInput),
        other => panic!("{other:?}"),
    }

    // -- delete for me: only our own devices hear of it
    let for_me = request(
        &conv,
        OutgoingKind::Delete {
            target: target(&conv, Some(&ada_user), hers),
            for_everyone: false,
        },
    );
    assert_sent(&core.transmit(for_me.clone()).await, 0);
    let sync = world
        .harness
        .wait_for_sync_message(
            &world.phone.aci,
            Some(&device.device_ref),
            "deleteForMe",
            WAIT,
        )
        .await
        .expect("deleteForMe sync");
    let deletes = &sync.sync_message["content"]["deleteForMe"]["messageDeletes"];
    assert_eq!(deletes.as_array().map(Vec::len), Some(1), "{deletes}");
    let printed = deletes[0].to_string();
    assert!(
        printed.contains(&aci_b64(&ada.aci)),
        "the chat and the author are hers: {printed}"
    );
    assert_eq!(
        deletes[0]["messages"][0]["sentTimestamp"], hers,
        "{deletes}"
    );
    world.nothing_more(ada, &device).await;

    // Everything went through the send log and the progress table (retry requests, §4.8).
    for req in [&reaction, &first, &second, &delete, &for_me] {
        let recorded = world
            .files
            .peek_i64(&format!(
                "SELECT count(*) FROM send_progress WHERE local = '{}'",
                req.local
            ))
            .expect("send_progress");
        assert!(
            recorded.is_some_and(|n| n >= 1),
            "progress of {}",
            req.kind.name()
        );
    }
    world.shutdown(core).await;
}

/// The group: a reaction, an edit and a delete go to every member as one sealed sender-key
/// message with the group context and the group's transcript; a member the server
/// rate-limits gets the modifier in the follow-up alone, as a recipient update.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn group_modifiers_go_over_sender_keys() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let conv = world.group_conv();
    let features = world
        .sink
        .wait_event_value("ConversationUpsert", |p| {
            p["key"] == key_json(&conv) && !p["features"].is_null()
        })
        .await["features"]
        .clone();
    assert_eq!(features["reply"], "Full");
    assert_eq!(features["edit"]["max_count"], 100);
    assert_eq!(
        features["file_packing"]["albums"][0]["max"], 31,
        "{features}"
    );
    assert!(
        features["disappearing"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty),
        "group timers need group writes: {features}"
    );

    let original = text(&conv, "Who brings the rope?");
    assert_sent(&core.transmit(original.clone()).await, 3);
    for member in &world.members {
        let got = world.got(member, &device, "message", true).await;
        assert_eq!(got.message_type, "SenderKey");
    }
    world.transcript(&device).await;
    let before = world.harness.send_stats().await.expect("stats");

    let reaction = request(
        &conv,
        OutgoingKind::React {
            target: target(&conv, None, sent_at(&original)),
            emoji: "🧗".to_owned(),
            remove: false,
        },
    );
    assert_sent(&core.transmit(reaction.clone()).await, 3);
    for member in &world.members {
        let got = world.got(member, &device, "reaction", true).await;
        assert!(got.sealed);
        assert_eq!(got.message_type, "SenderKey", "over the sender-key path");
        assert_eq!(got.group_id.as_deref(), Some(world.group.id.as_str()));
        let sent = &got.data_message["reaction"];
        assert_eq!(sent["targetSentTimestamp"], sent_at(&original));
        assert_eq!(
            sent["targetAuthorAciBinary"],
            aci_b64(&world.phone.aci),
            "our own message"
        );
        assert!(
            got.data_message["expireTimerVersion"].is_null(),
            "no timer version in groups"
        );
    }
    let transcript = world.transcript(&device).await;
    assert_eq!(transcript["message"]["reaction"]["emoji"], "🧗");
    assert!(transcript["destinationServiceIdBinary"].is_null());
    assert_eq!(
        transcript["unidentifiedStatus"].as_array().map(Vec::len),
        Some(3)
    );

    let edit = request(
        &conv,
        OutgoingKind::Edit {
            target: target(&conv, None, sent_at(&original)),
            body: Body::text("Who brings the rope and the chalk?"),
        },
    );
    assert_sent(&core.transmit(edit.clone()).await, 3);
    for member in &world.members {
        let got = world
            .harness
            .wait_for_edit(
                &member.aci,
                Some(&device.device_ref),
                Some(&world.group.id),
                WAIT,
            )
            .await
            .expect("the edit");
        assert_eq!(got.message_type, "SenderKey");
        assert_eq!(got.target_timestamp, sent_at(&original));
        assert_eq!(got.text, "Who brings the rope and the chalk?");
        assert!(got.data_message["groupV2"]["masterKey"].is_string());
    }
    let transcript = world.transcript(&device).await;
    assert_eq!(
        transcript["editMessage"]["targetSentTimestamp"],
        sent_at(&original)
    );
    assert!(transcript["editMessage"]["dataMessage"]["groupV2"]["masterKey"].is_string());

    let after = world.harness.send_stats().await.expect("stats");
    assert_eq!(
        after.multi,
        before.multi + 2,
        "one multi-recipient send each"
    );

    // A delete while Cal is rate-limited (the server refuses the group token, so everyone gets
    // a 1:1 send): partial, then the follow-up to Cal alone.
    world
        .harness
        .reject_group_send_tokens(true)
        .await
        .expect("reject tokens");
    let cal = &world.members[2];
    world
        .harness
        .rate_limit(&cal.device_ref, Some(1), 1)
        .await
        .expect("rate limit");
    let delete = request(
        &conv,
        OutgoingKind::Delete {
            target: target(&conv, None, sent_at(&original)),
            for_everyone: true,
        },
    );
    let outcome = core.transmit(delete.clone()).await;
    let TransmitOutcome::Partial { delivered, failed } = &outcome else {
        panic!("expected a partial send: {outcome:?}");
    };
    assert_eq!(delivered.len(), 2);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, World::user(cal));
    assert!(matches!(failed[0].1.kind, ErrorKind::RateLimited { .. }));
    for member in &world.members[..2] {
        let got = world.got(member, &device, "delete", true).await;
        assert_eq!(
            got.data_message["delete"]["targetSentTimestamp"],
            sent_at(&original)
        );
        assert!(got.data_message["groupV2"]["masterKey"].is_string());
    }
    let first = world.transcript(&device).await;
    assert_eq!(
        first["unidentifiedStatus"].as_array().map(Vec::len),
        Some(2)
    );
    assert_ne!(first["isRecipientUpdate"], true);
    world
        .harness
        .reject_group_send_tokens(false)
        .await
        .expect("accept tokens");
    let follow_up = delete.resend_to(vec![World::user(cal)]);
    assert_sent(&core.transmit(follow_up).await, 1);
    let got = world.got(cal, &device, "delete", true).await;
    assert_eq!(got.timestamp, sent_at(&delete), "the same delete");
    let update = world.transcript(&device).await;
    assert_eq!(update["isRecipientUpdate"], true, "{update}");
    assert_eq!(
        update["unidentifiedStatus"].as_array().map(Vec::len),
        Some(1)
    );
    world.nothing_more(&world.members[0], &device).await;

    // A delete for me in the group names the group, and no member hears of it.
    let for_me = request(
        &conv,
        OutgoingKind::Delete {
            target: target(&conv, None, sent_at(&original)),
            for_everyone: false,
        },
    );
    assert_sent(&core.transmit(for_me).await, 0);
    let sync = world
        .harness
        .wait_for_sync_message(
            &world.phone.aci,
            Some(&device.device_ref),
            "deleteForMe",
            WAIT,
        )
        .await
        .expect("deleteForMe sync");
    let deletes = &sync.sync_message["content"]["deleteForMe"]["messageDeletes"];
    let printed = deletes.to_string();
    assert!(printed.contains("threadGroupId"), "{printed}");
    assert_eq!(
        deletes[0]["messages"][0]["sentTimestamp"],
        sent_at(&original)
    );

    // A group's timer is part of the group state: changing it is refused, with a hint.
    let (reply, answer) = tokio::sync::oneshot::channel();
    core.send(Command::ChangeConversation {
        conv: conv.clone(),
        change: ConversationChange::SetTimer {
            timer: Some(Duration::from_secs(60)),
        },
        id: Some(allocate(&conv)),
        reply,
    })
    .await;
    let refused = answer.await.expect("answer").expect_err("refused");
    assert_eq!(refused.kind, ErrorKind::Unsupported);
    world.shutdown(core).await;
}

/// Incoming, both chat kinds, through the real receive pipeline: reactions, edits (of a text
/// and of a caption), deletes and quotes from a contact, in a group over sender keys, and from
/// our phone as transcripts; what a hostile member sends about someone else's message is
/// reported under the sender's own name, so the core can never match it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incoming_modifiers_are_mapped_for_both_chat_kinds() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let [ada, bo, _cal] = &world.members;
    let ada_user = World::user(ada);
    let bo_user = World::user(bo);
    let direct = World::direct(ada);
    let group = world.group_conv();
    let me = &device.device_ref;
    let send = |args: Value| async {
        let mut args = args;
        args["awaitDelivery"] = json!(true);
        args["timeout"] = json!(60_000);
        world
            .harness
            .send_data(args)
            .await
            .expect("sendData")
            .timestamp
    };
    let target_of = |event: &Value| {
        (
            event["target"]["author"].clone(),
            event["target"]["local"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    };
    let by_user = |user: &UserId| json!({ "User": user.as_str() });

    // -- 1:1 from Ada: a message, a reaction to it (by her), an edit, a quote, a delete
    let hers = world.says(ada, &device, "see you at 5").await;
    send(json!({
        "from": ada.aci, "to": me, "sealed": true,
        "reaction": { "emoji": "🎉", "targetAuthor": ada.aci, "targetTimestamp": hers },
    }))
    .await;
    let reaction = world
        .sink
        .wait_event_value("Reaction", |e| e["emoji"] == "🎉")
        .await;
    assert_eq!(reaction["by"], ada_user.as_str(), "{reaction}");
    assert_eq!(target_of(&reaction), (by_user(&ada_user), hers.to_string()));
    assert_eq!(reaction["target"]["conv"], key_json(&direct));
    assert_ne!(reaction["remove"], true);

    let edited_at = send(json!({
        "from": ada.aci, "to": me, "text": "see you at 6", "edit": { "targetTimestamp": hers },
    }))
    .await;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], edited_at))
        .await;
    assert_eq!(edit["by"], ada_user.as_str());
    assert_eq!(target_of(&edit), (by_user(&ada_user), hers.to_string()));
    assert_eq!(edit["body"]["Text"]["body"], "see you at 6", "{edit}");
    assert_eq!(
        edit["keeps_quote"], false,
        "the edit names no quote: {edit}"
    );

    let reply = send(json!({
        "from": ada.aci, "to": me, "text": "or at 7?",
        "quote": { "id": hers, "author": ada.aci, "text": "see you at 6",
                   "attachments": [{ "contentType": "image/png", "fileName": "map.png" }] },
    }))
    .await;
    let message = world
        .sink
        .wait_message_value(ada_user.as_str(), &reply.to_string())
        .await;
    let quote = &message["reply_to"];
    assert_eq!(quote["target"]["local"], hers.to_string(), "{quote}");
    assert_eq!(quote["author"], ada_user.as_str());
    assert_eq!(quote["snapshot"]["text"]["body"], "see you at 6");
    assert_eq!(quote["snapshot"]["attachment"], "Image");
    assert_eq!(quote["snapshot"]["file_name"], "map.png");
    assert_eq!(quote["snapshot"]["mime"], "image/png");

    // An edit of that reply says whether it still carries the quote: Desktop and Android keep
    // the original's quote only when the edit has one, so an edit can drop a quote and cannot
    // change it (`SD/ts/util/handleEditMessage.preload.ts` `editMessageHasQuote`; Android
    // `messages/EditMessageProcessor.kt`). The core hears which it is and shows what the
    // other clients show.
    let with_quote = send(json!({
        "from": ada.aci, "to": me, "text": "or at 8?", "edit": { "targetTimestamp": reply },
        "quote": { "id": hers, "author": ada.aci, "text": "see you at 6" },
    }))
    .await;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], with_quote))
        .await;
    assert_eq!(target_of(&edit).1, reply.to_string());
    assert_eq!(edit["keeps_quote"], true, "{edit}");
    let without = send(json!({
        "from": ada.aci, "to": me, "text": "at 8.", "edit": { "targetTimestamp": reply },
    }))
    .await;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], without))
        .await;
    assert_eq!(edit["keeps_quote"], false, "the quote goes: {edit}");

    send(json!({ "from": ada.aci, "to": me, "delete": { "targetTimestamp": hers } })).await;
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| is_id(&e["target"]["local"], hers))
        .await;
    assert_eq!(deleted["for_everyone"], true);
    assert_eq!(deleted["by"], ada_user.as_str());
    assert_eq!(target_of(&deleted).0, by_user(&ada_user));

    // -- a photo with a caption, then the caption edited: a media revision
    let photo = world
        .harness
        .send_attachments(&support::harness::SendAttachments {
            from: ada.aci.clone(),
            to: me.clone(),
            attachments: vec![support::harness::OutgoingFile::new(
                b"not really a png",
                "image/png",
            )],
            text: Some("the old caption".to_owned()),
            await_delivery: true,
            timeout: Some(60_000),
            ..Default::default()
        })
        .await
        .expect("photo")
        .timestamp;
    let recaption = world
        .harness
        .send_attachments(&support::harness::SendAttachments {
            from: ada.aci.clone(),
            to: me.clone(),
            attachments: vec![support::harness::OutgoingFile::new(
                b"not really a png",
                "image/png",
            )],
            text: Some("a better caption".to_owned()),
            edit_target: Some(photo),
            await_delivery: true,
            timeout: Some(60_000),
            ..Default::default()
        })
        .await
        .expect("caption edit")
        .timestamp;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], recaption))
        .await;
    assert_eq!(target_of(&edit).1, photo.to_string());
    assert_eq!(
        edit["body"]["Media"]["caption"]["body"], "a better caption",
        "{edit}"
    );

    // -- the group over sender keys: Ada's message, Bo's reaction to it, Ada's edit
    let to_us = json!([me]);
    let in_group = send(json!({
        "from": ada.aci, "to": to_us, "group": world.group.id, "senderKey": true,
        "text": "rope is packed",
    }))
    .await;
    world
        .sink
        .wait_message(ada_user.as_str(), &in_group.to_string(), WAIT)
        .await
        .expect("group message");
    send(json!({
        "from": bo.aci, "to": to_us, "group": world.group.id, "senderKey": true,
        "reaction": { "emoji": "👏", "targetAuthor": ada.aci, "targetTimestamp": in_group },
    }))
    .await;
    let reaction = world
        .sink
        .wait_event_value("Reaction", |e| e["emoji"] == "👏")
        .await;
    assert_eq!(reaction["by"], bo_user.as_str());
    assert_eq!(
        target_of(&reaction),
        (by_user(&ada_user), in_group.to_string())
    );
    assert_eq!(reaction["target"]["conv"], key_json(&group));
    let group_edit = send(json!({
        "from": ada.aci, "to": to_us, "group": world.group.id, "senderKey": true,
        "text": "rope and chalk are packed", "edit": { "targetTimestamp": in_group },
    }))
    .await;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], group_edit))
        .await;
    assert_eq!(edit["target"]["conv"], key_json(&group));
    assert_eq!(edit["by"], ada_user.as_str());

    // -- a hostile member: Bo edits and deletes "Ada's" message, and reacts naming himself as
    // its author. All three are reported with Bo as the target's author (or the author he
    // claimed), so nothing matches her message.
    let hostile_edit = send(json!({
        "from": bo.aci, "to": to_us, "group": world.group.id, "senderKey": true,
        "text": "I never said that", "edit": { "targetTimestamp": in_group },
    }))
    .await;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], hostile_edit))
        .await;
    assert_eq!(target_of(&edit), (by_user(&bo_user), in_group.to_string()));
    assert_eq!(edit["by"], bo_user.as_str());
    send(json!({
        "from": bo.aci, "to": to_us, "group": world.group.id, "senderKey": true,
        "delete": { "targetTimestamp": in_group },
    }))
    .await;
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| {
            is_id(&e["target"]["local"], in_group) && e["by"] == bo_user.as_str()
        })
        .await;
    assert_eq!(
        target_of(&deleted).0,
        by_user(&bo_user),
        "not Ada's message"
    );
    send(json!({
        "from": bo.aci, "to": to_us, "group": world.group.id, "senderKey": true,
        "reaction": { "emoji": "😈", "targetAuthor": bo.aci, "targetTimestamp": in_group },
    }))
    .await;
    let reaction = world
        .sink
        .wait_event_value("Reaction", |e| e["emoji"] == "😈")
        .await;
    assert_eq!(
        target_of(&reaction).0,
        by_user(&bo_user),
        "the author he named"
    );

    // -- from our phone: our own message to Ada, its edit, a reaction to hers, a delete
    let sync = |mut args: Value| {
        args["from"] = json!(world.phone.aci);
        args["to"] = json!(me);
        args["sync"] = json!({ "destination": ada.aci });
        args
    };
    let ours = send(sync(json!({ "text": "on my way", "withProfileKey": true }))).await;
    world
        .sink
        .wait_message("", &ours.to_string(), WAIT)
        .await
        .expect("our message from the phone");
    let own_edit = send(sync(json!({
        "text": "on my way now", "edit": { "targetTimestamp": ours },
    })))
    .await;
    let edit = world
        .sink
        .wait_event_value("Edited", |e| is_id(&e["new_local"], own_edit))
        .await;
    assert!(edit["by"].is_null(), "our own edit: {edit}");
    assert_eq!(target_of(&edit), (json!("Own"), ours.to_string()));
    send(sync(json!({
        "reaction": { "emoji": "🙂", "targetAuthor": ada.aci, "targetTimestamp": reply },
    })))
    .await;
    let reaction = world
        .sink
        .wait_event_value("Reaction", |e| e["emoji"] == "🙂")
        .await;
    assert!(reaction["by"].is_null(), "our own reaction: {reaction}");
    assert_eq!(
        target_of(&reaction),
        (by_user(&ada_user), reply.to_string())
    );
    send(sync(json!({ "delete": { "targetTimestamp": ours } }))).await;
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| is_id(&e["target"]["local"], ours))
        .await;
    assert!(deleted["by"].is_null());
    assert_eq!(deleted["for_everyone"], true);

    // -- sync messages of the phone: delete for me, a view-once message opened there
    world
        .harness
        .send_sync(json!({
            "from": world.phone.aci, "to": me, "awaitDelivery": true,
            "deleteForMe": { "messages": [
                { "conversation": { "aci": ada.aci }, "author": ada.aci, "timestamp": reply },
                { "conversation": { "group": world.group.id }, "author": ada.aci, "timestamp": in_group },
            ], "conversations": [ { "conversation": { "aci": bo.aci } } ] },
        }))
        .await
        .expect("deleteForMe");
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| {
            is_id(&e["target"]["local"], reply) && e["for_everyone"] == false
        })
        .await;
    assert!(deleted["by"].is_null(), "by our own account");
    assert_eq!(deleted["target"]["conv"], key_json(&direct));
    let deleted = world
        .sink
        .wait_event_value("Deleted", |e| {
            is_id(&e["target"]["local"], in_group) && e["for_everyone"] == false
        })
        .await;
    assert_eq!(deleted["target"]["conv"], key_json(&group));
    assert!(
        world.sink.events("ConversationRemoved").is_empty(),
        "a conversation delete has no event to map to"
    );

    let once = send(json!({ "from": ada.aci, "to": me, "text": "", "viewOnce": true })).await;
    let placeholder = world
        .sink
        .wait_message_value(ada_user.as_str(), &once.to_string())
        .await;
    assert_eq!(placeholder["body"]["Unsupported"]["kind_hint"], "view-once");
    assert_eq!(placeholder["view_once"], true);
    assert!(placeholder["raw"].is_null(), "never kept: {placeholder}");
    world
        .harness
        .send_sync(json!({
            "from": world.phone.aci, "to": me, "awaitDelivery": true,
            "viewOnceOpen": { "sender": ada.aci, "timestamp": once },
        }))
        .await
        .expect("viewOnceOpen");
    let viewed = world
        .sink
        .wait_event_value("Receipts", |e| e["kind"] == "Viewed")
        .await;
    assert_eq!(viewed["by"], format!("aci:{}", world.phone.aci), "{viewed}");
    assert_eq!(viewed["targets"][0]["local"], once.to_string());

    // -- content Kourier does not show: one placeholder of its kind each. A group-call
    //    update is none of them: in a group it is a signal of the call's line, outside one
    //    it is nothing (`tests/calls.rs`).
    for (unsupported, hint) in [
        ("contact", "contact"),
        ("payment", "payment"),
        ("giftBadge", "gift-badge"),
        ("poll", "poll"),
        ("pollEnd", "poll-ended"),
        ("storyReply", "story-reply"),
        ("pin", "pin"),
    ] {
        let at = send(json!({ "from": ada.aci, "to": me, "unsupported": unsupported })).await;
        let message = world
            .sink
            .wait_message_value(ada_user.as_str(), &at.to_string())
            .await;
        assert_eq!(
            message["body"]["Unsupported"]["kind_hint"], hint,
            "{unsupported}: {message}"
        );
        assert!(!message["raw"].is_null(), "{unsupported}: kept raw");
    }

    // -- flags: a profile key update shows nothing; a session reset leaves a notice. Nor
    //    does a group-call update in a 1:1 chat show anything: no "Call" bubble any more.
    let before = world.sink.messages_from(ada_user.as_str());
    send(json!({ "from": ada.aci, "to": me, "unsupported": "groupCall" })).await;
    send(json!({ "from": ada.aci, "to": me, "flags": 4, "withProfileKey": true })).await;
    let reset = send(json!({ "from": ada.aci, "to": me, "flags": 1 })).await;
    let notice = world
        .sink
        .wait_message_value(ada_user.as_str(), &reset.to_string())
        .await;
    assert!(
        !notice["body"]["System"]["SessionRefreshed"].is_null(),
        "{notice}"
    );
    assert_eq!(
        world.sink.messages_from(ada_user.as_str()),
        before + 1,
        "only the reset shows"
    );
    world.shutdown(core).await;
}

/// A 1:1 timer, both ways: our change goes out as a timer update with the next version and its
/// transcript and is confirmed to the core with the patch and the notice under the core's id;
/// everything we send afterwards carries the timer and the version; the peer's and the
/// phone's changes follow Desktop's version rules; an ordinary message brings its chat's
/// timer too. A kill between our change and its send still delivers it once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_to_one_timer_changes_both_ways() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let ada = world.ada();
    let ada_user = World::user(ada);
    let conv = World::direct(ada);
    let me = &device.device_ref;
    let key = key_json(&conv);
    let patch_with_version = |version: u64| {
        let key = key.clone();
        world.sink.wait_event_value("ConversationUpsert", move |p| {
            p["key"] == key && p["timer_version"] == version
        })
    };

    // -- our change: version 2 (Desktop's first version is 1), confirmed at once
    let (id, answer) = set_timer(&core, &conv, Some(2)).await;
    answer.expect("the change is accepted");
    let patch = patch_with_version(2).await;
    assert_eq!(patch["timer"]["Set"]["secs"], 2, "{patch}");
    let notice = world.sink.wait_message_value("", &id.local).await;
    assert_eq!(notice["origin"], "OwnThisDevice", "{notice}");
    assert_eq!(notice["sent_at"], id.sent_at);
    assert!(notice["body"]["System"]["TimerChanged"]["by"].is_null());
    assert_eq!(notice["body"]["System"]["TimerChanged"]["timer"]["secs"], 2);
    let got = world.got(ada, &device, "timer", false).await;
    assert_eq!(
        got.timestamp,
        u64::try_from(id.sent_at).unwrap(),
        "the core's id"
    );
    assert_eq!(got.data_message["flags"], 2);
    assert_eq!(got.data_message["expireTimer"], 2);
    assert_eq!(got.data_message["expireTimerVersion"], 2);
    assert!(got.data_message["profileKey"].is_string());
    let transcript = world.transcript(&device).await;
    assert_eq!(transcript["message"]["flags"], 2, "{transcript}");
    assert_eq!(transcript["message"]["expireTimerVersion"], 2);
    assert!(transcript["expirationStartTimestamp"].is_null());
    world
        .files
        .wait_for_i64(
            "the update job is done",
            "SELECT count(*) FROM jobs WHERE kind = 'timer_update'",
            |n| n == Some(0),
        )
        .await;

    // -- what we send now carries the timer and its version
    let message = request(
        &conv,
        OutgoingKind::Message {
            body: Body::text("this will go"),
            reply_to: None,
            expiry: Some(Duration::from_secs(2)),
        },
    );
    assert_sent(&core.transmit(message.clone()).await, 1);
    let got = world.got(ada, &device, "message", false).await;
    assert_eq!(got.data_message["expireTimer"], 2);
    assert_eq!(got.data_message["expireTimerVersion"], 2);
    let transcript = world.transcript(&device).await;
    let started = transcript["expirationStartTimestamp"]
        .as_u64()
        .expect("timer start");
    assert!(
        started >= sent_at(&message),
        "started when sent: {transcript}"
    );
    let reaction = request(
        &conv,
        OutgoingKind::React {
            target: target(&conv, None, sent_at(&message)),
            emoji: "⏳".to_owned(),
            remove: false,
        },
    );
    assert_sent(&core.transmit(reaction).await, 1);
    let got = world.got(ada, &device, "reaction", false).await;
    assert_eq!(
        got.data_message["expireTimer"], 2,
        "a reaction keeps the timer"
    );
    assert_eq!(got.data_message["expireTimerVersion"], 2);
    world.transcript(&device).await;

    // -- the same timer again: nothing goes out
    let (_, answer) = set_timer(&core, &conv, Some(2)).await;
    answer.expect("accepted");
    world.nothing_more(ada, &device).await;

    // -- Ada's change with a newer version applies; an older one does not; one without a
    // version (a client from before timer versions) changes the timer and not our version
    let update = |args: Value| async {
        let mut args = args;
        args["from"] = json!(ada.aci);
        args["to"] = json!(me);
        args["awaitDelivery"] = json!(true);
        world
            .harness
            .send_data(args)
            .await
            .expect("sendData")
            .timestamp
    };
    let theirs = update(json!({ "flags": 2, "expireTimer": 5, "expireTimerVersion": 3 })).await;
    let patch = patch_with_version(3).await;
    assert_eq!(patch["timer"]["Set"]["secs"], 5);
    let notice = world
        .sink
        .wait_message_value(ada_user.as_str(), &theirs.to_string())
        .await;
    assert_eq!(
        notice["body"]["System"]["TimerChanged"]["by"],
        ada_user.as_str(),
        "{notice}"
    );
    let stale = update(json!({ "flags": 2, "expireTimer": 9, "expireTimerVersion": 1 })).await;
    let legacy = update(json!({ "flags": 2, "expireTimer": 7 })).await;
    let patch = {
        let key = key.clone();
        world
            .sink
            .wait_event_value("ConversationUpsert", move |p| {
                p["key"] == key && p["timer"]["Set"]["secs"] == 7
            })
            .await
    };
    assert_eq!(patch["timer_version"], 3, "our version is kept: {patch}");
    let notice = world
        .sink
        .wait_message_value(ada_user.as_str(), &legacy.to_string())
        .await;
    assert_eq!(notice["body"]["System"]["TimerChanged"]["timer"]["secs"], 7);
    assert!(
        world
            .sink
            .message(ada_user.as_str(), &stale.to_string())
            .is_none(),
        "a stale update leaves no notice"
    );
    // An ordinary message of hers without a version states a timer the chat does not take.
    let unversioned = update(json!({ "text": "an old client", "expireTimer": 11 })).await;
    world
        .sink
        .wait_message(ada_user.as_str(), &unversioned.to_string(), WAIT)
        .await
        .expect("the message itself");
    assert!(
        world
            .sink
            .message(ada_user.as_str(), &format!("timer:{unversioned}"))
            .is_none(),
        "no timer change from an ordinary message without a version"
    );
    // An ordinary message of hers states the timer too: version 4, off.
    let plain = update(json!({ "text": "no timer here", "expireTimerVersion": 4 })).await;
    let patch = patch_with_version(4).await;
    assert_eq!(patch["timer"], "Clear", "{patch}");
    let implied = world
        .sink
        .wait_message_value(ada_user.as_str(), &format!("timer:{plain}"))
        .await;
    assert_eq!(implied["sent_at"], plain - 1, "above the message");
    assert!(implied["body"]["System"]["TimerChanged"]["timer"].is_null());
    world
        .sink
        .wait_message(ada_user.as_str(), &plain.to_string(), WAIT)
        .await
        .expect("the message itself");
    assert!(
        world
            .sink
            .message(ada_user.as_str(), &stale.to_string())
            .is_none(),
        "the stale update never applied"
    );

    // -- our phone changes it: an own notice, and our next message has version 5
    let from_phone = world
        .harness
        .send_data(json!({
            "from": world.phone.aci, "to": me, "awaitDelivery": true,
            "flags": 2, "expireTimer": 3, "expireTimerVersion": 5, "withProfileKey": true,
            "sync": { "destination": ada.aci },
        }))
        .await
        .expect("timer transcript")
        .timestamp;
    let patch = patch_with_version(5).await;
    assert_eq!(patch["timer"]["Set"]["secs"], 3);
    let notice = world
        .sink
        .wait_message_value("", &from_phone.to_string())
        .await;
    assert!(
        notice["body"]["System"]["TimerChanged"]["by"].is_null(),
        "{notice}"
    );
    assert!(!notice["origin"]["OwnOtherDevice"].is_null(), "{notice}");
    let next = request(
        &conv,
        OutgoingKind::Message {
            body: Body::text("three seconds"),
            reply_to: None,
            expiry: Some(Duration::from_secs(3)),
        },
    );
    assert_sent(&core.transmit(next).await, 1);
    let got = world.got(ada, &device, "message", false).await;
    assert_eq!(got.data_message["expireTimer"], 3);
    assert_eq!(got.data_message["expireTimerVersion"], 5);
    world.transcript(&device).await;

    // -- a change made while offline is kept across a restart and sent once
    core.send(Command::Disconnect).await;
    let (id, answer) = set_timer(&core, &conv, None).await;
    answer.expect("accepted while offline");
    patch_with_version(6).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let mut core = world.actor(StartMode::Resume);
    core.send(Command::Connect).await;
    core.wait_online().await;
    let got = world.got(ada, &device, "timer", false).await;
    assert_eq!(got.timestamp, u64::try_from(id.sent_at).unwrap());
    assert_eq!(got.data_message["expireTimer"], 0, "off");
    assert_eq!(got.data_message["expireTimerVersion"], 6);
    assert_eq!(
        world.transcript(&device).await["message"]["expireTimerVersion"],
        6
    );

    // -- four weeks is where the official clients' pickers end, not a limit of the wire or of
    // a receiver (`features.rs` has the sources): ten weeks go out like any other timer
    let ten_weeks: u64 = 10 * 7 * 24 * 3600;
    let (id, answer) = set_timer(&core, &conv, Some(ten_weeks)).await;
    answer.expect("ten weeks are accepted");
    let patch = patch_with_version(7).await;
    assert_eq!(patch["timer"]["Set"]["secs"], ten_weeks, "{patch}");
    let got = world.got(ada, &device, "timer", false).await;
    assert_eq!(got.timestamp, u64::try_from(id.sent_at).unwrap());
    assert_eq!(got.data_message["flags"], 2);
    assert_eq!(got.data_message["expireTimer"], ten_weeks);
    assert_eq!(got.data_message["expireTimerVersion"], 7);
    let transcript = world.transcript(&device).await;
    assert_eq!(transcript["message"]["expireTimer"], ten_weeks);
    // The bound that is kept is Android's: it reads the field as a signed 32-bit number, so
    // one second above that is refused before anything is sent, and the largest value goes.
    let (_, answer) = set_timer(&core, &conv, Some(2_147_483_648)).await;
    let refused = answer.expect_err("more than Android reads as it was sent");
    assert_eq!(refused.kind, kourier_model::ErrorKind::InvalidInput);
    world.nothing_more(ada, &device).await;
    let (_, answer) = set_timer(&core, &conv, Some(2_147_483_647)).await;
    answer.expect("the largest timer is accepted");
    patch_with_version(8).await;
    let got = world.got(ada, &device, "timer", false).await;
    assert_eq!(got.data_message["expireTimer"], 2_147_483_647u64);
    world.transcript(&device).await;
    // A long timer another client states is taken as it comes: Ada sets a year.
    let year: u64 = 365 * 24 * 3600;
    let update = json!({
        "from": ada.aci, "to": me, "awaitDelivery": true,
        "flags": 2, "expireTimer": year, "expireTimerVersion": 9,
    });
    world.harness.send_data(update).await.expect("sendData");
    let patch = patch_with_version(9).await;
    assert_eq!(patch["timer"]["Set"]["secs"], year, "{patch}");
    world.nothing_more(ada, &device).await;
    world.shutdown(core).await;
}
