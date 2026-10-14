//! Translations of this front-end: KI18n behind the viewmodel's [`Translator`] (architecture
//! §9.5, §8.10 "must provide" 2). Texts built in Rust and the QML strings read the same gettext
//! catalog, `kourier`: Rust through [`KdeTranslator`], QML through KI18n's context (`L10n.qml`
//! and the engine's context object, [`setup_engine`]).
//!
//! The translator only looks a message up. The arguments are filled in by
//! `kourier_viewmodel::i18n` after it returned, with one exception KI18n needs: a plural takes
//! its count to choose the form, and KI18n writes that count into `%1` itself.

use std::path::Path;
use std::pin::Pin;

use cxx_qt_lib::{QQmlApplicationEngine, QString, QStringList};
use kourier_viewmodel::i18n::{self, DOMAIN, Translator};

use crate::kde_ffi::qt;

/// The [`Translator`] over `KLocalizedString`. KI18n is thread-safe, so texts for
/// notifications can be asked for on the runtime's threads.
#[derive(Debug, Clone, Copy, Default)]
pub struct KdeTranslator;

impl Translator for KdeTranslator {
    fn translate(&self, context: &str, msgid: &str) -> String {
        String::from(&qt::translate(DOMAIN, context, msgid))
    }

    fn translate_plural(&self, context: &str, singular: &str, plural: &str, count: u64) -> String {
        String::from(&qt::translate_plural(
            DOMAIN, context, singular, plural, count,
        ))
    }

    fn number(&self, n: u64) -> Option<String> {
        Some(String::from(&qt::format_number(n)))
    }
}

/// Makes `kourier` KI18n's application domain and [`KdeTranslator`] the translator of every
/// text built in Rust. [`crate::run`] calls it first, before a row or `App` exists; tests that
/// load QML themselves may call it too. A translator that is installed already stays.
pub fn install() {
    qt::set_application_domain(DOMAIN);
    // A run that is not installed finds the catalogs `cargo xtask i18n build` wrote (debug
    // builds only: an installed application reads the system's `share/locale`).
    #[cfg(debug_assertions)]
    if let Some(dir) = std::env::var_os("KOURIER_LOCALE_DIR") {
        add_catalog_dir(Path::new(&dir));
    }
    if i18n::install(Box::new(KdeTranslator)).is_err() {
        tracing::debug!(target: "kourier_qt::l10n", "a translator is installed already");
    }
}

/// Gives `engine` KI18n's context object (before it loads QML): QML's own `i18n*()` functions
/// then work in every file, which Kirigami Add-ons' pages and delegates need. [`crate::run`]
/// does it; tests that create their own engine and open such a page call it too.
pub fn setup_engine(engine: Pin<&mut QQmlApplicationEngine>) {
    qt::setup_localized_context(engine, DOMAIN);
}

/// Catalogs are also looked for under `dir` (`<dir>/<language>/LC_MESSAGES/kourier.mo`):
/// tests, and a debug run with `KOURIER_LOCALE_DIR` ([`install`]).
#[doc(hidden)]
pub fn add_catalog_dir(dir: &Path) {
    qt::add_locale_dir(DOMAIN, &QString::from(dir.to_string_lossy().as_ref()));
}

/// Tests: the languages KI18n translates into, best first.
#[doc(hidden)]
pub fn set_languages(languages: &[&str]) {
    let mut list = QStringList::default();
    for language in languages {
        list.append(QString::from(*language));
    }
    qt::set_languages(&list);
}
