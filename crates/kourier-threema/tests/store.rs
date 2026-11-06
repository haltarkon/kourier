//! The rows of schema 1 (the identity row and the two nonce stores) and the tables of schema 2 (what a
//! device join imports, `migrations/0002_join.sql`): the migration of a schema-1 file and each CHECK;
//! the join's two zones (TH4 J4b: `import_join` writes no `core_outbox` row, `ready_join` writes the
//! rows with `AccountReady` first) and the Rebuild equality (`stated_events` over the file equals what
//! zone 2 wrote AND what the import states in memory). Schema 4 (TH6a, `send_*`): each CHECK, the send queue's zones
//! with literal rows, the two budgets at N and N+1, what may run next, the sweep's ages, the failed runs, the door; its
//! kill loops are in `tests/kill.rs`. Schema 5 (TH6 L1, `csp_inbound`, `inbound_*`): each CHECK at every state, ZONE
//! RECEIVE and ZONE REFLECTED with literal rows, step 0, the sweep's ages, the door; the kill loops in `tests/kill.rs`, the
//! plans of the statements in `src/leader/zone.rs`'s unit test.

use std::rc::Rc;

use kourier_backend_api::{BackendEvent, decode_outbox_row};
use kourier_db::rusqlite::Connection;
use kourier_model::UserId;
use kourier_threema::StoreError;
use kourier_threema::store::{
    Discarded, Inbound, InboundAck, InboundRow, InboundState, InboundSwept, Kept, NewBody, NewJob,
    RULE_PENDING_EXPIRED, Received, Receiving, To, fail_inbound_zone, inbound_row, inbound_rows,
    mark_inbound_reflected, receive_inbound, sweep_inbound,
};
use kourier_threema::store::{
    EVICT_AFTER_MS, Identity, Import, JOB_BUDGET, NewFate, NewOrigin, NewReceiver, NonceScope,
    RULE_JOB_EXPIRED, RULE_QUEUE_FULL, RULE_TRANSMIT_EVICTED, RULE_TRANSMIT_EXPIRED, ReceiverState,
    Runnable, SealedRows, SealedUpdate, SendBudget, SendEnd, SendOrigin, SendReceiver, SendState,
    SendTask, SlotState, Swept, TRANSMIT_BUDGET, add_nonce_batches, add_nonces, contains_nonce,
    count_failed_run, fail_send_zone, give_up_send_job, identity_exists, import_events,
    import_join, is_join_ready, load_identity, mark_join_ready, mark_send_acks, mark_send_done,
    mark_send_reflected, next_send, prepare_send, prepare_send_zone, ready_join, record_link,
    save_identity, send_ledger, send_receivers, send_task, send_tasks, set_slot_existing,
    stated_events, sweep_sends,
};
use tokio::sync::Notify;
use zeroize::Zeroizing;

use crate::support::{db_path, key, temp_store};

/// A synthetic Threema ID (8 characters, a substring of no other value of these tests).
const TEST_ID: &str = "TH1QW7KZ";
const TEST_LABEL: &str = "Kourier (th1-store-host)";

/// Distinct known bytes for every secret, so a print of any of them can be found.
fn ck() -> [u8; 32] {
    std::array::from_fn(|i| 0xc0 ^ i as u8)
}
fn dgk() -> [u8; 32] {
    std::array::from_fn(|i| 0x90 ^ i as u8)
}
fn cookie() -> [u8; 16] {
    std::array::from_fn(|i| 0x60 ^ i as u8)
}

/// The device ids sit at the edges of the i64 bit pattern: the top bit set in both.
const D2M_ID: u64 = u64::MAX;
const CSP_ID: u64 = i64::MAX as u64 + 1;

fn identity() -> Identity {
    Identity {
        threema_id: TEST_ID.to_owned(),
        client_key: Zeroizing::new(ck()),
        server_group: "a".to_owned(),
        device_group_key: Zeroizing::new(dgk()),
        device_cookie: Zeroizing::new(cookie()),
        d2m_device_id: D2M_ID,
        csp_device_id: CSP_ID,
        slot_state: SlotState::Existing,
        device_label: TEST_LABEL.to_owned(),
        linked_at_ms: 1_761_350_400_123,
    }
}

/// `err` is the file's own refusal of `op` (round A, H-1: a `Sql` failure with SQLite's primary code
/// `SQLITE_CONSTRAINT` (19), whatever the extended one), and prints nothing but the operation.
fn assert_refused(err: &StoreError, op: &str, case: &str) {
    assert_eq!(err.to_string(), op, "{case}");
    assert!(
        matches!(err, StoreError::Sql { code: Some(code), .. } if code & 0xff == 19),
        "{case}: not a constraint failure of the file: {err:?}"
    );
}

fn save(db: &kourier_db::ZoneDb, id: &Identity) -> Result<(), StoreError> {
    db.zone(|tx| save_identity(tx, id))
}

fn load(db: &kourier_db::ZoneDb) -> Option<Identity> {
    db.read(|conn: &Connection| load_identity(conn))
        .expect("load the identity")
}

/// Every form in which a test value could reach a printed text: the strings, the secrets in hex and
/// as Rust prints a byte slice, the device ids in decimal and hex.
fn needles() -> Vec<String> {
    let hex = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let dec = |b: &[u8]| format!("{:?}", &b[..4]).trim_matches(['[', ']']).to_owned();
    let mut out = vec![TEST_ID.to_owned(), TEST_LABEL.to_owned()];
    for secret in [&ck()[..], &dgk()[..], &cookie()[..]] {
        out.push(hex(secret)[..16].to_owned());
        out.push(dec(secret));
    }
    for id in [D2M_ID, CSP_ID] {
        out.push(id.to_string());
        out.push(format!("{id:x}"));
        // The i64 form of u64::MAX is "-1", too short to search for; that of 1 << 63 is not.
        let signed = i64::from_ne_bytes(id.to_ne_bytes()).to_string();
        if signed.len() > 4 {
            out.push(signed);
        }
    }
    out
}

fn assert_prints_nothing(text: &str, what: &str) {
    for needle in needles() {
        assert!(
            !text.contains(&needle),
            "{what} prints a test value (an ID, a label, a key, a cookie or a device id)"
        );
    }
}

#[test]
fn the_identity_row_round_trips_with_ids_above_i64_max() {
    let t = temp_store();
    assert!(load(&t.db).is_none(), "no identity before the link");
    let exists = t.db.read(|conn: &Connection| identity_exists(conn));
    assert_eq!(exists, Ok(false));
    save(&t.db, &identity()).expect("save");
    assert_eq!(
        t.db.read(|conn: &Connection| identity_exists(conn)),
        Ok(true)
    );
    let back = load(&t.db).expect("the saved identity");
    assert!(
        back == identity(),
        "the identity row did not come back as saved"
    );
    assert_eq!(back.d2m_device_id, u64::MAX);
    assert_eq!(back.csp_device_id, 1u64 << 63);
    // The file holds the same 64 bits as an i64.
    let stored: (i64, i64) =
        t.db.read(|conn: &Connection| {
            conn.query_row(
                "SELECT d2m_device_id, csp_device_id FROM identity",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("raw ids");
    assert_eq!(stored, (-1, i64::MIN));
    // A new slot reads back as new.
    let t = temp_store();
    let mut fresh = identity();
    fresh.slot_state = SlotState::New;
    save(&t.db, &fresh).expect("save new");
    assert_eq!(load(&t.db).map(|i| i.slot_state), Some(SlotState::New));
}

/// There is one identity row: a second save fails and leaves the first.
#[test]
fn a_second_identity_is_refused_and_the_first_stays() {
    let t = temp_store();
    save(&t.db, &identity()).expect("save");
    let mut other = identity();
    other.threema_id = "TH1ZZ9YX".to_owned();
    other.client_key = Zeroizing::new([7; 32]);
    let err = save(&t.db, &other).expect_err("a second identity");
    assert_refused(&err, "save identity failed", "a second identity");
    assert!(
        load(&t.db) == Some(identity()),
        "the first identity changed"
    );
}

/// The schema's CHECKs refuse a malformed row, and the error names the operation only.
#[test]
fn a_malformed_identity_is_refused_without_printing_it() {
    /// What makes a valid row malformed.
    type Spoil = fn(&mut Identity);
    let cases: [(&str, Spoil); 7] = [
        ("a 9-character ID", |i| i.threema_id.push('Q')),
        ("a 7-character ID", |i| {
            i.threema_id.pop();
        }),
        ("an empty server group", |i| i.server_group.clear()),
        // One character long, but outside [A-Za-z0-9-]: the old length rule let it through.
        ("a one-character server group with a symbol", |i| {
            i.server_group = "_".to_owned()
        }),
        ("a server group with a symbol", |i| {
            i.server_group = "ab_c".to_owned()
        }),
        ("a server group with a space", |i| {
            i.server_group = "ab c".to_owned()
        }),
        ("a server group with a non-ASCII letter", |i| {
            i.server_group = "\u{e9}a".to_owned()
        }),
    ];
    for (case, spoil) in cases {
        let t = temp_store();
        let mut bad = identity();
        spoil(&mut bad);
        let err = save(&t.db, &bad).expect_err(case);
        assert_refused(&err, "save identity failed", case);
        assert_prints_nothing(&format!("{err} {err:?}"), case);
        assert!(load(&t.db).is_none(), "{case}: a row was written");
    }
}

/// The chat server group is any non-empty `[A-Za-z0-9-]+` (the pinned md-d2d-join.proto and
/// libthreema's own `ChatServerGroup`), not one character: "ds" and "some-group-42" are real groups.
#[test]
fn a_server_group_of_the_spec_round_trips() {
    for group in ["a", "ds", "aAzZ09", "some-group-42", "-"] {
        let t = temp_store();
        let mut id = identity();
        id.server_group = group.to_owned();
        save(&t.db, &id).expect("save");
        let back = load(&t.db).expect("the saved identity");
        assert!(
            back.server_group == group,
            "a server group of the spec did not come back as saved (length {})",
            group.len()
        );
    }
}

/// `Debug` of the identity row and of every store error prints none of the row's values.
#[test]
fn debug_prints_no_id_no_key_no_device_id_and_no_label() {
    let id = identity();
    let text = format!("{id:?} {id:#?}");
    assert_prints_nothing(&text, "Debug of the identity row");
    assert!(text.contains("Identity"), "Debug names the type");
    for err in [
        StoreError::Failed("save identity failed"),
        StoreError::Sql {
            op: "save identity failed",
            code: Some(275),
        },
        StoreError::Unreadable("identity row unreadable"),
    ] {
        assert_prints_nothing(&format!("{err} {err:?}"), "a store error");
    }
}

#[test]
fn a_nonce_is_kept_once_per_store() {
    let t = temp_store();
    let a = [0xa1; 32];
    let b = [0xb2; 32];
    let contains = |scope, hash: &[u8; 32]| {
        t.db.read(|conn: &Connection| contains_nonce(conn, scope, hash))
            .expect("contains")
    };
    assert!(!contains(NonceScope::Csp, &a));
    t.db.zone(|tx| add_nonces(tx, NonceScope::Csp, &[a, b]))
        .expect("add");
    // A nonce already there is no error and stays one row.
    t.db.zone(|tx| add_nonces(tx, NonceScope::Csp, &[a, a]))
        .expect("add again");
    assert!(contains(NonceScope::Csp, &a));
    assert!(contains(NonceScope::Csp, &b));
    // The two stores are apart.
    assert!(!contains(NonceScope::D2d, &a));
    t.db.zone(|tx| add_nonces(tx, NonceScope::D2d, &[a]))
        .expect("add d2d");
    assert!(contains(NonceScope::D2d, &a));
    assert!(!contains(NonceScope::D2d, &b));
    let rows = |table: &str| -> i64 {
        t.db.read(|conn: &Connection| {
            conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect(table)
    };
    assert_eq!((rows("csp_nonce"), rows("d2d_nonce")), (2, 1));
}

/// SQLite's `hex()` form (upper case) of bytes, to compare a stored BLOB with a test value.
fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// `record_link` writes the identity row AND one `AccountReady` into `core_outbox`. The expected
/// rows are written out here (review F2), not read back through the store's own functions: a link
/// that drops the event (the core never told) or the row fails this.
#[test]
fn record_link_stores_the_identity_and_tells_the_core_once() {
    const NOW_MS: i64 = 1_761_350_400_999;
    let t = temp_store();
    let wake = Rc::new(Notify::new());
    record_link(&t.db, &identity(), NOW_MS, &wake).expect("link");

    type Row = (
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        String,
        String,
        i64,
    );
    let row: Row =
        t.db.read(|conn: &Connection| {
            conn.query_row(
                "SELECT threema_id, hex(client_key), server_group, hex(device_group_key),
                        hex(device_cookie), d2m_device_id, csp_device_id, slot_state,
                        device_label, linked_at_ms FROM identity",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("the identity row");
    let expected: Row = (
        TEST_ID.to_owned(),
        hex_upper(&ck()),
        "a".to_owned(),
        hex_upper(&dgk()),
        hex_upper(&cookie()),
        -1,
        i64::MIN,
        "existing".to_owned(),
        TEST_LABEL.to_owned(),
        1_761_350_400_123,
    );
    assert!(
        row == expected,
        "record_link did not store the identity row as given"
    );

    let outbox: Vec<(i64, Vec<u8>, i64)> =
        t.db.read(|conn: &Connection| {
            let mut stmt = conn
                .prepare("SELECT seq, envelope, created_at FROM core_outbox ORDER BY seq")
                .map_err(kourier_db::Error::from)?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(kourier_db::Error::from)?
                .collect::<Result<_, _>>()
                .map_err(kourier_db::Error::from)
        })
        .expect("the outbox rows");
    assert!(
        outbox.len() == 1 && outbox[0].0 == 1 && outbox[0].2 == NOW_MS,
        "record_link must leave exactly one core_outbox row (seq 1, created at the link's time)"
    );
    let events = decode_outbox_row(&outbox[0].1).expect("the outbox row decodes");
    let expected_events = vec![BackendEvent::AccountReady {
        own_ids: vec![UserId::parse("threema:TH1QW7KZ").expect("a valid id")],
        device: None,
        display_name: None,
        number: None,
    }];
    assert!(
        events == expected_events,
        "the outbox row is not exactly [AccountReady for the linked id]"
    );
    assert!(
        t.db.read(is_join_ready).expect("the marker"),
        "record_link wrote no zone-2 marker"
    );

    // A second link fails as a whole: no second row of either kind.
    assert!(
        record_link(&t.db, &identity(), NOW_MS + 1, &wake).is_err(),
        "a second link must be refused"
    );
    let rows: i64 =
        t.db.read(|conn: &Connection| {
            conn.query_row("SELECT count(*) FROM core_outbox", [], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect("count");
    assert_eq!(rows, 1, "a refused link left an event in core_outbox");
}

/// `add_nonce_batches` writes the CSP batch to the CSP store and the D2D batch to the D2D store,
/// and a nonce already there stays one row. The expected rows are literals (review F2).
#[test]
fn add_nonce_batches_puts_each_batch_in_its_own_store() {
    let t = temp_store();
    t.db.zone(|tx| {
        add_nonces(tx, NonceScope::Csp, &[[1; 32]])?;
        add_nonces(tx, NonceScope::D2d, &[[2; 32]])
    })
    .expect("the nonces before");
    // 01 is already in the CSP store; 03 and 04 are new there; 05 and 06 are new in the D2D store.
    add_nonce_batches(&t.db, &[[1; 32], [3; 32], [4; 32]], &[[5; 32], [6; 32]]).expect("batches");
    let rows = |table: &str| -> Vec<String> {
        t.db.read(|conn: &Connection| {
            let mut stmt = conn
                .prepare(&format!("SELECT hex(hash) FROM {table} ORDER BY 1"))
                .map_err(kourier_db::Error::from)?;
            stmt.query_map([], |r| r.get::<_, String>(0))
                .map_err(kourier_db::Error::from)?
                .collect::<Result<_, _>>()
                .map_err(kourier_db::Error::from)
        })
        .expect(table)
    };
    let repeated = |byte: u8| hex_upper(&[byte; 32]);
    assert!(
        rows("csp_nonce") == [repeated(1), repeated(3), repeated(4)],
        "the CSP store does not hold exactly 01, 03, 04"
    );
    assert!(
        rows("d2d_nonce") == [repeated(2), repeated(5), repeated(6)],
        "the D2D store does not hold exactly 02, 05, 06"
    );
}

/// TH3: `ServerInfo`'s one-row write: `new` becomes `existing` and nothing else of the row changes;
/// a file without the identity row is an error, not a silent no-op.
#[test]
fn set_slot_existing_changes_only_the_slot_state() {
    let empty = temp_store();
    assert!(
        matches!(set_slot_existing(&empty.db), Err(StoreError::Failed(_))),
        "a missing identity row was not an error"
    );
    let store = temp_store();
    let new = Identity {
        slot_state: SlotState::New,
        ..identity()
    };
    save(&store.db, &new).expect("save");
    set_slot_existing(&store.db).expect("the write");
    assert!(
        load(&store.db) == Some(identity()),
        "the row is not the stored one with `existing`"
    );
    set_slot_existing(&store.db).expect("a second write is no error");
    assert!(load(&store.db) == Some(identity()), "the second write");
}

/// Schema 1 alone, as TH1-TH3 left a file (the first migration of the crate, frozen).
const SCHEMA_1: kourier_db::Migrations =
    kourier_db::Migrations::new("threema", &[kourier_db::migration!(1, "0001_init")]);

/// The tables schema 2 adds.
const SCHEMA_2_TABLES: [&str; 7] = [
    "contact",
    "grp",
    "dist_list",
    "own_profile",
    "settings",
    "held_blob",
    "join_ready",
];

fn count(db: &kourier_db::ZoneDb, table: &str) -> i64 {
    db.read(|conn: &Connection| {
        conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .map_err(kourier_db::Error::from)
    })
    .expect(table)
}

/// A file of schema 1 (with a row) opens, is at the latest schema after (4 since TH6a; schema 3's own
/// migration from 2: `deferred::a_schema_2_file_migrates_to_schema_3_with_deferred_empty`, schema 4's from 3:
/// `open::a_schema_3_file_migrates_to_schema_4_with_the_send_tables_empty`), keeps its row, and every table of
/// schema 2 is there and empty.
#[test]
fn a_schema_1_file_migrates_to_schema_2_with_every_new_table_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let mut conn = kourier_db::open(&db_path(dir.path()), &key()).expect("open");
        kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
        kourier_db::migrate(&mut conn, &SCHEMA_1).expect("schema 1");
        conn.execute(
            "INSERT INTO csp_nonce (hash) VALUES (?1)",
            kourier_db::rusqlite::params![&[0x5a_u8; 32][..]],
        )
        .expect("a schema-1 row");
        for table in SCHEMA_2_TABLES {
            let there: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                    [table],
                    |r| r.get(0),
                )
                .expect(table);
            assert!(!there, "{table} exists at schema 1");
        }
    }
    let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("open migrates");
    let version: i64 = db
        .read(|conn: &Connection| {
            conn.query_row(
                "SELECT version FROM schema_version WHERE component = 'threema'",
                [],
                |r| r.get(0),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("version");
    assert_eq!(version, 5, "the file is not at the latest schema");
    for table in SCHEMA_2_TABLES {
        assert_eq!(
            count(&db, table),
            0,
            "{table} is not empty after the migration"
        );
    }
    assert_eq!(count(&db, "csp_nonce"), 1, "the schema-1 row is gone");
    assert!(
        db.read(|conn: &Connection| contains_nonce(conn, NonceScope::Csp, &[0x5a; 32]))
            .expect("contains"),
        "the schema-1 row changed"
    );
}

/// Whether one statement of a fresh schema-2 store takes its row.
fn takes(statements: &[&str]) -> bool {
    let t = temp_store();
    t.db.zone(|tx| {
        for statement in statements {
            tx.execute(statement, []).map_err(kourier_db::Error::from)?;
        }
        Ok::<(), kourier_db::Error>(())
    })
    .is_ok()
}

/// Each CHECK of schema 2 takes its good row and refuses the bad one, on both sides of each size: the
/// 8-character keys (7, 8, 9), the one-row tables (`id` 1 and 2; the zone-2 marker too), the blob id (15,
/// 16, 17 bytes), the text and blob keys that are NULL (S10: no table is WITHOUT ROWID, so NOT NULL is
/// written out; a CHECK passes NULL). Also
/// the keys that are no CHECK: a group id of the high bit (the i64 of a u64), a group twice, a NULL time.
#[test]
fn each_check_of_schema_2_refuses_its_bad_row_and_takes_the_good_one() {
    let contact =
        |id: &str| format!("INSERT INTO contact (identity, sync) VALUES ('{id}', X'0a00')");
    let grp = |creator: &str, group_id: i64| {
        format!(
            "INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES ('{creator}', {group_id}, X'0a00', 1)"
        )
    };
    let blob = |len: usize| {
        format!(
            "INSERT INTO held_blob (id, data, received_at) VALUES (X'{}', X'00', 1)",
            "b1".repeat(len)
        )
    };
    let one_row =
        |table: &str, id: i64| format!("INSERT INTO {table} (id, sync) VALUES ({id}, X'')");
    let cases: Vec<(String, Vec<String>, bool)> = vec![
        ("a contact ID of 7".into(), vec![contact("J4AS007")], false),
        ("a contact ID of 8".into(), vec![contact("J4AS0008")], true),
        ("a contact ID of 9".into(), vec![contact("J4AS00009")], false),
        (
            "a contact twice".into(),
            vec![contact("J4AS0008"), contact("J4AS0008")],
            false,
        ),
        (
            "a contact without a time".into(),
            vec!["INSERT INTO contact (identity, sync, last_update_at) VALUES ('J4AS0008', X'', NULL)".into()],
            true,
        ),
        ("a creator of 7".into(), vec![grp("J4AS007", 1)], false),
        ("a creator of 8".into(), vec![grp("J4AS0008", 1)], true),
        ("a creator of 9".into(), vec![grp("J4AS00009", 1)], false),
        (
            "a group id of the high bit".into(),
            vec![grp("J4AS0008", i64::MIN), grp("J4AS0008", -1)],
            true,
        ),
        (
            "a group twice".into(),
            vec![grp("J4AS0008", 5), grp("J4AS0008", 5)],
            false,
        ),
        (
            "the same group id of another creator".into(),
            vec![grp("J4AS0008", 5), grp("J4AS0009", 5)],
            true,
        ),
        (
            "a group without a time".into(),
            vec!["INSERT INTO grp (creator, group_id, sync) VALUES ('J4AS0008', 1, X'')".into()],
            false,
        ),
        (
            "a list of the high bit".into(),
            vec!["INSERT INTO dist_list (id, sync, last_update_at) VALUES (-1, X'', 1)".into()],
            true,
        ),
        ("the profile at id 1".into(), vec![one_row("own_profile", 1)], true),
        ("the profile at id 2".into(), vec![one_row("own_profile", 2)], false),
        ("the settings at id 1".into(), vec![one_row("settings", 1)], true),
        ("the settings at id 2".into(), vec![one_row("settings", 2)], false),
        (
            "the marker at id 1".into(),
            vec!["INSERT INTO join_ready (id, at) VALUES (1, 5)".into()],
            true,
        ),
        (
            "the marker at id 2".into(),
            vec!["INSERT INTO join_ready (id, at) VALUES (2, 5)".into()],
            false,
        ),
        (
            "the marker twice".into(),
            vec![
                "INSERT INTO join_ready (id, at) VALUES (1, 5)".into(),
                "INSERT INTO join_ready (id, at) VALUES (1, 6)".into(),
            ],
            false,
        ),
        (
            "the marker without a time".into(),
            vec!["INSERT INTO join_ready (id) VALUES (1)".into()],
            false,
        ),
        (
            "a contact without an ID".into(),
            vec!["INSERT INTO contact (identity, sync) VALUES (NULL, X'')".into()],
            false,
        ),
        (
            "a creator that is NULL".into(),
            vec!["INSERT INTO grp (creator, group_id, sync, last_update_at) VALUES (NULL, 1, X'', 1)".into()],
            false,
        ),
        (
            "a blob without an id".into(),
            vec!["INSERT INTO held_blob (id, data, received_at) VALUES (NULL, X'00', 1)".into()],
            false,
        ),
        ("a blob id of 15".into(), vec![blob(15)], false),
        ("a blob id of 16".into(), vec![blob(16)], true),
        ("a blob id of 17".into(), vec![blob(17)], false),
        (
            "a blob id as text".into(),
            vec!["INSERT INTO held_blob (id, data, received_at) VALUES ('0123456789abcdef', X'00', 1)".into()],
            false,
        ),
    ];
    for (case, statements, expected) in cases {
        let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
        assert_eq!(takes(&statements), expected, "{case}");
    }
}

// THE JOIN (TH4 J4b) ------------------------------------------------------------------------------

/// The seed of the join fixture's world and of its device ids (the same seed: the same `Import`, in
/// every process: the kill loop's child builds it again).
const JOIN_SEED: u64 = 0x004a_3462;
/// The import's label and time.
pub(crate) const JOIN_LABEL: &str = "Kourier (j4b-host)";
pub(crate) const JOIN_NOW_MS: i64 = 1_761_400_000_000;
/// The blob of the second picture (a contact's), 16 bytes.
const CONTACT_BLOB: [u8; 16] = [0xb7; 16];

/// The fixture's join: what the fake's builder hands over for `own` (3 contacts, 2 groups, a profile
/// picture, 5 hashed nonces of each store), plus one distribution list, a second blob (the first
/// contact's own picture), the second contact blocked, the third hidden (`GROUP_OR_DELETED`), read
/// receipts off and typing on, the first group's id `u64::MAX` (its i64 bits are -1); through
/// `Import::checked_for_tests`, the crate's doc-hidden door to `Import::check` (the real bounds).
pub(crate) struct JoinFixture {
    pub(crate) import: Import,
    /// The keys as the fake's phone holds them (the oracle of the stored identity row).
    pub(crate) ck: Vec<u8>,
    pub(crate) dgk: Vec<u8>,
    pub(crate) cookie: Vec<u8>,
}

pub(crate) fn join_fixture(own: &str) -> JoinFixture {
    use crate::support::mock::{self, essential};
    use libthreema::protobuf::{common, d2d_join, d2d_sync};

    let world = mock::world(JOIN_SEED);
    let group = world.device_group(own);
    let mut handed = essential::essential(
        &world,
        &group,
        &essential::Spec {
            contacts: 3,
            groups: 2,
            hashed_nonces: 5,
            profile_picture: true,
            bad: Vec::new(),
        },
    );
    let data = &mut handed.data;
    fn contact(data: &mut d2d_join::EssentialData, i: usize) -> &mut d2d_sync::Contact {
        data.contacts[i].contact.as_mut().expect("a contact")
    }
    contact(data, 0).contact_defined_profile_picture = Some(common::DeltaImage {
        image: Some(common::delta_image::Image::Updated(common::Image {
            r#type: 0,
            blob: Some(common::Blob {
                id: CONTACT_BLOB.to_vec(),
                ..Default::default()
            }),
        })),
    });
    let blocked = contact(data, 1).identity.clone();
    contact(data, 2).acquaintance_level =
        Some(d2d_sync::contact::AcquaintanceLevel::GroupOrDeleted as i32);
    let settings = data.settings.as_mut().expect("settings");
    settings.read_receipt_policy = Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32);
    settings.typing_indicator_policy =
        Some(d2d_sync::TypingIndicatorPolicy::SendTypingIndicator as i32);
    settings.blocked_identities = Some(common::Identities {
        identities: vec![blocked],
    });
    data.groups[0]
        .group
        .as_mut()
        .and_then(|g| g.group_identity.as_mut())
        .expect("a group identity")
        .group_id = u64::MAX;
    data.distribution_lists
        .push(d2d_join::essential_data::AugmentedDistributionList {
            distribution_list: Some(d2d_sync::DistributionList {
                distribution_list_id: 1 << 63,
                name: Some("List J4B".to_owned()),
                ..Default::default()
            }),
            last_update_at: 1_761_300_000_000,
        });
    handed.blobs.push(common::BlobData {
        id: CONTACT_BLOB.to_vec(),
        data: vec![0xd7; 64],
    });
    let blobs = handed
        .blobs
        .iter()
        .map(|b| (b.id.clone(), b.data.clone()))
        .collect();
    let import = Import::checked_for_tests(&handed.encoded(), blobs, JOIN_SEED)
        .unwrap_or_else(|rule| panic!("the fixture's join was refused: {rule}"));
    JoinFixture {
        import,
        ck: group.identity.ck.secret.to_vec(),
        dgk: group.dgk.to_vec(),
        cookie: group.cookie.to_vec(),
    }
}

/// The `core_outbox` rows of `db`, in `seq` order, decoded.
pub(crate) fn outbox_events(db: &kourier_db::ZoneDb) -> Vec<Vec<BackendEvent>> {
    db.read(|conn: &Connection| {
        let mut stmt = conn.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok::<_, kourier_db::Error>(rows)
    })
    .expect("read core_outbox")
    .iter()
    .map(|row| decode_outbox_row(row).expect("a row decodes"))
    .collect()
}

/// The tables of zone 1 and how many rows each holds after the fixture's import.
pub(crate) const ZONE_1_TABLES: [(&str, i64); 9] = [
    ("identity", 1),
    ("own_profile", 1),
    ("settings", 1),
    ("contact", 3),
    ("grp", 2),
    ("dist_list", 1),
    ("held_blob", 2),
    ("csp_nonce", 5),
    ("d2d_nonce", 5),
];

#[test]
fn import_join_writes_every_table_and_no_core_outbox_row() {
    const OWN: &str = "J4BSTOR1";
    let fixture = join_fixture(OWN);
    let store = temp_store();
    import_join(&store.db, &fixture.import, JOIN_LABEL, JOIN_NOW_MS).expect("zone 1");
    for (table, rows) in ZONE_1_TABLES {
        assert_eq!(count(&store.db, table), rows, "{table} after zone 1");
    }
    assert_eq!(
        count(&store.db, "core_outbox"),
        0,
        "zone 1 tells the core nothing"
    );
    let row = load(&store.db).expect("the identity row");
    assert!(
        row.threema_id == OWN
            && row.client_key[..] == fixture.ck[..]
            && row.device_group_key[..] == fixture.dgk[..]
            && row.device_cookie[..] == fixture.cookie[..]
            && row.server_group == "ds7"
            && row.slot_state == SlotState::New
            && row.device_label == JOIN_LABEL
            && row.linked_at_ms == JOIN_NOW_MS
            && row.d2m_device_id != 0
            && row.csp_device_id != 0,
        "the identity row is not the phone's with slot new, the label and the time"
    );
    let ints = |sql: &str| -> Vec<i64> {
        store
            .db
            .read(|conn: &Connection| {
                let mut stmt = conn.prepare(sql)?;
                let rows = stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<Result<Vec<i64>, _>>()?;
                Ok::<_, kourier_db::Error>(rows)
            })
            .expect(sql)
    };
    assert!(
        ints("SELECT group_id FROM grp").contains(&-1),
        "a group id u64::MAX is stored as the i64 of its bits"
    );
    assert_eq!(
        ints("SELECT id FROM dist_list"),
        vec![i64::MIN],
        "a list id 1 << 63 is stored as the i64 of its bits"
    );
    assert_eq!(
        ints("SELECT received_at FROM held_blob"),
        vec![JOIN_NOW_MS; 2],
        "the blobs are received at the import's time"
    );
    assert_eq!(
        ints("SELECT count(*) FROM contact WHERE last_update_at IS NOT NULL"),
        vec![3]
    );
    // A second import over the joined file fails as one and keeps what was there.
    let again = join_fixture(OWN);
    assert!(
        import_join(&store.db, &again.import, JOIN_LABEL, JOIN_NOW_MS + 1).is_err(),
        "a second import over a joined file"
    );
    for (table, rows) in ZONE_1_TABLES {
        assert_eq!(
            count(&store.db, table),
            rows,
            "{table} after the refused second import"
        );
    }
}

#[test]
fn ready_join_writes_the_rows_with_account_ready_first() {
    const OWN: &str = "J4BREDY1";
    let fixture = join_fixture(OWN);
    let store = temp_store();
    // Zone 2 over a file without zone 1 fails and writes nothing.
    let wake = Rc::new(Notify::new());
    assert!(ready_join(&store.db, JOIN_NOW_MS, &wake).is_err());
    assert_eq!(count(&store.db, "core_outbox"), 0);

    import_join(&store.db, &fixture.import, JOIN_LABEL, JOIN_NOW_MS).expect("zone 1");
    // S4: a file with zone 1 only carries no marker; zone 2 writes it, in the same commit.
    assert!(
        !store.db.read(is_join_ready).expect("the marker"),
        "zone 1 wrote the marker"
    );
    let ready = ready_join(&store.db, JOIN_NOW_MS + 5, &wake).expect("zone 2");
    assert!(
        store.db.read(is_join_ready).expect("the marker"),
        "zone 2 wrote no marker"
    );
    let marker_at: i64 = store
        .db
        .read(|conn: &Connection| {
            conn.query_row("SELECT at FROM join_ready WHERE id = 1", [], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect("the marker's time");
    assert_eq!(marker_at, JOIN_NOW_MS + 5, "the marker's time");
    let rows = outbox_events(&store.db);
    assert_eq!(
        (ready.first, ready.last),
        (1, 2),
        "the seqs of the first and the last row"
    );
    let sizes: Vec<usize> = rows.iter().map(Vec::len).collect();
    assert_eq!(
        sizes,
        vec![2, 7],
        "[AccountReady, AccountSettings], then 3 contacts + 2 1:1 chats (TH5 A7b) + 2 groups"
    );
    match rows[0].as_slice() {
        [
            BackendEvent::AccountReady {
                own_ids,
                device: None,
                display_name: Some(name),
                number: None,
            },
            BackendEvent::AccountSettings(settings),
        ] => {
            assert!(
                own_ids.as_slice() == [UserId::from_parts("threema", OWN).expect("id")],
                "AccountReady names another identity"
            );
            assert!(
                name.expose() == "Own nickname",
                "AccountReady carries the nickname"
            );
            assert_eq!(
                (settings.read_receipts, settings.typing_indicators),
                (Some(false), Some(true))
            );
        }
        _ => panic!("row 1 is not [AccountReady with the nickname, AccountSettings]"),
    }
    // The three contacts, the 1:1 chats of the two direct ones (TH5 A7b, E1: none for the hidden third),
    // the two groups.
    let names: Vec<&str> = rows[1].iter().map(BackendEvent::name).collect();
    assert_eq!(
        names,
        vec![
            "ContactUpsert",
            "ContactUpsert",
            "ContactUpsert",
            "ConversationUpsert",
            "ConversationUpsert",
            "ConversationUpsert",
            "ConversationUpsert"
        ]
    );
    let directs = rows[1]
        .iter()
        .filter(|e| {
            matches!(e, BackendEvent::ConversationUpsert(p)
                if matches!(p.key, kourier_model::ConvKey::Direct(_)))
        })
        .count();
    assert_eq!(
        directs, 2,
        "a 1:1 chat per direct contact, none for the hidden one"
    );
    let flags: Vec<(Option<bool>, Option<bool>)> = rows[1]
        .iter()
        .filter_map(|e| match e {
            BackendEvent::ContactUpsert(p) => Some((p.hidden, p.blocked)),
            _ => None,
        })
        .collect();
    assert_eq!(
        flags,
        vec![
            (Some(false), Some(false)),
            (Some(false), Some(true)),
            (Some(true), Some(false))
        ],
        "the second contact blocked, the third hidden"
    );
    let created: Vec<i64> = store
        .db
        .read(|conn: &Connection| {
            let mut stmt = conn.prepare("SELECT created_at FROM core_outbox")?;
            let rows = stmt
                .query_map([], |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;
            Ok::<_, kourier_db::Error>(rows)
        })
        .expect("created_at");
    assert_eq!(created, vec![JOIN_NOW_MS + 5; 2]);
}

/// THE REBUILD EQUALITY: what `Rebuild` states over the joined file (`stated_events` = `join::events`
/// over `load_snapshot`) equals what zone 2 wrote, AND equals what the import states in memory
/// (`import_events`, no store): the round trip through the BLOBs loses no contact, group or setting.
/// (A store-level equality; the actor-level Rebuild start is `network::` `a_rebuild_over_a_joined_file_…`.)
#[test]
fn the_rebuild_states_what_zone_2_wrote_and_what_the_import_holds() {
    let fixture = join_fixture("J4BREBL1");
    let store = temp_store();
    import_join(&store.db, &fixture.import, JOIN_LABEL, JOIN_NOW_MS).expect("zone 1");
    let wake = Rc::new(Notify::new());
    ready_join(&store.db, JOIN_NOW_MS, &wake).expect("zone 2");
    let written = outbox_events(&store.db);
    let rebuilt = store.db.read(stated_events).expect("the Rebuild's events");
    let in_memory = import_events(&fixture.import).expect("the import's events");
    assert!(
        rebuilt == written,
        "the Rebuild states other rows than zone 2 wrote"
    );
    assert!(
        in_memory == rebuilt,
        "the stored file states other events than the import holds"
    );
    let groups = rebuilt
        .iter()
        .flatten()
        .filter(|e| {
            matches!(e, BackendEvent::ConversationUpsert(p)
                if matches!(p.key, kourier_model::ConvKey::Group(_)))
        })
        .count();
    assert_eq!(groups, 2, "both groups are stated");
}

/// S4: the marker is absent on a new file and after zone 1, written by `mark_join_ready` (once: the first
/// time stays) and by nothing else but zone 2 and the link's commit.
#[test]
fn the_marker_is_written_once_and_read_back() {
    let t = temp_store();
    assert!(
        !t.db.read(is_join_ready).expect("the marker"),
        "a new file carries the marker"
    );
    t.db.zone(|tx| mark_join_ready(tx, 111)).expect("mark");
    t.db.zone(|tx| mark_join_ready(tx, 222))
        .expect("mark again");
    assert!(t.db.read(is_join_ready).expect("the marker"), "no marker");
    let at: i64 =
        t.db.read(|conn: &Connection| {
            conn.query_row("SELECT at FROM join_ready", [], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect("the time");
    assert_eq!(at, 111, "the second mark overwrote the first time");
    assert_eq!(count(&t.db, "join_ready"), 1, "not exactly one marker row");
}

// THE SEND QUEUE (TH6a S1, schema 4, `migrations/0004_send.sql`) ------------------------------------

/// The ledger's key of the fixture's `transmit` row: 16 lowercase hex digits (the store treats it as a key).
pub(crate) const SEND_LOCAL: &str = "00ff112233445566";
/// The message's own time and the time of its PREPARE.
pub(crate) const SEND_T0: i64 = 1_761_500_000_000;
pub(crate) const SEND_QUEUED: i64 = SEND_T0 + 1;
/// The hash of the fixture's update nonce.
pub(crate) const SEND_UPDATE_NONCE: [u8; 32] = [0xD2; 32];

/// The ID of receiver `n` (a digit).
pub(crate) fn send_id(n: u8) -> String {
    format!("S1RCVR0{n}")
}

/// The sealed box of receiver `n`: the schema's smallest, 88 bytes.
pub(crate) fn send_payload(n: u8) -> Vec<u8> {
    vec![0xB0 + n; 88]
}

/// The sealed reflect: the schema's smallest, 40 bytes.
pub(crate) fn send_reflect() -> Vec<u8> {
    vec![0xE1; 40]
}

/// The sealed update: the schema's smallest, 40 bytes.
pub(crate) fn send_update() -> Vec<u8> {
    vec![0xE2; 40]
}

/// A `transmit` origin under `local`.
pub(crate) fn transmit(local: &str) -> NewOrigin {
    NewOrigin::Transmit {
        local: local.to_owned(),
    }
}

/// A row for `receivers` receivers, each with its box and its hashed CSP nonce `0xC0 + n`, a sealed reflect with its
/// hashed D2D nonce `0xD1`, a Sent update owed. The message id's top bit is set (the i64 of the same 64 bits is -1).
pub(crate) fn send_rows(origin: NewOrigin, receivers: u8) -> SealedRows {
    SealedRows {
        origin,
        kind: "text",
        message_id: u64::MAX,
        conversation: vec![0x0A, 0x02, 0x53, 0x31],
        created_at: SEND_T0,
        has_update: true,
        queued_at: SEND_QUEUED,
        reflect_env: Some(send_reflect()),
        receivers: (1..=receivers)
            .map(|n| NewReceiver {
                identity: send_id(n),
                fate: NewFate::Send {
                    payload: send_payload(n),
                },
            })
            .collect(),
        csp_nonces: (1..=receivers).map(|n| [0xC0 + n; 32]).collect(),
        d2d_nonces: vec![[0xD1; 32]],
    }
}

/// The hashed update of the fixture.
pub(crate) fn sealed_update(envelope: &[u8]) -> SealedUpdate<'_> {
    SealedUpdate {
        envelope,
        nonce: SEND_UPDATE_NONCE,
    }
}

fn tasks(db: &kourier_db::ZoneDb) -> Vec<SendTask> {
    db.read(send_tasks).expect("the send rows")
}

fn receivers(db: &kourier_db::ZoneDb, task: i64) -> Vec<SendReceiver> {
    db.read(|conn: &Connection| send_receivers(conn, task))
        .expect("the receivers")
}

fn the_task(db: &kourier_db::ZoneDb, task: i64) -> SendTask {
    db.read(|conn: &Connection| send_task(conn, task))
        .expect("the read")
        .expect("the row")
}

/// The states of a task's receivers, by ID.
fn receiver_states(db: &kourier_db::ZoneDb, task: i64) -> Vec<ReceiverState> {
    receivers(db, task).iter().map(|r| r.state).collect()
}

/// `INSERT INTO send_task` of a good row (a `job`, `reflected`, nothing sealed), with `over`'s columns instead.
fn task_row(over: &[(&str, &str)]) -> String {
    let columns = [
        ("origin", "'job'"),
        ("kind", "'text'"),
        ("local", "NULL"),
        ("message_id", "1"),
        ("conversation", "X'0A'"),
        ("created_at", "0"),
        ("state", "'reflected'"),
        ("has_update", "0"),
        ("sent_at", "NULL"),
        ("bytes", "0"),
        ("attempts", "0"),
        ("queued_at", "1"),
        ("finished_at", "NULL"),
        ("reflect_env", "NULL"),
        ("update_env", "NULL"),
    ];
    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let values: Vec<&str> = columns
        .iter()
        .map(|(name, value)| {
            over.iter()
                .find(|(other, _)| other == name)
                .map_or(*value, |(_, value)| *value)
        })
        .collect();
    format!(
        "INSERT INTO send_task ({}) VALUES ({})",
        names.join(", "),
        values.join(", ")
    )
}

/// `INSERT INTO send_receiver` under the first task.
fn receiver_row(identity: &str, state: &str, payload: &str) -> String {
    format!(
        "INSERT INTO send_receiver (task, identity, state, payload) VALUES (1, {identity}, {state}, {payload})"
    )
}

/// Each CHECK of schema 4 takes its good row and refuses the bad one, on both sides of each size (N and N+1), with the
/// NULLs a CHECK alone would pass and the types STRICT refuses. Literal SQL on the real migration.
#[test]
fn send_each_check_of_schema_4_refuses_its_bad_row_and_takes_the_good_one() {
    /// A case of one `send_task` row over a good one: its name, the columns it changes, whether it is taken.
    type TaskCase<'a> = (&'a str, Vec<(&'a str, String)>, bool);
    let text = |n: usize| format!("'{}'", "a".repeat(n));
    let blob = |n: usize| format!("zeroblob({n})");
    let local = "'00ff112233445566'".to_owned();
    let s = |v: &str| v.to_owned();
    let task_cases: Vec<TaskCase<'_>> = vec![
        ("the good row", vec![], true),
        // origin, and its tie to `local`
        (
            "a transmit row with its local",
            vec![("origin", s("'transmit'")), ("local", local.clone())],
            true,
        ),
        (
            "a transmit row without a local",
            vec![("origin", s("'transmit'"))],
            false,
        ),
        ("a job with a local", vec![("local", local.clone())], false),
        ("another origin", vec![("origin", s("'other'"))], false),
        ("no origin", vec![("origin", s("NULL"))], false),
        // kind
        ("a kind of 0", vec![("kind", text(0))], false),
        ("a kind of 1", vec![("kind", text(1))], true),
        ("a kind of 32", vec![("kind", text(32))], true),
        ("a kind of 33", vec![("kind", text(33))], false),
        (
            "a kind with digits and a dash",
            vec![("kind", s("'no-key-9'"))],
            true,
        ),
        ("a kind with a capital", vec![("kind", s("'Text'"))], false),
        (
            "a kind with an underscore",
            vec![("kind", s("'a_b'"))],
            false,
        ),
        ("a kind with a space", vec![("kind", s("'a b'"))], false),
        ("no kind", vec![("kind", s("NULL"))], false),
        ("a kind as a blob", vec![("kind", s("X'61'"))], false),
        // local, on a transmit row
        (
            "a local of 15",
            vec![
                ("origin", s("'transmit'")),
                ("local", s("'00ff11223344556'")),
            ],
            false,
        ),
        (
            "a local of 17",
            vec![
                ("origin", s("'transmit'")),
                ("local", s("'00ff1122334455667'")),
            ],
            false,
        ),
        (
            "a local with a capital",
            vec![
                ("origin", s("'transmit'")),
                ("local", s("'00FF112233445566'")),
            ],
            false,
        ),
        (
            "a local that is not hex",
            vec![
                ("origin", s("'transmit'")),
                ("local", s("'00ff11223344556g'")),
            ],
            false,
        ),
        // message_id
        ("no message id", vec![("message_id", s("NULL"))], false),
        (
            "a message id of the high bit",
            vec![("message_id", s("-1"))],
            true,
        ),
        (
            "a message id as text",
            vec![("message_id", s("'x'"))],
            false,
        ),
        // conversation
        (
            "a conversation of 0",
            vec![("conversation", blob(0))],
            false,
        ),
        ("a conversation of 1", vec![("conversation", blob(1))], true),
        (
            "a conversation of 256",
            vec![("conversation", blob(256))],
            true,
        ),
        (
            "a conversation of 257",
            vec![("conversation", blob(257))],
            false,
        ),
        ("no conversation", vec![("conversation", s("NULL"))], false),
        (
            "a conversation as text",
            vec![("conversation", s("'a'"))],
            false,
        ),
        // created_at
        ("created at 0", vec![("created_at", s("0"))], true),
        ("created at -1", vec![("created_at", s("-1"))], false),
        ("no created_at", vec![("created_at", s("NULL"))], false),
        // state, and what each state holds
        ("another state", vec![("state", s("'sent'"))], false),
        ("no state", vec![("state", s("NULL"))], false),
        (
            "prepared with its reflect",
            vec![("state", s("'prepared'")), ("reflect_env", blob(40))],
            true,
        ),
        (
            "prepared without a reflect",
            vec![("state", s("'prepared'"))],
            false,
        ),
        (
            "reflected with a reflect (a top-up may run)",
            vec![("reflect_env", blob(40))],
            true,
        ),
        (
            "acked with its update",
            vec![("state", s("'acked'")), ("update_env", blob(40))],
            true,
        ),
        (
            "acked without an update",
            vec![("state", s("'acked'"))],
            false,
        ),
        (
            "done with its time",
            vec![("state", s("'done'")), ("finished_at", s("5"))],
            true,
        ),
        ("done without a time", vec![("state", s("'done'"))], false),
        (
            "done with a reflect",
            vec![
                ("state", s("'done'")),
                ("finished_at", s("5")),
                ("reflect_env", blob(40)),
            ],
            false,
        ),
        (
            "done with an update",
            vec![
                ("state", s("'done'")),
                ("finished_at", s("5")),
                ("update_env", blob(40)),
            ],
            false,
        ),
        // has_update
        ("has_update 1", vec![("has_update", s("1"))], true),
        ("has_update 2", vec![("has_update", s("2"))], false),
        ("has_update -1", vec![("has_update", s("-1"))], false),
        ("no has_update", vec![("has_update", s("NULL"))], false),
        // sent_at, finished_at
        ("a sent_at", vec![("sent_at", s("7"))], true),
        ("a sent_at as text", vec![("sent_at", s("'x'"))], false),
        (
            "a finished_at on an open row",
            vec![("finished_at", s("5"))],
            true,
        ),
        // bytes
        ("bytes 0", vec![("bytes", s("0"))], true),
        ("bytes -1", vec![("bytes", s("-1"))], false),
        ("no bytes", vec![("bytes", s("NULL"))], false),
        // attempts
        ("attempts 1", vec![("attempts", s("1"))], true),
        ("attempts -1", vec![("attempts", s("-1"))], false),
        ("attempts NULL", vec![("attempts", s("NULL"))], false),
        // queued_at
        ("no queued_at", vec![("queued_at", s("NULL"))], false),
        // reflect_env: 24 of nonce and 16 of tag at least, one reflection at most
        ("a reflect of 39", vec![("reflect_env", blob(39))], false),
        ("a reflect of 40", vec![("reflect_env", blob(40))], true),
        (
            "a reflect of 65516",
            vec![("reflect_env", blob(65_516))],
            true,
        ),
        (
            "a reflect of 65517",
            vec![("reflect_env", blob(65_517))],
            false,
        ),
        ("a reflect as text", vec![("reflect_env", text(40))], false),
        // update_env
        ("an update of 39", vec![("update_env", blob(39))], false),
        ("an update of 40", vec![("update_env", blob(40))], true),
        (
            "an update of 65516",
            vec![("update_env", blob(65_516))],
            true,
        ),
        (
            "an update of 65517",
            vec![("update_env", blob(65_517))],
            false,
        ),
        ("an update as text", vec![("update_env", text(40))], false),
    ];
    let many = |statements: Vec<String>, expected: bool, case: &str| {
        let statements: Vec<&str> = statements.iter().map(String::as_str).collect();
        assert_eq!(takes(&statements), expected, "{case}");
    };
    for (case, over, expected) in task_cases {
        let over: Vec<(&str, &str)> = over.iter().map(|(n, v)| (*n, v.as_str())).collect();
        many(vec![task_row(&over)], expected, case);
    }
    // Without the attempts column, its DEFAULT.
    many(
        vec![
            "INSERT INTO send_task (origin, kind, message_id, conversation, created_at, state, has_update, bytes, queued_at) \
             VALUES ('job', 'text', 1, X'0A', 0, 'reflected', 0, 0, 1)"
                .to_owned(),
        ],
        true,
        "attempts by its default",
    );

    // The ledger's key: unique, whatever the state; a job has none.
    let of_local = |local: &str, state: &[(&str, &str)]| {
        let local = format!("'{local}'");
        let mut over = vec![("origin", "'transmit'"), ("local", local.as_str())];
        over.extend_from_slice(state);
        task_row(&over)
    };
    let one = "00ff112233445566";
    let two = "00ff112233445567";
    many(
        vec![of_local(one, &[]), of_local(one, &[])],
        false,
        "a local twice",
    );
    many(
        vec![of_local(one, &[]), of_local(two, &[])],
        true,
        "two locals",
    );
    many(
        vec![
            of_local(one, &[("state", "'done'"), ("finished_at", "5")]),
            of_local(one, &[]),
        ],
        false,
        "a local twice, the first done",
    );
    many(
        vec![task_row(&[]), task_row(&[])],
        true,
        "two jobs have no local",
    );

    // send_receiver, under a task.
    let task = task_row(&[]);
    let on_task = |row: String| vec![task.clone(), row];
    let pending_box = "zeroblob(88)";
    for (case, row, expected) in [
        (
            "an ID of 7",
            receiver_row("'S1RCV07'", "'blocked'", "NULL"),
            false,
        ),
        (
            "an ID of 8",
            receiver_row("'S1RCVR08'", "'blocked'", "NULL"),
            true,
        ),
        (
            "an ID of 9",
            receiver_row("'S1RCVR009'", "'blocked'", "NULL"),
            false,
        ),
        (
            "a gateway ID",
            receiver_row("'*S1GATE1'", "'blocked'", "NULL"),
            true,
        ),
        ("no ID", receiver_row("NULL", "'blocked'", "NULL"), false),
        (
            "pending with its box",
            receiver_row("'S1RCVR08'", "'pending'", pending_box),
            true,
        ),
        (
            "pending without a box",
            receiver_row("'S1RCVR08'", "'pending'", "NULL"),
            false,
        ),
        ("acked", receiver_row("'S1RCVR08'", "'acked'", "NULL"), true),
        (
            "acked with a box",
            receiver_row("'S1RCVR08'", "'acked'", pending_box),
            false,
        ),
        (
            "blocked with a box",
            receiver_row("'S1RCVR08'", "'blocked'", pending_box),
            false,
        ),
        (
            "invalid",
            receiver_row("'S1RCVR08'", "'invalid'", "NULL"),
            true,
        ),
        (
            "invalid with a box",
            receiver_row("'S1RCVR08'", "'invalid'", pending_box),
            false,
        ),
        (
            "no-key",
            receiver_row("'S1RCVR08'", "'no-key'", "NULL"),
            true,
        ),
        (
            "no-key with a box",
            receiver_row("'S1RCVR08'", "'no-key'", pending_box),
            false,
        ),
        (
            "omitted",
            receiver_row("'S1RCVR08'", "'omitted'", "NULL"),
            true,
        ),
        (
            "omitted with a box",
            receiver_row("'S1RCVR08'", "'omitted'", pending_box),
            false,
        ),
        (
            "another state",
            receiver_row("'S1RCVR08'", "'sent'", "NULL"),
            false,
        ),
        (
            "no state",
            receiver_row("'S1RCVR08'", "NULL", "NULL"),
            false,
        ),
        (
            "a box of 87",
            receiver_row("'S1RCVR08'", "'pending'", "zeroblob(87)"),
            false,
        ),
        (
            "a box of 88",
            receiver_row("'S1RCVR08'", "'pending'", "zeroblob(88)"),
            true,
        ),
        (
            "a box of 8170",
            receiver_row("'S1RCVR08'", "'pending'", "zeroblob(8170)"),
            true,
        ),
        (
            "a box of 8171",
            receiver_row("'S1RCVR08'", "'pending'", "zeroblob(8171)"),
            false,
        ),
        (
            "a box as text",
            receiver_row("'S1RCVR08'", "'pending'", &text(88)),
            false,
        ),
    ] {
        many(on_task(row), expected, case);
    }
    many(
        vec![
            task.clone(),
            receiver_row("'S1RCVR08'", "'acked'", "NULL"),
            receiver_row("'S1RCVR08'", "'blocked'", "NULL"),
        ],
        false,
        "a receiver twice in a task",
    );
    many(
        vec![
            task.clone(),
            task.clone(),
            receiver_row("'S1RCVR08'", "'acked'", "NULL"),
            "INSERT INTO send_receiver (task, identity, state) VALUES (2, 'S1RCVR08', 'acked')"
                .to_owned(),
        ],
        true,
        "one receiver in two tasks",
    );
    many(
        vec![receiver_row("'S1RCVR08'", "'acked'", "NULL")],
        false,
        "a receiver of no task",
    );
    many(
        vec![
            task.clone(),
            "INSERT INTO send_receiver (task, identity, state) VALUES (NULL, 'S1RCVR08', 'acked')"
                .to_owned(),
        ],
        false,
        "a receiver with no task named",
    );
}

/// TH7's "Send Again" (brief B:229): a `no-key` receiver of a `done` row gets its key later. Its state -> `pending`,
/// its box set, one more sealed reflect, the row back from `done` to `prepared`: the frozen CHECKs hold it, no migration
/// is owed.
#[test]
fn send_a_top_up_of_a_done_row_holds_the_checks_of_schema_4() {
    let t = temp_store();
    let mut rows = send_rows(transmit(SEND_LOCAL), 1);
    rows.receivers.push(NewReceiver {
        identity: send_id(2),
        fate: NewFate::NoKey,
    });
    let task = prepare_send_zone(&t.db, &rows, &[]).expect("prepare");
    mark_send_reflected(&t.db, task).expect("reflected");
    let update = send_update();
    let name = send_id(1);
    mark_send_acks(&t.db, task, &[&name], Some(sealed_update(&update)), SEND_T0).expect("acks");
    mark_send_done(&t.db, task, SEND_T0, SEND_T0).expect("done");
    t.db.zone(|tx| {
        tx.execute(
            "UPDATE send_receiver SET state = 'pending', payload = zeroblob(88) WHERE task = ?1 AND state = 'no-key'",
            [task],
        )
        .map_err(kourier_db::Error::from)?;
        tx.execute(
            "UPDATE send_task SET state = 'prepared', reflect_env = zeroblob(40) WHERE id = ?1",
            [task],
        )
        .map_err(kourier_db::Error::from)?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the frozen schema takes the top-up");
    assert_eq!(the_task(&t.db, task).state, SendState::Prepared);
    assert_eq!(
        receiver_states(&t.db, task),
        [ReceiverState::Acked, ReceiverState::Pending]
    );
}

/// ZONE PREPARE writes the literal rows, and a `transmit` row walks its four states; every column is checked at each
/// step, and only the zone that owes a nonce writes it.
#[test]
fn send_a_transmit_row_walks_its_zones_with_literal_rows() {
    let t = temp_store();
    let db = &t.db;
    let mut rows = send_rows(transmit(SEND_LOCAL), 3);
    rows.receivers.push(NewReceiver {
        identity: send_id(4),
        fate: NewFate::Blocked,
    });
    let task = prepare_send_zone(db, &rows, &[]).expect("prepare");
    assert_eq!(task, 1, "the first row of the queue");
    let row = the_task(db, task);
    assert_eq!(row.origin, SendOrigin::Transmit);
    assert_eq!(row.kind, "text");
    assert!(row.local.as_deref() == Some(SEND_LOCAL), "the ledger's key");
    assert!(row.message_id == u64::MAX, "the id's 64 bits come back");
    assert!(
        row.conversation == [0x0A, 0x02, 0x53, 0x31],
        "the conversation's bytes"
    );
    assert_eq!(row.created_at, SEND_T0);
    assert_eq!(row.state, SendState::Prepared);
    assert!(row.has_update);
    assert_eq!(row.sent_at, None);
    assert_eq!(row.bytes, 40 + 3 * 88, "the reflect and the three boxes");
    assert_eq!(row.attempts, 0);
    assert_eq!(row.queued_at, SEND_QUEUED);
    assert_eq!(row.finished_at, None);
    assert!(
        row.reflect_env == Some(send_reflect()),
        "the sealed reflect"
    );
    assert!(row.update_env.is_none(), "no update yet");
    let stored = receivers(db, task);
    assert_eq!(
        stored.iter().map(|r| r.state).collect::<Vec<_>>(),
        [
            ReceiverState::Pending,
            ReceiverState::Pending,
            ReceiverState::Pending,
            ReceiverState::Blocked
        ]
    );
    for (r, n) in stored.iter().zip(1u8..) {
        assert!(r.identity == send_id(n), "receiver {n}'s ID");
        assert!(
            r.payload == (n < 4).then(|| send_payload(n)),
            "receiver {n}'s box"
        );
    }
    let nonce = |scope: NonceScope, hash: [u8; 32]| {
        db.read(|conn: &Connection| contains_nonce(conn, scope, &hash))
            .expect("the nonce")
    };
    for n in 1..=3 {
        assert!(nonce(NonceScope::Csp, [0xC0 + n; 32]), "CSP nonce {n}");
    }
    assert!(
        nonce(NonceScope::D2d, [0xD1; 32]),
        "the reflect's D2D nonce"
    );
    assert!(
        !nonce(NonceScope::D2d, SEND_UPDATE_NONCE),
        "the update's nonce before its zone"
    );
    let ledger = db
        .read(|conn: &Connection| send_ledger(conn, SEND_LOCAL))
        .expect("the ledger");
    assert!(ledger == Some(row), "the ledger reads the same row");

    mark_send_reflected(db, task).expect("reflected");
    let row = the_task(db, task);
    assert_eq!(row.state, SendState::Reflected);
    assert!(row.reflect_env.is_none(), "the reflect is emptied");

    let (one, two, three) = (send_id(1), send_id(2), send_id(3));
    let update = send_update();
    // A pass that leaves one pending: no update and no nonce, though the machine had one at hand.
    assert_eq!(
        mark_send_acks(
            db,
            task,
            &[&one, &two],
            Some(sealed_update(&update)),
            SEND_T0
        )
        .expect("two acks"),
        SendEnd::Open { pending: 1 }
    );
    assert_eq!(
        receiver_states(db, task),
        [
            ReceiverState::Acked,
            ReceiverState::Acked,
            ReceiverState::Pending,
            ReceiverState::Blocked
        ]
    );
    assert!(
        receivers(db, task)[0].payload.is_none(),
        "an acked box is emptied"
    );
    assert!(the_task(db, task).update_env.is_none(), "no update yet");
    assert!(
        !nonce(NonceScope::D2d, SEND_UPDATE_NONCE),
        "the update's nonce before the last pass"
    );
    // An ack that names nobody pending changes nothing.
    assert_eq!(
        mark_send_acks(db, task, &[&one, "S1NOBODY"], None, SEND_T0).expect("no new ack"),
        SendEnd::Open { pending: 1 }
    );
    // The last.
    assert_eq!(
        mark_send_acks(db, task, &[&three], Some(sealed_update(&update)), SEND_T0)
            .expect("the last ack"),
        SendEnd::Acked
    );
    let row = the_task(db, task);
    assert_eq!(row.state, SendState::Acked);
    assert!(row.update_env == Some(send_update()), "the sealed update");
    assert!(
        nonce(NonceScope::D2d, SEND_UPDATE_NONCE),
        "the update's nonce"
    );

    assert_eq!(
        mark_send_done(db, task, SEND_T0 + 7, SEND_T0 + 9).expect("done"),
        SendEnd::Done
    );
    let row = the_task(db, task);
    assert_eq!(row.state, SendState::Done);
    assert_eq!(row.sent_at, Some(SEND_T0 + 7));
    assert_eq!(row.finished_at, Some(SEND_T0 + 9));
    assert!(
        row.reflect_env.is_none() && row.update_env.is_none(),
        "a done row holds no sealed byte"
    );
    assert_eq!(row.bytes, 0, "a done row holds nothing");
    assert!(
        receivers(db, task).iter().all(|r| r.payload.is_none()),
        "a done row's receivers hold no box"
    );
    assert_eq!(
        receiver_states(db, task),
        [
            ReceiverState::Acked,
            ReceiverState::Acked,
            ReceiverState::Acked,
            ReceiverState::Blocked
        ],
        "the ledger keeps who was acked"
    );
}

/// What each kind of row does at its end: a type that does not reflect starts `reflected`; a row with no update ends at
/// its last ACKS zone (`done`, or gone for a job, its receivers with it); a job with an update ends at ZONE DONE, gone;
/// a notes group (nobody to send to) goes through an empty ACKS zone.
#[test]
fn send_each_kind_of_row_ends_as_the_state_machine_says() {
    let t = temp_store();
    let db = &t.db;
    let one = send_id(1);
    let update = send_update();

    // An FS reject's shape: a job, no reflect, no update.
    let mut job = send_rows(NewOrigin::Job, 1);
    job.reflect_env = None;
    job.d2d_nonces.clear();
    job.has_update = false;
    let id = prepare_send_zone(db, &job, &[]).expect("a job");
    let row = the_task(db, id);
    assert_eq!(
        row.state,
        SendState::Reflected,
        "no reflect: it starts reflected"
    );
    assert!(row.local.is_none(), "a job has no local");
    assert_eq!(row.bytes, 88, "no reflect to count");
    assert_eq!(
        mark_send_acks(db, id, &[&one], None, SEND_T0).expect("its ack"),
        SendEnd::Deleted
    );
    assert_eq!(count(db, "send_task"), 0, "the job's row is gone");
    assert_eq!(count(db, "send_receiver"), 0, "its receivers went with it");

    // A delivery receipt's shape: a job that reflects and has no update; ACKS deletes it as well.
    let mut receipt = send_rows(NewOrigin::Job, 1);
    receipt.has_update = false;
    let id = prepare_send_zone(db, &receipt, &[]).expect("a receipt");
    assert_eq!(the_task(db, id).state, SendState::Prepared);
    assert_eq!(id, 2, "the queue's order is never reused");
    mark_send_reflected(db, id).expect("reflected");
    assert_eq!(
        mark_send_acks(db, id, &[&one], None, SEND_T0).expect("its ack"),
        SendEnd::Deleted
    );

    // A job that owes an update: gone at ZONE DONE.
    let id = prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[]).expect("a job");
    mark_send_reflected(db, id).expect("reflected");
    assert_eq!(
        mark_send_acks(db, id, &[&one], Some(sealed_update(&update)), SEND_T0).expect("its ack"),
        SendEnd::Acked
    );
    assert_eq!(
        mark_send_done(db, id, SEND_T0, SEND_T0).expect("done"),
        SendEnd::Deleted
    );
    assert_eq!(count(db, "send_task"), 0, "the job's row is gone");
    assert_eq!(count(db, "send_receiver"), 0, "its receivers went with it");

    // A transmit type with no update: done at its last ACKS zone, no server time.
    let mut plain = send_rows(transmit(SEND_LOCAL), 1);
    plain.has_update = false;
    let id = prepare_send_zone(db, &plain, &[]).expect("a transmit");
    mark_send_reflected(db, id).expect("reflected");
    assert_eq!(
        mark_send_acks(db, id, &[&one], None, SEND_T0 + 3).expect("its ack"),
        SendEnd::Done
    );
    let row = the_task(db, id);
    assert_eq!(row.state, SendState::Done);
    assert_eq!(row.finished_at, Some(SEND_T0 + 3));
    assert_eq!(row.sent_at, None);

    // A notes group: nobody to send to, the empty ACKS zone writes the update.
    let notes = send_rows(transmit("00ff112233445567"), 0);
    let id = prepare_send_zone(db, &notes, &[]).expect("a notes group");
    mark_send_reflected(db, id).expect("reflected");
    assert_eq!(
        mark_send_acks(db, id, &[], Some(sealed_update(&update)), SEND_T0).expect("the empty zone"),
        SendEnd::Acked
    );
}

/// A zone on a row in the wrong state fails by name, and what it wrote before the check is rolled back. The second
/// `local` meets `send_by_local` (the SQLite code of a UNIQUE failure, 2067), and the first row stays.
#[test]
fn send_a_zone_on_the_wrong_row_fails_and_leaves_it_as_it_was() {
    let t = temp_store();
    let db = &t.db;
    let (one, two) = (send_id(1), send_id(2));
    let update = send_update();
    let task = prepare_send_zone(db, &send_rows(transmit(SEND_LOCAL), 2), &[]).expect("prepare");
    let refused = |what: &str, r: Result<SendEnd, StoreError>| {
        assert!(
            matches!(r, Err(StoreError::Failed(_))),
            "{what} is not refused by name"
        );
    };
    // `prepared`: only the reflect-ack moves it.
    refused(
        "the acks of a prepared row",
        mark_send_acks(db, task, &[&one], None, SEND_T0),
    );
    assert_eq!(
        receiver_states(db, task),
        [ReceiverState::Pending, ReceiverState::Pending],
        "the refused zone's ack stayed"
    );
    refused(
        "the end of a prepared row",
        mark_send_done(db, task, SEND_T0, SEND_T0),
    );
    // `reflected`: not twice.
    mark_send_reflected(db, task).expect("reflected");
    assert!(
        matches!(mark_send_reflected(db, task), Err(StoreError::Failed(_))),
        "a second reflect-ack"
    );
    refused(
        "the end of a reflected row",
        mark_send_done(db, task, SEND_T0, SEND_T0),
    );
    // The last pass of a row that owes an update, without one: its acks roll back.
    refused(
        "the last pass without its update",
        mark_send_acks(db, task, &[&one, &two], None, SEND_T0),
    );
    assert_eq!(
        receiver_states(db, task),
        [ReceiverState::Pending, ReceiverState::Pending],
        "the refused zone's acks stayed"
    );
    // `acked`: the boxes are over.
    mark_send_acks(
        db,
        task,
        &[&one, &two],
        Some(sealed_update(&update)),
        SEND_T0,
    )
    .expect("acks");
    refused(
        "a pass over an acked row",
        mark_send_acks(db, task, &[], None, SEND_T0),
    );
    // A row that is not there.
    assert!(
        matches!(mark_send_reflected(db, 99), Err(StoreError::Failed(_))),
        "a reflect-ack of no row"
    );
    refused(
        "a pass of no row",
        mark_send_acks(db, 99, &[], None, SEND_T0),
    );
    refused(
        "the end of no row",
        mark_send_done(db, 99, SEND_T0, SEND_T0),
    );
    // The ledger: one row per local.
    assert!(
        matches!(
            prepare_send_zone(db, &send_rows(transmit(SEND_LOCAL), 1), &[]),
            Err(StoreError::Sql {
                code: Some(2067),
                ..
            })
        ),
        "a second row of one local"
    );
    assert_eq!(count(db, "send_task"), 1, "the refused row stayed");
}

/// `seed` open rows of `origin` (`reflected`, nothing sealed), each counting `bytes` and queued at `queued_at`, in one
/// zone. A `transmit` seed that is not to be taken by an eviction is queued at the new row's own time (`SEND_QUEUED`: age
/// 0), never at an old one.
fn seed_open(db: &kourier_db::ZoneDb, origin: &str, seed: i64, bytes: i64, queued_at: i64) {
    db.zone(|tx| {
        tx.execute(
            "WITH RECURSIVE k(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM k WHERE i < ?1) \
             INSERT INTO send_task (origin, kind, local, message_id, conversation, created_at, state, \
               has_update, bytes, queued_at) \
             SELECT ?2, 'seed', CASE WHEN ?2 = 'transmit' THEN printf('aa%014x', i) END, i, X'0A', 0, \
               'reflected', 0, ?3, ?4 FROM k",
            kourier_db::rusqlite::params![seed, origin, bytes, queued_at],
        )
        .map_err(kourier_db::Error::from)?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the seed");
}

/// The open rows of `origin`, as the budget's own look would count them.
fn open_of(db: &kourier_db::ZoneDb, origin: &str) -> i64 {
    db.read(|conn: &Connection| {
        conn.query_row(
            "SELECT count(*) FROM send_task WHERE state <> 'done' AND origin = ?1",
            [origin],
            |r| r.get(0),
        )
        .map_err(kourier_db::Error::from)
    })
    .expect("the count")
}

/// The real budgets, written out (R3): a `transmit` row counts against 2,000 tasks and 64 MiB, a `job` against its own
/// 2,000 and 16 MiB.
#[test]
fn send_the_budgets_are_the_two_of_the_decision() {
    assert_eq!(
        TRANSMIT_BUDGET,
        SendBudget {
            tasks: 2_000,
            bytes: 67_108_864
        }
    );
    assert_eq!(
        JOB_BUDGET,
        SendBudget {
            tasks: 2_000,
            bytes: 16_777_216
        }
    );
}

/// N and N+1 of the rows of `transmit`: the 2,000th is taken, the 2,001st is `QueueFull` and leaves nothing (no row, no
/// receiver, no nonce). AND A JOB IS STILL TAKEN under the full `transmit` budget (R3: an un-attached `transmit` row never
/// stops the receive path's job). `done` rows do not count: once they are done, a `transmit` row is taken again.
#[test]
fn send_the_transmit_budget_takes_2000_refuses_2001_and_a_job_still_goes() {
    let t = temp_store();
    let db = &t.db;
    seed_open(db, "transmit", 1_999, 0, SEND_QUEUED);
    prepare_send_zone(db, &send_rows(transmit(SEND_LOCAL), 1), &[]).expect("the 2,000th");
    assert_eq!(open_of(db, "transmit"), 2_000);
    let mut over = send_rows(transmit("00ff112233445567"), 1);
    over.csp_nonces = vec![[0x77; 32]];
    assert_eq!(
        prepare_send_zone(db, &over, &[]),
        Err(StoreError::QueueFull),
        "the 2,001st"
    );
    assert_eq!(open_of(db, "transmit"), 2_000, "the refused row stayed");
    assert!(
        !db.read(|conn: &Connection| contains_nonce(conn, NonceScope::Csp, &[0x77; 32]))
            .expect("the nonce"),
        "the refused row's nonce stayed"
    );
    // The job's own budget is untouched.
    prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[])
        .expect("a job under a full transmit budget");
    assert_eq!(open_of(db, "job"), 1);
    // Done rows leave the budget.
    db.zone(|tx| {
        tx.execute(
            "UPDATE send_task SET state = 'done', finished_at = 1 WHERE kind = 'seed'",
            [],
        )
        .map_err(kourier_db::Error::from)?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("seed rows done");
    prepare_send_zone(db, &over, &[]).expect("the budget has room again");
}

/// The same for `job` rows, and a `transmit` row is still taken under a full `job` budget.
#[test]
fn send_the_job_budget_takes_2000_refuses_2001_and_a_transmit_still_goes() {
    let t = temp_store();
    let db = &t.db;
    seed_open(db, "job", 1_999, 0, SEND_QUEUED);
    prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[]).expect("the 2,000th");
    assert_eq!(open_of(db, "job"), 2_000);
    assert_eq!(
        prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[]),
        Err(StoreError::QueueFull),
        "the 2,001st"
    );
    assert_eq!(open_of(db, "job"), 2_000, "the refused row stayed");
    prepare_send_zone(db, &send_rows(transmit(SEND_LOCAL), 1), &[]).expect("a transmit");
}

/// N and N+1 of the bytes, for both budgets: a row of 128 bytes (a reflect of 40 and a box of 88) is taken when exactly
/// 128 are left and refused when 127 are, counted by `bytes` alone. No 64 MiB is written: the sum reads no BLOB.
#[test]
fn send_the_byte_budgets_take_the_last_byte_and_refuse_the_next() {
    for (origin, budget, make) in [
        ("transmit", 67_108_864, transmit(SEND_LOCAL)),
        ("job", 16_777_216, NewOrigin::Job),
    ] {
        let t = temp_store();
        seed_open(&t.db, origin, 1, budget - 128, SEND_QUEUED);
        let id =
            prepare_send_zone(&t.db, &send_rows(make.clone(), 1), &[]).expect("exactly the budget");
        assert_eq!(the_task(&t.db, id).bytes, 128, "{origin}");

        let t = temp_store();
        seed_open(&t.db, origin, 1, budget - 127, SEND_QUEUED);
        assert_eq!(
            prepare_send_zone(&t.db, &send_rows(make, 1), &[]),
            Err(StoreError::QueueFull),
            "{origin}: one byte over"
        );
        assert_eq!(
            open_of(&t.db, origin),
            1,
            "{origin}: the refused row stayed"
        );
    }
}

// S-M1 (a): `bytes` IS WHAT THE ROW HOLDS ---------------------------------------------------------------------------

/// What the row HOLDS, read from the stored columns the way the invariant says: the reflect, the update and every box still
/// pending, a NULL counting 0.
fn held_by(db: &kourier_db::ZoneDb, task: i64) -> i64 {
    let row = the_task(db, task);
    let boxes: usize = receivers(db, task)
        .iter()
        .map(|r| r.payload.as_ref().map_or(0, Vec::len))
        .sum();
    let sealed =
        row.reflect_env.as_ref().map_or(0, Vec::len) + row.update_env.as_ref().map_or(0, Vec::len);
    i64::try_from(sealed + boxes).expect("a length")
}

/// `bytes` is the literal value AND what the row holds.
fn assert_holds(db: &kourier_db::ZoneDb, task: i64, literal: i64, what: &str) {
    let bytes = the_task(db, task).bytes;
    assert_eq!(bytes, literal, "{what}: the row's bytes");
    assert_eq!(
        bytes,
        held_by(db, task),
        "{what}: bytes is not what the row holds"
    );
}

/// A row of `receivers` boxes of the given lengths (the IDs of `send_id`), a reflect of 100 bytes and no nonce: lengths no
/// two of which are equal, so a sum of the wrong things shows.
fn lengthy_rows(origin: NewOrigin, lengths: &[usize]) -> SealedRows {
    let mut rows = send_rows(origin, 0);
    rows.reflect_env = Some(vec![0xE1; 100]);
    rows.receivers = (1..)
        .zip(lengths)
        .map(|(n, len)| NewReceiver {
            identity: send_id(n),
            fate: NewFate::Send {
                payload: vec![0xB0 + n; *len],
            },
        })
        .collect();
    rows.csp_nonces = Vec::new();
    rows
}

/// A 1:1 and a group of three walk every state, and after EVERY zone `bytes` is the literal value and what the row holds:
/// the reflect's ack lowers it by the reflect, each ack by the boxes it empties, the last ack's pass raises it to the
/// update, the done row holds nothing; a pass with no ack changes nothing.
#[test]
fn send_bytes_is_what_the_row_holds_after_every_zone() {
    let t = temp_store();
    let db = &t.db;
    let update = vec![0xE2; 60];
    let (one, two, three) = (send_id(1), send_id(2), send_id(3));

    // A 1:1: a reflect of 100 and one box of 200.
    let task = prepare_send_zone(db, &lengthy_rows(transmit("00ff112233440001"), &[200]), &[])
        .expect("a 1:1");
    assert_holds(db, task, 300, "1:1 prepared");
    mark_send_reflected(db, task).expect("reflected");
    assert_holds(db, task, 200, "1:1 reflected");
    assert_eq!(
        mark_send_acks(db, task, &[&one], Some(sealed_update(&update)), SEND_T0),
        Ok(SendEnd::Acked)
    );
    assert_holds(db, task, 60, "1:1 acked, the update written");
    mark_send_done(db, task, SEND_T0 + 1, SEND_T0 + 2).expect("done");
    assert_holds(db, task, 0, "1:1 done");

    // A group of three boxes (200, 300, 400) and a blocked member.
    let mut rows = lengthy_rows(transmit("00ff112233440002"), &[200, 300, 400]);
    rows.receivers.push(NewReceiver {
        identity: send_id(4),
        fate: NewFate::Blocked,
    });
    let task = prepare_send_zone(db, &rows, &[]).expect("a group");
    assert_holds(db, task, 1_000, "group prepared");
    mark_send_reflected(db, task).expect("reflected");
    assert_holds(db, task, 900, "group reflected");
    let open = |pending| Ok(SendEnd::Open { pending });
    assert_eq!(mark_send_acks(db, task, &[&one], None, SEND_T0), open(2));
    assert_holds(db, task, 700, "group, the first box acked");
    assert_eq!(mark_send_acks(db, task, &[], None, SEND_T0), open(2));
    assert_holds(db, task, 700, "group, a pass with no ack");
    assert_eq!(mark_send_acks(db, task, &[&two], None, SEND_T0), open(1));
    assert_holds(db, task, 400, "group, the second box acked");
    assert_eq!(
        mark_send_acks(db, task, &[&three], Some(sealed_update(&update)), SEND_T0),
        Ok(SendEnd::Acked)
    );
    assert_holds(db, task, 60, "group acked: the update only");
    mark_send_done(db, task, SEND_T0 + 1, SEND_T0 + 2).expect("done");
    assert_holds(db, task, 0, "group done");

    // A row with no update ends at its last ACKS zone: nothing held either.
    let mut rows = lengthy_rows(transmit("00ff112233440003"), &[200]);
    rows.has_update = false;
    let task = prepare_send_zone(db, &rows, &[]).expect("no update owed");
    mark_send_reflected(db, task).expect("reflected");
    assert_eq!(
        mark_send_acks(db, task, &[&one], None, SEND_T0),
        Ok(SendEnd::Done)
    );
    assert_holds(db, task, 0, "no update owed: done at its last ACKS zone");
}

/// A group of 256 with 255 acked holds only the one box that is still pending, then only the update.
#[test]
fn send_a_group_with_all_but_one_acked_holds_only_that_one_box() {
    let t = temp_store();
    let db = &t.db;
    // The box of receiver n: 88 + (37 n mod 500) bytes, so the last one is 88 + 472 = 560.
    let length = |n: usize| 88 + (n * 37) % 500;
    let ids: Vec<String> = (1..=256).map(|n| format!("R{n:07}")).collect();
    let mut rows = send_rows(transmit(SEND_LOCAL), 0);
    rows.csp_nonces = Vec::new();
    rows.receivers = ids
        .iter()
        .zip(1..)
        .map(|(identity, n)| NewReceiver {
            identity: identity.clone(),
            fate: NewFate::Send {
                payload: vec![0xB0; length(n)],
            },
        })
        .collect();
    let task = prepare_send_zone(db, &rows, &[]).expect("a group of 256");
    let boxes: usize = (1..=256).map(length).sum();
    assert_holds(
        db,
        task,
        i64::try_from(40 + boxes).expect("a length"),
        "256 pending",
    );
    mark_send_reflected(db, task).expect("reflected");
    assert_holds(
        db,
        task,
        i64::try_from(boxes).expect("a length"),
        "reflected",
    );
    let names: Vec<&str> = ids[..255].iter().map(String::as_str).collect();
    assert_eq!(
        mark_send_acks(db, task, &names, None, SEND_T0),
        Ok(SendEnd::Open { pending: 1 })
    );
    assert_holds(db, task, 560, "255 of 256 acked: one box");
    let update = send_update();
    assert_eq!(
        mark_send_acks(
            db,
            task,
            &[ids[255].as_str()],
            Some(sealed_update(&update)),
            SEND_T0
        ),
        Ok(SendEnd::Acked)
    );
    assert_holds(db, task, 40, "256 of 256 acked: the update only");
}

// S-M1 (b): EVICTION UNDER PRESSURE ---------------------------------------------------------------------------------

const HOUR: i64 = 3_600_000;

/// A prepared `transmit` row of `receivers` boxes under a local of its own (`mark`), queued at `queued_at`.
fn queued_row(db: &kourier_db::ZoneDb, mark: u8, receivers: u8, queued_at: i64) -> i64 {
    let mut rows = send_rows(transmit(&format!("00ff1122334455{mark:02x}")), receivers);
    rows.queued_at = queued_at;
    prepare_send_zone(db, &rows, &[]).expect("a queued row")
}

/// The rows that are not seeds, by id, with their state.
fn real_rows(db: &kourier_db::ZoneDb) -> Vec<(i64, SendState)> {
    tasks(db)
        .iter()
        .filter(|row| row.kind != "seed")
        .map(|row| (row.id, row.state))
        .collect()
}

/// `UPDATE send_task SET <set> WHERE <which>`, in one zone.
fn update_tasks(db: &kourier_db::ZoneDb, set: &str, which: &str) {
    db.zone(|tx| {
        tx.execute(&format!("UPDATE send_task SET {set} WHERE {which}"), [])
            .map_err(kourier_db::Error::from)?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the update");
}

/// A full `transmit` budget of rows nobody owns and more than 24 hours old: the next PREPARE takes the OLDEST by `queued_at`
/// (not the lowest id), with its receivers, one row for one new row, one WARN with the count, until no old row is left, and
/// then it is `QueueFull` and nothing is taken.
#[test]
fn send_a_spent_transmit_budget_evicts_the_oldest_old_row_with_its_receivers() {
    use crate::support::warns::Warns;
    if crate::support::in_own_process!() {
        return;
    }
    let warns = Warns::install();
    let t = temp_store();
    let db = &t.db;
    // Ids 1, 2, 3 are 26, 30 and 25 hours old: the oldest by age is id 2. The rest of the budget is fresh.
    queued_row(db, 1, 2, SEND_QUEUED - 26 * HOUR);
    queued_row(db, 2, 3, SEND_QUEUED - 30 * HOUR);
    queued_row(db, 3, 1, SEND_QUEUED - 25 * HOUR);
    seed_open(db, "transmit", 1_997, 0, SEND_QUEUED);
    assert_eq!(open_of(db, "transmit"), 2_000, "the budget is spent");
    assert_eq!(count(db, "send_receiver"), 6);

    let new = |local: &str| send_rows(transmit(local), 1);
    let first = prepare_send_zone(db, &new("00ff1122334455a1"), &[]).expect("room by eviction");
    assert_eq!(first, 2_001, "the new row's id");
    let prepared = SendState::Prepared;
    assert_eq!(
        real_rows(db),
        [(1, prepared), (3, prepared), (2_001, prepared)],
        "the oldest went, the lowest id stayed"
    );
    assert_eq!(open_of(db, "transmit"), 2_000, "one out, one in");
    assert_eq!(
        count(db, "send_receiver"),
        4,
        "its three receivers went, the new row's one came"
    );
    assert!(
        receivers(db, 2).is_empty(),
        "no receiver of the evicted row stays"
    );

    prepare_send_zone(db, &new("00ff1122334455a2"), &[]).expect("the next oldest");
    assert_eq!(
        real_rows(db),
        [(3, prepared), (2_001, prepared), (2_002, prepared)]
    );
    prepare_send_zone(db, &new("00ff1122334455a3"), &[]).expect("the last old one");
    assert_eq!(
        real_rows(db),
        [(2_001, prepared), (2_002, prepared), (2_003, prepared)]
    );
    assert_eq!(count(db, "send_receiver"), 3);
    // Nothing old is left: the budget refuses and takes nothing.
    assert_eq!(
        prepare_send_zone(db, &new("00ff1122334455a4"), &[]),
        Err(StoreError::QueueFull)
    );
    assert_eq!(
        real_rows(db),
        [(2_001, prepared), (2_002, prepared), (2_003, prepared)]
    );
    assert_eq!(open_of(db, "transmit"), 2_000);
    assert_eq!(
        warns.at(tracing::Level::WARN),
        [
            "send rows evicted to make room: 1 by send-transmit-evicted",
            "send rows evicted to make room: 1 by send-transmit-evicted",
            "send rows evicted to make room: 1 by send-transmit-evicted",
            "a send row refused by send-queue-full: 2000 transmit rows open"
        ]
    );
    assert_eq!(RULE_TRANSMIT_EVICTED, "send-transmit-evicted");
    assert_eq!(EVICT_AFTER_MS, 24 * HOUR);
}

/// A row younger than 24 hours is never taken, however full the budget: 23 hours, and 24 hours less a millisecond, are
/// `QueueFull` and nothing goes; a row EXACTLY 24 hours old is taken (`<=`), and only it.
#[test]
fn send_a_spent_transmit_budget_takes_nothing_younger_than_24_hours() {
    let t = temp_store();
    let db = &t.db;
    seed_open(db, "transmit", 2_000, 0, SEND_QUEUED - 23 * HOUR);
    let new = send_rows(transmit(SEND_LOCAL), 1);
    assert_eq!(
        prepare_send_zone(db, &new, &[]),
        Err(StoreError::QueueFull),
        "23 hours"
    );
    assert_eq!(open_of(db, "transmit"), 2_000, "nothing was taken");
    update_tasks(
        db,
        &format!("queued_at = {}", SEND_QUEUED - 24 * HOUR + 1),
        "1",
    );
    assert_eq!(
        prepare_send_zone(db, &new, &[]),
        Err(StoreError::QueueFull),
        "24 hours less a millisecond"
    );
    assert_eq!(open_of(db, "transmit"), 2_000, "nothing was taken");
    update_tasks(
        db,
        &format!("queued_at = {}", SEND_QUEUED - 24 * HOUR),
        "id = 7",
    );
    let task = prepare_send_zone(db, &new, &[]).expect("a row exactly 24 hours old goes");
    assert_eq!(task, 2_001);
    assert_eq!(open_of(db, "transmit"), 2_000, "one out, one in");
    assert_eq!(
        count(db, "send_task WHERE id = 7"),
        0,
        "the 24 hours old row went"
    );
}

/// A row an eviction could not make room with is not taken: when even every old row would not make room, nothing goes (the
/// plan is made before the delete).
#[test]
fn send_an_eviction_that_cannot_make_room_takes_nothing() {
    let t = temp_store();
    let db = &t.db;
    let old = queued_row(db, 1, 1, SEND_QUEUED - 30 * HOUR);
    update_tasks(db, "bytes = 1048576", &format!("id = {old}"));
    // All but 1 MiB and 100 bytes of the budget is held by a fresh row: a new row of 1.7 MB cannot fit beside it.
    seed_open(
        db,
        "transmit",
        1,
        TRANSMIT_BUDGET.bytes - 1_048_576 - 100,
        SEND_QUEUED,
    );
    let mut big = send_rows(transmit(SEND_LOCAL), 0);
    big.reflect_env = Some(vec![0xE1; 65_516]);
    big.receivers = (0..200)
        .map(|n| NewReceiver {
            identity: format!("R{n:07}"),
            fate: NewFate::Send {
                payload: vec![0xB0; 8_170],
            },
        })
        .collect();
    big.csp_nonces = Vec::new();
    assert_eq!(prepare_send_zone(db, &big, &[]), Err(StoreError::QueueFull));
    assert_eq!(
        real_rows(db),
        [(old, SendState::Prepared)],
        "the old row stayed: it would not have made room"
    );
    assert_eq!(receivers(db, old).len(), 1, "with its receiver");
}

/// What an eviction never takes, however old: a row a `Transmit` is attached to, an `acked` row, a `done` row and a job; and
/// the attached row goes the moment nothing is attached to it.
#[test]
fn send_an_eviction_never_takes_an_attached_acked_done_or_job_row() {
    let t = temp_store();
    let db = &t.db;
    let old = SEND_QUEUED - 40 * HOUR;
    let update = send_update();
    let name = send_id(1);
    // Id 1: acked, the oldest of all. Id 2: attached. Id 3: done. Id 4: a job.
    let acked = queued_row(db, 1, 1, old - HOUR);
    mark_send_reflected(db, acked).expect("reflected");
    mark_send_acks(db, acked, &[&name], Some(sealed_update(&update)), SEND_T0).expect("acked");
    let attached = queued_row(db, 2, 2, old);
    let done = queued_row(db, 3, 1, old);
    mark_send_reflected(db, done).expect("reflected");
    mark_send_acks(db, done, &[&name], Some(sealed_update(&update)), SEND_T0).expect("acked");
    mark_send_done(db, done, SEND_T0, SEND_T0).expect("done");
    let mut job = send_rows(NewOrigin::Job, 1);
    job.queued_at = old;
    let job = prepare_send_zone(db, &job, &[]).expect("a job");
    assert_eq!(
        [acked, attached, done, job],
        [1, 2, 3, 4],
        "the rows are where the test says"
    );
    // The two that count against the budget, and fresh rows for the rest.
    seed_open(db, "transmit", 1_998, 0, SEND_QUEUED);
    assert_eq!(open_of(db, "transmit"), 2_000);
    let receivers_before = count(db, "send_receiver");

    let new = send_rows(transmit("00ff1122334455b1"), 1);
    assert_eq!(
        prepare_send_zone(db, &new, &[attached]),
        Err(StoreError::QueueFull),
        "only an attached, an acked, a done and a job row are old"
    );
    assert_eq!(
        real_rows(db),
        [
            (1, SendState::Acked),
            (2, SendState::Prepared),
            (3, SendState::Done),
            (4, SendState::Prepared)
        ],
        "none of them was taken"
    );
    assert_eq!(count(db, "send_receiver"), receivers_before);

    // Nobody attached: the old prepared row is the one that goes, and the other three stay.
    let task = prepare_send_zone(db, &new, &[]).expect("the unattached row makes room");
    assert_eq!(
        real_rows(db),
        [
            (1, SendState::Acked),
            (3, SendState::Done),
            (4, SendState::Prepared),
            (task, SendState::Prepared)
        ]
    );
    assert!(receivers(db, attached).is_empty(), "with its receivers");
    assert_eq!(receivers(db, acked).len(), 1, "the acked row's receiver");
    assert_eq!(receivers(db, done).len(), 1, "the done row's receiver");
    assert_eq!(receivers(db, job).len(), 1, "the job's receiver");
}

/// The BYTE budget: a new row of 1,699,516 bytes finds the budget short by a few hundred kilobytes and takes the two oldest
/// of the three old rows of 1 MiB (one is not enough), in order, and leaves the third.
#[test]
fn send_a_spent_byte_budget_evicts_as_many_old_rows_as_the_new_one_needs() {
    use crate::support::warns::Warns;
    if crate::support::in_own_process!() {
        return;
    }
    let warns = Warns::install();
    let t = temp_store();
    let db = &t.db;
    let mebibyte = 1_048_576;
    // Ids 1, 2, 3: 30, 28 and 26 hours old, holding 1 MiB each; id 4 is fresh and holds the rest but 100 bytes.
    for (mark, hours) in [(1, 30), (2, 28), (3, 26)] {
        let task = queued_row(db, mark, 1, SEND_QUEUED - hours * HOUR);
        update_tasks(db, &format!("bytes = {mebibyte}"), &format!("id = {task}"));
    }
    seed_open(
        db,
        "transmit",
        1,
        TRANSMIT_BUDGET.bytes - 3 * mebibyte - 100,
        SEND_QUEUED,
    );
    let mut big = send_rows(transmit("00ff1122334455b2"), 0);
    big.reflect_env = Some(vec![0xE1; 65_516]);
    big.receivers = (0..200)
        .map(|n| NewReceiver {
            identity: format!("R{n:07}"),
            fate: NewFate::Send {
                payload: vec![0xB0; 8_170],
            },
        })
        .collect();
    big.csp_nonces = Vec::new();
    let task = prepare_send_zone(db, &big, &[]).expect("room by two evictions");
    assert_eq!(the_task(db, task).bytes, 1_699_516, "the new row's size");
    assert_eq!(
        real_rows(db),
        [(3, SendState::Prepared), (task, SendState::Prepared)],
        "the two oldest went, the third stayed"
    );
    assert_eq!(
        warns.at(tracing::Level::WARN),
        ["send rows evicted to make room: 2 by send-transmit-evicted"]
    );
}

/// `prepare_send` takes the CALLER'S zone: a row its budget refuses rolls back what the caller wrote before it (a later
/// step's receive zone holds its ack then), and a row it takes commits with them.
#[test]
fn send_a_refused_row_rolls_the_callers_zone_back() {
    let t = temp_store();
    let db = &t.db;
    seed_open(db, "job", 2_000, 0, SEND_QUEUED);
    let result: Result<i64, StoreError> = db.zone(|tx| {
        add_nonces(tx, NonceScope::Csp, &[[0x66; 32]])?;
        prepare_send(tx, &send_rows(NewOrigin::Job, 1), &[])
    });
    assert_eq!(result, Err(StoreError::QueueFull));
    assert!(
        !db.read(|conn: &Connection| contains_nonce(conn, NonceScope::Csp, &[0x66; 32]))
            .expect("the nonce"),
        "the caller's write stayed"
    );
    let result: Result<i64, StoreError> = db.zone(|tx| {
        add_nonces(tx, NonceScope::Csp, &[[0x66; 32]])?;
        prepare_send(tx, &send_rows(transmit(SEND_LOCAL), 1), &[])
    });
    assert!(result.is_ok(), "a row of the other origin is taken");
    assert!(
        db.read(|conn: &Connection| contains_nonce(conn, NonceScope::Csp, &[0x66; 32]))
            .expect("the nonce"),
        "the caller's write went"
    );
}

/// `next_send`'s id for these attached and cooling rows, online or not.
fn next_id(
    db: &kourier_db::ZoneDb,
    attached: &[i64],
    cooling: &[i64],
    online: bool,
) -> Option<i64> {
    db.read(|conn: &Connection| {
        next_send(
            conn,
            &Runnable {
                attached,
                cooling,
                online,
            },
        )
    })
    .expect("the next row")
    .map(|task| task.id)
}

/// THE ORDER AND WHO MAY RUN: row order among the eligible; a `transmit` row only while attached; a `job` only `Online`;
/// an `acked` row by itself, attached or not, online or not, a job's too (R1); a cooling row never, attached or `acked`;
/// a `done` row never, attached or not.
#[test]
fn send_next_takes_the_first_eligible_row_in_queue_order() {
    let t = temp_store();
    let db = &t.db;
    let locals = ["00ff112233445501", "00ff112233445502", "00ff112233445503"];
    let one = send_id(1);
    let update = send_update();
    assert_eq!(next_id(db, &[], &[], true), None, "an empty queue");

    let t1 = prepare_send_zone(db, &send_rows(transmit(locals[0]), 1), &[]).expect("t1");
    let j2 = prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[]).expect("j2");
    let t3 = prepare_send_zone(db, &send_rows(transmit(locals[1]), 1), &[]).expect("t3");
    mark_send_reflected(db, t3).expect("t3 reflected");
    assert_eq!((t1, j2, t3), (1, 2, 3));

    for (case, attached, cooling, online, expected) in [
        ("nothing attached, offline", vec![], vec![], false, None),
        (
            "nothing attached, online: the job",
            vec![],
            vec![],
            true,
            Some(j2),
        ),
        ("t3 attached, offline", vec![t3], vec![], false, Some(t3)),
        (
            "t3 attached, online: the job is first",
            vec![t3],
            vec![],
            true,
            Some(j2),
        ),
        (
            "t1 attached, online: t1 is first",
            vec![t1],
            vec![],
            true,
            Some(t1),
        ),
        (
            "both attached, offline: t1 is first",
            vec![t3, t1],
            vec![],
            false,
            Some(t1),
        ),
        ("the job cooling, online", vec![], vec![j2], true, None),
        (
            "the job cooling, t3 attached",
            vec![t3],
            vec![j2],
            true,
            Some(t3),
        ),
        ("t1 attached and cooling", vec![t1], vec![t1], false, None),
        (
            "t1 attached and cooling, t3 attached",
            vec![t1, t3],
            vec![t1],
            false,
            Some(t3),
        ),
        ("an id nothing holds", vec![99], vec![98], false, None),
    ] {
        assert_eq!(next_id(db, &attached, &cooling, online), expected, "{case}");
    }

    // An `acked` row runs by itself: no attempt, offline.
    let t4 = prepare_send_zone(db, &send_rows(transmit(locals[2]), 1), &[]).expect("t4");
    mark_send_reflected(db, t4).expect("t4 reflected");
    mark_send_acks(db, t4, &[&one], Some(sealed_update(&update)), SEND_T0).expect("t4 acked");
    assert_eq!(next_id(db, &[], &[], false), Some(t4), "acked, by itself");
    assert_eq!(next_id(db, &[], &[t4], false), None, "acked and cooling");
    assert_eq!(next_id(db, &[t1], &[], false), Some(t1), "row order holds");
    assert_eq!(
        next_id(db, &[], &[], true),
        Some(j2),
        "row order holds online"
    );

    // A job's `acked` row, offline.
    let j5 = prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[]).expect("j5");
    mark_send_reflected(db, j5).expect("j5 reflected");
    mark_send_acks(db, j5, &[&one], Some(sealed_update(&update)), SEND_T0).expect("j5 acked");
    assert_eq!(
        next_id(db, &[], &[t4], false),
        Some(j5),
        "a job's acked row, offline"
    );

    // Done: never again, attached or not.
    mark_send_done(db, t4, SEND_T0, SEND_T0).expect("t4 done");
    assert_eq!(
        next_id(db, &[t4], &[j5], false),
        None,
        "a done row, attached"
    );

    // The row comes whole.
    let next = db
        .read(|conn: &Connection| {
            next_send(
                conn,
                &Runnable {
                    attached: &[t1],
                    cooling: &[],
                    online: false,
                },
            )
        })
        .expect("the next row")
        .expect("t1");
    assert!(
        next.reflect_env == Some(send_reflect()),
        "the sealed reflect"
    );
}

/// THE SWEEP, on both sides of each age, literal: `done` rows by `finished_at` past 14 days, open `transmit` rows by
/// `queued_at` past 30 days, open `job` rows by `queued_at` past 14 days; a row exactly at its age stays; the receivers
/// of a row that goes go with it. A second sweep finds nothing.
#[test]
fn send_the_sweep_goes_by_each_rows_own_age() {
    const NOW: i64 = 1_800_000_000_000;
    const DAY14: i64 = 1_209_600_000;
    const DAY30: i64 = 2_592_000_000;
    let t = temp_store();
    let db = &t.db;
    let one = send_id(1);
    let update = send_update();
    let row = |origin: NewOrigin, queued_at: i64| {
        let mut rows = send_rows(origin, 1);
        rows.queued_at = queued_at;
        prepare_send_zone(db, &rows, &[]).expect("a row")
    };
    let done_at = |local: &str, finished_at: i64| {
        // Queued long ago: only `finished_at` may count for a `done` row.
        let task = row(transmit(local), NOW - DAY30 - 5);
        mark_send_reflected(db, task).expect("reflected");
        mark_send_acks(db, task, &[&one], Some(sealed_update(&update)), NOW).expect("acked");
        mark_send_done(db, task, NOW, finished_at).expect("done");
        task
    };
    let done_goes = done_at("00ff112233445501", NOW - DAY14 - 1);
    let done_stays = done_at("00ff112233445502", NOW - DAY14);
    let transmit_goes = row(transmit("00ff112233445503"), NOW - DAY30 - 1);
    let transmit_stays = row(transmit("00ff112233445504"), NOW - DAY30);
    let job_goes = row(NewOrigin::Job, NOW - DAY14 - 1);
    let job_stays = row(NewOrigin::Job, NOW - DAY14);
    // An old `acked` row is open too.
    let acked_goes = row(transmit("00ff112233445505"), NOW - DAY30 - 1);
    mark_send_reflected(db, acked_goes).expect("reflected");
    mark_send_acks(db, acked_goes, &[&one], Some(sealed_update(&update)), NOW).expect("acked");

    let before_sweep = count(db, "send_receiver");
    assert_eq!(before_sweep, 7);
    assert_eq!(
        sweep_sends(db, NOW).expect("the sweep"),
        Swept {
            done: 1,
            transmit: 2,
            jobs: 1
        }
    );
    let left: Vec<i64> = tasks(db).iter().map(|task| task.id).collect();
    assert_eq!(left, [done_stays, transmit_stays, job_stays]);
    for gone in [done_goes, transmit_goes, job_goes, acked_goes] {
        assert!(
            receivers(db, gone).is_empty(),
            "row {gone}'s receivers stayed"
        );
    }
    assert_eq!(count(db, "send_receiver"), 3);
    assert_eq!(
        sweep_sends(db, NOW).expect("a second sweep"),
        Swept::default()
    );
}

/// THE FAILED RUNS (R2): one statement counts them, the count is returned and survives a reopen (a restart loses a row's
/// cooling, not this), nothing else of the row moves, and a row that is not there fails by name.
#[test]
fn send_the_failed_runs_are_durable_and_counted_one_by_one() {
    let t = temp_store();
    let task = prepare_send_zone(&t.db, &send_rows(transmit(SEND_LOCAL), 1), &[]).expect("prepare");
    let row = the_task(&t.db, task);
    assert_eq!(count_failed_run(&t.db, task), Ok(1));
    assert_eq!(count_failed_run(&t.db, task), Ok(2));
    assert!(matches!(
        count_failed_run(&t.db, 99),
        Err(StoreError::Failed(_))
    ));
    let dir = t.close();
    let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("reopen");
    let after = the_task(&db, task);
    assert_eq!(after.attempts, 2);
    assert!(
        after == SendTask { attempts: 2, ..row },
        "a failed run moved another column"
    );
}

/// A JOB'S GIVE-UP (R2): the row goes with its receivers, whatever it holds, and the nonces stay; a `transmit` row is the
/// core's and is refused by name, as a row that is not there.
#[test]
fn send_a_job_is_given_up_with_its_receivers_and_a_transmit_row_is_refused() {
    let t = temp_store();
    let job = prepare_send_zone(&t.db, &send_rows(NewOrigin::Job, 2), &[]).expect("the job");
    let local =
        prepare_send_zone(&t.db, &send_rows(transmit(SEND_LOCAL), 1), &[]).expect("the row");
    let nonces = (count(&t.db, "csp_nonce"), count(&t.db, "d2d_nonce"));
    assert_eq!(give_up_send_job(&t.db, job), Ok(()));
    assert!(
        receivers(&t.db, job).is_empty(),
        "the job's receivers stayed"
    );
    assert_eq!(count(&t.db, "send_task"), 1);
    assert_eq!(
        (count(&t.db, "csp_nonce"), count(&t.db, "d2d_nonce")),
        nonces,
        "the nonces are permanent"
    );
    assert!(matches!(
        give_up_send_job(&t.db, local),
        Err(StoreError::Failed(_))
    ));
    assert!(matches!(
        give_up_send_job(&t.db, job),
        Err(StoreError::Failed(_))
    ));
    assert_eq!(count(&t.db, "send_task"), 1, "the transmit row stands");
}

/// THE DOOR (R6) fails a zone behind its writes, with the code it was given (both of `is_content`'s branches: their
/// reading is `send::queue`'s unit test); nothing of the zone stays, only that zone fails, and off means off.
#[test]
fn send_the_door_fails_the_zone_it_names_and_nothing_of_it_stays() {
    use kourier_threema::failpoints as fp;
    if crate::support::in_own_process!() {
        return;
    }
    let t = temp_store();
    let db = &t.db;
    let one = send_id(1);
    let update = send_update();
    let failed = |code: i32| StoreError::Sql {
        op: "a send zone failed",
        code: Some(code),
    };
    let rows = send_rows(transmit(SEND_LOCAL), 1);

    fail_send_zone(fp::INSIDE_SEND_PREPARE, 13);
    assert_eq!(prepare_send_zone(db, &rows, &[]), Err(failed(13)));
    assert_eq!(count(db, "send_task"), 0, "PREPARE's row stayed");
    assert_eq!(count(db, "send_receiver"), 0, "PREPARE's receiver stayed");
    assert_eq!(count(db, "csp_nonce"), 0, "PREPARE's nonce stayed");
    fail_send_zone("", 0);
    let task = prepare_send_zone(db, &rows, &[]).expect("the door is off");

    fail_send_zone(fp::INSIDE_SEND_REFLECTED, 275);
    assert_eq!(mark_send_reflected(db, task), Err(failed(275)));
    assert_eq!(the_task(db, task).state, SendState::Prepared);
    assert!(the_task(db, task).reflect_env.is_some(), "the reflect went");
    fail_send_zone("", 0);
    mark_send_reflected(db, task).expect("the door is off");

    fail_send_zone(fp::INSIDE_SEND_ACKS, 13);
    assert_eq!(
        mark_send_acks(db, task, &[&one], Some(sealed_update(&update)), SEND_T0),
        Err(failed(13))
    );
    assert_eq!(the_task(db, task).state, SendState::Reflected);
    assert_eq!(receiver_states(db, task), [ReceiverState::Pending]);
    // Another zone is not the door's.
    assert_eq!(sweep_sends(db, SEND_T0), Ok(Swept::default()));
    fail_send_zone("", 0);
    mark_send_acks(db, task, &[&one], Some(sealed_update(&update)), SEND_T0)
        .expect("the door is off");

    fail_send_zone(fp::INSIDE_SEND_DONE, 13);
    assert_eq!(mark_send_done(db, task, SEND_T0, SEND_T0), Err(failed(13)));
    assert_eq!(the_task(db, task).state, SendState::Acked);
    fail_send_zone("", 0);
    assert_eq!(
        mark_send_done(db, task, SEND_T0, SEND_T0),
        Ok(SendEnd::Done)
    );

    fail_send_zone(fp::INSIDE_SEND_SWEEP, 13);
    assert_eq!(sweep_sends(db, SEND_T0 + 1_209_600_001), Err(failed(13)));
    assert_eq!(count(db, "send_task"), 1, "the sweep's delete stayed");
    fail_send_zone("", 0);
}

/// The WARN lines carry a rule name and a count, once per zone, written out; a refused row's line says so with no ID.
#[test]
fn send_the_warn_lines_carry_a_rule_and_a_count() {
    use crate::support::warns::Warns;
    if crate::support::in_own_process!() {
        return;
    }
    let warns = Warns::install();
    let t = temp_store();
    let db = &t.db;
    seed_open(db, "job", 2_000, 0, SEND_QUEUED);
    assert_eq!(
        prepare_send_zone(db, &send_rows(NewOrigin::Job, 1), &[]),
        Err(StoreError::QueueFull)
    );
    assert_eq!(
        warns.at(tracing::Level::WARN),
        ["a send row refused by send-queue-full: 2000 job rows open"]
    );
    // 2,000 old jobs and one old `transmit` row go in one zone: one line per rule.
    let mut old = send_rows(transmit(SEND_LOCAL), 1);
    old.queued_at = 1;
    prepare_send_zone(db, &old, &[]).expect("a transmit row");
    assert_eq!(
        sweep_sends(db, 1_800_000_000_000),
        Ok(Swept {
            done: 0,
            transmit: 1,
            jobs: 2_000
        })
    );
    assert_eq!(
        warns.at(tracing::Level::WARN),
        [
            "a send row refused by send-queue-full: 2000 job rows open",
            "send rows dropped by the sweep: 1 by send-transmit-expired",
            "send rows dropped by the sweep: 2000 by send-job-expired"
        ]
    );
    // The rule names, literal, as a later step names them.
    assert_eq!(RULE_QUEUE_FULL, "send-queue-full");
    assert_eq!(RULE_JOB_EXPIRED, "send-job-expired");
    assert_eq!(RULE_TRANSMIT_EXPIRED, "send-transmit-expired");
}

/// `Debug` of a row prints counts and names of the code: no ID, no local, no message id, no conversation, no sealed byte.
#[test]
fn send_debug_prints_counts_and_nothing_of_a_row() {
    let t = temp_store();
    let rows = send_rows(transmit(SEND_LOCAL), 2);
    let task = prepare_send_zone(&t.db, &rows, &[]).expect("prepare");
    let row = the_task(&t.db, task);
    let stored = receivers(&t.db, task);
    let texts = [
        format!("{rows:?}"),
        format!("{row:?}"),
        format!("{stored:?}"),
    ];
    for text in texts {
        for needle in [
            "S1RCVR",
            SEND_LOCAL,
            &u64::MAX.to_string(),
            "1761500000000",
            // The bytes as Rust prints them (the sealed ones and the conversation).
            "225",
            "177",
            "83, 49",
        ] {
            assert!(!text.contains(needle), "a Debug prints a row's content");
        }
    }
}

// THE LEADER'S ROWS (TH6 L1, schema 5, `migrations/0005_leader.sql`) -----------------------------------------------------

/// The leader's synthetic IDs (8 characters, each a substring of no other test value).
pub(crate) const IN_OWN: &str = "L1OWNID1";
pub(crate) const IN_PEER: &str = "L1PEERA2";
pub(crate) const IN_OTHER: &str = "L1PEERB3";
/// The local time of the zones, and the messages' own time.
pub(crate) const IN_T0: i64 = 1_761_600_000_000;
pub(crate) const IN_AT: u64 = 1_761_590_000_000;

/// A linked file: the identity, the marker and the two peers as direct contacts with real public keys (a job seals).
pub(crate) fn inbound_linked(db: &kourier_db::ZoneDb) {
    use crate::support::mock::crypto::x25519_public;
    use crate::support::mock::envelope::ContactDelta;
    use kourier_db::rusqlite::params;
    use prost::Message as _;
    let identity = Identity {
        threema_id: IN_OWN.to_owned(),
        client_key: Zeroizing::new([0x71; 32]),
        server_group: "c".to_owned(),
        device_group_key: Zeroizing::new([0x72; 32]),
        device_cookie: Zeroizing::new([0x73; 16]),
        d2m_device_id: 11,
        csp_device_id: 12,
        slot_state: SlotState::Existing,
        device_label: "Kourier (l1-leader)".to_owned(),
        linked_at_ms: IN_T0,
    };
    db.zone(|tx| {
        save_identity(tx, &identity)?;
        mark_join_ready(tx, IN_T0)
    })
    .expect("the identity");
    db.zone(|tx| {
        for (id, secret) in [(IN_PEER, [0x11; 32]), (IN_OTHER, [0x12; 32])] {
            let sync = ContactDelta::create(id, x25519_public(&secret), IN_AT).build();
            tx.execute(
                "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                params![sync.identity, sync.encode_to_vec()],
            )?;
        }
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the contacts");
}

/// The id of message `n` (`[n; 8]`, as the fake writes it).
pub(crate) fn inbound_id(n: u8) -> u64 {
    u64::from_le_bytes([n; 8])
}

/// The plaintext `d2d.Envelope { incoming_message }` the leader reflects for message `n` of `sender`: type `ty`, `body`.
pub(crate) fn inbound_plaintext(sender: &str, n: u8, ty: u8, body: Vec<u8>) -> Vec<u8> {
    use crate::support::mock::envelope::{Author, Msg};
    use prost::Message as _;
    Author::new(0x0A11)
        .incoming(sender, &Msg::raw([n; 8], IN_AT, ty, body).nonce([n; 24]))
        .encode_to_vec()
}

/// A text of `sender`, message `n`.
pub(crate) fn inbound_text(sender: &str, n: u8) -> Vec<u8> {
    use crate::support::mock::envelope::{Author, Msg};
    use prost::Message as _;
    Author::new(0x0A11)
        .incoming(
            sender,
            &Msg::text([n; 8], IN_AT, "leader text").nonce([n; 24]),
        )
        .encode_to_vec()
}

/// The synthetic sealed reflect of message `n`: `40 + n` bytes of `0xE0 + n`.
pub(crate) fn inbound_reflect(n: u8) -> Vec<u8> {
    vec![0xE0 + n; 40 + usize::from(n)]
}

/// Message `n` of `sender` kept: its CSP nonce hash `[0xC0 + n; 32]`, its reflect's D2D hash `[0xD0 + n; 32]`, flags 0
/// (an ack owed), no contact, no job.
pub(crate) fn inbound_kept<'a>(
    sender: &'a str,
    n: u8,
    message: &'a [u8],
    reflect: &'a [u8],
    receipt: bool,
) -> Kept<'a> {
    Kept {
        sender,
        message_id: inbound_id(n),
        message,
        contact: None,
        server_at: IN_AT,
        flags: 0,
        nonce_hash: [0xC0 + n; 32],
        d2d_hash: [0xD0 + n; 32],
        reflect_env: reflect,
        ack: true,
        receipt,
        jobs: &[],
    }
}

/// The context of the zones here, at `now_ms`.
pub(crate) fn receiving(wake: &Rc<Notify>, now_ms: i64) -> Receiving<'_> {
    Receiving {
        own: IN_OWN,
        now_ms,
        catch_up: false,
        bounds: kourier_threema::store::DeferredBounds::REAL,
        wake,
    }
}

/// A 1:1 text job to `to` (what a discard's steps send stands in for).
pub(crate) fn inbound_job(to: &str) -> NewJob {
    NewJob {
        kind: "l1-job",
        to: To::Contact(to.to_owned()),
        body: NewBody::Text("a job".to_owned()),
        only: None,
    }
}

/// The delivery-receipt job ZONE REFLECTED makes for `sender` and `ids` (status 1, the ids little-endian).
pub(crate) fn inbound_receipt(sender: &str, ids: &[u64]) -> NewJob {
    let mut body = vec![1u8];
    for id in ids {
        body.extend(id.to_le_bytes());
    }
    NewJob {
        kind: "receipt",
        to: To::Contact(sender.to_owned()),
        body: NewBody::Raw {
            ty: 0x80,
            body,
            distributes: false,
        },
        only: None,
    }
}

fn inbound_all(db: &kourier_db::ZoneDb) -> Vec<InboundRow> {
    db.read(|conn: &Connection| inbound_rows(conn))
        .expect("the rows")
}

fn has_nonce(db: &kourier_db::ZoneDb, scope: NonceScope, hash: [u8; 32]) -> bool {
    db.read(|conn: &Connection| contains_nonce(conn, scope, &hash))
        .expect("a nonce")
}

/// A literal `pending` row as RECEIVE writes it.
fn pending_row(id: i64, sender: &str, n: u8, ack: bool, receipt: bool) -> InboundRow {
    InboundRow {
        id,
        sender: sender.to_owned(),
        message_id: inbound_id(n),
        state: InboundState::Pending,
        ack,
        receipt,
        received_at: IN_T0,
        reflected_at: None,
        nonce_hash: Some([0xC0 + n; 32]),
        reflect_env: Some(Zeroizing::new(inbound_reflect(n))),
    }
}

/// `INSERT INTO csp_inbound` with the good `pending` row (`reflected`: the good `reflected` row), `over` its columns.
fn inbound_insert(reflected: bool, over: &[(&str, String)]) -> String {
    let columns: [(&str, &str); 10] = [
        ("sender", "'L1CHKID8'"),
        ("message_id", "1"),
        (
            "state",
            if reflected {
                "'reflected'"
            } else {
                "'pending'"
            },
        ),
        ("ack", "1"),
        ("receipt", "1"),
        ("received_at", "5"),
        ("reflected_at", if reflected { "6" } else { "NULL" }),
        (
            "nonce_hash",
            if reflected { "NULL" } else { "zeroblob(32)" },
        ),
        (
            "reflect_env",
            if reflected { "NULL" } else { "zeroblob(40)" },
        ),
        ("id", "NULL"),
    ];
    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let values: Vec<String> = columns
        .iter()
        .map(|(name, value)| {
            over.iter()
                .find(|(other, _)| other == name)
                .map_or_else(|| (*value).to_owned(), |(_, value)| value.clone())
        })
        .collect();
    format!(
        "INSERT INTO csp_inbound ({}) VALUES ({})",
        names.join(", "),
        values.join(", ")
    )
}

/// Each CHECK of schema 5 takes its good row and refuses the bad one AT EVERY STATE (`pending` and `reflected`), on both
/// sides of each size (N and N+1), with the NULLs a CHECK alone would pass, the types STRICT refuses and the `UNIQUE` of
/// step 0. Literal SQL on the real migration.
#[test]
fn inbound_each_check_of_schema_5_refuses_its_bad_row_and_takes_the_good_one() {
    let s = |v: &str| v.to_owned();
    let blob = |n: usize| format!("zeroblob({n})");
    let text = |n: usize| format!("'{}'", "a".repeat(n));
    // (name, the state it starts from: reflected?, the columns it changes, taken?)
    type Case<'a> = (&'a str, bool, Vec<(&'a str, String)>, bool);
    let mut cases: Vec<Case<'_>> = Vec::new();
    for reflected in [false, true] {
        cases.extend([
            ("the good row", reflected, vec![], true),
            (
                "a sender of 7",
                reflected,
                vec![("sender", s("'L1CHKID'"))],
                false,
            ),
            (
                "a sender of 9",
                reflected,
                vec![("sender", s("'L1CHKID89'"))],
                false,
            ),
            ("no sender", reflected, vec![("sender", s("NULL"))], false),
            (
                "a sender as a blob",
                reflected,
                vec![("sender", s("X'4C31434B49443838'"))],
                false,
            ),
            (
                "no message id",
                reflected,
                vec![("message_id", s("NULL"))],
                false,
            ),
            (
                "a message id of the high bit",
                reflected,
                vec![("message_id", s("-9223372036854775808"))],
                true,
            ),
            (
                "a message id as text",
                reflected,
                vec![("message_id", s("'x'"))],
                false,
            ),
            (
                "another state",
                reflected,
                vec![("state", s("'acked'"))],
                false,
            ),
            (
                "a state in capitals",
                reflected,
                vec![("state", s("'PENDING'"))],
                false,
            ),
            ("no state", reflected, vec![("state", s("NULL"))], false),
            ("no ack owed", reflected, vec![("ack", s("0"))], true),
            ("an ack of 2", reflected, vec![("ack", s("2"))], false),
            ("an ack of -1", reflected, vec![("ack", s("-1"))], false),
            ("no ack", reflected, vec![("ack", s("NULL"))], false),
            (
                "no receipt owed",
                reflected,
                vec![("receipt", s("0"))],
                true,
            ),
            (
                "a receipt of 2",
                reflected,
                vec![("receipt", s("2"))],
                false,
            ),
            ("no receipt", reflected, vec![("receipt", s("NULL"))], false),
            (
                "no received_at",
                reflected,
                vec![("received_at", s("NULL"))],
                false,
            ),
            (
                "a received_at as text",
                reflected,
                vec![("received_at", s("'x'"))],
                false,
            ),
            ("an id as text", reflected, vec![("id", s("'x'"))], false),
        ]);
    }
    cases.extend([
        // pending: its reflect and its nonce's hash are there, no reflected time.
        (
            "pending with a reflected time",
            false,
            vec![("reflected_at", s("6"))],
            false,
        ),
        (
            "pending without its nonce",
            false,
            vec![("nonce_hash", s("NULL"))],
            false,
        ),
        (
            "pending with a nonce of 31",
            false,
            vec![("nonce_hash", blob(31))],
            false,
        ),
        (
            "pending with a nonce of 33",
            false,
            vec![("nonce_hash", blob(33))],
            false,
        ),
        (
            "pending with a nonce as text",
            false,
            vec![("nonce_hash", text(32))],
            false,
        ),
        (
            "pending without its reflect",
            false,
            vec![("reflect_env", s("NULL"))],
            false,
        ),
        (
            "pending with a reflect of 39",
            false,
            vec![("reflect_env", blob(39))],
            false,
        ),
        (
            "pending with a reflect of 40",
            false,
            vec![("reflect_env", blob(40))],
            true,
        ),
        (
            "pending with a reflect of 65516",
            false,
            vec![("reflect_env", blob(65_516))],
            true,
        ),
        (
            "pending with a reflect of 65517",
            false,
            vec![("reflect_env", blob(65_517))],
            false,
        ),
        (
            "pending with a reflect as text",
            false,
            vec![("reflect_env", text(40))],
            false,
        ),
        // reflected: its time is there, its reflect and its nonce's hash are gone.
        (
            "reflected without its time",
            true,
            vec![("reflected_at", s("NULL"))],
            false,
        ),
        (
            "reflected with a nonce",
            true,
            vec![("nonce_hash", blob(32))],
            false,
        ),
        (
            "reflected with a reflect",
            true,
            vec![("reflect_env", blob(40))],
            false,
        ),
        (
            "reflected with a time as text",
            true,
            vec![("reflected_at", s("'x'"))],
            false,
        ),
    ]);
    for (name, reflected, over, taken) in &cases {
        let state = if *reflected { "reflected" } else { "pending" };
        assert_eq!(
            takes(&[inbound_insert(*reflected, over).as_str()]),
            *taken,
            "{name} ({state})"
        );
    }
    // UNIQUE (sender, message_id), at every state: a second row of the same message is refused, whatever its state; the
    // same id of another sender, and another id of the same sender, are taken.
    let good = |reflected: bool| inbound_insert(reflected, &[]);
    for (first, second) in [(false, false), (false, true), (true, false), (true, true)] {
        assert!(
            !takes(&[good(first).as_str(), good(second).as_str()]),
            "a message twice ({first}, {second})"
        );
    }
    assert!(takes(&[
        good(false).as_str(),
        inbound_insert(false, &[("sender", s("'L1CHKID9'"))]).as_str(),
        inbound_insert(true, &[("message_id", s("2"))]).as_str(),
    ]));
    // `state` moves only with its columns: REFLECTED's update is taken, a half move is refused.
    let moved = |set: &str| {
        takes(&[
            good(false).as_str(),
            format!("UPDATE csp_inbound SET {set} WHERE id = 1").as_str(),
        ])
    };
    assert!(moved(
        "state = 'reflected', reflected_at = 9, nonce_hash = NULL, reflect_env = NULL"
    ));
    assert!(!moved(
        "state = 'reflected', reflected_at = 9, reflect_env = NULL"
    ));
    assert!(!moved(
        "state = 'reflected', reflected_at = 9, nonce_hash = NULL"
    ));
    assert!(!moved(
        "state = 'reflected', nonce_hash = NULL, reflect_env = NULL"
    ));
}

/// ZONE RECEIVE with literal rows: a kept text (a receipt owed), a discard with its nonce and a job, a kept poll vote of
/// the other peer (flag 0x04: no ack; deferred as `csp`), a discard before decryption (nothing). ONE zone: the two
/// `pending` rows, the text's events in ONE `core_outbox` row, the vote's `deferred` row of source `csp`, the three CSP
/// nonces and the two reflects' D2D nonces, the job's row; step 0 then finds each kept message by (sender, id) and no
/// other.
#[test]
fn inbound_receive_writes_a_batch_in_one_zone_with_literal_rows() {
    use kourier_threema::store::{DeferredSource, deferred_page};
    let t = temp_store();
    let db = &t.db;
    inbound_linked(db);
    let wake = Rc::new(Notify::new());
    let text = inbound_text(IN_PEER, 1);
    let vote = inbound_plaintext(IN_OTHER, 3, 0x16, vec![7; 20]);
    let (r1, r3) = (inbound_reflect(1), inbound_reflect(3));
    let jobs = [inbound_job(IN_OTHER)];
    let vote_kept = Kept {
        flags: 0x04,
        ack: false,
        ..inbound_kept(IN_OTHER, 3, &vote, &r3, false)
    };
    let received = receive_inbound(
        db,
        &receiving(&wake, IN_T0),
        &[
            Inbound::Keep(inbound_kept(IN_PEER, 1, &text, &r1, true)),
            Inbound::Discard(Discarded {
                nonce_hash: Some([0xC2; 32]),
                jobs: &jobs,
                contact: None,
            }),
            Inbound::Keep(vote_kept),
            Inbound::Discard(Discarded {
                nonce_hash: None,
                jobs: &[],
                contact: None,
            }),
        ],
    )
    .expect("the zone");
    assert_eq!(
        received,
        [
            Received::Kept(1),
            Received::Discarded,
            Received::Kept(2),
            Received::Discarded
        ]
    );
    assert_eq!(
        inbound_all(db),
        [
            pending_row(1, IN_PEER, 1, true, true),
            pending_row(2, IN_OTHER, 3, false, false)
        ]
    );
    let events = outbox_events(db);
    let peer = kourier_model::ConvKey::Direct(UserId::from_parts("threema", IN_PEER).expect("id"));
    assert!(
        matches!(events.as_slice(), [row] if matches!(row.as_slice(), [BackendEvent::Message(m)] if m.conv == peer)),
        "the text's events are not ONE row with its message"
    );
    let deferred = db
        .read(|conn: &Connection| deferred_page(conn, u32::MAX, 0))
        .expect("the deferred rows");
    assert!(
        deferred.len() == 1
            && deferred[0].source == DeferredSource::Csp
            && deferred[0].kind == "poll-vote"
            && deferred[0].server_at == IN_AT
            && deferred[0].flags == 0x04
            && deferred[0].received_at == IN_T0
            && deferred[0].envelope.as_slice() == vote.as_slice(),
        "the vote is not ONE csp row with its plaintext"
    );
    for hash in [[0xC1; 32], [0xC2; 32], [0xC3; 32]] {
        assert!(
            has_nonce(db, NonceScope::Csp, hash),
            "a CSP nonce is missing"
        );
    }
    for hash in [[0xD1; 32], [0xD3; 32]] {
        assert!(
            has_nonce(db, NonceScope::D2d, hash),
            "a reflect's nonce is missing"
        );
    }
    let jobs = tasks(db);
    assert!(
        jobs.len() == 1 && jobs[0].kind == "l1-job" && jobs[0].origin == SendOrigin::Job,
        "the discard's job is not one row"
    );
    let step_0 = |sender: &str, n: u8| {
        db.read(|conn: &Connection| inbound_row(conn, sender, inbound_id(n)))
            .expect("step 0")
    };
    assert_eq!(
        step_0(IN_PEER, 1),
        Some(pending_row(1, IN_PEER, 1, true, true))
    );
    assert_eq!(step_0(IN_OTHER, 1), None, "another sender's id");
    assert_eq!(step_0(IN_PEER, 3), None, "another id of the sender");
    assert_eq!(step_0(IN_PEER, 2), None, "a discard has no row");
}

/// What a decision before the zone could not see: a second copy of a kept message in the same batch (another nonce)
/// writes nothing (`Known`: held, step 0 resumes it), a kept message whose CSP nonce the first one used writes nothing
/// (`NonceUsed`: step 2's discard); a reflect whose D2D nonce is in the store fails the whole zone, and nothing of it
/// stays.
#[test]
fn inbound_a_copy_or_a_used_nonce_writes_nothing_of_it_and_a_used_reflect_nonce_fails_the_zone() {
    let t = temp_store();
    let db = &t.db;
    inbound_linked(db);
    let wake = Rc::new(Notify::new());
    let (text, again, other) = (
        inbound_text(IN_PEER, 1),
        inbound_text(IN_PEER, 1),
        inbound_text(IN_OTHER, 2),
    );
    let (r1, r9, r2) = (inbound_reflect(1), inbound_reflect(9), inbound_reflect(2));
    let copy = Kept {
        nonce_hash: [0xC9; 32],
        d2d_hash: [0xD9; 32],
        ..inbound_kept(IN_PEER, 1, &again, &r9, true)
    };
    let reused = Kept {
        nonce_hash: [0xC1; 32],
        ..inbound_kept(IN_OTHER, 2, &other, &r2, true)
    };
    let received = receive_inbound(
        db,
        &receiving(&wake, IN_T0),
        &[
            Inbound::Keep(inbound_kept(IN_PEER, 1, &text, &r1, true)),
            Inbound::Keep(copy),
            Inbound::Keep(reused),
        ],
    )
    .expect("the zone");
    assert_eq!(
        received,
        [Received::Kept(1), Received::Known, Received::NonceUsed]
    );
    assert_eq!(inbound_all(db), [pending_row(1, IN_PEER, 1, true, true)]);
    assert_eq!(outbox_events(db).len(), 1, "only the first copy's events");
    for (scope, hash) in [
        (NonceScope::Csp, [0xC9; 32]),
        (NonceScope::D2d, [0xD9; 32]),
        (NonceScope::D2d, [0xD2; 32]),
    ] {
        assert!(
            !has_nonce(db, scope, hash),
            "a message written not at all left a nonce"
        );
    }
    // A reflect nonce in the store: the zone fails by its content, nothing of it stays (the other message neither).
    let fresh = inbound_text(IN_OTHER, 4);
    let r4 = inbound_reflect(4);
    let used = Kept {
        d2d_hash: [0xD1; 32],
        ..inbound_kept(IN_OTHER, 4, &fresh, &r4, true)
    };
    let error = receive_inbound(
        db,
        &receiving(&wake, IN_T0 + 1),
        &[
            Inbound::Keep(inbound_kept(IN_OTHER, 2, &other, &r2, true)),
            Inbound::Keep(used),
        ],
    )
    .expect_err("a used reflect nonce");
    assert!(
        matches!(error, StoreError::Failed(_)),
        "not the content's failure: {error:?}"
    );
    assert_eq!(inbound_all(db), [pending_row(1, IN_PEER, 1, true, true)]);
    assert_eq!(outbox_events(db).len(), 1);
    assert!(
        !has_nonce(db, NonceScope::Csp, [0xC2; 32]),
        "a rolled-back zone left a nonce"
    );
}

/// ZONE REFLECTED with literal rows: four `pending` rows of two senders (three owe a receipt, one no ack), the reflect-
/// acks of all four and of a row that is not there: each row `reflected` at the zone's time with its reflect and its
/// nonce's hash dropped, ONE receipt job per sender (the ids in arrival order), the acks returned in the order given
/// (the missing row not among them); a second ack of a moved row moves nothing and enqueues nothing.
#[test]
fn inbound_reflected_moves_the_rows_and_coalesces_one_receipt_per_sender() {
    use std::cell::RefCell;
    let t = temp_store();
    let db = &t.db;
    inbound_linked(db);
    let wake = Rc::new(Notify::new());
    let texts: Vec<(Vec<u8>, Vec<u8>)> = (1..=4u8)
        .map(|n| {
            let sender = if n % 2 == 1 { IN_PEER } else { IN_OTHER };
            (inbound_text(sender, n), inbound_reflect(n))
        })
        .collect();
    let kept: Vec<Inbound<'_>> = texts
        .iter()
        .zip(1u8..)
        .map(|((text, reflect), n)| {
            let sender = if n % 2 == 1 { IN_PEER } else { IN_OTHER };
            Inbound::Keep(Kept {
                ack: n != 4,
                ..inbound_kept(sender, n, text, reflect, n != 4)
            })
        })
        .collect();
    receive_inbound(db, &receiving(&wake, IN_T0), &kept).expect("the rows before");
    let calls: RefCell<Vec<(String, Vec<u64>)>> = RefCell::new(Vec::new());
    let receipt = |sender: &str, ids: &[u64]| {
        calls.borrow_mut().push((sender.to_owned(), ids.to_vec()));
        inbound_receipt(sender, ids)
    };
    let acks =
        mark_inbound_reflected(db, &[1, 2, 3, 4, 99], IN_T0 + 7, &receipt).expect("the zone");
    let ack = |id: i64, sender: &str, n: u8, ack: bool| InboundAck {
        id,
        sender: sender.to_owned(),
        message_id: inbound_id(n),
        ack,
    };
    assert_eq!(
        acks,
        [
            ack(1, IN_PEER, 1, true),
            ack(2, IN_OTHER, 2, true),
            ack(3, IN_PEER, 3, true),
            ack(4, IN_OTHER, 4, false)
        ]
    );
    assert_eq!(
        *calls.borrow(),
        [
            (IN_PEER.to_owned(), vec![inbound_id(1), inbound_id(3)]),
            (IN_OTHER.to_owned(), vec![inbound_id(2)])
        ],
        "not one receipt per sender, its ids in arrival order"
    );
    let rows = inbound_all(db);
    for (row, n) in rows.iter().zip(1u8..) {
        let sender = if n % 2 == 1 { IN_PEER } else { IN_OTHER };
        assert_eq!(
            *row,
            InboundRow {
                id: i64::from(n),
                sender: sender.to_owned(),
                message_id: inbound_id(n),
                state: InboundState::Reflected,
                ack: n != 4,
                receipt: n != 4,
                received_at: IN_T0,
                reflected_at: Some(IN_T0 + 7),
                nonce_hash: None,
                reflect_env: None,
            }
        );
    }
    let jobs = tasks(db);
    assert!(
        jobs.len() == 2
            && jobs
                .iter()
                .all(|j| j.kind == "receipt" && j.origin == SendOrigin::Job),
        "not two receipt jobs"
    );
    // The nonces of a reflected row stay for good.
    assert!(has_nonce(db, NonceScope::Csp, [0xC1; 32]));
    let again = mark_inbound_reflected(db, &[1], IN_T0 + 8, &receipt).expect("again");
    assert!(again.is_empty(), "a moved row moved again");
    assert_eq!(tasks(db).len(), 2, "a second ack enqueued a receipt");
    assert_eq!(calls.borrow().len(), 2);
}

/// THE SWEEP goes by each row's own age and state: a `pending` row past 30 days goes WITH its CSP nonce (T-L5 (b)), one
/// at exactly 30 days stays with its nonce; a `reflected` row past 14 days goes, one at exactly 14 days stays, and the
/// nonce of a reflected message stays for good; a young `pending` row stays. A second sweep finds nothing.
#[test]
fn inbound_the_sweep_goes_by_each_rows_own_age_and_takes_a_pending_rows_nonce() {
    const NOW: i64 = 1_800_000_000_000;
    const DAY14: i64 = 1_209_600_000;
    const DAY30: i64 = 2_592_000_000;
    let t = temp_store();
    let db = &t.db;
    let pending = |sender: &str, received_at: i64, nonce: u8| {
        inbound_insert(
            false,
            &[
                ("sender", format!("'{sender}'")),
                ("received_at", received_at.to_string()),
                (
                    "nonce_hash",
                    format!("X'{}'", format!("{nonce:02X}").repeat(32)),
                ),
            ],
        )
    };
    let reflected = |sender: &str, reflected_at: i64| {
        inbound_insert(
            true,
            &[
                ("sender", format!("'{sender}'")),
                ("received_at", "1".to_owned()),
                ("reflected_at", reflected_at.to_string()),
            ],
        )
    };
    let nonce = |byte: u8| {
        format!(
            "INSERT INTO csp_nonce (hash) VALUES (X'{}')",
            format!("{byte:02X}").repeat(32)
        )
    };
    db.zone(|tx| {
        for statement in [
            pending("L1SWEEP1", NOW - DAY30 - 1, 0xA1),
            pending("L1SWEEP2", NOW - DAY30, 0xA2),
            reflected("L1SWEEP3", NOW - DAY14 - 1),
            reflected("L1SWEEP4", NOW - DAY14),
            pending("L1SWEEP5", NOW, 0xA5),
            nonce(0xA1),
            nonce(0xA2),
            nonce(0xA5),
            // The nonce of the reflected messages: no row names it.
            nonce(0xA3),
        ] {
            tx.execute(&statement, [])?;
        }
        Ok::<(), kourier_db::Error>(())
    })
    .expect("the rows");
    assert_eq!(
        sweep_inbound(db, NOW).expect("the sweep"),
        InboundSwept {
            reflected: 1,
            pending: 1
        }
    );
    let left: Vec<String> = inbound_all(db).into_iter().map(|r| r.sender).collect();
    assert_eq!(left, ["L1SWEEP2", "L1SWEEP4", "L1SWEEP5"]);
    assert!(
        !has_nonce(db, NonceScope::Csp, [0xA1; 32]),
        "the expired row's nonce stayed"
    );
    for byte in [0xA2, 0xA3, 0xA5] {
        assert!(
            has_nonce(db, NonceScope::Csp, [byte; 32]),
            "a nonce that stays went"
        );
    }
    assert_eq!(
        sweep_inbound(db, NOW).expect("again"),
        InboundSwept::default()
    );
    assert_eq!(RULE_PENDING_EXPIRED, "inbound-pending-expired");
}

/// The test door fails the zone it names with its code after the zone's writes, and nothing of the zone stays: RECEIVE
/// (the store's 13), REFLECTED (the content's 275), the sweep (13); an empty point turns it off.
#[test]
fn inbound_the_door_fails_each_zone_and_nothing_of_it_stays() {
    use kourier_threema::failpoints as fp;
    let t = temp_store();
    let db = &t.db;
    inbound_linked(db);
    let wake = Rc::new(Notify::new());
    let text = inbound_text(IN_PEER, 1);
    let r1 = inbound_reflect(1);
    let batch = [Inbound::Keep(inbound_kept(IN_PEER, 1, &text, &r1, true))];
    let code = |e: StoreError| match e {
        StoreError::Sql { code, .. } => code,
        _ => None,
    };
    fail_inbound_zone(fp::INSIDE_INBOUND_RECEIVE, 13);
    let error = receive_inbound(db, &receiving(&wake, IN_T0), &batch).expect_err("the door");
    assert_eq!(code(error), Some(13));
    assert!(inbound_all(db).is_empty() && outbox_events(db).is_empty());
    assert!(!has_nonce(db, NonceScope::Csp, [0xC1; 32]));
    fail_inbound_zone("", 0);
    receive_inbound(db, &receiving(&wake, IN_T0), &batch).expect("the door is off");
    fail_inbound_zone(fp::INSIDE_INBOUND_REFLECTED, 275);
    let error =
        mark_inbound_reflected(db, &[1], IN_T0 + 1, &inbound_receipt).expect_err("the door");
    assert_eq!(code(error), Some(275));
    assert_eq!(inbound_all(db), [pending_row(1, IN_PEER, 1, true, true)]);
    assert!(tasks(db).is_empty(), "a failed zone left its receipt");
    fail_inbound_zone(fp::INSIDE_INBOUND_SWEEP, 13);
    let error = sweep_inbound(db, i64::MAX).expect_err("the door");
    assert_eq!(code(error), Some(13));
    assert_eq!(inbound_all(db).len(), 1, "a failed sweep took a row");
    fail_inbound_zone("", 0);
    assert_eq!(
        sweep_inbound(db, i64::MAX).expect("the door is off"),
        InboundSwept {
            reflected: 0,
            pending: 1
        }
    );
}
