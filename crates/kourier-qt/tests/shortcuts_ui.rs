//! The window's keyboard shortcuts and the keyboard's way through the window (KDE HIG pass,
//! architecture §14.2 item 4), with real key events on the app's own main window over the
//! real core on the fake network (offscreen, `harness = false`; the events come from Qt's
//! `TestEvent`, delivered to the window like the platform's):
//!
//! - Ctrl+K (and the platform's Find, Ctrl+F) puts the keyboard in the conversation list's
//!   search field; Enter there opens the first chat that matches and the composer takes the
//!   keyboard (the quick switch);
//! - Alt+Down / Ctrl+Tab open the next chat of the list, Alt+Up / Ctrl+Shift+Tab the one
//!   above (the ends do not wrap), also while the composer has the keyboard;
//! - a dialog has the keyboard to itself: the chat keys do nothing while one is open;
//! - Ctrl+, opens the settings; Ctrl+Q asks the application to quit (`closedForGood`);
//! - the tab order of the window is the search field, the conversations, the timeline and the
//!   composer;
//! - a row of the conversation list has a menu (the Menu key and Shift+F10 on the current row,
//!   a right click) with "Mark as Read" only for a chat with unread messages; the entry,
//!   chosen by the keyboard, clears that chat's unread count without opening the chat and
//!   leaves the other chats alone;
//! - every button and every text field that is on screen has an accessible name (its text, or
//!   `Accessible.name`).

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

// The app's own main window is the root (wide: the list and the chat side by side).
MainWindow {
    id: win

    width: 1100
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property bool quitSeen: false
    property real first: 0
    property real second: 0
    property int rowOpened: 0
    // The list rows of the "Mark as Read" steps: one with news, one without, the open chat
    // before, the news chat's id and the other chats that have news.
    property int menuUnread: -1
    property int menuRead: -1
    property real openBefore: 0
    property real unreadId: 0
    property var otherUnread: []

    onClosedForGood: win.quitSeen = true

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
    }

    function key(k: int, modifiers: int): void {
        if (!win.ev.keyClick(k, modifiers, -1)) {
            win.fail("the key event was not delivered");
        }
    }

    // The first item below `root` that `pred` accepts.
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

    function searchField(): Item {
        return win.find(win.pageStack.get(0), i => i.placeholderText === "Search" && i.selectAll !== undefined);
    }

    function composerField(): Item {
        return win.chatPage ? win.find(win.chatPage.composer, i => i.placeholderText !== undefined && i.cursorPosition !== undefined) : null;
    }

    // Buttons and text fields on screen that a screen reader would meet without a name.
    function unnamed(root: Item): var {
        const bad = [];
        const walk = item => {
            if (!item.visible || !item.enabled || item.width <= 0 || item.height <= 0) {
                return;
            }
            const button = item.pressed !== undefined && item.text !== undefined && item.checkable !== undefined;
            const field = item.placeholderText !== undefined && item.cursorPosition !== undefined;
            if (button && item.text === "" && item.Accessible.name === "") {
                bad.push("button " + item);
            } else if (field && item.Accessible.name === "") {
                bad.push("field " + item + " (" + item.placeholderText + ")");
            }
            for (const child of item.children) {
                walk(child);
            }
        };
        walk(root);
        return bad;
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
        case "start":
            if (!win.started || !App.hasAccounts || App.conversationAt(1) === 0) return win.later("the list");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "shortcuts_ui_events.qml");
            win.first = App.conversationAt(0);
            win.second = App.conversationAt(1);
            win.next("find");
            return;
        case "find": {
            // Ctrl+K: the keyboard is in the search field.
            const search = win.searchField();
            if (!search) return win.later("the search field");
            if (search.activeFocus) return win.fail("the search field has the keyboard at the start");
            win.key(Qt.Key_K, Qt.ControlModifier);
            if (!search.activeFocus) return win.fail("Ctrl+K does not focus the search field");
            // The platform's Find does the same from anywhere.
            win.contentItem.forceActiveFocus();
            if (search.activeFocus) return win.fail("the focus should have left the search field");
            win.key(Qt.Key_F, Qt.ControlModifier);
            if (!search.activeFocus) return win.fail("Ctrl+F does not focus the search field");
            // Enter in the search field: the first chat opens, the composer has the keyboard.
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("switched");
            return;
        }
        case "switched": {
            if (App.timeline.conversationId !== win.first) return win.later("the first chat opened by Enter");
            const area = win.composerField();
            if (!area || !area.activeFocus) return win.later("the composer has the keyboard after the quick switch");
            if (win.searchField().text !== "") return win.fail("the search field is cleared after the switch");
            win.next("next");
            return;
        }
        case "next":
            // With the composer focused: the next chat, then the previous one, by every key.
            win.key(Qt.Key_Down, Qt.AltModifier);
            win.next("next-opened");
            return;
        case "next-opened":
            if (App.timeline.conversationId !== win.second) return win.later("Alt+Down opens the next chat");
            if (!win.composerField().activeFocus) return win.fail("Alt+Down moved the keyboard out of the composer");
            win.key(Qt.Key_Up, Qt.AltModifier);
            win.next("previous-opened");
            return;
        case "previous-opened":
            if (App.timeline.conversationId !== win.first) return win.later("Alt+Up opens the previous chat");
            win.key(Qt.Key_Tab, Qt.ControlModifier);
            win.next("ctrl-tab");
            return;
        case "ctrl-tab":
            if (App.timeline.conversationId !== win.second) return win.later("Ctrl+Tab opens the next chat");
            win.key(Qt.Key_Tab, Qt.ControlModifier | Qt.ShiftModifier);
            win.next("ctrl-shift-tab");
            return;
        case "ctrl-shift-tab":
            if (App.timeline.conversationId !== win.first) return win.later("Ctrl+Shift+Tab opens the previous chat");
            // The first row has no row above it: nothing happens, nothing wraps.
            win.key(Qt.Key_Up, Qt.AltModifier);
            win.next("top");
            return;
        case "top":
            win.waits += 1;
            if (win.waits < 10) return;
            if (App.timeline.conversationId !== win.first) return win.fail("Alt+Up on the first chat went to " + App.timeline.conversationId);
            // A dialog has the keyboard to itself.
            win.chatPage.askClear();
            win.next("dialog");
            return;
        case "dialog": {
            const dialog = win.chatPage.deleteDialog;
            if (!dialog || !dialog.opened) return win.later("the dialog");
            win.key(Qt.Key_Down, Qt.AltModifier);
            win.next("dialog-held");
            return;
        }
        case "dialog-held":
            win.waits += 1;
            if (win.waits < 10) return;
            if (App.timeline.conversationId !== win.first) return win.fail("Alt+Down opened a chat behind a dialog");
            win.chatPage.deleteDialog.close();
            win.next("dialog-closed");
            return;
        case "dialog-closed":
            if (win.chatPage.deleteDialog && win.chatPage.deleteDialog.opened) return win.later("the dialog closes");
            // (Its closing transition is over a moment later.)
            win.waits += 1;
            if (win.waits < 20) return;
            win.next("order");
            return;
        case "order": {
            // The tab order of the window: search, conversations, timeline, composer, and
            // back with Shift+Tab.
            const search = win.searchField();
            win.key(Qt.Key_K, Qt.ControlModifier);
            if (!search.activeFocus) return win.fail("Ctrl+K does not focus the search field: keys " + win.keysActive + ", focus in " + win.activeFocusItem + ", overlay " + win.inOverlay(win.activeFocusItem) + ", active " + win.active + ", dialog " + win.chatPage.deleteDialog);
            // (A list is a focus scope: it has the keyboard while a row of it does.)
            const rows = win.pageStack.get(0).conversations;
            const timeline = win.chatPage.timeline.rowList;
            const composer = win.chatPage.composer.firstStop;
            const at = (what, item) => {
                if (!item.activeFocus) {
                    win.fail("the keyboard should be in " + what + ", it is in " + win.activeFocusItem);
                    return false;
                }
                return true;
            };
            win.key(Qt.Key_Tab, Qt.NoModifier);
            if (!at("the conversations after the search field", rows)) return;
            win.key(Qt.Key_Tab, Qt.NoModifier);
            if (!at("the timeline after the conversations", timeline)) return;
            win.key(Qt.Key_Tab, Qt.NoModifier);
            if (!at("the composer after the timeline", composer)) return;
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            if (!at("the timeline before the composer", timeline)) return;
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            if (!at("the conversations before the timeline", rows)) return;
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            if (!at("the search field before the conversations", search)) return;
            win.next("rows");
            return;
        }
        case "rows": {
            // The conversations take Down and Enter: the row below the open one opens.
            win.key(Qt.Key_Tab, Qt.NoModifier);
            const list = win.pageStack.get(0).conversations;
            if (!list.activeFocus) return win.fail("the keyboard is not in the list");
            const from = list.currentIndex;
            win.key(Qt.Key_Down, Qt.NoModifier);
            if (list.currentIndex !== from + 1) return win.fail("Down from row " + from + ": current " + list.currentIndex);
            win.rowOpened = list.currentIndex;
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("row-opened");
            return;
        }
        case "row-opened":
            if (App.timeline.conversationId !== App.conversationAt(win.rowOpened)) return win.later("Enter opens the current row of the list");
            win.next("menu-find");
            return;
        case "menu-find": {
            // The chats that are not open: the ones with news, and one without.
            const list = win.pageStack.get(0).conversations;
            win.openBefore = App.timeline.conversationId;
            win.menuUnread = -1;
            win.menuRead = -1;
            win.otherUnread = [];
            for (let i = 0; i < list.count; i++) {
                const row = list.itemAtIndex(i);
                if (!row || row.conversationId === win.openBefore) continue;
                if (row.unread > 0 && win.menuUnread < 0) {
                    win.menuUnread = i;
                    win.unreadId = row.conversationId;
                } else if (row.unread > 0) {
                    win.otherUnread.push(row.conversationId);
                } else if (!row.markedUnread && win.menuRead < 0) {
                    win.menuRead = i;
                }
            }
            if (win.menuUnread < 0 || win.menuRead < 0) return win.fail("the fixture has no chat with news and no chat without that is not open: " + win.menuUnread + ", " + win.menuRead);
            win.next("menu-none");
            return;
        }
        case "menu-none": {
            // A chat with nothing unread has nothing to offer: no menu opens, by any way.
            const page = win.pageStack.get(0);
            const list = page.conversations;
            list.forceActiveFocus();
            list.currentIndex = win.menuRead;
            win.key(Qt.Key_Menu, Qt.NoModifier);
            if (page.rowMenu.visible) return win.fail("the Menu key opened a menu for a chat with nothing unread");
            win.key(Qt.Key_F10, Qt.ShiftModifier);
            if (page.rowMenu.visible) return win.fail("Shift+F10 opened a menu for a chat with nothing unread");
            const row = list.itemAtIndex(win.menuRead);
            if (!win.ev.mouseClick(row, row.width / 2, row.height / 2, Qt.RightButton, Qt.NoModifier, -1)) return win.fail("the window took no mouse event");
            if (page.rowMenu.visible) return win.fail("a right click opened a menu for a chat with nothing unread");
            if (App.timeline.conversationId !== win.openBefore) return win.fail("a right click opened the chat");
            win.next("menu-key");
            return;
        }
        case "menu-key": {
            // The Menu key on the current row of a chat with news: its menu, with the entry.
            const page = win.pageStack.get(0);
            const list = page.conversations;
            list.forceActiveFocus();
            list.currentIndex = win.menuUnread;
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("menu-key-open");
            return;
        }
        case "menu-key-open": {
            const page = win.pageStack.get(0);
            if (!page.rowMenu.opened) return win.later("the Menu key opens the menu of the current row");
            if (page.rowMenu.conversationId !== win.unreadId) return win.fail("the menu is for chat " + page.rowMenu.conversationId + ", not " + win.unreadId);
            if (!page.markReadEntry.visible || page.markReadEntry.text !== "Mark as Read") return win.fail("the entry: visible " + page.markReadEntry.visible + ", text " + page.markReadEntry.text);
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("menu-key-closed");
            return;
        }
        case "menu-key-closed": {
            const page = win.pageStack.get(0);
            if (page.rowMenu.visible) return win.later("Escape closes the menu");
            if (!page.conversations.activeFocus) return win.fail("the keyboard did not return to the list, it is in " + win.activeFocusItem);
            // A right click on a row of a chat with news.
            const row = page.conversations.itemAtIndex(win.menuUnread);
            if (!win.ev.mouseClick(row, row.width / 2, row.height / 2, Qt.RightButton, Qt.NoModifier, -1)) return win.fail("the window took no mouse event");
            win.next("menu-click-open");
            return;
        }
        case "menu-click-open": {
            const page = win.pageStack.get(0);
            if (!page.rowMenu.opened) return win.later("a right click opens the menu of the row");
            if (page.rowMenu.conversationId !== win.unreadId || !page.markReadEntry.visible) return win.fail("the menu of a right click is not the row's");
            if (App.timeline.conversationId !== win.openBefore) return win.fail("a right click opened the chat");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("menu-click-closed");
            return;
        }
        case "menu-click-closed": {
            const page = win.pageStack.get(0);
            if (page.rowMenu.visible) return win.later("Escape closes the menu");
            page.conversations.forceActiveFocus();
            page.conversations.currentIndex = win.menuUnread;
            // Shift+F10, then the entry by the keyboard: Down, Enter.
            win.key(Qt.Key_F10, Qt.ShiftModifier);
            win.next("menu-f10-open");
            return;
        }
        case "menu-f10-open": {
            const page = win.pageStack.get(0);
            if (!page.rowMenu.opened) return win.later("Shift+F10 opens the menu of the current row");
            win.key(Qt.Key_Down, Qt.NoModifier);
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("menu-marked");
            return;
        }
        case "menu-marked": {
            // The chat is read; it was not opened; the other chats keep their news.
            const page = win.pageStack.get(0);
            const list = page.conversations;
            if (page.rowMenu.visible) return win.later("the entry closes the menu");
            const find = id => {
                for (let i = 0; i < list.count; i++) {
                    const row = list.itemAtIndex(i);
                    if (row && row.conversationId === id) return row;
                }
                return null;
            };
            const row = find(win.unreadId);
            if (!row) return win.fail("the chat's row is gone");
            if (row.unread > 0) return win.later("Mark as Read clears the chat's unread count");
            if (App.timeline.conversationId !== win.openBefore) return win.fail("Mark as Read opened the chat");
            for (const id of win.otherUnread) {
                const other = find(id);
                if (other && other.unread === 0) return win.fail("another chat was marked read too");
            }
            // The chat has nothing unread now: its menu stays shut.
            list.currentIndex = win.menuUnread;
            win.key(Qt.Key_Menu, Qt.NoModifier);
            if (page.rowMenu.visible) return win.fail("a chat just read still has a menu");
            win.next("names");
            return;
        }
        case "names": {
            const bad = win.unnamed(win.contentItem);
            if (bad.length > 0) return win.fail("without an accessible name: " + bad.join("; "));
            win.key(Qt.Key_Comma, Qt.ControlModifier);
            win.next("settings");
            return;
        }
        case "settings":
            if (win.settingsPage === null || win.pageStack.lastItem !== win.settingsPage) return win.later("Ctrl+, opens the settings");
            win.next("settings-names");
            return;
        case "settings-names": {
            win.waits += 1;
            if (win.waits < 10) return;
            const bad = win.unnamed(win.settingsPage);
            if (bad.length > 0) return win.fail("settings without an accessible name: " + bad.join("; "));
            if (win.quitSeen) return win.fail("quit before Ctrl+Q");
            win.key(Qt.Key_Q, Qt.ControlModifier);
            win.next("quit");
            return;
        }
        case "quit":
            if (!win.quitSeen) return win.later("Ctrl+Q asks to quit");
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 180000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn main() {
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
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
    let mut qt = QGuiApplication::new();

    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(SCRIPT),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/shortcuts_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the keyboard shortcuts ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }

    app.shutdown();
    println!("test the keyboard shortcuts ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
