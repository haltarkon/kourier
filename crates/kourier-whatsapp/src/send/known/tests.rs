//! The store's PN <-> LID pairs as a send reads them. Synthetic ids only (the fictitious 555-01xx range, tails of
//! 8 digits that are substrings of no other value).

use kourier_db::DbKey;
use kourier_db::rusqlite::params;
use kourier_model::UserId;

use super::load;
use crate::send::address::Mapping;
use crate::store::{StoreOwner, exec};

const PN: &str = "15550147302869";
const OLD_LID: &str = "77700128459063";
const NEW_LID: &str = "77700163807142";
const OTHER_PN: &str = "15550193724518";
const UNKNOWN_LID: &str = "77700185261937";

fn person(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a user")
}

#[tokio::test]
async fn a_send_reads_the_current_lid_of_a_number_and_the_number_of_a_lid() {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([5; 32]),
        "wa-known",
    )
    .expect("open");
    let store = owner.store();
    store
        .write("learn", |tx| {
            let put = "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                       VALUES (?1, ?2, 0, ?3, 'test')";
            exec(tx, "learn", put, params![OLD_LID, PN, 100])?;
            exec(tx, "learn", put, params![NEW_LID, PN, 200])
        })
        .await
        .expect("learn");
    let users = vec![
        person("pn", PN),
        person("lid", OLD_LID),
        person("pn", OTHER_PN),
        person("lid", UNKNOWN_LID),
    ];
    let known = store
        .read(move |conn| load(conn, &users))
        .await
        .expect("load");
    assert!(
        known.lid_of(PN).as_deref() == Some(NEW_LID),
        "a number with two LIDs is written as its newest"
    );
    assert!(
        known.pn_of(OLD_LID).as_deref() == Some(PN),
        "an older LID still names its number"
    );
    assert!(
        known.lid_of(OTHER_PN).is_none() && known.pn_of(UNKNOWN_LID).is_none(),
        "a person the store does not pair stays unpaired"
    );
    assert!(
        known.pn_of(NEW_LID).is_none(),
        "only the users the send named are read"
    );
    owner.close();
}
