//! The pending-inbound buffer (`ProtocolStore`): a decrypted message waits here until the inbound
//! durability hook committed it. The cases are upstream's own, ported by name from
//! `wacore/src/store/in_memory.rs` and `storages/sqlite-storage/src/sqlite_store.rs`, plus the three
//! rules the first port missed: an identical replacement is an idempotent success even with a stale
//! `expected` (delete has no such shortcut), the replacement is bounded before anything is read, and
//! a real replacement refreshes `inserted_at`.

use std::sync::Arc;

use bytes::Bytes;
use kourier_db::rusqlite::Connection;
use wacore::store::traits::{
    PendingInboundKey, PendingInboundLease, PendingInboundMutation, PendingInboundPayload,
    PendingInboundRevision, PendingInboundRow, ProtocolStore,
};

use crate::support::{TempStore, key, temp_store};

fn lease() -> PendingInboundLease {
    PendingInboundLease::new(Arc::new(()))
}

fn put(bytes: &'static [u8]) -> PendingInboundPayload {
    PendingInboundPayload::new(Bytes::from_static(bytes), lease())
}

fn raw(t: &TempStore) -> Connection {
    kourier_db::open(&t.path(), &key()).expect("raw connection")
}

fn inserted_at(t: &TempStore, id: &str) -> i64 {
    raw(t)
        .query_row(
            "SELECT inserted_at FROM pending_inbound_messages WHERE id = ?1",
            [id],
            |r| r.get(0),
        )
        .expect("row")
}

const KEY: PendingInboundKey<'static> = PendingInboundKey {
    chat: "test-chat",
    sender: "test-sender",
    id: "same-id",
};

#[tokio::test]
async fn pending_bounded_group_legacy_alias_preserves_physical_cas_and_lease() {
    let t = temp_store();
    let s = t.store();
    let chat = "120363000000000081@g.us";
    let sender = "200:75@lid";
    let key = PendingInboundKey {
        chat,
        sender,
        id: "LEGACY_ALIAS",
    };
    let logical = PendingInboundKey {
        sender: "200@lid",
        ..key
    };
    s.store_pending_inbound(chat, sender, key.id, b"old")
        .await
        .unwrap();
    s.store_pending_inbound(chat, "200:75@s.whatsapp.net", key.id, b"other")
        .await
        .unwrap();
    let owner = Arc::new(());
    let weak = Arc::downgrade(&owner);
    let (physical, record) = s
        .read_pending_inbound_group_bounded(logical, 16, PendingInboundLease::new(owner.clone()))
        .await
        .unwrap()
        .unwrap();
    drop(owner);
    assert!(
        weak.upgrade().is_some(),
        "the returned record retains the owner"
    );
    assert_eq!(physical, sender, "the physical spelling is returned");
    assert_eq!(record.bytes(), b"old");
    let stale = record.revision();
    drop(record);
    assert!(weak.upgrade().is_none());
    s.store_pending_inbound(chat, sender, key.id, b"new")
        .await
        .unwrap();
    assert_eq!(
        s.delete_pending_inbound_if(key, stale, 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    assert!(
        s.read_pending_inbound_group_bounded(logical, 2, lease())
            .await
            .is_err(),
        "too long"
    );
    s.store_pending_inbound(chat, logical.sender, key.id, b"ambiguous")
        .await
        .unwrap();
    assert!(
        s.read_pending_inbound_group_bounded(logical, 16, lease())
            .await
            .is_err(),
        "two spellings of one sender are ambiguous, not absent"
    );
    assert_eq!(
        s.get_pending_inbound(chat, sender, key.id)
            .await
            .unwrap()
            .unwrap(),
        b"new"
    );
    s.delete_pending_inbound(chat, logical.sender, key.id)
        .await
        .unwrap();
    let (_, record) = s
        .read_pending_inbound_group_bounded(logical, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        s.delete_pending_inbound_if(key, record.revision(), 16, record.lease())
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert!(
        s.read_pending_inbound_group_bounded(logical, 16, lease())
            .await
            .unwrap()
            .is_none()
    );
    t.owner.close();
}

#[tokio::test]
async fn pending_bounded_empty_is_present_and_delete_requires_revision() {
    let t = temp_store();
    let s = t.store();
    s.store_pending_inbound(KEY.chat, KEY.sender, KEY.id, &[])
        .await
        .unwrap();
    let empty = s
        .read_pending_inbound_bounded(KEY, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert!(empty.bytes().is_empty());
    assert_ne!(
        empty.revision(),
        PendingInboundRevision::missing(),
        "empty is present, not missing"
    );
    assert_eq!(
        s.delete_pending_inbound_if(KEY, PendingInboundRevision::missing(), 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict,
        "a present row against an expected absence"
    );
    let replacement = put(b"\x0a\x81\x00a");
    let installed = replacement.revision();
    assert_eq!(
        s.replace_pending_inbound_if(KEY, empty.revision(), replacement, 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    let original = s
        .read_pending_inbound_bounded(KEY, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.bytes(), b"\x0a\x81\x00a");
    assert_eq!(original.revision(), installed);
    // The same payload again, with the stale revision: idempotent.
    assert_eq!(
        s.replace_pending_inbound_if(KEY, empty.revision(), put(b"\x0a\x81\x00a"), 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert_eq!(
        s.replace_pending_inbound_if(KEY, empty.revision(), put(b"\x0a\x01a"), 16)
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    assert_eq!(
        s.delete_pending_inbound_if(KEY, empty.revision(), 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    assert_eq!(
        s.read_pending_inbound_bounded(KEY, 16, lease())
            .await
            .unwrap()
            .unwrap()
            .bytes(),
        original.bytes()
    );
    assert_eq!(
        s.delete_pending_inbound_if(KEY, original.revision(), 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert!(
        s.read_pending_inbound_bounded(KEY, 16, lease())
            .await
            .unwrap()
            .is_none()
    );
    t.owner.close();
}

#[tokio::test]
async fn pending_conditional_updates_preserve_noncanonical_legacy_bytes() {
    let t = temp_store();
    let s = t.store();
    let compact = [0x0a, 0x01, b'a'];
    let extended = [0x0a, 0x81, 0x00, b'a'];
    assert_eq!(
        waproto::codec::message_decode(&compact)
            .unwrap()
            .conversation,
        waproto::codec::message_decode(&extended)
            .unwrap()
            .conversation,
        "two encodings of one message"
    );
    s.store_pending_inbound(KEY.chat, KEY.sender, KEY.id, &compact)
        .await
        .unwrap();
    let original = s
        .read_pending_inbound_bounded(KEY, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.bytes(), compact);
    let replacement = PendingInboundPayload::new(Bytes::copy_from_slice(&extended), lease());
    let installed = replacement.revision();
    assert_eq!(
        s.replace_pending_inbound_if(KEY, original.revision(), replacement, 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    let successor = s
        .read_pending_inbound_bounded(KEY, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        successor.bytes(),
        extended,
        "the bytes are stored as they came, not re-encoded"
    );
    assert_eq!(successor.revision(), installed);
    assert_ne!(successor.revision(), original.revision());
    let stale = PendingInboundPayload::new(Bytes::copy_from_slice(b"stale"), lease());
    assert_eq!(
        s.replace_pending_inbound_if(KEY, original.revision(), stale, 16)
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    assert_eq!(
        s.delete_pending_inbound_if(KEY, original.revision(), 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    let again = PendingInboundPayload::new(Bytes::copy_from_slice(&extended), lease());
    assert_eq!(
        s.replace_pending_inbound_if(KEY, original.revision(), again, 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert_eq!(
        s.get_pending_inbound(KEY.chat, KEY.sender, KEY.id)
            .await
            .unwrap()
            .unwrap(),
        extended
    );
    t.owner.close();
}

/// The unconditional write of the legacy methods changes the revision a stale owner holds.
#[tokio::test]
async fn pending_bounded_legacy_write_invalidates_a_held_revision() {
    let t = temp_store();
    let s = t.store();
    let key = PendingInboundKey {
        id: "legacy-overwrite",
        ..KEY
    };
    let payload = put(b"old");
    let old = payload.revision();
    assert_eq!(
        s.replace_pending_inbound_if(key, PendingInboundRevision::missing(), payload, 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert!(
        s.delete_pending_inbound_if(key, old, 2, lease())
            .await
            .is_err(),
        "the stored row is longer than the bound: an error, not a conflict"
    );
    s.store_pending_inbound(key.chat, key.sender, key.id, b"new")
        .await
        .unwrap();
    assert_eq!(
        s.delete_pending_inbound_if(key, old, 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    assert_eq!(
        s.replace_pending_inbound_if(key, old, put(b"third"), 16)
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    let record = s
        .read_pending_inbound_bounded(key, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.bytes(), b"new");
    assert_eq!(record.revision(), PendingInboundRevision::for_bytes(b"new"));
    assert_eq!(
        s.delete_pending_inbound_if(key, record.revision(), 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    t.owner.close();
}

#[tokio::test]
async fn pending_bounded_rejects_oversize_without_mutation() {
    let t = temp_store();
    let s = t.store();
    let key = PendingInboundKey { id: "large", ..KEY };
    s.store_pending_inbound(key.chat, key.sender, key.id, &[1; 32])
        .await
        .unwrap();
    assert!(
        s.read_pending_inbound_bounded(key, 16, lease())
            .await
            .is_err()
    );
    assert!(
        s.replace_pending_inbound_if(key, PendingInboundRevision::missing(), put(b"small"), 16)
            .await
            .is_err(),
        "an oversized existing row is an error, not a conflict"
    );
    assert!(
        s.delete_pending_inbound_if(key, PendingInboundRevision::missing(), 16, lease())
            .await
            .is_err()
    );
    assert_eq!(
        s.get_pending_inbound(key.chat, key.sender, key.id)
            .await
            .unwrap()
            .unwrap(),
        [1; 32]
    );
    s.delete_pending_inbound(key.chat, key.sender, key.id)
        .await
        .unwrap();
    let big = PendingInboundPayload::new(Bytes::from_static(&[2; 32]), lease());
    assert!(
        s.replace_pending_inbound_if(key, PendingInboundRevision::missing(), big, 16)
            .await
            .is_err(),
        "an oversized replacement is refused"
    );
    assert!(
        s.get_pending_inbound(key.chat, key.sender, key.id)
            .await
            .unwrap()
            .is_none(),
        "and nothing is written"
    );
    t.owner.close();
}

/// An oversized blob, then a row that is no BLOB at all (text, an integer): every bounded
/// operation answers with an error, none with absence, and the row is never changed.
#[tokio::test]
async fn pending_bounded_rejects_oversized_blob_and_large_text_without_mutation() {
    let t = temp_store();
    let s = t.store();
    let key = PendingInboundKey {
        id: "oversized",
        ..KEY
    };
    let payload = vec![0x7a; 2 * 1024 * 1024];
    s.store_pending_inbound(key.chat, key.sender, key.id, &payload)
        .await
        .unwrap();
    let revision = PendingInboundRevision::for_bytes(&payload);
    assert!(
        s.read_pending_inbound_bounded(key, 16, lease())
            .await
            .is_err()
    );
    assert!(
        s.replace_pending_inbound_if(key, revision, put(b"new"), 16)
            .await
            .is_err()
    );
    assert!(
        s.delete_pending_inbound_if(key, revision, 16, lease())
            .await
            .is_err()
    );
    let replacement = PendingInboundPayload::new(Bytes::copy_from_slice(&payload), lease());
    assert!(
        s.replace_pending_inbound_if(key, revision, replacement, 16)
            .await
            .is_err()
    );
    assert_eq!(
        s.get_pending_inbound(key.chat, key.sender, key.id)
            .await
            .unwrap()
            .unwrap(),
        payload
    );

    for planted in [
        "'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'",
        "'ab'",
        "12345",
        "NULL_PLACEHOLDER",
    ] {
        if planted == "NULL_PLACEHOLDER" {
            continue;
        }
        raw(&t)
            .execute(
                &format!(
                    "UPDATE pending_inbound_messages SET message = {planted} WHERE id = 'oversized'"
                ),
                [],
            )
            .expect("plant a row that is no blob");
        assert!(
            s.read_pending_inbound_bounded(key, 1000, lease())
                .await
                .is_err(),
            "{planted}: read"
        );
        assert!(
            s.replace_pending_inbound_if(key, revision, put(b"new"), 1000)
                .await
                .is_err(),
            "{planted}: replace"
        );
        assert!(
            s.delete_pending_inbound_if(key, revision, 1000, lease())
                .await
                .is_err(),
            "{planted}: delete"
        );
        let kind: String = raw(&t)
            .query_row(
                "SELECT typeof(message) FROM pending_inbound_messages WHERE id = 'oversized'",
                [],
                |r| r.get(0),
            )
            .expect("row");
        assert_ne!(kind, "blob", "{planted}: the row was changed");
    }
    t.owner.close();
}

/// A retry of `replace_pending_inbound_if` after a dropped caller finds its own payload installed:
/// that is `Applied` whatever `expected` says. A plain compare-and-swap answers `Conflict` there,
/// which the durability flow would read as "another owner".
#[tokio::test]
async fn an_identical_replacement_is_applied_even_with_a_stale_expected_revision() {
    let t = temp_store();
    let s = t.store();
    let key = PendingInboundKey { id: "retry", ..KEY };
    let first = put(b"payload-v1");
    assert_eq!(
        s.replace_pending_inbound_if(key, PendingInboundRevision::missing(), first, 64)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    // The retry, with the revision the first attempt observed (missing): still Applied.
    assert_eq!(
        s.replace_pending_inbound_if(
            key,
            PendingInboundRevision::missing(),
            put(b"payload-v1"),
            64
        )
        .await
        .unwrap(),
        PendingInboundMutation::Applied
    );
    // A different payload with the same stale revision is a conflict, and nothing changed.
    assert_eq!(
        s.replace_pending_inbound_if(
            key,
            PendingInboundRevision::missing(),
            put(b"payload-v2"),
            64
        )
        .await
        .unwrap(),
        PendingInboundMutation::Conflict
    );
    assert_eq!(
        s.get_pending_inbound(key.chat, key.sender, key.id)
            .await
            .unwrap()
            .unwrap(),
        b"payload-v1"
    );
    // Delete has no such shortcut: a stale revision is a conflict even though nothing else changed.
    assert_eq!(
        s.delete_pending_inbound_if(key, PendingInboundRevision::missing(), 64, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    // Deleting an absent row with the absence expected is a no-op success.
    s.delete_pending_inbound(key.chat, key.sender, key.id)
        .await
        .unwrap();
    assert_eq!(
        s.delete_pending_inbound_if(key, PendingInboundRevision::missing(), 64, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    // A present revision against absence is a conflict.
    assert_eq!(
        s.delete_pending_inbound_if(key, PendingInboundRevision::for_bytes(b"x"), 64, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict
    );
    t.owner.close();
}

/// A replacement longer than the bound is refused whatever is stored; nothing is changed.
#[tokio::test]
async fn a_replacement_over_the_bound_is_refused_and_leaves_the_row() {
    let t = temp_store();
    let s = t.store();
    let key = PendingInboundKey { id: "bound", ..KEY };
    s.store_pending_inbound(key.chat, key.sender, key.id, b"small")
        .await
        .unwrap();
    let revision = PendingInboundRevision::for_bytes(b"small");
    let too_long = PendingInboundPayload::new(Bytes::from(vec![9; 100]), lease());
    assert!(
        s.replace_pending_inbound_if(key, revision, too_long, 64)
            .await
            .is_err()
    );
    assert_eq!(
        s.get_pending_inbound(key.chat, key.sender, key.id)
            .await
            .unwrap()
            .unwrap(),
        b"small"
    );
    // At the bound exactly is fine.
    let exact = PendingInboundPayload::new(Bytes::from(vec![9; 64]), lease());
    assert_eq!(
        s.replace_pending_inbound_if(key, revision, exact, 64)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    t.owner.close();
}

/// A real replacement refreshes `inserted_at` (else the expiry sweep would drop a just-replaced row);
/// the idempotent one writes nothing and so does not.
#[tokio::test]
async fn a_real_replacement_refreshes_inserted_at_and_an_idempotent_one_does_not() {
    let t = temp_store();
    let s = t.store();
    let key = PendingInboundKey { id: "age", ..KEY };
    s.store_pending_inbound(key.chat, key.sender, key.id, b"one")
        .await
        .unwrap();
    raw(&t)
        .execute(
            "UPDATE pending_inbound_messages SET inserted_at = 1 WHERE id = 'age'",
            [],
        )
        .unwrap();
    assert_eq!(
        s.replace_pending_inbound_if(
            key,
            PendingInboundRevision::for_bytes(b"one"),
            put(b"two"),
            64
        )
        .await
        .unwrap(),
        PendingInboundMutation::Applied
    );
    assert!(inserted_at(&t, "age") > 1_000_000_000, "refreshed to now");
    raw(&t)
        .execute(
            "UPDATE pending_inbound_messages SET inserted_at = 1 WHERE id = 'age'",
            [],
        )
        .unwrap();
    assert_eq!(
        s.replace_pending_inbound_if(
            key,
            PendingInboundRevision::for_bytes(b"one"),
            put(b"two"),
            64
        )
        .await
        .unwrap(),
        PendingInboundMutation::Applied,
        "idempotent"
    );
    assert_eq!(inserted_at(&t, "age"), 1, "nothing was written");
    // The sweep: older than the cutoff goes, the count is returned.
    assert_eq!(s.delete_expired_pending_inbound(2).await.unwrap(), 1);
    assert!(
        s.get_pending_inbound(key.chat, key.sender, key.id)
            .await
            .unwrap()
            .is_none()
    );
    t.owner.close();
}

#[tokio::test]
async fn the_group_read_knows_zero_one_two_and_too_many_spellings() {
    let t = temp_store();
    let s = t.store();
    let chat = "120363000000000082@g.us";
    let key = |sender| PendingInboundKey {
        chat,
        sender,
        id: "g1",
    };
    assert!(
        s.read_pending_inbound_group_bounded(key("200@lid"), 16, lease())
            .await
            .unwrap()
            .is_none(),
        "0"
    );
    s.store_pending_inbound(chat, "200:3@lid", "g1", b"a")
        .await
        .unwrap();
    let (physical, record) = s
        .read_pending_inbound_group_bounded(key("200@lid"), 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (physical.as_str(), record.bytes()),
        ("200:3@lid", &b"a"[..]),
        "1"
    );
    // Another id of the same chat is another row.
    s.store_pending_inbound(chat, "200:4@lid", "other", b"b")
        .await
        .unwrap();
    s.store_pending_inbound(chat, "200:4@lid", "g1", b"c")
        .await
        .unwrap();
    assert!(
        s.read_pending_inbound_group_bounded(key("200@lid"), 16, lease())
            .await
            .is_err(),
        "2: ambiguous"
    );
    // A sender over 128 bytes cannot be asked for.
    let long = format!("{}@lid", "2".repeat(130));
    assert!(
        s.read_pending_inbound_group_bounded(key(&long), 16, lease())
            .await
            .is_err()
    );
    // 400 other senders are fine, 401 are an error.
    let t2 = temp_store();
    let s2 = t2.store();
    let senders: Vec<String> = (0..401).map(|i| format!("300:{i}@lid")).collect();
    let batch: Vec<PendingInboundRow<'_>> = senders[..400]
        .iter()
        .map(|sender| PendingInboundRow {
            chat,
            sender,
            id: "g1",
            message: b"x",
        })
        .collect();
    s2.store_pending_inbound_batch(&batch).await.unwrap();
    assert!(
        s2.read_pending_inbound_group_bounded(key("200@lid"), 16, lease())
            .await
            .unwrap()
            .is_none(),
        "400"
    );
    s2.store_pending_inbound(chat, &senders[400], "g1", b"x")
        .await
        .unwrap();
    assert!(
        s2.read_pending_inbound_group_bounded(key("200@lid"), 16, lease())
            .await
            .is_err(),
        "401"
    );
    // An oversized spelling in the table is an error, not absence.
    let t3 = temp_store();
    let s3 = t3.store();
    s3.store_pending_inbound(chat, &format!("{}:75@lid", "2".repeat(129)), "g1", b"x")
        .await
        .unwrap();
    assert!(
        s3.read_pending_inbound_group_bounded(key("200@lid"), 16, lease())
            .await
            .is_err()
    );
    t.owner.close();
    t2.owner.close();
    t3.owner.close();
}

#[tokio::test]
async fn the_plain_methods_and_the_batches_round_trip() {
    let t = temp_store();
    let s = t.store();
    assert!(
        s.get_pending_inbound("c", "s", "i")
            .await
            .unwrap()
            .is_none()
    );
    s.store_pending_inbound("c", "s", "i", b"1").await.unwrap();
    s.store_pending_inbound("c", "s", "i", b"2").await.unwrap();
    assert_eq!(
        s.get_pending_inbound("c", "s", "i").await.unwrap().unwrap(),
        b"2"
    );
    s.store_pending_inbound_batch(&[
        PendingInboundRow {
            chat: "c",
            sender: "s",
            id: "j",
            message: b"3",
        },
        PendingInboundRow {
            chat: "c",
            sender: "s",
            id: "k",
            message: b"4",
        },
    ])
    .await
    .unwrap();
    s.delete_pending_inbound_batch(&[
        PendingInboundKey {
            chat: "c",
            sender: "s",
            id: "i",
        },
        PendingInboundKey {
            chat: "c",
            sender: "s",
            id: "j",
        },
        PendingInboundKey {
            chat: "c",
            sender: "s",
            id: "none",
        },
    ])
    .await
    .unwrap();
    assert!(
        s.get_pending_inbound("c", "s", "i")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        s.get_pending_inbound("c", "s", "j")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        s.get_pending_inbound("c", "s", "k").await.unwrap().unwrap(),
        b"4"
    );
    s.delete_pending_inbound("c", "s", "k").await.unwrap();
    s.delete_pending_inbound("c", "s", "k").await.unwrap();
    assert_eq!(s.delete_expired_pending_inbound(i64::MAX).await.unwrap(), 0);
    t.owner.close();
}
