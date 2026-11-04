//! The line under an own message on its way, through the real main window and the real core
//! over the fake network (offscreen, `harness = false`; link feedback L6). A narrow window at
//! the minimum width, one slow send whose attempt reports a step with a very long count:
//!
//! - After a few seconds the line shows under the bubble in two labels, the reason and the
//!   timer. The reason is cut short ("…") because it does not fit, the timer is whole: its text
//!   is "· m:ss", it is not truncated and has its natural width, and the line stays within the
//!   bubble's widest width and the row.
//! - The row's accessible description says why the message waits, without the seconds, and
//!   puts no second stop after a reason that ends in "…" or a stop.
//! - The timer counts: the second changes while the model does not reset, nothing is inserted
//!   or removed, and every change of the model is the one row's line (`waitTimer`, and `wait` when the reason changes).
//!
//!
//! The cases `xtile`, `beside`, `plus`, `file` and `menu` are the rings and the X of a file on its
//! way (upload progress), each in a process of its own (see `UPLOAD_SCRIPT`). The cases `late`
//! and `latenote` are the X that comes too late (the fake answers the stop `TooLate`): the note
//! "Already sent" with its Delete for Everyone, which opens the question of the message menu and
//! never deletes at once (`late`: Cancel leaves the message), and which rides on its own note
//! only (`latenote`: a notice the composer sets itself takes the action away).
//!
//! Needs a debug build (`KOURIER_DEV_FIXTURE_CLOCK=run` lets the UI's clock run with the
//! fixture's); in a release test build it skips.

mod support;

use std::process::Command;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{
    App, FixtureOptions, FixtureSends, FixtureUpload, Launch, SendPhase, StartOptions,
};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

// The app's own main window is the root (narrow: one page at a time, at its minimum width).
MainWindow {
    id: win

    width: 360
    height: 700
    visible: true

    property string step: "start"
    property int waits: 0
    property int still: 0
    property string seen: ""
    property real sent: 0
    property string timerAtFirst: ""
    property int seconds: 0
    // What the model did since the line first showed.
    property int resets: 0
    property int inserted: 0
    property int removed: 0
    property var changes: []

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
        win.still = 0;
        win.seen = "";
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
            required property bool own
            required property string wait
            required property string waitTimer
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
        function onDataChanged(topLeft: var, bottomRight: var, roles: var): void {
            win.changes.push(topLeft.row + ":" + bottomRight.row + ":" + roles.length);
        }
    }

    // The own message that has a line.
    function waiting(): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.own && r.wait !== "") {
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

    // The items of the line under the message on screen; `null` while it has no delegate.
    function look(id: real): var {
        const d = win.view.delegateFor(id);
        if (d === null) {
            return null;
        }
        const line = win.find(d, c => c.objectName === "waitLine");
        const reason = win.find(d, c => c.objectName === "waitReason");
        const timer = win.find(d, c => c.objectName === "waitTimer");
        if (!line || !reason || !timer) {
            return null;
        }
        return { "d": d, "line": line, "reason": reason, "timer": timer };
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
            win.openConversation(THE_CHAT);
            return win.next("opened");
        }
        case "opened": {
            if (win.chatPage === null || App.timeline.conversationId !== THE_CHAT || App.timeline.loading || rows.count < 3) return win.later("the chat");
            App.composer.send(THE_CHAT, "Are we still on for Saturday?");
            return win.next("line");
        }
        case "line": {
            // The send takes two minutes on this network: the line shows after three seconds.
            const r = win.waiting();
            if (r === null) return win.later("the line under the message");
            win.view.followNewest = true;
            const l = win.look(r.messageId);
            if (l === null) return win.later("the line on screen");
            const now = l.reason.text + "|" + l.timer.text + "|" + Math.round(l.line.width);
            win.still = now === win.seen ? win.still + 1 : 0;
            win.seen = now;
            if (win.still < 5) return win.later("the line laid out");
            // The two labels.
            if (!l.line.visible || !l.reason.visible || !l.timer.visible) return win.fail("a label is hidden");
            if (!/^· \d+:\d\d$/.test(l.timer.text)) return win.fail("the timer says '" + l.timer.text + "'");
            if (l.timer.text.indexOf(l.reason.text) >= 0 || l.reason.text.indexOf("Setting up encryption") !== 0) return win.fail("the reason says '" + l.reason.text + "'");
            // The reason does not fit the bubble's widest width and is cut; the timer never is.
            if (!l.reason.truncated) return win.fail("the reason is not cut: " + l.reason.width + " of " + l.reason.implicitWidth + " px");
            if (l.timer.truncated || l.timer.width < l.timer.implicitWidth - 1) return win.fail("the timer is cut: " + l.timer.width + " of " + l.timer.implicitWidth + " px, '" + l.timer.text + "'");
            if (l.line.width > l.d.maxBubble + 1) return win.fail("the line is wider than the bubble allows: " + l.line.width + " > " + l.d.maxBubble);
            const right = l.line.mapToItem(l.d, l.line.width, 0).x;
            if (right > l.d.width + 1) return win.fail("the line leaves the row: " + right + " > " + l.d.width);
            if (l.timer.mapToItem(l.d, l.timer.width, 0).x > l.d.width + 1) return win.fail("the timer leaves the row");
            // A screen reader hears why, without a counter that changes every second.
            if (l.d.waitHint.indexOf("Setting up encryption") !== 0 || /\d:\d\d/.test(l.d.waitHint)) return win.fail("the accessible hint says '" + l.d.waitHint + "'");
            win.sent = r.messageId;
            win.timerAtFirst = l.timer.text;
            win.resets = 0;
            win.inserted = 0;
            win.removed = 0;
            win.changes = [];
            return win.next("counting");
        }
        case "counting": {
            const l = win.look(win.sent);
            if (l === null) return win.fail("the row went away");
            if (l.timer.text === win.timerAtFirst || win.changes.length < 2) return win.later("the timer counting on");
            // The timer counts in place: the one row's two roles, nothing else.
            if (win.resets !== 0 || win.inserted !== 0 || win.removed !== 0) return win.fail("the model: resets " + win.resets + ", inserted " + win.inserted + ", removed " + win.removed);
            const at = win.indexOf(win.sent);
            // (A second later only the timer differs: one role; the reason stays.)
            for (const c of win.changes) {
                if (c !== at + ":" + at + ":1" && c !== at + ":" + at + ":2") return win.fail("a change of the model is '" + c + "', not the row " + at + " with its line's roles");
            }
            // The accessible hint ends the reason with a stop, but not after an ellipsis or a
            // stop of its own ("Sending…" is the commonest reason). Set in one go, read at once:
            // the next tick of the line would give the role its real value back.
            const hints = [];
            for (const reason of ["Sending…", "Waiting for you to verify.", "Waiting for the connection"]) {
                l.d.wait = reason;
                hints.push(l.d.waitHint);
            }
            const want = ["Sending… ", "Waiting for you to verify. ", "Waiting for the connection. "];
            if (JSON.stringify(hints) !== JSON.stringify(want)) return win.fail("the accessible hints say " + JSON.stringify(hints) + ", not " + JSON.stringify(want));
            Qt.exit(0);
        }
        }
    }
}
"##;

// The rings and the X of a file on its way (upload progress): the real main window, the real core
// and the fake network that HOLDS the uploads of one chat (`FixtureSends::Uploading`: the page
// sends the files itself before the window exists, so every case starts with an album of seven
// pictures going up, its first picture up, the second at 45 %, the next two waiting, the fifth
// tile the "+2" ring, and a file queued behind it; `File` is one 30 MB file at 40 %). Real
// mouse events (`TestEvent`, as `chat_ui` sends them); the effect is counted twice, in the window
// (the row goes, `viewersOpened`) and from the commands the fake heard (`AbortTransmit`).
const UPLOAD_SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

MainWindow {
    id: win

    width: 520
    height: 760
    visible: true

    readonly property string theCase: "THE_CASE"
    // One file goes up (a disc with an X), not an album.
    readonly property bool lone: theCase === "file" || theCase === "late" || theCase === "latenote"
    property string step: "start"
    property int waits: 0
    property int still: 0
    property string seen: ""
    property var ev: null
    property real target: 0
    property int viewersBefore: 0
    property int rest: 0

    readonly property var view: win.chatPage ? win.chatPage.timeline : null

    function fail(msg: string): void {
        console.error("FAIL [" + win.theCase + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 600) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.still = 0;
        win.seen = "";
    }

    // Every item below `root` that `pred` accepts.
    function all(root: Item, pred: var, out: var): var {
        if (pred(root)) {
            out.push(root);
        }
        for (const child of root.children) {
            win.all(child, pred, out);
        }
        return out;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property real messageId
        }
    }

    function hasRow(id: real): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
                return true;
            }
        }
        return false;
    }

    // The ring badges of single pictures and album tiles that show an X, the "+N" ring, and the
    // disc of a file that goes up with an X (a Tab stop only then).
    function badges(): var {
        return win.all(win.view, c => c.objectName === "badge" && c.visible && c.parent && c.parent.cancelShown === true, []);
    }

    function moreRing(): var {
        return win.all(win.view, c => c.objectName === "moreRing" && c.visible, [])[0] ?? null;
    }

    function discs(): var {
        return win.all(win.view, c => c.objectName === "disc" && c.visible && c.focusPolicy === Qt.StrongFocus, []);
    }

    // The button of the note "Already sent" (the composer's notice), when it is on screen.
    function noteAction(): var {
        return win.all(win.contentItem, c => c.text === "Delete for Everyone" && c.visible && c.width > 0 && c.clicked !== undefined, [])[0] ?? null;
    }

    // A button of the open question "Delete Message?" (a popup: its items live in the window's
    // overlay), or the question's title when `text` is empty.
    function inQuestion(text: string): var {
        const title = win.all(QQC2.Overlay.overlay, c => c.text === "Delete Message?" && c.visible, [])[0] ?? null;
        if (title === null || text === "") {
            return title;
        }
        return win.all(QQC2.Overlay.overlay, c => c.text === text && c.visible && c.width > 0 && c.clicked !== undefined, [])[0] ?? null;
    }

    // The message a part of a row belongs to.
    function messageOf(item: Item): real {
        for (let p = item; p; p = p.parent) {
            if (p.messageId !== undefined && p.messageId > 0) {
                return p.messageId;
            }
        }
        return 0;
    }

    // What the case works on: the item, the point to click (window coordinates) and its message.
    function subject(): var {
        let item = null;
        switch (win.theCase) {
        case "plus":
            item = win.moreRing();
            break;
        case "file":
        case "late":
        case "latenote": {
            const d = win.discs();
            item = d.length === 1 ? d[0] : null;
            break;
        }
        default: {
            const b = win.badges();
            // The album: the first picture (up, its ring stays), the second, and the two tiles that wait, show their X.
            item = b.length === 4 ? b[0] : null;
        }
        }
        if (item === null) {
            return null;
        }
        let at = item.mapToItem(null, item.width / 2, item.height / 2);
        if (win.theCase === "beside") {
            // The picture's own corner, away from the ring.
            const tile = item.parent;
            at = tile.mapToItem(null, tile.width * 0.12, tile.height * 0.12);
            const c = item.mapToItem(null, item.width / 2, item.height / 2);
            if (Math.hypot(at.x - c.x, at.y - c.y) < item.width / 2 + 6) {
                return null;
            }
        }
        if (at.x < 0 || at.y < 0 || at.x > win.width || at.y > win.height) {
            return null;
        }
        return { "x": at.x, "y": at.y, "id": win.messageOf(item), "item": item };
    }

    // A real click (press and release through the window) at a point of the window.
    function clickWindow(x: real, y: real): void {
        const p = win.contentItem.mapFromItem(null, x, y);
        if (!win.ev.mouseClick(win.contentItem, p.x, p.y, Qt.LeftButton, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
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
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            const list = win.pageStack.get(0);
            if (!list || !list.rowsShown || list.flickable.count < 15) return win.later("the list with its rows");
            win.openConversation(THE_CHAT);
            return win.next("opened");
        }
        case "opened": {
            if (win.chatPage === null || App.timeline.conversationId !== THE_CHAT || App.timeline.loading) return win.later("the chat");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "wait_ui_events.qml");
            win.view.followNewest = true;
            return win.next("rings");
        }
        case "rings": {
            // The thing is on screen and has stopped moving.
            const t = win.subject();
            if (t === null) return win.later("the ring on screen");
            const now = Math.round(t.x) + "," + Math.round(t.y) + "," + t.id;
            win.still = now === win.seen ? win.still + 1 : 0;
            win.seen = now;
            if (win.still < 5) return win.later("the ring laid out");
            if (t.id <= 0) return win.fail("the ring belongs to no message");
            if (!win.lone && win.moreRing() === null) return win.fail("the album has no '+N' ring");
            // (The first picture is up and keeps its full ring with the X while the others go on,
            // U9: four tiles show an X, the fifth is the "+N" ring.)
            if (!win.lone && win.badges().length !== 4) return win.fail("tiles with an X: " + win.badges().length);
            if (!win.lone) {
                // (U6: the "+N" number is never below the theme's small font, and stands beside
                // the ring or above it, never over it.)
                const more = win.all(win.view, c => c.objectName === "more" && c.visible, [])[0];
                const ring = win.moreRing();
                if (!more) return win.fail("the album has no '+N' number");
                const small = Kirigami.Theme.smallFont.pointSize;
                if (small > 0 && more.fontInfo.pointSize < small - 0.5) return win.fail("the '+N' is smaller than the small font: " + more.fontInfo.pointSize + " < " + small);
                const a = more.mapToItem(null, 0, 0);
                const r = ring.mapToItem(null, 0, 0);
                if (a.x < r.x + ring.width && r.x < a.x + more.width && a.y < r.y + ring.height && r.y < a.y + more.height) return win.fail("the '+N' number is over its ring");
                console.warn("plus: number " + more.width + "x" + more.height + " at " + more.fontInfo.pointSize + " pt, ring " + ring.width);
            }
            win.target = t.id;
            // (Decision 7, a number for the report: the items of the row, rings included.)
            console.warn("objects: " + win.theCase + " row has " + win.all(win.view.delegateFor(t.id), c => true, []).length + " items");
            win.viewersBefore = win.viewersOpened;
            if (win.theCase === "menu") {
                if (!win.view.menuFor(t.id)) return win.fail("the menu did not open");
                return win.next("menu");
            }
            win.clickWindow(t.x, t.y);
            // (U7: the X was pressed and waits for its answer: dimmed, and the disc of a file no
            // longer a button; the click returns before the core can answer.)
            if (win.theCase === "xtile" || win.theCase === "plus" || win.lone) {
                const x = win.all(t.item, c => String(c.source) === "window-close", [])[0];
                if (!x) return win.fail("the X has no icon after the click");
                if (!(x.opacity < 1)) return win.fail("the X is not dimmed while the answer is awaited: " + x.opacity);
                if (win.lone && t.item.still !== true) return win.fail("the disc is still a button after the click");
                if (win.theCase === "xtile" && t.item.parent.cancelAsked !== true) return win.fail("the picture did not remember the press");
            }
            return win.next(win.theCase === "late" || win.theCase === "latenote" ? "note" : "after");
        }
        case "note": {
            // The stop came too late: the message went out and the composer says so, with an
            // action that rides on this note.
            if (App.composer.notice !== "Already sent") return win.later("the note 'Already sent'");
            const a = win.noteAction();
            if (a === null) return win.later("the note's action on screen");
            const at = a.mapToItem(null, a.width / 2, a.height / 2);
            const now = Math.round(at.x) + "," + Math.round(at.y);
            win.still = now === win.seen ? win.still + 1 : 0;
            win.seen = now;
            if (win.still < 4) return win.later("the action laid out");
            if (!win.hasRow(win.target)) return win.fail("the message is gone after the too-late stop");
            if (App.noticeMessage() !== win.target) return win.fail("the note names message " + App.noticeMessage() + ", not " + win.target);
            if (win.theCase === "latenote") {
                // (U1: a notice the composer sets itself, a dropped link, takes the place of the note.)
                App.composer.addFiles(["https://example.com/picture.png"]);
                return win.next("other");
            }
            win.clickWindow(at.x, at.y);
            return win.next("ask");
        }
        case "ask": {
            // (U2: the note's action asks the question of the message menu, for that row; the
            // note goes once the question is up and nothing is deleted yet.)
            if (win.inQuestion("") === null) return win.later("the delete question (the note is " + JSON.stringify(App.composer.notice) + ")");
            const forAll = win.inQuestion("Delete for Everyone");
            const cancel = win.inQuestion("Cancel");
            if (forAll === null || cancel === null) return win.later("the question's buttons");
            const at = cancel.mapToItem(null, cancel.width / 2, cancel.height / 2);
            const now = Math.round(at.x) + "," + Math.round(at.y);
            win.still = now === win.seen ? win.still + 1 : 0;
            win.seen = now;
            if (win.still < 4) return win.later("the question laid out");
            if (App.composer.notice !== "") return win.fail("the note stays beside the question: " + App.composer.notice);
            if (!win.hasRow(win.target)) return win.fail("the message went at once");
            win.clickWindow(at.x, at.y);
            return win.next("cancelled");
        }
        case "cancelled": {
            if (win.inQuestion("") !== null) return win.later("the question closes");
            if (!win.hasRow(win.target)) return win.fail("Cancel took the message away");
            win.rest += 1;
            if (win.rest < 15) return;
            Qt.exit(0);
            return;
        }
        case "other": {
            // The composer's own notice is up: the action is not on it (no message to delete
            // for it), though the source still holds its note.
            if (App.composer.notice === "" || App.composer.notice === "Already sent") return win.later("the composer's own notice");
            if (App.noticeMessage() !== 0) return win.fail("the action rides on another notice: message " + App.noticeMessage());
            if (win.noteAction() !== null) return win.later("the action goes");
            if (!win.hasRow(win.target)) return win.fail("the message went");
            win.rest += 1;
            if (win.rest < 15) return;
            Qt.exit(0);
            return;
        }
        case "menu": {
            const m = win.view.messageMenu;
            if (!m.visible) return win.later("the row's menu");
            if (!m.row || m.row.messageId !== win.target) return win.fail("the menu is about another message");
            let entry = null;
            for (let i = 0; i < m.count; ++i) {
                const item = m.itemAt(i);
                if (item && item.objectName === "menuCancel") entry = item;
            }
            if (!entry || !entry.visible || entry.text !== "Cancel Sending") return win.fail("the menu has no 'Cancel Sending' while the album goes up");
            entry.triggered();
            m.close();
            return win.next("after");
        }
        case "after": {
            if (win.theCase === "beside") {
                // The viewer opens, once; nothing is cancelled (the Rust side counts the commands).
                if (win.viewersOpened === win.viewersBefore) return win.later("a click beside the ring opens the viewer");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("one click opened " + (win.viewersOpened - win.viewersBefore) + " viewers");
                if (!win.hasRow(win.target)) return win.fail("the message went");
                win.rest += 1;
                if (win.rest < 15) return;
                return Qt.exit(0);
            }
            // The X: the viewer stays shut, the message is taken back (the fake answered Stopping).
            if (win.viewersOpened !== win.viewersBefore) return win.fail("a click on the X opened a viewer");
            if (win.hasRow(win.target)) return win.later("the message goes");
            Qt.exit(0);
        }
        }
    }
}
"##;

/// One case in this process (a child of the runner): returns its exit code.
fn run_upload_case(case: &str) -> i32 {
    let upload = match case {
        "file" => FixtureUpload::File,
        "late" | "latenote" => FixtureUpload::Late,
        _ => FixtureUpload::Album,
    };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            sends: Some(FixtureSends::Uploading(upload)),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let chat = handle
        .fixture()
        .expect("fixture mode")
        .uploads_conversation
        .expect("the chat of the uploads");
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = UPLOAD_SCRIPT
        .replace("THE_CHAT", &chat.0.to_string())
        .replace("THE_CASE", case);
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/wait_ui_uploads.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        return code;
    }
    // What the fake network was told: the X (or the menu entry) is one abort, a click beside it
    // none.
    let aborts = handle
        .fixture_commands()
        .iter()
        .filter(|c| **c == "AbortTransmit")
        .count();
    let want = usize::from(case != "beside");
    std::thread::sleep(Duration::from_millis(200));
    let transmits = handle
        .fixture_commands()
        .iter()
        .filter(|c| **c == "Transmit")
        .count();
    app.shutdown();
    if aborts != want {
        eprintln!("FAIL [{case}]: the network was told AbortTransmit {aborts} times, not {want}");
        return 1;
    }
    // The too-late message went out once and nothing was sent after it: no delete.
    if case.starts_with("late") && transmits != 1 {
        eprintln!("FAIL [{case}]: the network was told Transmit {transmits} times, not 1");
        return 1;
    }
    0
}

/// The old case: the line under a slow send.
fn run_line_case() -> i32 {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            // Every send takes two minutes and says it sets up encryption with a count no
            // bubble has room for.
            sends: Some(FixtureSends::Slow(Some(SendPhase::SettingUpKeys {
                done: 3,
                of: u32::MAX,
            }))),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let chat = handle
        .fixture()
        .expect("fixture mode")
        .demo_conversation
        .expect("the demo chat");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT.replace("THE_CHAT", &chat.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/wait_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code == 0 {
        std::thread::sleep(Duration::from_millis(200));
        app.shutdown();
    }
    code
}

fn main() {
    support::answer_list("wait_ui");
    if !cfg!(debug_assertions) {
        // The UI clock runs with the fixture's only in a debug build.
        println!(
            "test the line under a message on its way ... ok (skipped in a release build)\n\ntest result: ok. 1 passed; 0 failed"
        );
        return;
    }
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // The UI's "now" runs with the fixture's, so the seconds of the line pass.
    // SAFETY: as above.
    unsafe { std::env::set_var("KOURIER_DEV_FIXTURE_CLOCK", "run") };
    // A child: one case, then exit with its code (a window and a core of its own: the fake's
    // uploads are per conversation and one send each).
    if let Ok(case) = std::env::var("KOURIER_WAIT_UI_CASE") {
        let code = if case == "line" {
            run_line_case()
        } else {
            run_upload_case(&case)
        };
        std::process::exit(code);
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_WAIT_UI_ONLY").unwrap_or_default();
    let cases: Vec<&str> = [
        "line", "xtile", "beside", "plus", "file", "menu", "late", "latenote",
    ]
    .into_iter()
    .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
    .collect();
    let mut failed = 0;
    for case in &cases {
        let out = Command::new(&exe)
            .env("KOURIER_WAIT_UI_CASE", case)
            .output()
            .expect("run a case");
        let said = String::from_utf8_lossy(&out.stderr);
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{said}");
        // No script error or failed assignment of Kirigami's or ours on the way.
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        if out.status.success() && errors.is_empty() {
            println!("test upload and wait: {case} ... ok");
        } else {
            println!("test upload and wait: {case} ... FAILED ({})", out.status);
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
