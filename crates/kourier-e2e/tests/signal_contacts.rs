//! S9 through the real core (docs/architecture.md §6.9-§6.11, §11 S9): contacts set on the mock
//! phone (address-book names in its contact sync, profile keys, blocked, pinned and archived
//! flags in its Storage Service state) reach `core.db`; the conversation list shows them with
//! their names and avatar files, and the phone's settings and pins apply. The link uses the app's
//! `InitialSync::ContactsAndStorage`, so `Done` waits for both syncs.
//!
//! Skips without Node ≥ 24 or the harness's `node_modules` (`KOURIER_REQUIRE_MOCK_HARNESS=1`
//! fails instead).

mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::InitialSync;
use kourier_core::facade::{ConversationListQuery, ConversationRow, Query, QueryData};
use kourier_model::AccountId;
use support::harness::{CreatePrimary, Harness, StorageAccount, StorageContact, StorageSpec};
use support::{App, WAIT, World, init_tracing, local_env, user};

/// The avatars the contacts' profiles carry (the core stores files; it never decodes them here).
const ADA_AVATAR: &[u8] = b"\x89PNG\r\n\x1a\nAda";
const BRUNO_AVATAR: &[u8] = b"\x89PNG\r\n\x1a\nBruno";

async fn rows(app: &App, account: AccountId, archived: bool) -> Vec<ConversationRow> {
    let query = ConversationListQuery {
        account: Some(account),
        archived,
        limit: 100,
        ..ConversationListQuery::default()
    };
    match app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    {
        QueryData::Conversations(page) => page.rows,
        other => panic!("unexpected {other:?}"),
    }
}

/// Finds `relative` below `root` (the account's blob directory lives under the profile).
fn find_file(root: &Path, relative: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.join(relative).is_file() {
                return Some(path.join(relative));
            }
            if let Some(found) = find_file(&path, relative) {
                return Some(found);
            }
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contacts_from_the_phone_appear_with_names_avatars_pins_and_blocks() {
    init_tracing();
    let Some(harness) = Harness::start_or_skip().await.expect("mock harness") else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let env = local_env(harness.handshake(), tmp.path());
    let primary = |name: &str, avatar: Option<&[u8]>| CreatePrimary {
        avatar: avatar.map(|a| STANDARD.encode(a)),
        ..CreatePrimary::named(name)
    };
    let ada = harness
        .create_primary(&primary("Ada Lindqvist", Some(ADA_AVATAR)))
        .await
        .expect("Ada");
    let bruno = harness
        .create_primary(&primary("Bruno Okafor", Some(BRUNO_AVATAR)))
        .await
        .expect("Bruno");
    let chiara = harness
        .create_primary(&primary("Chiara Velasco", None))
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
    harness
        .set_storage_state(&StorageSpec {
            primary: phone.device_ref.clone(),
            contacts: vec![
                StorageContact {
                    pinned: Some(true),
                    ..StorageContact::of(&ada.device_ref)
                },
                StorageContact {
                    blocked: Some(true),
                    whitelisted: Some(false),
                    ..StorageContact::of(&bruno.device_ref)
                },
                StorageContact {
                    archived: Some(true),
                    ..StorageContact::of(&chiara.device_ref)
                },
            ],
            account: Some(StorageAccount {
                read_receipts: Some(true),
                typing_indicators: Some(true),
                ..StorageAccount::default()
            }),
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    let world = World {
        harness,
        tmp,
        env,
        phone,
        alice: ada.clone(),
    };

    let mut app = App::start_with(
        world.tmp.path(),
        &world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, _device) = app.link(&world).await;

    // Right after Done: the pinned and the blocked chat, with the address-book names.
    let inbox = rows(&app, account, false).await;
    let row = |peer: &support::harness::Primary| {
        let id = user(peer);
        inbox
            .iter()
            .find(|r| r.peer.as_ref() == Some(&id))
            .cloned()
            .unwrap_or_else(|| panic!("no chat for {}: {inbox:?}", peer.profile_name))
    };
    let ada_row = row(&ada);
    assert_eq!(ada_row.title.as_deref(), Some("Ada Lindqvist"));
    assert_eq!(ada_row.pinned_order, Some(0));
    assert_eq!(inbox[0].peer, ada_row.peer, "pinned first");
    let bruno_row = row(&bruno);
    assert_eq!(bruno_row.title.as_deref(), Some("Bruno Okafor"));
    assert!(bruno_row.blocked);
    let archived = rows(&app, account, true).await;
    let chiara_row = archived
        .iter()
        .find(|r| r.peer.as_ref() == Some(&user(&chiara)))
        .expect("Chiara's chat is archived");
    assert_eq!(chiara_row.title.as_deref(), Some("Chiara Velasco"));

    // The profiles follow: avatar files in the account's blob directory.
    let deadline = tokio::time::Instant::now() + WAIT;
    let (ada_avatar, bruno_avatar) = loop {
        let inbox = rows(&app, account, false).await;
        let avatar = |peer: &support::harness::Primary| {
            inbox
                .iter()
                .find(|r| r.peer.as_ref() == Some(&user(peer)))
                .and_then(|r| r.avatar.clone())
        };
        if let (Some(a), Some(b)) = (avatar(&ada), avatar(&bruno)) {
            break (a, b);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "avatars never arrived: {inbox:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_ne!(ada_avatar.path, bruno_avatar.path);
    for avatar in [&ada_avatar, &bruno_avatar] {
        let file = find_file(world.tmp.path(), &avatar.path).expect("the avatar file exists");
        let bytes = std::fs::read(&file).expect("avatar file");
        assert!(
            bytes.starts_with(b"KOURBLOB"),
            "a kourier-blob file, never the plain image"
        );
        assert!(
            !bytes.windows(4).any(|w| w == b"\x89PNG"),
            "the image is encrypted at rest"
        );
    }
    app.stop().await;
    world.shutdown().await;
}
