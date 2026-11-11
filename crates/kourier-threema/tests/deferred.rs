//! Schema 3 (`migrations/0003_receive.sql`, TH5 A4): the `deferred` envelopes and the kept sum of
//! `deferred_total`. The migration of a schema-2 file; each CHECK on both sides; the bounds at N and
//! N+1 (small bounds through `DeferredBounds`, the real ones pinned by a test of the constants); the
//! kept sum against `SUM(length(envelope))` after every kind of write and after a rolled-back zone;
//! the re-map's pages by id (at most 30, BELOW the version only); the 45-day age bound. The kill loops
//! of the three zones are in `tests/kill.rs`. Envelopes are synthetic bytes, compared in code and never
//! printed.

use kourier_db::ZoneDb;
use kourier_db::rusqlite::{Connection, params};
use kourier_threema::StoreError;
use kourier_threema::store::{
    Deferred, DeferredBounds, DeferredSource, DeferredTotal, Dropped, NewDeferred, REMAP_PAGE,
    add_deferred, add_deferred_batch, deferred_page, deferred_total, delete_deferred,
    expire_deferred, set_deferred_version,
};

use crate::support::{db_path, key, temp_store};

/// A local time of the tests (ms).
const T0: i64 = 1_761_350_400_000;

/// Bounds nothing in a test reaches.
const WIDE: DeferredBounds = DeferredBounds {
    rows: 1_000_000,
    bytes: i64::MAX,
    age_ms: i64::MAX,
};

/// `refused` is the file's own CHECK refusing `op` (round A, H-1: a `Sql` failure with SQLite's primary code
/// `SQLITE_CONSTRAINT` (19), whatever the extended one).
fn assert_refused(refused: Option<&StoreError>, op: &str, case: &str) {
    assert!(
        matches!(refused, Some(StoreError::Sql { op: got, code: Some(code) }) if *got == op && code & 0xff == 19),
        "{case}: not a constraint failure of {op}: {refused:?}"
    );
}

/// A row of `len` bytes of `byte`, deferred by TH5's rule `envelope` at version 1.
fn row(envelope: &[u8]) -> NewDeferred<'_> {
    NewDeferred {
        source: DeferredSource::Reflected,
        kind: "envelope",
        map_version: 1,
        server_at: 1_761_000_000_000,
        flags: 0,
        envelope,
    }
}

fn add(db: &ZoneDb, rows: &[NewDeferred<'_>], now: i64, bounds: &DeferredBounds) -> Dropped {
    add_deferred_batch(db, rows, now, bounds).expect("defer")
}

/// The ids in the table, in order.
fn ids(db: &ZoneDb) -> Vec<i64> {
    db.read(|conn: &Connection| {
        let mut stmt = conn
            .prepare("SELECT id FROM deferred ORDER BY id")
            .map_err(kourier_db::Error::from)?;
        stmt.query_map([], |r| r.get(0))
            .map_err(kourier_db::Error::from)?
            .collect::<Result<Vec<i64>, _>>()
            .map_err(kourier_db::Error::from)
    })
    .expect("ids")
}

fn total(db: &ZoneDb) -> DeferredTotal {
    db.read(|conn: &Connection| deferred_total(conn))
        .expect("the kept sum")
}

/// The kept sum equals the table's own count and sum of `length(envelope)`. Returns it.
fn assert_sum_exact(db: &ZoneDb, when: &str) -> DeferredTotal {
    let counted: (i64, i64) = db
        .read(|conn: &Connection| {
            conn.query_row(
                "SELECT count(*), coalesce(sum(length(envelope)), 0) FROM deferred",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("count");
    let kept = total(db);
    assert!(
        (kept.rows, kept.bytes) == counted,
        "{when}: the kept sum is not the table's count and byte sum"
    );
    kept
}

/// Schema 2 alone, as TH4 left a file (the first two migrations of the crate, frozen).
const SCHEMA_2: kourier_db::Migrations = kourier_db::Migrations::new(
    "threema",
    &[
        kourier_db::migration!(1, "0001_init"),
        kourier_db::migration!(2, "0002_join"),
    ],
);

/// A file of schema 2 (with a contact row) opens, is at the latest schema after (4 since TH6a), keeps its row,
/// `deferred` is there and empty and the kept sum is `(0, 0)`.
#[test]
fn a_schema_2_file_migrates_to_schema_3_with_deferred_empty() {
    let dir = tempfile::tempdir().expect("temp dir");
    {
        let mut conn = kourier_db::open(&db_path(dir.path()), &key()).expect("open");
        kourier_db::migrate(&mut conn, &kourier_db::outbox::MIGRATIONS).expect("outbox");
        kourier_db::migrate(&mut conn, &SCHEMA_2).expect("schema 2");
        conn.execute(
            "INSERT INTO contact (identity, sync) VALUES ('A4MIGR08', X'0a00')",
            [],
        )
        .expect("a schema-2 row");
        for table in ["deferred", "deferred_total"] {
            let there: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                    [table],
                    |r| r.get(0),
                )
                .expect(table);
            assert!(!there, "{table} exists at schema 2");
        }
    }
    let db = kourier_threema::store::open(&db_path(dir.path()), &key()).expect("open migrates");
    let (version, contacts, deferred): (i64, i64, i64) = db
        .read(|conn: &Connection| {
            conn.query_row(
                "SELECT (SELECT version FROM schema_version WHERE component = 'threema'),
                        (SELECT count(*) FROM contact WHERE identity = 'A4MIGR08'),
                        (SELECT count(*) FROM deferred)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("read");
    assert_eq!(version, 6, "the file is not at the latest schema");
    assert_eq!(contacts, 1, "the schema-2 row is gone");
    assert_eq!(deferred, 0, "deferred is not empty after the migration");
    assert_eq!(total(&db), DeferredTotal { rows: 0, bytes: 0 });
}

/// Whether the statements, in one zone of a fresh store, take their rows.
fn takes(statements: &[String]) -> bool {
    let t = temp_store();
    t.db.zone(|tx| {
        for statement in statements {
            tx.execute(statement, []).map_err(kourier_db::Error::from)?;
        }
        Ok::<(), kourier_db::Error>(())
    })
    .is_ok()
}

/// An INSERT into `deferred` with these SQL values (source, kind, map_version, server_at, flags,
/// received_at, envelope).
fn insert(source: &str, kind: &str, version: &str, flags: &str, envelope: &str) -> String {
    format!(
        "INSERT INTO deferred (source, kind, map_version, server_at, flags, received_at, envelope) \
         VALUES ({source}, {kind}, {version}, 5, {flags}, 6, {envelope})"
    )
}

/// A good row with one column replaced: `col` names it.
fn with(col: &str, value: &str) -> String {
    let pick = |name: &str, good: &str| {
        if name == col {
            value.to_owned()
        } else {
            good.to_owned()
        }
    };
    insert(
        &pick("source", "'reflected'"),
        &pick("kind", "'envelope'"),
        &pick("version", "1"),
        &pick("flags", "0"),
        &pick("envelope", "X'0a'"),
    )
}

/// Each CHECK of schema 3 takes its good row and refuses the bad one, on both sides: `map_version` 0 and
/// 1; the two sources and another; a kind of 0, 1, 32 and 33 characters, upper case and `_`; `flags` -1,
/// 0, 65,535 and 65,536; an envelope of 0, 1, 65,536 and 65,537 bytes and as text; every NOT NULL column
/// as NULL (S10: a CHECK passes NULL); the kept sum's one row (`id` 2, a negative count).
#[test]
fn each_check_of_schema_3_refuses_its_bad_row_and_takes_the_good_one() {
    let bytes = |n: usize| format!("X'{}'", "5a".repeat(n));
    let kind = |n: usize| format!("'{}'", "k".repeat(n));
    let cases: Vec<(&str, Vec<String>, bool)> = vec![
        ("the good row", vec![with("", "")], true),
        ("map_version 0", vec![with("version", "0")], false),
        ("map_version 1", vec![with("version", "1")], true),
        ("map_version 2", vec![with("version", "2")], true),
        ("map_version -1", vec![with("version", "-1")], false),
        ("map_version NULL", vec![with("version", "NULL")], false),
        ("source csp", vec![with("source", "'csp'")], true),
        ("source other", vec![with("source", "'d2m'")], false),
        ("source upper case", vec![with("source", "'Reflected'")], false),
        ("source NULL", vec![with("source", "NULL")], false),
        ("a kind of 0", vec![with("kind", "''")], false),
        ("a kind of 1", vec![with("kind", &kind(1))], true),
        ("a kind of 32", vec![with("kind", &kind(32))], true),
        ("a kind of 33", vec![with("kind", &kind(33))], false),
        ("a kind with a digit and -", vec![with("kind", "'unknown-type-0'")], true),
        ("a kind in upper case", vec![with("kind", "'Envelope'")], false),
        ("a kind with _", vec![with("kind", "'unknown_type'")], false),
        ("a kind NULL", vec![with("kind", "NULL")], false),
        ("flags -1", vec![with("flags", "-1")], false),
        ("flags 0", vec![with("flags", "0")], true),
        ("flags 65535", vec![with("flags", "65535")], true),
        ("flags 65536", vec![with("flags", "65536")], false),
        ("flags NULL", vec![with("flags", "NULL")], false),
        ("an envelope of 0", vec![with("envelope", "X''")], false),
        ("an envelope of 1", vec![with("envelope", &bytes(1))], true),
        ("an envelope of 65536", vec![with("envelope", &bytes(65_536))], true),
        ("an envelope of 65537", vec![with("envelope", &bytes(65_537))], false),
        ("an envelope as text", vec![with("envelope", "'abc'")], false),
        ("an envelope NULL", vec![with("envelope", "NULL")], false),
        (
            "server_at NULL",
            vec![
                "INSERT INTO deferred (source, kind, map_version, server_at, flags, received_at, envelope) \
                 VALUES ('reflected', 'envelope', 1, NULL, 0, 6, X'0a')"
                    .to_owned(),
            ],
            false,
        ),
        (
            "received_at NULL",
            vec![
                "INSERT INTO deferred (source, kind, map_version, server_at, flags, received_at, envelope) \
                 VALUES ('reflected', 'envelope', 1, 5, 0, NULL, X'0a')"
                    .to_owned(),
            ],
            false,
        ),
        (
            "a server time of the high bit (a u64 as its i64)",
            vec![
                "INSERT INTO deferred (source, kind, map_version, server_at, flags, received_at, envelope) \
                 VALUES ('reflected', 'envelope', 1, -1, 0, 6, X'0a')"
                    .to_owned(),
            ],
            true,
        ),
        (
            "a second kept-sum row",
            vec!["INSERT INTO deferred_total (id, row_count, byte_sum) VALUES (2, 0, 0)".to_owned()],
            false,
        ),
        (
            "a negative kept count",
            vec!["UPDATE deferred_total SET row_count = -1".to_owned()],
            false,
        ),
        (
            "a negative kept sum",
            vec!["UPDATE deferred_total SET byte_sum = -1".to_owned()],
            false,
        ),
        (
            "a deleted row that was never there (the kept count would go below 0)",
            vec![
                "DELETE FROM deferred_total".to_owned(),
                "INSERT INTO deferred_total (id, row_count, byte_sum) VALUES (1, 0, 0)".to_owned(),
                with("", ""),
                "UPDATE deferred_total SET row_count = 0, byte_sum = 0".to_owned(),
                "DELETE FROM deferred".to_owned(),
            ],
            false,
        ),
    ];
    for (case, statements, expected) in cases {
        assert_eq!(takes(&statements), expected, "{case}");
    }

    // The same CHECK through the store: version 0 is refused on insert and on a re-defer, and the
    // error names the operation only.
    let t = temp_store();
    let envelope = [0x5a_u8; 4];
    let mut zero = row(&envelope);
    zero.map_version = 0;
    let refused = t.db.zone(|tx| add_deferred(tx, &zero, T0, &WIDE)).err();
    assert_refused(
        refused.as_ref(),
        "defer an envelope failed",
        "version 0 is taken by the store",
    );
    add(&t.db, &[row(&envelope)], T0, &WIDE);
    let id = ids(&t.db)[0];
    let refused =
        t.db.zone(|tx| set_deferred_version(tx, id, "envelope", 0))
            .err();
    assert_refused(
        refused.as_ref(),
        "defer a row again failed",
        "a re-defer to version 0 is taken",
    );
    assert_sum_exact(&t.db, "after the refusals");
}

/// The constants of C14: 20,000 rows, 256 MiB, 45 days; a re-map page of 30.
#[test]
fn the_real_bounds_are_20000_rows_256_mib_and_45_days() {
    assert_eq!(
        DeferredBounds::REAL,
        DeferredBounds {
            rows: 20_000,
            bytes: 268_435_456,
            age_ms: 3_888_000_000,
        }
    );
    assert_eq!(REMAP_PAGE, 30);
}

/// The row bound at N and N+1: N rows stay; one more drops the OLDEST (the lowest id), counted.
#[test]
fn the_row_bound_keeps_n_rows_and_drops_the_oldest_at_n_plus_1() {
    let t = temp_store();
    let bounds = DeferredBounds { rows: 3, ..WIDE };
    let envelopes: Vec<[u8; 8]> = (1..=5).map(|i| [i; 8]).collect();
    for e in &envelopes[..3] {
        assert_eq!(add(&t.db, &[row(e)], T0, &bounds), Dropped::default());
    }
    assert_eq!(ids(&t.db), [1, 2, 3], "N rows stay");
    assert_eq!(
        add(&t.db, &[row(&envelopes[3])], T0, &bounds),
        Dropped {
            full: 1,
            expired: 0
        }
    );
    assert_eq!(ids(&t.db), [2, 3, 4], "the oldest row did not go at N+1");
    // In one zone: two more rows drop the two oldest.
    let d = add(
        &t.db,
        &[row(&envelopes[4]), row(&envelopes[0])],
        T0,
        &bounds,
    );
    assert_eq!(d.full, 2);
    assert_eq!(ids(&t.db), [4, 5, 6]);
    assert_sum_exact(&t.db, "after the row bound");
}

/// The byte bound at B and B+1: rows summing to exactly B stay; one byte more drops the oldest rows
/// until the sum is at most B again.
#[test]
fn the_byte_bound_keeps_b_bytes_and_drops_the_oldest_at_b_plus_1() {
    let t = temp_store();
    let bounds = DeferredBounds { bytes: 100, ..WIDE };
    let (a, b, c, d) = ([1u8; 40], [2u8; 60], [3u8; 1], [4u8; 99]);
    add(&t.db, &[row(&a), row(&b)], T0, &bounds);
    assert_eq!(ids(&t.db), [1, 2], "B bytes stay");
    assert_eq!(
        total(&t.db),
        DeferredTotal {
            rows: 2,
            bytes: 100
        }
    );
    assert_eq!(add(&t.db, &[row(&c)], T0, &bounds).full, 1);
    assert_eq!(ids(&t.db), [2, 3], "the oldest row did not go at B+1");
    assert_eq!(total(&t.db), DeferredTotal { rows: 2, bytes: 61 });
    // 61 + 99 = 160: row 2 (60) goes, rows 3 and 4 make exactly B again.
    assert_eq!(add(&t.db, &[row(&d)], T0, &bounds).full, 1);
    assert_eq!(ids(&t.db), [3, 4]);
    assert_eq!(
        total(&t.db),
        DeferredTotal {
            rows: 2,
            bytes: 100
        }
    );
    assert_sum_exact(&t.db, "after the byte bound");
}

/// The kept sum equals `count(*)` and `SUM(length(envelope))` after inserts, a delete, a re-defer, the
/// bounds and the age bound, and is the one from before after a zone that rolled back (inserts,
/// a delete and an eviction in it).
#[test]
fn the_kept_sum_is_exact_after_every_write_and_after_a_rolled_back_zone() {
    let t = temp_store();
    let bounds = DeferredBounds { rows: 4, ..WIDE };
    let envelopes: Vec<Vec<u8>> = (1..=6).map(|i| vec![i as u8; 10 * i]).collect();
    add(
        &t.db,
        &[row(&envelopes[0]), row(&envelopes[1]), row(&envelopes[2])],
        T0,
        &bounds,
    );
    assert_eq!(
        assert_sum_exact(&t.db, "after the inserts"),
        DeferredTotal { rows: 3, bytes: 60 }
    );
    let deleted = t.db.zone(|tx| delete_deferred(tx, 2)).expect("delete");
    assert!(deleted, "row 2 was not there");
    assert_eq!(
        assert_sum_exact(&t.db, "after a delete"),
        DeferredTotal { rows: 2, bytes: 40 }
    );
    assert!(
        !t.db
            .zone(|tx| delete_deferred(tx, 2))
            .expect("delete again"),
        "a deleted row was there twice"
    );
    t.db.zone(|tx| set_deferred_version(tx, 1, "unknown-type", 2))
        .expect("re-defer");
    assert_sum_exact(&t.db, "after a re-defer");
    add(
        &t.db,
        &[row(&envelopes[3]), row(&envelopes[4]), row(&envelopes[5])],
        T0,
        &bounds,
    );
    assert_eq!(ids(&t.db), [3, 4, 5, 6], "the bound kept the wrong rows");
    let before = assert_sum_exact(&t.db, "after an eviction");
    assert_eq!(
        before,
        DeferredTotal {
            rows: 4,
            bytes: 180
        }
    );

    // A zone that writes, deletes and evicts, then fails: nothing of it stays, the sum included.
    let failed = t.db.zone(|tx| {
        add_deferred(tx, &row(&envelopes[0]), T0, &bounds)?;
        delete_deferred(tx, 5)?;
        add_deferred(tx, &row(&envelopes[1]), T0, &bounds)?;
        add_deferred(tx, &row(&envelopes[2]), T0, &bounds)?;
        Err::<(), _>(StoreError::Failed("the test's own failure"))
    });
    assert!(failed.is_err());
    assert_eq!(ids(&t.db), [3, 4, 5, 6], "a rolled-back zone left rows");
    assert_eq!(
        assert_sum_exact(&t.db, "after a rolled-back zone"),
        before,
        "a rolled-back zone moved the kept sum"
    );

    // The age bound at a start.
    let gone =
        expire_deferred(&t.db, T0 + 1, &DeferredBounds { age_ms: 0, ..WIDE }).expect("expire");
    assert_eq!(gone, 4);
    assert_eq!(
        assert_sum_exact(&t.db, "after the age bound"),
        DeferredTotal { rows: 0, bytes: 0 }
    );
}

/// A page is the rows BELOW the version (a row AT it is not mapped again: the `<` of M3), by id, after
/// the cursor, at most 30; a row deleted or deferred again in a zone is not in the pages after it.
#[test]
fn pages_come_by_id_at_most_30_below_the_version() {
    let t = temp_store();
    let envelope = [0x5a_u8; 3];
    let v1: Vec<NewDeferred<'_>> = (0..31).map(|_| row(&envelope)).collect();
    add(&t.db, &v1, T0, &WIDE);
    let mut v2 = row(&envelope);
    v2.map_version = 2;
    add(&t.db, &[v2, row(&envelope)], T0, &WIDE);
    // Ids 1-31 and 33 at version 1, 32 at version 2.
    let page = |version: u32, after: i64| -> Vec<i64> {
        t.db.read(|conn: &Connection| deferred_page(conn, version, after))
            .expect("page")
            .iter()
            .map(|d: &Deferred| d.id)
            .collect()
    };
    assert_eq!(
        page(1, 0),
        Vec::<i64>::new(),
        "a row AT the version is paged"
    );
    let first = page(2, 0);
    assert_eq!(first, (1..=30).collect::<Vec<i64>>(), "the first page");
    assert_eq!(
        page(2, 30),
        [31, 33],
        "the second page skips the row at the version"
    );
    assert_eq!(page(2, 33), Vec::<i64>::new());
    assert_eq!(page(3, 30), [31, 32, 33], "version 3 pages every row");

    // Inside a re-map zone: row 31 mapped, row 33 deferred again at version 2.
    t.db.zone(|tx| {
        assert!(delete_deferred(tx, 31)?);
        assert!(set_deferred_version(tx, 33, "unknown-type", 2)?);
        Ok::<(), StoreError>(())
    })
    .expect("a re-map zone");
    assert_eq!(
        page(2, 30),
        Vec::<i64>::new(),
        "a re-mapped row is paged again"
    );
    assert_eq!(page(3, 30), [32, 33]);
    assert_sum_exact(&t.db, "after the re-map zone");
}

/// A row comes back as stored: every column, a u64 time above `i64::MAX`, the top flags, the CSP source;
/// its `Debug` prints neither the envelope nor its length.
#[test]
fn a_row_round_trips_and_its_debug_prints_no_envelope() {
    let t = temp_store();
    let envelope: Vec<u8> = (0..=255u8).cycle().take(65_536).collect();
    let stored = NewDeferred {
        source: DeferredSource::Csp,
        kind: "unknown-type",
        map_version: 7,
        server_at: u64::MAX - 5,
        flags: u16::MAX,
        envelope: &envelope,
    };
    add(&t.db, &[stored], T0, &WIDE);
    let back =
        t.db.read(|conn: &Connection| deferred_page(conn, 8, 0))
            .expect("page");
    assert_eq!(back.len(), 1);
    let d = &back[0];
    assert!(
        d.id == 1
            && d.source == DeferredSource::Csp
            && d.kind == "unknown-type"
            && d.map_version == 7
            && d.server_at == u64::MAX - 5
            && d.flags == u16::MAX
            && d.received_at == T0
            && d.envelope.as_slice() == envelope.as_slice(),
        "the row did not come back as stored"
    );
    let printed = format!("{d:?}");
    assert!(
        !printed.contains("65536") && !printed.contains("[0, 1, 2"),
        "Debug prints the envelope or its length"
    );
    // The stored time is the i64 of the same 64 bits.
    let raw: i64 =
        t.db.read(|conn: &Connection| {
            conn.query_row("SELECT server_at FROM deferred", params![], |r| r.get(0))
                .map_err(kourier_db::Error::from)
        })
        .expect("raw");
    assert_eq!(raw, -6);
}

/// The age bound: a row exactly 45 days old stays, one ms more and it goes, at a start
/// (`expire_deferred`) and in the zone of an insert (counted as `expired`).
#[test]
fn rows_past_45_days_go_at_a_start_and_on_an_insert() {
    let age = DeferredBounds::REAL.age_ms;
    let t = temp_store();
    let envelope = [0x5a_u8; 5];
    add(&t.db, &[row(&envelope)], T0, &DeferredBounds::REAL);
    add(&t.db, &[row(&envelope)], T0 + 1, &DeferredBounds::REAL);
    assert_eq!(
        expire_deferred(&t.db, T0 + age, &DeferredBounds::REAL).expect("expire"),
        0,
        "a row of exactly 45 days went"
    );
    assert_eq!(ids(&t.db), [1, 2]);
    assert_eq!(
        expire_deferred(&t.db, T0 + age + 1, &DeferredBounds::REAL).expect("expire"),
        1
    );
    assert_eq!(ids(&t.db), [2], "the row past 45 days stayed at the start");
    // On an insert: row 2 (T0 + 1) is past the bound at T0 + 1 + age + 1.
    let d = add(
        &t.db,
        &[row(&envelope)],
        T0 + 1 + age + 1,
        &DeferredBounds::REAL,
    );
    assert_eq!(
        d,
        Dropped {
            full: 0,
            expired: 1
        }
    );
    // Id 3, not 1: the table was empty before the insert, and an id is never used again.
    assert_eq!(ids(&t.db), [3], "the row past 45 days stayed at an insert");
    assert_sum_exact(&t.db, "after the age bound");
}

// THE RE-MAP (TH5 A8, decision M3) ----------------------------------------------------------------

/// The re-map's synthetic IDs (8 characters, each a substring of no other test value).
const RM_OWN: &str = "A8REMAP1";
const RM_PEER: &str = "A8REMAP2";
const RM_HIDDEN: &str = "A8REMAP3";
/// The messages' `created_at` and the rows' `server_at` base.
const RM_AT: u64 = 1_761_000_000_000;
/// The local time of the re-map's zones (its `core_outbox` rows).
const RM_NOW: i64 = T0 + 60_000;

/// A linked file (identity + the marker) holding a DIRECT contact and a HIDDEN one.
fn linked_store() -> crate::support::TempStore {
    use crate::support::mock::envelope::ContactDelta;
    use kourier_threema::store::{Identity, SlotState, mark_join_ready, save_identity};
    use prost::Message as _;
    use zeroize::Zeroizing;
    let store = temp_store();
    let identity = Identity {
        threema_id: RM_OWN.to_owned(),
        client_key: Zeroizing::new([0x51; 32]),
        server_group: "b".to_owned(),
        device_group_key: Zeroizing::new([0x52; 32]),
        device_cookie: Zeroizing::new([0x53; 16]),
        d2m_device_id: 7,
        csp_device_id: 8,
        slot_state: SlotState::Existing,
        device_label: "Kourier (a8-remap)".to_owned(),
        linked_at_ms: T0,
    };
    store
        .db
        .zone(|tx| {
            save_identity(tx, &identity)?;
            mark_join_ready(tx, T0)
        })
        .expect("the identity");
    store
        .db
        .zone(|tx| {
            for sync in [
                ContactDelta::create(RM_PEER, [7; 32], RM_AT).build(),
                ContactDelta::create(RM_HIDDEN, [8; 32], RM_AT)
                    .hidden()
                    .build(),
            ] {
                tx.execute(
                    "INSERT INTO contact (identity, sync, last_update_at) VALUES (?1, ?2, NULL)",
                    params![sync.identity, sync.encode_to_vec()],
                )?;
            }
            Ok::<(), kourier_db::Error>(())
        })
        .expect("the contacts");
    store
}

/// The plaintext of an incoming 1:1 message `msg` of `sender`.
fn rm_plaintext(sender: &str, msg: crate::support::mock::envelope::Msg) -> Vec<u8> {
    use prost::Message as _;
    crate::support::mock::envelope::Author::new(0x0A08)
        .incoming(sender, &msg)
        .encode_to_vec()
}

fn rm_text(sender: &str, n: u8) -> Vec<u8> {
    use crate::support::mock::envelope::Msg;
    rm_plaintext(sender, Msg::text([n; 8], RM_AT, "remapped text"))
}

/// A deferred row of `envelope` at `version`, its `server_at` RM_AT + n.
fn rm_row(envelope: &[u8], n: u8, version: u32) -> NewDeferred<'_> {
    NewDeferred {
        source: DeferredSource::Reflected,
        kind: "envelope",
        map_version: version,
        server_at: RM_AT + u64::from(n),
        flags: 0,
        envelope,
    }
}

/// `core_outbox`'s rows, decoded, in seq order.
fn outbox(db: &ZoneDb) -> Vec<Vec<kourier_backend_api::BackendEvent>> {
    let rows: Vec<Vec<u8>> = db
        .read(|conn: &Connection| {
            let mut stmt = conn
                .prepare("SELECT envelope FROM core_outbox ORDER BY seq")
                .map_err(kourier_db::Error::from)?;
            stmt.query_map([], |r| r.get(0))
                .map_err(kourier_db::Error::from)?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(kourier_db::Error::from)
        })
        .expect("the outbox rows");
    rows.iter()
        .map(|r| kourier_backend_api::decode_outbox_row(r).expect("an outbox row decodes"))
        .collect()
}

/// `deferred`'s rows as (id, kind, map_version).
fn kinds(db: &ZoneDb) -> Vec<(i64, String, u32)> {
    db.read(|conn: &Connection| {
        let mut stmt = conn
            .prepare("SELECT id, kind, map_version FROM deferred ORDER BY id")
            .map_err(kourier_db::Error::from)?;
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(kourier_db::Error::from)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(kourier_db::Error::from)
    })
    .expect("the deferred rows")
}

fn direct(id: &str) -> kourier_model::ConvKey {
    kourier_model::ConvKey::Direct(
        kourier_model::UserId::from_parts("threema", id).expect("a user"),
    )
}

/// Round B, S6: the HISTORY rows of re-mapped rows `ns` (their `server_at` RM_AT + n): ONE `HistoryBatch` of
/// `conv` (`has_more: false`, no progress), its messages in row order, each with the row's `server_at` and
/// `received_at`.
fn assert_history_rows(
    event: &kourier_backend_api::BackendEvent,
    conv: &kourier_model::ConvKey,
    ns: &[u8],
) {
    match event {
        kourier_backend_api::BackendEvent::HistoryBatch {
            conv: batch,
            messages,
            has_more,
            progress,
        } => {
            assert_eq!(batch, conv, "the history batch is of another chat");
            assert!(
                !has_more && progress.is_none(),
                "a re-map's batch says more"
            );
            assert_eq!(
                messages.len(),
                ns.len(),
                "not one history row per mapped row"
            );
            for (m, n) in messages.iter().zip(ns) {
                assert_eq!(&m.conv, conv, "a re-mapped message is in another chat");
                assert_eq!(
                    m.server_at,
                    i64::try_from(RM_AT + u64::from(*n)).ok(),
                    "server_at is not the row's"
                );
                assert_eq!(m.received_at, T0, "received_at is not the row's");
                assert_eq!(m.body, kourier_model::Body::text("remapped text"));
            }
        }
        _ => panic!("the re-mapped rows are not one history batch"),
    }
}

/// M3: rows deferred by an older build (version 1, kind `envelope`) are mapped again by `remap_with(2)` with
/// this build's table: a TEXT (mapped now) reaches `core_outbox` as a HISTORY row (round B, S6: one
/// `HistoryBatch` per conversation of the page, all in the page's ONE row) and its row is deleted in the
/// zone of its events; a POLL VOTE (still deferred) gets kind `poll-vote` and version 2; a type
/// 0x00 (dropped now) is deleted with its counted WARN; a row ALREADY at version 2 is untouched (a row AT the
/// version is never mapped again); a hidden contact's text states its chat's patch first (E5). A version-1
/// re-map (the real build's) maps nothing.
#[test]
fn a_newer_version_maps_old_rows_again_and_a_row_at_the_version_stays() {
    use crate::support::mock::envelope::Msg;
    use crate::support::warns::Warns;
    use kourier_backend_api::BackendEvent;
    use kourier_threema::store::{Remapped, remap_with};
    let warns = Warns::install();
    let store = linked_store();
    let db = &store.db;
    let text = rm_text(RM_PEER, 1);
    let vote = rm_plaintext(
        RM_PEER,
        Msg::raw(
            [2; 8],
            RM_AT,
            crate::support::mock::body::ty::POLL_VOTE,
            vec![7; 20],
        ),
    );
    let invalid = rm_plaintext(RM_PEER, Msg::raw([3; 8], RM_AT, 0x00, b"zero".to_vec()));
    let at_two = rm_text(RM_PEER, 4);
    let hidden = rm_text(RM_HIDDEN, 5);
    add(
        db,
        &[
            rm_row(&text, 1, 1),
            rm_row(&vote, 2, 1),
            rm_row(&invalid, 3, 1),
            rm_row(&at_two, 4, 2),
            rm_row(&hidden, 5, 1),
        ],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());

    let none = remap_with(db, 1, RM_NOW, &wake).expect("a version-1 re-map");
    assert_eq!(
        none,
        Remapped::default(),
        "a version-1 build mapped a version-1 row again"
    );
    assert!(outbox(db).is_empty(), "a version-1 re-map wrote events");

    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!(
        done,
        Remapped {
            zones: 1,
            mapped: 2,
            deferred_again: 1,
            dropped: 1,
            left: 0,
        },
        "the re-map's counts"
    );
    assert_eq!(
        kinds(db),
        vec![
            (2, "poll-vote".to_owned(), 2),
            (4, "envelope".to_owned(), 2)
        ],
        "the deferred rows after the re-map: the vote again at 2, the row at 2 untouched"
    );
    assert_sum_exact(db, "after the re-map");
    let rows = outbox(db);
    assert_eq!(
        rows.len(),
        1,
        "not one core_outbox row for the page, or one for the row at 2"
    );
    match rows[0].as_slice() {
        [
            direct_batch,
            BackendEvent::ConversationUpsert(patch),
            hidden_batch,
        ] => {
            assert_history_rows(direct_batch, &direct(RM_PEER), &[1]);
            assert_eq!(
                patch.key,
                direct(RM_HIDDEN),
                "E5's patch is not the hidden contact's chat"
            );
            assert_eq!(
                patch.features.as_ref().and_then(|f| f.cannot_send.clone()),
                None,
                "E5's patch says the chat cannot be sent in"
            );
            assert_history_rows(hidden_batch, &direct(RM_HIDDEN), &[5]);
        }
        _ => panic!(
            "the page is not [the direct text's batch, the hidden chat's patch, its text's batch]"
        ),
    }
    let lines = warns.at(tracing::Level::WARN);
    assert!(
        lines
            .iter()
            .any(|l| l == "deferred rows dropped by the re-map: 1 by invalid-type"),
        "the dropped row has no counted WARN by its rule"
    );

    // Again: nothing is left below 2.
    let again = remap_with(db, 2, RM_NOW, &wake).expect("a second re-map");
    assert_eq!(
        again,
        Remapped::default(),
        "a second re-map found rows below 2"
    );
    assert_eq!(outbox(db).len(), 1, "a second re-map wrote events");
}

/// TH5b S-L1: a table whose rows are all AT the version (a text, a poll vote and a settings delta, as a start finds
/// the rows its own build deferred) is mapped by nothing: `remap_with(2)` (the start's re-map) does nothing, every
/// row stays byte for byte with its kept sum, and no `core_outbox` row is written. The check before the first page
/// (`store::has_deferred_below`) is a COST fix: without it the result is the same (an equivalent behaviour mutant),
/// so the plan test in `src/store.rs` pins the check itself.
#[test]
fn a_remap_over_rows_all_at_its_version_maps_nothing() {
    use crate::support::mock::envelope::{Author, Msg};
    use kourier_threema::store::{Remapped, remap_with};
    use libthreema::protobuf::d2d_sync;
    use prost::Message as _;
    let store = linked_store();
    let db = &store.db;
    let text = rm_text(RM_PEER, 1);
    let vote = rm_plaintext(
        RM_PEER,
        Msg::raw(
            [2; 8],
            RM_AT,
            crate::support::mock::body::ty::POLL_VOTE,
            vec![7; 20],
        ),
    );
    let settings = Author::new(0x0A08)
        .settings_update(d2d_sync::Settings {
            read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32),
            ..Default::default()
        })
        .encode_to_vec();
    add(
        db,
        &[
            rm_row(&text, 1, 2),
            rm_row(&vote, 2, 2),
            NewDeferred {
                kind: "settings",
                ..rm_row(&settings, 3, 2)
            },
        ],
        T0,
        &WIDE,
    );
    let envelopes = |db: &ZoneDb| -> Vec<Vec<u8>> {
        db.read(|conn: &Connection| {
            let mut stmt = conn
                .prepare("SELECT envelope FROM deferred ORDER BY id")
                .map_err(kourier_db::Error::from)?;
            stmt.query_map([], |r| r.get(0))
                .map_err(kourier_db::Error::from)?
                .collect::<Result<Vec<Vec<u8>>, _>>()
                .map_err(kourier_db::Error::from)
        })
        .expect("the envelopes")
    };
    let sum = assert_sum_exact(db, "before the re-map");
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!(done, Remapped::default(), "a row at the version was mapped");
    assert_eq!(
        kinds(db),
        vec![
            (1, "envelope".to_owned(), 2),
            (2, "envelope".to_owned(), 2),
            (3, "settings".to_owned(), 2)
        ],
        "a row at the version changed its kind or version"
    );
    assert!(
        envelopes(db) == [text, vote, settings],
        "a row at the version changed its bytes"
    );
    assert_eq!(
        assert_sum_exact(db, "after the re-map"),
        sum,
        "the kept sum moved"
    );
    assert!(
        outbox(db).is_empty(),
        "a re-map over rows at its version wrote events"
    );
}

/// TH5b B2 (R15): version-1 rows of an edit, a reaction and a delete (what a TH5a build deferred) are mapped by the
/// version-2 re-map like a message: the SAME mapper, their events LIVE (not history rows: only a message is a
/// history row, S6) right after the history batch of the text they change, each dated by its own `created_at`
/// (R2); the rows are gone and the page is ONE `core_outbox` row.
#[test]
fn version_one_modifiers_are_mapped_by_the_remap_as_live_events_after_their_text() {
    use crate::support::mock::envelope::Msg;
    use kourier_backend_api::BackendEvent;
    use kourier_model::{Body, MsgTarget, TargetAuthor};
    use kourier_threema::store::{Remapped, remap_with};
    let store = linked_store();
    let db = &store.db;
    let text = rm_text(RM_PEER, 1);
    let edit = rm_plaintext(
        RM_PEER,
        Msg::edit([2; 8], RM_AT + 10, [1; 8], "edited text"),
    );
    let reaction = rm_plaintext(RM_PEER, Msg::reaction([3; 8], RM_AT + 20, [1; 8], "👍"));
    let delete = rm_plaintext(RM_PEER, Msg::delete([4; 8], RM_AT + 30, [1; 8]));
    add(
        db,
        &[
            rm_row(&text, 1, 1),
            rm_row(&edit, 2, 1),
            rm_row(&reaction, 3, 1),
            rm_row(&delete, 4, 1),
        ],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!(
        done,
        Remapped {
            zones: 1,
            mapped: 4,
            deferred_again: 0,
            dropped: 0,
            left: 0,
        },
        "the re-map's counts"
    );
    assert!(kinds(db).is_empty(), "a mapped row is still deferred");
    let rows = outbox(db);
    assert_eq!(rows.len(), 1, "not one core_outbox row for the page");
    let target = MsgTarget {
        conv: Some(direct(RM_PEER)),
        author: TargetAuthor::Unknown,
        local: "0101010101010101".into(),
        alternates: Vec::new(),
    };
    let by = Some(kourier_model::UserId::from_parts("threema", RM_PEER).expect("a user"));
    match rows[0].as_slice() {
        [text_batch, modifiers @ ..] => {
            assert_history_rows(text_batch, &direct(RM_PEER), &[1]);
            assert_eq!(
                modifiers,
                [
                    BackendEvent::Edited {
                        target: target.clone(),
                        by: by.clone(),
                        new_local: None,
                        body: Body::text("edited text"),
                        edited_at: i64::try_from(RM_AT + 10).expect("fits"),
                        keeps_quote: None,
                    },
                    BackendEvent::Reaction {
                        target: target.clone(),
                        by: by.clone(),
                        emoji: "👍".to_owned(),
                        remove: false,
                        at: i64::try_from(RM_AT + 20).expect("fits"),
                    },
                    BackendEvent::Deleted {
                        target,
                        by,
                        for_everyone: true,
                        at: i64::try_from(RM_AT + 30).expect("fits"),
                    },
                ],
                "the modifiers are not the live events, in row order"
            );
        }
        [] => panic!("an empty core_outbox row"),
    }
    assert_sum_exact(db, "after the re-map of the modifiers");
}

/// TH5b B4 (R13, N-1): version-1 rows of the SYNC kinds (what a TH5a build deferred as `settings`, `profile`,
/// `list-sync`: never evicted, never expired) are APPLIED by the version-2 re-map in arrival order through the
/// same mapper: a settings delta that blocks the direct contact, a profile delta, a list create, a second
/// settings delta that unblocks it; their LIVE events in the page's ONE row (no message, so no history batch),
/// every row gone, the BLOBs written in the page's zone (the second delta read the first one's row).
#[test]
fn version_one_sync_rows_are_applied_by_the_remap_in_arrival_order() {
    use crate::support::mock::envelope::Author;
    use kourier_backend_api::BackendEvent;
    use kourier_model::{AccountSettingsPatch, ContactPatch, Redacted, UserId};
    use kourier_threema::store::{Remapped, remap_with};
    use libthreema::protobuf::{common, d2d_sync};
    use prost::Message as _;
    let store = linked_store();
    let db = &store.db;
    let author = Author::new(0x0A08);
    let blocked = |ids: &[&str]| d2d_sync::Settings {
        read_receipt_policy: Some(d2d_sync::ReadReceiptPolicy::DontSendReadReceipt as i32),
        blocked_identities: Some(common::Identities {
            identities: ids.iter().map(|i| (*i).to_owned()).collect(),
        }),
        ..Default::default()
    };
    let list = d2d_sync::DistributionList {
        distribution_list_id: 0x0b04,
        name: Some("remapped list".to_owned()),
        member_identities: Some(common::Identities {
            identities: vec![RM_PEER.to_owned()],
        }),
        ..Default::default()
    };
    let first = author.settings_update(blocked(&[RM_PEER])).encode_to_vec();
    let profile = author
        .profile_update(d2d_sync::UserProfile {
            nickname: Some("Remapped Own".to_owned()),
            ..Default::default()
        })
        .encode_to_vec();
    let created = author.list_create(list.clone()).encode_to_vec();
    let second = author.settings_update(blocked(&[])).encode_to_vec();
    add(
        db,
        &[
            NewDeferred {
                kind: "settings",
                ..rm_row(&first, 1, 1)
            },
            NewDeferred {
                kind: "profile",
                ..rm_row(&profile, 2, 1)
            },
            NewDeferred {
                kind: "list-sync",
                ..rm_row(&created, 3, 1)
            },
            NewDeferred {
                kind: "settings",
                ..rm_row(&second, 4, 1)
            },
        ],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!(
        done,
        Remapped {
            zones: 1,
            mapped: 4,
            deferred_again: 0,
            dropped: 0,
            left: 0,
        },
        "the re-map's counts"
    );
    assert!(kinds(db).is_empty(), "a sync row is still deferred");
    let peer = UserId::from_parts("threema", RM_PEER).expect("a user");
    let upsert = |blocked: bool| {
        let mut patch = ContactPatch::new(peer.clone());
        patch.hidden = Some(false);
        patch.blocked = Some(blocked);
        BackendEvent::ContactUpsert(patch)
    };
    let switches = BackendEvent::AccountSettings(AccountSettingsPatch {
        read_receipts: Some(false),
        ..AccountSettingsPatch::default()
    });
    assert_eq!(
        outbox(db),
        vec![vec![
            switches.clone(),
            upsert(true),
            BackendEvent::AccountReady {
                own_ids: vec![UserId::from_parts("threema", RM_OWN).expect("a user")],
                device: None,
                display_name: Some(Redacted::new("Remapped Own".to_owned())),
                number: None,
            },
            switches,
            upsert(false),
        ]],
        "the re-mapped syncs' events are not the live events in row order"
    );
    let stored_list: Vec<u8> = db
        .read(|c: &Connection| {
            c.query_row(
                "SELECT sync FROM dist_list WHERE id = ?1",
                [0x0b04_i64],
                |r| r.get(0),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("the stored list");
    assert!(
        d2d_sync::DistributionList::decode(stored_list.as_slice()).expect("a list") == list,
        "the re-mapped list create was not stored"
    );
    assert_sum_exact(db, "after the re-map of the syncs");
}

/// M3: 61 rows below the version are mapped in 3 zones of at most 30 (30, 30, 1), every row deleted; one
/// `core_outbox` row per zone holding the zone's ONE history batch of the chat (round B, S6), in id order.
#[test]
fn sixty_one_rows_are_mapped_in_three_zones_of_at_most_thirty() {
    use kourier_threema::store::{Remapped, remap_with};
    let store = linked_store();
    let db = &store.db;
    let texts: Vec<Vec<u8>> = (1..=61u8).map(|n| rm_text(RM_PEER, n)).collect();
    let rows: Vec<NewDeferred<'_>> = texts
        .iter()
        .zip(1u8..)
        .map(|(t, n)| rm_row(t, n, 1))
        .collect();
    add(db, &rows, T0, &WIDE);
    assert_eq!(REMAP_PAGE, 30, "the page is not 30 rows");
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!(
        done,
        Remapped {
            zones: 3,
            mapped: 61,
            deferred_again: 0,
            dropped: 0,
            left: 0,
        },
        "61 rows are not 3 zones"
    );
    assert!(ids(db).is_empty(), "a mapped row was not deleted");
    let out = outbox(db);
    assert_eq!(out.len(), 3, "not one core_outbox row per zone");
    for (row, ns) in out.iter().zip([1..=30u8, 31..=60, 61..=61]) {
        match row.as_slice() {
            [batch] => assert_history_rows(batch, &direct(RM_PEER), &ns.collect::<Vec<u8>>()),
            _ => panic!("a re-mapped page is not one history batch"),
        }
    }
    assert_sum_exact(db, "after 61 rows");
}

/// S4 (round A): the re-map decides "the store fails" by the error's CODE. A CONTENT failure (the door's
/// `SQLITE_CONSTRAINT_CHECK`) in every row of a page does not stop the rows after it: 31 rows of one length
/// that fail and then one row that maps: the good row is mapped and deleted, the 31 stay (counted WARN
/// `remap-failed`, one line per page), the sum is exact.
#[test]
fn a_page_of_rows_the_mapper_refuses_moves_on_to_the_rows_after_it() {
    use crate::support::mock::envelope::Msg;
    use crate::support::warns::Warns;
    use kourier_threema::store::{Remapped, fail_deferred_of_length, remap_with};
    let warns = Warns::install();
    let store = linked_store();
    let db = &store.db;
    let bad: Vec<Vec<u8>> = (1..=31u8).map(|n| rm_text(RM_PEER, n)).collect();
    let good = rm_plaintext(
        RM_PEER,
        Msg::text([32; 8], RM_AT, "remapped text, and a longer one"),
    );
    assert!(
        bad.iter().all(|b| b.len() == bad[0].len()) && good.len() != bad[0].len(),
        "the door tells the rows apart by their length"
    );
    let mut rows: Vec<NewDeferred<'_>> = bad
        .iter()
        .zip(1u8..)
        .map(|(t, n)| rm_row(t, n, 1))
        .collect();
    rows.push(rm_row(&good, 32, 1));
    add(db, &rows, T0, &WIDE);
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    fail_deferred_of_length(bad[0].len(), 275);
    let done = remap_with(db, 2, RM_NOW, &wake);
    fail_deferred_of_length(0, 0);
    assert_eq!(
        done.expect("a content failure is no reason to stop the re-map"),
        Remapped {
            zones: 1,
            mapped: 1,
            deferred_again: 0,
            dropped: 0,
            left: 31,
        },
        "the counts of a re-map in which 31 rows fail"
    );
    assert_eq!(
        ids(db),
        (1..=31).collect::<Vec<i64>>(),
        "the failing rows are left, the good one is mapped and deleted"
    );
    let out = outbox(db);
    assert_eq!(out.len(), 1, "not one core_outbox row for the good row");
    assert!(
        matches!(
            out[0].as_slice(),
            [kourier_backend_api::BackendEvent::HistoryBatch { messages, .. }] if messages.len() == 1
        ),
        "the good row is not one history row"
    );
    assert_sum_exact(db, "after the re-map that left 31 rows");
    let lines = warns.at(tracing::Level::WARN);
    for line in [
        "deferred rows left as they are: 30 by remap-failed",
        "deferred rows left as they are: 1 by remap-failed",
    ] {
        assert!(lines.iter().any(|l| l == line), "no counted WARN `{line}`");
    }
    // A later start tries them again (they are still below the version) and leaves them again.
    fail_deferred_of_length(bad[0].len(), 275);
    let again = remap_with(db, 2, RM_NOW, &wake).expect("the second re-map");
    fail_deferred_of_length(0, 0);
    assert_eq!(
        (again.mapped, again.left),
        (0, 31),
        "the rows were not tried again"
    );
}

/// S4: the STORE's code (the door's `SQLITE_FULL`) stops the re-map at once: no row of the page is tried one
/// by one (the good rows around the failing one are not mapped), the rows all stay, the error is the store's.
#[test]
fn a_store_failure_stops_the_remap_and_leaves_every_row() {
    use crate::support::mock::envelope::Msg;
    use kourier_threema::store::{fail_deferred_of_length, remap_with};
    let store = linked_store();
    let db = &store.db;
    let good_a = rm_text(RM_PEER, 1);
    let bad = rm_plaintext(
        RM_PEER,
        Msg::text([2; 8], RM_AT, "a text of another length"),
    );
    let good_b = rm_text(RM_PEER, 3);
    assert!(bad.len() != good_a.len(), "the door tells the rows apart");
    add(
        db,
        &[
            rm_row(&good_a, 1, 1),
            rm_row(&bad, 2, 1),
            rm_row(&good_b, 3, 1),
        ],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    fail_deferred_of_length(bad.len(), 13);
    let done = remap_with(db, 2, RM_NOW, &wake);
    fail_deferred_of_length(0, 0);
    assert!(
        matches!(done, Err(StoreError::Sql { code: Some(13), .. })),
        "the store's failure did not stop the re-map: {done:?}"
    );
    assert_eq!(ids(db), [1, 2, 3], "a row was mapped or lost");
    assert!(
        outbox(db).is_empty(),
        "a row of a stopped re-map was stated"
    );
    assert_sum_exact(db, "after the stopped re-map");
    // The door off, the next start maps all three.
    let ok = remap_with(db, 2, RM_NOW, &wake).expect("the next re-map");
    assert_eq!((ok.mapped, ok.left), (3, 0));
}

/// TH5b R16 (N-2): ONE page holds a row that fails by its CONTENT (the door's 275) and, after it, one that fails by
/// the STORE (13), then a good row. The page fails at the first; its rows are tried one per zone; the second's
/// code stops the re-map AT ONCE (the per-row guard, the TH5a re-check's mutant m8): `Err(Sql { code: Some(13) })`,
/// the 275 row left, the 13 row left, the good row after them not mapped either, nothing stated, nothing lost.
#[test]
fn a_store_failure_among_the_rows_of_a_failed_page_stops_the_remap_and_loses_nothing() {
    use crate::support::mock::envelope::Msg;
    use kourier_threema::store::{fail_deferred_of_lengths, remap_with};
    let store = linked_store();
    let db = &store.db;
    let content = rm_plaintext(
        RM_PEER,
        Msg::text([1; 8], RM_AT, "a text whose zone fails by its content"),
    );
    let full = rm_plaintext(RM_PEER, Msg::text([2; 8], RM_AT, "the store fails here"));
    let good = rm_text(RM_PEER, 3);
    assert!(
        content.len() != full.len() && content.len() != good.len() && full.len() != good.len(),
        "the door tells the rows apart by their length"
    );
    add(
        db,
        &[
            rm_row(&content, 1, 1),
            rm_row(&full, 2, 1),
            rm_row(&good, 3, 1),
        ],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    fail_deferred_of_lengths(&[(content.len(), 275), (full.len(), 13)]);
    let done = remap_with(db, 2, RM_NOW, &wake);
    fail_deferred_of_lengths(&[]);
    assert!(
        matches!(done, Err(StoreError::Sql { code: Some(13), .. })),
        "the store's failure in the one-row retry did not stop the re-map: {done:?}"
    );
    assert_eq!(
        kinds(db),
        [
            (1, "envelope".to_owned(), 1),
            (2, "envelope".to_owned(), 1),
            (3, "envelope".to_owned(), 1),
        ],
        "a row was mapped, deferred again or lost"
    );
    assert!(
        outbox(db).is_empty(),
        "a row of a stopped re-map was stated"
    );
    assert_sum_exact(db, "after the stopped re-map");
    // The door off, the next start maps all three.
    let ok = remap_with(db, 2, RM_NOW, &wake).expect("the next re-map");
    assert_eq!((ok.mapped, ok.left), (3, 0));
}

/// TH5b R16: the door's pairs fail each of their lengths (here in the re-map's zones, the content's code), a call
/// REPLACES the pairs of the one before (the one-pair door is the same door), and an empty slice turns it off.
#[test]
fn the_door_of_two_pairs_fails_both_lengths_and_a_call_replaces_the_pairs() {
    use crate::support::mock::envelope::Msg;
    use kourier_threema::store::{
        Remapped, fail_deferred_of_length, fail_deferred_of_lengths, remap_with,
    };
    let store = linked_store();
    let db = &store.db;
    let a = rm_text(RM_PEER, 1);
    let b = rm_plaintext(
        RM_PEER,
        Msg::text([2; 8], RM_AT, "a text of another length"),
    );
    let c = rm_plaintext(
        RM_PEER,
        Msg::text([3; 8], RM_AT, "and a third length, longer"),
    );
    assert!(a.len() != b.len() && b.len() != c.len() && a.len() != c.len());
    add(
        db,
        &[rm_row(&a, 1, 1), rm_row(&b, 2, 1), rm_row(&c, 3, 1)],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let left = |done: Remapped| (done.mapped, done.left);
    fail_deferred_of_lengths(&[(a.len(), 275), (b.len(), 275)]);
    fail_deferred_of_lengths(&[(b.len(), 275), (c.len(), 275)]);
    let first = remap_with(db, 2, RM_NOW, &wake).expect("a content failure");
    assert_eq!(
        left(first),
        (1, 2),
        "the second call did not replace the first's pairs"
    );
    assert_eq!(
        ids(db),
        [2, 3],
        "the row of the replaced pair was not mapped"
    );
    fail_deferred_of_length(c.len(), 275);
    let second = remap_with(db, 2, RM_NOW, &wake).expect("a content failure");
    assert_eq!(
        left(second),
        (1, 1),
        "the one-pair door did not replace both pairs"
    );
    assert_eq!(ids(db), [3]);
    fail_deferred_of_lengths(&[]);
    let third = remap_with(db, 2, RM_NOW, &wake).expect("the door off");
    fail_deferred_of_length(0, 0);
    assert_eq!(
        left(third),
        (1, 0),
        "the empty slice did not turn the door off"
    );
    assert!(ids(db).is_empty());
    assert_sum_exact(db, "after the door's re-maps");
}

/// TH5b R1, THE CLEAR at the store (`store::clear_deferred`, the zone of the actor's `ClearConversation` arm):
/// 70 rows over three pages, every other one a vote of the cleared chat (of both versions and two arrival
/// times: WHATEVER its time), the rest votes of another chat, and a `csp` row of the cleared chat (TH6's source, the
/// leader's `IncomingMessage` in the same form: TH6 decided it goes too). One zone deletes the 36 rows of the chat on
/// every page and writes ONE `core_outbox` row with the command's `CommandPersisted`; the kept sum stays exact. A second clear of the same chat deletes
/// nothing and confirms its own command.
#[test]
fn a_clear_deletes_the_rows_of_its_chat_on_every_page_and_confirms_in_the_same_zone() {
    use crate::support::mock::body::{filler, ty};
    use crate::support::mock::envelope::Msg;
    use kourier_backend_api::BackendEvent;
    use kourier_model::CmdId;
    use kourier_threema::store::clear_deferred;
    let store = linked_store();
    let db = &store.db;
    let vote = |sender: &str, n: u8| {
        rm_plaintext(
            sender,
            Msg::raw([n; 8], RM_AT, ty::POLL_VOTE, filler(20 + usize::from(n), n)),
        )
    };
    let envelopes: Vec<Vec<u8>> = (1..=70u8)
        .map(|n| vote(if n % 2 == 1 { RM_PEER } else { RM_HIDDEN }, n))
        .collect();
    for (n, envelope) in (1u8..).zip(&envelopes) {
        let at = if n <= 35 { T0 } else { T0 + 1 };
        add(
            db,
            &[rm_row(envelope, n, 1 + u32::from(n % 3 == 0))],
            at,
            &WIDE,
        );
    }
    let csp = vote(RM_PEER, 71);
    add(
        db,
        &[NewDeferred {
            source: DeferredSource::Csp,
            ..rm_row(&csp, 71, 2)
        }],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let deleted = clear_deferred(db, &direct(RM_PEER), None, &[], CmdId(41), RM_NOW, &wake)
        .expect("the clear");
    assert_eq!(
        deleted, 36,
        "not every row of the chat on every page, the csp row too"
    );
    let kept: Vec<i64> = (2..=70).step_by(2).collect();
    assert_eq!(ids(db), kept, "only the other chat's rows are kept");
    assert_eq!(
        outbox(db),
        [vec![BackendEvent::CommandPersisted { cmd: CmdId(41) }]],
        "not ONE core_outbox row with the command's confirmation"
    );
    assert_sum_exact(db, "after the clear");
    let again = clear_deferred(db, &direct(RM_PEER), None, &[], CmdId(42), RM_NOW, &wake)
        .expect("the second clear");
    assert_eq!(again, 0, "a second clear found rows");
    assert_eq!(ids(db), kept);
    assert_eq!(
        outbox(db),
        [
            vec![BackendEvent::CommandPersisted { cmd: CmdId(41) }],
            vec![BackendEvent::CommandPersisted { cmd: CmdId(42) }],
        ],
        "the second clear is not confirmed on its own"
    );
}

/// TH6: a `csp` row (the leader's `IncomingMessage`, the same envelope form as a reflection) is mapped by the re-map like a
/// `reflected` one: deleted in the zone of its ONE history row; nothing is left.
#[test]
fn a_csp_row_is_remapped_like_a_reflected_one() {
    use kourier_backend_api::BackendEvent;
    use kourier_threema::store::remap_with;
    let store = linked_store();
    let db = &store.db;
    let text = rm_text(RM_PEER, 1);
    add(
        db,
        &[NewDeferred {
            source: DeferredSource::Csp,
            ..rm_row(&text, 1, 1)
        }],
        T0,
        &WIDE,
    );
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!((done.zones, done.mapped, done.left), (1, 1, 0));
    assert!(ids(db).is_empty(), "the csp row was left");
    let rows = outbox(db);
    assert!(
        rows.len() == 1
            && matches!(rows[0].as_slice(), [BackendEvent::HistoryBatch { conv, messages, .. }]
                if messages.len() == 1 && *conv == direct(RM_PEER)),
        "the csp row is not ONE history row of its chat"
    );
    assert_sum_exact(db, "after the csp row's re-map");
}

/// TH6 FIX-B, S-L2 (T-L7's duplicate `deferred` rows): after a takeover the phone's reflection of a message that
/// Kourier took as the leader adds a `reflected` row beside the `csp` row of the SAME message (C10 deletes the
/// `pending` row, not the `deferred` one), and the re-map maps both sources. Whatever build maps the kind (a text stands
/// in for it here, as in the test above), the two rows must reach the core as ONE message: each event names the same
/// conversation and the same `local` (the message id), the key the core's ingest drops a second copy by
/// (`MsgIdScope::Conversation`: Threema ids are unique per conversation; the core's idempotence is pinned by
/// `kourier-core/tests/ingest.rs`, `every_script_applied_twice_gives_the_same_state`), and both rows are deleted. The
/// two envelopes are those of two devices (the phone's and Kourier's own) with different server times.
#[test]
fn a_csp_and_a_reflected_row_of_the_same_message_reach_the_core_as_one_message() {
    use crate::support::mock::envelope::{Author, Msg};
    use kourier_backend_api::BackendEvent;
    use kourier_threema::store::remap_with;
    use prost::Message as _;
    let store = linked_store();
    let db = &store.db;
    let msg = Msg::text([9; 8], RM_AT, "remapped text");
    let ours = Author::new(0x0A08).incoming(RM_PEER, &msg).encode_to_vec();
    let phones = Author::new(0x0B09).incoming(RM_PEER, &msg).encode_to_vec();
    assert_ne!(ours, phones, "two devices wrote the same bytes");
    add(
        db,
        &[
            NewDeferred {
                source: DeferredSource::Csp,
                ..rm_row(&ours, 1, 1)
            },
            rm_row(&phones, 2, 1),
        ],
        T0,
        &WIDE,
    );
    assert_eq!(ids(db).len(), 2, "two rows of one message");
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!((done.mapped, done.left), (2, 0), "both rows are mapped");
    assert!(ids(db).is_empty(), "a row was left");
    let mut keys = std::collections::BTreeSet::new();
    let mut copies = 0;
    for event in outbox(db).iter().flatten() {
        let BackendEvent::HistoryBatch { conv, messages, .. } = event else {
            continue;
        };
        for m in messages {
            assert_eq!(*conv, direct(RM_PEER), "another chat");
            assert_eq!(m.conv, *conv, "another chat");
            copies += 1;
            keys.insert((m.conv.clone(), m.local.to_string()));
        }
    }
    assert!(copies >= 1, "the message never reached the core");
    assert_eq!(keys.len(), 1, "two messages for the core: {keys:?}");
    assert_sum_exact(db, "after the two rows' re-map");
}

/// S5 (round A): the re-map checks the actor's cancel token before every page. A token that is already
/// cancelled stops it before the first page (no zone, no row touched, not an error); a live token maps the
/// 61 rows in three zones as before.
#[test]
fn a_cancelled_token_stops_the_remap_before_its_next_page() {
    use kourier_threema::store::{Remapped, remap_with_cancel};
    use tokio_util::sync::CancellationToken;
    let store = linked_store();
    let db = &store.db;
    let texts: Vec<Vec<u8>> = (1..=61u8).map(|n| rm_text(RM_PEER, n)).collect();
    let rows: Vec<NewDeferred<'_>> = texts
        .iter()
        .zip(1u8..)
        .map(|(t, n)| rm_row(t, n, 1))
        .collect();
    add(db, &rows, T0, &WIDE);
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let stopped =
        remap_with_cancel(db, 2, RM_NOW, &wake, &cancel).expect("a cancelled re-map is no error");
    assert_eq!(stopped, Remapped::default(), "a cancelled re-map worked");
    assert_eq!(ids(db).len(), 61, "a cancelled re-map touched a row");
    assert!(outbox(db).is_empty(), "a cancelled re-map stated a row");
    let done = remap_with_cancel(db, 2, RM_NOW, &wake, &CancellationToken::new())
        .expect("the re-map under a live token");
    assert_eq!((done.zones, done.mapped, done.left), (3, 61, 0));
    assert!(ids(db).is_empty(), "the live token left rows");
}

/// S7 (round A): the third trigger, `deferred_total_update`, keeps the byte sum exact when a row's
/// `envelope` is rewritten (grown, shrunk, and rolled back with its zone), and an update of the columns that
/// are not the envelope leaves the sum alone. No code rewrites an envelope today; the first step that does
/// (a re-map that stores a re-encoded one) relies on the frozen trigger.
#[test]
fn the_kept_sum_follows_a_rewritten_envelope_and_ignores_other_columns() {
    let t = temp_store();
    add(&t.db, &[row(&[1; 10]), row(&[2; 20])], T0, &WIDE);
    assert_eq!(
        assert_sum_exact(&t.db, "after the inserts"),
        DeferredTotal { rows: 2, bytes: 30 }
    );
    let run = |sql: &str, n: i64| {
        t.db.zone(|tx| {
            tx.execute(sql, params![n])?;
            Ok::<(), kourier_db::Error>(())
        })
        .expect("the update");
    };
    run(
        "UPDATE deferred SET envelope = zeroblob(?1) WHERE id = 1",
        55,
    );
    assert_eq!(
        assert_sum_exact(&t.db, "after a row grew"),
        DeferredTotal { rows: 2, bytes: 75 }
    );
    run(
        "UPDATE deferred SET envelope = zeroblob(?1) WHERE id = 1",
        3,
    );
    assert_eq!(
        assert_sum_exact(&t.db, "after a row shrank"),
        DeferredTotal { rows: 2, bytes: 23 }
    );
    run(
        "UPDATE deferred SET kind = 'other', map_version = 9 + ?1 WHERE id = 2",
        0,
    );
    run("UPDATE deferred SET flags = 1 WHERE id = ?1", 2);
    assert_eq!(
        assert_sum_exact(&t.db, "after other columns changed"),
        DeferredTotal { rows: 2, bytes: 23 }
    );
    // A rewrite in a zone that fails is rolled back with the sum.
    let failed = t.db.zone(|tx| {
        tx.execute(
            "UPDATE deferred SET envelope = zeroblob(1000) WHERE id = 2",
            [],
        )
        .map_err(|_| StoreError::Failed("the test's own update"))?;
        Err::<(), _>(StoreError::Failed("the test's own failure"))
    });
    assert!(failed.is_err());
    assert_eq!(
        assert_sum_exact(&t.db, "after a rolled-back rewrite"),
        DeferredTotal { rows: 2, bytes: 23 }
    );
}

// ROUND B: THE EVICTION ORDER AND THE EXEMPT KINDS (M-2 = S2), NO BRING-BACK AT A RE-MAP (M-3 = S3) -------

/// A row of `envelope` deferred under `kind`.
fn of_kind<'a>(kind: &'static str, envelope: &'a [u8]) -> NewDeferred<'a> {
    NewDeferred {
        kind,
        ..row(envelope)
    }
}

/// `deferred`'s rows as (id, kind).
fn id_kinds(db: &ZoneDb) -> Vec<(i64, String)> {
    kinds(db)
        .into_iter()
        .map(|(id, kind, _)| (id, kind))
        .collect()
}

/// M-2 (a): the SYNC kinds (`settings`, `profile`, `list-sync`) survive a full table and 46 days. A table of
/// N = 4 rows holding the three sync rows (the OLDEST ids) and one `file`: every insert past N takes a row
/// that is not a sync row, at the insert and at the start's age bound; when only sync rows are left over a
/// bound they stay (the bound is over, the kept sum exact). 46 days later the age bound (at an insert and at a
/// start) takes every row but the sync rows.
#[test]
fn the_sync_kinds_survive_a_full_table_and_46_days() {
    let t = temp_store();
    let bounds = DeferredBounds {
        rows: 4,
        ..DeferredBounds::REAL
    };
    let e = [0x5a_u8; 8];
    add(
        &t.db,
        &[
            of_kind("settings", &e),
            of_kind("profile", &e),
            of_kind("list-sync", &e),
            of_kind("file", &e),
        ],
        T0,
        &bounds,
    );
    // N+1 and N+2: the file (id 4), then the edit (id 5) go; never a sync row.
    assert_eq!(add(&t.db, &[of_kind("edit", &e)], T0, &bounds).full, 1);
    assert_eq!(add(&t.db, &[of_kind("delete", &e)], T0, &bounds).full, 1);
    assert_eq!(
        id_kinds(&t.db),
        [
            (1, "settings".to_owned()),
            (2, "profile".to_owned()),
            (3, "list-sync".to_owned()),
            (6, "delete".to_owned()),
        ],
        "a full table evicted a sync row"
    );
    // A bound of 2 and one more sync row: the delete goes, the four sync rows stay over the bound.
    let small = DeferredBounds { rows: 2, ..bounds };
    assert_eq!(add(&t.db, &[of_kind("settings", &e)], T0, &small).full, 1);
    assert_eq!(
        id_kinds(&t.db)
            .iter()
            .map(|(_, k)| k.as_str())
            .collect::<Vec<_>>(),
        ["settings", "profile", "list-sync", "settings"],
        "the bound took a sync row, or kept the delete"
    );
    assert_sum_exact(&t.db, "after the bounds");
    // 46 days later: a file deferred now takes the age bound with it, and the start's age bound takes the
    // rest but the sync rows.
    let later = T0 + 46 * 24 * 60 * 60 * 1000;
    add(&t.db, &[of_kind("file", &e)], T0 + 1, &DeferredBounds::REAL);
    let d = add(
        &t.db,
        &[of_kind("receipt", &e)],
        later,
        &DeferredBounds::REAL,
    );
    assert_eq!(
        d.expired, 1,
        "the 46-day-old file did not expire at the insert"
    );
    assert_eq!(
        expire_deferred(
            &t.db,
            later + 46 * 24 * 60 * 60 * 1000,
            &DeferredBounds::REAL
        )
        .expect("expire"),
        1,
        "the start's age bound did not take exactly the receipt"
    );
    assert_eq!(
        id_kinds(&t.db)
            .iter()
            .map(|(_, k)| k.as_str())
            .collect::<Vec<_>>(),
        ["settings", "profile", "list-sync", "settings"],
        "a sync row expired"
    );
    assert_sum_exact(&t.db, "after the age bound");
}

/// M-2 (c): at N+1 the bounds take the LOW-VALUE kinds first (receipts, read / sent updates, calls), the
/// oldest of them first, whatever their place among the rest; only when none is left the oldest of the rest.
/// The byte bound follows the same order.
#[test]
fn the_low_value_kinds_go_first_at_n_plus_1() {
    let t = temp_store();
    let bounds = DeferredBounds { rows: 5, ..WIDE };
    let e = [0x5a_u8; 8];
    add(
        &t.db,
        &[
            of_kind("file", &e),
            of_kind("edit", &e),
            of_kind("read", &e),
            of_kind("call", &e),
            of_kind("receipt", &e),
        ],
        T0,
        &bounds,
    );
    let next = |kind: &'static str| add(&t.db, &[of_kind(kind, &e)], T0, &bounds).full;
    // The oldest low-value row is the read (id 3), not the file (id 1).
    assert_eq!(next("delete"), 1);
    assert_eq!(
        ids(&t.db),
        [1, 2, 4, 5, 6],
        "not the oldest low-value row first"
    );
    // Then each time the oldest low-value row: the call (4), the receipt (5), the group call (8), the sent
    // update (9).
    assert_eq!(next("file"), 1);
    assert_eq!(ids(&t.db), [1, 2, 5, 6, 7]);
    assert_eq!(next("group-call"), 1);
    assert_eq!(ids(&t.db), [1, 2, 6, 7, 8], "the receipt stayed");
    assert_eq!(next("sent"), 1);
    assert_eq!(ids(&t.db), [1, 2, 6, 7, 9], "the group call stayed");
    assert_eq!(next("edit"), 1);
    assert_eq!(ids(&t.db), [1, 2, 6, 7, 10], "the new sent update stayed");
    // No low-value row left: the oldest of the rest (the file, id 1).
    assert_eq!(next("delete"), 1);
    assert_eq!(
        ids(&t.db),
        [2, 6, 7, 10, 11],
        "not the oldest row once no low-value row is left"
    );
    // The byte bound: 5 rows of 8 bytes; a bound of 40 bytes and one more row of 8 takes a low-value row.
    let by_bytes = DeferredBounds { bytes: 40, ..WIDE };
    add(&t.db, &[of_kind("sent", &e)], T0, &WIDE);
    assert_eq!(add(&t.db, &[of_kind("file", &e)], T0, &by_bytes).full, 2);
    assert_eq!(
        id_kinds(&t.db),
        [
            (6, "delete".to_owned()),
            (7, "file".to_owned()),
            (10, "edit".to_owned()),
            (11, "delete".to_owned()),
            (13, "file".to_owned()),
        ],
        "the byte bound did not take the sent update first, then the oldest"
    );
    assert_sum_exact(&t.db, "after the eviction order");
}

/// M-2: the eviction order's look-up is one seek on `deferred_by_kind` (the 0003 index), not a scan.
#[test]
fn the_oldest_row_of_a_kind_is_one_seek_on_the_kind_index() {
    let t = temp_store();
    let plan: Vec<String> = t
        .db
        .read(|conn: &Connection| {
            let mut stmt = conn
                .prepare(
                    "EXPLAIN QUERY PLAN SELECT id FROM deferred WHERE kind = ?1 ORDER BY id LIMIT 1",
                )
                .map_err(kourier_db::Error::from)?;
            stmt.query_map(["receipt"], |r| r.get::<_, String>(3))
                .map_err(kourier_db::Error::from)?
                .collect::<Result<Vec<String>, _>>()
                .map_err(kourier_db::Error::from)
        })
        .expect("the plan");
    assert!(
        plan.len() == 1 && plan[0].contains("USING COVERING INDEX deferred_by_kind (kind=?)"),
        "not one seek on the kind index: {plan:?}"
    );
}

/// M-3 = S3: a deferred text of a chat the person ARCHIVED, re-mapped by a later build: its history row only,
/// no `archived: Some(false)` stated, and the stored record stays ARCHIVED (H5's bring-back runs at arrival
/// only, never at a re-map).
#[test]
fn a_remapped_message_of_an_archived_chat_brings_nothing_back() {
    use crate::support::mock::envelope::ContactDelta;
    use kourier_backend_api::BackendEvent;
    use kourier_threema::store::remap_with;
    use libthreema::protobuf::d2d_sync;
    use prost::Message as _;
    let store = linked_store();
    let db = &store.db;
    let archived = ContactDelta::create(RM_PEER, [7; 32], RM_AT)
        .archived()
        .build();
    db.zone(|tx| {
        tx.execute(
            "UPDATE contact SET sync = ?2 WHERE identity = ?1",
            params![RM_PEER, archived.encode_to_vec()],
        )?;
        Ok::<(), kourier_db::Error>(())
    })
    .expect("archive the chat");
    let text = rm_text(RM_PEER, 1);
    add(db, &[rm_row(&text, 1, 1)], T0, &WIDE);
    let wake = std::rc::Rc::new(tokio::sync::Notify::new());
    let done = remap_with(db, 2, RM_NOW, &wake).expect("the re-map");
    assert_eq!(done.mapped, 1);
    let rows = outbox(db);
    assert!(
        !rows
            .iter()
            .flatten()
            .any(|e| matches!(e, BackendEvent::ConversationUpsert(p) if p.archived == Some(false))),
        "a re-map brought the archived chat back"
    );
    match rows.as_slice() {
        [row] => match row.as_slice() {
            [batch] => assert_history_rows(batch, &direct(RM_PEER), &[1]),
            _ => panic!("the re-mapped text is not its history row alone"),
        },
        _ => panic!("not one core_outbox row"),
    }
    let stored: Vec<u8> = db
        .read(|conn: &Connection| {
            conn.query_row(
                "SELECT sync FROM contact WHERE identity = ?1",
                [RM_PEER],
                |r| r.get(0),
            )
            .map_err(kourier_db::Error::from)
        })
        .expect("the record");
    let stored = d2d_sync::Contact::decode(stored.as_slice()).expect("the record decodes");
    assert_eq!(
        stored.conversation_visibility,
        Some(d2d_sync::ConversationVisibility::Archived as i32),
        "the re-map wrote the record NORMAL"
    );
}
