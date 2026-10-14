//! Independent review of step S3a (the rules of the contract and the core for a history
//! import; `tests/history_rules.rs` is the builder's). Each test here walks a path the
//! builder's tests do not. A test that shows a defect is `#[ignore = "review: …"]`, so the
//! tree stays green; the review's report is `target/s3-context/review-a.md`.

use std::time::Duration;

use kourier_backend_api::{BackendEvent, DurableCommand, HistoryEdit, IncomingMessage};
use kourier_core::facade::{ConversationListQuery, MessageRow, TimelineAnchor, TimelineQuery};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::scripts::{own, remote, target};
use kourier_model::{
    AccountSettingsPatch, Body, CallDirection, CallLine, CallMedia, CallState, ConvKey,
    ConversationId, ConversationPatch, DisappearKind, Expiry, MsgIdScope, MsgTarget, NetworkKind,
    Origin, OutgoingState, Placeholder, ReadState, ReceiptKind, SystemEvent, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";
const ALICE: &str = "aci:00000000-0000-4000-8000-0000000a11ce";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn alice() -> UserId {
    uid(ALICE)
}

fn d_alice() -> ConvKey {
    ConvKey::Direct(alice())
}

fn harness_in(dir: &std::path::Path) -> IngestHarness {
    IngestHarness::open(
        &dir.join("core.db"),
        &kourier_db::DbKey::from([5; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness")
}

fn harness() -> (tempfile::TempDir, IngestHarness) {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = harness_in(dir.path());
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

fn locals(targets: &[MsgTarget]) -> Vec<&str> {
    targets.iter().map(|t| t.local.as_ref()).collect()
}

fn timer(kind: DisappearKind, secs: u64, started_at: Option<i64>) -> Option<Expiry> {
    Some(Expiry {
        kind,
        timer: Duration::from_secs(secs),
        started_at,
    })
}

/// What `mark_read` queued: (the read sync's `up_to`, its list, the receipts' list).
fn told(commands: &[DurableCommand]) -> (Option<String>, Vec<String>, Vec<String>) {
    let mut up = None;
    let mut synced = Vec::new();
    let mut receipts = Vec::new();
    for command in commands {
        match command {
            DurableCommand::SyncReadMarker { up_to, read, .. } => {
                up = Some(up_to.local.to_string());
                synced = locals(read).into_iter().map(str::to_owned).collect();
            }
            DurableCommand::SendReceipts { targets, .. } => {
                receipts = locals(targets).into_iter().map(str::to_owned).collect();
            }
            other => panic!("reading queues no {other:?}"),
        }
    }
    (up, synced, receipts)
}

/// Rule 3, the paths the brief names for a receipt to the sender of an imported message:
/// the live copy of the same message that arrives AFTER the import (a message the phone
/// got between the link and its export is in the archive and in the new device's queue),
/// a re-read, and a second delivery. None announces the row and none earns the sender a
/// receipt; the row stays an imported one.
#[test]
fn review_the_live_copy_of_an_imported_message_earns_no_notification_and_no_receipt() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    h.apply(vec![
        read_receipts_on(),
        batch(&a, vec![incoming(&a, &alice(), 100, false)]),
    ])
    .expect("history");
    let said = h
        .apply_announcing(vec![BackendEvent::Message(incoming(
            &a,
            &alice(),
            100,
            false,
        ))])
        .expect("the live copy");
    assert_eq!((said.notify.len(), said.catch_up), (0, 0));
    assert_eq!(counts(&h, &a), (1, 0), "once");
    assert!(row(&h, &a, 100).history, "it stays an imported row");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let (up, synced, receipts) = told(&commands);
    assert_eq!(up.as_deref(), Some("100"));
    assert_eq!(synced, ["100"]);
    assert!(receipts.is_empty(), "no receipt: {receipts:?}");
    // A second delivery and a second read: nothing more.
    h.apply(vec![batch(&a, vec![incoming(&a, &alice(), 100, false)])])
        .expect("again");
    assert!(h.mark_read(conv(&h, &a)).expect("mark read").is_none());
}

/// Rule 3, the other order: the live copy came FIRST (it was received here), so the import
/// of the same message does not turn it into an imported row and its sender gets the
/// receipt a live message earns.
#[test]
fn review_a_message_received_live_keeps_its_receipt_when_the_import_brings_it_again() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    h.apply(vec![
        read_receipts_on(),
        BackendEvent::Message(incoming(&a, &alice(), 100, false)),
        batch(&a, vec![incoming(&a, &alice(), 100, false)]),
    ])
    .expect("live, then the import");
    assert!(!row(&h, &a, 100).history);
    assert_eq!(counts(&h, &a), (1, 0));
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let (_, synced, receipts) = told(&commands);
    assert_eq!(
        (synced, receipts),
        (vec!["100".to_owned()], vec!["100".to_owned()])
    );
}

/// Rule 2, "each lowers the count by exactly the rows it touches, for history and live rows
/// alike": a delete for me, a delete for everyone, a timer that runs out, a read marker.
#[test]
fn review_every_way_a_row_stops_being_unread_lowers_the_count_by_that_row() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let mut timed = incoming(&a, &alice(), 300, false);
    timed.expiry = timer(DisappearKind::AfterSend, 60, Some(now - 30_000));
    let mut mention = incoming(&a, &alice(), 350, false);
    mention.mentions_me = true;
    h.apply(vec![
        batch(
            &a,
            vec![
                incoming(&a, &alice(), 100, false),
                incoming(&a, &alice(), 200, false),
                timed,
                mention,
            ],
        ),
        BackendEvent::Message(incoming(&a, &alice(), 400, false)),
    ])
    .expect("four imported, one live");
    assert_eq!(counts(&h, &a), (5, 1));
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&a), Some(&alice()), "100"),
        by: None,
        for_everyone: false,
        at: 500,
    }])
    .expect("deleted for me on the phone");
    assert_eq!(counts(&h, &a), (4, 1));
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&a), Some(&alice()), "200"),
        by: Some(alice()),
        for_everyone: true,
        at: 510,
    }])
    .expect("deleted for everyone by its author");
    assert_eq!(counts(&h, &a), (3, 1));
    h.set_now(now + 31_000);
    h.sweep().expect("sweep");
    assert_eq!(counts(&h, &a), (2, 1), "the timer of the third ran out");
    h.apply(vec![BackendEvent::ReadMarker {
        conv: Some(a.clone()),
        up_to: target(Some(&a), Some(&alice()), "350"),
    }])
    .expect("the phone read the mention");
    assert_eq!(counts(&h, &a), (1, 0), "the live one is left");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let (up, synced, _) = told(&commands);
    assert_eq!(
        (up.as_deref(), synced),
        (Some("400"), vec!["400".to_owned()])
    );
    assert_eq!(counts(&h, &a), (0, 0));
}

/// Rule 2, what the contract forbids and a backend may do anyway: a hint AND rows for the
/// same messages. They count twice (as the contract warns), the phone's read lowers only the
/// rows, and a read here clears both. Nothing breaks; nothing is told twice.
#[test]
fn review_a_hint_beside_rows_for_the_same_messages_counts_twice_and_clears_at_once() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let mut chat = ConversationPatch::new(a.clone());
    chat.unread_hint = Some(2);
    h.apply(vec![
        batch(
            &a,
            vec![
                incoming(&a, &alice(), 100, false),
                incoming(&a, &alice(), 200, false),
            ],
        ),
        BackendEvent::ConversationUpsert(chat),
    ])
    .expect("rows and a hint for them");
    assert_eq!(counts(&h, &a), (4, 0), "twice, as the contract says");
    h.apply(vec![BackendEvent::ReadMarker {
        conv: Some(a.clone()),
        up_to: target(Some(&a), Some(&alice()), "200"),
    }])
    .expect("the phone read both");
    assert_eq!(counts(&h, &a), (2, 0), "the hint does not follow the phone");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("the hint");
    let (up, synced, receipts) = told(&commands);
    assert_eq!(up.as_deref(), Some("200"));
    assert!(synced.is_empty() && receipts.is_empty());
    assert_eq!(counts(&h, &a), (0, 0));
}

/// Rule 3, `up_to` walked over the rows the brief names: the newest row is an own message,
/// then a message deleted for everyone, then a row that is hidden because it is due on an
/// unconfirmed clock. `up_to` is always a row somebody else wrote that is no notice.
#[test]
fn review_up_to_is_a_message_of_somebody_else_whatever_the_newest_row_is() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let mut due = incoming(&a, &alice(), 500, false);
    due.expiry = timer(DisappearKind::AfterSend, 1, Some(now - 10_000));
    h.apply(vec![batch(
        &a,
        vec![
            incoming(&a, &alice(), 100, false),
            incoming(&a, &alice(), 200, true),
            own(&a, "300", 300, "mine, the newest shown message"),
            due,
        ],
    )])
    .expect("history");
    // The newest row of somebody else is due and hidden until the clock is confirmed.
    h.sweep_unconfirmed(kourier_core::facade::ExpiryPolicy::default())
        .expect("sweep, unconfirmed");
    assert_eq!(counts(&h, &a), (1, 0), "a hidden row does not count");
    // 200 is deleted for everyone: what is left of it is still a row a network can name.
    h.apply(vec![BackendEvent::Deleted {
        target: target(Some(&a), Some(&alice()), "200"),
        by: Some(alice()),
        for_everyone: true,
        at: 600,
    }])
    .expect("delete");
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let (up, synced, _) = told(&commands);
    assert_eq!(synced, ["100"], "the hidden row is not read");
    assert_eq!(
        up.as_deref(),
        Some("200"),
        "the newest row of somebody else that is shown"
    );
}

/// Rule 2 and the placeholder path of `existing()`: a history row that could not be read
/// (an undecryptable placeholder) and its resend that arrives live. The contract says a
/// history row NEVER notifies; the resend is a live message.
#[test]
fn review_a_live_resend_that_fills_an_imported_placeholder() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let mut hole = incoming(&a, &alice(), 100, false);
    hole.body = Body::Placeholder(Placeholder::Undecryptable { resendable: true });
    h.apply(vec![read_receipts_on(), batch(&a, vec![hole])])
        .expect("history");
    assert_eq!(counts(&h, &a), (1, 0));
    let said = h
        .apply_announcing(vec![BackendEvent::Message(incoming(
            &a,
            &alice(),
            100,
            false,
        ))])
        .expect("the resend");
    let filled = row(&h, &a, 100);
    assert!(matches!(filled.body, Body::Text(_)));
    // What the code does today (documented, not judged here): the row is announced like a
    // live message although it stays `history`, and its sender gets no receipt.
    assert_eq!(said.notify.len(), 1, "announced as the live message it is");
    assert!(filled.history);
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let (_, synced, receipts) = told(&commands);
    assert_eq!(synced, ["100"]);
    assert!(receipts.is_empty());
}

fn edit(local: &str, at: i64) -> HistoryEdit {
    HistoryEdit {
        local: local.into(),
        at,
    }
}

/// Rule 1, an id used twice: a history message whose `edits` name the id of ANOTHER message
/// of the same author. A reference to that id must mean the message that HAS the id. It
/// does once both are stored; but a modifier that waited for the other message is taken by
/// the edited one when that is imported first.
#[test]
#[ignore = "review, left (fix round): a version id that is another message's id takes the \
            modifiers that waited for that message. Not decidable at the import: the bytes of \
            a reaction parked under an id are the same for a version and for a message that \
            is not stored yet, and skipping a version whose id is a stored message (the \
            review's correction) changes nothing in this order. Needs a collision of two \
            sent times of one author"]
fn review_a_version_under_another_messages_id_does_not_take_its_modifiers() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    // A reaction to Alice's message "300", before any of the chat is here.
    h.apply(vec![BackendEvent::Reaction {
        target: target(Some(&a), Some(&alice()), "300"),
        by: None,
        emoji: "👍".into(),
        remove: false,
        at: 900,
    }])
    .expect("early reaction");
    let mut edited = incoming(&a, &alice(), 100, true);
    edited.edits = vec![edit("300", 150)];
    h.apply(vec![batch(&a, vec![edited])])
        .expect("the older row");
    h.apply(vec![batch(&a, vec![incoming(&a, &alice(), 300, true)])])
        .expect("the message that has the id");
    assert_eq!(
        (
            row(&h, &a, 100).reactions.len(),
            row(&h, &a, 300).reactions.len()
        ),
        (0, 1),
        "the reaction named the message 300"
    );
}

/// Rule 1, the same collision once both rows are there: the message wins over a version.
#[test]
fn review_a_reference_to_an_id_means_the_message_that_has_it_not_a_version() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let mut edited = incoming(&a, &alice(), 100, true);
    edited.edits = vec![edit("300", 150)];
    h.apply(vec![
        batch(&a, vec![edited]),
        batch(&a, vec![incoming(&a, &alice(), 300, true)]),
        BackendEvent::Reaction {
            target: target(Some(&a), Some(&alice()), "300"),
            by: None,
            emoji: "👍".into(),
            remove: false,
            at: 900,
        },
    ])
    .expect("both, then the reaction");
    assert_eq!(
        (
            row(&h, &a, 100).reactions.len(),
            row(&h, &a, 300).reactions.len()
        ),
        (0, 1)
    );
}

/// Rule 1, the row exists as a placeholder (a live message that could not be decrypted) and
/// the import brings the message, edited: the body is taken (placeholder replacement) but
/// the versions are not, so the row shows the newest text without the "edited" mark and a
/// modifier that names the newest version's id finds nothing.
#[test]
fn review_the_import_that_fills_a_placeholder_brings_its_versions() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let mut hole = incoming(&a, &alice(), 100, false);
    hole.body = Body::Placeholder(Placeholder::Undecryptable { resendable: true });
    let mut edited = incoming(&a, &alice(), 100, true);
    edited.body = Body::text("as edited");
    edited.edits = vec![edit("150", 150)];
    h.apply(vec![
        BackendEvent::Message(hole),
        batch(&a, vec![edited]),
        BackendEvent::Reaction {
            target: target(Some(&a), Some(&alice()), "150"),
            by: None,
            emoji: "👍".into(),
            remove: false,
            at: 900,
        },
    ])
    .expect("placeholder, import, reaction");
    let got = row(&h, &a, 100);
    assert!(matches!(&got.body, Body::Text(t) if t.body == "as edited"));
    assert_eq!((got.revision, got.reactions.len()), (1, 1));
}

/// Rule 1, an own imported edited message and the outgoing side: the versions are kept
/// under the own author, a receipt that names the ORIGINAL id and one that names the newest
/// version both land, and a second batch that says "edited" of an own row that came without
/// versions changes nothing.
#[test]
fn review_an_own_edited_history_message_takes_receipts_under_either_id() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let mut mine = own(&a, "500", 500, "mine, as edited");
    mine.initial_out_state = Some(OutgoingState::Sent { server_at: None });
    mine.edits = vec![edit("600", 600)];
    h.apply(vec![
        batch(&a, vec![mine]),
        BackendEvent::Receipts {
            targets: vec![target(Some(&a), None, "500")],
            by: alice(),
            kind: ReceiptKind::Delivered,
            at: 700,
        },
    ])
    .expect("history and a receipt for the original");
    assert_eq!(row(&h, &a, 500).out_state, Some(OutgoingState::Delivered));
    h.apply(vec![BackendEvent::Receipts {
        targets: vec![target(Some(&a), None, "600")],
        by: alice(),
        kind: ReceiptKind::Read,
        at: 710,
    }])
    .expect("a receipt for the newest version");
    assert_eq!(row(&h, &a, 500).out_state, Some(OutgoingState::Read));
}

fn missed_ring(conv: &ConvKey, id: &str, at: i64, deadline: i64, read: bool) -> IncomingMessage {
    let mut m = IncomingMessage::call(
        conv.clone(),
        id,
        Origin::Remote {
            author: alice(),
            device: None,
        },
        CallLine::signal(
            CallDirection::Incoming,
            false,
            CallState::Ringing { deadline },
            at,
        )
        .with_media(CallMedia::Voice, at),
        at,
        at,
    );
    m.read = read;
    m
}

/// Rule 4b and the deadline task: a call that still "rings" in a history batch (the import
/// ran within the ring's two minutes) and says `read: true`. It is never announced, by the
/// signal or by the clock. The clock makes it a missed call; the contract says an imported
/// line "is unread only when its `read` flag says so".
#[test]
#[ignore = "review, left (fix round; needs the lead): an imported ring that says `read: true` \
            becomes an UNREAD missed call when its deadline passes (the flag is not kept; \
            never announced). Stated in the contract (`IncomingMessage::call`). A real fix \
            persists a read state on a ringing line and raises it from a later signal, which \
            breaks the invariant the any-order convergence of lines rests on. A Signal \
            archive carries a ring only for a GROUP call that rings at the moment the phone \
            exports (individual calls have no ringing state), and S3b's mapper can give such \
            a line `read: false`, which is then right"]
fn review_an_imported_ring_that_says_read_is_not_unread_when_the_clock_settles_it() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let said = h
        .apply_announcing(vec![batch(
            &a,
            vec![missed_ring(&a, "9", now - 10_000, now + 50_000, true)],
        )])
        .expect("history");
    assert_eq!((said.notify.len(), said.catch_up), (0, 0));
    assert_eq!(counts(&h, &a), (0, 0), "it rings: nothing is unread");
    assert!(h.next_call_deadline().expect("deadline").is_some());
    h.set_now(now + 60_000);
    let said = h.settle_calls().expect("the deadline task");
    assert_eq!(
        (said.notify.len(), said.catch_up),
        (0, 0),
        "never announced"
    );
    let line = rows(&h, &a).pop().expect("the line");
    assert!(matches!(
        &line.body,
        Body::System(SystemEvent::Call(l)) if l.state == CallState::Missed
    ));
    assert_eq!(line.read_state, Some(ReadState::Read), "it said read");
    assert_eq!(counts(&h, &a), (0, 0));
}

/// The same line with `read: false`: unread and counted once the clock settles it, and
/// announced to nobody (not alone, not in the catch-up summary).
#[test]
fn review_an_imported_ring_settled_by_the_clock_is_never_announced() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    h.apply(vec![batch(
        &a,
        vec![missed_ring(&a, "9", now - 10_000, now + 50_000, false)],
    )])
    .expect("history");
    for late in [60_000, 3_600_000] {
        h.set_now(now + late);
        let said = h.settle_calls().expect("the deadline task");
        assert_eq!((said.notify.len(), said.catch_up), (0, 0));
    }
    assert_eq!(counts(&h, &a), (1, 0), "a missed call nobody saw");
}

/// Rule 4a, what the builder left: an own history row that was pending on the phone, and
/// what arrives for it afterwards. The echo starts the timer whichever state the row has by
/// then (a receipt may come before the transcript); a receipt alone does not.
#[test]
fn review_the_echo_starts_the_timer_of_a_pending_row_also_after_a_receipt() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    let now = h.now();
    let mut pending = own(&a, "100", 100, "pending on the phone");
    pending.initial_out_state = Some(OutgoingState::Sending { progress: None });
    pending.expiry = timer(DisappearKind::AfterSend, 60, None);
    h.apply(vec![
        batch(&a, vec![pending]),
        BackendEvent::Receipts {
            targets: vec![target(Some(&a), None, "100")],
            by: alice(),
            kind: ReceiptKind::Read,
            at: 500,
        },
    ])
    .expect("history, then the recipient's read receipt");
    let got = row(&h, &a, 100);
    assert_eq!(got.out_state, Some(OutgoingState::Read));
    assert_eq!(
        got.expiry,
        timer(DisappearKind::AfterSend, 60, None),
        "known and left by the builder: a receipt alone starts nothing"
    );
    h.set_now(now + 5_000);
    h.apply(vec![BackendEvent::Message(own(
        &a,
        "100",
        100,
        "pending on the phone",
    ))])
    .expect("the transcript, late");
    assert_eq!(
        row(&h, &a, 100).expiry,
        timer(DisappearKind::AfterSend, 60, Some(now + 5_000))
    );
}

/// Any order: an import with unread rows, a row delivered twice, the phone's read marker,
/// the live copy of one row and a live message end in the same counts and read states
/// whatever order the six deliveries come in (the `history` flag and the sort key of the row
/// that has a live copy depend on which copy came first, by design: they are left out).
#[test]
fn review_unread_history_rows_converge_in_any_order() {
    let a = d_alice();
    let deliveries = || -> Vec<Vec<BackendEvent>> {
        let mut edited = incoming(&a, &alice(), 250, false);
        edited.edits = vec![edit("260", 260)];
        vec![
            vec![batch(
                &a,
                vec![
                    incoming(&a, &alice(), 100, false),
                    incoming(&a, &alice(), 200, false),
                    incoming(&a, &alice(), 300, false),
                ],
            )],
            vec![batch(
                &a,
                vec![
                    incoming(&a, &alice(), 150, false),
                    edited,
                    incoming(&a, &alice(), 200, false),
                ],
            )],
            vec![BackendEvent::ReadMarker {
                conv: Some(a.clone()),
                up_to: target(Some(&a), Some(&alice()), "200"),
            }],
            vec![BackendEvent::Reaction {
                target: target(Some(&a), Some(&alice()), "260"),
                by: None,
                emoji: "👍".into(),
                remove: false,
                at: 900,
            }],
            vec![BackendEvent::Message(incoming(&a, &alice(), 400, false))],
            vec![batch(&a, vec![incoming(&a, &alice(), 300, true)])],
        ]
    };
    // Every order of the six deliveries.
    fn orders(n: usize) -> Vec<Vec<usize>> {
        if n == 0 {
            return vec![Vec::new()];
        }
        let mut out = Vec::new();
        for rest in orders(n - 1) {
            for at in 0..=rest.len() {
                let mut one = rest.clone();
                one.insert(at, n - 1);
                out.push(one);
            }
        }
        out
    }
    let summary = |h: &IngestHarness| {
        let states: Vec<(i64, Option<ReadState>, u16, usize)> = rows(h, &a)
            .iter()
            .map(|r| (r.sent_at, r.read_state, r.revision, r.reactions.len()))
            .collect();
        (counts(h, &a), states)
    };
    let mut first = None;
    for order in orders(6) {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut h = harness_in(dir.path());
        let all = deliveries();
        for i in &order {
            h.apply(all[*i].clone()).expect("delivery");
        }
        let mut got = summary(&h);
        got.1.sort_by_key(|s| s.0);
        match &first {
            None => first = Some(got),
            Some(expected) => assert_eq!(&got, expected, "order {order:?}"),
        }
    }
    let (count, states) = first.expect("some order");
    // 100, 150 and 200 are under the marker, 300 is read by its second copy; 250 and the
    // live 400 are unread.
    assert_eq!(count, (2, 0));
    assert_eq!(states.len(), 6);
    let _ = DumpMode::Exact;
}

/// Schema 12 on a profile that schema 11 really left behind, through the real migration
/// runner: an import delivered by the step before this one (S2) stored the phone's unread
/// messages as unread history rows AND gave their number as the chat's `unread_hint` (the
/// delivery sent it with the chat's last batch). Under schema 11 the chat showed the hint,
/// which was right. Schema 12 counts the rows, and so it clears the hint of a chat that has
/// unread history rows (the fix round; the review found it showing twice what the phone had).
#[test]
fn review_schema_12_does_not_double_what_an_earlier_delivery_counted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([5; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v11 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..11]);
    kourier_db::migrate(&mut conn, &v11).expect("schema 11");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind, unread_hint, unread)
           VALUES (1, 1, 'd:aci:00000000-0000-4000-8000-0000000a11ce', 'Direct', 3, 3);
         INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body, read_state, history, silent)
           VALUES
           (1, 1, 0, 'aci:00000000-0000-4000-8000-0000000a11ce', '100', 'Remote', 102400, 100,
            100, 'Text', '{}', 'Read', 1, 1),
           (1, 1, 0, 'aci:00000000-0000-4000-8000-0000000a11ce', '200', 'Remote', 204800, 200,
            200, 'Text', '{}', 'Unread', 1, 1),
           (1, 1, 0, 'aci:00000000-0000-4000-8000-0000000a11ce', '300', 'Remote', 307200, 300,
            300, 'Text', '{}', 'Unread', 1, 1),
           (1, 1, 0, 'aci:00000000-0000-4000-8000-0000000a11ce', '400', 'Remote', 409600, 400,
            400, 'Text', '{}', 'Unread', 1, 1);",
    )
    .expect("what an S2 delivery left");
    let unread = |conn: &kourier_db::rusqlite::Connection| -> i64 {
        conn.query_row("SELECT unread FROM conversations WHERE id = 1", [], |r| {
            r.get(0)
        })
        .expect("unread")
    };
    assert_eq!(unread(&conn), 3, "what the user saw under schema 11");
    // Only the step to 12 (later schemas add steps of their own).
    let v12 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..12]);
    let out = kourier_db::migrate(&mut conn, &v12).expect("migrate");
    assert_eq!((out.from, out.to), (11, 12));
    assert_eq!(unread(&conn), 3, "and sees after the step");
}

/// A measurement, not a test: a chat imported with 20,000 unread messages is read here.
/// `cargo test -p kourier-core --test history_rules_review review_measure -- --ignored
/// --nocapture`.
#[test]
#[ignore = "review: a measurement (one read of a chat with 20,000 unread imported rows)"]
fn review_measure_reading_a_chat_with_many_unread_imported_rows() {
    let (_dir, mut h) = harness();
    let a = d_alice();
    const N: i64 = 20_000;
    for chunk in 0..(N / 500) {
        let messages = (0..500)
            .map(|i| incoming(&a, &alice(), 1_000 + chunk * 500 + i, false))
            .collect();
        h.apply(vec![batch(&a, messages)]).expect("history");
    }
    assert_eq!(counts(&h, &a).0, u32::try_from(N).expect("small"));
    let started = std::time::Instant::now();
    let commands = h
        .mark_read(conv(&h, &a))
        .expect("mark read")
        .expect("unread");
    let took = started.elapsed();
    let listed = commands
        .iter()
        .map(|c| match c {
            DurableCommand::SyncReadMarker { read, .. } => read.len(),
            _ => 0,
        })
        .sum::<usize>();
    let stored: i64 = h
        .connection()
        .query_row(
            "SELECT sum(length(command)) FROM pending_commands",
            [],
            |r| r.get(0),
        )
        .expect("size");
    println!(
        "review: mark_read of {N} unread imported rows took {took:?}; one SyncReadMarker lists \
         {listed} messages; its stored command is {stored} bytes"
    );
    // One phone read marker per message, as Signal sends them, on the same chat.
    let (_dir2, mut h2) = harness();
    for chunk in 0..(N / 500) {
        let messages = (0..500)
            .map(|i| incoming(&a, &alice(), 1_000 + chunk * 500 + i, false))
            .collect();
        h2.apply(vec![batch(&a, messages)]).expect("history");
    }
    let started = std::time::Instant::now();
    for i in 0..200 {
        h2.apply(vec![BackendEvent::ReadMarker {
            conv: Some(a.clone()),
            up_to: target(Some(&a), Some(&alice()), &(1_000 + N - 200 + i).to_string()),
        }])
        .expect("marker");
    }
    println!(
        "review: 200 read markers of the phone on a chat of {N} rows took {:?}",
        started.elapsed()
    );
    // The same with an index of ALL unread rows (schema 12's asks for `deleted IS NULL`,
    // which the marker's update does not say): the planner takes it by itself.
    let (_dir3, mut h3) = harness();
    h3.connection()
        .execute_batch(
            "CREATE INDEX review_unread ON messages(conversation_id) WHERE read_state = 'Unread'",
        )
        .expect("index");
    for chunk in 0..(N / 500) {
        let messages = (0..500)
            .map(|i| incoming(&a, &alice(), 1_000 + chunk * 500 + i, false))
            .collect();
        h3.apply(vec![batch(&a, messages)]).expect("history");
    }
    let started = std::time::Instant::now();
    for i in 0..200 {
        h3.apply(vec![BackendEvent::ReadMarker {
            conv: Some(a.clone()),
            up_to: target(Some(&a), Some(&alice()), &(1_000 + N - 200 + i).to_string()),
        }])
        .expect("marker");
    }
    println!(
        "review: the same 200 markers with an index of all unread rows took {:?}",
        started.elapsed()
    );
    assert_eq!(counts(&h3, &a), (0, 0));
}

/// A measurement, not a test: the plan of the statement that reads rows by a `sent_at`
/// watermark (`ingest::READ_UP_TO`: `modifier.rs` `read_marker`, `merge.rs`), with the index
/// of schema 12 (since the fix round: `read_state = 'Unread'` alone), and with the index the
/// review found (the first form of schema 12 asked for `deleted IS NULL` too, which this
/// statement cannot say). It is held by `ingest::tests::the_unread_counts_use_the_index_of_unread_rows`;
/// `cargo test -p kourier-core --test history_rules_review review_measure_plan -- --ignored
/// --nocapture` prints it.
#[test]
#[ignore = "review: a measurement (the plan of the read marker's update)"]
fn review_measure_plan_of_the_read_markers_update() {
    let (_dir, h) = harness();
    let sql = "UPDATE messages SET read_state = 'Read'
                 WHERE conversation_id = 1 AND read_state = 'Unread' AND sent_at <= 5
                 RETURNING id";
    let plan = |h: &IngestHarness, sql: &str| -> Vec<String> {
        h.connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("prepare")
            .query_map([], |r| r.get::<_, String>(3))
            .expect("plan")
            .collect::<kourier_db::rusqlite::Result<_>>()
            .expect("rows")
    };
    println!("review: schema 12: {:?}", plan(&h, sql));
    h.connection()
        .execute_batch(
            "CREATE INDEX review_unread ON messages(conversation_id) WHERE read_state = 'Unread'",
        )
        .expect("index");
    println!(
        "review: with an index of all unread rows: {:?}",
        plan(&h, sql)
    );
    let pinned = sql.replace(
        "UPDATE messages",
        "UPDATE messages INDEXED BY review_unread",
    );
    println!("review: pinned: {:?}", plan(&h, &pinned));
}

/// Rule 2 through a merge of two chats of one person and a clear of the chat: the unread
/// imported rows of both sides count in the chat that is left, a read of it lists all of
/// them under the person's id of today and sends receipts for the live ones only, and a
/// cleared chat counts nothing.
#[test]
fn review_a_merge_and_a_clear_keep_the_count_true_to_the_rows() {
    let (_dir, mut h) = harness();
    let old = uid("aci:00000000-0000-4000-8000-00000000a01d");
    let (a, a_old) = (d_alice(), ConvKey::Direct(old.clone()));
    h.apply(vec![
        read_receipts_on(),
        batch(
            &a_old,
            vec![
                incoming(&a_old, &old, 100, false),
                incoming(&a_old, &old, 200, false),
            ],
        ),
        batch(&a, vec![incoming(&a, &alice(), 300, false)]),
        BackendEvent::Message(incoming(&a, &alice(), 400, false)),
    ])
    .expect("two chats of one person");
    assert_eq!((counts(&h, &a_old), counts(&h, &a)), ((2, 0), (2, 0)));
    h.apply(vec![BackendEvent::IdentityMerged {
        from: old,
        into: alice(),
    }])
    .expect("merge");
    assert_eq!(counts(&h, &a), (4, 0));
    let chat = conv(&h, &a);
    let commands = h.mark_read(chat).expect("mark read").expect("unread");
    let (up, synced, receipts) = told(&commands);
    assert_eq!(up.as_deref(), Some("400"));
    assert_eq!(synced, ["100", "200", "300", "400"]);
    assert_eq!(receipts, ["400"], "only the message received here");
    assert_eq!(counts(&h, &a), (0, 0));

    h.apply(vec![batch(
        &a,
        vec![
            incoming(&a, &alice(), 500, false),
            incoming(&a, &alice(), 600, false),
        ],
    )])
    .expect("more of the import");
    assert_eq!(counts(&h, &a), (2, 0));
    h.clear_conversation(chat, false).expect("clear");
    assert_eq!(counts(&h, &a), (0, 0));
}

/// Rule 3 in a chat that is blocked, and in a message request that was not accepted: our
/// own devices are told about the imported rows all the same, and nobody gets a receipt.
#[test]
fn review_a_blocked_or_unaccepted_imported_chat_tells_our_devices_only() {
    for (blocked, pending) in [(true, false), (false, true)] {
        let (_dir, mut h) = harness();
        let a = d_alice();
        let mut chat = ConversationPatch::new(a.clone());
        chat.blocked = Some(blocked);
        chat.request_pending = Some(pending);
        h.apply(vec![
            read_receipts_on(),
            BackendEvent::ConversationUpsert(chat),
            batch(&a, vec![incoming(&a, &alice(), 100, false)]),
            BackendEvent::Message(incoming(&a, &alice(), 200, false)),
        ])
        .expect("history and a live message");
        let commands = h
            .mark_read(conv(&h, &a))
            .expect("mark read")
            .expect("unread");
        let (up, synced, receipts) = told(&commands);
        assert_eq!(up.as_deref(), Some("200"));
        assert_eq!(synced, ["100", "200"]);
        assert!(receipts.is_empty(), "{receipts:?}");
    }
}
