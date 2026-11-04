//! The merge sweep's reads and its one write (W6a2, B9). Synthetic values only.

use kourier_db::DbKey;
use kourier_db::rusqlite::params;

use super::{book_pairs, crosses_a_stated_pair, currents, either_known, put_stated};
use crate::store::{KourierStore, StoreOwner, book, exec};

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([18; 32]),
        "wa-merge-rows",
    )
    .expect("open");
    (dir, owner)
}

/// Rows of `lid_pn_mapping` as `(lid, phone, updated_at, source)`, in insertion order.
async fn plant(store: &KourierStore, rows: &[(&str, &str, i64, &str)]) {
    let rows: Vec<(String, String, i64, String)> = rows
        .iter()
        .map(|(lid, phone, at, source)| {
            (
                (*lid).to_owned(),
                (*phone).to_owned(),
                *at,
                (*source).to_owned(),
            )
        })
        .collect();
    store
        .write("test: plant mappings", move |tx| {
            for (lid, phone, at, source) in &rows {
                exec(
                    tx,
                    "test",
                    "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source)
                     VALUES (?1, ?2, 1, ?3, ?4)",
                    params![lid, phone, at, source],
                )?;
            }
            Ok(())
        })
        .await
        .expect("plant");
}

async fn know(store: &KourierStore, people: &[&str]) {
    let people: Vec<String> = people.iter().map(|p| (*p).to_owned()).collect();
    store
        .write("test: know", move |tx| {
            for person in &people {
                book::know(tx, person)?;
            }
            Ok(())
        })
        .await
        .expect("know");
}

async fn stated(store: &KourierStore, pn: &str, lid: &str) -> bool {
    let (pn, lid) = (pn.to_owned(), lid.to_owned());
    store
        .write("test: stated", move |tx| put_stated(tx, &pn, &lid))
        .await
        .expect("stated")
}

fn pairs_of(rows: &[super::Current]) -> Vec<(String, String)> {
    rows.iter().map(|c| (c.pn.clone(), c.lid.clone())).collect()
}

/// A number's CURRENT LID is its newest row (ties: the later row), whatever the source; a pair already stated is not
/// listed, and a number nobody the core knows IS (the scope is the sweep's, applied after `decide`: W6a2 PRIV-3).
#[tokio::test]
async fn the_current_row_of_a_number_is_its_newest_and_every_unstated_one_is_listed() {
    let (_dir, owner) = open();
    let store = owner.store();
    plant(
        &store,
        &[
            // 555-0101 got a new LID later: only the newer one is current.
            ("99900481101", "15550481101", 10, "usync"),
            ("99900481102", "15550481101", 20, "usync"),
            // A tie in the time: the row written later is current.
            ("99900481103", "15550481103", 30, "usync"),
            ("99900481104", "15550481103", 30, "recipient_latest_lid"),
            // A number the core never heard of: listed (the sweep's scope keeps it out of core.db, after `decide`).
            ("99900481105", "15550481105", 40, "usync"),
            // Known by its LID only.
            ("99900481106", "15550481106", 50, "blocklist_active"),
            // Already told.
            ("99900481107", "15550481107", 60, "usync"),
        ],
    )
    .await;
    know(
        &store,
        &[
            "pn:15550481101",
            "pn:15550481103",
            "lid:99900481106",
            "pn:15550481107",
        ],
    )
    .await;
    assert!(stated(&store, "pn:15550481107", "lid:99900481107").await);

    let listed = store.read(currents).await.expect("currents");
    assert_eq!(
        pairs_of(&listed),
        [
            ("pn:15550481101".to_owned(), "lid:99900481102".to_owned()),
            ("pn:15550481103".to_owned(), "lid:99900481104".to_owned()),
            ("pn:15550481105".to_owned(), "lid:99900481105".to_owned()),
            ("pn:15550481106".to_owned(), "lid:99900481106".to_owned()),
        ]
    );
    assert_eq!(listed[1].source, "recipient_latest_lid");
}

/// The address book names a pair only with both ids; a stated pair is left out and a person the core does not know is
/// NOT (the scope is the sweep's, after `decide`: W6a2 PRIV-3); a pair two contacts name is listed once.
#[tokio::test]
async fn the_address_books_pairs_need_both_ids_and_no_statement() {
    let (_dir, owner) = open();
    let store = owner.store();
    store
        .write("test: the book", |tx| {
            for (contact, pn, lid) in [
                (
                    "pn:15550481201",
                    Some("pn:15550481201"),
                    Some("lid:99900481201"),
                ),
                (
                    "lid:99900481201",
                    Some("pn:15550481201"),
                    Some("lid:99900481201"),
                ),
                ("pn:15550481202", Some("pn:15550481202"), None),
                ("lid:99900481203", None, Some("lid:99900481203")),
                (
                    "pn:15550481204",
                    Some("pn:15550481204"),
                    Some("lid:99900481204"),
                ),
                (
                    "pn:15550481205",
                    Some("pn:15550481205"),
                    Some("lid:99900481205"),
                ),
            ] {
                book::put(
                    tx,
                    &book::Row {
                        contact: contact.to_owned(),
                        name: "Synthetic".to_owned(),
                        pn: pn.map(str::to_owned),
                        lid: lid.map(str::to_owned),
                    },
                )?;
            }
            Ok(())
        })
        .await
        .expect("book");
    // 1201 is known by his LID, 1204 by nobody (still listed), 1205 is known but told already.
    know(
        &store,
        &["lid:99900481201", "pn:15550481202", "pn:15550481205"],
    )
    .await;
    assert!(stated(&store, "pn:15550481205", "lid:99900481205").await);

    let pairs = store.read(book_pairs).await.expect("pairs");
    assert_eq!(
        pairs,
        [
            ("pn:15550481201".to_owned(), "lid:99900481201".to_owned()),
            ("pn:15550481204".to_owned(), "lid:99900481204".to_owned()),
        ]
    );
}

/// A pair that would put a second pair around a number or a LID the core was told about crosses it; the pair itself
/// and unrelated pairs do not; either side known is enough for `either_known`.
#[tokio::test]
async fn a_second_pair_around_a_stated_id_crosses_and_the_pair_itself_does_not() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert!(stated(&store, "pn:15550481301", "lid:99900481301").await);
    assert!(
        !stated(&store, "pn:15550481301", "lid:99900481301").await,
        "a pair is told once"
    );
    know(&store, &["lid:99900481302"]).await;
    let asked = |pn: &'static str, lid: &'static str| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| crosses_a_stated_pair(conn, pn, lid))
                .await
                .expect("crosses")
        }
    };
    assert!(
        !asked("pn:15550481301", "lid:99900481301").await,
        "the pair itself"
    );
    assert!(
        asked("pn:15550481301", "lid:99900481399").await,
        "the number has a LID"
    );
    assert!(
        asked("pn:15550481399", "lid:99900481301").await,
        "the LID has a number"
    );
    assert!(
        !asked("pn:15550481398", "lid:99900481397").await,
        "unrelated"
    );
    let known = |pn: &'static str, lid: &'static str| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| either_known(conn, pn, lid))
                .await
                .expect("known")
        }
    };
    assert!(known("pn:15550481399", "lid:99900481302").await);
    assert!(!known("pn:15550481399", "lid:99900481398").await);
}

/// Fix F20 (DUR-11): the crossing check asks for a pair around a LID too, and answers from `merges_stated_lid`, not from a scan of
/// the table on the writer thread. Mutant: the index left out of migration 8.
#[tokio::test]
async fn the_crossing_check_is_answered_from_an_index_for_the_lid_too() {
    let (_dir, owner) = open();
    let store = owner.store();
    let plan: Vec<String> = store
        .read(|conn| {
            let mut statement = conn
                .prepare(&format!("EXPLAIN QUERY PLAN {}", super::CROSSES))
                .map_err(crate::error::sql("plan"))?;
            let rows = statement
                .query_map(params!["pn:15550100101", "lid:99900100101"], |r| {
                    r.get::<_, String>(3)
                })
                .map_err(crate::error::sql("plan"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(crate::error::sql("plan"))?;
            Ok(rows)
        })
        .await
        .expect("plan");
    assert!(
        plan.iter().any(|line| line.contains("merges_stated_lid")),
        "the LID term is not answered from its index: {plan:?}"
    );
    assert!(
        plan.iter().all(|line| !line.starts_with("SCAN")),
        "a scan of the table: {plan:?}"
    );
    owner.close();
}

/// G2 (NEW-1): a zone claims the pairs IT recorded (the rows above the mark it read first) and a take-back forgets a pair only
/// for the row that claimed it: a pair another zone recorded before, even when this zone repeated it, stays told, and a row
/// with another number forgets nothing.
/// Mutant: `forget_told_by` without its `by_seq` condition.
#[tokio::test]
async fn a_take_back_forgets_only_the_pairs_its_own_row_recorded() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (told_pn, told_lid) = ("pn:15550483201", "lid:99900483201");
    let (own_pn, own_lid) = ("pn:15550483202", "lid:99900483202");
    assert!(
        stated(&store, told_pn, told_lid).await,
        "told by an earlier zone"
    );
    let claimed = store
        .write("test: a history row's zone", move |tx| {
            let mark = super::mark(tx)?;
            // The row repeats the earlier pair (nothing new) and records one of its own.
            assert!(!put_stated(tx, told_pn, told_lid)?);
            assert!(put_stated(tx, own_pn, own_lid)?);
            super::claim_since(tx, mark, 7)
        })
        .await
        .expect("zone");
    assert_eq!(claimed, 1, "the row claims the one pair it recorded");
    let forgotten = store
        .write("test: the take-backs", move |tx| {
            Ok((
                super::forget_told_by(tx, told_pn, told_lid, 7)?,
                super::forget_told_by(tx, own_pn, own_lid, 8)?,
                super::forget_told_by(tx, own_pn, own_lid, 7)?,
            ))
        })
        .await
        .expect("take-backs");
    assert_eq!(
        forgotten,
        (false, false, true),
        "the earlier pair stays, another row's take-back forgets nothing, the row's own pair goes"
    );
    let left: i64 = store
        .read(|conn| {
            conn.query_row("SELECT count(*) FROM merges_stated", [], |r| r.get(0))
                .map_err(crate::error::sql("count"))
        })
        .await
        .expect("count");
    assert_eq!(left, 1);
}
