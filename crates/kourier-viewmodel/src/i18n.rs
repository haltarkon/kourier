//! The translation hook for user-facing strings built in Rust (§8.10, §9.5).
//!
//! Every label the viewmodel produces goes through [`trc`], [`trnc`] (plural) or [`trfc`]
//! (arguments): each takes a KDE context marker as its first argument, a string literal such
//! as `@info:status` or `@action:button`, optionally followed by a few words that say what
//! the string is where the English alone is ambiguous ("@info:status banner of an account, %1
//! the name of the account"). There is no function without a context. The text comes from
//! the [`Translator`] the front-end installs once at its start, before the first row is built
//! ([`install`]): KI18n for the KDE front-end, gettext for a GNOME one, both over the domain
//! [`DOMAIN`] and one catalog. With none installed the functions return the English source
//! text, which is what tests and `--fixture` runs see.
//!
//! A translator only maps a message to its translation. The placeholders are filled in here,
//! after it returned, so that every translator behaves the same and a translation may reorder
//! them: `%1`…`%9` (KDE's spelling; in a plural `%1` is the count). A value is never scanned
//! for placeholders itself, and a placeholder without a value stays as it is. Text that
//! comes from a table (the weekday names) is marked with [`trc_noop`] where the table is
//! written and translated with [`trm`] where it is used.
//!
//! `cargo xtask i18n extract` reads the call sites with its own small Rust reader (GNU
//! xgettext has no Rust parser before 0.24): the name of a function below, an opening
//! parenthesis, the context and the message as string literals. A call whose context or
//! message is not a literal is an error of `cargo xtask i18n check`, with one exception: an
//! empty context with a message that is a variable looks up a text that the desktop file or
//! the metainfo holds too (they have no contexts), for example the application's summary.
//! The keywords, in
//! xgettext's spelling: `trc:1c,2`, `trnc:1c,2,3`, `trfc:1c,2`, `trc_noop:1c,2`. A
//! `// TRANSLATORS:` comment above a call (or on its line) goes into the catalog for the
//! translators.

use std::borrow::Cow;
use std::sync::OnceLock;

/// The gettext domain of the one catalog (`po/kourier.pot`, `kourier.mo`) that the Rust and
/// the UI strings of every front-end share.
pub const DOMAIN: &str = "kourier";

/// Looks messages up in a catalog. Implemented by a front-end over its toolkit's translation
/// system and installed with [`install`]; nothing here names a toolkit. It may be called from
/// any thread (rows are built on runtime threads), hence `Send + Sync`.
///
/// Both methods return the translation with its placeholders (`%1`, …) untouched: the caller
/// fills them in. A message without a translation comes back as the English text.
pub trait Translator: Send + Sync {
    /// The translation of `msgid` in `context` (`""`: no context).
    fn translate(&self, context: &str, msgid: &str) -> String;

    /// The translation of the plural message `singular`/`plural` in `context` for `count`:
    /// the catalog's plural rule of the language picks the form, not this crate.
    fn translate_plural(&self, context: &str, singular: &str, plural: &str, count: u64) -> String;

    /// A count written the way [`Translator::translate_plural`] writes the `%1` it fills in
    /// (the locale's digit groups), so that a second number of the same message looks the
    /// same (link feedback L3: "12,400 of 50 000" under an English UI with Ukrainian
    /// numbers). `None`: this crate groups it itself ([`crate::transfer::group_digits`]).
    fn number(&self, _n: u64) -> Option<String> {
        None
    }
}

/// `n` as the translator writes counts, when it has a way (see [`Translator::number`]).
pub fn number(n: u64) -> Option<String> {
    dispatch(|t| t.and_then(|t| t.number(n)))
}

/// A second [`install`]: the first translator stays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a translator is already installed; the first one stays")]
pub struct AlreadyInstalled;

static INSTALLED: OnceLock<Box<dyn Translator>> = OnceLock::new();

/// Installs the process-wide translator. Call it once, before the first row is built. The
/// first call wins; a later one is refused with [`AlreadyInstalled`] and changes nothing (the
/// caller reports it: this crate has no logger).
pub fn install(translator: Box<dyn Translator>) -> Result<(), AlreadyInstalled> {
    INSTALLED.set(translator).map_err(|_| AlreadyInstalled)
}

/// Runs `f` with the translator of this thread's test scope, else the installed one, else
/// `None` (English).
fn dispatch<R>(f: impl FnOnce(Option<&dyn Translator>) -> R) -> R {
    #[cfg(test)]
    if let Some(scoped) = scoped::current() {
        return f(Some(&*scoped));
    }
    f(INSTALLED.get().map(|t| &**t))
}

/// A translator for the current thread and the duration of a closure, so that a unit test can
/// use one without [`install`] (which cannot be undone and would reach every other test of the
/// binary). Other threads, and what runs after the closure, see the installed translator or
/// English; the previous scope is restored even when the closure panics. Only for tests of this
/// crate: code that is not on the test's thread (a spawned one) is not in the scope.
#[cfg(test)]
pub(crate) fn with_translator<R>(
    translator: impl Translator + 'static,
    f: impl FnOnce() -> R,
) -> R {
    scoped::with(std::rc::Rc::new(translator), f)
}

#[cfg(test)]
mod scoped {
    use super::Translator;
    use std::cell::RefCell;
    use std::rc::Rc;

    thread_local! {
        static CURRENT: RefCell<Option<Rc<dyn Translator>>> = const { RefCell::new(None) };
    }

    pub(super) fn current() -> Option<Rc<dyn Translator>> {
        CURRENT.with(|c| c.borrow().clone())
    }

    pub(super) fn with<R>(translator: Rc<dyn Translator>, f: impl FnOnce() -> R) -> R {
        struct Restore(Option<Rc<dyn Translator>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT.with(|c| *c.borrow_mut() = self.0.take());
            }
        }
        let _restore = Restore(CURRENT.with(|c| c.borrow_mut().replace(translator)));
        f()
    }
}

/// A message whose translation is looked up later: where a table of texts is written
/// (`const WEEKDAYS: [Msg; 7]`) the strings must be marked for extraction, and a function
/// cannot be called in a way `trc` could take. Made by [`trc_noop`], translated by [`trm`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Msg {
    context: &'static str,
    msgid: &'static str,
}

/// Marks a message in a `const` table for extraction; the text is translated by [`trm`].
pub const fn trc_noop(context: &'static str, msgid: &'static str) -> Msg {
    Msg { context, msgid }
}

/// Translates a message marked with [`trc_noop`].
pub fn trm(msg: Msg) -> Cow<'static, str> {
    trc(msg.context, msg.msgid)
}

/// Translates a message in a KDE context (`@action:button`, `@info:status`, …). `context`
/// and `msgid` are string literals at every call site: that is what the extractor reads.
pub fn trc(context: &str, msgid: &'static str) -> Cow<'static, str> {
    dispatch(|t| match t {
        Some(t) => Cow::Owned(t.translate(context, msgid)),
        None => Cow::Borrowed(msgid),
    })
}

/// Translates a plural message in a KDE context and fills in the count (`%1`): in English
/// `singular` for `n == 1`, else `plural`.
pub fn trnc(context: &str, singular: &'static str, plural: &'static str, n: u64) -> String {
    let template = dispatch(|t| match t {
        Some(t) => Cow::Owned(t.translate_plural(context, singular, plural, n)),
        None => Cow::Borrowed(if n == 1 { singular } else { plural }),
    });
    substitute(&template, &[&n.to_string()])
}

/// Translates a plural message chosen by `n` and fills in `%1`…`%9` from `args` (not from
/// `n`): for a count written another way than plain digits (grouped thousands) as `%1`, or
/// a plural with more numbers than its count. The extractor reads it like [`trnc`].
pub fn trnfc(
    context: &str,
    singular: &'static str,
    plural: &'static str,
    n: u64,
    args: &[&str],
) -> String {
    let template = dispatch(|t| match t {
        Some(t) => Cow::Owned(t.translate_plural(context, singular, plural, n)),
        None => Cow::Borrowed(if n == 1 { singular } else { plural }),
    });
    substitute(&template, args)
}

/// Translates a message in a KDE context and fills in `%1`…`%9` from `args`.
pub fn trfc(context: &str, msgid: &'static str, args: &[&str]) -> String {
    substitute(&trc(context, msgid), args)
}

/// One pass over `template`: `%d` (d = 1..=9, not followed by another digit) becomes
/// `values[d - 1]`. A placeholder without a value, and everything else, stays as it is;
/// inserted values are not scanned.
fn substitute(template: &str, values: &[&str]) -> String {
    let bytes = template.as_bytes();
    let mut out = String::with_capacity(template.len());
    let (mut copied, mut i) = (0, 0);
    // Only ASCII starts a match and a match is ASCII: every cut falls on a character border.
    while i < bytes.len() {
        let hit = if bytes[i] == b'%' {
            percent_at(&bytes[i..], values)
        } else {
            None
        };
        match hit {
            Some((len, value)) => {
                out.push_str(&template[copied..i]);
                out.push_str(value);
                i += len;
                copied = i;
            }
            None => i += 1,
        }
    }
    out.push_str(&template[copied..]);
    out
}

/// `%1`…`%9` at the start of `b`: its length and value.
fn percent_at<'a>(b: &[u8], values: &[&'a str]) -> Option<(usize, &'a str)> {
    let digit = *b.get(1)?;
    // `%10` is a placeholder this module does not know, not `%1` and a `0`.
    if !(b'1'..=b'9').contains(&digit) || b.get(2).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    Some((2, *values.get(usize::from(digit - b'1'))?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Wraps every message in markers that show its context, and records the message ids it
    /// was asked for. A plural picks its form like French: `0` and `1` are singular.
    #[derive(Default)]
    struct Marking {
        asked: Arc<Mutex<Vec<String>>>,
    }

    impl Translator for Marking {
        fn translate(&self, context: &str, msgid: &str) -> String {
            self.asked.lock().unwrap().push(msgid.to_owned());
            format!("<{context}|{msgid}>")
        }

        fn translate_plural(
            &self,
            context: &str,
            singular: &str,
            plural: &str,
            count: u64,
        ) -> String {
            self.asked
                .lock()
                .unwrap()
                .push(format!("{singular}/{plural}"));
            let form = if count <= 1 { singular } else { plural };
            format!("<{context}|{form}>")
        }
    }

    /// Translates a few messages the way a catalog does: placeholders stay, may be reordered.
    struct Catalog;

    impl Translator for Catalog {
        fn translate(&self, _context: &str, msgid: &str) -> String {
            match msgid {
                "%1 and %2" => "%2 und %1".to_owned(),
                "Hello %1" => "Hallo %1!".to_owned(),
                "%1 left" => "%1 ist gegangen".to_owned(),
                other => other.to_owned(),
            }
        }

        fn translate_plural(&self, _c: &str, singular: &str, plural: &str, count: u64) -> String {
            match (singular, count) {
                ("%1 member", 1) => "%1 Mitglied".to_owned(),
                ("%1 member", _) => "%1 Mitglieder".to_owned(),
                (_, 1) => singular.to_owned(),
                _ => plural.to_owned(),
            }
        }
    }

    #[test]
    fn without_a_translator_the_english_text_comes_back() {
        assert_eq!(DOMAIN, "kourier");
        assert!(matches!(trc("@info", "Photo"), Cow::Borrowed("Photo")));
        assert_eq!(trc("@info:status", "Typing"), "Typing");
        assert_eq!(trnc("@info", "%1 member", "%1 members", 1), "1 member");
        assert_eq!(trnc("@info", "%1 member", "%1 members", 3), "3 members");
        assert_eq!(trnc("@info", "%1 member", "%1 members", 0), "0 members");
        assert_eq!(trfc("@info", "%1 and %2", &["A", "B"]), "A and B");
        assert_eq!(
            trnfc(
                "@info",
                "%2 of %1 item",
                "%2 of %1 items",
                1_200,
                &["1,200", "7"]
            ),
            "7 of 1,200 items"
        );
        assert_eq!(
            trnfc("@info", "%1 item", "%1 items", 1, &["one"]),
            "one item"
        );
        assert_eq!(trfc("@info", "%1 left", &["A"]), "A left");
        // Arguments are not re-scanned for placeholders.
        assert_eq!(trfc("@info", "%1: %2", &["%2", "x"]), "%2: x");
    }

    #[test]
    fn a_marked_message_is_translated_where_it_is_used() {
        const MARKED: [Msg; 2] = [trc_noop("@item", "Monday"), trc_noop("@item", "Sunday")];
        assert_eq!(trm(MARKED[0]), "Monday");
        with_translator(Marking::default(), || {
            assert_eq!(trm(MARKED[1]), "<@item|Sunday>");
        });
    }

    #[test]
    fn the_old_brace_spelling_is_not_a_placeholder() {
        assert_eq!(trfc("@info", "{0} and {1}", &["A", "B"]), "{0} and {1}");
        assert_eq!(trnc("@info", "{n} file", "{n} files", 2), "{n} files");
    }

    #[test]
    fn an_installed_translator_is_used_for_singular_and_plural() {
        with_translator(Marking::default(), || {
            assert_eq!(trc("@info", "Photo"), "<@info|Photo>");
            assert_eq!(trc("@info:status", "Typing"), "<@info:status|Typing>");
            // The translator picks the plural form: French has `0` in the singular.
            assert_eq!(trnc("@info", "%1 item", "%1 items", 0), "<@info|0 item>");
            assert_eq!(trnc("@info", "%1 item", "%1 items", 2), "<@info|2 items>");
            assert_eq!(trnc("@info", "%1 item", "%1 items", 5), "<@info|5 items>");
            assert_eq!(trfc("@title", "Poll", &[]), "<@title|Poll>");
        });
    }

    #[test]
    fn placeholders_are_filled_after_the_translator_returned() {
        let marking = Marking::default();
        let asked = Arc::clone(&marking.asked);
        with_translator(marking, || {
            // The values never reach the translator: it is asked for the template.
            assert_eq!(trfc("@info", "Hi %1", &["Anna"]), "<@info|Hi Anna>");
            assert_eq!(trnc("@info", "%1 item", "%1 items", 1), "<@info|1 item>");
        });
        assert_eq!(*asked.lock().unwrap(), ["Hi %1", "%1 item/%1 items"]);
        // A translation may reorder placeholders, and a plural form may be a different text.
        with_translator(Catalog, || {
            assert_eq!(trfc("@info", "%1 and %2", &["A", "B"]), "B und A");
            assert_eq!(trfc("@info", "Hello %1", &["Anna"]), "Hallo Anna!");
            assert_eq!(trfc("@info", "%1 left", &["Anna"]), "Anna ist gegangen");
            assert_eq!(trnc("@info", "%1 member", "%1 members", 1), "1 Mitglied");
            assert_eq!(trnc("@info", "%1 member", "%1 members", 4), "4 Mitglieder");
            // A value that looks like a placeholder is left alone.
            assert_eq!(trfc("@info", "%1 and %2", &["%2", "%1"]), "%1 und %2");
        });
    }

    #[test]
    fn placeholders_may_come_in_any_order() {
        let fill = |msgid: &'static str, args: &[&str]| trfc("@info", msgid, args);
        assert_eq!(fill("%3 %1 %2", &["a", "b", "c"]), "c a b");
        assert_eq!(fill("%2%2 %1", &["a", "b"]), "bb a");
        assert_eq!(
            fill("%9", &["1", "2", "3", "4", "5", "6", "7", "8", "z"]),
            "z"
        );
        // No value, no placeholder syntax or a second digit: the text stays as it is.
        assert_eq!(fill("%1 %2", &["a"]), "a %2");
        assert_eq!(fill("%0 %a 100% %", &["a"]), "%0 %a 100% %");
        assert_eq!(fill("%10", &["a"]), "%10");
        assert_eq!(fill("{ {0} {x} {}", &["a"]), "{ {0} {x} {}");
        // A percent sign after a placeholder is text ("Packing… 40%").
        assert_eq!(fill("Packing… %1%", &["40"]), "Packing… 40%");
        // Text around multi-byte characters is cut on character borders.
        assert_eq!(fill("«%1» → %1", &["é"]), "«é» → é");
        assert_eq!(trnc("@info", "%1 · %1", "%1 · %1", 7), "7 · 7");
    }

    #[test]
    fn a_scoped_translator_does_not_leak() {
        let hello = || trfc("@info", "Hello %1", &["x"]);
        with_translator(Catalog, || {
            assert_eq!(hello(), "Hallo x!");
            // Another thread is outside the scope.
            std::thread::spawn(|| assert_eq!(trfc("@info", "Hello %1", &["x"]), "Hello x"))
                .join()
                .unwrap();
            // A nested scope hides the outer one and gives it back.
            with_translator(Marking::default(), || {
                assert_eq!(trc("@info", "a"), "<@info|a>");
            });
            assert_eq!(hello(), "Hallo x!");
        });
        assert_eq!(hello(), "Hello x");
        // A panic inside the scope still ends it.
        let unwound = std::panic::catch_unwind(|| with_translator(Catalog, || panic!("test")));
        assert!(unwound.is_err());
        assert_eq!(hello(), "Hello x");
    }

    const CHILD_ENV: &str = "KOURIER_I18N_INSTALL_CHILD";

    /// `install` cannot be undone, so its test runs in a child process (this test binary,
    /// asked for `install_child` alone) and cannot reach any other test.
    #[test]
    fn install_keeps_the_first_translator() {
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--exact", "i18n::tests::install_child", "--nocapture"])
            .env(CHILD_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("install_child ran"),
            "the child test did not run: {stdout}"
        );
    }

    /// The body of the child process of [`install_keeps_the_first_translator`]; a no-op in the
    /// parent run.
    #[test]
    fn install_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        let hello = || trfc("@info", "Hello %1", &["x"]);
        assert_eq!(hello(), "Hello x", "English before an install");
        assert_eq!(install(Box::new(Catalog)), Ok(()));
        assert_eq!(hello(), "Hallo x!");
        assert_eq!(trnc("@info", "%1 member", "%1 members", 2), "2 Mitglieder");
        // A second one is refused and changes nothing.
        assert_eq!(install(Box::new(Marking::default())), Err(AlreadyInstalled));
        assert_eq!(hello(), "Hallo x!");
        // A test scope goes before the installed translator.
        with_translator(Marking::default(), || {
            assert_eq!(trc("@info", "a"), "<@info|a>");
        });
        println!("install_child ran");
    }
}
