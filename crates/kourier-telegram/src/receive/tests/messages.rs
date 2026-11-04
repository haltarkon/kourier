//! The message machine (TG-3 C6a): new messages, catch-up, the link hold and the gap triggers,
//! replayed from TDLib's recordings by name; synthetic lines only for what no recording holds (a
//! scheduled, pending or local message, a re-delivery, the clock cases). Nothing here prints a
//! recorded value.

use kourier_backend_api::IncomingMessage;
use serde_json::{Value, json};

use super::super::messages::{CATCH_UP_AGE_MS, Cursor, Messages};
use super::*;
use crate::store::{GapCause, GapChange};

const SG401: i64 = -1_000_000_000_401;
const SG402: i64 = -1_000_000_000_402;
const STEP: i64 = 1 << 20;

/// One TDLib line through both machines (the chat machine first, as the actor will).
fn feed_both(chats: &mut Chats, msgs: &mut Messages, line: &str, now_ms: i64) -> Out {
    let mut out = Out::default();
    if let Some(Update::Receive(r)) = tdapi::parse_update(line) {
        out.extend(chats.on_update(&r));
        out.extend(msgs.on_update(chats, &r, now_ms));
    }
    out
}

fn feed_lines(chats: &mut Chats, msgs: &mut Messages, lines: &[String], now_ms: i64) -> Out {
    let mut out = Out::default();
    for l in lines {
        out.extend(feed_both(chats, msgs, l, now_ms));
    }
    out
}

/// The `Message` events of `out`, in order.
fn rows(out: &Out) -> Vec<&IncomingMessage> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::Message(m) => Some(m),
            _ => None,
        })
        .collect()
}

/// The linked machines after `lines` (a recording's part before its step), the helper started at
/// `start_ms`, the start sync over.
fn machines(lines: &[String], start_ms: i64, after_unclean: bool) -> (Chats, Messages) {
    let mut chats = Chats::new(Some(ME));
    chats.set_now(start_ms);
    chats.link(ME);
    let mut msgs = Messages::new(start_ms, after_unclean);
    feed_lines(&mut chats, &mut msgs, lines, start_ms);
    msgs.start_sync_done();
    (chats, msgs)
}

/// The recording's lines of `parts` and the recorded answers they hold (Q-L11).
fn part_lines(rec: &Recording, labels: &[Option<&str>]) -> (Vec<String>, usize) {
    let parts: Vec<&Part> = labels
        .iter()
        .map(|l| match l {
            Some(label) => rec.part(label),
            None => &rec.parts[0],
        })
        .collect();
    objects(parts)
}

/// The recordings' clock: a few seconds after their last date.
const REC_NOW: i64 = 1_700_000_200_000;

/// `rec-channel-difference.jsonl`'s supergroup 402 has an active username: a PUBLIC supergroup,
/// held until T7 (H5). The trigger's test makes it mapped in memory by dropping that username
/// from its `updateSupergroup` objects; every id, date and order stays as recorded.
fn unhold_402(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            let mut v: Value = serde_json::from_str(l).expect("a recorded object");
            if v["@type"] == "updateSupergroup" && v["supergroup"]["id"] == 402 {
                v["supergroup"]["usernames"] = Value::Null;
            }
            v.to_string()
        })
        .collect()
}

/// G-8: after `channelDifferenceTooLong` TDLib streams only the newest message
/// (`rec-channel-difference.jsonl` step `too-long-402`: cursor 1, newest 6): its row and the gap
/// `(1 << 20, 6 << 20)` come in ONE batch. As recorded (402 public, so held) nothing is staged.
#[test]
fn a_supergroup_id_jump_opens_a_gap_in_the_rows_batch() {
    let rec = Recording::load("rec-channel-difference.jsonl");
    let (before, answers) = part_lines(&rec, &[None, Some("too-long-401")]);
    assert_eq!(answers, 17, "recorded answers used");
    let (step, step_answers) = part_lines(&rec, &[Some("too-long-402")]);
    assert_eq!(step_answers, 0, "the step holds no request");

    // As recorded: the public supergroup is held, its message is no row and opens no gap.
    let (mut chats, mut msgs) = machines(&before, REC_NOW, false);
    assert_eq!(chats.held(SG402), Some(Held::Public));
    msgs.seed(
        SG402,
        Cursor {
            delivered_max: STEP,
            verified_max: STEP,
        },
    );
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    assert!(
        rows(&out).is_empty() && out.staged.gaps.is_empty(),
        "a held chat staged"
    );

    // Mapped: the jump opens the gap in the row's batch.
    let (mut chats, mut msgs) = machines(&unhold_402(&before), REC_NOW, false);
    assert!(
        chats.conv(SG402).is_some(),
        "402 is mapped once its username is gone"
    );
    msgs.seed(
        SG402,
        Cursor {
            delivered_max: STEP,
            verified_max: STEP,
        },
    );
    let out = feed_lines(&mut chats, &mut msgs, &unhold_402(&step), REC_NOW);
    let got = rows(&out);
    assert_eq!(got.len(), 1, "the newest message is one row");
    assert_eq!(&*got[0].local, (6 * STEP).to_string());
    assert_eq!(
        out.staged.gaps,
        [GapChange::Open {
            chat_id: SG402,
            above: STEP,
            below: 6 * STEP,
            cause: GapCause::IdJump,
        }]
    );
    assert_eq!(msgs.cursor(SG402).delivered_max, 6 * STEP);
    // The trigger needs no state beyond the cursor: a second jump of the next message is none.
    let printed = format!("{out:?}");
    assert!(!printed.contains("synthetic text"), "Debug printed a text");
}

/// G-8: a supergroup's stream in order opens no gap (`rec-channel-difference.jsonl` step
/// `too-long-401`: 2..6 after the cursor 1; `rec-gap-channel.jsonl` step `push-gap`: 2..4 after 1).
#[test]
fn an_in_order_channel_stream_opens_no_gap() {
    let rec = Recording::load("rec-channel-difference.jsonl");
    let (before, answers) = part_lines(&rec, &[None]);
    assert_eq!(answers, 17, "recorded answers used");
    let (step, _) = part_lines(&rec, &[Some("too-long-401")]);
    let (mut chats, mut msgs) = machines(&before, REC_NOW, false);
    msgs.seed(
        SG401,
        Cursor {
            delivered_max: STEP,
            verified_max: STEP,
        },
    );
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    let locals: Vec<String> = rows(&out).iter().map(|m| m.local.to_string()).collect();
    let want: Vec<String> = (2..=6).map(|n| (n * STEP).to_string()).collect();
    assert_eq!(locals, want);
    assert!(
        out.staged.gaps.is_empty(),
        "an in-order stream opened a gap"
    );

    let rec = Recording::load("rec-gap-channel.jsonl");
    let (before, answers) = part_lines(&rec, &[None]);
    assert_eq!(answers, 3, "recorded answers used");
    let (step, _) = part_lines(&rec, &[Some("push-gap")]);
    let (mut chats, mut msgs) = machines(&before, REC_NOW, false);
    msgs.seed(
        SG401,
        Cursor {
            delivered_max: STEP,
            verified_max: STEP,
        },
    );
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    assert_eq!(rows(&out).len(), 3);
    assert!(
        out.staged.gaps.is_empty(),
        "an in-order stream opened a gap"
    );
}

/// B1: private chats share one id box, so an id jump says nothing in a clean run
/// (`rec-gap-known-sender.jsonl`, `rec-gap-filled-by-push.jsonl`: chat 201 at 4, then 9, 10, 11).
/// Also each row's events: the author's contact, the chat's patch (restated first), the message.
#[test]
fn a_box_wide_id_jump_in_a_clean_run_opens_no_gap() {
    for name in ["rec-gap-known-sender.jsonl", "rec-gap-filled-by-push.jsonl"] {
        let rec = Recording::load(name);
        let (before, answers) = part_lines(&rec, &[None]);
        assert_eq!(answers, 3, "{name}: recorded answers used");
        let (step, _) = part_lines(&rec, &[Some("push-gap")]);
        let (mut chats, mut msgs) = machines(&before, REC_NOW, false);
        msgs.seed(
            201,
            Cursor {
                delivered_max: 4 * STEP,
                verified_max: 4 * STEP,
            },
        );
        let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
        assert_eq!(rows(&out).len(), 3, "{name}");
        assert!(
            out.staged.gaps.is_empty(),
            "{name}: a clean run opened a gap"
        );
        let first = &out.staged.rows[0];
        let kinds: Vec<&str> = first.events.iter().map(|e| e.event.name()).collect();
        assert_eq!(
            kinds,
            ["ContactUpsert", "ConversationUpsert", "Message"],
            "{name}"
        );
        assert_eq!(first.events[1].gate, Some(Gate::Restate(201)));
        assert_eq!(
            first
                .delivered
                .iter()
                .map(|d| (d.chat_id, d.msg_id, d.outgoing))
                .collect::<Vec<_>>(),
            [(201, 9 * STEP, false)]
        );
        assert_eq!(first.delivered[0].author, "tg:201");
        // verified_max follows every delivered message.
        assert_eq!(
            msgs.cursor(201),
            Cursor {
                delivered_max: 11 * STEP,
                verified_max: 11 * STEP
            }
        );
        let m = rows(&out)[0];
        assert_eq!(m.conv, direct(201));
        assert!(!m.catch_up && !m.read && m.server_at == Some(m.sent_at));
    }
}

/// B1: in a run after an UNCLEAN end, the first live message of a private chat opens the check
/// gap `(verified_max, id)` in its row's batch; the next message of that chat opens none
/// (`rec-gap-known-sender.jsonl`, the same stream as the clean case).
#[test]
fn after_an_unclean_end_the_first_live_message_opens_a_check_gap() {
    let rec = Recording::load("rec-gap-known-sender.jsonl");
    let (before, answers) = part_lines(&rec, &[None]);
    assert_eq!(answers, 3, "recorded answers used");
    let (step, _) = part_lines(&rec, &[Some("push-gap")]);
    let (mut chats, mut msgs) = machines(&before, REC_NOW, true);
    msgs.seed(
        201,
        Cursor {
            delivered_max: 4 * STEP,
            verified_max: 4 * STEP,
        },
    );
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    assert_eq!(rows(&out).len(), 3);
    assert_eq!(
        out.staged.gaps,
        [GapChange::Open {
            chat_id: 201,
            above: 4 * STEP,
            below: 9 * STEP,
            cause: GapCause::UncleanEnd,
        }]
    );
}

// --- synthetic cases (no recording holds them) ----------------------------------------------

/// The synthetic helper start (ms) and chat.
const START: i64 = 1_700_000_000_000;
const PEER: i64 = 4242;

/// A linked machine pair knowing the private chat [`PEER`] (and its user), started at [`START`],
/// the start sync still running.
fn synthetic() -> (Chats, Messages) {
    let mut chats = Chats::new(Some(ME));
    chats.set_now(START);
    chats.link(ME);
    feed(&mut chats, &user_line(PEER, "", false));
    feed(
        &mut chats,
        &chat_line(PEER, "chatListMain", 1, false, (true, 0), (0, 0)),
    );
    (chats, Messages::new(START, false))
}

/// An `updateNewMessage` of [`PEER`] with `id`, `date` (s) and extra message fields.
fn message_line(id: i64, date_s: i64, extra: Value) -> String {
    let mut m = json!({
        "@type": "message",
        "id": id,
        "chat_id": PEER,
        "sender_id": {"@type": "messageSenderUser", "user_id": PEER},
        "date": date_s,
        "can_be_saved": true,
        "content": {"@type": "messageText", "text": {"@type": "formattedText", "text": "synthetic text 1", "entities": []}},
    });
    if let (Some(m), Value::Object(extra)) = (m.as_object_mut(), extra) {
        m.extend(extra);
    }
    json!({"@type": "updateNewMessage", "message": m}).to_string()
}

fn one(chats: &mut Chats, msgs: &mut Messages, line: &str) -> Out {
    feed_both(chats, msgs, line, START + 1000)
}

/// H2: every message until the start sync ends is catch-up (TDLib streams a supergroup's backlog
/// while it says Ready); after it only `Updating` or the date rule make catch-up.
#[test]
fn the_backlog_until_the_start_sync_ends_is_catch_up() {
    let (mut chats, mut msgs) = synthetic();
    let now_s = START / 1000;
    let during = one(&mut chats, &mut msgs, &message_line(STEP, now_s, json!({})));
    assert!(rows(&during)[0].catch_up, "a message during the start sync");
    msgs.start_sync_done();
    let live = one(
        &mut chats,
        &mut msgs,
        &message_line(2 * STEP, now_s, json!({})),
    );
    assert!(
        !rows(&live)[0].catch_up,
        "a live message after the start sync"
    );
    msgs.set_updating(true);
    let updating = one(
        &mut chats,
        &mut msgs,
        &message_line(3 * STEP, now_s, json!({})),
    );
    assert!(
        rows(&updating)[0].catch_up,
        "a message while TDLib says Updating"
    );
}

/// Q-L7: a message dated more than five minutes before the helper's start is catch-up whatever
/// the state.
#[test]
fn an_old_message_is_catch_up_whatever_the_state() {
    let (mut chats, mut msgs) = synthetic();
    msgs.start_sync_done();
    let age_s = CATCH_UP_AGE_MS / 1000;
    let old = one(
        &mut chats,
        &mut msgs,
        &message_line(STEP, START / 1000 - age_s - 1, json!({})),
    );
    assert!(rows(&old)[0].catch_up, "an old message");
    let recent = one(
        &mut chats,
        &mut msgs,
        &message_line(2 * STEP, START / 1000 - age_s + 1, json!({})),
    );
    assert!(!rows(&recent)[0].catch_up, "a message inside the window");
}

/// L5: a scheduled message, one being sent from this client and an id with low bits set never
/// become rows; a server message does.
#[test]
fn scheduled_pending_and_local_ids_never_become_rows() {
    let (mut chats, mut msgs) = synthetic();
    let now_s = START / 1000;
    let scheduled = message_line(
        5 * STEP,
        now_s,
        json!({"scheduling_state": {"@type": "messageSchedulingStateSendAtDate", "send_date": now_s + 60}}),
    );
    assert!(
        one(&mut chats, &mut msgs, &scheduled).is_empty(),
        "a scheduled message"
    );
    let pending = message_line(
        5 * STEP,
        now_s,
        json!({"sending_state": {"@type": "messageSendingStatePending", "sending_id": 1}}),
    );
    assert!(
        one(&mut chats, &mut msgs, &pending).is_empty(),
        "a pending message"
    );
    let local = message_line(5 * STEP + 1, now_s, json!({}));
    assert!(one(&mut chats, &mut msgs, &local).is_empty(), "a local id");
    assert_eq!(msgs.dropped(), 3);
    let server = one(
        &mut chats,
        &mut msgs,
        &message_line(5 * STEP, now_s, json!({})),
    );
    assert_eq!(rows(&server).len(), 1, "a server message");
}

/// The link hold (critic H1): before the link a message is no row (the fresh-link load takes it
/// as history); after it the same message is one.
#[test]
fn nothing_is_staged_before_the_link() {
    let mut chats = Chats::new(Some(ME));
    feed(&mut chats, &user_line(PEER, "", false));
    feed(
        &mut chats,
        &chat_line(PEER, "chatListMain", 1, false, (true, 0), (0, 0)),
    );
    let mut msgs = Messages::new(START, false);
    let line = message_line(STEP, START / 1000, json!({}));
    assert!(
        one(&mut chats, &mut msgs, &line).is_empty(),
        "staged before the link"
    );
    chats.link(ME);
    assert_eq!(rows(&one(&mut chats, &mut msgs, &line)).len(), 1);
}

/// RC3: a kill re-delivers a message at most once (through the difference): the copy is the SAME
/// row (same conversation, `local` and author), so the core's idempotent ingest drops it; the
/// cursor does not move and no gap opens.
#[test]
fn a_redelivery_is_the_same_row() {
    let (mut chats, mut msgs) = synthetic();
    msgs.seed(
        PEER,
        Cursor {
            delivered_max: STEP,
            verified_max: STEP,
        },
    );
    let line = message_line(2 * STEP, START / 1000, json!({}));
    let first = one(&mut chats, &mut msgs, &line);
    let again = one(&mut chats, &mut msgs, &line);
    let (a, b) = (rows(&first)[0], rows(&again)[0]);
    assert_eq!(
        (&a.conv, &a.local, &a.origin),
        (&b.conv, &b.local, &b.origin)
    );
    assert_eq!(&*a.local, (2 * STEP).to_string());
    assert_eq!(
        first.staged.rows[0].delivered,
        again.staged.rows[0].delivered
    );
    assert!(again.staged.gaps.is_empty());
    assert_eq!(msgs.cursor(PEER).delivered_max, 2 * STEP);
}

/// L7 and the reply: an outgoing message (another own device's) is `OwnOtherDevice` with
/// `initial_out_state: Sent`, no author contact, its delivered author the own id; a reply to a
/// message of the same chat targets it by `local`.
#[test]
fn an_own_message_from_another_device_is_sent() {
    let (mut chats, mut msgs) = synthetic();
    let line = message_line(
        3 * STEP,
        START / 1000,
        json!({"is_outgoing": true, "sender_id": {"@type": "messageSenderUser", "user_id": ME},
               "reply_to": {"@type": "messageReplyToMessage", "chat_id": PEER, "message_id": STEP}}),
    );
    let out = one(&mut chats, &mut msgs, &line);
    let m = rows(&out)[0];
    assert_eq!(
        m.origin,
        kourier_model::Origin::OwnOtherDevice { device: None }
    );
    assert!(matches!(
        m.initial_out_state,
        Some(kourier_model::OutgoingState::Sent { server_at: Some(_) })
    ));
    let reply = m.reply_to.as_ref().expect("a reply");
    assert_eq!(&*reply.target.local, STEP.to_string());
    assert_eq!(reply.target.conv, Some(direct(PEER)));
    let row = &out.staged.rows[0];
    assert!(row.events.iter().all(|e| e.event.name() != "ContactUpsert"));
    assert_eq!(row.delivered[0].author, format!("tg:{ME}"));
    assert!(row.delivered[0].outgoing);
}

/// H1: a message of a chat the machine never learnt asks `getChat` once and waits for the
/// answer; the answer brings its row.
#[test]
fn a_message_of_an_unknown_chat_waits_for_its_chat() {
    let (mut chats, mut msgs) = synthetic();
    let mut line: Value =
        serde_json::from_str(&message_line(STEP, START / 1000, json!({}))).unwrap();
    line["message"]["chat_id"] = json!(77);
    line["message"]["sender_id"]["user_id"] = json!(77);
    let out = one(&mut chats, &mut msgs, &line.to_string());
    assert!(out.staged.rows.is_empty());
    assert_eq!(out.fetch, [Fetch::Chat(77)]);
    feed(&mut chats, &user_line(77, "", false));
    let chat: Value =
        serde_json::from_str(&chat_line(77, "chatListMain", 2, false, (true, 0), (0, 0))).unwrap();
    let chat: crate::tdapi::Chat = serde_json::from_value(chat["chat"].clone()).expect("a chat");
    chats.on_chat(chat);
    let out = msgs.on_chat(&mut chats, 77, START + 2000);
    assert_eq!(rows(&out).len(), 1);
    assert!(
        msgs.on_chat(&mut chats, 77, START + 3000).is_empty(),
        "replayed twice"
    );
}

/// An `updateNewMessage` of the chat 77, which the machines never learnt.
fn unknown_line(n: i64) -> String {
    let mut line: Value =
        serde_json::from_str(&message_line(n * STEP, START / 1000, json!({}))).unwrap();
    line["message"]["chat_id"] = json!(77);
    line["message"]["sender_id"]["user_id"] = json!(77);
    line.to_string()
}

/// `getChat`'s answer for chat 77, as the chat machine takes it.
fn chat_77() -> crate::tdapi::Chat {
    let chat: Value =
        serde_json::from_str(&chat_line(77, "chatListMain", 2, false, (true, 0), (0, 0))).unwrap();
    serde_json::from_value(chat["chat"].clone()).expect("a chat")
}

/// TF3 (review dF3, item 1): 70 messages of a chat the machine never learnt: 64 wait, 6 are
/// past the bound. The answer delivers the 64, and the first row carries a gap over the dropped
/// ids, so the reconcile pages them (a never-delivered chat: from the lowest dropped id).
/// (Mutant: the old silent drop.)
#[test]
fn an_unknown_chats_overflow_leaves_a_gap() {
    let (mut chats, mut msgs) = synthetic();
    for n in 1..=70 {
        one(&mut chats, &mut msgs, &unknown_line(n));
    }
    assert!(msgs.unsettled(), "a waiting list is no clean end");
    feed(&mut chats, &user_line(77, "", false));
    chats.on_chat(chat_77());
    let out = msgs.on_chat(&mut chats, 77, START + 2000);
    assert_eq!(rows(&out).len(), 64, "the waiting messages");
    assert_eq!(
        out.staged.gaps,
        [GapChange::Open {
            chat_id: 77,
            above: 65 * STEP - 1,
            below: 70 * STEP + 1,
            cause: GapCause::Snapshot,
        }],
        "one gap over 65..70"
    );
    assert!(!msgs.unsettled(), "the gap is staged");
    let again = msgs.on_chat(&mut chats, 77, START + 3000);
    assert!(again.staged.gaps.is_empty(), "the gap opens once");
}

/// TF3 (item 2): a failed `getChat` drops the waiting list but remembers its ids; the next
/// message of the chat asks once more (three asks per chat and helper run); the chat's first row
/// opens the gap over what was dropped. (Mutant: the id stays in `asked`.)
#[test]
fn a_failed_get_chat_is_asked_again_and_what_it_dropped_gets_a_gap() {
    let (mut chats, mut msgs) = synthetic();
    for n in 1..=2 {
        let out = one(&mut chats, &mut msgs, &unknown_line(n));
        assert_eq!(out.fetch, [Fetch::Chat(77)], "ask {n}");
        chats.on_chat_failed(77);
        msgs.on_chat_failed(77);
        assert!(msgs.unsettled(), "dropped ids are remembered");
    }
    let out = one(&mut chats, &mut msgs, &unknown_line(3));
    assert_eq!(out.fetch, [Fetch::Chat(77)], "the third ask");
    feed(&mut chats, &user_line(77, "", false));
    chats.on_chat(chat_77());
    let out = msgs.on_chat(&mut chats, 77, START + 2000);
    assert_eq!(rows(&out).len(), 1, "message 3");
    assert_eq!(
        out.staged.gaps,
        [GapChange::Open {
            chat_id: 77,
            above: STEP - 1,
            below: 2 * STEP + 1,
            cause: GapCause::Snapshot,
        }],
        "a gap over 1..2"
    );
    assert!(!msgs.unsettled());

    // A fourth ask is not made: three failures end the asking for this helper run.
    let (mut chats, mut msgs) = synthetic();
    for n in 1..=3 {
        let out = one(&mut chats, &mut msgs, &unknown_line(n));
        assert_eq!(out.fetch, [Fetch::Chat(77)], "ask {n}");
        chats.on_chat_failed(77);
        msgs.on_chat_failed(77);
    }
    let out = one(&mut chats, &mut msgs, &unknown_line(4));
    assert!(out.fetch.is_empty(), "a fourth ask");
    assert!(msgs.unsettled());
}

/// D4 of the re-check (TF3): 70 messages of a chat the machine never learnt remember 6 dropped
/// ids; the chat then turns out HELD (a restricted user) or SECRET, so it never delivers a row and
/// no row would ever open the gap. The memory is cleared when the waiting messages are replayed
/// against the decision: a stop now counts clean. (Mutant: `lost` kept for a chat without rows.)
#[test]
fn ids_dropped_for_a_chat_decided_held_or_secret_leave_nothing_unsettled() {
    for secret in [false, true] {
        let (mut chats, mut msgs) = synthetic();
        for n in 1..=70 {
            one(&mut chats, &mut msgs, &unknown_line(n));
        }
        assert!(
            msgs.unsettled(),
            "waiting and dropped ids (secret: {secret})"
        );
        let mut chat: Value =
            serde_json::from_str(&chat_line(77, "chatListMain", 2, false, (true, 0), (0, 0)))
                .unwrap();
        if secret {
            feed(&mut chats, &user_line(77, "", false));
            chat["chat"]["type"] =
                json!({"@type": "chatTypeSecret", "secret_chat_id": 5, "user_id": 77});
        } else {
            feed(
                &mut chats,
                r#"{"@type":"updateUser","user":{"@type":"user","id":77,"first_name":"Fay","last_name":"Sample","phone_number":"","is_contact":false,"type":{"@type":"userTypeRegular"},"restriction_info":{"@type":"restrictionInfo","restriction_reason":"synthetic-restriction","has_sensitive_content":false}}}"#,
            );
        }
        chats.on_chat(serde_json::from_value(chat["chat"].clone()).expect("a chat"));
        assert!(
            chats.conv(77).is_none(),
            "the chat is not mapped (secret: {secret})"
        );
        let out = msgs.on_chat(&mut chats, 77, START + 2000);
        assert!(rows(&out).is_empty(), "a row of a chat without rows");
        assert!(
            out.staged.gaps.is_empty(),
            "a gap for a chat that delivers nothing"
        );
        assert!(
            !msgs.unsettled(),
            "a held or secret chat never opens the gap (secret: {secret})"
        );
    }
}

/// TF4 (review dF4, item b): a KNOWN chat still undecided (its user not told: `getUser` is
/// asked) holds its messages like an unknown one; the user's arrival decides the chat and the
/// waiting message becomes its row. (Mutant: the old drop.)
#[test]
fn an_undecided_chats_messages_wait_for_the_decision() {
    let (mut chats, mut msgs) = synthetic();
    // Chat 78 is told, its user is not.
    let out = feed(
        &mut chats,
        &chat_line(78, "chatListMain", 3, false, (true, 0), (0, 0)),
    );
    assert_eq!(out.fetch, [Fetch::User(78)]);
    assert!(chats.deciding(78));
    let mut line: Value =
        serde_json::from_str(&message_line(STEP, START / 1000, json!({}))).unwrap();
    line["message"]["chat_id"] = json!(78);
    line["message"]["sender_id"]["user_id"] = json!(78);
    let out = one(&mut chats, &mut msgs, &line.to_string());
    assert!(out.staged.rows.is_empty() && out.fetch.is_empty());
    assert!(msgs.unsettled(), "a waiting message");
    assert!(
        msgs.release_decided(&mut chats, START + 1500).is_empty(),
        "released before the decision"
    );
    feed(&mut chats, &user_line(78, "", false));
    assert!(!chats.deciding(78));
    let out = msgs.release_decided(&mut chats, START + 2000);
    assert_eq!(rows(&out).len(), 1, "the waiting message");
    assert!(!msgs.unsettled());
}

// ----- C6b: edits, deletes, reads, typing -----

use kourier_model::{MsgTarget, ReceiptKind, TargetAuthor, UserId};

use super::super::messages::TYPING_TTL;
use crate::store::{Delivered, Parked};
use crate::tdapi::Message;

const SG403: i64 = -1_000_000_000_403;

fn uid(id: i64) -> UserId {
    UserId::from_parts("tg", &id.to_string()).expect("a user id")
}

/// What C7a seeds from `delivered` (or a history path notes).
fn note(msgs: &mut Messages, chat_id: i64, msg_id: i64, author: i64, outgoing: bool) {
    msgs.note_delivered(&Delivered {
        chat_id,
        msg_id,
        author: format!("tg:{author}"),
        outgoing,
    });
}

/// Every `Edited` of `out`: (target local, target author, by, edited_at), and the bodies.
type EditKey = (String, TargetAuthor, Option<UserId>, i64);
fn edits(out: &Out) -> (Vec<EditKey>, Vec<kourier_model::Body>) {
    let mut keys = Vec::new();
    let mut bodies = Vec::new();
    for e in out.staged.rows.iter().flat_map(|r| &r.events) {
        if let BackendEvent::Edited {
            target,
            by,
            body,
            edited_at,
            new_local,
            ..
        } = &e.event
        {
            assert!(new_local.is_none(), "an edit has no new local");
            keys.push((
                target.local.to_string(),
                target.author.clone(),
                by.clone(),
                *edited_at,
            ));
            bodies.push(body.clone());
        }
    }
    (keys, bodies)
}

/// The recorded `getMessage` answer of a part, as TDLib's `message`.
fn recorded_message(part: &Part) -> Message {
    let r = part
        .requests()
        .find(|r| r.ty == "getMessage")
        .expect("the part's getMessage");
    serde_json::from_value(r.answer().clone()).expect("a message")
}

/// The body TDLib's `message` holds now (the edit's content must be it).
fn body_of(msg: &Message) -> kourier_model::Body {
    crate::content::content(
        &msg.content,
        crate::markup::Me {
            id: ME,
            usernames: &[],
        },
    )
    .body
}

/// M1: TDLib sends `updateMessageContent` then `updateMessageEdited`; the `Edited` comes with the
/// second, carrying the first's content (`rec-edit.jsonl` steps `edit-loaded-u201`,
/// `edit-loaded-c401`). Its author comes from the delivered messages; without one ONE
/// `getMessage` asks it (the recorded answer).
#[test]
fn a_loaded_edit_pairs_content_then_edited() {
    let rec = Recording::load("rec-edit.jsonl");
    let (base, base_answers) = part_lines(&rec, &[None]);
    assert_eq!(base_answers, 6, "recorded answers used (the loadChats)");
    let cases = [
        (
            "edit-loaded-u201",
            201_i64,
            4 * STEP,
            201_i64,
            1_700_000_098_000_i64,
        ),
        ("edit-loaded-c401", SG401, 3 * STEP, 203, 1_700_000_100_000),
    ];
    let mut answers = 0;
    for (label, chat, id, author, at) in cases {
        let part = rec.part(label);
        let (step, _) = objects([part]);
        let want = (
            id.to_string(),
            TargetAuthor::User(uid(author)),
            Some(uid(author)),
            at,
        );

        // Remembered: the pair alone makes the edit, no request; nothing before the second update.
        let (mut chats, mut msgs) = machines(&base, REC_NOW, false);
        note(&mut msgs, chat, id, author, false);
        let mut out = Out::default();
        let mut seen_edited = false;
        for line in &step {
            let o = feed_both(&mut chats, &mut msgs, line, REC_NOW);
            let is_edited = line.contains("\"updateMessageEdited\"");
            assert!(
                edits(&o).0.is_empty() || is_edited,
                "{label}: an Edited before its updateMessageEdited"
            );
            seen_edited |= is_edited;
            out.extend(o);
        }
        assert!(seen_edited, "{label}: the step holds the edit");
        let (keys, bodies) = edits(&out);
        assert_eq!(keys, std::slice::from_ref(&want), "{label}: one edit");
        assert!(
            out.fetch.is_empty(),
            "{label}: a remembered author asks nothing"
        );
        let answer = recorded_message(part);
        assert!(
            bodies[0] == body_of(&answer),
            "{label}: the edit carries the paired content (TDLib's message holds it)"
        );

        // Not remembered: ONE getMessage, the recorded answer gives the author.
        let (mut chats, mut msgs) = machines(&base, REC_NOW, false);
        let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
        assert!(
            edits(&out).0.is_empty(),
            "{label}: no edit before the answer"
        );
        assert_eq!(
            out.fetch,
            [Fetch::Message {
                chat_id: chat,
                msg_id: id
            }]
        );
        let asked: Vec<Value> = out.fetch.iter().map(|f| f.request()).collect();
        let used = crate::replay::used([part], &asked);
        assert_eq!(used, 1, "{label}: the recorded answer of the one request");
        answers += used;
        let out = msgs.on_message(&chats, &answer, REC_NOW);
        let (keys, bodies) = edits(&out);
        assert_eq!(keys, [want], "{label}: the answered edit");
        assert!(
            bodies[0] == body_of(&answer),
            "{label}: the answer's content"
        );
        assert!(
            edits(&msgs.on_message(&chats, &answer, REC_NOW))
                .0
                .is_empty(),
            "{label}: an answer is used once"
        );
    }
    assert_eq!(answers, 2, "recorded answers used (the getMessage)");
}

/// G-8 (RC2, RC7): an edit of a message TDLib does not hold reaches the app as nothing; the
/// machine makes nothing of those steps and asks nothing (`rec-edit.jsonl` unloaded steps).
#[test]
fn unloaded_edits_bring_nothing_and_ask_nothing() {
    let rec = Recording::load("rec-edit.jsonl");
    let (base, _) = part_lines(&rec, &[None]);
    let (step, answers) = objects([
        rec.part("edit-unloaded-u201"),
        rec.part("edit-unloaded-c401"),
    ]);
    assert_eq!(answers, 0, "the unloaded steps hold no request");
    for remembered in [false, true] {
        let (mut chats, mut msgs) = machines(&base, REC_NOW, false);
        if remembered {
            note(&mut msgs, 201, 2 * STEP, 201, false);
            note(&mut msgs, SG402, STEP, 203, false);
        }
        let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
        assert!(edits(&out).0.is_empty(), "an unloaded edit made an Edited");
        assert!(out.fetch.is_empty(), "an unloaded edit asked something");
        assert!(out.staged.rows.is_empty() && out.staged.parked.is_empty());
    }
}

fn content_line(chat: i64, id: i64) -> String {
    json!({"@type": "updateMessageContent", "chat_id": chat, "message_id": id,
        "new_content": {"@type": "messageText", "text": {"@type": "formattedText", "text": "synthetic text 2", "entities": []}}})
    .to_string()
}

fn edited_line(chat: i64, id: i64) -> String {
    json!({"@type": "updateMessageEdited", "chat_id": chat, "message_id": id, "edit_date": 1_700_000_001})
        .to_string()
}

/// M1: an `updateMessageEdited` without a content update (buttons, the date only) is no edit, and
/// a content update alone (a link preview, a poll's results) is none either.
#[test]
fn an_edited_without_content_is_no_edit() {
    let (mut chats, mut msgs) = synthetic();
    note(&mut msgs, PEER, STEP, PEER, false);
    note(&mut msgs, PEER, 2 * STEP, PEER, false);
    for line in [
        edited_line(PEER, STEP),
        content_line(PEER, 2 * STEP),
        edited_line(PEER, STEP),
    ] {
        let out = one(&mut chats, &mut msgs, &line);
        assert!(edits(&out).0.is_empty(), "no edit");
        assert!(out.fetch.is_empty(), "nothing asked");
    }
    // The content kept for 2 is still its edit's.
    let out = one(&mut chats, &mut msgs, &edited_line(PEER, 2 * STEP));
    assert_eq!(edits(&out).0.len(), 1, "the paired edit");
    // An own edit: by None, the target ours.
    note(&mut msgs, PEER, 3 * STEP, ME, true);
    one(&mut chats, &mut msgs, &content_line(PEER, 3 * STEP));
    let out = one(&mut chats, &mut msgs, &edited_line(PEER, 3 * STEP));
    assert_eq!(
        edits(&out).0,
        [(
            (3 * STEP).to_string(),
            TargetAuthor::Own,
            None,
            1_700_000_001_000
        )]
    );
}

/// A media `updateMessageContent` (a photo with a remote id: the edit a pointer could come from).
fn photo_content_line(chat: i64, id: i64) -> String {
    json!({"@type": "updateMessageContent", "chat_id": chat, "message_id": id,
        "new_content": {"@type": "messagePhoto", "photo": {"@type": "photo", "sizes": [
            {"@type": "photoSize", "photo": {"@type": "file", "id": 5, "size": 900, "expected_size": 900,
                "remote": {"@type": "remoteFile", "id": "synthetic-remote-5", "unique_id": "synthetic-unique-5"}},
             "width": 100, "height": 100}]},
            "caption": {"@type": "formattedText", "text": "synthetic text 3", "entities": []}}})
    .to_string()
}

/// TF14 (review pF5): the edit of a message delivered as view once (or restricted) brings no file
/// pointer: it is dropped and the hint stays; the edit of a plain message of the same chat still
/// comes with its media. (Mutant: the delivered memory does not keep the seal.)
#[test]
fn an_edit_of_a_view_once_message_keeps_no_pointer() {
    let (mut chats, mut msgs) = synthetic();
    let now_s = START / 1000;
    let once = json!({"self_destruct_type": {"@type": "messageSelfDestructTypeImmediately"},
        "content": {"@type": "messagePhoto", "photo": {"@type": "photo", "sizes": []},
            "caption": {"@type": "formattedText", "text": "", "entities": []}, "is_secret": true}});
    let once_again = once.clone();
    let restricted = json!({"restriction_info": {"@type": "restrictionInfo",
        "restriction_reason": "synthetic-restriction", "has_sensitive_content": false}});
    for (id, extra) in [(STEP, once), (2 * STEP, restricted), (3 * STEP, json!({}))] {
        let out = one(&mut chats, &mut msgs, &message_line(id, now_s, extra));
        assert_eq!(rows(&out).len(), 1, "message {id} is a row");
    }
    for id in [STEP, 2 * STEP, 3 * STEP] {
        one(&mut chats, &mut msgs, &photo_content_line(PEER, id));
        let out = one(&mut chats, &mut msgs, &edited_line(PEER, id));
        let (keys, bodies) = edits(&out);
        if id == 3 * STEP {
            assert_eq!(keys.len(), 1, "the plain message's edit");
            assert!(
                matches!(&bodies[0], kourier_model::Body::Media { attachments, .. }
                    if attachments[0].remote.is_some()),
                "the plain edit lost its media"
            );
        } else {
            assert!(keys.is_empty(), "the edit of message {id} was delivered");
        }
        assert!(out.fetch.is_empty(), "nothing asked for message {id}");
    }
    // The history path (a reconcile page's row) seals like a live row.
    let line: Value =
        serde_json::from_str(&message_line(5 * STEP, now_s, once_again.clone())).expect("a line");
    let past: Message = serde_json::from_value(line["message"].clone()).expect("a message");
    msgs.history(&mut chats, PEER, std::slice::from_ref(&past), START + 1000);
    one(&mut chats, &mut msgs, &photo_content_line(PEER, 5 * STEP));
    let out = one(&mut chats, &mut msgs, &edited_line(PEER, 5 * STEP));
    assert!(
        edits(&out).0.is_empty() && out.fetch.is_empty(),
        "the history row's edit came"
    );
    // The `getMessage` path: a view-once message the machine does not remember is asked, and its
    // answer is no edit either.
    one(&mut chats, &mut msgs, &photo_content_line(PEER, 9 * STEP));
    let out = one(&mut chats, &mut msgs, &edited_line(PEER, 9 * STEP));
    assert_eq!(out.fetch.len(), 1, "the unknown message is asked");
    let line: Value =
        serde_json::from_str(&message_line(9 * STEP, now_s, once_again)).expect("a line");
    let msg: Message = serde_json::from_value(line["message"].clone()).expect("a message");
    let out = msgs.on_message(&chats, &msg, START + 1000);
    assert!(edits(&out).0.is_empty(), "the answered view-once edit came");
}

/// G-8 + his "Keep them; KT-2 first": every permanent delete is parked with the ids TDLib named,
/// the unloaded supergroup delete (RC8: it DOES arrive) included; none is a row
/// (`rec-delete.jsonl`, all four steps; the private unloaded step brings nothing).
#[test]
fn deletes_are_parked_including_unloaded_supergroup_ids() {
    let rec = Recording::load("rec-delete.jsonl");
    let (base, base_answers) = part_lines(&rec, &[None]);
    assert_eq!(base_answers, 6, "recorded answers used (the loadChats)");
    let steps = [
        "delete-unloaded-u201",
        "delete-loaded-u201",
        "delete-unloaded-c401",
        "delete-loaded-c401",
    ];
    let (step, answers) = objects(steps.iter().map(|s| rec.part(s)));
    assert_eq!(answers, 0, "the steps hold no request");
    let (mut chats, mut msgs) = machines(&base, REC_NOW, false);
    // Kourier has rows of the loaded ones only.
    note(&mut msgs, 201, 4 * STEP, 201, false);
    note(&mut msgs, SG401, 3 * STEP, 203, false);
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    assert_eq!(
        out.staged.parked,
        [
            Parked::Delete {
                chat_id: 201,
                ids: vec![4 * STEP]
            },
            Parked::Delete {
                chat_id: SG401,
                ids: vec![STEP]
            },
            Parked::Delete {
                chat_id: SG401,
                ids: vec![3 * STEP]
            },
        ]
    );
    assert!(
        !out.staged
            .rows
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(
                e.event,
                BackendEvent::Deleted { .. } | BackendEvent::Message(_)
            )),
        "a delete made a row"
    );
    // Committed: three deferred rows.
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = crate::db::open(
        &dir.path().join("backend.db"),
        &kourier_db::DbKey::from_bytes([7; 32]),
    )
    .expect("open");
    crate::store::commit_rows(&conn, &out.staged, REC_NOW).expect("commit");
    let parked = crate::store::parked_rows(&conn).expect("parked");
    assert_eq!(parked.len(), 3);
    assert!(parked.iter().all(|p| p.kind == "delete"));
}

fn delete_line(chat: i64, ids: &[i64], is_permanent: bool, from_cache: bool) -> String {
    json!({"@type": "updateDeleteMessages", "chat_id": chat, "message_ids": ids,
        "is_permanent": is_permanent, "from_cache": from_cache})
    .to_string()
}

/// A cache delete, a message that became inaccessible and an unsent message's id are no deletes.
#[test]
fn a_from_cache_delete_is_ignored() {
    let (mut chats, mut msgs) = synthetic();
    for line in [
        delete_line(PEER, &[STEP], true, true),
        delete_line(PEER, &[STEP], false, false),
        delete_line(PEER, &[STEP + 1], true, false),
        delete_line(-77, &[STEP], true, false),
    ] {
        let out = one(&mut chats, &mut msgs, &line);
        assert!(out.staged.is_empty(), "a delete parked");
    }
    let out = one(
        &mut chats,
        &mut msgs,
        &delete_line(PEER, &[STEP, 2 * STEP], true, false),
    );
    assert_eq!(
        out.staged.parked,
        [Parked::Delete {
            chat_id: PEER,
            ids: vec![STEP, 2 * STEP]
        }]
    );
}

/// The base of `rec-read-elsewhere.jsonl` with its fresh-link history noted as delivered (the
/// recorded `getChatHistory` answers), and how many recorded answers that took.
fn read_base(rec: &Recording) -> (Chats, Messages, usize) {
    let (base, answers) = part_lines(rec, &[None]);
    let (chats, mut msgs) = machines(&base, REC_NOW, false);
    let mut history = 0;
    for r in rec.parts[0].requests().filter(|r| r.ty == "getChatHistory") {
        history += 1;
        for m in r.answer()["messages"].as_array().expect("messages") {
            let m: Message = serde_json::from_value(m.clone()).expect("a message");
            let author = match m.sender_id {
                _ if m.is_outgoing => ME,
                tdapi::MessageSender::User { user_id } => user_id,
                tdapi::MessageSender::Chat { chat_id } => chat_id,
                tdapi::MessageSender::Unknown => 0,
            };
            note(&mut msgs, m.chat_id, m.id, author, m.is_outgoing);
        }
    }
    assert_eq!(history, 10, "the history answers");
    (chats, msgs, answers)
}

/// `ReadMarker{conv, up_to}` of `out`: (conv, local, author).
fn markers(out: &Out) -> Vec<(ConvKey, String, TargetAuthor)> {
    out.staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::ReadMarker { conv, up_to } => Some((
                conv.clone().expect("a conv"),
                up_to.local.to_string(),
                up_to.author.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// TDLib forwards the server's read state (`rec-read-elsewhere.jsonl` steps `read-inbox-u201`,
/// `read-inbox-c401`): the marker targets the newest DELIVERED id at or below it, once per rise;
/// a delivered message above the position is never the target (L10's case).
#[test]
fn read_inbox_targets_the_newest_delivered_message() {
    let rec = Recording::load("rec-read-elsewhere.jsonl");
    let (mut chats, mut msgs, answers) = read_base(&rec);
    assert_eq!(
        answers, 16,
        "recorded answers used (loadChats + getChatHistory)"
    );
    assert!(chats.conv(SG401).is_some(), "401 is mapped");
    let (step, _) = objects([rec.part("read-inbox-u201"), rec.part("read-inbox-c401")]);
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    assert_eq!(
        markers(&out),
        [
            (
                direct(201),
                (4 * STEP).to_string(),
                TargetAuthor::User(uid(201))
            ),
            (group(SG401), STEP.to_string(), TargetAuthor::User(uid(202))),
        ]
    );
    let read_in: Vec<(i64, Option<i64>)> = out
        .staged
        .chats
        .iter()
        .filter(|f| f.read_inbox_max_id.is_some())
        .map(|f| (f.chat_id, f.read_inbox_max_id))
        .collect();
    assert_eq!(read_in, [(201, Some(4 * STEP)), (SG401, Some(STEP))]);
    // The same position again: nothing.
    let again = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    assert!(markers(&again).is_empty(), "a marker per rise only");

    // L10: a live message 9 arrives, then 11 more are delivered, and the position reaches 10
    // (TDLib's read position need not be a message Kourier has): the target is 9.
    let live = |id: i64| {
        json!({"@type": "updateNewMessage", "message": {"@type": "message", "id": id, "chat_id": 201,
            "sender_id": {"@type": "messageSenderUser", "user_id": 201}, "date": 1_700_000_150,
            "can_be_saved": true,
            "content": {"@type": "messageText", "text": {"@type": "formattedText", "text": "synthetic text 3", "entities": []}}}})
        .to_string()
    };
    feed_both(&mut chats, &mut msgs, &live(9 * STEP), REC_NOW);
    feed_both(&mut chats, &mut msgs, &live(11 * STEP), REC_NOW);
    let read = json!({"@type": "updateChatReadInbox", "chat_id": 201,
        "last_read_inbox_message_id": 10 * STEP, "unread_count": 1})
    .to_string();
    let out = feed_both(&mut chats, &mut msgs, &read, REC_NOW);
    assert_eq!(
        markers(&out),
        [(
            direct(201),
            (9 * STEP).to_string(),
            TargetAuthor::User(uid(201))
        )]
    );
}

/// `updateChatReadOutbox` of a private chat: `Receipts{Read}` from the peer for our own delivered
/// messages up to it (`rec-read-elsewhere.jsonl` step `read-outbox-u201`); a group's says nothing
/// but its fact (KT-2's `PeerReadUpTo`).
#[test]
fn read_outbox_in_a_private_chat_is_read_receipts() {
    let rec = Recording::load("rec-read-elsewhere.jsonl");
    let (mut chats, mut msgs, answers) = read_base(&rec);
    assert_eq!(
        answers, 16,
        "recorded answers used (loadChats + getChatHistory)"
    );
    let (step, _) = objects([rec.part("read-outbox-u201")]);
    let out = feed_lines(&mut chats, &mut msgs, &step, REC_NOW);
    let receipts: Vec<(Vec<MsgTarget>, UserId, ReceiptKind)> = out
        .staged
        .rows
        .iter()
        .flat_map(|r| &r.events)
        .filter_map(|e| match &e.event {
            BackendEvent::Receipts {
                targets, by, kind, ..
            } => Some((targets.clone(), by.clone(), *kind)),
            _ => None,
        })
        .collect();
    assert_eq!(
        receipts,
        [(
            vec![MsgTarget {
                conv: Some(direct(201)),
                author: TargetAuthor::Own,
                local: STEP.to_string().into(),
                alternates: Vec::new(),
            }],
            uid(201),
            ReceiptKind::Read
        )]
    );
    assert!(
        out.staged
            .chats
            .iter()
            .any(|f| f.chat_id == 201 && f.read_outbox_max_id == Some(STEP))
    );

    // A basic group: the fact, no receipts.
    note(&mut msgs, -301, 8 * STEP, ME, true);
    let line = json!({"@type": "updateChatReadOutbox", "chat_id": -301,
        "last_read_outbox_message_id": 8 * STEP})
    .to_string();
    let out = feed_both(&mut chats, &mut msgs, &line, REC_NOW);
    // (Its inbox position, told at the start, now has a delivered target: a marker, no receipts.)
    assert!(
        !out.staged
            .rows
            .iter()
            .flat_map(|r| &r.events)
            .any(|e| matches!(e.event, BackendEvent::Receipts { .. })),
        "a group's outbox read made receipts"
    );
    assert!(
        out.staged
            .chats
            .iter()
            .any(|f| f.chat_id == -301 && f.read_outbox_max_id == Some(8 * STEP))
    );
}

fn action_line(chat: i64, user: i64, action: &str) -> String {
    json!({"@type": "updateChatAction", "chat_id": chat, "topic_id": null,
        "sender_id": {"@type": "messageSenderUser", "user_id": user},
        "action": {"@type": action}})
    .to_string()
}

/// L-11: typing is an ephemeral `Typing`, never staged; our own, a held chat's, another action's
/// and everything before the link are dropped (the chats of `rec-edit.jsonl`'s base: 403 is a
/// channel, 402 public).
#[test]
fn typing_is_ephemeral_and_dropped_for_held_chats() {
    let rec = Recording::load("rec-edit.jsonl");
    let (base, answers) = part_lines(&rec, &[None]);
    assert_eq!(answers, 6, "recorded answers used (the loadChats)");
    let (mut chats, mut msgs) = machines(&base, REC_NOW, false);
    let typing = |on: bool| BackendEvent::Typing {
        conv: direct(201),
        by: uid(201),
        device: None,
        on,
        ttl: TYPING_TTL,
    };
    let out = feed_both(
        &mut chats,
        &mut msgs,
        &action_line(201, 201, "chatActionTyping"),
        REC_NOW,
    );
    assert!(
        out.staged.is_empty() && out.fetch.is_empty(),
        "typing staged"
    );
    assert_eq!(out.ephemeral, [typing(true)]);
    let out = feed_both(
        &mut chats,
        &mut msgs,
        &action_line(201, 201, "chatActionCancel"),
        REC_NOW,
    );
    assert_eq!(out.ephemeral, [typing(false)]);
    assert_eq!(chats.held(SG403), Some(Held::Channel));
    assert_eq!(chats.held(SG402), Some(Held::Public));
    for line in [
        action_line(201, ME, "chatActionTyping"),
        action_line(SG403, 203, "chatActionTyping"),
        action_line(SG402, 203, "chatActionTyping"),
        action_line(201, 201, "chatActionRecordingVoiceNote"),
    ] {
        let out = feed_both(&mut chats, &mut msgs, &line, REC_NOW);
        assert!(out.is_empty(), "a dropped action gave something");
    }
    // Before the link.
    let mut chats = Chats::new(Some(ME));
    let mut msgs = Messages::new(REC_NOW, false);
    feed_lines(&mut chats, &mut msgs, &base, REC_NOW);
    let out = feed_both(
        &mut chats,
        &mut msgs,
        &action_line(201, 201, "chatActionTyping"),
        REC_NOW,
    );
    assert!(out.is_empty(), "typing before the link");
}

// ---- TG-4 K4: the read re-assert's rule, and the confirm's fact ---------------------------------

fn read_inbox_line(chat: i64, id: i64) -> String {
    json!({"@type": "updateChatReadInbox", "chat_id": chat, "last_read_inbox_message_id": id,
        "unread_count": 0})
    .to_string()
}

/// A delivered message of `chat` the machine remembers (C7a seeds them from `delivered`).
fn delivered_msg(msgs: &mut Messages, chat: i64, id: i64, outgoing: bool) {
    msgs.note_delivered(&crate::store::Delivered {
        chat_id: chat,
        msg_id: id,
        author: format!("tg:{}", if outgoing { ME } else { chat }),
        outgoing,
    });
}

/// TDLib tells the chat's last message (`updateChatLastMessage`), a synthetic text of `id`.
fn last_message_line(chat: i64, id: i64) -> String {
    let line = message_line(id, START / 1000, json!({}));
    let mut message: Value = serde_json::from_str(&line).expect("a line");
    message["message"]["chat_id"] = json!(chat);
    json!({"@type": "updateChatLastMessage", "chat_id": chat,
        "last_message": message["message"].clone(), "positions": []})
    .to_string()
}

/// A synthetic bot's chat 88: held until T7 (H5), listed in the main list.
fn held_bot_chat(chats: &mut Chats) {
    feed(
        chats,
        r#"{"@type":"updateUser","user":{"@type":"user","id":88,"first_name":"Di","last_name":"Sample","phone_number":"","is_contact":true,"type":{"@type":"userTypeBot"}}}"#,
    );
    feed(
        chats,
        &chat_line(88, "chatListMain", 2, false, (true, 0), (0, 0)),
    );
    assert!(chats.held(88).is_some(), "chat 88 is not held");
}

/// D-2: a read re-assert is due for a listed, mapped, not held chat whose stored read position
/// (the highest incoming id a read act confirmed, or TDLib's echo of it) is ABOVE what TDLib told
/// IN THIS RUN (the id asked is the highest incoming delivered one at or below it, U2). Nothing is due for a chat TDLib told nothing of in the run (the seeded value is
/// not a tell), at equality, or when TDLib told more; the chat is named ONCE per run, so a second
/// tell in the run, even a lower one, brings no second request; a held chat is never named.
/// (Mutants: `>` -> `>=`; the comparison made against the SEEDED `told_in`; the once-per-run set
/// not filled.)
#[test]
fn a_read_tdlib_lost_is_named_once_per_run_for_a_mapped_chat() {
    // Told below the stored position: one re-assert of the stored position.
    let (mut chats, mut msgs) = synthetic();
    msgs.seed_read(PEER, 5 * STEP, 0);
    delivered_msg(&mut msgs, PEER, 5 * STEP, false);
    assert!(
        msgs.lost_reads(&chats).is_empty(),
        "a chat TDLib told nothing of in this run was named (the seed is no tell)"
    );
    feed_both(
        &mut chats,
        &mut msgs,
        &read_inbox_line(PEER, 3 * STEP),
        START,
    );
    assert_eq!(msgs.lost_reads(&chats), [(PEER, 5 * STEP)]);
    // Once per run: a second call, and a tell after it, name nothing.
    assert!(msgs.lost_reads(&chats).is_empty(), "named twice in a run");
    feed_both(
        &mut chats,
        &mut msgs,
        &read_inbox_line(PEER, 2 * STEP),
        START,
    );
    assert!(
        msgs.lost_reads(&chats).is_empty(),
        "a second tell named it again"
    );

    // At equality (TDLib and the store agree) and above it: nothing.
    for told in [5, 7] {
        let (mut chats, mut msgs) = synthetic();
        msgs.seed_read(PEER, 5 * STEP, 0);
        delivered_msg(&mut msgs, PEER, 5 * STEP, false);
        delivered_msg(&mut msgs, PEER, 5 * STEP, false);
        feed_both(
            &mut chats,
            &mut msgs,
            &read_inbox_line(PEER, told * STEP),
            START,
        );
        assert!(
            msgs.lost_reads(&chats).is_empty(),
            "told {told} against a stored 5 named a lost read"
        );
    }

    // A chat TDLib told in two steps (read state 0 first, the real one next: a restart's
    // `updateNewChat` then `updateChatReadInbox`, rec-restart-saved-pts): the highest counts.
    let (mut chats, mut msgs) = synthetic();
    msgs.seed_read(PEER, 5 * STEP, 0);
    delivered_msg(&mut msgs, PEER, 5 * STEP, false);
    feed_both(&mut chats, &mut msgs, &read_inbox_line(PEER, 0), START);
    feed_both(
        &mut chats,
        &mut msgs,
        &read_inbox_line(PEER, 5 * STEP),
        START,
    );
    assert!(
        msgs.lost_reads(&chats).is_empty(),
        "the lower first tell counted"
    );

    // A held chat is never named, whatever it was told.
    let (mut chats, mut msgs) = synthetic();
    held_bot_chat(&mut chats);
    msgs.seed_read(88, 5 * STEP, 0);
    feed_both(&mut chats, &mut msgs, &read_inbox_line(88, 3 * STEP), START);
    assert!(msgs.lost_reads(&chats).is_empty(), "a held chat was named");
}

/// U2 (durability F2): the stored read position can name an OUTGOING message (a server that
/// stores a read whose newest message is ours): delivered incoming 10 and our own 12, stored 12.
/// TDLib tells 10 in this run (the server's mark is the last incoming message): the compare goes
/// through the highest INCOMING delivered id at or below the position (10), which TDLib already
/// has, so no re-assert is due; told 8 it asks for 10, never for the outgoing 12. (Mutant: the raw
/// stored position compared and asked: 12 > 10 repeats at every start.)
#[test]
fn a_told_position_on_our_own_message_is_not_reasserted_when_tdlib_tells_the_last_incoming() {
    for (told, due) in [(10, None), (8, Some(10))] {
        let (mut chats, mut msgs) = synthetic();
        msgs.seed_read(PEER, 12 * STEP, 0);
        delivered_msg(&mut msgs, PEER, 10 * STEP, false);
        delivered_msg(&mut msgs, PEER, 12 * STEP, true);
        feed_both(
            &mut chats,
            &mut msgs,
            &read_inbox_line(PEER, told * STEP),
            START,
        );
        let expected: Vec<(i64, i64)> = due.map(|id| (PEER, id * STEP)).into_iter().collect();
        assert_eq!(
            msgs.lost_reads(&chats),
            expected,
            "told {told} against a stored 12 (incoming 10, own 12)"
        );
    }
    // A stored position with no incoming delivered id at or below it names nothing.
    let (mut chats, mut msgs) = synthetic();
    msgs.seed_read(PEER, 12 * STEP, 0);
    delivered_msg(&mut msgs, PEER, 12 * STEP, true);
    feed_both(
        &mut chats,
        &mut msgs,
        &read_inbox_line(PEER, 3 * STEP),
        START,
    );
    assert!(
        msgs.lost_reads(&chats).is_empty(),
        "an own message was asked as a read"
    );
}

/// U2 (durability F9): a lost read on a top the peer deleted afterwards. Delivered incoming 10,
/// stored 10, but TDLib's told last message is 9 (its server's top): TDLib counts an id it holds
/// no message of only at or below its last, so the re-assert is CLAMPED to 9; told read 9 (all
/// read up to the top) names nothing, told 4 asks for 9. With no told last message the id is the
/// delivered one. (Mutant: no clamp: 10 > 9 repeats at every start.)
#[test]
fn a_re_assert_is_clamped_to_tdlibs_told_last_message() {
    let ask = |told: i64, last: Option<i64>| {
        let (mut chats, mut msgs) = synthetic();
        msgs.seed_read(PEER, 10 * STEP, 0);
        delivered_msg(&mut msgs, PEER, 10 * STEP, false);
        if let Some(last) = last {
            feed_both(
                &mut chats,
                &mut msgs,
                &last_message_line(PEER, last * STEP),
                START,
            );
        }
        feed_both(
            &mut chats,
            &mut msgs,
            &read_inbox_line(PEER, told * STEP),
            START,
        );
        msgs.lost_reads(&chats)
    };
    assert!(ask(9, Some(9)).is_empty(), "a repeat for a deleted top");
    assert_eq!(
        ask(4, Some(9)),
        [(PEER, 9 * STEP)],
        "not clamped to the told last"
    );
    assert_eq!(
        ask(4, Some(12)),
        [(PEER, 10 * STEP)],
        "a last above the position"
    );
    assert_eq!(ask(4, None), [(PEER, 10 * STEP)], "no told last message");
}

/// K4 (critic M3, D-2): the confirm's fact raises the stored position once. Where TDLib's echo
/// came first (`mark_read` staged it) the confirm adds none; where the `ok` came first the fact
/// is the confirm's and the late echo stages none; a confirm at or below the stored position
/// states nothing. (Mutant: the fact is always returned.)
#[test]
fn a_confirmed_read_states_the_position_once_whichever_comes_first() {
    // The `ok` first (the echo is postponed): the confirm's fact, then the echo adds none.
    let (mut chats, mut msgs) = synthetic();
    let fact = msgs
        .confirm_read(PEER, 4 * STEP)
        .expect("the confirm's fact");
    assert_eq!(
        (fact.chat_id, fact.read_inbox_max_id),
        (PEER, Some(4 * STEP))
    );
    let out = feed_both(
        &mut chats,
        &mut msgs,
        &read_inbox_line(PEER, 4 * STEP),
        START,
    );
    assert!(
        out.staged
            .chats
            .iter()
            .all(|f| f.read_inbox_max_id.is_none()),
        "the echo staged the position the confirm stated"
    );
    // The echo first: its fact, then the confirm adds none.
    let (mut chats, mut msgs) = synthetic();
    let out = feed_both(
        &mut chats,
        &mut msgs,
        &read_inbox_line(PEER, 4 * STEP),
        START,
    );
    assert_eq!(
        out.staged
            .chats
            .iter()
            .filter_map(|f| f.read_inbox_max_id)
            .collect::<Vec<_>>(),
        [4 * STEP],
        "the echo's own fact"
    );
    assert!(
        msgs.confirm_read(PEER, 4 * STEP).is_none(),
        "a double stage"
    );
    assert!(
        msgs.confirm_read(PEER, 3 * STEP).is_none(),
        "a lower position"
    );
    // A higher confirm raises it again.
    assert!(msgs.confirm_read(PEER, 6 * STEP).is_some());
}
