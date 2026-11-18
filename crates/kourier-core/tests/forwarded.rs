//! The forward state of a message (forward line F1): stored for a NEW row, live and from a
//! history batch; a second delivery does not clear it; the timeline's `MessageRow::forwarded`
//! is `Some` only on a network whose descriptor states `forwards`, and `many` follows its rule.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::facade::{ForwardedMark, TimelineAnchor, TimelineQuery};
use kourier_core::testing::IngestHarness;
use kourier_model::{
    Body, ConvKey, ConversationId, ForwardRules, Forwarded, MsgIdScope, NetworkKind, Origin, Text,
    UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-00000000000a";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

const RULES: ForwardRules = ForwardRules {
    max_chats: Some(5),
    many_from: 5,
    many_max_chats: 1,
};

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

fn text(at: i64, forwarded: Option<u32>) -> IncomingMessage {
    let mut m = IncomingMessage::new(
        ConvKey::Direct(uid(ALICE)),
        at.to_string().as_str(),
        Origin::Remote {
            author: uid(ALICE),
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
    m.forwarded = forwarded.map(|score| Forwarded { score });
    m
}

fn conv(h: &IngestHarness) -> ConversationId {
    h.connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [ConvKey::Direct(uid(ALICE)).encode()],
            |r| r.get::<_, i64>(0),
        )
        .map(ConversationId)
        .expect("conversation")
}

fn stored(h: &IngestHarness, at: i64) -> Option<i64> {
    h.connection()
        .query_row(
            "SELECT forwarded FROM messages WHERE net_local = ?1",
            [at.to_string()],
            |r| r.get(0),
        )
        .expect("row")
}

fn marks(h: &IngestHarness, rules: Option<ForwardRules>) -> Vec<Option<ForwardedMark>> {
    let page = h
        .timeline_with_forwards(
            &TimelineQuery {
                conv: conv(h),
                anchor: TimelineAnchor::Newest,
                limit: 50,
            },
            rules,
        )
        .expect("timeline");
    page.rows.iter().map(|r| r.forwarded).collect()
}

#[test]
fn a_live_and_a_history_message_store_the_forward_state() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    h.apply(vec![
        BackendEvent::Message(text(1_000, Some(0))),
        BackendEvent::Message(text(2_000, None)),
        BackendEvent::HistoryBatch {
            conv: ConvKey::Direct(uid(ALICE)),
            messages: vec![text(500, Some(127))],
            has_more: false,
            progress: None,
        },
    ])
    .expect("apply");
    assert_eq!(stored(&h, 1_000), Some(0));
    assert_eq!(stored(&h, 2_000), None);
    assert_eq!(stored(&h, 500), Some(127));
}

#[test]
fn a_second_delivery_does_not_clear_it() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    h.apply(vec![BackendEvent::Message(text(1_000, Some(3)))])
        .expect("first");
    h.apply(vec![BackendEvent::Message(text(1_000, None))])
        .expect("second");
    assert_eq!(stored(&h, 1_000), Some(3));
}

#[test]
fn the_row_shows_the_mark_only_where_the_network_states_forwards() {
    let dir = tempfile::tempdir().expect("dir");
    let mut h = harness(&dir);
    h.apply(
        [
            (1_000, Some(0)),
            (2_000, Some(4)),
            (3_000, Some(5)),
            (4_000, None),
        ]
        .into_iter()
        .map(|(at, f)| BackendEvent::Message(text(at, f)))
        .collect(),
    )
    .expect("apply");
    assert_eq!(marks(&h, None), vec![None; 4], "a network without forwards");
    assert_eq!(
        marks(&h, Some(RULES)),
        vec![
            Some(ForwardedMark { many: false }),
            Some(ForwardedMark { many: false }),
            Some(ForwardedMark { many: true }),
            None
        ]
    );
}
