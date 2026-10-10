//! The Signal backend against `@signalapp/mock-server` (docs/architecture.md §10.3, §11 S2, S6
//! and S7): link as a secondary device (sync requests through the job queue, remote config with
//! ETag, `AccountReady` first), receive texts (sealed and unsealed, and a PreKey message to our
//! PNI) exactly once through stage 1, the stage-2 lanes and the `core_outbox` forwarder into a
//! fake core, give the uncrashed state after a kill at every failpoint of the receive path,
//! reply, resume after a restart, keep the previous QR code valid across a rotation, and refuse
//! a relink to another account before `PUT /v1/devices/link`.
//!
//! The backend runs exactly as the app will run it: `SignalNetwork::spawn` on its own thread
//! with a current-thread runtime, network I/O on the test's multi-thread runtime. The test plays
//! the core (`support::fake_core`): it commits envelope events to a scratch SQLCipher table and
//! completes the `Ack` afterwards.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (set `KOURIER_REQUIRE_MOCK_HARNESS=1` to fail instead).

mod support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{BackendEnv, Command, Exit, Relink, StartMode, TransmitOutcome};
use kourier_model::{AccountId, ErrorKind, LoginStepKind, RecipientState, UserId};
use kourier_signal::SignalNetwork;
use serde::{Deserialize, Serialize};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env, text_request};
use support::harness::{CreatePrimary, DeviceInfo, Harness, Primary, SendText};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// Set (to a JSON [`ChildSpec`]) in the environment of the helper process that runs the actor
/// for the kill test; [`child_actor_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_SIGNAL_TEST_CHILD";
/// Prefix of the helper process's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-CHILD";
/// The attachment limit the mock's remote config announces (mapped into `AccountCaps`).
const MAX_ATTACHMENT_BYTES: &str = "104857600";
/// Jobs other than the periodic ones (remote config, MSL purge, profile refresh, key
/// maintenance once it ran), i.e. work still owed to the server.
const PENDING_JOBS: &str = "SELECT count(*) FROM jobs
     WHERE kind NOT IN ('remote_config', 'msl_purge', 'profile_refresh')
       AND NOT (kind = 'key_maintenance' AND next_at > unixepoch() * 1000)";

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// A harness with three scripted accounts: our own phone ("Myself", which links us), and two
/// contacts.
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    alice: Primary,
    bob: Primary,
    sink: Sink,
    files: AccountFiles,
}

impl World {
    async fn start() -> Option<Self> {
        init_tracing();
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        harness
            .set_remote_config(
                "global.attachments.maxBytes",
                true,
                Some(MAX_ATTACHMENT_BYTES),
            )
            .await
            .expect("remote config");
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = local_env(harness.handshake(), tmp.path()).expect("local env");
        let alice = harness
            .create_primary(&CreatePrimary::named("Alice"))
            .await
            .expect("Alice");
        let bob = harness
            .create_primary(&CreatePrimary::named("Bob"))
            .await
            .expect("Bob");
        let phone = harness
            .create_primary(&CreatePrimary {
                profile_name: "Myself".to_owned(),
                contacts: vec![alice.device_ref.clone()],
                ..CreatePrimary::default()
            })
            .await
            .expect("Myself");
        let sink = Sink::open(&tmp.path().join("sink.db")).expect("sink");
        let files = AccountFiles::new(tmp.path()).expect("account dir");
        Some(Self {
            harness,
            tmp,
            env,
            phone,
            alice,
            bob,
            sink,
            files,
        })
    }

    fn user(primary: &Primary) -> UserId {
        UserId::parse(&format!("aci:{}", primary.aci)).expect("user id")
    }

    /// (a): links a fresh account to "Myself" and checks every step of §6.5.
    async fn link(&self) -> (FakeCore, DeviceInfo) {
        let mut core = FakeCore::start(&self.env, &self.files, StartMode::Fresh, &self.sink)
            .expect("spawn the actor");
        let mut device = None;
        let (url, steps) = core
            .link(|url| {
                let device = &mut device;
                async move {
                    // completeProvision resolves once we linked and uploaded one-time prekeys.
                    *device = Some(
                        self.harness
                            .complete_provision(&url, &self.phone.device_ref, WAIT)
                            .await
                            .expect("completeProvision"),
                    );
                }
            })
            .await;
        let device = device.expect("linked device");

        assert!(url.starts_with("sgnl://linkdevice?uuid="), "QR URL");
        // The offer of the history transfer is off unless a network switches it on (§14.2
        // item 14: off in the app until the import exists), so the URL offers nothing.
        assert!(
            url.contains("&pub_key=") && url.ends_with("&capabilities="),
            "QR URL"
        );
        let kinds: Vec<&str> = steps.iter().map(|s| s.step_id.as_str()).collect();
        assert_eq!(kinds, ["device-name", "qr", "linking", "syncing", "done"]);
        assert!(matches!(steps[4].kind, LoginStepKind::Done));
        assert_eq!(device.aci, self.phone.aci);
        assert!(device.device_id > 1, "a secondary device");

        // `AccountReady` reached the core (through core_outbox) before `Done`.
        let ready = self.sink.events("AccountReady");
        assert_eq!(ready.len(), 1, "one AccountReady");
        let detail = ready[0].clone().unwrap_or_default();
        assert!(
            detail.contains(&format!("aci:{}", self.phone.aci)),
            "{detail}"
        );
        assert!(detail.contains("pni:"), "{detail}");
        assert!(
            detail.ends_with(&format!("device {}", device.device_id)),
            "{detail}"
        );

        // `AccountReady` is the first durable event of the link (§4.3 step 2).
        assert_eq!(
            self.sink.event_names().first().map(String::as_str),
            Some("AccountReady")
        );

        // `Done` came after the job queue sent CONTACTS, CONFIGURATION and BLOCKED (§6.5 step
        // 8): checked right after `Done`, before anything else waits (nothing re-adds sync
        // requests; the mock phone sends its account entropy pool, so no KEYS request).
        assert_eq!(
            self.files
                .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'sync_request'")
                .expect("jobs"),
            Some(0),
            "sync requests were sent before Done"
        );
        assert_eq!(
            self.files
                .peek_i64("SELECT post_link_sync = 'done' FROM account")
                .expect("account"),
            Some(1)
        );
        // The phone received them.
        self.harness
            .wait_for_sync(&self.phone.device_ref, &device.device_ref, WAIT)
            .await
            .expect("sync requests reached the phone");
        core.wait_online().await;
        // The phone's sync answers are acknowledged (and dropped by stage 2 until S8).
        self.harness
            .wait_for_deliveries(&device.device_ref, WAIT)
            .await
            .expect("sync answers delivered");

        // Remote config fetched with its ETag; its limits reached the core as `AccountCaps`.
        self.files
            .wait_for_i64(
                "remote config",
                "SELECT remote_config_at FROM account",
                |at| at.is_some(),
            )
            .await;
        let caps = self.sink.wait_events("AccountCaps", 1, WAIT).await;
        assert_eq!(
            caps[0].as_deref(),
            Some("max_attachment_bytes=Some(104857600) long_text_bytes=Some(2048)")
        );
        assert!(
            self.files
                .peek_i64("SELECT remote_config_etag IS NOT NULL FROM account")
                .expect("etag")
                == Some(1),
            "the ETag is stored"
        );
        self.files
            .wait_for_i64("jobs done", PENDING_JOBS, |n| n == Some(0))
            .await;
        // The phone has accepted the chats with both contacts (sends need it, §6.7 M1 rule).
        for contact in [&self.alice, &self.bob] {
            self.accept(&device, contact).await;
        }
        (core, device)
    }

    /// Our phone accepts the chat with `contact` (a `MessageRequestResponse` sync); waits until
    /// the backend stored it.
    async fn accept(&self, device: &DeviceInfo, contact: &Primary) {
        self.harness
            .send_message_request_response(
                &self.phone.device_ref,
                &device.device_ref,
                &contact.aci,
                "accept",
            )
            .await
            .expect("message request response");
        self.files
            .wait_for_i64(
                "chat accepted",
                &format!(
                    "SELECT profile_sharing FROM peers WHERE service_id = '{}'",
                    contact.aci
                ),
                |v| v == Some(1),
            )
            .await;
    }

    /// Sends a text from `from` to our device and waits until the device acked it.
    async fn send_text(
        &self,
        from: &Primary,
        device: &DeviceInfo,
        text: &str,
        sealed: bool,
    ) -> u64 {
        let sent = self
            .harness
            .send_text(&SendText {
                sealed,
                await_delivery: true,
                timeout: Some(60_000),
                ..SendText::new(&from.aci, &device.device_ref, text)
            })
            .await
            .expect("sendText");
        sent.timestamp
    }

    /// Waits until the message reached the sink and checks it was delivered exactly once.
    async fn expect_once(&self, from: &Primary, device: &DeviceInfo, timestamp: u64, text: &str) {
        let author = Self::user(from);
        let local = timestamp.to_string();
        let message = self
            .sink
            .wait_message(author.as_str(), &local, WAIT)
            .await
            .unwrap_or_else(|| panic!("{text:?} reached the core"));
        assert_eq!(message.body, text);
        assert_eq!(
            message.conv,
            format!("direct:{}", author.as_str()),
            "a 1:1 conversation keyed by the ACI"
        );
        assert_eq!(message.device, Some(from.device_id));
        // Nothing addressed to us is left, so no second copy can still be on its way.
        self.harness
            .wait_for_deliveries(&device.device_ref, WAIT)
            .await
            .expect("deliveries settled");
        assert_eq!(
            self.sink.deliveries(author.as_str(), &local),
            1,
            "exactly once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn link_receive_and_reply() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;

    // (b) Unsealed and sealed texts arrive exactly once, after the catch-up boundary.
    let unsealed = world
        .send_text(&world.alice, &device, "hello, unsealed", false)
        .await;
    world
        .expect_once(&world.alice, &device, unsealed, "hello, unsealed")
        .await;
    let sealed = world
        .send_text(&world.alice, &device, "hello, sealed", true)
        .await;
    world
        .expect_once(&world.alice, &device, sealed, "hello, sealed")
        .await;
    let author = World::user(&world.alice);
    assert!(
        !world
            .sink
            .message(author.as_str(), &unsealed.to_string())
            .expect("message")
            .catch_up,
        "a live message is not catch-up"
    );
    assert_eq!(world.sink.messages_from(author.as_str()), 2);
    core.wait_online().await;

    // (d) Replies: to Alice over the session her messages built, to Bob after a prekey fetch.
    let (sent_at, outcome) = core.transmit_text(&author, "reply from Kourier").await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Alice got the reply");
    assert_eq!(got.text, "reply from Kourier");
    assert_eq!(got.timestamp, u64::try_from(sent_at).unwrap_or(0));
    assert_eq!(got.source_device, device.device_id);
    // Alice never shared her profile key: the zero access key is refused (401), sealed sender
    // is off for her and the message went unsealed in the same transmit.
    assert!(!got.sealed, "unsealed fallback");
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT sealed_sender FROM peers WHERE service_id = '{}'",
                world.alice.aci
            ))
            .expect("peers"),
        Some(2),
        "sealed sender disabled after the 401"
    );

    let (sent_at, outcome) = core
        .transmit_text(&World::user(&world.bob), "hello Bob")
        .await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&world.bob.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Bob got the message");
    assert_eq!(got.text, "hello Bob");
    assert_eq!(got.timestamp, u64::try_from(sent_at).unwrap_or(0));
    assert_eq!(
        got.message_type, "PreKey",
        "a new session from Bob's prekeys"
    );

    // 409 repair (§6.7 step 3): Carol's message builds a session with her phone only; she also
    // has a linked device, so the server answers the reply with "missing device 2", and the
    // send fetches that device's prekeys and goes out again to both devices.
    let carol = world
        .harness
        .create_primary(&CreatePrimary {
            linked_devices: Some(1),
            ..CreatePrimary::named("Carol")
        })
        .await
        .expect("Carol");
    let from_carol = world
        .send_text(&carol, &device, "hi from Carol", false)
        .await;
    world
        .expect_once(&carol, &device, from_carol, "hi from Carol")
        .await;
    world.accept(&device, &carol).await;
    let (sent_at, outcome) = core
        .transmit_text(&World::user(&carol), "reply to both devices")
        .await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&carol.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("Carol got the reply");
    assert_eq!(got.text, "reply to both devices");
    assert_eq!(got.timestamp, u64::try_from(sent_at).unwrap_or(0));

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(
        world.files.session_devices(&carol.aci).expect("sessions"),
        [1, 2],
        "the missing device got a session from the 409 repair"
    );
    drop(world.tmp);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn account_survives_a_restart() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let fetched_at = world
        .files
        .peek_i64("SELECT remote_config_at FROM account")
        .expect("account")
        .expect("fetched once");
    // Forget the stored entries but keep the ETag: only a 304 leaves the table empty.
    world
        .files
        .execute("DELETE FROM remote_config")
        .expect("clear remote config");

    // (e) Reopen backend.db and reconnect with the stored credentials.
    let mut core =
        FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("respawn");
    core.send(Command::Connect).await;
    core.wait_online().await;
    // Every start registers the capabilities and refreshes the remote config (§6.12); the
    // stored ETag makes the server answer 304, so no new `AccountCaps`.
    world
        .files
        .wait_for_i64("capabilities registered", PENDING_JOBS, |n| n == Some(0))
        .await;
    world
        .files
        .wait_for_i64(
            "remote config refreshed",
            "SELECT remote_config_at FROM account",
            |at| at > Some(fetched_at),
        )
        .await;
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM remote_config")
            .expect("remote config"),
        Some(0),
        "304 Not Modified: the ETag was sent"
    );
    assert_eq!(world.sink.events("AccountCaps").len(), 1);

    // A changed config arrives with a new ETag at the next connect and updates the limits.
    world
        .harness
        .set_remote_config("global.attachments.maxBytes", true, Some("1000"))
        .await
        .expect("remote config");
    // Android's cut of a message's attachment list is another client's entry; Kourier keeps
    // it by name, because it bounds what one message of ours may carry.
    world
        .harness
        .set_remote_config("android.attachments.maxCount", true, Some("12"))
        .await
        .expect("remote config");
    core.send(Command::Disconnect).await;
    core.send(Command::Connect).await;
    let caps = world.sink.wait_events("AccountCaps", 2, WAIT).await;
    assert_eq!(
        caps[1].as_deref(),
        Some("max_attachment_bytes=Some(1000) long_text_bytes=Some(2048)")
    );
    core.wait_online().await;
    let timestamp = world
        .send_text(&world.alice, &device, "after the restart", true)
        .await;
    world
        .expect_once(&world.alice, &device, timestamp, "after the restart")
        .await;
    // The chat's features follow the new config: files up to the new limit, and albums of
    // eleven (twelve pointers on Android, one of them the long text of a caption).
    assert_eq!(
        world
            .files
            .peek_i64(
                "SELECT CAST(value AS INTEGER) FROM remote_config
                 WHERE name = 'android.attachments.maxCount'"
            )
            .expect("remote config"),
        Some(12)
    );
    let features = world
        .sink
        .wait_event_value("ConversationUpsert", |p| {
            p["features"]["file_packing"]["albums"][0]["max"] == 11
        })
        .await["features"]
        .clone();
    assert!(
        features["files"]["Image"]["max_size"]
            .as_u64()
            .is_some_and(|max| max < 1000),
        "the new attachment limit came with it: {features}"
    );
    let (_, outcome) = core
        .transmit_text(&World::user(&world.alice), "still here")
        .await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(&device.device_ref), WAIT)
        .await
        .expect("reply after restart");
    assert_eq!(got.text, "still here");
    // No second AccountReady: linking finished in the first run.
    assert_eq!(world.sink.events("AccountReady").len(), 1);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(world.files.count_rows("inbox").expect("inbox"), 0);
    assert_eq!(world.files.count_rows("core_outbox").expect("outbox"), 0);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_previous_qr_code_still_links_after_a_rotation() {
    let Some(world) = World::start().await else {
        return;
    };
    // Desktop keeps two provisioning sockets (§6.5 step 2): the code on screen and the previous
    // one. Rotating every 3 s instead of 45 s makes the edge cheap to reach.
    let network = SignalNetwork::new().with_qr_rotation(Duration::from_secs(3));
    let mut core = FakeCore::start_with(
        network,
        &world.env,
        &world.files,
        StartMode::Fresh,
        &world.sink,
    )
    .expect("spawn the actor");
    let (first, _) = core.begin_login(None).await;
    let mut codes = vec![first];
    while codes.len() < 3 {
        let step = core.next_login().await;
        match &step.kind {
            LoginStepKind::ShowQr { data, valid_for } => {
                assert_eq!(*valid_for, Some(Duration::from_secs(3)));
                codes.push(data.expose().clone());
            }
            other => panic!("expected another QR code, got {other:?}"),
        }
    }
    codes.dedup();
    assert_eq!(codes.len(), 3, "every rotation shows a new code");

    // The phone scanned the second code just before it rotated away: it still links. The
    // third code closed the first socket, so scanning the first code reaches nobody.
    let (stale, device, done) = tokio::join!(
        world.harness.complete_provision(
            &codes[0],
            &world.phone.device_ref,
            Duration::from_secs(2)
        ),
        world
            .harness
            .complete_provision(&codes[1], &world.phone.device_ref, WAIT),
        async {
            loop {
                let step = core.next_login().await;
                match step.kind {
                    LoginStepKind::Done => return step,
                    LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                    _ => {}
                }
            }
        }
    );
    assert!(stale.is_err(), "the oldest code is closed: {stale:?}");
    let device = device.expect("the previous code links");
    assert_eq!(done.step_id, "done");
    assert_eq!(device.aci, world.phone.aci);
    core.wait_online().await;
    assert_eq!(world.sink.events("AccountReady").len(), 1);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relinking_another_account_is_refused_before_the_link_request() {
    let Some(world) = World::start().await else {
        return;
    };
    // "Link again" for the account of "Myself" (§4.3 step 6) on a fresh backend.db.
    let relink = || Relink {
        account: AccountId(1),
        expect_own: vec![World::user(&world.phone)],
    };
    let mut core = FakeCore::start(&world.env, &world.files, StartMode::Fresh, &world.sink)
        .expect("spawn the actor");
    let (url, _) = core.begin_login(Some(relink())).await;

    // Alice's phone scans the code: another account.
    let (scanned, error) = tokio::join!(
        world
            .harness
            .complete_provision(&url, &world.alice.device_ref, Duration::from_secs(5)),
        async {
            loop {
                let step = core.next_login().await;
                match step.kind {
                    LoginStepKind::Failed { error } => return error,
                    LoginStepKind::Done => panic!("linked to another account"),
                    _ => {}
                }
            }
        }
    );
    assert_eq!(
        error.user_message.as_deref(),
        Some("This is a different Signal account; add it as a new account.")
    );
    assert!(scanned.is_err(), "the link never completed: {scanned:?}");
    // Refused before `PUT /v1/devices/link`: the server registered no device for Alice, and
    // nothing was stored or sent to the core.
    let devices = world
        .harness
        .list_devices(&world.alice.aci)
        .await
        .expect("devices");
    assert_eq!(
        devices.iter().map(|d| d.device_id).collect::<Vec<_>>(),
        [1],
        "only Alice's phone"
    );
    assert!(world.sink.event_names().is_empty());

    // The right phone relinks the same account.
    let (url, _) = core.begin_login(Some(relink())).await;
    let (device, done) = tokio::join!(
        world
            .harness
            .complete_provision(&url, &world.phone.device_ref, WAIT),
        async {
            loop {
                let step = core.next_login().await;
                match step.kind {
                    LoginStepKind::Done => return step,
                    LoginStepKind::Failed { error } => panic!("relink failed: {error:?}"),
                    _ => {}
                }
            }
        }
    );
    assert_eq!(device.expect("relinked").aci, world.phone.aci);
    assert_eq!(done.step_id, "done");
    assert_eq!(world.sink.events("AccountReady").len(), 1);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(world.files.count_rows("account").expect("account"), 1);
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn linking_without_a_reachable_server_fails_with_a_connection_error() {
    let Some(world) = World::start().await else {
        return;
    };
    // The mock's environment, but on a port nothing listens on: no provisioning socket opens,
    // so no QR code can be shown (Desktop: "the only active socket has failed").
    let BackendEnv::LocalTest(mut env) = world.env.clone() else {
        panic!("a local test env");
    };
    env.port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("a free port")
        .port();
    let env = BackendEnv::LocalTest(env);
    let mut core = FakeCore::start(&env, &world.files, StartMode::Fresh, &world.sink)
        .expect("spawn the actor");
    core.send(Command::StartLogin {
        flow: "qr".to_owned(),
        relink: None,
    })
    .await;
    let input = core.next_login().await;
    core.send(Command::SubmitLoginInput {
        txn: input.txn,
        values: Vec::new(),
    })
    .await;
    let error = loop {
        let step = core.next_login().await;
        match step.kind {
            LoginStepKind::Failed { error } => break error,
            LoginStepKind::ShowQr { .. } => panic!("a QR code without a server"),
            _ => {}
        }
    };
    assert_eq!(
        error.user_message.as_deref(),
        Some("Kourier could not reach the Signal server. Check the connection and try again.")
    );
    assert!(matches!(
        error.kind,
        kourier_model::ErrorKind::Transient { .. }
    ));
    assert!(world.sink.event_names().is_empty(), "nothing durable");
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// What the helper process of [`kill_at_every_failpoint_gives_the_uncrashed_state`] runs.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    uuid: String,
    key: Vec<u8>,
    env: BackendEnv,
    sink: PathBuf,
}

/// Every kill point of the receive path (§10.4) and the hit counts the three-message stream of a
/// run reaches.
const KILL_POINTS: [(&str, u32); 8] = [
    (kourier_signal::failpoints::AFTER_INBOX_COMMIT, 1),
    (kourier_signal::failpoints::AFTER_INBOX_COMMIT, 2),
    (kourier_signal::failpoints::BEFORE_ACK, 1),
    (kourier_signal::failpoints::BEFORE_ACK, 2),
    (kourier_signal::failpoints::AFTER_BACKEND_EFFECTS_COMMIT, 1),
    (kourier_signal::failpoints::AFTER_BACKEND_EFFECTS_COMMIT, 2),
    (kourier_signal::failpoints::AFTER_CORE_COMMIT, 1),
    (kourier_signal::failpoints::AFTER_CORE_COMMIT, 2),
];

/// One message of a run's stream, as the uncrashed state compares it (the run number and the
/// arrival-dependent `catch_up` left out).
#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamMessage {
    sender: &'static str,
    text: String,
    conv_is_sender: bool,
    device: Option<u32>,
}

impl World {
    /// The stream of run `run`: Alice unsealed, Alice sealed, Bob unsealed, sent 300 ms apart
    /// so each lands in its own stage-1 batch, lane step and forwarder batch.
    async fn send_stream(
        &self,
        device: &DeviceInfo,
        run: usize,
    ) -> Vec<(&'static str, u64, String)> {
        let mut sent = Vec::new();
        for (name, from, sealed, suffix) in [
            ("alice", &self.alice, false, "a"),
            ("alice", &self.alice, true, "b"),
            ("bob", &self.bob, false, "c"),
        ] {
            let text = format!("run {run} {suffix}");
            let result = self
                .harness
                .send_text(&SendText {
                    sealed,
                    ..SendText::new(&from.aci, &device.device_ref, &text)
                })
                .await
                .expect("sendText");
            sent.push((name, result.timestamp, text));
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        sent
    }

    /// Waits until the stream reached the sink and nothing is left in flight, and returns what
    /// the core holds of it.
    async fn stream_state(
        &self,
        device: &DeviceInfo,
        run: usize,
        sent: &[(&'static str, u64, String)],
    ) -> Vec<StreamMessage> {
        let mut state = Vec::new();
        for (name, timestamp, text) in sent {
            let primary = if *name == "alice" {
                &self.alice
            } else {
                &self.bob
            };
            let author = Self::user(primary);
            let message = self
                .sink
                .wait_message(author.as_str(), &timestamp.to_string(), WAIT)
                .await
                .unwrap_or_else(|| panic!("run {run}: {text:?} reached the core"));
            state.push(StreamMessage {
                sender: name,
                text: message.body.replacen(&format!("run {run} "), "", 1),
                conv_is_sender: message.conv == format!("direct:{}", author.as_str()),
                device: message.device,
            });
        }
        self.harness
            .wait_for_deliveries(&device.device_ref, WAIT)
            .await
            .expect("deliveries settled");
        // No other row of this run: every message exactly once.
        for (name, primary) in [("alice", &self.alice), ("bob", &self.bob)] {
            let author = Self::user(primary);
            let rows = self
                .sink
                .messages_with_prefix(author.as_str(), &format!("run {run} "));
            let expected = sent.iter().filter(|(sender, ..)| *sender == name).count();
            assert_eq!(rows, expected, "run {run}: no duplicate or stray rows");
        }
        state
    }

    /// The account's leftovers once the actor stopped: nothing in the inbox or the outbox,
    /// nothing quarantined.
    fn assert_drained(&self, what: &str) {
        for table in ["inbox", "core_outbox", "core_outbox_poison"] {
            assert_eq!(
                self.files.count_rows(table).expect(table),
                0,
                "{what}: {table}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn kill_at_every_failpoint_gives_the_uncrashed_state() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // The uncrashed run.
    let mut core =
        FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("start");
    core.send(Command::Connect).await;
    core.wait_online().await;
    let sent = world.send_stream(&device, 0).await;
    let reference = world.stream_state(&device, 0, &sent).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.assert_drained("uncrashed run");
    assert_eq!(
        reference
            .iter()
            .map(|m| (m.sender, m.text.as_str(), m.conv_is_sender))
            .collect::<Vec<_>>(),
        [
            ("alice", "a", true),
            ("alice", "b", true),
            ("bob", "c", true)
        ]
    );

    for (run, (failpoint, hit)) in (1..).zip(KILL_POINTS) {
        let point = format!("{failpoint}:{hit}");
        // The actor runs in a helper process armed to abort at the point; it shares the sink
        // (the core's database) with this process, as a crashed app shares core.db with the
        // next start.
        let spec = ChildSpec {
            dir: world.files.dir.clone(),
            uuid: world.files.uuid.to_string(),
            key: world.files.key.to_vec(),
            env: world.env.clone(),
            sink: world.tmp.path().join("sink.db"),
        };
        let mut child = spawn_child(&spec, &point);
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let online = tokio::time::timeout(WAIT, async {
            while let Some(line) = lines.next_line().await.expect("child stdout") {
                if line.trim() == format!("{CHILD_LINE} conn Online") {
                    return true;
                }
            }
            false
        })
        .await;
        assert_eq!(online, Ok(true), "{point}: the helper process came online");

        // The phone sends while the helper runs; the server keeps whatever the dead socket did
        // not acknowledge (the harness queues lost envelopes again).
        let (sent, status) = tokio::join!(world.send_stream(&device, run), async {
            tokio::time::timeout(WAIT, child.wait())
                .await
                .unwrap_or_else(|_| panic!("{point}: the helper process did not end"))
                .expect("helper status")
        });
        assert_eq!(
            status.signal(),
            Some(libc_sigabrt()),
            "{point}: the helper aborted at the failpoint ({status})"
        );

        // Restart: whatever was committed is finished, redelivered envelopes are duplicates.
        let mut core = FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink)
            .expect("respawn");
        core.send(Command::Connect).await;
        let state = world.stream_state(&device, run, &sent).await;
        core.wait_online().await;
        assert_eq!(core.shutdown().await, Exit::Shutdown);
        world.assert_drained(&point);
        assert_eq!(state, reference, "{point}: the uncrashed state");
    }
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Starts the helper process of [`child_actor_process`] armed with `failpoint`.
fn spawn_child(spec: &ChildSpec, failpoint: &str) -> tokio::process::Child {
    tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "child_actor_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, serde_json::to_string(spec).expect("spec"))
        .env("KOURIER_FAILPOINT", failpoint)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the helper process")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_prekey_message_to_our_pni_arrives_once() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    // Alice knows only our number: her first message goes to our PNI, as a PreKey message
    // from our PNI prekeys (the only type accepted there, §6.6).
    let text = "hello, phone number";
    let sent = world
        .harness
        .send_text(&SendText {
            service_id_kind: Some("PNI".to_owned()),
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&world.alice.aci, &device.device_ref, text)
        })
        .await
        .expect("sendText to our PNI");
    world
        .expect_once(&world.alice, &device, sent.timestamp, text)
        .await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT share_my_number FROM peers WHERE service_id = '{}'",
                world.alice.aci
            ))
            .expect("peers"),
        Some(1),
        "she knows our number"
    );
    world.assert_drained("PNI message");
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The base64 of an ACI's 16 bytes, as `*Binary` fields appear in the harness's JSON.
fn aci_b64(aci: &str) -> String {
    STANDARD.encode(uuid::Uuid::parse_str(aci).expect("uuid").as_bytes())
}

/// How long a test waits to be sure something did not arrive.
const QUIET: Duration = Duration::from_millis(1500);

/// S8 (§6.7, §11): sends refused in chats the phone has not accepted; a contact's sealed
/// message in an accepted chat owes a sealed delivery receipt; replies go sealed with the
/// contact's access key and are synced to our phone as transcripts (unsealed); a repeated
/// transmit sends nothing twice (`send_progress`); a device the contact adds triggers a 409 and
/// still gets the message; Note to Self is a transcript only; our PNI signature rides along
/// while the contact knows only our number and stops once they acknowledged it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_sends_transcripts_receipts_and_device_repair() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let alice = World::user(&world.alice);
    let alice_sid = world.alice.aci.clone();
    let me = &device.device_ref;

    // (1) Carol's chat is not accepted: nothing goes out.
    let carol = world
        .harness
        .create_primary(&CreatePrimary::named("Carol"))
        .await
        .expect("Carol");
    let (_, outcome) = core.transmit_text(&World::user(&carol), "hi Carol").await;
    let TransmitOutcome::Failed(error) = outcome else {
        panic!("refused: {outcome:?}");
    };
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(
        error.user_message.is_some(),
        "start this chat on your phone"
    );

    // (2) Alice writes sealed with her profile key: we owe her a delivery receipt, sealed.
    let sent = world
        .harness
        .send_text(&SendText {
            sealed: true,
            with_profile_key: true,
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&world.alice.aci, me, "sealed hello")
        })
        .await
        .expect("sendText")
        .timestamp;
    world
        .expect_once(&world.alice, &device, sent, "sealed hello")
        .await;
    let receipt = world
        .harness
        .wait_for_receipt(&world.alice.aci, Some(me), WAIT)
        .await
        .expect("delivery receipt");
    assert_eq!(receipt.kind, "delivery");
    assert_eq!(receipt.timestamps, [sent]);
    assert!(receipt.sealed, "receipts go sealed with her access key");
    // An unsealed message: the server's receipt covers it, none from us.
    let unsealed = world
        .send_text(&world.alice, &device, "unsealed hello", false)
        .await;
    world
        .expect_once(&world.alice, &device, unsealed, "unsealed hello")
        .await;
    assert!(
        world
            .harness
            .wait_for_receipt(&world.alice.aci, Some(me), QUIET)
            .await
            .is_err(),
        "no delivery receipt for an unsealed message"
    );

    // (3) A sealed reply, and its transcript to our phone.
    let req = text_request(&alice, "sealed reply");
    let reply_at = u64::try_from(req.sent_at).expect("timestamp");
    let outcome = core.transmit(req.clone()).await;
    assert_eq!(
        outcome,
        TransmitOutcome::Sent {
            server_at: None,
            per_recipient: vec![(alice.clone(), RecipientState::Sent { sealed: true })],
        }
    );
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(me), WAIT)
        .await
        .expect("Alice got the reply");
    assert_eq!(got.text, "sealed reply");
    assert!(got.sealed);
    assert_eq!(got.envelope_type, "SealedSender");
    assert_eq!(got.timestamp, reply_at);
    assert_eq!(
        got.data_message["profileKey"].as_str().map(str::len),
        Some(44),
        "our profile key rides along"
    );
    assert_eq!(got.data_message["expireTimerVersion"], 1);
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(me), "sent", WAIT)
        .await
        .expect("sync transcript");
    let sent_json = &sync.sync_message["content"]["sent"];
    assert_eq!(sent_json["message"]["body"], "sealed reply");
    assert_eq!(sent_json["timestamp"], reply_at);
    assert_eq!(
        sent_json["destinationServiceIdBinary"].as_str(),
        Some(aci_b64(&alice_sid).as_str())
    );
    assert_eq!(sent_json["unidentifiedStatus"][0]["unidentified"], true);
    assert_eq!(sent_json["isRecipientUpdate"], false);
    let progress = format!(
        "SELECT count(*) FROM send_progress WHERE local = '{}'",
        req.local
    );
    assert_eq!(
        world.files.peek_i64(&progress).expect("progress"),
        Some(2),
        "Alice and the transcript"
    );
    assert!(
        world
            .files
            .peek_i64(&format!(
                "SELECT count(*) FROM msl_payloads WHERE timestamp = {reply_at}"
            ))
            .expect("msl")
            >= Some(2),
        "the message and its transcript are in the send log"
    );

    // (4) The same transmit again (the core restarted before it committed the outcome): the
    // server has it for every recipient, so nothing is sent twice.
    let again = core.transmit(req.clone()).await;
    assert!(matches!(again, TransmitOutcome::Sent { .. }), "{again:?}");
    assert!(
        world
            .harness
            .wait_for_message(&world.alice.aci, Some(me), QUIET)
            .await
            .is_err(),
        "no second copy for Alice"
    );
    assert!(
        world
            .harness
            .wait_for_sync_message(&world.phone.aci, Some(me), "sent", QUIET)
            .await
            .is_err(),
        "no second transcript"
    );

    // (5) Alice adds a device: the server answers 409, the send fetches its keys and both get it.
    let added = world
        .harness
        .add_device(&world.alice.aci)
        .await
        .expect("addDevice");
    let (both_at, outcome) = core.transmit_text(&alice, "to both devices").await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&world.alice.aci, Some(me), WAIT)
        .await
        .expect("Alice's phone");
    assert_eq!(got.text, "to both devices");
    let envelope = world
        .harness
        .wait_for_envelope(&added.device_ref, WAIT)
        .await
        .expect("Alice's new device");
    assert!(envelope.sealed);
    assert_eq!(
        envelope.timestamp,
        u64::try_from(both_at).expect("timestamp")
    );
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(me), "sent", WAIT)
        .await
        .expect("transcript");
    assert_eq!(
        sync.sync_message["content"]["sent"]["message"]["body"],
        "to both devices"
    );

    // (6) Note to Self: only the transcript, to our phone.
    let own = World::user(&world.phone);
    let (note_at, outcome) = core.transmit_text(&own, "note to self").await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let sync = world
        .harness
        .wait_for_sync_message(&world.phone.aci, Some(me), "sent", WAIT)
        .await
        .expect("note to self transcript");
    let sent_json = &sync.sync_message["content"]["sent"];
    assert_eq!(sent_json["message"]["body"], "note to self");
    assert_eq!(sent_json["timestamp"], u64::try_from(note_at).expect("t"));
    assert_eq!(
        sent_json["destinationServiceIdBinary"].as_str(),
        Some(aci_b64(&world.phone.aci).as_str())
    );

    // (7) Alice's delivery receipt for the reply settles its send-log rows.
    let msl_rows = format!(
        "SELECT count(*) FROM msl_recipients r JOIN msl_payloads p ON p.id = r.payload_id
         WHERE p.timestamp = {reply_at} AND r.recipient = '{alice_sid}'"
    );
    assert_eq!(world.files.peek_i64(&msl_rows).expect("msl"), Some(1));
    world
        .harness
        .send_receipt(&world.alice.aci, me, "delivery", &[reply_at], true)
        .await
        .expect("receipt");
    world
        .files
        .wait_for_i64("send log settled", &msl_rows, |n| n == Some(0))
        .await;

    // (8) Carol knows only our number: once her chat is accepted, our reply carries the PNI
    // signature until her delivery receipt says she has merged it.
    let to_pni = world
        .harness
        .send_text(&SendText {
            service_id_kind: Some("PNI".to_owned()),
            await_delivery: true,
            timeout: Some(60_000),
            ..SendText::new(&carol.aci, me, "hello, number")
        })
        .await
        .expect("sendText to our PNI")
        .timestamp;
    world
        .expect_once(&carol, &device, to_pni, "hello, number")
        .await;
    world.accept(&device, &carol).await;
    let (carol_at, outcome) = core.transmit_text(&World::user(&carol), "hi Carol").await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&carol.aci, Some(me), WAIT)
        .await
        .expect("Carol got it");
    assert_eq!(got.text, "hi Carol");
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT max(has_pni_signature) FROM msl_payloads p
                 JOIN msl_recipients r ON r.payload_id = p.id
                 WHERE p.timestamp = {carol_at} AND r.recipient = '{}'",
                carol.aci
            ))
            .expect("msl"),
        Some(1),
        "the message carried our PNI signature"
    );
    let share = format!(
        "SELECT share_my_number FROM peers WHERE service_id = '{}'",
        carol.aci
    );
    assert_eq!(world.files.peek_i64(&share).expect("peers"), Some(1));
    world
        .harness
        .send_receipt(
            &carol.aci,
            me,
            "delivery",
            &[u64::try_from(carol_at).expect("t")],
            false,
        )
        .await
        .expect("Carol's receipt");
    world
        .files
        .wait_for_i64("PNI signature acknowledged", &share, |v| v == Some(0))
        .await;

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(
        world
            .files
            .session_devices(&world.alice.aci)
            .expect("sessions"),
        [1, added.device_id],
        "the added device got a session from the 409 repair"
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// SIGABRT, what `std::process::abort` raises.
fn libc_sigabrt() -> i32 {
    6
}

/// The helper process of the kill test: runs the account actor with `Resume`, connects, and
/// prints its connection states until the armed failpoint aborts it.
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
        let mut conn = core.conn_watch();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            println!("{CHILD_LINE} conn {}", conn.borrow_and_update().name());
            tokio::select! {
                changed = conn.changed() => if changed.is_err() { break },
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        // Only reached when the failpoint did not fire.
        println!("{CHILD_LINE} done");
        let _ = core.shutdown().await;
    });
}
