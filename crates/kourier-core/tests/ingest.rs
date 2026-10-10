//! Ingest rules (§4.9) on the per-network scripts of `kourier-fake`: idempotency (every script
//! applied twice), convergence under permutations and duplication (proptest, merges included),
//! modifiers before their targets, and the resulting rows as the facade queries them.

use kourier_backend_api::BackendEvent;
use kourier_core::facade::{ConversationListQuery, TimelineAnchor, TimelineQuery};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::{Flavor, Script, scripts};
use kourier_model::{
    Body, ConvKey, ConversationId, MsgTarget, NetMsgId, NetworkKind, OutgoingState, ReadState,
    ReceiptKind, UserId,
};
use proptest::prelude::*;

fn harness(flavor: Flavor, script: &Script) -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7; 32]),
        NetworkKind::Fake,
        flavor.descriptor().msg_id_scope,
        &script.own_ids,
    )
    .expect("harness");
    (dir, h)
}

fn apply_all(
    flavor: Flavor,
    script: &Script,
    envelopes: &[Vec<BackendEvent>],
) -> (tempfile::TempDir, IngestHarness) {
    let (dir, mut h) = harness(flavor, script);
    for env in envelopes {
        h.apply(env.clone()).expect("apply");
    }
    (dir, h)
}

fn dump(
    flavor: Flavor,
    script: &Script,
    envelopes: &[Vec<BackendEvent>],
    mode: DumpMode,
) -> String {
    apply_all(flavor, script, envelopes)
        .1
        .dump(mode)
        .expect("dump")
}

/// Equal dumps, or a panic listing only the differing lines.
fn diff(expected: &str, got: &str) -> Option<String> {
    if expected == got {
        return None;
    }
    let e: std::collections::BTreeSet<&str> = expected.lines().collect();
    let g: std::collections::BTreeSet<&str> = got.lines().collect();
    let mut out = String::new();
    for l in e.difference(&g) {
        out.push_str(&format!("- {}\n", &l[..l.len().min(600)]));
    }
    for l in g.difference(&e) {
        out.push_str(&format!("+ {}\n", &l[..l.len().min(600)]));
    }
    Some(out)
}

#[track_caller]
fn assert_same(expected: &str, got: &str, ctx: &str) {
    if let Some(d) = diff(expected, got) {
        panic!("{ctx}: states differ\n{d}");
    }
}

fn all_scripts() -> Vec<(Flavor, Script)> {
    let mut v = scripts::all().expect("scripts");
    v.push((
        Flavor::SignalLike,
        kourier_fake::fixture::fixture(&kourier_fake::fixture::FixtureSpec {
            contacts: 4,
            groups: 2,
            messages_per_conversation: 12,
            ..Default::default()
        })
        .expect("fixture"),
    ));
    // Every signal of a call, one delivery each (`kourier_fake::calls`).
    v.push((
        Flavor::SignalLike,
        kourier_fake::calls::script().expect("script"),
    ));
    v
}

#[test]
fn every_script_applied_twice_gives_the_same_state() {
    for (flavor, script) in all_scripts() {
        let once = script.deliveries();
        let base = dump(flavor, &script, &once, DumpMode::Exact);
        // The whole stream redelivered after a crash.
        let twice: Vec<_> = once.iter().chain(once.iter()).cloned().collect();
        assert_same(
            &base,
            &dump(flavor, &script, &twice, DumpMode::Exact),
            &format!("{}: stream twice", script.name),
        );
        // Every envelope redelivered at once (forwarder crash before its DELETE).
        let doubled: Vec<_> = once.iter().flat_map(|e| [e.clone(), e.clone()]).collect();
        assert_same(
            &base,
            &dump(flavor, &script, &doubled, DumpMode::Exact),
            &format!("{}: each twice", script.name),
        );
    }
}

fn permutation_case(
    flavor: Flavor,
    script: &Script,
    order: &[usize],
    dups: &[(usize, usize)],
) -> Result<(), TestCaseError> {
    let canonical = script.permutable_deliveries();
    let expected = dump(flavor, script, &canonical, DumpMode::OrderInsensitive);
    let mut shuffled: Vec<Vec<BackendEvent>> =
        order.iter().map(|i| canonical[*i].clone()).collect();
    for (src, at) in dups {
        let copy = shuffled[*src % shuffled.len()].clone();
        let at = (*at % (shuffled.len() + 1)).min(shuffled.len());
        shuffled.insert(at, copy);
    }
    let got = dump(flavor, script, &shuffled, DumpMode::OrderInsensitive);
    if let Some(d) = diff(&expected, &got) {
        return Err(TestCaseError::fail(format!(
            "order {order:?} dups {dups:?}\n{d}"
        )));
    }
    Ok(())
}

fn strategy(n: usize) -> impl Strategy<Value = (Vec<usize>, Vec<(usize, usize)>)> {
    (
        Just((0..n).collect::<Vec<_>>()).prop_shuffle(),
        proptest::collection::vec((0..n, 0..=n), 0..4),
    )
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    #[test]
    fn signal_like_converges_in_any_order((order, dups) in strategy(scripts::signal_like().expect("script").permutable_deliveries().len())) {
        let script = scripts::signal_like().expect("script");
        permutation_case(Flavor::SignalLike, &script, &order, &dups)?;
    }

    #[test]
    fn whatsapp_like_converges_in_any_order((order, dups) in strategy(scripts::whatsapp_like().expect("script").permutable_deliveries().len())) {
        let script = scripts::whatsapp_like().expect("script");
        permutation_case(Flavor::WhatsAppLike, &script, &order, &dups)?;
    }

    /// The signals of every call of `kourier_fake::calls` in any order, some of them twice:
    /// one line per call, the same line (rings, hang-ups, the phone's syncs, a delete of the
    /// entry before or after its line, two members starting one group call).
    #[test]
    fn call_lines_converge_in_any_order((order, dups) in strategy(kourier_fake::calls::script().expect("script").permutable_deliveries().len())) {
        let script = kourier_fake::calls::script().expect("script");
        permutation_case(Flavor::SignalLike, &script, &order, &dups)?;
    }

    #[test]
    fn threema_like_converges_in_any_order((order, dups) in strategy(scripts::threema_like().expect("script").permutable_deliveries().len())) {
        let script = scripts::threema_like().expect("script");
        permutation_case(Flavor::ThreemaLike, &script, &order, &dups)?;
    }
}

/// The merge runs before, between and after the messages of both conversations (§3.3).
#[test]
fn identity_merge_at_every_position_gives_the_same_state() {
    for (flavor, script) in scripts::all().expect("scripts") {
        let canonical = script.permutable_deliveries();
        let Some(merge_at) = canonical.iter().position(|d| {
            d.iter()
                .any(|e| matches!(e, BackendEvent::IdentityMerged { .. }))
        }) else {
            continue;
        };
        let expected = dump(flavor, &script, &canonical, DumpMode::OrderInsensitive);
        let mut rest = canonical.clone();
        let merge = rest.remove(merge_at);
        for pos in 0..=rest.len() {
            let mut order = rest.clone();
            order.insert(pos, merge.clone());
            assert_same(
                &expected,
                &dump(flavor, &script, &order, DumpMode::OrderInsensitive),
                &format!("{}: merge at {pos}", script.name),
            );
        }
    }
}

fn conv_id(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    let id: i64 = h
        .connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0),
        )
        .expect("conversation exists");
    ConversationId(id)
}

fn timeline(h: &IngestHarness, conv: ConversationId) -> Vec<kourier_core::facade::MessageRow> {
    h.timeline(&TimelineQuery {
        conv,
        anchor: TimelineAnchor::Newest,
        limit: 500,
    })
    .expect("timeline")
    .rows
}

fn text(body: &Body) -> &str {
    match body {
        Body::Text(t) => &t.body,
        _ => "",
    }
}

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

#[test]
fn receipt_before_its_message_is_applied_on_arrival() {
    let script = scripts::signal_like().expect("script");
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let d_alice = ConvKey::Direct(alice.clone());
    let (_dir, mut h) = harness(Flavor::SignalLike, &script);
    let local = (scripts::T0 + 20_000).to_string();
    h.apply(vec![BackendEvent::Receipts {
        targets: vec![MsgTarget::to_id(None, &NetMsgId::own(local.as_str()))],
        by: alice.clone(),
        kind: ReceiptKind::Delivered,
        at: scripts::T0 + 25_000,
    }])
    .expect("receipt");
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 1, "parked until the message arrives");
    h.apply(vec![BackendEvent::Message(scripts::own(
        &d_alice,
        &local,
        scripts::T0 + 20_000,
        "Hello Alice",
    ))])
    .expect("message");
    let rows = timeline(&h, conv_id(&h, &d_alice));
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].out_state, Some(OutgoingState::Delivered));
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0);
}

#[test]
fn signal_like_script_ends_in_the_expected_rows() {
    let script = scripts::signal_like().expect("script");
    let (_dir, h) = apply_all(Flavor::SignalLike, &script, &script.deliveries());
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let carol = uid("aci:00000000-0000-4000-8000-0000000ca202");
    let group = ConvKey::Group("gv2:ZmFrZS1zaWduYWwtZ3JvdXA".into());

    // Direct chat with Alice: edited twice, our reaction replaced, our message read by her.
    let rows = timeline(&h, conv_id(&h, &ConvKey::Direct(alice.clone())));
    assert_eq!(rows.len(), 2);
    let a = &rows[0];
    assert_eq!(text(&a.body), "Hi there!!");
    assert_eq!(a.revision, 2);
    assert_eq!(a.reactions.len(), 1, "one reaction per user: the newest");
    assert_eq!(a.reactions[0].emoji, "👍");
    assert!(a.reactions[0].by_me());
    let o = &rows[1];
    assert_eq!(o.author, None);
    assert_eq!(o.out_state, Some(OutgoingState::Read));
    assert_eq!(o.reactions[0].users, vec![Some(alice.clone())]);

    // Group: mention read on the phone, delete for everyone, placeholder replaced, system
    // message; the cross-conversation copy of Alice's message was a duplicate.
    let g = conv_id(&h, &group);
    let rows = timeline(&h, g);
    let texts: Vec<&str> = rows.iter().map(|r| text(&r.body)).collect();
    assert_eq!(texts, ["\u{FFFC} are you in?", "", "Saturday works", ""]);
    assert_eq!(rows[0].read_state, Some(ReadState::Read));
    assert!(rows[0].mentions_me);
    assert!(rows[1].deleted.is_some());
    assert_eq!(rows[2].read_state, Some(ReadState::Unread));
    assert!(matches!(rows[3].body, Body::System(_)));
    assert_eq!(rows[3].read_state, None, "system notices are never unread");
    let page = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    let grow = page.rows.iter().find(|r| r.id == g).expect("group row");
    assert_eq!(grow.title.as_deref(), Some("Weekend Plans"));
    assert_eq!(grow.unread, 1, "the resent placeholder");
    assert_eq!(grow.mentions, 0);

    // Carol: the PNI chat merged into the ACI chat; the merged contact has both names.
    let rows = timeline(&h, conv_id(&h, &ConvKey::Direct(carol.clone())));
    let texts: Vec<&str> = rows.iter().map(|r| text(&r.body)).collect();
    assert_eq!(texts, ["New number, who dis", "It's Carol", "Still me"]);
    assert!(rows.iter().all(|r| r.author.as_ref() == Some(&carol)));
    let pni_left: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM conversations WHERE conv_key LIKE 'direct:pni:%'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(pni_left, 0);
    let names: (Option<String>, Option<String>) = h
        .connection()
        .query_row(
            "SELECT profile_name, system_name FROM contacts WHERE user_id = ?1",
            [carol.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("contact");
    assert_eq!(
        names,
        (Some("Carol Example".into()), Some("Carol Phone".into()))
    );
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0, "every modifier found its target");
}

#[test]
fn whatsapp_like_script_ends_in_the_expected_rows() {
    let script = scripts::whatsapp_like().expect("script");
    let (_dir, h) = apply_all(Flavor::WhatsAppLike, &script, &script.deliveries());
    let alice = uid("lid:900000101");
    let d_alice = ConvKey::Direct(alice.clone());
    let c = conv_id(&h, &d_alice);
    let rows = timeline(&h, c);
    let locals: Vec<String> = rows
        .iter()
        .map(|r| {
            h.connection()
                .query_row(
                    "SELECT net_local FROM messages WHERE id = ?1",
                    [r.id.0],
                    |x| x.get(0),
                )
                .expect("local")
        })
        .collect();
    // History by sent_at, then live; the delete-for-me row is hidden; the duplicate own message
    // appears once with the better state.
    assert_eq!(
        locals,
        [
            "3EB0AA01", "3EB0AA02", "3EB0BB01", "3EB0BB02", "3EB0CC01", "3EB0CC02"
        ]
    );
    assert_eq!(rows[1].out_state, Some(OutgoingState::Read));
    assert!(rows[1].history);
    assert_eq!(
        rows[3].out_state,
        Some(OutgoingState::Read),
        "read receipt after the merge"
    );
    assert_eq!(rows[3].reactions[0].users, vec![Some(alice.clone())]);
    assert_eq!(text(&rows[4].body), "Live now (edited)");
    assert_eq!(rows[4].revision, 1);
    let page = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    let row = page.rows.iter().find(|r| r.id == c).expect("row");
    // unread_hint 1 (history) + two live messages.
    assert_eq!(row.unread, 3);
    let group = conv_id(&h, &ConvKey::Group("wag:120363000000000001".into()));
    assert_eq!(timeline(&h, group).len(), scripts::WHATSAPP_GROUP_HISTORY);
    // Keyset paging walks the whole group history without gaps or repeats.
    let mut seen = Vec::new();
    let mut anchor = TimelineAnchor::Newest;
    loop {
        let page = h
            .timeline(&TimelineQuery {
                conv: group,
                anchor,
                limit: 50,
            })
            .expect("page");
        let first = page.rows.first().map(|r| r.cursor());
        seen.splice(0..0, page.rows.iter().map(|r| r.id));
        match (page.has_older, first) {
            (true, Some(c)) => anchor = TimelineAnchor::Before(c),
            _ => break,
        }
    }
    let all: Vec<_> = timeline(&h, group).iter().map(|r| r.id).collect();
    assert_eq!(seen, all);
}

#[test]
fn threema_like_script_ends_in_the_expected_rows() {
    let script = scripts::threema_like().expect("script");
    let (_dir, h) = apply_all(Flavor::ThreemaLike, &script, &script.deliveries());
    let echo = uid("threema:ECHOECHO");
    let group = conv_id(&h, &ConvKey::Group("tgr:ECHOECHO-0000000000000001".into()));
    let rows = timeline(&h, group);
    assert_eq!(rows.len(), 2);
    // A set of reactions per user (Threema).
    let r = &rows[0].reactions;
    let thumbs = r.iter().find(|g| g.emoji == "👍").expect("thumbs");
    assert_eq!(thumbs.users.len(), 2);
    assert!(
        r.iter()
            .any(|g| g.emoji == "❤️" && g.users == vec![Some(echo.clone())])
    );
    assert!(
        rows[1].deleted.is_some(),
        "deleted by its author through an author-less reference"
    );
    let d_echo = conv_id(&h, &ConvKey::Direct(echo.clone()));
    let rows = timeline(&h, d_echo);
    assert_eq!(rows[0].out_state, Some(OutgoingState::Read));
    assert_eq!(rows.len(), 2);
}

/// A network whose history does not say WHICH messages are unread (the WhatsApp-like one)
/// says `read: true` for every history row and gives the count as `unread_hint`: its
/// batches raise no unread count, the hint and the live messages do (§4.9). A history row
/// that says `read: false` is known to be unread and counts: `tests/history_rules.rs`.
#[test]
fn history_rows_of_unknown_read_state_raise_no_unread_the_hint_and_live_messages_do() {
    let script = scripts::whatsapp_like().expect("script");
    let (_dir, h) = apply_all(Flavor::WhatsAppLike, &script, &script.deliveries());
    let group = conv_id(&h, &ConvKey::Group("wag:120363000000000001".into()));
    let page = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    let row = page.rows.iter().find(|r| r.id == group).expect("row");
    assert_eq!(
        row.unread, 0,
        "no row of the group's history says unread, and it has no hint"
    );
    // The history message that was edited before the import: the mark, and the reaction
    // that named its newest version.
    let rows = timeline(&h, group);
    let edited = &rows[scripts::WHATSAPP_EDITED_HISTORY.0];
    assert_eq!(edited.revision, 2);
    assert_eq!(text(&edited.body), "group history, as edited");
    assert_eq!(edited.reactions.len(), 1);
    assert!(rows.iter().filter(|r| r.revision > 0).count() == 1);
}

#[test]
fn pinned_list_removal_attachments_and_caps() {
    use kourier_model::{
        AccountCaps, Attachment, AttachmentKind, AttachmentMeta, AttachmentState, BackendBlob,
        BackendError, ErrorKind,
    };
    let script = scripts::signal_like().expect("script");
    let (_dir, mut h) = harness(Flavor::SignalLike, &script);
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let bob = uid("aci:00000000-0000-4000-8000-000000000b0b");
    let (da, db) = (ConvKey::Direct(alice.clone()), ConvKey::Direct(bob.clone()));
    let mut photo = scripts::remote(&da, &alice, "100", scripts::T0, "");
    photo.body = Body::Media {
        attachments: vec![
            Attachment {
                kind: AttachmentKind::Image,
                mime: "image/jpeg".into(),
                size: 1234,
                remote: Some(BackendBlob::new(1, vec![1, 2, 3])),
                ..Default::default()
            },
            Attachment {
                kind: AttachmentKind::File,
                mime: "application/pdf".into(),
                size: 99,
                ..Default::default()
            },
        ],
        caption: None,
    };
    h.apply(vec![
        BackendEvent::Message(photo),
        BackendEvent::Message(scripts::remote(&db, &bob, "200", scripts::T0 + 1, "hi")),
        BackendEvent::PinnedConversations(vec![db.clone(), da.clone()]),
        BackendEvent::AccountCaps(AccountCaps {
            max_attachment_bytes: Some(100 << 20),
            ..Default::default()
        }),
    ])
    .expect("apply");
    let page = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    let pins: Vec<(Option<UserId>, Option<u32>)> = page
        .rows
        .iter()
        .map(|r| (r.peer.clone(), r.pinned_order))
        .collect();
    assert_eq!(
        pins,
        [(Some(bob.clone()), Some(0)), (Some(alice.clone()), Some(1))]
    );
    // A new full list unpins what it leaves out.
    h.apply(vec![BackendEvent::PinnedConversations(vec![da.clone()])])
        .expect("apply");
    let page = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    assert_eq!(page.rows[0].peer.as_ref(), Some(&alice));
    assert_eq!(page.rows[1].pinned_order, None);

    // Attachment rows carry ids and download states into the timeline.
    let conv = conv_id(&h, &da);
    let rows = timeline(&h, conv);
    let Body::Media { attachments, .. } = &rows[0].body else {
        panic!("media");
    };
    let (first, second) = (
        attachments[0].id.expect("id"),
        attachments[1].id.expect("id"),
    );
    // The image is fetched at once (auto-download policy, §5.8); the file without a pointer
    // waits.
    assert_eq!(
        attachments[0].state,
        AttachmentState::Downloading {
            done: 0,
            total: 1234
        }
    );
    assert_eq!(attachments[1].state, AttachmentState::Remote);
    assert!(
        attachments[0].remote.is_none() && attachments[0].local.is_none(),
        "pointers and keys never reach the UI rows"
    );
    h.apply(vec![
        BackendEvent::AttachmentDone {
            attachment: first,
            request: None,
            result: Ok(AttachmentMeta {
                size: 1200,
                mime: None,
                dims: Some((640, 480)),
                duration_ms: None,
            }),
        },
        BackendEvent::AttachmentDone {
            attachment: second,
            request: None,
            result: Err(BackendError::new(ErrorKind::Expired, "gone")),
        },
    ])
    .expect("apply");
    let rows = timeline(&h, conv);
    let Body::Media { attachments, .. } = &rows[0].body else {
        panic!("media");
    };
    assert_eq!(attachments[0].state, AttachmentState::Local);
    assert_eq!(attachments[1].state, AttachmentState::Expired);
    let caps: Option<String> = h
        .connection()
        .query_row("SELECT caps FROM accounts", [], |r| r.get(0))
        .expect("caps");
    assert!(caps.is_some_and(|c| c.contains("104857600")));

    // A removed conversation takes its messages, attachments and parked modifiers with it.
    h.apply(vec![BackendEvent::Reaction {
        target: scripts::target(Some(&da), Some(&alice), "999"),
        by: Some(alice.clone()),
        emoji: "👍".into(),
        remove: false,
        at: 5,
    }])
    .expect("apply");
    h.apply(vec![BackendEvent::ConversationRemoved { conv: da.clone() }])
        .expect("apply");
    let counts: (i64, i64, i64) = h
        .connection()
        .query_row(
            "SELECT (SELECT count(*) FROM messages), (SELECT count(*) FROM attachments),
                    (SELECT count(*) FROM pending_modifiers)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("counts");
    assert_eq!(counts, (1, 0, 0));
    // Replaying the removal is a no-op; a reaction removal before the add wins by `at`.
    h.apply(vec![BackendEvent::ConversationRemoved { conv: da }])
        .expect("apply");
    let bob_conv = conv_id(&h, &db);
    let target = scripts::target(Some(&db), Some(&bob), "200");
    h.apply(vec![BackendEvent::Reaction {
        target: target.clone(),
        by: None,
        emoji: "🎉".into(),
        remove: true,
        at: 20,
    }])
    .expect("apply");
    h.apply(vec![BackendEvent::Reaction {
        target,
        by: None,
        emoji: "🎉".into(),
        remove: false,
        at: 10,
    }])
    .expect("apply");
    assert!(timeline(&h, bob_conv)[0].reactions.is_empty());
}

/// A Signal duplicate (same author and timestamp) in a conversation that does not exist yet is
/// dropped without leaving an empty chat behind (§3.1, §4.9); a history batch with a sender's
/// extreme timestamp is ingested, not refused forever.
#[test]
fn dropped_duplicate_creates_no_conversation_and_extreme_timestamps_saturate() {
    let script =
        kourier_fake::Script::new("dup", vec![uid("aci:00000000-0000-4000-8000-0000000000aa")]);
    let (_dir, mut h) = harness(Flavor::SignalLike, &script);
    let alice = uid("aci:00000000-0000-4000-8000-0000000a11ce");
    let direct = ConvKey::Direct(alice.clone());
    let group = ConvKey::Group("gv2:ZHVwbGljYXRlLWdyb3Vw".into());
    h.apply(vec![BackendEvent::Message(scripts::remote(
        &direct, &alice, "1000", 1000, "original",
    ))])
    .expect("apply");
    h.apply(vec![BackendEvent::Message(scripts::remote(
        &group,
        &alice,
        "1000",
        1000,
        "copy elsewhere",
    ))])
    .expect("apply");
    let convs: Vec<String> = h
        .connection()
        .prepare("SELECT conv_key FROM conversations")
        .expect("prepare")
        .query_map([], |r| r.get(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(convs, [direct.encode()]);

    let mut late = scripts::remote(&direct, &alice, "x", i64::MAX - 500, "far future");
    late.received_at = i64::MAX;
    h.apply(vec![BackendEvent::HistoryBatch {
        conv: direct.clone(),
        messages: vec![late],
        has_more: false,
        progress: None,
    }])
    .expect("an extreme sent_at saturates");
    let page = timeline(&h, conv_id(&h, &direct));
    assert_eq!(page.len(), 2);
    assert_eq!(page.last().map(|r| r.sort_key), Some(i64::MAX));
}
