//! The writes of the search index (G1b-G1d): the search task, the only writer of `search.db`
//! ([`super::file`]). Saving a message never waits for it: core.db holds only the log the
//! triggers append to ([`super::log`]), the task reads it on a READ-ONLY connection of core.db
//! and writes search.db on its own connection.
//!
//! THE FLUSH ([`flush`]): (1) in ONE read transaction of core.db, up to `batch` log rows above
//! search.db's `applied`, their distinct ids and those ids' rows (absent = deleted), and the
//! batch's last seq `N`; (2) ONE IMMEDIATE search.db transaction: per id the entry `indexed`
//! names goes (a tombstone, `dead + 1`), the row's entry comes in if it is searchable now
//! (sorted by rowid: FTS5 cuts a segment wherever the rowid goes down), `indexed` follows, and
//! `applied = N`; (3) the core prunes the log up to `N` (a Low job of its writer, posted after
//! the COMMIT, never above it). The snapshot at (1) holds every log row up to `N` and rows no
//! older than them; a change after it appends a row above `N` that the next batch reads. A
//! kill anywhere replays from `applied`, and a replay is harmless (`indexed` names what to
//! delete).
//!
//! What the index holds, decided here and nowhere else at write time: a row above `floor`
//! (below it, the backfill reads the row as it is when it gets there) that is not deleted, not
//! view-once, whose body parses (one that does not is counted, decisions M9) and has words
//! ([`super::normalize::words`]). What can flip later (protected, hidden, expiry-hidden chats
//! and rows, an account being removed) is a rule of the QUERY, never of the index.
//!
//! THE BACKFILL ([`backfill`]): the rows that were there when the file was made (`floor` =
//! `max(messages.id)` then) are read newest id first from a core.db snapshot, one batch per
//! job, from `Timeouts::search_backfill` after `UiReady`, with [`PACE`] between two; the
//! rows, `indexed` and the new `floor` go in one search.db transaction. THE INVARIANT (critic
//! H3): the index holds entries only for ids ABOVE `floor`, so the flush skips the log rows of
//! ids at or below it (the backfill reads those rows as they are), and the backfill inserts
//! without looking for an old entry. The flush goes first; the backfill takes the idle time.
//!
//! THE COMPACTION ([`super::compact`]) runs to its END with no flush between its steps (the
//! writer is the task's own: the log waits in core.db, the query is untouched), then the
//! truncating checkpoint, asked again while a reader holds a snapshot.
//!
//! [`run`] is the task: it starts at `UiReady` (the start does not pay for search.db: it opens
//! and judges the file then, [`super::file::open`]) and sleeps on the store's wake (set by any
//! core write job that leaves log rows above `applied`); no timer while idle but the 24 hour
//! rule's deadline on the core's clock. Its other waits are plain `tokio::time` (paused in
//! tests), never the core's clock (decisions M3).

use std::sync::Arc;
use std::time::{Duration, Instant};

use kourier_backend_api::Clock;
use kourier_db::failpoint;
use kourier_db::rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use kourier_model::Body;

use crate::error::{CoreError, Result};
use crate::runtime::Shared;
use crate::sql;
use crate::store::Store;

use super::compact::{self, Due};
use super::file::{
    self, Rebuild, is_corrupt, is_file_error, is_row_error, is_unwritable, meta, set_meta,
};
use super::log;
use super::normalize::words;

/// The kill point before the search.db commit of a backfill batch and of a flush (the kill
/// loops of `tests/search_kills.rs`).
pub const FAILPOINT_BEFORE_COMMIT: &str = "core_search_before_commit";
/// The kill point after a flush's search.db commit and before the core's prune is posted.
pub const FAILPOINT_AFTER_COMMIT: &str = "core_search_after_commit";

/// Bits of the rowid that hold the message's id (`messages.id`, AUTOINCREMENT).
pub(crate) const ID_BITS: u32 = 36;
/// The id part of a rowid.
pub(crate) const ID_MASK: i64 = (1 << ID_BITS) - 1;
/// One hour of sort keys (`sort_key` is milliseconds times 1024).
pub(crate) const HOUR: i64 = 1024 * 3_600_000;
/// The last hour a rowid can name (27 bits: the year ~17,000): a hostile `sent_at` lands
/// here, never past it.
pub(crate) const LAST_HOUR: i64 = (1 << 27) - 1;

/// The rows a job takes at first, at least, and at most.
pub(crate) const FIRST_BATCH: usize = 100;
pub(crate) const MIN_BATCH: usize = 25;
pub(crate) const MAX_BATCH: usize = 500;
/// What one job may hold the search writer for, its COMMIT included: the next job's size
/// follows from the last one's time (search.db's readers wait for nothing, but the backfill
/// shares a core with the app).
pub(crate) const JOB_BUDGET: Duration = Duration::from_millis(20);
/// How long the task gathers writes after a wake before it flushes.
pub(crate) const GATHER: Duration = Duration::from_millis(300);
/// How long the task waits after a failed job.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(30);
/// How many failures in a row of the same flush batch (the same first seq) end in the
/// rebuild of the file: the first is a plain back-off, the second goes id by id, the third is
/// the rule's end (P2).
pub(crate) const STUCK_AFTER: u32 = 3;
/// When the backfill starts after `UiReady` (the default of `Timeouts::search_backfill`;
/// decisions M2).
pub(crate) const BACKFILL_AFTER: Duration = Duration::from_secs(10);
/// The pause between two backfill batches: the flush passes, and the backfill takes a part of
/// one core, not all of it.
pub(crate) const PACE: Duration = Duration::from_millis(80);

/// The hour block of a sort key.
pub(crate) fn hour(sort_key: i64) -> i64 {
    (sort_key / HOUR).clamp(0, LAST_HOUR)
}

/// The rowid of a message in `messages_fts`: its hour in the high bits, its id in the low
/// [`ID_BITS`]; time-ordered by hour, unique because the id is, and reversible (`rowid &
/// ID_MASK` is the id). `None` for an id the rowid cannot hold (never, for an AUTOINCREMENT id
/// under 2^36).
pub(crate) fn rowid_of(sort_key: i64, id: i64) -> Option<i64> {
    (0..=ID_MASK)
        .contains(&id)
        .then(|| (hour(sort_key) << ID_BITS) | id)
}

/// The size of the next job: the last one's scaled to [`JOB_BUDGET`] by the time it took
/// (COMMIT included), at most doubled, within [`MIN_BATCH`]..=[`MAX_BATCH`].
pub(crate) fn next_batch(size: usize, took: Duration) -> usize {
    let took = took.as_micros().max(1);
    let budget = JOB_BUDGET.as_micros();
    let scaled = (size as u128).saturating_mul(budget) / took;
    let scaled = usize::try_from(scaled).unwrap_or(usize::MAX);
    scaled
        .min(size.saturating_mul(2))
        .clamp(MIN_BATCH, MAX_BATCH)
}

/// The search task's two connections: search.db (its writer) and core.db (read-only).
pub(crate) struct Files {
    pub(crate) search: Connection,
    pub(crate) core: Connection,
}

impl Files {
    /// Opens both for the task: core.db read-only, search.db through the open checks.
    pub(crate) fn open(paths: &file::Paths) -> Result<(Self, Option<Rebuild>)> {
        let core = kourier_db::open_read_only(&paths.core, &paths.core_key)?;
        let (search, why) = file::open(paths, &core)?;
        Ok((Self { search, core }, why))
    }

    /// THE REBUILD of a file that is damaged or does not belong: the connection to it is
    /// closed before the files go.
    pub(crate) fn rebuild(&mut self, paths: &file::Paths) -> Result<()> {
        let old = std::mem::replace(&mut self.search, Connection::open_in_memory()?);
        drop(old);
        self.search = file::rebuild(paths, &self.core)?;
        Ok(())
    }
}

/// What one [`flush`] did: counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flushed {
    /// Log rows taken (applied).
    pub(crate) taken: usize,
    /// Rows written into the index.
    pub(crate) indexed: usize,
    /// Old entries removed (each named by `indexed`): what `dead` grew by.
    pub(crate) removed: usize,
    /// Rows whose body does not parse: not searchable.
    pub(crate) unreadable: usize,
    /// Ids whose write failed and were skipped (storage LOW-6).
    pub(crate) skipped: usize,
    /// `applied` after the job (the prune goes up to it).
    pub(crate) applied: i64,
    /// Whether the log may hold more rows above `applied` (the batch was full).
    pub(crate) more: bool,
    /// The compaction ask of core.db, read in the same snapshot.
    pub(crate) compact_after: i64,
}

/// A row of `messages` as the index reads it.
struct Row {
    conv: i64,
    sort_key: i64,
    body: String,
    /// Not deleted, not view-once.
    searchable: bool,
}

fn read_row(conn: &Connection, id: i64) -> Result<Option<Row>> {
    Ok(conn
        .prepare_cached(
            "SELECT conversation_id, sort_key, body, deleted IS NULL AND view_once = 0
               FROM messages WHERE id = ?1",
        )?
        .query_row([id], |r| {
            Ok(Row {
                conv: r.get(0)?,
                sort_key: r.get(1)?,
                body: r.get(2)?,
                searchable: r.get(3)?,
            })
        })
        .optional()?)
}

/// How a flush job treats a failing batch (P2 + D2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pass {
    /// The whole batch in one transaction; any failure is the job's error and nothing moves
    /// (the keep loop backs off and asks again).
    Whole,
    /// The second try of a batch that failed: the whole batch again and, when it fails for a
    /// reason that may be one row's, once more id by id ([`write`]).
    Retry,
}

/// One flush job: up to `batch` log rows into the index, all or nothing.
pub(crate) fn flush(files: &mut Files, batch: usize) -> Result<Flushed> {
    flush_with(files, batch, Pass::Whole, &mut || {})
}

/// A flush job of the [`Pass::Retry`] kind.
pub(crate) fn flush_retry(files: &mut Files, batch: usize) -> Result<Flushed> {
    flush_with(files, batch, Pass::Retry, &mut || {})
}

/// [`flush`] with `between` run after the core.db snapshot ended and before the search.db
/// transaction (a test changes rows there).
pub(crate) fn flush_with(
    files: &mut Files,
    batch: usize,
    pass: Pass,
    between: &mut dyn FnMut(),
) -> Result<Flushed> {
    point_on("flush", &files.search);
    let applied = meta(&files.search, "applied")?;
    // (1) ONE snapshot of core.db: the log rows and the rows they name.
    let (taken, rows, compact_after) = {
        let tx = files.core.unchecked_transaction()?;
        let taken = log::after(&tx, applied, batch)?;
        let mut ids: Vec<i64> = taken.iter().map(|e| e.message_id).collect();
        ids.sort_unstable();
        ids.dedup();
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            rows.push((id, read_row(&tx, id)?));
        }
        let compact_after = log::number(&tx, "compact_after")?;
        tx.commit()?;
        (taken, rows, compact_after)
    };
    let mut out = Flushed {
        applied,
        compact_after,
        ..Flushed::default()
    };
    let Some(last) = taken.last().map(|e| e.seq) else {
        return Ok(out);
    };
    out.taken = taken.len();
    out.more = taken.len() >= batch.max(1);
    between();
    // The core's read snapshot is over: no search.db write runs under it (it would pin core.db's
    // WAL, and a checkpoint on the core's writer would wait for it).
    debug_assert!(files.core.is_autocommit());
    // (2) ONE search.db transaction.
    match write(&mut files.search, last, &rows, &mut out, false) {
        Ok(()) => {}
        // A first failure is never retried id by id at once: a transient error (memory, disk,
        // a busy file) would then skip ids for good. The job fails, the keep loop backs off
        // and asks again; only a batch that fails a second time goes id by id.
        Err(e) if pass == Pass::Whole || is_file_error(&e) => return Err(e),
        Err(e) => {
            tracing::warn!(error = %e, "search: a flush failed twice; trying its rows one by one");
            out = Flushed {
                applied,
                compact_after,
                taken: taken.len(),
                more: out.more,
                ..Flushed::default()
            };
            write(&mut files.search, last, &rows, &mut out, true)?;
            if out.skipped > 0 {
                tracing::warn!(
                    rows = out.skipped,
                    "search: rows the index could not take are not searchable"
                );
            }
        }
    }
    out.applied = last;
    Ok(out)
}

/// The search.db half of a flush: every id of `rows`, `applied = last`, in one transaction.
/// `one_by_one` (the poison pass): THE RULE: a skipped id may lose its words, never keep old
/// ones, and `applied` never moves past an id whose take-out did not happen. So for each id
/// the take-out of its old entry runs outside any savepoint and its failure is the whole
/// write's error; only the insert of the id's new entry runs in a savepoint of its own, and
/// when that fails for a reason of the row ([`is_row_error`]) the id is skipped (not
/// searchable; counted). An error that is not about the row ([`is_file_error`]) is returned at
/// once; so is the one of a batch whose every insert failed when a throwaway insert fails too
/// ([`probe`]: the file, not the rows; a lone bad row is skipped).
fn write(
    conn: &mut Connection,
    last: i64,
    rows: &[(i64, Option<Row>)],
    out: &mut Flushed,
    one_by_one: bool,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let floor = meta(&tx, "floor")?;
    let mut new: Vec<(i64, i64, String, String)> = Vec::new();
    let mut failed: Option<CoreError> = None;
    let mut tried = 0_usize;
    for (id, row) in rows {
        // At or below `floor` the index holds nothing: the backfill reads the row as it is.
        if *id <= floor {
            continue;
        }
        take_out(&tx, *id, out)?;
        if one_by_one {
            tried += 1;
            tx.execute_batch("SAVEPOINT one")?;
            // What the id counted goes with its savepoint when it fails.
            let unreadable = out.unreadable;
            let mut entry = Vec::new();
            match put_one(&tx, *id, row.as_ref(), out, &mut entry) {
                Ok(()) => out.indexed += entry.len(),
                Err(e) if is_row_error(&e) => {
                    tx.execute_batch("ROLLBACK TO one; RELEASE one")?;
                    out.unreadable = unreadable;
                    out.skipped += 1;
                    failed = Some(e);
                }
                Err(e) => return Err(e),
            }
        } else {
            new.extend(row.as_ref().and_then(|r| searchable(*id, r, out)));
        }
    }
    // Every id failed: a row (alone in its batch, as a new message usually is) or the file?
    // A throwaway insert tells: if the file takes it, the ids are rows and are skipped; if
    // not, nothing is skipped and the error is returned.
    if one_by_one
        && out.skipped == tried
        && let Some(e) = failed
    {
        probe(&tx).map_err(|pe| if is_file_error(&pe) { pe } else { e })?;
    }
    new.sort_unstable_by_key(|(rowid, ..)| *rowid);
    insert(&tx, &new)?;
    out.indexed += new.len();
    if out.removed > 0 {
        let dead = meta(&tx, "dead")?;
        let removed = i64::try_from(out.removed).unwrap_or(i64::MAX);
        set_meta(&tx, "dead", dead.saturating_add(removed))?;
    }
    set_meta(&tx, "applied", last)?;
    failpoint::hit(FAILPOINT_BEFORE_COMMIT);
    tx.commit()?;
    Ok(())
}

/// A throwaway insert (an entry no message has, in a savepoint that is rolled back): `Ok`
/// when the file takes an entry at all, so that a failure of real rows is about the rows.
fn probe(conn: &Connection) -> Result<()> {
    conn.execute_batch("SAVEPOINT probe")?;
    let put = insert(conn, &[(-1, -1, "probe".into(), "c0".into())]);
    // A file error inside the insert can roll back the whole transaction (no statement
    // journal): the savepoint is gone and this fails with "no such savepoint". The insert's
    // own error is the one that tells.
    let back = conn.execute_batch("ROLLBACK TO probe; RELEASE probe");
    match put {
        Err(e) => Err(e),
        Ok(()) => back.map_err(Into::into),
    }
}

/// The insert of one id's new entry inside the savepoint `one` (released when it went in).
fn put_one(
    conn: &Connection,
    id: i64,
    row: Option<&Row>,
    out: &mut Flushed,
    entry: &mut Vec<(i64, i64, String, String)>,
) -> Result<()> {
    entry.extend(row.and_then(|r| searchable(id, r, out)));
    insert(conn, entry)?;
    conn.execute_batch("RELEASE one").map_err(CoreError::from)
}

/// What the index holds of `id` goes: the entry `indexed` names (a tombstone), and its line
/// of `indexed`.
fn take_out(conn: &Connection, id: i64, out: &mut Flushed) -> Result<()> {
    let old: Option<i64> = conn
        .prepare_cached("SELECT rowid FROM indexed WHERE message_id = ?1")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    if let Some(old) = old {
        conn.prepare_cached("DELETE FROM messages_fts WHERE rowid = ?1")?
            .execute([old])?;
        conn.prepare_cached("DELETE FROM indexed WHERE message_id = ?1")?
            .execute([id])?;
        out.removed += 1;
    }
    Ok(())
}

/// The entry of a row if it is searchable now: (rowid, id, words, chat token).
fn searchable(id: i64, row: &Row, out: &mut Flushed) -> Option<(i64, i64, String, String)> {
    if !row.searchable {
        return None;
    }
    entry(id, row.conv, row.sort_key, &row.body, &mut out.unreadable)
}

/// The index entry of a row that may be searched (not deleted, not view-once): its rowid, its
/// id, its words and its chat's token; `None` for a body with no words (a notice, a call line,
/// a sticker, a placeholder) or one that does not parse (counted in `unreadable`, M9).
fn entry(
    id: i64,
    conv: i64,
    sort_key: i64,
    body: &str,
    unreadable: &mut usize,
) -> Option<(i64, i64, String, String)> {
    let body: Body = match sql::from_json(body) {
        Ok(body) => body,
        Err(_) => {
            *unreadable += 1;
            return None;
        }
    };
    let (Some(words), Some(rowid)) = (words(&body), rowid_of(sort_key, id)) else {
        return None;
    };
    Some((rowid, id, words, format!("c{conv}")))
}

/// Writes entries into the index and `indexed`, in the order given (the callers sort by
/// rowid: FTS5 cuts a segment wherever the rowid goes down).
fn insert(conn: &Connection, entries: &[(i64, i64, String, String)]) -> Result<()> {
    for (rowid, id, words, meta) in entries {
        conn.prepare_cached("INSERT INTO messages_fts(rowid, words, meta) VALUES (?1, ?2, ?3)")?
            .execute(params![rowid, words, meta])?;
        conn.prepare_cached("INSERT INTO indexed(message_id, rowid) VALUES (?1, ?2)")?
            .execute(params![id, rowid])?;
    }
    Ok(())
}

/// What one [`backfill`] batch did: counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Filled {
    /// Rows read (each id from the old `floor` down to the new one).
    pub(crate) looked: usize,
    /// Rows written into the index.
    pub(crate) indexed: usize,
    /// Rows whose body does not parse: not searchable.
    pub(crate) unreadable: usize,
    /// Rows the index would not take (a row error): not searchable, the cursor moves on.
    pub(crate) skipped: usize,
    /// `floor` after the batch: 0 = every row has been read.
    pub(crate) floor: i64,
}

/// One backfill batch: the `batch` rows at or below `floor`, newest id first, read in one
/// core.db snapshot and written as they are now (the flush's rule), sorted by rowid, with
/// their `indexed` lines and the new `floor` in ONE search.db transaction.
pub(crate) fn backfill(files: &mut Files, batch: usize) -> Result<Filled> {
    let floor = meta(&files.search, "floor")?;
    if floor <= 0 {
        return Ok(Filled::default());
    }
    let limit = batch.max(1);
    let rows: Vec<(i64, Row)> = {
        let tx = files.core.unchecked_transaction()?;
        let rows = tx
            .prepare_cached(
                "SELECT id, conversation_id, sort_key, body, deleted IS NULL AND view_once = 0
                   FROM messages WHERE id <= ?1 ORDER BY id DESC LIMIT ?2",
            )?
            .query_map(
                params![floor, i64::try_from(limit).unwrap_or(i64::MAX)],
                |r| {
                    Ok((
                        r.get(0)?,
                        Row {
                            conv: r.get(1)?,
                            sort_key: r.get(2)?,
                            body: r.get(3)?,
                            searchable: r.get(4)?,
                        },
                    ))
                },
            )?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        tx.commit()?;
        rows
    };
    let next_floor = match rows.last() {
        Some(&(id, _)) if rows.len() == limit => (id - 1).max(0),
        _ => 0,
    };
    let mut out = Filled {
        looked: rows.len(),
        floor: next_floor,
        ..Filled::default()
    };
    let mut new = Vec::new();
    for (id, row) in &rows {
        if row.searchable {
            new.extend(entry(
                *id,
                row.conv,
                row.sort_key,
                &row.body,
                &mut out.unreadable,
            ));
        }
    }
    new.sort_unstable_by_key(|(rowid, ..)| *rowid);
    // The core's read snapshot ended above: the search.db write holds none (C1).
    debug_assert!(files.core.is_autocommit());
    let tx = files
        .search
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    // The poison pass: when the batch fails for a reason of the rows, each entry goes in a
    // savepoint of its own and the ones that fail are skipped (the backfill takes nothing
    // out, so a skipped row only loses its words and never keeps old ones). A file error
    // is returned, and so is a failure of every entry when the file takes no test entry.
    tx.execute_batch("SAVEPOINT fill")?;
    match insert(&tx, &new) {
        Ok(()) => {
            tx.execute_batch("RELEASE fill")?;
            out.indexed = new.len();
        }
        Err(e) if is_row_error(&e) => {
            tx.execute_batch("ROLLBACK TO fill; RELEASE fill")?;
            for entry in &new {
                tx.execute_batch("SAVEPOINT one")?;
                match insert(&tx, std::slice::from_ref(entry)) {
                    Ok(()) => {
                        tx.execute_batch("RELEASE one")?;
                        out.indexed += 1;
                    }
                    Err(e) if is_row_error(&e) => {
                        tx.execute_batch("ROLLBACK TO one; RELEASE one")?;
                        out.skipped += 1;
                    }
                    Err(e) => return Err(e),
                }
            }
            if out.indexed == 0 {
                probe(&tx).map_err(|pe| if is_file_error(&pe) { pe } else { e })?;
            }
            tracing::warn!(
                rows = out.skipped,
                "search: rows the index could not take are not searchable"
            );
        }
        Err(e) => return Err(e),
    }
    // The cursor moves in the transaction of the rows it passed: a kill leaves both or
    // neither (critic H3).
    set_meta(&tx, "floor", next_floor)?;
    point("backfill");
    failpoint::hit(FAILPOINT_BEFORE_COMMIT);
    tx.commit()?;
    Ok(out)
}

/// Backfills until every row has been read (tests); returns the sum.
pub(crate) fn backfill_all(files: &mut Files, batch: usize) -> Result<Filled> {
    let mut sum = Filled::default();
    loop {
        let one = backfill(files, batch)?;
        sum.looked += one.looked;
        sum.indexed += one.indexed;
        sum.unreadable += one.unreadable;
        sum.skipped += one.skipped;
        sum.floor = one.floor;
        if one.floor <= 0 {
            return Ok(sum);
        }
    }
}

/// Test points that hold the search writer (unit tests: a High core job meanwhile).
#[cfg(test)]
pub(crate) mod hold {
    use std::sync::{Arc, Mutex, PoisonError, mpsc};

    type Gate = (
        &'static str,
        mpsc::Sender<()>,
        Arc<Mutex<mpsc::Receiver<()>>>,
    );
    static GATE: Mutex<Option<Gate>> = Mutex::new(None);

    /// Arms `name`: the next job that reaches it says so on the first receiver and waits until
    /// the sender is used or dropped.
    pub(crate) fn arm(name: &'static str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        *GATE.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((name, reached_tx, Arc::new(Mutex::new(release_rx))));
        (reached_rx, release_tx)
    }

    /// No point holds any more.
    pub(crate) fn disarm() {
        *GATE.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *ON_CONN.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    type OnConn = Box<dyn Fn(&kourier_db::rusqlite::Connection) + Send + Sync>;
    static ON_CONN: Mutex<Option<(&'static str, OnConn)>> = Mutex::new(None);

    /// Arms `name` with a closure that runs on the job's connection when it reaches
    /// [`point_on`] (and then the ordinary hold of the name, if armed).
    pub(crate) fn arm_on(name: &'static str, f: OnConn) {
        *ON_CONN.lock().unwrap_or_else(PoisonError::into_inner) = Some((name, f));
    }

    pub(crate) fn point_on(name: &str, conn: &kourier_db::rusqlite::Connection) {
        if let Some((n, f)) = &*ON_CONN.lock().unwrap_or_else(PoisonError::into_inner)
            && *n == name
        {
            f(conn);
        }
        point(name);
    }

    pub(crate) fn point(name: &str) {
        let gate = match &*GATE.lock().unwrap_or_else(PoisonError::into_inner) {
            Some((n, tx, rx)) if *n == name => Some((tx.clone(), rx.clone())),
            _ => None,
        };
        if let Some((tx, rx)) = gate {
            let _ = tx.send(());
            let _ = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
        }
    }
}

/// A test point of the search writer (no-op outside the crate's unit tests).
#[inline]
pub(crate) fn point(name: &str) {
    #[cfg(test)]
    hold::point(name);
    #[cfg(not(test))]
    let _ = name;
}

/// [`point`] with the connection the job holds, for a test that runs a statement of its own
/// there (no-op outside the crate's unit tests).
#[inline]
pub(crate) fn point_on(name: &str, conn: &Connection) {
    #[cfg(test)]
    hold::point_on(name, conn);
    #[cfg(not(test))]
    let _ = (name, conn);
}

/// Flushes until the log has nothing above `applied` (tests); returns the sum (`applied` the
/// last).
pub(crate) fn flush_all(files: &mut Files) -> Result<Flushed> {
    let mut sum = Flushed::default();
    loop {
        let one = flush(files, MAX_BATCH)?;
        sum.taken += one.taken;
        sum.indexed += one.indexed;
        sum.removed += one.removed;
        sum.unreadable += one.unreadable;
        sum.skipped += one.skipped;
        sum.applied = one.applied;
        sum.compact_after = one.compact_after;
        if !one.more {
            return Ok(sum);
        }
    }
}

/// The task that keeps the index up with the writes (spawned beside the expiry sweeper).
pub(crate) async fn run(sh: Arc<Shared>) {
    // It starts at `UiReady`, with no delay: the start and the first frame do not pay for it,
    // and what a kill left in the log goes at once (decisions M2).
    let mut ready = sh.ui_ready.subscribe();
    if ready.wait_for(|r| *r).await.is_err() {
        return;
    }
    let stopping = {
        let sh = sh.clone();
        move || sh.stopping()
    };
    keep_with(
        &sh.store,
        sh.timeouts.search_backfill,
        sh.timeouts.search_pace,
        sh.clock.clone(),
        stopping,
        RETRY_AFTER,
        None,
    )
    .await;
}

/// What the task does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// Open and judge search.db (rebuilt when it does not belong).
    Open,
    /// One flush job.
    Flush,
    /// One backfill batch.
    Fill,
    /// Whether a compaction is due.
    Plan,
    /// The start of a compaction (its snapshot of `dead`).
    Begin,
    /// One step of the compaction (no flush until its end).
    Merge,
    /// The end of the compaction.
    Finish,
    /// The truncating checkpoint; the number of tries so far.
    Checkpoint(u32),
}

/// What the open found.
struct Opened {
    /// Why the file was built again, if it was.
    why: Option<Rebuild>,
    applied: i64,
    floor: i64,
    /// Whether a truncating checkpoint is owed (`checkpoint_due`, or a large WAL).
    checkpoint: bool,
}

/// The size of search.db's WAL (0 when there is none).
fn wal_len(search: &std::path::Path) -> u64 {
    let mut wal = search.as_os_str().to_owned();
    wal.push("-wal");
    std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0)
}

/// What one job of the task gave.
enum Done {
    Opened(Opened),
    Flushed(Flushed),
    Filled(Filled),
    Planned(Due),
    Begun(i64),
    Merged(bool),
    Finished,
    Checkpointed(bool),
    Rebuilt,
}

/// Runs `job` on the blocking pool with the task's files (taken out and given back).
async fn on_files<F>(files: &mut Option<Files>, job: F) -> Result<(Done, Duration)>
where
    F: FnOnce(&mut Files) -> Result<Done> + Send + 'static,
{
    let mut taken = files.take().ok_or(CoreError::Stopped)?;
    let (back, out) = tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        let out = job(&mut taken).map(|d| (d, started.elapsed()));
        (taken, out)
    })
    .await
    .map_err(|_| CoreError::Stopped)?;
    *files = Some(back);
    out
}

/// The task's loop after `UiReady` (the store and the core's clock alone, so a unit test can
/// drive it): the log first, always; then a compaction that is due ([`compact`]), its steps
/// back to back with [`compact::STEP_PAUSE`] between two and NO flush until its end, and the
/// truncating checkpoint after it; the backfill from `backfill_after` on, one batch per job and
/// [`PACE`] between two; asleep on the store's wake when there is nothing to do, and on the
/// core's clock until the 24 hour rule falls due while tombstones wait for it. A damaged file
/// is built again once ([`is_corrupt`]); another error backs off for [`RETRY_AFTER`].
pub(crate) async fn keep(
    store: &Store,
    backfill_after: Duration,
    clock: Arc<dyn Clock>,
    stopping: impl Fn() -> bool,
) {
    keep_with(
        store,
        backfill_after,
        PACE,
        clock,
        stopping,
        RETRY_AFTER,
        None,
    )
    .await;
}

/// [`keep`] with the pause between two backfill batches (`pace`: `Timeouts::search_pace`, whose
/// fixture page `searchindexing` stretches it so that the index stays at its first batch, G1j),
/// its back-off after a failure (`retry_after`) and, when given, the busy timeout of the
/// writer's connection to search.db (`busy`, else the database layer's default) as parameters,
/// so a test makes them short and explicit instead of waiting out the real ones.
pub(crate) async fn keep_with(
    store: &Store,
    backfill_after: Duration,
    pace: Duration,
    clock: Arc<dyn Clock>,
    stopping: impl Fn() -> bool,
    retry_after: Duration,
    busy: Option<Duration>,
) {
    let link = store.search_link();
    let paths = link.paths().clone();
    let wake = link.wake();
    let backfill_at = tokio::time::Instant::now() + backfill_after;
    let now_ms = || i64::try_from(clock.now_ms()).unwrap_or(i64::MAX);
    let mut files: Option<Files> = None;
    let mut flush_size = FIRST_BATCH;
    let mut fill_size = FIRST_BATCH;
    // Whether old rows wait below `floor` (known after the open).
    let mut pending = false;
    let mut step = Step::Open;
    let mut rebuilt = false;
    // Whether to ask if a compaction is due (after rows were flushed, after a compaction, when
    // the 24 hour rule's time came), the time of that rule (the core's clock), the
    // compaction under way (its snapshot of `dead`, the ask it answers, its start), and when
    // to ask for a refused checkpoint again.
    let mut plan = true;
    let mut deadline: Option<i64> = None;
    let mut compact_after = 0;
    let mut compacting: Option<(i64, i64, i64)> = None;
    let mut checkpoint_at: Option<tokio::time::Instant> = None;
    // The batch that keeps failing: (its first seq, failures in a row). The first seq of a
    // batch is `applied + 1` (the log is contiguous above `applied`, checked at the open).
    // Kept in memory only: a restart starts the count again.
    let mut failing: Option<(i64, u32)> = None;
    loop {
        if stopping() {
            return;
        }
        let job = match step {
            Step::Open => {
                // The query's reader lets go of the file before it is judged (a judge may
                // build it again), and opens nothing until it is.
                link.set_ready(false);
                store.release_search().await;
                let paths = paths.clone();
                let old = files.take();
                let opened = tokio::task::spawn_blocking(move || {
                    // A file left from an earlier open of this task is closed first.
                    drop(old);
                    let (opened, why) = Files::open(&paths)?;
                    if let Some(busy) = busy {
                        opened.search.busy_timeout(busy)?;
                    }
                    let state = Opened {
                        why,
                        applied: meta(&opened.search, "applied")?,
                        floor: meta(&opened.search, "floor")?,
                        checkpoint: meta(&opened.search, "checkpoint_due")? > 0
                            || wal_len(&paths.search) > compact::WAL_LARGE,
                    };
                    Ok::<_, CoreError>((opened, state))
                })
                .await;
                match opened {
                    Ok(Ok((opened, state))) => {
                        link.set_writer_interrupt(opened.search.get_interrupt_handle());
                        files = Some(opened);
                        Ok((Done::Opened(state), Duration::ZERO))
                    }
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(CoreError::Stopped),
                }
            }
            Step::Flush => {
                let size = flush_size;
                // The same batch failed before: this time it may go id by id.
                let retry = failing.is_some_and(|(first, _)| first == link.applied() + 1);
                on_files(&mut files, move |f| {
                    if retry {
                        flush_retry(f, size)
                    } else {
                        flush(f, size)
                    }
                    .map(Done::Flushed)
                })
                .await
            }
            Step::Fill => {
                let size = fill_size;
                on_files(&mut files, move |f| backfill(f, size).map(Done::Filled)).await
            }
            Step::Plan => {
                let (now, asked) = (now_ms(), compact_after);
                on_files(&mut files, move |f| {
                    compact::due(&f.search, asked, now).map(Done::Planned)
                })
                .await
            }
            Step::Begin => {
                on_files(&mut files, |f| compact::begin(&f.search).map(Done::Begun)).await
            }
            Step::Merge => {
                on_files(&mut files, |f| {
                    compact::step(&mut f.search).map(Done::Merged)
                })
                .await
            }
            Step::Finish => {
                let (snapshot, asked, started) = compacting.unwrap_or_default();
                on_files(&mut files, move |f| {
                    compact::finish(&mut f.search, snapshot, asked, started)
                        .map(|()| Done::Finished)
                })
                .await
            }
            Step::Checkpoint(_) => {
                on_files(&mut files, |f| {
                    compact::checkpoint(&f.search).map(Done::Checkpointed)
                })
                .await
            }
        };
        // The count of one batch's failures (P2).
        let stuck = if matches!(step, Step::Flush) {
            match &job {
                Ok(_) => failing = None,
                Err(CoreError::Stopped) => {}
                // A file error (full disk, I/O, busy, locked...) is not cured by a rebuild,
                // which would only throw a healthy index away: it backs off for good, as any
                // other failed job does, and the count stays where it was.
                //
                // A file that cannot be written at all (read-only, permission, cannot be
                // opened) is the exception: a rebuild makes a new file in the directory, so
                // it counts like a row's failure.
                Err(e) if is_file_error(e) && !is_unwritable(e) => {}
                Err(_) => {
                    let first = link.applied() + 1;
                    let n = match failing {
                        Some((f, n)) if f == first => n + 1,
                        _ => 1,
                    };
                    failing = Some((first, n));
                }
            }
            job.as_ref()
                .is_err_and(|e| !is_file_error(e) || is_unwritable(e))
                && failing.is_some_and(|(_, n)| n >= STUCK_AFTER)
        } else {
            false
        };
        let unreadable = match job {
            Ok((Done::Opened(state), _)) => {
                if let Some(why) = state.why {
                    tracing::warn!(reason = ?why, "search: the index file was built again");
                }
                link.set_applied(state.applied);
                // What a kill between a commit and its prune left.
                store.post_prune(state.applied);
                link.set_ready(true);
                pending = state.floor > 0;
                plan = true;
                step = if state.checkpoint {
                    Step::Checkpoint(0)
                } else {
                    Step::Flush
                };
                0
            }
            Ok((Done::Rebuilt, _)) => {
                step = Step::Open;
                0
            }
            Ok((Done::Flushed(done), took)) => {
                flush_size = next_batch(flush_size, took);
                compact_after = done.compact_after;
                if done.taken > 0 {
                    failpoint::hit(FAILPOINT_AFTER_COMMIT);
                    link.set_applied(done.applied);
                    store.post_prune(done.applied);
                    plan = true;
                    if done.indexed + done.removed > 0 {
                        // The file took writes again: a later damage may be rebuilt again.
                        rebuilt = false;
                    }
                }
                if done.more {
                    step = Step::Flush;
                } else if plan {
                    step = Step::Plan;
                } else if pending && tokio::time::Instant::now() >= backfill_at {
                    step = Step::Fill;
                } else if checkpoint_at.is_some_and(|at| tokio::time::Instant::now() >= at) {
                    checkpoint_at = None;
                    step = Step::Checkpoint(0);
                } else {
                    // Nothing to do until a write, the backfill's time, the 24 hour rule's or
                    // a refused checkpoint's.
                    let backfill = async {
                        if pending {
                            tokio::time::sleep_until(backfill_at).await;
                        } else {
                            std::future::pending::<()>().await;
                        }
                    };
                    let rule = async {
                        match deadline {
                            Some(at) => clock.sleep_until(u64::try_from(at).unwrap_or(0)).await,
                            None => std::future::pending::<()>().await,
                        }
                    };
                    let again = async {
                        match checkpoint_at {
                            Some(at) => tokio::time::sleep_until(at).await,
                            None => std::future::pending::<()>().await,
                        }
                    };
                    tokio::select! {
                        () = wake.notified() => tokio::time::sleep(GATHER).await,
                        () = backfill => {}
                        () = rule => plan = true,
                        () = again => {}
                    }
                    step = Step::Flush;
                }
                done.unreadable
            }
            Ok((Done::Planned(due), _)) => {
                plan = false;
                deadline = None;
                step = match due {
                    Due::Now => Step::Begin,
                    Due::Not { at } => {
                        deadline = at;
                        if pending && tokio::time::Instant::now() >= backfill_at {
                            Step::Fill
                        } else {
                            // Back through the flush, which finds the log empty and sleeps.
                            Step::Flush
                        }
                    }
                };
                0
            }
            Ok((Done::Begun(snapshot), _)) => {
                compacting = Some((snapshot, compact_after, now_ms()));
                step = Step::Merge;
                0
            }
            Ok((Done::Merged(worked), _)) => {
                if worked {
                    // The next step at once (after a breath): the log waits in core.db, the
                    // query is untouched (HIGH-2: a flush between two steps restarts the
                    // merge).
                    tokio::time::sleep(compact::STEP_PAUSE).await;
                    step = Step::Merge;
                } else {
                    step = Step::Finish;
                }
                0
            }
            Ok((Done::Finished, _)) => {
                compacting = None;
                step = Step::Checkpoint(0);
                0
            }
            Ok((Done::Checkpointed(done), _)) => {
                let tries = match step {
                    Step::Checkpoint(n) => n + 1,
                    _ => compact::CHECKPOINT_TRIES,
                };
                if done || tries >= compact::CHECKPOINT_TRIES {
                    if !done {
                        tracing::warn!("search: a reader held the checkpoint; asked again later");
                        checkpoint_at =
                            Some(tokio::time::Instant::now() + compact::CHECKPOINT_RETRY);
                    }
                    plan = true;
                    step = Step::Flush;
                } else {
                    // A reader holds a snapshot: ask again.
                    tokio::time::sleep(compact::CHECKPOINT_AGAIN).await;
                    step = Step::Checkpoint(tries);
                }
                0
            }
            Ok((Done::Filled(done), took)) => {
                fill_size = next_batch(fill_size, took);
                if done.indexed > 0 {
                    rebuilt = false;
                }
                pending = done.floor > 0;
                if !pending {
                    // The 24 hour rule waits for the end of the backfill (L5): ask again.
                    plan = true;
                }
                // The log passes between two batches.
                tokio::time::sleep(pace).await;
                step = Step::Flush;
                done.unreadable
            }
            Err(CoreError::Stopped) => return,
            // The shutdown interrupts a running job (an interrupted write rolled back): the
            // task ends, it does not back off.
            Err(_) if stopping() => return,
            Err(e) if (stuck || is_corrupt(&e) && !rebuilt) && files.is_some() => {
                if stuck {
                    // Skipping what it cannot take would keep old words: build it again.
                    tracing::warn!(error = %e, reason = ?Rebuild::Stuck,
                        "search: the same batch failed three times; the index file is built again");
                    failing = None;
                } else {
                    tracing::warn!(error = %e, "search: the index file is damaged; it is built again");
                    rebuilt = true;
                }
                compacting = None;
                plan = true;
                // The query's reader lets go of the old file first.
                link.set_ready(false);
                store.release_search().await;
                let paths = paths.clone();
                match on_files(&mut files, move |f| {
                    f.rebuild(&paths).map(|()| Done::Rebuilt)
                })
                .await
                {
                    // The open judges the new file and lets the query in.
                    Ok(_) => step = Step::Open,
                    Err(e) => {
                        tracing::warn!(error = %e, "search: building the index file again failed");
                        tokio::time::sleep(retry_after).await;
                        step = Step::Open;
                    }
                }
                0
            }
            Err(e) => {
                tracing::warn!(error = %e, "search: writing the index failed; trying again later");
                flush_size = MIN_BATCH;
                fill_size = MIN_BATCH;
                compacting = None;
                plan = true;
                tokio::time::sleep(retry_after).await;
                step = Step::Open;
                0
            }
        };
        if unreadable > 0 {
            tracing::warn!(
                rows = unreadable,
                "search: message bodies that do not parse are not searchable"
            );
        }
    }
}

#[cfg(test)]
mod tests;
