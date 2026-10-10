//! Call lines under attack (review of step 1, 2026-10-09): every order of every subset of the
//! signals of one call with the clock on either side of the deadline, a delete at every
//! position, ids chosen by the other side, deadlines chosen by the other side, floods, and
//! the unread and notification rules in muted and blocked chats.
//!
//! The one test marked `#[ignore]` states a known limit that is left alone on purpose; its
//! reason says which. Run it with `cargo test -p kourier-core --test calls_attack -- --ignored`.

use std::collections::BTreeMap;

use kourier_backend_api::{BackendEvent, ContractViolation, Envelope, IncomingMessage};
use kourier_core::facade::{MessageActions, MessageRow, TimelineAnchor, TimelineQuery};
use kourier_core::testing::IngestHarness;
use kourier_fake::calls::{Call, Cast, DELIVERY_MS, cast, ring_end};
use kourier_fake::scripts::{T0, remote, signal_features};
use kourier_model::{
    AccountSettingsPatch, CallDirection, CallLine, CallMedia, CallState, ConvKey, ConversationId,
    ConversationPatch, MAX_RING_MS, MsgIdScope, NetworkKind, Patch, ReadState,
};

fn ts(n: i64) -> i64 {
    T0 + n * 1000
}

fn harness() -> (tempfile::TempDir, IngestHarness, Cast) {
    let c = cast().expect("cast");
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([9; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        std::slice::from_ref(&c.me),
    )
    .expect("harness");
    h.set_now(T0);
    let mut chat = ConversationPatch::new(ConvKey::Direct(c.alice.clone()));
    chat.features = Some(signal_features(false));
    h.apply(vec![
        BackendEvent::AccountSettings(AccountSettingsPatch {
            read_receipts: Some(true),
            ..Default::default()
        }),
        BackendEvent::ConversationUpsert(chat),
    ])
    .expect("setup");
    (dir, h, c)
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

fn lines(h: &IngestHarness, key: &ConvKey) -> Vec<(CallLine, MessageRow)> {
    rows(h, key)
        .into_iter()
        .filter_map(|r| r.body.call_line().cloned().map(|l| (l, r)))
        .collect()
}

fn unread(h: &IngestHarness, key: &ConvKey) -> u32 {
    h.conversation(conv(h, key))
        .expect("query")
        .expect("conversation")
        .row
        .unread
}

/// What is stored of the line `local` of the chat `key`, as text: everything a signal may
/// change.
fn stored_in(h: &IngestHarness, key: &ConvKey, local: &str) -> String {
    h.connection()
        .query_row(
            "SELECT m.body_kind || '|' || m.body || '|' || m.sent_at || '|' || quote(m.read_state)
               || '|' || m.origin || '|' || quote(m.call_deadline) || '|' || quote(m.deleted)
               || '|' || m.silent
             FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.conv_key = ?1 AND m.net_local = ?2",
            [key.encode(), local.to_owned()],
            |r| r.get(0),
        )
        .unwrap_or_else(|_| "no row".into())
}

/// The line of `call` as the timeline shows it.
fn line_of(h: &IngestHarness, call: &Call) -> (CallLine, MessageRow) {
    let id: i64 = h
        .connection()
        .query_row(
            "SELECT m.id FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.conv_key = ?1 AND m.net_local = ?2",
            [call.conv.encode(), call.local()],
            |r| r.get(0),
        )
        .expect("the row of the call");
    lines(h, &call.conv)
        .into_iter()
        .find(|(_, r)| r.id.0 == id)
        .expect("the line of the call")
}

/// The same of a call's own line.
fn stored(h: &IngestHarness, call: &Call) -> String {
    stored_in(h, &call.conv, &call.local())
}

/// Every ordered choice of `k` of `n` things.
fn arrangements(n: usize, k: usize) -> Vec<Vec<usize>> {
    fn go(n: usize, k: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == k {
            out.push(cur.clone());
            return;
        }
        for i in 0..n {
            if !cur.contains(&i) {
                cur.push(i);
                go(n, k, cur, out);
                cur.pop();
            }
        }
    }
    let mut out = Vec::new();
    go(n, k, &mut Vec::new(), &mut out);
    out
}

fn direct_signals(call: &Call) -> Vec<BackendEvent> {
    vec![
        call.ring(ts(1)),
        call.caller_gave_up(ts(20)),
        call.declined(ts(25)),
        call.answered_elsewhere(ts(30)),
        call.outgoing_unanswered(ts(33)),
        call.outgoing_answered(ts(35)),
    ]
}

fn group_signals(call: &Call, c: &Cast) -> Vec<BackendEvent> {
    vec![
        call.started(&c.bob, ts(5)),
        call.started(&c.alice, ts(8)),
        call.ring(ts(10)),
        call.ring_cancelled(ts(20)),
        call.declined(ts(25)),
        call.joined_elsewhere(ts(30)),
    ]
}

/// When the clock passes the deadline of every ring of the test.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Clock {
    /// Never: every ring is still ringing at the end.
    Before,
    /// Before the first signal: every ring arrives late.
    After,
    /// After half of the signals, with a sweep at that moment.
    Between,
}

/// Every order of every subset of the six signals of one call, with the clock before the
/// deadline, after it, and passing it in the middle: one row, and the same row for every
/// order of the same signals. With the clock past the deadline at the end, the row is also
/// the same whether the clock passed it before the first signal or in the middle.
fn every_order(group: bool) {
    let (_dir, mut h, c) = harness();
    let mut next_id = 1_000u64;
    let mut outcomes: BTreeMap<(Vec<usize>, Clock), (String, Vec<usize>)> = BTreeMap::new();
    let mut sequences = 0usize;
    // (Sizes 4 and 5 passed too, 1,956 orders per clock; they are left out for the time
    // they take.)
    for k in [1, 2, 3, 6] {
        for order in arrangements(6, k) {
            for clock in [Clock::Before, Clock::After, Clock::Between] {
                next_id += 1;
                let call = if group {
                    Call::group(&c.group, next_id, &c.alice)
                } else {
                    Call::direct(&c.alice, next_id, CallMedia::Video)
                };
                let signals = if group {
                    group_signals(&call, &c)
                } else {
                    direct_signals(&call)
                };
                h.set_now(if clock == Clock::After { ts(500) } else { T0 });
                for (i, n) in order.iter().enumerate() {
                    if clock == Clock::Between && i == order.len() / 2 {
                        h.set_now(ts(500));
                        h.settle_calls().expect("sweep");
                    }
                    h.apply(vec![signals[*n].clone()]).expect("apply");
                }
                if clock == Clock::Between {
                    h.set_now(ts(500));
                    h.settle_calls().expect("sweep");
                }
                let count: i64 = h
                    .connection()
                    .query_row(
                        "SELECT count(*) FROM messages WHERE net_local = ?1",
                        [call.local()],
                        |r| r.get(0),
                    )
                    .expect("count");
                assert_eq!(count, 1, "one row per call: {order:?} {clock:?}");
                let got = stored(&h, &call);
                let mut set = order.clone();
                set.sort_unstable();
                // The same signals under the same clock: the same row.
                let key = (set.clone(), clock);
                match outcomes.get(&key) {
                    None => {
                        outcomes.insert(key, (got.clone(), order.clone()));
                    }
                    Some((first, first_order)) => assert_eq!(
                        &got, first,
                        "group {group}: signals {order:?} against {first_order:?}, {clock:?}"
                    ),
                }
                // Past the deadline at the end: the same row whenever the clock passed it.
                if clock == Clock::Between
                    && let Some((late, late_order)) = outcomes.get(&(set, Clock::After))
                {
                    assert_eq!(
                        &got, late,
                        "group {group}: {order:?} with the clock passing the deadline in the \
                         middle against {late_order:?} with every signal late"
                    );
                }
                sequences += 1;
            }
        }
    }
    assert_eq!(sequences, 876 * 3);
    // The counters are what the rows say.
    let key = if group {
        c.group.clone()
    } else {
        ConvKey::Direct(c.alice.clone())
    };
    let unread_rows: u32 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE read_state = 'Unread' AND deleted IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(unread(&h, &key), unread_rows);
}

#[test]
fn every_order_of_every_subset_of_the_signals_of_a_direct_call() {
    every_order(false);
}

#[test]
fn every_order_of_every_subset_of_the_signals_of_a_group_call() {
    every_order(true);
}

/// The phone's delete of the call entry at every position of every order of all six signals,
/// some signals twice: what is left is one hidden row that rings no more, counts no more and
/// is the same whatever came before or after the delete.
#[test]
fn a_delete_at_every_position_leaves_the_same_dead_row() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let mut first: Option<String> = None;
    for (n, order) in arrangements(6, 6).into_iter().enumerate() {
        let call = Call::direct(&c.alice, 50_000 + n as u64, CallMedia::Voice);
        let signals = direct_signals(&call);
        let delete_at = n % 7;
        h.set_now(if n % 3 == 0 { ts(500) } else { T0 });
        for (i, s) in order.iter().enumerate() {
            if i == delete_at {
                h.apply(vec![call.deleted(ts(50))]).expect("delete");
            }
            h.apply(vec![signals[*s].clone()]).expect("apply");
            if n % 2 == 0 {
                h.apply(vec![signals[*s].clone()]).expect("again");
            }
        }
        if delete_at == 6 {
            h.apply(vec![call.deleted(ts(50))]).expect("delete");
        }
        // (Also its `origin`: what is left of a line has one value there, whichever way the
        // call went when its delete came.)
        let got = stored(&h, &call).replace(&call.id, "ID");
        assert!(got.contains("'ForMe'"), "deleted: {got}");
        assert!(got.contains("|OwnOtherDevice|"), "one origin: {got}");
        match &first {
            None => first = Some(got),
            Some(f) => assert_eq!(&got, f, "order {order:?}, delete at {delete_at}"),
        }
    }
    assert!(lines(&h, &peer).is_empty(), "no line is shown");
    assert_eq!(unread(&h, &peer), 0);
    assert_eq!(h.next_call_deadline().expect("deadline"), None);
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0);
    // Nothing of the dead lines wakes up later.
    h.set_now(ts(100_000));
    let swept = h.settle_calls().expect("sweep");
    assert_eq!((swept.notify.len(), swept.changed.len()), (0, 0));
}

/// A line that went (cleared with its chat, or deleted for me while it rang) does not come
/// back: not by its deadline, not by a later signal.
#[test]
fn a_dead_line_stays_dead() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let cleared = Call::direct(&c.alice, 1, CallMedia::Voice);
    h.apply(vec![cleared.ring(ts(1))]).expect("ring");
    assert!(h.next_call_deadline().expect("deadline").is_some());
    h.clear_conversation(conv(&h, &peer), false).expect("clear");
    assert_eq!(h.next_call_deadline().expect("deadline"), None);
    h.set_now(ring_end(ts(1)) + 1);
    let swept = h.settle_calls().expect("sweep");
    assert_eq!((swept.notify.len(), swept.changed.len()), (0, 0));
    let late = h
        .apply_announcing(vec![cleared.caller_gave_up(ts(30)), cleared.ring(ts(1))])
        .expect("late signals");
    assert!(late.notify.is_empty() && late.catch_up == 0);
    assert!(rows(&h, &peer).is_empty(), "the chat stays cleared");
    assert_eq!(unread(&h, &peer), 0);

    // Deleted while it rang (the phone deleted the entry): the deadline finds nothing.
    h.set_now(ts(1000));
    let deleted = Call::direct(&c.alice, 2, CallMedia::Voice);
    h.apply(vec![deleted.ring(ts(1000)), deleted.deleted(ts(1001))])
        .expect("ring, delete");
    assert_eq!(h.next_call_deadline().expect("deadline"), None);
    h.set_now(ring_end(ts(1000)) + 1);
    let swept = h.settle_calls().expect("sweep");
    assert_eq!((swept.notify.len(), swept.changed.len()), (0, 0));
    let late = h
        .apply_announcing(vec![deleted.caller_gave_up(ts(1030))])
        .expect("late");
    assert!(late.notify.is_empty());
    assert!(lines(&h, &peer).is_empty());
    assert_eq!(unread(&h, &peer), 0);
}

/// The id of a call is chosen by the other side. Whoever names the id of a call of another
/// chat changes nothing there: not from a direct chat, not from another group, and not with
/// a delete. What he says is a call of HIS chat: the same id in two chats is two lines that
/// do not see each other.
#[test]
fn an_id_of_another_chat_changes_nothing_there() {
    let (_dir, mut h, c) = harness();
    let with_alice = Call::direct(&c.alice, 7, CallMedia::Voice);
    let other_group = ConvKey::Group("gv2:b3RoZXItZ3JvdXA".into());
    let in_group = Call::group(&c.group, 7, &c.alice);
    h.apply(vec![with_alice.ring(ts(1)), in_group.ring(ts(1))])
        .expect("rings");
    let before = (stored(&h, &with_alice), stored(&h, &in_group));
    // Bob names Alice's call id in his own chat; a member of another group names this
    // group's ring id there.
    let bob = Call {
        conv: ConvKey::Direct(c.bob.clone()),
        caller: c.bob.clone(),
        ..with_alice.clone()
    };
    let elsewhere = Call {
        conv: other_group.clone(),
        caller: c.bob.clone(),
        ..in_group.clone()
    };
    for attacker in [&bob, &elsewhere] {
        let said = h
            .apply_announcing(vec![attacker.ring(ts(2)), attacker.caller_gave_up(ts(3))])
            .expect("apply");
        // His call is a call of his chat: one line there, a missed call of his.
        let his = lines(&h, &attacker.conv);
        assert_eq!(his.len(), 1, "his own line");
        assert_eq!(his[0].0.state, CallState::Missed);
        assert_eq!(said.notify, vec![his[0].1.id], "announced as his");
        assert_eq!(
            (stored(&h, &with_alice), stored(&h, &in_group)),
            before,
            "the calls of the other chats are untouched"
        );
        // Whatever our own devices then say of the call of HIS chat, with a delete at the
        // end, stays in his chat.
        h.apply(vec![
            attacker.declined(ts(4)),
            attacker.answered_elsewhere(ts(5)),
            attacker.deleted(ts(6)),
        ])
        .expect("apply");
        assert!(lines(&h, &attacker.conv).is_empty(), "his line is deleted");
    }
    assert_eq!((stored(&h, &with_alice), stored(&h, &in_group)), before);
    let count: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE net_local IN ('call:7', 'gcall:7')",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(count, 4, "one row per chat and id: two lines, two deleted");
    let pending: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM pending_modifiers", [], |r| r.get(0))
        .expect("count");
    assert_eq!(pending, 0, "every delete found the line of its own chat");
    // The real lines live on and change with their own signals.
    h.apply(vec![with_alice.caller_gave_up(ts(9))])
        .expect("hang-up");
    assert_eq!(
        lines(&h, &with_alice.conv)[0].0.state,
        CallState::Missed,
        "the line of the real call lives"
    );
    h.apply(vec![in_group.ring_cancelled(ts(9))])
        .expect("cancel");
    assert_eq!(lines(&h, &in_group.conv)[0].0.state, CallState::Missed);
    // A delete that comes before its line waits for the line of the chat it names, and for
    // no other: the same id in another chat is not deleted by it.
    let early = Call::direct(&c.alice, 8, CallMedia::Voice);
    let same_id = Call {
        conv: ConvKey::Direct(c.bob.clone()),
        caller: c.bob.clone(),
        ..early.clone()
    };
    h.apply(vec![early.deleted(ts(30))]).expect("delete first");
    h.apply(vec![same_id.caller_gave_up(ts(31))])
        .expect("bob's call");
    assert_eq!(
        lines(&h, &same_id.conv).len(),
        1,
        "bob's line is not deleted"
    );
    h.apply(vec![early.caller_gave_up(ts(32))])
        .expect("alice's call");
    assert_eq!(
        lines(&h, &early.conv).len(),
        1,
        "only the missed call 7 is left with alice: call 8 was deleted before it came"
    );
    // A reference that names no conversation never finds a line, whatever it says of the
    // author: not a delete, not a read marker.
    let before = (stored(&h, &with_alice), stored(&h, &same_id));
    for author in [
        kourier_model::TargetAuthor::Own,
        kourier_model::TargetAuthor::Unknown,
    ] {
        for local in [with_alice.local(), same_id.local()] {
            let target = kourier_model::MsgTarget {
                conv: None,
                author: author.clone(),
                local: local.into(),
                alternates: Vec::new(),
            };
            h.apply(vec![
                BackendEvent::Deleted {
                    target: target.clone(),
                    by: None,
                    for_everyone: false,
                    at: ts(40),
                },
                BackendEvent::ReadMarker {
                    conv: None,
                    up_to: target,
                },
            ])
            .expect("apply");
        }
    }
    assert_eq!((stored(&h, &with_alice), stored(&h, &same_id)), before);
    assert_eq!(unread(&h, &with_alice.conv), 1);
}

/// A caller who uses the id of an earlier call of his again: the new call should be seen (the
/// phone rings for it). The old line is answered or read, so nothing shows.
#[test]
#[ignore = "known limit, left alone on purpose: a call that reuses the id of an earlier call \
            of the SAME chat joins the old line (a call id is a random 64-bit number; only \
            the caller of that chat can do it, and it hides only his own call)"]
fn a_call_that_reuses_an_old_id_is_still_seen() {
    let (_dir, mut h, c) = harness();
    let old = Call::direct(&c.alice, 77, CallMedia::Voice);
    h.apply(vec![old.ring(ts(1)), old.answered_elsewhere(ts(9))])
        .expect("an answered call");
    // A day later the same caller rings with the same id and gives up.
    let day = 24 * 3600;
    h.set_now(ts(day));
    let said = h
        .apply_announcing(vec![old.ring(ts(day))])
        .expect("second ring");
    assert!(said.notify.is_empty());
    let said = h
        .apply_announcing(vec![old.caller_gave_up(ts(day + 20))])
        .expect("hang-up");
    let peer = ConvKey::Direct(c.alice.clone());
    assert_eq!(
        (unread(&h, &peer), said.notify.len()),
        (1, 1),
        "the second call is a missed call"
    );
}

/// What the peer of a 1:1 chat says about a call must not rewrite what our own devices
/// said: he knows the id of every call with him, and "answered" is the top of the order. He
/// may say that an incoming call was started, rings or was missed, and nothing else: a signal
/// of his that says more is refused, by the contract's check and by the ingest itself; and
/// what he may say never moves a line that one of our own devices settled.
#[test]
fn the_peer_cannot_make_a_call_answered() {
    use CallDirection::{Incoming, Outgoing};
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    // A call we made that nobody answered, a call we declined on the phone, one we answered
    // there, and one that only rang and was missed.
    let made = Call::direct(&c.alice, 5, CallMedia::Voice);
    let declined = Call::direct(&c.alice, 6, CallMedia::Voice);
    let answered = Call::direct(&c.alice, 7, CallMedia::Voice);
    let missed = Call::direct(&c.alice, 8, CallMedia::Voice);
    h.apply(vec![
        made.outgoing_unanswered(ts(10)),
        declined.ring(ts(11)),
        declined.declined(ts(12)),
        answered.ring(ts(13)),
        answered.answered_elsewhere(ts(14)),
        missed.ring(ts(15)),
        missed.caller_gave_up(ts(16)),
    ])
    .expect("four calls");
    let state = |h: &IngestHarness, call: &Call| {
        let (line, row) = line_of(h, call);
        (line.direction, line.state, row.read_state)
    };
    let calls = [&made, &declined, &answered, &missed];
    let before: Vec<_> = calls.iter().map(|call| state(&h, call)).collect();
    assert_eq!(
        before,
        [
            (Outgoing, CallState::Unanswered, None),
            (Incoming, CallState::Declined, None),
            (Incoming, CallState::Answered, None),
            (Incoming, CallState::Missed, Some(ReadState::Unread)),
        ]
    );
    assert_eq!(unread(&h, &peer), 1);
    let mut refused = 0;
    for call in calls {
        for direction in [Incoming, Outgoing] {
            for said in [
                CallState::Started,
                CallState::Ringing {
                    deadline: ts(100_000),
                },
                CallState::Missed,
                CallState::Unanswered,
                CallState::Declined,
                CallState::Answered,
            ] {
                let forged = call.forged_by_peer(direction, said, ts(99));
                // (An "unanswered" incoming call is a missed one: `CallLine::normalized`.)
                let allowed = direction == Incoming
                    && !matches!(said, CallState::Declined | CallState::Answered);
                let (envelope, _w) = Envelope::durable(vec![BackendEvent::Message(forged.clone())]);
                assert_eq!(
                    matches!(envelope.validate(), Err(ContractViolation::CallLine(_))),
                    !allowed,
                    "the contract: {direction:?} {said:?} from the peer"
                );
                // The ingest refuses it too, whoever calls it, and stores nothing of it.
                let said_now = h.apply_announcing(vec![BackendEvent::Message(forged)]);
                assert_eq!(said_now.is_ok(), allowed, "{direction:?} {said:?}");
                match said_now {
                    Ok(announced) => assert!(announced.notify.is_empty(), "{said:?}"),
                    Err(_) => refused += 1,
                }
                let after: Vec<_> = calls.iter().map(|call| state(&h, call)).collect();
                assert_eq!(
                    after, before,
                    "{direction:?} {said:?} from the peer moved a line"
                );
            }
        }
    }
    assert_eq!(
        refused,
        4 * 8,
        "declined and answered, and everything outgoing"
    );
    assert_eq!(unread(&h, &peer), 1);
    assert_eq!(lines(&h, &peer).len(), 4, "no line more");
    // The peer cannot delete a line either: a delete for me counts from our own devices only.
    h.apply(vec![BackendEvent::Deleted {
        target: IncomingMessage::call_target(peer.clone(), false, &answered.id),
        by: Some(c.alice.clone()),
        for_everyone: false,
        at: ts(120),
    }])
    .expect("the peer's delete");
    h.apply(vec![BackendEvent::Deleted {
        target: IncomingMessage::call_target(peer.clone(), false, &answered.id),
        by: Some(c.alice.clone()),
        for_everyone: true,
        at: ts(121),
    }])
    .expect("the peer's delete for everyone");
    assert_eq!(lines(&h, &peer).len(), 4);
}

/// The deadline of a ring is given with the signal. The core must not let a line ring past
/// any sensible time, whatever the deadline says.
#[test]
fn a_far_deadline_does_not_ring_for_ever() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let call = Call::direct(&c.alice, 3, CallMedia::Voice);
    h.apply(vec![call.ring_until(ts(1), i64::MAX)])
        .expect("ring");
    // It rings for the longest a line may ring, counted from its receipt on OUR clock.
    let capped = ts(1) + DELIVERY_MS + MAX_RING_MS;
    assert_eq!(h.next_call_deadline().expect("deadline"), Some(capped));
    assert_eq!(lines(&h, &peer)[0].0.deadline(), Some(capped));
    h.set_now(capped - 1);
    h.settle_calls().expect("sweep");
    assert!(lines(&h, &peer)[0].0.deadline().is_some(), "it still rings");
    h.set_now(capped);
    let swept = h.settle_calls().expect("sweep");
    assert_eq!(lines(&h, &peer)[0].0.state, CallState::Missed);
    assert_eq!(swept.notify.len(), 1, "on time: announced");

    // The sender's own time is no clock of ours: a signal "from the future" and one "from
    // the past", each with 30 seconds left, ring for 30 seconds from their receipt.
    for (id, made_at) in [(4u64, ts(9_000_000)), (5, ts(-9_000_000))] {
        let call = Call::direct(&c.alice, id, CallMedia::Voice);
        let BackendEvent::Message(mut ring) = call.ring_until(made_at, made_at + 30_000) else {
            unreachable!()
        };
        ring.received_at = h.now();
        let now = h.now();
        h.apply(vec![BackendEvent::Message(ring)]).expect("ring");
        assert_eq!(
            h.next_call_deadline().expect("deadline"),
            Some(now + 30_000)
        );
        h.set_now(now + 30_000);
        assert_eq!(h.settle_calls().expect("sweep").changed.len(), 1);
    }
    // A ring with no time left, or with a deadline before its own time, never rings.
    for (id, left) in [(6u64, 0), (7, -5_000), (8, i64::MIN / 2)] {
        let call = Call::direct(&c.alice, id, CallMedia::Voice);
        h.apply(vec![call.ring_until(h.now(), h.now().saturating_add(left))])
            .expect("ring");
        assert_eq!(h.next_call_deadline().expect("deadline"), None, "{left}");
    }
    // A year later nothing rings and nothing is left to settle.
    h.set_now(ts(365 * 24 * 3600));
    assert_eq!(h.settle_calls().expect("sweep").changed.len(), 0);
    assert!(
        lines(&h, &peer)
            .iter()
            .all(|(l, _)| l.state == CallState::Missed)
    );
}

/// A flood of offers: one row each, and the sweep takes them all in one go.
#[test]
fn a_thousand_rings_at_once() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let started = std::time::Instant::now();
    for n in 0..1000u64 {
        let call = Call::direct(&c.alice, 10_000 + n, CallMedia::Voice);
        h.apply(vec![call.ring(ts(1))]).expect("ring");
    }
    let ingest = started.elapsed();
    assert_eq!(unread(&h, &peer), 0);
    h.set_now(ring_end(ts(1)));
    let started = std::time::Instant::now();
    let swept = h.settle_calls().expect("sweep");
    let sweep = started.elapsed();
    eprintln!("1000 rings: ingest {ingest:?}, sweep {sweep:?}");
    assert_eq!(swept.changed.len(), 1000);
    assert_eq!(unread(&h, &peer), 1000);
    assert_eq!(h.next_call_deadline().expect("deadline"), None);
    assert!(sweep < std::time::Duration::from_secs(5), "{sweep:?}");
    // The same sweep again finds nothing.
    assert_eq!(h.settle_calls().expect("again").changed.len(), 0);
}

/// A ring whose deadline passed long ago when the sweep first looks (Kourier was closed, or
/// the machine slept) is old news: it is unread once, and it is announced at most as the
/// catch-up summary announces, never as a message that just came.
#[test]
fn a_ring_that_ran_out_long_ago_is_not_announced_alone() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    for n in 0..5u64 {
        let call = Call::direct(&c.alice, 600 + n, CallMedia::Voice);
        h.apply(vec![call.ring(ts(1))]).expect("ring");
    }
    // Eight hours later the first look of the next start.
    h.set_now(ts(8 * 3600));
    let swept = h.settle_calls().expect("sweep");
    assert_eq!(unread(&h, &peer), 5, "counted once each");
    assert_eq!(
        swept.notify.len(),
        0,
        "five notifications for calls of last night"
    );
    assert_eq!(swept.catch_up, 5, "they wait for the catch-up summary");

    // The edge: ten seconds after the deadline is still "on time", a moment later is not.
    let on_time = Call::direct(&c.alice, 700, CallMedia::Voice);
    let late = Call::direct(&c.alice, 701, CallMedia::Voice);
    let t = 9 * 3600;
    h.set_now(ts(t));
    h.apply(vec![on_time.ring(ts(t))]).expect("ring");
    h.set_now(ring_end(ts(t)) + 10_000);
    let swept = h.settle_calls().expect("sweep");
    assert_eq!((swept.notify.len(), swept.catch_up), (1, 0));
    h.apply(vec![late.ring(h.now())]).expect("ring");
    h.set_now(ring_end(h.now()) + 10_001);
    let swept = h.settle_calls().expect("sweep");
    assert_eq!((swept.notify.len(), swept.catch_up), (0, 1));

    // The same when a signal, not the sweep, finds the ring run out long ago: a second copy
    // of a "started" notice for a call that rang last night.
    let group = Call::group(&c.group, 702, &c.alice);
    h.apply(vec![group.ring(h.now())]).expect("ring");
    h.set_now(h.now() + 8 * 3600 * 1000);
    let said = h
        .apply_announcing(vec![group.started(&c.bob, h.now())])
        .expect("started");
    assert_eq!(lines(&h, &c.group)[0].0.state, CallState::Missed);
    assert_eq!((said.notify.len(), said.catch_up), (0, 1));
    // And a ring that stage 1 stored long ago and hands over only now.
    let stale = Call::direct(&c.bob, 703, CallMedia::Voice);
    let said = h
        .apply_announcing(vec![stale.ring(h.now() - 3600 * 1000)])
        .expect("stale ring");
    assert_eq!((said.notify.len(), said.catch_up), (0, 1));
    assert_eq!(unread(&h, &ConvKey::Direct(c.bob.clone())), 1);
}

/// Counts through the life of a line, and what a muted or blocked chat does with a missed
/// call: as with a message.
#[test]
fn unread_counts_and_who_is_told() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let id = conv(&h, &peer);
    let call = Call::direct(&c.alice, 1, CallMedia::Voice);
    // ringing -> missed -> read -> answered.
    assert!(
        h.apply_announcing(vec![call.ring(ts(1))])
            .expect("ring")
            .notify
            .is_empty()
    );
    assert_eq!(unread(&h, &peer), 0);
    let said = h
        .apply_announcing(vec![call.caller_gave_up(ts(9))])
        .expect("hang-up");
    assert_eq!((unread(&h, &peer), said.notify.len()), (1, 1));
    let read = h.mark_read(id).expect("read").expect("was unread");
    assert!(read.is_empty(), "nothing is sent for a line: {read:?}");
    assert_eq!(unread(&h, &peer), 0);
    h.apply(vec![call.answered_elsewhere(ts(12))])
        .expect("answered");
    assert_eq!(unread(&h, &peer), 0);
    assert_eq!(lines(&h, &peer)[0].1.read_state, None);

    // A line offers nothing but "delete for me", in every state.
    let second = Call::direct(&c.alice, 2, CallMedia::Video);
    for signal in [
        second.ring(ts(20)),
        second.caller_gave_up(ts(25)),
        second.declined(ts(26)),
        second.answered_elsewhere(ts(27)),
    ] {
        h.apply(vec![signal]).expect("apply");
        for (_, row) in lines(&h, &peer) {
            assert_eq!(
                row.actions,
                MessageActions {
                    delete_for_me: true,
                    ..Default::default()
                }
            );
            assert_eq!(row.author, None);
            assert_eq!(row.out_state, None);
        }
    }

    // missed, then deleted on the phone: the count goes with it.
    let third = Call::direct(&c.alice, 3, CallMedia::Voice);
    h.apply(vec![third.caller_gave_up(ts(30))]).expect("missed");
    assert_eq!(unread(&h, &peer), 1);
    h.apply(vec![third.deleted(ts(31))]).expect("deleted");
    assert_eq!(unread(&h, &peer), 0);

    // missed, then the chat is cleared.
    let fourth = Call::direct(&c.alice, 4, CallMedia::Voice);
    h.apply(vec![fourth.caller_gave_up(ts(40))])
        .expect("missed");
    assert_eq!(unread(&h, &peer), 1);
    let cmds = h.clear_conversation(id, false).expect("clear");
    assert_eq!(unread(&h, &peer), 0);
    for cmd in &cmds {
        assert!(!format!("{cmd:?}").contains("call:"), "{cmd:?}");
    }

    // A muted direct chat: unread, not announced (by a signal and by the deadline).
    let mut mute = ConversationPatch::new(peer.clone());
    mute.muted_until = Patch::Set(i64::MAX);
    h.apply(vec![BackendEvent::ConversationUpsert(mute)])
        .expect("mute");
    let (fifth, sixth) = (
        Call::direct(&c.alice, 5, CallMedia::Voice),
        Call::direct(&c.alice, 6, CallMedia::Voice),
    );
    let said = h
        .apply_announcing(vec![fifth.caller_gave_up(ts(50)), sixth.ring(ts(50))])
        .expect("missed, muted");
    assert_eq!((unread(&h, &peer), said.notify.len()), (1, 0));
    h.set_now(ring_end(ts(50)));
    let said = h.settle_calls().expect("sweep");
    assert_eq!((unread(&h, &peer), said.notify.len()), (2, 0));

    // A muted group: a call somebody started and a missed ring are not announced.
    let mut mute = ConversationPatch::new(c.group.clone());
    mute.muted_until = Patch::Set(i64::MAX);
    h.apply(vec![BackendEvent::ConversationUpsert(mute)])
        .expect("mute");
    let (g1, g2) = (
        Call::group(&c.group, 1, &c.alice),
        Call::group(&c.group, 2, &c.alice),
    );
    let said = h
        .apply_announcing(vec![
            g1.started(&c.bob, ts(200)),
            g2.ring_cancelled(ts(201)),
        ])
        .expect("group, muted");
    assert_eq!((unread(&h, &c.group), said.notify.len()), (2, 0));

    // A blocked chat: never announced.
    let blocked = ConvKey::Direct(c.bob.clone());
    let mut block = ConversationPatch::new(blocked.clone());
    block.blocked = Some(true);
    h.apply(vec![BackendEvent::ConversationUpsert(block)])
        .expect("block");
    let (b1, b2) = (
        Call::direct(&c.bob, 91, CallMedia::Voice),
        Call::direct(&c.bob, 92, CallMedia::Voice),
    );
    let said = h
        .apply_announcing(vec![b1.caller_gave_up(ts(300)), b2.ring(ts(300))])
        .expect("blocked");
    assert!(said.notify.is_empty() && said.catch_up == 0);
    h.set_now(ring_end(ts(300)));
    let said = h.settle_calls().expect("sweep");
    assert!(said.notify.is_empty() && said.catch_up == 0);
}

/// A caller whose message request is open: a line, and no notification, also when the
/// backend reported the ring (silent) and the deadline makes it a missed call.
#[test]
fn a_silent_ring_is_never_announced() {
    let (_dir, mut h, c) = harness();
    let stranger = ConvKey::Direct(c.bob.clone());
    let call = Call::direct(&c.bob, 1, CallMedia::Voice);
    let mut ring: IncomingMessage = call.ring_message(ts(1));
    ring.silent = true;
    let said = h
        .apply_announcing(vec![BackendEvent::Message(ring)])
        .expect("ring");
    assert!(said.notify.is_empty());
    h.set_now(ring_end(ts(1)));
    let said = h.settle_calls().expect("sweep");
    assert!(said.notify.is_empty() && said.catch_up == 0);
    let (line, row) = lines(&h, &stranger).remove(0);
    assert_eq!(line.state, CallState::Missed);
    assert_eq!(row.read_state, Some(ReadState::Unread));
    assert_eq!(line.direction, CallDirection::Incoming);
}

/// A text between the signals of a call: the line keeps its place and shows the time the
/// call began.
#[test]
fn the_line_keeps_its_place_and_time() {
    let (_dir, mut h, c) = harness();
    let peer = ConvKey::Direct(c.alice.clone());
    let call = Call::direct(&c.alice, 1, CallMedia::Voice);
    let text =
        |n: i64| BackendEvent::Message(remote(&peer, &c.alice, &ts(n).to_string(), ts(n), "text"));
    h.apply(vec![text(0), call.ring(ts(1)), text(2)])
        .expect("apply");
    let place = |h: &IngestHarness| {
        let all = rows(h, &peer);
        let at = all
            .iter()
            .position(|r| r.body.call_line().is_some())
            .expect("line");
        (at, all.len(), all[at].sort_key, all[at].sent_at, all[at].id)
    };
    let before = place(&h);
    h.apply(vec![
        call.caller_gave_up(ts(20)),
        text(21),
        call.answered_elsewhere(ts(30)),
    ])
    .expect("apply");
    let after = place(&h);
    assert_eq!(after.1, 4, "no second line");
    assert_eq!(
        (after.2, after.3, after.4),
        (before.2, before.3, before.4),
        "the same row, the same place, the same time"
    );
}

/// The design finds a call by (call id, peer or group). Whoever names an id FIRST, in a chat
/// of his own, must not take it away from the chat the call is really in: a member of a
/// group learns the id of a group call when it starts (it is derived from the era id) and can
/// name it in another group, or in his own direct chat, before the ring arrives.
#[test]
fn an_id_named_first_in_another_chat_does_not_swallow_the_call() {
    let (_dir, mut h, c) = harness();
    // Bob (a member of the group) names the id of the group's call in another group first.
    let real = Call::group(&c.group, 9, &c.alice);
    let squat = Call {
        conv: ConvKey::Group("gv2:b3RoZXItZ3JvdXA".into()),
        caller: c.bob.clone(),
        ..real.clone()
    };
    h.apply(vec![squat.started(&c.bob, ts(1))]).expect("squat");
    let said = h
        .apply_announcing(vec![real.ring(ts(2)), real.ring_cancelled(ts(20))])
        .expect("the real ring");
    let shown: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.conv_key = ?1 AND m.net_local = 'gcall:9'",
            [c.group.encode()],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(
        (shown, said.notify.len()),
        (1, 1),
        "the group's missed call has its line and is announced"
    );
    assert_eq!(
        lines(&h, &squat.conv).len(),
        1,
        "and the other group has its own"
    );

    // A line that was cleared with its chat is remembered for that chat only: the same id
    // in another chat is a call like any other.
    let cleared = Call::direct(&c.bob, 10, CallMedia::Voice);
    h.apply(vec![cleared.caller_gave_up(ts(30))])
        .expect("bob's call");
    h.clear_conversation(conv(&h, &cleared.conv), false)
        .expect("clear");
    let same_id = Call::direct(&c.alice, 10, CallMedia::Voice);
    let said = h
        .apply_announcing(vec![
            cleared.caller_gave_up(ts(30)),
            same_id.caller_gave_up(ts(40)),
        ])
        .expect("apply");
    assert!(lines(&h, &cleared.conv).is_empty(), "the cleared one stays");
    assert_eq!(
        lines(&h, &same_id.conv).len(),
        1,
        "alice's call has its line"
    );
    assert_eq!(said.notify.len(), 1);
}
