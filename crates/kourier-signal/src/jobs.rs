//! The backend job queue (§5.5 `jobs`, §6.12): network work that has to happen eventually and
//! must survive restarts, run on the authenticated socket with retries and backoff.
//!
//! Desktop runs the same work through its job queues (`SD/ts/jobs/JobQueue.std.ts`,
//! `singleProtoJobQueue.preload.ts` for sync requests): exponential backoff from
//! `SD/ts/util/exponentialBackoff.std.ts` (0 ms, 190 ms, then ×1.9 up to 15 minutes) and a
//! one-day retry budget for sync requests (`MAX_RETRY_TIME`).
//!
//! Jobs are rows keyed by `(kind, key)`: enqueueing an existing job is a no-op, so callers do not
//! track what is pending. One-shot jobs are deleted when they succeed or are dropped; a periodic
//! job (remote config) stays and is rescheduled. A job runs only while the authenticated socket
//! is up ([`run`] is aborted with it); a job that was in flight when the socket closed simply runs
//! again, so every job must be safe to repeat (a sync request repeats harmlessly, remote config
//! and capabilities are idempotent).

use std::convert::Infallible;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::clock::duration_ms;
use kourier_db::rusqlite::Connection;

use crate::actor::Shared;
use crate::error::{Error, Result};
use crate::net::{SignalTransport, TransportError};

/// Desktop's first backoffs (`FIRST_BACKOFFS`, ms) before the exponential part.
const FIRST_BACKOFFS_MS: [u64; 2] = [0, 190];
/// Growth per attempt (`BACKOFF_FACTOR`).
const BACKOFF_FACTOR: f64 = 1.9;
/// Longest wait between attempts (`MAX_BACKOFF`).
const MAX_BACKOFF: Duration = Duration::from_secs(15 * 60);
/// Most profile fetches that run at once. A history import or a first Storage Service sync
/// queues one per contact, hundreds, and one after another (a request, then the avatar from
/// the CDN) they took many minutes (the user's report, 2026-10-10). Desktop's
/// `ProfileService` runs 30 (`PROFILE_FETCH_CONCURRENCY`); fewer here, because the other
/// jobs wait for the slowest fetch of a batch (an avatar has up to 30 s).
const PROFILE_FETCH_BATCH: usize = 10;
/// How long every profile fetch waits after a 413 or 429 without a `Retry-After`
/// (Desktop's `DEFAULT_RETRY_AFTER`, `SD/ts/util/parseRetryAfter.std.ts`).
const PROFILE_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// What a job does. Stored as text in `jobs.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum JobKind {
    /// `GET /v2/config` with ETag; periodic (§6.4: at connect and every 2 h).
    RemoteConfig,
    /// `PUT /v1/devices/capabilities`, once per start (§6.12).
    Capabilities,
    /// `SyncMessage.Request` to our own devices; `key` is a [`SyncRequest`] (§6.5 step 8).
    SyncRequest,
    /// Read or viewed receipts of a durable `SendReceipts` command (§4.8); `key` is the command
    /// id with a digest of `payload`, what to send.
    Receipts,
    /// The `SyncMessage.read` of a durable `SyncReadMarker` command (§4.8).
    ReadSync,
    /// Delivery receipts owed for sealed messages (`delivery_receipts` table), batched.
    DeliveryReceipts,
    /// A sync transcript whose first attempt failed; `key` is the message id, `payload` the
    /// `Content`.
    SentTranscript,
    /// Deletes Message Send Log entries older than 14 days; periodic.
    MslPurge,
    /// Downloads and applies a contact-sync blob (§6.9); `payload` is the `SyncMessage.Contacts`.
    ContactSync,
    /// The link stops waiting for the phone's contact sync (§6.5 step 8); due at the deadline.
    ContactSyncWait,
    /// One Storage Service sync (§6.9): after link, at start, on `FetchLatest`, after new keys.
    StorageSync,
    /// Fetches one profile (§6.10); `key` is the service id string.
    ProfileFetch,
    /// Queues profile fetches of contacts not refreshed for 3 days (≤ 50); periodic, hourly.
    ProfileRefresh,
    /// Fetches one group and applies it (§6.8): a group from the Storage Service without state,
    /// a dirty group, a rebuilt core; `key` is the group id in hex.
    GroupRefresh,
    /// Sends a `DecryptionErrorMessage` for a message we could not decrypt (§6.6); `key` is
    /// `<sender>.<device>:<timestamp>`, `payload` the serialized message.
    RetryRequest,
    /// Answers a retry request (§6.14 item 17): resends the logged content, or a null message;
    /// `key` is `<requester>.<device>:<timestamp>`, `payload` a [`crate::retry::Resend`].
    Resend,
    /// Sends our change of a 1:1 chat's disappearing-message timer and its sync transcript
    /// ([`crate::timer`]); `key` is `<peer>:<timestamp>`, `payload` what to send.
    TimerUpdate,
    /// Prekey maintenance (§6.12, [`crate::keys`]): one-time keys by the server's counts, a new
    /// signed prekey and Kyber last-resort key by age; periodic (every start and every 2 days)
    /// with key `""`. Key `"pni"`: one forced upload of PNI keys after a number change.
    KeyMaintenance,
    /// The `SyncMessage.deleteForMe` of a durable `ClearConversation` or `DeleteAttachment`
    /// command ([`crate::delete_sync`]); `key` is the command id with a digest of `payload`,
    /// the message without its padding.
    DeleteSync,
    /// The `SyncMessage.attachmentBackfillRequest` of a durable `RequestAttachments` command
    /// ([`crate::backfill`]); `key` is the command id with a digest of `payload`, the
    /// message's conversation and reference (JSON). Expires an hour after the click.
    BackfillRequest,
}

impl JobKind {
    /// Every kind, for parsing.
    const ALL: [Self; 20] = [
        Self::RemoteConfig,
        Self::Capabilities,
        Self::SyncRequest,
        Self::Receipts,
        Self::ReadSync,
        Self::DeliveryReceipts,
        Self::SentTranscript,
        Self::MslPurge,
        Self::ContactSync,
        Self::ContactSyncWait,
        Self::StorageSync,
        Self::ProfileFetch,
        Self::ProfileRefresh,
        Self::GroupRefresh,
        Self::RetryRequest,
        Self::Resend,
        Self::TimerUpdate,
        Self::KeyMaintenance,
        Self::DeleteSync,
        Self::BackfillRequest,
    ];

    /// The `jobs.kind` value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RemoteConfig => "remote_config",
            Self::Capabilities => "capabilities",
            Self::SyncRequest => "sync_request",
            Self::Receipts => "receipts",
            Self::ReadSync => "read_sync",
            Self::DeliveryReceipts => "delivery_receipts",
            Self::SentTranscript => "sent_transcript",
            Self::MslPurge => "msl_purge",
            Self::ContactSync => "contact_sync",
            Self::ContactSyncWait => "contact_sync_wait",
            Self::StorageSync => "storage_sync",
            Self::ProfileFetch => "profile_fetch",
            Self::ProfileRefresh => "profile_refresh",
            Self::GroupRefresh => "group_refresh",
            Self::RetryRequest => "retry_request",
            Self::Resend => "resend",
            Self::TimerUpdate => "timer_update",
            Self::KeyMaintenance => "key_maintenance",
            Self::DeleteSync => "delete_sync",
            Self::BackfillRequest => "backfill_request",
        }
    }

    /// Lower runs first among due jobs: remote config before anything that may depend on it
    /// (Desktop refreshes it before sending the post-link sync requests).
    const fn priority(self) -> i64 {
        match self {
            Self::RemoteConfig => 0,
            // Before anything that sends: a peer who answers needs prekeys to answer with.
            Self::Capabilities | Self::KeyMaintenance => 1,
            // The phone's contact sync is what the link waits for (names in the list): ahead
            // of the group refreshes an imported archive queues, one per group (link
            // feedback L2: it waited ~9 s behind 50 refreshes at 150 ms round trip).
            Self::SyncRequest | Self::SentTranscript | Self::ContactSyncWait => 2,
            Self::ContactSync => 2,
            Self::TimerUpdate | Self::DeleteSync | Self::BackfillRequest => 2,
            Self::StorageSync | Self::GroupRefresh => 3,
            Self::RetryRequest | Self::Resend => 4,
            Self::ReadSync | Self::Receipts => 4,
            Self::DeliveryReceipts | Self::ProfileFetch => 5,
            Self::MslPurge | Self::ProfileRefresh => 6,
        }
    }

    /// How many due jobs of this kind [`run`] starts together. Only kinds whose jobs are
    /// independent of each other (one profile each) run more than one.
    const fn batch(self) -> usize {
        match self {
            Self::ProfileFetch => PROFILE_FETCH_BATCH,
            _ => 1,
        }
    }

    /// The job of a core durable command (`enqueue_command`).
    pub(crate) const fn is_command(self) -> bool {
        matches!(
            self,
            Self::Receipts | Self::ReadSync | Self::DeleteSync | Self::BackfillRequest
        )
    }

    /// Kinds that send a message to another account. While the server's send challenge
    /// stands they are held ([`hold_for_challenge`]): each would only be refused with the same
    /// 428. Sync messages to our own devices are not held; one that meets a 428 of its own
    /// waits like any challenged job.
    pub(crate) const fn sends_to_others(self) -> bool {
        matches!(
            self,
            Self::Receipts
                | Self::DeliveryReceipts
                | Self::RetryRequest
                | Self::Resend
                | Self::TimerUpdate
        )
    }

    /// The kinds that [send to others](Self::sends_to_others), as an SQL list.
    fn sql_sends_to_others() -> String {
        Self::ALL
            .into_iter()
            .filter(|kind| kind.sends_to_others())
            .map(|kind| format!("'{}'", kind.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Kinds whose new requests ([`request`], and a new payload in [`enqueue_payload`]) carry
    /// new work (a newer manifest, storage key, contact-sync blob or profile key): the run in
    /// progress may have read the old state, so the job runs once more after it
    /// (`jobs.requests`, migration 0006).
    pub(crate) const fn reruns_on_request(self) -> bool {
        matches!(
            self,
            Self::StorageSync | Self::ProfileFetch | Self::ContactSync
        )
    }

    /// Receipts and read syncs wait until the account is through its catch-up (`Online`,
    /// §6.7 step 6; Desktop pauses its receipt queues until the incoming queue is empty).
    pub(crate) const fn needs_online(self) -> bool {
        matches!(
            self,
            Self::Receipts
                | Self::ReadSync
                | Self::DeliveryReceipts
                | Self::RetryRequest
                | Self::TimerUpdate
        )
    }

    /// The Storage Service sync, and the wait for the phone's contact sync, start only when
    /// a history transfer is over (`transfer.rs`; Desktop enables its storage service after
    /// the import, `background.preload.ts`): the archive is the older state, what the
    /// account's storage and the live messages say comes on top of it.
    pub(crate) const fn waits_for_transfer(self) -> bool {
        matches!(self, Self::StorageSync | Self::ContactSyncWait)
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == s)
    }

    /// The kinds that wait for a history transfer, as an SQL list.
    fn sql_transfer_held() -> String {
        Self::ALL
            .into_iter()
            .filter(|kind| kind.waits_for_transfer())
            .map(|kind| format!("'{}'", kind.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The kinds that may run while catching up, as an SQL list.
    fn sql_not_online() -> String {
        Self::ALL
            .into_iter()
            .filter(|kind| kind.needs_online())
            .map(|kind| format!("'{}'", kind.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The `SyncMessage.Request.Type` a [`JobKind::SyncRequest`] asks our phone for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum SyncRequest {
    /// The contact list (answered with a contacts attachment).
    Contacts,
    /// Read receipts, typing, link previews, unidentified-delivery indicators.
    Configuration,
    /// Blocked numbers, ACIs and groups.
    Blocked,
    /// The account entropy pool / master key (only when we lack the AEP).
    Keys,
}

impl SyncRequest {
    /// What every link asks for (`SD/ts/textsecure/syncRequests.preload.ts`).
    pub(crate) const AFTER_LINK: [Self; 3] = [Self::Contacts, Self::Configuration, Self::Blocked];

    const ALL: [Self; 4] = [
        Self::Contacts,
        Self::Configuration,
        Self::Blocked,
        Self::Keys,
    ];

    /// The `jobs.key` value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Contacts => "contacts",
            Self::Configuration => "configuration",
            Self::Blocked => "blocked",
            Self::Keys => "keys",
        }
    }

    /// Parses a `jobs.key`.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == s)
    }
}

/// How long Desktop keeps retrying a sync request (`singleProtoJobQueue` `MAX_RETRY_TIME`).
pub(crate) const SYNC_REQUEST_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// One due job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Job {
    /// Row id.
    pub(crate) id: i64,
    /// What to do.
    pub(crate) kind: JobKind,
    /// Kind-specific key.
    pub(crate) key: String,
    /// Failed attempts so far.
    pub(crate) attempts: u32,
    /// Dropped when a retry would start after this (ms).
    pub(crate) expires_at: Option<i64>,
    /// What a command job sends.
    pub(crate) payload: Option<Vec<u8>>,
    /// `jobs.requests` when the job was picked: a different value after the run means it was
    /// asked for again meanwhile ([`JobKind::reruns_on_request`]).
    pub(crate) requests: i64,
}

impl Job {
    /// The key as logs may show it (§9.1): service ids (profile fetches) only by their last
    /// four hex digits, retry keys without their sender, group ids by their first bytes.
    pub(crate) fn log_key(&self) -> String {
        match self.kind {
            JobKind::RetryRequest | JobKind::Resend | JobKind::TimerUpdate => self
                .key
                .rsplit_once(':')
                .map_or_else(|| "…".to_owned(), |(_, timestamp)| format!("…:{timestamp}")),
            JobKind::GroupRefresh => self.key.chars().take(8).collect(),
            _ => log_safe_key(&self.key),
        }
    }
}

/// A job key for logs: a service id becomes `aci:…1f2e`; other keys (sync request types,
/// command keys of ids and digests) are log-safe as they are.
fn log_safe_key(key: &str) -> String {
    use crate::service_id::SignalUser;
    use libsignal_core::ServiceId;
    match ServiceId::parse_from_service_id_string(key) {
        Some(ServiceId::Aci(aci)) => format!("{:?}", SignalUser::Aci(aci)),
        Some(ServiceId::Pni(pni)) => format!("{:?}", SignalUser::Pni(pni)),
        None => key.to_owned(),
    }
}

/// How a job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Done {
    /// Finished; the row is deleted.
    Finished,
    /// Finished for now; run again at this time (ms since the epoch).
    RepeatAt(i64),
}

/// What [`retry`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Retry {
    /// The next attempt starts at this time (ms since the epoch).
    At(i64),
    /// Out of time (or permanent): the row is deleted.
    Dropped,
}

/// The wait before attempt number `attempt` (1-based, as Desktop's
/// `exponentialBackoffSleepTime`): 0, 190 ms, then ×1.9 per attempt up to 15 minutes.
pub(crate) fn backoff(attempt: u32) -> Duration {
    let index = usize::try_from(attempt.max(1) - 1).unwrap_or(usize::MAX);
    if let Some(ms) = FIRST_BACKOFFS_MS.get(index) {
        return Duration::from_millis(*ms);
    }
    let last = FIRST_BACKOFFS_MS[FIRST_BACKOFFS_MS.len() - 1] as f64;
    let exponent =
        i32::try_from(index.saturating_add(2) - FIRST_BACKOFFS_MS.len()).unwrap_or(i32::MAX);
    let ms = last / BACKOFF_FACTOR * BACKOFF_FACTOR.powi(exponent);
    let max_ms = MAX_BACKOFF.as_secs_f64() * 1000.0;
    if ms.is_finite() && ms < max_ms {
        Duration::from_secs_f64(ms / 1000.0)
    } else {
        MAX_BACKOFF
    }
}

fn ms(d: Duration) -> i64 {
    i64::try_from(duration_ms(d)).unwrap_or(i64::MAX)
}

/// Adds a one-shot job, due now, unless the same `(kind, key)` is already queued. `max_age`
/// bounds how long it is retried. Returns whether a row was inserted.
pub(crate) fn enqueue(
    conn: &Connection,
    kind: JobKind,
    key: &str,
    now: i64,
    max_age: Option<Duration>,
) -> Result<bool> {
    let expires_at = max_age.map(|age| now.saturating_add(ms(age)));
    let inserted = conn
        .prepare_cached(
            "INSERT INTO jobs(kind, key, priority, next_at, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?4) ON CONFLICT(kind, key) DO NOTHING",
        )?
        .execute((kind.as_str(), key, kind.priority(), now, expires_at))?;
    Ok(inserted > 0)
}

/// Adds a one-shot job like [`enqueue`], and gives one that is already queued a fresh start:
/// due now, no failed attempts, a new expiry. For a job that is asked for anew on purpose (the
/// sync requests of a start): with [`enqueue`] a row left from a run of days ago would keep
/// its old expiry and be dropped at its first failure.
pub(crate) fn enqueue_renewed(
    conn: &Connection,
    kind: JobKind,
    key: &str,
    now: i64,
    max_age: Option<Duration>,
) -> Result<()> {
    let expires_at = max_age.map(|age| now.saturating_add(ms(age)));
    conn.prepare_cached(
        "INSERT INTO jobs(kind, key, priority, next_at, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?4)
         ON CONFLICT(kind, key) DO UPDATE SET next_at = min(next_at, excluded.next_at),
           expires_at = excluded.expires_at, attempts = 0, last_error = NULL",
    )?
    .execute((kind.as_str(), key, kind.priority(), now, expires_at))?;
    Ok(())
}

/// Asks for a job because something new happened (a `FetchLatest`, a new storage key, a new
/// profile key): adds it like [`enqueue`], or asks a queued one again: due now (its backoff
/// grows on as before), and run once more after the run in progress, which may have read the
/// old state (`jobs.requests`, [`finish`]). For kinds that [re-run on
/// request](JobKind::reruns_on_request).
pub(crate) fn request(
    conn: &Connection,
    kind: JobKind,
    key: &str,
    now: i64,
    max_age: Option<Duration>,
) -> Result<()> {
    debug_assert!(kind.reruns_on_request(), "{} is not re-run", kind.as_str());
    let expires_at = max_age.map(|age| now.saturating_add(ms(age)));
    conn.prepare_cached(
        "INSERT INTO jobs(kind, key, priority, next_at, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?4)
         ON CONFLICT(kind, key) DO UPDATE SET requests = requests + 1,
           next_at = min(next_at, excluded.next_at)",
    )?
    .execute((kind.as_str(), key, kind.priority(), now, expires_at))?;
    Ok(())
}

/// The job key of the core's durable command `cmd` with `payload`: the id plus a digest of the
/// work, because the core reuses a command id once the command is confirmed
/// (`pending_commands.id` has no AUTOINCREMENT), while a resent command carries the same work.
pub(crate) fn command_key(cmd: i64, payload: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(payload);
    let mut key = format!("{cmd}:");
    for byte in &digest[..8] {
        let _ = write!(key, "{byte:02x}");
    }
    key
}

/// Adds the job of the core's durable command `cmd` with its `payload`, due now. A repeated
/// command (the core resends unconfirmed ones at every start) finds its job, or its record in
/// `done_commands` once the job ran, and adds nothing. Returns whether a row was inserted.
pub(crate) fn enqueue_command(
    conn: &Connection,
    kind: JobKind,
    cmd: i64,
    payload: &[u8],
    now: i64,
) -> Result<bool> {
    let key = command_key(cmd, payload);
    let done: bool = conn
        .prepare_cached("SELECT EXISTS (SELECT 1 FROM done_commands WHERE kind = ?1 AND key = ?2)")?
        .query_row((kind.as_str(), &key), |r| r.get(0))?;
    if done {
        return Ok(false);
    }
    let inserted = conn
        .prepare_cached(
            "INSERT INTO jobs(kind, key, priority, next_at, created_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5) ON CONFLICT(kind, key) DO NOTHING",
        )?
        .execute((kind.as_str(), &key, kind.priority(), now, payload))?;
    Ok(inserted > 0)
}

/// Adds a one-shot job with a payload (replacing the payload of a queued one), due now. For a
/// kind that [re-runs on request](JobKind::reruns_on_request) the replaced job is also asked for
/// again with a fresh retry budget (see [`request`]): a new contact-sync blob that arrives while
/// the old one is applied is applied after it.
pub(crate) fn enqueue_payload(
    conn: &Connection,
    kind: JobKind,
    key: &str,
    payload: &[u8],
    now: i64,
    max_age: Option<Duration>,
) -> Result<()> {
    let expires_at = max_age.map(|age| now.saturating_add(ms(age)));
    conn.prepare_cached(
        "INSERT INTO jobs(kind, key, priority, next_at, expires_at, created_at, payload)
         VALUES (?1, ?2, ?3, ?4, ?5, ?4, ?6)
         ON CONFLICT(kind, key) DO UPDATE SET payload = excluded.payload,
           requests = requests + ?7,
           next_at = CASE WHEN ?7 THEN min(next_at, excluded.next_at) ELSE next_at END,
           expires_at = CASE WHEN ?7 THEN excluded.expires_at ELSE expires_at END,
           attempts = CASE WHEN ?7 THEN 0 ELSE attempts END",
    )?
    .execute((
        kind.as_str(),
        key,
        kind.priority(),
        now,
        expires_at,
        payload,
        kind.reruns_on_request(),
    ))?;
    Ok(())
}

/// Makes a job due at `at` (adding it if needed); one due earlier keeps its time.
pub(crate) fn schedule_at(conn: &Connection, kind: JobKind, key: &str, at: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO jobs(kind, key, priority, next_at, created_at) VALUES (?1, ?2, ?3, ?4, ?4)
         ON CONFLICT(kind, key) DO UPDATE SET next_at = min(next_at, excluded.next_at)",
    )?
    .execute((kind.as_str(), key, kind.priority(), at))?;
    Ok(())
}

/// Makes a periodic job due now (adding it if needed); a pending retry keeps its backoff only
/// when it is due earlier anyway.
pub(crate) fn schedule_now(conn: &Connection, kind: JobKind, key: &str, now: i64) -> Result<()> {
    schedule_at(conn, kind, key, now)
}

/// The most urgent job due at `now`, if any; while not `online`, only kinds that may run during
/// the catch-up; while a history transfer is `held`, none of the kinds that wait for it
/// ([`JobKind::waits_for_transfer`]). Rows of unknown kinds (a newer build's) are left alone.
pub(crate) fn next_due(
    conn: &Connection,
    now: i64,
    online: bool,
    held: bool,
) -> Result<Option<Job>> {
    let sql = format!(
        "SELECT id, kind, key, attempts, expires_at, payload, requests FROM jobs
         WHERE next_at <= ?1 AND (?2 OR kind NOT IN ({})) AND (NOT ?3 OR kind NOT IN ({}))
         ORDER BY priority, next_at, id",
        JobKind::sql_not_online(),
        JobKind::sql_transfer_held()
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query((now, online, held))?;
    while let Some(row) = rows.next()? {
        if let Some(job) = job_of_row(row)? {
            return Ok(Some(job));
        }
    }
    Ok(None)
}

/// The job of a row of `SELECT id, kind, key, attempts, expires_at, payload, requests`, or
/// `None` for a kind this build does not know.
fn job_of_row(row: &kourier_db::rusqlite::Row<'_>) -> Result<Option<Job>> {
    let kind: String = row.get(1)?;
    let Some(kind) = JobKind::parse(&kind) else {
        return Ok(None);
    };
    Ok(Some(Job {
        id: row.get(0)?,
        kind,
        key: row.get(2)?,
        attempts: row.get(3)?,
        expires_at: row.get(4)?,
        payload: row.get(5)?,
        requests: row.get(6)?,
    }))
}

/// Up to `limit` more due jobs of `first`'s kind, to run beside it ([`JobKind::batch`]), in
/// the order [`next_due`] would pick them. The kinds that run in batches neither wait for the
/// catch-up nor for a history transfer, so `first`'s filter is the kind alone.
fn more_due(conn: &Connection, first: &Job, now: i64, limit: usize) -> Result<Vec<Job>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, kind, key, attempts, expires_at, payload, requests FROM jobs
         WHERE kind = ?1 AND next_at <= ?2 AND id != ?3 ORDER BY next_at, id LIMIT ?4",
    )?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut rows = stmt.query((first.kind.as_str(), now, first.id, limit))?;
    let mut jobs = Vec::new();
    while let Some(row) = rows.next()? {
        jobs.extend(job_of_row(row)?);
    }
    Ok(jobs)
}

/// Holds every queued job of `kind` until `until` (ms) at the latest; one that is due later
/// keeps its time. Returns how many were moved.
fn pause_kind(conn: &Connection, kind: JobKind, until: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached("UPDATE jobs SET next_at = ?2 WHERE kind = ?1 AND next_at < ?2")?
        .execute((kind.as_str(), until))?)
}

/// When the next job is due (ms), if any job that may run now (see [`next_due`]) is queued.
pub(crate) fn next_at(conn: &Connection, online: bool, held: bool) -> Result<Option<i64>> {
    let sql = format!(
        "SELECT kind, next_at FROM jobs
         WHERE (?1 OR kind NOT IN ({})) AND (NOT ?2 OR kind NOT IN ({})) ORDER BY next_at",
        JobKind::sql_not_online(),
        JobKind::sql_transfer_held()
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query([online, held])?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        if JobKind::parse(&kind).is_some() {
            return Ok(Some(row.get(1)?));
        }
    }
    Ok(None)
}

/// How long a finished command is remembered (`done_commands`).
pub(crate) const DONE_COMMAND_RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// Forgets finished commands older than [`DONE_COMMAND_RETENTION`].
pub(crate) fn purge_done_commands(conn: &Connection, now: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM done_commands WHERE done_at < ?1")?
        .execute([now.saturating_sub(ms(DONE_COMMAND_RETENTION))])?;
    Ok(())
}

/// Records a success at `now`. A job asked for again while it ran
/// ([`JobKind::reruns_on_request`]) stays, due now.
pub(crate) fn finish(conn: &Connection, job: &Job, done: Done, now: i64) -> Result<()> {
    match done {
        Done::Finished => {
            let deleted = conn
                .prepare_cached("DELETE FROM jobs WHERE id = ?1 AND requests = ?2")?
                .execute((job.id, job.requests))?;
            if deleted == 0 {
                rerun_now(conn, job, now, None)?;
            }
            if job.kind.is_command() {
                conn.prepare_cached(
                    "INSERT OR REPLACE INTO done_commands(kind, key, done_at) VALUES (?1, ?2, ?3)",
                )?
                .execute((job.kind.as_str(), &job.key, now))?;
            }
        }
        Done::RepeatAt(at) => {
            conn.prepare_cached(
                "UPDATE jobs SET attempts = 0, next_at = ?2, last_error = NULL, challenged = 0,
                   held_from = NULL
                 WHERE id = ?1",
            )?
            .execute((job.id, at))?;
        }
    }
    Ok(())
}

/// Records a failed attempt: the job runs again after [`backoff`], and not before `wait` has
/// passed when the server said how long to wait (the `Retry-After` of a rate limit or a
/// challenge), unless that would start after its expiry or `permanent` says no attempt can
/// succeed, in which case it is deleted (but a job asked for again while it ran gets a fresh
/// attempt, due now). What a dropped job leaves undone is noted in the same transaction
/// ([`crate::maintenance::job_dropped`]).
pub(crate) fn retry(
    conn: &Connection,
    job: &Job,
    now: i64,
    error: &str,
    permanent: bool,
    wait: Option<Duration>,
) -> Result<Retry> {
    let attempts = job.attempts.saturating_add(1);
    let at = now
        .saturating_add(ms(backoff(attempts.saturating_add(1))))
        .max(now.saturating_add(wait.map_or(0, ms)));
    if permanent || job.expires_at.is_some_and(|expires| at > expires) {
        let deleted = conn
            .prepare_cached("DELETE FROM jobs WHERE id = ?1 AND requests = ?2")?
            .execute((job.id, job.requests))?;
        if deleted == 0 && rerun_now(conn, job, now, Some(error))? {
            return Ok(Retry::At(now));
        }
        crate::maintenance::job_dropped(conn, job)?;
        return Ok(Retry::Dropped);
    }
    conn.prepare_cached(
        "UPDATE jobs SET attempts = ?2, next_at = ?3, last_error = ?4, challenged = 0,
           held_from = NULL
         WHERE id = ?1",
    )?
    .execute((job.id, attempts, at, error))?;
    Ok(Retry::At(at))
}

/// The server's send challenge stands until `until` (ms): marks `job` (which just met it, and
/// whose next attempt [`retry`] already scheduled) as waiting for it, and holds every other
/// queued job that [sends to others](JobKind::sends_to_others) until then, so that one
/// challenge costs one refused request and not one per queued job. A held job remembers when
/// it was due before (`held_from`), for [`release_challenged`]. Returns how many others were
/// held.
pub(crate) fn hold_for_challenge(conn: &Connection, job: &Job, until: i64) -> Result<usize> {
    conn.prepare_cached("UPDATE jobs SET challenged = 1, held_from = NULL WHERE id = ?1")?
        .execute([job.id])?;
    // SQLite reads every column of the row as it was before the update, so `held_from` gets
    // the time the job had; a job held before keeps the time it had then.
    let sql = format!(
        "UPDATE jobs SET held_from = CASE WHEN challenged = 1 THEN held_from ELSE next_at END,
           challenged = 1, next_at = ?1
         WHERE id != ?2 AND next_at < ?1 AND kind IN ({})",
        JobKind::sql_sends_to_others()
    );
    Ok(conn.prepare_cached(&sql)?.execute((until, job.id))?)
}

/// Holds one due job until `until` because the send challenge stands (a job queued after the
/// challenge was met, or one that something made due again): no request is made for it.
pub(crate) fn hold_one_for_challenge(conn: &Connection, job: &Job, until: i64) -> Result<()> {
    conn.prepare_cached(
        "UPDATE jobs SET held_from = CASE WHEN challenged = 1 THEN held_from ELSE next_at END,
           challenged = 1, next_at = max(next_at, ?2)
         WHERE id = ?1",
    )?
    .execute((job.id, until))?;
    Ok(())
}

/// The server lifted its send challenge, or the challenge is no longer known to stand (a fresh
/// `Online`, a restart: [`Shared::forget_job_challenge`]): the job that met it is due at `now`,
/// and every job held for it is due when it was due before the hold, at `now` if that time
/// has passed. So a job that waits after another failure (a rate limit, a network error)
/// keeps its time, whether the challenge held it or not. Returns how many were released.
pub(crate) fn release_challenged(conn: &Connection, now: i64) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "UPDATE jobs SET challenged = 0, held_from = NULL,
               next_at = min(next_at, max(?1, coalesce(held_from, ?1)))
             WHERE challenged = 1",
        )?
        .execute([now])?)
}

/// Replaces the payload of `job` while it runs (what is left of its work when a part is done
/// and the rest has to wait). Its key stays: a command job is still found by its command.
pub(crate) fn set_payload(conn: &Connection, job: &Job, payload: &[u8]) -> Result<()> {
    conn.prepare_cached("UPDATE jobs SET payload = ?2 WHERE id = ?1")?
        .execute((job.id, payload))?;
    Ok(())
}

/// Makes a job that was asked for again during its run due at `now` with a fresh retry budget.
/// Returns whether the row still exists.
fn rerun_now(conn: &Connection, job: &Job, now: i64, error: Option<&str>) -> Result<bool> {
    let updated = conn
        .prepare_cached(
            "UPDATE jobs SET attempts = 0, next_at = ?2, last_error = ?3 WHERE id = ?1",
        )?
        .execute((job.id, now, error))?;
    Ok(updated > 0)
}

/// How many jobs of `kind` are queued.
pub(crate) fn count(conn: &Connection, kind: JobKind) -> Result<i64> {
    Ok(conn
        .prepare_cached("SELECT count(*) FROM jobs WHERE kind = ?1")?
        .query_row([kind.as_str()], |r| r.get(0))?)
}

/// Whether an attempt that failed with `error` can never succeed, so the job is dropped instead
/// of retried: the server refused the request itself (4xx other than timeout and rate limits),
/// or libsignal refused the content. Network trouble, 5xx and rate limits are retried.
pub(crate) fn is_permanent(error: &Error) -> bool {
    match error {
        // The credential of "today" was refused (the credentials are forgotten at the 401, so
        // the next attempt fetches fresh ones). Desktop fails and tries again later too: "we
        // don't know if we have access or not". See [`gives_up`] for how long.
        error if is_group_auth_refusal(error) => false,
        Error::Status { status, .. } => {
            (400..500).contains(status) && !matches!(status, 408 | 413 | 425 | 429)
        }
        Error::Protocol(_) | Error::Decode(_) => true,
        Error::Transport(TransportError::InvalidRequest(_)) => true,
        Error::Challenge { .. } | Error::RateLimited { .. } => false,
        Error::Transport(_)
        | Error::Db(_)
        | Error::Outbox(_)
        | Error::NotLinked
        | Error::CoreGone
        | Error::CoreRejected { .. }
        // (Never a job's error: it ends the actor before any job runs.)
        | Error::WrongEnvironment { .. }
        | Error::Internal(_) => false,
    }
}

/// Whether `error` is the group server refusing our credential (401): the request for the
/// group state or the request for the credentials themselves.
pub(crate) fn is_group_auth_refusal(error: &Error) -> bool {
    matches!(
        error,
        Error::Status {
            what: "group fetch" | "group credentials",
            status: 401,
        }
    )
}

/// How many times a job may fail, the last time with a refused group credential
/// ([`is_group_auth_refusal`]), before it is dropped: about four hours with the backoff, long
/// enough for a clock that was wrong to be put right. The group refresh jobs carry no expiry,
/// so without this a credential that is never accepted would be asked for every fifteen
/// minutes for good. A group imported from the phone's history that loses its refresh here
/// gets one again at the next start (`maintenance::enqueue_stranded_group_refreshes`).
pub(crate) const GROUP_AUTH_ATTEMPTS: u32 = 30;

/// Whether a job that failed with `error` after `attempts` earlier failures is dropped
/// instead of retried: the error is [permanent](is_permanent), or a group credential was
/// refused [`GROUP_AUTH_ATTEMPTS`] times.
pub(crate) fn gives_up(error: &Error, attempts: u32) -> bool {
    is_permanent(error)
        || (is_group_auth_refusal(error) && attempts.saturating_add(1) >= GROUP_AUTH_ATTEMPTS)
}

/// Runs due jobs on `socket` until the task is aborted (the socket closed); returns only the
/// error that stopped it (`backend.db` failed, or the core is gone). `gate` (the post-link
/// gate, §6.5 step 8) runs whenever no job is due AND after every batch: the link must not wait
/// for jobs it does not need (every profile fetch and group refresh a big account queues; a
/// queue that keeps getting due jobs would put it off again and again; link feedback L2, F1).
pub(crate) async fn run<T, F, Fut>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    mut gate: F,
) -> Result<Infallible>
where
    T: SignalTransport,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    loop {
        let now = shared.now();
        let online = shared.is_online();
        // A history transfer that is not over; its end wakes this loop (`jobs_wake`).
        let held = shared.transfer.holds();
        let Some(job) = shared.db.read(|c| next_due(c, now, online, held))? else {
            gate().await?;
            let wake_at = shared.db.read(|c| next_at(c, online, held))?;
            let sleep = wake_at.map(|at| shared.clock.sleep_until(u64::try_from(at).unwrap_or(0)));
            match sleep {
                Some(sleep) => tokio::select! {
                    () = shared.jobs_wake.notified() => {}
                    () = sleep => {}
                },
                None => shared.jobs_wake.notified().await,
            }
            continue;
        };
        // A request for an old file's re-upload that waited for a connection longer than
        // its lifetime (a computer offline for hours) is not one the user still waits for:
        // the core ended that wait at its own bound. It is dropped here, before it can wake
        // the phone for nothing, and the core hears of it.
        if expired_before_run(&job, now) {
            let dropped = shared
                .db
                .zone(|tx| drop_expired(tx, &job, now, &shared.wake))?;
            tracing::info!(
                kind = job.kind.as_str(),
                dropped,
                "job dropped: it expired before it ran"
            );
            continue;
        }
        // The send challenge stands: a job that would only be refused with the same 428
        // waits for it without a request (one queued after the challenge was met).
        if job.kind.sends_to_others()
            && let Some(until) = shared.challenge_until(now)
        {
            shared
                .db
                .zone(|tx| hold_one_for_challenge(tx, &job, until))?;
            tracing::debug!(
                kind = job.kind.as_str(),
                key = %job.log_key(),
                "job held: the send challenge stands"
            );
            continue;
        }
        let mut batch = vec![job];
        let limit = batch[0].kind.batch().saturating_sub(1);
        if limit > 0 {
            let more = shared.db.read(|c| more_due(c, &batch[0], now, limit))?;
            batch.extend(more);
        }
        let outcomes = futures::future::join_all(
            batch
                .iter()
                .map(|job| crate::maintenance::run_job(shared, socket, job)),
        )
        .await;
        let mut failed = 0_usize;
        for (job, outcome) in batch.iter().zip(outcomes) {
            failed += usize::from(!settle(shared, job, outcome)?);
        }
        if batch.len() > 1 {
            let kind = batch[0].kind;
            let left = shared.db.read(|c| count(c, kind))?;
            tracing::info!(
                kind = kind.as_str(),
                ran = batch.len(),
                failed,
                left,
                "jobs done"
            );
        }
        gate().await?;
    }
}

/// Whether `job` is one whose time ran out before it was tried: only a backfill request is
/// judged so (the user waits for its answer); every other kind keeps trying, and
/// [`retry`] judges its expiry after a failed attempt.
pub(crate) fn expired_before_run(job: &Job, now: i64) -> bool {
    job.kind == JobKind::BackfillRequest && job.expires_at.is_some_and(|at| now > at)
}

/// Deletes the expired `job` that was never tried and tells the core what that leaves undone
/// ([`crate::backfill::dropped`]: the wait ends), in this zone. Returns whether it was deleted
/// (a job asked for again meanwhile is a new one and stays).
pub(crate) fn drop_expired(
    tx: &mut kourier_db::Tx<'_>,
    job: &Job,
    now: i64,
    wake: &Rc<tokio::sync::Notify>,
) -> Result<bool> {
    let deleted = tx
        .prepare_cached("DELETE FROM jobs WHERE id = ?1 AND requests = ?2")?
        .execute((job.id, job.requests))?;
    if deleted == 0 {
        return Ok(false);
    }
    crate::maintenance::job_dropped(tx, job)?;
    if job.kind == JobKind::BackfillRequest {
        crate::backfill::dropped(tx, job, now, wake)?;
    }
    Ok(true)
}

/// Records how one job of a batch ended; returns whether it succeeded. Fails only with the
/// errors that stop [`run`] (`backend.db` failed, or the core is gone).
fn settle<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    job: &Job,
    outcome: Result<Done>,
) -> Result<bool> {
    let now = shared.now();
    let e = match outcome {
        Ok(done) => {
            shared.db.zone(|tx| finish(tx, job, done, now))?;
            // A batch is summed up by `run`: a line per profile is hundreds after an import.
            if job.kind.batch() > 1 {
                tracing::debug!(kind = job.kind.as_str(), key = %job.log_key(), "job done");
            } else {
                tracing::info!(kind = job.kind.as_str(), key = %job.log_key(), "job done");
            }
            return Ok(true);
        }
        Err(e @ (Error::CoreGone | Error::Db(_))) => return Err(e),
        Err(e) => e,
    };
    let detail = e.to_string();
    let permanent = gives_up(&e, job.attempts);
    // A send challenge met by a job (a receipt, a read sync, a timer update, a
    // sync transcript): the account shows it like the challenge of a message, and
    // the job waits for the server's time instead of knocking again at once.
    // The challenge is announced once while it stands, whatever the number of
    // jobs and tries that meet it, and the jobs that would meet it too are held.
    let challenge_wait = match e.challenge() {
        Some(challenge) => Some(crate::challenge::announce(shared, &challenge, now)?),
        None => None,
    };
    // A rate limit of a profile fetch holds every profile fetch, as Desktop's
    // `ProfileService` pauses its queue: the next ones would only be refused too.
    let rate_wait = (job.kind == JobKind::ProfileFetch && matches!(e, Error::RateLimited { .. }))
        .then(|| e.server_wait().unwrap_or(PROFILE_RATE_LIMIT_WAIT));
    let wait = challenge_wait.or(rate_wait).or_else(|| e.server_wait());
    let retried = shared.db.zone(|tx| -> Result<Retry> {
        let retried = retry(tx, job, now, &detail, permanent, wait)?;
        if retried == Retry::Dropped && job.kind == JobKind::BackfillRequest {
            // The core ends the wait in the zone that drops the request.
            crate::backfill::dropped(tx, job, now, &shared.wake)?;
        }
        if let (Some(wait), Retry::At(_)) = (challenge_wait, retried) {
            let held = hold_for_challenge(tx, job, now.saturating_add(ms(wait)))?;
            if held > 0 {
                tracing::info!(held, "jobs held while the send challenge stands");
            }
        }
        if let Some(wait) = rate_wait {
            let held = pause_kind(tx, job.kind, now.saturating_add(ms(wait)))?;
            if held > 0 {
                tracing::info!(
                    held,
                    wait_s = wait.as_secs(),
                    "profile fetches wait for the rate limit"
                );
            }
        }
        Ok(retried)
    })?;
    match retried {
        Retry::At(at) => tracing::warn!(
            kind = job.kind.as_str(),
            key = %job.log_key(),
            error = %detail,
            retry_in_ms = at.saturating_sub(now),
            "job failed"
        ),
        Retry::Dropped => tracing::warn!(
            kind = job.kind.as_str(),
            key = %job.log_key(),
            error = %detail,
            attempts = job.attempts + 1,
            "job dropped"
        ),
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, kourier_db::ZoneDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([4; 32]),
        )
        .unwrap();
        (dir, db)
    }

    /// Profile fetches run in batches of [`PROFILE_FETCH_BATCH`]: the batch is the due jobs of
    /// the first one's kind, in the order they would be picked, never one twice, never one
    /// that is not due, never another kind; every other kind runs alone.
    #[test]
    fn due_profile_fetches_run_together_and_other_kinds_alone() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            assert!(enqueue(tx, JobKind::DeliveryReceipts, "", 0, None)?);
            for i in 0..25 {
                assert!(enqueue(
                    tx,
                    JobKind::ProfileFetch,
                    &format!("p{i:02}"),
                    i,
                    None
                )?);
            }
            assert!(enqueue(tx, JobKind::ProfileFetch, "later", 1_000, None)?);
            assert!(enqueue(tx, JobKind::ReadSync, "1:a", 0, None)?);
            let first = next_due(tx, 100, true, false)?.expect("due");
            assert_eq!(first.kind, JobKind::ReadSync, "priority first");
            assert_eq!(first.kind.batch(), 1);
            finish(tx, &first, Done::Finished, 100)?;
            // Delivery receipts share the profile fetches' priority and were queued first.
            let first = next_due(tx, 100, true, false)?.expect("due");
            assert_eq!(first.kind, JobKind::DeliveryReceipts);
            finish(tx, &first, Done::Finished, 100)?;
            let first = next_due(tx, 100, true, false)?.expect("due");
            assert_eq!(
                (first.kind, first.key.as_str()),
                (JobKind::ProfileFetch, "p00")
            );
            let more = more_due(tx, &first, 100, first.kind.batch() - 1)?;
            let keys: Vec<_> = more.iter().map(|j| j.key.as_str()).collect();
            assert_eq!(
                keys,
                [
                    "p01", "p02", "p03", "p04", "p05", "p06", "p07", "p08", "p09"
                ]
            );
            for job in std::iter::once(&first).chain(&more) {
                finish(tx, job, Done::Finished, 100)?;
            }
            let first = next_due(tx, 100, true, false)?.expect("due");
            assert_eq!(first.key, "p10");
            let more = more_due(tx, &first, 100, 100)?;
            assert_eq!(more.len(), 14, "p11..p24; not the one due later");
            assert!(
                more.iter()
                    .all(|j| j.kind == JobKind::ProfileFetch && j.key != "later")
            );
            Ok(())
        })
        .unwrap();
    }

    /// A rate limit holds every queued job of the kind until its end; a job due later keeps
    /// its time, and other kinds are not touched.
    #[test]
    fn a_rate_limit_holds_every_profile_fetch() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            assert!(enqueue(tx, JobKind::ProfileFetch, "a", 0, None)?);
            assert!(enqueue(tx, JobKind::ProfileFetch, "b", 5_000, None)?);
            assert!(enqueue(tx, JobKind::ProfileFetch, "c", 90_000, None)?);
            assert!(enqueue(tx, JobKind::DeliveryReceipts, "", 0, None)?);
            assert_eq!(pause_kind(tx, JobKind::ProfileFetch, 60_000)?, 2);
            let due = next_due(tx, 59_999, true, false)?.expect("due");
            assert_eq!(due.kind, JobKind::DeliveryReceipts);
            finish(tx, &due, Done::Finished, 0)?;
            assert!(next_due(tx, 59_999, true, false)?.is_none());
            let due = next_due(tx, 60_000, true, false)?.expect("due");
            assert_eq!(due.kind, JobKind::ProfileFetch);
            assert_eq!(more_due(tx, &due, 60_000, 9)?.len(), 1, "c is due at 90 s");
            Ok(())
        })
        .unwrap();
    }

    /// A job the server told to wait (a rate limit, a challenge) is not tried again before
    /// that time, whatever its backoff says; an accepted challenge answer makes it due at once;
    /// a wait that ends after the job's expiry drops it like any retry that would.
    #[test]
    fn a_failed_job_waits_as_long_as_the_server_asked() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            let hour = Some(Duration::from_secs(3600));
            assert!(enqueue(tx, JobKind::ReadSync, "1:a", 0, hour)?);
            let job = next_due(tx, 0, true, false)?.expect("due");
            let wait = Some(Duration::from_secs(90));
            let Retry::At(at) = retry(tx, &job, 1_000, "challenge", false, wait)? else {
                panic!("kept");
            };
            assert_eq!(at, 91_000, "not the 190 ms of the backoff");
            assert!(next_due(tx, 90_999, true, false)?.is_none());
            assert!(next_due(tx, 91_000, true, false)?.is_some());
            // The backoff still counts when it is the longer one.
            let Retry::At(at) = retry(tx, &job, 1_000, "challenge", false, Some(Duration::ZERO))?
            else {
                panic!("kept");
            };
            assert!(at > 1_000 && at < 2_000, "{at}");
            // The server lifted the challenge: what waited for it runs now; a job that never
            // failed keeps its time.
            retry(tx, &job, 1_000, "challenge", false, wait)?;
            hold_for_challenge(tx, &job, 91_000)?;
            assert!(enqueue(tx, JobKind::MslPurge, "", 500_000, None)?);
            assert_eq!(release_challenged(tx, 2_000)?, 1);
            assert_eq!(
                next_due(tx, 2_000, true, false)?.map(|j| j.kind),
                Some(JobKind::ReadSync)
            );
            assert_eq!(count(tx, JobKind::MslPurge)?, 1);
            assert!(
                next_due(tx, 400_000, true, false)?.is_some_and(|j| j.kind == JobKind::ReadSync)
            );
            // Told to wait past its expiry: dropped.
            let day = Some(Duration::from_secs(86_400));
            assert_eq!(
                retry(tx, &job, 1_000, "challenge", false, day)?,
                Retry::Dropped
            );
            assert_eq!(count(tx, JobKind::ReadSync)?, 0);
            Ok(())
        })
        .unwrap();
    }

    /// One challenge holds every queued job that sends to another account, costs no request
    /// for them, and an accepted answer wakes exactly the jobs that waited for the challenge:
    /// one that waits out a rate limit of its own keeps its time.
    #[test]
    fn a_challenge_holds_the_jobs_it_would_refuse_and_its_answer_wakes_only_them() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            let next_at = |tx: &Connection, kind: JobKind, key: &str| -> i64 {
                tx.query_row(
                    "SELECT next_at FROM jobs WHERE kind = ?1 AND key = ?2",
                    (kind.as_str(), key),
                    |r| r.get(0),
                )
                .unwrap()
            };
            let challenged = |tx: &Connection| -> i64 {
                tx.query_row("SELECT count(*) FROM jobs WHERE challenged = 1", [], |r| {
                    r.get(0)
                })
                .unwrap()
            };
            // Fifty receipts and a retry request are queued; a read sync to our own devices,
            // a profile fetch and a timer update that waits out a rate limit as well.
            for n in 0..50 {
                assert!(enqueue(tx, JobKind::Receipts, &format!("{n}:a"), 0, None)?);
            }
            assert!(enqueue(tx, JobKind::RetryRequest, "x.1:5", 0, None)?);
            assert!(enqueue(tx, JobKind::ReadSync, "7:b", 0, None)?);
            assert!(enqueue(tx, JobKind::ProfileFetch, "peer", 0, None)?);
            assert!(enqueue(tx, JobKind::TimerUpdate, "peer:9", 0, None)?);
            let limited = next_due_of(tx, JobKind::TimerUpdate);
            let hour = Some(Duration::from_secs(3600));
            assert!(matches!(
                retry(tx, &limited, 1_000, "rate limited", false, hour)?,
                Retry::At(3_601_000)
            ));
            // A resend that waits out a rate limit which ends before the challenge does (the
            // usual case: a real challenge names hours, a rate limit seconds).
            assert!(enqueue(tx, JobKind::Resend, "peer.1:7", 0, None)?);
            let resend = next_due_of(tx, JobKind::Resend);
            let half_minute = Some(Duration::from_secs(30));
            assert!(matches!(
                retry(tx, &resend, 1_000, "rate limited", false, half_minute)?,
                Retry::At(31_000)
            ));

            // The first receipt meets the challenge (Retry-After 90 s).
            let first = next_due_of(tx, JobKind::Receipts);
            let wait = Some(Duration::from_secs(90));
            retry(tx, &first, 1_000, "challenge", false, wait)?;
            let held = hold_for_challenge(tx, &first, 91_000)?;
            assert_eq!(
                held, 51,
                "49 receipts, the retry request and the resend, not the rest"
            );
            assert_eq!(challenged(tx), 52);
            assert_eq!(
                next_at(tx, JobKind::Resend, "peer.1:7"),
                91_000,
                "its rate limit is over before the challenge: held like the others"
            );
            // Nothing that sends to others is due before the challenge's time: no request.
            let mut due = Vec::new();
            while let Some(job) = next_due(tx, 90_999, true, false)? {
                due.push(job.kind);
                finish(tx, &job, Done::Finished, 90_999)?;
            }
            assert_eq!(due, [JobKind::ReadSync, JobKind::ProfileFetch]);
            // The rate-limited job was due later than the challenge and keeps its own time.
            assert_eq!(next_at(tx, JobKind::TimerUpdate, "peer:9"), 3_601_000);

            // A job queued while the challenge stands is held when it comes up.
            assert!(enqueue(tx, JobKind::Receipts, "late:c", 2_000, None)?);
            let late = next_due(tx, 2_000, true, false)?.expect("due");
            assert_eq!(late.key, "late:c");
            hold_one_for_challenge(tx, &late, 91_000)?;
            assert!(next_due(tx, 90_999, true, false)?.is_none());

            // Something asks for a held job again (delivery receipts are scheduled for
            // every message that arrives): it comes up, is held again without a request,
            // and still knows when it was due before the challenge.
            assert!(enqueue(tx, JobKind::DeliveryReceipts, "", 2_500, None)?);
            let receipts = next_due(tx, 2_500, true, false)?.expect("due");
            hold_one_for_challenge(tx, &receipts, 91_000)?;
            schedule_at(tx, JobKind::DeliveryReceipts, "", 3_000)?;
            let receipts = next_due(tx, 3_000, true, false)?.expect("due again");
            assert_eq!(receipts.kind, JobKind::DeliveryReceipts);
            hold_one_for_challenge(tx, &receipts, 91_000)?;
            assert!(next_due(tx, 90_999, true, false)?.is_none());

            // The answer was accepted: the receipt that met the challenge and the jobs that
            // were due when it held them run now. The resend goes back to the end of its own
            // rate limit, and the timer update, which the challenge never held, still waits
            // for its hour.
            assert_eq!(release_challenged(tx, 5_000)?, 54);
            assert_eq!(challenged(tx), 0);
            assert_eq!(next_at(tx, JobKind::Receipts, &first.key), 5_000);
            assert_eq!(next_at(tx, JobKind::Receipts, "3:a"), 5_000);
            assert_eq!(next_at(tx, JobKind::Receipts, "late:c"), 5_000);
            assert_eq!(next_at(tx, JobKind::RetryRequest, "x.1:5"), 5_000);
            assert_eq!(next_at(tx, JobKind::DeliveryReceipts, ""), 5_000);
            assert_eq!(
                next_at(tx, JobKind::Resend, "peer.1:7"),
                31_000,
                "a rate limit that ends before the challenge would have is not cut short"
            );
            assert_eq!(
                next_at(tx, JobKind::TimerUpdate, "peer:9"),
                3_601_000,
                "an unrelated rate limit is not cut short"
            );
            let left: i64 = tx.query_row(
                "SELECT count(*) FROM jobs WHERE held_from IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(left, 0, "nothing stays marked as held");
            // A challenged job that then fails for another reason no longer counts as
            // waiting for the challenge.
            let again = next_due_of(tx, JobKind::Receipts);
            retry(tx, &again, 5_000, "challenge", false, wait)?;
            hold_for_challenge(tx, &again, 95_000)?;
            retry(tx, &again, 6_000, "offline", false, None)?;
            let flag: i64 = tx.query_row(
                "SELECT challenged FROM jobs WHERE id = ?1",
                [again.id],
                |r| r.get(0),
            )?;
            assert_eq!(flag, 0);
            Ok(())
        })
        .unwrap();
    }

    /// The first queued job of `kind` (whatever its time).
    fn next_due_of(conn: &Connection, kind: JobKind) -> Job {
        conn.query_row(
            "SELECT id, key, attempts, expires_at, payload, requests FROM jobs
             WHERE kind = ?1 ORDER BY id LIMIT 1",
            [kind.as_str()],
            |r| {
                Ok(Job {
                    id: r.get(0)?,
                    kind,
                    key: r.get(1)?,
                    attempts: r.get(2)?,
                    expires_at: r.get(3)?,
                    payload: r.get(4)?,
                    requests: r.get(5)?,
                })
            },
        )
        .unwrap()
    }

    #[test]
    fn backoff_follows_desktop() {
        let ms = |attempt| backoff(attempt).as_millis();
        assert_eq!(ms(0), 0);
        assert_eq!(ms(1), 0);
        assert_eq!(ms(2), 190);
        assert_eq!(ms(3), 361);
        assert_eq!(ms(4), 685);
        assert!(backoff(5) > backoff(4));
        assert_eq!(backoff(30), MAX_BACKOFF);
        assert_eq!(backoff(u32::MAX), MAX_BACKOFF);
        // Desktop: retrying for a day is about 111 attempts.
        let mut total = Duration::ZERO;
        let mut attempts = 0;
        while total < SYNC_REQUEST_MAX_AGE {
            attempts += 1;
            total += backoff(attempts);
        }
        assert!((105..=115).contains(&attempts), "{attempts}");
    }

    /// The post-link gate is looked at after every batch, not only when no job is due (link
    /// feedback L2, F1): a queue that keeps having due jobs (every profile fetch and group
    /// refresh of a big account) must not put off the login's `Done`. Three due local jobs;
    /// the gate stops the runner the first time it is called, with two of them still due.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_link_gate_is_looked_at_after_every_batch() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let test = crate::actor::testing::shared(
                    fixture.db.clone(),
                    fixture.account.clone(),
                    Vec::new(),
                );
                let shared = Rc::new(test.shared);
                let now = shared.now();
                shared
                    .db
                    .zone(|tx| -> Result<()> {
                        tx.execute("DELETE FROM jobs", [])?;
                        for key in ["a", "b", "c"] {
                            enqueue(tx, JobKind::MslPurge, key, now, None)?;
                        }
                        Ok(())
                    })
                    .unwrap();
                let (socket, _remote, _events) =
                    crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
                let socket = Arc::new(socket);
                let due_at_gate = Rc::new(std::cell::RefCell::new(Vec::new()));
                let seen = due_at_gate.clone();
                let db = shared.db.clone();
                let stopped = run(&shared, &socket, move || {
                    let due: i64 = db
                        .read(|c| -> Result<i64> {
                            Ok(c.query_row(
                                "SELECT count(*) FROM jobs WHERE next_at <= ?1",
                                [now],
                                |r| r.get(0),
                            )?)
                        })
                        .unwrap();
                    seen.borrow_mut().push(due);
                    async { Err(Error::NotLinked) }
                })
                .await;
                assert!(matches!(stopped, Err(Error::NotLinked)), "{stopped:?}");
                assert_eq!(
                    *due_at_gate.borrow(),
                    [2],
                    "the gate ran after the first job, with two still due"
                );
            })
            .await;
    }

    /// The phone's contact sync, which the link waits for, runs ahead of the group refreshes an
    /// imported archive queues (one per group), even when they were due first (link feedback
    /// L2: it waited ~9 s behind 50 refreshes at 150 ms round trip).
    #[test]
    fn the_contact_sync_runs_ahead_of_group_refreshes() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            for group in ["g1", "g2", "g3"] {
                enqueue(tx, JobKind::GroupRefresh, group, 100, None)?;
            }
            enqueue(tx, JobKind::ContactSync, "", 150, None)?;
            Ok(())
        })
        .unwrap();
        let first = db.read(|c| next_due(c, 200, true, false)).unwrap().unwrap();
        assert_eq!(first.kind, JobKind::ContactSync);
        assert!(JobKind::ContactSync.priority() < JobKind::GroupRefresh.priority());
    }

    #[test]
    fn enqueue_dedups_and_due_jobs_run_by_priority() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            assert!(enqueue(tx, JobKind::SyncRequest, "contacts", 100, None)?);
            assert!(!enqueue(tx, JobKind::SyncRequest, "contacts", 50, None)?);
            assert!(enqueue(tx, JobKind::SyncRequest, "blocked", 100, None)?);
            assert!(enqueue(tx, JobKind::Capabilities, "", 120, None)?);
            schedule_now(tx, JobKind::RemoteConfig, "", 130)?;
            Ok(())
        })
        .unwrap();
        db.read(|c| -> Result<()> {
            assert_eq!(
                next_due(c, 99, true, false)?,
                None,
                "nothing is due before 100"
            );
            assert_eq!(next_at(c, true, false)?, Some(100));
            let first = next_due(c, 200, true, false)?.unwrap();
            assert_eq!(
                first.kind,
                JobKind::RemoteConfig,
                "lowest priority value first"
            );
            assert_eq!(count(c, JobKind::SyncRequest)?, 2);
            Ok(())
        })
        .unwrap();
        // Among equal priorities: the earlier one, then the lower id.
        db.zone(|tx| -> Result<()> {
            let job = next_due(tx, 200, true, false)?.unwrap();
            finish(tx, &job, Done::RepeatAt(10_000), 0)?;
            let job = next_due(tx, 200, true, false)?.unwrap();
            assert_eq!(job.kind, JobKind::Capabilities);
            finish(tx, &job, Done::Finished, 0)?;
            let job = next_due(tx, 200, true, false)?.unwrap();
            assert_eq!(
                (job.kind, job.key.as_str()),
                (JobKind::SyncRequest, "contacts")
            );
            Ok(())
        })
        .unwrap();
        db.read(|c| -> Result<()> {
            assert_eq!(
                count(c, JobKind::Capabilities)?,
                0,
                "one-shot jobs are deleted"
            );
            assert_eq!(count(c, JobKind::RemoteConfig)?, 1, "periodic jobs stay");
            Ok(())
        })
        .unwrap();
        // A periodic job made due again moves forward, never backward.
        db.zone(|tx| -> Result<()> {
            schedule_now(tx, JobKind::RemoteConfig, "", 5_000)?;
            let at: i64 = tx.query_row(
                "SELECT next_at FROM jobs WHERE kind = 'remote_config'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(at, 5_000);
            schedule_now(tx, JobKind::RemoteConfig, "", 9_000)?;
            let at: i64 = tx.query_row(
                "SELECT next_at FROM jobs WHERE kind = 'remote_config'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(at, 5_000);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn failures_back_off_until_the_job_expires() {
        let (_dir, db) = db();
        let day = ms(SYNC_REQUEST_MAX_AGE);
        db.zone(|tx| -> Result<()> {
            enqueue(
                tx,
                JobKind::SyncRequest,
                "contacts",
                0,
                Some(SYNC_REQUEST_MAX_AGE),
            )?;
            let mut now = 0;
            let mut last_wait = -1;
            loop {
                let job = next_due(tx, now, true, false)?.expect("due");
                match retry(tx, &job, now, "server answered 503", false, None)? {
                    Retry::At(at) => {
                        assert!(at - now >= last_wait, "waits never shrink");
                        assert!(at <= day, "never retried past its expiry");
                        last_wait = at - now;
                        now = at;
                    }
                    Retry::Dropped => break,
                }
            }
            assert!(
                now > day - ms(MAX_BACKOFF),
                "retried for about a day ({now})"
            );
            assert_eq!(count(tx, JobKind::SyncRequest)?, 0, "dropped after a day");
            Ok(())
        })
        .unwrap();
        // A permanent failure drops at once; a periodic job without expiry keeps retrying.
        db.zone(|tx| -> Result<()> {
            enqueue(
                tx,
                JobKind::SyncRequest,
                "blocked",
                0,
                Some(SYNC_REQUEST_MAX_AGE),
            )?;
            let job = next_due(tx, 0, true, false)?.unwrap();
            assert_eq!(
                retry(tx, &job, 0, "server answered 400", true, None)?,
                Retry::Dropped
            );
            schedule_now(tx, JobKind::RemoteConfig, "", 0)?;
            for _ in 0..200 {
                let job = next_due(tx, i64::MAX, true, false)?.unwrap();
                assert!(matches!(
                    retry(tx, &job, 0, "timed out", false, None)?,
                    Retry::At(_)
                ));
            }
            let (attempts, error): (u32, String) = tx.query_row(
                "SELECT attempts, last_error FROM jobs WHERE kind = 'remote_config'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            assert_eq!((attempts, error.as_str()), (200, "timed out"));
            let job = next_due(tx, i64::MAX, true, false)?.unwrap();
            finish(tx, &job, Done::RepeatAt(7), 0)?;
            let attempts: u32 = tx.query_row("SELECT attempts FROM jobs", [], |r| r.get(0))?;
            assert_eq!(attempts, 0, "a success resets the backoff");
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn command_jobs_dedup_on_the_command_and_wait_for_online() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            assert!(enqueue_command(tx, JobKind::ReadSync, 7, b"one", 100)?);
            assert!(
                !enqueue_command(tx, JobKind::ReadSync, 7, b"one", 100)?,
                "a resent command finds its job"
            );
            assert_eq!(
                next_due(tx, 200, false, false)?,
                None,
                "receipts wait for Online"
            );
            assert_eq!(next_at(tx, false, false)?, None);
            let job = next_due(tx, 200, true, false)?.unwrap();
            assert_eq!(job.kind, JobKind::ReadSync);
            assert_eq!(job.payload.as_deref(), Some(&b"one"[..]));
            finish(tx, &job, Done::Finished, 50)?;
            assert!(
                !enqueue_command(tx, JobKind::ReadSync, 7, b"one", 100)?,
                "a command resent after its job finished does not run again"
            );
            // The core reuses the id of a confirmed command for new work: a new job.
            assert!(enqueue_command(tx, JobKind::ReadSync, 7, b"two", 100)?);
            let reused = next_due(tx, 200, true, false)?.unwrap();
            assert_eq!(reused.payload.as_deref(), Some(&b"two"[..]));
            finish(tx, &reused, Done::Finished, 50)?;
            purge_done_commands(tx, 50 + ms(DONE_COMMAND_RETENTION) + 1)?;
            assert!(enqueue_command(tx, JobKind::ReadSync, 7, b"one", 100)?);
            let again = next_due(tx, 200, true, false)?.unwrap();
            finish(tx, &again, Done::Finished, 50)?;
            // Catch-up work still runs while syncing.
            schedule_at(tx, JobKind::MslPurge, "", 300)?;
            schedule_at(tx, JobKind::MslPurge, "", 900)?;
            assert_eq!(
                next_at(tx, false, false)?,
                Some(300),
                "the earlier time wins"
            );
            assert_eq!(
                next_due(tx, 300, false, false)?.unwrap().kind,
                JobKind::MslPurge
            );
            enqueue_payload(tx, JobKind::SentTranscript, "42", b"a", 0, None)?;
            enqueue_payload(tx, JobKind::SentTranscript, "42", b"b", 0, None)?;
            let job = next_due(tx, 300, false, false)?.unwrap();
            assert_eq!(
                (job.kind, job.payload.as_deref()),
                (JobKind::SentTranscript, Some(&b"b"[..]))
            );
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn unknown_kinds_are_left_alone() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO jobs(kind, key, priority, next_at, created_at)
                 VALUES ('from_the_future', '', -1, 0, 0)",
                [],
            )?;
            assert_eq!(next_due(tx, 10, true, false)?, None);
            assert_eq!(next_at(tx, true, false)?, None);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn permanent_errors_are_refusals_not_trouble() {
        let status = |status| Error::Status { what: "x", status };
        assert!(is_permanent(&status(400)));
        assert!(is_permanent(&status(404)));
        assert!(!is_permanent(&status(429)));
        assert!(!is_permanent(&status(408)));
        assert!(!is_permanent(&status(500)));
        assert!(!is_permanent(&Error::Transport(TransportError::Timeout)));
        assert!(!is_permanent(&Error::Internal("x")));
    }

    /// A group server that refuses the credential of the day (401) is not a refusal of the
    /// request: the credentials are forgotten and the job tries again with fresh ones, for a
    /// while. Any other 401 stays permanent, as does a 403 or 404 of the group server.
    #[test]
    fn a_refused_group_credential_is_trouble_for_a_while_not_a_refusal() {
        let group = |what, status| Error::Status { what, status };
        for what in ["group fetch", "group credentials"] {
            assert!(!is_permanent(&group(what, 401)), "{what}");
            assert!(is_group_auth_refusal(&group(what, 401)));
            assert!(is_permanent(&group(what, 403)), "{what}");
            assert!(is_permanent(&group(what, 404)), "{what}");
            assert!(!is_group_auth_refusal(&group(what, 403)));
        }
        assert!(is_permanent(&group("profile", 401)));
        assert!(!is_group_auth_refusal(&group("profile", 401)));
        // For a while: the 30th failure in a row drops the job, the 29th does not.
        let refused = group("group fetch", 401);
        assert!(!gives_up(&refused, 0));
        assert!(!gives_up(&refused, GROUP_AUTH_ATTEMPTS - 2));
        assert!(gives_up(&refused, GROUP_AUTH_ATTEMPTS - 1));
        assert!(gives_up(&group("group fetch", 403), 0));
        assert!(!gives_up(
            &group("group fetch", 503),
            GROUP_AUTH_ATTEMPTS * 2
        ));
        // About four hours of backoff (the doc says so).
        let total: Duration = (1..=GROUP_AUTH_ATTEMPTS).map(backoff).sum();
        let hours = total.as_secs_f64() / 3600.0;
        assert!((3.0..=5.5).contains(&hours), "{hours}");
    }

    /// The job of a refused group credential stays queued, backed off, until the budget is
    /// spent; then it is deleted. A refresh of a group that was imported from the phone and
    /// never fetched comes back at the next start.
    #[test]
    fn a_group_refresh_survives_refused_credentials_and_is_dropped_after_the_budget() {
        let (_dir, db) = db();
        let refused = Error::Status {
            what: "group fetch",
            status: 401,
        };
        db.zone(|tx| -> Result<()> {
            assert!(enqueue(tx, JobKind::GroupRefresh, "g", 0, None)?);
            let mut now = 0;
            for failure in 1..=GROUP_AUTH_ATTEMPTS {
                let job = next_due(tx, i64::MAX, true, false)?.expect("still queued");
                assert_eq!(job.attempts, failure - 1);
                let retried = retry(
                    tx,
                    &job,
                    now,
                    "refused",
                    gives_up(&refused, job.attempts),
                    None,
                )?;
                if failure < GROUP_AUTH_ATTEMPTS {
                    let Retry::At(at) = retried else {
                        panic!("dropped at failure {failure}");
                    };
                    assert!(at >= now);
                    now = at;
                } else {
                    assert_eq!(retried, Retry::Dropped);
                }
            }
            assert_eq!(count(tx, JobKind::GroupRefresh)?, 0);
            Ok(())
        })
        .unwrap();
    }

    /// The wiring: the job loop's [`settle`] decides by [`gives_up`] with the job's own count of
    /// failures, not by [`is_permanent`] alone, so the 401 of the group server (not permanent)
    /// ends the job after [`GROUP_AUTH_ATTEMPTS`] failures and cannot loop for good. The 403 of
    /// the same server is a refusal at the first failure, a 503 never one.
    #[tokio::test(flavor = "current_thread")]
    async fn settle_drops_a_group_job_after_the_budget_of_refused_credentials() {
        let fixture = crate::receive::tests::fixture();
        let test =
            crate::actor::testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let shared = Rc::new(test.shared);
        let status = |status| Error::Status {
            what: "group fetch",
            status,
        };
        let queued = || {
            fixture
                .db
                .read(|c| count(c, JobKind::GroupRefresh))
                .unwrap()
        };
        // The queued refresh, as the job loop would pick it, with `attempts` earlier failures.
        let picked = |key: &str, attempts: u32| -> Job {
            fixture
                .db
                .zone(|tx| -> Result<()> {
                    // (A second enqueue of a queued job is a no-op.)
                    enqueue(tx, JobKind::GroupRefresh, key, 0, None)?;
                    tx.execute(
                        "UPDATE jobs SET attempts = ?2, next_at = 0 WHERE key = ?1",
                        (key, attempts),
                    )?;
                    Ok(())
                })
                .unwrap();
            fixture
                .db
                .read(|c| next_due(c, i64::MAX, true, false))
                .unwrap()
                .expect("queued")
        };

        // A refused credential: backed off, not dropped, until the failure that spends the
        // budget; then the job is gone.
        let job = picked("g", GROUP_AUTH_ATTEMPTS - 2);
        assert!(!settle(&shared, &job, Err(status(401))).unwrap());
        assert_eq!(queued(), 1, "the {}th failure is retried", job.attempts + 1);
        let job = picked("g", GROUP_AUTH_ATTEMPTS - 1);
        assert!(!settle(&shared, &job, Err(status(401))).unwrap());
        assert_eq!(queued(), 0, "the {GROUP_AUTH_ATTEMPTS}th failure drops it");

        // The group server's other refusals end a job at once, its trouble never does.
        let job = picked("h", 0);
        assert!(!settle(&shared, &job, Err(status(403))).unwrap());
        assert_eq!(queued(), 0, "a 403 is a refusal");
        let job = picked("i", 0);
        assert!(!settle(&shared, &job, Err(status(503))).unwrap());
        assert_eq!(queued(), 1, "a 503 is retried");
    }

    #[test]
    fn kinds_and_requests_round_trip() {
        for kind in JobKind::ALL {
            assert_eq!(JobKind::parse(kind.as_str()), Some(kind));
        }
        for request in SyncRequest::ALL {
            assert_eq!(SyncRequest::parse(request.as_str()), Some(request));
        }
        assert_eq!(SyncRequest::parse("groups"), None);
    }

    /// A `FetchLatest`, a new contact-sync blob or a new profile key that arrives while the
    /// same job runs must not be swallowed by the run's `finish` (it may have read old state).
    #[test]
    fn a_job_asked_for_again_while_it_runs_runs_once_more() {
        let (_dir, db) = db();
        db.zone(|tx| -> Result<()> {
            assert!(enqueue(tx, JobKind::StorageSync, "", 100, None)?);
            let running = next_due(tx, 100, true, false)?.unwrap();
            // FetchLatest arrives during the run.
            request(tx, JobKind::StorageSync, "", 150, None)?;
            finish(tx, &running, Done::Finished, 200)?;
            let again = next_due(tx, 200, true, false)?.expect("asked for again: still queued");
            assert_eq!(again.kind, JobKind::StorageSync);
            finish(tx, &again, Done::Finished, 300)?;
            assert_eq!(count(tx, JobKind::StorageSync)?, 0, "nobody asked again");
            // A plain enqueue (every start) during a run adds nothing.
            assert!(enqueue(tx, JobKind::StorageSync, "", 400, None)?);
            let running = next_due(tx, 400, true, false)?.unwrap();
            assert!(!enqueue(tx, JobKind::StorageSync, "", 401, None)?);
            finish(tx, &running, Done::Finished, 402)?;
            assert_eq!(count(tx, JobKind::StorageSync)?, 0);

            // A queued job in its backoff asked for again is due now; its backoff grows on.
            assert!(enqueue(tx, JobKind::ProfileFetch, "p", 1_000, None)?);
            let failed = next_due(tx, 1_000, true, false)?.unwrap();
            let Retry::At(at) = retry(tx, &failed, 1_000, "timed out", false, None)? else {
                panic!("retried");
            };
            assert!(at > 1_001);
            request(tx, JobKind::ProfileFetch, "p", 1_001, None)?;
            let job = next_due(tx, 1_001, true, false)?.expect("due now, not after the backoff");
            assert_eq!(job.attempts, 1);
            finish(tx, &job, Done::Finished, 1_002)?;
            assert_eq!(
                count(tx, JobKind::ProfileFetch)?,
                0,
                "picked after the request"
            );

            // A permanent failure of the old request does not drop the new one.
            request(tx, JobKind::ProfileFetch, "q", 2_000, None)?;
            let running = next_due(tx, 2_000, true, false)?.unwrap();
            request(tx, JobKind::ProfileFetch, "q", 2_001, None)?;
            assert_eq!(
                retry(tx, &running, 2_002, "server answered 400", true, None)?,
                Retry::At(2_002)
            );
            let job = next_due(tx, 2_002, true, false)?.unwrap();
            assert_eq!(
                retry(tx, &job, 2_003, "server answered 400", true, None)?,
                Retry::Dropped
            );

            // A newer contact-sync blob during the run is applied after it.
            enqueue_payload(tx, JobKind::ContactSync, "", b"old", 3_000, None)?;
            let running = next_due(tx, 3_000, true, false)?.unwrap();
            enqueue_payload(tx, JobKind::ContactSync, "", b"new", 3_001, None)?;
            finish(tx, &running, Done::Finished, 3_002)?;
            let job = next_due(tx, 3_002, true, false)?.unwrap();
            assert_eq!(job.payload.as_deref(), Some(&b"new"[..]));
            finish(tx, &job, Done::Finished, 3_003)?;
            assert_eq!(count(tx, JobKind::ContactSync)?, 0);

            // Other kinds keep "a second enqueue is a no-op".
            assert!(enqueue(tx, JobKind::SyncRequest, "contacts", 4_000, None)?);
            let running = next_due(tx, 4_000, true, false)?.unwrap();
            assert!(!enqueue(tx, JobKind::SyncRequest, "contacts", 4_001, None)?);
            finish(tx, &running, Done::Finished, 4_002)?;
            assert_eq!(count(tx, JobKind::SyncRequest)?, 0);
            enqueue_payload(tx, JobKind::SentTranscript, "7", b"a", 5_000, None)?;
            let running = next_due(tx, 5_000, false, false)?.unwrap();
            enqueue_payload(tx, JobKind::SentTranscript, "7", b"a", 5_001, None)?;
            finish(tx, &running, Done::Finished, 5_002)?;
            assert_eq!(
                count(tx, JobKind::SentTranscript)?,
                0,
                "no second transcript"
            );
            Ok(())
        })
        .unwrap();
    }

    /// Profile fetches are keyed by service id; logs show only the last four hex digits (§9.1).
    #[test]
    fn log_keys_hide_service_ids() {
        let job = |key: &str| Job {
            id: 1,
            kind: JobKind::ProfileFetch,
            key: key.to_owned(),
            attempts: 0,
            expires_at: None,
            payload: None,
            requests: 0,
        };
        let aci = "9d0652a3-dcc3-4d11-975f-74d61598733f";
        assert_eq!(job(aci).log_key(), "aci:…733f");
        assert_eq!(
            job("PNI:9d0652a3-dcc3-4d11-975f-74d61598733f").log_key(),
            "pni:…733f"
        );
        assert_eq!(job("contacts").log_key(), "contacts");
        assert_eq!(job("7:0011223344556677").log_key(), "7:0011223344556677");
        assert_eq!(job("").log_key(), "");
    }
}
