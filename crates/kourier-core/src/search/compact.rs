//! The compaction of the search index (G1d; critic H4, H6; decisions M3, M7, M8, L5; the fix
//! round's HIGH-1, HIGH-2): the words of deleted messages leave the FILE, not only the index.
//!
//! A delete in a contentless FTS5 table writes a tombstone: `MATCH` no longer finds the row,
//! but the token's bytes stay in the segment's pages until the segment is merged. So, after
//! the log has drained, a compaction folds the segments into one in steps ([`step`]:
//! `'merge', -STEP_PAGES`, about that many pages written, every level as one) until a step
//! does no work. It runs to its END with NO flush between its steps: search.db's writer is the
//! search task's own, so the steps run back to back with [`STEP_PAUSE`] between two (the log
//! waits in core.db, the core's writer never waits, the query is untouched). Then a truncating
//! checkpoint of search.db takes the old page images out of the WAL, asked again while a
//! reader holds a snapshot; `checkpoint_due` in `meta` stays set until a TRUNCATE went through
//! (a kill after the compaction's end, 120 refused tries, a rebuild: storage LOW-2, privacy
//! LOW-1). `secure_delete` (on for every connection) zeroes what the merge frees.
//!
//! WHEN (`meta` of search.db): `dead` counts the tombstones written since the last compaction
//! (only deletes that found an entry in `indexed`, M8); `dead_since` is the core clock's time
//! when `dead` was first seen above 0 (one ahead of the clock is set to the clock's now:
//! privacy LOW-6). A compaction is due when `dead > 0` and
//! - core.db's `compact_after` (a log position: a clear, a chat's delete or an account's
//!   removal asked in its own transaction, [`super::log::ask_now`]) is above search.db's
//!   `compacted_for` and `applied` has reached it, so their words leave within minutes;
//! - or [`WINDOW_MS`] (24 hours: his answer "Within a day", decisions Q6) has passed since
//!   `dead_since` on the core's clock, and the backfill is over (`floor = 0`, L5: a
//!   compaction during the backfill would fold every segment again and again). The task's
//!   one sleep on the core's clock is this deadline, armed only while `dead > 0`.
//!
//! THE SNAPSHOT RULE (H6): a compaction takes `dead` as its snapshot ([`begin`]); at its end
//! ([`finish`]) `dead` is reduced by the snapshot, never set to 0, and `compacted_for` becomes
//! the ask it answered. With no flush between the steps nothing comes while it runs; the rule
//! stays for a writer that would change that.

use std::time::Duration;

use kourier_db::rusqlite::{Connection, TransactionBehavior};

use crate::error::Result;

use super::file::{meta, set_meta};

/// The kill point inside a compaction step, before its COMMIT (the kill loops).
pub const FAILPOINT_COMPACT_STEP: &str = "core_search_compact_step";

/// How long the words of a single delete or an expiry may stay in the file: his answer on
/// deleted text, "Within a day" (decisions Q6, M7). A later answer changes this constant only.
pub(crate) const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// About how many pages one step writes (FTS5's `merge` takes the number, negative: every
/// level counts as one, so the steps fold every segment into one).
pub(crate) const STEP_PAGES: i64 = 64;
/// The pause between two steps.
pub(crate) const STEP_PAUSE: Duration = Duration::from_millis(30);
/// How long the task waits before it asks for the truncating checkpoint again while a reader
/// holds a snapshot, and how often it asks in a row (a minute in all).
pub(crate) const CHECKPOINT_AGAIN: Duration = Duration::from_millis(500);
pub(crate) const CHECKPOINT_TRIES: u32 = 120;
/// When the task asks again after [`CHECKPOINT_TRIES`] refused tries (`checkpoint_due` stays).
pub(crate) const CHECKPOINT_RETRY: Duration = Duration::from_secs(600);
/// A WAL of search.db this large at the open is truncated then.
pub(crate) const WAL_LARGE: u64 = 4 << 20;

/// What [`due`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Due {
    /// Nothing to do now; `at` is the core clock's time (ms) at which the 24 hour rule falls
    /// due, `None` when nothing waits for it (`dead` is 0, or the backfill runs).
    Not { at: Option<i64> },
    /// A compaction is due.
    Now,
}

/// Whether a compaction is due at `now_ms` (the core's clock), given core.db's `compact_after`
/// read in the flush's snapshot. Notes `dead_since` the first time `dead` is seen above 0 (or
/// moves it back to `now_ms` when it is ahead of the clock), and answers an ask that found
/// nothing indexed.
pub(crate) fn due(conn: &Connection, compact_after: i64, now_ms: i64) -> Result<Due> {
    let dead = meta(conn, "dead")?;
    let applied = meta(conn, "applied")?;
    let answered = meta(conn, "compacted_for")?;
    let asked = compact_after > answered && applied >= compact_after;
    if dead <= 0 {
        // Nothing of the rows that went was in the index: nothing to compact.
        if asked {
            set_meta(conn, "compacted_for", compact_after)?;
        }
        if meta(conn, "dead_since")? != 0 {
            set_meta(conn, "dead_since", 0)?;
        }
        return Ok(Due::Not { at: None });
    }
    let mut since = meta(conn, "dead_since")?;
    if since <= 0 || since > now_ms {
        since = now_ms;
        set_meta(conn, "dead_since", since)?;
    }
    if asked {
        return Ok(Due::Now);
    }
    if meta(conn, "floor")? > 0 {
        return Ok(Due::Not { at: None });
    }
    let at = since.saturating_add(WINDOW_MS);
    Ok(if now_ms >= at {
        Due::Now
    } else {
        Due::Not { at: Some(at) }
    })
}

/// The start of a compaction: `dead` as its snapshot.
pub(crate) fn begin(conn: &Connection) -> Result<i64> {
    meta(conn, "dead")
}

/// One step: about [`STEP_PAGES`] pages of merging, every level as one, in one transaction.
/// Whether it did work (FTS5's own measure: the database changed by at least two rows).
pub(crate) fn step(conn: &mut Connection) -> Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = tx.total_changes();
    tx.prepare_cached("INSERT INTO messages_fts(messages_fts, rank) VALUES ('merge', ?1)")?
        .execute([-STEP_PAGES])?;
    let worked = tx.total_changes().saturating_sub(before) >= 2;
    super::index::point_on("compact_step", &tx);
    kourier_db::failpoint::hit(FAILPOINT_COMPACT_STEP);
    tx.commit()?;
    Ok(worked)
}

/// The end of a compaction that began with `snapshot`, answering core.db's `asked` (its
/// `compact_after` when it began), at `started_ms` (the core's clock): `dead` reduced by the
/// snapshot; `dead_since` cleared when nothing came while it ran, else the compaction's start
/// (what came is at most that old); `compacted_for` raised to `asked`; `checkpoint_due` set.
pub(crate) fn finish(
    conn: &mut Connection,
    snapshot: i64,
    asked: i64,
    started_ms: i64,
) -> Result<()> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let left = meta(&tx, "dead")?.saturating_sub(snapshot).max(0);
    set_meta(&tx, "dead", left)?;
    set_meta(&tx, "dead_since", if left == 0 { 0 } else { started_ms })?;
    if asked > meta(&tx, "compacted_for")? {
        set_meta(&tx, "compacted_for", asked)?;
    }
    set_meta(&tx, "checkpoint_due", 1)?;
    tx.commit()?;
    Ok(())
}

/// The truncating checkpoint of search.db: the old page images leave the WAL, and
/// `checkpoint_due` is cleared. `false` while a reader holds a snapshot (the task asks again).
pub(crate) fn checkpoint(conn: &Connection) -> Result<bool> {
    if !kourier_db::try_checkpoint_truncate(conn)? {
        return Ok(false);
    }
    if meta(conn, "checkpoint_due")? != 0 {
        // One small frame of `meta` in the new WAL: no words. A kill before it asks again.
        set_meta(conn, "checkpoint_due", 0)?;
    }
    Ok(true)
}

/// What a whole compaction did (tests), counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Compacted {
    /// Steps that did work.
    pub(crate) steps: usize,
    /// Whether the truncating checkpoint went through.
    pub(crate) checkpointed: bool,
}

/// A whole compaction in one go (tests).
pub(crate) fn compact_all(conn: &mut Connection, asked: i64, now_ms: i64) -> Result<Compacted> {
    let snapshot = begin(conn)?;
    let mut out = Compacted::default();
    while step(conn)? {
        out.steps += 1;
    }
    finish(conn, snapshot, asked, now_ms)?;
    out.checkpointed = checkpoint(conn)?;
    Ok(out)
}
