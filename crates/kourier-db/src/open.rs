//! Opening SQLCipher files with a raw key and the pragmas of §5.3.

use crate::{DbKey, Error, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension};
use std::path::Path;
use std::time::Duration;

/// `busy_timeout` for every connection: the writer is the only writer, so waits are short.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5000);

/// Opens (creating if missing) a read-write SQLCipher database with `key`.
///
/// Sets `journal_mode = WAL`, `synchronous = FULL` (a commit survives power loss before we ack a
/// server or complete an `Ack`), `foreign_keys = ON`, `busy_timeout = 5000`,
/// `cache_size = -8000`, `temp_store = MEMORY` and `secure_delete = ON`. A wrong key, or a file
/// that is not a SQLCipher database, fails with [`Error::WrongKey`] before anything is written.
pub fn open(path: &Path, key: &DbKey) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    unlock(&conn, key)?;
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(Error::JournalMode(mode));
    }
    conn.execute_batch("PRAGMA synchronous = FULL")?;
    common_pragmas(&conn)?;
    Ok(conn)
}

/// Opens an existing database read-only (UI query and media connections, §5.4). The same
/// pragmas apply where they mean anything to a reader, plus `query_only = ON`.
pub fn open_read_only(path: &Path, key: &DbKey) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)?;
    unlock(&conn, key)?;
    common_pragmas(&conn)?;
    conn.execute_batch("PRAGMA query_only = ON")?;
    Ok(conn)
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`: after removals and bulk deletes, so deleted keys and rows
/// leave the WAL file too (§5.2).
pub fn checkpoint_truncate(conn: &Connection) -> Result<()> {
    try_checkpoint_truncate(conn).map(drop)
}

/// [`checkpoint_truncate`] that says whether it ran to its end: `true` when every frame of the
/// WAL is in the database file and the WAL file is empty, `false` when a reader (another
/// connection with an open snapshot) held it back after the connection's `busy_timeout`. A
/// caller that needs a secret gone from the WAL asks again while this is `false`.
///
/// Must not be called inside a transaction of `conn` (SQLite answers `SQLITE_LOCKED`).
pub fn try_checkpoint_truncate(conn: &Connection) -> Result<bool> {
    // Returns (busy, log frames, checkpointed frames); busy = 1 means a reader held it back.
    let busy: i64 = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    if busy != 0 {
        tracing::debug!("wal_checkpoint(TRUNCATE) was blocked by a reader");
    }
    Ok(busy == 0)
}

/// Keys the connection and proves the key is right.
fn unlock(conn: &Connection, key: &DbKey) -> Result<()> {
    // Plain SQLite answers an unknown pragma with no rows: refuse to run unencrypted.
    let cipher: Option<String> = conn
        .query_row("PRAGMA cipher_version", [], |r| r.get(0))
        .optional()?;
    if cipher.is_none_or(|v| v.is_empty()) {
        return Err(Error::NoCipher);
    }
    // SQLCipher's own log goes to stderr (journald), unfiltered; failures surface as errors here.
    conn.execute_batch("PRAGMA cipher_log_level = NONE")?;
    {
        // Step it by hand: some SQLCipher versions answer `PRAGMA key` with an "ok" row.
        let sql = key.pragma();
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
    }
    // SQLCipher decrypts page 1 lazily; the first schema read tells a wrong key apart.
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    }) {
        Ok(_) => Ok(()),
        Err(e) if e.sqlite_error_code() == Some(ErrorCode::NotADatabase) => Err(Error::WrongKey),
        Err(e) => Err(e.into()),
    }
}

fn common_pragmas(conn: &Connection) -> Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA cache_size = -8000;
         PRAGMA temp_store = MEMORY;
         PRAGMA secure_delete = ON;",
    )?;
    Ok(())
}

/// Frees what `conn` keeps only for speed: its page cache (`sqlite3_db_release_memory`, the
/// same as `PRAGMA shrink_memory`) and its cached prepared statements. Nothing durable is
/// touched and the connection answers as before; the next reads fill the cache again. A failure
/// is logged at debug level (no path, no name).
pub fn release_memory(conn: &Connection) {
    conn.flush_prepared_statement_cache();
    if let Err(e) = conn.release_memory() {
        tracing::debug!(error = %e, "releasing a connection's memory failed");
    }
}

/// `SQLITE_DBSTATUS_CACHE_USED` of `conn` in bytes: what its page caches hold now. For the tests
/// of [`release_memory`] and of the backends' dispatch to it; nothing in a build reads it.
#[doc(hidden)]
pub fn cache_used(conn: &Connection) -> i64 {
    let (mut current, mut high) = (0_i32, 0_i32);
    // SAFETY: `handle()` is the open connection `conn` borrows for this call, and the two
    // out-pointers are live locals; the call only reads counters.
    let rc = unsafe {
        libsqlite3_sys::sqlite3_db_status(
            conn.handle(),
            libsqlite3_sys::SQLITE_DBSTATUS_CACHE_USED,
            &raw mut current,
            &raw mut high,
            0,
        )
    };
    assert_eq!(rc, libsqlite3_sys::SQLITE_OK, "sqlite3_db_status");
    i64::from(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_gives_the_page_cache_back_and_the_file_still_answers() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("t.db"), &DbKey::from([5; 32])).unwrap();
        conn.execute_batch("CREATE TABLE t(x BLOB);").unwrap();
        for _ in 0..200 {
            conn.execute("INSERT INTO t VALUES (zeroblob(4000))", [])
                .unwrap();
        }
        let n: i64 = conn
            .query_row("SELECT sum(length(x)) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 800_000);
        let full = cache_used(&conn);
        assert!(full > 100_000, "the reads filled the cache: {full}");
        release_memory(&conn);
        let after = cache_used(&conn);
        assert!(after < full / 4, "the cache went back: {full} -> {after}");
        let n: i64 = conn
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 200, "it still answers");
    }

    fn key(b: u8) -> DbKey {
        DbKey::from([b; 32])
    }

    fn pragma<T: rusqlite::types::FromSql>(conn: &Connection, name: &str) -> T {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn create_reopen_and_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        {
            let conn = open(&path, &key(1)).unwrap();
            conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (42);")
                .unwrap();
            assert_eq!(pragma::<String>(&conn, "journal_mode"), "wal");
            assert_eq!(pragma::<i64>(&conn, "synchronous"), 2, "FULL");
            assert_eq!(pragma::<i64>(&conn, "foreign_keys"), 1);
            assert_eq!(pragma::<i64>(&conn, "busy_timeout"), 5000);
            assert_eq!(pragma::<i64>(&conn, "cache_size"), -8000);
            assert_eq!(pragma::<i64>(&conn, "temp_store"), 2, "MEMORY");
            assert_eq!(pragma::<i64>(&conn, "secure_delete"), 1);
            // The RustCrypto provider, never OpenSSL next to BoringSSL (§5.3).
            assert_eq!(pragma::<String>(&conn, "cipher_provider"), "RustCrypto");
        }
        let conn = open(&path, &key(1)).unwrap();
        let x: i64 = conn.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(x, 42);
    }

    #[test]
    fn wrong_key_fails_cleanly_and_file_has_no_sqlite_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        {
            let conn = open(&path, &key(1)).unwrap();
            conn.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES ('secret');")
                .unwrap();
            checkpoint_truncate(&conn).unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.len() >= 4096, "page 1 was written");
        assert_ne!(&bytes[..16], b"SQLite format 3\0");
        assert!(!bytes.windows(6).any(|w| w == b"secret"));

        let before = std::fs::read(&path).unwrap();
        assert!(matches!(open(&path, &key(2)), Err(Error::WrongKey)));
        assert!(matches!(
            open_read_only(&path, &key(2)),
            Err(Error::WrongKey)
        ));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "a failed open writes nothing"
        );
        // The right key still works afterwards.
        open(&path, &key(1)).unwrap();
    }

    #[test]
    fn plaintext_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain.db");
        // No key: SQLCipher writes an ordinary SQLite file (the header the test above rules out).
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t(x)")
            .unwrap();
        assert_eq!(&std::fs::read(&path).unwrap()[..16], b"SQLite format 3\0");
        assert!(matches!(open(&path, &key(1)), Err(Error::WrongKey)));
    }

    #[test]
    fn read_only_connection_reads_but_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let writer = open(&path, &key(3)).unwrap();
        writer
            .execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (1);")
            .unwrap();
        let reader = open_read_only(&path, &key(3)).unwrap();
        let n: i64 = reader
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        assert!(reader.execute("INSERT INTO t VALUES (2)", []).is_err());
    }
}
