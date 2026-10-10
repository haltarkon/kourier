//! The KDE shim (architecture §8.10, §14.2 item 2), offscreen, `harness = false`:
//!
//! - translations go through KI18n on the gettext domain `kourier`, for texts built in Rust
//!   (the viewmodel's `Translator`, with a context and with plural forms) and for QML (the
//!   `L10n` singleton and QML's own `i18n*()` functions, which Kirigami Add-ons call);
//! - a translator only looks up: placeholders come back as they are and are filled in by the
//!   viewmodel, so a translation may reorder them;
//! - the application's `KAboutData` is `kourier_app::about::info()`, Kirigami Add-ons' About
//!   page shows it, and `App.version` is the same version.
//!
//! There is no real translation yet: the test writes a tiny catalog (a made-up language `xx`)
//! into a temporary directory and tells KI18n to look there. It also builds the pseudo-language
//! `x-test` with `cargo xtask i18n build` from the committed `po/kourier.pot` and finds it
//! through `KOURIER_LOCALE_DIR`, the way a debug run does: a message made in QML and one made
//! in Rust come back wrapped in `xx`, which proves the chain from the template to the screen.

use std::path::{Path, PathBuf};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_qt::l10n::{self, KdeTranslator};
use kourier_qt::{APP_ID, UiOptions};
use kourier_viewmodel::i18n::{self, Translator};

/// gettext's separator between a context and a message id.
const CTX: &str = "\u{4}";

/// The test catalog: message id (with its context, and for a plural both forms separated by
/// NUL) and translation (the plural forms separated by NUL).
fn catalog() -> Vec<(String, String)> {
    let mut entries = vec![
        (
            String::new(),
            "Content-Type: text/plain; charset=UTF-8\nPlural-Forms: nplurals=2; plural=(n != 1);\n"
                .to_owned(),
        ),
        (format!("@title{CTX}Settings"), "Agordoj".to_owned()),
        ("Plain %1".to_owned(), "Simpla %1".to_owned()),
        (
            format!("@info{CTX}Hello %1 and %2"),
            "Saluton %2 kaj %1".to_owned(),
        ),
        (
            format!("@info{CTX}%1 member\0%1 members"),
            "%1 membro\0%1 membroj".to_owned(),
        ),
        (
            "%1 file\0%1 files".to_owned(),
            "%1 dosiero\0%1 dosieroj".to_owned(),
        ),
    ];
    entries.sort();
    entries
}

/// Writes `entries` (sorted by message id) as a GNU `.mo` file.
fn write_mo(path: &Path, entries: &[(String, String)]) {
    let count = u32::try_from(entries.len()).expect("few entries");
    let ids_at = 28;
    let texts_at = ids_at + 8 * count;
    let mut strings = Vec::new();
    let mut table = |column: &dyn Fn(&(String, String)) -> &str| {
        let mut out = Vec::new();
        for entry in entries {
            let text = column(entry).as_bytes();
            let at = texts_at + 8 * count + u32::try_from(strings.len()).expect("small file");
            out.extend(u32::try_from(text.len()).expect("short text").to_le_bytes());
            out.extend(at.to_le_bytes());
            strings.extend(text);
            strings.push(0);
        }
        out
    };
    let ids = table(&|e| &e.0);
    let texts = table(&|e| &e.1);
    let mut file = Vec::new();
    for word in [0x9504_12de, 0, count, ids_at, texts_at, 0, 0] {
        file.extend(u32::to_le_bytes(word));
    }
    file.extend(ids);
    file.extend(texts);
    file.extend(strings);
    std::fs::create_dir_all(path.parent().expect("a directory")).expect("create the directory");
    std::fs::write(path, file).expect("write the catalog");
}

fn check(what: &str, got: &str, want: &str) {
    assert_eq!(got, want, "{what}");
}

/// Builds the pseudo-language `x-test` into `out` with the real tool, from the committed
/// template `po/kourier.pot` (`cargo xtask i18n build --out`). A message of the real catalog
/// then comes back as `xx` + text + `xx`.
fn build_pseudo_language(out: &Path) {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = std::process::Command::new(cargo)
        .current_dir(repo)
        .args([
            "run", "--quiet", "-p", "xtask", "--", "i18n", "build", "--out",
        ])
        .arg(out)
        .status()
        .expect("run cargo xtask i18n build");
    assert!(status.success(), "cargo xtask i18n build failed");
    assert!(
        out.join("x-test/LC_MESSAGES/kourier.mo").is_file(),
        "no x-test catalog in {}",
        out.display()
    );
}

/// The pseudo-language, for strings made in Rust: the viewmodel's own functions, over the
/// translator the application installs, over the catalog built from the real template.
fn check_pseudo_rust() {
    use kourier_viewmodel::preview;
    check(
        "pseudo-language: a text made in Rust",
        &preview::unsupported_label("payment").text,
        "xxPaymentxx",
    );
    check(
        "pseudo-language: a plural made in Rust, the count written by KI18n",
        &preview::timer_label(std::time::Duration::from_secs(3 * 3600)),
        "xx3 hoursxx",
    );
    check(
        "pseudo-language: arguments filled in around a translated text",
        &i18n::trfc(
            "@info:status chat notice, %1 a name",
            "%1 changed their phone number",
            &["Ana"],
        ),
        "xxAna changed their phone numberxx",
    );
}

/// The lookups of the translator itself, then the viewmodel's functions over it.
fn check_rust() {
    let t = KdeTranslator;
    check("a context", &t.translate("@title", "Settings"), "Agordoj");
    check(
        "the same text in another context is another message",
        &t.translate("@action", "Settings"),
        "Settings",
    );
    check(
        "a placeholder comes back as it is",
        &t.translate("", "Plain %1"),
        "Simpla %1",
    );
    check(
        "a text no catalog has",
        &t.translate("@info", "Not translated %1 %2"),
        "Not translated %1 %2",
    );
    check(
        "the singular",
        &t.translate_plural("@info", "%1 member", "%1 members", 1),
        "1 membro",
    );
    check(
        "the plural",
        &t.translate_plural("@info", "%1 member", "%1 members", 5),
        "5 membroj",
    );
    check(
        "a plural no catalog has keeps its other placeholders",
        &t.translate_plural("", "%1 file from %2", "%1 files from %2", 2),
        "2 files from %2",
    );
    check(
        "a plural without a context",
        &t.translate_plural("", "%1 file", "%1 files", 3),
        "3 dosieroj",
    );

    l10n::install();
    check(
        "arguments are filled in after the lookup, in the translation's order",
        &i18n::trfc("@info", "Hello %1 and %2", &["A %2", "B"]),
        "Saluton B kaj A %2",
    );
    check(
        "a plural through the viewmodel",
        &i18n::trnc("@info", "%1 member", "%1 members", 5),
        "5 membroj",
    );
    check(
        "a plural without a context through the viewmodel",
        &i18n::trnc("", "%1 file", "%1 files", 3),
        "3 dosieroj",
    );
    check("a plain text", &i18n::trc("@title", "Settings"), "Agordoj");
    check_pseudo_rust();
}

const SCRIPT: &str = r#"
import QtQuick
import org.kde.kirigamiaddons.formcard as FormCard
import app.kourier

Item {
    id: root

    function fail(msg: string): void {
        console.error("FAIL: " + msg);
        Qt.exit(1);
    }

    Component {
        id: about
        FormCard.AboutPage {}
    }

    function run(): void {
        // The singleton every QML file of the application uses.
        if (L10n.i18nc("@title", "Settings") !== "Agordoj") return fail("L10n.i18nc: " + L10n.i18nc("@title", "Settings"));
        if (L10n.i18ncp("@info", "%1 member", "%1 members", 5) !== "5 membroj") return fail("L10n.i18ncp: " + L10n.i18ncp("@info", "%1 member", "%1 members", 5));
        if (L10n.i18nc("@info", "Hello %1 and %2", "A", "B") !== "Saluton B kaj A") return fail("L10n arguments");
        // The pseudo-language `x-test` built from the real template, behind the test's own
        // catalog `xx`: a message `xx` lacks comes from it, wrapped in `xx`.
        if (L10n.i18nc("@action:button", "Add Account") !== "xxAdd Accountxx") return fail("pseudo L10n.i18nc: " + L10n.i18nc("@action:button", "Add Account"));
        if (L10n.i18ncp("@info a length of time", "%1 hour", "%1 hours", 3) !== "xx3 hoursxx") return fail("pseudo L10n.i18ncp: " + L10n.i18ncp("@info a length of time", "%1 hour", "%1 hours", 3));
        if (i18nc("@action:button", "Add Account") !== "xxAdd Accountxx") return fail("pseudo i18nc: " + i18nc("@action:button", "Add Account"));
        // QML's own functions (the engine's context object), with and without a domain.
        if (i18nc("@title", "Settings") !== "Agordoj") return fail("i18nc: " + i18nc("@title", "Settings"));
        if (i18ndc("kourier", "@title", "Settings") !== "Agordoj") return fail("i18ndc");
        if (i18nd("kirigami-addons6", "Homepage") !== "Homepage") return fail("another domain");

        // The About page reads the application's data.
        const page = about.createObject(root);
        if (page === null) return fail("the About page was not created");
        const data = page.aboutData;
        if (data.displayName !== "Kourier" || data.componentName !== "kourier") return fail("name: " + data.displayName + " / " + data.componentName);
        if (data.version !== App.version || App.version === "") return fail("version: " + data.version + " and " + App.version);
        // The summary is a text of the desktop file and the metainfo (no context), found in the pseudo-language.
        if (data.shortDescription !== "xxUnofficial messenger compatible with Signalxx") return fail("summary: " + data.shortDescription);
        if (data.desktopFileName !== expectedAppId) return fail("desktop file name: " + data.desktopFileName);
        if (data.homepage !== "" || data.bugAddress !== "") return fail("links that are not set: " + data.homepage + " " + data.bugAddress);
        if (data.licenses.length !== 1 || data.licenses[0].text.indexOf("AGPL-3.0-only") < 0) return fail("licence: " + data.licenses[0].text);
        if (data.copyrightStatement.indexOf("Kourier") < 0) return fail("copyright: " + data.copyrightStatement);
        if (data.authors.length !== 0) return fail("authors");
        if (page.title !== "About Kourier") return fail("title: " + page.title);
        if (page.donateUrl.toString() !== "" || page.getInvolvedUrl.toString() !== "") return fail("links of the KDE project");
        if (Qt.application.name !== "kourier" || Qt.application.version !== App.version || Qt.application.domain !== "") {
            return fail("application: " + Qt.application.name + " " + Qt.application.version + " " + Qt.application.domain);
        }
        Qt.exit(0);
    }

    Timer {
        interval: 1
        running: true
        onTriggered: root.run()
    }
}
"#;

/// A UTF-8 locale of this machine that is not the "C" one (`en_US` when it is there).
fn a_locale() -> Option<String> {
    let out = std::process::Command::new("locale")
        .arg("-a")
        .output()
        .ok()?;
    let listed = String::from_utf8_lossy(&out.stdout);
    let mut usable = listed.lines().filter(|l| {
        let lower = l.to_ascii_lowercase();
        !lower.starts_with("c.") && (lower.ends_with(".utf8") || lower.ends_with(".utf-8"))
    });
    let first = usable.next()?;
    Some(
        std::iter::once(first)
            .chain(usable)
            .find(|l| l.starts_with("en_US"))
            .unwrap_or(first)
            .to_owned(),
    )
}

fn main() {
    // SAFETY (all `set_var` below): single-threaded test binary, before Qt starts.
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // gettext translates nothing while the locale is "C", "POSIX" or "C.UTF-8", whatever
    // language is asked for: the test needs any other locale the machine has, and the
    // machine's own language must not decide what it reads.
    let Some(locale) = a_locale() else {
        println!("test kde shim ... ignored (`locale -a` lists no locale besides C and POSIX)");
        println!("\ntest result: ok. 0 passed; 0 failed; 1 ignored");
        return;
    };
    unsafe { std::env::set_var("LC_ALL", locale) };
    unsafe { std::env::remove_var("LANGUAGE") };

    let dir =
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("kde-{}", std::process::id()));
    write_mo(&dir.join("xx/LC_MESSAGES/kourier.mo"), &catalog());
    // KI18n looks for a domain's catalogs in one directory, so the pseudo-language goes into
    // the directory of the test's own catalog `xx`.
    build_pseudo_language(&dir);
    // The application finds the pseudo-language like a debug run does: by this variable, in
    // `l10n::install()`.
    unsafe { std::env::set_var("KOURIER_LOCALE_DIR", &dir) };

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_options(UiOptions::default());
    let mut app = QGuiApplication::new();
    l10n::add_catalog_dir(&dir);
    l10n::set_languages(&["xx", "x-test"]);
    check_rust();
    kourier_qt::kde::set_about_data(&kourier_app::about::info(), APP_ID);
    // A run that is not installed has no icon of the application in the theme: its windows
    // get the bundled file (an installed one keeps the theme's icon, which this leaves alone).
    kourier_qt::kde::set_fallback_window_icon();
    assert!(
        kourier_qt::kde::has_window_icon() || kourier_qt::kde::app_icon_in_theme(),
        "a run that is not installed has no window icon"
    );

    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        l10n::setup_engine(engine.as_mut());
        engine.load_data(
            &QByteArray::from(
                SCRIPT
                    .replace("expectedAppId", &format!("\"{APP_ID}\""))
                    .as_str(),
            ),
            &QUrl::from("qrc:/kourier-tests/kde.qml"),
        );
    }
    let code = app.as_mut().map_or(3, |app| app.exec());
    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
    if code == 0 {
        println!("test kde shim ... ok\n\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!(
            "test kde shim ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
