//! S9 against `@signalapp/mock-server` (docs/architecture.md §6.9-§6.11, §11 S9): the phone's
//! contact sync blob (downloaded from the CDN and decrypted), its Storage Service state (names,
//! profile keys, blocked, pinned, archived, muted, settings), profiles with avatars decrypted
//! into the blob store, `FetchLatest{STORAGE_MANIFEST}` resyncs, a missing manifest creates
//! nothing, and a kill between two merge zones loses no record.
//!
//! The backend runs as the app runs it (`SignalNetwork::spawn`); the test plays the core
//! (`support::fake_core`). Skips without Node ≥ 24 or the harness's `node_modules`
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

mod support;

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{BackendEnv, Command, Exit, InitialSync, StartMode};
use kourier_model::LoginStepKind;
use kourier_signal::SignalNetwork;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env};
use support::harness::{
    CreatePrimary, DeviceInfo, Harness, Primary, StorageAccount, StorageContact, StorageSpec,
};
use tokio::io::{AsyncBufReadExt as _, BufReader};

/// Set (to a JSON [`ChildSpec`]) for the helper process of the kill test.
const CHILD_ENV: &str = "KOURIER_SIGNAL_S9_CHILD";
/// Prefix of the helper's protocol lines on stdout.
const CHILD_LINE: &str = "KOURIER-S9-CHILD";
/// Synthetic contacts of the kill test: more than one merge zone (500 records).
const EXTRA_CONTACTS: u32 = 620;

/// Two tiny avatars (the backend stores bytes; it never decodes images).
const ADA_AVATAR: &[u8] = b"\x89PNG\r\n\x1a\nAda's avatar";
const BRUNO_AVATAR: &[u8] = b"\x89PNG\r\n\x1a\nBruno's avatar, a bit longer";

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
}

/// The phone ("Demo Phone") with three contacts in its contact sync blob.
struct World {
    harness: Harness,
    tmp: tempfile::TempDir,
    env: BackendEnv,
    phone: Primary,
    ada: Primary,
    bruno: Primary,
    chiara: Primary,
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
            .create_primary(&CreatePrimary {
                avatar: Some(STANDARD.encode(ADA_AVATAR)),
                about: Some("Out hiking".to_owned()),
                about_emoji: Some("🥾".to_owned()),
                ..CreatePrimary::named("Ada Lindqvist")
            })
            .await
            .expect("Ada");
        let bruno = harness
            .create_primary(&CreatePrimary {
                avatar: Some(STANDARD.encode(BRUNO_AVATAR)),
                ..CreatePrimary::named("Bruno Okafor")
            })
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
            sink,
            files,
        })
    }

    fn user(primary: &Primary) -> String {
        format!("aci:{}", primary.aci)
    }

    /// Links a fresh account; `initial_sync` decides what `Done` waits for.
    async fn link(&self, initial_sync: InitialSync) -> (FakeCore, DeviceInfo) {
        let mut core = FakeCore::start_full(
            SignalNetwork::new(),
            &self.env,
            &self.files,
            StartMode::Fresh,
            &self.sink,
            initial_sync,
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
        (core, device.expect("linked device"))
    }

    /// The phone's storage state of the S9 Done-when: names, profile keys, blocked, pinned.
    fn storage(&self) -> StorageSpec {
        StorageSpec {
            primary: self.phone.device_ref.clone(),
            contacts: vec![
                StorageContact {
                    pinned: Some(true),
                    ..StorageContact::of(&self.ada.device_ref)
                },
                StorageContact {
                    blocked: Some(true),
                    whitelisted: Some(false),
                    ..StorageContact::of(&self.bruno.device_ref)
                },
                StorageContact {
                    archived: Some(true),
                    marked_unread: Some(true),
                    muted_until: Some(4_102_444_800_000),
                    ..StorageContact::of(&self.chiara.device_ref)
                },
            ],
            account: Some(StorageAccount {
                read_receipts: Some(true),
                typing_indicators: Some(true),
                phone_number_sharing: Some("nobody".to_owned()),
                universal_expire_timer: Some(86_400),
                ..StorageAccount::default()
            }),
            ..StorageSpec::default()
        }
    }

    /// The contact patches the core got for `primary`, merged field by field in order.
    fn contact(&self, primary: &Primary) -> serde_json::Map<String, Value> {
        let user = Self::user(primary);
        let mut merged = serde_json::Map::new();
        for patch in self.sink.event_values("ContactUpsert") {
            if patch["user"] == user.as_str()
                && let Value::Object(fields) = patch
            {
                merged.extend(fields);
            }
        }
        merged
    }

    /// The direct-chat patches for `primary`, merged.
    fn chat(&self, primary: &Primary) -> serde_json::Map<String, Value> {
        let key = format!("direct:{}", Self::user(primary));
        let mut merged = serde_json::Map::new();
        for patch in self.sink.event_values("ConversationUpsert") {
            if patch["key"] == key.as_str()
                && let Value::Object(fields) = patch
            {
                merged.extend(fields);
            }
        }
        merged
    }

    async fn wait_for(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = tokio::time::Instant::now() + WAIT;
        while !done(self) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn storage_version(&self) -> Option<u64> {
        let result = self
            .harness
            .request_value(
                "storageVersion",
                serde_json::json!({"primary": self.phone.device_ref}),
            )
            .await
            .expect("storageVersion");
        result["version"].as_u64()
    }
}

fn set(value: &Value) -> Option<&Value> {
    value.get("Set")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_storage_and_profiles_reach_the_core_and_fetch_latest_resyncs() {
    let Some(world) = World::start().await else {
        return;
    };
    let written = world
        .harness
        .set_storage_state(&world.storage())
        .await
        .expect("storage state");
    assert_eq!(written.version, 1);

    // `ContactsAndStorage`: `Done` waits for the contact sync and the Storage Service.
    let (mut core, device) = world.link(InitialSync::ContactsAndStorage).await;
    for primary in [&world.ada, &world.bruno, &world.chiara] {
        let contact = world.contact(primary);
        assert_eq!(
            contact
                .get("system_name")
                .and_then(set)
                .and_then(Value::as_str),
            Some(primary.profile_name.as_str()),
            "the contact-sync name arrived before Done"
        );
        assert!(
            contact.contains_key("identifiers"),
            "the number from contact sync"
        );
        assert_eq!(
            contact
                .get("profile_name")
                .and_then(set)
                .and_then(Value::as_str),
            Some(primary.profile_name.as_str()),
            "the Storage Service name arrived before Done"
        );
    }
    let pins = world.sink.event_values("PinnedConversations");
    assert_eq!(
        pins.last(),
        Some(&serde_json::json!([format!(
            "direct:{}",
            World::user(&world.ada)
        )])),
        "Ada is pinned"
    );
    assert_eq!(
        world.contact(&world.bruno).get("blocked"),
        Some(&Value::Bool(true))
    );
    let bruno_chat = world.chat(&world.bruno);
    assert_eq!(bruno_chat.get("blocked"), Some(&Value::Bool(true)));
    let chiara_chat = world.chat(&world.chiara);
    assert_eq!(chiara_chat.get("archived"), Some(&Value::Bool(true)));
    assert_eq!(chiara_chat.get("marked_unread"), Some(&Value::Bool(true)));
    assert_eq!(
        chiara_chat.get("muted_until").and_then(set),
        Some(&serde_json::json!(4_102_444_800_000i64))
    );
    let settings = world.sink.event_values("AccountSettings");
    assert!(
        settings.iter().any(
            |s| s["share_phone_number"] == serde_json::json!({"Set": false})
                && s["universal_timer"].get("Set").is_some()
        ),
        "AccountRecord settings: {settings:?}"
    );
    // A secondary device never writes the manifest.
    assert_eq!(world.storage_version().await, Some(1));

    // Profiles: names decrypted with the profile keys, avatars from CDN0 into the blob store.
    world
        .wait_for("both avatars", |w| {
            [&w.ada, &w.bruno]
                .iter()
                .all(|p| w.contact(p).get("avatar").and_then(set).is_some())
        })
        .await;
    for (primary, image) in [(&world.ada, ADA_AVATAR), (&world.bruno, BRUNO_AVATAR)] {
        let avatar = world.contact(primary)["avatar"]["Set"].clone();
        let path = world
            .files
            .dir
            .join("blobs")
            .join(avatar["path"].as_str().expect("path"));
        let key: [u8; 32] = STANDARD
            .decode(avatar["key"].as_str().expect("key"))
            .expect("base64 key")
            .try_into()
            .expect("32-byte key");
        let plain = kourier_blob::read_all(&path, &kourier_blob::FileKey::from_bytes(key))
            .expect("the avatar decrypts from the blob store");
        assert_eq!(&plain[..], image);
    }
    assert!(
        world.contact(&world.chiara).get("avatar").is_none(),
        "no avatar, none set"
    );
    let meta = world.contact(&world.ada)["meta"]["Set"]["bytes"].clone();
    let meta: Value = serde_json::from_slice(
        &STANDARD
            .decode(meta.as_str().expect("meta bytes"))
            .expect("base64"),
    )
    .expect("profile meta JSON");
    assert_eq!(
        meta,
        serde_json::json!({"about": "Out hiking", "about_emoji": "🥾"})
    );
    world
        .files
        .wait_for_i64(
            "sealed sender enabled by the profile",
            &format!(
                "SELECT sealed_sender FROM peers WHERE service_id = '{}'",
                world.ada.aci
            ),
            |v| v == Some(1),
        )
        .await;
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT blocked FROM peers WHERE service_id = '{}'",
                world.bruno.aci
            ))
            .expect("peers"),
        Some(1)
    );

    // The phone changes its state (legacy record keys this time) and says FetchLatest.
    let mut changed = world.storage();
    changed.contacts[1].blocked = Some(false);
    changed.contacts[1].whitelisted = Some(true);
    changed.contacts[1].pinned = Some(true);
    changed.contacts[2].archived = Some(false);
    changed.record_ikm = Some(false);
    let written = world
        .harness
        .set_storage_state(&changed)
        .await
        .expect("storage state");
    assert_eq!(written.version, 2);
    world
        .harness
        .send_fetch_latest(
            &world.phone.device_ref,
            &device.device_ref,
            "storageManifest",
        )
        .await
        .expect("fetch latest");
    world
        .files
        .wait_for_i64(
            "manifest version 2",
            "SELECT manifest_version FROM account",
            |v| v == Some(2),
        )
        .await;
    world
        .wait_for("the new pins", |w| {
            w.sink.event_values("PinnedConversations").last()
                == Some(&serde_json::json!([
                    format!("direct:{}", World::user(&w.ada)),
                    format!("direct:{}", World::user(&w.bruno)),
                ]))
        })
        .await;
    assert_eq!(
        world.chat(&world.chiara).get("archived"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        world.chat(&world.bruno).get("blocked"),
        Some(&Value::Bool(false))
    );
    assert_eq!(world.storage_version().await, Some(2));
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let records = world.files.count_rows("storage_records").expect("records");
    assert_eq!(
        records, 4,
        "account + three contacts, the old versions deleted"
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_manifest_creates_nothing() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, _device) = world.link(InitialSync::ContactsAndStorage).await;
    core.wait_online().await;
    world
        .files
        .wait_for_i64(
            "storage sync",
            "SELECT storage_synced_at FROM account",
            |v| v.is_some(),
        )
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    assert_eq!(
        world.storage_version().await,
        None,
        "no manifest was created"
    );
    assert_eq!(
        world
            .files
            .peek_i64("SELECT manifest_version FROM account")
            .expect("account"),
        None
    );
    assert_eq!(
        world.files.count_rows("storage_records").expect("records"),
        0
    );
    assert!(world.sink.event_values("PinnedConversations").is_empty());
    // Contact sync still named the contacts.
    assert_eq!(
        world
            .contact(&world.ada)
            .get("system_name")
            .and_then(set)
            .and_then(Value::as_str),
        Some("Ada Lindqvist")
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// Encodes the `AttachmentPointer` fields the backend reads (field numbers of
/// `SignalService.proto`): `cdnKey` 15, `key` 3, `size` 4, `digest` 6, `contentType` 2.
fn pointer_bytes(stored: &Value, digest: &[u8]) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }
    fn bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
        varint(out, (field << 3) | 2);
        varint(out, data.len() as u64);
        out.extend_from_slice(data);
    }
    let mut out = Vec::new();
    bytes(
        &mut out,
        15,
        stored["cdnKey"].as_str().expect("cdnKey").as_bytes(),
    );
    bytes(
        &mut out,
        3,
        &STANDARD
            .decode(stored["key"].as_str().expect("key"))
            .expect("key"),
    );
    varint(&mut out, 4 << 3);
    varint(&mut out, stored["size"].as_u64().expect("size"));
    bytes(&mut out, 6, digest);
    bytes(
        &mut out,
        2,
        stored["contentType"].as_str().expect("type").as_bytes(),
    );
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn attachments_stream_from_the_cdn_into_blob_targets() {
    let Some(world) = World::start().await else {
        return;
    };
    let (mut core, _device) = world.link(InitialSync::LinkOnly).await;
    core.wait_online().await;
    let data: Vec<u8> = (0..300_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let stored = world
        .harness
        .request_value(
            "storeAttachment",
            serde_json::json!({"data": STANDARD.encode(&data), "contentType": "image/jpeg"}),
        )
        .await
        .expect("storeAttachment");
    let digest = STANDARD
        .decode(stored["digest"].as_str().expect("digest"))
        .expect("digest");
    let blobs = world.files.dir.join("blobs");
    let target = |name: &str| kourier_backend_api::BlobTarget {
        path: blobs.join("core").join("ab").join(name),
        key: kourier_model::BlobKey::new([0x42; 32]),
    };
    let download = |id: i64, digest: &[u8], name: &str| Command::DownloadAttachment {
        attachment: kourier_model::AttachmentId(id),
        request: kourier_model::DownloadId(id),
        remote: kourier_model::BackendBlob::new(2, pointer_bytes(&stored, digest)),
        dest: target(name),
    };
    core.send(download(1, &digest, "good")).await;
    let mut wrong = digest.clone();
    wrong[0] ^= 1;
    core.send(download(2, &wrong, "bad")).await;
    let done = world.sink.wait_events("AttachmentDone", 2, WAIT).await;
    assert_eq!(done.len(), 2);
    let results = world.sink.event_values("AttachmentDone");
    let result = |id: i64| {
        results
            .iter()
            .find(|r| r["attachment"] == id)
            .unwrap_or_else(|| panic!("AttachmentDone {id}: {results:?}"))["result"]
            .clone()
    };
    let good = result(1);
    assert_eq!(good["Ok"]["size"], 300_000, "{good}");
    assert_eq!(good["Ok"]["mime"], "image/jpeg");
    let plain = kourier_blob::read_all(
        &target("good").path,
        &kourier_blob::FileKey::from_bytes([0x42; 32]),
    )
    .expect("the downloaded blob decrypts");
    assert_eq!(&plain[..], &data[..]);
    assert!(
        result(2).get("Err").is_some(),
        "a wrong digest fails the download"
    );
    assert!(!target("bad").path.exists() && !blobs.join("core/ab/bad.tmp").exists());
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

/// What the helper process of the kill test runs.
#[derive(Serialize, Deserialize)]
struct ChildSpec {
    dir: PathBuf,
    uuid: String,
    key: Vec<u8>,
    env: BackendEnv,
    sink: PathBuf,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_kill_between_merge_zones_loses_no_record() {
    let Some(world) = World::start().await else {
        return;
    };
    // Linked while the phone has no manifest yet.
    let (mut core, _device) = world.link(InitialSync::LinkOnly).await;
    core.wait_online().await;
    world
        .files
        .wait_for_i64(
            "first storage sync",
            "SELECT storage_synced_at FROM account",
            |v| v.is_some(),
        )
        .await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);

    let mut spec = world.storage();
    spec.extra_contacts = Some(EXTRA_CONTACTS);
    world
        .harness
        .set_storage_state(&spec)
        .await
        .expect("storage state");

    // The next start syncs in a helper process that aborts after the first merge zone.
    let child = ChildSpec {
        dir: world.files.dir.clone(),
        uuid: world.files.uuid.to_string(),
        key: world.files.key.to_vec(),
        env: world.env.clone(),
        sink: world.tmp.path().join("sink.db"),
    };
    let failpoint = format!(
        "{}:1",
        kourier_signal::failpoints::AFTER_STORAGE_MERGE_COMMIT
    );
    let mut process = tokio::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "child_actor_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
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
    let merged = world.files.count_rows("storage_records").expect("records");
    assert!(
        (1..i64::from(EXTRA_CONTACTS)).contains(&merged),
        "one zone committed before the kill ({merged})"
    );
    assert_eq!(
        world
            .files
            .peek_i64("SELECT manifest_version FROM account")
            .expect("account"),
        None,
        "the version waits for the last zone"
    );

    // Restart: the sync reads the manifest again and fetches only what is missing.
    let mut core =
        FakeCore::start(&world.env, &world.files, StartMode::Resume, &world.sink).expect("start");
    core.send(Command::Connect).await;
    world
        .files
        .wait_for_i64(
            "manifest merged",
            "SELECT manifest_version FROM account",
            |v| v == Some(1),
        )
        .await;
    core.wait_online().await;
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    let synthetic: BTreeSet<String> = world
        .sink
        .event_values("ContactUpsert")
        .into_iter()
        .filter(|p| {
            p["profile_name"]["Set"]
                .as_str()
                .is_some_and(|n| n.starts_with("Contact "))
        })
        .filter_map(|p| p["user"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        synthetic.len(),
        EXTRA_CONTACTS as usize,
        "every record reached the core"
    );
    assert_eq!(
        world.files.count_rows("storage_records").expect("records"),
        i64::from(EXTRA_CONTACTS) + 4
    );
    assert_eq!(world.files.count_rows("core_outbox").expect("outbox"), 0);
    assert_eq!(
        world
            .files
            .count_rows("core_outbox_poison")
            .expect("poison"),
        0
    );
    world.harness.shutdown().await.expect("harness shutdown");
}

/// The helper process of the kill test: resumes the account and connects until the armed
/// failpoint aborts it.
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
        tokio::time::sleep(Duration::from_secs(120)).await;
        println!("{CHILD_LINE} the failpoint did not fire");
        let _ = core.shutdown().await;
    });
}
