//! The archive (A1, 2026-10-19): the core's list reads the inbox, the archive or both.
//!
//! The viewmodel asks for BOTH in one list (`archived: None`) and splits it by each row's flag,
//! so search, the quick switch and the archive's count work from one snapshot and a chat that
//! moves between the two is in exactly one of them. The older readers keep the inbox
//! (`ConversationListQuery::default()`).

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{ConversationListQuery, ConversationRow};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::remote;
use kourier_model::{ConvKey, ConversationPatch, MsgIdScope, NetworkKind, UserId};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";
const CAROL: &str = "aci:00000000-0000-4000-8000-00000000ca01";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn direct(s: &str) -> ConvKey {
    ConvKey::Direct(uid(s))
}

/// Who a row is: `alice`, `bob` or `carol`.
fn who(r: &ConversationRow) -> &'static str {
    match r.peer.as_ref().map(UserId::as_str) {
        Some(ALICE) => "alice",
        Some(BOB) => "bob",
        _ => "carol",
    }
}

/// Alice (the oldest chat) is archived, Bob and Carol are in the inbox; the list orders them
/// Carol, Bob, Alice by their newest activity.
fn scene() -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    let mut archive_alice = ConversationPatch::new(direct(ALICE));
    archive_alice.archived = Some(true);
    h.apply(vec![
        BackendEvent::Message(remote(&direct(ALICE), &uid(ALICE), "a", 100, "hello")),
        BackendEvent::Message(remote(&direct(BOB), &uid(BOB), "b", 200, "hello")),
        BackendEvent::Message(remote(&direct(CAROL), &uid(CAROL), "c", 300, "hello")),
        BackendEvent::ConversationUpsert(archive_alice),
    ])
    .expect("the three chats");
    (dir, h)
}

fn read(h: &IngestHarness, archived: Option<bool>, limit: u32) -> Vec<&'static str> {
    h.conversations(&ConversationListQuery {
        archived,
        limit,
        ..ConversationListQuery::default()
    })
    .expect("list")
    .rows
    .iter()
    .map(who)
    .collect()
}

/// `Some(false)` is the inbox (and the default), `Some(true)` the archive, `None` both in the
/// list's order; a read of one row at a time that follows the cursor across both returns each
/// chat once, the archived one last.
#[test]
fn the_list_reads_the_inbox_the_archive_or_both() {
    let (_dir, h) = scene();
    assert_eq!(read(&h, Some(false), 50), ["carol", "bob"], "the inbox");
    assert_eq!(read(&h, Some(true), 50), ["alice"], "the archive");
    assert_eq!(
        read(&h, None, 50),
        ["carol", "bob", "alice"],
        "both, in the list's order"
    );
    assert_eq!(
        ConversationListQuery::default().archived,
        Some(false),
        "the older readers keep the inbox"
    );

    let mut seen = Vec::new();
    let mut after = None;
    for _ in 0..5 {
        let page = h
            .conversations(&ConversationListQuery {
                archived: None,
                limit: 1,
                after,
                ..ConversationListQuery::default()
            })
            .expect("page");
        seen.extend(page.rows.iter().map(who));
        after = page.next;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(seen, ["carol", "bob", "alice"], "each chat once, by cursor");
}
