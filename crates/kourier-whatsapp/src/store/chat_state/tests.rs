//! The mirror of the phone's chat settings (W6a1, A2): a put answers "changed" only for a value the core does not
//! have, stamps its generation every time, keeps a value for a chat the core does not know until it is stated (once),
//! and the pin list is the stated pins, newest first. Synthetic ids only (tails of 8 digits that are substrings of no
//! other value).

use kourier_db::DbKey;
use kourier_model::{ConvKey, UserId};

use super::{
    DEFAULT, Entry, Field, Put, UNARCHIVE_CHATS, Written, account_flag, get, mark_stated, pinned,
    put, set_account_flag, to_state,
};
use crate::store::{KourierStore, StoreOwner};

const ONE: &str = "77700481537209";
const TWO: &str = "77700592640318";
const THREE: &str = "77700615724803";

fn chat(lid: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts("lid", lid).expect("a user"))
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([11; 32]),
        "wa-chat-state",
    )
    .expect("open");
    (dir, owner)
}

/// One value of the phone for `chat`: (field, value, action_ms, generation, collection, stated).
type Say = (Field, i64, i64, i64, &'static str, bool);

async fn say(
    store: &KourierStore,
    chat: &ConvKey,
    (field, value, action_ms, generation, collection, stated): Say,
) -> Written {
    let chat = chat.clone();
    store
        .write("put", move |tx| {
            put(
                tx,
                &Put {
                    chat: &chat,
                    field,
                    value,
                    action_ms,
                    generation,
                    collection,
                    stated,
                },
            )
        })
        .await
        .expect("put")
}

async fn row(store: &KourierStore, chat: &ConvKey, field: Field) -> Option<Entry> {
    let chat = chat.clone();
    store
        .read(move |conn| get(conn, &chat, field))
        .await
        .expect("get")
}

async fn waiting(store: &KourierStore, chat: &ConvKey) -> Vec<Entry> {
    let chat = chat.clone();
    store
        .read(move |conn| to_state(conn, &chat))
        .await
        .expect("to_state")
}

async fn pins(store: &KourierStore) -> Vec<ConvKey> {
    store.read(pinned).await.expect("pinned")
}

const LOW: &str = "regular_low";
const HIGH: &str = "regular_high";

#[tokio::test]
async fn a_put_says_changed_only_for_a_value_the_core_does_not_have() {
    let (_dir, owner) = open();
    let store = owner.store();
    let c = chat(ONE);
    // A chat never archived, "unarchived": the default stays the default.
    assert!(
        !say(&store, &c, (Field::Archived, 0, 10, 0, LOW, true))
            .await
            .changed
    );
    assert!(
        say(&store, &c, (Field::Archived, 1, 20, 0, LOW, true))
            .await
            .changed,
        "archived"
    );
    assert!(
        !say(&store, &c, (Field::Archived, 1, 30, 0, LOW, true))
            .await
            .changed,
        "the same again (a replay)"
    );
    assert!(
        say(&store, &c, (Field::Archived, 0, 40, 0, LOW, true))
            .await
            .changed,
        "unarchived"
    );
    // A mute: for ever is a value, the same again is not, a shorter one is another.
    assert!(
        say(&store, &c, (Field::Muted, i64::MAX, 50, 0, HIGH, true))
            .await
            .changed
    );
    assert!(
        !say(&store, &c, (Field::Muted, i64::MAX, 60, 0, HIGH, true))
            .await
            .changed
    );
    assert!(
        say(
            &store,
            &c,
            (Field::Muted, 1_760_000_000_000, 70, 0, HIGH, true)
        )
        .await
        .changed
    );
    // The fields of one chat do not hide each other.
    assert!(
        say(&store, &c, (Field::MarkedUnread, 1, 80, 0, LOW, true))
            .await
            .changed
    );
    // The account's setting answers the same way.
    let flag = |value: bool| {
        store.write("flag", move |tx| {
            set_account_flag(tx, UNARCHIVE_CHATS, value)
        })
    };
    assert_eq!(
        store
            .read(|c| account_flag(c, UNARCHIVE_CHATS))
            .await
            .expect("flag"),
        None,
        "the phone said nothing yet"
    );
    assert!(flag(true).await.expect("set"), "new");
    assert!(!flag(true).await.expect("set"), "the same");
    assert!(flag(false).await.expect("set"), "the other way");
    assert_eq!(
        store
            .read(|c| account_flag(c, UNARCHIVE_CHATS))
            .await
            .expect("flag"),
        Some(false)
    );
    owner.close();
}

#[tokio::test]
async fn a_put_stamps_the_generation_the_collection_and_the_time_every_time() {
    let (_dir, owner) = open();
    let store = owner.store();
    let c = chat(ONE);
    let first = say(&store, &c, (Field::Pinned, 1, 100, 3, LOW, true)).await;
    assert_eq!(
        first,
        Written {
            changed: true,
            retimed: false
        }
    );
    assert_eq!(
        row(&store, &c, Field::Pinned).await,
        Some(Entry {
            field: Field::Pinned,
            value: 1,
            action_ms: 100,
            generation: 3,
            collection: LOW.into(),
            stated: true,
        })
    );
    // The same value again in a later generation (a replay): not changed, but a touch, and the pin order moved.
    let again = say(&store, &c, (Field::Pinned, 1, 200, 4, LOW, true)).await;
    assert_eq!(
        again,
        Written {
            changed: false,
            retimed: true
        }
    );
    let e = row(&store, &c, Field::Pinned).await.expect("row");
    assert_eq!(
        (e.generation, e.action_ms),
        (4, 200),
        "the replay's touch is kept"
    );
    // And again with the same time: the generation still moves, the order does not.
    let same = say(&store, &c, (Field::Pinned, 1, 200, 5, LOW, true)).await;
    assert_eq!(
        same,
        Written {
            changed: false,
            retimed: false
        }
    );
    assert_eq!(
        row(&store, &c, Field::Pinned)
            .await
            .expect("row")
            .generation,
        5
    );
    // Another collection and another field are rows of their own.
    say(&store, &c, (Field::Muted, 0, 300, 7, HIGH, true)).await;
    let m = row(&store, &c, Field::Muted).await.expect("mute row");
    assert_eq!(
        (m.generation, m.collection.as_str(), m.value),
        (7, HIGH, DEFAULT)
    );
    assert_eq!(
        row(&store, &c, Field::Pinned)
            .await
            .expect("row")
            .generation,
        5
    );
    assert_eq!(
        row(&store, &chat(TWO), Field::Pinned).await,
        None,
        "another chat has none"
    );
    owner.close();
}

#[tokio::test]
async fn a_value_for_an_unknown_chat_waits_unstated_and_is_stated_once() {
    let (_dir, owner) = open();
    let store = owner.store();
    let c = chat(ONE);
    // The core does not know the chat: the mirror keeps the values and nobody is told (H5).
    assert!(
        !say(&store, &c, (Field::Pinned, 1, 500, 1, LOW, false))
            .await
            .changed
    );
    assert!(
        !say(&store, &c, (Field::Muted, i64::MAX, 510, 1, HIGH, false))
            .await
            .changed
    );
    // A default for an unknown chat is a row too, but there is nothing to tell.
    assert!(
        !say(&store, &c, (Field::Archived, 0, 520, 1, LOW, false))
            .await
            .changed
    );
    assert!(!row(&store, &c, Field::Pinned).await.expect("row").stated);
    let waiting_now = waiting(&store, &c).await;
    assert_eq!(
        waiting_now
            .iter()
            .map(|e| (e.field, e.value))
            .collect::<Vec<_>>(),
        vec![(Field::Muted, i64::MAX), (Field::Pinned, 1)],
        "what the first statement of the chat writes: the non-default rows, in field order"
    );
    assert_eq!(
        pins(&store).await,
        Vec::<ConvKey>::new(),
        "an unknown chat's pin is not in the list"
    );
    // The chat becomes known: the next word of the phone about a value the core never heard is a change, though the
    // mirror already held it.
    assert!(
        say(&store, &c, (Field::Pinned, 1, 500, 1, LOW, true))
            .await
            .changed
    );
    // The statement of the rest flips once.
    let cc = c.clone();
    let flipped = store
        .write("state", move |tx| mark_stated(tx, &cc))
        .await
        .expect("state");
    assert_eq!(
        flipped, 2,
        "the mute and the default archive row (the pin was stated by its put)"
    );
    let cc = c.clone();
    let again = store
        .write("state", move |tx| mark_stated(tx, &cc))
        .await
        .expect("state");
    assert_eq!(again, 0, "stated once");
    assert!(waiting(&store, &c).await.is_empty());
    assert_eq!(pins(&store).await, vec![c.clone()]);
    // A put that does not know the chat never takes a statement back.
    say(&store, &c, (Field::Muted, 0, 600, 2, HIGH, false)).await;
    assert!(row(&store, &c, Field::Muted).await.expect("row").stated);
    owner.close();
}

#[tokio::test]
async fn the_pin_list_is_the_stated_pins_newest_first() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (a, b, c, d, e) = (
        chat(ONE),
        chat(TWO),
        chat(THREE),
        chat("77700726358941"),
        chat("77700837461052"),
    );
    say(&store, &a, (Field::Pinned, 1, 300, 1, LOW, true)).await;
    say(&store, &b, (Field::Pinned, 1, 100, 1, LOW, true)).await;
    say(&store, &c, (Field::Pinned, 1, 200, 1, LOW, true)).await;
    // Not in the list: an unknown chat's pin, an unpinned chat, a chat that is only archived.
    say(&store, &d, (Field::Pinned, 1, 900, 1, LOW, false)).await;
    say(&store, &e, (Field::Pinned, 0, 800, 1, LOW, true)).await;
    say(&store, &e, (Field::Archived, 1, 810, 1, LOW, true)).await;
    assert_eq!(pins(&store).await, vec![a.clone(), c.clone(), b.clone()]);
    // Pinned again later: the chat moves to the top.
    say(&store, &b, (Field::Pinned, 1, 400, 2, LOW, true)).await;
    assert_eq!(pins(&store).await, vec![b.clone(), a.clone(), c.clone()]);
    // Unpinned: gone. Equal times order by the key, whatever the storage order.
    say(&store, &a, (Field::Pinned, 0, 500, 2, LOW, true)).await;
    say(&store, &c, (Field::Pinned, 1, 400, 2, LOW, true)).await;
    let mut tied = vec![b.clone(), c.clone()];
    tied.sort_by_key(ConvKey::encode);
    assert_eq!(pins(&store).await, tied);
    owner.close();
}
