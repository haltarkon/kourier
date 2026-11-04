//! The first pass over a chunk: the plan in the order of delivery, the same plan after a kill between the
//! passes, a chunk that does not read set aside, a chat that does not decode skipped and counted.

use kourier_db::{DbKey, Priority};

use wacore::history_sync::MAX_DECOMPRESSED;

use super::{
    Bad, Entry, Planned, Report, commit_plan, entries, plan, plan_pending, plan_with, scan,
};
use crate::error::sql;
use crate::history::stage::{MIGRATIONS, StageHandle, StageOwner, derive_key};
use crate::history::testing::{Chat, Chunk, Msg, SyncType, zlib};

const OWNER: &str = "the account";

struct Rig {
    // Held for its files.
    dir: tempfile::TempDir,
    stage: StageOwner,
}

fn open_in(dir: &std::path::Path) -> StageOwner {
    let key = derive_key(&DbKey::from([0x22; 32]));
    StageOwner::open(
        &dir.join("history").join("stage.db"),
        &key,
        OWNER,
        &MIGRATIONS,
    )
    .expect("open the stage")
    .0
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("dir");
    let stage = open_in(dir.path());
    Rig { dir, stage }
}

impl Rig {
    fn handle(&self) -> StageHandle {
        self.stage.handle()
    }

    /// Captures `bytes` as the capture would (no sync type: the plan takes the blob's own).
    fn put(&self, id: &str, bytes: Vec<u8>) -> i64 {
        let id = id.to_owned();
        self.handle()
            .write_blocking(Priority::High, "test capture", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, captured_at, size, state) \
                     VALUES (?1, 1, ?2, 'captured')",
                    (id, bytes.len() as i64),
                )
                .map_err(sql("test capture"))?;
                let seq = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, ?2)",
                    (seq, bytes),
                )
                .map_err(sql("test capture"))?;
                Ok(seq)
            })
            .expect("capture")
    }

    /// (state, sync type, chunk order, progress, skipped, bytes kept) of chunk `seq`.
    #[allow(clippy::type_complexity)]
    fn row(&self, seq: i64) -> (String, Option<i32>, Option<u32>, Option<u32>, u32, bool) {
        self.handle()
            .reader()
            .expect("reader")
            .query_row(
                "SELECT state, sync_type, chunk_order, progress, skipped, \
                 EXISTS (SELECT 1 FROM chunk_bytes b WHERE b.seq = chunks.seq) \
                 FROM chunks WHERE seq = ?1",
                [seq],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .expect("row")
    }

    fn plan_of(&self, seq: i64) -> Vec<Entry> {
        entries(&self.handle().reader().expect("reader"), seq).expect("plan")
    }
}

const GROUP: &str = "120363000000000001@g.us";
const P1: &str = "15550100101@s.whatsapp.net";
const P2: &str = "15550100102@s.whatsapp.net";
const P3: &str = "15550100103@s.whatsapp.net";

/// Five chats in the phone's order. Equal times (the group and P2) are NOT in id order in the stream,
/// and P3 states no time of its own (its newest message's is used).
fn bootstrap() -> Chunk {
    Chunk::new(SyncType::INITIAL_BOOTSTRAP, 2, 40)
        .chat(Chat::new(P1).active_at(100).texts("a", 90, 3))
        .chat(Chat::new(P2).active_at(300))
        .chat(
            Chat::new(GROUP)
                .name("A group")
                .active_at(300)
                .texts("g", 299, 1),
        )
        .chat(
            Chat::new(P3)
                .message(Msg::text(P3, "x1", 150, "older"))
                .message(Msg::text(P3, "x2", 200, "newest")),
        )
        .chat(Chat::new("status@broadcast").active_at(150))
}

fn entry(n: u32, pos: u32, conv: &str, ts: i64, count: u32) -> Entry {
    Entry {
        n,
        pos,
        conv: conv.to_owned(),
        ts,
        count,
    }
}

#[test]
fn a_chunk_is_planned_most_recent_chat_first_with_equal_times_by_id() {
    let rig = rig();
    let seq = rig.put("N1", bootstrap().blob());
    assert_eq!(
        plan(&rig.handle(), seq).expect("plan"),
        Planned::Read {
            chats: 5,
            skipped: 0
        }
    );
    assert_eq!(
        rig.plan_of(seq),
        [
            // 300 s: the group before P2 by id although P2 comes first in the stream.
            entry(0, 2, GROUP, 300, 1),
            entry(1, 1, P2, 300, 0),
            entry(2, 3, P3, 200, 2),
            entry(3, 4, "status@broadcast", 150, 0),
            entry(4, 0, P1, 100, 3),
        ]
    );
    let (state, sync, order, progress, skipped, kept) = rig.row(seq);
    assert_eq!(state, "planned");
    assert_eq!(
        (sync, order, progress, skipped),
        (
            Some(SyncType::INITIAL_BOOTSTRAP as i32),
            Some(2),
            Some(40),
            0
        ),
        "the type, the order and the progress come from the blob's own remainder"
    );
    assert!(kept, "the bytes stay until every row settled");
}

#[test]
fn a_chunk_without_chats_still_gets_its_type_order_and_progress() {
    let rig = rig();
    let blob = Chunk::new(SyncType::PUSH_NAME, 1, 100)
        .push_name(P1, "A Name")
        .blob();
    let seq = rig.put("N1", blob);
    assert_eq!(
        plan(&rig.handle(), seq).expect("plan"),
        Planned::Read {
            chats: 0,
            skipped: 0
        }
    );
    assert!(rig.plan_of(seq).is_empty());
    let (state, sync, order, progress, ..) = rig.row(seq);
    assert_eq!(state, "planned");
    assert_eq!(
        (sync, order, progress),
        (Some(SyncType::PUSH_NAME as i32), Some(1), Some(100))
    );
}

#[test]
fn the_plan_is_the_same_after_a_kill_between_passes() {
    let rig = rig();
    let seq = rig.put("N1", bootstrap().blob());
    assert_eq!(
        plan(&rig.handle(), seq).expect("plan"),
        Planned::Read {
            chats: 5,
            skipped: 0
        }
    );
    let first = rig.plan_of(seq);
    assert_eq!(first.len(), 5);

    // The process dies after the plan committed and before the second pass: the next start finds the
    // plan, and planning the chunk again changes nothing.
    let Rig { dir, stage } = rig;
    stage.close();
    let rig = Rig {
        stage: open_in(dir.path()),
        dir,
    };
    assert_eq!(rig.row(seq).0, "planned");
    assert_eq!(rig.plan_of(seq), first, "the committed plan survives");
    assert_eq!(plan(&rig.handle(), seq).expect("again"), Planned::Nothing);
    assert_eq!(rig.plan_of(seq), first, "a second planning writes nothing");

    // The process dies before the plan committed: the chunk is `captured` and has no plan; the plan
    // made now is the one the first pass would have made.
    rig.handle()
        .write_blocking(Priority::High, "test undo", None, |tx| {
            tx.execute("DELETE FROM plan", []).map_err(sql("undo"))?;
            tx.execute(
                "UPDATE chunks SET state = 'captured', chunk_order = NULL, progress = NULL, \
                 sync_type = NULL, skipped = 0",
                [],
            )
            .map_err(sql("undo"))?;
            Ok(())
        })
        .expect("undo");
    assert!(rig.plan_of(seq).is_empty());
    assert_eq!(
        plan(&rig.handle(), seq).expect("replan"),
        Planned::Read {
            chats: 5,
            skipped: 0
        }
    );
    assert_eq!(rig.plan_of(seq), first, "the same bytes give the same plan");
}

#[test]
fn a_damaged_blob_is_bad_and_delivers_nothing() {
    let rig = rig();
    let good = bootstrap();
    let proto = good.proto();
    // The kind a cut-short stream is named by is upstream's to choose (the inflate or the protobuf
    // reader meets the cut first): any `Bad` will do for it.
    let cases: [(&str, Vec<u8>, Option<Bad>); 3] = [
        (
            "not a zlib stream",
            b"this is not a zlib stream".to_vec(),
            Some(Bad::Inflate),
        ),
        (
            "a stream cut short",
            good.blob()[..good.blob().len() / 2].to_vec(),
            None,
        ),
        (
            "a protobuf cut short",
            zlib(&proto[..proto.len() - 3]),
            Some(Bad::Decode),
        ),
    ];
    for (n, (what, bytes, kind)) in cases.into_iter().enumerate() {
        let seq = rig.put(&format!("BAD{n}"), bytes);
        let planned = plan(&rig.handle(), seq).expect("plan");
        match kind {
            Some(kind) => assert_eq!(planned, Planned::Bad(kind), "{what}"),
            None => assert!(matches!(planned, Planned::Bad(_)), "{what}: {planned:?}"),
        }
        let (state, .., kept) = rig.row(seq);
        assert_eq!(state, "bad", "{what}");
        assert!(kept, "{what}: kept aside for a later build");
        assert!(
            rig.plan_of(seq).is_empty(),
            "{what}: nothing is planned, so nothing is delivered"
        );
        assert_eq!(
            plan(&rig.handle(), seq).expect("again"),
            Planned::Nothing,
            "{what}: a bad chunk is not read again"
        );
    }

    // A chunk that inflates past the bound is bad too, and so is one that holds no bytes.
    let seq = rig.put("BIG", good.blob());
    assert_eq!(
        plan_with(&rig.handle(), seq, 64).expect("plan"),
        Planned::Bad(Bad::Inflate)
    );
    assert_eq!(rig.row(seq).0, "bad");
    assert_eq!(scan(&good.blob(), 64), Err(Bad::Inflate));
    let seq = rig.put("EMPTY", Vec::new());
    rig.handle()
        .write_blocking(Priority::High, "test null", None, move |tx| {
            tx.execute("DELETE FROM chunk_bytes WHERE seq = ?1", [seq])
                .map_err(sql("null"))
                .map(drop)
        })
        .expect("null");
    assert_eq!(
        plan(&rig.handle(), seq).expect("plan"),
        Planned::Bad(Bad::Missing)
    );
}

#[test]
fn a_chat_that_does_not_decode_is_skipped_and_counted_the_others_are_planned() {
    let rig = rig();
    let a =
        Chunk::new(SyncType::RECENT, 1, 60).chat(Chat::new(P1).active_at(100).texts("a", 90, 2));
    let b = Chunk::new(SyncType::RECENT, 1, 60).chat(Chat::new(P2).active_at(200));
    // A chat entry (field 2, two bytes) whose `id` claims five bytes and has none.
    let broken = [0x12, 0x02, 0x0a, 0x05];
    let mut proto = a.proto();
    proto.extend_from_slice(&broken);
    proto.extend_from_slice(&b.proto());
    let seq = rig.put("N1", zlib(&proto));
    assert_eq!(
        plan(&rig.handle(), seq).expect("plan"),
        Planned::Read {
            chats: 2,
            skipped: 1
        }
    );
    assert_eq!(
        rig.plan_of(seq),
        [entry(0, 2, P2, 200, 0), entry(1, 0, P1, 100, 2)],
        "the position counts the entry that did not decode"
    );
    let (state, .., skipped, _) = rig.row(seq);
    assert_eq!((state.as_str(), skipped), ("planned", 1));
}

#[test]
fn every_captured_chunk_is_planned_once_and_the_bad_ones_are_counted() {
    let rig = rig();
    let one = rig.put("N1", bootstrap().blob());
    let broken = rig.put("N2", b"nothing".to_vec());
    let two = rig.put(
        "N3",
        Chunk::new(SyncType::RECENT, 3, 100)
            .chat(Chat::new(P1).active_at(5))
            .blob(),
    );
    assert_eq!(
        plan_pending(&rig.handle()).expect("plan"),
        Report { planned: 2, bad: 1 }
    );
    assert_eq!(rig.row(one).0, "planned");
    assert_eq!(rig.row(broken).0, "bad");
    assert_eq!(rig.row(two).0, "planned");
    assert_eq!(
        plan_pending(&rig.handle()).expect("again"),
        Report::default(),
        "nothing is captured any more"
    );
}

#[test]
fn a_planner_that_lost_the_race_writes_nothing() {
    let rig = rig();
    let blob = bootstrap().blob();
    let seq = rig.put("N1", blob.clone());
    assert_eq!(
        plan(&rig.handle(), seq).expect("plan"),
        Planned::Read {
            chats: 5,
            skipped: 0
        }
    );
    let first = rig.plan_of(seq);
    // A second pass that read the chunk before the first one committed: its plan is not written.
    let late = scan(&blob, MAX_DECOMPRESSED).expect("scan");
    assert_eq!(
        commit_plan(&rig.handle(), seq, late).expect("late"),
        Planned::Nothing
    );
    assert_eq!(rig.plan_of(seq), first, "the first plan stands, once");
}
