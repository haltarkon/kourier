//! What an identity merge costs (step L2 of the link feedback, `target/link-feedback/tail.md`
//! F2): the first Storage Service sync after a link tells one `IdentityMerged` (a phone
//! number's user into the ACI) for every contact with a number. Before core.db schema 14 each
//! merge walked all of the account's messages about nine times, ~2 µs per contact and message
//! in a release build: 300 contacts and 20,000 messages took 11 s in one write transaction,
//! while the link page waited. A merge must cost what the alias owns.

use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_core::testing::IngestHarness;
use kourier_fake::scripts::remote;
use kourier_model::{
    ConvKey, MsgIdScope, MsgTarget, NetworkKind, ReceiptKind, TargetAuthor, UserId,
};

const ME: &str = "aci:00000000-0000-4000-8000-0000000000aa";

fn uid(s: &str) -> UserId {
    UserId::parse(s).expect("user id")
}

fn aci(n: usize) -> UserId {
    uid(&format!(
        "aci:00000000-0000-4000-8000-{:012x}",
        0x10_0000 + n
    ))
}

fn number(n: usize) -> UserId {
    uid(&format!("e164:+1555{n:07}"))
}

/// `chats` chats of `per_chat` incoming texts each, from the chats' ACI users.
fn history(h: &mut IngestHarness, chats: usize, per_chat: usize) {
    for c in 0..chats {
        let by = aci(1_000 + c);
        let conv = ConvKey::Direct(by.clone());
        let all: Vec<IncomingMessage> = (0..per_chat)
            .map(|i| {
                let at = 1_700_000_000_000 + i64::try_from(c * per_chat + i).expect("time");
                remote(&conv, &by, &at.to_string(), at, "text")
            })
            .collect();
        for chunk in all.chunks(kourier_backend_api::MAX_HISTORY_BATCH) {
            h.apply(vec![BackendEvent::HistoryBatch {
                conv: conv.clone(),
                messages: chunk.to_vec(),
                has_more: false,
                progress: None,
            }])
            .expect("history");
        }
    }
}

/// What the account keeps besides its messages: the keys of 45 days of expired messages (the
/// sweeper remembers them for that long, `expiry::REMEMBER`; ~1,000 a day in busy chats with
/// a disappearing timer) and parked modifiers (reactions and edits whose message is not here).
/// None of it belongs to the people the merges name, but a merge that cannot find its rows by
/// an index walks all of them: `expired_messages` and `pending_modifiers` were looked at by
/// `net_author = ? OR conv_key = ?`, which the indexes of the first version of schema 14 did
/// not serve.
fn expired_and_parked(h: &IngestHarness, days: usize, per_day: usize, parked: usize) {
    let conn = h.connection();
    let account = h.account().0;
    let tx = conn.unchecked_transaction().expect("transaction");
    {
        let mut expired = tx
            .prepare(
                "INSERT INTO expired_messages(account_id, net_scope, conv_key, net_author,
                   net_local, expired_at) VALUES (?1, 0, ?2, ?3, ?4, ?5)",
            )
            .expect("prepare");
        for d in 0..days {
            for i in 0..per_day {
                // Fifty chats' worth of other people; never an alias the merges name.
                let by = aci(5_000 + (d * per_day + i) % 50);
                let key = ConvKey::Direct(by.clone()).encode();
                let local = format!("{}", 1_600_000_000_000_i64 + (d * per_day + i) as i64);
                let at = h.now() - (days - d) as i64 * 86_400_000;
                expired
                    .execute((account, key, by.as_str(), local, at))
                    .expect("expired");
            }
        }
        let mut park = tx
            .prepare(
                "INSERT INTO pending_modifiers(account_id, conv_key, target_author, target_local,
                   kind, payload, dedup, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, 'Reaction', '{}', ?5, ?6, ?7)",
            )
            .expect("prepare");
        for i in 0..parked {
            let by = aci(6_000 + i % 50);
            let key = ConvKey::Direct(by.clone()).encode();
            park.execute((
                account,
                key,
                by.as_str(),
                format!("{}", 1_650_000_000_000_i64 + i as i64),
                format!("parked-{i}"),
                h.now(),
                h.now() + 7 * 86_400_000,
            ))
            .expect("parked");
        }
    }
    tx.commit().expect("commit");
}

/// 300 merges of number-users that own nothing (the first Storage Service sync of a fresh
/// link) and of three that own a chat, over 20,000 messages, 45 days of expired keys and
/// parked modifiers: well under a second where the walks took 11 s (release) and about four
/// times that in a debug build.
#[test]
fn merges_cost_what_the_alias_owns() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([7; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    history(&mut h, 20, 1_000);
    expired_and_parked(&h, 45, 1_000, 5_000);
    // Three number-users that wrote before their ACI was known: their chats move.
    for n in 0..3 {
        let by = number(n);
        let conv = ConvKey::Direct(by.clone());
        let at = 1_800_000_000_000 + i64::try_from(n).expect("time");
        h.apply(vec![BackendEvent::Message(remote(
            &conv,
            &by,
            &at.to_string(),
            at,
            "text",
        ))])
        .expect("message");
    }
    let merges: Vec<BackendEvent> = (0..300)
        .map(|n| BackendEvent::IdentityMerged {
            from: number(n),
            into: aci(n),
        })
        .collect();
    let started = Instant::now();
    h.apply(merges).expect("merges");
    let took = started.elapsed();
    eprintln!(
        "300 merges over 20,000 messages, 45,000 expired keys, 5,000 parked modifiers: {took:?}"
    );
    let budget = if cfg!(debug_assertions) {
        Duration::from_secs(1)
    } else {
        Duration::from_millis(250)
    };
    assert!(
        took < budget,
        "300 merges over 20,000 messages and 45 days of expired keys took {took:?} (budget {budget:?})"
    );
    // The three chats were re-keyed to their ACI, with their messages.
    let moved: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE net_author LIKE 'aci:%' AND net_author IN
               (SELECT user_id FROM identity_alias WHERE alias LIKE 'e164:%')",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(moved, 3);
    let left: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM messages WHERE net_author LIKE 'e164:%'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(left, 0);
    let aliases: i64 = h
        .connection()
        .query_row("SELECT count(*) FROM identity_alias", [], |r| r.get(0))
        .expect("count");
    assert_eq!(
        aliases, 300,
        "every merge is recorded, also one with nothing to move"
    );
}

/// A receipt for an own message that is not here, from `by`: it is parked (`kind` 'Receipts').
fn parked_receipt(n: usize, by: &UserId) -> BackendEvent {
    BackendEvent::Receipts {
        targets: vec![MsgTarget {
            conv: None,
            author: TargetAuthor::Own,
            local: Arc::from(format!("{}", 1_900_000_000_000_i64 + n as i64)),
            alternates: Vec::new(),
        }],
        by: by.clone(),
        kind: ReceiptKind::Delivered,
        at: 1_900_000_100_000 + n as i64,
    }
}

/// The receipts that wait (their message is not here, or their sender is no recipient of it)
/// are looked at again after identity merges, because a merge can make the sender a
/// recipient. One transaction of 300 merges looks at each once, not once per merge: every
/// look decodes every parked receipt of the account, 300 times 40 here.
#[test]
fn parked_receipts_are_looked_at_once_for_all_merges_of_a_transaction() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([8; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[uid(ME)],
    )
    .expect("harness");
    history(&mut h, 2, 100);
    // One number-user with a chat, so a merge takes the long path too.
    let by = number(0);
    let at = 1_800_000_000_000_i64;
    h.apply(vec![BackendEvent::Message(remote(
        &ConvKey::Direct(by.clone()),
        &by,
        &at.to_string(),
        at,
        "text",
    ))])
    .expect("message");
    let parked = 40;
    h.apply(
        (0..parked)
            .map(|n| parked_receipt(n, &aci(7_000 + n)))
            .collect(),
    )
    .expect("receipts");
    let waiting: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM pending_modifiers WHERE kind = 'Receipts'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(waiting, parked as i64, "all parked");

    let before = IngestHarness::receipt_retry_decodes();
    h.apply(
        (0..300)
            .map(|n| BackendEvent::IdentityMerged {
                from: number(n),
                into: aci(n),
            })
            .collect(),
    )
    .expect("merges");
    let decoded = IngestHarness::receipt_retry_decodes() - before;
    assert_eq!(
        decoded, parked as u64,
        "300 merges in one transaction look at each of the {parked} parked receipts once"
    );
    // They still wait (their messages never came): looked at, not lost.
    let still: i64 = h
        .connection()
        .query_row(
            "SELECT count(*) FROM pending_modifiers WHERE kind = 'Receipts'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(still, parked as i64);

    // A transaction without a merge does not look at them at all.
    let before = IngestHarness::receipt_retry_decodes();
    h.apply(vec![BackendEvent::Message(remote(
        &ConvKey::Direct(aci(1_000)),
        &aci(1_000),
        "1900000000999",
        1_900_000_000_999,
        "later",
    ))])
    .expect("message");
    assert_eq!(IngestHarness::receipt_retry_decodes(), before);
}
