//! THE POINT OF NO RETURN (TH6a S4): one message becomes the rows of ZONE PREPARE. The receivers' fates are worked out
//! ([`super::receivers`]), a fresh nonce is drawn for each box, every box and the reflect are sealed ([`super::encode`])
//! and the hashes of their nonces go into the same rows, so a nonce is durable before its first use. After this commit a
//! resend writes the stored bytes as they are; nothing here runs again for that row. NEVER ENCODE AGAIN UNDER A STORED
//! NONCE (`encode.rs`).
//!
//! A DRAWN NONCE IS CHECKED AGAINST THE STORE in the zone, and against those of the same message. A hit means a new
//! nonce, or for an envelope a new seal with fresh padding, which is safe for a nonce never used: `add_nonces` is
//! `INSERT OR IGNORE` and would hide the reuse. Eight hits in a row are no chance: the zone fails.
//!
//! REFUSED BEFORE ANYTHING IS WRITTEN ([`SendRefused`]): a `local` that does not parse, a contact or a group the file
//! does not hold, a `Transmit` into a group the user is not in, a type no message has, a type the chat server never acks
//! (flag `0x04`, a typing indicator: volatile, no place in a persistent queue, `NotQueued`), a receiver that is no ID,
//! a box over 8,170 bytes, an envelope over 65,516. The refusal fails the zone, so nothing of it stays. A receiver that gets no
//! box for a reason of its own is no refusal: its row says why and the rest goes (`Blocked`, `Invalid`, `NoKey`), and
//! `OutgoingMessage.nonces` lists the receivers that get a box and nobody else. The size depends on random padding, so
//! the last 300 bytes under a limit are a matter of chance: a later step's `max_text_bytes` stays below them.
//!
//! Plaintext is held in `Zeroizing` where this module owns it (the canonical body, the unpadded container, the metadata,
//! the envelope's bytes); nothing here logs a body, an ID, a key or a nonce: rule names and counts. NOT WIPED, and plain
//! in freed heap after the call (S-L4, limit (4) of THE SEND CORE; no disk or log, and the core holds the text too):
//! the request's own text (`NewBody::Text`, `NewBody::Raw { body }`) and each `Specific.body`, which the caller owns;
//! the prost `Content` that `encode::outgoing_message` builds (it copies the canonical body into `OutgoingMessage.body`
//! and lives through the seal); and one `content.clone()` of it per seal try in `fresh_envelope`, which moves into the
//! `d2d::Envelope` that `encode::envelope_bytes` drops after `encode_to_vec` (the encoded bytes are `Zeroizing`). TH7
//! may wipe them with its kinds.

use std::collections::HashSet;
use std::fmt;
use std::num::NonZeroU8;

use kourier_db::rusqlite::Connection;
use kourier_db::{Tx, ZoneDb};
use libthreema::common::keys::{ClientKey, CspE2eKey};
use libthreema::common::{MessageId, Nonce, ThreemaId};
use libthreema::protobuf::common::{CspE2eMessageType, GroupIdentity};
use libthreema::protobuf::{d2d, d2d_sync};
use prost::Message as _;
use zeroize::Zeroizing;

use super::encode::{self, Head};
use super::props::{self, Distribution, Exempt, Props};
use super::queue::{self, NewFate, NewOrigin, NewReceiver, SealedRows, SendTask};
use super::receivers::{self, Fate, View};
use super::{NewJob, body};
use crate::error::{Result, SendRefused, StoreError};
use crate::msg_id;
use crate::reflect::Keys;
use crate::store::{self, NonceScope};

/// The rule name of the WARN line of a receiver that is the user itself (the spec: log a warning, remove it).
const RULE_RECEIVER_OWN: &str = "send-receiver-own";
/// The rule name of the WARN line of receivers marked invalid.
const RULE_RECEIVER_INVALID: &str = "send-receiver-invalid";
/// The rule name of the DEBUG line of blocked receivers: the user's own choice, quiet.
const RULE_RECEIVER_BLOCKED: &str = "send-receiver-blocked";
/// The rule name of the WARN line of receivers with no usable key.
const RULE_RECEIVER_NO_KEY: &str = "send-receiver-no-key";

/// The most bytes of one `message-with-metadata-box`: the frame limit 8,192 less its length, tag and container head.
const PAYLOAD_MAX: usize = 8_170;
/// The most bytes of one sealed envelope: what the mediator's frame carries.
const ENVELOPE_MAX: usize = 65_516;
/// How many draws a fresh nonce may take: a hit is a belt, eight in a row is no chance.
const NONCE_TRIES: u32 = 8;

const OP_NONCE: &str = "draw a fresh nonce failed";
const OP_SEAL: &str = "seal a message to send failed";
const OP_IDENTITY: &str = "read the identity of a send failed";

/// The conversation of a message.
#[derive(Clone, PartialEq, Eq)]
pub enum To {
    /// A 1:1 chat with this Threema ID.
    Contact(String),
    /// A group.
    Group {
        /// The creator's Threema ID.
        creator: String,
        /// The group's id.
        group_id: u64,
    },
}

impl fmt::Debug for To {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Contact(_) => "To::Contact",
            Self::Group { .. } => "To::Group",
        })
    }
}

/// What a message says.
#[derive(Clone, PartialEq, Eq)]
pub enum NewBody {
    /// A text: `TEXT` in a 1:1 chat, `GROUP_TEXT` in the member container in a group.
    Text(String),
    /// A body the caller wrote, whole (a group form holds its container).
    Raw {
        /// The CSP type's number.
        ty: u8,
        /// The unpadded `padded-data`, what a reflected `OutgoingMessage.body` holds.
        body: Vec<u8>,
        /// Whether it distributes the profile, read only where the type decides by its content (`ByBody`, L8).
        distributes: bool,
    },
}

impl fmt::Debug for NewBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, len) = match self {
            Self::Text(text) => ("NewBody::Text", text.len()),
            Self::Raw { body, .. } => ("NewBody::Raw", body.len()),
        };
        f.debug_tuple(kind).field(&len).finish()
    }
}

/// What one receiver gets instead of the canonical message (the spec's _specific_ message, step 2.1.4).
#[derive(Clone, PartialEq, Eq)]
pub struct Specific {
    /// The receiver.
    pub identity: String,
    /// The CSP type's number: its flags, its blocking exemption and its profile distribution count for this receiver.
    pub ty: u8,
    /// The unpadded `padded-data`.
    pub body: Vec<u8>,
}

impl fmt::Debug for Specific {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Specific")
            .field("ty", &self.ty)
            .field("body", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// One message of the core's outbox, to be prepared under its `local`. `Debug` prints counts.
#[derive(Clone, PartialEq, Eq)]
pub struct NewMessage {
    /// `req.local`, `msg_id`'s text form: the ledger's key and the message's id.
    pub local: String,
    /// A short name of the code's: `[a-z0-9-]`, 1 to 32 characters (the schema's CHECK).
    pub kind: &'static str,
    /// The conversation. A group must be stored and hold the user.
    pub to: To,
    /// `req.sent_at` (ms).
    pub created_at: i64,
    /// What it says: the canonical message, the one the device group is told.
    pub body: NewBody,
    /// Who gets another message than the canonical one.
    pub specific: Vec<Specific>,
    /// `None`: everyone the conversation says. `Some`: these alone, and the others get no row (a retry that found no
    /// ledger row: the contract's "to those and to nobody else").
    pub only: Option<Vec<String>>,
}

impl fmt::Debug for NewMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewMessage")
            .field("kind", &self.kind)
            .field("body", &self.body)
            .field("specific", &self.specific.len())
            .field("only", &self.only.as_ref().map(Vec::len))
            .finish_non_exhaustive()
    }
}

/// What the zone reads of the file.
struct StoreView<'a>(&'a Connection);

impl View for StoreView<'_> {
    fn contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
        store::load_contact(self.0, identity)
    }

    fn settings(&self) -> Result<Option<d2d_sync::Settings>> {
        store::load_settings(self.0)
    }

    fn identities_in_groups(&self) -> Result<HashSet<String>> {
        store::identities_in_groups(self.0)
    }
}

/// The keys of the stored identity.
struct SealKeys {
    keys: Keys,
    ck: ClientKey,
    device: u64,
}

impl SealKeys {
    fn of_file(conn: &Connection) -> Result<Self> {
        let identity = store::load_identity(conn)?.ok_or(StoreError::Failed(OP_IDENTITY))?;
        Ok(Self {
            keys: Keys::of(&identity).map_err(|_| StoreError::Failed(OP_IDENTITY))?,
            ck: ClientKey::from(*identity.client_key),
            device: identity.d2m_device_id,
        })
    }
}

fn refused(reason: SendRefused) -> StoreError {
    StoreError::Refused(reason)
}

fn threema_id(text: &str) -> Result<ThreemaId> {
    ThreemaId::try_from(text).map_err(|_| refused(SendRefused::BadId))
}

fn csp_type(ty: u8) -> Result<CspE2eMessageType> {
    CspE2eMessageType::try_from(i32::from(ty)).map_err(|_| refused(SendRefused::UnknownType))
}

/// The properties of a type PREPARE may queue. A type whose flags hold `NO_SERVER_ACK` (a typing indicator) is refused
/// ([`SendRefused::NotQueued`], P-L1): the chat server never acks it, so a row of it would wait for an ack that never
/// comes, fail its runs and be sent again on every socket. It is volatile and goes as a frame with no row (TH7). Every
/// use of a type's properties in PREPARE (the canonical type, a _specific_ one, the Gateway rule of the creator's)
/// goes through here.
fn props_of(ty: CspE2eMessageType) -> Result<Props> {
    let props = props::props(ty).ok_or(refused(SendRefused::UnknownType))?;
    if props.flags & props::NO_SERVER_ACK != 0 {
        return Err(refused(SendRefused::NotQueued));
    }
    Ok(props)
}

/// A nonce no box has used: not in the store, not drawn for this message. `seen` holds the hashes drawn so far.
fn fresh_nonce(
    keys: &Keys,
    seen: &mut HashSet<[u8; 32]>,
    used: &dyn Fn(&[u8; 32]) -> Result<bool>,
    draw: &mut dyn FnMut() -> Nonce,
) -> Result<(Nonce, [u8; 32])> {
    for _ in 0..NONCE_TRIES {
        let nonce = draw();
        let hash = keys.hash(&nonce);
        if !seen.contains(&hash) && !used(&hash)? {
            seen.insert(hash);
            return Ok((nonce, hash));
        }
    }
    Err(StoreError::Failed(OP_NONCE))
}

/// The padded container and the metadata of one receiver sealed under its nonce, in the head's layout, within the
/// limit of a payload.
fn one_box(
    head: &Head<'_>,
    key: &CspE2eKey,
    nonce: &Nonce,
    metadata: Zeroizing<Vec<u8>>,
    ty: CspE2eMessageType,
    data: &[u8],
    draw: NonZeroU8,
) -> Result<Vec<u8>> {
    let container = encode::padded_container(ty, data, draw);
    let sealed = encode::seal_box(key, nonce, metadata, container)
        .map_err(|_| StoreError::Failed(OP_SEAL))?;
    let payload = encode::layout(head, &sealed.metadata, nonce, &sealed.message)
        .map_err(|_| StoreError::Failed(OP_SEAL))?;
    if payload.len() > PAYLOAD_MAX {
        return Err(refused(SendRefused::TooLarge));
    }
    Ok(payload)
}

/// One sealed envelope within its limit, with the nonce `seal_reflected` drew.
fn one_envelope(
    keys: &SealKeys,
    content: d2d::envelope::Content,
    draw: u16,
) -> Result<(Nonce, Vec<u8>)> {
    let bytes = encode::envelope_bytes(keys.device, content, draw);
    let (nonce, sealed) =
        encode::seal_envelope(keys.keys.dgk(), bytes).map_err(|_| StoreError::Failed(OP_SEAL))?;
    if sealed.len() > ENVELOPE_MAX {
        return Err(refused(SendRefused::TooLarge));
    }
    Ok((nonce, sealed))
}

/// A sealed envelope under a D2D nonce the store has not seen, and that nonce's hash.
fn fresh_envelope(
    keys: &SealKeys,
    content: &d2d::envelope::Content,
    used: &dyn Fn(&[u8; 32]) -> Result<bool>,
) -> Result<(Vec<u8>, [u8; 32])> {
    for _ in 0..NONCE_TRIES {
        let (nonce, sealed) = one_envelope(keys, content.clone(), encode::draw_envelope_pad())?;
        let hash = keys.keys.hash(&nonce);
        if !used(&hash)? {
            return Ok((sealed, hash));
        }
    }
    Err(StoreError::Failed(OP_NONCE))
}

/// The conversation as `d2d.ConversationId`, what the reflect and the `Sent` update name.
fn conversation_of(to: &To) -> d2d::ConversationId {
    use d2d::conversation_id::Id;
    d2d::ConversationId {
        id: Some(match to {
            To::Contact(identity) => Id::Contact(identity.clone()),
            To::Group { creator, group_id } => Id::Group(GroupIdentity {
                group_id: *group_id,
                creator_identity: creator.clone(),
            }),
        }),
    }
}

/// The prost bytes of the conversation, as the row's `conversation` column holds them.
pub(crate) fn conversation_bytes(to: &To) -> Vec<u8> {
    conversation_of(to).encode_to_vec()
}

/// The canonical message: its type and its unpadded `padded-data`.
fn canonical(to: &To, new: &NewBody) -> Result<(CspE2eMessageType, Zeroizing<Vec<u8>>)> {
    match new {
        NewBody::Text(text) => {
            let inner = Zeroizing::new(body::text(text));
            match to {
                To::Contact(_) => Ok((CspE2eMessageType::Text, inner)),
                To::Group { creator, group_id } => Ok((
                    CspE2eMessageType::GroupText,
                    Zeroizing::new(body::group_member_container(
                        threema_id(creator)?,
                        *group_id,
                        &inner,
                    )),
                )),
            }
        }
        NewBody::Raw { ty, body, .. } => Ok((csp_type(*ty)?, Zeroizing::new(body.clone()))),
    }
}

/// The nickname a message that distributes the profile carries: the stored one trimmed, and an empty string when there
/// is none (the spec: "When the user cleared its nickname, send an empty string") or when it is the user's own ID ("Do
/// not send the user's Threema ID").
fn distributed_nickname(stored: Option<&str>, own: ThreemaId) -> String {
    let nickname = stored.unwrap_or_default().trim();
    if nickname == own.as_str() {
        String::new()
    } else {
        nickname.to_owned()
    }
}

/// What a message's own properties and its body say of the profile.
fn distributes(props: &Props, body: &NewBody) -> bool {
    match props.profile_distribution {
        Distribution::No => false,
        Distribution::Yes => true,
        Distribution::ByBody => match body {
            NewBody::Raw { distributes, .. } => *distributes,
            NewBody::Text(_) => true,
        },
    }
}

/// One WARN line per rule, a count and no ID; the blocked ones are the user's own choice and quiet.
fn note_left_out(fates: &[(String, Fate)]) {
    let count = |matches: &dyn Fn(&Fate) -> bool| fates.iter().filter(|(_, f)| matches(f)).count();
    for (rule, n) in [
        (RULE_RECEIVER_OWN, count(&|f| matches!(f, Fate::Own))),
        (
            RULE_RECEIVER_INVALID,
            count(&|f| matches!(f, Fate::Invalid)),
        ),
        (RULE_RECEIVER_NO_KEY, count(&|f| matches!(f, Fate::NoKey))),
    ] {
        if n > 0 {
            tracing::warn!("receivers of a send left out: {n} by {rule}");
        }
    }
    let blocked = count(&|f| matches!(f, Fate::Blocked));
    if blocked > 0 {
        tracing::debug!("receivers of a send left out: {blocked} by {RULE_RECEIVER_BLOCKED}");
    }
}

/// One message on its way into the rows: what both doors have in common.
struct Plan<'a> {
    origin: NewOrigin,
    kind: &'static str,
    to: &'a To,
    message_id: MessageId,
    created_at: i64,
    body: &'a NewBody,
    specific: &'a [Specific],
    only: Option<&'a [String]>,
    now_ms: i64,
}

impl Plan<'_> {
    /// The message `identity` gets: its _specific_ one, or the canonical one.
    fn specific_of(&self, identity: &str) -> Option<&Specific> {
        self.specific.iter().find(|s| s.identity == identity)
    }

    /// Who the message is for, sorted, each once. `only` replaces the list; a group's list goes by the Gateway rule of
    /// the type the creator gets (its _specific_ one, as Desktop's dynamic encoder).
    fn listed(&self, conn: &Connection, own: ThreemaId, canonical: &Props) -> Result<Vec<String>> {
        let mut ids = match self.to {
            To::Contact(identity) => {
                if !store::contact_exists(conn, identity)? {
                    return Err(refused(SendRefused::UnknownContact));
                }
                self.only
                    .map_or_else(|| vec![identity.clone()], <[String]>::to_vec)
            }
            To::Group { creator, group_id } => {
                let creator_id = threema_id(creator)?;
                let stored = store::load_group(conn, creator, *group_id)?;
                if matches!(self.origin, NewOrigin::Transmit { .. }) {
                    let group = stored.as_ref().ok_or(refused(SendRefused::UnknownGroup))?;
                    if !crate::join::events::is_member(group) {
                        return Err(refused(SendRefused::NotAMember));
                    }
                }
                match (self.only, stored) {
                    (Some(only), _) => only.to_vec(),
                    (None, None) => return Err(refused(SendRefused::UnknownGroup)),
                    (None, Some(group)) => {
                        let rule = match self.specific_of(creator) {
                            Some(specific) => props_of(csp_type(specific.ty)?)?.gateway_creator,
                            None => canonical.gateway_creator,
                        };
                        receivers::group_receivers(&group, creator_id, own, rule)
                    }
                }
            }
        };
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    /// What blocking asks of the type `identity` gets.
    fn exempt_of(&self, identity: &str, canonical: &Props) -> Exempt {
        self.specific_of(identity)
            .and_then(|s| CspE2eMessageType::try_from(i32::from(s.ty)).ok())
            .and_then(props::props)
            .map_or(canonical.exempt_from_blocking, |p| p.exempt_from_blocking)
    }

    /// THE SEAL: fates, nonces, boxes, the reflect, and the rows that hold them. Reads the file and writes nothing.
    fn seal(&self, conn: &Connection) -> Result<SealedRows> {
        self.seal_drawing(conn, &mut Nonce::random)
    }

    /// [`Plan::seal`] with the source of the boxes' nonces in hand, so a test can make a draw hit the store.
    fn seal_drawing(
        &self,
        conn: &Connection,
        draw: &mut dyn FnMut() -> Nonce,
    ) -> Result<SealedRows> {
        let keys = SealKeys::of_file(conn)?;
        let own = keys.keys.own();
        let (ty, canonical_body) = canonical(self.to, self.body)?;
        let props = props_of(ty)?;
        let created_at = u64::try_from(self.created_at).map_err(|_| StoreError::Failed(OP_SEAL))?;
        let listed = self.listed(conn, own, &props)?;
        for identity in &listed {
            threema_id(identity)?;
        }
        for specific in self.specific {
            props_of(csp_type(specific.ty)?)?;
        }
        let fates = receivers::fates(&StoreView(conn), &keys.ck, own, &listed, &|identity| {
            self.exempt_of(identity, &props)
        })?;
        note_left_out(&fates);
        let nickname = distributed_nickname(
            store::load_profile(conn)?
                .and_then(|p| p.nickname)
                .as_deref(),
            own,
        );

        let mut seen = HashSet::new();
        let used_csp = |hash: &[u8; 32]| store::contains_nonce(conn, NonceScope::Csp, hash);
        let mut nonces = Vec::with_capacity(fates.len());
        let mut csp_nonces = Vec::with_capacity(fates.len());
        let mut rows = Vec::with_capacity(fates.len());
        for (identity, fate) in fates {
            let fate = match fate {
                Fate::Send(key) => {
                    let (nonce, hash) = fresh_nonce(&keys.keys, &mut seen, &used_csp, draw)?;
                    let specific = self.specific_of(&identity);
                    let (this_ty, this_props) = match specific {
                        Some(s) => {
                            let ty = csp_type(s.ty)?;
                            (ty, props_of(ty)?)
                        }
                        None => (ty, props),
                    };
                    let data = specific.map_or(canonical_body.as_slice(), |s| s.body.as_slice());
                    let this_nickname =
                        distributes(&this_props, self.body).then_some(nickname.as_str());
                    let head = Head {
                        sender: own,
                        receiver: threema_id(&identity)?,
                        message_id: self.message_id,
                        created_at_ms: created_at,
                        flags: this_props.flags,
                        nickname: this_nickname,
                    };
                    let metadata = encode::metadata(self.message_id, created_at, this_nickname);
                    let payload = one_box(
                        &head,
                        &key,
                        &nonce,
                        metadata,
                        this_ty,
                        data,
                        encode::draw_pad(),
                    )?;
                    nonces.push(nonce);
                    csp_nonces.push(hash);
                    Some(NewFate::Send { payload })
                }
                Fate::Own => None,
                Fate::Invalid => Some(NewFate::Invalid),
                Fate::Blocked => Some(NewFate::Blocked),
                Fate::NoKey => Some(NewFate::NoKey),
            };
            if let Some(fate) = fate {
                rows.push(NewReceiver { identity, fate });
            }
        }

        let conversation = conversation_of(self.to);
        let (reflect_env, d2d_nonces) = if props.reflect_outgoing {
            let content = encode::outgoing_message(&encode::Outgoing {
                conversation: &conversation,
                message_id: self.message_id,
                created_at_ms: created_at,
                ty,
                body: &canonical_body,
                nonces: &nonces,
            });
            let (sealed, hash) = fresh_envelope(&keys, &content, &|hash| {
                store::contains_nonce(conn, NonceScope::D2d, hash)
            })?;
            (Some(sealed), vec![hash])
        } else {
            (None, Vec::new())
        };
        Ok(SealedRows {
            origin: self.origin.clone(),
            kind: self.kind,
            message_id: self.message_id.0,
            conversation: conversation.encode_to_vec(),
            created_at: self.created_at,
            has_update: props.reflect_sent_update,
            queued_at: self.now_ms,
            reflect_env,
            receivers: rows,
            csp_nonces,
            d2d_nonces,
        })
    }
}

/// THE SEAL of a [`NewMessage`]: the rows ZONE PREPARE would write, reading the file and writing nothing. A door: the
/// production path is [`prepare_outgoing`], which holds the seal and the write in ONE zone.
#[doc(hidden)]
pub fn seal_outgoing(conn: &Connection, message: &NewMessage, now_ms: i64) -> Result<SealedRows> {
    let message_id = msg_id::parse(&message.local).ok_or(refused(SendRefused::BadLocal))?;
    Plan {
        origin: NewOrigin::Transmit {
            local: message.local.clone(),
        },
        kind: message.kind,
        to: &message.to,
        message_id,
        created_at: message.created_at,
        body: &message.body,
        specific: &message.specific,
        only: message.only.as_deref(),
        now_ms,
    }
    .seal(conn)
}

/// ZONE PREPARE of a [`NewMessage`] (`INSIDE_SEND_PREPARE` / `AFTER_SEND_PREPARE`): sealed and written in ONE zone,
/// the point of no return; returns the task's id. The caller read the ledger first: a `local` the file holds fails by
/// `send_by_local`. Fails with [`StoreError::Refused`] or [`StoreError::QueueFull`], and nothing is written then.
/// `attached` are the tasks a `Transmit` is attached to: a spent `transmit` budget may evict old rows in this same zone
/// (S-M1), never one of these.
#[doc(hidden)]
pub fn prepare_outgoing(
    db: &ZoneDb,
    message: &NewMessage,
    now_ms: i64,
    attached: &[i64],
) -> Result<i64> {
    queue::prepare_sealed_zone(db, attached, |tx: &Tx<'_>| {
        seal_outgoing(tx, message, now_ms)
    })
}

/// THE SEAL of a [`NewJob`]: a random id and the clock's time (`enqueue_job`).
pub(crate) fn seal_job(conn: &Connection, job: &NewJob, now_ms: i64) -> Result<SealedRows> {
    Plan {
        origin: NewOrigin::Job,
        kind: job.kind,
        to: &job.to,
        message_id: MessageId::random(),
        created_at: now_ms,
        body: &job.body,
        specific: &[],
        only: job.only.as_deref(),
        now_ms,
    }
    .seal(conn)
}

/// THE `Sent` UPDATE of `task`, sealed for the last ACKS zone: the envelope and the hash of its D2D nonce, which that
/// zone writes before the frame goes (`mark_send_acks`).
#[doc(hidden)]
pub fn seal_sent_update(conn: &Connection, task: &SendTask) -> Result<(Vec<u8>, [u8; 32])> {
    let keys = SealKeys::of_file(conn)?;
    let conversation = d2d::ConversationId::decode(task.conversation.as_slice())
        .map_err(|_| StoreError::Unreadable("a send row unreadable"))?;
    let content = encode::sent_update(&conversation, MessageId(task.message_id));
    fresh_envelope(&keys, &content, &|hash| {
        store::contains_nonce(conn, NonceScope::D2d, hash)
    })
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use libthreema::common::keys::PublicKey;

    use super::*;
    use crate::store::{Identity, SlotState};

    const OWN: &str = "KOURIER3";
    const PEER: &str = "PEER0001";

    fn identity() -> Identity {
        Identity {
            threema_id: OWN.to_owned(),
            client_key: Zeroizing::new([0x11; 32]),
            server_group: "ds7".to_owned(),
            device_group_key: Zeroizing::new([0x22; 32]),
            device_cookie: Zeroizing::new([0x33; 16]),
            d2m_device_id: 9,
            csp_device_id: 10,
            slot_state: SlotState::Existing,
            device_label: "label".to_owned(),
            linked_at_ms: 1,
        }
    }

    fn seal_keys() -> SealKeys {
        let identity = identity();
        SealKeys {
            keys: Keys::of(&identity).expect("the keys"),
            ck: ClientKey::from(*identity.client_key),
            device: identity.d2m_device_id,
        }
    }

    fn peer_key() -> CspE2eKey {
        ClientKey::from([0x44; 32])
            .derive_csp_e2e_key(&PublicKey::from([7; 32]))
            .expect("a contributory key")
    }

    fn head() -> Head<'static> {
        Head {
            sender: ThreemaId::try_from(OWN).expect("an ID"),
            receiver: ThreemaId::try_from(PEER).expect("an ID"),
            message_id: MessageId(1),
            created_at_ms: 1_700_000_000_000,
            flags: 1,
            nickname: None,
        }
    }

    /// N and N+1 of a payload, through the real seal and the real layout with the pad and the metadata fixed: 64 head + 16
    /// tag + 32 metadata + 24 nonce + 16 tag + 1 type + 8,016 data + 1 pad is 8,170.
    #[test]
    fn a_payload_of_8170_bytes_goes_and_one_of_8171_is_refused() {
        let at = |len: usize| {
            one_box(
                &head(),
                &peer_key(),
                &Nonce([5; 24]),
                Zeroizing::new(vec![0; 32]),
                CspE2eMessageType::Text,
                &vec![0xab; len],
                NonZeroU8::MIN,
            )
            .map(|payload| payload.len())
        };
        assert_eq!(at(8_016), Ok(8_170));
        assert_eq!(at(8_017), Err(StoreError::Refused(SendRefused::TooLarge)));
    }

    /// N and N+1 of an envelope, through the real encode and seal (draw 0: no padding over 64 bytes): the sealed length
    /// grows by one with each byte of the body, and 65,516 is the last that goes.
    #[test]
    fn a_sealed_envelope_of_65516_bytes_goes_and_one_of_65517_is_refused() {
        let keys = seal_keys();
        let conversation = conversation_of(&To::Contact(PEER.to_owned()));
        let at = |len: usize| {
            let body = vec![0xab; len];
            one_envelope(
                &keys,
                encode::outgoing_message(&encode::Outgoing {
                    conversation: &conversation,
                    message_id: MessageId(1),
                    created_at_ms: 1,
                    ty: CspE2eMessageType::Text,
                    body: &body,
                    nonces: &[],
                }),
                0,
            )
        };
        let under = at(65_000).expect("well under the limit").1.len();
        let last = 65_000 + (65_516 - under);
        assert_eq!(at(last).expect("the limit itself").1.len(), 65_516);
        assert_eq!(
            at(last + 1).map(|(_, sealed)| sealed.len()),
            Err(StoreError::Refused(SendRefused::TooLarge))
        );
    }

    /// A drawn nonce the store holds is drawn again, and so is one this message already drew; the hash goes with its nonce.
    #[test]
    fn a_nonce_the_store_holds_or_the_message_drew_is_drawn_again() {
        let keys = seal_keys().keys;
        let (stored, drawn, fresh) = ([1; 24], [2; 24], [3; 24]);
        let stored_hash = keys.hash(&Nonce(stored));
        let mut seen = HashSet::from([keys.hash(&Nonce(drawn))]);
        let mut draws = vec![fresh, drawn, stored].into_iter().rev();
        let mut draw = move || Nonce(draws.next().expect("a draw left"));
        let (nonce, hash) = fresh_nonce(
            &keys,
            &mut seen,
            &|hash| Ok(*hash == stored_hash),
            &mut draw,
        )
        .expect("the third draw is fresh");
        assert!(nonce.0 == fresh, "the third draw");
        assert!(hash == keys.hash(&Nonce(fresh)), "the hash of that nonce");
        assert!(seen.contains(&hash), "the next box of the message sees it");
    }

    /// THE WIRING over a real file: a nonce the CSP store holds never reaches a box, a nonce only the D2D store holds is
    /// no hit, and neither does a nonce the message already drew; what the rows hash is what the boxes use.
    #[test]
    fn no_box_uses_a_nonce_of_the_csp_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = store::open(
            &dir.path().join("backend.db"),
            &kourier_db::DbKey::from_bytes([0x5f; 32]),
        )
        .expect("a file");
        let keys = seal_keys().keys;
        let (stored, d2d_only, next) = ([1; 24], [2; 24], [3; 24]);
        db.zone(|tx| {
            store::save_identity(tx, &identity())?;
            for peer in [PEER, "PEER0002"] {
                store::put_contact(
                    tx,
                    &d2d_sync::Contact {
                        identity: peer.to_owned(),
                        public_key: Some(vec![7; 32]),
                        ..Default::default()
                    },
                )?;
            }
            store::add_nonces(tx, NonceScope::Csp, &[keys.hash(&Nonce(stored))])?;
            store::add_nonces(tx, NonceScope::D2d, &[keys.hash(&Nonce(d2d_only))])
        })
        .expect("the seed");

        let mut draws = [stored, d2d_only, d2d_only, stored, next].into_iter();
        let mut draw = || Nonce(draws.next().expect("a draw left"));
        let to = To::Group {
            creator: OWN.to_owned(),
            group_id: 1,
        };
        let only = [PEER.to_owned(), "PEER0002".to_owned()];
        let body = NewBody::Text("x".to_owned());
        let plan = Plan {
            origin: NewOrigin::Job,
            kind: "test",
            to: &to,
            message_id: MessageId(1),
            created_at: 1,
            body: &body,
            specific: &[],
            only: Some(&only),
            now_ms: 1,
        };
        let rows = db
            .read(|conn: &Connection| plan.seal_drawing(conn, &mut draw))
            .expect("the seal");
        assert!(
            rows.csp_nonces == [keys.hash(&Nonce(d2d_only)), keys.hash(&Nonce(next))],
            "the hashes in the rows are of the two nonces no box has used"
        );
        let nonce_of = |payload: &[u8]| -> Vec<u8> {
            let metadata = usize::from(u16::from_le_bytes([payload[30], payload[31]]));
            payload[64 + metadata..64 + metadata + 24].to_vec()
        };
        let used: Vec<Vec<u8>> = rows
            .receivers
            .iter()
            .map(|r| match &r.fate {
                NewFate::Send { payload } => nonce_of(payload),
                _ => panic!("a box for each"),
            })
            .collect();
        assert!(
            used == [d2d_only.to_vec(), next.to_vec()],
            "the boxes use the nonces the rows hash"
        );
    }

    /// Eight hits in a row are no chance: the zone fails, and the store's own failure is not taken for a hit.
    #[test]
    fn eight_hits_fail_and_a_failing_store_is_no_hit() {
        let keys = seal_keys().keys;
        let draws = Cell::new(0_u32);
        let mut draw = || {
            draws.set(draws.get() + 1);
            Nonce([1; 24])
        };
        let out = fresh_nonce(&keys, &mut HashSet::new(), &|_| Ok(true), &mut draw);
        assert_eq!(out.map(|_| ()), Err(StoreError::Failed(OP_NONCE)));
        assert_eq!(draws.get(), 8);

        let full = StoreError::Sql {
            op: "x",
            code: Some(13),
        };
        let out = fresh_nonce(
            &keys,
            &mut HashSet::new(),
            &|_| Err(full.clone()),
            &mut Nonce::random,
        );
        assert_eq!(out.map(|_| ()), Err(full));
    }

    /// An envelope whose nonce the store holds is sealed anew under another; eight hits fail.
    #[test]
    fn an_envelope_under_a_stored_nonce_is_sealed_again() {
        let keys = seal_keys();
        let content = encode::sent_update(
            &conversation_of(&To::Contact(PEER.to_owned())),
            MessageId(1),
        );
        let first = Cell::new(None);
        let (sealed, hash) = fresh_envelope(&keys, &content, &|hash| {
            if first.get().is_none() {
                first.set(Some(*hash));
                return Ok(true);
            }
            Ok(false)
        })
        .expect("the second seal");
        assert!(first.get() != Some(hash), "another nonce");
        assert!(sealed.len() > 40, "nonce, tag and the envelope");
        assert_eq!(
            fresh_envelope(&keys, &content, &|_| Ok(true)).map(|_| ()),
            Err(StoreError::Failed(OP_NONCE))
        );
    }

    /// The nickname of the spec's sentences.
    #[test]
    fn the_nickname_is_trimmed_and_never_the_own_id() {
        let own = ThreemaId::try_from(OWN).expect("an ID");
        for (at, (stored, sent)) in [
            (None, ""),
            (Some(""), ""),
            (Some("   "), ""),
            (Some("Ann"), "Ann"),
            (Some("  Ann B  "), "Ann B"),
            (Some(OWN), ""),
            (Some(" KOURIER3 "), ""),
            (Some("KOURIER33"), "KOURIER33"),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(distributed_nickname(stored, own) == sent, "case {at}");
        }
    }

    /// `Yes` and `No` are the type's; `ByBody` is the caller's, and a text has no such type.
    #[test]
    fn only_a_type_that_decides_by_its_body_reads_the_callers_word() {
        let raw = |distributes| NewBody::Raw {
            ty: 0x80,
            body: vec![],
            distributes,
        };
        let of = |ty| props::props(ty).expect("a row");
        for (ty, body, expected) in [
            (CspE2eMessageType::Text, raw(false), true),
            (CspE2eMessageType::GroupName, raw(true), false),
            (CspE2eMessageType::DeliveryReceipt, raw(true), true),
            (CspE2eMessageType::DeliveryReceipt, raw(false), false),
        ] {
            assert_eq!(distributes(&of(ty), &body), expected, "{ty:?}");
        }
    }

    /// A refusal is the content's failure: the same message is refused again.
    #[test]
    fn a_refusal_is_the_contents_failure() {
        assert!(StoreError::Refused(SendRefused::TooLarge).is_content());
    }

    /// `Debug` of the inputs prints counts: no ID, no body.
    #[test]
    fn debug_of_the_inputs_prints_counts() {
        let message = NewMessage {
            local: "0102030405060708".to_owned(),
            kind: "text",
            to: To::Contact(PEER.to_owned()),
            created_at: 1,
            body: NewBody::Text("a secret text".to_owned()),
            specific: vec![Specific {
                identity: PEER.to_owned(),
                ty: 1,
                body: b"a secret body".to_vec(),
            }],
            only: Some(vec![PEER.to_owned()]),
        };
        let printed = format!("{message:?} {:?} {:?}", message.to, message.specific);
        for secret in [PEER, "secret", "0102030405060708"] {
            assert!(!printed.contains(secret), "Debug printed {secret}");
        }
    }
}
