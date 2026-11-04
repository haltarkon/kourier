//! The stage file: migrated and never removed for a version, a foreign owner counted lost, opened when
//! first needed, its key derived.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use kourier_db::rusqlite::Connection;
use kourier_db::{DbKey, Migration, Migrations, Priority, Step, migration};

use super::{LazyStage, MIGRATIONS, Origin, StageOwner, derive_key};
use crate::error::OpenError;

const BACKEND_KEY: [u8; 32] = [0x11; 32];

/// The stage layout of a LATER build: the first step and one more.
const LATER: Migrations = Migrations::new(
    "whatsapp-history-stage",
    &[
        migration!(1, "stage/0001_stage"),
        Migration {
            version: 2,
            name: "0002_later",
            step: Step::Sql("ALTER TABLE chunks ADD COLUMN later TEXT;"),
        },
    ],
);

fn key() -> DbKey {
    derive_key(&DbKey::from(BACKEND_KEY))
}

fn path_in(dir: &Path) -> PathBuf {
    dir.join("history").join("stage.db")
}

/// Puts one chunk in the state `state` straight into the file, as the capture and the feeder will.
fn insert(stage: &StageOwner, msg_id: &str, state: &str) {
    let (id, state) = (msg_id.to_owned(), state.to_owned());
    stage
        .handle()
        .write_blocking(Priority::High, "test insert", None, move |tx| {
            tx.execute(
                "INSERT INTO chunks (msg_id, captured_at, size, state) VALUES (?1, 1, 3, ?2)",
                (id, state),
            )
            .map_err(crate::error::sql("test insert"))?;
            tx.execute(
                "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, x'010203')",
                [tx.last_insert_rowid()],
            )
            .map_err(crate::error::sql("test insert"))?;
            Ok(())
        })
        .expect("insert");
}

fn count(path: &Path, key: &DbKey, sql: &str) -> i64 {
    let conn: Connection = kourier_db::open(path, key).expect("open for a look");
    conn.query_row(sql, [], |r| r.get(0)).expect("count")
}

#[test]
fn a_stage_is_migrated_never_removed_for_a_version() {
    let dir = tempfile::tempdir().expect("dir");
    let path = path_in(dir.path());

    // The first build's stage, with a chunk the phone was told had arrived.
    let (stage, origin) = StageOwner::open(&path, &key(), "me", &MIGRATIONS).expect("v1");
    assert_eq!(origin, Origin::Created);
    insert(&stage, "chunk-one", "captured");
    stage.close();

    // A later build with one more step: the stage is brought forward and the chunk is still there.
    let (stage, origin) = StageOwner::open(&path, &key(), "me", &LATER).expect("v2");
    assert_eq!(
        origin,
        Origin::Resumed,
        "a stage of ours is used, not replaced"
    );
    stage.close();
    assert_eq!(count(&path, &key(), "SELECT count(*) FROM chunks"), 1);
    assert_eq!(
        count(&path, &key(), "SELECT count(later IS NULL) FROM chunks"),
        1,
        "the new column is there"
    );

    // The first build again (a downgrade): it refuses the newer file and leaves it as it is.
    let refused = StageOwner::open(&path, &key(), "me", &MIGRATIONS).err();
    assert!(
        matches!(refused, Some(OpenError::TooNew { found: 2, .. })),
        "a newer stage is refused, got {refused:?}"
    );
    assert_eq!(
        count(&path, &key(), "SELECT count(*) FROM chunks"),
        1,
        "and its chunk is still there"
    );
}

#[test]
fn a_foreign_owner_counts_its_captured_chunks_lost() {
    let dir = tempfile::tempdir().expect("dir");
    let path = path_in(dir.path());
    let (stage, _) = StageOwner::open(&path, &key(), "first owner", &MIGRATIONS).expect("open");
    for (id, state) in [
        ("a", "captured"),
        ("b", "captured"),
        ("c", "planned"),
        ("d", "bad"),
        ("e", "done"),
    ] {
        insert(&stage, id, state);
    }
    stage.close();

    // The same file for another owner: replaced, and what it had not delivered is counted.
    let (stage, origin) =
        StageOwner::open(&path, &key(), "second owner", &MIGRATIONS).expect("reopen");
    assert_eq!(
        origin,
        Origin::Foreign { lost: 3 },
        "two captured and one planned chunk were not delivered; a bad one was counted when it went bad and a done one arrived"
    );
    stage.close();
    assert_eq!(count(&path, &key(), "SELECT count(*) FROM chunks"), 0);

    // The new stage is the second owner's: the next open resumes it.
    let (stage, origin) =
        StageOwner::open(&path, &key(), "second owner", &MIGRATIONS).expect("again");
    assert_eq!(origin, Origin::Resumed);
    stage.close();
}

/// MEDIUM-3: a stage our key does not open (in practice a damaged file) is NEVER deleted: its files are renamed aside,
/// a new stage starts, and the old bytes are still on disk under the aside name.
/// Mutant for the re-check: `set_aside` replaced by `remove_files` for `Unreadable` in `StageOwner::open`.
#[test]
fn a_stage_our_key_does_not_open_is_set_aside_not_deleted_and_says_so() {
    let dir = tempfile::tempdir().expect("dir");
    let path = path_in(dir.path());
    let other = derive_key(&DbKey::from([0x22; 32]));
    let (stage, _) = StageOwner::open(&path, &other, "me", &MIGRATIONS).expect("open");
    insert(&stage, "chunk", "captured");
    stage.close();
    let before = std::fs::read(&path).expect("the file");

    let (stage, origin) = StageOwner::open(&path, &key(), "me", &MIGRATIONS).expect("replace");
    assert_eq!(origin, Origin::Unreadable);
    stage.close();
    assert_eq!(count(&path, &key(), "SELECT count(*) FROM chunks"), 0);
    // The old file is beside it under an aside name, byte for byte, and still opens with its own key.
    let aside: Vec<PathBuf> = std::fs::read_dir(path.parent().expect("dir"))
        .expect("listing")
        .map(|e| e.expect("entry").path())
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                n.starts_with("stage.db.unreadable-")
                    && !n.ends_with("-wal")
                    && !n.ends_with("-shm")
            })
        })
        .collect();
    assert_eq!(aside.len(), 1, "one aside copy: {aside:?}");
    assert_eq!(std::fs::read(&aside[0]).expect("aside"), before);
    assert_eq!(count(&aside[0], &other, "SELECT count(*) FROM chunks"), 1);
}

/// HIGH-1: every file gets an incarnation id when it is made, a resumed file keeps it, and a replaced file has another.
/// Mutant for the re-check: `randomblob(16)` replaced by a constant in `open_checked`.
#[test]
fn a_file_has_an_incarnation_made_with_it_and_a_replacement_has_another() {
    let dir = tempfile::tempdir().expect("dir");
    let path = path_in(dir.path());
    let (stage, _) = StageOwner::open(&path, &key(), "first owner", &MIGRATIONS).expect("open");
    let first = stage.incarnation();
    assert_ne!(first, [0; 16]);
    stage.close();
    let (stage, origin) =
        StageOwner::open(&path, &key(), "first owner", &MIGRATIONS).expect("again");
    assert_eq!(origin, Origin::Resumed);
    assert_eq!(stage.incarnation(), first, "a resumed file keeps its id");
    stage.close();
    let (stage, origin) =
        StageOwner::open(&path, &key(), "second owner", &MIGRATIONS).expect("replace");
    assert!(matches!(origin, Origin::Foreign { .. }));
    assert_ne!(stage.incarnation(), first, "a replacement has another id");
    stage.close();
}

/// MEDIUM-1: the bytes of a chunk are not in the row every look at a chunk's state reads. A state change writes
/// the small row only: the WAL does not grow by the blob's size, and the scanned table stays a few pages.
/// Mutant for the re-check: the `bytes` column put back in `chunks` (the `0001_stage.sql` layout of the review).
#[test]
fn a_state_change_does_not_rewrite_the_bytes_of_the_chunk() {
    let dir = tempfile::tempdir().expect("dir");
    let path = path_in(dir.path());
    let (stage, _) = StageOwner::open(&path, &key(), "me", &MIGRATIONS).expect("open");
    let big = vec![0xA5_u8; 512 * 1024];
    for n in 0..4 {
        let (id, bytes) = (format!("chunk-{n}"), big.clone());
        stage
            .handle()
            .write_blocking(Priority::High, "test big", None, move |tx| {
                tx.execute(
                    "INSERT INTO chunks (msg_id, captured_at, size, state) VALUES (?1, 1, ?2, 'captured')",
                    (id, bytes.len() as i64),
                )
                .map_err(crate::error::sql("test big"))?;
                tx.execute(
                    "INSERT INTO chunk_bytes (seq, bytes) VALUES (?1, ?2)",
                    (tx.last_insert_rowid(), bytes),
                )
                .map_err(crate::error::sql("test big"))?;
                Ok(())
            })
            .expect("a big chunk");
    }
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    let wal_len = || std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
    let before = wal_len();
    assert!(
        before > 2 * 1024 * 1024,
        "the WAL holds the four blobs: {before}"
    );
    for n in 0..4 {
        let id = format!("chunk-{n}");
        stage
            .handle()
            .write_blocking(Priority::High, "test state", None, move |tx| {
                tx.execute(
                    "UPDATE chunks SET state = 'planned' WHERE msg_id = ?1",
                    [id],
                )
                .map(drop)
                .map_err(crate::error::sql("test state"))
            })
            .expect("a state change");
    }
    let grown = wal_len().saturating_sub(before);
    assert!(
        grown < 128 * 1024,
        "four state changes grew the WAL by {grown} bytes: the blob was rewritten"
    );
    // The table every look at a chunk's state scans is a page or two: the blobs are in `chunk_bytes`.
    let conn = stage.handle().reader().expect("reader");
    let pages = |table: &str| -> Option<i64> {
        conn.query_row(
            "SELECT count(*) FROM dbstat WHERE name = ?1",
            [table],
            |r| r.get(0),
        )
        .ok()
    };
    if let (Some(scanned), Some(bytes)) = (pages("chunks"), pages("chunk_bytes")) {
        assert!(scanned <= 2, "the scanned table is {scanned} pages");
        assert!(bytes > 400, "the blobs are {bytes} pages of their own");
    } else {
        eprintln!(
            "(this SQLite has no dbstat: the page count of the scanned table was not checked)"
        );
    }
}

#[test]
fn the_key_is_derived_from_the_backend_key_and_stored_nowhere() {
    let backend = DbKey::from(BACKEND_KEY);
    let stage = derive_key(&backend);
    assert_ne!(
        stage.as_bytes(),
        backend.as_bytes(),
        "not the backend key itself"
    );
    assert_eq!(
        stage.as_bytes(),
        derive_key(&backend).as_bytes(),
        "the same every time"
    );
    assert_ne!(
        stage.as_bytes(),
        derive_key(&DbKey::from([0x12; 32])).as_bytes(),
        "another account, another stage key"
    );
    // A stage made with the derived key does not open with the backend key.
    let dir = tempfile::tempdir().expect("dir");
    let path = path_in(dir.path());
    let (owner, _) = StageOwner::open(&path, &stage, "me", &MIGRATIONS).expect("open");
    owner.close();
    assert!(matches!(
        kourier_db::open(&path, &backend),
        Err(kourier_db::Error::WrongKey)
    ));
}

#[test]
fn a_stage_is_made_when_first_needed_and_removed_on_purpose() {
    let dir = tempfile::tempdir().expect("dir");
    let backend_db = dir.path().join("backend.db");
    let lazy = LazyStage::new(&backend_db, &DbKey::from(BACKEND_KEY));
    assert!(!lazy.on_disk(), "nothing exists before the first need");
    assert!(
        !dir.path().join("history").exists(),
        "not even the directory"
    );
    assert!(lazy.take_origin().is_none());

    lazy.handle("me").expect("the first need opens it");
    assert!(lazy.on_disk());
    assert_eq!(lazy.take_origin(), Some(Origin::Created));
    assert!(lazy.take_origin().is_none(), "told once");
    let mode = std::fs::metadata(dir.path().join("history"))
        .expect("directory")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700, "only the user reaches the stage");
    lazy.handle("me")
        .expect("the second need uses the open one");

    // A start with the file on disk resumes it, with the same owner.
    lazy.close();
    assert!(lazy.handle("me").is_err(), "closed for good");
    let again = LazyStage::new(&backend_db, &DbKey::from(BACKEND_KEY));
    assert!(
        again.on_disk(),
        "a stage on disk is found at the next start"
    );
    again.handle("me").expect("resumed");
    assert_eq!(again.take_origin(), Some(Origin::Resumed));

    // A logout removes the directory with everything in it.
    again.remove();
    assert!(!dir.path().join("history").exists());
    assert!(again.handle("me").is_err());
}
