use std::path::PathBuf;
use std::sync::Mutex;

use kourier_db::Priority;

use super::*;
use crate::store::GLOBAL_LANE;

#[test]
fn a_rowid_holds_the_hour_and_gives_the_id_back() {
    let sort_key = 1_700_000_000_000 * 1024;
    let rowid = rowid_of(sort_key, 42).expect("rowid");
    assert_eq!(rowid & ID_MASK, 42);
    assert_eq!(rowid >> ID_BITS, sort_key / HOUR);
    // Later hours give larger rowids whatever the ids.
    let later = rowid_of(sort_key + HOUR, 1).expect("rowid");
    assert!(later > rowid_of(sort_key, ID_MASK).expect("rowid"));
    // Inside an hour the id orders.
    assert!(rowid_of(sort_key, 43).expect("rowid") > rowid);
    // A hostile sort key lands at a clamp, never past it.
    assert_eq!(rowid_of(-5, 7), Some(7));
    assert_eq!(rowid_of(i64::MAX, 7), Some((LAST_HOUR << ID_BITS) | 7));
    const { assert!(i64::MAX / HOUR > LAST_HOUR) };
    assert!(rowid_of(i64::MAX, ID_MASK).is_some_and(|r| r > 0));
    assert_eq!(rowid_of(0, ID_MASK + 1), None);
    assert_eq!(rowid_of(0, -1), None);
}

#[test]
fn the_batch_follows_the_time_of_the_last_job() {
    assert_eq!(next_batch(100, JOB_BUDGET), 100);
    // A slow job (a mass delete's COMMIT) halves it and more, never below the floor.
    assert_eq!(next_batch(100, JOB_BUDGET * 4), MIN_BATCH);
    assert_eq!(next_batch(100, JOB_BUDGET * 2), 50);
    // A quick one doubles it at most, never above the cap.
    assert_eq!(next_batch(100, Duration::from_micros(10)), 200);
    assert_eq!(next_batch(400, Duration::from_micros(10)), MAX_BATCH);
    assert_eq!(next_batch(MIN_BATCH, Duration::from_secs(10)), MIN_BATCH);
}

const KEY: [u8; 32] = [3; 32];

/// A message row of chat 1 with a synthetic body.
fn insert(c: &Connection, id: i64) -> Result<()> {
    c.execute(
        "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
             origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES (?1, 1, 1, 0, 'aci:b', ?2, 'Remote', ?3 * 1024, ?3, ?3, 'Text', ?4)",
        params![
            id,
            id.to_string(),
            1_700_000_000_000 + id * 1000,
            format!("{{\"Text\":{{\"body\":\"row{id}q\"}}}}")
        ],
    )?;
    Ok(())
}

/// A store with `n` text rows of one chat, written before search.db exists: the task makes the
/// file at its open with every row at or below `floor` (as at the first start of this build).
async fn seeded(n: i64) -> (tempfile::TempDir, Arc<Store>) {
    let dir = tempfile::tempdir().expect("tempdir");
    let (store, _) =
        Store::open(&dir.path().join("core.db"), &kourier_db::DbKey::from(KEY)).expect("open");
    let store = Arc::new(store);
    store
        .write(Priority::High, GLOBAL_LANE, move |c| {
            c.execute_batch(
                "INSERT INTO accounts(id, uuid, network, state, created_at)
                   VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
                 INSERT INTO conversations(id, account_id, conv_key, kind)
                   VALUES (1, 1, 'aci:b', 'Direct');",
            )?;
            let tx = c.transaction()?;
            for id in 1..=n {
                insert(&tx, id)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("seed");
    (dir, store)
}

fn search_path(dir: &tempfile::TempDir) -> PathBuf {
    file::beside(&dir.path().join("core.db"))
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(kourier_backend_api::SystemClock)
}

/// A number of search.db's `meta`, read on a connection of the test's own (WAL: it reads while
/// the task holds a write transaction). The task makes the file after its start: until it is
/// there with its schema (up to 20 s), this waits.
async fn meta_of(dir: &tempfile::TempDir, key: &'static str) -> i64 {
    let path = search_path(dir);
    let started = Instant::now();
    loop {
        let path = path.clone();
        let read = tokio::task::spawn_blocking(move || {
            let conn = kourier_db::open_read_only(&path, &kourier_db::DbKey::from(KEY)).ok()?;
            meta(&conn, key).ok()
        })
        .await
        .expect("join");
        if let Some(v) = read {
            return v;
        }
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "search.db is made"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// How many entries the index holds.
async fn entries(dir: &tempfile::TempDir) -> i64 {
    let path = search_path(dir);
    tokio::task::spawn_blocking(move || {
        let conn =
            kourier_db::open_read_only(&path, &kourier_db::DbKey::from(KEY)).expect("search.db");
        conn.query_row("SELECT count(*) FROM messages_fts_docsize", [], |r| {
            r.get(0)
        })
        .expect("docsize")
    })
    .await
    .expect("join")
}

/// Waits (up to 20 s) until `cond` holds.
async fn until<F, Fut>(what: &str, mut cond: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = Instant::now();
    while !cond().await {
        assert!(started.elapsed() < Duration::from_secs(120), "{what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Waits on a hold point's receiver off the runtime, up to `timeout`.
async fn reached(rx: &Arc<Mutex<std::sync::mpsc::Receiver<()>>>, timeout: Duration) -> bool {
    let rx = rx.clone();
    tokio::task::spawn_blocking(move || {
        rx.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv_timeout(timeout)
            .is_ok()
    })
    .await
    .expect("join")
}

/// Runs the truncating checkpoint on core.db's WRITER and says whether it completed: it does
/// only when no read snapshot of core.db is open (a connection of the search task that held
/// one over its search.db work would make it busy, and a removed account's rows would stay
/// in the core's WAL, C1).
async fn core_truncates(store: &Arc<Store>) -> bool {
    store
        .write(Priority::High, GLOBAL_LANE, |c| {
            Ok(kourier_db::try_checkpoint_truncate(c)?)
        })
        .await
        .expect("a checkpoint job")
}

async fn close(store: Arc<Store>) {
    tokio::task::spawn_blocking(move || store.close())
        .await
        .expect("close");
}

/// The core's writer never waits for search (the fix round, HIGH-1): while the search task
/// holds search.db's writer inside a backfill batch, a High job of the core's writer runs at
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_core_writer_never_waits_for_a_backfill_batch() {
    let (dir, store) = seeded(300).await;
    let (reached_rx, release) = hold::arm("backfill");
    let reached_rx = Arc::new(Mutex::new(reached_rx));
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move { keep(&store, Duration::ZERO, clock(), || false).await })
    };
    assert!(
        reached(&reached_rx, Duration::from_secs(20)).await,
        "a backfill batch holds the search writer"
    );
    assert!(
        core_truncates(&store).await,
        "a read snapshot of core.db is open while the backfill writes search.db"
    );
    let started = Instant::now();
    store
        .write(Priority::High, GLOBAL_LANE, |c| insert(c, 301))
        .await
        .expect("a core write while the batch holds");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the core's writer waited for the search writer: {:?}",
        started.elapsed()
    );
    hold::disarm();
    drop(release);
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    until("every row is indexed", || async {
        entries(&dir).await == 301
    })
    .await;
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}

/// A compaction runs to its END with no flush between its steps (HIGH-2): `applied` does not
/// move while it runs, although the log holds a new row; the core's writer takes that row at
/// once meanwhile (HIGH-1); after the end the row is flushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compaction_runs_to_its_end_with_no_flush_and_the_core_writer_never_waits() {
    let (dir, store) = seeded(400).await;
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move { keep(&store, Duration::ZERO, clock(), || false).await })
    };
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    until("every row is indexed", || async {
        entries(&dir).await == 400
    })
    .await;
    let (reached_rx, release) = hold::arm("compact_step");
    let reached_rx = Arc::new(Mutex::new(reached_rx));
    // A clear's shape: rows go and a compaction is asked in the same transaction. One row in
    // twenty, spread: FTS5 folds a level whose rows are 10 % or more deleted by itself in the
    // flush that wrote the tombstones (its `deletemerge`), and the compaction would then find
    // nothing to do.
    store
        .write(Priority::High, GLOBAL_LANE, |c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM messages WHERE id % 20 = 0", [])?;
            log::ask_now(&tx)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("delete");
    assert!(
        reached(&reached_rx, Duration::from_secs(20)).await,
        "a compaction step holds the search writer"
    );
    let applied = meta_of(&dir, "applied").await;
    assert!(
        core_truncates(&store).await,
        "a read snapshot of core.db is open during a compaction step"
    );
    let started = Instant::now();
    store
        .write(Priority::High, GLOBAL_LANE, |c| insert(c, 401))
        .await
        .expect("a core write during the compaction");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the core's writer waited for the compaction: {:?}",
        started.elapsed()
    );
    let mut steps = 1;
    loop {
        release.send(()).expect("release the step");
        if !reached(&reached_rx, Duration::from_secs(3)).await {
            break;
        }
        steps += 1;
        assert_eq!(
            meta_of(&dir, "applied").await,
            applied,
            "a flush ran between two steps of the compaction (step {steps})"
        );
        assert!(steps < 1_000, "the compaction ends");
    }
    assert!(
        steps >= 2,
        "the compaction ends with a step that does no work"
    );
    hold::disarm();
    drop(release);
    until("the new row is flushed after the compaction", || async {
        meta_of(&dir, "applied").await > applied
    })
    .await;
    until("the index holds what is left", || async {
        entries(&dir).await == 381
    })
    .await;
    assert_eq!(
        meta_of(&dir, "dead").await,
        0,
        "the compaction took its snapshot"
    );
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}

/// The backfill waits for its time after `UiReady` (the log does not); then it reads every row
/// by itself. The log is pruned up to what search.db committed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_backfill_waits_for_its_time_and_the_log_does_not() {
    let (dir, store) = seeded(50).await;
    let keeper = {
        let store = store.clone();
        tokio::spawn(
            async move { keep(&store, Duration::from_secs(3600), clock(), || false).await },
        )
    };
    let link = store.search_link();
    until("the file is judged", || {
        let ready = link.is_ready();
        async move { ready }
    })
    .await;
    // A new row: the log is flushed at once.
    store
        .write(Priority::High, GLOBAL_LANE, |c| insert(c, 51))
        .await
        .expect("new row");
    until("the log is flushed", || async { entries(&dir).await == 1 }).await;
    until("the log is pruned", || async {
        store
            .read(|c| {
                Ok(c.query_row("SELECT count(*) FROM search_queue", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
            .expect("log")
            == 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        meta_of(&dir, "floor").await,
        50,
        "no batch before the backfill's time"
    );
    keeper.abort();
    let _ = keeper.await;
    // With a short delay it starts by itself and reads every row.
    let keeper = {
        let store = store.clone();
        tokio::spawn(
            async move { keep(&store, Duration::from_millis(200), clock(), || false).await },
        )
    };
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    assert_eq!(entries(&dir).await, 51);
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}

/// A row of chat 1 as the index reads it.
fn row_of(n: i64) -> (i64, Option<Row>) {
    (
        n,
        Some(Row {
            conv: 1,
            sort_key: 1_700_000_000_000 * 1024 + n,
            body: format!("{{\"Text\":{{\"body\":\"fileerr{n}q\"}}}}"),
            searchable: true,
        }),
    )
}

/// A FILE error in the id-by-id pass is returned, never skipped (review P2): the disk fills
/// up while the rows go in (`max_page_count` stops the file's growth: SQLITE_FULL inside an
/// insert), and the pass fails whole with `applied` unmoved and nothing counted skipped.
#[test]
fn a_file_error_in_the_id_by_id_pass_is_returned_and_never_skipped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = dir.path().join("core.db");
    let (store, _) = Store::open(&core, &kourier_db::DbKey::from(KEY)).expect("open");
    store.close();
    let paths = file::Paths {
        core: core.clone(),
        core_key: kourier_db::DbKey::from(KEY),
        search: file::beside(&core),
        search_key: kourier_db::DbKey::from([5; 32]),
    };
    let (mut files, _) = Files::open(&paths).expect("files");
    let pages: i64 = files
        .search
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("pages");
    files
        .search
        .execute_batch(&format!("PRAGMA max_page_count = {pages}"))
        .expect("a full disk");
    let rows: Vec<_> = (1..=1500).map(row_of).collect();
    let mut out = Flushed::default();
    let e = write(&mut files.search, 1500, &rows, &mut out, true)
        .expect_err("the disk is full: the pass fails");
    assert!(file::is_file_error(&e), "a file error, not a row's: {e}");
    assert_eq!(out.skipped, 0, "nothing is skipped for it");
    assert_eq!(meta(&files.search, "applied").expect("applied"), 0);
    // And the table the poison pass reads its errors by.
    files
        .search
        .execute_batch("PRAGMA max_page_count = 1073741823")
        .expect("room");
    let mut out = Flushed::default();
    write(&mut files.search, 1500, &rows, &mut out, true).expect("with room it all goes in");
    assert_eq!((out.indexed, out.skipped), (1500, 0));
}

/// A FILE error inside the probe's own insert (R1): the full file rolls the whole transaction
/// back, the savepoint is gone, and the probe still returns the insert's file error (not "no
/// such savepoint", which is no file error and would count toward Stuck).
#[test]
fn a_file_error_in_the_probe_is_returned_as_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = dir.path().join("core.db");
    let (store, _) = Store::open(&core, &kourier_db::DbKey::from(KEY)).expect("open");
    store.close();
    let paths = file::Paths {
        core: core.clone(),
        core_key: kourier_db::DbKey::from(KEY),
        search: file::beside(&core),
        search_key: kourier_db::DbKey::from([5; 32]),
    };
    let (mut files, _) = Files::open(&paths).expect("files");
    let rows: Vec<_> = (1..=3000).map(row_of).collect();
    let mut out = Flushed::default();
    write(&mut files.search, 3000, &rows, &mut out, true).expect("rows go in");
    files.search.execute_batch("VACUUM").expect("vacuum");
    let pages: i64 = files
        .search
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("pages");
    files
        .search
        .execute_batch(&format!("PRAGMA max_page_count = {pages}"))
        .expect("a full disk");
    let tx = files.search.unchecked_transaction().expect("tx");
    let e = probe(&tx).expect_err("the file is full");
    assert!(file::is_file_error(&e), "the insert's file error: {e}");
}

/// A READ-ONLY search.db is built again (R2): the file cannot be written, the directory can.
/// The mode drops while no task runs (a restart sees it), a new row is waiting, and the three
/// tries that fail with SQLITE_READONLY take the rebuild, which makes a new file.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_read_only_file_is_built_again() {
    use std::os::unix::fs::PermissionsExt;
    let (dir, store) = seeded(30).await;
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move { keep(&store, Duration::ZERO, clock(), || false).await })
    };
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    until("every row is indexed", || async {
        entries(&dir).await == 30
    })
    .await;
    keeper.abort();
    let _ = keeper.await;
    let path = search_path(&dir);
    {
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = kourier_db::open(&path, &kourier_db::DbKey::from(KEY)).expect("search.db");
            conn.execute_batch("CREATE TABLE marker(x)")
                .expect("a marker");
        })
        .await
        .expect("join");
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).expect("chmod");
    if std::fs::OpenOptions::new().write(true).open(&path).is_ok() {
        // The mode does not stop this user (root): nothing to test.
        eprintln!("skipped: a read-only file is writable for this user");
        close(store).await;
        return;
    }
    store
        .write(Priority::High, GLOBAL_LANE, |c| insert(c, 31))
        .await
        .expect("a new row");
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move {
            // The back-off is a few ms (explicit), not RETRY_AFTER: no load-dependent time.
            keep_with(
                &store,
                Duration::ZERO,
                PACE,
                clock(),
                || false,
                Duration::from_millis(5),
                None,
            )
            .await;
        })
    };
    until("the batch is applied", || async {
        entries(&dir).await == 31
    })
    .await;
    let marker: i64 = tokio::task::spawn_blocking(move || {
        let conn = kourier_db::open_read_only(&path, &kourier_db::DbKey::from(KEY)).expect("db");
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'marker'",
            [],
            |r| r.get(0),
        )
        .expect("marker")
    })
    .await
    .expect("join");
    assert_eq!(marker, 0, "the read-only file was built again");
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}

/// The error table of the poison pass: the file's and the machine's errors are never a row's.
#[test]
fn the_error_table_tells_a_row_from_the_file() {
    use kourier_db::rusqlite::ffi;
    let sql = |code: i32| {
        CoreError::Sql(kourier_db::rusqlite::Error::SqliteFailure(
            ffi::Error::new(code),
            None,
        ))
    };
    for code in [
        ffi::SQLITE_CORRUPT,
        ffi::SQLITE_NOTADB,
        ffi::SQLITE_FULL,
        ffi::SQLITE_IOERR,
        ffi::SQLITE_NOMEM,
        ffi::SQLITE_BUSY,
        ffi::SQLITE_LOCKED,
        ffi::SQLITE_INTERRUPT,
        ffi::SQLITE_READONLY,
        ffi::SQLITE_CANTOPEN,
    ] {
        let e = sql(code);
        assert!(file::is_file_error(&e), "{code} is the file's");
        assert!(!file::is_row_error(&e), "{code} is no row's");
    }
    for code in [
        ffi::SQLITE_CONSTRAINT,
        ffi::SQLITE_TOOBIG,
        ffi::SQLITE_ERROR,
    ] {
        let e = sql(code);
        assert!(file::is_row_error(&e), "{code} may be a row's");
        assert!(!file::is_file_error(&e), "{code} is not the file's");
    }
    // Extended codes read by their primary code.
    assert!(file::is_file_error(&sql(ffi::SQLITE_IOERR_WRITE)));
    assert!(file::is_row_error(&sql(ffi::SQLITE_CONSTRAINT_TRIGGER)));
    // What is no SQLite error at all is neither.
    let other = CoreError::Stopped;
    assert!(!file::is_file_error(&other) && !file::is_row_error(&other));
    assert!(file::is_corrupt(&sql(ffi::SQLITE_CORRUPT)));
    assert!(file::is_unwritable(&sql(ffi::SQLITE_READONLY)));
    assert!(!file::is_unwritable(&sql(ffi::SQLITE_FULL)));
    assert!(!file::is_corrupt(&sql(ffi::SQLITE_FULL)));
}

/// THE PRUNE IN PIECES (review C3): 12,000 log rows applied are pruned in three jobs of the
/// core's writer, none over [`log::PRUNE_ROWS`], and the log ends empty; a High job queued at
/// once is not held up for all three (it runs between two of them).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_big_prune_runs_in_jobs_of_at_most_five_thousand_rows() {
    let (_dir, store) = seeded(12_000).await;
    let rows = |store: Arc<Store>| async move {
        store
            .read(|c| {
                Ok(c.query_row("SELECT count(*) FROM search_queue", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
            .expect("log")
    };
    assert_eq!(rows(store.clone()).await, 12_000);
    store.post_prune(12_000);
    let seen = store
        .write(Priority::High, GLOBAL_LANE, {
            let store = store.clone();
            move |_| Ok(store.prune_counts().0)
        })
        .await
        .expect("a High job meanwhile");
    until("the log is pruned", || {
        let store = store.clone();
        // The job counts itself after its delete is committed: wait for the count too.
        async move { rows(store.clone()).await == 0 && store.prune_counts().0 >= 3 }
    })
    .await;
    let (jobs, most) = store.prune_counts();
    assert_eq!(jobs, 3, "5,000 + 5,000 + 2,000 rows");
    assert_eq!(most, log::PRUNE_ROWS, "no job deletes more than the cap");
    assert!(
        seen < 3,
        "the High job waited for all of the prune ({seen} jobs ran before it)"
    );
    close(store).await;
}

/// THE SAME BATCH FAILING THREE TIMES (review P2): the first failure backs off, the second goes
/// id by id (every id fails here: it is the file), the third builds the file again, which is
/// cured of what the test did to it, and the index ends whole. Time is paused: the back-offs
/// of 30 s pass at once.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn three_failures_of_the_same_batch_build_the_file_again() {
    let (dir, store) = seeded(30).await;
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move { keep(&store, Duration::ZERO, clock(), || false).await })
    };
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    until("every row is indexed", || async {
        entries(&dir).await == 30
    })
    .await;
    // The file is made unable to take a new line, as a damaged or full one would be.
    let path = search_path(&dir);
    {
        let path = path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = kourier_db::open(&path, &kourier_db::DbKey::from(KEY)).expect("search.db");
            conn.execute_batch(
                "CREATE TRIGGER stuck BEFORE INSERT ON indexed
                   BEGIN SELECT RAISE(ABORT, 'stuck'); END;",
            )
            .expect("the failure");
        })
        .await
        .expect("join");
    }
    store
        .write(Priority::High, GLOBAL_LANE, |c| insert(c, 31))
        .await
        .expect("a new row");
    // The rebuild removes the file with its trigger, and the rows come back.
    until("the file is built again and whole", || async {
        let path = path.clone();
        let (triggers, rows) = tokio::task::spawn_blocking(move || {
            let conn = kourier_db::open_read_only(&path, &kourier_db::DbKey::from(KEY)).ok()?;
            let triggers: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'trigger'",
                    [],
                    |r| r.get(0),
                )
                .ok()?;
            let rows: i64 = conn
                .query_row("SELECT count(*) FROM messages_fts_docsize", [], |r| {
                    r.get(0)
                })
                .ok()?;
            Some((triggers, rows))
        })
        .await
        .expect("join")
        .unwrap_or((1, 0));
        triggers == 0 && rows == 31
    })
    .await;
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}

/// A FILE error is not a reason to build the file again (re-check NEW): search.db is held
/// busy by another writer for far longer than three tries take, and the healthy index stays
/// (a marker table put in it is still there), then the flush applies its batch when the lock
/// goes. The busy timeout (20 ms) and the back-off (5 ms) are explicit, and the test counts the
/// attempts and lets the lock go after the third failure: no real-time wait.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_busy_file_for_three_tries_is_not_built_again() {
    let (dir, store) = seeded(30).await;
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move {
            // A short busy timeout and back-off (explicit), so three failed tries take
            // milliseconds, not three 5 s timeouts and three 30 s back-offs.
            keep_with(
                &store,
                Duration::ZERO,
                PACE,
                clock(),
                || false,
                Duration::from_millis(5),
                Some(Duration::from_millis(20)),
            )
            .await;
        })
    };
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    until("every row is indexed", || async {
        entries(&dir).await == 30
    })
    .await;
    let path = search_path(&dir);
    let other = kourier_db::open(&path, &kourier_db::DbKey::from(KEY)).expect("search.db");
    other
        .execute_batch("CREATE TABLE marker(x)")
        .expect("a marker");
    other.execute_batch("BEGIN IMMEDIATE").expect("the lock");
    store
        .write(Priority::High, GLOBAL_LANE, |c| insert(c, 31))
        .await
        .expect("a new row");
    // Counted from here, with the new row in the log: every flush attempt, where it starts.
    // The tries fail on the lock.
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let attempts = attempts.clone();
        hold::arm_on(
            "flush",
            Box::new(move |_| {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        );
    }
    // The fourth attempt starts after three have failed on the lock (and the third failure
    // did not take the rebuild).
    until("three tries failed on the lock", || async {
        attempts.load(std::sync::atomic::Ordering::SeqCst) >= 4
    })
    .await;
    hold::disarm();
    other.execute_batch("ROLLBACK").expect("the lock goes");
    until("the batch is applied", || async {
        entries(&dir).await == 31
    })
    .await;
    let marker: i64 = tokio::task::spawn_blocking(move || {
        let conn = kourier_db::open_read_only(&path, &kourier_db::DbKey::from(KEY)).expect("db");
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'marker'",
            [],
            |r| r.get(0),
        )
        .expect("marker")
    })
    .await
    .expect("join");
    assert_eq!(marker, 1, "the file was never built again");
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}

/// THE SHUTDOWN INTERRUPTS the search writer (review C4): a compaction step that runs a long
/// statement when the core shuts down is ended at once by `Store::interrupt_search` (the call
/// of `Core::shutdown`), and the task, told it is stopping, ends without a back-off; reopened,
/// the index is whole. The step's statement is a long one the test runs on the job's own
/// connection at the `compact_step` point (a real step at this size takes milliseconds).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_shutdown_interrupts_a_running_compaction_step() {
    let (dir, store) = seeded(400).await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let keeper = {
        let (store, stop) = (store.clone(), stop.clone());
        tokio::spawn(async move {
            keep(&store, Duration::ZERO, clock(), move || {
                stop.load(std::sync::atomic::Ordering::SeqCst)
            })
            .await;
        })
    };
    until("the backfill ends", || async {
        meta_of(&dir, "floor").await == 0
    })
    .await;
    until("every row is indexed", || async {
        entries(&dir).await == 400
    })
    .await;
    let (reached_tx, reached_rx) = std::sync::mpsc::channel::<()>();
    let (took_tx, took_rx) = std::sync::mpsc::channel::<(bool, Duration)>();
    let (reached_tx, took_tx) = (Mutex::new(reached_tx), Mutex::new(took_tx));
    hold::arm_on(
        "compact_step",
        Box::new(move |conn| {
            let _ = reached_tx.lock().expect("lock").send(());
            let started = Instant::now();
            let ran = conn.query_row(
                "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000000000)
                 SELECT count(*) FROM c",
                [],
                |r| r.get::<_, i64>(0),
            );
            let _ = took_tx
                .lock()
                .expect("lock")
                .send((ran.is_err(), started.elapsed()));
        }),
    );
    store
        .write(Priority::High, GLOBAL_LANE, |c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM messages WHERE id % 20 = 0", [])?;
            log::ask_now(&tx)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("delete");
    let reached = tokio::task::spawn_blocking(move || {
        reached_rx
            .recv_timeout(Duration::from_secs(20))
            .map(|()| reached_rx)
    })
    .await
    .expect("join");
    assert!(reached.is_ok(), "a compaction step runs a statement");
    // The shutdown: the flag, then the interrupt.
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    store.interrupt_search();
    let (interrupted, took) = tokio::task::spawn_blocking(move || {
        took_rx
            .recv_timeout(Duration::from_secs(20))
            .expect("ended")
    })
    .await
    .expect("join");
    assert!(interrupted, "the running statement was interrupted");
    assert!(
        took < Duration::from_secs(2),
        "the interrupt took {took:?} to end the step"
    );
    hold::disarm();
    tokio::time::timeout(Duration::from_secs(10), keeper)
        .await
        .expect("the task ends at once, with no back-off")
        .expect("join");
    close(store).await;
    // Reopened, the index is whole.
    let (store, _) =
        Store::open(&dir.path().join("core.db"), &kourier_db::DbKey::from(KEY)).expect("reopen");
    let store = Arc::new(store);
    let keeper = {
        let store = store.clone();
        tokio::spawn(async move { keep(&store, Duration::ZERO, clock(), || false).await })
    };
    until("the index holds what is left", || async {
        entries(&dir).await == 380
    })
    .await;
    keeper.abort();
    let _ = keeper.await;
    close(store).await;
}
