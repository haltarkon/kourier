//! Forwarder crash safety (§5.6 invariants 2-4, §10.4): kill the process between the core's
//! commit and the forwarder's DELETE, and inside the core transaction; after a restart every
//! event reaches the core exactly once and the outbox drains.
//!
//! The parent test re-runs this test binary as a child (`forwarder_crash_child`) with
//! `KOURIER_FAILPOINT` armed; the child aborts like a power cut after the last fsync.

use kourier_db::outbox::{self, FAILPOINT_AFTER_CORE_COMMIT};
use kourier_db::rusqlite::Connection;
use kourier_db::{Batch, DbKey, Forwarder, ForwarderConfig, ZoneDb, failpoint, migrate, open};
use std::cell::RefCell;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

const CHILD_DIR: &str = "KOURIER_TEST_FORWARDER_DIR";
const TOTAL: i64 = 250;
const CORE_BEFORE_COMMIT: &str = "test_core_before_commit";
const SIGABRT: i32 = 6;

fn backend_key() -> DbKey {
    DbKey::from([0xb0; 32])
}

fn core_key() -> DbKey {
    DbKey::from([0xc0; 32])
}

fn setup(dir: &Path) {
    let mut backend = open(&dir.join("backend.db"), &backend_key()).unwrap();
    migrate(&mut backend, &outbox::MIGRATIONS).unwrap();
    let backend = ZoneDb::new(backend);
    // 25 backend transactions, each committing 10 events with its (here: absent) state change.
    for chunk in 0..TOTAL / 10 {
        backend
            .zone(|tx| -> kourier_db::Result<()> {
                for i in chunk * 10..chunk * 10 + 10 {
                    tx.outbox_append(format!("v1:event:{i}").as_bytes(), kourier_db::now_ms())?;
                }
                Ok(())
            })
            .unwrap();
    }
    let core = open(&dir.join("core.db"), &core_key()).unwrap();
    core.execute_batch(
        "CREATE TABLE events(id INTEGER PRIMARY KEY);
         CREATE TABLE stats(delivered INTEGER NOT NULL);
         INSERT INTO stats VALUES (0);",
    )
    .unwrap();
}

/// The core side of the test: one transaction per envelope, idempotent on the event id.
fn ingest(core: &RefCell<Connection>, batch: &Batch) -> Result<(), kourier_db::rusqlite::Error> {
    let mut core = core.borrow_mut();
    let tx = core.transaction()?;
    for row in batch.rows() {
        let text = std::str::from_utf8(&row.envelope).unwrap();
        let id: i64 = text.strip_prefix("v1:event:").unwrap().parse().unwrap();
        tx.execute("INSERT OR IGNORE INTO events(id) VALUES (?1)", [id])?;
    }
    tx.execute(
        "UPDATE stats SET delivered = delivered + ?1",
        [i64::try_from(batch.rows().len()).unwrap()],
    )?;
    failpoint::hit(CORE_BEFORE_COMMIT);
    tx.commit()
}

async fn forward_all(dir: &Path) {
    let backend = Rc::new(ZoneDb::new(
        open(&dir.join("backend.db"), &backend_key()).unwrap(),
    ));
    let core = RefCell::new(open(&dir.join("core.db"), &core_key()).unwrap());
    let cfg = ForwarderConfig {
        max_rows: 100,
        ..ForwarderConfig::default()
    };
    Forwarder::new(backend, cfg)
        .drain(|batch| std::future::ready(ingest(&core, &batch)))
        .await
        .unwrap();
}

struct State {
    events: i64,
    distinct_ok: bool,
    delivered: i64,
    outbox: u64,
}

fn state(dir: &Path) -> State {
    let core = open(&dir.join("core.db"), &core_key()).unwrap();
    let events: i64 = core
        .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
        .unwrap();
    let max: Option<i64> = core
        .query_row("SELECT max(id) FROM events", [], |r| r.get(0))
        .unwrap();
    let delivered: i64 = core
        .query_row("SELECT delivered FROM stats", [], |r| r.get(0))
        .unwrap();
    let backend = open(&dir.join("backend.db"), &backend_key()).unwrap();
    State {
        events,
        // Ids are a prefix 0..events: nothing skipped, nothing out of order.
        distinct_ok: max.map_or(events == 0, |m| m + 1 == events),
        delivered,
        outbox: outbox::len(&backend).unwrap(),
    }
}

fn crash_child(dir: &Path, failpoint: &str) {
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "forwarder_crash_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env(CHILD_DIR, dir)
        .env(failpoint::ENV, failpoint)
        .status()
        .unwrap();
    assert_eq!(
        status.signal(),
        Some(SIGABRT),
        "child must die at {failpoint}: {status}"
    );
}

/// Runs only as the child of `forwarder_survives_crashes`; a no-op in a normal test run.
#[tokio::test]
async fn forwarder_crash_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR) else {
        return;
    };
    forward_all(Path::new(&dir)).await;
    eprintln!("child finished without hitting its failpoint");
    std::process::exit(3);
}

#[tokio::test]
async fn forwarder_survives_crashes() {
    if !failpoint::ENABLED {
        eprintln!("skipped: failpoints are compiled out (release build without `failpoints`)");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());

    // 1. Killed after the core committed batch 2 (rows 100..199), before the DELETE.
    crash_child(dir.path(), &format!("{FAILPOINT_AFTER_CORE_COMMIT}:2"));
    let s = state(dir.path());
    assert_eq!((s.events, s.delivered, s.outbox), (200, 200, 150));
    assert!(s.distinct_ok);

    // 2. Killed inside the core transaction while re-forwarding batch 2: nothing of it commits.
    crash_child(dir.path(), &format!("{CORE_BEFORE_COMMIT}:1"));
    let s = state(dir.path());
    assert_eq!((s.events, s.delivered, s.outbox), (200, 200, 150));

    // 3. Restart without failpoints: batch 2 ships again and is absorbed, the rest follows.
    forward_all(dir.path()).await;
    let s = state(dir.path());
    assert_eq!(s.events, TOTAL, "every event exactly once");
    assert!(s.distinct_ok);
    assert_eq!(s.delivered, 200 + 150, "batch 2 was delivered twice");
    assert_eq!(s.outbox, 0, "the outbox drained");
}
