//! The selection mode of the timeline (Export Selected, step SEL), with real input events on the
//! app's own timeline and composer over the real core on the fake network: a Ctrl+click on a message
//! starts the mode (and a link in its text is not opened), a plain click toggles a row, Shift+click
//! selects the range from the anchor, a click on a picture or a link while selecting only toggles
//! (the viewer does not open, no question is asked), the wheel still scrolls the list, the menu's
//! "Select" and "Deselect", Space toggles the keyboard's current row, Ctrl+C copies the selected
//! messages as text to the clipboard, and Escape ends the mode and brings the composer back with its
//! draft intact. A model reset keeps the selection and a row that went leaves the count
//! (`models.rs` `the_selection_lives_in_the_model`: the fake cannot burst rows at a chosen moment);
//! another chat ends the mode (the same check). Offscreen, `harness = false`.

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

Window {
    id: test

    width: 560
    height: 520
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property real linkId: 0
    property real a: 0
    property real b: 0
    property real c: 0
    property real pic: 0
    property int viewed: 0
    property int before: 0
    property real scrollFrom: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        test.waits += 1;
        if (test.waits > 1500) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        test.waits = 0;
        test.step = step;
    }

    function key(k: int, modifiers: int): void {
        if (!test.ev.keyClick(k, modifiers, -1)) {
            fail("the key event was not delivered");
        }
    }

    function click(item: Item, x: real, y: real, button: int, modifiers: int): void {
        if (!test.ev.mouseClick(item, x, y, button, modifiers, -1)) {
            fail("the mouse event was not delivered");
        }
    }

    function pasted(): string {
        const edit = Qt.createQmlObject("import QtQuick; TextEdit { visible: false }", test, "selectPaste");
        edit.paste();
        const text = edit.text;
        edit.destroy();
        return text;
    }

    function row(id: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
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

    function hasBlock(r: var, type: string): bool {
        for (const b of r.blocks ?? []) {
            if (b.type === type) {
                return true;
            }
        }
        return false;
    }

    function textOf(r: var): string {
        for (const b of r.blocks ?? []) {
            if (b.type === "text") {
                return b.plain;
            }
        }
        return "";
    }

    // The message rows (not notices) with a plain text and no link, newest first.
    function plainTexts(): var {
        const out = [];
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && !r.deleted && test.hasBlock(r, "text") && r.messageId !== test.linkId && !test.hasBlock(r, "image") && !test.hasBlock(r, "album")) {
                out.push(r.messageId);
            }
        }
        return out;
    }

    function selectable(): var {
        const out = [];
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId > 0) {
                out.push(r.messageId);
            }
        }
        return out;
    }

    function selectedIds(): var {
        const out = [];
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.selected) {
                out.push(r.messageId);
            }
        }
        return out;
    }

    // The row is in view with a delegate: false (and the view moves there) until it is.
    function shown(id: real): var {
        const at = test.indexOf(id);
        if (at < 0) {
            return null;
        }
        view.rowList.positionViewAtIndex(at, ListView.Contain);
        const d = view.delegateFor(id);
        if (!d) {
            return null;
        }
        const top = d.mapToItem(view, 0, 0).y;
        if (top < 0 || top + d.height > view.height + 1) {
            return null;
        }
        return d;
    }

    function find(item: Item, fits: var): Item {
        if (!item) {
            return null;
        }
        for (let i = 0; i < item.children.length; ++i) {
            const child = item.children[i];
            if (fits(child)) {
                return child;
            }
            const deeper = test.find(child, fits);
            if (deeper) {
                return deeper;
            }
        }
        return null;
    }

    // A click into the row's bubble (a point of text-free padding where there is one).
    function clickRow(id: real, button: int, modifiers: int): bool {
        const d = test.shown(id);
        if (!d) {
            return false;
        }
        const bubble = test.find(d, c => c.objectName === "bubble");
        if (!bubble) {
            fail("the row has a bubble");
            return false;
        }
        test.click(bubble, bubble.width - 3, bubble.height - 3, button, modifiers);
        return true;
    }

    function entry(text: string): Item {
        const m = view.messageMenu;
        for (let i = 0; i < m.count; ++i) {
            const item = m.itemAt(i);
            if (item && item.visible && item.text === text) {
                return item;
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
            required property bool own
            required property bool deleted
            required property var blocks
            required property bool selected
        }
    }

    Connections {
        target: MediaActions
        function onViewRequested(): void {
            test.viewed += 1;
        }
    }

    Item {
        id: holder
    }

    TimelineView {
        chatModel: App.timeline
        composerModel: App.composer
        id: view
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.bottom: composerItem.top
        onComposerRequested: composerItem.focusInput()
    }
    Composer {
        chatModel: App.timeline
        composerModel: App.composer
        id: composerItem
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
    }

    property bool busy: false
    Timer {
        interval: 30
        repeat: true
        running: true
        onTriggered: {
            if (test.busy) {
                return;
            }
            test.busy = true;
            try {
                test.tick();
            } finally {
                test.busy = false;
            }
        }
    }

    function expect(count: int, what: string): bool {
        if (App.timeline.selectedCount !== count || test.selectedIds().length !== count) {
            fail(what + ": count " + App.timeline.selectedCount + " (rows " + test.selectedIds().length + "), wanted " + count);
            return false;
        }
        return true;
    }

    function tick(): void {
        switch (test.step) {
        case "start":
            if (!App.loaded || !App.hasAccounts) return;
            try {
                test.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", holder, "select_ui_events.qml");
            } catch (e) {
                return fail("the QtTest QML module is needed for input events (package qml6-module-qttest): " + e);
            }
            App.ready();
            App.openConversation(QUOTES);
            test.requestActivate();
            return next("open");
        case "open":
            if (App.timeline.loading || rows.count < 6) return;
            // An own message with a link, and a draft in the composer.
            composerItem.setText("");
            if (!App.composer.send(QUOTES, "see https://example.org/page")) return fail("the link message was not sent");
            return next("linkrow");
        case "linkrow": {
            for (let i = 0; i < rows.count; ++i) {
                const r = rows.objectAt(i);
                if (r && r.kind === "message" && test.textOf(r).indexOf("example.org") >= 0) {
                    test.linkId = r.messageId;
                }
            }
            if (test.linkId === 0) return later("the link message shows");
            composerItem.setText("draft kept");
            const texts = test.plainTexts();
            if (texts.length < 3) return fail("the fixture chat has three plain texts: " + texts.length);
            test.a = texts[0];
            test.b = texts[1];
            test.c = texts[texts.length - 1];
            if (App.timeline.selecting || composerItem.selecting) return fail("the mode is on at the start");
            return next("ctrl");
        }

        // ---- Ctrl+click starts it, and a link is not opened by it ----
        case "ctrl": {
            if (!test.clickRow(test.a, Qt.LeftButton, Qt.ControlModifier)) return later("the first row is in view");
            return next("started");
        }
        case "started":
            if (!App.timeline.selecting) return later("the Ctrl+click starts the mode");
            if (!test.expect(1, "Ctrl+click")) return;
            if (!test.row(test.a).selected) return fail("the clicked row is the selected one");
            if (!composerItem.selecting) return fail("the composer knows the mode");
            const bar = test.find(composerItem, c => c.objectName === "selectionBar");
            if (!bar || !bar.visible) return fail("the bar stands in the composer's place");
            if (!bar.copyButton.enabled) return fail("Copy as Text is on with one row selected");
            return next("toggle");

        // ---- a click toggles, Shift+click selects the range ----
        case "toggle": {
            if (!test.clickRow(test.b, Qt.LeftButton, Qt.NoModifier)) return later("the second row is in view");
            return next("toggled");
        }
        case "toggled":
            if (App.timeline.selectedCount !== 2) return later("a click selects a second row");
            if (!test.expect(2, "click")) return;
            if (!test.clickRow(test.b, Qt.LeftButton, Qt.NoModifier)) return later("the row is in view");
            return next("untoggled");
        case "untoggled":
            if (App.timeline.selectedCount !== 1) return later("a second click deselects it");
            if (!test.expect(1, "second click")) return;
            if (!test.clickRow(test.b, Qt.LeftButton, Qt.NoModifier)) return later("the row is in view");
            return next("shift");
        case "shift": {
            if (App.timeline.selectedCount !== 2) return later("the row is selected again");
            // The anchor is b now. Shift+click on c: everything from b to c joins.
            const order = test.selectable();
            const from = order.indexOf(test.b);
            const to = order.indexOf(test.c);
            const want = {};
            want[test.a] = true;
            for (let i = Math.min(from, to); i <= Math.max(from, to); ++i) {
                want[order[i]] = true;
            }
            test.before = Object.keys(want).length;
            if (!test.clickRow(test.c, Qt.LeftButton, Qt.ShiftModifier)) return later("the far row is in view");
            return next("shifted");
        }
        case "shifted":
            if (App.timeline.selectedCount !== test.before) return later("Shift+click selects the range: " + App.timeline.selectedCount + " of " + test.before);
            if (!test.expect(test.before, "Shift+click")) return;
            return next("picture");

        // ---- a picture and a link while selecting: they only toggle ----
        case "picture": {
            if (test.pic === 0) {
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "message" && test.hasBlock(r, "image")) {
                        test.pic = r.messageId;
                        break;
                    }
                }
                if (test.pic === 0) return fail("the fixture chat has a picture");
            }
            const d = test.shown(test.pic);
            if (!d) return later("the picture row is in view");
            const img = test.find(d, c => c.boxW !== undefined && c.attachment !== undefined);
            if (!img) return later("the picture block exists");
            test.before = App.timeline.selectedCount;
            const was = test.row(test.pic).selected;
            test.click(img, img.boxX + img.boxW / 2, img.boxH / 2, Qt.LeftButton, Qt.NoModifier);
            test.scrollFrom = was ? -1 : 1;
            return next("pictured");
        }
        case "pictured":
            if (App.timeline.selectedCount === test.before) return later("a click on a picture toggles its row");
            if (App.timeline.selectedCount - test.before !== test.scrollFrom) return fail("a click on a picture changed the count by " + (App.timeline.selectedCount - test.before));
            if (test.viewed !== 0) return fail("a click on a picture while selecting opened the viewer");
            return next("link");
        case "link": {
            const d = test.shown(test.linkId);
            if (!d) return later("the link row is in view");
            const tb = test.find(d, c => c.edit !== undefined && c.openLink !== undefined);
            if (!tb) return fail("the link row has a text block");
            const at = tb.edit.getText(0, tb.edit.length).indexOf("https");
            const r = tb.edit.positionToRectangle(at + 3);
            test.before = App.timeline.selectedCount;
            test.scrollFrom = test.row(test.linkId).selected ? -1 : 1;
            test.click(tb.edit, r.x + 3, r.y + r.height / 2, Qt.LeftButton, Qt.NoModifier);
            return next("linked");
        }
        case "linked": {
            if (App.timeline.selectedCount === test.before) return later("a click on a link toggles its row");
            if (App.timeline.selectedCount - test.before !== test.scrollFrom) return fail("a click on a link changed the count by " + (App.timeline.selectedCount - test.before));
            const d = view.delegateFor(test.linkId);
            const tb = test.find(d, c => c.edit !== undefined && c.openLink !== undefined);
            if (tb.question !== null) return fail("a click on a link while selecting asked to open it");
            return next("wheel");
        }

        // ---- the wheel still scrolls the list ----
        case "wheel": {
            if (test.waits === 0) {
                view.rowList.positionViewAtIndex(Math.floor(view.rowList.count / 2), ListView.Center);
                test.scrollFrom = view.rowList.contentY;
                test.waits = 1;
                return;
            }
            if (test.waits === 1) {
                test.scrollFrom = view.rowList.contentY;
                if (!test.ev.mouseWheel(view.rowList, view.rowList.width / 2, view.rowList.height / 2, Qt.NoButton, Qt.NoModifier, 0, 120, -1)) return fail("the wheel event was not delivered");
                test.waits = 2;
                return;
            }
            if (Math.abs(view.rowList.contentY - test.scrollFrom) < 1) {
                test.waits += 1;
                if (test.waits > 100) return fail("the wheel did not scroll the list while selecting");
                return;
            }
            return next("menu");
        }

        // ---- the menu: Deselect on a selected row, Select on another ----
        case "menu": {
            if (test.waits === 0) {
                test.before = App.timeline.selectedCount;
                test.a = test.selectedIds()[0];
                test.waits = 1;
            }
            if (test.waits === 1) {
                if (!test.clickRow(test.a, Qt.RightButton, Qt.NoModifier)) return;
                test.waits = 2;
                return;
            }
            if (!view.popupOpen || !view.messageMenu.opened) {
                test.waits += 1;
                if (test.waits > 200) return fail("the right click opens the menu while selecting");
                return;
            }
            const deselect = test.entry("Deselect");
            if (!deselect) return fail("a selected row's menu has Deselect");
            test.click(deselect, deselect.width / 2, deselect.height / 2, Qt.LeftButton, Qt.NoModifier);
            return next("menu2");
        }
        case "menu2":
            if (view.popupOpen) return later("the menu closes");
            if (App.timeline.selectedCount !== test.before - 1) return later("Deselect drops the row");
            if (!test.expect(test.before - 1, "Deselect")) return;
            return next("menu3");
        case "menu3": {
            if (test.waits === 0) {
                test.before = App.timeline.selectedCount;
                test.waits = 1;
            }
            if (test.waits === 1) {
                if (!test.clickRow(test.a, Qt.RightButton, Qt.NoModifier)) return;
                test.waits = 2;
                return;
            }
            if (!view.popupOpen || !view.messageMenu.opened) {
                test.waits += 1;
                if (test.waits > 200) return fail("the right click opens the menu again");
                return;
            }
            const select = test.entry("Select");
            if (!select) return fail("an unselected row's menu has Select");
            test.click(select, select.width / 2, select.height / 2, Qt.LeftButton, Qt.NoModifier);
            return next("menu4");
        }
        case "menu4":
            if (view.popupOpen) return later("the menu closes");
            if (App.timeline.selectedCount !== test.before + 1) return later("Select adds the row");
            return next("space");

        // ---- Space toggles the keyboard's current row ----
        case "space": {
            if (test.waits === 0) {
                view.rowList.forceActiveFocus();
                test.key(Qt.Key_Down, Qt.NoModifier);
                test.before = App.timeline.selectedCount;
                test.waits = 1;
                return;
            }
            if (view.rowList.currentIndex < 0) return later("a key makes a row current");
            if (test.waits === 1) {
                test.key(Qt.Key_Space, Qt.NoModifier);
                test.waits = 2;
                return;
            }
            if (App.timeline.selectedCount === test.before) return later("Space toggles the current row");
            if (Math.abs(App.timeline.selectedCount - test.before) !== 1) return fail("Space changed the count by " + (App.timeline.selectedCount - test.before));
            if (view.popupOpen || test.viewed !== 0) return fail("Space while selecting opened something");
            return next("escape");
        }

        // ---- Escape ends it; the composer is back with its draft ----
        case "escape":
            if (test.waits === 0) {
                view.rowList.forceActiveFocus();
                test.key(Qt.Key_Escape, Qt.NoModifier);
                test.waits = 1;
                return;
            }
            if (App.timeline.selecting) return later("Escape ends the mode");
            if (App.timeline.selectedCount !== 0 || test.selectedIds().length !== 0) return fail("nothing stays selected");
            if (composerItem.selecting) return fail("the composer is back");
            if (composerItem.text !== "draft kept") return fail("the draft is intact: '" + composerItem.text + "'");
            const bar2 = test.find(composerItem, c => c.objectName === "selectionBar");
            if (bar2 && bar2.visible) return fail("the bar is gone");
            return next("control");
        case "control": {
            // Outside the mode a plain click on a picture does what it always did: the viewer asks.
            const d = test.shown(test.pic);
            if (!d) return later("the picture row is in view");
            const img = test.find(d, c => c.boxW !== undefined && c.attachment !== undefined);
            if (!img) return later("the picture block exists");
            test.click(img, img.boxX + img.boxW / 2, img.boxH / 2, Qt.LeftButton, Qt.NoModifier);
            return next("controlled");
        }
        case "controlled":
            if (test.viewed === 0) return later("outside the mode a click on a picture asks for the viewer");
            if (App.timeline.selecting) return fail("a plain click outside the mode selects");
            return next("copy");

        // ---- Copy as Text: the own link message, by Ctrl+click and Ctrl+C ----
        case "copy":
            if (test.waits === 0) {
                if (!test.clickRow(test.linkId, Qt.LeftButton, Qt.ControlModifier)) return;
                test.waits = 1;
                return;
            }
            if (!App.timeline.selecting) return later("Ctrl+click on the link row starts the mode");
            if (test.waits === 1) {
                const d = view.delegateFor(test.linkId);
                const tb = test.find(d, c => c.edit !== undefined && c.openLink !== undefined);
                if (tb && tb.question !== null) return fail("a Ctrl+click on a link opened it");
                if (!test.expect(1, "Ctrl+click on the link row")) return;
                view.rowList.forceActiveFocus();
                test.key(Qt.Key_C, Qt.ControlModifier);
                test.waits = 2;
                return;
            }
            const text = test.pasted();
            if (text === "") return later("Ctrl+C puts the text on the clipboard");
            if (text.charAt(0) !== "[" || text.indexOf("] You: see https://example.org/page") < 0 || text.indexOf("\n") >= 0) return fail("the copied text: '" + text + "'");
            if (!App.timeline.selecting) return fail("the mode stays on after a copy");
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 240000
        running: true
        onTriggered: fail("timeout in step " + test.step)
    }
}
"#;

fn main() {
    support::answer_list("select_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
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
    let fixture = handle.fixture().expect("fixture data");
    let quotes = fixture.quotes_conversation.expect("the quotes chat");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT.replace("QUOTES", &quotes.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/select_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    app.shutdown();
    if code == 0 {
        println!(
            "test the selection mode of the timeline ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
    } else {
        println!(
            "test the selection mode of the timeline ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
