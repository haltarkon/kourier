//! The feeder's worker thread (W4 fix MEDIUM-2): the chunk it has loaded is let go of when its last row was read, when
//! it stops reading and when no job asks for it for a while, so a quiet delivery holds no inflated chunk. Every id and
//! number is synthetic.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use kourier_backend_api::FakeClock;
use kourier_db::{DbKey, Priority};
use kourier_model::UserId;

use super::{Read, Worker, read_staged};
use crate::error::sql;
use crate::history::capture::{Capture, Captured};
use crate::history::feed;
use crate::history::progress::State;
use crate::history::rows::{Bounds, Cursor, Next};
use crate::history::stage::{self, LazyStage, MIGRATIONS, StageHandle, StageOwner, derive_key};
use crate::history::testing::{Chat, Chunk, Msg, SyncType};
use crate::map::{Ctx, OwnIds};

const P1: &str = "15550100101@s.whatsapp.net";
const NOW: u64 = 1_700_000_500_000;

struct World {
    _dir: tempfile::TempDir,
    stage_owner: StageOwner,
}

fn world() -> World {
    let dir = tempfile::tempdir().expect("temp dir");
    let stage_owner = StageOwner::open(
        &dir.path().join("history").join("stage.db"),
        &derive_key(&DbKey::from([0x22; 32])),
        "the account",
        &MIGRATIONS,
    )
    .expect("open the stage")
    .0;
    World {
        _dir: dir,
        stage_owner,
    }
}

fn ctx() -> Ctx {
    Ctx {
        own: OwnIds {
            lid: Some(UserId::from_parts("lid", "99900100999").expect("lid")),
            pn: Some(UserId::from_parts("pn", "15550100999").expect("pn")),
        },
    }
}

impl World {
    fn stage(&self) -> StageHandle {
        self.stage_owner.handle()
    }

    /// A planned chunk of one chat with `count` messages; its number.
    async fn planned(&self, count: usize) -> i64 {
        let chunk = Chunk::new(SyncType::RECENT, 1, 40).chat((0..count).fold(
            Chat::new(P1).active_at(1_700_000_400),
            |chat, n| {
                chat.message(Msg::text(
                    P1,
                    &format!("A{n}"),
                    1_700_000_100 + n as u64,
                    "t",
                ))
            },
        ));
        let (bytes, sync) = (chunk.blob(), chunk.sync.sync_type as i32);
        let seq = self
            .stage()
            .write_at(Priority::High, "test capture", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, sync_type, captured_at, size, state) \
                     VALUES ('n1', ?1, 1, ?2, 'captured')",
                    (sync, bytes.len() as i64),
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
            .await
            .expect("capture");
        let stage = self.stage();
        let report = tokio::task::spawn_blocking(move || feed::plan_pending(&stage))
            .await
            .expect("joins")
            .expect("plans");
        assert_eq!(report.bad, 0);
        seq
    }

    fn worker(&self, idle: Duration) -> (Worker, Arc<AtomicUsize>) {
        let released = Arc::new(AtomicUsize::new(0));
        let worker = Worker::start_with(
            self.stage(),
            ctx(),
            Arc::new(FakeClock::new(NOW)),
            idle,
            released.clone(),
        );
        (worker, released)
    }
}

/// Waits (real time) until the worker has let go of `n` chunks.
async fn until_released(released: &AtomicUsize, n: usize) {
    for _ in 0..400 {
        if released.load(Ordering::SeqCst) >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the worker let go of {} chunks, wanted {n}",
        released.load(Ordering::SeqCst)
    );
}

/// MEDIUM-2: the worker lets go of the chunk at the end of its rows, and not before.
/// Mutant for the re-check: the `finished` break removed (the chunk stays loaded after `Next::End`).
#[tokio::test]
async fn the_chunk_is_let_go_of_when_its_last_row_was_read() {
    let w = world();
    let seq = w.planned(3).await;
    let (worker, released) = w.worker(Duration::from_secs(60));
    let mut at = Cursor::start(seq);
    let mut rows = 0;
    loop {
        match worker.read(seq, at, Bounds::DEFAULT).await.expect("read") {
            Read::Row {
                found: Next::Row(row),
                ..
            } => {
                rows += 1;
                at = row.after;
                assert_eq!(
                    released.load(Ordering::SeqCst),
                    0,
                    "a chunk that still has rows to give stays loaded"
                );
            }
            Read::Row {
                found: Next::End, ..
            } => break,
            other => panic!("unexpected {:?}", std::mem::discriminant(&other)),
        }
    }
    assert!(rows >= 2, "chats and messages rows: {rows}");
    until_released(&released, 1).await;
    drop(worker);
}

/// MEDIUM-2: a chunk that stops reading is let go of at once.
/// Mutant for the re-check: `Read::Unreadable` taken out of `finished`.
#[tokio::test]
async fn a_chunk_that_stops_reading_is_let_go_of() {
    let w = world();
    let seq = w.planned(2).await;
    // The bytes are damaged after the planning pass read them.
    w.stage()
        .write_at(Priority::High, "damage", None, move |tx| {
            tx.execute(
                "UPDATE chunk_bytes SET bytes = x'0102030405060708' WHERE seq = ?1",
                [seq],
            )
            .map(drop)
            .map_err(sql("damage"))
        })
        .await
        .expect("damage");
    let (worker, released) = w.worker(Duration::from_secs(60));
    let read = worker
        .read(seq, Cursor::start(seq), Bounds::DEFAULT)
        .await
        .expect("read");
    assert!(matches!(read, Read::Unreadable(_)));
    until_released(&released, 1).await;
    drop(worker);
}

/// MEDIUM-2: a chunk nobody asks for any more is let go of after the idle time, and asked for again it is loaded again.
/// Mutant for the re-check: `recv_timeout(idle)` back to a blocking `recv`.
#[tokio::test]
async fn a_loaded_chunk_nobody_asks_for_is_let_go_of_after_the_idle_time() {
    let w = world();
    let seq = w.planned(3).await;
    let (worker, released) = w.worker(Duration::from_millis(150));
    let first = worker
        .read(seq, Cursor::start(seq), Bounds::DEFAULT)
        .await
        .expect("read");
    let Read::Row {
        found: Next::Row(row),
        ..
    } = first
    else {
        panic!("a row was expected")
    };
    assert_eq!(
        released.load(Ordering::SeqCst),
        0,
        "just used: still loaded"
    );
    until_released(&released, 1).await;
    // Asked again, it is read afresh and gives the next row all the same.
    let second = worker
        .read(seq, row.after, Bounds::DEFAULT)
        .await
        .expect("read");
    assert!(matches!(second, Read::Row { .. }));
    until_released(&released, 2).await;
    drop(worker);
}

/// The bytes this thread has been given by `read(2)` and its kin so far (`rchar` of `/proc/thread-self/io`). A
/// reader connection of the stage is made by the function that uses it and dropped at its end, on the calling
/// thread, with a cold page cache: so what the thread read while a function ran is the pages that function pulled
/// from the file (the WAL included), one page being 4,096 bytes.
fn thread_read_bytes() -> u64 {
    std::fs::read_to_string("/proc/thread-self/io")
        .expect("this thread's I/O counters (/proc/thread-self/io: Linux with task I/O accounting)")
        .lines()
        .find_map(|line| line.strip_prefix("rchar: "))
        .and_then(|bytes| bytes.trim().parse().ok())
        .expect("an `rchar` line")
}

/// Runs `f` on this thread and says how many bytes it read.
fn measured<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = thread_read_bytes();
    let out = f();
    (out, thread_read_bytes().saturating_sub(before))
}

/// A chunk of about `kib` KiB once zlib has seen it: one chat whose one message carries that much pseudo-random
/// noise in a field the plan never reads (`messageSecret`), which deflate cannot shrink.
fn large_chunk(n: u32, kib: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9_7F4A_7C15_u64 ^ u64::from(n);
    let noise: Vec<u8> = (0..kib * 1024)
        .map(|_| {
            // xorshift64*
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect();
    Chunk::new(SyncType::RECENT, n + 1, 25 * (n + 1))
        .chat(Chat::new(P1).active_at(1_700_000_400).message(
            Msg::text(P1, &format!("L{n}"), 1_700_000_100 + u64::from(n), "t").secret(&noise),
        ))
        .blob()
}

/// W4 re-check MEDIUM-1: the decision asks that the looks at the stage read a few pages and that a state change
/// does not grow the WAL by the blob, on the PRODUCTION functions (the layout test beside `stage::tests` runs raw
/// SQL of its own). Four chunks of about 512 KiB go in through `Capture::commit`; then `plan_pending` (whose
/// `commit_plan` moves them captured -> planned), `read_staged` and a second `plan_pending` run, each measured:
/// the planning reads each blob once (that is its job) and its commits write the plan rows only, the two looks read
/// a few pages and write nothing. A control walks the blobs on purpose, so the meter is known to see a walk.
/// Mutants for the re-check: `UPDATE chunk_bytes SET bytes = bytes` after the state UPDATE of `commit_plan`
/// (the WAL grows by the blobs), and `read_staged` joining `chunk_bytes` and reading `hex(bytes)` (the look walks).
#[tokio::test]
async fn a_stage_of_large_chunks_is_looked_at_and_planned_without_walking_or_rewriting_the_blobs() {
    // What a "few pages" is: 32 pages of 4,096 bytes, against blobs of 2 MiB and more.
    const FEW: u64 = 128 * 1024;
    const OWNER: &str = "the account";
    let dir = tempfile::tempdir().expect("temp dir");
    let lazy = Arc::new(LazyStage::new(
        &dir.path().join("backend.db"),
        &DbKey::from([0x22; 32]),
    ));
    let capture = Capture::new(Arc::clone(&lazy), Arc::new(tokio::sync::Notify::new()));
    let mut volume = 0u64;
    for n in 0..4 {
        let blob = large_chunk(n, 512);
        volume += blob.len() as u64;
        let captured = capture
            .commit(
                OWNER,
                &format!("chunk-{n}"),
                Some(SyncType::RECENT as i32),
                blob,
            )
            .await
            .expect("capture");
        assert_eq!(captured, Captured::New);
    }
    assert!(
        volume > 2 * 1024 * 1024,
        "four blobs of about 512 KiB: {volume}"
    );
    let stage = lazy.handle(OWNER).expect("the stage");
    let wal = dir
        .path()
        .join(stage::DIR)
        .join(format!("{}-wal", stage::FILE));
    let wal_len = || std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    let wal_after_capture = wal_len();
    assert!(
        wal_after_capture >= volume,
        "the WAL holds the four blobs (no checkpoint yet): {wal_after_capture} of {volume}"
    );

    // The control: a deliberate walk of the blobs, on a fresh reader like the looks below. The meter must see all of it.
    let (walked, walk_reads) = {
        let stage = stage.clone();
        tokio::task::spawn_blocking(move || {
            measured(|| {
                stage
                    .reader()
                    .expect("reader")
                    .query_row("SELECT sum(length(hex(bytes))) FROM chunk_bytes", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .expect("walk")
            })
        })
        .await
        .expect("joins")
    };
    assert_eq!(walked as u64, 2 * volume, "the walk read every blob");
    assert!(
        walk_reads >= volume * 9 / 10,
        "the meter does not see a walk: {walk_reads} bytes read for blobs of {volume}"
    );

    // The planning: each blob is read once, and the commits do not write a blob again.
    let (report, plan_reads) = {
        let stage = stage.clone();
        tokio::task::spawn_blocking(move || measured(|| feed::plan_pending(&stage)))
            .await
            .expect("joins")
    };
    assert_eq!(report.expect("plans"), feed::Report { planned: 4, bad: 0 });
    assert!(
        plan_reads >= volume * 9 / 10 && plan_reads <= volume * 2,
        "the planning reads each blob once: {plan_reads} bytes read for blobs of {volume}"
    );
    let grown = wal_len().saturating_sub(wal_after_capture);
    assert!(
        grown < FEW,
        "planning four chunks grew the WAL by {grown} bytes: a blob was rewritten (blobs of {volume})"
    );

    // The look every pass of the driver makes, and the plan's own look for what is left to plan.
    let (staged, look_reads) = {
        let stage = stage.clone();
        tokio::task::spawn_blocking(move || measured(|| read_staged(&stage)))
            .await
            .expect("joins")
    };
    let staged = staged.expect("the chunks");
    assert_eq!(
        staged.iter().map(|c| c.state).collect::<Vec<_>>(),
        vec![State::Planned; 4],
        "the plan moved every chunk captured -> planned"
    );
    assert!(
        look_reads < FEW,
        "read_staged read {look_reads} bytes of a stage whose blobs are {volume}: it walks them"
    );
    let wal_before_idle = wal_len();
    let (idle, idle_reads) = {
        let stage = stage.clone();
        tokio::task::spawn_blocking(move || measured(|| feed::plan_pending(&stage)))
            .await
            .expect("joins")
    };
    assert_eq!(idle.expect("looks"), feed::Report::default());
    assert!(
        idle_reads < FEW,
        "plan_pending with nothing to plan read {idle_reads} bytes of a stage whose blobs are {volume}"
    );
    assert_eq!(wal_len(), wal_before_idle, "a look wrote to the stage");
    let plan_rows: i64 = stage
        .reader()
        .expect("reader")
        .query_row("SELECT count(*) FROM plan", [], |r| r.get(0))
        .expect("count");
    assert_eq!(plan_rows, 4, "one chat in each chunk's plan");
    eprintln!(
        "large chunks: blobs {volume} bytes; the control walk read {walk_reads}; planning read {plan_reads} and grew the WAL by {grown}; \
         read_staged read {look_reads}; a plan_pending with nothing to plan read {idle_reads}"
    );
}
