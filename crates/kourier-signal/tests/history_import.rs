//! The import of a history transfer's archive against `@signalapp/mock-server`
//! (docs/architecture.md §14.2 item 14; `transfer.rs`, "The import"): the harness plays the
//! phone, the test plays the core.
//!
//! The archive is read into a private, encrypted stage file; one write in `backend.db`
//! records the stage (the commit); then the stage is DELIVERED to the core in small rows
//! with a durable cursor (`transfer/deliver.rs`). What is proven here:
//!
//! - up to the commit the core got no event of the import and `core_outbox`, `peers`,
//!   `groups` and `jobs` are what they were; an archive that breaks half-way, a quit and a
//!   kill during the read leave nothing, and the next start reads from the first frame;
//! - a quit leaves a recorded stage or none; a recorded stage is not read again;
//! - delivery: people, then the chats most recent first with their features, then each
//!   chat's rows and the phone's unread count; the chat of a person known only by a number
//!   cannot be sent to; a kill at every zone and a quit go on at the cursor and append
//!   every row exactly once; `backend.db` never holds more than two delivery rows; `done`
//!   and the login's `Done` come after the last acknowledgement;
//! - a stage that is lost after the commit is read again and nothing doubles; when the
//!   archive cannot be read either the transfer ends `Incomplete`, what was delivered
//!   stays and receiving starts; a row the core refuses comes back as single messages and
//!   the one bad message is dropped; a core that acknowledges nothing ends the transfer
//!   `Incomplete`;
//! - a row that is due is dropped only under a clock the server confirmed;
//! - nothing of the import is logged but counts.
//!
//! The kills are real: a helper process runs the actor (`import_child_process`) and dies by
//! SIGKILL or at a failpoint (`KOURIER_FAILPOINT`, an abort).
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing
//! (set `KOURIER_REQUIRE_MOCK_HARNESS=1` to fail instead). Tests print counts only.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kourier_backend_api::{BackendEnv, BackendEvent, Command, Exit, InitialSync, StartMode};
use kourier_model::{HistoryTransfer, HistoryTransferFailure, LoginStepKind, OutgoingState};
use kourier_signal::SignalNetwork;
use kourier_signal::archive_testing::{
    ChatSpec, ChatWith, ContactSpec, GeneratedSpec, HistorySpec, STAGE_FILE, SendStatusSpec,
    StageDump, TextSpec, generated_aci, read_stage, write_history_archive,
};
use serde::{Deserialize, Serialize};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env};
use support::harness::{
    ArchivePlace, CreatePrimary, DeviceInfo, Harness, Primary, SendText, StorageAccount,
    StorageContact, StorageSpec,
};
use uuid::Uuid;

/// A text that is in every message of the test archives and nowhere else.
const MARKER: &str = "import-marker-5d1c";

/// The people of the test archives (made-up ids: nobody on the mock server has them).
const ANN: Uuid = Uuid::from_bytes([0xa1; 16]);
const BEN_PNI: Uuid = Uuid::from_bytes([0xb2; 16]);
const CAT: Uuid = Uuid::from_bytes([0xc3; 16]);
const DAN: Uuid = Uuid::from_bytes([0xd4; 16]);
const GENERATED_CHATS: u32 = 5;
const T0: u64 = 1_700_000_000_000;

fn incoming(body: &str, sent_at: u64, read: bool) -> TextSpec {
    TextSpec {
        body: format!("{MARKER} {body}"),
        sent_at,
        received_at: sent_at + 2,
        read,
        ..TextSpec::default()
    }
}

fn outgoing(body: &str, sent_at: u64, statuses: &[SendStatusSpec]) -> TextSpec {
    TextSpec {
        outgoing: true,
        body: format!("{MARKER} {body}"),
        sent_at,
        received_at: sent_at,
        statuses: statuses.to_vec(),
        ..TextSpec::default()
    }
}

/// The history of the tests: two people with chats (one known by her ACI, one by his PNI
/// and number), Note to Self, a person without a chat, a chat without an item, the release
/// notes with a chat, items that name unknown ids, the chats' items interleaved, and
/// `generated` more messages over five more chats.
fn history(generated: u32) -> HistorySpec {
    HistorySpec {
        contacts: vec![
            ContactSpec {
                aci: Some(ANN),
                profile_name: format!("{MARKER} Ann"),
                profile_sharing: true,
                ..ContactSpec::default()
            },
            ContactSpec {
                pni: Some(BEN_PNI),
                e164: Some(15_550_100_022),
                system_name: format!("{MARKER} Ben"),
                ..ContactSpec::default()
            },
            ContactSpec {
                aci: Some(CAT),
                profile_name: format!("{MARKER} Cat"),
                ..ContactSpec::default()
            },
            ContactSpec {
                aci: Some(DAN),
                profile_name: format!("{MARKER} Dan"),
                ..ContactSpec::default()
            },
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![
                    incoming("a1", T0 + 1, true),
                    outgoing("a2", T0 + 4, &[SendStatusSpec::Read]),
                    incoming("a3", T0 + 7, false),
                    TextSpec {
                        unknown_author: true,
                        ..incoming("from nobody", T0 + 8, true)
                    },
                ],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Contact(1),
                texts: vec![
                    incoming("b1", T0 + 2, false),
                    outgoing("b2", T0 + 5, &[SendStatusSpec::Delivered]),
                ],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![outgoing("note", T0 + 3, &[])],
                ..ChatSpec::default()
            },
            // Dan has a chat and never wrote; Cat has no chat at all.
            ChatSpec {
                with: ChatWith::Contact(3),
                texts: Vec::new(),
                ..ChatSpec::default()
            },
        ],
        interleaved: true,
        release_notes_items: Some(1),
        orphan_items: 2,
        generated: (generated > 0).then(|| GeneratedSpec {
            chats: GENERATED_CHATS,
            messages: generated,
            text: MARKER.to_owned(),
            first_sent_at: T0 + 1_000_000,
        }),
        ..HistorySpec::default()
    }
}

/// What the stage of [`history`] must hold.
struct Expected {
    messages: u64,
    chats: usize,
    contacts: usize,
}

fn expected(generated: u32) -> Expected {
    let generated_chats = GENERATED_CHATS.min(generated) as usize;
    Expected {
        // a1, a2, a3, b1, b2, the note; not "from nobody", the release note, the orphans.
        messages: 6 + u64::from(generated),
        // Ann, Ben, Note to Self; not Dan's empty chat, not the release notes'.
        chats: 3 + generated_chats,
        // Ann and Ben; not Cat, not Dan.
        contacts: 2 + generated_chats,
    }
}

fn user(aci: Uuid) -> String {
    format!("aci:{}", aci.hyphenated())
}

/// Every user id the test archives name.
fn archive_users() -> Vec<String> {
    let mut users = vec![
        user(ANN),
        format!("pni:{}", BEN_PNI.hyphenated()),
        user(CAT),
        user(DAN),
    ];
    users.extend((0..GENERATED_CHATS).map(|i| user(generated_aci(i))));
    // The person one test's archive knows only by a number.
    users.push(format!("e164:+{EVE_NUMBER}"));
    users
}

/// The stage is the archive's chats, each in the order of its frames.
fn expect_the_stage(dump: &StageDump, frames: u64, generated: u32, own_aci: &str) {
    let want = expected(generated);
    assert_eq!(dump.frames, frames, "every frame of the archive was read");
    assert_eq!(dump.messages, want.messages);
    assert_eq!(dump.chats.len(), want.chats);
    assert_eq!(dump.shown_chats as usize, want.chats);
    assert_eq!(dump.contacts.len(), want.contacts);
    let ben = format!("pni:{}", BEN_PNI.hyphenated());
    assert_eq!(
        &dump.contacts[..2],
        [user(ANN), ben.clone()],
        "people are staged at their first row"
    );
    for absent in [user(CAT), user(DAN)] {
        assert!(!dump.contacts.contains(&absent), "a person nobody used");
    }
    let chat = |conv: String| {
        dump.chats
            .iter()
            .find(|chat| chat.conv == conv)
            .unwrap_or_else(|| panic!("a staged chat is missing"))
    };
    let texts = |conv: String| -> Vec<(String, String)> {
        chat(conv)
            .messages
            .iter()
            .map(|m| {
                let body = match &m.body {
                    kourier_model::Body::Text(text) => text.body.clone(),
                    _ => panic!("a text"),
                };
                (m.local.to_string(), body)
            })
            .collect()
    };
    let line = |n: u64, body: &str| ((T0 + n).to_string(), format!("{MARKER} {body}"));
    // The archive interleaves a1 b1 note a2 b2 a3: each chat comes back in its own order.
    let ann = chat(format!("direct:{}", user(ANN)));
    assert_eq!(
        texts(format!("direct:{}", user(ANN))),
        [line(1, "a1"), line(4, "a2"), line(7, "a3")]
    );
    assert_eq!((ann.rows, ann.loud, ann.unread), (3, 3, 1));
    assert!(ann.messages[0].read && !ann.messages[2].read);
    assert!(ann.messages.iter().all(|m| m.silent));
    assert_eq!(ann.messages[1].initial_out_state, Some(OutgoingState::Read));
    assert_eq!(
        ann.messages[0]
            .origin
            .author()
            .map(|a| a.as_str().to_owned()),
        Some(user(ANN))
    );
    assert!(ann.messages[1].origin.is_own());
    assert_eq!(
        texts(format!("direct:{ben}")),
        [line(2, "b1"), line(5, "b2")]
    );
    assert_eq!(chat(format!("direct:{ben}")).unread, 1);
    let note = chat(format!("direct:aci:{own_aci}"));
    assert!(note.is_self);
    assert_eq!(texts(format!("direct:aci:{own_aci}")), [line(3, "note")]);
    for chat in &dump.chats {
        assert_eq!(chat.messages.len() as u64, chat.rows);
        let mut locals: Vec<u64> = chat
            .messages
            .iter()
            .map(|m| m.local.parse().expect("a sent time"))
            .collect();
        let in_order = locals.clone();
        locals.sort_unstable();
        assert_eq!(
            locals, in_order,
            "a chat's rows in the order of their frames"
        );
    }
    assert_eq!(
        dump.chats.iter().map(|chat| chat.rows).sum::<u64>(),
        dump.messages
    );
}

/// What an import must not have touched before its delivery: the rows of `backend.db` it
/// will write to, and what the core holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Untouched {
    /// How many rows `core_outbox` was ever given.
    outbox_appended: i64,
    outbox_rows: i64,
    peers: i64,
    groups: i64,
    jobs: Vec<(String, i64)>,
    /// The names of the events the core committed, and its messages.
    core_events: Vec<String>,
    core_messages: i64,
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

/// A link that ran up to the linked device: the transfer is waiting.
struct Linked {
    core: FakeCore,
    device: DeviceInfo,
    key: [u8; 32],
}

fn network() -> SignalNetwork {
    SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
        .with_download_resume_pause(Duration::from_millis(50))
}

/// Delivery rows of `rows` messages (0: the app's 500).
fn rows_of(network: SignalNetwork, rows: usize) -> SignalNetwork {
    if rows == 0 {
        network
    } else {
        network.with_history_delivery_rows(rows, 1024 * 1024)
    }
}

/// What the actors of this process logged (every level): searched for content of the
/// test archives.
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

fn slow(frame_delay_ms: u64) -> SignalNetwork {
    network().with_history_transfer_frame_delay(Duration::from_millis(frame_delay_ms))
}

/// Reads events until a transfer state satisfies `pred`; fails on a failed login.
async fn until(
    core: &mut FakeCore,
    what: &str,
    pred: impl Fn(&HistoryTransfer) -> bool,
) -> HistoryTransfer {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::HistoryTransfer(state)) if pred(&state) => return state,
            Some(BackendEvent::Login(step)) => {
                if let LoginStepKind::Failed { error } = step.kind {
                    panic!("login failed: {error:?}");
                }
            }
            Some(_) => {}
            None => panic!("no history transfer state {what}"),
        }
    }
}

impl World {
    async fn start() -> Option<Self> {
        {
            use tracing_subscriber::layer::SubscriberExt as _;
            use tracing_subscriber::util::SubscriberInitExt as _;
            use tracing_subscriber::{Layer as _, filter};
            // Everything Kourier logs, at every level, into a buffer the tests search.
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

    /// A fresh link whose phone chose to transfer, up to the waiting transfer and past the
    /// link's own work (its sync requests reached the phone).
    async fn link(&self, network: SignalNetwork) -> Linked {
        self.link_with(network, InitialSync::LinkOnly).await
    }

    /// [`Self::link`] with the link's initial sync (the app's is `ContactsAndStorage`).
    async fn link_with(&self, network: SignalNetwork, initial_sync: InitialSync) -> Linked {
        let key = self
            .harness
            .set_ephemeral_backup_key(&self.phone.device_ref)
            .await
            .expect("setEphemeralBackupKey");
        let mut core = FakeCore::start_full(
            network,
            &self.env,
            &self.files,
            StartMode::Fresh,
            &self.sink,
            initial_sync,
        )
        .expect("spawn the actor");
        let (url, _) = core.begin_login(None).await;
        let device = self
            .harness
            .complete_provision(&url, &self.phone.device_ref, WAIT)
            .await
            .expect("completeProvision");
        self.wait_state("waiting").await;
        self.harness
            .wait_for_sync(&self.phone.device_ref, &device.device_ref, WAIT)
            .await
            .expect("sync requests reached the phone");
        Linked { core, device, key }
    }

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

    /// Writes the archive of `spec` for the linked device; returns its path and its frames.
    fn archive(&self, linked: &Linked, spec: &HistorySpec) -> (PathBuf, u64) {
        let path = self.tmp.path().join("phone-archive");
        let aci = linked.device.aci.parse().expect("the account's ACI");
        let frames = write_history_archive(&path, &linked.key, aci, spec).expect("archive");
        (path, frames)
    }

    /// The phone uploads `path` and answers the device's long-poll with its place.
    async fn upload(&self, device: &DeviceInfo, path: &Path) -> ArchivePlace {
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

    async fn alice_says(&self, device: &DeviceInfo, text: &str) -> u64 {
        self.harness
            .send_text(&SendText::new(&self.alice.aci, &device.device_ref, text))
            .await
            .expect("sendText")
            .timestamp
    }

    /// Alice's held message arrives: receiving started.
    async fn expect_arrival(&self, timestamp: u64) {
        let author = format!("aci:{}", self.alice.aci);
        self.sink
            .wait_message(&author, &timestamp.to_string(), WAIT)
            .await
            .expect("the held message reached the core");
    }

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

    fn transfer_dir(&self) -> PathBuf {
        self.files.dir.join("transfer")
    }

    fn stage_file(&self) -> PathBuf {
        self.transfer_dir().join(STAGE_FILE)
    }

    /// The files of the stage that are in the transfer directory now.
    fn stage_files(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.transfer_dir()) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .filter(|name| name.starts_with(STAGE_FILE))
            .collect();
        names.sort();
        names
    }

    /// Waits until the transfer reads and its stage has rows on disk.
    async fn wait_reading_into_the_stage(&self) {
        self.wait_state("reading").await;
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let wal = self.transfer_dir().join(format!("{STAGE_FILE}-wal"));
            let written = std::fs::metadata(&wal).map_or(0, |meta| meta.len())
                + std::fs::metadata(self.stage_file()).map_or(0, |meta| meta.len());
            if written > 8192 {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the read never wrote its stage"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn untouched(&self) -> Untouched {
        let int = |sql: &str| self.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
        let conn = kourier_db::open_read_only(
            &self.files.backend_db(),
            &kourier_db::DbKey::from(self.files.key),
        )
        .expect("backend.db");
        let jobs = conn
            .prepare("SELECT kind, count(*) FROM jobs GROUP BY kind ORDER BY kind")
            .and_then(|mut stmt| {
                stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<Vec<(String, i64)>, _>>()
            })
            .expect("jobs");
        Untouched {
            outbox_appended: int("SELECT seq FROM sqlite_sequence WHERE name = 'core_outbox'"),
            outbox_rows: int("SELECT count(*) FROM core_outbox"),
            peers: int("SELECT count(*) FROM peers"),
            groups: int("SELECT count(*) FROM groups"),
            jobs,
            core_events: self.sink.event_names(),
            core_messages: self.sink.messages_from("") + self.archive_messages_in_the_core(),
        }
    }

    /// Waits until nothing of [`Untouched`] has changed for a second (the work of a link or
    /// of a start has settled) and returns it.
    async fn settled(&self) -> Untouched {
        let deadline = tokio::time::Instant::now() + WAIT;
        let mut last = self.untouched();
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let now = self.untouched();
            if now == last {
                return now;
            }
            last = now;
            assert!(
                tokio::time::Instant::now() < deadline,
                "the account never settled"
            );
        }
    }

    fn archive_messages_in_the_core(&self) -> i64 {
        archive_users()
            .iter()
            .map(|user| self.sink.messages_from(user))
            .sum()
    }

    /// Nothing of the test archives is anywhere but the transfer directory: no event of an
    /// import in the core, no row of `backend.db` for a person of the archive, no text of it
    /// in a row waiting for the core. Holds at any time in this step, also after the end
    /// (when receiving has started and the account's own people arrive).
    fn expect_no_trace_of_the_archive(&self, when: &str) {
        assert!(
            self.sink.events("HistoryBatch").is_empty(),
            "{when}: the core got a history batch"
        );
        let users = archive_users();
        for name in self.sink.event_names() {
            for detail in self.sink.events(&name).into_iter().flatten() {
                assert!(
                    !detail.contains(MARKER) && !users.iter().any(|user| detail.contains(user)),
                    "{when}: an event {name} of the import reached the core"
                );
            }
        }
        assert_eq!(
            self.archive_messages_in_the_core(),
            0,
            "{when}: a message of the archive reached the core"
        );
        for user in &users {
            assert!(
                self.sink.messages_in(&format!("direct:{user}")).is_empty(),
                "{when}: a chat of the archive reached the core"
            );
        }
        let int = |sql: &str| self.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
        let ids: Vec<String> = [ANN, BEN_PNI, CAT, DAN]
            .into_iter()
            .chain((0..GENERATED_CHATS).map(generated_aci))
            .map(|id| id.hyphenated().to_string())
            .collect();
        for id in &ids {
            assert_eq!(
                int(&format!(
                    "SELECT count(*) FROM peers WHERE service_id LIKE '%{id}%'"
                )),
                0,
                "{when}: a person of the archive is in peers"
            );
            assert_eq!(
                int(&format!(
                    "SELECT count(*) FROM jobs WHERE key LIKE '%{id}%'"
                )),
                0,
                "{when}: a job for a person of the archive"
            );
            assert_eq!(
                int(&format!(
                    "SELECT count(*) FROM core_outbox
                      WHERE instr(envelope, CAST('{id}' AS BLOB)) > 0"
                )),
                0,
                "{when}: a row for the core names a person of the archive"
            );
        }
        assert_eq!(int("SELECT count(*) FROM groups"), 0, "{when}: a group row");
        assert_eq!(
            int(&format!(
                "SELECT count(*) FROM core_outbox
                  WHERE instr(envelope, CAST('{MARKER}' AS BLOB)) > 0"
            )),
            0,
            "{when}: a row for the core holds a text of the archive"
        );
        assert_eq!(
            int("SELECT count(*) FROM core_outbox_poison"),
            0,
            "{when}: a refused row"
        );
    }

    /// Messages of the test archives the core holds (each once: the sink is idempotent on
    /// the message id, as the core's ingest is). Our own are stored without an author.
    fn delivered(&self) -> i64 {
        self.sink.messages_from("") + self.archive_messages_in_the_core()
    }

    /// The transfer is over and the core holds what the stage held: every message once, no
    /// row was refused, nothing is waited for.
    fn expect_delivered(&self) {
        let staged = self.row("stage_messages").expect("the staged count");
        assert!(staged > 0, "a stage was committed");
        assert_eq!(
            self.delivered(),
            staged,
            "every staged message is in the core"
        );
        assert_eq!(
            self.row("delivered_messages"),
            Some(staged),
            "the cursor counted every message"
        );
        assert_eq!(self.row("deliver_dropped"), Some(0), "nothing was dropped");
        let int = |sql: &str| self.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
        assert_eq!(int("SELECT count(*) FROM history_delivery"), 0);
        assert_eq!(int("SELECT count(*) FROM core_outbox_poison"), 0);
        assert_eq!(
            int(&format!(
                "SELECT count(*) FROM core_outbox
                  WHERE instr(envelope, CAST('{MARKER}' AS BLOB)) > 0"
            )),
            0,
            "no row of the import is left for the core"
        );
    }

    /// The transfer is over as `state` with `reason`: no key, no record of a stage, no file.
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
            self.row("key IS NULL AND cdn_key IS NULL AND file IS NULL AND import_staged = 0"),
            Some(1),
            "the key is wiped and no stage is recorded"
        );
        assert!(
            !self.transfer_dir().exists(),
            "the archive, the stage and their directory are removed"
        );
    }
}

/// Set (to a JSON [`ChildSpec`]) in the environment of the helper process that runs the
/// actor for the kill tests; [`import_child_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_IMPORT_TEST_CHILD";

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
    /// Messages per delivery row (0: the app's).
    rows: usize,
}

/// The helper process of the kill tests: runs the account actor on the files of `CHILD_ENV`
/// with `Resume`, connects, and goes on until it is killed or a failpoint ends it.
#[test]
fn import_child_process() {
    let Ok(spec) = std::env::var(CHILD_ENV) else {
        return;
    };
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
        let core = FakeCore::start_with(
            rows_of(slow(spec.frame_delay_ms), spec.rows),
            &spec.env,
            &files,
            StartMode::Resume,
            &sink,
        )
        .expect("spawn the actor");
        core.send(Command::Connect).await;
        // Only reached when nothing ended this process.
        tokio::time::sleep(Duration::from_secs(120)).await;
        let _ = core.shutdown().await;
    });
}

/// Starts the helper process on the world's files; with `failpoint` it aborts there.
fn spawn_child(
    world: &World,
    frame_delay_ms: u64,
    failpoint: Option<&str>,
) -> tokio::process::Child {
    spawn_child_with(world, frame_delay_ms, failpoint, 0)
}

/// [`spawn_child`] with delivery rows of `rows` messages.
fn spawn_child_with(
    world: &World,
    frame_delay_ms: u64,
    failpoint: Option<&str>,
    rows: usize,
) -> tokio::process::Child {
    let spec = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
        frame_delay_ms,
        rows,
    };
    let mut command = tokio::process::Command::new(std::env::current_exe().expect("test binary"));
    command
        .args([
            "import_child_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, serde_json::to_string(&spec).expect("spec"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(failpoint) = failpoint {
        command.env(kourier_db::failpoint::ENV, failpoint);
    }
    command.spawn().expect("spawn the helper process")
}

/// Waits for a helper process that a failpoint ends.
async fn wait_aborted(child: &mut tokio::process::Child) {
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("the helper process reached its failpoint")
        .expect("wait");
    assert!(!status.success(), "the failpoint aborted the process");
}

/// A link whose actor was shut down with the transfer waiting, the archive of `spec`
/// uploaded and the long-poll answered: the next actor downloads and reads.
async fn linked_with_an_archive(world: &World, spec: &HistorySpec) -> (Linked, u64, u64) {
    let linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, frames) = world.archive(&linked, spec);
    world.upload(&linked.device, &path).await;
    (linked, frames, held)
}

/// The last start: the stage is delivered, the transfer goes on to `done`, receiving
/// starts, and the core holds every message of the stage once.
async fn finish(world: &World, mut core: FakeCore, held: u64) {
    core.send(Command::Connect).await;
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    world.expect_delivered();
    world.expect_arrival(held).await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
}

/// I1 of the design, for this step: while the archive is read and at the commit, the core
/// got no event of the import and `core_outbox`, `peers`, `groups` and `jobs` are
/// unchanged, while the stage holds the archive's rows. The process is killed right after
/// the commit zone, so the files are looked at exactly as the commit left them. Then the
/// next start does not read the archive again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_core_gets_nothing_before_the_last_byte_of_the_archive_was_checked() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 2000;
    let (linked, frames, held) = linked_with_an_archive(&world, &history(generated)).await;
    let Linked { core, device, key } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // A slow read (some sixteen seconds, so that the look below falls into it also on a
    // loaded machine), in a process that dies right after the commit zone.
    let mut child = spawn_child(
        &world,
        8,
        Some(kourier_signal::failpoints::AFTER_HISTORY_IMPORT_COMMIT),
    );
    world.wait_reading_into_the_stage().await;
    // The start's own work has settled while the read goes on: from here to the commit
    // nothing but the import runs. The picture is taken after the read began (the start's
    // work must settle first), so a write the import made at the very START of a read would
    // be in both pictures and pass the comparison below. What rules that out is not this
    // comparison: the reading thread has no handle of `backend.db` (`ImportContext`), the
    // account's thread writes two things between the start of a read and its commit (the
    // count of the start and the commit zone), and `expect_no_trace_of_the_archive` looks
    // for the archive's people and texts in every row, whenever it was written.
    let before = world.settled().await;
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 0"),
        Some(1)
    );
    assert!(world.stage_file().exists(), "the read is still going on");
    world.expect_no_trace_of_the_archive("while the archive is read");
    // The stage that is being written is not a stage anybody can use.
    assert!(read_stage(&world.transfer_dir(), &key).is_err());

    wait_aborted(&mut child).await;
    // The commit: one write. The stage is recorded with its totals and the frame count.
    let want = expected(generated);
    assert_eq!(
        world.row(&format!(
            "state = 'reading' AND import_staged = 1 AND read_open = 0 AND read_starts = 1
             AND stage_messages = {} AND stage_chats = {} AND stage_frames = {frames}
             AND length(key) = 32",
            want.messages, want.chats
        )),
        Some(1),
        "the commit zone recorded the stage"
    );
    // The stage holds the archive, chat by chat.
    let dump = read_stage(&world.transfer_dir(), &key).expect("the committed stage");
    expect_the_stage(&dump, frames, generated, &device.aci);
    assert!(
        read_stage(&world.transfer_dir(), &[0x55; 32]).is_err(),
        "only the key of this transfer opens it"
    );
    println!(
        "at the commit: {} frames, {} messages in {} chats, {} people staged",
        dump.frames,
        dump.messages,
        dump.chats.len(),
        dump.contacts.len()
    );
    // And nothing else changed, strictly: not one row was appended for the core.
    let after = world.untouched();
    assert_eq!(
        after, before,
        "the import touched something before its delivery"
    );
    world.expect_no_trace_of_the_archive("at the commit");
    let held_author = format!("aci:{}", world.alice.aci);
    assert!(
        world
            .sink
            .message(&held_author, &held.to_string())
            .is_none(),
        "nothing was received before the end"
    );

    // The next start finds the stage recorded: no second read of the archive, no download.
    let downloads = world
        .harness
        .downloads()
        .await
        .expect("downloadStats")
        .len();
    finish(&world, world.restart(network()), held).await;
    assert_eq!(
        world.row("read_starts"),
        Some(1),
        "the archive was read once"
    );
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames).expect("frames"))
    );
    assert_eq!(
        world
            .harness
            .downloads()
            .await
            .expect("downloadStats")
            .len(),
        downloads
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// An authentic archive whose read fails late: the frames before the break were staged,
/// and nothing of them is left anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_that_breaks_half_way_leaves_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 300;
    let mut spec = history(generated);
    let mut linked = world.link(slow(3)).await;
    let held = world.alice_says(&linked.device, "waits for the end").await;
    // The frame that does not decode stands in front of the last five messages.
    let (_, whole) = world.archive(&linked, &spec);
    spec.broken_after = Some(u32::try_from(whole - 5).expect("frames"));
    let (path, _) = world.archive(&linked, &spec);
    world.upload(&linked.device, &path).await;

    // While it reads, the stage fills.
    world.wait_reading_into_the_stage().await;
    world.expect_no_trace_of_the_archive("while the archive is read");
    let last = until(&mut linked.core, "over", HistoryTransfer::is_over).await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Corrupt)
    );
    world.expect_over("failed", Some("corrupt"));
    assert_eq!(
        world.row("stage_messages"),
        Some(0),
        "nothing was committed"
    );
    assert_eq!(world.stage_files(), Vec::<String>::new());
    // The transfer never blocks the account: receiving starts, and it brings nothing of
    // the archive.
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    world.expect_no_trace_of_the_archive("after the failed import");
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Quitting the application while the archive is read: the read ends at its next frame, its
/// stage is removed, and the next start reads the archive from its first frame without
/// using one of the read's starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_during_the_read_leaves_nothing_and_the_next_start_reads_from_the_first_frame() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 600;
    let linked = world.link(slow(5)).await;
    let held = world
        .alice_says(&linked.device, "waits through a quit")
        .await;
    let (path, frames) = world.archive(&linked, &history(generated));
    world.upload(&linked.device, &path).await;
    world.wait_reading_into_the_stage().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);

    // Nothing of the stage is left; the archive is, and the transfer still reads.
    assert_eq!(world.stage_files(), Vec::<String>::new());
    assert!(world.transfer_dir().join("archive").exists());
    assert_eq!(
        world.row(
            "state = 'reading' AND import_staged = 0 AND read_open = 0 AND read_starts = 1
             AND length(key) = 32"
        ),
        Some(1),
        "an orderly stop: the read is not open, nothing is recorded"
    );
    world.expect_no_trace_of_the_archive("after the quit");

    // The next start reads all of it again.
    finish(&world, world.restart(network()), held).await;
    let want = expected(generated);
    assert_eq!(
        world.row(&format!(
            "frames = {frames} AND stage_messages = {} AND stage_chats = {}",
            want.messages, want.chats
        )),
        Some(1),
        "read from the first frame to the last"
    );
    assert_eq!(
        world.row("read_starts"),
        Some(1),
        "a quit uses no read start"
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// A link whose read of the archive of `generated` messages is past its last frame: the
/// reader has checked the archive to its last byte and the importer has committed and
/// closed a whole stage, but the read's task has not handed its result over yet (it ends
/// three seconds after the importer's commit; a quit in those seconds is waited for by the
/// stop, which waits five). Returns the link, the archive's frames and the held message.
async fn linked_past_the_last_frame(world: &World, generated: u32) -> (Linked, u64, u64) {
    let pause = Duration::from_secs(3);
    let linked = world
        .link(network().with_history_transfer_commit_pause(pause))
        .await;
    let held = world
        .alice_says(&linked.device, "waits through a quit")
        .await;
    let (path, frames) = world.archive(&linked, &history(generated));
    world.upload(&linked.device, &path).await;
    world.wait_state("reading").await;
    // Committed and closed: everything is in the database file and its write-ahead log is
    // gone. (Not looked into here: a reader of ours could hold the importer's last
    // checkpoint back.)
    let deadline = tokio::time::Instant::now() + WAIT;
    let wal = world.transfer_dir().join(format!("{STAGE_FILE}-wal"));
    while std::fs::metadata(world.stage_file()).map_or(0, |meta| meta.len()) < 32 * 1024
        || wal.exists()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the importer never committed its stage"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 0 AND read_open = 1"),
        Some(1),
        "a whole stage, and nothing has recorded it yet"
    );
    (linked, frames, held)
}

/// F4 of the review of this step. A quit that reaches the read AFTER its last frame. The
/// stop waits for the read's task and runs the commit zone itself: the stage is recorded,
/// nothing lies unrecorded while the application is closed, and the next start goes on
/// after the commit instead of reading the archive again. (Before the fix the stop dropped
/// the task's result: a whole stage stayed on disk without a record, and the next start
/// removed it and read again.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_after_the_reads_last_frame_records_its_stage() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 200;
    let (linked, frames, held) = linked_past_the_last_frame(&world, generated).await;
    let Linked { core, device, key } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    // The quit recorded it: the commit zone ran in the stop.
    let want = expected(generated);
    assert_eq!(
        world.row(&format!(
            "state = 'reading' AND import_staged = 1 AND read_open = 0 AND read_starts = 1
             AND stage_messages = {} AND stage_chats = {} AND stage_frames = {frames}
             AND length(key) = 32",
            want.messages, want.chats
        )),
        Some(1),
        "the stop recorded the stage of the read that had ended"
    );
    let dump = read_stage(&world.transfer_dir(), &key).expect("the recorded stage");
    expect_the_stage(&dump, frames, generated, &device.aci);
    world.expect_no_trace_of_the_archive("after the quit");

    // The next start goes on after the commit: no second read of the archive.
    finish(&world, world.restart(network()), held).await;
    assert_eq!(
        world.row("read_starts"),
        Some(1),
        "the finished read was not thrown away"
    );
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames).expect("frames"))
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The other half of the same rule: when the commit zone of that stop does NOT record the
/// stage, the stage does not stay behind the quit either. Here the zone cannot apply,
/// because the row says a stage is recorded already (written behind the actor's back; the
/// zone records only where none is).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_whose_commit_zone_does_not_apply_leaves_no_stage() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 200;
    let (linked, frames, held) = linked_past_the_last_frame(&world, generated).await;
    world
        .files
        .execute("UPDATE history_transfer SET import_staged = 1")
        .expect("a record the read did not make");
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);

    assert_eq!(
        world.stage_files(),
        Vec::<String>::new(),
        "the stage that was not recorded went with the quit"
    );
    assert!(world.transfer_dir().join("archive").exists());
    assert_eq!(
        world.row("state = 'reading' AND read_open = 0 AND stage_messages = 0"),
        Some(1),
        "the zone wrote nothing, and the stop was an orderly one"
    );
    world.expect_no_trace_of_the_archive("after the quit");

    // The next start finds a record without a stage, takes it back and reads the archive.
    finish(&world, world.restart(network()), held).await;
    assert_eq!(world.row("read_starts"), Some(2));
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames).expect("frames"))
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// SIGKILL while the archive is read. The dead process leaves its half-written stage file;
/// nobody recorded it, nothing can read it, and the next start removes it before it reads
/// the archive from its first frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_during_the_read_leaves_nothing_and_the_next_start_reads_from_the_first_frame() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 600;
    let (linked, frames, held) = linked_with_an_archive(&world, &history(generated)).await;
    let Linked { core, key, .. } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    let mut child = spawn_child(&world, 5, None);
    world.wait_reading_into_the_stage().await;
    child.kill().await.expect("kill the reading process");
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 0 AND read_open = 1"),
        Some(1),
        "the read ended with its process and nothing was recorded"
    );
    // What the kill left is a file in the transfer directory that is not a stage anybody
    // can use.
    assert!(world.stage_file().exists(), "the half-written stage");
    assert!(read_stage(&world.transfer_dir(), &key).is_err());
    world.expect_no_trace_of_the_archive("after the kill");
    // Whatever a kill leaves under these names (here: bytes that are no database, and a
    // stale write-ahead log) must go before the next read, which could not start on them.
    std::fs::write(world.stage_file(), vec![0x5a; 12_288]).expect("a leftover");
    std::fs::write(
        world.transfer_dir().join(format!("{STAGE_FILE}-wal")),
        vec![0xa5; 4_096],
    )
    .expect("a leftover log");

    // The next start: the leftover goes before the read begins, and the read runs from
    // the first frame to the end.
    let mut core = world.restart(network());
    core.send(Command::Connect).await;
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    let want = expected(generated);
    assert_eq!(
        world.row(&format!(
            "frames = {frames} AND stage_messages = {} AND stage_chats = {}",
            want.messages, want.chats
        )),
        Some(1),
        "read from the first frame to the last, and nothing twice"
    );
    assert_eq!(
        world.row("read_starts"),
        Some(2),
        "a kill uses a read start"
    );
    world.expect_delivered();
    world.expect_arrival(held).await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// A kill between the importer's commit (the stage is whole and closed) and the commit
/// zone: a whole stage that nobody recorded. The next start does not trust it: it removes
/// it and reads the archive again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_between_the_importers_commit_and_the_commit_zone_reads_the_archive_again() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 200;
    let (linked, frames, held) = linked_with_an_archive(&world, &history(generated)).await;
    let Linked { core, device, key } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    let mut child = spawn_child(
        &world,
        0,
        Some(kourier_signal::failpoints::BEFORE_HISTORY_IMPORT_COMMIT),
    );
    wait_aborted(&mut child).await;
    assert_eq!(
        world.row(
            "state = 'reading' AND import_staged = 0 AND stage_messages = 0 AND read_open = 1"
        ),
        Some(1),
        "the commit zone never ran"
    );
    // The stage IS whole (the importer committed it), which is exactly why the record in
    // backend.db, and not the file, decides.
    let dump = read_stage(&world.transfer_dir(), &key).expect("the whole stage");
    expect_the_stage(&dump, frames, generated, &device.aci);
    world.expect_no_trace_of_the_archive("after the kill");

    // A start removes it before anything else of the read: a process that dies where the
    // read is about to begin (before the start is counted, before the archive is opened,
    // long before an importer exists that would replace the file) has left no stage.
    let mut child = spawn_child(
        &world,
        0,
        Some(kourier_signal::failpoints::BEFORE_HISTORY_IMPORT_READ),
    );
    wait_aborted(&mut child).await;
    assert_eq!(
        world.stage_files(),
        Vec::<String>::new(),
        "the unrecorded stage went before the read began"
    );
    assert!(world.transfer_dir().join("archive").exists());
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 0 AND read_starts = 1"),
        Some(1),
        "that start was not counted and read nothing"
    );

    // Had the start trusted the file, it would not read the archive a second time.
    let mut core = world.restart(network());
    core.send(Command::Connect).await;
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert_eq!(
        world.row("read_starts"),
        Some(2),
        "the archive was read again"
    );
    assert_eq!(
        world.row("stage_messages"),
        Some(i64::try_from(expected(generated).messages).expect("messages"))
    );
    world.expect_delivered();
    world.expect_arrival(held).await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// A stage that was recorded and is then gone, damaged, or not the one the record
/// describes: the record is taken back and the archive is read again (it is still there
/// until the transfer ends), which counts as a start of the read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_stage_that_is_lost_is_read_again_from_the_archive() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 200;
    let (linked, frames, held) = linked_with_an_archive(&world, &history(generated)).await;
    let Linked { core, key, .. } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let after_commit = Some(kourier_signal::failpoints::AFTER_HISTORY_IMPORT_COMMIT);

    // Recorded, then one page of it changed on disk.
    let mut child = spawn_child(&world, 0, after_commit);
    wait_aborted(&mut child).await;
    assert_eq!(world.row("import_staged = 1 AND read_starts = 1"), Some(1));
    let mut bytes = std::fs::read(world.stage_file()).expect("the stage");
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(world.stage_file(), &bytes).expect("damage the stage");
    assert!(read_stage(&world.transfer_dir(), &key).is_err());
    // A process finds it unusable and dies where the second read is about to begin: the
    // record is taken back and the damaged file is gone already, before the archive is
    // opened again (not only once an importer replaces it).
    let mut child = spawn_child(
        &world,
        0,
        Some(kourier_signal::failpoints::BEFORE_HISTORY_IMPORT_READ),
    );
    wait_aborted(&mut child).await;
    assert_eq!(
        world.row("import_staged = 0 AND read_open = 1 AND read_starts = 1"),
        Some(1),
        "the record was taken back, and the read that follows will count"
    );
    assert_eq!(
        world.stage_files(),
        Vec::<String>::new(),
        "the unusable stage went with its record"
    );
    // The next process reads again, records the new stage and dies there.
    let mut child = spawn_child(&world, 0, after_commit);
    wait_aborted(&mut child).await;
    assert_eq!(world.row("import_staged = 1 AND read_starts = 2"), Some(1));
    assert!(read_stage(&world.transfer_dir(), &key).is_ok());

    // A whole stage that is not the one that was recorded (the record says one message
    // more than it holds): not used either.
    world
        .files
        .execute("UPDATE history_transfer SET stage_messages = stage_messages + 1")
        .expect("change the record");
    let mut child = spawn_child(&world, 0, after_commit);
    wait_aborted(&mut child).await;
    assert_eq!(
        world.row(&format!(
            "import_staged = 1 AND read_starts = 3 AND stage_messages = {}",
            expected(generated).messages
        )),
        Some(1),
        "read again, and the record is the new stage's"
    );

    // Recorded, then the file is gone altogether.
    std::fs::remove_file(world.stage_file()).expect("lose the stage");
    finish(&world, world.restart(network()), held).await;
    assert_eq!(world.row("read_starts"), Some(4));
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames).expect("frames"))
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

// ---------------------------------------------------------------------------------------
// Delivery (`transfer/deliver.rs`)
// ---------------------------------------------------------------------------------------

/// The number of a person the archive knows only by a number (made up).
const EVE_NUMBER: u64 = 15_550_100_033;

/// [`history`] plus a person known only by a number, with the most recent chat, and one
/// read message of Ann's whose timer ran out long ago.
fn history_with_a_number_only_chat() -> HistorySpec {
    let mut spec = history(0);
    spec.contacts.push(ContactSpec {
        e164: Some(EVE_NUMBER),
        system_name: format!("{MARKER} Eve"),
        ..ContactSpec::default()
    });
    spec.chats.push(ChatSpec {
        with: ChatWith::Contact(4),
        texts: vec![incoming("e1", T0 + 9, true)],
        ..ChatSpec::default()
    });
    spec.chats[0].texts.push(TextSpec {
        expires_in_ms: Some(1_000),
        expire_start: Some(T0 + 7),
        ..incoming("due", T0 + 6, true)
    });
    spec
}

fn events_of_the_core(world: &World) -> Vec<(String, serde_json::Value)> {
    let mut all = Vec::new();
    for name in ["ContactUpsert", "ConversationUpsert", "HistoryBatch"] {
        let conn = kourier_db::open_read_only(
            &world.tmp.path().join("sink.db"),
            &kourier_db::DbKey::from([7; 32]),
        )
        .expect("sink");
        let rows: Vec<(i64, String)> = conn
            .prepare("SELECT seq, detail FROM events WHERE name = ?1 AND detail IS NOT NULL")
            .and_then(|mut stmt| {
                stmt.query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect()
            })
            .expect("events");
        for (seq, detail) in rows {
            all.push((
                seq,
                name.to_owned(),
                serde_json::from_str::<serde_json::Value>(&detail).expect("json"),
            ));
        }
    }
    all.sort_by_key(|(seq, ..)| *seq);
    all.into_iter().map(|(_, name, v)| (name, v)).collect()
}

/// The conversation an event of the sink is about, when it is one of the archive's.
fn conv_of(name: &str, value: &serde_json::Value) -> Option<String> {
    match name {
        "ConversationUpsert" => value["ConversationUpsert"]["key"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| Some(value["ConversationUpsert"]["key"].to_string())),
        "HistoryBatch" => value["conv"].as_str().map(str::to_owned),
        _ => None,
    }
}

/// Plain delivery, with the server's word on the clock: the people first, then the chats
/// most recent first with their features in front, then each chat's rows, which say
/// themselves whether the phone had read them (no unread count is sent beside them); the
/// chat of a person known only by a number cannot be sent to; the row
/// that is due is not delivered; `peers` holds what the live path would have; nothing of
/// the import is logged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stage_is_delivered_people_first_then_the_chats_then_their_rows() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, frames) = world.archive(&linked, &history_with_a_number_only_chat());
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames).expect("frames"))
    );
    // Seven texts and the due one were staged; the due one was not delivered.
    assert_eq!(world.row("stage_messages"), Some(8));
    assert_eq!(world.row("deliver_expired"), Some(1));
    assert_eq!(world.delivered(), 7);
    assert!(
        world
            .sink
            .message(&user(ANN), &(T0 + 6).to_string())
            .is_none(),
        "a row that is due under a confirmed clock is not delivered"
    );

    let events = events_of_the_core(&world);
    let eve = format!("direct:e164:+{EVE_NUMBER}");
    let ann = format!("direct:{}", user(ANN));
    let ben = format!("direct:pni:{}", BEN_PNI.hyphenated());
    let own = format!("direct:aci:{}", linked.device.aci);
    let archive_convs = [eve.clone(), ann.clone(), ben.clone(), own.clone()];
    let position = |pred: &dyn Fn(&str, &serde_json::Value) -> bool| {
        events.iter().position(|(name, v)| pred(name, v))
    };
    // People before chats, a chat's upsert before its first batch.
    let last_person = events
        .iter()
        .rposition(|(name, v)| name == "ContactUpsert" && v.to_string().contains(MARKER))
        .expect("the archive's people");
    for conv in &archive_convs {
        let upsert = position(&|name, v| {
            name == "ConversationUpsert" && conv_of(name, v).is_some_and(|c| c.contains(conv))
        })
        .expect("the chat's upsert");
        let batch = position(&|name, v| name == "HistoryBatch" && v["conv"] == **conv)
            .expect("the chat's batch");
        assert!(
            last_person < upsert && upsert < batch,
            "people, chats, rows"
        );
    }
    // The chats are told the least recent first (the core lists the chat it heard of last
    // on top, so the list stands in the phone's order before it fills) ...
    let told: Vec<String> = events
        .iter()
        .filter(|(name, v)| {
            // The chat's own upsert: not the features put in front of it (the number-only
            // chat carries its features itself).
            let patch = &v["ConversationUpsert"];
            name == "ConversationUpsert"
                && patch.get("unread_hint").is_none()
                && (patch.get("features").is_none()
                    || conv_of(name, v).is_some_and(|c| c.contains(&eve)))
        })
        .filter_map(|(name, v)| conv_of(name, v))
        .filter(|conv| archive_convs.iter().any(|c| conv.contains(c)))
        .collect();
    let least_recent_first: Vec<&String> = archive_convs.iter().rev().collect();
    assert_eq!(told.len(), 4);
    for (told, want) in told.iter().zip(least_recent_first) {
        assert!(told.contains(want.as_str()), "the order of the upserts");
    }
    // ... and filled the most recent first: Eve (T0+9), Ann (T0+7), Ben (T0+5), Note to
    // Self (T0+3).
    let batches: Vec<String> = events
        .iter()
        .filter(|(name, _)| name == "HistoryBatch")
        .filter_map(|(_, v)| v["conv"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(batches, archive_convs);
    assert!(
        events
            .iter()
            .filter(|(name, _)| name == "HistoryBatch")
            .all(|(_, v)| v["has_more"] == false && v["progress"].as_f64().is_some())
    );
    let upserts = |conv: &str| -> Vec<serde_json::Value> {
        events
            .iter()
            .filter(|(name, v)| {
                name == "ConversationUpsert" && conv_of(name, v).is_some_and(|c| c.contains(conv))
            })
            .map(|(_, v)| v["ConversationUpsert"].clone())
            .collect()
    };
    // D6: the number-only chat says that nothing can be sent there.
    let eve_features = upserts(&eve)
        .into_iter()
        .find_map(|patch| patch.get("features").cloned())
        .expect("features of the number-only chat");
    assert_eq!(eve_features["cannot_send"], "NotAccepted");
    // Ann's chat is accepted on the phone: it can be sent to.
    let ann_upserts = upserts(&ann);
    let ann_features = ann_upserts
        .iter()
        .find_map(|patch| patch.get("features").cloned())
        .expect("Ann's features");
    assert!(ann_features["cannot_send"].is_null());
    // No unread count is sent for any chat: the rows say what the phone had not read
    // (`read`), and the core counts them (the contract: the hint is only for unread
    // messages a backend has no rows for).
    assert!(
        events
            .iter()
            .filter(|(name, _)| name == "ConversationUpsert")
            .all(|(_, v)| v["ConversationUpsert"]["unread_hint"].is_null()),
        "no unread hint beside the rows"
    );
    let read = |author: &str, at: u64| -> bool {
        world
            .sink
            .message_values(author, &at.to_string())
            .first()
            .expect("the delivered message")["read"]
            .as_bool()
            .expect("read")
    };
    assert!(read(&user(ANN), T0 + 1), "read on the phone");
    assert!(!read(&user(ANN), T0 + 7), "unread on the phone");
    assert!(!read(&format!("pni:{}", BEN_PNI.hyphenated()), T0 + 2));
    // The backend's own rows for the people, as the live path would have them.
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    assert_eq!(
        int(&format!(
            "SELECT profile_sharing FROM peers WHERE service_id LIKE '%{}%'",
            ANN.hyphenated()
        )),
        1
    );
    assert_eq!(
        int(&format!(
            "SELECT system_contact FROM peers WHERE service_id LIKE '%{}%'",
            BEN_PNI.hyphenated()
        )),
        1
    );
    // Receiving starts after the end.
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    // Nothing of the import was logged: no text, no name, no number.
    let log = log_buffer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let log = String::from_utf8_lossy(&log);
    assert!(
        log.contains("the history import is delivered"),
        "the log was captured"
    );
    for secret in [
        MARKER.to_owned(),
        EVE_NUMBER.to_string(),
        "15550100022".to_owned(),
    ] {
        assert!(!log.contains(&secret), "content of the import in the log");
    }
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D7, the other half: without the server's word on the clock (the account is not
/// connected while it delivers) the row that is due goes to the core with its expiry, and
/// the core's policy deals with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_row_that_is_due_is_delivered_with_its_expiry_while_nobody_confirmed_the_clock() {
    let Some(world) = World::start().await else {
        return;
    };
    let (linked, _, held) =
        linked_with_an_archive(&world, &history_with_a_number_only_chat()).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    let mut child = spawn_child(
        &world,
        0,
        Some(kourier_signal::failpoints::AFTER_HISTORY_IMPORT_COMMIT),
    );
    wait_aborted(&mut child).await;
    // This start is never told to connect: nothing confirms its clock.
    let mut core = world.restart(network());
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    assert_eq!(world.row("deliver_expired"), Some(0));
    assert_eq!(world.delivered(), 8);
    let due = world
        .sink
        .message_values(&user(ANN), &(T0 + 6).to_string())
        .pop()
        .expect("the due row was delivered");
    assert!(
        due["expiry"]["started_at"].as_i64().is_some(),
        "with its expiry"
    );
    core.send(Command::Connect).await;
    world.expect_arrival(held).await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// How many delivery rows the recorded stage makes with rows of `rows` messages: one of
/// people, one of chats, and each shown chat's batches.
/// The delivery rows of `dump` at `rows` messages a batch: the people, the chats, the pins
/// when any chat is pinned, and every shown chat's batches (a chat without a row still
/// hears that nothing follows).
fn expected_rows(dump: &StageDump, rows: u64) -> i64 {
    let batches: u64 = dump
        .chats
        .iter()
        .filter(|chat| chat.shown)
        .map(|chat| chat.rows.div_ceil(rows).max(1))
        .sum();
    let pins = u64::from(dump.chats.iter().any(|chat| chat.pinned.is_some()));
    i64::try_from(2 + pins + batches).expect("rows")
}

/// [`history`] with what roams on the chat frames: pins with gaps (Dan's chat has no item
/// and is pinned), Ann's chat archived, muted for ever, with a timer of a minute; Ben's
/// marked unread with a timer version and no timer; Note to Self pinned too.
fn history_with_pins_and_timers(generated: u32) -> HistorySpec {
    let mut spec = history(generated);
    spec.chats[0].pinned_order = Some(5);
    spec.chats[0].archived = true;
    spec.chats[0].mute_until_ms = Some(i64::MAX as u64);
    spec.chats[0].expiration_timer_ms = Some(60_000);
    spec.chats[0].expire_timer_version = 3;
    spec.chats[1].marked_unread = true;
    spec.chats[1].expire_timer_version = 4;
    spec.chats[2].pinned_order = Some(9);
    spec.chats[3].pinned_order = Some(2);
    spec
}

/// The events of the sink named `names`, in commit order, as `(name, value of the name)`.
fn sink_events(world: &World, names: &[&str]) -> Vec<(String, serde_json::Value)> {
    let conn = kourier_db::open_read_only(
        &world.tmp.path().join("sink.db"),
        &kourier_db::DbKey::from([7; 32]),
    )
    .expect("sink");
    let rows: Vec<(String, String)> = conn
        .prepare("SELECT name, detail FROM events WHERE detail IS NOT NULL ORDER BY seq")
        .and_then(|mut stmt| {
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect()
        })
        .expect("events");
    rows.into_iter()
        .filter(|(name, _)| names.contains(&name.as_str()))
        .map(|(name, detail)| {
            let mut value: serde_json::Value = serde_json::from_str(&detail).expect("json");
            let inner = value
                .get_mut(&name)
                .map(serde_json::Value::take)
                .unwrap_or(value);
            (name, inner)
        })
        .collect()
}

/// The `ConversationUpsert` fields of `conv` merged in commit order (the later wins).
fn merged_upserts(world: &World, conv: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut merged = serde_json::Map::new();
    for patch in world.sink.event_values("ConversationUpsert") {
        if patch["key"] == conv
            && let serde_json::Value::Object(fields) = patch
        {
            merged.extend(fields);
        }
    }
    merged
}

/// Step S4a: the direct chats and Note to Self take what the phone's chat frames say. The
/// stage holds it; the chats' upserts carry archived, marked unread and the mute ("for
/// ever"); the 1:1 timer and its version are in `peers` and ride on the chat's features;
/// ONE `PinnedConversations` comes after every chat's upsert and before the first row, in
/// the phone's order (with gaps), and names a pinned chat without any item, which is
/// listed; a blocked contact's chat is blocked with `peers` saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_chats_take_the_archives_pins_archive_mute_marks_and_timer() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = history_with_pins_and_timers(0);
    spec.contacts[0].blocked = true;
    let (linked, _, held) = linked_with_an_archive(&world, &spec).await;
    let Linked { core, key, device } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    // The stage, as the commit left it.
    let mut child = spawn_child(
        &world,
        0,
        Some(kourier_signal::failpoints::AFTER_HISTORY_IMPORT_COMMIT),
    );
    wait_aborted(&mut child).await;
    let dump = read_stage(&world.transfer_dir(), &key).expect("the recorded stage");
    let ann = format!("direct:{}", user(ANN));
    let ben = format!("direct:pni:{}", BEN_PNI.hyphenated());
    let dan = format!("direct:{}", user(DAN));
    let own = format!("direct:aci:{}", device.aci);
    let staged = |conv: &str| {
        dump.chats
            .iter()
            .find(|chat| chat.conv == conv)
            .unwrap_or_else(|| panic!("{conv} is staged"))
    };
    let a = staged(&ann);
    assert_eq!(
        (a.pinned, a.archived, a.marked_unread, a.muted_until),
        (Some(5), true, false, Some(i64::MAX))
    );
    assert_eq!((a.timer_secs, a.timer_version), (Some(60), 3));
    let b = staged(&ben);
    assert_eq!(
        (b.pinned, b.marked_unread, b.timer_secs, b.timer_version),
        (None, true, None, 4)
    );
    let d = staged(&dan);
    assert_eq!(
        (d.pinned, d.rows, d.shown),
        (Some(2), 0, true),
        "pinned, no item, shown"
    );
    assert_eq!(staged(&own).pinned, Some(9));
    assert_eq!(dump.shown_chats, 4);

    let core = world.restart(network());
    finish(&world, core, held).await;
    // The upserts.
    let a = merged_upserts(&world, &ann);
    assert_eq!(a["archived"], true);
    assert_eq!(a["muted_until"]["Set"], i64::MAX, "muted for ever");
    assert_eq!(a["blocked"], true);
    assert_eq!(a["timer"]["Set"]["secs"], 60);
    assert_eq!(a["timer_version"], 3);
    let b = merged_upserts(&world, &ben);
    assert_eq!(b["marked_unread"], true);
    assert!(b.get("archived").is_none() && b.get("timer").is_none());
    assert!(
        merged_upserts(&world, &dan).contains_key("features"),
        "Dan's chat is listed"
    );
    // The pins: one list, after every upsert of the import and before its first row.
    let events = sink_events(
        &world,
        &["ConversationUpsert", "PinnedConversations", "HistoryBatch"],
    );
    let pins: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| name == "PinnedConversations")
        .map(|(i, _)| i)
        .collect();
    let first_pins = *pins.first().expect("the pins");
    assert_eq!(
        events[first_pins].1,
        serde_json::json!([dan, ann, own]),
        "ascending pinned order"
    );
    let last_upsert = events
        .iter()
        .rposition(|(name, v)| {
            name == "ConversationUpsert"
                && [&ann, &ben, &dan, &own]
                    .iter()
                    .any(|c| v["key"] == c.as_str())
        })
        .expect("upserts");
    let first_batch = events
        .iter()
        .position(|(name, _)| name == "HistoryBatch")
        .expect("batches");
    assert!(
        last_upsert < first_pins && first_pins < first_batch,
        "chats, pins, rows"
    );
    // The backend's rows: the timer and its version (also without a timer), blocked.
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(-1);
    let peer = |id: String, column: &str| {
        int(&format!(
            "SELECT {column} FROM peers WHERE service_id LIKE '%{id}%'"
        ))
    };
    assert_eq!(peer(ANN.hyphenated().to_string(), "expire_timer"), 60);
    assert_eq!(
        peer(ANN.hyphenated().to_string(), "expire_timer_version"),
        3
    );
    assert_eq!(peer(ANN.hyphenated().to_string(), "blocked"), 1);
    assert_eq!(peer(BEN_PNI.hyphenated().to_string(), "expire_timer"), 0);
    assert_eq!(
        peer(BEN_PNI.hyphenated().to_string(), "expire_timer_version"),
        4
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The pins are held until every row before them is settled (review of S4a, all three
/// lenses): the core cannot take the row of the four chats at once and refuses it for good;
/// it comes back as single chats, and no `PinnedConversations` reaches the core before the
/// `ConversationUpsert` of a pinned chat, or the core would make a bare chat of the key.
/// Without the hold the pins row, appended behind the refused row, is taken first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_pins_wait_for_a_row_of_chats_the_core_refused_and_for_its_singles() {
    let Some(world) = World::start().await else {
        return;
    };
    // The refusal takes a moment, as a core that is busy: the pins row is appended meanwhile
    // if nothing holds it.
    world.sink.set_behavior("refuse_wide_chats", Some("1"));
    world
        .sink
        .set_behavior("refuse_wide_chats_delay_ms", Some("400"));
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history_with_pins_and_timers(0));
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert!(
        world
            .sink
            .behavior("refused_wide")
            .and_then(|n| n.parse::<u32>().ok())
            .is_some_and(|n| n >= 1),
        "the core refused a row of several chats"
    );
    let ann = format!("direct:{}", user(ANN));
    let dan = format!("direct:{}", user(DAN));
    let own = format!("direct:aci:{}", linked.device.aci);
    let events = sink_events(&world, &["ConversationUpsert", "PinnedConversations"]);
    let first_pins = events
        .iter()
        .position(|(name, _)| name == "PinnedConversations")
        .expect("the pins");
    for chat in [&ann, &dan, &own] {
        let upserted = events
            .iter()
            .position(|(name, v)| name == "ConversationUpsert" && v["key"] == chat.as_str())
            .unwrap_or_else(|| panic!("{chat} was upserted"));
        assert!(upserted < first_pins, "the upsert of {chat} comes first");
    }
    assert_eq!(
        events[first_pins].1,
        serde_json::json!([dan, ann, own]),
        "and the pins are the phone's, whole"
    );
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D10: where the archive says pinned, archived, marked unread, muted and blocked and the
/// Storage Service, which has no previous record, says otherwise, the sync wins: for a
/// contact and for Note to Self.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_storage_sync_wins_over_the_archive_where_they_differ() {
    let Some(world) = World::start().await else {
        return;
    };
    // The phone's Storage Service: Alice's record and the account record say "no" to all.
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: world.phone.device_ref.clone(),
            contacts: vec![StorageContact {
                archived: Some(false),
                marked_unread: Some(false),
                blocked: Some(false),
                ..StorageContact::of(&world.alice.device_ref)
            }],
            account: Some(StorageAccount {
                note_to_self_archived: Some(false),
                note_to_self_marked_unread: Some(false),
                ..StorageAccount::default()
            }),
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    let alice_aci: Uuid = world.alice.aci.parse().expect("Alice's ACI");
    let spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(alice_aci),
            profile_name: format!("{MARKER} Alice"),
            profile_sharing: true,
            blocked: true,
            ..ContactSpec::default()
        }],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: vec![incoming("a1", T0 + 1, true)],
                archived: true,
                marked_unread: true,
                mute_until_ms: Some(i64::MAX as u64),
                pinned_order: Some(1),
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![outgoing("note", T0 + 2, &[])],
                archived: true,
                marked_unread: true,
                pinned_order: Some(2),
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    };
    let (mut linked, _, held) = linked_with_an_archive(&world, &spec).await;
    let alice = format!("direct:aci:{}", world.alice.aci);
    let own = format!("direct:aci:{}", linked.device.aci);
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_arrival(held).await;
    // The sync follows the transfer.
    world
        .files
        .wait_for_i64(
            "the first Storage Service sync",
            "SELECT count(*) FROM account WHERE manifest_version IS NOT NULL",
            |n| n == Some(1),
        )
        .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let pins = world.sink.event_values("PinnedConversations");
        if pins.len() >= 2 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "the sync's pins");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The import said yes ...
    let pins = world.sink.event_values("PinnedConversations");
    assert_eq!(
        pins[0],
        serde_json::json!([alice, own]),
        "the archive's pins"
    );
    // ... and the sync, which had no previous record, says no and wins.
    assert_eq!(pins.last(), Some(&serde_json::json!([])), "the sync's pins");
    let a = merged_upserts(&world, &alice);
    assert_eq!(a["archived"], false);
    assert_eq!(a["marked_unread"], false);
    assert_eq!(a["muted_until"], "Clear");
    assert_eq!(a["blocked"], false);
    let n = merged_upserts(&world, &own);
    assert_eq!(n["archived"], false);
    assert_eq!(n["marked_unread"], false);
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT blocked FROM peers WHERE service_id = '{}'",
                world.alice.aci
            ))
            .expect("peers"),
        Some(0),
        "the backend agrees with the core on blocked"
    );
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// I4: a kill at every zone of the delivery (before and after a chunk's zone, after the
/// zone that settles acknowledged rows, between the core's acknowledgement and the
/// forwarder's delete, before `done`). Every start goes on at the cursor, and at the end
/// every row was appended exactly once and the core holds every message.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_at_every_zone_of_the_delivery_goes_on_at_its_cursor_and_appends_every_row_once() {
    use kourier_signal::failpoints as fp;
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 2000;
    let rows = 25;
    // With pins (a row of their own), timers (written in the chats zone) and a pinned group
    // (its `groups` row and its refresh job are written in the chats zone too, step S4b).
    let mut spec = history_with_pins_and_timers(generated);
    spec.groups
        .push(kourier_signal::archive_testing::GroupSpec {
            master_key: vec![0x4b; 32],
            snapshot: Some(kourier_signal::archive_testing::SnapshotSpec {
                title: Some(format!("{MARKER} group")),
                members: vec![(kourier_signal::archive_testing::Party::Contact(0), false)],
                ..Default::default()
            }),
            ..Default::default()
        });
    spec.chats.push(ChatSpec {
        with: ChatWith::Group(0),
        pinned_order: Some(7),
        texts: vec![TextSpec {
            author: Some(kourier_signal::archive_testing::Who::Contact(0)),
            ..incoming(&format!("{MARKER} in the group"), T0 + 50, true)
        }],
        ..ChatSpec::default()
    });
    let (linked, frames, held) = linked_with_an_archive(&world, &spec).await;
    let Linked { core, key, .. } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let kills = [
        format!("{}:1", fp::BEFORE_HISTORY_DELIVERY_CHUNK),
        format!("{}:1", fp::AFTER_HISTORY_DELIVERY_CHUNK),
        format!("{}:3", fp::AFTER_HISTORY_DELIVERY_CHUNK),
        format!("{}:4", fp::BEFORE_HISTORY_DELIVERY_CHUNK),
        format!("{}:2", fp::AFTER_HISTORY_DELIVERY_SETTLE),
        format!("{}:3", fp::AFTER_CORE_COMMIT),
        // INSIDE the zone, after the row and its entry and before the cursor: a rollback.
        format!("{}:2", fp::INSIDE_HISTORY_DELIVERY_CHUNK),
        format!("{}:7", fp::INSIDE_HISTORY_DELIVERY_CHUNK),
        // At the commit, before the zone returns: the row, its entry and the cursor are
        // durable together (a cursor written by a second zone would be missing here).
        format!("{}:1", fp::AT_HISTORY_DELIVERY_COMMIT),
        format!("{}:5", fp::AT_HISTORY_DELIVERY_COMMIT),
        format!("{}:6", fp::AFTER_HISTORY_DELIVERY_CHUNK),
        format!("{}:4", fp::AFTER_HISTORY_DELIVERY_SETTLE),
        format!("{}:2", fp::BEFORE_HISTORY_DELIVERY_CHUNK),
        fp::BEFORE_HISTORY_DELIVERY_DONE.to_owned(),
    ];
    let mut appended = 0;
    let mut want = None;
    for (i, kill) in kills.iter().enumerate() {
        let mut child = spawn_child_with(&world, 0, Some(kill), rows);
        wait_aborted(&mut child).await;
        let now = world.row("deliver_rows").expect("rows");
        assert!(now >= appended, "the cursor never steps back");
        // Every row that was appended has its entry or was acknowledged: never more than
        // two wait.
        let waiting = world
            .files
            .peek_i64("SELECT count(*) FROM history_delivery")
            .expect("entries")
            .unwrap_or(0);
        assert!(waiting <= 2, "{waiting} unsettled rows after kill {i}");
        // ONE zone: a row, its entry and the count that moves with the cursor are written
        // together. A child that is killed at the k-th row's commit (`AT_…_COMMIT`, which lies
        // after the commit and before anything a second zone would write) or right after it
        // has counted exactly k rows; one killed INSIDE the k-th row's zone has counted k - 1,
        // the zone being rolled back. A cursor written by a second zone fails the first.
        println!("kill {i} ({kill}): {waiting} unsettled rows, {now} counted");
        if let Some((name, n)) = kill.split_once(':')
            && let Ok(n) = n.parse::<i64>()
        {
            let counted = if name == fp::INSIDE_HISTORY_DELIVERY_CHUNK {
                Some(n - 1)
            } else if name == fp::AT_HISTORY_DELIVERY_COMMIT
                || name == fp::AFTER_HISTORY_DELIVERY_CHUNK
            {
                Some(n)
            } else {
                None
            };
            if let Some(counted) = counted {
                assert_eq!(
                    now,
                    appended + counted,
                    "kill {i} ({kill}): the rows appended and the rows counted differ: \
                     the zone of a row is not one transaction"
                );
            }
        }
        appended = now;
        if want.is_none() {
            let dump = read_stage(&world.transfer_dir(), &key).expect("the recorded stage");
            want = Some(expected_rows(&dump, rows as u64));
        }
        assert_eq!(
            world.row("read_starts"),
            Some(1),
            "the archive is read once"
        );
    }
    let want = want.expect("rows");
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 1"),
        Some(1),
        "killed before `done`: the transfer is not over"
    );
    assert_eq!(appended, want, "every row is appended before `done`");
    let core = world.restart(rows_of(network(), rows));
    finish(&world, core, held).await;
    assert_eq!(
        world.row("deliver_rows"),
        Some(want),
        "every row was appended exactly once"
    );
    assert_eq!(
        world.row("frames"),
        Some(i64::try_from(frames).expect("frames"))
    );
    assert!(
        world.sink.history_deliveries() >= world.row("stage_messages").unwrap_or(0),
        "every message reached the core"
    );
    // The pins reached the core, every time as the same list, and Ann's timer stands.
    let pins = world.sink.event_values("PinnedConversations");
    assert!(!pins.is_empty(), "the pins reached the core");
    assert!(
        pins.iter().all(|list| *list == pins[0]),
        "a pin list appended again is the same list"
    );
    assert_eq!(
        pins[0].as_array().map(Vec::len),
        Some(4),
        "three chats and the group"
    );
    // The group: one row, its features recorded, and its refresh queued at most once.
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM groups WHERE features_hash IS NOT NULL")
            .expect("groups"),
        Some(1)
    );
    assert!(
        world
            .files
            .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'group_refresh'")
            .expect("jobs")
            .unwrap_or(0)
            <= 1
    );
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT expire_timer * 100 + expire_timer_version FROM peers
                  WHERE service_id LIKE '%{}%'",
                ANN.hyphenated()
            ))
            .expect("peers"),
        Some(6_003)
    );
    assert_eq!(
        merged_upserts(&world, &format!("direct:{}", user(ANN)))["timer"]["Set"]["secs"],
        60,
        "the timer reached the core with the chat"
    );
    println!(
        "{} kills, {want} delivery rows, {} deliveries of {} messages",
        kills.len(),
        world.sink.history_deliveries(),
        world.delivered()
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// I5 and I9: twenty thousand messages to a core that acknowledges slowly. `backend.db`
/// never holds more than two delivery rows (and so stays small); the transfer says `Done`
/// only when the core has every message, and the login's `Done` comes after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backend_db_stays_small_while_twenty_thousand_messages_are_delivered() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 20_000;
    world.sink.set_behavior("ack_delay_ms", Some("30"));
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(generated));
    world.upload(&linked.device, &path).await;
    // A watcher of backend.db while the delivery runs.
    let files = AccountFiles {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid,
        key: world.files.key,
    };
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watcher = {
        let stop = stop.clone();
        let sink = world.sink.clone();
        tokio::task::spawn_blocking(move || {
            let (mut most_rows, mut most_bytes, mut looks) = (0, 0, 0u32);
            // What the core had committed at the first look that found `done` written.
            let mut at_done = None;
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                if at_done.is_none()
                    && files
                        .peek_i64("SELECT state = 'done' FROM history_transfer")
                        .ok()
                        .flatten()
                        == Some(1)
                {
                    at_done = Some(sink.history_deliveries());
                }
                let rows = files
                    .peek_i64(
                        "SELECT count(*) FROM core_outbox
                          WHERE seq IN (SELECT seq FROM history_delivery)",
                    )
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                let bytes = files
                    .peek_i64("SELECT coalesce(sum(length(envelope)), 0) FROM core_outbox")
                    .ok()
                    .flatten()
                    .unwrap_or(0);
                most_rows = most_rows.max(rows);
                most_bytes = most_bytes.max(bytes);
                looks += u32::from(rows > 0);
                std::thread::sleep(Duration::from_millis(3));
            }
            (most_rows, most_bytes, looks, at_done)
        })
    };
    // The order of the two ends, as the core hears them.
    let deadline = tokio::time::Instant::now() + 3 * WAIT;
    let (mut transfer_done, mut login_done) = (false, false);
    while !(transfer_done && login_done) {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match linked.core.next_ephemeral(left).await {
            Some(BackendEvent::HistoryTransfer(HistoryTransfer::Done)) => {
                assert!(!login_done, "the login was done before the transfer");
                // `done` was written after the last acknowledgement: the core has it all.
                assert_eq!(world.delivered(), i64::from(generated) + 6);
                transfer_done = true;
            }
            Some(BackendEvent::HistoryTransfer(state)) => {
                assert!(!state.is_over(), "the transfer ended as {state:?}");
            }
            Some(BackendEvent::Login(step)) => match step.kind {
                LoginStepKind::Done => {
                    assert!(transfer_done, "the login's Done came before the transfer's");
                    login_done = true;
                }
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            Some(_) => {}
            None => panic!("the delivery never ended"),
        }
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    stop.store(true, std::sync::atomic::Ordering::Release);
    let (most_rows, most_bytes, looks, at_done) = watcher.await.expect("watcher");
    // I9, seen from the files: when `done` stood in backend.db, the core had committed
    // every message (the last row was acknowledged before `done` was written).
    assert_eq!(
        at_done,
        Some(i64::from(generated) + 6),
        "`done` was written before the core had the last row"
    );
    println!(
        "while delivering: at most {most_rows} delivery rows and {most_bytes} bytes in \
         core_outbox ({looks} looks with a row)"
    );
    assert!(looks > 10, "the watcher saw the delivery");
    assert!(most_rows <= 2, "{most_rows} delivery rows at once");
    assert!(
        most_bytes < 3 * 1024 * 1024,
        "{most_bytes} bytes waited for the core"
    );
    world.expect_over("done", None);
    world.expect_delivered();
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// A quit in the middle of the delivery: the next start goes on at the cursor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_quit_during_the_delivery_goes_on_at_its_cursor() {
    let Some(world) = World::start().await else {
        return;
    };
    let (generated, rows) = (3000, 25);
    world.sink.set_behavior("ack_delay_ms", Some("40"));
    let linked = world.link(rows_of(network(), rows)).await;
    let held = world
        .alice_says(&linked.device, "waits through a quit")
        .await;
    let (path, _) = world.archive(&linked, &history(generated));
    world.upload(&linked.device, &path).await;
    world
        .files
        .wait_for_i64(
            "some rows are delivered",
            "SELECT deliver_rows FROM history_transfer",
            |n| n.is_some_and(|n| n >= 6),
        )
        .await;
    let Linked { core, key, .. } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let at_quit = world.row("deliver_rows").expect("rows");
    let dump = read_stage(&world.transfer_dir(), &key).expect("the recorded stage");
    let want = expected_rows(&dump, rows as u64);
    assert!(at_quit < want, "the quit fell into the delivery");
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 1"),
        Some(1)
    );
    assert!(world.delivered() < i64::from(generated));
    world.sink.set_behavior("ack_delay_ms", None);
    finish(&world, world.restart(rows_of(network(), rows)), held).await;
    assert_eq!(
        world.row("deliver_rows"),
        Some(want),
        "no row twice, none left out"
    );
    assert_eq!(world.row("read_starts"), Some(1));
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D2: a stage that is lost during the delivery. The record is taken back, the archive is
/// read again (a read start) and delivered from its first row; what the core already had
/// is not doubled. And when the archive cannot be read either, the transfer ends
/// `Incomplete`: what was delivered stays, and receiving starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stage_that_is_lost_after_the_commit_is_read_again_and_nothing_doubles() {
    use kourier_signal::failpoints as fp;
    let Some(world) = World::start().await else {
        return;
    };
    let (generated, rows) = (1500, 25);
    let (linked, _, held) = linked_with_an_archive(&world, &history(generated)).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    let kill = format!("{}:12", fp::AFTER_HISTORY_DELIVERY_CHUNK);
    let mut child = spawn_child_with(&world, 0, Some(&kill), rows);
    wait_aborted(&mut child).await;
    assert_eq!(world.row("deliver_rows"), Some(12));
    assert_eq!(world.row("deliver_began"), Some(1));
    std::fs::remove_file(world.stage_file()).expect("lose the stage");

    // Read again, delivered again from the first row, killed again in the delivery.
    let mut child = spawn_child_with(&world, 0, Some(&kill), rows);
    wait_aborted(&mut child).await;
    assert_eq!(world.row("read_starts"), Some(2), "the second read counts");
    assert_eq!(world.row("import_staged"), Some(1));
    let before = world.delivered();
    assert!(before > 0 && before < i64::from(generated));
    assert!(
        world.sink.history_deliveries() > before,
        "rows the core had were delivered again, and each message is there once"
    );

    // Now neither the stage nor the archive can be read.
    std::fs::remove_file(world.stage_file()).expect("lose the stage again");
    let archive = world.transfer_dir().join("archive");
    let len = std::fs::metadata(&archive).expect("archive").len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&archive)
        .and_then(|file| file.set_len(len / 2))
        .expect("cut the archive");
    let mut core = world.restart(rows_of(network(), rows));
    core.send(Command::Connect).await;
    let last = until(&mut core, "over", HistoryTransfer::is_over).await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Incomplete)
    );
    world.expect_over("failed", Some("incomplete"));
    assert!(world.delivered() >= before, "what was delivered stays");
    // Receiving is released.
    world.expect_arrival(held).await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D3: a row the core refuses for good is found in `core_outbox_poison` after its
/// acknowledgement and comes back as single messages; the one message the core refuses is
/// dropped and counted, everything else arrives, and the transfer is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poisoned_row_comes_back_as_single_messages_and_the_bad_one_is_dropped() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 300;
    // The core refuses every row that holds Ann's second message.
    world
        .sink
        .set_behavior("refuse", Some(&format!("{MARKER} a2")));
    let mut linked = world.link(rows_of(network(), 50)).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(generated));
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert_eq!(world.row("deliver_dropped"), Some(1), "one message dropped");
    let staged = world.row("stage_messages").expect("staged");
    assert_eq!(world.delivered(), staged - 1, "everything else arrived");
    let own = "";
    assert!(world.sink.message(own, &(T0 + 4).to_string()).is_none());
    for n in [1, 7] {
        assert!(
            world
                .sink
                .message(&user(ANN), &(T0 + n).to_string())
                .is_some(),
            "the good messages of the refused row arrived as singles"
        );
    }
    // No unread count is sent, for Ann's chat or any other: her unread message says so
    // itself, and a dropped row is counted by nobody.
    let hinted = world
        .sink
        .events("ConversationUpsert")
        .into_iter()
        .flatten()
        .filter(|detail| detail.contains("\"unread_hint\":"))
        .count();
    assert_eq!(hinted, 0);
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    assert_eq!(int("SELECT count(*) FROM history_delivery"), 0);
    // Neither the refused row of three (it came back) nor the refused single stays in the
    // poison table: a refused history must not pile up in backend.db with its content.
    assert_eq!(int("SELECT count(*) FROM core_outbox_poison"), 0);
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D3: a core that acknowledges nothing. The unacknowledged row is taken back after the
/// bound of RUNNING time and appended again as single messages; a single that is not
/// acknowledged either is dropped; and when nothing helps the transfer ends `Incomplete`
/// and receiving starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivery_that_cannot_advance_is_split_and_ends_incomplete_with_receiving_released() {
    let Some(world) = World::start().await else {
        return;
    };
    world.sink.set_behavior("stall", Some("1"));
    let bounded = rows_of(network(), 50).with_history_delivery_bounds(
        3,
        Duration::from_secs(2),
        Duration::from_millis(200),
        2,
    );
    let mut linked = world.link(bounded).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(400));
    world.upload(&linked.device, &path).await;
    // First strike: the OLDEST row (of several messages) becomes a piece, appended again as
    // singles, and so does the row of several messages behind it.
    world
        .files
        .wait_for_i64(
            "single messages are appended",
            "SELECT count(*) FROM history_delivery WHERE single = 1 AND seq IS NOT NULL",
            |n| n.is_some_and(|n| n > 0),
        )
        .await;
    assert_eq!(world.row("deliver_strikes"), Some(1));
    assert_eq!(world.row("state = 'reading'"), Some(1));
    // Second strike: the oldest of the singles, which is dropped. Nothing helps.
    world.wait_state("failed").await;
    assert_eq!(world.row("reason = 'incomplete'"), Some(1));
    assert!(
        world.row("deliver_dropped").unwrap_or(0) >= 1,
        "the singles were dropped"
    );
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    assert_eq!(int("SELECT count(*) FROM history_delivery"), 0);
    // The core comes back to life: it hears how the transfer ended, and receiving starts.
    world.sink.set_behavior("stall", None);
    let last = until(&mut linked.core, "over", HistoryTransfer::is_over).await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Incomplete)
    );
    world.expect_over("failed", Some("incomplete"));
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D2, the rebuilt core: `core.db` is lost in the middle of a delivery and the next start
/// is a `Rebuild`. The new core must get the whole import, not its tail: the record of the
/// stage is taken back, the archive is read again and everything is delivered from its
/// first row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rebuilt_core_gets_the_whole_import_and_not_its_tail() {
    use kourier_signal::failpoints as fp;
    let Some(world) = World::start().await else {
        return;
    };
    let (generated, rows) = (1500, 25);
    let (linked, _, held) = linked_with_an_archive(&world, &history(generated)).await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    let kill = format!("{}:20", fp::AFTER_HISTORY_DELIVERY_CHUNK);
    let mut child = spawn_child_with(&world, 0, Some(&kill), rows);
    wait_aborted(&mut child).await;
    let tail = i64::from(generated) + 6 - world.delivered();
    assert!(tail > 0 && world.delivered() > 0, "killed in the middle");

    // The core's database is gone: a new, empty one, and a start that says so.
    let new_core = Sink::open(&world.tmp.path().join("rebuilt-sink.db")).expect("new sink");
    let mut core = FakeCore::start_with(
        rows_of(network(), rows),
        &world.env,
        &world.files,
        StartMode::Rebuild,
        &new_core,
    )
    .expect("respawn the actor");
    core.send(Command::Connect).await;
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert_eq!(
        world.row("read_starts"),
        Some(2),
        "the archive was read again"
    );
    let in_new_core: i64 = archive_users()
        .iter()
        .map(|user| new_core.messages_from(user))
        .sum::<i64>()
        + new_core.messages_from("");
    assert_eq!(
        in_new_core,
        world.row("stage_messages").expect("staged"),
        "the rebuilt core holds the whole import"
    );
    let author = format!("aci:{}", world.alice.aci);
    new_core
        .wait_message(&author, &held.to_string(), WAIT)
        .await
        .expect("receiving started");
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// D2 (c), the known exception: a logout in the middle of a delivery. The transfer ends
/// with the login (its row, its unsettled delivery rows and its files are gone, the actor
/// ends in an orderly way) and what the core was given before stays where it is: nothing
/// is withdrawn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_during_the_delivery_ends_the_transfer_and_withdraws_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    world.sink.set_behavior("ack_delay_ms", Some("40"));
    let linked = world.link(rows_of(network(), 25)).await;
    let (path, _) = world.archive(&linked, &history(3000));
    world.upload(&linked.device, &path).await;
    world
        .files
        .wait_for_i64(
            "some rows are delivered",
            "SELECT deliver_rows FROM history_transfer",
            |n| n.is_some_and(|n| n >= 6),
        )
        .await;
    let before = world.delivered();
    assert!(before > 0);
    let exit = linked.core.exit_with(Command::Logout).await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    assert_eq!(int("SELECT count(*) FROM history_transfer"), 0);
    assert_eq!(int("SELECT count(*) FROM history_delivery"), 0);
    assert!(
        !world.transfer_dir().exists(),
        "the archive and the stage are removed"
    );
    assert!(world.delivered() >= before, "what was delivered stays");
    assert!(world.delivered() < 3006, "and the rest never came");
    world.harness.shutdown().await.expect("harness shutdown");
}

// What the independent review of step S2 found (`target/s2-context/review.md`), fixed
// (`target/s2-context/fix.md`).

/// Review F2. A core that refuses EVERY history row for good (another version of the
/// contract, a check that fails for every message): every row comes back as singles and each
/// single is refused. That is no progress: after a bound of single units refused in a row the
/// transfer ends `Incomplete`, receiving starts, nothing of the history is kept in the poison
/// table (it was `done`, with the whole history in `core_outbox_poison`), and the feeder is
/// not started anew for every refused single.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_core_that_refuses_every_history_row_ends_the_transfer_incomplete_and_keeps_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    // Every text contains the empty string: every envelope with a history batch is refused.
    world.sink.set_behavior("refuse", Some(""));
    let mut linked = world.link(rows_of(network(), 50)).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(300));
    world.upload(&linked.device, &path).await;
    let last = until(&mut linked.core, "over", HistoryTransfer::is_over).await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Incomplete)
    );
    world.expect_over("failed", Some("incomplete"));
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    let staged = world.row("stage_messages").expect("staged");
    let dropped = world.row("deliver_dropped").expect("dropped");
    println!(
        "ended as {last:?}: {staged} staged, {} in the core, {dropped} dropped, {} refused rows \
         left in core_outbox_poison, {} delivery rows",
        world.delivered(),
        int("SELECT count(*) FROM core_outbox_poison"),
        world.row("deliver_rows").unwrap_or(0)
    );
    assert_eq!(world.delivered(), 0, "the core took nothing");
    assert!(
        (20..=30).contains(&dropped),
        "{dropped} single units were refused before it gave up, of {staged}"
    );
    // At most the rows that were on their way when it gave up (two), refused later.
    assert!(
        int("SELECT count(*) FROM core_outbox_poison") <= 2,
        "the refused history is kept in backend.db"
    );
    assert_eq!(int("SELECT count(*) FROM history_delivery"), 0);
    // Receiving is released.
    world.sink.set_behavior("refuse", None);
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Review F5. The last row of a chat is due under a clock the server confirmed, so it is not
/// delivered: the chat still hears that nothing follows (every batch before said that more
/// does). One message per row puts the due one alone in the chat's last row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_end_of_a_chat_whose_last_row_is_due_is_still_told_to_the_core() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(rows_of(network(), 1)).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history_with_a_number_only_chat());
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert_eq!(
        world.row("deliver_expired"),
        Some(1),
        "the due row was not delivered"
    );
    let ann_chat = kourier_model::ConvKey::Direct(
        kourier_model::UserId::parse(&user(ANN)).expect("Ann's user id"),
    )
    .encode();
    let ann = events_of_the_core(&world)
        .into_iter()
        .filter(|(name, value)| {
            name == "HistoryBatch" && value["conv"].as_str() == Some(ann_chat.as_str())
        })
        .map(|(_, value)| {
            (
                value["n"].as_u64().unwrap_or(99),
                value["has_more"].as_bool(),
            )
        })
        .collect::<Vec<_>>();
    // Ann's chat: three rows of one message and the due one, whose place is an empty batch
    // that says nothing follows. None before it does.
    assert_eq!(
        ann,
        [
            (1, Some(true)),
            (1, Some(true)),
            (1, Some(true)),
            (0, Some(false))
        ],
        "what the core heard of Ann's chat"
    );
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Review F3a. The core cannot store ONE message (its ingest fails, as a retryable error:
/// the real core answers every failure but a JSON one so). Its row waits for the core, then
/// it comes back as singles, and the single waits again. The message behind it in the window
/// was never tried by the core, which stops at the first job that fails: only the bad
/// message is dropped, every other one arrives, and the transfer is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_the_core_cannot_store_is_dropped_alone_and_the_one_behind_it_arrives() {
    let Some(world) = World::start().await else {
        return;
    };
    let bad = format!("{MARKER} 137");
    world.sink.set_behavior("stick", Some(&bad));
    // The forwarder backs off after each refusal (100 ms, doubling), so a bound of two
    // seconds lets it try a few times and leaves it asleep at the strike.
    let bounded = rows_of(network(), 50).with_history_delivery_bounds(
        3,
        Duration::from_secs(2),
        Duration::from_millis(200),
        3,
    );
    let mut linked = world.link(bounded).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(300));
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    let staged = world.row("stage_messages").expect("staged");
    // The core has every message but the bad one: the one behind it, which it never tried
    // (it stops at the first job that fails), was not dropped with it. The count may say
    // two: the row before the bad one in the same envelope is applied and still waits for
    // the answer to the whole envelope, and is the oldest when the bound runs out.
    assert_eq!(world.delivered(), staged - 1, "everything else arrived");
    let dropped = world.row("deliver_dropped").unwrap_or(0);
    assert!((1..=2).contains(&dropped), "{dropped} dropped");
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    assert_eq!(int("SELECT count(*) FROM history_delivery"), 0);
    // It waited two bounds (its row, then itself) and the total remembers it.
    assert!(
        world.row("deliver_stuck_ms").unwrap_or(0) >= 4_000,
        "{:?}",
        world.row("deliver_stuck_ms")
    );
    world.sink.set_behavior("stick", None);
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Review F3b. Messages the core cannot store, one here and another there: each holds
/// receiving back for the bound twice (its row, then itself), and the rows around them are
/// acknowledged in between, which ends the strikes in a row (the unit test
/// `the_time_spent_waiting_…` holds that part exactly; here the forwarder's own backoff
/// decides how many acknowledgements fall between). The total of the time spent waiting is
/// never reset, and at its bound the transfer ends `Incomplete` (the strikes in a row are
/// allowed ten here): what was delivered stays, receiving starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_the_core_cannot_store_one_after_another_end_the_transfer_at_the_total() {
    let Some(world) = World::start().await else {
        return;
    };
    // Texts 273 (the second row of the second chat delivered), 137 and 202 (the first row of
    // the third) and 51: the core is stuck on one, is let through for a while, and is stuck
    // on the next.
    let bad = ["273", "137", "202", "51"]
        .map(|n| format!("{MARKER} {n}"))
        .join("|");
    world.sink.set_behavior("stick", Some(&bad));
    // Strikes in a row cannot end it here (ten); the total can (five seconds, two and a bit
    // of the two-second bounds).
    let bounded = rows_of(network(), 50)
        .with_history_delivery_bounds(3, Duration::from_secs(2), Duration::from_millis(200), 10)
        .with_history_delivery_totals(20, Duration::from_secs(5));
    let mut linked = world.link(bounded).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(300));
    world.upload(&linked.device, &path).await;
    let last = until(&mut linked.core, "over", HistoryTransfer::is_over).await;
    assert_eq!(
        last,
        HistoryTransfer::Failed(HistoryTransferFailure::Incomplete)
    );
    world.expect_over("failed", Some("incomplete"));
    let stuck = world.row("deliver_stuck_ms").unwrap_or(0);
    println!(
        "ended as {last:?}: stuck {stuck} ms, {} strikes in a row, {} dropped, {} of {} in the core",
        world.row("deliver_strikes").unwrap_or(-1),
        world.row("deliver_dropped").unwrap_or(-1),
        world.delivered(),
        world.row("stage_messages").unwrap_or(0)
    );
    assert!(stuck >= 5_000, "the total reached its bound: {stuck} ms");
    assert!(
        world.row("deliver_strikes").unwrap_or(99) < 10,
        "not the strikes in a row"
    );
    assert!(world.delivered() > 0, "what was delivered stays");
    world.sink.set_behavior("stick", None);
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The user's report (2026-10-10): after an import "too many profile_fetch jobs which takes
/// ages". The delivery queues one fetch per person whose profile key is new, and the jobs
/// ran one after another. They run in batches now: every person of the archive with a key
/// is fetched (the name the server holds reaches the core), more than one at a time, and
/// the queue is empty at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_profiles_of_an_import_are_fetched_several_at_a_time() {
    use base64::Engine as _;
    const PEOPLE: usize = 30;
    let Some(world) = World::start().await else {
        return;
    };
    let mut contacts = Vec::new();
    let mut fetched_names = Vec::new();
    for i in 0..PEOPLE {
        let name = format!("Fetched {i}");
        let person = world
            .harness
            .create_primary(&CreatePrimary::named(&name))
            .await
            .expect("a person");
        let key: [u8; 32] = base64::engine::general_purpose::STANDARD
            .decode(&person.profile_key)
            .expect("base64")
            .try_into()
            .expect("32 bytes");
        contacts.push(ContactSpec {
            aci: Some(person.aci.parse().expect("ACI")),
            profile_name: format!("{MARKER} Archived {i}"),
            profile_sharing: true,
            profile_key: Some(key),
            ..ContactSpec::default()
        });
        fetched_names.push(name);
    }
    let spec = HistorySpec {
        chats: (0..PEOPLE)
            .map(|i| ChatSpec {
                with: ChatWith::Contact(i),
                texts: vec![incoming(&format!("{MARKER} {i}"), T0 + i as u64, true)],
                ..ChatSpec::default()
            })
            .collect(),
        contacts,
        ..HistorySpec::default()
    };
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &spec);
    let started = tokio::time::Instant::now();
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world
        .files
        .wait_for_i64(
            "every person's profile fetched",
            "SELECT count(*) FROM peers WHERE profile_fetched_at IS NOT NULL",
            |n| n.is_some_and(|n| n >= i64::try_from(PEOPLE).expect("people")),
        )
        .await
        .expect("profiles fetched");
    world
        .files
        .wait_for_i64(
            "no profile fetch left",
            "SELECT count(*) FROM jobs WHERE kind = 'profile_fetch'",
            |n| n == Some(0),
        )
        .await;
    println!(
        "{PEOPLE} profiles fetched {} ms after the upload",
        started.elapsed().as_millis()
    );
    // A fetched name is written to backend.db and to the core's outbox in one zone; the
    // forwarder brings it to the core a moment later: look until it is there.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    for name in &fetched_names {
        let needle = format!("\"{name}\"");
        loop {
            let there = events_of_the_core(&world)
                .iter()
                .any(|(event, v)| event == "ContactUpsert" && v.to_string().contains(&needle));
            if there {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the server's name of {name} reached the core"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let log = log_buffer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let log = String::from_utf8_lossy(&log);
    assert!(
        log.lines().any(|line| line.contains("jobs done")
            && line.contains("kind=\"profile_fetch\"")
            && !line.contains("ran=1 ")),
        "profile fetches ran in batches"
    );
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Link feedback L3: while the import is delivered the core hears how many messages of how
/// many are in, the count only climbs and reaches the total, and once nothing is left a
/// last report says `finished` (after the login's `Done`); nothing follows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_delivery_reports_its_message_counts_and_then_that_it_finished() {
    let Some(world) = World::start().await else {
        return;
    };
    let generated = 300;
    let mut linked = world.link(rows_of(network(), 40)).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &history(generated));
    world.upload(&linked.device, &path).await;
    let deadline = tokio::time::Instant::now() + 3 * WAIT;
    let mut counts: Vec<(u64, u64)> = Vec::new();
    let (mut login_done, mut finished) = (false, false);
    while !finished {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match linked.core.next_ephemeral(left).await {
            Some(BackendEvent::AccountProgress(p)) if p.finished => {
                assert!(login_done, "finished before the login's Done");
                finished = true;
            }
            Some(BackendEvent::AccountProgress(p)) => {
                if p.messages_total > 0 {
                    counts.push((p.messages_done, p.messages_total));
                }
            }
            Some(BackendEvent::Login(step)) => match step.kind {
                LoginStepKind::Done => login_done = true,
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            Some(_) => {}
            None => panic!("no finished report"),
        }
    }
    assert!(
        counts.len() > 3,
        "the delivery reported per chunk: {counts:?}"
    );
    let total = counts[0].1;
    assert!(
        counts.iter().all(|&(_, t)| t == total),
        "one total: {counts:?}"
    );
    assert!(
        counts.windows(2).all(|w| w[0].0 < w[1].0),
        "the count climbs and is not repeated: {counts:?}"
    );
    assert_eq!(
        counts.last().map(|c| c.0),
        Some(total),
        "it reaches the total"
    );
    world.expect_arrival(held).await;
    // Nothing more is reported once it finished.
    while let Some(event) = linked.core.next_ephemeral(Duration::from_millis(500)).await {
        assert!(
            !matches!(event, BackendEvent::AccountProgress(_)),
            "a report after the finish: {event:?}"
        );
    }
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Link feedback L3 (F3): a start that finds a transfer unfinished reports the account's work
/// again, whatever the age of the link: the UI of the new run knows nothing of the old one.
/// (A stranded group alone is the link's tail only within a day of the link: the `groups`
/// test `a_start_reports_a_stranded_group_only_within_a_day_of_the_link`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_in_the_middle_of_a_transfer_reports_the_work_again() {
    let Some(world) = World::start().await else {
        return;
    };
    let linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let Linked { core, device, key } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    // Long after the link: only the transfer is the reason to report.
    world
        .files
        .execute("UPDATE account SET linked_at = linked_at - 2592000000")
        .expect("an old link");

    let mut core = world.restart(network());
    core.send(Command::Connect).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::AccountProgress(p)) => {
                assert!(!p.finished, "finished while the transfer waits");
                break;
            }
            Some(_) => {}
            None => panic!("the start did not report the work of the unfinished transfer"),
        }
    }
    // The phone answers at last: the transfer ends and the report says so.
    let linked = Linked { core, device, key };
    let (path, _) = world.archive(&linked, &history(40));
    world.upload(&linked.device, &path).await;
    let Linked { mut core, .. } = linked;
    let deadline = tokio::time::Instant::now() + 3 * WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::AccountProgress(p)) if p.finished => break,
            Some(_) => {}
            None => panic!("no finished report after the transfer ended"),
        }
    }
    world.expect_arrival(held).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

// Step S4b: groups.

/// The core's key of a harness group (`group:gv2:<base64url group id>`).
fn group_conv(group: &support::harness::GroupInfo) -> String {
    use base64::Engine as _;
    let id = base64::engine::general_purpose::STANDARD
        .decode(&group.id)
        .expect("group id");
    format!(
        "group:gv2:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id)
    )
}

fn master_key_of(group: &support::harness::GroupInfo) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(&group.master_key)
        .expect("master key")
}

/// The upserts of `conv` in commit order.
fn upserts_of(world: &World, conv: &str) -> Vec<serde_json::Value> {
    world
        .sink
        .event_values("ConversationUpsert")
        .into_iter()
        .filter(|patch| patch["key"] == conv)
        .collect()
}

/// Waits until the merged upserts of `conv` satisfy `pred`.
async fn until_upserts(
    world: &World,
    conv: &str,
    what: &str,
    pred: impl Fn(&serde_json::Map<String, serde_json::Value>) -> bool,
) -> serde_json::Map<String, serde_json::Value> {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let merged = merged_upserts(world, conv);
        if pred(&merged) {
            return merged;
        }
        assert!(tokio::time::Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn contact_of(primary: &Primary, name: &str) -> ContactSpec {
    ContactSpec {
        aci: Some(primary.aci.parse().expect("ACI")),
        profile_name: format!("{MARKER} {name}"),
        profile_sharing: true,
        ..ContactSpec::default()
    }
}

/// Step S4b, mock tests 2, 3, 6 and 10: an imported group is upserted with the ARCHIVE's
/// title, members (admin, member, invited by PNI, requesting) and timer (the snapshot's
/// seconds), its features say "still loading" (not "no longer a member"), and its `groups`
/// row has no snapshot and one refresh. The refresh then fetches the SERVER's group: its
/// title, members and timer replace the archive's, no notice is made for the difference,
/// sending becomes possible, the archive's archived flag stays; and a member's live message
/// lands in the same conversation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_group_comes_from_the_archive_and_its_first_fetch_replaces_title_members_and_timer()
 {
    use kourier_signal::archive_testing::{GroupSpec, Party, SnapshotSpec, Who};
    use support::harness::{CreateGroup, SendGroupText};
    let Some(world) = World::start().await else {
        return;
    };
    let bo = world
        .harness
        .create_primary(&CreatePrimary::named("Bo"))
        .await
        .expect("Bo");
    let group = world
        .harness
        .create_group(&CreateGroup {
            from: world.phone.device_ref.clone(),
            title: "Server title".to_owned(),
            members: vec![world.alice.device_ref.clone(), bo.device_ref.clone()],
            timer: Some(600),
            ..CreateGroup::default()
        })
        .await
        .expect("createGroup");
    let conv = group_conv(&group);
    let invitee = Uuid::from_bytes([0x6b; 16]);
    let asking = Uuid::from_bytes([0x6c; 16]);
    let spec = HistorySpec {
        contacts: vec![contact_of(&world.alice, "Alice"), contact_of(&bo, "Bo")],
        groups: vec![GroupSpec {
            master_key: master_key_of(&group),
            whitelisted: true,
            blocked: false,
            snapshot: Some(SnapshotSpec {
                title: Some(format!("{MARKER} Archive title")),
                timer_secs: Some(3_600),
                members: vec![(Party::Own, true), (Party::Contact(0), false)],
                pending: vec![Party::Pni(invitee)],
                requesting: vec![Party::Aci(asking)],
                invite_link_password: MARKER.as_bytes().to_vec(),
            }),
        }],
        chats: vec![ChatSpec {
            with: ChatWith::Group(0),
            archived: true,
            texts: vec![
                TextSpec {
                    author: Some(Who::Contact(0)),
                    ..incoming(&format!("{MARKER} from Alice"), T0 + 1, true)
                },
                TextSpec {
                    author: Some(Who::Contact(1)),
                    ..incoming(&format!("{MARKER} from Bo"), T0 + 2, false)
                },
            ],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    };
    let fetches_before = world
        .harness
        .send_stats()
        .await
        .expect("sendStats")
        .group_fetches
        .get(&group.id)
        .copied()
        .unwrap_or(0);
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &spec);
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;

    // The import's own upsert: the archive's group.
    let first = upserts_of(&world, &conv)
        .into_iter()
        .find(|patch| !patch["features"].is_null())
        .expect("the import's upsert");
    assert_eq!(first["title"]["Set"], format!("{MARKER} Archive title"));
    assert_eq!(
        first["timer"]["Set"]["secs"], 3_600,
        "the snapshot's SECONDS"
    );
    assert_eq!(first["archived"], true);
    assert_eq!(first["features"]["cannot_send"], "StillLoading");
    let roles: Vec<(String, String)> = first["members"]["upsert"]
        .as_array()
        .expect("members")
        .iter()
        .map(|m| {
            (
                m["user"].as_str().unwrap_or_default().to_owned(),
                m["role"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let own = format!("aci:{}", linked.device.aci);
    assert_eq!(
        roles,
        [
            (own, "Admin".to_owned()),
            (format!("aci:{}", world.alice.aci), "Member".to_owned()),
            (
                format!("pni:{}", invitee.hyphenated()),
                "Pending".to_owned()
            ),
            (
                format!("aci:{}", asking.hyphenated()),
                "Requesting".to_owned()
            ),
        ]
    );
    // Both items are in the group.
    let in_group = world.sink.messages_in(&conv);
    assert!(
        in_group.iter().filter(|m| m.body.contains(MARKER)).count() == 2,
        "the group's two items"
    );

    // The refresh: the server's group replaces the archive's.
    let merged = until_upserts(&world, &conv, "the server's title", |m| {
        m.get("title").is_some_and(|t| t["Set"] == "Server title")
    })
    .await;
    assert_eq!(merged["timer"]["Set"]["secs"], 600);
    assert!(
        merged["features"]["cannot_send"].is_null(),
        "a member can send"
    );
    assert_eq!(merged["archived"], true, "the archive's flag stays");
    assert_eq!(
        merged["members"]["upsert"].as_array().map(Vec::len),
        Some(3)
    );
    assert!(
        !world
            .sink
            .messages_in(&conv)
            .iter()
            .any(|m| m.body.starts_with("<System>")),
        "a first fetch makes no notices for what changed since the export"
    );
    let fetches = world
        .harness
        .send_stats()
        .await
        .expect("sendStats")
        .group_fetches
        .get(&group.id)
        .copied()
        .unwrap_or(0);
    assert!(fetches > fetches_before, "the group was fetched");
    assert_eq!(
        world
            .files
            .peek_i64(
                "SELECT count(*) FROM groups WHERE state IS NOT NULL AND features_hash IS NOT NULL"
            )
            .expect("groups"),
        Some(1)
    );
    // The password of the invite link is nowhere in backend.db.
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT count(*) FROM groups WHERE instr(state, CAST('{MARKER}' AS BLOB)) > 0"
            ))
            .expect("groups"),
        Some(0)
    );
    world.expect_arrival(held).await;
    linked.core.wait_online().await;

    // A member writes into the imported group: the same conversation.
    let sent = world
        .harness
        .send_group_text(&SendGroupText {
            from: world.alice.aci.clone(),
            group: group.id.clone(),
            to: vec![linked.device.device_ref.clone()],
            text: "live in the group".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("sendGroupText");
    let message = world
        .sink
        .wait_message(
            &format!("aci:{}", world.alice.aci),
            &sent.timestamp.to_string(),
            WAIT,
        )
        .await
        .expect("the live group message");
    assert_eq!(message.conv, conv);
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    // Nothing of the group was logged: not the marker, not its title (the archive's or the
    // server's), not its members' names.
    let log = log_buffer()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let log = String::from_utf8_lossy(&log);
    assert!(
        log.contains("group state applied"),
        "the log was captured and holds the first fetch"
    );
    for secret in [
        MARKER,
        "Archive title",
        "Server title",
        "Alice",
        "\"Bo\"",
        "live in the group",
    ] {
        assert!(!log.contains(secret), "content of a group in the log");
    }
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Step S4b, mock tests 4, 5, 11, 12 and 13: a group the archive says we LEFT is read-only
/// and is refreshed once (D10); a group with an EMPTY snapshot comes without title and
/// members, then takes them from its first fetch; the group changes become the live path's
/// notices (the actor-less entry skipped); member items, a quote, a reaction; a missed
/// group call is a line of its group.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_left_group_is_refreshed_once_and_group_items_notices_and_calls_are_imported() {
    use kourier_signal::archive_testing::{
        CallSpec, GroupCallSpec, GroupChangeSpec, GroupSpec, ItemSpec, NoticeSpec, Party,
        QuoteSpec, ReactionSpec, SnapshotSpec, Who,
    };
    use support::harness::CreateGroup;
    let Some(world) = World::start().await else {
        return;
    };
    // A group without us on the server (Alice made it), and one with us whose state the
    // phone did not have.
    let left = world
        .harness
        .create_group(&CreateGroup {
            from: world.alice.device_ref.clone(),
            title: "Alice's group".to_owned(),
            ..CreateGroup::default()
        })
        .await
        .expect("createGroup");
    let empty = world
        .harness
        .create_group(&CreateGroup {
            from: world.phone.device_ref.clone(),
            title: "Fetched title".to_owned(),
            members: vec![world.alice.device_ref.clone()],
            ..CreateGroup::default()
        })
        .await
        .expect("createGroup");
    let (left_conv, empty_conv) = (group_conv(&left), group_conv(&empty));
    let notice = |entries| TextSpec {
        outgoing: true,
        kind: ItemSpec::Notice(NoticeSpec::GroupChange(entries)),
        sent_at: T0 + 10,
        ..TextSpec::default()
    };
    let spec = HistorySpec {
        contacts: vec![contact_of(&world.alice, "Alice")],
        groups: vec![
            GroupSpec {
                master_key: master_key_of(&left),
                snapshot: Some(SnapshotSpec {
                    title: Some(format!("{MARKER} Left")),
                    members: vec![(Party::Contact(0), true)],
                    ..SnapshotSpec::default()
                }),
                ..GroupSpec::default()
            },
            GroupSpec {
                master_key: master_key_of(&empty),
                snapshot: None,
                ..GroupSpec::default()
            },
        ],
        chats: vec![
            ChatSpec {
                with: ChatWith::Group(0),
                texts: vec![
                    TextSpec {
                        author: Some(Who::Contact(0)),
                        ..incoming(&format!("{MARKER} before we left"), T0 + 1, true)
                    },
                    TextSpec {
                        quote: Some(QuoteSpec {
                            target: Some(T0 + 1),
                            author: Some(Who::Contact(0)),
                            text: "before we left".to_owned(),
                            ..QuoteSpec::default()
                        }),
                        reactions: vec![ReactionSpec {
                            by: Who::Contact(0),
                            emoji: "\u{1f44d}".to_owned(),
                            sent_at: T0 + 3,
                            sort_order: 1,
                        }],
                        statuses_by: vec![(Who::Contact(0), SendStatusSpec::Read)],
                        ..outgoing(&format!("{MARKER} our reply"), T0 + 2, &[])
                    },
                    notice(vec![
                        GroupChangeSpec::Name {
                            by: Party::Contact(0),
                            name: Some("\u{2068}Новий\u{2069}".to_owned()),
                        },
                        GroupChangeSpec::Avatar { by: Party::Absent },
                        GroupChangeSpec::MemberRemoved {
                            by: Party::Contact(0),
                            removed: Party::Own,
                        },
                    ]),
                    TextSpec {
                        kind: ItemSpec::Call(CallSpec::Group {
                            call_id: Some(4_242),
                            state: GroupCallSpec::Missed,
                            ringer: Some(Who::Contact(0)),
                            started_by: None,
                            read: false,
                            started_at: T0 + 20,
                        }),
                        sent_at: T0 + 20,
                        ..TextSpec::default()
                    },
                ],
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::Group(1),
                texts: vec![TextSpec {
                    author: Some(Who::Contact(0)),
                    ..incoming(
                        &format!("{MARKER} in a group the phone had no state of"),
                        T0 + 30,
                        true,
                    )
                }],
                ..ChatSpec::default()
            },
        ],
        ..HistorySpec::default()
    };
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let (path, _) = world.archive(&linked, &spec);
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;

    // Left: read-only from the start, membership 'left', and its one refresh ran: the server
    // was asked for the group and the job is gone. (Not `fetched_at`: the mock answers 200
    // to a non-member where the real server answers 403, which stores nothing for a row the
    // import wrote 'left'; so the fetch is counted on the server's side.)
    let first = upserts_of(&world, &left_conv)
        .into_iter()
        .find(|patch| !patch["features"].is_null())
        .expect("the import's upsert");
    assert_eq!(first["features"]["cannot_send"], "NotMember");
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let fetches = world
            .harness
            .send_stats()
            .await
            .expect("sendStats")
            .group_fetches
            .get(&left.id)
            .copied()
            .unwrap_or(0);
        if fetches > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the left group was never fetched"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    world
        .files
        .wait_for_i64(
            "the left group's refresh job is gone",
            "SELECT count(*) FROM groups g WHERE g.membership = 'left'
               AND g.features_hash IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM jobs j
                               WHERE j.kind = 'group_refresh' AND j.key = lower(hex(g.group_id)))",
            |n| n >= Some(1),
        )
        .await
        .expect("refresh of the left group");
    // Empty snapshot: no title and no members from the import, then the server's.
    let first = upserts_of(&world, &empty_conv)
        .into_iter()
        .find(|patch| !patch["features"].is_null())
        .expect("the import's upsert");
    assert!(
        first["title"].is_null() && first["members"].is_null(),
        "{first}"
    );
    assert_eq!(first["features"]["cannot_send"], "StillLoading");
    until_upserts(&world, &empty_conv, "the fetched title", |m| {
        m.get("title").is_some_and(|t| t["Set"] == "Fetched title")
    })
    .await;
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'group_refresh'")
            .expect("jobs"),
        Some(0),
        "every refresh done, none left"
    );

    // The rows of the left group.
    let rows = world.sink.messages_in(&left_conv);
    let alice = format!("aci:{}", world.alice.aci);
    assert!(
        rows.iter()
            .any(|m| m.author == alice && m.body.contains("before we left"))
    );
    let notices: Vec<serde_json::Value> = rows
        .iter()
        .filter_map(|m| m.body.strip_prefix("<System>"))
        .map(|json| serde_json::from_str(json).expect("event"))
        .filter(|event: &serde_json::Value| event.get("Call").is_none())
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert_eq!(
        notices[0]["TitleChanged"]["title"], "Новий",
        "one isolate pair off"
    );
    assert_eq!(notices[0]["TitleChanged"]["by"], alice.as_str());
    assert_eq!(notices[1]["MembersRemoved"]["by"], alice.as_str());
    let reply = rows
        .iter()
        .find(|m| m.body.contains("our reply"))
        .expect("our reply");
    let json = world.sink.message_values(&reply.author, &reply.local);
    let json = json.first().expect("its json");
    assert_eq!(json["reply_to"]["target"]["author"]["User"], alice.as_str());
    assert_eq!(json["reactions"][0]["by"], alice.as_str());
    assert_eq!(json["initial_out_state"], "Read");
    assert!(
        rows.iter()
            .any(|m| m.local == "gcall:4242" && m.body.contains("Missed")),
        "the missed group call is a line of its group"
    );
    world.expect_arrival(held).await;
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Step S4b, mock test 1: with GROUPS in the archive, nothing of them reaches `backend.db`
/// or the core before the commit: while the archive is read and at the commit zone there is
/// no `groups` row, no refresh job, no row for the core and no event, while the stage holds
/// the group, its members and its notices; the delivery then makes the row and its refresh.
/// The process is killed right after the commit zone, so the files are looked at exactly as
/// the commit left them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_of_the_archives_groups_reaches_backend_db_before_the_commit() {
    use kourier_signal::archive_testing::{GroupSpec, Party, SnapshotSpec, Who};
    let Some(world) = World::start().await else {
        return;
    };
    // The 1:1 history, and a group on top of it: members by ACI and by PNI, an invitation,
    // a request, a title and a password that must not be kept, items by two members and
    // a change of the name.
    let mut spec = history(900);
    let group_key = vec![0x5a; 32];
    spec.groups = vec![GroupSpec {
        master_key: group_key.clone(),
        whitelisted: true,
        blocked: false,
        snapshot: Some(SnapshotSpec {
            title: Some(format!("{MARKER} Hikers")),
            timer_secs: Some(3_600),
            members: vec![
                (Party::Own, false),
                (Party::Contact(0), true),
                (Party::Contact(2), false),
            ],
            pending: vec![Party::Pni(Uuid::from_bytes([0x6b; 16]))],
            requesting: vec![Party::Aci(Uuid::from_bytes([0x6c; 16]))],
            invite_link_password: b"group-password-5d1c".to_vec(),
        }),
    }];
    spec.chats.push(ChatSpec {
        with: ChatWith::Group(0),
        pinned_order: Some(1),
        texts: vec![
            TextSpec {
                author: Some(Who::Contact(0)),
                ..incoming(&format!("{MARKER} from Ann in the group"), T0 + 40, true)
            },
            TextSpec {
                author: Some(Who::Contact(2)),
                ..incoming(&format!("{MARKER} from Cat in the group"), T0 + 41, false)
            },
            TextSpec {
                outgoing: true,
                kind: kourier_signal::archive_testing::ItemSpec::Notice(
                    kourier_signal::archive_testing::NoticeSpec::GroupChange(vec![
                        kourier_signal::archive_testing::GroupChangeSpec::Name {
                            by: Party::Contact(0),
                            name: Some(format!("{MARKER} Renamed")),
                        },
                    ]),
                ),
                sent_at: T0 + 42,
                ..TextSpec::default()
            },
        ],
        ..ChatSpec::default()
    });
    let (linked, frames, held) = linked_with_an_archive(&world, &spec).await;
    let Linked { core, device, key } = linked;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    let mut child = spawn_child(
        &world,
        8,
        Some(kourier_signal::failpoints::AFTER_HISTORY_IMPORT_COMMIT),
    );
    world.wait_reading_into_the_stage().await;
    let before = world.settled().await;
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 0"),
        Some(1)
    );
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    let nothing_of_the_groups = |when: &str| {
        assert_eq!(int("SELECT count(*) FROM groups"), 0, "{when}: a group row");
        assert_eq!(
            int("SELECT count(*) FROM jobs WHERE kind = 'group_refresh'"),
            0,
            "{when}: a refresh of a group"
        );
        for needle in [MARKER, "group-password-5d1c"] {
            assert_eq!(
                int(&format!(
                    "SELECT count(*) FROM core_outbox
                      WHERE instr(envelope, CAST('{needle}' AS BLOB)) > 0"
                )),
                0,
                "{when}: a row for the core holds a text of the group"
            );
        }
        assert!(
            world
                .sink
                .event_values("ConversationUpsert")
                .iter()
                .all(|patch| !patch["key"].as_str().unwrap_or("").starts_with("group:")),
            "{when}: an upsert of a group reached the core"
        );
    };
    nothing_of_the_groups("while the archive is read");

    wait_aborted(&mut child).await;
    // The commit recorded the stage; the stage holds the group, and still nothing else
    // changed, strictly.
    assert_eq!(
        world.row("state = 'reading' AND import_staged = 1 AND read_open = 0"),
        Some(1),
        "the commit zone recorded the stage"
    );
    let dump = read_stage(&world.transfer_dir(), &key).expect("the committed stage");
    assert_eq!(dump.frames, frames);
    let staged = dump
        .chats
        .iter()
        .find_map(|chat| chat.group.as_ref().map(|group| (chat, group)))
        .expect("the group is in the stage");
    assert_eq!(staged.1.membership, "member");
    assert_eq!(
        staged.1.title.as_deref(),
        Some(&*format!("{MARKER} Hikers"))
    );
    assert_eq!(staged.1.members.as_ref().map(Vec::len), Some(5));
    assert_eq!(staged.0.messages.len(), 3, "two texts and the notice");
    assert_eq!(world.untouched(), before, "the import touched something");
    nothing_of_the_groups("at the commit");
    let _ = device;

    // The delivery then makes the group's row, its features and one refresh; the server
    // does not know this made-up group, so the refresh is a 404 and the row is 'gone'.
    let mut core = world.restart(network());
    core.send(Command::Connect).await;
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_over("done", None);
    assert_eq!(int("SELECT count(*) FROM groups"), 1);
    assert_eq!(
        int("SELECT count(*) FROM groups WHERE features_hash IS NOT NULL AND state IS NULL"),
        1
    );
    world.expect_arrival(held).await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Step S4b, mock test 9 for groups: the Storage Service sync that follows the transfer
/// wins over the archive's flags of a group where they differ (archived, marked unread,
/// muted, blocked), and makes no event for a group where they agree.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_storage_sync_wins_over_the_archives_group_flags_and_is_silent_where_they_agree() {
    use kourier_signal::archive_testing::{GroupSpec, Party, SnapshotSpec, Who};
    use support::harness::{CreateGroup, StorageGroup};
    let Some(world) = World::start().await else {
        return;
    };
    let mut groups = Vec::new();
    for title in ["Differs", "Agrees"] {
        groups.push(
            world
                .harness
                .create_group(&CreateGroup {
                    from: world.phone.device_ref.clone(),
                    title: title.to_owned(),
                    members: vec![world.alice.device_ref.clone()],
                    ..CreateGroup::default()
                })
                .await
                .expect("createGroup"),
        );
    }
    // The phone's Storage Service: the first group is not archived, not unread, not muted;
    // the second is archived, as the archive says.
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: world.phone.device_ref.clone(),
            contacts: vec![StorageContact::of(&world.alice.device_ref)],
            groups: vec![
                StorageGroup {
                    id: groups[0].id.clone(),
                    archived: Some(false),
                    marked_unread: Some(false),
                    blocked: Some(false),
                    ..StorageGroup::default()
                },
                StorageGroup {
                    id: groups[1].id.clone(),
                    archived: Some(true),
                    marked_unread: Some(false),
                    blocked: Some(false),
                    ..StorageGroup::default()
                },
            ],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    let snapshot = || {
        Some(SnapshotSpec {
            title: Some(format!("{MARKER} archive")),
            members: vec![(Party::Own, false), (Party::Contact(0), false)],
            ..SnapshotSpec::default()
        })
    };
    let group_chat = |index: usize, archived: bool, differs: bool| ChatSpec {
        with: ChatWith::Group(index),
        archived,
        marked_unread: differs,
        mute_until_ms: differs.then_some(i64::MAX as u64),
        texts: vec![TextSpec {
            author: Some(Who::Contact(0)),
            ..incoming(
                &format!("{MARKER} in group {index}"),
                T0 + 1 + index as u64,
                true,
            )
        }],
        ..ChatSpec::default()
    };
    let spec = HistorySpec {
        contacts: vec![contact_of(&world.alice, "Alice")],
        groups: groups
            .iter()
            .map(|group| GroupSpec {
                master_key: master_key_of(group),
                whitelisted: true,
                blocked: false,
                snapshot: snapshot(),
            })
            .collect(),
        chats: vec![group_chat(0, true, true), group_chat(1, true, false)],
        ..HistorySpec::default()
    };
    let (mut linked, _, held) = linked_with_an_archive(&world, &spec).await;
    let (differs, agrees) = (group_conv(&groups[0]), group_conv(&groups[1]));
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_arrival(held).await;
    // The import said yes ...
    let first = upserts_of(&world, &differs)
        .into_iter()
        .find(|patch| !patch["features"].is_null())
        .expect("the import's upsert");
    assert_eq!(first["archived"], true);
    assert_eq!(first["marked_unread"], true);
    assert_eq!(first["muted_until"]["Set"], i64::MAX);
    // ... and the first sync, which had no previous record, says no and wins.
    let merged = until_upserts(&world, &differs, "the sync's flags", |m| {
        m.get("archived") == Some(&serde_json::json!(false))
    })
    .await;
    assert_eq!(merged["marked_unread"], false);
    assert_eq!(merged["muted_until"], "Clear");
    assert_eq!(merged["blocked"], false);
    world
        .files
        .wait_for_i64(
            "the first Storage Service sync",
            "SELECT count(*) FROM account WHERE manifest_version IS NOT NULL",
            |n| n == Some(1),
        )
        .await;
    // Where the record agrees with the row the import wrote, the sync says nothing: an
    // upsert that carries flags and neither a title nor features is the sync's.
    let by_the_sync: Vec<serde_json::Value> = upserts_of(&world, &agrees)
        .into_iter()
        .filter(|patch| {
            !patch["archived"].is_null() && patch["title"].is_null() && patch["features"].is_null()
        })
        .collect();
    assert!(by_the_sync.is_empty(), "{by_the_sync:?}");
    let sync_of_the_first = upserts_of(&world, &differs)
        .into_iter()
        .filter(|patch| patch["title"].is_null() && patch["features"].is_null())
        .count();
    assert_eq!(sync_of_the_first, 1, "one upsert of the flags that differ");
    // The backend's rows say what the sync said.
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM groups WHERE archived = 1 AND marked_unread = 0")
            .expect("groups"),
        Some(1),
        "only the agreeing group stays archived"
    );
    linked.core.wait_online().await;
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

// ---- Step S5: attachment pointers -------------------------------------------------------

/// This computer's clock (ms).
fn wall_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_millis(),
    )
    .expect("ms")
}

fn attachments_of(msg: &kourier_backend_api::IncomingMessage) -> Vec<kourier_model::Attachment> {
    match &msg.body {
        kourier_model::Body::Media { attachments, .. } => attachments.clone(),
        kourier_model::Body::Sticker(image) => vec![(**image).clone()],
        other => panic!("attachments: {other:?}"),
    }
}

/// D8 with the user's decisions (step S5): an imported picture whose place on the CDN is
/// young is delivered `Remote` with its pointer, and the import asks the CDN for NOTHING of
/// it (nor of any other attachment): only the archive itself is downloaded. A click (the
/// core's `DownloadAttachment`) makes exactly one request and the blob decrypts under the
/// plaintext hash. One the CDN no longer has becomes "no longer available" on the click
/// (404 → `Expired`); one older than the age rule is "not available on this device" at
/// once, without a pointer, and is never asked for. Nothing of a pointer is logged.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_imported_picture_downloads_on_a_click_and_the_import_asks_the_cdn_nothing() {
    use kourier_signal::archive_testing::{
        AttachmentSpec, LocatorSpec, PointerSpec, encrypt_attachment,
    };
    let Some(world) = World::start().await else {
        return;
    };
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    let now = wall_ms();
    let hour = 3_600_000;
    let key = [0x5a; 64];
    let plain: Vec<u8> = (0..30_000u32).map(|i| (i % 253) as u8).collect();
    let young = encrypt_attachment(&plain, &key, [1; 16]).expect("encrypt");
    // CDN keys as the server makes them (not route words: the request log replaces them).
    let cdn3 = world
        .harness
        .handshake()
        .state_dir
        .join("cdn3")
        .join("attachments");
    std::fs::create_dir_all(&cdn3).expect("cdn3");
    std::fs::write(cdn3.join("S5YoungKey"), &young.blob).expect("store");
    std::fs::write(cdn3.join("S5OldKey"), &young.blob).expect("store");
    let picture = |cdn_key: &str, upload: Option<u64>| AttachmentSpec {
        pointer: Some(PointerSpec {
            content_type: Some("image/png".to_owned()),
            locator: Some(LocatorSpec::of(&young, &key, 3, cdn_key, upload, true)),
            ..PointerSpec::default()
        }),
        flag: 0,
        client_uuid: Some([0x77; 16]),
    };
    let with = |sent_at: u64, attachment: AttachmentSpec| TextSpec {
        attachments: vec![attachment],
        ..incoming("", sent_at, true)
    };
    let spec = HistorySpec {
        contacts: history(0).contacts,
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                with(now - 2 * hour, picture("S5YoungKey", Some(now - 2 * hour))),
                with(now - 2 * hour + 1, picture("S5GoneKey", None)),
                // Two hundred days old: older than twice the 30-day floor (the mock's config
                // has no queue time).
                with(now - 200 * 24 * hour, picture("S5OldKey", None)),
            ],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    };
    let (path, _) = world.archive(&linked, &spec);
    let place = world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_arrival(held).await;

    let by_time = |at: u64| -> kourier_backend_api::IncomingMessage {
        let json = world.sink.message_values(&user(ANN), &at.to_string());
        serde_json::from_value(
            json.last()
                .cloned()
                .unwrap_or_else(|| panic!("the message of {at}")),
        )
        .expect("an IncomingMessage")
    };
    let [young_att] = &attachments_of(&by_time(now - 2 * hour))[..] else {
        panic!("one attachment");
    };
    assert_eq!(young_att.state, kourier_model::AttachmentState::Remote);
    let remote = young_att.remote.clone().expect("a pointer");
    assert_eq!(remote.v, 3, "the archive's pointer");
    assert!(
        young_att
            .net_ids
            .contains(&format!("uuid:{}", "77".repeat(16))),
        "named by its client UUID"
    );
    let [gone_att] = &attachments_of(&by_time(now - 2 * hour + 1))[..] else {
        panic!("one attachment");
    };
    assert_eq!(gone_att.state, kourier_model::AttachmentState::Remote);
    let [old_att] = &attachments_of(&by_time(now - 200 * 24 * hour))[..] else {
        panic!("one attachment");
    };
    assert_eq!(old_att.state, kourier_model::AttachmentState::Unavailable);
    assert_eq!(
        old_att.remote, None,
        "no key is kept for an object that is not there"
    );
    assert!(
        !old_att.net_ids.is_empty(),
        "its names stay for a later ask of the phone"
    );

    // The import asked the CDN for the archive and for nothing else.
    let asked = |stats: &[support::harness::DownloadStat]| -> Vec<String> {
        stats
            .iter()
            .filter(|s| s.key != place.key)
            .map(|s| s.key.clone())
            .collect()
    };
    let downloads = world.harness.downloads().await.expect("downloadStats");
    assert!(asked(&downloads).is_empty(), "{:?}", asked(&downloads));

    // A click: one request, the blob decrypts under the plaintext hash.
    let blobs = world.files.dir.join("blobs");
    let target = |name: &str| kourier_backend_api::BlobTarget {
        path: blobs.join("core").join("s5").join(name),
        key: kourier_model::BlobKey::new([0x42; 32]),
    };
    linked
        .core
        .send(Command::DownloadAttachment {
            attachment: kourier_model::AttachmentId(1),
            request: kourier_model::DownloadId(1),
            remote,
            dest: target("young"),
            net_ids: Vec::new().into(),
        })
        .await;
    linked
        .core
        .send(Command::DownloadAttachment {
            attachment: kourier_model::AttachmentId(2),
            request: kourier_model::DownloadId(2),
            remote: gone_att.remote.clone().expect("a pointer"),
            dest: target("gone"),
            net_ids: Vec::new().into(),
        })
        .await;
    world.sink.wait_events("AttachmentDone", 2, WAIT).await;
    let results = world.sink.event_values("AttachmentDone");
    let result = |id: i64| {
        results
            .iter()
            .find(|r| r["attachment"] == id)
            .unwrap_or_else(|| panic!("AttachmentDone {id}: {results:?}"))["result"]
            .clone()
    };
    assert_eq!(result(1)["Ok"]["size"], 30_000, "{}", result(1));
    let got = kourier_blob::read_all(
        &target("young").path,
        &kourier_blob::FileKey::from_bytes([0x42; 32]),
    )
    .expect("the downloaded blob");
    assert_eq!(&got[..], &plain[..]);
    assert_eq!(
        result(2)["Err"]["kind"],
        "Expired",
        "gone from the CDN: no longer available ({})",
        result(2)
    );
    assert!(
        !target("gone").path.exists(),
        "nothing is left of a failed download"
    );
    let downloads = world.harness.downloads().await.expect("downloadStats");
    let mut asked = asked(&downloads);
    asked.sort();
    assert_eq!(
        asked,
        ["S5GoneKey", "S5YoungKey"],
        "one request per click, the old one never"
    );

    // Nothing of a pointer reaches the log.
    let log = String::from_utf8_lossy(&log_buffer().lock().expect("log")).into_owned();
    for secret in ["S5YoungKey", "S5GoneKey", "S5OldKey", &"5a".repeat(8)] {
        let line = log.lines().find(|l| l.contains(secret));
        assert!(line.is_none(), "the log names {secret}: {line:?}");
    }
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
}

/// The age window and the receive limit come from the remote config `backend.db` holds when
/// the read begins: a message queue time of 60 days (a window of twice that) makes a picture
/// of 100 days a download (the 30-day floor would not), one of 250 days is still not
/// available, and a picture above the server's receive limit is not available either (no dead
/// "Retry Download"). The age counts back from the PHONE's export time, bounded to a day before
/// the import: the test writer's export time is in 2023, so the reference is the import's
/// time minus a day, and a picture half a day inside the window by that reference is a
/// download although it is half a day outside it by this computer's clock (and one half a
/// day outside it is not).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_servers_queue_time_and_receive_limit_decide_at_the_import() {
    use kourier_signal::archive_testing::{
        AttachmentSpec, LocatorSpec, PointerSpec, encrypt_attachment,
    };
    let Some(world) = World::start().await else {
        return;
    };
    let day: u64 = 24 * 3_600_000;
    world
        .harness
        .set_remote_config(
            "global.messageQueueTimeInSeconds",
            true,
            Some(&(60 * 86_400).to_string()),
        )
        .await
        .expect("queue time");
    world
        .harness
        .set_remote_config("global.attachments.maxReceiveBytes", true, Some("25000"))
        .await
        .expect("receive limit");
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    world
        .files
        .wait_for_i64(
            "the remote config is stored",
            "SELECT count(*) FROM remote_config
              WHERE name IN ('global.messageQueueTimeInSeconds',
                             'global.attachments.maxReceiveBytes')",
            |v| v == Some(2),
        )
        .await;
    let now = wall_ms();
    let key = [0x2b; 64];
    let small = encrypt_attachment(&[7; 20_000], &key, [1; 16]).expect("encrypt");
    let large = encrypt_attachment(&[7; 30_000], &key, [1; 16]).expect("encrypt");
    let picture = |encrypted, cdn_key: &str| AttachmentSpec {
        pointer: Some(PointerSpec {
            content_type: Some("image/png".to_owned()),
            locator: Some(LocatorSpec::of(encrypted, &key, 3, cdn_key, None, true)),
            ..PointerSpec::default()
        }),
        flag: 0,
        client_uuid: None,
    };
    let with = |sent_at: u64, attachment: AttachmentSpec| TextSpec {
        attachments: vec![attachment],
        ..incoming("", sent_at, true)
    };
    let (days100, days250, large_at) = (now - 100 * day, now - 250 * day, now - day);
    // The window's edge by the reference (the import's time minus a day): 121 days back.
    let (inside, outside) = (now - 121 * day + day / 2, now - 121 * day - day / 2);
    let spec = HistorySpec {
        contacts: history(0).contacts,
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                with(days100, picture(&small, "S5Days100")),
                with(days250, picture(&small, "S5Days250")),
                with(large_at, picture(&large, "S5Large")),
                with(inside, picture(&small, "S5Inside")),
                with(outside, picture(&small, "S5Outside")),
            ],
            ..ChatSpec::default()
        }],
        ..HistorySpec::default()
    };
    let (path, _) = world.archive(&linked, &spec);
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_arrival(held).await;
    let state = |at: u64| {
        let json = world.sink.message_values(&user(ANN), &at.to_string());
        let msg: kourier_backend_api::IncomingMessage =
            serde_json::from_value(json.last().cloned().expect("the message")).expect("message");
        attachments_of(&msg)[0].state
    };
    use kourier_model::AttachmentState::{Remote, Unavailable};
    assert_eq!(
        [state(days100), state(days250), state(large_at)],
        [Remote, Unavailable, Unavailable]
    );
    assert_eq!(
        [state(inside), state(outside)],
        [Remote, Unavailable],
        "the age counts from the phone's export time, a day before the import at most"
    );
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
}

/// The reference of the age rule IS the phone's export time when it is within a day of the
/// import (not the day-before bound the writer's default time falls to): with the export
/// twelve hours before the import, a picture an hour inside the window by it is a download,
/// and one an hour outside it is "not available" although it is half a day INSIDE the window
/// by the day-before bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_age_window_counts_from_the_export_time_when_it_is_within_a_day_of_the_import() {
    use kourier_signal::archive_testing::{
        AttachmentSpec, LocatorSpec, PointerSpec, encrypt_attachment,
    };
    let Some(world) = World::start().await else {
        return;
    };
    let (hour, day): (u64, u64) = (3_600_000, 24 * 3_600_000);
    world
        .harness
        .set_remote_config(
            "global.messageQueueTimeInSeconds",
            true,
            Some(&(60 * 86_400).to_string()),
        )
        .await
        .expect("queue time");
    let mut linked = world.link(network()).await;
    let held = world
        .alice_says(&linked.device, "waits for the import")
        .await;
    world
        .files
        .wait_for_i64(
            "the remote config is stored",
            "SELECT count(*) FROM remote_config
              WHERE name = 'global.messageQueueTimeInSeconds'",
            |v| v == Some(1),
        )
        .await;
    let now = wall_ms();
    let export = now - 12 * hour;
    let key = [0x2b; 64];
    let small = encrypt_attachment(&[7; 20_000], &key, [1; 16]).expect("encrypt");
    let picture = |cdn_key: &str| AttachmentSpec {
        pointer: Some(PointerSpec {
            content_type: Some("image/png".to_owned()),
            locator: Some(LocatorSpec::of(&small, &key, 3, cdn_key, None, true)),
            ..PointerSpec::default()
        }),
        flag: 0,
        client_uuid: None,
    };
    let with = |sent_at: u64, attachment: AttachmentSpec| TextSpec {
        attachments: vec![attachment],
        ..incoming("", sent_at, true)
    };
    // The window is 120 days. Its edge by the export time is 120 days before it; by the
    // day-before bound it would be 12 hours earlier still.
    let edge = export - 120 * day;
    let (inside, outside) = (edge + hour, edge - hour);
    let spec = HistorySpec {
        contacts: history(0).contacts,
        chats: vec![ChatSpec {
            with: ChatWith::Contact(0),
            texts: vec![
                with(inside, picture("S5ExportInside")),
                with(outside, picture("S5ExportOutside")),
            ],
            ..ChatSpec::default()
        }],
        export_time_ms: Some(export),
        ..HistorySpec::default()
    };
    let (path, _) = world.archive(&linked, &spec);
    world.upload(&linked.device, &path).await;
    until(&mut linked.core, "Done", |s| *s == HistoryTransfer::Done).await;
    world.expect_arrival(held).await;
    let state = |at: u64| {
        let json = world.sink.message_values(&user(ANN), &at.to_string());
        let msg: kourier_backend_api::IncomingMessage =
            serde_json::from_value(json.last().cloned().expect("the message")).expect("message");
        attachments_of(&msg)[0].state
    };
    use kourier_model::AttachmentState::{Remote, Unavailable};
    assert_eq!(
        [state(inside), state(outside)],
        [Remote, Unavailable],
        "the reference is the export time, twelve hours before the import"
    );
    assert_eq!(linked.core.shutdown().await, Exit::Shutdown);
}

/// Link feedback L2 (`target/link-feedback/tail.md` §4.1, §4.3): after an imported archive the
/// login's `Done` waits neither for the first Storage Service sync (the archive named every
/// chat; Desktop does the same) nor for the profile fetches the link queued. With every
/// manifest request failing and every profile fetch rate limited for ten minutes, `Done`
/// (`InitialSync::ContactsAndStorage`, the app's) comes once the transfer and the contact
/// sync are done, before the storage sync gave up (3 failed attempts would also let it
/// pass), with the profile fetches still queued; the sync runs after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_an_import_the_link_is_done_without_the_storage_sync_and_the_profile_fetches() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .set_storage_state(&StorageSpec {
            primary: world.phone.device_ref.clone(),
            contacts: vec![StorageContact::of(&world.alice.device_ref)],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    world
        .harness
        .fail_storage_manifest(true)
        .await
        .expect("failStorageManifest");
    world
        .harness
        .limit_profile_fetches(Some(600))
        .await
        .expect("limitProfileFetches");
    let linked = world
        .link_with(network(), InitialSync::ContactsAndStorage)
        .await;
    // Ann's profile key in the archive queues a fetch of her profile.
    let mut spec = history(0);
    spec.contacts[0].profile_key = Some([7; 32]);
    let (path, _) = world.archive(&linked, &spec);
    world.upload(&linked.device, &path).await;
    let mut core = linked.core;
    until(&mut core, "Done", |s| *s == HistoryTransfer::Done).await;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match core.next_ephemeral(left).await {
            Some(BackendEvent::Login(step)) => match step.kind {
                LoginStepKind::Done => break,
                LoginStepKind::Failed { error } => panic!("login failed: {error:?}"),
                _ => {}
            },
            Some(_) => {}
            None => panic!("no login Done after the transfer"),
        }
    }
    let refused = world
        .harness
        .fail_storage_manifest(true)
        .await
        .expect("failStorageManifest");
    let int = |sql: &str| world.files.peek_i64(sql).expect("backend.db").unwrap_or(0);
    let storage_attempts = int("SELECT max(attempts) FROM jobs WHERE kind = 'storage_sync'");
    let profiles = int("SELECT count(*) FROM jobs WHERE kind = 'profile_fetch'");
    println!(
        "at Done: {refused} manifest requests refused, storage sync attempts \
         {storage_attempts}, {profiles} profile fetches queued"
    );
    assert!(
        refused < 3 && storage_attempts < 3,
        "Done did not wait for the storage sync to give up ({refused} refused, \
         {storage_attempts} attempts)"
    );
    assert_eq!(
        int("SELECT count(*) FROM jobs WHERE kind = 'storage_sync'"),
        1,
        "the storage sync is still to run"
    );
    assert_eq!(
        int("SELECT contact_sync_done FROM account WHERE id = 1"),
        1,
        "the contact sync was applied before Done"
    );
    assert!(profiles >= 1, "the profile fetches are still queued");
    assert!(
        world
            .harness
            .limit_profile_fetches(Some(600))
            .await
            .expect("limitProfileFetches")
            >= 1,
        "a profile fetch met the rate limit"
    );

    // Served again: the storage sync runs after Done.
    world
        .harness
        .fail_storage_manifest(false)
        .await
        .expect("failStorageManifest");
    world
        .harness
        .limit_profile_fetches(None)
        .await
        .expect("limitProfileFetches");
    world
        .files
        .wait_for_i64(
            "the first Storage Service sync after Done",
            "SELECT count(*) FROM account WHERE manifest_version IS NOT NULL",
            |n| n == Some(1),
        )
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}
