//! Attachment backfill against `@signalapp/mock-server` (step S6, `src/backfill.rs`): the
//! core asks our phone to upload again the files of one message the server lost, and the
//! phone's answer reaches the core as `AttachmentRestore`. The harness plays the phone, the
//! test plays the core (`Command::RequestAttachments`, `Command::DownloadAttachment`).
//!
//! What is proven here:
//!
//! - one request per command, sent to our own account, naming the chat, the author (binary
//!   service id) and the message's ORIGINAL sent time; the job is stored once
//!   (`CommandPersisted`), reports `Sent` after the send, and is gone after it;
//! - an answer in steps (PENDING, then a pointer and TERMINAL_ERROR, then the long text)
//!   reaches the core slot by slot, and the pointer downloads to the phone's bytes;
//! - MESSAGE_NOT_FOUND reaches the core as such; an answer naming another chat carries that
//!   chat (the core finds no message there: `kourier-core` `tests/restore.rs`);
//! - a pointer without a digest downloads only against the attachment's plaintext name;
//! - an answer that arrives while Kourier is stopped lands after the restart, and a restart
//!   while the request waits sends no second request;
//! - nothing of a request or an answer reaches the log.
//!
//! What the CORE does with an answer (matching, the wait, the download it decides) is
//! `kourier-core` `tests/restore.rs`; that only the primary is listened to is the receive
//! unit test `backfill_answers_come_only_from_the_primary` (a scripted secondary cannot send).
//!
//! Skips with a message when the mock harness is not installed (set
//! `KOURIER_REQUIRE_MOCK_HARNESS=1` to fail instead). Tests print counts only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    dead_code,
    reason = "the shared support modules serve several test files"
)]

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{
    BackendEnv, BlobTarget, Command, Exit, InitialSync, RestoreSlot, RestoreUpdate, StartMode,
};
use kourier_model::{
    AttachmentId, BlobKey, CmdId, ConvKey, DownloadId, LoginStepKind, MsgTarget, TargetAuthor,
    UserId,
};
use kourier_signal::SignalNetwork;
use serde_json::{Value, json};
use sha2::Digest as _;
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT};
use support::harness::{
    BackfillAnswer, BackfillEntry, CreatePrimary, DeviceInfo, Harness, Primary,
};

/// How long "nothing more arrives" is watched.
const QUIET: Duration = Duration::from_millis(1500);

/// A client UUID the phone names its backfilled pointer with.
const UUID_A: [u8; 16] = [0x7a; 16];

/// What the actors of this process logged (every level): searched for secrets.
fn log_buffer() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOG: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOG.get_or_init(Default::default)
}

#[derive(Clone)]
struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        log_buffer()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{Layer as _, filter};
    let all = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(|| LogWriter)
        .with_filter(filter::filter_fn(|meta| {
            meta.target().starts_with("kourier")
        }));
    let shown = tracing_subscriber::fmt::layer()
        .with_test_writer()
        .with_filter(tracing_subscriber::EnvFilter::from_default_env());
    let _ = tracing_subscriber::registry()
        .with(all)
        .with(shown)
        .try_init();
}

/// The bytes of a picture the phone still has.
fn picture(seed: u32, len: u32) -> Vec<u8> {
    (0..len).map(|i| ((i * 7 + seed) % 251) as u8).collect()
}

fn plaintext_name(data: &[u8]) -> String {
    let hash = sha2::Sha256::digest(data);
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("plaintext:{hex}")
}

/// Our phone ("Myself", which links us) and Alice, a contact.
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    alice: Primary,
    sink: Sink,
    files: AccountFiles,
}

impl World {
    async fn start() -> Option<Self> {
        init_tracing();
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = support::fake_core::local_env(harness.handshake(), tmp.path()).expect("env");
        let alice = harness
            .create_primary(&CreatePrimary::named("Alice"))
            .await
            .expect("Alice");
        let phone = harness
            .create_primary(&CreatePrimary {
                contacts: vec![alice.device_ref.clone()],
                ..CreatePrimary::named("Myself")
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

    fn actor(&self, mode: StartMode) -> FakeCore {
        FakeCore::start_full(
            SignalNetwork::new().with_download_resume_pause(Duration::from_millis(50)),
            &self.env,
            &self.files,
            mode,
            &self.sink,
            InitialSync::LinkOnly,
        )
        .expect("spawn the actor")
    }

    /// Links a fresh account and waits until it is online and its link's syncs reached the
    /// phone (so the phone's session is there).
    async fn link(&self) -> (FakeCore, DeviceInfo) {
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
        let device = device.expect("linked device");
        self.harness
            .wait_for_sync(&self.phone.device_ref, &device.device_ref, WAIT)
            .await
            .expect("the link's sync requests reached the phone");
        (core, device)
    }

    fn alice_user(&self) -> UserId {
        UserId::parse(&format!("aci:{}", self.alice.aci)).expect("user id")
    }

    fn alice_chat(&self) -> ConvKey {
        ConvKey::Direct(self.alice_user())
    }

    /// Alice's text to our device; returns its timestamp once the device has it.
    async fn alice_says(&self, device: &DeviceInfo, text: &str) -> u64 {
        let at = self
            .harness
            .send_data(json!({
                "from": self.alice.aci, "to": device.device_ref, "text": text,
                "withProfileKey": true, "awaitDelivery": true, "timeout": 60_000,
            }))
            .await
            .expect("sendData")
            .timestamp;
        self.sink
            .wait_message(&format!("aci:{}", self.alice.aci), &at.to_string(), WAIT)
            .await
            .expect("Alice's message reached the core");
        at
    }

    fn target(&self, at: u64) -> MsgTarget {
        MsgTarget {
            conv: Some(self.alice_chat()),
            author: TargetAuthor::User(self.alice_user()),
            local: at.to_string().into(),
            alternates: Vec::new(),
        }
    }

    /// The core asks for the files of Alice's message `at` (command `cmd`).
    async fn ask(&self, core: &FakeCore, cmd: i64, at: u64) {
        core.send(Command::RequestAttachments {
            cmd: CmdId(cmd),
            conv: self.alice_chat(),
            target: self.target(at),
        })
        .await;
    }

    /// The phone answers about Alice's message `at`.
    fn answer(&self, at: u64) -> BackfillAnswer {
        BackfillAnswer {
            conversation: json!({ "aci": self.alice.aci }),
            author: self.alice.aci.clone(),
            timestamp: at,
            ..BackfillAnswer::default()
        }
    }

    async fn send(&self, device: &DeviceInfo, answer: &BackfillAnswer, await_delivery: bool) {
        self.harness
            .send_backfill_answer(
                &self.phone.device_ref,
                &device.device_ref,
                answer,
                await_delivery,
            )
            .await
            .expect("sendSync attachmentBackfillResponse");
    }

    /// The `AttachmentRestore` events the core got, as `(target, update)`.
    fn restores(&self) -> Vec<(MsgTarget, RestoreUpdate)> {
        self.sink
            .event_values("AttachmentRestore")
            .into_iter()
            .map(|v| {
                (
                    serde_json::from_value(v["target"].clone()).expect("a MsgTarget"),
                    serde_json::from_value(v["update"].clone()).expect("a RestoreUpdate"),
                )
            })
            .collect()
    }

    /// The command the `n`-th `AttachmentRestore` event (1-based) names: a report of the
    /// request (`Sent`, `NotSent`) names the core's command, an answer none.
    fn restore_cmd(&self, n: usize) -> Option<i64> {
        self.sink
            .event_values("AttachmentRestore")
            .get(n - 1)
            .and_then(|v| v["cmd"].as_i64())
    }

    /// Waits until the core got `n` `AttachmentRestore` events; returns the `n`-th update.
    async fn restore(&self, n: usize) -> (MsgTarget, RestoreUpdate) {
        self.sink.wait_events("AttachmentRestore", n, WAIT).await;
        self.restores()
            .into_iter()
            .nth(n - 1)
            .expect("the restore event")
    }

    fn jobs(&self) -> i64 {
        self.files
            .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'backfill_request'")
            .expect("backend.db")
            .unwrap_or(0)
    }

    async fn no_more_requests(&self) {
        assert!(
            self.harness
                .wait_for_backfill_request(&self.phone.device_ref, None, QUIET)
                .await
                .is_err(),
            "a second request reached the phone"
        );
    }

    fn dest(&self, name: &str) -> BlobTarget {
        BlobTarget {
            path: self
                .files
                .dir
                .join("blobs")
                .join("core")
                .join("s6")
                .join(name),
            key: BlobKey::new([0x42; 32]),
        }
    }

    /// Downloads `slot`'s pointer as attachment `id` with the names `net_ids`; returns the
    /// `AttachmentDone` result.
    async fn download(
        &self,
        core: &FakeCore,
        id: i64,
        slot: &RestoreSlot,
        net_ids: Vec<String>,
    ) -> Value {
        let RestoreSlot::File { remote, .. } = slot else {
            panic!("not a file: {slot:?}");
        };
        let before = self.sink.event_values("AttachmentDone").len();
        core.send(Command::DownloadAttachment {
            attachment: AttachmentId(id),
            request: DownloadId(id),
            remote: remote.clone(),
            dest: self.dest(&id.to_string()),
            net_ids: net_ids.into(),
        })
        .await;
        self.sink
            .wait_events("AttachmentDone", before + 1, WAIT)
            .await;
        self.sink
            .event_values("AttachmentDone")
            .into_iter()
            .find(|r| r["attachment"] == id)
            .unwrap_or_else(|| panic!("AttachmentDone {id}"))["result"]
            .clone()
    }

    fn read(&self, id: i64) -> Vec<u8> {
        kourier_blob::read_all(
            &self.dest(&id.to_string()).path,
            &kourier_blob::FileKey::from_bytes([0x42; 32]),
        )
        .expect("the downloaded blob")
        .to_vec()
    }
}

/// The base64 of an ACI's 16 bytes, as a binary service id of an ACI.
fn aci_b64(aci: &str) -> String {
    STANDARD.encode(uuid::Uuid::parse_str(aci).expect("aci").as_bytes())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_gets_one_request_naming_the_chat_the_author_and_the_original_time() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world.alice_says(&device, "a picture was here").await;
    let sends_before = world.harness.send_stats().await.expect("sendStats");

    world.ask(&core, 1, at).await;
    let request = world
        .harness
        .wait_for_backfill_request(&world.phone.device_ref, Some(&device.device_ref), WAIT)
        .await
        .expect("the request reached the phone");
    assert_eq!(request.sent_timestamp, Some(at), "{}", request.raw);
    assert_eq!(
        request.author.as_deref(),
        Some(aci_b64(&world.alice.aci).as_str()),
        "the author by its binary ACI"
    );
    assert_eq!(
        request
            .conversation
            .pointer("/identifier/threadServiceIdBinary")
            .or_else(|| request.conversation.get("threadServiceIdBinary"))
            .and_then(Value::as_str),
        Some(aci_b64(&world.alice.aci).as_str()),
        "the chat by Alice's binary ACI: {}",
        request.conversation
    );
    let (target, update) = world.restore(1).await;
    assert_eq!(update, RestoreUpdate::Sent);
    assert_eq!(target, world.target(at));
    assert!(
        world.restore_cmd(1).is_some(),
        "the report names the command of the request it is about"
    );
    assert!(
        world
            .sink
            .event_values("CommandPersisted")
            .iter()
            .any(|v| v["cmd"] == 1),
        "the command was confirmed"
    );
    world.no_more_requests().await;
    assert_eq!(world.jobs(), 0, "the job is done");
    let sends_after = world.harness.send_stats().await.expect("sendStats");
    let ours = |s: &support::harness::SendStats| {
        s.single
            .iter()
            .filter(|(id, _)| id.contains(&world.alice.aci))
            .map(|(_, n)| *n)
            .sum::<u64>()
    };
    assert_eq!(
        ours(&sends_after),
        ours(&sends_before),
        "nothing was sent to Alice"
    );

    // The same command again (the core's resend after a restart) finds its job: no request.
    world.ask(&core, 1, at).await;
    world.no_more_requests().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    // The request is URGENT, as Signal Android's requester sends it (module docs of
    // `src/backfill.rs`): the one new send to our own account says so.
    let flags =
        |s: &support::harness::SendStats| s.urgency.get(&device.aci).cloned().unwrap_or_default();
    let (before, after) = (flags(&sends_before), flags(&sends_after));
    let added = &after[before.len()..];
    assert_eq!(added.len(), 1, "one send to our own account: {added:?}");
    assert!(added[0].urgent, "the request is sent urgent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phones_answer_in_steps_lands_slot_by_slot_and_the_file_downloads() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world
        .alice_says(&device, "two pictures and a long text")
        .await;
    world.ask(&core, 1, at).await;
    world
        .harness
        .wait_for_backfill_request(&world.phone.device_ref, None, WAIT)
        .await
        .expect("the request");
    assert_eq!(world.restore(1).await.1, RestoreUpdate::Sent);

    // PENDING for every slot.
    let mut answer = world.answer(at);
    answer.attachments = Some(vec![BackfillEntry::pending(), BackfillEntry::pending()]);
    answer.long_text = Some(BackfillEntry::pending());
    world.send(&device, &answer, true).await;
    let (target, update) = world.restore(2).await;
    assert_eq!(target, world.target(at));
    assert_eq!(
        update,
        RestoreUpdate::Answer {
            attachments: vec![RestoreSlot::Pending, RestoreSlot::Pending],
            long_text: Some(RestoreSlot::Pending),
        }
    );

    // The first picture, uploaded again; the second is not on the phone; the long text.
    let first = picture(1, 40_000);
    let long = "a long text ".repeat(300).into_bytes();
    answer.attachments = Some(vec![
        BackfillEntry::upload(&first, "image/png", Some(UUID_A)),
        BackfillEntry::gone(),
    ]);
    answer.long_text = Some(BackfillEntry::upload(&long, "text/x-signal-plain", None));
    world.send(&device, &answer, true).await;
    let (_, update) = world.restore(3).await;
    let RestoreUpdate::Answer {
        attachments,
        long_text,
    } = update
    else {
        panic!("an answer: {update:?}");
    };
    let [file, gone] = &attachments[..] else {
        panic!("two slots: {attachments:?}");
    };
    assert_eq!(*gone, RestoreSlot::Gone);
    let RestoreSlot::File { net_ids, size, .. } = file else {
        panic!("a file: {file:?}");
    };
    assert_eq!(*size, 40_000);
    assert!(
        net_ids
            .expose()
            .contains(&format!("uuid:{}", "7a".repeat(16))),
        "named by its client UUID"
    );
    assert!(
        net_ids.expose().iter().any(|n| n.starts_with("digest:")),
        "and by its digest"
    );
    let downloads_before = world
        .harness
        .downloads()
        .await
        .expect("downloadStats")
        .len();
    let result = world
        .download(&core, 1, file, net_ids.expose().clone())
        .await;
    assert_eq!(result["Ok"]["size"], 40_000, "{result}");
    assert_eq!(world.read(1), first, "the phone's bytes");
    let long_text = long_text.expect("the long text slot");
    let result = world.download(&core, 2, &long_text, Vec::new()).await;
    assert_eq!(world.read(2), long, "{result}");
    let downloads = world.harness.downloads().await.expect("downloadStats");
    assert_eq!(
        downloads.len() - downloads_before,
        2,
        "one CDN request per download, none by the answer itself"
    );

    // Nothing of a request or an answer reaches the log.
    let log = String::from_utf8_lossy(&log_buffer().lock().expect("log")).into_owned();
    let mut secrets = vec![
        world.alice.aci.clone(),
        aci_b64(&world.alice.aci),
        "7a".repeat(8),
    ];
    for pointer in world
        .harness
        .downloads()
        .await
        .expect("downloadStats")
        .iter()
        .skip(downloads_before)
    {
        secrets.push(pointer.key.clone());
    }
    for secret in &secrets {
        let line = log.lines().find(|l| l.contains(secret.as_str()));
        assert!(line.is_none(), "the log names a secret: {line:?}");
    }
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn not_found_and_an_answer_naming_another_chat_reach_the_core_as_they_are() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world.alice_says(&device, "gone from the phone").await;
    world.ask(&core, 1, at).await;
    assert_eq!(world.restore(1).await.1, RestoreUpdate::Sent);

    let mut answer = world.answer(at);
    answer.error = Some(json!("messageNotFound"));
    world.send(&device, &answer, true).await;
    let (target, update) = world.restore(2).await;
    assert_eq!(update, RestoreUpdate::MessageNotFound);
    assert_eq!(target, world.target(at));

    // The right author and time under our phone's own chat: the core is told that chat, so it
    // finds no message there (`resolve_target` binds a stated conversation).
    let mut elsewhere = world.answer(at);
    elsewhere.conversation = json!({ "aci": world.phone.aci });
    elsewhere.attachments = Some(vec![BackfillEntry::upload(
        &picture(2, 1000),
        "image/png",
        None,
    )]);
    world.send(&device, &elsewhere, true).await;
    let (target, update) = world.restore(3).await;
    assert!(matches!(update, RestoreUpdate::Answer { .. }), "{update:?}");
    assert_ne!(target.conv, Some(world.alice_chat()), "not Alice's chat");
    assert_eq!(
        target.conv,
        Some(ConvKey::Direct(
            UserId::parse(&format!("aci:{}", world.phone.aci)).expect("user")
        ))
    );

    // An answer with neither a list nor an error changes nothing.
    let mut empty = world.answer(at);
    empty.empty = true;
    world.send(&device, &empty, true).await;
    tokio::time::sleep(QUIET).await;
    assert_eq!(world.restores().len(), 3, "an empty answer is dropped");
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pointer_without_a_digest_downloads_only_against_the_plaintext_name() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world
        .alice_says(&device, "a picture named by its hash")
        .await;
    world.ask(&core, 1, at).await;
    assert_eq!(world.restore(1).await.1, RestoreUpdate::Sent);

    let data = picture(3, 20_000);
    let mut answer = world.answer(at);
    answer.attachments = Some(vec![
        BackfillEntry::upload(&data, "image/png", None).forged(json!({ "digest": "none" })),
    ]);
    world.send(&device, &answer, true).await;
    let (_, update) = world.restore(2).await;
    let RestoreUpdate::Answer { attachments, .. } = update else {
        panic!("an answer: {update:?}");
    };
    let [slot] = &attachments[..] else {
        panic!("one slot");
    };
    assert!(matches!(slot, RestoreSlot::File { .. }), "{slot:?}");

    // Without a plaintext name it is refused before any request; with another file's name the
    // download fails for good and leaves nothing; with its own name it is the phone's bytes.
    let downloads_before = world
        .harness
        .downloads()
        .await
        .expect("downloadStats")
        .len();
    let result = world.download(&core, 1, slot, Vec::new()).await;
    assert!(result.get("Err").is_some(), "{result}");
    assert_eq!(
        world
            .harness
            .downloads()
            .await
            .expect("downloadStats")
            .len(),
        downloads_before,
        "refused before any request"
    );
    let result = world
        .download(&core, 2, slot, vec![plaintext_name(&picture(4, 20_000))])
        .await;
    assert!(result.get("Err").is_some(), "{result}");
    assert!(!world.dest("2").path.exists(), "nothing left of it");
    let result = world
        .download(&core, 3, slot, vec![plaintext_name(&data)])
        .await;
    assert_eq!(result["Ok"]["size"], 20_000, "{result}");
    assert_eq!(world.read(3), data);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

/// A pointer WITH a digest is also checked against the plaintext hash the attachment has: a
/// blob that arrived whole but is another file (an answer matched to the wrong attachment) is
/// refused and leaves nothing; the right file, and an attachment with no hash to check, download.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pointer_with_a_digest_is_also_checked_against_the_plaintext_name() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world.alice_says(&device, "a picture of another file").await;
    world.ask(&core, 1, at).await;
    assert_eq!(world.restore(1).await.1, RestoreUpdate::Sent);

    let data = picture(6, 20_000);
    let mut answer = world.answer(at);
    answer.attachments = Some(vec![BackfillEntry::upload(&data, "image/png", None)]);
    world.send(&device, &answer, true).await;
    let (_, update) = world.restore(2).await;
    let RestoreUpdate::Answer { attachments, .. } = update else {
        panic!("an answer: {update:?}");
    };
    let [slot] = &attachments[..] else {
        panic!("one slot");
    };
    let RestoreSlot::File { net_ids, .. } = slot else {
        panic!("a file: {slot:?}");
    };
    assert!(
        net_ids.expose().iter().any(|n| n.starts_with("digest:")),
        "it states its digest"
    );
    let result = world
        .download(&core, 1, slot, vec![plaintext_name(&picture(7, 20_000))])
        .await;
    assert!(result.get("Err").is_some(), "another file: {result}");
    assert!(!world.dest("1").path.exists(), "nothing left of it");
    let result = world
        .download(&core, 2, slot, vec![plaintext_name(&data)])
        .await;
    assert_eq!(result["Ok"]["size"], 20_000, "{result}");
    assert_eq!(world.read(2), data);
    let result = world.download(&core, 3, slot, Vec::new()).await;
    assert_eq!(result["Ok"]["size"], 20_000, "no hash to check: {result}");
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_that_arrives_while_kourier_is_stopped_lands_after_the_restart() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world.alice_says(&device, "asked before the restart").await;
    world.ask(&core, 1, at).await;
    world
        .harness
        .wait_for_backfill_request(&world.phone.device_ref, None, WAIT)
        .await
        .expect("the request");
    assert_eq!(world.restore(1).await.1, RestoreUpdate::Sent);
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    let mut answer = world.answer(at);
    answer.attachments = Some(vec![BackfillEntry::upload(
        &picture(5, 5000),
        "image/png",
        Some(UUID_A),
    )]);
    world.send(&device, &answer, false).await;

    let mut core = world.actor(StartMode::Resume);
    core.send(Command::Connect).await;
    core.wait_online().await;
    let (target, update) = world.restore(2).await;
    assert_eq!(target, world.target(at));
    assert!(
        matches!(&update, RestoreUpdate::Answer { attachments, .. }
            if matches!(attachments[..], [RestoreSlot::File { .. }])),
        "{update:?}"
    );
    world.no_more_requests().await;
    assert_eq!(world.jobs(), 0);
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

/// The backend states `AccountCaps` with the restore flag at EVERY start of a linked account,
/// not only after a remote config was fetched: a restart finds the stored config unchanged
/// (the fetch then sends no caps of its own), so a `core.db` made before the flag, or one whose
/// config never arrived, would never learn that the account can ask its phone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_start_of_a_linked_account_states_the_restore_capability() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link().await;
    world.sink.wait_caps_flags(1, WAIT).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let before = world.sink.caps_flags().len();

    let core = world.actor(StartMode::Resume);
    let flags = world.sink.wait_caps_flags(before + 1, WAIT).await;
    let last: Value =
        serde_json::from_str(flags.last().expect("the start's caps")).expect("the flags as JSON");
    assert_eq!(
        last["attachment_restore"],
        json!(true),
        "the start states the flag: {flags:?}"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

/// A request that outlived its hour before a socket came up (a computer offline for hours) is
/// dropped when one does, before it can wake the phone for nothing: the core hears `NotSent`,
/// the phone hears nothing, and the job is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_that_outlived_its_hour_before_a_socket_came_up_is_not_sent() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link().await;
    let at = world.alice_says(&device, "asked while offline").await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // The job as `persist` stored it when the click came, long ago, while no socket was up.
    let payload = json!({ "conv": world.alice_chat(), "target": world.target(at), "cmd": 77 });
    world
        .files
        .execute(&format!(
            "INSERT INTO jobs(kind, key, priority, next_at, expires_at, created_at, payload)
             VALUES ('backfill_request', 'aged', 2, 1, 2, 1, CAST('{payload}' AS BLOB))"
        ))
        .expect("an aged job");
    assert_eq!(world.jobs(), 1);
    let sends_before = world.harness.send_stats().await.expect("sendStats");

    let mut core = world.actor(StartMode::Resume);
    core.send(Command::Connect).await;
    core.wait_online().await;
    let (target, update) = world.restore(1).await;
    assert_eq!((target, update), (world.target(at), RestoreUpdate::NotSent));
    assert_eq!(
        world.restore_cmd(1),
        Some(77),
        "the report names the command that made the job"
    );
    world.no_more_requests().await;
    assert_eq!(world.jobs(), 0, "the job is gone");
    let own = |s: &support::harness::SendStats| s.urgency.get(&device.aci).map_or(0, Vec::len);
    let sends_after = world.harness.send_stats().await.expect("sendStats");
    assert_eq!(
        own(&sends_after),
        own(&sends_before),
        "nothing was sent to our own account for it"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}
