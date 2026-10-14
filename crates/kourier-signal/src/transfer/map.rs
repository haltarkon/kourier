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
//!   contacts, groups (step S4b: the master key, and from the phone's snapshot the title,
//!   the members, invited and requesting people with their roles, the timer in SECONDS and
//!   our membership: member, invited by our ACI or PNI, requesting, left, or unknown for an
//!   empty snapshot; `inviteLinkPassword` is never read), Signal's release notes; story
//!   lists and call links are "not a chat partner".
//! - Chats of a contact and Note to Self, with what their chat frame carries
//!   (`stage::ChatFrame`): archived, marked unread, muted until (through the Storage
//!   Service's own conversion, [`crate::storage::muted_until`]), the 1:1 timer in seconds
//!   (at most `i32::MAX`) and its version (at least 1, also when the timer is off), and the
//!   pinned order. The FIRST chat frame of a conversation gives them.
//! - Messages in both directions: author, id, times, read state, the aggregated send state,
//!   the disappearing timer; the text with its formatting and mentions (UTF-16 ranges
//!   through the live mapper's [`dispatch::ranged_text`]; a range that crosses the cut of a
//!   long text is shortened to the cut), `mentions_me`; the quote (its target, author and
//!   the quote's own copy, `local` "0" when the phone did not find the target); reactions
//!   (one per author, in their order, the live emoji rule); the ids of an edited message's
//!   later versions (`IncomingMessage.edits`, sorted by time, one per time, the newest
//!   last; earlier texts are not imported). A sticker is a `Body::Sticker`, its content type
//!   kept as the live path keeps it ([`media::sticker_mime`]); view-once, contact cards,
//!   payments, gift badges, polls and story replies are the live path's `Body::Unsupported`
//!   placeholders, all but view-once with their raw item ([`RAW_CHAT_ITEM`]).
//! - Attachments (step S5): a message's attachments (at most
//!   [`MapLimits::max_attachments`]), the rest of its long text and a sticker's image, each
//!   with its fields bounded as the live path bounds them and its kind from the archive's
//!   flag ENUM ([`media::archive_kind`]). One whose place on the server is valid and young
//!   ([`MediaRules`]) is `Remote` with the trimmed pointer as `remote` (v 3); any other is
//!   "not available on this device" (`AttachmentState::Unavailable`) with no pointer and no
//!   key, but with its names (`net_ids`) and the message's id, which a later step needs to
//!   ask the phone for it. Quote thumbnails, a view-once message's pointer, contact
//!   avatars and link previews keep no pointer.
//! - The notices of a 1:1 chat that the contract has (timer change, safety number, number
//!   change, session reset, accepted request) under ids of their own (`h:<frame>:0`), the
//!   placeholders of update items that are messages on the wire (newer protocol, payments
//!   activated, poll ended, pin) under the message's real id, and call lines in every state
//!   (a `callId` of 0 is no id; [`Mapper::group_call`] is the group call's line, used once
//!   groups are, S4).
//! - Group chats (step S4b): their items as in a 1:1 chat, by any person of the archive
//!   (authors, quotes, reactions, send states, notices about a member); the group changes the
//!   contract has a notice for, one notice per entry (`h:<frame>:<k>`, `sent_at + k`) with
//!   the entries that add or remove members folded as the live path folds them, at most
//!   [`MapLimits::max_group_notices`] per item; an entry whose actor is absent is skipped
//!   (`by: None` means "you"); group call lines ([`Mapper::group_call`]). A group's timer is
//!   the chat frame's (ms) when it has one, else the snapshot's (s).
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
//!   phone still shows), and that of an outgoing call line without a start, for the same
//!   reason. A timer-change notice gets no timer at all (Android never starts it).
//! - **People and chats are staged lazily**: a person when a staged row or a staged chat
//!   first uses them, a chat with its first row. A phone exports a recipient for everybody
//!   it knows and a chat for every thread, also empty ones; listing them all would show
//!   hundreds of empty conversations. The one exception: a chat the phone PINNED is staged
//!   at its chat frame and shown whatever it holds, as Android and Desktop list it.
//! - **Quiet and loud.** A chat is shown only when one of its rows is loud: a message, a
//!   placeholder, a call line, or a notice other than the four that Signal Desktop does not
//!   surface a chat for ([`is_loud`]). A chat with rows but no loud one is staged and not
//!   delivered, unless it is pinned.
//! - **A 1:1 chat holds what its peer and we wrote**, nothing else: an incoming item whose
//!   author is not the chat's contact is skipped. The same for the state of an own item:
//!   only the send-status entries of the chat's contact count. A quote, a reaction and a
//!   notice name only us and the contact (a quote of anybody else has an unknown author, a
//!   reaction by anybody else is dropped, a notice skipped); a mention keeps the ACI it
//!   names, as on the live path, with no name. In a GROUP any person of the archive may
//!   appear (Android does not check membership: past members wrote past items), and every
//!   person a staged row names is staged with it, as are the people of a staged group's
//!   member lists (so they have names).
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

use kourier_backend_api::{HistoryEdit, HistoryReaction, IncomingMessage, MAX_HISTORY_EDITS};
use kourier_model::{
    Attachment, AttachmentKind, AttachmentState, BackendBlob, Body, CallDirection, CallLine,
    CallMedia, CallState, ConvKey, DisappearKind, Expiry, FailReason, Member, MemberRole,
    MsgTarget, Origin, OutgoingState, QuoteSnapshot, RangeKind, ReplyTo, SystemEvent, TargetAuthor,
    Text, UserId,
};
use libsignal_core::{Aci, Pni};
use prost::Message as _;

use crate::attachment::{Pointer, archive_pointer_blob};

use super::stage::{
    ChatCounters, ChatFrame, GroupMembership, MasterKey, StagedChat, StagedContact, StagedGroup,
    StagedKind,
};
use crate::archive::Frame;
use crate::proto::backup;
use crate::receive::{dispatch, media};
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
    /// The longest text of a quote, in bytes; cut on a character.
    pub(crate) max_quote_bytes: usize,
    /// The most formatting and mention ranges of one text; the rest are dropped.
    pub(crate) max_ranges: usize,
    /// The most reactions of one message; the newest are kept.
    pub(crate) max_reactions: usize,
    /// The most ids of later versions of one edited message; the newest are kept (the
    /// original's id is the message's own and always stands).
    pub(crate) max_edits: usize,
    /// The largest raw item kept with a placeholder, in bytes of its encoding.
    pub(crate) max_raw_bytes: usize,
    /// The most pinned chats kept pinned: the first ones in the phone's order. The pin list
    /// goes to the core as ONE event, so its size is bounded here; Signal's clients pin a few
    /// chats. The rest stay chats and lose only the pin (counted, [`reason::PINS_OVER_CAP`]).
    pub(crate) max_pins: usize,
    /// The most entries of each member list of a group's snapshot (members, invited,
    /// requesting); the rest are dropped. The server's group size is far below.
    pub(crate) max_members: usize,
    /// The longest group title, in bytes; cut on a character.
    pub(crate) max_title_bytes: usize,
    /// The most notices one group-change item makes (after folding); the rest are skipped.
    /// At most [`ROWS_PER_FRAME`] (each notice is a row of its frame).
    pub(crate) max_group_notices: usize,
    /// The most attachments of one message; the rest are dropped.
    pub(crate) max_attachments: usize,
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
            max_quote_bytes: 4 * 1024,
            max_ranges: 2_048,
            max_reactions: 1_000,
            max_edits: MAX_HISTORY_EDITS,
            max_raw_bytes: 64 * 1024,
            max_pins: 1_000,
            max_members: 4_096,
            max_title_bytes: 1_024,
            max_group_notices: 64,
            max_attachments: 1_000,
        }
    }
}

/// What decides whether an attachment of the archive can still be fetched (step S5): Signal
/// Desktop's own statement of what "could still exist on the transit tier"
/// (`AttachmentDownloadManager.preload.ts`, the jobs of an import without media): the
/// pointer's upload time, else the message's ORIGINAL sent time (`uploadTimestamp ||
/// sentAt`: an upload time of 0 is none), is old when it is at or before the reference time
/// minus twice the server's message queue time (at least 30 days: [`MediaRules::new`]).
/// Desktop uses the rule only for what it fetches by itself; Kourier makes an old one "not
/// available on this device" at once (user decision, 2026-10-10: a later step asks the phone).
///
/// The reference is the PHONE's clock at the export (`ArchiveInfo.backup_time_ms`), the
/// clock that wrote the sent and upload times, bounded to within a day of the import's
/// time: this computer's clock decides nothing in the mapper, and a re-read of the same
/// archive gives the same rows. An upload time ahead of the reference (a sender's claim) is
/// young.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MediaRules {
    /// The reference time (ms).
    pub(crate) reference_ms: i64,
    /// Twice the queue time (ms): older than this before the reference is old.
    pub(crate) window_ms: i64,
    /// The largest attachment the server lets this account receive (bytes,
    /// `global.attachments.maxReceiveBytes`): a larger one is not available.
    pub(crate) receive_limit: u64,
}

/// The least queue time the rule counts with (Desktop's `getMessageQueueTime`: `MONTH`).
pub(crate) const QUEUE_TIME_FLOOR: Duration = Duration::from_secs(30 * 24 * 60 * 60);

impl MediaRules {
    /// The rules of an import at `now` (ms) of an archive the phone exported at
    /// `backup_time_ms` (0: unknown, then `now`), with the server's message queue time (none
    /// stored: the 30-day floor) and receive limit.
    pub(crate) fn new(
        now: i64,
        backup_time_ms: u64,
        queue_time: Option<Duration>,
        receive_limit: u64,
    ) -> Self {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        let reference_ms = match to_i64(backup_time_ms) {
            0 => now,
            at => at.clamp(now.saturating_sub(DAY), now.saturating_add(DAY)),
        };
        let queue = queue_time.unwrap_or(QUEUE_TIME_FLOOR).max(QUEUE_TIME_FLOOR);
        let window_ms = i64::try_from(queue.as_millis().saturating_mul(2)).unwrap_or(i64::MAX);
        Self {
            reference_ms,
            window_ms,
            receive_limit,
        }
    }

    /// Whether a place uploaded (else sent) at `at` (ms) is too old to be on the server.
    pub(crate) fn is_old(&self, at: i64) -> bool {
        at <= self.reference_ms.saturating_sub(self.window_ms)
    }
}

/// The longest disappearing timer: the bound of the live path (`i32::MAX` seconds).
const MAX_TIMER: Duration = Duration::from_secs(i32::MAX as u64);

/// `BackendBlob.v` of the raw item a placeholder keeps: the encoded backup `ChatItem` (the
/// live path keeps a `DataMessage` as `v` 1), for the version that shows the kind.
pub(crate) const RAW_CHAT_ITEM: u16 = 4;

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
    /// A group whose master key is not 32 bytes: nothing can be done with it.
    pub(crate) const GROUP_KEY_INVALID: &str = "group_key_invalid";
    /// A second group frame with a master key an earlier frame had: its recipient id leads
    /// to the first frame's group, its own fields are not used.
    pub(crate) const DUPLICATE_GROUP: &str = "duplicate_group";
    /// A group's member list (members, invited or requesting) longer than
    /// [`MapLimits::max_members`](super::MapLimits): the rest dropped (once per list).
    pub(crate) const MEMBERS_CUT: &str = "members_cut";
    /// A member entry whose id is not a service id: dropped (once per entry).
    pub(crate) const MEMBER_DROPPED: &str = "member_dropped";
    /// A group title cut at [`MapLimits::max_title_bytes`](super::MapLimits).
    pub(crate) const TITLE_CUT: &str = "title_cut";
    /// A group-change entry whose actor (or the member it is about) is absent, empty or not
    /// a service id: skipped, because a notice without one would read "You …" (`by: None`
    /// is our own account). Once per entry.
    pub(crate) const GROUP_CHANGE_NO_ACTOR: &str = "group_change_no_actor";
    /// Notices of one group-change item beyond [`MapLimits::max_group_notices`](super::MapLimits)
    /// (once per notice).
    pub(crate) const GROUP_NOTICES_CUT: &str = "group_notices_cut";
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
    /// A standard message with no text (and nothing this step imports), or a story reply
    /// with no reply in it.
    pub(crate) const EMPTY: &str = "empty";
    /// Imported, with only the first [`MapLimits::max_attachments`](super::MapLimits) of its
    /// attachments (once per message).
    pub(crate) const ATTACHMENTS_CUT: &str = "attachments_cut";
    /// An attachment (a long text, a sticker's image) imported as not available on this
    /// device: its pointer names no place a download can be made of (no locator, no place
    /// on the transit tier, a key, hash or digest of the wrong length, an unsafe CDN key, a
    /// CDN this version does not know, a size of 0). Once per pointer; a `MessageAttachment`
    /// without any pointer is dropped and counted here too.
    pub(crate) const ATTACHMENT_NOT_AVAILABLE: &str = "attachment_not_available";
    /// An attachment imported as not available on this device: its place is older than the
    /// server keeps it ([`MediaRules`](super::MediaRules)). Once per pointer.
    pub(crate) const ATTACHMENT_TOO_OLD: &str = "attachment_too_old";
    /// An attachment imported as not available on this device: larger than the server lets
    /// this account receive. Once per pointer.
    pub(crate) const ATTACHMENT_TOO_LARGE: &str = "attachment_too_large";
    /// A notice the contract has no row for (and the live path makes none): a simple
    /// update of another kind, a profile change, a merge of two chats, a group change
    /// outside a group, a notice about the chat's contact in Note to Self, a group-change
    /// entry of a kind the contract has no notice for (once per entry).
    pub(crate) const NOTICE_NOT_IN_CONTRACT: &str = "notice_not_in_contract";
    /// A call of a type, direction or state this version does not know.
    pub(crate) const UNKNOWN_CALL: &str = "unknown_call";
    /// A call in a chat that is not of its kind: a group call outside a group, a call in
    /// Note to Self.
    pub(crate) const CALL_NOT_IN_ITS_CHAT: &str = "call_not_in_its_chat";
    /// Imported, with only the first [`MapLimits::max_ranges`](super::MapLimits) of its
    /// formatting and mention ranges (once per text).
    pub(crate) const RANGES_CUT: &str = "ranges_cut";
    /// A formatting or mention range that does not fit its text (outside it, inside a
    /// character, a mention of no valid ACI or not on its placeholder, a style of no kind):
    /// dropped, the text is imported (once per range).
    pub(crate) const RANGE_DROPPED: &str = "range_dropped";
    /// Imported, the text of its quote cut at [`MapLimits::max_quote_bytes`](super::MapLimits).
    pub(crate) const QUOTE_TEXT_CUT: &str = "quote_text_cut";
    /// Imported, with only the newest [`MapLimits::max_reactions`](super::MapLimits) of its
    /// reactions (once per message).
    pub(crate) const REACTIONS_CUT: &str = "reactions_cut";
    /// A reaction by somebody who is not us and not the chat's contact, or by an unknown
    /// recipient: dropped (once per reaction).
    pub(crate) const REACTION_UNKNOWN_AUTHOR: &str = "reaction_unknown_author";
    /// A reaction that is not one emoji (the live rule, `crate::emoji`): dropped (once per
    /// reaction).
    pub(crate) const REACTION_NOT_ONE_EMOJI: &str = "reaction_not_one_emoji";
    /// Imported as edited, with the ids of only the newest
    /// [`MapLimits::max_edits`](super::MapLimits) of its later versions (once per message).
    pub(crate) const EDITS_CUT: &str = "edits_cut";
    /// A placeholder imported without its raw item, which is larger than
    /// [`MapLimits::max_raw_bytes`](super::MapLimits).
    pub(crate) const RAW_TOO_LARGE: &str = "raw_too_large";
    /// A pinned chat beyond the first [`MapLimits::max_pins`](super::MapLimits) in the
    /// phone's order: imported as a chat, without the pin (once per chat).
    pub(crate) const PINS_OVER_CAP: &str = "pins_over_cap";
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
        /// The phone's pinned order (`Chat.pinnedOrder`, not 0).
        pinned: Option<u32>,
    },
    /// A message of chat `chat`, row `n`.
    Item {
        /// The chat's `idx`.
        chat: u32,
        /// The row's place: its frame's running number times [`ROWS_PER_FRAME`] plus its
        /// place among that frame's rows.
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
    /// The staged chats whose pin goes: beyond [`MapLimits::max_pins`] in the phone's order
    /// (counted under [`reason::PINS_OVER_CAP`]). The stage keeps them as chats.
    pub(crate) unpinned: Vec<u32>,
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
    /// A group: an index into the mapper's groups.
    Group(usize),
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
    /// Its PNI as a user id, when the person is known by an ACI AND has a PNI (`Mapper::by_pni`).
    pni: Option<UserId>,
    /// The row in the stage's contacts, once staged.
    staged: Option<u32>,
    /// What the staged row needs, until it is staged.
    pending: Option<Box<StagedContact>>,
}

/// A group of the archive.
struct Group {
    key: ConvKey,
    /// What the stage gets, from the group frame and its snapshot.
    staged: StagedGroup,
    /// The people of its member lists who have a contact frame: staged with the group.
    people: Vec<usize>,
    /// The snapshot's timer (s), when on.
    timer_secs: Option<u32>,
}

/// A chat of the archive, by what its items become.
#[derive(Clone, Copy)]
enum Chat {
    /// A conversation: an index into the mapper's conversations.
    Conv(usize),
    /// The release notes' chat.
    ReleaseNotes,
}

/// Whom a conversation is with.
#[derive(Clone, Copy)]
enum ConvKind {
    /// Note to Self.
    NoteToSelf,
    /// A person: an index into the mapper's people.
    Direct(usize),
    /// A group: an index into the mapper's groups.
    Group(usize),
}

impl ConvKind {
    /// The person of a 1:1 chat.
    fn peer(self) -> Option<usize> {
        match self {
            Self::Direct(person) => Some(person),
            Self::NoteToSelf | Self::Group(_) => None,
        }
    }

    /// Whether `person` may write, react, be quoted or be named in a send state here: in a
    /// 1:1 chat its contact only, in a group anybody of the archive, in Note to Self nobody
    /// (but us).
    fn admits(self, person: usize) -> bool {
        match self {
            Self::Direct(peer) => peer == person,
            Self::Group(_) => true,
            Self::NoteToSelf => false,
        }
    }

    fn is_group(self) -> bool {
        matches!(self, Self::Group(_))
    }
}

/// Who a group-change entry names.
enum Named {
    /// Our own account (by its ACI or its PNI).
    Us,
    /// Somebody else.
    User(UserId),
    /// Nobody: the field is absent, empty or not a service id.
    Nobody,
}

/// A conversation: one or more chat frames of the same recipient.
struct Conv {
    key: ConvKey,
    kind: ConvKind,
    /// What the FIRST chat frame of the conversation carried.
    frame: ChatFrame,
    /// The first chat frame's `pinnedOrder`, when set and not 0.
    pinned: Option<u32>,
    /// The row in the stage's chats, once it is staged.
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
    media: MediaRules,
    /// The frames seen so far: a frame's running number, from 1.
    frame: u64,
    recipients: HashMap<u64, Recipient>,
    recipient_bytes: usize,
    people: Vec<Person>,
    by_user: HashMap<UserId, usize>,
    /// The people who are known by an ACI and also have a PNI, by that PNI: a group's invitee
    /// is named by PNI, and the contact frame of the same person leads with the ACI.
    by_pni: HashMap<UserId, usize>,
    groups: Vec<Group>,
    by_group: HashMap<ConvKey, usize>,
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
            media: MediaRules::new(now, 0, None, crate::attachment::DEFAULT_MAX_RECEIVE_BYTES),
            frame: 0,
            recipients: HashMap::new(),
            recipient_bytes: 0,
            people: Vec::new(),
            by_user: HashMap::new(),
            by_pni: HashMap::new(),
            groups: Vec::new(),
            by_group: HashMap::new(),
            chats: HashMap::new(),
            convs: Vec::new(),
            by_key: HashMap::new(),
            staged_contacts: 0,
            staged_chats: 0,
            counts: BTreeMap::new(),
        })
    }

    /// The same mapper with these rules for attachments (by default: the import's time as
    /// the reference, the 30-day floor and the default receive limit).
    pub(crate) fn with_media(mut self, media: MediaRules) -> Self {
        self.media = media;
        self
    }

    fn count(&mut self, reason: &'static str) {
        *self.counts.entry(reason).or_insert(0) += 1;
    }

    /// Maps the next frame; what it makes for the stage is appended to `out`.
    pub(crate) fn frame(&mut self, frame: Frame, out: &mut Vec<Staged>) {
        self.frame += 1;
        match frame {
            Frame::Recipient(recipient) => self.recipient(recipient),
            Frame::Chat(chat) => self.chat(&chat, out),
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
    pub(crate) fn finish(mut self) -> MapEnd {
        let mut chats: Vec<ChatCounters> = self
            .convs
            .iter()
            .filter(|conv| conv.idx.is_some())
            .map(|conv| conv.counters)
            .collect();
        chats.sort_by_key(|chat| chat.idx);
        // The pin list is bounded: the first `max_pins` in the phone's order (ascending
        // `pinnedOrder`, ties by the stage's order, as the delivery sorts them) keep their pin.
        let mut pinned: Vec<(u32, u32)> = self
            .convs
            .iter()
            .filter_map(|conv| Some((conv.pinned?, conv.idx?)))
            .collect();
        pinned.sort_unstable();
        let unpinned: Vec<u32> = pinned
            .into_iter()
            .skip(self.limits.max_pins)
            .map(|(_, idx)| idx)
            .collect();
        if !unpinned.is_empty() {
            *self.counts.entry(reason::PINS_OVER_CAP).or_insert(0) += unpinned.len() as u64;
        }
        MapEnd {
            chats,
            unpinned,
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
        let groups = self.groups.len();
        let kind = match recipient.destination {
            Some(Destination::Self_(_)) => Recipient::Own,
            Some(Destination::Contact(contact)) => match self.contact(contact) {
                Some((kind, bytes)) => {
                    cost += bytes;
                    kind
                }
                None => Recipient::Skipped,
            },
            Some(Destination::Group(group)) => match self.group(group) {
                Some((kind, bytes)) => {
                    cost += bytes;
                    kind
                }
                None => Recipient::Skipped,
            },
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
                // The PNI leads to this person only if nobody had it before (`or_insert`).
                if let Some(pni) = &person.pni
                    && self.by_pni.get(pni) == Some(&self.people.len())
                {
                    self.by_pni.remove(pni);
                }
            }
            if self.groups.len() > groups
                && let Some(group) = self.groups.pop()
            {
                self.by_group.remove(&group.key);
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
        let aci_user = aci.and_then(|aci| aci_user_id(aci).ok());
        let pni_user = pni.and_then(|pni| pni_user_id(pni).ok());
        let user = aci_user
            .clone()
            .or_else(|| pni_user.clone())
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
        // Known by an ACI and also by a PNI: a group may name the person by either.
        let pni = pni_user.filter(|pni| aci_user.is_some() && *pni != user);
        let bytes = std::mem::size_of::<Person>()
            + std::mem::size_of::<StagedContact>()
            + 2 * user.as_str().len()
            + 2 * pni.as_ref().map_or(0, |pni| pni.as_str().len())
            + [&staged.profile_name, &staged.system_name, &staged.nickname]
                .into_iter()
                .chain([&staged.e164])
                .map(|text| text.as_ref().map_or(0, String::len))
                .sum::<usize>();
        let person = self.people.len();
        self.by_user.insert(user.clone(), person);
        if let Some(pni) = &pni {
            self.by_pni.entry(pni.clone()).or_insert(person);
        }
        self.people.push(Person {
            user,
            pni,
            staged: None,
            pending: Some(Box::new(staged)),
        });
        Some((Recipient::Contact(person), bytes))
    }

    /// A group frame: its conversation and what its staged chat will need, from the master
    /// key and the phone's snapshot. Returns the kind and an estimate of the memory kept for
    /// it; `None` (counted) when its master key is not 32 bytes. The invite link's password
    /// in the snapshot is never read.
    fn group(&mut self, mut group: backup::Group) -> Option<(Recipient, usize)> {
        use backup::group::group_attribute_blob::Content;
        use backup::group::member::Role;
        // The key is wiped where it is read: the buffer of the decoded frame is ours now.
        let raw_key = zeroize::Zeroizing::new(std::mem::take(&mut group.master_key));
        let Some(keys) = crate::groups::GroupKeys::from_slice(&raw_key) else {
            self.count(reason::GROUP_KEY_INVALID);
            return None;
        };
        let master_key = MasterKey(keys.master);
        let key = keys.conv_key();
        if let Some(first) = self.by_group.get(&key) {
            // The same group again: one conversation, the first frame stands.
            let first = *first;
            self.count(reason::DUPLICATE_GROUP);
            return Some((Recipient::Group(first), 0));
        }
        let snapshot = group.snapshot.unwrap_or_default();
        let title = snapshot.title.and_then(|blob| match blob.content {
            Some(Content::Title(title)) => Some(title),
            _ => None,
        });
        // Android writes an EMPTY snapshot when it had no state of the group: no title blob
        // and no lists. That is "unknown", never "left".
        let empty = title.is_none()
            && snapshot.members.is_empty()
            && snapshot.members_pending_profile_key.is_empty()
            && snapshot.members_pending_admin_approval.is_empty();
        let own_aci = libsignal_core::ServiceId::from(self.own_aci);
        let own_pni = libsignal_core::ServiceId::from(self.own_pni);
        let aci_of = |bytes: &[u8]| <[u8; 16]>::try_from(bytes).ok().map(Aci::from_uuid_bytes);
        // Our place, over the whole lists (before any cut).
        let membership = if empty {
            GroupMembership::Unknown
        } else if snapshot
            .members
            .iter()
            .any(|m| aci_of(&m.user_id) == Some(self.own_aci))
        {
            GroupMembership::Member
        } else if snapshot.members_pending_profile_key.iter().any(|p| {
            p.member
                .as_ref()
                .and_then(|m| libsignal_core::ServiceId::parse_from_service_id_binary(&m.user_id))
                .is_some_and(|id| id == own_aci || id == own_pni)
        }) {
            GroupMembership::Invited
        } else if snapshot
            .members_pending_admin_approval
            .iter()
            .any(|r| aci_of(&r.user_id) == Some(self.own_aci))
        {
            GroupMembership::Requesting
        } else {
            GroupMembership::Left
        };
        let max = self.limits.max_members;
        let mut members: Vec<Member> = Vec::new();
        let mut list = |n: usize,
                        entries: &mut dyn Iterator<Item = Option<Member>>,
                        counts: &mut BTreeMap<&'static str, u64>| {
            if n > max {
                *counts.entry(reason::MEMBERS_CUT).or_insert(0) += 1;
            }
            for entry in entries.take(max) {
                match entry {
                    Some(member) => members.push(member),
                    None => *counts.entry(reason::MEMBER_DROPPED).or_insert(0) += 1,
                }
            }
        };
        list(
            snapshot.members.len(),
            &mut snapshot.members.iter().map(|m| {
                Some(Member {
                    user: aci_user_id(aci_of(&m.user_id)?).ok()?,
                    role: if m.role == Role::Administrator as i32 {
                        MemberRole::Admin
                    } else {
                        MemberRole::Member
                    },
                })
            }),
            &mut self.counts,
        );
        list(
            snapshot.members_pending_profile_key.len(),
            &mut snapshot.members_pending_profile_key.iter().map(|p| {
                // A service id BINARY: 16 bytes are an ACI, 17 (0x01 first) a PNI.
                let id = libsignal_core::ServiceId::parse_from_service_id_binary(
                    &p.member.as_ref()?.user_id,
                )?;
                Some(Member {
                    user: crate::service_id::service_id_user_id(id).ok()?,
                    role: MemberRole::Pending,
                })
            }),
            &mut self.counts,
        );
        list(
            snapshot.members_pending_admin_approval.len(),
            &mut snapshot.members_pending_admin_approval.iter().map(|r| {
                Some(Member {
                    user: aci_user_id(aci_of(&r.user_id)?).ok()?,
                    role: MemberRole::Requesting,
                })
            }),
            &mut self.counts,
        );
        let title = title.filter(|title| !title.is_empty()).map(|title| {
            let (title, was_cut) = cut(title, self.limits.max_title_bytes);
            if was_cut {
                self.count(reason::TITLE_CUT);
            }
            title
        });
        let timer_secs = match snapshot.disappearing_messages_timer.and_then(|b| b.content) {
            Some(Content::DisappearingMessagesDuration(secs)) if secs > 0 => {
                Some(secs.min(i32::MAX as u32))
            }
            _ => None,
        };
        let people: Vec<usize> = members
            .iter()
            .filter_map(|member| self.person_of(&member.user))
            .collect();
        let bytes = std::mem::size_of::<Group>()
            + title.as_ref().map_or(0, String::len)
            + members.len() * (std::mem::size_of::<Member>() + 48)
            + people.len() * std::mem::size_of::<usize>();
        let index = self.groups.len();
        self.by_group.insert(key.clone(), index);
        self.groups.push(Group {
            key,
            staged: StagedGroup {
                master_key,
                title,
                members: (!empty).then_some(members),
                membership,
                blocked: group.blocked,
                whitelisted: group.whitelisted,
            },
            people,
            timer_secs,
        });
        Some((Recipient::Group(index), bytes))
    }

    fn chat(&mut self, chat: &backup::Chat, out: &mut Vec<Staged>) {
        if chat.id == 0 || self.chats.contains_key(&chat.id) {
            return self.count(reason::DUPLICATE_ID);
        }
        if self.chats.len() >= self.limits.max_chats {
            return self.count(reason::CHAT_OVER_CAP);
        }
        let (key, kind) = match self.recipients.get(&chat.recipient_id) {
            Some(Recipient::Own) => (ConvKey::Direct(self.own.clone()), ConvKind::NoteToSelf),
            Some(Recipient::Contact(person)) => (
                ConvKey::Direct(self.people[*person].user.clone()),
                ConvKind::Direct(*person),
            ),
            Some(Recipient::Group(group)) => {
                (self.groups[*group].key.clone(), ConvKind::Group(*group))
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
        // Two chat frames of one recipient are one conversation: the first stands (its
        // fields too), the rows of both go into it.
        let conv = match self.by_key.get(&key) {
            Some(conv) => *conv,
            None => {
                let conv = self.convs.len();
                self.by_key.insert(key.clone(), conv);
                let pinned = chat.pinned_order.filter(|order| *order != 0);
                let mut frame = chat_frame(chat);
                let mut invited = false;
                if let ConvKind::Group(group) = kind {
                    // The group's timer: the chat frame's (ms) wins, else the snapshot's (s).
                    frame.timer_secs = frame.timer_secs.or(self.groups[group].timer_secs);
                    invited = self.groups[group].staged.membership == GroupMembership::Invited;
                }
                self.convs.push(Conv {
                    key,
                    kind,
                    frame,
                    pinned,
                    idx: None,
                    counters: ChatCounters::default(),
                });
                // A pinned chat, and a group we are invited to, are listed whatever they hold
                // (Android and Desktop list them): staged now, also when they never get a row.
                if pinned.is_some() || invited {
                    self.stage_conv(conv, out);
                }
                conv
            }
        };
        self.chats.insert(chat.id, Chat::Conv(conv));
    }

    fn item(&mut self, mut item: backup::ChatItem, out: &mut Vec<Staged>) {
        use backup::chat_item::Item;
        let conv = match self.chats.get(&item.chat_id) {
            Some(Chat::Conv(conv)) => *conv,
            Some(Chat::ReleaseNotes) => return self.count(reason::RELEASE_NOTES),
            None => return self.count(reason::UNKNOWN_CHAT),
        };
        if item.date_sent == 0 {
            return self.count(reason::NO_TIME);
        }
        let (Some(_), Some(content)) = (&item.directional_details, &item.item) else {
            return self.count(reason::UNKNOWN_ITEM);
        };
        // What is not imported at all, before anything else is looked at.
        let skip = match content {
            Item::RemoteDeletedMessage(_) | Item::AdminDeletedMessage(_) => Some(reason::DELETED),
            Item::StandardMessage(message)
                if message.attachments.is_empty()
                    && message.long_text.is_none()
                    && message
                        .text
                        .as_ref()
                        .is_none_or(|text| text.body.is_empty()) =>
            {
                Some(reason::EMPTY)
            }
            Item::DirectStoryReplyMessage(reply) if reply.reply.is_none() => Some(reason::EMPTY),
            _ => None,
        };
        if let Some(why) = skip {
            return self.count(why);
        }
        if let Item::UpdateMessage(_) = content {
            return self.update(conv, item, out);
        }

        // A message: who wrote it, its id, times, state and timer.
        let Some(mut msg) = self.message_of(conv, &item) else {
            return;
        };
        // The ORIGINAL sent time (an edited item's `dateSent` is its last edit's): what the
        // age of an attachment counts from when its pointer has no upload time.
        let sent = to_i64(
            item.revisions
                .first()
                .map(|first| first.date_sent)
                .filter(|at| *at != 0)
                .unwrap_or(item.date_sent),
        );
        // The encoded item, kept with a placeholder for the version that can show it
        // (never for view-once: its pointer and key would outlive the one view).
        let keeps_raw = match content {
            Item::StickerMessage(sticker) => {
                sticker.sticker.as_ref().is_none_or(|s| s.data.is_none())
            }
            Item::ContactMessage(_)
            | Item::PaymentNotification(_)
            | Item::GiftBadge(_)
            | Item::Poll(_)
            | Item::DirectStoryReplyMessage(_) => true,
            _ => false,
        };
        if keeps_raw {
            msg.raw = self.raw(&item);
        }
        msg.edits = self.edits(&item);
        let unsupported = |kind: &str| Body::Unsupported {
            kind_hint: kind.to_owned(),
            summary: None,
        };
        let reactions = match item.item.take() {
            Some(Item::StandardMessage(message)) => {
                let text = message.text.unwrap_or_default();
                let text = self.text(
                    text.body,
                    &text.body_ranges,
                    self.limits.max_text_bytes,
                    reason::TEXT_CUT,
                );
                msg.mentions_me = text.ranges.iter().any(
                    |range| matches!(&range.kind, RangeKind::Mention(user) if *user == self.own),
                );
                let mut attachments = Vec::new();
                for attachment in message.attachments.iter().take(self.limits.max_attachments) {
                    if let Some(attachment) = self.attachment(attachment, sent) {
                        attachments.push(attachment);
                    }
                }
                if message.attachments.len() > self.limits.max_attachments {
                    self.count(reason::ATTACHMENTS_CUT);
                }
                // The rest of the text, behind the inline body (the phone keeps the pointer
                // only when it never fetched the rest itself; the inline body is its first
                // 2 KiB). The core fetches it by itself while it has a pointer.
                if let Some(long) = &message.long_text {
                    attachments.push(self.long_text(long, sent));
                }
                msg.body = if attachments.is_empty() {
                    if text.body.is_empty() {
                        // Nothing of it is left (attachments without a pointer at all).
                        return self.count(reason::EMPTY);
                    }
                    Body::Text(text)
                } else {
                    Body::Media {
                        attachments,
                        caption: (!text.body.is_empty()).then_some(text),
                    }
                };
                msg.reply_to = message.quote.map(|quote| self.quote(conv, &quote));
                // A link preview is not shown by Kourier: its URL is in the text.
                message.reactions
            }
            Some(Item::StickerMessage(message)) => {
                msg.body = match message.sticker.as_ref().and_then(|s| self.sticker(s, sent)) {
                    Some(image) => Body::Sticker(Box::new(image)),
                    None => unsupported("sticker"),
                };
                message.reactions
            }
            Some(Item::ViewOnceMessage(message)) => {
                // As live: a placeholder, NO attachment row and no raw item, although an
                // archive carries the live pointer of one that was not viewed.
                msg.body = unsupported("view-once");
                msg.view_once = true;
                message.reactions
            }
            Some(Item::ContactMessage(message)) => {
                msg.body = unsupported("contact");
                message.reactions
            }
            Some(Item::PaymentNotification(_)) => {
                msg.body = unsupported("payment");
                Vec::new()
            }
            Some(Item::GiftBadge(_)) => {
                msg.body = unsupported("gift-badge");
                Vec::new()
            }
            Some(Item::Poll(poll)) => {
                msg.body = unsupported("poll");
                poll.reactions
            }
            Some(Item::DirectStoryReplyMessage(reply)) => {
                msg.body = unsupported("story-reply");
                reply.reactions
            }
            // Handled above.
            Some(
                Item::RemoteDeletedMessage(_)
                | Item::AdminDeletedMessage(_)
                | Item::UpdateMessage(_),
            )
            | None => return,
        };
        msg.reactions = self.reactions(conv, &reactions, msg.sent_at);
        let n = row_n(self.frame, 0);
        self.row(conv, n, msg, out);
    }

    /// The row of a message item without its content (direction, author, id, times, read
    /// and send state, timer), or `None`, counted, when the item is not one of this chat.
    fn message_of(&mut self, conv: usize, item: &backup::ChatItem) -> Option<IncomingMessage> {
        use backup::chat_item::DirectionalDetails;
        let key = self.convs[conv].key.clone();
        match item.directional_details.as_ref()? {
            DirectionalDetails::Incoming(incoming) => {
                let author = match self.recipients.get(&item.author_id) {
                    Some(Recipient::Contact(person)) => *person,
                    _ => {
                        self.count(reason::UNKNOWN_AUTHOR);
                        return None;
                    }
                };
                if !self.convs[conv].kind.admits(author) {
                    self.count(reason::AUTHOR_NOT_IN_CHAT);
                    return None;
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
                Some(msg)
            }
            DirectionalDetails::Outgoing(outgoing) => {
                let mut msg = self.message(
                    &key,
                    &item.revisions,
                    item.date_sent,
                    Origin::OwnOtherDevice { device: None },
                    outgoing.date_received,
                );
                let state = self.send_state(self.convs[conv].kind, &outgoing.send_status);
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
                Some(msg)
            }
            // A notice has no direction; a message without one is not a message.
            DirectionalDetails::Directionless(_) => {
                self.count(reason::UNKNOWN_ITEM);
                None
            }
        }
    }

    /// An update item of a 1:1 chat (or Note to Self): a notice, a placeholder or a call
    /// line (architecture §14.2 item 14; the design's tables 2.9 and 2.10).
    fn update(&mut self, conv: usize, item: backup::ChatItem, out: &mut Vec<Staged>) {
        use backup::chat_item::Item;
        use backup::chat_update_message::Update;
        use backup::simple_chat_update::Type;
        let Some(Item::UpdateMessage(backup::ChatUpdateMessage {
            update: Some(update),
        })) = &item.item
        else {
            return self.count(reason::UNKNOWN_ITEM);
        };
        let kind = self.convs[conv].kind;
        let peer = kind.peer();
        match update {
            Update::IndividualCall(call) => {
                let call = *call;
                return self.individual_call(conv, &item, &call, out);
            }
            // A group call is a line of its group.
            Update::GroupCall(call) if kind.is_group() => {
                let call = *call;
                let key = self.convs[conv].key.clone();
                if let Some(msg) = self.group_call(key, &item, &call, out) {
                    let n = row_n(self.frame, 0);
                    self.row(conv, n, msg, out);
                }
                return;
            }
            Update::GroupCall(_) => return self.count(reason::CALL_NOT_IN_ITS_CHAT),
            Update::GroupChange(change) if kind.is_group() => {
                return self.group_change(conv, &item, &change.updates, out);
            }
            _ => {}
        }
        // Who caused it: we, or a person of the chat (its contact; in a group anybody).
        let actor = match self.recipients.get(&item.author_id) {
            Some(Recipient::Own) => None,
            Some(Recipient::Contact(person)) if kind.admits(*person) => Some(*person),
            Some(Recipient::Contact(_)) => return self.count(reason::AUTHOR_NOT_IN_CHAT),
            _ => return self.count(reason::UNKNOWN_AUTHOR),
        };
        let user = |person: usize| self.people[person].user.clone();
        let system = |event: SystemEvent| Some(Body::System(event));
        let placeholder = |kind: &str| {
            Some(Body::Unsupported {
                kind_hint: kind.to_owned(),
                summary: None,
            })
        };
        // A notice about the chat's contact, by them.
        let by_peer = actor.map(user);
        let (body, keeps_raw) = match update {
            Update::ExpirationTimerChange(change) => (
                system(SystemEvent::TimerChanged {
                    by: by_peer.clone(),
                    timer: timer(Some(change.expires_in_ms)),
                }),
                false,
            ),
            Update::SimpleUpdate(simple) => (
                match Type::try_from(simple.r#type) {
                    Ok(Type::IdentityUpdate) => by_peer
                        .clone()
                        .and_then(|user| system(SystemEvent::IdentityKeyChanged { user })),
                    Ok(Type::ChangeNumber) => by_peer
                        .clone()
                        .and_then(|user| system(SystemEvent::NumberChanged { user })),
                    // With the chat's contact, whoever reset it.
                    Ok(Type::EndSession | Type::ChatSessionRefresh) => peer
                        .map(user)
                        .and_then(|user| system(SystemEvent::SessionRefreshed { user })),
                    Ok(Type::MessageRequestAccepted) => system(SystemEvent::MessageRequestAccepted),
                    // Messages on the wire: the live path's placeholders (they hold nothing
                    // more to keep).
                    Ok(Type::UnsupportedProtocolMessage) => placeholder("newer-protocol"),
                    Ok(Type::PaymentsActivated | Type::PaymentActivationRequest) => {
                        placeholder("payment")
                    }
                    Ok(
                        Type::Unknown
                        | Type::JoinedSignal
                        | Type::IdentityVerified
                        | Type::IdentityDefault
                        | Type::ReleaseChannelDonationRequest
                        | Type::BadDecrypt
                        | Type::ReportedSpam
                        | Type::Blocked
                        | Type::Unblocked,
                    )
                    | Err(_) => None,
                },
                false,
            ),
            Update::PollTerminate(_) => (placeholder("poll-ended"), true),
            Update::PinMessage(_) => (placeholder("pin"), true),
            Update::GroupChange(_)
            | Update::ProfileChange(_)
            | Update::LearnedProfileChange(_)
            | Update::ThreadMerge(_)
            | Update::SessionSwitchover(_)
            | Update::IndividualCall(_)
            | Update::GroupCall(_) => (None, false),
        };
        let Some(body) = body else {
            return self.count(reason::NOTICE_NOT_IN_CONTRACT);
        };
        let timer_notice = matches!(update, Update::ExpirationTimerChange(_));
        let origin = match actor {
            Some(person) => Origin::Remote {
                author: self.people[person].user.clone(),
                device: None,
            },
            None => Origin::OwnOtherDevice { device: None },
        };
        let key = self.convs[conv].key.clone();
        let mut msg = self.message(&key, &item.revisions, item.date_sent, origin, 0);
        if matches!(body, Body::System(_)) {
            // A notice has no id on the network: one of its own, the same on every read of
            // the same archive (the frame and the place in the item). A placeholder keeps
            // the message's real id (`D4`): it stands for a message the network can name.
            msg.local = format!("h:{}:0", self.frame).into();
        }
        // A notice is never unread.
        msg.read = true;
        msg.expiry = if timer_notice {
            // Android never starts the timer of a timer-change notice: given here, the
            // core's rule for a notice would start it at the import and erase what the
            // phone keeps.
            None
        } else {
            self.notice_expiry(&item, &msg.origin)
        };
        if keeps_raw {
            msg.raw = self.raw(&item);
        }
        msg.body = body;
        let n = row_n(self.frame, 0);
        self.row(conv, n, msg, out);
    }

    /// Who a field of a group-change entry names: a service id BINARY (16 bytes an ACI, 17
    /// with 0x01 first a PNI); our ACI or PNI is us; absent, empty or anything else nobody.
    fn named(&self, bytes: Option<&[u8]>) -> Named {
        let Some(id) = bytes
            .filter(|bytes| !bytes.is_empty())
            .and_then(libsignal_core::ServiceId::parse_from_service_id_binary)
        else {
            return Named::Nobody;
        };
        if id == libsignal_core::ServiceId::from(self.own_aci)
            || id == libsignal_core::ServiceId::from(self.own_pni)
        {
            return Named::Us;
        }
        match crate::service_id::service_id_user_id(id) {
            Ok(user) => Named::User(user),
            Err(_) => Named::Nobody,
        }
    }

    /// The notices of a group-change item in the group `conv` (design 2.11 as the LIVE
    /// decoder makes them, `groups::decode::actions`): one per entry the contract has a
    /// notice for, the entries that add (or remove) members by one actor folded into one
    /// notice, at most [`MapLimits::max_group_notices`]; ids `h:<frame>:<k>` and sent times
    /// `sent_at + k`. The actor of an entry is its own field, never the item's author (Android
    /// writes US as the author of a change whose editor it did not know). `by: None` means
    /// our own account, so an entry whose actor is absent is skipped and counted.
    fn group_change(
        &mut self,
        conv: usize,
        item: &backup::ChatItem,
        updates: &[backup::group_change_chat_update::Update],
        out: &mut Vec<Staged>,
    ) {
        use backup::group_change_chat_update::update::Update as Entry;
        let by_of = |named: Named| match named {
            Named::Us => Some(None),
            Named::User(user) => Some(Some(user)),
            Named::Nobody => None,
        };
        let mut notices: Vec<SystemEvent> = Vec::new();
        for update in updates {
            // What the entry becomes: `Ok(Some(event))`, `Ok(None)` for a kind the contract
            // has no notice for, `Err(())` for an entry without its actor.
            let event: Result<Option<SystemEvent>, ()> = match &update.update {
                Some(Entry::GroupCreationUpdate(u)) => by_of(self.named(u.updater_aci.as_deref()))
                    .map(|by| Some(SystemEvent::GroupCreated { by }))
                    .ok_or(()),
                Some(Entry::GroupNameUpdate(u)) => {
                    match by_of(self.named(u.updater_aci.as_deref())) {
                        // Absent: the name was removed.
                        Some(by) => {
                            let title = u
                                .new_group_name
                                .clone()
                                .map(|title| self.notice_title(title))
                                .unwrap_or_default();
                            Ok(Some(SystemEvent::TitleChanged { by, title }))
                        }
                        None => Err(()),
                    }
                }
                Some(Entry::GroupAvatarUpdate(u)) => by_of(self.named(u.updater_aci.as_deref()))
                    .map(|by| Some(SystemEvent::AvatarChanged { by }))
                    .ok_or(()),
                Some(Entry::GroupDescriptionUpdate(u)) => {
                    by_of(self.named(u.updater_aci.as_deref()))
                        .map(|by| Some(SystemEvent::DescriptionChanged { by }))
                        .ok_or(())
                }
                // Milliseconds (Android truncates them to 32 bits: a timer above about 49.7
                // days is wrong here). For the notice only: the group's timer comes from the
                // chat frame or the snapshot.
                Some(Entry::GroupExpirationTimerUpdate(u)) => {
                    by_of(self.named(u.updater_aci.as_deref()))
                        .map(|by| {
                            Some(SystemEvent::TimerChanged {
                                by,
                                timer: timer(Some(u.expires_in_ms)),
                            })
                        })
                        .ok_or(())
                }
                Some(Entry::GroupMemberLeftUpdate(u)) => match self.named(Some(&u.aci)) {
                    Named::Us => Ok(Some(SystemEvent::MemberLeft {
                        user: self.own.clone(),
                    })),
                    Named::User(user) => Ok(Some(SystemEvent::MemberLeft { user })),
                    Named::Nobody => Err(()),
                },
                Some(Entry::GroupMemberRemovedUpdate(u)) => {
                    match (
                        by_of(self.named(u.remover_aci.as_deref())),
                        self.subject(Some(&u.removed_aci)),
                    ) {
                        (Some(by), Some(user)) => Ok(Some(SystemEvent::MembersRemoved {
                            by,
                            users: vec![user],
                        })),
                        _ => Err(()),
                    }
                }
                Some(Entry::GroupMemberAddedUpdate(u)) => {
                    match (
                        by_of(self.named(u.updater_aci.as_deref())),
                        self.subject(Some(&u.new_member_aci)),
                    ) {
                        (Some(by), Some(user)) => Ok(Some(SystemEvent::MembersAdded {
                            by,
                            users: vec![user],
                        })),
                        _ => Err(()),
                    }
                }
                // Joins of their own: the new member is the actor. `Joined` only for us.
                Some(Entry::GroupMemberJoinedUpdate(u)) => {
                    self.joined(Some(&u.new_member_aci)).map(Some).ok_or(())
                }
                Some(Entry::GroupMemberJoinedByLinkUpdate(u)) => {
                    self.joined(Some(&u.new_member_aci)).map(Some).ok_or(())
                }
                Some(Entry::GroupInvitationAcceptedUpdate(u)) => {
                    self.joined(Some(&u.new_member_aci)).map(Some).ok_or(())
                }
                // An approved request: added by the admin who approved it. A rejection is
                // no notice of the contract.
                Some(Entry::GroupJoinRequestApprovalUpdate(u)) if u.was_approved => {
                    match (
                        by_of(self.named(u.updater_aci.as_deref())),
                        self.subject(Some(&u.requestor_aci)),
                    ) {
                        (Some(by), Some(user)) => Ok(Some(SystemEvent::MembersAdded {
                            by,
                            users: vec![user],
                        })),
                        _ => Err(()),
                    }
                }
                // Roles, access, invitations, requests, the invite link, migrations, the end
                // of a group, generic and unset entries: no notice in the contract (as live).
                _ => Ok(None),
            };
            let event = match event {
                Ok(Some(event)) => event,
                Ok(None) => {
                    self.count(reason::NOTICE_NOT_IN_CONTRACT);
                    continue;
                }
                Err(()) => {
                    self.count(reason::GROUP_CHANGE_NO_ACTOR);
                    continue;
                }
            };
            fold(&mut notices, event);
        }
        let max = self.limits.max_group_notices.min(ROWS_PER_FRAME as usize);
        if notices.len() > max {
            for _ in max..notices.len() {
                self.count(reason::GROUP_NOTICES_CUT);
            }
            notices.truncate(max);
        }
        let key = self.convs[conv].key.clone();
        let frame = self.frame;
        for (k, event) in notices.into_iter().enumerate() {
            let place = k as u64;
            let origin = match notice_actor(&event, &self.own) {
                Some(author) => Origin::Remote {
                    author: author.clone(),
                    device: None,
                },
                None => Origin::OwnOtherDevice { device: None },
            };
            let timer_notice = matches!(event, SystemEvent::TimerChanged { .. });
            let mut msg = self.message(&key, &[], item.date_sent, origin, 0);
            msg.local = format!("h:{frame}:{k}").into();
            let k = i64::try_from(k).unwrap_or(i64::MAX);
            msg.sent_at = msg.sent_at.saturating_add(k);
            msg.received_at = msg.received_at.saturating_add(k).min(self.now);
            // A notice is never unread.
            msg.read = true;
            // A timer-change notice keeps no timer (as in a 1:1 chat).
            msg.expiry = if timer_notice {
                None
            } else {
                self.notice_expiry(item, &msg.origin)
            };
            msg.body = Body::System(event);
            self.row(conv, row_n(frame, place), msg, out);
        }
    }

    /// The member an entry is about: our own user id for us, `None` for nobody.
    fn subject(&self, bytes: Option<&[u8]>) -> Option<UserId> {
        match self.named(bytes) {
            Named::Us => Some(self.own.clone()),
            Named::User(user) => Some(user),
            Named::Nobody => None,
        }
    }

    /// A join of a member's own (by an invitation accepted, a link, a join of an unknown
    /// editor): `Joined` when it is us, else "X joined", which the contract says as
    /// `MembersAdded { by: X, users: [X] }` (the live path names the invitee so). `None` when
    /// the entry names nobody.
    fn joined(&self, bytes: Option<&[u8]>) -> Option<SystemEvent> {
        match self.named(bytes) {
            Named::Us => Some(SystemEvent::Joined),
            Named::User(user) => Some(SystemEvent::MembersAdded {
                by: Some(user.clone()),
                users: vec![user],
            }),
            Named::Nobody => None,
        }
    }

    /// The title of a name change: one outer pair of bidi isolates taken off (Android wraps
    /// a title with any non-ASCII character in U+2068 … U+2069 in the notice, not in the
    /// snapshot), cut at [`MapLimits::max_title_bytes`] (counted).
    fn notice_title(&mut self, title: String) -> String {
        let title = match title
            .strip_prefix('\u{2068}')
            .and_then(|rest| rest.strip_suffix('\u{2069}'))
        {
            Some(inner) => inner.to_owned(),
            None => title,
        };
        let (title, was_cut) = cut(title, self.limits.max_title_bytes);
        if was_cut {
            self.count(reason::TITLE_CUT);
        }
        title
    }

    /// The timer of a notice or a placeholder of an update item: only one the phone started
    /// (an update whose timer never started on the phone stays there, and given without a
    /// start the core would start it at the import).
    fn notice_expiry(&mut self, item: &backup::ChatItem, origin: &Origin) -> Option<Expiry> {
        let timer = timer(item.expires_in_ms)?;
        let start = self.timer_start(item.expire_start_date)?;
        Some(Expiry {
            kind: if origin.is_own() {
                DisappearKind::AfterSend
            } else {
                DisappearKind::AfterRead
            },
            timer,
            started_at: Some(start),
        })
    }

    /// The time of a call line: when it started, else the item's sent time; one more than a
    /// day ahead is the import's time (as a sort time), counted.
    fn call_time(&mut self, started: u64, date_sent: u64) -> i64 {
        let at = to_i64(if started == 0 { date_sent } else { started });
        if at > self.latest() {
            self.count(reason::FUTURE_TIME);
            return self.now;
        }
        at
    }

    /// The timer of a call line (the S3a contract: in a history batch the line takes the
    /// timer it states). A start the phone wrote stands. Without one: an OUTGOING line is
    /// started at the import's time (the core would count it from the call's time and erase
    /// an old call at once, as for an own message the phone calls sent); an incoming one
    /// keeps its after-read timer without a start, which the core starts when the line is
    /// read (a line that is not unread: when it is stored).
    fn call_expiry(&mut self, item: &backup::ChatItem, outgoing: bool) -> Option<Expiry> {
        let timer = timer(item.expires_in_ms)?;
        let start = self.timer_start(item.expire_start_date);
        Some(if outgoing {
            if start.is_none() {
                self.count(reason::TIMER_NOT_STARTED);
            }
            Expiry {
                kind: DisappearKind::AfterSend,
                timer,
                started_at: Some(start.unwrap_or(self.now)),
            }
        } else {
            Expiry {
                kind: DisappearKind::AfterRead,
                timer,
                started_at: start,
            }
        })
    }

    /// The id of a call: its `callId` in decimal, the form of the phone's sync, so a later
    /// live signal of the same call joins the line; without one the frame's own id, which
    /// joins nothing. A `callId` of 0 is no id (Desktop's importer takes it so, and a writer
    /// that fills the field with its default writes it): every such call is a line of its
    /// own, or all of a chat's would be one.
    fn call_id(&self, call_id: Option<u64>) -> String {
        call_id
            .filter(|id| *id != 0)
            .map_or_else(|| format!("h{}", self.frame), |id| id.to_string())
    }

    /// A 1:1 call: one line in every state, said by our own device (only it may say
    /// answered, declined or unanswered, and the archive is what our phone says).
    fn individual_call(
        &mut self,
        conv: usize,
        item: &backup::ChatItem,
        call: &backup::IndividualCall,
        out: &mut Vec<Staged>,
    ) {
        use backup::individual_call::{Direction, State, Type};
        let Some(peer) = self.convs[conv].kind.peer() else {
            return self.count(reason::CALL_NOT_IN_ITS_CHAT);
        };
        // The zero values read as the proto says: audio, incoming, accepted.
        let media = match Type::try_from(call.r#type) {
            Ok(Type::UnknownType | Type::AudioCall) => CallMedia::Voice,
            Ok(Type::VideoCall) => CallMedia::Video,
            Err(_) => return self.count(reason::UNKNOWN_CALL),
        };
        let direction = match Direction::try_from(call.direction) {
            Ok(Direction::UnknownDirection | Direction::Incoming) => CallDirection::Incoming,
            Ok(Direction::Outgoing) => CallDirection::Outgoing,
            Err(_) => return self.count(reason::UNKNOWN_CALL),
        };
        let outgoing = direction == CallDirection::Outgoing;
        let state = match State::try_from(call.state) {
            Ok(State::UnknownState | State::Accepted) => CallState::Answered,
            Ok(State::NotAccepted) if outgoing => CallState::Unanswered,
            Ok(State::NotAccepted) => CallState::Declined,
            // `normalized` makes it unanswered when outgoing.
            Ok(State::Missed | State::MissedNotificationProfile) => CallState::Missed,
            Err(_) => return self.count(reason::UNKNOWN_CALL),
        };
        let at = self.call_time(call.started_call_timestamp, item.date_sent);
        let mut line = CallLine::signal(direction, false, state, at).with_media(media, at);
        if !outgoing {
            line = line.with_by(self.people[peer].user.clone(), at);
        }
        let read = !line.counts_unread() || call.read;
        let key = self.convs[conv].key.clone();
        let call_id = self.call_id(call.call_id);
        let mut msg = IncomingMessage::call(
            key,
            &call_id,
            Origin::OwnOtherDevice { device: None },
            line,
            at,
            // Never after the import: nothing was received in the future.
            at.min(self.now),
        );
        msg.silent = true;
        msg.read = read;
        msg.expiry = self.call_expiry(item, outgoing);
        let n = row_n(self.frame, 0);
        self.row(conv, n, msg, out);
    }

    /// The line of a group call in the group `conv` (design 2.10), said by our own device;
    /// the people it names are staged into `out`. `None`, counted, for a state this version
    /// does not know. "Started by" and the ringer are `by` only when they are a contact
    /// person (never us).
    ///
    /// A call that still RINGS in the archive (it rang at the moment the phone exported) is
    /// reported with `read: false` and a ring with no time left (a deadline at or before the
    /// import's time): a ringing line has no read state, and the core settles it at once, as
    /// an unread missed call and silently, whatever a `read` said (the S3a contract). It
    /// never rings here, however near the import it started and whichever clock is ahead.
    pub(crate) fn group_call(
        &mut self,
        conv: ConvKey,
        item: &backup::ChatItem,
        call: &backup::GroupCall,
        out: &mut Vec<Staged>,
    ) -> Option<IncomingMessage> {
        use backup::group_call::State;
        let at = self.call_time(call.started_call_timestamp, item.date_sent);
        let now = self.now;
        // Who rang or started it, when that is a person and not us.
        let mut person_of = |id: Option<u64>, out: &mut Vec<Staged>| match id
            .and_then(|id| self.recipients.get(&id))
        {
            Some(Recipient::Contact(person)) => {
                let person = *person;
                self.person_row(person, out);
                Some(self.people[person].user.clone())
            }
            _ => None,
        };
        let (state, by) = match State::try_from(call.state) {
            Ok(State::UnknownState | State::Generic) => (
                CallState::Started,
                person_of(call.started_call_recipient_id, out),
            ),
            Ok(State::Joined | State::Accepted | State::OutgoingRing) => {
                (CallState::Answered, None)
            }
            Ok(State::Declined) => (CallState::Declined, None),
            Ok(State::Missed | State::MissedNotificationProfile) => {
                (CallState::Missed, person_of(call.ringer_recipient_id, out))
            }
            // It rang on the phone and never rings here. The core keeps a ring by the time
            // it has LEFT (`deadline - at`) and adds that to the time the line was received
            // (`CallLine::on_local_clock`), so a ring dated a minute after its start would
            // still ring for the rest of the minute when the call started shortly before the
            // import or this computer's clock is behind the phone's. With no time left the
            // core settles the line at once, as an unread missed call, silently.
            Ok(State::Ringing) => (
                CallState::Ringing {
                    deadline: at.min(now),
                },
                person_of(call.ringer_recipient_id, out),
            ),
            Err(_) => {
                self.count(reason::UNKNOWN_CALL);
                return None;
            }
        };
        let mut line = CallLine::signal(CallDirection::Incoming, true, state, at);
        if let Some(by) = by {
            line = line.with_by(by, at);
        }
        let read = match line.state {
            CallState::Ringing { .. } => false,
            _ => !line.counts_unread() || call.read,
        };
        let call_id = self.call_id(call.call_id);
        let mut msg = IncomingMessage::call(
            conv,
            &call_id,
            Origin::OwnOtherDevice { device: None },
            line,
            at,
            // Never after the import: nothing was received in the future.
            at.min(self.now),
        );
        msg.silent = true;
        msg.read = read;
        msg.expiry = self.call_expiry(item, false);
        Some(msg)
    }

    /// `body` with its ranges as a Kourier text: cut to `max` bytes on a character
    /// (counted under `cut_reason`), at most [`MapLimits::max_ranges`] ranges, converted
    /// from UTF-16 units by the live mapper's helper; a range that does not fit is dropped
    /// and counted. When the body was cut, a range that runs past the cut keeps what lies
    /// before it (its formatting of the kept text stands) and is dropped, counted, only
    /// when nothing of it is left.
    fn text(
        &mut self,
        body: String,
        ranges: &[backup::BodyRange],
        max: usize,
        cut_reason: &'static str,
    ) -> Text {
        use backup::body_range::{AssociatedValue, Style};
        let (body, was_cut) = cut(body, max);
        if was_cut {
            self.count(cut_reason);
        }
        // The length of the kept text in the archive's units, to shorten a range to.
        let kept_units = was_cut.then(|| body.encode_utf16().count() as u64);
        let ranges = if ranges.len() > self.limits.max_ranges {
            self.count(reason::RANGES_CUT);
            &ranges[..self.limits.max_ranges]
        } else {
            ranges
        };
        let wanted = ranges.iter().filter_map(|range| {
            let kind = match range.associated_value.as_ref()? {
                AssociatedValue::Style(style) => match Style::try_from(*style).ok()? {
                    Style::Bold => RangeKind::Bold,
                    Style::Italic => RangeKind::Italic,
                    Style::Spoiler => RangeKind::Spoiler,
                    Style::Strikethrough => RangeKind::Strike,
                    Style::Monospace => RangeKind::Mono,
                    Style::None => return None,
                },
                AssociatedValue::MentionAci(aci) => dispatch::mention(Some(aci), None)?,
            };
            let Some(units) = kept_units else {
                return Some((range.start, range.length, kind));
            };
            // Nothing of it before the cut: dropped. Else it ends at the cut at the latest.
            let start = u64::from(range.start);
            let end = (start + u64::from(range.length)).min(units);
            if end <= start {
                return None;
            }
            Some((range.start, u32::try_from(end - start).ok()?, kind))
        });
        let text = dispatch::ranged_text(body, wanted);
        for _ in text.ranges.len()..ranges.len() {
            self.count(reason::RANGE_DROPPED);
        }
        text
    }

    /// A quote: the quoted message (`local` "0", which names nothing, when the phone did
    /// not find it), its author as far as this chat knows them, and the quote's own copy.
    fn quote(&mut self, conv: usize, quote: &backup::Quote) -> ReplyTo {
        let kind = self.convs[conv].kind;
        // A 1:1 chat names us and its contact (anybody else is not in the stage); a group
        // anybody of the archive.
        let (author, quoted) = match self.recipients.get(&quote.author_id) {
            Some(Recipient::Own) => (TargetAuthor::Own, None),
            Some(Recipient::Contact(person)) if kind.admits(*person) => {
                let user = self.people[*person].user.clone();
                (TargetAuthor::User(user.clone()), Some(user))
            }
            _ => (TargetAuthor::Unknown, None),
        };
        let local = quote
            .target_sent_timestamp
            .filter(|at| *at != 0)
            .map_or_else(|| "0".to_owned(), |at| at.to_string());
        let text = quote
            .text
            .as_ref()
            .filter(|text| !text.body.is_empty())
            .map(|text| {
                self.text(
                    text.body.clone(),
                    &text.body_ranges,
                    self.limits.max_quote_bytes,
                    reason::QUOTE_TEXT_CUT,
                )
            });
        // The first quoted attachment as the quote names it; no thumbnail.
        let file = quote.attachments.first();
        let mime = file.and_then(|f| media::kept_mime(f.content_type.as_deref()));
        ReplyTo {
            target: MsgTarget {
                conv: Some(self.convs[conv].key.clone()),
                author,
                local: local.into(),
                alternates: Vec::new(),
            },
            author: quoted,
            snapshot: Some(QuoteSnapshot {
                text,
                attachment: file.map(|_| media::kind_of(mime.as_deref().unwrap_or(""), 0)),
                file_name: file
                    .and_then(|f| f.file_name.clone())
                    .filter(|n| !n.is_empty() && n.len() <= media::MAX_FILE_NAME),
                mime,
                thumb: None,
            }),
        }
    }

    /// The reactions of a message: one per author (the highest `sortOrder` wins), in the
    /// order of `sortOrder`, ours with `by: None`; a reaction by anybody else than us and the
    /// chat's contact, or that is not one emoji, is dropped (the live rule); at most
    /// [`MapLimits::max_reactions`], the newest. `sent_at` dates one without a time.
    fn reactions(
        &mut self,
        conv: usize,
        reactions: &[backup::Reaction],
        sent_at: i64,
    ) -> Vec<HistoryReaction> {
        let kind = self.convs[conv].kind;
        // By author (None: us): the sort order and the place of the one that wins.
        let mut best: HashMap<Option<usize>, (u64, usize)> = HashMap::new();
        for (place, reaction) in reactions.iter().enumerate() {
            let by = match self.recipients.get(&reaction.author_id) {
                Some(Recipient::Own) => None,
                Some(Recipient::Contact(person)) if kind.admits(*person) => Some(*person),
                _ => {
                    self.count(reason::REACTION_UNKNOWN_AUTHOR);
                    continue;
                }
            };
            if !crate::emoji::is_one(&reaction.emoji) {
                self.count(reason::REACTION_NOT_ONE_EMOJI);
                continue;
            }
            let entry = best.entry(by).or_insert((reaction.sort_order, place));
            if reaction.sort_order >= entry.0 {
                *entry = (reaction.sort_order, place);
            }
        }
        let mut kept: Vec<(u64, usize, Option<usize>)> = best
            .into_iter()
            .map(|(by, (order, place))| (order, place, by))
            .collect();
        kept.sort_unstable();
        if kept.len() > self.limits.max_reactions {
            self.count(reason::REACTIONS_CUT);
            kept.drain(..kept.len() - self.limits.max_reactions);
        }
        kept.into_iter()
            .map(|(_, place, by)| {
                let reaction = &reactions[place];
                let at = match to_i64(reaction.sent_timestamp) {
                    0 => sent_at,
                    at if at > self.latest() => self.now,
                    at => at,
                };
                HistoryReaction {
                    by: by.map(|person| self.people[person].user.clone()),
                    emoji: reaction.emoji.clone(),
                    at,
                }
            })
            .collect()
    }

    /// The later versions of an edited item, oldest first: the revisions after the first
    /// (the original, whose time is the message's id) and the item itself (the newest),
    /// whatever order the archive lists them in. The contract wants the last to be the
    /// newest version, so they are sorted by time; the original's id is never among them,
    /// and versions at the same time are one, the newest of them (a version listed twice
    /// has one time, and so have the versions too far ahead, which stand at the latest
    /// time the mapper takes). At most [`MapLimits::max_edits`], the newest.
    fn edits(&mut self, item: &backup::ChatItem) -> Vec<HistoryEdit> {
        let Some(first) = item.revisions.first() else {
            return Vec::new();
        };
        let original = if first.date_sent == 0 {
            item.date_sent
        } else {
            first.date_sent
        };
        let mut sent: Vec<u64> = item.revisions[1..]
            .iter()
            .map(|revision| revision.date_sent)
            .chain([item.date_sent])
            .filter(|at| *at != 0 && *at != original)
            .collect();
        sent.sort_unstable();
        let latest = self.latest();
        let mut edits: Vec<HistoryEdit> = Vec::with_capacity(sent.len());
        for date_sent in sent {
            // A time more than a day ahead is the import's time, as every other such time of
            // the mapper; the id keeps the true value.
            let at = to_i64(date_sent);
            let edit = HistoryEdit {
                local: date_sent.to_string().into(),
                at: if at > latest { self.now } else { at },
            };
            // Sorted by sent time, so the versions at one time are next to each other: the
            // newer one stands.
            match edits.last_mut() {
                Some(last) if last.at == edit.at => *last = edit,
                _ => edits.push(edit),
            }
        }
        if edits.len() > self.limits.max_edits {
            self.count(reason::EDITS_CUT);
            edits.drain(..edits.len() - self.limits.max_edits);
        }
        edits
    }

    /// The item encoded, as a placeholder keeps it ([`RAW_CHAT_ITEM`]), when it is at most
    /// [`MapLimits::max_raw_bytes`]; else `None`, counted.
    fn raw(&mut self, item: &backup::ChatItem) -> Option<BackendBlob> {
        if item.encoded_len() > self.limits.max_raw_bytes {
            self.count(reason::RAW_TOO_LARGE);
            return None;
        }
        Some(BackendBlob::new(RAW_CHAT_ITEM, item.encode_to_vec()))
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

    /// One attachment of a message: its fields bounded as live, its kind from the archive's
    /// flag enum, and whether it can be fetched ([`Self::place`]). The `MessageAttachment`'s
    /// client UUID is its first name. `None`, counted, without any pointer.
    fn attachment(
        &mut self,
        attachment: &backup::MessageAttachment,
        sent: i64,
    ) -> Option<Attachment> {
        let Some(pointer) = attachment.pointer.as_ref() else {
            self.count(reason::ATTACHMENT_NOT_AVAILABLE);
            return None;
        };
        let mime = media::archive_mime(pointer.content_type.as_deref());
        let kind = media::archive_kind(&mime, attachment.flag);
        let mut out = media::file_pointer_attachment(pointer, kind, mime);
        if let Some(uuid) = attachment
            .client_uuid
            .as_deref()
            .and_then(media::net_id_uuid)
        {
            out.net_ids.insert(0, uuid);
        }
        (out.state, out.remote) = self.place(pointer, sent);
        Some(out)
    }

    /// The rest of a long text: a file the core fetches by itself and joins into the body
    /// while it has a pointer (`Attachment::long_text`); not available when the server is
    /// not thought to have it any more, and then the message keeps its inline 2 KiB.
    fn long_text(&mut self, pointer: &backup::FilePointer, sent: i64) -> Attachment {
        let mime = media::archive_mime(pointer.content_type.as_deref());
        let mut out = media::file_pointer_attachment(pointer, AttachmentKind::File, mime);
        out.long_text = true;
        (out.state, out.remote) = self.place(pointer, sent);
        out
    }

    /// The image of a sticker, as the live path makes it: an image whose type is
    /// `media::sticker_mime` (`image/webp` when none is given, `application/octet-stream`
    /// when it is too long), downloadable by the rules of every attachment; `None` without
    /// a pointer at all. (A sticker's pack could give an old one; that is a later step.)
    fn sticker(&mut self, sticker: &backup::Sticker, sent: i64) -> Option<Attachment> {
        let data = sticker.data.as_ref()?;
        let mime = media::sticker_mime(data.content_type.as_deref());
        let mut out = media::file_pointer_attachment(data, AttachmentKind::Image, mime);
        (out.state, out.remote) = self.place(data, sent);
        Some(out)
    }

    /// Whether `pointer` can be fetched, and its `remote` if so: a valid place on the transit
    /// tier ([`Pointer::from_file_pointer`]), within the receive limit, and young by
    /// [`MediaRules`] (its upload time, else the message's original sent time `sent`).
    /// Anything else is not available on this device and keeps no pointer (no key is kept
    /// for an object that is not there).
    fn place(
        &mut self,
        pointer: &backup::FilePointer,
        sent: i64,
    ) -> (AttachmentState, Option<BackendBlob>) {
        let unavailable = (AttachmentState::Unavailable, None);
        let Ok(parsed) = Pointer::from_file_pointer(pointer) else {
            self.count(reason::ATTACHMENT_NOT_AVAILABLE);
            return unavailable;
        };
        if parsed.size > self.media.receive_limit {
            self.count(reason::ATTACHMENT_TOO_LARGE);
            return unavailable;
        }
        let uploaded = pointer
            .locator_info
            .as_ref()
            .and_then(|locator| locator.transit_tier_upload_timestamp)
            .filter(|at| *at != 0)
            .map_or(sent, to_i64);
        if self.media.is_old(uploaded) {
            self.count(reason::ATTACHMENT_TOO_OLD);
            return unavailable;
        }
        (AttachmentState::Remote, Some(archive_pointer_blob(pointer)))
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

    /// The state of an own message, by the core's own rule for the aggregate over recipients
    /// (the minimum; a failure beside a success is a partial failure).
    ///
    /// In a group the entries of every person of the archive count (one per member at send
    /// time, people who left since among them). Only the entries of a 1:1 chat's own contact
    /// count there: a 1:1 chat has one
    /// recipient, so an entry that names anybody else (us, a person of another chat, an id
    /// no frame described) says nothing about this message, and a failed one must not put
    /// a person into the row whom the stage does not hold for it. Entries that say
    /// "skipped" do not count either. With none left (an empty list, Note to Self) the
    /// message was sent.
    ///
    /// Who it failed for is a set: the row names a person once however often the list does,
    /// so the row does not grow with the list. A partial failure always names somebody.
    fn send_state(&self, kind: ConvKind, statuses: &[backup::SendStatus]) -> OutgoingState {
        use backup::send_status::DeliveryStatus;
        use backup::send_status::failed::FailureReason;
        let mut lowest: Option<u8> = None;
        let mut failed: BTreeSet<usize> = BTreeSet::new();
        let mut reason = None;
        for status in statuses {
            let person = match self.recipients.get(&status.recipient_id) {
                Some(Recipient::Contact(person)) if kind.admits(*person) => *person,
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

    /// The row of conversation `conv` in the stage's chats, staging it (and before it, its
    /// peer) when this is its first use.
    fn stage_conv(&mut self, conv: usize, out: &mut Vec<Staged>) -> u32 {
        if let Some(idx) = self.convs[conv].idx {
            return idx;
        }
        let kind = match self.convs[conv].kind {
            ConvKind::NoteToSelf => StagedKind::NoteToSelf,
            ConvKind::Direct(person) => StagedKind::Direct {
                peer: self.person_row(person, out),
            },
            ConvKind::Group(group) => {
                // Its members get names: the people of its lists with a contact frame.
                for i in 0..self.groups[group].people.len() {
                    let person = self.groups[group].people[i];
                    self.person_row(person, out);
                }
                StagedKind::Group(Box::new(self.groups[group].staged.clone()))
            }
        };
        let idx = self.staged_chats;
        self.staged_chats += 1;
        let chat = &mut self.convs[conv];
        chat.idx = Some(idx);
        chat.counters.idx = idx;
        out.push(Staged::Chat {
            idx,
            conv: chat.key.clone(),
            chat: StagedChat {
                kind,
                frame: chat.frame,
            },
            pinned: chat.pinned,
        });
        idx
    }

    /// Stages one row of conversation `conv`: first, when they are not staged yet, the chat
    /// (with its peer, or a group's people) and every person of the archive the row names
    /// (its author, a quote's author, reactions, a failed send state, a notice's people; a
    /// mention keeps its ACI and stages nobody, as live), then the row; and counts it for
    /// its chat.
    fn row(&mut self, conv: usize, n: u64, msg: IncomingMessage, out: &mut Vec<Staged>) {
        let idx = self.stage_conv(conv, out);
        for user in named_users(&msg) {
            if let Some(person) = self.person_of(user) {
                self.person_row(person, out);
            }
        }
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

    /// The person a user id names: by the id the contact frame leads with, or by the PNI of a
    /// person who has an ACI too.
    fn person_of(&self, user: &UserId) -> Option<usize> {
        self.by_user
            .get(user)
            .or_else(|| self.by_pni.get(user))
            .copied()
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

/// Adds `event` to the notices of one group-change item, folded as the live path folds one
/// change: members added by one actor are one `MembersAdded`, members removed by one actor
/// one `MembersRemoved` (a person once), at the place of the first. A join of a member's own
/// (`by` is the new member) is never folded with additions by that member.
fn fold(notices: &mut Vec<SystemEvent>, event: SystemEvent) {
    let self_join =
        |by: &Option<UserId>, users: &[UserId]| by.as_ref().is_some_and(|by| users.contains(by));
    let into = match &event {
        SystemEvent::MembersAdded { by, users } if !self_join(by, users) => {
            notices.iter_mut().find_map(|n| match n {
                SystemEvent::MembersAdded { by: b, users: u } if b == by && !self_join(b, u) => {
                    Some(u)
                }
                _ => None,
            })
        }
        SystemEvent::MembersRemoved { by, .. } => notices.iter_mut().find_map(|n| match n {
            SystemEvent::MembersRemoved { by: b, users: u } if b == by => Some(u),
            _ => None,
        }),
        _ => None,
    };
    let (
        Some(into),
        SystemEvent::MembersAdded { users, .. } | SystemEvent::MembersRemoved { users, .. },
    ) = (into, &event)
    else {
        notices.push(event);
        return;
    };
    for user in users {
        if !into.contains(user) {
            into.push(user.clone());
        }
    }
}

/// Who caused a group notice: its `by`, or the member who left; `None` for us (`own`), also
/// when the member who left is us, so that our own leave is a notice of our own device.
fn notice_actor<'a>(event: &'a SystemEvent, own: &UserId) -> Option<&'a UserId> {
    match event {
        SystemEvent::MembersAdded { by, .. }
        | SystemEvent::MembersRemoved { by, .. }
        | SystemEvent::TitleChanged { by, .. }
        | SystemEvent::AvatarChanged { by }
        | SystemEvent::DescriptionChanged { by }
        | SystemEvent::TimerChanged { by, .. }
        | SystemEvent::GroupCreated { by } => by.as_ref(),
        SystemEvent::MemberLeft { user } if user != own => Some(user),
        _ => None,
    }
}

/// The people a row names who should have a name where it is shown: its author, the author
/// of its quote, its reactions, the people its send state failed for and the people of a
/// notice. (Mentions keep their ACI without a name, as on the live path.)
fn named_users(msg: &IncomingMessage) -> Vec<&UserId> {
    let mut users = Vec::new();
    if let Origin::Remote { author, .. } = &msg.origin {
        users.push(author);
    }
    if let Some(author) = msg.reply_to.as_ref().and_then(|r| r.author.as_ref()) {
        users.push(author);
    }
    users.extend(msg.reactions.iter().filter_map(|r| r.by.as_ref()));
    if let Some(OutgoingState::PartiallyFailed { failed }) = &msg.initial_out_state {
        users.extend(failed.iter());
    }
    if let Body::System(event) = &msg.body {
        match event {
            SystemEvent::MembersAdded { by, users: u }
            | SystemEvent::MembersRemoved { by, users: u } => {
                users.extend(by.iter());
                users.extend(u.iter());
            }
            SystemEvent::MemberLeft { user }
            | SystemEvent::IdentityKeyChanged { user }
            | SystemEvent::SessionRefreshed { user }
            | SystemEvent::NumberChanged { user } => users.push(user),
            SystemEvent::TitleChanged { by, .. }
            | SystemEvent::AvatarChanged { by }
            | SystemEvent::DescriptionChanged { by }
            | SystemEvent::TimerChanged { by, .. }
            | SystemEvent::GroupCreated { by } => users.extend(by.iter()),
            _ => {}
        }
    }
    users
}

/// The places a frame has for its rows in the stage: a row's `n` is its frame's running
/// number times this, plus its place among the rows of that frame (a group change makes one
/// row per notice, at most [`MapLimits::max_group_notices`]). So rows sort in frame order and
/// every row of a chat has a key of its own.
pub(crate) const ROWS_PER_FRAME: u64 = 128;

/// The `n` of row `k` of frame `frame` ([`ROWS_PER_FRAME`]).
fn row_n(frame: u64, k: u64) -> u64 {
    frame
        .saturating_mul(ROWS_PER_FRAME)
        .saturating_add(k.min(ROWS_PER_FRAME - 1))
}

/// What a chat frame says of its conversation (`stage::ChatFrame`): the flags as they are, the
/// mute through the one conversion the Storage Service sync uses (on the `u64`, before any
/// cast: `i64::MAX` and more is for ever, 0 is none), the timer from milliseconds to seconds
/// (at most `i32::MAX`; less than a second is none) and its version (at least 1, also
/// without a timer).
fn chat_frame(chat: &backup::Chat) -> ChatFrame {
    ChatFrame {
        archived: chat.archived,
        marked_unread: chat.marked_unread,
        muted_until: chat.mute_until_ms.and_then(crate::storage::muted_until),
        timer_secs: timer(chat.expiration_timer_ms)
            .and_then(|timer| u32::try_from(timer.as_secs()).ok())
            .filter(|secs| *secs > 0),
        timer_version: chat.expire_timer_version.max(1),
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
