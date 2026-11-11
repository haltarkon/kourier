//! The write job (`JobKind::StorageWrite`, A10c chunk C2; `target/next/a10-refresh.md` §3.4 as the
//! hand-off changed it, `a10b-build.md` §6): one run looks at the phone's manifest, judges a write
//! left in flight, merges what the phone wrote, plans what is left of the intents and sends it.
//!
//! The job is `needs_online` (V1): a live `identity` intent meets the phone's queued Verified syncs
//! (`safety::apply_remote_state`) before any write carries it.
//!
//! The steps of a run, in this order:
//! 1. Zone: intents older than 30 days are dropped ([`intents::drop_stale`], Y3); the switch, the
//!    `storage_generation` and whether a row is in flight are read. No row, and the switch off or
//!    no intent: the run ends with no request. No row and the rate spent ([`rate`], BQ4: 6 writes
//!    per 10 minutes, 30 per hour): the run ends with no request, the kind's hold and a
//!    [`Done::RepeatAt`] of the time a place frees; the intents wait and go out as one write.
//! 2. Credentials; the manifest UNCONDITIONALLY (BQ7: its identifiers are the only complete list).
//!    404: a linked device never creates one, the intents stay, it looks again in an hour.
//! 3. `storage::merge_manifest`, the ONE merge path it shares with `storage::sync`: a write that
//!    did not land is judged before the items are read (review N1 (a)), a landed one INSIDE the
//!    first merge zone, with no await before the last merge zone of that version (review B1). A
//!    row that path leaves (the manifest is behind the row, or not newer than the local version,
//!    so no merge zone runs) is judged right after it: nothing is read and nothing is merged in
//!    between. The row is resolved in every case BEFORE the switch is looked at again (M2).
//! 4. Zone: the switch off: finished. No intent left: finished, a merge alone never causes a
//!    write. The generation moved during the awaits (a `storage::reset`): nothing goes out under
//!    the old key.
//! 5. In the same zone: [`super::plan`] (pure) on the fetched manifest; what it left
//!    ([`super::Plan::left`]): `NotListed` and `Unreadable` keep the intent, the others are deleted
//!    ([`intents::drop_left`]). A refused plan keeps every intent.
//! 6. In the same zone: the rate is looked at again (a row left in flight was judged after step 1
//!    looked), then [`super::write_in_flight`] and the time of the write ([`rate::record`]). So
//!    plan, row, time and left intents are one moment. Failpoint
//!    [`FAILPOINT_BEFORE_STORAGE_WRITE_PUT`].
//! 7. The PUT of the sealed [`proto::WriteOperation`] (`PUT /v1/storage/`, 30 s).
//! 8. 200: failpoint [`FAILPOINT_AFTER_STORAGE_WRITE_PUT`], BQ11's counts (`storage::counts`:
//!    the written version and count, one more write; memory only), then the zone of the 200:
//!    [`intents::land_written`] (the written records, the version and the landing in one
//!    transaction) and [`after_landing`] (the fetch-latest sync; a landing renews its hour, D4);
//!    failpoint [`FAILPOINT_AFTER_STORAGE_WRITE_COMMIT`].
//! 9. THE ANSWER decides whether the server may have applied the write (R3). Refused BEFORE the
//!    write (the server checks the version and the size first): 409, 400, 401/403 (after the one
//!    new-credentials try), 413, 429 and every other status below 500: the body is never decoded
//!    (M1), [`intents::write_not_landed`] (the server applied nothing, the time of the write is
//!    taken back). 409 repeats 1 s / 5 s / 30 s later by the conflict count, then with the normal
//!    backoff; the next run's fetch is the re-fetch. 401 or 403: the credentials again, once, and
//!    the same body only while the switch is still on and the generation unchanged (V3: else the
//!    first answer is judged); then like any other status. 429: the server's `Retry-After` (at
//!    least a second, D3), remembered as the kind's hold. Any other such status: backoff. A LOST ANSWER: a transport error, a timeout, or ANY 5xx (a
//!    gateway answers 500-599 after the Storage Service may have applied the PUT): the write may
//!    be on the server, the ROW IS KEPT, the time of the write COUNTS (no [`rate::forget`]), the
//!    failure backoff applies, and the next run judges the row by the freshly fetched manifest
//!    (landed: no second PUT; not landed: written again under new ids; a manifest that moved on
//!    with none of our ids but still listing an id our write deleted is NOT landed, D1).
//!
//! EVERY FAILURE IS A [`Done::RepeatAt`], capped at an hour (Y3): `jobs::is_permanent` would drop a
//! job at a 4xx or an `Error::Decode` and orphan live intents. Only what stops every job (the
//! database, the core gone) stops this one. `Done::RepeatAt` resets `jobs.attempts`, so the counts
//! live in the job's payload ([`Tries`]). A run never sleeps (M1): the waits are the row's
//! `next_at`.
//!
//! The second job of this module is [`run_fetch_latest`] (`JobKind::FetchLatestSync`, M8): the
//! sync that tells our other devices to fetch the manifest after a write of ours landed.
//!
//! What it logs: counts, versions and statuses (architecture §9.1). The words are Storage Service
//! words on purpose: this module is in `kourier-signal`.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Method, StatusCode};
use kourier_backend_api::clock::duration_ms;
use kourier_db::rusqlite::Connection;
use prost::Message as _;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::{InFlight, Input, Plan, Refusal};
use crate::actor::Shared;
use crate::crypto::{GCM_IV_LEN, encrypt_gcm};
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::{HttpsHost, HttpsRequest, HttpsResponse, SignalTransport};
use crate::proto;
use crate::send::content;
use crate::send::{self, Delivery, Outgoing};
use crate::storage::intents::{self, Landing, Resolved};
use crate::storage::{
    Credentials, Fetched, REQUEST_TIMEOUT, credentials, fetch_manifest, fill_targets, item_key,
    local_state, manifest_key, merge_manifest,
};

mod rate;

/// The in-flight row is committed, the PUT has not gone out (§3.6).
pub(crate) const FAILPOINT_BEFORE_STORAGE_WRITE_PUT: &str = "before_storage_write_put";
/// The server answered 200, the zone of the 200 has not run (§3.6: the row decides after a kill).
pub(crate) const FAILPOINT_AFTER_STORAGE_WRITE_PUT: &str = "after_storage_write_put";
/// The zone of the 200 committed.
pub(crate) const FAILPOINT_AFTER_STORAGE_WRITE_COMMIT: &str = "after_storage_write_commit";

/// `PUT` of a write (the trailing slash is the route; the mock's and the server's).
const WRITE_PATH: &str = "/v1/storage/";
/// The longest wait of a failed run (Y3).
const LONGEST_WAIT_MS: i64 = 3_600_000;
/// How long a run waits when no manifest exists, or when only intents a later record decides are
/// left (`NotListed`, `Unreadable`): nothing it could do helps, the phone has to write first.
const LOOK_AGAIN_MS: i64 = 3_600_000;
/// The waits after the first conflicts (M1); then the normal backoff, never shorter than the last.
const CONFLICT_WAITS_MS: [i64; 3] = [1_000, 5_000, 30_000];
/// The shortest wait after a failure: a run costs two requests before it can fail again.
const SHORTEST_FAILURE_WAIT_MS: i64 = 1_000;
/// When an intent newer than the write that just ended is written: the debounce of a click.
const NEXT_WRITE_MS: i64 = 1_000;
/// A 429 without a `Retry-After` (Desktop's `DEFAULT_RETRY_AFTER`).
const DEFAULT_RETRY_AFTER_MS: i64 = 60_000;
/// How long the phone is told to fetch before the news counts as old (§3.4 step 10).
const FETCH_LATEST_MAX_AGE: Duration = Duration::from_secs(3600);

/// The job's payload: what `Done::RepeatAt` would forget with `jobs.attempts`. An unreadable
/// payload counts as none: it decides waits only and must not stop the job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Tries {
    /// 409s since the last write that landed.
    #[serde(default)]
    conflicts: u32,
    /// Other failures since then.
    #[serde(default)]
    failures: u32,
}

impl Tries {
    fn of(job: &Job) -> Self {
        job.payload
            .as_deref()
            .and_then(|payload| serde_json::from_slice(payload).ok())
            .unwrap_or_default()
    }

    fn store(self, conn: &Connection, job: &Job) -> Result<()> {
        let payload = serde_json::to_vec(&self).map_err(|_| Error::Decode("a job's counts"))?;
        jobs::set_payload(conn, job, &payload)
    }
}

/// The wait after the `conflicts`-th 409 in a row (M1).
fn conflict_wait_ms(conflicts: u32) -> i64 {
    let index = usize::try_from(conflicts.max(1) - 1).unwrap_or(usize::MAX);
    CONFLICT_WAITS_MS.get(index).copied().unwrap_or_else(|| {
        failure_wait_ms(conflicts).max(CONFLICT_WAITS_MS[CONFLICT_WAITS_MS.len() - 1])
    })
}

/// The wait after the `failures`-th other failure: the normal backoff (`jobs::backoff`, as
/// [`jobs::retry`] counts), capped at [`LONGEST_WAIT_MS`] (Y3).
fn failure_wait_ms(failures: u32) -> i64 {
    let backoff = jobs::backoff(failures.saturating_add(1));
    i64::try_from(duration_ms(backoff))
        .unwrap_or(i64::MAX)
        .clamp(SHORTEST_FAILURE_WAIT_MS, LONGEST_WAIT_MS)
}

/// The server's `Retry-After` of a 429 (seconds), capped at [`LONGEST_WAIT_MS`] (Y3) and never
/// under [`SHORTEST_FAILURE_WAIT_MS`] (D3: Desktop's `MINIMAL_RETRY_AFTER`; a `Retry-After: 0`
/// would repeat the run at once with its time refunded and no failure counted, a hot loop).
fn retry_after_ms(response: &HttpsResponse) -> i64 {
    response
        .headers
        .get(http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|seconds| *seconds >= 0)
        .map_or(DEFAULT_RETRY_AFTER_MS, |seconds| {
            seconds.saturating_mul(1000)
        })
        .clamp(SHORTEST_FAILURE_WAIT_MS, LONGEST_WAIT_MS)
}

/// [`Done::RepeatAt`] `wait_ms` after `now`, capped at an hour (Y3) and never before the hold of
/// the kind ends (`Done::RepeatAt` replaces `next_at`, so nothing else would keep it).
fn repeat(conn: &Connection, now: i64, wait_ms: i64) -> Result<Done> {
    let at = now.saturating_add(wait_ms.min(LONGEST_WAIT_MS));
    Ok(Done::RepeatAt(at.max(jobs::due_floor(
        conn,
        JobKind::StorageWrite,
        now,
    )?)))
}

/// What a zone that lands a write queues after the landing, in that zone: a write that landed
/// tells our other devices to fetch (M8; also a landing found after a kill, which a plain
/// `storage_sync` can be the one to find); a write planned before a `storage::reset` asks for
/// the sync again (M4: the intents stay, the full sync merges against them); a write already
/// judged queues nothing.
pub(crate) fn after_landing(conn: &Connection, landing: Landing, now: i64) -> Result<()> {
    match landing {
        Landing::Landed { .. } => {
            jobs::request(
                conn,
                JobKind::FetchLatestSync,
                "",
                now,
                Some(FETCH_LATEST_MAX_AGE),
            )?;
            // D4: `request` sets the expiry only when it inserts the row. A row queued already
            // (the account is offline or still catching up) must not run out before the news of
            // THIS landing is old: a landing renews its hour.
            let expires_at = now.saturating_add(
                i64::try_from(duration_ms(FETCH_LATEST_MAX_AGE)).unwrap_or(i64::MAX),
            );
            conn.prepare_cached(
                "UPDATE jobs SET expires_at = ?3 WHERE kind = ?1 AND key = ?2 AND expires_at < ?3",
            )?
            .execute((JobKind::FetchLatestSync.as_str(), "", expires_at))?;
            Ok(())
        }
        Landing::Stale => jobs::request(conn, JobKind::StorageSync, "", now, None),
        Landing::AlreadyJudged => Ok(()),
    }
}

/// Runs the fetch-latest job (`maintenance::run_job`, M8): `SyncMessage.fetchLatest
/// { STORAGE_MANIFEST }` to our own other devices, the pattern of `delete_sync::run`. The write it
/// reports landed whatever the switch says now (a switch that went off during the PUT still states
/// what was written, Y6), so the switch is not looked at. A failed send is the runner's retry
/// (`jobs::retry`: backoff, and the row's hour decides when the news is too old). Counts only.
pub(crate) async fn run_fetch_latest<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let sync = content::fetch_latest_storage_manifest();
    let ctx = shared.send_context(&account, socket);
    send::send_to(
        &ctx,
        account.aci.into(),
        &Outgoing {
            content: &sync,
            timestamp: shared.unique_timestamp(),
            delivery: Delivery::TRANSCRIPT,
            local: None,
            group: None,
        },
    )
    .await?;
    tracing::debug!("our other devices are told to fetch the storage manifest");
    Ok(Done::Finished)
}

/// Runs the write job (`maintenance::run_job`). Never a permanent error (Y3): what fails is
/// repeated, capped at an hour; only what stops every job stops it.
pub(crate) async fn run<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let tries = Tries::of(job);
    match attempt(shared, socket, job, tries).await {
        Err(e @ (Error::CoreGone | Error::Db(_))) => Err(e),
        Err(e) => failed(shared, job, tries, &e),
        done => done,
    }
}

/// One more failure: a row in flight stays what it is (the next run judges it), the intents stay.
fn failed<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    job: &Job,
    tries: Tries,
    error: &Error,
) -> Result<Done> {
    tracing::warn!(
        failures = tries.failures.saturating_add(1),
        error = %error,
        "a storage write failed; it looks again"
    );
    back_off(shared, job, tries)
}

/// The wait of one more failure, counted in the job's payload.
fn back_off<T: SignalTransport>(shared: &Rc<Shared<T>>, job: &Job, tries: Tries) -> Result<Done> {
    let tries = Tries {
        failures: tries.failures.saturating_add(1),
        ..tries
    };
    let now = shared.now();
    shared.db.zone(|tx| {
        tries.store(tx, job)?;
        repeat(tx, now, failure_wait_ms(tries.failures))
    })
}

/// What step 1 read.
struct Start {
    in_flight: bool,
    on: bool,
    any_intent: bool,
    generation: i64,
    /// The rate is spent until this time (ms): [`rate::wait_until`].
    limited_until: Option<i64>,
}

/// What the zone of steps 4 to 6 came to.
enum Step {
    /// The switch is off, or no intent is left: nothing to write.
    Nothing,
    /// A `storage::reset` ran during the awaits: nothing goes out under the old key.
    GenerationMoved,
    /// The rate is spent (a row left in flight was judged first): the write waits until this time.
    Limited(i64),
    /// A row stands (it was just resolved, so this is a guard): never a second one.
    RowStands,
    /// The plan was refused: every intent stays.
    Refused(Refusal),
    /// Only intents a later record decides are left.
    Kept(usize),
    /// A write to send: its row is committed.
    Write(Box<Write>),
}

/// A write on its way: the plan and the in-flight row made of it.
struct Write {
    plan: Plan,
    row: InFlight,
    /// The row of its time in `storage_write_times` ([`rate::record`]).
    time: i64,
}

/// One run; a failure of it is [`failed`].
async fn attempt<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
    tries: Tries,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let now = shared.now();
    let start = shared.db.zone(|tx| -> Result<Start> {
        intents::drop_stale(tx, &account, now, &shared.wake)?;
        Ok(Start {
            in_flight: super::in_flight_exists(tx)?,
            on: intents::switch_is_on(tx)?,
            any_intent: intents::any_intent(tx)?,
            generation: intents::storage_generation(tx)?,
            limited_until: rate::wait_until(tx, now)?,
        })
    })?;
    // A row left in flight is judged whatever the switch says (M2): it may be on the server.
    if !start.in_flight && !(start.on && start.any_intent) {
        return Ok(Done::Finished);
    }
    // Nothing to judge and no place free: no request is made at all (a row in flight is judged
    // first, it costs no write).
    if let (false, Some(until)) = (start.in_flight, start.limited_until) {
        return wait_for_the_rate(shared, now, until);
    }
    let local = shared.db.read(local_state)?;
    let credentials = credentials(shared, socket).await?;
    let manifest = match fetch_manifest(shared, &credentials, None).await? {
        Fetched::Manifest(manifest) => manifest,
        Fetched::Missing => {
            tracing::info!("storage service: no manifest to write on; a linked device makes none");
            let now = shared.now();
            return shared.db.read(|c| repeat(c, now, LOOK_AGAIN_MS));
        }
        // Only an answer to a conditional fetch, which this is not.
        Fetched::Unchanged => {
            return Err(Error::Status {
                what: "storage manifest",
                status: StatusCode::NO_CONTENT.as_u16(),
            });
        }
    };
    let fetched = merge_manifest(shared, &account, &local, &credentials, &manifest).await?;
    // The row the merge path left: no merge zone ran for this manifest (it is not newer than the
    // local version, and so behind the row, or the row's own version was merged before): nothing
    // of ours waits to be merged, and there is no window between a landing and a merge (review B1).
    if shared.db.read(super::in_flight_exists)? {
        let now = shared.now();
        shared.db.zone(|tx| -> Result<()> {
            if let Resolved::Found { landing, .. } =
                intents::resolve_in_flight(tx, &account, &fetched, now, &shared.wake)?
            {
                after_landing(tx, landing, now)?;
            }
            Ok(())
        })?;
    }
    let now = shared.now();
    let step = shared
        .db
        .zone(|tx| plan_and_row(tx, shared, &account, &fetched, start.generation, now))?;
    let write = match step {
        Step::Nothing => return Ok(Done::Finished),
        Step::GenerationMoved | Step::RowStands => {
            return shared.db.read(|c| repeat(c, now, NEXT_WRITE_MS));
        }
        Step::Limited(until) => return wait_for_the_rate(shared, now, until),
        Step::Refused(refusal) => {
            tracing::warn!(%refusal, "a planned storage write was refused; nothing was sent");
            return back_off(shared, job, tries);
        }
        Step::Kept(kept) => {
            tracing::info!(
                kept,
                "storage changes wait for a record the phone has to write"
            );
            return shared.db.read(|c| repeat(c, now, LOOK_AGAIN_MS));
        }
        Step::Write(write) => write,
    };
    kourier_db::failpoint::hit(FAILPOINT_BEFORE_STORAGE_WRITE_PUT);
    let body = Bytes::from(sealed_write(&local.storage_key, &write.plan));
    let response = match put_write(shared, socket, &credentials, body, write.row.generation).await {
        Ok(response) => response,
        // The answer is lost, the write may be on the server: the row decides at the next run.
        Err(e @ (Error::CoreGone | Error::Db(_))) => return Err(e),
        Err(e) => return failed(shared, job, tries, &e),
    };
    if response.status.is_success() {
        kourier_db::failpoint::hit(FAILPOINT_AFTER_STORAGE_WRITE_PUT);
        // BQ11's counts (debug-only line, `storage::counts`, M1): the written manifest is the
        // newest this process saw, and one more PUT was answered 200 (no zone, no await).
        crate::storage::counts::written(
            write.plan.manifest.version,
            write.plan.manifest.identifiers.len(),
        );
        let now = shared.now();
        let done = shared
            .db
            .zone(|tx| written(tx, shared, &account, job, tries, &write, now))?;
        kourier_db::failpoint::hit(FAILPOINT_AFTER_STORAGE_WRITE_COMMIT);
        return Ok(done);
    }
    let status = response.status;
    // A 5xx is a lost answer (R3): a gateway answers 500-599 whether or not the Storage Service
    // applied the PUT. The row is kept (the next run judges it by the fetched manifest), the time
    // of the write counts (the retry counts again: fewer writes, never more), the failure
    // backoff applies.
    if status.is_server_error() {
        tracing::warn!(
            status = status.as_u16(),
            failures = tries.failures.saturating_add(1),
            "the storage service answered with a server error; the write may be on it, the next run judges it"
        );
        return back_off(shared, job, tries);
    }
    // A response that is not a 2xx and no 5xx: the server applied nothing (never a body decoded,
    // M1).
    let (tries, wait_ms, hold) = match status {
        StatusCode::CONFLICT => {
            let tries = Tries {
                conflicts: tries.conflicts.saturating_add(1),
                ..tries
            };
            tracing::info!(
                conflicts = tries.conflicts,
                "a storage write met a newer one"
            );
            (tries, conflict_wait_ms(tries.conflicts), false)
        }
        StatusCode::TOO_MANY_REQUESTS => (tries, retry_after_ms(&response), true),
        _ => {
            let tries = Tries {
                failures: tries.failures.saturating_add(1),
                ..tries
            };
            tracing::warn!(
                status = status.as_u16(),
                failures = tries.failures,
                "the storage service refused a write"
            );
            (tries, failure_wait_ms(tries.failures), false)
        }
    };
    let now = shared.now();
    shared.db.zone(|tx| -> Result<Done> {
        intents::write_not_landed(tx, &account, &write.row, now, &shared.wake)?;
        // An answer below 500 with a status: the server applied nothing, the write does not count.
        rate::forget(tx, write.time)?;
        if hold {
            jobs::pause_kind(tx, JobKind::StorageWrite, now, now.saturating_add(wait_ms))?;
        }
        // The switch went off during the PUT: `write_not_landed` dropped what was left.
        if !intents::any_intent(tx)? {
            return Ok(Done::Finished);
        }
        tries.store(tx, job)?;
        repeat(tx, now, wait_ms)
    })
}

/// The rate is spent (BQ4): the kind is held until `until` (remembered, so no helper makes the job
/// due earlier, critic Y4) and the job repeats then. The intents stay and go out together.
fn wait_for_the_rate<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    now: i64,
    until: i64,
) -> Result<Done> {
    tracing::info!(
        wait_ms = until.saturating_sub(now),
        "storage writes wait for the rate limit; the changes go out together"
    );
    shared.db.zone(|tx| {
        jobs::pause_kind(tx, JobKind::StorageWrite, now, until)?;
        repeat(tx, now, until.saturating_sub(now))
    })
}

/// Steps 4 to 6 in ONE zone: nothing can be filed between the plan and its row, and the told bases
/// are read after the fetched manifest was merged, because the merge moves them.
fn plan_and_row<T: SignalTransport>(
    tx: &mut kourier_db::Tx<'_>,
    shared: &Rc<Shared<T>>,
    account: &Account,
    fetched: &proto::ManifestRecord,
    generation: i64,
    now: i64,
) -> Result<Step> {
    if !intents::switch_is_on(tx)? {
        return Ok(Step::Nothing);
    }
    if intents::storage_generation(tx)? != generation {
        return Ok(Step::GenerationMoved);
    }
    if super::in_flight_exists(tx)? {
        return Ok(Step::RowStands);
    }
    fill_targets(tx, account)?;
    let live = intents::all_intents(tx)?;
    if live.is_empty() {
        return Ok(Step::Nothing);
    }
    let targets = super::touched_targets(&live);
    let stored = super::load_stored(tx, &targets)?;
    let told_archived = intents::told_archived(tx, account, &targets, now)?;
    let pin_limit = intents::pin_limit(tx)?;
    let plan = super::plan(
        &Input {
            fetched,
            stored: &stored,
            intents: &live,
            told_archived: &told_archived,
            pin_limit,
            device_id: u32::from(account.device_id),
        },
        &mut super::random_record_id,
    );
    let plan = match plan {
        Ok(plan) => plan,
        Err(refusal) => return Ok(Step::Refused(refusal)),
    };
    let dropped = intents::drop_left(tx, account, &plan.left, now, &shared.wake)?;
    if plan.is_empty() {
        let kept = live.len().saturating_sub(dropped);
        return Ok(if kept == 0 {
            Step::Nothing
        } else {
            Step::Kept(kept)
        });
    }
    if let Some(until) = rate::wait_until(tx, now)? {
        return Ok(Step::Limited(until));
    }
    let row = InFlight::of(&plan, generation, now);
    super::write_in_flight(tx, &row)?;
    let time = rate::record(tx, now)?;
    Ok(Step::Write(Box::new(Write { plan, row, time })))
}

/// The zone of a 200: what was written is stored and landed, the next steps are queued. Whatever
/// the landing says, what comes next is decided by what is left.
fn written<T: SignalTransport>(
    tx: &mut kourier_db::Tx<'_>,
    shared: &Rc<Shared<T>>,
    account: &Account,
    job: &Job,
    tries: Tries,
    write: &Write,
    now: i64,
) -> Result<Done> {
    let Write { plan, row, .. } = write;
    let landing = intents::land_written(tx, account, row, plan, now, &shared.wake)?;
    after_landing(tx, landing, now)?;
    tracing::info!(
        version = row.version,
        inserts = plan.inserts.len(),
        deletes = plan.deletes.len(),
        landed = matches!(landing, Landing::Landed { .. }),
        "storage write answered 200"
    );
    if tries != Tries::default() {
        Tries::default().store(tx, job)?;
    }
    let left_of_plan = |intent: &intents::Intent| {
        plan.left.iter().any(|left| {
            left.why.keeps_the_intent()
                && left.target == intent.target
                && left.field == intent.field
                && left.seq == intent.seq
        })
    };
    let live = intents::all_intents(tx)?;
    if super::in_flight_exists(tx)? || live.iter().any(|intent| !left_of_plan(intent)) {
        // A click made during the PUT, or a write that is not judged yet (the next run does).
        repeat(tx, now, NEXT_WRITE_MS)
    } else if live.is_empty() {
        Ok(Done::Finished)
    } else {
        repeat(tx, now, LOOK_AGAIN_MS)
    }
}

/// A new random IV of the record cipher.
fn random_iv() -> [u8; GCM_IV_LEN] {
    use rand::RngCore as _;
    let mut iv = [0u8; GCM_IV_LEN];
    rand::rng().fill_bytes(&mut iv);
    iv
}

/// The body of the PUT: the manifest sealed with the key of ITS version, each record with the key
/// of its new identifier (the fetched `recordIkm` when the manifest has one, else the legacy key:
/// `storage::item_key`, what the phone reads with), the deleted identifiers as they are.
fn sealed_write(storage_key: &[u8; 32], plan: &Plan) -> Vec<u8> {
    let version = plan.manifest.version;
    let manifest = Zeroizing::new(plan.manifest.encode_to_vec());
    let record_ikm = Some(plan.manifest.record_ikm.as_slice()).filter(|ikm| !ikm.is_empty());
    proto::WriteOperation {
        manifest: Some(proto::StorageManifest {
            version,
            value: encrypt_gcm(&manifest_key(storage_key, version), random_iv(), &manifest),
        }),
        insert_item: plan
            .inserts
            .iter()
            .map(|insert| proto::StorageItem {
                key: insert.key.clone(),
                value: encrypt_gcm(
                    &item_key(storage_key, record_ikm, &insert.key),
                    random_iv(),
                    &insert.record,
                ),
            })
            .collect(),
        delete_key: plan.deletes.clone(),
        clear_all: false,
    }
    .encode_to_vec()
}

/// The PUT, and once more with fresh credentials after a 401 or 403 (§3.4 step 9). A failure to
/// get them is a failure of the run: the row is kept and the next run judges it. V3: asking for
/// the credentials is an await, so the switch and `generation` (the one the row was planned under)
/// are looked at again right before the second PUT. The first PUT was refused (nothing reached the
/// server, Y6's reason to let a write in flight land does not hold), so when the switch went off or
/// a `storage::reset` ran meanwhile the first answer is returned: the non-2xx path takes the row
/// back, drops what the switch-off dropped and restates it, and sends nothing under the old key.
async fn put_write<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    credentials_at_hand: &Credentials,
    body: Bytes,
    generation: i64,
) -> Result<HttpsResponse> {
    let response = put_once(shared, credentials_at_hand, body.clone()).await?;
    if !matches!(
        response.status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
        return Ok(response);
    }
    let fresh = credentials(shared, socket).await?;
    // A read: the database is synchronous and nothing is awaited between it and the PUT.
    let unchanged = shared.db.read(|c| -> Result<bool> {
        Ok(intents::switch_is_on(c)? && intents::storage_generation(c)? == generation)
    })?;
    if !unchanged {
        return Ok(response);
    }
    put_once(shared, &fresh, body).await
}

async fn put_once<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    credentials: &Credentials,
    body: Bytes,
) -> Result<HttpsResponse> {
    let mut request = HttpsRequest::new(Method::PUT, HttpsHost::Storage, WRITE_PATH);
    request.timeout = REQUEST_TIMEOUT;
    request.body = Some(body);
    request
        .headers
        .insert(http::header::AUTHORIZATION, credentials.0.clone());
    request.headers.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    Ok(shared.net.https(request).await?)
}

#[cfg(test)]
mod tests;
