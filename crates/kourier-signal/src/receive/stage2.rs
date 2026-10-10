//! Stage 2: inbox rows → core events, in the zone that deletes the rows (§5.6 invariant 2,
//! §6.6; Desktop's per-conversation `queueJob` in `handleDataMessage.preload.ts:274-293`).
//!
//! Rows run in *lanes*: one per conversation (the peer for 1:1 content, the group, `misc` for
//! receipts and settings). A lane keeps arrival order; lanes run concurrently as separate
//! `spawn_local` tasks (at most [`MAX_LANES`] at once), so a lane waiting for a network step
//! holds back only its own conversation. A lane step takes ≤ [`MAX_ROWS`] rows, runs their
//! network steps first (time-boxed, outside any zone: a group lane fetches the group when a
//! message names a newer revision, `crate::groups::lane`), then one zone applies the fetched
//! group state with the system messages it implies, maps the rows ([`dispatch`]), applies their
//! side effects, deletes them and appends one `core_outbox` row per inbox row.
//!
//! Poison rules (§5.6): a *content* failure (undecodable content, failed validation) counts:
//! the row is parked with a backoff and retried, and after [`MAX_ATTEMPTS`] failures (or when it
//! is older than 45 days) it is dropped with a placeholder. A *network* failure never counts: it
//! parks the whole lane (5 s … 10 min) and keeps its order.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_db::Tx;
use kourier_db::rusqlite::Connection;
use tokio::sync::{Notify, watch};
use tokio::task::spawn_local;
use tokio_util::sync::CancellationToken;

use super::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT;
use super::dispatch::{self, Ctx, Outcome, poison_placeholder};
use crate::actor::Shared;
use crate::db::Account;
use crate::error::Result;
use crate::features;
use crate::groups::GroupKeys;
use crate::groups::apply::{Trigger, Update};
use crate::net::SignalTransport;
use crate::outbox::append_event_rows;

/// Most inbox rows per lane step (§5.6).
pub(crate) const MAX_ROWS: usize = 50;
/// Lanes that run at the same time.
pub(crate) const MAX_LANES: usize = 8;
/// Content failures after which a row is dropped (Desktop's `MAX_UNPROCESSED_ATTEMPTS`).
pub(crate) const MAX_ATTEMPTS: u32 = 10;
/// Age after which a failing row is dropped (Desktop: 45 days, `Server.node.ts:6426-6460`).
pub(crate) const MAX_AGE_MS: i64 = 45 * 24 * 3600 * 1000;
/// Time box of a lane's network steps (§6.6).
pub(crate) const NETWORK_STEP_TIMEOUT: Duration = Duration::from_secs(10);
/// First park of a lane (network failure) or a row (content failure).
const PARK_MIN: Duration = Duration::from_secs(5);
/// Longest park.
const PARK_MAX: Duration = Duration::from_secs(600);
/// How long a lane rests after a `backend.db` failure of its step.
const DB_FAILURE_PAUSE: Duration = Duration::from_secs(5);

/// One `inbox` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Row {
    /// Stage-1 arrival counter (primary key).
    pub(crate) arrival: u64,
    /// Wall clock when stage 1 stored it (ms).
    pub(crate) received_at: i64,
    /// `content` or `delivery_receipt`.
    pub(crate) kind: String,
    /// The sender's service id string.
    pub(crate) sender: String,
    /// The sender's device.
    pub(crate) sender_device: u32,
    /// Envelope client timestamp (ms).
    pub(crate) timestamp: u64,
    /// Server timestamp (ms).
    pub(crate) server_timestamp: Option<i64>,
    /// When the server delivered the envelope (ms, the server's clock: `x-signal-timestamp`
    /// of the delivery), when it said so. With `server_timestamp` it gives the time the
    /// envelope waited on the server, on one clock (`super::calls::age_ms`).
    pub(crate) delivered_at: Option<i64>,
    /// It came sealed (delivery receipts are owed only for sealed messages).
    pub(crate) sealed: bool,
    /// Unpadded plaintext `Content` (not log-safe).
    pub(crate) content: Option<Vec<u8>>,
    /// Content failures so far.
    pub(crate) attempts: u32,
    /// A decryption failure: the serialized `DecryptionErrorMessage` for the sender's device
    /// (`content` is `None` then, §6.6 `retry.rs`).
    pub(crate) decrypt_error: Option<Vec<u8>>,
    /// The sealed envelope's content hint (decryption failures).
    pub(crate) content_hint: Option<u32>,
    /// The sealed envelope's group id (decryption failures of group messages).
    pub(crate) group_id: Option<Vec<u8>>,
}

/// Why a lane's network steps failed (log-safe).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NetworkFailure(pub(crate) String);

/// What a lane step's network steps hand to its zone.
#[derive(Debug, Default)]
pub(crate) struct LaneInputs {
    /// A fetched group, applied at the start of the zone (with its system messages) before the
    /// rows are mapped against the new state.
    pub(crate) group: Option<Update>,
    /// The group's fetch failed, but every sender is a member under the last known state: the
    /// rows are applied with it and the group is marked dirty (§6.6).
    pub(crate) degraded: Option<GroupKeys>,
    /// Only the first `limit` rows are applied in this step: a later row names a newer group
    /// revision, and the rows before it keep the state they were sent under; the next step
    /// starts at that row and fetches.
    pub(crate) limit: Option<usize>,
    /// The downloaded long texts of the rows, by arrival (§6.11).
    pub(crate) long_texts: HashMap<u64, String>,
}

/// A future of [`NetworkSteps::run`].
pub(crate) type StepFuture<'a> =
    Pin<Box<dyn Future<Output = Result<LaneInputs, NetworkFailure>> + 'a>>;

/// The network steps a lane step runs before its zone (§6.6). Production lanes use the group
/// fetch of `crate::groups::lane`; tests replace it through this hook (`Lanes::new`, a test-only
/// constructor) to hold a lane back or fail it. The result is an input of the zone; a failure
/// parks the lane and never counts towards the poison limit.
pub(crate) trait NetworkSteps {
    /// Runs the steps `rows` of `lane` need.
    fn run<'a>(&'a self, lane: &'a str, rows: &'a [Row]) -> StepFuture<'a>;
}

/// A lane parked after network failures.
#[derive(Clone, Copy, Debug)]
struct Park {
    until: i64,
    failures: u32,
}

/// Stage-2 state of an account actor, shared by the receive loop and the lane tasks.
pub(crate) struct Lanes {
    /// The catch-up boundary of the current connection (§4.5); `None` before its
    /// `QueueEmpty` or while offline: every message is `catch_up` then.
    boundary: Cell<Option<u64>>,
    /// Wakes the driver: new inbox rows, a lane finished.
    wake: Notify,
    /// Counts committed lane steps (backpressure and the catch-up boundary wait on it).
    progress: watch::Sender<u64>,
    /// Lanes with a running task.
    running: RefCell<HashSet<String>>,
    /// Lanes parked after network failures.
    parked: RefCell<HashMap<String, Park>>,
    /// Network steps replacing the production ones (tests).
    network: Option<Rc<dyn NetworkSteps>>,
}

impl Lanes {
    /// Lanes whose network steps are the production ones (the group fetch).
    pub(crate) fn production() -> Self {
        Self {
            boundary: Cell::new(None),
            wake: Notify::new(),
            progress: watch::Sender::new(0),
            running: RefCell::new(HashSet::new()),
            parked: RefCell::new(HashMap::new()),
            network: None,
        }
    }

    /// Lanes with the given network steps instead of the production ones (tests).
    #[cfg(test)]
    pub(crate) fn new(network: Rc<dyn NetworkSteps>) -> Self {
        Self {
            network: Some(network),
            ..Self::production()
        }
    }

    /// Sets the catch-up boundary of the current connection.
    pub(crate) fn set_boundary(&self, boundary: Option<u64>) {
        self.boundary.set(boundary);
    }

    /// The catch-up boundary of the current connection.
    pub(crate) fn boundary(&self) -> Option<u64> {
        self.boundary.get()
    }

    /// New rows are in the inbox (or the account appeared).
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// A receiver that sees every committed lane step.
    pub(crate) fn progress(&self) -> watch::Receiver<u64> {
        self.progress.subscribe()
    }

    fn park(&self, lane: &str, now: i64) -> i64 {
        let mut parked = self.parked.borrow_mut();
        let failures = parked.get(lane).map_or(0, |p| p.failures) + 1;
        let until = now.saturating_add(ms(backoff(failures)));
        parked.insert(lane.to_owned(), Park { until, failures });
        until
    }

    fn unpark(&self, lane: &str) {
        self.parked.borrow_mut().remove(lane);
    }

    fn rest(&self, lane: &str, now: i64) {
        let mut parked = self.parked.borrow_mut();
        let failures = parked.get(lane).map_or(0, |p| p.failures);
        parked.insert(
            lane.to_owned(),
            Park {
                until: now.saturating_add(ms(DB_FAILURE_PAUSE)),
                failures,
            },
        );
    }

    fn is_parked(&self, lane: &str, now: i64) -> bool {
        self.parked
            .borrow()
            .get(lane)
            .is_some_and(|p| p.until > now)
    }

    fn next_unpark(&self, now: i64) -> Option<i64> {
        self.parked
            .borrow()
            .values()
            .map(|p| p.until)
            .filter(|until| *until > now)
            .min()
    }
}

/// `PARK_MIN · 2^(n−1)`, capped at `PARK_MAX`.
fn backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    PARK_MIN.saturating_mul(1 << shift).min(PARK_MAX)
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// The stage-2 driver of an account actor: starts a task for every lane with due rows (at most
/// [`MAX_LANES`] at once), and wakes on new rows, finished lanes and park deadlines. Returns
/// when `stop` fires.
pub(crate) async fn drive<T: SignalTransport>(shared: Rc<Shared<T>>, stop: CancellationToken) {
    loop {
        let now = shared.now();
        if let Err(e) = start_due_lanes(&shared, now) {
            tracing::error!(error = %e, "stage 2: reading the inbox failed");
        }
        let next = next_wake(&shared, now);
        tokio::select! {
            () = stop.cancelled() => return,
            () = shared.lanes.wake.notified() => {}
            () = sleep_until(&shared, next) => {}
        }
    }
}

async fn sleep_until<T: SignalTransport>(shared: &Shared<T>, at: Option<i64>) {
    match at {
        Some(at) => {
            shared
                .clock
                .sleep_until(u64::try_from(at).unwrap_or(0))
                .await;
        }
        None => std::future::pending().await,
    }
}

/// The next moment a parked lane or row becomes due.
fn next_wake<T: SignalTransport>(shared: &Shared<T>, now: i64) -> Option<i64> {
    let rows = shared
        .db
        .read(|c| -> Result<Option<i64>> {
            Ok(c.query_row(
                "SELECT min(parked_until) FROM inbox WHERE parked_until > ?1",
                [now],
                |r| r.get(0),
            )?)
        })
        .unwrap_or(None);
    match (rows, shared.lanes.next_unpark(now)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn start_due_lanes<T: SignalTransport>(shared: &Rc<Shared<T>>, now: i64) -> Result<()> {
    if shared.account().is_none() {
        return Ok(());
    }
    let lanes = &shared.lanes;
    if lanes.running.borrow().len() >= MAX_LANES {
        return Ok(());
    }
    let due = shared.db.read(|c| due_lanes(c, now))?;
    for lane in due {
        if lanes.running.borrow().len() >= MAX_LANES {
            break;
        }
        if lanes.running.borrow().contains(&lane) || lanes.is_parked(&lane, now) {
            continue;
        }
        lanes.running.borrow_mut().insert(lane.clone());
        spawn_local(lane_task(shared.clone(), lane));
    }
    Ok(())
}

/// Lanes with due rows, the one with the oldest row first.
fn due_lanes(conn: &Connection, now: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT lane FROM inbox WHERE parked_until IS NULL OR parked_until <= ?1
         GROUP BY lane ORDER BY min(arrival)",
    )?;
    let lanes = stmt
        .query_map([now], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<Vec<String>>>()?;
    Ok(lanes)
}

/// Resolves once stage 2 has done what it can with the inbox as it is: no lane task runs and
/// every lane that still has due rows is parked after a network failure. The unlink procedure
/// waits for this before it stops stage 2 (§4.3 step 5).
pub(crate) async fn settle<T: SignalTransport>(shared: &Shared<T>) {
    let mut progress = shared.lanes.progress();
    loop {
        let now = shared.now();
        let idle = shared.lanes.running.borrow().is_empty();
        let due = shared.db.read(|c| due_lanes(c, now)).unwrap_or_default();
        if idle && due.iter().all(|lane| shared.lanes.is_parked(lane, now)) {
            return;
        }
        shared.lanes.wake();
        // A step committed, or a lane ended without one (parked, no progress): look again.
        let _ = tokio::time::timeout(Duration::from_millis(50), progress.changed()).await;
    }
}

/// One lane until it has no due rows (or is parked).
async fn lane_task<T: SignalTransport>(shared: Rc<Shared<T>>, lane: String) {
    loop {
        match lane_step(&shared, &lane).await {
            Ok(Step::More) => tokio::task::yield_now().await,
            Ok(Step::Idle | Step::Parked) => break,
            Err(e) => {
                tracing::error!(error = %e, "stage 2: lane step failed; the rows stay");
                shared.lanes.rest(&lane, shared.now());
                break;
            }
        }
    }
    shared.lanes.running.borrow_mut().remove(&lane);
    shared.lanes.wake();
}

/// What a lane step did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Rows were handled; there may be more.
    More,
    /// No due rows.
    Idle,
    /// A network step failed; the lane is parked.
    Parked,
}

/// One lane step: ≤ [`MAX_ROWS`] due rows → network steps → one zone.
pub(crate) async fn lane_step<T: SignalTransport>(shared: &Shared<T>, lane: &str) -> Result<Step> {
    let now = shared.now();
    let rows = shared.db.read(|c| load_lane(c, lane, now))?;
    if rows.is_empty() {
        return Ok(Step::Idle);
    }
    let fetched = match shared.lanes.network.clone() {
        Some(hook) => tokio::time::timeout(NETWORK_STEP_TIMEOUT, hook.run(lane, &rows))
            .await
            .unwrap_or_else(|_| Err(NetworkFailure("timed out".to_owned()))),
        // Time-boxed inside, so the failure policy also covers a timeout.
        None => crate::groups::lane::inputs(shared, lane, &rows).await,
    };
    let mut inputs = match fetched {
        Ok(inputs) => inputs,
        Err(NetworkFailure(reason)) => {
            let until = shared.lanes.park(lane, shared.now());
            tracing::warn!(
                rows = rows.len(),
                %reason,
                retry_in_ms = until.saturating_sub(now),
                "stage 2: network step failed; lane parked"
            );
            return Ok(Step::Parked);
        }
    };
    shared.lanes.unpark(lane);
    let rows = match inputs.limit {
        Some(limit) => &rows[..limit.min(rows.len())],
        None => &rows[..],
    };
    if shared.lanes.network.is_none() {
        // Long texts (§6.11): best effort within the step's time box, never a park.
        inputs.long_texts = super::media::long_texts(shared, rows).await;
    }
    let Some(account) = shared.account() else {
        if let Some(update) = &inputs.group {
            update.discard(&shared.blob_dir);
        }
        return Ok(Step::Idle);
    };
    let boundary = shared.lanes.boundary();
    let now = shared.now();
    let orphan = inputs.group.as_ref().and_then(Update::written_path);
    let ephemeral = shared.db.zone(|tx| {
        apply_rows(
            tx,
            &account,
            boundary,
            now,
            rows,
            &shared.wake,
            inputs,
            &shared.blob_dir,
        )
    });
    let ephemeral = match ephemeral {
        Ok(ephemeral) => ephemeral,
        Err(e) => {
            if let Some(path) = orphan {
                let _ = std::fs::remove_file(shared.blob_dir.join(path));
            }
            return Err(e);
        }
    };
    kourier_db::failpoint::hit(FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
    // The step may have queued delivery receipts (a job due in 500 ms).
    shared.jobs_wake.notify_one();
    shared
        .lanes
        .progress
        .send_modify(|n| *n = n.wrapping_add(1));
    for event in ephemeral {
        // Typing indicators are best effort: a closed core channel only loses them.
        let _ = shared.emit(event).await;
    }
    Ok(Step::More)
}

/// Due rows of `lane` in arrival order: rows parked after a content failure wait for their
/// time.
fn load_lane(conn: &Connection, lane: &str, now: i64) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare_cached(
        "SELECT arrival, received_at, kind, sender, sender_device, timestamp, server_timestamp,
           content, attempts, sealed, decrypt_error, content_hint, group_id, delivered_at
         FROM inbox WHERE lane = ?1 AND (parked_until IS NULL OR parked_until <= ?2)
         ORDER BY arrival LIMIT ?3",
    )?;
    let rows = stmt
        .query_map((lane, now, i64::try_from(MAX_ROWS).unwrap_or(50)), |r| {
            Ok(Row {
                arrival: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
                received_at: r.get(1)?,
                kind: r.get(2)?,
                sender: r.get(3)?,
                sender_device: r.get(4)?,
                timestamp: u64::try_from(r.get::<_, i64>(5)?).unwrap_or(0),
                server_timestamp: r.get(6)?,
                content: r.get(7)?,
                attempts: r.get(8)?,
                sealed: r.get(9)?,
                decrypt_error: r.get(10)?,
                content_hint: r.get(11)?,
                group_id: r.get(12)?,
                delivered_at: r.get(13)?,
            })
        })?
        .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The zone body of a lane step: applies the fetched group first (its upsert and system
/// messages are one `core_outbox` row of their own, ahead of the messages that named the new
/// revision), then maps each row, applies its side effects, deletes it and writes its events
/// (one `core_outbox` row per inbox row); a content failure rolls back that row's side effects
/// and counts an attempt. Returns the ephemeral events to send after the commit.
#[allow(
    clippy::too_many_arguments,
    reason = "one zone body, all inputs are distinct"
)]
pub(crate) fn apply_rows(
    tx: &mut Tx<'_>,
    account: &Account,
    boundary: Option<u64>,
    now: i64,
    rows: &[Row],
    wake: &Rc<Notify>,
    inputs: LaneInputs,
    blob_dir: &Path,
) -> Result<Vec<BackendEvent>> {
    let mut outbox = Vec::with_capacity(rows.len() + 1);
    let mut ephemeral = Vec::new();
    if let Some(update) = inputs.group {
        let trigger = rows.first().map(|row| Trigger {
            timestamp: row.timestamp,
            received_at: row.received_at,
            arrival: row.arrival,
            catch_up: boundary.is_none_or(|boundary| row.arrival <= boundary),
        });
        let applied =
            crate::groups::apply::apply(tx, account, blob_dir, update, trigger.as_ref(), now)?;
        outbox.push(features::with_features(tx, account, applied.events, now)?);
    }
    if let Some(keys) = inputs.degraded {
        crate::groups::mark_dirty(tx, &keys, now)?;
    }
    let long_texts = inputs.long_texts;
    for row in rows {
        let arrival = i64::try_from(row.arrival).unwrap_or(i64::MAX);
        let catch_up = boundary.is_none_or(|boundary| row.arrival <= boundary);
        let ctx = Ctx {
            account,
            catch_up,
            now,
            long_text: long_texts.get(&row.arrival).map(String::as_str),
        };
        tx.execute_batch("SAVEPOINT row")?;
        match dispatch::dispatch(tx, &ctx, row)? {
            Outcome::Handled {
                events,
                ephemeral: e,
                note,
            } => {
                tx.execute_batch("RELEASE row")?;
                if let Some(note) = note {
                    tracing::debug!(arrival = row.arrival, note, "content not mapped");
                }
                tx.prepare_cached("DELETE FROM inbox WHERE arrival = ?1")?
                    .execute([arrival])?;
                outbox.push(events);
                ephemeral.extend(e);
            }
            Outcome::Failed { reason, visible } => {
                tx.execute_batch("ROLLBACK TO row; RELEASE row")?;
                let attempts = row.attempts.saturating_add(1);
                let too_old = now.saturating_sub(row.received_at) > MAX_AGE_MS;
                if attempts >= MAX_ATTEMPTS || too_old {
                    tracing::warn!(
                        arrival = row.arrival,
                        attempts,
                        reason,
                        "stage 2: poison inbox row dropped"
                    );
                    tx.prepare_cached("DELETE FROM inbox WHERE arrival = ?1")?
                        .execute([arrival])?;
                    if visible
                        && let Some(placeholder) = poison_placeholder(row, account.aci, catch_up)
                    {
                        outbox.push(vec![placeholder]);
                    }
                } else {
                    let until = now.saturating_add(ms(backoff(attempts)));
                    tracing::debug!(
                        arrival = row.arrival,
                        attempts,
                        reason,
                        "stage 2: content failure; retrying later"
                    );
                    tx.prepare_cached(
                        "UPDATE inbox SET attempts = ?2, parked_until = ?3 WHERE arrival = ?1",
                    )?
                    .execute((arrival, attempts, until))?;
                }
            }
        }
    }
    append_event_rows(tx, &outbox, now, wake)?;
    Ok(ephemeral)
}

/// Inbox rows waiting for stage 2 that the catch-up boundary waits for: arrival ≤ `boundary`
/// and not parked after a content failure (those are retried later, like Desktop's cached
/// envelopes after the queue is empty).
pub(crate) fn pending_through(conn: &Connection, boundary: u64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM inbox WHERE arrival <= ?1 AND parked_until IS NULL",
        [i64::try_from(boundary).unwrap_or(i64::MAX)],
        |r| r.get(0),
    )?)
}

/// Inbox rows stage 2 still has to handle (the stage-1 backpressure measure, §6.6).
pub(crate) fn backlog(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT count(*) FROM inbox WHERE parked_until IS NULL",
        [],
        |r| r.get(0),
    )?)
}

/// Runs stage 2 synchronously over every due row, lane by lane, without network steps (unit
/// tests).
#[cfg(test)]
pub(crate) fn run_sync(
    db: &kourier_db::ZoneDb,
    account: &Account,
    boundary: Option<u64>,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<(usize, Vec<BackendEvent>)> {
    let mut total = 0;
    let mut ephemeral = Vec::new();
    loop {
        let mut handled = 0;
        for lane in db.read(|c| due_lanes(c, now))? {
            let rows = db.read(|c| load_lane(c, &lane, now))?;
            handled += rows.len();
            ephemeral.extend(db.zone(|tx| {
                apply_rows(
                    tx,
                    account,
                    boundary,
                    now,
                    &rows,
                    wake,
                    LaneInputs::default(),
                    Path::new("/nonexistent"),
                )
            })?);
        }
        if handled == 0 {
            return Ok((total, ephemeral));
        }
        total += handled;
    }
}
