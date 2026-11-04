//! The books of the start-up resync and the map version (W6a1, A2): generations count per collection, a fatal mark
//! stays until a new map version or a link clears it, the link stamps every collection, the map version reads 0 until
//! written and keeps the last.

use kourier_db::DbKey;

use super::{Book, asked, book, clear_fatal, fatal, map_version, set_map_version, stamp_link};
use crate::store::{KourierStore, StoreOwner};

const LOW: &str = "regular_low";
const HIGH: &str = "regular_high";
const BLOCK: &str = "critical_block";

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([12; 32]),
        "wa-resyncs",
    )
    .expect("open");
    (dir, owner)
}

async fn books(store: &KourierStore, collection: &'static str) -> Option<Book> {
    store
        .read(move |conn| book(conn, collection))
        .await
        .expect("book")
}

#[tokio::test]
async fn each_ask_opens_the_next_generation_and_a_fatal_answer_waits_for_a_link_or_a_new_map() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert_eq!(books(&store, LOW).await, None, "never asked");
    let first = store
        .write("ask", |tx| asked(tx, LOW, 1_000))
        .await
        .expect("ask");
    let second = store
        .write("ask", |tx| asked(tx, LOW, 2_000))
        .await
        .expect("ask");
    assert_eq!(
        (first, second),
        (1, 2),
        "the generations of one collection count up"
    );
    assert_eq!(
        store
            .write("ask", |tx| asked(tx, HIGH, 2_500))
            .await
            .expect("ask"),
        1,
        "another collection counts for itself"
    );
    assert_eq!(
        books(&store, LOW).await,
        Some(Book {
            asked_at: 2_000,
            generation: 2,
            fatal_at: None
        })
    );
    // A fatal answer keeps the time and the generation of the ask and marks the collection.
    store
        .write("fatal", |tx| fatal(tx, LOW, 3_000))
        .await
        .expect("fatal");
    assert_eq!(
        books(&store, LOW).await,
        Some(Book {
            asked_at: 2_000,
            generation: 2,
            fatal_at: Some(3_000)
        })
    );
    // A fatal mark of a collection never asked makes its row without a time.
    store
        .write("fatal", |tx| fatal(tx, BLOCK, 3_100))
        .await
        .expect("fatal");
    assert_eq!(
        books(&store, BLOCK).await,
        Some(Book {
            asked_at: 0,
            generation: 0,
            fatal_at: Some(3_100)
        })
    );
    // The link: every named collection counts as asked at the link and loses its fatal mark; generations stay.
    store
        .write("link", |tx| stamp_link(tx, &[LOW, HIGH, BLOCK], 9_000))
        .await
        .expect("link");
    assert_eq!(
        books(&store, LOW).await,
        Some(Book {
            asked_at: 9_000,
            generation: 2,
            fatal_at: None
        })
    );
    assert_eq!(
        books(&store, HIGH).await,
        Some(Book {
            asked_at: 9_000,
            generation: 1,
            fatal_at: None
        })
    );
    assert_eq!(
        books(&store, BLOCK).await,
        Some(Book {
            asked_at: 9_000,
            generation: 0,
            fatal_at: None
        })
    );
    // A new map version clears every fatal mark and nothing else.
    store
        .write("fatal", |tx| fatal(tx, HIGH, 9_500))
        .await
        .expect("fatal");
    store
        .write("fatal", |tx| fatal(tx, LOW, 9_600))
        .await
        .expect("fatal");
    let cleared = store.write("clear", clear_fatal).await.expect("clear");
    assert_eq!(cleared, 2);
    assert_eq!(
        books(&store, HIGH).await,
        Some(Book {
            asked_at: 9_000,
            generation: 1,
            fatal_at: None
        })
    );
    assert_eq!(store.write("clear", clear_fatal).await.expect("clear"), 0);
    owner.close();
}

#[tokio::test]
async fn the_map_version_reads_zero_until_written_and_keeps_the_last() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert_eq!(store.read(map_version).await.expect("version"), 0);
    store
        .write("set", |tx| set_map_version(tx, 1))
        .await
        .expect("set");
    assert_eq!(store.read(map_version).await.expect("version"), 1);
    store
        .write("set", |tx| set_map_version(tx, 2))
        .await
        .expect("set");
    store
        .write("set", |tx| set_map_version(tx, 2))
        .await
        .expect("set");
    assert_eq!(
        store.read(map_version).await.expect("version"),
        2,
        "one row, the last word"
    );
    let rows: i64 = store
        .read(|c| {
            c.query_row("SELECT count(*) FROM appstate_map", [], |r| r.get(0))
                .map_err(crate::error::sql("count"))
        })
        .await
        .expect("count");
    assert_eq!(rows, 1);
    owner.close();
}
