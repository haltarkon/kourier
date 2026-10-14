//! The independent review of the history transfer's screens (`target/h3-context/review.md`):
//! the question "Remove and Link Again?" in the style the application runs with
//! (`org.kde.desktop`, `QApplication`, the KDE platform theme), reached the way a user
//! reaches it: Settings, group "Accounts", a click on "Transfer History from Phone…".
//! `transfer_ui.rs` runs in Qt's default style and opens the question from the banner.
//!
//! Each case runs in a process of its own, with a configuration directory of its own.
//!
//! - `keys`: the keyboard starts on "Cancel"; Return, Enter and Space confirm nothing; the
//!   account is untouched after each.
//! - `double`, `double-instant`: a double click on the settings row. The question opens
//!   under the pointer; when the row happens to lie where the confirming button appears,
//!   the second click of the same double click lands on "Remove and Link Again" before
//!   the question can be read. It must do nothing: the button acts only once the
//!   question has been open for a moment (`ReplaceAccountDialog.armed`). Both cases failed
//!   before that delay existed. The window is 440 px high, where one account's row can be
//!   scrolled to that place. `double`: Plasma's default animation speed (the dialog is
//!   open 200 ms after the first click); `double-instant`: animations switched off.
//! - `late`: the same place, clicked again once the button acts: the account is replaced
//!   (the delay does not make the button dead).
//! - `wide-many`: the user's segfault of 2026-10-12 (`target/link-feedback/crash.md`). A
//!   WIDE window (the accounts drawer is a rail), about 200 chats, Qt Quick's RHI scene
//!   graph (null backend: no GPU) with the THREADED render loop, as on Plasma Wayland; the
//!   only account is replaced. When `App.hasAccounts` turned false, the drawer flipped from
//!   the rail to a modal drawer in the same moment its Repeater deleted the account's entry,
//!   and Kirigami's GlobalDrawer read the size of a layout that still held the freed entry
//!   (a use-after-free in Qt Quick Layouts). The drawer now follows `hasAccounts` a moment
//!   later (`AccountsDrawer.accountsShown`). The process must live on for three seconds
//!   after the account is gone; without the fix it dies by SIGSEGV. Run twice: before the
//!   fix a debug build crashed every time.

mod support;

use std::process::{Command, ExitCode, Stdio};

use cxx_qt_lib::{QByteArray, QQmlApplicationEngine, QUrl};
use cxx_qt_lib_extras::QApplication;
use kourier_app::{App, FixtureOptions, FixtureSecondLink, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

MainWindow {
    id: win

    width: WINDOW_WIDTH
    height: WINDOW_HEIGHT
    visible: true

    property string mode: "CASE"
    property string step: "start"
    property int waits: 0
    property int quiet: 0
    property var ev: null
    property real firstAccount: 0
    // The middle of the confirming button, in window coordinates.
    property point target: Qt.point(0, 0)
    // When the row was clicked, and how long the question took to take input.
    property double clickedAt: 0
    property double openedAfter: -1
    // Which key is tried.
    property int key: 0
    readonly property var keys: [[Qt.Key_Return, "Return"], [Qt.Key_Enter, "Enter"], [Qt.Key_Space, "Space"]]

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 500) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.quiet = 0;
    }

    function find(root: Item, pred: var): Item {
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

    // The settings row "Transfer History from Phone…".
    function row(): Item {
        return win.settingsPage ? win.find(win.settingsPage, i => typeof i.objectName === "string" && i.objectName.indexOf("transfer-history-") === 0) : null;
    }

    function asking(): bool {
        return win.replaceDialog !== null && win.replaceDialog.opened;
    }

    function linking(): bool {
        return win.linkPage !== null && win.pageStack.lastItem === win.linkPage;
    }

    // The account the question is about is still there and nothing was started.
    function untouched(): bool {
        return accounts.count === 1 && accounts.objectAt(0).accountId === win.firstAccount && !win.linking();
    }

    function click(item: Item, x: real, y: real): void {
        if (!win.ev.mouseClick(item, x, y, Qt.LeftButton, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
    }

    Instantiator {
        id: accounts
        model: App.accounts
        delegate: QtObject {
            required property real accountId
        }
    }

    // Every chat of the list as an object (as the crash probe had): more objects that go
    // with the account, which is the timing the crash needed.
    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {
            required property real conversationId
            required property string previewIcon
        }
    }

    // wide-many: the moment that crashed is the only account leaving (no account at all).
    property bool noAccountSeen: false
    Connections {
        target: App
        function onHasAccountsChanged(): void {
            if (!App.hasAccounts && win.step !== "start") {
                win.noAccountSeen = true;
            }
        }
    }

    Connections {
        target: win.replaceDialog
        function onOpened(): void {
            win.openedAfter = Date.now() - win.clickedAt;
        }
    }

    property bool busy: false
    Timer {
        interval: 20
        repeat: true
        running: true
        onTriggered: {
            if (win.busy) {
                return;
            }
            win.busy = true;
            try {
                win.tick();
            } finally {
                win.busy = false;
            }
        }
    }

    function tick(): void {
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts) return win.later("the fixture account");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "transfer_review_events.qml");
            if (win.mode === "wide-many") {
                win.next("chat");
                return;
            }
            win.openSettings();
            win.next("settings");
            return;
        // wide-many: a chat with pictures open and scrolled, as the user had.
        case "chat": {
            if (convs.count < 150) return win.later("about 200 chats");
            let id = convs.objectAt(0).conversationId;
            for (let i = 0; i < convs.count; ++i) {
                if (convs.objectAt(i).previewIcon !== "") {
                    id = convs.objectAt(i).conversationId;
                    break;
                }
            }
            win.openConversation(id);
            win.next("chat-rows");
            return;
        }
        case "chat-rows": {
            if (!win.chatPage || win.chatPage.timeline.rowList.count < 5) return win.later("the chat's rows");
            win.quiet += 1;
            const list = win.chatPage.timeline.rowList;
            if (win.quiet % 10 === 0 && win.quiet < 60) {
                list.contentY = list.contentY - list.height * 3;
            }
            if (win.quiet < 70) return;
            win.openSettings();
            win.next("settings");
            return;
        }
        case "settings": {
            const row = win.row();
            if (!row || App.historyAccounts.length !== 1) return win.later("the row of the account");
            win.firstAccount = App.historyAccounts[0].account;
            win.settingsPage.showAccounts();
            win.next("open");
            return;
        }
        // The question, by a click on the row.
        case "open": {
            const row = win.row();
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (!row || !row.visible) return win.later("the row");
            win.clickedAt = Date.now();
            win.click(row, row.width / 2, row.height / 2);
            win.next(win.mode === "keys" ? "key" : win.mode === "wide-many" ? "confirm" : "measure");
            return;
        }

        // ---- wide-many ----
        case "confirm": {
            if (!win.asking()) return win.later("the question");
            if (!win.replaceDialog.armed) return win.later("the confirming button acts");
            if (!win.globalDrawer.rail) return win.fail("the drawer is not a rail in a wide window: the case tests nothing");
            const b = win.replaceDialog.confirmButton;
            console.warn("review: " + convs.count + " chats, a chat open; confirming the replacement of the only account");
            win.click(b, b.width / 2, b.height / 2);
            win.next("gone");
            return;
        }
        case "gone":
            for (let i = 0; i < accounts.count; ++i) {
                if (accounts.objectAt(i).accountId === win.firstAccount) return win.later("the account is removed");
            }
            win.next("alive");
            return;
        // The crash came within a second of the removal: live on for three.
        case "alive":
            win.quiet += 1;
            if (win.quiet < 150) return;
            if (!win.noAccountSeen) return win.fail("the window never had no account: the case tests nothing");
            // The late flip (`AccountsDrawer.accountsShown`): the replacement waits at its QR
            // code, so no account is shown, and 300 ms after the last one went the drawer
            // is a modal one and disabled; the replacement's link page is the last page. A
            // timer that never flips fails here, as the old flip in the same moment crashed.
            if (App.hasAccounts) return win.fail("an account is shown while the replacement waits at its code");
            if (!win.globalDrawer.modal) return win.fail("the drawer is still a rail with no account: it never followed hasAccounts");
            if (win.globalDrawer.enabled) return win.fail("the drawer is still enabled with no account");
            if (!win.linking()) return win.fail("the replacement's link page is not the last page");
            console.warn("review: the window lived on for three seconds after the only account was removed");
            Qt.exit(0);
            return;

        // ---- keys ----
        case "key": {
            if (!win.asking()) return win.later("the question");
            const dialog = win.replaceDialog;
            if (!dialog.cancelButton || !dialog.confirmButton) return win.fail("the question's buttons");
            if (!dialog.cancelButton.activeFocus) return win.later("the keyboard on Cancel (in the app's style)");
            if (!win.ev.keyClick(win.keys[win.key][0], Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("keyed");
            return;
        }
        case "keyed": {
            win.quiet += 1;
            if (win.quiet < 25) return;
            const name = win.keys[win.key][1];
            if (!win.untouched()) return win.fail(name + " on the open question confirmed it");
            if (win.replaceDialog !== null && win.replaceDialog.answered) return win.fail(name + " answered the question");
            console.warn("review: " + name + " with the keyboard on Cancel " + (win.replaceDialog === null ? "closed the question (cancelled)" : "did nothing"));
            if (win.replaceDialog !== null) {
                win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            }
            win.next("closed");
            return;
        }
        case "closed":
            if (win.replaceDialog !== null) return win.later("the question closes");
            if (!win.untouched()) return win.fail("closing the question changed something");
            win.key += 1;
            if (win.key >= win.keys.length) {
                Qt.exit(0);
                return;
            }
            win.next("open");
            return;

        // ---- double ----
        // Where the confirming button appears, and how soon it takes input.
        case "measure": {
            if (!win.asking()) return win.later("the question");
            const b = win.replaceDialog.confirmButton;
            if (!b) return win.fail("the confirming button");
            const p = b.mapToItem(null, b.width / 2, b.height / 2);
            win.target = Qt.point(Math.round(p.x), Math.round(p.y));
            console.warn("review: the question took input " + win.openedAfter + " ms after the click; the confirming button is at " + win.target.x + "," + win.target.y + "; double click interval " + Qt.styleHints.mouseDoubleClickInterval + " ms");
            win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            win.next("place");
            return;
        }
        // The user's scroll position puts the row where that button will be.
        case "place": {
            if (win.replaceDialog !== null) return win.later("the question closes");
            if (!win.untouched()) return win.fail("measuring changed something");
            const row = win.row();
            const flick = win.settingsPage.flickable;
            const at = row.mapToItem(null, row.width / 2, row.height / 2);
            const y = flick.contentY + (at.y - win.target.y);
            if (y < 0 || y > flick.contentHeight - flick.height) return win.fail("geometry: the row cannot be scrolled under the button in this window (contentY " + y + ")");
            flick.contentY = y;
            win.next("first");
            return;
        }
        case "first": {
            win.quiet += 1;
            if (win.quiet < 10) return;
            const row = win.row();
            const q = row.mapFromItem(null, win.target.x, win.target.y);
            if (!row.contains(q)) return win.fail("geometry: the row is not under the button's place");
            win.openedAfter = -1;
            win.clickedAt = Date.now();
            win.click(row, q.x, q.y);
            win.next("second");
            return;
        }
        // The second click of the double click, at the same place.
        case "second": {
            if (win.replaceDialog === null) return win.later("the question");
            const gap = Date.now() - win.clickedAt;
            // (A person's second click comes 100 to 400 ms after the first.)
            if (gap < DOUBLE_CLICK_GAP) return;
            const row = win.row();
            const q = row.mapFromItem(null, win.target.x, win.target.y);
            win.waits = gap;
            win.click(row, q.x, q.y);
            win.step = "result";
            win.quiet = 0;
            return;
        }
        case "result":
            win.quiet += 1;
            if (win.quiet < 60) return;
            if (!win.untouched()) return win.fail("a double click on the settings row removed the account: the second click, " + win.waits + " ms after the first and at the same place, pressed \"Remove and Link Again\" (the question took input after " + win.openedAfter + " ms)");
            if (win.mode !== "late") {
                Qt.exit(0);
                return;
            }
            win.next("late");
            return;
        // The question is still open and has been for a while: the same click now confirms.
        case "late": {
            if (!win.asking()) return win.fail("the double click closed the question");
            if (!win.replaceDialog.armed) return win.later("the confirming button acts");
            const b = win.replaceDialog.confirmButton;
            win.click(b, b.width / 2, b.height / 2);
            win.next("replaced");
            return;
        }
        case "replaced":
            if (!win.linking()) return win.later("the link page of the new link");
            for (let i = 0; i < accounts.count; ++i) {
                if (accounts.objectAt(i).accountId === win.firstAccount) return win.later("the account is removed");
            }
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 90000
        running: true
        onTriggered: win.fail("timeout")
    }
}
"##;

/// The environment variable that names the case of a child process.
const RUN: &str = "KOURIER_TRANSFER_REVIEW_CASE";

/// One case.
struct Case {
    name: &'static str,
    width: u32,
    height: u32,
    /// KDE's animation factor.
    animation: f32,
    /// The gap of the double click in milliseconds.
    gap: u32,
    /// The case shows a defect (ignored unless asked for; none does today).
    shows_defect: bool,
    /// Qt Quick's RHI scene graph (null backend) with the threaded render loop, the way
    /// a desktop renders, instead of the software scene graph and the basic loop that
    /// `offscreen` picks by itself.
    threaded: bool,
    /// Generated contacts (each a chat) and groups of the fixture.
    chats: (usize, usize),
    /// Messages per chat.
    messages: usize,
    /// How many times the case runs.
    runs: u32,
}

const fn case(name: &'static str, height: u32, animation: f32, gap: u32) -> Case {
    Case {
        name,
        width: 520,
        height,
        animation,
        gap,
        shows_defect: false,
        threaded: false,
        chats: (12, 4),
        messages: 6,
        runs: 1,
    }
}

/// Messages per chat of `wide-many` (`KOURIER_TRANSFER_REVIEW_MESSAGES` while working).
const MANY_MESSAGES: usize = 120;

/// The cases.
const CASES: [Case; 5] = [
    case("keys", 760, 1.0, 0),
    case("double", 440, 1.0, 250),
    case("double-instant", 440, 0.0, 120),
    case("late", 440, 1.0, 250),
    Case {
        width: 1400,
        threaded: true,
        chats: (192, 8),
        messages: MANY_MESSAGES,
        runs: 2,
        ..case("wide-many", 900, 1.0, 0)
    },
];

/// Runs one case in this process.
fn run(case: &str) -> ExitCode {
    let Some(c) = CASES.iter().find(|c| c.name == case) else {
        eprintln!("FAIL: no case named {case}");
        return ExitCode::FAILURE;
    };
    // (To see at which window heights the row can lie under the button.)
    let height = std::env::var("KOURIER_TRANSFER_REVIEW_HEIGHT")
        .ok()
        .and_then(|h| h.parse::<u32>().ok())
        .unwrap_or(c.height);
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: std::env::var("KOURIER_TRANSFER_REVIEW_MESSAGES")
                .ok()
                .and_then(|m| m.parse().ok())
                .unwrap_or(c.messages),
            contacts: c.chats.0,
            groups: c.chats.1,
            // The fixture account offers "Transfer History from Phone…".
            history_on_link: true,
            // The replacement of the only account waits at its QR code, as with the user's
            // phone in his hand: no account is shown meanwhile (`wide-many` looks at the
            // drawer then).
            second_link: (case == "wide-many").then_some(FixtureSecondLink::AtQr),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    // QApplication, as the app: the org.kde.desktop style draws with a widget style.
    let mut qt = QApplication::new();
    let script = SCRIPT
        .replace("CASE", case)
        .replace("WINDOW_WIDTH", &c.width.to_string())
        .replace("WINDOW_HEIGHT", &height.to_string())
        .replace("DOUBLE_CLICK_GAP", &c.gap.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/transfer_review_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn main() -> ExitCode {
    support::answer_list("transfer_review_ui");
    if let Ok(case) = std::env::var(RUN) {
        return run(&case);
    }
    let all = std::env::args().any(|a| a == "--include-ignored" || a == "--ignored");
    let exe = std::env::current_exe().expect("the test binary");
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("transfer-review-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (mut passed, mut failed, mut ignored) = (0, 0, 0);
    // `KOURIER_TRANSFER_REVIEW_ONLY=a,b` runs only those cases (while working on one).
    let only = std::env::var("KOURIER_TRANSFER_REVIEW_ONLY").ok();
    for c in &CASES {
        let case = c.name;
        if only
            .as_deref()
            .is_some_and(|o| !o.split(',').any(|n| n == case))
        {
            continue;
        }
        if c.shows_defect && !all {
            println!(
                "test the question in the app's style: {case} ... ignored, review: a double click on the settings row can press \"Remove and Link Again\""
            );
            ignored += 1;
            continue;
        }
        for run in 0..c.runs {
            // A private configuration, data, cache and state directory: nothing of the
            // session's own is read or written.
            let home = base.join(format!("{case}-{run}"));
            for dir in ["config", "data", "cache", "state"] {
                std::fs::create_dir_all(home.join(dir)).expect("a private directory");
            }
            std::fs::write(
                home.join("config/kdeglobals"),
                format!("[KDE]\nAnimationDurationFactor={}\n", c.animation),
            )
            .expect("kdeglobals");
            let mut cmd = Command::new(&exe);
            cmd.env(RUN, case)
                .env("XDG_CONFIG_HOME", home.join("config"))
                .env("XDG_DATA_HOME", home.join("data"))
                .env("XDG_CACHE_HOME", home.join("cache"))
                .env("XDG_STATE_HOME", home.join("state"))
                // Never on the user's desktop, and in the style of the desktop.
                .env("QT_QPA_PLATFORM", "offscreen")
                .env("QT_QPA_PLATFORMTHEME", "kde")
                .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
                .env("QT_SCALE_FACTOR", "1")
                .env("QT_FORCE_STDERR_LOGGING", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for var in ["QT_QUICK_BACKEND", "QSG_RHI_BACKEND", "QSG_RENDER_LOOP"] {
                cmd.env_remove(var);
            }
            if c.threaded {
                cmd.env("QT_QUICK_BACKEND", "rhi")
                    .env("QSG_RHI_BACKEND", "null")
                    .env("QSG_RENDER_LOOP", "threaded");
            }
            let out = cmd.output().expect("run a case");
            let label = if c.runs > 1 {
                format!("{case} (run {})", run + 1)
            } else {
                case.to_owned()
            };
            let said = String::from_utf8_lossy(&out.stderr);
            for line in said
                .lines()
                .filter(|l| l.contains("review:") || l.contains("FAIL"))
            {
                println!("    {line}");
            }
            if out.status.success() {
                println!("test the question in the app's style: {label} ... ok");
                passed += 1;
            } else {
                println!(
                    "test the question in the app's style: {label} ... FAILED ({})",
                    out.status
                );
                if !said.contains("FAIL") {
                    let lines: Vec<&str> = said.lines().collect();
                    for line in &lines[lines.len().saturating_sub(15)..] {
                        println!("    {line}");
                    }
                }
                failed += 1;
            }
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed; {ignored} ignored");
        ExitCode::SUCCESS
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed; {ignored} ignored");
        ExitCode::FAILURE
    }
}
