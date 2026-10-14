//! An attack on the history transfer's promise that its key is gone from `backend.db` the
//! moment the transfer is over (`transfer.rs` "Secrets", migration 0016: "NULL from the
//! moment the transfer is over"), written by the reviewer of that step and switched on with
//! its fix.
//!
//! `backend.db` runs in WAL mode. The write that ends a transfer sets `key`, `cdn` and
//! `cdn_key` to NULL in a NEW version of the row's page, appended to `backend.db-wal`; the
//! versions of that page that hold the key stay in the two files until a checkpoint
//! overwrites them, and `secure_delete` does not reach them. On the tree the attack was
//! written for nothing checkpointed at the end of a transfer, and 7 of 32 committed states
//! of the WAL held the key. Now the end is followed by `wal_checkpoint(TRUNCATE)`
//! (`transfer::scrub`), a start that finds a transfer over does the same (the process was
//! killed between the end and its checkpoint), and a logout during a transfer deletes the
//! row and truncates the WAL before the actor ends.
//!
//! The files are looked at in two ways. Through SQLite: every prefix of the WAL that ends
//! in a commit is a database state a reader can open, and none may hold the key in the
//! transfer's row. And below SQLite: every page of `backend.db` and every frame of the WAL
//! is decrypted with the database key (SQLCipher 4: AES-256-CBC per page, the IV in the
//! page's reserve) and searched for the bytes of the ephemeral key, which also finds what
//! a freed cell or a freed page would still hold. A control proves each look finds the key
//! while the transfer waits.
//!
//! Nothing of the key is printed: only counts.
//!
//! Skips with a message when Node ≥ 24 or `tests/mock-harness/node_modules` is missing.

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use aes::Aes256Dec;
use aes::cipher::{BlockCipherDecrypt as _, KeyInit as _};
use kourier_backend_api::{BackendEnv, BackendEvent, Command, Exit, StartMode};
use kourier_model::HistoryTransfer;
use kourier_signal::SignalNetwork;
use serde::{Deserialize, Serialize};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env};
use support::harness::{CreatePrimary, DeviceInfo, Harness, Primary};

/// The byte lengths of every prefix of the WAL at `wal` that ends in a commit: each is a
/// database state a reader of the two files can open (the checksums of a WAL are cumulative,
/// so a prefix that ends at a commit frame is itself a valid WAL).
fn commit_prefixes(wal: &[u8]) -> Vec<usize> {
    const HEADER: usize = 32;
    const FRAME_HEADER: usize = 24;
    if wal.len() < HEADER {
        return Vec::new();
    }
    let be = |at: usize| u32::from_be_bytes(wal[at..at + 4].try_into().expect("four bytes"));
    let page = be(8) as usize;
    let salts = (be(16), be(20));
    let mut ends = Vec::new();
    let mut at = HEADER;
    while page > 0 && at + FRAME_HEADER + page <= wal.len() {
        // A frame of an earlier generation of the file (other salts) ends the valid part.
        if (be(at + 8), be(at + 12)) != salts {
            break;
        }
        let commit = be(at + 4) != 0;
        at += FRAME_HEADER + page;
        if commit {
            ends.push(at);
        }
    }
    ends
}

/// How many of the database states in `db` + `wal` hold `key_hex` in the transfer's row.
/// Nothing of the key is printed: the comparison happens in SQL.
fn states_with_the_key(scratch: &Path, db: &[u8], wal: &[u8], dbkey: [u8; 32], hex: &str) -> usize {
    let mut found = 0;
    for (i, end) in commit_prefixes(wal).into_iter().enumerate() {
        let dir = scratch.join(format!("state-{i}"));
        std::fs::create_dir_all(&dir).expect("scratch");
        let path = dir.join("backend.db");
        std::fs::write(&path, db).expect("copy of backend.db");
        std::fs::write(dir.join("backend.db-wal"), &wal[..end]).expect("copy of the WAL");
        let Ok(conn) = kourier_db::open(&path, &kourier_db::DbKey::from(dbkey)) else {
            continue;
        };
        // An early state has no such table yet: that is "no key".
        let has: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM history_transfer WHERE hex(key) = '{hex}'"),
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has > 0 {
            found += 1;
        }
    }
    found
}

/// SQLCipher 4's page layout, as `kourier_db::open` uses it (a raw key, the defaults): pages
/// of 4096 bytes whose last 80 are the reserve, which starts with the page's IV; the first
/// 16 bytes of page 1 are the salt and are not encrypted.
const PAGE: usize = 4096;
const RESERVE: usize = 80;
const SALT: usize = 16;

/// Decrypts one page image (`number` is 1 for the page that starts with the salt): the
/// plaintext of everything in front of the reserve.
fn decrypt_page(dbkey: &[u8; 32], page: &[u8], number: u32) -> Vec<u8> {
    let cipher = Aes256Dec::new(dbkey.into());
    let body_end = PAGE - RESERVE;
    let start = if number == 1 { SALT } else { 0 };
    let mut previous: [u8; 16] = page[body_end..body_end + 16].try_into().expect("the IV");
    let mut plain = Vec::with_capacity(body_end - start);
    for block in page[start..body_end].as_chunks::<16>().0 {
        let mut out = *block;
        cipher.decrypt_block((&mut out).into());
        plain.extend(out.iter().zip(previous).map(|(a, b)| a ^ b));
        previous = *block;
    }
    plain
}

/// What a look below SQLite found in the two files.
#[derive(Debug, Default, PartialEq, Eq)]
struct RawLook {
    /// Pages of `backend.db` and frames of the WAL that were decrypted.
    pages: usize,
    /// How many of them hold the needle.
    with_needle: usize,
    /// How many hold the text `history_transfer` (the table's definition in the schema): the
    /// proof that the decryption gives plaintext.
    with_schema: usize,
}

/// Decrypts every page of `db` and every frame of `wal`, whether or not its transaction
/// committed and whatever generation of the WAL it belongs to, and counts those that hold
/// `needle`.
fn raw_look(db: &[u8], wal: &[u8], dbkey: &[u8; 32], needle: &[u8]) -> RawLook {
    const WAL_HEADER: usize = 32;
    const FRAME_HEADER: usize = 24;
    let holds = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    let mut look = RawLook::default();
    let mut note = |plain: Vec<u8>| {
        look.pages += 1;
        look.with_needle += usize::from(holds(&plain, needle));
        look.with_schema += usize::from(holds(&plain, b"history_transfer"));
    };
    for (i, page) in db.as_chunks::<PAGE>().0.iter().enumerate() {
        note(decrypt_page(
            dbkey,
            page,
            u32::try_from(i + 1).expect("page"),
        ));
    }
    let mut at = WAL_HEADER;
    while at + FRAME_HEADER + PAGE <= wal.len() {
        let number = u32::from_be_bytes(wal[at..at + 4].try_into().expect("four bytes"));
        note(decrypt_page(
            dbkey,
            &wal[at + FRAME_HEADER..at + FRAME_HEADER + PAGE],
            number,
        ));
        at += FRAME_HEADER + PAGE;
    }
    look
}

/// The two database files as they are now: what a kill of the process would leave.
fn files_now(files: &AccountFiles) -> (Vec<u8>, Vec<u8>) {
    let db = std::fs::read(files.backend_db()).expect("backend.db");
    let wal = std::fs::read(files.backend_db().with_extension("db-wal")).unwrap_or_default();
    (db, wal)
}

/// Both looks at the files as they are now: `(committed states of the WAL, those that hold
/// the key in the transfer's row, the look below SQLite)`.
fn look(files: &AccountFiles, scratch: &Path, key: &[u8; 32]) -> (usize, usize, RawLook) {
    let (db, wal) = files_now(files);
    let hex: String = key.iter().map(|b| format!("{b:02X}")).collect();
    let states = commit_prefixes(&wal).len();
    let _ = std::fs::remove_dir_all(scratch);
    let with_key = states_with_the_key(scratch, &db, &wal, files.key, &hex);
    (states, with_key, raw_look(&db, &wal, &files.key, key))
}

/// A harness with our own phone and the files of one account.
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    sink: Sink,
    files: AccountFiles,
}

impl World {
    async fn start() -> Option<Self> {
        let harness = Harness::start_or_skip().await.expect("mock harness")?;
        let tmp = tempfile::tempdir().expect("temp dir");
        let env = local_env(harness.handshake(), tmp.path()).expect("local env");
        let phone = harness
            .create_primary(&CreatePrimary::named("Myself"))
            .await
            .expect("Myself");
        let sink = Sink::open(&tmp.path().join("sink.db")).expect("sink");
        let files = AccountFiles::new(tmp.path()).expect("account dir");
        Some(Self {
            harness,
            tmp,
            env,
            phone,
            sink,
            files,
        })
    }

    /// A fresh link whose phone chose to transfer (test data: a random key of the mock),
    /// up to the stored key. Returns the actor, the linked device and the key.
    async fn link(&self, network: SignalNetwork) -> (FakeCore, DeviceInfo, [u8; 32]) {
        let key = self
            .harness
            .set_ephemeral_backup_key(&self.phone.device_ref)
            .await
            .expect("setEphemeralBackupKey");
        let hex: String = key.iter().map(|b| format!("{b:02X}")).collect();
        let mut core = FakeCore::start_with(
            network,
            &self.env,
            &self.files,
            StartMode::Fresh,
            &self.sink,
        )
        .expect("actor");
        let (url, _) = core.begin_login(None).await;
        let device = self
            .harness
            .complete_provision(&url, &self.phone.device_ref, WAIT)
            .await
            .expect("completeProvision");
        self.files
            .wait_for_i64(
                "the key is stored",
                &format!("SELECT count(*) FROM history_transfer WHERE hex(key) = '{hex}'"),
                |n| n == Some(1),
            )
            .await;
        (core, device, key)
    }

    fn scratch(&self) -> PathBuf {
        self.tmp.path().join("states")
    }

    /// The control: while the transfer waits, both looks find the key (in the WAL, where
    /// the link's transaction put it), and the decryption finds the schema's text.
    fn expect_the_key_is_found(&self, key: &[u8; 32]) {
        let (states, with_key, raw) = look(&self.files, &self.scratch(), key);
        println!(
            "while waiting: {states} committed WAL states, {with_key} with the key; {} of {} \
             decrypted pages hold it",
            raw.with_needle, raw.pages
        );
        assert!(
            with_key > 0,
            "the look through SQLite finds a key that is there"
        );
        assert!(
            raw.with_needle > 0,
            "the look below SQLite finds a key that is there"
        );
        assert!(raw.with_schema > 0, "the pages decrypt to plaintext");
    }

    /// The claim: no state of the two files holds the key, in a row or in freed bytes.
    fn expect_no_key_anywhere(&self, key: &[u8; 32], when: &str) {
        let (states, with_key, raw) = look(&self.files, &self.scratch(), key);
        println!(
            "{when}: {states} committed WAL states, {with_key} with the key; {} of {} decrypted \
             pages hold it",
            raw.with_needle, raw.pages
        );
        assert!(
            raw.pages > 0 && raw.with_schema > 0,
            "the pages decrypt to plaintext"
        );
        assert_eq!(
            with_key, 0,
            "{when}: {with_key} of the {states} database states in backend.db + backend.db-wal \
             still hold the ephemeral backup key"
        );
        assert_eq!(
            raw.with_needle, 0,
            "{when}: {} of {} decrypted pages and WAL frames still hold the bytes of the \
             ephemeral backup key",
            raw.with_needle, raw.pages
        );
    }

    /// Waits for the actor to report a state that is over.
    async fn until_over(core: &mut FakeCore) -> HistoryTransfer {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match core.next_ephemeral(left).await {
                Some(BackendEvent::HistoryTransfer(state)) if state.is_over() => return state,
                Some(_) => {}
                None => panic!("the transfer never ended"),
            }
        }
    }
}

fn network() -> SignalNetwork {
    SignalNetwork::new()
        .with_history_transfer_offer(true)
        .with_history_transfer_wait(Duration::from_secs(120), Duration::from_secs(1))
}

/// A transfer that the phone declined is over: the row says so and holds no key. What a kill
/// at this moment leaves on disk must not hold it either: not in any committed state of the
/// WAL, and not in any byte of a page of either file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ended_transfer_leaves_its_key_in_no_state_of_the_database_files() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device, key) = world.link(network()).await;
    world.expect_the_key_is_found(&key);

    // The phone will not upload: the transfer ends as skipped.
    world
        .harness
        .provide_transfer_error(&device.device_ref, "CONTINUE_WITHOUT_UPLOAD")
        .await
        .expect("provideTransferArchive");
    assert_eq!(World::until_over(&mut core).await, HistoryTransfer::Skipped);
    assert_eq!(
        world
            .files
            .peek_i64(
                "SELECT count(*) FROM history_transfer WHERE state = 'skipped' AND key IS NULL"
            )
            .expect("history_transfer"),
        Some(1),
        "the row of the ended transfer holds no key"
    );
    // The end was reported after its checkpoint: what a kill would leave right now.
    world.expect_no_key_anywhere(&key, "right after the end");
    // The link's own work after the end settles (it writes too), and nothing brings the key
    // back.
    core.wait_online().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    world.expect_no_key_anywhere(&key, "a second after the end");

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The same for an archive that was downloaded and read: the CDN key that fetches the
/// archive is as gone as the ephemeral key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_transfer_leaves_neither_key_in_the_database_files() {
    use kourier_signal::archive_testing::{ArchiveSpec, write_archive};

    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, device, key) = world.link(network()).await;
    let path = world.tmp.path().join("phone-archive");
    let aci = device.aci.parse().expect("the account's ACI");
    write_archive(&path, &key, aci, &ArchiveSpec::default()).expect("archive");
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
    assert_eq!(World::until_over(&mut core).await, HistoryTransfer::Done);
    world.expect_no_key_anywhere(&key, "after a transfer that ended done");
    // The archive's place is a credential too (test data: a random name on the mock's CDN).
    let (db, wal) = files_now(&world.files);
    let raw = raw_look(&db, &wal, &world.files.key, place.key.as_bytes());
    assert!(raw.with_schema > 0);
    assert_eq!(
        raw.with_needle, 0,
        "{} of {} decrypted pages still hold the archive's CDN key",
        raw.with_needle, raw.pages
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// An account that is logged out (or removed) while its transfer waits: the key is gone from
/// the files when the actor ends, before the core deletes them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_logout_during_a_transfer_leaves_its_key_in_no_state_of_the_database_files() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device, key) = world.link(network()).await;
    world.expect_the_key_is_found(&key);
    let exit = core.exit_with(Command::Logout).await;
    assert!(matches!(exit, Exit::LoggedOut { .. }), "{exit:?}");
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM history_transfer")
            .expect("history_transfer"),
        Some(0),
        "the transfer's row is removed"
    );
    world.expect_no_key_anywhere(&key, "after a logout during the wait");
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Set (to a JSON [`ChildSpec`]) in the environment of the helper process that runs the
/// actor for the kill test; [`attack_child_process`] does nothing without it.
const CHILD_ENV: &str = "KOURIER_TRANSFER_ATTACK_CHILD";

/// What the helper process needs to run the actor on the parent's files.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    uuid: String,
    key: Vec<u8>,
    env: BackendEnv,
    sink: PathBuf,
}

/// The helper process of the kill test: runs the account actor on the files of `CHILD_ENV`
/// with `Resume` and WITHOUT the checkpoint that follows an end, connects, and goes on
/// until its parent kills it. It is the process that died between the write that ended
/// the transfer and the checkpoint.
#[test]
fn attack_child_process() {
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
        let killed_before_its_checkpoint = network().without_history_transfer_end_checkpoint();
        let core = FakeCore::start_with(
            killed_before_its_checkpoint,
            &spec.env,
            &files,
            StartMode::Resume,
            &sink,
        )
        .expect("spawn the actor");
        core.send(Command::Connect).await;
        // Only reached when the parent did not kill this process.
        tokio::time::sleep(Duration::from_secs(120)).await;
        let _ = core.shutdown().await;
    });
}

/// A kill right after the end: the process dies (SIGKILL) after the write that ended the
/// transfer and before its checkpoint, so the key IS in the WAL it leaves (the control
/// proves it). The next start truncates the WAL before anything else, and then no state of
/// the files holds the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_after_a_kill_at_the_end_removes_the_key_from_the_database_files() {
    let Some(world) = World::start().await else {
        return;
    };
    // The link runs here and ends by a shutdown, with the transfer waiting.
    let (core, device, key) = world.link(network()).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM history_transfer WHERE state = 'waiting'")
            .expect("history_transfer"),
        Some(1)
    );

    // The process that will be killed: it ends the transfer and does not checkpoint.
    let spec = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
    };
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "attack_child_process",
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
        .expect("spawn the helper process");
    world
        .harness
        .provide_transfer_error(&device.device_ref, "CONTINUE_WITHOUT_UPLOAD")
        .await
        .expect("provideTransferArchive");
    world
        .files
        .wait_for_i64(
            "the transfer ended in the helper process",
            "SELECT count(*) FROM history_transfer WHERE state = 'skipped' AND key IS NULL",
            |n| n == Some(1),
        )
        .await;
    child.kill().await.expect("kill right after the end");

    // The control: this is the state the attack found. The row holds no key, the files do.
    let (states, with_key, raw) = look(&world.files, &world.scratch(), &key);
    println!(
        "killed after the end, before its checkpoint: {states} committed WAL states, \
         {with_key} with the key; {} of {} decrypted pages hold it",
        raw.with_needle, raw.pages
    );
    assert!(
        with_key > 0 && raw.with_needle > 0,
        "the killed process left the key in the WAL (or this test proves nothing)"
    );

    // Start again, then look.
    let mut core = FakeCore::start_with(
        network(),
        &world.env,
        &world.files,
        StartMode::Resume,
        &world.sink,
    )
    .expect("the next start");
    // The start says how the transfer ended; by then it has cleaned up.
    assert_eq!(World::until_over(&mut core).await, HistoryTransfer::Skipped);
    world.expect_no_key_anywhere(&key, "after the start that followed the kill");
    core.send(Command::Connect).await;
    core.wait_online().await;
    world.expect_no_key_anywhere(&key, "after that start connected");
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

// ---- The import: nothing of an archive that was not committed is readable --------------

/// A text that is in every message and every name of the test archive, and nowhere else.
const MARKER: &str = "attack-marker-93e7";

/// The invite link's password of the test archive's group: a needle of its own (the
/// marker is in the group's title and every text, so a look for the marker would pass
/// whether or not the password was kept). The import never reads it: it must be in no page
/// of the stage, no file of the transfer, no page of `backend.db`.
const GROUP_PASSWORD: &str = "attack-password-71fa";

/// The password as a look must search for it: its bytes, and the numbers the stage's JSON
/// writes a byte string as (`[97,116,…]`; a copy kept in the stage would stand so, which a
/// look for the bytes alone does not find).
fn password_needles() -> [Vec<u8>; 2] {
    [
        GROUP_PASSWORD.as_bytes().to_vec(),
        serde_json::to_vec(GROUP_PASSWORD.as_bytes()).expect("json"),
    ]
}

/// The master key of the test archive's group (step S4b): a secret of the group that must
/// never stand in the clear in any file, as its bytes or as the numbers of the stage's JSON.
fn group_master_key() -> [u8; 32] {
    std::array::from_fn(|i| 0xd0 + i as u8)
}

/// The master key as the stage's JSON spells it (`[208,209,…]`), the needle for a look at
/// decrypted pages and at files.
fn group_master_key_json() -> Vec<u8> {
    serde_json::to_vec(&group_master_key()).expect("json")
}

/// The test archive of the import: people, chats and `generated` more messages, each with
/// [`MARKER`] in its text; with `broken_before_the_end` a frame that does not decode stands
/// in front of its last messages. Returns its frames.
fn write_marked_archive(
    path: &Path,
    key: &[u8; 32],
    aci: &str,
    generated: u32,
    broken_before_the_end: bool,
) -> u64 {
    use kourier_signal::archive_testing::{
        ChatSpec, ChatWith, ContactSpec, GeneratedSpec, GroupSpec, HistorySpec, Party,
        SendStatusSpec, SnapshotSpec, TextSpec, Who, write_history_archive,
    };
    let text = |outgoing: bool, n: u64| TextSpec {
        outgoing,
        body: format!("{MARKER} line {n}"),
        sent_at: 1_700_000_000_000 + n,
        received_at: 1_700_000_000_001 + n,
        read: true,
        statuses: if outgoing {
            vec![SendStatusSpec::Read]
        } else {
            Vec::new()
        },
        ..TextSpec::default()
    };
    let mut spec = HistorySpec {
        contacts: vec![ContactSpec {
            aci: Some(uuid::Uuid::from_bytes([0xe5; 16])),
            profile_name: format!("{MARKER} name"),
            system_name: format!("{MARKER} system name"),
            profile_sharing: true,
            ..ContactSpec::default()
        }],
        chats: vec![
            ChatSpec {
                with: ChatWith::Contact(0),
                texts: (0..40).map(|n| text(n % 2 == 0, n)).collect(),
                ..ChatSpec::default()
            },
            ChatSpec {
                with: ChatWith::NoteToSelf,
                texts: vec![text(true, 100)],
                ..ChatSpec::default()
            },
            // A group whose title carries the marker, with two texts.
            ChatSpec {
                with: ChatWith::Group(0),
                texts: vec![
                    text(true, 200),
                    TextSpec {
                        author: Some(Who::Contact(0)),
                        ..text(false, 201)
                    },
                ],
                ..ChatSpec::default()
            },
        ],
        groups: vec![GroupSpec {
            master_key: group_master_key().to_vec(),
            whitelisted: true,
            blocked: false,
            snapshot: Some(SnapshotSpec {
                title: Some(format!("{MARKER} group title")),
                members: vec![(Party::Own, true), (Party::Contact(0), false)],
                invite_link_password: GROUP_PASSWORD.as_bytes().to_vec(),
                ..SnapshotSpec::default()
            }),
        }],
        interleaved: true,
        generated: Some(GeneratedSpec {
            chats: 4,
            messages: generated,
            text: MARKER.to_owned(),
            first_sent_at: 1_700_000_100_000,
        }),
        ..HistorySpec::default()
    };
    let aci = aci.parse().expect("the account's ACI");
    let frames = write_history_archive(path, key, aci, &spec).expect("archive");
    if broken_before_the_end {
        spec.broken_after = Some(u32::try_from(frames - 5).expect("frames"));
        write_history_archive(path, key, aci, &spec).expect("archive");
    }
    frames
}

/// Whether `hay` holds `needle`.
fn holds(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The files of `dir` whose bytes, as they are on disk, hold `needle`; and how many files
/// there are.
fn files_holding(dir: &Path, needle: &[u8]) -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    let mut files = 0;
    let mut with_needle = 0;
    for entry in entries.flatten() {
        let Ok(bytes) = std::fs::read(entry.path()) else {
            continue;
        };
        files += 1;
        with_needle += usize::from(holds(&bytes, needle));
    }
    (with_needle, files)
}

/// (review) Like [`files_holding`], for every regular file below `root`, however deep:
/// `(files that hold the needle, files looked at)`.
fn tree_holding(root: &Path, needle: &[u8]) -> (usize, usize) {
    let mut files = 0;
    let mut with_needle = 0;
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                dirs.push(entry.path());
            } else if kind.is_file()
                && let Ok(bytes) = std::fs::read(entry.path())
            {
                files += 1;
                with_needle += usize::from(holds(&bytes, needle));
            }
        }
    }
    (with_needle, files)
}

/// The helper process of the import's attack: runs the actor on the files of `CHILD_ENV`
/// and dies at the failpoint in its environment (between the importer's commit and the
/// commit zone).
#[test]
fn import_attack_child_process() {
    let Ok(spec) = std::env::var(IMPORT_CHILD_ENV) else {
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
        let core = FakeCore::start_with(network(), &spec.env, &files, StartMode::Resume, &sink)
            .expect("spawn the actor");
        core.send(Command::Connect).await;
        // Only reached when the failpoint did not end this process.
        tokio::time::sleep(Duration::from_secs(120)).await;
        let _ = core.shutdown().await;
    });
}

/// Set in the environment of [`import_attack_child_process`].
const IMPORT_CHILD_ENV: &str = "KOURIER_IMPORT_ATTACK_CHILD";

/// The promise of the import before its commit: an archive that was read and not committed
/// leaves nothing readable. The archive's frames are mapped into the stage, a file of the
/// transfer directory under a key derived from the transfer's key; `backend.db` gets
/// nothing of them, and an abort removes the stage.
///
/// First the control, on a process that is killed between the importer's commit and the
/// commit zone, which is the moment the most is on disk: the whole stage. The marker text
/// IS in it (decrypted with the stage's key, the look finds it in many pages), and it is in
/// no file of the transfer directory as the bytes stand on disk, and in no page of
/// `backend.db` or its WAL. Then the abort: the archive in the transfer directory is
/// replaced by one that fails shortly before its end, the next start removes the stage
/// nobody recorded, stages most of the archive again and aborts. After that the transfer
/// directory is gone, and no page of `backend.db` and its WAL, decrypted, holds the marker.
///
/// Nothing of a text is printed: only counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_of_an_aborted_import_is_readable() {
    use kourier_signal::archive_testing::{STAGE_FILE, read_stage, stage_key};

    let Some(world) = World::start().await else {
        return;
    };
    let needle = MARKER.as_bytes();
    let (core, device, key) = world.link(network()).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let transfer = world.files.dir.join("transfer");

    // The phone uploads a whole archive; the process that reads it dies with the stage
    // whole on disk and no commit recorded.
    let upload = world.tmp.path().join("phone-archive");
    let generated = 400;
    write_marked_archive(&upload, &key, &device.aci, generated, false);
    let place = world
        .harness
        .store_transfer_archive(&upload)
        .await
        .expect("storeTransferArchive");
    world
        .harness
        .provide_transfer_archive(&device.device_ref, &place)
        .await
        .expect("provideTransferArchive");
    let spec = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
    };
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "import_attack_child_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(
            IMPORT_CHILD_ENV,
            serde_json::to_string(&spec).expect("spec"),
        )
        .env(
            kourier_db::failpoint::ENV,
            kourier_signal::failpoints::BEFORE_HISTORY_IMPORT_COMMIT,
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the helper process");
    let status = tokio::time::timeout(WAIT, child.wait())
        .await
        .expect("the helper process reached its failpoint")
        .expect("wait");
    assert!(!status.success(), "the failpoint aborted the process");
    assert_eq!(
        world
            .files
            .peek_i64(
                "SELECT count(*) FROM history_transfer
                  WHERE state = 'reading' AND import_staged = 0"
            )
            .expect("history_transfer"),
        Some(1),
        "the commit zone never ran"
    );

    // The control. The stage holds the archive: read as a stage, and looked at page by
    // page with its key, where the look finds the marker.
    let dump = read_stage(&transfer, &key).expect("the whole stage");
    assert_eq!(dump.messages, 43 + u64::from(generated));
    assert!(
        dump.chats.iter().any(|chat| chat
            .group
            .as_ref()
            .is_some_and(|g| g.title.as_deref() == Some(&format!("{MARKER} group title")))),
        "the group is staged with its title"
    );
    // The group's master key is in the stage (decrypted with its key), as JSON numbers.
    let master_json = group_master_key_json();
    let master_raw = group_master_key();
    let stage_db = std::fs::read(transfer.join(STAGE_FILE)).expect("the stage");
    let stage_wal = std::fs::read(transfer.join(format!("{STAGE_FILE}-wal"))).unwrap_or_default();
    let staged = raw_look(&stage_db, &stage_wal, &stage_key(&key), needle);
    assert!(
        staged.with_needle > 10,
        "the look finds the marker where it is: {} of {} pages of the stage",
        staged.with_needle,
        staged.pages
    );
    assert!(
        raw_look(&stage_db, &stage_wal, &stage_key(&key), &master_json).with_needle > 0,
        "the look finds the master key in the stage"
    );
    // The invite link's password is in the archive and in none of the stage's pages, as its
    // bytes or as the numbers of the stage's JSON.
    for password in password_needles() {
        assert_eq!(
            raw_look(&stage_db, &stage_wal, &stage_key(&key), &password).with_needle,
            0,
            "the stage holds the invite link's password"
        );
    }
    // With any other key the same pages are noise.
    assert_eq!(
        raw_look(&stage_db, &stage_wal, &[0x33; 32], needle).with_needle,
        0
    );
    // (review) And the stage's key is THIS transfer's: derived from another transfer's
    // ephemeral key it opens nothing. Without this the test passes on a stage whose key
    // does not depend on the transfer at all, which the end of the transfer would not make
    // unreadable.
    let mut another_transfer = key;
    another_transfer[0] ^= 0x01;
    assert_eq!(
        raw_look(&stage_db, &stage_wal, &stage_key(&another_transfer), needle).with_needle,
        0,
        "the stage opens with the key of another transfer"
    );
    // As they stand on disk, no file of the transfer directory holds it (the archive is
    // there too, compressed and encrypted).
    let (plain, files) = files_holding(&transfer, needle);
    assert!(files >= 2, "the archive and the stage: {files} files");
    assert_eq!(
        plain, 0,
        "{plain} of {files} files hold the marker in the clear"
    );
    for master in [&master_json[..], &master_raw[..]] {
        assert_eq!(
            files_holding(&transfer, master).0,
            0,
            "a file of the transfer holds the master key in the clear"
        );
    }
    for password in password_needles() {
        assert_eq!(
            files_holding(&transfer, &password).0,
            0,
            "a file of the transfer holds the invite link's password in the clear"
        );
    }
    // And backend.db has none of it, in any page of either file.
    let look_at_backend = |when: &str| {
        let (db, wal) = files_now(&world.files);
        let raw = raw_look(&db, &wal, &world.files.key, needle);
        println!(
            "{when}: {} of {} decrypted pages of backend.db and its WAL hold the marker",
            raw.with_needle, raw.pages
        );
        assert!(raw.pages > 0 && raw.with_schema > 0, "the pages decrypt");
        assert_eq!(
            raw.with_needle, 0,
            "{when}: {} of {} decrypted pages of backend.db and its WAL hold a text of the \
             archive",
            raw.with_needle, raw.pages
        );
        for master in [&master_json[..], &master_raw[..]] {
            assert_eq!(
                raw_look(&db, &wal, &world.files.key, master).with_needle,
                0,
                "{when}: backend.db holds the archive's group master key"
            );
        }
        for password in password_needles() {
            assert_eq!(
                raw_look(&db, &wal, &world.files.key, &password).with_needle,
                0,
                "{when}: backend.db holds the archive's invite link password"
            );
        }
    };
    println!(
        "control: the marker is in {} of {} decrypted pages of the stage, in 0 of {files} \
         files of the transfer directory as they stand",
        staged.with_needle, staged.pages
    );
    look_at_backend("with a whole stage on disk that no commit recorded");
    // (review) And no file anywhere under the test's directory holds it as its bytes stand:
    // the account's other files, its blob store, what the core was given (the sink).
    let (plain, files) = tree_holding(world.tmp.path(), needle);
    for master in [&master_json[..], &master_raw[..]] {
        assert_eq!(
            tree_holding(world.tmp.path(), master).0,
            0,
            "a file holds the group's master key in the clear"
        );
    }
    for password in password_needles() {
        assert_eq!(
            tree_holding(world.tmp.path(), &password).0,
            0,
            "a file holds the invite link's password in the clear"
        );
    }
    println!("control: {plain} of {files} files under the test's directory hold the marker");
    assert!(
        files > 4,
        "the account's files, the sink, the archive: {files}"
    );
    assert_eq!(plain, 0, "a file holds a text of the archive in the clear");

    // The abort. The archive now fails in front of its last messages: the next start
    // removes the stage nobody recorded, stages the frames again, and takes them back.
    write_marked_archive(
        &transfer.join("archive"),
        &key,
        &device.aci,
        generated,
        true,
    );
    let mut core = FakeCore::start_with(
        network().with_history_transfer_frame_delay(Duration::from_millis(2)),
        &world.env,
        &world.files,
        StartMode::Resume,
        &world.sink,
    )
    .expect("the next start");
    core.send(Command::Connect).await;
    let end = World::until_over(&mut core).await;
    assert_eq!(
        end,
        HistoryTransfer::Failed(kourier_model::HistoryTransferFailure::Corrupt)
    );
    assert_eq!(
        world
            .files
            .peek_i64(
                "SELECT count(*) FROM history_transfer
                  WHERE state = 'failed' AND key IS NULL AND import_staged = 0
                    AND stage_messages = 0"
            )
            .expect("history_transfer"),
        Some(1),
        "failed, nothing recorded, the key wiped"
    );
    // Nothing is left to read: no file of the transfer (the stage's key is gone with the
    // transfer's key, and so is the stage), and no text in backend.db.
    assert!(!transfer.exists(), "the transfer directory is removed");
    assert_eq!(files_holding(&transfer, needle), (0, 0));
    look_at_backend("after the aborted import");
    let (plain, files) = tree_holding(world.tmp.path(), needle);
    println!(
        "after the abort: {plain} of {files} files under the test's directory hold the marker"
    );
    assert_eq!(plain, 0, "a file holds a text of the archive in the clear");
    // (review) What the core was given is the sink, a database of its own (the fake core's
    // key): no page of it, decrypted, holds a text of the archive either.
    let sink_db = std::fs::read(world.tmp.path().join("sink.db")).expect("the sink");
    let sink_wal = std::fs::read(world.tmp.path().join("sink.db-wal")).unwrap_or_default();
    let given = raw_look(&sink_db, &sink_wal, &[7; 32], needle);
    println!(
        "after the abort: {} of {} decrypted pages of what the core was given hold the marker",
        given.with_needle, given.pages
    );
    assert!(
        raw_look(&sink_db, &sink_wal, &[7; 32], b"message_json").with_needle > 0,
        "the sink decrypts: its own schema is found"
    );
    assert_eq!(
        given.with_needle, 0,
        "the core was given a text of the archive"
    );
    world.expect_no_key_anywhere(&key, "after the aborted import");
    core.wait_online().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    look_at_backend("a second after the aborted import");
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}
