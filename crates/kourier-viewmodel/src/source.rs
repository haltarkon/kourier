//! Where the UI models get their rows: a small trait the Qt layer depends on, implemented by
//! the core facade ([`crate::CoreSource`], the app), the fixture data set ([`FixtureSource`],
//! pure UI tests) and an empty source.
//!
//! The getters build rows from what the source holds now. Events and commands return
//! [`Changes`]: which models to refresh. The fixture answers synchronously; the core answers
//! through `UiEvent::QueryResult` (§8.5), which the Qt layer feeds to [`UiSource::on_event`].

use std::path::PathBuf;
use std::time::Instant;

use kourier_core::facade::UiEvent;
use kourier_model::{AccountId, AttachmentId, ConversationId, MessageId, NetworkKind};

use kourier_core::facade::FileRefusal;

use crate::conversations::{AccountRow, Banner, ChatHeader, ConversationFilter, ConversationRow};
use crate::core_source::{Changes, Paging};
use crate::files::{ArchiveRule, PlanAnswer, PlanRequest, StripRules};
use crate::fixture::Fixture;
use crate::login::{LoginView, NetworkChoice, network_choices};
use crate::richtext::RichText;
use crate::time::{Calendar, TimeContext};
use crate::timeline::{self, MessageRow, QuoteBlock, RowKind};
use crate::transfer::{ReplaceQuestion, TransferView};

/// Why a source refused a command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceError {
    /// The conversation does not exist (any more).
    #[error("unknown conversation")]
    UnknownConversation,
    /// The text is empty after normalisation.
    #[error("nothing to send")]
    Empty,
    /// Sending is not possible here; user-facing reason.
    #[error("cannot send: {0}")]
    CannotSend(String),
    /// The source has no backend for this.
    #[error("not available")]
    Unavailable,
    /// The built-in fixture data could not be built (a bug).
    #[error("invalid fixture data")]
    InvalidFixture,
}

/// The answer to one [`UiSource::open_media`].
#[derive(Clone, PartialEq, Eq)]
pub enum MediaAnswer {
    /// The plaintext copy in the tmpfs media cache (personal data: never log the path).
    Opened(AttachmentId, PathBuf),
    /// An open failed, for the attachment the core names (`ActionSubject::Attachment`).
    /// `None` only if a failure ever comes without one: it then belongs to the oldest
    /// [`UiSource::open_media`] without an answer (the core answers opens in order).
    Failed(Option<AttachmentId>),
}

impl std::fmt::Debug for MediaAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opened(id, _) => f.debug_tuple("Opened").field(id).finish_non_exhaustive(),
            Self::Failed(id) => f.debug_tuple("Failed").field(id).finish(),
        }
    }
}

/// Files of a `SendFiles` the core did not take, handed back to the composer. Either the
/// whole request (nothing of it was queued: the files, the caption and the quote return), or,
/// when the core named the files it cannot take, only those: the others went out again
/// without them ([`Self::resent`]), so one file the network cannot carry never holds the
/// others back.
#[derive(Clone, PartialEq, Eq)]
pub struct RefusedSend {
    /// The conversation it was for.
    pub conv: ConversationId,
    /// The request the core refused ([`UiSource::send_files`] returned it).
    pub request: u64,
    /// The files that come back.
    pub files: Vec<ReturnedFile>,
    /// The caption as it was typed; empty when it went out with the other files.
    pub caption: String,
    /// The quoted message; `None` when the quote went out with the other files.
    pub reply_to: Option<MessageId>,
    /// How many files of the request were sent again without the refused ones.
    pub resent: usize,
    /// The request those files went out again with (`Some` exactly when `resent > 0`): the
    /// composer keeps looking after their source files under this id.
    pub resent_request: Option<u64>,
}

/// One file of a refused send on its way back to the composer.
#[derive(Clone, PartialEq, Eq)]
pub struct ReturnedFile {
    /// The file (personal data: never logged).
    pub path: PathBuf,
    /// The user had chosen to send it as the file it is.
    pub as_file: bool,
    /// Why the network cannot take it; `None`: the file could go, the request was refused
    /// for another reason.
    pub reason: Option<FileRefusal>,
}

crate::safe_debug!(ReturnedFile { as_file, reason });

impl RefusedSend {
    /// The files that come back.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.files.iter().map(|f| &f.path)
    }
}

impl std::fmt::Debug for RefusedSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefusedSend")
            .field("conv", &self.conv)
            .field("request", &self.request)
            .field("files", &self.files.len())
            .field("reply_to", &self.reply_to)
            .field("resent", &self.resent)
            .finish_non_exhaustive()
    }
}

/// How a [`UiSource::send_files`] request ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// Its messages are stored and queued: the core has its own copies of the files.
    Stored,
    /// The core did not take it; what comes back is in [`UiSource::take_refused_files`].
    Refused,
    /// Events were lost (a lagged channel): the request may still be copied, so files made
    /// for it must stay where they are.
    Unknown,
}

/// A [`UiSource::send_files`] request that is over ([`UiSource::take_finished_sends`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinishedSend {
    /// The request.
    pub request: u64,
    /// The conversation it was for.
    pub conv: ConversationId,
    /// How it ended.
    pub outcome: SendOutcome,
}

/// One version of a message in its edit history.
#[derive(Clone, PartialEq)]
pub struct RevisionRow {
    /// The text of this version; `None`: a message with files that had no caption then.
    pub text: Option<RichText>,
    /// When it was written ("Today 14:02", "12 Mar 09:15").
    pub when: String,
    /// The message as first sent.
    pub original: bool,
    /// The version the timeline shows.
    pub current: bool,
}

crate::safe_debug!(RevisionRow { original, current });

/// The versions of a message, newest first ([`UiSource::take_revisions`]).
#[derive(Clone, PartialEq)]
pub struct RevisionList {
    /// The message.
    pub message: MessageId,
    /// Its versions, newest first; empty when the message is gone or was deleted.
    pub rows: Vec<RevisionRow>,
}

crate::safe_debug!(RevisionList { message });

/// What an edit of a message starts from.
#[derive(Clone, PartialEq, Eq)]
pub struct EditTarget {
    /// The message.
    pub message: MessageId,
    /// Its text, or its caption (empty for files without one), as the composer edits it:
    /// plain text, each mention written out as "@Name".
    pub text: String,
    /// It is the caption of a message with files: it may be emptied, which removes it.
    pub caption: bool,
}

crate::safe_debug!(EditTarget { message, caption });

/// Rows and commands for the UI models.
pub trait UiSource {
    /// "Now" and the local calendar the row labels were built with.
    fn time(&self) -> TimeContext<'_>;
    /// Accounts for the drawer.
    fn accounts(&self) -> Vec<AccountRow>;
    /// Conversation-list rows passing `filter`, in list order.
    fn conversations(&self, filter: &ConversationFilter) -> Vec<ConversationRow>;
    /// Header of a conversation.
    fn chat(&self, id: ConversationId) -> Option<ChatHeader>;
    /// Timeline rows of a conversation, newest first.
    fn timeline(&self, id: ConversationId) -> Vec<MessageRow>;
    /// Networks offered on the welcome page.
    fn networks(&self) -> Vec<NetworkChoice> {
        network_choices()
    }
    /// The conversation `--page=chat` opens (the fixture's long chat); `None` for real data.
    fn demo_conversation(&self) -> Option<ConversationId> {
        None
    }
    /// Starts linking an account; returns the first step to show.
    fn start_login(&mut self, network: NetworkKind) -> Result<LoginView, SourceError>;
    /// Sends a text (optionally as a reply); the timeline shows it on the next query. The
    /// fixture returns the new row's id; the core assigns it asynchronously (`None`).
    fn send_text(
        &mut self,
        id: ConversationId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Option<MessageId>, SourceError>;

    /// Sends `paths`, any number of files of any kinds, with the composer text as `caption`
    /// (§5.8), optionally as a reply; `as_file[i]` sends file `i` as the file it is (missing
    /// entries are `false`). The core makes as many messages of them as the network needs to
    /// show every file ([`UiSource::plan_files`] says how many); the rows show them on a
    /// later query, each with its own state. Returns the id of the request: it is named
    /// again when the request ends ([`UiSource::take_finished_sends`],
    /// [`RefusedSend::request`]), until when the files must stay where they are.
    fn send_files(
        &mut self,
        _id: ConversationId,
        _paths: Vec<PathBuf>,
        _as_file: Vec<bool>,
        _caption: &str,
        _reply_to: Option<MessageId>,
    ) -> Result<u64, SourceError> {
        Err(SourceError::Unavailable)
    }
    /// The oldest lot of files a `SendFiles` was refused for that was not taken yet; call
    /// until `None` ([`Changes::files`]): several lots can wait.
    fn take_refused_files(&mut self) -> Option<RefusedSend> {
        None
    }
    /// Files of sends to `conv` the core is still preparing (copying, encrypting): their
    /// messages are not in the timeline yet ([`Changes::files`] when it changes). A send
    /// counts from [`UiSource::send_files`] until the core says its messages are stored or
    /// refuses it, whichever chat is open meanwhile. Only events lost to a lagged channel end
    /// the count without an answer.
    fn preparing_files(&self, _conv: ConversationId) -> usize {
        0
    }
    /// Of [`UiSource::preparing_files`], how many the core has copied already.
    fn prepared_files(&self, _conv: ConversationId) -> usize {
        0
    }
    /// The [`UiSource::send_files`] requests that ended since the last call
    /// ([`Changes::files`]): the composer may now delete the files it made for them.
    fn take_finished_sends(&mut self) -> Vec<FinishedSend> {
        Vec::new()
    }
    /// What the network of `conv` says about files that the composer only passes on to the
    /// user (types some client will not save).
    fn strip_rules(&self, _conv: ConversationId) -> StripRules {
        StripRules::default()
    }
    /// The longest text one message of `conv` carries, in bytes, when the network states
    /// one: a longer caption goes first as several messages, and the plan needs its text.
    fn max_text_bytes(&self, _conv: ConversationId) -> Option<u32> {
        None
    }
    /// What `conv` takes as one plain file: whether a folder can go there as one archive,
    /// and how large that archive may be.
    fn archive_rule(&self, _conv: ConversationId) -> ArchiveRule {
        ArchiveRule::default()
    }
    /// The merges since the last call: `(merged away, surviving)`, oldest first
    /// ([`Changes::merged`]).
    fn take_merges(&mut self) -> Vec<(ConversationId, ConversationId)> {
        Vec::new()
    }
    /// Asks the core for the settings it keeps ([`Self::expiry_policy`]); the answer comes
    /// with [`Changes::settings`]. The settings page asks when it is shown: nothing is read
    /// for it at the start.
    fn load_settings(&mut self) {}
    /// What happens to disappearing messages whose time has come while this computer's
    /// clock is unconfirmed (§9.6), by the core's stable name of the option (`hide`, `keep`
    /// or `erase`); `None` until the core has said
    /// ([`Self::load_settings`]).
    fn expiry_policy(&self) -> Option<&'static str> {
        None
    }
    /// Chooses that option by its name. It holds at once and is stored by the core; if it
    /// cannot be stored, [`Self::expiry_policy`] reads what it was again
    /// ([`Changes::settings`]) and a notice says so. `false`: no such option, or no core.
    fn set_expiry_policy(&mut self, _name: &str) -> bool {
        false
    }
    /// Asks how the composer's files would be sent to `conv`: how many messages, which files
    /// the network cannot take. The answer comes through [`UiSource::take_file_plan`]
    /// ([`Changes::plan`]); a newer request replaces one that is still waiting. `false`:
    /// this source cannot say.
    fn plan_files(&mut self, _conv: ConversationId, _request: PlanRequest) -> bool {
        false
    }
    /// The answer to the last [`UiSource::plan_files`], once.
    fn take_file_plan(&mut self) -> Option<PlanAnswer> {
        None
    }
    /// The serial of a [`UiSource::plan_files`] request the source could not answer, once
    /// ([`Changes::plan`]): the composer stops waiting for that plan.
    fn take_plan_failure(&mut self) -> Option<u64> {
        None
    }
    /// Asks for the versions of an edited message; the answer comes through
    /// [`UiSource::take_revisions`] ([`Changes::revisions`]). `false`: not available.
    fn request_revisions(&mut self, _message: MessageId) -> bool {
        false
    }
    /// The answer to the last [`UiSource::request_revisions`], once. While the history of a
    /// message is shown ([`UiSource::close_revisions`] ends that) a change of the open
    /// conversation asks again by itself, so another answer may follow.
    fn take_revisions(&mut self) -> Option<RevisionList> {
        None
    }
    /// The history asked for with [`UiSource::request_revisions`] is no longer shown.
    fn close_revisions(&mut self) {}

    /// What a reply to `message` quotes, for the composer's banner; `None` when the message
    /// cannot be replied to (a tombstone, a notice, a chat without replies).
    fn reply_target(&self, conv: ConversationId, message: MessageId) -> Option<QuoteBlock> {
        self.timeline(conv)
            .iter()
            .find(|r| r.message_id() == Some(message) && r.actions.reply)
            .and_then(timeline::quote_of_row)
    }
    /// What an edit of `message` starts from; `None` when it cannot be edited now.
    fn edit_target(&self, conv: ConversationId, message: MessageId) -> Option<EditTarget> {
        self.timeline(conv)
            .iter()
            .find(|r| r.message_id() == Some(message) && r.actions.edit)
            .map(|r| {
                let (text, caption) = timeline::edit_text_of_row(r);
                EditTarget {
                    message,
                    text,
                    caption,
                }
            })
    }
    /// The newest own message of `conv` that can be edited now (Up in an empty composer).
    fn last_editable(&self, conv: ConversationId) -> Option<MessageId> {
        self.timeline(conv)
            .iter()
            .filter(|r| r.kind == RowKind::Message && r.own)
            .find(|r| r.actions.edit)
            .and_then(MessageRow::message_id)
    }
    /// Replaces the text (or the caption) of an own message with what the composer holds.
    /// `Ok(false)`: nothing to do, the text is what it was. Formatting and mentions of the
    /// message that the change did not touch are kept ([`crate::edit::EditForm`]).
    fn edit_message(
        &mut self,
        _conv: ConversationId,
        _message: MessageId,
        _text: &str,
    ) -> Result<bool, SourceError> {
        Err(SourceError::Unavailable)
    }
    /// Deletes a message, for everyone or for this account only.
    fn delete_message(&mut self, _message: MessageId, _for_everyone: bool) -> bool {
        false
    }
    /// Adds (or with `remove` takes back) our reaction `emoji` on a message.
    fn react(&mut self, _message: MessageId, _emoji: &str, _remove: bool) -> bool {
        false
    }
    /// Sends a failed own message again.
    fn retry_message(&mut self, _message: MessageId) -> bool {
        false
    }
    /// Takes back an own message nobody has yet.
    fn cancel_message(&mut self, _message: MessageId) -> bool {
        false
    }
    /// Sets the conversation's disappearing-message timer (`0` turns it off): one of the
    /// offered values, or any value of the range the chat states ([`ChatHeader::timer_range`]).
    fn set_timer(&mut self, _conv: ConversationId, _secs: u32) -> bool {
        false
    }
    /// Deletes every message of the conversation for this account; the chat stays in the
    /// list. Other people keep their copies ([`ChatHeader::deletes_sync`] says whether the
    /// account's other devices follow).
    fn clear_conversation(&mut self, _conv: ConversationId) -> bool {
        false
    }
    /// Deletes the conversation for this account: its messages and its draft; it leaves the
    /// list until its next message.
    fn delete_conversation(&mut self, _conv: ConversationId) -> bool {
        false
    }
    /// Marks everything in `conv` read, up to its newest message, because the user asked: any
    /// chat of the list, whether or not it is the open one and whether or not the window has
    /// the focus (the open chat is marked by itself only under those conditions). The core
    /// sends the read receipts and the read sync as it does for the open chat. `false`: no
    /// core.
    fn mark_read(&mut self, _conv: ConversationId) -> bool {
        false
    }
    /// Removes one attachment of a message for this account (the message stays).
    fn delete_attachment(&mut self, _attachment: AttachmentId) -> bool {
        false
    }
    /// Shows `message` of the open conversation: [`Changes::jump`] names it once it is among
    /// the rows (at once when it is loaded, else after the window around it arrived).
    fn jump_to(&mut self, _message: MessageId) -> Changes {
        Changes::default()
    }
    /// Loads the newest messages of the open conversation when the window ends before them.
    fn jump_to_latest(&mut self) -> Changes {
        Changes::default()
    }
    /// When the rows' action flags must be read again: the first edit or delete window of a
    /// loaded message ends then (the core sends no event for time). `None`: nothing ends.
    fn flags_deadline(&self) -> Option<Instant> {
        None
    }
    /// The [`UiSource::flags_deadline`] may have passed: re-reads the open timeline if so.
    fn refresh_flags(&mut self) -> Changes {
        Changes::default()
    }

    /// What the accounts shown by `account` (`None` = all) need the user to know or do: the
    /// most pressing state among them. Never a history transfer: that is
    /// [`Self::transfer_banner`], chosen apart, so that one account's state does not hide
    /// another account's transfer.
    fn banner(&self, _account: Option<AccountId>) -> Option<Banner> {
        None
    }
    /// The history transfer the main window shows for the accounts shown by `account`
    /// (`None` = all): one that runs (after a restart in the middle of it, or once the link
    /// page was left), or one that failed and was not dismissed; a failed one before a
    /// running one, the first account among equals. Its `transfer` is the view. An account
    /// whose own state presses more than its transfer (the order of
    /// [`crate::conversations::BannerKind`]) shows that state in [`Self::banner`] and no
    /// transfer; the link page's own account shows its running transfer on the page only.
    fn transfer_banner(&self, _account: Option<AccountId>) -> Option<Banner> {
        None
    }
    /// "Link again" for a logged-out account: starts its login; returns the first step.
    fn relink(&mut self, _account: AccountId) -> Result<LoginView, SourceError> {
        Err(SourceError::Unavailable)
    }
    /// Restarts an account the supervisor gave up on.
    fn restart_account(&mut self, _account: AccountId) -> bool {
        false
    }
    /// Hands the network what the browser gave back after the user solved the account's
    /// challenge (the solution token or the whole redirect link). A secret: never logged.
    fn submit_challenge(&mut self, _account: AccountId, _answer: &str) -> bool {
        false
    }
    /// The account that waits for a solved challenge, whatever the banner shows (another
    /// account's more pressing state may be in front of it): where an answer that comes back
    /// from the browser by itself goes.
    fn challenged_account(&self) -> Option<AccountId> {
        None
    }

    /// The history transfer the link page shows in place of its "linking" step: the one of
    /// the account being linked, while it runs (waiting for the phone, downloading,
    /// reading). Never another account's, and never an end.
    fn link_transfer(&self) -> Option<TransferView> {
        None
    }
    /// "Continue Without History": ends the wait for the history of `account`, or its
    /// download. `false` when its transfer cannot be skipped now (none runs, or it is being
    /// read). The view stays until the backend reports the end, and that end is not
    /// announced: the user asked for it.
    fn skip_transfer(&mut self, _account: AccountId) -> bool {
        false
    }
    /// "Dismiss" on the banner of a failed history transfer: the banner goes and the
    /// backend forgets the failure. `false` when `account` shows none.
    fn dismiss_transfer(&mut self, _account: AccountId) -> bool {
        false
    }
    /// What the user is told once about transfers that ended ([`Changes::transfer`]): "the
    /// message history was transferred", or that it was not when this UI did not skip it.
    /// Taken once; the dismissal went to the backend when the notice was made.
    fn take_transfer_notices(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// "Transfer History from Phone…" of an account, and "Try Again…" on the banner of a
    /// failed transfer: the question to put before anything happens. `None`: the account
    /// does not exist (any more) or is being removed. Nothing is sent; the source remembers
    /// that the question is open ([`Self::confirm_replace`], [`Self::cancel_replace`]).
    fn ask_replace(&mut self, _account: AccountId) -> Option<ReplaceQuestion> {
        None
    }
    /// The user confirmed the question about `account`: the account is removed from this
    /// computer and a new account of the same network is linked (the first step to show is
    /// returned; the link page opens). It answers the question that [`Self::ask_replace`]
    /// opened last and only that: without an open question, for a second confirmation of
    /// the same one, and for a question about another account than the open one (a dialog
    /// that outlived its question), nothing is removed and the answer is
    /// [`SourceError::Unavailable`].
    fn confirm_replace(&mut self, _account: AccountId) -> Result<LoginView, SourceError> {
        Err(SourceError::Unavailable)
    }
    /// The user closed the question without confirming: nothing happens.
    fn cancel_replace(&mut self) {}

    /// Downloads an attachment the policy left for a click, or retries a failed download.
    fn download_attachment(&mut self, _id: AttachmentId) -> bool {
        false
    }
    /// Asks for a plaintext copy of a downloaded attachment (GIFs, voice notes, "open"); the
    /// answer comes later through [`UiSource::take_media`].
    fn open_media(&mut self, _id: AttachmentId) -> bool {
        false
    }
    /// The view of an opened copy closed (one call per answered open).
    fn close_media(&mut self, _id: AttachmentId) {}
    /// Answers to [`UiSource::open_media`] since the last call, in the order the opens were
    /// asked for.
    fn take_media(&mut self) -> Vec<MediaAnswer> {
        Vec::new()
    }

    /// Whether the first answers arrived (the UI picks its start page then).
    fn loaded(&self) -> bool {
        true
    }
    /// Whether `id` is (still) the open conversation.
    fn is_open(&self, _id: ConversationId) -> bool {
        true
    }
    /// Takes one core event (§8.5).
    fn on_event(&mut self, _event: &UiEvent) -> Changes {
        Changes::default()
    }
    /// Re-reads everything visible (after the event channel lagged).
    fn reload(&mut self) -> Changes {
        Changes::ALL
    }
    /// Opens a conversation in the chat page (`None` closes it).
    fn open_conversation(&mut self, _id: Option<ConversationId>) -> Changes {
        Changes {
            timeline: true,
            header: true,
            ..Changes::default()
        }
    }
    /// "Read more" on a long text of the open conversation: shows one step more of `message`
    /// ([`Changes::timeline`] when its row changed). The shown length is forgotten when
    /// another conversation is opened.
    fn read_more(&mut self, _message: MessageId) -> Changes {
        Changes::default()
    }
    /// Loads the page before the oldest loaded row; `false` when there is none.
    fn fetch_older(&mut self) -> bool {
        false
    }
    /// Loads the page after the newest loaded row; `false` when there is none.
    fn fetch_newer(&mut self) -> bool {
        false
    }
    /// Paging state of the open timeline.
    fn paging(&self) -> Paging {
        Paging::default()
    }
    /// The login step to show, while linking.
    fn login_view(&self) -> Option<LoginView> {
        None
    }
    /// The network of the login in progress (the link page shows its icon and its steps).
    fn login_network(&self) -> Option<NetworkKind> {
        None
    }
    /// Answers an input login step with `(field id, value)` pairs.
    fn submit_login(&mut self, _values: Vec<(String, String)>) -> Result<(), SourceError> {
        Err(SourceError::Unavailable)
    }
    /// Abandons the login.
    fn cancel_login(&mut self) {}
    /// Window focus and whether the chat page is on screen (notifications, read markers).
    fn set_presence(&mut self, _focused: bool, _chat_visible: bool) -> Changes {
        Changes::default()
    }
    /// Stores the composer text of a conversation being left (empty clears it).
    fn save_draft(&mut self, _id: ConversationId, _text: &str) {}
    /// The draft of a conversation (this session).
    fn draft(&self, _id: ConversationId) -> String {
        String::new()
    }
    /// The user typed (`true`) or stopped (`false`) in a conversation's composer.
    fn set_typing(&mut self, _id: ConversationId, _on: bool) {}
    /// A short notice for the composer ("Sending is not available yet"); empty for none.
    fn notice(&self) -> String {
        String::new()
    }
    /// Hides the notice.
    fn clear_notice(&mut self) {}
    /// Expires timed state (typing indicators).
    fn tick(&mut self) -> Changes {
        Changes::default()
    }
    /// Whether [`UiSource::tick`] has work (a timer should run).
    fn has_timers(&self) -> bool {
        false
    }
    /// Unread messages of the conversations that are not muted (the taskbar badge and the
    /// tray's attention state); a conversation marked unread counts as one.
    fn unread_total(&self) -> u32 {
        self.conversations(&ConversationFilter::default())
            .iter()
            .filter(|c| !c.muted)
            .map(|c| c.unread.max(u32::from(c.marked_unread)))
            .fold(0, u32::saturating_add)
    }
}

/// The fixture data set as a source (`--fixture`, tests, screenshots).
pub struct FixtureSource {
    fixture: Fixture,
}

impl FixtureSource {
    /// The standard fixture.
    pub fn new() -> Result<Self, SourceError> {
        Ok(Self {
            fixture: Fixture::build().map_err(|_| SourceError::InvalidFixture)?,
        })
    }

    /// The data behind the source.
    pub fn fixture(&self) -> &Fixture {
        &self.fixture
    }
}

impl UiSource for FixtureSource {
    fn time(&self) -> TimeContext<'_> {
        TimeContext::new(self.fixture.now_ms, &self.fixture.calendar)
    }

    fn accounts(&self) -> Vec<AccountRow> {
        self.fixture.account_rows()
    }

    fn conversations(&self, filter: &ConversationFilter) -> Vec<ConversationRow> {
        filter.apply(self.fixture.conversation_rows(&self.time()))
    }

    fn chat(&self, id: ConversationId) -> Option<ChatHeader> {
        self.fixture.chat_header(id, &self.time())
    }

    fn timeline(&self, id: ConversationId) -> Vec<MessageRow> {
        self.fixture.timeline_rows(id, &self.time())
    }

    fn demo_conversation(&self) -> Option<ConversationId> {
        Some(self.fixture.long_chat)
    }

    fn start_login(&mut self, network: NetworkKind) -> Result<LoginView, SourceError> {
        if network != NetworkKind::Signal {
            return Err(SourceError::Unavailable);
        }
        Ok(crate::login::login_view(&self.fixture.login_qr(), network))
    }

    fn send_text(
        &mut self,
        id: ConversationId,
        text: &str,
        reply_to: Option<MessageId>,
    ) -> Result<Option<MessageId>, SourceError> {
        let text = crate::composer::outgoing_text(text).ok_or(SourceError::Empty)?;
        self.fixture.send_text(id, text, reply_to).map(Some)
    }
}

/// A first-run source: no accounts, nothing to show (until S4 wires the core).
pub struct EmptySource {
    calendar: SystemUtc,
    now_ms: i64,
}

/// UTC calendar used by [`EmptySource`] (it has no rows to label).
struct SystemUtc;

impl Calendar for SystemUtc {
    fn utc_offset_s(&self, _ts_ms: i64) -> i32 {
        0
    }
}

impl EmptySource {
    /// An empty source at `now_ms`.
    pub fn new(now_ms: i64) -> Self {
        Self {
            calendar: SystemUtc,
            now_ms,
        }
    }
}

impl UiSource for EmptySource {
    fn time(&self) -> TimeContext<'_> {
        TimeContext::new(self.now_ms, &self.calendar)
    }

    fn accounts(&self) -> Vec<AccountRow> {
        Vec::new()
    }

    fn conversations(&self, _filter: &ConversationFilter) -> Vec<ConversationRow> {
        Vec::new()
    }

    fn chat(&self, _id: ConversationId) -> Option<ChatHeader> {
        None
    }

    fn timeline(&self, _id: ConversationId) -> Vec<MessageRow> {
        Vec::new()
    }

    fn start_login(&mut self, _network: NetworkKind) -> Result<LoginView, SourceError> {
        Err(SourceError::Unavailable)
    }

    fn send_text(
        &mut self,
        _id: ConversationId,
        _text: &str,
        _reply_to: Option<MessageId>,
    ) -> Result<Option<MessageId>, SourceError> {
        Err(SourceError::UnknownConversation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::{ListOp, RoleSet, diff};
    use crate::timeline::{Receipt, RowKey};

    #[test]
    fn sending_appends_a_pending_row_at_the_bottom() {
        let mut src = FixtureSource::new().expect("fixture");
        let conv = src.fixture().long_chat;
        let before = src.timeline(conv);
        let id = src
            .send_text(conv, "  hello\r\n", None)
            .expect("send")
            .expect("fixture ids");
        let after = src.timeline(conv);
        assert_eq!(after[0].key, RowKey::Message(id));
        assert_eq!(after[0].receipt, Receipt::Pending);
        let ops = diff(
            &before,
            &after,
            |r| r.key,
            |a, b| RoleSet::EMPTY.with_if(0, a != b),
        );
        assert_eq!(ops.first(), Some(&ListOp::Insert { at: 0, n: 1 }));
        assert_eq!(src.send_text(conv, " \n", None), Err(SourceError::Empty));
        assert_eq!(
            src.send_text(ConversationId(999), "x", None),
            Err(SourceError::UnknownConversation)
        );
    }

    #[test]
    fn conversation_list_follows_activity() {
        let mut src = FixtureSource::new().expect("fixture");
        let all = ConversationFilter::default();
        let rows = src.conversations(&all);
        let last = rows.last().expect("rows").id;
        src.send_text(last, "bump", None).expect("send");
        let rows = src.conversations(&all);
        let pinned = rows.iter().take_while(|r| r.pinned()).count();
        assert_eq!(
            rows[pinned].id, last,
            "the bumped chat is first after the pins"
        );
        assert_eq!(rows[pinned].preview, "bump");
        assert_eq!(rows[pinned].preview_author, "You");
        assert_eq!(rows[pinned].preview_source, crate::PreviewSource::Own);
    }
}
