//! The `App` QML singleton: owns the models and controllers (`CONSTANT` pointer properties,
//! §8.1), the row source, the conversation filter, the event pump and the refresh logic.
//!
//! With a [`UiContext`](crate::UiContext) the source is a [`CoreSource`] over the core facade:
//! a producer task on the app's runtime forwards every `UiEvent` through the [`PumpQueue`],
//! and [`qobject::App::drain_events`] hands them to the source on the Qt thread (§8.5). Without
//! one (pure UI tests) it is the fixture data set or an empty source.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cxx::UniquePtr;
use cxx_qt::{CxxQtThread, CxxQtType, Threading};
use cxx_qt_lib::{
    QColor, QDateTime, QList, QMap, QMapPair_QString_QVariant, QString, QTimeZone, QUrl, QVariant,
};
use kourier_app::facade::{UiCommand, UiEvent};
use kourier_app::{AppHandle, NetworkInfo, TelegramMissing, trace};
use kourier_model::{AccountId, AttachmentId, ConversationId, MessageId, NetworkKind, UserId};
use kourier_viewmodel::chat_media::MediaSlot;
use kourier_viewmodel::conversations::BannerKind;
use kourier_viewmodel::diff::ListOp;
use kourier_viewmodel::i18n::trc;
use kourier_viewmodel::login::{NetworkEntry, network_choices, privacy_note, welcome_footnote};
use kourier_viewmodel::source::{GroupUpdate, PersonUpdate, SaveAnswer, SaveFailure};
use kourier_viewmodel::time::{Calendar, FixedOffset};
use kourier_viewmodel::timeline::{Block, QuoteState};
use kourier_viewmodel::{
    AccountAction, AccountRow, ArchiveMove, Changes, ConversationFilter, CoreSource,
    CoreSourceConfig, EmptySource, FixtureSource, LoginViewKind, MediaAnswer, NetworkChoice,
    NetworkMissing, ReplaceNews, ReplaceQuestion, RowKind, ShownAccounts, TransferView, UiSource,
};
use tokio::sync::broadcast::error::RecvError;

use crate::bridge::qobject::{
    self, AccountListModel, AttachmentListModel, ComposerController, ConversationListModel,
    LoginController, MediaListModel, MemberListModel, QObject, SettingsController,
    ShareTargetListModel, TimelineModel,
};
use crate::controllers::SettingsRust;
use crate::models::{self, ListModel};
use crate::pump::{DRAIN_BUDGET, Drained, PumpItem, PumpQueue};
use crate::shared::{Shared, shared};
use crate::{UiContext, UiOptions, options, take_context};

/// What starts the program `App.openExternalUrl` asks for: [`kourier_app::opener::start`]. A
/// test replaces it ([`set_url_launcher`]) to see the command (the URL and the environment it
/// would hand the child) without starting anything.
static URL_LAUNCHER: std::sync::Mutex<fn(std::process::Command) -> bool> =
    std::sync::Mutex::new(kourier_app::opener::start);

/// Tests: what `App.openExternalUrl` starts instead of the real program.
#[doc(hidden)]
pub fn set_url_launcher(launcher: fn(std::process::Command) -> bool) {
    *URL_LAUNCHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = launcher;
}

/// What "Show in Folder" does with a saved file: [`kourier_app::show::in_folder`] (the file manager
/// selects it, else its folder opens). A test replaces it ([`set_folder_shower`]) to see the path.
static FOLDER_SHOWER: std::sync::Mutex<fn(&Path)> = std::sync::Mutex::new(show_in_file_manager);

/// Tests: what "Show in Folder" does instead of the file manager's call.
#[doc(hidden)]
pub fn set_folder_shower(shower: fn(&Path)) {
    *FOLDER_SHOWER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = shower;
}

fn show_in_file_manager(file: &Path) {
    let drop_env = crate::env_set_by_kourier()
        .into_iter()
        .map(str::to_owned)
        .collect();
    kourier_app::show::in_folder(file, drop_env);
}

/// Sub-objects owned by `App`; dropped (and deleted) with it.
/// The list's search and view (the rest of [`ConversationFilter`]; its `hidden` comes from the
/// choice of shown accounts).
#[derive(Default)]
struct ListView {
    query: String,
    archived: bool,
}

struct Owned {
    _accounts: UniquePtr<AccountListModel>,
    _group_members: UniquePtr<MemberListModel>,
    _group_preview: UniquePtr<MemberListModel>,
    _share_targets: UniquePtr<ShareTargetListModel>,
    _chat_media: [UniquePtr<MediaListModel>; MediaSlot::COUNT],
    _conversations: UniquePtr<ConversationListModel>,
    _timeline: UniquePtr<TimelineModel>,
    _attachments: UniquePtr<AttachmentListModel>,
    _composer: UniquePtr<ComposerController>,
    _login: UniquePtr<LoginController>,
    _settings: UniquePtr<SettingsController>,
}

/// Rust side of `App`.
pub struct AppRust {
    pub(crate) version: QString,
    pub(crate) fixture: bool,
    pub(crate) start_page: QString,
    pub(crate) screenshot: QString,
    pub(crate) window_width: i32,
    pub(crate) window_height: i32,
    pub(crate) has_accounts: bool,
    pub(crate) loaded: bool,
    pub(crate) timers_active: bool,
    pub(crate) current_account: i64,
    pub(crate) current_title: QString,
    /// The title's short form for several shown accounts ("3 accounts"), for when their
    /// names do not fit (the list page measures); empty otherwise.
    pub(crate) current_title_short: QString,
    /// Several accounts may be shown together (the setting `blendAccounts`, as the choice
    /// applied it).
    pub(crate) blend: bool,
    /// Every account is shown ("All Chats" is the tinted entry).
    pub(crate) all_shown: bool,
    /// How many accounts are shown (a replace's waiting place included).
    pub(crate) shown_count: i32,
    /// The unread count of the hidden accounts (the sum of their rail pills): the dot on the
    /// narrow window's menu button.
    pub(crate) hidden_unread: i32,
    /// The history transfer of a SHOWN account (the map of `bannerTransfer`): what the empty
    /// list's placeholder may tell about.
    pub(crate) banner_shown_transfer: QVariant,
    pub(crate) start_hidden: bool,
    pub(crate) release_delay_ms: i32,
    /// Timing marks wait for a frame (`frameTraced`).
    pub(crate) trace_frame: bool,
    /// The scripted session of a measuring build (empty in every other build).
    pub(crate) perf_scenario: QString,
    pub(crate) tray_status: i32,
    pub(crate) unread_count: i32,
    /// The archive view is shown instead of the chat list (`showArchive`).
    pub(crate) archive_shown: bool,
    /// How many archived chats the accounts shown have (the number on the list's last row).
    pub(crate) archived_count: i32,
    pub(crate) banner_kind: QString,
    pub(crate) banner_text: QString,
    pub(crate) banner_action: QString,
    /// What the network itself said about a ban or a logout (plain text, one line).
    pub(crate) banner_explanation: QString,
    /// The page the network offers about it (`https://` only) and the label of its button.
    pub(crate) banner_appeal_url: QString,
    pub(crate) banner_appeal_label: QString,
    /// The network whose link page opens once the window is up (the one-shot preference), or
    /// empty.
    pub(crate) pending_link: QString,
    pub(crate) banner_account: i64,
    /// The history transfer the account banner shows, under the state of the accounts if
    /// there is one (`bannerKind`), as a map; its `phase` is empty when there is none,
    /// `failed` for a failure that waits for the user.
    pub(crate) banner_transfer: QVariant,
    /// The accounts that offer "Transfer History from Phone…" (`account`, `label`, `about`),
    /// for the settings page.
    pub(crate) history_accounts: QVariant,
    pub(crate) account_actions: QVariant,
    /// The versions of the message `showRevisions` asked about, newest first.
    pub(crate) revisions: QVariant,
    /// The page of the person the details panel shows (`showPersonOfChat`), as a map.
    pub(crate) person: QVariant,
    /// The page of the group the details panel shows (`showGroupOfChat`), as a map.
    pub(crate) group: QVariant,
    /// The safety-number level the details panel shows (`showSafety`, P5a), as a map.
    pub(crate) safety: QVariant,
    pub(crate) networks: QVariant,
    /// The footnote under the welcome page's list (the viewmodel's choice, `welcome_footnote`).
    pub(crate) networks_note: QString,
    /// The second line Settings shows under "Read receipts" and "Typing indicators" ("Always sent
    /// for Telegram."): the viewmodel's choice (`privacy_note`), empty when no network that is on
    /// always sends them.
    pub(crate) privacy_note: QString,
    pub(crate) accounts: *mut AccountListModel,
    /// The group page's member rows, narrowed by the members search (`filter`).
    pub(crate) group_members: *mut MemberListModel,
    /// The first rows of the group level, unfiltered.
    pub(crate) group_preview: *mut MemberListModel,
    pub(crate) share_targets: *mut ShareTargetListModel,
    /// The gallery of a chat in its five places, in the order of `MediaSlot::index`: the two
    /// sections (group page, person page), the grid and the list of the level of all media, and
    /// the picture viewer's own (a model QML does not see: `galleryViewerItems("viewer", ..)`).
    pub(crate) chat_media: [*mut MediaListModel; MediaSlot::COUNT],
    /// The chat each slot shows (0: none): a slot that opens on another chat is emptied first.
    pub(crate) chat_media_convs: [i64; MediaSlot::COUNT],
    /// The numbers of the calls about the slots that may wait for the event loop.
    pub(crate) slot_calls: crate::models::media::SlotCalls,
    pub(crate) group_media: *mut MediaListModel,
    pub(crate) person_media: *mut MediaListModel,
    pub(crate) media_grid: *mut MediaListModel,
    pub(crate) media_files: *mut MediaListModel,
    pub(crate) conversations: *mut ConversationListModel,
    pub(crate) timeline: *mut TimelineModel,
    pub(crate) composer: *mut ComposerController,
    pub(crate) login: *mut LoginController,
    pub(crate) settings: *mut SettingsController,
    owned: Option<Owned>,
    shared: Option<Shared>,
    /// The search and the view of the list. The hidden accounts are NOT kept here: every
    /// filter is built from `shown` (`App::filter`), so no path can read a stale copy of the
    /// choice (one copy of the set).
    list_view: ListView,
    /// The choice of shown accounts (the rules live in the view model); the ONE copy of the
    /// hidden set every filter takes.
    shown: ShownAccounts,
    /// The drawer's rows as last read (with `shown` and `place` filled): the choice's input.
    account_rows: Vec<AccountRow>,
    /// The accounts have answered once (`UiSource::accounts_known`): from then on the source
    /// resolves the hidden set, so the list is read again at that moment.
    accounts_known: bool,
    /// The page last put into `group`, without its member rows: a refresh whose page equals it
    /// leaves `group` alone and touches the member models only.
    group_shown: Option<kourier_viewmodel::group::GroupView>,
    /// The safety-number level: whether one is open, the view last put into `safety` (a button
    /// acts on ITS person and key, the number on screen, never on a newer answer QML has not
    /// shown yet) and the number it last showed, held through answers without one (V3: what
    /// `swapped` compares the next numbered answer with). An answer taken while no level is
    /// open is dropped (Y4).
    safety_level: models::timeline::SafetyLevel,
    /// The link of the person's page on screen (P5b "Copy Link"), set with `person` and never
    /// put into it: the URL holds a third party's number and stays in Rust.
    person_link: Option<kourier_model::Redacted<String>>,
    /// Whose card "Share Contact…" sends (P5c): the account and the person of the page on screen
    /// when it offers a card (`ShareState::Ready`), set with `person`. Only the who: the card is
    /// built by the core.
    person_share: Option<(AccountId, UserId)>,
    generation: u64,
    app: Option<AppHandle>,
    pump: Option<Arc<PumpQueue>>,
    ready_sent: bool,
    auto_submit_login: bool,
    /// `openMedia`/`openExternally` requests and their answers.
    media: MediaRequests,
    /// Where the banner's challenge is solved (handed to the browser on request; never logged).
    banner_url: String,
    /// The instant a wake-up is scheduled for, to read the rows' action flags again
    /// (`UiSource::flags_deadline`).
    flags_armed: Option<Instant>,
    /// Timing marks in progress (`KOURIER_TRACE_STARTUP=1`, architecture §1.3).
    trace: UiTrace,
    /// What the window last reported: it has the focus, its chat page is on screen.
    presence: (bool, bool),
    /// What `bannerTransfer` was made from (a property is written only when its view
    /// changed: a download reports several times a second).
    shown_transfer: Option<TransferView>,
    /// What `bannerShownTransfer` was made from.
    shown_placeholder_transfer: Option<TransferView>,
    /// What `historyAccounts` was made from: `(account, which account it is)`.
    shown_history: Vec<(i64, String)>,
    /// What `accountActions` was made from.
    shown_actions: Vec<ShownActions>,
}

/// One account of `accountActions`: `(account, which account it is, offers the transfer,
/// offers "Unlink…")`.
/// `(account, about, offers history, offers unlink, shown, last shown)`.
type ShownActions = (i64, String, bool, bool, bool, bool);

/// Timing marks that end with the frame that shows their rows (`kourier_app::trace`).
#[derive(Clone, Copy, Debug, Default)]
struct UiTrace {
    /// Rows the conversation list got and no frame has shown yet.
    list_rows: Option<i32>,
    /// `startup.list_populated` was written.
    list_done: bool,
    /// Refreshes of the conversation list until then, and the time they took (ms).
    list_refreshes: u32,
    list_refresh_ms: f64,
    /// The conversation being opened.
    open: Option<OpenTrace>,
}

/// One `timeline.open` measurement.
#[derive(Clone, Copy, Debug)]
struct OpenTrace {
    conversation: i64,
    span: trace::Span,
    /// The rows once they arrived, and how long the query took (ms).
    rows: Option<(i32, f64)>,
}

/// Who waits for one `OpenMedia` answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Waiter {
    /// A QML view with its request token.
    View(i64),
    /// `openExternally`.
    External,
    /// Nobody any more: the copy is closed when it comes.
    Abandoned,
}

/// Bookkeeping of `OpenMedia` requests (§8.6). Every answer names its attachment
/// (`MediaOpened`, or a failed `OpenMedia` with `ActionSubject::Attachment`) and belongs to
/// the oldest request for that attachment: the core serves opens on one ordered queue, so
/// several requests for one attachment are answered in the order they were sent. Every
/// answered open needs one `CloseMedia`.
#[derive(Debug, Default)]
pub(crate) struct MediaRequests {
    next: i64,
    /// Requests without an answer yet, oldest first.
    pending: VecDeque<(AttachmentId, Waiter)>,
    answered: HashMap<i64, AttachmentId>,
    /// Copies handed to other applications: closed with the conversation.
    kept: Vec<AttachmentId>,
}

/// What to do with one answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// Emit `mediaOpened(token, url, movie)`.
    View(i64),
    /// Emit `externalReady(attachment, url)`.
    External,
    /// Close the copy at once.
    Close,
}

/// Requests that ended without a copy.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Unanswered {
    /// Views to tell (`mediaFailed(token)`); they may ask again.
    pub(crate) views: Vec<i64>,
    /// An `openExternally` among them: the user is told the file did not open.
    pub(crate) external: bool,
}

impl Unanswered {
    fn add(&mut self, waiter: Waiter) {
        match waiter {
            Waiter::View(token) => self.views.push(token),
            Waiter::External => self.external = true,
            Waiter::Abandoned => {}
        }
    }
}

impl MediaRequests {
    /// A view asks for `id`; returns its token.
    pub(crate) fn request(&mut self, id: AttachmentId) -> i64 {
        self.next += 1;
        let token = self.next;
        self.pending.push_back((id, Waiter::View(token)));
        token
    }

    /// `openExternally` asks for `id`.
    pub(crate) fn request_external(&mut self, id: AttachmentId) {
        self.pending.push_back((id, Waiter::External));
    }

    /// The view of `token` went; returns the attachment to close when its copy was open. A
    /// request still waiting keeps its place, so its answer is closed when it comes.
    pub(crate) fn release(&mut self, token: i64) -> Option<AttachmentId> {
        if let Some(id) = self.answered.remove(&token) {
            return Some(id);
        }
        for (_, w) in &mut self.pending {
            if *w == Waiter::View(token) {
                *w = Waiter::Abandoned;
            }
        }
        None
    }

    /// The core opened a copy of `id`: it goes to the oldest request for `id`. Requests sent
    /// before that one can no longer be answered (their answers were lost with lagged events)
    /// and are returned; an answer nobody waits for is closed.
    pub(crate) fn opened(&mut self, id: AttachmentId) -> (Delivery, Unanswered) {
        let mut lost = Unanswered::default();
        let Some(at) = self.pending.iter().position(|(a, _)| *a == id) else {
            return (Delivery::Close, lost);
        };
        for (_, w) in self.pending.drain(..at) {
            lost.add(w);
        }
        let delivery = match self.pending.pop_front() {
            Some((_, Waiter::View(token))) => {
                self.answered.insert(token, id);
                Delivery::View(token)
            }
            Some((_, Waiter::External)) => {
                self.kept.push(id);
                Delivery::External
            }
            Some((_, Waiter::Abandoned)) | None => Delivery::Close,
        };
        (delivery, lost)
    }

    /// An open failed. The core names the attachment: the oldest request for it is the one
    /// that failed, and every other request keeps waiting for its own answer. Without a name
    /// (`None`) it is the oldest request of all, as the core answers opens in order.
    pub(crate) fn failed(&mut self, id: Option<AttachmentId>) -> Unanswered {
        let mut failed = Unanswered::default();
        let at = match id {
            Some(id) => self.pending.iter().position(|(a, _)| *a == id),
            None => (!self.pending.is_empty()).then_some(0),
        };
        if let Some((_, w)) = at.and_then(|at| self.pending.remove(at)) {
            failed.add(w);
        }
        failed
    }

    /// Events were lost (a lagged channel): answers may be among them, so nobody waits any
    /// longer. Answers that still come are closed.
    pub(crate) fn lost(&mut self) -> Unanswered {
        let mut lost = Unanswered::default();
        for (_, w) in self.pending.drain(..) {
            lost.add(w);
        }
        lost
    }

    /// Takes the copies handed to other applications.
    pub(crate) fn take_kept(&mut self) -> Vec<AttachmentId> {
        std::mem::take(&mut self.kept)
    }
}

impl Default for AppRust {
    fn default() -> Self {
        let opts: UiOptions = options();
        let (w, h) = opts.size.unwrap_or((0, 0));
        Self {
            version: QString::from(kourier_app::about::info().version),
            fixture: opts.fixture,
            start_page: QString::from(opts.page.as_deref().unwrap_or("")),
            screenshot: QString::from(
                opts.screenshot
                    .as_deref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .as_str(),
            ),
            window_width: i32::try_from(w).unwrap_or(0),
            window_height: i32::try_from(h).unwrap_or(0),
            has_accounts: false,
            loaded: false,
            timers_active: false,
            current_account: -1,
            current_title: QString::from(
                trc(
                    "@title:window list header, the chats of all accounts",
                    "All Chats",
                )
                .as_ref(),
            ),
            current_title_short: QString::default(),
            blend: true,
            all_shown: true,
            shown_count: 0,
            hidden_unread: 0,
            banner_shown_transfer: transfer_variant(None),
            start_hidden: false,
            release_delay_ms: release_delay_ms(),
            trace_frame: false,
            perf_scenario: QString::from(perf_scenario(&opts).as_str()),
            // Unknown until the desktop integration reports (0 = no tray, 1 = tray shown); stays
            // unknown without desktop services (headless runs).
            tray_status: -1,
            unread_count: 0,
            archive_shown: false,
            archived_count: 0,
            banner_kind: QString::default(),
            banner_text: QString::default(),
            banner_action: QString::default(),
            banner_explanation: QString::default(),
            banner_appeal_url: QString::default(),
            banner_appeal_label: QString::default(),
            pending_link: QString::default(),
            banner_account: 0,
            banner_transfer: transfer_variant(None),
            history_accounts: QVariant::from(&QList::<QVariant>::default()),
            account_actions: QVariant::from(&QList::<QVariant>::default()),
            revisions: QVariant::from(&QList::<QVariant>::default()),
            person: QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()),
            group: QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()),
            safety: QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()),
            networks: QVariant::default(),
            networks_note: QString::default(),
            privacy_note: QString::default(),
            accounts: std::ptr::null_mut(),
            group_members: std::ptr::null_mut(),
            group_preview: std::ptr::null_mut(),
            share_targets: std::ptr::null_mut(),
            chat_media: [std::ptr::null_mut(); MediaSlot::COUNT],
            chat_media_convs: [0; MediaSlot::COUNT],
            slot_calls: crate::models::media::SlotCalls::default(),
            group_media: std::ptr::null_mut(),
            person_media: std::ptr::null_mut(),
            media_grid: std::ptr::null_mut(),
            media_files: std::ptr::null_mut(),
            conversations: std::ptr::null_mut(),
            timeline: std::ptr::null_mut(),
            composer: std::ptr::null_mut(),
            login: std::ptr::null_mut(),
            settings: std::ptr::null_mut(),
            owned: None,
            shared: None,
            list_view: ListView::default(),
            shown: ShownAccounts::default(),
            account_rows: Vec::new(),
            accounts_known: false,
            group_shown: None,
            safety_level: models::timeline::SafetyLevel::default(),
            person_link: None,
            person_share: None,
            generation: 0,
            app: None,
            pump: None,
            ready_sent: false,
            auto_submit_login: false,
            media: MediaRequests::default(),
            banner_url: String::new(),
            flags_armed: None,
            trace: UiTrace::default(),
            presence: (false, false),
            shown_transfer: None,
            shown_placeholder_transfer: None,
            shown_history: Vec::new(),
            shown_actions: Vec::new(),
        }
    }
}

fn string_variant(text: &str) -> QVariant {
    QVariant::from(&QString::from(text))
}

/// A history transfer as QML reads it; without one, the same keys with an empty `phase`.
fn transfer_variant(view: Option<&TransferView>) -> QVariant {
    let mut m = QMap::<QMapPair_QString_QVariant>::default();
    let text = |f: fn(&TransferView) -> &str| string_variant(view.map_or("", f));
    m.insert(
        QString::from("account"),
        QVariant::from(&view.map_or(0, |v| v.account.0)),
    );
    m.insert(
        QString::from("phase"),
        string_variant(view.map_or("", |v| v.phase.as_str())),
    );
    m.insert(QString::from("status"), text(|v| &v.status));
    m.insert(QString::from("detail"), text(|v| &v.detail));
    m.insert(
        QString::from("progress"),
        QVariant::from(&view.map_or(-1.0, |v| v.progress)),
    );
    m.insert(QString::from("skip"), text(|v| &v.skip));
    m.insert(QString::from("retry"), text(|v| &v.retry));
    m.insert(QString::from("dismiss"), text(|v| &v.dismiss));
    QVariant::from(&m)
}

/// A question with two answers (`title`, `text`, `confirm`, `cancel`) as QML reads it;
/// `undefined` without one.
fn abandon_variant(q: Option<kourier_viewmodel::AbandonQuestion>) -> QVariant {
    let Some(q) = q else {
        return QVariant::default();
    };
    let mut m = QMap::<QMapPair_QString_QVariant>::default();
    m.insert(QString::from("title"), string_variant(&q.title));
    m.insert(QString::from("text"), string_variant(&q.text));
    m.insert(QString::from("confirm"), string_variant(&q.confirm));
    m.insert(QString::from("cancel"), string_variant(&q.cancel));
    QVariant::from(&m)
}

/// The account action QML names (`"remove"`, `"unlink"`).
fn account_action(name: &QString) -> Option<AccountAction> {
    match name.to_string().as_str() {
        "remove" => Some(AccountAction::Remove),
        "unlink" => Some(AccountAction::Unlink),
        _ => None,
    }
}

/// The name QML uses for `action`.
fn account_action_name(action: AccountAction) -> &'static str {
    match action {
        AccountAction::Remove => "remove",
        AccountAction::Unlink => "unlink",
    }
}

/// The question before an account is removed and linked again, as QML reads it.
fn question_variant(q: &ReplaceQuestion) -> QVariant {
    QVariant::from(&question_variant_map(q))
}

/// [`question_variant`] as a map, to add to.
fn question_variant_map(q: &ReplaceQuestion) -> QMap<QMapPair_QString_QVariant> {
    let mut m = QMap::<QMapPair_QString_QVariant>::default();
    m.insert(QString::from("account"), QVariant::from(&q.account.0));
    m.insert(QString::from("title"), string_variant(&q.title));
    m.insert(QString::from("about"), string_variant(&q.about));
    let mut paragraphs = QList::<QVariant>::default();
    for p in &q.paragraphs {
        paragraphs.append(string_variant(p));
    }
    m.insert(QString::from("paragraphs"), QVariant::from(&paragraphs));
    m.insert(QString::from("confirm"), string_variant(&q.confirm));
    m.insert(QString::from("cancel"), string_variant(&q.cancel));
    m
}

/// The scripted session a measuring build was started with (always empty otherwise).
fn perf_scenario(opts: &UiOptions) -> String {
    #[cfg(feature = "perf")]
    {
        opts.perf_scenario.clone().unwrap_or_default()
    }
    #[cfg(not(feature = "perf"))]
    {
        let _ = opts;
        String::new()
    }
}

/// How long after the first frame the heap is trimmed once: the start is over by then, and
/// the user is not kept waiting by it (`malloc_trim` takes a few milliseconds).
const STARTUP_TRIM_AFTER: Duration = Duration::from_secs(5);

/// How long after a hidden window was released the heap is trimmed a second time: by then
/// the core has given back its caches and the window's last objects are deleted.
const RELEASE_SETTLE: Duration = Duration::from_secs(2);

/// The word QML reads a save failure by.
fn save_reason(failure: SaveFailure) -> &'static str {
    match failure {
        SaveFailure::NoSpace => "nospace",
        SaveFailure::NotWritable => "notwritable",
        SaveFailure::NoFolder => "nofolder",
        SaveFailure::Gone => "gone",
        SaveFailure::NotHere => "nothere",
        SaveFailure::Other => "other",
    }
}

/// `file://` URL of an absolute local path, percent-encoded (spaces, `#`, `%`, non-ASCII).
pub fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A pinned reference to a sub-object owned by `App`.
///
/// SAFETY (for callers): the pointer comes from a `UniquePtr` in `AppRust::owned`, which lives
/// as long as `App`, and sub-objects are only touched on the Qt thread.
pub(crate) fn pinned<'a, T>(ptr: *mut T) -> Option<Pin<&'a mut T>> {
    // SAFETY: see above; QObjects never move once created.
    unsafe { ptr.as_mut().map(|r| Pin::new_unchecked(r)) }
}

fn network_list(source: &dyn UiSource) -> QVariant {
    let mut list = QList::<QVariant>::default();
    for n in source.networks() {
        let mut m = QMap::<QMapPair_QString_QVariant>::default();
        m.insert(
            QString::from("kind"),
            QVariant::from(&QString::from(n.kind.as_str())),
        );
        m.insert(
            QString::from("name"),
            QVariant::from(&QString::from(n.name.as_str())),
        );
        m.insert(
            QString::from("description"),
            QVariant::from(&QString::from(n.description.as_str())),
        );
        m.insert(
            QString::from("icon"),
            QVariant::from(&QString::from(n.icon.as_str())),
        );
        m.insert(QString::from("available"), QVariant::from(&n.available));
        // Registered at this start: a login can begin. Not on: an experimental network that
        // was turned off (its warning page turns it on).
        m.insert(QString::from("on"), QVariant::from(&n.on));
        m.insert(
            QString::from("experimental"),
            QVariant::from(&n.experimental),
        );
        m.insert(QString::from("asksFirst"), QVariant::from(&n.asks_first));
        m.insert(
            QString::from("offDescription"),
            QVariant::from(&QString::from(n.off_description.as_str())),
        );
        m.insert(
            QString::from("turnOffText"),
            QVariant::from(&QString::from(n.turn_off_text.as_str())),
        );
        // This build cannot log in to the network: Settings lists the row without a switch and
        // the welcome page and Add Account skip it (a bool is all QML needs; which part is
        // missing is for the banner, the viewmodel's words).
        m.insert(
            QString::from("missing"),
            QVariant::from(&n.missing.is_some()),
        );
        list.append(QVariant::from(&m));
    }
    QVariant::from(&list)
}

/// The local calendar as Qt knows it (time zone rules from the system).
struct QtCalendar {
    zone: UniquePtr<QTimeZone>,
}

impl Calendar for QtCalendar {
    fn utc_offset_s(&self, ts_ms: i64) -> i32 {
        match self.zone.as_ref() {
            Some(zone) => QDateTime::from_msecs_since_epoch(ts_ms, zone).offset_from_utc(),
            None => 0,
        }
    }
}

/// How long the window stays hidden in the tray before it is released (§8.2 step 6): 60 s;
/// debug builds (tests) and measuring builds (`cargo xtask perf --quick`) read
/// `KOURIER_DEV_RELEASE_MS`.
fn release_delay_ms() -> i32 {
    const DEFAULT: i32 = 60_000;
    #[cfg(any(debug_assertions, feature = "perf"))]
    if let Some(ms) = std::env::var("KOURIER_DEV_RELEASE_MS")
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .filter(|ms| *ms > 0)
    {
        return ms;
    }
    DEFAULT
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// The welcome page's networks: what the application holds (the registered ones, then the
/// experimental ones that are off at this start), turned into rows by the viewmodel.
fn choices(app: &AppHandle) -> Vec<NetworkChoice> {
    let entry = |n: &NetworkInfo, on: bool| NetworkEntry {
        kind: n.kind,
        flows: n
            .flows
            .iter()
            .map(|f| (f.id.clone(), f.label.clone()))
            .collect(),
        experimental: n.experimental,
        risk_notice: n
            .risk_notice
            .as_ref()
            .map(|r| (r.title.clone(), r.body.clone())),
        history_transfer: n.history_transfer,
        history: n.history,
        on,
        receipts_always_sent: n.receipts_always_sent,
        missing: None,
    };
    let entries: Vec<NetworkEntry> = app
        .networks()
        .iter()
        .map(|n| entry(n, true))
        .chain(app.networks_off().iter().map(|n| entry(n, false)))
        // The networks this build cannot log in to (Telegram without its API key or its library,
        // on or off): neither registered nor listed as off, so the application names them here.
        .chain(
            app.networks_missing()
                .into_iter()
                .map(|(kind, missing)| missing_entry(kind, missing)),
        )
        .collect();
    network_choices(&entries, app.is_fixture())
}

/// What the viewmodel says is missing, from the application's typed value.
fn missing_reason(missing: TelegramMissing) -> NetworkMissing {
    match missing {
        TelegramMissing::ApiKey => NetworkMissing::ApiKey,
        TelegramMissing::Library => NetworkMissing::Library,
    }
}

/// The entry of a network this build cannot log in to: a Settings row without a switch ("Not
/// available in this build") and the banner of its accounts. No flow and no warning: it is no
/// choice of the welcome page or Add Account. Experimental, as the networks that can be missing
/// are (Telegram), so Settings lists it with the others.
fn missing_entry(kind: NetworkKind, missing: TelegramMissing) -> NetworkEntry {
    NetworkEntry {
        kind,
        flows: Vec::new(),
        experimental: true,
        risk_notice: None,
        history_transfer: false,
        history: kourier_model::HistorySource::None,
        on: false,
        receipts_always_sent: false,
        missing: Some(missing_reason(missing)),
    }
}

/// Whether a debug run asked for the fixture's UI clock to run (`KOURIER_DEV_FIXTURE_CLOCK=run`).
fn fixture_clock_runs() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("KOURIER_DEV_FIXTURE_CLOCK").is_some_and(|v| v == "run")
}

fn core_source(ctx: &UiContext) -> CoreSource {
    let handle = ctx.app.clone();
    let fixture = handle.fixture();
    let now: Box<dyn Fn() -> i64> = match fixture {
        // Debug builds: `KOURIER_DEV_FIXTURE_CLOCK=run` lets the UI's "now" run with the
        // core's fixture clock, so what counts time (the line under a message on its way,
        // link feedback L6) can be looked at in fixture mode.
        Some(_) if fixture_clock_runs() => {
            let core = handle.clone();
            Box::new(move || core.core_now_ms())
        }
        Some(f) => Box::new(move || f.now_ms),
        None => Box::new(system_now_ms),
    };
    // Fixture screenshots must not depend on the machine's time zone either.
    let calendar: Box<dyn Calendar> = if handle.is_fixture() {
        Box::new(FixedOffset(0))
    } else {
        Box::new(QtCalendar {
            zone: QTimeZone::system_time_zone(),
        })
    };
    let config = CoreSourceConfig {
        now,
        calendar,
        networks: choices(&handle),
        hostname: handle.hostname().to_owned(),
        demo: fixture.and_then(|f| f.demo_conversation),
    };
    let sink = handle.clone();
    let mut source = CoreSource::new(Box::new(move |cmd| sink.send(cmd).is_ok()), config);
    source.set_avatar_urls(Box::new(AppAvatars(handle)));
    source
}

/// Avatar URLs of the image provider (`kourier_app::media::AvatarHandles`).
struct AppAvatars(AppHandle);

impl kourier_viewmodel::AvatarUrls for AppAvatars {
    fn url(
        &self,
        account: &kourier_model::AccountUuid,
        avatar: &kourier_model::BlobRef,
    ) -> Option<String> {
        self.0.avatar_url(account, avatar)
    }
}

/// Forwards core events to the Qt thread until the app shuts down (§8.5, §8.2 step 5).
fn spawn_pump(app: &AppHandle, queue: Arc<PumpQueue>, thread: CxxQtThread<qobject::App>) {
    let mut rx = app.subscribe();
    let stop = app.shutdown_token();
    app.runtime().spawn(async move {
        loop {
            let item = tokio::select! {
                _ = stop.cancelled() => break,
                r = rx.recv() => match r {
                    Ok(e) => {
                        // What a plaintext copy really is, read here (off the Qt thread)
                        // before the UI hears of it: `AnimatedImage` detects formats by
                        // content with every installed image plugin, so it gets GIF and WebP
                        // files only (`media::note_copy`).
                        if let UiEvent::MediaOpened { path, .. } = &e {
                            let path = path.clone();
                            let noted =
                                tokio::task::spawn_blocking(move || crate::media::note_copy(&path));
                            if noted.await.is_err() {
                                tracing::warn!("looking at a media copy failed");
                            }
                        }
                        PumpItem::Event(e)
                    }
                    Err(RecvError::Lagged(n)) => PumpItem::Lagged(n),
                    Err(RecvError::Closed) => break,
                },
            };
            let post = queue.push(item);
            // (A test slows the Qt side down: the event is in the queue and the drain comes
            // late, which `PumpQueue::len` counts.)
            let delay = crate::pump::test_delay();
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if post && thread.queue(|app| app.drain_events()).is_err() {
                // `App` is gone (the engine was destroyed).
                break;
            }
        }
    });
}

impl cxx_qt::Initialize for qobject::App {
    fn initialize(mut self: Pin<&mut Self>) {
        let opts = options();
        let ctx = take_context();
        let source: Box<dyn UiSource> = match &ctx {
            Some(ctx) => Box::new(core_source(ctx)),
            None if opts.fixture => match FixtureSource::new() {
                Ok(s) => Box::new(s),
                Err(e) => {
                    tracing::error!(error = %e, "fixture unavailable");
                    Box::new(EmptySource::new(0))
                }
            },
            None => Box::new(EmptySource::new(0)),
        };
        let networks = network_list(source.as_ref());
        let networks_note = QString::from(welcome_footnote(&source.networks()).as_str());
        let privacy_note = QString::from(privacy_note(&source.networks()).as_str());
        let demo = source.demo_conversation();
        let shared = shared(source);
        let thread = self.qt_thread();

        let accounts = qobject::new_account_list_model();
        let group_members = qobject::new_member_list_model();
        let group_preview = qobject::new_member_list_model();
        let share_targets = qobject::new_share_target_list_model();
        let mut chat_media = [
            qobject::new_media_list_model(),
            qobject::new_media_list_model(),
            qobject::new_media_list_model(),
            qobject::new_media_list_model(),
            qobject::new_media_list_model(),
        ];
        let conversations = qobject::new_conversation_list_model();
        let mut timeline = qobject::new_timeline_model();
        // No conversation yet: QML reads empty values (an empty timer list), never invalid ones.
        if let Some(mut t) = timeline.as_mut() {
            t.as_mut().set_header(0, None);
            let mut rust = t.rust_mut();
            rust.shared = Some(shared.clone());
            rust.app = Some(Box::new(self.qt_thread()));
        }
        let attachments = qobject::new_attachment_list_model();
        let mut composer = qobject::new_composer_controller();
        let mut login = qobject::new_login_controller();
        let mut settings = qobject::new_settings_controller();
        // The saved choice of shown accounts, put to work BEFORE the first list is built (the
        // source lists nothing hidden-dependent until the accounts answered; the choice is
        // normalised at that answer, M13).
        let saved_choice = ctx.as_ref().map(|c| {
            let prefs = c.app.desktop().prefs();
            let hidden: Vec<kourier_model::AccountUuid> = prefs
                .hidden_accounts
                .iter()
                .filter_map(|u| u.parse().ok())
                .collect();
            ShownAccounts::load(prefs.blend_accounts, hidden)
        });
        if let (Some(ctx), Some(s)) = (&ctx, settings.as_mut()) {
            let desktop = ctx.app.desktop().clone();
            *s.rust_mut() = SettingsRust::from_prefs(&desktop.prefs(), Some(desktop));
        }
        if let Some(s) = settings.as_mut() {
            let mut rust = s.rust_mut();
            rust.shared = Some(shared.clone());
            rust.app = Some(Box::new(self.qt_thread()));
        }
        if let Some(c) = composer.as_mut() {
            let mut rust = c.rust_mut();
            rust.shared = Some(shared.clone());
            rust.app = Some(Box::new(self.qt_thread()));
            rust.handle = ctx.as_ref().map(|c| c.app.clone());
            rust.files = attachments.as_mut_ptr();
        }
        // The image provider reads from this app from now on (§8.6).
        if let Some(ctx) = &ctx {
            crate::media::set_store(Some(Arc::new(ctx.app.clone())));
        }
        if let Some(l) = login.as_mut() {
            let mut rust = l.rust_mut();
            rust.shared = Some(shared.clone());
            rust.app = Some(Box::new(self.qt_thread()));
        }

        {
            let mut rust = self.as_mut().rust_mut();
            rust.networks = networks;
            rust.networks_note = networks_note;
            rust.privacy_note = privacy_note;
            rust.accounts = accounts.as_mut_ptr();
            rust.group_members = group_members.as_mut_ptr();
            rust.group_preview = group_preview.as_mut_ptr();
            rust.share_targets = share_targets.as_mut_ptr();
            for (slot, m) in chat_media.iter_mut().enumerate() {
                rust.chat_media[slot] = m.as_mut_ptr();
            }
            rust.group_media = rust.chat_media[0];
            rust.person_media = rust.chat_media[1];
            rust.media_grid = rust.chat_media[2];
            rust.media_files = rust.chat_media[3];
            rust.conversations = conversations.as_mut_ptr();
            rust.timeline = timeline.as_mut_ptr();
            rust.composer = composer.as_mut_ptr();
            rust.login = login.as_mut_ptr();
            rust.settings = settings.as_mut_ptr();
            rust.owned = Some(Owned {
                _accounts: accounts,
                _group_members: group_members,
                _group_preview: group_preview,
                _share_targets: share_targets,
                _chat_media: chat_media,
                _conversations: conversations,
                _timeline: timeline,
                _attachments: attachments,
                _composer: composer,
                _login: login,
                _settings: settings,
            });
            rust.shared = Some(shared.clone());
            if let Some(choice) = saved_choice {
                rust.blend = choice.blend();
                // A saved choice that hides accounts is not "All Chats" before the accounts
                // answer: nothing is tinted and the title waits for the names.
                if !choice.all_shown(&[]) {
                    rust.all_shown = false;
                    rust.current_title = QString::default();
                }
                rust.shown = choice;
            }
            // (The pages of the link page's transfer states walk the link by themselves.)
            rust.auto_submit_login = matches!(
                opts.page.as_deref(),
                Some("qr" | "transferwait" | "transferdownload" | "transferread")
            );
        }
        // Every `Qt.openUrlExternally` of the process (Kirigami's About page and link delegates
        // included) ends in `open_external_url`, not in Qt's own services.
        // SAFETY: `self` is a live `App`, which has the slot `openExternalUrl(QUrl)`.
        unsafe {
            use cxx_qt::casting::Upcast;
            let receiver: *mut cxx_qt::QObject =
                Upcast::<cxx_qt::QObject>::upcast_pin(self.as_mut()).get_unchecked_mut();
            qobject::register_url_handlers(receiver);
        }

        if let Some(ctx) = ctx {
            let queue = PumpQueue::new();
            #[cfg(debug_assertions)]
            if opts.dev_control {
                crate::dev::start(&ctx.app, thread.clone());
            }
            // Subscribe before the first queries go out, so no answer is missed.
            spawn_pump(&ctx.app, queue.clone(), thread);
            crate::desktop::start(&ctx.app, self.qt_thread());
            let desktop = ctx.app.desktop();
            let mut rust = self.as_mut().rust_mut();
            // `--background` hides the window only when the tray can bring it back; scripted
            // runs (screenshots, pages) always show it.
            rust.start_hidden = desktop.background()
                && desktop.is_enabled()
                && desktop.prefs().tray
                && opts.screenshot.is_none()
                && opts.page.is_none();
            rust.fixture = ctx.app.is_fixture();
            rust.pending_link = QString::from(ctx.app.pending_link().map_or("", |k| k.as_str()));
            rust.pump = Some(queue);
            rust.app = Some(ctx.app);
        }
        let first = shared.borrow_mut().source.reload();
        shared.borrow_mut().pending.merge(first);
        self.as_mut().apply_pending();

        let fixture = self.app.as_ref().and_then(|a| a.fixture());
        let media_conv = fixture.and_then(|f| f.media_conversation);
        match opts.page.as_deref() {
            // The account-state pages show the list with its banner; beside it (wide
            // windows) a chat, whose composer says when the account cannot send.
            Some(
                "chat" | "history" | "reactions" | "offline" | "loggedout" | "challenge" | "clock"
                | "clearchat" | "transferbanner" | "transferfailed" | "transferupdate" | "waban"
                | "waoff" | "wahistory" | "walost",
            ) => {
                if let Some(id) = demo {
                    self.as_mut().open_conversation(id.0);
                }
            }
            // An own message refused as a first contact, or one that waits for the lock to end,
            // in a chat of the list's lower body.
            Some("wafailed" | "walock") => {
                if let Some(id) = fixture.and_then(|f| f.whatsapp_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            // An own message with files in its upload, in a chat of the list's lower body.
            Some("uploading" | "uploadfile") => {
                if let Some(id) = fixture.and_then(|f| f.uploads_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("quotes" | "menu" | "react" | "edit" | "reply" | "delete") => {
                if let Some(id) = fixture.and_then(|f| f.quotes_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some(
                "album" | "albumviewer" | "revisions" | "mediamenu" | "deleteattachment" | "save",
            ) => {
                if let Some(id) = fixture.and_then(|f| f.albums_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("timer" | "timercustom") => {
                if let Some(id) = fixture.and_then(|f| f.timer_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("placeholders") => {
                if let Some(id) = fixture.and_then(|f| f.kinds_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("readmore") => {
                if let Some(id) = fixture.and_then(|f| f.readmore_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("calls") => {
                if let Some(id) = fixture.and_then(|f| f.calls_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("unavailable") => {
                if let Some(id) = fixture.and_then(|f| f.unavailable_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("person" | "safety" | "safetychanged" | "share" | "timerpending") => {
                if let Some(id) = fixture.and_then(|f| f.person_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("group") => {
                if let Some(id) = fixture.and_then(|f| f.group_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            // The albums chat (a 1:1 chat with many pictures); the panel opens at its media.
            Some("chatmedia") => {
                if let Some(id) = fixture.and_then(|f| f.albums_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some(
                page @ ("media" | "viewer" | "compose" | "files" | "originals" | "folder"
                | "folderfiles" | "folderzip" | "location" | "locationmany"
                | "locationnever"),
            ) => {
                if let Some(id) = media_conv {
                    self.as_mut().open_conversation(id.0);
                    self.as_mut().fixture_media(page, id);
                }
            }
            // Blended chats: the list of the fixture's three accounts, no chat open (the page
            // hides the second account in `MainWindow.qml` `accountsPageReady`).
            Some("accounts" | "accountsdrawer") => {}
            Some("link" | "qr" | "transferwait" | "transferdownload" | "transferread") => {
                let network = shared
                    .borrow()
                    .source
                    .networks()
                    .into_iter()
                    .filter(|n| n.available)
                    .map(|n| n.kind)
                    .find(|k| self.app.is_none() || *k == NetworkKind::Fake)
                    .unwrap_or(NetworkKind::Signal);
                if let Some(login) = pinned(self.login) {
                    login.start(&QString::from(network.as_str()));
                }
            }
            _ => {}
        }
    }
}

/// The name an account stands for in the list's title: its name, else its network's.
fn title_name(a: &AccountRow) -> &str {
    if a.name.is_empty() {
        &a.network_name
    } else {
        &a.name
    }
}

/// The list's title for the one account it shows: its name, else its network's (as several
/// shown accounts are named).
fn single_title(rows: &[AccountRow], id: AccountId) -> String {
    rows.iter()
        .find(|a| a.id == id)
        .map(|a| title_name(a).to_owned())
        .unwrap_or_default()
}

/// The list's title for several shown accounts: their names joined by " · " in panel order,
/// or `None` when two of them have the same name ("Andrii · Andrii" says nothing: the
/// short form, "2 accounts", is used).
pub(crate) fn joined_title(names: &[&str]) -> Option<String> {
    let mut seen = std::collections::BTreeSet::new();
    if names.is_empty() || !names.iter().all(|n| seen.insert(*n)) {
        return None;
    }
    Some(names.join(" · "))
}

impl qobject::App {
    /// The list's filter: the search and the view, and the hidden accounts of the choice (the
    /// one copy of the set, read at each call).
    fn filter(&self) -> ConversationFilter {
        ConversationFilter {
            hidden: self.shown.hidden().clone(),
            query: self.list_view.query.clone(),
            archived: self.list_view.archived,
        }
    }

    fn next_generation(mut self: Pin<&mut Self>) -> u64 {
        let mut rust = self.as_mut().rust_mut();
        rust.generation += 1;
        rust.generation
    }

    fn refresh_accounts(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let (mut rows, known) = {
            let shared = shared.borrow();
            (shared.source.accounts(), shared.source.accounts_known())
        };
        let has = !rows.is_empty();
        // The rules of the choice (baseline, a new account, the prune) are the view model's.
        let news = shared.borrow_mut().source.take_replace_news();
        let mut outcome = kourier_viewmodel::shown_accounts::Outcome::default();
        {
            let mut rust = self.as_mut().rust_mut();
            for n in news {
                match n {
                    ReplaceNews::Answered { old, new } => rust.shown.replace_answered(old, new),
                    ReplaceNews::Ended { old } => {
                        outcome.merge(rust.shown.replace_ended(&rows, old));
                    }
                }
            }
            outcome.merge(rust.shown.observe(&rows, known));
            // The first answer: a saved hidden set held the list empty until now.
            if known && !rust.accounts_known {
                rust.accounts_known = true;
                outcome.changed |= !rust.shown.hidden().is_empty();
            }
        }
        self.shown.fill_rows(&mut rows);
        self.as_mut().rust_mut().account_rows = rows.clone();
        let generation = self.as_mut().next_generation();
        if let Some(m) = pinned(self.accounts) {
            models::set_rows(m, generation, rows);
        }
        if *self.has_accounts() != has {
            self.as_mut().set_has_accounts(has);
        }
        self.as_mut().refresh_history_accounts();
        if outcome.save {
            self.save_hidden();
        }
        if outcome.changed || outcome.save {
            self.apply_choice();
        } else {
            // The title of a selected account follows its name, the counts its unread.
            self.refresh_choice_props();
        }
    }

    /// Puts what an operation of the choice did to work: saves the hidden accounts when they
    /// changed, then refreshes everything that shows the choice.
    fn choice_done(mut self: Pin<&mut Self>, outcome: kourier_viewmodel::shown_accounts::Outcome) {
        if outcome.save {
            self.save_hidden();
        }
        if outcome.changed || outcome.save {
            self.as_mut().apply_choice();
        }
    }

    /// Saves the hidden accounts in the preferences. The App is their one writer, through the
    /// desktop handle's `update_prefs` (as `networks-on`): the Settings page does not list
    /// them, so its next toggle keeps what is stored.
    fn save_hidden(&self) {
        let Some(desktop) = self.app.as_ref().map(|a| a.desktop()) else {
            return;
        };
        let hidden: Vec<String> = self.shown.hidden().iter().map(|u| u.to_string()).collect();
        desktop.update_prefs(|p| p.hidden_accounts = hidden);
    }

    /// The setting "Show chats of several accounts together" changed (queued by the Settings
    /// controller): turning it off with several shown shows all chats (his Q1).
    pub(crate) fn blend_setting_changed(mut self: Pin<&mut Self>, blend: bool) {
        let rows = self.account_rows.clone();
        let outcome = self.as_mut().rust_mut().shown.set_blend(&rows, blend);
        self.choice_done(outcome);
    }

    /// The account the list shows alone, if the choice singles one out (`-1`: none).
    fn current_choice(&self) -> Option<AccountId> {
        self.shown.current(&self.account_rows)
    }

    /// Puts the choice of shown accounts to work: the filter, the panel's roles (`shown`,
    /// `place`), `currentAccount`, `blend`, `allShown`, `shownCount`, `hiddenUnread`, the
    /// account actions, the title, the banner, the list and its archive count. ONE function
    /// after every change of the choice (L2).
    fn apply_choice(mut self: Pin<&mut Self>) {
        {
            let mut rust = self.as_mut().rust_mut();
            let rust: &mut AppRust = &mut rust;
            rust.shown.fill_rows(&mut rust.account_rows);
        }
        let rows = self.account_rows.clone();
        let generation = self.as_mut().next_generation();
        if let Some(m) = pinned(self.accounts) {
            models::set_rows(m, generation, rows);
        }
        self.as_mut().refresh_choice_props();
        self.refresh_conversations();
    }

    /// The properties that follow the choice and the accounts' rows, without the list.
    fn refresh_choice_props(mut self: Pin<&mut Self>) {
        let rows = &self.account_rows;
        let current = self.current_choice().map_or(-1, |a| a.0);
        let blend = self.shown.blend();
        let all = self.shown.all_shown(rows);
        let count = i32::try_from(self.shown.shown_count(rows)).unwrap_or(i32::MAX);
        let unread = i32::try_from(kourier_viewmodel::hidden_unread(rows)).unwrap_or(i32::MAX);
        if *self.current_account() != current {
            self.as_mut().set_current_account(current);
        }
        if *self.blend() != blend {
            self.as_mut().set_blend(blend);
        }
        if *self.shown_count() != count {
            self.as_mut().set_shown_count(count);
        }
        if *self.hidden_unread() != unread {
            self.as_mut().set_hidden_unread(unread);
        }
        // Last: the panel's tint reads it with the rest settled.
        if *self.all_shown() != all {
            self.as_mut().set_all_shown(all);
        }
        self.as_mut().refresh_account_actions();
        self.as_mut().refresh_title();
        self.refresh_banner();
    }

    /// The title of the list page: ONE function of the selection (all chats, or an account's
    /// name) and of whether the archive view is shown, used by every writer of `currentTitle`
    /// (the account choice, the rename of an account, the archive view): two writers that
    /// each knew half of it made the title stop saying "Archived Chats" after an account was
    /// picked in the archive view.
    fn list_title(&self) -> String {
        if *self.archive_shown() {
            return trc(
                "@title:window list header, the archived chats",
                "Archived Chats",
            )
            .into_owned();
        }
        let rows = &self.account_rows;
        if let Some(id) = self.current_choice() {
            return single_title(rows, id);
        }
        let all = || {
            trc(
                "@title:window list header, the chats of all accounts",
                "All Chats",
            )
            .into_owned()
        };
        if self.shown.all_shown(rows) {
            return all();
        }
        // Before the accounts answered, a saved choice that hides accounts: the title waits.
        if rows.is_empty() {
            return String::new();
        }
        let names: Vec<&str> = rows.iter().filter(|a| a.shown).map(title_name).collect();
        joined_title(&names).unwrap_or_else(|| self.short_title())
    }

    /// "%1 accounts" for the accounts shown now (the title's short form).
    fn short_title(&self) -> String {
        let count = u64::try_from(self.shown.shown_count(&self.account_rows)).unwrap_or(u64::MAX);
        kourier_viewmodel::i18n::trnc(
            "@title:window list header, how many accounts' chats it shows",
            "%1 account",
            "%1 accounts",
            count,
        )
    }

    /// Writes `currentTitle` from [`Self::list_title`] and `currentTitleShort` when they
    /// differ. The short form is there only while several accounts are shown and the archive
    /// view is not.
    fn refresh_title(mut self: Pin<&mut Self>) {
        let title = QString::from(self.list_title().as_str());
        if *self.current_title() != title {
            self.as_mut().set_current_title(title);
        }
        let rows = &self.account_rows;
        let several = !*self.archive_shown()
            && self.current_choice().is_none()
            && !rows.is_empty()
            && !self.shown.all_shown(rows);
        let short = if several {
            QString::from(self.short_title().as_str())
        } else {
            QString::default()
        };
        if *self.current_title_short() != short {
            self.as_mut().set_current_title_short(short);
        }
    }

    /// The banner above the conversation list: what the accounts shown need the user to know
    /// or do (offline, logged out, a challenge, a wrong clock).
    fn refresh_banner(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        // The states of the shown accounts and of hidden ones that need the user; the history
        // transfer chosen apart from the states (the state of one account does not hide the
        // transfer of another; a hidden account's transfer shows too).
        let banners = shared.borrow().source.banners(&self.filter());
        let banner = banners.state;
        let transfer = banners.transfer.and_then(|b| b.transfer);
        let placeholder = banners.shown_transfer.and_then(|b| b.transfer);
        if self.shown_placeholder_transfer != placeholder {
            self.as_mut()
                .set_banner_shown_transfer(transfer_variant(placeholder.as_ref()));
            self.as_mut().rust_mut().shown_placeholder_transfer = placeholder;
        }
        let (kind, text, action, account, url, note) = match banner {
            Some(b) => (
                b.kind.as_str(),
                b.text,
                b.action,
                b.account.0,
                b.url,
                (b.explanation, b.appeal_url, b.appeal_label),
            ),
            None => (
                "",
                String::new(),
                String::new(),
                0,
                String::new(),
                (String::new(), String::new(), String::new()),
            ),
        };
        if self.shown_transfer != transfer {
            self.as_mut()
                .set_banner_transfer(transfer_variant(transfer.as_ref()));
            self.as_mut().rust_mut().shown_transfer = transfer;
        }
        if *self.banner_text() != QString::from(text.as_str()) {
            self.as_mut().set_banner_text(QString::from(text.as_str()));
        }
        if *self.banner_action() != QString::from(action.as_str()) {
            self.as_mut()
                .set_banner_action(QString::from(action.as_str()));
        }
        if *self.banner_account() != account {
            self.as_mut().set_banner_account(account);
        }
        // The network's own words, its page and the button's label: before `kind`, so a
        // switch on the kind reads them whole.
        if *self.banner_explanation() != QString::from(note.0.as_str()) {
            self.as_mut()
                .set_banner_explanation(QString::from(note.0.as_str()));
        }
        if *self.banner_appeal_url() != QString::from(note.1.as_str()) {
            self.as_mut()
                .set_banner_appeal_url(QString::from(note.1.as_str()));
        }
        if *self.banner_appeal_label() != QString::from(note.2.as_str()) {
            self.as_mut()
                .set_banner_appeal_label(QString::from(note.2.as_str()));
        }
        self.as_mut().rust_mut().banner_url = url;
        // `kind` last: QML switches on it and then reads the rest.
        if *self.banner_kind() != QString::from(kind) {
            self.as_mut().set_banner_kind(QString::from(kind));
        }
    }

    /// The accounts that offer "Transfer History from Phone…" (the settings page lists them).
    fn refresh_history_accounts(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let offering: Vec<(i64, String)> = shared
            .borrow()
            .source
            .accounts()
            .into_iter()
            .filter(|a| a.offers_history)
            // The same line the question shows for the account.
            .map(|a| {
                let about = kourier_viewmodel::transfer::with_device(
                    &kourier_viewmodel::transfer::account_line(&a.name, &a.number, &a.network_name),
                    &a.device,
                );
                (a.id.0, about)
            })
            .collect();
        if self.shown_history == offering {
            return;
        }
        let mut list = QList::<QVariant>::default();
        let label = kourier_viewmodel::transfer::transfer_action_label();
        for (account, about) in &offering {
            let mut m = QMap::<QMapPair_QString_QVariant>::default();
            m.insert(QString::from("label"), string_variant(&label));
            m.insert(QString::from("account"), QVariant::from(account));
            m.insert(QString::from("about"), string_variant(about));
            list.append(QVariant::from(&m));
        }
        self.as_mut().rust_mut().shown_history = offering;
        self.as_mut().set_history_accounts(QVariant::from(&list));
    }

    /// Every account with what Settings → "Accounts" offers for it: a map of `account`,
    /// `about` (which account it is), `transfer` and `unlink` (whether "Transfer History from
    /// Phone…" and "Unlink…" are offered; "Remove Account…" always is) and the three labels
    /// `transferLabel`, `removeLabel`, `unlinkLabel`.
    fn refresh_account_actions(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let panel = self.account_rows.clone();
        let rows: Vec<ShownActions> = shared
            .borrow()
            .source
            .accounts()
            .into_iter()
            .map(|a| {
                let about = kourier_viewmodel::transfer::with_device(
                    &kourier_viewmodel::transfer::account_line(&a.name, &a.number, &a.network_name),
                    &a.device,
                );
                let shown = self.shown.is_shown(a.uuid);
                let last = self.shown.is_last_shown(&panel, a.uuid);
                (
                    a.id.0,
                    about,
                    a.offers_history,
                    a.offers_unlink,
                    shown,
                    last,
                )
            })
            .collect();
        if self.shown_actions == rows {
            return;
        }
        let transfer = kourier_viewmodel::transfer::transfer_action_label();
        let remove = kourier_viewmodel::account_actions::remove_action_label();
        let unlink = kourier_viewmodel::account_actions::unlink_action_label();
        let mut list = QList::<QVariant>::default();
        for (account, about, offers_history, offers_unlink, shown, last) in &rows {
            let mut m = QMap::<QMapPair_QString_QVariant>::default();
            m.insert(QString::from("account"), QVariant::from(account));
            m.insert(QString::from("about"), string_variant(about));
            m.insert(QString::from("transfer"), QVariant::from(offers_history));
            m.insert(QString::from("unlink"), QVariant::from(offers_unlink));
            m.insert(QString::from("shown"), QVariant::from(shown));
            m.insert(QString::from("lastShown"), QVariant::from(last));
            m.insert(QString::from("transferLabel"), string_variant(&transfer));
            m.insert(QString::from("removeLabel"), string_variant(&remove));
            m.insert(QString::from("unlinkLabel"), string_variant(&unlink));
            list.append(QVariant::from(&m));
        }
        self.as_mut().rust_mut().shown_actions = rows;
        self.as_mut().set_account_actions(QVariant::from(&list));
    }

    /// The history transfer of an account changed: the banner, which accounts offer a
    /// transfer, and what is said once about an end.
    fn refresh_transfer(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        self.as_mut().refresh_history_accounts();
        self.as_mut().refresh_account_actions();
        self.as_mut().refresh_banner();
        let notices = shared.borrow_mut().source.take_transfer_notices();
        for text in notices {
            self.as_mut().transfer_notice(QString::from(text.as_str()));
        }
    }

    fn refresh_conversations(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        // Until the list is on screen its refreshes are counted and timed (`refreshes`,
        // `refresh_ms` of `startup.list_populated`).
        let span = if self.trace.list_done {
            None
        } else {
            trace::Span::begin()
        };
        let filter = self.filter();
        let rows = shared.borrow().source.conversations(&filter);
        if trace::enabled() && !self.trace.list_done && !rows.is_empty() {
            // The first rows: `startup.list_populated` is the frame that shows them.
            self.as_mut().rust_mut().trace.list_rows = i32::try_from(rows.len()).ok();
            self.as_mut().set_trace_frame(true);
        }
        let generation = self.as_mut().next_generation();
        if let Some(m) = pinned(self.conversations) {
            models::set_rows(m, generation, rows);
        }
        if let Some(span) = span {
            let mut rust = self.as_mut().rust_mut();
            rust.trace.list_refreshes += 1;
            rust.trace.list_refresh_ms += span.elapsed_ms();
        }
        let (unread, archived) = {
            let shared = shared.borrow();
            (
                shared.source.unread_total(),
                shared.source.archived_count(&filter),
            )
        };
        let archived = i32::try_from(archived).unwrap_or(i32::MAX);
        if *self.archived_count() != archived {
            self.as_mut().set_archived_count(archived);
        }
        self.update_unread(unread);
    }

    /// The badge and the tray follow the unread count.
    fn update_unread(mut self: Pin<&mut Self>, unread: u32) {
        let count = i32::try_from(unread).unwrap_or(i32::MAX);
        if *self.unread_count() == count {
            return;
        }
        self.as_mut().set_unread_count(count);
        // Only with desktop services: offscreen runs and tests never touch the session bus.
        if let Some(desktop) = self.app.as_ref().map(|a| a.desktop())
            && desktop.is_enabled()
        {
            qobject::set_badge_number(i64::from(unread));
            desktop.set_unread(unread);
        }
    }

    fn refresh_timeline(mut self: Pin<&mut Self>, switched: bool) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let Some(timeline) = pinned(self.timeline) else {
            return;
        };
        let mut id = ConversationId(*timeline.conversation_id());
        let (header, rows, paging) = {
            let state = shared.borrow();
            if id.0 != 0 && !state.source.is_open(id) {
                // The source closed it (deleted, or removed with its account).
                id = ConversationId(0);
            }
            match id.0 {
                0 => (None, Vec::new(), Default::default()),
                _ => (
                    state.source.chat(id),
                    state.source.timeline(id),
                    state.source.paging(),
                ),
            }
        };
        if id.0 == 0
            && let Some(c) = pinned(self.conversations)
        {
            c.set_current_id(0);
        }
        if let Some(open) = self.trace.open
            && open.conversation == id.0
            && open.rows.is_none()
            && !rows.is_empty()
        {
            // The rows of the conversation being opened: `timeline.open` ends with the frame
            // that shows them.
            let rows = (
                i32::try_from(rows.len()).unwrap_or(i32::MAX),
                open.span.elapsed_ms(),
            );
            self.as_mut().rust_mut().trace.open = Some(OpenTrace {
                rows: Some(rows),
                ..open
            });
            self.as_mut().set_trace_frame(true);
        }
        let generation = self.as_mut().next_generation();
        let Some(mut timeline) = pinned(self.timeline) else {
            return;
        };
        timeline.as_mut().set_header(id.0, header.as_ref());
        timeline.as_mut().set_paging(paging);
        if switched {
            // A different conversation: one reset, not a diff of unrelated rows.
            models::apply(timeline.as_mut(), &[ListOp::Reset], &rows);
            let count = i32::try_from(rows.len()).unwrap_or(i32::MAX);
            timeline.as_mut().store_mut().generation = generation;
            timeline.as_mut().update_count(count);
        } else {
            models::set_rows(timeline, generation, rows);
        }
        // What the chat's network says about files reaches the composer with the header.
        if let Some(c) = pinned(self.composer) {
            c.refresh_rules();
        }
        // The picture viewer follows the rows (a download that finishes, a message that goes).
        self.timeline_rows_changed();
    }

    /// The per-second tick of the lines under the messages on their way: only those rows
    /// change (their `wait` and `waitTimer` roles), the timeline is not built again.
    fn refresh_waits(self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let Some(mut timeline) = pinned(self.timeline) else {
            return;
        };
        let id = ConversationId(*timeline.conversation_id());
        let lines = shared.borrow().source.wait_lines(id);
        for (message, line) in lines {
            let Some(at) = timeline
                .row_of(message)
                .and_then(|i| usize::try_from(i).ok())
            else {
                continue;
            };
            let Some(mut row) = timeline.rows().get(at).cloned() else {
                continue;
            };
            row.wait = line.reason;
            row.wait_timer = line.timer;
            models::patch_row(timeline.as_mut(), at, row);
        }
    }

    /// The rings on the files of the own messages that wait or upload: only those rows change
    /// (their `blocks` role), the timeline is not built again and nothing is created anew
    /// (the blocks of a row are bound to the role).
    fn refresh_uploads(self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let Some(mut timeline) = pinned(self.timeline) else {
            return;
        };
        let id = ConversationId(*timeline.conversation_id());
        let patched = shared.borrow().source.upload_rows(id, timeline.rows());
        for (at, row) in patched {
            models::patch_row(timeline.as_mut(), at, row);
        }
    }

    fn refresh_paging(self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let paging = shared.borrow().source.paging();
        if let Some(t) = pinned(self.timeline) {
            t.set_paging(paging);
        }
    }

    fn show_login(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let view = shared.borrow().source.login_view();
        let Some(view) = view else { return };
        let auto = self.auto_submit_login && view.kind == LoginViewKind::Input;
        if let Some(login) = pinned(self.login) {
            login.show(&view);
        }
        if auto {
            // `--page=qr`: accept the suggested device name so the QR code shows.
            self.as_mut().rust_mut().auto_submit_login = false;
            if let Some(login) = pinned(self.login) {
                login.submit();
            }
        }
    }

    /// Applies the refreshes commands and events left pending.
    pub fn apply_pending(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let ch = std::mem::take(&mut shared.borrow_mut().pending);
        if ch.merged {
            // Merges (§3.3), of the open chat or of any other: the files that waited in the
            // composer of a chat that went wait for the chat that is left, and two lists
            // become one.
            let merges = shared.borrow_mut().source.take_merges();
            for (from, into) in merges {
                if let Some(c) = pinned(self.composer) {
                    c.merge_conversations(from.0, into.0);
                }
            }
        }
        if let Some(into) = ch.switch_to {
            // The open chat continues in the surviving conversation.
            self.as_mut().open_conversation(into.0);
        }
        if ch.settings {
            let (policy, quiet) = {
                let shared = shared.borrow();
                (shared.source.expiry_policy(), shared.source.quiet_notices())
            };
            if let Some(mut s) = pinned(self.settings) {
                s.as_mut().show_expiry_policy(policy);
                s.show_quiet_notices(quiet);
            }
        }
        if ch.accounts {
            self.as_mut().refresh_accounts();
        }
        if ch.conversations {
            self.as_mut().refresh_conversations();
        }
        if let Some(moved) = ch.archive_move {
            self.as_mut().archive_moved(moved);
        }
        if ch.timeline || ch.header {
            self.as_mut().refresh_timeline(false);
        } else {
            self.as_mut().refresh_paging();
            if ch.waits {
                self.as_mut().refresh_waits();
            }
            if ch.uploads {
                self.as_mut().refresh_uploads();
            }
        }
        if ch.login {
            self.as_mut().show_login();
        }
        if ch.transfer || ch.login {
            // (A login step can end what the link page shows of a transfer.)
            self.as_mut().refresh_transfer();
        }
        if ch.account_notice {
            let notices = shared.borrow_mut().source.take_logout_notices();
            for n in notices {
                self.as_mut().logout_notice(
                    QString::from(n.text.as_str()),
                    QString::from(n.on_phone.as_str()),
                    QString::from(n.title.as_str()),
                );
            }
        }
        if ch.notice {
            let notice = shared.borrow().source.notice();
            if let Some(c) = pinned(self.composer) {
                c.set_notice(QString::from(notice.as_str()));
            }
        }
        if ch.media {
            let answers = shared.borrow_mut().source.take_media();
            self.as_mut().deliver_media(answers);
        }
        if ch.saves {
            let answers = shared.borrow_mut().source.take_saves();
            self.as_mut().deliver_saves(answers);
        }
        if ch.files {
            // Files of a `SendFiles` the core did not take go back into the composer (with
            // the quote, when the whole request came back); what is still being prepared
            // for sending may have changed too.
            let open = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
            // (Every lot that came back: a send that went out again without its refused
            // files may have been refused once more since the last look.)
            loop {
                let refused = shared.borrow_mut().source.take_refused_files();
                let (Some(refused), Some(c)) = (refused, pinned(self.composer)) else {
                    break;
                };
                c.restore_refused(refused, open);
            }
            // Sends the core answered: the pasted files made for them can go.
            let finished = shared.borrow_mut().source.take_finished_sends();
            if let Some(mut c) = pinned(self.composer) {
                c.as_mut().sends_finished(&finished);
                c.refresh_preparing();
            }
        }
        if ch.plan {
            // What the core makes of the composer's files: the split and the refusals.
            let answer = shared.borrow_mut().source.take_file_plan();
            if let (Some(answer), Some(c)) = (answer, pinned(self.composer)) {
                c.plan_ready(&answer);
            }
            let failed = shared.borrow_mut().source.take_plan_failure();
            if let (Some(serial), Some(c)) = (failed, pinned(self.composer)) {
                c.plan_failed(serial);
            }
        }
        if ch.revisions {
            let list = shared.borrow_mut().source.take_revisions();
            if let Some(list) = list {
                let rows = models::timeline::revisions_variant(&list);
                self.as_mut().set_revisions(rows);
                self.as_mut().revisions_loaded(list.message.0);
            }
        }
        if ch.person {
            match shared.borrow_mut().source.take_person() {
                Some(PersonUpdate::Show(view)) => {
                    self.as_mut().put_person(Some(&view));
                    self.as_mut().person_loaded();
                }
                Some(PersonUpdate::Ended) => {
                    self.as_mut().put_person(None);
                    self.as_mut().person_ended();
                }
                None => {}
            }
        }
        if ch.safety {
            let view = shared.borrow_mut().source.take_safety();
            if let Some(view) = view {
                // The map and the held view change together: what is on screen is what a
                // button acts on. A view that comes while no level is open (the level was
                // closed inside this very drain, by a binding of an earlier arm, and the
                // source's close is still queued) is dropped (Y4).
                let shown = self.as_mut().rust_mut().safety_level.receive(view);
                if let Some(map) = shown {
                    self.as_mut().set_safety(map);
                    self.as_mut().safety_loaded();
                }
            }
        }
        if ch.group {
            match shared.borrow_mut().source.take_group() {
                Some(GroupUpdate::Show(view)) => {
                    self.as_mut().show_group(*view);
                    self.as_mut().group_loaded();
                }
                Some(GroupUpdate::Ended) => {
                    self.as_mut().clear_group();
                    self.as_mut().group_ended();
                }
                None => {}
            }
        }
        if ch.chat_media {
            // The gallery slots that changed (people pages P4).
            let updates = shared.borrow_mut().source.take_chat_media();
            for update in updates {
                let slot = update.slot;
                if let Some(m) = pinned(self.chat_media[slot.index()]) {
                    m.set_media(update);
                }
                // (A viewer opened from a square of that slot follows its message through this.)
                self.as_mut().gallery_changed(QString::from(slot.name()));
            }
        }
        if let Some(message) = ch.jump
            && let Some(timeline) = pinned(self.timeline)
            && let Some(row) = timeline.row_of(message)
        {
            // The rows are in place: the view scrolls to the message and highlights it.
            timeline.jump_requested(message.0, row);
        }
        if ch.draft {
            let open = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
            if open != 0
                && let Some(c) = pinned(self.composer)
            {
                c.draft_loaded(open);
            }
        }
        let (loaded, timers) = {
            let st = shared.borrow();
            (st.source.loaded(), st.source.has_timers())
        };
        if *self.loaded() != loaded {
            self.as_mut().set_loaded(loaded);
        }
        if *self.timers_active() != timers {
            self.as_mut().set_timers_active(timers);
        }
        self.arm_flags();
    }

    /// Schedules one wake-up for the instant the first edit or delete window of a loaded
    /// message ends: the rows' action flags are read again then (the core sends no event for
    /// time). No polling: nothing runs until that instant.
    fn arm_flags(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let deadline = shared.borrow().source.flags_deadline();
        let Some(deadline) = deadline else {
            self.as_mut().rust_mut().flags_armed = None;
            return;
        };
        // A reload reports the same window end a few milliseconds apart: one wake-up serves.
        let close = |a: Instant, b: Instant| {
            a.max(b).duration_since(a.min(b)) < Duration::from_secs(1) && a >= Instant::now()
        };
        if self.flags_armed.is_some_and(|armed| close(armed, deadline)) {
            return;
        }
        let Some(runtime) = self.app.as_ref().map(|a| a.runtime().clone()) else {
            return;
        };
        self.as_mut().rust_mut().flags_armed = Some(deadline);
        let thread = self.qt_thread();
        runtime.spawn(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            if thread.queue(|app| app.flags_due()).is_err() {
                tracing::debug!("App destroyed before an action window ended");
            }
        });
    }

    /// An action window may have ended: re-read the open timeline's flags if so.
    pub fn flags_due(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        self.as_mut().rust_mut().flags_armed = None;
        let ch = shared.borrow_mut().source.refresh_flags();
        shared.borrow_mut().pending.merge(ch);
        // Re-arms for what is left (an earlier wake-up of a deadline that moved).
        self.apply_pending();
    }

    /// Runs on the Qt thread for each posted drain: hands queued core events to the source for
    /// at most [`DRAIN_BUDGET`], refreshes the models once, and re-posts itself while events
    /// remain (§8.5).
    pub fn drain_events(mut self: Pin<&mut Self>) {
        let (Some(queue), Some(shared)) = (self.pump.clone(), self.shared.clone()) else {
            return;
        };
        let mut changes = Changes::default();
        let mut lagged = false;
        let outcome = {
            let mut state = shared.borrow_mut();
            queue.drain(DRAIN_BUDGET, |item| match item {
                PumpItem::Event(e) => changes.merge(state.source.on_event(&e)),
                PumpItem::Lagged(n) => {
                    tracing::warn!(lost = n, "UI events lagged; reloading the visible models");
                    lagged = true;
                    changes.merge(state.source.reload());
                }
            })
        };
        shared.borrow_mut().pending.merge(changes);
        self.as_mut().apply_pending();
        if lagged {
            // `OpenMedia` answers may be among the lost events: the waiting views are told
            // (they can ask again) instead of waiting forever.
            let lost = self.as_mut().rust_mut().media.lost();
            self.as_mut().media_unanswered(lost);
        }
        if outcome == Drained::More {
            let thread = self.qt_thread();
            if thread.queue(|app| app.drain_events()).is_err() {
                tracing::debug!("App destroyed while events were pending");
            }
        }
    }

    /// A click on an account shows only its conversations (`-1` = all chats).
    pub fn select_account(mut self: Pin<&mut Self>, account: i64) {
        let rows = self.account_rows.clone();
        let outcome = {
            let mut rust = self.as_mut().rust_mut();
            if account >= 0 {
                rust.shown.click(&rows, AccountId(account))
            } else {
                rust.shown.show_all()
            }
        };
        if outcome.save {
            self.save_hidden();
        }
        self.apply_choice();
    }

    /// Shift/Ctrl+click, Ctrl+Space, the box, the menu item: adds or removes the account
    /// (with blending off: a click). False when the last shown account stays.
    pub fn toggle_account(mut self: Pin<&mut Self>, account: i64) -> bool {
        let rows = self.account_rows.clone();
        let outcome = self
            .as_mut()
            .rust_mut()
            .shown
            .toggle(&rows, AccountId(account));
        self.choice_done(outcome);
        !outcome.last_stays
    }

    /// The open chat changed sides in the last read of the list (Signal Desktop's rules): one
    /// that left the archive (a new message brought it back) takes the list back to the chats;
    /// one that the phone archived closes (a narrow window then goes back to the list: the
    /// window follows the open chat going away).
    fn archive_moved(self: Pin<&mut Self>, moved: ArchiveMove) {
        match moved {
            ArchiveMove::ToInbox => self.show_archive(false),
            ArchiveMove::ToArchive => self.close_conversation(),
        }
    }

    /// A chat was picked from the list: when it is an archived chat found by a search, the
    /// list shows the archive with that chat in it (the chat is open and highlighted). Called
    /// for a pick of the list and of the quick switch; a notification opens its chat and leaves
    /// the list as it is. Before the search is emptied: it is what lists archived chats.
    pub fn reveal_chat(mut self: Pin<&mut Self>, conversation: i64) {
        if *self.archive_shown() {
            return;
        }
        let Some(shared) = self.shared.clone() else {
            return;
        };
        // Only a SHOWN account's chat: the archive view lists the shown accounts' archived chats
        // alone, so a hidden account's match opens and the list stays as chosen (H3).
        let archived = shared
            .borrow()
            .source
            .conversations(&self.filter())
            .iter()
            .any(|r| {
                r.id.0 == conversation
                    && r.archived
                    && self
                        .account_rows
                        .iter()
                        .any(|a| a.id == r.account && a.shown)
            });
        if archived {
            self.as_mut().show_archive(true);
        }
    }

    /// Shows the archive (`shown`) or the chat list again. The archive has no search field, so
    /// showing it empties the search; the account selection stays.
    pub fn show_archive(mut self: Pin<&mut Self>, shown: bool) {
        if *self.archive_shown() == shown {
            return;
        }
        // The filter first: `archiveShown` makes the page empty its search field, whose
        // `setSearch("")` then finds the query empty already and reads nothing, so the list
        // is read once (not as the inbox, then as the archive).
        {
            let mut rust = self.as_mut().rust_mut();
            rust.list_view.archived = shown;
            if shown {
                rust.list_view.query.clear();
            }
        }
        self.as_mut().set_archive_shown(shown);
        self.as_mut().refresh_title();
        self.refresh_conversations();
    }

    /// The first frame is on screen: backends may connect now (§8.2 step 4).
    pub fn ready(mut self: Pin<&mut Self>) {
        if self.ready_sent {
            return;
        }
        self.as_mut().rust_mut().ready_sent = true;
        trace::mark_with_cpu("startup.first_frame", &[]);
        if let Some(app) = &self.app
            && app.send(UiCommand::UiReady).is_err()
        {
            tracing::warn!("the core stopped before the UI was ready");
        }
        // A run from the build directory has no icon in the theme: its windows get the
        // bundled one now, after the frame the start is measured by.
        crate::kde::set_fallback_window_icon();
        // What the start allocated and freed again (QML compilation, the first rows) goes
        // back to the system once things are quiet, not only when the window is hidden
        // (architecture §1.3: idle memory with the window shown).
        if let Some(runtime) = self.app.as_ref().map(|a| a.runtime().clone()) {
            let thread = self.qt_thread();
            runtime.spawn(async move {
                tokio::time::sleep(STARTUP_TRIM_AFTER).await;
                let _ = thread.queue(|_| {
                    qobject::trim_memory();
                    trace::mark_with_cpu("startup.trimmed", &[]);
                });
            });
        }
    }

    /// A frame was presented while marks wait for one: writes those whose rows were on
    /// screen in it (`startup.list_populated`, `timeline.open`).
    pub fn frame_traced(mut self: Pin<&mut Self>, list_shown: bool, chat_shown: bool) {
        let mut waiting = false;
        if let Some(rows) = self.trace.list_rows {
            if list_shown {
                trace::mark(
                    "startup.list_populated",
                    &[
                        ("rows", f64::from(rows)),
                        ("refreshes", f64::from(self.trace.list_refreshes)),
                        ("refresh_ms", self.trace.list_refresh_ms),
                    ],
                );
                let mut rust = self.as_mut().rust_mut();
                rust.trace.list_rows = None;
                rust.trace.list_done = true;
            } else {
                waiting = true;
            }
        }
        if let Some(open) = self.trace.open {
            match open.rows {
                Some((rows, query_ms)) if chat_shown => {
                    open.span.end(
                        "timeline.open",
                        &[("query_ms", query_ms), ("rows", f64::from(rows))],
                    );
                    self.as_mut().rust_mut().trace.open = None;
                }
                _ => waiting = true,
            }
        }
        if !waiting {
            self.set_trace_frame(false);
        }
    }

    /// Writes a timing mark from QML (`key=number` fields; see `kourier_app::trace`).
    pub fn trace_mark(&self, name: &QString, fields: &QString) {
        if !trace::enabled() {
            return;
        }
        let fields = fields.to_string();
        let fields: Vec<(&str, f64)> = fields
            .split_whitespace()
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((key, value.parse().ok()?))
            })
            .collect();
        // With the CPU time and the heap so far: the marks of a scripted session say what
        // each of its steps cost.
        trace::mark_with_cpu(&name.to_string(), &fields);
    }

    /// The conversation at `row` of the filtered list (0 when there is none).
    pub fn conversation_at(&self, row: i32) -> i64 {
        let (Some(shared), Ok(row)) = (self.shared.as_ref(), usize::try_from(row)) else {
            return 0;
        };
        shared
            .borrow()
            .source
            .conversations(&self.filter())
            .get(row)
            .map_or(0, |r| r.id.0)
    }

    /// The conversation `step` rows from `from` in the filtered list (0 when there is none).
    pub fn adjacent_conversation(&self, from: i64, step: i32) -> i64 {
        let Some(shared) = self.shared.as_ref() else {
            return 0;
        };
        let rows = shared.borrow().source.conversations(&self.filter());
        let len = i64::try_from(rows.len()).unwrap_or(i64::MAX);
        let target = match rows.iter().position(|r| r.id.0 == from) {
            Some(at) => i64::try_from(at).unwrap_or(i64::MAX) + i64::from(step),
            None if step < 0 => len - 1,
            None => 0,
        };
        usize::try_from(target)
            .ok()
            .and_then(|row| rows.get(row))
            .map_or(0, |r| r.id.0)
    }

    /// Loads the page before the oldest loaded row of the open conversation.
    pub fn fetch_older(self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if shared.borrow_mut().source.fetch_older() {
            self.refresh_paging();
        }
    }

    /// Loads the page after the newest loaded row of the open conversation.
    pub fn fetch_newer(self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if shared.borrow_mut().source.fetch_newer() {
            self.refresh_paging();
        }
    }

    /// Window focus and whether the chat page is on screen (notifications, read markers).
    pub fn set_presence(mut self: Pin<&mut Self>, focused: bool, chat_visible: bool) {
        self.as_mut().rust_mut().presence = (focused, chat_visible);
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if let Some(app) = &self.app {
            app.desktop().set_focused(focused);
        }
        let ch = shared
            .borrow_mut()
            .source
            .set_presence(focused, chat_visible);
        shared.borrow_mut().pending.merge(ch);
        self.apply_pending();
    }

    /// What the window last said with [`Self::set_presence`]: 0 nothing, 1 it has the focus,
    /// 2 its chat page is on screen, 3 both.
    pub fn presence(&self) -> i32 {
        i32::from(self.presence.0) + 2 * i32::from(self.presence.1)
    }

    /// The window is being let go after a minute hidden (§8.2 step 6): nothing needs to be
    /// fast for a while, so the core gives back what it only keeps for speed, the page
    /// caches and prepared statements of its database connections (§1.3: up to 8 MiB each,
    /// in the heaps of the core's threads). It reads them again, bit by bit, when something
    /// is next asked of it: an activation costs its first queries a few disk reads and no
    /// more. Sent now, before the window is gone, so that the trim in
    /// [`Self::window_released`] finds that memory free. Returns whether the core was asked.
    pub fn window_releasing(self: Pin<&mut Self>) -> bool {
        let Some(app) = &self.app else {
            return false;
        };
        let asked = app.send(UiCommand::ReleaseMemory).is_ok();
        if !asked {
            tracing::debug!("the core stopped before the window was released");
        }
        asked
    }

    /// The window that was destroyed after a minute hidden (§8.2 step 6) is gone: the freed
    /// heap goes back to the system. Presence is not touched here. The shell said "nothing
    /// is on screen" when it let the window go, and the user may have come back since: a
    /// reset at this point would leave a new, focused window counted as absent (its chat
    /// notified about and not marked read) until its focus next changed.
    pub fn window_released(self: Pin<&mut Self>) {
        let later = self
            .app
            .as_ref()
            .map(|a| (a.runtime().clone(), self.qt_thread()));
        qobject::trim_memory();
        trace::mark_with_cpu("window.released", &[]);
        if let Some((runtime, thread)) = later {
            // Once more a little later. The core frees its caches on its own threads, after
            // the queries and writes that were running ([`Self::window_releasing`] does not
            // wait for it), and the event loop goes on deleting what the window owned for a
            // moment (`destroy()` only schedules that): what is freed after the first trim
            // would otherwise stay with the process for as long as it sits in the tray.
            runtime.spawn(async move {
                tokio::time::sleep(RELEASE_SETTLE).await;
                let _ = thread.queue(|_| {
                    qobject::trim_memory();
                    trace::mark_with_cpu("window.released_settled", &[]);
                });
            });
        }
        tracing::info!("window released while hidden");
    }

    /// Expires timed state (typing indicators).
    pub fn tick(self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let ch = shared.borrow_mut().source.tick();
        shared.borrow_mut().pending.merge(ch);
        self.apply_pending();
    }

    /// Opens `conversation` in the chat page.
    pub fn open_conversation(mut self: Pin<&mut Self>, conversation: i64) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let switched = pinned(self.timeline).is_some_and(|t| *t.conversation_id() != conversation);
        if let Some(t) = pinned(self.timeline) {
            t.set_conversation_id(conversation);
        }
        if let Some(c) = pinned(self.conversations) {
            c.set_current_id(conversation);
        }
        if switched {
            self.as_mut().close_kept();
            if let Some(c) = pinned(self.composer) {
                c.cancel_edit();
            }
            if let Some(c) = pinned(self.composer) {
                c.cancel_reply();
            }
            // (The files waiting in the composer stay with their chat: `set_conversation`.)
        }
        if let Some(c) = pinned(self.composer) {
            c.set_conversation(conversation);
        }
        if switched && conversation != 0 {
            // `timeline.open` (§1.3): from here to the frame that shows the rows.
            self.as_mut().rust_mut().trace.open = trace::Span::begin().map(|span| OpenTrace {
                conversation,
                span,
                rows: None,
            });
        }
        let ch = shared
            .borrow_mut()
            .source
            .open_conversation(Some(ConversationId(conversation)));
        self.as_mut().refresh_timeline(switched);
        shared.borrow_mut().pending.merge(ch);
        // The list shows the draft of the conversation just left.
        shared.borrow_mut().pending.conversations = true;
        self.apply_pending();
    }

    /// Closes the chat page's conversation.
    pub fn close_conversation(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        self.as_mut().close_kept();
        let ch = shared.borrow_mut().source.open_conversation(None);
        if let Some(c) = pinned(self.conversations) {
            c.set_current_id(0);
        }
        if let Some(c) = pinned(self.composer) {
            c.set_conversation(0);
        }
        if let Some(mut t) = pinned(self.timeline) {
            t.as_mut().set_header(0, None);
            t.as_mut().set_paging(Default::default());
            models::apply(t.as_mut(), &[ListOp::Reset], &[]);
            t.update_count(0);
        }
        shared.borrow_mut().pending.merge(Changes {
            timeline: false,
            header: false,
            ..ch
        });
        self.as_mut().apply_pending();
    }

    /// Filters the conversation list by `query`.
    pub fn set_search(mut self: Pin<&mut Self>, query: &QString) {
        let q = query.to_string();
        if self.list_view.query != q {
            self.as_mut().rust_mut().list_view.query = q;
            self.refresh_conversations();
        }
    }

    /// Downloads an attachment (or retries a failed download).
    pub fn download_attachment(self: Pin<&mut Self>, attachment: i64) {
        if attachment <= 0 {
            return;
        }
        if let Some(shared) = self.shared.clone() {
            shared
                .borrow_mut()
                .source
                .download_attachment(AttachmentId(attachment));
        }
    }

    /// Asks for a plaintext copy of `attachment`; returns the request token (answered with
    /// `mediaOpened`/`mediaFailed`), 0 when there is no core.
    pub fn open_media(mut self: Pin<&mut Self>, attachment: i64) -> i64 {
        // No attachment (fixture rows): nothing to ask.
        if attachment <= 0 {
            return 0;
        }
        let id = AttachmentId(attachment);
        let Some(shared) = self.shared.clone() else {
            return 0;
        };
        if !shared.borrow_mut().source.open_media(id) {
            return 0;
        }
        self.as_mut().rust_mut().media.request(id)
    }

    /// The view of the `openMedia` request `token` went.
    pub fn release_media(mut self: Pin<&mut Self>, token: i64) {
        if let Some(id) = self.as_mut().rust_mut().media.release(token)
            && let Some(shared) = self.shared.clone()
        {
            shared.borrow_mut().source.close_media(id);
        }
    }

    /// Opens `attachment` with the desktop's default application once its copy is ready.
    pub fn open_externally(mut self: Pin<&mut Self>, attachment: i64) {
        if attachment <= 0 {
            return;
        }
        let id = AttachmentId(attachment);
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if shared.borrow_mut().source.open_media(id) {
            self.as_mut().rust_mut().media.request_external(id);
        }
    }

    /// Asks the name `attachment` is offered under in the "Save As" dialog; answered with
    /// `saveSuggested`.
    pub fn ask_save(self: Pin<&mut Self>, attachment: i64) -> bool {
        if attachment <= 0 {
            return false;
        }
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        shared
            .borrow_mut()
            .source
            .ask_save(AttachmentId(attachment))
    }

    /// Saves `attachment` as the file `url` the dialog answered with (it asked about replacing).
    /// The name goes on EXACTLY as the dialog gave it: the question about replacing was about
    /// that name, so cleaning it here and writing the result with `Replace` could overwrite
    /// another file the dialog never asked about. Only the writer judges it
    /// (`kourier_blob::check_name`: one component, not empty, not `.` or `..`, at most 255
    /// bytes), and a refusal ends in a failed notice.
    pub fn save_to(self: Pin<&mut Self>, attachment: i64, url: &QString) {
        let Some(path) = crate::controllers::local_path(url) else {
            return;
        };
        let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
            return;
        };
        let name = name.to_string_lossy().into_owned();
        if attachment <= 0 {
            return;
        }
        if let Some(shared) = self.shared.clone() {
            shared
                .borrow_mut()
                .source
                .save_to(AttachmentId(attachment), dir.to_path_buf(), name);
        }
    }

    /// Saves the attachments of `attachments` (numbers, comma separated) into the folder `folder`.
    pub fn save_all(self: Pin<&mut Self>, attachments: &QString, folder: &QString) {
        let Some(dir) = crate::controllers::local_path(folder) else {
            return;
        };
        let ids: Vec<AttachmentId> = attachments
            .to_string()
            .split(',')
            .filter_map(|n| n.trim().parse::<i64>().ok())
            .filter(|n| *n > 0)
            .map(AttachmentId)
            .collect();
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.save_all(ids, dir);
        }
    }

    /// Asks where the folder dialog of "Save All…" starts: answered with `saveFolderReady(url)`
    /// (the last folder a save wrote into, else Downloads, else home). The look at the file
    /// system is made on a thread: a remembered folder on a network share that is gone must not
    /// freeze the window.
    pub fn ask_save_folder(self: Pin<&mut Self>) {
        let thread = self.qt_thread();
        kourier_app::show::start_folder_then(self.last_save_folder(), move |folder| {
            let url = file_url(&folder);
            let _ = thread.queue(move |app| app.save_folder_ready(QString::from(url.as_str())));
        });
    }

    /// A debug build may name another QML file for the save dialog (`KOURIER_DEV_SAVE_DIALOG`,
    /// the window tests: a native dialog cannot be driven offscreen); empty: the built-in one.
    pub fn save_dialog_source(&self) -> QString {
        #[cfg(debug_assertions)]
        if let Some(path) = std::env::var_os("KOURIER_DEV_SAVE_DIALOG") {
            return QString::from(file_url(Path::new(&path)).as_str());
        }
        QString::default()
    }

    /// "Show in Folder": the saved file `url` shown selected in the file manager.
    pub fn show_in_folder(&self, url: &QString) {
        let Some(path) = crate::controllers::local_path(url) else {
            return;
        };
        let shower = *FOLDER_SHOWER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shower(&path);
    }

    /// The folder the last save wrote into (a preference), as the dialog's start is looked for
    /// from it. Read from memory; whether it is still a folder is looked at on a thread.
    fn last_save_folder(&self) -> Option<String> {
        self.app
            .as_ref()
            .and_then(|a| a.desktop().prefs().last_save_folder)
    }

    /// Hands the answers to the save calls to QML: the offered name as the dialog's start, and
    /// how a save or a batch ended. The folder of a save that wrote a file is the next
    /// dialog's start (a preference, written here only).
    fn deliver_saves(mut self: Pin<&mut Self>, answers: Vec<SaveAnswer>) {
        for answer in answers {
            match answer {
                SaveAnswer::Suggested { attachment, name } => {
                    // The name is here; the folder is looked for on a thread (a dead share must
                    // not freeze the window), and the dialog is told when both are known.
                    let thread = self.qt_thread();
                    kourier_app::show::start_folder_then(self.last_save_folder(), move |folder| {
                        let url = file_url(&folder.join(name));
                        let _ = thread.queue(move |app| {
                            app.save_suggested(attachment.0, QString::from(url.as_str()));
                        });
                    });
                }
                SaveAnswer::Done(done) => {
                    let url = if done.saved > 0 {
                        if let Some(app) = self.app.as_ref()
                            && let Some(dir) = done.dir.to_str()
                        {
                            let dir = dir.to_owned();
                            app.desktop()
                                .update_prefs(|p| p.last_save_folder = Some(dir));
                        }
                        file_url(&done.dir.join(&done.name))
                    } else {
                        String::new()
                    };
                    let reason = done.failure.map_or("", save_reason);
                    self.as_mut().save_finished(
                        i32::try_from(done.saved).unwrap_or(i32::MAX),
                        i32::try_from(done.failed).unwrap_or(i32::MAX),
                        QString::from(done.name.as_str()),
                        QString::from(url.as_str()),
                        QString::from(reason),
                    );
                }
            }
        }
    }

    /// The full-size URL of the newest picture in the open timeline, or empty.
    pub fn latest_image(&self) -> QString {
        let url = pinned(self.timeline).and_then(|t| {
            t.rows()
                .iter()
                .filter(|r| r.kind == RowKind::Message)
                .flat_map(|r| r.blocks.iter())
                .flat_map(Block::images)
                .find(|i| !i.full_url.is_empty())
                .map(|i| i.full_url.clone())
        });
        QString::from(url.unwrap_or_default().as_str())
    }

    /// Runs `f` on the source and applies what it changed.
    fn with_source(self: Pin<&mut Self>, f: impl FnOnce(&mut dyn UiSource) -> Changes) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let ch = f(shared.borrow_mut().source.as_mut());
        shared.borrow_mut().pending.merge(ch);
        self.apply_pending();
    }

    /// The composer shows the source's notice (set or cleared by a command).
    fn notice_changed() -> Changes {
        Changes {
            notice: true,
            ..Changes::default()
        }
    }

    /// The composer shows the source's notice, and the pages of the details panel read their
    /// timer failure again (a new change forgets the last one: F4).
    fn timer_changed() -> Changes {
        Changes {
            notice: true,
            person: true,
            group: true,
            ..Changes::default()
        }
    }

    /// Adds our reaction `emoji` to `message`, or with `remove` takes it back.
    pub fn react(self: Pin<&mut Self>, message: i64, emoji: &QString, remove: bool) {
        if message <= 0 {
            return;
        }
        let emoji = emoji.to_string();
        self.with_source(|s| {
            s.react(MessageId(message), &emoji, remove);
            Self::notice_changed()
        });
    }

    /// Deletes `message`, for everyone or on this account only.
    pub fn delete_message(self: Pin<&mut Self>, message: i64, for_everyone: bool) {
        if message <= 0 {
            return;
        }
        self.with_source(|s| {
            s.delete_message(MessageId(message), for_everyone);
            Self::notice_changed()
        });
    }

    /// The message the composer's notice offers "Delete for Everyone" for; 0 for none. Only
    /// while the composer SHOWS the source's note: the composer sets notices of its own (a
    /// folder added, a strip it cannot send) without telling the source, and the action must
    /// not ride on those (upload progress fix U1).
    pub fn notice_message(&self) -> i64 {
        let shown = pinned(self.composer).map(|c| String::from(c.notice()));
        self.shared
            .as_ref()
            .and_then(|s| {
                let source = &s.borrow().source;
                (shown.as_deref() == Some(source.notice().as_str()))
                    .then(|| source.notice_message())
                    .flatten()
            })
            .map_or(0, |m| m.0)
    }

    /// Sends a failed own message again.
    pub fn retry_message(self: Pin<&mut Self>, message: i64) {
        if message <= 0 {
            return;
        }
        self.with_source(|s| {
            s.retry_message(MessageId(message));
            Self::notice_changed()
        });
    }

    /// Takes back an own message nobody has yet.
    pub fn cancel_message(self: Pin<&mut Self>, message: i64) {
        if message <= 0 {
            return;
        }
        self.with_source(|s| {
            s.cancel_message(MessageId(message));
            Self::notice_changed()
        });
    }

    /// Sets the open conversation's disappearing-message timer (`0` = off).
    pub fn set_timer(self: Pin<&mut Self>, seconds: i32) {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        let Ok(secs) = u32::try_from(seconds) else {
            return;
        };
        if conv == 0 {
            return;
        }
        self.with_source(|s| {
            s.set_timer(ConversationId(conv), secs);
            Self::timer_changed()
        });
    }

    /// Sets the disappearing-message timer of conversation `conversation` (`0` = off): the
    /// details panel's pages act on their own chat, never on the open one.
    pub fn set_timer_of(self: Pin<&mut Self>, conversation: i64, seconds: i32) {
        let Ok(secs) = u32::try_from(seconds) else {
            return;
        };
        if conversation <= 0 {
            return;
        }
        self.with_source(|s| {
            s.set_timer(ConversationId(conversation), secs);
            Self::timer_changed()
        });
    }

    /// Deletes every message of the open conversation for this account.
    pub fn clear_conversation(self: Pin<&mut Self>) {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        if conv == 0 {
            return;
        }
        self.with_source(|s| {
            s.clear_conversation(ConversationId(conv));
            Self::notice_changed()
        });
    }

    /// Deletes the open conversation for this account.
    pub fn delete_conversation(mut self: Pin<&mut Self>) {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        if conv == 0 {
            return;
        }
        let mut accepted = false;
        self.as_mut().with_source(|s| {
            accepted = s.delete_conversation(ConversationId(conv));
            Changes {
                // (The list no longer shows its draft.)
                conversations: true,
                ..Self::notice_changed()
            }
        });
        // What waited in its composer goes with the chat, but only once the delete was
        // accepted: a refused one (the chat fills, a transfer runs) leaves the chat and
        // everything the user put into its composer where it was.
        if accepted && let Some(c) = pinned(self.composer) {
            c.forget_conversation(conv);
        }
    }

    /// Marks `conversation` read up to its newest message (any chat, focused or not).
    pub fn mark_read(self: Pin<&mut Self>, conversation: i64) {
        if conversation <= 0 {
            return;
        }
        // (The list and the badges follow the core's `ConversationsChanged`.)
        self.with_source(|s| {
            s.mark_read(ConversationId(conversation));
            Changes::default()
        });
    }

    /// Removes one attachment of a message for this account.
    pub fn delete_attachment(self: Pin<&mut Self>, attachment: i64) {
        if attachment <= 0 {
            return;
        }
        self.with_source(|s| {
            s.delete_attachment(AttachmentId(attachment));
            Self::notice_changed()
        });
    }

    /// What the picture viewer shows for `message`, from the rows as they are now: a list, empty
    /// when nothing of the message is a picture any more AND when the message is not among the
    /// loaded rows (never an invalid value: `viewerMessageLoaded` tells the two apart).
    pub fn viewer_items(&self, message: i64) -> QVariant {
        pinned(self.timeline)
            .and_then(|t| {
                t.rows()
                    .iter()
                    .find(|r| message > 0 && r.message_id() == Some(MessageId(message)))
                    .map(models::timeline::viewer_items)
            })
            .unwrap_or_else(models::timeline::no_viewer_items)
    }

    /// Whether `message` is among the loaded rows of the open conversation.
    pub fn viewer_message_loaded(&self, message: i64) -> bool {
        message > 0
            && pinned(self.timeline).is_some_and(|t| {
                t.rows()
                    .iter()
                    .any(|r| r.message_id() == Some(MessageId(message)))
            })
    }

    /// What the picture viewer shows for `message` as the gallery slot `slot` has its rows now.
    pub fn gallery_viewer_items(&self, slot: &QString, message: i64) -> QVariant {
        let Some(slot) = MediaSlot::from_name(&slot.to_string()) else {
            return models::timeline::no_viewer_items();
        };
        pinned(self.chat_media[slot.index()])
            .map(|m| models::timeline::gallery_viewer_items(&m.store().view, MessageId(message)))
            .unwrap_or_else(models::timeline::no_viewer_items)
    }

    /// Whether the gallery slot `slot` has had its first answer since it was opened (a viewer
    /// that reads its own slot ignores the slot until then: what it opened with is shown).
    pub fn gallery_loaded(&self, slot: &QString) -> bool {
        MediaSlot::from_name(&slot.to_string())
            .and_then(|s| pinned(self.chat_media[s.index()]))
            .is_some_and(|m| *m.loaded())
    }

    /// Opens the picture viewer's own query for the pictures of `message`, a message among the
    /// rows of the slot `from` (`group`, `person` or `media`): the whole album, read AT that message,
    /// kept up to date as the chat changes (`galleryChanged("viewer")`), whatever the rows of `from`
    /// do. What the viewer showed before is dropped at once. `false` when `from` holds no such row.
    /// A call that finds the source borrowed repeats from the event loop unless another call about
    /// the viewer's slot was made meanwhile (`SlotCalls`).
    pub fn show_gallery_viewer(mut self: Pin<&mut Self>, from: &QString, message: i64) -> bool {
        let Some(from) = MediaSlot::from_name(&from.to_string()) else {
            return false;
        };
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if message <= 0 || from == MediaSlot::Viewer {
            return false;
        }
        let number = self.as_mut().rust_mut().slot_calls.begin(MediaSlot::Viewer);
        self.as_mut().empty_media_slot(MediaSlot::Viewer);
        if shared.try_borrow_mut().is_err() {
            let _ = self.qt_thread().queue(move |app| {
                if app.slot_calls.is_latest(MediaSlot::Viewer, number)
                    && let Some(shared) = app.shared.clone()
                {
                    shared
                        .borrow_mut()
                        .source
                        .open_media_viewer(from, MessageId(message));
                }
            });
            return true;
        }
        shared
            .borrow_mut()
            .source
            .open_media_viewer(from, MessageId(message))
    }

    /// CPU time the Qt thread has used so far (ms).
    pub fn thread_cpu_ms(&self) -> f64 {
        trace::cpu_ms().1
    }

    /// Shows `message` of the open conversation (scrolls to it once it is among the rows).
    pub fn jump_to(self: Pin<&mut Self>, message: i64) {
        if message <= 0 {
            return;
        }
        self.with_source(|s| s.jump_to(MessageId(message)));
    }

    /// Loads the newest messages of a window that ends before them.
    pub fn jump_to_latest(self: Pin<&mut Self>) {
        self.with_source(|s| s.jump_to_latest());
    }

    /// Puts `text` on the clipboard.
    pub fn copy_text(&self, text: &QString) {
        qobject::set_clipboard_text(text);
    }

    /// "Copy Link" of the person's page on screen (P5b): puts the link Rust holds for that page
    /// on the clipboard. The URL holds a third party's number: it never passes through QML (the
    /// page's map has no link) and it is copied only on this click. `false` when the page on
    /// screen has no link (nothing copied).
    pub fn copy_person_link(&self) -> bool {
        match &self.person_link {
            Some(link) => {
                qobject::set_clipboard_text(&QString::from(link.expose().as_str()));
                true
            }
            None => false,
        }
    }

    /// "Share Contact…" (P5c): the share picker's rows, the chats of the shown person's account
    /// a card may go to ([`kourier_viewmodel::share::listed_targets`]), the search emptied.
    /// `false` (rows emptied) when the page on screen offers no card.
    pub fn prepare_share(self: Pin<&mut Self>) -> bool {
        let who = self.person_share.clone();
        let all = match (&who, self.shared.clone()) {
            (Some(_), Some(shared)) => shared.borrow().source.share_targets(),
            _ => Vec::new(),
        };
        let account = who.as_ref().map_or(AccountId(0), |(a, _)| *a);
        if let Some(mut m) = pinned(self.share_targets) {
            m.as_mut().set_filter_text(&QString::default());
            m.set_targets(account, all);
        }
        who.is_some()
    }

    /// "Send" of the share picker (P5c): a card of the person on screen into `conversation`, a
    /// chat the picker lists (the same account: a card never leaves through another one). The
    /// core builds the card from the contact row and checks everything again.
    pub fn share_contact(mut self: Pin<&mut Self>, conversation: i64) -> bool {
        let Some((account, user)) = self.person_share.clone() else {
            return false;
        };
        let listed = pinned(self.share_targets).is_some_and(|m| m.is_listed(conversation));
        if !listed {
            return false;
        }
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        let sent =
            shared
                .borrow_mut()
                .source
                .send_contact(ConversationId(conversation), account, user);
        let ch = Self::notice_changed();
        shared.borrow_mut().pending.merge(ch);
        self.as_mut().apply_pending();
        sent
    }

    /// The person's page: its map for QML and, kept in Rust beside it, its link (P5b). `None`
    /// empties both.
    fn put_person(mut self: Pin<&mut Self>, view: Option<&kourier_viewmodel::person::PersonView>) {
        self.as_mut().rust_mut().person_link = view.and_then(|v| v.link.clone());
        self.as_mut().rust_mut().person_share = view
            .filter(|v| v.share_shown && v.share_note.is_empty())
            .map(|v| (v.account, v.user.clone()));
        let map = match view {
            Some(v) => models::timeline::person_variant(v),
            None => QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()),
        };
        self.as_mut().set_person(map);
    }

    /// Opens `url` with the desktop's program for it (`xdg-open`), started with the environment
    /// the person gave Kourier: the child's own copy lacks the variables Kourier set for Qt
    /// ([`crate::env_set_by_kourier`]); this process's environment is not touched. Also the slot
    /// of Qt's URL handlers (`registerUrlHandlers`). Never calls `QDesktopServices::openUrl`: Qt
    /// would hand the URL to its own services, which inherit the process's environment. Only the
    /// Qt thread calls it.
    pub fn open_external_url(&self, url: &QUrl) -> bool {
        let url = String::from(&qobject::url_for_the_opener(url));
        let Some(command) = kourier_app::opener::command(&url, &crate::env_set_by_kourier()) else {
            tracing::warn!("a URL without a scheme was not opened");
            return false;
        };
        let launcher = *URL_LAUNCHER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        launcher(command)
    }

    /// The banner's button: link again, solve the challenge, or restart the account.
    pub fn banner_activate(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let account = AccountId(*self.banner_account());
        let kind = self.banner_kind().to_string();
        if kind == BannerKind::LoggedOut.as_str() {
            let view = shared.borrow_mut().source.relink(account);
            match view {
                Ok(view) => {
                    // The link page is about the account's network (its icon and its steps),
                    // and its "Try again" starts a login on that network, which the source
                    // turns into linking the same account again.
                    let network = shared
                        .borrow()
                        .source
                        .accounts()
                        .iter()
                        .find(|a| a.id == account)
                        .map(|a| a.network);
                    if let Some(mut login) = pinned(self.login) {
                        if let Some(network) = network {
                            login.as_mut().set_network(QString::from(network.as_str()));
                        }
                        login.show(&view);
                    }
                    self.as_mut().relink_started();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "linking again could not start");
                    if let Some(c) = pinned(self.composer) {
                        c.set_notice(QString::from(
                            trc("@info:status", "Linking again could not be started.").as_ref(),
                        ));
                    }
                }
            }
        } else if kind == BannerKind::Challenge.as_str() {
            // QML hands the URL to the browser; what the browser gives back comes in through
            // `submitChallenge` (the dialog's field, or a `signalcaptcha://` link).
            let url = QString::from(self.banner_url.as_str());
            self.as_mut().challenge_requested(account.0, url);
        } else if kind == BannerKind::NetworkOff.as_str() {
            // "Turn On": the preference, and Kourier starts again (no link page opens).
            let network = shared
                .borrow()
                .source
                .accounts()
                .iter()
                .find(|a| a.id == account)
                .map(|a| a.network);
            if let Some(network) = network {
                self.turn_on_network_and_restart(&QString::from(network.as_str()), false);
            }
        } else if kind == BannerKind::Error.as_str() {
            self.with_source(|s| {
                s.restart_account(account);
                Changes {
                    accounts: true,
                    ..Changes::default()
                }
            });
        }
    }

    /// The warning before a NEW account of `kind` is linked, as QML reads it (an empty map
    /// when the network asks nothing first).
    pub fn network_risk(&self, kind: &QString) -> QVariant {
        let kind = kind.to_string();
        let view = self.shared.clone().and_then(|shared| {
            shared
                .borrow()
                .source
                .networks()
                .into_iter()
                .find(|n| n.kind.as_str() == kind)
                .map(|mut n| {
                    // The PREFERENCE decides, not this start's registry: a network switched
                    // off in Settings and not yet restarted is offered as off ("Turn On and
                    // Restart"), or the account linked now would be paused at the next start
                    // (K-A fix U7).
                    if let Some(app) = &self.app
                        && n.experimental
                        && !app.desktop().prefs().network_on(n.kind)
                    {
                        n.on = false;
                    }
                    n
                })
                .and_then(|n| n.risk_view())
        });
        let mut m = QMap::<QMapPair_QString_QVariant>::default();
        if let Some(v) = view {
            m.insert(QString::from("title"), string_variant(&v.title));
            let mut paragraphs = QList::<QVariant>::default();
            for p in &v.paragraphs {
                paragraphs.append(string_variant(p));
            }
            m.insert(QString::from("paragraphs"), QVariant::from(&paragraphs));
            m.insert(QString::from("confirm"), string_variant(&v.confirm));
            m.insert(QString::from("accept"), string_variant(&v.accept));
            m.insert(QString::from("cancel"), string_variant(&v.cancel));
            m.insert(QString::from("acceptHint"), string_variant(&v.accept_hint));
            m.insert(
                QString::from("restartNote"),
                string_variant(&v.restart_note),
            );
        }
        QVariant::from(&m)
    }

    /// Turns the experimental network `kind` on in the preferences and restarts Kourier; with
    /// `link`, its link page opens once, right after the restart ("Turn On and Restart").
    /// An unknown name changes nothing.
    pub fn turn_on_network_and_restart(mut self: Pin<&mut Self>, kind: &QString, link: bool) {
        let kind = kind.to_string();
        let Some(kind) = kourier_model::NetworkKind::ALL
            .into_iter()
            .find(|k| k.as_str() == kind)
        else {
            return;
        };
        if let Some(app) = &self.app {
            app.desktop().update_prefs(|p| {
                p.set_network_on(kind, true);
                if link {
                    p.pending_link = Some(kind.as_str().to_owned());
                }
            });
        }
        self.as_mut().restart_now();
    }

    /// The network whose link page opens now, or empty (see the bridge).
    pub fn take_pending_link(mut self: Pin<&mut Self>) -> QString {
        std::mem::take(&mut self.as_mut().rust_mut().pending_link)
    }

    /// Restarts Kourier ("Restart Now"): the window quits (`restartRequested`) and `main`
    /// starts the process anew once the core is down.
    pub fn restart_now(mut self: Pin<&mut Self>) {
        if let Some(app) = &self.app {
            app.request_restart();
        }
        self.as_mut().restart_requested();
    }

    fn transfer_changed() -> Changes {
        Changes {
            transfer: true,
            ..Changes::default()
        }
    }

    /// "Continue Without History".
    pub fn skip_transfer(self: Pin<&mut Self>, account: i64) {
        self.with_source(|s| {
            s.skip_transfer(AccountId(account));
            Changes::default()
        });
    }

    /// "Dismiss" on the banner of a failed history transfer.
    pub fn dismiss_transfer(self: Pin<&mut Self>, account: i64) {
        self.with_source(|s| {
            if s.dismiss_transfer(AccountId(account)) {
                Self::transfer_changed()
            } else {
                Changes::default()
            }
        });
    }

    /// Asks for the question before `account` is removed and linked again.
    pub fn ask_replace(self: Pin<&mut Self>, account: i64) {
        let question = self
            .shared
            .clone()
            .and_then(|shared| shared.borrow_mut().source.ask_replace(AccountId(account)));
        if let Some(q) = question {
            self.replace_asked(question_variant(&q));
        }
    }

    /// The question before an action that would cancel the link in progress, or an invalid
    /// value (`undefined` in QML) when nothing would be lost.
    pub fn abandon_question(&self) -> QVariant {
        abandon_variant(
            self.shared
                .clone()
                .and_then(|shared| shared.borrow().source.abandon_question()),
        )
    }

    /// The question before the application quits; `undefined` when nothing runs, and while the
    /// desktop session ends: the work goes on at the next start, and a question would hold
    /// the logout.
    pub fn quit_question(&self) -> QVariant {
        if crate::bridge::qobject::session_ending() {
            return abandon_variant(None);
        }
        abandon_variant(
            self.shared
                .clone()
                .and_then(|shared| shared.borrow().source.quit_question()),
        )
    }

    /// The question was closed without confirming.
    pub fn cancel_replace(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.cancel_replace();
        }
    }

    /// Asks for the question before "Remove Account…" or "Unlink…" of `account`.
    pub fn ask_account_action(self: Pin<&mut Self>, account: i64, action: &QString) {
        let Some(action) = account_action(action) else {
            return;
        };
        let question = self.shared.clone().and_then(|shared| {
            shared
                .borrow_mut()
                .source
                .ask_account_action(AccountId(account), action)
        });
        if let Some(q) = question {
            let mut m = question_variant_map(&q);
            m.insert(
                QString::from("action"),
                QVariant::from(&QString::from(account_action_name(action))),
            );
            self.account_action_asked(QVariant::from(&m));
        }
    }

    /// The user confirmed the question about `account` and `action`.
    pub fn confirm_account_action(self: Pin<&mut Self>, account: i64, action: &QString) {
        let (Some(shared), Some(action)) = (self.shared.clone(), account_action(action)) else {
            return;
        };
        let sent = shared
            .borrow_mut()
            .source
            .confirm_account_action(AccountId(account), action);
        if !sent {
            // No open question (a second activation), or the account went away or can no
            // longer do it: nothing was sent.
            tracing::debug!(?action, "no account action sent");
        }
    }

    /// The question about an account action was closed without confirming.
    pub fn cancel_account_action(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.cancel_account_action();
        }
    }

    /// The user confirmed the question about `account`: it is removed and linked again.
    pub fn confirm_replace(mut self: Pin<&mut Self>, account: i64) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let view = shared
            .borrow_mut()
            .source
            .confirm_replace(AccountId(account));
        match view {
            Ok(view) => {
                let network = shared
                    .borrow()
                    .source
                    .login_network()
                    .map(|n| QString::from(n.as_str()));
                if let Some(mut login) = pinned(self.login) {
                    if let Some(network) = network {
                        login.as_mut().set_network(network);
                    }
                    login.show(&view);
                }
                // The replaced account's choice waits for the new account of this login. A
                // replace still open before it ends here: its prune is saved and applied.
                let outcome = {
                    let mut rust = self.as_mut().rust_mut();
                    let rows = rust.account_rows.clone();
                    rust.shown.replace_begin(&rows, AccountId(account))
                };
                self.as_mut().choice_done(outcome);
                self.as_mut().replace_started();
                shared.borrow_mut().pending.merge(Self::transfer_changed());
                self.apply_pending();
            }
            // No open question (a second activation), the account went away or its network
            // cannot be linked in this build: nothing was sent. (Or the core is gone
            // between the removal and the new link, at shutdown: then nothing follows.)
            Err(e) => tracing::debug!(error = %e, "no account replaced"),
        }
    }

    /// Hands the network the answer to the challenge of `account`.
    pub fn submit_challenge(self: Pin<&mut Self>, account: i64, answer: &QString) {
        let answer = zeroize::Zeroizing::new(answer.to_string());
        self.with_source(|s| {
            s.submit_challenge(AccountId(account), &answer);
            Self::notice_changed()
        });
    }

    /// Asks for the versions of an edited message (`revisionsLoaded` announces them).
    pub fn show_revisions(mut self: Pin<&mut Self>, message: i64) -> bool {
        if message <= 0 {
            return false;
        }
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        // An earlier history is not shown under a new question.
        self.as_mut()
            .set_revisions(QVariant::from(&QList::<QVariant>::default()));
        shared
            .borrow_mut()
            .source
            .request_revisions(MessageId(message))
    }

    /// Asks for the page of the open 1:1 chat's person (`personLoaded` announces it).
    pub fn show_person_of_chat(mut self: Pin<&mut Self>) -> bool {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if conv == 0 {
            return false;
        }
        let header = shared.borrow().source.chat(ConversationId(conv));
        let Some((account, peer)) = header.and_then(|h| h.peer.map(|p| (h.account, p))) else {
            return false;
        };
        // An earlier page is not shown under a new question.
        self.as_mut().put_person(None);
        // The chat the panel opens from is the page's origin: it ends with that chat.
        shared
            .borrow_mut()
            .source
            .request_person(account, peer, Some(ConversationId(conv)))
    }

    /// The person's page is no longer on screen.
    pub fn close_person(mut self: Pin<&mut Self>) {
        // Nothing to copy once the page is gone (the map stays: a Back may show it again,
        // and the next answer brings the link back with it).
        self.as_mut().rust_mut().person_link = None;
        self.as_mut().rust_mut().person_share = None;
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.close_person();
        }
    }

    /// Asks for the page of a member of the open group (`personLoaded` announces it). The open
    /// chat is the page's origin: it ends with that chat, and with nothing else of the person's.
    pub fn show_person(mut self: Pin<&mut Self>, account: i64, user: &QString) -> bool {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        // The id came from a member row of the page; a string that is no id asks nothing.
        let Ok(user) = UserId::parse(&user.to_string()) else {
            return false;
        };
        if conv == 0 {
            return false;
        }
        // An earlier page is not shown under a new question (a stale map would read as loaded).
        self.as_mut().put_person(None);
        shared.borrow_mut().source.request_person(
            AccountId(account),
            user,
            Some(ConversationId(conv)),
        )
    }

    /// Asks for the page of `user` of the open chat's account (a sender in a group, a mention):
    /// the open chat is the page's origin, as for a member's row.
    pub fn show_person_in_chat(self: Pin<&mut Self>, user: &QString) -> bool {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if conv == 0 {
            return false;
        }
        let Some(account) = shared
            .borrow()
            .source
            .chat(ConversationId(conv))
            .map(|h| h.account)
        else {
            return false;
        };
        self.show_person(account.0, user)
    }

    /// Asks for the page of the open group (`groupLoaded` announces it).
    pub fn show_group_of_chat(mut self: Pin<&mut Self>) -> bool {
        let conv = pinned(self.timeline).map_or(0, |t| *t.conversation_id());
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if conv == 0 {
            return false;
        }
        let header = shared.borrow().source.chat(ConversationId(conv));
        let Some(account) = header
            .filter(|h| h.has_details && h.is_group)
            .map(|h| h.account)
        else {
            return false;
        };
        // An earlier page is not shown under a new question.
        self.as_mut().clear_group();
        shared
            .borrow_mut()
            .source
            .request_group(account, ConversationId(conv))
    }

    /// Puts a group's page on screen: its member rows go to the two member models (which diff
    /// them against the rows held), and `group` is set only when the rest of the page differs
    /// from what it holds (P2b: a refresh that changes one member must not make every binding
    /// on `App.group` read the page again).
    fn show_group(mut self: Pin<&mut Self>, view: kourier_viewmodel::group::GroupView) {
        let preview_rows = view.members_preview;
        let (members, rest) = kourier_viewmodel::group::split_page(view, self.group_shown.as_ref());
        let count = members.len();
        let preview = members[..preview_rows.min(count)].to_vec();
        if let Some(m) = pinned(self.group_preview) {
            m.set_members(preview);
        }
        if let Some(m) = pinned(self.group_members) {
            m.set_members(members);
        }
        if let Some(rest) = rest {
            let page = models::timeline::group_variant(&rest);
            self.as_mut().set_group(page);
            self.as_mut().rust_mut().group_shown = Some(rest);
        }
    }

    /// No group's page is on screen: the page and both member models are empty.
    fn clear_group(mut self: Pin<&mut Self>) {
        if let Some(m) = pinned(self.group_preview) {
            m.set_members(Vec::new());
        }
        if let Some(m) = pinned(self.group_members) {
            m.set_members(Vec::new());
        }
        self.as_mut().rust_mut().group_shown = None;
        self.set_group(QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()));
    }

    /// The group's page is no longer on screen.
    pub fn close_group(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.close_group();
        }
    }

    /// Asks for the safety number of `user` of `account` (`safetyLoaded` announces it).
    pub fn show_safety(mut self: Pin<&mut Self>, account: i64, user: &QString) -> bool {
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        // The id came from the person's page; a string that is no id asks nothing.
        let Ok(user) = UserId::parse(&user.to_string()) else {
            return false;
        };
        // An earlier level is not shown (or acted on) under a new question; the new one takes
        // answers from now on.
        self.as_mut().clear_safety();
        self.as_mut().rust_mut().safety_level.begin();
        if shared.try_borrow_mut().is_err() {
            // (Called while the source is busy, from a binding run by `apply_pending`: asked
            // from the event loop.)
            let _ = self.qt_thread().queue(move |app| {
                if let Some(shared) = app.shared.clone() {
                    shared
                        .borrow_mut()
                        .source
                        .request_safety(AccountId(account), user);
                }
            });
            return true;
        }
        let asked = shared
            .borrow_mut()
            .source
            .request_safety(AccountId(account), user);
        if !asked {
            // Nothing was asked: no level will come.
            self.as_mut().rust_mut().safety_level.end();
        }
        asked
    }

    /// The level and the view a button would act on are gone, and no answer is taken until the
    /// next level is asked for.
    fn clear_safety(mut self: Pin<&mut Self>) {
        self.as_mut().rust_mut().safety_level.end();
        self.set_safety(QVariant::from(&QMap::<QMapPair_QString_QVariant>::default()));
    }

    /// The safety-number level is no longer on screen.
    pub fn close_safety(mut self: Pin<&mut Self>) {
        self.as_mut().clear_safety();
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if let Ok(mut state) = shared.try_borrow_mut() {
            state.source.close_safety();
            return;
        }
        let _ = self.qt_thread().queue(move |app| {
            if let Some(shared) = app.shared.clone() {
                shared.borrow_mut().source.close_safety();
            }
        });
    }

    /// A button of the level on screen was pressed: it acts on the person and the key of the
    /// number on screen (`safety_level`), and only with a button that level offers.
    pub fn safety_action(self: Pin<&mut Self>, action: &QString) -> bool {
        let Some(action) = kourier_viewmodel::safety::SafetyAction::from_name(&action.to_string())
        else {
            return false;
        };
        let (Some(shared), Some((account, user, key))) =
            (self.shared.clone(), self.safety_level.target(action))
        else {
            return false;
        };
        let Ok(mut state) = shared.try_borrow_mut() else {
            return false;
        };
        state.source.safety_action(account, user, action, key)
    }

    /// Whether a member row matches what was typed into the members search.
    pub fn member_matches(&self, name: &QString, query: &QString) -> bool {
        kourier_viewmodel::group::member_matches(&name.to_string(), &query.to_string())
    }

    /// Shows the gallery of the chat `conversation` of `account` in the slot `slot` (people pages
    /// P4). A slot that showed another chat is emptied at once, so no picture of one chat stands
    /// under the title of another while the core answers. A section asks from a binding that
    /// follows the page's map, which is set while the source is borrowed (`apply_pending`): then
    /// the ask is made again from the event loop, once the source is free, unless another call
    /// about the slot was made meanwhile (`SlotCalls`: a late call never undoes a later one).
    pub fn show_chat_media(
        mut self: Pin<&mut Self>,
        slot: &QString,
        account: i64,
        conversation: i64,
    ) -> bool {
        let name = slot.to_string();
        let Some(slot) = kourier_viewmodel::chat_media::MediaSlot::from_name(&name) else {
            return false;
        };
        if slot == MediaSlot::Viewer {
            // (The viewer's own query starts at a message: `show_gallery_viewer`.)
            return false;
        }
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if conversation <= 0 {
            self.close_media_slot(slot);
            return false;
        }
        let number = self.as_mut().rust_mut().slot_calls.begin(slot);
        if shared.try_borrow_mut().is_err() {
            let _ = self.qt_thread().queue(move |mut app| {
                if app.slot_calls.is_latest(slot, number) {
                    app.as_mut().apply_show_media(slot, account, conversation);
                }
            });
            return true;
        }
        self.apply_show_media(slot, account, conversation)
    }

    /// The ask of `show_chat_media`, with the source free.
    fn apply_show_media(
        mut self: Pin<&mut Self>,
        slot: kourier_viewmodel::chat_media::MediaSlot,
        account: i64,
        conversation: i64,
    ) -> bool {
        let Some(shared) = self.shared.clone() else {
            return false;
        };
        if self.chat_media_convs[slot.index()] != conversation {
            if let Some(m) = pinned(self.chat_media[slot.index()]) {
                m.clear_media();
            }
            self.as_mut().rust_mut().chat_media_convs[slot.index()] = conversation;
        }
        let opened = shared.borrow_mut().source.open_chat_media(
            slot,
            AccountId(account),
            ConversationId(conversation),
        );
        if !opened {
            self.close_media_slot(slot);
        }
        opened
    }

    /// Asks for the next page of a slot of the level of all media.
    pub fn more_chat_media(self: Pin<&mut Self>, slot: &QString) {
        let name = slot.to_string();
        let (Some(slot), Some(shared)) = (
            kourier_viewmodel::chat_media::MediaSlot::from_name(&name),
            self.shared.clone(),
        ) else {
            return;
        };
        let Ok(mut state) = shared.try_borrow_mut() else {
            let _ = self.qt_thread().queue(move |app| {
                app.more_chat_media(&QString::from(name.as_str()));
            });
            return;
        };
        state.source.more_chat_media(slot);
    }

    /// A slot (`all`: every one) shows nothing any more.
    pub fn close_chat_media(mut self: Pin<&mut Self>, slot: &QString) {
        let name = slot.to_string();
        if name == "all" {
            // (The pages' places: the viewer's slot lives as long as the viewer, which closes it.)
            for slot in MediaSlot::ALL.into_iter().filter(|s| s.is_page_slot()) {
                self.as_mut().close_media_slot(slot);
            }
        } else if let Some(slot) = kourier_viewmodel::chat_media::MediaSlot::from_name(&name) {
            self.close_media_slot(slot);
        }
    }

    /// The slot shows nothing and the source is told. The model is emptied at once; when the source
    /// is borrowed (a handler that runs inside `apply_pending`) the source is told from the event
    /// loop, and the model is emptied again then (an answer delivered in between filled it), unless
    /// another call about the slot was made meanwhile (`SlotCalls`).
    fn close_media_slot(mut self: Pin<&mut Self>, slot: kourier_viewmodel::chat_media::MediaSlot) {
        let number = self.as_mut().rust_mut().slot_calls.begin(slot);
        self.as_mut().empty_media_slot(slot);
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if let Ok(mut state) = shared.try_borrow_mut() {
            state.source.close_chat_media(slot);
            return;
        }
        let _ = self.qt_thread().queue(move |app| {
            if !app.slot_calls.is_latest(slot, number) {
                return;
            }
            if let Some(shared) = app.shared.clone() {
                shared.borrow_mut().source.close_chat_media(slot);
            }
            app.empty_media_slot(slot);
        });
    }

    /// No row, no facts, no chat in the slot's model.
    fn empty_media_slot(mut self: Pin<&mut Self>, slot: kourier_viewmodel::chat_media::MediaSlot) {
        if let Some(m) = pinned(self.chat_media[slot.index()]) {
            m.clear_media();
        }
        self.as_mut().rust_mut().chat_media_convs[slot.index()] = 0;
    }

    /// The edit history is no longer on screen.
    pub fn close_revisions(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.close_revisions();
        }
    }

    /// The account whose challenge waits for an answer (a `signalcaptcha://` link came in):
    /// the one the banner is about, else any account that waits for one (the banner may show
    /// another account's more pressing state, or another account's list may be selected).
    pub(crate) fn challenged_account(&self) -> Option<AccountId> {
        (self.banner_kind().to_string() == BannerKind::Challenge.as_str())
            .then(|| AccountId(*self.banner_account()))
            .or_else(|| self.shared.as_ref()?.borrow().source.challenged_account())
    }

    /// Whether the run was started with the number tail a first link is checked against
    /// (`AppHandle::expected_number_check`; development state only, never the tail). Only the
    /// development control (`dev`, debug builds) reads it: in a release build it would be unused.
    #[cfg(debug_assertions)]
    pub(crate) fn expected_number_check(&self) -> bool {
        self.app
            .as_ref()
            .is_some_and(AppHandle::expected_number_check)
    }

    /// The history transfer the account banner shows, as the development state reports it: the
    /// phase name (`""` when there is none: `waiting`, `downloading`, `reading`, `failed`,
    /// `updating`, `history`) and how far it is (`None` when that is not known). This is
    /// `bannerTransfer`, NOT the state banner (`bannerKind` never says `history`).
    #[cfg(debug_assertions)]
    pub(crate) fn dev_transfer(&self) -> (&'static str, Option<f64>) {
        self.shown_transfer.as_ref().map_or(("", None), |t| {
            (t.phase.as_str(), (t.progress >= 0.0).then_some(t.progress))
        })
    }

    /// The app's handle, for the development control (debug builds only).
    #[cfg(debug_assertions)]
    pub(crate) fn dev_app(&self) -> Option<&AppHandle> {
        self.app.as_ref()
    }

    /// The newest message of the open timeline that fits `what` (fixture pages, tests).
    pub fn find_message(&self, what: &QString) -> i64 {
        let what = what.to_string();
        let found = pinned(self.timeline).and_then(|t| {
            t.rows()
                .iter()
                .filter(|r| r.kind == RowKind::Message)
                .find(|r| match what.as_str() {
                    "editable" => r.own && r.actions.edit,
                    "deletable" => r.own && r.actions.delete_for_everyone,
                    "photo" => {
                        r.actions.reply && r.blocks.iter().any(|b| matches!(b, Block::Image(_)))
                    }
                    "album" => r.blocks.iter().any(|b| matches!(b, Block::Album(_))),
                    "sticker" => r
                        .blocks
                        .iter()
                        .any(|b| matches!(b, Block::Image(i) if i.sticker)),
                    // A message one of whose several pictures can be removed.
                    "removable" => {
                        r.actions.delete_attachment
                            && r.blocks.iter().any(|b| matches!(b, Block::Album(_)))
                    }
                    "file" => r.blocks.iter().any(|b| matches!(b, Block::File(f) if !f.voice)),
                    "voice" => r.blocks.iter().any(|b| matches!(b, Block::File(f) if f.voice)),
                    "quote" => r.blocks.iter().any(
                        |b| matches!(b, Block::Quote(q) if q.target.is_some() && q.state == QuoteState::Known),
                    ),
                    "edited" => r.edited && !r.deleted,
                    "reacted" => r.reactions.len() > 1,
                    "own" => r.own && !r.deleted,
                    _ => false,
                })
                .and_then(|r| r.message_id())
        });
        found.map_or(0, |m| m.0)
    }

    /// Routes the core's answers to `openMedia`/`openExternally` (§8.6).
    fn deliver_media(mut self: Pin<&mut Self>, answers: Vec<MediaAnswer>) {
        for answer in answers {
            match answer {
                MediaAnswer::Opened(id, path) => {
                    let url = QString::from(file_url(&path).as_str());
                    // Only a GIF or WebP (by content) may go to `AnimatedImage`.
                    let movie = crate::media::take_copy(&path).is_some_and(|c| c.animates());
                    let (delivery, lost) = self.as_mut().rust_mut().media.opened(id);
                    self.as_mut().media_unanswered(lost);
                    match delivery {
                        Delivery::View(token) => self.as_mut().media_opened(token, url, movie),
                        Delivery::External => self.as_mut().external_ready(id.0, url),
                        Delivery::Close => {
                            if let Some(shared) = self.shared.clone() {
                                shared.borrow_mut().source.close_media(id);
                            }
                        }
                    }
                }
                MediaAnswer::Failed(id) => {
                    let failed = self.as_mut().rust_mut().media.failed(id);
                    self.as_mut().media_unanswered(failed);
                }
            }
        }
    }

    /// Tells the views (and, for `openExternally`, the user) whose requests got no copy.
    fn media_unanswered(mut self: Pin<&mut Self>, unanswered: Unanswered) {
        if unanswered.external
            && let Some(c) = pinned(self.composer)
        {
            c.set_notice(QString::from(
                trc("@info:status", "The file could not be opened.").as_ref(),
            ));
        }
        for token in unanswered.views {
            self.as_mut().media_failed(token);
        }
    }

    /// Closes the copies handed to other applications (the conversation closed).
    fn close_kept(mut self: Pin<&mut Self>) {
        let kept = self.as_mut().rust_mut().media.take_kept();
        if kept.is_empty() {
            return;
        }
        if let Some(shared) = self.shared.clone() {
            let mut state = shared.borrow_mut();
            for id in kept {
                state.source.close_media(id);
            }
        }
    }

    /// Fixture screenshots of media: `compose` fills the composer's strip, `media` sends a
    /// picture of our own (it shows as an own message with its thumbnail).
    fn fixture_media(self: Pin<&mut Self>, page: &str, conversation: ConversationId) {
        let Some(app) = self.app.clone() else {
            return;
        };
        let sample = |name: &str, w: i32, h: i32, hue: i32| {
            let png = qobject::sample_png(w, h, hue);
            app.save_pasted(name, &png).ok()
        };
        match page {
            "compose" => {
                let notes = app
                    .save_pasted("Flight notes.txt", b"Wind 4 m/s, gusts 7.\n")
                    .ok();
                let files: Vec<std::path::PathBuf> = [
                    sample("Field.png", 960, 640, 120),
                    sample("Sunset.png", 640, 640, 20),
                    notes,
                ]
                .into_iter()
                .flatten()
                .collect();
                if let Some(c) = pinned(self.composer) {
                    c.push_files(files, true);
                }
                if let Some(shared) = self.shared.clone() {
                    shared
                        .borrow_mut()
                        .source
                        .save_draft(conversation, "Photos from the field");
                }
            }
            // The choices and notes of the list: a picture sent as the file it is, and a file
            // of a type some apps of the network will not save (still sent).
            "originals" => {
                let files: Vec<std::path::PathBuf> = [
                    sample("Field.png", 960, 640, 120),
                    sample("Scan of the permit.png", 1240, 1754, 40),
                    app.save_pasted("Ground station.apk", &[b'P'; 2_611_200])
                        .ok(),
                    app.save_pasted("Flight notes.txt", b"Wind 4 m/s, gusts 7.\n")
                        .ok(),
                ]
                .into_iter()
                .flatten()
                .collect();
                if let Some(mut c) = pinned(self.composer) {
                    c.as_mut().push_files(files, true);
                    c.set_original(1, true);
                }
                if let Some(shared) = self.shared.clone() {
                    shared
                        .borrow_mut()
                        .source
                        .save_draft(conversation, "The permit, unchanged, and the app");
                }
            }
            // Many files of every kind at once: more than Signal Desktop's composer takes,
            // with one thing among them that no network can carry (a file that is gone).
            "files" => {
                let mut files: Vec<std::path::PathBuf> = Vec::new();
                for n in 0..40 {
                    let name = format!("Survey {:02}.png", n + 1);
                    files.extend(sample(&name, 480, 320, (n * 37) % 360));
                    if n == 1 {
                        // A file that was deleted after it was picked.
                        let gone = files
                            .first()
                            .and_then(|f| f.parent())
                            .map(|dir| dir.join("Raw footage.mov"));
                        files.extend(gone);
                    }
                    if n % 9 == 4 {
                        let name = format!("Track log {}.csv", n / 9 + 1);
                        let rows =
                            "t;lat;lon;alt\n".repeat(400 + usize::try_from(n).unwrap_or(0) * 60);
                        files.extend(app.save_pasted(&name, rows.as_bytes()).ok());
                    }
                }
                files.extend(app.save_pasted("Checklist.pdf", &[b'%'; 48_211]).ok());
                if let Some(c) = pinned(self.composer) {
                    c.push_files(files, true);
                }
                if let Some(shared) = self.shared.clone() {
                    shared
                        .borrow_mut()
                        .source
                        .save_draft(conversation, "Everything from Saturday's survey");
                }
            }
            // A folder given to the composer, beside a file: the question (`folder`), its
            // files in the list (`folderfiles`), and two folders being packed (`folderzip`).
            "folder" | "folderfiles" | "folderzip" => {
                let Some(first) = sample("Field.png", 960, 640, 120) else {
                    return;
                };
                let Some(dir) = first.parent().map(std::path::Path::to_path_buf) else {
                    return;
                };
                let png = |w, h, hue| qobject::sample_png(w, h, hue);
                let mut files = vec![first];
                let raw = dir.join("Raw footage");
                let made = [
                    ("Flight 1.png", png(960, 640, 200)),
                    ("Flight 2.png", png(960, 640, 260)),
                    ("Track log.csv", "t;lat;lon;alt\n".repeat(900).into_bytes()),
                    (
                        "notes/Checklist.txt",
                        b"Props, battery, compass.\n".to_vec(),
                    ),
                    ("notes/Weather.txt", b"Wind 4 m/s, gusts 7.\n".to_vec()),
                    // What a walk leaves out: a hidden file, and a link.
                    (
                        ".directory",
                        b"[Desktop Entry]\nIcon=folder-videos\n".to_vec(),
                    ),
                ]
                .into_iter()
                .all(|(name, bytes)| {
                    let path = raw.join(name);
                    path.parent()
                        .is_some_and(|p| std::fs::create_dir_all(p).is_ok())
                        && std::fs::write(&path, bytes).is_ok()
                });
                let linked =
                    std::os::unix::fs::symlink(raw.join("Flight 2.png"), raw.join("Latest"))
                        .is_ok();
                if made && linked {
                    files.push(raw);
                }
                if page == "folderzip" {
                    let site = dir.join("Site photos");
                    let made = (1..=3).all(|n| {
                        std::fs::create_dir_all(&site).is_ok()
                            && std::fs::write(
                                site.join(format!("Site {n}.png")),
                                png(1600, 1200, n * 70),
                            )
                            .is_ok()
                    });
                    if made {
                        files.push(site);
                    }
                }
                if let Some(c) = pinned(self.composer) {
                    c.push_files(files, false);
                }
                if let Some(shared) = self.shared.clone() {
                    shared
                        .borrow_mut()
                        .source
                        .save_draft(conversation, "Saturday's flights");
                }
            }
            // Files that would leave with where they were taken (made-up places, synthetic
            // bytes): the page presses Send and the question shows over the composer. One
            // located video (`location`); two located videos and a plain file (`locationmany`);
            // under "Never send" (set by `MainWindow.pageSetup`), a located video and a plain
            // file (`locationnever`).
            "location" | "locationmany" | "locationnever" => {
                use kourier_app::facade::{SampleLocated, sample_located};
                let mut made = vec![
                    app.save_pasted("Flight 3.mp4", &sample_located(SampleLocated::Mp4))
                        .ok(),
                ];
                if page == "locationmany" {
                    made.push(
                        app.save_pasted("Landing.mov", &sample_located(SampleLocated::Mov))
                            .ok(),
                    );
                }
                if page != "location" {
                    made.push(
                        app.save_pasted("Flight notes.txt", b"Wind 4 m/s, gusts 7.\n")
                            .ok(),
                    );
                }
                let files: Vec<std::path::PathBuf> = made.into_iter().flatten().collect();
                if let Some(c) = pinned(self.composer) {
                    c.push_files(files, true);
                }
                if let Some(shared) = self.shared.clone() {
                    shared
                        .borrow_mut()
                        .source
                        .save_draft(conversation, "Footage from the field");
                }
            }
            // A photo sent with a caption. Send waits for the core's look at the file (LOC, F12):
            // `MainWindow.pageSetup` presses it once the list is planned.
            "media" => {
                if let (Some(path), Some(c)) =
                    (sample("Field.png", 960, 640, 120), pinned(self.composer))
                {
                    c.push_files(vec![path], true);
                }
            }
            _ => {}
        }
    }

    /// Fixture pages only: answers the folder question the way a page shows it.
    pub fn fixture_folders(self: Pin<&mut Self>, answer: &QString) -> bool {
        if !self.fixture {
            return false;
        }
        let Some(composer) = pinned(self.composer) else {
            return false;
        };
        composer.fixture_folders(&answer.to_string())
    }

    /// Gives the links of a message text the color `color` (own bubbles use their ink).
    ///
    /// # Safety
    ///
    /// `document` must be null or a live `QObject`.
    pub unsafe fn color_links(&self, document: *mut QObject, color: &QColor, ink: &QColor) {
        // SAFETY: forwarded caller contract.
        unsafe { qobject::color_links_of(document, color, ink) };
    }

    /// The icon theme's name for the application's icon (`APP_ID`) when the theme has an icon
    /// of that name, else an empty string. QML cannot tell: `Kirigami.Icon.valid` is true for a
    /// name the theme lacks. Asked once, by `Style.appIcon`.
    pub fn themed_app_icon(&self) -> QString {
        if crate::kde::app_icon_in_theme() {
            QString::from(crate::APP_ID)
        } else {
            QString::default()
        }
    }

    /// Saves a screenshot of `window` to `path`; prints `shot: <path> luma=<0-255>` to stdout
    /// for `cargo xtask shots`.
    ///
    /// # Safety
    ///
    /// `window` must be null or a live `QObject`.
    pub unsafe fn save_screenshot(
        self: Pin<&mut Self>,
        window: *mut QObject,
        path: &QString,
    ) -> bool {
        // SAFETY: forwarded caller contract.
        let luma = unsafe { qobject::grab_window_to_file(window, path) };
        if luma < 0 {
            tracing::error!("screenshot failed");
            return false;
        }
        println!("shot: {path} luma={luma}");
        true
    }

    /// What still keeps a screenshot of `window` from being final (empty: nothing).
    ///
    /// # Safety
    ///
    /// `window` must be null or a live `QObject`.
    pub unsafe fn screenshot_holds(&self, window: *mut QObject) -> QString {
        // SAFETY: forwarded caller contract.
        let images = unsafe { qobject::images_loading(window) }.max(0);
        let requests = qobject::pending_image_requests().max(0);
        // Thumbnails the core derives change the URLs of the pictures that are on screen: a
        // window that shows none does not wait for them (the count is the process's, so a
        // derive of another chat still holds a window with a picture).
        // SAFETY: forwarded caller contract.
        let derive = if unsafe { qobject::attachment_pictures(window) } > 0 {
            self.app.as_ref().map_or(0, |a| a.media().deriving())
        } else {
            0
        };
        // What the core has said and the models have not shown yet: events sent and not read
        // by the pump, events in its queue, and the reads of the models that wait for the
        // core (`MediaHandles::deriving` is 0 as soon as the core has published; the new URLs
        // are on screen only after these).
        let events = self.app.as_ref().map_or(0, AppHandle::events_unseen)
            + self.pump.as_ref().map_or(0, |p| p.len());
        let queries = self
            .shared
            .as_ref()
            .map_or(0, |s| s.borrow().source.queries_in_flight());
        // A `MarkRead` the viewmodel sent (the open chat, once the window is active and its
        // newest rows are loaded) and the core has not finished: its events do not exist yet,
        // so "events" cannot see it; without this the list row and the rail's badge could be
        // captured still unread (strike 2, 2026-10-27). A page that never reads never waits.
        let reads = self.app.as_ref().map_or(0, AppHandle::reads_in_flight);
        let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        // Which images still load, named: a render that never settles ("images 1") says which
        // image stalls (its source and sourceSize).
        // SAFETY: forwarded caller contract.
        let loading = if images > 0 {
            unsafe { qobject::loading_images(window) }.to_string()
        } else {
            String::new()
        };
        let reasons: Vec<String> = [
            ("images", i64::from(images)),
            ("requests", i64::from(requests)),
        ]
        .into_iter()
        .chain([
            ("derive", count(derive)),
            ("events", count(events)),
            ("queries", count(queries)),
            ("read", count(reads)),
        ])
        .filter(|(_, n)| *n > 0)
        .map(|(what, n)| {
            if what == "images" && !loading.is_empty() {
                format!("{what} {n} [{loading}]")
            } else {
                format!("{what} {n}")
            }
        })
        .collect();
        QString::from(reasons.join(", "))
    }

    /// Saves a screenshot of `window` to `path` once two frames in a row are equal; prints
    /// `shot: <path> luma=<0-255> settle_ms=<n>` to stdout for `cargo xtask shots`. -2: the
    /// frame differs from the previous call's; -1: failed.
    ///
    /// # Safety
    ///
    /// `window` must be null or a live `QObject`.
    pub unsafe fn save_screenshot_if_stable(
        self: Pin<&mut Self>,
        window: *mut QObject,
        path: &QString,
        settle_ms: i32,
    ) -> i32 {
        // SAFETY: forwarded caller contract.
        let luma = unsafe { qobject::grab_if_stable(window, path) };
        if luma == -1 {
            tracing::error!("screenshot failed");
        } else if luma >= 0 {
            println!("shot: {path} luma={luma} settle_ms={settle_ms}");
        }
        luma
    }

    /// Saves the frame of `window` as it is (the screenshot did not settle) and says so on
    /// stderr: `shot: unsettled <holds>`.
    ///
    /// # Safety
    ///
    /// `window` must be null or a live `QObject`.
    pub unsafe fn save_unsettled_screenshot(
        self: Pin<&mut Self>,
        window: *mut QObject,
        path: &QString,
        holds: &QString,
        settle_ms: i32,
    ) -> i32 {
        // SAFETY: forwarded caller contract.
        let luma = unsafe { qobject::grab_window_to_file(window, path) };
        if luma < 0 {
            tracing::error!("screenshot failed");
            return luma;
        }
        println!("shot: {path} luma={luma} settle_ms={settle_ms}");
        // (Not `tracing`/`console.warn`: a line with a QML file counts as a QML problem.)
        eprintln!("shot: unsettled {holds}");
        luma
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A network this build cannot log in to reaches the viewmodel as an entry of Settings with
    /// the reason the application gave (a swap of the two would put the wrong sentence on its
    /// accounts' banner) and as no choice: not available, not on, no flow, no warning.
    /// Mutant: the two reasons swapped.
    #[test]
    fn a_missing_network_is_a_settings_entry_with_the_applications_reason() {
        for (given, wanted) in [
            (TelegramMissing::ApiKey, NetworkMissing::ApiKey),
            (TelegramMissing::Library, NetworkMissing::Library),
        ] {
            let entry = missing_entry(NetworkKind::Telegram, given);
            assert_eq!(entry.missing, Some(wanted), "{given:?}");
            let rows = network_choices(&[entry], false);
            let row = rows
                .iter()
                .find(|r| r.kind == NetworkKind::Telegram)
                .expect("the row");
            assert_eq!(row.missing, Some(wanted), "{given:?}");
            assert!(
                row.experimental,
                "Settings lists it with the experimental ones"
            );
            assert!(!row.available && !row.on && !row.asks_first);
            assert!(row.flows.is_empty() && row.risk_notice.is_none());
            assert_eq!(row.description, "Not available in this build");
        }
    }

    /// The list's title for several shown accounts: their names in panel order; two with the
    /// same name give the short form ("2 accounts") instead (P4).
    #[test]
    fn several_shown_accounts_title_the_list_by_name() {
        assert_eq!(
            joined_title(&["Andrii", "Work"]).as_deref(),
            Some("Andrii · Work")
        );
        assert_eq!(joined_title(&["A", "B", "C"]).as_deref(), Some("A · B · C"));
        assert_eq!(joined_title(&["Andrii", "Andrii"]), None);
        assert_eq!(joined_title(&["Andrii", "Work", "Andrii"]), None);
        assert_eq!(joined_title(&[]), None);
    }

    /// One shown account titles the list with its name, else its network's, as several shown
    /// accounts are named (UI-11): never an empty header.
    #[test]
    fn one_shown_account_without_a_name_titles_the_list_with_its_network() {
        let row = |id: i64, name: &str, network_name: &str| AccountRow {
            id: AccountId(id),
            uuid: kourier_model::AccountUuid::new_random(),
            name: name.to_owned(),
            network: kourier_model::NetworkKind::Fake,
            network_name: network_name.to_owned(),
            network_icon: String::new(),
            status: kourier_viewmodel::conversations::AccountStatus::Online,
            status_label: String::new(),
            unread: 0,
            avatar_url: String::new(),
            number: String::new(),
            offers_history: false,
            offers_unlink: false,
            device: String::new(),
            shown: true,
            place: 0,
        };
        let rows = [row(1, "", "WhatsApp"), row(2, "Andrii", "Signal")];
        assert_eq!(single_title(&rows, AccountId(1)), "WhatsApp");
        assert_eq!(single_title(&rows, AccountId(2)), "Andrii");
        assert_eq!(single_title(&rows, AccountId(9)), "");
        // The same name several shown accounts give it.
        assert_eq!(title_name(&rows[0]), single_title(&rows, AccountId(1)));
    }

    #[test]
    fn every_answered_open_is_closed_exactly_once() {
        let mut m = MediaRequests::default();
        let none = Unanswered::default;
        let a = AttachmentId(1);
        let t1 = m.request(a);
        let t2 = m.request(a);
        assert_ne!(t1, t2);
        // Answers go to the requests in order.
        assert_eq!(m.opened(a), (Delivery::View(t1), none()));
        // The second view went before its answer: the answer is closed at once.
        assert_eq!(m.release(t2), None);
        assert_eq!(m.opened(a), (Delivery::Close, none()));
        // The first view's copy closes when it goes, once.
        assert_eq!(m.release(t1), Some(a));
        assert_eq!(m.release(t1), None);
        // An unexpected answer is closed too.
        assert_eq!(m.opened(AttachmentId(9)), (Delivery::Close, none()));
        // External opens are kept until the conversation closes.
        m.request_external(a);
        assert_eq!(m.opened(a), (Delivery::External, none()));
        assert_eq!(m.take_kept(), vec![a]);
        assert!(m.take_kept().is_empty());
        assert!(m.pending.is_empty() && m.answered.is_empty());
    }

    #[test]
    fn a_failed_open_fails_the_request_for_its_attachment_and_can_be_retried() {
        let mut m = MediaRequests::default();
        let (a, b, c) = (AttachmentId(1), AttachmentId(2), AttachmentId(3));
        // Three requests wait.
        let ta = m.request(a);
        let tb = m.request(b);
        m.request_external(c);
        // The failure names `b`: only its view is told, although `a` was asked for first.
        assert_eq!(
            m.failed(Some(b)),
            Unanswered {
                views: vec![tb],
                external: false
            }
        );
        // The others still get their own answers.
        assert_eq!(m.opened(a), (Delivery::View(ta), Unanswered::default()));
        assert_eq!(m.opened(c), (Delivery::External, Unanswered::default()));
        // Asking again for the attachment that failed is answered: nothing stale is in its way.
        let tb2 = m.request(b);
        assert_eq!(m.opened(b), (Delivery::View(tb2), Unanswered::default()));
        assert_eq!(m.release(tb2), Some(b));
        // Two requests for one attachment: a failure is the older one's.
        let t1 = m.request(a);
        let t2 = m.request(a);
        assert_eq!(m.failed(Some(a)).views, vec![t1]);
        assert_eq!(m.failed(Some(a)).views, vec![t2]);
        // A failed `openExternally` is reported as such; a view that went before its failure
        // is not told; a failure nobody waits for is nothing.
        m.request_external(c);
        let gone = m.request(b);
        assert_eq!(m.release(gone), None);
        assert_eq!(
            m.failed(Some(c)),
            Unanswered {
                views: vec![],
                external: true
            }
        );
        assert_eq!(m.failed(Some(b)), Unanswered::default());
        assert_eq!(m.failed(Some(b)), Unanswered::default());
        assert_eq!(m.failed(Some(AttachmentId(99))), Unanswered::default());
        assert!(m.pending.is_empty());
        // A failure without a name is the oldest request's (the core answers in order).
        let t3 = m.request(a);
        let t4 = m.request(b);
        assert_eq!(m.failed(None).views, vec![t3]);
        assert_eq!(m.failed(None).views, vec![t4]);
        assert_eq!(m.failed(None), Unanswered::default());
    }

    #[test]
    fn requests_whose_answers_were_lost_are_failed_not_left_waiting() {
        let mut m = MediaRequests::default();
        let (a, b) = (AttachmentId(1), AttachmentId(2));
        // The answer for `a` was lost with lagged events: the answer for `b` shows it.
        let ta = m.request(a);
        m.request_external(a);
        let tb = m.request(b);
        assert_eq!(
            m.opened(b),
            (
                Delivery::View(tb),
                Unanswered {
                    views: vec![ta],
                    external: true
                }
            )
        );
        // After a lag nobody waits; an answer that still comes is closed.
        let tc = m.request(a);
        let td = m.request(b);
        assert_eq!(
            m.lost(),
            Unanswered {
                views: vec![tc, td],
                external: false
            }
        );
        assert_eq!(m.opened(a), (Delivery::Close, Unanswered::default()));
        assert_eq!(m.release(tc), None);
        // The view answered before the lag still closes its copy.
        assert_eq!(m.release(tb), Some(b));
        assert!(m.pending.is_empty() && m.answered.is_empty());
    }

    #[test]
    fn file_urls_are_percent_encoded() {
        assert_eq!(
            file_url(Path::new("/run/user/1000/kourier/media/a b#%ü.png")),
            "file:///run/user/1000/kourier/media/a%20b%23%25%C3%BC.png"
        );
    }
}
