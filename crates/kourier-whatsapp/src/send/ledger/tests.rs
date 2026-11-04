//! The outcome ledger (W5a, D4): look-up, first record wins, only `acked` and `refused`, the started fact, an
//! acked revoke's take of the sent copies, the 14-day sweep. Synthetic ids only (the fictitious 555-01xx range,
//! tails of 8 digits that are substrings of no other value).

use kourier_backend_api::BackendEvent;
use kourier_db::DbKey;
use kourier_db::rusqlite::params;
use kourier_model::{ConvKey, MsgTarget, ReceiptKind, TargetAuthor, UserId};

use super::{
    Entry, KEEP_MS, Kind, Outcome, Recorded, acked_at, commit, look, record, rehome_receipts, sweep,
};
use crate::send::started::holds;
use crate::store::{KourierStore, StoreOwner, exec};

const PEER_PN: &str = "15550146392817";
const PEER_LID: &str = "77700158204736";
const OTHER_PN: &str = "15550187360294";
const NOW: i64 = 1_760_000_000_000;

fn direct(scheme: &str, value: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts(scheme, value).expect("a user"))
}

fn entry(conv: &ConvKey, local: &str, kind: Kind, target: Option<&str>, outcome: Outcome) -> Entry {
    Entry {
        conv: conv.clone(),
        local: local.into(),
        kind,
        target: target.map(Into::into),
        outcome,
    }
}

fn acked(t: i64) -> Outcome {
    Outcome::Acked { server_t: Some(t) }
}

fn open() -> (tempfile::TempDir, StoreOwner) {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([10; 32]),
        "wa-ledger",
    )
    .expect("open");
    (dir, owner)
}

async fn put(store: &KourierStore, entry: Entry, at: i64) -> Recorded {
    store
        .write("record", move |tx| record(tx, &entry, at))
        .await
        .expect("record")
}

async fn looked(store: &KourierStore, local: &str) -> Option<Outcome> {
    let local = local.to_owned();
    store
        .read(move |conn| look(conn, &local))
        .await
        .expect("look")
}

#[tokio::test]
async fn an_outcome_is_read_back_the_first_record_stays_and_a_failure_cannot_be_written() {
    let (_dir, owner) = open();
    let store = owner.store();
    let conv = direct("pn", PEER_PN);
    assert_eq!(looked(&store, "3EB0A1").await, None, "nothing yet");
    let first = put(
        &store,
        entry(&conv, "3EB0A1", Kind::Message, None, acked(1_760_000_123)),
        NOW,
    )
    .await;
    assert!(first.new);
    assert_eq!(looked(&store, "3EB0A1").await, Some(acked(1_760_000_123)));
    let l = "3EB0A1".to_owned();
    let at = store
        .read(move |conn| acked_at(conn, &l))
        .await
        .expect("read");
    assert_eq!(
        at,
        Some(1_760_000_123),
        "the original's server time for the edit window"
    );
    // A second record of the same local keeps the first.
    let second = put(
        &store,
        entry(&conv, "3EB0A1", Kind::Message, None, Outcome::Refused),
        NOW,
    )
    .await;
    assert!(!second.new);
    assert_eq!(looked(&store, "3EB0A1").await, Some(acked(1_760_000_123)));
    put(
        &store,
        entry(&conv, "3EB0A2", Kind::Message, None, Outcome::Refused),
        NOW,
    )
    .await;
    assert_eq!(looked(&store, "3EB0A2").await, Some(Outcome::Refused));
    assert_eq!(
        looked(&store, "3EB0FE").await,
        None,
        "another message has no outcome"
    );
    // Critic H2: no failure is kept, the table refuses one.
    let refused = store
        .write("a failure", |tx| {
            exec(
                tx,
                "a failure",
                "INSERT INTO send_outcomes(conv, local, kind, outcome, at) VALUES ('x', 'y', 'message', 'failed', 0)",
                [],
            )
        })
        .await;
    assert!(refused.is_err(), "a failed outcome was written");
    owner.close();
}

#[tokio::test]
async fn an_acked_send_starts_its_chat_and_a_refused_one_does_not() {
    let (_dir, owner) = open();
    let store = owner.store();
    let (taken, refused) = (direct("pn", PEER_PN), direct("pn", OTHER_PN));
    // Through the outcome zone the actor uses (its kill points are the kill loop's, chunk 15b).
    commit(
        &store,
        entry(&taken, "3EB0B1", Kind::Message, None, acked(5)),
        NOW,
    )
    .await
    .expect("commit");
    put(
        &store,
        entry(&refused, "3EB0B2", Kind::Message, None, Outcome::Refused),
        NOW,
    )
    .await;
    let look = |conv: ConvKey| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| holds(conn, &conv))
                .await
                .expect("look")
        }
    };
    assert!(
        look(taken.clone()).await,
        "an acked message proves the chat"
    );
    assert!(!look(refused.clone()).await, "a 463 proves nothing");
    owner.close();
}

#[tokio::test]
async fn an_acked_revoke_takes_the_sent_copies_of_the_original_and_its_edits_under_both_forms() {
    let (_dir, owner) = open();
    let store = owner.store();
    let conv = direct("pn", PEER_PN);
    let pn_chat = format!("{PEER_PN}@s.whatsapp.net");
    let lid_chat = format!("{PEER_LID}@lid");
    let (pn_chat2, lid_chat2) = (pn_chat.clone(), lid_chat.clone());
    store
        .write("seed", move |tx| {
            exec(
                tx,
                "seed",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'test')",
                params![PEER_LID, PEER_PN],
            )?;
            for (chat, id) in [
                (&pn_chat2, "3EB0C0"),
                (&lid_chat2, "3EB0C1"),
                (&lid_chat2, "3EB0C9"),
                (&pn_chat2, "3EB0C2"),
            ] {
                exec(
                    tx,
                    "seed",
                    "INSERT INTO sent_messages(chat_jid, message_id, payload, created_at) VALUES (?1, ?2, x'00', 0)",
                    params![chat, id],
                )?;
            }
            Ok(())
        })
        .await
        .expect("seed");
    // 3EB0C0 the original, 3EB0C1 its acked edit (kept by upstream under the LID), 3EB0C2 a copy that holds no
    // edit (its payload is no message), 3EB0C9 another message. An edit with no ack is the next test's.
    put(
        &store,
        entry(&conv, "3EB0C0", Kind::Message, None, acked(1)),
        NOW,
    )
    .await;
    put(
        &store,
        entry(&conv, "3EB0C1", Kind::Edit, Some("3EB0C0"), acked(2)),
        NOW,
    )
    .await;
    // A refused revoke takes nothing.
    let none = put(
        &store,
        entry(
            &conv,
            "3EB0C7",
            Kind::Revoke,
            Some("3EB0C0"),
            Outcome::Refused,
        ),
        NOW,
    )
    .await;
    assert_eq!(none.taken, 0, "a refused revoke took a copy");
    let done = put(
        &store,
        entry(&conv, "3EB0C8", Kind::Revoke, Some("3EB0C0"), acked(3)),
        NOW,
    )
    .await;
    assert_eq!(done.taken, 2, "the original's and its acked edit's copies");
    let left: Vec<String> = store
        .read(|conn| {
            crate::store::rows(
                conn,
                "left",
                "SELECT message_id FROM sent_messages ORDER BY message_id",
                [],
                |r| r.get(0),
            )
        })
        .await
        .expect("left");
    assert_eq!(left, ["3EB0C2", "3EB0C9"]);
    owner.close();
}

/// Fix F13 (privacy MEDIUM): an acked revoke deletes the upload record of the message it deletes for everyone, in the
/// zone that records it, and of no other message; a refused revoke and an acked edit leave it (the caption edit of a
/// kept message reads that row). Mutant: the delete removed from the revoke's arm.
#[tokio::test]
async fn an_acked_revoke_deletes_the_upload_record_of_its_target_and_nothing_else() {
    let (_dir, owner) = open();
    let store = owner.store();
    let conv = direct("pn", PEER_PN);
    for local in ["3EB0D0", "3EB0D1"] {
        let conv = conv.clone();
        store
            .write("plant", move |tx| {
                crate::send::record::put(
                    tx,
                    &conv,
                    local,
                    &crate::media::MediaPointer {
                        media_type: "image".into(),
                        direct_path: "/v/t62.7118-24/synthetic-path-70192836".into(),
                        media_key: format!("{:0>64}", "70192836"),
                        file_length: 1,
                        ..crate::media::MediaPointer::default()
                    },
                    NOW,
                )
            })
            .await
            .expect("plant");
    }
    let rows = |store: &KourierStore| {
        let store = store.clone();
        async move {
            store
                .read(|conn| {
                    crate::store::rows(
                        conn,
                        "rows",
                        "SELECT local FROM media_uploads ORDER BY local",
                        [],
                        |r| r.get::<_, String>(0),
                    )
                })
                .await
                .expect("rows")
        }
    };
    put(
        &store,
        entry(&conv, "3EB0D0", Kind::Message, None, acked(1)),
        NOW,
    )
    .await;
    put(
        &store,
        entry(&conv, "3EB0D2", Kind::Edit, Some("3EB0D0"), acked(2)),
        NOW,
    )
    .await;
    put(
        &store,
        entry(
            &conv,
            "3EB0D3",
            Kind::Revoke,
            Some("3EB0D0"),
            Outcome::Refused,
        ),
        NOW,
    )
    .await;
    assert_eq!(
        rows(&store).await,
        ["3EB0D0", "3EB0D1"],
        "an acked edit and a refused revoke keep the record"
    );
    put(
        &store,
        entry(&conv, "3EB0D4", Kind::Revoke, Some("3EB0D0"), acked(3)),
        NOW,
    )
    .await;
    assert_eq!(
        rows(&store).await,
        ["3EB0D1"],
        "an acked revoke deletes its target's record and no other"
    );
    owner.close();
}

#[tokio::test]
async fn rows_older_than_fourteen_days_go_at_a_sweep() {
    let (_dir, owner) = open();
    let store = owner.store();
    let conv = direct("pn", PEER_PN);
    put(
        &store,
        entry(&conv, "3EB0D1", Kind::Message, None, acked(1)),
        NOW - KEEP_MS - 1,
    )
    .await;
    put(
        &store,
        entry(&conv, "3EB0D2", Kind::React, None, acked(2)),
        NOW - KEEP_MS,
    )
    .await;
    let gone = store
        .write("sweep", |tx| sweep(tx, NOW))
        .await
        .expect("sweep");
    assert_eq!(gone, 1);
    assert_eq!(looked(&store, "3EB0D1").await, None, "the old row went");
    assert_eq!(
        looked(&store, "3EB0D2").await,
        Some(acked(2)),
        "a row of exactly 14 days stays"
    );
    owner.close();
}

fn user(scheme: &str, value: &str) -> UserId {
    UserId::from_parts(scheme, value).expect("a user")
}

fn receipt_for(conv: &ConvKey, author: TargetAuthor, locals: &[&str], by: &UserId) -> BackendEvent {
    BackendEvent::Receipts {
        targets: locals
            .iter()
            .map(|local| MsgTarget {
                conv: Some(conv.clone()),
                author: author.clone(),
                local: (*local).into(),
                alternates: Vec::new(),
            })
            .collect(),
        by: by.clone(),
        kind: ReceiptKind::Delivered,
        at: 1_760_000_000_500,
    }
}

async fn rehomed(store: &KourierStore, events: Vec<BackendEvent>) -> Option<Vec<BackendEvent>> {
    store
        .read(move |conn| rehome_receipts(conn, &events))
        .await
        .expect("rehome")
}

/// Fix F9 (interop MEDIUM-1): Kourier writes a chat the core keys by the NUMBER to its LID (D20), so the peer's receipts
/// come back from the LID, and the core has no conversation under that form. The ledger knows the chat of every message
/// Kourier sent: the receipt of OUR message moves there, with the peer of that chat as `by`.
#[tokio::test]
async fn a_receipt_of_a_message_sent_into_a_number_chat_goes_to_that_chat_when_it_comes_from_the_lid()
 {
    let (_dir, owner) = open();
    let store = owner.store();
    let number_chat = direct("pn", PEER_PN);
    let lid_chat = direct("lid", PEER_LID);
    let other_chat = direct("pn", OTHER_PN);
    store
        .write("seed", |tx| {
            exec(
                tx,
                "seed",
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 0, 0, 'usync')",
                params![PEER_LID, PEER_PN],
            )
        })
        .await
        .expect("seed");
    for (conv, local) in [
        (&number_chat, "3EB0D1"),
        (&number_chat, "3EB0D2"),
        (&other_chat, "3EB0D3"),
    ] {
        put(
            &store,
            entry(conv, local, Kind::Message, None, acked(7)),
            NOW,
        )
        .await;
    }
    let (peer_lid, peer_pn) = (user("lid", PEER_LID), user("pn", PEER_PN));
    let merged = BackendEvent::IdentityMerged {
        from: peer_pn.clone(),
        into: peer_lid.clone(),
    };

    // The receipt from the LID of a message sent into the number chat lands on the number chat, by the number.
    let moved = rehomed(
        &store,
        vec![
            merged.clone(),
            receipt_for(
                &lid_chat,
                TargetAuthor::Own,
                &["3EB0D1", "3EB0D2"],
                &peer_lid,
            ),
        ],
    )
    .await
    .expect("the receipt moves");
    assert_eq!(
        moved,
        vec![
            merged.clone(),
            receipt_for(
                &number_chat,
                TargetAuthor::Own,
                &["3EB0D1", "3EB0D2"],
                &peer_pn
            ),
        ],
        "the events before it stay and the receipt names the ledger's chat and its peer"
    );

    // Targets the ledger holds in different chats of the same person split into one receipt per chat; the
    // message the ledger does not know (one sent from the phone) keeps the chat the receipt came in.
    let split = rehomed(
        &store,
        vec![receipt_for(
            &lid_chat,
            TargetAuthor::Own,
            &["3EB0D1", "3EB0FF"],
            &peer_lid,
        )],
    )
    .await
    .expect("one target moves");
    assert_eq!(
        split,
        vec![
            receipt_for(&number_chat, TargetAuthor::Own, &["3EB0D1"], &peer_pn),
            receipt_for(&lid_chat, TargetAuthor::Own, &["3EB0FF"], &peer_lid),
        ]
    );

    // Nothing moves: a message the ledger does not know, a chat that is already the ledger's, a ledger chat that
    // is ANOTHER PERSON by the mapping, a message of somebody else, a group.
    for (case, events) in [
        (
            "unknown to the ledger",
            vec![receipt_for(
                &lid_chat,
                TargetAuthor::Own,
                &["3EB0FF"],
                &peer_lid,
            )],
        ),
        (
            "already the ledger's chat",
            vec![receipt_for(
                &number_chat,
                TargetAuthor::Own,
                &["3EB0D1"],
                &peer_pn,
            )],
        ),
        (
            "the ledger's chat is another person",
            vec![receipt_for(
                &lid_chat,
                TargetAuthor::Own,
                &["3EB0D3"],
                &peer_lid,
            )],
        ),
        (
            "not our message",
            vec![receipt_for(
                &lid_chat,
                TargetAuthor::User(peer_lid.clone()),
                &["3EB0D1"],
                &peer_lid,
            )],
        ),
        ("no receipt", vec![merged.clone()]),
    ] {
        assert_eq!(rehomed(&store, events).await, None, "{case}");
    }
    owner.close();
}

/// Fix F13 (robustness LOW-2): the ledger is read by the local alone. A resend whose conversation the core re-keyed
/// (a `pn:` chat merged into its `lid:` form between the send and the resend) still finds the `acked` or `refused`
/// row: no frame again, and a 463 is not met twice. The edit window's original is found the same way.
#[tokio::test]
async fn an_outcome_is_found_under_another_form_of_the_chat_than_the_one_it_was_recorded_under() {
    let (_dir, owner) = open();
    let store = owner.store();
    let number = direct("pn", PEER_PN);
    put(
        &store,
        entry(&number, "3EB0E1", Kind::Message, None, acked(1_760_000_321)),
        NOW,
    )
    .await;
    put(
        &store,
        entry(&number, "3EB0E2", Kind::Message, None, Outcome::Refused),
        NOW,
    )
    .await;
    // The resend asks with the LID form: the row is the same message's.
    assert_eq!(
        looked(&store, "3EB0E1").await,
        Some(acked(1_760_000_321)),
        "an acked message is not found by its local alone"
    );
    assert_eq!(
        looked(&store, "3EB0E2").await,
        Some(Outcome::Refused),
        "a refused one is met twice"
    );
    let at = store
        .read(|conn| acked_at(conn, "3EB0E1"))
        .await
        .expect("read");
    assert_eq!(
        at,
        Some(1_760_000_321),
        "the original's time for the edit window"
    );
    owner.close();
}

/// A stored copy of an edit of `original`, or of a plain text, as upstream keeps it (`message_to_vec`).
fn copy_of(edit_of: Option<&str>) -> Vec<u8> {
    use waproto::whatsapp as wa;
    let mut message = wa::Message::default();
    match edit_of {
        Some(original) => {
            let mut key = wa::MessageKey::default();
            key.id = Some(original.to_owned());
            let mut protocol = wa::message::ProtocolMessage::default();
            protocol.key = waproto::buffa::MessageField::some(key);
            protocol.r#type = Some(wa::message::protocol_message::Type::MessageEdit);
            message.protocol_message = waproto::buffa::MessageField::some(protocol);
        }
        None => message.conversation = Some("synthetic text".to_owned()),
    }
    waproto::codec::message_to_vec(&message)
}

async fn copies(store: &KourierStore) -> Vec<String> {
    store
        .read(|conn| {
            crate::store::rows(
                conn,
                "left",
                "SELECT message_id FROM sent_messages ORDER BY message_id",
                [],
                |r| r.get(0),
            )
        })
        .await
        .expect("left")
}

/// Fix F18 (privacy LOW): an acked revoke takes the stored copy of EVERY edit of the revoked message, found by what
/// the copy holds, not by the ledger: an edit whose ack was never recorded (lost, or not committed before the
/// revoke) would keep the text the person deleted for up to 7,200 s. An edit whose ack is recorded after the
/// revoke's is taken in the zone that records it. A copy of another message's edit, and a plain one, stay.
/// Mutants: the scan by content removed; the late edit's take removed; every edit taken.
#[tokio::test]
async fn an_acked_revoke_takes_the_copy_of_an_edit_whose_ack_was_not_recorded_and_a_late_edit_ack_is_taken_too()
 {
    let (_dir, owner) = open();
    let store = owner.store();
    let conv = direct("pn", PEER_PN);
    let chat = format!("{PEER_PN}@s.whatsapp.net");
    let put_copy = |id: &'static str, payload: Vec<u8>| {
        let (store, chat) = (store.clone(), chat.clone());
        async move {
            store
                .write("seed", move |tx| {
                    exec(
                        tx,
                        "seed",
                        "INSERT INTO sent_messages(chat_jid, message_id, payload, created_at) VALUES (?1, ?2, ?3, 0)",
                        params![chat, id, payload],
                    )
                    .map(drop)
                })
                .await
                .expect("seed");
        }
    };
    // 3EB0F0 the original, 3EB0F1 an edit of it that never got an ack, 3EB0F3 an edit of ANOTHER message, 3EB0F4 a
    // plain text.
    put_copy("3EB0F0", copy_of(None)).await;
    put_copy("3EB0F1", copy_of(Some("3EB0F0"))).await;
    put_copy("3EB0F3", copy_of(Some("3EB0FF"))).await;
    put_copy("3EB0F4", copy_of(None)).await;
    put(
        &store,
        entry(&conv, "3EB0F0", Kind::Message, None, acked(1)),
        NOW,
    )
    .await;
    let done = put(
        &store,
        entry(&conv, "3EB0F8", Kind::Revoke, Some("3EB0F0"), acked(3)),
        NOW,
    )
    .await;
    assert_eq!(done.taken, 2, "the original's copy and the unacked edit's");
    assert_eq!(copies(&store).await, ["3EB0F3", "3EB0F4"]);

    // An edit sent after the revoke was acked: its copy is written by upstream when it sends, and its ack comes late.
    put_copy("3EB0F2", copy_of(Some("3EB0F0"))).await;
    let late = put(
        &store,
        entry(&conv, "3EB0F2", Kind::Edit, Some("3EB0F0"), acked(4)),
        NOW,
    )
    .await;
    assert_eq!(late.taken, 1, "the late edit's copy was not taken");
    // The ack of an edit of a message nobody revoked takes nothing.
    let other = put(
        &store,
        entry(&conv, "3EB0F3", Kind::Edit, Some("3EB0FF"), acked(5)),
        NOW,
    )
    .await;
    assert_eq!(other.taken, 0);
    assert_eq!(copies(&store).await, ["3EB0F3", "3EB0F4"]);
    owner.close();
}
