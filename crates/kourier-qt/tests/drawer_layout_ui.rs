//! The accounts drawer's layout (step F1, `target/next/trayed-fix-decisions.md` item 2b): the
//! REAL `qml/AccountsDrawer.qml` in a Kirigami window with stand-ins for the app's singletons
//! (`App`, `Style`, `L10n`, `Avatar`, `UnreadPill`), no core and no fake network.
//!
//! The crash it guards (found by the user's first real test and again by `account_actions_ui`
//! case `trayed`, a SIGSEGV 13-15 % of runs, once a hang): the account entries were direct
//! children of the drawer's `ColumnLayout`. A Qt Quick Layout keeps a removed child in its
//! engine until its next polish, the Repeater deletes the entry before that, and a window
//! hidden in the tray never polishes: the next size read of the drawer (Kirigami's
//! `GlobalDrawer.qml` reads `mainColumn.Layout.minimumHeight`) walks the freed entry. Here the
//! read is made by the case itself, at the one moment that does not depend on timing: window
//! hidden, account row removed, the entry deleted, no polish in between. Run against the old
//! `AccountsDrawer.qml` every case crashed (SIGSEGV) in every run measured (5 of 5 of each when
//! the test was written, 9 of 9 and 3 of 3 later); with the entries in a `Column` it passes.
//!
//! The crash itself needs the freed memory to be read, and whether that is a SIGSEGV depends on
//! the allocator (in the real app the freed slot is usually reused by a live item at once and
//! the walk is silent), so the verdict does not rest on it alone: (1) before the window is
//! hidden every case fails when an account entry's parent is a Qt Quick Layout (the check of
//! `account_actions_ui`, `entriesOutsideLayouts()`): deterministic, the shape that makes the
//! crash possible; (2) the children run with `MALLOC_PERTURB_=165`, so freed memory is filled
//! with a pattern and a walk over it crashes or spins (the 90 s kill below catches a spin).
//!
//! Cases (`KOURIER_DRAWER_LAYOUT_ONLY=a,b`): `wide-one` (a rail beside the page, the only
//! account goes), `wide-two` (one of two goes), `wide-two-expanded` (the sidebar expanded, so the
//! names and the boxes of blended chats show; one of two goes: the box flag follows late),
//! `narrow-one` (a modal drawer, closed). Each
//! case is a child process of its own, which the parent kills after 90 s (a stale walk can
//! also spin for ever) and whose directory below `CARGO_TARGET_TMPDIR` the parent removes
//! whatever the child's status was. `harness = false`, offscreen.

mod support;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

/// The real drawer; only the module it imports is the stand-in one.
const DRAWER: &str = include_str!("../qml/AccountsDrawer.qml");

const QMLDIR: &str = "module mockkourier
singleton App 1.0 App.qml
singleton Style 1.0 Style.qml
singleton L10n 1.0 L10n.qml
Avatar 1.0 Avatar.qml
UnreadPill 1.0 UnreadPill.qml
AccountsDrawer 1.0 AccountsDrawer.qml
";

const APP: &str = r#"pragma Singleton
import QtQuick
QtObject {
    property ListModel accounts: ListModel {
        ListElement { accountId: 1; name: "First"; avatarUrl: ""; networkName: "Signal"; networkIcon: ""; status: "online"; statusLabel: "Online"; unread: 0; number: "+1"; device: ""; shown: true; place: 1 }
        // (a second one is appended by the case)
    }
    property bool hasAccounts: accounts.count > 0
    property real currentAccount: -1
    property var accountActions: []
    property bool blend: true
    property bool allShown: true
    property int shownCount: accounts.count
    property int hiddenUnread: 0
    function selectAccount(a) {}
    function toggleAccount(a) { return true; }
    function askReplace(a) {}
    function askAccountAction(a, b) {}
}
"#;

const STYLE: &str = r#"pragma Singleton
import QtQuick
QtObject {
    property string appIcon: "dialog-messages"
    property bool highContrast: false
    function plain(t) { return t; }
    function secondary(a, b) { return a; }
}
"#;

const L10N: &str = "pragma Singleton
import QtQuick
QtObject {
    function i18nc(c, t) { return t; }
    function i18ncp(c, s, p, n) { return n === 1 ? s : p; }
}
";

const AVATAR: &str = "import QtQuick
Item { property string name; property string source; property string badge; property int badgeNumber; property string status }
";

const PILL: &str = "import QtQuick
Item { property int count; property bool compact }
";

const SCRIPT: &str = r##"
import QtQuick
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import mockkourier

Kirigami.ApplicationWindow {
    id: win

    readonly property string mode: "CASE"
    width: win.mode === "narrow-one" ? 600 : 1400
    height: 800
    visible: true

    globalDrawer: AccountsDrawer {
        id: drawer
        window: win
    }

    property string step: "start"
    property int waits: 0
    property int quiet: 0
    property Item entry: null
    property int layoutsRead: 0
    property bool shapeChecked: false

    function fail(msg: string): void {
        console.error("FAIL [" + win.mode + "/" + win.step + "]: " + msg);
        win.step = "failed";
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 300) {
            win.fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.quiet = 0;
    }

    function find(root: Item, pred: var): Item {
        if (root === null) {
            return null;
        }
        if (pred(root)) {
            return root;
        }
        for (const child of root.children) {
            const hit = win.find(child, pred);
            if (hit) {
                return hit;
            }
        }
        return null;
    }

    function findAll(root: Item, pred: var, out: var): void {
        if (root === null) {
            return;
        }
        if (pred(root)) {
            out.push(root);
        }
        for (const child of root.children) {
            win.findAll(child, pred, out);
        }
    }

    // The shape that makes the crash possible, judged without a crash: an account entry whose
    // parent is a Qt Quick Layout (a layout keeps a removed child in its engine until it
    // polishes; a positioner such as Column drops it at once). Same check as account_actions_ui.
    function entriesOutsideLayouts(): bool {
        const entries = [];
        win.findAll(drawer.contentItem, i => i.menuAccount > 0, entries);
        if (entries.length === 0) {
            win.fail("no account entry in the drawer");
            return false;
        }
        for (const e of entries) {
            const p = e.parent;
            if (p === null || p === undefined || String(p).indexOf("Layout") >= 0 || p.uniformCellSizes !== undefined) {
                win.fail("an account entry of the drawer sits in a layout: " + String(p));
                return false;
            }
        }
        return true;
    }

    Component.onCompleted: {
        if (win.mode.startsWith("wide-two")) {
            App.accounts.append({ accountId: 2, name: "Second", avatarUrl: "", networkName: "Signal", networkIcon: "", status: "online", statusLabel: "Online", unread: 0, number: "+2", device: "", shown: true, place: 2 });
        }
    }

    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: win.tick()
    }

    function tick(): void {
        switch (win.step) {
        case "start": {
            const e = win.find(drawer.contentItem, i => i.menuAccount === 1);
            if (e === null) return win.later("the entry of the first account");
            if (win.mode === "wide-one" && !drawer.rail) return win.fail("the drawer is no rail in a wide window");
            if (win.mode === "narrow-one" && !drawer.modal) return win.fail("the drawer is not modal in a narrow window");
            if (win.mode === "wide-two-expanded") {
                if (drawer.collapsed) {
                    drawer.collapsed = false;
                    return win.later("the sidebar expands");
                }
                const box = win.find(e, i => i.objectName === "entryBox");
                if (box === null || !box.visible) return win.later("the boxes beside the names");
            }
            win.entry = e;
            if (!win.shapeChecked) {
                win.shapeChecked = true;
                if (!win.entriesOutsideLayouts()) return;
            }
            win.quiet += 1;
            if (win.quiet < 5) return;
            // The window goes to the tray, then the account goes: nothing polishes from now on.
            win.hide();
            win.next("remove");
            return;
        }
        case "remove":
            App.accounts.remove(0);
            win.next("freed");
            return;
        case "freed":
            // The Repeater's delete is a deferred one: wait until the entry is gone, and a few
            // more turns (still hidden, so still no polish).
            if (win.entry !== null) return win.later("the entry is deleted");
            win.quiet += 1;
            if (win.quiet < 3) return;
            win.next("read");
            return;
        case "read": {
            // What Kirigami's GlobalDrawer does with its main column, for every layout above
            // an entry that stays ("Add Account"): the minimum size, through the attached
            // property, as a binding of QML would read it.
            const stays = win.find(drawer.contentItem, i => i.label === "Add Account");
            if (stays === null) return win.fail("no 'Add Account' entry");
            let sum = 0;
            for (let p = stays.parent; p !== null; p = p.parent) {
                if (p.uniformCellSizes !== undefined) {
                    sum += p.Layout.minimumHeight + p.Layout.minimumWidth;
                    win.layoutsRead += 1;
                }
            }
            if (win.layoutsRead < 3) return win.fail("only " + win.layoutsRead + " layouts above the entry (Kirigami's main column is the third)");
            console.log("layouts read: " + win.layoutsRead + ", sum " + Math.round(sum));
            win.show();
            win.next("shown");
            return;
        }
        case "shown":
            // The window comes back: the pending polishes run, and so would a walk.
            win.quiet += 1;
            if (win.quiet < 20) return;
            if (App.hasAccounts !== win.mode.startsWith("wide-two")) return win.fail("hasAccounts " + App.hasAccounts);
            console.log("drawer layout test " + win.mode + ": ok");
            Qt.exit(0);
            return;
        }
    }
}
"##;

/// Writes the stand-in module and the real drawer below `dir` and returns the module's path.
fn write_module(dir: &Path) {
    let module = dir.join("mockkourier");
    std::fs::create_dir_all(&module).expect("the module directory");
    let drawer = DRAWER.replace("import app.kourier", "import mockkourier");
    assert!(
        drawer.contains("import mockkourier") && !drawer.contains("app.kourier"),
        "AccountsDrawer.qml imports something this test does not stand in for"
    );
    for (name, text) in [
        ("qmldir", QMLDIR),
        ("App.qml", APP),
        ("Style.qml", STYLE),
        ("L10n.qml", L10N),
        ("Avatar.qml", AVATAR),
        ("UnreadPill.qml", PILL),
        ("AccountsDrawer.qml", drawer.as_str()),
    ] {
        std::fs::write(module.join(name), text).expect("a module file");
    }
}

/// The directory of one case: below `CARGO_TARGET_TMPDIR`, named by the parent's process id and
/// the case. The child writes its module there; the parent removes it after the child ended,
/// whatever the child's status (a child that crashed or was killed cannot).
fn case_dir(parent: &str, case: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("drawer-layout-ui-{parent}-{case}"))
}

/// Runs one case in this process; returns its exit code.
fn run(case: &str) -> i32 {
    let dir = case_dir(
        &std::env::var("KOURIER_DRAWER_LAYOUT_PARENT").unwrap_or_default(),
        case,
    );
    write_module(&dir);
    // SAFETY: single-threaded test binary, before Qt starts.
    unsafe { std::env::set_var("QML_IMPORT_PATH", &dir) };
    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load_data(
            &QByteArray::from(SCRIPT.replace("CASE", case).as_str()),
            &QUrl::from(format!("file://{}/drawer_layout_test.qml", dir.display()).as_str()),
        );
    }
    let code = app.as_mut().map_or(3, |app| app.exec());
    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
    code
}

fn main() {
    support::answer_list("drawer_layout_ui");
    // SAFETY: single-threaded test binary, before Qt starts.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_DRAWER_LAYOUT_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_DRAWER_LAYOUT_ONLY").unwrap_or_default();
    let cases: Vec<&str> = ["wide-one", "wide-two", "wide-two-expanded", "narrow-one"]
        .into_iter()
        .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
        .collect();
    let parent = std::process::id().to_string();
    let mut failed = 0;
    for case in &cases {
        let mut child = Command::new(&exe)
            .env("KOURIER_DRAWER_LAYOUT_UI_CASE", case)
            .env("KOURIER_DRAWER_LAYOUT_PARENT", &parent)
            // Freed memory is filled with a pattern, so a walk over a freed entry crashes (or
            // spins, which the kill below catches) instead of silently reading a reused slot.
            .env("MALLOC_PERTURB_", "165")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("start a case");
        // The pipes are read on threads so a chatty child never blocks on a full pipe.
        let mut out = child.stdout.take().expect("stdout");
        let mut err = child.stderr.take().expect("stderr");
        let out_thread = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = out.read_to_string(&mut text);
            text
        });
        let err_thread = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            text
        });
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait for a case") {
                break Some(status);
            }
            if started.elapsed() > Duration::from_secs(90) {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        // Whatever the child's status, its directory goes (a crashed or killed child left it).
        let _ = std::fs::remove_dir_all(case_dir(&parent, case));
        let said = err_thread.join().unwrap_or_default();
        print!("{}", out_thread.join().unwrap_or_default());
        eprint!("{said}");
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        match status {
            Some(status) if status.success() && errors.is_empty() => {
                println!("test drawer layout: {case} ... ok");
            }
            Some(status) => {
                println!("test drawer layout: {case} ... FAILED ({status})");
                failed += 1;
            }
            None => {
                println!("test drawer layout: {case} ... FAILED (killed after 90 s)");
                failed += 1;
            }
        }
    }
    let passed = cases.len() - failed;
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed");
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed");
        std::process::exit(1);
    }
}
