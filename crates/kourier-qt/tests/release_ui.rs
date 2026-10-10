//! The window that hides in the tray is released and comes back (architecture §8.2 step 6),
//! through the app's own shell (`Main.qml`) and the real core over the fake network
//! (offscreen, `harness = false`):
//!
//! - hidden, the window is destroyed after the release delay; the memory is handed back only
//!   once the window object is really gone (`destroy()` merely schedules the deletion: a
//!   trim before that returns nothing, which is what kept 34 MB with the hidden process);
//! - with each release the core is asked, once and before the trim, to let go of the memory
//!   it only keeps for speed (`UiCommand::ReleaseMemory`: the page caches of its database
//!   connections);
//! - activating the app creates the window again, with the list and the chat column and no
//!   other page, and a chat opens in it as before;
//! - a wide window has its two columns from the first frame on: the list never fills the
//!   window first and jumps to its column afterwards;
//! - an activation that falls between the release and the trim gets a new window whose
//!   presence (focus, chat on screen) stands: the end of the old window's release does not
//!   reset it.
//!
//! Debug builds only: the release delay of a release build is a minute.

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

// The window is the root of a test script; the app's shell (which owns the real main window)
// lives inside it.
Window {
    id: test

    width: 200
    height: 120
    visible: true

    property string step: "start"
    property int waits: 0
    property var first: null
    property bool goneAtRelease: false
    // The core had been asked to free its caches when the heap was trimmed (first release).
    property bool coreAskedFirst: false
    // The next release is overtaken by an activation; whether that happened in time.
    property bool race: false
    property bool raced: false

    // The window comes back while the released one is still being deleted.
    function comeBack(): void {
        test.raced = shell.releases === 1;
        shell.activate("", CHAT);
    }

    Connections {
        target: shell
        function onWindowChanged(): void {
            if (test.race && shell.window === null) {
                test.race = false;
                // On the next turn of the event loop: long before the shell has seen the
                // old window object go.
                Qt.callLater(test.comeBack);
            }
        }
    }

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
        test.step = step;
        test.waits = 0;
    }

    Main {
        id: shell
        // The moment the memory is handed back: the released window must be gone by then.
        onReleasesChanged: {
            test.goneAtRelease = shell.releasing === null && shell.window === null;
            if (shell.releases === 1) {
                test.coreAskedFirst = shell.coreReleases === 1;
            }
        }
    }

    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start": {
                if (App.releaseDelayMs > 2000) return test.fail("the release delay is " + App.releaseDelayMs + " ms: not a debug build?");
                if (shell.window === null || !shell.window.started || !App.hasAccounts) return test.later("the main window");
                // A wide window: the list has its column from the first frame on (a page
                // with the empty state stands in the chat column until the chat page is
                // made, right after that frame).
                const w = shell.window;
                if (!w.wideScreen) return test.fail("the window's own size is a wide one: " + w.width);
                if (w.pageStack.depth !== 2) return test.fail("two columns from the start: " + w.pageStack.depth);
                if (w.pageStack.get(0).width > w.width / 2) return test.fail("the list fills the window: " + w.pageStack.get(0).width + " of " + w.width);
                if (w.standIn !== null || w.chatPage === null) return test.later("the chat page takes its column after the first frame");
                if (w.pageStack.get(1) !== w.chatPage) return test.fail("the chat page stands in the chat column");
                test.first = shell.window;
                shell.window.openConversation(CHAT);
                test.next("chat");
                return;
            }
            case "chat":
                if (App.timeline.conversationId !== CHAT || App.timeline.loading || App.timeline.count < 3) return test.later("a chat in the window");
                if (shell.window.chatPage === null) return test.fail("the chat page exists while a chat is shown");
                if (shell.releases !== 0) return test.fail("released before it was hidden");
                if (shell.coreReleases !== 0) return test.fail("the core was asked for its memory while the window is shown");
                // Closing to the tray: the window hides and the release delay starts.
                shell.window.hideToTray();
                if (shell.window.visible) return test.fail("the window hides");
                test.next("released");
                return;
            case "released":
                if (shell.releases === 0) return test.later("the hidden window is released");
                if (shell.releases !== 1) return test.fail("released " + shell.releases + " times");
                if (shell.window !== null || shell.releasing !== null) return test.fail("the window is gone after its release");
                // The order that returns the memory: deleted first, trimmed after.
                if (!test.goneAtRelease) return test.fail("the memory was handed back before the window was deleted");
                // The core was asked to let go of its caches, once, and before the trim
                // (which is what returns them to the system).
                if (shell.coreReleases !== 1) return test.fail("the core was asked for its memory " + shell.coreReleases + " times");
                if (!test.coreAskedFirst) return test.fail("the heap was trimmed before the core was asked to free its caches");
                // The models stay: the conversation is still open in the core's view.
                if (App.conversations.count < 15) return test.fail("the list's rows went with the window");
                // A notification click, the tray, a second launch: the window comes back.
                shell.activate("", 0);
                test.next("back");
                return;
            case "back": {
                const w = shell.window;
                if (w === null || !w.visible || !w.started) return test.later("the window again");
                const list = w.pageStack.get(0);
                if (!list || !list.rowsShown || list.flickable.count < 15) return test.later("its list");
                if (w.welcomePage !== null || w.settingsPage !== null || w.linkPage !== null) return test.fail("pages nobody asked for");
                if (w.pageStack.depth !== 2 || list.width > w.width / 2) return test.fail("the new window's columns: " + w.pageStack.depth + " pages, list " + list.width);
                // It does not release itself while it is shown.
                if (shell.releases !== 1) return test.fail("released while shown");
                w.openConversation(CHAT);
                test.next("again");
                return;
            }
            case "again": {
                const w = shell.window;
                if (w.chatPage === null || App.timeline.conversationId !== CHAT || App.timeline.count < 3) return test.later("the chat in the new window");
                if (!w.active || !w.chatShown) return test.later("the new window has the focus and shows its chat");
                if (App.presence() !== 3) return test.fail("what a focused window with a chat reports: " + App.presence());
                // ---- the user comes back in the moment the hidden window takes to go ----
                test.race = true;
                w.hideToTray();
                test.next("raced");
                return;
            }
            case "raced": {
                if (shell.releases < 2) return test.later("the second release");
                if (shell.coreReleases !== 2) return test.fail("each release asks the core once: " + shell.coreReleases);
                if (!test.raced) return test.fail("the activation did not fall between the release and the trim");
                const w = shell.window;
                if (w === null || !w.visible) return test.fail("the window that came back is not there");
                if (!w.started || !w.active || !w.chatShown) return test.later("it has the focus and shows the chat");
                // The release of the old window is over (its memory was handed back after
                // the new one was on screen): that must not have reset what the new window
                // said. Else its chat is notified about and never marked read while the
                // user looks at it.
                if (App.presence() !== 3) return test.fail("the release of the old window reset the new one's presence: " + App.presence());
                Qt.exit(0);
                return;
            }
            }
        }
    }

    Timer {
        interval: 120000
        running: true
        onTriggered: test.fail("timeout in step " + test.step)
    }
}
"##;

fn main() {
    // This test shows windows: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if !cfg!(debug_assertions) {
        println!(
            "test the hidden window is released ... skipped (a release build waits a minute)\n\ntest result: ok. 0 passed; 0 failed"
        );
        return;
    }
    // A hidden window is released after 200 ms instead of a minute (debug builds read this).
    // SAFETY: as above.
    unsafe { std::env::set_var("KOURIER_DEV_RELEASE_MS", "200") };
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
    let chat = fixture.demo_conversation.expect("a chat");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = SCRIPT.replace("CHAT", &chat.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/release_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code == 0 {
        println!(
            "test the hidden window is released ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
    } else {
        println!(
            "test the hidden window is released ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
