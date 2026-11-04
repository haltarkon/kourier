//! The reconcile (TG-3 C9a, C9b; critic B1, H2, decisions G-5, G-8, Q-L1, Q-L8). SYNTHETIC
//! worlds (no recording holds a crash's lost tail, an unclean start, the page cap, a deleted
//! message's hole or a supergroup's difference during the start decision; L-14): those tests use
//! 0 recorded answers (Q-L11), their requests answered by `world` / `sg_world` from the synthetic
//! history they name. `a_channel_too_long_gap_is_fetched` replays `rec-channel-difference.jsonl`
//! and says how many recorded answers it used; its one reconcile page is synthetic (no recording
//! holds it: the fake world's history is MODEL). Nothing here prints a value: assertions name ids
//! and kinds.

use kourier_backend_api::BackendEvent;

use super::*;
use crate::store::{ChatKind, ChatRow, Delivered, GapCause, GapChange, GapRow, Seed};
use crate::sync::reconcile::QUIET_MS;

const CHAT: i64 = 77;

/// The seed of chat 77 (private, listed) with its cursor and continuity at `cursor` / `verified`
/// (server ids) and the cursor's message delivered.
fn seed_of(cursor: i64, verified: i64, gaps: Vec<GapRow>) -> Seed {
    Seed {
        chats: vec![ChatRow {
            chat_id: CHAT,
            kind: ChatKind::Direct,
            listed: true,
            held: None,
            delivered_max_id: cursor * STEP,
            verified_max_id: verified * STEP,
            low_id: STEP,
            read_inbox_max_id: cursor * STEP,
            read_outbox_max_id: 0,
            photo_small: None,
            photo_big: None,
            moved_to: None,
            updated_ms: NOW,
        }],
        delivered: vec![Delivered {
            chat_id: CHAT,
            msg_id: cursor * STEP,
            author: format!("tg:{CHAT}"),
            outgoing: false,
        }],
        gaps,
    }
}

/// A helper run over `seed` (`unclean`: the last one did not end cleanly) in which TDLib tells
/// chat 77 with its last message `td_last`, streams `live` during the start sync, and answers
/// both lists 404. What the machines gave, with the reconcile's first pages still to ask.
fn run(seed: &Seed, unclean: bool, td_last: i64, live: &[i64]) -> (Receiver, Out) {
    let mut recv = Receiver::new(Some(ME), NOW, unclean);
    recv.seed(seed);
    let mut out = recv.chats.link(ME);
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": CHAT,
        "type": {"@type": "chatTypePrivate", "user_id": CHAT}, "title": "Fay Sample",
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": "100", "is_pinned": false}],
        "last_message": message(CHAT, td_last, false), "unread_count": 0,
        "last_read_inbox_message_id": seed.chats[0].read_inbox_max_id,
        "last_read_outbox_message_id": 0}});
    for v in [user(CHAT), chat] {
        feed(&mut recv, &v, &mut out);
    }
    out.extend(recv.begin_sync());
    for &id in live {
        feed(&mut recv, &new_message(id), &mut out);
    }
    let mut queue: VecDeque<Fetch> = out.fetch.drain(..).collect();
    while let Some(f) = queue.pop_front() {
        if !matches!(f, Fetch::LoadChats { .. }) {
            out.fetch.push(f);
            continue;
        }
        let mut o = recv.on_fetched(f, Err(LinkError::Td { code: 404 }), NOW);
        queue.extend(o.fetch.drain(..));
        out.extend(o);
    }
    assert!(
        recv.sync_done(),
        "the start sync waits for the reconcile's pages"
    );
    (recv, out)
}

fn new_message(id: i64) -> Value {
    json!({"@type": "updateNewMessage", "message": message(CHAT, id, false)})
}

/// TDLib's pages of a synthetic history 1..=`newest`: the messages below `from`, newest first, at
/// most the limit. `extra(n)` adds updates before the n-th answer (0-based).
fn world(
    newest: i64,
    mut extra: impl FnMut(usize) -> Vec<Value>,
) -> impl FnMut(&Value) -> (Vec<Value>, Result<Value, LinkError>) {
    let mut n = 0;
    move |req| {
        assert_eq!(req["@type"], "getChatHistory", "only history pages");
        let from = req["from_message_id"].as_i64().expect("from") / STEP;
        let limit = req["limit"].as_i64().expect("limit");
        let msgs: Vec<Value> = (1..=limit)
            .map(|k| from - k)
            .filter(|id| *id > 0 && *id <= newest)
            .map(|id| message(CHAT, id, false))
            .collect();
        let updates = extra(n);
        n += 1;
        (
            updates,
            Ok(json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs})),
        )
    }
}

/// Every live `Message` row of chat 77: (server id, catch_up), in row order.
fn live(out: &Out) -> Vec<(i64, bool)> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::Message(m) => {
                Some((m.local.parse::<i64>().expect("an id") / STEP, m.catch_up))
            }
            _ => None,
        })
        .collect()
}

/// Every HISTORY row's message of chat 77 (Q-D1: a page's message a live row overtook), in
/// server ids, sorted.
fn history(out: &Out) -> Vec<i64> {
    history_of(out, CHAT)
}

/// The `HistoryBatch` messages of conversation `tg:<chat>` in server ids, sorted; each batch
/// never notified (`catch_up` false is the history path's).
fn history_of(out: &Out, chat: i64) -> Vec<i64> {
    let mut ids: Vec<i64> = out
        .staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::HistoryBatch { conv, messages, .. }
                if conv_name(conv) == format!("tg:{chat}") =>
            {
                Some(messages.iter().map(|m| {
                    assert!(!m.catch_up, "a history row marked catch-up");
                    m.local.parse::<i64>().expect("an id") / STEP
                }))
            }
            _ => None,
        })
        .flatten()
        .collect();
    let n = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), n, "a history row twice");
    ids
}

/// The gap changes in server ids, as `(kind, above, below)` (`below` 0 where it has none).
fn gaps(out: &Out) -> Vec<(&'static str, i64, i64)> {
    out.staged
        .gaps
        .iter()
        .map(|g| match *g {
            GapChange::Open {
                above,
                below,
                cause,
                ..
            } => (
                match cause {
                    GapCause::Start => "open-start",
                    GapCause::UncleanEnd => "open-unclean",
                    GapCause::IdJump => "open-id-jump",
                    GapCause::Snapshot => "open-snapshot",
                },
                above / STEP,
                below / STEP,
            ),
            GapChange::Paged { above, below, .. } => ("paged", above / STEP, below / STEP),
            GapChange::Close { above, .. } => ("close", above / STEP, 0),
            GapChange::Lost { above, .. } => ("lost", above / STEP, 0),
        })
        .collect()
}

fn no_notice(out: &Out) {
    assert!(
        !out.staged
            .rows
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e.event, BackendEvent::Notice(_))),
        "a notice was emitted (G-5: none for Telegram)"
    );
}

/// B1's first case: Kourier crashed with the cursor at 100 while TDLib had already processed up
/// to 150 (its pts saved): the start decision sees `td_last` 150 above the cursor, writes the gap
/// (100, 150) and pages it from `td_last` down to the cursor; 150 arrives as a live catch-up row,
/// 101..149 (overtaken by it) once each as history rows (Q-D1), and the gap closes.
#[test]
fn a_tail_lost_in_a_crash_is_fetched_from_td_last() {
    let (mut recv, first) = run(&seed_of(100, 100, Vec::new()), true, 150, &[]);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = drive(&mut recv, &mut tape, first, world(150, |_| Vec::new()));
    assert_eq!(asked(&tape), ["getChatHistory(77, 150, 50)"]);
    assert_eq!(
        tape.used, 0,
        "recorded answers used (Q-L11): none, a synthetic world"
    );
    assert_eq!(gaps(&out), [("open-start", 100, 150), ("close", 100, 0)]);
    assert_eq!(
        live(&out),
        [(150, true)],
        "td_last as one live catch-up row"
    );
    assert_eq!(
        history(&out),
        (101..150).collect::<Vec<_>>(),
        "each lost message once, as history (Q-D1)"
    );
    assert_eq!(recv.messages.cursor(CHAT).delivered_max, 150 * STEP);
    no_notice(&out);
}

/// B1's masking case: after an unclean end the difference streams 152 before the chat list is
/// loaded, so `td_last` equals the moved cursor; the first live message's CHECK GAP (100, 152) in
/// its own row's batch still brings 101..151 (two pages) as history rows (152 overtook them,
/// Q-D1), the gap closes.
#[test]
fn a_live_message_after_an_unclean_start_does_not_mask_the_lost_tail() {
    let (mut recv, first) = run(&seed_of(100, 100, Vec::new()), true, 152, &[152]);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = drive(&mut recv, &mut tape, first, world(152, |_| Vec::new()));
    assert_eq!(
        asked(&tape),
        ["getChatHistory(77, 152, 50)", "getChatHistory(77, 102, 50)"]
    );
    assert_eq!(tape.used, 0, "recorded answers used (Q-L11)");
    assert_eq!(
        gaps(&out),
        [
            ("open-unclean", 100, 152),
            ("paged", 100, 102),
            ("close", 100, 0)
        ]
    );
    assert_eq!(live(&out), [(152, true)], "the live message, once");
    assert_eq!(
        history(&out),
        (101..152).collect::<Vec<_>>(),
        "each lost message once, as history"
    );
}

/// B1's second variant: a live 200 while the reconcile of (100, 150) is under way moves the
/// cursor; the page still compares against the gap row's range, so 101..149 are not dropped (they
/// come as history rows: `td_last` and 200 overtook them, Q-D1).
#[test]
fn a_reconcile_works_on_its_snapshot_range() {
    let (mut recv, first) = run(&seed_of(100, 100, Vec::new()), false, 150, &[]);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = drive(
        &mut recv,
        &mut tape,
        first,
        world(150, |n| {
            if n == 0 {
                vec![new_message(200)]
            } else {
                Vec::new()
            }
        }),
    );
    assert_eq!(asked(&tape), ["getChatHistory(77, 150, 50)"]);
    assert_eq!(tape.used, 0, "recorded answers used (Q-L11)");
    let ids: Vec<i64> = live(&out).iter().map(|r| r.0).collect();
    assert_eq!(ids, [150, 200], "td_last and the live message");
    assert_eq!(
        history(&out),
        (101..150).collect::<Vec<_>>(),
        "the snapshot range was read live"
    );
    assert_eq!(gaps(&out).last(), Some(&("close", 100, 0)));
}

/// The cap (G-5, Q-L1): three pages at most per gap; a gap still open after them stays `lost`
/// for KT-3 and NOTHING is said. The store keeps it lost with its page count, and the next run
/// does not page a lost gap.
#[test]
fn the_cap_keeps_the_gap_lost_and_says_nothing() {
    let (mut recv, first) = run(&seed_of(100, 100, Vec::new()), false, 1000, &[]);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = drive(&mut recv, &mut tape, first, world(1000, |_| Vec::new()));
    assert_eq!(
        asked(&tape),
        [
            "getChatHistory(77, 1000, 50)",
            "getChatHistory(77, 950, 50)",
            "getChatHistory(77, 900, 50)",
        ]
    );
    assert_eq!(tape.used, 0, "recorded answers used (Q-L11)");
    assert_eq!(
        gaps(&out),
        [
            ("open-start", 100, 1000),
            ("paged", 100, 950),
            ("paged", 100, 900),
            ("paged", 100, 850),
            ("lost", 100, 0),
        ]
    );
    assert_eq!(live(&out), [(1000, true)], "td_last, live");
    assert_eq!(history(&out).len(), 3 * 50, "three pages, as history");
    no_notice(&out);

    let dir = tempfile::tempdir().expect("tempdir");
    let conn = crate::db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from_bytes([7; 32]),
    )
    .expect("open");
    crate::store::commit_rows(&conn, &out.staged, NOW).expect("commit");
    let rows = crate::store::gap_rows(&conn).expect("gaps");
    assert_eq!(rows.len(), 1);
    assert!(rows[0].lost, "the gap is not lost");
    assert_eq!((rows[0].pages, rows[0].below), (3, 850 * STEP));

    let seed = crate::store::seed(&conn).expect("seed");
    let (mut recv, first) = run(&seed, false, 1000, &[]);
    let mut tape = Tape::of(std::iter::empty()).0;
    drive(&mut recv, &mut tape, first, world(1000, |_| Vec::new()));
    assert!(asked(&tape).is_empty(), "a lost gap was paged again");
}

/// A gap the store holds from an earlier run is paged on at the start, and its page count goes
/// on from the stored one (two pages read before: one more reaches the cap).
#[test]
fn a_stored_gap_is_paged_on_with_its_page_count() {
    let stored = GapRow {
        chat_id: CHAT,
        above: 100 * STEP,
        below: 300 * STEP,
        cause: GapCause::UncleanEnd,
        lost: false,
        pages: 2,
    };
    let (mut recv, first) = run(&seed_of(300, 300, vec![stored]), false, 300, &[]);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = drive(&mut recv, &mut tape, first, world(300, |_| Vec::new()));
    assert_eq!(asked(&tape), ["getChatHistory(77, 300, 50)"]);
    assert_eq!(gaps(&out), [("paged", 100, 250), ("lost", 100, 0)]);
    assert!(live(&out).is_empty(), "a row below the cursor came live");
    assert_eq!(history(&out), (250..300).collect::<Vec<_>>());
}

/// After a CLEAN end the first live message opens no check gap (the stream is continuous), and
/// a chat whose `td_last` is the cursor asks nothing.
#[test]
fn after_a_clean_end_no_check_gap_opens() {
    let (mut recv, first) = run(&seed_of(100, 100, Vec::new()), false, 152, &[152]);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = drive(&mut recv, &mut tape, first, world(152, |_| Vec::new()));
    assert!(asked(&tape).is_empty(), "asked: {:?}", asked(&tape));
    assert!(gaps(&out).is_empty(), "gaps: {:?}", gaps(&out));
    assert_eq!(
        live(&out),
        [(152, true)],
        "the live message, catch-up in the start sync"
    );
    assert_eq!(recv.sync.reconcile.open(), 0);
}

// ---- C9b: supergroups --------------------------------------------------------------------------

const SG401: i64 = -1_000_000_000_401;
const SG402: i64 = -1_000_000_000_402;

/// `rec-channel-difference.jsonl`'s supergroup 402 has an active username (public, held until
/// T7, H5): the replay drops that username in memory, every id, date and order as recorded (as
/// C6a's trigger test does).
fn unhold_402(v: &Value) -> Value {
    let mut v = v.clone();
    if v["@type"] == "updateSupergroup" && v["supergroup"]["id"] == 402 {
        v["supergroup"]["usernames"] = Value::Null;
    }
    v
}

fn unhold_part(part: &crate::replay::Part) -> crate::replay::Part {
    use crate::replay::Item;
    crate::replay::Part {
        step: part.step.clone(),
        items: part
            .items
            .iter()
            .map(|i| match i {
                Item::Update(v) => Item::Update(unhold_402(v)),
                Item::Request(r) => {
                    let mut r = r.clone();
                    r.reply = r.reply.iter().map(unhold_402).collect();
                    Item::Request(r)
                }
            })
            .collect(),
    }
}

/// The gap changes of one chat, as [`gaps`] names them.
fn gaps_of(out: &Out, chat: i64) -> Vec<(&'static str, i64, i64)> {
    let only = Out {
        staged: crate::store::Staged {
            gaps: out
                .staged
                .gaps
                .iter()
                .filter(|g| {
                    let id = match **g {
                        GapChange::Open { chat_id, .. }
                        | GapChange::Paged { chat_id, .. }
                        | GapChange::Close { chat_id, .. }
                        | GapChange::Lost { chat_id, .. } => chat_id,
                    };
                    id == chat
                })
                .copied()
                .collect(),
            ..crate::store::Staged::default()
        },
        ..Out::default()
    };
    gaps(&only)
}

/// The live `Message` rows of one conversation `tg:<chat>` as (server id, catch_up).
fn live_of(out: &Out, chat: i64) -> Vec<(i64, bool)> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::Message(m) if conv_name(&m.conv) == format!("tg:{chat}") => {
                Some((m.local.parse::<i64>().expect("an id") / STEP, m.catch_up))
            }
            _ => None,
        })
        .collect()
}

/// G-8 (the brief's `a_silent_dialog_reload_is_caught_after_updating_ready`, re-cut): the fresh
/// link of `rec-channel-difference.jsonl` leaves 402's cursor at 1; after
/// `channelDifferenceTooLong` TDLib streams only the newest message, 6 (step `too-long-402`),
/// whose id jump opens the gap (1, 6); the reconcile asks ONE page from 6 and the missing 2..5
/// arrive once each as history rows (6 overtook them, Q-D1), the gap closes. 401's in-order difference (step `too-long-401`)
/// asks nothing and opens no gap.
#[test]
fn a_channel_too_long_gap_is_fetched() {
    let rec = Recording::load("rec-channel-difference.jsonl");
    let first_part = unhold_part(&rec.parts[0]);
    let (mut tape, loose) = Tape::of([&first_part]);
    let (mut recv, first) = begun(&Seed::default(), &loose);
    let fresh = drive(&mut recv, &mut tape, first, no_fallback);
    assert!(recv.sync_done(), "the start sync did not end");
    assert!(
        recv.chats.conv(SG402).is_some(),
        "402 is mapped once unheld"
    );
    recv.messages.start_sync_done();
    let loaded = tape.asked.len();
    assert_eq!(recv.messages.cursor(SG402).delivered_max, STEP);
    assert_eq!(recv.messages.cursor(SG401).delivered_max, STEP);

    // The two too-long steps as TDLib streamed them (no request of their own).
    let mut streamed = Out::default();
    for label in ["too-long-401", "too-long-402"] {
        let part = unhold_part(rec.part(label));
        assert_eq!(part.requests().count(), 0, "a step holds no request");
        for u in part.updates() {
            feed(&mut recv, u, &mut streamed);
        }
    }
    // The newest message of 402 as recorded, the page's messages made from it (SYNTHETIC: the
    // fake world's history is MODEL; 1 is the continuity point below the gap).
    let newest = rec
        .part("too-long-402")
        .updates()
        .find(|u| u["@type"] == "updateNewMessage")
        .map(|u| u["message"].clone())
        .expect("the newest message");
    let page = move |req: &Value| {
        assert_eq!(req["@type"], "getChatHistory");
        assert_eq!(req["chat_id"], SG402, "a page of another chat");
        let from = req["from_message_id"].as_i64().expect("from") / STEP;
        let msgs: Vec<Value> = (1..from)
            .rev()
            .map(|id| {
                let mut m = newest.clone();
                m["id"] = json!(id * STEP);
                m["date"] = json!(newest["date"].as_i64().unwrap_or(0) - (6 - id));
                m["content"]["text"]["text"] = json!(format!("synthetic text {}", 960 + id));
                m
            })
            .collect();
        (
            Vec::new(),
            Ok(json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs})),
        )
    };
    assert_eq!(history_of(&fresh, SG402), [1], "the fresh link's load");
    // What the two steps and the reconcile gave.
    let out = drive(&mut recv, &mut tape, streamed, page);
    assert_eq!(
        asked(&tape)[loaded..],
        [format!("getChatHistory({SG402}, 6, 50)")],
        "one reconcile page"
    );
    assert_eq!(
        tape.used, 7,
        "recorded answers used (Q-L11): 6 loadChats + 201's page (the reconcile page: synthetic)"
    );
    assert_eq!(
        gaps_of(&out, SG402),
        [("open-id-jump", 1, 6), ("close", 1, 0)]
    );
    assert!(
        gaps_of(&out, SG401).is_empty(),
        "401's in-order stream opened a gap"
    );
    let ids: Vec<i64> = live_of(&out, SG402).iter().map(|r| r.0).collect();
    assert_eq!(ids, [6], "the streamed newest message, live");
    assert_eq!(
        history_of(&out, SG402),
        [2, 3, 4, 5],
        "each missing message once, as history"
    );
    assert_eq!(recv.messages.cursor(SG402).delivered_max, 6 * STEP);
    assert_eq!(recv.sync.reconcile.open(), 0);
}

/// A synthetic private supergroup (id 777, no username) of the run helpers.
const SG: i64 = -1_000_000_000_777;

fn sg_message(id: i64) -> Value {
    json!({"@type": "message", "id": id * STEP, "chat_id": SG, "is_outgoing": false,
        "sender_id": {"@type": "messageSenderUser", "user_id": CHAT},
        "date": 1_700_000_100 + id, "edit_date": 0, "media_album_id": "0",
        "content": {"@type": "messageText", "text": {"@type": "formattedText",
            "text": "synthetic text 953", "entities": []}}})
}

fn sg_new(id: i64) -> Value {
    json!({"@type": "updateNewMessage", "message": sg_message(id)})
}

/// A helper run over a seed of supergroup 777 with its cursor at `cursor` (a clean end before),
/// TDLib telling it with its last message `td_last`; both lists answer 404. What the machines
/// gave (the decision taken or waiting).
fn sg_run(cursor: i64, td_last: i64) -> (Receiver, Out) {
    let seed = Seed {
        chats: vec![ChatRow {
            chat_id: SG,
            kind: ChatKind::Supergroup,
            listed: true,
            held: None,
            delivered_max_id: cursor * STEP,
            verified_max_id: cursor * STEP,
            low_id: STEP,
            read_inbox_max_id: cursor * STEP,
            read_outbox_max_id: 0,
            photo_small: None,
            photo_big: None,
            moved_to: None,
            updated_ms: NOW,
        }],
        delivered: vec![Delivered {
            chat_id: SG,
            msg_id: cursor * STEP,
            author: format!("tg:{CHAT}"),
            outgoing: false,
        }],
        gaps: Vec::new(),
    };
    let mut recv = Receiver::new(Some(ME), NOW, false);
    recv.seed(&seed);
    let mut out = recv.chats.link(ME);
    let group = json!({"@type": "updateSupergroup", "supergroup": {"@type": "supergroup",
        "id": 777, "usernames": null, "date": 1_700_000_000, "member_count": 3,
        "is_channel": false, "is_broadcast_group": false, "is_forum": false,
        "is_direct_messages_group": false}});
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": SG,
        "type": {"@type": "chatTypeSupergroup", "supergroup_id": 777, "is_channel": false},
        "title": "Group 9", "positions": [{"@type": "chatPosition",
            "list": {"@type": "chatListMain"}, "order": "100", "is_pinned": false}],
        "last_message": sg_message(td_last), "unread_count": 0,
        "last_read_inbox_message_id": cursor * STEP, "last_read_outbox_message_id": 0}});
    for v in [user(CHAT), group, chat] {
        feed(&mut recv, &v, &mut out);
    }
    out.extend(recv.begin_sync());
    let mut queue: VecDeque<Fetch> = out.fetch.drain(..).collect();
    while let Some(f) = queue.pop_front() {
        assert!(
            matches!(f, Fetch::LoadChats { .. }),
            "asked {f:?} before the lists"
        );
        let mut o = recv.on_fetched(f, Err(LinkError::Td { code: 404 }), NOW);
        queue.extend(o.fetch.drain(..));
        out.extend(o);
    }
    assert!(recv.sync_done());
    assert!(
        recv.chats.conv(SG).is_some(),
        "the supergroup is not mapped"
    );
    recv.messages.start_sync_done();
    (recv, out)
}

/// TDLib's pages of supergroup 777's synthetic history 1..=`newest` without the ids `deleted`.
fn sg_world(
    newest: i64,
    deleted: &'static [i64],
) -> impl FnMut(&Value) -> (Vec<Value>, Result<Value, LinkError>) {
    move |req| {
        assert_eq!(req["@type"], "getChatHistory", "only history pages");
        assert_eq!(req["chat_id"], SG);
        let from = req["from_message_id"].as_i64().expect("from") / STEP;
        let limit = usize::try_from(req["limit"].as_i64().expect("limit")).expect("limit");
        let msgs: Vec<Value> = (1..from)
            .rev()
            .filter(|id| *id <= newest && !deleted.contains(id))
            .take(limit)
            .map(sg_message)
            .collect();
        (
            Vec::new(),
            Ok(json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs})),
        )
    }
}

/// G-8: a message deleted on Telegram leaves a hole in a supergroup's ids: the next message
/// (12 after the cursor 10, 11 deleted) opens the id-jump gap (10, 12), and the reconcile's ONE
/// page finds nothing inside it and closes it (no page more, no retry to the cap).
#[test]
fn a_hole_left_by_a_delete_costs_one_empty_request() {
    let (mut recv, first) = sg_run(10, 10);
    let mut tape = Tape::of(std::iter::empty()).0;
    let mut out = drive(&mut recv, &mut tape, first, sg_world(12, &[11]));
    out.extend(recv.on_tick(NOW + QUIET_MS));
    assert!(
        asked(&tape).is_empty(),
        "td_last is the cursor: nothing to ask"
    );
    let mut o = Out::default();
    feed(&mut recv, &sg_new(12), &mut o);
    out.extend(drive(&mut recv, &mut tape, o, sg_world(12, &[11])));
    assert_eq!(asked(&tape), [format!("getChatHistory({SG}, 12, 50)")]);
    assert_eq!(
        tape.used, 0,
        "recorded answers used (Q-L11): none, a synthetic world"
    );
    assert_eq!(gaps(&out), [("open-id-jump", 10, 12), ("close", 10, 0)]);
    assert_eq!(live_of(&out, SG), [(12, false)]);
    assert_eq!(recv.sync.reconcile.open(), 0);
}

/// H2, Q-L8: a supergroup's start decision waits `QUIET_MS` for its own stream. TDLib's channel
/// difference streams 11..15 meanwhile (the cursor reaches `td_last`): no history request, no
/// gap. A supergroup whose stream stays silent is decided when the wait is over: its gap
/// (10, 15) and its first page then.
#[test]
fn no_history_request_while_a_channel_difference_runs() {
    let (mut recv, first) = sg_run(10, 15);
    assert!(
        first.fetch.is_empty() && first.staged.gaps.is_empty(),
        "decided before the stream was quiet"
    );
    assert_eq!(recv.due_ms(), Some(NOW + QUIET_MS));
    let mut out = first;
    for id in 11..=15 {
        feed(&mut recv, &sg_new(id), &mut out);
    }
    assert!(out.fetch.is_empty(), "asked during the difference");
    out.extend(recv.on_tick(NOW + QUIET_MS - 1));
    assert_eq!(recv.due_ms(), Some(NOW + QUIET_MS), "decided early");
    out.extend(recv.on_tick(NOW + QUIET_MS));
    assert!(
        out.fetch.is_empty(),
        "a history request after the difference"
    );
    assert!(gaps(&out).is_empty(), "gaps: {:?}", gaps(&out));
    assert_eq!(recv.due_ms(), None);
    let ids: Vec<i64> = live_of(&out, SG).iter().map(|r| r.0).collect();
    assert_eq!(ids, [11, 12, 13, 14, 15]);

    // Silent: decided at the end of the wait.
    let (mut recv, first) = sg_run(10, 15);
    assert!(first.fetch.is_empty());
    let mut tape = Tape::of(std::iter::empty()).0;
    let o = recv.on_tick(NOW + QUIET_MS);
    let out = drive(&mut recv, &mut tape, o, sg_world(15, &[]));
    assert_eq!(asked(&tape), [format!("getChatHistory({SG}, 15, 50)")]);
    assert_eq!(tape.used, 0, "recorded answers used (Q-L11)");
    assert_eq!(gaps(&out), [("open-start", 10, 15), ("close", 10, 0)]);
    assert_eq!(live_of(&out, SG), [(15, true)], "td_last, live catch-up");
    assert_eq!(
        history_of(&out, SG),
        [11, 12, 13, 14],
        "the page, as history"
    );
}

/// TF2 (review dF2, Q-D1 on every path): the supergroup is decided early (`QUIET_MS` over, TDLib's
/// last message 300, the machines' cursor 160 because the backlog streamed 101..160): 300 is the
/// live catch-up row and the gap (160, 300) is open. The backlog's rest streams in AFTER that
/// (161..165): those messages are at or below the delivered max, so they are HISTORY rows (sorted
/// by their date, never notified), not live rows that would show behind 300. (Mutant: the branch
/// dropped.)
#[test]
fn a_backlog_streaming_after_an_early_decision_keeps_its_order() {
    let (mut recv, first) = sg_run(100, 300);
    assert!(first.fetch.is_empty(), "decided before the quiet wait");
    let mut out = first;
    for id in 101..=160 {
        feed(&mut recv, &sg_new(id), &mut out);
    }
    assert_eq!(live_of(&out, SG).len(), 60, "the backlog so far, live");
    let decided = recv.on_tick(NOW + QUIET_MS);
    assert_eq!(live_of(&decided, SG), [(300, true)], "td_last, live");
    assert_eq!(gaps(&decided), [("open-start", 160, 300)]);
    let mut late = Out::default();
    for id in 161..=165 {
        feed(&mut recv, &sg_new(id), &mut late);
    }
    assert_eq!(live_of(&late, SG), [], "a live row behind 300");
    assert_eq!(history_of(&late, SG), [161, 162, 163, 164, 165]);
}

/// TF11 (review pF2): an open gap the store holds for a chat that is HELD now (its supergroup got
/// a public username after the rows came) is not paged: no `getChatHistory` for a held chat
/// (H5, Q-B3). It stays open for T7's lift, not marked lost. (Mutant: the guard removed.)
#[test]
fn a_stored_gap_of_a_held_chat_is_not_paged() {
    let seed = Seed {
        chats: vec![ChatRow {
            chat_id: SG,
            kind: ChatKind::Supergroup,
            listed: true,
            held: None,
            delivered_max_id: 300 * STEP,
            verified_max_id: 300 * STEP,
            low_id: STEP,
            read_inbox_max_id: 300 * STEP,
            read_outbox_max_id: 0,
            photo_small: None,
            photo_big: None,
            moved_to: None,
            updated_ms: NOW,
        }],
        delivered: vec![Delivered {
            chat_id: SG,
            msg_id: 300 * STEP,
            author: format!("tg:{CHAT}"),
            outgoing: false,
        }],
        gaps: vec![GapRow {
            chat_id: SG,
            above: 100 * STEP,
            below: 300 * STEP,
            cause: GapCause::IdJump,
            lost: false,
            pages: 0,
        }],
    };
    let mut recv = Receiver::new(Some(ME), NOW, false);
    recv.seed(&seed);
    let mut out = recv.chats.link(ME);
    let group = json!({"@type": "updateSupergroup", "supergroup": {"@type": "supergroup",
        "id": 777, "usernames": {"@type": "usernames", "active_usernames": ["synthetic_9"]},
        "is_channel": false}});
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": SG,
        "type": {"@type": "chatTypeSupergroup", "supergroup_id": 777, "is_channel": false},
        "title": "Group 9", "positions": [{"@type": "chatPosition",
            "list": {"@type": "chatListMain"}, "order": "100", "is_pinned": false}],
        "last_message": sg_message(300), "unread_count": 0,
        "last_read_inbox_message_id": 300 * STEP, "last_read_outbox_message_id": 0}});
    for v in [user(CHAT), group, chat] {
        feed(&mut recv, &v, &mut out);
    }
    out.extend(recv.begin_sync());
    let mut queue: VecDeque<Fetch> = out.fetch.drain(..).collect();
    while let Some(f) = queue.pop_front() {
        assert!(
            matches!(f, Fetch::LoadChats { .. }),
            "asked {f:?} for a held chat"
        );
        let mut o = recv.on_fetched(f, Err(LinkError::Td { code: 404 }), NOW);
        queue.extend(o.fetch.drain(..));
        out.extend(o);
    }
    assert!(recv.chats.is_held(SG), "the chat is not held");
    assert!(gaps(&out).is_empty(), "the gap changed: {:?}", gaps(&out));
    assert_eq!(recv.sync.reconcile.open(), 1, "the gap was dropped");
}

/// TF11 (review pF2, the next-page path): a reconcile page answered after its chat became HELD
/// (a public username came meanwhile) delivers nothing and asks no next page; the gap stays open
/// for T7's lift. (Mutant: the guard in `on_page` removed.)
#[test]
fn a_page_answered_for_a_chat_held_meanwhile_asks_no_next_page() {
    let (mut recv, _first) = sg_run(10, 10);
    let mut out = Out::default();
    feed(&mut recv, &sg_new(40), &mut out);
    let page = out
        .fetch
        .iter()
        .copied()
        .find(|f| matches!(f, Fetch::Gap { .. }))
        .expect("the id-jump gap's first page");
    let group = json!({"@type": "updateSupergroup", "supergroup": {"@type": "supergroup",
        "id": 777, "usernames": {"@type": "usernames", "active_usernames": ["synthetic_9"]},
        "is_channel": false}});
    let mut held = Out::default();
    feed(&mut recv, &group, &mut held);
    assert!(recv.chats.is_held(SG), "the chat is not held");
    let msgs: Vec<Value> = (30..40).rev().map(sg_message).collect();
    let answer = json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs});
    let after = recv.on_fetched(page, Ok(answer), NOW);
    assert!(after.fetch.is_empty(), "a next page for a held chat");
    assert!(
        after.staged.rows.is_empty() && after.staged.gaps.is_empty(),
        "a held chat's page was staged"
    );
    assert_eq!(recv.sync.reconcile.open(), 1, "the gap was dropped");
}

/// TF10 (review dF10): a gap the store holds as LOST is never opened again in memory either: an
/// `Open` of its key is not tracked and asks no page. (Mutant: lost gaps forgotten.)
#[test]
fn a_lost_gap_is_not_tracked_again() {
    let lost = GapRow {
        chat_id: CHAT,
        above: 100 * STEP,
        below: 300 * STEP,
        cause: GapCause::UncleanEnd,
        lost: true,
        pages: 3,
    };
    let (mut recv, _) = run(&seed_of(300, 300, vec![lost]), false, 300, &[]);
    assert_eq!(
        recv.sync.reconcile.open(),
        0,
        "a lost gap is open in memory"
    );
    let mut out = Out::default();
    out.staged.gaps.push(GapChange::Open {
        chat_id: CHAT,
        above: 100 * STEP,
        below: 400 * STEP,
        cause: GapCause::IdJump,
    });
    recv.sync.reconcile.track(&mut out);
    assert_eq!(recv.sync.reconcile.open(), 0, "the lost gap was reopened");
    assert!(out.fetch.is_empty(), "a page was asked for a lost gap");
}

// ---- TG-4 K3b: the recorded restart cases (rc12a, rc12b, rc14) ----------------------------------
//
// The restart runs of three recordings replayed into the machines: the chats TDLib tells, the
// first live message `M`, the pages TDLib answered for the product's own reconcile request
// (`replay::key` = chat + `from_message_id`; the limit is not part of the key). The store's seed
// is what the first run committed (every listed chat at its pre-stop top, the recording's `s`).
// Every test asserts how many recorded answers it used (Q-L11, exact). The archive list's `404`
// is synthetic (the recordings load the main list only) and is not counted. The pages are what
// TDLib answered: the recordings' GOLDEN facts (K3a, K3c) are asserted by the recording cases;
// these tests assert what the RECEIVE CODE does with them.

const U201: i64 = 201;
const G301: i64 = -301;
const U202: i64 = 202;

/// A chat as the first run's last commit left it: (chat, kind, cursor, read position), server
/// ids. TDLib's chats of the recordings' world before the restart (`World::standard`).
type Stored = (i64, ChatKind, i64, i64);

fn world_before_the_restart() -> Vec<Stored> {
    vec![
        (U201, ChatKind::Direct, 4, 1),
        (G301, ChatKind::Group, 8, 8),
        (SG401, ChatKind::Supergroup, 1, 0),
        (U202, ChatKind::Direct, 5, 5),
    ]
}

/// The store's seed for `chats`: the cursor and the continuity point at the stale `s`, the
/// cursor's message delivered.
fn stored_seed(chats: &[Stored]) -> Seed {
    Seed {
        chats: chats
            .iter()
            .map(|&(chat_id, kind, cursor, read_in)| ChatRow {
                chat_id,
                kind,
                listed: true,
                held: None,
                delivered_max_id: cursor * STEP,
                verified_max_id: cursor * STEP,
                low_id: STEP,
                read_inbox_max_id: read_in * STEP,
                read_outbox_max_id: 0,
                photo_small: None,
                photo_big: None,
                moved_to: None,
                updated_ms: NOW,
            })
            .collect(),
        delivered: chats
            .iter()
            .map(|&(chat_id, _, cursor, _)| Delivered {
                chat_id,
                msg_id: cursor * STEP,
                author: format!("tg:{}", chat_id.abs()),
                outgoing: false,
            })
            .collect(),
        gaps: Vec::new(),
    }
}

/// The archive list's end: no recording holds one (TD-4a loads the main list only), so TDLib's
/// `404` here is SYNTHETIC and not counted in `used`.
fn archive_404(req: &Value) -> (Vec<Value>, Result<Value, LinkError>) {
    assert_eq!(
        req["@type"], "loadChats",
        "a request the recording does not answer"
    );
    assert_eq!(
        req["chat_list"]["@type"], "chatListArchive",
        "the main list's answers are recorded"
    );
    (Vec::new(), Err(LinkError::Td { code: 404 }))
}

/// The `from` ids (server ids) of the `getChatHistory` requests asked for `chat`, in order.
fn pages_asked(tape: &Tape, chat: i64) -> Vec<i64> {
    tape.asked
        .iter()
        .filter(|r| r["@type"] == "getChatHistory" && r["chat_id"] == chat)
        .map(|r| r["from_message_id"].as_i64().expect("from") / STEP)
        .collect()
}

/// The ids (server ids, newest first as recorded) of the recorded answer to
/// `getChatHistory(chat, from)` in the step `part`.
fn recorded_page(rec: &Recording, part: &str, chat: i64, from: i64) -> Vec<i64> {
    let r = rec
        .part(part)
        .requests()
        .find(|r| {
            r.ty == "getChatHistory"
                && r.args["chat_id"] == chat
                && r.args["from_message_id"] == from * STEP
        })
        .expect("the recorded page");
    r.answer()["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["id"].as_i64().expect("an id") / STEP)
        .collect()
}

/// Where the recording's `first-live` messages reach the machines in a replay of
/// `rec-first-live-after-load.jsonl`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LiveAt {
    /// While the archive list's `loadChats` is still unanswered (the lists load at a pace of one
    /// request per second, so `M` can come then; critic M2). B1 opens `(s, M)` with the
    /// continuity point at `s`, and the start decision later finds `td_last` at the cursor.
    BeforeArchive,
    /// After both lists and before the supergroups' quiet wait ends (the recording's order).
    AfterLists,
}

/// The restart run of `rec-first-live-after-load.jsonl` (the main list, the first live messages
/// `M` of the private chat 201 and the supergroup 401 at `at`, the pages the machines ask), after
/// an UNCLEAN end, the supergroups' quiet wait over at the end.
fn after_load_run(at: LiveAt) -> (Receiver, Tape, Out) {
    let rec = Recording::load("rec-first-live-after-load.jsonl");
    let (mut tape, _housekeeping) = Tape::of([rec.part("chats"), rec.part("pages")]);
    let mut recv = Receiver::new(Some(ME), NOW, true);
    recv.seed(&stored_seed(&world_before_the_restart()));
    let mut out = recv.chats.link(ME);
    out.extend(recv.begin_sync());
    // The main list from the recording, one request after the other; the archive's is held.
    let mut held = None;
    let mut queue: VecDeque<Fetch> = out.fetch.drain(..).collect();
    while let Some(f) = queue.pop_front() {
        if f == (Fetch::LoadChats { archive: true }) {
            held = Some(f);
            break;
        }
        let (updates, answer) = tape
            .answer(&f.request())
            .expect("the main list is recorded");
        for u in &updates {
            feed(&mut recv, u, &mut out);
        }
        let mut o = recv.on_fetched(f, answer, NOW);
        queue.extend(o.fetch.drain(..));
        out.extend(o);
    }
    let live = |recv: &mut Receiver, out: &mut Out| {
        for u in rec.part("first-live").updates() {
            feed(recv, u, out);
        }
    };
    if at == LiveAt::BeforeArchive {
        live(&mut recv, &mut out);
    }
    out.fetch
        .push(held.expect("the archive load follows the main list"));
    let mut all = drive(&mut recv, &mut tape, out, archive_404);
    if at == LiveAt::AfterLists {
        let mut o = Out::default();
        live(&mut recv, &mut o);
        all.extend(drive(&mut recv, &mut tape, o, archive_404));
    }
    let tick = recv.on_tick(NOW + QUIET_MS);
    all.extend(drive(&mut recv, &mut tape, tick, archive_404));
    (recv, tape, all)
}

/// B1's case recorded, M BEFORE the chat load (`rec-first-live-before-load.jsonl`, W1 variant
/// (b)): after an unclean end TDLib knows the peers (chat-info database), creates the dialogs
/// from the first live messages (u201 15, g301 16) and tells nothing of the tail; the cursors
/// stand at the stale `s` (u201 4, g301 8). The first live message of each chat opens B1's check
/// gap `(s, M)` in its own row's batch, the pages come from the recording (`getChatHistory(chat,
/// from = M)` answers the server's whole page), the tail arrives once each as HISTORY rows (M
/// overtook it, Q-D1), the gap closes. 5 recorded answers: 3 `loadChats` + 2 pages (the archive's
/// `404` is synthetic). MUTANT: the B1 trigger off -> the tail never arrives.
#[test]
fn b1_pages_the_recorded_lost_tail_from_the_first_live_message() {
    let rec = Recording::load("rec-first-live-before-load.jsonl");
    // The recording's premise: the first page from M holds the tail and everything below `s`.
    assert_eq!(
        recorded_page(&rec, "pages", U201, 15),
        [13, 11, 9, 4, 3, 2, 1]
    );
    assert_eq!(recorded_page(&rec, "pages", G301, 16), [14, 12, 10, 8, 7]);

    let (mut tape, _housekeeping) = Tape::of([rec.part("pages"), rec.part("chats")]);
    let mut recv = Receiver::new(Some(ME), NOW, true);
    recv.seed(&stored_seed(&world_before_the_restart()));
    let mut out = recv.chats.link(ME);
    for u in rec.part("first-live").updates() {
        feed(&mut recv, u, &mut out);
    }
    out.extend(recv.begin_sync());
    let out = drive(&mut recv, &mut tape, out, archive_404);

    assert!(recv.sync_done(), "the start sync did not end");
    assert_eq!(
        tape.used, 5,
        "recorded answers used (Q-L11): 3 loadChats + 2 pages (the archive's 404 is synthetic)"
    );
    assert_eq!(pages_asked(&tape, U201), [15]);
    assert_eq!(pages_asked(&tape, G301), [16]);
    for (chat, s, m, tail) in [(U201, 4, 15, [9, 11, 13]), (G301, 8, 16, [10, 12, 14])] {
        assert_eq!(
            gaps_of(&out, chat),
            [("open-unclean", s, m), ("close", s, 0)],
            "B1's check gap (s, M), closed by the one page"
        );
        assert_eq!(
            live_of(&out, chat).iter().map(|r| r.0).collect::<Vec<_>>(),
            [m],
            "M, once"
        );
        assert_eq!(
            history_of(&out, chat),
            tail,
            "the lost tail once each, as history, nothing at or below s"
        );
    }
    assert_eq!(recv.sync.reconcile.open(), 0, "a gap is still open");
    no_notice(&out);
}

/// M2 (critic): the first page from `M` of a KNOWN chat is TDLib's top message alone
/// (`rec-first-live-after-load.jsonl`, u201: `[11]`, no id at or below `s`), so a gap `(s, M)`
/// must NOT close on it: the next page from 11 brings the rest and reaches `s`. The test feeds
/// `M` while the archive list's `loadChats` is still unanswered, so B1 opens `(4, 12)` with the
/// continuity point at `s` and the start decision later finds `td_last` at the cursor (after the
/// lists the decision covers the tail and the page `[T]` closes `(T, M)` legitimately, see the
/// next test). 7 recorded answers: 3 `loadChats` + u201's 2 pages + the supergroup's 2 pages.
/// MUTANT: `on_page` closes a gap on any non-empty page -> the rows below the top are missing.
#[test]
fn the_recorded_first_page_of_a_known_chat_does_not_close_the_gap() {
    let rec = Recording::load("rec-first-live-after-load.jsonl");
    // The recording's precondition: the first page from M is non-empty and holds no id at or
    // below `s` (an assert, not a skip: a test that skips silently would go vacuous).
    let first = recorded_page(&rec, "pages", U201, 12);
    assert!(
        !first.is_empty() && first.iter().all(|id| *id > 4),
        "precondition of the recording: the first page from M is TDLib's top, above s: {first:?}"
    );
    assert_eq!(recorded_page(&rec, "pages", U201, 11), [10, 9, 4, 3, 2, 1]);

    let (recv, tape, out) = after_load_run(LiveAt::BeforeArchive);
    assert_eq!(pages_asked(&tape, U201), [12, 11], "two pages, not one");
    assert_eq!(
        tape.used, 7,
        "recorded answers used (Q-L11): 3 loadChats + 2 + 2 pages"
    );
    assert_eq!(
        gaps_of(&out, U201),
        [("open-unclean", 4, 12), ("paged", 4, 11), ("close", 4, 0)]
    );
    assert_eq!(
        live_of(&out, U201).iter().map(|r| r.0).collect::<Vec<_>>(),
        [12],
        "M, once"
    );
    assert_eq!(
        history_of(&out, U201),
        [9, 10, 11],
        "the whole lost tail once each, as history"
    );
    assert_eq!(recv.sync.reconcile.open(), 0, "a gap is still open");
    no_notice(&out);
}

/// The recording's own order (the lists first, then `M`): for a PRIVATE chat the START DECISION
/// covers the lost tail: `td_last` 11 above the cursor 4 opens `(4, 11)`, its one page (from 11)
/// brings 10 and 9, and `M` (12) is then no B1 trigger (the decision's catch-up row went through
/// `new_message`, which marks the chat's first message of the run seen), so TDLib's page `[11]`
/// for `M` is never asked: the critic's M2 reading, pinned. 6 recorded answers (u201's page from
/// 12 stays unused). MUTANT: `first_live` true for every live message -> `M` opens a second gap
/// `(11, 12)` and asks that page.
#[test]
fn in_the_recorded_order_the_start_decision_covers_a_private_chats_tail() {
    let (_recv, tape, out) = after_load_run(LiveAt::AfterLists);
    assert_eq!(pages_asked(&tape, U201), [11], "the decision's page only");
    assert_eq!(
        gaps_of(&out, U201),
        [("open-start", 4, 11), ("close", 4, 0)]
    );
    assert_eq!(
        live_of(&out, U201).iter().map(|r| r.0).collect::<Vec<_>>(),
        [11, 12],
        "td_last as the catch-up row, then M"
    );
    assert_eq!(history_of(&out, U201), [9, 10]);
    assert_eq!(
        tape.used, 6,
        "recorded answers used (Q-L11): 3 loadChats + 1 + 2 pages"
    );
}

/// W4's golden half: a supergroup's first live message after a restart (`Mc` 5, the stream below
/// it, 2..4, lost) is fed AFTER the lists and BEFORE the quiet wait ends, so the start decision
/// finds `td_last` (5) at the cursor and only the ID TRIGGER opens `(1, 5)` (`rec-first-live-
/// after-load.jsonl`, c401: the page from 5 is TDLib's top `[4]`, the next from 4 is `[3, 2, 1]`).
/// The tail arrives once each as history rows and the gap closes. 6 recorded answers. MUTANT: the
/// id trigger off -> the tail is lost.
#[test]
fn an_id_jump_after_a_restart_pages_the_recorded_supergroup_tail() {
    let (recv, tape, out) = after_load_run(LiveAt::AfterLists);
    assert_eq!(pages_asked(&tape, SG401), [5, 4]);
    assert_eq!(
        gaps_of(&out, SG401),
        [("open-id-jump", 1, 5), ("paged", 1, 4), ("close", 1, 0)],
        "the id trigger's gap, the only one (the decision found nothing)"
    );
    assert_eq!(
        live_of(&out, SG401).iter().map(|r| r.0).collect::<Vec<_>>(),
        [5],
        "Mc, once"
    );
    assert_eq!(
        history_of(&out, SG401),
        [2, 3, 4],
        "the lost tail once each"
    );
    assert_eq!(
        tape.used, 6,
        "recorded answers used (Q-L11): 3 loadChats + 1 + 2 pages"
    );
    assert_eq!(recv.sync.reconcile.open(), 0, "a gap is still open");
    no_notice(&out);
}

/// The restart run of `rec-difference-too-long.jsonl` (step `restart-too-long`) after a CLEAN
/// stop: the common box's difference was TOO LONG, so TDLib dropped the window (u201 9, 10, 11
/// and g301 12 were queued: no `updateNewMessage`) and tells each chat's REAL top after
/// `loadChats` (u201 11, g301 12; the stale cursors are 4 and 8). Nothing live comes. The pages
/// are the recording's `getChatHistory(chat, from = td_last)`, the key of the start decision's
/// own request (critic L2).
fn too_long_restart() -> (Receiver, Tape, Out) {
    let rec = Recording::load("rec-difference-too-long.jsonl");
    let (mut tape, _housekeeping) = Tape::of([rec.part("restart-too-long")]);
    let mut recv = Receiver::new(Some(ME), NOW, false);
    recv.seed(&stored_seed(&world_before_the_restart()));
    let mut out = recv.chats.link(ME);
    out.extend(recv.begin_sync());
    let mut all = drive(&mut recv, &mut tape, out, archive_404);
    let tick = recv.on_tick(NOW + QUIET_MS);
    all.extend(drive(&mut recv, &mut tape, tick, archive_404));
    (recv, tape, all)
}

/// W3, at a restart: TDLib dropped the window of a too-long common-box difference and told the
/// real tops, so the START DECISION fetches it: for u201 `(4, 11)`, `td_last` 11 a live catch-up
/// row, the page from 11 brings 10 and 9 as history rows, once each, nothing at or below the
/// cursor; for g301 `(8, 12)`, `td_last` 12 the only row (the window was that one message), the
/// page from 12 is `[8, 7]`, below the cursor. The other chats (c401, u202) stand at their tops:
/// no row, no gap. 5 recorded answers: 3 `loadChats` + 2 pages. MUTANT: `decide` skips chats with
/// no stored gap -> the window is not fetched.
#[test]
fn a_recorded_common_too_long_at_a_restart_is_fetched_by_the_start_decision() {
    let rec = Recording::load("rec-difference-too-long.jsonl");
    assert_eq!(
        recorded_page(&rec, "restart-too-long", U201, 11),
        [10, 9, 4, 3, 2, 1]
    );
    assert_eq!(recorded_page(&rec, "restart-too-long", G301, 12), [8, 7]);

    let (recv, tape, out) = too_long_restart();
    assert!(recv.sync_done(), "the start sync did not end");
    assert_eq!(pages_asked(&tape, U201), [11], "from TDLib's told last");
    assert_eq!(pages_asked(&tape, G301), [12], "from TDLib's told last");
    assert_eq!(
        tape.used, 5,
        "recorded answers used (Q-L11): 3 loadChats + 2 pages (the archive's 404 is synthetic)"
    );
    assert_eq!(
        gaps_of(&out, U201),
        [("open-start", 4, 11), ("close", 4, 0)]
    );
    assert_eq!(
        gaps_of(&out, G301),
        [("open-start", 8, 12), ("close", 8, 0)]
    );
    assert_eq!(
        live_of(&out, U201),
        [(11, true)],
        "td_last, a live catch-up row"
    );
    assert_eq!(history_of(&out, U201), [9, 10], "the window once each");
    assert_eq!(
        live_of(&out, G301),
        [(12, true)],
        "td_last, the whole window"
    );
    assert!(history_of(&out, G301).is_empty(), "a row at or below s");
    for chat in [SG401, U202] {
        assert!(gaps_of(&out, chat).is_empty(), "{chat}: a gap");
        assert!(
            live_of(&out, chat).is_empty() && history_of(&out, chat).is_empty(),
            "{chat}: a row for a chat at its top"
        );
    }
    assert_eq!(recv.sync.reconcile.open(), 0, "a gap is still open");
    no_notice(&out);
}

/// W3, in a LIVE run: the stated limit pinned. After the restart above, a live common-box
/// too-long difference (`live-gap`: ids 13, 14 stored, 15 pushed) drops the window and the push
/// that revealed it; TDLib tells nothing of 13..15 (the recording's steps `live-gap` and
/// `live-next` hold no object naming them) and the next live message `M'` (16) arrives once.
/// The machines open NO gap (a private chat has no id trigger and the run is clean), ask NOTHING
/// and deliver `M'` alone: the hole is real and not fetched. And even a gap opened over it would
/// not fetch it: TDLib's page from `M'` (the recording's `live-pages`) is its LOCAL history, which
/// bridges the hole (`[11, 10, 9, ...]`, none of 13..15), so it reaches the gap's lower end and
/// closes the gap with no row: the remedy is a helper restart after a long absence (D-4, the
/// start decision then fetches the window), never a trigger. 6 recorded answers: the restart's 5
/// + the live page. No mutant: this pins a limit.
///
/// U12 (honesty MEDIUM-1): the remedy has a PRECONDITION, pinned at the end: the restart must
/// come BEFORE the live helper runs the difference (a resume hint after a long sleep, the helper
/// stopped before its network returns). Once `M'` is delivered the stored cursor is 16, the hole
/// 13..15 lies BELOW it, and a restart finds TDLib's told last (16) at the cursor: no gap, no
/// page, the hole stays (a new `Receiver` seeded with that committed state, replaying the
/// recording's restart answers with u201's last message told as 16).
#[test]
fn a_recorded_live_too_long_leaves_the_stated_hole() {
    let rec = Recording::load("rec-difference-too-long.jsonl");
    // The recording's premises: the local page from M' starts at the old last, none of the gap.
    let local = recorded_page(&rec, "live-pages", U201, 16);
    assert_eq!(local, [11, 10, 9, 4, 3, 2, 1]);
    assert!(
        !local.iter().any(|id| (13..=15).contains(id)),
        "the recorded page holds a dropped message"
    );

    let (mut recv, tape, _restart) = too_long_restart();
    recv.messages.start_sync_done();
    let (mut tape_live, _housekeeping) = Tape::of([rec.part("live-pages")]);
    let mut out = Out::default();
    for label in ["live-gap", "live-next"] {
        for u in rec.part(label).updates() {
            feed(&mut recv, u, &mut out);
        }
    }
    assert!(out.fetch.is_empty(), "a request was asked: {:?}", out.fetch);
    assert!(out.staged.gaps.is_empty(), "a gap opened: {:?}", gaps(&out));
    assert_eq!(
        live_of(&out, U201),
        [(16, false)],
        "M', once, a plain live row"
    );
    assert!(history_of(&out, U201).is_empty());
    assert_eq!(recv.sync.reconcile.open(), 0);
    assert_eq!(recv.messages.cursor(U201).delivered_max, 16 * STEP);

    // A gap over the hole, had one opened: TDLib's local page closes it with no row.
    let mut over = Out::default();
    over.staged.gaps.push(GapChange::Open {
        chat_id: U201,
        above: 11 * STEP,
        below: 16 * STEP,
        cause: GapCause::IdJump,
    });
    recv.sync.reconcile.track(&mut over);
    let paged = drive(&mut recv, &mut tape_live, over, no_fallback);
    assert_eq!(pages_asked(&tape_live, U201), [16]);
    assert_eq!(
        gaps_of(&paged, U201),
        [("open-id-jump", 11, 16), ("close", 11, 0)],
        "TDLib's local page closes the gap"
    );
    assert!(
        live_of(&paged, U201).is_empty() && history_of(&paged, U201).is_empty(),
        "a row came from the bridged page"
    );
    assert_eq!(
        tape.used + tape_live.used,
        6,
        "recorded answers used (Q-L11): 5 at the restart + the live page"
    );

    // The precondition of the remedy (U12): after `M'` the committed cursor is 16 and TDLib's
    // told last at the next restart is 16 too. The restart's recorded answers, u201's last
    // message told as 16 (the recorded objects that name it are rewritten), over a store at that
    // cursor: nothing to decide for u201.
    let (mut later_tape, _housekeeping) = Tape::of([rec.part("restart-too-long")]);
    assert!(
        retell_last(&mut later_tape, U201, 16) > 0,
        "no recorded object tells u201's last message"
    );
    let mut world = world_before_the_restart();
    assert_eq!(world[0].0, U201, "u201 is the first stored chat");
    world[0].2 = 16;
    let mut later = Receiver::new(Some(ME), NOW, false);
    later.seed(&stored_seed(&world));
    let mut start = later.chats.link(ME);
    start.extend(later.begin_sync());
    let restarted = drive(&mut later, &mut later_tape, start, archive_404);
    assert!(later.sync_done(), "the second start sync did not end");
    assert!(
        pages_asked(&later_tape, U201).is_empty(),
        "a page for a chat at its top: {:?}",
        pages_asked(&later_tape, U201)
    );
    assert!(
        gaps_of(&restarted, U201).is_empty(),
        "a gap for a chat at its top"
    );
    assert!(
        live_of(&restarted, U201).is_empty() && history_of(&restarted, U201).is_empty(),
        "a row for a chat at its top: the hole below the cursor stays"
    );
}

/// The recorded restart answers of a world whose last message of `chat` is `id` (a server id
/// divided by STEP): every recorded object that tells it (`updateNewChat`'s chat,
/// `updateChatLastMessage`) is rewritten. How many were.
fn retell_last(tape: &mut Tape, chat: i64, id: i64) -> usize {
    let mut changed = 0;
    for recorded in &mut tape.recorded {
        for v in &mut recorded.reply {
            let last = match v["@type"].as_str() {
                Some("updateChatLastMessage") if v["chat_id"] == chat => {
                    Some(&mut v["last_message"])
                }
                Some("updateNewChat") if v["chat"]["id"] == chat => {
                    Some(&mut v["chat"]["last_message"])
                }
                _ => None,
            };
            if let Some(last) = last
                && last.is_object()
            {
                last["id"] = (id * STEP).into();
                changed += 1;
            }
        }
    }
    changed
}

// ---- TG-4 K4: the recorded restart whose read TDLib lost --------------------------------------

/// The restart run of `rec-read-act.jsonl` (rc13): the first run read u202's first new message
/// (9) and g301's (10) with `hold-reads on`, so TDLib echoed both reads (the stored positions
/// below) and the fake never applied them; SIGKILL; after the restart TDLib tells u202 at 5 and
/// g301 at 8 (MODEL: the fake's held marks), u201 at 4 and c401 at 1 (their reads were applied).
/// The lists' load names the two chats whose stored position is above what TDLib told, the
/// product asks each one `viewMessages` of its stored position, and the recorded answers (the
/// recording's `reassert` and `reassert-below-top` steps, GOLDEN: ONE request each, TDLib accepts
/// the id whether or not it holds the message) come back. 5 recorded answers: the main list's 3
/// `loadChats` + the 2 `viewMessages` (the archive's `404` is synthetic). MUTANT: the comparison
/// made against the SEEDED `told_in` (today's `Reads`) -> no re-assert.
#[test]
fn the_recorded_restart_gets_the_lost_read_again() {
    use crate::session::lane::LaneAsk;
    let rec = Recording::load("rec-read-act.jsonl");
    let (mut tape, _housekeeping) = Tape::of([
        rec.part("kill-after-view"),
        rec.part("reassert"),
        rec.part("reassert-below-top"),
    ]);
    // What the first run left in the store (server ids): cursor, read position.
    let mut recv = Receiver::new(Some(ME), NOW, true);
    let mut seed = stored_seed(&[
        (U201, ChatKind::Direct, 4, 4),
        (G301, ChatKind::Group, 11, 10),
        (SG401, ChatKind::Supergroup, 1, 1),
        (U202, ChatKind::Direct, 9, 9),
    ]);
    // The read act's incoming message at the stored position of g301 (the others' cursor rows
    // are theirs): the re-assert asks the highest incoming delivered id at or below the stored
    // position (U2).
    seed.delivered.push(Delivered {
        chat_id: G301,
        msg_id: 10 * STEP,
        author: format!("tg:{}", G301.abs()),
        outgoing: false,
    });
    recv.seed(&seed);
    let mut out = recv.chats.link(ME);
    out.extend(recv.begin_sync());
    let all = drive(&mut recv, &mut tape, out, archive_404);
    assert!(recv.sync_done(), "the start sync did not end");
    assert_eq!(tape.used, 3, "the main list's three answers");

    // The chats TDLib told below the stored position, each ONCE, nothing for the others.
    let mut lost = all.reassert.clone();
    lost.sort_unstable();
    assert_eq!(
        lost,
        [(G301, 10 * STEP), (U202, 9 * STEP)],
        "the re-asserts of the run"
    );
    assert!(
        all.staged
            .chats
            .iter()
            .all(|f| f.read_inbox_max_id.is_none()),
        "a read position fact: the stored ones are at or above every tell of the run"
    );

    // The product asks them as `viewMessages`; TDLib's recorded answers come back and change
    // nothing in the machines (their echo equals the stored position: no fact, no marker).
    for (chat, id) in &lost {
        let req = LaneAsk::Reassert {
            chat_id: *chat,
            id: *id,
        }
        .request();
        let (updates, answer) = tape.answer(&req).expect("the recorded re-assert");
        assert!(answer.is_ok(), "TDLib refused a re-assert");
        let recorded = rec
            .parts
            .iter()
            .flat_map(Part::requests)
            .find(|r| r.ty == "viewMessages" && r.args["chat_id"] == *chat)
            .expect("the recorded request");
        for k in ["chat_id", "message_ids", "force_read", "source"] {
            assert_eq!(
                recorded.args[k], req[k],
                "the product's request differs from the recorded one in {k}"
            );
        }
        let mut o = Out::default();
        for u in &updates {
            feed(&mut recv, u, &mut o);
        }
        assert!(
            o.staged.chats.iter().all(|f| f.read_inbox_max_id.is_none()),
            "the echo of the re-assert staged a position"
        );
        assert!(o.reassert.is_empty(), "a re-assert asked another");
    }
    assert_eq!(
        tape.used, 5,
        "recorded answers used (Q-L11): 3 loadChats + 2 viewMessages (the archive's 404 is synthetic)"
    );
    assert!(
        recv.messages.lost_reads(&recv.chats).is_empty(),
        "a chat was named twice in the run"
    );
}
