//! Probes of the independent review of step S2 (`target/s2-context/review.md`): the zone of
//! a chunk on a real `backend.db` with an account behind it, which the builder's unit tests
//! do not run (they test the feeder and the bookkeeping apart).

use kourier_backend_api::decode_outbox_row;
use kourier_db::OutboxSource as _;

use super::*;
use crate::actor::testing;
use crate::receive::tests::fixture;
use crate::transfer::{ArchivePointer, StageTotals, Step, WAIT, apply, begin, commit_import};

pub(super) fn aci(i: u32) -> uuid::Uuid {
    uuid::Uuid::parse_str(&format!("00000000-0000-4000-8000-{i:012x}")).unwrap()
}

/// A stage as [`stage`] makes it, but of people the backend can keep rows for (they have an
/// ACI), every chat accepted on the phone.
fn stage_of_people(dir: &std::path::Path, sizes: &[u64]) -> Recorded {
    let mut writer = StageWriter::create(dir, &key(), StageLimits::default()).unwrap();
    for (i, _) in sizes.iter().enumerate() {
        let i = i as u32;
        let contact = StagedContact {
            user: peer(i),
            aci: Some(aci(i)),
            pni: None,
            e164: None,
            profile_key: None,
            profile_sharing: true,
            blocked: false,
            hidden: false,
            unregistered_at: None,
            profile_name: None,
            system_name: None,
            nickname: None,
        };
        writer.contact(i, &contact).unwrap();
        let chat = StagedChat::direct(i);
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
                writer.item(i as u32, n, &message(i as u32, n, 10)).unwrap();
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

/// The stored transfer of `db` reads, and its stage is recorded.
pub(super) fn reading(db: &ZoneDb, recorded: Recorded) {
    db.zone(|tx| begin(tx, &EPHEMERAL, NOW, WAIT)).unwrap();
    let pointer = Step::Pointer(ArchivePointer {
        cdn: 3,
        key: "abc".to_owned(),
    });
    db.zone(|tx| apply(tx, &pointer, NOW)).unwrap();
    db.zone(|tx| apply(tx, &Step::Downloaded, NOW)).unwrap();
    let totals = StageTotals {
        messages: recorded.messages,
        chats: recorded.chats,
        ..Default::default()
    };
    assert!(
        db.zone(|tx| commit_import(tx, &totals, recorded.frames, NOW))
            .unwrap()
    );
}

/// What a feeder that starts from the stored state of `db` hands over. The feeder blocks,
/// so it runs on a thread of its own, as in the application.
pub(super) fn fed_from(db: &ZoneDb, dir: &std::path::Path, recorded: Recorded) -> Vec<Chunk> {
    let start = db.read(load_start).unwrap();
    let dir = dir.to_owned();
    std::thread::spawn(move || chunks(fed(&dir, recorded, &start, DeliveryBounds::DEFAULT)))
        .join()
        .unwrap()
}

pub(super) fn outbox(db: &ZoneDb) -> Vec<(i64, Vec<BackendEvent>)> {
    db.read(|c| -> Result<Vec<(i64, Vec<u8>)>> {
        let mut stmt = c.prepare("SELECT seq, envelope FROM core_outbox ORDER BY seq")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .unwrap()
    .into_iter()
    .map(|(seq, bytes)| (seq, decode_outbox_row(&bytes).unwrap()))
    .collect()
}

fn with_features(events: &[BackendEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.features.is_some()))
        .count()
}

/// What the zone of a chunk may write, as numbers: rows of `core_outbox`, entries, the
/// cursor, the delivery's counts, rows of `peers`.
fn written(db: &ZoneDb) -> (i64, i64, i64, i64, i64, i64, i64) {
    (
        int(db, "SELECT count(*) FROM core_outbox"),
        int(db, "SELECT count(*) FROM history_delivery"),
        int(
            db,
            "SELECT deliver_phase * 1000000 + deliver_pos * 1000 + deliver_n FROM history_transfer",
        ),
        int(db, "SELECT deliver_rows FROM history_transfer"),
        int(db, "SELECT delivered_messages FROM history_transfer"),
        int(db, "SELECT count(*) FROM peers"),
        int(
            db,
            "SELECT coalesce(sum(after_n), 0) FROM history_delivery WHERE seq IS NULL",
        ),
    )
}

/// EXACTLY ONCE, the look at the stored state (step 1 of the zone). The builder's report
/// says that removing the look survives every test, because none hands the zone a chunk
/// that is out of step. This one does, for both kinds of place: a chunk the cursor has
/// moved past, and a single of a piece that was appended already. Each is refused and
/// NOTHING of it is written (no `peers` row, no `core_outbox` row, no entry, no count).
#[tokio::test(flavor = "current_thread")]
async fn review_a_chunk_that_does_not_continue_the_stored_state_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_people(dir.path(), &[3, 4]);
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;

    // Two feeders from the same stored state: the second one's first chunk is a copy.
    let mut first = fed_from(&shared.db, dir.path(), recorded);
    let mut copy = fed_from(&shared.db, dir.path(), recorded);
    let people = first.remove(0);
    assert!(matches!(&people.payload, Payload::Contacts(list) if list.len() == 2));
    let done = append(&shared, people, NOW).unwrap();
    let people_seq = done.seq.expect("the people's row");
    let before = written(&shared.db);
    assert_eq!((before.0, before.1, before.5), (1, 1, 2));
    let again = append(&shared, copy.remove(0), NOW);
    assert!(
        matches!(again, Err(Error::Internal("out of step"))),
        "a chunk the cursor has moved past was appended: {again:?}",
        again = again.map(|done| done.seq)
    );
    assert_eq!(written(&shared.db), before, "and it wrote something");
    // A chunk from further on, handed over too early, is refused as well.
    let too_early = first.remove(1);
    assert!(matches!(
        append(&shared, too_early, NOW),
        Err(Error::Internal("out of step"))
    ));
    assert_eq!(written(&shared.db), before);

    // The same for a piece that is appended again as singles: its first single twice.
    shared
        .db
        .quarantine(people_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    let mut singles = fed_from(&shared.db, dir.path(), recorded);
    let mut copy = fed_from(&shared.db, dir.path(), recorded);
    assert!(matches!(singles[0].place, Place::Redo { last: false, .. }));
    assert!(matches!(singles[1].place, Place::Redo { last: true, .. }));
    append(&shared, singles.remove(0), NOW).unwrap();
    let before = written(&shared.db);
    assert!(matches!(
        append(&shared, copy.remove(0), NOW),
        Err(Error::Internal("out of step"))
    ));
    assert_eq!(written(&shared.db), before, "a single was appended twice");
    // The piece's last single ends it; a copy of it finds no piece.
    append(&shared, singles.remove(0), NOW).unwrap();
    let before = written(&shared.db);
    assert!(matches!(
        append(&shared, copy.remove(0), NOW),
        Err(Error::Internal("out of step"))
    ));
    assert_eq!(written(&shared.db), before);
    assert_eq!(
        int(
            &shared.db,
            "SELECT count(*) FROM history_delivery WHERE seq IS NULL"
        ),
        0
    );
}

/// DEFECT (review finding 1). The zone of a row of chats records in `peers.features_hash`
/// that the core has each chat's features, because the same row carries them. When that
/// row is taken back (the core refused it, or it was not acknowledged within the bounds:
/// `take_back` deletes it from `core_outbox`), its chats are appended again as singles,
/// but `features::features_event` finds the hash recorded and puts NO features in front:
/// the core never hears the features of those chats, and nothing sends them later (a start
/// sends features only when their hash changed). Such a chat has the core's default
/// features, the most restrictive set.
#[tokio::test(flavor = "current_thread")]
async fn review_a_chats_row_that_is_taken_back_tells_the_core_its_features_again() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage_of_people(dir.path(), &[3, 4]);
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;

    // The people, the step into the chats, the row of chats.
    let mut chats_seq = None;
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.payload, Payload::Messages { .. }) {
            break;
        }
        let is_chats = matches!(chunk.payload, Payload::Chats(_));
        let done = append(&shared, chunk, NOW).unwrap();
        if is_chats {
            chats_seq = done.seq;
        }
    }
    let chats_seq = chats_seq.expect("the row of chats");
    let rows = outbox(&shared.db);
    let (_, events) = rows.iter().find(|(seq, _)| *seq == chats_seq).unwrap();
    assert_eq!(
        with_features(events),
        2,
        "the row of chats carries the features of both"
    );

    // The core refuses that row for good; the delivery finds it in the poison table.
    shared
        .db
        .quarantine(chats_seq, "refused by a test")
        .unwrap();
    assert!(settle(&shared.db, MAX_REFUSED_RUN).unwrap().new_pieces);
    for chunk in fed_from(&shared.db, dir.path(), recorded) {
        if matches!(chunk.place, Place::Redo { .. }) {
            append(&shared, chunk, NOW).unwrap();
        }
    }
    let again = outbox(&shared.db);
    let singles: Vec<&Vec<BackendEvent>> = again
        .iter()
        .filter(|(seq, _)| *seq > chats_seq)
        .map(|(_, events)| events)
        .collect();
    assert_eq!(singles.len(), 2, "both chats came back, one per row");
    assert_eq!(
        singles
            .iter()
            .map(|events| with_features(events))
            .sum::<usize>(),
        2,
        "the chats came back without the features the refused row carried"
    );
}

/// DEFECT (review finding 5, small). When the last row of a chat is not delivered (here:
/// due under a clock the server confirmed; the same when a single is dropped), no row says
/// `has_more: false` for that chat: every batch the core heard said that more follows.
#[tokio::test(flavor = "current_thread")]
async fn review_a_chat_whose_last_row_is_dropped_still_hears_that_nothing_follows() {
    let dir = tempfile::tempdir().unwrap();
    // One chat of three rows; the last one is due.
    let recorded = {
        let mut writer = StageWriter::create(dir.path(), &key(), StageLimits::default()).unwrap();
        let contact = StagedContact {
            user: peer(0),
            aci: Some(aci(0)),
            pni: None,
            e164: None,
            profile_key: None,
            profile_sharing: true,
            blocked: false,
            hidden: false,
            unregistered_at: None,
            profile_name: None,
            system_name: None,
            nickname: None,
        };
        writer.contact(0, &contact).unwrap();
        let chat = StagedChat::direct(0);
        writer
            .chat(0, &ConvKey::Direct(peer(0)), &chat, None)
            .unwrap();
        for n in 1..=3u64 {
            let mut msg = message(0, n, 10);
            if n == 3 {
                msg.read = true;
                msg.expiry = Some(kourier_model::Expiry {
                    kind: kourier_model::DisappearKind::AfterRead,
                    timer: Duration::from_secs(1),
                    started_at: Some(1_000),
                });
            }
            writer.item(0, n, &msg).unwrap();
        }
        let counters = [ChatCounters {
            idx: 0,
            n_rows: 3,
            n_loud: 3,
            last_at: 1_003,
        }];
        let totals = writer.commit(&counters, 10).unwrap();
        Recorded {
            messages: totals.messages,
            chats: totals.chats,
            frames: 10,
        }
    };
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let mut test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    test.shared.tuning.transfer_delivery.max_messages = 2;
    let shared = test.shared;
    shared.clock_confirmed.set(true);
    let start = shared.db.read(load_start).unwrap();
    let bounds = shared.tuning.transfer_delivery;
    let all = {
        let dir = dir.path().to_owned();
        std::thread::spawn(move || chunks(fed(&dir, recorded, &start, bounds)))
            .join()
            .unwrap()
    };
    for chunk in all {
        append(&shared, chunk, NOW).unwrap();
    }
    assert_eq!(
        int(&shared.db, "SELECT deliver_expired FROM history_transfer"),
        1
    );
    let said: Vec<bool> = outbox(&shared.db)
        .iter()
        .flat_map(|(_, events)| events.iter())
        .filter_map(|event| match event {
            BackendEvent::HistoryBatch { has_more, .. } => Some(*has_more),
            _ => None,
        })
        .collect();
    assert_eq!(
        said.last(),
        Some(&false),
        "what the core heard of the chat: {said:?}"
    );
}

/// DEFECT (review finding 3). The forwarder ships rows in order and the core stops at the
/// first job of an envelope that fails (`kourier-core/src/supervisor.rs`, `apply`: "later
/// jobs are skipped"), so of two waiting singles only the FIRST can be what the core is
/// stuck on; the one behind it was never applied. The bound of the running time takes back
/// every waiting row all the same (`strike(.., None)`), and a single that is taken back is
/// dropped: a good message is lost beside the bad one, and counted as refused.
#[test]
fn review_an_idle_strike_drops_only_the_single_the_core_is_stuck_on() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let stuck = in_flight(&db, true, false, 1);
    let behind = in_flight(&db, true, false, 1);
    let struck = strike(&db, None, Duration::from_secs(600)).unwrap();
    assert_eq!(struck.strikes, 1);
    assert_eq!(struck.taken, 1, "only the oldest waiting row is taken back");
    assert_eq!(
        int(
            &db,
            &format!("SELECT count(*) FROM core_outbox WHERE seq = {stuck}")
        ),
        0,
        "the row the core is stuck on is taken back"
    );
    assert_eq!(
        int(&db, "SELECT deliver_dropped FROM history_transfer"),
        1,
        "the message behind it was dropped too, untried"
    );
    assert_eq!(
        int(
            &db,
            &format!("SELECT count(*) FROM core_outbox WHERE seq = {behind}")
        ) + int(
            &db,
            "SELECT count(*) FROM history_delivery WHERE seq IS NULL"
        ),
        1,
        "the single behind it still waits for the core, or is appended again"
    );
}

// Added by the fix round for the review (`target/s2-context/fix.md`). The three tests above
// were the review's probes (ignored while the defects stood); they run as ordinary tests
// now.

/// A delivery row of `kind` in `core_outbox` with its entry; returns its seq.
fn in_flight_of_kind(db: &ZoneDb, kind: i64, single: bool, messages: i64) -> i64 {
    db.zone(|tx| -> Result<i64> {
        let seq = tx.outbox_append(b"row", NOW)?;
        tx.execute(
            "INSERT INTO history_delivery(seq, kind, chat, after_n, through_n, ends_chat,
               single, messages) VALUES (?1, ?2, 4, 10, 20, 0, ?3, ?4)",
            (seq, kind, single, messages),
        )?;
        Ok(seq)
    })
    .unwrap()
}

fn refuse(db: &ZoneDb, seq: i64) {
    db.zone(|tx| -> Result<bool> { Ok(kourier_db::outbox::quarantine(tx, seq, "refused", NOW)?) })
        .unwrap();
}

fn acknowledge(db: &ZoneDb, seq: i64) {
    db.zone(|tx| -> Result<usize> {
        Ok(tx.execute("DELETE FROM core_outbox WHERE seq = ?1", [seq])?)
    })
    .unwrap();
}

/// F2. A core that refuses what it is given is not going to take the rest: single units
/// refused one after another end the delivery at the bound. A refusal is not a piece to
/// append again (no restart of the feeder), the refused copy does not stay in the poison
/// table, an acknowledged row that carries something ends the run, and the bare end of a
/// chat does not (a core that refuses every message accepts an empty batch).
#[test]
fn single_units_refused_in_a_row_end_the_delivery_and_a_bare_end_does_not_end_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let run = |db: &ZoneDb| int(db, "SELECT deliver_refused FROM history_transfer");

    // Two refused, then a row that carries messages is acknowledged: the run is over.
    for _ in 0..2 {
        let seq = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
        refuse(&db, seq);
    }
    let good = in_flight_of_kind(&db, KIND_MESSAGES, false, 5);
    acknowledge(&db, good);
    let found = settle(&db, 3).unwrap();
    assert_eq!((found.answered, found.waiting), (3, 0));
    assert!(!found.new_pieces, "a refused single makes no piece");
    assert!(!found.pending && !found.refused_out);
    assert_eq!(run(&db), 0, "the acknowledged row ended the run");
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox_poison"), 0);
    assert_eq!(int(&db, "SELECT deliver_dropped FROM history_transfer"), 2);

    // Refused, a bare end of a chat acknowledged, refused, refused: it does not end the run.
    let a = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
    refuse(&db, a);
    let end = in_flight_of_kind(&db, KIND_END, true, 1);
    acknowledge(&db, end);
    let b = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
    refuse(&db, b);
    let found = settle(&db, 3).unwrap();
    assert!(!found.refused_out);
    assert_eq!(run(&db), 2);
    let c = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
    refuse(&db, c);
    let found = settle(&db, 3).unwrap();
    assert!(
        found.refused_out,
        "three refused singles with no good row between"
    );
    assert_eq!(run(&db), 3);
    assert_eq!(int(&db, "SELECT count(*) FROM core_outbox_poison"), 0);
    // A row of several that is refused is a piece, and does not count as a single.
    let many = in_flight_of_kind(&db, KIND_MESSAGES, false, 5);
    refuse(&db, many);
    let found = settle(&db, 100).unwrap();
    assert!(found.new_pieces && found.pending && !found.refused_out);
    assert_eq!(run(&db), 3);
}

/// F3b. The time a delivery waited for rows that were then taken back is added up, and
/// nothing resets the total: an acknowledgement in between ends the strikes in a row but
/// not the total, so a core that cannot store a message here and another there ends the
/// transfer at the bound.
#[test]
fn the_time_spent_waiting_for_rows_taken_back_adds_up_and_no_acknowledgement_resets_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let ten_minutes = Duration::from_secs(600);
    let stuck = |db: &ZoneDb| int(db, "SELECT deliver_stuck_ms FROM history_transfer");
    let mut total = Duration::ZERO;
    for round in 1..=3u32 {
        let waiting = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
        let struck = strike(&db, None, ten_minutes).unwrap();
        total += ten_minutes;
        assert_eq!((struck.strikes, struck.taken), (1, 1), "round {round}");
        assert_eq!(struck.stuck, total, "round {round}");
        assert_eq!(stuck(&db), i64::try_from(total.as_millis()).unwrap());
        // The core takes the next row: the strikes in a row end, the total does not.
        let next = in_flight_of_kind(&db, KIND_MESSAGES, false, 5);
        acknowledge(&db, next);
        settle(&db, MAX_REFUSED_RUN).unwrap();
        assert_eq!(int(&db, "SELECT deliver_strikes FROM history_transfer"), 0);
        assert_eq!(stuck(&db), i64::try_from(total.as_millis()).unwrap());
        let _ = waiting;
    }
    assert!(total >= Duration::from_secs(1800));
    // A lost stage read again goes on with the total it has; a rebuilt core starts over.
    db.zone(|tx| -> Result<usize> {
        Ok(tx.execute("UPDATE history_transfer SET deliver_began = 1", [])?)
    })
    .unwrap();
    db.zone(|tx| forget(tx, NOW)).unwrap();
    assert_eq!(stuck(&db), 1_800_000);
    assert!(db.zone(|tx| rebuild(tx, NOW)).unwrap());
    assert_eq!(stuck(&db), 0);
}

/// F3a, at a start: of the rows found unacknowledged at too many starts only the OLDEST is
/// taken back as it is (and, a single, dropped). A single behind it was never tried by the
/// core and stays. A row of several units behind it is split all the same (nothing is
/// lost): left whole, it could be shipped in one envelope with the first singles of the
/// oldest row, and cost a strike when the bad message is the first of those.
#[test]
fn at_a_start_only_the_oldest_row_over_the_bound_is_dropped_and_a_single_behind_it_stays() {
    let dir = tempfile::tempdir().unwrap();
    let db = reading_db(dir.path());
    let oldest = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
    let single = in_flight_of_kind(&db, KIND_MESSAGES, true, 1);
    let many = in_flight_of_kind(&db, KIND_MESSAGES, false, 5);
    for _ in 0..=MAX_STARTS {
        note_start(&db).unwrap();
    }
    let struck = strike(&db, Some(MAX_STARTS), Duration::ZERO).unwrap();
    assert_eq!((struck.strikes, struck.taken, struck.piece), (1, 1, true));
    let count = |seq: i64| {
        int(
            &db,
            &format!("SELECT count(*) FROM core_outbox WHERE seq = {seq}"),
        )
    };
    assert_eq!(
        (count(oldest), count(single), count(many)),
        (0, 1, 0),
        "the oldest single is dropped, the single behind it waits, the row of several is split"
    );
    assert_eq!(int(&db, "SELECT deliver_dropped FROM history_transfer"), 1);
    assert_eq!(
        int(
            &db,
            "SELECT count(*) FROM history_delivery WHERE seq IS NULL AND messages = 5"
        ),
        1
    );
    // A row behind the oldest that is not over the bound itself is left alone.
    let fresh = in_flight_of_kind(&db, KIND_MESSAGES, false, 5);
    let struck = strike(&db, Some(MAX_STARTS), Duration::ZERO).unwrap();
    assert_eq!(
        struck.taken, 1,
        "`single` is the oldest now and is over the bound"
    );
    assert_eq!((count(single), count(fresh)), (0, 1));
    // And when the oldest is not over the bound nothing is taken back.
    assert_eq!(
        strike(&db, Some(MAX_STARTS), Duration::ZERO).unwrap(),
        Struck::default()
    );
}

/// F5. The last single of a chat is dropped: the chat's end is owed on its own (a piece of
/// kind `KIND_END`), and the row that pays it says that nothing follows (an empty batch
/// with `has_more: false`) and nothing else: no unread count is sent (step S3a: the rows
/// say what is unread, and a dropped row is not counted).
#[tokio::test(flavor = "current_thread")]
async fn the_owed_end_of_a_chat_whose_last_row_was_dropped_says_nothing_follows() {
    let dir = tempfile::tempdir().unwrap();
    let recorded = stage(dir.path(), &[4, 6], 20);
    let fixture = fixture();
    reading(&fixture.db, recorded);
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    // What `take_back` leaves for the last single of chat 0 that was dropped.
    shared
        .db
        .zone(|tx| -> Result<usize> {
            Ok(tx.execute(
                "INSERT INTO history_delivery(seq, kind, chat, after_n, through_n, single)
                 VALUES (NULL, ?1, 0, 0, 0, 1)",
                [KIND_END],
            )?)
        })
        .unwrap();
    let first = fed_from(&shared.db, dir.path(), recorded).remove(0);
    assert!(matches!(first.place, Place::Redo { last: true, .. }));
    assert!(matches!(first.payload, Payload::End { .. }));
    append(&shared, first, NOW).unwrap();
    let rows = outbox(&shared.db);
    assert_eq!(rows.len(), 1);
    let [
        BackendEvent::HistoryBatch {
            messages, has_more, ..
        },
    ] = rows[0].1.as_slice()
    else {
        panic!("an empty batch and nothing else, not {:?}", rows[0].1);
    };
    assert!(messages.is_empty() && !has_more);
    assert_eq!(
        int(
            &shared.db,
            "SELECT count(*) FROM history_delivery WHERE seq IS NULL"
        ),
        0
    );
}

/// F7. A connection that is gone confirms nothing about the clock: the next statement of the
/// server does.
#[tokio::test(flavor = "current_thread")]
async fn a_connection_that_is_gone_no_longer_confirms_the_clock() {
    use kourier_model::ConnState;
    let fixture = fixture();
    let test = testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
    let shared = test.shared;
    shared.clock_confirmed.set(true);
    shared.set_conn(ConnState::Syncing { fraction: None });
    shared.set_conn(ConnState::Online { detail: None });
    assert!(
        shared.clock_confirmed.get(),
        "a connection that stands keeps it"
    );
    shared.set_conn(ConnState::Offline { retry_at_ms: None });
    assert!(!shared.clock_confirmed.get());
    shared.clock_confirmed.set(true);
    shared.set_conn(ConnState::Connecting);
    assert!(!shared.clock_confirmed.get());
}
