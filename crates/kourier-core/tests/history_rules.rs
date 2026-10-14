//! The rules a history import relies on (docs/architecture.md §4.10, §14.2 item 14; the
//! contract's text is on `IncomingMessage`, `BackendEvent::HistoryBatch` and
//! `ConversationPatch::unread_hint`):
//!
//! - THE ROWS ARE THE TRUTH about what is unread: a history row that says `read: false`
//!   counts (unread and mentions) like a live one, never notifies and never enters the
//!   catch-up summary; reading the chat here, a read marker of another device and a second
//!   delivery of the row behave as for a live row; `unread_hint` only adds what has no row;
//! - reading an imported chat tells our other devices (the read sync names the newest row
//!   that is a message, and lists the imported rows that were read) and never the sender of
//!   an imported message;
//! - a history message that was edited before the import has its versions' keys and the
//!   "edited" mark;
//! - three timer cases: an own message that was not sent, a call line, a notice.

use std::time::Duration;

use kourier_backend_api::{
    BackendEvent, DurableCommand, HistoryEdit, IncomingMessage, MAX_HISTORY_EDITS,
};
use kourier_core::facade::{
    ConversationListQuery, MessageRow, QuoteState, TimelineAnchor, TimelineQuery,
};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::scripts::{own, remote, target};
use kourier_model::{
    AccountSettingsPatch, Body, CallDirection, CallLine, CallMedia, CallState, ConvKey,
    ConversationId, ConversationPatch, DisappearKind, Expiry, FailReason, MsgIdScope, MsgTarget,
    NetworkKind, Origin, OutgoingState, Patch, ReadState, ReceiptKind, ReplyTo, SystemEvent,
    TargetAuthor, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";
const BOB: &str = "aci:00000000-0000-4000-8000-000000000b0b";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn alice() -> UserId {
    uid(ALICE)
}

fn d_alice() -> ConvKey {
    ConvKey::Direct(alice())
}

fn group() -> ConvKey {
    ConvKey::Group("gv2:aGlzdG9yeS1ydWxlcw".into())
}

fn harness() -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([5; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    (dir, h)
}

fn batch(conv: &ConvKey, messages: Vec<IncomingMessage>) -> BackendEvent {
    BackendEvent::HistoryBatch {
        conv: conv.clone(),
        messages,
        has_more: false,
        progress: None,
    }
}

/// An incoming text of `conv` sent at `at` (its id is the time), read or not on the phone.
fn incoming(conv: &ConvKey, by: &UserId, at: i64, read: bool) -> IncomingMessage {
    let mut m = remote(conv, by, &at.to_string(), at, "text");
    m.read = read;
    m
}

fn conv(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    h.conversations(&ConversationListQuery::default())
        .expect("list")
        .rows
        .iter()
        .find(|r| match key {
            ConvKey::Direct(u) => r.peer.as_ref() == Some(u),
            _ => r.peer.is_none(),
        })
        .map(|r| r.id)
        .expect("the conversation")
}

/// (unread, mentions) as the conversation list shows them.
fn counts(h: &IngestHarness, key: &ConvKey) -> (u32, u32) {
    let id = conv(h, key);
    let page = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    let row = page.rows.iter().find(|r| r.id == id).expect("row");
    (row.unread, row.mentions)
}

fn rows(h: &IngestHarness, key: &ConvKey) -> Vec<MessageRow> {
    h.timeline(&TimelineQuery {
        conv: conv(h, key),
        anchor: TimelineAnchor::Newest,
        limit: 500,
    })
    .expect("timeline")
    .rows
}

fn row(h: &IngestHarness, key: &ConvKey, at: i64) -> MessageRow {
    rows(h, key)
        .into_iter()
        .find(|r| r.sent_at == at)
        .expect("the row")
}

fn read_receipts_on() -> BackendEvent {
    BackendEvent::AccountSettings(AccountSettingsPatch {
        read_receipts: Some(true),
        ..Default::default()
    })
}

/// The ids (`local`) of the targets of a list.
fn locals(targets: &[MsgTarget]) -> Vec<&str> {
    targets.iter().map(|t| t.local.as_ref()).collect()
}

/// Rule 2: an unread row of a history batch counts like a live one, for the unread count and
/// for the mentions, and is announced to nobody (not alone, not in the catch-up summary,
/// whatever its `silent` and `catch_up` say). A second delivery counts nothing twice.
#[test]
fn an_unread_history_row_counts_and_notifies_nobody() {
    let (_dir, mut h) = harness();
    let (a, g) = (d_alice(), group());
    let mut mention = incoming(&g, &alice(), 310, false);
    mention.mentions_me = true;
    // A backend that forgot `silent`, and one that set `catch_up`: neither is read.
    let mut loud = incoming(&a, &alice(), 400, false);
    loud.silent = false;
    loud.catch_up = true;
    let history = || {
        vec![
            batch(
                &a,
                vec![
                    incoming(&a, &alice(), 100, true),
                    own(&a, "200", 200, "mine"),
                    incoming(&a, &alice(), 300, false),
                    loud.clone(),
                ],
            ),
            batch(
                &g,
                vec![
                    incoming(&g, &alice(), 110, true),
                    mention.clone(),
                    incoming(&g, &uid(BOB), 320, false),
                ],
            ),
        ]
    };
    for event in history() {
        let said = h.apply_announcing(vec![event]).expect("history");
        assert_eq!((said.notify.len(), said.catch_up), (0, 0));
    }
    assert_eq!(counts(&h, &a), (2, 0), "the two the phone had not read");
    assert_eq!(counts(&h, &g), (2, 1), "and the mention among them");
    assert!(rows(&h, &a).iter().all(|r| r.history));
    assert_eq!(row(&h, &a, 300).read_state, Some(ReadState::Unread));
    assert_eq!(row(&h, &a, 100).read_state, Some(ReadState::Read));

    // The same rows again (a re-read after a lost stage): each counts once.
    let before = h.dump(DumpMode::Exact).expect("dump");
    for event in history() {
        let said = h.apply_announcing(vec![event]).expect("history, again");
        assert_eq!((said.notify.len(), said.catch_up), (0, 0));
    }
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before);

    // A live message is one more, and the only one that is announced.
    let said = h
        .apply_announcing(vec![BackendEvent::Message(incoming(
            &a,
            &alice(),
            500,
            false,
        ))])
        .expect("live");
    assert_eq!(said.notify.len(), 1);
    assert_eq!(counts(&h, &a), (3, 0));
}

/// Rule 2: reading the chat here clears the count for good. It does not come back when the
/// same rows are delivered again, when the rest of the chat is delivered afterwards (older
/// rows, the chat's end, an upsert of the chat), nor by a copy that says "unread" of a row
/// that was read here. A row that arrives afterwards and was never here is unread, as it
/// says: nobody has seen it.
#[test]
fn a_chat_read_here_stays_read_while_the_rest_of_it_is_delivered() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let first = || {
        batch(
            &a,
            vec![
                incoming(&a, &alice(), 900, false),
                incoming(&a, &alice(), 910, false),
            ],
        )
    };
    h.apply(vec![first()]).expect("the newest rows first");
    assert_eq!(counts(&h, &a), (2, 0));
    h.mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("something was unread");
    assert_eq!(counts(&h, &a), (0, 0));

    // The rest of the chat: the same two again, older rows, then the chat's end and an
    // upsert that names no count.
    let mut rest = vec![
        incoming(&a, &alice(), 900, false),
        incoming(&a, &alice(), 910, false),
    ];
    rest.extend((0..50).map(|i| incoming(&a, &alice(), 100 + i, true)));
    rest.push(own(&a, "800", 800, "mine"));
    h.apply(vec![
        first(),
        BackendEvent::HistoryBatch {
            conv: a.clone(),
            messages: rest,
            has_more: true,
            progress: Some(0.5),
        },
        batch(&a, Vec::new()),
        BackendEvent::ConversationUpsert(ConversationPatch::new(a.clone())),
    ])
    .expect("the rest");
    assert_eq!(counts(&h, &a), (0, 0), "what was read here stays read");
    assert_eq!(rows(&h, &a).len(), 53);
    assert!(
        h.mark_read(conv(&h, &a)).expect("mark read").is_none(),
        "nothing is left to read"
    );

    // A row that was never here and says unread: it is unread.
    h.apply(vec![batch(&a, vec![incoming(&a, &alice(), 950, false)])])
        .expect("a later row");
    assert_eq!(counts(&h, &a), (1, 0));
}

/// Rule 2: the phone reads the chat afterwards. Its read marker lowers the count by the
/// rows it covers, imported or live (the old hint could not follow it), and a row that is
/// imported after the marker, with a time at or before it, arrives read.
#[test]
fn a_read_marker_of_the_phone_lowers_the_count_of_imported_rows() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    h.apply(vec![batch(
        &a,
        vec![
            incoming(&a, &alice(), 100, false),
            incoming(&a, &alice(), 200, false),
            incoming(&a, &alice(), 300, false),
        ],
    )])
    .expect("history");
    assert_eq!(counts(&h, &a), (3, 0));
    h.apply(vec![BackendEvent::ReadMarker {
        conv: Some(a.clone()),
        up_to: target(Some(&a), Some(&alice()), "200"),
    }])
    .expect("the phone read two");
    assert_eq!(counts(&h, &a), (1, 0));
    h.apply(vec![batch(&a, vec![incoming(&a, &alice(), 150, false)])])
        .expect("a row from before the marker");
    assert_eq!(counts(&h, &a), (1, 0));
    assert_eq!(row(&h, &a, 150).read_state, Some(ReadState::Read));
    h.apply(vec![BackendEvent::ReadMarker {
        conv: Some(a.clone()),
        up_to: target(Some(&a), Some(&alice()), "300"),
    }])
    .expect("and the rest");
    assert_eq!(counts(&h, &a), (0, 0));
}

/// Rule 2: `unread_hint` is for unread messages the backend has no rows for, and nothing
/// else: it adds to what the rows say. A backend whose rows are of unknown read state says
/// `read: true` for them and gives the count as the hint.
#[test]
fn the_hint_adds_only_what_has_no_row() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let mut chat = ConversationPatch::new(a.clone());
    chat.unread_hint = Some(5);
    h.apply(vec![
        BackendEvent::ConversationUpsert(chat),
        batch(
            &a,
            vec![
                incoming(&a, &alice(), 100, true),
                incoming(&a, &alice(), 200, true),
                incoming(&a, &alice(), 300, false),
            ],
        ),
    ])
    .expect("history");
    assert_eq!(counts(&h, &a), (6, 0), "five without a row and one row");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    assert_eq!(counts(&h, &a), (0, 0));
    assert!(
        matches!(
            commands.as_slice(),
            [DurableCommand::SyncReadMarker { up_to, read, .. }]
                if up_to.local.as_ref() == "300" && locals(read) == ["300"]
        ),
        "{commands:?}"
    );
}

/// Rule 3 (D4, D9): reading an imported chat tells OUR devices: one read sync that names the
/// newest row that is a message (not the notice and not the call line that follow it) and
/// lists every row that was read, imported ones too. It tells no SENDER of an imported
/// message, also with read receipts on; the live message beside them gets its receipt.
#[test]
fn reading_an_imported_chat_tells_our_devices_and_no_sender() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let notice = IncomingMessage::new(
        a.clone(),
        "notice-1",
        Origin::Remote {
            author: alice(),
            device: None,
        },
        Body::System(SystemEvent::TitleChanged {
            by: Some(alice()),
            title: "x".into(),
        }),
        700,
        700,
    );
    let mut call = IncomingMessage::call(
        a.clone(),
        "77",
        Origin::Remote {
            author: alice(),
            device: None,
        },
        CallLine::signal(CallDirection::Incoming, false, CallState::Missed, 800)
            .with_media(CallMedia::Voice, 800),
        800,
        800,
    );
    call.read = false;
    h.apply(vec![
        read_receipts_on(),
        batch(
            &a,
            vec![
                incoming(&a, &alice(), 100, true),
                incoming(&a, &alice(), 200, false),
                incoming(&a, &alice(), 300, false),
                notice,
                call,
            ],
        ),
    ])
    .expect("history");
    assert_eq!(counts(&h, &a), (3, 0), "two messages and the missed call");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let [
        DurableCommand::SyncReadMarker {
            conv: synced_chat,
            up_to,
            read,
        },
    ] = commands.as_slice()
    else {
        panic!("one read sync and no receipt: {commands:?}");
    };
    assert_eq!(synced_chat, &a);
    assert_eq!(up_to.local.as_ref(), "300", "the newest MESSAGE");
    assert_eq!(up_to.author, TargetAuthor::User(alice()));
    assert_eq!(locals(read), ["200", "300"], "the call line is never named");
    assert_eq!(counts(&h, &a), (0, 0));

    // A live message next to an imported one: the receipt names only the live one.
    h.apply(vec![
        batch(&a, vec![incoming(&a, &alice(), 900, false)]),
        BackendEvent::Message(incoming(&a, &alice(), 1_000, false)),
    ])
    .expect("one imported, one live");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let mut synced = Vec::new();
    let mut receipts = Vec::new();
    for command in &commands {
        match command {
            DurableCommand::SyncReadMarker { read, up_to, .. } => {
                assert_eq!(up_to.local.as_ref(), "1000");
                synced = locals(read);
            }
            DurableCommand::SendReceipts { targets, kind, .. } => {
                assert_eq!(*kind, ReceiptKind::Read);
                receipts = locals(targets);
            }
            other => panic!("reading queues no {other:?}"),
        }
    }
    assert_eq!(synced, ["900", "1000"]);
    assert_eq!(receipts, ["1000"], "no receipt for the imported message");
}

/// Rule 3: a chat that holds nothing but notices and an unread count without rows has no
/// message a network could name: it is read here and nothing is queued.
#[test]
fn a_chat_without_a_message_queues_no_read_sync() {
    let (_dir, mut h) = harness();
    let g = group();
    let mut chat = ConversationPatch::new(g.clone());
    chat.unread_hint = Some(2);
    let notice = IncomingMessage::new(
        g.clone(),
        "notice-1",
        Origin::Remote {
            author: alice(),
            device: None,
        },
        Body::System(SystemEvent::TitleChanged {
            by: Some(alice()),
            title: "x".into(),
        }),
        700,
        700,
    );
    h.apply(vec![
        BackendEvent::ConversationUpsert(chat),
        batch(&g, vec![notice]),
    ])
    .expect("history");
    assert_eq!(counts(&h, &g), (2, 0));
    let commands = h
        .mark_read(conv(&h, &g))
        .expect("mark read")
        .expect("the hint");
    assert!(commands.is_empty(), "{commands:?}");
    assert_eq!(counts(&h, &g), (0, 0));
}

fn edit(local: &str, at: i64) -> HistoryEdit {
    HistoryEdit {
        local: local.into(),
        at,
    }
}

fn text_of(row: &MessageRow) -> &str {
    match &row.body {
        Body::Text(t) => &t.body,
        other => panic!("a text: {other:?}"),
    }
}

/// Rule 1 (C1): a NEW history row that was edited before the import carries the "edited"
/// mark and one key per version, so what names the NEWEST version's id finds the row: a
/// reaction, a receipt, a quote and a later edit; also a modifier that came before the
/// import. Its id and its place stay the original's.
#[test]
fn an_edited_history_message_is_found_by_its_newest_versions_id() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    // A reaction that names the newest version, before the import: it waits.
    h.apply(vec![BackendEvent::Reaction {
        target: target(Some(&a), Some(&alice()), "200"),
        by: None,
        emoji: "👀".into(),
        remove: false,
        at: 250,
    }])
    .expect("early reaction");
    let mut theirs = incoming(&a, &alice(), 100, true);
    theirs.body = Body::text("as edited");
    theirs.edits = vec![edit("150", 150), edit("200", 200)];
    let mut mine = own(&a, "500", 500, "mine, as edited");
    mine.initial_out_state = Some(OutgoingState::Sent { server_at: None });
    mine.edits = vec![edit("600", 600)];
    let history = || batch(&a, vec![theirs.clone(), mine.clone()]);
    h.apply(vec![history()]).expect("history");

    let got = row(&h, &a, 100);
    assert_eq!((got.revision, got.edited_at), (2, Some(200)));
    assert_eq!(text_of(&got), "as edited");
    assert_eq!(got.reactions.len(), 1, "the reaction that waited");
    let got_mine = row(&h, &a, 500);
    assert_eq!((got_mine.revision, got_mine.edited_at), (1, Some(600)));
    // The edit history shows what was imported: the newest version, at its time.
    let versions = h.revisions(got.id).expect("revisions").expect("some");
    let shown: Vec<(u16, Option<&str>, i64)> = versions
        .revisions
        .iter()
        .map(|v| (v.revision, v.text.as_ref().map(|t| t.body.as_str()), v.at))
        .collect();
    assert_eq!(shown, [(2, Some("as edited"), 200)]);

    // A second delivery changes nothing.
    let before = h.dump(DumpMode::Exact).expect("dump");
    h.apply(vec![history()]).expect("history, again");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before);

    // By the newest version's id: a reaction, a read receipt, a quote, an edit.
    let mut quoting = incoming(&a, &alice(), 700, true);
    quoting.reply_to = Some(ReplyTo {
        target: target(Some(&a), Some(&alice()), "200"),
        author: Some(alice()),
        snapshot: None,
    });
    h.apply(vec![
        BackendEvent::Reaction {
            target: target(Some(&a), Some(&alice()), "200"),
            by: Some(alice()),
            emoji: "👍".into(),
            remove: false,
            at: 710,
        },
        BackendEvent::Receipts {
            targets: vec![target(Some(&a), None, "600")],
            by: alice(),
            kind: ReceiptKind::Read,
            at: 720,
        },
        BackendEvent::Message(quoting),
        BackendEvent::Edited {
            target: target(Some(&a), Some(&alice()), "200"),
            by: Some(alice()),
            new_local: Some("730".into()),
            body: Body::text("edited once more"),
            edited_at: 730,
            keeps_quote: None,
        },
    ])
    .expect("modifiers");
    let got = row(&h, &a, 100);
    assert_eq!(got.reactions.len(), 2);
    assert_eq!((got.revision, got.edited_at), (3, Some(730)));
    assert_eq!(text_of(&got), "edited once more");
    assert_eq!(row(&h, &a, 500).out_state, Some(OutgoingState::Read));
    let quote = row(&h, &a, 700).reply.expect("the quote");
    assert_eq!(
        (quote.state, quote.target),
        (QuoteState::Known, Some(got.id))
    );
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0, "every modifier found the row");
}

/// Rule 1: what the field does not do. A row that is there already ignores it (a live copy
/// came first; a second batch with other versions); a live message's `edits` are not read
/// (a live edit is `Edited`); a notice takes none; of more than the cap the NEWEST are
/// read; a version under the message's own id or a repeated id is passed over.
#[test]
fn the_edits_of_a_history_message_are_read_once_and_bounded() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let edited = |at: i64, edits: Vec<HistoryEdit>| {
        let mut m = incoming(&a, &alice(), at, true);
        m.edits = edits;
        m
    };
    // The row is there (a live copy): the import's versions are not taken.
    h.apply(vec![
        BackendEvent::Message(incoming(&a, &alice(), 100, true)),
        batch(&a, vec![edited(100, vec![edit("150", 150)])]),
    ])
    .expect("live first");
    assert_eq!(row(&h, &a, 100).revision, 0);
    // A live message with `edits`: not read.
    h.apply(vec![BackendEvent::Message(edited(
        200,
        vec![edit("250", 250)],
    ))])
    .expect("live with edits");
    assert_eq!(row(&h, &a, 200).revision, 0);
    // A second batch with more versions of an imported row: ignored.
    h.apply(vec![
        batch(&a, vec![edited(300, vec![edit("350", 350)])]),
        batch(
            &a,
            vec![edited(300, vec![edit("350", 350), edit("360", 360)])],
        ),
    ])
    .expect("twice, differently");
    assert_eq!(row(&h, &a, 300).revision, 1);
    // A notice has no versions.
    let mut notice = edited(400, vec![edit("450", 450)]);
    notice.body = Body::System(SystemEvent::TitleChanged {
        by: Some(alice()),
        title: "x".into(),
    });
    h.apply(vec![batch(&a, vec![notice])]).expect("notice");
    assert_eq!(row(&h, &a, 400).revision, 0);
    // Its own id and a repeated id are no versions; the order given does not matter.
    h.apply(vec![batch(
        &a,
        vec![edited(
            500,
            vec![
                edit("560", 560),
                edit("500", 510),
                edit("550", 550),
                edit("560", 555),
            ],
        )],
    )])
    .expect("untidy");
    let untidy = row(&h, &a, 500);
    assert_eq!((untidy.revision, untidy.edited_at), (2, Some(560)));
    // More than the cap: the newest are read, so the newest id resolves and the oldest of
    // the list does not.
    let many: Vec<HistoryEdit> = (0..i64::try_from(MAX_HISTORY_EDITS).expect("small") + 50)
        .map(|i| edit(&format!("v{i}"), 10_000 + i))
        .collect();
    let newest = many.last().expect("some").local.clone();
    h.apply(vec![batch(&a, vec![edited(600, many)])])
        .expect("many");
    assert_eq!(
        usize::from(row(&h, &a, 600).revision),
        MAX_HISTORY_EDITS,
        "the cap of the limits table"
    );
    let react = |local: &str| BackendEvent::Reaction {
        target: target(Some(&a), Some(&alice()), local),
        by: None,
        emoji: "👍".into(),
        remove: false,
        at: 20_000,
    };
    h.apply(vec![react(&newest), react("v0")])
        .expect("reactions");
    assert_eq!(row(&h, &a, 600).reactions.len(), 1);
    let waiting: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(waiting, 1, "the version that was not read names nothing");
}

fn timer(kind: DisappearKind, secs: u64, started_at: Option<i64>) -> Option<Expiry> {
    Some(Expiry {
        kind,
        timer: Duration::from_secs(secs),
        started_at,
    })
}

/// Rule 4a (D5): an own history message that was not sent (queued, sending or failed on the
/// phone) keeps its timer WITHOUT a start: it is not erased `timer` after a send that never
/// happened, also not by a second delivery of the same row. The phone's later transcript of
/// it (the echo) starts the timer. An own message that was sent counts from its send, as
/// before.
#[test]
fn an_unsent_own_history_message_keeps_its_timer_unstarted_until_its_echo() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let unsent = |at: i64, state: OutgoingState| {
        let mut m = own(&a, &at.to_string(), at, "pending on the phone");
        m.initial_out_state = Some(state);
        m.expiry = timer(DisappearKind::AfterSend, 60, None);
        m
    };
    let failed = OutgoingState::Failed {
        reason: FailReason::Network,
        retryable: false,
    };
    let mut sent = own(&a, "400", 400, "sent long ago");
    sent.initial_out_state = Some(OutgoingState::Delivered);
    sent.expiry = timer(DisappearKind::AfterSend, 60, None);
    let mut kept = own(&a, "500", 500, "sent, with the phone's start");
    kept.expiry = timer(DisappearKind::AfterSend, 60, Some(now - 1_000));
    let history = || {
        batch(
            &a,
            vec![
                unsent(100, OutgoingState::Sending { progress: None }),
                unsent(200, OutgoingState::Queued),
                unsent(300, failed.clone()),
                sent.clone(),
                kept.clone(),
            ],
        )
    };
    h.apply(vec![history()]).expect("history");
    for at in [100, 200, 300] {
        let got = row(&h, &a, at);
        assert_eq!(
            got.expiry,
            timer(DisappearKind::AfterSend, 60, None),
            "{at}: the timer, not started"
        );
    }
    assert_eq!(
        row(&h, &a, 400).expiry,
        timer(DisappearKind::AfterSend, 60, Some(400)),
        "a sent message counts from its send"
    );
    assert_eq!(
        row(&h, &a, 500).expiry,
        timer(DisappearKind::AfterSend, 60, Some(now - 1_000)),
        "a start that is given stands"
    );
    // The same rows again: still not started.
    h.apply(vec![history()]).expect("history, again");
    assert_eq!(row(&h, &a, 100).expiry.and_then(|e| e.started_at), None);
    // The sweeper takes what is due (the message of long ago) and leaves the unsent ones,
    // however late it looks.
    h.set_now(now + 3_600_000);
    h.sweep().expect("sweep");
    let left: Vec<i64> = rows(&h, &a).iter().map(|r| r.sent_at).collect();
    assert_eq!(left, [100, 200, 300]);

    // The phone sends one of them at last: its transcript names the same message.
    let later = now + 3_700_000;
    h.set_now(later);
    h.apply(vec![BackendEvent::Message(own(
        &a,
        "100",
        100,
        "pending on the phone",
    ))])
    .expect("the echo");
    let echoed = row(&h, &a, 100);
    assert!(matches!(echoed.out_state, Some(OutgoingState::Sent { .. })));
    assert_eq!(
        echoed.expiry,
        timer(DisappearKind::AfterSend, 60, Some(later)),
        "the echo starts the timer"
    );
    assert_eq!(rows(&h, &a).len(), 3, "raised, not doubled");
    h.set_now(later + 61_000);
    h.sweep().expect("sweep");
    let left: Vec<i64> = rows(&h, &a).iter().map(|r| r.sent_at).collect();
    assert_eq!(left, [200, 300], "and it goes when its time has come");
}

/// The lead's decision on an own history message the phone calls SENT whose timer has no
/// start: the mapper gives it the timer started at the IMPORT's time, and the core takes a
/// start that is given as it is. A second delivery of the same row (the archive read again
/// later, its start the time of that second import) moves nothing: the core ignores the
/// `expiry` of a copy of a row it has. The message goes when that start plus the timer has
/// passed, and not before (not from its sent time, long ago).
#[test]
fn the_start_of_a_sent_own_history_message_is_the_imports_and_stays() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let day = 86_400;
    let sent_long_ago = |start: i64| {
        let mut m = own(&a, "100", 100, "sent two days ago");
        m.initial_out_state = Some(OutgoingState::Delivered);
        m.expiry = timer(DisappearKind::AfterSend, day, Some(start));
        m
    };
    h.apply(vec![batch(&a, vec![sent_long_ago(now)])])
        .expect("history");
    assert_eq!(
        row(&h, &a, 100).expiry,
        timer(DisappearKind::AfterSend, day, Some(now)),
        "the import's time, as given"
    );
    // The archive is read again an hour later: that import's time is not the start.
    let later = now + 3_600_000;
    h.set_now(later);
    h.apply(vec![batch(&a, vec![sent_long_ago(later)])])
        .expect("history, again");
    assert_eq!(
        row(&h, &a, 100).expiry,
        timer(DisappearKind::AfterSend, day, Some(now)),
        "the first start stands"
    );
    assert_eq!(rows(&h, &a).len(), 1);
    // Not erased for the days the phone has had it; erased when its day after the import
    // has passed.
    h.set_now(now + 86_400_000 - 1);
    h.sweep().expect("sweep");
    assert_eq!(
        rows(&h, &a).len(),
        1,
        "a day has not passed since the import"
    );
    h.set_now(now + 86_400_000 + 1);
    h.sweep().expect("sweep");
    assert!(rows(&h, &a).is_empty(), "and then it goes");
}

fn call(conv: &ConvKey, id: &str, state: CallState, at: i64, read: bool) -> IncomingMessage {
    let origin = if matches!(state, CallState::Missed) {
        Origin::Remote {
            author: alice(),
            device: None,
        }
    } else {
        Origin::OwnOtherDevice { device: None }
    };
    let mut m = IncomingMessage::call(
        conv.clone(),
        id,
        origin,
        CallLine::signal(CallDirection::Incoming, false, state, at)
            .with_media(CallMedia::Voice, at),
        at,
        at,
    );
    m.read = read;
    m
}

/// Rule 4b (D5): a call line of a history batch honours the `expiry` its signal states and
/// its `read` flag. A missed call the phone had not seen is unread and counts; its
/// after-read timer starts when the chat is read here. A line that is not unread starts
/// such a timer when it is stored; a start that is given stands. The chat's own timer is
/// not taken for an imported line, and none of them is announced.
#[test]
fn a_call_line_of_the_history_honours_its_timer_and_its_read_flag() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let mut chat = ConversationPatch::new(a.clone());
    chat.timer = Patch::Set(Duration::from_secs(300));
    let with = |mut m: IncomingMessage, expiry: Option<Expiry>| {
        m.expiry = expiry;
        m
    };
    let said = h
        .apply_announcing(vec![
            BackendEvent::ConversationUpsert(chat),
            batch(
                &a,
                vec![
                    with(
                        call(&a, "1", CallState::Missed, 100, false),
                        timer(DisappearKind::AfterRead, 60, None),
                    ),
                    with(
                        call(&a, "2", CallState::Missed, 200, true),
                        timer(DisappearKind::AfterRead, 60, None),
                    ),
                    with(
                        call(&a, "3", CallState::Answered, 300, false),
                        timer(DisappearKind::AfterRead, 60, None),
                    ),
                    with(
                        call(&a, "4", CallState::Answered, 400, true),
                        timer(DisappearKind::AfterRead, 60, Some(now - 5_000)),
                    ),
                    with(call(&a, "5", CallState::Declined, 500, true), None),
                ],
            ),
        ])
        .expect("history");
    assert_eq!((said.notify.len(), said.catch_up), (0, 0));
    assert_eq!(
        counts(&h, &a),
        (1, 0),
        "the missed call the phone had not seen"
    );
    let line = |h: &IngestHarness, at: i64| row(h, &a, at);
    assert_eq!(line(&h, 100).read_state, Some(ReadState::Unread));
    assert_eq!(
        line(&h, 100).expiry,
        timer(DisappearKind::AfterRead, 60, None),
        "unread: it starts when it is read"
    );
    assert_eq!(line(&h, 200).read_state, Some(ReadState::Read));
    assert_eq!(
        line(&h, 200).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now))
    );
    assert_eq!(
        line(&h, 300).read_state,
        None,
        "an answered call never counts"
    );
    assert_eq!(
        line(&h, 300).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now)),
        "never unread: nothing else would start it"
    );
    assert_eq!(
        line(&h, 400).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now - 5_000)),
        "the phone's start stands"
    );
    assert_eq!(line(&h, 500).expiry, None, "not the chat's timer");

    // Read here: the missed call's timer starts, and nobody is told about a call line.
    h.set_now(now + 10_000);
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("the missed call");
    assert!(commands.is_empty(), "{commands:?}");
    assert_eq!(counts(&h, &a), (0, 0));
    assert_eq!(
        line(&h, 100).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now + 10_000))
    );
    // The lines go when their time has come: 4 first, then 2 and 3, then 1; 5 stays.
    h.set_now(now + 65_000);
    h.sweep().expect("sweep");
    let left: Vec<i64> = rows(&h, &a).iter().map(|r| r.sent_at).collect();
    assert_eq!(left, [100, 500]);
    h.set_now(now + 75_000);
    h.sweep().expect("sweep");
    let left: Vec<i64> = rows(&h, &a).iter().map(|r| r.sent_at).collect();
    assert_eq!(left, [500]);
}

/// Rule 4b: the phone's read marker reads an imported missed call like the chat's messages,
/// and starts its after-read timer.
#[test]
fn a_read_marker_reads_an_imported_missed_call_and_starts_its_timer() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let mut missed = call(&a, "1", CallState::Missed, 200, false);
    missed.expiry = timer(DisappearKind::AfterRead, 60, None);
    h.apply(vec![batch(
        &a,
        vec![
            incoming(&a, &alice(), 100, false),
            missed,
            incoming(&a, &alice(), 300, false),
        ],
    )])
    .expect("history");
    assert_eq!(counts(&h, &a), (3, 0));
    h.apply(vec![BackendEvent::ReadMarker {
        conv: Some(a.clone()),
        up_to: target(Some(&a), Some(&alice()), "300"),
    }])
    .expect("the phone read the chat");
    assert_eq!(counts(&h, &a), (0, 0));
    assert_eq!(
        row(&h, &a, 200).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now))
    );
}

/// Rule 4b: an imported missed call whose after-read timer waits for the reading, and what
/// reads it without a reading of the chat: the phone's later word that the call was
/// answered (the line moves on and has no read state any more), and a signal that shows
/// the call began before the point the phone had read up to. Each starts the timer, or
/// nothing ever would.
#[test]
fn an_imported_missed_call_that_stops_being_unread_starts_its_timer() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let missed = |id: &str, at: i64| {
        let mut m = call(&a, id, CallState::Missed, at, false);
        m.expiry = timer(DisappearKind::AfterRead, 60, None);
        m
    };
    h.apply(vec![
        batch(
            &a,
            vec![
                missed("1", 200),
                incoming(&a, &alice(), 400, true),
                missed("2", 500),
            ],
        ),
        // The phone had read the chat up to the message between the two calls.
        BackendEvent::ReadMarker {
            conv: Some(a.clone()),
            up_to: target(Some(&a), Some(&alice()), "400"),
        },
    ])
    .expect("history");
    assert_eq!(counts(&h, &a), (1, 0), "the call after the marker");
    assert_eq!(row(&h, &a, 200).read_state, Some(ReadState::Read));
    assert_eq!(
        row(&h, &a, 500).expiry,
        timer(DisappearKind::AfterRead, 60, None),
        "unread: not started"
    );
    // Another signal of call 2 says it began earlier, before the marker: it is read.
    h.set_now(now + 1_000);
    h.apply(vec![batch(&a, vec![missed("2", 300)])])
        .expect("an earlier signal");
    let line = row(&h, &a, 300);
    assert_eq!(line.read_state, Some(ReadState::Read));
    assert_eq!(
        line.expiry,
        timer(DisappearKind::AfterRead, 60, Some(now + 1_000))
    );
    assert_eq!(counts(&h, &a), (0, 0));

    // A missed call nobody read, and then the phone says it was answered after all.
    h.apply(vec![batch(&a, vec![missed("3", 900)])])
        .expect("history");
    assert_eq!(counts(&h, &a), (1, 0));
    h.set_now(now + 2_000);
    h.apply(vec![BackendEvent::Message(call(
        &a,
        "3",
        CallState::Answered,
        910,
        false,
    ))])
    .expect("the phone's sync");
    let line = row(&h, &a, 900);
    assert_eq!(line.read_state, None, "an answered call counts no more");
    assert_eq!(
        line.expiry,
        timer(DisappearKind::AfterRead, 60, Some(now + 2_000))
    );
    assert_eq!(counts(&h, &a), (0, 0));
}

/// Rule 4c (D5): a notice somebody else caused is never unread, so nothing would ever
/// start its after-read timer: it starts when the notice is stored. A start that is given
/// stands.
#[test]
fn a_history_notice_with_an_unstarted_after_read_timer_starts_at_ingest() {
    let (_dir, mut h) = harness();
    let g = group();
    let now = h.now();
    let notice = |local: &str, at: i64, expiry: Option<Expiry>| {
        let mut m = IncomingMessage::new(
            g.clone(),
            local,
            Origin::Remote {
                author: alice(),
                device: None,
            },
            Body::System(SystemEvent::TitleChanged {
                by: Some(alice()),
                title: "x".into(),
            }),
            at,
            at,
        );
        m.expiry = expiry;
        m
    };
    let mut unread = incoming(&g, &alice(), 300, false);
    unread.expiry = timer(DisappearKind::AfterRead, 60, None);
    h.apply(vec![batch(
        &g,
        vec![
            notice("n1", 100, timer(DisappearKind::AfterRead, 60, None)),
            notice(
                "n2",
                200,
                timer(DisappearKind::AfterRead, 60, Some(now - 30_000)),
            ),
            unread,
        ],
    )])
    .expect("history");
    assert_eq!(row(&h, &g, 100).read_state, None);
    assert_eq!(
        row(&h, &g, 100).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now)),
        "it starts when it is stored"
    );
    assert_eq!(
        row(&h, &g, 200).expiry,
        timer(DisappearKind::AfterRead, 60, Some(now - 30_000))
    );
    assert_eq!(
        row(&h, &g, 300).expiry,
        timer(DisappearKind::AfterRead, 60, None),
        "an unread message still waits for its reading"
    );
    assert_eq!(counts(&h, &g), (1, 0));
    h.set_now(now + 61_000);
    h.sweep().expect("sweep");
    let left: Vec<i64> = rows(&h, &g).iter().map(|r| r.sent_at).collect();
    assert_eq!(left, [300], "both notices went, the unread message stays");
}

/// Schema 12 (the migration of this rule): a profile of an earlier build holds unread rows of
/// a history import that the old rule did not count, under an index that left them out.
/// The step replaces the index and counts every conversation anew: such a chat shows its
/// unread rows, a chat without them shows what it showed, and a hint stays a hint.
#[test]
fn schema_12_counts_the_unread_history_rows_of_an_earlier_build() {
    let (_dir, mut h) = harness();
    let (a, g) = (d_alice(), group());
    let mut hinted = ConversationPatch::new(g.clone());
    hinted.unread_hint = Some(4);
    let mut mention = incoming(&a, &alice(), 300, false);
    mention.mentions_me = true;
    h.apply(vec![
        batch(
            &a,
            vec![
                incoming(&a, &alice(), 100, true),
                incoming(&a, &alice(), 200, false),
                mention,
            ],
        ),
        BackendEvent::ConversationUpsert(hinted),
        BackendEvent::Message(incoming(&g, &alice(), 400, false)),
    ])
    .expect("rows");
    assert_eq!((counts(&h, &a), counts(&h, &g)), ((2, 1), (5, 0)));
    // The state schema 11 left: the old index, and counters that passed the history rows by.
    h.connection()
        .execute_batch(
            "DROP INDEX messages_unread;
             CREATE INDEX messages_unread ON messages(conversation_id)
               WHERE read_state = 'Unread' AND history = 0 AND deleted IS NULL;
             UPDATE conversations SET unread = unread_hint + (SELECT count(*) FROM messages m
                 WHERE m.conversation_id = conversations.id AND m.read_state = 'Unread'
                   AND m.history = 0),
               mentions = 0;",
        )
        .expect("the old state");
    assert_eq!((counts(&h, &a), counts(&h, &g)), ((0, 0), (5, 0)));
    h.connection()
        .execute_batch(include_str!("../migrations/0012_unread_history.sql"))
        .expect("schema 12");
    assert_eq!((counts(&h, &a), counts(&h, &g)), ((2, 1), (5, 0)));
    let index: String = h
        .connection()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'messages_unread'",
            [],
            |r| r.get(0),
        )
        .expect("the index");
    assert!(!index.contains("history"), "{index}");
}

/// Link feedback L5 (`buttons.md` 1.5): a chat deleted while the rest of its history is still
/// to be delivered stays deleted. The imported rows that come afterwards (also a call line of
/// the chat) are dropped: no row, no listing, no failure. A LIVE message of that chat brings
/// it back, as before.
#[test]
fn an_imported_row_of_a_deleted_chat_is_dropped_and_a_live_message_brings_it_back() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    h.apply(vec![batch(
        &a,
        vec![
            incoming(&a, &alice(), 100, true),
            incoming(&a, &alice(), 110, true),
        ],
    )])
    .expect("the first rows");
    let id = conv(&h, &a);
    h.clear_conversation(id, true).expect("delete the chat");
    fn listed(h: &IngestHarness) -> usize {
        h.conversations(&ConversationListQuery::default())
            .expect("list")
            .rows
            .len()
    }
    assert_eq!(listed(&h), 0, "the delete took it from the list");

    // The rest of the history: texts, a call line, a row nobody has seen.
    h.apply(vec![BackendEvent::HistoryBatch {
        conv: a.clone(),
        messages: vec![
            incoming(&a, &alice(), 120, false),
            incoming(&a, &alice(), 130, true),
            call(&a, "c1", CallState::Missed, 140, false),
        ],
        has_more: true,
        progress: Some(0.5),
    }])
    .expect("the rest is not a failure");
    assert_eq!(
        listed(&h),
        0,
        "the chat did not come back with old messages"
    );
    let timeline = h
        .timeline(&TimelineQuery {
            conv: id,
            anchor: TimelineAnchor::Newest,
            limit: 500,
        })
        .expect("timeline")
        .rows;
    assert!(
        timeline.is_empty(),
        "nothing was stored: {}",
        timeline.len()
    );

    // News is not history: a live message lists the chat again.
    h.apply(vec![BackendEvent::Message(incoming(
        &a,
        &alice(),
        500,
        false,
    ))])
    .expect("a live message");
    assert_eq!(listed(&h), 1, "a live message brings the chat back");
    assert_eq!(rows(&h, &a).len(), 1);
}
