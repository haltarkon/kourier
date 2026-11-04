//! The inbox's own behaviour (W3a, chunk C2): one zone per commit, the unique key, the catch-up flag,
//! and the seam that makes the zone fail. The hook-level kill tests (a child process killed at the
//! failpoints) are chunk C4.
#![allow(
    clippy::await_holding_lock,
    reason = "the seam is process-wide: a test holds fault::serial() for its whole run"
)]

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, FakeClock};
use kourier_db::{DbKey, Priority};

use super::{Arrival, Inbox, InboxMeta, Origin};
use crate::error::sql;
use crate::fault;
use crate::store::{KourierStore, StoreOwner};

const KEY: [u8; 32] = [9; 32];

fn setup() -> (tempfile::TempDir, StoreOwner, Inbox) {
    let dir = tempfile::tempdir().expect("temp dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from(KEY),
        "wa-inbox",
    )
    .expect("open");
    let inbox = Inbox::new(owner.store(), Arc::new(FakeClock::new(1_000_000)));
    (dir, owner, inbox)
}

fn arrival(id: &str, offline: bool) -> Arrival {
    Arrival {
        chat: "100@g.us".into(),
        sender: "200@lid".into(),
        id: id.into(),
        origin: Origin::E2e,
        message: vec![1, 2, 3, 4],
        meta: InboxMeta {
            v: InboxMeta::VERSION,
            timestamp: 1_700_000_000,
            from_me: false,
            is_group: true,
            push_name: "Synthetic Name".into(),
            lid_addressing: true,
            sender_alt: Some("31000000000@s.whatsapp.net".into()),
            recipient_alt: None,
            ephemeral_expiration: Some(86_400),
            is_offline: offline,
        },
    }
}

async fn count(store: &KourierStore, table: &'static str) -> i64 {
    store
        .read(move |conn| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .map_err(sql("count rows"))
        })
        .await
        .expect("count")
}

/// One column of the row `id`.
async fn field(store: &KourierStore, id: &'static str, column: &'static str) -> Option<String> {
    store
        .read(move |conn| {
            conn.query_row(
                &format!("SELECT CAST({column} AS TEXT) FROM inbox WHERE id = ?1"),
                [id],
                |r| r.get(0),
            )
            .map_err(sql("read a column"))
        })
        .await
        .expect("field")
}

/// Parks the store's writer thread until the returned sender is used: every job queued meanwhile waits.
async fn park(store: &KourierStore) -> mpsc::Sender<()> {
    let (release, wait) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel::<()>();
    let store = store.clone();
    tokio::spawn(async move {
        let _ = store
            .raw(Priority::High, move |_| {
                let _ = started_tx.send(());
                let _ = wait.recv();
                Ok(())
            })
            .await;
    });
    // The spawned task runs on this (current-thread) runtime: wait for it without blocking the thread.
    for _ in 0..500 {
        if started.try_recv().is_ok() {
            return release;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the writer did not park");
}

#[tokio::test]
async fn hook_is_pending_until_commit() {
    let _serial = fault::serial();
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    let release = park(&store).await;
    let mut commit = Box::pin(inbox.commit(vec![arrival("A", false)], Vec::new()));
    // The writer cannot have committed anything: the call must not be ready.
    assert!(
        futures::poll!(commit.as_mut()).is_pending(),
        "a commit that returns before the writer ran would acknowledge a message that is not durable"
    );
    release.send(()).expect("release");
    let done = commit.await.expect("committed");
    assert_eq!((done.total, done.inserted), (1, 1));
    assert_eq!(count(&store, "inbox").await, 1);
    owner.close();
}

#[tokio::test]
async fn a_replayed_message_is_not_written_twice() {
    let _serial = fault::serial();
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    let batch = || vec![arrival("A", false), arrival("B", false)];
    let first = inbox.commit(batch(), Vec::new()).await.expect("first");
    assert_eq!((first.total, first.inserted), (2, 2));
    let replay = inbox.commit(batch(), Vec::new()).await.expect("replay");
    assert_eq!(
        (replay.total, replay.inserted),
        (2, 0),
        "the unique key absorbs the replay"
    );
    assert_eq!(count(&store, "inbox").await, 2);

    // A row stage 2 finished stays as the ledger: a replay must not bring it back as `new`.
    store
        .write("test: finish a row", |tx| {
            tx.execute(
                "UPDATE inbox SET state = 'done', message = NULL, meta = NULL WHERE id = 'A'",
                [],
            )
            .map(drop)
            .map_err(sql("finish a row"))
        })
        .await
        .expect("finish");
    let again = inbox.commit(batch(), Vec::new()).await.expect("again");
    assert_eq!(again.inserted, 0);
    assert_eq!(field(&store, "A", "state").await.as_deref(), Some("done"));
    assert_eq!(
        field(&store, "A", "message").await,
        None,
        "the wiped content stays wiped"
    );
    assert_eq!(field(&store, "B", "state").await.as_deref(), Some("new"));
    owner.close();
}

#[tokio::test]
async fn a_failed_zone_leaves_nothing_behind_and_the_message_lands_once_when_it_comes_again() {
    let _serial = fault::serial();
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    let facts = || vec![BackendEvent::PinnedConversations(Vec::new())];
    fault::arm(fault::INBOX_COMMIT, 1);
    let failed = inbox
        .commit(vec![arrival("A", false), arrival("B", false)], facts())
        .await;
    assert!(failed.is_err(), "an armed seam must fail the commit");
    assert_eq!(count(&store, "inbox").await, 0, "the rows are rolled back");
    assert_eq!(
        count(&store, "core_outbox").await,
        0,
        "so are the facts of the same zone"
    );
    // The seam is quiet after its one failure: the server sends the batch again.
    let again = inbox
        .commit(vec![arrival("A", false), arrival("B", false)], facts())
        .await
        .expect("the redelivery commits");
    assert_eq!(again.inserted, 2);
    assert_eq!(count(&store, "inbox").await, 2);
    assert_eq!(
        count(&store, "core_outbox").await,
        1,
        "the facts rode in the same zone"
    );
    owner.close();
}

#[tokio::test]
async fn a_row_is_catch_up_while_the_boundary_is_unknown_or_when_it_came_from_the_offline_queue() {
    let _serial = fault::serial();
    let (_dir, owner, inbox) = setup();
    let store = owner.store();
    inbox
        .commit(vec![arrival("A", false)], Vec::new())
        .await
        .expect("A");
    store
        .write("test: boundary", |tx| {
            tx.execute("UPDATE catch_up SET boundary = 5 WHERE id = 1", [])
                .map(drop)
                .map_err(sql("boundary"))
        })
        .await
        .expect("boundary");
    inbox
        .commit(vec![arrival("B", false), arrival("C", true)], Vec::new())
        .await
        .expect("B, C");
    assert_eq!(field(&store, "A", "catch_up").await.as_deref(), Some("1"));
    assert_eq!(field(&store, "B", "catch_up").await.as_deref(), Some("0"));
    assert_eq!(field(&store, "C", "catch_up").await.as_deref(), Some("1"));
    owner.close();
}

#[tokio::test]
async fn an_empty_commit_writes_nothing() {
    let _serial = fault::serial();
    let (_dir, owner, inbox) = setup();
    let done = inbox.commit(Vec::new(), Vec::new()).await.expect("empty");
    assert_eq!((done.total, done.inserted), (0, 0));
    owner.close();
}

#[test]
fn nothing_printed_names_a_person_or_a_message() {
    let a = arrival("A-secret-id", false);
    let text = format!("{a:?} {:?}", a.meta);
    for needle in [
        "Synthetic Name",
        "100@g.us",
        "200@lid",
        "31000000000",
        "A-secret-id",
    ] {
        assert!(!text.contains(needle), "Debug printed {needle}");
    }
}

/// Plants ledger rows with their arrival numbers: `(arrival, id, state, received_at)`. A `new` row
/// carries its content, a `done` one does not (the table's check).
async fn plant(store: &KourierStore, rows: &[(i64, &'static str, &'static str, i64)]) {
    let rows = rows.to_vec();
    store
        .write("test: plant", move |tx| {
            for (arrival, id, state, at) in &rows {
                let content: Option<&[u8]> = if *state == "new" { Some(&[0]) } else { None };
                tx.execute(
                    "INSERT INTO inbox(arrival, chat, sender, id, origin, received_at, catch_up, \
                                       message, meta, state) \
                     VALUES (?1, '100@g.us', '200@lid', ?2, 'e2e', ?3, 0, ?4, ?4, ?5)",
                    kourier_db::rusqlite::params![arrival, id, at, content, state],
                )
                .map_err(sql("plant"))?;
            }
            Ok(())
        })
        .await
        .expect("plant");
}

async fn ids(store: &KourierStore) -> Vec<String> {
    store
        .read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id FROM inbox ORDER BY arrival")
                .map_err(sql("ids"))?;
            stmt.query_map([], |r| r.get(0))
                .and_then(|rows| rows.collect())
                .map_err(sql("ids"))
        })
        .await
        .expect("ids")
}

/// D7: the sweep takes a row only when it is finished (`done`: an unfinished one is a message that
/// is not yet in the core), was committed before the connection whose drain just completed
/// (`arrival <= covers`) and is older than the age floor (strictly: a row exactly at the floor
/// stays). A message must never be lost to the sweep.
#[tokio::test]
async fn the_ledger_sweep_takes_only_old_finished_rows_up_to_the_covering_arrival() {
    use super::{LEDGER_AGE_MS, sweep_ledger};
    const NOW: i64 = 3_000_000_000_000;
    let old = NOW - LEDGER_AGE_MS - 1;
    let (_dir, owner, _inbox) = setup();
    let store = owner.store();
    plant(
        &store,
        &[
            (1, "old-done", "done", old),
            (2, "at-the-floor", "done", NOW - LEDGER_AGE_MS),
            (3, "old-unfinished", "new", old),
            (4, "after-the-bound", "done", old),
        ],
    )
    .await;
    assert_eq!(sweep_ledger(&store, 3, NOW).await.expect("sweep"), 1);
    assert_eq!(
        ids(&store).await,
        ["at-the-floor", "old-unfinished", "after-the-bound"]
    );
    // The next connection covers the rest; the unfinished row and the one at the floor stay.
    assert_eq!(sweep_ledger(&store, 4, NOW).await.expect("sweep"), 1);
    assert_eq!(ids(&store).await, ["at-the-floor", "old-unfinished"]);
    // A day later the row at the floor is old enough.
    assert_eq!(
        sweep_ledger(&store, 4, NOW + 24 * 3_600_000)
            .await
            .expect("sweep"),
        1
    );
    assert_eq!(ids(&store).await, ["old-unfinished"]);
    owner.close();
}
