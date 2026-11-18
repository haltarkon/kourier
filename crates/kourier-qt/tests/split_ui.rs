//! Split view through `App` (S2): two chats open side by side on the real core over the fake
//! network. Pane 0's models stay `App.timeline` / `App.composer`, pane 1's are `App.sideTimeline`
//! / `App.sideComposer`; the active pane's chat is the one in hand. Opening in the split, paging
//! one pane, activating, the same chat never in two panes, closing either pane (the files parked
//! for a chat follow it). Offscreen, silent, `harness = false`.

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
    property real group: 0
    property real direct: 0
    property real listChangedAt: 0
    property int mainCount: 0
    property int sideCount: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function arrived(): bool {
        for (let i = 0; i < convs.count; ++i) {
            const c = convs.objectAt(i);
            if (c && c.title === "Wren Tallow" && c.preview === "Message for a newer version") {
                return true;
            }
        }
        return false;
    }

    function ready(model: var, id: real): bool {
        return model.conversationId === id && !model.loading && model.count > 0;
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

    Connections {
        target: App.conversations
        function onDataChanged(): void { test.listChangedAt = Date.now(); }
        function onRowsInserted(): void { test.listChangedAt = Date.now(); }
        function onRowsMoved(): void { test.listChangedAt = Date.now(); }
    }

    Timer {
        interval: 20
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                if (!App.loaded) return;
                App.ready();
                App.login.start("Fake");
                test.step = "input";
                return;
            case "input":
                if (App.login.kind === "failed") return fail(App.login.error);
                if (App.login.kind !== "input") return;
                App.login.submit();
                test.step = "qr";
                return;
            case "qr":
                if (App.login.kind === "failed") return fail(App.login.error);
                if (App.login.kind !== "done") return;
                test.step = "list";
                return;
            case "list":
                if (!App.hasAccounts || convs.count < 15) return;
                if (!test.arrived()) return;
                if (Date.now() - test.listChangedAt < 1500) return;
                for (let i = 0; i < convs.count; ++i) {
                    const c = convs.objectAt(i);
                    if (c.isGroup && test.group === 0) test.group = c.conversationId;
                    else if (!c.isGroup && test.direct === 0) test.direct = c.conversationId;
                }
                if (test.group === 0 || test.direct === 0) return fail("a group and a direct chat");
                if (App.split || App.activePane !== 0) return fail("no split at the start");
                if (App.sideTimeline.conversationId !== 0) return fail("pane 1 empty at the start");
                App.openConversation(test.direct);
                test.step = "openA";
                return;
            case "openA":
                if (!test.ready(App.timeline, test.direct)) return;
                if (App.paneOf(test.direct) !== 0 || App.paneOf(test.group) !== -1) return fail("paneOf without a split");
                if (App.composer.addFiles(["@F1@"]) !== 1) return fail("a file added to pane 0's composer");
                test.step = "filesA";
                return;
            case "filesA":
                if (App.composer.attachmentCount !== 1) return;
                App.openInSplit(test.group);
                // Synchronous: the models are set up in the call.
                if (!App.split) return fail("split true");
                if (App.activePane !== 1) return fail("the new pane is active, got " + App.activePane);
                if (App.sideTimeline.conversationId !== test.group) return fail("sideTimeline shows the group");
                if (App.timeline.conversationId !== test.direct) return fail("timeline still shows the direct chat");
                test.step = "split";
                return;
            case "split":
                if (!test.ready(App.sideTimeline, test.group)) return;
                if (App.timeline.conversationId !== test.direct || App.timeline.count === 0) return fail("pane 0 keeps its rows");
                if (App.composer.attachmentCount !== 1) return fail("pane 0's files stay, got " + App.composer.attachmentCount);
                if (App.sideComposer.attachmentCount !== 0) return fail("pane 1's composer starts empty");
                if (App.sideComposer.addFiles(["@F2@", "@F3@"]) !== 2) return fail("two files added to pane 1's composer");
                test.step = "filesB";
                return;
            case "filesB":
                if (App.sideComposer.attachmentCount !== 2) return;
                if (App.composer.attachmentCount !== 1) return fail("pane 0's files unchanged by pane 1's");
                if (!App.sideTimeline.hasOlder) return fail("older rows in the group");
                test.mainCount = App.timeline.count;
                test.sideCount = App.sideTimeline.count;
                App.fetchOlderIn(1);
                test.step = "paging";
                return;
            case "paging":
                if (App.sideTimeline.count <= test.sideCount) return;
                if (App.sideTimeline.loading) return;
                if (App.timeline.count !== test.mainCount) return fail("pane 0's rows are unchanged by pane 1's paging");
                if (App.timeline.conversationId !== test.direct) return fail("pane 0 chat");
                App.activatePane(0);
                if (App.activePane !== 0 || !App.split) return fail("pane 0 active");
                if (App.timeline.conversationId !== test.direct || App.sideTimeline.conversationId !== test.group) return fail("each model keeps its chat");
                if (App.composer.attachmentCount !== 1 || App.sideComposer.attachmentCount !== 2) return fail("each composer keeps its files");
                test.step = "again";
                return;
            case "again":
                // The group is pane 1's: opening it activates pane 1, no chat in two panes.
                App.openConversation(test.group);
                if (App.activePane !== 1) return fail("opening the other pane's chat activates it, got " + App.activePane);
                if (App.timeline.conversationId !== test.direct || App.sideTimeline.conversationId !== test.group) return fail("no chat moved");
                if (App.paneOf(test.group) !== 1 || App.paneOf(test.direct) !== 0) return fail("paneOf with a split");
                App.activatePane(0);
                test.step = "closeSide";
                return;
            case "closeSide":
                App.closePane(1);
                if (App.split || App.activePane !== 0) return fail("closing pane 1 ends the split");
                if (App.timeline.conversationId !== test.direct) return fail("pane 0 keeps the direct chat");
                if (App.sideTimeline.conversationId !== 0) return fail("pane 1 is empty");
                if (App.composer.attachmentCount !== 1) return fail("pane 0's files stay");
                App.openInSplit(test.group);
                test.step = "resplit";
                return;
            case "resplit":
                if (!App.split || !test.ready(App.sideTimeline, test.group)) return;
                if (App.sideComposer.attachmentCount !== 2) return fail("the group's files waited for it, got " + App.sideComposer.attachmentCount);
                App.closePane(0);
                test.step = "closeMain";
                return;
            case "closeMain":
                if (App.split) return fail("closing pane 0 ends the split");
                if (!test.ready(App.timeline, test.group)) return;
                if (App.sideTimeline.conversationId !== 0) return fail("pane 1 is empty");
                if (App.composer.attachmentCount !== 2) return fail("the group's files followed it into pane 0, got " + App.composer.attachmentCount);
                if (App.sideComposer.attachmentCount !== 0) return fail("pane 1's composer is empty");
                console.warn("PASS");
                Qt.exit(0);
                return;
            }
        }
    }

    Timer {
        interval: 90000
        running: true
        onTriggered: fail("timeout; main " + App.timeline.conversationId + "/" + App.timeline.count + ", side " + App.sideTimeline.conversationId + "/" + App.sideTimeline.count + ", split " + App.split + ", active " + App.activePane)
    }
}
"#;

fn main() {
    support::answer_list("split_ui");
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before the app and Qt start.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // Three small files for the composers' strips.
    let dir = std::env::temp_dir().join(format!("kourier-split-ui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory for the files");
    let urls: Vec<String> = ["one.txt", "two.txt", "three.txt"]
        .iter()
        .map(|name| {
            let path = dir.join(name);
            std::fs::write(&path, name).expect("write a file");
            path.display().to_string()
        })
        .collect();
    let script = SCRIPT
        .replace("@F1@", &urls[0])
        .replace("@F2@", &urls[1])
        .replace("@F3@", &urls[2]);
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
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/split_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    app.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    if code == 0 {
        println!("split view on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!(
            "split view on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
