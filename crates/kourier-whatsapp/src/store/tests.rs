//! The store's own plumbing: the writer thread, the closed store, the text of a failure.

use std::error::Error as _;
use std::sync::mpsc;
use std::time::Duration;

use kourier_db::{DbKey, Priority};
use wacore::store::error::StoreError;

use super::{KourierStore, StoreOwner};
use crate::error::sql;

const KEY: [u8; 32] = [7; 32];

fn owner() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(&dir.path().join("backend.db"), &DbKey::from(KEY), "wa-test")
        .expect("open");
    (dir, owner)
}

async fn thread_name(store: &KourierStore) -> Result<Option<String>, StoreError> {
    store
        .read(|_| Ok(std::thread::current().name().map(str::to_owned)))
        .await
}

#[tokio::test]
async fn a_job_runs_on_the_writer_thread() {
    let (_dir, owner) = owner();
    let name = thread_name(&owner.store()).await.expect("read");
    assert_eq!(name.as_deref(), Some("wa-test"));
    owner.close();
}

#[tokio::test]
async fn a_call_after_close_fails_and_does_not_hang() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let clone = store.clone();
    owner.close();
    for s in [store, clone] {
        let answer = tokio::time::timeout(Duration::from_secs(5), thread_name(&s)).await;
        let err = answer
            .expect("a closed store must answer, not wait")
            .expect_err("a closed store fails");
        assert_eq!(err.to_string(), "database operation error");
        assert_eq!(err.source().expect("source").to_string(), "store closed");
    }
}

/// A job that PANICKED is not a closed store: the writer thread lives on and the next call works, so
/// the caller (W2's client, a log line) must not be told that the store is closed.
#[tokio::test]
async fn a_job_that_panicked_is_not_reported_as_a_closed_store() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let err = store
        .read::<(), _>(|_| panic!("a bug in a job"))
        .await
        .expect_err("the job panicked");
    assert_eq!(err.to_string(), "database operation error");
    assert_eq!(
        err.source().expect("source").to_string(),
        "store job failed"
    );
    // The thread lives on.
    let name = thread_name(&store).await.expect("the next call works");
    assert_eq!(name.as_deref(), Some("wa-test"));
    owner.close();
}

/// `close` is the actor's, after the client released the store: the jobs queued before it finish,
/// then the thread stops and the clones fail.
#[test]
fn a_job_queued_before_close_completes() {
    let (dir, owner) = owner();
    let store = owner.store();
    let handle = store.inner.handle.clone();
    // The writer is busy with a job that waits for us; a write queues behind it.
    let (release, held) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel::<()>();
    let busy = handle
        .submit(Priority::High, 0, move |_| {
            started_tx.send(()).expect("started");
            held.recv().expect("released");
        })
        .expect("queue the blocker");
    started.recv().expect("the blocker runs");
    let write = handle
        .submit(Priority::Low, 0, |conn| {
            conn.execute(
                "INSERT INTO group_metadata(group_jid, blob) VALUES ('g', x'01')",
                [],
            )
        })
        .expect("queue the write");
    let closing = std::thread::spawn(move || owner.close());
    // `close` has been entered once the writer takes no new job. The blocker still runs, so what
    // follows proves that `close` waits for the jobs queued before it.
    while handle.submit(Priority::High, 0, |_| ()).is_ok() {
        std::thread::yield_now();
    }
    assert!(
        !closing.is_finished(),
        "close returned before the queued jobs ran"
    );
    release.send(()).expect("release");
    closing.join().expect("close returns");
    busy.blocking_recv().expect("the blocker ended");
    assert_eq!(
        write.blocking_recv().expect("the write ran").expect("ok"),
        1
    );
    // The row is durable: a new store on the same file has it.
    let reopened = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from(KEY),
        "wa-test2",
    )
    .expect("reopen");
    let rows: i64 = reopened
        .store()
        .inner
        .handle
        .call_blocking(Priority::High, 0, |conn| {
            conn.query_row("SELECT count(*) FROM group_metadata", [], |r| r.get(0))
                .expect("count")
        })
        .expect("call");
    assert_eq!(rows, 1);
    reopened.close();
}

/// A failure names the operation and nothing the caller passed in (a JID, a phone number, a token).
#[tokio::test]
async fn a_failure_names_the_operation_and_no_input() {
    let (_dir, owner) = owner();
    let store = owner.store();
    let jid = "380501234567:7@s.whatsapp.net";
    let err = store
        .write("put session failed", move |tx| {
            // record IS NOT NULL: the statement fails and SQLite's own text would name the column.
            tx.execute(
                "INSERT INTO sessions(address, record) VALUES (?1, NULL)",
                [jid],
            )
            .map_err(sql("put session failed"))?;
            Ok(())
        })
        .await
        .expect_err("the insert is refused");
    let mut shown = format!("{err} | {err:?}");
    let mut source = err.source();
    while let Some(s) = source {
        shown.push_str(&format!(" | {s} | {s:?}"));
        source = s.source();
    }
    assert!(shown.contains("put session failed"), "{shown}");
    for leak in [
        "380501234567",
        "whatsapp.net",
        "sessions",
        "NOT NULL",
        "record",
    ] {
        assert!(!shown.contains(leak), "{leak} leaked into: {shown}");
    }
    // The failed zone left nothing behind and the store works on.
    let rows = store
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .map_err(sql("count"))
        })
        .await
        .expect("count");
    assert_eq!(rows, 0);
    assert_eq!(format!("{store:?}"), "KourierStore { .. }");
    assert_eq!(format!("{owner:?}"), "StoreOwner { .. }");
    owner.close();
}

/// The lookups of `has_signal_state_for_user` search the primary key by range; a scan of either
/// table would read every session on each call (`core.db`-style files carry no planner statistics, so
/// the plan is pinned by a test, as the core does for its statements).
#[tokio::test]
async fn the_lookups_for_a_users_state_use_the_primary_key() {
    let (_dir, owner) = owner();
    let store = owner.store();
    for statement in super::signal::HAS_STATE_SQL {
        let plan = store
            .read(move |conn| {
                let mut stmt = conn
                    .prepare(&format!("EXPLAIN QUERY PLAN {statement}"))
                    .map_err(sql("plan"))?;
                stmt.query_map(["a", "b"], |row| row.get::<_, String>(3))
                    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                    .map_err(sql("plan"))
            })
            .await
            .expect("plan");
        let plan = plan.join(" / ");
        assert!(plan.contains("SEARCH"), "{statement}: {plan}");
        assert!(plan.contains("PRIMARY KEY"), "{statement}: {plan}");
        assert!(!plan.contains("SCAN"), "{statement}: {plan}");
    }
    owner.close();
}
