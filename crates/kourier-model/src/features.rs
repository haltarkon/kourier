//! Capabilities in three tiers (architecture §3.8): the static [`NetworkDescriptor`], dynamic
//! [`AccountCaps`] and per-conversation [`ConversationFeatures`].

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::content::{AttachmentKind, DisappearKind};
use crate::ids::{IdentifierKind, MsgIdScope};
use crate::text::Text;

/// How well a feature is supported (bridgev2 levels). Ordered from worst to best.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub enum Support {
    /// The network rejects messages that use it.
    Rejected = -2,
    /// The network silently drops it.
    Dropped = -1,
    /// Not supported; the UI hides it.
    #[default]
    Unsupported = 0,
    /// Supported with limits.
    Partial = 1,
    /// Fully supported.
    Full = 2,
}

impl Support {
    /// The bridgev2 level (-2 … 2).
    pub fn level(self) -> i8 {
        self as i8
    }

    /// `Partial` or `Full`: the UI may offer the feature.
    pub fn is_usable(self) -> bool {
        self >= Self::Partial
    }
}

/// Formatting a network may support (one per [`crate::RangeKind`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum FormatKind {
    /// Bold.
    Bold,
    /// Italic.
    Italic,
    /// Strikethrough.
    Strike,
    /// Spoiler.
    Spoiler,
    /// Monospace.
    Mono,
    /// Mention of a user.
    Mention,
    /// Mention of everyone.
    MentionAll,
    /// Link with a target different from its text.
    Link,
}

/// Limits for one attachment kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileLimits {
    /// Maximum size in bytes.
    #[serde(default)]
    pub max_size: u64,
    /// Whether a caption is allowed.
    #[serde(default)]
    pub caption: bool,
}

/// Files that may share one message, and how many of them one message carries
/// ([`FilePacking`]). A file is named by its kind and by its MIME type, as the core reads both
/// from the file's content, so a network can say exactly what its clients show together: a
/// kind as a whole (`kinds`), a kind without some of its formats (`except`), or single formats
/// of a kind that otherwise travels alone (`types`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct FileAlbum {
    /// The kinds that may be mixed in one message, whatever their format (apart from
    /// `except`). The fake network: pictures, GIFs and videos.
    pub kinds: BTreeSet<AttachmentKind>,
    /// MIME types that may be mixed in this album although their kind is not in `kinds`
    /// (Signal: `video/mp4`, the one video format every client draws in an album). Lowercase
    /// `type/subtype` without parameters; compared ignoring case and parameters.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub types: BTreeSet<String>,
    /// MIME types that do not join the album although their kind is in `kinds`: such a file
    /// travels in a message of its own (Signal: `image/heic` and `image/heif`, which Signal
    /// Desktop cannot draw, and an album it cannot draw whole shows only its first item).
    /// Written and compared like `types`; `except` wins over `types`.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub except: BTreeSet<String>,
    /// Most attachments of this album in one message such that every client of the network
    /// shows all of them. 0 and 1 both mean one per message.
    pub max: u16,
}

/// A MIME type as the packing rule compares it: without parameters, trimmed, lowercase.
fn bare_mime(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

impl FileAlbum {
    /// Whether a file of `kind` whose content is `mime` may share a message of this album.
    /// An unknown type (`""`) is decided by its kind alone.
    pub fn takes(&self, kind: AttachmentKind, mime: &str) -> bool {
        if self.types.is_empty() && self.except.is_empty() {
            return self.kinds.contains(&kind);
        }
        let mime = bare_mime(mime);
        if !mime.is_empty() && self.except.contains(&mime) {
            return false;
        }
        self.kinds.contains(&kind) || (!mime.is_empty() && self.types.contains(&mime))
    }
}

/// One file of a selection as the packing rule sees it
/// ([`ConversationFeatures::plan`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlanFile<'a> {
    /// The kind it is sent as.
    pub kind: AttachmentKind,
    /// Its MIME type from the content (`""` when unknown: the kind alone decides then).
    pub mime: &'a str,
}

impl<'a> PlanFile<'a> {
    /// A file of `kind` with the type `mime`.
    pub fn new(kind: AttachmentKind, mime: &'a str) -> Self {
        Self { kind, mime }
    }
}

/// How files pack into messages (§3.8): what one message of the network can carry so that
/// every client of that network shows all of it. It is a statement about the network and its
/// clients, never a limit on what the user may pick: the core splits a selection of any size
/// and any mix of kinds into as many messages as this rule needs
/// ([`ConversationFeatures::plan_files`]).
///
/// The default (no albums) is the safe one: every file travels in a message of its own, which
/// every client shows. Whether a caption can go with a kind is [`FileLimits::caption`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct FilePacking {
    /// Groups of files that may share a message, each with its size. A file no album takes
    /// is sent alone; a file several albums take belongs to the first of them.
    pub albums: Vec<FileAlbum>,
    /// A message with files cannot quote another message on this network: a reply sent with
    /// files then needs a text (the caption), which goes out first as a message of its own and
    /// carries the quote. `false` (Signal): the first message with files carries the quote.
    pub reply_needs_text: bool,
}

impl FilePacking {
    /// The album of `file` and how many attachments it takes, when more than one.
    fn album_of(&self, file: PlanFile<'_>) -> Option<(usize, usize)> {
        self.albums
            .iter()
            .enumerate()
            .find(|(_, a)| a.takes(file.kind, file.mime))
            .map(|(i, a)| (i, usize::from(a.max)))
            .filter(|(_, max)| *max > 1)
    }
}

/// How a selection of files is sent ([`ConversationFeatures::plan`]): the messages, in
/// sending order. The user's order is kept: a message holds neighbours of the selection, never
/// files gathered from further apart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct FilePlan {
    /// The caption goes out first as text of its own (and carries the reply): the first
    /// message with files cannot carry a caption (Signal: a voice note), files cannot carry a
    /// quote on this network, or the caption is longer than one message carries
    /// (`ConversationFeatures::max_text_bytes`). `false`: the first message with files
    /// carries both. Always `caption_messages > 0`.
    pub lead_text: bool,
    /// How many text messages the caption becomes when it goes first (`lead_text`): one, or
    /// as many as `Text::split` makes of a caption above `max_text_bytes` when the plan was
    /// given the caption itself. 0 when the caption rides on the first message with files, or
    /// there is none.
    pub caption_messages: usize,
    /// The messages with files, in sending order; each lists positions in the selection,
    /// ascending.
    pub messages: Vec<Vec<usize>>,
    /// The reply cannot be sent with this selection: files cannot quote on this network
    /// ([`FilePacking::reply_needs_text`]) and there is no caption to carry the quote. The
    /// core refuses such a send; with a caption, or without the reply, it goes out.
    pub reply_unsendable: bool,
}

impl FilePlan {
    /// How many messages the selection becomes (the composer's "5 files, 3 messages"): the
    /// messages with files and the texts the caption becomes when it goes first.
    pub fn message_count(&self) -> usize {
        self.messages.len() + self.caption_messages
    }
}

/// Disappearing-message timers a conversation takes besides the ones it offers by name
/// (`ConversationFeatures::disappearing`): every value `min_secs + k × step_secs` up to
/// `max_secs`, in whole seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct TimerRange {
    /// The shortest timer (seconds).
    pub min_secs: u32,
    /// The longest timer (seconds).
    pub max_secs: u32,
    /// The distance between two values (seconds); 0 and 1 both mean every whole second.
    pub step_secs: u32,
}

impl TimerRange {
    /// Whether a timer of `secs` seconds is one of the range's values. Never 0 (that is
    /// "off").
    pub fn contains(&self, secs: u64) -> bool {
        let (min, max) = (u64::from(self.min_secs), u64::from(self.max_secs));
        let step = u64::from(self.step_secs.max(1));
        secs > 0 && secs >= min && secs <= max && (secs - min).is_multiple_of(step)
    }
}

/// A time/count window for edits and deletes. `None` limits mean unlimited.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Window {
    /// Maximum age of the target message in seconds.
    #[serde(default)]
    pub max_age_secs: Option<u32>,
    /// Maximum number of times (edits per message).
    #[serde(default)]
    pub max_count: Option<u16>,
}

impl Window {
    /// Whether an action on a message `age_secs` old, done `count` times before, is allowed.
    pub fn allows(&self, age_secs: u64, count: u16) -> bool {
        self.max_age_secs.is_none_or(|m| age_secs <= u64::from(m))
            && self.max_count.is_none_or(|m| count < m)
    }
}

/// Where older history comes from (§4.10).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HistorySource {
    /// Nothing beyond what was received live (Signal M1, Threema).
    #[default]
    None,
    /// The network pushes history after linking (WhatsApp): the backend sends
    /// `BackendEvent::HistoryBatch`es of the chats it has, and a conversation says
    /// `has_more` while more is coming. It says nothing about ONE archive that a phone uploads
    /// for a fresh link (Signal): that is [`NetworkDescriptor::history_transfer`], and a UI
    /// offers "Transfer History from Phone…" only for that flag.
    PushedOnLink,
    /// The core requests it with `FetchHistory` (WhatsApp PDO, newsletters).
    OnDemand,
}

/// Whether the `read` flags of a network's HISTORY rows tell what the person has seen (W4 fix F1,
/// the lead's decision). A history copy never lowers what the person has not seen: where the
/// network cannot say it ([`HistoryReadState::Unknown`], WhatsApp: its backend states `read:
/// true` for rows it knows nothing about and gives the unread count as a hint), the core does
/// NOT raise an EXISTING row's read state from a history copy, because that copy may repeat a
/// message that arrived live and is still unread here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HistoryReadState {
    /// A history row's `read` says whether the message was seen (Signal's archive, the fake):
    /// a copy that says "read" raises an existing row.
    #[default]
    Known,
    /// A history row's `read` is only "read or not known" (WhatsApp): it never raises an
    /// existing row's read state.
    Unknown,
}

/// Group management actions the user may take (all false in M1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupActions {
    /// Add members.
    pub add_members: bool,
    /// Remove members.
    pub remove_members: bool,
    /// Change the title.
    pub rename: bool,
    /// Change the avatar.
    pub change_avatar: bool,
    /// Leave the group.
    pub leave: bool,
    /// Disband the group.
    pub disband: bool,
}

/// What a group's rules say, as the network states them (P2, the group's page). Read-only here:
/// Kourier writes no group state. The flags are the rules that are ON; a rule the network does
/// not have, or an access level that is not "only admins" (everyone, or nobody), is `false`.
/// [`ConversationFeatures::group_info`] is `None` when the network did not state the rules
/// at all (the page then shows no rule line), so "everyone may change everything" is never
/// claimed for a network that did not say.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct GroupInfo {
    /// Only admins may change the group's name, picture, description and disappearing-message
    /// timer.
    pub attributes_admin_only: bool,
    /// Only admins may add members.
    pub members_admin_only: bool,
    /// Only admins may send messages (an announcement group).
    pub announcements_only: bool,
    /// The group was ended for everyone: nobody can send, and nobody can change anything.
    pub terminated: bool,
}

/// Why the user cannot send in a conversation.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CannotSend {
    /// We are not a member.
    NotMember,
    /// Only admins may send.
    AdminsOnly,
    /// The conversation is blocked.
    Blocked,
    /// A message request is pending.
    RequestPending,
    /// The peer is unregistered.
    Unregistered,
    /// The chat must be started on the phone (Signal M1, §6.7).
    NotAccepted,
    /// The group is known only from the phone's history so far: its members and our place in
    /// it come with its first fetch from the server.
    StillLoading,
    /// We are invited to the group and have not accepted: Kourier cannot accept an
    /// invitation, the phone does.
    InvitationPending,
    /// We asked to join the group and an administrator has not answered yet.
    JoinRequestPending,
    /// Kourier cannot send on this network yet: sending comes in a later version; a network
    /// states it until its send path exists.
    NotYetBuilt,
    /// Another reason, as user-facing text.
    Other(String),
}

/// Per-conversation features (§3.8), sent with `ConversationUpsert`. The default is the most
/// restrictive set. Maps are ordered, so the serialized form and [`Self::stable_hash`] are
/// canonical.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationFeatures {
    /// Maximum text length in bytes.
    pub max_text_bytes: Option<u32>,
    /// Formatting support per kind; absent kinds are unsupported.
    pub formatting: BTreeMap<FormatKind, Support>,
    /// Attachment limits per kind; absent kinds cannot be sent.
    pub files: BTreeMap<AttachmentKind, FileLimits>,
    /// How several files pack into messages (which kinds share a message, and how many).
    pub file_packing: FilePacking,
    /// Quoting replies.
    pub reply: Support,
    /// Editing own messages (send side).
    pub edit: Option<Window>,
    /// Deleting own messages for everyone (send side).
    pub delete_for_everyone: Option<Window>,
    /// Deleting for this account only.
    pub delete_for_me: bool,
    /// Window for incoming edits.
    pub edit_receive: Option<Window>,
    /// Window for incoming deletes.
    pub delete_receive: Option<Window>,
    /// Group admins may delete others' messages.
    pub admin_delete: bool,
    /// Reactions at all.
    pub reactions: Support,
    /// Reactions one user may have on a message: `Some(1)` replaces (Signal, WhatsApp), `None`
    /// is an unlimited set (Threema).
    pub reactions_per_user: Option<u8>,
    /// Typing indicators.
    pub typing: bool,
    /// Read receipts.
    pub read_receipts: bool,
    /// Delivery receipts.
    pub delivery_receipts: bool,
    /// Disappearing-message timers in seconds, per kind: the values the network's clients
    /// offer by name (a menu). A kind listed here with no values still has timers when
    /// `disappearing_custom` has a range for it.
    pub disappearing: BTreeMap<DisappearKind, Vec<u32>>,
    /// Per kind, the range of timers the network takes besides the offered values (a
    /// "custom time"): Signal takes any whole second up to four weeks. Empty: only the
    /// offered values. [`Self::takes_timer`] is the check.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub disappearing_custom: BTreeMap<DisappearKind, TimerRange>,
    /// File-name extensions (lower case, without the dot) of files that an official client of
    /// the network shows to its user but will not save or open: Signal Desktop does that for
    /// what it calls dangerous types (`.exe`, `.apk`, `.js`, …). It is never a reason to
    /// refuse a file: the phones of the same people save it. A composer may say so beside
    /// such a file ([`Self::is_hard_to_save`]), so the sender can pack it differently. Empty: no
    /// client is known to do that.
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub hard_to_save: BTreeSet<String>,
    /// History source (the descriptor's unless this conversation differs).
    pub history: HistorySource,
    /// Group management actions.
    pub group_actions: GroupActions,
    /// The group's rules as the network states them; `None` = the network did not say (a
    /// direct chat, a group known only from a history import, a network that states no rules).
    /// Skipped from the serialised form when `None`, so a conversation that has none keeps
    /// the [`Self::stable_hash`] it had before the field existed (its stored hash stays equal
    /// and nothing is stated again for it), while every group that has a snapshot gets a new
    /// hash and is stated once at the next start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_info: Option<GroupInfo>,
    /// Set when the user cannot send here.
    pub cannot_send: Option<CannotSend>,
    /// Whether the network LISTS this chat for its roaming settings, the ones it keeps for the
    /// account's other devices (archive, pin, mute, marked unread; [`AccountCaps::roaming_writes`]).
    /// `Some(false)`: the network does not list the chat yet, so a change here has nothing to
    /// attach to and the chat offers none (a message request, a person known only by a number:
    /// the entries appear when the chat is accepted elsewhere and the network lists it).
    /// `None` and `Some(true)`: listed. `None` is "the network says nothing", which is what
    /// every network that states no roaming settings leaves, so a chat keeps the JSON and the
    /// [`Self::stable_hash`] it had before the field existed. It travels with the whole
    /// features value (they are replaced as a whole): a backend that lists or unlists a chat
    /// states the features again. Read it through [`Self::lists_for_roaming`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roaming_listed: Option<bool>,
}

impl ConversationFeatures {
    /// Whether the network lists this chat for its roaming settings
    /// ([`Self::roaming_listed`]); a chat the network said nothing about counts as listed.
    pub fn lists_for_roaming(&self) -> bool {
        self.roaming_listed != Some(false)
    }

    /// Stable hash of the canonical JSON form (`conversations.features_hash`, UI caching).
    pub fn stable_hash(&self) -> u64 {
        // Serializing plain data with string map keys cannot fail; an empty input still
        // hashes deterministically if it ever did.
        let json = serde_json::to_vec(self).unwrap_or_default();
        crate::stable_hash(&json)
    }

    /// Support level for one formatting kind.
    pub fn format_support(&self, kind: FormatKind) -> Support {
        self.formatting.get(&kind).copied().unwrap_or_default()
    }

    /// Whether the conversation takes `timer` as its disappearing-message timer: one of the
    /// offered values (`disappearing`) or a value of a range (`disappearing_custom`), in
    /// whole seconds. `SetConversationTimer` runs this check; a composer's "custom time" input
    /// can run it before. Turning the timer off is not asked here: it is allowed wherever the
    /// conversation has timers at all ([`Self::has_timers`]).
    pub fn takes_timer(&self, timer: Duration) -> bool {
        let secs = timer.as_secs();
        if timer.subsec_nanos() != 0 || secs == 0 {
            return false;
        }
        self.disappearing
            .values()
            .any(|list| list.iter().any(|s| u64::from(*s) == secs))
            || self.disappearing_custom.values().any(|r| r.contains(secs))
    }

    /// Whether some client of the network will not save a file with this name
    /// (the `hard_to_save` extensions). Judged like those clients judge: by the extension at the end
    /// of the name, whatever its case, with dots and white space after it passed over.
    pub fn is_hard_to_save(&self, file_name: &str) -> bool {
        if self.hard_to_save.is_empty() {
            return false;
        }
        let name = file_name.trim_end_matches(|c: char| c == '.' || c.is_whitespace());
        name.rsplit_once('.')
            .is_some_and(|(_, ext)| self.hard_to_save.contains(&ext.to_lowercase()))
    }

    /// Whether the conversation has disappearing-message timers at all (offered values or a
    /// range).
    pub fn has_timers(&self) -> bool {
        self.disappearing.values().any(|list| !list.is_empty())
            || self.disappearing_custom.values().any(|r| r.max_secs > 0)
    }

    /// How a selection of files (in the user's order) is sent here: split into as many
    /// messages as [`Self::file_packing`] needs so that every client shows every file.
    /// `caption`: the text the user wrote to go with them (a blank one counts as none);
    /// `reply`: the send quotes a message. Pure: the core's send and the composer's preview
    /// ("5 files, 3 messages") call the same function, so they never disagree. Whether each
    /// kind can be sent at all, and how large, is [`Self::files`] and is not checked here.
    ///
    /// Neighbours of the same album share a message until it is full; every other file is a
    /// message of its own. Each file is judged by its kind and its MIME type
    /// ([`FileAlbum::takes`]). The caption and the quote go on the first message with files
    /// when each of its kinds takes a caption and the caption fits one message
    /// ([`Self::max_text_bytes`]); else the caption goes first as text of its own
    /// ([`FilePlan::lead_text`]), in as many messages as [`Text::split`] makes of it
    /// ([`FilePlan::caption_messages`]).
    pub fn plan(&self, files: &[PlanFile<'_>], caption: Option<&Text>, reply: bool) -> FilePlan {
        let caption = caption.filter(|t| !t.body.trim().is_empty());
        let max = self.max_text_bytes.and_then(|m| usize::try_from(m).ok());
        let pieces = caption.map(|t| match max {
            Some(max) if t.body.len() > max => t.split(max).len().max(1),
            _ => 1,
        });
        let too_long = caption.is_some_and(|t| max.is_some_and(|max| t.body.len() > max));
        self.plan_with(files, pieces, too_long, reply)
    }

    /// [`Self::plan`] for a caller that knows the kinds of the files but not their types, and
    /// only whether there is a caption (`caption`), not its text: every type counts as
    /// unknown (the kind alone decides, [`FileAlbum::takes`]) and a caption is one message
    /// at most. Use [`Self::plan`] where the types or the caption are at hand.
    pub fn plan_files(&self, kinds: &[AttachmentKind], caption: bool, reply: bool) -> FilePlan {
        let files: Vec<PlanFile<'_>> = kinds.iter().map(|k| PlanFile::new(*k, "")).collect();
        self.plan_with(&files, caption.then_some(1), false, reply)
    }

    /// `pieces`: how many texts the caption is when it goes first (`None`: no caption);
    /// `too_long`: it cannot ride on a message with files.
    fn plan_with(
        &self,
        files: &[PlanFile<'_>],
        pieces: Option<usize>,
        too_long: bool,
        reply: bool,
    ) -> FilePlan {
        let mut messages: Vec<Vec<usize>> = Vec::new();
        // The album of the message being filled and the room left in it.
        let mut open: Option<(usize, usize)> = None;
        for (i, file) in files.iter().enumerate() {
            let album = self.file_packing.album_of(*file);
            match (album, open, messages.last_mut()) {
                (Some((a, _)), Some((o, room)), Some(last)) if a == o && room > 0 => {
                    last.push(i);
                    open = Some((o, room - 1));
                }
                (Some((a, max)), _, _) => {
                    messages.push(vec![i]);
                    open = Some((a, max - 1));
                }
                (None, _, _) => {
                    messages.push(vec![i]);
                    open = None;
                }
            }
        }
        let takes_caption = |i: &usize| {
            files
                .get(*i)
                .and_then(|f| self.files.get(&f.kind))
                .is_some_and(|l| l.caption)
        };
        let caption_fits = messages
            .first()
            .is_some_and(|m| m.iter().all(takes_caption));
        let reply_needs_text = reply && self.file_packing.reply_needs_text;
        let caption = pieces.is_some();
        let lead_text =
            caption && !messages.is_empty() && (!caption_fits || reply_needs_text || too_long);
        FilePlan {
            lead_text,
            caption_messages: if lead_text { pieces.unwrap_or(1) } else { 0 },
            messages,
            reply_unsendable: reply_needs_text && !caption,
        }
    }
}

/// A login flow a network offers ("qr", "pair-code", "id-backup").
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LoginFlow {
    /// The id passed to `StartLogin`.
    pub id: String,
    /// User-facing label.
    pub label: String,
}

/// Typing-indicator timing (Signal: refresh 10 s, stop after 3 s).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypingPolicy {
    /// Resend "typing" this often while the user types.
    #[serde(default)]
    pub refresh: Duration,
    /// Send "stopped" after this much inactivity.
    #[serde(default)]
    pub stop_after: Duration,
}

/// A warning the UI shows, and the user must accept, before logging in (§7.4).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RiskNotice {
    /// Heading.
    pub title: String,
    /// Body text.
    pub body: String,
    /// Link to more information.
    #[serde(default)]
    pub url: Option<String>,
}

/// Which message an own edit names on the wire (architecture §3.5.3). The core always keeps
/// the edit as a new revision under its own id; this says what the request's `target` is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EditTarget {
    /// The latest revision the recipients have: Signal Desktop names the revision it edited
    /// last (`SD/ts/util/sendEditedMessage.preload.ts`), and a retry of an edit names the one
    /// before its own.
    #[default]
    LatestRevision,
    /// The original message, whatever was edited since: WhatsApp (the edit is a protocol
    /// message whose key is the original), Threema and Telegram.
    Original,
}

/// The numbers of a network's outbox (architecture §4.8). The curve of the back-off (none for
/// the first failure, 190 ms, then x1.9) is the core's; the policy gives the caps. The default
/// is Signal Desktop's: the receive windows of the other Signal clients are built on them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default)]
pub struct OutboxPolicy {
    /// How long a message is tried before it fails for good (`Failed`).
    pub budget: Duration,
    /// The longest wait between two attempts.
    pub max_backoff: Duration,
    /// How long a stored challenge ("solve this to send") can still be answered.
    pub challenge_lifetime: Duration,
}

impl OutboxPolicy {
    /// Signal Desktop's numbers: 24 h, 15 min, 24 h.
    pub const SIGNAL: Self = Self {
        budget: Duration::from_secs(24 * 3600),
        max_backoff: Duration::from_secs(15 * 60),
        challenge_lifetime: Duration::from_secs(24 * 3600),
    };
}

impl Default for OutboxPolicy {
    fn default() -> Self {
        Self::SIGNAL
    }
}

/// What a network's SERVER decides, where most networks leave it to the clients (KT-2:
/// Telegram). Every flag off is how the core has always worked; the core keeps its promises
/// under either setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerRules {
    /// The server counts what is unread and says so
    /// ([`ConversationPatch::server_unread`](crate::ConversationPatch::server_unread) and
    /// `server_mentions`): the core shows the stated count instead of counting rows, and the
    /// network states it at every change. Off: the count is the rows' (plus `unread_hint`), and
    /// a stated count is ignored.
    #[serde(default)]
    pub owns_unread: bool,
    /// The server checks a delete for everyone before it tells a client: the core applies
    /// `BackendEvent::Deleted` of such a network without its own author, admin and window checks,
    /// and a delete that names nobody (`by: None`) reads
    /// [`Deletion::Unattributed`](crate::Deletion::Unattributed), not "deleted by me". Off:
    /// today's rules (the author, an admin of a group, the receive window).
    #[serde(default)]
    pub validates_deletes: bool,
}

/// Static description of a network (§3.8 tier 1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkDescriptor {
    /// Name shown to users ("Signal").
    pub display_name: String,
    /// Freedesktop icon name.
    pub icon_name: String,
    /// Offered login flows, preferred first.
    #[serde(default)]
    pub login_flows: Vec<LoginFlow>,
    /// Identifier kinds accepted to start a chat.
    #[serde(default)]
    pub start_chat: Vec<IdentifierKind>,
    /// Default history source for conversations.
    #[serde(default)]
    pub history: HistorySource,
    /// What the `read` flag of a history row of this network means (W4 fix F1).
    #[serde(default)]
    pub history_read: HistoryReadState,
    /// How message ids are scoped.
    #[serde(default)]
    pub msg_id_scope: MsgIdScope,
    /// Typing-indicator timing.
    #[serde(default)]
    pub typing: TypingPolicy,
    /// Shown before login when set: the network is one the user should be warned about. The
    /// UI asks first (a page the user must accept) and writes its own translated text for a
    /// network it knows; this text is only the fallback for a network it has no words for.
    #[serde(default)]
    pub risk_notice: Option<RiskNotice>,
    /// A fresh link can bring the account's history as ONE archive the phone uploads
    /// (`BackendEvent::HistoryTransfer`), and a UI offers "Transfer History from Phone…"
    /// (remove the account and link it anew) for its accounts. Signal sets it together with
    /// the offer (`with_history_transfer_offer`); a network that pushes history batches
    /// (WhatsApp, [`HistorySource::PushedOnLink`]) does not: for it that action would be a
    /// link and unlink cycle, itself a risk for the account.
    #[serde(default)]
    pub history_transfer: bool,
    /// Off until the user turns it on (architecture §7.4): `kourier-app` registers the
    /// network only while its switch is on (a preference read at the start), and a UI lists
    /// it as "Off · experimental" while it is off. Signal and the plain fake say `false`;
    /// WhatsApp, Telegram and Threema say `true`.
    #[serde(default)]
    pub experimental: bool,
    /// Which message an own edit names on the wire.
    #[serde(default)]
    pub edit_target: EditTarget,
    /// The outbox's budget, back-off cap and challenge lifetime for this network.
    #[serde(default)]
    pub outbox: OutboxPolicy,
    /// How long the core waits for an account's actor to stop on its own at a quit (after
    /// `Command::Shutdown`, then again after the actor's token is cancelled), when this
    /// network needs more than the core's default (`Timeouts::shutdown_grace`, 5 s; `None`).
    /// WhatsApp states 20 s (W4, R-Q1): its actor waits up to 15 s for a history chunk the
    /// phone announced and has not finished sending, which the phone sends only once. The
    /// application's own limit for the whole core follows the largest grace of the
    /// registered networks.
    #[serde(default)]
    pub shutdown_grace: Option<Duration>,
    /// The network has safety numbers a person page offers (P5a, `Command::SafetyNumber`):
    /// Signal and the Signal-like fake say `true`, WhatsApp (whose security code is a later
    /// item) `false`. A static capability, so a UI decides whether the row is shown without
    /// asking the backend; what the number is, or why there is none, the backend answers.
    #[serde(default)]
    pub safety_numbers: bool,
    /// The network can SEND a contact card (P5c, `UiCommand::SendContact`; the lead's SC4):
    /// Signal and the Signal-like fake say `true`, WhatsApp (whose vCard send is a later item)
    /// `false`. A UI shows "Share Contact…" only when it is on, and the core refuses the
    /// command otherwise. Receiving cards is not this flag (the kinds step).
    #[serde(default)]
    pub contact_cards: bool,
    /// The pictures this network's backend cleans before they leave (LOC, F14, F27): the bytes
    /// that leave this PC carry no location. A backend may state it ONLY when its send path runs
    /// every picture of these types through `kourier_media::clean_still` (an xtask source check
    /// holds that; the fake, which sends nothing off the PC, is its one named exception). A
    /// network that strips on its server does not qualify. `None` = nothing is cleaned: every
    /// located file asks. Signal and the Signal-like fake state JPEG, PNG, WebP and BMP.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleaned_pictures: Option<CleanedPictures>,
    /// The network's accounts always send read receipts, typing and online status: no setting
    /// turns them off (Telegram: its API rules, HA TG6). A UI says so in its privacy settings
    /// (KT-1b). Signal, WhatsApp, Threema and the fakes say `false`.
    #[serde(default)]
    pub receipts_always_sent: bool,
    /// What the network's server owns (KT-2, [`ServerRules`]). All off for Signal, WhatsApp,
    /// Threema and the fakes that play them, and for the real Telegram until its backend maps
    /// onto it.
    #[serde(default)]
    pub server: ServerRules,
}

/// What a network's backend cleans of the location (LOC, [`NetworkDescriptor::cleaned_pictures`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanedPictures {
    /// The MIME types it cleans (`image/jpeg` ...).
    pub types: BTreeSet<String>,
    /// The largest picture it cleans; `None` = no bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
}

impl CleanedPictures {
    /// The types `kourier_media::clean_still` cleans (`CLEANED_TYPES`), with no size bound:
    /// what Signal states.
    pub fn stills(types: &[&str]) -> Self {
        Self {
            types: types.iter().map(|t| (*t).to_owned()).collect(),
            max_bytes: None,
        }
    }

    /// Whether a picture of this type and size is cleaned.
    pub fn cleans(&self, mime: &str, size: u64) -> bool {
        self.types.contains(mime) && self.max_bytes.is_none_or(|max| size <= max)
    }
}

/// Dynamic account capabilities from server config (§3.8 tier 2), sent as `AccountCaps`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountCaps {
    /// Largest attachment we may send (Signal `global.attachments.maxBytes`).
    pub max_attachment_bytes: Option<u64>,
    /// Largest attachment we accept (Signal `global.attachments.maxReceiveBytes`).
    pub max_receive_attachment_bytes: Option<u64>,
    /// Bodies longer than this are sent as a long-text attachment (Signal 2 KiB).
    pub long_text_bytes: Option<u32>,
    /// Delete-for-everyone send window from server config.
    pub delete_for_everyone_max_age_secs: Option<u32>,
    /// How many chats the account may pin at most; `None` = the network states no limit. A
    /// network with a limit states it (the common limit is 4; a network may have its own): the
    /// core assumes none and refuses no pin for a count while this is `None`. Counted by the
    /// network per account, over every chat it lists as pinned, the ones this device does not
    /// show included.
    pub max_pinned_chats: Option<u32>,
    /// Named feature flags.
    pub flags: BTreeMap<String, bool>,
}

impl AccountCaps {
    /// Flag: the network can WRITE the settings it keeps for the account's other devices
    /// (archive, pin, mute, marked unread: a "roaming" setting), and whether this account
    /// lets it. Three states, read through [`Self::roaming_writes`]: ABSENT = the network
    /// cannot (the entries are hidden), `false` = it can but this account's switch is off (the
    /// entries are shown greyed, with the reason), `true` = on. Stated by the backend of the
    /// account, never by the core; changed by `Command::SetRoamingWrites`.
    pub const FLAG_ROAMING_WRITES: &'static str = "roaming_writes";

    /// Flag: the account's other device can be asked to upload again the files of a message
    /// the network no longer has (`Command::RequestAttachments`; Signal: attachment
    /// backfill). Without it a file the network lost stays lost.
    pub const FLAG_ATTACHMENT_RESTORE: &'static str = "attachment_restore";

    /// Whether [`Self::FLAG_ATTACHMENT_RESTORE`] is set.
    pub fn restores_attachments(&self) -> bool {
        self.flags
            .get(Self::FLAG_ATTACHMENT_RESTORE)
            .copied()
            .unwrap_or(false)
    }

    /// [`Self::FLAG_ROAMING_WRITES`] in its three states: `None` = the network cannot write
    /// roaming settings (hide the entries), `Some(false)` = it can and this account's switch is
    /// off (show them greyed, with the reason), `Some(true)` = on.
    pub fn roaming_writes(&self) -> Option<bool> {
        self.flags.get(Self::FLAG_ROAMING_WRITES).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_order_and_levels() {
        assert!(Support::Rejected < Support::Dropped && Support::Partial < Support::Full);
        assert_eq!(Support::Rejected.level(), -2);
        assert_eq!(Support::Full.level(), 2);
        assert!(Support::Partial.is_usable() && !Support::Unsupported.is_usable());
    }

    #[test]
    fn window_limits() {
        let w = Window {
            max_age_secs: Some(86_400),
            max_count: Some(10),
        };
        assert!(w.allows(86_400, 9));
        assert!(!w.allows(86_401, 0));
        assert!(!w.allows(0, 10));
        assert!(Window::default().allows(u64::MAX, u16::MAX));
    }

    #[test]
    fn default_features_are_restrictive_and_hash_is_stable() {
        let f = ConversationFeatures::default();
        assert_eq!(f.reactions, Support::Unsupported);
        assert_eq!(f.format_support(FormatKind::Bold), Support::Unsupported);
        assert!(f.cannot_send.is_none() && !f.typing);
        let mut g = f.clone();
        assert_eq!(f.stable_hash(), g.stable_hash());
        g.formatting.insert(FormatKind::Bold, Support::Full);
        g.formatting.insert(FormatKind::Spoiler, Support::Full);
        let mut h = f.clone();
        h.formatting.insert(FormatKind::Spoiler, Support::Full);
        h.formatting.insert(FormatKind::Bold, Support::Full);
        assert_ne!(f.stable_hash(), g.stable_hash());
        assert_eq!(
            g.stable_hash(),
            h.stable_hash(),
            "insertion order does not matter"
        );
    }

    fn packing_features() -> ConversationFeatures {
        use AttachmentKind::{Audio, File, Gif, Image, Video, Voice};
        let with = |caption| FileLimits {
            max_size: 0,
            caption,
        };
        ConversationFeatures {
            files: [Image, Gif, Video, Audio, File]
                .into_iter()
                .map(|k| (k, with(true)))
                .chain([(Voice, with(false))])
                .collect(),
            file_packing: FilePacking {
                albums: vec![FileAlbum {
                    kinds: [Image, Gif, Video].into(),
                    max: 3,
                    ..FileAlbum::default()
                }],
                reply_needs_text: false,
            },
            ..ConversationFeatures::default()
        }
    }

    #[test]
    fn files_split_into_albums_in_the_users_order() {
        use AttachmentKind::{File, Image, Video, Voice};
        let f = packing_features();
        // Seven pictures: albums of three, the rest in a last one.
        let plan = f.plan_files(&[Image; 7], true, true);
        assert_eq!(
            plan.messages,
            [vec![0, 1, 2], vec![3, 4, 5], vec![6]],
            "{plan:?}"
        );
        assert!(!plan.lead_text && !plan.reply_unsendable);
        assert_eq!(plan.message_count(), 3);
        // Other files travel alone and never move across a neighbour: the order is the user's.
        let plan = f.plan_files(&[Image, Video, File, File, Image, Image], false, false);
        assert_eq!(
            plan.messages,
            [vec![0, 1], vec![2], vec![3], vec![4, 5]],
            "{plan:?}"
        );
        assert_eq!(plan.message_count(), 4);
        // A first message that takes no caption: the caption goes first, as a text.
        let plan = f.plan_files(&[Voice, Image], true, true);
        assert!(plan.lead_text && !plan.reply_unsendable);
        assert_eq!(plan.caption_messages, 1);
        assert_eq!(plan.messages, [vec![0], vec![1]]);
        assert_eq!(plan.message_count(), 3);
        assert!(!f.plan_files(&[Voice, Image], false, true).lead_text);
        // Nothing picked: nothing to send, with or without a caption.
        assert_eq!(f.plan_files(&[], true, false).message_count(), 0);
        // Every position appears exactly once, whatever the mix and the count.
        let many: Vec<AttachmentKind> = (0..1000)
            .map(|i| [Image, File, Video, Voice, Image][i % 5])
            .collect();
        let plan = f.plan_files(&many, false, false);
        let flat: Vec<usize> = plan.messages.iter().flatten().copied().collect();
        assert_eq!(flat, (0..1000).collect::<Vec<_>>());
        assert!(plan.messages.iter().all(|m| m.len() <= 3));
    }

    #[test]
    fn without_a_packing_rule_every_file_travels_alone() {
        use AttachmentKind::Image;
        let mut f = packing_features();
        f.file_packing = FilePacking::default();
        assert_eq!(
            f.plan_files(&[Image; 3], true, false).messages,
            [vec![0], vec![1], vec![2]]
        );
        // An album of one (or of none) is no album.
        for max in [0, 1] {
            f.file_packing.albums = vec![FileAlbum {
                kinds: [Image].into(),
                max,
                ..FileAlbum::default()
            }];
            assert_eq!(f.plan_files(&[Image; 2], false, false).message_count(), 2);
        }
        // A kind listed twice belongs to the first album that lists it.
        f.file_packing.albums = vec![
            FileAlbum {
                kinds: [Image].into(),
                max: 2,
                ..FileAlbum::default()
            },
            FileAlbum {
                kinds: [Image].into(),
                max: 9,
                ..FileAlbum::default()
            },
        ];
        assert_eq!(f.plan_files(&[Image; 4], false, false).message_count(), 2);
    }

    #[test]
    fn a_quote_files_cannot_carry_rides_on_the_caption() {
        use AttachmentKind::Image;
        let mut f = packing_features();
        f.file_packing.reply_needs_text = true;
        let plan = f.plan_files(&[Image, Image], true, true);
        assert!(plan.lead_text && !plan.reply_unsendable, "{plan:?}");
        assert_eq!(plan.message_count(), 2);
        let plan = f.plan_files(&[Image, Image], false, true);
        assert!(plan.reply_unsendable && !plan.lead_text, "{plan:?}");
        // Without a reply the caption stays on the pictures.
        assert!(!f.plan_files(&[Image], true, false).lead_text);
    }

    #[test]
    fn packing_survives_serde_and_is_part_of_the_hash() {
        let f = packing_features();
        let json = serde_json::to_string(&f).expect("serialize");
        let back: ConversationFeatures = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, f);
        let mut g = f.clone();
        g.file_packing.albums[0].max = 32;
        assert_ne!(f.stable_hash(), g.stable_hash());
        // Features stored before the field existed: every file alone.
        let old: ConversationFeatures = serde_json::from_str(r#"{"typing":true}"#).expect("old");
        assert_eq!(old.file_packing, FilePacking::default());
    }

    /// A network states which formats its clients show together (Signal: HEIC and HEIF
    /// pictures travel alone, MP4 clips may share an album with pictures).
    #[test]
    fn an_album_is_stated_by_kind_and_by_type() {
        use AttachmentKind::{File, Gif, Image, Video};
        let mut f = packing_features();
        f.file_packing.albums = vec![FileAlbum {
            kinds: [Image, Gif].into(),
            types: ["video/mp4".to_owned()].into(),
            except: ["image/heic".to_owned(), "image/heif".to_owned()].into(),
            max: 4,
        }];
        let album = &f.file_packing.albums[0];
        assert!(album.takes(Image, "image/jpeg") && album.takes(Gif, "image/gif"));
        assert!(album.takes(Video, "video/mp4") && album.takes(Video, "Video/MP4; codecs=avc1"));
        assert!(!album.takes(Video, "video/quicktime") && !album.takes(File, "application/pdf"));
        assert!(!album.takes(Image, "image/heic") && !album.takes(Image, " IMAGE/HEIF "));
        // A type the caller does not know: the kind decides.
        assert!(album.takes(Image, "") && !album.takes(Video, ""));
        // `except` wins over `types`.
        let odd = FileAlbum {
            types: ["image/heic".to_owned()].into(),
            except: ["image/heic".to_owned()].into(),
            max: 4,
            ..FileAlbum::default()
        };
        assert!(!odd.takes(Image, "image/heic"));

        let file = PlanFile::new;
        let selection = [
            file(Image, "image/jpeg"),
            file(Video, "video/mp4"),
            file(Image, "image/heic"),
            file(Image, "image/png"),
            file(Video, "video/quicktime"),
            file(Video, "video/mp4"),
            file(Image, "image/webp"),
        ];
        let plan = f.plan(&selection, None, false);
        assert_eq!(
            plan.messages,
            [vec![0, 1], vec![2], vec![3], vec![4], vec![5, 6]],
            "{plan:?}"
        );
        // Without types the same kinds pack by kind alone: every video travels alone.
        let kinds: Vec<AttachmentKind> = selection.iter().map(|s| s.kind).collect();
        assert_eq!(
            f.plan_files(&kinds, false, false).messages,
            [vec![0], vec![1], vec![2, 3], vec![4], vec![5], vec![6]]
        );
        // The new fields are part of the hash, and absent from the stored form while unused.
        let plain = packing_features();
        assert_ne!(plain.stable_hash(), f.stable_hash());
        let json = serde_json::to_string(&plain).expect("serialize");
        assert!(
            !json.contains("types") && !json.contains("except"),
            "{json}"
        );
        let back: ConversationFeatures =
            serde_json::from_str(&serde_json::to_string(&f).expect("serialize")).expect("back");
        assert_eq!(back, f);
    }

    /// The plan counts every message of the send: a caption longer than one message of the
    /// chat carries goes first, as the texts it is split into.
    #[test]
    fn the_plan_counts_the_texts_of_a_long_caption() {
        use AttachmentKind::{Image, Voice};
        let mut f = packing_features();
        f.max_text_bytes = Some(10);
        let pictures = [PlanFile::new(Image, "image/jpeg"); 4];
        let short = Text::plain("short");
        let plan = f.plan(&pictures, Some(&short), false);
        assert!(!plan.lead_text);
        assert_eq!((plan.caption_messages, plan.message_count()), (0, 2));
        let long = Text::plain("one two three four five six seven");
        let pieces = long.split(10).len();
        assert!(pieces > 2, "{pieces}");
        let plan = f.plan(&pictures, Some(&long), true);
        assert!(plan.lead_text && !plan.reply_unsendable);
        assert_eq!(plan.caption_messages, pieces);
        assert_eq!(plan.message_count(), 2 + pieces);
        // A caption that leads for another reason is one message when it fits.
        let voice = [PlanFile::new(Voice, "audio/aac")];
        let plan = f.plan(&voice, Some(&short), false);
        assert_eq!((plan.lead_text, plan.caption_messages), (true, 1));
        // A blank caption is none; no files, nothing to send.
        let plan = f.plan(&pictures, Some(&Text::plain("  \n ")), false);
        assert_eq!((plan.lead_text, plan.caption_messages), (false, 0));
        assert_eq!(f.plan(&[], Some(&long), false).message_count(), 0);
        // Without the text a caption counts as one message at most.
        let plan = f.plan_files(&[Voice], true, false);
        assert_eq!((plan.lead_text, plan.caption_messages), (true, 1));
        // No limit on the text: a long caption stays on the pictures.
        f.max_text_bytes = None;
        assert_eq!(f.plan(&pictures, Some(&long), false).caption_messages, 0);
    }

    #[test]
    fn a_file_some_client_will_not_save_is_told_by_its_extension() {
        let features = ConversationFeatures {
            hard_to_save: ["exe".to_owned(), "apk".to_owned()].into(),
            ..ConversationFeatures::default()
        };
        for name in [
            "setup.exe",
            "Setup.EXE",
            "app.apk.",
            "tool.exe  ",
            "a.b.apk",
        ] {
            assert!(features.is_hard_to_save(name), "{name}");
        }
        for name in ["notes.txt", "exe", "exe.txt", "photo.exe.jpg", "", "."] {
            assert!(!features.is_hard_to_save(name), "{name}");
        }
        // Nothing stated: nothing is marked, and the stored form (so the hash) is unchanged.
        let plain = ConversationFeatures::default();
        assert!(!plain.is_hard_to_save("setup.exe"));
        let json = serde_json::to_string(&plain).expect("json");
        assert!(!json.contains("hard_to_save"));
        let stated: ConversationFeatures =
            serde_json::from_str(&serde_json::to_string(&features).expect("json")).expect("back");
        assert_eq!(stated, features);
    }

    #[test]
    fn timers_are_offered_values_or_a_value_of_a_range() {
        let mut f = ConversationFeatures::default();
        assert!(!f.has_timers() && !f.takes_timer(Duration::from_secs(30)));
        f.disappearing
            .insert(DisappearKind::AfterRead, vec![30, 3600]);
        assert!(f.has_timers());
        assert!(f.takes_timer(Duration::from_secs(30)) && f.takes_timer(Duration::from_secs(3600)));
        assert!(
            !f.takes_timer(Duration::from_secs(45)),
            "not offered, no range"
        );
        f.disappearing_custom.insert(
            DisappearKind::AfterRead,
            TimerRange {
                min_secs: 1,
                max_secs: 4 * 7 * 24 * 3600,
                step_secs: 1,
            },
        );
        assert!(f.takes_timer(Duration::from_secs(45)) && f.takes_timer(Duration::from_secs(1)));
        assert!(f.takes_timer(Duration::from_secs(4 * 7 * 24 * 3600)));
        assert!(!f.takes_timer(Duration::from_secs(4 * 7 * 24 * 3600 + 1)));
        assert!(!f.takes_timer(Duration::ZERO), "off is not a timer");
        assert!(!f.takes_timer(Duration::from_millis(1500)), "whole seconds");
        // A step: minutes from five minutes on.
        let minutes = TimerRange {
            min_secs: 300,
            max_secs: 3600,
            step_secs: 60,
        };
        assert!(minutes.contains(300) && minutes.contains(360) && minutes.contains(3600));
        assert!(!minutes.contains(330) && !minutes.contains(240) && !minutes.contains(3660));
        assert!(!TimerRange::default().contains(0) && !TimerRange::default().contains(1));
        // A range alone gives a chat timers; the field is absent from the stored form unused.
        let mut only_range = ConversationFeatures::default();
        only_range
            .disappearing_custom
            .insert(DisappearKind::AfterSend, minutes);
        assert!(only_range.has_timers());
        let plain = serde_json::to_string(&ConversationFeatures::default()).expect("json");
        assert!(!plain.contains("disappearing_custom"), "{plain}");
        let back: ConversationFeatures =
            serde_json::from_str(&serde_json::to_string(&f).expect("json")).expect("back");
        assert_eq!(back, f);
    }

    #[test]
    fn features_tolerate_missing_and_unknown_fields() {
        let f: ConversationFeatures =
            serde_json::from_str(r#"{"typing":true,"future_field":1}"#).expect("deserialize");
        assert!(f.typing);
        assert_eq!(f.history, HistorySource::None);
    }
}
