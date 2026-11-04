//! Retention over a real store and a real stage (W4 C18): a delivered chunk keeps no bytes, a bad chunk is kept
//! 14 days, and the books start over when the stage was replaced. Every id and number is synthetic.

use std::sync::Arc;

use kourier_db::{DbKey, Priority};
use tokio::sync::Notify;

use super::{Adopted, BAD_KEEP_MS, adopt, forget_bad, reset, retire, settle_notices};
use crate::error::sql;
use crate::history::capture::{Capture, Captured};
use crate::history::stage::{LazyStage, StageHandle};
use crate::history::testing::{Chat, Chunk, SyncType};
use crate::store::{KourierStore, StoreOwner, exec};

const KEY: [u8; 32] = [0x55; 32];
const OWNER: &str = "99900100123";
const DAY: i64 = 24 * 60 * 60 * 1000;
const NOW: i64 = 1_800_000_000_000;

struct Rig {
    dir: tempfile::TempDir,
    _owner: StoreOwner,
    store: KourierStore,
    lazy: Arc<LazyStage>,
    stage: StageHandle,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().expect("temp dir");
    let backend_db = dir.path().join("backend.db");
    let owner = StoreOwner::open(&backend_db, &DbKey::from(KEY), "wa-retention").expect("open");
    let store = owner.store();
    let lazy = Arc::new(LazyStage::new(&backend_db, &DbKey::from(KEY)));
    let stage = lazy.handle(OWNER).expect("the stage opens");
    Rig {
        dir,
        _owner: owner,
        store,
        lazy,
        stage,
    }
}

impl Rig {
    /// A chunk straight into the stage in `state`, with a plan row and `bad_at`; its seq.
    async fn chunk(&self, msg_id: &str, state: &str, captured_at: i64, bad_at: Option<i64>) -> i64 {
        let (id, state) = (msg_id.to_owned(), state.to_owned());
        self.stage
            .write_at(Priority::High, "test chunk", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, captured_at, bad_at, size, state) \
                     VALUES (?1, ?2, ?3, 3, ?4)",
                    (&id, captured_at, bad_at, &state),
                )
                .map_err(sql("test chunk"))?;
                let seq = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, x'010203')",
                    [seq],
                )
                .map_err(sql("test bytes"))?;
                tx.execute(
                    "INSERT INTO plan (chunk, n, pos, conv, ts, count) VALUES (?1, 0, 0, 'c', 1, 1)",
                    [seq],
                )
                .map_err(sql("test plan"))?;
                Ok(seq)
            })
            .await
            .expect("a chunk")
    }

    /// The stage's row of a chunk: (state, has bytes, plan rows).
    async fn look(&self, seq: i64) -> Option<(String, bool, i64)> {
        let reader = self.stage.reader().expect("a reader");
        reader
            .query_row(
                "SELECT state, EXISTS (SELECT 1 FROM chunk_bytes WHERE seq = ?1), \
                 (SELECT count(*) FROM plan WHERE chunk = ?1) \
                 FROM chunks WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .ok()
    }

    /// The core's books: a delivered chunk, and whether a delivery row of it is still unsettled.
    async fn books(&self, sql_text: &'static str) {
        self.store
            .write("test books", move |tx| {
                tx.execute_batch(sql_text).map_err(sql("test books"))
            })
            .await
            .expect("books");
    }

    async fn number(&self, sql_text: &'static str) -> i64 {
        self.store
            .read(move |conn| {
                conn.query_row(sql_text, [], |r| r.get(0))
                    .map_err(sql("count"))
            })
            .await
            .expect("count")
    }
}

#[tokio::test]
async fn a_done_chunk_keeps_no_bytes() {
    let rig = rig();
    let first = rig.chunk("n1", "planned", 1, None).await;
    let second = rig.chunk("n2", "planned", 1, None).await;
    let third = rig.chunk("n3", "captured", 1, None).await;
    // Chunks 1 and 2 are delivered; the core has not answered the last row of chunk 2.
    rig.books("INSERT INTO history_chunks(chunk) VALUES (1), (2);")
        .await;
    rig.books("INSERT INTO history_delivery(seq, chunk, kind) VALUES (5, 2, 3);")
        .await;

    assert_eq!(retire(&rig.store, &rig.stage).await.expect("retire"), 1);
    assert_eq!(
        rig.look(first).await,
        Some(("done".to_owned(), false, 0)),
        "a delivered chunk keeps no bytes and no plan"
    );
    assert_eq!(
        rig.look(second).await,
        Some(("planned".to_owned(), true, 1)),
        "a chunk with a row the core has not answered keeps its bytes: that row may be read again"
    );
    assert_eq!(
        rig.look(third).await,
        Some(("captured".to_owned(), true, 1))
    );
    assert_eq!(
        rig.number("SELECT retired FROM history_chunks WHERE chunk = 1")
            .await,
        1
    );
    assert_eq!(
        rig.number("SELECT retired FROM history_chunks WHERE chunk = 2")
            .await,
        0
    );
    // The stage file stays (the next push is appended to it) and the chunk's row stays, so a second notice of it is
    // still one chunk.
    assert!(rig.dir.path().join("history").join("stage.db").exists());
    let capture = Capture::new(rig.lazy.clone(), Arc::new(Notify::new()));
    let again = Chunk::new(SyncType::INITIAL_BOOTSTRAP, 0, 100)
        .chat(Chat::new("15550100101@s.whatsapp.net").texts("A", 1_700_000_100, 1))
        .blob();
    assert_eq!(
        capture
            .commit(OWNER, "n1", Some(0), again)
            .await
            .expect("capture"),
        Captured::Duplicate
    );

    // Nothing changes while nothing changes.
    assert_eq!(retire(&rig.store, &rig.stage).await.expect("retire"), 0);

    // The core answers chunk 2's last row: it is retired at the next look.
    rig.books("DELETE FROM history_delivery;").await;
    assert_eq!(retire(&rig.store, &rig.stage).await.expect("retire"), 1);
    assert_eq!(rig.look(second).await, Some(("done".to_owned(), false, 0)));
    assert_eq!(
        rig.number("SELECT count(*) FROM history_chunks WHERE retired = 1")
            .await,
        2
    );
}

#[tokio::test]
async fn a_retired_chunk_is_redone_harmlessly_after_a_kill_between_the_stage_and_the_books() {
    let rig = rig();
    let seq = rig.chunk("n1", "done", 1, None).await;
    // The stage job ran, the mark in backend.db did not: the chunk is `done` and `retired` is still 0.
    rig.stage
        .write_at(Priority::High, "test done", None, move |tx| {
            exec(tx, "test done", "DELETE FROM chunk_bytes", [])?;
            exec(tx, "test done", "DELETE FROM plan", []).map(drop)
        })
        .await
        .expect("done");
    rig.books("INSERT INTO history_chunks(chunk) VALUES (1);")
        .await;
    assert_eq!(retire(&rig.store, &rig.stage).await.expect("retire"), 1);
    assert_eq!(rig.look(seq).await, Some(("done".to_owned(), false, 0)));
    assert_eq!(
        rig.number("SELECT retired FROM history_chunks WHERE chunk = 1")
            .await,
        1
    );
}

#[tokio::test]
async fn a_bad_chunk_is_kept_14_days() {
    let rig = rig();
    let long_ago = NOW - 100 * DAY;
    let fresh = rig.chunk("b1", "bad", long_ago, Some(NOW - 13 * DAY)).await;
    let old = rig.chunk("b2", "bad", long_ago, Some(NOW - 15 * DAY)).await;
    // A bad chunk with no time of its own is judged by the day it was captured.
    let undated = rig.chunk("b3", "bad", NOW - 15 * DAY, None).await;
    // Neither a chunk that waits nor one that is done is ever forgotten, however old.
    let waiting = rig.chunk("p1", "planned", long_ago, None).await;
    let done = rig.chunk("d1", "done", long_ago, None).await;
    assert_eq!(BAD_KEEP_MS, 14 * DAY);
    // Every chunk was announced; the notice of a forgotten chunk is no lost chunk, the rest stay as they were.
    rig.books(
        "INSERT INTO history_announced(msg_id, announced_at) VALUES ('b1', 1), ('b2', 1), ('b3', 1), ('p1', 1), ('d1', 1);",
    )
    .await;

    let mut forgotten = forget_bad(&rig.stage, NOW).await.expect("forget");
    forgotten.sort();
    assert_eq!(forgotten, ["b2", "b3"]);
    settle_notices(&rig.store, forgotten).await.expect("settle");
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced WHERE msg_id IN ('b1', 'p1', 'd1')")
            .await,
        3
    );
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced WHERE msg_id IN ('b2', 'b3')")
            .await,
        0,
        "the notice of a forgotten chunk is not held against the person as a chunk that never came"
    );
    assert_eq!(rig.look(fresh).await, Some(("bad".to_owned(), true, 1)));
    assert_eq!(rig.look(old).await, None, "after 14 days it is forgotten");
    assert_eq!(rig.look(undated).await, None);
    assert_eq!(
        rig.look(waiting).await,
        Some(("planned".to_owned(), true, 1))
    );
    assert_eq!(rig.look(done).await, Some(("done".to_owned(), true, 1)));
    let plans = rig.stage.reader().expect("reader");
    let left: i64 = plans
        .query_row("SELECT count(*) FROM plan", [], |r| r.get(0))
        .expect("plans");
    assert_eq!(left, 3, "the plans of the forgotten chunks went with them");

    // A day later the fresh one has aged out too.
    assert_eq!(
        forget_bad(&rig.stage, NOW + 2 * DAY).await.expect("forget"),
        ["b1"]
    );
    assert_eq!(rig.look(fresh).await, None);
}

#[tokio::test]
async fn the_books_start_over_when_the_stage_was_replaced() {
    let rig = rig();
    rig.books(
        "UPDATE history_cursor SET chunk = 7, phase = 2, conv = 3, msg = 9, idle_ms = 500 WHERE id = 1; \
         INSERT INTO history_delivery(seq, chunk, kind) VALUES (11, 7, 2); \
         INSERT INTO history_delivery(seq, chunk, kind) VALUES (NULL, 7, 2); \
         INSERT INTO history_chunks(chunk) VALUES (5), (6); \
         INSERT INTO history_chats(conv, more, hint_stated, read_here) VALUES ('a', 1, 1, 1), ('b', 0, 1, 0); \
         INSERT INTO history_announced(msg_id, announced_at, lost) VALUES ('n1', 1, 0), ('n2', 1, 1); \
         UPDATE history_counts SET delivered = 40, lost = 1, lost_told = 1 WHERE id = 1; \
         INSERT INTO core_outbox(envelope, created_at) VALUES (x'00', 1); \
         INSERT INTO history_deferred(chunk_msg_id, n, conv, item, map_version) VALUES ('n1', 0, 'a', x'00', 1);",
    )
    .await;

    reset(&rig.store, 3).await.expect("reset");

    assert_eq!(
        rig.number("SELECT chunk + phase + conv + msg + idle_ms FROM history_cursor")
            .await,
        0,
        "the cursor is back at no chunk yet"
    );
    assert_eq!(rig.number("SELECT count(*) FROM history_delivery").await, 0);
    assert_eq!(rig.number("SELECT count(*) FROM history_chunks").await, 0);
    assert_eq!(
        rig.number("SELECT count(*) FROM history_chats WHERE more = 1")
            .await,
        0,
        "no chat waits for an end the replaced stage would have given"
    );
    assert_eq!(
        rig.number(
            "SELECT count(*) FROM history_chats WHERE hint_stated = 1 AND conv IN ('a', 'b')"
        )
        .await,
        2,
        "what the person was told stays told"
    );
    assert_eq!(
        rig.number("SELECT read_here FROM history_chats WHERE conv = 'a'")
            .await,
        1
    );
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced").await,
        1,
        "a notice not yet counted is forgotten, one already counted stays counted"
    );
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 4);
    assert_eq!(rig.number("SELECT delivered FROM history_counts").await, 40);
    assert_eq!(rig.number("SELECT lost_told FROM history_counts").await, 1);
    assert_eq!(
        rig.number("SELECT count(*) FROM core_outbox").await,
        1,
        "what was already appended is real content and stays"
    );
    assert_eq!(rig.number("SELECT count(*) FROM history_deferred").await, 1);

    // Idempotent but for the count: the books are as they were after the first reset.
    reset(&rig.store, 0).await.expect("reset again");
    assert_eq!(rig.number("SELECT lost FROM history_counts").await, 4);
}

const INCARNATION: [u8; 16] = [7; 16];

/// HIGH-1: the first adoption of a stage over books that point into nothing records the id and changes nothing
/// else (the notices announced before the first capture stay); the same id again changes nothing.
/// Mutant for the re-check: `books_are_empty` always false (the first push would forget its notices).
#[tokio::test]
async fn the_first_stage_is_adopted_without_starting_the_books_over() {
    let rig = rig();
    rig.books("INSERT INTO history_announced(msg_id, announced_at) VALUES ('n1', 1);")
        .await;
    assert_eq!(
        adopt(&rig.store, INCARNATION, None).await.expect("adopt"),
        Adopted::First
    );
    assert_eq!(
        rig.number("SELECT count(*) FROM history_announced WHERE lost = 0")
            .await,
        1,
        "the notice announced before the first capture is kept"
    );
    assert_eq!(
        adopt(&rig.store, INCARNATION, None).await.expect("again"),
        Adopted::Same
    );
}

/// HIGH-1: books that hold delivery state and no recorded stage (a stage made over non-empty books), and books that
/// point into another incarnation, start over in the zone that records the new id, whoever opened the file.
/// Mutant for the re-check: the id check removed from `adopt` (every call answers `Same`).
#[tokio::test]
async fn books_that_point_elsewhere_start_over_in_the_zone_that_writes_the_new_id() {
    let rig = rig();
    rig.books(
        "UPDATE history_cursor SET chunk = 5, phase = 2, conv = 1, msg = 4 WHERE id = 1; \
         INSERT INTO history_chunks(chunk) VALUES (1), (5); \
         INSERT INTO history_announced(msg_id, announced_at, lost) VALUES ('a', 1, 0), ('b', 1, 0), ('c', 1, 0);",
    )
    .await;
    // No stage was recorded and the cursor stands inside chunk 5: the books describe a file that is not this one.
    assert_eq!(
        adopt(&rig.store, INCARNATION, None).await.expect("adopt"),
        Adopted::Reset
    );
    assert_eq!(rig.number("SELECT chunk FROM history_cursor").await, 0);
    assert_eq!(rig.number("SELECT count(*) FROM history_chunks").await, 0);
    assert_eq!(
        rig.number("SELECT lost FROM history_counts").await,
        1,
        "three announced, two delivered: the books' estimate of what the old stage lost"
    );
    // The id is recorded: a second open of this file changes nothing, another id starts over again.
    assert_eq!(
        adopt(&rig.store, INCARNATION, None).await.expect("same"),
        Adopted::Same
    );
    rig.books("UPDATE history_cursor SET chunk = 2 WHERE id = 1;")
        .await;
    assert_eq!(
        adopt(&rig.store, [8; 16], Some(4)).await.expect("other"),
        Adopted::Reset
    );
    assert_eq!(rig.number("SELECT chunk FROM history_cursor").await, 0);
    assert_eq!(
        rig.number("SELECT lost FROM history_counts").await,
        5,
        "a known loss is used as it is"
    );
}
