//! What ONE reflected envelope becomes (TH5 A6): [`map`] reads the classification ([`classify`]) and builds
//! the contract's events, or says to keep the plaintext ([`Mapped::deferred`]) or to drop the envelope
//! ([`Mapped::dropped`], a rule name). Pure over a [`View`] of what the file knows (which contacts and groups
//! exist): no clock, no logging, nothing written.
//!
//! TH5a MAPS: a TEXT in a 1:1 chat or a group, incoming (from a contact) or outgoing (from our own other
//! device), and the `Unsupported` kinds (a poll, the legacy media: `Body::Unsupported` with the raw body).
//! TH5b B1 adds the LOCATION (`Body::Location`) and the FILE (`Body::Media` with one attachment, or
//! `Body::Sticker`), both forms of each (decisions R3, R4; `body.rs` reads them): a body that is no location or
//! no file is an Unsupported `location` / `file` with the raw body, never dropped. TH5b B2 adds the three MODIFIERS
//! (decisions R6, R7, R8; [`modifier`]): an edit (`Edited`), a delete (`Deleted`) and a reaction (`Reaction`),
//! each of a message in the conversation `place` finds, 1:1 or group, incoming or from our own other device; they
//! make no message and no row of their own. TH5b B3 adds the DELIVERY RECEIPTS (decision R9; [`receipt`]: the
//! peer's `Receipts` of our messages, our own read, the deprecated 👍 / 👎 as a reaction pair, 1:1 and group),
//! the SENT and READ UPDATES (R10, R11; [`sent_update`], [`read_update`]: `OwnSendState` and our own `Receipts`,
//! the parts the file has no conversation for skipped and counted) and the TYPING of an ephemeral envelope
//! (R12; [`typing`], called by `reflect::open_ephemeral`, never by [`map`]). Everything else follows the
//! table: DEFERRED kinds keep the plaintext under `MAP_VERSION`, DROPPED ones carry their rule. The CONTACT and GROUP SYNC deltas (TH5 A7a) are [`sync`]'s: they read the stored BLOB
//! through the [`View`] and return the new one as a [`Write`] the zone makes (so a later envelope of the
//! same batch reads it), with the contact and group events and the group notices ([`notice`]). TH5b B4
//! (decision R13) adds the SETTINGS, PROFILE and DISTRIBUTION LIST deltas there in the same form. TH5b B5
//! (R1): [`conversation_of`] names the conversation of a STORED envelope (a `deferred` row) for the user's
//! Clear Chat, by [`place`]'s rules without the file's reads.
//!
//! THE FIELDS of a message (brief §2.5): `conv` = `Direct(threema:<sender>)` (incoming 1:1),
//! `Direct(threema:<ConversationId.contact>)` (outgoing 1:1) or `join::events::group_key` of the container's
//! header (incoming) / of `ConversationId.group` (outgoing; the container must name the same group, else
//! DROPPED `group-mismatch`, `md-d2d.proto`); `local` = `msg_id::format` of the message id; `origin` =
//! `Remote { author: threema:<sender>, device: None }` or `OwnOtherDevice { device: None }` (L10 option B: no
//! device id reaches the contract); `sent_at` = `created_at`; `server_at` = the `Reflected` timestamp (C10);
//! `received_at` = the zone's local time; `catch_up` as the connection says (before Dry); a quote becomes
//! `reply_to` with an unknown-author target in the same conversation (Threema's quote names no author).
//!
//! DROPPED (decision M2, as Desktop `reflected-incoming-message.ts` / `reflected-outgoing-message.ts`): a
//! sender that is no stored contact (`sender-unknown`), an outgoing 1:1 message to no stored contact
//! (`contact-unknown`), a group the file does not hold (`group-unknown`); and the spec's validity rules: a
//! conversation that does not match the message type (`conversation-mismatch`, the five "reject" cases of
//! Desktop's `reflected-message.spec.ts`), a container that names another group than the envelope
//! (`group-mismatch`), a malformed body (`body.rs` rule names), a `created_at` over `i64::MAX`.
//!
//! C9, AN OWN MESSAGE (an `OutgoingMessage` reflection) is `Sending` (`initial_out_state`): the core derives an
//! own message's state from its floor merged forward with the aggregate of the per-recipient send states
//! (`ingest/message.rs` `recompute_out`, `merge_out`; `ingest/modifier.rs` `send_state`), so TH5b's
//! `OutgoingMessageUpdate::Sent` -> `OwnSendState { per_recipient: Sent }` (B3, R10) moves the row to `Sent`
//! (the aggregate of `Sent` for every recipient ranks above the floor's `Sending`) and a later delivery receipt
//! moves it on (`Delivered`, then `Read`, once every recipient said so). A TH5a file's `sent` rows (`deferred`,
//! `MAP_VERSION` 1) are mapped by the start's re-map. FIX-P, P-M1: the exception is a message into a NOTES
//! group (the user's own group with nobody else, `csp.struct.yml`): the phone reflects its Sent update with no
//! receivers, there is no recipient the core could move the row by, so the message is `Sent { server_at }` at
//! once ([`is_notes_group`]) and the group's Sent update is skipped quietly ([`sent_update`]).

use kourier_backend_api::{BackendEvent, IncomingMessage};
use kourier_model::{
    BackendBlob, Body, ConvKey, ConversationPatch, MsgTarget, Origin, OutgoingState, ReceiptKind,
    RecipientState, ReplyTo, TargetAuthor,
};
use libthreema::common::MessageId;
use libthreema::protobuf::common::CspE2eMessageType as Ty;
use libthreema::protobuf::d2d;
use libthreema::protobuf::d2d::conversation_id::Id;
use libthreema::protobuf::d2d::envelope::Content;

use super::body::{self, GroupRef};
use libthreema::protobuf::d2d_sync;

use super::classify::{self, Class};
use super::sync;
use crate::error::Result;
use crate::join::events::{group_key, is_direct, is_visible, one_to_one_patch, user};
use crate::msg_id;

/// The version `Body::Unsupported`'s `raw` is written in (`BackendBlob::v`): the body bytes as received.
const RAW_VERSION: u16 = 1;

/// The Unsupported hint of a file message whose JSON is unusable (R3).
const HINT_FILE: &str = "file";
/// The Unsupported hint of a location message whose body is unusable (R4).
const HINT_LOCATION: &str = "location";

/// Rule: a sender or a contact that is no Threema ID.
const RULE_SENDER_INVALID: &str = "sender-invalid";
/// Rule: a sender the file has no contact for (decision M2).
const RULE_SENDER_UNKNOWN: &str = "sender-unknown";
/// Rule: an own message to a contact the file does not have.
const RULE_CONTACT_UNKNOWN: &str = "contact-unknown";
/// Rule: a group the file does not have (decision M2).
const RULE_GROUP_UNKNOWN: &str = "group-unknown";
/// Rule: a group whose key cannot be made (an ID the contract refuses).
const RULE_GROUP_KEY: &str = "group-key";
/// Rule (round A, L-3): a message for a stored group that is not visible (M1: the user was kicked or left
/// and nobody is listed; the group is never stated), dropped as Desktop drops a message of a group we left.
const RULE_GROUP_NOT_VISIBLE: &str = "group-not-visible";
/// Rule: the conversation kind does not match the message type (Desktop's "conversation type mismatch").
const RULE_CONVERSATION_MISMATCH: &str = "conversation-mismatch";
/// Rule: an outgoing message without a (valid) conversation.
const RULE_CONVERSATION_INVALID: &str = "conversation-invalid";
/// Rule: the container names another group than the envelope (`md-d2d.proto`, `OutgoingMessage.conversation`).
const RULE_GROUP_MISMATCH: &str = "group-mismatch";
/// Rule: a `created_at` that does not fit a signed 64-bit time.
const RULE_CREATED_AT: &str = "created-at";
/// Rule (a belt): a kind classified Mapped that has no map arm (the table test keeps it unreachable).
const RULE_NOT_MAPPED: &str = "not-mapped";
/// Rule (R9): a 1:1 delivery receipt whose status is none of 1-4.
const RULE_RECEIPT_STATUS: &str = "receipt-status";
/// Rule (R9, the spec's "log a notice, discard"): a group delivery receipt that is no reaction (status 3 / 4).
const RULE_GROUP_RECEIPT: &str = "group-receipt";
/// Skip (R10, R11): an update of a message update envelope whose conversation the file does not have (an
/// unknown contact or group, a group that is not shown), that names no update, or whose time does not fit.
/// The envelope's other updates are stated; the skips are counted. (FIX-P: a LIST update is deferred, not
/// skipped, P-L1; a NOTES group's is [`classify::RULE_SENT_NOTES_GROUP`], quiet, P-M1.)
const RULE_UPDATE_UNKNOWN: &str = "update-unknown";
/// Typing (R12, DEBUG only): an incoming typing indicator whose body is not ONE byte 0 or 1.
pub(super) const RULE_TYPING_BYTE: &str = "typing-byte";
/// Typing (R12, DEBUG only): a typing indicator whose sender is no Threema ID.
pub(super) const RULE_TYPING_SENDER: &str = "typing-sender";
/// Typing (R12, DEBUG only): an ephemeral envelope that is no INCOMING typing indicator (an own other device's
/// typing included: the spec reflects none, and it would state nothing).
pub(super) const RULE_EPHEMERAL_KIND: &str = "ephemeral-kind";

/// The deprecated reactions of a delivery receipt (`csp.struct.yml` `delivery-receipt` status 3 and 4).
const THUMBS_UP: &str = "\u{1f44d}";
const THUMBS_DOWN: &str = "\u{1f44e}";
/// How long a typing indicator shows without a refresh (`csp.struct.yml` `typing-indicator`: "for the next
/// 15s").
const TYPING_TTL: std::time::Duration = std::time::Duration::from_secs(15);

/// What the file knows: the lookups behind decision M2 and the BLOBs a sync delta applies to. Reads only;
/// they may fail like any store read. Inside the zone they see the writes of the batch's earlier envelopes.
pub(crate) trait View {
    /// Whether the file holds this contact.
    fn contact(&self, identity: &str) -> Result<bool>;
    /// Whether the file holds this group.
    fn group(&self, creator: &str, group_id: u64) -> Result<bool>;
    /// The stored `d2d_sync.Contact` of `identity`.
    fn stored_contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>>;
    /// The stored `d2d_sync.Group` of (`creator`, `group_id`).
    fn stored_group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>>;
    /// Whether the stored settings block `identity`.
    fn blocked(&self, identity: &str) -> Result<bool>;
    /// How many contacts and groups the file holds.
    fn sync_counts(&self) -> Result<(u64, u64)>;
    /// The stored `d2d_sync.Settings` (TH5b B4, R13; `None`: the file has no settings row).
    fn stored_settings(&self) -> Result<Option<d2d_sync::Settings>>;
    /// The stored `d2d_sync.UserProfile` (`None`: the file has no profile row).
    fn stored_profile(&self) -> Result<Option<d2d_sync::UserProfile>>;
    /// The stored `d2d_sync.DistributionList` of `id`.
    fn stored_list(&self, id: u64) -> Result<Option<d2d_sync::DistributionList>>;
    /// How many distribution lists the file holds.
    fn list_count(&self) -> Result<u64>;
}

/// A BLOB the zone writes for a sync delta (the WHOLE new message, L10's prost re-encode).
pub(crate) enum Write {
    Contact(Box<d2d_sync::Contact>),
    Group {
        creator: String,
        group_id: u64,
        sync: Box<d2d_sync::Group>,
    },
    GroupDeleted {
        creator: String,
        group_id: u64,
    },
    /// TH5b B4 (R13): the one settings row.
    Settings(Box<d2d_sync::Settings>),
    /// The one profile row.
    Profile(Box<d2d_sync::UserProfile>),
    /// A distribution list's row (its id is the message's `distribution_list_id`).
    List(Box<d2d_sync::DistributionList>),
    ListDeleted {
        id: u64,
    },
}

impl std::fmt::Debug for Write {
    /// The kind only: names and IDs never reach a printed line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Contact(_) => "Write::Contact",
            Self::Group { .. } => "Write::Group",
            Self::GroupDeleted { .. } => "Write::GroupDeleted",
            Self::Settings(_) => "Write::Settings",
            Self::Profile(_) => "Write::Profile",
            Self::List(_) => "Write::List",
            Self::ListDeleted { .. } => "Write::ListDeleted",
        })
    }
}

/// What the map needs besides the envelope.
pub(crate) struct Cx<'a> {
    /// The own Threema ID (text): which mention is ours.
    pub(crate) own: &'a str,
    /// The zone's local time (ms): `received_at`.
    pub(crate) now_ms: i64,
    /// The mediator's `Reflected.timestamp` (ms): `server_at` (C10).
    pub(crate) server_at: u64,
    /// The envelope came before the connection's Dry.
    pub(crate) catch_up: bool,
    /// The first 8 bytes of the envelope's hashed D2D nonce: a notice's stable id (`notice::notice_id`).
    pub(crate) notice_tag: [u8; 8],
    /// The start's RE-MAP of a deferred row (round B, M-3 = S3): H5's bring-back is NOT run. It is decided
    /// at arrival only: the re-map runs after every later sync the batch path applied, so an old message
    /// must not un-archive a chat the person archived on the phone after it (the phone's later state is
    /// the truth). TH5b, where `unarchives` grew (files, locations), keeps this.
    pub(crate) remap: bool,
}

/// What an envelope became.
#[derive(Debug, Default)]
pub(crate) struct Mapped {
    /// The events, in the order of ONE `core_outbox` row (a kill cannot split them). Empty: none.
    pub(crate) events: Vec<BackendEvent>,
    /// Keep the plaintext under this kind and `MAP_VERSION` (acked; a later build maps it).
    pub(crate) deferred: Option<&'static str>,
    /// Acked and dropped, by this rule (a counted line).
    pub(crate) dropped: Option<&'static str>,
    /// Applied, with a counted WARN by this rule (H6 `contact-key-change`).
    pub(crate) warned: Option<&'static str>,
    /// The BLOBs the zone writes (in order) before the next envelope is mapped.
    pub(crate) writes: Vec<Write>,
    /// Parts of a message update envelope that were skipped, by rule and count, each rule once (R10, R11: the
    /// envelope's other parts are its events; one WARN per rule and batch, DEBUG for a quiet rule such as
    /// P-M1's `sent-notes-group`).
    pub(crate) skipped: Vec<(&'static str, u32)>,
}

impl Mapped {
    pub(super) fn events(events: Vec<BackendEvent>) -> Self {
        Self {
            events,
            ..Self::default()
        }
    }

    fn deferred(kind: &'static str) -> Self {
        Self {
            deferred: Some(kind),
            ..Self::default()
        }
    }

    pub(super) fn dropped(rule: &'static str) -> Self {
        Self {
            dropped: Some(rule),
            ..Self::default()
        }
    }
}

/// Which side a message is on.
#[derive(Clone, Copy)]
enum Dir<'a> {
    /// A contact's message (the sender's ID).
    In(&'a str),
    /// Our own other device's message.
    Out(Option<&'a d2d::ConversationId>),
}

/// One message envelope's common fields.
struct Msg<'a> {
    dir: Dir<'a>,
    message_id: u64,
    created_at: u64,
    ty: i32,
    body: &'a [u8],
}

/// What `envelope` becomes (module doc). `Err` only when a read of the file fails.
pub(crate) fn map(envelope: &d2d::Envelope, cx: &Cx<'_>, view: &dyn View) -> Result<Mapped> {
    let class = classify::classify(envelope.content.as_ref());
    match class {
        Class::Deferred(kind) => Ok(Mapped::deferred(kind)),
        Class::Ephemeral => Ok(Mapped::default()),
        Class::Dropped(rule) => Ok(Mapped::dropped(dropped_rule(envelope, rule))),
        Class::Mapped | Class::Notice | Class::Unsupported(_) => match &envelope.content {
            Some(Content::IncomingMessage(m)) => message(
                &Msg {
                    dir: Dir::In(&m.sender_identity),
                    message_id: m.message_id,
                    created_at: m.created_at,
                    ty: m.r#type,
                    body: &m.body,
                },
                class,
                cx,
                view,
            ),
            Some(Content::OutgoingMessage(m)) => message(
                &Msg {
                    dir: Dir::Out(m.conversation.as_ref()),
                    message_id: m.message_id,
                    created_at: m.created_at,
                    ty: m.r#type,
                    body: &m.body,
                },
                class,
                cx,
                view,
            ),
            Some(Content::ContactSync(c)) => sync::contact_sync(c, cx, view),
            Some(Content::GroupSync(g)) => sync::group_sync(g, cx, view),
            Some(Content::OutgoingMessageUpdate(u)) => sent_update(u, cx, view),
            Some(Content::IncomingMessageUpdate(u)) => read_update(u, cx, view),
            // TH5b B4 (R13): the settings, the user's profile and the distribution lists.
            Some(Content::SettingsSync(s)) => sync::settings_sync(s, view),
            Some(Content::UserProfileSync(p)) => sync::profile_sync(p, cx, view),
            Some(Content::DistributionListSync(l)) => sync::list_sync(l, view),
            // Every other kind's row is Deferred or Dropped (the table test holds it); a belt.
            Some(Content::MdmParameterSync(_)) | None => Ok(Mapped::dropped(RULE_NOT_MAPPED)),
        },
    }
}

/// The rule a dropped envelope is counted under: a group control message from a device that speaks V0.1 (or
/// does not say) is the spec's "the group is going to desync" warning, not the quiet V0.2+ path.
fn dropped_rule(envelope: &d2d::Envelope, rule: &'static str) -> &'static str {
    let is_message = matches!(
        envelope.content,
        Some(Content::IncomingMessage(_) | Content::OutgoingMessage(_))
    );
    // `protocol_version` 0 = unspecified = V0.1 (`md-d2d.proto`).
    if is_message && rule == "group-control" && envelope.protocol_version <= 1 {
        classify::RULE_GROUP_DESYNC
    } else {
        rule
    }
}

/// Whether the type's body is wrapped in a `group-member-container` (the types this build reads; R15: every
/// group type TH5b maps joins this list in its chunk: B1 the location and the file, B2 the edit, the delete and
/// the reaction, B3 the group delivery receipt).
fn is_group_form(ty: i32) -> bool {
    matches!(
        Ty::try_from(ty),
        Ok(Ty::GroupText
            | Ty::GroupPollSetup
            | Ty::GroupImage
            | Ty::GroupAudio
            | Ty::GroupVideo
            | Ty::GroupLocation
            | Ty::GroupFile
            | Ty::GroupEditMessage
            | Ty::GroupDeleteMessage
            | Ty::GroupReaction
            | Ty::GroupDeliveryReceipt)
    )
}

/// Whether the type is a delivery receipt (B3, R9), 1:1 or group.
fn is_receipt(ty: i32) -> bool {
    matches!(
        Ty::try_from(ty),
        Ok(Ty::DeliveryReceipt | Ty::GroupDeliveryReceipt)
    )
}

/// Whether the type is a modifier B2 maps: an edit, a delete or a reaction, 1:1 or group.
fn is_modifier(ty: i32) -> bool {
    matches!(
        Ty::try_from(ty),
        Ok(Ty::EditMessage
            | Ty::GroupEditMessage
            | Ty::DeleteMessage
            | Ty::GroupDeleteMessage
            | Ty::Reaction
            | Ty::GroupReaction)
    )
}

/// Where a message goes: its conversation and, for a group, the container's inner data.
struct Placed<'a> {
    conv: ConvKey,
    inner: &'a [u8],
    in_group: bool,
    /// Whose stored BLOB holds the chat's visibility (H5).
    owner: Owner,
}

/// The stored record of a chat: the contact of a 1:1 chat, or the group.
enum Owner {
    Contact(String),
    Group { creator: String, group_id: u64 },
}

/// Whether a message of CSP type `ty` brings an ARCHIVED chat back (H5, his QC): the spec's "Unarchive: Yes"
/// (`csp.struct.yml`) for the types that reach [`message`]: text, location and file, the deprecated image /
/// audio / video and the poll setup, 1:1 and group (R15; a reaction, an edit, a delete, a receipt, typing, a
/// vote and a call are "No").
fn unarchives(ty: i32) -> bool {
    matches!(
        Ty::try_from(ty),
        Ok(Ty::Text
            | Ty::GroupText
            | Ty::Location
            | Ty::GroupLocation
            | Ty::File
            | Ty::GroupFile
            | Ty::DeprecatedImage
            | Ty::GroupImage
            | Ty::DeprecatedAudio
            | Ty::GroupAudio
            | Ty::DeprecatedVideo
            | Ty::GroupVideo
            | Ty::PollSetup
            | Ty::GroupPollSetup)
    )
}

/// H5 (his QC, as Threema's own rule): a new message in an ARCHIVED chat brings it back. Returns the
/// `ConversationUpsert { archived: Some(false) }` that goes FIRST in the message's row and the stored BLOB
/// with its visibility set to NORMAL, which the zone writes in the same zone (so a Rebuild and the next
/// sync delta start from the chat as it is now); `None` when the chat is not archived. Applied to an own
/// other device's message too (the phone unarchives what it sends in).
fn bring_back(
    owner: &Owner,
    conv: &ConvKey,
    view: &dyn View,
) -> Result<Option<(BackendEvent, Write)>> {
    const ARCHIVED: Option<i32> = Some(d2d_sync::ConversationVisibility::Archived as i32);
    const NORMAL: Option<i32> = Some(d2d_sync::ConversationVisibility::Normal as i32);
    let write = match owner {
        Owner::Contact(identity) => match view.stored_contact(identity)? {
            Some(mut contact) if contact.conversation_visibility == ARCHIVED => {
                contact.conversation_visibility = NORMAL;
                Write::Contact(Box::new(contact))
            }
            _ => return Ok(None),
        },
        Owner::Group { creator, group_id } => match view.stored_group(creator, *group_id)? {
            Some(mut group) if group.conversation_visibility == ARCHIVED => {
                group.conversation_visibility = NORMAL;
                Write::Group {
                    creator: creator.clone(),
                    group_id: *group_id,
                    sync: Box::new(group),
                }
            }
            _ => return Ok(None),
        },
    };
    let mut patch = ConversationPatch::new(conv.clone());
    patch.archived = Some(false);
    Ok(Some((BackendEvent::ConversationUpsert(patch), write)))
}

/// What a message's body became.
struct Made {
    body: Body,
    reply_to: Option<ReplyTo>,
    mentions_me: bool,
    /// The event keeps the message's whole body in `raw` (an Unsupported body).
    raw: bool,
}

impl Made {
    /// An Unsupported body of `hint` (the viewmodel's words for it), the raw body kept.
    fn unsupported(hint: &str) -> Self {
        Self {
            body: Body::Unsupported {
                kind_hint: hint.to_owned(),
                summary: None,
            },
            reply_to: None,
            mentions_me: false,
            raw: true,
        }
    }

    /// A body with no quote.
    fn plain(body: Body, mentions_me: bool) -> Self {
        Self {
            body,
            reply_to: None,
            mentions_me,
            raw: false,
        }
    }
}

/// The body of a mapped message: its text (the quote and mentions of TH5a, R5's markup), its file (R3) or
/// its location (R4). `Err` is the rule that drops the message.
fn content(
    msg: &Msg<'_>,
    placed: &Placed<'_>,
    class: Class,
    cx: &Cx<'_>,
) -> std::result::Result<Made, &'static str> {
    if let Class::Unsupported(hint) = class {
        return Ok(Made::unsupported(hint));
    }
    match Ty::try_from(msg.ty) {
        Ok(Ty::Text | Ty::GroupText) => {
            let parsed = body::text(
                placed.inner,
                MessageId(msg.message_id),
                cx.own,
                placed.in_group,
            )?;
            let reply_to = parsed.quoted.map(|quoted| ReplyTo {
                target: target(&placed.conv, quoted),
                author: None,
                snapshot: None,
            });
            Ok(Made {
                reply_to,
                ..Made::plain(Body::Text(parsed.text), parsed.mentions_me)
            })
        }
        Ok(Ty::File | Ty::GroupFile) => {
            Ok(match body::file(placed.inner, cx.own, placed.in_group) {
                Some(file) if file.sticker => {
                    Made::plain(Body::Sticker(Box::new(file.attachment)), false)
                }
                Some(file) => Made::plain(
                    Body::Media {
                        attachments: vec![file.attachment],
                        caption: file.caption,
                    },
                    file.mentions_me,
                ),
                None => Made::unsupported(HINT_FILE),
            })
        }
        Ok(Ty::Location | Ty::GroupLocation) => Ok(match body::location(placed.inner) {
            Some(location) => Made::plain(Body::Location(location), false),
            None => Made::unsupported(HINT_LOCATION),
        }),
        _ => Err(RULE_NOT_MAPPED),
    }
}

/// A reference to message `id` of `conv`: Threema's ids are the sender's own, so the author is not known (a quote
/// names none, and an edit, a delete or a reaction names the message by its id alone); the conversation binds it.
fn target(conv: &ConvKey, id: MessageId) -> MsgTarget {
    MsgTarget {
        conv: Some(conv.clone()),
        author: TargetAuthor::Unknown,
        local: msg_id::format(id).into(),
        alternates: Vec::new(),
    }
}

/// An edit, a delete or a reaction (R6, R7, R8; R2 for the times): ONE event about a message that was shown
/// before (or is on the phone only, then the core drops it), placed like any message (the sender must be a
/// stored contact, the group a stored visible one, the container must agree with the envelope). `by` is the
/// sender, `None` for our own other device; the time is the modifier's own `created_at` (R2), never the
/// reflection's. They bring no archived chat back (the spec's "Unarchive: No") and state no chat (E5): a
/// modifier cannot open a chat. A protobuf that does not decode is dropped by its rule (`body.rs`).
fn modifier(msg: &Msg<'_>, cx: &Cx<'_>, view: &dyn View) -> Result<Mapped> {
    let placed = match place(msg, view)? {
        Ok(placed) => placed,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let Ok(at) = i64::try_from(msg.created_at) else {
        return Ok(Mapped::dropped(RULE_CREATED_AT));
    };
    let by = match msg.dir {
        Dir::In(sender) => match user(sender) {
            Ok(author) => Some(author),
            Err(_) => return Ok(Mapped::dropped(RULE_SENDER_INVALID)),
        },
        Dir::Out(_) => None,
    };
    let event = match Ty::try_from(msg.ty) {
        Ok(Ty::EditMessage | Ty::GroupEditMessage) => {
            match body::edit(placed.inner, cx.own, placed.in_group) {
                Ok(edit) => BackendEvent::Edited {
                    target: target(&placed.conv, edit.target),
                    by,
                    new_local: None,
                    body: Body::Text(edit.text),
                    edited_at: at,
                    keeps_quote: None,
                },
                Err(rule) => return Ok(Mapped::dropped(rule)),
            }
        }
        Ok(Ty::DeleteMessage | Ty::GroupDeleteMessage) => match body::delete(placed.inner) {
            Ok(id) => BackendEvent::Deleted {
                target: target(&placed.conv, id),
                by,
                for_everyone: true,
                at,
            },
            Err(rule) => return Ok(Mapped::dropped(rule)),
        },
        Ok(Ty::Reaction | Ty::GroupReaction) => match body::reaction(placed.inner) {
            Ok(reaction) => BackendEvent::Reaction {
                target: target(&placed.conv, reaction.target),
                by,
                emoji: reaction.emoji,
                remove: reaction.remove,
                at,
            },
            Err(rule) => return Ok(Mapped::dropped(rule)),
        },
        _ => return Ok(Mapped::dropped(RULE_NOT_MAPPED)),
    };
    Ok(Mapped::events(vec![event]))
}

/// A delivery receipt (R9; R2 for the time: the receipt's own `created_at`), placed like any message (the
/// sender a stored contact, the group a stored visible one, the container agreeing with the envelope). Like
/// a modifier it brings no archived chat back ("Unarchive: No") and states no chat (E5).
///
/// - INCOMING 1:1 (the peer's receipt of OUR messages): status 1 -> `Receipts { Delivered }`, 2 -> `Read`, by
///   the sender, every target authored by us (`TargetAuthor::Own`) in the direct chat.
/// - OUTGOING 1:1 (our phone's receipt to the peer): 2 -> `Receipts { Read }` by our own ID (the core's "our
///   own read on another device"), the targets the peer's (author unknown); 1 -> nothing, counted QUIET
///   [`classify::RULE_OWN_DELIVERED`] (our device got a peer's message: nothing for the core to show).
/// - Status 3 / 4 in either direction (C13: the deprecated 👍 / 👎 "replace all existing reactions of the
///   respective sender"): per id `Reaction { emoji: "", remove: true, at: created_at - 1 }`, then
///   `Reaction { emoji, at: created_at }`, by the sender (`None`: ours). The target's author (P-L2): in a 1:1
///   chat `Own` for the peer's receipt (it reacts to OUR message) and `User(peer)` for ours (our phone reacts
///   to the PEER's), as the spec's steps 1.3 say; in a group unknown.
/// - A GROUP receipt: only 3 / 4 (the pair, in the group); any other status is dropped
///   [`RULE_GROUP_RECEIPT`]. A 1:1 status outside 1-4 is dropped [`RULE_RECEIPT_STATUS`].
fn receipt(msg: &Msg<'_>, cx: &Cx<'_>, view: &dyn View) -> Result<Mapped> {
    let placed = match place(msg, view)? {
        Ok(placed) => placed,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let Ok(at) = i64::try_from(msg.created_at) else {
        return Ok(Mapped::dropped(RULE_CREATED_AT));
    };
    let parsed = match body::receipt(placed.inner) {
        Ok(parsed) => parsed,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let by = match msg.dir {
        Dir::In(sender) => match user(sender) {
            Ok(author) => Some(author),
            Err(_) => return Ok(Mapped::dropped(RULE_SENDER_INVALID)),
        },
        Dir::Out(_) => None,
    };
    let emoji = match (parsed.status, placed.in_group) {
        (3, _) => THUMBS_UP,
        (4, _) => THUMBS_DOWN,
        (_, true) => return Ok(Mapped::dropped(RULE_GROUP_RECEIPT)),
        (1 | 2, false) => {
            let kind = if parsed.status == 1 {
                ReceiptKind::Delivered
            } else {
                ReceiptKind::Read
            };
            let (by, author) = match (by, kind) {
                (Some(by), _) => (by, TargetAuthor::Own),
                // Our own device received the peer's message: nothing to state.
                (None, ReceiptKind::Delivered) => {
                    return Ok(Mapped::dropped(classify::RULE_OWN_DELIVERED));
                }
                (None, _) => match user(cx.own) {
                    Ok(own) => (own, TargetAuthor::Unknown),
                    Err(_) => return Ok(Mapped::dropped(RULE_SENDER_INVALID)),
                },
            };
            let targets = parsed
                .ids
                .into_iter()
                .map(|id| MsgTarget {
                    author: author.clone(),
                    ..target(&placed.conv, id)
                })
                .collect();
            return Ok(Mapped::events(vec![BackendEvent::Receipts {
                targets,
                by,
                kind,
                at,
            }]));
        }
        (_, false) => return Ok(Mapped::dropped(RULE_RECEIPT_STATUS)),
    };
    // P-L2: in a 1:1 chat the spec binds the message to a side (`csp.struct.yml`, the Common Incoming Delivery
    // Receipt Steps 1.3 abort unless the original sender is the user; the Outgoing steps abort when it IS the
    // user): the peer reacts to OURS (`Own`), our phone to the PEER's (`User(peer)`). A group has no such rule:
    // unknown, the conversation binds it.
    let author = match (&placed.conv, msg.dir, placed.in_group) {
        (_, _, true) => TargetAuthor::Unknown,
        (_, Dir::In(_), false) => TargetAuthor::Own,
        (ConvKey::Direct(peer), Dir::Out(_), false) => TargetAuthor::User(peer.clone()),
        _ => TargetAuthor::Unknown,
    };
    let mut events = Vec::with_capacity(2 * parsed.ids.len());
    for id in parsed.ids {
        let target = MsgTarget {
            author: author.clone(),
            ..target(&placed.conv, id)
        };
        events.push(BackendEvent::Reaction {
            target: target.clone(),
            by: by.clone(),
            emoji: String::new(),
            remove: true,
            at: at.saturating_sub(1),
        });
        events.push(BackendEvent::Reaction {
            target,
            by: by.clone(),
            emoji: emoji.to_owned(),
            remove: false,
            at,
        });
    }
    Ok(Mapped::events(events))
}

/// The recipients of a message into a stored group (R10): its `member_identities` and the creator (the spec's
/// list holds neither the creator nor the user, so the creator is added), minus us, sorted, each once. EMPTY
/// for a NOTES group (`csp.struct.yml`: the user created it and nobody else is in it, so nothing is sent to the
/// chat server, P-M1).
fn group_recipients<'a>(stored: &'a d2d_sync::Group, creator: &'a str, own: &str) -> Vec<&'a str> {
    let listed = stored
        .member_identities
        .as_ref()
        .map_or(&[][..], |m| m.identities.as_slice());
    let mut ids: Vec<&str> = listed
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(creator))
        .filter(|id| *id != own)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// P-M1: whether the chat the message is placed in is a NOTES group (the stored group has no recipient, see
/// [`group_recipients`]). Its own messages are stated `Sent` at their arrival: the phone reflects the Sent
/// update with no receivers ("a legitimate case", `csp.struct.yml`), which the update's mapper skips quietly,
/// and with no recipient the core would never move a `Sending` row on.
fn is_notes_group(owner: &Owner, own: &str, view: &dyn View) -> Result<bool> {
    let Owner::Group { creator, group_id } = owner else {
        return Ok(false);
    };
    Ok(view
        .stored_group(creator, *group_id)?
        .is_some_and(|stored| group_recipients(&stored, creator, own).is_empty()))
}

/// The conversation an update names (R10, R11), when the file holds it and the core was told of it: a stored
/// contact's direct chat, a stored VISIBLE group (L-3: one that is not shown has no chat); a list, an unknown
/// contact or group, an ID or a key that is not valid: `None`. For R10 also the update's recipients: the
/// contact, or every stored member of the group but us ([`group_recipients`]: the creator is added unless it
/// is us), sorted by ID; a group with nobody but us (a notes group) has an EMPTY list.
fn update_conversation(
    conversation: Option<&d2d::ConversationId>,
    own: &str,
    view: &dyn View,
) -> Result<Option<(ConvKey, Vec<kourier_model::UserId>)>> {
    let Some(id) = conversation.and_then(|c| c.id.as_ref()) else {
        return Ok(None);
    };
    match id {
        Id::Contact(contact) => {
            if !body::is_identity(contact) || !view.contact(contact)? {
                return Ok(None);
            }
            Ok(user(contact)
                .ok()
                .map(|peer| (ConvKey::Direct(peer.clone()), vec![peer])))
        }
        Id::Group(group) => {
            let creator = &group.creator_identity;
            if !body::is_identity(creator) {
                return Ok(None);
            }
            let Some(stored) = view.stored_group(creator, group.group_id)? else {
                return Ok(None);
            };
            if !is_visible(&stored) {
                return Ok(None);
            }
            let Ok(conv) = group_key(creator, group.group_id) else {
                return Ok(None);
            };
            let Ok(members) = group_recipients(&stored, creator, own)
                .into_iter()
                .map(user)
                .collect::<std::result::Result<_, _>>()
            else {
                return Ok(None);
            };
            Ok(Some((conv, members)))
        }
        Id::DistributionList(_) => Ok(None),
    }
}

/// R10, THE SENT UPDATE (`OutgoingMessageUpdate`, our own other device's message accepted by the chat server):
/// per update with `sent`, ONE `OwnSendState { local: None, target (author Own), per_recipient }` with every
/// recipient `Sent { sealed: false }` (C9: the core merges it into the message's state, so an own `Sending` row
/// becomes `Sent`); the update's time is the reflection's and the contract has no field for it. Every other
/// update is skipped and counted [`RULE_UPDATE_UNKNOWN`]; all events go in the envelope's ONE row; no event at
/// all is acked with nothing. Two parts are NOT unknown (FIX-P):
///
/// - P-M1: the update of a message in a NOTES group (a group of ours with nobody else) is skipped QUIETLY
///   ([`classify::RULE_SENT_NOTES_GROUP`]): the message was stated `Sent` when it arrived ([`message`]).
/// - P-L1: an update that names a DISTRIBUTION LIST is not stated (the list's message is deferred `list`, its
///   1:1 copies are the later lists step's) and is not counted either: the whole envelope is ALSO deferred as
///   `list`, so that step gets it. The envelope's contact and group updates are stated now as ever.
fn sent_update(
    update: &d2d::OutgoingMessageUpdate,
    cx: &Cx<'_>,
    view: &dyn View,
) -> Result<Mapped> {
    use d2d::outgoing_message_update::update::Update;
    let mut events = Vec::new();
    let mut skipped = Vec::new();
    let mut unknown = 0u32;
    let mut notes = 0u32;
    let mut lists = false;
    for one in &update.updates {
        let Some(Update::Sent(_)) = one.update else {
            unknown += 1;
            continue;
        };
        if matches!(
            one.conversation.as_ref().and_then(|c| c.id.as_ref()),
            Some(Id::DistributionList(_))
        ) {
            lists = true;
            continue;
        }
        let Some((conv, recipients)) =
            update_conversation(one.conversation.as_ref(), cx.own, view)?
        else {
            unknown += 1;
            continue;
        };
        if recipients.is_empty() {
            notes += 1;
            continue;
        }
        events.push(BackendEvent::OwnSendState {
            local: None,
            target: MsgTarget {
                author: TargetAuthor::Own,
                ..target(&conv, MessageId(one.message_id))
            },
            per_recipient: recipients
                .into_iter()
                .map(|r| (r, RecipientState::Sent { sealed: false }))
                .collect(),
        });
    }
    if unknown > 0 {
        skipped.push((RULE_UPDATE_UNKNOWN, unknown));
    }
    if notes > 0 {
        skipped.push((classify::RULE_SENT_NOTES_GROUP, notes));
    }
    Ok(Mapped {
        skipped,
        deferred: lists.then_some(classify::KIND_LIST),
        ..Mapped::events(events)
    })
}

/// R11, THE READ UPDATE (`IncomingMessageUpdate`: our phone read a peer's message with read receipts off):
/// per (conversation, `Read.at`) in the order first seen, ONE `Receipts { Read }` by our own ID (the core's
/// "our own read on another device"), its targets the messages in order (author unknown, the conversation
/// binds them). Skips as [`sent_update`].
fn read_update(
    update: &d2d::IncomingMessageUpdate,
    cx: &Cx<'_>,
    view: &dyn View,
) -> Result<Mapped> {
    use d2d::incoming_message_update::update::Update;
    let Ok(own) = user(cx.own) else {
        return Ok(Mapped::dropped(RULE_SENDER_INVALID));
    };
    let mut grouped: Vec<(ConvKey, i64, Vec<MsgTarget>)> = Vec::new();
    let mut skipped = 0u32;
    for one in &update.updates {
        let Some(Update::Read(read)) = &one.update else {
            skipped += 1;
            continue;
        };
        let Ok(at) = i64::try_from(read.at) else {
            skipped += 1;
            continue;
        };
        let Some((conv, _)) = update_conversation(one.conversation.as_ref(), cx.own, view)? else {
            skipped += 1;
            continue;
        };
        let read = target(&conv, MessageId(one.message_id));
        match grouped.iter_mut().find(|(c, a, _)| *c == conv && *a == at) {
            Some((_, _, targets)) => targets.push(read),
            None => grouped.push((conv, at, vec![read])),
        }
    }
    let events = grouped
        .into_iter()
        .map(|(_, at, targets)| BackendEvent::Receipts {
            targets,
            by: own.clone(),
            kind: ReceiptKind::Read,
            at,
        })
        .collect();
    Ok(Mapped {
        skipped: if skipped > 0 {
            vec![(RULE_UPDATE_UNKNOWN, skipped)]
        } else {
            Vec::new()
        },
        ..Mapped::events(events)
    })
}

/// R12, TYPING: what an EPHEMERAL envelope states (`reflect::open_ephemeral` opens it; no file is read, no
/// nonce written, nothing acked). Only an INCOMING typing indicator from a Threema ID whose body is ONE byte
/// 0 or 1 is `Typing { conv: Direct(sender), by: sender, device: None, on, ttl: 15 s }`; anything else is the
/// rule it is left by (logged at DEBUG by the caller).
pub(super) fn typing(envelope: &d2d::Envelope) -> std::result::Result<BackendEvent, &'static str> {
    let Some(Content::IncomingMessage(m)) = &envelope.content else {
        return Err(RULE_EPHEMERAL_KIND);
    };
    if m.r#type != Ty::TypingIndicator as i32 {
        return Err(RULE_EPHEMERAL_KIND);
    }
    if !body::is_identity(&m.sender_identity) {
        return Err(RULE_TYPING_SENDER);
    }
    let Some(on) = body::typing(&m.body) else {
        return Err(RULE_TYPING_BYTE);
    };
    let by = user(&m.sender_identity).map_err(|_| RULE_TYPING_SENDER)?;
    Ok(BackendEvent::Typing {
        conv: ConvKey::Direct(by.clone()),
        by,
        device: None,
        on,
        ttl: TYPING_TTL,
    })
}

fn message(msg: &Msg<'_>, class: Class, cx: &Cx<'_>, view: &dyn View) -> Result<Mapped> {
    if is_modifier(msg.ty) {
        return modifier(msg, cx, view);
    }
    if is_receipt(msg.ty) {
        return receipt(msg, cx, view);
    }
    let placed = match place(msg, view)? {
        Ok(placed) => placed,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let Ok(sent_at) = i64::try_from(msg.created_at) else {
        return Ok(Mapped::dropped(RULE_CREATED_AT));
    };
    let this = MessageId(msg.message_id);
    let Made {
        body,
        reply_to,
        mentions_me,
        raw,
    } = match content(msg, &placed, class, cx) {
        Ok(content) => content,
        Err(rule) => return Ok(Mapped::dropped(rule)),
    };
    let (origin, out_state) = match msg.dir {
        Dir::In(sender) => match user(sender) {
            Ok(author) => (
                Origin::Remote {
                    author,
                    device: None,
                },
                None,
            ),
            Err(_) => return Ok(Mapped::dropped(RULE_SENDER_INVALID)),
        },
        // C9: see the module doc. P-M1: a notes group's message is `Sent` at once.
        Dir::Out(_) => (
            Origin::OwnOtherDevice { device: None },
            Some(if is_notes_group(&placed.owner, cx.own, view)? {
                OutgoingState::Sent {
                    server_at: i64::try_from(cx.server_at).ok(),
                }
            } else {
                OutgoingState::Sending { progress: None }
            }),
        ),
    };
    // M-3: never at a re-map (the `Cx::remap` doc).
    let back = if unarchives(msg.ty) && !cx.remap {
        bring_back(&placed.owner, &placed.conv, view)?
    } else {
        None
    };
    let mut event = IncomingMessage::new(
        placed.conv,
        msg_id::format(this),
        origin,
        body,
        sent_at,
        cx.now_ms,
    );
    event.server_at = i64::try_from(cx.server_at).ok();
    event.reply_to = reply_to;
    event.mentions_me = mentions_me && !event.origin.is_own();
    event.catch_up = cx.catch_up;
    event.initial_out_state = out_state;
    if raw {
        event.raw = Some(BackendBlob::new(RAW_VERSION, msg.body.to_vec()));
    }
    let message = BackendEvent::Message(event);
    let hidden = match &placed.owner {
        Owner::Contact(identity) => hidden_chat(identity, back.is_some(), view)?,
        Owner::Group { .. } => None,
    };
    Ok(match (hidden, back) {
        // E5: the chat's whole patch first (it already says what the bring-back says), then the message.
        (Some(chat), back) => {
            let mut mapped = Mapped::events(vec![chat, message]);
            mapped.writes.extend(back.map(|(_, write)| write));
            mapped
        }
        (None, Some((upsert, write))) => {
            let mut mapped = Mapped::events(vec![upsert, message]);
            mapped.writes.push(write);
            mapped
        }
        (None, None) => Mapped::events(vec![message]),
    })
}

/// E5 (TH5 A8; the lead's call after A7b): a 1:1 chat whose contact the file holds as NOT direct (the phone
/// HIDES it, acquaintance `GROUP_OR_DELETED`) was never stated (E1 states only direct contacts' chats), so the
/// core would create it from the message alone, without features: no limits, a composer shown. Such a
/// message states the chat's WHOLE patch from the stored record (`join::events::one_to_one_patch`: features,
/// protected, archive / pin, mute) FIRST in the same row. `brought_back`: H5 sets the record
/// NORMAL in the same zone, so the patch says so too. A direct contact's chat was stated: `None`.
fn hidden_chat(
    identity: &str,
    brought_back: bool,
    view: &dyn View,
) -> Result<Option<BackendEvent>> {
    let Some(mut contact) = view.stored_contact(identity)? else {
        return Ok(None);
    };
    if is_direct(&contact) {
        return Ok(None);
    }
    if brought_back {
        contact.conversation_visibility = Some(d2d_sync::ConversationVisibility::Normal as i32);
    }
    // The ID was checked by `place` (a `user` of it exists); a belt: no patch rather than a failed zone.
    Ok(one_to_one_patch(&contact)
        .ok()
        .map(BackendEvent::ConversationUpsert))
}

/// The conversation of `msg` and the data inside its container, or the rule that drops it. The outer
/// `Result` is a failed read.
fn place<'a>(
    msg: &Msg<'a>,
    view: &dyn View,
) -> Result<std::result::Result<Placed<'a>, &'static str>> {
    let group_form = is_group_form(msg.ty);
    match msg.dir {
        Dir::In(sender) => {
            if !body::is_identity(sender) {
                return Ok(Err(RULE_SENDER_INVALID));
            }
            if !view.contact(sender)? {
                return Ok(Err(RULE_SENDER_UNKNOWN));
            }
            if group_form {
                let (group, inner) = match body::member_container(msg.body) {
                    Ok(parts) => parts,
                    Err(rule) => return Ok(Err(rule)),
                };
                return group_place(&group, inner, view);
            }
            Ok(match user(sender) {
                Ok(user) => Ok(Placed {
                    conv: ConvKey::Direct(user),
                    inner: msg.body,
                    in_group: false,
                    owner: Owner::Contact(sender.to_owned()),
                }),
                Err(_) => Err(RULE_SENDER_INVALID),
            })
        }
        Dir::Out(conversation) => {
            let Some(id) = conversation.and_then(|c| c.id.as_ref()) else {
                return Ok(Err(RULE_CONVERSATION_INVALID));
            };
            match id {
                Id::Contact(contact) => {
                    if group_form {
                        return Ok(Err(RULE_CONVERSATION_MISMATCH));
                    }
                    if !body::is_identity(contact) {
                        return Ok(Err(RULE_CONVERSATION_INVALID));
                    }
                    if !view.contact(contact)? {
                        return Ok(Err(RULE_CONTACT_UNKNOWN));
                    }
                    Ok(match user(contact) {
                        Ok(user) => Ok(Placed {
                            conv: ConvKey::Direct(user),
                            inner: msg.body,
                            in_group: false,
                            owner: Owner::Contact(contact.clone()),
                        }),
                        Err(_) => Err(RULE_CONVERSATION_INVALID),
                    })
                }
                Id::Group(named) => {
                    if !group_form {
                        return Ok(Err(RULE_CONVERSATION_MISMATCH));
                    }
                    let (group, inner) = match body::member_container(msg.body) {
                        Ok(parts) => parts,
                        Err(rule) => return Ok(Err(rule)),
                    };
                    if group.creator != named.creator_identity || group.group_id != named.group_id {
                        return Ok(Err(RULE_GROUP_MISMATCH));
                    }
                    group_place(&group, inner, view)
                }
                // Classified `list` before this point; kept as a belt.
                Id::DistributionList(_) => Ok(Err(RULE_CONVERSATION_MISMATCH)),
            }
        }
    }
}

fn group_place<'a>(
    group: &GroupRef,
    inner: &'a [u8],
    view: &dyn View,
) -> Result<std::result::Result<Placed<'a>, &'static str>> {
    if !view.group(&group.creator, group.group_id)? {
        return Ok(Err(RULE_GROUP_UNKNOWN));
    }
    // L-3: a group that is not shown (never stated) must not get a chat from a message: the core would
    // make one without the features (E5's problem, for a group).
    if view
        .stored_group(&group.creator, group.group_id)?
        .is_some_and(|stored| !is_visible(&stored))
    {
        return Ok(Err(RULE_GROUP_NOT_VISIBLE));
    }
    Ok(match group_key(&group.creator, group.group_id) {
        Ok(conv) => Ok(Placed {
            conv,
            inner,
            in_group: true,
            owner: Owner::Group {
                creator: group.creator.clone(),
                group_id: group.group_id,
            },
        }),
        Err(_) => Err(RULE_GROUP_KEY),
    })
}

/// Where a CSP type's message names its conversation (TH5b R1, [`conversation_of`]): exhaustive over the pinned
/// enum, so a type a libthreema update adds does not compile here until it is placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    /// A 1:1 type: the sender (incoming) or the `ConversationId` contact (outgoing).
    Direct,
    /// Wrapped by a `group-member-container` (every type of [`is_group_form`], and the deferred group kinds:
    /// the group poll vote, the group call start; the group leave): the container's group.
    Member,
    /// No conversation this build can name: a creator-container group control, a type that is never reflected,
    /// the invalid and the empty type, and a number the pinned enum lacks (its form is unknown: a later build's
    /// group type would be misread as its sender's 1:1 chat).
    Unplaced,
}

fn form(ty: i32) -> Form {
    let Ok(ty) = Ty::try_from(ty) else {
        return Form::Unplaced;
    };
    match ty {
        Ty::Text
        | Ty::DeprecatedImage
        | Ty::Location
        | Ty::DeprecatedAudio
        | Ty::DeprecatedVideo
        | Ty::File
        | Ty::PollSetup
        | Ty::PollVote
        | Ty::CallOffer
        | Ty::CallAnswer
        | Ty::CallIceCandidate
        | Ty::CallHangup
        | Ty::CallRinging
        | Ty::DeliveryReceipt
        | Ty::TypingIndicator
        | Ty::Reaction
        | Ty::EditMessage
        | Ty::DeleteMessage
        | Ty::ContactSetProfilePicture
        | Ty::ContactDeleteProfilePicture
        | Ty::ContactRequestProfilePicture => Form::Direct,
        Ty::GroupText
        | Ty::GroupLocation
        | Ty::GroupImage
        | Ty::GroupAudio
        | Ty::GroupVideo
        | Ty::GroupFile
        | Ty::GroupPollSetup
        | Ty::GroupPollVote
        | Ty::GroupCallStart
        | Ty::GroupLeave
        | Ty::GroupDeliveryReceipt
        | Ty::GroupEditMessage
        | Ty::GroupDeleteMessage
        | Ty::GroupReaction => Form::Member,
        Ty::InvalidType
        | Ty::Empty
        | Ty::GroupSetup
        | Ty::GroupName
        | Ty::GroupSetProfilePicture
        | Ty::GroupDeleteProfilePicture
        | Ty::GroupSyncRequest
        | Ty::ForwardSecurityEnvelope
        | Ty::WorkSyncDelta
        | Ty::WebSessionResume => Form::Unplaced,
    }
}

/// TH5b R1: the conversation a (deferred) envelope belongs to, by the rules of [`place`] without its reads of
/// the file (no key is needed either: the rows are plaintext): an INCOMING message of a 1:1 type is its
/// sender's direct chat, of a group type the group its `group-member-container` names; an OUTGOING one is the
/// `ConversationId` contact's direct chat for a 1:1 type, and the `ConversationId` group for a group type
/// whose container names the SAME group (`place`'s `group-mismatch` otherwise). A sender or a contact that is
/// no Threema ID, a short container, a type of the wrong form for its `ConversationId`, a distribution list, a
/// sync, a message update, an envelope without content: `None` (the clear keeps such a row; the map drops or
/// defers it by its own rule). Pure.
pub(crate) fn conversation_of(envelope: &d2d::Envelope) -> Option<ConvKey> {
    match envelope.content.as_ref()? {
        Content::IncomingMessage(m) => {
            if !body::is_identity(&m.sender_identity) {
                return None;
            }
            match form(m.r#type) {
                Form::Direct => user(&m.sender_identity).ok().map(ConvKey::Direct),
                Form::Member => {
                    let (group, _) = body::member_container(&m.body).ok()?;
                    group_key(&group.creator, group.group_id).ok()
                }
                Form::Unplaced => None,
            }
        }
        Content::OutgoingMessage(m) => {
            match (m.conversation.as_ref()?.id.as_ref()?, form(m.r#type)) {
                (Id::Contact(contact), Form::Direct) if body::is_identity(contact) => {
                    user(contact).ok().map(ConvKey::Direct)
                }
                (Id::Group(named), Form::Member) => {
                    let (group, _) = body::member_container(&m.body).ok()?;
                    if group.creator != named.creator_identity || group.group_id != named.group_id {
                        return None;
                    }
                    group_key(&named.creator_identity, named.group_id).ok()
                }
                _ => None,
            }
        }
        Content::OutgoingMessageUpdate(_)
        | Content::IncomingMessageUpdate(_)
        | Content::UserProfileSync(_)
        | Content::ContactSync(_)
        | Content::GroupSync(_)
        | Content::DistributionListSync(_)
        | Content::SettingsSync(_)
        | Content::MdmParameterSync(_) => None,
    }
}

/// The [`View`] reads of TH5b B4 for a test file that holds no settings, profile or list rows (every test
/// view of a message mapper; `sync.rs` has its own views of those rows).
#[cfg(test)]
macro_rules! no_b4_rows {
    () => {
        fn stored_settings(&self) -> Result<Option<d2d_sync::Settings>> {
            Ok(None)
        }

        fn stored_profile(&self) -> Result<Option<d2d_sync::UserProfile>> {
            Ok(None)
        }

        fn stored_list(&self, _: u64) -> Result<Option<d2d_sync::DistributionList>> {
            Ok(None)
        }

        fn list_count(&self) -> Result<u64> {
            Ok(0)
        }
    };
}
#[cfg(test)]
pub(super) use no_b4_rows;

#[cfg(test)]
mod tests {
    use kourier_model::{RangeKind, UserId};
    use libthreema::protobuf::common::GroupIdentity;

    use super::*;

    /// Synthetic IDs (8 characters), each a substring of no other test value.
    const OWN: &str = "A6MAP001";
    const PEER: &str = "A6MAP002";
    const STRANGER: &str = "A6MAP003";
    const CREATOR: &str = "A6MAP004";
    const GROUP: u64 = 0x1122_3344_5566_7788;
    const OTHER_GROUP: u64 = 0x0000_0000_0000_0042;
    const AT: u64 = 1_700_000_000_000;
    const SERVER_AT: u64 = 1_700_000_005_000;

    /// A file with two contacts and one group.
    struct Known;

    impl View for Known {
        fn contact(&self, identity: &str) -> Result<bool> {
            Ok([PEER, CREATOR].contains(&identity))
        }

        fn group(&self, creator: &str, group_id: u64) -> Result<bool> {
            Ok(creator == CREATOR && group_id == GROUP)
        }

        fn stored_contact(&self, _: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(None)
        }

        fn stored_group(&self, _: &str, _: u64) -> Result<Option<d2d_sync::Group>> {
            Ok(None)
        }

        fn blocked(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((2, 1))
        }

        no_b4_rows!();
    }

    fn cx(catch_up: bool) -> Cx<'static> {
        Cx {
            own: OWN,
            now_ms: 1_700_000_009_000,
            server_at: SERVER_AT,
            catch_up,
            notice_tag: [0; 8],
            remap: false,
        }
    }

    fn container(creator: &str, group_id: u64, inner: &[u8]) -> Vec<u8> {
        let mut body = creator.as_bytes().to_vec();
        body.extend_from_slice(&group_id.to_le_bytes());
        body.extend_from_slice(inner);
        body
    }

    fn incoming(sender: &str, ty: i32, id: u64, body: Vec<u8>) -> d2d::Envelope {
        d2d::Envelope {
            protocol_version: 3,
            content: Some(Content::IncomingMessage(d2d::IncomingMessage {
                sender_identity: sender.to_owned(),
                message_id: id,
                created_at: AT,
                r#type: ty,
                body,
                nonce: vec![1; 24],
            })),
            ..Default::default()
        }
    }

    fn outgoing(conversation: Id, ty: i32, id: u64, body: Vec<u8>) -> d2d::Envelope {
        d2d::Envelope {
            protocol_version: 3,
            content: Some(Content::OutgoingMessage(d2d::OutgoingMessage {
                conversation: Some(d2d::ConversationId {
                    id: Some(conversation),
                }),
                message_id: id,
                created_at: AT,
                r#type: ty,
                body,
                ..Default::default()
            })),
            ..Default::default()
        }
    }

    fn to_group(creator: &str, group_id: u64) -> Id {
        Id::Group(GroupIdentity {
            group_id,
            creator_identity: creator.to_owned(),
        })
    }

    fn run(envelope: &d2d::Envelope) -> Mapped {
        map(envelope, &cx(false), &Known).expect("the view does not fail")
    }

    /// The one message event of `mapped`.
    fn the_message(mapped: Mapped) -> IncomingMessage {
        assert!(
            mapped.deferred.is_none() && mapped.dropped.is_none(),
            "the envelope was not mapped"
        );
        match <[BackendEvent; 1]>::try_from(mapped.events) {
            Ok([BackendEvent::Message(m)]) => m,
            _ => panic!("not exactly one message event"),
        }
    }

    fn peer() -> UserId {
        UserId::from_parts("threema", PEER).expect("a user")
    }

    #[test]
    fn an_incoming_text_in_a_1_1_chat() {
        let m = the_message(run(&incoming(
            PEER,
            Ty::Text as i32,
            0x0807_0605_0403_0201,
            b"hi".to_vec(),
        )));
        assert_eq!(m.conv, ConvKey::Direct(peer()));
        assert_eq!(
            &*m.local, "0102030405060708",
            "the id is the wire bytes in order"
        );
        assert_eq!(
            m.origin,
            Origin::Remote {
                author: peer(),
                device: None
            }
        );
        assert_eq!(m.body, Body::text("hi"));
        assert_eq!(
            (m.sent_at, m.server_at),
            (AT as i64, Some(SERVER_AT as i64))
        );
        assert_eq!(m.received_at, 1_700_000_009_000);
        assert!(m.reply_to.is_none() && !m.mentions_me && !m.read && !m.silent);
        assert!(
            m.initial_out_state.is_none(),
            "an incoming message has no out state"
        );
        assert!(m.raw.is_none() && m.arrival.is_none() && m.linked_local.is_none());
        assert!(!m.catch_up);
    }

    /// A file whose one group is STORED but not visible (M1: the user LEFT and nobody is listed), or visible.
    struct StoredGroup {
        visible: bool,
    }

    impl View for StoredGroup {
        fn contact(&self, identity: &str) -> Result<bool> {
            Ok([PEER, CREATOR].contains(&identity))
        }

        fn group(&self, creator: &str, group_id: u64) -> Result<bool> {
            Ok(creator == CREATOR && group_id == GROUP)
        }

        fn stored_contact(&self, _: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(None)
        }

        fn stored_group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>> {
            use d2d_sync::group::UserState;
            Ok(
                (creator == CREATOR && group_id == GROUP).then(|| d2d_sync::Group {
                    user_state: Some(if self.visible {
                        UserState::Member as i32
                    } else {
                        UserState::Left as i32
                    }),
                    member_identities: Some(libthreema::protobuf::common::Identities {
                        identities: Vec::new(),
                    }),
                    ..Default::default()
                }),
            )
        }

        fn blocked(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((2, 1))
        }

        no_b4_rows!();
    }

    /// L-3 (round A): a message, in either direction, for a stored group that is not visible is DROPPED
    /// `group-not-visible` (the core would make a chat from it without the features: E5's problem for a
    /// group); the same messages for a visible group map, and an unknown group is still `group-unknown`.
    #[test]
    fn a_message_for_a_stored_group_that_is_not_visible_is_dropped() {
        let incoming_text = incoming(
            PEER,
            Ty::GroupText as i32,
            5,
            container(CREATOR, GROUP, b"late"),
        );
        let outgoing_text = outgoing(
            to_group(CREATOR, GROUP),
            Ty::GroupText as i32,
            6,
            container(CREATOR, GROUP, b"mine"),
        );
        for envelope in [&incoming_text, &outgoing_text] {
            let hidden = map(envelope, &cx(false), &StoredGroup { visible: false }).expect("maps");
            assert_eq!(hidden.dropped, Some("group-not-visible"));
            assert!(
                hidden.events.is_empty() && hidden.deferred.is_none() && hidden.writes.is_empty(),
                "a message of a hidden group left something"
            );
            let shown = map(envelope, &cx(false), &StoredGroup { visible: true }).expect("maps");
            assert!(
                shown.dropped.is_none() && shown.events.len() == 1,
                "a message of a visible group was dropped"
            );
        }
        let unknown = incoming(
            PEER,
            Ty::GroupText as i32,
            7,
            container(CREATOR, OTHER_GROUP, b"x"),
        );
        assert_eq!(
            map(&unknown, &cx(false), &StoredGroup { visible: false })
                .expect("maps")
                .dropped,
            Some("group-unknown")
        );
    }

    #[test]
    fn catch_up_is_the_connections_word() {
        let envelope = incoming(PEER, Ty::Text as i32, 1, b"hi".to_vec());
        let before_dry = map(&envelope, &cx(true), &Known).expect("maps");
        assert!(the_message(before_dry).catch_up);
    }

    #[test]
    fn an_incoming_group_text_is_in_the_group_key_of_the_header() {
        let body = container(CREATOR, GROUP, b"hello group");
        let m = the_message(run(&incoming(PEER, Ty::GroupText as i32, 5, body)));
        assert_eq!(
            m.conv,
            group_key(CREATOR, GROUP).expect("a key"),
            "the conversation is not join::events::group_key"
        );
        assert_eq!(m.body, Body::text("hello group"));
        assert_eq!(
            m.origin,
            Origin::Remote {
                author: peer(),
                device: None
            },
            "the author is the sender, not the creator"
        );
    }

    #[test]
    fn an_outgoing_text_is_ours_and_sending() {
        let m = the_message(run(&outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::Text as i32,
            9,
            b"out".to_vec(),
        )));
        assert_eq!(m.conv, ConvKey::Direct(peer()), "the chat is the contact's");
        assert_eq!(m.origin, Origin::OwnOtherDevice { device: None });
        assert_eq!(
            m.initial_out_state,
            Some(OutgoingState::Sending { progress: None }),
            "C9: Sending, then the Sent update"
        );
        assert_eq!(m.body, Body::text("out"));
        assert!(!m.mentions_me && !m.read);
    }

    #[test]
    fn an_outgoing_group_text() {
        let body = container(CREATOR, GROUP, b"out group");
        let m = the_message(run(&outgoing(
            to_group(CREATOR, GROUP),
            Ty::GroupText as i32,
            9,
            body,
        )));
        assert_eq!(m.conv, group_key(CREATOR, GROUP).expect("a key"));
        assert_eq!(m.origin, Origin::OwnOtherDevice { device: None });
        assert_eq!(m.body, Body::text("out group"));
        assert!(m.initial_out_state.is_some());
    }

    #[test]
    fn a_quote_is_a_reply_to_an_unknown_author_in_the_same_chat() {
        let text = "> quote #0a0b0c0d0e0f1011\n\nyes";
        let m = the_message(run(&incoming(
            PEER,
            Ty::Text as i32,
            1,
            text.as_bytes().to_vec(),
        )));
        assert_eq!(m.body, Body::text("yes"), "the prefix stayed in the text");
        let reply = m.reply_to.expect("a reply");
        assert_eq!(&*reply.target.local, "0a0b0c0d0e0f1011");
        assert_eq!(reply.target.author, TargetAuthor::Unknown);
        assert_eq!(reply.target.conv, Some(ConvKey::Direct(peer())));
        assert!(reply.author.is_none() && reply.snapshot.is_none());
        // In a group the quote is looked up in the group.
        let body = container(CREATOR, GROUP, text.as_bytes());
        let g = the_message(run(&incoming(PEER, Ty::GroupText as i32, 1, body)));
        assert_eq!(
            g.reply_to.expect("a reply").target.conv,
            Some(group_key(CREATOR, GROUP).expect("a key"))
        );
    }

    #[test]
    fn a_mention_of_us_sets_mentions_me_and_at_all_does_in_a_group_only() {
        let named = format!("hey @[{OWN}]");
        let m = the_message(run(&incoming(PEER, Ty::Text as i32, 1, named.into_bytes())));
        assert!(m.mentions_me);
        let Body::Text(text) = &m.body else {
            panic!("not text")
        };
        assert!(matches!(&text.ranges[0].kind, RangeKind::Mention(_)));
        let all = b"@[@@@@@@@@] all".to_vec();
        assert!(!the_message(run(&incoming(PEER, Ty::Text as i32, 2, all.clone()))).mentions_me);
        let body = container(CREATOR, GROUP, &all);
        assert!(the_message(run(&incoming(PEER, Ty::GroupText as i32, 2, body))).mentions_me);
        // Our own message never mentions us.
        let ours = format!("@[{OWN}]");
        let out = outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::Text as i32,
            3,
            ours.into_bytes(),
        );
        assert!(!the_message(run(&out)).mentions_me);
    }

    #[test]
    fn a_sender_or_a_group_the_file_lacks_is_dropped_by_its_rule() {
        let text = |sender: &str| incoming(sender, Ty::Text as i32, 1, b"x".to_vec());
        assert_eq!(run(&text(STRANGER)).dropped, Some("sender-unknown"));
        assert_eq!(run(&text("bad")).dropped, Some("sender-invalid"));
        // A group message: the sender is unknown, or the group is.
        let g = |sender: &str, group: u64| {
            incoming(
                sender,
                Ty::GroupText as i32,
                1,
                container(CREATOR, group, b"x"),
            )
        };
        assert_eq!(run(&g(STRANGER, GROUP)).dropped, Some("sender-unknown"));
        assert_eq!(run(&g(PEER, OTHER_GROUP)).dropped, Some("group-unknown"));
        // Outgoing: the contact, the group.
        let out = outgoing(
            Id::Contact(STRANGER.to_owned()),
            Ty::Text as i32,
            1,
            b"x".to_vec(),
        );
        assert_eq!(run(&out).dropped, Some("contact-unknown"));
        let out = outgoing(
            to_group(CREATOR, OTHER_GROUP),
            Ty::GroupText as i32,
            1,
            container(CREATOR, OTHER_GROUP, b"x"),
        );
        assert_eq!(run(&out).dropped, Some("group-unknown"));
        // Nothing of a dropped message is kept or stated.
        assert!(run(&text(STRANGER)).events.is_empty() && run(&text(STRANGER)).deferred.is_none());
    }

    #[test]
    fn the_five_desktop_reject_cases_are_dropped() {
        // Desktop `reflected-message.spec.ts`: a plain text directed at a group; a group text (container) directed
        // at a contact; and a group message whose container names ANOTHER group than the envelope.
        let plain = outgoing(to_group(CREATOR, GROUP), Ty::Text as i32, 1, b"x".to_vec());
        assert_eq!(run(&plain).dropped, Some("conversation-mismatch"));
        let wrapped = outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::GroupText as i32,
            1,
            container(CREATOR, GROUP, b"x"),
        );
        assert_eq!(run(&wrapped).dropped, Some("conversation-mismatch"));
        let other = outgoing(
            to_group(CREATOR, GROUP),
            Ty::GroupText as i32,
            1,
            container(CREATOR, OTHER_GROUP, b"x"),
        );
        assert_eq!(run(&other).dropped, Some("group-mismatch"));
        let creator = outgoing(
            to_group(PEER, GROUP),
            Ty::GroupText as i32,
            1,
            container(CREATOR, GROUP, b"x"),
        );
        assert_eq!(run(&creator).dropped, Some("group-mismatch"));
        // An envelope without a conversation.
        let none = d2d::Envelope {
            content: Some(Content::OutgoingMessage(d2d::OutgoingMessage {
                r#type: Ty::Text as i32,
                body: b"x".to_vec(),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(run(&none).dropped, Some("conversation-invalid"));
    }

    #[test]
    fn a_malformed_body_is_dropped_by_the_parsers_rule() {
        let bad = |ty: Ty, body: Vec<u8>| run(&incoming(PEER, ty as i32, 1, body)).dropped;
        assert_eq!(bad(Ty::Text, vec![0xff, 0xfe]), Some("text-utf8"));
        assert_eq!(bad(Ty::Text, Vec::new()), Some("text-empty"));
        assert_eq!(
            bad(Ty::GroupText, b"short".to_vec()),
            Some("container-short")
        );
        assert_eq!(
            bad(Ty::GroupText, container("lower123", GROUP, b"x")),
            Some("container-creator")
        );
        assert_eq!(
            bad(Ty::GroupText, container(CREATOR, GROUP, &[0xff])),
            Some("text-utf8")
        );
        // A time that does not fit.
        let mut late = incoming(PEER, Ty::Text as i32, 1, b"x".to_vec());
        if let Some(Content::IncomingMessage(m)) = &mut late.content {
            m.created_at = u64::MAX;
        }
        assert_eq!(run(&late).dropped, Some("created-at"));
    }

    #[test]
    fn a_poll_and_the_legacy_media_are_unsupported_with_their_raw_body() {
        for (ty, hint) in [
            (Ty::PollSetup, "poll"),
            (Ty::DeprecatedImage, "legacy-media"),
            (Ty::DeprecatedAudio, "legacy-media"),
        ] {
            let m = the_message(run(&incoming(PEER, ty as i32, 1, vec![7; 40])));
            assert_eq!(
                m.body,
                Body::Unsupported {
                    kind_hint: hint.to_owned(),
                    summary: None
                }
            );
            assert_eq!(
                m.raw,
                Some(BackendBlob::new(1, vec![7; 40])),
                "the raw body"
            );
            assert_eq!(m.conv, ConvKey::Direct(peer()));
        }
        // In a group the raw body is the whole container; the chat is the group's.
        let body = container(CREATOR, GROUP, &[9; 30]);
        let m = the_message(run(&incoming(
            PEER,
            Ty::GroupPollSetup as i32,
            1,
            body.clone(),
        )));
        assert_eq!(m.conv, group_key(CREATOR, GROUP).expect("a key"));
        assert_eq!(m.raw, Some(BackendBlob::new(1, body)));
        // Our own poll to a contact is an own unsupported row, Sending.
        let own = the_message(run(&outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::PollSetup as i32,
            1,
            vec![1; 8],
        )));
        assert!(own.initial_out_state.is_some() && own.raw.is_some());
        // A poll from an unknown sender is dropped like any message.
        let stranger = incoming(STRANGER, Ty::PollSetup as i32, 1, vec![7; 10]);
        assert_eq!(run(&stranger).dropped, Some("sender-unknown"));
    }

    #[test]
    fn every_deferred_class_keeps_the_plaintext_and_states_nothing() {
        // TH5b B1 mapped the location and the file (R3, R4), B2 the edit, the delete and the reaction (R6-R8)
        // and B3 the receipts (R9): the votes and the calls wait (R15).
        for (ty, kind) in [
            (Ty::PollVote, "poll-vote"),
            (Ty::GroupPollVote, "poll-vote"),
            (Ty::CallOffer, "call"),
            (Ty::GroupCallStart, "group-call"),
        ] {
            // From a sender the file does NOT know: a deferred kind is kept whatever the sender
            // (the re-map decides later; C3 never shows an unsupported bubble for a modifier).
            let mapped = run(&incoming(STRANGER, ty as i32, 1, vec![3; 12]));
            assert_eq!(
                mapped.deferred,
                Some(kind),
                "{ty:?} is not deferred as {kind}"
            );
            assert!(mapped.events.is_empty() && mapped.dropped.is_none());
        }
        // A newer type number is deferred too (M6), never a bubble.
        let newer = run(&incoming(PEER, 0x7e, 1, vec![3; 12]));
        assert_eq!(newer.deferred, Some("unknown-type"));
        assert!(newer.events.is_empty());
        // So is anything to a distribution list, and an envelope with no content.
        let list = outgoing(Id::DistributionList(5), Ty::Text as i32, 1, b"x".to_vec());
        assert_eq!(run(&list).deferred, Some("list"));
        assert_eq!(run(&d2d::Envelope::default()).deferred, Some("envelope"));
    }

    #[test]
    fn the_kinds_that_are_not_messages_follow_their_rows() {
        let wrap = |content| d2d::Envelope {
            content: Some(content),
            ..Default::default()
        };
        let mapped = |c| run(&wrap(c));
        // B3 (R10, R11): an update envelope with no update states nothing, skips nothing, keeps nothing.
        for update in [
            Content::OutgoingMessageUpdate(Default::default()),
            Content::IncomingMessageUpdate(Default::default()),
        ] {
            let none = mapped(update);
            assert!(
                none.events.is_empty()
                    && none.deferred.is_none()
                    && none.dropped.is_none()
                    && none.skipped.is_empty()
                    && none.writes.is_empty(),
                "an empty update did something"
            );
        }
        assert_eq!(
            mapped(Content::MdmParameterSync(Default::default())).dropped,
            Some("mdm")
        );
        // A7a's kinds and B4's (R13): a contact, group, settings, profile or list sync without its action or
        // message is dropped by its rule (the deltas themselves: `sync.rs`); none is deferred any more (N-1).
        for sync in [
            Content::ContactSync(Default::default()),
            Content::GroupSync(Default::default()),
            Content::SettingsSync(Default::default()),
            Content::UserProfileSync(Default::default()),
            Content::DistributionListSync(Default::default()),
        ] {
            let empty = mapped(sync);
            assert_eq!(empty.dropped, Some("sync-empty"));
            assert!(empty.events.is_empty() && empty.writes.is_empty() && empty.deferred.is_none());
        }
    }

    #[test]
    fn typing_the_invalid_type_and_the_never_reflected_kinds() {
        // Typing is EPHEMERAL: nothing stated, nothing kept (the flag decided the ack elsewhere).
        let typing = run(&incoming(PEER, Ty::TypingIndicator as i32, 1, vec![1]));
        assert!(typing.events.is_empty() && typing.deferred.is_none() && typing.dropped.is_none());
        // Decision M6: 0x00 is dropped.
        assert_eq!(
            run(&incoming(PEER, 0, 1, vec![1])).dropped,
            Some("invalid-type")
        );
        for (ty, rule) in [
            (Ty::Empty, "empty"),
            (Ty::ForwardSecurityEnvelope, "forward-security"),
            (Ty::WebSessionResume, "web-session-resume"),
            (Ty::WorkSyncDelta, "work-sync-delta"),
            (Ty::ContactSetProfilePicture, "contact-control"),
            (Ty::ContactRequestProfilePicture, "contact-control"),
        ] {
            assert_eq!(
                run(&incoming(PEER, ty as i32, 1, vec![1])).dropped,
                Some(rule)
            );
        }
    }

    #[test]
    fn group_control_is_dropped_quietly_from_v02_and_with_the_desync_warning_from_v01() {
        let control = |version: u32| {
            let mut e = incoming(
                CREATOR,
                Ty::GroupName as i32,
                1,
                container(CREATOR, GROUP, b"x"),
            );
            e.protocol_version = version;
            e
        };
        assert_eq!(run(&control(3)).dropped, Some("group-control"));
        assert_eq!(run(&control(2)).dropped, Some("group-control"));
        assert_eq!(run(&control(1)).dropped, Some("group-control-desync"));
        // 0 = unspecified = V0.1.
        assert_eq!(run(&control(0)).dropped, Some("group-control-desync"));
        assert!(classify::is_quiet("group-control"));
        assert!(!classify::is_quiet("group-control-desync"));
    }

    #[test]
    fn a_failed_read_of_the_file_is_an_error_not_a_drop() {
        struct Broken;
        impl View for Broken {
            fn contact(&self, _: &str) -> Result<bool> {
                Err(crate::error::StoreError::Failed("look up a contact failed"))
            }

            fn group(&self, _: &str, _: u64) -> Result<bool> {
                Ok(true)
            }

            fn stored_contact(&self, _: &str) -> Result<Option<d2d_sync::Contact>> {
                Err(crate::error::StoreError::Failed("load a contact failed"))
            }

            fn stored_group(&self, _: &str, _: u64) -> Result<Option<d2d_sync::Group>> {
                Err(crate::error::StoreError::Failed("load a group failed"))
            }

            fn blocked(&self, _: &str) -> Result<bool> {
                Ok(false)
            }

            fn sync_counts(&self) -> Result<(u64, u64)> {
                Ok((0, 0))
            }

            no_b4_rows!();
        }
        let envelope = incoming(PEER, Ty::Text as i32, 1, b"x".to_vec());
        assert!(
            map(&envelope, &cx(false), &Broken).is_err(),
            "a failed read dropped the message"
        );
    }

    /// E5's file: PEER direct, STRANGER hidden (`GROUP_OR_DELETED`, protected, ARCHIVED when `archived`).
    struct Hidden {
        archived: bool,
    }

    fn record(identity: &str, hidden: bool, archived: bool) -> d2d_sync::Contact {
        use d2d_sync::contact::AcquaintanceLevel;
        let level = if hidden {
            AcquaintanceLevel::GroupOrDeleted
        } else {
            AcquaintanceLevel::Direct
        };
        let visibility = if archived {
            d2d_sync::ConversationVisibility::Archived
        } else {
            d2d_sync::ConversationVisibility::Normal
        };
        d2d_sync::Contact {
            identity: identity.to_owned(),
            acquaintance_level: Some(level as i32),
            conversation_category: Some(d2d_sync::ConversationCategory::Protected as i32),
            conversation_visibility: Some(visibility as i32),
            ..Default::default()
        }
    }

    impl View for Hidden {
        fn contact(&self, identity: &str) -> Result<bool> {
            Ok([PEER, STRANGER].contains(&identity))
        }

        fn group(&self, _: &str, _: u64) -> Result<bool> {
            Ok(false)
        }

        fn stored_contact(&self, identity: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(match identity {
                PEER => Some(record(PEER, false, self.archived)),
                STRANGER => Some(record(STRANGER, true, self.archived)),
                _ => None,
            })
        }

        fn stored_group(&self, _: &str, _: u64) -> Result<Option<d2d_sync::Group>> {
            Ok(None)
        }

        fn blocked(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((2, 0))
        }

        no_b4_rows!();
    }

    /// The patch the hidden contact's chat would get as a DIRECT contact (E1's seam), as the oracle of E5.
    fn as_if_direct(identity: &str, archived: bool) -> ConversationPatch {
        crate::join::events::direct_patch(&record(identity, false, archived))
            .expect("a good ID")
            .expect("a direct contact has a chat")
    }

    #[test]
    fn a_hidden_contacts_message_states_its_chat_first_in_the_same_row() {
        let view = Hidden { archived: false };
        let stranger = || ConvKey::Direct(UserId::from_parts("threema", STRANGER).expect("a user"));
        for envelope in [
            incoming(STRANGER, Ty::Text as i32, 7, b"hi".to_vec()),
            outgoing(
                Id::Contact(STRANGER.to_owned()),
                Ty::Text as i32,
                8,
                b"yo".to_vec(),
            ),
        ] {
            let mapped = map(&envelope, &cx(true), &view).expect("maps");
            assert!(
                mapped.writes.is_empty(),
                "a chat that is not archived wrote a record"
            );
            match <[BackendEvent; 2]>::try_from(mapped.events) {
                Ok(
                    [
                        BackendEvent::ConversationUpsert(patch),
                        BackendEvent::Message(m),
                    ],
                ) => {
                    assert_eq!(patch.key, stranger(), "the patch is not the message's chat");
                    assert_eq!(
                        m.conv,
                        stranger(),
                        "the message is not in the hidden contact's chat"
                    );
                    assert_eq!(
                        patch.features.as_ref().and_then(|f| f.cannot_send.clone()),
                        None,
                        "the hidden contact's chat says it cannot be sent in"
                    );
                    assert_eq!(patch.protected, Some(true), "the protected flag is lost");
                    assert_eq!(
                        patch,
                        as_if_direct(STRANGER, false),
                        "the patch is not the stored record's whole chat state"
                    );
                }
                _ => panic!("not [the chat's patch, the message] in one row"),
            }
        }
    }

    #[test]
    fn a_direct_contacts_message_states_nothing_extra() {
        let view = Hidden { archived: false };
        for envelope in [
            incoming(PEER, Ty::Text as i32, 9, b"hi".to_vec()),
            outgoing(
                Id::Contact(PEER.to_owned()),
                Ty::Text as i32,
                10,
                b"yo".to_vec(),
            ),
        ] {
            let m = the_message(map(&envelope, &cx(true), &view).expect("maps"));
            assert_eq!(m.conv, ConvKey::Direct(peer()));
        }
    }

    #[test]
    fn a_hidden_archived_chat_is_stated_brought_back_in_one_patch() {
        let view = Hidden { archived: true };
        let envelope = incoming(STRANGER, Ty::Text as i32, 11, b"hi".to_vec());
        let mapped = map(&envelope, &cx(true), &view).expect("maps");
        match mapped.writes.as_slice() {
            [Write::Contact(c)] => assert_eq!(
                c.conversation_visibility,
                Some(d2d_sync::ConversationVisibility::Normal as i32),
                "the record is not brought back"
            ),
            _ => panic!("the bring-back did not write the record once"),
        }
        match <[BackendEvent; 2]>::try_from(mapped.events) {
            Ok(
                [
                    BackendEvent::ConversationUpsert(patch),
                    BackendEvent::Message(_),
                ],
            ) => {
                assert_eq!(patch.archived, Some(false), "the chat is stated archived");
                assert_eq!(
                    patch,
                    as_if_direct(STRANGER, false),
                    "not the whole state, brought back"
                );
            }
            _ => panic!("not [the chat's patch, the message] in one row"),
        }
    }

    /// Round B, M-3 = S3: a RE-MAPPED text into an ARCHIVED chat brings nothing back (`Cx::remap`): a direct
    /// contact's is the message alone, no record written; a hidden contact's states E5's patch from the record
    /// as it is (ARCHIVED), no record written. The live path (above) still brings both back.
    #[test]
    fn a_remapped_message_never_brings_an_archived_chat_back() {
        let view = Hidden { archived: true };
        let remap = Cx {
            remap: true,
            ..cx(true)
        };
        let direct = map(
            &incoming(PEER, Ty::Text as i32, 12, b"hi".to_vec()),
            &remap,
            &view,
        )
        .expect("maps");
        assert!(direct.writes.is_empty(), "a re-map wrote the record");
        assert!(
            matches!(direct.events.as_slice(), [BackendEvent::Message(_)]),
            "a re-mapped text of a direct contact is not the message alone"
        );
        let hidden = map(
            &incoming(STRANGER, Ty::Text as i32, 13, b"hi".to_vec()),
            &remap,
            &view,
        )
        .expect("maps");
        assert!(hidden.writes.is_empty(), "a re-map wrote the record");
        match hidden.events.as_slice() {
            [
                BackendEvent::ConversationUpsert(patch),
                BackendEvent::Message(_),
            ] => {
                assert_eq!(patch.archived, Some(true), "a re-map brought the chat back");
                assert_eq!(
                    patch,
                    &as_if_direct(STRANGER, true),
                    "not the record as it is"
                );
            }
            _ => panic!("not [the chat's patch, the message] in one row"),
        }
    }

    // ---------------------------------------------------------------------------------------------------
    // TH5b B1: the location (R4) and the file (R3)
    // ---------------------------------------------------------------------------------------------------

    use kourier_model::{Attachment, AttachmentKind, AttachmentState, Location};

    const BLOB: &str = "0123456789abcdef0123456789abcdef";
    const KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    /// A picture's file JSON, the bytes a phone writes, with a caption and the metadata.
    fn picture_json() -> Vec<u8> {
        format!(
            "{{\"j\":1,\"i\":1,\"k\":\"{KEY}\",\"b\":\"{BLOB}\",\"m\":\"image/jpeg\",\"n\":\"synthetic.jpg\",\
             \"s\":123456,\"d\":\"look *here*\",\"x\":{{\"w\":640,\"h\":480}}}}"
        )
        .into_bytes()
    }

    fn sticker_json() -> Vec<u8> {
        format!("{{\"j\":2,\"k\":\"{KEY}\",\"b\":\"{BLOB}\",\"m\":\"image/webp\",\"s\":2048,\"x\":{{\"w\":512,\"h\":512}}}}")
            .into_bytes()
    }

    /// The attachment `picture_json` becomes, written out field by field.
    fn the_picture() -> Attachment {
        Attachment {
            kind: AttachmentKind::Image,
            mime: "image/jpeg".to_owned(),
            size: 123_456,
            file_name: Some("synthetic.jpg".to_owned()),
            dims: Some((640, 480)),
            state: AttachmentState::Remote,
            remote: Some(BackendBlob::new(1, picture_json())),
            ..Attachment::default()
        }
    }

    /// What `look *here*` becomes: the markup removed, one bold range over "here".
    fn the_caption() -> kourier_model::Text {
        kourier_model::Text {
            body: "look here".to_owned(),
            ranges: vec![kourier_model::TextRange {
                start: 5,
                len: 4,
                kind: RangeKind::Bold,
            }],
        }
    }

    #[test]
    fn a_file_is_media_with_one_attachment_and_its_caption_in_every_direction() {
        let media = Body::Media {
            attachments: vec![the_picture()],
            caption: Some(the_caption()),
        };
        // Incoming, 1:1.
        let m = the_message(run(&incoming(
            PEER,
            Ty::File as i32,
            0x0807_0605_0403_0201,
            picture_json(),
        )));
        assert_eq!(m.conv, ConvKey::Direct(peer()));
        assert_eq!(&*m.local, "0102030405060708");
        assert_eq!(
            m.origin,
            Origin::Remote {
                author: peer(),
                device: None
            }
        );
        assert_eq!(m.body, media);
        assert!(m.initial_out_state.is_none() && m.raw.is_none() && m.reply_to.is_none());
        assert_eq!(
            (m.sent_at, m.server_at),
            (AT as i64, Some(SERVER_AT as i64))
        );
        assert!(!m.mentions_me && !m.read && !m.silent && !m.catch_up);
        // Incoming, in the group: the group's key, the container's inner JSON is the pointer.
        let g = the_message(run(&incoming(
            PEER,
            Ty::GroupFile as i32,
            5,
            container(CREATOR, GROUP, &picture_json()),
        )));
        assert_eq!(g.conv, group_key(CREATOR, GROUP).expect("a key"));
        assert_eq!(g.body, media, "the pointer is not the inner JSON alone");
        assert_eq!(
            g.origin,
            Origin::Remote {
                author: peer(),
                device: None
            }
        );
        // Our own other device, 1:1 and group: ours, Sending.
        let own = the_message(run(&outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::File as i32,
            9,
            picture_json(),
        )));
        assert_eq!(own.conv, ConvKey::Direct(peer()));
        assert_eq!(own.origin, Origin::OwnOtherDevice { device: None });
        assert_eq!(
            own.initial_out_state,
            Some(OutgoingState::Sending { progress: None })
        );
        assert_eq!(own.body, media);
        let own_group = the_message(run(&outgoing(
            to_group(CREATOR, GROUP),
            Ty::GroupFile as i32,
            10,
            container(CREATOR, GROUP, &picture_json()),
        )));
        assert_eq!(own_group.conv, group_key(CREATOR, GROUP).expect("a key"));
        assert_eq!(own_group.body, media);
        assert!(own_group.initial_out_state.is_some());
    }

    #[test]
    fn a_sticker_is_a_sticker_and_a_file_without_a_caption_has_none() {
        let sticker = Attachment {
            kind: AttachmentKind::Image,
            mime: "image/webp".to_owned(),
            size: 2048,
            dims: Some((512, 512)),
            state: AttachmentState::Remote,
            remote: Some(BackendBlob::new(1, sticker_json())),
            ..Attachment::default()
        };
        for (conv_group, envelope) in [
            (false, incoming(PEER, Ty::File as i32, 1, sticker_json())),
            (
                true,
                incoming(
                    PEER,
                    Ty::GroupFile as i32,
                    2,
                    container(CREATOR, GROUP, &sticker_json()),
                ),
            ),
            (
                false,
                outgoing(
                    Id::Contact(PEER.to_owned()),
                    Ty::File as i32,
                    3,
                    sticker_json(),
                ),
            ),
        ] {
            let m = the_message(run(&envelope));
            assert_eq!(m.body, Body::Sticker(Box::new(sticker.clone())));
            assert_eq!(
                m.conv,
                if conv_group {
                    group_key(CREATOR, GROUP).expect("a key")
                } else {
                    ConvKey::Direct(peer())
                }
            );
            assert!(m.raw.is_none());
        }
        // A file with no caption: `caption: None`.
        let plain = format!(
            "{{\"j\":0,\"k\":\"{KEY}\",\"b\":\"{BLOB}\",\"m\":\"application/pdf\",\"s\":7}}"
        )
        .into_bytes();
        let m = the_message(run(&incoming(PEER, Ty::File as i32, 4, plain.clone())));
        assert_eq!(
            m.body,
            Body::Media {
                attachments: vec![Attachment {
                    kind: AttachmentKind::File,
                    mime: "application/pdf".to_owned(),
                    size: 7,
                    state: AttachmentState::Remote,
                    remote: Some(BackendBlob::new(1, plain)),
                    ..Attachment::default()
                }],
                caption: None
            }
        );
    }

    #[test]
    fn a_caption_that_names_us_sets_mentions_me_only_for_the_others() {
        let json = |d: &str| {
            format!("{{\"j\":1,\"k\":\"{KEY}\",\"b\":\"{BLOB}\",\"m\":\"image/jpeg\",\"s\":7,\"d\":\"{d}\"}}")
                .into_bytes()
        };
        let named = json(&format!("hi @[{OWN}]"));
        assert!(the_message(run(&incoming(PEER, Ty::File as i32, 1, named.clone()))).mentions_me);
        // Our own message never mentions us; another person's mention does not.
        assert!(
            !the_message(run(&outgoing(
                Id::Contact(PEER.to_owned()),
                Ty::File as i32,
                2,
                named
            )))
            .mentions_me
        );
        let other = json(&format!("hi @[{PEER}]"));
        assert!(!the_message(run(&incoming(PEER, Ty::File as i32, 3, other))).mentions_me);
        // `@All` counts in a group only.
        let all = json("@[@@@@@@@@] look");
        assert!(!the_message(run(&incoming(PEER, Ty::File as i32, 4, all.clone()))).mentions_me);
        let in_group = container(CREATOR, GROUP, &all);
        assert!(the_message(run(&incoming(PEER, Ty::GroupFile as i32, 5, in_group))).mentions_me);
    }

    #[test]
    fn a_location_is_a_location_in_every_direction() {
        let body = b"48.8584,-122.4194,5\nHarbour\n1 Quay Street".to_vec();
        let location = Body::Location(Location {
            lat_e7: 488_584_000,
            lon_e7: -1_224_194_000,
            name: Some("Harbour".to_owned()),
            address: Some("1 Quay Street".to_owned()),
        });
        let m = the_message(run(&incoming(PEER, Ty::Location as i32, 1, body.clone())));
        assert_eq!(m.conv, ConvKey::Direct(peer()));
        assert_eq!(m.body, location);
        assert!(m.raw.is_none() && m.initial_out_state.is_none() && !m.mentions_me);
        let g = the_message(run(&incoming(
            PEER,
            Ty::GroupLocation as i32,
            2,
            container(CREATOR, GROUP, &body),
        )));
        assert_eq!(g.conv, group_key(CREATOR, GROUP).expect("a key"));
        assert_eq!(
            g.body, location,
            "the container's header is part of the place"
        );
        let own = the_message(run(&outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::Location as i32,
            3,
            body.clone(),
        )));
        assert_eq!(own.origin, Origin::OwnOtherDevice { device: None });
        assert_eq!(own.body, location);
        assert!(own.initial_out_state.is_some());
        let own_group = the_message(run(&outgoing(
            to_group(CREATOR, GROUP),
            Ty::GroupLocation as i32,
            4,
            container(CREATOR, GROUP, b"1,2"),
        )));
        assert_eq!(
            own_group.body,
            Body::Location(Location {
                lat_e7: 10_000_000,
                lon_e7: 20_000_000,
                name: None,
                address: None
            })
        );
    }

    #[test]
    fn a_file_or_a_location_that_cannot_be_read_is_unsupported_with_its_raw_body() {
        let unsupported = |hint: &str| Body::Unsupported {
            kind_hint: hint.to_owned(),
            summary: None,
        };
        // A file: not JSON, and JSON without its key.
        let no_key =
            format!("{{\"j\":1,\"b\":\"{BLOB}\",\"m\":\"image/jpeg\",\"s\":7}}").into_bytes();
        for bad in [b"not json".to_vec(), no_key, Vec::new()] {
            let m = the_message(run(&incoming(PEER, Ty::File as i32, 1, bad.clone())));
            assert_eq!(m.body, unsupported("file"));
            assert_eq!(m.raw, Some(BackendBlob::new(1, bad)), "the raw body");
        }
        // In a group the raw body is the whole container, as for a poll.
        let wrapped = container(CREATOR, GROUP, b"{}");
        let g = the_message(run(&incoming(
            PEER,
            Ty::GroupFile as i32,
            2,
            wrapped.clone(),
        )));
        assert_eq!(g.body, unsupported("file"));
        assert_eq!(g.raw, Some(BackendBlob::new(1, wrapped)));
        assert_eq!(g.conv, group_key(CREATOR, GROUP).expect("a key"));
        // Our own is an own unsupported row, Sending.
        let own = the_message(run(&outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::File as i32,
            3,
            b"x".to_vec(),
        )));
        assert_eq!(own.body, unsupported("file"));
        assert!(own.initial_out_state.is_some() && own.raw.is_some());
        // A location: out of range, one coordinate, not UTF-8.
        for bad in [
            b"91,0".to_vec(),
            b"1".to_vec(),
            vec![0xff, 0xfe],
            Vec::new(),
        ] {
            let m = the_message(run(&incoming(PEER, Ty::Location as i32, 4, bad.clone())));
            assert_eq!(m.body, unsupported("location"));
            assert_eq!(m.raw, Some(BackendBlob::new(1, bad)));
        }
        let g = the_message(run(&incoming(
            PEER,
            Ty::GroupLocation as i32,
            5,
            container(CREATOR, GROUP, b"0,181"),
        )));
        assert_eq!(g.body, unsupported("location"));
        assert_eq!(g.conv, group_key(CREATOR, GROUP).expect("a key"));
    }

    #[test]
    fn a_file_or_a_location_goes_through_place_like_a_text() {
        // R15: a mapped kind is no longer kept whatever its sender.
        for (ty, body) in [(Ty::File, picture_json()), (Ty::Location, b"1,2".to_vec())] {
            assert_eq!(
                run(&incoming(STRANGER, ty as i32, 1, body.clone())).dropped,
                Some("sender-unknown"),
                "{ty:?} from a stranger"
            );
            assert_eq!(
                run(&incoming("bad", ty as i32, 1, body.clone())).dropped,
                Some("sender-invalid")
            );
            let out = outgoing(Id::Contact(STRANGER.to_owned()), ty as i32, 1, body.clone());
            assert_eq!(run(&out).dropped, Some("contact-unknown"));
            // A plain body aimed at a group is a mismatch.
            let at_group = outgoing(to_group(CREATOR, GROUP), ty as i32, 1, body);
            assert_eq!(run(&at_group).dropped, Some("conversation-mismatch"));
        }
        for (ty, body, bare) in [
            // A group body without its container: the JSON's first eight bytes are no creator.
            (Ty::GroupFile, picture_json(), "container-creator"),
            (Ty::GroupLocation, b"1,2".to_vec(), "container-short"),
        ] {
            assert_eq!(
                run(&incoming(
                    PEER,
                    ty as i32,
                    1,
                    container(CREATOR, OTHER_GROUP, &body)
                ))
                .dropped,
                Some("group-unknown")
            );
            assert_eq!(
                run(&incoming(
                    STRANGER,
                    ty as i32,
                    1,
                    container(CREATOR, GROUP, &body)
                ))
                .dropped,
                Some("sender-unknown")
            );
            assert_eq!(
                run(&incoming(PEER, ty as i32, 1, body.clone())).dropped,
                Some(bare)
            );
            // A group body aimed at a contact is a mismatch; a container naming another group is one too.
            let at_contact = outgoing(
                Id::Contact(PEER.to_owned()),
                ty as i32,
                1,
                container(CREATOR, GROUP, &body),
            );
            assert_eq!(run(&at_contact).dropped, Some("conversation-mismatch"));
            let other = outgoing(
                to_group(CREATOR, GROUP),
                ty as i32,
                1,
                container(CREATOR, OTHER_GROUP, &body),
            );
            assert_eq!(run(&other).dropped, Some("group-mismatch"));
            // A group that is stored but not visible.
            let hidden = map(
                &incoming(PEER, ty as i32, 1, container(CREATOR, GROUP, &body)),
                &cx(false),
                &StoredGroup { visible: false },
            )
            .expect("maps");
            assert_eq!(hidden.dropped, Some("group-not-visible"));
        }
    }

    /// H5 (R15): a file and a location into an ARCHIVED chat bring it back, in either direction, a hidden
    /// contact's chat too; the start's re-map does not (round B, M-3).
    #[test]
    fn a_file_and_a_location_bring_an_archived_chat_back_and_a_remap_does_not() {
        let view = Hidden { archived: true };
        let remap = Cx {
            remap: true,
            ..cx(true)
        };
        let stranger = || ConvKey::Direct(UserId::from_parts("threema", STRANGER).expect("a user"));
        for (ty, body) in [(Ty::File, picture_json()), (Ty::Location, b"1,2".to_vec())] {
            for envelope in [
                incoming(PEER, ty as i32, 1, body.clone()),
                outgoing(Id::Contact(PEER.to_owned()), ty as i32, 2, body.clone()),
            ] {
                let mapped = map(&envelope, &cx(true), &view).expect("maps");
                match mapped.writes.as_slice() {
                    [Write::Contact(c)] => assert_eq!(
                        c.conversation_visibility,
                        Some(d2d_sync::ConversationVisibility::Normal as i32),
                        "{ty:?}: the record is not brought back"
                    ),
                    _ => panic!("{ty:?}: the bring-back did not write the record once"),
                }
                match mapped.events.as_slice() {
                    [
                        BackendEvent::ConversationUpsert(patch),
                        BackendEvent::Message(_),
                    ] => {
                        assert_eq!(patch.key, ConvKey::Direct(peer()));
                        assert_eq!(patch.archived, Some(false));
                    }
                    _ => panic!("{ty:?}: not [the bring-back, the message]"),
                }
                // A re-map: the message alone, no record written.
                let again = map(&envelope, &remap, &view).expect("maps");
                assert!(again.writes.is_empty(), "{ty:?}: a re-map wrote the record");
                assert!(matches!(
                    again.events.as_slice(),
                    [BackendEvent::Message(_)]
                ));
            }
            // A hidden contact's chat: E5's whole patch, brought back (archived false), the record written.
            let hidden = map(
                &incoming(STRANGER, ty as i32, 3, body.clone()),
                &cx(true),
                &view,
            )
            .expect("maps");
            assert_eq!(hidden.writes.len(), 1);
            match hidden.events.as_slice() {
                [
                    BackendEvent::ConversationUpsert(patch),
                    BackendEvent::Message(m),
                ] => {
                    assert_eq!(patch.key, stranger());
                    assert_eq!(patch.archived, Some(false));
                    assert_eq!(m.conv, stranger());
                }
                _ => panic!("{ty:?}: a hidden contact's message is not [chat, message]"),
            }
        }
    }

    /// A file whose one group (CREATOR's `GROUP`, PEER a member) is stored with this visibility (P-L4).
    struct VisibleGroup {
        visibility: d2d_sync::ConversationVisibility,
    }

    impl View for VisibleGroup {
        fn contact(&self, identity: &str) -> Result<bool> {
            Ok([PEER, CREATOR].contains(&identity))
        }

        fn group(&self, creator: &str, group_id: u64) -> Result<bool> {
            Ok(creator == CREATOR && group_id == GROUP)
        }

        fn stored_contact(&self, _: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(None)
        }

        fn stored_group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>> {
            Ok(
                (creator == CREATOR && group_id == GROUP).then(|| d2d_sync::Group {
                    user_state: Some(d2d_sync::group::UserState::Member as i32),
                    member_identities: Some(libthreema::protobuf::common::Identities {
                        identities: vec![PEER.to_owned()],
                    }),
                    conversation_visibility: Some(self.visibility as i32),
                    ..Default::default()
                }),
            )
        }

        fn blocked(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((2, 1))
        }

        no_b4_rows!();
    }

    /// P-L4 (the review's surviving mutant: `GroupLocation` and `GroupFile` out of `unarchives`), H5 for the
    /// GROUP forms: a group file and a group location, from a member and from our own other device, into an
    /// ARCHIVED group bring it back: `ConversationUpsert { archived: Some(false) }` FIRST in the message's row
    /// and the stored group written NORMAL in the same zone; a start's re-map does neither; a group that is
    /// not archived (NORMAL, PINNED) gets the message alone. The wrong types of the same chat (an edit, a
    /// reaction, a receipt) bring nothing back, in the same group.
    #[test]
    fn a_group_file_and_a_group_location_bring_an_archived_group_back_and_a_remap_does_not() {
        use d2d_sync::ConversationVisibility::{Archived, Normal, Pinned};
        let remap = Cx {
            remap: true,
            ..cx(true)
        };
        let archived = VisibleGroup {
            visibility: Archived,
        };
        for (ty, inner) in [
            (Ty::GroupFile, picture_json()),
            (Ty::GroupLocation, b"1,2".to_vec()),
        ] {
            for (what, envelope) in [
                (
                    "from a member",
                    incoming(PEER, ty as i32, 1, container(CREATOR, GROUP, &inner)),
                ),
                (
                    "from our other device",
                    outgoing(
                        to_group(CREATOR, GROUP),
                        ty as i32,
                        2,
                        container(CREATOR, GROUP, &inner),
                    ),
                ),
            ] {
                let mapped = map(&envelope, &cx(true), &archived).expect("maps");
                match mapped.writes.as_slice() {
                    [
                        Write::Group {
                            creator,
                            group_id,
                            sync,
                        },
                    ] => {
                        assert_eq!((creator.as_str(), *group_id), (CREATOR, GROUP));
                        assert_eq!(
                            sync.conversation_visibility,
                            Some(Normal as i32),
                            "{ty:?} {what}: the group record is not brought back"
                        );
                        assert_eq!(
                            sync.member_identities
                                .as_ref()
                                .map(|m| m.identities.clone()),
                            Some(vec![PEER.to_owned()]),
                            "{ty:?} {what}: the record lost a field"
                        );
                    }
                    _ => panic!("{ty:?} {what}: the bring-back did not write the group once"),
                }
                match mapped.events.as_slice() {
                    [
                        BackendEvent::ConversationUpsert(patch),
                        BackendEvent::Message(m),
                    ] => {
                        assert_eq!(patch.key, group_conv(), "{ty:?} {what}");
                        assert_eq!(patch.archived, Some(false), "{ty:?} {what}");
                        assert_eq!(m.conv, group_conv(), "{ty:?} {what}");
                    }
                    _ => panic!("{ty:?} {what}: not [the bring-back, the message]"),
                }
                // A re-map: the message alone, no record written.
                let again = map(&envelope, &remap, &archived).expect("maps");
                assert!(
                    again.writes.is_empty(),
                    "{ty:?} {what}: a re-map wrote the group"
                );
                assert!(
                    matches!(again.events.as_slice(), [BackendEvent::Message(_)]),
                    "{ty:?} {what}: a re-map brought the group back"
                );
                // A group that is not archived: the message alone.
                for visibility in [Normal, Pinned] {
                    let shown =
                        map(&envelope, &cx(true), &VisibleGroup { visibility }).expect("maps");
                    assert!(
                        shown.writes.is_empty()
                            && matches!(shown.events.as_slice(), [BackendEvent::Message(_)]),
                        "{ty:?} {what}: a group that is not archived brought something back"
                    );
                }
            }
        }
        // The modifiers of the archived group bring nothing back ("Unarchive: No").
        let reaction = wire::apply(TARGET, "\u{1f44d}".as_bytes());
        for envelope in [
            incoming(
                PEER,
                Ty::GroupReaction as i32,
                3,
                container(CREATOR, GROUP, &reaction),
            ),
            incoming(
                PEER,
                Ty::GroupDeliveryReceipt as i32,
                4,
                container(CREATOR, GROUP, &receipt_body(3, &[TARGET])),
            ),
        ] {
            let mapped = map(&envelope, &cx(true), &archived).expect("maps");
            assert!(mapped.writes.is_empty() && !mapped.events.is_empty());
            assert!(
                mapped
                    .events
                    .iter()
                    .all(|e| !matches!(e, BackendEvent::ConversationUpsert(_))),
                "a modifier or a receipt stated the archived group"
            );
        }
    }

    /// A chat that is not archived brings nothing back for a file or a location either.
    #[test]
    fn a_file_into_a_chat_that_is_not_archived_is_the_message_alone() {
        let view = Hidden { archived: false };
        let mapped = map(
            &incoming(PEER, Ty::File as i32, 1, picture_json()),
            &cx(true),
            &view,
        )
        .expect("maps");
        assert!(mapped.writes.is_empty());
        assert!(matches!(
            mapped.events.as_slice(),
            [BackendEvent::Message(_)]
        ));
    }

    // ---- TH5b B2: the edit, the delete and the reaction (R2, R6, R7, R8, R15) -------------------------

    use super::body::test_wire as wire;
    use kourier_model::{Text, TextRange};

    /// The id of the message the modifiers name: its eight wire bytes, and the text of its `local`.
    const TARGET: [u8; 8] = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88];
    const TARGET_LOCAL: &str = "1122334455667788";

    /// The edit's / delete's / reaction's own `created_at` (the envelope's), apart from `SERVER_AT` and `now_ms`.
    const MODIFIER_AT: i64 = 1_700_000_000_000;

    /// `GROUP` under `CREATOR`, written out: the creator, a dot, the 16 hex digits of the little-endian bytes.
    fn group_conv() -> ConvKey {
        ConvKey::Group("threema:A6MAP004.8877665544332211".into())
    }

    fn target_in(conv: ConvKey) -> MsgTarget {
        MsgTarget {
            conv: Some(conv),
            author: TargetAuthor::Unknown,
            local: TARGET_LOCAL.into(),
            alternates: Vec::new(),
        }
    }

    /// The one event of a mapped modifier: nothing deferred, dropped, warned or written.
    fn the_event(mapped: Mapped) -> BackendEvent {
        assert!(
            mapped.deferred.is_none()
                && mapped.dropped.is_none()
                && mapped.warned.is_none()
                && mapped.writes.is_empty(),
            "the modifier was not mapped to events alone"
        );
        match <[BackendEvent; 1]>::try_from(mapped.events) {
            Ok([event]) => event,
            Err(_) => panic!("not exactly one event"),
        }
    }

    /// The four places a modifier comes from, with the conversation its target must be in and the author it
    /// states: a contact's 1:1 modifier, our other device's to that contact, the contact's in the group, our
    /// other device's in the group.
    fn four(
        ty: Ty,
        group_ty: Ty,
        inner: &[u8],
    ) -> [(&'static str, d2d::Envelope, ConvKey, Option<UserId>); 4] {
        [
            (
                "in 1:1",
                incoming(PEER, ty as i32, 1, inner.to_vec()),
                ConvKey::Direct(peer()),
                Some(peer()),
            ),
            (
                "out 1:1",
                outgoing(Id::Contact(PEER.to_owned()), ty as i32, 1, inner.to_vec()),
                ConvKey::Direct(peer()),
                None,
            ),
            (
                "in group",
                incoming(PEER, group_ty as i32, 1, container(CREATOR, GROUP, inner)),
                group_conv(),
                Some(peer()),
            ),
            (
                "out group",
                outgoing(
                    to_group(CREATOR, GROUP),
                    group_ty as i32,
                    1,
                    container(CREATOR, GROUP, inner),
                ),
                group_conv(),
                None,
            ),
        ]
    }

    /// R6, R2: an edit is `Edited` with an unknown-author target in the conversation of its placement (the
    /// group's key for a group edit, the container's), `by` the sender or `None` for our other device, the new
    /// text read through mentions and markup, no new id, no quote statement, and its time the EDIT's own
    /// `created_at`.
    #[test]
    fn an_edit_is_edited_in_the_conversation_of_its_placement_at_its_own_time() {
        let inner = wire::edit(TARGET, "new *text*");
        for (what, envelope, conv, by) in four(Ty::EditMessage, Ty::GroupEditMessage, &inner) {
            assert_eq!(
                the_event(run(&envelope)),
                BackendEvent::Edited {
                    target: target_in(conv),
                    by,
                    new_local: None,
                    body: Body::Text(Text {
                        body: "new text".to_owned(),
                        ranges: vec![TextRange {
                            start: 4,
                            len: 4,
                            kind: RangeKind::Bold
                        }],
                    }),
                    edited_at: MODIFIER_AT,
                    keeps_quote: None,
                },
                "{what}"
            );
        }
    }

    /// R6: the quote prefix at the start of an edit is removed, a mention is read, an empty text is passed on
    /// (the core drops it for a text message and removes a caption).
    #[test]
    fn an_edit_reads_a_quote_prefix_a_mention_and_an_empty_text() {
        let text = format!("> quote #0a0a0a0a0a0a0a0a\n\nsee @[{CREATOR}]");
        let edited = |inner: &[u8]| match the_event(run(&incoming(
            PEER,
            Ty::EditMessage as i32,
            1,
            inner.to_vec(),
        ))) {
            BackendEvent::Edited { body, .. } => body,
            _ => panic!("not an edit"),
        };
        let Body::Text(shown) = edited(&wire::edit(TARGET, &text)) else {
            panic!("an edit is not a text")
        };
        assert_eq!(
            shown.body, "see \u{fffc}",
            "the prefix stayed or the mention did not turn"
        );
        assert_eq!(shown.ranges.len(), 1);
        assert_eq!((shown.ranges[0].start, shown.ranges[0].len), (4, 3));
        assert_eq!(
            edited(&wire::edit(TARGET, "")),
            Body::Text(Text {
                body: String::new(),
                ranges: Vec::new()
            }),
            "an empty edit is not passed on"
        );
    }

    /// R7, R2: a delete is `Deleted` for everyone, same targets, `by`, and its own `created_at`.
    #[test]
    fn a_delete_is_deleted_for_everyone_at_its_own_time() {
        let inner = wire::delete(TARGET);
        for (what, envelope, conv, by) in four(Ty::DeleteMessage, Ty::GroupDeleteMessage, &inner) {
            assert_eq!(
                the_event(run(&envelope)),
                BackendEvent::Deleted {
                    target: target_in(conv),
                    by,
                    for_everyone: true,
                    at: MODIFIER_AT,
                },
                "{what}"
            );
        }
    }

    /// R8, R2: `apply` adds the emoji, `withdraw` removes that emoji, at the reaction's own `created_at`;
    /// same targets and `by`.
    #[test]
    fn a_reaction_applies_or_withdraws_its_emoji_at_its_own_time() {
        for (remove, inner) in [
            (false, wire::apply(TARGET, "\u{1f44d}".as_bytes())),
            (true, wire::withdraw(TARGET, "\u{1f44d}".as_bytes())),
        ] {
            for (what, envelope, conv, by) in four(Ty::Reaction, Ty::GroupReaction, &inner) {
                assert_eq!(
                    the_event(run(&envelope)),
                    BackendEvent::Reaction {
                        target: target_in(conv),
                        by,
                        emoji: "\u{1f44d}".to_owned(),
                        remove,
                        at: MODIFIER_AT,
                    },
                    "{what} (remove {remove})"
                );
            }
        }
    }

    /// R6-R8: a protobuf that does not decode is dropped by its kind's rule, in every one of the four places;
    /// a reaction with no action is `reaction-invalid`, an emoji of no bytes, over 64 or not UTF-8
    /// `reaction-bytes`.
    #[test]
    fn a_modifier_that_cannot_be_read_is_dropped_by_its_rule() {
        let sixty_five = [b'x'; 65];
        for (ty, group_ty, inner, rule) in [
            (
                Ty::EditMessage,
                Ty::GroupEditMessage,
                vec![0x09, 1, 2, 3],
                "edit-invalid",
            ),
            (
                Ty::EditMessage,
                Ty::GroupEditMessage,
                vec![0x12, 2, 0xff, 0xfe],
                "edit-invalid",
            ),
            (
                Ty::DeleteMessage,
                Ty::GroupDeleteMessage,
                vec![0x09, 1],
                "delete-invalid",
            ),
            (
                Ty::Reaction,
                Ty::GroupReaction,
                vec![0x1a, 9, b'a'],
                "reaction-invalid",
            ),
            (
                Ty::Reaction,
                Ty::GroupReaction,
                wire::no_action(TARGET),
                "reaction-invalid",
            ),
            (
                Ty::Reaction,
                Ty::GroupReaction,
                wire::apply(TARGET, &sixty_five),
                "reaction-bytes",
            ),
            (
                Ty::Reaction,
                Ty::GroupReaction,
                wire::withdraw(TARGET, b""),
                "reaction-bytes",
            ),
            (
                Ty::Reaction,
                Ty::GroupReaction,
                wire::apply(TARGET, &[0xff, 0xfe]),
                "reaction-bytes",
            ),
        ] {
            for (what, envelope, _, _) in four(ty, group_ty, &inner) {
                let mapped = run(&envelope);
                assert_eq!(mapped.dropped, Some(rule), "{ty:?} {what}");
                assert!(
                    mapped.events.is_empty()
                        && mapped.deferred.is_none()
                        && mapped.writes.is_empty(),
                    "{ty:?} {what}: a dropped modifier left something"
                );
            }
        }
    }

    /// R15: a modifier goes through `place` like a text: the sender must be a stored contact, an own message to
    /// a stored contact, the group a stored one, the container agree with the envelope, the time fit.
    #[test]
    fn a_modifier_goes_through_place_like_a_text() {
        for (ty, group_ty, inner) in [
            (
                Ty::EditMessage,
                Ty::GroupEditMessage,
                wire::edit(TARGET, "x"),
            ),
            (
                Ty::DeleteMessage,
                Ty::GroupDeleteMessage,
                wire::delete(TARGET),
            ),
            (Ty::Reaction, Ty::GroupReaction, wire::apply(TARGET, b"+")),
        ] {
            let (t, g) = (ty as i32, group_ty as i32);
            let dropped = |envelope: &d2d::Envelope| run(envelope).dropped;
            assert_eq!(
                dropped(&incoming(STRANGER, t, 1, inner.clone())),
                Some("sender-unknown")
            );
            assert_eq!(
                dropped(&incoming("bad", t, 1, inner.clone())),
                Some("sender-invalid")
            );
            assert_eq!(
                dropped(&outgoing(
                    Id::Contact(STRANGER.to_owned()),
                    t,
                    1,
                    inner.clone()
                )),
                Some("contact-unknown")
            );
            // A plain body aimed at a group, a group body aimed at a contact.
            assert_eq!(
                dropped(&outgoing(to_group(CREATOR, GROUP), t, 1, inner.clone())),
                Some("conversation-mismatch")
            );
            let grouped = container(CREATOR, GROUP, &inner);
            assert_eq!(
                dropped(&outgoing(
                    Id::Contact(PEER.to_owned()),
                    g,
                    1,
                    grouped.clone()
                )),
                Some("conversation-mismatch")
            );
            // A group the file lacks, a stranger inside a group, a container that names another group.
            assert_eq!(
                dropped(&incoming(
                    PEER,
                    g,
                    1,
                    container(CREATOR, OTHER_GROUP, &inner)
                )),
                Some("group-unknown")
            );
            assert_eq!(
                dropped(&incoming(STRANGER, g, 1, grouped.clone())),
                Some("sender-unknown")
            );
            assert_eq!(
                dropped(&outgoing(
                    to_group(CREATOR, GROUP),
                    g,
                    1,
                    container(CREATOR, OTHER_GROUP, &inner)
                )),
                Some("group-mismatch")
            );
            // A group body too short for its container.
            assert_eq!(
                dropped(&incoming(PEER, g, 1, vec![1; 5])),
                Some("container-short")
            );
            // A time that does not fit a signed 64-bit time.
            let mut late = incoming(PEER, t, 1, inner.clone());
            if let Some(Content::IncomingMessage(m)) = &mut late.content {
                m.created_at = u64::MAX;
            }
            assert_eq!(dropped(&late), Some("created-at"));
        }
    }

    /// R15: an edit, a delete and a reaction bring no archived chat back (the spec's "Unarchive: No") and state
    /// no chat, not even for a contact the phone hides (E5 is for messages that can open a chat): the one
    /// event, no BLOB written.
    #[test]
    fn a_modifier_brings_no_archived_chat_back_and_states_no_chat() {
        let view = Hidden { archived: true };
        for (ty, inner) in [
            (Ty::EditMessage, wire::edit(TARGET, "x")),
            (Ty::DeleteMessage, wire::delete(TARGET)),
            (Ty::Reaction, wire::apply(TARGET, b"+")),
        ] {
            for envelope in [
                incoming(PEER, ty as i32, 1, inner.clone()),
                incoming(STRANGER, ty as i32, 1, inner.clone()),
                outgoing(
                    Id::Contact(STRANGER.to_owned()),
                    ty as i32,
                    1,
                    inner.clone(),
                ),
            ] {
                for catch_up in [false, true] {
                    let mapped = map(&envelope, &cx(catch_up), &view).expect("maps");
                    assert!(mapped.writes.is_empty(), "{ty:?}: a BLOB was written");
                    assert!(
                        matches!(
                            mapped.events.as_slice(),
                            [BackendEvent::Edited { .. }
                                | BackendEvent::Deleted { .. }
                                | BackendEvent::Reaction { .. }]
                        ),
                        "{ty:?}: not the one modifier event"
                    );
                }
            }
        }
    }

    /// The re-map reads them too, with the same events as at arrival.
    #[test]
    fn a_remap_maps_a_modifier_like_an_arrival() {
        let envelope = incoming(PEER, Ty::DeleteMessage as i32, 1, wire::delete(TARGET));
        let at_arrival = run(&envelope);
        let mut remapped_cx = cx(true);
        remapped_cx.remap = true;
        let remapped = map(&envelope, &remapped_cx, &Known).expect("maps");
        assert_eq!(remapped.events, at_arrival.events);
    }

    // ---- TH5b B3: the receipts, the sent and read updates, typing (R2, R9-R12, R15) --------------------

    /// A second message id the receipts name, and its text.
    const SECOND: [u8; 8] = [0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x01];
    const SECOND_LOCAL: &str = "99aabbccddeeff01";

    /// A `delivery-receipt` body: the status byte and the ids' wire bytes, written by hand.
    fn receipt_body(status: u8, ids: &[[u8; 8]]) -> Vec<u8> {
        let mut body = vec![status];
        for id in ids {
            body.extend_from_slice(id);
        }
        body
    }

    fn own() -> UserId {
        UserId::from_parts("threema", OWN).expect("a user")
    }

    fn target_of(conv: &ConvKey, local: &str, author: TargetAuthor) -> MsgTarget {
        MsgTarget {
            conv: Some(conv.clone()),
            author,
            local: local.into(),
            alternates: Vec::new(),
        }
    }

    /// R9: a peer's receipt of OUR messages in the 1:1 chat: status 1 is `Delivered`, 2 is `Read`, by the
    /// peer, every id one target authored by us in the direct chat (in the body's order), at the receipt's own
    /// `created_at` (R2: not the reflection's `SERVER_AT`).
    #[test]
    fn an_incoming_receipt_is_delivered_or_read_by_the_peer_for_our_messages() {
        let direct = ConvKey::Direct(peer());
        for (status, kind) in [(1, ReceiptKind::Delivered), (2, ReceiptKind::Read)] {
            let envelope = incoming(
                PEER,
                Ty::DeliveryReceipt as i32,
                1,
                receipt_body(status, &[TARGET, SECOND]),
            );
            assert_eq!(
                the_event(run(&envelope)),
                BackendEvent::Receipts {
                    targets: vec![
                        target_of(&direct, TARGET_LOCAL, TargetAuthor::Own),
                        target_of(&direct, SECOND_LOCAL, TargetAuthor::Own),
                    ],
                    by: peer(),
                    kind,
                    at: MODIFIER_AT,
                },
                "status {status}"
            );
        }
    }

    /// R9: our phone's receipt to the peer: status 2 is OUR read of the peer's messages (`by` our own ID, the
    /// targets' author unknown); status 1 states nothing and is dropped QUIETLY `receipt-own-delivered`.
    #[test]
    fn an_outgoing_read_receipt_is_our_own_read_and_an_own_delivered_one_is_quiet() {
        let direct = ConvKey::Direct(peer());
        let read = outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::DeliveryReceipt as i32,
            1,
            receipt_body(2, &[TARGET]),
        );
        assert_eq!(
            the_event(run(&read)),
            BackendEvent::Receipts {
                targets: vec![target_of(&direct, TARGET_LOCAL, TargetAuthor::Unknown)],
                by: own(),
                kind: ReceiptKind::Read,
                at: MODIFIER_AT,
            }
        );
        let delivered = run(&outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::DeliveryReceipt as i32,
            1,
            receipt_body(1, &[TARGET, SECOND]),
        ));
        assert!(
            mapped_nothing(&delivered),
            "our own received receipt stated something"
        );
        assert_eq!(delivered.dropped, Some("receipt-own-delivered"));
        assert!(classify::is_quiet("receipt-own-delivered"));
    }

    fn mapped_nothing(mapped: &Mapped) -> bool {
        mapped.events.is_empty() && mapped.deferred.is_none() && mapped.writes.is_empty()
    }

    /// R9, C13, P-L2: a deprecated reaction (status 3 thumbs up, 4 thumbs down) in each of the four places: per
    /// id the removal of every reaction of the sender at `created_at - 1`, then the emoji at `created_at`; `by`
    /// the sender, `None` for our own other device. The target's author is the side the spec binds in a 1:1
    /// chat (steps 1.3 of the Common Incoming / Outgoing Delivery Receipt Steps): the PEER's receipt reacts to
    /// OUR message (`Own`), ours to the PEER's (`User(peer)`); in a group the wire binds nothing (unknown).
    #[test]
    fn a_legacy_reaction_receipt_is_the_removal_then_the_emoji_in_every_place() {
        for (status, emoji) in [(3u8, "\u{1f44d}"), (4, "\u{1f44e}")] {
            let inner = receipt_body(status, &[TARGET, SECOND]);
            let authors = [
                ("in 1:1", TargetAuthor::Own),
                ("out 1:1", TargetAuthor::User(peer())),
                ("in group", TargetAuthor::Unknown),
                ("out group", TargetAuthor::Unknown),
            ];
            for ((what, envelope, conv, by), (named, author)) in
                four(Ty::DeliveryReceipt, Ty::GroupDeliveryReceipt, &inner)
                    .into_iter()
                    .zip(authors)
            {
                assert_eq!(what, named, "the places are not in the table's order");
                let mapped = run(&envelope);
                assert!(mapped.dropped.is_none(), "{what}: dropped");
                let pair = |local: &str| {
                    let target = target_of(&conv, local, author.clone());
                    [
                        BackendEvent::Reaction {
                            target: target.clone(),
                            by: by.clone(),
                            emoji: String::new(),
                            remove: true,
                            at: MODIFIER_AT - 1,
                        },
                        BackendEvent::Reaction {
                            target,
                            by: by.clone(),
                            emoji: emoji.to_owned(),
                            remove: false,
                            at: MODIFIER_AT,
                        },
                    ]
                };
                let expected: Vec<BackendEvent> = [pair(TARGET_LOCAL), pair(SECOND_LOCAL)].concat();
                assert_eq!(mapped.events, expected, "{what}, status {status}");
            }
        }
    }

    /// R9: a body with no id or with a part of an id is `receipt-short` (a 1-byte body, 10 bytes, an empty one);
    /// a 9-byte body is ONE id. A 1:1 status outside 1-4 is `receipt-status`; a GROUP receipt that is no
    /// reaction (status 1, 2, 0 or 5) is `group-receipt`, in both directions.
    #[test]
    fn a_receipt_that_is_short_or_of_another_status_is_dropped_by_its_rule() {
        let one_to_one = |body: Vec<u8>| run(&incoming(PEER, Ty::DeliveryReceipt as i32, 1, body));
        for body in [
            vec![],
            vec![1],
            [vec![1], TARGET.to_vec(), vec![7]].concat(),
        ] {
            let mapped = one_to_one(body.clone());
            assert!(
                mapped_nothing(&mapped),
                "{} bytes stated something",
                body.len()
            );
            assert_eq!(
                mapped.dropped,
                Some("receipt-short"),
                "{} bytes",
                body.len()
            );
        }
        let nine = one_to_one(receipt_body(1, &[TARGET]));
        assert!(
            matches!(nine.events.as_slice(), [BackendEvent::Receipts { targets, .. }] if targets.len() == 1),
            "a 9-byte receipt is not one target"
        );
        for status in [0u8, 5, 0xff] {
            let mapped = one_to_one(receipt_body(status, &[TARGET]));
            assert!(mapped_nothing(&mapped));
            assert_eq!(mapped.dropped, Some("receipt-status"), "status {status}");
            let out = run(&outgoing(
                Id::Contact(PEER.to_owned()),
                Ty::DeliveryReceipt as i32,
                1,
                receipt_body(status, &[TARGET]),
            ));
            assert_eq!(out.dropped, Some("receipt-status"), "own status {status}");
        }
        for status in [1u8, 2, 0, 5] {
            let inner = receipt_body(status, &[TARGET]);
            for (what, envelope) in [
                (
                    "in",
                    incoming(
                        PEER,
                        Ty::GroupDeliveryReceipt as i32,
                        1,
                        container(CREATOR, GROUP, &inner),
                    ),
                ),
                (
                    "out",
                    outgoing(
                        to_group(CREATOR, GROUP),
                        Ty::GroupDeliveryReceipt as i32,
                        1,
                        container(CREATOR, GROUP, &inner),
                    ),
                ),
            ] {
                let mapped = run(&envelope);
                assert!(mapped_nothing(&mapped), "{what} {status}");
                assert_eq!(
                    mapped.dropped,
                    Some("group-receipt"),
                    "{what}, status {status}"
                );
            }
        }
        // A short group receipt is `receipt-short`, before its status is read.
        let short = run(&incoming(
            PEER,
            Ty::GroupDeliveryReceipt as i32,
            1,
            container(CREATOR, GROUP, &[3]),
        ));
        assert_eq!(short.dropped, Some("receipt-short"));
    }

    /// R15: a receipt is placed like a text: a stranger's is `sender-unknown`, one in a group the file lacks
    /// `group-unknown`, a 1:1 receipt aimed at a group (and a group one at a contact) `conversation-mismatch`, an
    /// own one to an unknown contact `contact-unknown`, a time over `i64::MAX` `created-at`.
    #[test]
    fn a_receipt_goes_through_place_like_a_text() {
        let read = receipt_body(2, &[TARGET]);
        let thumbs = receipt_body(3, &[TARGET]);
        let cases = [
            (
                incoming(STRANGER, Ty::DeliveryReceipt as i32, 1, read.clone()),
                "sender-unknown",
            ),
            (
                incoming(
                    PEER,
                    Ty::GroupDeliveryReceipt as i32,
                    1,
                    container(CREATOR, OTHER_GROUP, &thumbs),
                ),
                "group-unknown",
            ),
            (
                outgoing(
                    to_group(CREATOR, GROUP),
                    Ty::DeliveryReceipt as i32,
                    1,
                    read.clone(),
                ),
                "conversation-mismatch",
            ),
            (
                outgoing(
                    Id::Contact(PEER.to_owned()),
                    Ty::GroupDeliveryReceipt as i32,
                    1,
                    container(CREATOR, GROUP, &thumbs),
                ),
                "conversation-mismatch",
            ),
            (
                outgoing(
                    Id::Contact(STRANGER.to_owned()),
                    Ty::DeliveryReceipt as i32,
                    1,
                    read.clone(),
                ),
                "contact-unknown",
            ),
        ];
        for (envelope, rule) in cases {
            let mapped = run(&envelope);
            assert!(mapped_nothing(&mapped), "{rule}: stated something");
            assert_eq!(mapped.dropped, Some(rule));
        }
        let mut late = incoming(PEER, Ty::DeliveryReceipt as i32, 1, read);
        if let Some(Content::IncomingMessage(m)) = &mut late.content {
            m.created_at = u64::MAX;
        }
        assert_eq!(run(&late).dropped, Some("created-at"));
    }

    /// A receipt brings no archived chat back and states no chat, not even for a hidden contact (like a
    /// modifier: "Unarchive: No"; E5 is for messages).
    #[test]
    fn a_receipt_brings_no_archived_chat_back_and_states_no_chat() {
        let view = Hidden { archived: true };
        for envelope in [
            incoming(
                STRANGER,
                Ty::DeliveryReceipt as i32,
                1,
                receipt_body(2, &[TARGET]),
            ),
            incoming(
                STRANGER,
                Ty::DeliveryReceipt as i32,
                1,
                receipt_body(3, &[TARGET]),
            ),
        ] {
            let mapped = map(&envelope, &cx(false), &view).expect("maps");
            assert!(mapped.writes.is_empty(), "a BLOB was written");
            assert!(
                mapped.events.iter().all(|e| matches!(
                    e,
                    BackendEvent::Receipts { .. } | BackendEvent::Reaction { .. }
                )),
                "a receipt stated a chat"
            );
            assert!(!mapped.events.is_empty());
        }
    }

    /// A member of the update tests' group (and no contact of the file).
    const MEMBER: &str = "A6MAP005";
    /// A group we created alone, and a group we left that lists nobody (not shown, M1).
    const OUR_GROUP: u64 = 0x0000_0000_0000_0007;
    const LEFT_GROUP: u64 = 0x0000_0000_0000_0009;
    /// A group of ours with PEER in it (not a notes group).
    const OUR_SHARED_GROUP: u64 = 0x0000_0000_0000_000b;

    /// The update tests' file: the contacts PEER and CREATOR, GROUP of CREATOR whose stored members are MEMBER,
    /// PEER and (wrongly listed, a belt) us; OUR_GROUP of ours with no other member (a NOTES group);
    /// OUR_SHARED_GROUP of ours with PEER; LEFT_GROUP left and empty.
    struct Members;

    impl View for Members {
        fn contact(&self, identity: &str) -> Result<bool> {
            Known.contact(identity)
        }

        fn group(&self, creator: &str, group_id: u64) -> Result<bool> {
            Ok(self.stored_group(creator, group_id)?.is_some())
        }

        fn stored_contact(&self, _: &str) -> Result<Option<d2d_sync::Contact>> {
            Ok(None)
        }

        fn stored_group(&self, creator: &str, group_id: u64) -> Result<Option<d2d_sync::Group>> {
            use libthreema::protobuf::common::Identities;
            let group = |members: &[&str], left: bool| d2d_sync::Group {
                member_identities: Some(Identities {
                    identities: members.iter().map(|m| (*m).to_owned()).collect(),
                }),
                user_state: left.then_some(d2d_sync::group::UserState::Left as i32),
                ..Default::default()
            };
            Ok(match (creator, group_id) {
                (CREATOR, GROUP) => Some(group(&[MEMBER, PEER, OWN], false)),
                (OWN, OUR_GROUP) => Some(group(&[], false)),
                (OWN, OUR_SHARED_GROUP) => Some(group(&[PEER], false)),
                (CREATOR, LEFT_GROUP) => Some(group(&[], true)),
                _ => None,
            })
        }

        fn blocked(&self, _: &str) -> Result<bool> {
            Ok(false)
        }

        fn sync_counts(&self) -> Result<(u64, u64)> {
            Ok((2, 3))
        }

        no_b4_rows!();
    }

    fn sent_of(updates: Vec<(Option<Id>, u64, bool)>) -> d2d::Envelope {
        use d2d::outgoing_message_update::{Sent, Update, update};
        d2d::Envelope {
            content: Some(Content::OutgoingMessageUpdate(d2d::OutgoingMessageUpdate {
                updates: updates
                    .into_iter()
                    .map(|(id, message_id, sent)| Update {
                        conversation: id.map(|id| d2d::ConversationId { id: Some(id) }),
                        message_id,
                        update: sent.then_some(update::Update::Sent(Sent {})),
                    })
                    .collect(),
            })),
            ..Default::default()
        }
    }

    fn user_of(id: &str) -> UserId {
        UserId::from_parts("threema", id).expect("a user")
    }

    fn sent_state(conv: &ConvKey, local: &str, recipients: &[&str]) -> BackendEvent {
        BackendEvent::OwnSendState {
            local: None,
            target: target_of(conv, local, TargetAuthor::Own),
            per_recipient: recipients
                .iter()
                .map(|r| (user_of(r), RecipientState::Sent { sealed: false }))
                .collect(),
        }
    }

    /// R10, C9: a `sent` update of a 1:1 message is `OwnSendState { local: None }` of our message in the direct
    /// chat with the peer `Sent { sealed: false }`; of a group message, every stored member but us, the creator
    /// added (the spec's list holds neither the creator nor the user), sorted by ID. Both in ONE envelope, in
    /// order, nothing skipped.
    #[test]
    fn a_sent_update_is_sent_for_the_contact_or_every_member_but_us() {
        let envelope = sent_of(vec![
            (
                Some(Id::Contact(PEER.to_owned())),
                u64::from_le_bytes(TARGET),
                true,
            ),
            (
                Some(to_group(CREATOR, GROUP)),
                u64::from_le_bytes(SECOND),
                true,
            ),
        ]);
        let mapped = map(&envelope, &cx(false), &Members).expect("maps");
        assert!(mapped.skipped.is_empty() && mapped.dropped.is_none() && mapped.deferred.is_none());
        assert_eq!(
            mapped.events,
            vec![
                sent_state(&ConvKey::Direct(peer()), TARGET_LOCAL, &[PEER]),
                sent_state(&group_conv(), SECOND_LOCAL, &[PEER, CREATOR, MEMBER]),
            ]
        );
    }

    /// R10: an update to a contact or a group the file lacks, to a group not shown (left, nobody listed), with no
    /// conversation or with no `sent` is SKIPPED and counted `update-unknown`; the good one among them is the one
    /// event. An envelope of skips alone states nothing (acked with nothing) and defers nothing. (A list and a
    /// notes group are not unknown: P-L1 and P-M1 below.)
    #[test]
    fn a_sent_update_to_an_unknown_or_an_unshown_conversation_is_skipped_and_counted() {
        let id = u64::from_le_bytes(TARGET);
        let skips = vec![
            (Some(Id::Contact(STRANGER.to_owned())), id, true),
            (Some(Id::Contact("not an ID".to_owned())), id, true),
            (Some(to_group(CREATOR, OTHER_GROUP)), id, true),
            (Some(to_group(CREATOR, LEFT_GROUP)), id, true),
            (None, id, true),
            (Some(Id::Contact(PEER.to_owned())), id, false),
        ];
        let alone = map(&sent_of(skips.clone()), &cx(false), &Members).expect("maps");
        assert!(mapped_nothing(&alone) && alone.dropped.is_none());
        assert_eq!(alone.skipped, vec![("update-unknown", 6)]);
        let mut with_one = skips;
        with_one.insert(3, (Some(Id::Contact(PEER.to_owned())), id, true));
        let mapped = map(&sent_of(with_one), &cx(false), &Members).expect("maps");
        assert_eq!(
            mapped.events,
            vec![sent_state(&ConvKey::Direct(peer()), TARGET_LOCAL, &[PEER])]
        );
        assert_eq!(mapped.skipped, vec![("update-unknown", 6)]);
        assert!(mapped.deferred.is_none() && mapped.dropped.is_none());
    }

    /// P-M1 (the spec's notes group, `csp.struct.yml` "Notes Group" and footnote 2 of the Bundled Messages Send
    /// Steps): the Sent update of a message in a group of ours with NOBODY else has no receiver; it is skipped
    /// QUIETLY under `sent-notes-group` (a quiet rule), not as `update-unknown`, and the group of ours WITH a
    /// member is stated as ever. Nothing is deferred.
    #[test]
    fn a_sent_update_of_a_notes_group_is_skipped_quietly() {
        let id = u64::from_le_bytes(TARGET);
        let alone = map(
            &sent_of(vec![(Some(to_group(OWN, OUR_GROUP)), id, true)]),
            &cx(false),
            &Members,
        )
        .expect("maps");
        assert!(mapped_nothing(&alone) && alone.dropped.is_none());
        assert_eq!(alone.skipped, vec![("sent-notes-group", 1)]);
        assert!(classify::is_quiet("sent-notes-group"));
        // Beside an update that is stated and one that is unknown: each counted under its own rule.
        let mixed = map(
            &sent_of(vec![
                (Some(to_group(OWN, OUR_GROUP)), id, true),
                (Some(to_group(OWN, OUR_SHARED_GROUP)), id, true),
                (Some(to_group(OWN, OUR_GROUP)), id, true),
                (Some(Id::Contact(STRANGER.to_owned())), id, true),
            ]),
            &cx(false),
            &Members,
        )
        .expect("maps");
        let shared = group_key(OWN, OUR_SHARED_GROUP).expect("a key");
        assert_eq!(
            mixed.events,
            vec![sent_state(&shared, TARGET_LOCAL, &[PEER])],
            "a group of ours with a member is not stated"
        );
        assert_eq!(
            mixed.skipped,
            vec![("update-unknown", 1), ("sent-notes-group", 2)]
        );
        assert!(mixed.deferred.is_none() && mixed.dropped.is_none());
    }

    /// P-L1: a Sent update that names a DISTRIBUTION LIST is not stated and not counted; the envelope is ALSO
    /// deferred as `list` (the later lists step maps its 1:1 copies), whatever else it holds: the contact and
    /// group updates beside it are stated now, as ever, the unknown ones counted. A list update with no `sent`
    /// is an unknown update (nothing a later step could use) and defers nothing.
    #[test]
    fn a_sent_update_that_names_a_list_is_stated_and_deferred_as_list() {
        let id = u64::from_le_bytes(TARGET);
        let list_only = map(
            &sent_of(vec![(Some(Id::DistributionList(5)), id, true)]),
            &cx(false),
            &Members,
        )
        .expect("maps");
        assert_eq!(list_only.deferred, Some("list"));
        assert!(list_only.events.is_empty() && list_only.writes.is_empty());
        assert!(
            list_only.skipped.is_empty() && list_only.dropped.is_none(),
            "the list part was counted"
        );
        let mixed = map(
            &sent_of(vec![
                (Some(Id::Contact(PEER.to_owned())), id, true),
                (Some(Id::DistributionList(5)), id, true),
                (
                    Some(to_group(CREATOR, GROUP)),
                    u64::from_le_bytes(SECOND),
                    true,
                ),
                (Some(Id::DistributionList(6)), id, true),
                (Some(Id::Contact(STRANGER.to_owned())), id, true),
            ]),
            &cx(false),
            &Members,
        )
        .expect("maps");
        assert_eq!(mixed.deferred, Some("list"));
        assert_eq!(
            mixed.events,
            vec![
                sent_state(&ConvKey::Direct(peer()), TARGET_LOCAL, &[PEER]),
                sent_state(&group_conv(), SECOND_LOCAL, &[PEER, CREATOR, MEMBER]),
            ],
            "the contact and the group are stated beside the list"
        );
        assert_eq!(mixed.skipped, vec![("update-unknown", 1)]);
        let no_sent = map(
            &sent_of(vec![(Some(Id::DistributionList(5)), id, false)]),
            &cx(false),
            &Members,
        )
        .expect("maps");
        assert!(no_sent.deferred.is_none());
        assert_eq!(no_sent.skipped, vec![("update-unknown", 1)]);
        // A re-mapped TH5a row maps the same.
        let mut remapped_cx = cx(true);
        remapped_cx.remap = true;
        let again = map(
            &sent_of(vec![
                (Some(Id::Contact(PEER.to_owned())), id, true),
                (Some(Id::DistributionList(5)), id, true),
            ]),
            &remapped_cx,
            &Members,
        )
        .expect("maps");
        assert_eq!(again.deferred, Some("list"));
        assert_eq!(again.events.len(), 1);
    }

    /// P-M1: an own message into a NOTES group (a group of ours with nobody else) is stated `Sent` at once, with
    /// the reflection's time (the phone reflects a Sent update with no receivers and the update's mapper skips
    /// it, so a `Sending` floor would never move: the core moves a row only through per-recipient states). Every
    /// own kind, live and re-mapped. A group of ours WITH a member and a group of another creator stay
    /// `Sending`; a group the file has no record of (the plain `Known` view) too.
    #[test]
    fn an_own_message_into_a_notes_group_is_sent_at_once() {
        let sent = Some(OutgoingState::Sent {
            server_at: Some(i64::try_from(SERVER_AT).expect("fits")),
        });
        let sending = Some(OutgoingState::Sending { progress: None });
        let state = |view: &dyn View, cx: &Cx<'_>, group: (&str, u64), ty: Ty, inner: &[u8]| {
            let envelope = outgoing(
                to_group(group.0, group.1),
                ty as i32,
                1,
                container(group.0, group.1, inner),
            );
            the_message(map(&envelope, cx, view).expect("maps")).initial_out_state
        };
        for (ty, inner) in [
            (Ty::GroupText, b"note".to_vec()),
            (Ty::GroupLocation, b"1,2".to_vec()),
            (Ty::GroupFile, picture_json()),
            (Ty::GroupPollSetup, vec![9; 10]),
        ] {
            for cx in [
                cx(false),
                Cx {
                    remap: true,
                    ..cx(true)
                },
            ] {
                assert_eq!(
                    state(&Members, &cx, (OWN, OUR_GROUP), ty, &inner),
                    sent,
                    "{ty:?}: a notes group's message is not Sent"
                );
                assert_eq!(
                    state(&Members, &cx, (OWN, OUR_SHARED_GROUP), ty, &inner),
                    sending,
                    "{ty:?}: a group of ours with a member is not Sending"
                );
            }
            assert_eq!(
                state(&Members, &cx(false), (CREATOR, GROUP), ty, &inner),
                sending,
                "{ty:?}: a group of another creator is not Sending"
            );
        }
        // No record of the group in the view: Sending (the plain text test's view holds none).
        let plain = outgoing(
            to_group(CREATOR, GROUP),
            Ty::GroupText as i32,
            1,
            container(CREATOR, GROUP, b"x"),
        );
        assert_eq!(
            the_message(run(&plain)).initial_out_state,
            sending,
            "a group with no record is not Sending"
        );
        // A 1:1 own message is Sending as ever.
        let direct = outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::Text as i32,
            1,
            b"x".to_vec(),
        );
        assert_eq!(
            the_message(map(&direct, &cx(false), &Members).expect("maps")).initial_out_state,
            sending
        );
    }

    fn read_of(updates: Vec<(Option<Id>, u64, Option<u64>)>) -> d2d::Envelope {
        use d2d::incoming_message_update::{Read, Update, update};
        d2d::Envelope {
            content: Some(Content::IncomingMessageUpdate(d2d::IncomingMessageUpdate {
                updates: updates
                    .into_iter()
                    .map(|(id, message_id, at)| Update {
                        conversation: id.map(|id| d2d::ConversationId { id: Some(id) }),
                        message_id,
                        update: at.map(|at| update::Update::Read(Read { at })),
                    })
                    .collect(),
            })),
            ..Default::default()
        }
    }

    /// R11, R2: a `read` update is ONE `Receipts { Read }` by our own ID per (conversation, `Read.at`) in the
    /// order first seen, its targets the messages in order (author unknown), dated by `Read.at`; the skips are
    /// counted as R10's (a list, a stranger, no `read`, a time over `i64::MAX`).
    #[test]
    fn a_read_update_is_one_receipt_per_conversation_and_time_in_first_seen_order() {
        const T1: u64 = 1_700_000_100_000;
        const T2: u64 = 1_700_000_200_000;
        let (a, b, c) = (
            u64::from_le_bytes(TARGET),
            u64::from_le_bytes(SECOND),
            u64::from_le_bytes([0x05; 8]),
        );
        let direct = || Some(Id::Contact(PEER.to_owned()));
        let group = || Some(to_group(CREATOR, GROUP));
        let envelope = read_of(vec![
            (direct(), a, Some(T1)),
            (group(), c, Some(T1)),
            (Some(Id::DistributionList(5)), a, Some(T1)),
            (direct(), b, Some(T2)),
            (direct(), c, Some(T1)),
            (Some(Id::Contact(STRANGER.to_owned())), a, Some(T1)),
            (group(), a, None),
            (direct(), a, Some(u64::MAX)),
        ]);
        let mapped = map(&envelope, &cx(false), &Members).expect("maps");
        let peer_chat = ConvKey::Direct(peer());
        let unknown = |conv: &ConvKey, local: &str| target_of(conv, local, TargetAuthor::Unknown);
        let at = |t: u64| i64::try_from(t).expect("fits");
        assert_eq!(
            mapped.events,
            vec![
                BackendEvent::Receipts {
                    targets: vec![
                        unknown(&peer_chat, TARGET_LOCAL),
                        unknown(&peer_chat, "0505050505050505"),
                    ],
                    by: own(),
                    kind: ReceiptKind::Read,
                    at: at(T1),
                },
                BackendEvent::Receipts {
                    targets: vec![unknown(&group_conv(), "0505050505050505")],
                    by: own(),
                    kind: ReceiptKind::Read,
                    at: at(T1),
                },
                BackendEvent::Receipts {
                    targets: vec![unknown(&peer_chat, SECOND_LOCAL)],
                    by: own(),
                    kind: ReceiptKind::Read,
                    at: at(T2),
                },
            ]
        );
        assert_eq!(mapped.skipped, vec![("update-unknown", 4)]);
        assert!(mapped.dropped.is_none() && mapped.deferred.is_none() && mapped.writes.is_empty());
    }

    /// A version-1 receipt or update row of TH5a is mapped by the re-map like at its arrival.
    #[test]
    fn a_remap_maps_a_receipt_and_an_update_like_an_arrival() {
        let mut remapped_cx = cx(true);
        remapped_cx.remap = true;
        for envelope in [
            incoming(
                PEER,
                Ty::DeliveryReceipt as i32,
                1,
                receipt_body(3, &[TARGET]),
            ),
            sent_of(vec![(Some(Id::Contact(PEER.to_owned())), 1, true)]),
            read_of(vec![(Some(Id::Contact(PEER.to_owned())), 1, Some(AT))]),
        ] {
            let at_arrival = map(&envelope, &cx(false), &Members).expect("maps");
            let remapped = map(&envelope, &remapped_cx, &Members).expect("maps");
            assert!(!at_arrival.events.is_empty());
            assert_eq!(remapped.events, at_arrival.events);
        }
    }

    fn typing_of(sender: &str, body: Vec<u8>) -> d2d::Envelope {
        incoming(sender, Ty::TypingIndicator as i32, 1, body)
    }

    /// R12: an incoming typing indicator from an ID with ONE byte: 1 is `Typing { on: true }`, 0 `on: false`,
    /// in the sender's direct chat, by the sender, no device, 15 s; byte 2, no byte or two bytes are
    /// `typing-byte`; a sender that is no ID is `typing-sender`; our own other device's typing and any other
    /// kind are `ephemeral-kind`. No file is read: a sender the file lacks is typing too.
    #[test]
    fn typing_is_one_byte_from_an_id_and_incoming_only() {
        for (byte, on) in [(1u8, true), (0, false)] {
            for sender in [PEER, STRANGER] {
                let by = user_of(sender);
                assert_eq!(
                    typing(&typing_of(sender, vec![byte])),
                    Ok(BackendEvent::Typing {
                        conv: ConvKey::Direct(by.clone()),
                        by,
                        device: None,
                        on,
                        ttl: std::time::Duration::from_secs(15),
                    })
                );
            }
        }
        for body in [vec![2], vec![], vec![1, 1], vec![0xff]] {
            assert_eq!(typing(&typing_of(PEER, body)), Err("typing-byte"));
        }
        for sender in ["peeraaaa", "A6MAP0", "", "A6MAP0021"] {
            assert_eq!(typing(&typing_of(sender, vec![1])), Err("typing-sender"));
        }
        let own_typing = outgoing(
            Id::Contact(PEER.to_owned()),
            Ty::TypingIndicator as i32,
            1,
            vec![1],
        );
        assert_eq!(typing(&own_typing), Err("ephemeral-kind"));
        assert_eq!(
            typing(&incoming(PEER, Ty::Text as i32, 1, b"x".to_vec())),
            Err("ephemeral-kind")
        );
        assert_eq!(typing(&d2d::Envelope::default()), Err("ephemeral-kind"));
        // Reflected WITHOUT the flag it is an ordinary envelope: acked, its nonce written, nothing stated.
        let flagless = run(&typing_of(PEER, vec![1]));
        assert!(mapped_nothing(&flagless) && flagless.dropped.is_none());
    }

    /// TH5b R1: the group keys of the clear's cases, written out (creator, a dot, the 16 hex digits of the
    /// little-endian bytes).
    fn clear_group(other: bool) -> ConvKey {
        ConvKey::Group(if other {
            "threema:A6MAP004.4200000000000000".into()
        } else {
            "threema:A6MAP004.8877665544332211".into()
        })
    }

    /// R1, `conversation_of`: the conversation of a stored envelope by `place`'s rules and without the file (a
    /// sender the file lacks still names his chat), for the deferred kinds (votes, calls, the group call) and
    /// the mapped ones alike; everything `place` refuses for its form, and every envelope that is no message, is
    /// `None` (the clear keeps it).
    #[test]
    fn the_conversation_of_a_stored_envelope_follows_place_without_the_file() {
        let direct = |id: &str| {
            Some(ConvKey::Direct(
                UserId::from_parts("threema", id).expect("a user"),
            ))
        };
        let filler = vec![9; 12];
        let in_g = |creator: &str, group: u64| container(creator, group, &filler);
        let ty = |t: Ty| t as i32;
        let no_conversation = d2d::Envelope {
            content: Some(Content::OutgoingMessage(d2d::OutgoingMessage {
                conversation: None,
                r#type: ty(Ty::PollVote),
                ..Default::default()
            })),
            ..Default::default()
        };
        let cases: Vec<(&str, d2d::Envelope, Option<ConvKey>)> = vec![
            (
                "an incoming vote",
                incoming(PEER, ty(Ty::PollVote), 1, filler.clone()),
                direct(PEER),
            ),
            (
                "a stranger's call offer",
                incoming(STRANGER, ty(Ty::CallOffer), 2, filler.clone()),
                direct(STRANGER),
            ),
            (
                "an incoming text",
                incoming(PEER, ty(Ty::Text), 3, b"x".to_vec()),
                direct(PEER),
            ),
            (
                "an incoming group vote",
                incoming(PEER, ty(Ty::GroupPollVote), 4, in_g(CREATOR, GROUP)),
                Some(clear_group(false)),
            ),
            (
                "an incoming group call of the other group",
                incoming(PEER, ty(Ty::GroupCallStart), 5, in_g(CREATOR, OTHER_GROUP)),
                Some(clear_group(true)),
            ),
            (
                "a short container",
                incoming(PEER, ty(Ty::GroupPollVote), 6, CREATOR.as_bytes().to_vec()),
                None,
            ),
            (
                "a sender that is no ID",
                incoming("peeraaaa", ty(Ty::PollVote), 7, filler.clone()),
                None,
            ),
            (
                "an unknown type",
                incoming(PEER, 0x7e, 8, filler.clone()),
                None,
            ),
            (
                "a creator-container control",
                incoming(PEER, ty(Ty::GroupSetup), 9, filler.clone()),
                None,
            ),
            (
                "an outgoing call offer",
                outgoing(
                    Id::Contact(PEER.to_owned()),
                    ty(Ty::CallOffer),
                    10,
                    filler.clone(),
                ),
                direct(PEER),
            ),
            (
                "an outgoing group vote",
                outgoing(
                    to_group(CREATOR, GROUP),
                    ty(Ty::GroupPollVote),
                    11,
                    in_g(CREATOR, GROUP),
                ),
                Some(clear_group(false)),
            ),
            (
                "an outgoing group call",
                outgoing(
                    to_group(CREATOR, OTHER_GROUP),
                    ty(Ty::GroupCallStart),
                    12,
                    in_g(CREATOR, OTHER_GROUP),
                ),
                Some(clear_group(true)),
            ),
            (
                "a container of another group id",
                outgoing(
                    to_group(CREATOR, GROUP),
                    ty(Ty::GroupPollVote),
                    13,
                    in_g(CREATOR, OTHER_GROUP),
                ),
                None,
            ),
            (
                "a container of another creator",
                outgoing(
                    to_group(CREATOR, GROUP),
                    ty(Ty::GroupPollVote),
                    14,
                    in_g(PEER, GROUP),
                ),
                None,
            ),
            (
                "a 1:1 type into a group",
                outgoing(
                    to_group(CREATOR, GROUP),
                    ty(Ty::PollVote),
                    15,
                    filler.clone(),
                ),
                None,
            ),
            (
                "a group type to a contact",
                outgoing(
                    Id::Contact(PEER.to_owned()),
                    ty(Ty::GroupPollVote),
                    16,
                    in_g(CREATOR, GROUP),
                ),
                None,
            ),
            (
                "a distribution list",
                outgoing(
                    Id::DistributionList(7),
                    ty(Ty::PollVote),
                    17,
                    filler.clone(),
                ),
                None,
            ),
            (
                "a contact that is no ID",
                outgoing(
                    Id::Contact("A6MAP0".to_owned()),
                    ty(Ty::CallOffer),
                    18,
                    filler.clone(),
                ),
                None,
            ),
            ("no conversation", no_conversation, None),
            (
                "a sync",
                d2d::Envelope {
                    content: Some(Content::SettingsSync(d2d::SettingsSync::default())),
                    ..Default::default()
                },
                None,
            ),
            (
                "an update",
                d2d::Envelope {
                    content: Some(Content::IncomingMessageUpdate(
                        d2d::IncomingMessageUpdate::default(),
                    )),
                    ..Default::default()
                },
                None,
            ),
            ("no content", d2d::Envelope::default(), None),
        ];
        for (case, envelope, expected) in &cases {
            assert_eq!(conversation_of(envelope), *expected, "{case}");
        }
        // Where `place` runs (a mapped type the file knows), the message lands in the same conversation, and
        // what it drops for the container is `None` here too.
        let text_in = |creator: &str, group: u64| container(creator, group, b"hi");
        for (case, envelope, expected) in [
            (
                "a text",
                incoming(PEER, ty(Ty::Text), 20, b"hi".to_vec()),
                direct(PEER),
            ),
            (
                "a group text",
                incoming(PEER, ty(Ty::GroupText), 21, text_in(CREATOR, GROUP)),
                Some(clear_group(false)),
            ),
            (
                "an own group text",
                outgoing(
                    to_group(CREATOR, GROUP),
                    ty(Ty::GroupText),
                    22,
                    text_in(CREATOR, GROUP),
                ),
                Some(clear_group(false)),
            ),
        ] {
            assert_eq!(conversation_of(&envelope), expected, "{case}");
            assert_eq!(
                Some(the_message(run(&envelope)).conv),
                expected,
                "{case}: place"
            );
        }
        let mismatch = outgoing(
            to_group(CREATOR, GROUP),
            ty(Ty::GroupText),
            23,
            text_in(CREATOR, OTHER_GROUP),
        );
        assert_eq!(conversation_of(&mismatch), None);
        assert_eq!(run(&mismatch).dropped, Some("group-mismatch"));
    }

    /// R1: the forms of the pinned types, written out: the group types `place` reads are member-container forms,
    /// and so are the deferred group kinds; the creator-container controls and the types that are never reflected
    /// name no conversation; every other type is 1:1.
    #[test]
    fn the_form_of_every_type_is_written_out() {
        let of = |wanted: Form| -> Vec<&str> {
            let mut names: Vec<&str> = classify::CSP_TYPES
                .iter()
                .filter(|(_, n, _)| form(i32::from(*n)) == wanted)
                .map(|(name, _, _)| *name)
                .collect();
            names.sort_unstable();
            names
        };
        assert_eq!(
            of(Form::Member),
            [
                "GROUP_AUDIO",
                "GROUP_CALL_START",
                "GROUP_DELETE_MESSAGE",
                "GROUP_DELIVERY_RECEIPT",
                "GROUP_EDIT_MESSAGE",
                "GROUP_FILE",
                "GROUP_IMAGE",
                "GROUP_LEAVE",
                "GROUP_LOCATION",
                "GROUP_POLL_SETUP",
                "GROUP_POLL_VOTE",
                "GROUP_REACTION",
                "GROUP_TEXT",
                "GROUP_VIDEO",
            ]
        );
        assert_eq!(
            of(Form::Unplaced),
            [
                "EMPTY",
                "FORWARD_SECURITY_ENVELOPE",
                "GROUP_DELETE_PROFILE_PICTURE",
                "GROUP_NAME",
                "GROUP_SETUP",
                "GROUP_SET_PROFILE_PICTURE",
                "GROUP_SYNC_REQUEST",
                "WEB_SESSION_RESUME",
                "WORK_SYNC_DELTA",
                "_INVALID_TYPE",
            ]
        );
        assert_eq!(of(Form::Direct).len(), 21, "the 1:1 types");
        for (name, n, _) in classify::CSP_TYPES {
            if is_group_form(i32::from(*n)) {
                assert_eq!(
                    form(i32::from(*n)),
                    Form::Member,
                    "{name}: place reads a container"
                );
            }
        }
        assert_eq!(form(0x7e), Form::Unplaced, "a number the enum lacks");
    }
}
