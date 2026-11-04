//! The people books (W6a2, B3): the blocklist's rows and the forgetting of a `Rebuild`. Synthetic values only.

use kourier_db::DbKey;
use kourier_db::rusqlite::params;

use kourier_model::{ConvKey, UserId};

use super::{
    NameKind, blocked, blocklist_read_at, dirty, drop_blocked, forget_blocklist_stamp,
    forget_statements, group, put_blocked, put_group, put_name, received_as, stamp_blocklist,
};
use crate::groups::{Rules, State};
use crate::store::{KourierStore, StoreOwner};

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([14; 32]),
        "wa-people-books",
    )
    .expect("open");
    (dir, owner)
}

async fn put(store: &KourierStore, person: &'static str) -> bool {
    store
        .write("test: put", move |tx| put_blocked(tx, person))
        .await
        .expect("put")
}

async fn drop_one(store: &KourierStore, person: &'static str) -> bool {
    store
        .write("test: drop", move |tx| drop_blocked(tx, person))
        .await
        .expect("drop")
}

#[tokio::test]
async fn a_blocked_person_is_noted_once_listed_in_order_and_dropped_once() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert!(blocked_ids(&store).await.is_empty());
    assert!(put(&store, "pn:15550170422").await);
    assert!(put(&store, "lid:99900170411").await);
    assert!(
        !put(&store, "pn:15550170422").await,
        "the same person again is no new row"
    );
    assert_eq!(
        blocked_ids(&store).await,
        ["lid:99900170411", "pn:15550170422"]
    );
    assert!(drop_one(&store, "lid:99900170411").await);
    assert!(!drop_one(&store, "lid:99900170411").await);
    assert_eq!(blocked_ids(&store).await, ["pn:15550170422"]);
}

async fn blocked_ids(store: &KourierStore) -> Vec<String> {
    store.read(blocked).await.expect("blocked")
}

/// A `Rebuild` made the core again: none of what it was told is true. Every one of the four books goes, in the one zone; a
/// group's row stays (fix F15, DUR-1: it is also the server's word) and loses only the two flags that say "the core was
/// told". Mutant: the row deleted again.
#[tokio::test]
async fn a_rebuild_forgets_what_the_core_was_told_of_groups_merges_names_and_the_blocklist() {
    let (_dir, owner) = open();
    let store = owner.store();
    let forgotten = store
        .write("test: plant", |tx| {
            tx.execute(
                "INSERT INTO group_state(conv, members_stated, subject_time, rules, truncated, not_member, parent, parent_stated,
                                        dirty_at, fetched_at)
                 VALUES (?1, 1, 1700000100, 9, 1, 1, ?2, 1, 1800000000000, 1800000001000)",
                params!["group:wag:120363000071529611", "group:wag:120363000071529612"],
            )
            .expect("group");
            tx.execute(
                "INSERT INTO merges_stated(pn, lid) VALUES (?1, ?2)",
                params!["pn:15550170422", "lid:99900170411"],
            )
            .expect("merge");
            tx.execute(
                "INSERT INTO names_stated(person, kind, value) VALUES (?1, 'profile', ?2)",
                params!["pn:15550170422", "A Name"],
            )
            .expect("name");
            // The address book's name and number: the mirror states them again, so they go (the push name stays, F21).
            tx.execute(
                "INSERT INTO names_stated(person, kind, value) VALUES (?1, 'system', ?2), (?1, 'number', ?3)",
                params!["pn:15550170422", "A Contact", "+15550170422"],
            )
            .expect("contact names");
            put_blocked(tx, "pn:15550170422")?;
            // And the stamp of the blocklist's last answer (Q1): a rebuilt core is told the list anew.
            stamp_blocklist(tx, 1_800_000_000_000)?;
            forget_statements(tx)
        })
        .await
        .expect("forget");
    assert_eq!(forgotten, 7);
    for table in ["merges_stated", "blocked_stated", "blocklist_read"] {
        let left: i64 = store
            .read(move |conn| {
                Ok(conn
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                    .expect("count"))
            })
            .await
            .expect("read");
        assert_eq!(left, 0, "{table} still holds a row");
    }
    // The push name stays, marked not stated (F21); the address book's name and number went.
    let names: Vec<(String, i64)> = store
        .read(|conn| {
            let mut ask = conn
                .prepare("SELECT kind, stated FROM names_stated ORDER BY kind")
                .expect("prepare");
            Ok(ask
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .expect("query")
                .collect::<Result<Vec<_>, _>>()
                .expect("rows"))
        })
        .await
        .expect("names");
    assert_eq!(names, [("profile".to_owned(), 0)]);
    // The group: the server's facts stay, the two "told" flags and the title's time go.
    let key = ConvKey::Group("wag:120363000071529611".into());
    let state = store
        .read(move |conn| group(conn, &key))
        .await
        .expect("group");
    assert!(!state.members_stated, "the new core holds no member list");
    assert!(!state.parent_stated, "the new core was told no parent");
    assert!(state.not_member, "a group we left stays left");
    assert_eq!(state.rules.map(Rules::bits), Some(9));
    assert!(state.truncated);
    assert_eq!(
        state.subject_time, None,
        "the new core was told no title, so its time goes (round 2, G3)"
    );
    assert_eq!(state.fetched_at, Some(1_800_000_001_000));
    assert!(state.parent.is_some(), "the community the fetch named");
    let marked = store.read(dirty).await.expect("dirty");
    assert_eq!(marked.len(), 1, "a fetch owed stays owed");
}

fn group_key(user: &str) -> ConvKey {
    ConvKey::Group(format!("wag:{user}").into())
}

/// A group nobody answered about has the default row; a row comes back as it was written, each group has its own, a
/// second write replaces the first and leaves the fetch policy's `dirty_at` alone.
#[tokio::test]
async fn a_groups_row_is_kept_as_written_and_replaced_whole() {
    let (_dir, owner) = open();
    let store = owner.store();
    let one = group_key("120363000000000071");
    let two = group_key("120363000000000072");
    let read = |key: ConvKey| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| group(conn, &key))
                .await
                .expect("read")
        }
    };
    assert_eq!(read(one.clone()).await, State::default());

    let row = State {
        members_stated: true,
        subject_time: Some(1_700_000_100),
        timer: Some(86_400),
        timer_version: Some(1_800_000_000),
        rules: Some(Rules {
            announcements_only: true,
            own_admin: true,
            ..Rules::default()
        }),
        truncated: false,
        not_member: false,
        fetched_at: Some(1_800_000_000_000),
        parent: Some(group_key("120363000000000099")),
        parent_stated: true,
    };
    let (key, written) = (one.clone(), row.clone());
    store
        .write("test: put", move |tx| {
            put_group(tx, &key, &written)?;
            // The fetch policy's mark (B6) is written by another function; this one must not lose it.
            tx.execute(
                "UPDATE group_state SET dirty_at = 7 WHERE conv = ?1",
                params![key.encode()],
            )
            .expect("mark");
            Ok(())
        })
        .await
        .expect("write");
    assert_eq!(read(one.clone()).await, row);
    assert_eq!(
        read(two).await,
        State::default(),
        "another group has its own row"
    );

    let later = State {
        truncated: true,
        not_member: true,
        rules: None,
        timer: None,
        ..row
    };
    let (key, written) = (one.clone(), later.clone());
    store
        .write("test: replace", move |tx| put_group(tx, &key, &written))
        .await
        .expect("replace");
    assert_eq!(read(one.clone()).await, later);
    let dirty: Option<i64> = store
        .read(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT dirty_at FROM group_state WHERE conv = ?1",
                    params![one.encode()],
                    |r| r.get(0),
                )
                .expect("dirty"))
        })
        .await
        .expect("read");
    assert_eq!(dirty, Some(7), "a replaced row keeps the mark");
}

/// A group marked for a fetch keeps the time of the FIRST ask, whatever asked later; the marks come oldest first and a
/// group with no mark is not listed (W6a2, B5).
/// Mutant: the mark overwritten by a later ask.
#[tokio::test]
async fn a_group_mark_keeps_its_first_time_and_lists_oldest_first() {
    let (_dir, owner) = open();
    let store = owner.store();
    let a = ConvKey::Group("wag:120363000071529611".into());
    let b = ConvKey::Group("wag:120363000071529622".into());
    let mark = |conv: ConvKey, at| {
        let store = store.clone();
        async move {
            store
                .write("test: mark", move |tx| super::mark_dirty(tx, &conv, at))
                .await
                .expect("mark");
        }
    };
    mark(b.clone(), 50).await;
    mark(a.clone(), 100).await;
    mark(b.clone(), 900).await;
    assert_eq!(
        store.read(super::dirty).await.expect("marks"),
        vec![(b.clone(), 50), (a.clone(), 100)]
    );
    // A mark makes the row with the defaults and touches nothing else a row holds.
    store
        .write("test: row", {
            let a = a.clone();
            move |tx| {
                put_group(
                    tx,
                    &a,
                    &State {
                        members_stated: true,
                        ..State::default()
                    },
                )
            }
        })
        .await
        .expect("row");
    let row = store.read(move |conn| group(conn, &a)).await.expect("row");
    assert!(row.members_stated, "putting the row leaves the mark alone");
    assert_eq!(store.read(super::dirty).await.expect("marks").len(), 2);
}

/// B6: an answer clears the mark of the group it covers and no other; the truncated groups are listed for their single
/// query. Kills "clearing a mark clears every mark".
#[tokio::test]
async fn an_answer_clears_only_its_groups_mark_and_the_truncated_groups_are_listed() {
    let (_dir, owner) = open();
    let store = owner.store();
    let a = ConvKey::Group("wag:120363000071529611".into());
    let b = ConvKey::Group("wag:120363000071529622".into());
    let c = ConvKey::Group("wag:120363000071529633".into());
    store
        .write("test: marks", {
            let (a, b) = (a.clone(), b.clone());
            move |tx| {
                super::mark_dirty(tx, &a, 10)?;
                super::mark_dirty(tx, &b, 20)?;
                put_group(
                    tx,
                    &ConvKey::Group("wag:120363000071529633".into()),
                    &State {
                        truncated: true,
                        ..State::default()
                    },
                )
            }
        })
        .await
        .expect("seed");
    let cleared = store
        .write("test: clear", {
            let a = a.clone();
            move |tx| super::clear_dirty(tx, &a, 10)
        })
        .await
        .expect("clear");
    assert!(cleared);
    assert_eq!(
        store.read(super::dirty).await.expect("marks"),
        vec![(b.clone(), 20)]
    );
    let again = store
        .write("test: clear again", move |tx| {
            super::clear_dirty(tx, &a, 10)
        })
        .await
        .expect("clear");
    assert!(!again, "nothing to clear the second time");
    // A mark newer than the ask is not covered by its answer.
    let newer = store
        .write("test: a newer mark", {
            let b = b.clone();
            move |tx| super::clear_dirty(tx, &b, 19)
        })
        .await
        .expect("clear");
    assert!(!newer, "the mark of 20 is newer than an ask at 19");
    assert_eq!(
        store.read(super::dirty).await.expect("marks"),
        vec![(b, 20)]
    );
    assert_eq!(store.read(super::truncated).await.expect("list"), vec![c]);
}

/// Fix F16 (DUR-2): a group asked for again while its mark stands keeps the first time (the order) and takes the NEWEST (what an
/// answer must not clear): an answer asked at 15 does not cover the mark that was made again at 20, one asked at 20 does.
/// Mutant: `clear_dirty` compares the first ask's time.
#[tokio::test]
async fn a_mark_made_again_keeps_both_times_and_an_older_answer_does_not_clear_it() {
    let (_dir, owner) = open();
    let store = owner.store();
    let a = ConvKey::Group("wag:120363000071529611".into());
    let mark = |at: i64| {
        let (store, a) = (store.clone(), a.clone());
        async move {
            store
                .write("test: mark", move |tx| super::mark_dirty(tx, &a, at))
                .await
                .expect("mark");
        }
    };
    mark(10).await;
    mark(20).await;
    mark(15).await;
    assert_eq!(
        store.read(super::dirty).await.expect("marks"),
        vec![(a.clone(), 10)]
    );
    assert_eq!(
        store.read(super::dirty_last).await.expect("marks"),
        vec![(a.clone(), 20)],
        "the newest ask, whatever order the writes came in"
    );
    let clear = |asked: i64| {
        let (store, a) = (store.clone(), a.clone());
        async move {
            store
                .write("test: clear", move |tx| super::clear_dirty(tx, &a, asked))
                .await
                .expect("clear")
        }
    };
    assert!(
        !clear(15).await,
        "the answer was computed before the mark of 20"
    );
    assert_eq!(store.read(super::dirty).await.expect("marks").len(), 1);
    assert!(clear(20).await);
    assert!(store.read(super::dirty).await.expect("marks").is_empty());
    // A cleared mark starts over: the next ask is both the first and the newest.
    mark(40).await;
    assert_eq!(
        store.read(super::dirty_last).await.expect("marks"),
        vec![(a.clone(), 40)]
    );
    assert_eq!(
        store.read(super::dirty).await.expect("marks"),
        vec![(a, 40)]
    );
}

/// The blocklist's stamp: none before an answer, the time of the last one after it (a second stamp replaces the first), and
/// the link's forgetting takes it away. Kills "a second stamp keeps the first" and "the link keeps the stamp".
#[tokio::test]
async fn the_blocklists_stamp_is_the_last_answers_time_and_a_link_forgets_it() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert_eq!(store.read(blocklist_read_at).await.expect("read"), None);
    store
        .write("test: stamp", |tx| stamp_blocklist(tx, 1_000))
        .await
        .expect("stamp");
    store
        .write("test: stamp again", |tx| stamp_blocklist(tx, 2_000))
        .await
        .expect("stamp");
    assert_eq!(
        store.read(blocklist_read_at).await.expect("read"),
        Some(2_000)
    );
    store
        .write("test: link", forget_blocklist_stamp)
        .await
        .expect("forget");
    assert_eq!(store.read(blocklist_read_at).await.expect("read"), None);
}

#[tokio::test]
async fn a_name_is_a_change_only_when_the_kind_or_the_value_differs() {
    let (_dir, owner) = open();
    let store = owner.store();
    let put = |person: &'static str, kind: NameKind, value: &'static str| {
        store.write("test: put a name", move |tx| {
            put_name(tx, person, kind, value)
        })
    };
    assert!(
        put("pn:15550170431", NameKind::Profile, "Ann Example")
            .await
            .expect("put")
    );
    assert!(
        !put("pn:15550170431", NameKind::Profile, "Ann Example")
            .await
            .expect("put"),
        "the same name of the same kind is no change"
    );
    assert!(
        put("pn:15550170431", NameKind::System, "Ann Example")
            .await
            .expect("put"),
        "the address book's name is another row"
    );
    assert!(
        put("pn:15550170431", NameKind::Profile, "Ann Renamed")
            .await
            .expect("put")
    );
    assert!(
        put("lid:99900170431", NameKind::Profile, "Ann Renamed")
            .await
            .expect("put")
    );
    let rows: i64 = store
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM names_stated", [], |r| r.get(0))
                .map_err(crate::error::sql("count names"))
        })
        .await
        .expect("count");
    assert_eq!(rows, 3, "a row per person and kind");
}

/// Fix F3: the form a received group message's author was named in is the person of the ledger row's sender, when that
/// person is the author or his paired id; a row of another member with the same id, a row of another group, a sender with
/// a device suffix (still the same person), a hosted id and an author nobody paired all behave as the doc says.
/// Mutants: the pair not accepted; the id not compared (any row of the chat).
#[tokio::test]
async fn the_form_a_message_arrived_under_is_found_in_the_ledger_for_the_same_person_only() {
    let (_dir, owner) = open();
    let store = owner.store();
    let group = ConvKey::Group("wag:120363000071529611".into());
    let other_group = ConvKey::Group("wag:120363000071529622".into());
    let lid = UserId::from_parts("lid", "99000071501").unwrap();
    let pn = UserId::from_parts("pn", "31000071502").unwrap();
    let stranger_lid = UserId::from_parts("lid", "99000071503").unwrap();
    store
        .write("test: the ledger", |tx| {
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES ('99000071501', '31000071502', 1, 1, 'test')",
                [],
            )
            .map_err(crate::error::sql("test: pair"))?;
            for (chat, sender, id) in [
                (
                    "120363000071529611@g.us",
                    "31000071502:3@s.whatsapp.net",
                    "ID1",
                ),
                ("120363000071529611@g.us", "99000071599@lid", "ID1"),
                ("120363000071529611@g.us", "99000071503@lid", "ID2"),
                ("120363000071529622@g.us", "99000071501@lid", "ID3"),
            ] {
                tx.execute(
                    "INSERT INTO inbox(chat, sender, id, origin, received_at, catch_up, state) \
                     VALUES (?1, ?2, ?3, 'e2e', 1, 0, 'done')",
                    params![chat, sender, id],
                )
                .map_err(crate::error::sql("test: ledger row"))?;
            }
            Ok(())
        })
        .await
        .expect("seed");
    let ask = |conv: &ConvKey, local: &'static str, author: &UserId| {
        let (conv, author) = (conv.clone(), author.clone());
        let store = store.clone();
        async move {
            store
                .read(move |conn| received_as(conn, &conv, local, &author))
                .await
                .expect("read")
        }
    };
    assert_eq!(
        ask(&group, "ID1", &lid).await,
        Some(pn.clone()),
        "the core holds his LID, the message arrived by number (device dropped)"
    );
    assert_eq!(
        ask(&group, "ID1", &pn).await,
        Some(pn.clone()),
        "the same person, the same form"
    );
    assert_eq!(
        ask(&group, "ID1", &stranger_lid).await,
        None,
        "another member's row with the same id is not his"
    );
    assert_eq!(
        ask(&group, "ID2", &stranger_lid).await,
        Some(stranger_lid.clone())
    );
    assert_eq!(ask(&group, "ID9", &lid).await, None, "no row for the id");
    assert_eq!(
        ask(&group, "ID3", &lid).await,
        None,
        "a row of another group does not count"
    );
    assert_eq!(ask(&other_group, "ID3", &lid).await, Some(lid.clone()));
}
