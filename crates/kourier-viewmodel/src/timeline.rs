//! Timeline rows: message bubbles with content blocks, date headers, the unread marker and
//! system rows, with the grouping flags the delegates need (§8.3, §8.4).
//!
//! Rows are newest first (row 0 = newest), the order of a `ListView` with
//! `verticalLayoutDirection: ListView.BottomToTop`.

use std::collections::HashMap;

use kourier_model::{
    AskFailure, Attachment, AttachmentId, AttachmentKind, AttachmentState, Body, CannotSend,
    FailReason, Message, MessageId, OutgoingState, SendPhase, UserId,
};

use crate::i18n::{trc, trfc, trnc, trnfc};
use crate::media::{self, MediaVariant};
use crate::media_layout;
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

/// The ring on a file of an own message that waits to go up or goes up (upload progress): what
/// the ring and the X over the file show. A file whose upload is over has none, nor has any
/// file of a message that was sent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UploadView {
    /// How much of the file is up, 0 … 1; 0 for a file nothing has been reported for (it
    /// waits for its turn, or the backend is still preparing it).
    pub share: f32,
    /// The percentage the ring's note shows ("45%"); empty for a file nothing has been
    /// reported for.
    pub label: String,
    /// The X is offered: the core accepts "Cancel Sending" for the message now
    /// ([`RowActions::cancel_sending`], copied, never decided here).
    pub cancel: bool,
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
    /// "Save As…" accepts it: the core lists this attachment among the ones it may save
    /// (`MessageActions::save_attachments`); no rule of the viewmodel's.
    pub save: bool,
    /// Its ring while it waits to go up or goes up (own message on its way).
    pub upload: Option<UploadView>,
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
    ask,
    save,
    upload
});

/// Several pictures and videos of one message, shown as one album.
///
/// The pictures are not cropped to a fixed grid: [`Self::rows`] cuts the first [`ALBUM_TILES`]
/// of them into justified rows of their own shapes ([`media_layout`]), once, when the block is
/// built. The rows depend on the sizes of the pictures only, never on the width the layout
/// gives (the pixel step is the view's).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AlbumBlock {
    /// Every picture and video of the album, in order (the viewer steps through all of them).
    pub items: Vec<ImageBlock>,
    /// The rows of the shown tiles: the first [`ALBUM_TILES`] of `items`.
    pub rows: Vec<media_layout::AlbumRow>,
    /// The one ring of the "+N" tile while the pictures behind it wait or go up: their bytes
    /// together (`None`: none of them has a ring).
    pub more: Option<UploadView>,
}

impl AlbumBlock {
    /// An album of `items`, with its rows.
    pub fn new(items: Vec<ImageBlock>) -> Self {
        let aspects: Vec<f64> = items
            .iter()
            .take(ALBUM_TILES)
            .map(|i| media_layout::tile_aspect(i.width, i.height))
            .collect();
        let rows = media_layout::album_rows(&aspects);
        Self {
            items,
            rows,
            more: None,
        }
    }
}

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
    /// "Save As…" accepts it (as [`ImageBlock::save`]).
    pub save: bool,
    /// Its ring while it waits to go up or goes up (own message on its way).
    pub upload: Option<UploadView>,
}

crate::safe_debug!(FileBlock {
    attachment,
    kind,
    media,
    icon,
    state,
    progress,
    voice,
    ask,
    save,
    upload
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
    Album(AlbumBlock),
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
            Self::Album(album) => &album.items,
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
    /// "Cancel Sending" is accepted (`CancelMessage`): `take_back`, or an upload that can still
    /// be stopped before anything reaches anyone. The menu's entry and the X on the rings of its
    /// files read this one flag (the core's, `MessageActions::cancel_sending`).
    pub cancel_sending: bool,
    /// Download on one of its gone attachments asks the user's phone to upload it again
    /// (the core's `MessageActions::ask_attachments` is not empty; the blocks carry it as `ask`).
    pub ask_attachments: bool,
    /// "Save As…" (or "Save All…") is offered: one of its attachments is on this PC and may be
    /// saved to a folder (the core's `MessageActions::save_attachments` is not empty; the blocks
    /// carry it as `save`).
    pub save_attachments: bool,
    /// "Show Sender's Page" is offered (the core's `MessageActions::show_sender`).
    pub show_sender: bool,
    /// The message mentions people the menu offers a page of (the core's
    /// `MessageActions::show_people` is not empty; the row's `pages` carry them).
    pub show_people: bool,
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
            || self.cancel_sending
            || self.retry
            || self.save_attachments
            || self.show_sender
            || self.show_people
    }
}

/// One page the row's menu offers ("Show Sender's Page", "Show <name>'s Page", "Show Your Page"): who
/// it opens and what the entry says. The label holds a name from the network: plain text only.
#[derive(Clone, PartialEq, Eq)]
pub struct PageEntry {
    /// Whose page it opens.
    pub user: UserId,
    /// The entry's text.
    pub label: String,
    /// It opens the person's safety-number page on top of their page ("Show <name>'s Safety
    /// Number", P5a BQ-3), not only their page.
    pub safety: bool,
}

crate::safe_debug!(PageEntry { user });

/// The pages a message's menu offers, in the menu's order: the sender's, then one per person the
/// core listed for the text. The core states which ([`RowActions::show_sender`] and the list);
/// this only gives them their words. A name is one line of plain text, never cut here (the menu
/// elides what does not fit).
///
/// `safety`: the core named this row part of a queue that waits for that person's safety
/// number (`MessageActions::safety_number`): "Show <name>'s Safety Number" comes first.
fn page_entries(
    actions: &RowActions,
    author: Option<&UserId>,
    people: &[(UserId, bool)],
    safety: Option<&UserId>,
    names: &dyn Names,
) -> Vec<PageEntry> {
    let mut out = Vec::new();
    if let Some(user) = safety {
        out.push(PageEntry {
            user: user.clone(),
            label: trfc(
                "@action:inmenu, %1 the name of a person whose safety number changed",
                "Show %1's Safety Number",
                &[&single_line(&names.display_name(user))],
            ),
            safety: true,
        });
    }
    if let (true, Some(user)) = (actions.show_sender, author) {
        out.push(PageEntry {
            user: user.clone(),
            label: trc("@action:inmenu", "Show Sender's Page").into_owned(),
            safety: false,
        });
    }
    if actions.show_people {
        for (user, own) in people {
            let label = if *own {
                trc(
                    "@action:inmenu the user's own page, from a mention of himself",
                    "Show Your Page",
                )
                .into_owned()
            } else {
                trfc(
                    "@action:inmenu, %1 the name of a person the message mentions",
                    "Show %1's Page",
                    &[&single_line(&names.display_name(user))],
                )
            };
            out.push(PageEntry {
                user: user.clone(),
                label,
                safety: false,
            });
        }
    }
    out
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
    /// The network restricted the account, or holds back what is sent from it, until
    /// `until_ms` when it said. `network` is its name.
    Restricted {
        /// The network's name ("WhatsApp").
        network: &'static str,
        /// When it ends (ms), when the network said.
        until_ms: Option<i64>,
    },
    /// This version of Kourier is too old for the network.
    UpdateApp,
    /// Another session took over the connection.
    Reconnect,
    /// The attempt is at this step (the backend's ephemeral `SendPhase`).
    Phase(SendPhase),
    /// The attempt uploads the message's files and the backend reports their bytes: `done` of
    /// `total` plaintext bytes in all (the files reported so far; "Preparing files" while none
    /// has a byte up).
    Uploading {
        /// Bytes up.
        done: u64,
        /// Bytes to go up in all.
        total: u64,
    },
}

impl WaitReason {
    /// The message waits for something other than its own send: no countdown to a next try
    /// means anything then.
    pub fn is_hold(self) -> bool {
        !matches!(
            self,
            Self::Sending | Self::Phase(_) | Self::Uploading { .. }
        )
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
    wait_line_in(wait, now, None)
}

/// [`wait_line`] with the calendar a date needs ("until today at 14:00"): without one a
/// restriction names no date.
pub fn wait_line_in(wait: &SendWait, now: i64, time: Option<&TimeContext<'_>>) -> WaitLine {
    wait_line_named(wait, now, time, None)
}

/// [`wait_line_in`] that names `who` in the safety-number wait (P5a BQ-3: "Waiting for you to
/// confirm the new safety number with Ada"); `None` keeps the line without a name.
pub fn wait_line_named(
    wait: &SendWait,
    now: i64,
    time: Option<&TimeContext<'_>>,
    who: Option<&str>,
) -> WaitLine {
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
        WaitReason::SafetyNumber => match who {
            Some(who) => trfc(
                "@info:status under a message not sent yet, %1 the name of the person whose safety number changed",
                "Waiting for you to confirm the new safety number with %1",
                &[who],
            ),
            None => trc(
                "@info:status under a message not sent yet",
                "Waiting for you to confirm the new safety number",
            )
            .into_owned(),
        },
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
        // A date that has passed is not named (the core lifts a hold when it ends).
        WaitReason::Restricted { network, until_ms } => {
            match until_ms.filter(|t| *t > now).zip(time) {
                Some((t, time)) => trfc(
                    "@info:status under a message not sent yet, %1 the name of the network, %2 the end",
                    "Waiting: %1 restricted this account until %2",
                    &[network, &time.until(t)],
                ),
                None => trfc(
                    "@info:status under a message not sent yet, %1 the name of the network",
                    "Waiting: %1 restricted this account",
                    &[network],
                ),
            }
        }
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
        // The backend says it makes the files ready (the pictures are re-encoded) before the
        // first byte goes up; and nothing of the files is up yet (the upload form is asked
        // for, the first bytes are on their way): Android's "Processing".
        WaitReason::Phase(SendPhase::Preparing) | WaitReason::Uploading { done: 0, .. } => {
            trc("@info:status under a message being sent", "Preparing files").into_owned()
        }
        // His form: whole numbers, the bytes done in the unit of the total.
        WaitReason::Uploading { done, total } => {
            let (up, all) = time::file_size_pair(done, total);
            trfc(
                "@info:status under a message being sent, %1 percent, %2 bytes done, %3 total with its unit",
                "Uploading %1% · %2 of %3",
                &[&upload_percent(done, total).to_string(), &up, &all],
            )
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
    // A restriction lasts hours or days: the seconds since the send mean nothing there, and the
    // date in the reason is the part that matters, so the reason wraps whole (as the 463 line).
    let timer = if matches!(wait.reason, WaitReason::Restricted { .. }) {
        String::new()
    } else {
        trfc(
            "@info:status after the reason under a message on its way, %1 time since it was sent (m:ss)",
            "· %1",
            &[&since],
        )
    };
    WaitLine { reason: why, timer }
}

/// Whether the network refused the message as a first contact to someone who has not written
/// to the account (WhatsApp's 463): Kourier never sends it again by itself.
pub(crate) fn reach_out_refused(state: Option<&OutgoingState>) -> bool {
    matches!(
        state,
        Some(OutgoingState::Failed {
            reason: FailReason::ReachOutRefused,
            ..
        })
    )
}

/// The line under a message the network refused as a first contact: what happened and the
/// one thing that works. It has no timer, and no "Send Again" goes with it (the core refuses
/// the retry, `MessageActions::retry`).
fn refused_line() -> WaitLine {
    WaitLine {
        reason: trc(
            "@info:status under a message the network refused",
            "Not sent: WhatsApp accepts this message only from your phone. Send it from your phone.",
        )
        .into_owned(),
        timer: String::new(),
    }
}

/// The line under the paused head of a queue that waits for a changed safety number: the
/// head shows as failed, and this says whose number and what it needs (P5a U4; Signal Desktop
/// says "Not sent" and opens the safety-number dialog on a click).
fn paused_head_line(who: &str) -> WaitLine {
    WaitLine {
        reason: trfc(
            "@info:status under a message not sent, %1 the name of the person whose safety number changed",
            "Not sent: the safety number with %1 changed",
            &[who],
        ),
        timer: String::new(),
    }
}

/// The line of a row: only an own message that is still pending has one (or one that was
/// refused as a first contact, which says so for good, or the paused head of a queue that
/// waits for a safety number, which says whose).
///
/// `who`: the name of the person whose safety number a [`WaitReason::SafetyNumber`] wait names.
/// `paused`: the core marked this row as part of that wait
/// ([`kourier_core::facade::MessageActions::safety_number`]); a `Failed` row that is (the paused
/// head) gets the named line even though its receipt is not `Pending` (U4).
pub(crate) fn row_wait_line(
    receipt: Receipt,
    out_state: Option<&OutgoingState>,
    wait: Option<SendWait>,
    who: Option<&str>,
    paused: bool,
    time: &TimeContext<'_>,
) -> WaitLine {
    if receipt == Receipt::Failed && reach_out_refused(out_state) {
        return refused_line();
    }
    if receipt == Receipt::Failed
        && paused
        && let Some(who) = who
    {
        return paused_head_line(who);
    }
    match wait {
        Some(w) if receipt == Receipt::Pending => wait_line_named(&w, time.now_ms, Some(time), who),
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
    /// The pages its menu offers: the sender's and the mentioned people's (`actions` says which
    /// kinds; empty for every row that offers none).
    pub pages: Vec<PageEntry>,
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
            pages: Vec::new(),
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
            Block::Album(album) => album
                .items
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

/// How many pictures, files and "+N" rings of `blocks` have an upload ring with a figure in it
/// (the backend has reported that file: a ring that waits for its turn has none).
pub fn blocks_uploads_reported(blocks: &[Block]) -> usize {
    let figure = |u: &Option<UploadView>| u.as_ref().is_some_and(|u| !u.label.is_empty());
    blocks
        .iter()
        .map(|block| match block {
            Block::Image(image) => usize::from(figure(&image.upload)),
            Block::Album(album) => {
                album.items.iter().filter(|t| figure(&t.upload)).count()
                    + usize::from(figure(&album.more))
            }
            Block::File(file) => usize::from(figure(&file.upload)),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => 0,
        })
        .sum()
}

/// How many upload rings of the `rows` shown have a figure (a `--screenshot` of the upload pages
/// waits for one: the fake says each file's share once a second).
pub fn uploads_reported(rows: &[MessageRow]) -> usize {
    rows.iter()
        .map(|r| blocks_uploads_reported(&r.blocks))
        .sum()
}

/// Whether the newest own message of the `rows` shown (newest first) has left the outbox: the
/// server took it (`Sent`) or it is further (`Delivered`, `Read`; a row with no receipt, such as
/// a deleted one, is no message on its way). False while it is queued or being sent (the clock
/// icon, the line "Sending..." under it), when it failed, and when no own message is shown. A
/// `--screenshot` of a page that SENDS from the window waits for it (`MainWindow.qml`, case
/// `media`), so a picture is never taken with the clock icon.
pub fn newest_own_left_the_outbox(rows: &[MessageRow]) -> bool {
    rows.iter()
        .find(|r| r.own && r.kind == RowKind::Message)
        .is_some_and(|r| !matches!(r.receipt, Receipt::Pending | Receipt::Failed))
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

pub(crate) fn attachment_block(att: &Attachment, sticker: bool) -> Block {
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
                save: false,
                upload: None,
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
            save: false,
            upload: None,
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
                save: false,
                upload: None,
            })
        }
    }
}

/// Marks the blocks of the attachments "Save As…" accepts: the ones the core listed
/// ([`Item::save`]), copied as they are.
fn mark_savable(blocks: &mut [Block], save: &[AttachmentId]) {
    let saves = |id: Option<AttachmentId>| id.is_some_and(|id| save.contains(&id));
    for block in blocks {
        match block {
            Block::Image(i) => i.save = saves(i.attachment),
            Block::Album(album) => {
                for i in &mut album.items {
                    i.save = saves(i.attachment);
                }
            }
            Block::File(f) => f.save = saves(f.attachment),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => {}
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
            Block::Album(album) => {
                for i in &mut album.items {
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
            Block::Album(album) => {
                for i in &mut album.items {
                    apply(i.attachment, i.state, &mut i.progress);
                }
            }
            Block::File(f) => apply(f.attachment, f.state, &mut f.progress),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => {}
        }
    }
}

/// The whole percent of `done` in `total` (0 for an empty total); 100 only when all is up.
pub(crate) fn upload_percent(done: u64, total: u64) -> u64 {
    if total == 0 {
        return 0;
    }
    if done >= total {
        return 100;
    }
    u64::try_from(u128::from(done) * 100 / u128::from(total)).unwrap_or(99)
}

/// The ring over a file with `bytes` reported (`None`: nothing reported for it). `going`: the
/// message has a file that is not up yet, so a file that is up keeps a FULL ring with the same
/// X while the message can still be stopped (brief §3.7 row 3: 100 % / 45 % / 0 %); when all
/// are up the rings go (and none stays at 100 % through the group's key setup).
fn ring(
    bytes: Option<(u64, u64)>,
    empty_ring: bool,
    cancel: bool,
    going: bool,
) -> Option<UploadView> {
    match bytes {
        // Its upload is over, and no file of the message goes on: no ring.
        Some((done, total)) if done >= total && !going => None,
        Some((done, total)) => {
            let (share, percent) = if done >= total {
                (1.0, 100)
            } else {
                (
                    (done as f64 / total as f64).clamp(0.0, 1.0) as f32,
                    upload_percent(done, total),
                )
            };
            Some(UploadView {
                share,
                label: trfc(
                    "@info:status percent of a file's upload",
                    "%1%",
                    &[&percent.to_string()],
                ),
                cancel,
            })
        }
        // The file waits for its turn (or is being prepared): an empty ring with the X the
        // core offers.
        None if empty_ring => Some(UploadView {
            share: 0.0,
            label: String::new(),
            cancel,
        }),
        None => None,
    }
}

/// Puts the rings on the files of an own message on its way (`row`), from the bytes the
/// backend reported for them (`bytes`, by attachment). `empty_ring`: a file nothing was
/// reported for waits for its turn or is being prepared and shows an empty ring; else it has
/// none. The X is the row's `cancel_sending`. A file that is up keeps a full ring while another
/// file of the message is not up (or waits); when every file is up there are no rings.
/// Idempotent: every earlier ring is taken away first, so a row that carries rings can be given
/// its next ones. Stickers have none. The "+N" ring of an album with more than
/// [`ALBUM_TILES`] pictures counts what the tile covers: the fifth tile's own picture and the
/// ones behind it.
pub(crate) fn apply_uploads(
    row: &mut MessageRow,
    bytes: &dyn Fn(AttachmentId) -> Option<(u64, u64)>,
    empty_ring: bool,
) {
    let cancel = row.actions.cancel_sending;
    let of = |id: Option<AttachmentId>| id.and_then(bytes);
    // A file that is not up: bytes still to come, or nothing reported while it waits.
    let pending = |id: Option<AttachmentId>| match of(id) {
        Some((done, total)) => done < total,
        None => empty_ring,
    };
    let going = row.blocks.iter().any(|b| match b {
        Block::Image(i) => !i.sticker && pending(i.attachment),
        Block::Album(a) => a.items.iter().any(|i| pending(i.attachment)),
        Block::File(f) => pending(f.attachment),
        Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => false,
    });
    for block in &mut row.blocks {
        match block {
            Block::Image(i) => {
                i.upload = if i.sticker {
                    None
                } else {
                    ring(of(i.attachment), empty_ring, cancel, going)
                };
            }
            Block::Album(album) => {
                for i in &mut album.items {
                    i.upload = ring(of(i.attachment), empty_ring, cancel, going);
                }
                // The "+N" tile has the ring of what it covers: its own picture (the overlay
                // hides it) and the ones behind it, their bytes together.
                album.more = if album.items.len() > ALBUM_TILES {
                    let covered = album.items.iter().skip(ALBUM_TILES - 1);
                    let reported: Vec<(u64, u64)> =
                        covered.clone().filter_map(|i| of(i.attachment)).collect();
                    if reported.is_empty() {
                        let waits = covered.clone().any(|i| i.upload.is_some());
                        ring(None, waits, cancel, going)
                    } else {
                        let done: u64 = reported.iter().map(|b| b.0).sum();
                        let total: u64 = reported.iter().map(|b| b.1).sum();
                        ring(Some((done, total)), empty_ring, cancel, going)
                    }
                } else {
                    None
                };
            }
            Block::File(f) => f.upload = ring(of(f.attachment), empty_ring, cancel, going),
            Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => {}
        }
    }
}

/// Whether a row carries a ring (so that the next [`apply_uploads`] has something to take away).
pub(crate) fn has_ring(row: &MessageRow) -> bool {
    row.blocks.iter().any(|b| match b {
        Block::Image(i) => i.upload.is_some(),
        Block::Album(a) => a.more.is_some() || a.items.iter().any(|i| i.upload.is_some()),
        Block::File(f) => f.upload.is_some(),
        Block::Text(_) | Block::Quote(_) | Block::Placeholder(_) => false,
    })
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
                _ => blocks.push(Block::Album(AlbumBlock::new(std::mem::take(run)))),
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
    /// The attachments "Save As…" accepts (the core's per-attachment answer, copied to the
    /// blocks as `save`).
    pub save: &'a [AttachmentId],
    /// The people the core listed for the menu's pages (`MessageActions::show_people`): who, and
    /// whether he is the user himself.
    pub people: Vec<(UserId, bool)>,
    /// The core named this row part of a queue that waits for this person's safety number
    /// (`MessageActions::safety_number`: the paused head and every row behind it that waits):
    /// its menu offers their safety-number page and its line names them.
    pub safety: Option<&'a UserId>,
    /// Whose safety number the line under it names while its wait is
    /// [`WaitReason::SafetyNumber`] (the paused head's person; `None`: not known, the line
    /// names nobody).
    pub waits_for: Option<&'a UserId>,
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
                text: preview::deleted_text(d, msg.own, msg.author),
                icon: "trash-empty".into(),
            }));
        } else {
            blocks.extend(msg.quote);
            blocks.extend(content_blocks(msg.id, msg.body, names, read_more));
            if !msg.ask.is_empty() {
                mark_askable(&mut blocks, msg.ask);
            }
            if !msg.save.is_empty() {
                mark_savable(&mut blocks, msg.save);
            }
        }
        let receipt = if msg.own && !deleted {
            Receipt::from_state(msg.out_state)
        } else {
            Receipt::None
        };
        let who = msg
            .waits_for
            .or(msg.safety)
            .map(|u| single_line(&names.display_name(u)));
        let wait = row_wait_line(
            receipt,
            msg.out_state,
            msg.wait,
            who.as_deref(),
            msg.safety.is_some(),
            time,
        );
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
            pages: page_entries(&msg.actions, msg.author, &msg.people, msg.safety, names),
            actions: RowActions {
                // "Send Again" is the core's answer (`MessageActions::retry`: the row is kept
                // for it and the network did not refuse it as a reach-out) on a message that
                // shows as failed.
                retry: receipt == Receipt::Failed && msg.actions.retry,
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
        cancel_sending: false,
        // No core here to refuse: a message that shows as failed may be sent again.
        retry: true,
        ask_attachments: false,
        save_attachments: false,
        // (No core here: the pure fixture offers no page.)
        show_sender: false,
        show_people: false,
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
        save: &[],
        people: Vec::new(),
        safety: None,
        waits_for: None,
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
        CannotSend::NotYetBuilt => trc(
            "@info:status why the composer is off",
            "Sending is not available here yet; it comes in a later version of Kourier.",
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
            Block::Album(AlbumBlock::new(vec![
                tile(MediaState::Downloading),
                tile(MediaState::Local),
                tile(MediaState::Downloading),
                tile(MediaState::Remote),
            ])),
            Block::File(FileBlock {
                state: MediaState::Downloading,
                ..FileBlock::default()
            }),
            Block::File(FileBlock::default()),
        ];
        assert_eq!(blocks_downloading(&blocks), 4);
        assert_eq!(blocks_downloading(&[]), 0);
    }

    /// An album of `n` pictures with the given sizes, as the viewmodel builds it from a message.
    fn album_of(dims: &[(u32, u32)]) -> AlbumBlock {
        let attachments = dims
            .iter()
            .enumerate()
            .map(|(i, &d)| Attachment {
                id: Some(AttachmentId(i as i64 + 1)),
                kind: AttachmentKind::Image,
                mime: "image/jpeg".into(),
                state: AttachmentState::Local,
                dims: Some(d),
                ..Attachment::default()
            })
            .collect();
        let body = Body::Media {
            attachments,
            caption: None,
        };
        let blocks = content_blocks(MessageId(1), &body, &NameMap::new(), &ReadMore::default());
        match blocks.as_slice() {
            [Block::Album(album)] => album.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_album_lays_out_the_tiles_it_shows_and_none_of_the_rest() {
        // Seven pictures: the rows cover the first five, all seven are in the viewer.
        let album = album_of(&[(600, 450); 7]);
        assert_eq!(album.items.len(), 7);
        let counts: Vec<usize> = album.rows.iter().map(|r| r.count).collect();
        assert_eq!(counts.iter().sum::<usize>(), ALBUM_TILES);
        assert_eq!(counts, [2, 3]);
        assert_eq!(album.rows[1].first, 2);
        // Two pictures are one row; the sizes are the pictures' own.
        let two = album_of(&[(600, 450), (450, 600)]);
        assert_eq!(two.rows.len(), 1);
        assert!((two.rows[0].aspect - (600.0 / 450.0 + 450.0 / 600.0)).abs() < 1e-9);
        // The sixth picture's shape does not change the rows.
        let mut dims = vec![(600, 450); 6];
        let before = album_of(&dims).rows;
        dims[5] = (6000, 300);
        assert_eq!(album_of(&dims).rows, before);
    }

    #[test]
    fn a_change_of_one_pictures_size_rebuilds_the_rows() {
        // A sender claimed 4:3 for all three; the decoded file of the second is a screenshot.
        let claimed = album_of(&[(1600, 1200), (1600, 1200), (1600, 1200)]);
        assert_eq!(
            claimed.rows.iter().map(|r| r.count).collect::<Vec<_>>(),
            [1, 2]
        );
        let decoded = album_of(&[(1600, 1200), (1080, 2340), (1600, 1200)]);
        assert_ne!(decoded.rows, claimed.rows);
        // Unknown sizes are 4:3 until they are known.
        let unknown = album_of(&[(0, 0), (0, 0), (0, 0)]);
        assert_eq!(unknown.rows, claimed.rows);
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

    /// The `media` page's picture waits for the newest own message to leave the outbox
    /// (`MainWindow.qml`): only the NEWEST own row counts (an incoming message above it, or an
    /// older own one that is still waiting, change nothing), and "on its way" means the clock
    /// icon: queued or sending, and a failure is not "left" either.
    #[test]
    fn the_newest_own_message_has_left_the_outbox_once_the_server_took_it() {
        let a = UserId::parse("aci:a").expect("id");
        let own = |id: u32, state: Option<OutgoingState>| {
            let mut m = msg(i64::from(id), None, at(6, 10, id), "mine");
            m.out_state = state;
            m
        };
        let left = |messages: &[Message]| newest_own_left_the_outbox(&build(messages, None, false));
        // Newest first, as the timeline's input is.
        let steps = [
            (Some(OutgoingState::Queued), false),
            (Some(OutgoingState::Sending { progress: None }), false),
            (
                Some(OutgoingState::Sending {
                    progress: Some(0.5),
                }),
                false,
            ),
            (Some(OutgoingState::Sent { server_at: None }), true),
            (Some(OutgoingState::Delivered), true),
            (Some(OutgoingState::Read), true),
            (Some(OutgoingState::Viewed), true),
            (
                Some(OutgoingState::Failed {
                    reason: FailReason::Network,
                    retryable: true,
                }),
                false,
            ),
            (
                Some(OutgoingState::PartiallyFailed { failed: vec![] }),
                false,
            ),
            (None, true),
        ];
        for (state, want) in steps {
            let text = format!("{state:?}");
            assert_eq!(left(&[own(2, state.clone())]), want, "{text}");
            // An older own message that was read does not say anything for the newest one.
            assert_eq!(
                left(&[own(2, state.clone()), own(1, Some(OutgoingState::Read))]),
                want,
                "{text} above a read one"
            );
        }
        // The NEWEST own message decides, not an older one that is still waiting, and a newer
        // incoming message is no own message.
        let incoming = msg(3, Some(&a), at(6, 10, 3), "theirs");
        assert!(left(&[
            incoming.clone(),
            own(2, Some(OutgoingState::Delivered)),
            own(1, Some(OutgoingState::Queued)),
        ]));
        assert!(!left(&[
            incoming.clone(),
            own(2, Some(OutgoingState::Queued)),
            own(1, Some(OutgoingState::Delivered)),
        ]));
        // No own message at all: nothing has left.
        assert!(!left(&[incoming]));
        assert!(!left(&[]));
        // A deleted own message has no receipt: it is not on its way.
        let mut gone = own(2, Some(OutgoingState::Queued));
        gone.deleted = Some(kourier_model::Deletion::ForEveryone { by: None });
        assert!(left(&[gone]));
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
                WaitReason::Restricted {
                    network: "WhatsApp",
                    until_ms: None,
                },
                "Waiting: WhatsApp restricted this account",
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

    /// The line under a message that uploads (upload progress): "Preparing files" until a byte
    /// is up, then his form, whole numbers with the bytes done in the unit of the total
    /// ("Uploading 45% · 12 of 30 MB"), the timer after it; Ukrainian reorders nothing but the
    /// verb; a hold (the connection) still goes first (that is the source's, `send_wait`).
    #[test]
    fn the_upload_line_says_preparing_then_the_bytes_in_his_form() {
        struct Uk;
        impl crate::i18n::Translator for Uk {
            fn translate(&self, _context: &str, msgid: &str) -> String {
                match msgid {
                    "Preparing files" => "Підготовка файлів".into(),
                    "Uploading %1% · %2 of %3" => "Вивантаження %1% · %2 з %3".into(),
                    other => other.to_owned(),
                }
            }
            fn translate_plural(&self, _c: &str, one: &str, many: &str, n: u64) -> String {
                (if n == 1 { one } else { many }).to_owned()
            }
        }
        const T0: i64 = 1_760_000_000_000;
        let line = |done: u64, total: u64, at: i64| {
            let wait = SendWait {
                reason: WaitReason::Uploading { done, total },
                since: T0,
                retry_at: None,
            };
            wait_line(&wait, T0 + at).text()
        };
        // Nothing is up yet.
        assert_eq!(line(0, 30_000_000, 5_000), "Preparing files · 0:05");
        assert_eq!(
            line(13_500_000, 30_000_000, 12_000),
            "Uploading 45% · 13 of 30 MB · 0:12"
        );
        assert_eq!(
            line(29_999_999, 30_000_000, 12_000),
            "Uploading 99% · 29 of 30 MB · 0:12"
        );
        // Small files read in bytes, and below 10 units with one decimal (U5), the total the
        // text of the size label; from 10 units the total is the label's figure rounded.
        assert_eq!(line(300, 900, 3_000), "Uploading 33% · 300 of 900 B · 0:03");
        assert_eq!(
            line(1_000_000, 2_400_000, 3_000),
            "Uploading 41% · 1.0 of 2.4 MB · 0:03"
        );
        assert_eq!(
            line(12_582_912, 31_457_280, 3_000),
            "Uploading 40% · 12 of 32 MB · 0:03",
            "31.5 MB is the label: the total is 32, never 31"
        );
        // Nothing before the line is shown at all.
        assert_eq!(line(13_500_000, 30_000_000, 2_999), "");
        crate::i18n::with_translator(Uk, || {
            assert_eq!(line(0, 30_000_000, 5_000), "Підготовка файлів · 0:05");
            assert_eq!(
                line(13_500_000, 30_000_000, 12_000),
                "Вивантаження 45% · 13 з 30 MB · 0:12"
            );
        });
        assert!(!WaitReason::Uploading { done: 1, total: 2 }.is_hold());
    }

    /// The ring of a file (upload progress): the bytes reported, whole percent, the X from the
    /// row; a file that is up has none; a file nothing is reported for has an empty ring only
    /// when the caller says it waits; the pictures behind "+N" share one ring of their bytes.
    #[test]
    fn the_screenshot_hold_counts_only_rings_with_a_figure() {
        let ring = |label: &str| {
            Some(UploadView {
                share: 0.4,
                label: label.into(),
                cancel: true,
            })
        };
        let image = |label: &str| ImageBlock {
            upload: ring(label),
            ..ImageBlock::default()
        };
        let album = AlbumBlock {
            items: vec![image("45%"), image("")],
            more: ring(""),
            ..AlbumBlock::default()
        };
        let file = FileBlock {
            upload: ring("12%"),
            ..FileBlock::default()
        };
        // A ring that waits for its turn (no figure) does not count; one that says a share does.
        assert_eq!(
            blocks_uploads_reported(&[
                Block::Image(image("40%")),
                Block::Image(image("")),
                Block::Album(album),
                Block::File(file),
            ]),
            3
        );
        let mut more = AlbumBlock::default();
        assert_eq!(blocks_uploads_reported(&[Block::Album(more.clone())]), 0);
        more.more = ring("30%");
        assert_eq!(blocks_uploads_reported(&[Block::Album(more)]), 1);
    }

    #[test]
    fn the_rings_follow_the_reported_bytes_and_the_rows_cancel_flag() {
        let picture = |n: i64| ImageBlock {
            attachment: Some(AttachmentId(n)),
            kind: AttachmentKind::Image,
            state: MediaState::Local,
            ..ImageBlock::default()
        };
        let file = FileBlock {
            attachment: Some(AttachmentId(99)),
            state: MediaState::Local,
            ..FileBlock::default()
        };
        let mut row = MessageRow::marker(
            RowKey::Message(MessageId(1)),
            RowKind::Message,
            String::new(),
        );
        row.actions.cancel_sending = true;
        row.blocks = vec![
            Block::Album(AlbumBlock::new((1..=8).map(picture).collect())),
            Block::File(file),
            Block::Image(ImageBlock {
                sticker: true,
                ..picture(50)
            }),
        ];
        // 8 pictures: tiles 1..=5, the pictures 6, 7 and 8 stand behind "+3".
        let bytes = |a: AttachmentId| match a.0 {
            1 => Some((100, 100)),
            2 => Some((45, 100)),
            3 => Some((0, 100)),
            5 => Some((60, 100)),
            6 => Some((10, 100)),
            7 => Some((50, 100)),
            8 => Some((30, 100)),
            99 => Some((3_000, 4_000)),
            50 => Some((1, 2)),
            _ => None,
        };
        apply_uploads(&mut row, &bytes, false);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        let rings: Vec<_> = album.items.iter().map(|i| i.upload.clone()).collect();
        assert_eq!(
            rings[0],
            Some(UploadView {
                share: 1.0,
                label: "100%".into(),
                cancel: true
            }),
            "a picture that is up keeps a full ring with its X while the others go on"
        );
        assert_eq!(
            rings[1],
            Some(UploadView {
                share: 0.45,
                label: "45%".into(),
                cancel: true
            })
        );
        assert_eq!(rings[2].as_ref().map(|r| r.label.as_str()), Some("0%"));
        assert_eq!(rings[3], None, "nothing reported, nothing waits: no ring");
        let more = album.more.as_ref().expect("the ring of +3");
        assert_eq!((more.label.as_str(), more.cancel), ("37%", true));
        assert!(
            (more.share - 0.375).abs() < 1e-6,
            "the fifth tile's own 60 and the 90 behind it, of 400 bytes together"
        );
        assert_eq!(
            rings[4].as_ref().map(|r| r.label.as_str()),
            Some("60%"),
            "the covered tile keeps its own figure (the overlay hides its ring)"
        );
        let Block::File(f) = &row.blocks[1] else {
            panic!("file");
        };
        assert_eq!(f.upload.as_ref().map(|r| r.label.as_str()), Some("75%"));
        let Block::Image(sticker) = &row.blocks[2] else {
            panic!("sticker");
        };
        assert_eq!(sticker.upload, None, "a sticker has no ring");
        assert!(has_ring(&row));

        // The next report takes the earlier rings away first, and the X follows the row.
        row.actions.cancel_sending = false;
        apply_uploads(&mut row, &|_| None, false);
        assert!(!has_ring(&row), "nothing reported, nothing waits");
        // A message that waits for its turn: every file an empty ring with the core's X.
        row.actions.cancel_sending = true;
        apply_uploads(&mut row, &|_| None, true);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        let empty = UploadView {
            share: 0.0,
            label: String::new(),
            cancel: true,
        };
        assert!(
            album
                .items
                .iter()
                .all(|i| i.upload.as_ref() == Some(&empty))
        );
        assert_eq!(
            album.more.as_ref(),
            Some(&empty),
            "one empty ring behind +3"
        );
        let Block::File(f) = &row.blocks[1] else {
            panic!("file");
        };
        assert_eq!(f.upload.as_ref(), Some(&empty));
    }

    /// U9: a file that is up keeps a FULL ring with its X while another file of the message goes
    /// on (or waits for its turn); once every file is up there are no rings. The "+N" ring
    /// counts the fifth tile's own picture (U6) and exists only with pictures behind the tiles.
    #[test]
    fn a_file_that_is_up_keeps_its_full_ring_while_the_others_go_on() {
        let picture = |n: i64| ImageBlock {
            attachment: Some(AttachmentId(n)),
            kind: AttachmentKind::Image,
            state: MediaState::Local,
            ..ImageBlock::default()
        };
        let mut row = MessageRow::marker(
            RowKey::Message(MessageId(1)),
            RowKind::Message,
            String::new(),
        );
        row.actions.cancel_sending = true;
        row.blocks = vec![
            Block::Album(AlbumBlock::new((1..=5).map(picture).collect())),
            Block::File(FileBlock {
                attachment: Some(AttachmentId(99)),
                state: MediaState::Local,
                ..FileBlock::default()
            }),
        ];
        let full = Some(UploadView {
            share: 1.0,
            label: "100%".into(),
            cancel: true,
        });
        // The file still goes: every picture that is up keeps its full ring.
        let going = |a: AttachmentId| match a.0 {
            99 => Some((10, 100)),
            _ => Some((50, 50)),
        };
        apply_uploads(&mut row, &going, false);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        assert!(album.items.iter().all(|i| i.upload == full), "full rings");
        assert_eq!(album.more, None, "five pictures are five tiles: no \"+N\"");
        // A file nothing was reported for yet waits: the others that are up keep their rings.
        let waiting = |a: AttachmentId| (a.0 != 99).then_some((50, 50));
        apply_uploads(&mut row, &waiting, true);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        assert!(
            album.items.iter().all(|i| i.upload == full),
            "a waiting file goes on"
        );
        // Everything is up: no ring stays at 100 %.
        let all_up = |a: AttachmentId| {
            let size = if a.0 == 99 { 100 } else { 50 };
            Some((size, size))
        };
        apply_uploads(&mut row, &all_up, false);
        assert!(!has_ring(&row), "every file is up: no rings");
        // The X follows the core's word for a full ring too.
        row.actions.cancel_sending = false;
        apply_uploads(&mut row, &going, false);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        assert_eq!(
            album.items[0].upload.as_ref().map(|r| (r.share, r.cancel)),
            Some((1.0, false)),
            "a full ring without an X when the message cannot be stopped"
        );
    }

    /// U6: six pictures, five tiles, "+1": the ring of the "+N" tile is the fifth picture's
    /// bytes and the sixth's together, and it waits (empty) when none of them is reported.
    #[test]
    fn the_plus_ring_counts_the_fifth_tiles_own_picture() {
        let picture = |n: i64| ImageBlock {
            attachment: Some(AttachmentId(n)),
            kind: AttachmentKind::Image,
            state: MediaState::Local,
            ..ImageBlock::default()
        };
        let mut row = MessageRow::marker(
            RowKey::Message(MessageId(1)),
            RowKind::Message,
            String::new(),
        );
        row.actions.cancel_sending = true;
        row.blocks = vec![Block::Album(AlbumBlock::new(
            (1..=6).map(picture).collect(),
        ))];
        // Only the fifth is reported (at 80 %): the ring behind +1 shows it, not the sixth's 0.
        let bytes = |a: AttachmentId| match a.0 {
            5 => Some((80, 100)),
            _ => None,
        };
        apply_uploads(&mut row, &bytes, false);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        let more = album.more.as_ref().expect("the ring of +1");
        assert_eq!(more.label, "80%");
        assert!((more.share - 0.8).abs() < 1e-6);
        // Both reported: their bytes together (80 + 20 of 200).
        let both = |a: AttachmentId| match a.0 {
            5 => Some((80, 100)),
            6 => Some((20, 100)),
            _ => None,
        };
        apply_uploads(&mut row, &both, false);
        let Block::Album(album) = &row.blocks[0] else {
            panic!("album");
        };
        assert_eq!(album.more.as_ref().map(|r| r.label.as_str()), Some("50%"));
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
        let mut album = vec![Block::Album(AlbumBlock::new(vec![tile(1), tile(2)]))];
        mark_askable(&mut album, &[AttachmentId(2)]);
        let Block::Album(album) = &album[0] else {
            panic!("album")
        };
        assert_eq!(
            album.items.iter().map(|i| i.ask).collect::<Vec<_>>(),
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

    /// W6a2 (B7b, his U4): the tombstone of a message another member's admin deleted says so; the author's own delete does
    /// not. Mutant: the author not handed to the words.
    #[test]
    fn a_tombstone_of_an_admins_delete_says_so() {
        let a = UserId::parse("aci:a").expect("id");
        let b = UserId::parse("aci:b").expect("id");
        let mut by_admin = msg(2, Some(&a), at(6, 10, 2), "secret");
        by_admin.deleted = Some(kourier_model::Deletion::ForEveryone {
            by: Some(b.clone()),
        });
        let mut by_author = msg(1, Some(&a), at(6, 10, 1), "secret");
        by_author.deleted = Some(kourier_model::Deletion::ForEveryone {
            by: Some(a.clone()),
        });
        // Newest first, as the core gives them.
        let rows = build(&[by_admin, by_author], None, true);
        let texts: Vec<&str> = rows
            .iter()
            .filter_map(|row| match &row.blocks[..] {
                [Block::Placeholder(s)] => Some(s.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            [
                "This message was deleted by an admin",
                "This message was deleted"
            ]
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
            (
                CannotSend::NotYetBuilt,
                "Sending is not available here yet; it comes in a later version of Kourier.",
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

    /// People pages P3: the words of the menu's pages. The core states which entries (the flags
    /// and the list); this only names them: the sender's, one per mentioned person with the name
    /// as ONE line of plain text and never cut here, and the user's own as "Show Your Page".
    #[test]
    fn the_menu_pages_have_their_words() {
        let ari = UserId::parse("aci:ari").expect("id");
        let me = UserId::parse("aci:me").expect("id");
        let bold = UserId::parse("aci:bold").expect("id");
        let mut names = NameMap::new();
        names.insert(ari.clone(), "Ari");
        names.insert(me.clone(), "Me Myself");
        let long = format!("<b>x</b>\n{}", "N".repeat(400));
        names.insert(bold.clone(), long.clone());
        let on = RowActions {
            show_sender: true,
            show_people: true,
            ..RowActions::default()
        };
        let people = [
            (ari.clone(), false),
            (me.clone(), true),
            (bold.clone(), false),
        ];
        let pages = page_entries(&on, Some(&ari), &people, None, &names);
        let labels: Vec<&str> = pages.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(labels[0], "Show Sender's Page");
        assert_eq!(labels[1], "Show Ari's Page");
        assert_eq!(labels[2], "Show Your Page", "the user's own, not his name");
        // The name is one line of plain text, kept whole (the menu elides it).
        assert_eq!(
            labels[3],
            format!("Show <b>x</b> {}'s Page", "N".repeat(400)),
            "markup stays text, the newline is a space, nothing is cut"
        );
        assert_eq!(
            pages.iter().map(|p| &p.user).collect::<Vec<_>>(),
            vec![&ari, &ari, &me, &bold]
        );
        // What the core did not state is not offered, and a sender without an id has no entry.
        let only_people = RowActions {
            show_people: true,
            ..RowActions::default()
        };
        assert_eq!(
            page_entries(&only_people, Some(&ari), &people, None, &names).len(),
            3
        );
        let only_sender = RowActions {
            show_sender: true,
            ..RowActions::default()
        };
        assert_eq!(
            page_entries(&only_sender, Some(&ari), &people, None, &names).len(),
            1
        );
        assert!(page_entries(&only_sender, None, &people, None, &names).is_empty());
        assert!(page_entries(&RowActions::default(), Some(&ari), &people, None, &names).is_empty());
        // A row that offers pages offers a menu.
        assert!(only_sender.any() && only_people.any());
    }

    /// P5a BQ-3: the paused head of a queue that waits for a person's safety number offers
    /// their safety-number page first, named; the line under the waiting messages names them
    /// (without a known person it stays as before).
    #[test]
    fn a_paused_head_offers_the_safety_number_and_names_the_person() {
        const T0: i64 = 1_760_000_000_000;
        let ada = UserId::parse("aci:ada").expect("id");
        let mut names = NameMap::new();
        names.insert(ada.clone(), "Ada\nLovelace");
        let on = RowActions {
            show_sender: true,
            ..RowActions::default()
        };
        let pages = page_entries(&on, Some(&ada), &[], Some(&ada), &names);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0].label, "Show Ada Lovelace's Safety Number");
        assert!(pages[0].safety && pages[0].user == ada);
        assert!(!pages[1].safety, "the sender's page is a page");
        let wait = SendWait {
            reason: WaitReason::SafetyNumber,
            since: T0,
            retry_at: None,
        };
        assert_eq!(
            wait_line_named(&wait, T0 + 5_000, None, Some("Ada")).reason,
            "Waiting for you to confirm the new safety number with Ada"
        );
        assert_eq!(
            wait_line_named(&wait, T0 + 5_000, None, None).reason,
            "Waiting for you to confirm the new safety number"
        );
    }

    /// U4: a FAILED row the core named (the paused head) says whose number, at once and with
    /// no timer; a failed row that is not named, a named row without a known person and a
    /// network refusal keep what they had.
    #[test]
    fn a_failed_row_in_a_safety_wait_names_the_person() {
        use kourier_model::{FailReason, OutgoingState};
        let time = TimeContext::new(at(6, 18, 0), &TZ);
        let wait = SendWait {
            reason: WaitReason::SafetyNumber,
            since: 1_760_000_000_000,
            retry_at: None,
        };
        let line = |receipt, state: Option<&OutgoingState>, who, paused| {
            row_wait_line(receipt, state, Some(wait), who, paused, &time)
        };
        assert_eq!(
            line(Receipt::Failed, None, Some("Ada"), true).reason,
            "Not sent: the safety number with Ada changed"
        );
        assert_eq!(line(Receipt::Failed, None, Some("Ada"), true).timer, "");
        assert_eq!(
            line(Receipt::Failed, None, Some("Ada"), false),
            WaitLine::default()
        );
        assert_eq!(line(Receipt::Failed, None, None, true), WaitLine::default());
        let refused = OutgoingState::Failed {
            reason: FailReason::ReachOutRefused,
            retryable: false,
        };
        // (A reach-out refusal keeps its own line whatever else is true of the row.)
        assert_eq!(
            line(Receipt::Failed, Some(&refused), Some("Ada"), true),
            row_wait_line(Receipt::Failed, Some(&refused), None, None, false, &time)
        );
    }
}
