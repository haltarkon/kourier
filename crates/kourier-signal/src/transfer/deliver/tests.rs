//! The feeder on its own: the order and the sizes of the delivery rows, where a restart
//! goes on, and what a piece that was taken back becomes. Synthetic data, no account.

use kourier_model::{Body, Origin, UserId};

use super::*;
use crate::transfer::stage::{ChatCounters, StageLimits, StageWriter, StagedChat};

const EPHEMERAL: [u8; 32] = [0x51; 32];

fn key() -> StageKey {
    StageKey::derive(&EPHEMERAL)
}

fn peer(i: u32) -> UserId {
    UserId::parse(&format!("aci:00000000-0000-4000-8000-{i:012x}")).unwrap()
}

fn message(chat: u32, n: u64, len: usize) -> IncomingMessage {
    let mut msg = IncomingMessage::new(
        ConvKey::Direct(peer(chat)),
        (1_000 + n).to_string(),
        Origin::Remote {
            author: peer(chat),
            device: None,
        },
        Body::text("x".repeat(len)),
        i64::try_from(1_000 + n).unwrap(),
        i64::try_from(1_001 + n).unwrap(),
    );
    msg.silent = true;
    msg
}

/// A stage of `sizes.len()` chats; chat `i` has `sizes[i]` messages of `len` bytes of text,
/// interleaved over the chats, and is the more recent the higher `i` is.
fn stage(dir: &std::path::Path, sizes: &[u64], len: usize) -> Recorded {
    let mut writer = StageWriter::create(dir, &key(), StageLimits::default()).unwrap();
    for (i, _) in sizes.iter().enumerate() {
        let i = i as u32;
        let contact = StagedContact {
            user: peer(i),
            aci: None,
            pni: None,
            e164: None,
            profile_key: None,
            profile_sharing: true,
            blocked: i == 1,
            hidden: false,
            unregistered_at: None,
            profile_name: None,
            system_name: None,
            nickname: None,
        };
        writer.contact(i, &contact).unwrap();
        let chat = StagedChat {
            is_self: false,
            peer: Some(i),
        };
        writer
            .chat(i, &ConvKey::Direct(peer(i)), &chat, None)
            .unwrap();
    }
    let mut n = 0;
    let mut left: Vec<u64> = sizes.to_vec();
    while left.iter().any(|l| *l > 0) {
        for (i, l) in left.iter_mut().enumerate() {
            if *l > 0 {
                n += 1;
                *l -= 1;
                writer
                    .item(i as u32, n, &message(i as u32, n, len))
                    .unwrap();
            }
        }
    }
    let counters: Vec<ChatCounters> = sizes
        .iter()
        .enumerate()
        .map(|(i, rows)| ChatCounters {
            idx: i as u32,
            n_rows: *rows,
            n_loud: *rows,
            last_at: 1_000 * (i as i64 + 1),
        })
        .collect();
    let totals = writer.commit(&counters, n + 7).unwrap();
    Recorded {
        messages: totals.messages,
        chats: totals.chats,
        frames: n + 7,
    }
}

fn fed(
    dir: &std::path::Path,
    recorded: Recorded,
    start: &FeedStart,
    bounds: DeliveryBounds,
) -> Vec<Fed> {
    let (tx, mut rx) = mpsc::channel(100_000);
    feed(dir, &key(), recorded, start, bounds, &tx);
    drop(tx);
    let mut out = Vec::new();
    while let Ok(fed) = rx.try_recv() {
        out.push(fed);
    }
    out
}

fn from_the_start() -> FeedStart {
    FeedStart {
        cursor: Cursor::START,
        redo: Vec::new(),
        delivered: 0,
    }
}

fn chunks(fed: Vec<Fed>) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut ended = false;
    for item in fed {
        match item {
            Fed::Chunk(chunk) => {
                assert!(!ended, "nothing follows the end");
                out.push(*chunk);
            }
            Fed::End { whole } => {
                assert!(whole, "the rows read agree with the counters");
                ended = true;
            }
            Fed::Failed(e) => panic!("the feeder failed: {e}"),
        }
    }
    assert!(ended, "the feeder said it was through");
    out
}

#[test]
fn people_come_first_then_the_chats_then_each_chat_most_recent_first_in_frame_order() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage(dir.path(), &[5, 0, 1200, 7], 20);
    let all = chunks(fed(
        dir.path(),
        recorded,
        &from_the_start(),
        DeliveryBounds::DEFAULT,
    ));
    // Every chunk begins where the one before it ended: the cursor is a chain.
    let mut cursor = Cursor::START;
    for chunk in &all {
        let Place::Advance { before, after } = chunk.place else {
            panic!("no piece is appended again here");
        };
        assert_eq!(before, cursor, "a chunk out of step");
        cursor = after;
    }
    let payloads: Vec<&Payload> = all
        .iter()
        .map(|chunk| &chunk.payload)
        .filter(|payload| !matches!(payload, Payload::Contacts(list) if list.is_empty()))
        .collect();
    let Payload::Contacts(people) = payloads[0] else {
        panic!("people first");
    };
    assert_eq!(people.len(), 4);
    let Payload::Chats(chats) = payloads[1] else {
        panic!("then the chats");
    };
    // Chat 1 has no row that shows it. The others are upserted the LEAST recent first (the
    // core puts the chat it heard of last on top) and filled the most recent first.
    let order: Vec<u32> = chats.iter().map(|(chat, _)| chat.counters.idx).collect();
    assert_eq!(order, [0, 2, 3]);
    assert!(chats.iter().all(|(_, blocked)| !blocked));
    let mut seen: Vec<(String, usize, bool)> = Vec::new();
    let mut last_progress = 0.0;
    for payload in &payloads[2..] {
        let Payload::Messages {
            conv,
            messages,
            has_more,
            progress,
        } = payload
        else {
            panic!("then messages only");
        };
        assert!(messages.len() <= MAX_HISTORY_BATCH);
        let locals: Vec<u64> = messages.iter().map(|m| m.local.parse().unwrap()).collect();
        assert!(locals.windows(2).all(|w| w[0] < w[1]), "frame order");
        assert!(messages.iter().all(|m| m.conv == *conv));
        let progress = progress.expect("a fraction");
        assert!(progress >= last_progress && progress <= 1.0);
        last_progress = progress;
        seen.push((conv.encode(), messages.len(), *has_more));
    }
    assert!((last_progress - 1.0).abs() < f32::EPSILON);
    let conv = |i: u32| ConvKey::Direct(peer(i)).encode();
    assert_eq!(
        seen,
        [
            (conv(3), 7, false),
            (conv(2), 500, true),
            (conv(2), 500, true),
            (conv(2), 200, false),
            // Batches and nothing else: no unread count rides on a chat's last batch
            // (the rows say what is unread).
            (conv(0), 5, false),
        ]
    );
}

#[test]
fn batches_never_exceed_500_messages_or_the_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    // 300 messages of about 8 KiB each: the byte budget cuts before the count does.
    let recorded = stage(dir.path(), &[300], 8 * 1024);
    let bounds = DeliveryBounds::DEFAULT;
    let all = chunks(fed(dir.path(), recorded, &from_the_start(), bounds));
    let mut total = 0;
    let mut rows = 0;
    for chunk in &all {
        if let Payload::Messages { messages, .. } = &chunk.payload {
            let bytes: usize = messages
                .iter()
                .map(|m| serde_json::to_vec(m).unwrap().len())
                .sum();
            assert!(bytes as u64 <= bounds.max_bytes, "{bytes} bytes in a row");
            assert!(messages.len() <= bounds.max_messages);
            total += messages.len();
            rows += 1;
        }
    }
    assert_eq!(total, 300);
    assert!(rows >= 3, "about a megabyte per row");
    // A message larger than the budget travels alone.
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage(dir.path(), &[3], 600 * 1024);
    let small = DeliveryBounds {
        max_bytes: 100 * 1024,
        ..bounds
    };
    let sizes: Vec<usize> = chunks(fed(dir.path(), recorded, &from_the_start(), small))
        .iter()
        .filter_map(|chunk| match &chunk.payload {
            Payload::Messages { messages, .. } => Some(messages.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [1, 1, 1]);
}

#[test]
fn a_feeder_goes_on_where_the_cursor_stands() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage(dir.path(), &[5, 1200], 20);
    let all = chunks(fed(
        dir.path(),
        recorded,
        &from_the_start(),
        DeliveryBounds::DEFAULT,
    ));
    // Stop after the first batch of chat 1 (the more recent one).
    let at = all
        .iter()
        .position(|chunk| matches!(chunk.payload, Payload::Messages { .. }))
        .unwrap();
    let Place::Advance { after, .. } = all[at].place else {
        panic!("an advance");
    };
    assert_eq!((after.phase, after.pos), (PHASE_MESSAGES, 0));
    let start = FeedStart {
        cursor: after,
        redo: Vec::new(),
        delivered: 500,
    };
    let rest = chunks(fed(dir.path(), recorded, &start, DeliveryBounds::DEFAULT));
    let sizes: Vec<usize> = rest
        .iter()
        .filter_map(|chunk| match &chunk.payload {
            Payload::Messages { messages, .. } => Some(messages.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sizes, [500, 200, 5]);
    assert!(matches!(rest[0].place, Place::Advance { before, .. } if before == after));
}

#[test]
fn a_piece_that_was_taken_back_comes_as_single_messages_before_anything_else() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage(dir.path(), &[4, 6], 20);
    let all = chunks(fed(
        dir.path(),
        recorded,
        &from_the_start(),
        DeliveryBounds::DEFAULT,
    ));
    // The batch of chat 0 (four messages, the chat's last) was taken back.
    let taken = all
        .iter()
        .find(|chunk| chunk.kind == KIND_MESSAGES && chunk.chat == 0)
        .unwrap();
    let Place::Advance { after, .. } = taken.place else {
        panic!("an advance");
    };
    let start = FeedStart {
        cursor: after,
        redo: vec![Piece {
            id: 9,
            kind: KIND_MESSAGES,
            chat: 0,
            after_n: taken.after_n,
            through_n: taken.through_n,
            ends_chat: true,
        }],
        delivered: 10,
    };
    let again = chunks(fed(dir.path(), recorded, &start, DeliveryBounds::DEFAULT));
    assert_eq!(
        again.len(),
        4,
        "four singles and nothing after the cursor's end"
    );
    let mut before = taken.after_n;
    for (i, chunk) in again.iter().enumerate() {
        let last = i == 3;
        assert_eq!(
            chunk.place,
            Place::Redo {
                piece: 9,
                before,
                last
            }
        );
        assert!(chunk.single);
        let Payload::Messages {
            messages, has_more, ..
        } = &chunk.payload
        else {
            panic!("a message");
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(*has_more, !last, "the chat's end rides on the last single");
        before = chunk.through_n;
    }
    assert_eq!(before, taken.through_n);
}

#[test]
fn a_stage_that_is_not_the_recorded_one_or_has_a_damaged_page_is_not_fed() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage(dir.path(), &[2000], 200);
    let other = Recorded {
        messages: recorded.messages + 1,
        ..recorded
    };
    let out = fed(
        dir.path(),
        other,
        &from_the_start(),
        DeliveryBounds::DEFAULT,
    );
    assert!(matches!(
        out.as_slice(),
        [Fed::Failed(StageError::Unreadable)]
    ));
    // One byte of a page in the middle of the file: the rows before it are fed, the row
    // that needs the page is not, and the feeder says the stage cannot be read.
    let path = crate::transfer::stage::path_in(dir.path());
    let mut bytes = std::fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();
    let out = fed(
        dir.path(),
        recorded,
        &from_the_start(),
        DeliveryBounds::DEFAULT,
    );
    assert!(
        matches!(out.last(), Some(Fed::Failed(_))),
        "a damaged page is refused when it is read"
    );
    assert!(!out.iter().any(|fed| matches!(fed, Fed::End { .. })));
}

// The bookkeeping of the delivery rows, on `backend.db` alone.

const NOW: i64 = 1_700_000_000_000;

fn reading_db(dir: &std::path::Path) -> ZoneDb {
    use crate::transfer::{ArchivePointer, Step, WAIT, apply, begin, commit_import};
    let db = crate::db::open(
        &dir.join("backend.db"),
        &kourier_backend_api::DbKey::new([3; 32]),
    )
    .unwrap();
    db.zone(|tx| begin(tx, &EPHEMERAL, NOW, WAIT)).unwrap();
    let pointer = Step::Pointer(ArchivePointer {
        cdn: 3,
        key: "abc".to_owned(),
    });
    db.zone(|tx| apply(tx, &pointer, NOW)).unwrap();
    db.zone(|tx| apply(tx, &Step::Downloaded, NOW)).unwrap();
    let totals = crate::transfer::StageTotals {
        messages: 10,
        chats: 1,
        ..Default::default()
    };
    assert!(db.zone(|tx| commit_import(tx, &totals, 20, NOW)).unwrap());
    db
}

/// A delivery row in `core_outbox` with its entry; returns its seq.
fn in_flight(db: &ZoneDb, single: bool, ends_chat: bool, messages: i64) -> i64 {
    db.zone(|tx| -> Result<i64> {
        let seq = tx.outbox_append(b"row", NOW)?;
        tx.execute(
            "INSERT INTO history_delivery(seq, kind, chat, after_n, through_n, ends_chat,
               single, messages) VALUES (?1, ?2, 4, 10, 20, ?3, ?4, ?5)",
            (seq, KIND_MESSAGES, ends_chat, single, messages),
        )?;
        Ok(seq)
    })
    .unwrap()
}

fn int(db: &ZoneDb, sql: &str) -> i64 {
    db.read(|conn| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) })
        .unwrap()
}

#[test]
fn an_acknowledged_row_is_settled_and_a_refused_one_comes_back_as_a_piece() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let acked = in_flight(&db, false, false, 5);
    let refused = in_flight(&db, false, true, 5);
    let waiting = in_flight(&db, false, false, 5);
    db.zone(|tx| -> Result<()> {
        tx.execute(
            "UPDATE history_transfer SET deliver_idle_ms = 500, deliver_strikes = 1",
            [],
        )?;
        // The core acknowledged one and refused the other for good (the forwarder's work).
        tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [acked])?;
        kourier_db::outbox::quarantine(tx, refused, "refused", NOW)?;
        Ok(())
    })
    .unwrap();
    let found = settle(&db, MAX_REFUSED_RUN).unwrap();
    assert_eq!((found.answered, found.waiting), (2, 1));
    assert!(found.new_pieces && found.pending);
    // The acknowledged row's entry is gone; the refused one is a piece without a row, and
    // its copy in the poison table is not kept; the third still waits.
    assert_eq!(int(&db, "SELECT count(*) FROM history_delivery"), 2);
    assert_eq!(
        int(
            &db,
            "SELECT count(*) FROM history_delivery
              WHERE seq IS NULL AND after_n = 10 AND through_n = 20 AND ends_chat = 1"
        ),
        1
    );
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox_poison"), 0);
    assert_eq!(
        int(
            &db,
            &format!("SELECT count(*) FROM history_delivery WHERE seq = {waiting}")
        ),
        1
    );
    // An answer of the core ends the count of the time without one, and the strikes.
    assert_eq!(
        int(
            &db,
            "SELECT deliver_idle_ms + deliver_strikes FROM history_transfer"
        ),
        0
    );
    // A second look changes nothing.
    let again = settle(&db, MAX_REFUSED_RUN).unwrap();
    assert_eq!((again.answered, again.waiting), (0, 1));
    assert!(!again.new_pieces && again.pending);
}

#[test]
fn a_refused_single_is_dropped_and_counted_and_its_chat_still_gets_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let single = in_flight(&db, true, true, 1);
    db.zone(|tx| -> Result<bool> {
        Ok(kourier_db::outbox::quarantine(tx, single, "refused", NOW)?)
    })
    .unwrap();
    let found = settle(&db, MAX_REFUSED_RUN).unwrap();
    assert_eq!(found.answered, 1);
    assert_eq!(int(&db, "SELECT deliver_dropped FROM history_transfer"), 1);
    // What is left is the end of chat 4, owed on its own.
    assert_eq!(
        int(
            &db,
            &format!(
                "SELECT count(*) FROM history_delivery
                  WHERE seq IS NULL AND kind = {KIND_END} AND chat = 4"
            )
        ),
        1
    );
    assert_eq!(int(&db, "SELECT count(*) FROM history_delivery"), 1);
    // The refused single's copy does not stay in the poison table: a refused history must
    // not pile up in `backend.db` with its content. A single that ends its chat leaves the
    // chat's end owed, which is a piece to append again.
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox_poison"), 0);
    assert!(found.new_pieces);
}

#[test]
fn a_row_unacknowledged_over_three_starts_is_taken_back_and_counts_a_strike() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let row = in_flight(&db, false, false, 5);
    let nothing = Duration::ZERO;
    for start in 1..=MAX_STARTS {
        note_start(&db).unwrap();
        assert_eq!(
            strike(&db, Some(MAX_STARTS), nothing).unwrap(),
            Struck::default(),
            "start {start}: still within the bound"
        );
    }
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox"), 1);
    note_start(&db).unwrap();
    let struck = strike(&db, Some(MAX_STARTS), nothing).unwrap();
    assert_eq!((struck.strikes, struck.taken, struck.piece), (1, 1, true));
    // Taken back: its row is gone from `core_outbox`, its content is a piece again.
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox"), 0);
    assert_eq!(
        int(
            &db,
            "SELECT count(*) FROM history_delivery WHERE seq IS NULL AND starts = 0"
        ),
        1
    );
    let _ = row;
    // The running-time bound takes back whatever waits, and a single is dropped.
    in_flight(&db, true, false, 1);
    let struck = strike(&db, None, Duration::from_secs(600)).unwrap();
    assert_eq!((struck.strikes, struck.taken, struck.piece), (2, 1, false));
    assert_eq!(struck.stuck, Duration::from_secs(600));
    assert_eq!(int(&db, "SELECT deliver_dropped FROM history_transfer"), 1);
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox"), 0);
    // Nothing waits: no strike.
    assert_eq!(
        strike(&db, None, Duration::from_secs(600)).unwrap(),
        Struck::default()
    );
    assert_eq!(int(&db, "SELECT deliver_strikes FROM history_transfer"), 2);
}

#[test]
fn a_lost_stage_resets_the_cursor_and_keeps_the_total_of_the_stuck_time() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    in_flight(&db, false, false, 5);
    let moved = "UPDATE history_transfer SET deliver_phase = 2, deliver_pos = 7, deliver_n = 99,
        delivered_messages = 123, deliver_began = 1, deliver_strikes = 1,
        deliver_refused = 4, deliver_stuck_ms = 99";
    db.zone(|tx| -> Result<usize> { Ok(tx.execute(moved, [])?) })
        .unwrap();
    db.zone(|tx| forget(tx, NOW)).unwrap();
    let state = "SELECT import_staged || '/' || read_open || '/' || deliver_phase || '/'
        || deliver_pos || '/' || deliver_n || '/' || delivered_messages || '/'
        || deliver_began || '/' || deliver_strikes || '/'
        || deliver_refused || '/' || deliver_stuck_ms
        FROM history_transfer";
    let text = |db: &ZoneDb| -> String {
        db.read(|conn| -> Result<String> { Ok(conn.query_row(state, [], |r| r.get(0))?) })
            .unwrap()
    };
    // The record is taken back and the read marked open (the read that follows counts);
    // the cursor is at its start, the run of refused units is over; the total of the
    // stuck time stays (it is the delivery's, however often the stage is read), and the
    // core still holds a part of the import.
    assert_eq!(text(&db), "0/1/0/0/-1/0/1/0/0/99");
    assert!(db.read(began).unwrap());
    assert_eq!(int(&db, "SELECT count(*) FROM history_delivery"), 0);
    // The row itself is still on its way to the core.
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox"), 1);
    // A rebuilt core has nothing: everything starts over. Only an import that was being
    // delivered is touched.
    db.zone(|tx| -> Result<usize> { Ok(tx.execute(moved, [])?) })
        .unwrap();
    assert!(db.zone(|tx| rebuild(tx, NOW)).unwrap());
    assert_eq!(text(&db), "0/1/0/0/-1/0/0/0/0/0");
    assert!(
        !db.zone(|tx| rebuild(tx, NOW)).unwrap(),
        "nothing to start over"
    );
}

/// What `archive_testing::delivery_rows` gives a test of another crate: the same rows in
/// the same order, as events.
#[test]
fn the_plain_rows_are_the_feeders_chunks_as_events() {
    let dir = tempfile::tempdir().unwrap();
    stage(dir.path(), &[5, 0, 600], 20);
    let rows = plain_rows(dir.path(), &key(), DeliveryBounds::DEFAULT).unwrap();
    let names: Vec<Vec<&str>> = rows
        .iter()
        .map(|row| row.iter().map(BackendEvent::name).collect())
        .collect();
    assert_eq!(
        names,
        [
            vec!["ContactUpsert"; 3],
            vec!["ConversationUpsert"; 2],
            vec!["HistoryBatch"],
            vec!["HistoryBatch"],
            // A chat's last batch is a batch and nothing else: no unread count is sent.
            vec!["HistoryBatch"],
        ]
    );
    for row in rows {
        let (envelope, _waiter) = Envelope::durable(row);
        envelope
            .validate()
            .expect("every row passes the contract check");
    }
}

/// Probes of the independent review of step S2.
mod review;
