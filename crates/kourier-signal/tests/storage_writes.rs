//! A10c against `@signalapp/mock-server` (docs/architecture.md §6.9, `target/next/a10-refresh.md`
//! §5.2): what a chat setting changed HERE (archive, pin, mute, marked unread) does to the phone's
//! Storage Service, with the account's switch turned ON through the real command. The phone is the
//! mock's primary: its state is read the way the phone reads it (`storageState`: the manifest and
//! EVERY listed item, decrypted with the phone's keys, so a write the phone could not read fails the
//! read) and the bytes of one record are read as they are (`rawStorageRecord`, nothing
//! re-encoded).
//!
//! The backend runs as the app runs it (`SignalNetwork::spawn`); the test plays the core
//! (`support::fake_core`). Skips without Node >= 24 or the harness's `node_modules`
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).
//!
//! The write job waits a second after the last click (a trailing debounce), so a test that needs
//! few writes clicks everything at once and waits for the phone's state, never for a count of
//! writes: the server may be slow and a click may land in the next write (the job's rate cap,
//! BQ4, is 6 writes in 10 minutes).

use crate::support;

use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use kourier_backend_api::{BackendEnv, Command, Exit, InitialSync, StartMode};
use kourier_model::{
    CmdId, ConvKey, ConversationChange, ErrorKind, IdentityFingerprint, LoginStepKind,
    SafetyNumber, UserId,
};
use kourier_signal::SignalNetwork;
use serde::{Deserialize, Serialize};
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env};
use support::harness::{
    ChatFlags, CreateGroup, CreatePrimary, DeviceInfo, GroupInfo, Harness, PhoneChange, Primary,
    SendText, StorageAccount, StorageContact, StorageGroup, StorageSpec, StorageSubject,
    StorageView,
};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// Set (to a JSON [`ChildSpec`]) for the helper process of the kill tests.
const CHILD_ENV: &str = "KOURIER_SIGNAL_A10C_CHILD";
/// Prefix of the helper's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-A10C-CHILD";

/// Two protobuf fields this build has never heard of, as a newer phone writes them: field 999 (a
/// varint, 7) and field 1000 (a 5-byte string), appended inside a record's message.
const UNKNOWN_FIELDS: [u8; 11] = [
    0xB8, 0x3E, 0x07, // field 999, varint 7
    0xC2, 0x3E, 0x05, b'h', b'e', b'l', b'l', b'o', // field 1000, "hello"
];

/// "Always": the mute end Signal writes for a chat muted for ever.
const ALWAYS: i64 = i64::MAX;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// Now, in ms, on the clock the test's actor reads (`SystemClock`).
fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_millis(),
    )
    .expect("fits")
}

/// The flags of a contact or group record.
fn flags(archived: bool, marked_unread: bool, muted_until: u64) -> ChatFlags {
    ChatFlags {
        archived,
        marked_unread,
        muted_until: Some(muted_until.to_string()),
    }
}

/// The flags of Note to Self (it has no mute).
fn note_flags(archived: bool, marked_unread: bool) -> ChatFlags {
    ChatFlags {
        archived,
        marked_unread,
        muted_until: None,
    }
}

/// The phone ("Demo Phone") with three contacts and the group "Climbing club" with all of them.
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    ada: Primary,
    bruno: Primary,
    chiara: Primary,
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
        let ada = harness
            .create_primary(&CreatePrimary::named("Ada Lindqvist"))
            .await
            .expect("Ada");
        let bruno = harness
            .create_primary(&CreatePrimary::named("Bruno Okafor"))
            .await
            .expect("Bruno");
        let chiara = harness
            .create_primary(&CreatePrimary::named("Chiara Velasco"))
            .await
            .expect("Chiara");
        let phone = harness
            .create_primary(&CreatePrimary {
                contacts: vec![
                    ada.device_ref.clone(),
                    bruno.device_ref.clone(),
                    chiara.device_ref.clone(),
                ],
                ..CreatePrimary::named("Demo Phone")
            })
            .await
            .expect("phone");
        let group = harness
            .create_group(&CreateGroup {
                from: phone.device_ref.clone(),
                title: "Climbing club".to_owned(),
                members: vec![
                    ada.device_ref.clone(),
                    bruno.device_ref.clone(),
                    chiara.device_ref.clone(),
                ],
                ..CreateGroup::default()
            })
            .await
            .expect("create group");
        let sink = Sink::open(&tmp.path().join("sink.db")).expect("sink");
        let files = AccountFiles::new(tmp.path()).expect("account dir");
        Some(Self {
            harness,
            tmp,
            env,
            phone,
            ada,
            bruno,
            chiara,
            group,
            sink,
            files,
        })
    }

    /// The phone's state: three accepted contacts and the account record.
    fn spec(&self) -> StorageSpec {
        StorageSpec {
            primary: self.phone.device_ref.clone(),
            contacts: vec![
                StorageContact::of(&self.ada.device_ref),
                StorageContact::of(&self.bruno.device_ref),
                StorageContact::of(&self.chiara.device_ref),
            ],
            ..StorageSpec::default()
        }
    }

    /// [`Self::spec`] and the group's record.
    fn spec_with_group(&self) -> StorageSpec {
        StorageSpec {
            groups: vec![StorageGroup {
                id: self.group.id.clone(),
                ..StorageGroup::default()
            }],
            ..self.spec()
        }
    }

    fn user(primary: &Primary) -> UserId {
        UserId::parse(&format!("aci:{}", primary.aci)).expect("user id")
    }

    fn direct(primary: &Primary) -> ConvKey {
        ConvKey::Direct(Self::user(primary))
    }

    /// The key of the 1:1 chat with `primary` in an event.
    fn chat_key(primary: &Primary) -> String {
        format!("direct:aci:{}", primary.aci)
    }

    fn group_conv(&self) -> ConvKey {
        let id = STANDARD.decode(&self.group.id).expect("group id");
        ConvKey::Group(format!("gv2:{}", URL_SAFE_NO_PAD.encode(id)).into())
    }

    /// Note to Self: the chat with our own ACI.
    fn note_to_self(&self) -> ConvKey {
        Self::direct(&self.phone)
    }

    /// Links a fresh account to the phone whose state is `spec`, and waits until the first
    /// manifest is merged (and, with `group`, until the group can be written to).
    async fn link(&self, spec: &StorageSpec, group: bool) -> (FakeCore, DeviceInfo) {
        self.harness
            .set_storage_state(spec)
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
            .wait_for_i64_noting(
                "the first manifest merged",
                "SELECT manifest_version FROM account",
                |v| v == Some(1),
                || self.files.timeout_note(self.harness.handshake().port),
            )
            .await;
        if group {
            self.files
                .wait_for_i64_noting(
                    "group state with endorsements",
                    "SELECT count(*) FROM groups WHERE revision IS NOT NULL
                       AND endorsements_expire_at IS NOT NULL AND membership = 'member'",
                    |v| v == Some(1),
                    || self.files.timeout_note(self.harness.handshake().port),
                )
                .await;
        }
        (core, device.expect("linked device"))
    }

    /// Turns the account's switch ON through the real command (every account starts off).
    async fn switch_on(&self, core: &FakeCore) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        core.send(Command::SetRoamingWrites { on: true, reply })
            .await;
        answer.await.expect("answer").expect("the switch is set");
    }

    /// Waits until the phone's state, as the phone reads it, satisfies `done`; returns it.
    async fn wait_phone(&self, what: &str, done: impl Fn(&StorageView) -> bool) -> StorageView {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            let view = self
                .harness
                .storage_state(&self.phone.device_ref)
                .await
                .expect("the phone reads its own storage state");
            if done(&view) {
                return view;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}; the phone's state: {view:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The version of the phone's manifest.
    async fn version(&self) -> Option<u64> {
        let v = self
            .harness
            .request_value(
                "storageVersion",
                serde_json::json!({"primary": self.phone.device_ref}),
            )
            .await
            .expect("storageVersion");
        v["version"].as_u64()
    }

    /// How many writes of Kourier the server applied and how many it answered 409.
    async fn writes(&self) -> (u64, u64) {
        let counted = self.harness.storage_writes().await.expect("storageWrites");
        (counted.writes, counted.conflicts)
    }

    /// How many PUTs of Kourier reached the server and were refused (V4): the oracle of the
    /// switch-off tests must see a PUT the mock turned away too.
    async fn refused(&self) -> u64 {
        self.harness
            .storage_writes()
            .await
            .expect("storageWrites")
            .refused
    }

    /// One integer from `backend.db`.
    fn count(&self, sql: &str) -> i64 {
        self.files
            .peek_i64(sql)
            .expect("backend.db")
            .expect("one row")
    }

    /// Waits until `sql` (one integer) is `want`.
    async fn wait_count(&self, what: &str, sql: &str, want: i64) {
        self.files
            .wait_for_i64(what, sql, |v| v == Some(want))
            .await;
    }
}

/// A click: `change` on `conv`, accepted.
async fn click(core: &FakeCore, conv: &ConvKey, change: ConversationChange) {
    let (reply, answer) = tokio::sync::oneshot::channel();
    core.send(Command::ChangeConversation {
        conv: conv.clone(),
        change,
        id: None,
        reply,
    })
    .await;
    answer
        .await
        .expect("answer")
        .expect("the change is accepted");
}

fn archive(archived: bool) -> ConversationChange {
    ConversationChange::SetArchived { archived }
}

fn pin(pinned: bool) -> ConversationChange {
    ConversationChange::SetPinned { pinned }
}

fn mute(until: Option<i64>) -> ConversationChange {
    ConversationChange::SetMuted { until }
}

fn mark(marked_unread: bool) -> ConversationChange {
    ConversationChange::SetMarkedUnread { marked_unread }
}

/// 1. A change made here reaches the phone, and its undo: an archive, a pin, a mute for an hour
/// and for ever, a mark unread; on a contact, on a group and on Note to Self (whose two flags
/// and whose pin live in the account record). The phone reads every record the writes produced
/// (its own `getStorageState()` decrypts the manifest and every item), no record is left
/// behind and no item is an orphan. Three batches of clicks, each one write: the changes, their
/// undo, and the rest. Guards: the whole of the write (plan, seal, PUT, landing) and the
/// mapping of every field to the record's field number. Mutant: a mute is written as "not muted".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_made_here_reaches_the_phone() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link(&world.spec_with_group(), true).await;
    world.switch_on(&core).await;
    let (ada, bruno, chiara) = (
        &world.ada.device_ref,
        &world.bruno.device_ref,
        &world.chiara.device_ref,
    );
    let group = world.group.id.as_str();
    let (ada_chat, bruno_chat) = (World::direct(&world.ada), World::direct(&world.bruno));
    let chiara_chat = World::direct(&world.chiara);
    let (group_chat, own_chat) = (world.group_conv(), world.note_to_self());
    let hour = now_ms() + 3_600_000;

    // The changes.
    click(&core, &ada_chat, archive(true)).await;
    click(&core, &bruno_chat, pin(true)).await;
    click(&core, &chiara_chat, mute(Some(hour))).await;
    click(&core, &group_chat, mark(true)).await;
    click(&core, &group_chat, mute(Some(ALWAYS))).await;
    click(&core, &own_chat, archive(true)).await;
    click(&core, &own_chat, mark(true)).await;
    let view = world
        .wait_phone("the first batch", |v| {
            v.contacts.get(ada).is_some_and(|c| c.archived)
                && v.groups.get(group).is_some_and(|g| g.marked_unread)
                && v.note_to_self.marked_unread
                && v.pins.len() == 1
                && v.contacts
                    .get(chiara)
                    .is_some_and(|c| c.muted_until_ms() != 0)
        })
        .await;
    assert_eq!(view.contact(ada), &flags(true, false, 0));
    assert_eq!(view.contact(bruno), &flags(false, false, 0));
    assert_eq!(view.contact(chiara).muted_until_ms(), hour as u64);
    assert!(!view.contact(chiara).archived && !view.contact(chiara).marked_unread);
    assert_eq!(
        view.group(group),
        &flags(false, true, ALWAYS as u64),
        "a group: marked unread, muted for ever (i64::MAX)"
    );
    assert_eq!(view.note_to_self, note_flags(true, true));
    assert_eq!(view.pins, [format!("contact:{bruno}")]);
    assert_eq!(view.records, 5, "the account, three contacts, the group");
    assert_eq!(view.orphans, 0, "the replaced records are deleted");

    // The undo of each.
    click(&core, &ada_chat, archive(false)).await;
    click(&core, &bruno_chat, pin(false)).await;
    click(&core, &chiara_chat, mute(None)).await;
    click(&core, &group_chat, mark(false)).await;
    click(&core, &group_chat, mute(None)).await;
    click(&core, &own_chat, archive(false)).await;
    click(&core, &own_chat, mark(false)).await;
    let view = world
        .wait_phone("the undo", |v| {
            v.pins.is_empty()
                && !v.contacts.get(ada).is_some_and(|c| c.archived)
                && !v.groups.get(group).is_some_and(|g| g.marked_unread)
                && v.note_to_self == note_flags(false, false)
                && v.contacts
                    .get(chiara)
                    .is_some_and(|c| c.muted_until_ms() == 0)
        })
        .await;
    for contact in [ada, bruno, chiara] {
        assert_eq!(view.contact(contact), &flags(false, false, 0), "{contact}");
    }
    assert_eq!(view.group(group), &flags(false, false, 0));
    assert_eq!(view.records, 5);
    assert_eq!(view.orphans, 0);

    // A mark and a mute for ever on a contact, an archive of the group, two pins (a contact and
    // Note to Self, in the order they were clicked).
    click(&core, &ada_chat, mark(true)).await;
    click(&core, &ada_chat, mute(Some(ALWAYS))).await;
    click(&core, &ada_chat, pin(true)).await;
    click(&core, &group_chat, archive(true)).await;
    click(&core, &own_chat, pin(true)).await;
    let view = world
        .wait_phone("the rest", |v| {
            v.pins.len() == 2 && v.groups.get(group).is_some_and(|g| g.archived)
        })
        .await;
    assert_eq!(view.contact(ada), &flags(false, true, ALWAYS as u64));
    assert_eq!(view.group(group), &flags(true, false, 0));
    assert_eq!(
        view.pins,
        [
            format!("contact:{ada}"),
            format!("contact:{}", world.phone.device_ref)
        ],
        "pins in click order, Note to Self by our own ACI"
    );
    assert_eq!(view.records, 5);
    assert_eq!(view.orphans, 0);
    let (applied, conflicts) = world.writes().await;
    assert!(
        applied >= 3 && conflicts == 0,
        "{applied} writes, {conflicts} conflicts"
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 2. Fields a record has that this build does not know survive our write byte for byte: the
/// phone's contact, group and account records carry two unknown fields (a newer phone's); an
/// archive, a mark unread and a pin rewrite the three records, and each still ends with the
/// unknown bytes, inside its message, while the flag we set is there. A client that re-encoded
/// the record from its own types would lose them. Mutant: the written bytes are re-encoded from
/// the generated type.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_fields_of_a_record_survive_our_write() {
    let Some(world) = World::start().await else {
        return;
    };
    let unknown = STANDARD.encode(UNKNOWN_FIELDS);
    let mut spec = world.spec_with_group();
    spec.contacts[0].unknown_fields = Some(unknown.clone());
    spec.groups[0].unknown_fields = Some(unknown.clone());
    spec.account = Some(StorageAccount {
        unknown_fields: Some(unknown),
        ..StorageAccount::default()
    });
    let (core, _device) = world.link(&spec, true).await;
    world.switch_on(&core).await;
    let (ada, bruno) = (&world.ada.device_ref, &world.bruno.device_ref);
    let group = world.group.id.as_str();
    let phone = world.phone.device_ref.as_str();

    let before_ada = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record");
    assert!(
        before_ada.bytes().ends_with(&UNKNOWN_FIELDS),
        "the harness put the unknown fields at the end of the contact record"
    );

    click(&core, &World::direct(&world.ada), archive(true)).await;
    click(&core, &world.group_conv(), mark(true)).await;
    click(&core, &World::direct(&world.bruno), pin(true)).await;
    let view = world
        .wait_phone("the three records written", |v| {
            v.contacts.get(ada).is_some_and(|c| c.archived)
                && v.groups.get(group).is_some_and(|g| g.marked_unread)
                && v.pins.len() == 1
        })
        .await;
    assert_eq!(view.pins, [format!("contact:{bruno}")]);

    let after_ada = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record");
    assert_ne!(after_ada.id, before_ada.id, "the record was rotated");
    assert!(after_ada.flags.archived);
    assert!(
        after_ada.bytes().ends_with(&UNKNOWN_FIELDS),
        "the contact record lost its unknown fields"
    );
    let group_record = world
        .harness
        .raw_storage_record(phone, StorageSubject::Group(group))
        .await
        .expect("the group's record");
    assert!(group_record.flags.marked_unread);
    assert!(
        group_record.bytes().ends_with(&UNKNOWN_FIELDS),
        "the group record lost its unknown fields"
    );
    let account = world
        .harness
        .raw_storage_record(phone, StorageSubject::Account)
        .await
        .expect("the account record");
    assert!(
        account.bytes().ends_with(&UNKNOWN_FIELDS),
        "the account record lost its unknown fields"
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 3. A record we cannot read stays in the manifest: one of the phone's contact records is not a
/// ciphertext this build (or the phone's own client) can open. A write for another chat must keep
/// its identifier in the new manifest, with its type, and leave its item on the server: the
/// manifest is built from the FETCHED one, never from the records we stored (the unreadable one
/// is not among them). Mutant: the write is planned on a manifest that lists only the records we
/// have stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_we_cannot_read_stays_in_the_manifest() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = world.spec();
    spec.contacts[1].undecryptable = true;
    let (core, _device) = world.link(&spec, false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());
    let before = world
        .harness
        .storage_identifiers(phone)
        .await
        .expect("the manifest");
    assert_eq!(before.ids.len(), 4, "the account and three contacts");
    assert_eq!(before.unreadable, 1, "the harness broke one item");
    assert!(
        world.harness.storage_state(phone).await.is_err(),
        "the phone cannot read its own state either: the premise of the test"
    );
    assert_eq!(
        world.count("SELECT count(*) FROM storage_records WHERE target LIKE 'contact:%'"),
        2,
        "we stored the two records we could read"
    );

    click(&core, &World::direct(&world.ada), archive(true)).await;
    let wrote = world
        .files
        .wait_for_i64(
            "the write landed",
            "SELECT manifest_version FROM account",
            |v| v == Some(2),
        )
        .await;
    assert_eq!(wrote, Some(2));
    let after = world
        .harness
        .storage_identifiers(phone)
        .await
        .expect("the manifest");
    assert_eq!(after.version, Some(2));
    let kept = |ids: &[support::harness::StorageIdentifier]| -> std::collections::BTreeSet<_> {
        ids.iter().map(|i| (i.kind, i.id.clone())).collect()
    };
    let gone: Vec<_> = kept(&before.ids)
        .difference(&kept(&after.ids))
        .cloned()
        .collect();
    let added: Vec<_> = kept(&after.ids)
        .difference(&kept(&before.ids))
        .cloned()
        .collect();
    assert_eq!(
        (gone.len(), added.len()),
        (1, 1),
        "only Ada's record was replaced (gone {gone:?}, added {added:?})"
    );
    assert_eq!(after.ids.len(), 4, "the unreadable record is still listed");
    assert_eq!(after.unreadable, 1, "its item is still there, still broken");
    assert_eq!(
        (after.missing, after.orphans),
        (0, 0),
        "every identifier has its item and no item is left over"
    );
    let record = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record, written by us");
    assert!(record.flags.archived);

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 4. Our write leaves no orphans: the record a write replaces is deleted from the server, in the
/// same request, every time. Two writes in a row on the same record and one on the account record
/// (a pin); after each the server holds exactly the items the manifest lists. Mutant: the old
/// identifier's delete is left out of the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn our_write_leaves_no_orphans() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());

    click(&core, &World::direct(&world.ada), archive(true)).await;
    click(&core, &World::direct(&world.bruno), pin(true)).await;
    world
        .wait_phone("the first write", |v| {
            v.contacts.get(ada).is_some_and(|c| c.archived) && v.pins.len() == 1
        })
        .await;
    let first = world
        .harness
        .storage_identifiers(phone)
        .await
        .expect("the manifest");
    assert_eq!(
        (first.items, first.orphans, first.missing),
        (4, 0, 0),
        "after the first write the server holds the four listed items and nothing else: {first:?}"
    );

    click(&core, &World::direct(&world.ada), archive(false)).await;
    let view = world
        .wait_phone("the second write", |v| {
            v.contacts.get(ada).is_some_and(|c| !c.archived)
        })
        .await;
    assert_eq!(view.orphans, 0);
    let second = world
        .harness
        .storage_identifiers(phone)
        .await
        .expect("the manifest");
    assert_eq!(
        (second.items, second.orphans, second.missing),
        (4, 0, 0),
        "after the second write too: {second:?}"
    );
    assert_eq!(second.version, Some(first.version.expect("a version") + 1));

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 5. A conflict is merged and written again: the phone writes (it marks Chiara's chat unread)
/// just before our PUT is looked at, so the server answers 409. The job does not give the click
/// up: its next run fetches the newer manifest, MERGES it (the core hears of Chiara's mark) and
/// writes the archive of Ada on top of it, version 3. Both changes are on the server, once each;
/// the server saw one write of ours applied and one refused. Mutant: the retry does not merge
/// what it fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conflict_is_merged_and_written_again() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (ada, chiara, phone) = (
        &world.ada.device_ref,
        &world.chiara.device_ref,
        world.phone.device_ref.as_str(),
    );
    world
        .harness
        .conflict_next_storage_write(
            phone,
            StorageSubject::Contact(chiara),
            &PhoneChange {
                marked_unread: Some(true),
                ..PhoneChange::default()
            },
        )
        .await
        .expect("arm the phone's change");

    click(&core, &World::direct(&world.ada), archive(true)).await;
    let view = world
        .wait_phone("both changes on the server", |v| {
            v.version == Some(3)
                && v.contacts.get(ada).is_some_and(|c| c.archived)
                && v.contacts.get(chiara).is_some_and(|c| c.marked_unread)
        })
        .await;
    assert_eq!(view.contact(ada), &flags(true, false, 0));
    assert_eq!(view.contact(chiara), &flags(false, true, 0));
    assert_eq!(view.records, 4);
    assert_eq!(view.orphans, 0);
    assert_eq!(
        world.writes().await,
        (1, 1),
        "one write of ours applied, one answered 409"
    );
    // The retry merged the phone's change: the core hears of it.
    let chiara_key = World::chat_key(&world.chiara);
    world
        .sink
        .wait_event_value("ConversationUpsert", |patch| {
            patch["key"] == chiara_key.as_str() && patch["marked_unread"] == true
        })
        .await;
    world
        .wait_count(
            "the conflict is settled",
            "SELECT count(*) FROM storage_intents",
            0,
        )
        .await;
    assert_eq!(
        world.count("SELECT count(*) FROM storage_write_inflight"),
        0
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 6. The phone wins the same field: Ada's chat is muted for ever HERE, and before the write goes
/// out the phone mutes it until another time. The phone's word is the newer one: the job merges
/// it, drops our intent, the core is told the phone's mute and NOTHING is written (the server
/// never saw a write of ours). The write is held back with the Storage Service's manifest
/// failing (500), so the phone's change is certain to come first. Mutant: the three-way rule
/// always keeps our value.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_wins_the_same_field() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());
    let phones_mute: u64 = 4_102_444_800_000;

    world
        .harness
        .fail_storage_manifest(true)
        .await
        .expect("fail the manifest");
    click(&core, &World::direct(&world.ada), mute(Some(ALWAYS))).await;
    // The job tried and was refused: the phone's change is made while our write waits.
    let deadline = tokio::time::Instant::now() + WAIT;
    while world
        .harness
        .fail_storage_manifest(true)
        .await
        .expect("fail the manifest")
        == 0
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the write job never asked for the manifest"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    world
        .harness
        .update_storage_record(
            phone,
            StorageSubject::Contact(ada),
            &PhoneChange {
                muted_until: Some(phones_mute),
                ..PhoneChange::default()
            },
        )
        .await
        .expect("the phone mutes Ada");
    world
        .harness
        .fail_storage_manifest(false)
        .await
        .expect("serve the manifest again");

    let ada_key = World::chat_key(&world.ada);
    world
        .sink
        .wait_event_value("ConversationUpsert", |patch| {
            patch["key"] == ada_key.as_str() && patch["muted_until"]["Set"] == phones_mute
        })
        .await;
    world
        .wait_count(
            "our intent is gone",
            "SELECT count(*) FROM storage_intents",
            0,
        )
        .await;
    // Nothing is written, now or after the job had every chance to.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(world.writes().await, (0, 0), "the phone's word stands");
    assert_eq!(world.version().await, Some(2), "only the phone wrote");
    let view = world.wait_phone("the phone's state", |_| true).await;
    assert_eq!(view.contact(ada).muted_until_ms(), phones_mute);
    assert_eq!(
        world.count("SELECT count(*) FROM storage_write_inflight"),
        0
    );
    assert_eq!(
        world.count("SELECT count(*) FROM jobs WHERE kind = 'storage_write'"),
        0,
        "no write job waits"
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// What the helper process of the kill tests runs.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    uuid: String,
    key: Vec<u8>,
    env: BackendEnv,
    sink: PathBuf,
    /// The chat the helper archives.
    conv: ConvKey,
}

/// Links an account, turns the switch on, stops it, and lets a helper process start it again,
/// archive Ada's chat and run until the failpoint after the server's 200 aborts it (the write
/// is on the server, nothing of it is committed here). Checks what the kill left and returns
/// the linked device.
async fn kill_after_the_put(world: &World) -> DeviceInfo {
    let (core, device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    let child = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
        conv: World::direct(&world.ada),
    };
    let failpoint = format!("{}:1", kourier_signal::failpoints::AFTER_STORAGE_WRITE_PUT);
    let mut process = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .arg(support::child_filter(module_path!(), "child_actor_process"))
        .args(["--exact", "--nocapture", "--test-threads=1"])
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

    // What the kill left: the server applied the write, we hold the row and the intent.
    assert_eq!(world.writes().await, (1, 0), "the server applied one write");
    assert_eq!(world.version().await, Some(2));
    assert_eq!(
        world.count("SELECT count(*) FROM storage_intents"),
        1,
        "the click is still an intent here: nothing of the write was committed"
    );
    assert_eq!(
        world.count("SELECT manifest_version FROM account"),
        1,
        "the version waits for the landing"
    );
    device
}

/// Starts the account again in this process and connects it.
fn restart(world: &World) -> FakeCore {
    FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("start")
}

/// 7. A kill after the server applied our write writes nothing twice: the helper is killed
/// between the server's 200 and our own commit. After the restart the job finds its row, sees
/// its own identifier in the manifest and lands the write: the intent is closed, the row is gone,
/// the version is 2, the new record is stored as OURS, and no second PUT goes out (the server's
/// count of our writes stays 1, no orphan, no record twice). Mutant: the row is not written
/// before the PUT (the kill then leaves no row: the record is merged as the phone's).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_after_the_server_applied_writes_nothing_twice() {
    let Some(world) = World::start().await else {
        return;
    };
    let _device = kill_after_the_put(&world).await;

    let mut core = restart(&world);
    core.send(Command::Connect).await;
    world
        .wait_count(
            "the write landed",
            "SELECT manifest_version FROM account",
            2,
        )
        .await;
    world
        .wait_count(
            "the row is judged",
            "SELECT count(*) FROM storage_write_inflight",
            0,
        )
        .await;
    world
        .wait_count(
            "the intent is closed",
            "SELECT count(*) FROM storage_intents",
            0,
        )
        .await;
    core.wait_online().await;
    // Give a second PUT every chance to show.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(world.writes().await, (1, 0), "the write is not made again");
    assert_eq!(world.version().await, Some(2));
    assert_eq!(
        world.count("SELECT count(*) FROM storage_records WHERE ours = 1"),
        1,
        "the landing stored the written record as ours: the phone did not write it"
    );
    assert_eq!(
        world.count("SELECT count(*) FROM storage_records WHERE target LIKE 'contact:%'"),
        3
    );
    let view = world
        .wait_phone("Ada archived", |v| {
            v.contacts
                .get(&world.ada.device_ref)
                .is_some_and(|c| c.archived)
        })
        .await;
    assert_eq!((view.records, view.orphans), (4, 0));
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 7, M8: and the phone is still told to fetch. The landing after the kill queues the
/// fetch-latest sync like a landing in the zone of a 200 does: our other devices (the phone) hear
/// `SyncMessage.FetchLatest { STORAGE_MANIFEST }` (`storage::write::run_fetch_latest`, once the
/// account is `Online`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_after_the_server_applied_still_tells_the_phone_to_fetch() {
    let Some(world) = World::start().await else {
        return;
    };
    let device = kill_after_the_put(&world).await;
    let mut core = restart(&world);
    core.send(Command::Connect).await;
    core.wait_online().await;
    let told = world
        .harness
        .wait_for_sync_message(
            &world.phone.device_ref,
            Some(&device.device_ref),
            "fetchLatest",
            Duration::from_secs(20),
        )
        .await
        .expect("the phone is told to fetch the manifest");
    let kind = &told.sync_message["content"]["fetchLatest"]["type"];
    assert!(
        kind == 2 || kind == "STORAGE_MANIFEST",
        "the manifest is what the phone is told to fetch: {kind}"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The helper process of the kill tests: resumes the account, connects, archives the chat of its
/// spec and runs until the armed failpoint aborts it.
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
        click(&core, &spec.conv, archive(true)).await;
        tokio::time::sleep(Duration::from_secs(120)).await;
        println!("{CHILD_LINE} the failpoint did not fire");
        let _ = core.shutdown().await;
    });
}

/// 8. A change the PHONE alone made causes no write: the switch is on, nothing was clicked here,
/// the phone archives Bruno's chat and says FetchLatest. The core hears of the archive, and the
/// Storage Service sees nothing of ours: no write, no job, no row in flight, no intent, the
/// version the phone's. Mutant: a sync files a write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_phone_change_alone_causes_no_write() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (bruno, phone) = (&world.bruno.device_ref, world.phone.device_ref.as_str());

    let version = world
        .harness
        .update_storage_record(
            phone,
            StorageSubject::Contact(bruno),
            &PhoneChange {
                archived: Some(true),
                ..PhoneChange::default()
            },
        )
        .await
        .expect("the phone archives Bruno");
    assert_eq!(version, 2);
    world
        .harness
        .send_fetch_latest(phone, &device.device_ref, "storageManifest")
        .await
        .expect("fetch latest");
    let bruno_key = World::chat_key(&world.bruno);
    world
        .sink
        .wait_event_value("ConversationUpsert", |patch| {
            patch["key"] == bruno_key.as_str() && patch["archived"] == true
        })
        .await;
    world
        .wait_count(
            "the manifest is merged",
            "SELECT manifest_version FROM account",
            2,
        )
        .await;
    // A write, if anything caused one, is made a second after the merge.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(world.writes().await, (0, 0), "we wrote nothing");
    assert_eq!(
        world.version().await,
        Some(2),
        "the manifest is the phone's"
    );
    for table in ["storage_intents", "storage_write_inflight"] {
        assert_eq!(
            world.count(&format!("SELECT count(*) FROM {table}")),
            0,
            "{table}"
        );
    }
    assert_eq!(
        world.count("SELECT count(*) FROM jobs WHERE kind = 'storage_write'"),
        0,
        "no write job"
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

// ---------------------------------------------------------------------------------------------
// Tests 9-14 (C6): the phone is told, the debounce, the switch, the archive rules, the other
// kinds of records, the safety number's state. Test 15 (a reset during a write) and the 409
// back-off are proven below the wire (see the end of the file).
// ---------------------------------------------------------------------------------------------

impl World {
    /// A text from `from` to the linked device; returns when the core has it (and so the
    /// receive zone, A1's bring-back included, has committed).
    async fn message_from(&self, from: &Primary, device: &DeviceInfo, text: &str) {
        let sent = self
            .harness
            .send_text(&SendText {
                await_delivery: true,
                timeout: Some(60_000),
                ..SendText::new(&from.aci, &device.device_ref, text)
            })
            .await
            .expect("sendText");
        let author = Self::user(from);
        self.sink
            .wait_message(author.as_str(), &sent.timestamp.to_string(), WAIT)
            .await
            .expect("the message reached the core");
    }

    /// How many times the core was told that the chat with `primary` is out of the archive.
    fn unarchives(&self, primary: &Primary) -> usize {
        let key = Self::chat_key(primary);
        self.sink
            .event_values("ConversationUpsert")
            .iter()
            .filter(|p| p["key"] == key.as_str() && p["archived"] == false)
            .count()
    }

    /// Waits until the core was told `n` times that the chat left the archive.
    async fn wait_unarchives(&self, primary: &Primary, n: usize) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while self.unarchives(primary) < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the chat never left the archive"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The safety number of `primary` as Kourier shows it (the key it was computed from).
    async fn safety_number(core: &FakeCore, primary: &Primary) -> SafetyNumber {
        let (reply, answer) = tokio::sync::oneshot::channel();
        core.send(Command::SafetyNumber {
            user: Self::user(primary),
            reply,
        })
        .await;
        answer
            .await
            .expect("answer")
            .expect("answered")
            .expect("a safety number")
    }

    /// Mark (`verified`) or clear the verification of the key the user was shown.
    async fn set_verified(
        core: &FakeCore,
        primary: &Primary,
        verified: bool,
        key: &IdentityFingerprint,
    ) {
        let (reply, answer) = tokio::sync::oneshot::channel();
        core.send(Command::SetVerified {
            user: Self::user(primary),
            verified,
            key: key.clone(),
            reply,
        })
        .await;
        answer.await.expect("answer").expect("the state is set");
    }
}

/// A click the backend refuses: the kind of its refusal.
async fn click_refused(core: &FakeCore, conv: &ConvKey, change: ConversationChange) -> ErrorKind {
    let (reply, answer) = tokio::sync::oneshot::channel();
    core.send(Command::ChangeConversation {
        conv: conv.clone(),
        change,
        id: None,
        reply,
    })
    .await;
    answer
        .await
        .expect("answer")
        .expect_err("the change is refused")
        .kind
}

/// The core's own change of a chat, as it queues one when the user reads a chat the phone
/// marked: `CommandPersisted` says the backend has it.
async fn durable(core: &FakeCore, cmd: i64, conv: &ConvKey, change: ConversationChange) {
    core.send(Command::ChangeConversationDurable {
        cmd: CmdId(cmd),
        conv: conv.clone(),
        change,
        asked_at: now_ms(),
    })
    .await;
}

/// 9. The phone is told to fetch: once our write is on the server (the 200 landed in the same
/// zone that stores the written records), the phone hears `SyncMessage.FetchLatest {
/// STORAGE_MANIFEST }` from our device, so it does not wait for its own poll. The write is
/// already counted by the server when the phone hears. Mutant (dropped by the lead's speed
/// rule): the landing queues no fetch-latest job.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_phone_is_told_to_fetch() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;

    click(&core, &World::direct(&world.ada), archive(true)).await;
    let told = world
        .harness
        .wait_for_sync_message(
            &world.phone.device_ref,
            Some(&device.device_ref),
            "fetchLatest",
            WAIT,
        )
        .await
        .expect("the phone is told to fetch the manifest");
    let kind = &told.sync_message["content"]["fetchLatest"]["type"];
    assert!(
        kind == 2 || kind == "STORAGE_MANIFEST",
        "the manifest is what the phone is told to fetch: {kind}"
    );
    assert_eq!(
        world.writes().await,
        (1, 0),
        "it hears after the server applied our write"
    );
    let view = world
        .wait_phone("Ada archived", |v| {
            v.contacts
                .get(&world.ada.device_ref)
                .is_some_and(|c| c.archived)
        })
        .await;
    assert_eq!(view.version, Some(2));

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 10. Clicks within a second are one write: three changes of three chats, one after the other
/// with nothing between them, are ONE request (the job waits a second after the LAST click), and
/// nothing else is written afterwards. The unit tests pin the due time exactly; this is the
/// real runner on the real clock. Mutant (dropped by the lead's speed rule): the job is queued
/// at the click, not a second after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clicks_within_a_second_are_one_write() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (ada, bruno, chiara) = (
        &world.ada.device_ref,
        &world.bruno.device_ref,
        &world.chiara.device_ref,
    );
    let hour = now_ms() + 3_600_000;

    click(&core, &World::direct(&world.ada), archive(true)).await;
    click(&core, &World::direct(&world.bruno), pin(true)).await;
    click(&core, &World::direct(&world.chiara), mute(Some(hour))).await;
    let view = world
        .wait_phone("the three changes", |v| {
            v.contacts.get(ada).is_some_and(|c| c.archived)
                && v.pins.len() == 1
                && v.contacts
                    .get(chiara)
                    .is_some_and(|c| c.muted_until_ms() != 0)
        })
        .await;
    assert_eq!(view.version, Some(2), "one write: the phone's version + 1");
    assert_eq!(view.pins, [format!("contact:{bruno}")]);
    assert_eq!(view.contact(chiara).muted_until_ms(), hour as u64);
    // Nothing else follows.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(world.writes().await, (1, 0), "one request for three clicks");
    assert_eq!(world.version().await, Some(2));

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 11. With the switch off nothing is written (B1), however the change comes: the phone's mark
/// on Ada's chat is read through the core's durable clear (acknowledged, dropped, the phone's
/// mark stays), four kinds of click are refused (`Unsupported`, nothing stored), and a counting
/// message in the chat the phone archived brings it back HERE only (the core is told, no intent
/// is filed). The server saw no request of ours, the phone's records are as it left them and no
/// job, intent or row in flight exists. The switch is never turned on in this test. Mutant: the
/// backend's gate is left to the core (the switch is not looked at in `intents::intend`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_the_switch_off_nothing_is_written() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = world.spec();
    spec.contacts[0].marked_unread = Some(true); // Ada
    spec.contacts[1].archived = Some(true); // Bruno
    let (core, device) = world.link(&spec, false).await;
    let (ada, bruno) = (&world.ada.device_ref, &world.bruno.device_ref);

    // The core reads Ada's marked chat: its clear is confirmed and dropped.
    durable(&core, 41, &World::direct(&world.ada), mark(false)).await;
    world
        .sink
        .wait_event_value("CommandPersisted", |v| v["cmd"] == 41)
        .await;
    // Every click that would be written is refused.
    let chiara = World::direct(&world.chiara);
    for change in [archive(true), pin(true), mute(Some(ALWAYS)), mark(true)] {
        assert_eq!(
            click_refused(&core, &chiara, change).await,
            ErrorKind::Unsupported
        );
    }
    // A counting message in the archived chat.
    let before = world.unarchives(&world.bruno);
    world
        .message_from(&world.bruno, &device, "are you around tonight?")
        .await;
    world.wait_unarchives(&world.bruno, before + 1).await;

    // Give a write every chance to show.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        world.writes().await,
        (0, 0),
        "the server saw nothing of ours"
    );
    assert_eq!(world.refused().await, 0, "not even a PUT it turned away");
    assert_eq!(
        world.version().await,
        Some(1),
        "the phone's manifest, as it was"
    );
    for table in [
        "storage_intents",
        "storage_write_inflight",
        "storage_write_times",
    ] {
        assert_eq!(
            world.count(&format!("SELECT count(*) FROM {table}")),
            0,
            "{table}"
        );
    }
    assert_eq!(
        world.count(
            "SELECT count(*) FROM jobs WHERE kind IN ('storage_write', 'fetch_latest_sync')"
        ),
        0,
        "no write job"
    );
    let view = world.wait_phone("the phone's state", |_| true).await;
    assert!(view.contact(ada).marked_unread, "the phone's mark stays");
    assert!(view.contact(bruno).archived, "the phone's archive stays");
    assert_eq!(view.orphans, 0);

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 12. An archive after the phone brought the chat back is written (H2): the phone's record of
/// Ada's chat says archived; a message brings the chat back HERE (A1: nothing is written for
/// that); the user archives it again. The click's base is what the core was told (not
/// archived), so it is a change although the record already says archived: ONE request, the
/// record rotated, archived. Mutant: a value equal to the record's is not written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_archive_after_the_phone_brought_the_chat_back_is_written() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = world.spec();
    spec.contacts[0].archived = Some(true);
    let (core, device) = world.link(&spec, false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());
    let before = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record");
    assert!(before.flags.archived, "the phone archived the chat");

    let seen = world.unarchives(&world.ada);
    world
        .message_from(&world.ada, &device, "can we talk later?")
        .await;
    world.wait_unarchives(&world.ada, seen + 1).await;
    click(&core, &World::direct(&world.ada), archive(true)).await;

    let view = world
        .wait_phone("the archive written", |v| v.version == Some(2))
        .await;
    assert_eq!(view.contact(ada), &flags(true, false, 0));
    assert_eq!(world.writes().await, (1, 0), "one request for the click");
    let after = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record");
    assert_ne!(after.id, before.id, "the record was rotated by our write");
    assert!(after.flags.archived);
    assert_eq!((view.records, view.orphans), (4, 0));

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 12b. X1, the first test of the decisions' BQ1: the phone's record says archived, a message
/// brings the chat back here (nothing is written for that: the phone keeps the old flag), and
/// the user then mutes it. The write that rotates the record carries the chat's state HERE, not
/// archived, beside the mute: the phone never re-archives a chat it shows in its inbox.
/// Mutant: a rotated record keeps the record's own flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rotated_record_is_written_unarchived_when_a_message_brought_the_chat_back() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = world.spec();
    spec.contacts[0].archived = Some(true);
    let (core, device) = world.link(&spec, false).await;
    world.switch_on(&core).await;
    let ada = &world.ada.device_ref;

    let seen = world.unarchives(&world.ada);
    world
        .message_from(&world.ada, &device, "did you see the forecast?")
        .await;
    world.wait_unarchives(&world.ada, seen + 1).await;
    // The bring-back alone writes nothing: the phone's record says archived.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(world.writes().await, (0, 0));
    let view = world.wait_phone("the phone's state", |_| true).await;
    assert!(view.contact(ada).archived, "the phone's flag is as it was");

    let hour = now_ms() + 3_600_000;
    click(&core, &World::direct(&world.ada), mute(Some(hour))).await;
    let view = world
        .wait_phone("the mute written", |v| {
            v.contacts.get(ada).is_some_and(|c| c.muted_until_ms() != 0)
        })
        .await;
    assert_eq!(
        view.contact(ada),
        &flags(false, false, hour as u64),
        "muted, and no longer archived: the chat is in the inbox here"
    );
    assert_eq!(world.writes().await, (1, 0));
    assert_eq!(view.orphans, 0);

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 12c. X1, the second test of the decisions' BQ1: the user archives Ada's chat and a message
/// arrives BEFORE the write job has run (the job's credentials are held back for four seconds,
/// so the order is certain): the message takes the archive back, the Archive intent is gone,
/// and the write that carries the user's next change of the chat (a mute) says archived =
/// false. Mutant: a message leaves the Archive intent in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_before_the_job_runs_takes_the_archive_back() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let ada = &world.ada.device_ref;
    world
        .harness
        .hold_request("GET", "/v1/storage/auth", 4_000, 1)
        .await
        .expect("hold the job's credentials");

    let seen = world.unarchives(&world.ada);
    click(&core, &World::direct(&world.ada), archive(true)).await;
    world
        .message_from(&world.ada, &device, "see you at the wall?")
        .await;
    world.wait_unarchives(&world.ada, seen + 1).await;
    assert_eq!(
        world.count("SELECT count(*) FROM storage_intents WHERE field = 'archived'"),
        0,
        "the message took the Archive intent back"
    );
    assert_eq!(world.writes().await, (0, 0), "the job has not written yet");

    let hour = now_ms() + 3_600_000;
    click(&core, &World::direct(&world.ada), mute(Some(hour))).await;
    let view = world
        .wait_phone("the mute written", |v| {
            v.contacts.get(ada).is_some_and(|c| c.muted_until_ms() != 0)
        })
        .await;
    assert_eq!(
        view.contact(ada),
        &flags(false, false, hour as u64),
        "muted, not archived"
    );
    assert_eq!(view.orphans, 0);

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 12d. The A10a review's F3: the user reads a chat the phone marked (the core's durable
/// clear) and marks it again at once. The clear came first and the mark is the later word, so
/// the phone's record ends MARKED (a mute in the same batch makes the write certain).
/// Mutant: the older clear overtakes the newer click.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mark_clicked_right_after_the_reading_is_the_word_that_stands() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = world.spec();
    spec.contacts[0].marked_unread = Some(true);
    let (core, _device) = world.link(&spec, false).await;
    world.switch_on(&core).await;
    let ada = &world.ada.device_ref;
    let hour = now_ms() + 3_600_000;

    durable(&core, 51, &World::direct(&world.ada), mark(false)).await;
    click(&core, &World::direct(&world.ada), mark(true)).await;
    click(&core, &World::direct(&world.ada), mute(Some(hour))).await;
    world
        .sink
        .wait_event_value("CommandPersisted", |v| v["cmd"] == 51)
        .await;
    let view = world
        .wait_phone("the mute written", |v| {
            v.contacts.get(ada).is_some_and(|c| c.muted_until_ms() != 0)
        })
        .await;
    assert_eq!(
        view.contact(ada),
        &flags(false, true, hour as u64),
        "the later mark stands over the reading's clear"
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 13. Records of kinds we never merge survive our write (the critic's L8): the phone also keeps
/// a story distribution list, a call link, a chat folder and a notification profile. A write of
/// ours (an archive, a pin: the contact and the account record are rotated) leaves each of the
/// four identifiers in the manifest, with its type, and each item on the server untouched: the
/// phone still reads all eight records, nothing is an orphan, nothing is missing. Mutant: the
/// manifest is built from the records we stored (see test 3, which kills it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn records_of_kinds_we_never_merge_survive() {
    let Some(world) = World::start().await else {
        return;
    };
    let mut spec = world.spec();
    spec.others = [
        "storyDistributionList",
        "callLink",
        "chatFolder",
        "notificationProfile",
    ]
    .map(str::to_owned)
    .to_vec();
    let (core, _device) = world.link(&spec, false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());
    let others = |ids: &[support::harness::StorageIdentifier]| -> std::collections::BTreeSet<_> {
        ids.iter()
            .filter(|i| matches!(i.kind, 5 | 7 | 8 | 9))
            .map(|i| (i.kind, i.id.clone()))
            .collect()
    };
    let before = world
        .harness
        .storage_identifiers(phone)
        .await
        .expect("the manifest");
    assert_eq!(
        before.ids.len(),
        8,
        "the account, three contacts, four others"
    );
    assert_eq!(others(&before.ids).len(), 4, "one of each kind");
    assert_eq!(before.unreadable, 0);

    click(&core, &World::direct(&world.ada), archive(true)).await;
    click(&core, &World::direct(&world.bruno), pin(true)).await;
    let view = world
        .wait_phone("both changes written", |v| {
            v.contacts.get(ada).is_some_and(|c| c.archived) && v.pins.len() == 1
        })
        .await;
    assert_eq!(view.records, 8, "the phone reads every record it has");
    let after = world
        .harness
        .storage_identifiers(phone)
        .await
        .expect("the manifest");
    assert_eq!(after.version, Some(2));
    assert_eq!(
        others(&after.ids),
        others(&before.ids),
        "the four other records are listed as they were, type included"
    );
    assert_eq!(after.ids.len(), 8);
    assert_eq!(
        (after.items, after.orphans, after.missing, after.unreadable),
        (8, 0, 0, 0),
        "every identifier has its item, readable, and no item is left over"
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 14. The identity state travels with a contact write (P5a): Mark as Verified on Ada, whose
/// record holds the key Kourier shows, is carried by the next write as `identityState` beside
/// the key (the record's other fields and the key bytes stay), and no Verified sync message is
/// queued (the write is the one route); Clear Verification writes `default` again. Mutant: the
/// plan leaves the state out of the record.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_identity_state_travels_with_a_contact_write() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, _device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());
    let number = World::safety_number(&core, &world.ada).await;
    let view = world.wait_phone("the phone's state", |_| true).await;
    assert_eq!(
        view.identity_states.get(ada).map(String::as_str),
        Some("default")
    );

    World::set_verified(&core, &world.ada, true, &number.key).await;
    let view = world
        .wait_phone("Ada verified", |v| {
            v.identity_states.get(ada).map(String::as_str) == Some("verified")
        })
        .await;
    assert_eq!(
        view.contact(ada),
        &flags(false, false, 0),
        "only the state changed"
    );
    let record = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record");
    let key = &number.key.0;
    assert!(
        record
            .bytes()
            .windows(key.len())
            .any(|w| w == key.as_slice()),
        "the record still holds the key bytes the user was shown"
    );
    assert_eq!(world.writes().await, (1, 0));
    assert_eq!(
        world.count("SELECT count(*) FROM jobs WHERE kind = 'verified_sync'"),
        0,
        "the write is the one route for a key the record holds"
    );

    World::set_verified(&core, &world.ada, false, &number.key).await;
    world
        .wait_phone("Ada cleared", |v| {
            v.identity_states.get(ada).map(String::as_str) == Some("default")
        })
        .await;
    assert_eq!(world.writes().await, (2, 0));

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// 14b. A record with ANOTHER key keeps the phone's key bytes: Ada's record holds her old key;
/// Kourier has learned her new one (a profile fetch). The user marks the new key verified and
/// mutes the chat. The write rotates the record for the mute but never states the new key's
/// state onto the old key: the record keeps its key bytes and its state, and the phone is told
/// by the Verified sync message instead (the same state, the new key). Mutant: the plan writes
/// the state onto a record that holds another key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_with_another_key_keeps_the_phones_key_and_the_phone_hears_by_sync() {
    let Some(world) = World::start().await else {
        return;
    };
    let (core, device) = world.link(&world.spec(), false).await;
    world.switch_on(&core).await;
    let (ada, phone) = (&world.ada.device_ref, world.phone.device_ref.as_str());
    let old = World::safety_number(&core, &world.ada).await;

    // A new install of Ada: her profile answers the new key; the phone's record has the old one.
    let new_key = world
        .harness
        .replace_identity_key(ada)
        .await
        .expect("a new identity key for Ada");
    world
        .files
        .execute("UPDATE peers SET profile_fetched_at = NULL, profile_tried_at = NULL")
        .expect("allow a fetch");
    core.send(Command::RefreshContact {
        user: World::user(&world.ada),
    })
    .await;
    let deadline = tokio::time::Instant::now() + WAIT;
    let new = loop {
        let number = World::safety_number(&core, &world.ada).await;
        if number.key != old.key {
            break number;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "Kourier never learned Ada's new key"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        STANDARD.encode(&new.key.0) == new_key,
        "the key Kourier shows is the one the server answers"
    );

    World::set_verified(&core, &world.ada, true, &new.key).await;
    let hour = now_ms() + 3_600_000;
    click(&core, &World::direct(&world.ada), mute(Some(hour))).await;
    let view = world
        .wait_phone("the mute written", |v| {
            v.contacts.get(ada).is_some_and(|c| c.muted_until_ms() != 0)
        })
        .await;
    assert_eq!(view.contact(ada), &flags(false, false, hour as u64));
    assert_eq!(
        view.identity_states.get(ada).map(String::as_str),
        Some("default"),
        "the new key's state is not written onto the old key"
    );
    let record = world
        .harness
        .raw_storage_record(phone, StorageSubject::Contact(ada))
        .await
        .expect("Ada's record");
    let bytes = record.bytes();
    let (old_key, new_key_bytes) = (&old.key.0, &new.key.0);
    assert!(
        bytes
            .windows(old_key.len())
            .any(|w| w == old_key.as_slice()),
        "the phone's key bytes are kept"
    );
    assert!(
        !bytes
            .windows(new_key_bytes.len())
            .any(|w| w == new_key_bytes.as_slice()),
        "our key is not put into the phone's record"
    );

    // The phone hears of the state by the sync message, with the key it was marked for.
    let told = world
        .harness
        .wait_for_sync_message(
            &world.phone.device_ref,
            Some(&device.device_ref),
            "verified",
            WAIT,
        )
        .await
        .expect("a Verified sync reached the phone");
    let verified = &told.sync_message["content"]["verified"];
    assert_eq!(verified["state"].as_u64().unwrap_or(0), 1, "verified");
    assert!(
        verified["identityKey"].as_str() == Some(new_key.as_str()),
        "the sync names the key that was marked"
    );

    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

// Not written here, on purpose (the lead's speed rule: a behaviour a unit test proves is not
// proved a second time over the wire):
// - 15, a reset during a write drops its result: `storage::write::job::tests::
//   a_reset_during_the_put_drops_its_result` runs the real job over a scripted Storage Service
//   that holds the PUT, calls `storage::reset` in its middle and checks the 200's zone (no
//   rows stored, the intents kept, a sync asked for, no fetch-latest);
//   `storage::intents::tests::a_reset_drops_the_landing` pins the landing itself. The mock
//   cannot send a `SyncMessage.Keys` (no harness command makes the new storage key).
// - the 409 back-off through the mock: test 5 (`a_conflict_is_merged_and_written_again`) is the
//   conflict through `conflictNextStorageWrite`: one 409, the merge, the write lands, the server
//   counts (1, 1); `a_conflict_repeats_later_without_sleeping`,
//   `three_conflicts_in_a_row_count_up_and_a_landing_starts_over` and
//   `a_click_during_the_conflict_back_off_sends_no_put_before_it_ends` (job tests) pin that the
//   runner returns `RepeatAt` and sleeps nowhere, and what a click does meanwhile.
