//! The start sync's loads (TG-3 C8): replayed from `rec-fresh-start.jsonl` and
//! `rec-restart-saved-pts.jsonl` by name (each test asserts how many recorded answers it used,
//! Q-L11), synthetic lines only for what no recording holds (the 20-message cap, a short page,
//! Q-B4's read state). Nothing here prints a recorded value: assertions name ids and kinds.

use std::collections::{BTreeMap, VecDeque};

use kourier_backend_api::BackendEvent;
use kourier_model::{ConvKey, OutgoingState};
use serde_json::{Value, json};

use super::*;
use crate::receive::Receiver;
use crate::replay::{Item, Part, Recorded, Recording, key};
use crate::tdapi::{self, Update};

/// The account's own id in the recordings.
const ME: i64 = 100;
const NOW: i64 = 1_700_000_000_000;
const STEP: i64 = 1 << 20;

/// TDLib as the recorded parts answer it: the k-th request of one key gets the k-th recorded
/// reply of that key (the replies' updates first, then the answer); a request the parts have no
/// answer for goes to `fallback` (synthetic) and is not counted.
struct Tape {
    recorded: VecDeque<Recorded>,
    used: usize,
    asked: Vec<Value>,
}

impl Tape {
    fn of<'a>(parts: impl IntoIterator<Item = &'a Part>) -> (Self, Vec<Value>) {
        let mut recorded = VecDeque::new();
        let mut loose = Vec::new();
        for part in parts {
            for item in &part.items {
                match item {
                    Item::Update(v) => loose.push(v.clone()),
                    Item::Request(r) => recorded.push_back(r.clone()),
                }
            }
        }
        (
            Self {
                recorded,
                used: 0,
                asked: Vec::new(),
            },
            loose,
        )
    }

    fn answer(&mut self, req: &Value) -> Option<(Vec<Value>, Result<Value, LinkError>)> {
        self.asked.push(req.clone());
        let ty = req["@type"].as_str().unwrap_or_default();
        let want = key(ty, req);
        let at = self
            .recorded
            .iter()
            .position(|r| r.ty == ty && key(&r.ty, &r.args) == want)?;
        let r = self.recorded.remove(at)?;
        self.used += 1;
        let mut reply = r.reply.clone();
        let answer = reply.pop().expect("an answer");
        Some((reply, as_result(answer)))
    }
}

fn as_result(answer: Value) -> Result<Value, LinkError> {
    if answer["@type"] == "error" {
        Err(LinkError::Td {
            code: i32::try_from(answer["code"].as_i64().unwrap_or(0)).unwrap_or(0),
        })
    } else {
        Ok(answer)
    }
}

/// One TDLib line into the machines.
fn feed(recv: &mut Receiver, v: &Value, out: &mut Out) {
    if let Some(Update::Receive(r)) = tdapi::parse_update(&v.to_string()) {
        out.extend(recv.on_update(&r, NOW));
    }
}

/// Runs every fetch the machines ask (in order, the lane's one-in-flight order) against `tape`
/// and `fallback`, until none is left. Everything the machines gave, flushed at the end.
fn drive(
    recv: &mut Receiver,
    tape: &mut Tape,
    first: Out,
    mut fallback: impl FnMut(&Value) -> (Vec<Value>, Result<Value, LinkError>),
) -> Out {
    let mut all = Out::default();
    let mut queue: VecDeque<Fetch> = first.fetch.iter().copied().collect();
    all.extend(Out {
        fetch: Vec::new(),
        ..first
    });
    while let Some(f) = queue.pop_front() {
        let req = f.request();
        let (updates, answer) = tape.answer(&req).unwrap_or_else(|| fallback(&req));
        for u in &updates {
            let mut o = Out::default();
            feed(recv, u, &mut o);
            queue.extend(o.fetch.drain(..));
            all.extend(o);
        }
        let mut o = recv.on_fetched(f, answer, NOW);
        queue.extend(o.fetch.drain(..));
        all.extend(o);
    }
    all.extend(recv.chats.flush());
    all
}

fn no_fallback(req: &Value) -> (Vec<Value>, Result<Value, LinkError>) {
    panic!("a request the recording does not answer: {}", req["@type"]);
}

/// A linked run's machines (seeded from `seed`), its loose updates fed, the start sync begun.
fn begun(seed: &crate::store::Seed, loose: &[Value]) -> (Receiver, Out) {
    let mut recv = Receiver::new(Some(ME), NOW, false);
    recv.seed(seed);
    let mut out = recv.chats.link(ME);
    for v in loose {
        feed(&mut recv, v, &mut out);
    }
    out.extend(recv.begin_sync());
    (recv, out)
}

/// The requests asked, as `type` or `type(chat_id, from, limit)` / `loadChats(list)`.
fn asked(tape: &Tape) -> Vec<String> {
    tape.asked
        .iter()
        .map(|r| match r["@type"].as_str().unwrap_or_default() {
            "loadChats" => format!(
                "loadChats({})",
                r["chat_list"]["@type"].as_str().unwrap_or("")
            ),
            "getChatHistory" => format!(
                "getChatHistory({}, {}, {})",
                r["chat_id"],
                r["from_message_id"].as_i64().unwrap_or(0) / STEP,
                r["limit"]
            ),
            other => other.to_owned(),
        })
        .collect()
}

/// The history messages per conversation (server ids), in row order; and every `Message` event.
fn history(out: &Out) -> (BTreeMap<String, Vec<i64>>, usize) {
    let mut by_conv: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    let mut live = 0;
    for row in &out.staged.rows {
        for e in &row.events {
            match &e.event {
                BackendEvent::HistoryBatch {
                    conv,
                    messages,
                    has_more,
                    ..
                } => {
                    assert!(!has_more, "a fresh-link batch says has_more");
                    assert!(messages.len() <= kourier_backend_api::MAX_HISTORY_BATCH);
                    by_conv.entry(conv_name(conv)).or_default().extend(
                        messages
                            .iter()
                            .map(|m| m.local.parse::<i64>().expect("an id") / STEP),
                    );
                }
                BackendEvent::Message(_) => live += 1,
                _ => {}
            }
        }
    }
    (by_conv, live)
}

fn conv_name(conv: &ConvKey) -> String {
    match conv {
        ConvKey::Direct(u) => u.as_str().to_owned(),
        ConvKey::Group(g) => g.to_string(),
        other => format!("{other:?}"),
    }
}

/// The fresh link of `rec-fresh-start.jsonl`: its first part replayed.
fn fresh_start() -> (Receiver, Tape, Out) {
    let rec = Recording::load("rec-fresh-start.jsonl");
    let (mut tape, loose) = Tape::of(&rec.parts);
    let (mut recv, first) = begun(&crate::store::Seed::default(), &loose);
    let out = drive(&mut recv, &mut tape, first, no_fallback);
    (recv, tape, out)
}

/// Golden at the pin: `loadChats` ends in 404 per list. The main list pages until its 404, then
/// the archive until its 404, one after the other; then the start sync's fresh-link load.
#[test]
fn pages_until_404_in_both_lists() {
    let (recv, tape, _) = fresh_start();
    let a = asked(&tape);
    assert_eq!(
        a[..6],
        [
            "loadChats(chatListMain)",
            "loadChats(chatListMain)",
            "loadChats(chatListMain)",
            "loadChats(chatListArchive)",
            "loadChats(chatListArchive)",
            "loadChats(chatListArchive)",
        ]
    );
    assert!(recv.sync_done(), "the start sync did not end");
    assert_eq!(tape.used, a.len(), "every request took a recorded answer");
    assert_eq!(tape.used, 7, "recorded answers used (Q-L11)");
    // The archive's chat came only through the archive's load.
    assert!(recv.chats.knows(203), "the archive was not loaded");
}

/// His "Last + unread, up to 20": chat 201 (last 4, 3 unread, the last one counted in them) gets its
/// last message from TDLib's chat (no request) and ONE page from it of the 2 unread messages below
/// it (U5; the recording's own request asked for 3, and the replay answers a request by its chat
/// and `from` with the recorded page whatever the limit, so the replay's page still holds the
/// recorded read message 1: the exact count is pinned by
/// `the_fresh_page_asks_exactly_the_unread_messages_below_the_last_one`, whose pages honour the
/// limit). Every other chat of the recording has at most one unread message: its last message
/// only, no request. Held chats get nothing. A synthetic chat with 50 unread is cut at 20
/// whatever the pages bring.
#[test]
fn fresh_link_loads_last_and_unread_up_to_20() {
    let (recv, tape, out) = fresh_start();
    assert_eq!(asked(&tape)[6..], ["getChatHistory(201, 4, 2)"]);
    assert_eq!(tape.used, 7, "recorded answers used (Q-L11)");
    let (by_conv, _) = history(&out);
    // ONE batch per chat (K5): the page's rows and the last message, in id order.
    assert_eq!(by_conv.get("tg:201"), Some(&vec![1, 2, 3, 4]));
    assert_eq!(by_conv.get("tg:202"), Some(&vec![5]));
    assert_eq!(by_conv.get("tg:203"), Some(&vec![6]));
    for held in [-1_000_000_000_402_i64, -1_000_000_000_403] {
        assert!(recv.chats.held(held).is_some(), "{held} is not held");
        assert!(
            !by_conv.keys().any(|k| k.contains(&held.to_string())),
            "a held chat got rows"
        );
    }
    assert!(
        by_conv.len() >= 4,
        "conversations loaded: {}",
        by_conv.len()
    );
    // Their cursors moved with the rows.
    assert_eq!(recv.messages.cursor(201).delivered_max, 4 * STEP);

    // The cap: 50 unread, pages that bring 7 each.
    let (mut recv, mut tape, first) = synthetic(50, 60);
    let out = drive(&mut recv, &mut tape, first, pages_of(7));
    assert_eq!(
        asked(&tape),
        [
            "getChatHistory(77, 60, 20)",
            "getChatHistory(77, 53, 13)",
            "getChatHistory(77, 46, 6)",
        ]
    );
    let (by_conv, _) = history(&out);
    assert_eq!(
        by_conv["tg:77"].len(),
        1 + 20,
        "the last message and 20 unread"
    );
    assert!(recv.sync_done());
}

/// U5 (K7's question a): the fresh page takes EXACTLY the unread messages below the last one.
/// TDLib's `unread_count` counts the last message itself when it is incoming and above the read
/// position, so the page is `unread - 1` long then; when the last message is our own or already
/// read, every unread message lies below it and the page is `unread` long (so ONE unread message
/// below an own last message is fetched too, which the old rule `unread > 1` never did); both
/// are cut at 20. (Mutant: the old `min(unread, 20)` for the page and `unread > 1` for the plan.)
#[test]
fn the_fresh_page_asks_exactly_the_unread_messages_below_the_last_one() {
    let asked_and_batch = |unread: i32, last: Value, last_read: i64| {
        let (mut recv, begun) = unloaded_with(unread, last, last_read);
        let lists = answer_lists(&mut recv, begun);
        let mut tape = Tape::of(std::iter::empty()).0;
        let out = drive(&mut recv, &mut tape, lists, pages_of(40));
        assert!(recv.sync_done());
        (asked(&tape), history(&out).0.remove("tg:77"))
    };
    // The last message (9) is incoming and unread: TDLib's count of 3 includes it, two lie below.
    let (asked, batch) = asked_and_batch(3, message(77, 9, false), 0);
    assert_eq!(asked, ["getChatHistory(77, 9, 2)"]);
    assert_eq!(
        batch,
        Some(vec![7, 8, 9]),
        "the last message and the two below it"
    );
    // The same chat with the last message read (position 9, no unread): no page at all.
    let (asked, batch) = asked_and_batch(0, message(77, 9, false), 9 * STEP);
    assert!(
        asked.is_empty(),
        "a page for a chat with nothing unread: {asked:?}"
    );
    assert_eq!(batch, Some(vec![9]));
    // One unread message and it is the last one: the last message alone, no request.
    let (asked, batch) = asked_and_batch(1, message(77, 9, false), 0);
    assert!(
        asked.is_empty(),
        "a page for the last message alone: {asked:?}"
    );
    assert_eq!(batch, Some(vec![9]));
    // The last message is OUR OWN: TDLib's count (2) holds only incoming messages below it.
    let (asked, batch) = asked_and_batch(2, message(77, 9, true), 0);
    assert_eq!(asked, ["getChatHistory(77, 9, 2)"]);
    assert_eq!(batch, Some(vec![7, 8, 9]));
    // ... and one unread message below an own last message is fetched.
    let (asked, batch) = asked_and_batch(1, message(77, 9, true), 0);
    assert_eq!(asked, ["getChatHistory(77, 9, 1)"]);
    assert_eq!(batch, Some(vec![8, 9]));
    // The cap: 40 unread below an own last message and 41 with an incoming one, both 20.
    let (asked, batch) = asked_and_batch(40, message(77, 90, true), 0);
    assert_eq!(asked, ["getChatHistory(77, 90, 20)"]);
    assert_eq!(batch.map(|b| b.len()), Some(1 + 20));
    let (asked, batch) = asked_and_batch(41, message(77, 90, false), 0);
    assert_eq!(asked, ["getChatHistory(77, 90, 20)"]);
    assert_eq!(batch.map(|b| b.len()), Some(1 + 20));
}

/// The rule behind the page ([`unread_below_last`]): the last message counts in TDLib's unread
/// number only when it is incoming and above the read position; never below zero.
#[test]
fn the_unread_count_below_the_last_message() {
    let last = |id: i64, own: bool| {
        let line = json!({"@type": "updateNewMessage", "message": message(77, id, own)});
        match tdapi::parse_update(&line.to_string()) {
            Some(Update::Receive(tdapi::Receive::NewMessage(m))) => *m,
            _ => panic!("not a message"),
        }
    };
    assert_eq!(unread_below_last(3, &last(9, false), 0), 2);
    assert_eq!(unread_below_last(3, &last(9, true), 0), 3);
    assert_eq!(unread_below_last(3, &last(9, false), 9 * STEP), 3);
    assert_eq!(unread_below_last(3, &last(9, false), 12 * STEP), 3);
    assert_eq!(unread_below_last(0, &last(9, false), 0), 0);
    assert_eq!(unread_below_last(1, &last(9, false), 0), 0);
    assert_eq!(unread_below_last(-1, &last(9, true), 0), 0);
}

/// A short, non-empty page is not the end (TDLib answers what it holds in memory first): the next
/// page starts from the oldest id returned; an empty page ends the chat's load.
#[test]
fn a_short_page_is_not_the_end() {
    let (mut recv, mut tape, first) = synthetic(10, 30);
    let mut n = 0;
    let out = drive(&mut recv, &mut tape, first, |req| {
        n += 1;
        pages_of(if n == 1 { 4 } else { 0 })(req)
    });
    assert_eq!(
        asked(&tape),
        ["getChatHistory(77, 30, 9)", "getChatHistory(77, 26, 5)"]
    );
    assert_eq!(history(&out).0["tg:77"], [26, 27, 28, 29, 30]);
    assert!(recv.sync_done());
}

/// The fresh-link rows are HISTORY: never a live `Message` (they must not notify), every batch
/// `has_more: false` (else the UI shows "getting your history" for good) and one conversation.
#[test]
fn fresh_link_rows_are_history_never_notified() {
    let (_, tape, out) = fresh_start();
    assert_eq!(tape.used, 7, "recorded answers used (Q-L11)");
    let (by_conv, live) = history(&out);
    assert_eq!(live, 0, "a fresh-link row is a live message");
    assert!(!by_conv.is_empty());
    // An unread incoming row is `read: false`, an older one `read: true`.
    let reads: Vec<(i64, bool)> = out
        .staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::HistoryBatch { conv, messages, .. } if conv_name(conv) == "tg:201" => {
                Some(
                    messages
                        .iter()
                        .map(|m| (m.local.parse::<i64>().unwrap_or(0) / STEP, m.read)),
                )
            }
            _ => None,
        })
        .flatten()
        .collect();
    assert_eq!(reads, [(1, true), (2, false), (3, false), (4, false)]);
}

/// No crawl: `rec-restart-saved-pts.jsonl`'s second helper run starts from the cursors the first
/// run committed (backend.db's seed): its chats are not new, so the start sync asks no history
/// at all (the chats that changed while Kourier was off are the reconcile's, C9).
#[test]
fn no_history_request_for_an_unchanged_chat() {
    let rec = Recording::load("rec-restart-saved-pts.jsonl");
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = crate::db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from_bytes([7; 32]),
    )
    .expect("open");
    let (mut tape, loose) = Tape::of(rec.parts.iter().take(1));
    let (mut recv, first) = begun(&crate::store::Seed::default(), &loose);
    let out = drive(&mut recv, &mut tape, first, no_fallback);
    assert_eq!(tape.used, 7, "first run: recorded answers used (Q-L11)");
    crate::store::commit_rows(&conn, &out.staged, NOW).expect("commit");
    let seed = crate::store::seed(&conn).expect("seed");
    assert!(
        seed.chats
            .iter()
            .any(|c| c.chat_id == 203 && c.delivered_max_id == 6 * STEP)
    );

    let (mut tape, loose) = Tape::of(rec.parts.iter().skip(1).take(1));
    let (mut recv, first) = begun(&seed, &loose);
    let out = drive(&mut recv, &mut tape, first, no_fallback);
    let a = asked(&tape);
    assert!(
        !a.iter().any(|r| r.starts_with("getChatHistory")),
        "the second run asked history: {a:?}"
    );
    assert_eq!(a.len(), 6, "the two lists' pages");
    assert_eq!(tape.used, 6, "second run: recorded answers used (Q-L11)");
    assert!(recv.sync_done());
    // The live messages of the second run still came (as rows), the unchanged chat got nothing.
    let (by_conv, _) = history(&out);
    assert!(by_conv.is_empty(), "history rows in the second run");
}

/// Q-B4: an OWN message in a private chat at or below the peer's read position TDLib told is
/// `Read`, history and live alike; above it `Sent`; a group's own messages stay `Sent`.
#[test]
fn an_own_message_the_peer_read_is_read() {
    let mut recv = Receiver::new(Some(ME), NOW, false);
    let mut out = recv.chats.link(ME);
    let chat = |id: i64, ty: Value, last: Value| {
        json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": id, "type": ty,
            "title": "Fay Sample", "positions": [{"@type": "chatPosition",
            "list": {"@type": "chatListMain"}, "order": "100", "is_pinned": false}],
            "last_message": last, "unread_count": 0, "last_read_inbox_message_id": 0,
            "last_read_outbox_message_id": 5 * STEP}})
    };
    for v in [
        user(78),
        chat(
            78,
            json!({"@type": "chatTypePrivate", "user_id": 78}),
            message(78, 4, true),
        ),
        json!({"@type": "updateBasicGroup", "basic_group": {"@type": "basicGroup", "id": 79,
            "member_count": 3, "status": {"@type": "chatMemberStatusMember"}, "is_active": true}}),
        chat(
            -79,
            json!({"@type": "chatTypeBasicGroup", "basic_group_id": 79}),
            message(-79, 4, true),
        ),
    ] {
        feed(&mut recv, &v, &mut out);
    }
    out.extend(recv.begin_sync());
    let mut tape = Tape::of(std::iter::empty()).0;
    let mut out = drive(&mut recv, &mut tape, out, |req| {
        match req["@type"].as_str() {
            Some("loadChats") => (Vec::new(), Err(LinkError::Td { code: 404 })),
            _ => panic!("no history is asked"),
        }
    });
    for (id, own) in [(5, true), (6, true)] {
        feed(
            &mut recv,
            &json!({"@type": "updateNewMessage", "message": message(78, id, own)}),
            &mut out,
        );
    }
    let states: Vec<(String, i64, Option<OutgoingState>)> = out
        .staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .flat_map(|e| match &e.event {
            BackendEvent::HistoryBatch { conv, messages, .. } => messages
                .iter()
                .map(|m| {
                    (
                        conv_name(conv),
                        m.local.parse::<i64>().unwrap_or(0) / STEP,
                        m.initial_out_state.clone(),
                    )
                })
                .collect::<Vec<_>>(),
            BackendEvent::Message(m) => vec![(
                conv_name(&m.conv),
                m.local.parse::<i64>().unwrap_or(0) / STEP,
                m.initial_out_state.clone(),
            )],
            _ => Vec::new(),
        })
        .collect();
    let read = |s: &Option<OutgoingState>| matches!(s, Some(OutgoingState::Read));
    let of = |conv: &str, id: i64| {
        states
            .iter()
            .find(|(c, i, _)| c == conv && *i == id)
            .map(|(_, _, s)| s.clone())
            .unwrap_or_else(|| panic!("no row {conv} {id}"))
    };
    assert!(read(&of("tg:78", 4)), "history at the read position");
    assert!(read(&of("tg:78", 5)), "live at the read position");
    assert!(!read(&of("tg:78", 6)), "live above the read position");
    let group = states
        .iter()
        .find(|(c, ..)| c != "tg:78")
        .expect("the group's row");
    assert!(!read(&group.2), "a group's own message");
}

// ---- synthetic chats ------------------------------------------------------------------------

fn user(id: i64) -> Value {
    json!({"@type": "updateUser", "user": {"@type": "user", "id": id, "first_name": "Fay",
        "last_name": "Sample", "phone_number": "", "is_contact": true,
        "type": {"@type": "userTypeRegular"}}})
}

/// A synthetic message `id` (a server id) of `chat`; own or from the chat's peer.
fn message(chat: i64, id: i64, own: bool) -> Value {
    let sender = if own { ME } else { chat.abs() };
    json!({"@type": "message", "id": id * STEP, "chat_id": chat, "is_outgoing": own,
        "sender_id": {"@type": "messageSenderUser", "user_id": sender},
        "date": 1_700_000_100 + id, "edit_date": 0, "media_album_id": "0",
        "content": {"@type": "messageText", "text": {"@type": "formattedText",
            "text": "synthetic text 952", "entities": []}}})
}

/// A linked run with ONE synthetic private chat 77 whose start sync has begun and whose lists
/// are NOT loaded yet: `unread` unread messages, its last one `last`. The output holds the first
/// `loadChats`.
fn synthetic_unloaded(unread: i32, last: i64) -> (Receiver, Out) {
    unloaded_with(unread, message(77, last, false), 0)
}

/// [`synthetic_unloaded`] with the chat's last message as given (own or the peer's) and TDLib's
/// read position `last_read` (a server id times [`STEP`], 0: nothing read).
fn unloaded_with(unread: i32, last: Value, last_read: i64) -> (Receiver, Out) {
    let mut recv = Receiver::new(Some(ME), NOW, false);
    let mut out = recv.chats.link(ME);
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": 77,
        "type": {"@type": "chatTypePrivate", "user_id": 77}, "title": "Fay Sample",
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": "100", "is_pinned": false}],
        "last_message": last, "unread_count": unread,
        "last_read_inbox_message_id": last_read, "last_read_outbox_message_id": 0}});
    for v in [user(77), chat] {
        feed(&mut recv, &v, &mut out);
    }
    out.extend(recv.begin_sync());
    (recv, out)
}

/// The two lists end at once (404 each): what the machines gave meanwhile, the fetches that are
/// not `loadChats` (the fresh-link pages) kept.
fn answer_lists(recv: &mut Receiver, mut out: Out) -> Out {
    let mut lists = Out::default();
    let mut queue: VecDeque<Fetch> = out.fetch.drain(..).collect();
    lists.extend(out);
    while let Some(f) = queue.pop_front() {
        assert!(matches!(f, Fetch::LoadChats { .. }));
        let mut o = recv.on_fetched(f, Err(LinkError::Td { code: 404 }), NOW);
        for f in o.fetch.drain(..) {
            if matches!(f, Fetch::LoadChats { .. }) {
                queue.push_back(f);
            } else {
                lists.fetch.push(f);
            }
        }
        lists.extend(o);
    }
    lists
}

/// A linked run with ONE synthetic private chat 77: `unread` unread messages, its last one
/// `last`; the lists answer 404 at once. Its start sync begun, the lists already loaded.
fn synthetic(unread: i32, last: i64) -> (Receiver, Tape, Out) {
    let (mut recv, out) = synthetic_unloaded(unread, last);
    let mut tape = Tape::of(std::iter::empty()).0;
    let out = answer_lists(&mut recv, out);
    tape.asked.clear();
    (recv, tape, out)
}

/// TDLib's page of a synthetic chat: `n` messages (at most the limit) below `from`, newest first.
fn pages_of(n: i64) -> impl FnMut(&Value) -> (Vec<Value>, Result<Value, LinkError>) {
    move |req| {
        assert_eq!(req["@type"], "getChatHistory");
        let chat = req["chat_id"].as_i64().expect("chat");
        let from = req["from_message_id"].as_i64().expect("from") / STEP;
        let limit = req["limit"].as_i64().expect("limit");
        let msgs: Vec<Value> = (1..=n.min(limit))
            .map(|k| from - k)
            .filter(|id| *id > 0)
            .map(|id| message(chat, id, false))
            .collect();
        (
            Vec::new(),
            Ok(json!({"@type": "messages", "total_count": msgs.len(), "messages": msgs})),
        )
    }
}

/// TF4 (review dF4, item c; K5): a private chat whose user TDLib has not told is undecided, so the
/// fresh-link load skips it. When its user arrives (`getUser` or an update) the chat gets its
/// own load: the page for its unread messages, and its last message with the page's rows in ONE
/// batch (K5, like the lists' load). A live row that waited for the decision waits for that batch
/// too (critic H1 (b)): it follows it in the same output. (Mutant: no late load, the chat is never
/// loaded; the plan does not hold the rows the decision released.)
#[test]
fn a_late_decided_chat_gets_its_fresh_link_load() {
    let mut recv = Receiver::new(Some(ME), NOW, false);
    let mut out = recv.chats.link(ME);
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": 77,
        "type": {"@type": "chatTypePrivate", "user_id": 77}, "title": "Fay Sample",
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": "100", "is_pinned": false}],
        "last_message": message(77, 5, false), "unread_count": 3,
        "last_read_inbox_message_id": 0, "last_read_outbox_message_id": 0}});
    feed(&mut recv, &chat, &mut out);
    out.extend(recv.begin_sync());
    assert!(
        out.fetch.contains(&Fetch::User(77)),
        "the undecided chat asks its user"
    );
    let mut queue: VecDeque<Fetch> = out.fetch.drain(..).collect();
    let mut lists = Out::default();
    while let Some(f) = queue.pop_front() {
        if matches!(f, Fetch::LoadChats { .. }) {
            let mut o = recv.on_fetched(f, Err(LinkError::Td { code: 404 }), NOW);
            queue.extend(o.fetch.drain(..));
            lists.extend(o);
        }
    }
    assert!(recv.sync_done(), "the lists are over");
    assert!(history(&lists).0.is_empty(), "an undecided chat was loaded");
    assert!(lists.fetch.is_empty(), "a page was asked for it");

    // A live message while the chat is undecided waits (TF4).
    let mut waiting = Out::default();
    feed(&mut recv, &live(6), &mut waiting);
    assert!(shape(&waiting).is_empty(), "a row of an undecided chat");

    let mut late = Out::default();
    feed(&mut recv, &user(77), &mut late);
    assert!(
        shape(&late).is_empty(),
        "the late chat's last message or its live row came before the page: {:?}",
        shape(&late)
    );
    assert_eq!(
        late.fetch,
        [Fetch::History {
            chat_id: 77,
            from: 5 * STEP,
            limit: 2
        }],
        "the unread page"
    );
    // The page answers: the batch (rows + last message), then the live row, one output.
    let page = pages_of(7)(&late.fetch[0].request()).1;
    let done = recv.on_fetched(late.fetch[0], page, NOW);
    assert_eq!(
        shape(&done),
        ["batch tg:77 [3, 4, 5]", "live tg:77 6"],
        "the late batch and the live row after it"
    );
    // Asked once: the user's second update loads nothing more.
    let mut again = Out::default();
    feed(&mut recv, &user(77), &mut again);
    assert!(again.fetch.is_empty() && history(&again).0.is_empty());
}

// ---- TG-4 K5: the one-commit fresh load and the fresh-chat hold ---------------------------------

/// A live `updateNewMessage` of the synthetic chat 77 (server id `id`).
fn live(id: i64) -> Value {
    json!({"@type": "updateNewMessage", "message": message(77, id, false)})
}

/// What an output's rows say, in order: `batch <conv> [ids]` for a history batch, `live <conv>
/// <id>` for a live message (server ids; contact and conversation patches left out).
fn shape(out: &Out) -> Vec<String> {
    let mut shape = Vec::new();
    for row in &out.staged.rows {
        for e in &row.events {
            match &e.event {
                BackendEvent::HistoryBatch { conv, messages, .. } => {
                    let ids: Vec<i64> = messages
                        .iter()
                        .map(|m| m.local.parse::<i64>().expect("an id") / STEP)
                        .collect();
                    shape.push(format!("batch {} {ids:?}", conv_name(conv)));
                }
                BackendEvent::Message(m) => shape.push(format!(
                    "live {} {}",
                    conv_name(&m.conv),
                    m.local.parse::<i64>().expect("an id") / STEP
                )),
                _ => {}
            }
        }
    }
    shape
}

/// The only fetch of an output (the one page of chat 77).
fn only_fetch(out: &Out) -> Fetch {
    assert_eq!(out.fetch.len(), 1, "fetches: {:?}", out.fetch);
    out.fetch[0]
}

/// Brief section 4 (W5): the last message of a chat with unread messages is in NO output before
/// its page answers; the answer's output holds the page's rows AND the last message in ONE
/// history batch (one `Out` is one `Act::Rows` is one transaction), and the cursor stays at 0
/// until then. (Mutant: today's early `messages.history(.., [last])` back.)
#[test]
fn a_fresh_chats_last_message_and_its_unread_page_are_one_batch() {
    let (mut recv, _tape, lists) = synthetic(3, 5);
    assert!(
        shape(&lists).is_empty(),
        "the last message came before its page: {:?}",
        shape(&lists)
    );
    assert_eq!(
        recv.messages.cursor(77).delivered_max,
        0,
        "the cursor moved early"
    );
    let fetch = only_fetch(&lists);
    assert!(!recv.sync_done(), "the sync ended with a page open");
    let page = pages_of(7)(&fetch.request()).1;
    let done = recv.on_fetched(fetch, page, NOW);
    assert_eq!(
        shape(&done),
        ["batch tg:77 [3, 4, 5]"],
        "one batch: the page's rows and the last message"
    );
    assert_eq!(recv.messages.cursor(77).delivered_max, 5 * STEP);
    assert!(recv.sync_done());
}

/// W5's live rule while a plan is open (critic M5): a live message of the chat WAITS, still waits
/// after an unrelated update (the waiting list's release runs after every update), and follows
/// the batch in the same output. A clean stop meanwhile is not clean (`unsettled`). (Mutants: the
/// plan does not hold its chat's rows; `release_decided` replays the held rows.)
#[test]
fn a_live_message_of_a_chat_with_an_open_fresh_plan_waits_for_its_batch() {
    let (mut recv, _tape, lists) = synthetic(3, 5);
    let fetch = only_fetch(&lists);
    let mut held = Out::default();
    feed(&mut recv, &live(6), &mut held);
    assert!(
        shape(&held).is_empty(),
        "a live row before the batch: {:?}",
        shape(&held)
    );
    assert!(recv.unsettled(), "a held row left a stop clean");
    // Another chat's update runs the waiting list's release: the held row stays.
    let mut other = Out::default();
    feed(&mut recv, &user(78), &mut other);
    feed(&mut recv, &live(7), &mut other);
    assert!(
        shape(&other).is_empty(),
        "a later update released a held row"
    );
    assert_eq!(recv.messages.cursor(77).delivered_max, 0);
    let page = pages_of(7)(&fetch.request()).1;
    let done = recv.on_fetched(fetch, page, NOW);
    assert_eq!(
        shape(&done),
        ["batch tg:77 [3, 4, 5]", "live tg:77 6", "live tg:77 7"],
        "the batch first, then the held rows in arrival order"
    );
    assert!(!recv.unsettled(), "the rows were released");
    assert_eq!(recv.messages.cursor(77).delivered_max, 7 * STEP);
}

/// Critic H1 (d): a live message of a fresh chat that comes BEFORE the lists are loaded is held
/// too (a plan does not exist yet): with a page plan it waits for the batch; a chat with no plan
/// (one unread message: its last message alone) gets its rows right after that last message,
/// in the output of the lists' load. (Mutant: the hold only while a plan is open.)
#[test]
fn a_live_message_of_a_fresh_chat_before_the_lists_load_waits_for_its_batch() {
    // With a plan (3 unread).
    let (mut recv, begun) = synthetic_unloaded(3, 5);
    let mut early = Out::default();
    feed(&mut recv, &live(6), &mut early);
    assert!(
        shape(&early).is_empty(),
        "a live row before the lists: {:?}",
        shape(&early)
    );
    assert_eq!(
        recv.messages.cursor(77).delivered_max,
        0,
        "the cursor moved before the lists"
    );
    let lists = answer_lists(&mut recv, begun);
    assert!(
        shape(&lists).is_empty(),
        "the lists' output holds rows while the page is open: {:?}",
        shape(&lists)
    );
    let fetch = only_fetch(&lists);
    let page = pages_of(7)(&fetch.request()).1;
    let done = recv.on_fetched(fetch, page, NOW);
    assert_eq!(
        shape(&done),
        ["batch tg:77 [3, 4, 5]", "live tg:77 6"],
        "the page's batch, then the row that came before the lists"
    );

    // Without a plan (1 unread message): the last message, then the live row, in the lists' output.
    let (mut recv, begun) = synthetic_unloaded(1, 5);
    let mut early = Out::default();
    feed(&mut recv, &live(6), &mut early);
    assert!(
        shape(&early).is_empty(),
        "a live row before the lists: {:?}",
        shape(&early)
    );
    let lists = answer_lists(&mut recv, begun);
    assert!(lists.fetch.is_empty(), "no page for one unread message");
    assert_eq!(
        shape(&lists),
        ["batch tg:77 [5]", "live tg:77 6"],
        "the last message first, then the held row"
    );
    assert!(recv.sync_done());
    assert!(!recv.unsettled());
}

/// U4 (durability F6, honesty LOW-3): a held live row the batch delivered is not shipped a second
/// time as a live message. With a page plan: the chat's told last message (5) and a message inside
/// the page (4) came live before the lists; the batch holds both once and only the row above
/// (6) follows as live. Without a plan (one unread message): the last message (5) came live too,
/// the lists' output holds it once. (Mutant: the skip dropped: `live tg:77 4` and `live tg:77 5`
/// follow the batch, two repeats in a normal run.)
#[test]
fn a_held_row_the_batch_delivered_is_not_shipped_again() {
    let (mut recv, begun) = synthetic_unloaded(3, 5);
    let mut early = Out::default();
    for id in [4, 5, 6] {
        feed(&mut recv, &live(id), &mut early);
    }
    assert!(shape(&early).is_empty(), "a live row before the lists");
    let lists = answer_lists(&mut recv, begun);
    let fetch = only_fetch(&lists);
    let page = pages_of(7)(&fetch.request()).1;
    let done = recv.on_fetched(fetch, page, NOW);
    assert_eq!(
        shape(&done),
        ["batch tg:77 [3, 4, 5]", "live tg:77 6"],
        "a row the batch delivered came again as a live message"
    );
    assert!(!recv.unsettled());

    let (mut recv, begun) = synthetic_unloaded(1, 5);
    let mut early = Out::default();
    for id in [5, 6] {
        feed(&mut recv, &live(id), &mut early);
    }
    let lists = answer_lists(&mut recv, begun);
    assert_eq!(
        shape(&lists),
        ["batch tg:77 [5]", "live tg:77 6"],
        "the last message came again as a live message"
    );
    assert!(!recv.unsettled());
}

/// A chat first seen AFTER the fresh-link load has run is a normal live chat: its rows are not held
/// (critic H1 (a)).
#[test]
fn a_chat_first_seen_after_the_lists_is_a_normal_live_chat() {
    let (mut recv, _tape, lists) = synthetic(0, 5);
    assert!(lists.fetch.is_empty());
    assert!(recv.sync_done());
    let mut out = Out::default();
    let chat = json!({"@type": "updateNewChat", "chat": {"@type": "chat", "id": 78,
        "type": {"@type": "chatTypePrivate", "user_id": 78}, "title": "Fay Sample",
        "positions": [{"@type": "chatPosition", "list": {"@type": "chatListMain"},
            "order": "99", "is_pinned": false}],
        "last_message": null, "unread_count": 0,
        "last_read_inbox_message_id": 0, "last_read_outbox_message_id": 0}});
    for v in [user(78), chat] {
        feed(&mut recv, &v, &mut out);
    }
    let mut live = Out::default();
    feed(
        &mut recv,
        &json!({"@type": "updateNewMessage", "message": message(78, 3, false)}),
        &mut live,
    );
    assert_eq!(
        shape(&live),
        ["live tg:78 3"],
        "a held row of a chat first seen late"
    );
}

/// A refused page (or a short page followed by a refusal) delivers what it has plus the last
/// message, in one batch (brief section 4 point 2); the held live rows follow and the start sync
/// ends. The unread rows without a page stay the chat's hint.
#[test]
fn a_refused_fresh_page_delivers_the_last_message_alone() {
    let (mut recv, _tape, lists) = synthetic(3, 5);
    let fetch = only_fetch(&lists);
    let mut held = Out::default();
    feed(&mut recv, &live(6), &mut held);
    let done = recv.on_fetched(fetch, Err(LinkError::Td { code: 500 }), NOW);
    assert_eq!(
        shape(&done),
        ["batch tg:77 [5]", "live tg:77 6"],
        "the last message alone, then the held row"
    );
    assert!(done.fetch.is_empty(), "a refused page was asked again");
    assert!(recv.sync_done());

    // A short page, then a refusal of the next one: the short page's rows and the last message.
    let (mut recv, _tape, lists) = synthetic(10, 30);
    let fetch = only_fetch(&lists);
    let page = pages_of(2)(&fetch.request()).1;
    let next = recv.on_fetched(fetch, page, NOW);
    assert!(
        shape(&next).is_empty(),
        "rows before the plan was complete: {:?}",
        shape(&next)
    );
    let next_fetch = only_fetch(&next);
    let done = recv.on_fetched(next_fetch, Err(LinkError::Td { code: 500 }), NOW);
    assert_eq!(shape(&done), ["batch tg:77 [28, 29, 30]"]);
    assert!(recv.sync_done());
}

/// Past `WAITING_PER_CHAT` held rows the lost-ids rule applies (critic M5): the highest ids are
/// dropped and remembered, and the first row released opens a `snapshot` gap over them from the
/// lowest dropped id (the chat has no stored cursor).
#[test]
fn held_rows_past_the_bound_keep_the_lost_rule() {
    use crate::receive::messages::WAITING_PER_CHAT;
    let (mut recv, _tape, lists) = synthetic(3, 5);
    let fetch = only_fetch(&lists);
    let mut held = Out::default();
    let bound = i64::try_from(WAITING_PER_CHAT).expect("small");
    for id in 6..6 + bound + 2 {
        feed(&mut recv, &live(id), &mut held);
    }
    assert!(shape(&held).is_empty());
    let page = pages_of(7)(&fetch.request()).1;
    let done = recv.on_fetched(fetch, page, NOW);
    let shape = shape(&done);
    assert_eq!(
        shape.len(),
        1 + WAITING_PER_CHAT,
        "the batch and the held rows"
    );
    assert_eq!(shape.last().map(String::as_str), Some("live tg:77 69"));
    let lowest = (6 + bound) * STEP;
    let highest = (7 + bound) * STEP;
    assert!(
        done.staged.gaps.iter().any(|g| matches!(g,
            crate::store::GapChange::Open { chat_id: 77, above, below, cause }
                if *above == lowest - 1 && *below == highest + 1
                    && *cause == crate::store::GapCause::Snapshot)),
        "no snapshot gap over the dropped ids: {:?}",
        done.staged.gaps
    );
}

/// U3 (durability F7): the hold's overflow BELOW the chat's last message. A busy fresh chat gets
/// live ids 1..79 while the lists load (its last message is 100, 30 unread, so the page is
/// 80..99); the hold keeps 64 and remembers the rest (65..79) as lost. Every replayed row is
/// older than the batch's last message, so it takes the history branch (TF2), which never
/// reaches the lost-ids code: the batch's own output must carry the snapshot gap over the
/// dropped ids, and the machines must be settled after it (a gap that lived only in memory died
/// with a run that ended first, leaving a silent hole between delivered rows). (Mutant: the
/// post-replay `lost_gap` call dropped: no gap in the batch, the lost range stays in memory.)
#[test]
fn held_rows_past_the_bound_below_the_last_message_open_their_gap_in_the_batch() {
    use crate::receive::messages::WAITING_PER_CHAT;
    let (mut recv, _tape, lists) = synthetic(30, 100);
    let fetch = only_fetch(&lists);
    let bound = i64::try_from(WAITING_PER_CHAT).expect("small");
    let top = bound + 15;
    let mut held = Out::default();
    for id in 1..=top {
        feed(&mut recv, &live(id), &mut held);
    }
    assert!(shape(&held).is_empty(), "a held row reached a commit early");
    assert!(
        recv.unsettled(),
        "the hold and the dropped ids left the run settled"
    );
    let page = pages_of(20)(&fetch.request()).1;
    let done = recv.on_fetched(fetch, page, NOW);
    // The batch: the page's rows 80..99 and the last message, then the 64 held rows (history
    // rows, below the last message: none is a live message).
    let ids: Vec<i64> = history(&done).0.values().flatten().copied().collect();
    assert_eq!(
        history(&done).1,
        0,
        "a held row was shipped as a live message"
    );
    assert!(
        (1..=bound).all(|id| ids.contains(&id)) && (80..=100).all(|id| ids.contains(&id)),
        "the batch lost a row: {ids:?}"
    );
    let gaps: Vec<_> = done
        .staged
        .gaps
        .iter()
        .filter(|g| matches!(g, crate::store::GapChange::Open { chat_id: 77, .. }))
        .collect();
    assert_eq!(gaps.len(), 1, "one gap over the dropped ids: {gaps:?}");
    assert!(
        matches!(gaps[0], crate::store::GapChange::Open { above, below, cause, .. }
            if *above == (bound + 1) * STEP - 1 && *below == top * STEP + 1
                && *cause == crate::store::GapCause::Snapshot),
        "the gap does not cover the dropped ids {}..{}: {:?}",
        bound + 1,
        top,
        gaps[0]
    );
    assert!(
        !recv.unsettled(),
        "the dropped ids are still only in memory after the batch"
    );
    assert!(recv.sync_done());
}

/// U3's other path, `end_lists_hold`: the same overflow below the chat's last message, but the
/// chat has ONE unread message (the last one), so no page is planned: the lists' load delivers the
/// last message at once and the end of the lists' hold replays the held rows into that output. The
/// snapshot gap over the dropped ids must be in it, and nothing may stay in memory. (Mutant, the
/// re-check's RK-U3b: `end_lists_hold` clears the replayed output's gaps.)
#[test]
fn held_rows_past_the_bound_below_the_last_message_open_their_gap_without_a_plan() {
    use crate::receive::messages::WAITING_PER_CHAT;
    let (mut recv, begun) = synthetic_unloaded(1, 100);
    let bound = i64::try_from(WAITING_PER_CHAT).expect("small");
    let top = bound + 15;
    let mut held = Out::default();
    for id in 1..=top {
        feed(&mut recv, &live(id), &mut held);
    }
    assert!(shape(&held).is_empty(), "a held row reached a commit early");
    assert!(
        recv.unsettled(),
        "the hold and the dropped ids left the run settled"
    );
    let lists = answer_lists(&mut recv, begun);
    // No fresh-link page (one unread message); the only ask is the reconcile's page over the gap.
    assert!(
        lists
            .fetch
            .iter()
            .all(|f| matches!(f, Fetch::Gap { chat_id: 77, .. })),
        "a fresh-link page for one unread message: {:?}",
        lists.fetch
    );
    let gaps: Vec<_> = lists
        .staged
        .gaps
        .iter()
        .filter(|g| matches!(g, crate::store::GapChange::Open { chat_id: 77, .. }))
        .collect();
    assert_eq!(gaps.len(), 1, "one gap over the dropped ids: {gaps:?}");
    assert!(
        matches!(gaps[0], crate::store::GapChange::Open { above, below, cause, .. }
            if *above == (bound + 1) * STEP - 1 && *below == top * STEP + 1
                && *cause == crate::store::GapCause::Snapshot),
        "the gap does not cover the dropped ids {}..{}: {:?}",
        bound + 1,
        top,
        gaps[0]
    );
    assert!(
        !recv.unsettled(),
        "the dropped ids are still only in memory after the lists"
    );
    assert!(recv.sync_done());
}

#[path = "tests/reconcile.rs"]
mod reconcile;
