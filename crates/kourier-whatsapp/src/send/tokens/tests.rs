//! The token gate (W5a, D12): the rule and what it reads from the store. Synthetic ids only (each tail of 8 digits a
//! substring of no other value).

use kourier_db::DbKey;
use kourier_model::{ConvKey, UserId};

use super::{Facts, TOKEN_WAIT_BOUND, decide, facts};
use crate::error::sql;
use crate::store::{StoreOwner, keep_history_token};

const PEER_PN: &str = "15550148203967";
const PEER_LID: &str = "77700159371046";
const NOW: i64 = 1_761_600_000_000;
const MINUTE: i64 = 60_000;

fn dm(scheme: &str, value: &str) -> ConvKey {
    ConvKey::Direct(UserId::from_parts(scheme, value).expect("a synthetic user"))
}

/// The rule: only a direct chat with no token waits, only while the push runs, only before the bound counted from
/// the EARLIER of the first notice and this run's link. Mutants: the bound counted from the later of the two; the
/// token not looked at.
#[test]
fn only_a_direct_chat_with_no_token_waits_while_the_push_runs_within_the_bound() {
    let running = Facts {
        has_token: false,
        first_notice_ms: Some(NOW - 10 * MINUTE),
        push_running: true,
    };
    assert!(decide(true, running, None, NOW), "the first sends wait");
    assert!(!decide(false, running, None, NOW), "a group never waits");
    assert!(
        !decide(
            true,
            Facts {
                has_token: true,
                ..running
            },
            None,
            NOW
        ),
        "a chat with a token never waits"
    );
    assert!(
        !decide(
            true,
            Facts {
                push_running: false,
                ..running
            },
            None,
            NOW
        ),
        "a push that is over holds nothing"
    );
    let bound = i64::try_from(TOKEN_WAIT_BOUND.as_millis()).expect("ms");
    assert_eq!(bound, 30 * MINUTE);
    assert!(
        !decide(
            true,
            Facts {
                first_notice_ms: Some(NOW - bound),
                ..running
            },
            None,
            NOW
        ),
        "30 minutes after the link the send goes bare"
    );
    assert!(
        !decide(
            true,
            Facts {
                first_notice_ms: None,
                ..running
            },
            None,
            NOW
        ),
        "no link time known: no wait"
    );
    assert!(
        decide(
            true,
            Facts {
                first_notice_ms: None,
                ..running
            },
            Some(NOW - MINUTE),
            NOW
        ),
        "this run's link before any notice"
    );
    assert!(
        !decide(
            true,
            Facts {
                first_notice_ms: Some(NOW - MINUTE),
                ..running
            },
            Some(NOW - bound - MINUTE),
            NOW
        ),
        "the bound counts from the earlier of the notice and the link"
    );
}

/// What the gate reads: a token with bytes under any form of the chat (a byte-less placeholder is none), the first
/// notice, and the push: running until a bootstrap chunk at 100 % is delivered, and again while a chunk waits.
/// Mutant: `live_floor` not read.
#[tokio::test]
async fn the_gate_reads_the_chats_token_the_first_notice_and_the_push() {
    let dir = tempfile::tempdir().expect("dir");
    let owner = StoreOwner::open(
        &dir.path().join("backend.db"),
        &DbKey::from([12; 32]),
        "wa-tokens",
    )
    .expect("open");
    let store = owner.store();
    let read = |conv: ConvKey| {
        let store = store.clone();
        async move {
            store
                .read(move |conn| facts(conn, &conv))
                .await
                .expect("facts")
        }
    };
    let pn = dm("pn", PEER_PN);
    assert_eq!(
        read(pn.clone()).await,
        Facts {
            has_token: false,
            first_notice_ms: None,
            push_running: true,
        }
    );
    let exec = |statement: &'static str| {
        let store = store.clone();
        async move {
            store
                .write("set up", move |tx| {
                    tx.execute_batch(statement).map_err(sql("set up"))
                })
                .await
                .expect("write");
        }
    };
    exec(
        "INSERT INTO history_announced(msg_id, announced_at) VALUES ('n2', 1761599000000), ('n1', 1761598000000); \
         INSERT INTO history_chunks(chunk, sync_type, chunk_order, progress) VALUES (1, 0, 1, 100);",
    )
    .await;
    assert_eq!(
        read(pn.clone()).await,
        Facts {
            has_token: false,
            first_notice_ms: Some(1_761_598_000_000),
            push_running: false,
        },
        "the bootstrap at 100 % ends the push"
    );
    exec("UPDATE history_cursor SET live_floor = 1761599500000 WHERE id = 1;").await;
    assert!(read(pn.clone()).await.push_running, "a chunk still waits");
    // A placeholder (the sender side's, no bytes) is no token; a token under the LID counts for the number's chat.
    let (lid_value, pn_value) = (PEER_LID, PEER_PN);
    store
        .write("tokens", move |tx| {
            tx.execute(
                "INSERT INTO tc_tokens(jid, token, token_timestamp, sender_timestamp) VALUES (?1, x'', 5, 5)",
                [pn_value],
            )
            .map_err(sql("placeholder"))?;
            tx.execute(
                "INSERT INTO lid_pn_mapping(lid, phone, created_at, updated_at, learning_source) \
                 VALUES (?1, ?2, 1, 1, 'other')",
                [lid_value, pn_value],
            )
            .map_err(sql("pair"))?;
            Ok(())
        })
        .await
        .expect("write");
    assert!(
        !read(pn.clone()).await.has_token,
        "a placeholder is no token"
    );
    store
        .write("token", move |tx| {
            keep_history_token(tx, lid_value, &[9, 9], 1_761_500_000, None)
        })
        .await
        .expect("write");
    assert!(read(pn).await.has_token, "the LID's token is the chat's");
    assert!(read(dm("lid", PEER_LID)).await.has_token);
    assert_eq!(
        read(ConvKey::Group("wag:120363000064810297".into())).await,
        Facts::default(),
        "a group reads nothing"
    );
}
