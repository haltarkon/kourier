//! The notes of live messages: who is noted, what is counted up to a cut, what is forgotten.

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::DbKey;
use kourier_model::{Body, ConvKey, Origin, SystemEvent, UserId};

use super::{KEEP_MS, clear, count_up_to, horizon, prune, record, set_floor, tidy, wanted};
use crate::store::{KourierStore, StoreOwner};

fn user(n: u32) -> UserId {
    UserId::from_parts("pn", &format!("1555010011{n}")).expect("a user")
}

fn chat(n: u32) -> ConvKey {
    ConvKey::Direct(user(n))
}

fn message(conv: &ConvKey, origin: Origin, sent_at: i64, body: Body) -> BackendEvent {
    BackendEvent::Message(IncomingMessage::new(
        conv.clone(),
        format!("M{sent_at}"),
        origin,
        body,
        sent_at,
        sent_at,
    ))
}

fn theirs(conv: &ConvKey, sent_at: i64) -> BackendEvent {
    let author = conv_user(conv);
    message(
        conv,
        Origin::Remote {
            author,
            device: None,
        },
        sent_at,
        Body::text("hi"),
    )
}

fn conv_user(conv: &ConvKey) -> UserId {
    match conv {
        ConvKey::Direct(user) => user.clone(),
        _ => user(0),
    }
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([8; 32]),
        "wa-live",
    )
    .expect("open");
    (dir, owner)
}

async fn noted(store: &KourierStore, events: Vec<BackendEvent>) -> usize {
    store
        .write("note", move |tx| record(tx, &events))
        .await
        .expect("record")
}

async fn upto(store: &KourierStore, conv: &ConvKey, cut: i64) -> u32 {
    let conv = conv.encode();
    store
        .read(move |conn| count_up_to(conn, &conv, cut))
        .await
        .expect("count")
}

#[tokio::test]
async fn only_the_messages_of_others_are_noted_by_chat_and_counted_up_to_the_cut() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (a, b) = (chat(1), chat(2));
    let events = vec![
        theirs(&a, 1_000),
        theirs(&a, 2_000),
        theirs(&a, 3_000),
        theirs(&b, 1_500),
        // Ours (the phone's echo), a notice somebody else caused, and a call-less event: not unread.
        message(
            &a,
            Origin::OwnOtherDevice { device: None },
            1_200,
            Body::text("mine"),
        ),
        message(&a, Origin::OwnThisDevice, 1_300, Body::text("mine")),
        message(
            &a,
            Origin::Remote {
                author: user(1),
                device: None,
            },
            1_400,
            Body::System(SystemEvent::Joined),
        ),
        BackendEvent::ConversationUpsert(kourier_model::ConversationPatch::new(a.clone())),
    ];
    assert_eq!(noted(&store, events).await, 4);
    assert_eq!(
        upto(&store, &a, i64::MAX).await,
        3,
        "ours and the notice are not counted"
    );
    assert_eq!(upto(&store, &a, 2_000).await, 2, "the cut is inclusive");
    assert_eq!(upto(&store, &a, 999).await, 0);
    assert_eq!(upto(&store, &b, i64::MAX).await, 1, "by chat");
    assert_eq!(
        upto(&store, &chat(3), i64::MAX).await,
        0,
        "a chat nobody wrote in"
    );
    owner.close();
}

#[tokio::test]
async fn what_is_too_old_is_forgotten_and_the_end_of_the_push_forgets_all() {
    let (_dir, owner) = open();
    let store = owner.store();
    let a = chat(1);
    let now = 10 * KEEP_MS;
    let events = vec![
        theirs(&a, now - KEEP_MS - 1),
        theirs(&a, now - KEEP_MS),
        theirs(&a, now),
    ];
    assert_eq!(noted(&store, events).await, 3);
    let gone = store
        .write("prune", move |tx| prune(tx, now - KEEP_MS))
        .await
        .expect("prune");
    assert_eq!(gone, 1, "only the note older than the keeping time goes");
    assert_eq!(upto(&store, &a, i64::MAX).await, 2);
    let all = store.write("clear", clear).await.expect("clear");
    assert_eq!(all, 2);
    assert_eq!(upto(&store, &a, i64::MAX).await, 0);
    owner.close();
}

async fn is_wanted(store: &KourierStore) -> bool {
    store.read(wanted).await.expect("wanted")
}

async fn books(store: &KourierStore, statements: &'static str) {
    store
        .write("books", move |tx| {
            tx.execute_batch(statements)
                .map_err(crate::error::sql("books"))
        })
        .await
        .expect("books");
}

/// F4: a push can state a hint only while a chat owes an end, a chunk waits (the floor), or an announced chunk is
/// unresolved; with none of them the live path notes nothing.
/// Mutant for the re-check: `wanted` answering true.
#[tokio::test]
async fn a_push_is_open_only_while_something_of_it_can_still_come() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert!(!is_wanted(&store).await, "no push: nothing to note");

    books(
        &store,
        "INSERT INTO history_chats(conv, more) VALUES ('c', 1);",
    )
    .await;
    assert!(is_wanted(&store).await, "a chat owes its end");
    books(&store, "UPDATE history_chats SET more = 0;").await;
    assert!(!is_wanted(&store).await);

    books(
        &store,
        "INSERT INTO history_announced(msg_id, announced_at) VALUES ('n1', 1);",
    )
    .await;
    assert!(is_wanted(&store).await, "an announced chunk is unresolved");
    books(&store, "INSERT INTO history_chunks(chunk) VALUES (1);").await;
    assert!(
        !is_wanted(&store).await,
        "its chunk was delivered: nothing is unresolved"
    );
    books(
        &store,
        "UPDATE history_announced SET lost = 1; DELETE FROM history_chunks;",
    )
    .await;
    assert!(
        !is_wanted(&store).await,
        "a lost chunk is no longer awaited"
    );

    store
        .write("floor", |tx| set_floor(tx, Some(5)))
        .await
        .expect("floor");
    assert!(is_wanted(&store).await, "a chunk waits for its delivery");
    store
        .write("floor", |tx| set_floor(tx, None))
        .await
        .expect("floor");
    assert!(!is_wanted(&store).await);
    owner.close();
}

/// F5: the notes are kept for a day BEFORE the oldest chunk that waits for its delivery, whatever the time is now; with
/// no chunk waiting, a day before now (the old rule).
/// Mutant for the re-check: `horizon` ignoring the floor.
#[tokio::test]
async fn the_notes_are_kept_from_a_day_before_the_oldest_chunk_that_waits() {
    let (_dir, owner) = open();
    let store = owner.store();
    let a = chat(1);
    let captured = 100 * KEEP_MS;
    let two_days_later = captured + 2 * KEEP_MS;
    let events = vec![
        theirs(&a, captured - KEEP_MS - 1),
        theirs(&a, captured - KEEP_MS),
        theirs(&a, captured - 1_000),
    ];
    assert_eq!(noted(&store, events).await, 3);

    // Nothing waits: a day before now, so all three go two days later.
    let none = store
        .read(move |conn| horizon(conn, two_days_later))
        .await
        .expect("horizon");
    assert_eq!(none, two_days_later - KEEP_MS);

    // A chunk captured at `captured` waits: the horizon is a day before IT, two days later too.
    store
        .write("floor", move |tx| set_floor(tx, Some(captured)))
        .await
        .expect("floor");
    let waiting = store
        .read(move |conn| horizon(conn, two_days_later))
        .await
        .expect("horizon");
    assert_eq!(waiting, captured - KEEP_MS);
    let gone = store
        .write("tidy", move |tx| tidy(tx, two_days_later))
        .await
        .expect("tidy");
    assert_eq!(
        gone, 1,
        "only the note older than a day before the chunk goes"
    );
    assert_eq!(
        upto(&store, &a, i64::MAX).await,
        2,
        "a delivery resumed two days later still subtracts these"
    );
    owner.close();
}
