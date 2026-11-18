//! Translations of this front-end: KI18n behind the viewmodel's [`Translator`] (architecture
//! §9.5, §8.10 "must provide" 2). Texts built in Rust and the QML strings read the same gettext
//! catalog, `kourier`: Rust through [`KdeTranslator`], QML through KI18n's context (`L10n.qml`
//! and the engine's context object, [`setup_engine`]).
//!
//! The translator only looks a message up. The arguments are filled in by
//! `kourier_viewmodel::i18n` after it returned, with one exception KI18n needs: a plural takes
//! its count to choose the form, and KI18n writes that count into `%1` itself.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use cxx_qt_lib::{QQmlApplicationEngine, QString, QStringList};
use kourier_viewmodel::i18n::{self, DOMAIN, Translator};

use crate::kde_ffi::qt;

/// The [`Translator`] over `KLocalizedString`. KI18n is thread-safe, so texts for
/// notifications can be asked for on the runtime's threads.
#[derive(Debug, Clone, Copy, Default)]
pub struct KdeTranslator;

/// The most answers [`TRANSLATIONS`] keeps: the program's literals are a few thousand at the
/// most; a full cache is cleared (`trc("", variable)` is the one lookup that is not a literal).
const CACHE_MAX: usize = 4096;

/// The answers of [`KdeTranslator::translate`] by context, then message: KI18n made three
/// allocations and a catalog lookup per call, for every UI string on every delegate update
/// (structs.md #5). A lookup by `&str` allocates nothing but the returned `String`.
static TRANSLATIONS: Mutex<Cache> = Mutex::new(Cache {
    by_context: None,
    entries: 0,
});

/// Counts the clears: an answer KI18n gave before a clear is not stored after it.
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Cache {
    by_context: Option<HashMap<String, HashMap<String, String>>>,
    entries: usize,
}

/// Forgets every answer: the domain or a locale directory or the languages changed.
fn clear_cache() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let mut cache = TRANSLATIONS.lock().unwrap_or_else(PoisonError::into_inner);
    cache.by_context = None;
    cache.entries = 0;
}

/// Tests: how many answers are kept.
#[doc(hidden)]
pub fn cache_len() -> usize {
    TRANSLATIONS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entries
}

/// Tests: the answer of KI18n itself, bypassing the cache.
#[doc(hidden)]
pub fn translate_uncached(context: &str, msgid: &str) -> String {
    String::from(&qt::translate(DOMAIN, context, msgid))
}

impl Translator for KdeTranslator {
    fn translate(&self, context: &str, msgid: &str) -> String {
        let generation = GENERATION.load(Ordering::SeqCst);
        {
            let cache = TRANSLATIONS.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(hit) = cache
                .by_context
                .as_ref()
                .and_then(|c| c.get(context))
                .and_then(|m| m.get(msgid))
            {
                return hit.clone();
            }
        }
        // KI18n runs outside the lock: a slow first lookup stops no other thread.
        let answer = translate_uncached(context, msgid);
        let mut cache = TRANSLATIONS.lock().unwrap_or_else(PoisonError::into_inner);
        if GENERATION.load(Ordering::SeqCst) == generation {
            if cache.entries >= CACHE_MAX {
                cache.by_context = None;
                cache.entries = 0;
            }
            let map = cache.by_context.get_or_insert_with(HashMap::new);
            if map
                .entry(context.to_owned())
                .or_default()
                .insert(msgid.to_owned(), answer.clone())
                .is_none()
            {
                cache.entries += 1;
            }
        }
        answer
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
    clear_cache();
    // The catalogs beside the program, when there are any: the AppImage's and those of an
    // unpacked archive that runs where it was unpacked.
    if let Some(dir) = std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(catalog_dir_beside)
        .filter(|dir| dir.is_dir())
    {
        add_catalog_dir(&dir);
    }
    // A run that is not installed finds the catalogs `cargo xtask i18n build` wrote (debug
    // builds only: an installed application reads the system's `share/locale`). KI18n keeps one
    // folder per domain, so this one wins over the one beside the program.
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

/// Where the catalogs of the program at `exe` lie when it is not installed: `<prefix>/bin/kourier`
/// has them in `<prefix>/share/locale` (the AppImage's `usr/`, an unpacked archive's top folder;
/// architecture §14.1). KI18n looks only in the XDG data folders, which hold neither.
fn catalog_dir_beside(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.parent()?.join("share/locale"))
}

/// Catalogs are also looked for under `dir` (`<dir>/<language>/LC_MESSAGES/kourier.mo`) before
/// the XDG data folders: the folder beside the program, tests, and a debug run with
/// `KOURIER_LOCALE_DIR` ([`install`]). KI18n keeps one such folder per domain: the last call wins.
#[doc(hidden)]
pub fn add_catalog_dir(dir: &Path) {
    qt::add_locale_dir(DOMAIN, &QString::from(dir.to_string_lossy().as_ref()));
    clear_cache();
}

/// Tests: the languages KI18n translates into, best first.
#[doc(hidden)]
pub fn set_languages(languages: &[&str]) {
    let mut list = QStringList::default();
    for language in languages {
        list.append(QString::from(*language));
    }
    qt::set_languages(&list);
    clear_cache();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The AppImage's mount, an unpacked archive and an installed program each have their
    /// catalogs in `share/locale` beside `bin/`; a bare name has no folder to look in.
    #[test]
    fn the_catalogs_beside_the_program_are_in_share_locale() {
        assert_eq!(
            catalog_dir_beside(Path::new("/tmp/.mount_KourieAbC123/usr/bin/kourier")),
            Some(PathBuf::from("/tmp/.mount_KourieAbC123/usr/share/locale"))
        );
        assert_eq!(
            catalog_dir_beside(Path::new("/home/p/kourier-0.1.0-linux-x86_64/bin/kourier")),
            Some(PathBuf::from(
                "/home/p/kourier-0.1.0-linux-x86_64/share/locale"
            ))
        );
        assert_eq!(catalog_dir_beside(Path::new("kourier")), None);
    }
}
