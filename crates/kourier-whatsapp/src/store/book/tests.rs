//! The address book's mirror and the people the core knows (W6a2, B8b). Synthetic values only.

use kourier_db::DbKey;

use super::{Row, know, known_of, of, put, take};
use crate::store::StoreOwner;

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([17; 32]),
        "wa-book-rows",
    )
    .expect("open");
    (dir, owner)
}

fn row(contact: &str, name: &str, pn: Option<&str>, lid: Option<&str>) -> Row {
    Row {
        contact: contact.to_owned(),
        name: name.to_owned(),
        pn: pn.map(str::to_owned),
        lid: lid.map(str::to_owned),
    }
}

#[tokio::test]
async fn a_row_is_a_change_only_when_a_value_differs_and_is_found_by_any_of_its_ids() {
    let (_dir, owner) = open();
    let store = owner.store();
    let ann = row(
        "pn:15550481201",
        "Ann Example",
        Some("pn:15550481201"),
        Some("lid:99900481201"),
    );
    let put_it = |r: Row| {
        let store = store.clone();
        async move {
            store
                .write("test: put", move |tx| put(tx, &r))
                .await
                .expect("put")
        }
    };
    assert!(put_it(ann.clone()).await, "a new row");
    assert!(!put_it(ann.clone()).await, "the same row is no change");
    assert!(
        put_it(row(
            "pn:15550481201",
            "Ann Renamed",
            ann.pn.as_deref(),
            ann.lid.as_deref()
        ))
        .await
    );
    assert!(
        put_it(row(
            "pn:15550481201",
            "Ann Renamed",
            ann.pn.as_deref(),
            None
        ))
        .await,
        "a form that went is a change"
    );
    assert!(put_it(ann.clone()).await);

    for id in ["pn:15550481201", "lid:99900481201"] {
        let found = store.read(move |conn| of(conn, id)).await.expect("read");
        assert!(found.is_some(), "{id} finds the row");
    }
    let none = store
        .read(|conn| of(conn, "pn:15550481299"))
        .await
        .expect("read");
    assert!(none.is_none());
    let taken = store
        .write("test: take", |tx| take(tx, "pn:15550481201"))
        .await
        .expect("take");
    assert!(taken.is_some());
    assert!(
        store
            .write("test: take again", |tx| take(tx, "pn:15550481201"))
            .await
            .expect("take")
            .is_none()
    );
    owner.close();
}

#[tokio::test]
async fn a_person_is_known_once_and_only_the_ones_asked_for_are_returned() {
    let (_dir, owner) = open();
    let store = owner.store();
    assert!(
        store
            .write("test: know", |tx| know(tx, "pn:15550481201"))
            .await
            .expect("know")
    );
    assert!(
        !store
            .write("test: know", |tx| know(tx, "pn:15550481201"))
            .await
            .expect("know")
    );
    let found = store
        .read(|conn| known_of(conn, &["lid:99900481201", "pn:15550481201"]))
        .await
        .expect("known");
    assert_eq!(found, ["pn:15550481201"]);
    owner.close();
}
