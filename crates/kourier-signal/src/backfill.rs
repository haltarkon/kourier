//! Attachment backfill, the request (S6): the core asks our PRIMARY device (the phone) to
//! upload again the files of one message the server no longer has
//! (`Command::RequestAttachments`). Signal: `SyncMessage.attachmentBackfillRequest {
//! targetMessage, targetConversation }`, sent like every sync message to our own account (so
//! every other device of ours sees it too; only the primary answers).
//!
//! - One zone stores the job (`JobKind::BackfillRequest`, keyed by the command and a digest
//!   of its payload, so a command the core sends again finds its job) and confirms with
//!   `CommandPersisted`. A message that cannot be named (no Signal id, an author or a chat
//!   known only by a number: the answer could not be placed) gets no job: the same zone tells
//!   the core `AttachmentRestore { NotSent }`, which ends the wait as "did not answer".
//! - The job sends the request URGENT, as Signal Android's requester does
//!   (`SignalServiceMessageSender.java` `sendSyncMessage` with `urgent = true` for
//!   `MultiDeviceAttachmentBackfillRequestJob`). Signal Desktop sends it `urgent: false` and
//!   its timeout text tells the user to open Signal on the phone; an urgent request may let
//!   the server wake the phone (the server's push rules are not on disk: unverified). After
//!   the send it reports `AttachmentRestore { Sent }`: the core's wait (30 s, 2 min more per
//!   PENDING answer) starts then, not at the click.
//! - The job retries with the usual backoff and survives restarts. It expires
//!   [`REQUEST_MAX_AGE`] after the click (a request that could not go out for that long is
//!   not one the user still waits for); a dropped job reports `AttachmentRestore { NotSent }`
//!   in the zone that drops it ([`dropped`]).
//! - The message is named by its ORIGINAL sent time (Kourier's message id; for an imported
//!   edited message the first version's time), as Desktop does: the phone looks up exactly
//!   that `DATE_SENT` (Signal Android `MessageTable.kt` `getMessageFor`), whose row holds the
//!   message's files. Its long text is the FIRST version's: the core takes no long text for
//!   an edited message from an answer (`kourier-core` `ingest/restore.rs`).
//! - Kourier never answers a request (only the primary does): `receive/dispatch.rs` drops
//!   `attachmentBackfillRequest` with its own reason.

use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use kourier_backend_api::{BackendEvent, RestoreSlot, RestoreUpdate};
use kourier_db::Tx;
use kourier_model::{BackendBlob, CmdId, ConvKey, MsgTarget, Redacted};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use crate::actor::Shared;
use crate::attachment::{POINTER_BLOB_V, Pointer};
use crate::db::Account;
use crate::delete_sync::{addressable, conversation};
use crate::error::{Error, Result};
use crate::jobs::{self, Done, Job, JobKind};
use crate::net::SignalTransport;
use crate::outbox::append_events;
use crate::proto;
use crate::proto::sync_message::AttachmentBackfillRequest;
use crate::receive::media;
use crate::send::content;
use crate::send::{self, Delivery, Outgoing};

/// How long a request may wait to go out (no connection, a failing server) before it is
/// dropped and the core says "your phone did not answer". Kourier's own number: Android's
/// request job lives 30 s and Desktop's send queue a day; the user is looking at "Asking
/// your phone…" meanwhile.
pub(crate) const REQUEST_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// The device id of the account's primary, the only device that answers.
const PRIMARY: libsignal_core::DeviceId = match libsignal_core::DeviceId::new(1) {
    Ok(id) => id,
    Err(_) => unreachable!(),
};

/// What a backfill job stores: the command's message, so that a drop can name it to the core
/// and the request is built from the account at send time.
#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    conv: ConvKey,
    target: MsgTarget,
    /// The command of the core's request: the reports name it, so the core applies them only
    /// to the request that made it. Absent in a job stored before the field existed (the
    /// core then ignores its reports; the wait ends by its deadline).
    #[serde(default)]
    cmd: Option<i64>,
}

/// The request for `target`; `None` when the message or its chat cannot be named, and for a
/// chat or an author known by phone number alone: the phone would answer naming the number
/// back, and `receive/dispatch.rs` `backfill_response` cannot place such an answer (a
/// number-only chat is read-only in Kourier), so the request would only make the phone
/// upload for nothing.
pub(crate) fn request(
    account: &Account,
    conv: &ConvKey,
    target: &MsgTarget,
) -> Option<AttachmentBackfillRequest> {
    use proto::addressable_message::Author;
    use proto::conversation_identifier::Identifier;
    let message = addressable(account, target)?;
    let conversation = conversation(conv)?;
    if matches!(message.author, Some(Author::AuthorE164(_)))
        || matches!(conversation.identifier, Some(Identifier::ThreadE164(_)))
    {
        return None;
    }
    Some(AttachmentBackfillRequest {
        target_message: Some(message),
        target_conversation: Some(conversation),
    })
}

fn not_sent(target: &MsgTarget, cmd: Option<CmdId>, now: i64) -> BackendEvent {
    BackendEvent::AttachmentRestore {
        target: target.clone(),
        update: RestoreUpdate::NotSent,
        at: now,
        cmd,
    }
}

/// `Command::RequestAttachments`: the job and `CommandPersisted` in one zone (or, for a
/// message that cannot be named, `NotSent` with the confirmation). Returns whether a job was
/// added.
pub(crate) fn persist(
    tx: &mut Tx<'_>,
    account: &Account,
    cmd: CmdId,
    conv: &ConvKey,
    target: &MsgTarget,
    now: i64,
    wake: &Rc<Notify>,
) -> Result<bool> {
    let mut events = vec![BackendEvent::CommandPersisted { cmd }];
    let added = if request(account, conv, target).is_some() {
        let payload = serde_json::to_vec(&Payload {
            conv: conv.clone(),
            target: target.clone(),
            cmd: Some(cmd.0),
        })
        .map_err(|_| Error::Internal("backfill payload"))?;
        let added = jobs::enqueue_command(tx, JobKind::BackfillRequest, cmd.0, &payload, now)?;
        if added {
            let expires =
                now.saturating_add(i64::try_from(REQUEST_MAX_AGE.as_millis()).unwrap_or(i64::MAX));
            tx.prepare_cached("UPDATE jobs SET expires_at = ?3 WHERE kind = ?1 AND key = ?2")?
                .execute((
                    JobKind::BackfillRequest.as_str(),
                    jobs::command_key(cmd.0, &payload),
                    expires,
                ))?;
        }
        added
    } else {
        tracing::debug!(
            cmd = cmd.0,
            "backfill request for a message that cannot be named"
        );
        events.insert(0, not_sent(target, Some(cmd), now));
        false
    };
    append_events(tx, &events, now, wake)?;
    Ok(added)
}

/// A backfill job was dropped (expired, or refused for good): the core ends the wait, in the
/// zone that drops it.
pub(crate) fn dropped(tx: &mut Tx<'_>, job: &Job, now: i64, wake: &Rc<Notify>) -> Result<()> {
    let Some(payload) = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_slice::<Payload>(p).ok())
    else {
        return Ok(());
    };
    append_events(
        tx,
        &[not_sent(&payload.target, payload.cmd.map(CmdId), now)],
        now,
        wake,
    )?;
    Ok(())
}

/// Runs a `backfill_request` job: the request to our own devices, then `Sent` to the core.
pub(crate) async fn run<T: SignalTransport>(
    shared: &Rc<Shared<T>>,
    socket: &Arc<T::Socket>,
    job: &Job,
) -> Result<Done> {
    let Some(payload) = job
        .payload
        .as_deref()
        .and_then(|p| serde_json::from_slice::<Payload>(p).ok())
    else {
        tracing::warn!("backfill request job without a valid payload dropped");
        return Ok(Done::Finished);
    };
    let account = shared.account().ok_or(Error::NotLinked)?;
    let Some(request) = request(&account, &payload.conv, &payload.target) else {
        // (`persist` checked it; an account whose ACI changed cannot name it any more.)
        let now = shared.now();
        let wake = shared.wake.clone();
        shared
            .db
            .zone(|tx| {
                append_events(
                    tx,
                    &[not_sent(&payload.target, payload.cmd.map(CmdId), now)],
                    now,
                    &wake,
                )
            })
            .map(drop)?;
        return Ok(Done::Finished);
    };
    let sync = content::sync_content(proto::SyncMessage {
        content: Some(proto::sync_message::Content::AttachmentBackfillRequest(
            request,
        )),
        ..Default::default()
    });
    let ctx = shared.send_context(&account, socket);
    send::send_to(
        &ctx,
        account.aci.into(),
        &Outgoing {
            content: &sync,
            timestamp: shared.unique_timestamp(),
            // Urgent, as Signal Android's requester (module docs).
            delivery: Delivery::sync_request(true),
            local: None,
            group: None,
        },
    )
    .await?;
    // Only the primary (device 1) answers. `send_to` succeeds with no other device of ours
    // ("nothing to sync to"), and a primary without a session after it means no one got the
    // request: the wait ends at once ("did not answer") instead of after its 30 s.
    let reached = shared.db.read(|c| -> Result<bool> {
        Ok(send::usable_devices(
            c,
            &account.aci.service_id_string(),
            Some(account.device_id),
            false,
        )?
        .contains(&PRIMARY))
    })?;
    let now = shared.now();
    let wake = shared.wake.clone();
    // A crash before this zone sends the request again at the next start and reports `Sent`
    // then; a second `Sent` changes nothing in the core.
    shared.db.zone(|tx| {
        append_events(
            tx,
            &[BackendEvent::AttachmentRestore {
                target: payload.target.clone(),
                update: if reached {
                    RestoreUpdate::Sent
                } else {
                    RestoreUpdate::NotSent
                },
                at: now,
                cmd: payload.cmd.map(CmdId),
            }],
            now,
            &wake,
        )
        .map(drop)
    })?;
    tracing::debug!(reached, "attachment backfill requested");
    Ok(Done::Finished)
}

/// What an answer of the primary (`SyncMessage.attachmentBackfillResponse`) says, its target
/// left to the caller (`receive/dispatch.rs`); `None` for an answer with neither a list nor
/// an error. Both enums' first values are 0 inside a `oneof`, so the ARM present decides,
/// never the value; an unknown value is taken as Desktop takes it (`MessageReceiver.preload.ts`:
/// an unknown status is PENDING, an unknown error is MESSAGE_NOT_FOUND).
pub(crate) fn answer(
    response: &proto::sync_message::AttachmentBackfillResponse,
) -> Option<RestoreUpdate> {
    use proto::sync_message::attachment_backfill_response::{Data, Error as AnswerError};
    Some(match response.data.as_ref()? {
        Data::Error(value) => {
            if AnswerError::try_from(*value).is_err() {
                tracing::debug!("backfill answer with an unknown error: taken as not found");
            }
            RestoreUpdate::MessageNotFound
        }
        Data::Attachments(list) => RestoreUpdate::Answer {
            attachments: list.attachments.iter().map(slot).collect(),
            long_text: list.long_text.as_ref().map(slot),
        },
    })
}

/// One slot of an answer. A pointer is a file when a download can be made of it
/// ([`Pointer::from_proto`]: a known CDN, a URL-safe key, a 64-byte key, a size); one that
/// states no digest at all is taken too and checked at the download against the plaintext
/// hash the attachment's names state ([`Pointer::from_blob_named`]; refused there without
/// one). Anything else is [`RestoreSlot::Skip`], as is an empty slot (Signal Android sends
/// one for a file it cannot describe): it keeps its place and changes nothing.
fn slot(data: &proto::sync_message::attachment_backfill_response::AttachmentData) -> RestoreSlot {
    use proto::sync_message::attachment_backfill_response::attachment_data::{Data, Status};
    match data.data.as_ref() {
        None => RestoreSlot::Skip,
        Some(Data::Status(value)) => match Status::try_from(*value) {
            Ok(Status::Pending) => RestoreSlot::Pending,
            Ok(Status::TerminalError) => RestoreSlot::Gone,
            Err(_) => {
                tracing::debug!("backfill answer with an unknown status: taken as pending");
                RestoreSlot::Pending
            }
        },
        Some(Data::Attachment(pointer)) => {
            let usable = Pointer::from_proto(pointer).is_ok()
                || (pointer.digest.as_deref().is_none_or(<[u8]>::is_empty)
                    && Pointer::from_proto_or_hash(pointer, Some([0; 32])).is_ok());
            if !usable {
                return RestoreSlot::Skip;
            }
            RestoreSlot::File {
                remote: BackendBlob::new(POINTER_BLOB_V, pointer.encode_to_vec()),
                net_ids: Redacted::new(media::net_ids(pointer)),
                size: u64::from(pointer.size.unwrap_or(0)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::TargetAuthor;
    use libsignal_core::{Aci, ServiceId};
    use uuid::Uuid;

    use super::*;
    use crate::receive::tests::fixture;
    use crate::service_id::aci_user_id;

    /// The request names the message by its author's binary service id and its original sent
    /// time, and the 1:1 chat by the peer's binary service id; a target without an author
    /// Signal knows cannot be asked for.
    #[test]
    fn the_request_names_the_message_as_the_phone_finds_it() {
        use proto::addressable_message::Author;
        use proto::conversation_identifier::Identifier;
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let conv = ConvKey::Direct(user.clone());
        let theirs = ServiceId::from(alice).service_id_binary();
        let target = MsgTarget {
            conv: Some(conv.clone()),
            author: TargetAuthor::User(user),
            local: "1700000000123".into(),
            alternates: Vec::new(),
        };
        let r = request(account, &conv, &target).expect("a request");
        let message = r.target_message.expect("message");
        assert_eq!(message.sent_timestamp, Some(1_700_000_000_123));
        assert_eq!(
            message.author,
            Some(Author::AuthorServiceIdBinary(theirs.clone()))
        );
        assert_eq!(
            r.target_conversation.and_then(|c| c.identifier),
            Some(Identifier::ThreadServiceIdBinary(theirs))
        );
        let unknown = MsgTarget {
            author: TargetAuthor::Unknown,
            ..target
        };
        assert!(request(account, &conv, &unknown).is_none());
    }

    fn events(fixture: &crate::receive::tests::Fixture) -> Vec<BackendEvent> {
        fixture
            .db
            .read(|c| -> Result<Vec<Vec<u8>>> {
                let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, Vec<u8>>(0))?
                    .collect::<kourier_db::rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
            .iter()
            .flat_map(|row| kourier_backend_api::decode_outbox_row(row).unwrap())
            .collect()
    }

    /// A request is stored once per command with its expiry, and confirmed every time; a
    /// message that cannot be named is confirmed with `NotSent` and no job; a dropped job
    /// tells the core `NotSent` for its message.
    #[test]
    fn requests_are_stored_once_and_a_drop_ends_the_wait() {
        let fixture = fixture();
        let account = &fixture.account;
        let alice = Aci::from(Uuid::from_u128(0xa11ce));
        let user = aci_user_id(alice).unwrap();
        let conv = ConvKey::Direct(user.clone());
        let wake = Rc::new(Notify::new());
        let target = MsgTarget {
            conv: Some(conv.clone()),
            author: TargetAuthor::User(user),
            local: "50".into(),
            alternates: Vec::new(),
        };
        let unknown = MsgTarget {
            author: TargetAuthor::Unknown,
            ..target.clone()
        };
        fixture
            .db
            .zone(|tx| -> Result<()> {
                assert!(persist(tx, account, CmdId(1), &conv, &target, 5, &wake)?);
                assert!(!persist(tx, account, CmdId(1), &conv, &target, 6, &wake)?);
                assert!(!persist(tx, account, CmdId(2), &conv, &unknown, 7, &wake)?);
                assert_eq!(jobs::count(tx, JobKind::BackfillRequest)?, 1);
                let job = jobs::next_due(tx, 10, true, false)?.expect("due");
                assert_eq!(job.kind, JobKind::BackfillRequest);
                let max = i64::try_from(REQUEST_MAX_AGE.as_millis()).unwrap();
                assert_eq!(job.expires_at, Some(5 + max));
                // Past its expiry the next failure drops it, and the core hears of it.
                let retried = jobs::retry(tx, &job, 5 + max, "offline", false, None)?;
                assert_eq!(retried, jobs::Retry::Dropped);
                dropped(tx, &job, 5 + max, &wake)?;
                Ok(())
            })
            .unwrap();
        let seen: Vec<String> = events(&fixture)
            .into_iter()
            .map(|e| match e {
                BackendEvent::CommandPersisted { cmd } => format!("ok {}", cmd.0),
                BackendEvent::AttachmentRestore { target, update, .. } => {
                    format!("{:?} {}", update, target.local)
                }
                other => other.name().to_owned(),
            })
            .collect();
        assert_eq!(seen, ["ok 1", "ok 1", "NotSent 50", "ok 2", "NotSent 50"]);
    }

    fn pointer(digest: Option<Vec<u8>>, cdn: u32) -> proto::AttachmentPointer {
        proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "re-uploaded".to_owned(),
            )),
            client_uuid: Some(vec![0x11; 16]),
            key: Some(vec![1; 64]),
            digest,
            size: Some(1234),
            cdn_number: Some(cdn),
            ..Default::default()
        }
    }

    /// The answer's slots: a pointer a download can be made of is a file (with its `uuid:`
    /// and `digest:` names), one without any digest too (checked at the download), anything
    /// else and an empty slot keep their place as `Skip`; the status ARM decides (PENDING is
    /// 0), an unknown status is pending, every error is "not found", and an answer with
    /// neither is none.
    #[test]
    fn answers_map_slot_by_slot() {
        use proto::sync_message::AttachmentBackfillResponse as Response;
        use proto::sync_message::attachment_backfill_response::attachment_data::Data as Slot;
        use proto::sync_message::attachment_backfill_response::{
            AttachmentData, AttachmentDataList, Data,
        };
        let data = |slot: Option<Slot>| AttachmentData { data: slot };
        let list = Response {
            data: Some(Data::Attachments(AttachmentDataList {
                attachments: vec![
                    data(Some(Slot::Attachment(pointer(Some(vec![2; 32]), 3)))),
                    data(Some(Slot::Attachment(pointer(None, 2)))),
                    data(Some(Slot::Attachment(pointer(Some(vec![2; 31]), 2)))),
                    data(Some(Slot::Attachment(pointer(Some(vec![2; 32]), 7)))),
                    data(None),
                    data(Some(Slot::Status(0))),
                    data(Some(Slot::Status(1))),
                    data(Some(Slot::Status(9))),
                ],
                long_text: Some(data(Some(Slot::Status(1)))),
            })),
            ..Default::default()
        };
        let Some(RestoreUpdate::Answer {
            attachments,
            long_text,
        }) = answer(&list)
        else {
            panic!("an answer");
        };
        assert_eq!(long_text, Some(RestoreSlot::Gone));
        let names = |slot: &RestoreSlot| match slot {
            RestoreSlot::File {
                remote,
                net_ids,
                size,
            } => {
                assert_eq!((remote.v, *size), (POINTER_BLOB_V, 1234));
                net_ids.expose().clone()
            }
            other => panic!("{other:?}"),
        };
        let uuid = format!("uuid:{}", "11".repeat(16));
        assert_eq!(
            names(&attachments[0]),
            [uuid.clone(), format!("digest:{}", "02".repeat(32))]
        );
        assert_eq!(names(&attachments[1]), [uuid]);
        assert_eq!(
            attachments[2..],
            [
                RestoreSlot::Skip,
                RestoreSlot::Skip,
                RestoreSlot::Skip,
                RestoreSlot::Pending,
                RestoreSlot::Gone,
                RestoreSlot::Pending,
            ]
        );
        for error in [0, 5] {
            let not_found = Response {
                data: Some(Data::Error(error)),
                ..Default::default()
            };
            assert_eq!(answer(&not_found), Some(RestoreUpdate::MessageNotFound));
        }
        assert_eq!(answer(&Response::default()), None);
    }

    /// A backfilled pointer without a digest downloads only against the plaintext hash among
    /// the attachment's names; one with a digest is checked against that too when the names
    /// have it.
    #[test]
    fn a_pointer_without_a_digest_is_checked_by_the_plaintext_name() {
        use crate::attachment::Integrity;
        let blob =
            |p: &proto::AttachmentPointer| BackendBlob::new(POINTER_BLOB_V, p.encode_to_vec());
        let named = vec![
            format!("uuid:{}", "11".repeat(16)),
            format!("plaintext:{}", "05".repeat(32)),
        ];
        let bare = blob(&pointer(None, 2));
        assert!(Pointer::from_blob_named(&bare, &[]).is_err());
        assert!(Pointer::from_blob_named(&bare, &named[..1]).is_err());
        assert_eq!(
            Pointer::from_blob_named(&bare, &named).unwrap().integrity,
            Integrity::PlaintextHash([5; 32])
        );
        // A pointer with a digest is checked against it AND the plaintext hash the attachment
        // has: an answer matched to the wrong attachment is refused, not shown under it.
        let digested = blob(&pointer(Some(vec![2; 32]), 2));
        assert_eq!(
            Pointer::from_blob_named(&digested, &named)
                .unwrap()
                .integrity,
            Integrity::Both {
                digest: [2; 32],
                plaintext: [5; 32]
            }
        );
        assert_eq!(
            Pointer::from_blob_named(&digested, &named[..1])
                .unwrap()
                .integrity,
            Integrity::Digest([2; 32]),
            "no plaintext hash to check against: the digest alone"
        );
    }
}
