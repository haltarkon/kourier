//! S10 through the real core (docs/architecture.md §6.8, §11 S10): a group on the phone's
//! Storage Service reaches `core.db` as a group conversation with its title, members and avatar;
//! a member's message appears in the group timeline with its author; and a member removal shows a
//! system message. Exercises the real core ingest of group `ConversationUpsert`s and group
//! `Message`s (the `kourier-signal` tests use a fake core instead).
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
use kourier_model::{Body, ConvKind, SystemEvent};
use support::harness::{
    CreateGroup, CreatePrimary, Harness, ModifyGroup, SendGroupText, StorageGroup, StorageSpec,
};
use support::{App, WAIT, World, init_tracing, local_env, user};

const GROUP_AVATAR: &[u8] = b"\x89PNG\r\n\x1a\nClimbing club avatar";

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

async fn group_rows(app: &App, account: kourier_model::AccountId) -> Vec<ConversationRow> {
    let query = ConversationListQuery {
        account: Some(account),
        limit: 100,
        ..ConversationListQuery::default()
    };
    match app
        .handle
        .query(Query::Conversations(query))
        .await
        .expect("conversations")
    {
        QueryData::Conversations(page) => page
            .rows
            .into_iter()
            .filter(|r| matches!(r.kind, ConvKind::Group))
            .collect(),
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_shows_title_members_avatar_messages_and_a_removal() {
    init_tracing();
    let Some(harness) = Harness::start_or_skip().await.expect("mock harness") else {
        return;
    };
    let tmp = tempfile::tempdir().expect("temp dir");
    let env = local_env(harness.handshake(), tmp.path());
    let ada = harness
        .create_primary(&CreatePrimary::named("Ada Lindqvist"))
        .await
        .expect("Ada");
    let bruno = harness
        .create_primary(&CreatePrimary::named("Bruno Okafor"))
        .await
        .expect("Bruno");
    let phone = harness
        .create_primary(&CreatePrimary::named("Demo Phone"))
        .await
        .expect("phone");
    let group = harness
        .create_group(&CreateGroup {
            from: phone.device_ref.clone(),
            title: "Climbing club".to_owned(),
            members: vec![ada.device_ref.clone(), bruno.device_ref.clone()],
            avatar: Some(STANDARD.encode(GROUP_AVATAR)),
            ..CreateGroup::default()
        })
        .await
        .expect("create group");
    harness
        .set_storage_state(&StorageSpec {
            primary: phone.device_ref.clone(),
            groups: vec![StorageGroup {
                id: group.id.clone(),
                ..StorageGroup::default()
            }],
            ..StorageSpec::default()
        })
        .await
        .expect("storage state");
    let world = World {
        harness,
        tmp,
        env,
        phone: phone.clone(),
        alice: ada.clone(),
    };

    let mut app = App::start_with(
        world.tmp.path(),
        &world.env,
        InitialSync::ContactsAndStorage,
    )
    .await;
    let (account, device) = app.link(&world).await;

    // The group conversation reaches the core with its title, members and avatar.
    let deadline = tokio::time::Instant::now() + WAIT;
    let row = loop {
        let rows = group_rows(&app, account).await;
        if let Some(row) = rows
            .into_iter()
            .find(|r| r.title.as_deref() == Some("Climbing club") && r.avatar.is_some())
        {
            break row;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the group never fully arrived"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let conv = row.id;
    assert!(row.peer.is_none(), "a group has no single peer");

    // Its members: us, Ada and Bruno.
    let detail = match app
        .handle
        .query(Query::Conversation(conv))
        .await
        .expect("detail")
    {
        QueryData::Conversation(Some(detail)) => detail,
        other => panic!("unexpected {other:?}"),
    };
    let members: Vec<String> = detail
        .members
        .iter()
        .map(|m| m.user.as_str().to_owned())
        .collect();
    for peer in [&ada, &bruno] {
        assert!(
            members.contains(&user(peer).as_str().to_owned()),
            "{} is a member: {members:?}",
            peer.profile_name
        );
    }
    // The avatar is an encrypted file at rest.
    let file =
        find_file(world.tmp.path(), &row.avatar.as_ref().unwrap().path).expect("group avatar file");
    let bytes = std::fs::read(&file).expect("avatar");
    assert!(bytes.starts_with(b"KOURBLOB"), "encrypted blob at rest");

    // A member's message arrives in the group with its author.
    let sent = world
        .harness
        .send_group_text(&SendGroupText {
            from: ada.aci.clone(),
            group: group.id.clone(),
            to: vec![device.device_ref.clone()],
            text: "who is coming on Saturday?".to_owned(),
            ..SendGroupText::default()
        })
        .await
        .expect("group text");
    let rows = app.wait_text(conv, "who is coming on Saturday?").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].author.as_ref(),
        Some(&user(&ada)),
        "authored by Ada"
    );
    assert_eq!(rows[0].sent_at, i64::try_from(sent.timestamp).unwrap());

    // The phone removes Bruno; a system message appears.
    world
        .harness
        .modify_group(&ModifyGroup {
            from: phone.device_ref.clone(),
            group: group.id.clone(),
            remove_members: vec![bruno.device_ref.clone()],
            to: vec![device.device_ref.clone()],
            ..ModifyGroup::default()
        })
        .await
        .expect("remove Bruno");

    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let rows = app.rows(conv).await;
        let removed = rows.iter().any(|r| {
            matches!(&r.body, Body::System(SystemEvent::MembersRemoved { users, .. })
                if users.iter().any(|u| *u == user(&bruno)))
        });
        if removed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no removal system message: {:?}",
            app.rows(conv)
                .await
                .iter()
                .map(|r| r.body.kind_name())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Bruno is no longer a member of the conversation.
    let after = match app
        .handle
        .query(Query::Conversation(conv))
        .await
        .expect("detail")
    {
        QueryData::Conversation(Some(detail)) => detail,
        other => panic!("unexpected {other:?}"),
    };
    assert!(
        !after.members.iter().any(|m| m.user == user(&bruno)),
        "Bruno was removed from the member list"
    );

    app.stop().await;
    world.shutdown().await;
}
