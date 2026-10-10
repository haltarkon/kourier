pragma ComponentBehavior: Bound

import QtQuick
import app.kourier

// The scripted session of a measuring build (cargo feature `perf`; `cargo xtask perf` reads
// the marks it writes, architecture §1.3). `MainWindow.qml` loads it when `App.perfScenario`
// is set:
//
//   startup            the conversation list is on screen: quit
//   gc                 the same, then run the JavaScript garbage collector and say what the
//                      heap holds afterwards (`perf.gc`): what was only waiting for a collection
//   notify             the conversation list is on screen: wait (a live message arrives)
//   session[:<idle>]   open `opens` conversations one after the other (`timeline.open`), flick
//                      through the image-heavy one (`scroll.frames`), stay idle for <idle>
//                      seconds with the window shown (`perf.idle`), then hide it to the tray;
//                      the shell releases it a minute later (`window.released`)
//
// While idle nothing here runs: no timer of this file is active then, except the single one
// that ends the idle time.
Item {
    id: driver

    // The main window and its chat page (the window creates that page when it first shows a
    // chat column, so it is read from the window each time).
    property var window: null
    readonly property var chatPage: driver.window ? driver.window.chatPage : null

    readonly property string scenario: App.perfScenario.split(":")[0]
    readonly property int idleSeconds: Number(App.perfScenario.split(":")[1] || "60")
    // Conversations opened for `timeline.open`.
    readonly property int opens: 20
    // How long the flicking lasts (ms) and how often a flick is given.
    readonly property int scrollMs: 8000
    readonly property int flickEveryMs: 350

    property string phase: "list"
    property real phaseSince: Date.now()
    property int opened: 0
    property real current: 0
    property real lastFlick: 0
    property real scrollStart: 0
    property int rowsBefore: 0
    property var samples: []

    readonly property var rowList: driver.chatPage ? driver.chatPage.timeline.rowList : null

    function enter(phase: string): void {
        driver.phase = phase;
        driver.phaseSince = Date.now();
    }

    function waited(): real {
        return Date.now() - driver.phaseSince;
    }

    // The list is on screen and its mark is written.
    function listShown(): bool {
        return driver.window !== null && driver.window.started && App.conversations.count > 0 && !App.traceFrame;
    }

    // The rows of `conversation` are on screen and the mark is written.
    function rowsShown(conversation: real): bool {
        return App.timeline.conversationId === conversation && App.timeline.count > 0 && !App.timeline.loading && !App.traceFrame;
    }

    // Delegates the conversation list has instantiated (a virtualized list makes only the
    // visible ones and a few more).
    function listDelegates(): int {
        const page = driver.window ? driver.window.pageStack.get(0) : null;
        const view = page ? page.flickable : null;
        return view && view.contentItem ? view.contentItem.children.length : -1;
    }

    function fail(step: int): void {
        App.traceMark("perf.failed", "step=" + step);
        Qt.exit(3);
    }

    // p in 0 … 1 of the sorted `values`.
    function percentile(values: var, p: real): real {
        if (values.length === 0) {
            return 0;
        }
        return values[Math.min(values.length - 1, Math.floor(p * values.length))];
    }

    function reportScroll(): void {
        const sorted = driver.samples.slice().sort((a, b) => a - b);
        const over = limit => sorted.filter(v => v > limit).length;
        const distance = driver.rowList ? Math.abs(driver.rowList.contentY - driver.scrollStart) : 0;
        App.traceMark("scroll.frames", "frames=" + sorted.length + " p50_ms=" + driver.percentile(sorted, 0.5).toFixed(2) + " p95_ms=" + driver.percentile(sorted, 0.95).toFixed(2) + " max_ms=" + driver.percentile(sorted, 1).toFixed(2) + " over16=" + over(16.7) + " over33=" + over(33.4) + " distance_px=" + Math.round(distance) + " rows_before=" + driver.rowsBefore + " rows_after=" + App.timeline.count);
    }

    function advance(): void {
        switch (driver.phase) {
        case "list":
            if (driver.listShown()) {
                driver.enter("settle");
            } else if (driver.waited() > 60000) {
                driver.fail(1);
            }
            break;
        case "settle":
            if (driver.waited() < 1500) {
                break;
            }
            App.traceMark("perf.list", "delegates=" + driver.listDelegates() + " rows=" + App.conversations.count);
            if (driver.scenario === "startup") {
                App.traceMark("perf.done", "");
                driver.enter("idle");
                Qt.quit();
            } else if (driver.scenario === "gc") {
                gc();
                driver.enter("gc");
            } else if (driver.scenario === "notify") {
                App.traceMark("perf.idle", "");
                driver.enter("idle");
            } else {
                driver.enter("open");
            }
            break;
        case "gc":
            // The collected objects are deleted by the event loop.
            if (driver.waited() >= 1000) {
                App.traceMark("perf.gc", "delegates=" + driver.listDelegates());
                App.traceMark("perf.done", "");
                driver.enter("idle");
                Qt.quit();
            }
            break;
        case "open":
            {
                if (driver.opened >= driver.opens) {
                    driver.enter("heavy");
                    break;
                }
                // Rows 0 and 1 are pinned and row 2 is the image-heavy group: start below
                // them and take every third chat (groups first, then direct chats).
                const id = App.conversationAt(3 + driver.opened * 3);
                if (id === 0) {
                    driver.enter("heavy");
                    break;
                }
                driver.current = id;
                driver.window.openConversation(id);
                driver.enter("open-wait");
                break;
            }
        case "open-wait":
            // A moment on each chat, as a user would give it (pictures decode meanwhile).
            if (driver.rowsShown(driver.current) && driver.waited() >= 400) {
                driver.opened += 1;
                driver.enter("open");
            } else if (driver.waited() > 15000) {
                driver.fail(2);
            }
            break;
        case "heavy":
            driver.current = App.conversationAt(2);
            if (driver.current === 0) {
                driver.fail(3);
                break;
            }
            driver.window.openConversation(driver.current);
            driver.enter("heavy-wait");
            break;
        case "heavy-wait":
            if (driver.rowsShown(driver.current) && driver.waited() >= 3000) {
                driver.samples = [];
                driver.scrollStart = driver.rowList.contentY;
                driver.rowsBefore = App.timeline.count;
                driver.lastFlick = 0;
                App.traceMark("scroll.begin", "group=" + (App.timeline.isGroup ? 1 : 0));
                driver.enter("scroll");
            } else if (driver.waited() > 20000) {
                driver.fail(4);
            }
            break;
        case "scroll":
            if (driver.waited() >= driver.scrollMs) {
                driver.reportScroll();
                driver.enter("rest");
                break;
            }
            if (Date.now() - driver.lastFlick >= driver.flickEveryMs) {
                // Towards older messages (the list runs bottom to top).
                driver.rowList.flick(0, 2500);
                driver.lastFlick = Date.now();
            }
            break;
        case "rest":
            // Paging and decoding finish, then the idle time starts.
            if (driver.waited() >= 3000) {
                App.traceMark("perf.idle", "seconds=" + driver.idleSeconds);
                driver.enter("idle");
                hide.start();
            }
            break;
        }
    }

    Timer {
        interval: 25
        repeat: true
        running: driver.scenario !== "" && driver.phase !== "idle"
        onTriggered: driver.advance()
    }

    // The time between two animation ticks while flicking: with the software renderer a frame
    // is rendered on this thread, so a slow frame delays the next tick.
    FrameAnimation {
        running: driver.phase === "scroll"
        onTriggered: driver.samples.push(frameTime * 1000)
    }

    // The idle time with the window shown is over (plus a margin for the last sample): hide to
    // the tray. The shell destroys the window, and this object with it, a minute later.
    Timer {
        id: hide
        interval: driver.idleSeconds * 1000 + 4000
        onTriggered: {
            App.traceMark("window.hidden", "");
            driver.window.hideToTray();
        }
    }
}
