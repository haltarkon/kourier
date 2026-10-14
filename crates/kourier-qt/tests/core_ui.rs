//! The UI on the real core (S4 "Done when"): an empty profile starts without accounts, the fake
//! account links through rotating QR codes (device name prefilled with the host name), the
//! conversation list fills from `core.db`, a conversation opens with its newest 50 messages,
//! scrolling up pages older ones, and sending answers with a notice until S5 lands (or with the
//! new row once it has). Driven from QML through the same invokables the pages use; offscreen,
//! `harness = false`.

mod support;

use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

Item {
    id: test

    property string step: "start"
    property var codes: ({})
    property int codeCount: 0
    property real group: 0
    property real direct: 0
    property int messagesBefore: 0
    property real openedAt: 0
    property real listChangedAt: 0
    property real shownAt: 0
    // CPU time of the Qt thread when the chat was opened, and what opening it took of it.
    property real openedCpu: 0
    property real openCpu: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function messageRows(): int {
        let n = 0;
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message") {
                ++n;
            }
        }
        return n;
    }

    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {
            required property real conversationId
            required property bool isGroup
            required property string title
            required property string preview
        }
    }

    // The fixture's last delivery is in the list: the chat with stand-ins for content that is
    // not shown yet (kourier-app's showcase extras, delivered after everything else), with its
    // newest message. The history before it has been stored by then.
    function arrived(): bool {
        for (let i = 0; i < convs.count; ++i) {
            const c = convs.objectAt(i);
            if (c && c.title === "Wren Tallow" && c.preview === "Message for a newer version") {
                return true;
            }
        }
        return false;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
        }
    }

    // The fixture history streams in after the link: the list settles when it stops changing.
    Connections {
        target: App.conversations
        function onDataChanged(): void {
            test.listChangedAt = Date.now();
        }
        function onRowsInserted(): void {
            test.listChangedAt = Date.now();
        }
        function onRowsMoved(): void {
            test.listChangedAt = Date.now();
        }
    }

    // When the first page of the opened conversation is in the model.
    Connections {
        target: App.timeline
        function onCountChanged(): void {
            if (test.openedAt > 0 && test.shownAt === 0 && test.messageRows() >= 50) {
                test.shownAt = Date.now();
                test.openCpu = App.threadCpuMs() - test.openedCpu;
            }
        }
    }

    // Collect every QR code the page would show.
    Connections {
        target: App.login
        function onQrChanged(): void {
            if (App.login.qr !== "" && !test.codes[App.login.qr]) {
                test.codes[App.login.qr] = true;
                test.codeCount += 1;
            }
        }
    }

    Timer {
        interval: 20
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                if (!App.loaded) return;
                if (App.hasAccounts) return fail("an empty profile has no accounts");
                if (!App.fixture) return fail("fixture mode");
                let fake = false;
                for (const n of App.networks) {
                    if (n.kind === "Fake" && n.available) fake = true;
                }
                if (!fake) return fail("the fake network is offered");
                App.ready();
                App.login.start("Fake");
                test.step = "input";
                return;
            case "input":
                if (App.login.kind === "failed") return fail(App.login.error);
                if (App.login.kind !== "input") return;
                if (App.login.fields.length !== 1) return fail("one field");
                if (App.login.fields[0].id !== "device-name") return fail("device-name field");
                if (App.login.fields[0].value === "" || App.login.fields[0].value === "kourier")
                    return fail("prefilled with the host name, not the backend default");
                App.login.submit();
                test.step = "qr";
                return;
            case "qr":
                if (App.login.kind === "failed") return fail(App.login.error);
                if (App.login.kind !== "done") return;
                if (test.codeCount < 2) return fail("QR codes rotate: saw " + test.codeCount);
                test.step = "list";
                return;
            case "list":
                if (!App.hasAccounts || convs.count < 15) return;
                // All of the history, not a pause in its arrival (a slow stretch of the
                // ingest also leaves the list unchanged for a while).
                if (!test.arrived()) return;
                if (Date.now() - test.listChangedAt < 1500) return;
                for (let i = 0; i < convs.count; ++i) {
                    const c = convs.objectAt(i);
                    if (c.isGroup && test.group === 0) {
                        test.group = c.conversationId;
                    } else if (!c.isGroup && test.direct === 0) {
                        test.direct = c.conversationId;
                    }
                }
                if (test.group === 0) return fail("a group in the list");
                test.openedAt = Date.now();
                App.openConversation(test.group);
                if (App.timeline.conversationId !== test.group) return fail("open");
                test.step = "timeline";
                return;
            case "timeline":
                if (App.timeline.loading || App.timeline.title === "") return;
                if (test.messageRows() === 0) return;
                if (test.messageRows() !== 50) return fail("the newest 50 rows, got " + test.messageRows());
                if (!App.timeline.hasOlder) return fail("older rows exist");
                if (!App.timeline.isGroup) return fail("header of a group");
                App.fetchOlder();
                test.step = "older";
                return;
            case "older":
                if (App.timeline.loading) return;
                if (test.messageRows() === 100) {
                    App.fetchOlder();
                    test.step = "oldest";
                }
                return;
            case "oldest":
                if (App.timeline.loading || App.timeline.hasOlder) return;
                if (test.messageRows() !== 120) return fail("every row once, got " + test.messageRows());
                // The fixture has arrived by now: time a plain open (§1.3: first 50 rows).
                test.shownAt = 0;
                test.openedAt = Date.now();
                test.openedCpu = App.threadCpuMs();
                App.openConversation(test.direct);
                test.step = "measure";
                return;
            case "measure":
                if (test.shownAt === 0 || App.timeline.loading) return;
                // What opening a chat costs the Qt thread, in CPU time: that is the UI's own
                // share of §1.3's 100 ms, and it does not grow when other processes keep the
                // machine busy (this test once failed its wall-clock budget during a parallel
                // build). The time a user waits, on a release build, is measured and
                // enforced by `cargo xtask perf` (`timeline.open`).
                console.warn("open: first 50 rows in " + (test.shownAt - test.openedAt) + " ms, " + test.openCpu.toFixed(1) + " ms of the Qt thread");
                if (test.openCpu > 100) return fail("opening took " + test.openCpu.toFixed(1) + " ms of the Qt thread (budget 100)");
                App.openConversation(test.group);
                test.step = "again";
                return;
            case "again":
                if (App.timeline.loading || test.messageRows() !== 50) return;
                test.messagesBefore = test.messageRows();
                if (!App.composer.send(test.group, "hello from the test")) return fail("send refused");
                test.step = "sent";
                return;
            case "sent":
                // Until S5 the core answers NotImplemented (a notice); afterwards a new row.
                if (App.composer.notice !== "" || test.messageRows() > test.messagesBefore) {
                    console.warn("PASS");
                    Qt.exit(0);
                }
                return;
            }
        }
    }

    Timer {
        interval: 60000
        running: true
        onTriggered: fail("timeout; " + test.messageRows() + " message rows of " + rows.count + ", older " + App.timeline.hasOlder + ", newer " + App.timeline.hasNewer + ", loading " + App.timeline.loading)
    }
}
"#;

fn main() {
    support::answer_list("core_ui");
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before the app and Qt start.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            link: true,
            qr_valid_for: Duration::from_millis(300),
            qr_codes: 3,
            messages_per_conversation: 120,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext { app: app.handle() });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load_data(
            &QByteArray::from(SCRIPT),
            &QUrl::from("qrc:/kourier-tests/core_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    app.shutdown();
    if code == 0 {
        println!("test ui on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!(
            "test ui on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
