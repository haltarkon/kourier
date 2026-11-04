//! The pages of blended chats in the real main window, over the real core on the fake network
//! (offscreen, `harness = false`; each case in a process of its own): `--page=accounts` and
//! `--page=accountsdrawer`, wide and narrow. The profile holds three linked accounts (the fixture
//! account, a second one of the test network and a registered fake that plays WhatsApp,
//! `FixtureOptions::accounts`); the page hides the second through `App.toggleAccount`, the call a
//! click on its checkbox makes (`MainWindow.qml` `accountsPageReady`), and is ready when the list
//! has followed.
//!
//! Every case asserts:
//! - at least three accounts on the panel, the second one not shown, `shownCount` 2 and its three
//!   unread messages counted in `hiddenUnread` (the narrow window's dot, the rail's pill);
//! - the list holds the chats of two accounts, none of the hidden one, and the title names the
//!   two shown accounts;
//! - NO banner: the WhatsApp fake pushes its history when it is linked, which must not leave a
//!   banner on the page: `bannerKind` is empty and no history transfer shows.
//!
//! `accountsdrawer` also asserts the side panel is shown (narrow: with only the first account shown, its box
//! greyed, UI-12): expanded when wide, open over the list
//! when narrow. `KOURIER_ACCOUNTS_SHOTS=<dir>` writes the window's picture (`<case>.png`) for a
//! look (not a test).

mod support;

use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

MainWindow {
    id: win

    width: win.narrow ? 480 : 1100
    height: 760
    visible: true

    property string mode: "CASE"
    readonly property bool narrow: win.mode.endsWith("narrow")
    readonly property bool drawerPage: win.mode.startsWith("accountsdrawer")
    property int waits: 0
    property bool busy: false

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "]: " + msg);
        Qt.exit(1);
    }
    function find(root: Item, pred: var): Item {
        if (root === null || root === undefined) return null;
        if (pred(root)) return root;
        for (const child of root.children) {
            const hit = win.find(child, pred);
            if (hit) return hit;
        }
        return null;
    }

    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {
            required property real accountId
            required property string title
        }
    }

    Timer {
        interval: 40
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
        win.waits += 1;
        if (!win.pageReady) {
            if (win.waits > 1500) win.fail("the page never got ready");
            return;
        }
        // A moment for what a wrong state would still change (the history batch of the
        // WhatsApp fake, the second account's unread count).
        if (win.waits < 40) return;
        const actions = App.accountActions;
        if (actions.length < 3) return win.fail("accounts on the panel: " + actions.length);
        // (The narrow open drawer leaves only the first account shown: its greyed box, UI-12.)
        const oneLeft = win.drawerPage && win.narrow;
        if (actions[1].shown) return win.fail("the second account is shown");
        if (!actions[0].shown || actions[2].shown === oneLeft) return win.fail("an account is shown or hidden against the page");
        if (App.shownCount !== (oneLeft ? 1 : 2)) return win.fail("shownCount " + App.shownCount);
        if (oneLeft ? App.hiddenUnread < 3 : App.hiddenUnread !== 3) return win.fail("hiddenUnread " + App.hiddenUnread);
        // The list: chats of the shown accounts, none of the hidden ones.
        const listed = new Set();
        for (let i = 0; i < convs.count; i++) listed.add(convs.objectAt(i).accountId);
        if (listed.size !== (oneLeft ? 1 : 2)) return win.fail("accounts in the list: " + listed.size);
        if (listed.has(actions[1].account)) return win.fail("a chat of the hidden account is listed");
        if (!listed.has(actions[0].account) || listed.has(actions[2].account) === oneLeft) return win.fail("the list does not follow the shown accounts");
        // The title names the shown accounts (two joined, or the one).
        if ((App.currentTitle.indexOf(" · ") < 0) !== oneLeft) return win.fail("the title: " + App.currentTitle);
        if (oneLeft) {
            const box = win.find(win.globalDrawer.accountEntry(0), i => i.objectName === "entryBox");
            if (box === null || !box.visible || box.enabled) return win.fail("the last shown account's box is not greyed");
        }
        // No banner: no state, no restriction, no history transfer.
        if (App.bannerKind !== "") return win.fail("a banner shows: " + App.bannerKind);
        if ((App.bannerTransfer.phase ?? "") !== "") return win.fail("a history transfer shows: " + App.bannerTransfer.phase);
        if (win.drawerPage) {
            const drawer = win.globalDrawer;
            if (win.narrow ? !drawer.drawerOpen : drawer.collapsed) return win.fail("the side panel is not shown");
        }
        if ("SHOTS_DIR" !== "") {
            App.saveScreenshot(win, "SHOTS_DIR/CASE.png");
        }
        Qt.exit(0);
    }
}
"##;

fn run(case: &str) -> i32 {
    let page = case.split('-').next().unwrap_or("accounts").to_owned();
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 30,
            accounts: 3,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext { app: app.handle() });
    kourier_qt::set_options(UiOptions {
        page: Some(page),
        ..UiOptions::default()
    });
    let mut qt = QGuiApplication::new();
    let script = SCRIPT.replace("CASE", case).replace(
        "SHOTS_DIR",
        &std::env::var("KOURIER_ACCOUNTS_SHOTS").unwrap_or_default(),
    );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/accounts_pages_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    code
}

fn main() {
    support::answer_list("accounts_pages_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_ACCOUNTS_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let cases = [
        "accounts-wide",
        "accounts-narrow",
        "accountsdrawer-wide",
        "accountsdrawer-narrow",
    ];
    let mut failed = 0;
    for case in cases {
        let out = Command::new(&exe)
            .env("KOURIER_ACCOUNTS_UI_CASE", case)
            .output()
            .expect("run a case");
        let said = String::from_utf8_lossy(&out.stderr);
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{said}");
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        if out.status.success() && errors.is_empty() {
            println!("test accounts pages: {case} ... ok");
        } else {
            println!("test accounts pages: {case} ... FAILED ({})", out.status);
            failed += 1;
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
