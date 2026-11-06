//! Stage 2 of the receive path (W3a; architecture §5.6, §7.1; the Signal backend's
//! `receive::stage2` is the model): inbox rows -> core events, in the zone that finishes them.
//!
//! One task per account, started with the actor (so rows a previous run left are handled at once) and
//! woken after every inbox commit. A step takes at most [`MAX_ROWS`] `new` rows in arrival order, maps
//! them ([`crate::map`], pure) and applies ALL of it in ONE zone: one `core_outbox` row per row that
//! stands for an event, the row set `done` with its content wiped (`secure_delete` clears the file's
//! pages; the old pages stay in the WAL, encrypted, until a checkpoint), the poison counters. So the events are in `core_outbox` exactly when the rows are `done`: a kill
//! before COMMIT leaves every row `new` and no event, a kill after it leaves both, and a row can
//! never be turned into events twice. A `done` row stays as the ledger that makes upstream's
//! redelivery a no-op (`Inbox::commit`'s unique key).
//!
//! DEFERRED rows (fix F3): an item whose kind this build does not map (see [`crate::map`]) is set
//! `deferred` with its content and the map's version, not wiped and not counted by the poison rule. At
//! the start of the task ([`Stage2::drive`]) the rows a LOWER map version deferred are made `new`
//! again, so a later build maps what an earlier one could not. Only an item that is provably no
//! message (`map::class::Class::Inert`) is finished without an event; so is a history-sync notice of our
//! own phone, whose id is recorded in the same zone (`history_announced`, W4).
//!
//! Poison rule (Signal's): a content failure counts; the row is parked with a backoff, and after
//! [`MAX_ATTEMPTS`] failures, or when it is older than 45 days, it is finished with a placeholder
//! (when the sender is known).
//!
//! WAITING FOR A GROUP'S MEMBERS (W6a2, D11, M2): an ADMIN's delete-for-everyone in a group is judged by the core from the
//! member list it was told, and dropped for good without one. So the map says the row WAITS ([`Mapped::waits_for_members`])
//! and the zone parks it on the group's lane (`inbox.members_wait`, the group's key) until `received_at` + 45 days, with NO
//! attempt counted; the zone that states the group's members lets the lane go ([`people::release_waiting`]) and an
//! answer that says we are not in the group finishes it with no event ([`people::end_waiting`]). Whatever the mapper
//! decided from the facts it read, the zone looks at the group's row AGAIN (one writer, so the order is total): a list stated
//! in between is no reason to park. A row that reaches its 45th day (or whose group we are no member of) is finished with no
//! event: waiting is no failure and a delete nobody can judge is not kept for ever.
//!
//! Backlog (decision D8): the hook waits in [`Notes::room`](crate::hook::Notes::room) while
//! [`MAX_BACKLOG`] rows wait for this stage, so upstream's pipeline holds the acks instead of the inbox
//! growing without bound. The count is soft (a commit and a step race), and the wait is bounded by
//! [`Timing::room_patience`]: a stage 2 that cannot run (a failing disk) must not hang the connection.
//!
//! LINK HOLD (fix F5): while a login has no account yet ([`crate::hold`]) a step does nothing, so no row
//! of the new connection can reach `core_outbox` below `AccountReady`.
//!
//! Logs carry counts only.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use kourier_backend_api::{BackendEvent, Clock, encode_outbox_row};
use kourier_db::outbox;
use kourier_db::rusqlite::{Connection, params};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use wacore::store::Device;
use wacore::store::error::StoreError;
use wacore::store::traits::DeviceStore as _;

use crate::db::Points;
use crate::error::{self, sql};
use crate::fault;
use crate::features;
use crate::history;
use crate::hook::Notes;
use crate::inbox::{Committed, Origin};
use crate::map::{self, Ctx, Failure, MAP_VERSION, Mapped, OwnIds, Row};
use crate::store::{self, KourierStore};

/// Most inbox rows per step.
pub(crate) const MAX_ROWS: usize = 50;
/// Content failures after which a row is finished with a placeholder (Desktop's
/// `MAX_UNPROCESSED_ATTEMPTS`, as Signal's stage 2).
pub(crate) const MAX_ATTEMPTS: u32 = 10;
/// Age after which a failing row is finished (Desktop: 45 days).
pub(crate) const MAX_AGE_MS: i64 = 45 * 24 * 3600 * 1000;
/// How long a newsletter (channel) item is kept deferred (W6a1, critic L4): channels are shown by a later step (K8b); an
/// item older than this by its ARRIVAL (`received_at`, never `aged_from`: a map-version bump restarts that) would only
/// fill the store, so it is finished at a start with no event.
pub(crate) const NEWSLETTER_KEEP_MS: i64 = 14 * 24 * 3600 * 1000;
/// Rows waiting for this stage above which the hook waits (Signal's `MAX_BACKLOG`).
pub(crate) const MAX_BACKLOG: usize = 2000;
/// First and longest park of a failing row.
const PARK_MIN: Duration = Duration::from_secs(5);
const PARK_MAX: Duration = Duration::from_secs(600);

/// The kill points of the stage-2 zone (`failpoints::INSIDE_STAGE2` / `AFTER_STAGE2`).
const POINTS: Points = Points {
    inside: crate::failpoints::INSIDE_STAGE2,
    after: crate::failpoints::AFTER_STAGE2,
};

/// The waits and bounds of one stage 2; the defaults are the production ones, tests shrink them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// How long the task rests after a step failed (the rows stay).
    pub(crate) db_failure_pause: Duration,
    /// How long the hook waits for room before it goes on regardless.
    pub(crate) room_patience: Duration,
    /// The backlog above which the hook waits.
    pub(crate) max_backlog: usize,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            db_failure_pause: Duration::from_secs(5),
            room_patience: Duration::from_secs(60),
            max_backlog: MAX_BACKLOG,
        }
    }
}

/// What a step did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Report {
    /// Rows that were looked at.
    pub(crate) taken: usize,
    /// Rows finished (an event, or none because the item is no message).
    pub(crate) finished: usize,
    /// Rows kept for a later build (their kind is not mapped yet).
    pub(crate) deferred: usize,
    /// `core_outbox` rows written.
    pub(crate) events: usize,
    /// Rows parked after a content failure.
    pub(crate) parked: usize,
    /// Rows finished after too many failures (a placeholder where the sender was known).
    pub(crate) poisoned: usize,
    /// Rows still waiting for this stage and not parked.
    pub(crate) remaining: usize,
    /// Notices of a history chunk our phone announced that this step recorded for the first time.
    pub(crate) announced: usize,
    /// Admin deletes parked until their group's member list is stated (W6a2, D11).
    pub(crate) waiting: usize,
    /// Admin deletes finished with no event: the 45 days ran out, or we are no member of the group (W6a2, M2, M3).
    pub(crate) ended: usize,
    /// Push names stated for the senders of this step's messages: only the ones that changed (W6a2, B8a, D8).
    pub(crate) names: usize,
}

/// Stage 2 of one account.
pub(crate) struct Stage2 {
    store: KourierStore,
    clock: Arc<dyn Clock>,
    /// Wakes the `core_outbox` forwarder.
    outbox_wake: Arc<Notify>,
    /// Wakes the driver (shared with the event writer, which wakes it after it let a group's waiting deletes go).
    wake: Arc<Notify>,
    /// Rows waiting for this stage.
    backlog: watch::Sender<usize>,
    /// Counts committed steps.
    progress: watch::Sender<u64>,
    timing: Timing,
    /// Our own ids, once the device row names both (a pairing never changes them).
    own: Mutex<Option<OwnIds>>,
}

impl Stage2 {
    pub(crate) fn new(
        store: KourierStore,
        clock: Arc<dyn Clock>,
        outbox_wake: Arc<Notify>,
        timing: Timing,
    ) -> Self {
        Self {
            store,
            clock,
            outbox_wake,
            wake: Arc::new(Notify::new()),
            backlog: watch::Sender::new(0),
            progress: watch::Sender::new(0),
            timing,
            own: Mutex::new(None),
        }
    }

    /// Our own ids, for whoever must tell our own presence from a peer's (the actor's typing).
    pub(crate) async fn own(&self) -> OwnIds {
        self.ctx().await.own
    }

    /// Our own ids as far as they are known, without a look at the store: what the handler (a thread
    /// that must not wait for a job) tells our own acts by. Empty until the actor read the device
    /// row or the link stated them (`set_own`), which happens before any client can run.
    pub(crate) fn own_known(&self) -> OwnIds {
        self.own
            .lock()
            .ok()
            .and_then(|own| own.clone())
            .unwrap_or_default()
    }

    /// The link stated our ids.
    pub(crate) fn set_own(&self, own: OwnIds) {
        if own.lid.is_some()
            && own.pn.is_some()
            && let Ok(mut slot) = self.own.lock()
        {
            *slot = Some(own);
        }
    }

    /// What the map needs besides a row: our own ids from the device row. Read until both are known
    /// (a device without them has no mention to match), then kept.
    async fn ctx(&self) -> Ctx {
        let known = self.own.lock().ok().and_then(|own| own.clone());
        if let Some(own) = known {
            return Ctx { own };
        }
        let own = match self.store.load().await {
            Ok(Some(device)) => own_ids(&device),
            // A failed or empty read maps without our ids this step (a mention of us is then not
            // marked): the same read is made again at the next step.
            Ok(None) | Err(_) => OwnIds::default(),
        };
        if own.lid.is_some()
            && own.pn.is_some()
            && let Ok(mut slot) = self.own.lock()
        {
            *slot = Some(own.clone());
        }
        Ctx { own }
    }

    fn now(&self) -> i64 {
        i64::try_from(self.clock.now_ms()).unwrap_or(i64::MAX)
    }

    /// New rows are in the inbox.
    pub(crate) fn wake(&self) {
        self.wake.notify_one();
    }

    /// The driver's wake, for whoever makes rows due without writing the inbox (the event writer, W6a2).
    pub(crate) fn waker(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// A receiver that sees every committed step (the catch-up machine waits on it).
    pub(crate) fn progress(&self) -> watch::Receiver<u64> {
        self.progress.subscribe()
    }

    /// Rows waiting for this stage, as of the last step and commit (soft).
    pub(crate) fn backlog(&self) -> usize {
        *self.backlog.borrow()
    }

    /// One step: at most [`MAX_ROWS`] due rows -> events, in one zone. `Err` leaves every row as it
    /// was.
    pub(crate) async fn step(&self) -> Result<Report, StoreError> {
        // A link with no account yet: no row may reach `core_outbox` before `AccountReady` does
        // (`crate::hold`). The rows stay `new`; the actor wakes this stage when the hold opens.
        if self.store.hold().is_closed() {
            return Ok(Report::default());
        }
        let now = self.now();
        let rows = self.store.read(move |conn| load(conn, now)).await?;
        if rows.is_empty() {
            let remaining = self
                .store
                .read(move |conn| backlog_count(conn, now))
                .await?;
            self.backlog.send_replace(remaining);
            return Ok(Report {
                remaining,
                ..Report::default()
            });
        }
        // Mapping is pure and happens outside the zone, in arrival order.
        let ctx = self.ctx().await;
        let mut results: Vec<Result<Mapped, Failure>> =
            rows.iter().map(|row| map::map_row(row, &ctx)).collect();
        // A revoke in a group is judged by two facts of the store (D11, M3): read only for the rows that say they need
        // them, then those rows are mapped again. A failed read leaves every row as it was.
        let needing: Vec<usize> = results
            .iter()
            .enumerate()
            .filter_map(|(i, result)| result.as_ref().is_ok_and(|m| m.needs_facts).then_some(i))
            .collect();
        if !needing.is_empty() {
            let wants: Vec<(String, String)> = needing
                .iter()
                .map(|&i| (rows[i].chat.clone(), rows[i].sender.clone()))
                .collect();
            let facts = self
                .store
                .read(move |conn| group_facts(conn, &wants))
                .await?;
            for i in needing {
                results[i] = map::map_row_with(&rows[i], &ctx, Some(&facts));
            }
        }
        let plan: Vec<Planned> = rows
            .into_iter()
            .zip(results)
            .map(|(row, result)| Planned::new(row, result, now))
            .collect();
        let report = self
            .store
            .write_at("stage 2", POINTS, move |tx| apply(tx, now, &plan))
            .await?;
        if report.events > 0 {
            self.outbox_wake.notify_one();
        }
        // After the zone committed, one fixed line per notice (T1: no id, no count of anything else).
        for _ in 0..report.announced {
            tracing::info!("history notice recorded");
        }
        self.backlog.send_replace(report.remaining);
        self.progress.send_modify(|n| *n = n.wrapping_add(1));
        Ok(report)
    }

    /// The task: steps until nothing is due, then waits for a wake or the next parked row. Returns
    /// when `stop` fires (a step in flight finishes first).
    pub(crate) async fn drive(self: Arc<Self>, stop: CancellationToken) {
        // Rows an earlier build's map deferred are this build's to try (a failure only waits for the
        // next start: they are deferred, not lost).
        match self.finish_old_newsletters().await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "stage 2 finishes old deferred newsletter rows"),
            Err(e) => tracing::error!(error = %e, "stage 2: finishing old newsletter rows failed"),
        }
        // Once per start, with `rows=0` too: `wa-run`'s receive steps wait for this line as the sign
        // that stage 2 began.
        match self.take_up_deferred().await {
            Ok(n) => tracing::info!(rows = n, "stage 2 takes up deferred rows"),
            Err(e) => tracing::error!(error = %e, "stage 2: taking up deferred rows failed"),
        }
        loop {
            if stop.is_cancelled() {
                return;
            }
            match self.step().await {
                Ok(report) => {
                    if report.taken > 0 {
                        tracing::debug!(
                            taken = report.taken,
                            finished = report.finished,
                            deferred = report.deferred,
                            events = report.events,
                            parked = report.parked,
                            poisoned = report.poisoned,
                            waiting = report.waiting,
                            ended = report.ended,
                            names = report.names,
                            remaining = report.remaining,
                            "stage 2 step"
                        );
                        tokio::task::yield_now().await;
                        continue;
                    }
                }
                Err(e) => {
                    // The error names the operation, never an input.
                    tracing::error!(error = %e, "stage 2: a step failed; the rows stay");
                    tokio::select! {
                        () = stop.cancelled() => return,
                        () = self.clock.sleep(self.timing.db_failure_pause) => {}
                    }
                    continue;
                }
            }
            let next = self.next_due().await;
            let nap = async {
                match next {
                    Some(at) => self.clock.sleep_until(u64::try_from(at).unwrap_or(0)).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = stop.cancelled() => return,
                () = self.wake.notified() => {}
                () = nap => {}
            }
        }
    }

    /// Finishes (state `done`, content wiped, no event) the deferred newsletter rows that arrived more than
    /// [`NEWSLETTER_KEEP_MS`] ago; how many. The age is the row's arrival, so no take-up restarts the clock (L4).
    pub(crate) async fn finish_old_newsletters(&self) -> Result<usize, StoreError> {
        let oldest = self.now().saturating_sub(NEWSLETTER_KEEP_MS);
        self.store
            .write("finish old newsletter rows", move |tx| {
                store::exec(
                    tx,
                    "finish old newsletter rows",
                    "UPDATE inbox SET state = 'done', message = NULL, meta = NULL, parked_until = 0 \
                     WHERE state = 'deferred' AND origin = 'newsletter' AND received_at < ?1",
                    [oldest],
                )
            })
            .await
    }

    /// Makes the rows a lower map version deferred `new` again (attempts cleared); how many. The age
    /// the poison rule counts restarts at this moment (`aged_from`, fix F12): a row that waited longer
    /// than [`MAX_AGE_MS`] for a map that can try it is not poisoned at its first failure.
    pub(crate) async fn take_up_deferred(&self) -> Result<usize, StoreError> {
        let now = self.now();
        self.store
            .write("take up deferred inbox rows", move |tx| {
                store::exec(
                    tx,
                    "take up deferred inbox rows",
                    "UPDATE inbox SET state = 'new', attempts = 0, parked_until = 0, aged_from = ?2 \
                     WHERE state = 'deferred' AND map_version < ?1",
                    params![MAP_VERSION, now],
                )
            })
            .await
    }

    /// When the first parked row becomes due again.
    async fn next_due(&self) -> Option<i64> {
        let now = self.now();
        self.store
            .read(move |conn| {
                store::first(
                    conn,
                    "read parked inbox rows",
                    "SELECT min(parked_until) FROM inbox WHERE state = 'new' AND parked_until > ?1",
                    [now],
                    |r| r.get::<_, Option<i64>>(0),
                )
            })
            .await
            .ok()
            .flatten()
            .flatten()
    }
}

#[async_trait]
impl Notes for Stage2 {
    fn committed(&self, done: &Committed) {
        if done.inserted > 0 {
            self.backlog.send_modify(|n| *n += done.inserted);
            self.wake();
        }
    }

    fn failed(&self) {}

    async fn room(&self) {
        let mut backlog = self.backlog.subscribe();
        let max = self.timing.max_backlog;
        let wait = async {
            loop {
                if *backlog.borrow_and_update() < max {
                    return;
                }
                if backlog.changed().await.is_err() {
                    return;
                }
            }
        };
        tokio::select! {
            () = wait => {}
            () = self.clock.sleep(self.timing.room_patience) => {
                tracing::warn!(backlog = self.backlog(), "stage 2 is behind; the hook goes on");
            }
        }
    }
}

/// One row with what the zone needs of its outcome.
struct Planned {
    arrival: i64,
    /// The wall clock (ms) the row arrived: a waiting delete's 45 days count from it (M2).
    received_at: i64,
    attempts: u32,
    result: Result<Mapped, Failure>,
    /// The row has failed for too long to be tried again.
    exhausted: bool,
    placeholder: Option<BackendEvent>,
}

impl Planned {
    fn new(row: Row, result: Result<Mapped, Failure>, now: i64) -> Self {
        let exhausted = result.is_err()
            && (row.attempts.saturating_add(1) >= MAX_ATTEMPTS
                // The age counts from the arrival or from the last take-up, whichever is later (F12).
                || now.saturating_sub(row.received_at.max(row.aged_from)) > MAX_AGE_MS);
        let placeholder = exhausted.then(|| map::poison_placeholder(&row)).flatten();
        Self {
            arrival: row.arrival,
            received_at: row.received_at,
            attempts: row.attempts,
            result,
            exhausted,
            placeholder,
        }
    }
}

/// `PARK_MIN * 2^(n-1)`, capped.
fn backoff(attempts: u32) -> Duration {
    let shift = attempts.saturating_sub(1).min(16);
    PARK_MIN.saturating_mul(1 << shift).min(PARK_MAX)
}

/// Due rows in arrival order.
fn load(conn: &Connection, now: i64) -> Result<Vec<Row>, StoreError> {
    store::rows(
        conn,
        "read inbox",
        "SELECT arrival, chat, sender, id, origin, received_at, catch_up, message, meta, attempts, \
                aged_from \
         FROM inbox WHERE state = 'new' AND parked_until <= ?1 ORDER BY arrival LIMIT ?2",
        params![now, i64::try_from(MAX_ROWS).unwrap_or(50)],
        |r| {
            Ok(Row {
                arrival: r.get(0)?,
                chat: r.get(1)?,
                sender: r.get(2)?,
                id: r.get(3)?,
                origin: Origin::parse(&r.get::<_, String>(4)?).unwrap_or(Origin::E2e),
                received_at: r.get(5)?,
                catch_up: r.get::<_, i64>(6)? != 0,
                message: r.get(7)?,
                meta: r.get(8)?,
                attempts: r.get(9)?,
                aged_from: r.get(10)?,
            })
        },
    )
}

/// Rows waiting for this stage and not parked.
fn backlog_count(conn: &Connection, now: i64) -> Result<usize, StoreError> {
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM inbox WHERE state = 'new' AND parked_until <= ?1",
            [now],
            |r| r.get(0),
        )
        .map_err(sql("count inbox rows"))?;
    Ok(usize::try_from(n).unwrap_or(0))
}

/// The zone body: everything a step does, so that it commits as one.
fn apply(
    tx: &kourier_db::rusqlite::Transaction<'_>,
    now: i64,
    plan: &[Planned],
) -> Result<Report, StoreError> {
    let mut report = Report {
        taken: plan.len(),
        ..Report::default()
    };
    // Whether a push can still state a hint or a name (W4 fixes F3, F4): read once per step, and only when a row has an
    // event to note. Without a push the live path notes nothing.
    let mut push_open: Option<bool> = None;
    for item in plan {
        match &item.result {
            Ok(mapped) if mapped.deferred => {
                defer(tx, item.arrival)?;
                report.deferred += 1;
            }
            Ok(mapped) if mapped.waits_for_members.is_some() => {
                wait_for_members(
                    tx,
                    item,
                    mapped.waits_for_members.as_ref(),
                    now,
                    &mut report,
                )?;
            }
            Ok(mapped) => {
                if !mapped.events.is_empty() {
                    // The sender's own name, when it is not what the core was told (W6a2, B8a, D8): in the same row as
                    // the message, after the merges, and recorded in this zone.
                    let named = match &mapped.push_name {
                        Some(sender) => {
                            let mut events = mapped.events.clone();
                            crate::names::state_push_name(
                                tx,
                                &mut events,
                                &sender.user,
                                &sender.name,
                            )?
                            .then_some(events)
                        }
                        None => None,
                    };
                    if named.is_some() {
                        report.names += 1;
                    }
                    let events: &[BackendEvent] = named.as_deref().unwrap_or(&mapped.events);
                    // A message of the peer or ours from another device proves the chat started on the phone
                    // (W5a, D7): noted in the zone that hands the message over, BEFORE the chat's features are
                    // stated in it (D9), so the peer's first message lifts `NotAccepted` in its own row.
                    crate::send::started::note_messages(tx, events)?;
                    // The chats are stated with what the phone said about them, and a counting message of an
                    // archived chat brings it back when the phone's setting says so (W6a1, H5, H6).
                    match crate::appstate::zone::prepare(tx, events)? {
                        Some(stated) => append(tx, &stated, now)?,
                        None => append(tx, events, now)?,
                    }
                    report.events += 1;
                    // The messages of others are noted for the history's unread hint (W4, O2), and the titles and
                    // push names it states for the history delivery (F3), in the zone that hands them over: a kill
                    // leaves both or neither. Only while a push can still state them (F4).
                    let open = match push_open {
                        Some(open) => open,
                        None => *push_open.insert(history::live::wanted(tx)?),
                    };
                    if open {
                        history::live::record(tx, events)?;
                        history::named::record(tx, &history::named::notes_of(events), now)?;
                    }
                }
                // The record of an announced chunk goes in the zone that finishes its row (W4, H1): a
                // kill leaves both or neither, never a finished notice nobody counted.
                if let Some(id) = &mapped.announced
                    && announce(tx, id, now)?
                {
                    report.announced += 1;
                }
                finish(tx, item.arrival)?;
                report.finished += 1;
            }
            Err(_) if item.exhausted => {
                if let Some(event) = &item.placeholder {
                    let events = std::slice::from_ref(event);
                    crate::send::started::note_messages(tx, events)?;
                    match features::with_statements(tx, events)? {
                        Some(stated) => append(tx, &stated, now)?,
                        None => append(tx, events, now)?,
                    }
                    report.events += 1;
                }
                finish(tx, item.arrival)?;
                report.poisoned += 1;
            }
            Err(_) => {
                let attempts = item.attempts.saturating_add(1);
                let until = now.saturating_add(
                    i64::try_from(backoff(attempts).as_millis()).unwrap_or(i64::MAX),
                );
                store::exec(
                    tx,
                    "park an inbox row",
                    "UPDATE inbox SET attempts = ?2, parked_until = ?3 WHERE arrival = ?1 AND state = 'new'",
                    params![item.arrival, attempts, until],
                )?;
                report.parked += 1;
            }
        }
    }
    // The notes nothing needs any more go in this zone too (F4; an index serves the prune).
    if report.events > 0 {
        history::live::tidy(tx, now)?;
    }
    // The seam of D3: everything written, nothing committed.
    fault::hit(fault::STAGE2_COMMIT)?;
    report.remaining = backlog_count(tx, now)?;
    Ok(report)
}

/// An admin's delete of `group` that waits for the group's member list (module docs). Looks at the group's row in THIS zone:
/// a list stated since the mapper read the facts leaves the row as it is (due at once, mapped again with the new facts by
/// the next step); otherwise the row is finished with no event when we are no member of the group or its 45th day came
/// (counted from its arrival: a take-up by a newer map restarts nothing, M2), else parked, with no attempt counted.
fn wait_for_members(
    tx: &kourier_db::rusqlite::Transaction<'_>,
    item: &Planned,
    group: Option<&kourier_model::ConvKey>,
    now: i64,
    report: &mut Report,
) -> Result<(), StoreError> {
    let Some(group) = group else {
        return Ok(());
    };
    let state = crate::store::people::group(tx, group)?;
    if state.members_stated {
        return Ok(());
    }
    if state.not_member || now.saturating_sub(item.received_at) >= MAX_AGE_MS {
        finish(tx, item.arrival)?;
        report.ended += 1;
        report.finished += 1;
        return Ok(());
    }
    let until = item.received_at.saturating_add(MAX_AGE_MS);
    if crate::store::people::wait_for_members(tx, item.arrival, group, until)? {
        report.waiting += 1;
    }
    Ok(())
}

/// ONE `core_outbox` row for all the events of one inbox row, in order: a kill between them cannot
/// split them, and the core applies them as one envelope.
fn append(
    tx: &kourier_db::rusqlite::Transaction<'_>,
    events: &[BackendEvent],
    now: i64,
) -> Result<(), StoreError> {
    let row = encode_outbox_row(events)
        .map_err(|_| error::invalid("a stage 2 event could not be encoded"))?;
    outbox::append(tx, &row, now)
        .map(drop)
        .map_err(|_| error::database("append stage 2 events failed"))
}

/// Records a chunk of the message history our phone announced (once per id: a take-up of the same notice
/// by a newer map, or a second row for it, changes nothing). `true` when the id is new.
fn announce(
    tx: &kourier_db::rusqlite::Transaction<'_>,
    id: &str,
    now: i64,
) -> Result<bool, StoreError> {
    store::exec(
        tx,
        "record an announced history chunk",
        "INSERT INTO history_announced(msg_id, announced_at) VALUES (?1, ?2) \
         ON CONFLICT (msg_id) DO NOTHING",
        params![id, now],
    )
    .map(|changed| changed > 0)
}

/// Sets the row `done` and wipes its content.
fn finish(tx: &kourier_db::rusqlite::Transaction<'_>, arrival: i64) -> Result<(), StoreError> {
    store::exec(
        tx,
        "finish an inbox row",
        "UPDATE inbox SET state = 'done', message = NULL, meta = NULL, parked_until = 0, members_wait = NULL \
         WHERE arrival = ?1 AND state = 'new'",
        [arrival],
    )
    .map(drop)
}

/// Keeps the row, with its content, for a later build's map (state `deferred`).
fn defer(tx: &kourier_db::rusqlite::Transaction<'_>, arrival: i64) -> Result<(), StoreError> {
    store::exec(
        tx,
        "defer an inbox row",
        "UPDATE inbox SET state = 'deferred', parked_until = 0, members_wait = NULL, map_version = ?2 \
         WHERE arrival = ?1 AND state = 'new'",
        params![arrival, MAP_VERSION],
    )
    .map(drop)
}

/// The store's facts the group revokes among `wants` (the rows' chat and sender ids as the inbox holds them) are judged by.
/// A chat or sender that is no group or person reads as "nothing known": the map decides without.
fn group_facts(
    conn: &Connection,
    wants: &[(String, String)],
) -> Result<map::GroupFacts, StoreError> {
    let mut asked = Vec::new();
    for (chat, sender) in wants {
        let group = chat
            .parse::<wacore_binary::Jid>()
            .ok()
            .and_then(|jid| map::conv_of(&jid))
            .filter(|conv| matches!(conv, kourier_model::ConvKey::Group(_)));
        let person = sender
            .parse::<wacore_binary::Jid>()
            .ok()
            .and_then(|jid| map::person_of(&jid));
        if let (Some(group), Some(person)) = (group, person) {
            asked.push((group, person));
        }
    }
    store::people::group_facts(conn, &asked)
}

/// Our own ids from the device row.
fn own_ids(device: &Device) -> OwnIds {
    let id = |scheme: &str, jid: Option<&wacore_binary::Jid>| {
        jid.and_then(|jid| kourier_model::UserId::from_parts(scheme, jid.user.as_str()).ok())
    };
    OwnIds {
        lid: id("lid", device.lid.as_ref()),
        pn: id("pn", device.pn.as_ref()),
    }
}

#[cfg(test)]
mod tests;
