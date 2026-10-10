//! The 1:1 send path (§6.7; ports `SD/ts/textsecure/SendMessage.preload.ts`,
//! `OutgoingMessage.preload.ts`, `SD/ts/messages/send.preload.ts` and
//! `SD/ts/util/handleMessageSend.preload.ts`).
//!
//! [`send_to`] sends one `Content` to every device of one recipient: skip it when
//! `send_progress` says the server already took this message for them; pick sealed or unsealed
//! ([`sealed`]); find the devices (sessions that pass the usability flags, else a prekey fetch
//! and `process_prekey_bundle`); `message_encrypt` (and seal) per device in one zone; send
//! `PUT /v1/messages/{destination}` (sealed on the unauthenticated socket with the access key,
//! unsealed on the authenticated one); repair the device list on 409/410 and retry in the same
//! call; fall back to unsealed on 401/403; and once the server accepted, record send progress,
//! the Message Send Log row and the sealed-sender state in one zone.
//!
//! [`transmit`] is `Command::Transmit` in a 1:1 chat: a message (its body prepared by [`media`]:
//! attachments uploaded, a long text attached, its quote), an edit, a delete for everyone or a
//! reaction ([`outgoing`]) to the peer (with our PNI signature while they know us by number
//! only), then the sync transcript to our other devices (unsealed, not urgent); Note to Self is
//! the transcript only. A failed transcript becomes a job, it never fails the message. A delete
//! for me is a sync message to our own devices and nothing else.

pub(crate) mod content;
pub(crate) mod group;
pub(crate) mod media;
pub(crate) mod outgoing;
pub(crate) mod sealed;
pub(crate) mod sender_key;
pub(crate) mod unauth;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http::{HeaderName, HeaderValue, Method, StatusCode};
use kourier_backend_api::{Clock, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_db::ZoneDb;
use kourier_model::{BackendError, ConvKey, ErrorKind, RecipientState, UserId};
use libsignal_core::{DeviceId, ServiceId};
use libsignal_protocol::{
    CiphertextMessage, ContentHint, IdentityKeyStore as _, PlaintextContent, ProtocolAddress,
    PublicKey, SessionStore as _, SessionUsabilityRequirements, SignalProtocolError,
    UnidentifiedSenderMessageContent, message_encrypt, process_prekey_bundle,
    sealed_sender_encrypt_from_usmc,
};
use prost::Message as _;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

pub(crate) use unauth::UnauthSocket;

use self::content::{RecipientStatus, Thread, Transcript};
use self::sealed::Seal;
use crate::db::Account;
use crate::error::{Error, Result};
use crate::features;
use crate::jobs::{self, JobKind, SyncRequest};
use crate::keys::parse_bundles;
use crate::msl;
use crate::net::{ChatRequest, ChatResponse, Net, SignalTransport, json_request, parse_json};
use crate::outbox::append_events;
use crate::padding::pad;
use crate::peers::{self, SealedSenderState};
use crate::proto::{self, envelope::Type as EnvelopeType};
use crate::service_id::{SignalUser, parse_user_id};
use crate::store::{
    IdentityKind, Stores, archive_all_sessions, archive_session, ready, session_devices,
};

/// Attempts within one send: device-list repairs (409/410) and the unsealed fallback.
const MAX_ATTEMPTS: usize = 6;

/// How long a failed sync transcript is retried as a job (Desktop's one-day job budget).
const TRANSCRIPT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The failpoint after the encrypt zone committed, before the ciphertext leaves (§10.4).
pub(crate) const FAILPOINT_AFTER_ENCRYPT_COMMIT: &str = "after_encrypt_commit";
/// The same point, hit only by edits, deletes for everyone and reactions
/// ([`Delivery::MODIFIER`]): a kill test can stop exactly such a send, whatever else the
/// account sends meanwhile.
pub(crate) const FAILPOINT_AFTER_MODIFIER_ENCRYPT_COMMIT: &str = "after_modifier_encrypt_commit";

/// Serializes the sends to one recipient (§13 phase D): a `Transmit` and a sync-request job can
/// both send to our own ACI, and two sends that repair the same device list (409/410) or build
/// sessions from the same prekey fetch must not interleave between their zones and requests.
/// Sends to different recipients still run concurrently.
#[derive(Default)]
pub(crate) struct SendLocks {
    locks: RefCell<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl SendLocks {
    /// Waits until no other send to `recipient` (a service id string) runs; the send holds the
    /// guard until it is done.
    pub(crate) async fn lock(&self, recipient: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.locks.borrow_mut();
            // Forget recipients nobody holds or waits for (only the map refers to them).
            locks.retain(|_, mutex| Arc::strong_count(mutex) > 1);
            locks.entry(recipient.to_owned()).or_default().clone()
        };
        mutex.lock_owned().await
    }
}

/// Everything a send needs. `socket` is the authenticated chat socket.
pub(crate) struct SendContext<'a, T: SignalTransport> {
    /// The transport.
    pub(crate) net: &'a Net<T>,
    /// The authenticated socket.
    pub(crate) socket: &'a Arc<T::Socket>,
    /// The unauthenticated socket (sealed sends).
    pub(crate) unauth: &'a UnauthSocket<T>,
    /// `backend.db`.
    pub(crate) db: &'a ZoneDb,
    /// Our account.
    pub(crate) account: &'a Account,
    /// The per-recipient send locks of the account.
    pub(crate) locks: &'a SendLocks,
    /// Sealed-sender trust roots (sender certificates are checked against them).
    pub(crate) trust_roots: &'a [PublicKey],
    /// Time source.
    pub(crate) clock: &'a dyn Clock,
    /// Wakes the job runner (a failed transcript became a job).
    pub(crate) jobs_wake: &'a Rc<Notify>,
    /// Wakes the forwarder (a prekey fetch revealed a new identity key: `core_outbox` events).
    pub(crate) outbox_wake: &'a Rc<Notify>,
}

impl<T: SignalTransport> SendContext<'_, T> {
    /// Milliseconds since the epoch.
    pub(crate) fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// [`Self::now_ms`] as a column value.
    pub(crate) fn now(&self) -> i64 {
        i64::try_from(self.now_ms()).unwrap_or(i64::MAX)
    }
}

/// How a `Content` goes out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    /// The server may wake the recipient's phone for it.
    pub(crate) urgent: bool,
    /// Only to connected devices, never queued (typing).
    pub(crate) online: bool,
    /// libsignal's content hint (in the sealed envelope and the MSL).
    pub(crate) hint: ContentHint,
    /// Saved in the Message Send Log (`shouldSaveProto`: all but typing).
    pub(crate) log: bool,
    /// May be sealed (never sync messages to our own devices).
    pub(crate) may_seal: bool,
    /// A failpoint of its own between the encrypt zone and the request (§10.4).
    pub(crate) failpoint: Option<&'static str>,
}

impl Delivery {
    /// A message to a contact.
    pub(crate) const MESSAGE: Self = Self {
        urgent: true,
        online: false,
        hint: ContentHint::Resendable,
        log: true,
        may_seal: true,
        failpoint: None,
    };
    /// An edit, a delete for everyone or a reaction: sent like a message (Desktop's jobs use
    /// `ContentHint.Resendable` and `urgent: true` for all three).
    pub(crate) const MODIFIER: Self = Self {
        failpoint: Some(FAILPOINT_AFTER_MODIFIER_ENCRYPT_COMMIT),
        ..Self::MESSAGE
    };
    /// A change of the 1:1 timer (`sendDirectExpirationTimerUpdate`: `urgent: false`).
    pub(crate) const TIMER_UPDATE: Self = Self {
        urgent: false,
        ..Self::MESSAGE
    };
    /// A sync transcript of our own message (`urgent: false`).
    pub(crate) const TRANSCRIPT: Self = Self {
        urgent: false,
        online: false,
        hint: ContentHint::Resendable,
        log: true,
        may_seal: false,
        failpoint: None,
    };
    /// A read sync (`urgent: true`, `syncReadMessages`).
    pub(crate) const READ_SYNC: Self = Self {
        urgent: true,
        ..Self::TRANSCRIPT
    };
    /// A receipt (`urgent: false`, sealed when possible).
    pub(crate) const RECEIPT: Self = Self {
        urgent: false,
        ..Self::MESSAGE
    };
    /// A typing indicator: online only, implicit, not logged.
    pub(crate) const TYPING: Self = Self {
        urgent: false,
        online: true,
        hint: ContentHint::Implicit,
        log: false,
        may_seal: true,
        failpoint: None,
    };
    /// A retry request or a null message (`sendResendRequest`, `sendNullMessage`): not urgent,
    /// default hint, never logged (`handleMessageSend.preload.ts:93-111`).
    pub(crate) const RETRY: Self = Self {
        urgent: false,
        online: false,
        hint: ContentHint::Default,
        log: false,
        may_seal: true,
        failpoint: None,
    };

    /// A sync request to our own devices.
    pub(crate) const fn sync_request(urgent: bool) -> Self {
        Self {
            urgent,
            ..Self::TRANSCRIPT
        }
    }
}

/// The group a 1:1 send belongs to (a group message to one member, a distribution message).
#[derive(Clone, Copy)]
pub(crate) struct InGroup<'a> {
    /// The group id: named in the sealed content, and logged with the content (a resend adds
    /// our distribution message).
    pub(crate) id: &'a [u8],
    /// This recipient's group send token (a serialized `GroupSendFullToken`): seals the send
    /// when sealed sender is disabled for them (`SD/ts/util/getSendOptions.preload.ts`).
    pub(crate) token: Option<&'a [u8]>,
    /// The content itself belongs to the group (a group message, a distribution message): the
    /// sealed content names the group and the Message Send Log remembers it. `false` for a
    /// send that only borrows the group's token: a read receipt for a group message goes to
    /// its author as a plain 1:1 receipt (`SendMessage.preload.ts` `#sendReceiptMessage`
    /// passes no `groupId` to `sendIndividualProto`).
    pub(crate) named: bool,
}

impl<'a> InGroup<'a> {
    /// The group id the content is sent under, if any.
    fn content_group(&self) -> Option<&'a [u8]> {
        self.named.then_some(self.id)
    }
}

impl std::fmt::Debug for InGroup<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InGroup")
            .field("id", &crate::groups::log_id(self.id))
            .field("token", &self.token.is_some())
            .field("named", &self.named)
            .finish()
    }
}

/// One `Content` for one recipient.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Outgoing<'a> {
    /// What to send (unpadded).
    pub(crate) content: &'a proto::Content,
    /// The envelope timestamp (a message's sent timestamp).
    pub(crate) timestamp: u64,
    /// How.
    pub(crate) delivery: Delivery,
    /// The core's message id: progress is recorded under it (a repeated send skips recipients
    /// that have it), and the MSL row refers to it.
    pub(crate) local: Option<&'a str>,
    /// The group it belongs to, if any.
    pub(crate) group: Option<InGroup<'a>>,
}

/// The server took a `Content` for every device of the recipient.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Accepted {
    /// It went sealed.
    pub(crate) sealed: bool,
}

/// One message of `PUT /v1/messages/{destination}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct OutgoingMessage {
    #[serde(rename = "type")]
    kind: i32,
    destination_device_id: u32,
    destination_registration_id: u32,
    content: String,
}

/// Body of `PUT /v1/messages/{destination}`.
#[derive(Debug, Serialize)]
struct OutgoingMessages {
    messages: Vec<OutgoingMessage>,
    timestamp: u64,
    online: bool,
    urgent: bool,
}

/// 409 body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct MismatchedDevices {
    missing_devices: Vec<u32>,
    extra_devices: Vec<u32>,
}

/// 410 body.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct StaleDevices {
    stale_devices: Vec<u32>,
}

/// 428 body (`SendMessageChallengeData`): the challenge's token and the ways the server
/// accepts to lift it.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ChallengeBody {
    token: String,
    options: Vec<String>,
}

/// The challenge option a person at a linked device can meet (the other one, `pushChallenge`,
/// goes to the phone's push channel).
const CHALLENGE_CAPTCHA: &str = "captcha";

/// Handles `Command::Transmit` in a 1:1 chat; `uploader` prepares a message's body (attachments,
/// long text, quote) once the chat is known to take it.
pub(crate) async fn transmit<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    req: &OutgoingRequest,
    uploader: &media::Uploader<'_, T>,
) -> TransmitOutcome {
    match transmit_message(ctx, req, uploader).await {
        Ok(outcome) => outcome,
        Err(error) => {
            tracing::warn!(
                kind = error.kind.name(),
                request = req.kind.name(),
                error = %error.detail,
                "transmit failed"
            );
            TransmitOutcome::Failed(error)
        }
    }
}

async fn transmit_message<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    req: &OutgoingRequest,
    uploader: &media::Uploader<'_, T>,
) -> std::result::Result<TransmitOutcome, BackendError> {
    let ConvKey::Direct(user) = &req.conv else {
        return Err(BackendError::unsupported(
            "group sends go through send::group",
        ));
    };
    let aci = match parse_user_id(user) {
        Ok(SignalUser::Aci(aci)) => aci,
        Ok(_) => {
            return Err(BackendError::unsupported(
                "sends to a PNI or phone number need contact discovery",
            ));
        }
        Err(e) => return Err(BackendError::new(ErrorKind::InvalidInput, e.to_string())),
    };
    let timestamp = u64::try_from(req.sent_at)
        .map_err(|_| BackendError::new(ErrorKind::InvalidInput, "negative sent_at"))?;
    let peer = ServiceId::from(aci);
    let own_aci = ServiceId::from(ctx.account.aci);
    let note_to_self = peer == own_aci;
    if let OutgoingKind::Delete {
        target,
        for_everyone: false,
    } = &req.kind
    {
        return outgoing::sync_delete_for_me(ctx, &Thread::Direct(peer), target, req).await;
    }
    let backend = |e: Error| send_error(&e, user);
    let (accepted, row) = ctx
        .db
        .read(|c| -> Result<_> {
            Ok((
                features::is_accepted(c, ctx.account, peer)?,
                peers::load(c, &peer.service_id_string())?,
            ))
        })
        .map_err(backend)?;
    if !accepted {
        // §6.7 M1 rule: Kourier cannot turn on profile sharing (a Storage Service write).
        return Err(
            BackendError::new(ErrorKind::Unsupported, "chat not accepted on the phone")
                .with_user_message("Start this chat on your phone first."),
        );
    }
    let local = &*req.local;
    let wanted = req.only.as_ref().is_none_or(|only| only.contains(user));
    if !note_to_self && !wanted {
        // A resend to other members of a 1:1 chat: nothing left to send.
        return Ok(TransmitOutcome::Sent {
            server_at: None,
            per_recipient: Vec::new(),
        });
    }
    let chat = outgoing::Chat {
        account: ctx.account,
        timer: row.expire_timer,
        timer_version: Some(row.expire_timer_version),
        group: None,
    };
    let built = outgoing::build(ctx.db, uploader, &chat, req, ctx.now()).await?;
    let mut statuses = Vec::new();
    if !note_to_self {
        let mut outgoing = built.wire.content();
        if row.share_my_number {
            content::add_pni_signature(&mut outgoing, ctx.account).map_err(backend)?;
        }
        let accepted = send_to(
            ctx,
            peer,
            &Outgoing {
                content: &outgoing,
                timestamp,
                delivery: built.delivery,
                local: Some(local),
                group: None,
            },
        )
        .await
        .map_err(backend)?;
        statuses.push(RecipientStatus {
            recipient: peer,
            sealed: accepted.sealed,
        });
    }
    let transcript = content::sent_transcript(&Transcript {
        sent: &built.wire,
        timestamp,
        destination: Some(peer),
        statuses: &statuses,
        expiration_start: built.expires.then(|| ctx.now_ms()),
        is_recipient_update: req.is_recipient_update,
    });
    sync_transcript(ctx, &transcript, timestamp, local, note_to_self)
        .await
        .map_err(backend)?;
    let sealed = statuses.first().is_some_and(|s| s.sealed);
    Ok(TransmitOutcome::Sent {
        server_at: None,
        per_recipient: vec![(user.clone(), RecipientState::Sent { sealed })],
    })
}

/// Sends the sync transcript `transcript` of our message at `timestamp` to our other devices,
/// with progress under `key` (a repeated send skips it). A failure becomes a one-day job, unless
/// the transcript is `essential` (Note to Self, a lonely group: it is the message).
pub(crate) async fn sync_transcript<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    transcript: &proto::Content,
    timestamp: u64,
    key: &str,
    essential: bool,
) -> Result<()> {
    let synced = send_to(
        ctx,
        ServiceId::from(ctx.account.aci),
        &Outgoing {
            content: transcript,
            timestamp,
            delivery: Delivery::TRANSCRIPT,
            local: Some(key),
            group: None,
        },
    )
    .await;
    match synced {
        Ok(_) => Ok(()),
        Err(e) if essential => Err(e),
        Err(e) => {
            tracing::warn!(error = %e, "sync transcript failed; queued as a job");
            let now = ctx.now();
            let payload = transcript.encode_to_vec();
            ctx.db.zone(|tx| {
                jobs::enqueue_payload(
                    tx,
                    JobKind::SentTranscript,
                    key,
                    &payload,
                    now,
                    Some(TRANSCRIPT_MAX_AGE),
                )
            })?;
            ctx.jobs_wake.notify_one();
            Ok(())
        }
    }
}

/// The contract error of a failed send to `user`.
pub(crate) fn send_error(e: &Error, user: &UserId) -> BackendError {
    match e {
        Error::Protocol(SignalProtocolError::UntrustedIdentity(_)) => BackendError::new(
            ErrorKind::UntrustedIdentity { user: user.clone() },
            e.to_string(),
        ),
        _ => e.to_backend_error(),
    }
}

/// One sync request to our other devices (§6.5 step 8, `SD/ts/textsecure/syncRequests.preload.ts`),
/// an unsealed message sent through the job queue. CONTACTS and KEYS are urgent, CONFIGURATION
/// and BLOCKED are not (`SendMessage.preload.ts:1654-1836`). `timestamp` must be unique among our
/// sends.
pub(crate) async fn send_sync_request<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    request: SyncRequest,
    timestamp: u64,
) -> Result<()> {
    use proto::sync_message::request::Type as RequestType;
    let (kind, urgent) = match request {
        SyncRequest::Contacts => (RequestType::Contacts, true),
        SyncRequest::Configuration => (RequestType::Configuration, false),
        SyncRequest::Blocked => (RequestType::Blocked, false),
        SyncRequest::Keys => (RequestType::Keys, true),
    };
    let content = content::sync_content(proto::SyncMessage {
        content: Some(proto::sync_message::Content::Request(
            proto::sync_message::Request {
                r#type: Some(kind.into()),
            },
        )),
        ..Default::default()
    });
    send_to(
        ctx,
        ctx.account.aci.into(),
        &Outgoing {
            content: &content,
            timestamp,
            delivery: Delivery::sync_request(urgent),
            local: None,
            group: None,
        },
    )
    .await
    .map(|_| ())
}

/// What [`send_payload`] encrypts for each device.
#[derive(Clone, Copy)]
enum Payload<'a> {
    /// A `Content`: padded, then encrypted with the device's session.
    Content(&'a proto::Content),
    /// A `PlaintextContent` (a decryption-error message): sent as it is, sealed or as a
    /// `PLAINTEXT_CONTENT` envelope; the session only names the registration id.
    Plaintext(&'a PlaintextContent),
}

/// The bytes [`encrypt_for`] sends to each device.
enum Encrypt<'a> {
    /// The padded `Content`.
    Padded(&'a [u8]),
    /// A plaintext content.
    Plaintext(&'a PlaintextContent),
}

/// Sends `out` to every device of `destination` (our other devices when it is our own ACI).
/// See the module docs for the steps.
pub(crate) async fn send_to<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    destination: ServiceId,
    out: &Outgoing<'_>,
) -> Result<Accepted> {
    send_payload(
        ctx,
        destination,
        Payload::Content(out.content),
        out.timestamp,
        out.delivery,
        out.local,
        out.group,
    )
    .await
}

/// Sends a `PlaintextContent` (a retry request, §6.6) to every device of `destination`: Desktop
/// addresses the sender's account, and the message names the device it is about.
pub(crate) async fn send_plaintext_to<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    destination: ServiceId,
    plaintext: &PlaintextContent,
    timestamp: u64,
    delivery: Delivery,
) -> Result<Accepted> {
    send_payload(
        ctx,
        destination,
        Payload::Plaintext(plaintext),
        timestamp,
        delivery,
        None,
        None,
    )
    .await
}

async fn send_payload<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    destination: ServiceId,
    payload: Payload<'_>,
    timestamp: u64,
    delivery: Delivery,
    local: Option<&str>,
    group: Option<InGroup<'_>>,
) -> Result<Accepted> {
    let their = destination.service_id_string();
    let _serialized = ctx.locks.lock(&their).await;
    if let Some(local) = local
        && let Some(sealed) = ctx.db.read(|c| msl::accepted(c, local, &their))?
    {
        tracing::debug!(
            recipient = ?log_safe(destination),
            "the server has this message for the recipient already; not sent again"
        );
        return Ok(Accepted { sealed });
    }
    let own = destination == ServiceId::from(ctx.account.aci);
    let own_device = own.then_some(ctx.account.device_id);
    let (bytes, padded) = match payload {
        Payload::Content(content) => {
            let bytes = content.encode_to_vec();
            let padded = pad(&bytes);
            (bytes, padded)
        }
        Payload::Plaintext(_) => (Vec::new(), Vec::new()),
    };
    let input = match payload {
        Payload::Content(_) => Encrypt::Padded(&padded),
        Payload::Plaintext(plaintext) => Encrypt::Plaintext(plaintext),
    };
    let mut seal = if delivery.may_seal && !own {
        sealed::seal_for(ctx, destination, group.and_then(|g| g.token)).await?
    } else {
        None
    };
    let mut fetched = false;
    for _ in 0..MAX_ATTEMPTS {
        let mut devices = ctx
            .db
            .read(|c| usable_devices(c, &their, own_device, true))?;
        if devices.is_empty() && !fetched {
            fetch_and_process(ctx, destination, None, own_device, &mut seal).await?;
            fetched = true;
            devices = ctx
                .db
                .read(|c| usable_devices(c, &their, own_device, false))?;
        }
        if devices.is_empty() {
            if own {
                // No other device of ours: nothing to sync to.
                return Ok(Accepted { sealed: false });
            }
            return Err(Error::Internal("no session with any recipient device"));
        }
        let now = ctx.now();
        let messages = ctx.db.zone(|tx| {
            encrypt_for(
                tx,
                ctx.account,
                &their,
                &devices,
                &input,
                now,
                seal.as_ref(),
                delivery.hint,
                group.and_then(|g| g.content_group()),
            )
        })?;
        kourier_db::failpoint::hit(FAILPOINT_AFTER_ENCRYPT_COMMIT);
        if let Some(point) = delivery.failpoint {
            kourier_db::failpoint::hit(point);
        }
        let body = OutgoingMessages {
            messages,
            timestamp,
            online: delivery.online,
            urgent: delivery.urgent,
        };
        let request = json_request(Method::PUT, &format!("/v1/messages/{their}"), &body)?;
        let response = request_with(ctx, request, seal.as_ref()).await?;
        match response.status {
            status if status.is_success() => {
                let sealed = seal.is_some();
                let now = ctx.now();
                ctx.db.zone(|tx| -> Result<()> {
                    if let Some(local) = local {
                        msl::record_accepted(tx, local, &their, sealed, now)?;
                    }
                    if let Payload::Content(content) = payload
                        && delivery.log
                    {
                        let payload = msl::Payload {
                            timestamp,
                            content: &bytes,
                            hint: delivery.hint,
                            urgent: delivery.urgent,
                            has_pni_signature: content.pni_signature_message.is_some(),
                            local,
                            group_id: group.and_then(|g| g.content_group()),
                        };
                        msl::record(tx, &payload, &their, &devices, now)?;
                    }
                    if let Some(seal) = &seal {
                        let before = peers::load(tx, &their)?.sealed;
                        let after = seal.state_after_success(before);
                        if after != before {
                            peers::set_sealed_state(tx, &their, after, now)?;
                        }
                    }
                    Ok(())
                })?;
                tracing::debug!(
                    recipient = ?log_safe(destination),
                    devices = devices.len(),
                    sealed,
                    "message accepted by the server"
                );
                return Ok(Accepted { sealed });
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN if seal.is_some() => {
                // `UnauthorizedMessageSendError`: sealed sender is off for them; resend unsealed.
                tracing::info!(
                    recipient = ?log_safe(destination),
                    "sealed send refused; sending unsealed"
                );
                disable_sealed(ctx, &their)?;
                seal = None;
            }
            StatusCode::NOT_FOUND => {
                let now = ctx.now();
                ctx.db
                    .zone(|tx| peers::mark_unregistered(tx, &their, now))?;
                return Err(Error::Status {
                    what: "message send",
                    status: 404,
                });
            }
            StatusCode::CONFLICT => {
                let mismatch: MismatchedDevices = parse_json(&response)?;
                if mismatch.missing_devices.is_empty() && mismatch.extra_devices.is_empty() {
                    // `handleMismatchedDevicesError`: an empty entry refetches every device.
                    fetch_and_process(ctx, destination, None, own_device, &mut seal).await?;
                    fetched = true;
                    continue;
                }
                let now = ctx.now();
                ctx.db.zone(|tx| -> Result<()> {
                    for device in &mismatch.extra_devices {
                        if let Ok(device) = DeviceId::try_from(*device) {
                            archive_session(tx, IdentityKind::Aci, &their, device, now)?;
                        }
                    }
                    Ok(())
                })?;
                for device in mismatch.missing_devices {
                    if let Ok(device) = DeviceId::try_from(device)
                        && Some(device) != own_device
                    {
                        fetch_and_process(ctx, destination, Some(device), own_device, &mut seal)
                            .await?;
                    }
                }
                tracing::debug!(recipient = ?log_safe(destination), "device list repaired (409)");
            }
            StatusCode::GONE => {
                let stale: StaleDevices = parse_json(&response)?;
                let stale: Vec<DeviceId> = stale
                    .stale_devices
                    .into_iter()
                    .filter_map(|d| DeviceId::try_from(d).ok())
                    .collect();
                let now = ctx.now();
                ctx.db.zone(|tx| -> Result<()> {
                    for device in &stale {
                        archive_session(tx, IdentityKind::Aci, &their, *device, now)?;
                    }
                    Ok(())
                })?;
                for device in stale {
                    fetch_and_process(ctx, destination, Some(device), own_device, &mut seal)
                        .await?;
                }
                tracing::debug!(recipient = ?log_safe(destination), "stale sessions renewed (410)");
            }
            status => return Err(status_error(&response, status, "message send")),
        }
    }
    Err(Error::Internal("the recipient's device list kept changing"))
}

/// The error of an unexpected answer: 428 challenge, 413/429 rate limit, or the bare status.
///
/// A 428 is a challenge the user can answer only when it names a token and offers a captcha
/// (`SD/ts/challenge.dom.ts` `register`: "unexpected options", "no token in data"). The server
/// leaves the captcha out when the account has used up its captcha attempts; such a 428 is a
/// rate limit that lifts by itself at its `Retry-After`, and asking the user for a captcha the
/// server will not take would pause the outbox for nothing.
fn status_error(response: &ChatResponse, status: StatusCode, what: &'static str) -> Error {
    let retry_after_secs = || {
        response
            .headers
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse().ok())
    };
    match status.as_u16() {
        428 => {
            let challenge = parse_json::<ChallengeBody>(response).unwrap_or_default();
            let solvable = !challenge.token.is_empty()
                && challenge.options.iter().any(|o| o == CHALLENGE_CAPTCHA);
            if solvable {
                Error::Challenge {
                    token: kourier_model::Redacted::new(challenge.token),
                    retry_after_secs: retry_after_secs(),
                }
            } else {
                tracing::warn!(
                    options = challenge.options.len(),
                    "a send challenge without a captcha to solve; waiting it out"
                );
                Error::RateLimited {
                    retry_after_secs: retry_after_secs(),
                }
            }
        }
        413 | 429 => Error::RateLimited {
            retry_after_secs: retry_after_secs(),
        },
        status => Error::Status { what, status },
    }
}

/// Sends `request` sealed (on the unauthenticated socket, with the access key) or unsealed.
async fn request_with<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    request: ChatRequest,
    seal: Option<&Seal>,
) -> Result<ChatResponse> {
    let Some(seal) = seal else {
        return Ok(ctx.net.request(ctx.socket, request).await?);
    };
    let (name, value) = seal.header();
    let mut value = HeaderValue::from_str(&value)
        .map_err(|_| Error::Internal("access key is not a header value"))?;
    value.set_sensitive(true);
    let request = request.with_header(HeaderName::from_static(name), value);
    let socket = ctx.unauth.get(ctx.net, ctx.now_ms()).await?;
    match ctx.net.request(&socket, request).await {
        Ok(response) => Ok(response),
        Err(e) => {
            ctx.unauth.invalidate(&socket);
            Err(e.into())
        }
    }
}

/// Marks sealed sender off for `their` (401/403 on a sealed request).
fn disable_sealed<T: SignalTransport>(ctx: &SendContext<'_, T>, their: &str) -> Result<()> {
    let now = ctx.now();
    ctx.db
        .zone(|tx| peers::set_sealed_state(tx, their, SealedSenderState::Disabled, now))
}

/// Log-safe form of a service id.
fn log_safe(service_id: ServiceId) -> SignalUser {
    match service_id {
        ServiceId::Aci(aci) => SignalUser::Aci(aci),
        ServiceId::Pni(pni) => SignalUser::Pni(pni),
    }
}

/// The devices of `their` we can encrypt to now: a current session whose sender chain is
/// usable (`strict`: also not stale, PQXDH and SPQR, as the bridge's `hasCurrentState`), minus
/// our own device.
pub(crate) fn usable_devices(
    conn: &kourier_db::rusqlite::Connection,
    their: &str,
    own_device: Option<DeviceId>,
    strict: bool,
) -> Result<Vec<DeviceId>> {
    let requirements = if strict {
        SessionUsabilityRequirements::NotStale
            | SessionUsabilityRequirements::EstablishedWithPqxdh
            | SessionUsabilityRequirements::Spqr
    } else {
        SessionUsabilityRequirements::empty()
    };
    let now = SystemTime::now();
    Ok(session_devices(conn, IdentityKind::Aci, their)?
        .into_iter()
        .filter(|(device, record)| {
            Some(*device) != own_device
                && record
                    .has_usable_sender_chain(now, requirements)
                    .unwrap_or(false)
        })
        .map(|(device, _)| device)
        .collect())
}

/// Encrypts `input` for each device in one zone (§6.3 rule 3: the ciphertext leaves only after
/// this commit); sealed with `seal` when given, naming `group_id` in the sealed content for a
/// send that belongs to a group (`SD/ts/textsecure/OutgoingMessage.preload.ts`: receivers file
/// a decryption failure under that group and name it in their retry request). A plaintext
/// content is not encrypted, but still needs the device's session for its registration id.
#[allow(
    clippy::too_many_arguments,
    reason = "one zone body, all inputs are distinct"
)]
fn encrypt_for(
    tx: &kourier_db::Tx<'_>,
    account: &Account,
    their: &str,
    devices: &[DeviceId],
    input: &Encrypt<'_>,
    now: i64,
    seal: Option<&Seal>,
    hint: ContentHint,
    group_id: Option<&[u8]>,
) -> Result<Vec<OutgoingMessage>> {
    let mut stores = Stores::new(
        tx,
        IdentityKind::Aci,
        account.aci_identity,
        account.aci_registration_id,
        now,
    );
    let local = account.address(IdentityKind::Aci);
    let mut messages = Vec::with_capacity(devices.len());
    for device in devices {
        let remote = ProtocolAddress::new(their.to_owned(), *device);
        let ciphertext = match input {
            Encrypt::Padded(padded) => ready(message_encrypt(
                padded,
                &remote,
                &local,
                &mut stores.session,
                &mut stores.identity,
                SystemTime::now(),
                &mut rand::rng(),
            ))??,
            Encrypt::Plaintext(plaintext) => {
                CiphertextMessage::PlaintextContent((*plaintext).clone())
            }
        };
        let registration_id = ready(stores.session.load_session(&remote))??
            .ok_or(Error::Internal("session vanished after encrypt"))?
            .remote_registration_id()?;
        let (kind, bytes) = match (seal, &ciphertext) {
            (_, CiphertextMessage::SenderKeyMessage(_)) => {
                return Err(Error::Internal("unexpected ciphertext type"));
            }
            (Some(seal), _) => {
                let usmc = UnidentifiedSenderMessageContent::new(
                    ciphertext.message_type(),
                    seal.certificate.clone(),
                    ciphertext.serialize().to_vec(),
                    hint,
                    group_id.map(<[u8]>::to_vec),
                )?;
                let sealed = ready(sealed_sender_encrypt_from_usmc(
                    &remote,
                    &usmc,
                    &stores.identity,
                    &mut rand::rng(),
                ))??;
                (EnvelopeType::UnidentifiedSender, sealed)
            }
            (None, CiphertextMessage::SignalMessage(m)) => {
                (EnvelopeType::DoubleRatchet, m.serialized().to_vec())
            }
            (None, CiphertextMessage::PreKeySignalMessage(m)) => {
                (EnvelopeType::PrekeyMessage, m.serialized().to_vec())
            }
            (None, CiphertextMessage::PlaintextContent(m)) => {
                (EnvelopeType::PlaintextContent, m.serialized().to_vec())
            }
        };
        messages.push(OutgoingMessage {
            kind: kind.into(),
            destination_device_id: u32::from(*device),
            destination_registration_id: registration_id,
            content: STANDARD.encode(bytes),
        });
    }
    Ok(messages)
}

/// `GET /v2/keys/{destination}/{device|*}` (with the access key on the unauthenticated socket
/// when the send is sealed, falling back to the authenticated socket on 401/403, Desktop's
/// `getServerKeys` cascade) and a new session per returned device, skipping our own device.
/// 404: the account is gone; every session with it is archived.
pub(crate) async fn fetch_and_process<T: SignalTransport>(
    ctx: &SendContext<'_, T>,
    destination: ServiceId,
    device: Option<DeviceId>,
    own_device: Option<DeviceId>,
    seal: &mut Option<Seal>,
) -> Result<()> {
    let their = destination.service_id_string();
    let path = match device {
        Some(device) => format!("/v2/keys/{their}/{device}"),
        None => format!("/v2/keys/{their}/*"),
    };
    let mut response = request_with(ctx, ChatRequest::get(&path)?, seal.as_ref()).await?;
    if seal.is_some() && matches!(response.status.as_u16(), 401 | 403) {
        disable_sealed(ctx, &their)?;
        *seal = None;
        response = ctx
            .net
            .request(ctx.socket, ChatRequest::get(&path)?)
            .await?;
    }
    if response.status == StatusCode::NOT_FOUND && device.is_none() {
        let now = ctx.now();
        ctx.db.zone(|tx| -> Result<()> {
            peers::mark_unregistered(tx, &their, now)?;
            archive_all_sessions(tx, &their, now)?;
            Ok(())
        })?;
    }
    if !response.status.is_success() {
        return Err(status_error(&response, response.status, "prekey fetch"));
    }
    let bundles = parse_bundles(&response.body)?;
    let now = ctx.now();
    let untrusted = ctx.db.zone(|tx| -> Result<Option<SignalProtocolError>> {
        let mut untrusted = None;
        let changes = {
            let mut stores = Stores::new(
                tx,
                IdentityKind::Aci,
                ctx.account.aci_identity,
                ctx.account.aci_registration_id,
                now,
            );
            let local = ctx.account.address(IdentityKind::Aci);
            for (device, bundle) in &bundles {
                if Some(*device) == own_device {
                    continue;
                }
                let remote = ProtocolAddress::new(their.clone(), *device);
                match ready(process_prekey_bundle(
                    &remote,
                    &local,
                    &mut stores.session,
                    &mut stores.identity,
                    bundle,
                    SystemTime::now(),
                    &mut rand::rng(),
                ))? {
                    Ok(()) => {}
                    // Never for our own account: our identity comes from provisioning only.
                    Err(e @ SignalProtocolError::UntrustedIdentity(_)) if own_device.is_none() => {
                        // A new identity key first seen here (the contact reinstalled): store
                        // it as Desktop's profile fetch does (`saveIdentity`: a verified
                        // contact becomes unverified, the old sessions are archived), tell the
                        // core, and fail this send so the user hears of the change. The next
                        // attempt builds the session (after `ConfirmIdentity` for a contact
                        // that was verified); without this, every attempt would fail the same
                        // way until the contact wrote to us.
                        ready(
                            stores
                                .identity
                                .save_identity(&remote, bundle.identity_key()?),
                        )??;
                        untrusted = Some(e);
                        break;
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            stores.identity.take_changes()
        };
        let events = crate::receive::stage1::identity_events(tx, &changes, now)?;
        if !events.is_empty() {
            append_events(tx, &events, now, ctx.outbox_wake)?;
        }
        Ok(untrusted)
    })?;
    if let Some(e) = untrusted {
        tracing::info!(
            recipient = ?log_safe(destination),
            "a prekey fetch brought a new identity key; the send waits for the user"
        );
        return Err(e.into());
    }
    tracing::debug!(
        recipient = ?log_safe(destination),
        devices = bundles.len(),
        "sessions from prekey bundles"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    use tokio::task::{LocalSet, spawn_local};

    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn sends_to_one_recipient_are_serialized_and_others_run_concurrently() {
        LocalSet::new()
            .run_until(async {
                let locks = Rc::new(SendLocks::default());
                let log = Rc::new(RefCell::new(Vec::new()));
                let send = |who: &'static str, recipient: &'static str| {
                    let (locks, log) = (locks.clone(), log.clone());
                    spawn_local(async move {
                        let _guard = locks.lock(recipient).await;
                        log.borrow_mut().push(format!("{who} start"));
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        log.borrow_mut().push(format!("{who} end"));
                    })
                };
                // A transmit and a sync request to our own ACI, and a send to someone else.
                let a = send("transmit", "own");
                let b = send("sync request", "own");
                let c = send("other", "peer");
                for task in [a, b, c] {
                    task.await.unwrap();
                }
                let log = log.borrow().clone();
                let at = |entry: &str| log.iter().position(|e| e == entry).unwrap();
                assert!(at("transmit end") < at("sync request start"), "{log:?}");
                assert!(at("other start") < at("transmit end"), "{log:?}");
                // Idle recipients are forgotten.
                let _guard = locks.lock("third").await;
                assert_eq!(locks.locks.borrow().len(), 1);
            })
            .await;
    }

    /// §4.8 step 4: a recipient whose server acceptance is recorded for this message is not
    /// sent to again (the fake server never answers, so any request would time out).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recipient_with_recorded_progress_is_skipped() {
        LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let test = crate::actor::testing::shared(
                    fixture.db.clone(),
                    fixture.account.clone(),
                    Vec::new(),
                );
                let shared = test.shared;
                let (socket, _remote, _events) =
                    crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
                let socket = Arc::new(socket);
                let peer = ServiceId::from(libsignal_core::Aci::from(uuid::Uuid::from_u128(4)));
                shared
                    .db
                    .zone(|tx| msl::record_accepted(tx, "1700", &peer.service_id_string(), true, 1))
                    .unwrap();
                let account = fixture.account.clone();
                let ctx = shared.send_context(&account, &socket);
                let content = content::receipt(content::ReceiptType::Read, &[1]);
                let out = |local| Outgoing {
                    content: &content,
                    timestamp: 1700,
                    delivery: Delivery::MESSAGE,
                    local,
                    group: None,
                };
                let skipped = tokio::time::timeout(
                    Duration::from_secs(5),
                    send_to(&ctx, peer, &out(Some("1700"))),
                )
                .await
                .expect("no network round trip")
                .unwrap();
                assert_eq!(skipped, Accepted { sealed: true });
                // Without recorded progress the send goes to the network (here: it fails, as
                // there is no session and the fake server never answers the key fetch).
                let tried = tokio::time::timeout(
                    Duration::from_millis(300),
                    send_to(&ctx, peer, &out(None)),
                )
                .await;
                assert!(
                    tried.is_err() || tried.is_ok_and(|r| r.is_err()),
                    "a send without progress does not short-circuit"
                );
            })
            .await;
    }

    /// A contact as the fake server knows it: an identity, and fresh prekeys per fetch.
    struct Contact {
        aci: libsignal_core::Aci,
        identity: libsignal_protocol::IdentityKeyPair,
    }

    impl Contact {
        fn new(n: u128) -> Self {
            Self {
                aci: libsignal_core::Aci::from(uuid::Uuid::from_u128(n)),
                identity: libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng()),
            }
        }

        fn sid(&self) -> String {
            ServiceId::from(self.aci).service_id_string()
        }

        /// `GET /v2/keys` for `(device, registration id)`s, signed by the current identity.
        fn keys(&self, devices: &[(u32, u32)]) -> Vec<u8> {
            use crate::keys::{SignedPreKeyJson, generate_kyber_prekey, generate_signed_prekey};
            let rng = &mut rand::rng();
            let devices: Vec<_> = devices
                .iter()
                .map(|&(device, registration)| {
                    let now = libsignal_protocol::Timestamp::from_epoch_millis(1);
                    let signed = generate_signed_prekey(&self.identity, device, now, rng).unwrap();
                    let kyber = generate_kyber_prekey(&self.identity, device + 100).unwrap();
                    serde_json::json!({
                        "deviceId": device,
                        "registrationId": registration,
                        "signedPreKey": SignedPreKeyJson::signed(&signed).unwrap(),
                        "pqPreKey": SignedPreKeyJson::kyber(&kyber).unwrap(),
                    })
                })
                .collect();
            serde_json::to_vec(&serde_json::json!({
                "identityKey": STANDARD.encode(self.identity.identity_key().serialize()),
                "devices": devices,
            }))
            .unwrap()
        }
    }

    /// The fake server answers the client's next request, which must be `verb path`; returns
    /// the request body.
    async fn answer(
        remote: &libsignal_net::chat::fake::FakeChatRemote,
        verb: &str,
        path: &str,
        status: u16,
        body: &[u8],
    ) -> Vec<u8> {
        let request = remote
            .receive_request()
            .await
            .expect("a request")
            .expect("socket open");
        assert_eq!(
            (request.verb.as_deref(), request.path.as_deref()),
            (Some(verb), Some(path))
        );
        remote
            .send_response(libsignal_net::chat::ResponseProto {
                id: request.id,
                status: Some(u32::from(status)),
                message: Some(String::new()),
                headers: vec!["Content-Type: application/json".to_owned()],
                body: Some(bytes::Bytes::copy_from_slice(body)),
            })
            .unwrap();
        request.body.map(|b| b.to_vec()).unwrap_or_default()
    }

    /// `(device, registration id, envelope type)` of a `PUT /v1/messages` body, by device.
    fn sent_to(body: &[u8]) -> Vec<(u64, u64, u64)> {
        let body: serde_json::Value = serde_json::from_slice(body).unwrap();
        let mut sent: Vec<_> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["destinationDeviceId"].as_u64().unwrap(),
                    m["destinationRegistrationId"].as_u64().unwrap(),
                    m["type"].as_u64().unwrap(),
                )
            })
            .collect();
        sent.sort_unstable();
        sent
    }

    /// The core events in `core_outbox`.
    fn outbox_events(db: &ZoneDb) -> Vec<kourier_backend_api::BackendEvent> {
        db.read(|c| -> Result<Vec<Vec<u8>>> {
            let mut stmt = c.prepare("SELECT envelope FROM core_outbox ORDER BY seq")?;
            let rows = stmt
                .query_map([], |r| r.get(0))?
                .collect::<kourier_db::rusqlite::Result<_>>()?;
            Ok(rows)
        })
        .unwrap()
        .iter()
        .flat_map(|row| kourier_backend_api::decode_outbox_row(row).unwrap())
        .collect()
    }

    /// §6.7 step 3 against a scripted server (the mock never answers 410): an empty 409
    /// refetches every device, a 409 archives the extra device and builds a session for the
    /// missing one, a 410 renews the stale session (new registration id), and the message goes
    /// to exactly the devices the server wants.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn device_lists_are_repaired_on_409_and_410() {
        LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let test = crate::actor::testing::shared(
                    fixture.db.clone(),
                    fixture.account.clone(),
                    Vec::new(),
                );
                let shared = test.shared;
                let (socket, remote, _events) =
                    crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
                let socket = Arc::new(socket);
                let alice = Contact::new(0xa11ce);
                let sid = alice.sid();
                // Unsealed: the scripted server is the authenticated socket only.
                shared
                    .db
                    .zone(|tx| peers::set_sealed_state(tx, &sid, SealedSenderState::Disabled, 1))
                    .unwrap();
                let account = fixture.account.clone();
                let ctx = shared.send_context(&account, &socket);
                let content = content::receipt(content::ReceiptType::Read, &[1]);
                let out = Outgoing {
                    content: &content,
                    timestamp: 1700,
                    delivery: Delivery::MESSAGE,
                    local: Some("1700"),
                    group: None,
                };
                let messages = format!("/v1/messages/{sid}");
                let server = async {
                    let keys = format!("/v2/keys/{sid}/*");
                    answer(
                        &remote,
                        "GET",
                        &keys,
                        200,
                        &alice.keys(&[(1, 101), (2, 102)]),
                    )
                    .await;
                    // An empty mismatch refetches every device.
                    answer(&remote, "PUT", &messages, 409, b"{}").await;
                    answer(
                        &remote,
                        "GET",
                        &keys,
                        200,
                        &alice.keys(&[(1, 101), (2, 102)]),
                    )
                    .await;
                    let first = answer(
                        &remote,
                        "PUT",
                        &messages,
                        409,
                        br#"{"missingDevices":[3],"extraDevices":[2]}"#,
                    )
                    .await;
                    assert_eq!(
                        sent_to(&first),
                        [(1, 101, 3), (2, 102, 3)],
                        "PreKey messages"
                    );
                    let keys = format!("/v2/keys/{sid}/3");
                    answer(&remote, "GET", &keys, 200, &alice.keys(&[(3, 103)])).await;
                    let second =
                        answer(&remote, "PUT", &messages, 410, br#"{"staleDevices":[1]}"#).await;
                    assert_eq!(sent_to(&second), [(1, 101, 3), (3, 103, 3)]);
                    let keys = format!("/v2/keys/{sid}/1");
                    answer(&remote, "GET", &keys, 200, &alice.keys(&[(1, 201)])).await;
                    let third = answer(&remote, "PUT", &messages, 200, b"{}").await;
                    assert_eq!(
                        sent_to(&third),
                        [(1, 201, 3), (3, 103, 3)],
                        "the renewed session"
                    );
                };
                let (sent, ()) = tokio::join!(send_to(&ctx, alice.aci.into(), &out), server);
                assert_eq!(sent.unwrap(), Accepted { sealed: false });
                let (devices, accepted, logged) = shared
                    .db
                    .read(|c| -> Result<_> {
                        let logged: Vec<u32> = c
                            .prepare("SELECT device FROM msl_recipients ORDER BY device")?
                            .query_map([], |r| r.get(0))?
                            .collect::<kourier_db::rusqlite::Result<_>>()?;
                        Ok((
                            usable_devices(c, &sid, None, false)?,
                            msl::accepted(c, "1700", &sid)?,
                            logged,
                        ))
                    })
                    .unwrap();
                let ids: Vec<u32> = devices.into_iter().map(u32::from).collect();
                assert_eq!(ids, [1, 3], "the extra device's session is archived");
                assert_eq!(accepted, Some(false), "send progress recorded");
                assert_eq!(logged, [1, 3], "the send log has the devices that got it");
            })
            .await;
    }

    /// A 404 marks the contact unregistered (`PeerUnregistered` for the core). A new identity key
    /// first seen in a prekey fetch (the contact reinstalled) is stored and reported, this send
    /// fails as `UntrustedIdentity`, and the next one goes through; for a contact that was
    /// verified only after `ConfirmIdentity`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unregistered_contacts_and_new_identity_keys() {
        LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let test = crate::actor::testing::shared(
                    fixture.db.clone(),
                    fixture.account.clone(),
                    Vec::new(),
                );
                let shared = test.shared;
                let (socket, remote, _events) =
                    crate::net::libsignal::fake_chat(tokio::runtime::Handle::current());
                let socket = Arc::new(socket);
                let account = fixture.account.clone();
                let ctx = shared.send_context(&account, &socket);
                let content = content::receipt(content::ReceiptType::Read, &[1]);
                let out = Outgoing {
                    content: &content,
                    timestamp: 1800,
                    delivery: Delivery::MESSAGE,
                    local: None,
                    group: None,
                };
                let disable = |sid: &str| {
                    shared
                        .db
                        .zone(|tx| peers::set_sealed_state(tx, sid, SealedSenderState::Disabled, 1))
                        .unwrap();
                };
                let user = |contact: &Contact| crate::service_id::aci_user_id(contact.aci).unwrap();

                // Gone: 404 on the key fetch.
                let gone = Contact::new(0x60e);
                disable(&gone.sid());
                let keys = format!("/v2/keys/{}/*", gone.sid());
                let (sent, _) = tokio::join!(
                    send_to(&ctx, gone.aci.into(), &out),
                    answer(&remote, "GET", &keys, 404, b"")
                );
                let error = send_error(&sent.unwrap_err(), &user(&gone));
                assert_eq!(error.kind, ErrorKind::PeerUnregistered);
                let row = shared.db.read(|c| peers::load(c, &gone.sid())).unwrap();
                assert!(row.unregistered);

                // Bob: a session with his first identity.
                let mut bob = Contact::new(0xb0b);
                let sid = bob.sid();
                disable(&sid);
                let all_keys = format!("/v2/keys/{sid}/*");
                let device_keys = format!("/v2/keys/{sid}/1");
                let messages = format!("/v1/messages/{sid}");
                let first_send = async {
                    answer(&remote, "GET", &all_keys, 200, &bob.keys(&[(1, 7)])).await;
                    answer(&remote, "PUT", &messages, 200, b"{}").await;
                };
                let (sent, ()) = tokio::join!(send_to(&ctx, bob.aci.into(), &out), first_send);
                sent.unwrap();
                let stored_key = |c: &kourier_db::rusqlite::Connection| -> Result<(Vec<u8>, i64)> {
                    Ok(c.query_row(
                        "SELECT identity_key, verified FROM identities WHERE service_id = ?1",
                        [&sid],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?)
                };
                let identity_changes = || {
                    outbox_events(&shared.db)
                        .into_iter()
                        .filter(|e| {
                            matches!(e, kourier_backend_api::BackendEvent::ContactUpsert(p)
                                if p.trust == Some(kourier_model::TrustLevel::Unverified))
                        })
                        .count()
                };

                // Bob reinstalls (and so for a second time below, as a verified contact): the
                // server says the session is stale, the fresh keys carry a new identity.
                for (round, verified) in [(1, false), (2, true)] {
                    bob.identity = libsignal_protocol::IdentityKeyPair::generate(&mut rand::rng());
                    if verified {
                        shared
                            .db
                            .zone(|tx| -> Result<()> {
                                tx.execute(
                                    "UPDATE identities SET verified = 1 WHERE service_id = ?1",
                                    [&sid],
                                )?;
                                Ok(())
                            })
                            .unwrap();
                    }
                    let reinstall = async {
                        answer(&remote, "PUT", &messages, 410, br#"{"staleDevices":[1]}"#).await;
                        answer(&remote, "GET", &device_keys, 200, &bob.keys(&[(1, 8)])).await;
                    };
                    let (sent, ()) = tokio::join!(send_to(&ctx, bob.aci.into(), &out), reinstall);
                    let error = send_error(&sent.unwrap_err(), &user(&bob));
                    assert!(
                        matches!(error.kind, ErrorKind::UntrustedIdentity { .. }),
                        "round {round}: {error:?}"
                    );
                    let (key, state) = shared.db.read(stored_key).unwrap();
                    assert_eq!(
                        key,
                        bob.identity.identity_key().serialize().to_vec(),
                        "round {round}: the new key is stored"
                    );
                    assert_eq!(state, if verified { 2 } else { 0 }, "round {round}");
                    assert_eq!(
                        identity_changes(),
                        round,
                        "round {round}: the core hears of it"
                    );

                    if verified {
                        // Still refused (and no second report) until the user confirms.
                        let keys = bob.keys(&[(1, 8)]);
                        let (sent, _) = tokio::join!(
                            send_to(&ctx, bob.aci.into(), &out),
                            answer(&remote, "GET", &all_keys, 200, &keys)
                        );
                        assert!(matches!(
                            sent,
                            Err(Error::Protocol(SignalProtocolError::UntrustedIdentity(_)))
                        ));
                        assert_eq!(identity_changes(), round);
                        shared
                            .db
                            .zone(|tx| crate::commands::confirm_identity(tx, bob.aci))
                            .unwrap();
                    }
                    let retry = async {
                        answer(&remote, "GET", &all_keys, 200, &bob.keys(&[(1, 8)])).await;
                        answer(&remote, "PUT", &messages, 200, b"{}").await
                    };
                    let (sent, body) = tokio::join!(send_to(&ctx, bob.aci.into(), &out), retry);
                    sent.unwrap();
                    assert_eq!(sent_to(&body), [(1, 8, 3)], "round {round}: a new session");
                }

                // A foreign key in our own account's prekeys never replaces our identity.
                let own = ServiceId::from(account.aci).service_id_string();
                let own_key = account.aci_identity.identity_key().serialize().to_vec();
                shared
                    .db
                    .zone(|tx| -> Result<()> {
                        crate::store::save_own_identity(
                            tx,
                            &own,
                            account.aci_identity.identity_key(),
                            1,
                        )?;
                        Ok(())
                    })
                    .unwrap();
                let impostor = Contact::new(0xbad);
                let keys = impostor.keys(&[(1, 9)]);
                let sync = content::read_sync(&[(bob.aci, 1)]);
                let to_own = Outgoing {
                    content: &sync,
                    timestamp: 1900,
                    delivery: Delivery::READ_SYNC,
                    local: None,
                    group: None,
                };
                let own_keys = format!("/v2/keys/{own}/*");
                let (sent, _) = tokio::join!(
                    send_to(&ctx, account.aci.into(), &to_own),
                    answer(&remote, "GET", &own_keys, 200, &keys)
                );
                assert!(matches!(
                    sent,
                    Err(Error::Protocol(SignalProtocolError::UntrustedIdentity(_)))
                ));
                let stored: Vec<u8> = shared
                    .db
                    .read(|c| -> Result<Vec<u8>> {
                        Ok(c.query_row(
                            "SELECT identity_key FROM identities WHERE service_id = ?1",
                            [&own],
                            |r| r.get(0),
                        )?)
                    })
                    .unwrap();
                assert_eq!(stored, own_key, "our identity is untouched");
                assert_eq!(identity_changes(), 2, "nothing reported");
            })
            .await;
    }

    #[test]
    fn deliveries_follow_desktop() {
        let flags = |d: Delivery| (d.urgent, d.online, d.log, d.may_seal);
        assert_eq!(flags(Delivery::MESSAGE), (true, false, true, true));
        // Edits, deletes and reactions go out like messages; a timer change is not urgent.
        assert_eq!(flags(Delivery::MODIFIER), flags(Delivery::MESSAGE));
        assert_eq!(Delivery::MODIFIER.hint, ContentHint::Resendable);
        assert_eq!(
            Delivery::MODIFIER.failpoint,
            Some(FAILPOINT_AFTER_MODIFIER_ENCRYPT_COMMIT)
        );
        assert_eq!(Delivery::MESSAGE.failpoint, None);
        assert_eq!(flags(Delivery::TIMER_UPDATE), (false, false, true, true));
        assert_eq!(flags(Delivery::TRANSCRIPT), (false, false, true, false));
        assert_eq!(flags(Delivery::READ_SYNC), (true, false, true, false));
        assert_eq!(flags(Delivery::RECEIPT), (false, false, true, true));
        assert_eq!(flags(Delivery::TYPING), (false, true, false, true));
        assert_eq!(
            flags(Delivery::sync_request(true)),
            (true, false, true, false)
        );
        assert_eq!(Delivery::TYPING.hint, ContentHint::Implicit);
        assert_eq!(Delivery::RECEIPT.hint, ContentHint::Resendable);
    }

    #[test]
    fn challenges_and_rate_limits_keep_their_details() {
        let response = |status: u16, body: &str, retry: Option<&str>| {
            let mut headers = http::HeaderMap::new();
            if let Some(retry) = retry {
                headers.insert(
                    http::header::RETRY_AFTER,
                    HeaderValue::from_str(retry).unwrap(),
                );
            }
            ChatResponse {
                status: StatusCode::from_u16(status).unwrap(),
                message: None,
                headers,
                body: bytes::Bytes::from(body.to_owned()),
            }
        };
        let r = response(428, r#"{"token":"t0k","options":["captcha"]}"#, None);
        match status_error(&r, r.status, "x") {
            Error::Challenge { token, .. } => assert_eq!(token.expose(), "t0k"),
            other => panic!("{other:?}"),
        }
        // A challenge the user cannot answer (no captcha on offer: the account used up its
        // captcha attempts; or no token) is a rate limit that ends at its Retry-After, as
        // Desktop waits it out instead of showing a captcha.
        for body in [
            r#"{"token":"t0k","options":["pushChallenge"]}"#,
            r#"{"token":"t0k","options":[]}"#,
            r#"{"token":"t0k"}"#,
            r#"{"options":["captcha"]}"#,
            r#"{"token":"","options":["captcha"]}"#,
            "",
            "not json",
        ] {
            let r = response(428, body, Some("3600"));
            assert!(
                matches!(
                    status_error(&r, r.status, "x"),
                    Error::RateLimited {
                        retry_after_secs: Some(3600)
                    }
                ),
                "{body}"
            );
        }
        let r = response(428, r#"{"token":"t0k","options":["pushChallenge"]}"#, None);
        assert!(matches!(
            status_error(&r, r.status, "x"),
            Error::RateLimited {
                retry_after_secs: None
            }
        ));
        // A captcha among other options is the one we take.
        let r = response(
            428,
            r#"{"token":"t1","options":["pushChallenge","captcha"]}"#,
            Some("60"),
        );
        match status_error(&r, r.status, "x") {
            Error::Challenge {
                token,
                retry_after_secs,
            } => {
                assert_eq!(token.expose(), "t1");
                assert_eq!(retry_after_secs, Some(60), "the server's Retry-After");
            }
            other => panic!("{other:?}"),
        }
        let r = response(429, "", Some("17"));
        assert!(matches!(
            status_error(&r, r.status, "x"),
            Error::RateLimited {
                retry_after_secs: Some(17)
            }
        ));
        let r = response(500, "", None);
        assert!(matches!(
            status_error(&r, r.status, "x"),
            Error::Status { status: 500, .. }
        ));
        let user = UserId::parse("aci:9d0652a3-dcc3-4d11-975f-74d61598733f").unwrap();
        let e = send_error(
            &Error::Challenge {
                token: kourier_model::Redacted::new("secret".to_owned()),
                retry_after_secs: Some(60),
            },
            &user,
        );
        assert!(matches!(
            &e.kind,
            ErrorKind::Challenge { retry_after: Some(wait), .. } if wait.as_secs() == 60
        ));
        assert!(
            !format!("{e:?}").contains("secret"),
            "the token stays out of logs"
        );
    }
}
