//! The C++ side of the KDE shim (`cpp/kde.cpp`, `cpp/i18n.cpp`): the only place where KDE
//! Frameworks are called (architecture §8.10). [`crate::kde`] and [`crate::l10n`] are the Rust
//! side. (It lives next to `bridge.rs`: cxx-qt wants every bridge of a QML module in one
//! directory.)

#[cxx_qt::bridge]
pub mod qt {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        /// Qt string.
        type QString = cxx_qt_lib::QString;
        include!("cxx-qt-lib/qstringlist.h");
        /// Qt string list.
        type QStringList = cxx_qt_lib::QStringList;
        include!("cxx-qt-lib/qqmlapplicationengine.h");
        /// The QML engine.
        type QQmlApplicationEngine = cxx_qt_lib::QQmlApplicationEngine;
    }

    #[namespace = "kourier::kde"]
    unsafe extern "C++" {
        include!("kourier-qt/cpp/kde.h");

        /// Sets the application's `KAboutData`. `program_logo` (a theme icon name or a URL;
        /// empty: none) is the About page's logo. `authors` and `components` are flat lists
        /// of three strings per entry (name, task, e-mail; name, version, SPDX licence).
        #[cxx_name = "setAboutData"]
        #[allow(clippy::too_many_arguments)]
        fn set_about_data(
            component_name: &QString,
            display_name: &QString,
            version: &QString,
            summary: &QString,
            spdx: &QString,
            copyright: &QString,
            homepage: &QString,
            bug_address: &QString,
            desktop_file_name: &QString,
            program_logo: &QString,
            authors: &QStringList,
            components: &QStringList,
        );

        /// An icon of this name is installed in the fallback theme of every icon theme
        /// (`icons/hicolor/scalable/apps/<name>.svg` in a data directory).
        #[cxx_name = "hasInstalledIcon"]
        fn has_installed_icon(name: &QString) -> bool;

        /// `KirigamiAppDefaults::apply` and the window icon by its theme name (empty: none).
        #[cxx_name = "applyDefaults"]
        fn apply_defaults(icon_name: &QString);

        /// `QGuiApplication::setWindowIcon` with the icon of the file `path`.
        #[cxx_name = "setWindowIconFile"]
        fn set_window_icon_file(path: &QString);

        /// Whether the application has a window icon.
        #[cxx_name = "hasWindowIcon"]
        fn has_window_icon() -> bool;

        /// Gives `engine` KI18n's context object for the catalog `domain`.
        #[cxx_name = "setupLocalizedContext"]
        fn setup_localized_context(engine: Pin<&mut QQmlApplicationEngine>, domain: &str);

        /// `KCrash::initialize`.
        #[cxx_name = "initializeCrashHandler"]
        fn initialize_crash_handler();

        /// `KCrash::setErrorMessage`.
        #[cxx_name = "setCrashMessage"]
        fn set_crash_message(line: &str);
    }

    #[namespace = "kourier::i18n"]
    unsafe extern "C++" {
        include!("kourier-qt/cpp/i18n.h");

        /// `KLocalizedString::setApplicationDomain`.
        #[cxx_name = "setApplicationDomain"]
        fn set_application_domain(domain: &str);

        /// The translated template of `msgid` (`context` may be empty).
        fn translate(domain: &str, context: &str, msgid: &str) -> QString;

        /// The translated plural form for `count`, with `%1` already filled in by KI18n.
        #[cxx_name = "translatePlural"]
        fn translate_plural(
            domain: &str,
            context: &str,
            singular: &str,
            plural: &str,
            count: u64,
        ) -> QString;

        /// A count as KI18n writes the `%1` of a plural (the locale's digit groups).
        #[cxx_name = "formatNumber"]
        fn format_number(n: u64) -> QString;

        /// Tests: one more directory to look for catalogs of `domain` in.
        #[cxx_name = "addLocaleDir"]
        fn add_locale_dir(domain: &str, dir: &QString);

        /// Tests: the languages to translate into, best first.
        #[cxx_name = "setLanguages"]
        fn set_languages(languages: &QStringList);
    }
}
