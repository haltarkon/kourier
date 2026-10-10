//! Call lines (`kourier_model::call`, `ingest::call`, `calls`): every signal of the design's
//! table on the real ingest, signals out of order, twice and after a delete, the deadline
//! (while running, when a line arrives late, across a restart and across a suspend), unread
//! counts and the notification decision through ringing -> missed -> answered, reading and
//! deleting a line without anything being sent, and one run through the runtime on the fake
//! network.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kourier_backend_api::{BackendEvent, Clock, DurableCommand};
use kourier_core::facade::{
    ConversationListQuery, MessageRow, OriginKind, TimelineAnchor, TimelineQuery, UiCommand,
    UiEvent,
};
use kourier_core::testing::{DumpMode, IngestHarness};
use kourier_fake::calls::{Call, RING_MS, cast, ring_end};
use kourier_fake::scripts::{T0, remote, signal_features};
use kourier_fake::{Flavor, Script, Step};
use kourier_model::{
    AccountSettingsPatch, CallDirection, CallLine, CallMedia, CallState, ConvKey, ConversationId,
    ConversationPatch, MessageId, MsgIdScope, NetworkKind, Patch, ReadState, TargetAuthor,
};
use support::send::core_db;
use support::{Env, WAIT, add_linked, eventually};

fn ts(n: i64) -> i64 {
    T0 + n * 1000
}

/// A Signal-like account (account-wide ids) with read receipts on and the clock at `T0`.
fn harness() -> (tempfile::TempDir, IngestHarness) {
    let c = cast().expect("cast");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[c.me],
    )
    .expect("harness");
    h.set_now(T0);
    let mut chat = ConversationPatch::new(ConvKey::Direct(c.alice));
    chat.features = Some(signal_features(false));
    h.apply(vec![
        BackendEvent::AccountSettings(AccountSettingsPatch {
            read_receipts: Some(true),
            ..Default::default()
        }),
        BackendEvent::ConversationUpsert(chat),
    ])
    .expect("setup");
    (dir, h)
}

fn conv(h: &IngestHarness, key: &ConvKey) -> ConversationId {
    let id: i64 = h
        .connection()
        .query_row(
            "SELECT id FROM conversations WHERE conv_key = ?1",
            [key.encode()],
            |r| r.get(0),
        )
        .expect("conversation");
    ConversationId(id)
}

fn rows(h: &IngestHarness, key: &ConvKey) -> Vec<MessageRow> {
    h.timeline(&TimelineQuery {
        conv: conv(h, key),
        anchor: TimelineAnchor::Newest,
        limit: 100,
    })
    .expect("timeline")
    .rows
}

/// The one call line of the chat `key`, as the timeline shows it.
fn line(h: &IngestHarness, key: &ConvKey) -> (CallLine, MessageRow) {
    let lines: Vec<(CallLine, MessageRow)> = rows(h, key)
        .into_iter()
        .filter_map(|r| r.body.call_line().cloned().map(|l| (l, r)))
        .collect();
    assert_eq!(lines.len(), 1, "one line per call");
    lines.into_iter().next().expect("line")
}

fn unread(h: &IngestHarness, key: &ConvKey) -> u32 {
    h.conversation(conv(h, key))
        .expect("query")
        .expect("conversation")
        .row
        .unread
}

fn stored_deadline(h: &IngestHarness, id: MessageId) -> Option<i64> {
    h.connection()
        .query_row(
            "SELECT call_deadline FROM messages WHERE id = ?1",
            [id.0],
            |r| r.get(0),
        )
        .expect("row")
}

/// What one row of the design's table ends in.
struct Expect {
    direction: CallDirection,
    state: CallState,
    /// Unread, and announced like a message.
    counts: bool,
}

/// Every row of the design's table: the signals of one call, in the order the wire gives
/// them, end in the line the table names; a line is unread and announced exactly when it is
/// a missed incoming call or a group call somebody started; a ringing line is neither.
#[test]
fn every_signal_of_the_table_makes_or_changes_one_line() {
    let c = cast().expect("cast");
    let peer = ConvKey::Direct(c.alice.clone());
    let direct = Call::direct(&c.alice, 1, CallMedia::Video);
    let group = Call::group(&c.group, 1, &c.alice);
    let ringing = CallState::Ringing {
        deadline: ring_end(ts(1)),
    };
    let expect = |direction, state, counts| Expect {
        direction,
        state,
        counts,
    };
    use CallDirection::{Incoming, Outgoing};
    let table: Vec<(&str, &Call, Vec<BackendEvent>, Expect)> = vec![
        (
            "an Offer rings",
            &direct,
            vec![direct.ring(ts(1))],
            expect(Incoming, ringing, false),
        ),
        (
            "the caller gave up",
            &direct,
            vec![direct.ring(ts(1)), direct.caller_gave_up(ts(9))],
            expect(Incoming, CallState::Missed, true),
        ),
        (
            "answered on the phone",
            &direct,
            vec![direct.ring(ts(1)), direct.answered_elsewhere(ts(9))],
            expect(Incoming, CallState::Answered, false),
        ),
        (
            "declined on the phone",
            &direct,
            vec![direct.ring(ts(1)), direct.declined(ts(9))],
            expect(Incoming, CallState::Declined, false),
        ),
        (
            "a call from the phone, answered",
            &direct,
            vec![direct.outgoing_answered(ts(9))],
            expect(Outgoing, CallState::Answered, false),
        ),
        (
            "a call from the phone, unanswered",
            &direct,
            vec![direct.outgoing_unanswered(ts(9))],
            expect(Outgoing, CallState::Unanswered, false),
        ),
        (
            "a group ring",
            &group,
            vec![group.ring(ts(1))],
            expect(Incoming, ringing, false),
        ),
        (
            "a group ring taken back",
            &group,
            vec![group.ring(ts(1)), group.ring_cancelled(ts(9))],
            expect(Incoming, CallState::Missed, true),
        ),
        (
            "a group call started without a ring",
            &group,
            vec![group.started(&c.bob, ts(1))],
            expect(Incoming, CallState::Started, true),
        ),
        (
            "a group call joined on the phone",
            &group,
            vec![group.ring(ts(1)), group.joined_elsewhere(ts(9))],
            expect(Incoming, CallState::Answered, false),
        ),
        (
            "a group ring declined on the phone",
            &group,
            vec![group.ring(ts(1)), group.declined(ts(9))],
            expect(Incoming, CallState::Declined, false),
        ),
    ];
    for (name, call, signals, want) in table {
        let (_dir, mut h) = harness();
        let mut announced = Vec::new();
        for s in signals {
            announced.extend(h.apply_announcing(vec![s]).expect("apply").notify);
        }
        let (got, row) = line(&h, &call.conv);
        assert_eq!(
            (got.direction, got.state),
            (want.direction, want.state),
            "{name}"
        );
        assert_eq!(got.group, call.conv != peer, "{name}");
        assert_eq!(got.counts_unread(), want.counts, "{name}");
        assert_eq!(
            row.read_state,
            want.counts.then_some(ReadState::Unread),
            "{name}: a line has a read state exactly while it counts"
        );
        assert_eq!(unread(&h, &call.conv), u32::from(want.counts), "{name}");
        let announced_line: Vec<MessageId> = if want.counts { vec![row.id] } else { vec![] };
        assert_eq!(
            announced, announced_line,
            "{name}: announced once, or never"
        );
        // The row is nobody's message: no author, no send state, no timer; the way the
        // call went is its origin.
        assert_eq!(row.author, None, "{name}");
        assert_eq!(row.out_state, None, "{name}");
        assert_eq!(row.expiry, None, "{name}");
        let origin = match want.direction {
            Incoming => OriginKind::Remote,
            Outgoing => OriginKind::OwnOtherDevice,
        };
        assert_eq!(row.origin, origin, "{name}");
        assert_eq!(stored_deadline(&h, row.id), got.deadline(), "{name}");
        // The line shows when the call began.
        assert!(row.sent_at <= ts(9), "{name}");
        if got.state != CallState::Started && got.direction == Incoming {
            let by = (!got.group || got.state != CallState::Started).then(|| c.alice.clone());
            assert_eq!(got.by, by, "{name}: who rang");
        }
    }
}

/// The kind of a line follows the latest signal that names one, a hang-up names none, and
/// who started a group call is whom the earliest signal named.
#[test]
fn kind_and_caller_of_a_line() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let voice = Call::direct(&c.alice, 5, CallMedia::Voice);
    let video = Call {
        media: Some(CallMedia::Video),
        ..voice.clone()
    };
    h.apply(vec![voice.ring(ts(1))]).expect("ring");
    h.apply(vec![voice.caller_gave_up(ts(5))]).expect("hang-up");
    assert_eq!(line(&h, &voice.conv).0.media, Some(CallMedia::Voice));
    // The phone says it was answered with the camera on.
    h.apply(vec![video.answered_elsewhere(ts(8))])
        .expect("answered");
    let (got, row) = line(&h, &voice.conv);
    assert_eq!(got.media, Some(CallMedia::Video));
    assert_eq!(row.sent_at, ts(1), "the first signal's time");

    let g = Call::group(&c.group, 6, &c.alice);
    h.apply(vec![g.started(&c.alice, ts(30))]).expect("second");
    h.apply(vec![g.started(&c.bob, ts(20))]).expect("first");
    let (got, row) = line(&h, &g.conv);
    assert_eq!(got.by, Some(c.bob.clone()), "the earliest sender");
    assert_eq!(row.sent_at, ts(20));
    assert_eq!(unread(&h, &g.conv), 1, "one line, announced once");
    // The row has no author; the name of who started the call comes with it, for the
    // notification ("Bob started a group call").
    assert_eq!((row.author.clone(), row.author_name.clone()), (None, None));
    let mut bob = kourier_model::ContactPatch::new(c.bob.clone());
    bob.profile_name = Patch::Set("Bob Example".into());
    h.apply(vec![BackendEvent::ContactUpsert(bob)])
        .expect("contact");
    let (_, row) = line(&h, &g.conv);
    assert_eq!(row.author, None);
    assert_eq!(
        row.author_name.as_ref().map(|n| n.expose().as_str()),
        Some("Bob Example")
    );
}

/// Schema 10 stored a call line under the account-wide key of a Signal-like network. The
/// migration to schema 11 moves such rows, and the memory of removed ones, to the key of
/// their conversation; the signals of the call find the line again afterwards.
#[test]
fn lines_of_schema_10_move_to_the_key_of_their_conversation() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let kept = Call::direct(&c.alice, 1, CallMedia::Voice);
    let deleted = Call::direct(&c.alice, 2, CallMedia::Voice);
    let cleared = Call::direct(&c.bob, 3, CallMedia::Voice);
    let text = remote(
        &kept.conv,
        &c.alice,
        "call:9",
        ts(1),
        "a text with an odd id",
    );
    h.apply(vec![
        kept.ring(ts(1)),
        deleted.caller_gave_up(ts(2)),
        deleted.deleted(ts(3)),
        cleared.caller_gave_up(ts(4)),
        BackendEvent::Message(text),
    ])
    .expect("apply");
    h.clear_conversation(conv(&h, &cleared.conv), false)
        .expect("clear");
    let scopes = |h: &IngestHarness| -> Vec<(String, i64)> {
        let mut rows: Vec<(String, i64)> = h
            .connection()
            .prepare(
                "SELECT net_local, net_scope FROM messages
                 UNION ALL SELECT net_local || ' (gone)', net_scope FROM expired_messages",
            )
            .expect("prepare")
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("rows");
        rows.sort();
        rows
    };
    let now = vec![
        ("call:1".to_owned(), 1),
        ("call:2".to_owned(), 1),
        ("call:3 (gone)".to_owned(), 1),
        ("call:9".to_owned(), 0),
    ];
    assert_eq!(
        scopes(&h),
        now,
        "lines by conversation, messages by account"
    );
    // As schema 10 stored them.
    h.connection()
        .execute_batch(
            "UPDATE messages SET net_scope = 0; UPDATE expired_messages SET net_scope = 0;",
        )
        .expect("back to schema 10");
    h.connection()
        .execute_batch(include_str!("../migrations/0011_call_line_keys.sql"))
        .expect("the migration");
    assert_eq!(scopes(&h), now, "a message that only looks like one stays");
    h.apply(vec![
        kept.caller_gave_up(ts(9)),
        deleted.ring(ts(1)),
        cleared.ring(ts(1)),
    ])
    .expect("the calls go on");
    assert_eq!(line(&h, &kept.conv).0.state, CallState::Missed);
    assert!(
        rows(&h, &cleared.conv).is_empty(),
        "the cleared line stays gone"
    );
    assert_eq!(scopes(&h), now);
}

/// Two direct chats of one person turn out to be one (an identity merge) and each holds a
/// line under the same call id: the merge goes through, one line is left, and the chat
/// works on. (How the two lines should be joined is not designed: the older row stays.)
#[test]
fn a_merge_of_two_chats_with_the_same_call_id_does_not_fail() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let pni = kourier_model::UserId::parse("pni:00000000-0000-4000-8000-0000000a11ce").expect("id");
    let by_aci = Call::direct(&c.alice, 7, CallMedia::Voice);
    let by_pni = Call::direct(&pni, 7, CallMedia::Voice);
    h.apply(vec![
        by_pni.caller_gave_up(ts(1)),
        by_aci.caller_gave_up(ts(2)),
    ])
    .expect("two lines");
    h.apply(vec![BackendEvent::IdentityMerged {
        from: pni.clone(),
        into: c.alice.clone(),
    }])
    .expect("the merge");
    let (got, _) = line(&h, &by_aci.conv);
    assert_eq!(got.state, CallState::Missed);
    let count: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE net_local = 'call:7'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(count, 1);
    h.apply(vec![by_aci.answered_elsewhere(ts(9))])
        .expect("the call goes on");
    assert_eq!(line(&h, &by_aci.conv).0.state, CallState::Answered);
    assert_eq!(unread(&h, &by_aci.conv), 0);
}

/// The same signals in every order, each also twice, end in the same database; a signal
/// after the delete of the call entry changes nothing, and a delete that arrives before
/// the line waits for it.
#[test]
fn signals_out_of_order_twice_and_after_a_delete() {
    let c = cast().expect("cast");
    let call = Call::direct(&c.alice, 9, CallMedia::Voice);
    let text = BackendEvent::Message(remote(
        &call.conv,
        &c.alice,
        &ts(40).to_string(),
        ts(40),
        "call me back",
    ));
    let signals = [
        call.ring(ts(1)),
        call.caller_gave_up(ts(20)),
        call.answered_elsewhere(ts(30)),
        text,
    ];
    let run = |order: &[usize], twice: bool, delete_at: Option<usize>| -> String {
        let (_dir, mut h) = harness();
        for (i, n) in order.iter().enumerate() {
            if delete_at == Some(i) {
                h.apply(vec![call.deleted(ts(50))]).expect("delete");
            }
            h.apply(vec![signals[*n].clone()]).expect("apply");
            if twice {
                h.apply(vec![signals[*n].clone()]).expect("apply again");
            }
        }
        if delete_at == Some(order.len()) {
            h.apply(vec![call.deleted(ts(50))]).expect("delete");
        }
        if delete_at.is_some() {
            assert!(
                rows(&h, &call.conv)
                    .iter()
                    .all(|r| r.body.call_line().is_none()),
                "the line is gone: order {order:?}, delete at {delete_at:?}"
            );
            let pending: i64 = h
                .connection()
                .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
                .expect("count");
            assert_eq!(pending, 0, "the delete found its line");
        }
        h.dump(DumpMode::OrderInsensitive).expect("dump")
    };
    let mut orders = Vec::new();
    for a in 0..4 {
        for b in (0..4).filter(|b| *b != a) {
            for c in (0..4).filter(|c| *c != a && *c != b) {
                let d = (0..4).find(|d| ![a, b, c].contains(d)).expect("the fourth");
                orders.push([a, b, c, d]);
            }
        }
    }
    assert_eq!(orders.len(), 24);
    let kept = run(&orders[0], false, None);
    assert!(kept.contains("\"Answered\""), "{kept}");
    let deleted = run(&orders[0], false, Some(4));
    assert_ne!(kept, deleted);
    for order in &orders {
        assert_eq!(run(order, false, None), kept, "order {order:?}");
        assert_eq!(run(order, true, None), kept, "order {order:?}, each twice");
        for at in 0..=4 {
            assert_eq!(
                run(order, at % 2 == 0, Some(at)),
                deleted,
                "order {order:?}, deleted before signal {at}"
            );
        }
    }
}

/// The deadline: a line rings until its deadline and no longer. Past it the sweep makes it
/// a missed call that is unread and announced; a later "answered" takes both back.
#[test]
fn a_ring_past_its_deadline_is_missed_until_it_is_answered() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let call = Call::direct(&c.alice, 11, CallMedia::Voice);
    let announced = h.apply_announcing(vec![call.ring(ts(1))]).expect("ring");
    assert!(
        announced.notify.is_empty(),
        "the phone rings, Kourier does not"
    );
    let deadline = ring_end(ts(1));
    assert_eq!(h.next_call_deadline().expect("next"), Some(deadline));
    assert_eq!(unread(&h, &call.conv), 0);

    // A moment before: nothing.
    h.set_now(deadline - 1);
    assert_eq!(h.settle_calls().expect("sweep"), Default::default());
    assert!(matches!(
        line(&h, &call.conv).0.state,
        CallState::Ringing { .. }
    ));

    h.set_now(deadline);
    let swept = h.settle_calls().expect("sweep");
    let (missed, row) = line(&h, &call.conv);
    assert_eq!(missed.state, CallState::Missed);
    assert_eq!(swept.notify, vec![row.id], "announced like a message");
    assert_eq!(swept.changed, vec![row.id]);
    assert_eq!(row.read_state, Some(ReadState::Unread));
    assert_eq!(unread(&h, &call.conv), 1);
    assert_eq!(h.next_call_deadline().expect("next"), None);
    assert_eq!(row.sent_at, ts(1), "it keeps its time and place");
    // The sweep is done: another look changes nothing, and neither does the ring again.
    assert_eq!(h.settle_calls().expect("sweep"), Default::default());
    let again = h.apply_announcing(vec![call.ring(ts(1))]).expect("ring");
    assert!(again.notify.is_empty() && again.changed.is_empty());
    assert_eq!(h.next_call_deadline().expect("next"), None);

    // The phone's sync was late: the call was answered there.
    let answered = h
        .apply_announcing(vec![call.answered_elsewhere(ts(30))])
        .expect("answered");
    assert!(answered.notify.is_empty());
    assert_eq!(answered.changed, vec![row.id], "the desktop is told");
    let (line_now, row) = line(&h, &call.conv);
    assert_eq!(line_now.state, CallState::Answered);
    assert_eq!(row.read_state, None, "its unread mark is gone");
    assert_eq!(unread(&h, &call.conv), 0);
}

/// A ring that has no time left when it arrives never shows as ringing: after a catch-up it
/// counts towards the summary, live it is announced alone; and never when the signal is
/// silent. The time left is the signal's own (its deadline minus its own time) and is counted
/// from its receipt on this computer's clock: how the two clocks stand to each other does
/// not matter.
#[test]
fn a_ring_that_arrives_late_is_missed_at_once() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    // The backend says the ring has nothing left (an Offer older than the ring time).
    let live = Call::direct(&c.alice, 21, CallMedia::Video);
    let announced = h
        .apply_announcing(vec![live.ring_until(ts(1), ts(1))])
        .expect("ring");
    let (got, row) = line(&h, &live.conv);
    assert_eq!(got.state, CallState::Missed);
    assert_eq!(announced.notify, vec![row.id]);
    assert_eq!(stored_deadline(&h, row.id), None);
    assert_eq!(h.next_call_deadline().expect("next"), None);

    // The ring had time left at its receipt, and the core gets it a moment after that ran
    // out: missed at once, announced like a message.
    h.set_now(ring_end(ts(1)) + 5);
    let just = Call::direct(&c.alice, 24, CallMedia::Voice);
    let announced = h.apply_announcing(vec![just.ring(ts(1))]).expect("ring");
    assert_eq!(announced.notify.len(), 1);
    assert_eq!(h.next_call_deadline().expect("next"), None);

    let old = Call::direct(&c.bob, 22, CallMedia::Voice);
    let mut m = old.ring_message(ts(1));
    m.catch_up = true;
    let announced = h
        .apply_announcing(vec![BackendEvent::Message(m)])
        .expect("ring");
    assert_eq!(line(&h, &old.conv).0.state, CallState::Missed);
    assert_eq!((announced.notify.len(), announced.catch_up), (0, 1));
    assert_eq!(unread(&h, &old.conv), 1);

    let quiet = Call::group(&c.group, 23, &c.alice);
    let mut m = quiet.ring_message(ts(1));
    m.silent = true;
    let announced = h
        .apply_announcing(vec![BackendEvent::Message(m)])
        .expect("ring");
    assert_eq!((announced.notify.len(), announced.catch_up), (0, 0));
    assert_eq!(unread(&h, &quiet.conv), 1, "unread all the same");

    // A sender whose clock is an hour ahead of ours: the ring still rings for the time it
    // has left, from its receipt here.
    let ahead = Call::direct(&c.bob, 25, CallMedia::Voice);
    let BackendEvent::Message(mut m) = ahead.ring(h.now() + 3_600_000) else {
        unreachable!()
    };
    m.received_at = h.now();
    h.apply(vec![BackendEvent::Message(m)]).expect("ring");
    assert_eq!(
        h.next_call_deadline().expect("next"),
        Some(h.now() + RING_MS)
    );
}

/// Unread through ringing -> missed -> read here -> the same signals again -> answered:
/// reading a line queues nothing for it (no receipt, no read sync), a repeated signal does
/// not undo the read, and a text read with it is the only thing anybody is told about.
#[test]
fn reading_a_line_tells_nobody() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let call = Call::direct(&c.alice, 31, CallMedia::Voice);
    // An earlier text of the caller, read long ago (the read sync would name it as the
    // newest message of the chat, if one were sent).
    let earlier = (ts(0) - 5).to_string();
    h.apply(vec![BackendEvent::Message(remote(
        &call.conv,
        &c.alice,
        &earlier,
        ts(0) - 5,
        "earlier",
    ))])
    .expect("text");
    let id = conv(&h, &call.conv);
    assert_eq!(
        h.mark_read(id).expect("mark read").map(|c| c.len()),
        Some(2),
        "a text is synced and receipted"
    );
    h.apply(vec![call.ring(ts(1))]).expect("ring");
    assert_eq!(unread(&h, &call.conv), 0);
    assert_eq!(h.mark_read(id).expect("mark read"), None, "nothing to read");
    h.apply(vec![call.caller_gave_up(ts(9))]).expect("hang-up");
    assert_eq!(unread(&h, &call.conv), 1);

    // Only the line was unread: it is read, and nothing is queued at all.
    let commands = h.mark_read(id).expect("mark read").expect("was unread");
    assert_eq!(commands, Vec::<DurableCommand>::new());
    let (_, row) = line(&h, &call.conv);
    assert_eq!(row.read_state, Some(ReadState::Read));
    assert_eq!(unread(&h, &call.conv), 0);
    assert_eq!(h.mark_read(id).expect("mark read"), None);

    // The same signals again do not make it unread again.
    let again = h
        .apply_announcing(vec![call.ring(ts(1)), call.caller_gave_up(ts(9))])
        .expect("again");
    assert!(again.notify.is_empty());
    assert_eq!(unread(&h, &call.conv), 0);

    // A second missed call and a text: the read sync and the receipt name the text only.
    let second = Call::direct(&c.alice, 32, CallMedia::Voice);
    let local = ts(50).to_string();
    h.apply(vec![
        second.caller_gave_up(ts(40)),
        BackendEvent::Message(remote(&call.conv, &c.alice, &local, ts(50), "hello?")),
    ])
    .expect("apply");
    assert_eq!(unread(&h, &call.conv), 2);
    let commands = h.mark_read(id).expect("mark read").expect("was unread");
    assert_eq!(commands.len(), 2, "{commands:?}");
    for cmd in commands {
        let targets = match cmd {
            DurableCommand::SyncReadMarker { up_to, read, .. } => {
                assert_eq!(&*up_to.local, local.as_str());
                read
            }
            DurableCommand::SendReceipts { targets, .. } => targets,
            other => panic!("reading queues no {other:?}"),
        };
        assert_eq!(targets.len(), 1);
        assert_eq!(&*targets[0].local, local.as_str());
        assert_eq!(targets[0].author, TargetAuthor::User(c.alice.clone()));
    }
    assert_eq!(unread(&h, &call.conv), 0);
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM outbox", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0);
}

/// A read marker of our other devices covers a line like any row: the phone read the chat
/// past the call, in whatever order the two arrive.
#[test]
fn a_read_marker_of_the_phone_reads_a_missed_line() {
    let c = cast().expect("cast");
    let call = Call::direct(&c.alice, 41, CallMedia::Voice);
    let local = ts(60).to_string();
    let text = BackendEvent::Message(remote(&call.conv, &c.alice, &local, ts(60), "?"));
    let marker = BackendEvent::ReadMarker {
        conv: None,
        up_to: kourier_fake::scripts::target(None, Some(&c.alice), &local),
    };
    let events = [call.ring(ts(1)), call.caller_gave_up(ts(9)), text, marker];
    let mut dumps = Vec::new();
    for order in [[0, 1, 2, 3], [2, 3, 0, 1], [0, 2, 3, 1], [2, 0, 3, 1]] {
        let (_dir, mut h) = harness();
        for i in order {
            h.apply(vec![events[i].clone()]).expect("apply");
        }
        assert_eq!(unread(&h, &call.conv), 0, "order {order:?}");
        assert_eq!(
            line(&h, &call.conv).1.read_state,
            Some(ReadState::Read),
            "order {order:?}"
        );
        dumps.push(h.dump(DumpMode::OrderInsensitive).expect("dump"));
    }
    assert!(dumps.windows(2).all(|w| w[0] == w[1]));
}

/// Nothing but its own signals, the phone's delete and a read marker touches a line: not a
/// receipt, a send state, a reaction, an edit or a delete for everyone that names its id,
/// and not a message that happens to carry it.
#[test]
fn a_line_is_no_message_we_sent() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let call = Call::direct(&c.alice, 51, CallMedia::Voice);
    h.apply(vec![call.caller_gave_up(ts(1))]).expect("missed");
    let before = h.dump(DumpMode::Exact).expect("dump");
    let target =
        || kourier_backend_api::IncomingMessage::call_target(call.conv.clone(), false, &call.id);
    h.apply(vec![
        BackendEvent::Receipts {
            targets: vec![target()],
            by: c.alice.clone(),
            kind: kourier_model::ReceiptKind::Read,
            at: ts(5),
        },
        BackendEvent::OwnSendState {
            local: None,
            target: target(),
            per_recipient: vec![(
                c.alice.clone(),
                kourier_model::RecipientState::Sent { sealed: true },
            )],
        },
        BackendEvent::Reaction {
            target: target(),
            by: Some(c.alice.clone()),
            emoji: "👍".into(),
            remove: false,
            at: ts(6),
        },
        BackendEvent::Edited {
            target: target(),
            by: None,
            new_local: Some(ts(7).to_string().into()),
            body: kourier_model::Body::text("not a call"),
            edited_at: ts(7),
            keeps_quote: None,
        },
        BackendEvent::Deleted {
            target: target(),
            by: None,
            for_everyone: true,
            at: ts(8),
        },
        BackendEvent::Message(kourier_fake::scripts::own(
            &call.conv,
            &call.local(),
            ts(9),
            "an echo with the line's id",
        )),
    ])
    .expect("apply");
    assert_eq!(h.dump(DumpMode::Exact).expect("dump"), before);
    assert_eq!(line(&h, &call.conv).0.state, CallState::Missed);
    // The same call id in another chat is another call: it does not reach this line.
    let other = Call {
        conv: ConvKey::Direct(c.bob.clone()),
        ..call.clone()
    };
    h.apply(vec![other.answered_elsewhere(ts(10))])
        .expect("apply");
    assert_eq!(line(&h, &call.conv).0.state, CallState::Missed);
    assert_eq!(
        line(&h, &other.conv).0.state,
        CallState::Answered,
        "it has its own line there"
    );
}

/// Clearing the chat removes a line like any row and names no line to the other devices; a
/// line of imported history is read, silent and sorted by its time, and becomes a live line
/// (unread, announced) when a later live signal makes it a missed call.
#[test]
fn clear_chat_and_history() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let call = Call::direct(&c.alice, 61, CallMedia::Voice);
    h.apply(vec![call.ring(ts(1))]).expect("ring");
    let (_, row) = line(&h, &call.conv);
    assert_eq!(row.expiry, None, "a chat without a timer: no timer");
    h.set_now(ring_end(ts(1)));
    h.settle_calls().expect("sweep");
    assert_eq!(h.sweep().expect("expiry sweep"), 0);
    assert_eq!(line(&h, &call.conv).0.state, CallState::Missed);
    let listed = h
        .conversations(&ConversationListQuery::default())
        .expect("list");
    let preview = listed.rows[0].preview.as_ref().expect("preview");
    assert!(
        preview.system.as_ref().is_some_and(
            |e| matches!(e, kourier_model::SystemEvent::Call(l) if l.state == CallState::Missed)
        ),
        "the list previews the line"
    );

    let id = conv(&h, &call.conv);
    let commands = h.clear_conversation(id, false).expect("clear");
    assert!(
        rows(&h, &call.conv).is_empty(),
        "the line went with the chat"
    );
    assert_eq!(unread(&h, &call.conv), 0);
    for cmd in &commands {
        let text = format!("{cmd:?}");
        assert!(!text.contains("call:"), "a line is never named: {text}");
    }
    // The phone's late sync for the call whose line went with the chat brings nothing back.
    h.apply(vec![call.answered_elsewhere(ts(90))])
        .expect("late signal");
    assert!(rows(&h, &call.conv).is_empty(), "it stays cleared");

    // History: two calls of the past, one of which "rings" in the archive. The phone had
    // seen the older one (`read: true`) and not the other: an imported missed call follows
    // its `read` flag like an imported message. Neither is announced.
    let past = Call::direct(&c.bob, 62, CallMedia::Video);
    let older = Call::direct(&c.bob, 63, CallMedia::Voice);
    let batch = || BackendEvent::HistoryBatch {
        conv: past.conv.clone(),
        messages: vec![past.ring_message(ts(-500)), {
            let BackendEvent::Message(mut m) = older.caller_gave_up(ts(-900)) else {
                unreachable!()
            };
            m.read = true;
            m
        }],
        has_more: false,
        progress: None,
    };
    let announced = h.apply_announcing(vec![batch()]).expect("history");
    assert_eq!((announced.notify.len(), announced.catch_up), (0, 0));
    let history = rows(&h, &past.conv);
    let states: Vec<CallState> = history
        .iter()
        .filter_map(|r| r.body.call_line().map(|l| l.state))
        .collect();
    assert_eq!(states, [CallState::Missed, CallState::Missed]);
    let times: Vec<i64> = history.iter().map(|r| r.sent_at).collect();
    assert_eq!(
        times,
        [ts(-900), ts(-500)],
        "sorted by their time, oldest first"
    );
    assert!(history.iter().all(|r| r.history));
    let read: Vec<Option<ReadState>> = history.iter().map(|r| r.read_state).collect();
    assert_eq!(read, [Some(ReadState::Read), Some(ReadState::Unread)]);
    assert_eq!(
        unread(&h, &past.conv),
        1,
        "the missed call the phone had not seen is unread"
    );
    // Reading the chat here reads it and tells nobody (a line is never named), and the
    // same batch delivered again does not make it unread again.
    let read_here = h
        .mark_read(conv(&h, &past.conv))
        .expect("mark read")
        .expect("the line was unread");
    assert!(read_here.is_empty(), "{read_here:?}");
    assert_eq!(unread(&h, &past.conv), 0);
    let announced = h.apply_announcing(vec![batch()]).expect("history, again");
    assert_eq!((announced.notify.len(), announced.catch_up), (0, 0));
    assert_eq!(unread(&h, &past.conv), 0, "a row delivered twice");

    // A call that was ringing when the history was written, and whose first signal came
    // with the import: the live hang-up that follows makes it a missed call like any other.
    let now = h.now();
    let ringing = Call::group(&c.group, 64, &c.alice);
    h.apply(vec![BackendEvent::HistoryBatch {
        conv: ringing.conv.clone(),
        messages: vec![ringing.ring_message(now)],
        has_more: false,
        progress: None,
    }])
    .expect("history");
    assert!(line(&h, &ringing.conv).0.deadline().is_some());
    assert_eq!(unread(&h, &ringing.conv), 0);
    let announced = h
        .apply_announcing(vec![ringing.ring_cancelled(now + 5_000)])
        .expect("the live signal");
    let (got, row) = line(&h, &ringing.conv);
    assert_eq!(got.state, CallState::Missed);
    assert_eq!(row.read_state, Some(ReadState::Unread));
    assert!(!row.history, "it is a live line now");
    assert_eq!(unread(&h, &ringing.conv), 1);
    assert_eq!(announced.notify, vec![row.id]);
}

/// A call line takes the chat's disappearing timer like a message that is read at once: the
/// timer starts when the line is stored, a later signal does not start it again, the expiry
/// sweeper removes the line like any row, and no later signal brings it back. A timer given
/// with a signal is not read.
#[test]
fn a_line_disappears_with_the_chats_timer() {
    let c = cast().expect("cast");
    let (_dir, mut h) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let mut chat = ConversationPatch::new(peer.clone());
    chat.timer = Patch::Set(Duration::from_secs(300));
    h.apply(vec![BackendEvent::ConversationUpsert(chat)])
        .expect("a timer of five minutes");
    let call = Call::direct(&c.alice, 65, CallMedia::Voice);
    let mut ring = call.ring_message(ts(1));
    // What a backend says here is not read: the chat's timer counts.
    ring.expiry = Some(kourier_model::Expiry {
        kind: kourier_model::DisappearKind::AfterSend,
        timer: Duration::from_secs(5),
        started_at: None,
    });
    h.set_now(ts(2));
    h.apply(vec![BackendEvent::Message(ring)]).expect("ring");
    let stored_at = ts(2);
    let timer = |h: &IngestHarness| {
        let (_, row) = line(h, &peer);
        let e = row.expiry.expect("the chat's timer");
        (e.timer, e.started_at, e.expires_at())
    };
    let want = (
        Duration::from_secs(300),
        Some(stored_at),
        Some(stored_at + 300_000),
    );
    assert_eq!(timer(&h), want, "started when the line was stored");
    // The line changes; its timer runs on from the first signal.
    h.set_now(ring_end(ts(1)));
    h.settle_calls().expect("sweep");
    assert_eq!(line(&h, &peer).0.state, CallState::Missed);
    assert_eq!(timer(&h), want);
    h.set_now(ts(200));
    h.apply(vec![call.answered_elsewhere(ts(190))])
        .expect("answered");
    assert_eq!(line(&h, &peer).0.state, CallState::Answered);
    assert_eq!(timer(&h), want);
    assert_eq!(h.sweep().expect("expiry sweep"), 0, "not yet");

    // Its time is up: the sweeper removes it like any row, and it stays gone.
    h.set_now(stored_at + 300_000);
    assert_eq!(h.sweep().expect("expiry sweep"), 1);
    assert!(rows(&h, &peer).is_empty(), "the line disappeared");
    h.apply(vec![call.ring(ts(1)), call.answered_elsewhere(ts(190))])
        .expect("late copies");
    assert!(rows(&h, &peer).is_empty(), "it does not come back");
    assert_eq!(h.next_call_deadline().expect("next"), None);

    // A missed call in the same chat: unread, and gone with the timer too (who called and
    // when does not stay for ever).
    let second = Call::direct(&c.alice, 66, CallMedia::Voice);
    h.apply(vec![second.caller_gave_up(h.now())])
        .expect("missed");
    assert_eq!(unread(&h, &peer), 1);
    h.set_now(h.now() + 300_000);
    assert_eq!(h.sweep().expect("expiry sweep"), 1);
    assert!(rows(&h, &peer).is_empty());
    assert_eq!(unread(&h, &peer), 0);

    // The timer is switched off: the next line has none.
    let mut chat = ConversationPatch::new(peer.clone());
    chat.timer = Patch::Clear;
    h.apply(vec![BackendEvent::ConversationUpsert(chat)])
        .expect("no timer");
    let third = Call::direct(&c.alice, 67, CallMedia::Voice);
    h.apply(vec![third.caller_gave_up(h.now())])
        .expect("missed");
    assert_eq!(line(&h, &peer).1.expiry, None);
}

/// On a network whose message ids count per conversation (WhatsApp-like, Threema-like) a
/// line is found by its chat and its id: the same call id in two chats is two calls, each
/// with its own line, signals and delete.
#[test]
fn conversation_scoped_ids_keep_one_line_per_chat() {
    let c = cast().expect("cast");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7; 32]),
        NetworkKind::Fake,
        MsgIdScope::Conversation,
        std::slice::from_ref(&c.me),
    )
    .expect("harness");
    h.set_now(T0);
    let with_alice = Call::direct(&c.alice, 7, CallMedia::Voice);
    let with_bob = Call::direct(&c.bob, 7, CallMedia::Video);
    h.apply(vec![with_alice.ring(ts(1)), with_bob.ring(ts(2))])
        .expect("rings");
    h.apply(vec![with_bob.answered_elsewhere(ts(5))])
        .expect("answered");
    assert!(matches!(
        line(&h, &with_alice.conv).0.state,
        CallState::Ringing { .. }
    ));
    assert_eq!(line(&h, &with_bob.conv).0.state, CallState::Answered);
    h.apply(vec![
        with_alice.caller_gave_up(ts(9)),
        with_bob.deleted(ts(9)),
    ])
    .expect("apply");
    assert_eq!(line(&h, &with_alice.conv).0.state, CallState::Missed);
    assert!(rows(&h, &with_bob.conv).is_empty(), "only that chat's line");
    assert_eq!(unread(&h, &with_alice.conv), 1);
    // A delete that comes first waits for the line of its own chat.
    let third = Call::group(&c.group, 7, &c.alice);
    h.apply(vec![third.deleted(ts(20))]).expect("delete first");
    h.apply(vec![third.ring_cancelled(ts(15))]).expect("line");
    assert!(rows(&h, &third.conv).is_empty());
    assert_eq!(line(&h, &with_alice.conv).0.state, CallState::Missed);
}

// ---------- through the runtime, on the fake network ----------

async fn linked(script: Script) -> (Env, kourier_model::AccountId) {
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    (env, account)
}

fn conv_of(env: &Env, key: &ConvKey) -> ConversationId {
    support::send::conv_id(&env.core_db(), key).expect("conversation")
}

async fn line_of(env: &Env, key: &ConvKey) -> Option<(CallLine, MessageRow)> {
    let conv = support::send::conv_id(&env.core_db(), key)?;
    env.timeline(conv)
        .await
        .into_iter()
        .find_map(|r| r.body.call_line().cloned().map(|l| (l, r)))
}

async fn state_is(env: &Env, key: &ConvKey, what: &str, f: fn(CallState) -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        if line_of(env, key).await.is_some_and(|(l, _)| f(l.state)) {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn unread_of(env: &Env, key: &ConvKey) -> u32 {
    let conv = conv_of(env, key);
    env.conversations()
        .await
        .into_iter()
        .find(|r| r.id == conv)
        .map_or(u32::MAX, |r| r.unread)
}

fn ringing(s: CallState) -> bool {
    matches!(s, CallState::Ringing { .. })
}

fn missed(s: CallState) -> bool {
    s == CallState::Missed
}

/// Every event that is there right now.
fn drain(env: &mut Env) -> Vec<UiEvent> {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut out = Vec::new();
    loop {
        match env.events.try_recv() {
            Ok(e) => out.push(e),
            Err(TryRecvError::Lagged(n)) => panic!("{n} events were lost"),
            Err(_) => return out,
        }
    }
}

/// Waits until something sleeps on the fake clock until `at`: only then does moving the
/// clock there wake it.
async fn sleeping_until(env: &Env, at: u64) {
    let clock = env.clock.clone();
    eventually(&format!("a sleep until {at}"), WAIT, || {
        let clock = clock.clone();
        async move { clock.deadlines().contains(&at) }
    })
    .await;
}

/// End to end: a call rings in the chat without a notification; when its deadline comes on
/// the core's clock the line is a missed call, unread and announced; reading it and deleting
/// it for me send nothing to the network; the phone's late "answered" for a second call takes
/// that line's unread mark away and tells the desktop which row changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_rings_is_missed_at_its_deadline_and_nothing_is_sent() {
    let c = cast().expect("cast");
    let call = Call::direct(&c.alice, 71, CallMedia::Video);
    let second = Call::direct(&c.alice, 72, CallMedia::Voice);
    let mut chat = ConversationPatch::new(call.conv.clone());
    chat.features = Some(signal_features(false));
    let script = Script::new("call-e2e", vec![c.me.clone()])
        .deliver(vec![BackendEvent::ConversationUpsert(chat)])
        .deliver(vec![call.ring(ts(1))])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(600)))
        .deliver(vec![second.caller_gave_up(ts(700))])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(600)))
        .deliver(vec![second.answered_elsewhere(ts(1300))]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let db = env.core_db();
    eventually("the chat", WAIT, || {
        let (db, key) = (db.clone(), call.conv.clone());
        async move { support::send::conv_id(&db, &key).is_some() }
    })
    .await;
    state_is(&env, &call.conv, "the call rings", ringing).await;
    assert_eq!(unread_of(&env, &call.conv).await, 0);
    assert!(
        !drain(&mut env)
            .iter()
            .any(|e| matches!(e, UiEvent::Notify(_))),
        "a ringing call is not announced"
    );

    // The deadline comes (the clock only moves when the test moves it; the script sleeps
    // for ten minutes meanwhile).
    let t0 = T0 as u64;
    sleeping_until(&env, t0 + 600_000).await;
    let (_, row) = line_of(&env, &call.conv).await.expect("line");
    env.clock.set(ring_end(ts(1)) as u64);
    let notified = env
        .event("the missed call is announced", |e| match e {
            UiEvent::Notify(n) => Some(n.message),
            _ => None,
        })
        .await;
    assert_eq!(notified, row.id);
    state_is(&env, &call.conv, "the call is missed", missed).await;
    assert_eq!(unread_of(&env, &call.conv).await, 1);
    let conv = conv_of(&env, &call.conv);

    // Read here, then deleted for me: the network hears of neither.
    env.handle.send(UiCommand::MarkRead { conv }).expect("send");
    let h = env.handle.clone();
    eventually("read", WAIT, || {
        let h = h.clone();
        async move {
            matches!(
                h.query(kourier_core::facade::Query::Conversation(conv)).await,
                Ok(kourier_core::facade::QueryData::Conversation(Some(d))) if d.row.unread == 0
            )
        }
    })
    .await;
    env.handle
        .send(UiCommand::DeleteMessage {
            message: row.id,
            for_everyone: false,
        })
        .expect("send");
    let deadline = tokio::time::Instant::now() + WAIT;
    while line_of(&env, &call.conv).await.is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the line is deleted"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(env.control.transmits(uuid).is_empty(), "nothing was sent");
    assert_eq!(env.control.durable_commands(uuid), Vec::new());
    let queued = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT (SELECT count(*) FROM outbox) + (SELECT count(*) FROM pending_commands)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
    });
    assert_eq!(queued, 0);
    // A late copy of the ring does not bring the deleted line back (the script goes on).
    drain(&mut env);

    // A second call, missed at once, then answered on the phone after all.
    env.clock.set(t0 + 600_000);
    state_is(&env, &call.conv, "the second call is missed", missed).await;
    let (_, second_row) = line_of(&env, &call.conv).await.expect("line");
    assert_ne!(second_row.id, row.id, "the deleted line stays deleted");
    assert_eq!(unread_of(&env, &call.conv).await, 1);
    sleeping_until(&env, t0 + 1_200_000).await;
    drain(&mut env);
    env.clock.set(t0 + 1_200_000);
    let changed = env
        .event("the line changed", |e| match e {
            UiEvent::MessagesChanged { ids, .. } if ids.contains(&second_row.id) => Some(()),
            _ => None,
        })
        .await;
    let () = changed;
    state_is(&env, &call.conv, "answered", |s| s == CallState::Answered).await;
    assert_eq!(unread_of(&env, &call.conv).await, 0);
    let (_, answered) = line_of(&env, &call.conv).await.expect("line");
    assert_eq!(answered.read_state, None);
    env.wait_finished(account).await;
    assert!(env.control.transmits(uuid).is_empty(), "nothing was sent");
    env.stop().await;
}

/// The deadline holds across a restart: a call that rang when Kourier was closed is a
/// missed call at the first look of the next start, whatever came in meanwhile. It is unread
/// at once, but nobody is told yet: the desktop may not listen, and the phone's sync may still
/// say that the call was answered. The summary of the catch-up carries it, once the user
/// interface is ready and the account is online, if it is still a missed call then.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ring_that_ran_out_while_kourier_was_closed_is_missed_at_the_start() {
    let c = cast().expect("cast");
    let call = Call::direct(&c.alice, 81, CallMedia::Voice);
    let script = Script::new("call-restart", vec![c.me.clone()]).deliver(vec![call.ring(ts(1))]);
    let (mut env, account) = linked(script).await;
    state_is(&env, &call.conv, "the call rings", ringing).await;
    env.stop().await;
    let still = core_db(&env.core_db(), |c| {
        c.query_row(
            "SELECT count(*) FROM messages WHERE call_deadline IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect("count")
    });
    assert_eq!(still, 1, "the deadline is stored");
    env.clock.advance(Duration::from_secs(3600));
    env.restart().await;
    state_is(&env, &call.conv, "missed after the restart", missed).await;
    assert_eq!(unread_of(&env, &call.conv).await, 1);
    // Nobody is told before the user interface is ready and the account caught up.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let early = drain(&mut env);
    assert!(
        !early
            .iter()
            .any(|e| matches!(e, UiEvent::Notify(_) | UiEvent::CatchUpSummary { .. })),
        "announced before anybody listens: {early:?}"
    );
    // The interface is ready: the account connects, and the summary carries the missed call.
    env.handle.send(UiCommand::UiReady).expect("send");
    let summary = env
        .event("the summary", |e| match e {
            UiEvent::Notify(n) => panic!("a missed call of last night announced alone: {n:?}"),
            UiEvent::CatchUpSummary {
                messages,
                conversations,
                ..
            } => Some((*messages, *conversations)),
            _ => None,
        })
        .await;
    assert_eq!(summary, (1, 1));
    env.wait_finished(account).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let later = drain(&mut env);
    assert!(
        !later
            .iter()
            .any(|e| matches!(e, UiEvent::Notify(_) | UiEvent::CatchUpSummary { .. })),
        "told once: {later:?}"
    );
    env.stop().await;
}

/// A clock as the machine has it around a suspend (as in `robustness.rs`): the wall clock
/// jumps ahead, and a sleep that was armed before the suspend does not end for it. A sleep
/// armed after the machine woke ends when its time comes.
#[derive(Clone)]
struct SuspendClock {
    now: Arc<AtomicU64>,
    /// How often the machine slept.
    suspends: Arc<AtomicU64>,
}

impl SuspendClock {
    /// The machine sleeps until `now`.
    fn suspend_until(&self, now: u64) {
        self.suspends.fetch_add(1, Ordering::SeqCst);
        self.now.store(now, Ordering::SeqCst);
    }
}

impl Clock for SuspendClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    fn sleep_until(&self, deadline_ms: u64) -> kourier_backend_api::Sleep {
        let (now, suspends) = (self.now.clone(), self.suspends.clone());
        let armed = suspends.load(Ordering::SeqCst);
        Box::pin(async move {
            loop {
                if suspends.load(Ordering::SeqCst) != armed {
                    std::future::pending::<()>().await;
                }
                if now.load(Ordering::SeqCst) >= deadline_ms {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    }
}

/// The deadline holds across a suspend: the timer towards it slept with the machine, and
/// `Resumed` makes the core look at the clock again. The ring ran out long ago by then: the
/// line is a missed call at once, but it is not announced alone, because what the phone
/// synced during the suspend comes with the reconnect. The backend here keeps its
/// connection, so when the quiet time after the resume is over the missed call is announced
/// as a summary.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_settles_the_ring_that_ran_out_during_the_suspend() {
    let c = cast().expect("cast");
    let call = Call::direct(&c.alice, 91, CallMedia::Voice);
    let script = Script::new("call-suspend", vec![c.me.clone()]).deliver(vec![call.ring(ts(1))]);
    let t0 = T0 as u64;
    let clock = SuspendClock {
        now: Arc::new(AtomicU64::new(t0)),
        suspends: Arc::new(AtomicU64::new(0)),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let fake = kourier_fake::FakeNetwork::new(Flavor::SignalLike, script);
    let vault = Arc::new(kourier_core::testing::DirVault::new(
        dir.path().join("data"),
    ));
    let core = support::start_core_with(dir.path(), &fake, &vault, Arc::new(clock.clone())).await;
    let handle = core.handle();
    let mut env = Env {
        dir,
        // Not the core's clock: only there because the helpers take an `Env`.
        clock: kourier_backend_api::FakeClock::new(t0),
        control: fake.control(),
        fake,
        vault,
        events: handle.subscribe(),
        handle,
        core: Some(core),
    };
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    env.wait_finished(account).await;
    state_is(&env, &call.conv, "the call rings", ringing).await;
    // The deadline task has looked and sleeps towards the deadline.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The machine sleeps for an hour. Nothing fires by itself afterwards.
    clock.suspend_until(t0 + 3_600_000);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        line_of(&env, &call.conv)
            .await
            .is_some_and(|(l, _)| ringing(l.state)),
        "the timer slept through the suspend"
    );
    drain(&mut env);
    env.handle.send(UiCommand::Resumed).expect("send");
    state_is(&env, &call.conv, "missed after the resume", missed).await;
    assert_eq!(unread_of(&env, &call.conv).await, 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let told = drain(&mut env);
    assert!(
        !told
            .iter()
            .any(|e| matches!(e, UiEvent::Notify(_) | UiEvent::CatchUpSummary { .. })),
        "a ring that ran out an hour ago is not announced alone: {told:?}"
    );
    // The quiet time after the resume passes and the connection never dropped: now.
    clock.now.store(t0 + 3_600_000 + 30_000, Ordering::SeqCst);
    let summary = env
        .event("the summary", |e| match e {
            UiEvent::Notify(n) => panic!("announced alone: {n:?}"),
            UiEvent::CatchUpSummary {
                messages,
                conversations,
                ..
            } => Some((*messages, *conversations)),
            _ => None,
        })
        .await;
    assert_eq!(summary, (1, 1));
    env.stop().await;
}

/// A missed call that was held is not announced when the phone's sync says meanwhile that it
/// was answered. Here the deadline task finds the ring run out minutes ago with no resume
/// reported (a suspend nobody told us of): that alone starts the quiet time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_missed_call_that_was_answered_is_never_announced() {
    let c = cast().expect("cast");
    let call = Call::direct(&c.alice, 95, CallMedia::Voice);
    let script = Script::new("call-held", vec![c.me.clone()])
        .deliver(vec![call.ring(ts(1))])
        .step(Step::WaitAcked)
        .step(Step::Sleep(Duration::from_secs(310)))
        .deliver(vec![call.answered_elsewhere(ts(30))]);
    let mut env = Env::new(Flavor::SignalLike, script).await;
    env.handle.send(UiCommand::UiReady).expect("send");
    let account = add_linked(&mut env).await;
    state_is(&env, &call.conv, "the call rings", ringing).await;
    let t0 = T0 as u64;
    sleeping_until(&env, t0 + 310_000).await;
    drain(&mut env);
    // Five minutes later the deadline task wakes: the ring ran out long ago.
    env.clock.set(t0 + 300_000);
    state_is(&env, &call.conv, "missed", missed).await;
    assert_eq!(unread_of(&env, &call.conv).await, 1);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let told = drain(&mut env);
    assert!(
        !told
            .iter()
            .any(|e| matches!(e, UiEvent::Notify(_) | UiEvent::CatchUpSummary { .. })),
        "a ring that ran out minutes ago is not announced at once: {told:?}"
    );
    // Ten seconds on, within the quiet time, the phone's sync arrives: answered.
    env.clock.set(t0 + 310_000);
    state_is(&env, &call.conv, "answered", |s| s == CallState::Answered).await;
    assert_eq!(unread_of(&env, &call.conv).await, 0);
    env.wait_finished(account).await;
    // The quiet time ends: the line is no missed call any more, nobody is told anything.
    env.clock.advance(Duration::from_secs(120));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let told = drain(&mut env);
    assert!(
        !told
            .iter()
            .any(|e| matches!(e, UiEvent::Notify(_) | UiEvent::CatchUpSummary { .. })),
        "an answered call was announced as missed: {told:?}"
    );
    env.stop().await;
}

/// The whole script of `kourier_fake::calls` through the runtime: one line per call, the
/// deleted one gone, and the counts of the two chats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_calls_script_through_the_runtime() {
    let c = cast().expect("cast");
    let (mut env, account) = linked(kourier_fake::calls::script().expect("script")).await;
    let uuid = env.account(account).await.expect("account").uuid;
    let states = |rows: Vec<MessageRow>| -> Vec<(CallDirection, u8)> {
        let mut v: Vec<(i64, CallDirection, u8)> = rows
            .iter()
            .filter_map(|r| {
                r.body
                    .call_line()
                    .map(|l| (r.sent_at, l.direction, l.state.rank()))
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, d, s)| (d, s)).collect()
    };
    use CallDirection::{Incoming, Outgoing};
    let alice = env
        .timeline(conv_of(&env, &ConvKey::Direct(c.alice.clone())))
        .await;
    // Given up (missed), declined, from the phone and answered; the deleted one is gone.
    assert_eq!(states(alice), [(Incoming, 2), (Incoming, 3), (Outgoing, 4)]);
    let bob = env
        .timeline(conv_of(&env, &ConvKey::Direct(c.bob.clone())))
        .await;
    // Answered on the phone, one that still rings, one from the phone nobody answered.
    assert_eq!(states(bob), [(Incoming, 4), (Incoming, 1), (Outgoing, 2)]);
    let group = env.timeline(conv_of(&env, &c.group)).await;
    // Ring cancelled, joined on the phone, declined there, started without a ring.
    assert_eq!(
        states(group),
        [(Incoming, 2), (Incoming, 4), (Incoming, 3), (Incoming, 0)]
    );
    assert_eq!(unread_of(&env, &ConvKey::Direct(c.alice.clone())).await, 1);
    assert_eq!(unread_of(&env, &ConvKey::Direct(c.bob.clone())).await, 0);
    assert_eq!(unread_of(&env, &c.group).await, 2, "missed, and started");
    let notified = drain(&mut env)
        .iter()
        .filter(|e| matches!(e, UiEvent::Notify(_)))
        .count();
    // The two missed calls (the deleted one was announced before its delete came) and the
    // started group call; never a ringing, answered or declined one.
    assert_eq!(notified, 4);
    assert!(env.control.transmits(uuid).is_empty());
    assert_eq!(env.control.durable_commands(uuid), Vec::new());
    env.stop().await;
}
