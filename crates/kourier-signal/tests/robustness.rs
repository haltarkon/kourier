//! S15 "Robustness" against the mock Signal server (architecture §4.3, §6.4, §6.12, §11 S15):
//! the authenticated connection survives a suspend, a dropped socket, a server that stops
//! answering and a wrong clock; a second client with our credentials (4409) stops it until the
//! user reconnects; an unlink from the phone, a refused login and a logout end the device;
//! prekeys are refilled and rotated; a send challenge (428) is answered; a change of the
//! phone's number is applied.
//!
//! The account actor runs under the fake core of `support`. What the core itself does with
//! these states (the wipe, the relink, the outbox pause) is tested through the real core in
//! `kourier-e2e` (`signal_robustness.rs`).
//!
//! Skipped (with a notice) when the mock harness is not installed; with
//! `KOURIER_REQUIRE_MOCK_HARNESS=1` a missing harness is a failure.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    dead_code,
    reason = "the shared support modules serve several test files"
)]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{
    BackendEnv, Clock as _, Command, Exit, InitialSync, StartMode, SystemClock, TransmitOutcome,
};
use kourier_model::{
    BackendError, ConnState, DeviceLogout, ErrorKind, LoginStepKind, LogoutReport, Redacted,
    UserAction, UserId,
};
use kourier_signal::SignalNetwork;
use support::fake_core::{AccountFiles, FakeCore, ShiftClock, Sink, WAIT, local_env, text_request};
use support::harness::{CreatePrimary, DeviceInfo, Harness, KeyUpload, Primary, SendText};

/// How long "nothing happens" is watched.
const QUIET: Duration = Duration::from_millis(1500);
const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// The WARN and ERROR lines of every actor in this test binary (an actor logs on its own
/// thread, so a per-test subscriber would not see them).
static WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Keeps the message of each WARN and ERROR event in [`WARNINGS`].
struct Warnings;

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Warnings {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        WARNINGS.lock().expect("warnings").push(message.0);
    }
}

/// What the actor warned about so far when it had a device to remove from the account
/// (`Command::Logout`, a cancelled login, a shutdown during a link), in any test of this
/// binary: these warnings start with "logout" or tell the user to "unlink it" on the phone.
/// None of these tests leaves a device it cannot remove, so none of them may warn.
fn logout_warnings() -> Vec<String> {
    WARNINGS
        .lock()
        .expect("warnings")
        .iter()
        .filter(|line| line.starts_with("logout") || line.contains("unlink it"))
        .cloned()
        .collect()
}

fn init_tracing() {
    use tracing_subscriber::Layer as _;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_test_writer()
                .with_filter(tracing_subscriber::EnvFilter::from_default_env()),
        )
        .with(Warnings.with_filter(tracing_subscriber::filter::LevelFilter::WARN))
        .try_init();
}

/// Our phone ("Myself") and two contacts: Alice, whose chat the phone accepts after the link,
/// and Bob, a stranger.
struct World {
    harness: Harness,
    _tmp: tempfile::TempDir,
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
            .create_primary(&CreatePrimary::named("Myself"))
            .await
            .expect("Myself");
        let sink = Sink::open(&tmp.path().join("sink.db")).expect("sink");
        let files = AccountFiles::new(tmp.path()).expect("account dir");
        Some(Self {
            harness,
            _tmp: tmp,
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

    /// The actor over this world's account files.
    fn actor(&self, network: SignalNetwork, mode: StartMode, clock: &ShiftClock) -> FakeCore {
        FakeCore::start_clocked(
            network,
            &self.env,
            &self.files,
            mode,
            &self.sink,
            InitialSync::LinkOnly,
            Arc::new(clock.clone()),
        )
        .expect("spawn the actor")
    }

    /// Links a fresh account to "Myself", waits for `Online`, and has the phone accept the
    /// chat with Alice.
    async fn link_with(
        &self,
        network: SignalNetwork,
        clock: &ShiftClock,
    ) -> (FakeCore, DeviceInfo) {
        let mut core = self.actor(network, StartMode::Fresh, clock);
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
        let device = device.expect("linked device");
        core.wait_online().await;
        self.harness
            .wait_for_sync(&self.phone.device_ref, &device.device_ref, WAIT)
            .await
            .expect("sync requests reached the phone");
        self.harness
            .send_message_request_response(
                &self.phone.device_ref,
                &device.device_ref,
                &self.alice.aci,
                "accept",
            )
            .await
            .expect("message request response");
        self.files
            .wait_for_i64(
                "chat accepted",
                &format!(
                    "SELECT profile_sharing FROM peers WHERE service_id = '{}'",
                    self.alice.aci
                ),
                |v| v == Some(1),
            )
            .await;
        // Everything the link owes the server is done (capabilities, the first key run).
        self.files
            .wait_for_i64(
                "link jobs",
                "SELECT count(*) FROM jobs WHERE kind IN ('capabilities', 'sync_request')
                   OR (kind = 'key_maintenance' AND next_at <= unixepoch() * 1000)",
                |n| n == Some(0),
            )
            .await;
        (core, device)
    }

    async fn link(&self) -> (FakeCore, DeviceInfo) {
        self.link_with(SignalNetwork::new(), &ShiftClock::default())
            .await
    }

    /// `from` sends `text` to our device; returns its timestamp once the server took it
    /// (delivered or queued for our next connection).
    async fn says(&self, from: &Primary, device: &DeviceInfo, text: &str) -> u64 {
        self.harness
            .send_text(&SendText::new(&from.aci, &device.device_ref, text))
            .await
            .expect("sendText")
            .timestamp
    }

    /// Waits until `text` from `from` reached the core, exactly once.
    async fn arrives(&self, from: &Primary, device: &DeviceInfo, timestamp: u64, text: &str) {
        let author = Self::user(from);
        let local = timestamp.to_string();
        let message = self
            .sink
            .wait_message(author.as_str(), &local, WAIT)
            .await
            .unwrap_or_else(|| panic!("{text:?} reached the core"));
        assert_eq!(message.body, text);
        self.harness
            .wait_for_deliveries(&device.device_ref, WAIT)
            .await
            .expect("deliveries settled");
        assert_eq!(
            self.sink.deliveries(author.as_str(), &local),
            1,
            "{text:?} exactly once"
        );
    }

    /// Authenticated websocket upgrades our device asked for so far.
    async fn connects(&self, device: &DeviceInfo) -> u64 {
        self.harness
            .connection_stats(&device.device_ref)
            .await
            .expect("connectionStats")
            .connects
    }

    /// Waits until the device has opened `n` more sockets than `before` and is `Online`.
    async fn reconnected(&self, core: &mut FakeCore, device: &DeviceInfo, before: u64, n: u64) {
        let deadline = Instant::now() + WAIT;
        loop {
            let stats = self
                .harness
                .connection_stats(&device.device_ref)
                .await
                .expect("connectionStats");
            if stats.connects >= before + n && stats.open && core.conn_state().is_online() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "no reconnect: {stats:?}, {}",
                core.conn_state().name()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// `Command::SubmitChallenge` and its answer.
async fn submit_challenge(
    core: &FakeCore,
    token: Option<&str>,
    answer: &str,
) -> Result<(), BackendError> {
    let (reply, answered) = tokio::sync::oneshot::channel();
    core.send(Command::SubmitChallenge {
        token: token.map(|t| Redacted::new(t.to_owned())),
        answer: Redacted::new(answer.to_owned()),
        reply,
    })
    .await;
    tokio::time::timeout(WAIT, answered)
        .await
        .expect("the challenge answer was handled")
        .expect("a reply")
}

/// §11 S15 "Done when": after a simulated 10-minute suspend (`Resumed`) the client is connected
/// again within 10 s. The socket of a machine that slept that long is dead on the server's
/// side whatever the client's TCP stack believes, so it is replaced without asking; after a
/// short sleep it is asked, and kept when it answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_a_ten_minute_suspend_the_device_is_connected_again_within_ten_seconds() {
    let Some(world) = World::start().await else {
        return;
    };
    let clock = ShiftClock::default();
    let (mut core, device) = world.link_with(SignalNetwork::new(), &clock).await;
    let before = world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats");
    assert!(before.open);

    // A minute asleep: the socket is asked whether it lives (a keepalive), and stays.
    clock.shift(MINUTE_MS);
    core.send(Command::Resumed).await;
    let deadline = Instant::now() + WAIT;
    loop {
        let stats = world
            .harness
            .connection_stats(&device.device_ref)
            .await
            .expect("stats");
        if stats.keepalives > before.keepalives {
            break;
        }
        assert!(Instant::now() < deadline, "no keepalive after the resume");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        world.connects(&device).await,
        before.connects,
        "same socket"
    );
    assert!(core.conn_state().is_online());

    // Ten minutes asleep: the wall clock jumped, no timer ran.
    clock.shift(10 * MINUTE_MS);
    let resumed = Instant::now();
    core.send(Command::Resumed).await;
    world
        .reconnected(&mut core, &device, before.connects, 1)
        .await;
    let took = resumed.elapsed();
    assert!(took < Duration::from_secs(10), "reconnected in {took:?}");
    assert_eq!(
        world.connects(&device).await,
        before.connects + 1,
        "one new socket, no attempts in between"
    );

    // The new socket receives.
    let sent = world.says(&world.alice, &device, "good morning").await;
    world
        .arrives(&world.alice, &device, sent, "good morning")
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §11 S15 "Done when": a second socket for the device gets the 4409 handling. The server
/// closes ours with 4409 when another client connects with our credentials; reconnecting by
/// ourselves would take the connection back from it, and so on for ever. The state tells the
/// user, hints from the system change nothing, and `Connect` (the user's "Reconnect") does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_socket_for_the_device_stops_the_connection_until_the_user_reconnects() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    assert!(
        world
            .harness
            .second_socket(&device.device_ref)
            .await
            .expect("secondSocket"),
        "the second socket took our place"
    );
    let held = ConnState::NeedsUserAction(UserAction::Reconnect);
    core.wait_conn("NeedsUserAction(Reconnect)", |state| *state == held)
        .await;
    // Ours and the second socket's.
    let connects = world.connects(&device).await;

    // The network changes, the machine wakes up: still no reconnect.
    core.send(Command::NetworkChanged).await;
    core.send(Command::Resumed).await;
    let waiting = world.says(&world.alice, &device, "are you there").await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(world.connects(&device).await, connects, "no socket opened");
    assert_eq!(core.conn_state(), held);
    assert_eq!(
        world.sink.messages_from(World::user(&world.alice).as_str()),
        0,
        "nothing is received meanwhile"
    );

    // The user reconnects: back online, and what waited on the server arrives.
    core.send(Command::Connect).await;
    world.reconnected(&mut core, &device, connects, 1).await;
    world
        .arrives(&world.alice, &device, waiting, "are you there")
        .await;
    // Hints work again from here on (a network change asks the live socket).
    core.send(Command::NetworkChanged).await;
    tokio::time::sleep(QUIET).await;
    assert!(core.conn_state().is_online());
    assert_eq!(world.connects(&device).await, connects + 1);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §4.3 step 5, §11 S15: the phone unlinks this device. The server closes our socket and
/// refuses the next login (403): the device stops, hands the core what it still has and exits
/// `LoggedOut`. (The wipe of `backend.db` is the core's: `kourier-e2e`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unlink_from_the_phone_ends_the_device_with_logged_out() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    let sent = world.says(&world.alice, &device, "before the unlink").await;
    world
        .arrives(&world.alice, &device, sent, "before the unlink")
        .await;
    let connects = world.connects(&device).await;

    world
        .harness
        .unlink(&world.phone.device_ref, &device.device_ref)
        .await
        .expect("unlink");
    let state = core
        .wait_conn("LoggedOut", |s| matches!(s, ConnState::LoggedOut { .. }))
        .await;
    let ConnState::LoggedOut { reason } = &state else {
        unreachable!()
    };
    assert_eq!(reason, "device is not registered");
    let exit = core.wait_exit().await;
    assert_eq!(
        exit,
        Exit::LoggedOut {
            reason: reason.clone()
        }
    );
    // One refused login told us; no retries against a server that said no.
    assert_eq!(world.connects(&device).await, connects + 1);
    // Everything decrypted before went to the core first.
    assert_eq!(world.files.count_rows("core_outbox").expect("outbox"), 0);
    assert_eq!(world.files.count_rows("inbox").expect("inbox"), 0);

    // Started again on the same files (a core that crashed before its wipe): the first
    // connect says the same, at once.
    let mut again = world.actor(
        SignalNetwork::new(),
        StartMode::Resume,
        &ShiftClock::default(),
    );
    again.send(Command::Connect).await;
    again
        .wait_conn("LoggedOut", |s| matches!(s, ConnState::LoggedOut { .. }))
        .await;
    assert!(matches!(again.wait_exit().await, Exit::LoggedOut { .. }));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.4: 401 on the authenticated socket means the same as an unlink (Desktop's `authError`),
/// also when the server dropped us for another reason first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_refused_with_401_ends_the_device_with_logged_out() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    world
        .harness
        .fail_auth(&device.device_ref, 401)
        .await
        .expect("failAuth");
    assert!(
        world
            .harness
            .close_socket(&device.device_ref, 1001)
            .await
            .expect("closeSocket")
    );
    let state = core
        .wait_conn("LoggedOut", |s| matches!(s, ConnState::LoggedOut { .. }))
        .await;
    let ConnState::LoggedOut { reason } = state else {
        unreachable!()
    };
    assert!(reason.contains("401"), "{reason}");
    assert_eq!(core.wait_exit().await, Exit::LoggedOut { reason });
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.4 reconnect: a socket the server drops is reopened after a short delay and gets what
/// queued up; while the server keeps refusing, the delays grow (1, 2, 3, 5, 8 … s), the state
/// says when the next attempt comes, and a network change cuts the wait short.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_socket_is_reopened_with_growing_delays_and_at_once_on_a_network_change() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    let connects = world.connects(&device).await;

    // The server goes away for a moment (1001).
    assert!(
        world
            .harness
            .close_socket(&device.device_ref, 1001)
            .await
            .expect("closeSocket")
    );
    let offline = core
        .wait_conn("Offline", |s| matches!(s, ConnState::Offline { .. }))
        .await;
    let ConnState::Offline {
        retry_at_ms: Some(retry_at),
    } = offline
    else {
        panic!("the next attempt is announced: {offline:?}");
    };
    let wait = retry_at.saturating_sub(SystemClock.now_ms());
    assert!(wait <= 1_100, "the first delay is a second ({wait} ms)");
    let queued = world
        .says(&world.alice, &device, "while you were away")
        .await;
    world.reconnected(&mut core, &device, connects, 1).await;
    world
        .arrives(&world.alice, &device, queued, "while you were away")
        .await;

    // The server refuses every attempt (503): the delays grow.
    world
        .harness
        .fail_auth(&device.device_ref, 503)
        .await
        .expect("failAuth");
    world
        .harness
        .close_socket(&device.device_ref, 1001)
        .await
        .expect("closeSocket");
    let deadline = Instant::now() + WAIT;
    let far = loop {
        if let ConnState::Offline {
            retry_at_ms: Some(at),
        } = core.conn_state()
        {
            let wait = at.saturating_sub(SystemClock.now_ms());
            if wait >= 4_000 {
                break wait;
            }
        }
        assert!(Instant::now() < deadline, "the delays never grew");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // 1 s, 2 s and 3 s were waited out; the fourth wait is 5 s (no jitter up to there).
    assert!(far <= 5_100, "{far} ms");
    let attempts = world.connects(&device).await - connects - 1;
    assert_eq!(attempts, 3, "one attempt per delay, not a storm");

    // The server is back and the network changes: no waiting for the rest of the delay.
    world
        .harness
        .stop_fail_auth(&device.device_ref)
        .await
        .expect("stop failAuth");
    let changed = Instant::now();
    core.send(Command::NetworkChanged).await;
    core.wait_online().await;
    assert!(
        changed.elapsed() < Duration::from_secs(3),
        "reconnected {:?} after the hint",
        changed.elapsed()
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.4, §9.6: the server states its clock with every connect. A local clock more than a day
/// away from it (Desktop's limit, `client.maxAllowedClockSkewSeconds`) ends the connection,
/// tells the user by how much, and keeps the device offline, without connect attempts, until
/// the clock is right; only a resume from sleep asks the server for its time once more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_clock_keeps_the_device_offline_until_it_is_right() {
    let Some(world) = World::start().await else {
        return;
    };
    let clock = ShiftClock::default();
    let network = SignalNetwork::new().with_clock_recheck(Duration::from_millis(200));
    let (mut core, device) = world.link_with(network, &clock).await;

    // The server's clock is 25 hours ahead of ours from the next connect on.
    world
        .harness
        .set_server_time(25 * HOUR_MS)
        .await
        .expect("setServerTime");
    world
        .harness
        .close_socket(&device.device_ref, 1001)
        .await
        .expect("closeSocket");
    let state = core
        .wait_conn("NeedsUserAction(FixClock)", |s| {
            matches!(s, ConnState::NeedsUserAction(UserAction::FixClock { .. }))
        })
        .await;
    let ConnState::NeedsUserAction(UserAction::FixClock { skew_ms }) = state else {
        unreachable!()
    };
    assert!(
        (skew_ms + 25 * HOUR_MS).abs() < 10_000,
        "our clock is 25 h behind: {skew_ms}"
    );
    assert_eq!(skew_ms % 1000, 0, "whole seconds");
    // Disconnected, and it stays so: no attempts while the clock is wrong.
    let deadline = Instant::now() + WAIT;
    while world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats")
        .open
    {
        assert!(Instant::now() < deadline, "the socket stayed open");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let connects = world.connects(&device).await;
    let waiting = world.says(&world.alice, &device, "what time is it").await;
    core.send(Command::NetworkChanged).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(world.connects(&device).await, connects);
    assert!(matches!(
        core.conn_state(),
        ConnState::NeedsUserAction(UserAction::FixClock { .. })
    ));

    // The machine wakes up with the clock still wrong. The server's time we carried forward
    // stood still while it slept, so the server is asked once more: one connect, the same
    // verdict, the socket closed again.
    core.send(Command::Resumed).await;
    let deadline = Instant::now() + WAIT;
    loop {
        let stats = world
            .harness
            .connection_stats(&device.device_ref)
            .await
            .expect("stats");
        let fix_clock = matches!(
            core.conn_state(),
            ConnState::NeedsUserAction(UserAction::FixClock { .. })
        );
        if stats.connects == connects + 1 && !stats.open && fix_clock {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no fresh look at the server's clock: {stats:?}, {}",
            core.conn_state().name()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(QUIET).await;
    assert_eq!(
        world.connects(&device).await,
        connects + 1,
        "one connect per resume, no more"
    );
    assert_eq!(
        world.sink.messages_from(World::user(&world.alice).as_str()),
        0,
        "nothing is received with a wrong clock"
    );
    let connects = connects + 1;

    // The user sets the clock: found at the next look, connected again.
    clock.shift(25 * HOUR_MS);
    world.reconnected(&mut core, &device, connects, 1).await;
    world
        .arrives(&world.alice, &device, waiting, "what time is it")
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.4 keepalive: `GET /v1/keepalive` goes out on the authenticated socket in a steady
/// rhythm; a server that answers it with an error has lost the socket as far as we can tell,
/// and it is replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keepalives_prove_the_socket_and_a_failing_one_replaces_it() {
    let Some(world) = World::start().await else {
        return;
    };
    let network = SignalNetwork::new().with_keepalive_interval(Duration::from_millis(250));
    let (mut core, device) = world.link_with(network, &ShiftClock::default()).await;
    let before = world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats");
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let after = world
        .harness
        .connection_stats(&device.device_ref)
        .await
        .expect("stats");
    let sent = after.keepalives - before.keepalives;
    assert!(
        (3..=7).contains(&sent),
        "one keepalive per interval after each answer: {sent} in 1.6 s"
    );
    assert_eq!(after.connects, before.connects, "the socket stays");
    assert!(core.conn_state().is_online());

    // The server stops answering properly: the socket is given up and replaced.
    world
        .harness
        .fail_keepalive(true)
        .await
        .expect("failKeepalive");
    core.wait_conn("Offline", |s| matches!(s, ConnState::Offline { .. }))
        .await;
    world
        .harness
        .fail_keepalive(false)
        .await
        .expect("failKeepalive");
    world
        .reconnected(&mut core, &device, before.connects, 1)
        .await;
    let sent = world.says(&world.alice, &device, "still there").await;
    world
        .arrives(&world.alice, &device, sent, "still there")
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.12 key maintenance: one-time prekeys are refilled when the server runs low (for the
/// identity that ran low only), and a signed prekey and Kyber last-resort key older than 1.5
/// days are replaced at the next start, in an upload that leaves the one-time keys alone; a
/// stranger's first message built from the new keys decrypts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prekeys_are_refilled_when_low_and_rotated_when_old() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    let counts = world
        .harness
        .key_counts(&device.device_ref)
        .await
        .expect("keyCounts");
    let (aci, pni) = (counts.aci.expect("ACI keys"), counts.pni.expect("PNI keys"));
    let batch = KeyUpload {
        pre_keys: 100,
        pq_pre_keys: 100,
        signed_pre_key: None,
        pq_last_resort_pre_key: None,
    };
    let (signed, last_resort) = (aci.signed_pre_key_id, aci.last_resort_key_id);
    // What a link brings: the signed prekey and the last-resort key with the link request,
    // then one batch of one-time keys (a few of them are in use already: the phone's and the
    // contacts' sessions).
    for (kind, keys) in [("ACI", &aci), ("PNI", &pni)] {
        assert!((90..=100).contains(&keys.count), "{kind}: {keys:?}");
        assert!((90..=100).contains(&keys.pq_count), "{kind}: {keys:?}");
        assert_eq!(keys.uploads.len(), 2, "{kind}: nothing since the link");
        assert_eq!(
            keys.uploads[0],
            KeyUpload {
                pre_keys: 0,
                pq_pre_keys: 0,
                signed_pre_key: keys.signed_pre_key_id,
                pq_last_resort_pre_key: keys.last_resort_key_id,
            },
            "{kind}"
        );
        assert_eq!(keys.uploads[1], batch, "{kind}");
    }

    // Senders used our ACI one-time keys up: the next connect brings a new batch.
    world
        .harness
        .consume_keys(&device.device_ref, "ACI", 9)
        .await
        .expect("consumeKeys");
    let connects = world.connects(&device).await;
    world
        .harness
        .close_socket(&device.device_ref, 1001)
        .await
        .expect("closeSocket");
    world.reconnected(&mut core, &device, connects, 1).await;
    let deadline = Instant::now() + WAIT;
    let refilled = loop {
        let counts = world
            .harness
            .key_counts(&device.device_ref)
            .await
            .expect("keyCounts");
        let aci = counts.aci.expect("ACI keys");
        if aci.uploads.len() == 3 {
            assert_eq!(
                counts.pni.expect("PNI keys").uploads.len(),
                2,
                "PNI was full"
            );
            break aci;
        }
        assert!(Instant::now() < deadline, "no refill: {aci:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!((refilled.count, refilled.pq_count), (100, 100));
    assert_eq!(refilled.uploads[2], batch);
    assert_eq!(refilled.signed_pre_key_id, signed, "not rotated yet");
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // Two days later (the stored keys made two days older): the start's key run rotates.
    let two_days = 2 * 24 * HOUR_MS;
    world
        .files
        .execute(&format!(
            "UPDATE signed_prekeys SET created_at = created_at - {two_days}"
        ))
        .expect("age signed prekeys");
    world
        .files
        .execute(&format!(
            "UPDATE kyber_prekeys SET created_at = created_at - {two_days} WHERE last_resort = 1"
        ))
        .expect("age last-resort keys");
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Resume,
        &ShiftClock::default(),
    );
    core.send(Command::Connect).await;
    core.wait_online().await;
    let deadline = Instant::now() + WAIT;
    let rotated = loop {
        let counts = world
            .harness
            .key_counts(&device.device_ref)
            .await
            .expect("keyCounts");
        let (aci, pni) = (counts.aci.expect("ACI keys"), counts.pni.expect("PNI keys"));
        if aci.uploads.len() == 4 && pni.uploads.len() == 3 {
            break (aci, pni);
        }
        assert!(Instant::now() < deadline, "no rotation: {aci:?} {pni:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    for (kind, keys) in [("ACI", &rotated.0), ("PNI", &rotated.1)] {
        let upload = keys.uploads.last().expect("an upload");
        assert_eq!(
            (upload.pre_keys, upload.pq_pre_keys),
            (0, 0),
            "{kind}: a rotation brings no one-time keys"
        );
        assert!(upload.signed_pre_key.is_some() && upload.pq_last_resort_pre_key.is_some());
        assert_eq!(keys.signed_pre_key_id, upload.signed_pre_key, "{kind}");
        assert_eq!(keys.last_resort_key_id, upload.pq_last_resort_pre_key);
        assert!(
            keys.count >= 90 && keys.pq_count >= 90,
            "{kind}: the server kept the one-time keys: {keys:?}"
        );
    }
    assert_ne!(rotated.0.signed_pre_key_id, signed);
    assert_ne!(rotated.0.last_resort_key_id, last_resort);
    // The old keys stay here for messages already on their way; the new ones are confirmed;
    // the next run is two days off.
    world
        .files
        .wait_for_i64(
            "confirmed keys",
            "SELECT count(*) FROM signed_prekeys WHERE confirmed = 1",
            |n| n == Some(4),
        )
        .await;
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM kyber_prekeys WHERE last_resort = 1 AND confirmed = 1")
            .expect("last-resort keys"),
        Some(4)
    );
    let next_run = world
        .files
        .wait_for_i64(
            "the next key run",
            "SELECT next_at - unixepoch() * 1000 FROM jobs
             WHERE kind = 'key_maintenance' AND key = '' AND attempts = 0
               AND next_at > unixepoch() * 1000",
            |n| n.is_some(),
        )
        .await
        .expect("scheduled");
    assert!(
        // (`unixepoch()` counts whole seconds.)
        (47 * HOUR_MS..=48 * HOUR_MS + 1000).contains(&next_run),
        "in two days: {next_run} ms"
    );

    // Bob never wrote to us: his first message is built from the keys the server has now.
    let hello = world
        .says(&world.bob, &device, "hello from a stranger")
        .await;
    world
        .arrives(&world.bob, &device, hello, "hello from a stranger")
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.4, §11 S15: the server refuses a send with 428 until a person solved its captcha. The
/// failure names the challenge; the answer the browser hands back (a `signalcaptcha://` link)
/// goes to the server with the challenge's token; after that the same message is accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_challenge_is_answered_and_the_message_goes_out() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let alice = World::user(&world.alice);
    world
        .harness
        .challenge(
            &world.alice.device_ref,
            "tok-428",
            Some("solved.by-a.person"),
        )
        .await
        .expect("challenge");

    let request = text_request(&alice, "are you a robot?");
    let TransmitOutcome::Failed(error) = core.transmit(request.clone()).await else {
        panic!("the send is refused");
    };
    let ErrorKind::Challenge { token, url, .. } = &error.kind else {
        panic!("a challenge: {error:?}");
    };
    assert_eq!(token.expose(), "tok-428");
    assert_eq!(url, "https://signalcaptchas.org/challenge/generate.html");

    // No answer, a wrong answer: refused, the challenge stands.
    let empty = submit_challenge(&core, Some("tok-428"), "signalcaptcha://")
        .await
        .expect_err("an empty answer");
    assert_eq!(empty.kind, ErrorKind::InvalidInput);
    let wrong = submit_challenge(&core, Some("tok-428"), "signalcaptcha://guessed")
        .await
        .expect_err("a wrong answer");
    assert!(
        matches!(&wrong.kind, ErrorKind::Challenge { token, .. } if token.expose() == "tok-428"),
        "asked again: {wrong:?}"
    );
    assert!(wrong.user_message.is_some());
    assert!(matches!(
        core.transmit(request.clone()).await,
        TransmitOutcome::Failed(e) if matches!(e.kind, ErrorKind::Challenge { .. })
    ));

    // The right one, as the browser hands it over; the core's token may be gone (a restart):
    // the backend remembers the challenge it reported.
    submit_challenge(&core, None, "signalcaptcha://solved.by-a.person/")
        .await
        .expect("accepted");
    let answers = world
        .harness
        .challenge_answers()
        .await
        .expect("challengeAnswers");
    assert!(answers.open.is_empty(), "the challenge is lifted");
    let accepted: Vec<_> = answers.answers.iter().filter(|a| a.accepted).collect();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].r#type, "captcha");
    assert_eq!(accepted[0].token, "tok-428");
    assert_eq!(accepted[0].captcha, "solved.by-a.person");
    assert_eq!(answers.answers.len(), 2, "the empty answer never left");

    // The paused message goes out now, once.
    let outcome = core.transmit(request.clone()).await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    let got = world
        .harness
        .wait_for_message(&world.alice.device_ref, Some(&device.device_ref), WAIT)
        .await
        .expect("Alice got it");
    assert_eq!(got.text, "are you a robot?");
    assert_eq!(
        i64::try_from(got.timestamp).expect("timestamp"),
        request.sent_at
    );

    // Nothing waits any more: an answer has nothing to answer.
    let nothing = submit_challenge(&core, None, "signalcaptcha://again")
        .await
        .expect_err("no challenge");
    assert_eq!(nothing.kind, ErrorKind::InvalidInput);
    assert_eq!(
        world
            .harness
            .challenge_answers()
            .await
            .expect("challengeAnswers")
            .answers
            .len(),
        2
    );
    assert!(core.conn_state().is_online());
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.12: the jobs a send challenge holds are held no longer than the challenge is known to
/// stand. A reconnect ends the core's notice and the backend's memory of the challenge, a
/// restart loses both (and the token an answer needs): each time the held jobs are tried
/// once. While the server still wants its captcha that is one refused request and one new
/// notice per connect, never one per job, and the notice can be answered again; before, the
/// jobs waited out the server's time (here ten minutes) with nothing on show.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn jobs_held_for_a_challenge_are_tried_again_after_a_reconnect_and_a_restart() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    let harness = &world.harness;
    let me = device.device_ref.clone();
    let alice = &world.alice;
    // A sealed message from Alice: its delivery receipt is a job that sends to her.
    let sealed = async |text: &str| -> u64 {
        harness
            .send_data(serde_json::json!({
                "from": alice.aci, "to": me, "text": text, "sealed": true,
                "withProfileKey": true, "awaitDelivery": true, "timeout": 60_000,
            }))
            .await
            .expect("sendData")
            .timestamp
    };
    // Sends to Alice the server refused so far, and the notices the core got.
    let state = async || -> (u64, usize) {
        let refused = harness
            .challenge_answers()
            .await
            .expect("challengeAnswers")
            .open
            .get(&alice.aci)
            .map_or(0, |open| open.refused);
        (refused, world.sink.events("Notice").len())
    };
    let reaches = async |what: &str, want: (u64, usize)| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let now = state().await;
            if now == want {
                return;
            }
            assert!(Instant::now() < deadline, "{what}: {now:?}, not {want:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };

    // The server wants a person to vouch before it takes another send to Alice, and says to
    // come back in ten minutes.
    harness
        .challenge_for(&alice.device_ref, "hold-challenge", 600)
        .await
        .expect("challenge");
    let one = sealed("one").await;
    reaches("the receipt meets the challenge", (1, 1)).await;
    let two = sealed("two").await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(state().await, (1, 1), "the second receipt is held unasked");
    world
        .files
        .wait_for_i64(
            "held",
            "SELECT count(*) FROM jobs WHERE challenged = 1",
            |n| n >= Some(1),
        )
        .await;

    // --- The socket drops and the account is back: the hold ends with the core's notice.
    let before = world.connects(&device).await;
    assert!(harness.close_socket(&me, 1001).await.expect("closeSocket"));
    world.reconnected(&mut core, &device, before, 1).await;
    reaches("asked again after the reconnect", (2, 2)).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(state().await, (2, 2), "once per connect, not once per job");

    // --- A restart: nothing of the challenge is in memory, not even its token.
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Resume,
        &ShiftClock::default(),
    );
    core.send(Command::Connect).await;
    core.wait_online().await;
    reaches("asked again after the restart", (3, 3)).await;
    // The notice of this run can be answered: the refusal brought the token again. The
    // receipts that waited go out at once.
    submit_challenge(&core, None, "signalcaptcha://solved.by-a.person")
        .await
        .expect("the answer is accepted");
    let mut acknowledged = Vec::new();
    while acknowledged.len() < 2 {
        let receipt = harness
            .wait_for_receipt(&alice.aci, Some(&me), Duration::from_secs(20))
            .await
            .expect("the receipts right after the answer");
        assert_eq!(receipt.kind, "delivery");
        acknowledged.extend(receipt.timestamps);
    }
    acknowledged.sort_unstable();
    assert_eq!(acknowledged, [one, two]);
    assert_eq!(state().await.1, 3, "no further notice");
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.14 item 13, §11 S15: our phone changes its number. The `PniChangeNumber` sync gives this
/// device the new PNI, its identity, keys and registration id; the core learns the new id;
/// fresh PNI prekeys of our own go to the server; and somebody who only knows the new number
/// reaches us.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_number_change_from_the_phone_is_applied() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let old_pni = device.pni.clone().expect("a PNI");
    assert_eq!(world.sink.events("AccountReady").len(), 1);
    // The link told the core the account's number.
    let linked_number = world.sink.events("AccountNumber");
    assert_eq!(linked_number.len(), 1, "{linked_number:?}");
    let old_number = linked_number[0].clone().expect("a number");
    assert!(old_number.starts_with('+'), "E.164");

    let changed = world
        .harness
        .change_number(&world.phone.device_ref)
        .await
        .expect("changeNumber");
    assert_ne!(changed.pni, old_pni);
    let new_pni = changed.pni.strip_prefix("PNI:").unwrap_or(&changed.pni);
    world
        .files
        .wait_for_i64(
            "the new PNI",
            &format!("SELECT count(*) FROM account WHERE pni = '{new_pni}'"),
            |n| n == Some(1),
        )
        .await;
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT count(*) FROM account WHERE e164 = '{}'",
                changed.e164
            ))
            .expect("account"),
        Some(1),
        "the new number"
    );
    if let Some(registration_id) = changed.pni_registration_id {
        assert_eq!(
            world
                .files
                .peek_i64("SELECT pni_registration_id FROM account")
                .expect("account"),
            Some(i64::from(registration_id))
        );
    }
    // The core's own ids: the ACI as before, the new PNI.
    let ready = world.sink.wait_events("AccountReady", 2, WAIT).await;
    let detail = ready[1].clone().unwrap_or_default();
    assert!(
        detail.contains(&format!("aci:{}", world.phone.aci)),
        "{detail}"
    );
    assert!(detail.contains(&format!("pni:{new_pni}")), "{detail}");
    // And the new number, with the same event.
    assert_ne!(changed.e164, old_number);
    assert_eq!(
        world.sink.events("AccountNumber"),
        [Some(old_number), Some(changed.e164.clone())],
        "the core learns the new number"
    );

    // Fresh PNI prekeys of our own: a whole new set, whatever the server had.
    let deadline = Instant::now() + WAIT;
    let pni = loop {
        let pni = world
            .harness
            .key_counts(&device.device_ref)
            .await
            .expect("keyCounts")
            .pni
            .expect("PNI keys");
        // The link's two, then ours after the change.
        if pni.uploads.len() == 3 {
            break pni;
        }
        assert!(Instant::now() < deadline, "no PNI key upload: {pni:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let upload = &pni.uploads[2];
    assert_eq!((upload.pre_keys, upload.pq_pre_keys), (100, 100));
    assert!(
        upload.signed_pre_key.is_some() && upload.pq_last_resort_pre_key.is_some(),
        "{pni:?}"
    );
    world
        .files
        .wait_for_i64(
            "the forced upload is done",
            "SELECT count(*) FROM jobs WHERE kind = 'key_maintenance' AND key = 'pni'",
            |n| n == Some(0),
        )
        .await;

    // Bob knows only our new number: a PreKey message to the new PNI.
    let text = "is this your new number?";
    let sent = world
        .harness
        .send_text(&SendText {
            service_id_kind: Some("PNI".to_owned()),
            ..SendText::new(&world.bob.aci, &device.device_ref, text)
        })
        .await
        .expect("sendText to the new PNI");
    world
        .arrives(&world.bob, &device, sent.timestamp, text)
        .await;
    // The ACI side is as it was.
    let sent = world.says(&world.alice, &device, "same old me").await;
    world
        .arrives(&world.alice, &device, sent, "same old me")
        .await;
    assert!(core.conn_state().is_online());
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §4.3 step 7: `Logout` (the user removes the account) tells the server to forget this
/// device (`DELETE /v1/devices/<id>`, Desktop's `unlink`) and ends the actor `LoggedOut`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_removes_the_device_from_the_account() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let devices = world
        .harness
        .list_devices(&world.phone.device_ref)
        .await
        .expect("listDevices");
    assert_eq!(devices.len(), 2);
    let started = Instant::now();
    let (exit, reports) = core.logout().await;
    assert_eq!(
        exit,
        Exit::LoggedOut {
            reason: "logged out on request".to_owned()
        }
    );
    // The core is told once that the phone no longer lists the device, with the name the
    // link gave it (link feedback L4).
    assert_eq!(
        reports,
        vec![LogoutReport {
            outcome: DeviceLogout::Removed,
            device_name: Some("Kourier test".to_owned()),
        }]
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "within the core's 10 s: {:?}",
        started.elapsed()
    );
    let devices = world
        .harness
        .list_devices(&world.phone.device_ref)
        .await
        .expect("listDevices");
    assert_eq!(devices.len(), 1, "only the phone is left");
    assert_ne!(devices[0].device_ref, device.device_ref);
    assert_eq!(logout_warnings(), Vec::<String>::new());
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The exit of a logout on request.
fn logged_out() -> Exit {
    Exit::LoggedOut {
        reason: "logged out on request".to_owned(),
    }
}

/// How many devices the server has for our phone's account (the phone included).
async fn devices_of_the_phone(world: &World) -> usize {
    world
        .harness
        .list_devices(&world.phone.device_ref)
        .await
        .expect("listDevices")
        .len()
}

/// Has the phone scan `url` and waits until the server has linked the device and `linked`
/// holds. The harness's own wait (for the device's one-time keys) is kept running meanwhile
/// and is expected not to end: these tests stop the device before it uploads any.
async fn scanned_until(world: &World, url: &str, what: &str, linked: impl AsyncFn() -> bool) {
    let provision =
        world
            .harness
            .complete_provision(url, &world.phone.device_ref, Duration::from_secs(20));
    tokio::pin!(provision);
    let reached = async {
        let deadline = Instant::now() + WAIT;
        while !(devices_of_the_phone(world).await == 2 && linked().await) {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        done = &mut provision => panic!("the device finished its link: {done:?}"),
        () = reached => {}
    }
}

/// §4.3 step 2: the user cancels a login at the QR code, and the core sends `Logout` right
/// behind. Nothing was registered: the actor ends `LoggedOut` at once and has nothing to tell
/// the server, and nothing to warn about (no device stays on the phone).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_at_the_qr_code_has_no_device_to_remove() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Fresh,
        &ShiftClock::default(),
    );
    let (_url, _steps) = core.begin_login(None).await;
    let started = Instant::now();
    core.send(Command::CancelLogin).await;
    let (exit, reports) = core.logout().await;
    assert_eq!(exit, logged_out());
    assert_eq!(
        reports,
        vec![LogoutReport {
            outcome: DeviceLogout::NoDevice,
            device_name: None,
        }]
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "nothing to wait for: {:?}",
        started.elapsed()
    );
    assert_eq!(devices_of_the_phone(&world).await, 1, "only the phone");
    assert_eq!(world.files.count_rows("account").expect("account"), 0);
    assert_eq!(logout_warnings(), Vec::<String>::new());

    // The same without the cancel (a login the core turned away while the code was shown).
    let files = AccountFiles::new(world._tmp.path()).expect("account dir");
    let mut core = FakeCore::start(&world.env, &files, StartMode::Fresh, &world.sink)
        .expect("spawn the actor");
    let (_url, _steps) = core.begin_login(None).await;
    assert_eq!(core.exit_with(Command::Logout).await, logged_out());
    assert_eq!(devices_of_the_phone(&world).await, 1);
    assert_eq!(logout_warnings(), Vec::<String>::new());
    world.harness.shutdown().await.expect("harness shutdown");
}

/// A link finished on the server a moment before the core turned the login away (a relink
/// that proved to be another account, a cancel that came too late): the account is stored,
/// the authenticated socket is not up yet. `Logout` opens one for its request and the device
/// leaves the account, instead of staying on the user's phone with nobody behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_right_after_the_link_removes_the_device_without_a_socket() {
    let Some(world) = World::start().await else {
        return;
    };
    let harness = &world.harness;
    // The device this link creates is the account's second. Its first connection is refused
    // once, as by a server in trouble, so the actor has an account and no socket.
    let username = format!("{}.2", world.phone.aci);
    harness
        .fail_auth_named(&username, 500, 1)
        .await
        .expect("failAuth");
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Fresh,
        &ShiftClock::default(),
    );
    let (url, _steps) = core.begin_login(None).await;
    scanned_until(
        &world,
        &url,
        "the first connection was refused",
        async || {
            harness
                .connection_stats(&username)
                .await
                .expect("connectionStats")
                .connects
                >= 1
        },
    )
    .await;
    let stats = harness
        .connection_stats(&username)
        .await
        .expect("connectionStats");
    assert!(!stats.open, "no socket: {stats:?}");

    let started = Instant::now();
    let exit = core.exit_with(Command::Logout).await;
    assert_eq!(exit, logged_out());
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "within the core's 10 s: {:?}",
        started.elapsed()
    );
    assert_eq!(
        devices_of_the_phone(&world).await,
        1,
        "the device left the account"
    );
    assert_eq!(logout_warnings(), Vec::<String>::new());
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The user cancels while the link request is on its way: the server has linked the device,
/// its answer is still out. The login is not torn down there (only the answer names the
/// device): it ends with the answer, stores nothing, reports nothing to the core, and the
/// device is removed from the account, with the `Logout` the core sends and without it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_cancelled_during_its_link_request_removes_the_device_it_created() {
    let Some(world) = World::start().await else {
        return;
    };
    let harness = &world.harness;
    harness
        .delay_link(Duration::from_millis(1500))
        .await
        .expect("delayLink");

    // --- `CancelLogin`, then `Logout` (what the core sends).
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Fresh,
        &ShiftClock::default(),
    );
    let (url, _steps) = core.begin_login(None).await;
    scanned_until(&world, &url, "linked on the server", async || true).await;
    let started = Instant::now();
    core.send(Command::CancelLogin).await;
    let exit = core.exit_with(Command::Logout).await;
    assert_eq!(exit, logged_out());
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "within the core's 10 s: {:?}",
        started.elapsed()
    );
    assert_eq!(
        devices_of_the_phone(&world).await,
        1,
        "the device left the account"
    );
    assert_eq!(
        world.files.count_rows("account").expect("account"),
        0,
        "nothing was stored"
    );
    assert!(
        world.sink.events("AccountReady").is_empty(),
        "the core was told of no account"
    );

    // --- `CancelLogin` alone: the actor removes the device by itself and lives on.
    let files = AccountFiles::new(world._tmp.path()).expect("account dir");
    let mut core = FakeCore::start(&world.env, &files, StartMode::Fresh, &world.sink)
        .expect("spawn the actor");
    let (url, _steps) = core.begin_login(None).await;
    scanned_until(&world, &url, "linked on the server", async || true).await;
    core.send(Command::CancelLogin).await;
    let deadline = Instant::now() + WAIT;
    while devices_of_the_phone(&world).await != 1 {
        assert!(Instant::now() < deadline, "the device stays on the account");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(files.count_rows("account").expect("account"), 0);
    assert!(world.sink.events("AccountReady").is_empty());
    assert_eq!(logout_warnings(), Vec::<String>::new());
    harness.delay_link(Duration::ZERO).await.expect("delayLink");
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The app is closed while the link request is on its way and the server has linked the
/// device already. The core never takes a link up again after a stop (it discards an
/// unfinished one at its next start, §4.3), so `Shutdown` ends the login like a cancel: the
/// answer is awaited, nothing is stored, and the device is removed from the account inside
/// the 5 s the core waits for a shutdown. Before, the task was dropped and the device stayed
/// on the phone's list with nobody behind it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shutdown_during_the_link_request_removes_the_device_it_created() {
    let Some(world) = World::start().await else {
        return;
    };
    let harness = &world.harness;
    harness
        .delay_link(Duration::from_millis(1500))
        .await
        .expect("delayLink");
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Fresh,
        &ShiftClock::default(),
    );
    let (url, _steps) = core.begin_login(None).await;
    scanned_until(&world, &url, "linked on the server", async || true).await;
    let started = Instant::now();
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "within the core's 5 s: {:?}",
        started.elapsed()
    );
    assert_eq!(
        devices_of_the_phone(&world).await,
        1,
        "the device left the account"
    );
    assert_eq!(
        world.files.count_rows("account").expect("account"),
        0,
        "nothing was stored"
    );
    assert!(
        world.sink.events("AccountReady").is_empty(),
        "the core was told of no account"
    );

    // At the QR code a shutdown has nothing to wait for and nothing to remove.
    harness.delay_link(Duration::ZERO).await.expect("delayLink");
    let files = AccountFiles::new(world._tmp.path()).expect("account dir");
    let mut core = FakeCore::start(&world.env, &files, StartMode::Fresh, &world.sink)
        .expect("spawn the actor");
    let (_url, _steps) = core.begin_login(None).await;
    let started = Instant::now();
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "at once: {:?}",
        started.elapsed()
    );
    assert_eq!(devices_of_the_phone(&world).await, 1);
    assert_eq!(logout_warnings(), Vec::<String>::new());
    world.harness.shutdown().await.expect("harness shutdown");
}

/// `CancelLogin` only asks a login to stop, and one whose link request is on its way ends
/// with the answer. A `StartLogin` that arrives meanwhile is not taken for a second login
/// and dropped: the cancelled one is brought to its end first (its device leaves the
/// account), then the new login runs and shows its own QR code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_login_started_while_a_cancelled_one_ends_is_not_lost() {
    let Some(world) = World::start().await else {
        return;
    };
    let harness = &world.harness;
    harness
        .delay_link(Duration::from_millis(1500))
        .await
        .expect("delayLink");
    let mut core = world.actor(
        SignalNetwork::new(),
        StartMode::Fresh,
        &ShiftClock::default(),
    );
    let (url, _steps) = core.begin_login(None).await;
    scanned_until(&world, &url, "linked on the server", async || true).await;
    // The first login's last step, so that the steps read below are the new login's.
    let linking = core.next_login().await;
    assert!(
        matches!(linking.kind, LoginStepKind::Progress { .. }),
        "{linking:?}"
    );
    core.send(Command::CancelLogin).await;
    // The answer to the first link is still out for more than a second: this `StartLogin`
    // reaches the actor while the cancelled login is alive.
    let (again, _steps) = core.begin_login(None).await;
    assert_ne!(again, url, "a new code of a new login");
    assert_eq!(
        devices_of_the_phone(&world).await,
        1,
        "the cancelled login's device left the account before the new login began"
    );
    assert_eq!(world.files.count_rows("account").expect("account"), 0);
    assert!(world.sink.events("AccountReady").is_empty());
    // The new login is an ordinary one: at its QR code a logout finds no device.
    assert_eq!(core.exit_with(Command::Logout).await, logged_out());
    assert_eq!(devices_of_the_phone(&world).await, 1);
    assert_eq!(logout_warnings(), Vec::<String>::new());
    harness.delay_link(Duration::ZERO).await.expect("delayLink");
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The phone removed this device while it was disconnected, and then the user removes the
/// account here. The socket `Logout` opens for its request is refused as an unknown device:
/// there is nothing left to remove, the actor ends `LoggedOut`, and it does not tell the
/// user to unlink on the phone a device the phone has unlinked already.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_of_a_device_the_phone_removed_already_warns_about_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device) = world.link().await;
    core.send(Command::Disconnect).await;
    core.wait_conn("Offline", |state| {
        matches!(state, ConnState::Offline { .. })
    })
    .await;
    world
        .harness
        .unlink(&world.phone.device_ref, &device.device_ref)
        .await
        .expect("unlink");
    assert_eq!(devices_of_the_phone(&world).await, 1);
    let started = Instant::now();
    let (exit, reports) = core.logout().await;
    assert_eq!(exit, logged_out());
    // Nothing for the user to do on the phone.
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].outcome, DeviceLogout::Removed);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "within the core's 10 s: {:?}",
        started.elapsed()
    );
    assert_eq!(logout_warnings(), Vec::<String>::new());
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §6.5 step 6, §9.2: the account is bound to the server that linked it. A start that names
/// another server (a leftover `KOURIER_SIGNAL_ENV`, the wrong handshake) is refused before
/// anything connects, and the login is still there afterwards. Without the binding that
/// server would answer the device's credentials with 403, which reads as an unlink and ends
/// in the wipe of the login.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_for_another_server_is_refused_and_the_login_survives() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let BackendEnv::LocalTest(local) = &world.env else {
        panic!("the world runs on the mock");
    };
    let bound = format!("local:{}:{}", local.host, local.port);
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT count(*) FROM account WHERE env = '{bound}'"
            ))
            .expect("account.env"),
        Some(1),
        "the link stored the server it was made on"
    );
    let connects = world.connects(&device).await;

    // Another local server: the same certificates, another port (another mock instance, which
    // would not know this device). Nothing listens there, and nothing is asked.
    let mut elsewhere = local.clone();
    elsewhere.port = if local.port == u16::MAX {
        local.port - 1
    } else {
        local.port + 1
    };
    let other = BackendEnv::LocalTest(elsewhere.clone());
    let refused = FakeCore::start(&other, &world.files, StartMode::Resume, &world.sink)
        .expect("the actor thread starts");
    // (No `Connect` is needed to see it: the actor ends before it takes a command.)
    let Exit::Failed(error) = refused.wait_exit().await else {
        panic!("a start for another server must fail");
    };
    assert_eq!(error.kind, ErrorKind::Unsupported, "{error:?}");
    assert!(
        error.detail.contains(&bound)
            && error
                .detail
                .contains(&format!("local:{}:{}", elsewhere.host, elsewhere.port)),
        "the error names both servers: {}",
        error.detail
    );
    // Nothing was wiped and the right server was not asked either.
    assert_eq!(world.files.count_rows("account").expect("account"), 1);
    assert_eq!(world.connects(&device).await, connects);

    // The same files with their own server: connected again, no new link.
    let mut again = world.actor(
        SignalNetwork::new(),
        StartMode::Resume,
        &ShiftClock::default(),
    );
    again.send(Command::Connect).await;
    again.wait_online().await;
    assert_eq!(devices_of_the_phone(&world).await, 2);
    assert_eq!(again.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// §4.4: a first link checks which account answered before it registers anything
/// (`BackendConfig::expect_number_suffix`). The wrong phone scanning the code links nothing:
/// no device on its account, no account in `backend.db`; the expected one links as usual.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_link_by_another_account_than_the_expected_one_registers_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    let number = world.phone.e164.clone().expect("the phone has a number");
    let tail: String = number
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    // The same four digits with the last one changed: some other account's number.
    let last = tail
        .chars()
        .last()
        .and_then(|c| c.to_digit(10))
        .expect("a digit");
    let wrong = format!("{}{}", &tail[..3], (last + 1) % 10);
    let config = |suffix: &str| kourier_backend_api::BackendConfig {
        env: world.env.clone(),
        initial_sync: InitialSync::LinkOnly,
        expect_number_suffix: Some(Redacted::new(suffix.to_owned())),
        ..kourier_backend_api::BackendConfig::default()
    };

    let mut core = FakeCore::start_configured(
        SignalNetwork::new(),
        config(&wrong),
        &world.files,
        StartMode::Fresh,
        &world.sink,
        Arc::new(SystemClock),
    )
    .expect("spawn the actor");
    let (url, _steps) = core.begin_login(None).await;
    // The phone scans and waits for a device that never comes.
    let error = {
        let provision = world.harness.complete_provision(
            &url,
            &world.phone.device_ref,
            Duration::from_secs(20),
        );
        tokio::pin!(provision);
        let failed = async {
            loop {
                let step = core.next_login().await;
                match step.kind {
                    LoginStepKind::Failed { error } => return error,
                    LoginStepKind::Done => panic!("the wrong account was linked"),
                    _ => {}
                }
            }
        };
        tokio::select! {
            done = &mut provision => panic!("the device finished its link: {done:?}"),
            error = failed => error,
        }
    };
    assert_eq!(error.kind, ErrorKind::InvalidInput, "{error:?}");
    assert_eq!(error.detail, "the scanned account is not the expected one");
    assert_eq!(devices_of_the_phone(&world).await, 1, "only the phone");
    // The core turns such a login away with `Logout`: there is no device to remove.
    assert_eq!(core.exit_with(Command::Logout).await, logged_out());
    assert_eq!(world.files.count_rows("account").expect("account"), 0);
    assert_eq!(devices_of_the_phone(&world).await, 1);

    // The expected account links.
    let files = AccountFiles::new(world._tmp.path()).expect("account dir");
    let mut core = FakeCore::start_configured(
        SignalNetwork::new(),
        config(&tail),
        &files,
        StartMode::Fresh,
        &world.sink,
        Arc::new(SystemClock),
    )
    .expect("spawn the actor");
    core.link(|url| {
        let world = &world;
        async move {
            world
                .harness
                .complete_provision(&url, &world.phone.device_ref, WAIT)
                .await
                .expect("completeProvision");
        }
    })
    .await;
    assert_eq!(devices_of_the_phone(&world).await, 2);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(files.count_rows("account").expect("account"), 1);
    world.harness.shutdown().await.expect("harness shutdown");
}
