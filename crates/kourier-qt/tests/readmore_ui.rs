//! "Read more" on a long text, through the real main window and the real core over the fake
//! network (offscreen, `harness = false`; the fixture's chat with one long message). One
//! session of a user, in a narrow window:
//!
//! - The long message is shown cut, with "Read more" under the text, and the row's Copy takes
//!   the whole text.
//! - A click on "Read more" shows the rest. The row is updated in place (no reset, no row
//!   inserted or removed), the view stops following the newest message, the top of the row
//!   and the line where the text was cut stay where they were on the screen, and the view is
//!   no longer at the newest message's end.
//! - The row goes out of view (its delegate is destroyed) and comes back: it is still
//!   expanded.
//! - Another chat is opened and this one again: the message is cut again.
//! - From the keyboard: Enter on the current row shows more of the text, and does not open the
//!   row's menu.

mod support;

use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

// The app's own main window is the root (narrow: one page at a time).
MainWindow {
    id: win

    // Low: the chat has one message, and the view can hold a line of it in place only while
    // the content is taller than the view (the cut message and the text that follows it).
    width: 520
    height: 380
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    // The long message, and the whole length of its text.
    property real long: 0
    property int full: 0
    // The row as it was before "Read more": its top and the line where the text was cut, in
    // the view's coordinates, and how much of the text was shown.
    property var before: null
    property string seen: ""
    property int still: 0
    // What the model did since the click.
    property int resets: 0
    property int inserted: 0
    property int removed: 0
    property int changed: 0

    readonly property var chat: win.chatPage
    readonly property var view: win.chatPage ? win.chatPage.timeline : null

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
        win.seen = "";
        win.still = 0;
    }

    function key(k: int, modifiers: int): void {
        if (!win.ev.keyClick(k, modifiers, -1)) {
            fail("the window took no key event");
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

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property var blocks
        }
    }

    Connections {
        target: App.timeline
        function onModelReset(): void {
            win.resets += 1;
        }
        function onRowsInserted(): void {
            win.inserted += 1;
        }
        function onRowsRemoved(): void {
            win.removed += 1;
        }
        function onDataChanged(): void {
            win.changed += 1;
        }
    }

    // How many rows a person wrote.
    function written(): int {
        let n = 0;
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            n += r && r.kind === "message" ? 1 : 0;
        }
        return n;
    }

    // The model's row of the long message: its text is more than the first step shows.
    function longRow(): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.blocks.length > 0 && r.blocks[0].type === "text" && r.blocks[0].plain.length > 1000) {
                return r;
            }
        }
        return null;
    }

    function indexOf(id: real): int {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
                return i;
            }
        }
        return -1;
    }

    // The long message on screen: its text, its "Read more" (`null` when it has none), and
    // where its top and the line of character `at` are in the view; `null` while it has no
    // delegate.
    function look(at: int): var {
        const d = win.view.delegateFor(win.long);
        if (d === null) {
            return null;
        }
        const edit = win.find(d, c => c.selectByMouse === true && c.visible && c.height > 0);
        if (edit === null) {
            return null;
        }
        // (The label says how much is left: "Read more (123 words)".)
        const more = win.find(d, c => /^Read more \(\d+ words?\)$/.test(c.text ?? "") && c.visible && c.width > 0);
        return {
            "delegate": d,
            "edit": edit,
            "more": more,
            "length": edit.length,
            "top": d.mapToItem(win.view, 0, 0).y,
            "line": edit.mapToItem(win.view, 0, edit.positionToRectangle(Math.max(0, at - 1)).y).y
        };
    }

    // True once what `look` reports has stood still for a few ticks (the row is laid out).
    function settled(l: var): bool {
        const now = l.length + ":" + Math.round(l.top) + ":" + Math.round(l.line) + ":" + (l.more !== null);
        win.still = now === win.seen ? win.still + 1 : 0;
        win.seen = now;
        return win.still >= 6;
    }

    // The keyboard is in the timeline, on the row of `id` (which is brought on screen).
    function focusRow(id: real): bool {
        const at = win.indexOf(id);
        if (at < 0) {
            return false;
        }
        const list = win.view.rowList;
        win.view.followNewest = false;
        list.positionViewAtIndex(at, ListView.Center);
        list.forceActiveFocus();
        list.currentIndex = at;
        return true;
    }

    function rowWithKeyboard(id: real): var {
        const d = win.view.delegateFor(id);
        const list = win.view.rowList;
        if (d === null || !list.activeFocus || list.currentItem !== d) {
            win.focusRow(id);
            return null;
        }
        return d;
    }

    // An input event can spin the event loop while it is delivered: the steps never nest.
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
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            const list = win.pageStack.get(0);
            if (!list || !list.rowsShown || list.flickable.count < 15) return win.later("the list with its rows");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "readmore_ui_events.qml");
            win.openConversation(LONG_CHAT);
            return win.next("collapsed");
        }
        case "collapsed": {
            if (win.chatPage === null || App.timeline.conversationId !== LONG_CHAT || App.timeline.loading) return win.later("the chat");
            const r = win.longRow();
            if (r === null) return win.later("the long message");
            win.long = r.messageId;
            win.full = r.blocks[0].plain.length;
            const l = win.look(0);
            if (l === null || !win.settled(l)) return win.later("the row laid out");
            if (!r.blocks[0].more) return win.fail("the long message is not cut");
            if (l.more === null) return win.fail("no \"Read more\" under the text");
            if (l.length > win.full - 500) return win.fail("the text is shown in full: " + l.length + " of " + win.full);
            if (l.length < 700) return win.fail("what is shown is short: " + l.length);
            // Copy takes the whole text, not the part that is shown.
            if (l.delegate.copyText.length !== win.full) return win.fail("the menu's Copy takes " + l.delegate.copyText.length + " of " + win.full);
            if (!win.view.followNewest) return win.fail("the view does not follow the newest message at the start");
            // (Where the text was cut: its last character.)
            win.before = win.look(l.length);
            win.resets = 0;
            win.inserted = 0;
            win.removed = 0;
            win.changed = 0;
            if (!win.ev.mouseClick(l.more, l.more.width / 2, l.more.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) return win.fail("the window took no mouse event");
            return win.next("expanded");
        }
        case "expanded": {
            const l = win.look(win.before.length);
            if (l === null) return win.fail("the row went away");
            if (l.length <= win.before.length + 100 || !win.settled(l)) return win.later("the rest of the text");
            if (l.length < win.full - 2) return win.fail("the text is not shown in full: " + l.length + " of " + win.full);
            if (l.more !== null) return win.fail("\"Read more\" is still there with all the text shown");
            // In place: the row changed, and nothing came or went.
            if (win.resets !== 0 || win.inserted !== 0 || win.removed !== 0 || win.changed < 1) return win.fail("the model: resets " + win.resets + ", inserted " + win.inserted + ", removed " + win.removed + ", changed " + win.changed);
            // The view holds on to what is being read.
            if (win.view.followNewest) return win.fail("the view still follows the newest message");
            if (Math.abs(l.top - win.before.top) > 2) return win.fail("the top of the row moved from " + win.before.top + " to " + l.top);
            const was = win.look(win.before.length);
            if (Math.abs(was.line - win.before.line) > 2) return win.fail("the line where the text was cut moved from " + win.before.line + " to " + was.line);
            if (win.view.atBottom) return win.fail("the view is at the newest message's end: the new text is below it");
            // Out of view: messages come after it and the view goes to the newest one.
            for (let i = 1; i <= 30; ++i) {
                App.composer.send(LONG_CHAT, "Later message " + i);
            }
            return win.next("later");
        }
        case "later": {
            if (win.written() < 31) return win.later("the messages that were sent");
            win.view.followNewest = true;
            win.view.rowList.toNewest();
            if (!win.view.atBottom || win.view.delegateFor(win.long) !== null) return win.later("the long message out of view");
            return win.next("back");
        }
        case "back": {
            win.view.followNewest = false;
            win.view.rowList.positionViewAtIndex(win.indexOf(win.long), ListView.Center);
            const l = win.look(win.full);
            if (l === null || !win.settled(l)) return win.later("the long message back in view");
            if (l.length < win.full - 2 || l.more !== null) return win.fail("the row is not expanded any more: " + l.length + " of " + win.full);
            return win.next("away");
        }
        case "away": {
            win.openConversation(OTHER_CHAT);
            return win.next("other");
        }
        case "other": {
            if (App.timeline.conversationId !== OTHER_CHAT || App.timeline.loading || rows.count < 3) return win.later("the other chat");
            win.openConversation(LONG_CHAT);
            return win.next("again");
        }
        case "again": {
            if (App.timeline.conversationId !== LONG_CHAT || App.timeline.loading) return win.later("the chat again");
            const r = win.longRow();
            if (r === null) return win.later("the long message again");
            if (!r.blocks[0].more) return win.fail("the message is still expanded in the chat that was opened again");
            win.focusRow(r.messageId);
            const l = win.look(0);
            if (l === null || !win.settled(l)) return win.later("the row on screen");
            if (l.more === null || l.length > win.full - 500) return win.fail("the row is not cut again: " + l.length + " of " + win.full);
            return win.next("keyboard");
        }
        case "keyboard": {
            const d = win.rowWithKeyboard(win.long);
            if (d === null) return win.later("the keyboard on the row");
            win.key(Qt.Key_Return, Qt.NoModifier);
            return win.next("entered");
        }
        case "entered": {
            const r = win.longRow();
            if (r === null || r.blocks[0].more) return win.later("more of the text after Enter");
            if (win.view.messageMenu.visible) return win.fail("Enter opened the menu instead of showing more");
            const l = win.look(0);
            if (l === null || l.length < win.full - 2 || l.more !== null) return win.later("the whole text shown");
            Qt.exit(0);
        }
        }
    }
}
"##;

fn main() {
    support::answer_list("readmore_ui");
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
    let fixture = handle.fixture().expect("fixture mode");
    let long = fixture
        .readmore_conversation
        .expect("the chat with a long text");
    let other = fixture.media_conversation.expect("another chat");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT
        .replace("LONG_CHAT", &long.0.to_string())
        .replace("OTHER_CHAT", &other.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/readmore_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test \"Read more\" on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    // (The messages the test sent are stored by now or on their way: let the core finish.)
    std::thread::sleep(Duration::from_millis(200));
    app.shutdown();
    println!("test \"Read more\" on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
