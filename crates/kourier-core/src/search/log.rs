//! What core.db keeps of message search (migration `_search`): the LOG the triggers append to,
//! the EPOCH that names this core.db, and the compaction asks of a clear, a chat's delete and
//! an account's removal. Everything else of search lives in `search.db` ([`super::file`]).
//!
//! The log is append-only for the core: a row per change of a message's searchable state
//! (`seq` AUTOINCREMENT, never used twice). The search task reads it from a read-only
//! connection and deletes what search.db COMMITTED ([`prune`], a Low job of the core's writer,
//! never above the committed `applied`). These are the only search statements on the core's
//! writer: the triggers' appends, [`ask_now`] and the prune's range delete (at most [`PRUNE_ROWS`] a job).

use kourier_db::rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// The log's position: the last seq it handed out (`sqlite_sequence`), 0 when none. Never
/// goes down while this core.db lives (AUTOINCREMENT), whatever was deleted.
pub(crate) fn position(conn: &Connection) -> Result<i64> {
    let seq: Option<i64> = conn
        .prepare_cached("SELECT seq FROM sqlite_sequence WHERE name = 'search_queue'")?
        .query_row([], |r| r.get(0))
        .optional()?;
    Ok(seq.unwrap_or(0))
}

/// How many rows the log holds (what the search task has not applied and pruned yet).
pub(crate) fn queued(conn: &Connection) -> Result<u64> {
    let n: i64 = conn
        .prepare_cached("SELECT count(*) FROM search_queue")?
        .query_row([], |r| r.get(0))?;
    Ok(u64::try_from(n).unwrap_or(0))
}

/// The smallest seq the log still holds; 0 when it is empty.
pub(crate) fn first(conn: &Connection) -> Result<i64> {
    let seq: Option<i64> = conn
        .prepare_cached("SELECT min(seq) FROM search_queue")?
        .query_row([], |r| r.get(0))?;
    Ok(seq.unwrap_or(0))
}

/// A text of the search's settings in core.db (`epoch`); `None` when absent.
fn text(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM settings WHERE scope = 'search' AND key = ?1")?
        .query_row([key], |r| r.get(0))
        .optional()?)
}

/// A number of the search's settings in core.db (`compact_after`); 0 when absent or unreadable.
pub(crate) fn number(conn: &Connection, key: &str) -> Result<i64> {
    Ok(text(conn, key)?.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// Which core.db this is (written by the migration): a search.db made under another epoch
/// belongs to another core.db. `""` when absent (a file the migration has not reached cannot
/// be opened by this build anyway).
pub(crate) fn epoch(conn: &Connection) -> Result<String> {
    Ok(text(conn, "epoch")?.unwrap_or_default())
}

/// Writes a NEW epoch: every search.db of this profile is built again at its next open. For a
/// path that rewrites message ids or replaces the rows wholesale (none in this build: every
/// writer of `messages` goes row by row through the triggers).
pub(crate) fn new_epoch(conn: &Connection) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO settings(scope, key, value) VALUES ('search', 'epoch', lower(hex(randomblob(16))))
           ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
    )?
    .execute([])?;
    Ok(())
}

/// Asks for a compaction of search.db once it has applied the log up to here (and the log has
/// drained: the search task starts a compaction only with nothing left to apply, so the rows
/// a transaction appends after this call are covered too). Called in the transaction of a
/// clear, a chat's delete and an account's removal, whose words should not wait a day; a
/// missed call only means the 24 hour rule catches them. Raises the setting, never lowers it.
pub(crate) fn ask_now(conn: &Connection) -> Result<()> {
    let at = position(conn)?.max(number(conn, "compact_after")?);
    conn.prepare_cached(
        "INSERT INTO settings(scope, key, value) VALUES ('search', 'compact_after', ?1)
           ON CONFLICT(scope, key) DO UPDATE SET value = excluded.value",
    )?
    .execute([at.to_string()])?;
    Ok(())
}

/// Whether the log holds rows above `applied` (the store's wake asks this after every write
/// job, with the `applied` the search task last committed).
pub(crate) fn pending(conn: &Connection, applied: i64) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM search_queue WHERE seq > ?1)")?
        .query_row([applied], |r| r.get(0))?)
}

/// The most rows one prune job deletes (a Low job holds the core's writer for the delete, with
/// secure_delete on: 100,000 rows took 80-130 ms, over the writer's 50 ms budget; a job
/// posts itself again while rows at or below `applied` remain).
pub(crate) const PRUNE_ROWS: i64 = 5_000;

/// THE PRUNE: the log rows search.db has applied go, ONE JOB's share of them (the oldest
/// [`PRUNE_ROWS`] at most; a range delete by primary key). `applied` is a value search.db
/// COMMITTED: a row above it is still to be read. Returns how many rows went and whether rows
/// at or below `applied` remain (the job posts itself again).
pub(crate) fn prune(conn: &Connection, applied: i64) -> Result<(usize, bool)> {
    let gone = conn
        .prepare_cached(
            "DELETE FROM search_queue
              WHERE seq <= min(?1, (SELECT min(seq) FROM search_queue) + ?2 - 1)",
        )?
        .execute(params![applied, PRUNE_ROWS])?;
    let more = conn
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM search_queue WHERE seq <= ?1)")?
        .query_row([applied], |r| r.get(0))?;
    Ok((gone, more))
}

/// The whole prune up to `applied` in as many jobs as it takes (the test harness).
pub(crate) fn prune_all(conn: &Connection, applied: i64) -> Result<usize> {
    let mut total = 0;
    loop {
        let (gone, more) = prune(conn, applied)?;
        total += gone;
        if !more || gone == 0 {
            return Ok(total);
        }
    }
}

/// One entry of the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) seq: i64,
    pub(crate) message_id: i64,
}

/// Up to `limit` log rows above `applied`, in seq order.
pub(crate) fn after(conn: &Connection, applied: i64, limit: usize) -> Result<Vec<Entry>> {
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
    Ok(conn
        .prepare_cached(
            "SELECT seq, message_id FROM search_queue WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )?
        .query_map(params![applied, limit], |r| {
            Ok(Entry {
                seq: r.get(0)?,
                message_id: r.get(1)?,
            })
        })?
        .collect::<kourier_db::rusqlite::Result<_>>()?)
}
