//! Retention (W4 C18): what the stage keeps and for how long, and what a replaced stage leaves behind.
//!
//! The stage is the ONLY copy of a chunk until its rows are delivered, so nothing here shortens that. After:
//!
//! - A chunk is RETIRED once the cursor has passed it and every delivery row of it is settled ([`retire`]): its
//!   stage row becomes `done` with the bytes and the plan gone (`secure_delete` makes that rewrite the pages), and
//!   `history_chunks.retired` says so. The row stays: a second notice of the chunk (the same message id) is still one chunk.
//!   The order is the stage job first, `backend.db`'s mark after, so a kill between redoes a job that changes
//!   nothing; a chunk is never retired while a row of it could still be read again (a taken-back row is read from
//!   the bytes). The stage FILE stays when every chunk is done (O5): the next push is appended to it.
//! - A `bad` chunk (one that does not read) is kept for [`BAD_KEEP_MS`] counted from the moment it was set aside, in
//!   case a later build can read it, and forgotten after ([`forget_bad`]).
//! - A stage REPLACED at open (another owner's file, or one our key does not open) leaves the books in
//!   `backend.db` pointing into a stage that is gone: [`reset`] starts them over. What is already in `core_outbox`
//!   is real content that was appended while the stage was ours, so it stays and is delivered; what was not
//!   appended is lost with the stage and counted ([`reset`]).
//!
//! The REMOVAL of the stage (a logged-out exit, a fresh link's leftover) is the actor's: `LazyStage::remove` and
//! `LazyStage::clear_leftover`.
//!
//! Nothing here logs an id: errors name an operation.

use std::collections::HashSet;

use kourier_db::Priority;
use kourier_db::rusqlite::Transaction;
use wacore::store::error::StoreError;

use super::stage::StageHandle;
use crate::db::Points;
use crate::error::{self, sql};
use crate::failpoints::{AFTER_HISTORY_DONE, INSIDE_HISTORY_DONE};
use crate::store::{KourierStore, exec};

/// How long a `bad` chunk is kept: 14 days in ms.
pub(crate) const BAD_KEEP_MS: i64 = 14 * 24 * 60 * 60 * 1000;

/// How many chunks one stage job retires (a look repeats until nothing is left).
const BATCH: i64 = 16;

const POINTS: Points = Points {
    inside: INSIDE_HISTORY_DONE,
    after: AFTER_HISTORY_DONE,
};

/// Retires every chunk whose rows are all settled and returns how many it did. See the module docs.
pub(crate) async fn retire(store: &KourierStore, stage: &StageHandle) -> Result<usize, StoreError> {
    let mut retired = 0;
    loop {
        let due = store
            .read(|conn| {
                let mut statement = conn
                    .prepare_cached(
                        "SELECT chunk FROM history_chunks AS c WHERE retired = 0 \
                         AND NOT EXISTS (SELECT 1 FROM history_delivery AS d WHERE d.chunk = c.chunk) \
                         ORDER BY chunk LIMIT ?1",
                    )
                    .map_err(sql("look for delivered history chunks"))?;
                statement
                    .query_map([BATCH], |row| row.get::<_, i64>(0))
                    .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                    .map_err(sql("look for delivered history chunks"))
            })
            .await?;
        if due.is_empty() {
            return Ok(retired);
        }
        let in_stage = due.clone();
        stage
            .write_at(
                Priority::Low,
                "retire delivered history chunks",
                Some(POINTS),
                move |tx| {
                    for seq in &in_stage {
                        exec(
                            tx,
                            "retire a delivered history chunk",
                            "UPDATE chunks SET state = 'done' WHERE seq = ?1 AND state = 'planned'",
                            [seq],
                        )?;
                        exec(
                            tx,
                            "drop the bytes of a delivered history chunk",
                            "DELETE FROM chunk_bytes WHERE seq = ?1 \
                             AND EXISTS (SELECT 1 FROM chunks WHERE seq = ?1 AND state = 'done')",
                            [seq],
                        )?;
                        exec(
                            tx,
                            "forget the plan of a delivered history chunk",
                            "DELETE FROM plan WHERE chunk = ?1",
                            [seq],
                        )?;
                    }
                    Ok(())
                },
            )
            .await?;
        let count = due.len();
        store
            .sweep("mark history chunks retired", move |tx| {
                for seq in &due {
                    exec(
                        tx,
                        "mark a history chunk retired",
                        "UPDATE history_chunks SET retired = 1 WHERE chunk = ?1",
                        [seq],
                    )?;
                }
                Ok(())
            })
            .await?;
        retired += count;
        if i64::try_from(count).unwrap_or(BATCH) < BATCH {
            return Ok(retired);
        }
    }
}

/// Forgets the `bad` chunks that have been kept for [`BAD_KEEP_MS`] at `now_ms`; the message ids of the notices
/// that announced them, for [`settle_notices`].
pub(crate) async fn forget_bad(
    stage: &StageHandle,
    now_ms: i64,
) -> Result<Vec<String>, StoreError> {
    let before = now_ms.saturating_sub(BAD_KEEP_MS);
    stage
        .write_at(
            Priority::Low,
            "forget old bad history chunks",
            None,
            move |tx| {
                let ids = tx
                    .prepare_cached(
                        "SELECT msg_id FROM chunks \
                         WHERE state = 'bad' AND COALESCE(bad_at, captured_at) < ?1",
                    )
                    .and_then(|mut statement| {
                        statement
                            .query_map([before], |row| row.get::<_, String>(0))
                            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                    })
                    .map_err(sql("look for old bad history chunks"))?;
                exec(
                    tx,
                    "forget the plan of an old bad history chunk",
                    "DELETE FROM plan WHERE chunk IN (SELECT seq FROM chunks \
                     WHERE state = 'bad' AND COALESCE(bad_at, captured_at) < ?1)",
                    [before],
                )?;
                exec(
                    tx,
                    "forget an old bad history chunk",
                    "DELETE FROM chunks WHERE state = 'bad' AND COALESCE(bad_at, captured_at) < ?1",
                    [before],
                )?;
                Ok(ids)
            },
        )
        .await
}

/// The notices of chunks [`forget_bad`] forgot are no lost chunks: the chunk was captured (and counted `bad`
/// when it did not read), and the next start must not find its notice "announced and never captured" because
/// its stage row is gone. A kill between the two files leaves the notice, which the next start then counts lost
/// once: the one over-count this order allows.
pub(crate) async fn settle_notices(
    store: &KourierStore,
    ids: Vec<String>,
) -> Result<(), StoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    store
        .sweep(
            "settle the notices of forgotten history chunks",
            move |tx| {
                for id in &ids {
                    exec(
                        tx,
                        "settle the notice of a forgotten history chunk",
                        "DELETE FROM history_announced WHERE msg_id = ?1 AND lost = 0",
                        [id],
                    )?;
                }
                Ok(())
            },
        )
        .await
}

/// Sets `history_cursor.live_floor` to when the oldest chunk of the stage that waits for its delivery was captured
/// (W4 fix F5): a chunk in the state `captured` or `planned` that the books do not record as delivered. `NULL` when
/// none waits. The live notes are kept for a day before it ([`super::live::horizon`]).
pub(crate) async fn set_live_floor(
    store: &KourierStore,
    stage: &StageHandle,
) -> Result<(), StoreError> {
    let delivered: HashSet<i64> = store
        .read(|conn| {
            let mut statement = conn
                .prepare_cached("SELECT chunk FROM history_chunks")
                .map_err(sql("read the delivered history chunks"))?;
            statement
                .query_map([], |row| row.get::<_, i64>(0))
                .and_then(|rows| rows.collect::<Result<HashSet<_>, _>>())
                .map_err(sql("read the delivered history chunks"))
        })
        .await?;
    let stage = stage.clone();
    let floor = tokio::task::spawn_blocking(move || {
        let reader = stage
            .reader()
            .map_err(|_| error::database("read the history stage"))?;
        let mut statement = reader
            .prepare(
                "SELECT seq, captured_at FROM chunks WHERE state IN ('captured', 'planned') \
                 ORDER BY captured_at, seq",
            )
            .map_err(sql("look for the chunks that wait for their delivery"))?;
        let waiting = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
            .map_err(sql("look for the chunks that wait for their delivery"))?;
        Ok::<_, StoreError>(
            waiting
                .into_iter()
                .find(|(seq, _)| !delivered.contains(seq))
                .map(|(_, at)| at),
        )
    })
    .await
    .map_err(|_| error::job_failed())??;
    store
        .write("set the floor of the live notes", move |tx| {
            super::live::set_floor(tx, floor)
        })
        .await
}

/// The books of the delivery as the stage's incarnation (W4 fix HIGH-1) finds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Adopted {
    /// The books already point into this incarnation.
    Same,
    /// No stage was adopted before and the books hold nothing that points into one (the first push): the id is
    /// recorded and nothing else changes.
    First,
    /// The books pointed into another incarnation (or held cursor state with none recorded): they started over in
    /// the zone that recorded the new id.
    Reset,
}

/// Makes the books point into the stage whose incarnation is `incarnation`, BEFORE anything is delivered from it:
/// in ONE zone, an id the books already hold changes nothing; an id they do not hold starts them over
/// ([`reset_in`]) and is written in the same commit, so whoever opened or replaced the file, and however the run
/// ended in between, the next start finds either the old books with the old id or the new books with the new id,
/// never new chunks under old books. `known_lost` is the count of undelivered chunks the opener of a replaced
/// stage had read from it, when it did (a foreign owner); else the books' own estimate is used.
pub(crate) async fn adopt(
    store: &KourierStore,
    incarnation: [u8; 16],
    known_lost: Option<u64>,
) -> Result<Adopted, StoreError> {
    store
        .write("adopt the history stage", move |tx| {
            let stored: Option<Vec<u8>> = tx
                .query_row("SELECT stage FROM history_cursor WHERE id = 1", [], |r| {
                    r.get(0)
                })
                .map_err(sql("read the stage the history books point into"))?;
            if stored.as_deref() == Some(incarnation.as_slice()) {
                return Ok(Adopted::Same);
            }
            let first = stored.is_none() && books_are_empty(tx)?;
            if !first {
                reset_in(tx, known_lost)?;
            }
            exec(
                tx,
                "record the incarnation of the history stage",
                "UPDATE history_cursor SET stage = ?1 WHERE id = 1",
                [incarnation.as_slice()],
            )?;
            Ok(if first {
                Adopted::First
            } else {
                Adopted::Reset
            })
        })
        .await
}

/// Whether the books point into no stage: the cursor stands at "no chunk yet" and nothing was delivered.
fn books_are_empty(tx: &Transaction<'_>) -> Result<bool, StoreError> {
    tx.query_row(
        "SELECT (SELECT chunk FROM history_cursor WHERE id = 1) = 0 \
            AND NOT EXISTS (SELECT 1 FROM history_delivery) \
            AND NOT EXISTS (SELECT 1 FROM history_chunks)",
        [],
        |r| r.get::<_, bool>(0),
    )
    .map_err(sql("look at the history books"))
}

/// Whether the books point into a stage other than `incarnation` (one is recorded and it differs): the stage is not
/// the one they describe, so what an announced notice is missing from it says nothing about a loss, and the driver
/// will start the books over ([`adopt`]) before it delivers.
pub(crate) async fn stale(
    store: &KourierStore,
    incarnation: Option<[u8; 16]>,
) -> Result<bool, StoreError> {
    let stored = store
        .read(|conn| {
            conn.query_row("SELECT stage FROM history_cursor WHERE id = 1", [], |r| {
                r.get::<_, Option<Vec<u8>>>(0)
            })
            .map_err(sql("read the stage the history books point into"))
        })
        .await?;
    Ok(match (stored, incarnation) {
        (Some(stored), Some(now)) => stored.as_slice() != now.as_slice(),
        _ => false,
    })
}

/// Starts the books of the delivery over because the stage they point into was replaced, in ONE zone ([`adopt`]'s):
/// the cursor goes back to "no chunk yet", the delivery entries and the record of delivered chunks go (the new
/// stage numbers its chunks from 1 again), the chats that owed an end owe none (the chunks that would end them
/// are gone; the banner's own idle rule ends the fill), and the chunks the replaced stage held that were not
/// delivered are added to the lost count: `known_lost` when the opener knew it, else the books' estimate (the
/// notices announced and not counted, less the chunks whose content the cursor passed; a chunk is one notice).
///
/// Kept: the rows already in `core_outbox` (real content, appended while the stage was ours), the counts, the
/// deferred parts, the chats' hint and read marks. Dropped: the announced notices not yet counted lost, once they
/// are counted in the estimate.
fn reset_in(tx: &Transaction<'_>, known_lost: Option<u64>) -> Result<(), StoreError> {
    let lost: i64 = match known_lost {
        Some(lost) => i64::try_from(lost).unwrap_or(i64::MAX),
        None => tx
            .query_row(
                "SELECT max(0, (SELECT count(*) FROM history_announced WHERE lost = 0) \
                              - (SELECT count(*) FROM history_chunks))",
                [],
                |r| r.get(0),
            )
            .map_err(sql("estimate the history chunks a replaced stage lost"))?,
    };
    exec(
        tx,
        "move the history cursor back",
        "UPDATE history_cursor SET chunk = 0, phase = 0, conv = 0, msg = 0, idle_ms = 0 WHERE id = 1",
        [],
    )?;
    exec(
        tx,
        "forget the history delivery entries",
        "DELETE FROM history_delivery",
        [],
    )?;
    exec(
        tx,
        "forget the delivered history chunks",
        "DELETE FROM history_chunks",
        [],
    )?;
    exec(
        tx,
        "forget the chats that owed an end",
        "UPDATE history_chats SET more = 0 WHERE more = 1",
        [],
    )?;
    exec(
        tx,
        "forget the history notices not counted",
        "DELETE FROM history_announced WHERE lost = 0",
        [],
    )?;
    exec(
        tx,
        "count the chunks of a replaced stage lost",
        "UPDATE history_counts SET lost = lost + ?1 WHERE id = 1",
        [lost],
    )?;
    Ok(())
}

/// [`reset_in`] as a job of its own, for a loss the caller knows (the tests of the books).
#[cfg(test)]
pub(crate) async fn reset(store: &KourierStore, lost: u64) -> Result<(), StoreError> {
    store
        .write("reset the history books of a replaced stage", move |tx| {
            reset_in(tx, Some(lost))
        })
        .await
}

#[cfg(test)]
mod tests;
