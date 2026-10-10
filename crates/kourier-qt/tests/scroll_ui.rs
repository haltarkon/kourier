//! Who moves the timeline decides whether it follows the newest message (architecture §13,
//! phase M), with real input events in the app's own main window, in the style the app
//! runs with (`org.kde.desktop`, widget style and settings through the KDE platform theme),
//! over the real core on the fake network (offscreen, `harness = false`).
//!
//! The other UI tests run with the default Qt Quick Controls style, where the wheel goes to
//! the list itself and the list reports a movement. With the desktop style the ScrollView
//! holds a `Kirigami.WheelHandler` that takes the wheel and touchpad events and writes or
//! animates `contentY`: the list reports no movement at all. The test asserts that this is
//! the path its wheel events take.
//!
//! One session of a user, in a chat where a message arrives every 0.6 s and disappears 0.3 s
//! later (`FixtureOptions::arrivals`), so rows come and go at the newest end all the time:
//!
//! - A long message was sent (three windows tall) and is the newest one; the view rests at
//!   its end and follows.
//! - The wheel goes up through it in steps smaller than the window, as single turns of
//!   several notches and as the many small steps of a touchpad, and two steps past its first
//!   line. After each step the view stays where the wheel left it while messages arrive and
//!   disappear: it never returns to the newest message, the row being read does not move,
//!   "Jump to Latest" shows, and every line of the long message was in view on the way.
//!   Delegates were made above on the way.
//! - A row changes (a reaction makes the long message taller) and the window is made smaller
//!   and larger: what is being read stays where it is.
//! - The scroll bar: a click on its track (a page per click in one run, to the place of the
//!   click in the other) leaves the view where it went; the handle dragged to the end makes
//!   it follow again, also while the button is still down; dragged up again it stops.
//! - The wheel back down to the newest message: the view follows again and shows what
//!   arrives, also while the window gets smaller and larger (as when the composer grows by
//!   a line: a list at the very end of its content does not keep its bottom edge then).
//! - The keyboard: rows made current with Up until the view has to move; it stays there,
//!   also after Escape.
//! - The view's own moves: "Jump to Latest", another chat and back. In the other chat the
//!   wheel goes up to the oldest message while pages of older rows load above the view.
//! - A finger: the list dragged and flicked away (the list's own movement) stays; dragged
//!   back to the newest message it follows again.
//!
//! The session runs twice, each in a process of its own with its own `kdeglobals`: with the
//! handler's scroll animation and clicks on the track that go to the place of the click
//! (Plasma's defaults), and without the animation and with clicks that page.

use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QQmlApplicationEngine, QUrl};
use cxx_qt_lib_extras::QApplication;
use kourier_app::{App, Arrivals, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The app's own main window is the root (narrow: one page at a time).
MainWindow {
    id: win

    width: 520
    height: 620
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    // The style's wheel handler, how many wheel events it took and how many this test sent.
    property var handler: null
    property int handled: 0
    property int sent: 0
    // Movements the list reported itself (a drag, a flick).
    property int movements: 0
    // The long message, and the messages before it that had no delegate while the view was
    // at the newest message.
    property real long: 0
    property var unbuilt: []
    // Arrivals: the highest number seen, how many disappeared, which are there now.
    property int arrived: 0
    property int gone: 0
    property var alive: ({})
    // The row a check holds on to (the topmost one in view), where its top was, and for how
    // many ticks it has been there.
    property real mark: 0
    property real markAt: 0
    property int still: 0
    // A watch: the counts when it began, and ticks in a row the view was away from the end.
    property int fromArrived: 0
    property int fromGone: 0
    property int away: 0
    property int off: 0
    // The part of the long message's text that was in view so far (in its own coordinates).
    property real readFrom: 0
    property real readTo: 0
    property real textHeight: 0
    property int ups: 0
    property bool sawTop: false
    property int beyond: 0
    property real before: 0
    property real dragY: 0
    property var touch: null
    property bool finger: false
    property int strokes: 0
    property int climbs: 0
    // Times the timeline's model replaced its rows.
    property int resets: 0
    property int tries: 0

    readonly property var view: win.chatPage ? win.chatPage.timeline : null
    readonly property var list: win.view ? win.view.rowList : null
    readonly property var bar: win.view ? win.view.QQC2.ScrollBar.vertical : null

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg + " (" + win.facts() + ")");
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        // A liveness limit, not a time budget.
        if (win.waits > 1500) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.still = 0;
        win.away = 0;
        win.off = 0;
        win.tries = 0;
    }

    // What a failure is told with.
    function facts(): string {
        if (!win.list) {
            return "no timeline";
        }
        const l = win.list;
        return "following " + win.view.followNewest + ", at the newest message " + win.view.atBottom + ", " + Math.round(win.view.fromBottom) + " px from it, wheel events sent " + win.sent + " and taken by the style's handler " + win.handled + ", movements the list reported " + win.movements + ", moving " + l.moving + ", arrivals " + win.arrived + ", disappeared " + win.gone + ", rows " + l.count;
    }

    // The first item below `root` that `pred` accepts.
    function find(root: Item, pred: var): Item {
        if (!root) {
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

    function textOf(r: var): string {
        for (const b of r.blocks) {
            if (b.type === "text") {
                return b.plain;
            }
        }
        return "";
    }

    // The id of the message that says `text` (or, with `more`, starts with it); 0 when there
    // is none.
    function messageWith(text: string, more: bool): real {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r || r.kind !== "message") {
                continue;
            }
            const said = win.textOf(r);
            if (said === text || (more && said.indexOf(text) === 0)) {
                return r.messageId;
            }
        }
        return 0;
    }

    // Counts the arrivals and the ones that disappeared since the last look.
    function look(): void {
        const now = {};
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r || r.kind !== "message") {
                continue;
            }
            const text = win.textOf(r);
            if (text.indexOf("Arrival ") === 0) {
                now[r.messageId] = true;
                win.arrived = Math.max(win.arrived, parseInt(text.substring(8)));
            }
        }
        for (const id in win.alive) {
            if (!now[id]) {
                win.gone += 1;
            }
        }
        win.alive = now;
    }

    // Where the top of a message's row is in the view; NaN while it has no delegate.
    function topOf(id: real): real {
        const d = win.view.delegateFor(id);
        return d ? d.mapToItem(win.view, 0, 0).y : NaN;
    }

    // Holds on to the topmost row in view that is no arrival (those disappear).
    function pick(): bool {
        for (let i = win.list.count - 1; i >= 0; --i) {
            const item = win.list.itemAtIndex(i);
            if (!item || item.messageId === undefined || !(item instanceof MessageDelegate) || win.alive[item.messageId]) {
                continue;
            }
            const top = item.mapToItem(win.view, 0, 0).y;
            if (top + item.height > 1 && top < win.view.height - 1) {
                win.mark = item.messageId;
                win.markAt = top;
                return true;
            }
        }
        return false;
    }

    // The view has not moved for `ticks` ticks: holds on to the topmost row meanwhile.
    function rests(ticks: int): bool {
        if (win.list.moving) {
            win.still = 0;
            return false;
        }
        const at = win.still > 0 ? win.topOf(win.mark) : NaN;
        if (isNaN(at) || Math.abs(at - win.markAt) > 0.01) {
            win.still = win.pick() ? 1 : 0;
            return false;
        }
        win.still += 1;
        return win.still > ticks;
    }

    function begin(): void {
        win.fromArrived = win.arrived;
        win.fromGone = win.gone;
        win.away = 0;
    }

    // `n` messages arrived and `n` disappeared since `begin`.
    function passed(n: int): bool {
        return win.arrived >= win.fromArrived + n && win.gone >= win.fromGone + n;
    }

    // One tick of a view the user has left away from the newest message, while messages
    // arrive and disappear at the newest end: it stays where it is.
    function staysAway(what: string): bool {
        if (win.view.atBottom) {
            win.fail(what + ": the view went back to the newest message");
            return false;
        }
        const at = win.topOf(win.mark);
        if (isNaN(at)) {
            win.fail(what + ": the row that was being read is gone");
            return false;
        }
        // (A row that changes its height a frame after its content is put back in that
        // frame or the next: one tick may see it on its way.)
        win.off = Math.abs(at - win.markAt) > 1 ? win.off + 1 : 0;
        if (win.off > 1) {
            win.fail(what + ": what was being read moved by " + (at - win.markAt) + " px");
            return false;
        }
        if (win.view.followNewest) {
            win.fail(what + ": the view still follows the newest message");
            return false;
        }
        return true;
    }

    // One tick of a view at the newest message while messages arrive and disappear there:
    // it follows them. (A row that settles its height is followed within a frame or two.)
    function staysNewest(what: string): bool {
        if (!win.view.followNewest) {
            win.fail(what + ": the view does not follow the newest message");
            return false;
        }
        win.away = win.view.atBottom ? 0 : win.away + 1;
        if (win.away > 3) {
            win.fail(what + ": the view fell behind the newest message");
            return false;
        }
        return true;
    }

    function jumpButton(): Item {
        return win.find(win.view, c => c.text === "Jump to Latest");
    }

    function wheel(delta: int): void {
        win.sent += 1;
        if (!win.ev.mouseWheel(win.list, win.list.width / 2, win.list.height / 2, Qt.NoButton, Qt.NoModifier, 0, delta, -1)) {
            win.fail("the wheel event was not delivered");
        }
    }

    // The text of the long message and the part of it that is in view now.
    function read(): bool {
        const text = win.find(win.view.delegateFor(win.long), c => c.selectByMouse === true && c.visible && c.height > 0);
        if (!text) {
            return false;
        }
        const top = text.mapToItem(win.view, 0, 0).y;
        const from = Math.max(0, -top);
        const to = Math.min(text.height, win.view.height - top);
        if (to <= from) {
            return true;
        }
        if (win.textHeight === 0) {
            win.textHeight = text.height;
            win.readFrom = from;
            win.readTo = to;
            return true;
        }
        // Each step shows lines that join what was read before: none is passed over.
        if (to < win.readFrom || from > win.readTo) {
            win.fail("a part of the long message was passed over: read " + win.readFrom + " to " + win.readTo + ", now " + from + " to " + to);
            return false;
        }
        win.readFrom = Math.min(win.readFrom, from);
        win.readTo = Math.max(win.readTo, to);
        return true;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property var blocks
            required property var reactions
        }
    }

    Connections {
        target: win.handler
        function onWheel(wheel: var): void {
            win.handled += 1;
        }
    }
    Connections {
        target: win.list
        function onMovementStarted(): void {
            win.movements += 1;
        }
    }
    Connections {
        target: App.timeline
        function onModelReset(): void {
            win.resets += 1;
        }
    }

    // An input event can spin the event loop while it is delivered: the steps never nest.
    property bool busy: false
    Timer {
        interval: 30
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
        if (win.list && win.step !== "start") {
            win.look();
        }
        switch (win.step) {
        case "start": {
            if (!win.started || !App.loaded || !App.hasAccounts) return win.later("the start page");
            try {
                win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "scroll_ui_events.qml");
            } catch (e) {
                return win.fail("the QtTest QML module is needed for input events (package qml6-module-qttest): " + e);
            }
            win.openConversation(CHAT_ID);
            win.requestActivate();
            return win.next("open");
        }
        case "open": {
            if (win.chatPage === null || App.timeline.conversationId !== CHAT_ID || App.timeline.loading || rows.count < 3) return win.later("the chat");
            if (!win.view.atBottom) return win.later("a chat opens at its newest message");
            // The wheel path of the desktop: the org.kde.desktop style's ScrollView holds a
            // Kirigami.WheelHandler for its list, which takes the wheel events.
            const data = win.view.data;
            for (let i = 0; i < data.length; ++i) {
                if (data[i] instanceof Kirigami.WheelHandler) {
                    win.handler = data[i];
                }
            }
            if (win.handler === null) return win.fail("the timeline's ScrollView has no Kirigami.WheelHandler: this is not the org.kde.desktop style");
            if (win.handler.target !== win.list) return win.fail("the wheel handler is not the timeline's");
            if (Kirigami.Settings.smoothScroll !== SMOOTH_SCROLL) return win.fail("smooth scrolling is " + Kirigami.Settings.smoothScroll);
            if (SMOOTH_SCROLL && Kirigami.Units.longDuration <= 0) return win.fail("no time for the scroll animation");
            // A chat with a past, then the long message: ninety short numbered lines, three
            // times as tall as the window with a few hundred characters.
            for (let i = 1; i <= 30; ++i) {
                App.composer.send(CHAT_ID, "Earlier message " + i);
            }
            let text = "";
            for (let i = 1; i <= 90; ++i) {
                text += (i > 1 ? "\n" : "") + "L" + i;
            }
            App.composer.send(CHAT_ID, text);
            return win.next("sent");
        }
        case "sent": {
            win.long = win.messageWith("L1\nL2\n", true);
            if (win.long <= 0 || win.messageWith("Earlier message 1", false) <= 0 || win.messageWith("Earlier message 30", false) <= 0) return win.later("the messages that were sent");
            const d = win.view.delegateFor(win.long);
            if (!d) return win.later("the long message has a row");
            // The scene: the long message is the newest one (no arrival is there in this
            // moment), the view rests at its end and follows.
            if (rows.objectAt(0).messageId !== win.long || !win.view.atBottom || !win.rests(2)) return win.later("the long message is the newest one and the view rests at it");
            if (d.height <= win.view.height * 2) return win.fail("the long message is not taller than the window: " + d.height);
            if (!win.view.followNewest) return win.fail("a view at the newest message follows it");
            const button = win.jumpButton();
            if (!button) return win.fail("the Jump to Latest button exists");
            if (button.visible) return win.fail("Jump to Latest shows at the newest message");
            if (!win.read()) return win.later("the text of the long message");
            if (win.readTo < win.textHeight - 1) return win.fail("the last line of the long message is not in view: " + win.readTo + " of " + win.textHeight);
            // The rows far above have no delegates yet: scrolling up makes them.
            const none = [];
            for (let i = 1; i <= 30; ++i) {
                const id = win.messageWith("Earlier message " + i, false);
                if (id > 0 && win.view.delegateFor(id) === null) {
                    none.push(id);
                }
            }
            if (none.length === 0) return win.fail("every row has a delegate already");
            win.unbuilt = none;
            return win.next("up");
        }

        // ---- the wheel, up through the long message and past it ----
        case "up": {
            if (win.ups >= 30) return win.fail("the top of the long message is out of reach");
            win.before = win.topOf(win.long);
            if (isNaN(win.before)) return win.fail("the long message has no row");
            // Four notches a step, less than the window is tall: as one turn of a fast
            // wheel, or as the many small steps of a touchpad or a fine wheel.
            if (win.ups % 2 === 0) {
                win.wheel(480);
            } else {
                for (let i = 0; i < 8; ++i) {
                    win.wheel(60);
                }
            }
            win.ups += 1;
            return win.next("climbed");
        }
        case "climbed": {
            if (!win.rests(5)) return win.later("the view comes to rest after the wheel");
            if (win.handled !== win.sent) return win.fail("the style's wheel handler did not take every wheel event");
            if (win.view.atBottom) return win.fail("after the wheel: the view went back to the newest message");
            const moved = win.topOf(win.long) - win.before;
            // (With the animation a message can arrive or disappear while the view is on
            // its way: the rows then move once by its height.)
            if (SMOOTH_SCROLL ? (moved < 100 || moved > 380) : (moved < 239 || moved > 241)) return win.fail("four notches of the wheel moved the view by " + moved + " px");
            if (!win.staysAway("after the wheel")) return;
            const button = win.jumpButton();
            if (!button.visible) return win.fail("no Jump to Latest away from the newest message");
            if (!win.read()) return win.fail("the long message has no row");
            win.begin();
            return win.next("reading");
        }
        case "reading": {
            // Messages arrive at the newest end and disappear again while the user reads.
            if (!win.staysAway("while messages arrive")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            if (win.topOf(win.long) >= 0) {
                win.sawTop = true;
            }
            if (win.sawTop) {
                win.beyond += 1;
            }
            // Past the first line, two steps on into the messages before it.
            return win.next(win.beyond > 2 ? "above" : "up");
        }
        case "above": {
            if (win.readFrom > 0 || win.readTo < win.textHeight - 1) return win.fail("the long message could not be read from its first line to its last: " + win.readFrom + " to " + win.readTo + " of " + win.textHeight);
            if (win.movements !== 0) return win.fail("the wheel made the list report a movement: the events did not go through the style's handler alone");
            if (!win.unbuilt.some(id => win.view.delegateFor(id) !== null)) return win.fail("no delegate was made above while scrolling up");
            // A row changes: a reaction makes the long message taller.
            App.react(win.long, "👍", false);
            win.begin();
            return win.next("reacted");
        }
        case "reacted": {
            if (!win.staysAway("while a row changes")) return;
            let shown = false;
            for (let i = 0; i < rows.count; ++i) {
                const r = rows.objectAt(i);
                shown = shown || (r && r.messageId === win.long && r.reactions.some(c => c.emoji === "👍"));
            }
            if (!shown) return win.later("the reaction shows");
            if (!win.passed(1) || win.waits < 10) return win.later("a message arrives and disappears");
            // ---- the window is resized ----
            win.height = win.height - 110;
            return win.next("smaller");
        }
        case "smaller": {
            if (!win.staysAway("in a smaller window")) return;
            if (win.waits < 12) return win.later("the smaller window settles");
            win.height = win.height + 110;
            return win.next("larger");
        }
        case "larger": {
            if (!win.staysAway("in a larger window")) return;
            if (win.waits < 12) return win.later("the larger window settles");
            return win.next("track");
        }

        // ---- the scroll bar: a click on its track, its handle dragged ----
        case "track": {
            if (!win.bar || !win.bar.visible || !win.bar.interactive) return win.fail("the timeline has no scroll bar to use");
            // The pointer over the track above the handle, then the button goes down there.
            if (win.waits === 0) {
                if (win.bar.position * win.bar.height < 40) return win.fail("no track above the handle");
                win.ev.mouseMove(win.bar, win.bar.width / 2, 12, -1, Qt.NoButton, Qt.NoModifier);
                return win.later("the pointer is over the track");
            }
            win.before = win.list.contentY;
            win.ev.mousePress(win.bar, win.bar.width / 2, 12, Qt.LeftButton, Qt.NoModifier, -1);
            return win.next("trackdown");
        }
        case "trackdown": {
            if (win.list.contentY === win.before) return win.later("a click on the track moves the view");
            // A page per click with the one setting, to the place of the click with the other.
            if (win.bar.pressed === TRACK_PAGES) return win.fail("the click on the track was not taken the way this run expects");
            if (win.waits < 4) return win.later("the button is held");
            win.ev.mouseRelease(win.bar, win.bar.width / 2, 12, Qt.LeftButton, Qt.NoModifier, -1);
            return win.next("trackup");
        }
        case "trackup": {
            if (!win.rests(5)) return win.later("the view comes to rest after the click on the track");
            if (!win.staysAway("after a click on the track")) return;
            win.begin();
            return win.next("trackread");
        }
        case "trackread": {
            if (!win.staysAway("after a click on the track, while messages arrive")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            // The handle, dragged to the end of the bar.
            win.dragY = (win.bar.position + win.bar.size / 2) * win.bar.height;
            win.ev.mouseMove(win.bar, win.bar.width / 2, win.dragY, -1, Qt.NoButton, Qt.NoModifier);
            win.ev.mousePress(win.bar, win.bar.width / 2, win.dragY, Qt.LeftButton, Qt.NoModifier, -1);
            return win.next("dragdown");
        }
        case "dragdown": {
            if (!win.bar.pressed) return win.fail("the handle is not held");
            if (win.dragY < win.bar.height + 30) {
                win.dragY += 30;
                win.ev.mouseMove(win.bar, win.bar.width / 2, win.dragY, -1, Qt.LeftButton, Qt.NoModifier);
                return;
            }
            if (!win.view.atBottom || !win.view.followNewest) {
                // (The list corrects its guess of the content's height as rows are made:
                // the pointer keeps moving below the bar until the end is the end.)
                win.tries += 1;
                win.ev.mouseMove(win.bar, win.bar.width / 2, win.dragY + win.tries % 2, -1, Qt.LeftButton, Qt.NoModifier);
                return win.later("the handle at the end of the bar: the view is at the newest message and follows");
            }
            win.begin();
            return win.next("dragheld");
        }
        case "dragheld": {
            // The button still down: the view follows what arrives.
            if (!win.bar.pressed) return win.fail("the handle is not held");
            if (!win.staysNewest("with the handle held at the end of the bar")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            return win.next("dragup");
        }
        case "dragup": {
            if (win.view.atBottom) {
                if (win.tries > 40) return win.fail("the handle does not move the view up");
                win.tries += 1;
                win.dragY -= 6;
                win.ev.mouseMove(win.bar, win.bar.width / 2, win.dragY, -1, Qt.LeftButton, Qt.NoModifier);
                return;
            }
            win.ev.mouseRelease(win.bar, win.bar.width / 2, win.dragY, Qt.LeftButton, Qt.NoModifier, -1);
            return win.next("dragged");
        }
        case "dragged": {
            if (!win.rests(5)) return win.later("the view comes to rest after the drag");
            if (!win.staysAway("after the handle was dragged up")) return;
            win.begin();
            return win.next("dragread");
        }
        case "dragread": {
            if (!win.staysAway("after the handle was dragged up, while messages arrive")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            return win.next("down");
        }

        // ---- the wheel, back down to the newest message ----
        case "down": {
            if (!win.view.atBottom) {
                if (win.tries > 200) return win.fail("the wheel does not reach the newest message");
                win.tries += 1;
                if (win.tries % 4 === 1) {
                    win.wheel(-720);
                }
                return;
            }
            return win.next("landed");
        }
        case "landed": {
            if (!win.view.atBottom) return win.next("down");
            if (!win.rests(5)) return win.later("the view comes to rest at the newest message");
            if (!win.view.followNewest) return win.later("a view left at the newest message follows it again");
            if (win.jumpButton().visible) return win.later("Jump to Latest goes at the newest message");
            win.begin();
            return win.next("following");
        }
        case "following": {
            if (!win.staysNewest("back at the newest message")) return;
            if (!win.passed(2) || win.waits < 6) return win.later("messages arrive and disappear");
            // What arrived last is in view.
            const newest = win.list.itemAtIndex(0);
            if (!newest) return win.later("the newest row has a delegate");
            const end = newest.mapToItem(win.view, 0, 0).y + newest.height;
            if (end > win.view.height + 1 || end < win.view.height / 2) return win.later("the newest row ends at the bottom of the view: " + end);
            // The view gets smaller at the newest message (as when the composer grows by a
            // line), and larger again: it stays there and follows.
            win.height = win.height - 110;
            return win.next("lower");
        }
        case "lower": {
            if (!win.staysNewest("in a smaller window")) return;
            if (win.waits < 12) return win.later("the smaller window settles");
            win.height = win.height + 110;
            return win.next("higher");
        }
        case "higher": {
            if (!win.staysNewest("in a larger window")) return;
            if (win.waits < 12) return win.later("the larger window settles");
            win.list.forceActiveFocus();
            return win.next("keys");
        }

        // ---- the keyboard: rows made current until the view has to move ----
        case "keys": {
            if (!win.list.activeFocus) return win.later("the timeline takes the keyboard focus");
            if (win.view.atBottom) {
                if (win.tries > 60) return win.fail("Up never moved the view");
                win.tries += 1;
                win.ev.keyClick(Qt.Key_Up, Qt.NoModifier, -1);
                return;
            }
            return win.next("keyed");
        }
        case "keyed": {
            if (!win.rests(8)) return win.later("the view comes to rest at the current row");
            if (win.list.currentIndex < 0) return win.fail("no row is current");
            if (!win.staysAway("after the keyboard moved the view")) return;
            win.begin();
            return win.next("keyread");
        }
        case "keyread": {
            if (!win.staysAway("at a row made current, while messages arrive")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            // Escape: back to writing. The view stays.
            win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            win.begin();
            return win.next("escaped");
        }
        case "escaped": {
            if (win.list.currentIndex >= 0) return win.later("no row is current without the keyboard");
            if (!win.staysAway("after Escape")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            const button = win.jumpButton();
            if (!button.visible) return win.fail("no Jump to Latest away from the newest message");
            win.ev.mouseClick(button, button.width / 2, button.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
            return win.next("jumped");
        }

        // ---- the view's own moves: Jump to Latest, another chat and back ----
        case "jumped": {
            if (!win.view.atBottom || !win.view.followNewest) return win.later("Jump to Latest goes to the newest message and follows it");
            if (win.tries === 0) {
                win.tries = 1;
                win.begin();
            }
            if (!win.staysNewest("after Jump to Latest")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            win.openConversation(OTHER_ID);
            return win.next("elsewhere");
        }
        case "elsewhere": {
            if (App.timeline.conversationId !== OTHER_ID || App.timeline.loading || rows.count < 3) return win.later("the other chat");
            if (!win.view.atBottom || !win.view.followNewest) return win.later("another chat opens at its newest message");
            if (win.waits < 10) return win.later("the other chat settles");
            if (!App.timeline.hasOlder) return win.fail("the other chat has no older pages to load");
            return win.next("pages");
        }
        // Up to its oldest message: pages of older rows load above the view on the way.
        case "pages": {
            if (!App.timeline.hasOlder && win.view.atTop) return win.next("oldest");
            if (win.climbs > 80) return win.fail("the oldest message is out of reach");
            win.climbs += 1;
            win.wheel(2400);
            return win.next("paged");
        }
        case "paged": {
            if (!win.rests(4)) return win.later("the view comes to rest after the wheel");
            return win.next("pageread");
        }
        case "pageread": {
            if (!App.timeline.hasOlder && win.view.atTop) return win.next("oldest");
            if (!win.staysAway("while older pages load")) return;
            if (win.waits < 10) return win.later("an older page loads");
            return win.next("pages");
        }
        case "oldest": {
            if (!win.rests(5)) return win.later("the view comes to rest at the oldest message");
            if (!win.staysAway("at the oldest message")) return;
            win.openConversation(CHAT_ID);
            return win.next("back");
        }
        case "back": {
            if (App.timeline.conversationId !== CHAT_ID || App.timeline.loading || win.messageWith("L1\nL2\n", true) <= 0) return win.later("the chat again");
            if (!win.view.atBottom || !win.view.followNewest) return win.later("the chat opens at its newest message again");
            if (win.tries === 0) {
                win.tries = 1;
                win.begin();
            }
            if (!win.staysNewest("in the chat that was opened again")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            // ---- a finger: the list is dragged and flicked ----
            win.dragY = 160;
            win.touch = win.ev.touchEvent(win.list);
            win.touch.press(0, win.list, win.list.width / 2, win.dragY);
            win.touch.commit();
            return win.next("finger");
        }
        case "finger": {
            if (win.waits < 8) {
                win.dragY += 30;
                win.touch.move(0, win.list, win.list.width / 2, win.dragY);
                win.touch.commit();
                return win.later("the finger moves down");
            }
            if (!win.list.dragging) return win.fail("the finger does not drag the list");
            win.touch.release(0, win.list, win.list.width / 2, win.dragY);
            win.touch.commit();
            return win.next("flicked");
        }
        case "flicked": {
            if (win.list.moving || !win.rests(5)) return win.later("the list comes to rest after the flick");
            if (win.movements === 0) return win.fail("the list reported no movement for a drag");
            if (!win.staysAway("after a drag")) return;
            win.begin();
            return win.next("flickread");
        }
        case "flickread": {
            if (!win.staysAway("after a drag, while messages arrive")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            return win.next("fingerback");
        }
        case "fingerback": {
            // Dragged back a stroke at a time until the view is at the newest message.
            if (!win.finger) {
                if (win.list.moving) return win.later("the flick ends");
                if (win.view.atBottom) return win.next("fingerhome");
                if (win.strokes > 40) return win.fail("dragging does not reach the newest message");
                win.strokes += 1;
                win.dragY = 420;
                win.touch = win.ev.touchEvent(win.list);
                win.touch.press(0, win.list, win.list.width / 2, win.dragY);
                win.touch.commit();
                win.finger = true;
                return;
            }
            if (win.dragY > 100) {
                win.dragY -= 80;
                win.touch.move(0, win.list, win.list.width / 2, win.dragY);
                win.touch.commit();
                return;
            }
            win.touch.release(0, win.list, win.list.width / 2, win.dragY);
            win.touch.commit();
            win.finger = false;
            return;
        }
        case "fingerhome": {
            if (win.list.moving) return win.later("the list comes to rest");
            if (!win.view.atBottom) return win.next("fingerback");
            if (!win.view.followNewest) return win.later("a view dragged to the newest message follows it again");
            if (win.tries === 0) {
                win.tries = 1;
                win.begin();
            }
            if (!win.staysNewest("after a drag to the newest message")) return;
            if (!win.passed(1) || win.waits < 6) return win.later("a message arrives and disappears");
            Qt.exit(0);
            return;
        }
        }
    }

    Timer {
        interval: 240000
        running: true
        onTriggered: win.fail("timeout")
    }
}
"##;

/// The environment variable that names the run in a child process.
const RUN: &str = "KOURIER_SCROLL_UI_RUN";

/// The two runs: name, smooth scrolling, clicks on the scroll bar's track page.
const RUNS: [(&str, bool, bool); 2] = [("smooth", true, false), ("instant", false, true)];

fn main() -> ExitCode {
    match std::env::var(RUN) {
        Ok(name) => session(&name),
        Err(_) => both(),
    }
}

/// Runs the session once per entry of [`RUNS`], each in a child process with a
/// configuration directory of its own (the settings are read once per process).
fn both() -> ExitCode {
    let exe = std::env::current_exe().expect("this test's binary");
    let base = std::env::temp_dir().join(format!("kourier-scroll-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let children: Vec<_> = RUNS
        .iter()
        .map(|(name, smooth, pages)| {
            let config = base.join(name);
            std::fs::create_dir_all(&config).expect("a configuration directory");
            std::fs::write(
                config.join("kdeglobals"),
                format!(
                    "[KDE]\nSmoothScroll={smooth}\nScrollbarLeftClickNavigatesByPage={pages}\n"
                ),
            )
            .expect("kdeglobals");
            let child = Command::new(&exe)
                .env(RUN, name)
                .env("XDG_CONFIG_HOME", &config)
                // This test shows a window: always offscreen, whatever the session's
                // platform is, and in the style and with the platform theme of the desktop.
                .env("QT_QPA_PLATFORM", "offscreen")
                .env("QT_QPA_PLATFORMTHEME", "kde")
                .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
                .env("QT_SCALE_FACTOR", "1")
                // What the script says when it fails must reach the test's output.
                .env("QT_FORCE_STDERR_LOGGING", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start the run");
            (*name, child)
        })
        .collect();
    let mut failed = 0;
    for (name, child) in children {
        let out = child.wait_with_output().expect("the run ends");
        if out.status.success() {
            println!("test the timeline follows who moved it ({name}) ... ok");
        } else {
            failed += 1;
            println!(
                "test the timeline follows who moved it ({name}) ... FAILED ({})",
                out.status
            );
            // Why: the script's own line, then the end of what else was said.
            let said = String::from_utf8_lossy(&out.stderr);
            let (why, rest): (Vec<&str>, Vec<&str>) =
                said.lines().partition(|l| l.contains("FAIL"));
            for line in why {
                println!("{line}");
            }
            for line in rest.iter().rev().take(12).rev() {
                println!("    {line}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    let passed = RUNS.len() - failed;
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed");
        ExitCode::SUCCESS
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed");
        ExitCode::FAILURE
    }
}

/// One run of the session, in this process.
fn session(name: &str) -> ExitCode {
    let Some((_, smooth, pages)) = RUNS.iter().find(|(n, ..)| *n == name) else {
        eprintln!("FAIL: no run named {name}");
        return ExitCode::FAILURE;
    };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            // Rows come and go at the newest end for as long as the session can take.
            arrivals: Some(Arrivals {
                count: 500,
                every: Duration::from_millis(600),
                lifetime: Some(Duration::from_millis(300)),
            }),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let chat = fixture.timer_conversation.expect("the timer chat");
    // A chat with more messages than a page holds.
    let other = fixture.demo_conversation.expect("the first group");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    // QApplication, as the app: the org.kde.desktop style draws with a widget style.
    let mut qt = QApplication::new();

    let script = SCRIPT
        .replace("CHAT_ID", &chat.0.to_string())
        .replace("OTHER_ID", &other.0.to_string())
        .replace("SMOOTH_SCROLL", &smooth.to_string())
        .replace("TRACK_PAGES", &pages.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/scroll_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
