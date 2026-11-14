//! The migration of message search (G1b, `_search`, found by its NAME so that the number it
//! gets at the landing touches nothing here), after the fix round (the index in its own file):
//! core.db gets only the LOG (`search_queue`, AUTOINCREMENT), the three triggers and the
//! EPOCH; the index itself lives in `search.db`, which the search task makes. No row of
//! `messages` is touched. At the head, the three triggers exist (a later migration that
//! rebuilds `messages` must create them again: `.claude/rules/core.md`). The last test reads
//! search.db's own schema (contentless, columns only, no prefix index).

use kourier_core::testing::{IngestHarness, search};
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{MsgIdScope, NetworkKind};

/// The step of this change, by the end of its name.
const STEP: &str = "_search";

/// The index of the step in `MIGRATIONS` (0-based).
fn step_index() -> usize {
    kourier_core::MIGRATIONS
        .steps
        .iter()
        .position(|m| m.name.ends_with(STEP))
        .expect("the step is registered")
}

fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

fn setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM settings WHERE scope = 'search' AND key = ?1",
        [key],
        |r| r.get(0),
    )
    .optional()
    .expect("setting")
}

fn messages(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT * FROM messages ORDER BY id")
        .expect("prepare")
        .query_map([], |r| {
            let n = r.as_ref().column_count();
            (0..n)
                .map(|i| r.get_ref(i).map(|v| format!("{v:?}")))
                .collect::<Result<Vec<_>, _>>()
                .map(|v| v.join("|"))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

fn message(id: i64, text: &str) -> String {
    format!(
        "INSERT INTO messages(id, account_id, conversation_id, net_scope, net_author, net_local,
             origin, sort_key, sent_at, received_at, body_kind, body)
           VALUES ({id}, 1, 1, 0, 'aci:b', '{id}', 'Remote', {id}0, {id}0, {id}0, 'Text',
             '{{\"Text\":{{\"body\":\"{text}\"}}}}');"
    )
}

fn triggers(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'messages'
           AND name LIKE 'messages_search_%' ORDER BY name",
    )
    .expect("prepare")
    .query_map([], |r| r.get(0))
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("names")
}

/// Whether core.db holds a table or virtual table named `name`.
fn has_table(conn: &Connection, name: &str) -> bool {
    count(
        conn,
        &format!("SELECT count(*) FROM sqlite_master WHERE name = '{name}'"),
    ) > 0
}

fn at_head(dir: &tempfile::TempDir, name: &str) -> Connection {
    let mut conn =
        kourier_db::open(&dir.path().join(name), &kourier_db::DbKey::from([6; 32])).expect("open");
    kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    conn
}

/// An epoch is 16 random bytes in lower-case hex.
fn is_epoch(v: &str) -> bool {
    v.len() == 32
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[test]
fn the_migration_adds_the_log_and_the_epoch_and_touches_no_row() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("core.db");
    let key = kourier_db::DbKey::from([6; 32]);
    let mut conn = kourier_db::open(&path, &key).expect("open");
    let at = step_index();
    let steps: &'static [kourier_db::Migration] = kourier_core::MIGRATIONS.steps;
    let before = kourier_db::Migrations::new("core", &steps[..at]);
    kourier_db::migrate(&mut conn, &before).expect("the schema before the step");
    conn.execute_batch(&format!(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:b', 'Direct');
         {}{}{}",
        message(1, "oldoneq"),
        message(2, "oldtwoq"),
        message(7, "oldsevenq"),
    ))
    .expect("rows");
    let rows = messages(&conn);
    let out = kourier_db::migrate(&mut conn, &kourier_core::MIGRATIONS).expect("migrate");
    assert_eq!(
        (out.from, out.to),
        (
            u32::try_from(at).expect("small"),
            kourier_core::MIGRATIONS.latest()
        )
    );
    assert_eq!(messages(&conn), rows, "no row of messages is touched");
    let epoch = setting(&conn, "epoch").expect("the epoch is written");
    assert!(is_epoch(&epoch), "an epoch of 16 random bytes");
    assert_eq!(search::epoch(&conn).expect("epoch"), epoch);
    // Nothing of the index is in core.db: the search task makes search.db.
    assert!(!has_table(&conn, "messages_fts"), "no index in core.db");
    assert_eq!(setting(&conn, "floor"), None, "the floor is search.db's");
    assert_eq!(count(&conn, "SELECT count(*) FROM search_queue"), 0);
    assert_eq!(triggers(&conn).len(), 3);
    // A row written after the step is logged, under the first seq.
    conn.execute_batch(&message(8, "newq")).expect("a new row");
    assert_eq!(
        count(
            &conn,
            "SELECT count(*) FROM search_queue WHERE message_id = 8 AND seq = 1"
        ),
        1
    );
    assert_eq!(search::position(&conn).expect("position"), 1);
}

/// AUTOINCREMENT: a seq is NEVER used twice, even after the log was emptied (the prune deletes
/// what search.db applied; a reused seq at or below search.db's `applied` would be skipped for
/// good) and after the file is closed and opened again.
#[test]
fn a_seq_is_never_used_twice_even_after_the_log_was_emptied() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = at_head(&dir, "core.db");
    conn.execute_batch(
        "INSERT INTO accounts(id, uuid, network, state, created_at)
           VALUES (1, '6b3f0c1e-1d2a-4c5b-9e8f-0a1b2c3d4e5f', 'fake', 'Active', 0);
         INSERT INTO conversations(id, account_id, conv_key, kind) VALUES (1, 1, 'aci:b', 'Direct');",
    )
    .expect("chat");
    for id in 1..=3 {
        conn.execute_batch(&message(id, "seqq")).expect("row");
    }
    assert_eq!(search::log_seqs(&conn).expect("seqs"), [1, 2, 3]);
    // The prune of everything applied: the log is empty.
    conn.execute("DELETE FROM search_queue WHERE seq <= 3", [])
        .expect("prune");
    conn.execute_batch(&message(4, "seqq")).expect("row");
    assert_eq!(
        search::log_seqs(&conn).expect("seqs"),
        [4],
        "an emptied log goes on above its last seq"
    );
    conn.execute("DELETE FROM search_queue", []).expect("prune");
    drop(conn);
    let conn = at_head(&dir, "core.db");
    conn.execute_batch(&message(5, "seqq")).expect("row");
    assert_eq!(
        search::log_seqs(&conn).expect("seqs"),
        [5],
        "and so after a restart"
    );
    assert_eq!(search::position(&conn).expect("position"), 5);
}

#[test]
fn every_core_db_gets_its_own_epoch() {
    let dir = tempfile::tempdir().expect("tempdir");
    let one = setting(&at_head(&dir, "one.db"), "epoch").expect("epoch");
    let two = setting(&at_head(&dir, "two.db"), "epoch").expect("epoch");
    assert!(is_epoch(&one) && is_epoch(&two));
    assert_ne!(one, two, "the epoch names one core.db");
    // A new epoch replaces it (a wholesale rewrite would write one).
    let conn = at_head(&dir, "one.db");
    search::new_epoch(&conn).expect("new epoch");
    let three = search::epoch(&conn).expect("epoch");
    assert!(is_epoch(&three));
    assert_ne!(three, one);
}

/// At the head of the migrations the three triggers stand on `messages` (a later step that
/// rebuilds the table and forgets them fails here), and the log is AUTOINCREMENT.
#[test]
fn the_three_triggers_exist_at_the_head() {
    let dir = tempfile::tempdir().expect("tempdir");
    let conn = at_head(&dir, "core.db");
    assert_eq!(
        triggers(&conn),
        [
            "messages_search_delete",
            "messages_search_insert",
            "messages_search_update"
        ]
    );
    let log: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'search_queue'",
            [],
            |r| r.get(0),
        )
        .expect("the log");
    assert!(log.contains("AUTOINCREMENT"), "{log}");
    assert!(!has_table(&conn, "messages_fts"), "no index in core.db");
}

/// search.db's own schema: contentless, columns only (positions are off, so a phrase is
/// refused), no prefix index (decisions M6), the `indexed` map and `meta` at this version.
#[test]
fn search_db_is_contentless_with_columns_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let h = IngestHarness::open(
        &dir.path().join("core.db"),
        &kourier_db::DbKey::from([6; 32]),
        NetworkKind::Fake,
        MsgIdScope::AccountAuthor,
        &[],
    )
    .expect("harness");
    let s = h.search_db();
    let options: String = s
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'messages_fts'",
            [],
            |r| r.get(0),
        )
        .expect("table");
    assert!(options.contains("content = ''") && options.contains("detail = column"));
    assert!(
        !options.contains("prefix"),
        "no prefix= option (decisions M6)"
    );
    let err = s
        .query_row(
            "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH '\"a b\"'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .expect_err("no phrases with detail=column");
    assert!(err.to_string().contains("phrase"), "{err}");
    assert!(has_table(s, "indexed") && has_table(s, "meta"));
    assert_eq!(
        search::meta(s, "version").expect("version"),
        search::SEARCH_FILE_VERSION
    );
    assert_eq!(
        search::meta(s, "norm").expect("norm"),
        i64::from(search::NORMALIZER_VERSION)
    );
    assert_eq!(
        search::meta(s, "floor").expect("floor"),
        0,
        "an empty core.db"
    );
    assert_eq!(search::meta(s, "dead").expect("dead"), 0);
}
