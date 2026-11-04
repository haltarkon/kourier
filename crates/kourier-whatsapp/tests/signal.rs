//! `SignalStore` over `backend.db`: the behaviour of upstream's `InMemoryBackend` (what "absent",
//! "batch" and "has state for user" mean), a batch that is atomic, and a prekey that stays deleted.

use std::sync::Arc;

use bytes::Bytes;
use kourier_db::rusqlite::Connection;
use wacore::store::traits::SignalStore;

use crate::support::{TempStore, key, temp_store};

fn addr(s: &str) -> Arc<str> {
    Arc::from(s)
}

fn session(byte: u8) -> Bytes {
    Bytes::from(vec![byte; 5])
}

/// A second connection to the store's file: the test's own view, and the place to plant a trigger.
fn raw(t: &TempStore) -> Connection {
    kourier_db::open(&t.path(), &key()).expect("raw connection")
}

#[tokio::test]
async fn identities_round_trip_and_absent_is_none() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.load_identity("a@s.whatsapp.net").await.unwrap(), None);
    s.put_identity("a@s.whatsapp.net", [1; 32]).await.unwrap();
    s.put_identity("a@s.whatsapp.net", [2; 32]).await.unwrap();
    assert_eq!(
        s.load_identity("a@s.whatsapp.net").await.unwrap(),
        Some([2; 32]),
        "the later key replaces"
    );
    s.delete_identity("a@s.whatsapp.net").await.unwrap();
    s.delete_identity("a@s.whatsapp.net").await.unwrap();
    assert_eq!(s.load_identity("a@s.whatsapp.net").await.unwrap(), None);

    s.put_identities_batch(&[(addr("b@lid"), [3; 32]), (addr("c@lid"), [4; 32])])
        .await
        .unwrap();
    assert_eq!(s.load_identity("c@lid").await.unwrap(), Some([4; 32]));
    s.delete_identities_batch(&[addr("b@lid"), addr("c@lid"), addr("never@lid")])
        .await
        .unwrap();
    assert_eq!(s.load_identity("b@lid").await.unwrap(), None);
    assert_eq!(s.load_identity("c@lid").await.unwrap(), None);
    t.owner.close();
}

#[tokio::test]
async fn sessions_round_trip_in_single_and_batch_form() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.get_session("a@lid").await.unwrap(), None);
    assert!(!s.has_session("a@lid").await.unwrap());
    s.put_session("a@lid", &[1, 2, 3]).await.unwrap();
    assert!(s.has_session("a@lid").await.unwrap());
    assert_eq!(
        s.get_session("a@lid").await.unwrap().unwrap().as_ref(),
        [1, 2, 3]
    );

    // A batch is applied in order: the later entry of one address wins.
    s.put_sessions_batch(&[
        (addr("a@lid"), session(9)),
        (addr("b:2@lid"), session(8)),
        (addr("a@lid"), session(7)),
    ])
    .await
    .unwrap();
    assert_eq!(s.get_session("a@lid").await.unwrap(), Some(session(7)));
    // The read gives the addresses that exist, in the order asked, and skips the rest.
    let got = s
        .get_sessions_batch(&[
            addr("zz@lid"),
            addr("b:2@lid"),
            addr("a@lid"),
            addr("a@lid"),
        ])
        .await
        .unwrap();
    let got: Vec<_> = got
        .iter()
        .map(|(a, r)| (a.to_string(), r.clone()))
        .collect();
    assert_eq!(
        got,
        [
            ("b:2@lid".to_owned(), session(8)),
            ("a@lid".to_owned(), session(7)),
            ("a@lid".to_owned(), session(7)),
        ]
    );
    s.delete_session("a@lid").await.unwrap();
    s.delete_session("a@lid").await.unwrap();
    assert!(!s.has_session("a@lid").await.unwrap());
    s.delete_sessions_batch(&[addr("b:2@lid"), addr("nope@lid")])
        .await
        .unwrap();
    assert_eq!(s.get_session("b:2@lid").await.unwrap(), None);
    t.owner.close();
}

/// `has_signal_state_for_user` means what the in-memory store's does: a session or an identity whose
/// address is `user@…` or `user:device@…`, and no other user whose name merely starts the same.
#[tokio::test]
async fn state_for_a_user_is_found_across_device_ids_and_only_for_that_user() {
    let t = temp_store();
    let s = t.store();
    assert!(!s.has_signal_state_for_user("100").await.unwrap());
    s.put_session("1000@s.whatsapp.net", &[1]).await.unwrap();
    s.put_identity("2000:7@lid", [1; 32]).await.unwrap();
    s.put_session("300:12@lid", &[1]).await.unwrap();
    assert!(
        !s.has_signal_state_for_user("100").await.unwrap(),
        "100 is not 1000"
    );
    assert!(!s.has_signal_state_for_user("20").await.unwrap());
    assert!(
        s.has_signal_state_for_user("1000").await.unwrap(),
        "device 0, a session"
    );
    assert!(
        s.has_signal_state_for_user("2000").await.unwrap(),
        "a device, an identity"
    );
    assert!(s.has_signal_state_for_user("300").await.unwrap());
    assert!(!s.has_signal_state_for_user("3000").await.unwrap());
    // The characters around the range's ends: a user that sorts right above or below.
    s.put_session("5@a", &[1]).await.unwrap();
    s.put_session("5A@a", &[1]).await.unwrap();
    s.put_session("5;@a", &[1]).await.unwrap();
    s.delete_session("5@a").await.unwrap();
    assert!(!s.has_signal_state_for_user("5").await.unwrap());
    t.owner.close();
}

#[tokio::test]
async fn prekeys_follow_the_contract() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.get_max_prekey_id().await.unwrap(), 0, "no prekey, id 0");
    assert_eq!(s.load_prekey(7).await.unwrap(), None);
    s.store_prekey(7, &[7], false).await.unwrap();
    s.store_prekeys_batch(&[(8, session(8)), (u32::MAX, session(9))], false)
        .await
        .unwrap();
    assert_eq!(s.get_max_prekey_id().await.unwrap(), u32::MAX);
    assert_eq!(s.load_prekey(8).await.unwrap(), Some(session(8)));
    let got = s.load_prekeys_batch(&[9, u32::MAX, 7, 8]).await.unwrap();
    assert_eq!(
        got,
        [
            (u32::MAX, session(9)),
            (7, Bytes::from_static(&[7])),
            (8, session(8))
        ],
        "the keys that exist, in the order asked"
    );
    s.remove_prekeys_batch(&[u32::MAX, 12345]).await.unwrap();
    assert_eq!(s.get_max_prekey_id().await.unwrap(), 8);
    s.remove_prekey(8).await.unwrap();
    s.remove_prekey(8).await.unwrap();
    assert_eq!(s.load_prekey(8).await.unwrap(), None);
    t.owner.close();
}

/// UPDATE semantics: a prekey consumed between the upload snapshot and the mark stays deleted.
#[tokio::test]
async fn marking_prekeys_uploaded_never_brings_a_deleted_one_back() {
    let t = temp_store();
    let s = t.store();
    s.store_prekeys_batch(&[(1, session(1)), (2, session(2))], false)
        .await
        .unwrap();
    s.remove_prekey(2).await.unwrap();
    s.mark_prekeys_uploaded(&[1, 2, 3]).await.unwrap();
    assert_eq!(s.load_prekey(2).await.unwrap(), None, "no resurrection");
    assert_eq!(s.load_prekey(3).await.unwrap(), None, "no insertion");
    let uploaded: Vec<(i64, i64)> = {
        let conn = raw(&t);
        let mut stmt = conn
            .prepare("SELECT id, uploaded FROM prekeys ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(uploaded, [(1, 1)]);
    // Storing again with the flag cleared clears it (upsert), the record is replaced.
    s.store_prekey(1, &[9], false).await.unwrap();
    assert_eq!(s.load_prekey(1).await.unwrap().unwrap().as_ref(), [9]);
    t.owner.close();
}

#[tokio::test]
async fn signed_prekeys_and_sender_keys_round_trip() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.load_signed_prekey(1).await.unwrap(), None);
    s.store_signed_prekey(2, &[2]).await.unwrap();
    s.store_signed_prekey(1, &[1]).await.unwrap();
    s.store_signed_prekey(2, &[22]).await.unwrap();
    assert_eq!(s.load_signed_prekey(2).await.unwrap(), Some(vec![22]));
    assert_eq!(
        s.load_all_signed_prekeys().await.unwrap(),
        [(1, vec![1]), (2, vec![22])]
    );
    s.remove_signed_prekey(1).await.unwrap();
    s.remove_signed_prekey(1).await.unwrap();
    assert_eq!(s.load_all_signed_prekeys().await.unwrap(), [(2, vec![22])]);

    assert_eq!(s.get_sender_key("g::a").await.unwrap(), None);
    s.put_sender_key("g::a", &[1]).await.unwrap();
    s.put_sender_keys_batch(&[(addr("g::b"), session(2)), (addr("g::a"), session(3))])
        .await
        .unwrap();
    assert_eq!(s.get_sender_key("g::a").await.unwrap(), Some(vec![3; 5]));
    assert_eq!(s.get_sender_key("g::b").await.unwrap(), Some(vec![2; 5]));
    s.delete_sender_key("g::a").await.unwrap();
    s.delete_sender_keys_batch(&[addr("g::b"), addr("g::none")])
        .await
        .unwrap();
    assert_eq!(s.get_sender_key("g::b").await.unwrap(), None);
    t.owner.close();
}

/// A batch is ONE transaction: an error on its second row leaves none of it visible. A trigger
/// makes the second statement fail inside the transaction, as upstream's atomicity test does. A
/// batch written as one job per row would leave the first row behind, and fails this test.
#[tokio::test]
async fn a_batch_that_fails_on_its_second_row_leaves_nothing() {
    let t = temp_store();
    let s = t.store();
    s.put_session("old@lid", &[1]).await.unwrap();
    let conn = raw(&t);
    for (table, column) in [
        ("sessions", "address"),
        ("identities", "address"),
        ("sender_keys", "address"),
    ] {
        conn.execute_batch(&format!(
            "CREATE TRIGGER fail_b_{table} BEFORE INSERT ON {table} WHEN NEW.{column} = 'b@lid' \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;"
        ))
        .unwrap();
    }
    conn.execute_batch(
        "CREATE TRIGGER fail_prekey_2 BEFORE INSERT ON prekeys WHEN NEW.id = 2 \
         BEGIN SELECT RAISE(ABORT, 'refused'); END;",
    )
    .unwrap();
    drop(conn);

    let err = s
        .put_sessions_batch(&[
            (addr("a@lid"), session(1)),
            (addr("b@lid"), session(2)),
            (addr("c@lid"), session(3)),
        ])
        .await
        .expect_err("the second row is refused");
    assert_eq!(err.to_string(), "database operation error");
    for address in ["a@lid", "b@lid", "c@lid"] {
        assert_eq!(
            s.get_session(address).await.unwrap(),
            None,
            "{address} is visible"
        );
    }
    assert!(
        s.has_session("old@lid").await.unwrap(),
        "what was there stays"
    );

    s.put_identities_batch(&[(addr("a@lid"), [1; 32]), (addr("b@lid"), [2; 32])])
        .await
        .expect_err("refused");
    assert_eq!(s.load_identity("a@lid").await.unwrap(), None);
    s.put_sender_keys_batch(&[(addr("a@lid"), session(1)), (addr("b@lid"), session(2))])
        .await
        .expect_err("refused");
    assert_eq!(s.get_sender_key("a@lid").await.unwrap(), None);
    s.store_prekeys_batch(&[(1, session(1)), (2, session(2))], false)
        .await
        .expect_err("refused");
    assert_eq!(s.load_prekey(1).await.unwrap(), None);

    // The store works on after a refused batch.
    s.put_session("a@lid", &[5]).await.unwrap();
    assert_eq!(
        s.get_session("a@lid").await.unwrap(),
        Some(Bytes::from_static(&[5]))
    );
    t.owner.close();
}
