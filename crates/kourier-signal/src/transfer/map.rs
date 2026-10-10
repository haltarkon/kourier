//! The mapper of a history import: one frame of the archive in, the rows it makes for the
//! stage out (docs/architecture.md §14.2 item 14; `transfer.rs`, "The import").
//!
//! Pure: no file, no database, no clock of its own (the time of the import is given), so
//! the whole mapping is tested on frames alone. Its output is the contract's
//! `IncomingMessage` and the plain staged types of `stage`: no protobuf or libsignal type
//! leaves it.
//!
//! # What this step maps
//!
//! - Recipients: our own account (`self`, and a contact frame that carries our ACI or PNI),
//!   contacts, Signal's release notes; story lists and call links are "not a chat partner".
//!   A group is known as a group and nothing more: its chat and its items are counted as
//!   not supported yet.
//! - Chats of a contact and Note to Self.
//! - Chat items that are plain text, in both directions: author, id, times, read state,
//!   the aggregated send state, the disappearing timer.
//!
//! Everything else is skipped and counted under a reason ([`Mapper::finish`]); one item
//! never fails an import.
//!
//! # Rules a change here must keep
//!
//! - **The id of a message is what the live path gives the same message**: its author's
//!   user id and its ORIGINAL sent time in decimal (`receive::dispatch`). An edited item's
//!   `dateSent` is the time of its LAST edit; the original time is its first revision's.
//!   With that id an envelope the server still holds for a message that is also in the
//!   archive makes no second row, and a later reaction, receipt or read sync finds the row.
//! - **Nothing is skipped by this computer's clock, and nothing is made due by it.** A row
//!   whose disappearing timer has run out by the local clock is staged with its expiry like
//!   any other: whether it is due is decided where the clock is known to be right (the
//!   core's expiry policy, and delivery once the server confirmed the time). The time of
//!   the import is used for one thing only: a time MORE THAN A DAY AHEAD of it
//!   ([`MapLimits::max_ahead_ms`]) is not taken for the sort order, for the server's time or
//!   for a timer's start. A timer's start that is ahead by less stays as the phone wrote it:
//!   this computer's clock can be behind, and a start moved back to it would make the row
//!   due earlier than on the phone. The one start the import's time IS given as: that of the
//!   timer of an own item the phone calls SENT and has no start for (a disappearing message
//!   never stays for ever, and the core would count it from the sent time and erase what the
//!   phone still shows).
//! - **People and chats are staged lazily**: a person when a staged row or a staged chat
//!   first uses them, a chat with its first row. A phone exports a recipient for everybody
//!   it knows and a chat for every thread, also empty ones; listing them all would show
//!   hundreds of empty conversations.
//! - **Quiet and loud.** A chat is shown only when one of its rows is loud: a message, a
//!   placeholder, a call line, or a notice other than the four that Signal Desktop does not
//!   surface a chat for ([`is_loud`]). A chat with rows but no loud one is staged and not
//!   delivered. This step maps no notice, so every row it stages is loud; the rule is here
//!   for the steps that add them.
//! - **A 1:1 chat holds what its peer and we wrote**, nothing else: an incoming item whose
//!   author is not the chat's contact is skipped. The same for the state of an own item:
//!   only the send-status entries of the chat's contact count, and the row names nobody
//!   else (a person a row names must be in the stage).
//! - **Two frames of one person are one person.** A second contact frame with a user id an
//!   earlier frame had leads to the first frame's person (whose fields stand): a chat on
//!   either recipient id is the one conversation with that person, and rows of both go
//!   into it. Nothing an archive holds is dropped for being described twice.
//!
//! # Bounds
//!
//! [`MapLimits`]. What the mapper keeps between frames is the recipients it saw (their
//! kind, and for a contact what its staged row needs until it is staged) and the chats
//! (the conversation and its counters); both are capped, and a frame over a cap is not
//! kept and counted, so what names it later is unknown.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use kourier_backend_api::IncomingMessage;
use kourier_model::{
    Body, ConvKey, DisappearKind, Expiry, FailReason, Origin, OutgoingState, SystemEvent, UserId,
};
use libsignal_core::{Aci, Pni};

use super::stage::{ChatCounters, StagedChat, StagedContact};
use crate::archive::Frame;
use crate::proto::backup;
use crate::service_id::{aci_user_id, e164_user_id, pni_user_id};

/// The bounds of the mapper (architecture §13, the limits table). The archive is
/// authenticated by a key only the user's phone and this device hold, so each of these is
/// Kourier's own bound against a bug, a newer phone or a hostile holder of that key.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MapLimits {
    /// The most recipients kept.
    pub(crate) max_recipients: usize,
    /// The most memory the kept recipients may take, by an estimate.
    pub(crate) max_recipient_bytes: usize,
    /// The most chats kept.
    pub(crate) max_chats: usize,
    /// The longest text body, in bytes (the format's own cap for an inline body); a longer
    /// one is cut on a character.
    pub(crate) max_text_bytes: usize,
    /// The longest name (profile, system, nickname), in bytes; cut on a character.
    pub(crate) max_name_bytes: usize,
    /// How far ahead of the import's time (ms) a time of the archive may be and still be
    /// used: a sent time for the order of the chat (a row's sort time decides where the chat
    /// stands in the list, so a time of next year would pin its chat to the top for good),
    /// the server's time of an incoming row (the core measures the edit and delete windows
    /// from it) and the start of a disappearing timer. Up to this bound a time is taken as
    /// the phone wrote it: this computer's clock may be the one that is off.
    pub(crate) max_ahead_ms: i64,
}

impl Default for MapLimits {
    fn default() -> Self {
        Self {
            max_recipients: 200_000,
            max_recipient_bytes: 128 * 1024 * 1024,
            max_chats: 100_000,
            max_text_bytes: 128 * 1024,
            max_name_bytes: 256,
            max_ahead_ms: 24 * 60 * 60 * 1000,
        }
    }
}

/// The longest disappearing timer: the bound of the live path (`i32::MAX` seconds).
const MAX_TIMER: Duration = Duration::from_secs(i32::MAX as u64);

/// Why something of the archive was not imported, or was imported with less than it had.
/// The names are what the import's summary and the log carry: static, never content.
pub(crate) mod reason {
    /// A recipient or chat frame beyond [`MapLimits`](super::MapLimits): not kept.
    pub(crate) const RECIPIENT_OVER_CAP: &str = "recipient_over_cap";
    /// See [`RECIPIENT_OVER_CAP`].
    pub(crate) const CHAT_OVER_CAP: &str = "chat_over_cap";
    /// A recipient or chat whose id is 0 or was seen before: the first stands.
    pub(crate) const DUPLICATE_ID: &str = "duplicate_id";
    /// A recipient of no kind this version knows (a newer phone's), or a contact without
    /// an ACI, a PNI or a valid number.
    pub(crate) const UNKNOWN_RECIPIENT: &str = "unknown_recipient";
    /// A second contact frame for a person an earlier frame described: its recipient id
    /// leads to the first frame's person, its own fields are not used.
    pub(crate) const DUPLICATE_CONTACT: &str = "duplicate_contact";
    /// A chat whose recipient is unknown or cannot have a chat (a story list, a call link),
    /// and every item of a chat that is not known.
    pub(crate) const UNKNOWN_CHAT: &str = "unknown_chat";
    /// The chat of Signal's release notes and every item of it.
    pub(crate) const RELEASE_NOTES: &str = "release_notes";
    /// A group's chat: not supported by this step.
    pub(crate) const GROUP_CHAT_NOT_YET: &str = "group_chat_not_yet";
    /// An item of a group's chat: not supported by this step.
    pub(crate) const GROUP_ITEM_NOT_YET: &str = "group_item_not_yet";
    /// An item without a sent time.
    pub(crate) const NO_TIME: &str = "no_time";
    /// An item whose direction or content is unset (a newer phone's kind), or whose
    /// direction does not fit its content.
    pub(crate) const UNKNOWN_ITEM: &str = "unknown_item";
    /// An incoming item whose author is unknown, is us, or is not a person.
    pub(crate) const UNKNOWN_AUTHOR: &str = "unknown_author";
    /// An incoming item of a 1:1 chat whose author is not the chat's contact.
    pub(crate) const AUTHOR_NOT_IN_CHAT: &str = "author_not_in_chat";
    /// A message that was deleted for everyone (decided: not imported).
    pub(crate) const DELETED: &str = "deleted";
    /// A standard message with no text (and nothing this step imports).
    pub(crate) const EMPTY: &str = "empty";
    /// A standard message with attachments: not supported by this step.
    pub(crate) const ATTACHMENTS_NOT_YET: &str = "attachments_not_yet";
    /// A standard message whose text continues in a file: not supported by this step.
    pub(crate) const LONG_TEXT_NOT_YET: &str = "long_text_not_yet";
    /// An item of a kind this step does not import (one reason per kind).
    pub(crate) const UPDATE_NOT_YET: &str = "update_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const STICKER_NOT_YET: &str = "sticker_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const CONTACT_CARD_NOT_YET: &str = "contact_card_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const PAYMENT_NOT_YET: &str = "payment_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const GIFT_BADGE_NOT_YET: &str = "gift_badge_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const VIEW_ONCE_NOT_YET: &str = "view_once_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const STORY_REPLY_NOT_YET: &str = "story_reply_not_yet";
    /// See [`UPDATE_NOT_YET`].
    pub(crate) const POLL_NOT_YET: &str = "poll_not_yet";
    /// Imported, without its quote: not supported by this step.
    pub(crate) const QUOTE_NOT_YET: &str = "quote_not_yet";
    /// Imported, without its reactions: not supported by this step.
    pub(crate) const REACTIONS_NOT_YET: &str = "reactions_not_yet";
    /// Imported, without its formatting and mentions: not supported by this step.
    pub(crate) const RANGES_NOT_YET: &str = "ranges_not_yet";
    /// Imported under its original id, without the "edited" mark: not supported by this
    /// step.
    pub(crate) const EDITED_MARK_NOT_YET: &str = "edited_mark_not_yet";
    /// Imported, its text cut at the bound.
    pub(crate) const TEXT_CUT: &str = "text_cut";
    /// Imported, with a sort time that is not its sent time (which is too far ahead).
    pub(crate) const FUTURE_TIME: &str = "future_time";
    /// Imported without the server's time of it (which is too far ahead): the core then
    /// measures from the sort time.
    pub(crate) const FUTURE_SERVER_TIME: &str = "future_server_time";
    /// Imported with its timer started at the import: the start it states is too far ahead.
    pub(crate) const FUTURE_TIMER_START: &str = "future_timer_start";
    /// An own message whose timer never started. One that was not sent (pending or failed
    /// on the phone) is imported with its timer and without a start: the core starts it
    /// when the message's echo says it was sent. One the phone calls sent is imported with
    /// its timer started at the import's time.
    pub(crate) const TIMER_NOT_STARTED: &str = "timer_not_started";
}

/// What one frame makes for the stage, in the order it must be written.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Staged {
    /// A person, as row `n` of the stage's contacts.
    Contact {
        /// The row.
        n: u32,
        /// The person.
        contact: Box<StagedContact>,
    },
    /// A conversation, as row `idx` of the stage's chats.
    Chat {
        /// The row.
        idx: u32,
        /// The conversation.
        conv: ConvKey,
        /// What the chat frame carried.
        chat: StagedChat,
    },
    /// A message of chat `chat`, from frame `n`.
    Item {
        /// The chat's `idx`.
        chat: u32,
        /// The frame's running number.
        n: u64,
        /// The message.
        msg: Box<IncomingMessage>,
    },
}

/// What the mapper knows when the last frame was mapped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MapEnd {
    /// The counters of every staged chat, by `idx`.
    pub(crate) chats: Vec<ChatCounters>,
    /// What was skipped or imported with less, by reason ([`reason`]).
    pub(crate) counts: BTreeMap<&'static str, u64>,
}

/// Whether a row with this body makes its chat show in the list. Signal Desktop does not
/// surface a chat for the notices named here (`ts/services/backups/import.preload.ts`,
/// the items that do not "affect presence", cut down to the notices the contract has).
pub(crate) fn is_loud(body: &Body) -> bool {
    !matches!(
        body,
        Body::System(
            SystemEvent::IdentityKeyChanged { .. }
                | SystemEvent::NumberChanged { .. }
                | SystemEvent::MessageRequestAccepted
                | SystemEvent::MemberLeft { .. }
        )
    )
}

/// A recipient of the archive, by what it can be to a chat or an item.
enum Recipient {
    /// Our own account.
    Own,
    /// A person: an index into the mapper's people.
    Contact(usize),
    /// A group (not supported by this step).
    Group,
    /// Signal's release notes.
    ReleaseNotes,
    /// A story list or a call link.
    NotChatPartner,
    /// A frame that was read and not understood: what names it is unknown.
    Skipped,
}

/// A person of the archive.
struct Person {
    user: UserId,
    /// The row in the stage's contacts, once staged.
    staged: Option<u32>,
    /// What the staged row needs, until it is staged.
    pending: Option<Box<StagedContact>>,
}

/// A chat of the archive, by what its items become.
#[derive(Clone, Copy)]
enum Chat {
    /// A conversation: an index into the mapper's conversations.
    Conv(usize),
    /// A group's chat (not supported by this step).
    Group,
    /// The release notes' chat.
    ReleaseNotes,
}

/// A conversation: one or more chat frames of the same recipient.
struct Conv {
    key: ConvKey,
    /// The person it is with; `None` for Note to Self.
    peer: Option<usize>,
    /// The row in the stage's chats, once it has a row of its own.
    idx: Option<u32>,
    counters: ChatCounters,
}

/// Maps the frames of one archive, in the archive's order.
pub(crate) struct Mapper {
    own: UserId,
    own_aci: Aci,
    own_pni: Pni,
    now: i64,
    limits: MapLimits,
    /// The frames seen so far: a frame's running number, from 1.
    frame: u64,
    recipients: HashMap<u64, Recipient>,
    recipient_bytes: usize,
    people: Vec<Person>,
    by_user: HashMap<UserId, usize>,
    chats: HashMap<u64, Chat>,
    convs: Vec<Conv>,
    by_key: HashMap<ConvKey, usize>,
    staged_contacts: u32,
    staged_chats: u32,
    counts: BTreeMap<&'static str, u64>,
}

impl Mapper {
    /// A mapper for the account `own_aci` / `own_pni`, importing at `now` (ms, this
    /// device's clock; see the module docs for the one thing it is used for). `None` when
    /// the ACI makes no user id, which cannot happen for a linked account.
    pub(crate) fn new(own_aci: Aci, own_pni: Pni, now: i64, limits: MapLimits) -> Option<Self> {
        Some(Self {
            own: aci_user_id(own_aci).ok()?,
            own_aci,
            own_pni,
            now,
            limits,
            frame: 0,
            recipients: HashMap::new(),
            recipient_bytes: 0,
            people: Vec::new(),
            by_user: HashMap::new(),
            chats: HashMap::new(),
            convs: Vec::new(),
            by_key: HashMap::new(),
            staged_contacts: 0,
            staged_chats: 0,
            counts: BTreeMap::new(),
        })
    }

    fn count(&mut self, reason: &'static str) {
        *self.counts.entry(reason).or_insert(0) += 1;
    }

    /// Maps the next frame; what it makes for the stage is appended to `out`.
    pub(crate) fn frame(&mut self, frame: Frame, out: &mut Vec<Staged>) {
        self.frame += 1;
        match frame {
            Frame::Recipient(recipient) => self.recipient(recipient),
            Frame::Chat(chat) => self.chat(&chat),
            Frame::ChatItem(item) => self.item(*item, out),
            // Nothing is taken from the account frame (the Storage Service sync that
            // follows the transfer sets what Kourier knows of it), and the other kinds are
            // not in the model. The import's summary counts them by kind.
            Frame::Account(_)
            | Frame::InstalledStickerPack(_)
            | Frame::HiddenStickerPack(_)
            | Frame::AdHocCall(_)
            | Frame::NotificationProfile(_)
            | Frame::ChatFolder(_)
            | Frame::FavoriteStickerList(_)
            | Frame::Unknown => {}
        }
    }

    /// The counters of the staged chats and the counts by reason.
    pub(crate) fn finish(self) -> MapEnd {
        let mut chats: Vec<ChatCounters> = self
            .convs
            .iter()
            .filter(|conv| conv.idx.is_some())
            .map(|conv| conv.counters)
            .collect();
        chats.sort_by_key(|chat| chat.idx);
        MapEnd {
            chats,
            counts: self.counts,
        }
    }

    fn recipient(&mut self, recipient: backup::Recipient) {
        use backup::recipient::Destination;
        if recipient.id == 0 || self.recipients.contains_key(&recipient.id) {
            return self.count(reason::DUPLICATE_ID);
        }
        if self.recipients.len() >= self.limits.max_recipients {
            return self.count(reason::RECIPIENT_OVER_CAP);
        }
        let mut cost = std::mem::size_of::<(u64, Recipient)>() * 2;
        let people = self.people.len();
        let kind = match recipient.destination {
            Some(Destination::Self_(_)) => Recipient::Own,
            Some(Destination::Contact(contact)) => match self.contact(contact) {
                Some((kind, bytes)) => {
                    cost += bytes;
                    kind
                }
                None => Recipient::Skipped,
            },
            Some(Destination::Group(_)) => Recipient::Group,
            Some(Destination::ReleaseNotes(_)) => Recipient::ReleaseNotes,
            Some(Destination::DistributionList(_) | Destination::CallLink(_)) => {
                Recipient::NotChatPartner
            }
            None => {
                self.count(reason::UNKNOWN_RECIPIENT);
                Recipient::Skipped
            }
        };
        if self.recipient_bytes.saturating_add(cost) > self.limits.max_recipient_bytes {
            // A person this frame made is the last one: taken back with the frame. A second
            // frame of a person made none, and the person it leads to stays.
            if self.people.len() > people
                && let Some(person) = self.people.pop()
            {
                self.by_user.remove(&person.user);
            }
            return self.count(reason::RECIPIENT_OVER_CAP);
        }
        self.recipient_bytes += cost;
        self.recipients.insert(recipient.id, kind);
    }

    /// A contact frame: who it is, and what its staged row will need. Returns the kind and
    /// an estimate of the memory kept for it; `None` (counted) when it names nobody.
    fn contact(&mut self, contact: backup::Contact) -> Option<(Recipient, usize)> {
        let aci = contact
            .aci
            .as_deref()
            .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
            .map(Aci::from_uuid_bytes);
        let pni = contact
            .pni
            .as_deref()
            .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
            .map(Pni::from_uuid_bytes);
        // The phone writes `self` in place of its own contact row; a contact frame that
        // carries one of our ids all the same is us, never a chat partner.
        if aci == Some(self.own_aci) || pni == Some(self.own_pni) {
            return Some((Recipient::Own, 0));
        }
        // The number is a plain integer in the archive, without its "+".
        let e164 = contact
            .e164
            .filter(|number| *number != 0)
            .map(|number| format!("+{number}"))
            .filter(|number| crate::service_id::validate_e164(number).is_ok());
        let user = aci
            .and_then(|aci| aci_user_id(aci).ok())
            .or_else(|| pni.and_then(|pni| pni_user_id(pni).ok()))
            .or_else(|| e164.as_deref().and_then(|number| e164_user_id(number).ok()));
        let Some(user) = user else {
            self.count(reason::UNKNOWN_RECIPIENT);
            return None;
        };
        if let Some(first) = self.by_user.get(&user) {
            // The same person again. The first frame's fields stand and nothing more is
            // kept, but this recipient id leads to that person too: a chat on it is the
            // conversation with them, an item by it is theirs. Dropping the frame would
            // drop every message of a chat that names this id.
            let first = *first;
            self.count(reason::DUPLICATE_CONTACT);
            return Some((Recipient::Contact(first), 0));
        }
        let max = self.limits.max_name_bytes;
        let name = |given: &str, family: &str| -> Option<String> {
            let joined = match (given.trim(), family.trim()) {
                ("", "") => return None,
                (given, "") => given.to_owned(),
                ("", family) => family.to_owned(),
                (given, family) => format!("{given} {family}"),
            };
            Some(cut(joined, max).0)
        };
        let system_name = name(&contact.system_given_name, &contact.system_family_name)
            .or_else(|| name(&contact.system_nickname, ""));
        let staged = StagedContact {
            user: user.clone(),
            aci: aci.map(uuid::Uuid::from),
            pni: pni.map(uuid::Uuid::from),
            e164,
            profile_key: contact
                .profile_key
                .as_deref()
                .and_then(|key| <[u8; 32]>::try_from(key).ok()),
            profile_sharing: contact.profile_sharing,
            blocked: contact.blocked,
            hidden: contact.visibility == backup::contact::Visibility::Hidden as i32
                || contact.visibility == backup::contact::Visibility::HiddenMessageRequest as i32,
            unregistered_at: match contact.registration {
                Some(backup::contact::Registration::NotRegistered(since)) => {
                    Some(to_i64(since.unregistered_timestamp))
                }
                _ => None,
            },
            profile_name: name(
                contact.profile_given_name.as_deref().unwrap_or(""),
                contact.profile_family_name.as_deref().unwrap_or(""),
            ),
            system_name,
            nickname: contact
                .nickname
                .as_ref()
                .and_then(|nickname| name(&nickname.given, &nickname.family)),
        };
        let bytes = std::mem::size_of::<Person>()
            + std::mem::size_of::<StagedContact>()
            + 2 * user.as_str().len()
            + [&staged.profile_name, &staged.system_name, &staged.nickname]
                .into_iter()
                .chain([&staged.e164])
                .map(|text| text.as_ref().map_or(0, String::len))
                .sum::<usize>();
        let person = self.people.len();
        self.by_user.insert(user.clone(), person);
        self.people.push(Person {
            user,
            staged: None,
            pending: Some(Box::new(staged)),
        });
        Some((Recipient::Contact(person), bytes))
    }

    fn chat(&mut self, chat: &backup::Chat) {
        if chat.id == 0 || self.chats.contains_key(&chat.id) {
            return self.count(reason::DUPLICATE_ID);
        }
        if self.chats.len() >= self.limits.max_chats {
            return self.count(reason::CHAT_OVER_CAP);
        }
        let (key, peer) = match self.recipients.get(&chat.recipient_id) {
            Some(Recipient::Own) => (ConvKey::Direct(self.own.clone()), None),
            Some(Recipient::Contact(person)) => (
                ConvKey::Direct(self.people[*person].user.clone()),
                Some(*person),
            ),
            Some(Recipient::Group) => {
                self.count(reason::GROUP_CHAT_NOT_YET);
                self.chats.insert(chat.id, Chat::Group);
                return;
            }
            Some(Recipient::ReleaseNotes) => {
                self.count(reason::RELEASE_NOTES);
                self.chats.insert(chat.id, Chat::ReleaseNotes);
                return;
            }
            Some(Recipient::NotChatPartner | Recipient::Skipped) | None => {
                return self.count(reason::UNKNOWN_CHAT);
            }
        };
        // Two chat frames of one recipient are one conversation: the first stands, the
        // rows of both go into it.
        let conv = match self.by_key.get(&key) {
            Some(conv) => *conv,
            None => {
                let conv = self.convs.len();
                self.by_key.insert(key.clone(), conv);
                self.convs.push(Conv {
                    key,
                    peer,
                    idx: None,
                    counters: ChatCounters::default(),
                });
                conv
            }
        };
        self.chats.insert(chat.id, Chat::Conv(conv));
    }

    fn item(&mut self, item: backup::ChatItem, out: &mut Vec<Staged>) {
        use backup::chat_item::{DirectionalDetails, Item};
        let conv = match self.chats.get(&item.chat_id) {
            Some(Chat::Conv(conv)) => *conv,
            Some(Chat::Group) => return self.count(reason::GROUP_ITEM_NOT_YET),
            Some(Chat::ReleaseNotes) => return self.count(reason::RELEASE_NOTES),
            None => return self.count(reason::UNKNOWN_CHAT),
        };
        if item.date_sent == 0 {
            return self.count(reason::NO_TIME);
        }
        let (Some(direction), Some(content)) = (item.directional_details, item.item) else {
            return self.count(reason::UNKNOWN_ITEM);
        };
        let message = match content {
            Item::StandardMessage(message) => message,
            Item::RemoteDeletedMessage(_) | Item::AdminDeletedMessage(_) => {
                return self.count(reason::DELETED);
            }
            Item::UpdateMessage(_) => return self.count(reason::UPDATE_NOT_YET),
            Item::StickerMessage(_) => return self.count(reason::STICKER_NOT_YET),
            Item::ContactMessage(_) => return self.count(reason::CONTACT_CARD_NOT_YET),
            Item::PaymentNotification(_) => return self.count(reason::PAYMENT_NOT_YET),
            Item::GiftBadge(_) => return self.count(reason::GIFT_BADGE_NOT_YET),
            Item::ViewOnceMessage(_) => return self.count(reason::VIEW_ONCE_NOT_YET),
            Item::DirectStoryReplyMessage(_) => return self.count(reason::STORY_REPLY_NOT_YET),
            Item::Poll(_) => return self.count(reason::POLL_NOT_YET),
        };
        if !message.attachments.is_empty() {
            return self.count(reason::ATTACHMENTS_NOT_YET);
        }
        if message.long_text.is_some() {
            return self.count(reason::LONG_TEXT_NOT_YET);
        }
        let Some(text) = message.text.filter(|text| !text.body.is_empty()) else {
            return self.count(reason::EMPTY);
        };

        // Who wrote it, and what the phone says of its state.
        let key = self.convs[conv].key.clone();
        let mut msg = match direction {
            DirectionalDetails::Incoming(incoming) => {
                let author = match self.recipients.get(&item.author_id) {
                    Some(Recipient::Contact(person)) => *person,
                    _ => return self.count(reason::UNKNOWN_AUTHOR),
                };
                if self.convs[conv].peer != Some(author) {
                    return self.count(reason::AUTHOR_NOT_IN_CHAT);
                }
                let mut msg = self.message(
                    &key,
                    &item.revisions,
                    item.date_sent,
                    Origin::Remote {
                        author: self.people[author].user.clone(),
                        device: None,
                    },
                    incoming.date_received,
                );
                // The core measures the edit and delete windows of a row from the server's
                // time of it: one that is far ahead would keep both open for good. Without
                // it the core measures from the sort time, which is bounded the same way.
                msg.server_at = match incoming.date_server_sent.filter(|at| *at != 0) {
                    Some(at) if to_i64(at) > self.latest() => {
                        self.count(reason::FUTURE_SERVER_TIME);
                        None
                    }
                    at => at.map(to_i64),
                };
                msg.read = incoming.read;
                msg.expiry = match timer(item.expires_in_ms) {
                    None => None,
                    Some(timer) => Some(Expiry {
                        kind: DisappearKind::AfterRead,
                        timer,
                        started_at: match self.timer_start(item.expire_start_date) {
                            Some(start) => Some(start),
                            // Read on the phone, and the phone did not say when its timer
                            // started: at the latest when the phone received it. Never
                            // `None`, which would start it at the time of the import.
                            None if incoming.read => Some(
                                self.timer_start(Some(incoming.date_received))
                                    .unwrap_or(msg.received_at),
                            ),
                            // Unread: it starts when it is read here.
                            None => None,
                        },
                    }),
                };
                msg
            }
            DirectionalDetails::Outgoing(outgoing) => {
                let mut msg = self.message(
                    &key,
                    &item.revisions,
                    item.date_sent,
                    Origin::OwnOtherDevice { device: None },
                    outgoing.date_received,
                );
                let state = self.send_state(self.convs[conv].peer, &outgoing.send_status);
                let unsent = matches!(
                    state,
                    OutgoingState::Queued
                        | OutgoingState::Sending { .. }
                        | OutgoingState::Failed { .. }
                );
                msg.initial_out_state = Some(state);
                msg.expiry = match (
                    timer(item.expires_in_ms),
                    self.timer_start(item.expire_start_date),
                ) {
                    (Some(timer), Some(start)) => Some(Expiry {
                        kind: DisappearKind::AfterSend,
                        timer,
                        started_at: Some(start),
                    }),
                    (Some(timer), None) => {
                        self.count(reason::TIMER_NOT_STARTED);
                        // The timer is always kept (a disappearing message must never stay
                        // for ever), and the phone has no start for it. Not sent yet, or
                        // failed: WITHOUT a start. The core leaves the timer of an own
                        // message that was not sent alone and starts it when the phone's
                        // transcript of the message (its echo) says it was sent. Sent, as
                        // the phone says: started at the import's time. Given without a
                        // start the core would count from the sent time and erase what the
                        // phone still shows, and a start that is given stands (the core
                        // ignores the `expiry` of a copy, so a second read of the archive
                        // moves nothing).
                        Some(Expiry {
                            kind: DisappearKind::AfterSend,
                            timer,
                            started_at: if unsent { None } else { Some(self.now) },
                        })
                    }
                    (None, _) => None,
                };
                msg
            }
            // A notice has no direction; a message without one is not a message.
            DirectionalDetails::Directionless(_) => return self.count(reason::UNKNOWN_ITEM),
        };

        // The content this step imports: the text. What else the message has is counted.
        if message.quote.is_some() {
            self.count(reason::QUOTE_NOT_YET);
        }
        if !message.reactions.is_empty() {
            self.count(reason::REACTIONS_NOT_YET);
        }
        if !text.body_ranges.is_empty() {
            self.count(reason::RANGES_NOT_YET);
        }
        if !item.revisions.is_empty() {
            self.count(reason::EDITED_MARK_NOT_YET);
        }
        let (body, was_cut) = cut(text.body, self.limits.max_text_bytes);
        if was_cut {
            self.count(reason::TEXT_CUT);
        }
        msg.body = Body::text(body);
        let n = self.frame;
        self.row(conv, n, msg, out);
    }

    /// The message of an item, without its content: its id, its times and the flags every
    /// imported row has. `received` is the phone's received time (0: unknown).
    fn message(
        &mut self,
        conv: &ConvKey,
        revisions: &[backup::ChatItem],
        date_sent: u64,
        origin: Origin,
        received: u64,
    ) -> IncomingMessage {
        // An edited item states the time of its last edit; the message's id is the time of
        // its first version (module docs).
        let original = revisions
            .first()
            .map(|first| first.date_sent)
            .filter(|at| *at != 0)
            .unwrap_or(date_sent);
        let sent = to_i64(original);
        let latest = self.latest();
        let received = match to_i64(received) {
            0 => None,
            at => Some(at),
        };
        let sent_at = if sent > latest {
            // Too far ahead to stand in the order: the time it was received when that is a
            // time of the past, else the time of the import. The id keeps the true value.
            self.count(reason::FUTURE_TIME);
            received.filter(|at| *at <= self.now).unwrap_or(self.now)
        } else {
            sent
        };
        // Never after the import: nothing was received in the future.
        let received_at = received.unwrap_or(sent_at).min(self.now);
        let mut msg = IncomingMessage::new(
            conv.clone(),
            original.to_string(),
            origin,
            Body::text(""),
            sent_at,
            received_at,
        );
        // History never notifies.
        msg.silent = true;
        msg
    }

    /// The latest time of the archive that is taken as it stands
    /// ([`MapLimits::max_ahead_ms`] after the import).
    fn latest(&self) -> i64 {
        self.now.saturating_add(self.limits.max_ahead_ms)
    }

    /// When an item's timer started, when the phone says so. The phone's time stands, also
    /// when it is later than the import: this computer's clock can be behind, and a start
    /// moved back to it would make the row due earlier than on the phone (module docs).
    /// Only a start more than a day ahead is no time a timer started at: that one is the
    /// time of the import, and counted.
    fn timer_start(&mut self, expire_start_date: Option<u64>) -> Option<i64> {
        let at = expire_start_date.filter(|at| *at != 0).map(to_i64)?;
        if at > self.latest() {
            self.count(reason::FUTURE_TIMER_START);
            return Some(self.now);
        }
        Some(at)
    }

    /// The state of an own message of a 1:1 chat, by the core's own rule for the aggregate
    /// over recipients (the minimum; a failure beside a success is a partial failure).
    ///
    /// Only the entries of the chat's own contact `peer` count. A 1:1 chat has one
    /// recipient, so an entry that names anybody else (us, a person of another chat, an id
    /// no frame described) says nothing about this message, and a failed one must not put
    /// a person into the row whom the stage does not hold for it. Entries that say
    /// "skipped" do not count either. With none left (an empty list, Note to Self) the
    /// message was sent.
    ///
    /// Who it failed for is a set: the row names a person once however often the list does,
    /// so the row does not grow with the list. A partial failure always names somebody.
    fn send_state(&self, peer: Option<usize>, statuses: &[backup::SendStatus]) -> OutgoingState {
        use backup::send_status::DeliveryStatus;
        use backup::send_status::failed::FailureReason;
        let mut lowest: Option<u8> = None;
        let mut failed: BTreeSet<usize> = BTreeSet::new();
        let mut reason = None;
        for status in statuses {
            let person = match self.recipients.get(&status.recipient_id) {
                Some(Recipient::Contact(person)) if Some(*person) == peer => *person,
                _ => continue,
            };
            let rank = match &status.delivery_status {
                Some(DeliveryStatus::Skipped(_)) => continue,
                // "If unset, importers should consider the status to be pending."
                Some(DeliveryStatus::Pending(_)) | None => 0,
                Some(DeliveryStatus::Sent(_)) => 1,
                Some(DeliveryStatus::Delivered(_)) => 2,
                Some(DeliveryStatus::Read(_)) => 3,
                Some(DeliveryStatus::Viewed(_)) => 4,
                Some(DeliveryStatus::Failed(failure)) => {
                    failed.insert(person);
                    reason.get_or_insert(failure.reason);
                    continue;
                }
            };
            lowest = Some(lowest.map_or(rank, |lowest| lowest.min(rank)));
        }
        if failed.is_empty() {
            return match lowest {
                None | Some(1) => OutgoingState::Sent { server_at: None },
                Some(0) => OutgoingState::Sending { progress: None },
                Some(2) => OutgoingState::Delivered,
                Some(3) => OutgoingState::Read,
                Some(_) => OutgoingState::Viewed,
            };
        }
        match lowest {
            Some(_) => OutgoingState::PartiallyFailed {
                failed: failed
                    .into_iter()
                    .map(|person| self.people[person].user.clone())
                    .collect(),
            },
            None => OutgoingState::Failed {
                reason: match reason {
                    Some(r) if r == FailureReason::Network as i32 => FailReason::Network,
                    Some(r) if r == FailureReason::IdentityKeyMismatch as i32 => {
                        FailReason::UntrustedIdentity
                    }
                    _ => FailReason::Other("failed on the phone".to_owned()),
                },
                retryable: false,
            },
        }
    }

    /// Stages one row of conversation `conv`: first, when they are not staged yet, the
    /// chat's peer (in a 1:1 chat the only person a row of this step can name) and the chat,
    /// then the row; and counts it for its chat.
    fn row(&mut self, conv: usize, n: u64, msg: IncomingMessage, out: &mut Vec<Staged>) {
        let peer = self.convs[conv].peer;
        let peer_row = peer.map(|person| self.person_row(person, out));
        let idx = match self.convs[conv].idx {
            Some(idx) => idx,
            None => {
                let idx = self.staged_chats;
                self.staged_chats += 1;
                let chat = &mut self.convs[conv];
                chat.idx = Some(idx);
                chat.counters.idx = idx;
                out.push(Staged::Chat {
                    idx,
                    conv: chat.key.clone(),
                    chat: StagedChat {
                        is_self: peer.is_none(),
                        peer: peer_row,
                    },
                });
                idx
            }
        };
        let counters = &mut self.convs[conv].counters;
        counters.n_rows += 1;
        counters.n_loud += u64::from(is_loud(&msg.body));
        counters.last_at = counters.last_at.max(msg.sent_at);
        out.push(Staged::Item {
            chat: idx,
            n,
            msg: Box::new(msg),
        });
    }

    /// The row of `person` in the stage's contacts, staging the person when this is their
    /// first use.
    fn person_row(&mut self, person: usize, out: &mut Vec<Staged>) -> u32 {
        if let Some(row) = self.people[person].staged {
            return row;
        }
        let n = self.staged_contacts;
        self.staged_contacts += 1;
        let entry = &mut self.people[person];
        entry.staged = Some(n);
        // The heavy fields leave the map with the staged row.
        if let Some(contact) = entry.pending.take() {
            out.push(Staged::Contact { n, contact });
        }
        n
    }
}

/// The timer of an item: `None` when it has none; at most [`MAX_TIMER`].
fn timer(expires_in_ms: Option<u64>) -> Option<Duration> {
    expires_in_ms
        .filter(|ms| *ms != 0)
        .map(|ms| Duration::from_millis(ms).min(MAX_TIMER))
}

/// `text` cut to at most `max` bytes on a character boundary, and whether it was cut.
fn cut(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
