//! S9 against `@signalapp/mock-server` (docs/architecture.md §6.9-§6.11, §11 S9): the phone's
//! contact sync blob (downloaded from the CDN and decrypted), its Storage Service state (names,
//! profile keys, blocked, pinned, archived, muted, settings), profiles with avatars decrypted
//! into the blob store, `FetchLatest{STORAGE_MANIFEST}` resyncs, a missing manifest creates
//! nothing, and a kill between two merge zones loses no record.
//!
//! The backend runs as the app runs it (`SignalNetwork::spawn`); the test plays the core
//! (`support::fake_core`). Skips without Node ≥ 24 or the harness's `node_modules`
//! (`KOURIER_REQUIRE_MOCK_HARNESS=1` fails instead).

use crate::support;

use std::collections::BTreeSet;
use std::os::unix::process::ExitStatusExt as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{
    BackendEnv, Command, Exit, InitialSync, OutgoingKind, StartMode, TransmitOutcome,
};
use kourier_model::{ConvKey, LoginStepKind, MsgTarget, TargetAuthor, UserId};
use kourier_signal::SignalNetwork;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use support::fake_core::{AccountFiles, FakeCore, Sink, WAIT, local_env, text_request};
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
                about: Some("Calls only".to_owned()),
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
        self.contact_of(&Self::user(primary))
    }

    /// The same for a user id.
    fn contact_of(&self, user: &str) -> serde_json::Map<String, Value> {
        let mut merged = serde_json::Map::new();
        for patch in self.sink.event_values("ContactUpsert") {
            if patch["user"] == user
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
    // P1: the about text and its emoji have fields of their own; the v1 blob is gone.
    let ada = world.contact(&world.ada);
    assert_eq!(
        ada.get("about").and_then(set).and_then(Value::as_str),
        Some("Out hiking")
    );
    assert_eq!(
        ada.get("about_emoji").and_then(set).and_then(Value::as_str),
        Some("🥾")
    );
    assert_eq!(
        ada.get("meta"),
        Some(&Value::String("Clear".to_owned())),
        "the old profile blob is cleared by the fetch, not written again"
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

/// People pages P1, the number rule (Desktop's `getE164`): a number is SHOWN when the person shares
/// it, or is a system contact, or when we hold no profile key for them; a profile never fetched
/// counts as not sharing. The verdict `number_shown` reaches the core with the Storage Service
/// record (before any fetch), with the profile fetch (the field `phoneNumberSharing`), and again
/// when a record gives the person an address-book name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_number_is_shown_by_signals_rule_and_the_verdict_follows_every_change() {
    let Some(world) = World::start().await else {
        return;
    };
    let person = |name: &str, sharing: Option<bool>| {
        let world = &world;
        let name = name.to_owned();
        async move {
            world
                .harness
                .create_primary(&CreatePrimary {
                    phone_number_sharing: sharing,
                    ..CreatePrimary::named(name)
                })
                .await
                .expect("person")
        }
    };
    let dora = person("Dora Hidden", Some(false)).await;
    let eli = person("Eli Sharing", Some(true)).await;
    let fay = person("Fay Silent", None).await;
    let gus = person("Gus Booked", Some(false)).await;
    let hal = person("Hal Keyless", Some(false)).await;
    let spec = |dora_system: Option<&str>| StorageSpec {
        primary: world.phone.device_ref.clone(),
        contacts: vec![
            StorageContact {
                system_given_name: dora_system.map(str::to_owned),
                ..StorageContact::of(&dora.device_ref)
            },
            StorageContact::of(&eli.device_ref),
            StorageContact::of(&fay.device_ref),
            StorageContact {
                system_given_name: Some("Gus Gray".to_owned()),
                ..StorageContact::of(&gus.device_ref)
            },
            StorageContact {
                profile_key: Some(false),
                ..StorageContact::of(&hal.device_ref)
            },
        ],
        ..StorageSpec::default()
    };
    // Profile fetches are refused for now: nobody's profile is fetched.
    world
        .harness
        .limit_profile_fetches(Some(2))
        .await
        .expect("limit");
    world
        .harness
        .set_storage_state(&spec(None))
        .await
        .expect("storage state");
    let (core, device) = world.link(InitialSync::ContactsAndStorage).await;
    let shown = |w: &World, p: &Primary| -> Option<bool> {
        w.contact(p).get("number_shown").and_then(Value::as_bool)
    };
    world
        .wait_for("every verdict from the records", |w| {
            [&dora, &eli, &fay, &gus, &hal]
                .iter()
                .all(|p| shown(w, p).is_some())
        })
        .await;
    assert_eq!(
        shown(&world, &dora),
        Some(false),
        "a key, not fetched, not a system contact"
    );
    assert_eq!(
        shown(&world, &eli),
        Some(false),
        "sharing is not known before the fetch"
    );
    assert_eq!(shown(&world, &fay), Some(false));
    assert_eq!(shown(&world, &gus), Some(true), "a system contact shows");
    assert_eq!(shown(&world, &hal), Some(true), "no profile key: shows");

    // The profiles come: Eli says he shares, Dora and Fay (no field) do not, Gus is a system
    // contact whatever his profile says.
    world
        .harness
        .limit_profile_fetches(None)
        .await
        .expect("lift");
    for p in [&dora, &eli, &fay, &gus] {
        core.send(Command::RefreshContact {
            user: UserId::parse(&World::user(p)).expect("user id"),
        })
        .await;
    }
    world
        .wait_for("Eli's profile", |w| shown(w, &eli) == Some(true))
        .await;
    world
        .files
        .wait_for_i64(
            "the four profiles fetched",
            "SELECT count(*) FROM peers WHERE profile_key IS NOT NULL AND profile_fetched_at IS NOT NULL",
            |n| n.is_some_and(|n| n >= 4),
        )
        .await;
    assert_eq!(shown(&world, &dora), Some(false), "does not share");
    assert_eq!(shown(&world, &fay), Some(false), "no field is not sharing");
    assert_eq!(
        shown(&world, &gus),
        Some(true),
        "address book beats the flag"
    );

    // A record that gives Dora an address-book name states the verdict again.
    world
        .harness
        .set_storage_state(&spec(Some("Dora D")))
        .await
        .expect("storage state 2");
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
        .wait_for("Dora in the address book", |w| {
            shown(w, &dora) == Some(true)
        })
        .await;
    drop(core);
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

/// People pages P1: the page asks for a fresh profile when it opens (`RefreshContact`), and the
/// backend answers at most once a day per person, whatever the page does (his "no mass fetch"
/// rule); the first fetch after the link also brought OUR OWN about text (BQ3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_page_refresh_is_one_profile_request_a_day_and_our_own_about_comes_too() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .set_storage_state(&world.storage())
        .await
        .expect("storage state");
    let (core, device) = world.link(InitialSync::ContactsAndStorage).await;
    // Everyone's first fetch is done, ours too (the routine refresh queues it).
    world
        .files
        .wait_for_i64(
            "the three contacts' profiles",
            "SELECT count(*) FROM peers WHERE profile_key IS NOT NULL AND profile_fetched_at IS NOT NULL",
            |n| n.is_some_and(|n| n >= 3),
        )
        .await;
    let own = format!("aci:{}", device.aci);
    world
        .wait_for("our own about text", |w| {
            w.contact_of(&own).get("about").and_then(set).is_some()
        })
        .await;
    assert_eq!(
        world.contact_of(&own)["about"]["Set"],
        serde_json::json!("Calls only"),
        "our own profile's about text"
    );
    assert!(
        world.contact_of(&own).get("avatar").is_none(),
        "our own picture is the account's, not a contact's"
    );

    // From now on every request that reaches the server is counted (and refused, so a job that
    // does run shows).
    assert_eq!(
        world
            .harness
            .limit_profile_fetches(Some(600))
            .await
            .expect("limit"),
        0
    );
    let ada = UserId::parse(&World::user(&world.ada)).expect("user id");
    core.send(Command::RefreshContact { user: ada.clone() })
        .await;
    core.send(Command::RefreshContact { user: ada.clone() })
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        world
            .files
            .peek_i64("SELECT count(*) FROM jobs WHERE kind = 'profile_fetch'")
            .unwrap(),
        Some(0),
        "no job for a profile fetched a moment ago"
    );
    assert_eq!(
        world
            .harness
            .limit_profile_fetches(Some(600))
            .await
            .expect("count"),
        0,
        "and no request reached the server"
    );

    // A day later the page's ask goes through, once, however often it is repeated.
    world
        .files
        .execute(&format!(
            "UPDATE peers SET profile_fetched_at = 1 WHERE service_id = '{}'",
            world.ada.aci
        ))
        .expect("an old fetch");
    core.send(Command::RefreshContact { user: ada.clone() })
        .await;
    core.send(Command::RefreshContact { user: ada }).await;
    let harness = &world.harness;
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if harness
            .limit_profile_fetches(Some(600))
            .await
            .expect("count")
            >= 1
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the due request never reached the server"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Encodes the `AttachmentPointer` fields the backend reads (field numbers of
/// `SignalService.proto`): `cdnKey` 15, `key` 3, `size` 4, `digest` 6, `contentType` 2.
/// A1 (BQ5): a message of Kourier's own to a chat the phone archived (and muted: its "Keep
/// muted chats archived" is off) brings the chat back to the inbox, once, in the one zone that
/// moves the diff base; a second message finds it there. The send is the real one, to the mock
/// server; the rule itself is tested over one `backend.db` in `storage/tests.rs`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_own_message_brings_an_archived_chat_back() {
    let Some(world) = World::start().await else {
        return;
    };
    world
        .harness
        .set_storage_state(&world.storage())
        .await
        .expect("storage state");
    let (mut core, _device) = world.link(InitialSync::ContactsAndStorage).await;
    core.wait_online().await;
    let chiara = UserId::parse(&World::user(&world.chiara)).expect("user id");
    let key = format!("direct:{}", World::user(&world.chiara));
    assert_eq!(
        world.chat(&world.chiara).get("archived"),
        Some(&Value::Bool(true)),
        "the phone archived the chat"
    );
    let unarchives = |world: &World| {
        world
            .sink
            .event_values("ConversationUpsert")
            .iter()
            .filter(|p| p["key"] == key.as_str() && p["archived"] == Value::Bool(false))
            .count()
    };
    assert_eq!(unarchives(&world), 0);
    let (first_at, outcome) = core.transmit_text(&chiara, "hello from Kourier").await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    world
        .wait_for("the chat to come back", |w| unarchives(w) == 1)
        .await;
    let (_, outcome) = core.transmit_text(&chiara, "and again").await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(unarchives(&world), 1, "the second message says nothing");
    // Archived again on the phone (its base, as the Storage merge writes it): a reaction is
    // no new message, so the chat stays where the phone put it.
    let archived_peers = format!(
        "UPDATE peers SET roaming_archived = 1 WHERE service_id = '{}'",
        world.chiara.aci
    );
    world.files.execute(&archived_peers).expect("archive again");
    let mut react = text_request(&chiara, "unused");
    react.kind = OutgoingKind::React {
        target: MsgTarget {
            conv: Some(ConvKey::Direct(chiara.clone())),
            author: TargetAuthor::Own,
            local: first_at.to_string().into(),
            alternates: Vec::new(),
        },
        emoji: "👍".to_owned(),
        remove: false,
    };
    let outcome = core.transmit(react).await;
    assert!(
        matches!(outcome, TransmitOutcome::Sent { .. }),
        "{outcome:?}"
    );
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(unarchives(&world), 1, "a reaction brings nothing back");
    assert_eq!(
        world
            .files
            .peek_i64(&format!(
                "SELECT roaming_archived FROM peers WHERE service_id = '{}'",
                world.chiara.aci
            ))
            .unwrap(),
        Some(1),
        "and leaves the base as the phone has it"
    );
    assert_eq!(
        world.chat(&world.chiara).get("archived"),
        Some(&Value::Bool(false))
    );
    assert_eq!(core.shutdown().await, Exit::Shutdown);
    world.harness.shutdown().await.expect("harness shutdown");
}

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
        net_ids: Vec::new().into(),
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
