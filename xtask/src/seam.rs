//! The front-end seam rules of docs/architecture.md §8.10 that `cargo xtask deps-check`
//! enforces: R1 (only front-end crates link a toolkit or a desktop framework) and R3 (logic
//! every front-end would need goes below the seam, so a front-end's Rust file that names
//! nothing of its toolkit has to be listed with a reason).

use crate::workspace::{Kind, Workspace};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

/// The front-end crates: each draws the UI with one toolkit, and its binary links it. A second
/// front-end is a new line here (§8.10).
pub const FRONTENDS: [&str; 2] = ["kourier-qt", "kourier"];

/// Name prefixes of toolkit and desktop-framework crates. Besides a front-end, no workspace
/// package may have one as a normal, dev or build dependency, directly or through other crates.
/// Qt is linked through cxx-qt (`cxx`, `cxx-qt*`, `qt-build-utils`); the rest are the GTK
/// family's crates for the day a second front-end appears.
pub const TOOLKIT_PREFIXES: [&str; 14] = [
    "cxx",
    "cxx-qt",
    "qt-build-utils",
    "qmetaobject",
    "gtk",
    "libadwaita",
    "glib",
    "gio",
    "gdk",
    "gsk4",
    "graphene",
    "cairo",
    "pango",
    "relm4",
];

/// Packages outside the front-ends that link a toolkit on purpose, with the reason.
pub const TOOLKIT_EXCEPTIONS: [(&str, &str); 1] = [(
    "kourier-coexist",
    "test-only: runs BoringSSL, SQLCipher and Qt's OpenSSL TLS backend in one process (§5.3), \
     which needs Qt linked; nothing may depend on it",
)];

/// What counts as naming a toolkit in a Rust file.
#[derive(Clone, Copy)]
pub enum Mark {
    /// An identifier that starts with this (`cxx` covers `cxx_qt` and `cxx_qt_lib`).
    Prefix(&'static str),
    /// This identifier.
    Ident(&'static str),
    /// A Qt class: `Q`, an upper-case letter, and a lower-case letter somewhere (`QString`,
    /// not the constant `QUEUE_LEN`).
    QType,
    /// This identifier followed by `::`.
    Path(&'static str),
}

/// Qt's marks in the Qt crate: the cxx-qt bridge, its `qobject`, Qt classes and `ffi::` calls.
const QT_MARKS: [Mark; 4] = [
    Mark::Prefix("cxx"),
    Mark::Ident("qobject"),
    Mark::QType,
    Mark::Path("ffi"),
];

/// The binary names its front-end crate (and starts cxx-qt's crate initialisation).
const BINARY_MARKS: [Mark; 2] = [Mark::Prefix("cxx"), Mark::Ident("kourier_qt")];

/// The front-end crates whose sources R3 reads: (package, toolkit, what names it).
const R3_CRATES: [(&str, &str, &[Mark]); 2] = [
    ("kourier-qt", "Qt", &QT_MARKS),
    ("kourier", "Qt", &BINARY_MARKS),
];

/// Rust files of a front-end that name nothing of its toolkit and stay there on purpose
/// (path from the workspace root, reason). Each is logic a second front-end would need as
/// well; the debt is §8.10's "Known debts".
pub const R3_LISTED: [(&str, &str); 4] = [
    (
        "crates/kourier-qt/src/pump.rs",
        "the hand-over queue of UiEvents from the runtime to the UI thread: scheduling \
         logic tested with plain threads; moves below the seam when a second front-end starts",
    ),
    (
        "crates/kourier-qt/src/shared.rs",
        "the row source and the pending model refreshes, as plain Rust over `UiSource` and \
         `Changes`; moves below the seam with the composer's session state",
    ),
    (
        "crates/kourier-qt/src/media.rs",
        "the picture policy of §8.6 (`Codec::sniff`, the media store and request parser) and \
         the decrypting read behind the image provider; the Qt-free part moves below the seam \
         when a second front-end starts, a front-end must keep the rule either way",
    ),
    (
        "crates/kourier-qt/src/dev/protocol.rs",
        "the wire format of the development control channel, JSON lines that `cargo xtask \
         demo`, `smoke` and `perf` speak; shared by any front-end that implements it",
    ),
];

/// R1 and R3 over the tree at `root`.
pub fn check(root: &Path, ws: &Workspace) -> Result<Vec<String>> {
    let mut errors = toolkit_links(ws, &FRONTENDS, &TOOLKIT_PREFIXES, &TOOLKIT_EXCEPTIONS);
    for (package, toolkit, marks) in R3_CRATES {
        let Some(krate) = ws.package(package) else {
            continue; // reported by `toolkit_links`
        };
        let src = root.join(&krate.dir).join("src");
        let mut sources = Vec::new();
        for file in crate::rust_files(&src)? {
            let rel = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            sources.push((rel, text));
        }
        sources.sort();
        errors.extend(unlisted_toolkit_free(&sources, marks, &R3_LISTED, toolkit));
    }
    for (path, _) in R3_LISTED {
        if !root.join(path).is_file() {
            errors.push(format!(
                "{path}: listed in R3_LISTED (xtask/src/seam.rs) but there is no such file"
            ));
        }
    }
    Ok(errors)
}

/// R1: one error per way a package that is neither a front-end nor an exception reaches a
/// toolkit crate, a front-end or an exception. The search runs through external crates; another
/// member that links one is reported under its own name, so a leak is reported once, where it
/// is.
pub fn toolkit_links(
    ws: &Workspace,
    frontends: &[&str],
    prefixes: &[&str],
    exceptions: &[(&str, &str)],
) -> Vec<String> {
    let mut errors = Vec::new();
    for name in frontends.iter().chain(exceptions.iter().map(|(n, _)| n)) {
        if !ws.is_member(name) {
            errors.push(format!(
                "{name}: listed in xtask/src/seam.rs but not a workspace package"
            ));
        }
    }
    let is_exception = |n: &str| exceptions.iter().any(|(e, _)| *e == n);
    for pkg in &ws.packages {
        let start = pkg.name.as_str();
        if frontends.contains(&start) || is_exception(start) {
            continue;
        }
        let mut parent: BTreeMap<&str, (&str, Kind)> = BTreeMap::new();
        let mut seen = BTreeSet::from([start]);
        let mut queue = VecDeque::from([start]);
        let mut hits = Vec::new();
        while let Some(current) = queue.pop_front() {
            for (to, kind) in ws.deps_of(current) {
                let to = to.as_str();
                if !seen.insert(to) {
                    continue;
                }
                parent.insert(to, (current, *kind));
                if prefixes.iter().any(|p| to.starts_with(p)) {
                    hits.push((to, "a toolkit crate"));
                } else if frontends.contains(&to) {
                    hits.push((to, "a front-end crate"));
                } else if is_exception(to) {
                    hits.push((to, "a crate that links a toolkit"));
                } else if !ws.is_member(to) {
                    queue.push_back(to);
                }
            }
        }
        for (hit, what) in hits {
            let mut path = vec![hit];
            let mut last = None;
            let mut at = hit;
            while let Some(&(from, kind)) = parent.get(at) {
                last.get_or_insert(kind);
                path.push(from);
                at = from;
            }
            path.reverse();
            errors.push(format!(
                "{} ({} dependency): {hit} is {what}; only front-end crates ({}) may link a \
                 toolkit (architecture §8.10 R1)",
                path.join(" -> "),
                last.map_or("?", Kind::label),
                frontends.join(", "),
            ));
        }
    }
    errors
}

/// R3: the files of `sources` (path, text) that name nothing of the toolkit through `marks` and
/// are not in `listed`, as messages.
pub fn unlisted_toolkit_free(
    sources: &[(String, String)],
    marks: &[Mark],
    listed: &[(&str, &str)],
    toolkit: &str,
) -> Vec<String> {
    sources
        .iter()
        .filter(|(path, _)| !listed.iter().any(|(l, _)| l == path))
        .filter(|(_, text)| !names_toolkit(text, marks))
        .map(|(path, _)| {
            format!(
                "{path}: names nothing of {toolkit}; move it below the seam (kourier-viewmodel, \
                 kourier-app, kourier-desktop) or list it with a reason in R3_LISTED \
                 (xtask/src/seam.rs; architecture §8.10 R3)"
            )
        })
        .collect()
}

/// Whether the code of `source` (comments and literals left out) has an identifier any of
/// `marks` matches.
pub fn names_toolkit(source: &str, marks: &[Mark]) -> bool {
    let code = code_only(source);
    let bytes = code.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if is_ident_start(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
            let token = &code[start..i];
            let rest = code[i..].trim_start();
            if marks.iter().any(|mark| matches(*mark, token, rest)) {
                return true;
            }
        } else if bytes[i].is_ascii_digit() {
            // A number with a suffix (`1u8`) is not an identifier.
            while i < bytes.len() && is_ident_byte(bytes[i]) {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    false
}

fn matches(mark: Mark, token: &str, rest: &str) -> bool {
    match mark {
        Mark::Prefix(prefix) => token.starts_with(prefix),
        Mark::Ident(name) => token == name,
        Mark::QType => {
            let mut chars = token.chars();
            chars.next() == Some('Q')
                && chars.next().is_some_and(|c| c.is_ascii_uppercase())
                && token.chars().any(|c| c.is_ascii_lowercase())
        }
        Mark::Path(name) => token == name && rest.starts_with("::"),
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `source` with comments, string literals and character literals blanked out. Lifetimes are
/// kept, a raw string ends at its own number of hashes and block comments nest.
fn code_only(source: &str) -> String {
    let b = source.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 1;
                i += 2;
                while i < b.len() && depth > 0 {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                out.push(b' ');
            }
            b'r' if raw_string_hashes(b, i).is_some() => {
                let hashes = raw_string_hashes(b, i).unwrap_or(0);
                let mut end = vec![b'"'];
                end.extend(std::iter::repeat_n(b'#', hashes));
                i += 2 + hashes;
                while i < b.len() && !b[i..].starts_with(&end) {
                    i += 1;
                }
                i = (i + end.len()).min(b.len());
                out.push(b' ');
            }
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                out.push(b' ');
            }
            b'\'' => match char_literal_len(&source[i + 1..]) {
                Some(len) => {
                    i += 1 + len;
                    out.push(b' ');
                }
                None => {
                    out.push(b'\'');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The number of `#` of a raw string literal starting with the `r` at `b[i]`, if one does.
fn raw_string_hashes(b: &[u8], i: usize) -> Option<usize> {
    if i > 0 && is_ident_byte(b[i - 1]) && b[i - 1] != b'b' {
        return None;
    }
    let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
    (b.get(i + 1 + hashes) == Some(&b'"')).then_some(hashes)
}

/// The length in bytes of the rest of a character literal (`text` follows the opening quote),
/// or `None` when the quote starts a lifetime.
fn char_literal_len(text: &str) -> Option<usize> {
    let mut chars = text.char_indices();
    let (_, first) = chars.next()?;
    if first == '\\' {
        // `'\''`, `'\n'`, `'\u{1F600}'`: skip the escaped character, then find the close.
        let (at, escaped) = chars.next()?;
        let from = at + escaped.len_utf8();
        let close = text[from..].find('\'').filter(|&n| n < 10)?;
        return Some(from + close + 1);
    }
    let (at, second) = chars.next()?;
    (second == '\'').then_some(at + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::tests::made_up;

    fn graph(edges: &[(&str, &str, Kind)]) -> Workspace {
        let members = [
            "kourier-model",
            "kourier-core",
            "kourier-app",
            "kourier-viewmodel",
            "kourier-qt",
            "kourier",
            "kourier-coexist",
            "kourier-e2e",
        ]
        .map(|n| (n, "", &[][..]));
        made_up(&members, edges)
    }

    fn r1(ws: &Workspace) -> Vec<String> {
        toolkit_links(ws, &FRONTENDS, &TOOLKIT_PREFIXES, &TOOLKIT_EXCEPTIONS)
    }

    #[test]
    fn front_ends_and_the_exception_may_link_a_toolkit() {
        let ws = graph(&[
            ("kourier-core", "kourier-model", Kind::Normal),
            ("kourier-viewmodel", "kourier-core", Kind::Normal),
            ("kourier-qt", "kourier-viewmodel", Kind::Normal),
            ("kourier-qt", "cxx-qt", Kind::Normal),
            ("kourier-qt", "cxx-qt-build", Kind::Build),
            ("kourier-qt", "gtk4", Kind::Dev),
            ("kourier", "kourier-qt", Kind::Normal),
            ("kourier", "cxx-qt", Kind::Normal),
            ("kourier-coexist", "cxx-qt-build", Kind::Build),
            ("kourier-coexist", "qt-build-utils", Kind::Build),
        ]);
        assert_eq!(r1(&ws), Vec::<String>::new());
    }

    #[test]
    fn a_build_dependency_on_a_toolkit_is_a_leak() {
        let ws = graph(&[("kourier-viewmodel", "cxx-qt-build", Kind::Build)]);
        let errors = r1(&ws);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("kourier-viewmodel -> cxx-qt-build (build dependency)"));
        assert!(errors[0].contains("R1"));
    }

    #[test]
    fn a_dev_dependency_on_a_toolkit_is_a_leak() {
        let ws = graph(&[("kourier-app", "libadwaita", Kind::Dev)]);
        let errors = r1(&ws);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("kourier-app -> libadwaita (dev dependency)"));
    }

    #[test]
    fn a_toolkit_behind_an_external_crate_is_a_leak() {
        let ws = graph(&[
            ("kourier-app", "some-widgets", Kind::Normal),
            ("some-widgets", "glib-sys", Kind::Normal),
        ]);
        let errors = r1(&ws);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("kourier-app -> some-widgets -> glib-sys (normal"));
    }

    #[test]
    fn a_toolkit_behind_another_member_is_reported_where_it_is() {
        let ws = graph(&[
            ("kourier-app", "kourier-viewmodel", Kind::Normal),
            ("kourier-viewmodel", "relm4", Kind::Normal),
        ]);
        let errors = r1(&ws);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("kourier-viewmodel -> relm4"));
    }

    #[test]
    fn depending_on_a_front_end_or_the_exception_is_a_leak() {
        let ws = graph(&[
            ("kourier-app", "kourier-qt", Kind::Normal),
            ("kourier-e2e", "kourier-coexist", Kind::Dev),
        ]);
        let errors = r1(&ws);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("kourier-app -> kourier-qt"));
        assert!(errors[1].contains("kourier-e2e -> kourier-coexist"));
    }

    #[test]
    fn unrelated_crates_are_not_toolkits() {
        let ws = graph(&[
            ("kourier-core", "tokio", Kind::Normal),
            ("kourier-core", "gimli-like", Kind::Normal),
            ("kourier-app", "cc", Kind::Build),
            ("kourier-app", "pkg-config", Kind::Build),
        ]);
        assert_eq!(r1(&ws), Vec::<String>::new());
    }

    #[test]
    fn a_listed_front_end_that_is_not_a_package_is_reported() {
        let ws = graph(&[]);
        let errors = toolkit_links(&ws, &["kourier-gtk"], &TOOLKIT_PREFIXES, &[]);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("kourier-gtk"));
    }

    fn marks_in(source: &str) -> bool {
        names_toolkit(source, &QT_MARKS)
    }

    #[test]
    fn a_file_that_uses_the_toolkit_names_it() {
        assert!(marks_in("use cxx_qt_lib::QString;"));
        assert!(marks_in("#[cxx_qt::bridge]\nmod ffi {}"));
        assert!(marks_in("let s: QString = s.into();"));
        assert!(marks_in("pub struct Thing { object: QObject }"));
        assert!(marks_in("fn go() { ffi::start(); }"));
        assert!(marks_in("fn go() { ffi :: start(); }"));
        assert!(marks_in("#[qobject]\npub struct Model;"));
        assert!(marks_in("let r = QRect::new();"));
    }

    #[test]
    fn a_toolkit_free_file_names_nothing() {
        assert!(!marks_in("use std::sync::Arc;\npub struct PumpQueue;"));
        assert!(!marks_in("const QUEUE_LEN: usize = 8;\nlet q = QR_SIZE;"));
        assert!(!marks_in("let ffi_ready = true; mod ffi_like {}"));
        assert!(!marks_in("fn f<'a>(x: &'a str) -> &'a str { x }"));
        assert!(!marks_in("let n = 1u8; let qt = n;"));
    }

    #[test]
    fn comments_and_literals_do_not_count() {
        assert!(!marks_in(
            "// Qt-free: no QObject here, no cxx_qt, no ffi::\nfn f() {}"
        ));
        assert!(!marks_in(
            "/* a QString /* nested QObject */ still comment */ fn f() {}"
        ));
        assert!(!marks_in("//! the QML side calls ffi::thing\nfn f() {}"));
        assert!(!marks_in(r#"let s = "QString and \" cxx_qt"; let t = 1;"#));
        assert!(!marks_in(
            r####"let s = r##"QObject "# still raw"##; let t = 1;"####
        ));
        assert!(!marks_in("let c = '\"'; let q = '\\''; let u = 'x';"));
        // The code after a literal still counts.
        assert!(marks_in(r#"let s = "text"; let q: QString;"#));
        assert!(marks_in("let c = 'a'; let q: QUrl;"));
        assert!(marks_in("fn f<'a>(x: &'a QImage) {}"));
    }

    fn sources(files: &[(&str, &str)]) -> Vec<(String, String)> {
        files
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect()
    }

    #[test]
    fn an_unlisted_toolkit_free_file_fails_and_a_listed_one_passes() {
        let files = sources(&[
            ("crates/kourier-qt/src/app.rs", "use cxx_qt_lib::QString;"),
            ("crates/kourier-qt/src/new.rs", "pub fn plain() {}"),
            ("crates/kourier-qt/src/pump.rs", "pub struct PumpQueue;"),
        ]);
        let listed = [("crates/kourier-qt/src/pump.rs", "queue logic")];
        let errors = unlisted_toolkit_free(&files, &QT_MARKS, &listed, "Qt");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].starts_with("crates/kourier-qt/src/new.rs: names nothing of Qt"));
        assert!(errors[0].contains("move it below the seam"));
        assert!(errors[0].contains("list it with a reason"));
    }

    #[test]
    fn the_binary_is_a_front_end_while_it_names_its_front_end_crate() {
        let marks = &BINARY_MARKS;
        assert!(names_toolkit("use kourier_qt::{PAGES, UiOptions};", marks));
        assert!(names_toolkit("cxx_qt::init_crate!(kourier_qt);", marks));
        assert!(!names_toolkit("use kourier_app::AppHandle;", marks));
    }

    #[test]
    fn the_listed_files_have_reasons() {
        for (path, reason) in R3_LISTED {
            assert!(path.starts_with("crates/kourier-qt/src/"), "{path}");
            assert!(reason.len() > 30, "{path}");
        }
        for (name, reason) in TOOLKIT_EXCEPTIONS {
            assert!(reason.len() > 30, "{name}");
        }
    }
}
