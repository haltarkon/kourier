//! Stage 1: decrypt a batch of envelopes in one zone and store them in `inbox`; the caller
//! acks the server only after this commit (§5.6, §6.6; `MessageReceiver.preload.ts`
//! `decryptAndCache`, `#unsealEnvelope`, `#decryptEnvelope`, `#innerDecrypt`).
//!
//! Every envelope type is handled: `DOUBLE_RATCHET`, `PREKEY_MESSAGE` (the only type accepted on
//! our PNI), `UNIDENTIFIED_SENDER` (v1 and single-recipient multi-recipient; `PreKey`,
//! `Whisper`, `SenderKey` (`group_decrypt`) and `Plaintext` inside), `PLAINTEXT_CONTENT`
//! (decryption errors only) and `SERVER_DELIVERY_RECEIPT`. A `senderKeyDistributionMessage` in
//! an encrypted `Content` is processed here, inside the decrypt zone, before later envelopes
//! that need it (§6.6). Facts learned here are written into `core_outbox` in the same zone
//! (§5.6 invariant 1): a replaced identity key, a verified PNI signature (`IdentityMerged`), and
//! the number of a sealed sender's certificate. Our phone's `SyncMessage.pniChangeNumber` is
//! applied here too, in the envelope's savepoint (`crate::number_change`): the envelopes after it
//! may already be addressed to the new PNI.
//!
//! A message that does not decrypt (other than a duplicate) from a known sender device is
//! stored as a row without content but with the `DecryptionErrorMessage` for that device, its
//! content hint and group id: stage 2 queues the retry request and emits the placeholder
//! (`crate::retry`). The failed decrypt's own writes are rolled back first.

use std::rc::Rc;

use http::StatusCode;
use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_db::rusqlite::{Connection, OptionalExtension};
use kourier_db::{Tx, ZoneDb};
use kourier_model::{
    Body, ContactPatch, ConvKey, Identifier, IdentifierKind, IdentityFingerprint, Origin, Patch,
    SystemEvent, TrustLevel,
};
use libsignal_core::{Aci, DeviceId, Pni, ServiceId};
use libsignal_protocol::{
    CiphertextMessageType, ContentHint, DecryptionErrorMessage, PlaintextContent,
    PreKeySignalMessage, ProtocolAddress, PublicKey, SenderKeyDistributionMessage, SignalMessage,
    SignalProtocolError, Timestamp, group_decrypt, message_decrypt_prekey, message_decrypt_signal,
    process_sender_key_distribution_message, sealed_sender_decrypt_to_usmc,
};
use prost::Message as _;
use tokio::sync::Notify;
use uuid::Uuid;

use super::{KIND_CONTENT, KIND_DELIVERY_RECEIPT, MISC_LANE, group_lane};
use crate::db::{self, Account};
use crate::error::{Result, protocol_error_kind};
use crate::net::{IncomingEnvelope, ServerAck};
use crate::outbox::append_event_rows;
use crate::padding::unpad;
use crate::proto::{self, content::Content as ContentKind, envelope::Type as EnvelopeType};
use crate::service_id::{
    SignalUser, aci_user_id, service_id_from_wire, service_id_user_id, validate_e164,
};
use crate::store::{
    IdentityChangeRecord, IdentityKind, SqlSenderKeyStore, Stores, is_store_failure,
    load_identity_key, ready,
};

/// Most envelopes per stage-1 zone (Desktop's `decryptAndCacheBatcher`).
pub(crate) const MAX_BATCH: usize = 30;

/// What stage 1 needs besides the database.
pub(crate) struct Stage1<'a> {
    /// Our account.
    pub(crate) account: &'a Account,
    /// Sealed-sender trust roots of the environment.
    pub(crate) trust_roots: &'a [PublicKey],
    /// Wall clock (ms) for `received_at`.
    pub(crate) now: i64,
    /// Wakes the forwarder after a commit that wrote core events.
    pub(crate) wake: &'a Rc<Notify>,
}

/// The fate of one envelope inside the zone.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Disposition {
    /// Stored in `inbox` (ack 200 after commit).
    Stored,
    /// Handled in full inside the zone (a number change): its writes stay, nothing goes to the
    /// inbox (ack 200 after commit).
    Consumed,
    /// Nothing to store (duplicate, not for us, unsupported): ack 200, never redeliver.
    Dropped(&'static str),
    /// It did not decrypt: the decrypt's writes are rolled back and a retry row is stored
    /// instead (ack 200).
    Undecryptable(Box<FailedRow>),
    /// The envelope does not decode: ack 500 (Desktop's `handleRequest`).
    Rejected,
}

/// What one envelope taught us besides its content: core events for `core_outbox`, written in
/// the zone that stores the envelope.
#[derive(Default)]
pub(super) struct Facts {
    /// Remote identity keys that `save_identity` replaced.
    pub(super) identity_changes: Vec<IdentityChangeRecord>,
    /// Other events (PNI merges, numbers, our own ids after a number change).
    pub(super) events: Vec<BackendEvent>,
    /// The account after a number change this envelope applied.
    pub(super) account: Option<Account>,
}

impl Facts {
    /// The core events, identity changes first (their notices are looked up on `conn`).
    pub(super) fn into_events(self, conn: &Connection, now: i64) -> Result<Vec<BackendEvent>> {
        let mut events = identity_events(conn, &self.identity_changes, now)?;
        events.extend(self.events);
        Ok(events)
    }
}

/// The server acks to send after the commit, in envelope order.
pub(crate) type Acks = Vec<(ServerAck, StatusCode)>;

/// What a committed stage-1 batch left behind.
pub(crate) struct Committed {
    /// The acks to send now.
    pub(crate) acks: Acks,
    /// The account as it is after a number change in this batch (§6.6): the caller swaps it in
    /// for every later batch and send.
    pub(crate) account: Option<Account>,
}

/// Decrypts and stores `envelopes` in one zone. On `Err` nothing was committed and the acks are
/// dropped, so the server redelivers everything on the next connection.
pub(crate) fn commit_batch(
    db: &ZoneDb,
    ctx: &Stage1<'_>,
    envelopes: Vec<IncomingEnvelope>,
) -> Result<Committed> {
    db.zone(|tx| {
        let mut counter = db::arrival_counter(tx)?;
        let mut acks = Vec::with_capacity(envelopes.len());
        let mut rows = Vec::new();
        // Set once a number change was applied: the envelopes after it are for the new PNI.
        let mut changed: Option<Account> = None;
        for envelope in envelopes {
            let IncomingEnvelope {
                bytes,
                server_delivery_timestamp,
                ack,
            } = envelope;
            // The server's clock when it delivered the envelope (0: it stated none): kept with
            // the row, because the age of a call's ring is measured on that clock (stage 2,
            // `super::calls`).
            let delivered_at = i64::try_from(server_delivery_timestamp)
                .ok()
                .filter(|t| *t > 0);
            tx.execute_batch("SAVEPOINT envelope")?;
            let mut facts = Facts::default();
            let current = Stage1 {
                account: changed.as_ref().unwrap_or(ctx.account),
                trust_roots: ctx.trust_roots,
                now: ctx.now,
                wake: ctx.wake,
            };
            let disposition =
                process(tx, &current, &bytes, delivered_at, &mut counter, &mut facts)?;
            let status = match disposition {
                Disposition::Stored => {
                    tx.execute_batch("RELEASE envelope")?;
                    rows.push(facts.into_events(tx, ctx.now)?);
                    StatusCode::OK
                }
                Disposition::Consumed => {
                    tx.execute_batch("RELEASE envelope")?;
                    if let Some(account) = facts.account.take() {
                        tracing::info!(account = ?account, "number change applied");
                        changed = Some(account);
                    }
                    rows.push(facts.into_events(tx, ctx.now)?);
                    StatusCode::OK
                }
                Disposition::Dropped(reason) => {
                    // A failed decrypt may have written part of its state (an identity save):
                    // drop it with the envelope.
                    tx.execute_batch("ROLLBACK TO envelope; RELEASE envelope")?;
                    tracing::debug!(reason, "envelope dropped");
                    StatusCode::OK
                }
                Disposition::Undecryptable(failed) => {
                    tx.execute_batch("ROLLBACK TO envelope")?;
                    failed.insert(tx, &mut counter)?;
                    tx.execute_batch("RELEASE envelope")?;
                    tracing::warn!(
                        reason = failed.reason,
                        "envelope did not decrypt; retry request queued"
                    );
                    StatusCode::OK
                }
                Disposition::Rejected => {
                    tx.execute_batch("ROLLBACK TO envelope; RELEASE envelope")?;
                    tracing::warn!("undecodable envelope rejected");
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            };
            acks.push((ack, status));
        }
        db::set_arrival_counter(tx, counter)?;
        append_event_rows(tx, &rows, ctx.now, ctx.wake)?;
        Ok(Committed {
            acks,
            account: changed,
        })
    })
}

/// Core events for remote identity keys that changed (§6.6; Desktop's `keychange` event and
/// `ConversationModel.addKeyChange`): the key was replaced by `save_identity` (the sibling
/// sessions archived by the store in the same zone), so the contact gets its new fingerprint
/// and is unverified again, and every conversation the core has with them shows a notice
/// (`SystemEvent::IdentityKeyChanged`): the 1:1 chat and each group they are a member of.
///
/// The notices are written by the backend, so they need ids of their own (§13 phase B): the
/// author is the contact, `local` is `keychange:<first 8 bytes of SHA-256(new key), hex>` for
/// the 1:1 chat and that plus `:<first 8 bytes of the group id, hex>` in a group: unique per
/// key and conversation, and the same when the change is reported twice (decrypt and prekey
/// fetch). The send path uses this for keys first seen in a prekey fetch, the profile fetch for
/// keys a profile states.
pub(crate) fn identity_events(
    conn: &Connection,
    changes: &[IdentityChangeRecord],
    now: i64,
) -> Result<Vec<BackendEvent>> {
    use sha2::{Digest as _, Sha256};
    let mut events = Vec::new();
    for change in changes {
        let Some(service_id) = ServiceId::parse_from_service_id_string(&change.service_id) else {
            continue;
        };
        let Ok(user) = service_id_user_id(service_id) else {
            continue;
        };
        let mut patch = ContactPatch::new(user.clone());
        patch.trust = Some(TrustLevel::Unverified);
        patch.identity_fingerprint =
            Patch::Set(IdentityFingerprint(change.identity.serialize().into_vec()));
        events.push(BackendEvent::ContactUpsert(patch));

        let digest = Sha256::digest(change.identity.serialize());
        let key_id = hex8(&digest[..8]);
        let notice = |conv: ConvKey, local: String| {
            let mut message = IncomingMessage::new(
                conv,
                local,
                Origin::Remote {
                    author: user.clone(),
                    device: None,
                },
                Body::System(SystemEvent::IdentityKeyChanged { user: user.clone() }),
                now,
                now,
            );
            message.silent = true;
            BackendEvent::Message(message)
        };
        // Only where the core has the conversation: a notice must not open a chat.
        if crate::peers::load(conn, &change.service_id)?
            .features_hash
            .is_some()
        {
            events.push(notice(
                ConvKey::Direct(user.clone()),
                format!("keychange:{key_id}"),
            ));
        }
        if let ServiceId::Aci(aci) = service_id {
            for group in crate::groups::in_core(conn)? {
                if group.snapshot.as_ref().is_some_and(|s| s.is_member(aci)) {
                    events.push(notice(
                        group.keys.conv_key(),
                        format!("keychange:{key_id}:{}", hex8(&group.keys.id[..8])),
                    ));
                }
            }
        }
    }
    Ok(events)
}

/// Lower-case hex of `bytes`.
fn hex8(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Who sent an envelope, once known (from the envelope, or the sealed-sender certificate).
pub(super) struct Sender {
    pub(super) service_id: ServiceId,
    pub(super) device: DeviceId,
}

/// One envelope, delivered by the server at `delivered_at` (its clock, ms; `None` when it
/// stated none). `Err` only for database failures (they abort the whole batch).
pub(super) fn process(
    tx: &Tx<'_>,
    ctx: &Stage1<'_>,
    bytes: &[u8],
    delivered_at: Option<i64>,
    counter: &mut u64,
    facts: &mut Facts,
) -> Result<Disposition> {
    let Ok(envelope) = proto::Envelope::decode(bytes) else {
        return Ok(Disposition::Rejected);
    };
    let account = ctx.account;
    let kind = envelope.r#type();
    let timestamp = envelope.client_timestamp.unwrap_or(0);
    let server_timestamp = envelope.server_timestamp.unwrap_or(0);

    // Destination: binary first, string fallback, our ACI by default.
    let destination = match service_id_from_wire(
        envelope.destination_service_id_binary.as_deref(),
        envelope.destination_service_id.as_deref(),
    ) {
        Ok(Some(destination)) => destination,
        Ok(None) => account.aci.into(),
        Err(_) => return Ok(Disposition::Rejected),
    };
    let Some(our_kind) = account.kind_of(destination) else {
        return Ok(Disposition::Dropped("not addressed to this account"));
    };

    // PNI rules (§6.6, `MessageReceiver.preload.ts:1345-1366, 1866-1871`).
    let sealed = kind == EnvelopeType::UnidentifiedSender;
    if our_kind == IdentityKind::Pni {
        if sealed {
            return Ok(Disposition::Dropped("sealed sender to our PNI"));
        }
        if !matches!(
            kind,
            EnvelopeType::PrekeyMessage | EnvelopeType::ServerDeliveryReceipt
        ) {
            return Ok(Disposition::Dropped("only prekey messages on our PNI"));
        }
    }
    let unsealed_sender = if sealed {
        None
    } else {
        let source = match service_id_from_wire(
            envelope.source_service_id_binary.as_deref(),
            envelope.source_service_id.as_deref(),
        ) {
            Ok(Some(source)) => source,
            Ok(None) => return Ok(Disposition::Dropped("no source")),
            Err(_) => return Ok(Disposition::Rejected),
        };
        if matches!(source, ServiceId::Pni(_)) && kind != EnvelopeType::ServerDeliveryReceipt {
            return Ok(Disposition::Dropped("message from a PNI"));
        }
        let Some(device) = envelope
            .source_device_id
            .and_then(|d| DeviceId::try_from(d).ok())
        else {
            return Ok(Disposition::Dropped("no source device"));
        };
        Some(Sender {
            service_id: source,
            device,
        })
    };

    let content = envelope.content.as_deref().unwrap_or_default();
    let stores = Stores::new(
        tx,
        our_kind,
        *account.identity(our_kind),
        account.registration_id(our_kind),
        ctx.now,
    );
    let row = InboxRow {
        our_kind,
        timestamp,
        server_timestamp,
        server_guid: envelope.server_guid.clone(),
        sealed,
        urgent: envelope.urgent.unwrap_or(true),
        received_at: ctx.now,
        delivered_at,
    };

    let changes = &mut facts.identity_changes;
    let unsealed_meta = |message_type| Original {
        bytes: content.to_vec(),
        message_type,
        hint: ContentHint::Default,
        group_id: None,
    };
    // `encrypted`: the payload came through a session (not `PlaintextContent`), so it may
    // carry facts about the sender (Desktop's `wasEncrypted`).
    let (sender, plaintext, sender_e164, encrypted, original) = match (kind, unsealed_sender) {
        (EnvelopeType::ServerDeliveryReceipt, Some(sender)) => {
            row.insert(tx, counter, KIND_DELIVERY_RECEIPT, MISC_LANE, &sender, None)?;
            return Ok(Disposition::Stored);
        }
        (EnvelopeType::DoubleRatchet, Some(sender)) => {
            let Ok(message) = SignalMessage::try_from(content) else {
                return Ok(Disposition::Dropped("malformed signal message"));
            };
            let decrypted = decrypt_signal(stores, account, our_kind, &sender, &message, changes)?;
            let original = decrypted
                .is_err()
                .then(|| unsealed_meta(CiphertextMessageType::Whisper));
            (sender, decrypted, None, true, original)
        }
        (EnvelopeType::PrekeyMessage, Some(sender)) => {
            let Ok(message) = PreKeySignalMessage::try_from(content) else {
                return Ok(Disposition::Dropped("malformed prekey message"));
            };
            let decrypted = decrypt_prekey(stores, account, our_kind, &sender, &message, changes)?;
            let original = decrypted
                .is_err()
                .then(|| unsealed_meta(CiphertextMessageType::PreKey));
            (sender, decrypted, None, true, original)
        }
        (EnvelopeType::PlaintextContent, Some(sender)) => {
            // Only decryption-error messages travel as plaintext (checked below).
            let Ok(plaintext) = PlaintextContent::try_from(content) else {
                return Ok(Disposition::Dropped("malformed plaintext content"));
            };
            (sender, Ok(plaintext.body().to_vec()), None, false, None)
        }
        (EnvelopeType::UnidentifiedSender, None) => {
            match unseal(stores, tx, ctx, server_timestamp, content, changes)? {
                Ok(unsealed) => (
                    unsealed.sender,
                    unsealed.decrypted,
                    unsealed.e164,
                    unsealed.encrypted,
                    unsealed.original,
                ),
                Err(reason) => return Ok(Disposition::Dropped(reason)),
            }
        }
        _ => return Ok(Disposition::Dropped("unsupported envelope type")),
    };
    let plaintext = match plaintext {
        Ok(plaintext) => plaintext,
        Err(Failure::Drop(reason)) => return Ok(Disposition::Dropped(reason)),
        Err(Failure::Undecryptable(reason)) => {
            let Some(original) = original else {
                return Ok(Disposition::Dropped(reason));
            };
            return failed_row(tx, &row, &sender, original, reason);
        }
    };
    let Ok(content) = unpad(&plaintext) else {
        return Ok(Disposition::Dropped("invalid content padding"));
    };
    // The plaintext is stored even when it does not decode: the ratchet already moved, so the
    // envelope cannot be redelivered; stage 2 counts it as a content failure (poison rule §5.6).
    let decoded = proto::Content::decode(content).ok();
    if !encrypted && !is_retry_request_only(decoded.as_ref()) {
        // Desktop accepts nothing but a decryption-error message without encryption.
        return Ok(Disposition::Dropped(
            "plaintext content that is not a retry request",
        ));
    }
    if let Some(change) = decoded
        .as_ref()
        .filter(|_| encrypted)
        .and_then(pni_change_number)
    {
        return number_change(tx, ctx, &envelope, &sender, our_kind, change, facts);
    }
    let lane = match typing_group(decoded.as_ref()) {
        Some(group_id) => crate::groups::master_of(tx, group_id)?
            .map_or_else(|| MISC_LANE.to_owned(), |master| group_lane(&master)),
        None => lane_of(&sender, decoded.as_ref()),
    };
    if let Some(decoded) = decoded.as_ref().filter(|_| encrypted) {
        pni_signature(tx, account, &sender, decoded, ctx.now, facts)?;
        if let Some(skdm) = decoded.sender_key_distribution_message.as_deref() {
            sender_key_distribution(tx, &sender, skdm, ctx.now)?;
        }
    }
    if let (ServiceId::Aci(aci), Some(e164)) = (sender.service_id, sender_e164.as_deref())
        && aci != account.aci
    {
        sender_number(tx, account, aci, e164, ctx.now, facts)?;
    }
    if our_kind == IdentityKind::Pni {
        // They know our number (they wrote to our PNI): replies may share it (§6.7).
        share_my_number(tx, &sender, ctx.now)?;
    }
    row.insert(tx, counter, KIND_CONTENT, &lane, &sender, Some(content))?;
    Ok(Disposition::Stored)
}

/// The `PniChangeNumber` of a sync message, if it is one.
fn pni_change_number(content: &proto::Content) -> Option<&proto::sync_message::PniChangeNumber> {
    match content.content.as_ref()? {
        ContentKind::SyncMessage(sync) => match sync.content.as_ref()? {
            proto::sync_message::Content::PniChangeNumber(change) => Some(change),
            _ => None,
        },
        _ => None,
    }
}

/// Our phone changed its number: applies it in this envelope's savepoint (see
/// [`crate::number_change`]). Only our own account can say so, on our ACI; a message that is
/// not whole changes nothing and is dropped (its session step with it), as Desktop ignores it.
fn number_change(
    tx: &Tx<'_>,
    ctx: &Stage1<'_>,
    envelope: &proto::Envelope,
    sender: &Sender,
    our_kind: IdentityKind,
    change: &proto::sync_message::PniChangeNumber,
    facts: &mut Facts,
) -> Result<Disposition> {
    if sender.service_id != ServiceId::from(ctx.account.aci) || our_kind != IdentityKind::Aci {
        return Ok(Disposition::Dropped("number change from another account"));
    }
    let change =
        match crate::number_change::validate(crate::number_change::updated_pni(envelope), change) {
            Ok(change) => change,
            Err(reason) => return Ok(Disposition::Dropped(reason)),
        };
    let (account, ready) = crate::number_change::apply(tx, ctx.account, &change, ctx.now)?;
    facts.events.push(ready);
    facts.account = Some(account);
    Ok(Disposition::Consumed)
}

/// The ciphertext of a message that did not decrypt, for its `DecryptionErrorMessage`.
pub(super) struct Original {
    /// The (inner) ciphertext.
    bytes: Vec<u8>,
    /// Its type.
    message_type: CiphertextMessageType,
    /// The sealed envelope's content hint (`Default` for unsealed ones).
    hint: ContentHint,
    /// The sealed envelope's group id.
    group_id: Option<Vec<u8>>,
}

/// A row for a message that did not decrypt (§6.6 `retry.rs`): no content, the serialized
/// `DecryptionErrorMessage` for the sender's device, the content hint and the group id.
#[derive(PartialEq, Eq)]
pub(crate) struct FailedRow {
    row: InboxRow,
    lane: String,
    sender: String,
    device: u32,
    dem: Vec<u8>,
    hint: ContentHint,
    group_id: Option<Vec<u8>>,
    /// Why it failed (log-safe).
    reason: &'static str,
}

/// Log-safe: the reason and the lane kind only.
impl std::fmt::Debug for FailedRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailedRow")
            .field("reason", &self.reason)
            .field("hint", &self.hint)
            .field("group", &self.group_id.is_some())
            .finish_non_exhaustive()
    }
}

impl FailedRow {
    fn insert(&self, conn: &Connection, counter: &mut u64) -> Result<()> {
        *counter += 1;
        conn.prepare_cached(
            "INSERT INTO inbox(arrival, received_at, kind, lane, our_kind, sender, sender_device,
               timestamp, server_timestamp, server_guid, sealed, urgent, content, decrypt_error,
               content_hint, group_id, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13, ?14, ?15, ?16)",
        )?
        .execute(kourier_db::rusqlite::params![
            i64::try_from(*counter).unwrap_or(i64::MAX),
            self.row.received_at,
            KIND_CONTENT,
            self.lane,
            self.row.our_kind.as_str(),
            self.sender,
            self.device,
            i64::try_from(self.row.timestamp).unwrap_or(i64::MAX),
            i64::try_from(self.row.server_timestamp)
                .ok()
                .filter(|t| *t > 0),
            self.row.server_guid,
            self.row.sealed,
            self.row.urgent,
            self.dem,
            u32::from(self.hint),
            self.group_id,
            self.row.delivered_at,
        ])?;
        Ok(())
    }
}

/// The retry row of a message from `sender` that did not decrypt, in the group's lane when the
/// sealed envelope named a group we know, else the sender's.
fn failed_row(
    tx: &Tx<'_>,
    row: &InboxRow,
    sender: &Sender,
    original: Original,
    reason: &'static str,
) -> Result<Disposition> {
    let Ok(dem) = DecryptionErrorMessage::for_original(
        &original.bytes,
        original.message_type,
        Timestamp::from_epoch_millis(row.timestamp),
        u32::from(sender.device),
    ) else {
        return Ok(Disposition::Dropped(reason));
    };
    let lane = match original.group_id.as_deref() {
        Some(id) => crate::groups::master_of(tx, id)?
            .map(|master| group_lane(&master))
            .unwrap_or_else(|| sender.service_id.service_id_string()),
        None => sender.service_id.service_id_string(),
    };
    Ok(Disposition::Undecryptable(Box::new(FailedRow {
        row: row.clone(),
        lane,
        sender: sender.service_id.service_id_string(),
        device: u32::from(sender.device),
        dem: dem.serialized().to_vec(),
        hint: original.hint,
        group_id: original.group_id,
        reason,
    })))
}

/// Whether a plaintext `Content` is a decryption-error message and nothing else.
fn is_retry_request_only(content: Option<&proto::Content>) -> bool {
    content.is_some_and(|c| {
        matches!(c.content, Some(ContentKind::DecryptionErrorMessage(_)))
            && c.sender_key_distribution_message.is_none()
            && c.pni_signature_message.is_none()
    })
}

/// The group id of a group typing message (its lane is the group's, found by id).
fn typing_group(content: Option<&proto::Content>) -> Option<&[u8]> {
    match content?.content.as_ref()? {
        ContentKind::TypingMessage(typing) => typing.group_id.as_deref(),
        _ => None,
    }
}

/// `Content.senderKeyDistributionMessage` (`#handleSenderKeyDistributionMessage`): the sender's
/// chain for a distribution id, stored in this zone so the group messages after it decrypt. A
/// malformed one is ignored, as in Desktop.
fn sender_key_distribution(
    conn: &Connection,
    sender: &Sender,
    bytes: &[u8],
    now: i64,
) -> Result<()> {
    let Ok(message) = SenderKeyDistributionMessage::try_from(bytes) else {
        tracing::debug!("malformed sender key distribution message ignored");
        return Ok(());
    };
    let address = ProtocolAddress::new(sender.service_id.service_id_string(), sender.device);
    let mut store = SqlSenderKeyStore::new(conn, now);
    match ready(process_sender_key_distribution_message(
        &address, &message, &mut store,
    ))? {
        Ok(()) => {}
        Err(e) if is_store_failure(&e) => return Err(e.into()),
        Err(e) => tracing::debug!(
            error = protocol_error_kind(&e),
            "sender key distribution message ignored"
        ),
    }
    Ok(())
}

/// The stage-2 lane of a row (§6.6): the group for group content, the peer for 1:1 content and
/// for sync transcripts of a 1:1 message to that peer, [`MISC_LANE`] for content without a
/// conversation (receipts, other sync messages, null and call messages). Content that does not
/// decode stays in the sender's lane, where stage 2 counts it as a failure.
pub(super) fn lane_of(sender: &Sender, content: Option<&proto::Content>) -> String {
    let peer = || sender.service_id.service_id_string();
    let data_lane = |message: &proto::DataMessage| {
        message
            .group_v2
            .as_ref()
            .and_then(|group| group.master_key.as_deref())
            .map(group_lane)
    };
    let Some(content) = content else {
        return peer();
    };
    match &content.content {
        Some(ContentKind::DataMessage(message)) => data_lane(message).unwrap_or_else(peer),
        Some(ContentKind::EditMessage(edit)) => edit
            .data_message
            .as_ref()
            .and_then(data_lane)
            .unwrap_or_else(peer),
        Some(ContentKind::TypingMessage(typing)) if typing.group_id.is_none() => peer(),
        Some(ContentKind::SyncMessage(sync)) => match &sync.content {
            Some(proto::sync_message::Content::Sent(sent)) => {
                let group = sent
                    .message
                    .as_ref()
                    .or(sent
                        .edit_message
                        .as_ref()
                        .and_then(|e| e.data_message.as_ref()))
                    .and_then(data_lane);
                let destination = service_id_from_wire(
                    sent.destination_service_id_binary.as_deref(),
                    sent.destination_service_id.as_deref(),
                )
                .ok()
                .flatten()
                .map(|d| d.service_id_string());
                group
                    .or(destination)
                    .unwrap_or_else(|| MISC_LANE.to_owned())
            }
            _ => MISC_LANE.to_owned(),
        },
        _ => MISC_LANE.to_owned(),
    }
}

/// `Content.pniSignatureMessage` (`MessageReceiver.preload.ts` `#handlePniSignatureMessage`,
/// `SignalProtocolStore.verifyAlternateIdentity`): the PNI identity key signed the sender's ACI
/// identity key, so the PNI belongs to that ACI → `IdentityMerged{pni → aci}`. Both keys must
/// be stored already (the ACI's is saved by the decrypt in this zone); otherwise nothing merges.
fn pni_signature(
    conn: &Connection,
    account: &Account,
    sender: &Sender,
    content: &proto::Content,
    now: i64,
    facts: &mut Facts,
) -> Result<()> {
    let Some(signature) = &content.pni_signature_message else {
        return Ok(());
    };
    let ServiceId::Aci(aci) = sender.service_id else {
        return Ok(());
    };
    let (Some(pni), Some(signature)) = (
        signature
            .pni
            .as_deref()
            .and_then(|bytes| Uuid::from_slice(bytes).ok())
            .map(Pni::from),
        signature.signature.as_deref(),
    ) else {
        tracing::debug!("malformed PNI signature message ignored");
        return Ok(());
    };
    let aci_key = load_identity_key(conn, &ServiceId::from(aci).service_id_string())?;
    let pni_key = load_identity_key(conn, &ServiceId::from(pni).service_id_string())?;
    let (Some(aci_key), Some(pni_key)) = (aci_key, pni_key) else {
        tracing::debug!(sender = ?SignalUser::Aci(aci), "PNI signature without both identity keys");
        return Ok(());
    };
    if !pni_key
        .verify_alternate_identity(&aci_key, signature)
        .unwrap_or(false)
    {
        tracing::warn!(sender = ?SignalUser::Aci(aci), "PNI signature did not verify");
        return Ok(());
    }
    // Told once: a contact repeats the signature with every message until our receipt arrives.
    if let Some(event) = crate::links::learn(conn, account, pni, aci, now)? {
        facts.events.push(event);
    }
    Ok(())
}

/// The number in a sealed sender's certificate (`background.preload.ts:2594-2622`, the contact
/// merge on `EnvelopeUnsealedEvent`): stored in `peers` and, when it is new or changed, sent to
/// the core as the contact's phone identifier (titles before the contact sync of S9).
fn sender_number(
    conn: &Connection,
    account: &Account,
    aci: Aci,
    e164: &str,
    now: i64,
    facts: &mut Facts,
) -> Result<()> {
    if validate_e164(e164).is_err() {
        return Ok(());
    }
    // A contact our phone knows by this number only (a PNI): the certificate says which ACI
    // has the number now, so the two are one contact (`links`).
    if let Some(event) = crate::links::number_of_aci(conn, account, aci, e164, now)? {
        facts.events.push(event);
    }
    let service_id = ServiceId::from(aci).service_id_string();
    let known: Option<Option<String>> = conn
        .prepare_cached("SELECT e164 FROM peers WHERE service_id = ?1")?
        .query_row([&service_id], |r| r.get(0))
        .optional()?;
    if known.flatten().as_deref() == Some(e164) {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO peers(service_id, e164, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(service_id) DO UPDATE SET e164 = excluded.e164,
           updated_at = excluded.updated_at",
    )?
    .execute((&service_id, e164, now))?;
    let Ok(user) = aci_user_id(aci) else {
        return Ok(());
    };
    let mut patch = ContactPatch::new(user);
    patch.identifiers = Some(vec![Identifier {
        kind: IdentifierKind::Phone,
        value: e164.to_owned(),
    }]);
    facts.events.push(BackendEvent::ContactUpsert(patch));
    Ok(())
}

/// A message addressed to our PNI: its sender knows our number (`peers.share_my_number`).
fn share_my_number(conn: &Connection, sender: &Sender, now: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO peers(service_id, share_my_number, updated_at) VALUES (?1, 1, ?2)
         ON CONFLICT(service_id) DO UPDATE SET share_my_number = 1,
           updated_at = excluded.updated_at",
    )?
    .execute((sender.service_id.service_id_string(), now))?;
    Ok(())
}

/// The columns of an inbox row known before decryption.
#[derive(Clone, PartialEq, Eq)]
struct InboxRow {
    our_kind: IdentityKind,
    timestamp: u64,
    server_timestamp: u64,
    server_guid: Option<String>,
    sealed: bool,
    urgent: bool,
    received_at: i64,
    /// The server's delivery time (`x-signal-timestamp` of the request), when it stated one.
    delivered_at: Option<i64>,
}

impl InboxRow {
    fn insert(
        &self,
        conn: &Connection,
        counter: &mut u64,
        kind: &str,
        lane: &str,
        sender: &Sender,
        content: Option<&[u8]>,
    ) -> Result<()> {
        *counter += 1;
        conn.prepare_cached(
            "INSERT INTO inbox(arrival, received_at, kind, lane, our_kind, sender, sender_device,
               timestamp, server_timestamp, server_guid, sealed, urgent, content, delivered_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        )?
        .execute(kourier_db::rusqlite::params![
            i64::try_from(*counter).unwrap_or(i64::MAX),
            self.received_at,
            kind,
            lane,
            self.our_kind.as_str(),
            sender.service_id.service_id_string(),
            u32::from(sender.device),
            i64::try_from(self.timestamp).unwrap_or(i64::MAX),
            i64::try_from(self.server_timestamp).ok().filter(|t| *t > 0),
            self.server_guid,
            self.sealed,
            self.urgent,
            content,
            self.delivered_at,
        ])?;
        Ok(())
    }
}

/// Why a payload has no plaintext (log-safe reasons).
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// Nothing to tell the sender (a duplicate, a malformed or misaddressed message).
    Drop(&'static str),
    /// It did not decrypt: the sender gets a retry request.
    Undecryptable(&'static str),
}

/// A decrypted payload, or why there is none.
type Decrypted = std::result::Result<Vec<u8>, Failure>;

fn addresses(
    account: &Account,
    our_kind: IdentityKind,
    sender: &Sender,
) -> (ProtocolAddress, ProtocolAddress) {
    (
        ProtocolAddress::new(sender.service_id.service_id_string(), sender.device),
        account.address(our_kind),
    )
}

fn decrypt_signal(
    mut stores: Stores<'_>,
    account: &Account,
    our_kind: IdentityKind,
    sender: &Sender,
    message: &SignalMessage,
    changes: &mut Vec<IdentityChangeRecord>,
) -> Result<Decrypted> {
    let (remote, local) = addresses(account, our_kind, sender);
    let result = ready(message_decrypt_signal(
        message,
        &remote,
        &local,
        &mut stores.session,
        &mut stores.identity,
        &mut rand::rng(),
    ))?;
    changes.extend(stores.identity.take_changes());
    decrypt_outcome(result)
}

fn decrypt_prekey(
    mut stores: Stores<'_>,
    account: &Account,
    our_kind: IdentityKind,
    sender: &Sender,
    message: &PreKeySignalMessage,
    changes: &mut Vec<IdentityChangeRecord>,
) -> Result<Decrypted> {
    let (remote, local) = addresses(account, our_kind, sender);
    let result = ready(message_decrypt_prekey(
        message,
        &remote,
        &local,
        &mut stores.session,
        &mut stores.identity,
        &mut stores.prekey,
        &stores.signed,
        &mut stores.kyber,
        &mut rand::rng(),
    ))?;
    changes.extend(stores.identity.take_changes());
    decrypt_outcome(result)
}

/// Maps a libsignal decrypt result: duplicates are dropped silently, other failures are logged
/// by kind and become retry requests (`crate::retry`). A failure of our own store is a
/// database error: it aborts the batch, so nothing is acked.
fn decrypt_outcome(result: std::result::Result<Vec<u8>, SignalProtocolError>) -> Result<Decrypted> {
    match result {
        Ok(plaintext) => Ok(Ok(plaintext)),
        Err(e) if is_store_failure(&e) => Err(e.into()),
        Err(SignalProtocolError::DuplicatedMessage(..)) => Ok(Err(Failure::Drop("duplicate"))),
        Err(e) => {
            tracing::warn!(error = protocol_error_kind(&e), "decryption failed");
            Ok(Err(Failure::Undecryptable("decryption failed")))
        }
    }
}

/// An unsealed sealed-sender envelope.
struct Unsealed {
    /// The sender the certificate names.
    sender: Sender,
    /// The inner payload.
    decrypted: Decrypted,
    /// The sender's number, when the certificate carries one (personal data).
    e164: Option<String>,
    /// The inner payload came through a session (not `PlaintextContent`).
    encrypted: bool,
    /// The inner ciphertext, when it did not decrypt.
    original: Option<Original>,
}

/// Sealed sender (§6.6, `#unsealEnvelope`, `#validateUnsealedEnvelope`, `#decryptSealedSender`):
/// unwrap to the USMC with our ACI identity, validate the sender certificate against the trust
/// roots at the server timestamp, refuse our own ACI and device, then decrypt the inner message.
fn unseal(
    mut stores: Stores<'_>,
    conn: &Connection,
    ctx: &Stage1<'_>,
    server_timestamp: u64,
    content: &[u8],
    changes: &mut Vec<IdentityChangeRecord>,
) -> Result<std::result::Result<Unsealed, &'static str>> {
    if server_timestamp == 0 {
        return Ok(Err("sealed sender without server timestamp"));
    }
    let usmc = match ready(sealed_sender_decrypt_to_usmc(content, &stores.identity))? {
        Ok(usmc) => usmc,
        Err(e) if is_store_failure(&e) => return Err(e.into()),
        Err(e) => {
            tracing::warn!(
                error = protocol_error_kind(&e),
                "sealed sender unwrap failed"
            );
            return Ok(Err("sealed sender unwrap failed"));
        }
    };
    let Ok(certificate) = usmc.sender() else {
        return Ok(Err("sealed sender without certificate"));
    };
    let valid = certificate
        .validate_with_trust_roots(
            ctx.trust_roots,
            Timestamp::from_epoch_millis(server_timestamp),
        )
        .unwrap_or(false);
    if !valid {
        return Ok(Err("invalid sender certificate"));
    }
    let (Ok(uuid), Ok(device)) = (certificate.sender_uuid(), certificate.sender_device_id()) else {
        return Ok(Err("sender certificate without sender"));
    };
    let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(uuid) else {
        return Ok(Err("sender certificate names no ACI"));
    };
    let account = ctx.account;
    if aci == account.aci && device == account.device_id {
        return Ok(Err("sealed sender from this device"));
    }
    let e164 = certificate.sender_e164().ok().flatten().map(str::to_owned);
    tracing::trace!(sender = ?SignalUser::Aci(aci), "sealed sender unwrapped");
    let sender = Sender {
        service_id: aci.into(),
        device,
    };
    let (Ok(message_type), Ok(contents)) = (usmc.msg_type(), usmc.contents()) else {
        return Ok(Err("malformed sealed sender content"));
    };
    let hint = usmc.content_hint().unwrap_or(ContentHint::Default);
    let group_id = usmc.group_id().ok().flatten().map(<[u8]>::to_vec);
    let decrypted = match message_type {
        CiphertextMessageType::PreKey => match PreKeySignalMessage::try_from(contents) {
            Ok(message) => {
                let (remote, local) = addresses(account, IdentityKind::Aci, &sender);
                let result = ready(message_decrypt_prekey(
                    &message,
                    &remote,
                    &local,
                    &mut stores.session,
                    &mut stores.identity,
                    &mut stores.prekey,
                    &stores.signed,
                    &mut stores.kyber,
                    &mut rand::rng(),
                ))?;
                decrypt_outcome(result)?
            }
            Err(_) => Err(Failure::Drop("malformed prekey message")),
        },
        CiphertextMessageType::Whisper => match SignalMessage::try_from(contents) {
            Ok(message) => {
                let (remote, local) = addresses(account, IdentityKind::Aci, &sender);
                let result = ready(message_decrypt_signal(
                    &message,
                    &remote,
                    &local,
                    &mut stores.session,
                    &mut stores.identity,
                    &mut rand::rng(),
                ))?;
                decrypt_outcome(result)?
            }
            Err(_) => Err(Failure::Drop("malformed signal message")),
        },
        CiphertextMessageType::Plaintext => PlaintextContent::try_from(contents)
            .map(|plaintext| plaintext.body().to_vec())
            .map_err(|_| Failure::Drop("malformed plaintext content")),
        CiphertextMessageType::SenderKey => {
            let address =
                ProtocolAddress::new(sender.service_id.service_id_string(), sender.device);
            let mut keys = SqlSenderKeyStore::new(conn, ctx.now);
            let result = ready(group_decrypt(contents, &mut keys, &address))?;
            decrypt_outcome(result)?
        }
    };
    changes.extend(stores.identity.take_changes());
    let original = matches!(decrypted, Err(Failure::Undecryptable(_))).then(|| Original {
        bytes: contents.to_vec(),
        message_type,
        hint,
        group_id,
    });
    Ok(Ok(Unsealed {
        sender,
        decrypted,
        e164,
        encrypted: message_type != CiphertextMessageType::Plaintext,
        original,
    }))
}
