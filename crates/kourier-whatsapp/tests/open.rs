//! Opening an account's `backend.db`: the schema, the wrong key, a newer or foreign file.

use std::path::Path;

use kourier_db::rusqlite::{Connection, params};
use kourier_whatsapp::{OpenError, StoreOwner};
use sha2::{Digest, Sha256};

use crate::support::{self, TEST_KEY, db_path, key, temp_store};

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

#[test]
fn opening_twice_gives_one_schema() {
    let t = temp_store();
    t.owner.close();
    // `temp_store` consumed the owner by value; open the same file again, twice.
    let dir = t.dir;
    for _ in 0..2 {
        let owner = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect("reopen");
        owner.close();
    }
    let conn = raw(dir.path());
    let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
    for (component, version) in [("whatsapp", 8), ("core_outbox", outbox)] {
        let found: i64 = conn
            .query_row(
                "SELECT version FROM schema_version WHERE component = ?1",
                params![component],
                |r| r.get(0),
            )
            .expect(component);
        assert_eq!(found, version, "{component}");
    }
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM schema_version WHERE component = 'whatsapp'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(rows, 1);
    // The catch-up boundary is one row that starts unknown, however often the file was opened.
    let (boundaries, unknown): (i64, i64) = conn
        .query_row(
            "SELECT count(*), count(*) FILTER (WHERE boundary IS NULL) FROM catch_up",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("catch_up");
    assert_eq!((boundaries, unknown), (1, 1));
    // The tables of the contract and the outbox are there.
    for table in [
        "device",
        "identities",
        "sessions",
        "prekeys",
        "signed_prekeys",
        "sender_keys",
        "app_state_keys",
        "app_state_versions",
        "app_state_mutation_macs",
        "appstate_dirty",
        "sender_key_devices",
        "lid_pn_mapping",
        "base_keys",
        "device_registry",
        "group_metadata",
        "tc_tokens",
        "sent_messages",
        "pending_inbound_messages",
        "msg_secrets",
        "inbox",
        "catch_up",
        "core_outbox",
    ] {
        let found: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                params![table],
                |r| r.get(0),
            )
            .expect(table);
        assert!(found, "table {table}");
    }
}

/// W4 (C6), W5a (C6), W5b (B9), W6a1 (A2) and W6a2 (B3): a new file is at schema 8 and holds the history tables, the
/// send tables, the upload records, the app-state books (the mirror of the phone's chat settings, the account settings,
/// the resync books, the map version, the call lines) and the people books (what the core was told of groups, merges,
/// names, the phone's address book and the blocklist), each in its starting state: the cursor and the counts are ONE row
/// of zeros (the delivery updates them, never inserts), the rest are empty (the map version has no row until it is
/// first used). A second and a third open change nothing.
#[test]
fn a_new_file_is_at_schema_8_with_the_history_send_upload_app_state_and_people_tables() {
    let t = temp_store();
    let dir = t.dir.path().to_owned();
    t.owner.close();
    for _ in 0..2 {
        StoreOwner::open(&db_path(&dir), &key(), "wa-test")
            .expect("reopen")
            .close();
    }
    let conn = raw(&dir);
    let version: i64 = conn
        .query_row(
            "SELECT version FROM schema_version WHERE component = 'whatsapp'",
            [],
            |r| r.get(0),
        )
        .expect("version");
    assert_eq!(version, 8);
    let count = |table: &str| -> i64 {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or_else(|e| panic!("table {table}: {e}"))
    };
    for table in [
        "history_announced",
        "history_delivery",
        "history_chats",
        "history_chunks",
        "history_deferred",
        "history_live",
        "history_named",
        "send_outcomes",
        "chat_started",
        "features_stated",
        "receipt_jobs",
        "media_uploads",
        "chat_state",
        "chat_counted",
        "chat_cut",
        "account_link",
        "account_appstate",
        "appstate_resyncs",
        "appstate_map",
        "call_lines",
        "group_state",
        "merges_stated",
        "names_stated",
        "blocked_stated",
        "blocklist_read",
        "address_book",
        "persons_known",
        // The phone's pairs and push names of people the core does not know yet (W6a2 F11, N25): kept for the day it does.
        "kept_pairs",
        "kept_names",
    ] {
        assert_eq!(count(table), 0, "{table} starts empty");
    }
    let cursor: (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT chunk, phase, conv, msg FROM history_cursor WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("the cursor row");
    assert_eq!(cursor, (0, 0, 0, 0));
    assert_eq!(
        count("history_cursor"),
        1,
        "one cursor, however often opened"
    );
    let counts: Vec<i64> = (0..6)
        .map(|i| {
            conn.query_row(
                "SELECT delivered, dropped, deferred, bad, lost, lost_told FROM history_counts WHERE id = 1",
                [],
                |r| r.get(i),
            )
            .expect("the counts row")
        })
        .collect();
    assert_eq!(counts, vec![0; 6]);
    assert_eq!(
        count("history_counts"),
        1,
        "one row of counts, however often opened"
    );
}

#[test]
fn a_wrong_key_fails_and_the_files_do_not_change() {
    let t = temp_store();
    let dir = t.dir.path().to_owned();
    t.owner.close();
    let before = files_hash(&dir);
    let mut wrong = TEST_KEY;
    wrong[0] ^= 1;
    let err = StoreOwner::open(&db_path(&dir), &wrong.into(), "wa-test").expect_err("wrong key");
    assert!(matches!(err, OpenError::WrongKey), "{err}");
    assert_eq!(files_hash(&dir), before, "a wrong key wrote something");
    // The right key still opens it: nothing was wiped.
    StoreOwner::open(&db_path(&dir), &key(), "wa-test")
        .expect("the right key")
        .close();
}

#[test]
fn a_newer_schema_is_refused_and_not_touched() {
    let t = temp_store();
    let dir = t.dir.path().to_owned();
    t.owner.close();
    raw(&dir)
        .execute(
            "UPDATE schema_version SET version = 999 WHERE component = 'whatsapp'",
            [],
        )
        .expect("bump");
    let before = files_hash(&dir);
    let err = StoreOwner::open(&db_path(&dir), &key(), "wa-test").expect_err("newer");
    match err {
        OpenError::TooNew {
            component,
            found,
            supported,
        } => assert_eq!((component.as_str(), found, supported), ("whatsapp", 999, 8)),
        other => panic!("{other}"),
    }
    assert_eq!(files_hash(&dir), before);
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
/// directory is byte for byte what it was. The rule is an allow-list: Signal's file, the core's and
/// the file of a backend that does not exist yet are all refused.
#[test]
fn a_foreign_file_is_refused_before_any_write_and_stays_byte_for_byte() {
    let outbox = i64::from(kourier_db::outbox::MIGRATIONS.latest());
    /// A case: its name, the version of `core_outbox` (none: not migrated), the other components.
    type Case<'a> = (&'a str, Option<i64>, Vec<(&'a str, i64)>);
    let cases: [Case<'_>; 6] = [
        ("signal", Some(outbox), vec![("signal", 22)]),
        ("the core", None, vec![("core", 7)]),
        ("a backend of the future", None, vec![("threema", 1)]),
        (
            "signal with a newer outbox",
            Some(outbox + 50),
            vec![("signal", 22)],
        ),
        (
            "a file that is both",
            Some(outbox),
            vec![("signal", 22), ("whatsapp", 1)],
        ),
        ("only an unknown row", None, vec![("anything", 3)]),
    ];
    for (case, outbox, others) in cases {
        let dir = tempfile::tempdir().expect("temp dir");
        plant_foreign(dir.path(), outbox, &others);
        let before = files_hash(dir.path());
        let err = StoreOwner::open(&db_path(dir.path()), &key(), "wa-test").expect_err(case);
        assert!(matches!(err, OpenError::OtherNetwork), "{case}: {err}");
        assert_eq!(
            files_hash(dir.path()),
            before,
            "{case}: the foreign file was written"
        );
        let conn = raw(dir.path());
        let ours: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('device', 'sessions', 'tc_tokens')",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(ours, 0, "{case}: our tables were made in a foreign file");
    }
}

#[test]
fn the_support_helpers_agree_with_each_other() {
    // A guard for the helpers the other test modules build on.
    let t = temp_store();
    assert!(t.path().exists());
    assert_eq!(format!("{:?}", t.store()), "KourierStore { .. }");
    assert!(!support::child_filter("it::open", "x").contains("it::"));
}
