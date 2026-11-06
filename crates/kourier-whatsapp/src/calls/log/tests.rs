//! The phone's call log as call lines (W6a1, A6): D12's result table, the direction from the creator, where a line goes
//! (the interceptor's chat first, a group, the peer of an outgoing call), the call's own time, a chat the core does not
//! have added quietly, a hostile id refused. Over a real `backend.db`; synthetic ids only (tails of 8 digits that are
//! substrings of no other value).

use std::sync::Arc;

use kourier_backend_api::{BackendEvent, FakeClock, IncomingMessage, decode_outbox_row};
use kourier_db::{DbKey, outbox};
use kourier_model::{
    Body, CallDirection, CallLine, CallMedia, CallState, CannotSend, ConvKey, Origin, SystemEvent,
    UserId,
};
use tokio::sync::Notify;
use wacore::types::events::CallLogSync;
use wacore_binary::Jid;
use waproto::whatsapp as wa;
use waproto::whatsapp::call_log_record::CallResult;

use super::{Placed, Seen, Verdict, note_lines, place, rehome_lines, state_of};
use crate::events::EventWriter;
use crate::features;
use crate::map::OwnIds;
use crate::send::started;
use crate::store::{KourierStore, StoreOwner, call_lines, chat_state, resyncs};

const PEER_LID: &str = "88800412975361";
const PEER_PN: &str = "15550163829047";
const US_LID: &str = "88800527361940";
const US_PN: &str = "15550194026538";
const THIRD_PN: &str = "15550127384605";
const FOURTH_PN: &str = "15550149208357";
const GROUP_TWO: &str = "120363445566778899";
const GROUP: &str = "120363778899001122";
const NOW: i64 = 1_800_000_000_000;

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a user")
}

fn direct(scheme: &str, value: &str) -> ConvKey {
    ConvKey::Direct(user(scheme, value))
}

fn group() -> ConvKey {
    ConvKey::Group(format!("wag:{GROUP}").into())
}

fn own() -> OwnIds {
    OwnIds {
        lid: Some(user("lid", US_LID)),
        pn: Some(user("pn", US_PN)),
    }
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([14; 32]),
        "wa-call-log",
    )
    .expect("open");
    (dir, owner)
}

/// A record of an incoming 1:1 call the peer placed (his LID), missed, at second 1,760,000,100.
fn incoming(id: &str, result: CallResult) -> Seen {
    Seen {
        call_id: id.to_owned(),
        creator: Some(user("lid", PEER_LID)),
        from_me: false,
        group: None,
        others: Vec::new(),
        video: Some(false),
        result: Some(result),
        started_s: Some(1_760_000_100),
        written_ms: 1_760_009_999_000,
    }
}

/// A record of a call we placed from the phone: the creator is our LID, the participants as the phone listed them.
fn outgoing(id: &str, result: CallResult, others: Vec<UserId>) -> Seen {
    Seen {
        creator: Some(user("lid", US_LID)),
        from_me: true,
        others,
        ..incoming(id, result)
    }
}

async fn run(store: &KourierStore, seen: &Seen) -> Placed {
    let seen = seen.clone();
    store
        .write("test: place", move |tx| place(tx, &seen, &own(), NOW))
        .await
        .expect("place")
}

/// The line of a placed record: the conversation of its batch and the one message it carries.
fn the_line(placed: &Placed) -> (ConvKey, IncomingMessage) {
    let Some(BackendEvent::HistoryBatch {
        conv,
        messages,
        has_more,
        ..
    }) = placed.events.last()
    else {
        panic!("a history batch comes last: {:?}", placed.verdict);
    };
    assert!(!has_more, "a call line ends its own batch");
    assert_eq!(messages.len(), 1);
    (conv.clone(), messages[0].clone())
}

/// The one live message of a placed record (the line after the cut, or the one that settles a line already there).
fn the_live_line(placed: &Placed) -> IncomingMessage {
    let Some(BackendEvent::Message(message)) = placed.events.last() else {
        panic!("a live message comes last: {:?}", placed.verdict);
    };
    message.clone()
}

/// The chat's cut as the phone's clear or delete (or the user's own clear) noted it.
async fn cut(store: &KourierStore, conv: &ConvKey, ms: i64) {
    let conv = conv.clone();
    store
        .write("test: cut", move |tx| chat_state::raise_cut(tx, &conv, ms))
        .await
        .expect("cut");
}

/// The interceptor's ringing line of call `id` from `author`'s chat `conv`, as the event writer's zone sees it.
fn ring(conv: &ConvKey, id: &str, author: UserId) -> BackendEvent {
    BackendEvent::Message(IncomingMessage::call(
        conv.clone(),
        id,
        Origin::Remote {
            author: author.clone(),
            device: None,
        },
        CallLine::signal(
            CallDirection::Incoming,
            false,
            CallState::Ringing { deadline: 70_000 },
            10_000,
        )
        .with_media(CallMedia::Voice, 10_000)
        .with_by(author, 10_000),
        10_000,
        10_000,
    ))
}

async fn noted_conv(store: &KourierStore, id: &'static str) -> Option<String> {
    store
        .read(move |conn| call_lines::conv_of(conn, id))
        .await
        .expect("conv_of")
}

fn call_of(message: &IncomingMessage) -> &CallLine {
    match &message.body {
        Body::System(SystemEvent::Call(line)) => line,
        _ => panic!("not a call line"),
    }
}

async fn learn(store: &KourierStore, lid: &str, pn: &str) {
    let (lid, pn) = (lid.to_owned(), pn.to_owned());
    store
        .write("test: learn", move |tx| {
            crate::store::exec(
                tx,
                "learn",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'test')",
                kourier_db::rusqlite::params![lid, pn],
            )
        })
        .await
        .expect("learn");
}

/// The events of every `core_outbox` row, in order.
async fn outbox_events(store: &KourierStore) -> Vec<BackendEvent> {
    store
        .read(|conn| {
            let rows = outbox::peek(conn, 1000).map_err(|_| crate::error::database("peek"))?;
            Ok(rows
                .iter()
                .flat_map(|row| decode_outbox_row(&row.envelope).expect("decodes"))
                .collect())
        })
        .await
        .expect("outbox")
}

/// D12's table: what a record's result says of a call, by who placed it. Mutants: REJECTED answered.
#[test]
fn the_result_table_gives_each_result_its_state() {
    use CallResult::{
        ABANDONED, ACCEPTEDELSEWHERE, CANCELLED, CONNECTED, FAILED, INVALID, MISSED, ONGOING,
        REJECTED, UNAVAILABLE, UPCOMING,
    };
    let incoming = [
        (CONNECTED, Some(CallState::Answered)),
        (ACCEPTEDELSEWHERE, Some(CallState::Answered)),
        (REJECTED, Some(CallState::Declined)),
        (MISSED, Some(CallState::Missed)),
        (CANCELLED, Some(CallState::Missed)),
        (UNAVAILABLE, None),
        (INVALID, None),
        (UPCOMING, None),
        (FAILED, None),
        (ABANDONED, None),
        (ONGOING, None),
    ];
    for (result, state) in incoming {
        assert_eq!(state_of(false, Some(result)), state, "incoming");
    }
    let outgoing = [
        (CONNECTED, Some(CallState::Answered)),
        (REJECTED, Some(CallState::Declined)),
        (MISSED, Some(CallState::Unanswered)),
        (CANCELLED, Some(CallState::Unanswered)),
        (UNAVAILABLE, Some(CallState::Unanswered)),
        (ACCEPTEDELSEWHERE, None),
        (INVALID, None),
        (UPCOMING, None),
        (FAILED, None),
        (ABANDONED, None),
        (ONGOING, None),
    ];
    for (result, state) in outgoing {
        assert_eq!(state_of(true, Some(result)), state, "outgoing");
    }
    assert_eq!(state_of(false, None), None);
    assert_eq!(state_of(true, None), None);
}

/// The record as upstream hands it: the direction is the CREATOR's (`from_me`), never the record's `isIncoming`, which
/// means the opposite of its name in some mutations; the creator and the id come from the index; the time of the call
/// is the record's start (seconds) and the write time only a fallback; the participants are persons only.
/// Mutant: the direction read from `isIncoming`.
#[test]
fn the_direction_is_the_creators_never_the_records_is_incoming() {
    let sync = |from_me: bool, is_incoming: bool| {
        let mut record = wa::CallLogRecord::default();
        record.is_incoming = Some(is_incoming);
        record.is_video = Some(true);
        record.call_result = Some(CallResult::CONNECTED);
        record.start_time = Some(1_760_000_100);
        record.group_jid = Some(format!("{GROUP}@g.us"));
        for jid in [
            format!("{PEER_LID}@lid"),
            format!("{US_PN}@s.whatsapp.net"),
            "status@broadcast".to_owned(),
            "not a jid at all".to_owned(),
        ] {
            let mut p = wa::call_log_record::ParticipantInfo::default();
            p.user_jid = Some(jid);
            record.participants.push(p);
        }
        CallLogSync::builder()
            .call_creator_jid(format!("{US_LID}@lid").parse::<Jid>().expect("jid"))
            .call_id("CALL-77310001".to_owned())
            .from_me(from_me)
            .timestamp(wacore::chrono::DateTime::from_timestamp(1_760_009_999, 0).expect("time"))
            .record(Box::new(record))
            .from_full_sync(false)
            .build()
    };
    let placed = Seen::from_sync(&sync(true, true));
    assert!(
        placed.from_me,
        "from_me is the creator's, whatever isIncoming says"
    );
    assert!(!Seen::from_sync(&sync(false, false)).from_me);
    assert_eq!(placed.call_id, "CALL-77310001");
    assert_eq!(placed.creator, Some(user("lid", US_LID)));
    assert_eq!(placed.group, Some(group()));
    assert_eq!(
        placed.others,
        [user("lid", PEER_LID), user("pn", US_PN)],
        "persons only, in record order, our own among them"
    );
    assert_eq!(placed.video, Some(true));
    assert_eq!(placed.result, Some(CallResult::CONNECTED));
    assert_eq!(placed.started_s, Some(1_760_000_100));
    assert_eq!(placed.written_ms, 1_760_009_999_000);
}

/// A record of the history push (no mutation) must name its call, a creator that is a person and its own start: else no
/// line (never a guess or a made-up time). The direction is the creator compared with OUR ids, never `isIncoming`; the
/// shared fields read as the mutation's. Mutants: `from_me` from `isIncoming`; the start not required.
#[test]
fn a_history_record_names_its_call_creator_and_start_or_gives_no_line() {
    let full = || {
        let mut record = wa::CallLogRecord::default();
        record.call_id = Some("CALL-77310101".into());
        record.call_creator_jid = Some(format!("{PEER_LID}@lid"));
        record.start_time = Some(1_760_000_100);
        record.call_result = Some(CallResult::MISSED);
        record.is_video = Some(true);
        record.is_incoming = Some(false);
        record.group_jid = Some(format!("{GROUP}@g.us"));
        record
    };
    let seen = Seen::from_record(&full(), &own()).expect("a whole record");
    assert_eq!(seen.call_id, "CALL-77310101");
    assert_eq!(seen.creator, Some(user("lid", PEER_LID)));
    assert!(!seen.from_me);
    assert_eq!(seen.group, Some(group()));
    assert_eq!(seen.video, Some(true));
    assert_eq!(seen.result, Some(CallResult::MISSED));
    assert_eq!(seen.started_s, Some(1_760_000_100));
    assert_eq!(
        seen.written_ms, 0,
        "there is no mutation time in a history record"
    );

    // Our own id as the creator is outgoing, in either form, whatever isIncoming says.
    for creator in [format!("{US_LID}@lid"), format!("{US_PN}@s.whatsapp.net")] {
        let mut record = full();
        record.call_creator_jid = Some(creator);
        record.is_incoming = Some(true);
        let seen = Seen::from_record(&record, &own()).expect("a record");
        assert!(seen.from_me, "the creator is us: the direction is outgoing");
    }

    let without = |change: &dyn Fn(&mut wa::CallLogRecord)| {
        let mut record = full();
        change(&mut record);
        Seen::from_record(&record, &own())
    };
    assert!(without(&|r| r.call_id = None).is_none(), "no call id");
    assert!(
        without(&|r| r.call_id = Some(String::new())).is_none(),
        "an empty call id"
    );
    assert!(
        without(&|r| r.call_creator_jid = None).is_none(),
        "no creator"
    );
    assert!(
        without(&|r| r.call_creator_jid = Some("status@broadcast".into())).is_none(),
        "a creator that is no person"
    );
    assert!(
        without(&|r| r.call_creator_jid = Some("not a jid at all".into())).is_none(),
        "a creator that is no jid"
    );
    assert!(without(&|r| r.start_time = None).is_none(), "no start");
    assert!(
        without(&|r| r.start_time = Some(0)).is_none(),
        "a start of zero"
    );
}

/// The log's answer to a call the interceptor filed lands in the SAME conversation and under the same `call:<id>`,
/// also when the log names the person in the other namespace (the stanza said his number, the log says his LID: the
/// store maps him, so it is the one chat); the interceptor's zone is what notes the call (`note_lines`), and only for call
/// lines. The answer is a live message that settles the line there (INTEROP-8). Mutants: the conversation from the log's
/// creator; the writer's zoned arm without `note_lines`.
#[tokio::test]
async fn the_interceptors_line_and_the_logs_answer_share_one_conversation_and_id() {
    let (_dir, owner) = open();
    let store = owner.store();
    let writer = EventWriter::new(
        store.clone(),
        Arc::new(FakeClock::new(1_000_000)),
        Arc::new(Notify::new()),
    );
    learn(&store, PEER_LID, PEER_PN).await;
    let chat = direct("pn", PEER_PN);
    let ring = ring(&chat, "CALL-77310002", user("pn", PEER_PN));
    let not_a_call = BackendEvent::ConversationRemoved {
        conv: ConvKey::Group("wag:120363000000000001".into()),
    };
    {
        let writer = writer.clone();
        let events = vec![ring, not_a_call];
        std::thread::spawn(move || writer.write_and_wait(&events))
            .join()
            .expect("handler thread")
            .expect("written");
    }
    let id = "CALL-77310002";
    assert_eq!(
        noted_conv(&store, id).await,
        Some(chat.encode()),
        "the interceptor's zone noted the call"
    );
    // The phone's log: the same call, the person as his LID, answered on another device.
    let seen = incoming(id, CallResult::ACCEPTEDELSEWHERE);
    {
        let writer = writer.clone();
        std::thread::spawn(move || writer.write_call_log_and_wait(seen, own()))
            .join()
            .expect("handler thread")
            .expect("written");
    }
    let events = outbox_events(&store).await;
    let Some(BackendEvent::Message(message)) = events.last() else {
        panic!("the log's signal is the last event, live");
    };
    assert_eq!(
        message.conv, chat,
        "the interceptor's conversation, not the creator's"
    );
    assert_eq!(&*message.local, "call:CALL-77310002");
    let line = call_of(message);
    assert_eq!(line.state, CallState::Answered);
    assert_eq!(line.direction, CallDirection::Incoming);
    assert_eq!(
        message.origin,
        Origin::OwnOtherDevice { device: None },
        "only an own device may say answered"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, BackendEvent::ConversationUpsert(_)))
            .count(),
        1,
        "the chat was stated once, with the interceptor's line"
    );
}

/// An outgoing call goes to the ONE other person its record lists (our own ids skipped, his number as his current LID
/// when the store maps it), with our device as the origin, no `by` and the right state; none or several other persons:
/// no line. Mutant: the conversation from the log's creator for an outgoing record.
#[tokio::test]
async fn an_outgoing_call_goes_to_the_one_other_person_and_none_or_several_make_no_line() {
    let (_dir, owner) = open();
    let store = owner.store();
    let both = vec![user("pn", US_PN), user("pn", PEER_PN)];
    let placed = run(
        &store,
        &outgoing("OUT-77310003", CallResult::CONNECTED, both.clone()),
    )
    .await;
    assert_eq!(placed.verdict, Verdict::Line);
    let (conv, message) = the_line(&placed);
    assert_eq!(
        conv,
        direct("pn", PEER_PN),
        "no mapping: as the phone gave him"
    );
    let line = call_of(&message);
    assert_eq!(line.direction, CallDirection::Outgoing);
    assert_eq!(line.state, CallState::Answered);
    assert_eq!(line.by, None, "the user himself");
    assert_eq!(message.origin, Origin::OwnOtherDevice { device: None });
    assert_eq!(&*message.local, "call:OUT-77310003");

    learn(&store, PEER_LID, PEER_PN).await;
    let placed = run(
        &store,
        &outgoing("OUT-77310004", CallResult::UNAVAILABLE, both),
    )
    .await;
    let (conv, message) = the_line(&placed);
    assert_eq!(
        conv,
        direct("lid", PEER_LID),
        "his current LID once the store maps him"
    );
    assert_eq!(call_of(&message).state, CallState::Unanswered);

    for (id, others) in [
        ("OUT-77310005", vec![user("pn", US_PN), user("lid", US_LID)]),
        ("OUT-77310006", Vec::new()),
        (
            "OUT-77310007",
            vec![user("pn", PEER_PN), user("pn", THIRD_PN)],
        ),
    ] {
        let placed = run(&store, &outgoing(id, CallResult::CONNECTED, others)).await;
        assert_eq!(placed.verdict, Verdict::Unplaced, "{id}");
        assert!(placed.events.is_empty(), "{id}: no line, no chat");
    }
}

/// A group call goes to its group under `gcall:<id>`, with no media; the creator is `by` for an incoming one and
/// nobody for one we placed. Mutant: the record's group ignored.
#[tokio::test]
async fn a_group_call_goes_to_its_group_under_a_gcall_id() {
    let (_dir, owner) = open();
    let store = owner.store();
    let mut seen = incoming("GROUP-77310008", CallResult::ACCEPTEDELSEWHERE);
    seen.group = Some(group());
    seen.others = vec![user("lid", PEER_LID), user("pn", US_PN)];
    let placed = run(&store, &seen).await;
    let (conv, message) = the_line(&placed);
    assert_eq!(conv, group());
    assert_eq!(&*message.local, "gcall:GROUP-77310008");
    let line = call_of(&message);
    assert!(line.group);
    assert_eq!(line.media, None, "a group call has no kind");
    assert_eq!(line.state, CallState::Answered);
    assert_eq!(line.by, Some(user("lid", PEER_LID)), "who started it");

    let mut ours = outgoing(
        "GROUP-77310009",
        CallResult::CONNECTED,
        vec![user("lid", PEER_LID)],
    );
    ours.group = Some(group());
    let (conv, message) = the_line(&run(&store, &ours).await);
    assert_eq!(conv, group(), "not the one participant's chat");
    let line = call_of(&message);
    assert_eq!(
        (line.group, line.direction, line.by.clone()),
        (true, CallDirection::Outgoing, None)
    );
}

/// The line's time is the CALL's start in milliseconds (the record's seconds), the mutation's write time only when the
/// record has no start, and never later than now. Mutant: the write time always.
#[tokio::test]
async fn the_lines_time_is_the_calls_own_start_not_the_mutations() {
    let (_dir, owner) = open();
    let store = owner.store();
    let mut seen = incoming("TIME-77310010", CallResult::MISSED);
    let at = |placed: &Placed| the_line(placed).1.sent_at;
    assert_eq!(at(&run(&store, &seen).await), 1_760_000_100_000);
    seen.call_id = "TIME-77310011".into();
    seen.started_s = None;
    assert_eq!(
        at(&run(&store, &seen).await),
        1_760_009_999_000,
        "no start: the write time"
    );
    seen.call_id = "TIME-77310012".into();
    seen.started_s = Some(0);
    assert_eq!(
        at(&run(&store, &seen).await),
        1_760_009_999_000,
        "a zero start is none"
    );
    seen.call_id = "TIME-77310013".into();
    seen.started_s = Some(4_000_000_000);
    assert_eq!(
        at(&run(&store, &seen).await),
        NOW,
        "a start in the future is now"
    );
}

/// A call from a person the core has no chat with ADDS the chat (his answer: "if there is no chat - it should be
/// added"): the features statement (a `ConversationUpsert` that creates it) comes first, with `cannot_send:
/// NotAccepted` because a call line is no start; the line is a history row, silent and read, so it is never announced;
/// the chat is stated once. Mutants: no statement; a line that notifies (`read` false).
#[tokio::test]
async fn a_call_from_a_person_with_no_chat_adds_the_chat_quietly_and_it_is_no_start() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct("lid", PEER_LID);
    let placed = run(&store, &incoming("NEW-77310014", CallResult::MISSED)).await;
    assert_eq!(placed.verdict, Verdict::Line);
    let Some(BackendEvent::ConversationUpsert(statement)) = placed.events.first() else {
        panic!("the chat is stated first");
    };
    assert_eq!(statement.key, chat);
    assert_eq!(
        statement
            .features
            .as_ref()
            .and_then(|f| f.cannot_send.clone()),
        Some(CannotSend::NotAccepted),
        "a call is no start: the chat says start it on the phone"
    );
    let (conv, message) = the_line(&placed);
    assert_eq!(conv, chat);
    assert!(
        message.silent && message.read,
        "quiet: never announced, never counted"
    );
    assert_eq!(call_of(&message).state, CallState::Missed);
    assert_eq!(call_of(&message).by, Some(user("lid", PEER_LID)));
    assert_eq!(call_of(&message).media, Some(CallMedia::Voice));
    let started = {
        let chat = chat.clone();
        store
            .read(move |conn| started::holds(conn, &chat))
            .await
            .expect("started")
    };
    assert!(!started, "a call line proves no start");

    // The next record of the chat finds it stated: just the line.
    let placed = run(&store, &incoming("NEW-77310015", CallResult::CONNECTED)).await;
    assert_eq!(placed.events.len(), 1);
}

/// A call id the contract refuses (a control character) makes no line and is never noted: a hostile id must not pin a
/// chat for the real call. Mutant: the id noted before the check.
#[tokio::test]
async fn a_hostile_call_id_is_refused_and_never_noted() {
    let (_dir, owner) = open();
    let store = owner.store();
    let hostile = "CALL\u{7}77310016";
    let placed = run(&store, &incoming(hostile, CallResult::MISSED)).await;
    assert_eq!(placed.verdict, Verdict::Refused);
    assert!(placed.events.is_empty());
    assert_eq!(
        store
            .read(move |conn| call_lines::conv_of(conn, hostile))
            .await
            .expect("conv_of"),
        None
    );
}

/// An incoming record whose creator is one of our own ids or is no person names no chat (the log never files a call
/// under ourselves). Mutant: the own-id guard removed.
#[tokio::test]
async fn an_incoming_record_whose_creator_is_us_or_nobody_names_no_chat() {
    let (_dir, owner) = open();
    let store = owner.store();
    let mut seen = incoming("SELF-77310017", CallResult::MISSED);
    seen.creator = Some(user("pn", US_PN));
    assert_eq!(run(&store, &seen).await.verdict, Verdict::Unplaced);
    seen.call_id = "SELF-77310018".into();
    seen.creator = None;
    assert_eq!(run(&store, &seen).await.verdict, Verdict::Unplaced);
    seen.call_id = "SELF-77310019".into();
    seen.result = Some(CallResult::FAILED);
    seen.creator = Some(user("lid", PEER_LID));
    let placed = run(&store, &seen).await;
    assert_eq!(
        placed.verdict,
        Verdict::Silent,
        "a result that says nothing says nothing"
    );
    assert!(placed.events.is_empty());
}

/// The interceptor's zone notes a call line under its id without the prefix, for a group line too, and nothing for any
/// other event. Mutant: the group prefix read as the direct one.
#[tokio::test]
async fn call_lines_are_noted_under_their_bare_ids_and_other_events_note_nothing() {
    let (_dir, owner) = open();
    let store = owner.store();
    let line = |conv: ConvKey, group_call: bool, id: &str| {
        BackendEvent::Message(IncomingMessage::call(
            conv,
            id,
            Origin::Remote {
                author: user("lid", PEER_LID),
                device: None,
            },
            CallLine::signal(
                CallDirection::Incoming,
                group_call,
                CallState::Missed,
                5_000,
            ),
            5_000,
            5_000,
        ))
    };
    let events = vec![
        line(direct("lid", PEER_LID), false, "ONE-77310020"),
        line(group(), true, "TWO-77310021"),
        BackendEvent::ConversationRemoved { conv: group() },
    ];
    store
        .write("test: note", move |tx| note_lines(tx, &events, NOW))
        .await
        .expect("note");
    let conv = |id: &'static str| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| call_lines::conv_of(conn, id))
                .await
                .expect("conv_of")
        }
    };
    assert_eq!(
        conv("ONE-77310020").await,
        Some(direct("lid", PEER_LID).encode())
    );
    assert_eq!(conv("TWO-77310021").await, Some(group().encode()));
    assert_eq!(
        conv("gcall:TWO-77310021").await,
        None,
        "the prefix is not part of the id"
    );
}

/// INTEROP-4 (his "if there is no chat - it should be added"): a call of the phone's log AFTER the chat's cut (the newest
/// clear or delete the backend knows) is a LIVE message, silent and read, so the core brings a deleted chat back; a call at
/// or below the cut (a tie too) and a call in a chat with no known cut stay history rows, which the core drops again. The
/// cut is looked up in either form of the person. Mutant: the live form for a record below the cut.
#[tokio::test]
async fn a_call_after_the_chats_cut_is_a_live_quiet_line_and_one_below_it_stays_history() {
    let (_dir, owner) = open();
    let store = owner.store();
    let call_ms = 1_760_000_100_000;
    let at = |creator: UserId, id: &str| {
        let mut seen = incoming(id, CallResult::MISSED);
        seen.creator = Some(creator);
        seen
    };

    // No cut known: the chat was never deleted, the line is history.
    let (conv, _) = the_line(&run(&store, &incoming("CUT-77310101", CallResult::MISSED)).await);
    assert_eq!(conv, direct("lid", PEER_LID));

    // A cut just below the call: live, quiet, and read (nothing is announced or counted).
    let one = direct("pn", THIRD_PN);
    cut(&store, &one, call_ms - 1).await;
    let placed = run(&store, &at(user("pn", THIRD_PN), "CUT-77310102")).await;
    let message = the_live_line(&placed);
    assert_eq!(message.conv, one);
    assert!(
        message.silent && message.read,
        "quiet: never announced, never counted"
    );
    assert_eq!(&*message.local, "call:CUT-77310102");
    assert_eq!(call_of(&message).state, CallState::Missed);
    assert_eq!(
        message.origin,
        Origin::OwnOtherDevice { device: None },
        "still the phone's word"
    );
    assert!(
        matches!(
            placed.events.first(),
            Some(BackendEvent::ConversationUpsert(_))
        ),
        "the chat is still added when the core has none"
    );

    // The call IS the cut (a tie), or older: it was taken away, and stays history for the core to drop.
    let tie = direct("pn", FOURTH_PN);
    cut(&store, &tie, call_ms).await;
    let placed = run(&store, &at(user("pn", FOURTH_PN), "CUT-77310103")).await;
    assert_eq!(the_line(&placed).0, tie, "a tie is below the cut");
    let mut older = at(user("pn", FOURTH_PN), "CUT-77310104");
    older.started_s = Some(1_700_000_000);
    assert_eq!(the_line(&run(&store, &older).await).0, tie);

    // The cut was noted under the person's other form: the chat is the LID, the clear came for his number.
    learn(&store, PEER_LID, PEER_PN).await;
    cut(&store, &direct("pn", PEER_PN), call_ms - 1).await;
    let placed = run(&store, &incoming("CUT-77310105", CallResult::MISSED)).await;
    assert_eq!(
        the_live_line(&placed).conv,
        direct("lid", PEER_LID),
        "found in either form"
    );
}

/// The cut only rises: an older clear that is replayed lowers nothing.
#[tokio::test]
async fn a_cut_is_only_ever_raised() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct("pn", THIRD_PN);
    cut(&store, &chat, 5_000).await;
    cut(&store, &chat, 3_000).await;
    let read = |chat: ConvKey| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| chat_state::cut_of(conn, &chat))
                .await
                .expect("cut_of")
        }
    };
    assert_eq!(read(chat.clone()).await, Some(5_000));
    cut(&store, &chat, 9_000).await;
    assert_eq!(read(chat).await, Some(9_000));
    assert_eq!(read(direct("pn", FOURTH_PN)).await, None);
}

/// INTEROP-8: when the interceptor's line of the call exists (`call_lines` has the id) and the call is minutes old at most
/// (it can still ring), the log's signal is a LIVE message with `read: false` and not silent, so it settles the line as the
/// interceptor's own terminate would (a Missed becomes unread and is announced); an older filed call is no news (its
/// line settled by its own deadline long ago; after a `Rebuild` a replay must not announce every old missed call): quiet,
/// like any record; below the cut it is history. Mutants: `read: true` on a filed call; the window ignored.
#[tokio::test]
async fn a_log_signal_for_a_line_already_there_settles_it_live_and_unread() {
    let (_dir, owner) = open();
    let store = owner.store();
    let chat = direct("lid", PEER_LID);
    let events = vec![
        ring(&chat, "FILED-77310111", user("lid", PEER_LID)),
        ring(&chat, "FILED-77310112", user("lid", PEER_LID)),
    ];
    store
        .write("test: note", move |tx| note_lines(tx, &events, NOW))
        .await
        .expect("note");
    let fresh = |id: &str, result: CallResult| {
        let mut seen = incoming(id, result);
        seen.started_s = Some(NOW / 1000 - 30);
        seen
    };

    let placed = run(&store, &fresh("FILED-77310111", CallResult::MISSED)).await;
    let message = the_live_line(&placed);
    assert_eq!(message.conv, chat);
    assert_eq!(&*message.local, "call:FILED-77310111");
    assert!(
        !message.read && !message.silent,
        "a missed call whose hang-up was lost is unread and announced, as the interceptor's own would be"
    );
    assert_eq!(call_of(&message).state, CallState::Missed);
    assert_eq!(message.origin, Origin::OwnOtherDevice { device: None });

    // Answered counts nothing in the core, whatever the flags say.
    let placed = run(&store, &fresh("FILED-77310111", CallResult::CONNECTED)).await;
    assert_eq!(call_of(&the_live_line(&placed)).state, CallState::Answered);

    // Older than any ring can be: quiet history (no cut is known), never announced.
    let (conv, message) =
        the_line(&run(&store, &incoming("FILED-77310112", CallResult::MISSED)).await);
    assert_eq!(conv, chat);
    assert!(message.silent && message.read, "an old call is no news");

    // A line the clear took away stays gone: below the cut it is history (the core drops it).
    cut(&store, &chat, NOW / 1000 * 1000 - 20_000).await;
    let (conv, message) =
        the_line(&run(&store, &fresh("FILED-77310111", CallResult::MISSED)).await);
    assert_eq!(conv, chat);
    assert!(message.silent && message.read);
}

/// R1 (2026-11-04): a call answered on the phone whose ring the core's deadline already made MISSED. The phone's log
/// is our own side's word, and it may come long after the ring (R1 saw none within ten minutes): CONNECTED (answered
/// on the phone itself) or ACCEPTEDELSEWHERE is an answered line from our own device in the chat and under the id of the
/// interceptor's line, quiet (the call is no news), and the core's join moves the missed line up to answered (the
/// order only moves up; `kourier-core/tests/calls.rs` `a_ring_past_its_deadline_is_missed_until_it_is_answered`, and
/// `ingest/call.rs` joins a history signal into the line that is there like a live one). Nothing here is new: it pins
/// the path R1's call step relies on.
#[tokio::test]
async fn a_late_log_answer_settles_a_line_the_ring_deadline_made_missed() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The interceptor's ring, filed under the peer's number (the record names his LID: the mapping ties the two).
    learn(&store, PEER_LID, PEER_PN).await;
    let chat = direct("pn", PEER_PN);
    let events = vec![
        ring(&chat, "LATE-58810211", user("pn", PEER_PN)),
        ring(&chat, "LATE-58810212", user("pn", PEER_PN)),
    ];
    let rung = events.clone();
    store
        .write("test: note", move |tx| note_lines(tx, &events, NOW))
        .await
        .expect("note");
    for (id, result, rang) in [
        ("LATE-58810211", CallResult::CONNECTED, &rung[0]),
        ("LATE-58810212", CallResult::ACCEPTEDELSEWHERE, &rung[1]),
    ] {
        // Ten minutes after the call: the ring's deadline passed long ago.
        let mut seen = incoming(id, result);
        seen.started_s = Some(NOW / 1000 - 600);
        let (conv, message) = the_line(&run(&store, &seen).await);
        assert_eq!(conv, chat, "{result:?}: the interceptor's chat");
        assert_eq!(&*message.local, format!("call:{id}"));
        assert_eq!(message.origin, Origin::OwnOtherDevice { device: None });
        assert!(message.silent && message.read, "an old call is no news");
        let answered = call_of(&message);
        assert_eq!(answered.state, CallState::Answered, "{result:?}");
        // What the core makes of it: the ring settled as missed at its deadline, then this signal.
        let BackendEvent::Message(rang) = rang else {
            panic!("a ring")
        };
        let missed = call_of(rang).settled(NOW);
        assert_eq!(missed.state, CallState::Missed);
        assert_eq!(missed.join(answered).state, CallState::Answered);
        assert_eq!(answered.join(&missed).state, CallState::Answered);
    }
}

/// THE OWN ANSWER's lookup (`calls::own_answer`): the chat a 1:1 call's line was filed under, only when it is a direct
/// chat of the person the accept names, in either form of him; nothing for no line, a group's line or another person's
/// chat. Mutant: the fit dropped (`Ok(Some(filed))`; `calls::tests::the_interceptor_answers_the_filed_line_...` dies too).
#[tokio::test]
async fn the_filed_chat_of_an_own_answer_is_a_direct_chat_of_its_creator_or_none() {
    let (_dir, owner) = open();
    let store = owner.store();
    learn(&store, PEER_LID, PEER_PN).await;
    let peer_chat = direct("lid", PEER_LID);
    let events = vec![
        ring(&peer_chat, "OWN-66120311", user("lid", PEER_LID)),
        ring(
            &direct("pn", THIRD_PN),
            "OWN-66120312",
            user("pn", THIRD_PN),
        ),
        BackendEvent::Message(IncomingMessage::call(
            group(),
            "OWN-66120313",
            Origin::Remote {
                author: user("lid", PEER_LID),
                device: None,
            },
            CallLine::signal(
                CallDirection::Incoming,
                true,
                CallState::Ringing { deadline: 70_000 },
                10_000,
            ),
            10_000,
            10_000,
        )),
    ];
    store
        .write("test: note", move |tx| note_lines(tx, &events, NOW))
        .await
        .expect("note");
    let ask = |id: &'static str, person: UserId| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| super::filed_chat_of(conn, id, &person))
                .await
                .expect("filed")
        }
    };
    // His LID chat, asked with either form of him.
    assert_eq!(
        ask("OWN-66120311", user("lid", PEER_LID)).await,
        Some(peer_chat.clone())
    );
    assert_eq!(
        ask("OWN-66120311", user("pn", PEER_PN)).await,
        Some(peer_chat)
    );
    // Another person's chat, a group's line, no line.
    assert_eq!(ask("OWN-66120312", user("pn", PEER_PN)).await, None);
    assert_eq!(ask("OWN-66120313", user("lid", PEER_LID)).await, None);
    assert_eq!(ask("OWN-66120314", user("lid", PEER_LID)).await, None);
}

/// INTEROP-7: the conversation `call_lines` holds for a call id is taken only when it FITS the record. Call ids are
/// chosen by the calling side and every member of a group sees a group call's id, so a 1:1 offer that names the id of a
/// group call must not pull the group's log record into his chat. A group record takes only the group it names; a 1:1
/// record only a direct chat of its creator or of one other person it lists, in either form of that person (the store's
/// mapping); anything else is placed by the record's own rules. Mutant: the filed conversation taken for a group record
/// that names another group.
#[tokio::test]
async fn a_filed_conversation_that_does_not_fit_the_record_is_ignored() {
    let (_dir, owner) = open();
    let store = owner.store();
    let note = |conv: ConvKey, id: &'static str| {
        let store = store.clone();
        async move {
            let events = vec![ring(&conv, id, user("lid", PEER_LID))];
            store
                .write("test: note", move |tx| note_lines(tx, &events, NOW))
                .await
                .expect("note");
        }
    };

    // The group's call id was first seen in a 1:1 offer of PEER: the group record goes to its group.
    note(direct("lid", PEER_LID), "GRP-77310121").await;
    let mut seen = incoming("GRP-77310121", CallResult::ACCEPTEDELSEWHERE);
    seen.group = Some(group());
    seen.creator = Some(user("pn", THIRD_PN));
    let (conv, message) = the_line(&run(&store, &seen).await);
    assert_eq!(conv, group(), "not the 1:1 chat that named the id");
    assert_eq!(&*message.local, "gcall:GRP-77310121");

    // The same call id filed under another group: still only the group the record names.
    note(
        ConvKey::Group(format!("wag:{GROUP_TWO}").into()),
        "GRP-77310122",
    )
    .await;
    let mut seen = incoming("GRP-77310122", CallResult::ACCEPTEDELSEWHERE);
    seen.group = Some(group());
    assert_eq!(the_line(&run(&store, &seen).await).0, group());

    // A 1:1 record of THIRD whose call id was first filed in PEER's chat: THIRD's own chat.
    note(direct("lid", PEER_LID), "ONE-77310123").await;
    let mut seen = incoming("ONE-77310123", CallResult::MISSED);
    seen.creator = Some(user("pn", THIRD_PN));
    assert_eq!(
        the_line(&run(&store, &seen).await).0,
        direct("pn", THIRD_PN),
        "a chat of nobody the record names is not taken"
    );

    // The person in his number's form with nothing mapped: not the same chat as far as the store knows.
    note(direct("lid", PEER_LID), "FORM-77310124").await;
    let mut seen = incoming("FORM-77310124", CallResult::MISSED);
    seen.creator = Some(user("pn", PEER_PN));
    assert_eq!(
        the_line(&run(&store, &seen).await).0,
        direct("pn", PEER_PN),
        "no mapping: the record's own placement"
    );

    // Once the store maps him, the same record fits the chat the interceptor used (and it is a line already there).
    learn(&store, PEER_LID, PEER_PN).await;
    note(direct("lid", PEER_LID), "FORM-77310125").await;
    let mut seen = incoming("FORM-77310125", CallResult::MISSED);
    seen.creator = Some(user("pn", PEER_PN));
    seen.started_s = Some(NOW / 1000 - 30);
    let placed = run(&store, &seen).await;
    assert_eq!(the_live_line(&placed).conv, direct("lid", PEER_LID));
    assert_eq!(
        noted_conv(&store, "FORM-77310125").await,
        Some(direct("lid", PEER_LID).encode())
    );
}

/// DUR-9: log first, interceptor second makes ONE line. The phone's log filed the call under the person's LID; the
/// stanza of the same call names his number; the interceptor's zone gives the line the chat the log chose (the store maps
/// him), so no second chat is stated and the two signals share `call:<id>` in one chat. A line of another person with the
/// same id is left where it is. Mutant: the interceptor's zone does not move the line (a second line).
#[tokio::test]
async fn a_log_line_first_and_the_interceptors_line_after_it_are_one_line() {
    let (_dir, owner) = open();
    let store = owner.store();
    let writer = EventWriter::new(
        store.clone(),
        Arc::new(FakeClock::new(1_000_000)),
        Arc::new(Notify::new()),
    );
    learn(&store, PEER_LID, PEER_PN).await;
    let id = "BOTH-77310131";
    let lid_chat = direct("lid", PEER_LID);
    {
        let writer = writer.clone();
        let seen = incoming(id, CallResult::MISSED);
        std::thread::spawn(move || writer.write_call_log_and_wait(seen, own()))
            .join()
            .expect("handler thread")
            .expect("written");
    }
    // The offline stanza names his NUMBER.
    {
        let writer = writer.clone();
        let events = vec![ring(&direct("pn", PEER_PN), id, user("pn", PEER_PN))];
        std::thread::spawn(move || writer.write_and_wait(&events))
            .join()
            .expect("handler thread")
            .expect("written");
    }
    let events = outbox_events(&store).await;
    let homes: Vec<ConvKey> = events
        .iter()
        .filter_map(|event| match event {
            BackendEvent::Message(m) if *m.local == format!("call:{id}") => Some(m.conv.clone()),
            BackendEvent::HistoryBatch { conv, messages, .. }
                if messages.iter().any(|m| *m.local == format!("call:{id}")) =>
            {
                Some(conv.clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        homes,
        [lid_chat.clone(), lid_chat.clone()],
        "both signals of the call are in the one chat"
    );
    assert!(
        events.iter().all(|event| !matches!(
            event,
            BackendEvent::ConversationUpsert(patch) if patch.key == direct("pn", PEER_PN)
        )),
        "the number's chat is never stated"
    );
    assert_eq!(
        noted_conv(&store, "BOTH-77310131").await,
        Some(lid_chat.encode())
    );

    // A line of another person's chat is not moved: the id is shared by a stranger.
    let stranger = direct("pn", THIRD_PN);
    let other = vec![ring(&stranger, id, user("pn", THIRD_PN))];
    let moved = store
        .write("test: rehome", move |tx| rehome_lines(tx, &other))
        .await
        .expect("rehome");
    assert!(
        moved.is_none(),
        "a chat of somebody else stays where it was"
    );
}

/// The link's time and a chat the backend holds, for the tests of a call from before the link.
async fn linked_at(store: &KourierStore, ms: i64) {
    store
        .write("test: link", move |tx| resyncs::set_linked(tx, ms))
        .await
        .expect("link");
}

async fn state_chat(store: &KourierStore, conv: &ConvKey) {
    let conv = conv.clone();
    store
        .write("test: state a chat", move |tx| {
            features::state(tx, &conv).map(drop)
        })
        .await
        .expect("state");
}

async fn was_stated(store: &KourierStore, conv: &ConvKey) -> bool {
    let conv = conv.clone();
    store
        .read(move |conn| started::was_stated(conn, &conv))
        .await
        .expect("was_stated")
}

/// F13 (INTEROP-3, his answer "Only calls after link"): a record of the phone's log whose call is from BEFORE the link
/// adds no chat. It lands only in a chat the backend already holds (the line goes to the form of the person that chat
/// has), else it is `Unplaced`: no event, no `call_lines` note, no chat stated. A call after the link keeps his rule
/// ("if there is no chat - it should be added"). The time is the call's start, else the mutation's write time; the bound
/// is the link's time, strictly. A store with no link time treats nothing as old.
/// Mutants: the bound ignored; the bound by `now`; the line kept in the person's other form.
#[tokio::test]
async fn a_call_from_before_the_link_adds_no_chat_and_lands_only_in_one_that_exists() {
    let (_dir, owner) = open();
    let store = owner.store();
    // The calls below start at second 1,760,000,100 and are written at 1,760,009,999 s; the link is later than the first.
    const LINK_MS: i64 = 1_760_000_500_000;

    // A store that saw no link: nothing is old, the chat is added.
    let mut third = incoming("OLD-77310201", CallResult::MISSED);
    third.creator = Some(user("pn", THIRD_PN));
    let placed = run(&store, &third).await;
    assert_eq!(
        placed.verdict,
        Verdict::Line,
        "no link time: nothing is old"
    );
    assert!(was_stated(&store, &direct("pn", THIRD_PN)).await);

    linked_at(&store, LINK_MS).await;

    // Before the link, a person with no chat: no chat, no line, counted.
    let chat = direct("lid", PEER_LID);
    let placed = run(&store, &incoming("OLD-77310202", CallResult::MISSED)).await;
    assert_eq!(placed.verdict, Verdict::Unplaced);
    assert!(placed.events.is_empty(), "no statement, no line");
    assert!(!was_stated(&store, &chat).await, "no chat was stated");
    assert_eq!(
        noted_conv(&store, "OLD-77310202").await,
        None,
        "nothing was filed"
    );
    // The write time stands in for a start that is missing, on either side of the link.
    let mut no_start = incoming("OLD-77310203", CallResult::MISSED);
    no_start.started_s = None;
    no_start.written_ms = LINK_MS - 1;
    assert_eq!(run(&store, &no_start).await.verdict, Verdict::Unplaced);
    // A group call of a group the backend never held is a chat too.
    let mut in_group = incoming("OLD-77310204", CallResult::MISSED);
    in_group.group = Some(group());
    assert_eq!(run(&store, &in_group).await.verdict, Verdict::Unplaced);
    assert!(!was_stated(&store, &group()).await);

    // The same person after the link: the chat is added, quietly, with the line.
    let mut after = incoming("NEW-77310205", CallResult::MISSED);
    after.started_s = Some(LINK_MS / 1000 + 60);
    let placed = run(&store, &after).await;
    assert_eq!(placed.verdict, Verdict::Line);
    assert!(
        matches!(placed.events.first(), Some(BackendEvent::ConversationUpsert(p)) if p.key == chat),
        "the chat is stated first"
    );
    assert!(was_stated(&store, &chat).await);
    // The call at the link's own time is not older than it.
    let mut at_link = incoming("NEW-77310206", CallResult::MISSED);
    at_link.creator = Some(user("pn", FOURTH_PN));
    at_link.started_s = Some(LINK_MS / 1000);
    assert_eq!(run(&store, &at_link).await.verdict, Verdict::Line);
    // A record with no start whose write time is after the link is new.
    let mut written_after = incoming("NEW-77310207", CallResult::MISSED);
    written_after.started_s = None;
    written_after.written_ms = LINK_MS + 1;
    assert_eq!(run(&store, &written_after).await.verdict, Verdict::Line);

    // An old call of the person whose chat exists: its line, in that chat, with no second statement.
    let placed = run(&store, &incoming("OLD-77310208", CallResult::MISSED)).await;
    assert_eq!(placed.verdict, Verdict::Line);
    assert_eq!(placed.events.len(), 1, "the chat is stated already");
    assert_eq!(the_line(&placed).0, chat);
    // An old group call of a group that exists.
    state_chat(&store, &group()).await;
    let placed = run(&store, &in_group).await;
    assert_eq!(placed.verdict, Verdict::Line);
    assert_eq!(the_line(&placed).0, group());

    // The chat the backend holds is under the person's number; the log names his LID: the line goes there, not to a
    // second chat under the LID.
    learn(&store, "88800498127653", "15550179403628").await;
    let number_chat = direct("pn", "15550179403628");
    state_chat(&store, &number_chat).await;
    let mut other_form = incoming("OLD-77310209", CallResult::MISSED);
    other_form.creator = Some(user("lid", "88800498127653"));
    let placed = run(&store, &other_form).await;
    assert_eq!(placed.verdict, Verdict::Line);
    assert_eq!(placed.events.len(), 1, "no statement for a second chat");
    assert_eq!(the_line(&placed).0, number_chat);
    assert!(!was_stated(&store, &direct("lid", "88800498127653")).await);
}
