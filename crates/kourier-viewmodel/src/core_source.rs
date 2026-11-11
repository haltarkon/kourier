//! [`CoreSource`]: the UI data flow over the core facade (§8.5).
//!
//! The Qt layer feeds it every `UiEvent` and calls its commands; it answers with [`Changes`]
//! (which models to refresh) and builds rows from the latest query results on demand. It never
//! blocks and never touches a database: it only sends `UiCommand`s through a [`CommandSink`].
//!
//! Queries follow §8.5. Each model (accounts, conversation list, the open conversation's
//! header and its timeline) keeps one query in flight; invalidations that arrive meanwhile are
//! coalesced into one follow-up query. Every [`QueryToken`] carries the model's monotonic
//! generation, and a result is applied only if its generation is at least the last applied one
//! and not older than the model's floor (a conversation switch raises the floor, so results for
//! the previous conversation are dropped whenever they arrive).
//!
//! The timeline is a window of at most [`TIMELINE_WINDOW`] rows, loaded [`TIMELINE_PAGE`] at a
//! time: newest first, then older pages as the user scrolls up (keyset paging by
//! `(sort_key, id)`); a window that outgrows the limit drops rows at the far end and pages them
//! back in on the way down.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use kourier_core::facade::Progress;
use kourier_core::facade::{
    AccountRow as CoreAccount, ActionSubject, BackendNotice, ConversationCursor,
    ConversationDetail, ConversationListQuery, ConversationRow as CoreConversation, CoreError,
    ExpiryPolicy, FilePlanQuery, MessageActions, MessageRevisions, MessageRow as CoreMessage,
    OriginKind, Query, QueryData, QueryToken, QuoteState as CoreQuoteState, Refusal, RefusedFiles,
    RequestId, TimelineAnchor, TimelineCursor, TimelinePage, TimelineQuery, UiCommand, UiEvent,
};
use kourier_model::{
    AccountHold, AccountId, AccountState, AttachmentId, AttachmentKind, AttachmentState, Body,
    ConnState, ConvKind, ConversationChange, ConversationId, ErrorKind, FailReason,
    HistoryTransfer, LoginStep, LoginStepKind, MessageId, NetworkKind, OutgoingState, ReadState,
    Redacted, SendPhase, Text, UserAction, UserId,
};

use crate::account_actions::{AccountAction, LogoutNotice};
use crate::conversations::{
    AccountRow, AccountStatus, AccountTable, Banner, BannerContext, BannerKind, Banners,
    ChatHeader, ConversationFilter, ConversationRow, Hidden, PreviewSource, TableAccount,
    account_banner, history_lost_banner, hold_banner, network_icon, network_missing_banner,
    network_name, network_off_banner, network_tint, shown_archived, timer_menu, timer_secs,
    timer_span, typing_label,
};
use crate::edit::EditForm;
use crate::files::{ArchiveRule, PlanAnswer, PlanRequest, StripRules, refused_notice};
use crate::i18n::{trc, trfc};
use crate::login::{LoginView, LoginViewKind, NetworkChoice, NetworkMissing, login_view};
use crate::media::{self, MediaVariant};
use crate::names::{NameMap, Names, fallback_name, first_name, single_line};
use crate::preview::{self, Summary};
use crate::readmore::ReadMore;
use crate::roaming::{self, AccountFacts, ChatOffer, ChatTold};
use crate::source::{
    EditTarget, FinishedSend, GroupUpdate, MediaAnswer, PersonUpdate, RefusedSend, ReplaceNews,
    ReturnedFile, RevisionList, RevisionRow, SaveAnswer, SaveFailure, SaveOutcome, SendOutcome,
    SourceError, UiSource,
};
use crate::time::{Calendar, TimeContext};
use crate::timeline::{
    self, Block, Item, MessageRow, QuoteBlock, QuoteState, ReactionChip, Receipt, RowActions,
};
use crate::transfer::{
    AbandonQuestion, QuitWork, ReplaceQuestion, TransferBook, abandon_question, quit_question,
    replace_question, replace_question_cancelling,
};

/// Query model id of the accounts drawer.
pub const MODEL_ACCOUNTS: u32 = 1;
/// Query model id of the conversation list.
pub const MODEL_CONVERSATIONS: u32 = 2;
/// Query model id of the open conversation's header.
pub const MODEL_CONVERSATION: u32 = 3;
/// Query model id of the open timeline.
pub const MODEL_TIMELINE: u32 = 4;
/// Query model id of the composer's file plan.
pub const MODEL_FILE_PLAN: u32 = 5;
/// Query model id of a message's edit history.
pub const MODEL_REVISIONS: u32 = 6;
/// Query model id of the core's own settings (what the settings page chooses there).
pub const MODEL_SETTINGS: u32 = 7;
/// Query model id of the quiet-notices choice (the settings page's other core setting; a
/// tracker of its own, so a choice of one setting never drops the answer of the other).
pub const MODEL_QUIET: u32 = 8;
/// Query model id of the person's page in the details panel.
pub const MODEL_PERSON: u32 = 9;
/// Query model id of the group's page in the details panel.
pub const MODEL_GROUP: u32 = 10;
/// Query model id of the safety-number level of the details panel (P5a; after the five
/// gallery slots, `MODEL_CHAT_MEDIA` 11-15).
pub const MODEL_SAFETY: u32 = 16;
/// The facts a saved file's name is made of (F3b): one query per file, none of them coalesced.
pub const MODEL_SAVE: u32 = 17;
/// What a chat's header needs, read ahead of the chat being opened ([`READ_AHEAD_ROWS`]).
pub const MODEL_READ_AHEAD: u32 = 18;
/// Token model id of a copy action's person.
pub const MODEL_COPY: u32 = 19;
pub use media_slots::MODEL_CHAT_MEDIA;

/// How many rows from the top of the list have their chat's features read ahead, so the chat
/// opens with its composer already right. Rows with features already seen cost nothing.
const READ_AHEAD_ROWS: usize = 60;
/// Chats whose details are remembered for the next time they are opened.
const KNOWN_DETAILS: usize = 256;

/// Rows per timeline page.
pub const TIMELINE_PAGE: u32 = 50;
/// Most timeline rows kept (§1.3: no message caches beyond the visible window).
pub const TIMELINE_WINDOW: usize = 500;
/// Conversations read per query for the list. A page size, not a limit: the list reads page
/// after page until it has every conversation (an account with thousands of chats shows all
/// of them).
pub const CONVERSATION_PAGE: u32 = 500;
/// How often in a row a read of the list over several pages starts over because the list
/// changed under it, before one pass is read to its end and shown regardless (and read again
/// right after). Without the bound a steady stream of changes, such as a sync after linking,
/// would keep a long list from ever showing.
pub const LIST_RESTARTS: u8 = 2;
/// A typing "on" is repeated at most this often while the user keeps typing.
pub const TYPING_REFRESH_MS: i64 = 5_000;

/// Where commands go: the app handle in the UI, a recorder in tests. `false` when the core has
/// stopped.
pub trait CommandSink {
    /// Sends one command without blocking.
    fn send(&mut self, cmd: UiCommand) -> bool;
}

impl<F: FnMut(UiCommand) -> bool> CommandSink for F {
    fn send(&mut self, cmd: UiCommand) -> bool {
        self(cmd)
    }
}

/// The bytes a backend reported for one file of an own message while its attempt uploads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct UploadBytes {
    /// The message the file belongs to.
    message: MessageId,
    /// Plaintext bytes up.
    done: u64,
    /// Plaintext bytes this attempt uploads for the file.
    total: u64,
}

/// Which way the open chat moved between the inbox and the archive (Signal Desktop's three
/// rules, `conversations.preload.ts` `conversationsUpdated`): the front-end acts on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveMove {
    /// Archive -> inbox (a new message brought the chat back, or the phone pinned it): the
    /// list goes back to the inbox.
    ToInbox,
    /// Inbox -> archive (the phone archived the chat that is open): the chat closes.
    ToArchive,
}

/// What a model refresh must pick up after an event or a command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// The accounts drawer (and whether any account exists).
    pub accounts: bool,
    /// The conversation list.
    pub conversations: bool,
    /// The open timeline's rows.
    pub timeline: bool,
    /// Only the lines under the open timeline's messages on their way count time
    /// ([`UiSource::wait_lines`]); the rows are not rebuilt.
    pub waits: bool,
    /// Only the rings on the files of the open timeline's own messages that wait or upload
    /// moved ([`UiSource::upload_rows`]); the rows are not rebuilt.
    pub uploads: bool,
    /// The open conversation's header (or it was closed).
    pub header: bool,
    /// The login page.
    pub login: bool,
    /// The composer notice.
    pub notice: bool,
    /// The open conversation's stored draft became known (restore it into an empty composer).
    pub draft: bool,
    /// The open conversation was merged into this one (§3.3): the chat switches to it.
    pub switch_to: Option<ConversationId>,
    /// The open chat moved between the inbox and the archive (a new message brought it back,
    /// or the phone archived it) in the list read just applied.
    pub archive_move: Option<ArchiveMove>,
    /// Answers to `OpenMedia` arrived ([`UiSource::take_media`]).
    pub media: bool,
    /// Answers to the save calls arrived ([`UiSource::take_saves`]).
    pub saves: bool,
    /// Scroll to this message of the open timeline and highlight it (a quote was activated;
    /// the message is among the rows now).
    pub jump: Option<MessageId>,
    /// The composer's files changed from the core's side: files of a `SendFiles` came back
    /// ([`UiSource::take_refused_files`]), a send ended ([`UiSource::take_finished_sends`]),
    /// or what is being prepared for sending changed ([`UiSource::preparing_files`],
    /// [`UiSource::prepared_files`]).
    pub files: bool,
    /// The answer to a file plan request arrived ([`UiSource::take_file_plan`]).
    pub plan: bool,
    /// The versions of a message arrived ([`UiSource::take_revisions`]).
    pub revisions: bool,
    /// The person's page changed or ended ([`UiSource::take_person`]).
    pub person: bool,
    /// A copy action is answered ([`UiSource::take_copy`]).
    pub copy: bool,
    /// The group's page changed or ended ([`UiSource::take_group`]).
    pub group: bool,
    /// The safety-number level has a new answer ([`UiSource::take_safety`]).
    pub safety: bool,
    /// A gallery slot's rows changed ([`UiSource::take_chat_media`]).
    pub chat_media: bool,
    /// A setting the core keeps became known or changed ([`UiSource::expiry_policy`],
    /// [`UiSource::quiet_notices`]).
    pub settings: bool,
    /// Conversations were merged ([`UiSource::take_merges`]): what waits for a merged-away
    /// chat (the files in its composer) goes on waiting for the one that is left.
    pub merged: bool,
    /// The history transfer of an account changed: the banner ([`UiSource::banner`]), which
    /// accounts offer a transfer ([`UiSource::accounts`]), or a notice waits
    /// ([`UiSource::take_transfer_notices`]).
    pub transfer: bool,
    /// A notice about the device of a removed or unlinked account waits
    /// ([`UiSource::take_logout_notices`]).
    pub account_notice: bool,
    /// A short notice about a change of a chat or of the sync switch waits
    /// ([`UiSource::take_told`]).
    pub told: bool,
}

impl Changes {
    /// Everything visible.
    pub const ALL: Self = Self {
        accounts: true,
        conversations: true,
        timeline: true,
        waits: false,
        uploads: false,
        header: true,
        login: true,
        notice: true,
        draft: false,
        switch_to: None,
        archive_move: None,
        media: false,
        saves: false,
        jump: None,
        files: false,
        plan: false,
        revisions: false,
        person: false,
        copy: false,
        group: false,
        safety: false,
        chat_media: false,
        settings: false,
        merged: false,
        transfer: true,
        account_notice: true,
        told: false,
    };

    /// Whether anything changed.
    pub fn any(&self) -> bool {
        self.accounts
            || self.conversations
            || self.timeline
            || self.waits
            || self.uploads
            || self.header
            || self.login
            || self.notice
            || self.draft
            || self.switch_to.is_some()
            || self.archive_move.is_some()
            || self.media
            || self.saves
            || self.jump.is_some()
            || self.files
            || self.plan
            || self.revisions
            || self.person
            || self.copy
            || self.group
            || self.safety
            || self.chat_media
            || self.settings
            || self.merged
            || self.transfer
            || self.told
    }

    /// Adds `other`.
    pub fn merge(&mut self, other: Changes) {
        self.accounts |= other.accounts;
        self.conversations |= other.conversations;
        self.timeline |= other.timeline;
        self.waits |= other.waits;
        self.uploads |= other.uploads;
        self.header |= other.header;
        self.login |= other.login;
        self.notice |= other.notice;
        self.draft |= other.draft;
        self.media |= other.media;
        self.saves |= other.saves;
        self.files |= other.files;
        self.plan |= other.plan;
        self.revisions |= other.revisions;
        self.person |= other.person;
        self.copy |= other.copy;
        self.group |= other.group;
        self.safety |= other.safety;
        self.chat_media |= other.chat_media;
        self.settings |= other.settings;
        self.merged |= other.merged;
        self.transfer |= other.transfer;
        self.account_notice |= other.account_notice;
        self.told |= other.told;
        if other.switch_to.is_some() {
            self.switch_to = other.switch_to;
        }
        if other.archive_move.is_some() {
            self.archive_move = other.archive_move;
        }
        if other.jump.is_some() {
            self.jump = other.jump;
        }
    }
}

/// Whether to apply a query result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    /// Newest so far: apply it.
    Apply,
    /// Older than what is shown, or for data the model switched away from: drop it.
    Stale,
}

/// The query bookkeeping of one model (§8.5): generations, one query in flight, coalesced
/// follow-ups and a floor for switches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryTracker {
    model: u32,
    next: u64,
    in_flight: Option<u64>,
    dirty: bool,
    applied: u64,
    floor: u64,
}

impl QueryTracker {
    /// A tracker for `model`.
    pub fn new(model: u32) -> Self {
        Self {
            model,
            next: 1,
            in_flight: None,
            dirty: false,
            applied: 0,
            floor: 0,
        }
    }

    /// Wants fresh data: the token of a new query, or `None` while one is in flight (the
    /// request is remembered and [`Self::follow_up`] issues it once that one completes).
    pub fn request(&mut self) -> Option<QueryToken> {
        if self.in_flight.is_some() {
            self.dirty = true;
            return None;
        }
        self.dirty = false;
        let generation = self.next;
        self.next += 1;
        self.in_flight = Some(generation);
        Some(QueryToken {
            model: self.model,
            generation,
        })
    }

    /// The model switched to other data: results of every query issued so far are dropped.
    pub fn reset(&mut self) {
        self.floor = self.next;
        self.in_flight = None;
        self.dirty = false;
    }

    /// A result arrived for `token`.
    pub fn complete(&mut self, token: QueryToken) -> Completion {
        if token.model != self.model {
            return Completion::Stale;
        }
        if self.in_flight == Some(token.generation) {
            self.in_flight = None;
        }
        if token.generation < self.floor || token.generation < self.applied {
            return Completion::Stale;
        }
        self.applied = token.generation;
        Completion::Apply
    }

    /// The coalesced follow-up query, when invalidations arrived while one was in flight.
    pub fn follow_up(&mut self) -> Option<QueryToken> {
        if self.dirty && self.in_flight.is_none() {
            self.request()
        } else {
            None
        }
    }

    /// A query is in flight.
    pub fn in_flight(&self) -> bool {
        self.in_flight.is_some()
    }

    /// A query is in flight or a follow-up waits for it: the model is going to change.
    pub fn pending(&self) -> bool {
        self.in_flight.is_some() || self.dirty
    }

    /// Generation of the last applied result (0 = none).
    pub fn applied(&self) -> u64 {
        self.applied
    }
}

/// What a timeline query is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fetch {
    /// Re-read the current window.
    Reload,
    /// The page before the oldest row.
    Older,
    /// The page after the newest row.
    Newer,
    /// A window around one message (a quote was activated and its original is not loaded).
    Around(MessageId),
    /// The newest page, replacing a window that ended before it ("Jump to latest").
    Newest,
}

/// How long after an action window ended the flags are read again (the core counts ages in
/// whole seconds; a read right at the instant could still see the old answer).
const FLAGS_SLACK: Duration = Duration::from_millis(250);

/// The text the composer sent last, kept so that a refusal of THAT send can give the text
/// back. The core's `ActionFailed` for a `SendText` names the conversation and nothing more, and
/// the composer is not the only front-end that sends: the desktop's inline reply sends
/// `SendText` too. So the entry is taken only by a failure for its own conversation, and it is
/// forgotten once the core has stored the send ([`CoreSource::forget_stored_send`]), or a
/// refusal of someone else's send would put an old, long sent text back into a draft.
struct LastSent {
    conv: ConversationId,
    text: String,
    reply: Option<MessageId>,
    /// The newest row the open timeline held when the text was sent (`None`: the chat was not
    /// open and loaded at its newest messages, so no timeline can tell that the send was
    /// stored). A row of this device above it is the stored send.
    after: Option<MessageId>,
}

/// The open conversation's loaded rows.
#[derive(Clone, Debug, Default)]
struct Window {
    /// Ascending by `(sort_key, id)`.
    rows: Vec<CoreMessage>,
    has_older: bool,
    has_newer: bool,
    loaded: bool,
    /// The last reload failed: wait for the next invalidation instead of re-asking at once
    /// (a persistent error would otherwise loop between the UI and the core).
    failed: bool,
}

/// Paging state of the open timeline, for the view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Paging {
    /// Rows before the window exist.
    pub has_older: bool,
    /// Rows after the window exist (the newest messages are not loaded).
    pub has_newer: bool,
    /// A page is being loaded.
    pub loading: bool,
}

/// A login in progress.
#[derive(Clone, Debug)]
struct Login {
    network: NetworkKind,
    account: Option<AccountId>,
    view: LoginView,
    /// The login links this existing (logged-out) account again: a retry after a failure
    /// must do the same, never add a second account.
    relink: Option<AccountId>,
    /// The backend reported a history transfer of this login's account: it is linked (a
    /// backend reports a transfer only after the link), and leaving the page loses nothing.
    transferred: bool,
    /// The page ended because the core has the account ([`CoreSource::end_link_page`]):
    /// the backend's later steps of this login (its tail, its `Done`) change nothing.
    ended_linked: bool,
    /// The id of the flow this login runs (`qr`, `pair-code`).
    flow: String,
    /// What was typed into the last input step that was answered, for the same step of the
    /// next try ("Try Again" after a refused number, a switch of the flow and back).
    typed: Option<Typed>,
}

/// What a person typed into an input step of a login (a phone number). It lives in memory only
/// while the link page's login does, and it never prints.
#[derive(Clone)]
struct Typed {
    /// The step it answered.
    step_id: String,
    /// `(field id, value)`.
    values: Vec<(String, Redacted<String>)>,
}

impl std::fmt::Debug for Typed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typed")
            .field("step_id", &self.step_id)
            .finish_non_exhaustive()
    }
}

/// How long a pushed message history may go without a batch before the banner and the hold on
/// "Clear" and "Delete" let go of it (ms): the phone may have gone away, and nothing says so.
/// Later pieces (a chat's older messages asked for) show no banner of their own.
pub const HISTORY_FILL_IDLE_MS: i64 = 5 * 60_000;

/// What an account whose network pushes its history is waiting for.
#[derive(Clone, Copy)]
struct HistoryFill {
    /// The latest progress a batch reported (0 … 1), when it gave one.
    fraction: Option<f32>,
    /// When the last batch came (ms).
    last_batch: i64,
}

/// Which banner of the accounts a call asks for ([`CoreSource::banner_of`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BannerSort {
    /// The most pressing state.
    State,
    /// The most pressing history transfer, of any account.
    Transfer,
    /// The most pressing history transfer of the SHOWN accounts alone.
    ShownTransfer,
}

/// A replace under way: the account being replaced and how many `AddAccount`s were waiting
/// ahead of the replace's own.
#[derive(Clone, Copy, Debug)]
struct Replacing {
    old: AccountId,
    ahead: usize,
}

/// A replace whose account the core announced ([`ReplaceNews::Answered`]): kept until that
/// account is visible or gone, so that a cancel or a failure of its link after the
/// announcement still ends the replace ([`ReplaceNews::Ended`]).
#[derive(Clone, Copy, Debug)]
struct Announced {
    old: AccountId,
    new: AccountId,
    /// An accounts answer has listed the new account: one that no longer does means it went.
    seen: bool,
}

/// What [`CoreSource::new`] needs besides the sink.
pub struct CoreSourceConfig {
    /// "Now" in ms since the Unix epoch (a fixed instant in fixture mode).
    pub now: Box<dyn Fn() -> i64>,
    /// Local time offsets.
    pub calendar: Box<dyn Calendar>,
    /// The networks the welcome page offers.
    pub networks: Vec<NetworkChoice>,
    /// The default device name for linking.
    pub hostname: String,
    /// The conversation `--page=chat` opens.
    pub demo: Option<ConversationId>,
}

/// The UI's view of the core (§8.5).
pub struct CoreSource {
    sink: Box<dyn CommandSink>,
    now: Box<dyn Fn() -> i64>,
    calendar: Box<dyn Calendar>,
    networks: Vec<NetworkChoice>,
    hostname: String,
    demo: Option<ConversationId>,

    accounts_q: QueryTracker,
    accounts: Vec<CoreAccount>,
    conn: HashMap<AccountId, ConnState>,
    /// The send hold of each account as the last event or query said (like `conn`: the live
    /// state wins over a row that is older).
    holds: HashMap<AccountId, Option<AccountHold>>,
    account_errors: HashSet<AccountId>,

    list_q: QueryTracker,
    list: Vec<CoreConversation>,
    /// The pages of a list read that is still going on (more conversations than one page).
    list_partial: Vec<CoreConversation>,
    /// How many reads over several pages in a row started over because the list changed
    /// under them ([`LIST_RESTARTS`]).
    list_restarts: u8,
    /// The pass being read was overtaken and is read to its end anyway: the list is read
    /// once more from the top when it is done.
    list_again: bool,

    open: Option<ConversationId>,
    detail_q: QueryTracker,
    detail: Option<ConversationDetail>,
    /// `detail` is what was known before the chat opened and its own answer is not in yet.
    detail_seeded: bool,
    /// The last detail seen of each chat, what a chat opens with until its answer arrives.
    known_details: HashMap<ConversationId, ConversationDetail>,
    /// Features by `features_hash`: most direct chats of a network share them, so a chat
    /// never opened yet still opens with the composer it is going to have.
    features_seen: HashMap<u64, kourier_model::ConversationFeatures>,
    /// The hashes already read ahead (answered or not): each is asked for once.
    features_asked: HashSet<u64>,
    /// The colors the person gave accounts (the preference `account-tints`).
    own_tints: HashMap<kourier_model::AccountUuid, String>,
    read_ahead_q: QueryTracker,
    timeline_q: QueryTracker,
    fetching: Option<(u64, Fetch)>,
    want_reload: bool,
    want_older: bool,
    want_newer: bool,
    /// A quote was activated whose original is not loaded: load the window around it.
    want_around: Option<MessageId>,
    /// "Jump to latest" from a window that ends before the newest message.
    want_newest: bool,
    /// The window was at the newest message when a burst arrived: page newer rows in until it
    /// is again.
    following: bool,
    window: Window,
    /// How far the long texts of the open conversation are shown ("Read more").
    read_more: ReadMore,
    unread_from: Option<MessageId>,
    /// When the first edit or delete window of a loaded row ends (`MessageActions::until`,
    /// measured from the arrival of the page that reported it).
    flags_deadline: Option<Instant>,

    names: NameMap,
    typing: HashMap<ConversationId, Vec<(UserId, i64)>>,
    login: Option<Login>,
    /// `AddAccount`s sent whose `AccountAdded` has not arrived yet, oldest first (the core
    /// answers them in order); `true` once the user cancelled that login.
    adds: VecDeque<bool>,
    /// The replace ("Transfer History from Phone…") under way: whose `AddAccount` is its own
    /// login (blended chats: the choice of the replaced account passes to the id that login
    /// gets, and to no other login's).
    replacing: Option<Replacing>,
    /// What became of the replace's login, for [`UiSource::take_replace_news`].
    replace_news: Vec<ReplaceNews>,
    /// The replace whose account the core announced, until it is visible or gone.
    announced: Option<Announced>,
    /// Accounts whose login was cancelled before the core announced them: their steps are
    /// ignored.
    cancelled: HashSet<AccountId>,
    /// Accounts whose login has started and not ended (`Done` or `Failed`), whether or not
    /// the link page still shows it: after the transfer a link goes on with its tail, and
    /// "Transfer History from Phone…" is not offered for it meanwhile (`buttons.md` F4).
    unfinished_links: HashSet<AccountId>,
    /// Chats an imported history still fills (`UiEvent::HistoryProgress` with more to
    /// come) → their account: clearing or deleting them waits (link feedback L3).
    filling: HashMap<ConversationId, AccountId>,
    /// Accounts whose network pushes the history (no `history_transfer`) and that have chats
    /// filling: the latest fraction and when the last batch came. No batch for
    /// [`HISTORY_FILL_IDLE_MS`] ends it (a phone that went away leaves no end to wait for).
    fills: HashMap<AccountId, HistoryFill>,
    /// Accounts the backend told this session that part of their pushed history never arrived
    /// (`BackendNotice::HistoryLost`): the quiet line of their banner. Memory only: the backend
    /// tells it once per start, until the core took the notice, so a restart does not show it
    /// again. Cleared with the account (removed, logged out).
    lost_history: HashSet<AccountId>,
    /// Conversations merged away this session → the surviving one (§3.3).
    merged: HashMap<ConversationId, ConversationId>,
    /// Merges the composer has not heard of yet: `(merged away, surviving)`, oldest first.
    merges: Vec<(ConversationId, ConversationId)>,
    settings_q: QueryTracker,
    /// What happens to disappearing messages while the clock is unconfirmed (§9.6), as the
    /// core holds it; `None` until its first answer.
    expiry: Option<ExpiryPolicy>,
    quiet_q: QueryTracker,
    /// Whether quiet notices count for the conversation list, as the core holds it (`true`:
    /// shown); `None` until its first answer.
    quiet: Option<bool>,
    focused: bool,
    chat_visible: bool,
    presence_sent: Option<(bool, Option<ConversationId>)>,
    last_marked: Option<(ConversationId, MessageId)>,
    /// Clicks whose notice waits for the list to show the change ([`Awaited`]).
    awaited: Vec<Awaited>,
    /// Notices ready for the front end ([`UiSource::take_told`]).
    told: Vec<ChatTold>,
    typing_sent: Option<(ConversationId, i64)>,
    drafts: HashMap<ConversationId, String>,
    /// The last text the composer sent, given back to the composer if the core refuses that
    /// send ([`LastSent`]).
    last_sent: Option<LastSent>,
    notice: String,
    stopped: bool,
    /// Resolves avatar files to URLs (none: no avatars, the UI shows initials).
    avatars: Option<Box<dyn AvatarUrls>>,
    /// A row was built while its avatar was still being prepared: rebuild on a tick.
    avatars_waiting: std::cell::Cell<bool>,
    /// The instant of the last [`UiSource::tick`], to see which instants of the banners have
    /// passed since ([`Self::banner_instants`]).
    last_tick: Option<i64>,
    /// Live download progress of the open timeline's attachments (`Progress::Download`).
    downloads: HashMap<AttachmentId, f32>,
    /// The step of each running send of the open timeline (`Progress::Transmit`'s phase, link
    /// feedback L6): ephemeral, kept while the row says its attempt runs, for the line under
    /// the message.
    send_phases: HashMap<MessageId, SendPhase>,
    /// The bytes of each file of the open timeline's own messages that a backend reports
    /// while its attempt uploads (`Progress::Upload`): ephemeral, kept while the row says its
    /// attempt runs, dropped for a message when its uploads are over. The rings and the line
    /// "Uploading 45% · 12 of 30 MB" are built from it.
    uploads: HashMap<AttachmentId, UploadBytes>,
    /// The message and the text of the note "Already sent" (the X came too late), while it is
    /// the composer's notice: its Delete for Everyone acts on that message.
    too_late: Option<(MessageId, String)>,
    /// Answers to `OpenMedia` not taken yet.
    media_answers: Vec<MediaAnswer>,
    /// The save calls: what each `SaveFacts` query in flight is for, by its generation.
    save_asks: HashMap<u64, SaveAsk>,
    /// The generation of the next `SaveFacts` query.
    save_next: u64,
    /// The files whose `SaveAttachment` the core has not answered yet: ONE run per attachment,
    /// because the core's answers name the attachment only.
    save_runs: HashMap<AttachmentId, SaveRun>,
    /// Runs of an attachment whose earlier save is still in flight, in the order they were asked;
    /// the next one starts when the one in flight ends (the core serves its saves in order).
    save_waiting: HashMap<AttachmentId, VecDeque<SaveRun>>,
    /// The batches that wait for their files, by number.
    save_batches: HashMap<u64, SaveBatch>,
    /// The number of the next batch.
    save_batch_next: u64,
    /// Answers to the save calls not taken yet.
    save_answers: Vec<SaveAnswer>,
    /// Files of every `SendFiles` the core did not take, oldest first, until the composer
    /// takes them back (several can wait: a send that went out again without the refused
    /// files may be refused once more before the composer looked).
    refused_files: VecDeque<RefusedSend>,
    /// The id of the next `SendFiles` (the core echoes it in everything about that send).
    next_request: u64,
    /// The `SendFiles` requests the core has not answered yet, by their id.
    preparing: HashMap<RequestId, Preparing>,
    /// Requests that ended, until the composer takes them.
    finished: Vec<FinishedSend>,
    plan_q: QueryTracker,
    /// The file plan the composer wants (the newest request).
    plan_wanted: Option<(ConversationId, PlanRequest)>,
    /// Which request each plan query in flight was sent for: query generation → serial.
    plan_sent: HashMap<u64, u64>,
    plan_answer: Option<PlanAnswer>,
    /// The serial of a plan request the core could not answer.
    plan_failed: Option<u64>,
    revisions_q: QueryTracker,
    revisions_wanted: Option<MessageId>,
    revisions_sent: HashMap<u64, MessageId>,
    revisions_answer: Option<RevisionList>,
    /// The message whose edit history is on screen: asked about again when the open
    /// conversation changes.
    revisions_shown: Option<MessageId>,
    person_q: QueryTracker,
    /// The copy action asked last: its generation (one a counter of its own, every action is
    /// sent) and what it takes. An older action's answer is dropped.
    copy_asked: Option<(u64, crate::person::CopyAsked)>,
    copy_generation: u64,
    copy_answer: Option<crate::person::CopyUpdate>,
    /// The person the details panel shows, as asked (the canonical id once an answer named it).
    person_shown: Option<(AccountId, UserId)>,
    /// The chat the person's page was opened from (the open chat): the page ends with it, and a
    /// merge moves it. NOT the person's own 1:1 chat unless the page was opened from that chat:
    /// opened from a group's member list, the removal of her (maybe hidden) chat ends nothing
    /// (people pages P2, F9).
    person_origin: Option<ConversationId>,
    /// Which person each query in flight was sent for: query generation → person.
    person_sent: HashMap<u64, (AccountId, UserId)>,
    person_answer: Option<PersonUpdate>,
    /// The page last handed to the front end: an answer equal to it changes nothing (a message in
    /// a busy group asks the page again, and nearly every answer is the same).
    person_last: Option<crate::person::PersonView>,
    group_q: QueryTracker,
    /// The group the details panel shows (its account, its conversation): also the page's origin.
    group_shown: Option<(AccountId, ConversationId)>,
    /// Which group each query in flight was sent for: query generation → group.
    group_sent: HashMap<u64, (AccountId, ConversationId)>,
    group_answer: Option<GroupUpdate>,
    /// As `person_last`, for the group's page.
    group_last: Option<crate::group::GroupView>,
    /// The safety-number level (P5a): one query in flight; a re-ask while one runs is asked
    /// once after it (critic F16).
    safety_q: QueryTracker,
    /// The person whose number the level shows.
    safety_shown: Option<(AccountId, UserId)>,
    /// Which person each query in flight was sent for: query generation → person.
    safety_sent: HashMap<u64, (AccountId, UserId)>,
    safety_answer: Option<crate::safety::SafetyView>,
    /// The level last handed over: an equal answer changes nothing.
    safety_last: Option<crate::safety::SafetyView>,
    /// The key the level last SHOWED, held through answers without a number (the account
    /// stopped, a query failed) so a swap across them is still a swap (V3); a click sets it to
    /// the key it acts on (W3); forgotten by a refusal (which has its own line), a new request
    /// and the close.
    safety_key: Option<kourier_model::IdentityFingerprint>,
    /// The line of a refused click, shown with the next answer.
    safety_notice: String,
    /// The gallery of a chat in five places: four of the details panel and the picture viewer's own (people pages P4).
    media: media_slots::MediaSlots,
    /// The history transfers of the accounts, as last reported (`UiEvent::HistoryTransfer`).
    transfers: TransferBook,
    /// What to say once about transfers that ended, until the front-end takes it.
    transfer_notices: Vec<String>,
    /// The account the question "Remove and Link Again?" is open for (the one asked last).
    /// Only a confirmation of that question, naming that account, removes an account, and
    /// it takes the question with it.
    replace_asked: Option<AccountId>,
    /// The open question of "Remove Account…" or "Unlink…" (`ask_account_action`).
    action_asked: Option<(AccountId, AccountAction)>,
    /// What is told once about removed or unlinked accounts' devices.
    logout_notices: Vec<LogoutNotice>,
    /// The `seq`s of the `LogoutOutcome`s told (the newest [`LOGOUT_SEEN_KEPT`]): the core
    /// tells the ones that came before the front-end was ready once more at `UiReady`, and an
    /// outcome told live in between may have a higher number than ones only the replay brings.
    logout_seen: std::collections::BTreeSet<u64>,
}

/// How long a click on Archive, Unarchive or Mark as Unread waits for the core's row to say it
/// happened before it is forgotten (the notice would arrive for a change nobody remembers).
const TOLD_WAIT_MS: i64 = 30_000;

/// What a click waits for before it is told ([`Awaited`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AwaitedKind {
    /// The chat is to be in the archive.
    Archive,
    /// The chat is to be in the inbox.
    Unarchive,
    /// The chat is to be marked unread.
    MarkUnread,
}

impl AwaitedKind {
    /// Whether two kinds are the same question: an archive and an unarchive of one chat
    /// replace each other, a mark is its own.
    fn same_family(self, other: Self) -> bool {
        (self == Self::MarkUnread) == (other == Self::MarkUnread)
    }
}

/// A click whose notice waits for the core's row to show the change ("Chat archived" only once
/// the chat IS archived, never at the click: a refusal tells nothing, and a narrow window may
/// show nothing of the change). Memory only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Awaited {
    /// The chat (the survivor of a merge).
    conv: ConversationId,
    /// What it waits for.
    kind: AwaitedKind,
    /// The chat was pinned when the click came (an archive takes the pin with it).
    was_pinned: bool,
    /// The source's clock at the click, ms.
    asked_at: i64,
}

/// One `SendFiles` the core is importing: nothing of it is in the timeline yet. It ends with
/// the core's `FilesStored` or its refusal, both of which name the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Preparing {
    /// The conversation it is for.
    conv: ConversationId,
    /// How many files it has.
    files: usize,
    /// How many of them the core has copied (`Progress::Import`).
    done: usize,
}

/// Turns an avatar file of the core (relative to the account's blob directory) into a URL QML
/// can show, e.g. `file://` of a decrypted copy in the tmpfs media cache (§8.6). It must not
/// block: `None` means "not ready yet", and the next rebuild of the row asks again.
pub trait AvatarUrls {
    /// The URL of `avatar` of the account `account`, if ready.
    fn url(
        &self,
        account: &kourier_model::AccountUuid,
        avatar: &kourier_model::BlobRef,
    ) -> Option<String>;

    /// Some avatar is still being prepared (rows that asked for it are rebuilt once it is not).
    fn pending(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for CoreSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreSource")
            .field("accounts", &self.accounts.len())
            .field("conversations", &self.list.len())
            .field("open", &self.open)
            .field("window", &self.window.rows.len())
            .finish_non_exhaustive()
    }
}

/// Typing "on" without a refresh expires after this long when the event gave no TTL.
const DEFAULT_TYPING_TTL_MS: i64 = 15_000;

/// How many `LogoutOutcome` numbers [`CoreSource`] remembers (the core keeps 32 for a replay).
const LOGOUT_SEEN_KEPT: usize = 64;

impl CoreSource {
    /// A source over `sink`. Call [`UiSource::reload`] to load the first data.
    pub fn new(sink: Box<dyn CommandSink>, config: CoreSourceConfig) -> Self {
        Self {
            sink,
            now: config.now,
            calendar: config.calendar,
            networks: config.networks,
            hostname: config.hostname,
            demo: config.demo,
            accounts_q: QueryTracker::new(MODEL_ACCOUNTS),
            accounts: Vec::new(),
            conn: HashMap::new(),
            holds: HashMap::new(),
            account_errors: HashSet::new(),
            list_q: QueryTracker::new(MODEL_CONVERSATIONS),
            list: Vec::new(),
            list_partial: Vec::new(),
            list_restarts: 0,
            list_again: false,
            open: None,
            detail_q: QueryTracker::new(MODEL_CONVERSATION),
            detail: None,
            detail_seeded: false,
            known_details: HashMap::new(),
            features_seen: HashMap::new(),
            features_asked: HashSet::new(),
            own_tints: HashMap::new(),
            read_ahead_q: QueryTracker::new(MODEL_READ_AHEAD),
            timeline_q: QueryTracker::new(MODEL_TIMELINE),
            fetching: None,
            want_reload: false,
            want_older: false,
            want_newer: false,
            want_around: None,
            want_newest: false,
            following: false,
            window: Window::default(),
            read_more: ReadMore::default(),
            unread_from: None,
            flags_deadline: None,
            names: NameMap::new(),
            typing: HashMap::new(),
            login: None,
            adds: VecDeque::new(),
            replacing: None,
            replace_news: Vec::new(),
            announced: None,
            cancelled: HashSet::new(),
            unfinished_links: HashSet::new(),
            filling: HashMap::new(),
            fills: HashMap::new(),
            lost_history: HashSet::new(),
            merged: HashMap::new(),
            focused: false,
            chat_visible: false,
            presence_sent: None,
            last_marked: None,
            awaited: Vec::new(),
            told: Vec::new(),
            typing_sent: None,
            drafts: HashMap::new(),
            last_sent: None,
            notice: String::new(),
            stopped: false,
            avatars: None,
            avatars_waiting: std::cell::Cell::new(false),
            last_tick: None,
            downloads: HashMap::new(),
            send_phases: HashMap::new(),
            uploads: HashMap::new(),
            too_late: None,
            media_answers: Vec::new(),
            save_asks: HashMap::new(),
            save_next: 1,
            save_runs: HashMap::new(),
            save_waiting: HashMap::new(),
            save_batches: HashMap::new(),
            save_batch_next: 1,
            save_answers: Vec::new(),
            refused_files: VecDeque::new(),
            next_request: 0,
            preparing: HashMap::new(),
            finished: Vec::new(),
            plan_q: QueryTracker::new(MODEL_FILE_PLAN),
            plan_wanted: None,
            plan_sent: HashMap::new(),
            plan_answer: None,
            plan_failed: None,
            revisions_q: QueryTracker::new(MODEL_REVISIONS),
            revisions_wanted: None,
            revisions_sent: HashMap::new(),
            revisions_answer: None,
            revisions_shown: None,
            person_q: QueryTracker::new(MODEL_PERSON),
            copy_asked: None,
            copy_generation: 0,
            copy_answer: None,
            safety_q: QueryTracker::new(MODEL_SAFETY),
            safety_shown: None,
            safety_sent: HashMap::new(),
            safety_answer: None,
            safety_last: None,
            safety_key: None,
            safety_notice: String::new(),
            person_shown: None,
            person_origin: None,
            person_sent: HashMap::new(),
            person_answer: None,
            person_last: None,
            group_q: QueryTracker::new(MODEL_GROUP),
            group_shown: None,
            group_sent: HashMap::new(),
            group_answer: None,
            group_last: None,
            media: media_slots::MediaSlots::new(),
            merges: Vec::new(),
            settings_q: QueryTracker::new(MODEL_SETTINGS),
            expiry: None,
            quiet_q: QueryTracker::new(MODEL_QUIET),
            quiet: None,
            transfers: TransferBook::default(),
            transfer_notices: Vec::new(),
            replace_asked: None,
            action_asked: None,
            logout_notices: Vec::new(),
            logout_seen: std::collections::BTreeSet::new(),
        }
    }

    /// Shows avatars: rows get URLs from `urls` (by default they have none).
    pub fn set_avatar_urls(&mut self, urls: Box<dyn AvatarUrls>) {
        self.avatars = Some(urls);
    }

    /// The URL of a conversation's avatar, or empty (no avatar, or not ready yet).
    fn avatar_url(&self, account: AccountId, avatar: Option<&kourier_model::BlobRef>) -> String {
        let (Some(urls), Some(avatar)) = (self.avatars.as_deref(), avatar) else {
            return String::new();
        };
        let url = self
            .accounts
            .iter()
            .find(|a| a.id == account)
            .and_then(|a| urls.url(&a.uuid, avatar));
        if url.is_none() && urls.pending() {
            self.avatars_waiting.set(true);
        }
        url.unwrap_or_default()
    }

    fn send(&mut self, cmd: UiCommand) -> bool {
        let ok = self.sink.send(cmd);
        self.stopped |= !ok;
        ok
    }

    fn query(&mut self, token: Option<QueryToken>, query: impl FnOnce() -> Query) {
        if let Some(token) = token {
            let query = query();
            self.send(UiCommand::Query { token, query });
        }
    }

    fn request_accounts(&mut self) {
        let t = self.accounts_q.request();
        self.query(t, || Query::Accounts);
    }

    fn request_list(&mut self) {
        let t = self.list_q.request();
        if t.is_some() {
            // A fresh read starts at the first page.
            self.list_partial.clear();
            self.list_again = false;
        }
        self.query(t, || list_query(None));
    }

    /// Asks the core for the settings it keeps.
    fn request_settings(&mut self) {
        self.request_expiry();
        self.request_quiet();
    }

    /// Asks the core which option holds for disappearing messages while the clock is
    /// unconfirmed.
    fn request_expiry(&mut self) {
        let t = self.settings_q.request();
        self.query(t, || Query::ExpiryPolicy);
    }

    /// Asks the core whether quiet notices count for the conversation list.
    fn request_quiet(&mut self) {
        let t = self.quiet_q.request();
        self.query(t, || Query::QuietNotices);
    }

    fn request_detail(&mut self) {
        let Some(conv) = self.open else { return };
        let t = self.detail_q.request();
        self.query(t, || Query::Conversation(conv));
    }

    /// Reads the details of the next chat near the top of the list whose features were not
    /// seen yet (one query at a time, after the list is on screen).
    fn read_ahead(&mut self) {
        if self.read_ahead_q.pending() {
            return;
        }
        let next = self
            .list
            .iter()
            .take(READ_AHEAD_ROWS)
            .find(|c| {
                !self.features_seen.contains_key(&c.features_hash)
                    && !self.features_asked.contains(&c.features_hash)
            })
            .map(|c| (c.id, c.features_hash));
        let Some((conv, hash)) = next else { return };
        self.features_asked.insert(hash);
        let t = self.read_ahead_q.request();
        self.query(t, || Query::Conversation(conv));
    }

    /// Remembers what a read of a chat said, for the next time it opens.
    fn remember_detail(&mut self, d: &ConversationDetail) {
        self.features_seen
            .insert(d.row.features_hash, d.features.clone());
        if self.known_details.len() >= KNOWN_DETAILS && !self.known_details.contains_key(&d.row.id)
        {
            self.known_details.clear();
        }
        self.known_details.insert(d.row.id, d.clone());
    }

    /// What is known of a chat before it opens: its own last detail, else its list row with
    /// the features of a chat that has the same ones. `None`: nothing to go by yet.
    fn seed_detail(&self, id: ConversationId) -> Option<ConversationDetail> {
        let row = self.list.iter().find(|c| c.id == id);
        if let Some(known) = self.known_details.get(&id) {
            let mut seed = known.clone();
            if let Some(row) = row {
                seed.row = row.clone();
            }
            return Some(seed);
        }
        let row = row?;
        let features = self.features_seen.get(&row.features_hash)?;
        Some(ConversationDetail {
            row: row.clone(),
            members: Vec::new(),
            features: features.clone(),
            timer: None,
            draft: None,
        })
    }

    /// Issues the next wanted timeline query unless one is in flight.
    fn pump_timeline(&mut self) {
        let Some(conv) = self.open else { return };
        if self.timeline_q.in_flight() {
            return;
        }
        let fetch = if let Some(target) = self.want_around.take() {
            Fetch::Around(target)
        } else if std::mem::take(&mut self.want_newest) {
            Fetch::Newest
        } else if self.want_reload || !(self.window.loaded || self.window.failed) {
            Fetch::Reload
        } else if self.want_older && self.window.has_older {
            Fetch::Older
        } else if self.want_newer && self.window.has_newer {
            Fetch::Newer
        } else {
            self.want_older = false;
            self.want_newer = false;
            return;
        };
        let rows = &self.window.rows;
        let (anchor, limit) = match fetch {
            // Re-read the loaded stretch from its oldest row, with room for a page of new
            // messages: rows above stay where they are (no jump), new ones are added below,
            // and a bigger burst leaves `has_newer` for the bottom to page in.
            Fetch::Reload => match rows.first() {
                Some(oldest) if self.window.loaded => (
                    TimelineAnchor::After(TimelineCursor {
                        sort_key: oldest.sort_key,
                        id: MessageId(oldest.id.0.saturating_sub(1)),
                    }),
                    u32::try_from((rows.len() + TIMELINE_PAGE as usize).min(TIMELINE_WINDOW))
                        .unwrap_or(TIMELINE_PAGE),
                ),
                _ => (TimelineAnchor::Newest, TIMELINE_PAGE),
            },
            Fetch::Older => match rows.first() {
                Some(r) => (TimelineAnchor::Before(r.cursor()), TIMELINE_PAGE),
                None => (TimelineAnchor::Newest, TIMELINE_PAGE),
            },
            Fetch::Newer => match rows.last() {
                Some(r) => (TimelineAnchor::After(r.cursor()), TIMELINE_PAGE),
                None => (TimelineAnchor::Newest, TIMELINE_PAGE),
            },
            Fetch::Around(target) => (TimelineAnchor::Around(target), TIMELINE_PAGE),
            Fetch::Newest => (TimelineAnchor::Newest, TIMELINE_PAGE),
        };
        match fetch {
            Fetch::Reload => self.want_reload = false,
            Fetch::Older => self.want_older = false,
            Fetch::Newer => self.want_newer = false,
            // A window somewhere else: what was wanted for the old one no longer applies.
            Fetch::Around(_) | Fetch::Newest => {
                self.want_reload = false;
                self.want_older = false;
                self.want_newer = false;
            }
        }
        if let Some(token) = self.timeline_q.request() {
            self.fetching = Some((token.generation, fetch));
            self.send(UiCommand::Query {
                token,
                query: Query::Timeline(TimelineQuery {
                    conv,
                    anchor,
                    limit,
                }),
            });
        }
    }

    fn reload_timeline(&mut self) {
        if self.open.is_some() {
            self.want_reload = true;
            self.pump_timeline();
        }
    }

    /// The open conversation, if any.
    pub fn open(&self) -> Option<ConversationId> {
        self.open
    }

    /// `id`, or the conversation it was merged into (following repeated merges).
    fn survivor(&self, mut id: ConversationId) -> ConversationId {
        for _ in 0..self.merged.len() {
            match self.merged.get(&id) {
                Some(into) if *into != id => id = *into,
                _ => break,
            }
        }
        id
    }

    /// Paging state of the open timeline.
    pub fn paging(&self) -> Paging {
        Paging {
            has_older: self.window.has_older,
            has_newer: self.window.has_newer,
            loading: self.timeline_q.in_flight(),
        }
    }

    /// Number of loaded timeline messages (tests, diagnostics).
    pub fn window_len(&self) -> usize {
        self.window.rows.len()
    }

    /// Whether the core stopped accepting commands.
    pub fn stopped(&self) -> bool {
        self.stopped
    }

    fn account_network(&self, account: AccountId) -> NetworkKind {
        self.accounts
            .iter()
            .find(|a| a.id == account)
            .map_or(NetworkKind::Fake, |a| a.network)
    }

    /// Takes the oldest waiting `AddAccount` off the queue (`Some(true)`: the user cancelled
    /// that login) and tells the replace under way, if that was its own login's, how it went:
    /// `answered` is the id the core announced, `None` when the add failed.
    fn pop_add(&mut self, answered: Option<AccountId>) -> Option<bool> {
        let cancelled = self.adds.pop_front();
        if let Some(r) = self.replacing.as_mut() {
            if r.ahead == 0 {
                let old = r.old;
                self.replacing = None;
                self.replace_news.push(match (cancelled, answered) {
                    (Some(false), Some(new)) => {
                        self.announced = Some(Announced {
                            old,
                            new,
                            seen: false,
                        });
                        ReplaceNews::Answered { old, new }
                    }
                    _ => ReplaceNews::Ended { old },
                });
            } else {
                r.ahead -= 1;
            }
        }
        cancelled
    }

    /// The announced account of a replace will not come (its link was cancelled or failed, or
    /// it is gone): the replace ends. `true` when that was the replace's account (the caller
    /// returns `Changes { accounts: true, .. }` so the choice is pruned at once).
    fn end_announced(&mut self, account: AccountId) -> bool {
        match self.announced {
            Some(a) if a.new == account => {
                self.announced = None;
                self.replace_news.push(ReplaceNews::Ended { old: a.old });
                true
            }
            _ => false,
        }
    }

    /// An accounts answer: the announced account of a replace that is visible now passes to
    /// the choice (nothing more to keep); one that is gone after an answer listed it ends the
    /// replace. `true` when news was pushed.
    fn follow_announced(&mut self, rows: &[CoreAccount]) -> bool {
        let Some(a) = self.announced.as_mut() else {
            return false;
        };
        match rows.iter().find(|r| r.id == a.new) {
            Some(r) if !matches!(r.state, AccountState::Linking | AccountState::Removing) => {
                self.announced = None;
                false
            }
            Some(_) => {
                a.seen = true;
                false
            }
            // An answer read before the announcement may lack it: only one after an answer
            // that listed it says it went.
            None if a.seen => {
                let new = a.new;
                self.end_announced(new)
            }
            None => false,
        }
    }

    /// The filter's hidden uuids as this source's account ids, for one call: `Unknown` until
    /// the accounts answered ([`UiSource::accounts_known`]).
    fn resolve_hidden(&self, filter: &ConversationFilter) -> Hidden {
        filter.resolve(
            self.accounts.iter().map(|a| (a.id, a.uuid)),
            self.accounts_known(),
        )
    }

    fn visible_accounts(&self) -> impl Iterator<Item = &CoreAccount> {
        self.accounts
            .iter()
            .filter(|a| !matches!(a.state, AccountState::Linking | AccountState::Removing))
    }

    fn now_ms(&self) -> i64 {
        (self.now)()
    }

    fn typers(&self, conv: ConversationId) -> Vec<UserId> {
        let now = self.now_ms();
        self.typing
            .get(&conv)
            .map(|v| {
                v.iter()
                    .filter(|(_, until)| *until > now)
                    .map(|(u, _)| u.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// We sent something to `conv`: its "N unread messages" divider has served its purpose.
    fn answered(&mut self, conv: ConversationId) {
        if self.open == Some(conv) {
            self.unread_from = None;
        }
    }

    // ---------- events ----------

    fn on_query_result(
        &mut self,
        token: QueryToken,
        result: &Result<QueryData, std::sync::Arc<impl std::fmt::Display>>,
    ) -> Changes {
        let mut ch = Changes::default();
        match token.model {
            MODEL_ACCOUNTS => {
                if self.accounts_q.complete(token) == Completion::Apply
                    && let Ok(QueryData::Accounts(rows)) = result
                {
                    for a in rows {
                        // A fresh query is newer than the last state event only when no event
                        // came; keep live states, take queried ones for new accounts.
                        self.conn.entry(a.id).or_insert_with(|| a.conn.clone());
                        self.holds.entry(a.id).or_insert(a.hold);
                    }
                    self.conn.retain(|id, _| rows.iter().any(|a| a.id == *id));
                    self.holds.retain(|id, _| rows.iter().any(|a| a.id == *id));
                    // An account found logged out (at a start, or when its state event was
                    // lost): the backend deleted a transfer that ran with the login.
                    self.fills.retain(|id, _| rows.iter().any(|a| a.id == *id));
                    self.lost_history
                        .retain(|id| rows.iter().any(|a| a.id == *id));
                    self.filling
                        .retain(|_, id| rows.iter().any(|a| a.id == *id));
                    for a in rows {
                        if matches!(a.state, AccountState::LoggedOut { .. }) {
                            self.fills.remove(&a.id);
                            self.lost_history.remove(&a.id);
                            self.filling.retain(|_, f| *f != a.id);
                            ch.transfer |= self.transfers.logged_out(a.id)
                                | self.transfers.forget_progress(a.id);
                        }
                    }
                    // (`ch.accounts` is set below for every answer.)
                    let _ended = self.follow_announced(rows);
                    self.accounts = rows.clone();
                    ch.person |= self.end_person_unless_usable();
                    ch.group |= self.end_group_unless_usable();
                    ch.accounts = true;
                    ch.conversations = true;
                    ch.login |= self.end_link_page();
                    // What the open chat says about sending (and about deleting) goes by
                    // its account's state. The answers about the chat itself may have come
                    // before this one (the queries run on two connections): without this
                    // the composer of a chat whose account was just linked again went on
                    // saying "link this device again" until something else changed.
                    ch.header = self.detail.is_some();
                }
                let t = self.accounts_q.follow_up();
                self.query(t, || Query::Accounts);
            }
            MODEL_CONVERSATIONS => {
                if self.list_q.complete(token) == Completion::Apply
                    && let Ok(QueryData::Conversations(page)) = result
                {
                    for row in &page.rows {
                        if let Some(p) = &row.preview
                            && let (Some(a), Some(n)) = (&p.author, &p.author_name)
                        {
                            self.names.insert(a.clone(), n.clone());
                        }
                        if let (Some(peer), Some(t)) = (&row.peer, &row.title) {
                            self.names.insert(peer.clone(), t.clone());
                        }
                    }
                    let mut rows = std::mem::take(&mut self.list_partial);
                    // One query that read the whole list is a consistent snapshot.
                    let single = rows.is_empty() && page.next.is_none();
                    // (A chat that moved between two pages is in both: once is enough.) A
                    // chat the user deleted is hidden until it is used again: whatever a page
                    // holds, such a row is not listed.
                    let have: HashSet<ConversationId> = rows.iter().map(|r| r.id).collect();
                    rows.extend(
                        page.rows
                            .iter()
                            .filter(|r| !r.hidden && !have.contains(&r.id))
                            .cloned(),
                    );
                    // `Some`: the list changed while this page was read.
                    let overtaken = self.list_q.follow_up();
                    let patient = self.list_restarts < LIST_RESTARTS;
                    match (overtaken, page.next) {
                        // A read over several pages that was overtaken may miss a chat that
                        // moved meanwhile: it starts over from the top. Not for ever: while
                        // changes keep coming (a sync) the list would never show, so after
                        // `LIST_RESTARTS` such passes one is read to its end regardless.
                        (Some(t), _) if !single && patient => {
                            self.list_restarts += 1;
                            self.query(Some(t), || list_query(None));
                        }
                        // More conversations than one page: read on until all are here
                        // (and once more from the top afterwards, when this pass was
                        // overtaken).
                        (overtaken, Some(next)) => {
                            self.list_partial = rows;
                            self.list_again |= overtaken.is_some();
                            let t = overtaken.or_else(|| self.list_q.request());
                            self.query(t, || list_query(Some(next)));
                        }
                        // Everything is here: shown, also when it is already outdated (the
                        // list keeps up with a stream of changes instead of freezing until
                        // it ends); then the next read is on its way.
                        (overtaken, None) => {
                            ch.archive_move = archive_move(self.open, &self.list, &rows);
                            // (A gallery follows the list's `protected`: S-P, media_slots.rs.)
                            let withheld = self.hidden_media_slots();
                            self.list = rows;
                            ch.told |= self.settle_awaited();
                            ch.chat_media |= self.rerun_media_slots(&withheld);
                            self.list_restarts = 0;
                            ch.conversations = true;
                            ch.accounts = true;
                            self.read_ahead();
                            let again = std::mem::take(&mut self.list_again);
                            let t = overtaken
                                .or_else(|| again.then(|| self.list_q.request()).flatten());
                            self.query(t, || list_query(None));
                        }
                    }
                } else {
                    self.list_partial.clear();
                    self.list_again = false;
                    let t = self.list_q.follow_up();
                    self.query(t, || list_query(None));
                }
                self.maybe_mark_read();
            }
            MODEL_CONVERSATION => {
                if self.detail_q.complete(token) == Completion::Apply
                    && let Ok(QueryData::Conversation(d)) = result
                {
                    match d {
                        Some(d) if Some(d.row.id) == self.open => {
                            let id = d.row.id;
                            if let Some(draft) =
                                d.draft.as_ref().filter(|t| !t.body.trim().is_empty())
                                && !self.drafts.contains_key(&id)
                            {
                                self.drafts.insert(id, draft.body.clone());
                                ch.draft = true;
                            }
                            self.remember_detail(d);
                            self.detail = Some((**d).clone());
                            self.detail_seeded = false;
                            ch.header = true;
                        }
                        Some(_) => {}
                        None => {
                            ch.merge(self.close());
                        }
                    }
                }
                if let Some(conv) = self.open {
                    let t = self.detail_q.follow_up();
                    self.query(t, || Query::Conversation(conv));
                }
            }
            MODEL_READ_AHEAD => {
                if self.read_ahead_q.complete(token) == Completion::Apply
                    && let Ok(QueryData::Conversation(Some(d))) = result
                {
                    self.remember_detail(d);
                }
                self.read_ahead();
            }
            MODEL_TIMELINE => {
                let fetch = match self.fetching {
                    Some((g, f)) if g == token.generation => {
                        self.fetching = None;
                        Some(f)
                    }
                    _ => None,
                };
                let apply = self.timeline_q.complete(token) == Completion::Apply;
                match (result, fetch) {
                    (Ok(QueryData::Timeline(page)), Some(Fetch::Around(target)))
                        if apply
                            && Some(page.conv) == self.open
                            && !page.rows.iter().any(|r| r.id == target) =>
                    {
                        // The original went away meanwhile: the window stays where it is.
                        self.notice = trc(
                            "@info:status",
                            "The original message is no longer available.",
                        )
                        .into_owned();
                        ch.notice = true;
                    }
                    (Ok(QueryData::Timeline(page)), Some(fetch))
                        if apply && Some(page.conv) == self.open =>
                    {
                        self.apply_page(page, fetch);
                        ch.timeline = true;
                        if let Fetch::Around(target) = fetch {
                            ch.jump = Some(target);
                        }
                        self.maybe_mark_read();
                    }
                    (Err(_), Some(Fetch::Reload)) if apply => {
                        // Keep what is shown; the next invalidation or reload asks again.
                        self.window.failed = true;
                        ch.timeline = true;
                    }
                    _ => {}
                }
                self.pump_timeline();
            }
            MODEL_FILE_PLAN => {
                let serial = self.plan_sent.remove(&token.generation);
                let wanted = self.plan_wanted.as_ref().map(|(_, r)| r.serial);
                if self.plan_q.complete(token) == Completion::Apply
                    && serial.is_some()
                    && serial == wanted
                    && let Some(serial) = serial
                {
                    self.plan_wanted = None;
                    match result {
                        Ok(QueryData::FilePlan(preview)) => {
                            self.plan_answer = Some(PlanAnswer {
                                serial,
                                preview: preview.clone(),
                            });
                        }
                        // The core cannot say (the chat is gone): the composer stops
                        // waiting; a send would tell the user what is wrong.
                        _ => self.plan_failed = Some(serial),
                    }
                    ch.plan = true;
                }
                // The selection changed while the core looked: ask about what it is now.
                let t = self.plan_q.follow_up();
                self.send_plan(t);
            }
            MODEL_SETTINGS => {
                if self.settings_q.complete(token) == Completion::Apply
                    && let Ok(QueryData::ExpiryPolicy(policy)) = result
                    && self.expiry != Some(*policy)
                {
                    self.expiry = Some(*policy);
                    ch.settings = true;
                }
                let t = self.settings_q.follow_up();
                self.query(t, || Query::ExpiryPolicy);
            }
            MODEL_QUIET => {
                if self.quiet_q.complete(token) == Completion::Apply
                    && let Ok(QueryData::QuietNotices(shown)) = result
                    && self.quiet != Some(*shown)
                {
                    self.quiet = Some(*shown);
                    ch.settings = true;
                }
                let t = self.quiet_q.follow_up();
                self.query(t, || Query::QuietNotices);
            }
            MODEL_PERSON => {
                let asked = self.person_sent.remove(&token.generation);
                if self.person_q.complete(token) == Completion::Apply
                    && let Some(asked) = asked
                    && self.person_shown.as_ref().is_some_and(|s| *s == asked)
                {
                    match result {
                        Ok(QueryData::Person(Some(detail))) => {
                            // The answer names the canonical person (an alias is resolved).
                            self.person_shown = Some((detail.account, detail.user.clone()));
                            let network = self
                                .accounts
                                .iter()
                                .find(|a| a.id == detail.account)
                                .map_or(NetworkKind::Signal, |a| a.network);
                            let mut view = crate::person::person_view(detail, network, &|blob| {
                                self.avatar_url(detail.account, blob)
                            });
                            // The failure of her chat's timer stays on her page through the
                            // refreshes (F4); a new ask forgot it with `person_last`.
                            if let Some(last) = self.person_last.as_ref()
                                && last.account == view.account
                                && last.user == view.user
                                && last.chat == view.chat
                            {
                                view.timer_failed.clone_from(&last.timer_failed);
                            }
                            // The same page again (a message in some group asked): nothing to
                            // tell the front end.
                            if self.person_last.as_ref() != Some(&view) {
                                self.person_last = Some(view.clone());
                                self.person_answer = Some(PersonUpdate::Show(Box::new(view)));
                                ch.person = true;
                            }
                        }
                        // The core no longer has the account (or the query failed): nothing
                        // to show.
                        _ => {
                            self.person_answer = Some(PersonUpdate::Ended);
                            self.person_shown = None;
                            self.person_origin = None;
                            self.person_last = None;
                            ch.person = true;
                        }
                    }
                }
                let t = self.person_q.follow_up();
                self.send_person(t);
            }
            MODEL_COPY => {
                if let Some((generation, asked)) = self.copy_asked
                    && generation == token.generation
                {
                    use crate::person::CopyAsked;
                    self.copy_asked = None;
                    self.copy_answer = Some(match asked {
                        CopyAsked::Person(part) => {
                            let detail = match result {
                                Ok(QueryData::Person(d)) => d.as_deref(),
                                _ => None,
                            };
                            let network = detail
                                .and_then(|d| self.accounts.iter().find(|a| a.id == d.account))
                                .map_or(NetworkKind::Signal, |a| a.network);
                            crate::person::copy_update(detail, part, network)
                        }
                        CopyAsked::Invite(action) => match result {
                            Ok(QueryData::GroupInvite(link)) => {
                                crate::group::invite_update(action, false, link.clone())
                            }
                            _ => crate::group::invite_update(action, true, None),
                        },
                    });
                    ch.copy = true;
                }
            }
            MODEL_SAFETY => {
                let asked = self.safety_sent.remove(&token.generation);
                if self.safety_q.complete(token) == Completion::Apply
                    && let Some((account, user)) = asked
                    && self
                        .safety_shown
                        .as_ref()
                        .is_some_and(|s| *s == (account, user.clone()))
                {
                    let (answer, waits) = match result {
                        Ok(QueryData::SafetyNumber { answer, waits }) => (answer.clone(), *waits),
                        // The core could not ask (the query failed): as an account that does
                        // not run.
                        _ => (Err(kourier_model::SafetyUnavailable::NotRunning), false),
                    };
                    let name = self
                        .person_last
                        .as_ref()
                        .filter(|p| p.account == account && p.user == user)
                        .map_or_else(|| crate::names::fallback_name(&user), |p| p.name.clone());
                    let mut view = crate::safety::safety_view(account, user, &name, &answer, waits);
                    // A number that turned into another one under the open level (both known,
                    // no click in between: `safety_key` is the key last shown, held through
                    // answers without one, and a click or a refusal forgets it) is said so, and
                    // the line stays until the next click or the close, also through unrelated
                    // re-asks (U2, V3). The buttons act on the NEW number, which nobody has
                    // compared yet.
                    let swapped = matches!(
                        (self.safety_key.as_ref(), view.key.as_ref()),
                        (Some(before), Some(now)) if before != now
                    );
                    if swapped {
                        self.safety_notice = crate::safety::swapped_text();
                    }
                    if view.key.is_some() {
                        self.safety_key.clone_from(&view.key);
                    }
                    view.notice.clone_from(&self.safety_notice);
                    if self.safety_last.as_ref() != Some(&view) {
                        self.safety_last = Some(view.clone());
                        self.safety_answer = Some(view);
                        ch.safety = true;
                    }
                }
                let t = self.safety_q.follow_up();
                self.send_safety(t);
            }
            MODEL_GROUP => {
                let asked = self.group_sent.remove(&token.generation);
                if self.group_q.complete(token) == Completion::Apply
                    && let Some(asked) = asked
                    && self.group_shown == Some(asked)
                {
                    match result {
                        Ok(QueryData::Group(Some(detail))) => {
                            let mut view = crate::group::group_view(detail, &|blob| {
                                self.avatar_url(detail.account, blob)
                            });
                            // As on the person's page: the group's timer failure stays (F4).
                            if let Some(last) = self.group_last.as_ref()
                                && last.account == view.account
                                && last.id == view.id
                            {
                                view.timer_failed.clone_from(&last.timer_failed);
                            }
                            if self.group_last.as_ref() != Some(&view) {
                                self.group_last = Some(view.clone());
                                self.group_answer = Some(GroupUpdate::Show(Box::new(view)));
                                ch.group = true;
                            }
                        }
                        // The group is gone, is no group, or the query failed: nothing to show.
                        _ => {
                            self.group_answer = Some(GroupUpdate::Ended);
                            self.group_shown = None;
                            self.group_last = None;
                            ch.group = true;
                        }
                    }
                }
                let t = self.group_q.follow_up();
                self.send_group(t);
            }
            model if media_slots::slot_of_model(model).is_some() => {
                if let Some(slot) = media_slots::slot_of_model(model) {
                    ch.merge(self.on_media_result(slot, token, result));
                }
            }
            MODEL_SAVE => {
                let facts = match result {
                    Ok(QueryData::SaveFacts(f)) => f.as_ref(),
                    _ => None,
                };
                ch.merge(self.on_save_facts(token.generation, facts));
            }
            MODEL_REVISIONS => {
                let message = self.revisions_sent.remove(&token.generation);
                if self.revisions_q.complete(token) == Completion::Apply
                    && message.is_some()
                    && message == self.revisions_wanted
                    && let Some(message) = message
                {
                    self.revisions_wanted = None;
                    let found = match result {
                        Ok(QueryData::Revisions(found)) => found.as_ref(),
                        _ => None,
                    };
                    self.revisions_answer = Some(self.revision_list(message, found));
                    ch.revisions = true;
                }
                let t = self.revisions_q.follow_up();
                self.send_revisions(t);
            }
            _ => {}
        }
        ch
    }

    /// Sends the wanted file plan query with `token`.
    fn send_plan(&mut self, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        let Some((conv, request)) = self.plan_wanted.as_ref() else {
            // Nothing is asked any more (the same selection was asked about twice and one
            // answer settled both): the token is not used, and the tracker must not go on
            // waiting for its answer, or no later request would ever be sent.
            self.plan_q.reset();
            return;
        };
        let query = FilePlanQuery {
            conv: *conv,
            paths: request.paths.clone().into(),
            as_file: request.as_file.clone(),
            caption: request.caption,
            // Only a caption longer than one message is handed over: its text then decides
            // how many messages it becomes. (`outgoing_text` is what a send would make of it.)
            caption_text: request
                .caption_text
                .as_deref()
                .and_then(crate::composer::outgoing_text),
            reply: request.reply,
        };
        self.plan_sent.insert(token.generation, request.serial);
        self.send(UiCommand::Query {
            token,
            query: Query::FilePlan(query),
        });
    }

    /// Sends the query for the person on screen with `token`.
    fn send_person(&mut self, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        let Some((account, user)) = self.person_shown.clone() else {
            // Nothing is shown any more: the tracker must not wait for this token.
            self.person_q.reset();
            return;
        };
        self.person_sent
            .insert(token.generation, (account, user.clone()));
        self.send(UiCommand::Query {
            token,
            query: Query::Person { account, user },
        });
    }

    /// Sends the query for the safety number on screen with `token`.
    fn send_safety(&mut self, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        let Some((account, user)) = self.safety_shown.clone() else {
            // Nothing is shown any more: the tracker must not wait for this token.
            self.safety_q.reset();
            return;
        };
        self.safety_sent
            .insert(token.generation, (account, user.clone()));
        self.send(UiCommand::Query {
            token,
            query: Query::SafetyNumber { account, user },
        });
    }

    /// The safety number is asked again (a contact or an account changed, a click was
    /// refused); with one query in flight it is asked once after it (critic F16).
    fn refresh_safety(&mut self) {
        if self.safety_shown.is_none() {
            return;
        }
        let t = self.safety_q.request();
        self.send_safety(t);
    }

    /// The person's page is read again (the contact, their chat or a group changed).
    fn refresh_person(&mut self) {
        if self.person_shown.is_none() {
            return;
        }
        let t = self.person_q.request();
        self.send_person(t);
    }

    /// The page ends: nothing of it is asked about or shown any more.
    fn end_person(&mut self) -> bool {
        if self.person_shown.take().is_none() {
            return false;
        }
        self.person_origin = None;
        self.person_sent.clear();
        self.person_q.reset();
        self.person_last = None;
        self.person_answer = Some(PersonUpdate::Ended);
        true
    }

    /// The page ends when its account cannot be used any more (removed, unlinking, logged out,
    /// or not linked yet), judged by the accounts as last read.
    fn end_person_unless_usable(&mut self) -> bool {
        let Some((account, _)) = self.person_shown.as_ref() else {
            return false;
        };
        if self.usable(*account) {
            false
        } else {
            self.end_person()
        }
    }

    /// Whether the account is linked and in use, judged by the accounts as last read.
    fn usable(&self, account: AccountId) -> bool {
        self.accounts
            .iter()
            .any(|a| a.id == account && matches!(a.state, AccountState::Active))
    }

    /// Sends the query for the group on screen with `token`.
    fn send_group(&mut self, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        let Some(shown) = self.group_shown else {
            // Nothing is shown any more: the tracker must not wait for this token.
            self.group_q.reset();
            return;
        };
        self.group_sent.insert(token.generation, shown);
        self.send(UiCommand::Query {
            token,
            query: Query::Group(shown.1),
        });
    }

    /// The group's page is read again (the group, a member's contact or the accounts changed).
    fn refresh_group(&mut self) {
        if self.group_shown.is_none() {
            return;
        }
        let t = self.group_q.request();
        self.send_group(t);
    }

    /// The group's page ends: nothing of it is asked about or shown any more.
    fn end_group(&mut self) -> bool {
        if self.group_shown.take().is_none() {
            return false;
        }
        self.group_sent.clear();
        self.group_q.reset();
        self.group_last = None;
        self.group_answer = Some(GroupUpdate::Ended);
        true
    }

    /// The group's page ends when its account cannot be used any more (as for the person's).
    fn end_group_unless_usable(&mut self) -> bool {
        let Some((account, _)) = self.group_shown else {
            return false;
        };
        if self.usable(account) {
            false
        } else {
            self.end_group()
        }
    }

    /// Ends the details pages whose origin is among the removed chats; the one rule both pages
    /// follow (a page opened from a chat ends with that chat and with nothing else).
    fn end_pages_of(&mut self, ids: &[ConversationId]) -> (bool, bool) {
        let person = self.person_origin.is_some_and(|o| ids.contains(&o)) && self.end_person();
        let group = self.group_shown.is_some_and(|(_, g)| ids.contains(&g)) && self.end_group();
        (person, group)
    }

    /// Sends the wanted revisions query with `token`.
    fn send_revisions(&mut self, token: Option<QueryToken>) {
        let Some(token) = token else { return };
        let Some(message) = self.revisions_wanted else {
            // Nothing is asked any more (a double click asked for the same history twice
            // and one answer settled both): the tracker is free again (see `send_plan`).
            self.revisions_q.reset();
            return;
        };
        self.revisions_sent.insert(token.generation, message);
        self.send(UiCommand::Query {
            token,
            query: Query::Revisions(message),
        });
    }

    /// The edit history as the dialog lists it: newest first, each version with its time.
    fn revision_list(&self, message: MessageId, found: Option<&MessageRevisions>) -> RevisionList {
        let time = TimeContext::new(self.now_ms(), self.calendar.as_ref());
        let versions = found.map(|f| f.revisions.as_slice()).unwrap_or_default();
        let newest = versions.len().saturating_sub(1);
        let rows = versions
            .iter()
            .enumerate()
            .rev()
            .map(|(i, r)| RevisionRow {
                text: r
                    .text
                    .as_ref()
                    .filter(|t| !t.body.trim().is_empty())
                    .map(|t| crate::richtext::render(t, &self.names)),
                when: format!(
                    "{}, {}",
                    time.date_header(time.day_number(r.at)),
                    time.clock(r.at)
                ),
                original: r.revision == 0,
                current: i == newest,
            })
            .collect();
        RevisionList { message, rows }
    }

    /// Hands a `SendFiles` to the core under a new request id and counts its files as in
    /// preparation. `None` when the core has stopped.
    fn send_files_request(
        &mut self,
        conv: ConversationId,
        paths: Vec<std::path::PathBuf>,
        as_file: Vec<bool>,
        with_location: Vec<bool>,
        caption: Option<Text>,
        reply_to: Option<MessageId>,
    ) -> Option<RequestId> {
        self.next_request += 1;
        let request = RequestId(self.next_request);
        let files = paths.len();
        if !self.send(UiCommand::SendFiles {
            conv,
            request,
            paths: paths.into(),
            as_file,
            with_location,
            caption,
            reply_to,
        }) {
            return None;
        }
        // The core copies and encrypts them first; it says when their messages are stored.
        self.preparing.insert(
            request,
            Preparing {
                conv,
                files,
                done: 0,
            },
        );
        Some(request)
    }

    /// The core answered `request`: it is no longer in preparation. Returns whether it was.
    fn end_preparing(&mut self, request: RequestId, outcome: SendOutcome) -> bool {
        let Some(sending) = self.preparing.remove(&request) else {
            return false;
        };
        self.finished.push(FinishedSend {
            request: request.0,
            conv: self.survivor(sending.conv),
            outcome,
        });
        true
    }

    /// The core copied `done` of the files of `request`. Returns whether the count changed.
    fn on_import_progress(&mut self, request: RequestId, done: u32) -> bool {
        let Some(sending) = self.preparing.get_mut(&request) else {
            return false;
        };
        let done = usize::try_from(done)
            .unwrap_or(usize::MAX)
            .min(sending.files);
        // (Never backwards: the core's counts do not go back, and a late event of a request
        // that was sent again under the same id cannot exist.)
        if done <= sending.done {
            return false;
        }
        sending.done = done;
        true
    }

    /// The core stored the send the composer made last: its row (of this device, above what
    /// the timeline held at the send) is in a page of the open chat. From then on a refusal for
    /// that chat is some other send's (the desktop's inline reply), and the entry must not give
    /// the sent text back. A chat that was not loaded at its newest messages when the text went
    /// has no baseline and keeps the entry until the next send replaces it.
    fn forget_stored_send(&mut self, page: &TimelinePage) {
        let stored = self.last_sent.as_ref().is_some_and(|s| {
            s.conv == page.conv
                && s.after.is_some_and(|after| {
                    page.rows
                        .iter()
                        .any(|r| r.origin == OriginKind::OwnThisDevice && r.id > after)
                })
        });
        if stored {
            self.last_sent = None;
        }
    }

    fn apply_page(&mut self, page: &TimelinePage, fetch: Fetch) {
        self.forget_stored_send(page);
        for r in &page.rows {
            if let (Some(a), Some(n)) = (&r.author, &r.author_name) {
                self.names.insert(a.clone(), n.expose().clone());
            }
        }
        // A row that was shown as read (or without a read state) is unread now: a call line
        // that turned into a missed call keeps its id, so "the newest incoming row was
        // marked already" says nothing about it. The chat is marked read again under the
        // usual rule (focused, visible, at its newest messages).
        if page.rows.iter().any(|r| {
            r.read_state == Some(ReadState::Unread)
                && self
                    .window
                    .rows
                    .iter()
                    .any(|old| old.id == r.id && old.read_state != Some(ReadState::Unread))
        }) {
            self.last_marked = None;
        }
        let first_load = !self.window.loaded;
        let w = &mut self.window;
        match fetch {
            Fetch::Reload => {
                let followed_newest = w.loaded && !w.has_newer;
                w.rows = page.rows.clone();
                w.has_older = page.has_older;
                w.has_newer = page.has_newer;
                // A burst bigger than the reload's slack: keep following the newest messages.
                if followed_newest && page.has_newer {
                    self.want_newer = true;
                    self.following = true;
                }
            }
            Fetch::Around(_) | Fetch::Newest => {
                // Another stretch of the conversation replaces the window.
                self.following = false;
                w.rows = page.rows.clone();
                w.has_older = page.has_older;
                w.has_newer = page.has_newer;
            }
            Fetch::Older => {
                // The user scrolls up: stop chasing the newest messages.
                self.following = false;
                let have: HashSet<MessageId> = w.rows.iter().map(|r| r.id).collect();
                let mut rows: Vec<CoreMessage> = page
                    .rows
                    .iter()
                    .filter(|r| !have.contains(&r.id))
                    .cloned()
                    .collect();
                rows.append(&mut w.rows);
                w.rows = rows;
                w.has_older = page.has_older;
                if w.rows.len() > TIMELINE_WINDOW {
                    w.rows.truncate(TIMELINE_WINDOW);
                    w.has_newer = true;
                }
            }
            Fetch::Newer => {
                let have: HashSet<MessageId> = w.rows.iter().map(|r| r.id).collect();
                w.rows
                    .extend(page.rows.iter().filter(|r| !have.contains(&r.id)).cloned());
                w.has_newer = page.has_newer;
                if self.following && page.has_newer {
                    self.want_newer = true;
                } else {
                    self.following = false;
                }
                if w.rows.len() > TIMELINE_WINDOW {
                    let extra = w.rows.len() - TIMELINE_WINDOW;
                    w.rows.drain(..extra);
                    w.has_older = true;
                }
            }
        }
        w.loaded = true;
        w.failed = false;
        if first_load {
            self.unread_from = first_unread(&self.window.rows);
        }
        self.drop_answered_marker();
        // One timer for the first action window that ends (`MessageActions::until`): the core
        // sends no event for time. `page.now` is the core's clock when it read the rows.
        let received = Instant::now();
        self.flags_deadline = self
            .window
            .rows
            .iter()
            .filter_map(|r| r.actions.until)
            .min()
            .map(|until| {
                let left = u64::try_from(until.saturating_sub(page.now)).unwrap_or(0);
                received + Duration::from_millis(left) + FLAGS_SLACK
            });
        // Live progress is kept only for downloads the window still shows as running.
        if !self.downloads.is_empty() {
            let running: HashSet<AttachmentId> = self
                .window
                .rows
                .iter()
                .flat_map(|r| attachments_of(&r.body))
                .filter(|a| matches!(a.state, AttachmentState::Downloading { .. }))
                .filter_map(|a| a.id)
                .collect();
            self.downloads.retain(|id, _| running.contains(id));
        }
        // A phase is kept while the row says its attempt runs; the next attempt reports again.
        if !self.send_phases.is_empty() {
            let running: HashSet<MessageId> = self
                .window
                .rows
                .iter()
                .filter(|r| r.outbox.as_ref().is_some_and(|o| o.in_flight))
                .map(|r| r.id)
                .collect();
            self.send_phases.retain(|id, _| running.contains(id));
        }
        // Upload bytes too: the next attempt reports its own.
        if !self.uploads.is_empty() {
            let running: HashSet<MessageId> = self
                .window
                .rows
                .iter()
                .filter(|r| r.outbox.as_ref().is_some_and(|o| o.in_flight))
                .map(|r| r.id)
                .collect();
            self.uploads.retain(|_, u| running.contains(&u.message));
        }
    }

    /// What an edit of `message` changes, from the core's row: its text, or its caption
    /// (`true`; empty for files without one). `None` when the core does not offer the edit
    /// (any more) or the message is not loaded.
    fn edit_source(&self, conv: ConversationId, message: MessageId) -> Option<(&Text, bool)> {
        static NO_CAPTION: std::sync::LazyLock<Text> = std::sync::LazyLock::new(|| Text::plain(""));
        if self.open != Some(conv) || !self.window.loaded {
            return None;
        }
        let row = self
            .window
            .rows
            .iter()
            .find(|r| r.id == message && r.actions.edit && r.deleted.is_none())?;
        match &row.body {
            Body::Text(text) => Some((text, false)),
            Body::Media {
                attachments,
                caption,
            } => {
                // As the timeline draws it: the message's caption, else its first file's; the
                // rest of a long text is no file.
                let text = caption
                    .as_ref()
                    .or_else(|| attachments.first().and_then(|a| a.caption.as_ref()))
                    .filter(|c| !c.body.trim().is_empty());
                let files = attachments.iter().any(|a| !a.long_text);
                Some((text.unwrap_or(&NO_CAPTION), files))
            }
            _ => None,
        }
    }

    /// The "N unread messages" divider goes once we answered: when the newest loaded message
    /// is our own and comes after the divider, there is nothing unread below it any more.
    fn drop_answered_marker(&mut self) {
        let Some(marker) = self.unread_from else {
            return;
        };
        let rows = &self.window.rows;
        if self.window.has_newer {
            return;
        }
        let Some(at) = rows.iter().position(|r| r.id == marker) else {
            return;
        };
        let answered = rows.last().is_some_and(|newest| {
            newest.origin != OriginKind::Remote && !matches!(newest.body, Body::System(_))
        });
        if answered && at + 1 < rows.len() {
            self.unread_from = None;
        }
    }

    /// Sends `MarkRead` when the open conversation is visible, focused, scrolled to its newest
    /// messages and has an incoming message not marked yet.
    fn maybe_mark_read(&mut self) {
        let Some(conv) = self.open else { return };
        if !(self.focused && self.chat_visible && self.window.loaded && !self.window.has_newer) {
            return;
        }
        let Some(newest) = self
            .window
            .rows
            .iter()
            .rev()
            .find(|r| r.origin == OriginKind::Remote)
            .map(|r| r.id)
        else {
            return;
        };
        let unread_row = self
            .window
            .rows
            .iter()
            .any(|r| r.read_state == Some(ReadState::Unread));
        let unread_list = self
            .list
            .iter()
            .any(|c| c.id == conv && (c.unread > 0 || c.marked_unread));
        if !(unread_row || unread_list) || self.last_marked == Some((conv, newest)) {
            return;
        }
        self.last_marked = Some((conv, newest));
        self.send(UiCommand::MarkRead { conv });
    }

    /// The core's row of `conv`: the list's, else the open chat's detail (a chat the list does not
    /// show: another account's under the filter, a search hit).
    fn row_of(&self, conv: ConversationId) -> Option<&CoreConversation> {
        self.list.iter().find(|c| c.id == conv).or_else(|| {
            self.detail
                .as_ref()
                .map(|d| &d.row)
                .filter(|row| row.id == conv)
        })
    }

    /// What a click on a chat's menu waits for before it is told.
    fn await_told(&mut self, conv: ConversationId, kind: AwaitedKind, was_pinned: bool) {
        // A newer click on the same question replaces the older one.
        self.awaited
            .retain(|a| !(a.conv == conv && a.kind.same_family(kind)));
        let asked_at = self.now_ms();
        self.awaited.push(Awaited {
            conv,
            kind,
            was_pinned,
            asked_at,
        });
    }

    /// Turns the clicks whose change the list shows now into notices, and forgets the ones older
    /// than [`TOLD_WAIT_MS`]. `true` when a notice is ready.
    fn settle_awaited(&mut self) -> bool {
        if self.awaited.is_empty() {
            return false;
        }
        let now = self.now_ms();
        let list = &self.list;
        let mut ready = Vec::new();
        self.awaited.retain(|a| {
            if now.saturating_sub(a.asked_at) > TOLD_WAIT_MS {
                return false;
            }
            let Some(row) = list.iter().find(|c| c.id == a.conv) else {
                return true;
            };
            let (done, told) = match a.kind {
                AwaitedKind::Archive => (
                    archived_row(row),
                    ChatTold::Archived {
                        conv: a.conv,
                        was_pinned: a.was_pinned,
                    },
                ),
                AwaitedKind::Unarchive => {
                    (!archived_row(row), ChatTold::Unarchived { conv: a.conv })
                }
                AwaitedKind::MarkUnread => {
                    (row.marked_unread, ChatTold::MarkedUnread { conv: a.conv })
                }
            };
            if done {
                ready.push(told);
            }
            !done
        });
        let any = !ready.is_empty();
        self.told.extend(ready);
        any
    }

    /// What `account` says about itself for the menu of one of its chats: whether it is linked
    /// and running (the condition of "Unlink…"), its pin limit and how many chats of it the list
    /// holds pinned. The list holds no hidden row (the read drops them), so the count is the
    /// core's own (`roaming::check` counts `hidden = 0`).
    fn account_facts(&self, account: AccountId) -> AccountFacts {
        let row = self.accounts.iter().find(|a| a.id == account);
        let pinned = self
            .list
            .iter()
            .filter(|c| c.account == account && c.pinned_order.is_some())
            .count();
        AccountFacts {
            linked: row.is_some_and(|a| self.offers_unlink(a)),
            pin_limit: row.and_then(|a| a.pin_limit),
            pinned: u32::try_from(pinned).unwrap_or(u32::MAX),
        }
    }

    /// The offer of the core's row `c`, with its account's facts.
    fn offer_for(&self, c: &CoreConversation, time: &TimeContext<'_>) -> ChatOffer {
        roaming::offer_in_account(c, time, self.account_facts(c.account))
    }

    fn send_presence(&mut self) {
        let visible = if self.chat_visible { self.open } else { None };
        let now = (self.focused, visible);
        if self.presence_sent != Some(now) {
            self.presence_sent = Some(now);
            self.send(UiCommand::SetPresence {
                focused: self.focused,
                visible,
            });
        }
    }

    fn close(&mut self) -> Changes {
        if self.open.is_none() {
            return Changes::default();
        }
        if let Some((conv, _)) = self.typing_sent.take() {
            self.send(UiCommand::SetTyping { conv, on: false });
        }
        self.open = None;
        self.detail = None;
        self.detail_seeded = false;
        self.window = Window::default();
        self.read_more.clear();
        self.unread_from = None;
        // A chat opened again is read again, and that clears a mark the user put on it
        // (Desktop's): `last_marked` says "the newest message was marked already" for a chat
        // that was open, and would keep a mark the chat in between never touched.
        self.last_marked = None;
        self.timeline_q.reset();
        self.detail_q.reset();
        self.fetching = None;
        self.want_reload = false;
        self.want_older = false;
        self.want_newer = false;
        self.want_around = None;
        self.want_newest = false;
        self.following = false;
        self.flags_deadline = None;
        self.downloads.clear();
        self.send_phases.clear();
        self.uploads.clear();
        self.revisions_shown = None;
        self.send_presence();
        Changes {
            timeline: true,
            header: true,
            ..Changes::default()
        }
    }

    /// The open conversation changed while the edit history of one of its messages is on
    /// screen: the history is read again (another edit may have arrived, or the message may
    /// be gone).
    fn refresh_revisions(&mut self) {
        let Some(message) = self.revisions_shown else {
            return;
        };
        self.revisions_wanted = Some(message);
        let t = self.revisions_q.request();
        self.send_revisions(t);
    }

    fn login_failed(&mut self, error: String) -> Changes {
        let Some(login) = self.login.as_mut() else {
            return Changes::default();
        };
        login.account = None;
        login.view = LoginView {
            kind: LoginViewKind::Failed,
            title: trc("@title:window", "Linking Failed").into_owned(),
            error,
            retry: true,
            progress: -1.0,
            ..LoginView::default()
        };
        Changes {
            login: true,
            ..Changes::default()
        }
    }

    fn notice_change(&mut self, text: String) -> Changes {
        self.notice = text;
        Changes {
            notice: true,
            ..Changes::default()
        }
    }

    /// The pages of the details panel that show the timer of `conv` say why its last change
    /// failed (`text`), or, with an empty text, forget it (F4, critic M6): the person's page of
    /// her 1:1 chat and the group's page. The failure lives in the page the source holds
    /// (`person_last`, `group_last`), so a refresh carries it on and a new ask for a page
    /// (which empties them) or the next change of any timer clears it. `conv` `None`: every
    /// page.
    fn timer_failure_on_pages(&mut self, conv: Option<ConversationId>, text: &str) -> Changes {
        let mut ch = Changes::default();
        if let Some(view) = self.person_last.as_mut()
            && conv.is_none_or(|c| view.chat == Some(c))
            && view.timer_failed != text
        {
            text.clone_into(&mut view.timer_failed);
            self.person_answer = Some(PersonUpdate::Show(Box::new(view.clone())));
            ch.person = true;
        }
        if let Some(view) = self.group_last.as_mut()
            && conv.is_none_or(|c| view.id == c)
            && view.timer_failed != text
        {
            text.clone_into(&mut view.timer_failed);
            self.group_answer = Some(GroupUpdate::Show(Box::new(view.clone())));
            ch.group = true;
        }
        ch
    }

    /// The loaded row of `message` in the open timeline.
    fn loaded_row(&self, message: Option<MessageId>) -> Option<&CoreMessage> {
        let message = message?;
        self.window.rows.iter().find(|r| r.id == message)
    }

    /// Whether a quote of `target` is what the core refused: the message is gone, deleted, or
    /// no longer offers a reply.
    fn quote_gone(&self, target: Option<MessageId>) -> bool {
        target.is_some_and(|_| {
            self.loaded_row(target)
                .is_none_or(|r| r.deleted.is_some() || !r.actions.reply)
        })
    }

    /// A failed action shows as a notice. Which one is decided by the kind of the failure
    /// ([`Failure`]: the core refused the request, the network did, the build cannot do it)
    /// and by what the rows say about the message now, never by the wording of the error:
    /// the core may reword its reasons freely.
    fn on_action_failed(
        &mut self,
        action: &str,
        error: &CoreError,
        subject: &ActionSubject,
    ) -> Changes {
        let failure = Failure::of(error);
        let refused_here = failure == Failure::Refused;
        match action {
            "SendText" | "RetryMessage" | "CancelMessage" => {
                // The text goes back only when this refusal is of the send the composer made:
                // the core names the conversation of a refused `SendText`, and a send of
                // another front-end (the desktop's inline reply) or into another chat is not
                // ours. The entry stays for its own refusal, which may still come.
                let sent = match subject {
                    ActionSubject::Conversation(conv)
                        if action == "SendText"
                            && self.last_sent.as_ref().is_some_and(|s| s.conv == *conv) =>
                    {
                        self.last_sent.take()
                    }
                    _ => None,
                };
                let quoted = sent.as_ref().and_then(|s| s.reply);
                self.notice = if failure == Failure::Unavailable {
                    trc("@info:status", "Sending messages is not available yet.").into_owned()
                } else if refused_here && self.quote_gone(quoted) {
                    trc(
                        "@info:status",
                        "The message you reply to is no longer there. Send it without the quote.",
                    )
                    .into_owned()
                } else if action == "CancelMessage" {
                    trc(
                        "@info:status",
                        "The message is already on its way and can no longer be taken back.",
                    )
                    .into_owned()
                } else if failure == Failure::Network(NetworkFailure::RateLimited) {
                    trc(
                        "@info:status",
                        "The network asks to slow down. Try again in a little while.",
                    )
                    .into_owned()
                } else {
                    trc("@info:status", "The message could not be sent.").into_owned()
                };
                let mut ch = Changes {
                    notice: true,
                    ..Changes::default()
                };
                // Nothing is lost: the refused text goes back into the composer, and is stored
                // as the chat's draft again. The composer cleared the stored draft before it
                // sent (`send_text`), and its next `save_draft` of this very text would find it
                // unchanged and send nothing, so without this a quit keeps nothing of it.
                if let Some(LastSent { conv, text, .. }) = sent
                    && !self.drafts.contains_key(&conv)
                {
                    let draft = crate::composer::outgoing_text(&text);
                    self.drafts.insert(conv, text);
                    self.send(UiCommand::SaveDraft { conv, draft });
                    ch.draft = self.open == Some(conv);
                    ch.conversations = true;
                }
                ch
            }
            "SendFiles" => match subject {
                ActionSubject::Files(refused) => self.on_files_refused(refused, failure),
                _ => self.notice_change(
                    trc("@info:status", "The files could not be sent.").into_owned(),
                ),
            },
            "EditMessage" => {
                let row = self.loaded_row(subject.message());
                let edits = self.detail.as_ref().and_then(|d| d.features.edit);
                // The core's own reason, where it names one: no guessing from the rows.
                let named = match error.refusal() {
                    Some(Refusal::TooOld) => {
                        Some(trc("@info:status", "This message can no longer be edited."))
                    }
                    Some(Refusal::EditedTooOften) => Some(trc(
                        "@info:status",
                        "This message was edited too often to change it again.",
                    )),
                    Some(Refusal::Deleted | Refusal::Gone) => {
                        Some(trc("@info:status", "This message was deleted."))
                    }
                    Some(Refusal::BeingSent) => Some(trc(
                        "@info:status",
                        "This message is being sent. Edit it once it is.",
                    )),
                    Some(Refusal::TooLong) => Some(trc(
                        "@info:status",
                        "This text is too long for one message. Shorten it to save the edit.",
                    )),
                    // The text is what it was: nothing to say, the rows are read again.
                    Some(Refusal::Unchanged) => {
                        self.reload_timeline();
                        return Changes::default();
                    }
                    _ => None,
                };
                if let Some(text) = named {
                    return self.notice_change(text.into_owned());
                }
                let text = if !refused_here {
                    trc(
                        "@info:status",
                        "The edit could not be sent. The message is as it was.",
                    )
                } else if row.is_some_and(|r| r.deleted.is_some()) {
                    trc("@info:status", "This message was deleted.")
                } else if let (Some(r), Some(max)) = (row, edits.and_then(|w| w.max_count))
                    && r.revision >= max
                {
                    trc(
                        "@info:status",
                        "This message was edited too often to change it again.",
                    )
                } else if row.is_none_or(|r| !r.actions.edit) {
                    trc("@info:status", "This message can no longer be edited.")
                } else {
                    // The row still offers the edit: the request itself was not taken (the
                    // text is what it was, or the window ended this very moment). The rows
                    // are read again and say what holds now.
                    self.reload_timeline();
                    return Changes::default();
                };
                self.notice_change(text.into_owned())
            }
            "DeleteMessage" => {
                let row = self.loaded_row(subject.message());
                let text = if !refused_here {
                    // The tombstone stays, for this account only (§3.5.2).
                    trc(
                        "@info:status",
                        "The network did not delete the message for everyone. It is deleted on your devices only.",
                    )
                } else if row
                    .is_some_and(|r| matches!(r.out_state, Some(OutgoingState::Sending { .. })))
                {
                    trc(
                        "@info:status",
                        "This message is being sent. Delete it once it is.",
                    )
                } else if row
                    .is_some_and(|r| r.actions.delete_for_me && !r.actions.delete_for_everyone)
                {
                    trc(
                        "@info:status",
                        "This message can no longer be deleted for everyone.",
                    )
                } else {
                    trc("@info:status", "This message cannot be deleted.")
                };
                self.notice_change(text.into_owned())
            }
            "React" => {
                let row = self.loaded_row(subject.message());
                let text = if !refused_here {
                    trc("@info:status", "The reaction could not be sent.")
                } else if row.is_some_and(|r| r.deleted.is_some()) {
                    trc("@info:status", "This message was deleted.")
                } else {
                    trc("@info:status", "This reaction is not possible here.")
                };
                self.notice_change(text.into_owned())
            }
            "SetConversationTimer" => {
                let text = if refused_here {
                    trc(
                        "@info:status",
                        "This timer is not available in this conversation.",
                    )
                } else {
                    trc(
                        "@info:status",
                        "The disappearing-message timer could not be changed.",
                    )
                };
                // The pages that show this chat's timer say it under the selector too: the
                // notice is the open chat's composer's, hidden under a covering panel and
                // another chat's for a member's page (F4, critic M6).
                let mut ch = match subject {
                    ActionSubject::Conversation(conv) => {
                        self.timer_failure_on_pages(Some(*conv), &text)
                    }
                    _ => Changes::default(),
                };
                ch.merge(self.notice_change(text.into_owned()));
                ch
            }
            // (A failure here is storage or a chat that went away meanwhile: the core
            // refuses neither for a reason of the network.)
            "ClearConversation" => self.notice_change(
                trc("@info:status", "The chat history could not be deleted.").into_owned(),
            ),
            "DeleteConversation" => self
                .notice_change(trc("@info:status", "The chat could not be deleted.").into_owned()),
            "DeleteAttachment" => {
                let text = match error.refusal() {
                    Some(Refusal::BeingSent) => trc(
                        "@info:status",
                        "This message is being sent. Delete the attachment once it is.",
                    ),
                    Some(Refusal::Deleted | Refusal::Gone) => {
                        trc("@info:status", "This attachment is no longer there.")
                    }
                    _ => trc("@info:status", "The attachment could not be deleted."),
                };
                self.notice_change(text.into_owned())
            }
            "SubmitChallenge" => self.notice_change(
                trc(
                    "@info:status",
                    "The verification was not accepted. Try it again from the banner.",
                )
                .into_owned(),
            ),
            "Relink" => {
                if self.login.is_some() {
                    self.login_failed(
                        trc("@info:status", "This account could not be linked again.").into_owned(),
                    )
                } else {
                    Changes::default()
                }
            }
            "DownloadAttachment" => self.notice_change(
                trc("@info:status", "The attachment could not be downloaded.").into_owned(),
            ),
            "SaveAttachment" => self.on_save_failed(error, subject),
            "OpenMedia" => {
                self.media_answers
                    .push(MediaAnswer::Failed(subject.attachment()));
                Changes {
                    media: true,
                    ..Changes::default()
                }
            }
            // The oldest `AddAccount` still waiting failed; nothing to show if the user
            // cancelled that login already.
            "AddAccount" => {
                let news = self.replace_news.len();
                let mut ch = if self.pop_add(None) == Some(true) {
                    Changes::default()
                } else {
                    self.login_failed(if failure == Failure::Unavailable {
                        trc(
                            "@info:status",
                            "Linking is not available in this build yet.",
                        )
                        .into_owned()
                    } else {
                        trc("@info:status", "The account could not be created.").into_owned()
                    })
                };
                // A replace that ended with it: the choice is pruned at once.
                ch.accounts |= self.replace_news.len() != news;
                ch
            }
            // The option could not be stored: it is what it was. The core says which that is,
            // and the settings page shows it again.
            "SetExpiryPolicy" => {
                self.settings_q.reset();
                self.request_expiry();
                self.notice_change(
                    trc("@info:status", "The setting could not be saved.").into_owned(),
                )
            }
            // The same for the quiet notices: the choice is what it was and the core changed
            // nothing, so the list read behind the command already shows the old order.
            "SetQuietNotices" => {
                self.quiet_q.reset();
                self.request_quiet();
                self.notice_change(
                    trc("@info:status", "The setting could not be saved.").into_owned(),
                )
            }
            // A Mark, Clear or Confirm that failed (P5a). ONLY the backend's refusal of a stale
            // key (`InvalidInput`: the number changed after it was shown) is worded as a changed
            // number; anything else (the account is not running, the store failed) says the
            // change was not saved and nothing about the number (U1). Either way the level of
            // that person asks again and shows what is true now.
            "SetVerified" | "ConfirmIdentity" => {
                if let ActionSubject::Person(user) = subject
                    && self.safety_shown.as_ref().is_some_and(|(_, u)| u == user)
                {
                    let stale = matches!(
                        error,
                        CoreError::Backend(e) if e.kind == ErrorKind::InvalidInput
                    );
                    self.safety_notice = if !stale {
                        crate::safety::not_saved_text()
                    } else if action == "ConfirmIdentity" {
                        crate::safety::refused_text(crate::safety::SafetyAction::Confirm)
                    } else {
                        crate::safety::refused_text(crate::safety::SafetyAction::Mark)
                    };
                    self.safety_last = None;
                    self.safety_key = None;
                    self.refresh_safety();
                }
                Changes::default()
            }
            // A contact card the core refused (P5c: the chat cannot take it, the person's page
            // offers no card any more, the account is not running): nothing was queued.
            "SendContact" => self.notice_change(
                trc(
                    "@info:status a contact card was not sent",
                    "The contact card could not be sent.",
                )
                .into_owned(),
            ),
            // A roaming change the core or the network refused (A10d). The click's notice never
            // comes (the entry is dropped), and the words say what is known: a pin beyond the
            // account's limit names the limit (the core's `TooMany`, or the network's own count,
            // which can list chats this device does not), anything else says the change was not
            // made. The pin limit is the account's, never a constant.
            "ChangeConversation" => {
                let conv = match subject {
                    ActionSubject::Conversation(conv) => Some(self.survivor(*conv)),
                    _ => None,
                };
                if let Some(conv) = conv {
                    self.awaited.retain(|a| a.conv != conv);
                }
                let too_many = matches!(error.refusal(), Some(Refusal::TooMany))
                    || matches!(error, CoreError::Backend(e) if e.kind == ErrorKind::TooLarge);
                let limit = conv
                    .and_then(|c| self.row_of(c))
                    .and_then(|c| self.accounts.iter().find(|a| a.id == c.account))
                    .and_then(|a| a.pin_limit);
                let text = match limit {
                    Some(limit) if too_many => roaming::pin_limit_text(limit),
                    _ => trc("@info:status", "The change could not be made.").into_owned(),
                };
                // A toast, not the composer: the click came from the list as often as from
                // the open chat, and the composer may be off screen or another chat's.
                match conv {
                    Some(conv) => {
                        self.told.push(ChatTold::Refused { conv, words: text });
                        Changes {
                            told: true,
                            ..Changes::default()
                        }
                    }
                    None => self.notice_change(text),
                }
            }
            // The sync switch was refused: a notice of its own, no composer notice (the
            // composer is not on screen over Settings).
            "SetRoamingWrites" => {
                self.told.push(ChatTold::SwitchRefused);
                Changes {
                    told: true,
                    ..Changes::default()
                }
            }
            // Drafts, typing and read markers fail quietly (they are retried by the next use).
            _ => Changes::default(),
        }
    }

    /// The core did not take a `SendFiles` (nothing of it was queued). When it names the
    /// files it cannot take, the others go out again at once without them and only the named
    /// ones return to the composer, each with its reason: one file the network cannot carry
    /// never holds the others back. Otherwise the whole request returns.
    fn on_files_refused(&mut self, refused: &RefusedFiles, failure: Failure) -> Changes {
        let conv = self.survivor(refused.conv);
        let paths = refused.paths.expose();
        self.end_preparing(refused.request, SendOutcome::Refused);
        let mut ch = Changes {
            notice: true,
            files: true,
            ..Changes::default()
        };
        let reason_of = |index: usize| {
            refused
                .refused
                .iter()
                .find(|r| r.index == index)
                .map(|r| r.reason)
        };
        let flag = |flags: &[bool], index: usize| flags.get(index).copied().unwrap_or(false);
        // The files the core did not name go out again, each with the choices made for it
        // (as a file, and the agreement to send it with its location), re-indexed to the rest.
        let rest: Vec<(std::path::PathBuf, bool, bool)> = paths
            .iter()
            .enumerate()
            .filter(|(i, _)| reason_of(*i).is_none())
            .map(|(i, p)| {
                (
                    p.clone(),
                    flag(&refused.as_file, i),
                    flag(&refused.with_location, i),
                )
            })
            .collect();
        let named = paths.len() - rest.len();
        let resent_request = if named > 0 && !rest.is_empty() {
            let again = rest.iter().map(|r| r.0.clone()).collect();
            let as_file = rest.iter().map(|r| r.1).collect();
            let with_location = rest.iter().map(|r| r.2).collect();
            self.send_files_request(
                conv,
                again,
                as_file,
                with_location,
                refused.caption.clone(),
                refused.reply_to,
            )
        } else {
            None
        };
        let resent = if resent_request.is_some() {
            rest.len()
        } else {
            0
        };
        let caption = refused
            .caption
            .as_ref()
            .map(|c| c.body.clone())
            .unwrap_or_default();
        self.notice = if named > 0 {
            refused_notice(named, resent, self.open == Some(conv))
        } else if failure == Failure::Refused && self.quote_gone(refused.reply_to) {
            trc(
                "@info:status",
                "The message you reply to is no longer there. Send it without the quote.",
            )
            .into_owned()
        } else {
            trc("@info:status", "The files could not be sent.").into_owned()
        };
        // What did not go out again returns: the caption as the draft, the files and the
        // quote to the composer.
        if resent == 0 && !caption.trim().is_empty() && !self.drafts.contains_key(&conv) {
            self.drafts.insert(conv, caption.clone());
            ch.draft = self.open == Some(conv);
            ch.conversations = true;
        }
        self.refused_files.push_back(RefusedSend {
            conv,
            request: refused.request.0,
            files: paths
                .iter()
                .enumerate()
                .filter(|(i, _)| resent == 0 || reason_of(*i).is_some())
                .map(|(i, p)| ReturnedFile {
                    path: p.clone(),
                    as_file: flag(&refused.as_file, i),
                    reason: reason_of(i),
                })
                .collect(),
            reply_to: refused.reply_to.filter(|_| resent == 0),
            resent,
            resent_request: resent_request.map(|r| r.0),
        });
        ch
    }

    /// Live progress of a download shown in the open timeline: the row is rebuilt when the
    /// shown percentage changes.
    fn on_download_progress(&mut self, attachment: AttachmentId, done: u64, total: u64) -> Changes {
        // The gallery slots that hold the attachment (people pages P4) show it too.
        let mut ch = self.media_download_progress(attachment, done, total);
        if self.open.is_none() || total == 0 {
            return ch;
        }
        let shown = self
            .window
            .rows
            .iter()
            .flat_map(|r| attachments_of(&r.body))
            .any(|a| a.id == Some(attachment));
        if !shown {
            return ch;
        }
        let p = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
        let old = self.downloads.insert(attachment, p).unwrap_or(0.0);
        ch.timeline |= (p * 100.0).floor() != (old * 100.0).floor();
        ch
    }

    /// `UiEvent::HistoryTransfer`: where the transfer of `account` stands now. The same
    /// state can come any number of times (at every start and connect, an end until it is
    /// dismissed): an end is announced once, and the backend is told to forget it each time
    /// it reports it, so a dismissal that crossed a report is made up for.
    fn on_history_transfer(&mut self, account: AccountId, state: HistoryTransfer) -> Changes {
        let out = self.transfers.on_state(account, state);
        if let Some(login) = self.login.as_mut()
            && login.relink.is_none()
            && login.account == Some(account)
        {
            login.transferred = true;
        }
        let page_ended = self.end_link_page();
        if out.dismiss {
            self.send(UiCommand::DismissHistoryTransfer { account });
        }
        if let Some(notice) = out.notice {
            let mut text = notice.text();
            // With several accounts the notice says which one it is about.
            if self.visible_accounts().count() > 1
                && let Some(a) = self.visible_accounts().find(|a| a.id == account)
            {
                text = crate::transfer::named_notice(&Self::account_label(a), &text);
            }
            self.transfer_notices.push(text);
        }
        // A transfer that ended (a failed import too) fills nothing any more.
        if state.is_over() {
            self.filling.retain(|_, a| *a != account);
        }
        Changes {
            transfer: out.changed || out.notice.is_some(),
            login: page_ended,
            // Whether the open chat can be cleared or deleted depends on the transfer.
            header: out.changed && self.open_account() == Some(account),
            ..Changes::default()
        }
    }

    /// The account of the open chat.
    fn open_account(&self) -> Option<AccountId> {
        self.detail.as_ref().map(|d| d.row.account)
    }

    /// Whether the chat `conv` cannot be cleared or deleted now ([`Self::still_filling`]);
    /// its account from the open chat or the list.
    fn fills_now(&self, conv: ConversationId) -> bool {
        let account = self
            .detail
            .as_ref()
            .map(|d| &d.row)
            .filter(|c| c.id == conv)
            .or_else(|| self.list.iter().find(|c| c.id == conv))
            .map(|c| c.account);
        match account {
            Some(account) => !self.still_filling(conv, account).is_empty(),
            None => self.filling.contains_key(&conv),
        }
    }

    /// Why the chat `conv` of `account` cannot be cleared or deleted now; empty when it
    /// can. While a history transfer of its account runs, or its imported history still
    /// arrives, a cleared chat would fill again (the user's decision, link feedback L3).
    fn still_filling(&self, conv: ConversationId, account: AccountId) -> String {
        let runs = self.transfers.running(account) && !self.logged_out(account);
        if !runs && !self.filling.contains_key(&conv) {
            return String::new();
        }
        trc(
            "@info:status a disabled entry of the chat menu: why the chat cannot be cleared or deleted yet",
            "Available once this chat's history has arrived",
        )
        .into_owned()
    }

    /// Ends the link page as soon as the core has the login's account (link feedback L3,
    /// the user's decision: chats usable as soon as the account is linked): it is Active in
    /// the core's list (it took `AccountReady`), or the backend reported a history transfer
    /// of a fresh link (it comes only after the link). The page says "Linked" and leaves as
    /// at the backend's `Done`; the transfer and the rest of the link's work show in the
    /// banner. The backend's later steps of this login find the page ended. Returns whether
    /// the page changed.
    fn end_link_page(&mut self) -> bool {
        let Some(login) = self.login.as_ref() else {
            return false;
        };
        if matches!(login.view.kind, LoginViewKind::Done | LoginViewKind::Failed) {
            return false;
        }
        if !(login.transferred || self.login_account_active(login)) {
            return false;
        }
        let network = login.network;
        let view = login_view(
            &LoginStep {
                step_id: "done".to_owned(),
                txn: 0,
                kind: LoginStepKind::Done,
            },
            network,
        );
        if let Some(login) = self.login.as_mut() {
            login.view = view;
            login.ended_linked = true;
        }
        self.request_list();
        true
    }

    /// The login flows of `network` as (id, descriptor label), the first is the default.
    fn flows_of(&self, network: NetworkKind) -> &[(String, String)] {
        self.networks
            .iter()
            .find(|n| n.kind == network)
            .map_or(&[], |n| n.flows.as_slice())
    }

    /// The flow a login of `network` starts with: the first one it offers (`qr` for a network
    /// the list does not know).
    fn first_flow(&self, network: NetworkKind) -> String {
        self.flows_of(network)
            .first()
            .map_or_else(|| "qr".to_owned(), |(id, _)| id.clone())
    }

    /// `view` of a login that runs `flow`: names the flow and, where switching is a way on (a
    /// code to scan, a number to type, a code to enter, or a failure), the other flow of the
    /// network with the label of the button. A step in the middle of the link (`Progress`, the
    /// emoji) offers none: a switch there would cancel a link that is nearly made. So does the
    /// password and the e-mail of Telegram's login and their retries (reached by a QR scan or
    /// by the phone flow, the account is known by then), and a failure that goes back with "OK"
    /// (a banned number, a number without an account: the other way fails the same).
    fn with_flows(&self, mut view: LoginView, network: NetworkKind, flow: &str) -> LoginView {
        view.flow = flow.to_owned();
        view.other_flow.clear();
        view.other_flow_label.clear();
        let offered = match view.kind {
            LoginViewKind::Qr | LoginViewKind::Code => true,
            LoginViewKind::Input => !matches!(
                view.step_id.as_str(),
                "password"
                    | "password-again"
                    | "email"
                    | "email-again"
                    | "email-code"
                    | "email-code-again"
            ),
            // A link that cannot be tried again leaves for the list instead.
            LoginViewKind::Failed => view.leave.is_empty(),
            _ => false,
        };
        if offered
            && let Some((id, label)) = self.flows_of(network).iter().find(|(id, _)| id != flow)
        {
            view.other_flow = id.clone();
            view.other_flow_label = crate::login::flow_switch_label(id, label);
        }
        view
    }

    /// `view` with the number typed earlier into the same step put back into its fields.
    fn with_typed(view: LoginView, typed: Option<&Typed>) -> LoginView {
        let mut view = view;
        if let Some(typed) = typed.filter(|t| t.step_id == view.step_id) {
            for f in &mut view.fields {
                if let Some((_, value)) = typed.values.iter().find(|(id, _)| *id == f.id) {
                    f.value = value.expose().clone();
                }
            }
        }
        view
    }

    /// The waiting page of a login to `network` that runs `flow`.
    fn waiting_view(&self, network: NetworkKind, flow: &str) -> LoginView {
        self.with_flows(
            LoginView {
                kind: LoginViewKind::Waiting,
                title: trfc(
                    "@title:window page to link an account, %1 the name of a messenger",
                    "Link %1",
                    &[network_name(network)],
                ),
                progress: -1.0,
                icon: network_icon(network).to_owned(),
                ..LoginView::default()
            },
            network,
            flow,
        )
    }

    /// Sends `AddAccount` for `network` with `flow` and makes its login the one the link page
    /// shows.
    fn add_account(
        &mut self,
        network: NetworkKind,
        flow: &str,
        typed: Option<Typed>,
    ) -> Result<LoginView, SourceError> {
        let view = self.waiting_view(network, flow);
        self.login = Some(Login {
            network,
            account: None,
            view: view.clone(),
            relink: None,
            transferred: false,
            ended_linked: false,
            flow: flow.to_owned(),
            typed,
        });
        if !self.send(UiCommand::AddAccount {
            network,
            flow: flow.into(),
        }) {
            return Err(SourceError::Unavailable);
        }
        self.adds.push_back(false);
        Ok(view)
    }

    /// Sends `Relink` for `account` with `flow` and makes its login the one the link page shows.
    fn relink_with(
        &mut self,
        account: AccountId,
        flow: &str,
        typed: Option<Typed>,
    ) -> Result<LoginView, SourceError> {
        let network = self.account_network(account);
        let view = self.waiting_view(network, flow);
        if !self.send(UiCommand::Relink {
            account,
            flow: flow.into(),
        }) {
            return Err(SourceError::Unavailable);
        }
        // The account exists already: its login steps are ours from the first one.
        self.cancelled.remove(&account);
        self.login = Some(Login {
            network,
            account: Some(account),
            view: view.clone(),
            relink: Some(account),
            transferred: false,
            ended_linked: false,
            flow: flow.to_owned(),
            typed,
        });
        Ok(view)
    }

    fn device_name_default(&self, mut view: LoginView) -> LoginView {
        for f in &mut view.fields {
            if f.id == "device-name" && !self.hostname.is_empty() {
                f.value = self.hostname.clone();
            }
        }
        view
    }

    // ---------- row building ----------

    fn account_rows(&self) -> Vec<AccountRow> {
        let now = self.now_ms();
        self.visible_accounts()
            .map(|a| {
                let conn = self.conn.get(&a.id).unwrap_or(&a.conn);
                let mut status = AccountStatus::new(&a.state, conn);
                if self.account_errors.contains(&a.id) {
                    status = AccountStatus::NeedsAction;
                }
                let network_name = network_name(a.network).to_owned();
                // (the archive's unread messages count nowhere, as on Signal Desktop's badge)
                let unread = self
                    .list
                    .iter()
                    .filter(|c| c.account == a.id && !archived_row(c) && !muted(c, now))
                    .map(|c| c.unread)
                    .sum();
                AccountRow {
                    id: a.id,
                    uuid: a.uuid,
                    name: a
                        .display_name
                        .as_deref()
                        .map(single_line)
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| network_name.clone()),
                    network: a.network,
                    network_icon: network_icon(a.network).to_owned(),
                    network_name,
                    tint: self.tint_of(a),
                    own_tint: self.own_tints.get(&a.uuid).cloned().unwrap_or_default(),
                    status,
                    status_label: status.label(),
                    unread,
                    avatar_url: self.avatar_url(a.id, a.avatar.as_ref()),
                    number: a.number.as_deref().map(single_line).unwrap_or_default(),
                    offers_history: self.offers_history(a),
                    offers_unlink: self.offers_unlink(a),
                    device: self.twin_label(a).unwrap_or_default(),
                    shown: true,
                    place: 0,
                    roaming_writes: a.roaming_writes,
                    roaming_note: self.roaming_note(a),
                }
            })
            .collect()
    }

    /// Whether `a` offers "Transfer History from Phone…": its network hands a newly linked
    /// device the history as one archive (the descriptor's `history_transfer`), it can be linked
    /// in this build, no transfer of it runs, and its link is not still going on (a link's
    /// tail after the transfer, `buttons.md` F4: confirming would delete what just arrived
    /// and ask the phone for all of it again).
    fn offers_history(&self, a: &CoreAccount) -> bool {
        self.networks
            .iter()
            .any(|n| n.kind == a.network && n.available && n.on && n.history_transfer)
            && !self.transfer_runs(a)
            && !self.links_now(a.id)
    }

    /// Which link `a` is ("device 3 · linked 12 Sep") when another visible account of its
    /// network has the same name and number: two links of one identity (a profile from before
    /// one identity was one account, link feedback D10) are told apart by what the phone shows
    /// of each (its link date) and the device number, where the user picks one.
    fn twin_label(&self, a: &CoreAccount) -> Option<String> {
        let label = Self::account_label(a);
        self.visible_accounts()
            .any(|b| {
                b.id != a.id
                    && b.network == a.network
                    && b.number == a.number
                    && Self::account_label(b) == label
            })
            .then(|| crate::transfer::device_label(a.own_device, &self.link_date(a.linked_at)))
            .filter(|label| !label.is_empty())
    }

    /// The day a device was linked, in the user's locale; empty when it is not known.
    fn link_date(&self, linked_at: i64) -> String {
        if linked_at <= 0 {
            return String::new();
        }
        UiSource::time(self).short_date(linked_at)
    }

    /// Which account `a` is, for a question about it: its name, its number where the network
    /// has one, and its device when another account looks the same.
    fn account_about(&self, a: &CoreAccount) -> String {
        let number = a.number.as_deref().map(single_line).unwrap_or_default();
        let about = crate::transfer::account_line(
            &Self::account_label(a),
            &number,
            network_name(a.network),
        );
        match self.twin_label(a) {
            Some(label) => crate::transfer::with_device(&about, &label),
            None => about,
        }
    }

    /// Whether `a` offers "Unlink…": it is linked (Active, its live state not logged out),
    /// its link is not still going on and its network is not turned off.
    fn offers_unlink(&self, a: &CoreAccount) -> bool {
        matches!(a.state, AccountState::Active)
            && !self.logged_out(a.id)
            && !self.links_now(a.id)
            // Unlinking asks the network to forget the device; a network that is turned off
            // cannot be asked (the core refuses it too). "Remove Account…" stays.
            && !self.network_paused(a.network)
    }

    /// Why the sync switch of `a` cannot be flipped now; empty when it can, or when the account
    /// has no such switch. The condition is "Unlink…"'s without its wording: the zone that
    /// stores the switch is local, so an account that is only offline can flip it, but one that
    /// is not linked (logged out, its link still going on, its network turned off) cannot.
    fn roaming_note(&self, a: &CoreAccount) -> String {
        if a.roaming_writes.is_some() && !self.offers_unlink(a) {
            roaming::not_linked_text()
        } else {
            String::new()
        }
    }

    /// Whether a login of `account` runs: on the link page or left to the background. A
    /// logged-out account has none (its backend forgot it with the login).
    fn links_now(&self, account: AccountId) -> bool {
        self.linking(account)
            || (self.unfinished_links.contains(&account) && !self.logged_out(account))
    }

    /// Whether a history transfer of `a` runs. A logged-out account has none: its backend
    /// forgot the transfer with the login, and no event says so.
    fn transfer_runs(&self, a: &CoreAccount) -> bool {
        self.transfers.running(a.id) && !self.logged_out(a.id)
    }

    /// The name an account is told apart by: its profile name, else its network's.
    fn account_label(a: &CoreAccount) -> String {
        a.display_name
            .as_deref()
            .map(single_line)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| network_name(a.network).to_owned())
    }

    /// The history transfer of `a` as a banner: while it runs, or failed and not dismissed.
    /// Not while the link page of this very account is up: the page shows it large.
    /// After it (or without one), the profiles and groups a fresh link still fetches.
    fn transfer_of(&self, a: &CoreAccount) -> Option<Banner> {
        let view = self
            .transfers
            .view(a.id)
            .filter(|v| v.phase == crate::transfer::TransferPhase::Failed || self.transfer_runs(a))
            .or_else(|| {
                self.transfers
                    .updating(a.id)
                    .filter(|_| !self.logged_out(a.id))
            })
            .or_else(|| self.fill_view(a))?;
        if view.phase != crate::transfer::TransferPhase::Failed && self.linking(a.id) {
            return None;
        }
        Some(Banner {
            account: a.id,
            kind: match view.phase {
                crate::transfer::TransferPhase::Failed => BannerKind::TransferFailed,
                crate::transfer::TransferPhase::Updating => BannerKind::Updating,
                crate::transfer::TransferPhase::History => BannerKind::History,
                _ => BannerKind::Transfer,
            },
            text: view.status.clone(),
            action: String::new(),
            url: String::new(),
            transfer: Some(view),
            explanation: String::new(),
            appeal_url: String::new(),
            appeal_label: String::new(),
        })
    }

    /// The messenger of the login that [`Source::cancel_login`] would stop now: one that
    /// has not ended and whose account the core has not linked yet. Once the core took
    /// `AccountReady` the account is Active in its list and a cancel is dropped
    /// (`supervisor.rs` `Control::CancelLogin if !linked`), so nothing is lost by leaving
    /// the page: also in the tail of a link WITHOUT a transfer (the phone declined the
    /// history) and in the tail of "Link Again", where the account was logged out before.
    /// A history transfer reported by the backend says the same sooner than the account
    /// rows do (it comes only after the link).
    fn abandoned_network(&self) -> Option<&'static str> {
        let login = self.login.as_ref()?;
        if matches!(login.view.kind, LoginViewKind::Done | LoginViewKind::Failed) {
            return None;
        }
        if login.transferred || self.login_account_active(login) {
            return None;
        }
        Some(network_name(login.network))
    }

    /// Whether the core's account list holds the login's account as Active (it took
    /// `AccountReady`).
    /// The live state must not say logged out either: a row of a relinked account can be
    /// older than its logout (the answer to the accounts query follows the event).
    fn login_account_active(&self, login: &Login) -> bool {
        login.account.is_some_and(|id| {
            self.accounts.iter().any(|a| {
                a.id == id
                    && matches!(a.state, AccountState::Active)
                    && !matches!(self.conn_of(a), ConnState::LoggedOut { .. })
            })
        })
    }

    /// Whether the link page shows the login of `account` (a login that has not ended).
    fn linking(&self, account: AccountId) -> bool {
        self.login.as_ref().is_some_and(|l| {
            l.account == Some(account)
                && !matches!(l.view.kind, LoginViewKind::Done | LoginViewKind::Failed)
        })
    }

    /// The live connection state of an account (events are newer than the queried row).
    fn conn_of<'a>(&'a self, a: &'a CoreAccount) -> &'a ConnState {
        self.conn.get(&a.id).unwrap_or(&a.conn)
    }

    /// The instants (ms) at which an account's banner is written to change: the end of a ban
    /// ([`ConnState::Restricted`]), after which the banner offers "Reconnect".
    fn banner_instants(&self) -> impl Iterator<Item = i64> + '_ {
        self.visible_accounts().filter_map(|a| {
            let end = match self.conn_of(a) {
                ConnState::Restricted {
                    until_ms: Some(t), ..
                } => Some(*t),
                // A hold's banner and line name its end; at the end they change.
                _ => self.hold_of(a).and_then(|h| h.until_ms),
            };
            end.map(|t| i64::try_from(t).unwrap_or(i64::MAX))
        })
    }

    /// The end (ms) of the open chat's own mute, when it has a bounded one: "always" (the
    /// largest instant) never ends and needs no tick.
    fn open_mute_end(&self) -> Option<i64> {
        let row = self.row_of(self.open?)?;
        [row.muted_until, row.local_muted_until]
            .into_iter()
            .flatten()
            .filter(|t| *t != i64::MAX)
            .max()
    }

    /// Whether the account's network pushes its message history after a link (WhatsApp: the
    /// descriptor has no `history_transfer`), so a filling chat shows the quiet banner. A
    /// network that is not in the list does not.
    fn pushes_history(&self, account: AccountId) -> bool {
        self.accounts
            .iter()
            .find(|a| a.id == account)
            .is_some_and(|a| {
                self.networks
                    .iter()
                    .any(|n| n.kind == a.network && !n.history_transfer)
            })
    }

    /// A batch of a pushed history came for `account` (`filling` is already up to date): remember
    /// how far it is and when. `true` when what the banner shows changed (it appears, ends or
    /// reads another percentage).
    fn track_fill(&mut self, account: AccountId, progress: Option<f32>) -> bool {
        if !self.pushes_history(account) {
            return false;
        }
        let now = self.now_ms();
        let filling = self.filling.values().any(|a| *a == account);
        if !filling {
            return self.fills.remove(&account).is_some();
        }
        let fraction = progress.or(self.fills.get(&account).and_then(|f| f.fraction));
        let old = self.fills.insert(
            account,
            HistoryFill {
                fraction,
                last_batch: now,
            },
        );
        old.is_none_or(|o| o.fraction != fraction)
    }

    /// The pushed history of `account` stops being waited for (its connection went, it was
    /// logged out or removed, or no batch came for a long time): the banner and the hold on
    /// clearing and deleting its chats go. `true` when something was cleared.
    fn end_fills(&mut self, account: AccountId) -> bool {
        let banner = self.fills.remove(&account).is_some();
        let before = self.filling.len();
        self.filling.retain(|_, a| *a != account);
        banner || self.filling.len() != before
    }

    /// The quiet banner of an account whose pushed history is still coming.
    fn fill_view(&self, a: &CoreAccount) -> Option<crate::transfer::TransferView> {
        let fill = self.fills.get(&a.id)?;
        (a.state == AccountState::Active
            && !self.logged_out(a.id)
            && self.now_ms().saturating_sub(fill.last_batch) < HISTORY_FILL_IDLE_MS)
            .then(|| crate::transfer::history_fill_view(a.id, fill.fraction))
    }

    /// The account's send hold now.
    fn hold_of(&self, a: &CoreAccount) -> Option<AccountHold> {
        match self.holds.get(&a.id) {
            Some(h) => *h,
            None => a.hold,
        }
    }

    /// Whether `kind` is an experimental network that is in this build but turned off at this
    /// start: its accounts are paused (no backend, so nothing is sent or received; chats stay
    /// readable). The setting is the app's (`networks-on`); the front-end tells the choices.
    ///
    /// A network this build cannot log in to is not "off": turning it on would not help, and
    /// its accounts say what is missing instead ([`Self::network_missing`]).
    fn network_off(&self, kind: NetworkKind) -> bool {
        self.networks
            .iter()
            .any(|n| n.kind == kind && n.experimental && !n.on && n.missing.is_none())
    }

    /// Why this build cannot log in to `kind` (Telegram without its API key or library), when
    /// the front-end listed the network as missing. Its accounts are paused whether the person
    /// turned the network on or off, so they all show the banner of a missing part.
    fn network_missing(&self, kind: NetworkKind) -> Option<NetworkMissing> {
        self.networks
            .iter()
            .find(|n| n.kind == kind)
            .and_then(|n| n.missing)
    }

    /// Whether the accounts of `kind` have no backend in this run: the network is turned off
    /// or missing. Nothing is sent, received, held or unlinked for them.
    fn network_paused(&self, kind: NetworkKind) -> bool {
        self.network_off(kind) || self.network_missing(kind).is_some()
    }

    /// The quiet line of an account whose pushed history lost a part, while the account is
    /// linked (a logged-out account says so in its own banner, and a paused one is `NetworkOff`).
    fn history_lost_of(&self, a: &CoreAccount) -> Option<Banner> {
        (self.lost_history.contains(&a.id)
            && a.state == AccountState::Active
            && !self.logged_out(a.id))
        .then(|| history_lost_banner(a.id))
    }

    /// What ONE account shows of itself: its own state or its history transfer, whichever
    /// presses more (the order of `BannerKind`). An account never shows both. The text does not
    /// name the account yet ([`Self::named_banner`]).
    fn own_banner_bare(&self, a: &CoreAccount) -> Option<Banner> {
        let stopped = self.account_errors.contains(&a.id);
        let time = TimeContext::new(self.now_ms(), self.calendar.as_ref());
        let ctx = BannerContext {
            network: network_name(a.network),
            time: &time,
        };
        let state = account_banner(a.id, &a.state, self.conn_of(a), stopped, &ctx);
        // A network that holds back what is sent while the connection is up: the more
        // pressing of that and the state (a restriction stands above "offline" and below
        // what the person must fix). A paused account has no backend that could hold.
        let state = match self
            .hold_of(a)
            .filter(|_| a.state == AccountState::Active && !self.network_paused(a.network))
            .and_then(|h| hold_banner(a.id, &h, &ctx))
        {
            Some(h) if state.as_ref().is_none_or(|s| s.kind < h.kind) => Some(h),
            _ => state,
        };
        // A paused account (its network is turned off): "turned off" stands in for every state
        // that passes ("offline" is what the core shows an account without a backend), and
        // what the person must fix first (logged out, gave up) keeps its place without its
        // button, because linking or restarting an account needs the network on.
        // A network this build cannot log in to is paused too, on or off, and says what is
        // missing instead of "turned off" (turning it on would not help: no button).
        let state = if self.network_paused(a.network)
            && !matches!(a.state, AccountState::Linking | AccountState::Removing)
        {
            match state {
                Some(s) if s.kind > BannerKind::NetworkOff => Some(Banner {
                    action: String::new(),
                    ..s
                }),
                _ => Some(match self.network_missing(a.network) {
                    Some(missing) => network_missing_banner(a.id, network_name(a.network), missing),
                    None => network_off_banner(a.id, network_name(a.network)),
                }),
            }
        } else {
            state
        };
        Some(match (state, self.transfer_of(a)) {
            (Some(s), Some(t)) if t.kind > s.kind => t,
            (Some(s), _) => s,
            (None, Some(t)) => t,
            // Where the account shows nothing else: the quiet line of a history that lost a part.
            (None, None) => self.history_lost_of(a)?,
        })
    }

    /// With several accounts a banner says which one it is about.
    fn named_banner(mut banner: Banner, a: &CoreAccount, several: bool) -> Banner {
        if several {
            banner.text = trfc(
                "@info:status banner of an account, %1 the name of the account, %2 what is wrong",
                "%1: %2",
                &[&Self::account_label(a), &banner.text],
            );
            if let Some(t) = banner.transfer.as_mut() {
                t.status = banner.text.clone();
            }
        }
        banner
    }

    /// The banner of the accounts, of one sort: the states of accounts ([`BannerSort::State`]),
    /// or their history transfers. The two are chosen apart, so the state of one account never
    /// hides the transfer of another: a failed transfer stays until its user acts, and
    /// "Continue Without History" stays in reach, whatever another account is going through.
    /// Within one sort the most pressing kind wins (a failed transfer before a running one),
    /// the first account among equals.
    ///
    /// An account in `hidden` (its chats are not in the list) still speaks when it needs the
    /// user: its transfer shows whatever its state is (`transfer_of`, not what `own_banner`
    /// picked, which ranks Offline above a running transfer; the phases `History` and
    /// `Updating` are transfers too), and its state shows only for `Error`, `Restricted`,
    /// `Clock`, `Challenge` and `LoggedOut`, the kinds above `NetworkOff`: offline, connecting,
    /// syncing, turned off and a lost history pass or are the user's own choice.
    /// [`BannerSort::ShownTransfer`] is the transfer sort of the shown accounts alone (the
    /// placeholder of an empty list tells about those).
    fn banner_of(
        &self,
        account: Option<AccountId>,
        hidden: &Hidden,
        sort: BannerSort,
    ) -> Option<Banner> {
        let several = self.visible_accounts().count() > 1;
        self.visible_accounts()
            .filter(|a| account.is_none_or(|id| id == a.id))
            .filter_map(|a| {
                let is_hidden = matches!(hidden, Hidden::Ids(ids) if ids.contains(&a.id));
                let banner = match (sort, is_hidden) {
                    (BannerSort::ShownTransfer, true) => None,
                    (BannerSort::State, true) => self
                        .own_banner_bare(a)
                        .filter(|b| b.transfer.is_none() && b.kind > BannerKind::NetworkOff),
                    (BannerSort::Transfer, true) => self.transfer_of(a),
                    (BannerSort::State, false) => {
                        self.own_banner_bare(a).filter(|b| b.transfer.is_none())
                    }
                    (BannerSort::Transfer | BannerSort::ShownTransfer, false) => {
                        self.own_banner_bare(a).filter(|b| b.transfer.is_some())
                    }
                }?;
                Some(Self::named_banner(banner, a, several))
            })
            .fold(None, |best: Option<Banner>, b| match best {
                Some(have) if have.kind >= b.kind => Some(have),
                _ => Some(b),
            })
    }

    /// Whether `account` is linked (sends are refused otherwise).
    fn logged_out(&self, account: AccountId) -> bool {
        self.accounts.iter().any(|a| {
            a.id == account
                && (matches!(a.state, AccountState::LoggedOut { .. })
                    || matches!(self.conn_of(a), ConnState::LoggedOut { .. }))
        })
    }

    fn conversation_row(&self, c: &CoreConversation, time: &TimeContext<'_>) -> ConversationRow {
        let is_group = c.kind != ConvKind::Direct;
        let network = self.account_network(c.account);
        let (mut preview_author, mut preview_source, mut text, mut icon, mut receipt) = (
            String::new(),
            PreviewSource::None,
            String::new(),
            String::new(),
            Receipt::None,
        );
        if let Some(p) = &c.preview {
            let system = p.body_kind == "System";
            let own = p.author.is_none() && !system;
            if !system && !c.is_self {
                if own {
                    preview_author = trc(
                        "@item:intext author of the last message in the chat list, yourself",
                        "You",
                    )
                    .into_owned();
                    preview_source = PreviewSource::Own;
                } else if is_group {
                    let name = p
                        .author_name
                        .as_deref()
                        .map(|n| first_name(&single_line(n)).to_owned())
                        .or_else(|| {
                            p.author
                                .as_ref()
                                .map(|u| first_name(&self.names.display_name(u)).to_owned())
                        })
                        .unwrap_or_default();
                    if !name.is_empty() {
                        preview_author = name;
                        preview_source = PreviewSource::Peer;
                    }
                }
            }
            let summary = core_preview(p, &self.names);
            text = summary.text;
            icon = summary.icon;
            if own && p.deleted.is_none() {
                receipt = Receipt::from_state(p.out_state.as_ref());
            }
        }
        // The draft of a conversation the user left (this session's text, or the stored one).
        if self.open != Some(c.id)
            && let Some(draft) = self.drafts.get(&c.id).filter(|d| !d.trim().is_empty())
        {
            preview_author = trc(
                "@item:intext in the chat list, the text of an unsent message follows",
                "Draft",
            )
            .into_owned();
            preview_source = PreviewSource::Draft;
            receipt = Receipt::None;
            text = single_line(draft);
            icon = String::new();
        }
        let sent_at = c.preview.as_ref().map_or(0, |p| p.sent_at);
        let mut row = ConversationRow {
            id: c.id,
            account: c.account,
            network,
            kind: c.kind,
            is_self: c.is_self,
            title: conversation_title(c),
            avatar_url: self.avatar_url(c.account, c.avatar.as_ref()),
            preview_author,
            preview_source,
            preview: text,
            preview_icon: icon,
            typing: typing_label(&self.typers(c.id), is_group, &self.names),
            time_label: if sent_at > 0 {
                time.list_label(sent_at)
            } else {
                String::new()
            },
            last_activity: c.last_activity,
            unread: c.unread,
            mentions: c.mentions,
            marked_unread: c.marked_unread,
            muted: muted(c, time.now_ms),
            pinned_order: c.pinned_order,
            archived: archived_row(c),
            receipt,
            // (the chip, icon, number and panel place of the account are the table's, once per
            // call: `account_table`)
            account_badge: String::new(),
            badge_icon: String::new(),
            tint: String::new(),
            badge_number: 0,
            account_order: 0,
            protected: c.protected,
        };
        // (The core gives a protected chat no preview; the draft this session keeps is hidden
        // here.)
        if row.protected {
            row.hide_messages();
        }
        row
    }

    /// The account table of one list call: the visible accounts in panel order, with their
    /// chip name (the profile name, else the network's) and the call's hidden set.
    fn account_table(&self, hidden: &Hidden, searching: bool) -> AccountTable {
        AccountTable::new(
            self.visible_accounts().map(|a| TableAccount {
                id: a.id,
                network: a.network,
                name: Self::account_label(a),
                tint: self.tint_of(a),
            }),
            hidden,
            searching,
        )
    }

    /// The color of `a`: the one the person gave it, else its network's.
    fn tint_of(&self, a: &CoreAccount) -> String {
        self.own_tints
            .get(&a.uuid)
            .cloned()
            .unwrap_or_else(|| network_tint(a.network).to_owned())
    }

    fn header(&self, d: &ConversationDetail, time: &TimeContext<'_>) -> ChatHeader {
        let c = &d.row;
        let is_group = c.kind != ConvKind::Direct;
        let typing = typing_label(&self.typers(c.id), is_group, &self.names);
        let subtitle = if !typing.is_empty() {
            typing.clone()
        } else if c.is_self {
            trc(
                "@info:status subtitle of the chat with yourself",
                "Messages to yourself",
            )
            .into_owned()
        } else if is_group && self.detail_seeded && d.members.is_empty() {
            // Opening: the count comes with the chat's own answer.
            String::new()
        } else if is_group {
            crate::conversations::members_line(crate::conversations::full_member_count(
                d.members.iter().map(|m| m.role),
            ) as u64)
        } else {
            network_name(self.account_network(c.account)).to_owned()
        };
        // A logged-out account cannot send: say so instead of failing every message. (In
        // other words than the banner above the list, which names the state and offers
        // "Link Again": side by side in a wide window the same sentence would show twice.)
        let cannot_send = if self.logged_out(c.account) {
            trc(
                "@info:status",
                "You can read this chat. To write here, link this device again.",
            )
            .into_owned()
        } else if self
            .network_missing(self.account_network(c.account))
            .is_some()
        {
            // The build cannot log in to the network: nothing the person can switch on.
            trfc(
                "@info:status why the composer is off, %1 the name of the network",
                "You can read this chat. This build of Kourier cannot connect to %1.",
                &[network_name(self.account_network(c.account))],
            )
        } else if self.network_off(self.account_network(c.account)) {
            // A paused account: the core would refuse the send (no backend for the network).
            trfc(
                "@info:status why the composer is off, %1 the name of the network",
                "You can read this chat. %1 is turned off: turn it on to write here.",
                &[network_name(self.account_network(c.account))],
            )
        } else {
            d.features
                .cannot_send
                .as_ref()
                .map(timeline::cannot_send_text)
                .unwrap_or_default()
        };
        ChatHeader {
            id: c.id,
            account: c.account,
            title: conversation_title(c),
            subtitle,
            typing: !typing.is_empty(),
            avatar_url: self.avatar_url(c.account, c.avatar.as_ref()),
            tint: self
                .accounts
                .iter()
                .find(|a| a.id == c.account)
                .map_or_else(
                    || network_tint(self.account_network(c.account)).to_owned(),
                    |a| self.tint_of(a),
                ),
            is_group,
            is_self: c.is_self,
            peer: c.peer.clone(),
            has_details: crate::conversations::kind_has_details(c.kind),
            muted: muted(c, time.now_ms),
            cannot_send: cannot_send.clone(),
            attach: crate::media::AttachRules::new(&d.features.files),
            timer_secs: timer_secs(d.timer),
            timer: d.timer.map(preview::timer_label).unwrap_or_default(),
            timers: if cannot_send.is_empty() && !c.blocked {
                timer_menu(&d.features, timer_secs(d.timer))
            } else {
                Vec::new()
            },
            timer_range: if cannot_send.is_empty() && !c.blocked {
                timer_span(&d.features)
            } else {
                None
            },
            // The core tells the other devices only where the network has such a message
            // and the account is linked.
            deletes_sync: d.features.delete_for_me && !self.logged_out(c.account),
            still_filling: self.still_filling(c.id, c.account),
            timer_held: self.timer_held(c.account, time.now_ms),
            offer: self.offer_for(c, time),
        }
    }

    /// Why the disappearing-message timer cannot be changed now; empty when it can. While the
    /// account's network holds back what is sent ([`AccountHold`]) the core refuses a timer
    /// change (`Refusal::Held`: on some networks it is a message of its own, one more reach-out),
    /// so the menu says so instead of failing the click (K-A fix C3).
    fn timer_held(&self, account: AccountId, now_ms: i64) -> String {
        let hold = self
            .accounts
            .iter()
            .find(|a| a.id == account)
            .and_then(|a| self.hold_of(a));
        let Some(hold) = hold else {
            return String::new();
        };
        if hold
            .until_ms
            .is_some_and(|t| i64::try_from(t).unwrap_or(i64::MAX) <= now_ms)
        {
            return String::new();
        }
        trfc(
            "@info:status why the disappearing-messages entry of the chat is off, %1 the name of the network",
            "Disappearing messages cannot be changed while %1 restricts this account.",
            &[network_name(self.account_network(account))],
        )
    }

    fn timeline_rows(&self, time: &TimeContext<'_>) -> Vec<MessageRow> {
        let is_group = self
            .detail
            .as_ref()
            .is_some_and(|d| d.row.kind != ConvKind::Direct);
        let rows = &self.window.rows;
        let by_id: HashMap<MessageId, &CoreMessage> = rows.iter().map(|r| (r.id, r)).collect();
        let names = &self.names;
        let (hold, safety_number, who) = self.wait_context();
        let items = rows.iter().map(|r| {
            let wait = self.send_wait(r, hold, safety_number);
            Item {
                id: r.id,
                sent_at: r.sent_at,
                own: r.origin != OriginKind::Remote,
                author: r.author.as_ref(),
                body: &r.body,
                deleted: r.deleted.as_ref(),
                edited: r.revision > 0 || r.edited_at.is_some(),
                out_state: r.out_state.as_ref(),
                quote: if r.deleted.is_none() {
                    quote_block(r, &by_id, names)
                } else {
                    None
                },
                reactions: reaction_chips(r, names),
                actions: row_actions(&r.actions),
                ask: &r.actions.ask_attachments,
                save: &r.actions.save_attachments,
                people: r
                    .actions
                    .show_people
                    .iter()
                    .map(|p| (p.user.clone(), p.own))
                    .collect(),
                safety: r.actions.safety_number.as_ref(),
                waits_for: who.filter(|_| {
                    wait.is_some_and(|w| w.reason == timeline::WaitReason::SafetyNumber)
                }),
                expiry: r.expiry.as_ref(),
                wait,
            }
        });
        let mut rows = timeline::build_rows(
            items,
            is_group,
            self.unread_from,
            self.window.has_older,
            names,
            time,
            &self.read_more,
        );
        timeline::apply_progress(&mut rows, &self.downloads);
        let waiting: Vec<&CoreMessage> = self
            .window
            .rows
            .iter()
            .filter(|r| r.outbox.is_some())
            .collect();
        if !waiting.is_empty() {
            for row in &mut rows {
                if let Some(core) = row
                    .message_id()
                    .and_then(|m| waiting.iter().find(|r| r.id == m))
                {
                    self.give_rings(core, row);
                }
            }
        }
        self.author_avatars(&mut rows);
        rows
    }

    /// The bytes reported for `attachment`, while its message's attempt uploads.
    fn upload_bytes(&self, attachment: AttachmentId) -> Option<(u64, u64)> {
        self.uploads.get(&attachment).map(|u| (u.done, u.total))
    }

    /// The files of `message` that a backend reported bytes for: `(done, total)` summed over
    /// them, and whether one of them is not up yet. `None` when nothing was reported.
    fn upload_sum(&self, message: MessageId) -> Option<(u64, u64, bool)> {
        let mut found: Option<(u64, u64, bool)> = None;
        for u in self.uploads.values().filter(|u| u.message == message) {
            let (done, total, open) = found.get_or_insert((0, 0, false));
            *done += u.done;
            *total += u.total;
            *open |= u.done < u.total;
        }
        found
    }

    /// Puts the rings of the files of the own message `core` (a row with a send row that waits)
    /// on its built `row`. A file nothing is reported for has an EMPTY ring while its message
    /// waits for its turn (queued, staged: offline, held, a challenge, a retry wait included)
    /// and while the backend says it prepares them (`SendPhase::Preparing`); the X is the
    /// row's `cancel_sending`.
    fn give_rings(&self, core: &CoreMessage, row: &mut MessageRow) {
        let in_flight = core.outbox.as_ref().is_some_and(|o| o.in_flight);
        // The backend SAYS it prepares the files (`SendPhase::Preparing`, upload progress fix
        // U3): an empty ring and the X on every file at once. A backend that reports no file
        // and no `Preparing` keeps the line "Uploading files" and no ring.
        let preparing = self.send_phases.get(&core.id) == Some(&SendPhase::Preparing);
        let bytes = |a: AttachmentId| self.upload_bytes(a);
        timeline::apply_uploads(row, &bytes, !in_flight || preparing);
    }

    /// What holds every own message of the open chat that is on its way: the account (not
    /// connected, a challenge, a wrong clock, logged out …, [`Self::account_hold`]) and
    /// whether its queue waits for a changed safety number to be confirmed, and for whose (the
    /// core names the person on the paused head, `MessageActions::safety_number`, BQ-3; `None`
    /// while that head is not loaded).
    fn wait_context(&self) -> (Option<timeline::WaitReason>, bool, Option<&UserId>) {
        let hold = self
            .detail
            .as_ref()
            .and_then(|d| self.account_hold(d.row.account));
        let who = self
            .window
            .rows
            .iter()
            .find_map(|r| r.actions.safety_number.as_ref());
        let safety_number = who.is_some()
            || self.window.rows.iter().any(|r| {
                r.outbox.is_some()
                    && matches!(
                        r.out_state,
                        Some(OutgoingState::Failed {
                            reason: FailReason::UntrustedIdentity,
                            ..
                        })
                    )
            });
        (hold, safety_number, who)
    }

    /// What the line under an own message on its way is built from (link feedback L6):
    /// `None` for every other row. `hold`: what keeps the chat's account from sending
    /// ([`Self::account_hold`]); `safety_number`: the chat's queue waits for the user to
    /// confirm a changed safety number (its head shows as failed, the rows behind it wait).
    fn send_wait(
        &self,
        r: &CoreMessage,
        hold: Option<timeline::WaitReason>,
        safety_number: bool,
    ) -> Option<timeline::SendWait> {
        let outbox = r.outbox.as_ref()?;
        let reason = if let Some(hold) = hold {
            hold
        } else if safety_number && !outbox.in_flight {
            timeline::WaitReason::SafetyNumber
        } else if let Some((done, total, true)) = self.upload_sum(r.id).filter(|_| outbox.in_flight)
        {
            // The files' own bytes say more than the step: "Uploading 45% · 12 of 30 MB".
            timeline::WaitReason::Uploading { done, total }
        } else if let Some(phase) = self.send_phases.get(&r.id).filter(|_| outbox.in_flight) {
            timeline::WaitReason::Phase(*phase)
        } else {
            timeline::WaitReason::Sending
        };
        Some(timeline::SendWait {
            reason,
            since: r.sent_at,
            retry_at: outbox.next_attempt_at.filter(|_| !outbox.in_flight),
        })
    }

    /// What keeps the account from sending its queue now, in the terms of its banner; `None`
    /// while the connection is up (the outbox drains, link feedback L6).
    fn account_hold(&self, account: AccountId) -> Option<timeline::WaitReason> {
        use timeline::WaitReason as W;
        let row = self.accounts.iter().find(|a| a.id == account);
        if row.is_some_and(|a| matches!(a.state, AccountState::LoggedOut { .. })) {
            return Some(W::LoggedOut);
        }
        let Some(conn) = self.conn.get(&account).or(row.map(|a| &a.conn)) else {
            // An account nothing is known about is not connected.
            return Some(W::Connection);
        };
        let network = row.map_or("", |a| network_name(a.network));
        match conn {
            // The connection is up; the network may still hold what the user sends.
            ConnState::Syncing { .. } | ConnState::Online { .. } => {
                let hold = row.and_then(|a| self.hold_of(a))?;
                let until_ms = hold.until_ms.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
                // The core lifts a hold at its end; a line that outlives it names none.
                (until_ms.is_none_or(|t| t > self.now_ms()))
                    .then_some(W::Restricted { network, until_ms })
            }
            ConnState::Offline { .. } | ConnState::Connecting => Some(W::Connection),
            ConnState::NeedsUserAction(UserAction::SolveChallenge { .. }) => Some(W::Challenge),
            ConnState::NeedsUserAction(UserAction::FixClock { .. }) => Some(W::Clock),
            ConnState::NeedsUserAction(UserAction::Relink) | ConnState::LoggedOut { .. } => {
                Some(W::LoggedOut)
            }
            ConnState::NeedsUserAction(UserAction::UpdateApp) => Some(W::UpdateApp),
            ConnState::NeedsUserAction(UserAction::Reconnect) => Some(W::Reconnect),
            // A ban that has ended waits for the person (the banner offers "Reconnect").
            ConnState::Restricted { until_ms, .. } => {
                let until_ms = until_ms.map(|t| i64::try_from(t).unwrap_or(i64::MAX));
                Some(if until_ms.is_some_and(|t| t <= self.now_ms()) {
                    W::Reconnect
                } else {
                    W::Restricted { network, until_ms }
                })
            }
        }
    }

    /// The open timeline holds an own message on its way (the line under it counts time).
    fn sends_waiting(&self) -> bool {
        self.open.is_some() && self.window.rows.iter().any(|r| r.outbox.is_some())
    }

    /// Live step of a send of the open timeline (`Progress::Transmit`). A step changes the
    /// reason under that one message only (a group send reports up to four a second): the
    /// lines are brought up to date like the tick does ([`Changes::waits`]), the timeline is
    /// not rebuilt.
    fn on_send_phase(&mut self, message: MessageId, phase: Option<SendPhase>) -> Changes {
        let Some(row) = self.window.rows.iter().find(|r| r.id == message) else {
            return Changes::default();
        };
        let shown = self.now_ms().saturating_sub(row.sent_at) >= timeline::WAIT_SHOWN_AFTER_MS;
        let old = match phase {
            Some(p) => self.send_phases.insert(message, p),
            None => self.send_phases.remove(&message),
        };
        // The empty rings of a message whose files are being made ready come with the phase and
        // go with it; a message with figures per file shows its own (a report moves one ring).
        let preparing = |p: Option<SendPhase>| p == Some(SendPhase::Preparing);
        let rings = preparing(old) != preparing(phase)
            && !self.uploads.values().any(|u| u.message == message);
        Changes {
            waits: shown && old != phase,
            uploads: rings,
            ..Changes::default()
        }
    }

    /// Live bytes of one file of an own message of the open timeline whose attempt uploads
    /// (`Progress::Upload`). The rings move per whole percent of a file and the line under the
    /// message per whole percent of the message, on the per-row paths ([`Changes::uploads`],
    /// [`Changes::waits`]): the timeline is never rebuilt for a byte count.
    fn on_upload_progress(
        &mut self,
        message: MessageId,
        attachment: AttachmentId,
        done: u64,
        total: u64,
    ) -> Changes {
        if self.open.is_none() || total == 0 {
            return Changes::default();
        }
        let Some(row) = self.window.rows.iter().find(|r| r.id == message) else {
            return Changes::default();
        };
        let ours = row.outbox.is_some()
            && attachments_of(&row.body)
                .iter()
                .any(|a| a.id == Some(attachment));
        if !ours {
            return Changes::default();
        }
        let shown = self.now_ms().saturating_sub(row.sent_at) >= timeline::WAIT_SHOWN_AFTER_MS;
        // What the line says: "Preparing files" until a byte is up, then the whole percent.
        let line = |sum: Option<(u64, u64, bool)>| {
            sum.map(|(d, t, open)| (d == 0, open, timeline::upload_percent(d, t)))
        };
        let before = line(self.upload_sum(message));
        let old = self.uploads.insert(
            attachment,
            UploadBytes {
                message,
                done,
                total,
            },
        );
        let after = line(self.upload_sum(message));
        let percent = |d: u64, t: u64| timeline::upload_percent(d, t);
        Changes {
            // A first report turns the empty ring into a ring at its percent.
            uploads: old.is_none_or(|o| percent(o.done, o.total) != percent(done, total)),
            waits: shown && before != after,
            ..Changes::default()
        }
    }

    /// The uploads of `message` are over when its share is full or its step is another one:
    /// the bytes are let go, so that no ring stays at 100 % through a group's key setup.
    /// The X came after the message went out (`UiEvent::CancelTooLate`, sent once the attempt's
    /// outcome is recorded as sent): a note in the chat's composer, whose Delete for Everyone
    /// works at once (the row is sent now). Only in the chat that is open.
    fn on_cancel_too_late(&mut self, conv: ConversationId, message: MessageId) -> Changes {
        if self.open != Some(conv) {
            return Changes::default();
        }
        let text = trc(
            "@info:status the X came after the message went out",
            "Already sent",
        )
        .into_owned();
        self.notice.clone_from(&text);
        self.too_late = Some((message, text));
        Changes {
            notice: true,
            ..Changes::default()
        }
    }

    fn on_uploads_over(
        &mut self,
        message: MessageId,
        progress: f32,
        phase: Option<SendPhase>,
    ) -> Changes {
        let over = progress >= 1.0
            || phase.is_some_and(|p| !matches!(p, SendPhase::Uploading | SendPhase::Preparing));
        if !over || !self.uploads.values().any(|u| u.message == message) {
            return Changes::default();
        }
        let open = self.upload_sum(message).is_some_and(|(_, _, open)| open);
        self.uploads.retain(|_, u| u.message != message);
        let shown = self
            .window
            .rows
            .iter()
            .find(|r| r.id == message)
            .is_some_and(|r| {
                self.now_ms().saturating_sub(r.sent_at) >= timeline::WAIT_SHOWN_AFTER_MS
            });
        Changes {
            uploads: true,
            waits: shown && open,
            ..Changes::default()
        }
    }

    /// Authors' avatars (groups): the author's own picture, else the picture of their direct
    /// chat on the same account.
    fn author_avatars(&self, rows: &mut [MessageRow]) {
        let Some(account) = self.detail.as_ref().map(|d| d.row.account) else {
            return;
        };
        if self.avatars.is_none() || !rows.iter().any(|r| r.show_avatar) {
            return;
        }
        let own: HashMap<MessageId, &kourier_model::BlobRef> = self
            .window
            .rows
            .iter()
            .filter_map(|r| r.author_avatar.as_ref().map(|a| (r.id, a)))
            .collect();
        let by_user: HashMap<&UserId, &CoreConversation> = self
            .list
            .iter()
            .filter(|c| c.account == account && c.kind == ConvKind::Direct && c.avatar.is_some())
            .filter_map(|c| c.peer.as_ref().map(|p| (p, c)))
            .collect();
        for row in rows.iter_mut().filter(|r| r.show_avatar) {
            let picture = row
                .message_id()
                .and_then(|id| own.get(&id).copied())
                .or_else(|| {
                    row.author_id
                        .as_ref()
                        .and_then(|u| by_user.get(u))
                        .and_then(|c| c.avatar.as_ref())
                });
            if picture.is_some() {
                row.avatar_url = self.avatar_url(account, picture);
            }
        }
    }
}

/// The core's flags as the row shows them (`retry`: the core accepts "Send Again"; the row
/// offers it only for a message that shows as failed, see `timeline::rows`).
fn row_actions(a: &MessageActions) -> RowActions {
    RowActions {
        reply: a.reply,
        react: a.react,
        edit: a.edit,
        copy: a.copy,
        delete_for_me: a.delete_for_me,
        delete_for_everyone: a.delete_for_everyone,
        delete_attachment: a.delete_attachment,
        take_back: a.take_back,
        cancel_sending: a.cancel_sending,
        retry: a.retry,
        ask_attachments: !a.ask_attachments.is_empty(),
        save_attachments: !a.save_attachments.is_empty(),
        show_sender: a.show_sender,
        show_people: !a.show_people.is_empty(),
    }
}

/// The attachments of a message body.
fn attachments_of(body: &Body) -> Vec<&kourier_model::Attachment> {
    match body {
        Body::Media { attachments, .. } => attachments.iter().collect(),
        Body::Sticker(a) => vec![a.as_ref()],
        _ => Vec::new(),
    }
}

/// The whole list in one read, the inbox and the archive together (`archived: None`): search,
/// the quick switch and the archive's count work from one snapshot, and a chat that moves
/// between the two is in exactly one of them. The rows split by their `archived` flag.
fn list_query(after: Option<ConversationCursor>) -> Query {
    Query::Conversations(ConversationListQuery {
        archived: None,
        limit: CONVERSATION_PAGE,
        after,
        ..ConversationListQuery::default()
    })
}

/// Whether the core's row is in the archive (the one rule: [`shown_archived`]).
fn archived_row(c: &CoreConversation) -> bool {
    shown_archived(c.archived, c.pinned_order)
}

/// Whether the open chat changed sides between two reads of the list. Only a chat that both
/// reads list can have moved: one that was not listed before (the first read, a chat just
/// made) or is not any more (deleted, its account removed) has no side to leave.
fn archive_move(
    open: Option<ConversationId>,
    old: &[CoreConversation],
    new: &[CoreConversation],
) -> Option<ArchiveMove> {
    let open = open?;
    let side = |rows: &[CoreConversation]| rows.iter().find(|c| c.id == open).map(archived_row);
    match (side(old)?, side(new)?) {
        (true, false) => Some(ArchiveMove::ToInbox),
        (false, true) => Some(ArchiveMove::ToArchive),
        _ => None,
    }
}

fn muted(c: &CoreConversation, now_ms: i64) -> bool {
    [c.muted_until, c.local_muted_until]
        .into_iter()
        .flatten()
        .any(|until| until > now_ms)
}

pub(crate) fn conversation_title(c: &CoreConversation) -> String {
    if c.is_self {
        return trc(
            "@item:intext title of the chat with yourself",
            "Note to Self",
        )
        .into_owned();
    }
    match c
        .title
        .as_deref()
        .map(single_line)
        .filter(|t| !t.is_empty())
    {
        Some(t) => t,
        // No name known yet (profiles arrive in S9): the number or "Signal user ⋯1f2e".
        None if c.kind == ConvKind::Direct => c.peer.as_ref().map_or_else(
            || trc("@item:intext a person without a name", "Unknown").into_owned(),
            fallback_name,
        ),
        None => trc(
            "@item:intext title of a group without a name",
            "Unnamed group",
        )
        .into_owned(),
    }
}

/// The list preview of the newest message, from the core's preview columns: its text or
/// caption, else what it is (a voice message, three photos, who was removed from the group,
/// view-once media).
fn core_preview(p: &kourier_core::facade::Preview, names: &dyn Names) -> Summary {
    // The timeline's words: a delete that names nobody (KT-2) is not "You deleted this message".
    if let Some(deletion) = &p.deleted {
        return Summary {
            text: preview::deleted_text(deletion, p.author.is_none(), p.author.as_ref()),
            icon: "trash-empty".into(),
        };
    }
    // Mention placeholders (U+FFFC) carry no name in the preview column.
    let text = p
        .text
        .as_deref()
        .map(|t| single_line(&t.replace('\u{fffc}', "@")))
        .unwrap_or_default();
    let attachment = || {
        let kind = p.attachment.unwrap_or(AttachmentKind::File);
        let label = preview::attachment_label(kind, p.attachments.max(1) as usize);
        // A lone document or sound file is called by its name: "Invoice.pdf" says more than
        // "File". (Pictures, videos and voice notes are what they are, whatever their name.)
        let name = p
            .file_name
            .as_deref()
            .map(single_line)
            .filter(|n| !n.is_empty())
            .filter(|_| {
                p.attachments <= 1 && matches!(kind, AttachmentKind::File | AttachmentKind::Audio)
            });
        match name {
            Some(name) => Summary {
                text: clip(&name),
                icon: label.icon,
            },
            None => label,
        }
    };
    let what = match p.body_kind.as_str() {
        "Text" => Summary::default(),
        "Media" => attachment(),
        "Sticker" => Summary {
            text: trc("@info:preview kind of message", "Sticker").into_owned(),
            icon: "smiley".into(),
        },
        "Contact" => Summary {
            text: trc("@info:preview kind of message", "Contact").into_owned(),
            icon: "addressbook-details".into(),
        },
        "Location" => Summary {
            text: trc("@info:preview kind of message", "Location").into_owned(),
            icon: "mark-location".into(),
        },
        "Poll" => Summary {
            text: trc("@info:preview kind of message", "Poll").into_owned(),
            icon: "view-statistics".into(),
        },
        // What happened, in words: "Ari removed Bo", "You set the timer to 1 day".
        "System" => match &p.system {
            Some(event) => {
                return Summary {
                    text: clip(&preview::system_text(event, names)),
                    icon: preview::system_icon(event).to_owned(),
                };
            }
            None => Summary {
                text: trc(
                    "@info:preview stand-in for a message",
                    "Conversation updated",
                )
                .into_owned(),
                icon: "dialog-information".into(),
            },
        },
        "Placeholder" => Summary {
            text: trc(
                "@info:preview stand-in for a message",
                "Message not available",
            )
            .into_owned(),
            icon: "help-about".into(),
        },
        _ => preview::unsupported_label(p.kind_hint.as_deref().unwrap_or_default()),
    };
    if text.is_empty() {
        return what;
    }
    // A caption says more than "Photo"; the icon still tells what it captions.
    Summary {
        text: clip(&text),
        icon: what.icon,
    }
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(preview::PREVIEW_MAX_CHARS) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_owned(),
    }
}

/// The oldest incoming unread message such that every newer incoming message is unread too.
/// Of the notices only a call line can be unread (a missed call, a group call that was
/// started): the core gives it a read state exactly while it counts.
fn first_unread(rows: &[CoreMessage]) -> Option<MessageId> {
    rows.iter()
        .rev()
        .take_while(|m| {
            m.origin != OriginKind::Remote || m.read_state.is_none_or(|r| r == ReadState::Unread)
        })
        .filter(|m| {
            m.origin == OriginKind::Remote
                && (!matches!(m.body, kourier_model::Body::System(_))
                    || m.body.call_line().is_some())
                && m.read_state == Some(ReadState::Unread)
        })
        .last()
        .map(|m| m.id)
}

fn reaction_chips(r: &CoreMessage, names: &dyn Names) -> Vec<ReactionChip> {
    let mut chips: Vec<ReactionChip> = r
        .reactions
        .iter()
        .filter(|g| !g.users.is_empty())
        .map(|g| ReactionChip {
            emoji: g.emoji.clone(),
            count: u32::try_from(g.users.len()).unwrap_or(u32::MAX),
            mine: g.by_me(),
            names: g
                .users
                .iter()
                .map(|u| timeline::reactor_name(u.as_ref(), names))
                .collect(),
            own_at: g.users.iter().position(Option::is_none),
        })
        .collect();
    chips.sort_by_key(|c| std::cmp::Reverse(c.count));
    chips
}

/// The quote block of a reply, from what the core resolved (`ReplyPreview`): who was quoted,
/// the quoted text or what was quoted instead, a thumbnail for pictures, and whether the
/// original is here to jump to, missing or deleted.
fn quote_block(
    r: &CoreMessage,
    window: &HashMap<MessageId, &CoreMessage>,
    names: &dyn Names,
) -> Option<Block> {
    let reply = r.reply.as_ref()?;
    let own = reply.own;
    let author = if own {
        trc(
            "@item:intext the author of a quoted message, yourself",
            "You",
        )
        .into_owned()
    } else {
        match (&reply.author_name, &reply.author) {
            (Some(name), _) if !name.expose().trim().is_empty() => single_line(name.expose()),
            (_, Some(user)) => names.display_name(user),
            _ => trc("@item:intext a person without a name", "Unknown").into_owned(),
        }
    };
    let state = match reply.state {
        CoreQuoteState::Known => QuoteState::Known,
        CoreQuoteState::Missing => QuoteState::Missing,
        CoreQuoteState::Deleted => QuoteState::Deleted,
    };
    let text = reply
        .text
        .as_ref()
        .map(|t| preview::clip_line(&preview::text_line(t, names)))
        .filter(|t| !t.is_empty());
    let kind = reply.attachment.map(|k| preview::attachment_label(k, 1));
    let file = reply
        .file_name
        .as_ref()
        .map(|n| single_line(n.expose()))
        .filter(|n| !n.is_empty());
    let (text, icon, stand_in) = match (text, kind, state) {
        // A deleted original shows nothing of what it was, whatever the quote carried.
        (_, _, QuoteState::Deleted) => {
            let s = preview::deleted_quote();
            (s.text, s.icon, true)
        }
        (Some(text), kind, _) => (text, kind.map(|k| k.icon).unwrap_or_default(), false),
        (None, Some(kind), _) => match (reply.attachment, file) {
            (Some(AttachmentKind::File | AttachmentKind::Audio), Some(name)) => {
                (preview::clip_line(&name), kind.icon, false)
            }
            _ => (kind.text, kind.icon, false),
        },
        (None, None, _) => {
            let s = preview::missing_quote();
            (s.text, s.icon, true)
        }
    };
    // The picture of the original, through the image provider like any thumbnail of the
    // timeline; its revision follows the attachment when the original is loaded.
    let thumb_url = match (state, reply.thumb) {
        (QuoteState::Known, Some(id)) => {
            let rev = reply
                .target
                .and_then(|t| window.get(&t))
                .and_then(|t| {
                    attachments_of(&t.body)
                        .into_iter()
                        .find(|a| a.id == Some(id))
                        .map(media::revision)
                })
                .unwrap_or(0);
            media::media_url(MediaVariant::Thumb, id, rev)
        }
        _ => String::new(),
    };
    Some(Block::Quote(QuoteBlock {
        author,
        text,
        icon,
        own,
        // A tombstone can still be jumped to; a missing or hidden original cannot.
        target: reply.target,
        state,
        thumb_url,
        stand_in,
    }))
}

/// What kind of failure an `ActionFailed` reports: all a notice is chosen by, together with
/// what the rows say. The text of an error is for the log, not for decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// The core did not take the request: it does not fit what is here (any more).
    Refused,
    /// This build cannot do it.
    Unavailable,
    /// The network answered no, or gave no answer in time.
    Network(NetworkFailure),
    /// Storage, the vault, or a bug.
    Internal,
}

/// How the network failed, as far as a notice tells it apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NetworkFailure {
    /// It asks to slow down.
    RateLimited,
    /// Anything else.
    Other,
}

/// What a `SaveFacts` query in flight is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SaveAsk {
    /// The name for the dialog of one attachment.
    Suggest(AttachmentId),
    /// The name of one file of a batch.
    Name {
        /// The batch.
        batch: u64,
        /// The attachment.
        attachment: AttachmentId,
    },
}

/// One file of a batch whose `SaveAttachment` waits for the core.
struct SaveRun {
    batch: u64,
    dir: std::path::PathBuf,
    /// The name the file is offered under.
    base: String,
    /// The name asked for now (`base`, then `base (1)` …).
    name: String,
    /// How many names were tried after the first.
    tries: u32,
    /// Whether a clash is answered with the next name (a batch) or replaced (the dialog asked).
    keep_both: bool,
}

/// The files of one save call: ONE answer when the last has ended.
struct SaveBatch {
    left: u32,
    saved: u32,
    failed: u32,
    name: String,
    dir: std::path::PathBuf,
    failure: Option<SaveFailure>,
}

/// The most names tried for one file of a batch before it counts as failed.
const SAVE_TRIES: u32 = 99;

/// What the core's refusal of a save is called in the notice.
fn save_failure(error: &CoreError) -> SaveFailure {
    use kourier_core::facade::SaveError;
    match error {
        CoreError::Save(SaveError::NoSpace) => SaveFailure::NoSpace,
        CoreError::Save(SaveError::NotWritable) => SaveFailure::NotWritable,
        CoreError::Save(SaveError::NoFolder) => SaveFailure::NoFolder,
        CoreError::Refused { .. } => SaveFailure::Gone,
        CoreError::InvalidInput(_) => SaveFailure::NotHere,
        _ => SaveFailure::Other,
    }
}

impl CoreSource {
    /// Asks the core for the facts the name of `attachment` is made of.
    fn ask_save_facts(&mut self, what: SaveAsk) -> bool {
        let id = match &what {
            SaveAsk::Suggest(a) | SaveAsk::Name { attachment: a, .. } => *a,
        };
        let generation = self.save_next;
        self.save_next += 1;
        self.save_asks.insert(generation, what);
        self.send(UiCommand::Query {
            token: QueryToken {
                model: MODEL_SAVE,
                generation,
            },
            query: Query::SaveFacts(id),
        });
        !self.stopped
    }

    /// The name a file is offered under, from the facts the core gave.
    fn save_name(&self, facts: &kourier_core::facade::SaveFacts) -> String {
        let sender = match &facts.author {
            Some(user) => self.names.display_name(user),
            None => self
                .accounts
                .iter()
                .find(|a| a.id == facts.account)
                .and_then(|a| a.display_name.clone())
                .unwrap_or_default(),
        };
        crate::save_name::suggested_name(
            &crate::save_name::NameFacts {
                file_name: facts.file_name.as_deref(),
                sender: &sender,
                sent_at_ms: facts.sent_at,
                position: usize::try_from(facts.position).unwrap_or(1),
                mime: facts.mime.as_deref(),
                sniffed: facts.sniffed.as_deref(),
            },
            self.calendar.as_ref(),
        )
    }

    /// Starts the batch `batch`'s file `attachment` under `name`. The core's answers (a saved
    /// file, a refusal) name the attachment and nothing else, so only ONE run per attachment
    /// is in flight: when an earlier save of it has not been answered (Save All of an album and
    /// the viewer's Ctrl+S on one of its pictures), the new run waits for that answer and
    /// nothing is sent yet.
    fn start_save(&mut self, batch: u64, attachment: AttachmentId, name: String, keep_both: bool) {
        let Some(dir) = self.save_batches.get(&batch).map(|b| b.dir.clone()) else {
            return;
        };
        let run = SaveRun {
            batch,
            dir,
            base: name.clone(),
            name,
            tries: 0,
            keep_both,
        };
        if self.save_runs.contains_key(&attachment) {
            self.save_waiting
                .entry(attachment)
                .or_default()
                .push_back(run);
            return;
        }
        self.send_save_run(attachment, run);
    }

    /// Makes `run` the run in flight of `attachment` and sends its first `SaveAttachment`.
    fn send_save_run(&mut self, attachment: AttachmentId, run: SaveRun) {
        let target = kourier_core::facade::SaveTarget {
            dir: run.dir.clone(),
            name: run.name.clone(),
        };
        let mode = if run.keep_both {
            kourier_core::facade::SaveMode::NoReplace
        } else {
            kourier_core::facade::SaveMode::Replace
        };
        self.save_runs.insert(attachment, run);
        self.send(UiCommand::SaveAttachment {
            attachment,
            target,
            mode,
        });
    }

    /// The run of `attachment` in flight ended: the next one asked for it, if any, starts now.
    fn start_next_save(&mut self, attachment: AttachmentId) {
        let next = self
            .save_waiting
            .get_mut(&attachment)
            .and_then(VecDeque::pop_front);
        if self
            .save_waiting
            .get(&attachment)
            .is_some_and(VecDeque::is_empty)
        {
            self.save_waiting.remove(&attachment);
        }
        if let Some(run) = next {
            self.send_save_run(attachment, run);
        }
    }

    /// One file of a batch ended; the batch's answer is made with its last file.
    fn save_file_ended(&mut self, batch: u64, saved: Option<&str>, failure: Option<SaveFailure>) {
        let Some(b) = self.save_batches.get_mut(&batch) else {
            return;
        };
        b.left = b.left.saturating_sub(1);
        if let Some(name) = saved {
            b.saved += 1;
            b.name = name.to_owned();
        } else {
            b.failed += 1;
            b.failure = failure.or(b.failure);
        }
        if b.left > 0 {
            return;
        }
        if let Some(b) = self.save_batches.remove(&batch) {
            self.save_answers.push(SaveAnswer::Done(SaveOutcome {
                saved: b.saved,
                failed: b.failed,
                name: b.name,
                dir: b.dir,
                failure: b.failure,
            }));
        }
    }

    /// A new batch of `files` files into `dir`.
    fn new_save_batch(&mut self, files: u32, dir: std::path::PathBuf) -> u64 {
        let batch = self.save_batch_next;
        self.save_batch_next += 1;
        self.save_batches.insert(
            batch,
            SaveBatch {
                left: files,
                saved: 0,
                failed: 0,
                name: String::new(),
                dir,
                failure: None,
            },
        );
        batch
    }

    /// The result of a `SaveFacts` query.
    fn on_save_facts(
        &mut self,
        generation: u64,
        result: Option<&kourier_core::facade::SaveFacts>,
    ) -> Changes {
        let Some(what) = self.save_asks.remove(&generation) else {
            return Changes::default();
        };
        match (what, result) {
            (SaveAsk::Suggest(attachment), Some(facts)) => {
                let name = self.save_name(facts);
                self.save_answers
                    .push(SaveAnswer::Suggested { attachment, name });
            }
            (SaveAsk::Suggest(_), None) => {
                self.save_answers.push(SaveAnswer::Done(SaveOutcome {
                    saved: 0,
                    failed: 1,
                    name: String::new(),
                    dir: std::path::PathBuf::new(),
                    failure: Some(SaveFailure::Gone),
                }));
            }
            (SaveAsk::Name { batch, attachment }, Some(facts)) => {
                let name = self.save_name(facts);
                self.start_save(batch, attachment, name, true);
                return Changes::default();
            }
            (SaveAsk::Name { batch, .. }, None) => {
                self.save_file_ended(batch, None, Some(SaveFailure::Gone));
            }
        }
        Changes {
            saves: !self.save_answers.is_empty(),
            ..Changes::default()
        }
    }

    /// The core saved `attachment`.
    fn on_attachment_saved(&mut self, attachment: AttachmentId) -> Changes {
        if let Some(run) = self.save_runs.remove(&attachment) {
            self.save_file_ended(run.batch, Some(&run.name), None);
            self.start_next_save(attachment);
        }
        Changes {
            saves: !self.save_answers.is_empty(),
            ..Changes::default()
        }
    }

    /// The core refused to save the attachment of `subject`: a clash in a batch tries the next
    /// name, anything else ends the file.
    fn on_save_failed(&mut self, error: &CoreError, subject: &ActionSubject) -> Changes {
        let Some(attachment) = subject.attachment() else {
            return Changes::default();
        };
        let Some(run) = self.save_runs.get_mut(&attachment) else {
            return Changes::default();
        };
        let clash = matches!(
            error,
            CoreError::Save(kourier_core::facade::SaveError::Exists)
        );
        if clash && run.keep_both && run.tries < SAVE_TRIES {
            run.tries += 1;
            run.name = crate::save_name::with_number(&run.base, run.tries);
            let target = kourier_core::facade::SaveTarget {
                dir: run.dir.clone(),
                name: run.name.clone(),
            };
            self.send(UiCommand::SaveAttachment {
                attachment,
                target,
                mode: kourier_core::facade::SaveMode::NoReplace,
            });
            return Changes::default();
        }
        if let Some(run) = self.save_runs.remove(&attachment) {
            self.save_file_ended(run.batch, None, Some(save_failure(error)));
            self.start_next_save(attachment);
        }
        Changes {
            saves: !self.save_answers.is_empty(),
            ..Changes::default()
        }
    }
}

impl Failure {
    /// The kind of `error`.
    pub(crate) fn of(error: &CoreError) -> Self {
        match error {
            CoreError::InvalidInput(_)
            | CoreError::Refused { .. }
            | CoreError::InvalidState(_)
            | CoreError::UnknownConversation(_)
            | CoreError::UnknownAccount(_) => Self::Refused,
            CoreError::NotImplemented(_) | CoreError::UnknownNetwork(_) => Self::Unavailable,
            CoreError::Backend(e) => Self::Network(match e.kind {
                ErrorKind::RateLimited { .. } => NetworkFailure::RateLimited,
                _ => NetworkFailure::Other,
            }),
            _ => Self::Internal,
        }
    }
}

impl UiSource for CoreSource {
    fn time(&self) -> TimeContext<'_> {
        TimeContext::new(self.now_ms(), self.calendar.as_ref())
    }

    fn set_account_tints(&mut self, tints: HashMap<kourier_model::AccountUuid, String>) -> Changes {
        self.own_tints = tints;
        Changes {
            accounts: true,
            conversations: true,
            header: self.detail.is_some(),
            ..Changes::default()
        }
    }

    fn accounts(&self) -> Vec<AccountRow> {
        self.account_rows()
    }

    fn conversations(&self, filter: &ConversationFilter) -> Vec<ConversationRow> {
        let time = self.time();
        let hidden = self.resolve_hidden(filter);
        let table = self.account_table(&hidden, !filter.query.trim().is_empty());
        let mut rows = filter.apply_resolved(
            self.list.iter().map(|c| {
                let mut row = self.conversation_row(c, &time);
                table.decorate(&mut row);
                row
            }),
            &hidden,
        );
        table.finish(&mut rows);
        rows
    }

    fn archived_count(&self, filter: &ConversationFilter) -> u32 {
        let hidden = self.resolve_hidden(filter);
        let n = self
            .list
            .iter()
            .filter(|c| archived_row(c) && filter.counts_archived(c.account, &hidden))
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    fn accounts_known(&self) -> bool {
        self.accounts_q.applied() > 0
    }

    fn chat(&self, id: ConversationId) -> Option<ChatHeader> {
        let d = self
            .detail
            .as_ref()
            .filter(|d| Some(d.row.id) == self.open)?;
        (d.row.id == id).then(|| self.header(d, &self.time()))
    }

    fn timeline(&self, id: ConversationId) -> Vec<MessageRow> {
        if self.open != Some(id) || !self.window.loaded {
            return Vec::new();
        }
        self.timeline_rows(&self.time())
    }

    fn wait_lines(&self, id: ConversationId) -> Vec<(MessageId, timeline::WaitLine)> {
        if self.open != Some(id) || !self.window.loaded {
            return Vec::new();
        }
        let time = self.time();
        let (hold, safety_number, who) = self.wait_context();
        let who = who.map(|u| crate::names::single_line(&self.names.display_name(u)));
        self.window
            .rows
            .iter()
            .filter(|r| r.outbox.is_some())
            .map(|r| {
                // The rule of `build_rows`: a pending own message that was not deleted.
                let receipt = if r.origin != OriginKind::Remote && r.deleted.is_none() {
                    timeline::Receipt::from_state(r.out_state.as_ref())
                } else {
                    timeline::Receipt::None
                };
                let wait = self.send_wait(r, hold, safety_number);
                (
                    r.id,
                    timeline::row_wait_line(
                        receipt,
                        r.out_state.as_ref(),
                        wait,
                        who.as_deref(),
                        r.actions.safety_number.is_some(),
                        &time,
                    ),
                )
            })
            .collect()
    }

    fn upload_rows(&self, id: ConversationId, shown: &[MessageRow]) -> Vec<(usize, MessageRow)> {
        if self.open != Some(id) || !self.window.loaded {
            return Vec::new();
        }
        let waiting: Vec<&CoreMessage> = self
            .window
            .rows
            .iter()
            .filter(|r| r.outbox.is_some())
            .collect();
        let mut out = Vec::new();
        for (at, row) in shown.iter().enumerate() {
            let Some(message) = row
                .message_id()
                .filter(|_| row.kind == timeline::RowKind::Message)
            else {
                continue;
            };
            let core = waiting.iter().find(|r| r.id == message);
            if core.is_none() && !timeline::has_ring(row) {
                continue;
            }
            let mut next = row.clone();
            match core {
                Some(core) => self.give_rings(core, &mut next),
                // The message left the outbox (the timeline is rebuilt for that): no rings.
                None => timeline::apply_uploads(&mut next, &|_| None, false),
            }
            if next.blocks != row.blocks {
                out.push((at, next));
            }
        }
        out
    }

    fn networks(&self) -> Vec<NetworkChoice> {
        self.networks.clone()
    }

    fn demo_conversation(&self) -> Option<ConversationId> {
        self.demo
    }

    fn start_login(&mut self, network: NetworkKind) -> Result<LoginView, SourceError> {
        if !self
            .networks
            .iter()
            .any(|n| n.kind == network && n.available && n.on)
        {
            // Not in this build, or an experimental network that is off at this start: its
            // row opens the warning page, which turns it on and restarts; no login starts.
            return Err(SourceError::Unavailable);
        }
        // "Try again" after a login that failed goes the SAME way (an expired code asks for a
        // new code, a refused number is typed again with the number still in the field), and
        // after a relink that failed it links the same account again: its conversations and
        // messages belong to it. A login the user left (`cancel_login`) is forgotten, so "Add
        // account" afterwards adds one, the first way.
        let again = self
            .login
            .as_ref()
            .filter(|l| l.view.kind == LoginViewKind::Failed && l.network == network)
            .map(|l| (l.flow.clone(), l.typed.clone(), l.relink));
        let (flow, typed, relink) = again.unwrap_or_else(|| (self.first_flow(network), None, None));
        if let Some(account) = relink
            && self.account_network(account) == network
            && self.accounts.iter().any(|a| a.id == account)
        {
            return self.relink_with(account, &flow, typed);
        }
        self.cancel_login();
        self.add_account(network, &flow, typed)
    }

    fn switch_login_flow(&mut self, flow: &str) -> Result<LoginView, SourceError> {
        let Some(login) = self.login.as_ref() else {
            return Err(SourceError::Unavailable);
        };
        let network = login.network;
        // The login has not shown a step yet (the click before was just made, or the number
        // is being checked): a second click on the button (a double click, a key held) changes
        // nothing. One click, one restart.
        if login.view.kind == LoginViewKind::Waiting && !login.ended_linked {
            return Ok(login.view.clone());
        }
        // Only the other flow of the network, and only where the page offers the switch
        // (`with_flows`): never in the middle of a link that is made, or after it.
        if login.ended_linked || login.flow == flow || login.view.other_flow != flow {
            return Err(SourceError::Unavailable);
        }
        let (relink, typed) = (login.relink, login.typed.clone());
        // The running login is stopped (its late steps belong to an account nobody waits for any
        // more) and the other flow starts at once: the person did not leave the page, so no
        // "Stop Linking?".
        self.cancel_login();
        match relink {
            Some(account) => self.relink_with(account, flow, typed),
            None => self.add_account(network, flow, typed),
        }
    }

    fn send_text(
        &mut self,
        id: ConversationId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Option<MessageId>, SourceError> {
        let text = crate::composer::outgoing_text(text).ok_or(SourceError::Empty)?;
        if self.typing_sent.is_some_and(|(c, _)| c == id) {
            self.typing_sent = None;
            self.send(UiCommand::SetTyping {
                conv: id,
                on: false,
            });
        }
        if self.drafts.remove(&id).is_some() {
            self.send(UiCommand::SaveDraft {
                conv: id,
                draft: None,
            });
        }
        let raw = text.body.clone();
        if !self.send(UiCommand::SendText {
            conv: id,
            text,
            reply_to,
        }) {
            return Err(SourceError::Unavailable);
        }
        let after =
            (self.open == Some(id) && self.window.loaded && !self.window.has_newer).then(|| {
                self.window
                    .rows
                    .iter()
                    .map(|r| r.id)
                    .max()
                    .unwrap_or(MessageId(0))
            });
        self.last_sent = Some(LastSent {
            conv: id,
            text: raw,
            reply: reply_to,
            after,
        });
        self.notice.clear();
        self.answered(id);
        Ok(None)
    }

    fn send_files(
        &mut self,
        id: ConversationId,
        paths: Vec<std::path::PathBuf>,
        as_file: Vec<bool>,
        with_location: Vec<bool>,
        caption: &str,
        reply_to: Option<MessageId>,
    ) -> Result<u64, SourceError> {
        if paths.is_empty() {
            return Err(SourceError::Empty);
        }
        let id = self.survivor(id);
        if self.typing_sent.is_some_and(|(c, _)| c == id) {
            self.typing_sent = None;
            self.send(UiCommand::SetTyping {
                conv: id,
                on: false,
            });
        }
        if self.drafts.remove(&id).is_some() {
            self.send(UiCommand::SaveDraft {
                conv: id,
                draft: None,
            });
        }
        let caption_text = crate::composer::outgoing_text(caption);
        let Some(request) =
            self.send_files_request(id, paths, as_file, with_location, caption_text, reply_to)
        else {
            return Err(SourceError::Unavailable);
        };
        self.notice.clear();
        self.answered(id);
        Ok(request.0)
    }

    fn take_refused_files(&mut self) -> Option<RefusedSend> {
        self.refused_files.pop_front()
    }

    fn preparing_files(&self, conv: ConversationId) -> usize {
        let conv = self.survivor(conv);
        self.preparing
            .values()
            .filter(|p| self.survivor(p.conv) == conv)
            .map(|p| p.files)
            .sum()
    }

    fn prepared_files(&self, conv: ConversationId) -> usize {
        let conv = self.survivor(conv);
        self.preparing
            .values()
            .filter(|p| self.survivor(p.conv) == conv)
            .map(|p| p.done)
            .sum()
    }

    fn take_finished_sends(&mut self) -> Vec<FinishedSend> {
        std::mem::take(&mut self.finished)
    }

    fn strip_rules(&self, conv: ConversationId) -> StripRules {
        let Some(d) = self
            .detail
            .as_ref()
            .filter(|d| d.row.id == self.survivor(conv))
        else {
            return StripRules::default();
        };
        if d.features.hard_to_save.is_empty() {
            return StripRules::default();
        }
        StripRules {
            hard_to_save: d.features.hard_to_save.clone(),
            network: network_name(self.account_network(d.row.account)).to_owned(),
        }
    }

    fn max_text_bytes(&self, conv: ConversationId) -> Option<u32> {
        self.detail
            .as_ref()
            .filter(|d| d.row.id == self.survivor(conv))
            .and_then(|d| d.features.max_text_bytes)
    }

    fn archive_rule(&self, conv: ConversationId) -> ArchiveRule {
        let Some(d) = self
            .detail
            .as_ref()
            .filter(|d| d.row.id == self.survivor(conv))
        else {
            return ArchiveRule::default();
        };
        // An archive is a plain file: what the chat says about those (§3.8; `max_size` 0
        // means the network states no limit of its own for the kind).
        match d.features.files.get(&AttachmentKind::File) {
            Some(limits) => ArchiveRule {
                accepted: true,
                max: (limits.max_size > 0).then_some(limits.max_size),
            },
            None => ArchiveRule {
                accepted: false,
                max: None,
            },
        }
    }

    fn take_merges(&mut self) -> Vec<(ConversationId, ConversationId)> {
        std::mem::take(&mut self.merges)
    }

    fn load_settings(&mut self) {
        self.request_settings();
    }

    fn expiry_policy(&self) -> Option<&'static str> {
        self.expiry.map(ExpiryPolicy::as_str)
    }

    fn set_expiry_policy(&mut self, name: &str) -> bool {
        let Some(policy) = ExpiryPolicy::from_name(name) else {
            return false;
        };
        if self.expiry == Some(policy) {
            return true;
        }
        // A command that did not leave (the core has stopped) changes nothing: the option
        // reads as it was, and what was asked before still counts.
        if !self.send(UiCommand::SetExpiryPolicy { policy }) {
            return false;
        }
        // Shown at once; the core's answer to the query behind the command confirms it, or
        // says what the option still is when it could not be stored. Answers to queries
        // sent before the command are about the old option and are dropped.
        self.expiry = Some(policy);
        self.settings_q.reset();
        self.request_expiry();
        true
    }

    fn quiet_notices(&self) -> Option<bool> {
        self.quiet
    }

    fn set_quiet_notices(&mut self, shown: bool) -> bool {
        if self.quiet == Some(shown) {
            return true;
        }
        // A command that did not leave (the core has stopped) changes nothing: the switch
        // reads as it was.
        if !self.send(UiCommand::SetQuietNotices { shown }) {
            return false;
        }
        // Shown at once; the core's answer to the query behind the command confirms it, or
        // says what the choice still is when it could not be stored. The choice changes the
        // sort time of chats and so the order and the previews of the list, which come from
        // the core (§8.10): answers to queries sent before the command describe the old
        // order and are dropped, and the whole list is read again (the command's
        // `ConversationsChanged` names only the chats whose sort time moved; a preview can
        // change without that).
        self.quiet = Some(shown);
        self.quiet_q.reset();
        self.request_quiet();
        self.list_q.reset();
        self.request_list();
        true
    }

    fn plan_files(&mut self, conv: ConversationId, request: PlanRequest) -> bool {
        self.plan_wanted = Some((self.survivor(conv), request));
        let t = self.plan_q.request();
        self.send_plan(t);
        !self.stopped
    }

    fn take_file_plan(&mut self) -> Option<PlanAnswer> {
        self.plan_answer.take()
    }

    fn take_plan_failure(&mut self) -> Option<u64> {
        self.plan_failed.take()
    }

    fn request_revisions(&mut self, message: MessageId) -> bool {
        self.revisions_wanted = Some(message);
        self.revisions_answer = None;
        self.revisions_shown = Some(message);
        let t = self.revisions_q.request();
        self.send_revisions(t);
        !self.stopped
    }

    fn take_revisions(&mut self) -> Option<RevisionList> {
        self.revisions_answer.take()
    }

    fn close_revisions(&mut self) {
        self.revisions_shown = None;
    }

    fn request_person(
        &mut self,
        account: AccountId,
        user: UserId,
        origin: Option<ConversationId>,
    ) -> bool {
        self.person_shown = Some((account, user.clone()));
        self.person_origin = origin;
        self.person_answer = None;
        // The first answer of a new ask is always handed over, even when it equals the page
        // of the one before (the front end cleared that one).
        self.person_last = None;
        // A fresh profile, once for each time the page is opened; the backend decides whether
        // it is due (Signal: not within a day), and the page shows what it has meanwhile.
        self.send(UiCommand::RefreshContact {
            account,
            user: user.clone(),
        });
        // Answers still on their way are for a person that is not shown (or for this one,
        // asked for earlier: the new query follows them).
        self.person_q.reset();
        self.person_sent.clear();
        let t = self.person_q.request();
        self.send_person(t);
        !self.stopped
    }

    fn take_person(&mut self) -> Option<PersonUpdate> {
        self.person_answer.take()
    }

    fn close_person(&mut self) {
        self.person_shown = None;
        self.person_origin = None;
        self.person_answer = None;
        self.person_last = None;
        self.person_sent.clear();
        self.person_q.reset();
    }

    fn request_copy(
        &mut self,
        part: crate::person::CopyPart,
        target: crate::person::CopyTarget,
    ) -> bool {
        use crate::person::{CopyAsked, CopyTarget};
        use kourier_core::facade::PersonRef;
        self.copy_generation += 1;
        self.copy_asked = Some((self.copy_generation, CopyAsked::Person(part)));
        let query = match target {
            CopyTarget::Person { account, user } => Query::Person { account, user },
            CopyTarget::Chat(id) => Query::PersonOf(PersonRef::Chat(id)),
            CopyTarget::Account(id) => Query::PersonOf(PersonRef::Own(id)),
        };
        self.send(UiCommand::Query {
            token: QueryToken {
                model: MODEL_COPY,
                generation: self.copy_generation,
            },
            query,
        });
        true
    }

    fn request_group_invite(
        &mut self,
        conversation: ConversationId,
        action: kourier_model::GroupInviteAction,
    ) -> bool {
        self.copy_generation += 1;
        self.copy_asked = Some((
            self.copy_generation,
            crate::person::CopyAsked::Invite(action),
        ));
        self.send(UiCommand::Query {
            token: QueryToken {
                model: MODEL_COPY,
                generation: self.copy_generation,
            },
            query: Query::GroupInvite {
                conversation,
                action,
            },
        });
        true
    }

    fn take_copy(&mut self) -> Option<crate::person::CopyUpdate> {
        self.copy_answer.take()
    }

    fn request_safety(&mut self, account: AccountId, user: UserId) -> bool {
        self.safety_shown = Some((account, user));
        self.safety_answer = None;
        // The first answer of a new ask is always handed over.
        self.safety_last = None;
        self.safety_key = None;
        self.safety_notice.clear();
        self.safety_q.reset();
        self.safety_sent.clear();
        let t = self.safety_q.request();
        self.send_safety(t);
        !self.stopped
    }

    fn take_safety(&mut self) -> Option<crate::safety::SafetyView> {
        self.safety_answer.take()
    }

    fn close_safety(&mut self) {
        self.safety_shown = None;
        self.safety_answer = None;
        self.safety_last = None;
        self.safety_key = None;
        self.safety_notice.clear();
        self.safety_sent.clear();
        self.safety_q.reset();
    }

    fn safety_action(
        &mut self,
        account: AccountId,
        user: UserId,
        action: crate::safety::SafetyAction,
        key: kourier_model::IdentityFingerprint,
    ) -> bool {
        use crate::safety::SafetyAction;
        // A new click: the line of an earlier refusal is not carried into the next answer. The
        // key it acts on IS the one on screen, so it stays "the one last shown": a number that
        // turns into another after the click is a swap and says so (W3; the Qt side compares the
        // same way). A refusal drops it and brings its own line (`on_action_failed`).
        self.safety_notice.clear();
        self.safety_key = Some(key.clone());
        self.send(match action {
            SafetyAction::Mark | SafetyAction::Clear => UiCommand::SetVerified {
                account,
                user,
                verified: action == SafetyAction::Mark,
                key,
            },
            // BQ-2b: Confirm is its own command, never a Clear (a Clear always tells the
            // other devices).
            SafetyAction::Confirm => UiCommand::ConfirmIdentity { account, user, key },
        })
    }

    fn share_targets(&self) -> Vec<crate::share::ShareTarget> {
        // The list holds no chat the user deleted (hidden), so the picker lists none either.
        self.list
            .iter()
            .map(|c| crate::share::ShareTarget {
                id: c.id,
                account: c.account,
                title: conversation_title(c),
                avatar_url: self.avatar_url(c.account, c.avatar.as_ref()),
                is_self: c.is_self,
                can_send: c.can_send,
                archived: archived_row(c),
                pinned_order: c.pinned_order,
                last_activity: c.last_activity,
            })
            .collect()
    }

    fn send_contact(&mut self, conv: ConversationId, account: AccountId, user: UserId) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        self.send(UiCommand::SendContact {
            conv,
            account,
            user,
        })
    }

    fn request_group(&mut self, account: AccountId, conv: ConversationId) -> bool {
        self.group_shown = Some((account, conv));
        self.group_answer = None;
        self.group_last = None;
        // Answers still on their way are for a group that is not shown (or for this one, asked
        // for earlier: the new query follows them).
        self.group_q.reset();
        self.group_sent.clear();
        let t = self.group_q.request();
        self.send_group(t);
        !self.stopped
    }

    fn take_group(&mut self) -> Option<GroupUpdate> {
        self.group_answer.take()
    }

    fn close_group(&mut self) {
        self.group_shown = None;
        self.group_answer = None;
        self.group_last = None;
        self.group_sent.clear();
        self.group_q.reset();
    }

    fn open_chat_media(
        &mut self,
        slot: crate::chat_media::MediaSlot,
        account: AccountId,
        conv: ConversationId,
    ) -> bool {
        self.open_media_slot(slot, account, conv)
    }

    fn open_media_viewer(
        &mut self,
        from: crate::chat_media::MediaSlot,
        message: MessageId,
    ) -> bool {
        self.open_media_viewer_slot(from, message)
    }

    fn more_chat_media(&mut self, slot: crate::chat_media::MediaSlot) {
        self.more_media_slot(slot);
    }

    fn close_chat_media(&mut self, slot: crate::chat_media::MediaSlot) {
        self.close_media_slot(slot);
    }

    fn take_chat_media(&mut self) -> Vec<crate::chat_media::MediaUpdate> {
        self.take_media_updates()
    }

    fn edit_target(&self, conv: ConversationId, message: MessageId) -> Option<EditTarget> {
        let (original, caption) = self.edit_source(conv, message)?;
        // What the composer can hold: the text as sent, mentions written out as "@Name".
        let form = EditForm::new(original, &self.names);
        Some(EditTarget {
            message,
            text: form.text().trim_end().to_owned(),
            caption,
        })
    }

    fn edit_message(
        &mut self,
        conv: ConversationId,
        message: MessageId,
        text: &str,
    ) -> Result<bool, SourceError> {
        let Some((original, caption)) = self.edit_source(conv, message) else {
            return Err(SourceError::CannotSend(
                trc("@info:status", "This message can no longer be edited.").into_owned(),
            ));
        };
        let typed = crate::composer::outgoing_text(text);
        // A text cannot become empty; a caption can (that removes it).
        if typed.is_none() && !caption {
            return Err(SourceError::Empty);
        }
        let typed = typed.map(|t| t.body).unwrap_or_default();
        let form = EditForm::new(original, &self.names);
        // The text is what it was (the composer normalizes what it shows the same way).
        let unchanged = crate::composer::outgoing_text(form.text())
            .map(|t| t.body)
            .unwrap_or_default();
        if typed == unchanged {
            return Ok(false);
        }
        // The composer has no formatting and no mention marks: what the user did not touch
        // keeps its formatting ranges and its mentions (a bare text would reveal a spoiler
        // and turn every mention into plain text).
        let new = form.apply(original, &typed);
        if !self.send(UiCommand::EditMessage { message, text: new }) {
            return Err(SourceError::Unavailable);
        }
        self.notice.clear();
        Ok(true)
    }

    fn delete_message(&mut self, message: MessageId, for_everyone: bool) -> bool {
        self.notice.clear();
        self.send(UiCommand::DeleteMessage {
            message,
            for_everyone,
        })
    }

    fn react(&mut self, message: MessageId, emoji: &str, remove: bool) -> bool {
        if emoji.trim().is_empty() {
            return false;
        }
        self.notice.clear();
        self.send(UiCommand::React {
            message,
            emoji: emoji.to_owned(),
            remove,
        })
    }

    fn retry_message(&mut self, message: MessageId) -> bool {
        self.notice.clear();
        self.send(UiCommand::RetryMessage { message })
    }

    fn cancel_message(&mut self, message: MessageId) -> bool {
        self.notice.clear();
        self.send(UiCommand::CancelMessage { message })
    }

    fn set_timer(&mut self, conv: ConversationId, secs: u32) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        // The next change forgets the last failure on the pages (the front end reads them
        // again: `Changes::person` and `group` come with the call).
        let _ = self.timer_failure_on_pages(None, "");
        self.send(UiCommand::SetConversationTimer {
            conv,
            timer: (secs > 0).then(|| Duration::from_secs(u64::from(secs))),
        })
    }

    fn clear_conversation(&mut self, conv: ConversationId) -> bool {
        let conv = self.survivor(conv);
        if self.fills_now(conv) {
            return false;
        }
        self.notice.clear();
        self.send(UiCommand::ClearConversation { conv })
    }

    fn delete_conversation(&mut self, conv: ConversationId) -> bool {
        let conv = self.survivor(conv);
        if self.fills_now(conv) {
            return false;
        }
        self.notice.clear();
        // The core deletes the stored draft with the chat; this session's copy goes too, or
        // the list would show a draft for a chat that is gone.
        self.drafts.remove(&conv);
        self.send(UiCommand::DeleteConversation { conv })
    }

    fn mark_read(&mut self, conv: ConversationId) -> bool {
        // (Not `maybe_mark_read`: no focus, visibility or `last_marked` rule applies to what
        // the user asked for. The core ignores a chat with nothing unread.)
        let conv = self.survivor(conv);
        self.send(UiCommand::MarkRead { conv })
    }

    fn chat_offer(&self, conv: ConversationId) -> ChatOffer {
        let conv = self.survivor(conv);
        match self.row_of(conv) {
            Some(row) => self.offer_for(row, &UiSource::time(self)),
            None => ChatOffer::default(),
        }
    }

    fn set_archived(&mut self, conv: ConversationId, archived: bool) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        let was_pinned = self.row_of(conv).is_some_and(|c| c.pinned_order.is_some());
        let sent = self.send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetArchived { archived },
        });
        if sent {
            let kind = if archived {
                AwaitedKind::Archive
            } else {
                AwaitedKind::Unarchive
            };
            self.await_told(conv, kind, was_pinned);
        }
        sent
    }

    fn set_pinned(&mut self, conv: ConversationId, pinned: bool) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        self.send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetPinned { pinned },
        })
    }

    fn set_muted(&mut self, conv: ConversationId, until: Option<i64>) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        self.send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetMuted { until },
        })
    }

    fn set_marked_unread(&mut self, conv: ConversationId, marked: bool) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        // The click's own echo runs `maybe_mark_read`, which reads an open chat that is marked
        // away at once. The mark is the user's word that this chat is not read yet: say the
        // newest incoming message is marked already, so only a newer one (a real reading)
        // clears it, as on Desktop.
        if marked
            && self.open == Some(conv)
            && let Some(newest) = self
                .window
                .rows
                .iter()
                .rev()
                .find(|r| r.origin == OriginKind::Remote)
                .map(|r| r.id)
        {
            self.last_marked = Some((conv, newest));
        }
        let sent = self.send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetMarkedUnread {
                marked_unread: marked,
            },
        });
        if sent && marked {
            self.await_told(conv, AwaitedKind::MarkUnread, false);
        }
        sent
    }

    fn undo_archive(&mut self, conv: ConversationId, was_pinned: bool) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        // (No notice of its own, and nothing awaited: Desktop's Undo shows no second toast.)
        let sent = self.send(UiCommand::ChangeConversation {
            conv,
            change: ConversationChange::SetArchived { archived: false },
        });
        if was_pinned {
            return self.send(UiCommand::ChangeConversation {
                conv,
                change: ConversationChange::SetPinned { pinned: true },
            }) && sent;
        }
        sent
    }

    fn set_roaming_writes(&mut self, account: AccountId, on: bool) -> bool {
        self.notice.clear();
        self.send(UiCommand::SetRoamingWrites { account, on })
    }

    fn take_told(&mut self) -> Vec<ChatTold> {
        std::mem::take(&mut self.told)
    }

    fn delete_attachment(&mut self, attachment: AttachmentId) -> bool {
        self.notice.clear();
        self.send(UiCommand::DeleteAttachment { attachment })
    }

    fn jump_to(&mut self, message: MessageId) -> Changes {
        if self.open.is_none() || !self.window.loaded {
            return Changes::default();
        }
        if self.window.rows.iter().any(|r| r.id == message) {
            return Changes {
                jump: Some(message),
                ..Changes::default()
            };
        }
        // Not loaded: the window moves to the stretch around it (the answer names the jump).
        self.want_around = Some(message);
        self.pump_timeline();
        Changes::default()
    }

    fn jump_to_latest(&mut self) -> Changes {
        if self.open.is_some() && self.window.loaded && self.window.has_newer {
            self.want_newest = true;
            self.pump_timeline();
        }
        Changes::default()
    }

    fn flags_deadline(&self) -> Option<Instant> {
        self.flags_deadline
    }

    fn queries_in_flight(&self) -> usize {
        [
            &self.accounts_q,
            &self.list_q,
            &self.detail_q,
            &self.read_ahead_q,
            &self.timeline_q,
        ]
        .into_iter()
        .filter(|q| q.pending())
        .count()
    }

    fn refresh_flags(&mut self) -> Changes {
        if self.flags_deadline.is_some_and(|at| at <= Instant::now()) {
            // Read once; the answer sets the next deadline.
            self.flags_deadline = None;
            self.reload_timeline();
        }
        Changes::default()
    }

    fn banner(&self, account: Option<AccountId>) -> Option<Banner> {
        self.banner_of(account, &Hidden::default(), BannerSort::State)
    }

    fn transfer_banner(&self, account: Option<AccountId>) -> Option<Banner> {
        self.banner_of(account, &Hidden::default(), BannerSort::Transfer)
    }

    fn banners(&self, filter: &ConversationFilter) -> Banners {
        let hidden = self.resolve_hidden(filter);
        Banners {
            state: self.banner_of(None, &hidden, BannerSort::State),
            transfer: self.banner_of(None, &hidden, BannerSort::Transfer),
            shown_transfer: self.banner_of(None, &hidden, BannerSort::ShownTransfer),
        }
    }

    fn relink(&mut self, account: AccountId) -> Result<LoginView, SourceError> {
        let flow = self.first_flow(self.account_network(account));
        self.cancel_login();
        self.relink_with(account, &flow, None)
    }

    fn restart_account(&mut self, account: AccountId) -> bool {
        self.account_errors.remove(&account);
        self.send(UiCommand::RestartAccount { account })
    }

    fn submit_challenge(&mut self, account: AccountId, answer: &str) -> bool {
        let answer = answer.trim();
        if answer.is_empty() {
            return false;
        }
        self.notice.clear();
        self.send(UiCommand::SubmitChallenge {
            account,
            answer: Redacted::new(answer.to_owned()),
        })
    }

    fn challenged_account(&self) -> Option<AccountId> {
        self.visible_accounts()
            .find(|a| {
                matches!(a.state, AccountState::Active)
                    && matches!(
                        self.conn_of(a),
                        ConnState::NeedsUserAction(
                            kourier_model::UserAction::SolveChallenge { .. }
                        )
                    )
            })
            .map(|a| a.id)
    }

    fn skip_transfer(&mut self, account: AccountId) -> bool {
        self.transfers.skip(account) && self.send(UiCommand::SkipHistoryTransfer { account })
    }

    fn dismiss_transfer(&mut self, account: AccountId) -> bool {
        self.transfers.dismiss(account) && self.send(UiCommand::DismissHistoryTransfer { account })
    }

    fn take_transfer_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.transfer_notices)
    }

    fn ask_replace(&mut self, account: AccountId) -> Option<ReplaceQuestion> {
        let a = self.visible_accounts().find(|a| a.id == account)?;
        // Only where the action is offered (`offers_history`): a front-end path that reaches
        // the question some other way (a menu item that is hidden but not disabled, a stale
        // click) must not remove the account whose history is being imported, or one whose
        // network has no transfer (`buttons.md` F4).
        if !self.offers_history(a) {
            return None;
        }
        // Which account it is: its name and, where the network has one, its number.
        let about = self.account_about(a);
        self.replace_asked = Some(account);
        self.action_asked = None;
        // Confirming cancels a link in progress (of another account: this one's link page
        // offers no question): the question says so (`buttons.md` F3).
        Some(match self.abandoned_network() {
            Some(network) => replace_question_cancelling(account, about, network),
            None => replace_question(account, about),
        })
    }

    fn confirm_replace(&mut self, account: AccountId) -> Result<LoginView, SourceError> {
        // Only the question that is open, about this very account, can be confirmed, and
        // once: a second confirmation finds none.
        if self.replace_asked != Some(account) {
            return Err(SourceError::Unavailable);
        }
        self.replace_asked = None;
        let network = self
            .visible_accounts()
            .find(|a| a.id == account)
            .map(|a| a.network)
            .ok_or(SourceError::Unavailable)?;
        // Nothing is removed that could not be linked again now (in this build, and on).
        if !self
            .networks
            .iter()
            .any(|n| n.kind == network && n.available && n.on)
        {
            return Err(SourceError::Unavailable);
        }
        self.cancel_login();
        if !self.send(UiCommand::RemoveAccount { account }) {
            return Err(SourceError::Unavailable);
        }
        // The history transfer is Signal's: the first flow of the network.
        let flow = self.first_flow(network);
        let view = self.add_account(network, &flow, None)?;
        // (the add just queued is the newest: this many are ahead of it)
        self.replacing = Some(Replacing {
            old: account,
            ahead: self.adds.len().saturating_sub(1),
        });
        Ok(view)
    }

    fn take_replace_news(&mut self) -> Vec<ReplaceNews> {
        std::mem::take(&mut self.replace_news)
    }

    fn cancel_replace(&mut self) {
        self.replace_asked = None;
    }

    fn ask_account_action(
        &mut self,
        account: AccountId,
        action: AccountAction,
    ) -> Option<ReplaceQuestion> {
        let a = self.visible_accounts().find(|a| a.id == account)?;
        if action == AccountAction::Unlink && !self.offers_unlink(a) {
            return None;
        }
        let about = self.account_about(a);
        let off = self.network_paused(a.network);
        let missing = self.network_missing(a.network).is_some();
        let kind = a.network;
        // A logged-out account has been unlinked already (by the phone, or by "Unlink…"):
        // there is nothing for Kourier to unlink.
        let linked =
            !matches!(a.state, AccountState::LoggedOut { .. }) && !self.logged_out(account);
        // One question about an account at a time: this one replaces any other.
        self.replace_asked = None;
        self.action_asked = Some((account, action));
        Some(match action {
            AccountAction::Remove if linked && off => {
                crate::account_actions::remove_question_while_off(account, about, kind, missing)
            }
            AccountAction::Remove => {
                crate::account_actions::remove_question(account, about, linked)
            }
            AccountAction::Unlink => crate::account_actions::unlink_question(account, about),
        })
    }

    fn confirm_account_action(&mut self, account: AccountId, action: AccountAction) -> bool {
        // Only the open question about this very account and action, and once. Nothing
        // else is sent: no other login is cancelled (`buttons.md` F3), nothing is linked.
        if self.action_asked != Some((account, action)) {
            return false;
        }
        self.action_asked = None;
        let Some(a) = self.visible_accounts().find(|a| a.id == account) else {
            return false;
        };
        match action {
            AccountAction::Remove => self.send(UiCommand::RemoveAccount { account }),
            AccountAction::Unlink => {
                // The account may have logged out or begun linking while the question was
                // open: it has no device to unlink then.
                self.offers_unlink(a) && self.send(UiCommand::Unlink { account })
            }
        }
    }

    fn cancel_account_action(&mut self) {
        self.action_asked = None;
    }

    fn take_logout_notices(&mut self) -> Vec<LogoutNotice> {
        std::mem::take(&mut self.logout_notices)
    }

    fn download_attachment(&mut self, id: AttachmentId) -> bool {
        self.send(UiCommand::DownloadAttachment { attachment: id })
    }

    fn open_media(&mut self, id: AttachmentId) -> bool {
        self.send(UiCommand::OpenMedia { attachment: id })
    }

    fn close_media(&mut self, id: AttachmentId) {
        self.send(UiCommand::CloseMedia { attachment: id });
    }

    fn take_media(&mut self) -> Vec<MediaAnswer> {
        std::mem::take(&mut self.media_answers)
    }

    fn ask_save(&mut self, id: AttachmentId) -> bool {
        self.ask_save_facts(SaveAsk::Suggest(id))
    }

    fn save_to(&mut self, id: AttachmentId, dir: std::path::PathBuf, name: String) -> bool {
        let batch = self.new_save_batch(1, dir);
        self.start_save(batch, id, name, false);
        !self.stopped
    }

    fn save_all(&mut self, ids: Vec<AttachmentId>, dir: std::path::PathBuf) -> bool {
        let Ok(files) = u32::try_from(ids.len()) else {
            return false;
        };
        if files == 0 {
            return false;
        }
        let batch = self.new_save_batch(files, dir);
        for attachment in ids {
            self.ask_save_facts(SaveAsk::Name { batch, attachment });
        }
        !self.stopped
    }

    fn take_saves(&mut self) -> Vec<SaveAnswer> {
        std::mem::take(&mut self.save_answers)
    }

    fn loaded(&self) -> bool {
        self.accounts_q.applied() > 0 && self.list_q.applied() > 0
    }

    fn is_open(&self, id: ConversationId) -> bool {
        self.open == Some(id)
    }

    fn on_event(&mut self, event: &UiEvent) -> Changes {
        match event {
            // The hold is a state: the event is the newest word (the rows carry it for a
            // start). The banner and the line under the messages that wait say it.
            UiEvent::AccountHold { account, hold } => {
                if self.holds.get(account).copied().flatten() == *hold {
                    return Changes::default();
                }
                self.holds.insert(*account, *hold);
                let open_here = self
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.row.account == *account);
                Changes {
                    accounts: true,
                    timeline: open_here && self.sends_waiting(),
                    ..Changes::default()
                }
            }
            UiEvent::AccountsChanged => {
                self.request_accounts();
                self.request_list();
                // An account linked or logged out: what its messages offer changed with it.
                self.request_detail();
                self.reload_timeline();
                // A safety number "once this account is connected" may be there now.
                self.refresh_safety();
                Changes::default()
            }
            UiEvent::AccountAdded { account } => {
                let news = self.replace_news.len();
                if self.pop_add(Some(*account)) == Some(true) {
                    // The user cancelled this login before the core announced the account.
                    self.cancelled.insert(*account);
                    self.send(UiCommand::CancelLogin { account: *account });
                } else if let Some(login) = self.login.as_mut()
                    && login.account.is_none()
                {
                    login.account = Some(*account);
                }
                self.request_accounts();
                Changes {
                    accounts: self.replace_news.len() != news,
                    ..Changes::default()
                }
            }
            UiEvent::LogoutOutcome {
                seq,
                account,
                removed,
                report,
                who,
            } => {
                // Told once: the core sends the outcomes of before `UiReady` again at it.
                if !self.logout_seen.insert(*seq) {
                    return Changes::default();
                }
                while self.logout_seen.len() > LOGOUT_SEEN_KEPT {
                    self.logout_seen.pop_first();
                }
                // Named by what the event carries (the account's row may be gone, or the
                // list not loaded yet): as the lists name it, and two links that look the
                // same by device and link date.
                let name = single_line(who.display_name.as_deref().unwrap_or_default());
                let name = if name.is_empty() {
                    network_name(who.network).to_owned()
                } else {
                    name
                };
                let link = crate::account_actions::LinkInfo {
                    twin: who.twin,
                    label: crate::transfer::device_label(
                        who.own_device,
                        &self.link_date(who.linked_at),
                    ),
                    date: self.link_date(who.linked_at),
                };
                self.logout_notices
                    .push(crate::account_actions::logout_notice(
                        *account,
                        *removed,
                        report,
                        &name,
                        network_name(who.network),
                        &link,
                    ));
                Changes {
                    account_notice: true,
                    ..Changes::default()
                }
            }
            UiEvent::AccountRemoved { account } => {
                let person_ended = self
                    .person_shown
                    .as_ref()
                    .is_some_and(|(a, _)| a == account)
                    && self.end_person();
                let group_ended =
                    self.group_shown.is_some_and(|(a, _)| a == *account) && self.end_group();
                let media_ended = self.end_media_of_account(*account);
                self.filling.retain(|_, a| a != account);
                self.lost_history.remove(account);
                self.account_errors.remove(account);
                self.cancelled.remove(account);
                self.unfinished_links.remove(account);
                // Nothing of its history transfer is shown any more, and a question about
                // it has lost its subject.
                let transfer = self.transfers.remove(*account);
                if self.replace_asked == Some(*account) {
                    self.replace_asked = None;
                }
                if self.action_asked.is_some_and(|(a, _)| a == *account) {
                    self.action_asked = None;
                }
                self.request_accounts();
                self.request_list();
                Changes {
                    transfer,
                    person: person_ended,
                    group: group_ended,
                    chat_media: media_ended,
                    ..Changes::default()
                }
            }
            UiEvent::AccountError { account, .. } => {
                self.account_errors.insert(*account);
                Changes {
                    accounts: true,
                    ..Changes::default()
                }
            }
            UiEvent::ConnState { account, state } => {
                if matches!(state, ConnState::Online { .. }) {
                    self.account_errors.remove(account);
                }
                self.conn.insert(*account, state.clone());
                // A pushed history stops with the connection it came on (the phone sends
                // again after a new one, if it has more): nothing is still "coming".
                let connected =
                    matches!(state, ConnState::Syncing { .. } | ConnState::Online { .. });
                let ended = !connected && self.pushes_history(*account) && self.end_fills(*account);
                // The backend deleted a transfer that ran with the login, and says nothing
                // about it.
                let out = matches!(state, ConnState::LoggedOut { .. });
                if out {
                    // The backend stopped the delivery with the login.
                    self.filling.retain(|_, a| a != account);
                    // And what it lost of the old link says nothing about a new one.
                    self.lost_history.remove(account);
                }
                // (Both: the numbers go with the backend's work, whatever the transfer did.)
                let transfer = out
                    && (self.transfers.logged_out(*account)
                        | self.transfers.forget_progress(*account));
                // A relinked account connects again: its row may have said Active already.
                let login = self.end_link_page();
                let open_here = self
                    .detail
                    .as_ref()
                    .is_some_and(|d| d.row.account == *account);
                Changes {
                    accounts: true,
                    login,
                    // The line under a message on its way says whether it waits for the
                    // connection.
                    timeline: open_here && self.sends_waiting(),
                    // The composer of a chat of this account says when it cannot send.
                    header: self
                        .detail
                        .as_ref()
                        .is_some_and(|d| d.row.account == *account),
                    transfer: transfer | ended,
                    ..Changes::default()
                }
            }
            UiEvent::LoginStep { account, step } => {
                // Every login's steps, the page's or one it left: whether the account still
                // links (what it offers depends on it).
                let ended = matches!(
                    step.kind,
                    LoginStepKind::Done | LoginStepKind::Failed { .. }
                );
                let mut changed = if ended {
                    self.unfinished_links.remove(account)
                } else {
                    !self.cancelled.contains(account) && self.unfinished_links.insert(*account)
                };
                // The announced account of a replace failed to link: the replace ends.
                if matches!(step.kind, LoginStepKind::Failed { .. }) {
                    changed |= self.end_announced(*account);
                }
                let Some(login) = self.login.as_ref() else {
                    return Changes {
                        accounts: changed,
                        ..Changes::default()
                    };
                };
                let ours = match login.account {
                    Some(a) => a == *account,
                    // The core announces an account before its first step, so while ours is
                    // not announced, a step belongs to another (cancelled) login. Without a
                    // pending `AddAccount` (lost to a lagged channel) the step is taken.
                    None => self.adds.is_empty() && !self.cancelled.contains(account),
                };
                // The page ended when the core took the account (`end_link_page`): the
                // backend's later steps of that login (its tail, its `Done`) change nothing.
                // (Only then: after the backend's own `Done` the core can still refuse the
                // account, a second link of one that is here, and that failure shows.)
                if !ours || login.ended_linked {
                    return Changes {
                        accounts: changed,
                        ..Changes::default()
                    };
                }
                let view = Self::with_typed(
                    self.device_name_default(login_view(step, login.network)),
                    login.typed.as_ref(),
                );
                let view = self.with_flows(view, login.network, &login.flow);
                let done = matches!(step.kind, LoginStepKind::Done);
                let failed = matches!(step.kind, LoginStepKind::Failed { .. });
                if let Some(login) = self.login.as_mut() {
                    login.account = if failed { None } else { Some(*account) };
                    login.view = view;
                    if done {
                        login.typed = None;
                    }
                }
                if done {
                    self.request_accounts();
                    self.request_list();
                }
                Changes {
                    login: true,
                    accounts: changed,
                    ..Changes::default()
                }
            }
            UiEvent::ConversationsChanged { ids } => {
                self.request_list();
                if self.open.is_some_and(|o| ids.contains(&o)) {
                    self.request_detail();
                }
                // The person's chat, or any group (a title, a membership): the page reads again.
                self.refresh_person();
                // The group's page changes with its own group only (members, rules, a title).
                if self.group_shown.is_some_and(|(_, g)| ids.contains(&g)) {
                    self.refresh_group();
                }
                Changes::default()
            }
            UiEvent::ConversationsRemoved { ids } => {
                self.request_list();
                let (person_ended, group_ended) = self.end_pages_of(ids);
                let media_ended = self.end_media_of(ids);
                if self.open.is_some_and(|o| ids.contains(&o)) {
                    let mut ch = self.close();
                    ch.person |= person_ended;
                    ch.group |= group_ended;
                    ch.chat_media |= media_ended;
                    return ch;
                }
                // A group in common may have gone: the person's page reads again unless it
                // ended.
                if !person_ended {
                    self.refresh_person();
                }
                Changes {
                    person: person_ended,
                    group: group_ended,
                    chat_media: media_ended,
                    ..Changes::default()
                }
            }
            UiEvent::MessagesChanged { conv, .. } | UiEvent::TimelineInvalidated { conv } => {
                if self.open == Some(*conv) {
                    self.reload_timeline();
                    self.refresh_revisions();
                }
                // The gallery of that chat, wherever it is shown (a picture came, was deleted,
                // finished downloading).
                self.refresh_media_of(*conv);
                Changes::default()
            }
            // The core stored the messages of a send of files (their rows are announced by
            // `MessagesChanged`): it is no longer in preparation, and the composer may
            // delete what it made for it.
            UiEvent::FilesStored { request, .. } => Changes {
                files: self.end_preparing(*request, SendOutcome::Stored),
                ..Changes::default()
            },
            UiEvent::Progress(Progress::Import {
                request,
                files_done,
                ..
            }) => Changes {
                files: self.on_import_progress(*request, *files_done),
                ..Changes::default()
            },
            UiEvent::ConversationMerged { from, into } => {
                if self.person_origin == Some(*from) {
                    self.person_origin = Some(*into);
                }
                self.merge_media_chat(*from, *into);
                self.refresh_person();
                // The composer saves the text of the chat it leaves under the old id: it goes
                // to the surviving conversation.
                self.merged.insert(*from, *into);
                self.merges.push((*from, *into));
                if let Some(draft) = self.drafts.remove(from) {
                    self.drafts.entry(*into).or_insert(draft);
                }
                self.request_list();
                Changes {
                    switch_to: (self.open == Some(*from)).then_some(*into),
                    merged: true,
                    ..Changes::default()
                }
            }
            UiEvent::ContactsChanged { account } => {
                self.request_list();
                self.request_detail();
                self.reload_timeline();
                self.refresh_person();
                self.refresh_group();
                if self
                    .safety_shown
                    .as_ref()
                    .is_some_and(|(a, _)| a == account)
                {
                    self.refresh_safety();
                }
                Changes::default()
            }
            // Sends to a person began or ended waiting for their safety number (BQ-3): their
            // open level asks again (it offers Confirm while sends wait) and so does their
            // page (its line says the number changed while they wait, U7). The rows come as
            // `MessagesChanged`.
            UiEvent::SafetyWaitChanged { account, user } => {
                if self
                    .safety_shown
                    .as_ref()
                    .is_some_and(|(a, u)| a == account && u == user)
                {
                    self.refresh_safety();
                }
                if self
                    .person_shown
                    .as_ref()
                    .is_some_and(|(a, u)| a == account && u == user)
                {
                    self.refresh_person();
                }
                Changes::default()
            }
            UiEvent::Typing {
                conv,
                user,
                on,
                ttl,
                ..
            } => {
                let until = self.now_ms()
                    + i64::try_from(ttl.as_millis())
                        .ok()
                        .filter(|t| *t > 0)
                        .unwrap_or(DEFAULT_TYPING_TTL_MS);
                let list = self.typing.entry(*conv).or_default();
                list.retain(|(u, _)| u != user);
                if *on {
                    list.push((user.clone(), until));
                }
                Changes {
                    conversations: true,
                    header: self.open == Some(*conv),
                    ..Changes::default()
                }
            }
            UiEvent::ActionFailed {
                action,
                error,
                subject,
                ..
            } => self.on_action_failed(action, error, subject),
            UiEvent::QueryResult { token, result } => self.on_query_result(*token, result),
            UiEvent::Progress(Progress::Transmit {
                message,
                progress,
                phase,
            }) => {
                let mut ch = self.on_send_phase(*message, *phase);
                ch.merge(self.on_uploads_over(*message, *progress, *phase));
                ch
            }
            UiEvent::Progress(Progress::Upload {
                message,
                attachment,
                done,
                total,
            }) => self.on_upload_progress(*message, *attachment, *done, *total),
            UiEvent::Progress(Progress::Download {
                attachment,
                done,
                total,
            }) => self.on_download_progress(*attachment, *done, *total),
            UiEvent::MediaOpened { attachment, path } => {
                self.media_answers
                    .push(MediaAnswer::Opened(*attachment, path.clone()));
                Changes {
                    media: true,
                    ..Changes::default()
                }
            }
            UiEvent::HistoryTransfer { account, state } => {
                self.on_history_transfer(*account, *state)
            }
            UiEvent::AccountProgress { account, progress } => {
                let changed = self.transfers.on_progress(*account, *progress);
                Changes {
                    // The transfer's banner counts the messages, the account's banner the
                    // profiles and groups.
                    transfer: changed,
                    accounts: changed,
                    ..Changes::default()
                }
            }
            UiEvent::HistoryProgress {
                account,
                conv,
                progress,
                has_more,
            } => {
                let changed = if *has_more {
                    self.filling.insert(*conv, *account).is_none()
                } else {
                    self.filling.remove(conv).is_some()
                };
                // A network that pushes its history shows a banner while any chat of the
                // account is still filling.
                let banner = self.track_fill(*account, *progress);
                Changes {
                    header: changed && self.open == Some(*conv),
                    accounts: banner,
                    transfer: banner,
                    ..Changes::default()
                }
            }
            UiEvent::CancelTooLate { conv, message, .. } => {
                self.on_cancel_too_late(*conv, *message)
            }
            // Part of a pushed history never arrived: the quiet line of the account's banner
            // (a count of 0 says nothing; the line is the same for one chunk and for many).
            UiEvent::Notice {
                account,
                notice: BackendNotice::HistoryLost { count },
            } => Changes {
                accounts: *count > 0 && self.lost_history.insert(*account),
                ..Changes::default()
            },
            // F3b: a finished save is told to the UI as the answer to its own request (the
            // notice "Saved ...").
            UiEvent::AttachmentSaved { attachment, .. } => self.on_attachment_saved(*attachment),
            UiEvent::Notify(_) | UiEvent::CatchUpSummary { .. } | UiEvent::Notice { .. } => {
                Changes::default()
            }
        }
    }

    fn reload(&mut self) -> Changes {
        // After a lagged channel an `AccountAdded` may be lost: do not wait for it forever.
        self.adds.clear();
        if let Some(r) = self.replacing.take() {
            self.replace_news.push(ReplaceNews::Ended { old: r.old });
        }
        // The same for the answer to a send of files: without it the composer would say for
        // ever that those files are being prepared. Whether the core is done with them is
        // not known, so the composer keeps what it made for them.
        let files = !self.preparing.is_empty();
        let lost: Vec<RequestId> = self.preparing.keys().copied().collect();
        for request in lost {
            self.end_preparing(request, SendOutcome::Unknown);
        }
        // And a lost `finished` of the work after a link, which is sent once: the numbers are
        // forgotten (the next changed report brings them back) or the update's banner and the
        // question on quitting would stay up for the rest of the session.
        let progress = self.transfers.forget_all_progress();
        self.window.failed = false;
        self.request_accounts();
        self.request_list();
        self.request_detail();
        self.reload_timeline();
        Changes {
            files,
            transfer: progress,
            accounts: progress,
            ..Changes::default()
        }
    }

    fn open_conversation(&mut self, id: Option<ConversationId>) -> Changes {
        if id == self.open {
            return Changes::default();
        }
        let mut ch = self.close();
        let Some(id) = id else { return ch };
        self.open = Some(id);
        // The first frame already has the chat's title and composer; the answer corrects them.
        self.detail = self.seed_detail(id);
        self.detail_seeded = self.detail.is_some();
        self.request_detail();
        self.pump_timeline();
        self.send_presence();
        ch.timeline = true;
        ch.header = true;
        ch
    }

    fn read_more(&mut self, message: MessageId) -> Changes {
        if self.open.is_none() || !self.window.rows.iter().any(|r| r.id == message) {
            return Changes::default();
        }
        self.read_more.raise(message);
        Changes {
            timeline: true,
            ..Changes::default()
        }
    }

    fn fetch_older(&mut self) -> bool {
        if self.open.is_none() || !self.window.loaded || !self.window.has_older {
            return false;
        }
        // A page in flight already answers this request.
        if !matches!(self.fetching, Some((_, Fetch::Older))) {
            self.want_older = true;
        }
        self.pump_timeline();
        true
    }

    fn fetch_newer(&mut self) -> bool {
        if self.open.is_none() || !self.window.loaded || !self.window.has_newer {
            return false;
        }
        if !matches!(self.fetching, Some((_, Fetch::Newer))) {
            self.want_newer = true;
        }
        self.pump_timeline();
        true
    }

    fn paging(&self) -> Paging {
        CoreSource::paging(self)
    }

    fn login_view(&self) -> Option<LoginView> {
        self.login.as_ref().map(|l| l.view.clone())
    }

    fn login_network(&self) -> Option<NetworkKind> {
        self.login.as_ref().map(|l| l.network)
    }

    fn submit_login(&mut self, values: Vec<(String, String)>) -> Result<(), SourceError> {
        let Some(login) = self.login.as_mut() else {
            return Err(SourceError::Unavailable);
        };
        let (Some(account), LoginViewKind::Input) = (login.account, login.view.kind) else {
            return Err(SourceError::Unavailable);
        };
        let txn = login.view.txn;
        login.view.kind = LoginViewKind::Waiting;
        let values: Vec<(String, Redacted<String>)> = values
            .into_iter()
            .map(|(k, v)| (k, Redacted::new(v)))
            .collect();
        // What was typed comes back into a step of the same id (a wrong phone number or code
        // is corrected, not typed again), but never a secret: a wrong password is not put back
        // into the field of its retry, and a value that is no field of the step (the request
        // for a new code) is nothing to put back.
        let kept: Vec<(String, Redacted<String>)> = values
            .iter()
            .filter(|(id, _)| {
                login
                    .view
                    .fields
                    .iter()
                    .any(|f| f.id == *id && f.kind != "secret")
            })
            .cloned()
            .collect();
        login.typed = Some(Typed {
            step_id: login.view.step_id.clone(),
            values: kept,
        });
        if self.send(UiCommand::SubmitLoginInput {
            account,
            txn,
            values,
        }) {
            Ok(())
        } else {
            Err(SourceError::Unavailable)
        }
    }

    fn abandon_question(&self) -> Option<AbandonQuestion> {
        self.abandoned_network().map(abandon_question)
    }

    fn quit_question(&self) -> Option<AbandonQuestion> {
        // A pushed history first: it is the only work that quitting can lose (the phone sends
        // it once), so its words must not be hidden behind those of an import that goes on at
        // the next start. It is the banner's own condition (`fill_view`): a batch came in the
        // last five minutes and a chat still has more; so nothing is asked in the seconds
        // after a link before the first batch, after the push ended, or for a down account.
        let work = if self.visible_accounts().any(|a| self.fill_view(a).is_some()) {
            QuitWork::History
        } else if self.visible_accounts().any(|a| self.transfer_runs(a)) {
            QuitWork::Import
        } else if self
            .visible_accounts()
            .any(|a| !self.logged_out(a.id) && self.transfers.updating(a.id).is_some())
        {
            QuitWork::Update
        } else {
            return None;
        };
        Some(quit_question(work))
    }

    fn cancel_login(&mut self) {
        let Some(login) = self.login.take() else {
            return;
        };
        if matches!(login.view.kind, LoginViewKind::Done | LoginViewKind::Failed) {
            return;
        }
        match login.account {
            // A fresh link whose history transfer runs: the account is linked (a backend
            // reports a transfer only after the link), and the core may not have taken it
            // yet, so a `CancelLogin` now could still stop a login that has already made
            // a device on the phone. Leaving the page leaves the transfer to the account
            // banner. Not for "Link Again" (it has no transfer, and its cancel must reach
            // the backend), and not once the transfer is over.
            Some(account) if login.relink.is_none() && self.transfers.running(account) => {}
            Some(account) => {
                self.send(UiCommand::CancelLogin { account });
                // The announced account of a replace: the replace ends (the front-end reads
                // the accounts after a cancel).
                self.end_announced(account);
            }
            // Not announced yet (this login's `AddAccount` is the newest one waiting): cancel
            // the account as soon as the core names it.
            None => {
                if let Some(cancel) = self.adds.back_mut() {
                    *cancel = true;
                }
            }
        }
    }

    fn set_presence(&mut self, focused: bool, chat_visible: bool) -> Changes {
        self.focused = focused;
        self.chat_visible = chat_visible;
        self.send_presence();
        self.maybe_mark_read();
        Changes::default()
    }

    fn save_draft(&mut self, id: ConversationId, text: &str) {
        let id = self.survivor(id);
        let unchanged = match self.drafts.get(&id) {
            Some(had) => had == text,
            None => text.trim().is_empty(),
        };
        if unchanged {
            return;
        }
        let draft = crate::composer::outgoing_text(text);
        if draft.is_some() {
            self.drafts.insert(id, text.to_owned());
        } else {
            self.drafts.remove(&id);
        }
        self.send(UiCommand::SaveDraft { conv: id, draft });
    }

    fn draft(&self, id: ConversationId) -> String {
        self.drafts
            .get(&self.survivor(id))
            .cloned()
            .unwrap_or_default()
    }

    fn set_typing(&mut self, id: ConversationId, on: bool) {
        let now = self.now_ms();
        match (on, self.typing_sent) {
            (true, Some((c, at))) if c == id && now - at < TYPING_REFRESH_MS => {}
            (true, prev) => {
                if let Some((c, _)) = prev.filter(|(c, _)| *c != id) {
                    self.send(UiCommand::SetTyping { conv: c, on: false });
                }
                self.typing_sent = Some((id, now));
                self.send(UiCommand::SetTyping { conv: id, on: true });
            }
            (false, Some((c, _))) => {
                self.typing_sent = None;
                self.send(UiCommand::SetTyping { conv: c, on: false });
            }
            (false, None) => {}
        }
    }

    fn notice(&self) -> String {
        self.notice.clone()
    }

    fn notice_message(&self) -> Option<MessageId> {
        // Only while the note itself is what the notice says (another notice replaced it).
        self.too_late
            .as_ref()
            .filter(|(_, text)| *text == self.notice)
            .map(|(message, _)| *message)
    }

    fn clear_notice(&mut self) {
        self.notice.clear();
    }

    fn tick(&mut self) -> Changes {
        let now = self.now_ms();
        let mut ch = Changes::default();
        for (conv, list) in &mut self.typing {
            let before = list.len();
            list.retain(|(_, until)| *until > now);
            if list.len() != before {
                ch.conversations = true;
                ch.header |= self.open == Some(*conv);
            }
        }
        self.typing.retain(|_, l| !l.is_empty());
        // Avatars that were being prepared when their rows were built.
        if self.avatars_waiting.get() && !self.avatars.as_deref().is_some_and(|urls| urls.pending())
        {
            self.avatars_waiting.set(false);
            ch.conversations = true;
            ch.header = true;
        }
        // A banner that is written for a time (a ban's end, a hold's end) changes when the
        // time comes: the core sends no event for it.
        let before = self.last_tick.replace(now);
        if let Some(before) = before
            && self.banner_instants().any(|t| t > before && t <= now)
        {
            ch.accounts = true;
        }
        // The open chat's menu words its mute against now ("Muted until tomorrow at 08:00"),
        // and a mute that ends offers Mute again: the core sends no event for either, so the
        // header (which carries the offer) is raised when the end passed between two ticks or
        // the local date changed under a muted chat (M2).
        if let Some(before) = before
            && let Some(end) = self.open_mute_end()
        {
            let time = TimeContext::new(now, self.calendar.as_ref());
            if (end > before && end <= now) || time.day_number(before) != time.day_number(now) {
                ch.header = true;
            }
        }
        // A pushed history nobody has heard of for a while is over for the person: the
        // banner and the hold on Clear and Delete let go.
        let idle: Vec<AccountId> = self
            .fills
            .iter()
            .filter(|(_, f)| now.saturating_sub(f.last_batch) >= HISTORY_FILL_IDLE_MS)
            .map(|(a, _)| *a)
            .collect();
        for account in idle {
            let open_here = self
                .open
                .is_some_and(|c| self.filling.get(&c) == Some(&account));
            if self.end_fills(account) {
                ch.accounts = true;
                ch.transfer = true;
                ch.header |= open_here;
            }
        }
        // The line under a message on its way counts the seconds once it is shown: those
        // rows only, never the whole timeline (`UiSource::wait_lines`).
        let shown_from = now.saturating_sub(timeline::WAIT_SHOWN_AFTER_MS);
        if self.open.is_some()
            && self
                .window
                .rows
                .iter()
                .any(|r| r.outbox.is_some() && r.sent_at <= shown_from)
        {
            ch.waits = true;
        }
        ch
    }

    fn has_timers(&self) -> bool {
        let now = self.now_ms();
        !self.typing.is_empty()
            || self.avatars_waiting.get()
            || self.sends_waiting()
            || !self.fills.is_empty()
            || self.banner_instants().any(|t| t > now)
            || self.open_mute_end().is_some_and(|t| t > now)
    }

    fn unread_total(&self) -> u32 {
        let now = (self.now)();
        self.list
            .iter()
            // (the archive's unread messages count nowhere; they still notify by the mute rules)
            .filter(|c| !archived_row(c) && !muted(c, now))
            .map(|c| c.unread.max(u32::from(c.marked_unread)))
            .fold(0, u32::saturating_add)
    }
}

mod media_slots;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod action_tests;

#[cfg(test)]
mod save_tests;

#[cfg(test)]
mod files_tests;

#[cfg(test)]
mod transfer_tests;

#[cfg(test)]
mod network_tests;

#[cfg(test)]
mod upload_tests;

#[cfg(test)]
mod person_tests;

#[cfg(test)]
mod group_tests;

#[cfg(test)]
mod safety_tests;

#[cfg(test)]
mod media_tests;

#[cfg(test)]
mod choice_tests;

#[cfg(test)]
mod rows_tests;

#[cfg(test)]
mod telegram_tests;

#[cfg(test)]
mod roaming_tests;
