//! `search.db`: the index in its own SQLCipher file, beside `core.db`, with its own writer (the
//! search task, [`super::index`]) and its own key (`Keyring::search_db_key`), so saving a
//! message never waits for search. It is NEVER needed: every check that fails ends in THE
//! REBUILD ([`rebuild`]): the file and its `-wal`, `-shm` and `-journal` deleted, a new one
//! made, the backfill started. A backup needs no copy of it.
//!
//! What it holds: `messages_fts` (contentless, deletes by rowid, `detail=column`, the `ascii`
//! tokenizer; the rowid is `search::index::rowid_of`), `indexed` (message id -> the rowid it was
//! indexed under: every delete is exact, so `dead` counts only real tombstones and a replay is
//! harmless) and `meta` (numbers: `version`, `norm`, `applied` = the last log seq applied,
//! `floor` and `top` of the backfill, `dead`, `dead_since`, `compacted_for`, `checkpoint_due`),
//! and the core.db `epoch` it was made for in a table of its own.
//!
//! THE OPEN CHECKS ([`open`], on the search task at `UiReady`; the query's reader opens nothing
//! it would have to judge): a file that cannot be opened or decrypted, or is damaged, or was
//! left half made; another [`SEARCH_FILE_VERSION`] or [`super::NORMALIZER_VERSION`]; another
//! core.db's epoch (a restore, a rebuilt or replaced core.db); core.db's log position BELOW
//! `applied` (an older copy of core.db under the same epoch: its next seqs would be skipped).
//! Each is a rebuild, never a migration.
//!
//! CREATING the file ([`create`]): one read snapshot of core.db gives the epoch, the log
//! position `P`, `max(messages.id)` and the compaction ask; the file gets `applied = P`,
//! `floor = top = max id`, `dead = 0`. Log rows above `P` are flushed as usual; the rows at or
//! below `floor` are read by the backfill.

use std::path::{Path, PathBuf};

use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use kourier_db::{DbKey, failpoint};

use crate::error::{CoreError, Result};

use super::{NORMALIZER_VERSION, log};

/// The version of search.db's schema: a change is a rebuild, never a migration.
pub(crate) const SEARCH_FILE_VERSION: i64 = 1;

/// The kill point after the old files were deleted and before the new one is made (the
/// rebuild's kill loop).
pub const FAILPOINT_REBUILD_DELETED: &str = "core_search_rebuild_deleted";
/// The kill point before the commit of a new file's schema and numbers.
pub const FAILPOINT_CREATE_BEFORE_COMMIT: &str = "core_search_create_before_commit";

/// The file's schema. One leaf of FTS5 fits one SQLCipher page: 4096 bytes less the 80
/// reserved give 4016 usable, of which a cell holds at most 3,981 bytes of local payload
/// (storage LOW-1), so `pgsz` stays at 3900, under it with room; 4050 would overflow a leaf
/// into a second page.
pub(super) const SCHEMA: &str = "
CREATE TABLE meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE epoch(value TEXT NOT NULL);
CREATE VIRTUAL TABLE messages_fts USING fts5(words, meta,
  content = '', contentless_delete = 1, tokenize = 'ascii', detail = column);
INSERT INTO messages_fts(messages_fts, rank) VALUES ('pgsz', 3900);
CREATE TABLE indexed(message_id INTEGER PRIMARY KEY, rowid INTEGER NOT NULL);
";

/// Where search.db goes for a core.db at `core_db`: `search.db` beside a `core.db` (the
/// profile's, `Paths::search_db`), else `<name>-search.db` beside it (tests keep several
/// core.db files in one directory).
pub fn beside(core_db: &Path) -> PathBuf {
    let name = core_db
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name == "core.db" {
        core_db.with_file_name("search.db")
    } else {
        core_db.with_file_name(format!("{name}-search.db"))
    }
}

/// The two files of message search and their keys (`Debug` names no key).
#[derive(Clone)]
pub(crate) struct Paths {
    pub(crate) core: PathBuf,
    pub(crate) core_key: DbKey,
    pub(crate) search: PathBuf,
    pub(crate) search_key: DbKey,
}

impl std::fmt::Debug for Paths {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("search::file::Paths(<redacted>)")
    }
}

/// A number of `meta`; 0 when absent.
pub(crate) fn meta(conn: &Connection, key: &str) -> Result<i64> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |r| r.get(0))
        .optional()?
        .unwrap_or(0))
}

/// Stores a number of `meta`.
pub(crate) fn set_meta(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
           ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )?
    .execute(params![key, value])?;
    Ok(())
}

/// What a new file is made from: ONE read snapshot of core.db.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    pub(crate) epoch: String,
    /// The log's position: the new file's `applied`.
    pub(crate) position: i64,
    /// `max(messages.id)`: the new file's `floor` and `top`.
    pub(crate) top: i64,
    /// The compaction ask stored in core.db: a new file holds no tombstone, so it is answered.
    pub(crate) compact_after: i64,
    /// The smallest seq the log still holds, 0 when it is empty (the log is contiguous from
    /// here to `position`: AUTOINCREMENT on one writer, and the prune deletes only a prefix).
    pub(crate) log_first: i64,
}

/// Reads [`Origin`] in one read transaction of `core` (a read-only connection is enough).
pub(crate) fn origin(core: &Connection) -> Result<Origin> {
    let tx = core.unchecked_transaction()?;
    let out = Origin {
        epoch: log::epoch(&tx)?,
        position: log::position(&tx)?,
        top: tx.query_row("SELECT coalesce(max(id), 0) FROM messages", [], |r| {
            r.get(0)
        })?,
        compact_after: log::number(&tx, "compact_after")?,
        log_first: log::first(&tx)?,
    };
    tx.commit()?;
    Ok(out)
}

/// Why a file is built again (logged by name; never a text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rebuild {
    /// It cannot be opened or decrypted (another key, garbage).
    Unreadable,
    /// SQLite says the file is damaged.
    Damaged,
    /// A kill left it half made (no schema).
    Unfinished,
    /// Another `SEARCH_FILE_VERSION`.
    Version,
    /// Another `NORMALIZER_VERSION`.
    Norm,
    /// It was made for another core.db.
    Epoch,
    /// core.db's log is behind what the file applied (an older copy of core.db).
    LogBehind,
    /// core.db's log no longer holds every row above what the file applied (the file is older
    /// than the log's prune: an older copy of search.db beside a newer core.db). Its next
    /// flush would skip the deletes and the new rows of that stretch for good.
    LogPruned,
    /// The file opens but its numbers or tables do not read in the shape this build expects
    /// (another build's file, a damaged `meta`): any error of the open checks that is not core.db's.
    Unjudged,
    /// The same flush batch failed three times in a row (the search task's own rule,
    /// [`super::index`]): the file is built again rather than skipping what it cannot take.
    Stuck,
}

/// The SQLite error code of an error, if it is one.
fn sqlite_code(e: &CoreError) -> Option<kourier_db::rusqlite::ErrorCode> {
    let sqlite = match e {
        CoreError::Sql(e) => e,
        CoreError::Db(kourier_db::Error::Sqlite(e)) => e,
        _ => return None,
    };
    sqlite.sqlite_error_code()
}

/// Whether an error says the file is damaged: such an error takes the rebuild once, not the
/// back-off.
pub(crate) fn is_corrupt(e: &CoreError) -> bool {
    use kourier_db::rusqlite::ErrorCode::{DatabaseCorrupt, NotADatabase};
    matches!(sqlite_code(e), Some(DatabaseCorrupt | NotADatabase))
}

/// Whether an error is about the FILE or the machine, not about one row: the file is damaged,
/// or the disk is full or failing, memory is short, the file is busy, locked, read-only,
/// cannot be opened, or the work was interrupted. The poison pass never skips a row for it
/// (P2): it returns the error and the flush is tried again later.
pub(crate) fn is_file_error(e: &CoreError) -> bool {
    use kourier_db::rusqlite::ErrorCode as C;
    is_corrupt(e)
        || matches!(
            sqlite_code(e),
            Some(
                C::DiskFull
                    | C::SystemIoFailure
                    | C::OutOfMemory
                    | C::DatabaseBusy
                    | C::DatabaseLocked
                    | C::OperationInterrupted
                    | C::ReadOnly
                    | C::CannotOpen
                    | C::PermissionDenied
                    | C::InternalMalfunction
            )
        )
}

/// Whether an error says THIS FILE cannot be written (read-only, permission denied, cannot be
/// opened) while the directory may be fine: unlike a full disk or a busy file, a rebuild
/// cures it, so the writer counts it toward `Stuck`.
pub(crate) fn is_unwritable(e: &CoreError) -> bool {
    use kourier_db::rusqlite::ErrorCode as C;
    matches!(
        sqlite_code(e),
        Some(C::ReadOnly | C::PermissionDenied | C::CannotOpen)
    )
}

/// Whether an error is about one ROW (a constraint, a value too big, a statement the row
/// breaks): the only kind the poison pass may skip an id for.
pub(crate) fn is_row_error(e: &CoreError) -> bool {
    sqlite_code(e).is_some() && !is_file_error(e)
}

/// THE OPEN CHECKS of an opened file against core.db's [`Origin`]; `None`: it belongs.
pub(crate) fn judge(conn: &Connection, origin: &Origin) -> Result<Option<Rebuild>> {
    let made: bool = conn.query_row(
        "SELECT count(*) = 4 FROM sqlite_master
          WHERE name IN ('meta', 'epoch', 'messages_fts', 'indexed')",
        [],
        |r| r.get(0),
    )?;
    if !made {
        return Ok(Some(Rebuild::Unfinished));
    }
    if meta(conn, "version")? != SEARCH_FILE_VERSION {
        return Ok(Some(Rebuild::Version));
    }
    if meta(conn, "norm")? != i64::from(NORMALIZER_VERSION) {
        return Ok(Some(Rebuild::Norm));
    }
    let epoch: Option<String> = conn
        .query_row("SELECT value FROM epoch LIMIT 1", [], |r| r.get(0))
        .optional()?;
    if epoch.as_deref() != Some(origin.epoch.as_str()) {
        return Ok(Some(Rebuild::Epoch));
    }
    let applied = meta(conn, "applied")?;
    if origin.position < applied {
        return Ok(Some(Rebuild::LogBehind));
    }
    // Every seq above `applied` must still be in the log (or the log is empty and nothing is
    // owed): else the stretch the log was pruned over was never applied to THIS file.
    if origin.position > applied && (origin.log_first == 0 || origin.log_first > applied + 1) {
        return Ok(Some(Rebuild::LogPruned));
    }
    // Every other number the task reads after the open (the Open step, the compaction plan)
    // is read here, so one that does not read as an integer ends in the rebuild, not in a
    // search that is never ready.
    for key in [
        "floor",
        "top",
        "dead",
        "dead_since",
        "compacted_for",
        "checkpoint_due",
    ] {
        meta(conn, key)?;
    }
    Ok(None)
}

/// Opens search.db for the search task: a new file is made, one that does not belong is built
/// again. Returns the writer's connection and the reason of a rebuild (`None`: the file was
/// there and belongs, or it was new).
pub(crate) fn open(paths: &Paths, core: &Connection) -> Result<(Connection, Option<Rebuild>)> {
    if !paths.search.exists() {
        // A `-wal` without its file (copied, or left by hand) would be replayed into the new
        // file: whatever sits beside it goes first.
        remove(&paths.search)?;
        let origin = origin(core)?;
        return Ok((create(paths, &origin)?, None));
    }
    let why = match kourier_db::open(&paths.search, &paths.search_key) {
        Ok(conn) => {
            let origin = origin(core)?;
            match judge(&conn, &origin) {
                Ok(None) => return Ok((conn, None)),
                Ok(Some(why)) => why,
                Err(e) if is_corrupt(&e) => Rebuild::Damaged,
                // A busy, full or failing file is not a file to throw away: the task backs
                // off and opens it again later.
                Err(e) if is_file_error(&e) => return Err(e),
                // The file is never needed: whatever the checks cannot read is built again.
                // Only core.db's own errors (`origin` above) stay errors.
                Err(e) => {
                    tracing::warn!(error = %e, "search: the index file could not be judged");
                    Rebuild::Unjudged
                }
            }
        }
        Err(kourier_db::Error::WrongKey | kourier_db::Error::NoCipher) => Rebuild::Unreadable,
        Err(e) => {
            let e = CoreError::from(e);
            if !is_corrupt(&e) {
                return Err(e);
            }
            Rebuild::Damaged
        }
    };
    Ok((rebuild(paths, core)?, Some(why)))
}

/// THE REBUILD: the old file goes with its `-wal`, `-shm` and `-journal` (the caller holds no
/// connection to it: the task's is dropped, the query's reader released), and a new one is
/// made from a new snapshot of core.db.
pub(crate) fn rebuild(paths: &Paths, core: &Connection) -> Result<Connection> {
    remove(&paths.search)?;
    failpoint::hit(FAILPOINT_REBUILD_DELETED);
    let origin = origin(core)?;
    create(paths, &origin)
}

/// The kill point after the FIRST file of a removal went (the main file) and before the side
/// files do (the removal's kill loop).
pub const FAILPOINT_REMOVE_PARTWAY: &str = "core_search_remove_partway";

/// Deletes a search.db and the files SQLite keeps beside it, the MAIN FILE FIRST: a kill or an
/// error in between leaves side files without their file, which [`open`] clears before it
/// makes a new one. The other order could leave an older main file whose log tail was
/// already pruned, and the open would keep it (privacy P3).
pub(crate) fn remove(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for (n, file) in [
        path.to_path_buf(),
        path.with_file_name(format!("{name}-wal")),
        path.with_file_name(format!("{name}-shm")),
        path.with_file_name(format!("{name}-journal")),
    ]
    .into_iter()
    .enumerate()
    {
        match std::fs::remove_file(&file) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CoreError::Db(kourier_db::Error::Io(e))),
        }
        if n == 0 {
            failpoint::hit(FAILPOINT_REMOVE_PARTWAY);
        }
    }
    Ok(())
}

/// Makes a new file from `origin`, in one transaction: a kill leaves a file without schema,
/// which the next open builds again ([`Rebuild::Unfinished`]).
pub(crate) fn create(paths: &Paths, origin: &Origin) -> Result<Connection> {
    let mut conn = kourier_db::open(&paths.search, &paths.search_key)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(SCHEMA)?;
    for (key, value) in [
        ("version", SEARCH_FILE_VERSION),
        ("norm", i64::from(NORMALIZER_VERSION)),
        ("applied", origin.position),
        ("floor", origin.top),
        ("top", origin.top),
        ("dead", 0),
        ("dead_since", 0),
        ("compacted_for", origin.compact_after),
        // The old file's pages may still sit in a WAL of the same name: the first idle moment
        // asks for the truncating checkpoint (privacy LOW-1).
        ("checkpoint_due", 1),
    ] {
        set_meta(&tx, key, value)?;
    }
    tx.execute("INSERT INTO epoch(value) VALUES (?1)", [&origin.epoch])?;
    failpoint::hit(FAILPOINT_CREATE_BEFORE_COMMIT);
    tx.commit()?;
    Ok(conn)
}

/// How far the backfill is: `(ready, percent)` (`1 - floor / top`, no `count(*)`).
pub(crate) fn progress(conn: &Connection) -> Result<(bool, u8)> {
    let floor = meta(conn, "floor")?.max(0);
    if floor == 0 {
        return Ok((true, 100));
    }
    let top = meta(conn, "top")?.max(floor);
    let done = (top - floor).saturating_mul(100) / top;
    Ok((false, u8::try_from(done.clamp(0, 99)).unwrap_or(99)))
}
