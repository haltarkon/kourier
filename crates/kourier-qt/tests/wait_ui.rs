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
//! Needs a debug build (`KOURIER_DEV_FIXTURE_CLOCK=run` lets the UI's clock run with the
//! fixture's); in a release test build it skips.

mod support;

use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, FixtureSends, Launch, SendPhase, StartOptions};
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
    if code != 0 {
        println!(
            "test the line under a message on its way ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    std::thread::sleep(Duration::from_millis(200));
    app.shutdown();
    println!(
        "test the line under a message on its way ... ok\n\ntest result: ok. 1 passed; 0 failed"
    );
}
