//! Lost history (W4 C20): the chunks the phone announced and Kourier never captured.
//!
//! The phone announces each piece of the history with a notice (a message of our own, `from_me`); stage 2 records its id
//! in `history_announced` ([`crate::stage2`]). The chunk itself is downloaded by upstream's client afterwards and handed to
//! the capture hook ([`super::capture`]), which keeps it in the stage; upstream keeps no copy and offers Kourier no
//! second ask at this pin, and the phone is not known to send a chunk again unless it got no receipt. So an announced id
//! that the stage does not hold is a part of the history that may not come:
//!
//! - A QUIT waits for it ([`wait_for_captures`]): `finish` gives the captures that are still in flight up to
//!   [`QUIT_WAIT`] before the client is stopped, and ONLY while there is such a chunk, so a quit with nothing in flight is as
//!   fast as it ever was. The core's stop grace for this network is 20 s (`NetworkDescriptor::shutdown_grace`) and the
//!   application's limit for the whole core 25 s, which this wait, a client's release and the outbox's drain fit in.
//! - The NEXT START counts it ([`count_at_start`], before the client and stage 2 exist, so what it finds is the earlier
//!   runs' and none of this run's): in one `backend.db` zone the notice is marked `lost` once and `history_counts.lost` grows
//!   by the number of ids marked. A chunk the stage holds is never lost, whatever its state (a `bad` one is counted `bad` by
//!   the feeder); a stage that was REPLACED at open is not judged here: the driver's reset counts what that replacement lost
//!   and forgets the notices ([`super::retention::reset`]).
//! - But NOT TOO EARLY (W4 fix MEDIUM-4): upstream itself says the phone keeps re-uploading a chunk that got no receipt, so a
//!   notice is counted lost only when no re-upload can be hoped for any more: its announcement is older than
//!   [`REUPLOAD_HOPE_MS`] (24 h), or the phone's push has finished since (a chunk with the final progress, 100, was captured
//!   after the announcement; the notice's own sync type is not known, upstream takes the payload out before the hook, so
//!   "the push of its type is over" is read from the other chunks). A recent notice stays `lost = 0` and "unresolved"
//!   (it keeps the chats' ends owed), and the quit does not wait for it (it waits only for a chunk announced in the run
//!   that quits, which is the only one whose download can still be running).
//! - A chunk that is captured AFTER its notice was counted lost takes the count back ([`take_back`], run by the driver at its
//!   start and after each wake, right behind the capture's commit): `lost = 0`, `history_counts.lost - 1`, `lost_told`
//!   clamped to the new count. A person who was told already keeps the quiet line for the run; nothing is told again.
//!
//! The notice to the person is [`notice_owed`] / [`append_notice`] / [`mark_told`].
//!
//! Nothing here logs an id or a name: counts only.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendNotice, Clock};
use wacore::store::error::StoreError;

use super::capture::decided_owner;
use super::retention;
use super::stage::{LazyStage, Origin, StageHandle};
use crate::error::{self, sql};
use crate::store::{KourierStore, exec};

/// How long a quit waits for a chunk that was announced and is not captured yet.
pub(crate) const QUIT_WAIT: Duration = Duration::from_secs(15);

/// The environment variable that shortens the quit's wait to that many milliseconds. Read only where kourier-db's kill
/// points are (debug builds and the `failpoints` feature), so a release build can never be told to wait less: it is the
/// seam of a test child whose chunk is taken by the TEST's own hook (the wire probe of upstream), never by the stage.
pub(crate) const QUIT_WAIT_ENV: &str = "KOURIER_WA_QUIT_WAIT_MS";

/// The bound of the quit's wait: [`QUIT_WAIT`], or what [`QUIT_WAIT_ENV`] says in a build that reads it.
pub(crate) fn quit_wait() -> Duration {
    if kourier_db::failpoint::ENABLED
        && let Some(ms) = std::env::var(QUIT_WAIT_ENV)
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
    {
        return Duration::from_millis(ms);
    }
    QUIT_WAIT
}

/// How long after its announcement a chunk that never arrived may still come again (the phone re-uploads what it got no
/// receipt for): only then is it counted lost, unless the phone's push has finished since (module docs).
pub(crate) const REUPLOAD_HOPE_MS: i64 = 24 * 60 * 60 * 1000;

/// The chunk with this progress is the last of its sync type (the phone's own number, 0 to 100).
const FINAL_PROGRESS: i64 = 100;

/// How often the wait looks again (real time: a capture happens on another thread).
const LOOK: Duration = Duration::from_millis(100);

/// How a wait for the captures ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waited {
    /// Nothing was announced and missing: no wait at all.
    Nothing,
    /// Every announced chunk was captured.
    Captured,
    /// The bound ran out with some still missing: they will be counted lost at the next start.
    TimedOut,
}

/// A notice announced and not yet counted lost.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Notice {
    id: String,
    /// When it was announced (wall clock, ms).
    at: i64,
}

/// The notices announced and not yet counted lost.
async fn announced(store: &KourierStore) -> Result<Vec<Notice>, StoreError> {
    store
        .read(|conn| {
            let mut statement = conn
                .prepare_cached(
                    "SELECT msg_id, announced_at FROM history_announced WHERE lost = 0 ORDER BY msg_id",
                )
                .map_err(sql("look for announced history chunks"))?;
            statement
                .query_map([], |row| {
                    Ok(Notice {
                        id: row.get(0)?,
                        at: row.get(1)?,
                    })
                })
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(sql("look for announced history chunks"))
        })
        .await
}

/// The account's own id as the stage names it, decided once ([`decided_owner`]); `None` before there is a device.
async fn owner(store: &KourierStore) -> Result<Option<String>, StoreError> {
    decided_owner(store).await
}

/// What the stage holds, for judging a notice.
struct Held {
    /// The ids of the chunks the stage holds, in any state.
    ids: HashSet<String>,
    /// When the newest chunk with the final progress was captured (wall clock, ms): the phone's push had ended by then.
    final_at: Option<i64>,
}

/// What the stage holds. Nothing when no stage file exists (nothing was captured); `None` when the stage was replaced
/// when it opened (its books are the driver's to reset).
async fn held_by_the_stage(
    stage: &Arc<LazyStage>,
    owner: String,
) -> Result<Option<Held>, StoreError> {
    if !stage.on_disk() {
        return Ok(Some(Held {
            ids: HashSet::new(),
            final_at: None,
        }));
    }
    let stage = Arc::clone(stage);
    tokio::task::spawn_blocking(move || {
        let handle = stage
            .handle(&owner)
            .map_err(|_| error::database("open the history stage"))?;
        if matches!(
            stage.origin(),
            Some(Origin::Foreign { .. } | Origin::Unreadable)
        ) {
            return Ok(None);
        }
        let reader = handle
            .reader()
            .map_err(|_| error::database("read the history stage"))?;
        let mut statement = reader
            .prepare("SELECT msg_id FROM chunks")
            .map_err(sql("look for the captured history chunks"))?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))
            .and_then(|rows| rows.collect::<Result<HashSet<_>, _>>())
            .map_err(sql("look for the captured history chunks"))?;
        let final_at = reader
            .query_row(
                "SELECT max(captured_at) FROM chunks WHERE progress >= ?1",
                [FINAL_PROGRESS],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(sql("look for the end of the history push"))?;
        Ok(Some(Held { ids, final_at }))
    })
    .await
    .map_err(|_| error::job_failed())?
}

/// The announced notices (not counted lost) whose chunk the stage does not hold, with when the phone's push last ended
/// (see [`Held::final_at`]); `None` when that cannot be told (no device yet, or the stage was replaced).
async fn missing(
    store: &KourierStore,
    stage: &Arc<LazyStage>,
) -> Result<Option<(Vec<Notice>, Option<i64>)>, StoreError> {
    let announced = announced(store).await?;
    if announced.is_empty() {
        return Ok(Some((announced, None)));
    }
    let Some(owner) = owner(store).await? else {
        return Ok(None);
    };
    let Some(held) = held_by_the_stage(stage, owner).await? else {
        return Ok(None);
    };
    // Books that point into another incarnation describe a file that is gone: what the stage lacks says nothing
    // about a loss, and the driver starts the books over before it delivers (W4 fix HIGH-1).
    if retention::stale(store, stage.incarnation()).await? {
        return Ok(None);
    }
    Ok(Some((
        announced
            .into_iter()
            .filter(|notice| !held.ids.contains(&notice.id))
            .collect(),
        held.final_at,
    )))
}

/// How many announced chunks are not in the stage and were announced at or after `since_ms` (the start of this run):
/// the downloads that can still be running. An older one is a loss for [`count_at_start`] to judge, and no download of
/// this run can bring it.
pub(crate) async fn owed(
    store: &KourierStore,
    stage: &Arc<LazyStage>,
    since_ms: i64,
) -> Result<usize, StoreError> {
    Ok(missing(store, stage).await?.map_or(0, |(notices, _)| {
        notices
            .iter()
            .filter(|notice| notice.at >= since_ms)
            .count()
    }))
}

/// Counts the chunks announced in an EARLIER run and never captured as lost, when no re-upload can be hoped for any
/// more (module docs; `now_ms` is the clock's time). Returns how many it counted. Idempotent: a notice is marked once,
/// and a kill leaves the zone whole or not at all.
pub(crate) async fn count_at_start(
    store: &KourierStore,
    stage: &Arc<LazyStage>,
    now_ms: i64,
) -> Result<usize, StoreError> {
    let Some((notices, final_at)) = missing(store, stage).await? else {
        return Ok(0);
    };
    let ids: Vec<String> = notices
        .into_iter()
        .filter(|notice| {
            notice.at.saturating_add(REUPLOAD_HOPE_MS) <= now_ms
                || final_at.is_some_and(|ended| notice.at <= ended)
        })
        .map(|notice| notice.id)
        .collect();
    if ids.is_empty() {
        return Ok(0);
    }
    mark_lost(store, ids).await
}

/// Marks the notices of `ids` lost and adds how many it marked to `history_counts.lost`, in one zone. A notice that is
/// marked already (a second counter, a repeated call) is not counted again.
async fn mark_lost(store: &KourierStore, ids: Vec<String>) -> Result<usize, StoreError> {
    store
        .write("count the lost history chunks", move |tx| {
            let mut counted = 0;
            for id in &ids {
                counted += exec(
                    tx,
                    "mark a history chunk lost",
                    "UPDATE history_announced SET lost = 1 WHERE msg_id = ?1 AND lost = 0",
                    [id],
                )?;
            }
            exec(
                tx,
                "count the lost history chunks",
                "UPDATE history_counts SET lost = lost + ?1 WHERE id = 1",
                [i64::try_from(counted).unwrap_or(i64::MAX)],
            )?;
            Ok(counted)
        })
        .await
}

/// What [`take_back`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TakenBack {
    /// Chunks that were counted lost and are in the stage: their count was taken back.
    pub(crate) back: usize,
    /// Notices still counted lost whose chunk has not come: another look after the next capture.
    pub(crate) waiting: usize,
}

/// A chunk captured after its notice was counted lost takes the count back (module docs), in one `backend.db` zone:
/// `lost = 0` on the notice, `history_counts.lost` down by one for each, `lost_told` never above the new `lost`. The
/// ids come from the stage (`stage`), the zone is the books': a kill between them leaves the count where it was and
/// the next look takes it back.
pub(crate) async fn take_back(
    store: &KourierStore,
    stage: &StageHandle,
) -> Result<TakenBack, StoreError> {
    let counted: Vec<String> = store
        .read(|conn| {
            let mut statement = conn
                .prepare_cached("SELECT msg_id FROM history_announced WHERE lost = 1")
                .map_err(sql("look for the lost history chunks"))?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .map_err(sql("look for the lost history chunks"))
        })
        .await?;
    if counted.is_empty() {
        return Ok(TakenBack {
            back: 0,
            waiting: 0,
        });
    }
    let stage = stage.clone();
    let asked = counted.clone();
    let arrived: Vec<String> = tokio::task::spawn_blocking(move || {
        let reader = stage
            .reader()
            .map_err(|_| error::database("read the history stage"))?;
        let mut found = Vec::new();
        for id in asked {
            let held = reader
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM chunks WHERE msg_id = ?1)",
                    [&id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(sql("look for a late history chunk"))?;
            if held {
                found.push(id);
            }
        }
        Ok::<_, StoreError>(found)
    })
    .await
    .map_err(|_| error::job_failed())??;
    let waiting = counted.len() - arrived.len();
    if arrived.is_empty() {
        return Ok(TakenBack { back: 0, waiting });
    }
    let back = store
        .write("take back the count of late history chunks", move |tx| {
            let mut back = 0;
            for id in &arrived {
                back += exec(
                    tx,
                    "take a late history chunk back",
                    "UPDATE history_announced SET lost = 0 WHERE msg_id = ?1 AND lost = 1",
                    [id],
                )?;
            }
            if back > 0 {
                // In an UPDATE every right-hand side reads the OLD row: the clamp uses the same new count.
                exec(
                    tx,
                    "take the late history chunks off the lost count",
                    "UPDATE history_counts SET lost = MAX(0, lost - ?1), \
                     lost_told = MIN(lost_told, MAX(0, lost - ?1)) WHERE id = 1",
                    [i64::try_from(back).unwrap_or(i64::MAX)],
                )?;
            }
            Ok(back)
        })
        .await?;
    Ok(TakenBack { back, waiting })
}

/// A quit's wait (module docs): returns at once when no chunk announced since `since_ms` (this run) is missing, else looks
/// again every [`LOOK`] until every one is captured or `bound` of the clock's time has passed. A store that does not answer ends the wait: it is a
/// quit, and what was in flight is counted at the next start.
pub(crate) async fn wait_for_captures(
    store: &KourierStore,
    stage: &Arc<LazyStage>,
    clock: &Arc<dyn Clock>,
    since_ms: i64,
    bound: Duration,
) -> Waited {
    match owed(store, stage, since_ms).await {
        Ok(0) | Err(_) => return Waited::Nothing,
        Ok(_) => {}
    }
    let mut deadline = clock.sleep(bound);
    loop {
        tokio::select! {
            () = &mut deadline => return Waited::TimedOut,
            () = tokio::time::sleep(LOOK) => {}
        }
        match owed(store, stage, since_ms).await {
            Ok(0) => return Waited::Captured,
            Ok(_) => {}
            Err(_) => return Waited::TimedOut,
        }
    }
}

// ----- the notice to the person (W4 C20b) -----

/// A notice appended to `core_outbox` and not yet taken by the core: `seq` is its row, `count` the chunks it tells of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Told {
    pub(crate) seq: i64,
    pub(crate) count: u32,
}

/// How many lost chunks the person has not been told of (`lost - lost_told`); `None` when it is none.
pub(crate) async fn notice_owed(store: &KourierStore) -> Result<Option<u32>, StoreError> {
    let owed = store
        .read(|conn| {
            conn.query_row(
                "SELECT lost - lost_told FROM history_counts WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map_err(sql("look for the lost history to tell"))
        })
        .await?;
    Ok(Some(u32::try_from(owed.max(0)).unwrap_or(u32::MAX)).filter(|count| *count > 0))
}

/// Appends the typed notice of what the person was not told of to `core_outbox` (durable, so the core has it before the
/// UI shows it), once per call: the actor calls it once per start, before its forwarder runs. `None` when nothing is
/// owed. The caller wakes the forwarder.
pub(crate) async fn append_notice(
    store: &KourierStore,
    now_ms: i64,
) -> Result<Option<Told>, StoreError> {
    let Some(count) = notice_owed(store).await? else {
        return Ok(None);
    };
    let seq =
        crate::outbox::append_notice(store, BackendNotice::HistoryLost { count }, now_ms).await?;
    Ok(Some(Told { seq, count }))
}

/// The core took the notice: the person has been told of `count` more lost chunks. Never past `lost`.
pub(crate) async fn mark_told(store: &KourierStore, count: u32) -> Result<(), StoreError> {
    store
        .write("mark the lost history told", move |tx| {
            exec(
                tx,
                "mark the lost history told",
                "UPDATE history_counts SET lost_told = MIN(lost, lost_told + ?1) WHERE id = 1",
                [i64::from(count)],
            )
            .map(|_| ())
        })
        .await
}

#[cfg(test)]
mod tests;
