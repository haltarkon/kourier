//! The history transfer of a fresh link against `@signalapp/mock-server`
//! (docs/architecture.md §14.2 item 14): the harness plays the phone (it puts an ephemeral
//! backup key into its provisioning message when the linking URL offers `backup5`, stores an
//! archive on the CDN and answers the new device's long-poll), the test plays the core.
//!
//! The offer of the transfer is off in the library (`SignalNetwork::new()`) and on in the
//! app, which turns it on with `SignalNetwork::with_history_transfer_offer`; every test here
//! switches it on, and one proves what a link does while it is off (the run with
//! `KOURIER_HISTORY_TRANSFER=0`).
//!
//! What is proven here: the URL of a fresh link offers `backup5` and a relink's does not; the
//! key is kept with the linked account; the long-poll asks again on 204 and after a failed
//! request; the archive is downloaded, with a range request where a transfer broke off or a
//! restart came between; every frame is read; the three ends (done, skipped, failed with a
//! kind) wipe the key and remove the file; no envelope is acknowledged and no message
//! reaches the core before the transfer is over, and after every end receiving starts; a
//! restart in each of waiting, downloading and reading goes on from the stored state.
//!
//! Two tests restart in every state: one by `Shutdown` and a new actor on the same files,
//! one by SIGKILL of a helper process that runs the actor (`transfer_child_process`), which
//! is the crash the stored state is there for.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (set `KOURIER_REQUIRE_MOCK_HARNESS=1` to fail instead).

mod support;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use kourier_backend_api::{BackendEnv, BackendEvent, Command, Exit, Relink, StartMode};
use kourier_model::{AccountId, HistoryTransfer, HistoryTransferFailure, LoginStepKind, UserId};
use kourier_signal::SignalNetwork;
use kourier_signal::archive_testing::{ArchiveSpec, frames_of, write_archive};
use serde::{Deserialize, Serialize};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env};
use support::harness::{ArchivePlace, CreatePrimary, DeviceInfo, Harness, Primary, SendText};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{Layer, filter};

/// The numbers of every `link.registered` mark this process saw (microseconds by field).
fn link_marks() -> &'static Mutex<Vec<BTreeMap<String, u64>>> {
    static MARKS: OnceLock<Mutex<Vec<BTreeMap<String, u64>>>> = OnceLock::new();
    MARKS.get_or_init(Mutex::default)
}

/// Collects the measuring marks of the link (`kourier_signal::PERF_LINK_REGISTERED`).
struct MarkLayer;

#[derive(Default)]
struct MarkVisitor {
    name: Option<String>,
    numbers: BTreeMap<String, u64>,
}

impl tracing::field::Visit for MarkVisitor {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.numbers.insert(field.name().to_owned(), value);
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "name" {
            self.name = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
}

impl<S: tracing::Subscriber> Layer<S> for MarkLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = MarkVisitor::default();
        event.record(&mut visitor);
        if visitor.name.as_deref() == Some(kourier_signal::PERF_LINK_REGISTERED) {
            link_marks().lock().expect("marks").push(visitor.numbers);
        }
    }
}

fn init_tracing() {
    let marks = MarkLayer.with_filter(filter::filter_fn(|meta| {
        meta.target() == kourier_signal::PERF_TARGET
    }));
    let log = tracing_subscriber::fmt::layer()
        .with_test_writer()
        .with_filter(tracing_subscriber::EnvFilter::from_default_env());
    let _ = tracing_subscriber::registry()
        .with(marks)
        .with(log)
        .try_init();
}

/// A harness with our own phone ("Myself", which links us) and one contact.
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    alice: Primary,
    sink: Sink,
    files: AccountFiles,
}

/// A link that ran up to the linked device: the transfer (if any) is waiting.
struct Linked {
    core: FakeCore,
    device: DeviceInfo,
    url: String,
    /// The key the phone sent, when its user chose to transfer.
    key: Option<[u8; 32]>,
    seen: Seen,
}

/// The ephemeral events of one actor, in the order they came.
#[derive(Default)]
struct Seen {
    transfer: Vec<HistoryTransfer>,
    /// `transfer.len()` when the login reported `Done`.
    login_done_after: Option<usize>,
}

impl Seen {
    fn note(&mut self, event: BackendEvent) {
        match event {
            BackendEvent::HistoryTransfer(state) => self.transfer.push(state),
            BackendEvent::Login(step) => match step.kind {
                LoginStepKind::Done => self.login_done_after = Some(self.transfer.len()),
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            _ => {}
        }
    }

    /// Reads events until a transfer state satisfies `pred`; returns it.
    async fn until(
        &mut self,
        core: &mut FakeCore,
        what: &str,
        pred: impl Fn(&HistoryTransfer) -> bool,
    ) -> HistoryTransfer {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = core.next_ephemeral(left).await else {
                panic!("no history transfer state {what}; saw {:?}", self.transfer);
            };
            let hit = matches!(&event, BackendEvent::HistoryTransfer(state) if pred(state));
            self.note(event);
            if hit {
                return *self.transfer.last().expect("just pushed");
            }
        }
    }

    /// Reads events until the login reported `Done`.
    async fn until_login_done(&mut self, core: &mut FakeCore) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.login_done_after.is_none() {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let Some(event) = core.next_ephemeral(left).await else {
                panic!("the login never reported Done");
            };
            self.note(event);
        }
    }

    /// The states without the repeats of a download's progress.
    fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> =
            self.transfer.iter().map(HistoryTransfer::name).collect();
        names.dedup();
        names
    }
}

/// A network that offers the transfer and whose timings fit a test: a long-poll of one
/// second, short pauses.
fn network() -> SignalNetwork {
    SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
        .with_download_resume_pause(Duration::from_millis(50))
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
            sink,
            files,
        })
    }

    fn user(primary: &Primary) -> UserId {
        UserId::parse(&format!("aci:{}", primary.aci)).expect("user id")
    }

    /// Starts a fresh link on `network` and runs it up to the linked device. With
    /// `transfer` the phone's user chose "Transfer message history" before scanning.
    async fn link(&self, network: SignalNetwork, transfer: bool) -> Linked {
        let key = if transfer {
            Some(
                self.harness
                    .set_ephemeral_backup_key(&self.phone.device_ref)
                    .await
                    .expect("setEphemeralBackupKey"),
            )
        } else {
            None
        };
        let mut core = FakeCore::start_with(
            network,
            &self.env,
            &self.files,
            StartMode::Fresh,
            &self.sink,
        )
        .expect("spawn the actor");
        let (url, _) = core.begin_login(None).await;
        // Resolves once the device is linked and has uploaded its one-time prekeys: neither
        // waits for the transfer.
        let device = self
            .harness
            .complete_provision(&url, &self.phone.device_ref, WAIT)
            .await
            .expect("completeProvision");
        Linked {
            core,
            device,
            url,
            key,
            seen: Seen::default(),
        }
    }

    /// A new actor on the same files (a restart), on `network`.
    fn restart(&self, network: SignalNetwork) -> FakeCore {
        FakeCore::start_with(
            network,
            &self.env,
            &self.files,
            StartMode::Resume,
            &self.sink,
        )
        .expect("respawn the actor")
    }

    /// Writes an archive for the linked `device`, encrypted with the phone's `key` and shaped
    /// by `spec`, and returns its path.
    fn archive(&self, device: &DeviceInfo, key: &[u8; 32], spec: &ArchiveSpec) -> PathBuf {
        let path = self.tmp.path().join("phone-archive");
        let aci = device.aci.parse().expect("the account's ACI");
        write_archive(&path, key, aci, spec).expect("archive");
        path
    }

    /// The phone uploads `path` and answers the device's long-poll with its place.
    async fn upload(&self, device: &DeviceInfo, path: &std::path::Path) -> ArchivePlace {
        let place = self
            .harness
            .store_transfer_archive(path)
            .await
            .expect("storeTransferArchive");
        self.harness
            .provide_transfer_archive(&device.device_ref, &place)
            .await
            .expect("provideTransferArchive");
        place
    }

    /// Alice sends a text that is queued on the server for our device; returns its timestamp.
    async fn alice_says(&self, device: &DeviceInfo, text: &str) -> u64 {
        self.harness
            .send_text(&SendText::new(&self.alice.aci, &device.device_ref, text))
            .await
            .expect("sendText")
            .timestamp
    }

    /// Whether Alice's message `timestamp` reached the core.
    fn arrived(&self, timestamp: u64) -> bool {
        self.sink
            .message(Self::user(&self.alice).as_str(), &timestamp.to_string())
            .is_some()
    }

    /// Waits until Alice's message `timestamp` reached the core, once.
    async fn expect_arrival(&self, device: &DeviceInfo, timestamp: u64, text: &str) {
        let author = Self::user(&self.alice);
        let message = self
            .sink
            .wait_message(author.as_str(), &timestamp.to_string(), WAIT)
            .await
            .unwrap_or_else(|| panic!("{text:?} reached the core"));
        assert_eq!(message.body, text);
        self.harness
            .wait_for_deliveries(&device.device_ref, WAIT)
            .await
            .expect("every envelope is acknowledged once receiving started");
        assert_eq!(
            self.sink
                .deliveries(author.as_str(), &timestamp.to_string()),
            1,
            "exactly once"
        );
    }

    /// Nothing is taken off the socket: the message is not in the core, and the server still
    /// waits for the acknowledgement of what it delivered.
    async fn expect_held(&self, device: &DeviceInfo, timestamp: u64) {
        let unacked = self
            .harness
            .wait_for_deliveries(&device.device_ref, Duration::from_millis(1500))
            .await;
        assert!(
            unacked.is_err(),
            "an envelope was acknowledged while the transfer held receiving back"
        );
        assert!(!self.arrived(timestamp), "a message reached the core early");
    }

    /// One integer of the transfer's row.
    fn row(&self, expr: &str) -> Option<i64> {
        self.files
            .peek_i64(&format!("SELECT {expr} FROM history_transfer"))
            .expect("history_transfer")
    }

    async fn wait_state(&self, state: &str) {
        self.files
            .wait_for_i64(
                state,
                &format!("SELECT count(*) FROM history_transfer WHERE state = '{state}'"),
                |n| n == Some(1),
            )
            .await;
    }

    /// The transfer is over as `state` with `reason`, and nothing of it is left: no key, no
    /// place and no file.
    fn expect_over(&self, state: &str, reason: Option<&str>) {
        let reason_sql = match reason {
            Some(reason) => format!("reason = '{reason}'"),
            None => "reason IS NULL".to_owned(),
        };
        assert_eq!(
            self.row(&format!("state = '{state}' AND {reason_sql}")),
            Some(1),
            "the transfer ended as {state} ({reason:?})"
        );
        assert_eq!(
            self.row("key IS NULL AND cdn IS NULL AND cdn_key IS NULL AND file IS NULL"),
            Some(1),
            "the key and the archive's place are wiped"
        );
        assert!(
            !self.transfer_dir().exists(),
            "the archive file and its directory are removed"
        );
    }

    fn transfer_dir(&self) -> PathBuf {
        self.files.dir.join("transfer")
    }

    fn archive_file(&self) -> PathBuf {
        self.transfer_dir().join("archive")
    }
}

/// (a) and (i): a fresh link with a transfer, end to end, and the time the link took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_link_takes_the_history_transfer_before_it_receives() {
    use std::os::unix::fs::PermissionsExt as _;

    let Some(world) = World::start().await else {
        return;
    };
    // A read that lasts about two seconds, so the test can look while it runs.
    let extra = 2_000;
    let network = network().with_history_transfer_frame_delay(Duration::from_millis(1));
    let mut linked = world.link(network, true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));

    // The URL asked for the transfer, the phone sent its key, and the key is kept with the
    // account (test data: a random key of the mock).
    assert!(
        linked.url.ends_with("&capabilities=backup5"),
        "the URL offers backup5"
    );
    assert_eq!(device.capabilities, ["backup5"]);
    assert!(device.sent_transfer_key);
    let hex: String = key.iter().map(|b| format!("{b:02X}")).collect();
    world.wait_state("waiting").await;
    assert_eq!(
        world.row(&format!("hex(key) = '{hex}'")),
        Some(1),
        "the key is kept"
    );
    assert_eq!(
        world.sink.event_names().first().map(String::as_str),
        Some("AccountReady"),
        "the account is durable in the core with the key behind it"
    );

    // A message waits on the server from before the transfer.
    let early = world
        .alice_says(&device, "sent while the phone prepared")
        .await;

    // The long-poll: the server has nothing, answers 204, and is asked again.
    let polls = loop {
        let polls = world.harness.transfer_polls().await.expect("transferStats");
        if polls.iter().filter(|p| p.status == 204).count() >= 2 {
            break polls;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        polls.iter().all(|p| p.timeout == Some(1.0)),
        "every long-poll states its timeout: {polls:?}"
    );
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    world.expect_held(&device, early).await;
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'storage_sync'")
            .expect("jobs"),
        Some(1),
        "the Storage Service sync has not run"
    );
    // The sync requests of the link are not held: the phone got them.
    world
        .harness
        .wait_for_sync(&world.phone.device_ref, &device.device_ref, WAIT)
        .await
        .expect("sync requests reached the phone");

    // The phone uploads its archive and answers the long-poll.
    let path = world.archive(
        &device,
        &key,
        &ArchiveSpec {
            extra_messages: extra,
            ..ArchiveSpec::default()
        },
    );
    let size = std::fs::metadata(&path).expect("archive").len();
    assert!(size > 1024 * 1024, "an archive of megabytes: {size}");
    let place = world.upload(&device, &path).await;

    // Downloading (with the size the CDN stated), then reading.
    let downloading = linked
        .seen
        .until(&mut linked.core, "Downloading with its size", |s| {
            matches!(s, HistoryTransfer::Downloading { total: Some(_), .. })
        })
        .await;
    assert!(
        matches!(downloading, HistoryTransfer::Downloading { total: Some(total), .. } if total == size),
        "{downloading:?}"
    );
    linked
        .seen
        .until(&mut linked.core, "Reading", |s| {
            *s == HistoryTransfer::Reading
        })
        .await;
    // While it reads: the file is whole, private, and still nothing is received.
    let file = world.archive_file();
    assert_eq!(
        std::fs::metadata(&file).expect("the archive file").len(),
        size
    );
    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&world.transfer_dir()), 0o700);
    assert_eq!(mode(&file), 0o600);
    assert!(
        !world.arrived(early),
        "nothing is received while the archive is read"
    );
    assert!(
        linked.seen.login_done_after.is_none(),
        "the link is not done yet"
    );

    // Done: every frame was read, and nothing of the transfer is left.
    linked
        .seen
        .until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done)
        .await;
    world.expect_over("done", None);
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames_of(extra)).expect("frames")),
        "every frame of the archive was read"
    );
    assert_eq!(
        linked.seen.names(),
        ["Waiting", "Downloading", "Reading", "Done"],
        "{:?}",
        linked.seen.transfer
    );
    let downloads = world.harness.downloads().await.expect("downloadStats");
    let ours: Vec<_> = downloads.iter().filter(|d| d.key == place.key).collect();
    assert_eq!(ours.len(), 1, "one request: {ours:?}");
    assert_eq!(
        (ours[0].range.as_deref(), ours[0].status, ours[0].sent),
        (None, 200, size)
    );

    // Only now does the message that waited arrive, and the link finish.
    world
        .expect_arrival(&device, early, "sent while the phone prepared")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    assert_eq!(
        linked.seen.login_done_after,
        Some(linked.seen.transfer.len()),
        "the login is done after the transfer's last state"
    );
    linked.core.wait_online().await;
    world
        .files
        .wait_for_i64(
            "the Storage Service sync ran",
            "SELECT count(*) FROM jobs WHERE kind = 'storage_sync'",
            |n| n == Some(0),
        )
        .await;
    let late = world.alice_says(&device, "sent afterwards").await;
    world.expect_arrival(&device, late, "sent afterwards").await;

    // (i) The 60 seconds: from the phone's provisioning message to the answered link request.
    let marks = link_marks().lock().expect("marks").clone();
    let mark = marks.last().expect("the link's measuring mark");
    let ms = |name: &str| mark.get(name).copied().unwrap_or(0) as f64 / 1000.0;
    println!(
        "LINK TIMING (mock, loopback): phone sent the provisioning message -> link request at \
         the server {} ms -> answered {} ms. Client side: envelope taken off its socket -> link \
         answered {:.1} ms (decryption, the account check and the \"Linking\" step {:.1} ms, \
         keys and request body {:.1} ms, unauthenticated socket {:.1} ms, PUT /v1/devices/link \
         {:.1} ms), -> account stored {:.1} ms (socket close and the storing transaction {:.1} ms)",
        device.link_request_after_ms.unwrap_or(0),
        device.linked_after_ms.unwrap_or(0),
        ms("linked_us"),
        ms("taken_us"),
        ms("keys_us"),
        ms("connect_us"),
        ms("request_us"),
        ms("total_us"),
        ms("store_us"),
    );
    let linked_after = device.linked_after_ms.expect("the link request was seen");
    assert!(
        linked_after < 10_000,
        "the phone waits 60 s for the new device; the link took {linked_after} ms on loopback"
    );

    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (b) The phone sends no key (its user chose "Don't transfer", or it is too old to know
/// `backup5`): an ordinary link, and receiving starts at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_that_sends_no_key_links_as_before() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(network(), false).await;
    assert!(
        linked.url.ends_with("&capabilities=backup5"),
        "a fresh link asks when the offer is on"
    );
    assert!(!linked.device.sent_transfer_key);
    linked.seen.until_login_done(&mut linked.core).await;
    assert!(
        linked.seen.transfer.is_empty(),
        "{:?}",
        linked.seen.transfer
    );
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM history_transfer")
            .expect("history_transfer"),
        Some(0),
        "no transfer was stored"
    );
    let text = world.alice_says(&linked.device, "no transfer").await;
    world
        .expect_arrival(&linked.device, text, "no transfer")
        .await;
    assert!(
        world
            .harness
            .transfer_polls()
            .await
            .expect("stats")
            .is_empty(),
        "no long-poll without a key"
    );
    assert!(!world.transfer_dir().exists());
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Runs a transfer that the phone ends with `error` and returns what the core saw.
async fn ended_by_the_phone(world: &World, error: &str) -> (Linked, u64) {
    let mut linked = world.link(network(), true).await;
    let held = world
        .alice_says(&linked.device, "waits for the answer")
        .await;
    world.wait_state("waiting").await;
    world.expect_held(&linked.device, held).await;
    world
        .harness
        .provide_transfer_error(&linked.device.device_ref, error)
        .await
        .expect("provideTransferArchive");
    linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    (linked, held)
}

/// (c) `CONTINUE_WITHOUT_UPLOAD`: skipped, and receiving starts. The first long-poll fails
/// (500): the wait backs off and asks again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_that_continues_without_upload_ends_the_transfer_as_skipped() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .hold_transfer_archive(false, 1)
        .await
        .expect("holdTransferArchive");
    let (mut linked, held) = ended_by_the_phone(&world, "CONTINUE_WITHOUT_UPLOAD").await;
    assert_eq!(linked.seen.transfer.last(), Some(&HistoryTransfer::Skipped));
    world.expect_over("skipped", Some("phone_declined"));
    let polls = world.harness.transfer_polls().await.expect("stats");
    assert_eq!(polls[0].status, 500, "{polls:?}");
    assert!(
        polls.len() >= 2,
        "a failed long-poll is followed by another: {polls:?}"
    );
    world
        .expect_arrival(&linked.device, held, "waits for the answer")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (d) `RELINK_REQUESTED`: failed with the kind a UI answers with "link again"; receiving
/// starts all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_that_asks_for_a_relink_fails_the_transfer_and_receiving_starts() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut linked, held) = ended_by_the_phone(&world, "RELINK_REQUESTED").await;
    assert_eq!(
        linked.seen.transfer.last(),
        Some(&HistoryTransfer::Failed(HistoryTransferFailure::LinkAgain))
    );
    world.expect_over("failed", Some("link_again"));
    world
        .expect_arrival(&linked.device, held, "waits for the answer")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (e) The user skips while the transfer waits for the phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_user_skips_while_the_transfer_waits() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(network(), true).await;
    let held = world.alice_says(&linked.device, "waits for the skip").await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    assert!(HistoryTransfer::Waiting.can_skip());
    world.expect_held(&linked.device, held).await;
    linked.core.send(Command::SkipHistoryTransfer).await;
    linked
        .seen
        .until(&mut linked.core, "Skipped", |s| {
            *s == HistoryTransfer::Skipped
        })
        .await;
    world.expect_over("skipped", Some("user"));
    world
        .expect_arrival(&linked.device, held, "waits for the skip")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    // A second skip finds nothing to skip and changes nothing.
    linked.core.send(Command::SkipHistoryTransfer).await;
    linked.core.wait_online().await;
    world.expect_over("skipped", Some("user"));
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (e) The user skips while the archive downloads: the part that is there is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_user_skips_while_the_archive_downloads() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(network(), true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world.alice_says(&linked.device, "waits for the skip").await;
    let path = world.archive(
        &device,
        &key,
        &ArchiveSpec {
            extra_messages: 500,
            ..ArchiveSpec::default()
        },
    );
    let size = std::fs::metadata(&path).expect("archive").len();
    // The transfer breaks off after half the file and then brings nothing more.
    let half = size / 2;
    let mut afters = vec![half];
    afters.extend(std::iter::repeat_n(0, 500));
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "afters": afters }))
        .await
        .expect("breakDownloads");
    world.upload(&device, &path).await;
    linked
        .seen
        .until(
            &mut linked.core,
            "half the file",
            |s| matches!(s, HistoryTransfer::Downloading { done, .. } if *done == half),
        )
        .await;
    assert_eq!(
        std::fs::metadata(world.archive_file()).expect("part").len(),
        half
    );
    linked.core.send(Command::SkipHistoryTransfer).await;
    linked
        .seen
        .until(&mut linked.core, "Skipped", |s| {
            *s == HistoryTransfer::Skipped
        })
        .await;
    world.expect_over("skipped", Some("user"));
    world
        .expect_arrival(&linked.device, held, "waits for the skip")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (f) A restart in each of waiting, downloading (half the file there) and reading: the
/// transfer goes on from the stored state and ends done; the file is not asked for from
/// byte 0 a second time; nothing is received before the end, across every restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_in_every_state_goes_on_to_done() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(network(), true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world
        .alice_says(&device, "waits through three restarts")
        .await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    world.expect_held(&device, held).await;

    // Restart while waiting: the login the transfer belonged to is gone with the process.
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    assert_eq!(world.row("state = 'waiting' AND length(key) = 32"), Some(1));
    let polls_before = world.harness.transfer_polls().await.expect("stats").len();
    let mut core = world.restart(network());
    let mut seen = Seen::default();
    core.send(Command::Connect).await;
    seen.until(&mut core, "Waiting again", |s| {
        *s == HistoryTransfer::Waiting
    })
    .await;
    loop {
        let polls = world.harness.transfer_polls().await.expect("stats");
        if polls.len() > polls_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    world.expect_held(&device, held).await;

    // The archive: its download breaks off after half the file and brings nothing more.
    let extra = 400;
    let path = world.archive(
        &device,
        &key,
        &ArchiveSpec {
            extra_messages: extra,
            ..ArchiveSpec::default()
        },
    );
    let size = std::fs::metadata(&path).expect("archive").len();
    let half = size / 2;
    let mut afters = vec![half];
    afters.extend(std::iter::repeat_n(0, 500));
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "afters": afters }))
        .await
        .expect("breakDownloads");
    let place = world.upload(&device, &path).await;
    seen.until(
        &mut core,
        "half the file",
        |s| matches!(s, HistoryTransfer::Downloading { done, .. } if *done == half),
    )
    .await;

    // Restart while downloading, half the file on disk.
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(
        world.row("state = 'downloading' AND length(key) = 32"),
        Some(1)
    );
    assert_eq!(
        std::fs::metadata(world.archive_file()).expect("part").len(),
        half
    );
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "stop": true }))
        .await
        .expect("breakDownloads stop");
    let before = world.harness.downloads().await.expect("downloadStats");
    let from_zero = |downloads: &[support::harness::DownloadStat]| {
        downloads
            .iter()
            .filter(|d| d.key == place.key && d.range.is_none())
            .count()
    };
    assert_eq!(from_zero(&before), 1, "{before:?}");
    // The read after this restart is slow, so the next restart finds it reading.
    let slow = network().with_history_transfer_frame_delay(Duration::from_millis(20));
    let mut core = world.restart(slow);
    let mut seen = Seen::default();
    core.send(Command::Connect).await;
    // The download goes on from the bytes on disk.
    seen.until(
        &mut core,
        "Downloading again, behind the half",
        |s| matches!(s, HistoryTransfer::Downloading { done, .. } if *done >= half),
    )
    .await;
    seen.until(&mut core, "Reading", |s| *s == HistoryTransfer::Reading)
        .await;
    let after = world.harness.downloads().await.expect("downloadStats");
    assert_eq!(
        from_zero(&after),
        1,
        "the file is not asked for from byte 0 again"
    );
    let rest = after.last().expect("the request for the rest");
    assert_eq!(
        (rest.range.as_deref(), rest.status, rest.start, rest.sent),
        (
            Some(format!("bytes={half}-").as_str()),
            206,
            half,
            size - half
        ),
        "{rest:?}"
    );
    assert_eq!(
        std::fs::metadata(world.archive_file())
            .expect("whole")
            .len(),
        size
    );

    // Restart while reading.
    world.wait_state("reading").await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(world.row("state = 'reading' AND length(key) = 32"), Some(1));
    assert!(
        !world.arrived(held),
        "nothing was received through the restarts"
    );
    let mut core = world.restart(network());
    let mut seen = Seen::default();
    core.send(Command::Connect).await;
    seen.until(&mut core, "Done", |s| *s == HistoryTransfer::Done)
        .await;
    assert_eq!(seen.names(), ["Reading", "Done"], "{:?}", seen.transfer);
    world.expect_over("done", None);
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames_of(extra)).expect("frames"))
    );
    assert_eq!(
        world.row("read_starts"),
        Some(1),
        "the read that the shutdown stopped in an orderly way does not count as a start"
    );
    let end = world.harness.downloads().await.expect("downloadStats");
    assert_eq!(
        end.len(),
        after.len(),
        "a read that starts over downloads nothing"
    );

    // Receiving starts, and the account finishes its link's sync.
    world
        .expect_arrival(&device, held, "waits through three restarts")
        .await;
    core.wait_online().await;
    world
        .files
        .wait_for_i64(
            "the link's sync finished",
            "SELECT post_link_sync = 'done' FROM account",
            |v| v == Some(1),
        )
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // A start after the end touches nothing, and says the end again (until it is dismissed:
    // `an_end_is_reported_at_every_start_and_connect_until_it_is_dismissed`).
    let mut core = world.restart(network());
    let mut seen = Seen::default();
    seen.until(&mut core, "Done, at the start", |s| {
        *s == HistoryTransfer::Done
    })
    .await;
    core.send(Command::Connect).await;
    core.wait_online().await;
    world.expect_over("done", None);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The history transfer states among the ephemeral events of the next `within`.
async fn transfer_states_within(core: &mut FakeCore, within: Duration) -> Vec<HistoryTransfer> {
    let deadline = tokio::time::Instant::now() + within;
    let mut states = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::HistoryTransfer(state)) => states.push(state),
            Some(_) => {}
            None => return states,
        }
    }
}

/// Finding 2. The end of a transfer is not told once: the row keeps the state and its
/// reason, and every start of the actor and every `Connect` says it again, so a UI that was
/// not listening when the transfer failed learns of it and of its kind. After
/// `DismissHistoryTransfer` the row is gone and nothing is reported any more. A transfer
/// that is not over cannot be dismissed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_end_is_reported_at_every_start_and_connect_until_it_is_dismissed() {
    let Some(world) = World::start().await else {
        return;
    };
    let failed = HistoryTransfer::Failed(HistoryTransferFailure::LinkAgain);
    let mut linked = world.link(network(), true).await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    // Not over: a dismissal changes nothing, the wait goes on.
    linked.core.send(Command::DismissHistoryTransfer).await;
    linked.core.send(Command::Connect).await;
    linked
        .seen
        .until(&mut linked.core, "Waiting, said again at Connect", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    assert_eq!(world.row("state = 'waiting' AND length(key) = 32"), Some(1));

    world
        .harness
        .provide_transfer_error(&linked.device.device_ref, "RELINK_REQUESTED")
        .await
        .expect("provideTransferArchive");
    linked
        .seen
        .until(&mut linked.core, "Failed", |s| *s == failed)
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    linked.core.wait_online().await;
    // The same actor says it again when it is told to connect.
    linked.core.send(Command::Connect).await;
    assert_eq!(
        transfer_states_within(&mut linked.core, Duration::from_millis(700)).await,
        [failed],
        "Connect repeats the end"
    );
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);

    // Two more starts: each says how the transfer ended, at the start and at Connect, with
    // the kind of the failure.
    for start in 1..=2 {
        let mut core = world.restart(network());
        assert_eq!(
            transfer_states_within(&mut core, Duration::from_millis(700)).await,
            [failed],
            "start {start} reports the stored end"
        );
        core.send(Command::Connect).await;
        core.wait_online().await;
        assert_eq!(
            transfer_states_within(&mut core, Duration::from_millis(700)).await,
            [failed],
            "start {start}: Connect reports it again"
        );
        world.expect_over("failed", Some("link_again"));
        assert_eq!(core.shutdown().await, Exit::Shutdown);
    }

    // The user has seen it: dismissed. Nothing is reported from then on, in this run or
    // after another start.
    let mut core = world.restart(network());
    assert_eq!(
        transfer_states_within(&mut core, Duration::from_millis(700)).await,
        [failed]
    );
    core.send(Command::DismissHistoryTransfer).await;
    world
        .files
        .wait_for_i64(
            "the row is removed",
            "SELECT count(*) FROM history_transfer",
            |n| n == Some(0),
        )
        .await;
    core.send(Command::Connect).await;
    core.wait_online().await;
    assert!(
        transfer_states_within(&mut core, Duration::from_millis(700))
            .await
            .is_empty(),
        "nothing after the dismissal"
    );
    // A second dismissal finds nothing.
    core.send(Command::DismissHistoryTransfer).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let mut core = world.restart(network());
    core.send(Command::Connect).await;
    core.wait_online().await;
    assert!(
        transfer_states_within(&mut core, Duration::from_millis(700))
            .await
            .is_empty(),
        "nothing at the next start"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 3. An `error` value of the long-poll that this version does not know is a newer
/// protocol: the transfer fails as "update Kourier", not as a download that failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_this_version_does_not_know_asks_for_an_update() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut linked, held) = ended_by_the_phone(&world, "A_REASON_FROM_THE_FUTURE").await;
    assert_eq!(
        linked.seen.transfer.last(),
        Some(&HistoryTransfer::Failed(HistoryTransferFailure::UpdateApp))
    );
    world.expect_over("failed", Some("update_app"));
    world
        .expect_arrival(&linked.device, held, "waits for the answer")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 4. A server (or a proxy in front of it) that answers the long-poll with 204 at
/// once, instead of holding it, is not asked in a loop: the next request waits for the
/// backoff (1 s, 2 s, 3 s, 5 s …), so a handful of requests go out where a spinning client
/// would send thousands. Once the server holds requests again, the wait goes on as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_poll_that_is_not_held_is_asked_again_after_a_pause() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .answer_transfer_archive_at_once(true)
        .await
        .expect("holdTransferArchive");
    // A long-poll of five seconds: a 204 within milliseconds was plainly not held.
    let network = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(5));
    let mut linked = world.link(network, true).await;
    let held = world
        .alice_says(&linked.device, "waits for the pause")
        .await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let polls = world.harness.transfer_polls().await.expect("stats");
    assert!(
        polls.iter().all(|p| p.status == 204),
        "every one was answered at once: {polls:?}"
    );
    // At 0 s, 1 s, 3 s and (about) 6 s after the first: three or four in four and a half
    // seconds, with room for a slow machine. Without the pause there are thousands.
    assert!(
        (2..=5).contains(&polls.len()),
        "{} long-polls in 4.5 s",
        polls.len()
    );
    let gaps: Vec<u64> = polls.windows(2).map(|w| w[1].at - w[0].at).collect();
    assert!(
        gaps.iter().all(|gap| *gap >= 900),
        "at least the first step of the backoff lies between two requests: {gaps:?} ms"
    );
    assert_eq!(world.row("state = 'waiting'"), Some(1), "the wait goes on");
    world.expect_held(&linked.device, held).await;

    // The server holds requests again, and the phone answers: the wait ends as it should.
    world
        .harness
        .answer_transfer_archive_at_once(false)
        .await
        .expect("holdTransferArchive");
    world
        .harness
        .provide_transfer_error(&linked.device.device_ref, "CONTINUE_WITHOUT_UPLOAD")
        .await
        .expect("provideTransferArchive");
    linked
        .seen
        .until(&mut linked.core, "Skipped", |s| {
            *s == HistoryTransfer::Skipped
        })
        .await;
    world.expect_over("skipped", Some("phone_declined"));
    world
        .expect_arrival(&linked.device, held, "waits for the pause")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 5 (b). A long-poll the server refuses for good (a 4xx that is not 408 or 429)
/// ends the wait at once, as skipped: one request, no hour of asking. A 429 is asked again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_long_poll_refused_for_good_ends_the_wait_as_skipped() {
    let Some(world) = World::start().await else {
        return;
    };
    // First a 429, which is "not now": asked again. Then the refusal.
    world
        .harness
        .refuse_transfer_archive(429, 1)
        .await
        .expect("holdTransferArchive");
    let mut linked = world.link(network(), true).await;
    let held = world
        .alice_says(&linked.device, "waits for the refusal")
        .await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    loop {
        let polls = world.harness.transfer_polls().await.expect("stats");
        if polls.len() >= 2 {
            assert_eq!(polls[0].status, 429, "{polls:?}");
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        world.row("state = 'waiting'"),
        Some(1),
        "a 429 does not end it"
    );
    let before = world.harness.transfer_polls().await.expect("stats").len();
    world
        .harness
        .refuse_transfer_archive(403, 1000)
        .await
        .expect("holdTransferArchive");
    let started = std::time::Instant::now();
    let last = linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    assert_eq!(last, HistoryTransfer::Skipped);
    world.expect_over("skipped", Some("refused"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "at once, not at the deadline two minutes away: {:?}",
        started.elapsed()
    );
    let polls = world.harness.transfer_polls().await.expect("stats");
    let refused = polls[before.saturating_sub(1)..]
        .iter()
        .filter(|p| p.status == 403)
        .count();
    assert_eq!(refused, 1, "one refusal is enough: {polls:?}");
    world
        .expect_arrival(&linked.device, held, "waits for the refusal")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 5 (a). The download as a whole has a deadline: a body that trickles (it breaks
/// off again and again, and each time a little more comes) never stalls for the stall time,
/// and still fails once the download has taken longer than its deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_that_trickles_past_its_deadline_fails_the_transfer() {
    let Some(world) = World::start().await else {
        return;
    };
    // A deadline of three seconds; the stall time stays far away (a minute).
    let bounded = network()
        .with_history_transfer_limits(1 << 30, Duration::from_secs(60))
        .with_history_transfer_download_bounds(
            Duration::from_secs(3),
            0,
            Duration::from_secs(60),
            Duration::from_secs(600),
        );
    let mut linked = world.link(bounded, true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world
        .alice_says(&linked.device, "waits for the trickle")
        .await;
    let path = world.archive(
        &device,
        &key,
        &ArchiveSpec {
            extra_messages: 2_000,
            ..ArchiveSpec::default()
        },
    );
    // Every request brings 64 bytes of a megabyte and breaks off: progress all the time,
    // never the file.
    world
        .harness
        .request_value(
            "breakDownloads",
            serde_json::json!({ "after": 64, "times": -1 }),
        )
        .await
        .expect("breakDownloads");
    world.upload(&device, &path).await;
    let started = std::time::Instant::now();
    let last = linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Download)
    );
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "by its deadline of 3 s, not by the stall time: {:?}",
        started.elapsed()
    );
    assert!(
        linked
            .seen
            .transfer
            .iter()
            .any(|s| matches!(s, HistoryTransfer::Downloading { done, .. } if *done >= 64)),
        "it was moving: {:?}",
        linked.seen.transfer
    );
    world.expect_over("failed", Some("download"));
    world
        .expect_arrival(&linked.device, held, "waits for the trickle")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The deadline of the download counts the time it RAN, not the wall clock: a download cut
/// off by quitting goes on from the bytes on disk however long the application stayed
/// closed (overnight, say), instead of failing at the next start and costing the user a
/// second link. Here the deadline is eight seconds, the application is closed for nine in
/// the middle of the download, and the transfer still ends done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_time_the_application_was_closed_does_not_count_for_the_download() {
    let Some(world) = World::start().await else {
        return;
    };
    let bounded = || {
        network().with_history_transfer_download_bounds(
            Duration::from_secs(8),
            0,
            Duration::from_secs(60),
            Duration::from_secs(600),
        )
    };
    let mut linked = world.link(bounded(), true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let path = world.archive(
        &device,
        &key,
        &ArchiveSpec {
            extra_messages: 400,
            ..ArchiveSpec::default()
        },
    );
    let half = std::fs::metadata(&path).expect("archive").len() / 2;
    // The download breaks off after half the file and brings nothing more.
    let mut afters = vec![half];
    afters.extend(std::iter::repeat_n(0, 500));
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "afters": afters }))
        .await
        .expect("breakDownloads");
    world.upload(&device, &path).await;
    linked
        .seen
        .until(
            &mut linked.core,
            "half the file",
            |s| matches!(s, HistoryTransfer::Downloading { done, .. } if *done == half),
        )
        .await;
    // The time it ran is stored with the progress, about once a second (a stop stores
    // nothing: it is a crash for the transfer).
    let ran = world
        .files
        .wait_for_i64(
            "the time the download ran",
            "SELECT download_ran_ms FROM history_transfer",
            |ms| ms.is_some_and(|ms| ms > 0),
        )
        .await
        .expect("the time it ran");
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    assert_eq!(world.row("state = 'downloading'"), Some(1));
    assert!(ran < 8_000, "under its deadline at the stop: {ran} ms");

    // Closed for longer than the whole deadline.
    tokio::time::sleep(Duration::from_secs(9)).await;
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "stop": true }))
        .await
        .expect("breakDownloads stop");
    let mut core = world.restart(bounded());
    let mut seen = Seen::default();
    core.send(Command::Connect).await;
    let last = seen
        .until(&mut core, "over", HistoryTransfer::is_over)
        .await;
    assert_eq!(
        last,
        HistoryTransfer::Done,
        "the nine seconds the application was closed are not the download's: {:?}",
        seen.transfer
    );
    assert!(
        world.row("download_ran_ms").expect("the time it ran") > ran,
        "the second run counted on from the first"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 6. A wait does not end by its deadline before the server was asked once more: an
/// archive that is ready when this device's clock says the wait is over (the clock jumped
/// ahead, the machine slept past the deadline, or the long-poll in flight was lost) is
/// taken, not skipped. Here the long-poll in flight is never answered, the phone uploads
/// meanwhile, and the deadline passes: the last question finds the archive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_that_is_ready_at_the_deadline_is_not_skipped() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .hold_transfer_archive(true, 0)
        .await
        .expect("holdTransferArchive");
    // The whole wait is five seconds; one long-poll asks for more than that.
    let short = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(5), Duration::from_secs(30));
    let mut linked = world.link(short, true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world.alice_says(&device, "waits for the last ask").await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    // The one long-poll of the wait is on its way and will never be answered.
    loop {
        let polls = world.harness.transfer_polls().await.expect("stats");
        if !polls.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The phone uploads; later requests would be answered, the one in flight is not.
    world
        .harness
        .hold_transfer_archive(false, 0)
        .await
        .expect("holdTransferArchive");
    let path = world.archive(&device, &key, &ArchiveSpec::default());
    world.upload(&device, &path).await;
    assert_eq!(
        world.row("state = 'waiting'"),
        Some(1),
        "nothing told the device yet"
    );

    // The deadline passes. The device asks once more and gets the archive.
    linked
        .seen
        .until(&mut linked.core, "Done", HistoryTransfer::is_over)
        .await;
    assert_eq!(
        linked.seen.names(),
        ["Waiting", "Downloading", "Reading", "Done"],
        "{:?}",
        linked.seen.transfer
    );
    world.expect_over("done", None);
    let polls = world.harness.transfer_polls().await.expect("stats");
    assert_eq!(
        polls.len(),
        2,
        "the long-poll and the last question: {polls:?}"
    );
    assert_eq!(polls[0].status, "silent");
    assert_eq!(
        (polls[1].status.clone(), polls[1].timeout),
        (serde_json::json!(200), Some(1.0)),
        "the last question asks for the shortest hold"
    );
    assert!(
        polls[1].at - polls[0].at >= 3_000,
        "it came at the deadline, not before: {polls:?}"
    );
    world
        .expect_arrival(&device, held, "waits for the last ask")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 8. A skip that arrives while the downloaded file is synced to disk is not lost:
/// the transfer ends as skipped instead of becoming a read, which cannot be skipped. (The
/// sync of this small file takes no time, so the test makes it last two seconds.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skip_during_the_sync_of_the_downloaded_file_is_honoured() {
    let Some(world) = World::start().await else {
        return;
    };
    let slow_disk = network().with_history_transfer_sync_delay(Duration::from_secs(2));
    let mut linked = world.link(slow_disk, true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world.alice_says(&device, "waits for the skip").await;
    let path = world.archive(&device, &key, &ArchiveSpec::default());
    let size = std::fs::metadata(&path).expect("archive").len();
    world.upload(&device, &path).await;
    // The whole file is on disk and the transfer still says "downloading": it syncs.
    let deadline = tokio::time::Instant::now() + WAIT;
    while std::fs::metadata(world.archive_file()).map_or(0, |meta| meta.len()) != size {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the file never came"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(world.row("state = 'downloading'"), Some(1));
    linked.core.send(Command::SkipHistoryTransfer).await;
    let last = linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    assert_eq!(last, HistoryTransfer::Skipped);
    assert!(
        !linked.seen.transfer.contains(&HistoryTransfer::Reading),
        "the skip came before the read: {:?}",
        linked.seen.transfer
    );
    world.expect_over("skipped", Some("user"));
    world
        .expect_arrival(&device, held, "waits for the skip")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Finding 10. The offer is OFF unless a network switches it on (the app does, except in a
/// run with `KOURIER_HISTORY_TRANSFER=0`): a fresh link's URL carries no capability, so a
/// phone exports and uploads nothing; and a key that a phone sends all the same is ignored:
/// no transfer is stored, no long-poll goes out, and receiving starts at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_transfer_is_not_offered_unless_it_is_switched_on() {
    let Some(world) = World::start().await else {
        return;
    };
    // What a run with the offer off gets: `SignalNetwork::new()` as it is (the app builds
    // its network with the offer on).
    // (1) A phone whose user would transfer: it is not asked, so it sends no key.
    world
        .harness
        .set_ephemeral_backup_key(&world.phone.device_ref)
        .await
        .expect("setEphemeralBackupKey");
    let mut core = FakeCore::start_with(
        SignalNetwork::new(),
        &world.env,
        &world.files,
        StartMode::Fresh,
        &world.sink,
    )
    .expect("spawn the actor");
    let (url, _) = core.begin_login(None).await;
    assert!(
        url.ends_with("&capabilities="),
        "the URL of a fresh link offers nothing while the offer is off"
    );
    assert!(!url.contains("backup5"));
    let device = world
        .harness
        .complete_provision(&url, &world.phone.device_ref, WAIT)
        .await
        .expect("completeProvision");
    assert!(device.capabilities.is_empty());
    assert!(!device.sent_transfer_key, "not asked: no key, no export");
    let mut seen = Seen::default();
    seen.until_login_done(&mut core).await;
    assert!(seen.transfer.is_empty(), "{:?}", seen.transfer);
    assert_eq!(world.row("count(*)"), Some(0), "no transfer was stored");
    let text = world.alice_says(&device, "no offer").await;
    world.expect_arrival(&device, text, "no offer").await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert!(
        world
            .harness
            .transfer_polls()
            .await
            .expect("stats")
            .is_empty()
    );
    world.harness.shutdown().await.expect("harness shutdown");

    // (2) A phone that sends its key although nothing asked for it: ignored.
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .force_ephemeral_backup_key(&world.phone.device_ref)
        .await
        .expect("setEphemeralBackupKey");
    let mut core = FakeCore::start_with(
        SignalNetwork::new(),
        &world.env,
        &world.files,
        StartMode::Fresh,
        &world.sink,
    )
    .expect("spawn the actor");
    let (url, _) = core.begin_login(None).await;
    assert!(url.ends_with("&capabilities="));
    let device = world
        .harness
        .complete_provision(&url, &world.phone.device_ref, WAIT)
        .await
        .expect("completeProvision");
    assert!(
        device.sent_transfer_key,
        "this phone sent a key that was not asked for"
    );
    let mut seen = Seen::default();
    seen.until_login_done(&mut core).await;
    assert!(seen.transfer.is_empty(), "{:?}", seen.transfer);
    assert_eq!(world.row("count(*)"), Some(0), "the key was not kept");
    let text = world.alice_says(&device, "an unasked key").await;
    world.expect_arrival(&device, text, "an unasked key").await;
    assert!(
        world
            .harness
            .transfer_polls()
            .await
            .expect("stats")
            .is_empty(),
        "no long-poll without an offer"
    );
    assert!(!world.transfer_dir().exists());
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Set (to a JSON [`ChildSpec`]) in the environment of the helper process that runs the actor
/// for the kill test; [`transfer_child_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_TRANSFER_TEST_CHILD";

/// What the helper process needs to run the actor on the parent's files.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    uuid: String,
    key: Vec<u8>,
    env: BackendEnv,
    sink: PathBuf,
    /// The pause after every frame of the archive's read, in ms.
    frame_delay_ms: u64,
}

/// The helper process of the kill test: runs the account actor on the files of `CHILD_ENV`
/// with `Resume`, connects, and goes on until its parent kills it.
#[test]
fn transfer_child_process() {
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
        let slow =
            network().with_history_transfer_frame_delay(Duration::from_millis(spec.frame_delay_ms));
        let core = FakeCore::start_with(slow, &spec.env, &files, StartMode::Resume, &sink)
            .expect("spawn the actor");
        core.send(Command::Connect).await;
        // Only reached when the parent did not kill this process.
        tokio::time::sleep(Duration::from_secs(120)).await;
        let _ = core.shutdown().await;
    });
}

/// Starts the helper process of [`transfer_child_process`] on the world's files.
fn spawn_child(world: &World, frame_delay_ms: u64) -> tokio::process::Child {
    let spec = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
        frame_delay_ms,
    };
    tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "transfer_child_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the helper process")
}

/// (f), as a crash: the process that runs the actor is killed (SIGKILL) while the transfer
/// waits, while it downloads with half the file on disk, and while it reads. Every next
/// process goes on from what the killed one had stored, the file is asked for from byte 0
/// only once, nothing is received before the end, and the last one ends done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_in_every_state_goes_on_to_done() {
    let Some(world) = World::start().await else {
        return;
    };
    // The link runs in this process and ends by a shutdown; from then on every actor is a
    // process of its own that is killed where the test wants it.
    let mut linked = world.link(network(), true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world.alice_says(&device, "waits through three kills").await;
    linked
        .seen
        .until(&mut linked.core, "Waiting", |s| {
            *s == HistoryTransfer::Waiting
        })
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);

    // Killed while it waits for the phone (it was long-polling: the server saw it ask).
    let polls_before = world.harness.transfer_polls().await.expect("stats").len();
    let mut child = spawn_child(&world, 0);
    loop {
        let polls = world.harness.transfer_polls().await.expect("stats");
        if polls.len() > polls_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    child.kill().await.expect("kill the waiting process");
    assert_eq!(world.row("state = 'waiting' AND length(key) = 32"), Some(1));

    // Killed while it downloads, half the file on disk.
    let extra = 400;
    let spec = ArchiveSpec {
        extra_messages: extra,
        ..ArchiveSpec::default()
    };
    let path = world.archive(&device, &key, &spec);
    let size = std::fs::metadata(&path).expect("archive").len();
    let half = size / 2;
    let mut afters = vec![half];
    afters.extend(std::iter::repeat_n(0, 500));
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "afters": afters }))
        .await
        .expect("breakDownloads");
    let place = world.upload(&device, &path).await;
    let mut child = spawn_child(&world, 0);
    let on_disk = || std::fs::metadata(world.archive_file()).map_or(0, |meta| meta.len());
    let deadline = tokio::time::Instant::now() + WAIT;
    while on_disk() != half {
        assert!(
            tokio::time::Instant::now() < deadline,
            "half the file never came"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    world.wait_state("downloading").await;
    child.kill().await.expect("kill the downloading process");
    assert_eq!(
        world.row("state = 'downloading' AND length(key) = 32"),
        Some(1)
    );
    assert_eq!(on_disk(), half, "what was written before the kill is there");
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "stop": true }))
        .await
        .expect("breakDownloads stop");

    // Killed while it reads (a slow read): it had asked for the rest of the file only.
    let mut child = spawn_child(&world, 20);
    world.wait_state("reading").await;
    child.kill().await.expect("kill the reading process");
    assert_eq!(world.row("state = 'reading' AND length(key) = 32"), Some(1));
    assert_eq!(on_disk(), size, "the file is whole");
    let downloads = world.harness.downloads().await.expect("downloadStats");
    let ours: Vec<_> = downloads.iter().filter(|d| d.key == place.key).collect();
    assert_eq!(
        ours.iter().filter(|d| d.range.is_none()).count(),
        1,
        "asked for from byte 0 once: {ours:?}"
    );
    let rest = ours.last().expect("the request for the rest");
    assert_eq!(
        (rest.range.as_deref(), rest.status, rest.sent),
        (Some(format!("bytes={half}-").as_str()), 206, size - half),
        "{rest:?}"
    );
    assert!(
        !world.arrived(held),
        "nothing was received through the kills"
    );

    // The last start, in this process: the read starts over and ends done.
    let mut core = world.restart(network());
    let mut seen = Seen::default();
    core.send(Command::Connect).await;
    seen.until(&mut core, "Done", |s| *s == HistoryTransfer::Done)
        .await;
    world.expect_over("done", None);
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames_of(extra)).expect("frames"))
    );
    assert_eq!(
        world.row("read_starts"),
        Some(2),
        "the read was started twice"
    );
    assert_eq!(
        world
            .harness
            .downloads()
            .await
            .expect("downloadStats")
            .len(),
        downloads.len(),
        "a read that starts over downloads nothing"
    );
    world
        .expect_arrival(&device, held, "waits through three kills")
        .await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Runs a transfer whose archive `shape` prepared (the file to upload, or `None` for a place
/// the CDN does not have) on `network`, to its end; checks that receiving starts and
/// nothing is left; returns the last state.
async fn ends_with(
    network: SignalNetwork,
    spec: &ArchiveSpec,
    damage: impl FnOnce(&std::path::Path),
    missing: bool,
) -> Option<(HistoryTransfer, World)> {
    let world = World::start().await?;
    let mut linked = world.link(network, true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world.alice_says(&linked.device, "waits for the end").await;
    world.wait_state("waiting").await;
    let path = world.archive(&device, &key, spec);
    damage(&path);
    if missing {
        let place = ArchivePlace {
            cdn: 3,
            key: "0123456789abcdef0123456789abcdef".to_owned(),
            size: 0,
        };
        world
            .harness
            .provide_transfer_archive(&linked.device.device_ref, &place)
            .await
            .expect("provideTransferArchive");
    } else {
        world.upload(&device, &path).await;
    }
    let last = linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    world
        .expect_arrival(&linked.device, held, "waits for the end")
        .await;
    linked.seen.until_login_done(&mut linked.core).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    Some((last, world))
}

/// (g) One flipped byte: failed as corrupt, before a single frame was read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_corrupted_archive_fails_the_transfer() {
    let flip = |path: &std::path::Path| {
        let mut bytes = std::fs::read(path).expect("archive");
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x01;
        std::fs::write(path, bytes).expect("archive");
    };
    let Some((last, world)) = ends_with(network(), &ArchiveSpec::default(), flip, false).await
    else {
        return;
    };
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Corrupt)
    );
    world.expect_over("failed", Some("corrupt"));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (g) A download that is cut short (the CDN has less than the phone made): corrupt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_truncated_archive_fails_the_transfer() {
    let cut = |path: &std::path::Path| {
        let bytes = std::fs::read(path).expect("archive");
        std::fs::write(path, &bytes[..bytes.len() - 1000]).expect("archive");
    };
    let spec = ArchiveSpec {
        extra_messages: 20,
        ..ArchiveSpec::default()
    };
    let Some((last, world)) = ends_with(network(), &spec, cut, false).await else {
        return;
    };
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Corrupt)
    );
    world.expect_over("failed", Some("corrupt"));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (g) An archive that is authentic and stops decoding half-way: corrupt, although frames
/// were read before the break.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_that_breaks_half_way_fails_the_transfer() {
    let spec = ArchiveSpec {
        extra_messages: 50,
        broken_after: Some(30),
        ..ArchiveSpec::default()
    };
    let Some((last, world)) = ends_with(network(), &spec, |_| {}, false).await else {
        return;
    };
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Corrupt)
    );
    world.expect_over("failed", Some("corrupt"));
    assert_eq!(
        world.row("frames"),
        None,
        "no frame count of an archive that did not end"
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (g) An archive of a version this reader does not know: "update Kourier".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_of_another_version_asks_for_an_update() {
    let spec = ArchiveSpec {
        version: 2,
        ..ArchiveSpec::default()
    };
    let Some((last, world)) = ends_with(network(), &spec, |_| {}, false).await else {
        return;
    };
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::UpdateApp)
    );
    world.expect_over("failed", Some("update_app"));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (g) An archive larger than the cap: failed as too large, and the part that came is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_larger_than_the_cap_fails_the_transfer() {
    let small = network().with_history_transfer_limits(64 * 1024, Duration::from_secs(30));
    let spec = ArchiveSpec {
        extra_messages: 300,
        ..ArchiveSpec::default()
    };
    let Some((last, world)) = ends_with(small, &spec, |_| {}, false).await else {
        return;
    };
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::TooLarge)
    );
    world.expect_over("failed", Some("too_large"));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (g) The CDN has no such archive (404): skipped, as in Signal Desktop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_the_cdn_does_not_have_ends_the_transfer_as_skipped() {
    let Some((last, world)) = ends_with(network(), &ArchiveSpec::default(), |_| {}, true).await
    else {
        return;
    };
    assert_eq!(last, HistoryTransfer::Skipped);
    world.expect_over("skipped", Some("not_found"));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// A download that makes no progress for the stall time fails for good, and the part that
/// came is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_that_stalls_for_too_long_fails_the_transfer() {
    let Some(world) = World::start().await else {
        return;
    };
    let stalls = network().with_history_transfer_limits(1 << 30, Duration::from_secs(2));
    let mut linked = world.link(stalls, true).await;
    let (device, key) = (linked.device.clone(), linked.key.expect("key"));
    let held = world
        .alice_says(&linked.device, "waits for the stall")
        .await;
    let path = world.archive(
        &device,
        &key,
        &ArchiveSpec {
            extra_messages: 200,
            ..ArchiveSpec::default()
        },
    );
    let mut afters = vec![4096];
    afters.extend(std::iter::repeat_n(0, 500));
    world
        .harness
        .request_value("breakDownloads", serde_json::json!({ "afters": afters }))
        .await
        .expect("breakDownloads");
    world.upload(&device, &path).await;
    let last = linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Download)
    );
    world.expect_over("failed", Some("download"));
    world
        .expect_arrival(&linked.device, held, "waits for the stall")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The wait for the phone has an end: a server that never answers the long-poll is given
/// up at the deadline, as skipped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wait_for_the_phone_ends_at_its_deadline() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .hold_transfer_archive(true, 0)
        .await
        .expect("holdTransferArchive");
    let short = SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(4), Duration::from_secs(2));
    let mut linked = world.link(short, true).await;
    let held = world
        .alice_says(&linked.device, "waits for the deadline")
        .await;
    let last = linked
        .seen
        .until(&mut linked.core, "over", HistoryTransfer::is_over)
        .await;
    assert_eq!(last, HistoryTransfer::Skipped);
    world.expect_over("skipped", Some("deadline"));
    let polls = world.harness.transfer_polls().await.expect("stats");
    assert!(
        polls.iter().all(|p| p.status == "silent"),
        "the server never answered: {polls:?}"
    );
    world
        .expect_arrival(&linked.device, held, "waits for the deadline")
        .await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The app's link (`InitialSync::ContactsAndStorage`: `Done` waits for the phone's contact
/// sync and the first Storage Service sync) with a transfer in front of it: no contact
/// reaches the core while the transfer holds receiving back, and once it is over the link
/// syncs as it always did and only then reports `Done`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_links_own_sync_comes_after_the_transfer() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .set_ephemeral_backup_key(&world.phone.device_ref)
        .await
        .expect("setEphemeralBackupKey");
    let mut core = FakeCore::start_full(
        network(),
        &world.env,
        &world.files,
        StartMode::Fresh,
        &world.sink,
        kourier_backend_api::InitialSync::ContactsAndStorage,
    )
    .expect("spawn the actor");
    let (url, _) = core.begin_login(None).await;
    let device = world
        .harness
        .complete_provision(&url, &world.phone.device_ref, WAIT)
        .await
        .expect("completeProvision");
    assert!(device.sent_transfer_key);
    let mut seen = Seen::default();
    seen.until(&mut core, "Waiting", |s| *s == HistoryTransfer::Waiting)
        .await;
    // The phone answered the sync requests; its answers wait on the socket.
    world
        .harness
        .wait_for_sync(&world.phone.device_ref, &device.device_ref, WAIT)
        .await
        .expect("sync requests reached the phone");
    let unacked = world
        .harness
        .wait_for_deliveries(&device.device_ref, Duration::from_millis(1500))
        .await;
    assert!(
        unacked.is_err(),
        "the phone's sync answers are not taken yet"
    );
    assert!(
        world.sink.events("ContactUpsert").is_empty(),
        "no contact reaches the core before the transfer is over"
    );
    assert_eq!(
        world
            .files
            .peek_i64("SELECT contact_sync_done FROM account")
            .expect("account"),
        Some(0)
    );

    world
        .harness
        .provide_transfer_error(&device.device_ref, "CONTINUE_WITHOUT_UPLOAD")
        .await
        .expect("provideTransferArchive");
    seen.until(&mut core, "Skipped", |s| *s == HistoryTransfer::Skipped)
        .await;
    seen.until_login_done(&mut core).await;
    // `Done` came after the contact sync was applied and the Storage Service sync ran.
    assert_eq!(
        world
            .files
            .peek_i64("SELECT contact_sync_done FROM account")
            .expect("account"),
        Some(1),
        "the phone's contact sync was applied before Done"
    );
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'storage_sync'")
            .expect("jobs"),
        Some(0),
        "the Storage Service sync ran before Done"
    );
    assert!(
        !world.sink.events("ContactUpsert").is_empty(),
        "contacts arrived"
    );
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// (h) A relink of an account that kept its chats never asks for the transfer: no `backup5`
/// in the URL, so the phone sends no key although its user would transfer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_relink_does_not_ask_for_the_history() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .set_ephemeral_backup_key(&world.phone.device_ref)
        .await
        .expect("setEphemeralBackupKey");
    let relink = Relink {
        account: AccountId(1),
        expect_own: vec![World::user(&world.phone)],
    };
    let mut core = FakeCore::start_with(
        network(),
        &world.env,
        &world.files,
        StartMode::Fresh,
        &world.sink,
    )
    .expect("spawn the actor");
    let (url, _) = core.begin_login(Some(relink)).await;
    assert!(url.ends_with("&capabilities="), "a relink offers nothing");
    let device = world
        .harness
        .complete_provision(&url, &world.phone.device_ref, WAIT)
        .await
        .expect("completeProvision");
    assert!(device.capabilities.is_empty());
    assert!(!device.sent_transfer_key, "no backup5, no key");
    let mut seen = Seen::default();
    seen.until_login_done(&mut core).await;
    assert!(seen.transfer.is_empty());
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM history_transfer")
            .expect("history_transfer"),
        Some(0),
        "no transfer was stored"
    );
    let text = world.alice_says(&device, "after the relink").await;
    world
        .expect_arrival(&device, text, "after the relink")
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}
