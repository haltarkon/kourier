//! Timeline rows: message bubbles with content blocks, date headers, the unread marker and
//! system rows, with the grouping flags the delegates need (§8.3, §8.4).
//!
//! Rows are newest first (row 0 = newest), the order of a `ListView` with
//! `verticalLayoutDirection: ListView.BottomToTop`.

use std::collections::HashMap;

use kourier_model::{
    AskFailure, Attachment, AttachmentId, AttachmentKind, AttachmentState, Body, CannotSend,
    Message, MessageId, OutgoingState, SendPhase, UserId,
};

use crate::i18n::{trc, trfc, trnc, trnfc};
use crate::media::{self, MediaVariant};
use crate::names::{Names, single_line};
use crate::preview::{self, NoticeTone, Summary};
use crate::readmore::ReadMore;
use crate::richtext::RichText;
use crate::time::{self, TimeContext};

/// Messages by the same author closer than this are one run (no repeated author line).
pub const GROUP_WINDOW_MS: i64 = 5 * 60 * 1000;

/// Kind of a timeline row (the `DelegateChooser` role).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowKind {
    /// A message bubble.
    Message,
    /// A date separator.
    DateHeader,
    /// "N unread messages".
    UnreadMarker,
    /// A centered notice (member changes, safety numbers, missed calls).
    System,
    /// Older history is loading.
    Loading,
}

impl RowKind {
    /// Stable name for QML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::DateHeader => "date",
            Self::UnreadMarker => "unread",
            Self::System => "system",
            Self::Loading => "loading",
        }
    }
}

/// Identity of a row across rebuilds (the diff key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RowKey {
    /// A message (also system messages).
    Message(MessageId),
    /// The date header of a local day number.
    Date(i64),
    /// The unread marker.
    Unread,
    /// The loading row.
    Loading,
}

/// Delivery state icon of an own message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Receipt {
    /// Not an own message (or nothing to show).
    #[default]
    None,
    /// Queued or being sent.
    Pending,
    /// Accepted by the server.
    Sent,
    /// Delivered to every recipient.
    Delivered,
    /// Read (or viewed) by every recipient.
    Read,
    /// Failed (for some or all recipients).
    Failed,
}

impl Receipt {
    /// Maps the aggregated outgoing state.
    pub fn from_state(state: Option<&OutgoingState>) -> Self {
        match state {
            None => Self::Sent,
            Some(OutgoingState::Queued | OutgoingState::Sending { .. }) => Self::Pending,
            Some(OutgoingState::Sent { .. }) => Self::Sent,
            Some(OutgoingState::Delivered) => Self::Delivered,
            Some(OutgoingState::Read | OutgoingState::Viewed) => Self::Read,
            Some(OutgoingState::Failed { .. } | OutgoingState::PartiallyFailed { .. }) => {
                Self::Failed
            }
        }
    }

    /// Stable name for QML (`""`, `pending`, `sent`, `delivered`, `read`, `failed`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Pending => "pending",
            Self::Sent => "sent",
            Self::Delivered => "delivered",
            Self::Read => "read",
            Self::Failed => "failed",
        }
    }
}

/// Download state of a media or file block.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum MediaState {
    /// Not downloaded; tap to download.
    #[default]
    Remote,
    /// Downloading.
    Downloading,
    /// Available.
    Local,
    /// Download failed.
    Failed,
    /// No longer available (the network said so).
    Expired,
    /// Not available on this device (known without asking).
    Unavailable,
    /// The user's phone is being asked to upload it again ("Asking your phone…").
    Asking,
    /// The phone did not answer in time; a click asks again.
    NoAnswer,
    /// The phone no longer has the message.
    NoMessage,
    /// The phone does not have this file either.
    NoFile,
}

impl MediaState {
    fn from_attachment(s: AttachmentState) -> (Self, f32) {
        match s {
            AttachmentState::Remote => (Self::Remote, 0.0),
            AttachmentState::Downloading { done, total } => {
                let f = if total > 0 {
                    (done as f64 / total as f64).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                (Self::Downloading, f)
            }
            AttachmentState::Local => (Self::Local, 1.0),
            AttachmentState::Failed { .. } => (Self::Failed, 0.0),
            AttachmentState::Expired => (Self::Expired, 0.0),
            AttachmentState::Unavailable => (Self::Unavailable, 0.0),
            AttachmentState::Asking => (Self::Asking, 0.0),
            AttachmentState::AskFailed { why } => (
                match why {
                    AskFailure::NoAnswer => Self::NoAnswer,
                    AskFailure::NoMessage => Self::NoMessage,
                    AskFailure::NoFile => Self::NoFile,
                },
                0.0,
            ),
        }
    }

    /// Stable name for QML.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Downloading => "downloading",
            Self::Local => "local",
            Self::Failed => "failed",
            Self::Expired => "expired",
            Self::Unavailable => "unavailable",
            Self::Asking => "asking",
            Self::NoAnswer => "noanswer",
            Self::NoMessage => "nomessage",
            Self::NoFile => "nofile",
        }
    }
}

/// An image, GIF, video or sticker.
#[derive(Clone, Default, PartialEq)]
pub struct ImageBlock {
    /// The core's attachment (`None` in pure fixture data).
    pub attachment: Option<AttachmentId>,
    /// How it is shown: Image, Gif or Video (also a "GIF" that is a video clip,
    /// [`media::is_clip`]).
    pub kind: AttachmentKind,
    /// Thumbnail URL (`image://kourier/thumb/…`); empty until downloaded.
    pub url: String,
    /// Full-size URL (`image://kourier/att/…`, the viewer's still); empty until downloaded,
    /// for clips and for GIFs that play from their copy.
    pub full_url: String,
    /// Blurhash placeholder URL (`image://kourier/blurhash/…`); empty without a blurhash.
    pub placeholder_url: String,
    /// Shown animated from a tmpfs copy (GIFs, animated stickers; `OpenMedia`).
    pub animated: bool,
    /// A well-known picture or video type: handed to another application without a
    /// confirmation (other types ask first, [`media::is_media_mime`]).
    pub media: bool,
    /// Pixel width (0 = unknown).
    pub width: u32,
    /// Pixel height (0 = unknown).
    pub height: u32,
    /// Blurhash for the placeholder; may be empty.
    pub blurhash: String,
    /// Download state.
    pub state: MediaState,
    /// Download progress 0 … 1.
    pub progress: f32,
    /// Size or duration label ("1.2 MB", "0:42").
    pub label: String,
    /// Sticker (no bubble).
    pub sticker: bool,
    /// It is gone here, and a click asks the user's phone to upload it again: the core lists
    /// this attachment among the ones its Download asks for
    /// (`MessageActions::ask_attachments`); no rule of the viewmodel's.
    pub ask: bool,
}

crate::safe_debug!(ImageBlock {
    attachment,
    kind,
    animated,
    media,
    width,
    height,
    state,
    progress,
    sticker,
    ask
});

/// A file, audio file or voice note.
#[derive(Clone, Default, PartialEq)]
pub struct FileBlock {
    /// The core's attachment (`None` in pure fixture data).
    pub attachment: Option<AttachmentId>,
    /// What it is: a voice note, a sound file or any other file.
    pub kind: AttachmentKind,
    /// A picture, sound or video: opened without a confirmation (other types ask first).
    pub media: bool,
    /// File name ("Voice message" for voice notes).
    pub name: String,
    /// "2.4 MB · PDF".
    pub detail: String,
    /// Breeze icon name for the MIME type.
    pub icon: String,
    /// Download state.
    pub state: MediaState,
    /// Download progress 0 … 1.
    pub progress: f32,
    /// Voice note.
    pub voice: bool,
    /// It is gone here, and a click asks the user's phone to upload it again (as
    /// [`ImageBlock::ask`]).
    pub ask: bool,
}

crate::safe_debug!(FileBlock {
    attachment,
    kind,
    media,
    icon,
    state,
    progress,
    voice,
    ask
});

/// Whether the message a quote points at is (still) here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum QuoteState {
    /// The original is in this conversation: activating the quote jumps to it.
    #[default]
    Known,
    /// The original is not on this device (older than its history, expired, or made up).
    Missing,
    /// The original was deleted after it was quoted.
    Deleted,
}

impl QuoteState {
    /// Stable name for QML (`known`, `missing`, `deleted`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Known => "known",
            Self::Missing => "missing",
            Self::Deleted => "deleted",
        }
    }
}

/// A quoted message.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct QuoteBlock {
    /// Author name ("You" for own).
    pub author: String,
    /// One-line text: the quoted text, else what was quoted ("Photo", a file name), else why
    /// there is nothing to show.
    pub text: String,
    /// Icon of quoted media or of a missing or deleted original, empty for text.
    pub icon: String,
    /// The quoted message is our own.
    pub own: bool,
    /// The message to jump to; `None` when the original is not here.
    pub target: Option<MessageId>,
    /// Known, missing or deleted.
    pub state: QuoteState,
    /// Thumbnail of a quoted picture or video (`image://kourier/thumb/…`), else empty.
    pub thumb_url: String,
    /// [`Self::text`] is not quoted content but a note in its place ("Original message not
    /// found", "Deleted message"): shown as such.
    pub stand_in: bool,
}

crate::safe_debug!(QuoteBlock { own, target, state });

/// One content block of a message bubble.
#[derive(Clone, Debug, PartialEq)]
pub enum Block {
    /// Formatted text.
    Text(RichText),
    /// A quote.
    Quote(QuoteBlock),
    /// Visual media.
    Image(ImageBlock),
    /// Several pictures and videos of one message, shown as one grid ([`ALBUM_TILES`] tiles,
    /// the last one counting what is left); the viewer steps through all of them.
    Album(Vec<ImageBlock>),
    /// A file or voice note.
    File(FileBlock),
    /// A stand-in (deleted, undecryptable, unsupported, contact, location, …).
    Placeholder(Summary),
}

impl Block {
    /// Stable type name for QML (`text`, `quote`, `image`, `album`, `file`, `placeholder`).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Quote(_) => "quote",
            Self::Image(_) => "image",
            Self::Album(_) => "album",
            Self::File(_) => "file",
            Self::Placeholder(_) => "placeholder",
        }
    }

    /// The pictures and videos of the block (one for an image, all of an album).
    pub fn images(&self) -> &[ImageBlock] {
        match self {
            Self::Image(i) => std::slice::from_ref(i),
            Self::Album(items) => items,
            _ => &[],
        }
    }
}

/// How many tiles an album grid shows at most. With more pictures the last tile says how many
/// are not shown ("+N"); every one of them is in the viewer. This is a layout choice of the
/// bubble, not a limit on what a message may carry: a received message with 40 pictures shows
/// 5 tiles and all 40 in the viewer.
pub const ALBUM_TILES: usize = 5;

/// One emoji chip under a bubble.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct ReactionChip {
    /// The emoji.
    pub emoji: String,
    /// How many people reacted with it.
    pub count: u32,
    /// We are one of them.
    pub mine: bool,
    /// Who reacted with it, in the order they did ("You" for our account), for the
    /// "who reacted" list.
    pub names: Vec<String>,
    /// Which of [`Self::names`] is our own entry (it can be taken back from the list).
    pub own_at: Option<usize>,
}

crate::safe_debug!(ReactionChip { count, mine });

/// What the user may do with a message row right now: the context menu and the hover toolbar
/// show exactly these. The flags come from the core (`MessageActions`: each is the answer of
/// the check its command runs), so the UI decides none of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RowActions {
    /// A new message may quote it.
    pub reply: bool,
    /// A reaction may be added or removed.
    pub react: bool,
    /// Its text or caption may be edited.
    pub edit: bool,
    /// It has text to copy.
    pub copy: bool,
    /// It may be deleted on this account.
    pub delete_for_me: bool,
    /// It may be deleted for everyone.
    pub delete_for_everyone: bool,
    /// One of its attachments may be removed for this account (the message stays).
    pub delete_attachment: bool,
    /// Nobody has it yet (queued or failed): deleting it simply takes it back
    /// (`CancelMessage`), so the menu offers one entry and no "for everyone" choice.
    pub take_back: bool,
    /// Its send failed for some or all recipients: it can be sent again.
    pub retry: bool,
    /// Download on one of its gone attachments asks the user's phone to upload it again
    /// (the core's `MessageActions::ask_attachments` is not empty; the blocks carry it as `ask`).
    pub ask_attachments: bool,
}

impl RowActions {
    /// Whether the row offers anything (a context menu makes sense).
    pub fn any(&self) -> bool {
        self.reply
            || self.react
            || self.edit
            || self.copy
            || self.delete_for_me
            || self.delete_for_everyone
            || self.delete_attachment
            || self.retry
    }
}

/// A reaction on a message (input of [`build_timeline`]).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Reaction {
    /// Who reacted; `None` = our account.
    pub user: Option<UserId>,
    /// The emoji.
    pub emoji: String,
    /// When (ms).
    pub at: i64,
}

/// An own message on its way is explained under the bubble only once it has taken this long
/// since it was sent (link feedback L6, "after a few seconds"): normal sends stay clean.
pub const WAIT_SHOWN_AFTER_MS: i64 = 3_000;

/// Why an own message that is still on its way waits (link feedback L6): a hold names the
/// REAL one, in the terms of the banner of the account that says the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// Queued or being sent, nothing more known ("Sending… 0:42").
    Sending,
    /// The account is not connected (offline, connecting).
    Connection,
    /// The network asks for a check that a person is sending (banner "Verify…").
    Challenge,
    /// The conversation waits for the user to confirm a changed safety number.
    SafetyNumber,
    /// The computer's clock is off.
    Clock,
    /// The device is no longer linked.
    LoggedOut,
    /// The network restricted the account.
    Restricted,
    /// This version of Kourier is too old for the network.
    UpdateApp,
    /// Another session took over the connection.
    Reconnect,
    /// The attempt is at this step (the backend's ephemeral `SendPhase`).
    Phase(SendPhase),
}

impl WaitReason {
    /// The message waits for something other than its own send: no countdown to a next try
    /// means anything then.
    pub fn is_hold(self) -> bool {
        !matches!(self, Self::Sending | Self::Phase(_))
    }
}

/// What the line under an own message on its way is built from: the reason, the stored
/// `sent_at` the timer counts up from (it survives a restart) and the outbox's stored next
/// try (`next_attempt_at`) while it waits to retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendWait {
    /// Why it waits.
    pub reason: WaitReason,
    /// The message's `sent_at` (ms).
    pub since: i64,
    /// When the next attempt starts (ms), while it waits to retry after a failed attempt.
    pub retry_at: Option<i64>,
}

/// The line under an own message on its way, in the two labels the UI draws: the reason may be
/// cut short when the bubble is narrow, the timer never is (link feedback L6).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WaitLine {
    /// Why it waits ("Setting up encryption with 12 members"); empty for a countdown.
    pub reason: String,
    /// The time: "· 0:42" after a reason, "0:42" after "Sending…", "Next try in 0:30" alone.
    pub timer: String,
}

impl WaitLine {
    /// Nothing to show.
    pub fn is_empty(&self) -> bool {
        self.reason.is_empty() && self.timer.is_empty()
    }

    /// Both labels as one text, as a plain reader would say them.
    pub fn text(&self) -> String {
        match (self.reason.is_empty(), self.timer.is_empty()) {
            (_, true) => self.reason.clone(),
            (true, _) => self.timer.clone(),
            _ => format!("{} {}", self.reason, self.timer),
        }
    }
}

/// The line under an own message on its way at `now` (ms): "Sending… 0:42",
/// "Waiting for the connection · 1:05", "Setting up encryption with 12 members · 0:42",
/// "Sending to 20 members · 0:05", "Sent to 8 of 20 members · 0:42", "Next try in 0:30".
/// Empty before [`WAIT_SHOWN_AFTER_MS`].
pub fn wait_line(wait: &SendWait, now: i64) -> WaitLine {
    let elapsed = now.saturating_sub(wait.since);
    if elapsed < WAIT_SHOWN_AFTER_MS {
        return WaitLine::default();
    }
    let clock = |ms: i64| time::duration(u32::try_from(ms.max(0)).unwrap_or(u32::MAX));
    // While the message waits for something else the countdown means nothing: the try waits
    // for that (a connection, the user).
    if !wait.reason.is_hold()
        && let Some(at) = wait.retry_at.filter(|at| *at > now)
    {
        // Rounded up: "0:01" until the instant, never "0:00" before it.
        let left = (at - now).saturating_add(999) / 1000 * 1000;
        return WaitLine {
            reason: String::new(),
            timer: trfc(
                "@info:status under a message not sent yet, %1 time left (m:ss)",
                "Next try in %1",
                &[&clock(left)],
            ),
        };
    }
    let since = clock(elapsed);
    let why = match wait.reason {
        WaitReason::Sending | WaitReason::Phase(SendPhase::Sending { of: 0..=1, .. }) => {
            return WaitLine {
                reason: trc("@info:status under a message being sent", "Sending…").into_owned(),
                timer: since,
            };
        }
        WaitReason::Connection => trc(
            "@info:status under a message not sent yet",
            "Waiting for the connection",
        )
        .into_owned(),
        // The words of the banners above the chat ("Verify…", the safety number notice, the
        // clock, "no longer linked", "restricted").
        WaitReason::Challenge => trc(
            "@info:status under a message not sent yet",
            "Waiting for you to verify",
        )
        .into_owned(),
        WaitReason::SafetyNumber => trc(
            "@info:status under a message not sent yet",
            "Waiting for you to confirm the new safety number",
        )
        .into_owned(),
        WaitReason::Clock => trc(
            "@info:status under a message not sent yet",
            "Waiting for the computer's clock to be set",
        )
        .into_owned(),
        WaitReason::LoggedOut => trc(
            "@info:status under a message not sent yet",
            "Waiting for this device to be linked again",
        )
        .into_owned(),
        WaitReason::Restricted => trc(
            "@info:status under a message not sent yet",
            "Waiting: the network restricted this account",
        )
        .into_owned(),
        WaitReason::UpdateApp => trc(
            "@info:status under a message not sent yet",
            "Waiting for Kourier to be updated",
        )
        .into_owned(),
        WaitReason::Reconnect => trc(
            "@info:status under a message not sent yet",
            "Waiting for you to reconnect",
        )
        .into_owned(),
        WaitReason::Phase(SendPhase::Uploading) => {
            trc("@info:status under a message being sent", "Uploading files").into_owned()
        }
        WaitReason::Phase(SendPhase::RefreshingGroup) => trc(
            "@info:status under a message being sent",
            "Checking the group",
        )
        .into_owned(),
        WaitReason::Phase(SendPhase::SettingUpKeys { of, .. }) => trnc(
            "@info:status under a message being sent, %1 how many members",
            "Setting up encryption with %1 member",
            "Setting up encryption with %1 members",
            u64::from(of),
        ),
        WaitReason::Phase(SendPhase::RepairingDevices) => trc(
            "@info:status under a message being sent",
            "Updating the members' devices",
        )
        .into_owned(),
        // Nothing has reached anybody yet (the one request to the network is out): "Sent to 0
        // of 20 members" would read like a failure.
        WaitReason::Phase(SendPhase::Sending { done: 0, of }) => trnc(
            "@info:status under a message being sent, %1 how many members",
            "Sending to %1 member",
            "Sending to %1 members",
            u64::from(of),
        ),
        // The count of a plural is `%1`, which KI18n fills in itself: so `%1` is all members
        // and `%2` those it went to (with `%1` for these the KDE translator said "Sent to 20
        // of 20 members").
        WaitReason::Phase(SendPhase::Sending { done, of }) => {
            let count = |n: u32| crate::i18n::number(u64::from(n)).unwrap_or_else(|| n.to_string());
            trnfc(
                "@info:status under a message being sent, %1 all members, %2 members it went to",
                "Sent to %2 of %1 member",
                "Sent to %2 of %1 members",
                u64::from(of),
                &[&count(of), &count(done)],
            )
        }
        WaitReason::Phase(SendPhase::SyncingOwnDevices) => trc(
            "@info:status under a message being sent",
            "Sending to your other devices",
        )
        .into_owned(),
    };
    WaitLine {
        reason: why,
        timer: trfc(
            "@info:status after the reason under a message on its way, %1 time since it was sent (m:ss)",
            "· %1",
            &[&since],
        ),
    }
}

/// The line of a row: only an own message that is still pending has one.
pub(crate) fn row_wait_line(receipt: Receipt, wait: Option<SendWait>, now: i64) -> WaitLine {
    match wait {
        Some(w) if receipt == Receipt::Pending => wait_line(&w, now),
        _ => WaitLine::default(),
    }
}

crate::safe_debug!(Reaction { user, at });

/// One timeline row.
#[derive(Clone, PartialEq)]
pub struct MessageRow {
    /// Diff identity.
    pub key: RowKey,
    /// Delegate kind.
    pub kind: RowKind,
    /// Sent by our account.
    pub own: bool,
    /// Author name ("You" for own messages).
    pub author: String,
    /// Author id (`None` for own messages and markers); the grouping key.
    pub author_id: Option<UserId>,
    /// Author avatar URL, empty for initials.
    pub avatar_url: String,
    /// Show the author line (groups, first message of a run).
    pub show_author: bool,
    /// Show the author avatar (groups, last message of a run).
    pub show_avatar: bool,
    /// Oldest message of a run (rounded top corners).
    pub group_start: bool,
    /// Newest message of a run (rounded bottom corners, extra spacing below).
    pub group_end: bool,
    /// `HH:MM`.
    pub time_label: String,
    /// Sent timestamp (ms).
    pub sent_at: i64,
    /// Content blocks.
    pub blocks: Vec<Block>,
    /// Reaction chips.
    pub reactions: Vec<ReactionChip>,
    /// Edited at least once.
    pub edited: bool,
    /// A tombstone.
    pub deleted: bool,
    /// Delivery state of own messages.
    pub receipt: Receipt,
    /// Text of date headers, unread markers and system rows.
    pub text: String,
    /// Icon of system rows.
    pub icon: String,
    /// How a system row looks beyond its text and icon (a ringing or a missed call).
    pub tone: NoticeTone,
    /// What the user may do with the message now.
    pub actions: RowActions,
    /// The message disappears: what the timer mark says ("Disappears 1 week after it is
    /// read"); empty for messages that stay.
    pub expiry: String,
    /// The reason in the line under an own message on its way ([`wait_line`]); empty for
    /// every other row, before [`WAIT_SHOWN_AFTER_MS`] and for a countdown.
    pub wait: String,
    /// The timer of that line: never cut short, it has a label of its own.
    pub wait_timer: String,
}

crate::safe_debug!(MessageRow {
    key,
    kind,
    own,
    show_author,
    show_avatar,
    group_start,
    group_end,
    sent_at,
    blocks,
    reactions,
    edited,
    deleted,
    receipt,
    actions,
});

impl MessageRow {
    fn marker(key: RowKey, kind: RowKind, text: String) -> Self {
        Self {
            key,
            kind,
            own: false,
            author: String::new(),
            author_id: None,
            avatar_url: String::new(),
            show_author: false,
            show_avatar: false,
            group_start: true,
            group_end: true,
            time_label: String::new(),
            sent_at: 0,
            blocks: Vec::new(),
            reactions: Vec::new(),
            edited: false,
            deleted: false,
            receipt: Receipt::None,
            text,
            icon: String::new(),
            actions: RowActions::default(),
            tone: NoticeTone::Plain,
            expiry: String::new(),
            wait: String::new(),
            wait_timer: String::new(),
        }
    }

    /// The message id of message and system rows.
    pub fn message_id(&self) -> Option<MessageId> {
        match self.key {
            RowKey::Message(id) => Some(id),
            _ => None,
        }
    }

    /// The kinds of the attachments the row shows, in order, by their stable names (`image`,
    /// `gif`, `video`, `sticker`, `voice`, `audio`, `file`): every picture of an album counts.
    pub fn attachment_kinds(&self) -> Vec<&'static str> {
        let mut kinds = Vec::new();
        for block in &self.blocks {
            match block {
                Block::Image(_) | Block::Album(_) => {
                    kinds.extend(block.images().iter().map(|i| {
                        if i.sticker {
                            "sticker"
                        } else {
                            media::kind_name(i.kind)
                        }
                    }));
                }
                Block::File(f) => kinds.push(media::kind_name(f.kind)),
                Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => {}
            }
        }
        kinds
    }

    /// The quote the row shows, when it is a reply.
    pub fn quote(&self) -> Option<&QuoteBlock> {
        self.blocks.iter().find_map(|b| match b {
            Block::Quote(q) => Some(q),
            _ => None,
        })
    }

    /// Whether the row shows a text (a message text or a caption).
    pub fn has_text(&self) -> bool {
        self.blocks.iter().any(|b| matches!(b, Block::Text(_)))
    }
}

/// What [`build_timeline`] takes.
pub struct TimelineInput<'a> {
    /// Loaded messages, newest first.
    pub messages: &'a [Message],
    /// Reactions per message.
    pub reactions: &'a HashMap<MessageId, Vec<Reaction>>,
    /// Group-like conversation (author lines and avatars).
    pub is_group: bool,
    /// The oldest unread message: the unread marker goes right above it.
    pub unread_from: Option<MessageId>,
    /// Older history exists beyond the loaded window (adds a loading row on top).
    pub has_older: bool,
}

/// The oldest incoming unread message in `messages` (newest first), for
/// [`TimelineInput::unread_from`].
pub fn first_unread(messages: &[Message]) -> Option<MessageId> {
    messages
        .iter()
        .take_while(|m| {
            m.origin.is_own()
                || m.read_state
                    .is_none_or(|r| r == kourier_model::ReadState::Unread)
        })
        .filter(|m| {
            !m.origin.is_own()
                && (!matches!(m.body, Body::System(_)) || m.body.call_line().is_some())
                && m.read_state == Some(kourier_model::ReadState::Unread)
        })
        .last()
        .map(|m| m.id)
}

/// How many pictures, videos and files of `blocks` are downloading.
pub fn blocks_downloading(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            Block::Image(image) => usize::from(image.state == MediaState::Downloading),
            Block::Album(tiles) => tiles
                .iter()
                .filter(|t| t.state == MediaState::Downloading)
                .count(),
            Block::File(file) => usize::from(file.state == MediaState::Downloading),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => 0,
        })
        .sum()
}

/// How many pictures, videos and files of the `rows` shown are downloading (a `--screenshot`
/// waits for none: their state, and with it their URLs, changes when the download ends).
pub fn downloading(rows: &[MessageRow]) -> usize {
    rows.iter().map(|r| blocks_downloading(&r.blocks)).sum()
}

/// Aggregates reactions into chips, most frequent first (ties: first reacted first).
pub fn reaction_chips(reactions: &[Reaction], names: &dyn Names) -> Vec<ReactionChip> {
    let mut sorted: Vec<&Reaction> = reactions.iter().collect();
    sorted.sort_by_key(|r| r.at);
    let mut chips: Vec<ReactionChip> = Vec::new();
    for r in sorted {
        let name = reactor_name(r.user.as_ref(), names);
        match chips.iter_mut().find(|c| c.emoji == r.emoji) {
            Some(c) => {
                c.count += 1;
                c.mine |= r.user.is_none();
                if r.user.is_none() {
                    c.own_at = Some(c.names.len());
                }
                c.names.push(name);
            }
            None => chips.push(ReactionChip {
                emoji: r.emoji.clone(),
                count: 1,
                mine: r.user.is_none(),
                names: vec![name],
                own_at: r.user.is_none().then_some(0),
            }),
        }
    }
    // Stable sort keeps first-reacted order among equal counts.
    chips.sort_by_key(|c| std::cmp::Reverse(c.count));
    chips
}

/// The name of someone who reacted ("You" for our account).
pub(crate) fn reactor_name(user: Option<&UserId>, names: &dyn Names) -> String {
    match user {
        Some(u) => names.display_name(u),
        None => trc("@item:intext the author of a message, yourself", "You").into_owned(),
    }
}

/// Breeze icon name for a MIME type.
pub fn mime_icon(mime: &str) -> &'static str {
    let m = mime.to_ascii_lowercase();
    match m.as_str() {
        "application/pdf" => "application-pdf",
        "application/zip"
        | "application/x-7z-compressed"
        | "application/x-tar"
        | "application/gzip" => "application-x-archive",
        "text/plain" => "text-plain",
        _ if m.starts_with("image/") => "image-x-generic",
        _ if m.starts_with("audio/") => "audio-x-generic",
        _ if m.starts_with("video/") => "video-x-generic",
        _ if m.starts_with("text/") => "text-x-generic",
        _ if m.contains("spreadsheet") || m.contains("excel") => "x-office-spreadsheet",
        _ if m.contains("presentation") || m.contains("powerpoint") => "x-office-presentation",
        _ if m.contains("document") || m.contains("msword") => "x-office-document",
        _ => "application-octet-stream",
    }
}

/// Short type label from a file name or MIME type ("PDF", "ZIP").
fn type_label(att: &Attachment) -> String {
    let ext = att
        .file_name
        .as_deref()
        .and_then(|n| n.rsplit_once('.'))
        .map(|(_, e)| e)
        .filter(|e| !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()));
    match ext {
        Some(e) => e.to_ascii_uppercase(),
        None => att
            .mime
            .split('/')
            .nth(1)
            .map(|s| s.to_ascii_uppercase())
            .unwrap_or_default(),
    }
}

fn attachment_block(att: &Attachment, sticker: bool) -> Block {
    let (state, progress) = MediaState::from_attachment(att.state);
    let media_mime = media::is_media_mime(&att.mime);
    match att.kind {
        AttachmentKind::Image | AttachmentKind::Gif | AttachmentKind::Video => {
            let (width, height) = att.dims.unwrap_or((0, 0));
            let label = match (att.kind, att.duration_ms) {
                (AttachmentKind::Video, Some(d)) => time::duration(d),
                (AttachmentKind::Gif, _) => {
                    trc("@info:preview attachment type", "GIF").into_owned()
                }
                _ => time::file_size(att.size),
            };
            let rev = media::revision(att);
            let url = |variant| {
                att.id
                    .filter(|_| state == MediaState::Local)
                    .map(|id| media::media_url(variant, id, rev))
                    .unwrap_or_default()
            };
            let blurhash = att.blurhash.clone().unwrap_or_default();
            // Videos, and "GIFs" that are videos, are clips for another application.
            let clip = media::is_clip(att);
            let animated = media::is_animated(att, sticker);
            Block::Image(ImageBlock {
                attachment: att.id,
                kind: if clip {
                    AttachmentKind::Video
                } else {
                    att.kind
                },
                // Clips have no decodable still yet: only their placeholder shows.
                url: if clip {
                    String::new()
                } else {
                    url(MediaVariant::Thumb)
                },
                // The viewer's still; a GIF that plays is shown from its copy instead.
                full_url: if clip || att.kind == AttachmentKind::Gif && animated {
                    String::new()
                } else {
                    url(MediaVariant::Full)
                },
                placeholder_url: match att.id {
                    Some(id) if !blurhash.is_empty() => {
                        media::media_url(MediaVariant::Blurhash, id, rev)
                    }
                    _ => String::new(),
                },
                animated,
                media: media_mime,
                width,
                height,
                blurhash,
                state,
                progress,
                label,
                sticker,
                ask: false,
            })
        }
        AttachmentKind::Voice => Block::File(FileBlock {
            attachment: att.id,
            kind: AttachmentKind::Voice,
            media: true,
            name: trc("@info:preview attachment type", "Voice message").into_owned(),
            detail: att.duration_ms.map(time::duration).unwrap_or_default(),
            icon: "media-record".into(),
            state,
            progress,
            voice: true,
            ask: false,
        }),
        AttachmentKind::Audio | AttachmentKind::File => {
            let name = att
                .file_name
                .as_deref()
                .map(single_line)
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| trc("@info:preview attachment type", "File").into_owned());
            let kind = type_label(att);
            let size = time::file_size(att.size);
            Block::File(FileBlock {
                attachment: att.id,
                kind: att.kind,
                media: media_mime,
                name,
                detail: if kind.is_empty() {
                    size
                } else {
                    format!("{size} · {kind}")
                },
                icon: mime_icon(&att.mime).into(),
                state,
                progress,
                voice: false,
                ask: false,
            })
        }
    }
}

/// Marks the blocks of the attachments whose Download asks the user's phone to upload them
/// again: the ones the core listed ([`Item::ask`]), copied as they are. Which attachments those
/// are is the core's rule (the click's own check, [`RowActions::ask_attachments`]), not the
/// viewmodel's.
fn mark_askable(blocks: &mut [Block], ask: &[AttachmentId]) {
    let asks = |id: Option<AttachmentId>| id.is_some_and(|id| ask.contains(&id));
    for block in blocks {
        match block {
            Block::Image(i) => i.ask = asks(i.attachment),
            Block::Album(items) => {
                for i in items {
                    i.ask = asks(i.attachment);
                }
            }
            Block::File(f) => f.ask = asks(f.attachment),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => {}
        }
    }
}

/// Applies live download progress (`Progress::Download` events, ahead of the next row query)
/// to the media blocks of `rows`.
pub fn apply_progress(rows: &mut [MessageRow], progress: &HashMap<AttachmentId, f32>) {
    if progress.is_empty() {
        return;
    }
    let apply = |id: Option<AttachmentId>, state: MediaState, p: &mut f32| {
        if state == MediaState::Downloading
            && let Some(live) = id.and_then(|id| progress.get(&id))
        {
            *p = p.max(*live);
        }
    };
    for block in rows.iter_mut().flat_map(|r| r.blocks.iter_mut()) {
        match block {
            Block::Image(i) => apply(i.attachment, i.state, &mut i.progress),
            Block::Album(items) => {
                for i in items {
                    apply(i.attachment, i.state, &mut i.progress);
                }
            }
            Block::File(f) => apply(f.attachment, f.state, &mut f.progress),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => {}
        }
    }
}

fn quote_block(
    msg: &Message,
    by_net: &HashMap<&kourier_model::NetMsgId, &Message>,
    window: &[Message],
    names: &dyn Names,
) -> Option<Block> {
    let reply = msg.reply_to.as_ref()?;
    let target = reply
        .target
        .net_id()
        .and_then(|id| by_net.get(&id).copied())
        .or_else(|| window.iter().find(|m| reply.target.matches(&m.net_id)));
    let author_of = |own: bool, user: Option<&UserId>| {
        if own {
            trc("@item:intext the author of a message, yourself", "You").into_owned()
        } else {
            user.map_or_else(
                || trc("@item:intext a person without a name", "Unknown").into_owned(),
                |u| names.display_name(u),
            )
        }
    };
    let block = match target {
        Some(t) => {
            let s = preview::message_summary(t, names);
            QuoteBlock {
                author: author_of(t.origin.is_own(), t.origin.author()),
                text: s.text,
                icon: s.icon,
                own: t.origin.is_own(),
                target: Some(t.id),
                state: if t.deleted.is_some() {
                    QuoteState::Deleted
                } else {
                    QuoteState::Known
                },
                thumb_url: String::new(),
                stand_in: t.deleted.is_some(),
            }
        }
        None => {
            let s = reply
                .snapshot
                .as_ref()
                .map_or_else(preview::missing_quote, |snap| {
                    preview::snapshot_summary(snap, names)
                });
            let stand_in = reply
                .snapshot
                .as_ref()
                .is_none_or(|snap| snap.text.is_none() && snap.attachment.is_none());
            let own = reply.author.is_none()
                && matches!(reply.target.author, kourier_model::TargetAuthor::Own);
            QuoteBlock {
                author: author_of(own, reply.author.as_ref()),
                text: s.text,
                icon: s.icon,
                own,
                target: None,
                state: QuoteState::Missing,
                thumb_url: String::new(),
                stand_in,
            }
        }
    };
    Some(Block::Quote(block))
}

fn content_blocks(
    id: MessageId,
    body: &Body,
    names: &dyn Names,
    read_more: &ReadMore,
) -> Vec<Block> {
    let mut blocks = Vec::new();
    match body {
        Body::Text(t) => blocks.push(Block::Text(read_more.render(id, t, names))),
        Body::Media {
            attachments,
            caption,
        } => {
            // The rest of a long text is no file of the message (it becomes the text once it
            // is downloaded).
            // Neighbouring pictures and videos form one album grid; a file between them
            // ends the grid, so everything stays in the order it was sent in.
            let mut run: Vec<ImageBlock> = Vec::new();
            let flush = |run: &mut Vec<ImageBlock>, blocks: &mut Vec<Block>| match run.len() {
                0 => {}
                1 => blocks.extend(run.pop().map(Block::Image)),
                _ => blocks.push(Block::Album(std::mem::take(run))),
            };
            for a in attachments.iter().filter(|a| !a.long_text) {
                match attachment_block(a, false) {
                    Block::Image(image) => run.push(image),
                    other => {
                        flush(&mut run, &mut blocks);
                        blocks.push(other);
                    }
                }
            }
            flush(&mut run, &mut blocks);
            let caption = caption
                .as_ref()
                .or_else(|| attachments.first().and_then(|a| a.caption.as_ref()));
            if let Some(c) = caption.filter(|c| !c.body.trim().is_empty()) {
                blocks.push(Block::Text(read_more.render(id, c, names)));
            }
        }
        Body::Sticker(a) => blocks.push(attachment_block(a, true)),
        Body::Placeholder(p) => blocks.push(Block::Placeholder(preview::placeholder_summary(*p))),
        other => blocks.push(Block::Placeholder(preview::body_summary(other, names))),
    }
    blocks
}

/// Whether `b` (newer) continues the run of `a` (older).
fn same_run(a: &MessageRow, b: &MessageRow, time: &TimeContext<'_>) -> bool {
    a.kind == RowKind::Message
        && b.kind == RowKind::Message
        && a.own == b.own
        && a.author_id == b.author_id
        && (b.sent_at - a.sent_at).abs() <= GROUP_WINDOW_MS
        && time.day_number(a.sent_at) == time.day_number(b.sent_at)
}

/// One message as the row builder sees it, whatever its source (model messages of the fixture,
/// query rows of the core).
pub(crate) struct Item<'a> {
    pub id: MessageId,
    pub sent_at: i64,
    pub own: bool,
    pub author: Option<&'a UserId>,
    pub body: &'a Body,
    pub deleted: Option<&'a kourier_model::Deletion>,
    pub edited: bool,
    pub out_state: Option<&'a OutgoingState>,
    /// The quote block (built by the caller, which knows how to resolve the target).
    pub quote: Option<Block>,
    /// Reaction chips.
    pub reactions: Vec<ReactionChip>,
    /// What the user may do with it (`retry` is derived here from the send state).
    pub actions: RowActions,
    /// The attachments whose Download asks the user's phone to upload them again (the core's
    /// per-attachment answer, copied to the blocks as `ask`).
    pub ask: &'a [AttachmentId],
    /// Its disappearing timer.
    pub expiry: Option<&'a kourier_model::Expiry>,
    /// What the line under it says while it is on its way (own messages; `None`: nothing
    /// known, e.g. the fixture).
    pub wait: Option<SendWait>,
}

/// The common part of every timeline: date headers, the unread marker, system rows, bubbles
/// with their grouping flags and the loading row. `items` are chronological (oldest first);
/// the result is newest first. Long texts are shown as far as `read_more` says.
pub(crate) fn build_rows<'a>(
    items: impl Iterator<Item = Item<'a>>,
    is_group: bool,
    unread_from: Option<MessageId>,
    has_older: bool,
    names: &dyn Names,
    time: &TimeContext<'_>,
    read_more: &ReadMore,
) -> Vec<MessageRow> {
    read_more.begin();
    // Chronological pass: date headers and the unread marker go before their message.
    let mut rows: Vec<MessageRow> = Vec::new();
    let mut last_day: Option<i64> = None;
    let mut unread_count: u64 = 0;
    let mut marker_at: Option<usize> = None;
    for msg in items {
        let day = time.day_number(msg.sent_at);
        if last_day != Some(day) {
            rows.push(MessageRow::marker(
                RowKey::Date(day),
                RowKind::DateHeader,
                time.date_header(day),
            ));
            last_day = Some(day);
        }
        if unread_from == Some(msg.id) {
            marker_at = Some(rows.len());
            rows.push(MessageRow::marker(
                RowKey::Unread,
                RowKind::UnreadMarker,
                String::new(),
            ));
        }
        let system = matches!(msg.body, Body::System(_));
        // The marker counts incoming messages, not system events (as `first_unread`), and of
        // those the call lines that count as unread: a missed call, a group call started.
        let unread_call = msg.body.call_line().is_some_and(|l| l.counts_unread());
        if marker_at.is_some() && ((!msg.own && !system) || unread_call) {
            unread_count += 1;
        }
        if let (Body::System(event), None) = (msg.body, msg.deleted) {
            let mut row = MessageRow::marker(
                RowKey::Message(msg.id),
                RowKind::System,
                preview::system_text(event, names),
            );
            row.icon = preview::system_icon(event).to_owned();
            row.tone = preview::system_tone(event);
            row.sent_at = msg.sent_at;
            row.time_label = time.clock(msg.sent_at);
            // A notice can only be removed from this account's view.
            row.actions = RowActions {
                delete_for_me: msg.actions.delete_for_me,
                ..RowActions::default()
            };
            rows.push(row);
            continue;
        }
        let author = if msg.own {
            trc("@item:intext the author of a message, yourself", "You").into_owned()
        } else {
            msg.author.map_or_else(
                || trc("@item:intext a person without a name", "Unknown").into_owned(),
                |u| names.display_name(u),
            )
        };
        let mut blocks = Vec::new();
        let deleted = msg.deleted.is_some();
        if let Some(d) = msg.deleted {
            blocks.push(Block::Placeholder(Summary {
                text: preview::deleted_text(d, msg.own),
                icon: "trash-empty".into(),
            }));
        } else {
            blocks.extend(msg.quote);
            blocks.extend(content_blocks(msg.id, msg.body, names, read_more));
            if !msg.ask.is_empty() {
                mark_askable(&mut blocks, msg.ask);
            }
        }
        let receipt = if msg.own && !deleted {
            Receipt::from_state(msg.out_state)
        } else {
            Receipt::None
        };
        let wait = row_wait_line(receipt, msg.wait, time.now_ms);
        rows.push(MessageRow {
            key: RowKey::Message(msg.id),
            kind: RowKind::Message,
            own: msg.own,
            author,
            author_id: msg.author.cloned(),
            avatar_url: String::new(),
            show_author: false,
            show_avatar: false,
            group_start: true,
            group_end: true,
            time_label: time.clock(msg.sent_at),
            sent_at: msg.sent_at,
            blocks,
            reactions: if deleted { Vec::new() } else { msg.reactions },
            edited: !deleted && msg.edited,
            deleted,
            receipt,
            text: String::new(),
            icon: String::new(),
            actions: RowActions {
                retry: receipt == Receipt::Failed,
                ..msg.actions
            },
            tone: NoticeTone::Plain,
            expiry: if deleted {
                String::new()
            } else {
                msg.expiry.map(preview::expiry_text).unwrap_or_default()
            },
            wait: wait.reason,
            wait_timer: wait.timer,
        });
    }
    read_more.end();
    if let Some(i) = marker_at {
        rows[i].text = trnc(
            "@info:status divider above the first unread message, %1 how many",
            "%1 unread message",
            "%1 unread messages",
            unread_count,
        );
    }

    // Grouping flags over chronological neighbours.
    for i in 0..rows.len() {
        if rows[i].kind != RowKind::Message {
            continue;
        }
        let joins_prev = i > 0 && same_run(&rows[i - 1], &rows[i], time);
        let joins_next = i + 1 < rows.len() && same_run(&rows[i], &rows[i + 1], time);
        let row = &mut rows[i];
        row.group_start = !joins_prev;
        row.group_end = !joins_next;
        row.show_author = is_group && !row.own && row.group_start;
        row.show_avatar = is_group && !row.own && row.group_end;
    }

    if has_older {
        rows.insert(
            0,
            MessageRow::marker(RowKey::Loading, RowKind::Loading, String::new()),
        );
    }
    rows.reverse();
    rows
}

/// What the pure fixture data offers for a message (no core checks its windows): reply, react
/// and copy for content, edit for own texts and captions, delete for everything.
fn model_actions(msg: &Message) -> RowActions {
    let own = msg.origin.is_own();
    let content =
        msg.deleted.is_none() && !matches!(msg.body, Body::System(_) | Body::Placeholder(_));
    let text = match &msg.body {
        Body::Text(_) => true,
        Body::Media {
            attachments,
            caption,
        } => caption
            .as_ref()
            .or_else(|| attachments.first().and_then(|a| a.caption.as_ref()))
            .is_some_and(|c| !c.body.trim().is_empty()),
        _ => false,
    };
    RowActions {
        reply: content,
        react: content,
        edit: content && own && matches!(msg.body, Body::Text(_) | Body::Media { .. }),
        copy: content && text,
        delete_for_me: true,
        delete_for_everyone: content && own,
        delete_attachment: content
            && matches!(&msg.body, Body::Media { attachments, .. } if attachments.iter().any(|a| !a.long_text)),
        take_back: false,
        retry: false,
        ask_attachments: false,
    }
}

/// The quote a reply to `row` shows in the composer's banner and what an edit of it starts
/// from, read off the built row.
pub fn quote_of_row(row: &MessageRow) -> Option<QuoteBlock> {
    let target = row.message_id()?;
    if row.kind != RowKind::Message {
        return None;
    }
    let mut quote = QuoteBlock {
        author: row.author.clone(),
        own: row.own,
        target: Some(target),
        ..QuoteBlock::default()
    };
    for block in &row.blocks {
        match block {
            Block::Text(t) if quote.text.is_empty() => {
                quote.text = preview::clip_line(&t.plain);
            }
            Block::Image(_) | Block::Album(_) if quote.icon.is_empty() => {
                let images = block.images();
                let Some(i) = images.first() else { continue };
                // "3 photos" for an album of pictures; a mixed one is named after its first.
                let alike = images.iter().all(|other| other.kind == i.kind);
                let s = preview::attachment_label(i.kind, if alike { images.len() } else { 1 });
                quote.icon = s.icon;
                quote.thumb_url.clone_from(&i.url);
                if quote.text.is_empty() {
                    quote.text = if i.sticker {
                        trc("@info:preview kind of message", "Sticker").into_owned()
                    } else {
                        s.text
                    };
                }
            }
            Block::File(f) if quote.icon.is_empty() => {
                quote.icon = if f.voice {
                    "media-record".to_owned()
                } else {
                    "mail-attachment".to_owned()
                };
                if quote.text.is_empty() {
                    quote.text.clone_from(&f.name);
                }
            }
            Block::Placeholder(p) if quote.text.is_empty() => {
                quote.text.clone_from(&p.text);
                quote.icon.clone_from(&p.icon);
            }
            _ => {}
        }
    }
    // A caption after its picture is the text of the quote.
    if let Some(Block::Text(t)) = row.blocks.iter().find(|b| matches!(b, Block::Text(_))) {
        quote.text = preview::clip_line(&t.plain);
    }
    Some(quote)
}

/// The text an edit of `row` starts from (its text, or its caption; empty for a message with
/// files and no caption) and whether it is a caption.
pub fn edit_text_of_row(row: &MessageRow) -> (String, bool) {
    let caption = row
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Image(_) | Block::Album(_) | Block::File(_)));
    let text = row
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Text(t) => Some(t.full_plain().to_owned()),
            _ => None,
        })
        .unwrap_or_default();
    (text, caption)
}

/// Builds the rows of a timeline window, newest first.
pub fn build_timeline(
    input: &TimelineInput<'_>,
    names: &dyn Names,
    time: &TimeContext<'_>,
) -> Vec<MessageRow> {
    let by_net: HashMap<&kourier_model::NetMsgId, &Message> =
        input.messages.iter().map(|m| (&m.net_id, m)).collect();
    let empty = Vec::new();
    let items = input.messages.iter().rev().map(|msg| Item {
        id: msg.id,
        sent_at: msg.sent_at,
        own: msg.origin.is_own(),
        author: msg.origin.author(),
        body: &msg.body,
        deleted: msg.deleted.as_ref(),
        edited: msg.edited_at.is_some() || msg.revision > 0,
        out_state: msg.out_state.as_ref(),
        quote: if msg.deleted.is_none() {
            quote_block(msg, &by_net, input.messages, names)
        } else {
            None
        },
        reactions: reaction_chips(input.reactions.get(&msg.id).unwrap_or(&empty), names),
        actions: model_actions(msg),
        ask: &[],
        expiry: msg.expiry.as_ref(),
        wait: None,
    });
    build_rows(
        items,
        input.is_group,
        input.unread_from,
        input.has_older,
        names,
        time,
        &ReadMore::default(),
    )
}

/// User-facing reason for [`CannotSend`].
pub fn cannot_send_text(reason: &CannotSend) -> String {
    match reason {
        CannotSend::NotMember => trc(
            "@info:status why the composer is off",
            "You are no longer a member of this group.",
        )
        .into_owned(),
        CannotSend::AdminsOnly => trc(
            "@info:status why the composer is off",
            "Only admins can send messages.",
        )
        .into_owned(),
        CannotSend::Blocked => trc(
            "@info:status why the composer is off",
            "You blocked this contact.",
        )
        .into_owned(),
        CannotSend::RequestPending => trc(
            "@info:status why the composer is off",
            "Accept the message request on your phone to reply.",
        )
        .into_owned(),
        CannotSend::Unregistered => trc(
            "@info:status why the composer is off",
            "This person is no longer using Signal.",
        )
        .into_owned(),
        CannotSend::NotAccepted => trc(
            "@info:status why the composer is off",
            "Start this chat on your phone first.",
        )
        .into_owned(),
        CannotSend::StillLoading => trc(
            "@info:status why the composer is off",
            "This group is still loading.",
        )
        .into_owned(),
        CannotSend::InvitationPending => trc(
            "@info:status why the composer is off",
            "Accept the invitation on your phone to join this group.",
        )
        .into_owned(),
        CannotSend::JoinRequestPending => trc(
            "@info:status why the composer is off",
            "Your request to join this group is waiting for an admin.",
        )
        .into_owned(),
        CannotSend::Other(s) => single_line(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{self, Fixture};
    use crate::names::NameMap;
    use crate::time::{CivilDate, FixedOffset, local_instant};
    use kourier_model::{ConversationId, NetMsgId, Origin, ReadState, SystemEvent};

    const TZ: FixedOffset = FixedOffset(3 * 3600);

    #[test]
    fn only_blocks_in_state_downloading_count_as_downloading() {
        let tile = |state| ImageBlock {
            state,
            ..ImageBlock::default()
        };
        let blocks = vec![
            Block::Image(tile(MediaState::Downloading)),
            Block::Image(tile(MediaState::Local)),
            Block::Album(vec![
                tile(MediaState::Downloading),
                tile(MediaState::Local),
                tile(MediaState::Downloading),
                tile(MediaState::Remote),
            ]),
            Block::File(FileBlock {
                state: MediaState::Downloading,
                ..FileBlock::default()
            }),
            Block::File(FileBlock::default()),
        ];
        assert_eq!(blocks_downloading(&blocks), 4);
        assert_eq!(blocks_downloading(&[]), 0);
    }

    fn at(day: u32, h: u32, m: u32) -> i64 {
        local_instant(
            CivilDate {
                year: 2026,
                month: 10,
                day,
            },
            h,
            m,
            TZ,
        )
    }

    fn msg(id: i64, author: Option<&UserId>, sent_at: i64, body: &str) -> Message {
        let origin = match author {
            Some(a) => Origin::Remote {
                author: a.clone(),
                device: None,
            },
            None => Origin::OwnThisDevice,
        };
        fixture::message(
            MessageId(id),
            ConversationId(1),
            origin,
            sent_at,
            Body::text(body),
        )
    }

    fn build(messages: &[Message], unread: Option<MessageId>, group: bool) -> Vec<MessageRow> {
        let names = NameMap::new();
        let time = TimeContext::new(at(6, 18, 0), &TZ);
        build_timeline(
            &TimelineInput {
                messages,
                reactions: &HashMap::new(),
                is_group: group,
                unread_from: unread,
                has_older: false,
            },
            &names,
            &time,
        )
    }

    fn kinds(rows: &[MessageRow]) -> Vec<&'static str> {
        rows.iter().map(|r| r.kind.as_str()).collect()
    }

    #[test]
    fn the_line_under_a_message_on_its_way_says_why_and_how_long() {
        const T0: i64 = 1_760_000_000_000;
        let w = |reason, retry_at| SendWait {
            reason,
            since: T0,
            retry_at,
        };
        // Both labels as one text, and each label by itself where it matters.
        let text = |wait: &SendWait, now: i64| wait_line(wait, now).text();
        let sending = w(WaitReason::Sending, None);
        // A normal send stays clean: nothing before three seconds.
        assert!(wait_line(&sending, T0).is_empty());
        assert!(wait_line(&sending, T0 + WAIT_SHOWN_AFTER_MS - 1).is_empty());
        assert_eq!(text(&sending, T0 + WAIT_SHOWN_AFTER_MS), "Sending… 0:03");
        // It counts up from the stored `sent_at`.
        assert_eq!(text(&sending, T0 + 42_900), "Sending… 0:42");
        assert_eq!(
            text(&w(WaitReason::Connection, None), T0 + 65_000),
            "Waiting for the connection · 1:05"
        );
        // The timer is a label of its own, so a narrow bubble can cut the reason and never it.
        let line = wait_line(&w(WaitReason::Connection, None), T0 + 65_000);
        assert_eq!(
            (line.reason.as_str(), line.timer.as_str()),
            ("Waiting for the connection", "· 1:05")
        );
        let line = wait_line(&sending, T0 + 42_900);
        assert_eq!(
            (line.reason.as_str(), line.timer.as_str()),
            ("Sending…", "0:42")
        );
        let keys = SendPhase::SettingUpKeys { done: 3, of: 12 };
        assert_eq!(
            text(&w(WaitReason::Phase(keys), None), T0 + 42_000),
            "Setting up encryption with 12 members · 0:42"
        );
        let sent = SendPhase::Sending { done: 8, of: 20 };
        assert_eq!(
            text(&w(WaitReason::Phase(sent), None), T0 + 42_000),
            "Sent to 8 of 20 members · 0:42"
        );
        // Nothing has reached anybody yet: never "Sent to 0 of 20 members".
        let none = SendPhase::Sending { done: 0, of: 20 };
        assert_eq!(
            text(&w(WaitReason::Phase(none), None), T0 + 5_000),
            "Sending to 20 members · 0:05"
        );
        // Waiting to retry: the countdown to the outbox's next try, rounded up, alone in its
        // label.
        let retry = w(WaitReason::Sending, Some(T0 + 90_000));
        assert_eq!(text(&retry, T0 + 60_001), "Next try in 0:30");
        assert_eq!(text(&retry, T0 + 89_999), "Next try in 0:01");
        let line = wait_line(&retry, T0 + 60_001);
        assert_eq!(
            (line.reason.as_str(), line.timer.as_str()),
            ("", "Next try in 0:30")
        );
        // Its instant passed (the row is not read again yet): it is being sent.
        assert_eq!(text(&retry, T0 + 90_000), "Sending… 1:30");
        // Offline, the countdown means nothing: the try waits for the connection.
        assert_eq!(
            text(&w(WaitReason::Connection, Some(T0 + 90_000)), T0 + 60_000),
            "Waiting for the connection · 1:00"
        );
        // Not even a countdown before three seconds.
        assert!(wait_line(&w(WaitReason::Sending, Some(T0 + 30_000)), T0 + 1_000).is_empty());
    }

    /// A message held by something other than its own send says what holds it, in the words
    /// of the banner that says the same, and a countdown to a next try does not hide that.
    #[test]
    fn the_line_names_what_really_holds_the_message() {
        const T0: i64 = 1_760_000_000_000;
        let holds = [
            (WaitReason::Challenge, "Waiting for you to verify · 0:12"),
            (
                WaitReason::SafetyNumber,
                "Waiting for you to confirm the new safety number · 0:12",
            ),
            (
                WaitReason::Clock,
                "Waiting for the computer's clock to be set · 0:12",
            ),
            (
                WaitReason::LoggedOut,
                "Waiting for this device to be linked again · 0:12",
            ),
            (
                WaitReason::Restricted,
                "Waiting: the network restricted this account · 0:12",
            ),
            (
                WaitReason::UpdateApp,
                "Waiting for Kourier to be updated · 0:12",
            ),
            (WaitReason::Reconnect, "Waiting for you to reconnect · 0:12"),
        ];
        for (reason, want) in holds {
            let wait = SendWait {
                reason,
                since: T0,
                retry_at: Some(T0 + 60_000),
            };
            assert_eq!(wait_line(&wait, T0 + 12_000).text(), want, "{reason:?}");
        }
    }

    /// KI18n fills a plural's `%1` with its count before this crate fills the rest
    /// (`Translator::translate_plural`): the members line must still name both numbers
    /// (link feedback L6: the KDE translator showed "Sent to 20 of 20 members").
    #[test]
    fn the_members_line_survives_a_translator_that_fills_the_count() {
        struct LikeKi18n;
        impl crate::i18n::Translator for LikeKi18n {
            fn translate(&self, _context: &str, msgid: &str) -> String {
                msgid.to_owned()
            }
            fn translate_plural(&self, _c: &str, one: &str, many: &str, n: u64) -> String {
                let form = if n == 1 { one } else { many };
                form.replace("%1", &n.to_string())
            }
        }
        const T0: i64 = 1_760_000_000_000;
        let wait = SendWait {
            reason: WaitReason::Phase(SendPhase::Sending { done: 8, of: 20 }),
            since: T0,
            retry_at: None,
        };
        let none = SendWait {
            reason: WaitReason::Phase(SendPhase::Sending { done: 0, of: 20 }),
            ..wait
        };
        crate::i18n::with_translator(LikeKi18n, || {
            assert_eq!(
                wait_line(&wait, T0 + 42_000).text(),
                "Sent to 8 of 20 members · 0:42"
            );
            assert_eq!(
                wait_line(&none, T0 + 5_000).text(),
                "Sending to 20 members · 0:05"
            );
        });
    }

    /// The two ways an attachment cannot be had are two states for QML, and so two texts:
    /// "No longer available" after the network said so, "Not available on this device" when
    /// the backend knew it without asking (user decision, 2026-10-10: "Two texts").
    #[test]
    fn gone_and_not_on_this_device_are_two_states() {
        assert_eq!(
            MediaState::from_attachment(AttachmentState::Expired)
                .0
                .as_str(),
            "expired"
        );
        assert_eq!(
            MediaState::from_attachment(AttachmentState::Unavailable)
                .0
                .as_str(),
            "unavailable"
        );
        assert_ne!(
            crate::media::revision(&kourier_model::Attachment {
                state: AttachmentState::Expired,
                ..Default::default()
            }),
            crate::media::revision(&kourier_model::Attachment {
                state: AttachmentState::Unavailable,
                ..Default::default()
            })
        );
    }

    /// The phone's part (S6): asking and its three endings are their own states (their own
    /// notes, a new picture revision each). Which attachments offer Download is the core's
    /// answer, copied block by block: no rule of the viewmodel's, so a message with a file the
    /// core does not list (too large, being asked) shows its note and no button.
    #[test]
    fn the_blocks_ask_where_the_core_lists_them() {
        let states = [
            (AttachmentState::Asking, "asking"),
            (
                AttachmentState::AskFailed {
                    why: AskFailure::NoAnswer,
                },
                "noanswer",
            ),
            (
                AttachmentState::AskFailed {
                    why: AskFailure::NoMessage,
                },
                "nomessage",
            ),
            (
                AttachmentState::AskFailed {
                    why: AskFailure::NoFile,
                },
                "nofile",
            ),
            (AttachmentState::Expired, "expired"),
            (AttachmentState::Unavailable, "unavailable"),
            (AttachmentState::Remote, "remote"),
        ];
        let mut revisions = std::collections::HashSet::new();
        let ask = |b: &Block| match b {
            Block::Image(i) => i.ask,
            Block::File(f) => f.ask,
            _ => panic!("not a media block"),
        };
        for (state, name) in states {
            assert_eq!(MediaState::from_attachment(state).0.as_str(), name);
            assert!(
                revisions.insert(crate::media::revision(&Attachment {
                    state,
                    ..Default::default()
                })),
                "{name} has its own revision"
            );
            for kind in [AttachmentKind::Image, AttachmentKind::File] {
                let att = Attachment {
                    id: Some(AttachmentId(3)),
                    kind,
                    state,
                    ..Attachment::default()
                };
                let mut blocks = vec![attachment_block(&att, false)];
                assert!(!ask(&blocks[0]), "{name}: not by itself");
                // Whatever its state, the core's list decides.
                mark_askable(&mut blocks, &[AttachmentId(3)]);
                assert!(ask(&blocks[0]), "{name} {kind:?}: listed");
                mark_askable(&mut blocks, &[AttachmentId(4)]);
                assert!(
                    !ask(&blocks[0]),
                    "{name} {kind:?}: another attachment listed"
                );
            }
        }
        // An album: only the listed tile asks.
        let tile = |id| {
            let att = Attachment {
                id: Some(AttachmentId(id)),
                kind: AttachmentKind::Image,
                state: AttachmentState::Unavailable,
                ..Attachment::default()
            };
            match attachment_block(&att, false) {
                Block::Image(i) => i,
                _ => panic!("not an image"),
            }
        };
        let mut album = vec![Block::Album(vec![tile(1), tile(2)])];
        mark_askable(&mut album, &[AttachmentId(2)]);
        let Block::Album(items) = &album[0] else {
            panic!("album")
        };
        assert_eq!(
            items.iter().map(|i| i.ask).collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn only_well_known_media_types_open_without_a_confirmation() {
        let att = |kind, mime: &str, name: &str| Attachment {
            id: Some(AttachmentId(3)),
            kind,
            mime: mime.into(),
            file_name: Some(name.into()),
            state: AttachmentState::Local,
            ..Attachment::default()
        };
        let media = |a: &Attachment| match attachment_block(a, false) {
            Block::Image(i) => i.media,
            Block::File(f) => f.media,
            _ => panic!("not a media block"),
        };
        assert!(media(&att(AttachmentKind::Video, "video/mp4", "clip.mp4")));
        assert!(media(&att(AttachmentKind::Image, "image/jpeg", "a.jpg")));
        assert!(media(&att(AttachmentKind::Audio, "audio/mpeg", "song.mp3")));
        // The sender's claim of an unusual type keeps their extension on the copy: ask first.
        assert!(!media(&att(
            AttachmentKind::Video,
            "video/x-made-up",
            "page.html"
        )));
        assert!(!media(&att(
            AttachmentKind::Image,
            "image/svg+xml",
            "a.svg"
        )));
        assert!(!media(&att(
            AttachmentKind::File,
            "application/pdf",
            "plan.pdf"
        )));
    }

    #[test]
    fn gif_files_play_and_video_gifs_show_as_clips() {
        let gif = |mime: &str| Attachment {
            id: Some(AttachmentId(4)),
            kind: AttachmentKind::Gif,
            mime: mime.into(),
            state: AttachmentState::Local,
            blurhash: Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".into()),
            ..Attachment::default()
        };
        let image = |a: &Attachment| match attachment_block(a, false) {
            Block::Image(i) => i,
            _ => panic!("not an image block"),
        };
        let file = image(&gif("image/gif"));
        assert!(file.animated && file.kind == AttachmentKind::Gif);
        assert!(
            file.url.starts_with("image://kourier/thumb/4?"),
            "a still under the animation"
        );
        assert!(file.full_url.is_empty(), "the viewer plays the copy");
        // Signal's GIFs are MP4 clips: shown and opened like videos, labelled GIF.
        let clip = image(&gif("video/mp4"));
        assert!(!clip.animated && clip.kind == AttachmentKind::Video);
        assert!(clip.url.is_empty() && clip.full_url.is_empty());
        assert!(
            clip.placeholder_url
                .starts_with("image://kourier/blurhash/4?")
        );
        assert_eq!(clip.label, "GIF");
        assert!(clip.media, "an MP4 opens without a confirmation");
        // A GIF-flagged still of another type is a picture for the viewer.
        let still = image(&gif("image/webp"));
        assert!(!still.animated && still.kind == AttachmentKind::Gif);
        assert!(still.full_url.starts_with("image://kourier/att/4?"));
    }

    #[test]
    fn grouping_within_five_minutes() {
        let a = UserId::parse("aci:a").expect("id");
        let b = UserId::parse("aci:b").expect("id");
        // Chronological: a 10:00, a 10:04, a 10:10 (gap > 5 min), b 10:11, own 10:12, own 10:13.
        let mut chrono = vec![
            msg(1, Some(&a), at(6, 10, 0), "1"),
            msg(2, Some(&a), at(6, 10, 4), "2"),
            msg(3, Some(&a), at(6, 10, 10), "3"),
            msg(4, Some(&b), at(6, 10, 11), "4"),
            msg(5, None, at(6, 10, 12), "5"),
            msg(6, None, at(6, 10, 13), "6"),
        ];
        chrono.reverse();
        let rows = build(&chrono, None, true);
        assert_eq!(
            kinds(&rows),
            [
                "message", "message", "message", "message", "message", "message", "date"
            ]
        );
        let flags: Vec<(bool, bool, bool, bool)> = rows
            .iter()
            .filter(|r| r.kind == RowKind::Message)
            .map(|r| (r.show_author, r.show_avatar, r.group_start, r.group_end))
            .collect();
        // Newest first: 6, 5, 4, 3, 2, 1.
        assert_eq!(
            flags,
            [
                (false, false, false, true), // 6 own, ends the own run
                (false, false, true, false), // 5 own, starts it
                (true, true, true, true),    // 4 b alone
                (true, true, true, true),    // 3 a after a 6-minute gap
                (false, true, false, true),  // 2 a, ends run 1-2
                (true, false, true, false),  // 1 a, starts it
            ]
        );
        // 1:1 chats never show author lines or avatars.
        let rows = build(&chrono, None, false);
        assert!(rows.iter().all(|r| !r.show_author && !r.show_avatar));
    }

    #[test]
    fn date_headers_split_days_and_runs() {
        let a = UserId::parse("aci:a").expect("id");
        let mut chrono = vec![
            msg(1, Some(&a), at(4, 23, 58), "1"),
            msg(2, Some(&a), at(5, 0, 1), "2"),
            msg(3, Some(&a), at(6, 9, 0), "3"),
        ];
        chrono.reverse();
        let rows = build(&chrono, None, true);
        assert_eq!(
            kinds(&rows),
            ["message", "date", "message", "date", "message", "date"]
        );
        let headers: Vec<&str> = rows
            .iter()
            .filter(|r| r.kind == RowKind::DateHeader)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(headers, ["Today", "Yesterday", "Sunday"]);
        // 23:58 and 00:01 are 3 minutes apart but on different days: two runs.
        assert!(
            rows.iter()
                .filter(|r| r.kind == RowKind::Message)
                .all(|r| r.show_author)
        );
        let keys: std::collections::HashSet<_> = rows.iter().map(|r| r.key).collect();
        assert_eq!(keys.len(), rows.len(), "keys are unique");
    }

    #[test]
    fn unread_marker_counts_and_breaks_runs() {
        let a = UserId::parse("aci:a").expect("id");
        let mut system = msg(5, Some(&a), at(6, 10, 4), "");
        system.body = Body::System(SystemEvent::MemberLeft { user: a.clone() });
        let mut chrono = vec![
            msg(1, Some(&a), at(6, 10, 0), "read"),
            msg(2, Some(&a), at(6, 10, 1), "unread 1"),
            msg(3, None, at(6, 10, 2), "own"),
            msg(4, Some(&a), at(6, 10, 3), "unread 2"),
            system,
        ];
        chrono[0].read_state = Some(ReadState::Read);
        for m in &mut chrono[1..] {
            if !m.origin.is_own() {
                m.read_state = Some(ReadState::Unread);
            }
        }
        chrono.reverse();
        assert_eq!(first_unread(&chrono), Some(MessageId(2)));
        let rows = build(&chrono, first_unread(&chrono), true);
        assert_eq!(
            kinds(&rows),
            [
                "system", "message", "message", "message", "unread", "message", "date"
            ]
        );
        // Two incoming messages; the system event below the marker is not counted.
        assert_eq!(rows[4].text, "2 unread messages");
        let rows = &rows[1..];
        // Message 2 starts a new run after the marker although message 1 is 1 minute older.
        assert!(rows[2].show_author && rows[2].group_start);
        assert!(rows[4].group_end);
    }

    /// A call line under the unread marker: not while it rings (the phone rings), from the
    /// moment it is a missed call, and no more once it turned out answered. The core gives
    /// the line a read state exactly while it counts.
    #[test]
    fn a_call_line_and_the_unread_marker() {
        use kourier_model::{CallDirection, CallLine, CallMedia, CallState};
        let a = UserId::parse("aci:a").expect("id");
        let line = |state: CallState, read: Option<ReadState>| {
            let mut m = msg(2, Some(&a), at(6, 10, 1), "");
            // A line has no author; its origin says which way the call went.
            m.net_id = NetMsgId::own("call:7");
            m.body = Body::System(SystemEvent::Call(
                CallLine::signal(CallDirection::Incoming, false, state, at(6, 10, 1))
                    .with_media(CallMedia::Voice, at(6, 10, 1))
                    .with_by(a.clone(), at(6, 10, 1)),
            ));
            m.read_state = read;
            m
        };
        let timeline = |call: Message, text_unread: bool| {
            let mut read = msg(1, Some(&a), at(6, 10, 0), "read");
            read.read_state = Some(ReadState::Read);
            let mut later = msg(3, Some(&a), at(6, 10, 2), "later");
            later.read_state = Some(if text_unread {
                ReadState::Unread
            } else {
                ReadState::Read
            });
            // Newest first, as the builder takes them.
            vec![later, call, read]
        };
        // Ringing: only the text behind it is unread.
        let ringing = timeline(line(CallState::Ringing { deadline: 0 }, None), true);
        assert_eq!(first_unread(&ringing), Some(MessageId(3)));
        let rows = build(&ringing, first_unread(&ringing), false);
        assert_eq!(
            kinds(&rows),
            ["message", "unread", "system", "message", "date"]
        );
        assert_eq!(rows[1].text, "1 unread message");
        assert_eq!(rows[2].text, "Incoming voice call…");
        assert_eq!(rows[2].icon, "call-incoming");

        // Missed: the marker stands above the line and counts it.
        let missed = timeline(line(CallState::Missed, Some(ReadState::Unread)), true);
        assert_eq!(first_unread(&missed), Some(MessageId(2)));
        let rows = build(&missed, first_unread(&missed), false);
        assert_eq!(
            kinds(&rows),
            ["message", "system", "unread", "message", "date"]
        );
        assert_eq!(rows[2].text, "2 unread messages");
        assert_eq!(rows[1].text, "Missed voice call");
        // A missed call alone is unread too.
        let alone = timeline(line(CallState::Missed, Some(ReadState::Unread)), false);
        assert_eq!(first_unread(&alone), None, "a read message is newer");
        let newest = vec![
            line(CallState::Missed, Some(ReadState::Unread)),
            alone[2].clone(),
        ];
        assert_eq!(first_unread(&newest), Some(MessageId(2)));
        assert_eq!(
            build(&newest, Some(MessageId(2)), false)[1].text,
            "1 unread message"
        );

        // Answered on the phone after all: the line counts no more.
        let answered = timeline(line(CallState::Answered, None), true);
        assert_eq!(first_unread(&answered), Some(MessageId(3)));
        let rows = build(&answered, first_unread(&answered), false);
        assert_eq!(rows[1].text, "1 unread message");
        assert_eq!(rows[2].text, "Voice call · answered on another device");
        // Read here: it ends the run of unread rows like a read message.
        let read = timeline(line(CallState::Missed, Some(ReadState::Read)), true);
        assert_eq!(first_unread(&read), Some(MessageId(3)));
    }

    #[test]
    fn reactions_receipts_edits_and_tombstones() {
        let a = UserId::parse("aci:a").expect("id");
        let b = UserId::parse("aci:b").expect("id");
        let mut own = msg(1, None, at(6, 10, 0), "hi");
        own.out_state = Some(OutgoingState::Delivered);
        own.edited_at = Some(at(6, 10, 1));
        let mut gone = msg(2, Some(&a), at(6, 10, 2), "secret");
        gone.deleted = Some(kourier_model::Deletion::ForEveryone { by: None });
        let messages = vec![gone, own];
        let mut reactions = HashMap::new();
        reactions.insert(
            MessageId(1),
            vec![
                Reaction {
                    user: Some(a.clone()),
                    emoji: "👍".into(),
                    at: 3,
                },
                Reaction {
                    user: None,
                    emoji: "❤️".into(),
                    at: 1,
                },
                Reaction {
                    user: Some(b.clone()),
                    emoji: "👍".into(),
                    at: 2,
                },
            ],
        );
        reactions.insert(
            MessageId(2),
            vec![Reaction {
                user: Some(a.clone()),
                emoji: "😂".into(),
                at: 1,
            }],
        );
        let mut names = NameMap::new();
        names.insert(a, "Olena");
        names.insert(b, "Taras");
        let time = TimeContext::new(at(6, 18, 0), &TZ);
        let rows = build_timeline(
            &TimelineInput {
                messages: &messages,
                reactions: &reactions,
                is_group: false,
                unread_from: None,
                has_older: true,
            },
            &names,
            &time,
        );
        assert_eq!(kinds(&rows), ["message", "message", "date", "loading"]);
        let own = &rows[1];
        assert_eq!(own.receipt, Receipt::Delivered);
        assert!(own.edited);
        assert_eq!(
            own.reactions,
            [
                ReactionChip {
                    emoji: "👍".into(),
                    count: 2,
                    mine: false,
                    // In the order they reacted.
                    names: vec!["Taras".into(), "Olena".into()],
                    own_at: None,
                },
                ReactionChip {
                    emoji: "❤️".into(),
                    count: 1,
                    mine: true,
                    names: vec!["You".into()],
                    own_at: Some(0),
                },
            ]
        );
        // What the pure fixture offers: everything for an own text, nothing but a local
        // delete for a tombstone.
        assert!(own.actions.edit && own.actions.reply && own.actions.react && own.actions.copy);
        assert!(own.actions.delete_for_everyone && !own.actions.retry);
        let gone = &rows[0];
        assert!(gone.deleted && gone.reactions.is_empty());
        assert_eq!(
            gone.actions,
            RowActions {
                delete_for_me: true,
                ..RowActions::default()
            }
        );
        assert!(
            matches!(&gone.blocks[..], [Block::Placeholder(s)] if s.text == "This message was deleted")
        );
    }

    #[test]
    fn a_failed_send_can_be_retried_and_timers_are_marked() {
        let mut failed = msg(1, None, at(6, 10, 0), "did not go");
        failed.out_state = Some(OutgoingState::Failed {
            reason: kourier_model::FailReason::Network,
            retryable: true,
        });
        let mut timed = msg(2, None, at(6, 10, 1), "gone soon");
        timed.out_state = Some(OutgoingState::Delivered);
        timed.expiry = Some(kourier_model::Expiry {
            kind: kourier_model::DisappearKind::AfterRead,
            timer: std::time::Duration::from_secs(604_800),
            started_at: None,
        });
        let rows = build(&[timed, failed], None, false);
        assert_eq!(rows[1].receipt, Receipt::Failed);
        assert!(rows[1].actions.retry && rows[1].expiry.is_empty());
        assert!(!rows[0].actions.retry);
        assert_eq!(rows[0].expiry, "Disappears 1 week after it is read");
    }

    #[test]
    fn a_long_text_is_cut_in_its_row_and_edited_whole() {
        let body = "word ".repeat(400);
        let rows = build(&[msg(1, None, at(6, 10, 0), &body)], None, false);
        let Some(Block::Text(t)) = rows[0].blocks.first() else {
            panic!("a text block");
        };
        assert!(t.more && t.plain.len() < 1000, "cut: {}", t.plain.len());
        // What an edit starts from, and the whole text for Copy, are not the shown part.
        let whole = body.trim_end();
        assert_eq!(t.full_plain(), whole);
        assert_eq!(edit_text_of_row(&rows[0]), (whole.to_owned(), false));
        // A short text has nothing more.
        let rows = build(&[msg(1, None, at(6, 10, 0), "short")], None, false);
        let Some(Block::Text(t)) = rows[0].blocks.first() else {
            panic!("a text block");
        };
        assert!(!t.more && t.whole.is_empty());
    }

    #[test]
    fn a_row_says_what_a_reply_quotes_and_what_an_edit_starts_from() {
        let a = UserId::parse("aci:a").expect("id");
        let mut names = NameMap::new();
        names.insert(a.clone(), "Olena");
        let time = TimeContext::new(at(6, 18, 0), &TZ);
        let photo = |caption: Option<&str>| Body::Media {
            attachments: vec![Attachment {
                id: Some(AttachmentId(7)),
                kind: AttachmentKind::Image,
                mime: "image/jpeg".into(),
                state: AttachmentState::Local,
                ..Attachment::default()
            }],
            caption: caption.map(kourier_model::Text::plain),
        };
        let mut captioned = msg(1, Some(&a), at(6, 10, 0), "");
        captioned.body = photo(Some("The view\nfrom the ridge"));
        let mut bare = msg(2, None, at(6, 10, 1), "");
        bare.body = photo(None);
        let mut file = msg(3, Some(&a), at(6, 10, 2), "");
        file.body = Body::Media {
            attachments: vec![Attachment {
                id: Some(AttachmentId(8)),
                kind: AttachmentKind::File,
                mime: "application/pdf".into(),
                file_name: Some("Plan.pdf".into()),
                state: AttachmentState::Local,
                ..Attachment::default()
            }],
            caption: None,
        };
        let text = msg(4, None, at(6, 10, 3), "plain text");
        let messages = vec![text, file, bare, captioned];
        let rows = build_timeline(
            &TimelineInput {
                messages: &messages,
                reactions: &HashMap::new(),
                is_group: false,
                unread_from: None,
                has_older: false,
            },
            &names,
            &time,
        );
        let quote = |i: usize| quote_of_row(&rows[i]).expect("quote");
        // A caption is the quoted text, on one line, with the picture's thumbnail and icon.
        let q = quote(3);
        assert_eq!(
            (q.author.as_str(), q.text.as_str(), q.icon.as_str(), q.own),
            ("Olena", "The view from the ridge", "photo", false)
        );
        assert!(q.thumb_url.starts_with("image://kourier/thumb/7?"));
        assert_eq!(q.target, Some(MessageId(1)));
        // Without a caption the quote says what it is.
        let q = quote(2);
        assert_eq!((q.text.as_str(), q.own), ("Photo", true));
        let q = quote(1);
        assert_eq!(
            (q.text.as_str(), q.icon.as_str()),
            ("Plan.pdf", "mail-attachment")
        );
        assert_eq!(quote(0).text, "plain text");
        // Date headers quote nothing.
        assert!(quote_of_row(rows.last().expect("date")).is_none());
        // An edit starts from the text or the caption; a picture without one from nothing.
        assert_eq!(
            edit_text_of_row(&rows[3]),
            ("The view\nfrom the ridge".to_owned(), true)
        );
        assert_eq!(edit_text_of_row(&rows[2]), (String::new(), true));
        assert_eq!(edit_text_of_row(&rows[0]), ("plain text".to_owned(), false));
    }

    #[test]
    fn the_rest_of_a_long_text_is_not_drawn_as_a_file() {
        let mut m = msg(1, None, at(6, 10, 0), "");
        m.body = Body::Media {
            attachments: vec![Attachment {
                id: Some(AttachmentId(9)),
                kind: AttachmentKind::File,
                mime: "text/x-signal-plain".into(),
                long_text: true,
                ..Attachment::default()
            }],
            caption: Some(kourier_model::Text::plain("The first part of a long text")),
        };
        let rows = build(&[m], None, false);
        assert!(
            matches!(&rows[0].blocks[..], [Block::Text(t)] if t.plain.starts_with("The first"))
        );
    }

    #[test]
    fn quotes_resolve_in_window_or_snapshot() {
        let a = UserId::parse("aci:a").expect("id");
        let mut names = NameMap::new();
        names.insert(a.clone(), "Olena");
        let original = msg(1, Some(&a), at(6, 10, 0), "original text");
        let mut reply = msg(2, None, at(6, 10, 1), "reply");
        reply.reply_to = Some(kourier_model::ReplyTo {
            target: kourier_model::MsgTarget::to_id(None, &original.net_id),
            author: Some(a.clone()),
            snapshot: None,
        });
        let mut missing = msg(3, None, at(6, 10, 2), "reply 2");
        missing.reply_to = Some(kourier_model::ReplyTo {
            target: kourier_model::MsgTarget::to_id(None, &NetMsgId::remote(a.clone(), "999")),
            author: Some(a),
            snapshot: Some(kourier_model::QuoteSnapshot {
                text: Some(kourier_model::Text::plain("old text")),
                ..Default::default()
            }),
        });
        let messages = vec![missing, reply, original];
        let time = TimeContext::new(at(6, 18, 0), &TZ);
        let rows = build_timeline(
            &TimelineInput {
                messages: &messages,
                reactions: &HashMap::new(),
                is_group: false,
                unread_from: None,
                has_older: false,
            },
            &names,
            &time,
        );
        let quote = |row: &MessageRow| match &row.blocks[0] {
            Block::Quote(q) => q.clone(),
            other => panic!("not a quote: {other:?}"),
        };
        let q = quote(&rows[1]);
        assert_eq!(
            (q.author.as_str(), q.text.as_str(), q.target, q.state),
            (
                "Olena",
                "original text",
                Some(MessageId(1)),
                QuoteState::Known
            )
        );
        let q = quote(&rows[0]);
        assert_eq!(
            (q.author.as_str(), q.text.as_str(), q.target, q.state),
            ("Olena", "old text", None, QuoteState::Missing)
        );
    }

    #[test]
    fn media_blocks() {
        let fx = Fixture::build().expect("fixture");
        let rows = fx.timeline_rows(fx.long_chat, &TimeContext::new(fx.now_ms, &fx.calendar));
        let types: std::collections::BTreeSet<&str> = rows
            .iter()
            .flat_map(|r| r.blocks.iter().map(Block::type_name))
            .collect();
        for t in ["text", "quote", "image", "file", "placeholder"] {
            assert!(types.contains(t), "fixture lacks a {t} block");
        }
        assert_eq!(mime_icon("application/PDF"), "application-pdf");
        assert_eq!(mime_icon("weird/thing"), "application-octet-stream");
    }

    /// Every reason the composer is off says its own thing: the invitation to accept on the
    /// phone, the request that waits for an admin and the group that is still loading are
    /// three different states with three different remedies, and none reads as "not a member".
    #[test]
    fn each_reason_the_composer_is_off_has_its_own_text() {
        let reasons = [
            (
                CannotSend::NotMember,
                "You are no longer a member of this group.",
            ),
            (CannotSend::AdminsOnly, "Only admins can send messages."),
            (CannotSend::Blocked, "You blocked this contact."),
            (
                CannotSend::RequestPending,
                "Accept the message request on your phone to reply.",
            ),
            (
                CannotSend::Unregistered,
                "This person is no longer using Signal.",
            ),
            (
                CannotSend::NotAccepted,
                "Start this chat on your phone first.",
            ),
            (CannotSend::StillLoading, "This group is still loading."),
            (
                CannotSend::InvitationPending,
                "Accept the invitation on your phone to join this group.",
            ),
            (
                CannotSend::JoinRequestPending,
                "Your request to join this group is waiting for an admin.",
            ),
            (CannotSend::Other("Some\nreason".to_owned()), "Some reason"),
        ];
        for (reason, text) in &reasons {
            assert_eq!(&cannot_send_text(reason), text, "{reason:?}");
        }
        let distinct: std::collections::BTreeSet<_> = reasons
            .iter()
            .map(|(reason, _)| cannot_send_text(reason))
            .collect();
        assert_eq!(distinct.len(), reasons.len(), "two reasons share a text");
    }
}
