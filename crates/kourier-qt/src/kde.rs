//! The KDE side of the application object (architecture §8.10, §14.2 item 2): the application's
//! `KAboutData` from [`kourier_app::about::AboutInfo`], the defaults KDE sets up for a Kirigami
//! application, and KCrash. The calls are in `cpp/kde.cpp`; [`crate::run`] makes them in the
//! order that file's header states.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use cxx_qt_lib::{QString, QStringList};
use kourier_app::about::AboutInfo;
use kourier_viewmodel::i18n::trc;

use crate::kde_ffi::qt;

/// KCrash is running ([`start_crash_handler`]): [`panic_sink`] hands it the line.
static CRASH_HANDLER: AtomicBool = AtomicBool::new(false);

/// Whether the icon theme has the application's own icon (named [`crate::APP_ID`]): an installed
/// Kourier has, a run from the build directory has not. It looks for the file an install puts
/// into the fallback theme (`icons/hicolor/scalable/apps/`), a few file lookups; asking the
/// icon theme for a name it lacks cost 12 ms of the start. Asked once, when the About data is
/// set or `Style.appIcon` is first read, so both take the same answer; the application object
/// must exist.
pub fn app_icon_in_theme() -> bool {
    static ANSWER: OnceLock<bool> = OnceLock::new();
    *ANSWER.get_or_init(|| qt::has_installed_icon(&QString::from(crate::APP_ID)))
}

/// The bundled logo as a URL QML's icons take (the file of [`crate::desktop::APP_ICON`]).
pub fn bundled_logo_url() -> String {
    format!("qrc{}", crate::desktop::APP_ICON)
}

/// Makes `info` the application's data and `desktop_file_name` its desktop file name (the
/// Wayland `app_id`). The About page shows it and the crash reporter reads it. Links that are
/// not set are left out. The About page's logo is the icon theme's icon of the application
/// ([`app_icon_in_theme`]) when the theme has one, else the bundled file, which a run that is
/// not installed has no other way to show. The application object must exist.
pub fn set_about_data(info: &AboutInfo, desktop_file_name: &str) {
    let logo = if app_icon_in_theme() {
        String::new()
    } else {
        bundled_logo_url()
    };
    let mut authors = QStringList::default();
    for credit in info.authors {
        authors.append(QString::from(credit.name));
        authors.append(QString::from(credit.task.unwrap_or_default()));
        authors.append(QString::from(credit.email.unwrap_or_default()));
    }
    let mut components = QStringList::default();
    for component in info.components {
        components.append(QString::from(component.name));
        components.append(QString::from(component.version));
        components.append(QString::from(component.license));
    }
    qt::set_about_data(
        &QString::from(info.component_name),
        &QString::from(info.display_name),
        &QString::from(info.version),
        // The summary is the text of the desktop file's Comment and of the metainfo's
        // summary, which have no context: the one place where `trc` takes an empty one.
        &QString::from(trc("", info.summary).as_ref()),
        &QString::from(info.license),
        &QString::from(info.copyright),
        &QString::from(info.homepage.unwrap_or_default()),
        &QString::from(info.bug_report.unwrap_or_default()),
        &QString::from(desktop_file_name),
        &QString::from(logo.as_str()),
        &authors,
        &components,
    );
}

/// What KDE provides for a Kirigami application inside and outside Plasma
/// (`KirigamiAppDefaults::apply`): the Qt Quick Controls style when neither the platform theme
/// nor `QT_QUICK_CONTROLS_STYLE` chose one, Breeze as the fallback icon theme, a colour scheme
/// that follows the desktop; and the window icon by the theme name `icon_name`. Right after
/// the application object was created, and after [`set_about_data`]: Kirigami Add-ons start
/// KCrash by themselves when the application data changes after this call, and a run that
/// must show no crash dialog (a test, a screenshot) would get one.
pub fn apply_defaults(icon_name: &str) {
    // A window icon by theme name only when the theme has it: the lookup of a missing name
    // is the slow one, and [`set_fallback_window_icon`] serves a run that is not installed.
    let name = if app_icon_in_theme() { icon_name } else { "" };
    qt::apply_defaults(&QString::from(name));
}

/// The window icon of a run that is not installed: the icon theme has no icon of the
/// application then ([`app_icon_in_theme`]) and [`apply_defaults`] left the windows without
/// one, so they get the bundled file. Called once the first frame is on screen: making an
/// icon of a file loads the SVG icon plugin, which the start does not wait for. An installed
/// application keeps the theme's icon and this does nothing.
pub fn set_fallback_window_icon() {
    if !app_icon_in_theme() {
        qt::set_window_icon_file(&QString::from(crate::desktop::APP_ICON));
    }
}

/// Whether the application's windows have an icon.
pub fn has_window_icon() -> bool {
    qt::has_window_icon()
}

/// Starts KCrash: a crash is handed to the desktop's crash reporter. Only for a normal run of
/// the application, never in fixture mode, for a screenshot or under the development channel.
pub fn start_crash_handler() {
    qt::initialize_crash_handler();
    CRASH_HANDLER.store(true, Ordering::Release);
}

/// The sink for `kourier_app::process::install_panic_hook`: the redacted line of a panic
/// becomes the message the crash reporter shows, once [`start_crash_handler`] has run (before
/// that, and in runs without KCrash, it does nothing).
pub fn panic_sink(line: &str) {
    if CRASH_HANDLER.load(Ordering::Acquire) {
        qt::set_crash_message(line);
    }
}
