//! Account maintenance on the authenticated socket (§6.4, §6.5 step 8, §6.12): remote config
//! with ETag, capability registration, the post-link sync requests, and the decisions of when
//! each runs. The work itself runs as jobs ([`crate::jobs`]).
//!
//! Desktop sources: `SD/ts/textsecure/WebAPI.preload.ts` `getConfig` (ETag, 304) and
//! `registerCapabilities`, `SD/ts/RemoteConfig.dom.ts`, `SD/ts/background.preload.ts`
//! `afterEveryLinkedStartup` (capabilities at every start, `ensureAEP`) and
//! `SD/ts/textsecure/syncRequests.preload.ts`.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderValue, Method, StatusCode};
use kourier_backend_api::BackendEvent;
use kourier_backend_api::clock::duration_ms;
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_db::{Tx, ZoneDb};
use kourier_model::AccountCaps;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind, SYNC_REQUEST_MAX_AGE, SyncRequest};
use crate::net::{ChatRequest, ChatResponse, SignalTransport, json_request, parse_json};
use crate::outbox::append_events;
use crate::{commands, contact_sync, msl, send};

/// Path of the remote config (`configV2`).
const REMOTE_CONFIG_PATH: &str = "/v2/config";
/// Path of the capability registration.
const CAPABILITIES_PATH: &str = "/v1/devices/capabilities";
/// How often the remote config is refreshed while connected (§6.4).
pub(crate) const REMOTE_CONFIG_REFRESH: Duration = Duration::from_secs(2 * 60 * 60);
/// Remote config entries kept: Desktop's `desktop.`, `global.` and `cds.`, plus `client.` for
/// `client.maxAllowedClockSkewSeconds` (§6.4).
const KEPT_PREFIXES: [&str; 4] = ["desktop.", "global.", "cds.", "client."];
/// Entries of other clients kept by name: Android's cut of a received message's attachment
/// list, which bounds what one message of ours may carry (`crate::features::album_max`).
const KEPT_NAMES: [&str; 1] = [crate::features::CONFIG_MAX_POINTERS];
/// Signal sends bodies longer than this as a long-text attachment (Desktop
/// `MAX_MESSAGE_BODY_BYTE_LENGTH`).
const LONG_TEXT_BYTES: u32 = 2048;
/// How often the Message Send Log is purged.
const MSL_PURGE_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// How often a missing account entropy pool is asked for (`ensureAEP`: once a day).
const KEYS_REQUEST_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// This build's version: a start under a new version re-sends the sync requests.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Our capabilities, sent with `PUT /v1/devices/link` and at every start (§6.5 step 5).
/// They are Desktop's, value for value: `CapabilitiesUploadType` in
/// `SD/ts/types/Capabilities.d.ts` fixes `attachmentBackfill`, `spqr` and
/// `usernameChangeSyncMessage` to `true`, and Desktop sends `optionalPhoneNumber: !hasE164`
/// (`SD/ts/textsecure/WebAPI.preload.ts:2962-2967` at link,
/// `SD/ts/background.preload.ts:2091-2098` at every start); every M1 account has a number.
///
/// The server derives an account's capabilities from its devices, and which ones it takes
/// from the primary alone cannot be read from any source available here, so a linked device
/// that claims less than Desktop could lower what the whole account has. `attachmentBackfill`
/// puts no duty on Kourier: a linked device only ever gets answers to backfill requests it
/// sent itself (`SD/ts/textsecure/MessageReceiver.preload.ts:3150`), and Kourier sends none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capabilities {
    /// Sparse post-quantum ratchet.
    pub(crate) spqr: bool,
    /// Attachment backfill: this device may ask the primary for an attachment again.
    pub(crate) attachment_backfill: bool,
    /// Username change sync messages.
    pub(crate) username_change_sync_message: bool,
    /// Accounts without a phone number.
    pub(crate) optional_phone_number: bool,
}

/// What Kourier advertises.
pub(crate) const CAPABILITIES: Capabilities = Capabilities {
    spqr: true,
    attachment_backfill: true,
    username_change_sync_message: true,
    optional_phone_number: false,
};

/// A `GET /v2/config` answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fetched {
    /// 304: the stored config is current.
    Unmodified {
        /// The server's ETag, when it sent one.
        etag: Option<String>,
    },
    /// 200: the whole config.
    Config {
        /// Its ETag (sent as `If-None-Match` next time).
        etag: Option<String>,
        /// The entries Kourier keeps.
        entries: BTreeMap<String, String>,
    },
}

#[derive(Deserialize)]
struct ConfigBody {
    config: BTreeMap<String, String>,
}

/// Parses a `GET /v2/config` response (`WebAPI.preload.ts` `getConfig`).
pub(crate) fn parse_remote_config(response: &ChatResponse) -> Result<Fetched> {
    let etag = response
        .headers
        .get(http::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    match response.status {
        StatusCode::NOT_MODIFIED => Ok(Fetched::Unmodified { etag }),
        status if status.is_success() => {
            let body: ConfigBody = parse_json(response)?;
            let entries = body
                .config
                .into_iter()
                .filter(|(name, _)| {
                    KEPT_PREFIXES.iter().any(|p| name.starts_with(p))
                        || KEPT_NAMES.contains(&name.as_str())
                })
                .collect();
            Ok(Fetched::Config { etag, entries })
        }
        status => Err(Error::Status {
            what: "remote config",
            status: status.as_u16(),
        }),
    }
}

/// The limits of `config` the core knows (§3.8 tier 2, `SD/ts/RemoteConfig.dom.ts`). Unparsable
/// values count as absent; without a receive limit of its own, a config's receive limit is
/// 1.25 × its send limit (`SD/ts/types/AttachmentSize.std.ts`
/// `getMaximumIncomingAttachmentSize`).
pub(crate) fn account_caps(config: &BTreeMap<String, String>) -> AccountCaps {
    let number = |name: &str| config.get(name).and_then(|v| v.trim().parse::<u64>().ok());
    let max_attachment_bytes = number("global.attachments.maxBytes");
    AccountCaps {
        max_attachment_bytes,
        max_receive_attachment_bytes: number("global.attachments.maxReceiveBytes")
            .or_else(|| max_attachment_bytes.map(|max| max.saturating_add(max / 4))),
        long_text_bytes: Some(LONG_TEXT_BYTES),
        // Both are part of every chat's features (`crate::features::Limits`): a change sends
        // the features again.
        delete_for_everyone_max_age_secs: Some(crate::features::delete_max_age(config)),
        flags: BTreeMap::from([(
            "admin_delete".to_owned(),
            crate::features::flag(config, crate::features::CONFIG_ADMIN_DELETE),
        )]),
    }
}

/// One entry of the stored remote config.
pub(crate) fn remote_config_value(conn: &Connection, name: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM remote_config WHERE name = ?1")?
        .query_row([name], |r| r.get(0))
        .optional()?)
}

/// The stored remote config.
pub(crate) fn load_remote_config(conn: &Connection) -> Result<BTreeMap<String, String>> {
    let mut stmt = conn.prepare_cached("SELECT name, value FROM remote_config")?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Stores a fetched config and, when its limits changed (or it is the first one), writes
/// `AccountCaps` into `core_outbox` in the same zone. Returns whether it wrote events.
pub(crate) fn store_remote_config(
    tx: &mut Tx<'_>,
    fetched: &Fetched,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    match fetched {
        Fetched::Unmodified { etag } => {
            tx.execute(
                "UPDATE account SET remote_config_at = ?1,
                   remote_config_etag = coalesce(?2, remote_config_etag) WHERE id = 1",
                (now, etag),
            )?;
        }
        Fetched::Config { etag, entries } => {
            let old = load_remote_config(tx)?;
            let first: bool = tx.query_row(
                "SELECT remote_config_at IS NULL FROM account WHERE id = 1",
                [],
                |r| r.get(0),
            )?;
            tx.execute("DELETE FROM remote_config", [])?;
            for (name, value) in entries {
                tx.prepare_cached("INSERT INTO remote_config(name, value) VALUES (?1, ?2)")?
                    .execute((name, value))?;
            }
            tx.execute(
                "UPDATE account SET remote_config_at = ?1, remote_config_etag = ?2 WHERE id = 1",
                (now, etag),
            )?;
            let caps = account_caps(entries);
            let caps_changed = first || caps != account_caps(&old);
            // What every chat's features state (the attachment limit, the delete window, the
            // album size) may change without the caps changing.
            let limits_changed =
                crate::features::Limits::of(entries) != crate::features::Limits::of(&old);
            if caps_changed || limits_changed {
                let mut events = Vec::new();
                if caps_changed {
                    events.push(BackendEvent::AccountCaps(caps));
                }
                // Best effort: an unreadable account row fails everything else anyway.
                if let Some(account) = crate::db::Account::load(tx).ok().flatten() {
                    events.extend(crate::features::refresh_features(tx, &account, now)?);
                }
                if !events.is_empty() {
                    append_events(tx, &events, now, wake)?;
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

/// The ETag of the stored config.
fn stored_etag(conn: &Connection) -> Result<Option<String>> {
    Ok(conn.query_row(
        "SELECT remote_config_etag FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?)
}

/// The jobs of a fresh link, in the zone that stores the account (§6.5 step 8): the three sync
/// requests (and KEYS when the phone sent no account entropy pool), retried for a day; the first
/// Storage Service sync; the deadline after which the link stops waiting for the contact sync;
/// and the routine profile refresh.
pub(crate) fn enqueue_after_link(conn: &Connection, now: i64) -> Result<()> {
    enqueue_sync_requests(conn, now)?;
    request_keys_if_needed(conn, now)?;
    jobs::enqueue(conn, JobKind::StorageSync, "", now, None)?;
    let wait = i64::try_from(duration_ms(contact_sync::LINK_WAIT)).unwrap_or(i64::MAX);
    jobs::schedule_at(conn, JobKind::ContactSyncWait, "", now.saturating_add(wait))?;
    jobs::schedule_now(conn, JobKind::ProfileRefresh, "", now)?;
    // The link made every prekey just now: the first maintenance run is one interval away
    // (an app that is never restarted must still rotate its keys, §6.12).
    let interval =
        i64::try_from(duration_ms(crate::keys::MAINTENANCE_INTERVAL)).unwrap_or(i64::MAX);
    jobs::schedule_at(
        conn,
        JobKind::KeyMaintenance,
        "",
        now.saturating_add(interval),
    )?;
    Ok(())
}

/// CONTACTS, CONFIGURATION and BLOCKED, and the version they are asked under. The mark stands
/// for "asked": [`job_dropped`] takes it back when one of the three never went out.
fn enqueue_sync_requests(conn: &Connection, now: i64) -> Result<()> {
    for request in SyncRequest::AFTER_LINK {
        // (Renewed: a request still queued from an earlier ask gets its day of retries again.)
        jobs::enqueue_renewed(
            conn,
            JobKind::SyncRequest,
            request.as_str(),
            now,
            Some(SYNC_REQUEST_MAX_AGE),
        )?;
    }
    conn.execute(
        "UPDATE account SET app_version = ?1 WHERE id = 1",
        [APP_VERSION],
    )?;
    Ok(())
}

/// A job was dropped without success (a failure no retry can mend, or its time ran out):
/// called in the transaction that deletes it. One of the three sync requests of a link
/// clears the version they were asked under, so the next start asks for all three again
/// ([`enqueue_at_start`]); so does the job that applies the phone's answer to CONTACTS (its
/// blob could not be fetched or read: the request went out, the contacts did not come).
/// Without this a request that failed once was lost for good: on the first real link
/// (2026-10-07) contacts, configuration and the blocked list never came.
/// The next start and not at once: what failed permanently fails again until something
/// changes, and a start is when a new build or new server data arrives. KEYS has its own
/// daily retry (`request_keys_if_needed`). An answer that is malformed in itself (no payload,
/// no usable blob pointer) ends its job as finished and is not asked for again: the phone
/// would send the same.
pub(crate) fn job_dropped(conn: &Connection, job: &Job) -> Result<()> {
    let asked_again = match job.kind {
        JobKind::SyncRequest => {
            SyncRequest::parse(&job.key).is_some_and(|r| SyncRequest::AFTER_LINK.contains(&r))
        }
        JobKind::ContactSync => true,
        _ => false,
    };
    if asked_again {
        conn.execute("UPDATE account SET app_version = NULL WHERE id = 1", [])?;
    }
    Ok(())
}

/// Jobs of every start of a linked account: capabilities (§6.12; the link request carried them
/// on the first start), and the sync requests again when this is a new Kourier version
/// (Desktop `afterEveryLinkedStartupOnNewVersion`) or when one of them was dropped under
/// this version ([`job_dropped`]).
pub(crate) fn enqueue_at_start(conn: &Connection, now: i64) -> Result<()> {
    jobs::enqueue(conn, JobKind::Capabilities, "", now, None)?;
    // Prekeys at every start and every two days after (§6.12; Desktop runs it at once on a
    // new version and otherwise on its two-day timer).
    jobs::schedule_now(conn, JobKind::KeyMaintenance, "", now)?;
    jobs::schedule_now(conn, JobKind::MslPurge, "", now)?;
    // The Storage Service at every start (§6.9), and the routine profile refresh (§6.10).
    jobs::enqueue(conn, JobKind::StorageSync, "", now, None)?;
    jobs::schedule_now(conn, JobKind::ProfileRefresh, "", now)?;
    let version: Option<String> =
        conn.query_row("SELECT app_version FROM account WHERE id = 1", [], |r| {
            r.get(0)
        })?;
    if version.as_deref() != Some(APP_VERSION) {
        tracing::info!(
            "sync requests again: the first start of this version, or one was dropped before"
        );
        enqueue_sync_requests(conn, now)?;
    }
    Ok(())
}

/// Jobs of every authenticated connect: the remote config now (§6.4), and KEYS when the account
/// entropy pool is still missing (`ensureAEP`).
pub(crate) fn schedule_at_connect(conn: &Connection, now: i64) -> Result<()> {
    jobs::schedule_now(conn, JobKind::RemoteConfig, "", now)?;
    request_keys_if_needed(conn, now)?;
    Ok(())
}

/// Desktop's `ensureAEP`: without an account entropy pool, ask the phone for KEYS at most once a
/// day (again at once if the last request lies in the future). Returns whether it enqueued one.
pub(crate) fn request_keys_if_needed(conn: &Connection, now: i64) -> Result<bool> {
    let missing: bool = conn.query_row(
        "SELECT account_entropy_pool IS NULL FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    if !missing {
        return Ok(false);
    }
    request_keys(conn, now)
}

/// Asks the phone for KEYS at most once a day (again at once if the last request lies in the
/// future): without an account entropy pool, and when the storage key no longer decrypts the
/// manifest (Desktop's `stopStorageServiceSync`). Returns whether it enqueued a request.
pub(crate) fn request_keys(conn: &Connection, now: i64) -> Result<bool> {
    let last: Option<i64> = conn.query_row(
        "SELECT keys_requested_at FROM account WHERE id = 1",
        [],
        |r| r.get(0),
    )?;
    let interval = i64::try_from(duration_ms(KEYS_REQUEST_INTERVAL)).unwrap_or(i64::MAX);
    if last.is_some_and(|last| last <= now && now - last < interval) {
        return Ok(false);
    }
    jobs::enqueue(
        conn,
        JobKind::SyncRequest,
        SyncRequest::Keys.as_str(),
        now,
        Some(SYNC_REQUEST_MAX_AGE),
    )?;
    conn.execute(
        "UPDATE account SET keys_requested_at = ?1 WHERE id = 1",
        [now],
    )?;
    Ok(true)
}

/// The mirrored state a rebuilt `core.db` needs (§4.3 step 8): `AccountReady` and the stored
/// limits, in one `core_outbox` row; then the contacts and settings again: a full Storage
/// Service sync (from scratch), a new contact sync request, and profile fetches (which rewrite
/// the avatars the new `core.db` does not know). Groups join in S10.
pub(crate) fn rebuild_events(
    db: &ZoneDb,
    account: &Account,
    blob_dir: &std::path::Path,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<()> {
    let own_ids = account.own_ids()?;
    db.zone(|tx| -> Result<()> {
        let config = load_remote_config(tx)?;
        let configured: bool = tx.query_row(
            "SELECT remote_config_at IS NOT NULL FROM account WHERE id = 1",
            [],
            |r| r.get(0),
        )?;
        let mut events = vec![BackendEvent::AccountReady {
            own_ids,
            device: Some(u32::from(account.device_id)),
            display_name: None,
            number: account.number(),
        }];
        if configured {
            events.push(BackendEvent::AccountCaps(account_caps(&config)));
        }
        // The new core.db has no conversation features: send them again with the next event
        // that touches each chat (§6.7, `crate::features`).
        tx.execute("UPDATE peers SET features_hash = NULL", [])?;
        // The new core.db has lost its aliases: every PNI and ACI pair this device learned,
        // from whichever source, is told again (the Storage Service sync below repeats only
        // the pairs of its own records).
        events.extend(crate::links::all(tx)?);
        append_events(tx, &events, now, wake)?;
        // Contacts and settings again (the old avatar files belong to the lost core.db rows).
        crate::storage::reset(tx, now)?;
        jobs::enqueue(
            tx,
            JobKind::SyncRequest,
            SyncRequest::Contacts.as_str(),
            now,
            Some(SYNC_REQUEST_MAX_AGE),
        )?;
        // The avatar files only the lost core.db referenced go once this zone committed.
        let old_avatars: Vec<String> = tx
            .prepare("SELECT avatar_blob FROM peers WHERE avatar_blob IS NOT NULL")?
            .query_map([], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let files: Vec<std::path::PathBuf> =
            old_avatars.iter().map(|path| blob_dir.join(path)).collect();
        tx.on_commit(move || {
            for file in files {
                let _ = std::fs::remove_file(file);
            }
        });
        tx.execute(
            "UPDATE peers SET profile_fetched_at = NULL, profile_avatar = NULL,
               avatar_blob = NULL, avatar_source = NULL, avatar_hash = NULL",
            [],
        )?;
        jobs::schedule_now(tx, JobKind::ProfileRefresh, "", now)?;
        // Groups: the new core.db has none of them; each is fetched again and sent whole (title,
        // members, avatar, roaming flags), its old avatar file goes with the lost core.db.
        let old_group_avatars: Vec<String> = tx
            .prepare("SELECT avatar_blob FROM groups WHERE avatar_blob IS NOT NULL")?
            .query_map([], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        let files: Vec<std::path::PathBuf> = old_group_avatars
            .iter()
            .map(|path| blob_dir.join(path))
            .collect();
        tx.on_commit(move || {
            for file in files {
                let _ = std::fs::remove_file(file);
            }
        });
        tx.execute(
            "UPDATE groups SET features_hash = NULL, avatar_key = NULL, avatar_blob = NULL",
            [],
        )?;
        let group_ids: Vec<Vec<u8>> = tx
            .prepare("SELECT group_id FROM groups WHERE membership NOT IN ('left', 'gone')")?
            .query_map([], |r| r.get(0))?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        for id in group_ids {
            jobs::enqueue(
                tx,
                JobKind::GroupRefresh,
                &crate::groups::job_key(&id),
                now,
                None,
            )?;
        }
        Ok(())
    })
}

/// Runs one job (called by [`jobs::run`]).
pub(crate) async fn run_job<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    match job.kind {
        JobKind::RemoteConfig => remote_config(shared, socket).await,
        JobKind::Capabilities => register_capabilities(shared, socket).await,
        JobKind::SyncRequest => sync_request(shared, socket, &job.key).await,
        JobKind::Receipts => commands::run_receipts(shared, socket, job).await,
        JobKind::ReadSync => commands::run_read_sync(shared, socket, job).await,
        JobKind::DeliveryReceipts => commands::run_delivery_receipts(shared, socket).await,
        JobKind::SentTranscript => commands::run_sent_transcript(shared, socket, job).await,
        JobKind::MslPurge => msl_purge(shared),
        JobKind::ContactSync => contact_sync::run(shared, job).await,
        JobKind::ContactSyncWait => contact_sync_wait(shared),
        JobKind::StorageSync => crate::storage::sync(shared, socket).await,
        JobKind::ProfileFetch => crate::profile::run_fetch(shared, socket, job).await,
        JobKind::GroupRefresh => crate::groups::run_refresh(shared, socket, job).await,
        JobKind::RetryRequest => crate::retry::run_request(shared, socket, job).await,
        JobKind::Resend => crate::retry::run_resend(shared, socket, job).await,
        JobKind::TimerUpdate => crate::timer::run_update(shared, socket, job).await,
        JobKind::KeyMaintenance => key_maintenance(shared, socket, job).await,
        JobKind::DeleteSync => crate::delete_sync::run(shared, socket, job).await,
        JobKind::ProfileRefresh => {
            let account = shared.account().ok_or(Error::NotLinked)?;
            crate::profile::refresh(shared, account.aci)
        }
    }
}

/// The link stops waiting for the contact sync (§6.5 step 8): a phone that does not answer must
/// not keep the login on "Syncing". A contact sync that arrives later is still applied.
fn contact_sync_wait<T: SignalTransport>(shared: &Rc<Shared<T>>) -> Result<Done> {
    let changed = shared.db.zone(|tx| -> Result<usize> {
        Ok(tx.execute(
            "UPDATE account SET contact_sync_done = 1 WHERE id = 1 AND contact_sync_done = 0",
            [],
        )?)
    })?;
    if changed > 0 {
        tracing::info!("no contact sync from the phone yet; the link stops waiting for it");
    }
    Ok(Done::Finished)
}

/// `GET /v2/config` with `If-None-Match`; stores the answer and schedules the next refresh.
async fn remote_config<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
) -> Result<Done> {
    let mut request = ChatRequest::get(REMOTE_CONFIG_PATH)?;
    if let Some(etag) = shared.db.read(stored_etag)?
        && let Ok(value) = HeaderValue::from_str(&etag)
    {
        request = request.with_header(http::header::IF_NONE_MATCH, value);
    }
    let response = shared.net.request(socket, request).await?;
    let fetched = parse_remote_config(&response)?;
    let now = shared.now();
    let wrote_events = shared
        .db
        .zone(|tx| store_remote_config(tx, &fetched, now, &shared.wake))?;
    if let Fetched::Config { entries, .. } = &fetched {
        // The transports libsignal-net picks for its typed requests; in force from the next
        // connection on (§6.4).
        shared.net.transport().remote_config_changed(entries);
    }
    if wrote_events {
        // A job zone committed with its events still in core_outbox (§10.4).
        kourier_db::failpoint::hit(crate::receive::FAILPOINT_AFTER_BACKEND_EFFECTS_COMMIT);
    }
    tracing::info!(
        modified = matches!(fetched, Fetched::Config { .. }),
        "remote config refreshed"
    );
    Ok(Done::RepeatAt(now.saturating_add(
        i64::try_from(duration_ms(REMOTE_CONFIG_REFRESH)).unwrap_or(i64::MAX),
    )))
}

/// `PUT /v1/devices/capabilities` (`registerCapabilities`).
async fn register_capabilities<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
) -> Result<Done> {
    let request = json_request(Method::PUT, CAPABILITIES_PATH, &CAPABILITIES)?;
    let response = shared.net.request(socket, request).await?;
    if !response.status.is_success() {
        return Err(Error::Status {
            what: "capabilities",
            status: response.status.as_u16(),
        });
    }
    Ok(Done::Finished)
}

/// One sync request to our other devices.
async fn sync_request<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    key: &str,
) -> Result<Done> {
    let Some(request) = SyncRequest::parse(key) else {
        tracing::warn!(key, "unknown sync request job dropped");
        return Ok(Done::Finished);
    };
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    let timestamp = shared.unique_timestamp();
    send::send_sync_request(&ctx, request, timestamp).await?;
    Ok(Done::Finished)
}

/// How long after a failed routine key run the next one starts (`UpdateKeysListener`: "trying
/// again in five minutes").
const KEY_RETRY: Duration = Duration::from_secs(5 * 60);

/// Whether the server refused our PNI prekeys for good (403 or 422): this device's PNI identity
/// is not the account's any more (a number change it never learned of), and only a new link
/// brings the right one. Desktop unlinks (`UpdateKeysListener.preload.ts` `#run`).
fn pni_keys_refused(error: &Error) -> bool {
    matches!(
        error,
        Error::Status {
            status: 403 | 422,
            ..
        }
    )
}

/// Prekey maintenance (§6.12; `UpdateKeysListener.preload.ts` `#run`): ACI, then PNI. The job
/// with key [`crate::number_change::FORCED_PNI_KEYS`] is the one upload a number change asks
/// for; it is retried by the job queue, where Desktop gives up after one attempt.
async fn key_maintenance<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    use crate::keys::{MAINTENANCE_INTERVAL, Mode, maintain};
    use crate::store::IdentityKind;
    let account = shared.account().ok_or(Error::NotLinked)?;
    let now = shared.now();
    let later =
        |wait: Duration| now.saturating_add(i64::try_from(duration_ms(wait)).unwrap_or(i64::MAX));
    let run = |kind, mode| maintain(&shared.net, socket, &shared.db, &account, kind, mode, now);
    let refused = "the server refused this device's phone-number prekeys";
    if job.key == crate::number_change::FORCED_PNI_KEYS {
        return match run(IdentityKind::Pni, Mode::Forced).await {
            Ok(uploaded) => {
                tracing::info!(?uploaded, "PNI prekeys uploaded after the number change");
                Ok(Done::Finished)
            }
            Err(e) if pni_keys_refused(&e) => {
                shared.unlink(refused);
                Ok(Done::Finished)
            }
            Err(e) => Err(e),
        };
    }
    match run(IdentityKind::Aci, Mode::Routine).await {
        Ok(uploaded) => tracing::info!(identity = "aci", ?uploaded, "prekeys maintained"),
        Err(e @ (Error::CoreGone | Error::Db(_))) => return Err(e),
        Err(e) => {
            tracing::warn!(error = %e, "prekey maintenance failed; again in five minutes");
            return Ok(Done::RepeatAt(later(KEY_RETRY)));
        }
    }
    match run(IdentityKind::Pni, Mode::Routine).await {
        Ok(uploaded) => tracing::info!(identity = "pni", ?uploaded, "prekeys maintained"),
        Err(e @ (Error::CoreGone | Error::Db(_))) => return Err(e),
        Err(e) if pni_keys_refused(&e) => shared.unlink(refused),
        // Desktop: "Failure uploading PNI keys. Not trying again": the next routine run does.
        Err(e) => tracing::warn!(error = %e, "PNI prekey maintenance failed"),
    }
    Ok(Done::RepeatAt(later(MAINTENANCE_INTERVAL)))
}

/// Deletes Message Send Log entries past their retention (Desktop purges every 5 minutes;
/// hourly is plenty for an indexed delete).
fn msl_purge<T: SignalTransport>(shared: &Rc<Shared<T>>) -> Result<Done> {
    let now = shared.now();
    let purged = shared.db.zone(|tx| -> Result<usize> {
        jobs::purge_done_commands(tx, now)?;
        crate::retry::purge(tx, now)?;
        crate::upload::purge(tx, now)?;
        crate::send::outgoing::purge_quotes(tx, now)?;
        msl::purge(tx, now)
    })?;
    if purged > 0 {
        tracing::debug!(purged, "message send log purged");
    }
    Ok(Done::RepeatAt(now.saturating_add(
        i64::try_from(duration_ms(MSL_PURGE_INTERVAL)).unwrap_or(i64::MAX),
    )))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::HeaderMap;

    use super::*;
    use crate::db;

    fn response(status: u16, etag: Option<&str>, body: &str) -> ChatResponse {
        let mut headers = HeaderMap::new();
        if let Some(etag) = etag {
            headers.insert(http::header::ETAG, HeaderValue::from_str(etag).unwrap());
        }
        ChatResponse {
            status: StatusCode::from_u16(status).unwrap(),
            message: None,
            headers,
            body: Bytes::from(body.to_owned()),
        }
    }

    /// A `backend.db` with a minimal linked `account` row.
    fn linked_db(aep: Option<&str>) -> (tempfile::TempDir, ZoneDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = db::open(
            &dir.path().join("backend.db"),
            &kourier_backend_api::DbKey::new([6; 32]),
        )
        .unwrap();
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO account(id, aci, pni, e164, device_id, password, aci_registration_id,
                   pni_registration_id, profile_key, master_key, account_entropy_pool,
                   storage_key, linked_at)
                 VALUES (1, 'a', 'p', '+15550100', 2, 'pw', 1, 2, x'00', x'00', ?1, x'00', 0)",
                [aep],
            )?;
            Ok(())
        })
        .unwrap();
        (dir, db)
    }

    fn outbox_events(db: &ZoneDb) -> Vec<BackendEvent> {
        db.read(|c| -> Result<Vec<BackendEvent>> {
            let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
            let rows: Vec<Vec<u8>> = stmt
                .query_map([], |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            let mut events = Vec::new();
            for row in rows {
                events.extend(kourier_backend_api::decode_outbox_row(&row)?);
            }
            Ok(events)
        })
        .unwrap()
    }

    #[test]
    fn remote_config_parses_200_and_304_and_keeps_known_prefixes() {
        let fetched = parse_remote_config(&response(
            200,
            Some("abc"),
            r#"{"config":{"global.attachments.maxBytes":"104857600","desktop.x":"true",
                "android.y":"1","android.attachments.maxCount":"32",
                "client.maxAllowedClockSkewSeconds":"3600"}}"#,
        ))
        .unwrap();
        let Fetched::Config { etag, entries } = fetched else {
            panic!("expected a config");
        };
        assert_eq!(etag.as_deref(), Some("abc"));
        assert_eq!(
            entries.keys().map(String::as_str).collect::<Vec<_>>(),
            [
                "android.attachments.maxCount",
                "client.maxAllowedClockSkewSeconds",
                "desktop.x",
                "global.attachments.maxBytes"
            ],
            "Desktop's prefixes, and Android's attachment count by name"
        );
        assert_eq!(
            parse_remote_config(&response(304, Some("abc"), "")).unwrap(),
            Fetched::Unmodified {
                etag: Some("abc".to_owned())
            }
        );
        assert!(matches!(
            parse_remote_config(&response(500, None, "")),
            Err(Error::Status { status: 500, .. })
        ));
        assert!(parse_remote_config(&response(200, None, "{}")).is_err());
    }

    #[test]
    fn caps_map_the_attachment_limits() {
        let config = BTreeMap::from([
            (
                "global.attachments.maxBytes".to_owned(),
                "104857600".to_owned(),
            ),
            (
                "global.attachments.maxReceiveBytes".to_owned(),
                " 209715200 ".to_owned(),
            ),
            ("global.groupsv2.maxGroupSize".to_owned(), "32".to_owned()),
        ]);
        let caps = account_caps(&config);
        assert_eq!(caps.max_attachment_bytes, Some(104_857_600));
        assert_eq!(caps.max_receive_attachment_bytes, Some(209_715_200));
        assert_eq!(caps.long_text_bytes, Some(2048));
        let broken =
            BTreeMap::from([("global.attachments.maxBytes".to_owned(), "lots".to_owned())]);
        assert_eq!(account_caps(&broken).max_attachment_bytes, None);
        assert_eq!(account_caps(&broken).max_receive_attachment_bytes, None);
        // Without a receive limit of its own: 1.25 × the send limit, as Desktop.
        let send_only =
            BTreeMap::from([("global.attachments.maxBytes".to_owned(), "1000".to_owned())]);
        assert_eq!(
            account_caps(&send_only).max_receive_attachment_bytes,
            Some(1250)
        );
    }

    #[test]
    fn caps_are_emitted_for_the_first_config_and_changes_only() {
        let (_dir, db) = linked_db(Some("aep"));
        let wake = Rc::new(Notify::new());
        let config = |max: &str| Fetched::Config {
            etag: Some(format!("etag-{max}")),
            entries: BTreeMap::from([("global.attachments.maxBytes".to_owned(), max.to_owned())]),
        };
        let store = |fetched: &Fetched, now| {
            db.zone(|tx| store_remote_config(tx, fetched, now, &wake))
                .map(drop)
                .unwrap();
        };
        store(&config("100"), 1);
        store(&Fetched::Unmodified { etag: None }, 2);
        store(&config("100"), 3);
        store(&config("200"), 4);
        let caps: Vec<Option<u64>> = outbox_events(&db)
            .into_iter()
            .map(|e| match e {
                BackendEvent::AccountCaps(caps) => caps.max_attachment_bytes,
                other => panic!("unexpected {}", other.name()),
            })
            .collect();
        assert_eq!(caps, [Some(100), Some(200)], "first config and the change");
        let (etag, at): (Option<String>, i64) = db
            .read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT remote_config_etag, remote_config_at FROM account",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .unwrap();
        assert_eq!((etag.as_deref(), at), (Some("etag-200"), 4));
        assert_eq!(db.read(stored_etag).unwrap().as_deref(), Some("etag-200"));
    }

    #[test]
    fn link_enqueues_the_sync_requests_and_keys_only_without_an_aep() {
        let (_dir, db) = linked_db(Some("aep"));
        db.zone(|tx| enqueue_after_link(tx, 10)).unwrap();
        let keys: Vec<String> = db
            .read(|c| -> Result<_> {
                let mut stmt = c.prepare("SELECT key FROM jobs ORDER BY id")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .unwrap();
        assert_eq!(
            keys,
            ["contacts", "configuration", "blocked", "", "", "", ""],
            "sync requests, then the storage sync, the contact-sync deadline, the profile \
             refresh, key maintenance"
        );
        let key_run: i64 = db
            .read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT next_at FROM jobs WHERE kind = 'key_maintenance'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(
            key_run,
            10 + 2 * 24 * 60 * 60 * 1000,
            "two days after the link"
        );

        let (_dir, db) = linked_db(None);
        let day = i64::try_from(duration_ms(KEYS_REQUEST_INTERVAL)).unwrap();
        db.zone(|tx| -> Result<()> {
            enqueue_after_link(tx, 10)?;
            assert_eq!(
                jobs::count(tx, JobKind::SyncRequest)?,
                4,
                "KEYS without an AEP"
            );
            // Asked once a day at most, also across connects.
            tx.execute("DELETE FROM jobs WHERE key = 'keys'", [])?;
            assert!(!request_keys_if_needed(tx, 10 + day - 1)?);
            assert!(request_keys_if_needed(tx, 10 + day)?);
            tx.execute("DELETE FROM jobs WHERE key = 'keys'", [])?;
            // A request "in the future" (clock moved back) does not block a new one.
            assert!(request_keys_if_needed(tx, 5)?);
            Ok(())
        })
        .unwrap();

        // A manifest the storage key no longer decrypts asks for KEYS even with an AEP, also
        // at most once a day.
        let (_dir, db) = linked_db(Some("aep"));
        db.zone(|tx| -> Result<()> {
            assert!(!request_keys_if_needed(tx, 10)?, "the AEP is there");
            assert!(request_keys(tx, 10)?);
            assert!(!request_keys(tx, 10 + day - 1)?, "once a day");
            assert_eq!(jobs::count(tx, JobKind::SyncRequest)?, 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn starts_register_capabilities_and_a_new_version_syncs_again() {
        let (_dir, db) = linked_db(Some("aep"));
        let keys = |db: &ZoneDb| -> Vec<String> {
            db.read(|c| -> Result<_> {
                let mut stmt = c.prepare("SELECT kind || ':' || key FROM jobs ORDER BY id")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .unwrap()
        };
        db.zone(|tx| enqueue_after_link(tx, 1)).unwrap();
        db.zone(|tx| -> Result<()> {
            tx.execute("DELETE FROM jobs", [])?;
            Ok(())
        })
        .unwrap();
        db.zone(|tx| enqueue_at_start(tx, 2)).unwrap();
        assert_eq!(
            keys(&db),
            [
                "capabilities:",
                "key_maintenance:",
                "msl_purge:",
                "storage_sync:",
                "profile_refresh:"
            ],
            "same version: capabilities, prekeys, the purge, storage and profile refresh only"
        );
        db.zone(|tx| -> Result<()> {
            tx.execute("DELETE FROM jobs", [])?;
            tx.execute("UPDATE account SET app_version = '0.0.1'", [])?;
            Ok(())
        })
        .unwrap();
        db.zone(|tx| enqueue_at_start(tx, 3)).unwrap();
        assert_eq!(
            keys(&db),
            [
                "capabilities:",
                "key_maintenance:",
                "msl_purge:",
                "storage_sync:",
                "profile_refresh:",
                "sync_request:contacts",
                "sync_request:configuration",
                "sync_request:blocked"
            ]
        );
        let version: String = db
            .read(|c| -> Result<_> {
                Ok(c.query_row("SELECT app_version FROM account", [], |r| r.get(0))?)
            })
            .unwrap();
        assert_eq!(version, APP_VERSION);
    }

    /// A sync request of the link that is dropped (a failure no retry mends, or its day ran
    /// out) is asked again at the next start, with the other two; a dropped KEYS request or a
    /// dropped job of another kind changes nothing. (First real link, 2026-10-07: the three
    /// were dropped by a decode failure and never asked again.)
    #[test]
    fn a_dropped_sync_request_is_asked_again_at_the_next_start() {
        let (_dir, db) = linked_db(None);
        let sync_requests = |db: &ZoneDb| -> Vec<String> {
            db.read(|c| -> Result<_> {
                let mut stmt =
                    c.prepare("SELECT key FROM jobs WHERE kind = 'sync_request' ORDER BY id")?;
                Ok(stmt
                    .query_map([], |r| r.get(0))?
                    .collect::<kourier_db::rusqlite::Result<_>>()?)
            })
            .unwrap()
        };
        let version = |db: &ZoneDb| -> Option<String> {
            db.read(|c| -> Result<_> {
                Ok(c.query_row("SELECT app_version FROM account", [], |r| r.get(0))?)
            })
            .unwrap()
        };
        // The queued job of a kind and key, as the job loop would pick it.
        let job = |db: &ZoneDb, kind: JobKind, key: &str| -> Job {
            db.read(|c| -> Result<_> {
                Ok(c.query_row(
                    "SELECT id, attempts, expires_at, payload, requests FROM jobs
                     WHERE kind = ?1 AND key = ?2",
                    (kind.as_str(), key),
                    |r| {
                        Ok(Job {
                            id: r.get(0)?,
                            kind,
                            key: key.to_owned(),
                            attempts: r.get(1)?,
                            expires_at: r.get(2)?,
                            payload: r.get(3)?,
                            requests: r.get(4)?,
                        })
                    },
                )?)
            })
            .unwrap()
        };
        let fail = |db: &ZoneDb, job: &Job, now: i64, permanent: bool| -> jobs::Retry {
            db.zone(|tx| jobs::retry(tx, job, now, "failed", permanent, None))
                .unwrap()
        };
        db.zone(|tx| enqueue_after_link(tx, 1)).unwrap();
        assert_eq!(
            sync_requests(&db),
            ["contacts", "configuration", "blocked", "keys"]
        );
        assert_eq!(version(&db).as_deref(), Some(APP_VERSION));

        // KEYS dropped, and a job of another kind dropped: the mark stays.
        let keys = job(&db, JobKind::SyncRequest, "keys");
        assert!(matches!(fail(&db, &keys, 2, true), jobs::Retry::Dropped));
        let storage = job(&db, JobKind::StorageSync, "");
        assert!(matches!(fail(&db, &storage, 2, true), jobs::Retry::Dropped));
        assert_eq!(version(&db).as_deref(), Some(APP_VERSION));

        // A failure that is retried keeps the mark; the permanent one clears it.
        let contacts = job(&db, JobKind::SyncRequest, "contacts");
        assert!(matches!(fail(&db, &contacts, 2, false), jobs::Retry::At(_)));
        assert_eq!(version(&db).as_deref(), Some(APP_VERSION));
        assert!(matches!(
            fail(&db, &contacts, 3, true),
            jobs::Retry::Dropped
        ));
        assert_eq!(version(&db), None);
        assert_eq!(sync_requests(&db), ["configuration", "blocked"]);

        // The next start asks for all three again and marks them as asked: the one that went
        // out meanwhile (configuration) as well as the dropped one, and the one still queued
        // from before (blocked, after a failed attempt) gets its day of retries anew.
        let configuration = job(&db, JobKind::SyncRequest, "configuration");
        db.zone(|tx| jobs::finish(tx, &configuration, Done::Finished, 3))
            .unwrap();
        let blocked = job(&db, JobKind::SyncRequest, "blocked");
        assert!(matches!(fail(&db, &blocked, 3, false), jobs::Retry::At(_)));
        assert_eq!(sync_requests(&db), ["blocked"]);
        let day = i64::try_from(duration_ms(SYNC_REQUEST_MAX_AGE)).unwrap();
        let later = 4 + 3 * day;
        db.zone(|tx| enqueue_at_start(tx, later)).unwrap();
        assert_eq!(sync_requests(&db), ["blocked", "contacts", "configuration"]);
        assert_eq!(version(&db).as_deref(), Some(APP_VERSION));
        let blocked = job(&db, JobKind::SyncRequest, "blocked");
        assert_eq!(
            (blocked.attempts, blocked.expires_at),
            (0, Some(later + day))
        );

        // The phone's answer to CONTACTS that cannot be fetched: asked again as well.
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO jobs(kind, key, priority, next_at, created_at)
                 VALUES ('contact_sync', '', 0, 0, 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let answer = job(&db, JobKind::ContactSync, "");
        assert!(matches!(
            fail(&db, &answer, later, true),
            jobs::Retry::Dropped
        ));
        assert_eq!(version(&db), None);
        db.zone(|tx| enqueue_at_start(tx, later + 1)).unwrap();
        assert_eq!(version(&db).as_deref(), Some(APP_VERSION));

        // A request whose day ran out is dropped like a permanent failure.
        let blocked = job(&db, JobKind::SyncRequest, "blocked");
        let late = blocked.expires_at.expect("sync requests expire") + 1;
        assert!(matches!(
            fail(&db, &blocked, late, false),
            jobs::Retry::Dropped
        ));
        assert_eq!(version(&db), None);
    }

    #[test]
    fn rebuild_reemits_account_ready_and_stored_caps() {
        let (_dir, db) = linked_db(Some("aep"));
        let wake = Rc::new(Notify::new());
        let account = db::test_account();
        let blobs = tempfile::tempdir().unwrap();
        let avatar = crate::avatar::write(blobs.path(), b"old avatar").unwrap();
        let avatar_path = blobs.path().join(&avatar.blob.path);
        db.zone(|tx| -> Result<()> {
            tx.execute(
                "INSERT INTO peers(service_id, updated_at, avatar_blob, avatar_source)
                 VALUES ('p', 0, ?1, 'profile')",
                [avatar.blob.path.to_string_lossy().as_ref()],
            )?;
            Ok(())
        })
        .unwrap();
        rebuild_events(&db, &account, blobs.path(), 1, &wake).unwrap();
        assert!(
            !avatar_path.exists(),
            "avatar files of the lost core.db are deleted"
        );
        let names: Vec<&str> = outbox_events(&db).iter().map(BackendEvent::name).collect();
        assert_eq!(names, ["AccountReady"], "no config fetched yet: no caps");
        db.zone(|tx| {
            store_remote_config(
                tx,
                &Fetched::Config {
                    etag: None,
                    entries: BTreeMap::new(),
                },
                2,
                &wake,
            )
        })
        .unwrap();
        // A contact's PNI and ACI this device learned are one person (whichever source said
        // so): the rebuilt core has lost that and is told again.
        let aci = libsignal_core::Aci::from(uuid::Uuid::from_u128(0xa11ce));
        let pni = libsignal_core::Pni::from(uuid::Uuid::from_u128(0xa11ce0));
        db.zone(|tx| -> Result<()> {
            assert!(crate::links::learn(tx, &account, pni, aci, 2)?.is_some());
            tx.execute("DELETE FROM core_outbox", [])?;
            Ok(())
        })
        .unwrap();
        rebuild_events(&db, &account, blobs.path(), 3, &wake).unwrap();
        let events = outbox_events(&db);
        let names: Vec<&str> = events.iter().map(BackendEvent::name).collect();
        assert_eq!(names, ["AccountReady", "AccountCaps", "IdentityMerged"]);
        assert_eq!(
            events[2],
            BackendEvent::IdentityMerged {
                from: crate::service_id::pni_user_id(pni).unwrap(),
                into: crate::service_id::aci_user_id(aci).unwrap(),
            }
        );
    }

    /// Desktop's values for an account with a phone number, key for key
    /// (`SD/ts/textsecure/WebAPI.preload.ts:2962-2967`: `attachmentBackfill: true, spqr: true,
    /// usernameChangeSyncMessage: true, optionalPhoneNumber: !hasE164`). A flag below
    /// Desktop's could lower the account's capabilities on the server.
    #[test]
    fn capabilities_serialize_like_desktop() {
        let json = serde_json::to_value(CAPABILITIES).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "attachmentBackfill": true,
                "spqr": true,
                "usernameChangeSyncMessage": true,
                "optionalPhoneNumber": false,
            })
        );
        // Exactly Desktop's four keys: an extra one is something Desktop never claims.
        assert_eq!(json.as_object().map(serde_json::Map::len), Some(4));
    }
}
