//! `ProtocolStore` (except the pending-inbound buffer) over `backend.db`: upstream's token contract
//! with its race rounds, and the meaning of every method as `InMemoryBackend` has it.

use wacore::store::traits::{
    DeviceInfo, DeviceListRecord, LidPnMappingEntry, ProtocolStore, TcTokenEntry,
};

use crate::support::tc_token_contract::assert_tc_token_contract;
use crate::support::temp_store;

#[tokio::test]
async fn upstreams_tc_token_contract_holds_with_its_race_rounds() {
    let t = temp_store();
    assert_tc_token_contract(&t.store()).await;
    t.owner.close();
}

#[tokio::test]
async fn sender_key_devices_are_tracked_per_group() {
    let t = temp_store();
    let s = t.store();
    assert!(
        s.get_sender_key_devices("g1@g.us")
            .await
            .unwrap()
            .is_empty()
    );
    s.set_sender_key_status("g1@g.us", &[("a:1@lid", true), ("b:2@lid", false)])
        .await
        .unwrap();
    s.set_sender_key_status("g2@g.us", &[("a:1@lid", true)])
        .await
        .unwrap();
    s.set_sender_key_status("g1@g.us", &[("b:2@lid", true)])
        .await
        .unwrap();
    assert_eq!(
        s.get_sender_key_devices("g1@g.us").await.unwrap(),
        [("a:1@lid".to_owned(), true), ("b:2@lid".to_owned(), true)]
    );
    // One device out of every group.
    s.delete_sender_key_device_rows(&["a:1@lid", "never@lid"])
        .await
        .unwrap();
    assert_eq!(
        s.get_sender_key_devices("g1@g.us").await.unwrap(),
        [("b:2@lid".to_owned(), true)]
    );
    assert!(
        s.get_sender_key_devices("g2@g.us")
            .await
            .unwrap()
            .is_empty()
    );
    s.delete_sender_key_device_rows(&[]).await.unwrap();
    s.set_sender_key_status("g2@g.us", &[("c:3@lid", false)])
        .await
        .unwrap();
    s.clear_sender_key_devices("g1@g.us").await.unwrap();
    assert!(
        s.get_sender_key_devices("g1@g.us")
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        s.get_sender_key_devices("g2@g.us").await.unwrap().len(),
        1,
        "only that group"
    );
    s.clear_all_sender_key_devices().await.unwrap();
    assert!(
        s.get_sender_key_devices("g2@g.us")
            .await
            .unwrap()
            .is_empty()
    );
    t.owner.close();
}

fn lid(lid: &str, phone: &str, updated_at: i64, source: &str) -> LidPnMappingEntry {
    LidPnMappingEntry {
        lid: lid.to_owned(),
        phone_number: phone.to_owned(),
        created_at: 10,
        updated_at,
        learning_source: source.to_owned(),
    }
}

/// The upsert on a LID replaces `created_at` with the rest of the entry, as `InMemoryBackend` does
/// (upstream's SQLite store keeps the first one; see `put_lid_mapping`). A pin bump that changes
/// either store is noticed here.
#[tokio::test]
async fn a_lid_upsert_replaces_created_at_like_the_in_memory_store() {
    let t = temp_store();
    let s = t.store();
    let mut entry = lid("100", "15550000001", 50, "usync");
    entry.created_at = 10;
    s.put_lid_mapping(&entry).await.unwrap();
    entry.created_at = 0; // the live cache writes 0 for a stale source
    entry.updated_at = 60;
    s.put_lid_mapping(&entry).await.unwrap();
    let got = s.get_lid_mapping("100").await.unwrap().unwrap();
    assert_eq!((got.created_at, got.updated_at), (0, 60));
    t.owner.close();
}

/// A phone number that gets a new LID keeps the old LID's row; the newest answers by phone number.
#[tokio::test]
async fn a_phone_with_two_lids_answers_with_the_newer_and_both_lids_still_resolve() {
    let t = temp_store();
    let s = t.store();
    s.put_lid_mapping(&lid("100", "15550000001", 50, "usync"))
        .await
        .unwrap();
    s.put_lid_mapping(&lid("200", "15550000001", 60, "peer_pn_message"))
        .await
        .unwrap();
    assert_eq!(
        s.get_lid_mapping("100")
            .await
            .unwrap()
            .unwrap()
            .phone_number,
        "15550000001"
    );
    assert_eq!(
        s.get_lid_mapping("200")
            .await
            .unwrap()
            .unwrap()
            .learning_source,
        "peer_pn_message"
    );
    assert_eq!(
        s.get_pn_mapping("15550000001").await.unwrap().unwrap().lid,
        "200"
    );
    // The older LID updated later is the newer answer.
    s.put_lid_mapping(&lid("100", "15550000001", 70, "usync"))
        .await
        .unwrap();
    assert_eq!(
        s.get_pn_mapping("15550000001").await.unwrap().unwrap().lid,
        "100"
    );
    // The same time: the row that was made later.
    s.put_lid_mappings(&[
        lid("300", "15550000002", 5, "a"),
        lid("400", "15550000002", 5, "b"),
    ])
    .await
    .unwrap();
    assert_eq!(
        s.get_pn_mapping("15550000002").await.unwrap().unwrap().lid,
        "400"
    );
    // The same LID with another phone number moves: the old number has no mapping any more.
    s.put_lid_mapping(&lid("300", "15550000003", 6, "c"))
        .await
        .unwrap();
    assert_eq!(
        s.get_pn_mapping("15550000003").await.unwrap().unwrap().lid,
        "300"
    );
    assert_eq!(
        s.get_pn_mapping("15550000002").await.unwrap().unwrap().lid,
        "400"
    );
    assert!(s.get_pn_mapping("19990000000").await.unwrap().is_none());
    assert!(s.get_lid_mapping("nope").await.unwrap().is_none());
    let all = s.get_all_lid_mappings().await.unwrap();
    assert_eq!(all.len(), 4, "{all:?}");
    t.owner.close();
}

#[tokio::test]
async fn base_keys_detect_a_collision_and_expire() {
    let t = temp_store();
    let s = t.store();
    assert!(!s.has_same_base_key("a@lid", "m1", &[1, 2]).await.unwrap());
    s.save_base_key("a@lid", "m1", &[1, 2]).await.unwrap();
    assert!(s.has_same_base_key("a@lid", "m1", &[1, 2]).await.unwrap());
    assert!(!s.has_same_base_key("a@lid", "m1", &[9]).await.unwrap());
    assert!(!s.has_same_base_key("a@lid", "m2", &[1, 2]).await.unwrap());
    s.save_base_key("a@lid", "m1", &[7]).await.unwrap();
    assert!(
        s.has_same_base_key("a@lid", "m1", &[7]).await.unwrap(),
        "replaced"
    );
    s.delete_base_key("a@lid", "m1").await.unwrap();
    assert!(!s.has_same_base_key("a@lid", "m1", &[7]).await.unwrap());
    // Expiry: the rows made now are older than a cutoff in the future, not than one in the past.
    s.save_base_key("b@lid", "m1", &[1]).await.unwrap();
    s.save_base_key("b@lid", "m2", &[2]).await.unwrap();
    assert_eq!(s.delete_expired_base_keys(1).await.unwrap(), 0);
    assert_eq!(
        s.delete_expired_base_keys(i64::MAX).await.unwrap(),
        2,
        "overridden: not the Ok(0) default"
    );
    assert!(!s.has_same_base_key("b@lid", "m1", &[1]).await.unwrap());
    t.owner.close();
}

fn record(user: &str, timestamp: i64) -> DeviceListRecord {
    DeviceListRecord {
        user: user.into(),
        devices: vec![
            DeviceInfo::new(0, None),
            DeviceInfo::new(3, Some(9)).with_hosting(true),
        ]
        .into(),
        timestamp,
        phash: Some("2:abcdef".into()),
        raw_id: Some(7),
    }
}

#[tokio::test]
async fn the_device_registry_round_trips_in_single_and_batch_form() {
    let t = temp_store();
    let s = t.store();
    assert!(s.get_devices("u1").await.unwrap().is_none());
    s.update_device_list(record("u1", 10)).await.unwrap();
    s.update_device_list(record("u1", 11)).await.unwrap();
    let got = s.get_devices("u1").await.unwrap().expect("record");
    assert_eq!((&*got.user, got.timestamp, got.raw_id), ("u1", 11, Some(7)));
    assert_eq!(got.devices.len(), 2);
    assert!(got.devices[1].is_hosted());
    assert_eq!(got.devices[1].key_index(), Some(9));
    assert_eq!(got.phash.as_deref(), Some("2:abcdef"));
    s.update_device_lists(vec![record("u2", 1), record("u3", 2)])
        .await
        .unwrap();
    let mut users: Vec<String> = s
        .get_devices_batch(&["u3", "missing", "u1", "u2"])
        .await
        .unwrap()
        .iter()
        .map(|r| r.user.to_string())
        .collect();
    users.sort();
    assert_eq!(users, ["u1", "u2", "u3"], "absent users are left out");
    s.delete_devices("u1").await.unwrap();
    s.delete_devices("u1").await.unwrap();
    assert!(s.get_devices("u1").await.unwrap().is_none());
    t.owner.close();
}

#[tokio::test]
async fn group_metadata_is_stored_and_forgotten() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.get_group_metadata("g@g.us").await.unwrap(), None);
    s.put_group_metadata("g@g.us", &[1, 2, 3]).await.unwrap();
    s.put_group_metadata("g@g.us", &[4]).await.unwrap();
    assert_eq!(s.get_group_metadata("g@g.us").await.unwrap(), Some(vec![4]));
    s.delete_group_metadata("g@g.us").await.unwrap();
    s.delete_group_metadata("g@g.us").await.unwrap();
    assert_eq!(s.get_group_metadata("g@g.us").await.unwrap(), None);
    t.owner.close();
}

fn token(bytes: &[u8], at: i64, sender: Option<i64>) -> TcTokenEntry {
    TcTokenEntry {
        token: bytes.to_vec(),
        token_timestamp: at,
        sender_timestamp: sender,
    }
}

#[tokio::test]
async fn tc_tokens_round_trip_and_the_merges_follow_the_traits_rules() {
    let t = temp_store();
    let s = t.store();
    assert!(s.get_tc_token("a@lid").await.unwrap().is_none());
    s.put_tc_token("a@lid", &token(&[1], 10, Some(20)))
        .await
        .unwrap();
    s.put_tc_token("b@lid", &token(&[], 5, None)).await.unwrap();
    let got = s
        .get_tc_tokens(&[
            "b@lid".to_owned(),
            "none@lid".to_owned(),
            "a@lid".to_owned(),
        ])
        .await
        .unwrap();
    assert_eq!(got.len(), 3, "in the order asked");
    assert_eq!(got[0].as_ref().unwrap().token, Vec::<u8>::new());
    assert!(got[1].is_none());
    assert_eq!(got[2].as_ref().unwrap().sender_timestamp, Some(20));
    assert_eq!(s.get_all_tc_token_jids().await.unwrap(), ["a@lid", "b@lid"]);

    // touch: forward only, and a missing row becomes a byte-less placeholder with the sender time as its token time.
    s.touch_tc_token_sender_timestamp("a@lid", 15)
        .await
        .unwrap();
    assert_eq!(
        s.get_tc_token("a@lid")
            .await
            .unwrap()
            .unwrap()
            .sender_timestamp,
        Some(20),
        "never back"
    );
    s.touch_tc_token_sender_timestamp("a@lid", 25)
        .await
        .unwrap();
    let a = s.get_tc_token("a@lid").await.unwrap().unwrap();
    assert_eq!(
        (a.token, a.token_timestamp, a.sender_timestamp),
        (vec![1], 10, Some(25)),
        "token untouched"
    );
    s.touch_tc_token_sender_timestamp("new@lid", 40)
        .await
        .unwrap();
    let n = s.get_tc_token("new@lid").await.unwrap().unwrap();
    assert_eq!(
        (n.token.len(), n.token_timestamp, n.sender_timestamp),
        (0, 40, Some(40))
    );
    s.touch_tc_token_sender_timestamp("b@lid", 7).await.unwrap();
    assert_eq!(
        s.get_tc_token("b@lid")
            .await
            .unwrap()
            .unwrap()
            .sender_timestamp,
        Some(7),
        "NULL becomes the time"
    );

    // received: newer wins, equal accepts, a placeholder is replaced, the sender time stays.
    s.store_received_tc_token("a@lid", &[2], 5).await.unwrap();
    assert_eq!(
        s.get_tc_token("a@lid").await.unwrap().unwrap().token,
        vec![1],
        "a stale token is dropped"
    );
    s.store_received_tc_token("a@lid", &[3], 10).await.unwrap();
    let a = s.get_tc_token("a@lid").await.unwrap().unwrap();
    assert_eq!(
        (a.token, a.sender_timestamp),
        (vec![3], Some(25)),
        "equal time accepts; sender kept"
    );
    s.store_received_tc_token("new@lid", &[4], 1).await.unwrap();
    assert_eq!(
        s.get_tc_token("new@lid").await.unwrap().unwrap().token,
        vec![4],
        "a placeholder yields even to an older time"
    );
    s.store_received_tc_token("fresh@lid", &[5], 9)
        .await
        .unwrap();
    assert_eq!(
        s.get_tc_token("fresh@lid")
            .await
            .unwrap()
            .unwrap()
            .sender_timestamp,
        None,
        "a new row has none"
    );

    s.delete_tc_token("a@lid").await.unwrap();
    s.delete_tc_token("a@lid").await.unwrap();
    assert!(s.get_tc_token("a@lid").await.unwrap().is_none());
    t.owner.close();
}

/// A row goes only when its received token AND its sender bucket are both expired or absent.
#[tokio::test]
async fn expiry_keeps_a_row_whose_sender_bucket_is_recent() {
    let t = temp_store();
    let s = t.store();
    s.put_tc_token("old_both@lid", &token(&[1], 10, Some(10)))
        .await
        .unwrap();
    s.put_tc_token("old_token_no_sender@lid", &token(&[1], 10, None))
        .await
        .unwrap();
    s.put_tc_token("old_token_recent_sender@lid", &token(&[1], 10, Some(100)))
        .await
        .unwrap();
    s.put_tc_token("recent_token@lid", &token(&[1], 100, None))
        .await
        .unwrap();
    s.put_tc_token("placeholder_old@lid", &token(&[], 100, Some(10)))
        .await
        .unwrap();
    s.put_tc_token("empty_token_recent_time@lid", &token(&[], 100, None))
        .await
        .unwrap();
    let gone = s.delete_expired_tc_tokens(50, 50).await.unwrap();
    let left = s.get_all_tc_token_jids().await.unwrap();
    assert_eq!(
        left,
        ["old_token_recent_sender@lid", "recent_token@lid"],
        "{gone} removed"
    );
    assert_eq!(gone, 4);
    t.owner.close();
}

#[tokio::test]
async fn sent_messages_are_read_without_consuming_and_taken_once() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(s.get_sent_message("c@lid", "m1").await.unwrap(), None);
    s.store_sent_message("c@lid", "m1", &[1, 2, 3])
        .await
        .unwrap();
    assert_eq!(
        s.get_sent_message("c@lid", "m1").await.unwrap(),
        Some(vec![1, 2, 3])
    );
    assert_eq!(
        s.get_sent_message("c@lid", "m1").await.unwrap(),
        Some(vec![1, 2, 3]),
        "a read does not consume"
    );
    assert_eq!(
        s.take_sent_message("c@lid", "m1").await.unwrap(),
        Some(vec![1, 2, 3])
    );
    assert_eq!(
        s.take_sent_message("c@lid", "m1").await.unwrap(),
        None,
        "taken once"
    );
    assert_eq!(s.get_sent_message("c@lid", "m1").await.unwrap(), None);
    s.store_sent_message("c@lid", "m2", &[9]).await.unwrap();
    s.store_sent_message("c@lid", "m3", &[8]).await.unwrap();
    assert_eq!(s.delete_expired_sent_messages(1).await.unwrap(), 0);
    assert_eq!(s.delete_expired_sent_messages(i64::MAX).await.unwrap(), 2);
    assert_eq!(s.take_sent_message("c@lid", "m2").await.unwrap(), None);
    t.owner.close();
}
