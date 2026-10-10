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
    QColor, QDateTime, QList, QMap, QMapPair_QString_QVariant, QString, QTimeZone, QVariant,
};
use kourier_app::facade::{UiCommand, UiEvent};
use kourier_app::{AppHandle, trace};
use kourier_model::{AccountId, AttachmentId, ConversationId, MessageId, NetworkKind};
use kourier_viewmodel::conversations::{BannerKind, network_name};
use kourier_viewmodel::diff::ListOp;
use kourier_viewmodel::i18n::trc;
use kourier_viewmodel::login::network_choices;
use kourier_viewmodel::time::{Calendar, FixedOffset};
use kourier_viewmodel::timeline::{Block, QuoteState};
use kourier_viewmodel::{
    Changes, ConversationFilter, CoreSource, CoreSourceConfig, EmptySource, FixtureSource,
    LoginViewKind, MediaAnswer, NetworkChoice, ReplaceQuestion, RowKind, TransferView, UiSource,
};
use tokio::sync::broadcast::error::RecvError;

use crate::bridge::qobject::{
    self, AccountListModel, AttachmentListModel, ComposerController, ConversationListModel,
    LoginController, QObject, SettingsController, TimelineModel,
};
use crate::controllers::SettingsRust;
use crate::models::{self, ListModel};
use crate::pump::{DRAIN_BUDGET, Drained, PumpItem, PumpQueue};
use crate::shared::{Shared, shared};
use crate::{UiContext, UiOptions, options, take_context};

/// Sub-objects owned by `App`; dropped (and deleted) with it.
struct Owned {
    _accounts: UniquePtr<AccountListModel>,
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
    pub(crate) start_hidden: bool,
    pub(crate) release_delay_ms: i32,
    /// Timing marks wait for a frame (`frameTraced`).
    pub(crate) trace_frame: bool,
    /// The scripted session of a measuring build (empty in every other build).
    pub(crate) perf_scenario: QString,
    pub(crate) tray_status: i32,
    pub(crate) unread_count: i32,
    pub(crate) banner_kind: QString,
    pub(crate) banner_text: QString,
    pub(crate) banner_action: QString,
    pub(crate) banner_account: i64,
    /// The history transfer the account banner shows, under the state of the accounts if
    /// there is one (`bannerKind`), as a map; its `phase` is empty when there is none,
    /// `failed` for a failure that waits for the user.
    pub(crate) banner_transfer: QVariant,
    /// The history transfer the link page shows in place of its "linking" step, as a map;
    /// its `phase` is empty when there is none.
    pub(crate) link_transfer: QVariant,
    /// The accounts that offer "Transfer History from Phone…" (`account`, `label`, `about`),
    /// for the settings page.
    pub(crate) history_accounts: QVariant,
    /// The versions of the message `showRevisions` asked about, newest first.
    pub(crate) revisions: QVariant,
    pub(crate) networks: QVariant,
    pub(crate) accounts: *mut AccountListModel,
    pub(crate) conversations: *mut ConversationListModel,
    pub(crate) timeline: *mut TimelineModel,
    pub(crate) composer: *mut ComposerController,
    pub(crate) login: *mut LoginController,
    pub(crate) settings: *mut SettingsController,
    owned: Option<Owned>,
    shared: Option<Shared>,
    filter: ConversationFilter,
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
    /// What `bannerTransfer` and `linkTransfer` were made from (a property is written only
    /// when its view changed: a download reports several times a second).
    shown_transfers: (Option<TransferView>, Option<TransferView>),
    /// What `historyAccounts` was made from: `(account, which account it is)`.
    shown_history: Vec<(i64, String)>,
}

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
            start_hidden: false,
            release_delay_ms: release_delay_ms(),
            trace_frame: false,
            perf_scenario: QString::from(perf_scenario(&opts).as_str()),
            // Unknown until the desktop integration reports (0 = no tray, 1 = tray shown); stays
            // unknown without desktop services (headless runs).
            tray_status: -1,
            unread_count: 0,
            banner_kind: QString::default(),
            banner_text: QString::default(),
            banner_action: QString::default(),
            banner_account: 0,
            banner_transfer: transfer_variant(None),
            link_transfer: transfer_variant(None),
            history_accounts: QVariant::from(&QList::<QVariant>::default()),
            revisions: QVariant::from(&QList::<QVariant>::default()),
            networks: QVariant::default(),
            accounts: std::ptr::null_mut(),
            conversations: std::ptr::null_mut(),
            timeline: std::ptr::null_mut(),
            composer: std::ptr::null_mut(),
            login: std::ptr::null_mut(),
            settings: std::ptr::null_mut(),
            owned: None,
            shared: None,
            filter: ConversationFilter::default(),
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
            shown_transfers: (None, None),
            shown_history: Vec::new(),
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
    m.insert(QString::from("title"), text(|v| &v.title));
    m.insert(QString::from("text"), text(|v| &v.text));
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

/// The question before an account is removed and linked again, as QML reads it.
fn question_variant(q: &ReplaceQuestion) -> QVariant {
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
    QVariant::from(&m)
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

/// The welcome page's networks: the announced ones, available when compiled in.
fn choices(app: &AppHandle) -> Vec<NetworkChoice> {
    let registered: Vec<NetworkKind> = app.networks().iter().map(|n| n.kind).collect();
    // A network whose descriptor says that a newly linked device is handed the history.
    let brings_history = |kind: NetworkKind| {
        app.networks()
            .iter()
            .any(|n| n.kind == kind && n.history == kourier_model::HistorySource::PushedOnLink)
    };
    let mut out: Vec<NetworkChoice> = network_choices()
        .into_iter()
        .map(|mut c| {
            c.history_on_link = brings_history(c.kind);
            if c.kind == NetworkKind::Signal && !registered.contains(&c.kind) {
                c.available = false;
                c.description = if app.is_fixture() {
                    trc(
                        "@info:status a messenger that cannot be linked",
                        "Not available in fixture mode",
                    )
                    .into_owned()
                } else {
                    trc(
                        "@info:status a messenger that cannot be linked",
                        "Not in this build",
                    )
                    .into_owned()
                };
            }
            c
        })
        .collect();
    if registered.contains(&NetworkKind::Fake) {
        out.insert(
            1.min(out.len()),
            NetworkChoice {
                kind: NetworkKind::Fake,
                name: network_name(NetworkKind::Fake).to_owned(),
                description: trc(
                    "@info a test account that can be linked",
                    "A scripted demo account with sample chats",
                )
                .into_owned(),
                icon: "fake".into(),
                available: true,
                history_on_link: brings_history(NetworkKind::Fake),
            },
        );
    }
    out
}

fn core_source(ctx: &UiContext) -> CoreSource {
    let handle = ctx.app.clone();
    let fixture = handle.fixture();
    let now: Box<dyn Fn() -> i64> = match fixture {
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
            if queue.push(item) && thread.queue(|app| app.drain_events()).is_err() {
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
        let demo = source.demo_conversation();
        let shared = shared(source);
        let thread = self.qt_thread();

        let accounts = qobject::new_account_list_model();
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
            rust.accounts = accounts.as_mut_ptr();
            rust.conversations = conversations.as_mut_ptr();
            rust.timeline = timeline.as_mut_ptr();
            rust.composer = composer.as_mut_ptr();
            rust.login = login.as_mut_ptr();
            rust.settings = settings.as_mut_ptr();
            rust.owned = Some(Owned {
                _accounts: accounts,
                _conversations: conversations,
                _timeline: timeline,
                _attachments: attachments,
                _composer: composer,
                _login: login,
                _settings: settings,
            });
            rust.shared = Some(shared.clone());
            // (The pages of the link page's transfer states walk the link by themselves.)
            rust.auto_submit_login = matches!(
                opts.page.as_deref(),
                Some("qr" | "transferwait" | "transferdownload" | "transferread")
            );
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
                | "clearchat" | "transferbanner" | "transferfailed",
            ) => {
                if let Some(id) = demo {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("quotes" | "menu" | "react" | "edit" | "reply" | "delete") => {
                if let Some(id) = fixture.and_then(|f| f.quotes_conversation) {
                    self.as_mut().open_conversation(id.0);
                }
            }
            Some("album" | "albumviewer" | "revisions" | "mediamenu" | "deleteattachment") => {
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
            Some(
                page @ ("media" | "viewer" | "compose" | "files" | "originals" | "folder"
                | "folderfiles" | "folderzip"),
            ) => {
                if let Some(id) = media_conv {
                    self.as_mut().open_conversation(id.0);
                    self.as_mut().fixture_media(page, id);
                }
            }
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

impl qobject::App {
    fn next_generation(mut self: Pin<&mut Self>) -> u64 {
        let mut rust = self.as_mut().rust_mut();
        rust.generation += 1;
        rust.generation
    }

    fn refresh_accounts(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let rows = shared.borrow().source.accounts();
        let has = !rows.is_empty();
        let generation = self.as_mut().next_generation();
        if let Some(m) = pinned(self.accounts) {
            models::set_rows(m, generation, rows);
        }
        if *self.has_accounts() != has {
            self.as_mut().set_has_accounts(has);
        }
        self.as_mut().refresh_history_accounts();
        self.as_mut().refresh_banner();
        // The title of a selected account follows its name.
        if *self.current_account() >= 0 {
            let id = AccountId(*self.current_account());
            let name = pinned(self.accounts).and_then(|m| {
                m.store()
                    .view
                    .iter()
                    .find(|a| a.id == id)
                    .map(|a| a.name.clone())
            });
            match name {
                Some(n) if *self.current_title() != QString::from(n.as_str()) => {
                    self.as_mut().set_current_title(QString::from(n.as_str()));
                }
                Some(_) => {}
                None => self.select_account(-1),
            }
        }
    }

    /// The banner above the conversation list: what the accounts shown need the user to know
    /// or do (offline, logged out, a challenge, a wrong clock).
    fn refresh_banner(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let (banner, transfer) = {
            let shared = shared.borrow();
            (
                shared.source.banner(self.filter.account),
                // The history transfer of an account shown, chosen apart from the states:
                // the state of one account does not hide the transfer of another.
                shared
                    .source
                    .transfer_banner(self.filter.account)
                    .and_then(|b| b.transfer),
            )
        };
        let (kind, text, action, account, url) = match banner {
            Some(b) => (b.kind.as_str(), b.text, b.action, b.account.0, b.url),
            None => ("", String::new(), String::new(), 0, String::new()),
        };
        if self.shown_transfers.0 != transfer {
            self.as_mut()
                .set_banner_transfer(transfer_variant(transfer.as_ref()));
            self.as_mut().rust_mut().shown_transfers.0 = transfer;
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
                let about =
                    kourier_viewmodel::transfer::account_line(&a.name, &a.number, &a.network_name);
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

    /// The history transfer of an account changed: the link page's view of it, the banner,
    /// which accounts offer a transfer, and what is said once about an end.
    fn refresh_transfer(mut self: Pin<&mut Self>) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let link = shared.borrow().source.link_transfer();
        if self.shown_transfers.1 != link {
            self.as_mut()
                .set_link_transfer(transfer_variant(link.as_ref()));
            self.as_mut().rust_mut().shown_transfers.1 = link;
        }
        self.as_mut().refresh_history_accounts();
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
        let rows = shared.borrow().source.conversations(&self.filter);
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
        let unread = shared.borrow().source.unread_total();
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
            let policy = shared.borrow().source.expiry_policy();
            if let Some(s) = pinned(self.settings) {
                s.show_expiry_policy(policy);
            }
        }
        if ch.accounts {
            self.as_mut().refresh_accounts();
        }
        if ch.conversations {
            self.as_mut().refresh_conversations();
        }
        if ch.timeline || ch.header {
            self.as_mut().refresh_timeline(false);
        } else {
            self.as_mut().refresh_paging();
        }
        if ch.login {
            self.as_mut().show_login();
        }
        if ch.transfer || ch.login {
            // (A login step can end what the link page shows of a transfer.)
            self.as_mut().refresh_transfer();
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

    /// Shows one account's conversations (`-1` = all chats).
    pub fn select_account(mut self: Pin<&mut Self>, account: i64) {
        let title = match (account >= 0).then_some(AccountId(account)) {
            Some(id) => {
                self.as_mut().rust_mut().filter.account = Some(id);
                pinned(self.accounts)
                    .and_then(|m| {
                        m.store()
                            .view
                            .iter()
                            .find(|a| a.id == id)
                            .map(|a| a.name.clone())
                    })
                    .unwrap_or_default()
            }
            None => {
                self.as_mut().rust_mut().filter.account = None;
                trc(
                    "@title:window list header, the chats of all accounts",
                    "All Chats",
                )
                .into_owned()
            }
        };
        self.as_mut().set_current_account(account.max(-1));
        self.as_mut()
            .set_current_title(QString::from(title.as_str()));
        self.as_mut().refresh_banner();
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
            .conversations(&self.filter)
            .get(row)
            .map_or(0, |r| r.id.0)
    }

    /// The conversation `step` rows from `from` in the filtered list (0 when there is none).
    pub fn adjacent_conversation(&self, from: i64, step: i32) -> i64 {
        let Some(shared) = self.shared.as_ref() else {
            return 0;
        };
        let rows = shared.borrow().source.conversations(&self.filter);
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
        if self.filter.query != q {
            self.as_mut().rust_mut().filter.query = q;
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
            Self::notice_changed()
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
        // What waited in its composer goes with the chat.
        if let Some(c) = pinned(self.composer) {
            c.forget_conversation(conv);
        }
        self.as_mut().with_source(|s| {
            s.delete_conversation(ConversationId(conv));
            Changes {
                // (The list no longer shows its draft.)
                conversations: true,
                ..Self::notice_changed()
            }
        });
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

    /// What the picture viewer shows for `message`, from the rows as they are now: a list
    /// (empty when nothing of the message is a picture any more), or an invalid value
    /// (`undefined` in QML) when the message is not among the loaded rows.
    pub fn viewer_items(&self, message: i64) -> QVariant {
        if message <= 0 {
            return QVariant::default();
        }
        pinned(self.timeline)
            .and_then(|t| {
                t.rows()
                    .iter()
                    .find(|r| r.message_id() == Some(MessageId(message)))
                    .map(models::timeline::viewer_items)
            })
            .unwrap_or_default()
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

    /// The question was closed without confirming.
    pub fn cancel_replace(self: Pin<&mut Self>) {
        if let Some(shared) = self.shared.clone() {
            shared.borrow_mut().source.cancel_replace();
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
            "media" => {
                if let (Some(path), Some(mut c)) =
                    (sample("Field.png", 960, 640, 120), pinned(self.composer))
                {
                    c.as_mut().push_files(vec![path], true);
                    c.send(conversation.0, &QString::from("Sent from Kourier"));
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
    pub unsafe fn color_links(&self, document: *mut QObject, color: &QColor) {
        // SAFETY: forwarded caller contract.
        unsafe { qobject::color_links_of(document, color) };
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
