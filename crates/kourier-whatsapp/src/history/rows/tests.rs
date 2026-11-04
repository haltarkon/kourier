//! The feeder's second pass (W4 C12): a planned chunk cut into rows in the order of delivery, from any cursor,
//! within the row bounds. Every id, number and name is synthetic.

use kourier_db::{DbKey, Priority};
use kourier_model::UserId;
use wa::web_message_info::StubType;
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use super::{
    Body, Bounds, Cursor, Kept, Loaded, MAX_ROW_BYTES, Next, Phase, Reader, Row, deferred_n, load,
    part_n,
};
use crate::error::sql;
use crate::history::feed::{Entry, Planned, plan, scan};
use crate::history::map::Item;
use crate::history::stage::{MIGRATIONS, StageOwner, derive_key};
use crate::history::testing::{Chat, Chunk, Msg, SyncType};
use crate::map::{Ctx, OwnIds, conv_of};

const P1: &str = "15550100101@s.whatsapp.net";
const P1_LID: &str = "99900100101@lid";
const P2: &str = "15550100102@s.whatsapp.net";
const P3: &str = "15550100103@s.whatsapp.net";
const MEMBER: &str = "15550100104@s.whatsapp.net";
const GROUP: &str = "120363000000000001@g.us";
/// A broadcast list: the build has no conversation for it.
const LIST: &str = "12345@broadcast";

/// "Now" for the map, after every message time below.
const NOW: i64 = 1_700_000_500_000;
/// The chunk's stage number.
const SEQ: i64 = 7;

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", "99900100999").expect("lid")),
            pn: Some(UserId::from_parts("pn", "15550100999").expect("pn")),
        },
    }
}

/// A planned chunk: its bytes and its plan (the first pass is pure, no stage is needed).
struct Fixture {
    bytes: Vec<u8>,
    entries: Vec<Entry>,
    ctx: Ctx,
}

impl Fixture {
    fn of(chunk: &Chunk) -> Self {
        let bytes = chunk.blob();
        let entries = scan(&bytes, wacore::history_sync::MAX_DECOMPRESSED)
            .expect("scan")
            .entries;
        Self {
            bytes,
            entries,
            ctx: ctx(),
        }
    }

    fn reader(&self) -> Reader<'_> {
        Reader::new(&self.bytes, &self.entries, &self.ctx, NOW)
    }
}

/// The encoded conversation key the core knows a chat by.
fn key(jid: &str) -> String {
    conv_of(&jid.parse().expect("jid"))
        .expect("a conversation")
        .encode()
}

fn at(phase: Phase, conv: u32, msg: u32) -> Cursor {
    Cursor::at(SEQ, phase, conv, msg)
}

/// Every row from `start` to the end, each asked for at the previous row's `after`.
fn walk(reader: &mut Reader<'_>, start: Cursor, bounds: Bounds) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut here = start;
    for _ in 0..10_000 {
        match reader.next(here, bounds).expect("a row") {
            Next::Row(row) => {
                here = row.after;
                rows.push(row);
            }
            Next::End => return rows,
        }
    }
    panic!("the walk did not end");
}

/// What a row carries, as text: ids and reasons, never a body.
fn describe(row: &Row) -> String {
    match &row.body {
        Body::People { events, parts } => {
            let kept: Vec<i64> = parts.iter().map(|Kept { n, .. }| *n).collect();
            format!("people(events {}, parts {kept:?})", events.len())
        }
        Body::Chats(chats) => {
            let names: Vec<String> = chats.iter().map(|chat| chat.conv.encode()).collect();
            format!("chats{names:?}")
        }
        Body::Messages {
            conv,
            place,
            pieces,
            ..
        } => {
            let items: Vec<String> = pieces
                .iter()
                .map(|piece| match &piece.item {
                    Item::Message(message) => format!("{}:{}", piece.index, message.local),
                    Item::Deferred(reason) => format!("{}:deferred-{reason}", piece.index),
                    Item::Skip => panic!("a skip is never carried"),
                })
                .collect();
            format!("messages(conv {conv}, {}, {items:?})", place.jid)
        }
        Body::End { .. } => panic!("the reader never makes an end row"),
    }
}

/// `before -> after what` of every row: two walks that agree here agree on everything the zone sees.
fn shapes(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .map(|row| format!("{:?} -> {:?} {}", row.before, row.after, describe(row)))
        .collect()
}

/// Entries of a chat's list that a messages row consumed (the next chat's cursor starts again at 0, so the
/// plan's count of the chat stands in for it).
fn consumed(fixture: &Fixture, row: &Row) -> u32 {
    if row.after.phase == Phase::Messages && row.after.conv == row.before.conv {
        row.after.msg - row.before.msg
    } else {
        fixture.entries[row.before.conv as usize].count - row.before.msg
    }
}

fn bootstrap() -> Chunk {
    let mut stub = Msg::text(GROUP, "gs", 270, "").from(MEMBER);
    stub.info.message = MessageField::none();
    stub.info.message_stub_type = Some(StubType::GROUP_PARTICIPANT_ADD);
    let mut p3 = Chat::new(P3)
        // The phone lists the newest first, and two of these share a second.
        .message(Msg::text(P3, "x2", 200, "third"))
        .message(Msg::text(P3, "x1", 200, "second"))
        .message(Msg::text(P3, "x0", 100, "first"));
    // An entry with nothing in it: consumed by the cursor, carried nowhere.
    p3.conversation.messages.push(wa::HistorySyncMsg::default());
    Chunk::new(SyncType::INITIAL_BOOTSTRAP, 1, 40)
        .push_name(P1, "Ann Example")
        .pair(P1, P1_LID)
        .chat(
            Chat::new(P1)
                .lid(P1_LID)
                .unread(2)
                .active_at(100)
                .texts("a", 90, 3),
        )
        .chat(
            Chat::new(GROUP)
                .name("A group")
                .active_at(300)
                .message(stub)
                .message(Msg::text(GROUP, "g1", 260, "later").from(MEMBER))
                .message(Msg::text(GROUP, "g0", 250, "earlier").from(MEMBER)),
        )
        .chat(
            Chat::new("status@broadcast")
                .active_at(200)
                .texts("s", 150, 2),
        )
        .chat(p3)
        .chat(Chat::new(LIST).active_at(50).texts("l", 40, 2))
        .chat(Chat::new(P2))
}

#[test]
fn a_chunk_is_cut_into_people_then_chats_then_messages_in_the_order_of_delivery() {
    let fixture = Fixture::of(&bootstrap());
    // The plan: the group, P3, the status chat, P1, the list, the chat in which nothing happened.
    let order: Vec<&str> = fixture.entries.iter().map(|e| e.conv.as_str()).collect();
    assert_eq!(
        order,
        [GROUP, P3, "status@broadcast", P1_LID, LIST, P2],
        "the chats the plan places by their LID form"
    );

    let rows = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::DEFAULT);
    assert_eq!(
        shapes(&rows),
        [
            // The pair and the push name, in one row; no kept part in this chunk.
            format!(
                "{:?} -> {:?} people(events 2, parts [])",
                at(Phase::People, 0, 0),
                at(Phase::Chats, 0, 0)
            ),
            // The least recent chat first. The status chat, the list and the empty chat give no entry.
            format!(
                "{:?} -> {:?} chats{:?}",
                at(Phase::Chats, 0, 0),
                at(Phase::Messages, 0, 0),
                [key(P1_LID), key(P3), key(GROUP)]
            ),
            // The most recent chat first, its messages oldest first; a stub is kept, not shown.
            format!(
                "{:?} -> {:?} messages(conv 0, {GROUP}, [\"0:g0\", \"1:g1\", \"2:deferred-stub\"])",
                at(Phase::Messages, 0, 0),
                at(Phase::Messages, 1, 0)
            ),
            // Two messages of one second: the reverse of the phone's order, so x1 before x2. The empty
            // entry sorts first and is carried nowhere; the status chat (place 2) is walked over.
            format!(
                "{:?} -> {:?} messages(conv 1, {P3}, [\"1:x0\", \"2:x1\", \"3:x2\"])",
                at(Phase::Messages, 1, 0),
                at(Phase::Messages, 3, 0)
            ),
            format!(
                "{:?} -> {:?} messages(conv 3, {P1_LID}, [\"0:a0\", \"1:a1\", \"2:a2\"])",
                at(Phase::Messages, 3, 0),
                at(Phase::Messages, 4, 0)
            ),
            // A chat the build has no conversation for: everything is kept, nothing is shown. The empty
            // chat after it is walked over, and the chunk's content ends.
            format!(
                "{:?} -> {:?} messages(conv 4, {LIST}, [\"0:deferred-chat\", \"1:deferred-chat\"])",
                at(Phase::Messages, 4, 0),
                at(Phase::End, 0, 0)
            ),
        ]
    );
    assert!(
        matches!(
            fixture
                .reader()
                .next(at(Phase::End, 0, 0), Bounds::DEFAULT)
                .expect("end"),
            Next::End
        ),
        "nothing follows the content"
    );
}

#[test]
fn the_same_rows_from_a_mid_conversation_cursor() {
    let chunk = Chunk::new(SyncType::RECENT, 1, 60)
        .chat(Chat::new(P1).active_at(500).texts("a", 400, 7))
        .chat(Chat::new(P3).active_at(100).texts("c", 90, 2));
    let fixture = Fixture::of(&chunk);
    let bounds = Bounds {
        messages: 3,
        ..Bounds::DEFAULT
    };
    let mut reader = fixture.reader();
    let all = walk(&mut reader, Cursor::start(SEQ), bounds);
    let all_shapes = shapes(&all);
    // The chats row, then the first chat in rows of 3, 3, 1 and the second in one.
    assert_eq!(all.len(), 5, "people are empty: {all_shapes:?}");

    let middle = all
        .iter()
        .position(|row| row.before.phase == Phase::Messages && row.before.msg == 3)
        .expect("a row in the middle of the first chat");
    // A reader that never saw the rows before: the cursor alone is the state.
    let mut fresh = fixture.reader();
    let again = walk(&mut fresh, all[middle].before, bounds);
    assert_eq!(
        shapes(&again),
        all_shapes[middle..],
        "the rows after a kill are the rows that would have followed"
    );
    // The reader that read to the end is asked for an earlier row again (a taken-back row): the same row.
    let back = reader.next(all[middle].before, bounds).expect("row");
    let Next::Row(back) = back else {
        panic!("a row")
    };
    assert_eq!(shapes(&[back]), all_shapes[middle..=middle]);
}

#[test]
fn a_row_holds_at_most_500_messages_and_one_mib() {
    // 620 messages: 500 and 120.
    let long = Chunk::new(SyncType::RECENT, 1, 60)
        .chat(Chat::new(P1).active_at(9_000).texts("t", 1_000, 620));
    let fixture = Fixture::of(&long);
    let rows = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::DEFAULT);
    let sizes: Vec<usize> = rows
        .iter()
        .filter_map(|row| match &row.body {
            Body::Messages { pieces, .. } => Some(pieces.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [500, 120]);

    // 24 messages of 100 KiB and one of 2 MiB between them: ten fit a MiB, the big one travels alone.
    let body = "x".repeat(100 * 1024);
    let mut heavy = Chat::new(P3).active_at(9_000);
    for n in 0..25u64 {
        heavy = if n == 12 {
            heavy.message(Msg::text(
                P3,
                "big",
                2_000 + n,
                &"y".repeat(2 * 1024 * 1024),
            ))
        } else {
            heavy.message(Msg::text(P3, &format!("m{n}"), 2_000 + n, &body))
        };
    }
    let fixture = Fixture::of(&Chunk::new(SyncType::RECENT, 1, 60).chat(heavy));
    let rows = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::DEFAULT);
    let mut sizes = Vec::new();
    for row in &rows {
        let Body::Messages { pieces, .. } = &row.body else {
            continue;
        };
        let bytes: usize = pieces
            .iter()
            .map(|piece| {
                use waproto::buffa::Message as _;
                piece.info.encoded_len() as usize
            })
            .sum();
        assert!(
            bytes <= MAX_ROW_BYTES || pieces.len() == 1,
            "a row of {} messages holds {bytes} bytes",
            pieces.len()
        );
        sizes.push(pieces.len());
    }
    assert_eq!(
        sizes,
        [10, 2, 1, 10, 2],
        "the oversized message is a row of its own"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(row.body, Body::Messages { .. }))
            .map(|row| consumed(&fixture, row))
            .sum::<u32>(),
        25,
        "every entry is consumed once"
    );
}

#[test]
fn a_row_read_again_as_singles_covers_the_same_messages() {
    let fixture = Fixture::of(&bootstrap());
    let ids = |rows: &[Row]| -> Vec<String> {
        rows.iter()
            .filter_map(|row| match &row.body {
                Body::Messages { pieces, .. } => Some(pieces),
                _ => None,
            })
            .flatten()
            .map(|piece| match &piece.item {
                Item::Message(message) => message.local.to_string(),
                Item::Deferred(reason) => format!("deferred-{reason}"),
                Item::Skip => panic!("a skip is never carried"),
            })
            .collect()
    };
    let whole = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::DEFAULT);
    let singles = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::SINGLE);
    assert_eq!(
        ids(&singles),
        ids(&whole),
        "the same messages in the same order"
    );
    assert!(singles.len() > whole.len());
    for row in &singles {
        match &row.body {
            Body::People { events, .. } => assert!(events.len() <= 1),
            Body::Chats(chats) => assert_eq!(chats.len(), 1),
            // The empty entry of P3's list is consumed by a row of its own (nothing to carry).
            Body::Messages { pieces, .. } => {
                assert!(pieces.len() <= 1 && consumed(&fixture, row) == 1)
            }
            Body::End { .. } => panic!("the reader never makes an end row"),
        }
    }
}

#[test]
fn kept_parts_have_positions_after_the_people_events() {
    let mut chunk = Chunk::new(SyncType::RECENT, 2, 60)
        .push_name(P1, "Ann Example")
        .push_name(P2, "Bob Example")
        .push_name(P3, "Cy Example");
    let mut call = wa::CallLogRecord::default();
    call.call_id = Some("CALL-0001".to_owned());
    chunk.sync.call_log_records = vec![call];
    chunk.sync.global_settings = MessageField::some(wa::GlobalSettings::default());
    let fixture = Fixture::of(&chunk);
    let bounds = Bounds {
        people: 1,
        ..Bounds::DEFAULT
    };
    // One unit a row: the three events, then each part on a position of its own (LOW-4).
    let rows = walk(&mut fixture.reader(), Cursor::start(SEQ), bounds);
    assert_eq!(
        shapes(&rows),
        [
            format!(
                "{:?} -> {:?} people(events 1, parts [])",
                at(Phase::People, 0, 0),
                at(Phase::People, 0, 1)
            ),
            format!(
                "{:?} -> {:?} people(events 1, parts [])",
                at(Phase::People, 0, 1),
                at(Phase::People, 0, 2)
            ),
            format!(
                "{:?} -> {:?} people(events 1, parts [])",
                at(Phase::People, 0, 2),
                at(Phase::People, 0, 3)
            ),
            format!(
                "{:?} -> {:?} people(events 0, parts {:?})",
                at(Phase::People, 0, 3),
                at(Phase::People, 0, 4),
                [part_n(0)]
            ),
            format!(
                "{:?} -> {:?} people(events 0, parts {:?})",
                at(Phase::People, 0, 4),
                at(Phase::Chats, 0, 0),
                [part_n(1)]
            ),
        ]
    );
    // With the default bounds the small chunk is still one row: the events, then the parts that fit.
    let rows = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::DEFAULT);
    assert_eq!(
        shapes(&rows),
        [format!(
            "{:?} -> {:?} people(events 3, parts {:?})",
            at(Phase::People, 0, 0),
            at(Phase::Chats, 0, 0),
            [part_n(0), part_n(1)]
        )]
    );

    // A chunk of only kept parts: one row without events, and then the end.
    let mut only = Chunk::new(SyncType::NON_BLOCKING_DATA, 1, 100);
    only.sync.global_settings = MessageField::some(wa::GlobalSettings::default());
    let fixture = Fixture::of(&only);
    let rows = walk(&mut fixture.reader(), Cursor::start(SEQ), Bounds::DEFAULT);
    assert_eq!(
        shapes(&rows),
        [format!(
            "{:?} -> {:?} people(events 0, parts {:?})",
            at(Phase::People, 0, 0),
            at(Phase::Chats, 0, 0),
            [part_n(0)]
        )]
    );

    // The numbers of the kept messages and of the kept parts never meet.
    assert_ne!(deferred_n(0, 5), deferred_n(1, 0));
    assert!(deferred_n(1 << 29, u32::MAX) < part_n(0));
}

#[test]
fn a_chunk_with_nothing_to_deliver_has_no_rows() {
    let chunk = Chunk::new(SyncType::RECENT, 1, 100)
        .chat(Chat::new("status@broadcast").active_at(9).texts("s", 1, 3))
        .chat(Chat::new(P2));
    let fixture = Fixture::of(&chunk);
    let mut reader = fixture.reader();
    assert!(matches!(
        reader
            .next(Cursor::start(SEQ), Bounds::DEFAULT)
            .expect("next"),
        Next::End
    ));
}

#[test]
fn a_planned_chunk_loads_from_the_stage_and_reads_the_same_rows() {
    let dir = tempfile::tempdir().expect("dir");
    let key = derive_key(&DbKey::from([0x22; 32]));
    let stage = StageOwner::open(
        &dir.path().join("history").join("stage.db"),
        &key,
        "the account",
        &MIGRATIONS,
    )
    .expect("open the stage")
    .0;
    let chunk = bootstrap();
    let blob = chunk.blob();
    let seq = stage
        .handle()
        .write_blocking(Priority::High, "test capture", None, move |tx| {
            tx.execute(
                "INSERT INTO chunks (msg_id, captured_at, size, state) \
                 VALUES ('N1', 1, ?1, 'captured')",
                [blob.len() as i64],
            )
            .map_err(sql("test capture"))?;
            let seq = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, ?2)",
                (seq, blob),
            )
            .map_err(sql("test capture"))?;
            Ok(seq)
        })
        .expect("capture");
    assert!(
        load(&stage.handle(), seq).expect("load").is_none(),
        "a chunk that was not planned is not loaded"
    );
    assert!(matches!(
        plan(&stage.handle(), seq).expect("plan"),
        Planned::Read { chats: 6, .. }
    ));
    let Loaded {
        msg_id: _,
        bytes,
        entries,
        sync_type,
        order,
        progress,
        skipped,
    } = load(&stage.handle(), seq).expect("load").expect("planned");
    assert_eq!(
        (sync_type, order, progress, skipped),
        (
            Some(SyncType::INITIAL_BOOTSTRAP as i32),
            Some(1),
            Some(40),
            0
        )
    );
    let ctx = ctx();
    let from_stage = walk(
        &mut Reader::new(&bytes, &entries, &ctx, NOW),
        Cursor::start(seq),
        Bounds::DEFAULT,
    );
    let pure = Fixture::of(&chunk);
    let direct = walk(&mut pure.reader(), Cursor::start(seq), Bounds::DEFAULT);
    assert_eq!(shapes(&from_stage), shapes(&direct));
    assert_eq!(from_stage.len(), 6);
}

/// LOW-4: a chunk with a great deal of kept material is cut by the row's byte bound, a part is never split, every part
/// is in exactly one row and keeps its own number, and the first unit of a row goes in however large it is.
/// Mutant for the re-check: the byte test removed from `people_row` (all parts in the first row again).
#[test]
fn many_large_kept_parts_are_cut_into_rows_by_the_byte_bound() {
    let mut chunk = Chunk::new(SyncType::RECENT, 2, 60).push_name(P1, "Ann Example");
    chunk.sync.inline_contacts = (0..5)
        .map(|n| {
            let mut contact = wa::InlineContact::default();
            // 300 bytes of name: a part of about 303 encoded bytes.
            contact.full_name = Some(format!("{n}").repeat(300));
            contact
        })
        .collect();
    let fixture = Fixture::of(&chunk);
    let bounds = Bounds {
        bytes: 700,
        ..Bounds::DEFAULT
    };
    let mut reader = fixture.reader();
    let rows = walk(&mut reader, Cursor::start(SEQ), bounds);
    let per_row: Vec<(usize, usize)> = rows
        .iter()
        .map(|row| match &row.body {
            Body::People { events, parts } => (events.len(), parts.len()),
            other => panic!("not a people row: {other:?}"),
        })
        .collect();
    // 303 bytes a part: two fit in 700 (the event started the first row, which takes two too), the third waits.
    assert!(
        per_row.len() >= 3,
        "the parts were cut into rows: {per_row:?}"
    );
    for row in &rows {
        if let Body::People { parts, .. } = &row.body {
            let size: usize = parts.iter().map(|kept| kept.part.item.len()).sum();
            assert!(
                parts.len() <= 1 || size <= 700,
                "a row with several parts stays inside the bound: {size}"
            );
        }
    }
    let mut numbers: Vec<i64> = rows
        .iter()
        .flat_map(|row| match &row.body {
            Body::People { parts, .. } => parts.iter().map(|kept| kept.n).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        (0..5).map(part_n).collect::<Vec<_>>(),
        "every part is in exactly one row, with its own number"
    );
    // The walk is a function of the position: asking again from any row's start gives the same row.
    for row in &rows {
        let again = fixture.reader().next(row.before, bounds).expect("next");
        let Next::Row(again) = again else {
            panic!("no row again at {:?}", row.before)
        };
        assert_eq!((again.before, again.after), (row.before, row.after));
    }
}
