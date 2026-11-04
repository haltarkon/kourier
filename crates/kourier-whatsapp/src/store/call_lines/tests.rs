//! The call-line bookkeeping (W6a1, A6): the first note of a call id stands, a lookup answers with its conversation, and
//! the rows go 30 days after they were written. Synthetic values only.

use kourier_db::DbKey;

use super::{KEEP_MS, conv_of, note, sweep};
use crate::store::{KourierStore, StoreOwner};

const NOW: i64 = 1_800_000_000_000;

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([13; 32]),
        "wa-call-lines",
    )
    .expect("open");
    (dir, owner)
}

async fn noted(store: &KourierStore, id: &'static str, conv: &'static str, now: i64) -> bool {
    store
        .write("test: note", move |tx| note(tx, id, conv, now - 5, now))
        .await
        .expect("note")
}

async fn found(store: &KourierStore, id: &'static str) -> Option<String> {
    store
        .read(move |conn| conv_of(conn, id))
        .await
        .expect("conv_of")
}

#[tokio::test]
async fn the_first_note_of_a_call_id_stands_and_a_lookup_answers_with_its_conversation() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert_eq!(found(&store, "CALLID-7731").await, None);
    assert!(noted(&store, "CALLID-7731", "direct:lid:70001", NOW).await);
    // The same call named again under another chat (the other namespace, or a hostile id): nothing moves.
    assert!(!noted(&store, "CALLID-7731", "direct:pn:70002", NOW + 10).await);
    assert_eq!(
        found(&store, "CALLID-7731").await.as_deref(),
        Some("direct:lid:70001")
    );
    assert_eq!(found(&store, "CALLID-7732").await, None, "another call id");
}

#[tokio::test]
async fn rows_older_than_thirty_days_go_at_a_sweep_and_younger_ones_stay() {
    let (_dir, owner) = open();
    let store = owner.store();
    noted(&store, "OLD-CALL-1", "direct:lid:70001", NOW - KEEP_MS - 1).await;
    noted(&store, "EDGE-CALL-1", "direct:lid:70001", NOW - KEEP_MS).await;
    noted(&store, "NEW-CALL-1", "direct:lid:70001", NOW - 1).await;
    let gone = store
        .write("test: sweep", |tx| sweep(tx, NOW))
        .await
        .expect("sweep");
    assert_eq!(gone, 1, "only the row older than the bound goes");
    assert_eq!(found(&store, "OLD-CALL-1").await, None);
    assert!(found(&store, "EDGE-CALL-1").await.is_some());
    assert!(found(&store, "NEW-CALL-1").await.is_some());
}
