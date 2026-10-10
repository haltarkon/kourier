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
    AccountRow as CoreAccount, ActionSubject, ConversationCursor, ConversationDetail,
    ConversationListQuery, ConversationRow as CoreConversation, CoreError, ExpiryPolicy,
    FilePlanQuery, MessageActions, MessageRevisions, MessageRow as CoreMessage, OriginKind, Query,
    QueryData, QueryToken, QuoteState as CoreQuoteState, Refusal, RefusedFiles, RequestId,
    TimelineAnchor, TimelineCursor, TimelinePage, TimelineQuery, UiCommand, UiEvent,
};
use kourier_model::{
    AccountId, AccountState, AttachmentId, AttachmentKind, AttachmentState, Body, ConnState,
    ConvKind, ConversationId, ErrorKind, HistoryTransfer, LoginStepKind, MessageId, NetworkKind,
    OutgoingState, ReadState, Redacted, Text, UserId,
};

use crate::conversations::{
    AccountRow, AccountStatus, Banner, BannerKind, ChatHeader, ConversationFilter, ConversationRow,
    PreviewSource, account_banner, network_icon, network_name, timer_menu, timer_secs, timer_span,
    typing_label,
};
use crate::edit::EditForm;
use crate::files::{ArchiveRule, PlanAnswer, PlanRequest, StripRules, refused_notice};
use crate::i18n::{trc, trfc, trnc};
use crate::login::{LoginView, LoginViewKind, NetworkChoice, login_view};
use crate::media::{self, MediaVariant};
use crate::names::{NameMap, Names, fallback_name, first_name, single_line};
use crate::preview::{self, Summary};
use crate::readmore::ReadMore;
use crate::source::{
    EditTarget, FinishedSend, MediaAnswer, RefusedSend, ReturnedFile, RevisionList, RevisionRow,
    SendOutcome, SourceError, UiSource,
};
use crate::time::{Calendar, TimeContext};
use crate::timeline::{
    self, Block, Item, MessageRow, QuoteBlock, QuoteState, ReactionChip, Receipt, RowActions,
};
use crate::transfer::{ReplaceQuestion, TransferBook, TransferView, replace_question};

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

/// What a model refresh must pick up after an event or a command.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// The accounts drawer (and whether any account exists).
    pub accounts: bool,
    /// The conversation list.
    pub conversations: bool,
    /// The open timeline's rows.
    pub timeline: bool,
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
    /// Answers to `OpenMedia` arrived ([`UiSource::take_media`]).
    pub media: bool,
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
    /// A setting the core keeps became known or changed ([`UiSource::expiry_policy`]).
    pub settings: bool,
    /// Conversations were merged ([`UiSource::take_merges`]): what waits for a merged-away
    /// chat (the files in its composer) goes on waiting for the one that is left.
    pub merged: bool,
    /// The history transfer of an account changed: what the link page shows of it
    /// ([`UiSource::link_transfer`]), the banner ([`UiSource::banner`]), which accounts
    /// offer a transfer ([`UiSource::accounts`]), or a notice waits
    /// ([`UiSource::take_transfer_notices`]).
    pub transfer: bool,
}

impl Changes {
    /// Everything visible.
    pub const ALL: Self = Self {
        accounts: true,
        conversations: true,
        timeline: true,
        header: true,
        login: true,
        notice: true,
        draft: false,
        switch_to: None,
        media: false,
        jump: None,
        files: false,
        plan: false,
        revisions: false,
        settings: false,
        merged: false,
        transfer: true,
    };

    /// Whether anything changed.
    pub fn any(&self) -> bool {
        self.accounts
            || self.conversations
            || self.timeline
            || self.header
            || self.login
            || self.notice
            || self.draft
            || self.switch_to.is_some()
            || self.media
            || self.jump.is_some()
            || self.files
            || self.plan
            || self.revisions
            || self.settings
            || self.merged
            || self.transfer
    }

    /// Adds `other`.
    pub fn merge(&mut self, other: Changes) {
        self.accounts |= other.accounts;
        self.conversations |= other.conversations;
        self.timeline |= other.timeline;
        self.header |= other.header;
        self.login |= other.login;
        self.notice |= other.notice;
        self.draft |= other.draft;
        self.media |= other.media;
        self.files |= other.files;
        self.plan |= other.plan;
        self.revisions |= other.revisions;
        self.settings |= other.settings;
        self.merged |= other.merged;
        self.transfer |= other.transfer;
        if other.switch_to.is_some() {
            self.switch_to = other.switch_to;
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
    /// Accounts whose login was cancelled before the core announced them: their steps are
    /// ignored.
    cancelled: HashSet<AccountId>,
    /// Conversations merged away this session → the surviving one (§3.3).
    merged: HashMap<ConversationId, ConversationId>,
    /// Merges the composer has not heard of yet: `(merged away, surviving)`, oldest first.
    merges: Vec<(ConversationId, ConversationId)>,
    settings_q: QueryTracker,
    /// What happens to disappearing messages while the clock is unconfirmed (§9.6), as the
    /// core holds it; `None` until its first answer.
    expiry: Option<ExpiryPolicy>,
    focused: bool,
    chat_visible: bool,
    presence_sent: Option<(bool, Option<ConversationId>)>,
    last_marked: Option<(ConversationId, MessageId)>,
    typing_sent: Option<(ConversationId, i64)>,
    drafts: HashMap<ConversationId, String>,
    /// The last text sent (with the message it replied to), given back to the composer if the
    /// core refuses it.
    last_sent: Option<(ConversationId, String, Option<MessageId>)>,
    notice: String,
    stopped: bool,
    /// Resolves avatar files to URLs (none: no avatars, the UI shows initials).
    avatars: Option<Box<dyn AvatarUrls>>,
    /// A row was built while its avatar was still being prepared: rebuild on a tick.
    avatars_waiting: std::cell::Cell<bool>,
    /// Live download progress of the open timeline's attachments (`Progress::Download`).
    downloads: HashMap<AttachmentId, f32>,
    /// Answers to `OpenMedia` not taken yet.
    media_answers: Vec<MediaAnswer>,
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
    /// The history transfers of the accounts, as last reported (`UiEvent::HistoryTransfer`).
    transfers: TransferBook,
    /// What to say once about transfers that ended, until the front-end takes it.
    transfer_notices: Vec<String>,
    /// The account the question "Remove and Link Again?" is open for (the one asked last).
    /// Only a confirmation of that question, naming that account, removes an account, and
    /// it takes the question with it.
    replace_asked: Option<AccountId>,
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
            account_errors: HashSet::new(),
            list_q: QueryTracker::new(MODEL_CONVERSATIONS),
            list: Vec::new(),
            list_partial: Vec::new(),
            list_restarts: 0,
            list_again: false,
            open: None,
            detail_q: QueryTracker::new(MODEL_CONVERSATION),
            detail: None,
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
            cancelled: HashSet::new(),
            merged: HashMap::new(),
            focused: false,
            chat_visible: false,
            presence_sent: None,
            last_marked: None,
            typing_sent: None,
            drafts: HashMap::new(),
            last_sent: None,
            notice: String::new(),
            stopped: false,
            avatars: None,
            avatars_waiting: std::cell::Cell::new(false),
            downloads: HashMap::new(),
            media_answers: Vec::new(),
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
            merges: Vec::new(),
            settings_q: QueryTracker::new(MODEL_SETTINGS),
            expiry: None,
            transfers: TransferBook::default(),
            transfer_notices: Vec::new(),
            replace_asked: None,
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
        let t = self.settings_q.request();
        self.query(t, || Query::ExpiryPolicy);
    }

    fn request_detail(&mut self) {
        let Some(conv) = self.open else { return };
        let t = self.detail_q.request();
        self.query(t, || Query::Conversation(conv));
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
                    }
                    self.conn.retain(|id, _| rows.iter().any(|a| a.id == *id));
                    // An account found logged out (at a start, or when its state event was
                    // lost): the backend deleted a transfer that ran with the login.
                    for a in rows {
                        if matches!(a.state, AccountState::LoggedOut { .. }) {
                            ch.transfer |= self.transfers.logged_out(a.id);
                        }
                    }
                    self.accounts = rows.clone();
                    ch.accounts = true;
                    ch.conversations = true;
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
                            self.list = rows;
                            self.list_restarts = 0;
                            ch.conversations = true;
                            ch.accounts = true;
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
                            self.detail = Some((**d).clone());
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

    fn apply_page(&mut self, page: &TimelinePage, fetch: Fetch) {
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
        self.window = Window::default();
        self.read_more.clear();
        self.unread_from = None;
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
                let sent = if action == "SendText" {
                    self.last_sent.take()
                } else {
                    None
                };
                let quoted = sent.as_ref().and_then(|(_, _, reply)| *reply);
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
                // Nothing is lost: the refused text goes back into the composer.
                if let Some((conv, text, _)) = sent
                    && !self.drafts.contains_key(&conv)
                {
                    self.drafts.insert(conv, text);
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
                self.notice_change(text.into_owned())
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
            "AddAccount" if self.adds.pop_front() == Some(true) => Changes::default(),
            "AddAccount" => self.login_failed(if failure == Failure::Unavailable {
                trc(
                    "@info:status",
                    "Linking is not available in this build yet.",
                )
                .into_owned()
            } else {
                trc("@info:status", "The account could not be created.").into_owned()
            }),
            // The option could not be stored: it is what it was. The core says which that is,
            // and the settings page shows it again.
            "SetExpiryPolicy" => {
                self.settings_q.reset();
                self.request_settings();
                self.notice_change(
                    trc("@info:status", "The setting could not be saved.").into_owned(),
                )
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
        let as_file = |index: usize| refused.as_file.get(index).copied().unwrap_or(false);
        // The files the core did not name go out again, each with the choice made for it.
        let rest: Vec<(std::path::PathBuf, bool)> = paths
            .iter()
            .enumerate()
            .filter(|(i, _)| reason_of(*i).is_none())
            .map(|(i, p)| (p.clone(), as_file(i)))
            .collect();
        let named = paths.len() - rest.len();
        let resent_request = if named > 0 && !rest.is_empty() {
            let (again, flags): (Vec<_>, Vec<_>) = rest.iter().cloned().unzip();
            self.send_files_request(
                conv,
                again,
                flags,
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
                    as_file: as_file(i),
                    reason: reason_of(i),
                })
                .collect(),
            caption: if resent == 0 { caption } else { String::new() },
            reply_to: refused.reply_to.filter(|_| resent == 0),
            resent,
            resent_request: resent_request.map(|r| r.0),
        });
        ch
    }

    /// Live progress of a download shown in the open timeline: the row is rebuilt when the
    /// shown percentage changes.
    fn on_download_progress(&mut self, attachment: AttachmentId, done: u64, total: u64) -> Changes {
        if self.open.is_none() || total == 0 {
            return Changes::default();
        }
        let shown = self
            .window
            .rows
            .iter()
            .flat_map(|r| attachments_of(&r.body))
            .any(|a| a.id == Some(attachment));
        if !shown {
            return Changes::default();
        }
        let p = (done as f64 / total as f64).clamp(0.0, 1.0) as f32;
        let old = self.downloads.insert(attachment, p).unwrap_or(0.0);
        Changes {
            timeline: (p * 100.0).floor() != (old * 100.0).floor(),
            ..Changes::default()
        }
    }

    /// `UiEvent::HistoryTransfer`: where the transfer of `account` stands now. The same
    /// state can come any number of times (at every start and connect, an end until it is
    /// dismissed): an end is announced once, and the backend is told to forget it each time
    /// it reports it, so a dismissal that crossed a report is made up for.
    fn on_history_transfer(&mut self, account: AccountId, state: HistoryTransfer) -> Changes {
        let out = self.transfers.on_state(account, state);
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
        Changes {
            transfer: out.changed || out.notice.is_some(),
            ..Changes::default()
        }
    }

    /// Sends `AddAccount` for `network` and makes its login the one the link page shows.
    fn add_account(&mut self, network: NetworkKind) -> Result<LoginView, SourceError> {
        let view = LoginView {
            kind: LoginViewKind::Waiting,
            title: trfc(
                "@title:window page to link an account, %1 the name of a messenger",
                "Link %1",
                &[network_name(network)],
            ),
            progress: -1.0,
            ..LoginView::default()
        };
        self.login = Some(Login {
            network,
            account: None,
            view: view.clone(),
            relink: None,
        });
        if !self.send(UiCommand::AddAccount {
            network,
            flow: "qr".into(),
        }) {
            return Err(SourceError::Unavailable);
        }
        self.adds.push_back(false);
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
                let unread = self
                    .list
                    .iter()
                    .filter(|c| c.account == a.id && !muted(c, now))
                    .map(|c| c.unread)
                    .sum();
                AccountRow {
                    id: a.id,
                    name: a
                        .display_name
                        .as_deref()
                        .map(single_line)
                        .filter(|n| !n.is_empty())
                        .unwrap_or_else(|| network_name.clone()),
                    network: a.network,
                    network_icon: network_icon(a.network).to_owned(),
                    network_name,
                    status,
                    status_label: status.label(),
                    unread,
                    avatar_url: self.avatar_url(a.id, a.avatar.as_ref()),
                    number: a.number.as_deref().map(single_line).unwrap_or_default(),
                    offers_history: self.offers_history(a),
                }
            })
            .collect()
    }

    /// Whether `a` offers "Transfer History from Phone…": its network hands a newly linked
    /// device the history (the descriptor's `HistorySource::PushedOnLink`), it can be linked
    /// in this build, and no transfer of it runs.
    fn offers_history(&self, a: &CoreAccount) -> bool {
        self.networks
            .iter()
            .any(|n| n.kind == a.network && n.available && n.history_on_link)
            && !self.transfer_runs(a)
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
    fn transfer_of(&self, a: &CoreAccount) -> Option<Banner> {
        let view = self.transfers.view(a.id)?;
        let failed = view.phase == crate::transfer::TransferPhase::Failed;
        if !failed && (!self.transfer_runs(a) || self.linking(a.id)) {
            return None;
        }
        Some(Banner {
            account: a.id,
            kind: if failed {
                BannerKind::TransferFailed
            } else {
                BannerKind::Transfer
            },
            text: view.status.clone(),
            action: String::new(),
            url: String::new(),
            transfer: Some(view),
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

    /// What ONE account shows of itself: its own state or its history transfer, whichever
    /// presses more (the order of `BannerKind`). An account never shows both.
    fn own_banner(&self, a: &CoreAccount, several: bool) -> Option<Banner> {
        let stopped = self.account_errors.contains(&a.id);
        let state = account_banner(a.id, &a.state, self.conn_of(a), stopped);
        let mut banner = match (state, self.transfer_of(a)) {
            (Some(s), Some(t)) if t.kind > s.kind => t,
            (Some(s), _) => s,
            (None, t) => t?,
        };
        // With several accounts the banner says which one it is about.
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
        Some(banner)
    }

    /// The banner of the accounts shown, of one sort: the states of accounts (`transfer`
    /// false), or their history transfers (`transfer` true). The two are chosen apart, so
    /// the state of one account never hides the transfer of another: a failed transfer
    /// stays until its user acts, and "Continue Without History" stays in reach, whatever
    /// another account is going through. Within one sort the most pressing kind wins (a
    /// failed transfer before a running one), the first account among equals.
    fn banner_of(&self, account: Option<AccountId>, transfer: bool) -> Option<Banner> {
        let several = self.visible_accounts().count() > 1;
        self.visible_accounts()
            .filter(|a| account.is_none_or(|id| id == a.id))
            .filter_map(|a| self.own_banner(a, several))
            .filter(|b| b.transfer.is_some() == transfer)
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
            if own && !p.deleted {
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
        let multi = self.visible_accounts().count() > 1;
        let (account_badge, badge_icon) = if multi {
            let label = self
                .accounts
                .iter()
                .find(|a| a.id == c.account)
                .and_then(|a| a.display_name.as_deref())
                .map(single_line)
                .unwrap_or_default();
            (label, network_icon(network).to_owned())
        } else {
            (String::new(), String::new())
        };
        let sent_at = c.preview.as_ref().map_or(0, |p| p.sent_at);
        ConversationRow {
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
            receipt,
            account_badge,
            badge_icon,
        }
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
        } else if is_group {
            trnc(
                "@info:status subtitle of a group chat, %1 how many members",
                "%1 member",
                "%1 members",
                d.members.len() as u64,
            )
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
            is_group,
            is_self: c.is_self,
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
        }
    }

    fn timeline_rows(&self, time: &TimeContext<'_>) -> Vec<MessageRow> {
        let is_group = self
            .detail
            .as_ref()
            .is_some_and(|d| d.row.kind != ConvKind::Direct);
        let rows = &self.window.rows;
        let by_id: HashMap<MessageId, &CoreMessage> = rows.iter().map(|r| (r.id, r)).collect();
        let names = &self.names;
        let items = rows.iter().map(|r| Item {
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
            expiry: r.expiry.as_ref(),
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
        self.author_avatars(&mut rows);
        rows
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

/// The core's flags as the row shows them (`retry` comes from the send state).
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
        retry: false,
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

fn list_query(after: Option<ConversationCursor>) -> Query {
    Query::Conversations(ConversationListQuery {
        limit: CONVERSATION_PAGE,
        after,
        ..ConversationListQuery::default()
    })
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
    if p.deleted {
        let own = p.author.is_none();
        return Summary {
            text: if own {
                trc(
                    "@info:preview stand-in for a deleted message",
                    "You deleted this message",
                )
                .into_owned()
            } else {
                trc(
                    "@info:preview stand-in for a deleted message",
                    "This message was deleted",
                )
                .into_owned()
            },
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

    fn accounts(&self) -> Vec<AccountRow> {
        self.account_rows()
    }

    fn conversations(&self, filter: &ConversationFilter) -> Vec<ConversationRow> {
        let time = self.time();
        filter.apply(self.list.iter().map(|c| self.conversation_row(c, &time)))
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
            .any(|n| n.kind == network && n.available)
        {
            return Err(SourceError::Unavailable);
        }
        // "Try again" after a relink that failed (an expired code, a refused link) links the
        // same account again: its conversations and messages belong to it. A login the user
        // left (`cancel_login`) is forgotten, so "Add account" afterwards adds one.
        if let Some(account) = self
            .login
            .as_ref()
            .filter(|l| l.view.kind == LoginViewKind::Failed)
            .and_then(|l| l.relink)
            && self.account_network(account) == network
            && self.accounts.iter().any(|a| a.id == account)
        {
            return self.relink(account);
        }
        self.cancel_login();
        self.add_account(network)
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
        self.last_sent = Some((id, raw, reply_to));
        self.notice.clear();
        self.answered(id);
        Ok(None)
    }

    fn send_files(
        &mut self,
        id: ConversationId,
        paths: Vec<std::path::PathBuf>,
        as_file: Vec<bool>,
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
        let Some(request) = self.send_files_request(id, paths, as_file, caption_text, reply_to)
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
        self.request_settings();
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
        self.send(UiCommand::SetConversationTimer {
            conv,
            timer: (secs > 0).then(|| Duration::from_secs(u64::from(secs))),
        })
    }

    fn clear_conversation(&mut self, conv: ConversationId) -> bool {
        let conv = self.survivor(conv);
        self.notice.clear();
        self.send(UiCommand::ClearConversation { conv })
    }

    fn delete_conversation(&mut self, conv: ConversationId) -> bool {
        let conv = self.survivor(conv);
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

    fn refresh_flags(&mut self) -> Changes {
        if self.flags_deadline.is_some_and(|at| at <= Instant::now()) {
            // Read once; the answer sets the next deadline.
            self.flags_deadline = None;
            self.reload_timeline();
        }
        Changes::default()
    }

    fn banner(&self, account: Option<AccountId>) -> Option<Banner> {
        self.banner_of(account, false)
    }

    fn transfer_banner(&self, account: Option<AccountId>) -> Option<Banner> {
        self.banner_of(account, true)
    }

    fn relink(&mut self, account: AccountId) -> Result<LoginView, SourceError> {
        let network = self.account_network(account);
        self.cancel_login();
        let view = LoginView {
            kind: LoginViewKind::Waiting,
            title: trfc(
                "@title:window page to link an account, %1 the name of a messenger",
                "Link %1",
                &[network_name(network)],
            ),
            progress: -1.0,
            ..LoginView::default()
        };
        if !self.send(UiCommand::Relink {
            account,
            flow: "qr".into(),
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
        });
        Ok(view)
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

    fn link_transfer(&self) -> Option<TransferView> {
        let login = self.login.as_ref()?;
        // "Link Again" of an account that kept its chats transfers nothing: its page shows
        // the steps of that login, whatever was last heard of a transfer of the account.
        if login.relink.is_some() {
            return None;
        }
        let account = login.account?;
        if !self.linking(account) {
            return None;
        }
        self.transfers
            .view(account)
            .filter(|v| v.phase != crate::transfer::TransferPhase::Failed)
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
        // Which account it is: its name and, where the network has one, its number.
        let number = a.number.as_deref().map(single_line).unwrap_or_default();
        let about = crate::transfer::account_line(
            &Self::account_label(a),
            &number,
            network_name(a.network),
        );
        self.replace_asked = Some(account);
        Some(replace_question(account, about))
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
        // Nothing is removed that could not be linked again in this build.
        if !self
            .networks
            .iter()
            .any(|n| n.kind == network && n.available)
        {
            return Err(SourceError::Unavailable);
        }
        self.cancel_login();
        if !self.send(UiCommand::RemoveAccount { account }) {
            return Err(SourceError::Unavailable);
        }
        self.add_account(network)
    }

    fn cancel_replace(&mut self) {
        self.replace_asked = None;
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

    fn loaded(&self) -> bool {
        self.accounts_q.applied() > 0 && self.list_q.applied() > 0
    }

    fn is_open(&self, id: ConversationId) -> bool {
        self.open == Some(id)
    }

    fn on_event(&mut self, event: &UiEvent) -> Changes {
        match event {
            UiEvent::AccountsChanged => {
                self.request_accounts();
                self.request_list();
                // An account linked or logged out: what its messages offer changed with it.
                self.request_detail();
                self.reload_timeline();
                Changes::default()
            }
            UiEvent::AccountAdded { account } => {
                if self.adds.pop_front() == Some(true) {
                    // The user cancelled this login before the core announced the account.
                    self.cancelled.insert(*account);
                    self.send(UiCommand::CancelLogin { account: *account });
                } else if let Some(login) = self.login.as_mut()
                    && login.account.is_none()
                {
                    login.account = Some(*account);
                }
                self.request_accounts();
                Changes::default()
            }
            UiEvent::AccountRemoved { account } => {
                self.account_errors.remove(account);
                self.cancelled.remove(account);
                // Nothing of its history transfer is shown any more, and a question about
                // it has lost its subject.
                let transfer = self.transfers.remove(*account);
                if self.replace_asked == Some(*account) {
                    self.replace_asked = None;
                }
                self.request_accounts();
                self.request_list();
                Changes {
                    transfer,
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
                // The backend deleted a transfer that ran with the login, and says nothing
                // about it.
                let transfer = matches!(state, ConnState::LoggedOut { .. })
                    && self.transfers.logged_out(*account);
                Changes {
                    accounts: true,
                    // The composer of a chat of this account says when it cannot send.
                    header: self
                        .detail
                        .as_ref()
                        .is_some_and(|d| d.row.account == *account),
                    transfer,
                    ..Changes::default()
                }
            }
            UiEvent::LoginStep { account, step } => {
                let Some(login) = self.login.as_ref() else {
                    return Changes::default();
                };
                let ours = match login.account {
                    Some(a) => a == *account,
                    // The core announces an account before its first step, so while ours is
                    // not announced, a step belongs to another (cancelled) login. Without a
                    // pending `AddAccount` (lost to a lagged channel) the step is taken.
                    None => self.adds.is_empty() && !self.cancelled.contains(account),
                };
                if !ours {
                    return Changes::default();
                }
                let view = self.device_name_default(login_view(step, login.network));
                let done = matches!(step.kind, LoginStepKind::Done);
                let failed = matches!(step.kind, LoginStepKind::Failed { .. });
                if let Some(login) = self.login.as_mut() {
                    login.account = if failed { None } else { Some(*account) };
                    login.view = view;
                }
                if done {
                    self.request_accounts();
                    self.request_list();
                }
                Changes {
                    login: true,
                    ..Changes::default()
                }
            }
            UiEvent::ConversationsChanged { ids } => {
                self.request_list();
                if self.open.is_some_and(|o| ids.contains(&o)) {
                    self.request_detail();
                }
                Changes::default()
            }
            UiEvent::ConversationsRemoved { ids } => {
                self.request_list();
                if self.open.is_some_and(|o| ids.contains(&o)) {
                    return self.close();
                }
                Changes::default()
            }
            UiEvent::MessagesChanged { conv, .. } | UiEvent::TimelineInvalidated { conv } => {
                if self.open == Some(*conv) {
                    self.reload_timeline();
                    self.refresh_revisions();
                }
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
            UiEvent::ContactsChanged { .. } => {
                self.request_list();
                self.request_detail();
                self.reload_timeline();
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
            UiEvent::Notify(_)
            | UiEvent::CatchUpSummary { .. }
            | UiEvent::HistoryProgress { .. }
            | UiEvent::Progress(_)
            | UiEvent::Notice { .. } => Changes::default(),
        }
    }

    fn reload(&mut self) -> Changes {
        // After a lagged channel an `AccountAdded` may be lost: do not wait for it forever.
        self.adds.clear();
        // The same for the answer to a send of files: without it the composer would say for
        // ever that those files are being prepared. Whether the core is done with them is
        // not known, so the composer keeps what it made for them.
        let files = !self.preparing.is_empty();
        let lost: Vec<RequestId> = self.preparing.keys().copied().collect();
        for request in lost {
            self.end_preparing(request, SendOutcome::Unknown);
        }
        self.window.failed = false;
        self.request_accounts();
        self.request_list();
        self.request_detail();
        self.reload_timeline();
        Changes {
            files,
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
        let values = values
            .into_iter()
            .map(|(k, v)| (k, Redacted::new(v)))
            .collect();
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
        ch
    }

    fn has_timers(&self) -> bool {
        !self.typing.is_empty() || self.avatars_waiting.get()
    }

    fn unread_total(&self) -> u32 {
        let now = (self.now)();
        self.list
            .iter()
            .filter(|c| !muted(c, now))
            .map(|c| c.unread.max(u32::from(c.marked_unread)))
            .fold(0, u32::saturating_add)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod action_tests;

#[cfg(test)]
mod files_tests;

#[cfg(test)]
mod transfer_tests;
