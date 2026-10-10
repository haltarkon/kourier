//! Group send against `@signalapp/mock-server` (docs/architecture.md §6.7 step 5, §11 S11):
//! every member gets Kourier's group text as one sealed sender-key message (multi-recipient,
//! authorized by a group send token the harness verifies), after our distribution message;
//! endorsements outside their window refresh the group first; a member removal starts a new
//! chain the removed member never gets; with the token refused, or an answer the typed API does
//! not know, the members get 1:1 sends, and
//! a rate-limited member gets only the follow-up (`is_recipient_update`); a member that lost our
//! sender key asks for a resend and gets the logged message with our distribution message; group
//! typing goes the sender-key way too.
//!
//! The backend runs as the app runs it (`SignalNetwork::spawn`); the test plays the core
//! (`support::fake_core`). Skips without Node ≥ 24 or the harness's `node_modules`
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kourier_backend_api::{BackendEnv, Command, Exit, InitialSync, StartMode, TransmitOutcome};
use kourier_model::{
    CmdId, ConvKey, ErrorKind, LoginStepKind, MsgTarget, ReceiptKind, RecipientState, TargetAuthor,
    UserId,
};
use kourier_signal::SignalNetwork;
use serde_json::Value;
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env, text_request_in};
use support::harness::{
    CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, ModifyGroup, Primary,
    ReceivedReceipt, SendGroupText, SendText, StorageContact, StorageGroup, StorageSpec,
};

/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(1500);

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// Our phone ("Myself") and the group "Climbing club" with three other members.
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
        let env = local_env(harness.handshake(), tmp.path()).expect("local env");
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

    fn conv(&self) -> ConvKey {
        let id = STANDARD.decode(&self.group.id).expect("group id");
        ConvKey::Group(format!("gv2:{}", URL_SAFE_NO_PAD.encode(id)).into())
    }

    fn user(primary: &Primary) -> UserId {
        UserId::parse(&format!("aci:{}", primary.aci)).expect("user id")
    }

    /// Links a fresh account whose phone has the group (and its members as accepted contacts),
    /// and waits until the group state with its endorsements is stored.
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
        core.wait_online().await;
        self.files
            .wait_for_i64(
                "group state with endorsements",
                "SELECT count(*) FROM groups WHERE revision IS NOT NULL
                   AND endorsements_expire_at IS NOT NULL AND membership = 'member'",
                |v| v == Some(1),
            )
            .await;
        // The core has the group and may write to it (no "not available yet" notice).
        let key = self.conv();
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let writable = self
                .sink
                .event_values("ConversationUpsert")
                .iter()
                .filter(|p| p["key"] == key_json(&key))
                .filter_map(|p| p.get("features").filter(|f| !f.is_null()))
                .next_back()
                .is_some_and(|f| f["cannot_send"].is_null());
            if writable {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the group's features never allowed sending"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        (core, device.expect("linked device"))
    }

    /// Sends `text` to the group and returns the request's timestamp and the outcome.
    async fn say(&self, core: &FakeCore, text: &str) -> (u64, TransmitOutcome) {
        let req = text_request_in(self.conv(), text);
        let sent_at = u64::try_from(req.sent_at).expect("timestamp");
        (sent_at, core.transmit(req).await)
    }

    /// Waits until member `i` got `text` in the group from our device.
    async fn received(
        &self,
        i: usize,
        device: &DeviceInfo,
        text: &str,
    ) -> support::harness::ReceivedMessage {
        let got = self
            .harness
            .wait_for_group_message(
                &self.members[i].aci,
                &self.group.id,
                Some(&device.device_ref),
                WAIT,
            )
            .await
            .unwrap_or_else(|e| panic!("member {i} got no group message: {e}"));
        assert_eq!(got.text, text, "member {i}");
        got
    }

    /// Asserts member `i` gets nothing more in the group for a while.
    async fn nothing_more(&self, i: usize) {
        let more = self
            .harness
            .wait_for_group_message(&self.members[i].aci, &self.group.id, None, QUIET)
            .await;
        assert!(more.is_err(), "member {i} got another message: {more:?}");
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

/// A conversation key as the sink stores it (serde JSON of `ConvKey`).
fn key_json(key: &ConvKey) -> Value {
    serde_json::to_value(key).expect("conv key json")
}

fn sent_to(outcome: &TransmitOutcome) -> Vec<(String, bool)> {
    let TransmitOutcome::Sent { per_recipient, .. } = outcome else {
        panic!("not sent to everyone: {outcome:?}");
    };
    let mut out: Vec<(String, bool)> = per_recipient
        .iter()
        .map(|(user, state)| {
            (
                user.as_str().to_owned(),
                matches!(state, RecipientState::Sent { sealed: true }),
            )
        })
        .collect();
    out.sort();
    out
}

fn acis(world: &World) -> Vec<String> {
    let mut acis: Vec<String> = world.members.iter().map(|m| m.aci.clone()).collect();
    acis.sort();
    acis
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_member_gets_the_text_as_one_sender_key_message() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let before = world.harness.send_stats().await.expect("stats");

    let (sent_at, outcome) = world.say(&core, "Who brings the rope?").await;
    let mut expected: Vec<(String, bool)> = world
        .members
        .iter()
        .map(|m| (World::user(m).as_str().to_owned(), true))
        .collect();
    expected.sort();
    assert_eq!(sent_to(&outcome), expected, "every member, sealed");
    for i in 0..3 {
        let got = world.received(i, &device, "Who brings the rope?").await;
        assert!(got.sealed, "sealed sender");
        assert_eq!(got.message_type, "SenderKey", "a sender-key message");
        assert_eq!(got.timestamp, sent_at);
        assert_eq!(got.group_id.as_deref(), Some(world.group.id.as_str()));
        assert_eq!(
            got.sealed_group_id.as_deref(),
            Some(world.group.id.as_str()),
            "the sealed content names the group"
        );
    }
    let stats = world.harness.send_stats().await.expect("stats");
    assert_eq!(stats.multi, before.multi + 1, "one multi-recipient send");
    let mut recipients: Vec<String> =
        stats.multi_recipients[before.multi_recipients.len()..].to_vec();
    recipients.sort();
    assert_eq!(recipients, acis(&world), "to every member at once");
    for member in &world.members {
        assert_eq!(
            stats.single_to(&member.aci) - before.single_to(&member.aci),
            1,
            "one 1:1 send per member: our distribution message"
        );
        // It prepares a message, so a member who cannot read it asks for it again.
        let distribution = world
            .harness
            .wait_for_distribution(&member.aci, Some(&device.device_ref), WAIT)
            .await
            .expect("distribution message");
        assert!(distribution.sealed);
        assert_eq!(
            distribution.sealed_group_id.as_deref(),
            Some(world.group.id.as_str())
        );
        assert_eq!(distribution.content_hint.as_deref(), Some("resendable"));
    }
    // The phone's transcript: the group message and how each member got it.
    let sent = world.transcript(&device).await;
    assert_eq!(sent["message"]["body"], "Who brings the rope?");
    assert!(sent["message"]["groupV2"]["masterKey"].is_string());
    assert!(
        sent["destinationServiceIdBinary"].is_null(),
        "no 1:1 destination"
    );
    assert_eq!(
        sent["unidentifiedStatus"].as_array().map(Vec::len),
        Some(3),
        "{sent}"
    );
    assert_ne!(sent["isRecipientUpdate"], true);

    // The next message needs no new distribution message.
    let (_, outcome) = world.say(&core, "I do.").await;
    assert_eq!(sent_to(&outcome).len(), 3);
    for i in 0..3 {
        assert_eq!(
            world.received(i, &device, "I do.").await.message_type,
            "SenderKey"
        );
    }
    let after = world.harness.send_stats().await.expect("stats");
    assert_eq!(after.multi, stats.multi + 1);
    for member in &world.members {
        assert_eq!(
            after.single_to(&member.aci),
            stats.single_to(&member.aci),
            "no 1:1 sends for the second message"
        );
    }

    // Group typing goes the sender-key way (online only, not logged) once the phone's setting
    // allows typing indicators.
    world
        .harness
        .send_configuration(&world.phone.device_ref, &device.device_ref, true, true)
        .await
        .expect("configuration");
    world
        .files
        .wait_for_i64(
            "typing indicators on",
            "SELECT typing_indicators FROM account",
            |v| v == Some(1),
        )
        .await;
    core.send(Command::SetTyping {
        conv: world.conv(),
        on: true,
    })
    .await;
    for member in &world.members {
        let typing = world
            .harness
            .wait_for_typing(&member.aci, Some(&device.device_ref), WAIT)
            .await
            .expect("typing");
        assert_eq!(typing.action, "started");
        assert_eq!(typing.group_id.as_deref(), Some(world.group.id.as_str()));
    }
    world.shutdown(core).await;
}

/// A typing indicator as the first thing we send to a group: the distribution message it needs
/// is implicit like the indicator itself (Desktop's `sendToGroup` hands its content hint on),
/// so a member who cannot read it shows nothing and asks for nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typing_sends_distribution_message_is_implicit() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    world
        .harness
        .send_configuration(&world.phone.device_ref, &device.device_ref, true, true)
        .await
        .expect("configuration");
    world
        .files
        .wait_for_i64(
            "typing indicators on",
            "SELECT typing_indicators FROM account",
            |v| v == Some(1),
        )
        .await;
    core.send(Command::SetTyping {
        conv: world.conv(),
        on: true,
    })
    .await;
    for member in &world.members {
        let distribution = world
            .harness
            .wait_for_distribution(&member.aci, Some(&device.device_ref), WAIT)
            .await
            .expect("distribution message");
        assert!(distribution.sealed);
        assert_eq!(
            distribution.sealed_group_id.as_deref(),
            Some(world.group.id.as_str())
        );
        assert_eq!(distribution.content_hint.as_deref(), Some("implicit"));
        let typing = world
            .harness
            .wait_for_typing(&member.aci, Some(&device.device_ref), WAIT)
            .await
            .expect("typing");
        assert_eq!(typing.action, "started");
    }
    world.shutdown(core).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expired_endorsements_refresh_the_group_before_the_send() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    // The stored endorsements expire within the hour: outside the usable window.
    world
        .files
        .execute(&format!(
            "UPDATE groups SET endorsements_expire_at = {}",
            now + 30 * 60_000
        ))
        .expect("age the endorsements");
    let before = world.harness.send_stats().await.expect("stats");
    let (_, outcome) = world.say(&core, "Fresh tokens?").await;
    assert_eq!(sent_to(&outcome).len(), 3, "{outcome:?}");
    for i in 0..3 {
        assert_eq!(
            world
                .received(i, &device, "Fresh tokens?")
                .await
                .message_type,
            "SenderKey"
        );
    }
    let stats = world.harness.send_stats().await.expect("stats");
    assert!(
        stats.group_fetches[&world.group.id] > before.group_fetches[&world.group.id],
        "the group was fetched again before the send"
    );
    assert_eq!(stats.multi, before.multi + 1);
    assert_eq!(stats.refused_tokens, before.refused_tokens, "a valid token");
    let expiry = world
        .files
        .peek_i64("SELECT endorsements_expire_at FROM groups")
        .expect("expiry")
        .expect("stored");
    assert!(
        expiry > now + 2 * 3_600_000,
        "fresh endorsements were stored"
    );
    world.shutdown(core).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rate_limited_member_gets_only_the_follow_up() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    // The server refuses the group send token, so every member gets a 1:1 send; Cal's sends
    // are rate-limited twice (our distribution message, then the text).
    world
        .harness
        .reject_group_send_tokens(true)
        .await
        .expect("reject tokens");
    let cal = &world.members[2];
    world
        .harness
        .rate_limit(&cal.device_ref, Some(2), 1)
        .await
        .expect("rate limit");
    let req = text_request_in(world.conv(), "Meet at the gym");
    let outcome = core.transmit(req.clone()).await;
    let TransmitOutcome::Partial { delivered, failed } = &outcome else {
        panic!("expected a partial send: {outcome:?}");
    };
    let mut delivered: Vec<&str> = delivered.iter().map(|(u, _)| u.as_str()).collect();
    delivered.sort_unstable();
    let mut expected = vec![
        World::user(&world.members[0]).as_str().to_owned(),
        World::user(&world.members[1]).as_str().to_owned(),
    ];
    expected.sort();
    assert_eq!(delivered, expected);
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, World::user(cal));
    assert!(
        matches!(failed[0].1.kind, ErrorKind::RateLimited { .. }),
        "{:?}",
        failed[0].1
    );
    for i in 0..2 {
        let got = world.received(i, &device, "Meet at the gym").await;
        assert_ne!(got.message_type, "SenderKey", "a 1:1 send");
    }
    let first = world.transcript(&device).await;
    assert_eq!(
        first["unidentifiedStatus"].as_array().map(Vec::len),
        Some(2)
    );
    assert_ne!(first["isRecipientUpdate"], true);
    let stats = world.harness.send_stats().await.expect("stats");
    assert!(stats.refused_tokens >= 1, "the token was refused");

    // The core's follow-up: only Cal, as a recipient update.
    world
        .harness
        .reject_group_send_tokens(false)
        .await
        .expect("accept tokens");
    let follow_up = req.resend_to(vec![World::user(cal)]);
    assert!(follow_up.is_recipient_update);
    let outcome = core.transmit(follow_up).await;
    assert_eq!(
        sent_to(&outcome),
        vec![(World::user(cal).as_str().to_owned(), true)]
    );
    let got = world.received(2, &device, "Meet at the gym").await;
    assert_eq!(
        got.timestamp,
        u64::try_from(req.sent_at).unwrap(),
        "same message"
    );
    let update = world.transcript(&device).await;
    assert_eq!(update["isRecipientUpdate"], true, "{update}");
    let statuses = update["unidentifiedStatus"].as_array().expect("statuses");
    assert_eq!(statuses.len(), 1, "only Cal: {update}");
    // Ada and Bo got the message exactly once.
    world.nothing_more(0).await;
    world.nothing_more(1).await;
    world.shutdown(core).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unexpected_sender_key_answer_falls_over_to_1_1_sends() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    // The server answers the multi-recipient send with nothing the typed API knows (as the
    // REST route's 404 for an unknown recipient): Desktop sends 1:1 then, it does not fail.
    world
        .harness
        .answer_multi_recipient_unexpectedly(true)
        .await
        .expect("unexpected answers");
    let before = world.harness.send_stats().await.expect("stats");
    let (sent_at, outcome) = world.say(&core, "Anyone there?").await;
    let mut expected: Vec<(String, bool)> = world
        .members
        .iter()
        .map(|m| (World::user(m).as_str().to_owned(), true))
        .collect();
    expected.sort();
    assert_eq!(sent_to(&outcome), expected, "every member, sealed");
    for i in 0..3 {
        let got = world.received(i, &device, "Anyone there?").await;
        assert_ne!(got.message_type, "SenderKey", "a 1:1 send");
        assert_eq!(got.timestamp, sent_at);
        assert!(got.sealed);
        assert_eq!(
            got.sealed_group_id.as_deref(),
            Some(world.group.id.as_str()),
            "a 1:1 send in a group names the group in its sealed content"
        );
    }
    let stats = world.harness.send_stats().await.expect("stats");
    assert_eq!(stats.unexpected_answers, before.unexpected_answers + 1);
    assert_eq!(stats.multi, before.multi, "nothing went the sender-key way");
    for i in 0..3 {
        world.nothing_more(i).await;
    }
    world.shutdown(core).await;
}

/// §4.6, §6.7 step 5: the server answers the sender-key send with 428 (on the websocket route
/// of multi-recipient sends, which a client uses while the remote config keeps them there).
/// libsignal-net takes no challenge on an unauthenticated request and reports the status
/// without the token, so nothing could be answered from it: the members get 1:1 sends, and the
/// 428 the server gives those (it challenges the account, whatever the route) is a challenge
/// for the user with its token and the server's retry time. Nothing is reported as an invalid
/// answer, nobody gets the message twice, and once the server lifted the challenge the same
/// message goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_challenged_sender_key_send_is_a_challenge_with_its_retry_time() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .set_remote_config(
            "desktop.libsignalNet.grpc.MessagesAnonymousSendMultiRecipientMessage.2",
            true,
            Some("ws"),
        )
        .await
        .expect("multi-recipient sends over the websocket");
    let (core, device) = world.link().await;
    world
        .harness
        .challenge_multi_recipient("group-challenge", 45)
        .await
        .expect("challenge");
    // Only the sender-key send is refused: the message still reaches everyone, 1:1.
    let (first_at, first) = world.say(&core, "First try").await;
    assert_eq!(sent_to(&first).len(), 3, "{first:?}");
    for i in 0..3 {
        let got = world.received(i, &device, "First try").await;
        assert_ne!(got.message_type, "SenderKey", "a 1:1 send");
        assert_eq!(got.timestamp, first_at);
    }
    // Now the account itself is challenged: the 1:1 sends are refused too.
    for member in &world.members {
        world
            .harness
            .challenge_for(&member.device_ref, "account-challenge", 45)
            .await
            .expect("challenge");
    }
    let req = text_request_in(world.conv(), "Anyone there?");
    let sent_at = u64::try_from(req.sent_at).expect("timestamp");
    let outcome = core.transmit(req.clone()).await;
    let errors: Vec<&kourier_model::BackendError> = match &outcome {
        TransmitOutcome::Failed(error) => vec![error],
        TransmitOutcome::Partial { delivered, failed } => {
            assert!(delivered.is_empty(), "nobody got it: {delivered:?}");
            assert_eq!(failed.len(), 3, "every member waits for the challenge");
            failed.iter().map(|(_, error)| error).collect()
        }
        other @ TransmitOutcome::Sent { .. } => panic!("{other:?}"),
    };
    for error in errors {
        let ErrorKind::Challenge {
            token,
            url,
            retry_after,
        } = &error.kind
        else {
            panic!("not a challenge: {error:?}");
        };
        assert_eq!(
            token.expose(),
            "account-challenge",
            "a token to answer with"
        );
        assert!(url.starts_with("https://"), "{url}");
        assert_eq!(
            *retry_after,
            Some(Duration::from_secs(45)),
            "the server's Retry-After reaches the core"
        );
    }
    for i in 0..3 {
        world.nothing_more(i).await;
    }
    // The server lifts the account's challenge: the core's next try of the same message
    // reaches everyone, once (still 1:1: the mock has no websocket route of its own for the
    // sender-key send).
    for member in &world.members {
        world
            .harness
            .stop_challenge(&member.device_ref)
            .await
            .expect("lift");
    }
    let outcome = core.transmit(req).await;
    let mut expected: Vec<(String, bool)> = world
        .members
        .iter()
        .map(|m| (World::user(m).as_str().to_owned(), true))
        .collect();
    expected.sort();
    assert_eq!(sent_to(&outcome), expected);
    for i in 0..3 {
        let got = world.received(i, &device, "Anyone there?").await;
        assert_eq!(got.timestamp, sent_at);
        world.nothing_more(i).await;
    }
    assert_eq!(
        world
            .harness
            .stop_challenge_multi_recipient()
            .await
            .expect("stop"),
        3,
        "each of the three sender-key sends met the 428"
    );
    world.shutdown(core).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unconfirmed_safety_number_change_holds_back_only_that_member() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let (_, outcome) = world.say(&core, "Hello all").await;
    assert_eq!(sent_to(&outcome).len(), 3);
    for i in 0..3 {
        world.received(i, &device, "Hello all").await;
    }
    // Cal was verified and his safety number changed; the user has not confirmed it yet
    // (`UNVERIFIED`). The sender-key encryption checks no trust, so Kourier must.
    let cal = &world.members[2];
    let changed = world
        .files
        .execute(&format!(
            "UPDATE identities SET verified = 2 WHERE service_id = '{}'",
            cal.aci
        ))
        .expect("unverify Cal");
    assert_eq!(changed, 1, "Kourier knows Cal's identity");
    let (_, outcome) = world.say(&core, "Second round").await;
    let TransmitOutcome::Partial { delivered, failed } = &outcome else {
        panic!("expected a partial send: {outcome:?}");
    };
    assert_eq!(delivered.len(), 2, "{outcome:?}");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].0, World::user(cal));
    assert!(
        matches!(&failed[0].1.kind, ErrorKind::UntrustedIdentity { user } if *user == World::user(cal)),
        "{:?}",
        failed[0].1
    );
    for i in 0..2 {
        assert_eq!(
            world
                .received(i, &device, "Second round")
                .await
                .message_type,
            "SenderKey"
        );
    }
    world.nothing_more(2).await;
    world.shutdown(core).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_who_lost_our_sender_key_gets_a_resend_with_it() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let (_, outcome) = world.say(&core, "First").await;
    assert_eq!(sent_to(&outcome).len(), 3);
    for i in 0..3 {
        world.received(i, &device, "First").await;
    }
    // Ada loses our sender key; the next sender-key message fails to decrypt on her phone.
    let ada = &world.members[0];
    let forgotten = world
        .harness
        .break_sender_keys(&ada.aci, &device.device_ref)
        .await
        .expect("break sender keys");
    assert!(forgotten >= 1);
    let (sent_at, outcome) = world.say(&core, "Second").await;
    assert_eq!(sent_to(&outcome).len(), 3);
    for i in 1..3 {
        world.received(i, &device, "Second").await;
    }
    let failure = world
        .harness
        .wait_for_decrypt_failure(&ada.aci, WAIT)
        .await
        .expect("Ada could not decrypt");
    assert_eq!(failure.timestamp, sent_at);
    assert_eq!(failure.envelope_type, "SenderKey");
    // Her retry request is answered from the Message Send Log, with our distribution message.
    let again = world.received(0, &device, "Second").await;
    assert_eq!(again.timestamp, sent_at, "the original timestamp");
    assert!(again.sender_key_distribution, "our sender key came with it");
    assert_ne!(again.message_type, "SenderKey", "a 1:1 resend");
    assert!(again.sealed);
    assert_eq!(
        again.sealed_group_id.as_deref(),
        Some(world.group.id.as_str()),
        "the resend names the group in its sealed content"
    );
    // And the next sender-key message decrypts again.
    world.say(&core, "Third").await;
    assert_eq!(
        world.received(0, &device, "Third").await.message_type,
        "SenderKey"
    );
    world.shutdown(core).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_removal_starts_a_new_chain_without_them() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let (_, outcome) = world.say(&core, "Before").await;
    assert_eq!(sent_to(&outcome).len(), 3);
    for i in 0..3 {
        world.received(i, &device, "Before").await;
    }
    // The phone removes Cal and tells our device.
    let cal = &world.members[2];
    world
        .harness
        .modify_group(&ModifyGroup {
            from: world.phone.device_ref.clone(),
            group: world.group.id.clone(),
            remove_members: vec![cal.device_ref.clone()],
            to: vec![device.device_ref.clone()],
            ..ModifyGroup::default()
        })
        .await
        .expect("remove Cal");
    world
        .files
        .wait_for_i64("the removal applied", "SELECT revision FROM groups", |v| {
            v == Some(1)
        })
        .await;
    let before = world.harness.send_stats().await.expect("stats");
    let (_, outcome) = world.say(&core, "After").await;
    let mut expected: Vec<(String, bool)> = world.members[..2]
        .iter()
        .map(|m| (World::user(m).as_str().to_owned(), true))
        .collect();
    expected.sort();
    assert_eq!(sent_to(&outcome), expected, "Cal is no recipient");
    for i in 0..2 {
        assert_eq!(
            world.received(i, &device, "After").await.message_type,
            "SenderKey"
        );
    }
    world.nothing_more(2).await;
    let stats = world.harness.send_stats().await.expect("stats");
    for member in &world.members[..2] {
        assert_eq!(
            stats.single_to(&member.aci) - before.single_to(&member.aci),
            1,
            "a new distribution message for the new chain"
        );
    }
    assert_eq!(stats.single_to(&cal.aci), before.single_to(&cal.aci));
    world.shutdown(core).await;
}

/// The next read or viewed receipt `member` got from our device (delivery receipts, which the
/// backend sends on its own, are passed over).
async fn read_receipt(world: &World, member: &Primary, me: &str) -> ReceivedReceipt {
    loop {
        let receipt = world
            .harness
            .wait_for_receipt(&member.aci, Some(me), WAIT)
            .await
            .expect("a read receipt");
        if receipt.kind != "delivery" {
            return receipt;
        }
    }
}

/// §6.7 step 6: a read receipt for a group message, as it is on the wire. Desktop sends it
/// 1:1 to the author, sealed and resendable, with no group in the sealed content
/// (`SD/ts/textsecure/SendMessage.preload.ts` `#sendReceiptMessage`: `sendIndividualProto`
/// with `ContentHint.Resendable` and no `groupId`), and without our PNI signature, which it
/// adds only `if (isDirectConversation)`. So the author learns nothing from the receipt's
/// envelope about where the message was read, and a member who knows us by number only gets
/// no proof of our number from a group. The same author's 1:1 message, read, does carry it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_read_receipt_names_no_group_and_carries_no_pni_signature() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let me = device.device_ref.clone();
    let ada = &world.members[0];
    let ada_user = World::user(ada);
    let harness = &world.harness;
    let target = |conv: &ConvKey, timestamp: u64| MsgTarget {
        conv: Some(conv.clone()),
        author: TargetAuthor::User(ada_user.clone()),
        local: timestamp.to_string().into(),
        alternates: Vec::new(),
    };

    // Ada writes to our number (our PNI): from now on what we send her 1:1 carries our PNI
    // signature, until she answers our ACI.
    let by_number = harness
        .send_text(&SendText {
            service_id_kind: Some("PNI".to_owned()),
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&ada.aci, &me, "is this your number?")
        })
        .await
        .expect("sendText to our PNI")
        .timestamp;
    world
        .files
        .wait_for_i64(
            "she knows us by number",
            &format!(
                "SELECT share_my_number FROM peers WHERE service_id = '{}'",
                ada.aci
            ),
            |v| v == Some(1),
        )
        .await;
    // And she writes in the group.
    let in_group = harness
        .send_group_text(&SendGroupText {
            from: ada.aci.clone(),
            group: world.group.id.clone(),
            to: vec![me.clone()],
            text: "Who brings the rope?".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("group text")
        .timestamp;
    for timestamp in [by_number, in_group] {
        world
            .sink
            .wait_message(ada_user.as_str(), &timestamp.to_string(), WAIT)
            .await
            .expect("her message reached the core");
    }

    // The user reads the group message.
    let group = world.conv();
    core.send(Command::SendReceipts {
        cmd: CmdId(1),
        conv: group.clone(),
        targets: vec![target(&group, in_group)],
        kind: ReceiptKind::Read,
    })
    .await;
    let receipt = read_receipt(&world, ada, &me).await;
    assert_eq!(receipt.kind, "read");
    assert_eq!(receipt.timestamps, [in_group]);
    assert!(receipt.sealed, "sealed, as Desktop sends receipts");
    assert_eq!(
        receipt.sealed_group_id, None,
        "the sealed content names no group"
    );
    assert_eq!(receipt.content_hint.as_deref(), Some("resendable"));
    assert!(
        !receipt.pni_signature,
        "no PNI signature rides on a receipt for a group message"
    );

    // Her 1:1 message, read: this receipt does carry the signature, so the harness would
    // have seen one above.
    let direct = ConvKey::Direct(ada_user.clone());
    core.send(Command::SendReceipts {
        cmd: CmdId(2),
        conv: direct.clone(),
        targets: vec![target(&direct, by_number)],
        kind: ReceiptKind::Read,
    })
    .await;
    let receipt = read_receipt(&world, ada, &me).await;
    assert_eq!(receipt.kind, "read");
    assert_eq!(receipt.timestamps, [by_number]);
    assert!(
        receipt.pni_signature,
        "a 1:1 receipt to someone who knows us by number carries it"
    );
    assert_eq!(receipt.sealed_group_id, None);
    world.shutdown(core).await;
}
