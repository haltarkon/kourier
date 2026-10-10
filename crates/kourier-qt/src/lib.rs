//! The Kourier UI: the only crate that uses cxx-qt (architecture §8). QML lives in `qml/`; view
//! logic lives in `kourier-viewmodel`; this crate holds the bridges (`App`, the list models,
//! the controllers), the event pump between the core and the Qt thread, and a small C++ shim
//! for screenshots and the Qt message handler. It is also the only crate that links KDE
//! Frameworks ([`kde`]: application data, Kirigami's defaults, KCrash; [`l10n`]: KI18n behind
//! the viewmodel's `Translator`). Debug builds add the development control channel ([`dev`],
//! `--dev-control`).
//!
//! The binary starts the app (`kourier_app::App::start`), hands its [`UiContext`] over with
//! [`set_context`] and calls [`run`]. Without a context (pure UI tests), `App` shows the
//! fixture data set or nothing.

pub mod app;
pub mod bridge;
pub mod controllers;
pub mod desktop;
#[cfg(debug_assertions)]
pub mod dev;
#[cfg(debug_assertions)]
pub mod dev_ffi;
pub mod kde;
pub mod kde_ffi;
pub mod l10n;
pub mod media;
pub mod models;
pub mod pump;
pub mod qtlog;
pub mod shared;

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use cxx_qt_lib::{QQmlApplicationEngine, QUrl};
use cxx_qt_lib_extras::QApplication;
use kourier_app::AppHandle;

pub use bridge::qobject::{
    AccountListModel, App, ComposerController, ConversationListModel, LoginController,
    SettingsController, TimelineModel,
};

/// Desktop file name, Wayland `app_id` and D-Bus name.
pub use kourier_app::desktop::APP_ID;

const MAIN_QML: &str = "qrc:/qt/qml/app/kourier/qml/Main.qml";

/// Pages `--page` can open (`drawer` = the conversation list with the accounts drawer open;
/// `link` = the first login step, `qr` = the QR code after the device name was confirmed;
/// `history` = the chat scrolled up to its first message, paging older rows in; `media` = the
/// fixture's chat with a photo, a file and a voice note after sending a picture; `viewer` = the
/// image viewer over it; `compose` = the composer with attachments waiting; `files` = the
/// composer with many files of every kind at once, split into messages, one of them marked as
/// not sendable; `album` = a chat with albums of two, three, four and nine pictures;
/// `albumviewer` = the viewer stepping through an album; `revisions` = the edit history of a
/// message; `compact` = the list in its compact layout).
///
/// The message actions, on the fixture's showcase chats: `quotes` = replies with their quote
/// blocks (a known, a missing and a deleted original, a quoted photo) and an edited caption;
/// `menu` = the context menu of an own message; `react` = the reaction picker; `reactions` =
/// who reacted to a group message; `edit` = the composer editing the last own message; `reply`
/// = the composer replying to a photo; `delete` = the delete question; `timer` = a chat with a
/// disappearing-message timer and its menu; `placeholders` = stand-ins for view-once media
/// and other content that is not shown yet.
///
/// The account states (the fixture account ends in them): `offline`, `loggedout`,
/// `challenge` and `clock` show the banner above the conversation list.
///
/// Phase I: `originals` = the composer with a picture sent as the file it is and a file of a
/// type some apps of the network will not save; `timercustom` = the dialog for a custom
/// disappearing-message time; `clearchat` = the question before a chat's history is deleted;
/// `mediamenu` = the context menu of a message with pictures (view, delete an attachment);
/// `deleteattachment` = the question which attachment of an album to delete.
///
/// Phase J: `folder` = the question about a folder given to the composer; `folderfiles` = the
/// list after "add the files inside"; `folderzip` = two folders sent as archives, one written
/// and one being packed; `expiry` = the settings page at the choice for disappearing messages
/// while the clock is unconfirmed.
///
/// Phase M: `readmore` = a chat with one long text, shown cut with "Read more" under it.
///
/// R1 gate: `about` = the settings page with "About Kourier" opened (Kirigami Add-ons'
/// `AboutPage` fed by the application's `KAboutData`).
///
/// Calls: `calls` = a chat with a line for every state of a 1:1 call (missed, declined,
/// answered on another device, made from the phone, unanswered, and one that rings now).
///
/// History transfer (architecture §14.2 item 14): `transferwait`, `transferdownload` and
/// `transferread` = the link page while the phone is asked, while the history downloads
/// and while it is read (the page links the fixture account itself and the transfer stays
/// in that state); `transferbanner` = the main window after a restart in the middle of a
/// download, the transfer in the account banner; `transferfailed` = the main window with
/// the banner of a failed transfer; `transferask` = the settings page with the question
/// "Remove and Link Again?" of "Transfer History from Phone…".
pub const PAGES: [&str; 47] = [
    "welcome",
    "link",
    "qr",
    "list",
    "chat",
    "history",
    "settings",
    "drawer",
    "media",
    "viewer",
    "compose",
    "files",
    "album",
    "albumviewer",
    "revisions",
    "compact",
    "quotes",
    "menu",
    "react",
    "reactions",
    "edit",
    "reply",
    "delete",
    "timer",
    "placeholders",
    "offline",
    "loggedout",
    "challenge",
    "clock",
    "originals",
    "timercustom",
    "clearchat",
    "mediamenu",
    "deleteattachment",
    "folder",
    "folderfiles",
    "folderzip",
    "expiry",
    "readmore",
    "about",
    "calls",
    "transferwait",
    "transferdownload",
    "transferread",
    "transferbanner",
    "transferfailed",
    "transferask",
];

/// Registers the `image://kourier` provider on `engine` (before it loads QML; §8.6). [`run`]
/// does it; tests that create their own engine call it too.
pub fn install_image_provider(engine: std::pin::Pin<&mut QQmlApplicationEngine>) {
    bridge::qobject::install_image_provider(engine, media::read);
}

/// How the UI starts (from the command line, §8.2).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiOptions {
    /// Without a [`UiContext`]: show the built-in fixture data set instead of nothing.
    pub fixture: bool,
    /// Start on this page (one of [`PAGES`]).
    pub page: Option<String>,
    /// Save a screenshot of the window here and quit.
    pub screenshot: Option<PathBuf>,
    /// Initial window size in pixels.
    pub size: Option<(u32, u32)>,
    /// Debug builds: serve the development control channel on stdin/stdout (`--dev-control`,
    /// [`dev`]).
    #[cfg(debug_assertions)]
    pub dev_control: bool,
    /// Measuring builds (cargo feature `perf`): the scripted session `perf/PerfDriver.qml`
    /// runs (`startup`, `session[:<idle seconds>]`, `notify`; `cargo xtask perf`).
    #[cfg(feature = "perf")]
    pub perf_scenario: Option<String>,
}

/// What `App` needs from the running app: commands, events and facts (§8.1).
#[derive(Clone, Debug)]
pub struct UiContext {
    /// The app.
    pub app: AppHandle,
}

static OPTIONS: OnceLock<UiOptions> = OnceLock::new();
static CONTEXT: Mutex<Option<UiContext>> = Mutex::new(None);

/// The options [`run`] was started with (defaults before that).
pub fn options() -> UiOptions {
    OPTIONS.get().cloned().unwrap_or_default()
}

/// Sets the options before the QML engine creates `App` (tests that load QML themselves).
/// Returns `false` when options were already set.
pub fn set_options(opts: UiOptions) -> bool {
    OPTIONS.set(opts).is_ok()
}

/// Hands the running app to the `App` singleton the QML engine creates next.
pub fn set_context(ctx: UiContext) {
    *CONTEXT.lock().unwrap_or_else(PoisonError::into_inner) = Some(ctx);
}

/// Takes the context (once, when `App` initializes).
pub(crate) fn take_context() -> Option<UiContext> {
    CONTEXT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
}

/// Whether this run is the application on a person's desktop: a real profile, no screenshot,
/// no development channel, no measuring session. Only such a run gets the crash reporter; in
/// every other one a crash must not open a dialog.
fn is_normal_run() -> bool {
    let real_profile = CONTEXT
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|ctx| !ctx.app.is_fixture());
    let opts = options();
    #[cfg(debug_assertions)]
    let dev_control = opts.dev_control;
    #[cfg(not(debug_assertions))]
    let dev_control = false;
    real_profile
        && !opts.fixture
        && opts.screenshot.is_none()
        && !dev_control
        && !cfg!(feature = "perf")
}

/// Creates the application, loads the QML UI and runs the Qt event loop. Returns the exit code.
pub fn run(opts: UiOptions) -> i32 {
    let _ = set_options(opts);
    qtlog::install();
    // Before anything builds a text: `App` and its models translate while they are created.
    l10n::install();

    // Headless screenshots must show no hover state: park the pointer (also for scripted
    // offscreen runs of the development channel; never on a real display).
    let screenshot = OPTIONS.get().is_some_and(|o| {
        #[cfg(debug_assertions)]
        let scripted =
            o.dev_control && std::env::var_os("QT_QPA_PLATFORM").is_some_and(|p| p == "offscreen");
        #[cfg(not(debug_assertions))]
        let scripted = false;
        o.screenshot.is_some() || scripted
    });
    // QApplication (not QGuiApplication) because the org.kde.desktop style uses QStyle.
    let mut app = QApplication::new();
    kourier_app::trace::mark_with_cpu("startup.qt_app", &[]);
    // The window may hide in the tray or be released while hidden; QML quits explicitly.
    bridge::qobject::set_quit_on_last_window_closed(false);
    if screenshot {
        bridge::qobject::park_cursor();
    }
    // The application's name, version and desktop file name (the Wayland `app_id`) come from
    // its About data; then KDE's defaults for a Kirigami application: the style unless
    // `QT_QUICK_CONTROLS_STYLE` or the platform theme chose one, Breeze icons as the fallback,
    // the desktop's colour scheme. In this order: see `kde::apply_defaults`.
    kde::set_about_data(&kourier_app::about::info(), APP_ID);
    kde::apply_defaults(APP_ID);
    if is_normal_run() {
        kde::start_crash_handler();
    }
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        l10n::setup_engine(engine.as_mut());
        install_image_provider(engine.as_mut());
        kourier_app::trace::mark_with_cpu("startup.qml_engine", &[]);
        engine.load(&QUrl::from(MAIN_QML));
    }
    kourier_app::trace::mark_with_cpu("startup.qml_loaded", &[]);
    let code = match app.as_mut() {
        Some(app) => app.exec(),
        None => 1,
    };
    // §8.2 step 5: the engine (and with it `App` and the models) goes before the app stops.
    drop(engine);
    media::set_store(None);
    // What Qt says from here on (the application object going, static destructors at exit)
    // is not logged any more: see `qtlog::end`.
    drop(app);
    qtlog::end();
    code
}
