//! "Remove Account…" and "Unlink…" of an account (`target/link-feedback/plan.md` L4,
//! `buttons.md` §2.3), in the real main window on the real core over the fake network
//! (offscreen, a wide window whose drawer is a rail, `harness = false`; each case in a
//! process of its own).
//!
//! - `remove`: two accounts (the fixture account with its chats and another one linked
//!   after it). "Remove Account…" of the fixture account's group in Settings → Accounts,
//!   by a real click, asks "Remove Account?" with the keyboard on "Cancel"; a second ask
//!   while it is open makes no second question; in the first moment "Remove Account" does
//!   nothing and Enter confirms nothing; "Cancel" removes nothing. Asked again and
//!   confirmed once it acts: only that account goes (its chats with it), the other stays
//!   with its own group, and the user is told once, in passing (the device was removed).
//! - `last`: the only account, removed from the drawer's context menu (a right click on its
//!   entry): the same question; once it is gone the welcome page is the last page.
//! - `unreachable`: the logout does not reach the network: the removal still happens, and a
//!   notice that stays says to remove the device ("Kourier fixture") on the phone; "Close"
//!   closes it, and nothing more is said.
//! - `unlink`: "Unlink…" asks "Unlink This Computer?"; confirmed, the account stays with its
//!   chats, logged out (the banner offers "Link Again"), it no longer offers "Unlink…", and
//!   the user is told once.
//! - `nohover`: as `last`, but the pointer never came over the entry before its right click:
//!   the menu is made then and still opens, with its size.
//! - `nohover-kde` (L5): `nohover` in the app's style (`QApplication`, `org.kde.desktop`, the
//!   KDE platform theme, private XDG directories). Kills the L4 build's menu that was made
//!   but never opened. The immediate popup of a menu made in that moment (instead of
//!   `Qt.callLater`) opens with its size here too: the failure seen in the L4 build is not
//!   reproduced by it, offscreen.
//! - `hidden`: the account's history is being imported, so "Transfer History from Phone…" is
//!   hidden from its menu. The keyboard (Menu key, Down, Return) lands on the first item that
//!   is shown, never on the hidden one: no "Remove and Link Again?" is asked.
//!
//! - `trayed` (L5): as `unreachable`, but the window is hidden (in the tray) right after the
//!   confirmation: the notice is told while hidden and shown only when the window is back.
//! - `notices` (L5): two notices that stay, told at once: the second waits for the first to
//!   close (one dialog at a time, `logoutDialog` always names the open one), then opens.
//!
//! No case may print a script error (`TypeError`) or a failed assignment ("Unable to
//! assign"), Kirigami's included (L5: the return to the list after the second link of
//! `remove` printed three errors and a warning).
//!
//! `KOURIER_ACCOUNT_ACTIONS_ONLY=a,b` picks cases; `KOURIER_ACCOUNT_ACTIONS_SHOTS=<dir>`
//! writes pictures of the questions, the menu and the notice for a look.

mod support;

use std::process::Command;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use cxx_qt_lib_extras::QApplication;
use kourier_app::facade::{Query, QueryData};
use kourier_app::{App, FixtureOptions, FixtureSecondLink, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

MainWindow {
    id: win

    width: 1400
    height: 800
    visible: true

    property string mode: "CASE"
    readonly property string action: win.mode === "unlink" ? "unlink" : "remove"
    property string step: "start"
    property int waits: 0
    property int quiet: 0
    property var ev: null
    // The account the case acts on, the other one (`remove`), and the chats before.
    property real target: 0
    property real other: 0
    property int chatsBefore: 0
    // How the question was asked the first time (`remove`: asked again after Cancel).
    property int asks: 0
    // `last`: the drawer's entry of the account.
    property Item entry: null

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            fail("never happened: " + what + " (accounts " + App.accountActions.length + ")");
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

    function named(root: Item, name: string): Item {
        return win.find(root, i => i.objectName === name);
    }

    function click(item: Item, button: int): void {
        if (!win.ev.mouseClick(item, item.width / 2, item.height / 2, button, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
    }

    function asking(): bool {
        return win.replaceDialog !== null && win.replaceDialog.opened;
    }

    function actionsOf(account: real): var {
        return App.accountActions.find(a => a.account === account);
    }

    function shot(name: string): void {
        if ("SHOTS_DIR" !== "") {
            App.saveScreenshot(win, "SHOTS_DIR/" + name + ".png");
        }
    }

    // Asks the question the way the case does: the settings row, or the drawer's menu.
    function ask(): void {
        win.asks += 1;
        if (win.mode === "last" || win.mode === "nohover" || win.mode === "hidden") {
            const entry = win.find(win.globalDrawer.contentItem, i => i.menuAccount === win.target && i.visible);
            if (entry === null) return win.fail("no drawer entry of the account");
            win.entry = entry;
            if (win.mode === "hidden") {
                // The keyboard: the entry has the focus, the Menu key opens its menu.
                entry.forceActiveFocus();
                win.next("focused");
                return;
            }
            if (win.mode === "nohover") {
                // The pointer is on the entry (a window or a list that came up under it) but
                // the entry was never told: no hover, no focus, so no menu exists yet (the
                // start may have made one; it is taken away).
                entry.hoverEnabled = false;
                const old = win.globalDrawer.accountMenu;
                win.globalDrawer.accountMenu = null;
                win.globalDrawer.menuWanted = false;
                if (old !== null) old.destroy();
            }
            // The pointer comes over the entry before its right click, as a user's does.
            if (!win.ev.mouseMove(entry, entry.width / 2, entry.height / 2, -1, Qt.NoButton, Qt.NoModifier)) return win.fail("no pointer event");
            if (win.mode === "nohover") {
                if (entry.hovered) return win.fail("the entry is hovered");
                if (win.globalDrawer.accountMenu !== null) return win.fail("the menu was made before anything asked for it (entry focus " + entry.activeFocus + ", hovered " + entry.hovered + ", hoverEnabled " + entry.hoverEnabled + ")");
            }
            win.next("hovered");
            return;
        }
        const row = win.named(win.settingsPage, win.action === "unlink" ? "unlink-" + win.target : "remove-account-" + win.target);
        if (row === null || !row.visible) return win.fail("no visible row for " + win.action);
        win.click(row, Qt.LeftButton);
        win.next("asked");
    }

    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {}
    }

    property bool busy: false
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
        switch (win.step) {
        case "start": {
            if (!win.started) return win.later("the start page");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "account_actions_ui_events.qml");
            if (App.accountActions.length !== 1) return win.later("the fixture account");
            if (convs.count === 0) return win.later("the chat list");
            if (win.mode === "remove") {
                // A second account, linked at once (another identity, no chats).
                win.openWelcome();
                win.openLink("Fake");
                win.next("second");
                return;
            }
            win.next("first");
            return;
        }
        case "second":
            if (App.accountActions.length !== 2) return win.later("the second account");
            if (win.linkPage !== null && win.pageStack.lastItem === win.linkPage) return win.later("the link page closes");
            win.next("first");
            return;
        case "first": {
            const wanted = win.mode === "remove" ? 2 : 1;
            win.target = App.accountActions[0].account;
            if (wanted === 2) win.other = App.accountActions[1].account;
            if (win.mode === "hidden") {
                // The import's state arrives once the window is up: the transfer is hidden.
                if (App.accountActions[0].transfer) return win.later("the transfer shows");
            } else if (!App.accountActions[0].unlink) {
                return win.fail("a linked account does not offer Unlink");
            }
            win.chatsBefore = convs.count;
            if (win.mode === "notices") {
                win.takeLogoutNotice("One.", "Remove “one” on your phone.", "Remove This Computer on Your Phone");
                win.takeLogoutNotice("Two.", "Remove “two” on your phone.", "Remove This Computer on Your Phone");
                win.next("notice1");
                return;
            }
            const inDrawer = win.mode === "last" || win.mode === "nohover" || win.mode === "hidden";
            if (!inDrawer) {
                win.openSettings();
            }
            win.next(inDrawer ? "drawer" : "settings");
            return;
        }
        case "settings":
            if (win.settingsPage === null) return win.later("the settings page");
            if (win.mode === "remove" && win.named(win.settingsPage, "account-" + win.other) === null) return win.later("the group of the second account");
            win.settingsPage.showAccounts();
            win.quiet += 1;
            if (win.quiet < 10) return;
            win.ask();
            return;
        case "drawer":
            if (!win.globalDrawer.rail) return win.fail("the drawer is no rail in a wide window");
            win.quiet += 1;
            if (win.quiet < 10) return;
            win.ask();
            return;
        case "focused":
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (!win.entry.activeFocus) return win.fail("the entry has no focus");
            win.ev.keyClick(Qt.Key_Menu, Qt.NoModifier, -1);
            win.next("keymenu");
            return;
        case "keymenu": {
            const menu = win.globalDrawer.accountMenu;
            if (menu === null || !menu.opened) return win.later("the menu opens on the Menu key");
            win.quiet += 1;
            if (win.quiet < 8) return;
            // Down goes to the first item that is shown; Return acts on it.
            win.ev.keyClick(Qt.Key_Down, Qt.NoModifier, -1);
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            win.next("asked");
            return;
        }
        case "hovered":
            win.quiet += 1;
            if (win.quiet < 10) return;
            win.click(win.entry, Qt.RightButton);
            win.next("menu");
            return;
        case "menu": {
            const menu = win.globalDrawer.accountMenu;
            if (menu === null || !menu.opened) return win.later("the account's menu (made " + (menu !== null) + ", wanted " + win.globalDrawer.menuWanted + ", visible " + (menu !== null && menu.visible) + ")");
            const item = win.find(menu.contentItem, i => i.objectName === "drawer-remove-account");
            if (item === null || !item.visible || item.width === 0) return win.later("the menu's Remove Account…");
            // The menu has its size (a menu popped up in the moment it was made can stay the
            // size of its padding in the app's style, `nohover-kde`).
            if (menu.height < item.height || menu.width < item.width) return win.later("the menu's size (" + menu.width + "x" + menu.height + ", item " + item.width + "x" + item.height + ")");
            win.quiet += 1;
            if (win.quiet < 8) return;
            win.shot("drawer-menu");
            win.click(item, Qt.LeftButton);
            win.next("asked");
            return;
        }
        case "asked": {
            if (!win.asking()) return win.later("the question");
            const d = win.replaceDialog;
            if (win.mode === "hidden") {
                // The hidden "Transfer History from Phone…" was skipped: the question is the
                // one of the first item that is shown, never "Remove and Link Again?".
                const wanted = win.actionsOf(win.target).unlink ? "Unlink This Computer?" : "Remove Account?";
                if (d.title !== wanted) return win.fail("the keyboard reached '" + d.title + "', not '" + wanted + "'");
                win.quiet += 1;
                win.shot("hidden-item");
                Qt.exit(0);
                return;
            }
            const title = win.action === "unlink" ? "Unlink This Computer?" : "Remove Account?";
            const confirm = win.action === "unlink" ? "Unlink" : "Remove Account";
            if (d.title !== title) return win.fail("the title: " + d.title);
            if (d.confirmButton.text !== confirm || d.cancelButton.text !== "Cancel") return win.fail("the buttons: " + d.confirmButton.text + " / " + d.cancelButton.text);
            if (!d.cancelButton.activeFocus) return win.fail("the keyboard is not on Cancel");
            if (win.asks === 1) {
                // A second ask while it is open makes no second question.
                App.askAccountAction(win.target, win.action);
                if (win.replaceDialog !== d) return win.fail("a second question");
                // In the first moment the confirming button does nothing.
                d.confirmButton.click();
                if (!d.opened) return win.fail("an early press of " + confirm + " acted");
                win.next("arming");
                return;
            }
            win.next("confirm");
            return;
        }
        case "arming": {
            const d = win.replaceDialog;
            if (d === null || !d.armed) return win.later("the question takes input");
            win.shot("question-" + win.mode);
            // Enter on the default answer confirms nothing; Cancel removes nothing.
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            win.next("cancelled");
            return;
        }
        case "cancelled":
            if (win.replaceDialog !== null) {
                win.replaceDialog.cancelButton.click();
                return win.later("the question closes");
            }
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.actionsOf(win.target) === undefined || !win.actionsOf(win.target).unlink) return win.fail("Cancel acted");
            if (win.logoutNotices !== 0) return win.fail("a notice after Cancel");
            if (win.mode === "last" || win.mode === "nohover") {
                win.next("drawer");
            } else {
                win.next("settings");
            }
            return;
        case "confirm": {
            const d = win.replaceDialog;
            if (d === null || !d.armed) return win.later("the question takes input");
            win.click(d.confirmButton, Qt.LeftButton);
            if (win.mode === "trayed") {
                // To the tray, before the core has told anything.
                win.hide();
                win.next("intray");
                return;
            }
            win.next("done");
            return;
        }
        case "intray":
            if (win.actionsOf(win.target) !== undefined) return win.later("the account goes");
            if (win.logoutNotices === 0) return win.later("the notice is told");
            if (win.logoutDialog !== null) return win.fail("a notice shown while the window is hidden");
            win.quiet += 1;
            if (win.quiet < 15) return;
            if (win.logoutDialog !== null) return win.fail("a notice shown while the window is hidden");
            win.show();
            win.next("done");
            return;
        case "notice1": {
            const n = win.logoutDialog;
            if (n === null || !n.opened) return win.later("the first notice");
            if (n.onPhone.indexOf("“one”") < 0) return win.fail("the first open notice is " + n.onPhone);
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (win.logoutDialog !== n) return win.fail("the second notice took the first one's place");
            win.ev.mouseClick(n.closeButton, n.closeButton.width / 2, n.closeButton.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
            win.next("notice2");
            return;
        }
        case "notice2": {
            const n = win.logoutDialog;
            if (n === null || !n.opened) return win.later("the second notice opens after the first closed");
            if (n.onPhone.indexOf("“two”") < 0) return win.fail("the second open notice is " + n.onPhone);
            win.quiet += 1;
            if (win.quiet < 10) return;
            win.ev.mouseClick(n.closeButton, n.closeButton.width / 2, n.closeButton.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
            win.next("notices-closed");
            return;
        }
        case "notices-closed":
            if (win.logoutDialog !== null) return win.later("the second notice closes");
            win.quiet += 1;
            if (win.quiet < 15) return;
            if (win.logoutDialog !== null || win.pendingLogoutNotices.length !== 0) return win.fail("a third notice");
            if (win.logoutNotices !== 2) return win.fail(win.logoutNotices + " notices, not two");
            Qt.exit(0);
            return;
        case "done": {
            if (win.asking()) return win.later("the question closes");
            if (win.logoutNotices === 0) return win.later("the notice");
            if (win.mode === "unlink") {
                const a = win.actionsOf(win.target);
                if (a === undefined) return win.fail("the unlinked account went");
                if (a.unlink) return win.later("Unlink… goes with the link");
                if (App.bannerKind !== "loggedOut") return win.later("the banner of the logged-out account");
                if (convs.count !== win.chatsBefore) return win.fail("the chats: " + convs.count + " of " + win.chatsBefore);
            } else {
                if (win.actionsOf(win.target) !== undefined) return win.later("the account goes");
            }
            if (win.mode === "unreachable" || win.mode === "trayed") {
                if (win.logoutDialog === null || !win.logoutDialog.opened) return win.later("the notice that stays");
                const n = win.logoutDialog;
                if (n.onPhone.indexOf("“Kourier fixture”") < 0) return win.fail("the instruction: " + n.onPhone);
                if (n.title !== "Remove This Computer on Your Phone") return win.fail("the notice's title: " + n.title);
                if (!n.closeButton.activeFocus) return win.fail("the keyboard is not on Close");
                win.quiet += 1;
                if (win.quiet < 15) return;
                win.shot("notice-unreachable");
                win.click(n.closeButton, Qt.LeftButton);
                win.next("closed");
                return;
            }
            if (win.logoutDialog !== null) return win.fail("a notice that stays after a confirmed logout");
            win.next("after");
            return;
        }
        case "closed":
            if (win.logoutDialog !== null) return win.later("the notice closes");
            win.next("after");
            return;
        case "after":
            win.quiet += 1;
            if (win.quiet < 40) return;
            if (win.logoutNotices !== 1) return win.fail(win.logoutNotices + " notices, not one");
            if (win.mode === "remove") {
                if (App.accountActions.length !== 1 || App.accountActions[0].account !== win.other) return win.fail("the other account did not stay");
                if (win.named(win.settingsPage, "account-" + win.other) === null) return win.fail("the other account's group went");
                if (win.named(win.settingsPage, "account-" + win.target) !== null) return win.fail("the removed account's group stayed");
                if (convs.count !== 0) return win.fail("the removed account's chats stayed: " + convs.count);
            }
            if (win.mode === "last" || win.mode === "nohover" || win.mode === "unreachable" || win.mode === "trayed") {
                if (App.hasAccounts) return win.fail("an account is left");
                if (win.pageStack.lastItem !== win.welcomePage) return win.fail("the welcome page is not the last page");
            }
            Qt.exit(0);
            return;
        }
    }
}
"##;

fn options(case: &str) -> FixtureOptions {
    let base = FixtureOptions {
        messages_per_conversation: 6,
        transfer_stage: Duration::from_millis(600),
        ..FixtureOptions::default()
    };
    match case {
        "remove" => FixtureOptions {
            second_link: Some(FixtureSecondLink::OtherAccount),
            ..base
        },
        "unreachable" | "trayed" => FixtureOptions {
            logout_unreachable: true,
            ..base
        },
        "hidden" => FixtureOptions {
            // The account's history is being imported (a download that stays).
            transfer: Some(kourier_app::FixtureTransfer::Left(
                kourier_model::HistoryTransfer::Downloading {
                    done: 1_000_000,
                    total: Some(10_000_000),
                },
            )),
            history_on_link: true,
            ..base
        },
        "last" | "nohover" | "unlink" | "notices" => base,
        other => panic!("unknown case {other}"),
    }
}

/// The accounts the core has.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn account_rows(app: &kourier_app::AppHandle) -> Vec<kourier_app::facade::AccountRow> {
    match app.runtime().block_on(app.query(Query::Accounts)) {
        Ok(QueryData::Accounts(accounts)) => accounts,
        _ => panic!("the accounts"),
    }
}

/// Runs one case in this process; returns its exit code.
fn run(case: &str) -> i32 {
    // `nohover-kde`: `nohover` in the app's style (QApplication, org.kde.desktop).
    let kde = case == "nohover-kde";
    let case = if kde { "nohover" } else { case };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(options(case)),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut gui = (!kde).then(QGuiApplication::new);
    let mut widgets = kde.then(QApplication::new);
    let script = SCRIPT.replace("CASE", case).replace(
        "SHOTS_DIR",
        &std::env::var("KOURIER_ACCOUNT_ACTIONS_SHOTS").unwrap_or_default(),
    );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/account_actions_ui_test.qml"),
        );
    }
    let code = if kde {
        widgets
            .as_mut()
            .and_then(|qt| qt.as_mut())
            .map_or(3, |qt| qt.exec())
    } else {
        gui.as_mut()
            .and_then(|qt| qt.as_mut())
            .map_or(3, |qt| qt.exec())
    };
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        app.shutdown();
        return code;
    }

    let commands = handle.fixture_commands();
    let count = |name: &str| commands.iter().filter(|c| **c == name).count();
    let mut problems = Vec::new();
    let rows = account_rows(&handle);
    let wanted = match case {
        "remove" | "unlink" | "hidden" | "notices" => 1,
        _ => 0,
    };
    if rows.len() != wanted {
        problems.push(format!("{} accounts, expected {wanted}", rows.len()));
    }
    // The server is told once, for the one account (the other is not logged out); in `hidden`
    // nothing was confirmed.
    let logouts = usize::from(case != "hidden" && case != "notices");
    if count("Logout") != logouts {
        problems.push(format!("{logouts} logout(s): {commands:?}"));
    }
    if count("CancelLogin") != 0 {
        problems.push(format!("a login was cancelled: {commands:?}"));
    }
    if case == "remove"
        && !rows
            .iter()
            .all(|a| matches!(a.state, kourier_model::AccountState::Active))
    {
        problems.push("the other account is not active".to_owned());
    }
    if case == "unlink"
        && !rows
            .iter()
            .all(|a| matches!(a.conn, kourier_model::ConnState::LoggedOut { .. }))
    {
        problems.push("the unlinked account is not logged out".to_owned());
    }
    app.shutdown();
    for problem in &problems {
        eprintln!("FAIL [{case}]: {problem}");
    }
    i32::from(!problems.is_empty())
}

fn main() {
    support::answer_list("account_actions_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_ACCOUNT_ACTIONS_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_ACCOUNT_ACTIONS_ONLY").unwrap_or_default();
    let cases: Vec<&str> = [
        "remove",
        "last",
        "nohover",
        "nohover-kde",
        "hidden",
        "unreachable",
        "unlink",
        "trayed",
        "notices",
    ]
    .into_iter()
    .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
    .collect();
    let mut failed = 0;
    let base = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("account-actions-ui-{}", std::process::id()));
    for case in &cases {
        let mut cmd = Command::new(&exe);
        cmd.env("KOURIER_ACCOUNT_ACTIONS_UI_CASE", case);
        if *case == "nohover-kde" {
            // The style of the desktop, with a private configuration (Plasma's animation
            // speed) and nothing of the session's own read or written.
            let home = base.join(case);
            for dir in ["config", "data", "cache", "state"] {
                std::fs::create_dir_all(home.join(dir)).expect("a private directory");
            }
            std::fs::write(
                home.join("config/kdeglobals"),
                "[KDE]\nAnimationDurationFactor=1\n",
            )
            .expect("kdeglobals");
            cmd.env("XDG_CONFIG_HOME", home.join("config"))
                .env("XDG_DATA_HOME", home.join("data"))
                .env("XDG_CACHE_HOME", home.join("cache"))
                .env("XDG_STATE_HOME", home.join("state"))
                .env("QT_QPA_PLATFORMTHEME", "kde")
                .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
                .env("QT_SCALE_FACTOR", "1");
        }
        let out = cmd.output().expect("run a case");
        let said = String::from_utf8_lossy(&out.stderr);
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{said}");
        let status = out.status;
        // No script error of Kirigami's or ours on the way (L5: the second link's return to
        // the list cleared a stack that started with the list: TypeErrors in Kirigami's
        // NavigationButtons).
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        if status.success() && errors.is_empty() {
            println!("test account actions: {case} ... ok");
        } else {
            println!("test account actions: {case} ... FAILED ({status})");
            failed += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    let passed = cases.len() - failed;
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed");
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed");
        std::process::exit(1);
    }
}
