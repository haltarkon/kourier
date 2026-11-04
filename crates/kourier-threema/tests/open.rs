//! Opening an account's `backend.db`: the schema, the wrong key, a newer or foreign file.

use std::path::Path;

use kourier_db::rusqlite::{Connection, params};
use kourier_threema::OpenError;
use kourier_threema::store::open;
use sha2::{Digest, Sha256};

use crate::support::{TEST_KEY, db_path, key, temp_store};

/// A read-write connection to the file, as a test's own view of what the store wrote.
fn raw(dir: &Path) -> Connection {
    kourier_db::open(&db_path(dir), &key()).expect("raw open")
}

/// The hash of every file in the directory (`backend.db`, `-wal`, `-shm`), by name.
fn files_hash(dir: &Path) -> String {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .expect("read dir")
        .map(|e| e.expect("entry").path())
        .collect();
    names.sort();
    let mut hasher = Sha256::new();
    for path in names {
        hasher.update(path.file_name().expect("name").as_encoded_bytes());
        hasher.update(std::fs::read(&path).expect("read"));
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The version of `component` in the file, `None` when it has no row.
fn version(conn: &Connection, component: &str) -> Option<i64> {
    conn.query_row(
        "SELECT version FROM schema_version WHERE component = ?1",
        params![component],
        |r| r.get(0),
    )
    .ok()
}

/// The tables of schema 4 that start empty (1: the identity and the nonce stores; 2: the join's import;
/// 3: the deferred envelopes; 4: what is being sent) and the outbox. Schema 3's `deferred_total` starts with
/// its one row.
const TABLES: [&str; 14] = [
    "identity",
    "csp_nonce",
    "d2d_nonce",
    "contact",
    "grp",
    "dist_list",
    "own_profile",
    "settings",
    "held_blob",
    "join_ready",
    "deferred",
    "send_task",
    "send_receiver",
    "core_outbox",
];

fn has_table(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![table],
        |r| r.get(0),
    )
    .expect(table)
}

/// The kept sum of `deferred` (schema 3): one row, `(1, 0, 0)` in a new file.
fn deferred_total_rows(conn: &Connection) -> Vec<(i64, i64, i64)> {
    let mut stmt = conn
        .prepare("SELECT id, row_count, byte_sum FROM deferred_total")
        .expect("deferred_total");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows")
}

/// A new file is at schema 4 with its tables, all empty but the kept sum's one row `(1, 0, 0)`, and
/// `core_outbox` at kourier-db's latest.
#[test]
fn a_new_file_is_at_schema_4_with_empty_tables() {
    let t = temp_store();
    let dir = t.close();
    let conn = raw(dir.path());
    let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
    assert_eq!(version(&conn, "threema"), Some(4), "threema schema");
    assert_eq!(version(&conn, "core_outbox"), Some(outbox), "outbox schema");
    for table in TABLES {
        assert!(has_table(&conn, table), "table {table} is missing");
        let rows: i64 = conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .expect(table);
        assert_eq!(rows, 0, "{table} starts empty");
    }
    assert_eq!(
        deferred_total_rows(&conn),
        vec![(1, 0, 0)],
        "deferred_total starts as its one row of nothing"
    );
}

/// Opening the same file again, twice, leaves ONE schema: one row per component at the same
/// versions, the same tables.
#[test]
fn opening_twice_gives_one_schema() {
    let dir = temp_store().close();
    for _ in 0..2 {
        drop(open(&db_path(dir.path()), &key()).expect("reopen"));
    }
    let conn = raw(dir.path());
    let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
    assert_eq!(version(&conn, "threema"), Some(4));
    assert_eq!(version(&conn, "core_outbox"), Some(outbox));
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM schema_version", [], |r| r.get(0))
        .expect("count");
    assert_eq!(rows, 2, "exactly the two components");
    for table in TABLES {
        assert!(has_table(&conn, table), "table {table} is missing");
    }
    assert_eq!(
        deferred_total_rows(&conn),
        vec![(1, 0, 0)],
        "a second open wrote the kept sum's row again"
    );
}

/// Schema 3's migrations, as a file of TH5b has them.
const SCHEMA_3: kourier_db::Migrations = kourier_db::Migrations::new(
    "threema",
    &[
        kourier_db::migration!(1, "0001_init"),
        kourier_db::migration!(2, "0002_join"),
        kourier_db::migration!(3, "0003_receive"),
    ],
);

/// A file of schema 3 (with a nonce, a contact and a deferred row) opens, is at schema 4 after, keeps its rows, the send
/// tables and their three indexes are there and empty. The last two columns of `send_task` are the BLOBs (critic L2),
/// and a second open changes nothing.
#[test]
fn a_schema_3_file_migrates_to_schema_4_with_the_send_tables_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let mut conn = kourier_db::open(&db_path(dir.path()), &key()).expect("open");
        kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
        kourier_db::migrate(&mut conn, &SCHEMA_3).expect("schema 3");
        assert_eq!(version(&conn, "threema"), Some(3));
        assert!(
            !has_table(&conn, "send_task") && !has_table(&conn, "send_receiver"),
            "a send table exists at schema 3"
        );
        conn.execute_batch(
            "INSERT INTO csp_nonce (hash) VALUES (zeroblob(32));
             INSERT INTO contact (identity, sync) VALUES ('S1MIGR08', X'0a00');
             INSERT INTO deferred (source, kind, map_version, server_at, flags, received_at, envelope)
               VALUES ('reflected', 'envelope', 1, 5, 0, 6, X'0a00');",
        )
        .expect("schema 3's rows");
    }
    for _ in 0..2 {
        drop(open(&db_path(dir.path()), &key()).expect("open migrates"));
        let conn = raw(dir.path());
        assert_eq!(version(&conn, "threema"), Some(4));
        for (table, rows) in [
            ("csp_nonce", 1),
            ("contact", 1),
            ("deferred", 1),
            ("send_task", 0),
            ("send_receiver", 0),
        ] {
            let n: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .expect(table);
            assert_eq!(n, rows, "{table}");
        }
        assert_eq!(deferred_total_rows(&conn), vec![(1, 1, 2)], "the kept sum");
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'send_task' \
                 AND sql IS NOT NULL ORDER BY name",
            )
            .expect("the indexes");
        let indexes: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            indexes,
            ["send_by_local", "send_task_done", "send_task_open"]
        );
        let mut stmt = conn
            .prepare("SELECT name FROM pragma_table_info('send_task') ORDER BY cid")
            .expect("the columns");
        let columns: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert_eq!(
            columns,
            [
                "id",
                "origin",
                "kind",
                "local",
                "message_id",
                "conversation",
                "created_at",
                "state",
                "has_update",
                "sent_at",
                "bytes",
                "attempts",
                "queued_at",
                "finished_at",
                "reflect_env",
                "update_env"
            ],
            "the frozen columns, the BLOBs last"
        );
    }
}

#[test]
fn a_wrong_key_fails_and_the_files_do_not_change() {
    let dir = temp_store().close();
    let before = files_hash(dir.path());
    let mut wrong = TEST_KEY;
    wrong[0] ^= 1;
    let err = open(&db_path(dir.path()), &wrong.into()).err();
    assert!(
        matches!(err, Some(OpenError::WrongKey)),
        "a wrong key must be refused as WrongKey"
    );
    assert_eq!(
        files_hash(dir.path()),
        before,
        "a wrong key wrote something"
    );
    // The right key still opens it: nothing was wiped.
    drop(open(&db_path(dir.path()), &key()).expect("the right key"));
}

#[test]
fn a_newer_schema_is_refused_and_not_touched() {
    let dir = temp_store().close();
    raw(dir.path())
        .execute(
            "UPDATE schema_version SET version = 999 WHERE component = 'threema'",
            [],
        )
        .expect("bump");
    let before = files_hash(dir.path());
    match open(&db_path(dir.path()), &key()).err() {
        Some(OpenError::TooNew {
            component,
            found,
            supported,
        }) => assert_eq!((component.as_str(), found, supported), ("threema", 999, 4)),
        _ => panic!("a newer threema schema must be refused as TooNew"),
    }
    assert_eq!(files_hash(dir.path()), before, "a newer file was written");
}

/// Plants a `backend.db` as another part of Kourier would leave it: the `core_outbox` component at
/// `outbox` (not at all for `None`) and the `others` rows of `schema_version`.
fn plant_foreign(dir: &Path, outbox: Option<i64>, others: &[(&str, i64)]) {
    let mut conn = kourier_db::open(&db_path(dir), &key()).expect("open");
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_version(
           component TEXT PRIMARY KEY, version INTEGER NOT NULL)",
    )
    .expect("schema_version");
    if let Some(version) = outbox {
        kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
        conn.execute(
            "UPDATE schema_version SET version = ?1 WHERE component = 'core_outbox'",
            params![version],
        )
        .expect("outbox version");
    }
    for (component, version) in others {
        conn.execute(
            "INSERT INTO schema_version(component, version) VALUES (?1, ?2)",
            params![component, version],
        )
        .expect("component row");
    }
}

/// A file that carries any component but `core_outbox` and ours is refused as `OtherNetwork` BEFORE
/// anything is written: not our tables, not its `core_outbox` migrated, and an outbox NEWER than this
/// build (which would have answered `TooNew` after a write) does not come first. Every file of the
/// directory is byte for byte what it was. The rule is an allow-list: Signal's file, WhatsApp's, the
/// core's and the file of a backend that does not exist yet are all refused.
#[test]
fn a_foreign_file_is_refused_before_any_write_and_stays_byte_for_byte() {
    let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
    /// A case: its name, the version of `core_outbox` (none: not migrated), the other components.
    type Case<'a> = (&'a str, Option<i64>, Vec<(&'a str, i64)>);
    let cases: [Case<'_>; 7] = [
        ("signal", Some(outbox), vec![("signal", 24)]),
        ("whatsapp", Some(outbox), vec![("whatsapp", 4)]),
        ("the core", None, vec![("core", 7)]),
        ("a backend of the future", None, vec![("telegram", 1)]),
        (
            "whatsapp with a newer outbox",
            Some(outbox + 50),
            vec![("whatsapp", 4)],
        ),
        (
            "a file that is both",
            Some(outbox),
            vec![("whatsapp", 4), ("threema", 1)],
        ),
        ("only an unknown row", None, vec![("anything", 3)]),
    ];
    for (case, outbox, others) in cases {
        let dir = tempfile::tempdir().expect("temp dir");
        plant_foreign(dir.path(), outbox, &others);
        let before = files_hash(dir.path());
        let err = open(&db_path(dir.path()), &key()).err();
        assert!(
            matches!(err, Some(OpenError::OtherNetwork)),
            "{case}: a foreign file must be refused as OtherNetwork"
        );
        assert_eq!(
            files_hash(dir.path()),
            before,
            "{case}: the foreign file was written"
        );
        let conn = raw(dir.path());
        let ours: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('identity', 'csp_nonce', 'd2d_nonce')",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(ours, 0, "{case}: our tables were made in a foreign file");
    }
}

/// The errors of opening print no path and no key bytes.
#[test]
fn an_open_error_names_no_path_and_no_key() {
    let dir = temp_store().close();
    let path = db_path(dir.path());
    let mut wrong = TEST_KEY;
    wrong[0] ^= 1;
    let err = open(&path, &wrong.into()).err().expect("wrong key refused");
    let texts = [format!("{err}"), format!("{err:?}")];
    let path_text = path.display().to_string();
    let key_hex: String = wrong.iter().map(|b| format!("{b:02x}")).collect();
    let key_dec = format!("{:?}", &wrong[..4]);
    for text in texts {
        assert!(!text.contains(&path_text), "an open error names the path");
        assert!(
            !text.contains(&key_hex[..16]),
            "an open error names the key in hex"
        );
        assert!(
            !text.contains(key_dec.trim_matches(['[', ']'])),
            "an open error names the key's bytes"
        );
    }
}
