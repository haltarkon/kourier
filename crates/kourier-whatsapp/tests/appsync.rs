//! `AppSyncStore` over `backend.db`: upstream's patch contract, a patch that is atomic (a trigger
//! makes each statement of it fail inside the transaction), the dirty flag that follows the version,
//! and the sync keys.

use kourier_db::rusqlite::Connection;
use wacore::appstate::hash::HashState;
use wacore::appstate::processor::AppStateMutationMAC;
use wacore::store::traits::{AppStateSyncKey, AppSyncStore};

use crate::support::appstate_commit_contract::assert_patch_commit_contract;
use crate::support::{TempStore, key, temp_store};

fn state(version: u64) -> HashState {
    HashState {
        version,
        hash: [version as u8; 128],
        ..HashState::default()
    }
}

fn mac(index: u8, value: u8) -> AppStateMutationMAC {
    AppStateMutationMAC {
        index_mac: vec![index; 32],
        value_mac: vec![value; 32],
    }
}

fn raw(t: &TempStore) -> Connection {
    kourier_db::open(&t.path(), &key()).expect("raw connection")
}

fn dirty(t: &TempStore) -> Vec<(String, i64)> {
    let conn = raw(t);
    let mut stmt = conn
        .prepare("SELECT name, version FROM appstate_dirty ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn upstreams_patch_commit_contract_holds() {
    let t = temp_store();
    assert_patch_commit_contract(&t.store()).await;
    t.owner.close();
}

#[tokio::test]
async fn a_collection_never_synced_has_no_version_and_an_empty_one_has_version_zero() {
    let t = temp_store();
    let s = t.store();
    assert!(
        s.get_version("regular").await.unwrap().is_none(),
        "never synced is None"
    );
    let mut empty = state(0);
    empty.bootstrapped = true;
    s.set_version("regular", empty).await.unwrap();
    let got = s
        .get_version("regular")
        .await
        .unwrap()
        .expect("synced and empty");
    assert_eq!(got.version, 0);
    assert!(got.bootstrapped, "the flag round trips");
    assert!(!got.mac_mismatch_fatal);
    s.delete_version("regular").await.unwrap();
    assert!(
        s.get_version("regular").await.unwrap().is_none(),
        "a rebuild is expressed as None"
    );
    s.delete_version("never").await.unwrap();
    t.owner.close();
}

#[tokio::test]
async fn macs_are_stored_read_singly_and_in_batches_and_deleted() {
    let t = temp_store();
    let s = t.store();
    // More than one IN list (500) of index MACs.
    let macs: Vec<AppStateMutationMAC> = (0..1200u32)
        .map(|i| {
            let mut index = vec![0u8; 32];
            index[..4].copy_from_slice(&i.to_le_bytes());
            AppStateMutationMAC {
                index_mac: index,
                value_mac: vec![(i % 251) as u8; 32],
            }
        })
        .collect();
    s.put_mutation_macs("regular", 3, &macs).await.unwrap();
    s.put_mutation_macs("other", 3, &macs[..1]).await.unwrap();
    assert_eq!(
        s.get_mutation_mac("regular", &macs[7].index_mac)
            .await
            .unwrap(),
        Some(macs[7].value_mac.clone())
    );
    assert_eq!(s.get_mutation_mac("regular", &[9; 32]).await.unwrap(), None);
    let asked: Vec<[u8; 32]> = macs
        .iter()
        .map(|m| <[u8; 32]>::try_from(m.index_mac.as_slice()).unwrap())
        .chain([[0xee; 32]])
        .collect();
    let got = s.get_mutation_macs("regular", &asked).await.unwrap();
    assert_eq!(got.len(), 1200, "the one that does not exist is left out");
    assert_eq!(got[&asked[1199]], macs[1199].value_mac);
    // Another collection's MACs are not mixed in.
    assert_eq!(s.get_mutation_macs("other", &asked).await.unwrap().len(), 1);
    s.delete_mutation_macs("regular", &[macs[0].index_mac.clone(), vec![1; 32]])
        .await
        .unwrap();
    assert_eq!(
        s.get_mutation_mac("regular", &macs[0].index_mac)
            .await
            .unwrap(),
        None
    );
    s.clear_mutation_macs("regular").await.unwrap();
    assert!(
        s.get_mutation_macs("regular", &asked)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.get_mutation_macs("other", &asked).await.unwrap().len(),
        1,
        "only that collection"
    );
    t.owner.close();
}

#[tokio::test]
async fn sync_keys_round_trip_and_the_latest_is_the_newest_by_its_own_time() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.get_latest_sync_key_id().await.unwrap(), None);
    assert!(s.get_sync_key(b"k1").await.unwrap().is_none());
    let k = |data: u8, ts: i64| AppStateSyncKey {
        key_data: vec![data; 4],
        fingerprint: vec![data; 2],
        timestamp: ts,
    };
    s.set_sync_key(b"k2", k(2, 200)).await.unwrap();
    s.set_sync_key(b"k1", k(1, 100)).await.unwrap();
    assert_eq!(
        s.get_latest_sync_key_id().await.unwrap(),
        Some(b"k2".to_vec()),
        "a re-shared OLDER key does not become the latest"
    );
    let got = s.get_sync_key(b"k1").await.unwrap().unwrap();
    assert_eq!(
        (got.key_data, got.fingerprint, got.timestamp),
        (vec![1; 4], vec![1; 2], 100)
    );
    // The same time: the higher key id.
    s.set_sync_key(b"k3", k(3, 200)).await.unwrap();
    assert_eq!(
        s.get_latest_sync_key_id().await.unwrap(),
        Some(b"k3".to_vec())
    );
    // A key set again is replaced, not duplicated.
    s.set_sync_key(b"k1", k(9, 300)).await.unwrap();
    assert_eq!(
        s.get_latest_sync_key_id().await.unwrap(),
        Some(b"k1".to_vec())
    );
    assert_eq!(
        s.get_sync_key(b"k1").await.unwrap().unwrap().key_data,
        vec![9; 4]
    );
    t.owner.close();
}

#[tokio::test]
async fn the_dirty_flag_follows_every_version_write() {
    let t = temp_store();
    let s = t.store();
    assert!(dirty(&t).is_empty());
    // The snapshot / pagination path.
    s.set_version("regular", state(4)).await.unwrap();
    assert_eq!(dirty(&t), [("regular".to_owned(), 4)]);
    // The patch path, with its MACs.
    s.commit_patch("regular", state(5), &[], &[mac(1, 1)])
        .await
        .unwrap();
    s.commit_patch("critical_block", state(2), &[], &[])
        .await
        .unwrap();
    assert_eq!(
        dirty(&t),
        [("critical_block".to_owned(), 2), ("regular".to_owned(), 5)]
    );
    // A rebuild leaves the flag (and makes a clean collection dirty).
    s.delete_version("regular").await.unwrap();
    s.delete_version("never_synced").await.unwrap();
    assert_eq!(
        dirty(&t),
        [
            ("critical_block".to_owned(), 2),
            ("never_synced".to_owned(), 0),
            ("regular".to_owned(), 5)
        ]
    );
}

/// What a quiet look's clearing does to a collection whose events reached `core_outbox`.
fn clear_flag(t: &TempStore, name: &str) {
    raw(t)
        .execute("DELETE FROM appstate_dirty WHERE name = ?1", [name])
        .expect("clear the flag");
}

/// `set_version` marks a collection dirty when its version MOVES. Upstream's empty sync writes the
/// state again at the version it already has (`bootstrapped`, nothing else): no event zone follows,
/// so a flag set there could never be cleared, and the start-up resync (W6a1) would ask for the
/// collection at every start.
#[tokio::test]
async fn a_set_version_at_the_stored_version_leaves_a_clean_collection_clean() {
    let t = temp_store();
    let s = t.store();
    s.set_version("regular", state(4)).await.unwrap();
    assert_eq!(dirty(&t), [("regular".to_owned(), 4)], "4 is new: it moved");
    clear_flag(&t, "regular");
    // The empty sync: the same version, `bootstrapped` now set.
    let mut again = state(4);
    again.bootstrapped = true;
    s.set_version("regular", again).await.unwrap();
    assert!(
        dirty(&t).is_empty(),
        "the version did not move: still clean"
    );
    assert!(
        s.get_version("regular")
            .await
            .unwrap()
            .unwrap()
            .bootstrapped,
        "the state itself was written"
    );
    // A dirty collection stays as it is, at its own version.
    s.set_version("regular", state(5)).await.unwrap();
    assert_eq!(dirty(&t), [("regular".to_owned(), 5)], "5 moved");
    s.set_version("regular", state(5)).await.unwrap();
    assert_eq!(dirty(&t), [("regular".to_owned(), 5)], "left alone");
    // A version that goes back moved too; a collection without a stored state is new at any
    // version, 0 included.
    clear_flag(&t, "regular");
    s.set_version("regular", state(3)).await.unwrap();
    assert_eq!(dirty(&t), [("regular".to_owned(), 3)], "3 moved");
    s.delete_version("regular").await.unwrap();
    clear_flag(&t, "regular");
    s.set_version("regular", state(3)).await.unwrap();
    assert_eq!(
        dirty(&t),
        [("regular".to_owned(), 3)],
        "no stored state: new"
    );
    let mut empty = state(0);
    empty.bootstrapped = true;
    s.set_version("fresh", empty).await.unwrap();
    assert_eq!(
        dirty(&t),
        [("fresh".to_owned(), 0), ("regular".to_owned(), 3)]
    );
    t.owner.close();
}

fn plant_trigger(t: &TempStore, sql: &str) {
    raw(t).execute_batch(sql).expect("trigger");
}

/// Each statement of a patch fails INSIDE the transaction in turn: the call returns `Err` and the
/// version, the MACs and the dirty row are exactly what they were. A patch written as several jobs
/// (or the dirty row in a job of its own) would leave part of it behind and fails here.
async fn a_patch_that_fails_at(trigger: &str) {
    let t = temp_store();
    let s = t.store();
    s.commit_patch("regular", state(1), &[], &[mac(1, 1), mac(2, 2)])
        .await
        .unwrap();
    plant_trigger(&t, trigger);
    let err = s
        .commit_patch("regular", state(2), &[vec![1; 32]], &[mac(3, 3), mac(2, 9)])
        .await
        .expect_err("the statement is refused");
    assert_eq!(err.to_string(), "database operation error");
    assert_eq!(
        s.get_version("regular").await.unwrap().unwrap().version,
        1,
        "{trigger}"
    );
    assert_eq!(
        s.get_mutation_mac("regular", &[1; 32]).await.unwrap(),
        Some(vec![1; 32]),
        "removed MAC is back"
    );
    assert_eq!(
        s.get_mutation_mac("regular", &[2; 32]).await.unwrap(),
        Some(vec![2; 32]),
        "replaced MAC is old"
    );
    assert_eq!(
        s.get_mutation_mac("regular", &[3; 32]).await.unwrap(),
        None,
        "added MAC is absent"
    );
    assert_eq!(
        dirty(&t),
        [("regular".to_owned(), 1)],
        "the dirty row is the old one"
    );
    t.owner.close();
}

#[tokio::test]
async fn a_patch_whose_version_insert_fails_leaves_nothing() {
    a_patch_that_fails_at(
        "CREATE TRIGGER t BEFORE INSERT ON app_state_versions BEGIN SELECT RAISE(ABORT, 'x'); END;",
    )
    .await;
}

#[tokio::test]
async fn a_patch_whose_mac_delete_fails_leaves_nothing() {
    a_patch_that_fails_at(
        "CREATE TRIGGER t BEFORE DELETE ON app_state_mutation_macs BEGIN SELECT RAISE(ABORT, 'x'); END;",
    )
    .await;
}

#[tokio::test]
async fn a_patch_whose_mac_insert_fails_leaves_nothing() {
    a_patch_that_fails_at(
        "CREATE TRIGGER t BEFORE INSERT ON app_state_mutation_macs WHEN NEW.index_mac = x'0303030303030303030303030303030303030303030303030303030303030303' \
         BEGIN SELECT RAISE(ABORT, 'x'); END;",
    )
    .await;
}

/// Kourier's own statement of the patch: the dirty row is part of the transaction, so a failure to
/// write it undoes the version too.
#[tokio::test]
async fn a_patch_whose_dirty_row_fails_leaves_nothing() {
    a_patch_that_fails_at(
        "CREATE TRIGGER t BEFORE INSERT ON appstate_dirty BEGIN SELECT RAISE(ABORT, 'x'); END;",
    )
    .await;
}

#[tokio::test]
async fn set_version_is_atomic_with_its_dirty_row_too() {
    let t = temp_store();
    let s = t.store();
    s.set_version("regular", state(1)).await.unwrap();
    plant_trigger(
        &t,
        "CREATE TRIGGER t BEFORE INSERT ON appstate_dirty BEGIN SELECT RAISE(ABORT, 'x'); END;",
    );
    s.set_version("regular", state(2))
        .await
        .expect_err("the dirty row is refused");
    assert_eq!(s.get_version("regular").await.unwrap().unwrap().version, 1);
    assert_eq!(dirty(&t), [("regular".to_owned(), 1)]);
    t.owner.close();
}

/// `delete_version` writes two statements, the delete and the dirty row of a clean collection; one
/// transaction, so a refused dirty row leaves the stored version where it was (a write as two jobs
/// would have deleted the version already).
#[tokio::test]
async fn delete_version_is_atomic_with_its_dirty_row() {
    let t = temp_store();
    let s = t.store();
    s.set_version("regular", state(3)).await.unwrap();
    clear_flag(&t, "regular");
    plant_trigger(
        &t,
        "CREATE TRIGGER t BEFORE INSERT ON appstate_dirty BEGIN SELECT RAISE(ABORT, 'x'); END;",
    );
    s.delete_version("regular")
        .await
        .expect_err("the dirty row is refused");
    assert_eq!(
        s.get_version("regular").await.unwrap().unwrap().version,
        3,
        "the delete was undone with it"
    );
    assert!(dirty(&t).is_empty());
    t.owner.close();
}
