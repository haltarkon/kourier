//! `Query::MessagesByIds` (selection mode, step SEL of Export Selected): the messages asked for
//! by id as a timeline page shows them: each once, oldest first, only what the timeline would
//! show (never deleted for me, never hidden by the expiry rule, never another chat's), and any
//! number of ids (they are bound in chunks).

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::facade::{TimelineAnchor, TimelineQuery};
use kourier_core::testing::IngestHarness;
use kourier_model::{
    Body, ConvKey, ConversationId, MessageId, MsgIdScope, NetworkKind, Origin, Text, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-00000000000a";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-0000000b0b00";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn harness(dir: &tempfile::TempDir) -> IngestHarness {
    IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([4; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

/// A text from `who` at `at` (the network id is the time).
fn text(who: &str, at: i64) -> IncomingMessage {
    let mut m = IncomingMessage::new(
        ConvKey::Direct(uid(who)),
        at.to_string().as_str(),
        Origin::Remote {
            author: uid(who),
            device: Some(1),
        },
        Body::Text(Text {
            body: format!("text {at}"),
            ranges: Vec::new(),
        }),
        at,
        at + 500,
    );
    m.read = true;
    m
}

fn ingest(h: &mut IngestHarness, messages: Vec<IncomingMessage>) {
    h.apply(messages.into_iter().map(BackendEvent::Message).collect())
        .expect("apply");
}

fn conv_of(h: &IngestHarness, who: &str) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [ConvKey::Direct(uid(who)).encode()],
            |r| r.get::<_, i64>(0),
        )
        .map(ConversationId)
        .expect("conversation")
}

/// The id of the message made at `at`.
fn id_at(h: &IngestHarness, at: i64) -> MessageId {
    h.connection()
        .query_row(
            "SELECT id FROM messages WHERE net_local = ?1",
            [at.to_string()],
            |r| r.get::<_, i64>(0),
        )
        .map(MessageId)
        .expect("message")
}

fn texts(page: &kourier_core::facade::TimelinePage) -> Vec<String> {
    page.rows
        .iter()
        .map(|r| match &r.body {
            Body::Text(t) => t.body.clone(),
            other => other.kind_name().to_owned(),
        })
        .collect()
}

#[test]
fn every_asked_visible_row_comes_once_and_in_timeline_order() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    ingest(&mut h, (1..=6).map(|n| text(ALICE, n * 1_000)).collect());
    let conv = conv_of(&h, ALICE);
    // Asked out of order, one of them twice.
    let ids = [5, 2, 4, 2].map(|n| id_at(&h, n * 1_000));
    let page = h.messages_by_ids(conv, &ids).expect("the page");
    assert_eq!(texts(&page), ["text 2000", "text 4000", "text 5000"]);
    assert_eq!((page.has_older, page.has_newer), (false, false));
    assert_eq!(page.conv, conv);
    // The rows are the timeline's own: the same fields and action flags.
    let all = h
        .timeline(&TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 100,
        })
        .expect("timeline");
    for row in &page.rows {
        let same = all
            .rows
            .iter()
            .find(|r| r.id == row.id)
            .expect("in the timeline");
        assert_eq!(format!("{row:?}"), format!("{same:?}"));
    }
    assert!(h.messages_by_ids(conv, &[]).expect("empty").rows.is_empty());
}

#[test]
fn a_row_deleted_for_me_and_an_expiry_hidden_row_are_absent() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    ingest(&mut h, (1..=4).map(|n| text(ALICE, n * 1_000)).collect());
    let conv = conv_of(&h, ALICE);
    let ids: Vec<MessageId> = (1..=4).map(|n| id_at(&h, n * 1_000)).collect();
    let c = h.connection();
    c.execute(
        "UPDATE messages SET deleted = 'ForMe' WHERE id = ?1",
        [ids[1].0],
    )
    .expect("delete for me");
    c.execute(
        "UPDATE messages SET expiry_hidden = 1 WHERE id = ?1",
        [ids[2].0],
    )
    .expect("expiry");
    let page = h.messages_by_ids(conv, &ids).expect("the page");
    assert_eq!(texts(&page), ["text 1000", "text 4000"]);
}

#[test]
fn an_id_of_another_conversation_or_an_unknown_id_is_absent() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    ingest(
        &mut h,
        vec![text(ALICE, 1_000), text(BOB, 2_000), text(ALICE, 3_000)],
    );
    let alice = conv_of(&h, ALICE);
    let bobs = id_at(&h, 2_000);
    let ids = [
        id_at(&h, 1_000),
        bobs,
        MessageId(9_999_999),
        id_at(&h, 3_000),
    ];
    let page = h.messages_by_ids(alice, &ids).expect("the page");
    assert_eq!(texts(&page), ["text 1000", "text 3000"]);
    let page = h.messages_by_ids(conv_of(&h, BOB), &ids).expect("bob's");
    assert_eq!(texts(&page), ["text 2000"]);
}

#[test]
fn twelve_hundred_ids_are_all_answered_in_order() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    ingest(
        &mut h,
        (1..=1_300).map(|n| text(ALICE, n * 1_000)).collect(),
    );
    let conv = conv_of(&h, ALICE);
    // The first 1,200, newest first: more than two chunks of 500.
    let ids: Vec<MessageId> = (1..=1_200).rev().map(|n| id_at(&h, n * 1_000)).collect();
    let page = h.messages_by_ids(conv, &ids).expect("the page");
    assert_eq!(page.rows.len(), 1_200);
    let expected: Vec<String> = (1..=1_200).map(|n| format!("text {}", n * 1_000)).collect();
    assert_eq!(texts(&page), expected);
}
