//! THE LEADER'S RECEIVE STEPS (TH6 L3; `TP/csp.struct.yml:686-873`, the brief's §2.3 and §2.4 with the critic's
//! changes T-B1, T-M1, T-M3, T-L3, T-L4): ONE PURE FUNCTION per incoming message, [`decide`], over a read [`View`] of
//! the file and the socket's memory. It returns ONE [`Verdict`] with the reason as a RULE NAME:
//!
//! - [`Verdict::Process`]: map, reflect, ack (steps 23-25), with the jobs the type's steps send.
//! - [`Verdict::Discard`]: the spec's discard, then _Acknowledge_ (the ack unless `0x04`, the nonce where marked).
//! - [`Verdict::Hold`]: no zone, no nonce, no ack, nothing reflected: the server redelivers it to whichever device
//!   leads next (unknown types C2, real group changes C1/T-M1, the knock-on T-B1, a creator's answer that needs a
//!   picture T-M3).
//! - [`Verdict::RejectFs`]: an FS envelope with `encapsulated` from a stored contact: discarded, acked, and a `Reject`
//!   job (C8).
//! - [`Verdict::Ephemeral`]: the typing indicator: an event and an ephemeral reflect; no zone, nonce or ack.
//! - [`Verdict::Lookup`] and [`Verdict::Contact`]: the head WAITS (L5): the sender needs the directory, or the steps
//!   22.4-22.6 need a `CONTACT_SYNC` transaction. The caller does that and calls [`decide`] again with the answer
//!   ([`Input::looked`]) or with the change committed ([`Input::contact_done`]).
//!
//! THE ORDER is the spec's, with these departures, each named where it is made:
//! - Step 0 (the `csp_inbound` row of (sender, id), premise P1) and step 14 (an id seen) are the caller's, BEFORE
//!   this function: `leader::zone::inbound_row`. Step 1 (not leader) is the connection's H1 guard.
//! - Step 9/16, an unknown type: HELD (C2), never acked (Desktop `incoming-message.ts:1039-1052`).
//! - GROUP CONTROL (setup, name, leave, set/delete picture) is decided right after step 21, BEFORE 22.4-22.6 (T-M1): no
//!   change against the stored group is the spec's discard (`group-no-change`); a real change is HELD
//!   (`group-sync-needed`, his TH6-Q1 "They wait for your phone"), so no contact transaction runs for a message that
//!   is held. A group in the socket's held set holds ALL its control messages (`group-held-before`: the order of
//!   changes is the phone's to apply). From a sender the file does not hold (TH6 FIX-A, P-L1) every group control type
//!   but `group-setup` is the spec's 22.4.1 discard; a stranger's `group-setup` that its own steps discard makes the
//!   contact first (22.4.2), as the spec orders it.
//! - THE BLIND MARK (TH6 FIX-A, S-B1): while the socket holds a message whose group the steps never saw,
//!   [`View::held_group`] is true for EVERY group, so the knock-on holds what it would discard.
//! - THE KNOCK-ON (T-B1): a group message whose group is held is HELD where the steps would DISCARD it: at 22.4.1 (an
//!   unknown sender of a group form, e.g. a member the held setup added) and in the Common Group Receive Steps.
//! - THE CREATOR'S ANSWER (C7, T-M3): three jobs with no transaction, only when the stored group's picture is
//!   EXPLICITLY removed; a picture set or unknown holds `creator-answer-needs-picture`.
//!
//! The Identity Blocked Steps are `send::receivers::Blocking`, shared with the send. Pure: no write, no clock but the
//! view's, no log; `Debug` prints no ID, nickname, key, nonce or body.

use std::collections::BTreeSet;
use std::fmt;

use libthreema::common::keys::{ClientKey, PublicKey};
use libthreema::common::{MessageId, Nonce};
use libthreema::protobuf::common::{self, CspE2eMessageType};
use libthreema::protobuf::d2d_sync;
use prost::Message as _;
use zeroize::Zeroizing;

use super::directory::{self, ContactInit, Looked};
use super::open::{self, Boxed};
use super::props::{self, Action, GroupForm, Replay};
use crate::error::Result;
use crate::join::events::is_member;
use crate::reflect::body::{self as parse, GroupRef};
use crate::send::props::{Distribution, Exempt};
use crate::send::receivers::{self, Blocking};

/// Step 2: the nonce was used before.
pub(crate) const RULE_NONCE_USED: &str = "csp-nonce-used";
/// Step 3: the receiver is not the user.
pub(crate) const RULE_RECEIVER: &str = "csp-receiver";
/// Step 5: the sender is the user.
pub(crate) const RULE_SENDER_OWN: &str = "csp-sender-own";
/// Step 5: the directory does not know the sender (never existed or revoked).
pub(crate) const RULE_SENDER_INVALID: &str = "csp-sender-invalid";
/// Steps 9 and 16: a type the pinned enum lacks (HOLD, C2).
pub(crate) const RULE_UNKNOWN_TYPE: &str = "csp-unknown-type";
/// Step 10: the body of the type does not decode (a group container too short, a setup's members, the FS envelope,
/// the invalid type `0x00`).
pub(crate) const RULE_BODY: &str = "csp-body";
/// Step 12: an FS envelope with `encapsulated` (Kourier has no FS session: rejected, C8).
pub(crate) const RULE_FS_REJECTED: &str = "fs-rejected";
/// Step 12: an FS envelope of any other content (init, accept, reject, terminate).
pub(crate) const RULE_FS_IGNORED: &str = "fs-ignored";
/// Step 13: the metadata's id is not the head's.
pub(crate) const RULE_ID_MISMATCH: &str = "csp-id-mismatch";
/// Steps 18-19: the sender (or a group setup's creator) is blocked.
pub(crate) const RULE_BLOCKED: &str = "csp-blocked";
/// Steps 20-21: a special contact's own type (web session resume, work sync): Kourier has neither a web nor a work
/// client.
pub(crate) const RULE_SPECIAL: &str = "csp-special";
/// Steps 20-21: a special contact sent another type.
pub(crate) const RULE_SPECIAL_TYPE: &str = "csp-special-type";
/// 22.7: a type only a special contact sends came from another sender.
pub(crate) const RULE_SPECIAL_ONLY: &str = "csp-special-only";
/// 22.4.1: an unknown sender of a type that creates no direct contact.
pub(crate) const RULE_SENDER_UNKNOWN: &str = "csp-sender-unknown";
/// 22.7: the `empty` message.
pub(crate) const RULE_EMPTY: &str = "csp-empty";
/// T-M1: group control that changes the group: HELD for the phone.
pub(crate) const RULE_GROUP_SYNC_NEEDED: &str = "group-sync-needed";
/// T-B1 and T-M1: a message of a group with held control: HELD where the steps would discard it.
pub(crate) const RULE_GROUP_HELD_BEFORE: &str = "group-held-before";
/// T-M1: group control that changes nothing (the periodic re-send of an unchanged group).
pub(crate) const RULE_GROUP_NO_CHANGE: &str = "group-no-change";
/// Common Group Receive Steps 2.1: the group is unknown and the user is its creator.
pub(crate) const RULE_GROUP_UNKNOWN_OWN: &str = "group-unknown-own";
/// Common Group Receive Steps 2.2: the group is unknown and its creator is blocked.
pub(crate) const RULE_GROUP_UNKNOWN_BLOCKED: &str = "group-unknown-blocked";
/// Common Group Receive Steps 2.3: the group is unknown (a `group-sync-request` job unless one went within the hour).
pub(crate) const RULE_GROUP_UNKNOWN: &str = "group-unknown";
/// Common Group Receive Steps 3: the user left the group (an empty setup or a leave job to the sender).
pub(crate) const RULE_GROUP_LEFT: &str = "group-left";
/// Common Group Receive Steps 4: the sender is not a member (an empty setup or a sync request job).
pub(crate) const RULE_GROUP_NOT_MEMBER: &str = "group-not-member";
/// `group-setup` step 4.1: a setup without the user for a group the user does not know.
pub(crate) const RULE_SETUP_NOT_MEMBER: &str = "group-setup-not-member";
/// `group-leave` step 1: the creator cannot leave.
pub(crate) const RULE_LEAVE_CREATOR: &str = "group-leave-creator";
/// `group-sync-request` step 2: the user is not the creator.
pub(crate) const RULE_SYNC_NOT_CREATOR: &str = "group-sync-not-creator";
/// `group-sync-request` step 3: answered this sender within the hour.
pub(crate) const RULE_SYNC_THROTTLED: &str = "group-sync-throttled";
/// T-M3: the creator's answer needs a picture Kourier cannot send yet (TH8), or one it does not know: HELD.
pub(crate) const RULE_CREATOR_PICTURE: &str = "creator-answer-needs-picture";

/// Every rule name a verdict can carry, the open's included: the run tool's `held_by` keys and the discard counts are
/// these words.
pub(crate) const RULES: [&str; 33] = [
    open::RULE_HEAD,
    open::RULE_METADATA,
    open::RULE_UNDECRYPTABLE,
    open::RULE_CONTAINER,
    open::RULE_LEGACY_FF,
    RULE_NONCE_USED,
    RULE_RECEIVER,
    RULE_SENDER_OWN,
    RULE_SENDER_INVALID,
    RULE_UNKNOWN_TYPE,
    RULE_BODY,
    RULE_FS_REJECTED,
    RULE_FS_IGNORED,
    RULE_ID_MISMATCH,
    RULE_BLOCKED,
    RULE_SPECIAL,
    RULE_SPECIAL_TYPE,
    RULE_SPECIAL_ONLY,
    RULE_SENDER_UNKNOWN,
    RULE_EMPTY,
    RULE_GROUP_SYNC_NEEDED,
    RULE_GROUP_HELD_BEFORE,
    RULE_GROUP_NO_CHANGE,
    RULE_GROUP_UNKNOWN_OWN,
    RULE_GROUP_UNKNOWN_BLOCKED,
    RULE_GROUP_UNKNOWN,
    RULE_GROUP_LEFT,
    RULE_GROUP_NOT_MEMBER,
    RULE_SETUP_NOT_MEMBER,
    RULE_LEAVE_CREATOR,
    RULE_SYNC_NOT_CREATOR,
    RULE_SYNC_THROTTLED,
    RULE_CREATOR_PICTURE,
];

/// The no-ack flag of the CSP head (`0x04`): such a message is never acked (the _Acknowledge_ steps' step 2).
const NO_SERVER_ACK: u8 = 0x04;
/// No automatic delivery receipts (`0x80`, step 24).
const NO_RECEIPT: u8 = 0x80;

/// What the steps read: the file (contacts, settings, groups, nonces; the send's [`receivers::View`] for the blocked
/// steps) and the socket's and actor's memory (the lookup cache, the held groups, the two 1 h throttles).
pub(crate) trait View: receivers::View {
    /// The user's Threema ID.
    fn own(&self) -> &str;
    /// The actor's clock (ms): the T-L3 clamp of `created_at`, the cache's age.
    fn now_ms(&self) -> u64;
    /// Step 2: whether the CSP nonce is in the store (hashed by `reflect::Keys::hash`, the nonce rule's one home).
    fn csp_nonce_used(&self, nonce: &Nonce) -> Result<bool>;
    /// The stored group of (creator, id).
    fn group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>>;
    /// The lookup cache's init of `identity` (positive answers younger than 30 min).
    fn cached(&self, identity: &str) -> Option<ContactInit>;
    /// Whether the socket holds a control message of `group` (T-B1's set), or any message whose group the steps never
    /// saw (S-B1's blind mark: then every group counts as held).
    fn held_group(&self, group: &GroupRef) -> bool;
    /// TH6 re-check R-L2: whether a BLIND hold stands on the socket (the mark [`View::held_group`] reads for every
    /// group): a 1:1 message has no group to ask about, so 22.4.1 asks this.
    fn blind(&self) -> bool;
    /// Whether the user sent a `group-sync-request` for `group` within the hour (Group Sync Request Steps, step 2).
    fn sync_request_throttled(&self, group: &GroupRef) -> bool;
    /// Whether the user answered `sender`'s request for its own group `group_id` within the hour.
    fn answer_throttled(&self, group_id: u64, sender: &str) -> bool;
}

/// One message for the steps.
pub(crate) struct Input<'a> {
    /// The whole `message-with-metadata-box` payload.
    pub(crate) bytes: &'a [u8],
    /// The user's client key.
    pub(crate) ck: &'a ClientKey,
    /// The directory's answer for the sender, when the caller asked after a [`Verdict::Lookup`].
    pub(crate) looked: Option<&'a Looked>,
    /// The contact change of 22.4-22.6 committed (after a [`Verdict::Contact`]): the steps go on to 22.7.
    pub(crate) contact_done: bool,
}

/// What the steps send besides the message (each a job of the persistent queue, L6 writes its body).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Job {
    /// The Group Sync Request Steps: a `group-sync-request` (member container) to the group's creator.
    SyncRequest { group: GroupRef },
    /// A `group-setup` with no members (creator container) to `to`: the user's group, `to` is not (or no longer) in it.
    EmptySetup { to: String, group_id: u64 },
    /// A `group-leave` (member container) to `to`: the user left `group`.
    Leave { to: String, group: GroupRef },
    /// The creator's answer to `to`: `group-setup` (the stored members), `group-name`, `group-delete-profile-picture`,
    /// each in the creator container, `only: [to]` (C7).
    Answer { to: String, group_id: u64 },
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SyncRequest { .. } => "SyncRequest",
            Self::EmptySetup { .. } => "EmptySetup",
            Self::Leave { .. } => "Leave",
            Self::Answer { .. } => "Answer",
        })
    }
}

/// A message the steps keep: mapped, reflected (when the type says so), then acked.
pub(crate) struct Keep {
    /// The OUTER type (footnote 2): what the reflected `IncomingMessage.type` says.
    pub(crate) outer_type: CspE2eMessageType,
    /// The unpadded `padded-data`: the reflected `IncomingMessage.body`.
    pub(crate) body: Zeroizing<Vec<u8>>,
    /// T-L3: the metadata's created-at (the head's seconds x 1000 without metadata), clamped to the view's now.
    pub(crate) created_at_ms: u64,
    /// `Reflect: Incoming` of the type.
    pub(crate) reflect: bool,
    /// Step 24: an automatic `received` receipt (the type has one and the flags do not hold `0x80`).
    pub(crate) receipt: bool,
    /// The _Acknowledge_ step 2: a `message-ack` is owed (no `0x04`).
    pub(crate) ack: bool,
    /// The _Acknowledge_ step 3: the nonce is marked used.
    pub(crate) nonce: bool,
    /// The jobs the type's steps send (the creator's answer, the empty setup).
    pub(crate) jobs: Vec<Job>,
}

impl fmt::Debug for Keep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Keep")
            .field("outer_type", &self.outer_type)
            .field("body", &self.body.len())
            .field("reflect", &self.reflect)
            .field("receipt", &self.receipt)
            .field("ack", &self.ack)
            .field("nonce", &self.nonce)
            .field("jobs", &self.jobs)
            .finish_non_exhaustive()
    }
}

/// The spec's discard: _Acknowledge_ (the ack unless `0x04`, the nonce where marked) and the jobs of its step.
#[derive(Debug)]
pub(crate) struct Discard {
    pub(crate) rule: &'static str,
    pub(crate) ack: bool,
    pub(crate) nonce: bool,
    pub(crate) jobs: Vec<Job>,
}

/// A held message: nothing written, nothing acked. `group` names a group whose control message is held, for the
/// socket's held set (T-B1).
#[derive(Debug)]
pub(crate) struct Hold {
    pub(crate) rule: &'static str,
    pub(crate) group: Option<GroupRef>,
}

/// The FS rejection (C8): discarded, acked, the nonce marked, and a `Reject { message_id, group_identity,
/// DISABLED_BY_LOCAL }` job to the stored sender under its `session_id`.
pub(crate) struct RejectFs {
    pub(crate) to: String,
    pub(crate) session_id: Vec<u8>,
    pub(crate) message_id: MessageId,
    /// TH6 FIX-B, P-L4: the group the rejected message was sent in (`Encapsulated.group_identity`,
    /// `csp-e2e-fs.proto:873-877`), copied into the `Reject` (`:587`); `None` for a 1:1 message.
    pub(crate) group: Option<GroupRef>,
    pub(crate) ack: bool,
}

impl fmt::Debug for RejectFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RejectFs")
            .field("ack", &self.ack)
            .finish_non_exhaustive()
    }
}

/// The typing indicator: an event and an ephemeral reflect, then the ack when one is owed.
pub(crate) struct Ephemeral {
    pub(crate) outer_type: CspE2eMessageType,
    pub(crate) body: Zeroizing<Vec<u8>>,
    pub(crate) created_at_ms: u64,
    /// TH6 FIX-A, S-L3 = P-L2: the _Acknowledge_ step 2: a `message-ack` is owed (the flags lack `0x04`; a conforming
    /// sender sets `0x02 | 0x04`, `csp.struct.yml:4395-4397`, so this is a non-conforming sender's typing).
    pub(crate) ack: bool,
}

impl fmt::Debug for Ephemeral {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ephemeral")
            .field("outer_type", &self.outer_type)
            .field("body", &self.body.len())
            .field("ack", &self.ack)
            .finish_non_exhaustive()
    }
}

/// A change of the sender's contact that 22.4-22.6 make in a `CONTACT_SYNC` transaction (L5).
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Change {
    /// 22.4.2: a new direct contact from the directory's init, with the nickname (`None`: none given).
    Create {
        init: ContactInit,
        nickname: Option<String>,
    },
    /// 22.5 and 22.6 in ONE update: the nickname (`None`: unchanged; `Some("")`: cleared) and/or the acquaintance level
    /// raised to direct.
    Update {
        identity: String,
        nickname: Option<String>,
        direct: bool,
    },
}

impl fmt::Debug for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Create { nickname, .. } => f
                .debug_struct("Create")
                .field("nickname", &nickname.is_some())
                .finish_non_exhaustive(),
            Self::Update {
                nickname, direct, ..
            } => f
                .debug_struct("Update")
                .field("nickname", &nickname.is_some())
                .field("direct", direct)
                .finish_non_exhaustive(),
        }
    }
}

/// THE VERDICT on one message (module doc).
#[derive(Debug)]
pub(crate) enum Verdict {
    Process(Keep),
    Discard(Discard),
    Hold(Hold),
    RejectFs(RejectFs),
    Ephemeral(Ephemeral),
    /// The sender is unknown to the file and the cache: the directory first (the head waits).
    Lookup,
    /// 22.4-22.6: the contact change first (the head waits).
    Contact(Change),
}

impl Verdict {
    /// The rule name of a discard or a hold (the count line's and D2's word); `None` for the others.
    pub(crate) fn rule(&self) -> Option<&'static str> {
        match self {
            Self::Discard(d) => Some(d.rule),
            Self::Hold(h) => Some(h.rule),
            Self::RejectFs(_) => Some(RULE_FS_REJECTED),
            Self::Process(_) | Self::Ephemeral(_) | Self::Lookup | Self::Contact(_) => None,
        }
    }
}

/// Who sent it, as the Valid Contacts Lookup Steps found it.
enum Sender {
    /// A special contact: its builtin key (note 2: before a stored contact).
    Special([u8; 32]),
    /// A stored contact.
    Stored(Box<d2d_sync::Contact>),
    /// Not stored: the cache's or the directory's init.
    Init(ContactInit),
}

impl Sender {
    fn key(&self) -> Option<[u8; 32]> {
        match self {
            Self::Special(key) => Some(*key),
            Self::Stored(contact) => contact.public_key.as_deref()?.try_into().ok(),
            Self::Init(init) => Some(init.public_key),
        }
    }
}

/// The FS envelope as the steps need it (`csp-e2e-fs.proto` `Envelope`: `session_id` = 1, `encapsulated` = 6 of the
/// `content` oneof). Written by hand, wire-compatible (a message field read as its bytes; the other variants skipped as
/// unknown fields), so the steps need no generated FS type (L4b compiles `csp-e2e-fs.proto` for the `Reject` job).
#[derive(Clone, PartialEq, prost::Message)]
struct FsEnvelope {
    #[prost(bytes = "vec", tag = "1")]
    session_id: Vec<u8>,
    #[prost(bytes = "vec", optional, tag = "6")]
    encapsulated: Option<Vec<u8>>,
}

/// The FS `Encapsulated` as the steps need it (`csp-e2e-fs.proto`: `group_identity` = 6), by hand as [`FsEnvelope`].
#[derive(Clone, PartialEq, prost::Message)]
struct FsEncapsulated {
    #[prost(message, optional, tag = "6")]
    group_identity: Option<common::GroupIdentity>,
}

/// The group an `Encapsulated` message names (P-L4). An `Encapsulated` that does not read names none: the `Reject` is
/// then the 1:1 form (the peer looks the id up in the 1:1 chat).
fn encapsulated_group(encapsulated: &[u8]) -> Option<GroupRef> {
    let identity = FsEncapsulated::decode(encapsulated).ok()?.group_identity?;
    Some(GroupRef {
        creator: identity.creator_identity,
        group_id: identity.group_id,
    })
}

fn discard(rule: &'static str, ack: bool, nonce: bool) -> Verdict {
    Verdict::Discard(Discard {
        rule,
        ack,
        nonce,
        jobs: Vec::new(),
    })
}

fn discard_with(rule: &'static str, ack: bool, nonce: bool, jobs: Vec<Job>) -> Verdict {
    Verdict::Discard(Discard {
        rule,
        ack,
        nonce,
        jobs,
    })
}

fn hold(rule: &'static str, group: Option<GroupRef>) -> Verdict {
    Verdict::Hold(Hold { rule, group })
}

/// What the type's message carries after its group container (the inner struct), and the group it names.
struct Parts<'a> {
    group: Option<GroupRef>,
    inner: &'a [u8],
}

/// THE STEPS for one message (module doc). `Err` only when the view fails (a store error: the caller's close).
pub(crate) fn decide<V: View>(view: &V, input: &Input<'_>) -> Result<Verdict> {
    let flags = input.bytes.get(28).copied().unwrap_or(0);
    let ack = flags & NO_SERVER_ACK == 0;
    let boxed = match open::parse(input.bytes) {
        Ok(boxed) => boxed,
        Err(rule) => return Ok(discard(rule, ack, false)),
    };
    // 2. The nonce was used.
    if view.csp_nonce_used(&boxed.nonce)? {
        return Ok(discard(RULE_NONCE_USED, ack, false));
    }
    // 3. The receiver.
    if boxed.receiver != *view.own().as_bytes() {
        return Ok(discard(RULE_RECEIVER, ack, false));
    }
    // 4-5. The Valid Contacts Lookup Steps.
    let Some(sender_id) = std::str::from_utf8(&boxed.sender)
        .ok()
        .filter(|id| parse::is_identity(id))
    else {
        return Ok(discard(open::RULE_HEAD, ack, false));
    };
    if sender_id == view.own() {
        return Ok(discard(RULE_SENDER_OWN, ack, false));
    }
    let sender = if let Some(key) = directory::is_special(sender_id)
        .then(|| directory::predefined_key(sender_id))
        .flatten()
    {
        Sender::Special(key)
    } else if let Some(contact) = view.contact(sender_id)? {
        Sender::Stored(Box::new(contact))
    } else if let Some(init) = view.cached(sender_id) {
        Sender::Init(init)
    } else {
        match input.looked {
            None => return Ok(Verdict::Lookup),
            Some(Looked::Invalid) => return Ok(discard(RULE_SENDER_INVALID, ack, false)),
            Some(Looked::Valid(init)) => Sender::Init(init.clone()),
        }
    };
    // 6-8. The two opens with the key shared with the sender.
    let Some(key) = sender
        .key()
        .and_then(|k| input.ck.derive_csp_e2e_key(&PublicKey::from(k)))
    else {
        return Ok(discard(open::RULE_UNDECRYPTABLE, ack, false));
    };
    let opened = match open::open(&key, &boxed) {
        Ok(opened) => opened,
        Err(rule) => return Ok(discard(rule, ack, false)),
    };
    drop(key);
    // 9. A type the pinned enum lacks: HELD (C2).
    let Ok(ty) = CspE2eMessageType::try_from(i32::from(opened.outer_type)) else {
        return Ok(hold(RULE_UNKNOWN_TYPE, None));
    };
    let Some(inc) = props::incoming(ty) else {
        // `_INVALID_TYPE`: the body of no type (step 10).
        return Ok(discard(RULE_BODY, ack, false));
    };
    let send = crate::send::props::props(ty);
    let nonce = replay(inc.replay, ty, &opened.data);
    // 10, 12. The FS envelope (no FS session: rejected or ignored, C8).
    if inc.action == Action::Fs {
        let Ok(envelope) = FsEnvelope::decode(opened.data.as_slice()) else {
            return Ok(discard(RULE_BODY, ack, nonce));
        };
        let Some(encapsulated) = envelope.encapsulated.as_deref() else {
            return Ok(discard(RULE_FS_IGNORED, ack, nonce));
        };
        return Ok(match sender {
            Sender::Stored(_) => Verdict::RejectFs(RejectFs {
                to: sender_id.to_owned(),
                group: encapsulated_group(encapsulated),
                session_id: envelope.session_id,
                message_id: boxed.message_id,
                ack,
            }),
            Sender::Special(_) | Sender::Init(_) => discard(RULE_FS_REJECTED, ack, nonce),
        });
    }
    // 10. The group container of a group form.
    let parts = match inc.group {
        GroupForm::No => Parts {
            group: None,
            inner: opened.data.as_slice(),
        },
        GroupForm::Member => match parse::member_container(&opened.data) {
            Ok((group, inner)) => Parts {
                group: Some(group),
                inner,
            },
            Err(_) => return Ok(discard(RULE_BODY, ack, nonce)),
        },
        GroupForm::Creator => match parse::creator_container(&opened.data) {
            Ok((group_id, inner)) => Parts {
                group: Some(GroupRef {
                    creator: sender_id.to_owned(),
                    group_id,
                }),
                inner,
            },
            Err(_) => return Ok(discard(RULE_BODY, ack, nonce)),
        },
    };
    // 13. The metadata's id.
    if let Some(meta) = &opened.metadata
        && meta.message_id != boxed.message_id
    {
        return Ok(discard(RULE_ID_MISMATCH, ack, nonce));
    }
    // 18-19. Blocking: the dedicated steps of a group setup, else the Identity Blocked Steps unless exempt.
    let stored = match &sender {
        Sender::Stored(contact) => Some(&**contact),
        Sender::Special(_) | Sender::Init(_) => None,
    };
    let exempt = send.map_or(Exempt::No, |p| p.exempt_from_blocking);
    let mut blocking = Blocking::new(view)?;
    let blocked = match exempt {
        Exempt::Yes => false,
        Exempt::No => blocking.is_blocked(sender_id, stored)?,
        Exempt::DedicatedSteps => match &parts.group {
            Some(group) if view.group(&group.creator, group.group_id)?.is_some() => false,
            _ => blocking.is_blocked(sender_id, stored)?,
        },
    };
    if blocked {
        return Ok(discard(RULE_BLOCKED, ack, nonce));
    }
    // 20-21. The special contacts.
    if let Sender::Special(_) = sender {
        return Ok(discard(special_rule(sender_id, ty), ack, nonce));
    }
    // 22.1-22.3. The nickname.
    let nickname = || match &opened.metadata {
        Some(meta) => meta.nickname.clone(),
        None if distributes(send.map(|p| p.profile_distribution), ty, parts.inner) => {
            boxed.legacy_nickname()
        }
        None => None,
    };
    // T-M1: group control is decided here, before any contact transaction. TH6 FIX-A, P-L1: from a sender the file does
    // not hold, the spec's 22.4 comes first. `group-name` (`csp.struct.yml:4873`), `group-leave` (`:4936`) and the two
    // group picture types (`:4497`, `:4600`) create no implicit direct contact, so 22.4.1 discards them (with T-B1's
    // knock-on): no T-M1 hold and no Common Group Receive Steps for a stranger (no job a stranger can make the user
    // send). `group-setup` creates one (`:4722`, Desktop's `missingContactHandling: 'create'`): T-M1 decides it first;
    // a hold waits for the phone, whose own run makes the contact; a discard by the setup's own steps comes after 22.4.2
    // in the spec, so the contact is made first (the head waits for its transaction and is decided again, then
    // discarded with the delta applied in its zone). A body that does not decode is step 10's, before 22.4.
    if inc.action == Action::GroupControl {
        if let Sender::Init(init) = &sender
            && !input.contact_done
        {
            if !inc.implicit_direct {
                return Ok(sender_unknown(view, parts.group.as_ref(), ack, nonce));
            }
            return Ok(
                match group_control(view, &mut blocking, ty, sender_id, &parts, ack, nonce)? {
                    Verdict::Discard(d) if d.rule != RULE_BODY => {
                        Verdict::Contact(Change::Create {
                            init: init.clone(),
                            nickname: nickname(),
                        })
                    }
                    other => other,
                },
            );
        }
        return group_control(view, &mut blocking, ty, sender_id, &parts, ack, nonce);
    }
    // 22.4-22.6. The contact.
    if !input.contact_done {
        let nickname = nickname();
        match &sender {
            Sender::Init(init) => {
                if !inc.implicit_direct {
                    return Ok(sender_unknown(view, parts.group.as_ref(), ack, nonce));
                }
                return Ok(Verdict::Contact(Change::Create {
                    init: init.clone(),
                    nickname,
                }));
            }
            Sender::Stored(contact) => {
                if let Some(change) = contact_change(contact, nickname, inc.implicit_direct) {
                    return Ok(Verdict::Contact(change));
                }
            }
            Sender::Special(_) => {}
        }
    }
    // 22.7. The type's steps.
    let jobs = match inc.action {
        Action::Ephemeral => {
            return Ok(Verdict::Ephemeral(Ephemeral {
                outer_type: ty,
                body: opened.data,
                created_at_ms: created_at(&boxed, opened.metadata.as_ref(), view.now_ms()),
                ack,
            }));
        }
        Action::Discard => return Ok(discard(RULE_EMPTY, ack, nonce)),
        Action::SpecialOnly => return Ok(discard(RULE_SPECIAL_ONLY, ack, nonce)),
        Action::CreatorAnswer => {
            let Some(group) = &parts.group else {
                return Ok(discard(RULE_BODY, ack, nonce));
            };
            match creator_answer(view, sender_id, group, ack, nonce)? {
                Ok(jobs) => jobs,
                Err(verdict) => return Ok(verdict),
            }
        }
        Action::Map => match &parts.group {
            Some(group) if inc.group == GroupForm::Member => {
                if let Some(verdict) =
                    common_group_steps(view, &mut blocking, sender_id, group, ack, nonce)?
                {
                    return Ok(verdict);
                }
                Vec::new()
            }
            _ => Vec::new(),
        },
        // Decided above (T-M1, step 12).
        Action::GroupControl | Action::Fs => return Ok(discard(RULE_BODY, ack, nonce)),
    };
    Ok(Verdict::Process(Keep {
        outer_type: ty,
        created_at_ms: created_at(&boxed, opened.metadata.as_ref(), view.now_ms()),
        body: opened.data,
        reflect: inc.reflect_incoming,
        receipt: inc.receipt && flags & NO_RECEIPT == 0,
        ack,
        nonce,
        jobs,
    }))
}

/// Steps 20 and 21: a special contact's own type (`*3MAPUSH`: web session resume, `*3MAW0RK`: work sync) is
/// [`RULE_SPECIAL`] (Kourier runs neither a web nor a work client), another type [`RULE_SPECIAL_TYPE`].
fn special_rule(sender: &str, ty: CspE2eMessageType) -> &'static str {
    let own_type = match sender {
        "*3MAPUSH" => CspE2eMessageType::WebSessionResume,
        _ => CspE2eMessageType::WorkSyncDelta,
    };
    if ty == own_type {
        RULE_SPECIAL
    } else {
        RULE_SPECIAL_TYPE
    }
}

/// T-L3: the metadata's created-at, else the head's seconds x 1000; clamped to `now_ms` (Desktop
/// `incoming-message.ts:1232-1234`).
fn created_at(
    boxed: &Boxed<'_>,
    metadata: Option<&libthreema::common::MessageMetadata>,
    now_ms: u64,
) -> u64 {
    metadata
        .map_or(u64::from(boxed.created_at_s).saturating_mul(1000), |m| {
            m.created_at
        })
        .min(now_ms)
}

/// Whether the steps mark the nonce: the type's `Protect against replay`; a delivery receipt only for a reaction
/// (status 3 or 4; a group receipt's status is after its container).
fn replay(rule: Replay, ty: CspE2eMessageType, data: &[u8]) -> bool {
    match rule {
        Replay::Yes => true,
        Replay::No => false,
        Replay::ByBody => {
            let inner = if ty == CspE2eMessageType::GroupDeliveryReceipt {
                data.get(16..).unwrap_or_default()
            } else {
                data
            };
            matches!(inner.first(), Some(3 | 4))
        }
    }
}

/// 22.2: whether User Profile Distribution was expected for the type (a receipt only as a reaction).
fn distributes(rule: Option<Distribution>, ty: CspE2eMessageType, inner: &[u8]) -> bool {
    match rule {
        Some(Distribution::Yes) => true,
        Some(Distribution::ByBody) => {
            matches!(
                ty,
                CspE2eMessageType::DeliveryReceipt | CspE2eMessageType::GroupDeliveryReceipt
            ) && matches!(inner.first(), Some(3 | 4))
        }
        Some(Distribution::No) | None => false,
    }
}

/// 22.5 and 22.6 for a stored contact: the nickname when given and different (a stored `None` reads as the empty
/// name), the level raised to direct when the type creates a direct contact and it is not direct (a contact that
/// states no level counts as direct, as the follower's mapper and the blocked steps read it).
fn contact_change(
    contact: &d2d_sync::Contact,
    nickname: Option<String>,
    implicit_direct: bool,
) -> Option<Change> {
    use d2d_sync::contact::AcquaintanceLevel;
    let direct = implicit_direct
        && contact
            .acquaintance_level
            .is_some_and(|level| level != AcquaintanceLevel::Direct as i32);
    let nickname =
        nickname.filter(|name| contact.nickname.as_deref().unwrap_or_default() != name.as_str());
    (direct || nickname.is_some()).then(|| Change::Update {
        identity: contact.identity.clone(),
        nickname,
        direct,
    })
}

/// 22.4.1 (`csp.struct.yml:770-773`), with the knock-on (T-B1): a type that creates no implicit direct contact from a
/// sender the file does not hold is discarded and acked, unless its group counts as held (a held control message of
/// it, or a blind hold on the socket, S-B1): then it waits for the phone (the member a held setup added is no contact).
/// TH6 re-check R-L2: under a BLIND hold a 1:1 message waits too (a reaction, edit or delete of a message held before
/// the steps placed it: a newer type of that sender may make the contact the phone's run keeps this one for); the
/// word is the same, `group-held-before` (a redelivery costs nothing, a lost message cannot be had back).
fn sender_unknown<V: View>(view: &V, group: Option<&GroupRef>, ack: bool, nonce: bool) -> Verdict {
    if view.blind() || group.is_some_and(|group| view.held_group(group)) {
        return hold(RULE_GROUP_HELD_BEFORE, None);
    }
    discard(RULE_SENDER_UNKNOWN, ack, nonce)
}

/// Whether `identity` is the creator or a stored member of `group` (the list holds neither the creator nor the user).
fn is_in(group: &d2d_sync::Group, key: &GroupRef, identity: &str) -> bool {
    identity == key.creator
        || group
            .member_identities
            .as_ref()
            .is_some_and(|m| m.identities.iter().any(|i| i == identity))
}

/// THE COMMON GROUP RECEIVE STEPS (`csp.struct.yml:2279-2321`) with the knock-on (T-B1): `Some` is the verdict of a
/// discard (or, for a held group, its hold); `None`: the message goes on.
fn common_group_steps<V: View>(
    view: &V,
    blocking: &mut Blocking<'_>,
    sender: &str,
    key: &GroupRef,
    ack: bool,
    nonce: bool,
) -> Result<Option<Verdict>> {
    let held = view.held_group(key);
    let knock = |verdict: Verdict| {
        if held {
            hold(RULE_GROUP_HELD_BEFORE, None)
        } else {
            verdict
        }
    };
    let own = view.own();
    let Some(group) = view.group(&key.creator, key.group_id)? else {
        return Ok(Some(knock(unknown_group(view, blocking, key, ack, nonce)?)));
    };
    if !is_member(&group) {
        let job = if key.creator == own {
            Job::EmptySetup {
                to: sender.to_owned(),
                group_id: key.group_id,
            }
        } else {
            Job::Leave {
                to: sender.to_owned(),
                group: key.clone(),
            }
        };
        return Ok(Some(knock(discard_with(
            RULE_GROUP_LEFT,
            ack,
            nonce,
            vec![job],
        ))));
    }
    if !is_in(&group, key, sender) {
        let jobs = if key.creator == own {
            vec![Job::EmptySetup {
                to: sender.to_owned(),
                group_id: key.group_id,
            }]
        } else {
            sync_request(view, key)
        };
        return Ok(Some(knock(discard_with(
            RULE_GROUP_NOT_MEMBER,
            ack,
            nonce,
            jobs,
        ))));
    }
    Ok(None)
}

/// The Common Group Receive Steps' step 2 (and `group-leave`'s step 3) for a group the file does not hold.
fn unknown_group<V: View>(
    view: &V,
    blocking: &mut Blocking<'_>,
    key: &GroupRef,
    ack: bool,
    nonce: bool,
) -> Result<Verdict> {
    if key.creator == view.own() {
        return Ok(discard(RULE_GROUP_UNKNOWN_OWN, ack, nonce));
    }
    let creator = view.contact(&key.creator)?;
    if blocking.is_blocked(&key.creator, creator.as_ref())? {
        return Ok(discard(RULE_GROUP_UNKNOWN_BLOCKED, ack, nonce));
    }
    Ok(discard_with(
        RULE_GROUP_UNKNOWN,
        ack,
        nonce,
        sync_request(view, key),
    ))
}

/// The Group Sync Request Steps: a job to the creator unless the user is the creator or one went within the hour.
fn sync_request<V: View>(view: &V, key: &GroupRef) -> Vec<Job> {
    if key.creator == view.own() || view.sync_request_throttled(key) {
        return Vec::new();
    }
    vec![Job::SyncRequest { group: key.clone() }]
}

/// T-M1: group control against the stored group: no change is the spec's discard, a real change is HELD for the phone
/// (`group-sync-needed`, with the group for the socket's held set); a group already held holds every control message.
fn group_control<V: View>(
    view: &V,
    blocking: &mut Blocking<'_>,
    ty: CspE2eMessageType,
    sender: &str,
    parts: &Parts<'_>,
    ack: bool,
    nonce: bool,
) -> Result<Verdict> {
    use CspE2eMessageType as T;
    let Some(key) = &parts.group else {
        return Ok(discard(RULE_BODY, ack, nonce));
    };
    let change = || hold(RULE_GROUP_SYNC_NEEDED, Some(key.clone()));
    let no_change = || discard(RULE_GROUP_NO_CHANGE, ack, nonce);
    if view.held_group(key) {
        return Ok(hold(RULE_GROUP_HELD_BEFORE, Some(key.clone())));
    }
    let own = view.own();
    match ty {
        T::GroupSetup => {
            let (members, rest) = parts.inner.as_chunks::<8>();
            if !rest.is_empty() {
                return Ok(discard(RULE_BODY, ack, nonce));
            }
            let mut wanted = BTreeSet::new();
            for member in members {
                match std::str::from_utf8(member) {
                    Ok(id) if parse::is_identity(id) => {
                        if id != sender {
                            wanted.insert(id.to_owned());
                        }
                    }
                    _ => return Ok(discard(RULE_BODY, ack, nonce)),
                }
            }
            let group = view.group(&key.creator, key.group_id)?;
            if let Some(group) = &group {
                if !is_member(group) && wanted.is_empty() {
                    return Ok(no_change());
                }
                if is_member(group) {
                    let mut current: BTreeSet<String> = group
                        .member_identities
                        .as_ref()
                        .map(|m| m.identities.iter().cloned().collect())
                        .unwrap_or_default();
                    current.remove(sender);
                    current.insert(own.to_owned());
                    if current == wanted {
                        return Ok(no_change());
                    }
                }
            } else if !wanted.contains(own) {
                return Ok(discard(RULE_SETUP_NOT_MEMBER, ack, nonce));
            }
            Ok(change())
        }
        T::GroupLeave => {
            if sender == key.creator {
                return Ok(discard(RULE_LEAVE_CREATOR, ack, nonce));
            }
            match view.group(&key.creator, key.group_id)? {
                Some(group) if is_member(&group) => {
                    if is_in(&group, key, sender) {
                        Ok(change())
                    } else {
                        Ok(no_change())
                    }
                }
                _ => unknown_group(view, blocking, key, ack, nonce),
            }
        }
        T::GroupName | T::GroupSetProfilePicture | T::GroupDeleteProfilePicture => {
            if let Some(verdict) = common_group_steps(view, blocking, sender, key, ack, nonce)? {
                return Ok(verdict);
            }
            let Some(group) = view.group(&key.creator, key.group_id)? else {
                return Ok(change());
            };
            let same = match ty {
                T::GroupName => group
                    .name
                    .as_deref()
                    .is_some_and(|name| name.as_bytes() == parts.inner),
                // P-M1's SET HALF stays a HOLD: the spec's "no change" compares the DOWNLOADED picture
                // (`csp.struct.yml:4541-4542`), which Kourier cannot fetch before TH8, so only the same blob id reads
                // as no change. [U: real phone, RT1] whether a creator uploads its picture again at every periodic
                // resync (the spec's Active Group State Resync step 9, `:1554-1556`; Desktop's
                // `sendGroupSetProfilePicture` encrypts and uploads each time): then every weekly resync of a group
                // WITH a picture is held `group-sync-needed` until the phone leads (a LIMIT until TH8).
                T::GroupSetProfilePicture => {
                    let blob_id = parts.inner.get(..16);
                    blob_id.is_some() && stored_blob_id(&group) == blob_id
                }
                _ => no_picture(&group),
            };
            Ok(if same { no_change() } else { change() })
        }
        _ => Ok(discard(RULE_BODY, ack, nonce)),
    }
}

/// The blob id of the stored group's picture, when the file states one.
fn stored_blob_id(group: &d2d_sync::Group) -> Option<&[u8]> {
    match group.profile_picture.as_ref()?.image.as_ref()? {
        common::delta_image::Image::Updated(image) => Some(image.blob.as_ref()?.id.as_slice()),
        common::delta_image::Image::Removed(_) => None,
    }
}

/// TH6 FIX-A, P-M1 (the DELETE half): the stored group has NO picture: the field absent (Desktop's
/// `getD2dGroupSyncCreate` leaves it out for a group made without one), a `DeltaImage` without an image, or one
/// EXPLICITLY removed. The receive side's "`group.profile-picture` is not defined (i.e. no change)"
/// (`csp.struct.yml:4640-4641`): a creator's periodic `group-delete-profile-picture` of such a group is no change. A
/// wrong "no change" here needs a stored group that lacks a picture the phone has, which the join and the follower's
/// syncs do not make [I]. The creator's ANSWER keeps T-M3's stricter [`picture_removed`].
fn no_picture(group: &d2d_sync::Group) -> bool {
    !matches!(
        group
            .profile_picture
            .as_ref()
            .and_then(|p| p.image.as_ref()),
        Some(common::delta_image::Image::Updated(_))
    )
}

/// T-M3: the stored group's picture is EXPLICITLY removed (`DeltaImage { removed }`). `None` (never stated) and an
/// updated picture are not "no picture" (for the creator's answer, where a wrong delete removes a real picture on the
/// members' devices).
fn picture_removed(group: &d2d_sync::Group) -> bool {
    matches!(
        group
            .profile_picture
            .as_ref()
            .and_then(|p| p.image.as_ref()),
        Some(common::delta_image::Image::Removed(_))
    )
}

/// `group-sync-request` received (`csp.struct.yml:5035-5059`), C7 and T-M3: `Ok(jobs)` goes on to reflect and ack;
/// `Err` is the verdict of a discard or a hold.
fn creator_answer<V: View>(
    view: &V,
    sender: &str,
    key: &GroupRef,
    ack: bool,
    nonce: bool,
) -> Result<std::result::Result<Vec<Job>, Verdict>> {
    let Some(group) = view.group(&key.creator, key.group_id)? else {
        return Ok(Err(discard(RULE_GROUP_UNKNOWN, ack, nonce)));
    };
    if key.creator != view.own() {
        return Ok(Err(discard(RULE_SYNC_NOT_CREATOR, ack, nonce)));
    }
    if view.answer_throttled(key.group_id, sender) {
        return Ok(Err(discard(RULE_SYNC_THROTTLED, ack, nonce)));
    }
    if !is_member(&group) || !is_in(&group, key, sender) {
        return Ok(Ok(vec![Job::EmptySetup {
            to: sender.to_owned(),
            group_id: key.group_id,
        }]));
    }
    if !picture_removed(&group) {
        return Ok(Err(hold(RULE_CREATOR_PICTURE, None)));
    }
    Ok(Ok(vec![Job::Answer {
        to: sender.to_owned(),
        group_id: key.group_id,
    }]))
}

#[cfg(test)]
mod tests;
