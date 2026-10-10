//! Retry requests, both directions (§6.6 decryption failures, §6.7 step 7, §6.14 items 16-17;
//! ports `SD/ts/util/handleRetry.preload.ts`, `SD/ts/jobs/helpers/sendResendRequest.preload.ts`
//! and `SignalProtocolStore.preload.ts` `lightSessionReset`).
//!
//! **We could not decrypt** (stage 1 stores a row with the `DecryptionErrorMessage` for the
//! sender's device): stage 2 counts the attempt (≤ [`MAX_REQUESTS`] per message, `retry_state`),
//! queues a `retry_request` job that sends the message as `PlaintextContent` to the sender once
//! the account is through its catch-up, and, in the same zone, emits the placeholder: a
//! `Resendable` hint gives `Undecryptable{resendable: true}` with the would-be id (sender,
//! timestamp), which the resent message replaces in the core; the default hint gives
//! `Undecryptable{resendable: false}` (Desktop's "delivery issue"); `Implicit` gives nothing.
//! When retry requests are off (`desktop.senderKey.retry`) or the request cannot be sent, the
//! session is reset instead (archive it, send a null message on a new one), at most once an hour
//! per device.
//!
//! **Someone could not decrypt ours** (an incoming `DecryptionErrorMessage` for this device, at
//! most [`MAX_REQUESTS`] answered per requester device and message): our session with the
//! requester's device is archived when its ratchet key matches, and a `resend` job sends the
//! content the Message Send Log kept for that device and timestamp (≤ 14 days); when there is
//! none, a null message starts the new session if ours was archived, and nothing is sent
//! otherwise (Desktop's `sendDistributionMessageOrNullMessage`).

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use kourier_backend_api::{BackendEvent, IncomingMessage, clock::duration_ms};
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_model::{Body, ConvKey, Origin, Placeholder};
use libsignal_core::{DeviceId, ServiceId};
use libsignal_protocol::{
    ContentHint, DecryptionErrorMessage, PlaintextContent, ProtocolAddress, SessionStore as _,
};
use prost::Message as _;
use serde::{Deserialize, Serialize};

use crate::actor::Shared;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::SignalTransport;
use crate::proto;
use crate::receive::stage2::Row;
use crate::send::{self, Delivery, Outgoing};
use crate::service_id::{SignalUser, aci_user_id};
use crate::store::{IdentityKind, SqlSessionStore, archive_session, ready};
use crate::{maintenance, msl};

/// Most retry requests per failed message (Desktop's `RETRY_LIMIT`).
pub(crate) const MAX_REQUESTS: u32 = 5;
/// Shortest time between two automatic session resets with one device (Desktop: 1 hour).
pub(crate) const RESET_INTERVAL: Duration = Duration::from_secs(3600);
/// How long a retry request or resend job is tried.
const JOB_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
/// How long `retry_state` rows are kept.
const STATE_RETENTION: Duration = Duration::from_secs(2 * 24 * 3600);
/// The remote-config flag that enables retry requests.
pub(crate) const RETRY_FLAG: &str = "desktop.senderKey.retry";

fn ms(d: Duration) -> i64 {
    i64::try_from(duration_ms(d)).unwrap_or(i64::MAX)
}

/// The `jobs.key` of a retry for `(sender, device, timestamp)`.
fn key(sender: &str, device: u32, timestamp: u64) -> String {
    format!("{sender}.{device}:{timestamp}")
}

/// Parses a retry job key.
fn parse_key(key: &str) -> Option<(ServiceId, u32, u64)> {
    let (address, timestamp) = key.rsplit_once(':')?;
    let (sender, device) = address.rsplit_once('.')?;
    Some((
        ServiceId::parse_from_service_id_string(sender)?,
        device.parse().ok()?,
        timestamp.parse().ok()?,
    ))
}

/// Counts one more `kind` event for `(sender, device, timestamp)` and returns the count.
fn bump(
    conn: &Connection,
    kind: &str,
    sender: &str,
    device: u32,
    timestamp: u64,
    now: i64,
) -> Result<u32> {
    conn.prepare_cached(
        "INSERT INTO retry_state(kind, sender, device, timestamp, attempts, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, ?5)
         ON CONFLICT(kind, sender, device, timestamp)
         DO UPDATE SET attempts = attempts + 1, updated_at = excluded.updated_at",
    )?
    .execute((
        kind,
        sender,
        device,
        i64::try_from(timestamp).unwrap_or(i64::MAX),
        now,
    ))?;
    Ok(conn
        .prepare_cached(
            "SELECT attempts FROM retry_state
             WHERE kind = ?1 AND sender = ?2 AND device = ?3 AND timestamp = ?4",
        )?
        .query_row(
            (
                kind,
                sender,
                device,
                i64::try_from(timestamp).unwrap_or(i64::MAX),
            ),
            |r| r.get(0),
        )?)
}

/// Deletes `retry_state` rows older than two days (with the MSL purge).
pub(crate) fn purge(conn: &Connection, now: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM retry_state WHERE updated_at < ?1")?
        .execute([now.saturating_sub(ms(STATE_RETENTION))])?;
    Ok(())
}

/// A content hint as stored in `inbox.content_hint`.
fn hint_of(value: Option<u32>) -> ContentHint {
    value.map_or(ContentHint::Default, ContentHint::from)
}

/// Stage 2 for a row that did not decrypt: the retry request job and the placeholder (module
/// docs). Returns the core events.
pub(crate) fn on_decryption_failure(
    conn: &Connection,
    account: &Account,
    row: &Row,
    dem: &[u8],
    catch_up: bool,
    now: i64,
) -> Result<Vec<BackendEvent>> {
    let Some(ServiceId::Aci(sender)) = ServiceId::parse_from_service_id_string(&row.sender) else {
        return Ok(Vec::new());
    };
    let attempts = bump(
        conn,
        "request",
        &row.sender,
        row.sender_device,
        row.timestamp,
        now,
    )?;
    if attempts <= MAX_REQUESTS {
        jobs::enqueue_payload(
            conn,
            JobKind::RetryRequest,
            &key(&row.sender, row.sender_device, row.timestamp),
            dem,
            now,
            Some(JOB_MAX_AGE),
        )?;
    } else {
        tracing::info!(
            sender = ?SignalUser::Aci(sender),
            attempts,
            "decryption failed again; no more retry requests for this message"
        );
    }
    let resendable = match hint_of(row.content_hint) {
        ContentHint::Resendable => true,
        ContentHint::Implicit => return Ok(Vec::new()),
        _ => false,
    };
    if sender == account.aci {
        // A message from our other device: no chat to put a placeholder in.
        return Ok(Vec::new());
    }
    let Ok(author) = aci_user_id(sender) else {
        return Ok(Vec::new());
    };
    let conv = match row.group_id.as_deref() {
        Some(id) => match crate::groups::load_by_id(conn, id)? {
            Some(group) if group.features_hash.is_some() => group.keys.conv_key(),
            _ => ConvKey::Direct(author.clone()),
        },
        None => ConvKey::Direct(author.clone()),
    };
    let mut message = IncomingMessage::new(
        conv,
        row.timestamp.to_string(),
        Origin::Remote {
            author,
            device: Some(row.sender_device),
        },
        Body::Placeholder(Placeholder::Undecryptable { resendable }),
        i64::try_from(row.timestamp).unwrap_or(i64::MAX),
        row.received_at,
    );
    message.server_at = row.server_timestamp;
    message.arrival = Some(row.arrival);
    message.catch_up = catch_up;
    Ok(vec![BackendEvent::Message(message)])
}

/// What a `resend` job sends.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Resend {
    /// The original timestamp.
    timestamp: u64,
    /// The logged `Content` (base64), or `None` for a null message.
    content: Option<String>,
    /// Its content hint.
    hint: u32,
    /// Its urgency.
    urgent: bool,
    /// The group (id in hex) whose sender-key distribution message goes with the resend.
    #[serde(default)]
    group: Option<String>,
}

/// Stage 2 for an incoming `DecryptionErrorMessage`: archive the session it names and queue the
/// resend (module docs). Returns a log-safe note.
pub(crate) fn on_retry_request(
    conn: &Connection,
    account: &Account,
    row: &Row,
    bytes: &[u8],
    now: i64,
) -> Result<&'static str> {
    let Ok(request) = DecryptionErrorMessage::try_from(bytes) else {
        return Ok("malformed retry request");
    };
    if request.device_id() != u32::from(account.device_id) {
        return Ok("retry request for another device");
    }
    let Some(requester @ ServiceId::Aci(_)) = ServiceId::parse_from_service_id_string(&row.sender)
    else {
        return Ok("retry request from a non-ACI sender");
    };
    let Ok(device) = DeviceId::try_from(row.sender_device) else {
        return Ok("retry request from an invalid device");
    };
    let timestamp = request.timestamp().epoch_millis();
    let answered = bump(
        conn,
        "resend",
        &row.sender,
        row.sender_device,
        timestamp,
        now,
    )?;
    if answered > MAX_REQUESTS {
        return Ok("retry request over the limit for this message; ignored");
    }
    let mut archived = false;
    if let Some(ratchet_key) = request.ratchet_key() {
        let address = ProtocolAddress::new(row.sender.clone(), device);
        let store = SqlSessionStore::new(conn, IdentityKind::Aci, now);
        let matches = ready(store.load_session(&address))??
            .map(|session| session.current_ratchet_key_matches(ratchet_key))
            .transpose()?
            .unwrap_or(false);
        if matches {
            archive_session(conn, IdentityKind::Aci, &row.sender, device, now)?;
            archived = true;
            tracing::info!("retry request: our session with the requester was archived");
        }
    }
    let logged = msl::find(conn, &row.sender, row.sender_device, timestamp)?;
    // The group of the failed message: from the log, else from the request's sealed envelope
    // (Desktop seals retry requests with the group id of the message they are about).
    let group = match logged
        .as_ref()
        .and_then(|l| l.group_id.clone())
        .or_else(|| row.group_id.clone())
    {
        Some(id) if crate::send::sender_key::has_chain(conn, &id)? => Some(id),
        _ => None,
    };
    let resend = match logged {
        Some(logged) => Resend {
            timestamp,
            content: Some(STANDARD.encode(logged.content)),
            hint: u32::try_from(logged.hint).unwrap_or(0),
            urgent: logged.urgent,
            group: group.as_deref().map(crate::groups::job_key),
        },
        // Nothing logged for a group message: our distribution message alone lets the member
        // read what follows (`sendDistributionMessageOrNullMessage`).
        None if group.is_some() => Resend {
            timestamp,
            content: None,
            hint: 0,
            urgent: false,
            group: group.as_deref().map(crate::groups::job_key),
        },
        // Nothing to resend: a null message only starts the new session where ours was just
        // archived; the old one still works otherwise.
        None if archived => Resend {
            timestamp,
            content: None,
            hint: 0,
            urgent: false,
            group: None,
        },
        None => return Ok("retry request for a message the send log does not have"),
    };
    let payload =
        serde_json::to_vec(&resend).map_err(|_| Error::Internal("resend does not serialize"))?;
    jobs::enqueue_payload(
        conn,
        JobKind::Resend,
        &key(&row.sender, row.sender_device, timestamp),
        &payload,
        now,
        Some(JOB_MAX_AGE),
    )?;
    tracing::debug!(
        requester = ?log_safe(requester),
        logged = resend.content.is_some(),
        "retry request queued"
    );
    Ok(if resend.content.is_some() {
        "retry request answered from the send log"
    } else {
        "retry request answered with a null message"
    })
}

fn log_safe(service_id: ServiceId) -> SignalUser {
    match service_id {
        ServiceId::Aci(aci) => SignalUser::Aci(aci),
        ServiceId::Pni(pni) => SignalUser::Pni(pni),
    }
}

/// Whether retry requests are on (`desktop.senderKey.retry`; Desktop treats an absent flag as
/// off).
fn retry_enabled(conn: &Connection) -> Result<bool> {
    let config = maintenance::load_remote_config(conn)?;
    Ok(config
        .get(RETRY_FLAG)
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("true")))
}

/// The `retry_request` job: the `DecryptionErrorMessage` as `PlaintextContent` to the sender.
pub(crate) async fn run_request<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let Some((sender, device, timestamp)) = parse_key(&job.key) else {
        return Ok(Done::Finished);
    };
    if !shared.db.read(retry_enabled)? {
        reset_session(shared, socket, &account, sender, device).await?;
        return Ok(Done::Finished);
    }
    let Some(request) = job
        .payload
        .as_deref()
        .and_then(|bytes| DecryptionErrorMessage::try_from(bytes).ok())
    else {
        return Ok(Done::Finished);
    };
    let plaintext = PlaintextContent::from(request);
    let ctx = shared.send_context(&account, socket);
    match send::send_plaintext_to(&ctx, sender, &plaintext, timestamp, Delivery::RETRY).await {
        Ok(_) => {
            tracing::info!(sender = ?log_safe(sender), "retry request sent");
            Ok(Done::Finished)
        }
        Err(e) if jobs::is_permanent(&e) => {
            tracing::warn!(error = %e, "retry request refused; resetting the session instead");
            reset_session(shared, socket, &account, sender, device).await?;
            Ok(Done::Finished)
        }
        Err(e) => Err(e),
    }
}

/// The `resend` job: the logged content again, or a null message.
pub(crate) async fn run_resend<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let Some((requester, _device, _)) = parse_key(&job.key) else {
        return Ok(Done::Finished);
    };
    let Some(resend) = job
        .payload
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<Resend>(bytes).ok())
    else {
        return Ok(Done::Finished);
    };
    let ctx = shared.send_context(&account, socket);
    let own = requester == ServiceId::from(account.aci);
    // Our distribution message for the group of the failed message, when the requester is a
    // member: they may have lost our sender key (`maybeAddSenderKeyDistributionMessage`).
    let group = resend
        .group
        .as_deref()
        .and_then(crate::groups::parse_job_key);
    let distribution = match &group {
        Some(id) if !own => {
            let now = shared.now();
            shared.db.zone(|tx| -> Result<Option<Vec<u8>>> {
                let member = crate::groups::load_by_id(tx, id)?
                    .and_then(|row| row.snapshot)
                    .is_some_and(|snapshot| match requester {
                        ServiceId::Aci(aci) => snapshot.is_member(aci),
                        ServiceId::Pni(_) => false,
                    });
                match crate::send::sender_key::chain(tx, id)? {
                    Some(chain) if member => Ok(Some(
                        crate::send::sender_key::distribution_message(tx, &account, &chain, now)?,
                    )),
                    _ => Ok(None),
                }
            })?
        }
        _ => None,
    };
    let in_group = group.as_deref().map(|id| send::InGroup {
        id,
        token: None,
        named: true,
    });
    let logged = match resend.content.as_deref() {
        Some(content) => {
            let bytes = STANDARD
                .decode(content)
                .map_err(|_| Error::Decode("logged content"))?;
            Some(proto::Content::decode(&bytes[..]).map_err(|_| Error::Decode("logged content"))?)
        }
        None => None,
    };
    match (logged, distribution) {
        (Some(mut content), distribution) => {
            let with_key = distribution.is_some();
            if let Some(message) = distribution {
                content.sender_key_distribution_message = Some(message);
            }
            send::send_to(
                &ctx,
                requester,
                &Outgoing {
                    content: &content,
                    timestamp: resend.timestamp,
                    delivery: Delivery {
                        failpoint: None,
                        urgent: resend.urgent,
                        online: false,
                        hint: ContentHint::from(resend.hint),
                        log: true,
                        may_seal: !own,
                    },
                    local: None,
                    group: in_group,
                },
            )
            .await?;
            tracing::info!(
                requester = ?log_safe(requester),
                sender_key = with_key,
                "logged content resent"
            );
        }
        (None, Some(message)) => {
            let content = crate::send::content::distribution_content(message);
            send::send_to(
                &ctx,
                requester,
                &Outgoing {
                    content: &content,
                    timestamp: shared.unique_timestamp(),
                    delivery: Delivery::MESSAGE,
                    local: None,
                    group: in_group,
                },
            )
            .await?;
            tracing::info!(
                requester = ?log_safe(requester),
                "nothing logged for the retry request; our sender key sent"
            );
        }
        (None, None) => {
            send_null_message(shared, socket, &account, requester).await?;
            tracing::info!(
                requester = ?log_safe(requester),
                "nothing logged for the retry request; null message sent"
            );
        }
    }
    Ok(Done::Finished)
}

/// A `NullMessage` to every device of `destination` (`sendNullMessage`): starts a fresh session
/// where ours was archived.
async fn send_null_message<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    account: &Account,
    destination: ServiceId,
) -> Result<()> {
    let content = proto::Content {
        content: Some(proto::content::Content::NullMessage(proto::NullMessage {
            padding: Some(send::content::random_padding()),
        })),
        ..Default::default()
    };
    let ctx = shared.send_context(account, socket);
    send::send_to(
        &ctx,
        destination,
        &Outgoing {
            content: &content,
            timestamp: shared.unique_timestamp(),
            delivery: Delivery::RETRY,
            local: None,
            group: None,
        },
    )
    .await
    .map(drop)
}

/// The light session reset (`lightSessionReset`): archive the session with `device` of
/// `sender` and send a null message on a new one, at most once per [`RESET_INTERVAL`].
async fn reset_session<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    account: &Account,
    sender: ServiceId,
    device: u32,
) -> Result<()> {
    let sid = sender.service_id_string();
    let now = shared.now();
    let reset = shared.db.zone(|tx| -> Result<bool> {
        let last: Option<i64> = tx
            .prepare_cached(
                "SELECT updated_at FROM retry_state
                 WHERE kind = 'reset' AND sender = ?1 AND device = ?2 AND timestamp = 0",
            )?
            .query_row((&sid, device), |r| r.get(0))
            .optional()?;
        if last.is_some_and(|at| now.saturating_sub(at) < ms(RESET_INTERVAL)) {
            return Ok(false);
        }
        bump(tx, "reset", &sid, device, 0, now)?;
        if let Ok(device) = DeviceId::try_from(device) {
            archive_session(tx, IdentityKind::Aci, &sid, device, now)?;
        }
        Ok(true)
    })?;
    if !reset {
        tracing::info!(sender = ?log_safe(sender), "session reset skipped (one per hour)");
        return Ok(());
    }
    tracing::info!(sender = ?log_safe(sender), "session reset after a decryption failure");
    send_null_message(shared, socket, account, sender).await
}
