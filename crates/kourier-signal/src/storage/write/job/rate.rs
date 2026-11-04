//! The rate of the write job (A10c chunk C3, BQ4): at most 6 writes in any 10 minutes and 30 in any
//! hour per account, whatever the user clicks. Extra clicks wait in their intents and go out as ONE
//! write when a window frees ("batch"). One write is in flight at a time by construction (one job
//! row, one run at a time, one in-flight row).
//!
//! THE TIMES ARE PERSISTED (`storage_write_times`, backend.db 25, one row per write): a restart, a
//! dropped job row or a kill keeps the count. The row is inserted in the zone that writes the
//! in-flight row ([`record`]), so a PUT that went out always has its time; a kill between that zone
//! and the PUT counts a write that never went out, which errs on the side of fewer writes. COUNTED
//! is a write the server applied or may have applied: a 200, and a PUT whose answer was lost (a
//! timeout, a dropped connection, or ANY 5xx, which a gateway may answer after the Storage Service
//! applied the write: applied or not, the next run judges the row, may write again and counts
//! again). A status below 500 (409, 429, 400, 413, a second 401 or 403) says the server refused
//! the write before it applied anything: the zone that handles it takes the time back
//! ([`forget`]), so a refusing server is paced by the normal back-off alone and does not use up the
//! places of the writes that will succeed (a recovery would otherwise wait for a window that
//! refusals filled). Once per row: the second PUT after a 401 or 403 is the same write. Not
//! counted: a run that sends nothing (a refused plan, intents a later record decides, a switch
//! that went off).
//!
//! WHEN A WINDOW IS FULL the wait is the REMEMBERED HOLD of the kind (`jobs::pause_kind`,
//! `job_holds`) plus `Done::RepeatAt` of the same time: while it stands, no helper makes the job
//! due earlier (`jobs::due_floor`; critic Y4), and a click only batches into the next write. The
//! hold is never made by pulling `next_at` forward or back: the only helper a click uses is
//! `jobs::delay_to`, which moves `next_at` LATER only.

use kourier_db::rusqlite::Connection;

use crate::error::Result;

/// `(window in ms, most writes in it)`: 6 per 10 minutes, 30 per hour (BQ4).
pub(super) const WINDOWS: [(i64, usize); 2] = [(10 * 60 * 1000, 6), (60 * 60 * 1000, 30)];

/// A time older than the longest window counts for nothing and is deleted.
const KEEP_MS: i64 = WINDOWS[1].0;

/// When the next write may be made, given the times (ms) of the earlier ones: `None` when every
/// window has room, else the time at which the LAST window that is full frees one place (the
/// oldest write inside a full window leaves it `window` after it was made). A time after `now` (the
/// clock stepped back) counts as `now`, so the longest wait is the longest window. Pure.
pub(super) fn release_at(times: &[i64], now: i64, windows: &[(i64, usize)]) -> Option<i64> {
    let mut times: Vec<i64> = times.iter().map(|at| (*at).min(now)).collect();
    times.sort_unstable_by(|a, b| b.cmp(a));
    windows
        .iter()
        .filter_map(|&(window, cap)| {
            let first_inside = now.saturating_sub(window);
            let inside = times.iter().take_while(|at| **at > first_inside).count();
            if inside < cap {
                return None;
            }
            // The `cap`-th newest write is the one whose leaving makes room.
            let decisive = times.get(cap.checked_sub(1)?)?;
            Some(decisive.saturating_add(window))
        })
        .max()
}

/// Forgets the times that no window counts any more, and says when the next write may be made
/// ([`release_at`] over the stored times). A zone: it deletes.
pub(super) fn wait_until(conn: &Connection, now: i64) -> Result<Option<i64>> {
    conn.prepare_cached("DELETE FROM storage_write_times WHERE at <= ?1")?
        .execute([now.saturating_sub(KEEP_MS)])?;
    let times = conn
        .prepare_cached("SELECT at FROM storage_write_times ORDER BY at DESC")?
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(release_at(&times, now, &WINDOWS))
}

/// One more write at `now`, in the zone that writes the in-flight row. Returns the row, for
/// [`forget`].
pub(super) fn record(conn: &Connection, now: i64) -> Result<i64> {
    conn.prepare_cached("INSERT INTO storage_write_times(at) VALUES (?1)")?
        .execute([now])?;
    Ok(conn.last_insert_rowid())
}

/// The server answered with a status: nothing was applied, the place is free again.
pub(super) fn forget(conn: &Connection, time: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM storage_write_times WHERE rowid = ?1")?
        .execute([time])?;
    Ok(())
}

/// How many times are stored (tests).
#[cfg(test)]
pub(super) fn stored(conn: &Connection) -> Result<usize> {
    let count: i64 = conn
        .prepare_cached("SELECT count(*) FROM storage_write_times")?
        .query_row([], |r| r.get(0))?;
    Ok(usize::try_from(count).unwrap_or(usize::MAX))
}
