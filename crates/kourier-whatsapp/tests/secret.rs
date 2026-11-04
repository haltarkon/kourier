//! `MsgSecretStore`: the secret of each message and the merge rules of a second write.

use std::sync::Arc;

use wacore::store::error::StoreError;
use wacore::store::traits::{MsgSecretEntry, MsgSecretStore};

use crate::support::{TempStore, key, temp_store};

fn entry(msg_id: &str, secret: u8, expires_at: i64, message_ts: i64) -> MsgSecretEntry {
    MsgSecretEntry {
        chat: Arc::from("chat@g.us"),
        sender: Arc::from("sender@lid"),
        msg_id: Arc::from(msg_id),
        secret: [secret; 32],
        expires_at,
        message_ts,
    }
}

async fn stored(t: &TempStore, msg_id: &str) -> Option<(u8, Option<i64>)> {
    let found = t
        .store()
        .get_stored_msg_secret("chat@g.us", "sender@lid", msg_id)
        .await
        .expect("read");
    found.map(|s| (s.secret.into_bytes()[0], s.message_ts))
}

fn expires_at(t: &TempStore, msg_id: &str) -> i64 {
    let conn = kourier_db::open(&t.path(), &key()).expect("raw");
    conn.query_row(
        "SELECT expires_at FROM msg_secrets WHERE msg_id = ?1",
        [msg_id],
        |r| r.get(0),
    )
    .expect("row")
}

#[tokio::test]
async fn a_secret_round_trips_and_absent_is_none() {
    let t = temp_store();
    let s = t.store();
    assert_eq!(stored(&t, "m1").await, None);
    assert_eq!(
        s.put_msg_secrets(vec![entry("m1", 1, 0, 100), entry("m2", 2, 0, 0)])
            .await
            .unwrap(),
        2
    );
    assert_eq!(stored(&t, "m1").await, Some((1, Some(100))));
    // A parent time of 0 is "unknown", not the epoch.
    assert_eq!(stored(&t, "m2").await, Some((2, None)));
    let bytes = s
        .get_msg_secret("chat@g.us", "sender@lid", "m1")
        .await
        .unwrap();
    assert_eq!(bytes, Some(vec![1; 32]));
    // The convenience wrapper stores with no expiry.
    s.put_msg_secret("chat@g.us", "sender@lid", "m3", &[3; 32])
        .await
        .unwrap();
    assert_eq!(expires_at(&t, "m3"), 0);
    t.owner.close();
}

/// On a second write the INCOMING secret wins (both upstream stores take it), `0` ("never") wins the
/// deadline both ways, otherwise the later deadline, and a `0` parent time never clobbers a known one.
#[tokio::test]
async fn a_second_write_merges_by_the_traits_rules() {
    let t = temp_store();
    let s = t.store();
    // existing, incoming, wanted: (deadline, parent time)
    /// (deadline, parent time)
    type Pair = (i64, i64);
    let cases: [(Pair, Pair, Pair); 6] = [
        ((0, 10), (500, 20), (0, 20)),     // existing never: stays never
        ((500, 10), (0, 20), (0, 20)),     // incoming never: becomes never
        ((500, 10), (900, 5), (900, 10)),  // the later deadline, the later parent time
        ((900, 10), (500, 30), (900, 30)), // an earlier deadline never shortens the window
        ((700, 40), (700, 0), (700, 40)),  // a 0 parent time never clobbers a known one
        ((0, 0), (0, 0), (0, 0)),
    ];
    for (i, (existing, incoming, want)) in cases.into_iter().enumerate() {
        let id = format!("case{i}");
        s.put_msg_secrets(vec![entry(&id, 1, existing.0, existing.1)])
            .await
            .unwrap();
        s.put_msg_secrets(vec![entry(&id, 9, incoming.0, incoming.1)])
            .await
            .unwrap();
        let (secret, ts) = stored(&t, &id).await.expect("row");
        assert_eq!(secret, 9, "{id}: the incoming secret replaces");
        assert_eq!((expires_at(&t, &id), ts.unwrap_or(0)), want, "{id}");
    }
    t.owner.close();
}

#[tokio::test]
async fn expired_secrets_go_and_the_ones_that_never_expire_stay() {
    let t = temp_store();
    let s = t.store();
    s.put_msg_secrets(vec![
        entry("old", 1, 100, 1),
        entry("edge", 2, 200, 1),
        entry("future", 3, 300, 1),
        entry("never", 4, 0, 1),
    ])
    .await
    .unwrap();
    assert_eq!(
        s.delete_expired_msg_secrets(200).await.unwrap(),
        2,
        "at or before the cutoff"
    );
    assert_eq!(stored(&t, "old").await, None);
    assert_eq!(stored(&t, "edge").await, None);
    assert!(stored(&t, "future").await.is_some());
    assert!(stored(&t, "never").await.is_some(), "0 is never");
    assert_eq!(s.delete_expired_msg_secrets(i64::MAX).await.unwrap(), 1);
    assert!(stored(&t, "never").await.is_some());
    t.owner.close();
}

#[tokio::test]
async fn malformed_stored_bytes_are_an_invalid_secret_error_naming_only_the_length() {
    let t = temp_store();
    let s = t.store();
    s.put_msg_secrets(vec![entry("good", 1, 0, 1)])
        .await
        .unwrap();
    kourier_db::open(&t.path(), &key())
        .expect("raw")
        .execute(
            "INSERT INTO msg_secrets(chat, sender, msg_id, secret) VALUES ('chat@g.us', 'sender@lid', 'bad', x'0102')",
            [],
        )
        .expect("plant a short secret");
    let err = s
        .get_stored_msg_secret("chat@g.us", "sender@lid", "bad")
        .await
        .expect_err("2 bytes is no secret");
    assert!(
        matches!(err, StoreError::InvalidMessageSecret(_)),
        "{err:?}"
    );
    let shown = format!("{err} {err:?}");
    assert!(!shown.contains("0102") && !shown.contains("bad"), "{shown}");
    // Even the byte-only projection validates.
    s.get_msg_secret("chat@g.us", "sender@lid", "bad")
        .await
        .expect_err("validated");
    t.owner.close();
}
