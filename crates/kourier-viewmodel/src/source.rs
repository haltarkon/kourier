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
use kourier_model::{AccountId, AttachmentId, ConversationId, MessageId, NetworkKind, UserId};

use kourier_core::facade::FileRefusal;

use crate::account_actions::{AccountAction, LogoutNotice};
use crate::conversations::{
    AccountRow, Banner, Banners, ChatHeader, ConversationFilter, ConversationRow,
};
use crate::core_source::{Changes, Paging};
use crate::files::{ArchiveRule, PlanAnswer, PlanRequest, StripRules};
use crate::fixture::Fixture;
use crate::login::{LoginView, NetworkChoice, NetworkEntry, network_choices};
use crate::person::{CopyPart, CopyTarget, CopyUpdate};
use crate::richtext::RichText;
use crate::roaming::{ChatOffer, ChatTold};
use crate::time::{Calendar, TimeContext};
use crate::timeline::{self, MessageRow, QuoteBlock, RowKind, WaitLine};
use crate::transfer::{AbandonQuestion, ReplaceQuestion};

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

/// Why a save did not write a file (F3b): the class the notice names, never a path or a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveFailure {
    /// The disk is full.
    NoSpace,
    /// The folder does not let this user write.
    NotWritable,
    /// The folder is gone.
    NoFolder,
    /// The message is gone, deleted or no longer savable.
    Gone,
    /// The file is not on this PC (any more).
    NotHere,
    /// The file could not be read or written for another reason.
    Other,
}

/// What a save call answers, taken with [`UiSource::take_saves`].
#[derive(Clone, PartialEq, Eq)]
pub enum SaveAnswer {
    /// The name `attachment` is offered under in the dialog ([`UiSource::ask_save`]).
    Suggested {
        /// The attachment.
        attachment: AttachmentId,
        /// The cleaned name (personal data: never log it).
        name: String,
    },
    /// A save ([`UiSource::save_to`]) or a batch ([`UiSource::save_all`]) ended.
    Done(SaveOutcome),
}

/// How a save or a batch of saves ended.
#[derive(Clone, PartialEq, Eq)]
pub struct SaveOutcome {
    /// Files written.
    pub saved: u32,
    /// Files that were not written.
    pub failed: u32,
    /// The name of the last file written (the notice names it when `saved == 1`); empty when none.
    pub name: String,
    /// The folder they went into (personal data: never log it).
    pub dir: PathBuf,
    /// The class of the last failure, when `failed > 0`.
    pub failure: Option<SaveFailure>,
}

impl std::fmt::Debug for SaveAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Suggested { attachment, .. } => f
                .debug_struct("Suggested")
                .field("attachment", attachment)
                .finish_non_exhaustive(),
            Self::Done(o) => f
                .debug_struct("Done")
                .field("saved", &o.saved)
                .field("failed", &o.failed)
                .field("failure", &o.failure)
                .finish_non_exhaustive(),
        }
    }
}

/// Files of a `SendFiles` the core did not take, handed back to the composer. Either the
/// whole request (nothing of it was queued: the files and the quote return, and the caption
/// comes back as the chat's draft, [`UiSource::draft`]), or, when the core named the files it
/// cannot take, only those: the others went out again without them ([`Self::resent`]), so
/// one file the network cannot carry never holds the others back.
#[derive(Clone, PartialEq, Eq)]
pub struct RefusedSend {
    /// The conversation it was for.
    pub conv: ConversationId,
    /// The request the core refused ([`UiSource::send_files`] returned it).
    pub request: u64,
    /// The files that come back.
    pub files: Vec<ReturnedFile>,
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

/// What the details panel is told about the person it shows ([`UiSource::take_person`]).
#[derive(Clone, PartialEq)]
pub enum PersonUpdate {
    /// The page, new or fresher than the one on screen.
    Show(Box<crate::person::PersonView>),
    /// There is nothing to show any more: the chat or the account it belongs to is gone, the
    /// account cannot be used, or the core no longer knows the account. The panel closes.
    Ended,
}

impl std::fmt::Debug for PersonUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Show(view) => write!(f, "PersonUpdate::Show({view:?})"),
            Self::Ended => f.write_str("PersonUpdate::Ended"),
        }
    }
}

/// What the details panel is told about the group it shows ([`UiSource::take_group`]).
#[derive(Clone, PartialEq)]
pub enum GroupUpdate {
    /// The page, new or fresher than the one on screen.
    Show(Box<crate::group::GroupView>),
    /// There is nothing to show any more: the group is gone, its account cannot be used, or the
    /// core no longer knows the group. The panel closes.
    Ended,
}

impl std::fmt::Debug for GroupUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Show(view) => write!(f, "GroupUpdate::Show({view:?})"),
            Self::Ended => f.write_str("GroupUpdate::Ended"),
        }
    }
}

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

/// The end of a replace's own login ([`UiSource::take_replace_news`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplaceNews {
    /// The core announced the account the replace's link made.
    Answered {
        /// The replaced account.
        old: AccountId,
        /// The new account.
        new: AccountId,
    },
    /// No new account comes.
    Ended {
        /// The replaced account.
        old: AccountId,
    },
}

/// Rows and commands for the UI models.
pub trait UiSource {
    /// "Now" and the local calendar the row labels were built with.
    fn time(&self) -> TimeContext<'_>;
    /// Accounts for the drawer.
    fn accounts(&self) -> Vec<AccountRow>;
    /// Conversation-list rows passing `filter`, in list order.
    fn conversations(&self, filter: &ConversationFilter) -> Vec<ConversationRow>;
    /// How many archived chats the accounts of `filter` have (the number on the list's
    /// "Archived Chats" entry); not by the filter's query or view.
    fn archived_count(&self, filter: &ConversationFilter) -> u32 {
        let archive = ConversationFilter {
            hidden: filter.hidden.clone(),
            query: String::new(),
            archived: true,
        };
        u32::try_from(self.conversations(&archive).len()).unwrap_or(u32::MAX)
    }
    /// Whether the accounts have answered, so the uuids of a filter's hidden set can be told
    /// from the ids of the rows (a source that knows its accounts from the start says so).
    /// Until then a non-empty hidden set lists nothing, and the shown accounts' choice
    /// ([`crate::shown_accounts`]) forgets nothing.
    fn accounts_known(&self) -> bool {
        true
    }
    /// Header of a conversation.
    fn chat(&self, id: ConversationId) -> Option<ChatHeader>;
    /// Timeline rows of a conversation, newest first.
    fn timeline(&self, id: ConversationId) -> Vec<MessageRow>;
    /// The lines under the own messages of the open conversation that are on its way, as the
    /// rows of [`timeline`](Self::timeline) would show them now: what [`Changes::waits`]
    /// asks the UI to bring up to date, row by row, without rebuilding the timeline.
    fn wait_lines(&self, _id: ConversationId) -> Vec<(MessageId, WaitLine)> {
        Vec::new()
    }
    /// The rings on the files of the open conversation's own messages that wait or upload, as
    /// the rows of [`timeline`](Self::timeline) would show them now, for the rows of the
    /// model that differ (`shown` is the model's rows, newest first; the answer is `(index,
    /// row)`): what [`Changes::uploads`] asks the UI to bring up to date, row by row, without
    /// rebuilding the timeline.
    fn upload_rows(&self, _id: ConversationId, _shown: &[MessageRow]) -> Vec<(usize, MessageRow)> {
        Vec::new()
    }
    /// Networks offered on the welcome page.
    fn networks(&self) -> Vec<NetworkChoice> {
        network_choices(&[NetworkEntry::plain(NetworkKind::Signal)], false)
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
    /// (§5.8), optionally as a reply; `as_file[i]` sends file `i` as the file it is and
    /// `with_location[i]` says the user agreed that file `i` leaves with the place it holds
    /// (missing entries are `false`; the core refuses a located file without it). The core makes as many messages of them as the network needs to
    /// show every file ([`UiSource::plan_files`] says how many); the rows show them on a
    /// later query, each with its own state. Returns the id of the request: it is named
    /// again when the request ends ([`UiSource::take_finished_sends`],
    /// [`RefusedSend::request`]), until when the files must stay where they are.
    fn send_files(
        &mut self,
        _id: ConversationId,
        _paths: Vec<PathBuf>,
        _as_file: Vec<bool>,
        _with_location: Vec<bool>,
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
    /// Whether quiet notices (a safety number that changed, a refreshed session, a changed
    /// number, an accepted request) count for the conversation list: `Some(true)` they are
    /// shown there as any message, `Some(false)` (the default) the list shows the chat's
    /// last real message and the notice does not move it. `None` until the core has said
    /// ([`Self::load_settings`]) and for a source without the setting (the fixture).
    fn quiet_notices(&self) -> Option<bool> {
        None
    }
    /// Chooses that. It holds at once and is stored by the core, which then gives every
    /// chat the sort time that choice says; the list is read again behind the command (the
    /// order and the previews come from the core, §8.10). If it cannot be stored,
    /// [`Self::quiet_notices`] reads what it was again ([`Changes::settings`]) and a notice
    /// says so. `false`: the command did not leave (the core has stopped), or no core.
    fn set_quiet_notices(&mut self, _shown: bool) -> bool {
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

    /// Asks for the page of `user` (people pages P1): the answer comes through
    /// [`UiSource::take_person`] ([`Changes::person`]), and again whenever the contact, the
    /// chat or the groups change while the page is shown ([`UiSource::close_person`] ends that).
    /// A fresh profile is asked of the network once for each call. `origin` is the chat the
    /// panel was opened from (the open chat: the person's own 1:1 chat, or the group whose member
    /// list the page was opened from): the page ends with it, and with nothing else of the
    /// person's (a 1:1 chat of theirs that is deleted meanwhile only changes what the page
    /// offers). `false`: not available.
    fn request_person(
        &mut self,
        _account: AccountId,
        _user: UserId,
        _origin: Option<ConversationId>,
    ) -> bool {
        false
    }
    /// The newest answer to [`UiSource::request_person`], once.
    fn take_person(&mut self) -> Option<PersonUpdate> {
        None
    }
    /// The page asked for with [`UiSource::request_person`] is no longer shown.
    fn close_person(&mut self) {}

    /// A copy action about a person (the menu of a chat, an account or a member): the answer
    /// comes through [`UiSource::take_copy`] ([`Changes::copy`]). Only the newest action is
    /// answered: what the user clicked last is what lands on the clipboard. `false`: not asked.
    fn request_copy(&mut self, _part: CopyPart, _target: CopyTarget) -> bool {
        false
    }
    /// Asks the network about a group's invite link; the answer comes as a copy
    /// ([`UiSource::take_copy`]). `false`: nothing was asked.
    fn request_group_invite(
        &mut self,
        _conversation: ConversationId,
        _action: kourier_model::GroupInviteAction,
    ) -> bool {
        false
    }
    /// The answer to [`UiSource::request_copy`], once.
    fn take_copy(&mut self) -> Option<CopyUpdate> {
        None
    }

    /// Asks for the safety number of `user` (P5a, the details panel's "safety" level): the
    /// answer comes through [`UiSource::take_safety`] ([`Changes::safety`]), and again whenever
    /// the account's contacts or the accounts change while the level is shown (one query in
    /// flight; an answer equal to the one on screen hands nothing over), and after a refused
    /// Mark, Clear or Confirm of that person (with the line of [`crate::safety::refused_text`] or
    /// [`crate::safety::not_saved_text`]); a number that turned into another one under the level
    /// carries [`crate::safety::swapped_text`]; the line stays until the next click or the close.
    /// [`UiSource::close_safety`] ends that. `false`: not available.
    fn request_safety(&mut self, _account: AccountId, _user: UserId) -> bool {
        false
    }
    /// The newest answer to [`UiSource::request_safety`], once.
    fn take_safety(&mut self) -> Option<crate::safety::SafetyView> {
        None
    }
    /// The level asked for with [`UiSource::request_safety`] is no longer shown.
    fn close_safety(&mut self) {}
    /// A button of the safety-number level was pressed (P5a): `Mark` and `Clear` send
    /// `UiCommand::SetVerified`, `Confirm` sends `UiCommand::ConfirmIdentity` (BQ-2b: never a
    /// Clear), each with `key`, the key of the number ON SCREEN (the front end keeps the
    /// [`crate::safety::SafetyView`] it shows and passes its key; a key that changed meanwhile
    /// is refused by the backend and the level asks again). `false`: not available.
    fn safety_action(
        &mut self,
        _account: AccountId,
        _user: UserId,
        _action: crate::safety::SafetyAction,
        _key: kourier_model::IdentityFingerprint,
    ) -> bool {
        false
    }

    /// The chats a contact card may go to (P5c, the share picker): every chat of the list the
    /// source holds, with whether the user can send there, before the person's account and the
    /// search narrow them ([`crate::share::listed_targets`]).
    fn share_targets(&self) -> Vec<crate::share::ShareTarget> {
        Vec::new()
    }
    /// "Send" of the share picker (P5c): a contact card of `user`, a person of `account`, into
    /// `conv`. Only the core builds the card (from the contact row, with the page's number rule
    /// and the profile name) and refuses what is not allowed (another account's chat, a person
    /// without a card); a refusal arrives later as a notice. `false`: the command did not leave.
    fn send_contact(&mut self, _conv: ConversationId, _account: AccountId, _user: UserId) -> bool {
        false
    }

    /// Asks for the page of the group `conv` (people pages P2): the answer comes through
    /// [`UiSource::take_group`] ([`Changes::group`]), and again whenever the group, a member's
    /// contact or the accounts change while the page is shown ([`UiSource::close_group`] ends
    /// that). The group is the page's origin: the page ends with it. `false`: not available.
    fn request_group(&mut self, _account: AccountId, _conv: ConversationId) -> bool {
        false
    }
    /// The newest answer to [`UiSource::request_group`], once.
    fn take_group(&mut self) -> Option<GroupUpdate> {
        None
    }
    /// The page asked for with [`UiSource::request_group`] is no longer shown.
    fn close_group(&mut self) {}

    /// Shows the gallery of the chat `conv` of `account` in `slot` (people pages P4: the section
    /// of a page, the grid or the list of the level of all media): the rows come through
    /// [`UiSource::take_chat_media`] ([`Changes::chat_media`]), and again whenever the chat's
    /// messages change while the slot is open ([`UiSource::close_chat_media`] ends that); the
    /// slot ends, shown empty, with its chat (removed) and with its account (removed).
    /// `false`: not available.
    fn open_chat_media(
        &mut self,
        _slot: crate::chat_media::MediaSlot,
        _account: AccountId,
        _conv: ConversationId,
    ) -> bool {
        false
    }
    /// Opens the picture viewer's own slot ([`crate::chat_media::MediaSlot::Viewer`]) for the
    /// pictures of `message`, a message among the rows of `from` (a section or the level's grid):
    /// the whole album, read AT the message, and kept up to date as the chat changes, whatever
    /// the rows of `from` do (a section holds six squares); it is empty when the message has no
    /// picture left. The rows come through [`UiSource::take_chat_media`]; [`UiSource::close_chat_media`]
    /// ends it. `false`: the message is not among the rows of `from` (or not available).
    fn open_media_viewer(
        &mut self,
        _from: crate::chat_media::MediaSlot,
        _message: MessageId,
    ) -> bool {
        false
    }
    /// Asks for the next page of a slot of the level of all media (after its last row); a section
    /// never goes on. The rows come through [`UiSource::take_chat_media`].
    fn more_chat_media(&mut self, _slot: crate::chat_media::MediaSlot) {}
    /// The slot shows nothing any more.
    fn close_chat_media(&mut self, _slot: crate::chat_media::MediaSlot) {}
    /// What the slots changed since the last call, once per slot (the newest of each).
    fn take_chat_media(&mut self) -> Vec<crate::chat_media::MediaUpdate> {
        Vec::new()
    }

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
    /// What the menu of `conv` offers of Archive, Pin, Mute and Mark as Unread
    /// ([`ChatOffer`]): the chat's own account's statement, read from the core's row, in the
    /// list or not. Nothing shown for a chat the source does not know.
    fn chat_offer(&self, _conv: ConversationId) -> ChatOffer {
        ChatOffer::default()
    }
    /// Archives (`true`) or unarchives `conv`. The notice ("Chat archived") comes later, once
    /// the core's row says so ([`UiSource::take_told`]).
    fn set_archived(&mut self, _conv: ConversationId, _archived: bool) -> bool {
        false
    }
    /// Pins or unpins `conv`.
    fn set_pinned(&mut self, _conv: ConversationId, _pinned: bool) -> bool {
        false
    }
    /// Mutes `conv` until `until` (ms since the epoch; `i64::MAX` = always), or unmutes it
    /// (`None`).
    fn set_muted(&mut self, _conv: ConversationId, _until: Option<i64>) -> bool {
        false
    }
    /// Marks `conv` unread (`true`) or removes the mark. Marking the open chat unread keeps
    /// the mark until the chat is read for real.
    fn set_marked_unread(&mut self, _conv: ConversationId, _marked: bool) -> bool {
        false
    }
    /// Takes an archive back (Undo): unarchives `conv` and pins it again when the archive took
    /// a pin with it. Leaves no notice of its own.
    fn undo_archive(&mut self, _conv: ConversationId, _was_pinned: bool) -> bool {
        false
    }
    /// Turns the account's switch for writing the chat settings to its other devices on or off.
    fn set_roaming_writes(&mut self, _account: AccountId, _on: bool) -> bool {
        false
    }
    /// The short notices these changes left since the last call, in order
    /// ([`Changes::told`]).
    fn take_told(&mut self) -> Vec<ChatTold> {
        Vec::new()
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
    /// How many of the models' reads are asked of the core and not applied yet (a query in
    /// flight, or an invalidation that waits for the one in flight to be followed up): the
    /// rows on screen are going to change. A screenshot waits for 0.
    fn queries_in_flight(&self) -> usize {
        0
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
    /// The banners above the list for the accounts `filter` leaves shown: [`Self::banner`] and
    /// [`Self::transfer_banner`] over every account, where an account in `filter.hidden` speaks
    /// only when it needs the user (a history transfer whatever its state, or `Error`,
    /// `Restricted`, `Clock`, `Challenge`, `LoggedOut`), plus the transfer of the shown
    /// accounts alone. The default knows no hidden account: the three over all accounts.
    fn banners(&self, _filter: &ConversationFilter) -> Banners {
        Banners {
            state: self.banner(None),
            transfer: self.transfer_banner(None),
            shown_transfer: self.transfer_banner(None),
        }
    }
    /// "Link again" for a logged-out account: starts its login; returns the first step.
    fn relink(&mut self, _account: AccountId) -> Result<LoginView, SourceError> {
        Err(SourceError::Unavailable)
    }
    /// Switches the link page to another way of linking the same network (`qr`, `pair-code`):
    /// the running login is stopped and the other flow starts at once, with no question (the
    /// person did not leave the page). Returns the first view of the new login; a second call
    /// while it has shown no step changes nothing. `Unavailable`: no login, a flow the network
    /// does not offer, or a step where the page offers no switch.
    fn switch_login_flow(&mut self, _flow: &str) -> Result<LoginView, SourceError> {
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
    /// What became of the login of a confirmed replace, once, in order: its new account's id
    /// (the choice of shown accounts passes to it) or its end without an account (cancelled,
    /// failed, lost with the core's channel; also after the announcement: the announced
    /// account's link cancelled or failed, or the account gone before it was visible).
    fn take_replace_news(&mut self) -> Vec<ReplaceNews> {
        Vec::new()
    }

    /// "Remove Account…" (`AccountAction::Remove`) or "Unlink…" (`AccountAction::Unlink`) of an
    /// account: the question to put first. `None`: the account does not exist (any more), is
    /// being removed, or does not offer the action (an account that is logged out or still
    /// linking cannot be unlinked). Nothing is sent; the source remembers which question is
    /// open, and opening it closes any other question about an account.
    fn ask_account_action(
        &mut self,
        _account: AccountId,
        _action: AccountAction,
    ) -> Option<ReplaceQuestion> {
        None
    }
    /// The user confirmed the open question about `account` and `action`: the command goes
    /// out, alone (no other login is cancelled, nothing is linked). Only that very question,
    /// and only once: anything else sends nothing and answers `false`.
    fn confirm_account_action(&mut self, _account: AccountId, _action: AccountAction) -> bool {
        false
    }
    /// The question about an account action was closed without confirming.
    fn cancel_account_action(&mut self) {}
    /// What the user is told once about the device of an account that was removed or
    /// unlinked ([`crate::Changes::account_notice`]). Taken once.
    fn take_logout_notices(&mut self) -> Vec<LogoutNotice> {
        Vec::new()
    }

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
    /// Asks the name `id` is offered under in the "Save As" dialog; the answer comes later through
    /// [`UiSource::take_saves`] as [`SaveAnswer::Suggested`] (or, when the attachment is gone,
    /// as a [`SaveAnswer::Done`] that failed).
    fn ask_save(&mut self, _id: AttachmentId) -> bool {
        false
    }
    /// Saves `id` as the file `name` in `dir` (the user's choice in the dialog, which asked
    /// about replacing): `name` is used EXACTLY as given, replacing a file of that name (it is
    /// never cleaned on the way: the dialog's question was about this very name, and the
    /// writer alone refuses a name that is no file name). The answer comes through
    /// [`UiSource::take_saves`].
    fn save_to(&mut self, _id: AttachmentId, _dir: PathBuf, _name: String) -> bool {
        false
    }
    /// Saves every one of `ids` into `dir` under the name it is offered with, never replacing a
    /// file (a clash gets " (1)", " (2)"): ONE [`SaveAnswer::Done`] when the last one ended.
    fn save_all(&mut self, _ids: Vec<AttachmentId>, _dir: PathBuf) -> bool {
        false
    }
    /// The answers to the save calls since the last call, in the order they came.
    fn take_saves(&mut self) -> Vec<SaveAnswer> {
        Vec::new()
    }
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
    /// Abandons the login. It may end a replace ([`Self::take_replace_news`]): the front-end
    /// reads the accounts after it.
    fn cancel_login(&mut self) {}
    /// The question to put before an action that would abandon the login in progress
    /// ([`Self::cancel_login`] would stop it: a link not taken by the core yet, e.g. one
    /// waiting at its QR code). `None`: nothing would be lost, go on without asking.
    fn abandon_question(&self) -> Option<AbandonQuestion> {
        None
    }
    /// The question to put before the application quits (`buttons.md` F8): while a history
    /// transfer runs or a new link still fetches its contacts and groups. `None`: quit at once.
    fn quit_question(&self) -> Option<AbandonQuestion> {
        None
    }
    /// Window focus and whether the chat page is on screen (notifications, read markers).
    /// The colors the person gave accounts (uuid to `#rrggbb`); the others have their network's.
    fn set_account_tints(
        &mut self,
        _tints: std::collections::HashMap<kourier_model::AccountUuid, String>,
    ) -> Changes {
        Changes::default()
    }

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
    /// The message the notice is about, when the notice offers "Delete for Everyone" for it
    /// (the X came after the message went out: "Already sent"); `None` for every other notice.
    fn notice_message(&self) -> Option<MessageId> {
        None
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
        let hidden = filter.resolve(
            self.fixture.accounts.iter().map(|(a, _, _)| (a.id, a.uuid)),
            self.accounts_known(),
        );
        self.fixture.listed_rows(filter, &hidden, &self.time())
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

    fn share_targets(&self) -> Vec<crate::share::ShareTarget> {
        let time = self.time();
        self.fixture
            .conversation_rows(&time)
            .into_iter()
            .map(|r| crate::share::ShareTarget {
                can_send: self
                    .fixture
                    .chat_header(r.id, &time)
                    .is_none_or(|h| h.cannot_send.is_empty()),
                id: r.id,
                account: r.account,
                title: r.title,
                avatar_url: r.avatar_url,
                is_self: r.is_self,
                archived: r.archived,
                pinned_order: r.pinned_order,
                last_activity: r.last_activity,
            })
            .collect()
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
