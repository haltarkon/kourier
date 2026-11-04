//! core.db schema 16 (K-B1, `0016_message_key_author.sql`): the partial unique key of the third
//! message-id scope and the table of clears by time, on a profile that schema 15 really left
//! behind, through the real migration runner.

use kourier_db::rusqlite::{Connection, ErrorCode};

const ALICE: &str = "pn:15550000001";
const BOB: &str = "pn:15550000002";

fn insert_message(
    conn: &Connection,
    conv: i64,
    scope: i64,
    author: &str,
    local: &str,
) -> kourier_db::rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO messages(account_id, conversation_id, net_scope, net_author, net_local,
           origin, sort_key, sent_at, received_at, body_kind, body, read_state, history, silent)
         VALUES (1, ?1, ?2, ?3, ?4, 'Remote', 1000, 100, 100, 'Text', '{}', 'Read', 0, 0)",
        (conv, scope, author, local),
    )
}

fn refused_as_a_duplicate(r: kourier_db::rusqlite::Result<usize>) -> bool {
    matches!(
        r,
        Err(kourier_db::rusqlite::Error::SqliteFailure(e, _))
            if e.code == ErrorCode::ConstraintViolation
    )
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

/// T8: a schema-15 file with rows of scopes 0 and 1 migrates to 16 with those rows unchanged;
/// the partial index refuses a second `(conversation, author, local)` of scope 2 and nothing
/// else; `conversation_cuts` is there with its primary key and goes with its account.
#[test]
fn schema_16_adds_the_per_sender_key_and_the_cuts_and_changes_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([6; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let v15 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..15]);
    kourier_db::migrate(&mut conn, &v15).expect("schema 15");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind)
           VALUES (1, 1, 'g:gv2:one', 'Group'), (2, 1, 'g:gv2:two', 'Group');",
    )
    .expect("accounts and chats");
    // What schema 15 holds: Signal's account-wide key (scope 0), the per-chat key (scope 1,
    // also the call lines of every network).
    insert_message(&conn, 1, 0, ALICE, "100").expect("scope 0");
    insert_message(&conn, 1, 1, ALICE, "100").expect("scope 1");
    insert_message(&conn, 2, 1, "", "100").expect("scope 1, another chat");
    let before: Vec<(i64, i64, String, String)> = {
        let mut q = conn
            .prepare("SELECT id, net_scope, net_author, net_local FROM messages ORDER BY id")
            .expect("prepare");
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(before.len(), 3);

    // Through step 16 only: a later migration must not change what this test says about 16.
    let v16 = kourier_db::Migrations::new("core", &kourier_core::MIGRATIONS.steps[..16]);
    let out = kourier_db::migrate(&mut conn, &v16).expect("migrate");
    assert_eq!((out.from, out.to), (15, 16));

    let after: Vec<(i64, i64, String, String)> = {
        let mut q = conn
            .prepare("SELECT id, net_scope, net_author, net_local FROM messages ORDER BY id")
            .expect("prepare");
        q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(after, before, "no row is converted");

    // The key: partial on scope 2, unique per (conversation, author, local).
    let index_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'messages_key_conv_author'",
            [],
            |r| r.get(0),
        )
        .expect("the index exists");
    assert!(
        index_sql.contains("UNIQUE") && index_sql.contains("WHERE net_scope = 2"),
        "{index_sql}"
    );
    insert_message(&conn, 1, 2, ALICE, "3EB0X1").expect("alice's id");
    insert_message(&conn, 1, 2, BOB, "3EB0X1").expect("bob may use the same id");
    insert_message(&conn, 1, 2, "", "3EB0X1").expect("so may we");
    insert_message(&conn, 2, 2, ALICE, "3EB0X1").expect("and alice in another chat");
    assert!(
        refused_as_a_duplicate(insert_message(&conn, 1, 2, ALICE, "3EB0X1")),
        "one (conversation, author, local) of scope 2 once"
    );
    // The other scopes' keys are not touched by it: a scope-2 row never clashes with the rows
    // of scopes 0 and 1 that share its numbers, and those two keys still refuse as before.
    insert_message(&conn, 1, 2, ALICE, "100").expect("a scope-2 row beside scope 0 and 1 rows");
    assert!(refused_as_a_duplicate(insert_message(
        &conn, 1, 1, BOB, "100"
    )));
    assert!(refused_as_a_duplicate(insert_message(
        &conn, 1, 0, ALICE, "100"
    )));

    // The cuts: one row per (account, conversation key), gone with the account. `claimed_ms` is
    // the claim the cut came from; the upsert the core uses (`clear::STORE_CUT`) raises the cut
    // and takes the claim only for a LARGER claim.
    conn.execute(
        "INSERT INTO conversation_cuts(account_id, conv_key, before_ms, claimed_ms)
         VALUES (1, 'd:pn:1', 5, 5)",
        [],
    )
    .expect("a cut");
    assert!(refused_as_a_duplicate(conn.execute(
        "INSERT INTO conversation_cuts(account_id, conv_key, before_ms, claimed_ms)
         VALUES (1, 'd:pn:1', 9, 9)",
        [],
    )));
    assert!(
        refused_as_a_duplicate(conn.execute(
            "INSERT INTO conversation_cuts(account_id, conv_key, before_ms) VALUES (1, 'd:pn:2', 9)",
            [],
        )),
        "a cut without its claim is refused (NOT NULL)"
    );
    let upsert = |before: i64, claim: i64| {
        conn.execute(
            "INSERT INTO conversation_cuts(account_id, conv_key, before_ms, claimed_ms)
             VALUES (1, 'd:pn:1', ?1, ?2)
             ON CONFLICT(account_id, conv_key) DO UPDATE SET
               before_ms = max(before_ms, excluded.before_ms), claimed_ms = excluded.claimed_ms
             WHERE excluded.claimed_ms > claimed_ms",
            [before, claim],
        )
        .expect("an upsert")
    };
    assert_eq!(upsert(3, 3), 0, "a smaller claim changes nothing");
    assert_eq!(upsert(5, 5), 0, "the same claim again changes nothing");
    assert_eq!(count(&conn, "SELECT before_ms FROM conversation_cuts"), 5);
    assert_eq!(
        upsert(7, 20),
        1,
        "a larger claim raises the cut and the claim"
    );
    assert_eq!(count(&conn, "SELECT before_ms FROM conversation_cuts"), 7);
    assert_eq!(count(&conn, "SELECT claimed_ms FROM conversation_cuts"), 20);
    assert_eq!(upsert(2, 30), 1, "a larger claim never lowers the cut");
    assert_eq!(count(&conn, "SELECT before_ms FROM conversation_cuts"), 7);
    assert_eq!(count(&conn, "SELECT claimed_ms FROM conversation_cuts"), 30);
    conn.execute("DELETE FROM accounts WHERE id = 1", [])
        .expect("remove the account");
    assert_eq!(count(&conn, "SELECT count(*) FROM conversation_cuts"), 0);
}
