//! Receipts, read syncs and typing (§4.8 "durable commands", §6.7 step 6;
//! `SD/ts/util/sendReceipts.preload.ts`, `SD/ts/util/deliveryReceipt.preload.ts`,
//! `SD/ts/util/markConversationRead.preload.ts`, `SendMessage.preload.ts` `syncReadMessages`,
//! `conversations.preload.ts` typing).
//!
//! - `SendReceipts` / `SyncReadMarker` are durable commands: one zone inserts the job (keyed by
//!   the core's `cmd` and a digest of its work, `jobs::enqueue_command`) and
//!   `CommandPersisted{cmd}` into `core_outbox`, so the core forgets its `pending_commands` row
//!   only once the backend can no longer lose the work. A repeated command finds its job (or
//!   its `done_commands` record) and is confirmed again. Read and viewed receipts go only to accepted
//!   chats and only with the read-receipt setting on; read syncs always. Both run once the
//!   account is through its catch-up.
//! - In a group, a read or viewed receipt goes to the author of each message, one 1:1 receipt
//!   per author, as Desktop sends them (`sendReceipts.preload.ts`: grouped by conversation and
//!   sender, `shouldSendToDirectConversation(sender)`, `getSendOptions(sender, {groupId})`;
//!   `markConversationRead.preload.ts`: only from an accepted conversation). It is sealed with
//!   the author's access key, or with their group send token when sealed sender is off for
//!   them; it does not name the group and carries no PNI signature
//!   (`SendMessage.preload.ts` `#sendReceiptMessage`). None goes for a group that is blocked,
//!   not accepted or that we left, and none to an author who is blocked, unregistered, or
//!   whose own 1:1 chat is a message request we have not accepted.
//! - Delivery receipts are backend-internal: stage 2 queues one for each sealed message in an
//!   accepted chat (`delivery_receipts`), the `delivery_receipts` job sends them 500 ms later in
//!   batches of ≤ 100 per sender.
//! - Typing indicators are fire and forget (setting-gated, accepted 1:1 chats, `online`).
//! - Nothing of this goes to a blocked contact: no read or viewed receipt is queued for one,
//!   and the receipts owed to someone blocked since are forgotten when their job runs.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_db::Tx;
use kourier_db::rusqlite::Connection;
use kourier_model::{CmdId, ConvKey, MsgTarget, ReceiptKind, TargetAuthor};
use libsignal_core::{Aci, ServiceId};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::actor::Shared;
use crate::db::{self, Account};
use crate::error::{Error, Result};
use crate::features;
use crate::groups;
use crate::jobs::{self, Done, Job, JobKind, is_permanent};
use crate::net::SignalTransport;
use crate::outbox::append_events;
use crate::peers;
use crate::send::content::{self, ReceiptType};
use crate::send::{self, Delivery, Outgoing, SendContext};
use crate::service_id::{SignalUser, parse_user_id};

/// The failpoint after a durable command's job and `CommandPersisted` committed (§10.4).
pub(crate) const FAILPOINT_AFTER_COMMAND_PERSISTED: &str = "after_command_persisted";

/// Most timestamps in one receipt or read sync (`CHUNK_SIZE`, Desktop's batcher size).
pub(crate) const MAX_PER_MESSAGE: usize = 100;
/// How long delivery receipts wait for more of their kind (`deliveryReceiptBatcher.wait`).
pub(crate) const DELIVERY_RECEIPT_WAIT: Duration = Duration::from_millis(500);
/// Senders served per run of the delivery-receipt job; the rest run right after.
const DELIVERY_PEERS_PER_RUN: i64 = 20;

/// What a `receipts` job sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptsPayload {
    /// `read` or `viewed`.
    kind: String,
    /// One entry per author.
    recipients: Vec<ReceiptBatch>,
    /// The group the messages were read in (its id in hex, [`groups::job_key`]); `None` for a
    /// 1:1 chat. Old jobs lack the field: they are 1:1 receipts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
}

/// Receipts for one author.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReceiptBatch {
    /// The author's ACI (service id string).
    peer: String,
    /// Sent timestamps of their messages.
    timestamps: Vec<u64>,
}

/// What a `read_sync` job sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ReadSyncPayload {
    /// `(author ACI service id string, sent timestamp)` of each message read.
    reads: Vec<(String, u64)>,
}

/// The ACI of a target written by someone else.
fn author_aci(target: &MsgTarget) -> Option<Aci> {
    let TargetAuthor::User(user) = &target.author else {
        return None;
    };
    match parse_user_id(user).ok()? {
        SignalUser::Aci(aci) => Some(aci),
        SignalUser::Pni(_) | SignalUser::E164(_) => None,
    }
}

/// Confirms `cmd` (its work is safe in `backend.db` now, or there is none).
fn confirm(tx: &mut Tx<'_>, cmd: CmdId, now: i64, wake: &Rc<Notify>) -> Result<()> {
    append_events(tx, &[BackendEvent::CommandPersisted { cmd }], now, wake)?;
    Ok(())
}

/// The job of `SendReceipts`, if one is due (setting, accepted chat, someone else's messages).
fn receipts_payload(
    conn: &Connection,
    account: &Account,
    conv: &ConvKey,
    targets: &[MsgTarget],
    kind: ReceiptKind,
) -> Result<Option<ReceiptsPayload>> {
    let kind = match kind {
        ReceiptKind::Read => "read",
        ReceiptKind::Viewed => "viewed",
        // Delivery receipts are the backend's own (`queue_delivery_receipt`).
        ReceiptKind::Delivered => return Ok(None),
    };
    if !db::synced_settings(conn)?.read_receipts {
        return Ok(None);
    }
    let (recipients, group) = match conv {
        ConvKey::Direct(user) => {
            let Ok(SignalUser::Aci(peer)) = parse_user_id(user) else {
                return Ok(None);
            };
            if peer == account.aci || !features::is_accepted(conn, account, peer.into())? {
                return Ok(None);
            }
            // Nothing goes to a blocked contact, receipts included (Desktop's
            // `isConversationAccepted` is false for a blocked chat).
            if peers::load(conn, &ServiceId::from(peer).service_id_string())?.blocked {
                return Ok(None);
            }
            let timestamps: Vec<u64> = targets
                .iter()
                .filter(|target| author_aci(target) == Some(peer))
                .filter_map(|target| target.local.parse().ok())
                .collect();
            let batch = ReceiptBatch {
                peer: ServiceId::from(peer).service_id_string(),
                timestamps,
            };
            (vec![batch], None)
        }
        ConvKey::Group(_) => {
            let Some(id) = groups::id_of_conv(conv) else {
                return Ok(None);
            };
            if !group_takes_receipts(conn, &id)? {
                return Ok(None);
            }
            (
                group_batches(conn, account, targets)?,
                Some(groups::job_key(&id)),
            )
        }
        _ => return Ok(None),
    };
    let recipients: Vec<ReceiptBatch> = recipients
        .into_iter()
        .filter(|batch| !batch.timestamps.is_empty())
        .collect();
    if recipients.is_empty() {
        return Ok(None);
    }
    Ok(Some(ReceiptsPayload {
        kind: kind.to_owned(),
        recipients,
        group,
    }))
}

/// Whether messages read in the group `id` are answered with receipts: we are a member, and
/// the group is accepted and not blocked (Desktop's `markConversationRead` queues receipts
/// only when `isConversationAccepted(conversation)`; `groups.whitelisted` is the phone's
/// accept, from the Storage Service or its message-request response).
fn group_takes_receipts(conn: &Connection, id: &[u8]) -> Result<bool> {
    let Some(row) = groups::load_by_id(conn, id)? else {
        return Ok(false);
    };
    if row.membership != groups::Membership::Member || row.blocked {
        return Ok(false);
    }
    groups::whitelisted(conn, id)
}

/// Whether the author of a group message gets our read receipt (Desktop's
/// `shouldSendToDirectConversation(sender)`): not when they are blocked or unregistered, and
/// not while their own 1:1 chat is a message request we have not accepted. An author we have
/// no 1:1 chat with at all gets it (`isConversationAccepted` is true for an empty chat).
fn author_takes_receipts(conn: &Connection, service_id: &str) -> Result<bool> {
    let peer = peers::load(conn, service_id)?;
    Ok(!peer.blocked && !peer.unregistered && (peer.accepted() || peer.features_hash.is_none()))
}

/// The receipts for messages read in a group: one batch per author (an ACI other than ours
/// who takes receipts), authors in the order they first appear, timestamps in the order of
/// `targets`.
fn group_batches(
    conn: &Connection,
    account: &Account,
    targets: &[MsgTarget],
) -> Result<Vec<ReceiptBatch>> {
    let mut batches: Vec<ReceiptBatch> = Vec::new();
    let mut refused: Vec<String> = Vec::new();
    for target in targets {
        let (Some(author), Ok(timestamp)) = (author_aci(target), target.local.parse::<u64>())
        else {
            continue;
        };
        if author == account.aci {
            continue;
        }
        let peer = ServiceId::from(author).service_id_string();
        if refused.contains(&peer) {
            continue;
        }
        if let Some(batch) = batches.iter_mut().find(|batch| batch.peer == peer) {
            batch.timestamps.push(timestamp);
        } else if author_takes_receipts(conn, &peer)? {
            batches.push(ReceiptBatch {
                peer,
                timestamps: vec![timestamp],
            });
        } else {
            refused.push(peer);
        }
    }
    Ok(batches)
}

/// `Command::SendReceipts`: the job (when receipts are due) and `CommandPersisted` in one zone.
/// Returns whether a job was added.
#[allow(
    clippy::too_many_arguments,
    reason = "the command's fields plus the zone's context"
)]
pub(crate) fn persist_receipts(
    tx: &mut Tx<'_>,
    account: &Account,
    cmd: CmdId,
    conv: &ConvKey,
    targets: &[MsgTarget],
    kind: ReceiptKind,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let added = match receipts_payload(tx, account, conv, targets, kind)? {
        Some(payload) => {
            let payload =
                serde_json::to_vec(&payload).map_err(|_| Error::Internal("receipt payload"))?;
            jobs::enqueue_command(tx, JobKind::Receipts, cmd.0, &payload, now)?
        }
        None => false,
    };
    confirm(tx, cmd, now, wake)?;
    Ok(added)
}

/// `Command::SyncReadMarker`: the read-sync job (for messages of others) and `CommandPersisted`
/// in one zone. `read` lists the messages that became read; without it, `up_to` alone.
pub(crate) fn persist_read_sync(
    tx: &mut Tx<'_>,
    up_to: &MsgTarget,
    read: &[MsgTarget],
    cmd: CmdId,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let targets = if read.is_empty() {
        std::slice::from_ref(up_to)
    } else {
        read
    };
    let reads: Vec<(String, u64)> = targets
        .iter()
        .filter_map(|target| {
            let aci = author_aci(target)?;
            let timestamp = target.local.parse().ok()?;
            Some((ServiceId::from(aci).service_id_string(), timestamp))
        })
        .collect();
    let added = if reads.is_empty() {
        false
    } else {
        let payload = serde_json::to_vec(&ReadSyncPayload { reads })
            .map_err(|_| Error::Internal("read sync payload"))?;
        jobs::enqueue_command(tx, JobKind::ReadSync, cmd.0, &payload, now)?
    };
    confirm(tx, cmd, now, wake)?;
    Ok(added)
}

/// Queues a delivery receipt for the message `peer` (an ACI service id string) sent at
/// `timestamp`, in the caller's zone; the job runs [`DELIVERY_RECEIPT_WAIT`] later.
pub(crate) fn queue_delivery_receipt(
    conn: &Connection,
    peer: &str,
    timestamp: u64,
    now: i64,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO delivery_receipts(peer, timestamp, created_at) VALUES (?1, ?2, ?3)",
    )?
    .execute((peer, i64::try_from(timestamp).unwrap_or(i64::MAX), now))?;
    let wait = i64::try_from(DELIVERY_RECEIPT_WAIT.as_millis()).unwrap_or(0);
    jobs::schedule_at(
        conn,
        JobKind::DeliveryReceipts,
        "",
        now.saturating_add(wait),
    )
}

/// The group a receipt was read in: its id and the author's group send token, if we have one.
#[derive(Clone, Copy)]
struct ReceiptGroup<'a> {
    id: &'a [u8],
    token: Option<&'a [u8]>,
}

/// Sends one receipt to `peer`, sealed when possible. For a message of a 1:1 chat it carries
/// our PNI signature while they know us by number only; for a message of a group
/// (`in_group`) it carries none and may be sealed with the author's group send token.
async fn send_receipt<T: SignalTransport>(
    shared: &Shared<T>,
    ctx: &SendContext<'_, T>,
    peer: Aci,
    kind: ReceiptType,
    timestamps: &[u64],
    in_group: Option<ReceiptGroup<'_>>,
) -> Result<()> {
    let destination = ServiceId::from(peer);
    let mut receipt = content::receipt(kind, timestamps);
    if in_group.is_none()
        && ctx
            .db
            .read(|c| peers::load(c, &destination.service_id_string()))?
            .share_my_number
    {
        content::add_pni_signature(&mut receipt, ctx.account)?;
    }
    send::send_to(
        ctx,
        destination,
        &Outgoing {
            content: &receipt,
            timestamp: shared.unique_timestamp(),
            delivery: Delivery::RECEIPT,
            local: None,
            group: in_group.map(|group| send::InGroup {
                id: group.id,
                token: group.token,
                named: false,
            }),
        },
    )
    .await
    .map(|_| ())
}

fn parse_aci(service_id: &str) -> Option<Aci> {
    match ServiceId::parse_from_service_id_string(service_id)? {
        ServiceId::Aci(aci) => Some(aci),
        ServiceId::Pni(_) => None,
    }
}

/// What is left of `payload` when the receipt for recipient number `index` failed after
/// `sent` of that recipient's timestamps went out: that recipient's other timestamps and
/// every recipient behind it. `None` when nothing went out yet (the job is as it was).
fn receipts_left(payload: &ReceiptsPayload, index: usize, sent: usize) -> Option<ReceiptsPayload> {
    if index == 0 && sent == 0 {
        return None;
    }
    let mut recipients = payload.recipients.get(index..)?.to_vec();
    let first = recipients.first_mut()?;
    first.timestamps.drain(..sent.min(first.timestamps.len()));
    Some(ReceiptsPayload {
        kind: payload.kind.clone(),
        recipients,
        group: payload.group.clone(),
    })
}

/// Runs a `receipts` job. A recipient that can never get it (unregistered) is skipped; so is
/// one blocked since the job was queued, and every author of a group that was blocked or
/// left since. When a receipt has to wait (a rate limit, a challenge, the network), the job
/// keeps only what was not sent, so the authors served before it are not told again at
/// every retry.
pub(crate) async fn run_receipts<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let Some(payload) = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_slice::<ReceiptsPayload>(p).ok())
    else {
        tracing::warn!("receipt job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    let kind = if payload.kind == "viewed" {
        ReceiptType::Viewed
    } else {
        ReceiptType::Read
    };
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    // The group's id and its send endorsements (a token seals a receipt for an author whose
    // sealed sender is off; without usable endorsements the receipt goes with the access key
    // or unsealed: a receipt is never worth a group refresh).
    let group = match payload.group.as_deref().map(groups::parse_job_key) {
        None => None,
        Some(None) => {
            tracing::warn!("receipt job with a malformed group dropped");
            return Ok(Done::Finished);
        }
        Some(Some(id)) => {
            if !shared.db.read(|c| group_takes_receipts(c, &id))? {
                tracing::info!(
                    group = %groups::log_id(&id),
                    "receipts for a group we left, blocked or never accepted dropped"
                );
                return Ok(Done::Finished);
            }
            let now = shared.now();
            let endorsements = match shared.zk_params.as_ref() {
                Some(server) => shared.db.read(|c| -> Result<_> {
                    match groups::load_by_id(c, &id)? {
                        Some(row) => groups::endorsements::load(c, &row, server, now),
                        None => Ok(None),
                    }
                })?,
                None => None,
            };
            Some((id, endorsements))
        }
    };
    for (index, batch) in payload.recipients.iter().enumerate() {
        let Some(peer) = parse_aci(&batch.peer) else {
            continue;
        };
        // Blocked since the job was queued: they get nothing any more.
        if shared.db.read(|c| peers::load(c, &batch.peer))?.blocked {
            continue;
        }
        let token = group
            .as_ref()
            .and_then(|(_, endorsements)| endorsements.as_ref())
            .and_then(|endorsements| endorsements.token(&[peer]));
        let in_group = group.as_ref().map(|(id, _)| ReceiptGroup {
            id,
            token: token.as_deref(),
        });
        for (chunks_sent, chunk) in batch.timestamps.chunks(MAX_PER_MESSAGE).enumerate() {
            match send_receipt(shared, &ctx, peer, kind, chunk, in_group).await {
                Ok(()) => {}
                Err(e) if is_permanent(&e) => {
                    tracing::info!(error = %e, "receipts to a recipient dropped");
                    break;
                }
                Err(e) => {
                    // The job runs again later: only for what is still owed.
                    if let Some(left) =
                        receipts_left(&payload, index, chunks_sent * MAX_PER_MESSAGE)
                    {
                        let left = serde_json::to_vec(&left)
                            .map_err(|_| Error::Internal("receipt payload"))?;
                        shared.db.zone(|tx| jobs::set_payload(tx, job, &left))?;
                    }
                    return Err(e);
                }
            }
        }
    }
    Ok(Done::Finished)
}

/// What is left of `payload` when the sync message that carried its entries from number
/// `sent` on failed: those entries. `None` when nothing went out yet (the job is as it was).
/// Entries are kept whole; a message is [`MAX_PER_MESSAGE`] of them, so `sent` is a
/// multiple of it.
fn reads_left(payload: &ReadSyncPayload, sent: usize) -> Option<ReadSyncPayload> {
    if sent == 0 {
        return None;
    }
    Some(ReadSyncPayload {
        reads: payload.reads.get(sent..)?.to_vec(),
    })
}

/// Runs a `read_sync` job: `SyncMessage.read` to our other devices, ≤ 100 entries each.
///
/// The first read of a chat that came with a history import lists every message of it that
/// was unread (tens of thousands), so the job is many sync messages. One that has to wait
/// (a rate limit, the network) keeps only the entries that were not sent, as a `receipts`
/// job does, or every retry would start again from the first message.
pub(crate) async fn run_read_sync<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let Some(mut payload) = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_slice::<ReadSyncPayload>(p).ok())
    else {
        tracing::warn!("read sync job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    // What cannot be sent to anybody is passed over for good, so a position in `reads` is a
    // position in the payload.
    payload
        .reads
        .retain(|(sender, _)| parse_aci(sender).is_some());
    let reads: Vec<(Aci, u64)> = payload
        .reads
        .iter()
        .filter_map(|(sender, timestamp)| Some((parse_aci(sender)?, *timestamp)))
        .collect();
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    for (number, chunk) in reads.chunks(MAX_PER_MESSAGE).enumerate() {
        let sync = content::read_sync(chunk);
        let sent = send::send_to(
            &ctx,
            account.aci.into(),
            &Outgoing {
                content: &sync,
                timestamp: shared.unique_timestamp(),
                delivery: Delivery::READ_SYNC,
                local: None,
                group: None,
            },
        )
        .await;
        if let Err(e) = sent {
            // The job runs again later: only for what is still owed.
            if let Some(left) = reads_left(&payload, number * MAX_PER_MESSAGE) {
                let left =
                    serde_json::to_vec(&left).map_err(|_| Error::Internal("read sync payload"))?;
                shared.db.zone(|tx| jobs::set_payload(tx, job, &left))?;
            }
            return Err(e);
        }
    }
    tracing::debug!(entries = reads.len(), "read sync sent");
    Ok(Done::Finished)
}

/// The delivery receipts owed, ≤ 100 of the oldest per sender, for at most
/// [`DELIVERY_PEERS_PER_RUN`] senders.
fn owed_delivery_receipts(conn: &Connection) -> Result<Vec<(String, Vec<u64>)>> {
    let peers: Vec<String> = conn
        .prepare_cached(
            "SELECT peer FROM delivery_receipts GROUP BY peer ORDER BY min(created_at) LIMIT ?1",
        )?
        .query_map([DELIVERY_PEERS_PER_RUN], |r| r.get(0))?
        .collect::<kourier_db::rusqlite::Result<_>>()?;
    let mut owed = Vec::with_capacity(peers.len());
    for peer in peers {
        let timestamps: Vec<i64> = conn
            .prepare_cached(
                "SELECT timestamp FROM delivery_receipts WHERE peer = ?1
                 ORDER BY created_at, timestamp LIMIT ?2",
            )?
            .query_map(
                (&peer, i64::try_from(MAX_PER_MESSAGE).unwrap_or(100)),
                |r| r.get(0),
            )?
            .collect::<kourier_db::rusqlite::Result<_>>()?;
        owed.push((
            peer,
            timestamps
                .into_iter()
                .filter_map(|t| u64::try_from(t).ok())
                .collect(),
        ));
    }
    Ok(owed)
}

/// Forgets the delivery receipts owed to senders that are blocked now (blocked after their
/// message arrived): a receipt would tell them this device still takes their messages, and
/// Desktop sends none to a blocked chat. Returns how many were dropped.
fn forget_blocked_delivery_receipts(conn: &Connection) -> Result<usize> {
    Ok(conn
        .prepare_cached(
            "DELETE FROM delivery_receipts
             WHERE peer IN (SELECT service_id FROM peers WHERE blocked = 1)",
        )?
        .execute([])?)
}

/// Forgets delivery receipts that were sent (or can never be).
fn forget_delivery_receipts(conn: &Connection, peer: &str, timestamps: &[u64]) -> Result<()> {
    for timestamp in timestamps {
        conn.prepare_cached("DELETE FROM delivery_receipts WHERE peer = ?1 AND timestamp = ?2")?
            .execute((peer, i64::try_from(*timestamp).unwrap_or(i64::MAX)))?;
    }
    Ok(())
}

/// Runs the `delivery_receipts` job: one receipt per sender with what is owed; again at once
/// while more is owed. A transient failure keeps that sender's receipts and retries the job.
pub(crate) async fn run_delivery_receipts<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
) -> Result<Done> {
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    let dropped = shared.db.zone(|tx| forget_blocked_delivery_receipts(tx))?;
    if dropped > 0 {
        tracing::info!(
            receipts = dropped,
            "delivery receipts for blocked senders dropped"
        );
    }
    let owed = shared.db.read(owed_delivery_receipts)?;
    let mut failure = None;
    for (peer, timestamps) in &owed {
        let result = match parse_aci(peer) {
            Some(aci) => {
                send_receipt(shared, &ctx, aci, ReceiptType::Delivery, timestamps, None).await
            }
            None => Ok(()),
        };
        match result {
            Ok(()) => {}
            Err(e) if is_permanent(&e) => {
                tracing::info!(error = %e, "delivery receipts to a sender dropped");
            }
            Err(e @ Error::Db(_)) => return Err(e),
            Err(e) => {
                failure = Some(e);
                continue;
            }
        }
        shared
            .db
            .zone(|tx| forget_delivery_receipts(tx, peer, timestamps))?;
    }
    if let Some(e) = failure {
        return Err(e);
    }
    let left: i64 = shared.db.read(|c| -> Result<i64> {
        Ok(c.query_row("SELECT count(*) FROM delivery_receipts", [], |r| r.get(0))?)
    })?;
    if left > 0 {
        Ok(Done::RepeatAt(shared.now()))
    } else {
        Ok(Done::Finished)
    }
}

/// Runs a `sent_transcript` job: the transcript `Content` in its payload to our devices.
pub(crate) async fn run_sent_transcript<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    use prost::Message as _;
    let Some(transcript) = job
        .payload
        .as_deref()
        .and_then(|p| crate::proto::Content::decode(p).ok())
    else {
        tracing::warn!("sync transcript job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    let timestamp = content::transcript_timestamp(&transcript)?;
    let account = shared.account().ok_or(Error::NotLinked)?;
    let ctx = shared.send_context(&account, socket);
    send::send_to(
        &ctx,
        account.aci.into(),
        &Outgoing {
            content: &transcript,
            timestamp,
            delivery: Delivery::TRANSCRIPT,
            local: Some(&job.key),
            group: None,
        },
    )
    .await?;
    Ok(Done::Finished)
}

/// `Command::SetTyping`: a `TypingMessage` to the peer of an accepted 1:1 chat that is not
/// blocked, when the typing setting is on; fire and forget.
pub(crate) async fn send_typing<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    conv: ConvKey,
    on: bool,
) {
    let ConvKey::Direct(user) = &conv else {
        if matches!(conv, ConvKey::Group(_)) {
            crate::send::group::send_typing(shared, conv, on).await;
        }
        return;
    };
    let Ok(SignalUser::Aci(peer)) = parse_user_id(user) else {
        return;
    };
    let (Some(account), Some(socket)) = (shared.account(), shared.socket.borrow().clone()) else {
        return;
    };
    if peer == account.aci {
        return;
    }
    let destination = ServiceId::from(peer);
    let allowed = shared.db.read(|c| -> Result<(bool, bool)> {
        let row = peers::load(c, &destination.service_id_string())?;
        let ok = db::synced_settings(c)?.typing_indicators
            && !row.blocked
            && features::is_accepted(c, &account, destination)?;
        Ok((ok, row.share_my_number))
    });
    let (ok, share) = match allowed {
        Ok(allowed) => allowed,
        Err(e) => {
            tracing::warn!(error = %e, "typing: reading settings failed");
            return;
        }
    };
    if !ok {
        return;
    }
    let ctx = shared.send_context(&account, &socket);
    let timestamp = shared.unique_timestamp();
    let mut typing = content::typing(on, timestamp);
    if share && let Err(e) = content::add_pni_signature(&mut typing, &account) {
        tracing::warn!(error = %e, "typing: PNI signature failed");
        return;
    }
    let sent = send::send_to(
        &ctx,
        destination,
        &Outgoing {
            content: &typing,
            timestamp,
            delivery: Delivery::TYPING,
            local: None,
            group: None,
        },
    )
    .await;
    if let Err(e) = sent {
        tracing::debug!(error = %e, "typing indicator not sent");
    }
}

/// `Command::ConfirmIdentity`: the user accepted `user`'s new safety number; a contact that was
/// verified and became unverified by the change is trusted for sending again.
pub(crate) fn confirm_identity(conn: &Connection, aci: Aci) -> Result<()> {
    conn.prepare_cached(
        "UPDATE identities SET verified = 0 WHERE service_id = ?1 AND verified = 2",
    )?
    .execute([ServiceId::from(aci).service_id_string()])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use kourier_model::UserId;
    use uuid::Uuid;

    use super::*;
    use crate::receive::tests::fixture;
    use crate::service_id::aci_user_id;

    fn target(author: &UserId, local: u64) -> MsgTarget {
        MsgTarget {
            conv: None,
            author: TargetAuthor::User(author.clone()),
            local: local.to_string().into(),
            alternates: Vec::new(),
        }
    }

    fn outbox_rows(conn: &Connection) -> Vec<Vec<BackendEvent>> {
        let mut stmt = conn
            .prepare("SELECT envelope FROM core_outbox ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))
            .unwrap()
            .map(|row| kourier_backend_api::decode_outbox_row(&row.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn receipts_need_the_setting_and_an_accepted_chat_and_are_always_confirmed() {
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let alice_user = aci_user_id(alice).unwrap();
        let conv = ConvKey::Direct(alice_user.clone());
        let wake = Rc::new(Notify::new());
        let targets = [target(&alice_user, 10), target(&alice_user, 11)];
        let jobs = |conn: &Connection| jobs::count(conn, JobKind::Receipts).unwrap();
        fixture
            .db
            .zone(|tx| -> Result<()> {
                // Setting off (the default before the configuration sync).
                assert!(!persist_receipts(
                    tx,
                    account,
                    CmdId(1),
                    &conv,
                    &targets,
                    ReceiptKind::Read,
                    5,
                    &wake
                )?);
                db::store_configuration(tx, Some(true), None)?;
                // Not accepted.
                assert!(!persist_receipts(
                    tx,
                    account,
                    CmdId(2),
                    &conv,
                    &targets,
                    ReceiptKind::Read,
                    5,
                    &wake
                )?);
                peers::set_profile_sharing(
                    tx,
                    &ServiceId::from(alice).service_id_string(),
                    true,
                    5,
                )?;
                assert!(persist_receipts(
                    tx,
                    account,
                    CmdId(3),
                    &conv,
                    &targets,
                    ReceiptKind::Read,
                    5,
                    &wake
                )?);
                // The core resends the same command after a restart: one job.
                assert!(!persist_receipts(
                    tx,
                    account,
                    CmdId(3),
                    &conv,
                    &targets,
                    ReceiptKind::Read,
                    6,
                    &wake
                )?);
                assert_eq!(jobs(tx), 1);
                // Blocked on the phone: no read receipt goes to them any more.
                let sid = ServiceId::from(alice).service_id_string();
                peers::set_blocked(tx, &sid, true, 7)?;
                assert!(!persist_receipts(
                    tx,
                    account,
                    CmdId(4),
                    &conv,
                    &targets,
                    ReceiptKind::Read,
                    7,
                    &wake
                )?);
                assert_eq!(jobs(tx), 1, "nothing queued for a blocked contact");
                peers::set_blocked(tx, &sid, false, 8)?;
                let job = jobs::next_due(tx, 10, true, false)?.unwrap();
                let payload: ReceiptsPayload =
                    serde_json::from_slice(job.payload.as_deref().unwrap()).unwrap();
                assert_eq!(payload.kind, "read");
                assert_eq!(payload.recipients[0].timestamps, [10, 11]);
                assert!(job.key.starts_with("3:"), "{}", job.key);
                Ok(())
            })
            .unwrap();
        let rows = fixture
            .db
            .read(|c| -> Result<_> { Ok(outbox_rows(c)) })
            .unwrap();
        let confirmed: Vec<i64> = rows
            .iter()
            .flatten()
            .filter_map(|e| match e {
                BackendEvent::CommandPersisted { cmd } => Some(cmd.0),
                _ => None,
            })
            .collect();
        assert_eq!(
            confirmed,
            [1, 2, 3, 3, 4],
            "every command is confirmed, each time"
        );
    }

    /// Messages read in a group: one batch of receipts per author, as Desktop sends them.
    /// None for our own messages, for an author who is blocked, unregistered or whose 1:1
    /// chat is an unaccepted message request, and none at all for a group that is not
    /// accepted, blocked or left, or with the read-receipt setting off.
    #[test]
    fn group_receipts_go_to_each_author_of_an_accepted_group() {
        let fixture = fixture();
        let account = &fixture.account;
        let wake = Rc::new(Notify::new());
        let keys = groups::GroupKeys::from_master([4; 32]);
        let conv = keys.conv_key();
        let member = |n: u128| Aci::from(Uuid::from_u128(n));
        let (ada, bruno, chiara, dario, enzo) = (
            member(0xada),
            member(0xb0),
            member(0xc1),
            member(0xda),
            member(0xe0),
        );
        let user = |aci: Aci| aci_user_id(aci).unwrap();
        let sid = |aci: Aci| ServiceId::from(aci).service_id_string();
        let own = MsgTarget {
            author: TargetAuthor::Own,
            ..target(&user(ada), 15)
        };
        let targets = [
            target(&user(ada), 10),
            target(&user(bruno), 11),
            own,
            target(&user(ada), 12),
            target(&user(chiara), 13),
            target(&user(dario), 14),
            target(&user(enzo), 16),
            // A notice of the group has no Signal id.
            MsgTarget {
                local: "group:5".into(),
                ..target(&user(ada), 0)
            },
        ];
        fixture
            .db
            .zone(|tx| -> Result<()> {
                let payload = |tx: &Connection| {
                    receipts_payload(tx, account, &conv, &targets, ReceiptKind::Read)
                };
                let set_group = |tx: &Connection, column: &str, value: &str| {
                    tx.execute(
                        &format!("UPDATE groups SET {column} = ?1 WHERE group_id = ?2"),
                        (value, &keys.id[..]),
                    )
                    .map(drop)
                };
                db::store_configuration(tx, Some(true), None)?;
                assert_eq!(payload(tx)?, None, "a group Kourier does not know");
                tx.execute(
                    "INSERT INTO groups(master_key, group_id, updated_at, membership, whitelisted)
                     VALUES (?1, ?2, 0, 'member', 0)",
                    (&keys.master[..], &keys.id[..]),
                )?;
                assert_eq!(payload(tx)?, None, "not accepted on the phone yet");
                set_group(tx, "whitelisted", "1")?;
                // Chiara is blocked, Dario left Signal, Enzo's own chat is a message request
                // we have not answered (the core has the chat, nothing accepted it).
                peers::set_blocked(tx, &sid(chiara), true, 1)?;
                peers::mark_unregistered(tx, &sid(dario), 1)?;
                peers::set_features_hash(tx, &sid(enzo), 7, 1)?;
                // Bruno's chat exists and is accepted; Ada has no 1:1 chat with us at all.
                peers::set_features_hash(tx, &sid(bruno), 7, 1)?;
                peers::set_profile_sharing(tx, &sid(bruno), true, 1)?;
                let sent = payload(tx)?.expect("receipts are due");
                assert_eq!(sent.kind, "read");
                assert_eq!(
                    sent.group.as_deref(),
                    Some(groups::job_key(&keys.id).as_str())
                );
                assert_eq!(
                    sent.recipients,
                    [
                        ReceiptBatch {
                            peer: sid(ada),
                            timestamps: vec![10, 12]
                        },
                        ReceiptBatch {
                            peer: sid(bruno),
                            timestamps: vec![11]
                        },
                    ],
                    "per author, in the order read; nothing for our own message"
                );
                // The stored job carries the group; an old job without one is a 1:1 receipt.
                assert!(persist_receipts(
                    tx,
                    account,
                    CmdId(9),
                    &conv,
                    &targets,
                    ReceiptKind::Viewed,
                    5,
                    &wake
                )?);
                let job = jobs::next_due(tx, 10, true, false)?.unwrap();
                let stored: ReceiptsPayload =
                    serde_json::from_slice(job.payload.as_deref().unwrap()).unwrap();
                assert_eq!(stored.kind, "viewed");
                assert!(stored.group.is_some());
                let old: ReceiptsPayload = serde_json::from_str(
                    r#"{"kind":"read","recipients":[{"peer":"x","timestamps":[1]}]}"#,
                )
                .unwrap();
                assert_eq!(old.group, None);

                // The group's own state decides first.
                assert!(group_takes_receipts(tx, &keys.id)?);
                set_group(tx, "blocked", "1")?;
                assert_eq!(payload(tx)?, None, "a blocked group");
                set_group(tx, "blocked", "0")?;
                set_group(tx, "membership", "left")?;
                assert_eq!(payload(tx)?, None, "a group we left");
                assert!(!group_takes_receipts(tx, &keys.id)?);
                set_group(tx, "membership", "member")?;
                // The setting off: nothing, as in a 1:1 chat.
                db::store_configuration(tx, Some(false), None)?;
                assert_eq!(payload(tx)?, None, "read receipts are off");
                Ok(())
            })
            .unwrap();
    }

    /// A receipts job that has to wait keeps what it still owes: the authors already served
    /// and the timestamps already sent are gone from it, the rest is as it was.
    #[test]
    fn a_receipts_job_that_waits_keeps_only_what_is_left() {
        let batch = |peer: &str, timestamps: std::ops::Range<u64>| ReceiptBatch {
            peer: peer.to_owned(),
            timestamps: timestamps.collect(),
        };
        let payload = ReceiptsPayload {
            kind: "viewed".to_owned(),
            recipients: vec![
                batch("ada", 0..3),
                batch("bruno", 10..260),
                batch("chiara", 5..6),
            ],
            group: Some("00ff".to_owned()),
        };
        assert_eq!(receipts_left(&payload, 0, 0), None, "nothing went out");
        // Ada was served, Bruno's first receipt failed: Bruno and Chiara are left.
        let left = receipts_left(&payload, 1, 0).expect("ada is done");
        assert_eq!(left.recipients, payload.recipients[1..]);
        assert_eq!(
            (left.kind.as_str(), left.group.as_deref()),
            ("viewed", Some("00ff"))
        );
        // Two of Bruno's three receipts went out (100 timestamps each), the third failed.
        let left = receipts_left(&payload, 1, 2 * MAX_PER_MESSAGE).expect("partly done");
        assert_eq!(
            left.recipients,
            [batch("bruno", 210..260), batch("chiara", 5..6)]
        );
        // The first author's second receipt failed.
        let long = ReceiptsPayload {
            recipients: vec![batch("ada", 0..150)],
            ..payload.clone()
        };
        let left = receipts_left(&long, 0, MAX_PER_MESSAGE).expect("partly done");
        assert_eq!(left.recipients, [batch("ada", 100..150)]);
        // Out of range: nothing to keep apart.
        assert_eq!(receipts_left(&payload, 9, 0), None);
        // The stored form is what the job reads back.
        let stored = serde_json::to_vec(&left).unwrap();
        assert_eq!(
            serde_json::from_slice::<ReceiptsPayload>(&stored).unwrap(),
            left
        );
    }

    /// A read sync job that has to wait keeps the entries that were not sent: the messages
    /// already sent are gone from it, the rest is as it was (the first read of an imported
    /// chat is hundreds of messages; every retry must not start again from the first).
    #[test]
    fn a_read_sync_job_that_waits_keeps_only_what_is_left() {
        let entries = |range: std::ops::Range<u64>| -> Vec<(String, u64)> {
            range.map(|t| ("ada".to_owned(), t)).collect()
        };
        let payload = ReadSyncPayload {
            reads: entries(0..250),
        };
        assert_eq!(reads_left(&payload, 0), None, "nothing went out");
        // The third message (entries 200..250) failed after two went out.
        let left = reads_left(&payload, 2 * MAX_PER_MESSAGE).expect("partly done");
        assert_eq!(left.reads, entries(200..250));
        // Out of range: nothing to keep.
        assert_eq!(reads_left(&payload, 300), None);
        // The stored form is what the job reads back.
        let stored = serde_json::to_vec(&left).unwrap();
        assert_eq!(
            serde_json::from_slice::<ReadSyncPayload>(&stored).unwrap(),
            left
        );
    }

    #[test]
    fn read_syncs_list_messages_of_others_only() {
        let fixture = fixture();
        let wake = Rc::new(Notify::new());
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let alice_user = aci_user_id(alice).unwrap();
        let own = MsgTarget {
            author: TargetAuthor::Own,
            ..target(&alice_user, 1)
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert!(
                    !persist_read_sync(tx, &own, &[], CmdId(1), 5, &wake)?,
                    "own only"
                );
                assert!(persist_read_sync(
                    tx,
                    &target(&alice_user, 3),
                    &[target(&alice_user, 2), own.clone(), target(&alice_user, 3)],
                    CmdId(2),
                    5,
                    &wake
                )?);
                let job = jobs::next_due(tx, 10, true, false)?.unwrap();
                assert_eq!(job.kind, JobKind::ReadSync);
                let payload: ReadSyncPayload =
                    serde_json::from_slice(job.payload.as_deref().unwrap()).unwrap();
                let sid = ServiceId::from(alice).service_id_string();
                assert_eq!(payload.reads, [(sid.clone(), 2), (sid, 3)]);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn delivery_receipts_batch_per_sender() {
        let fixture = fixture();
        fixture
            .db
            .zone(|tx| -> Result<()> {
                for t in 0..150u64 {
                    queue_delivery_receipt(tx, "alice", 1000 + t, 100 + i64::try_from(t).unwrap())?;
                }
                queue_delivery_receipt(tx, "bob", 7, 1000)?;
                queue_delivery_receipt(tx, "bob", 7, 1001)?;
                let job = jobs::next_due(tx, 600, true, false)?.unwrap();
                assert_eq!(job.kind, JobKind::DeliveryReceipts);
                assert!(
                    jobs::next_due(tx, 599, true, false)?.is_none(),
                    "500 ms after the first"
                );
                let owed = owed_delivery_receipts(tx)?;
                assert_eq!(owed.len(), 2);
                assert_eq!(owed[0].0, "alice");
                assert_eq!(owed[0].1.len(), MAX_PER_MESSAGE);
                assert_eq!(owed[0].1[0], 1000);
                assert_eq!(owed[1], ("bob".to_owned(), vec![7]));
                forget_delivery_receipts(tx, "alice", &owed[0].1)?;
                let owed = owed_delivery_receipts(tx)?;
                assert_eq!(owed[0].1.len(), 50);
                // Alice was blocked after her messages arrived: she is owed nothing any more;
                // Bob still is.
                assert_eq!(forget_blocked_delivery_receipts(tx)?, 0);
                peers::set_blocked(tx, "alice", true, 2000)?;
                assert_eq!(forget_blocked_delivery_receipts(tx)?, 50);
                assert_eq!(
                    owed_delivery_receipts(tx)?,
                    [("bob".to_owned(), vec![7])],
                    "nothing goes to a blocked sender"
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn confirming_an_identity_trusts_the_new_key_again() {
        let fixture = fixture();
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        fixture
            .db
            .zone(|tx| -> Result<()> {
                tx.execute(
                    "INSERT INTO identities(service_id, identity_key, verified, updated_at)
                     VALUES (?1, x'05', 2, 0)",
                    [ServiceId::from(alice).service_id_string()],
                )?;
                confirm_identity(tx, alice)?;
                let verified: i64 =
                    tx.query_row("SELECT verified FROM identities", [], |r| r.get(0))?;
                assert_eq!(verified, 0);
                Ok(())
            })
            .unwrap();
    }
}
