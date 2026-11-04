//! The group page of a group with a thousand members, through the real main window and the real
//! core over the fake network (offscreen, `harness = false`): the measurement of people pages
//! P2's decision F9 (4). The page is open on its "Show All" level (the 1,000-row list is alive),
//! and twenty times the group changes for real (its disappearing-messages timer moves: the
//! fake answers with a conversation patch, the core asks the page again, the answer DIFFERS,
//! the Qt thread diffs the member rows into its two list models and sets the new map only when
//! the rest of the page changed). The Qt thread's CPU time and
//! the wall time from the command to the new page are printed per refresh (`INFO:` lines); the
//! budget is 16 ms of the Qt thread. The test fails only far over it (a loose guard for a
//! loaded machine), never on a number it prints.

mod support;

use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

/// The test fails when the mean of the Qt thread's CPU per refresh is above this many ms: 3x the
/// mean measured on 2026-10-23 (21.4 ms, so 65) capped at 50, a loose guard for a loaded machine.
/// The budget is 16 ms.
const THRESHOLD_MS: u32 = 50;

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

MainWindow {
    id: win

    width: 1000
    height: 700
    visible: true

    property string step: "start"
    property int waits: 0
    property int loads: 0
    property int round: 0
    property real cpu0: 0
    property real wall0: 0
    property var cpus: []
    property var walls: []
    readonly property var chat: win.chatPage

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

    Connections {
        target: App
        function onGroupLoaded(): void {
            win.loads += 1;
        }
    }

    property bool busy: false
    Timer {
        interval: 20
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

    property int phase: 1

    function report(what: string): void {
        const sum = a => a.reduce((x, y) => x + y, 0);
        const mx = a => Math.max.apply(null, a);
        console.warn("INFO: 1000-member group, 20 real refreshes, " + what + ": Qt thread "
            + (sum(win.cpus) / 20).toFixed(1) + " ms mean, " + mx(win.cpus).toFixed(1) + " ms max; wall "
            + (sum(win.walls) / 20).toFixed(0) + " ms mean, " + mx(win.walls).toFixed(0) + " ms max (cpu each: "
            + win.cpus.map(c => c.toFixed(0)).join(" ") + ")");
        win.means.push(sum(win.cpus) / 20);
    }

    property var means: []
    property int gtWas: 0
    property int loads2: 0

    function timerFor(i: int): int {
        return i % 2 === 0 ? 3600 : 604800;
    }

    function tick(): void {
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            win.openConversation(THE_GROUP);
            win.next("opening");
            return;
        case "opening":
            if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !win.chat || !win.chat.detailsShown) return win.later("the group's chat");
            win.chat.openDetails();
            win.next("page");
            return;
        case "page":
            if (!win.chat.panelOpen || !win.chat.panel || !win.chat.panel.loaded) return win.later("the group's page");
            if (App.group.memberCount < 1000 || App.groupMembers.count < 1000) return win.fail("the group has " + App.group.memberCount + " members (" + App.groupMembers.count + " rows), not 1,000");
            win.chat.panel.showAll();
            win.next("all");
            return;
        case "all": {
            const list = win.findList();
            if (win.chat.panel.level !== "members" || !list || list.count < 1000) return win.later("the members level with its 1,000 rows");
            win.next("round");
            return;
        }
        case "round": {
            if (win.round >= 20 && win.phase === 1) {
                // The same twenty changes with only the group's own page open (five rows).
                win.report("members level open");
                const list = win.findList();
                if (!list || list.count < 1000 || App.groupMembers.count < 1000) return win.fail("the list lost rows: " + (list ? list.count : "none"));
                const time = f => {
                    const t = App.threadCpuMs();
                    for (let i = 0; i < 10; i++) f();
                    return ((App.threadCpuMs() - t) / 10).toFixed(1);
                };
                console.warn("INFO: one part alone, ms of the Qt thread: reading App.group " + time(() => App.group.memberCount + App.group.name.length)
                    + ", counting the members model " + time(() => App.groupMembers.count)
                    + " (the models' own apply is timed in Rust, not here)");
                win.chat.panel.back();
                win.phase = 2;
                win.round = 0;
                win.cpus = [];
                win.walls = [];
                win.next("back");
                return;
            }
            if (win.round >= 20) {
                win.report("group level only");
                win.next("gtChoose");
                return;
            }
            win.cpu0 = App.threadCpuMs();
            win.wall0 = Date.now();
            win.mark = win.loads;
            App.setTimerOf(THE_GROUP, win.timerFor(win.round));
            win.next("wait");
            return;
        }
        case "back":
            if (win.chat.panel.level !== "group") return win.later("the group level");
            win.next("round");
            return;
        case "wait":
            if (win.loads <= win.mark || App.group.timerSecs !== win.timerFor(win.round)) return win.later("the changed page, round " + win.round);
            win.cpus.push(App.threadCpuMs() - win.cpu0);
            win.walls.push(Date.now() - win.wall0);
            win.round += 1;
            win.next("round");
            return;
        // ---- F4 (his answer: "the group panel follows the same rule"): a choice in the group's selector sends nothing and
        // dies with the panel (`GroupPage.reset()` -> `TimerPart.reset()`), and a refused change is a line under the
        // selector (`GroupPage`'s `failed:`). After the budget was measured: these steps are no part of it.
        case "gtChoose": {
            const part = win.findBy(item => item.objectName === "groupTimer");
            const box = win.findBy(item => item.objectName === "groupTimerBox" && item.visible);
            if (!part || !box || box.entries.length < 2) return win.later("the group's selector");
            const at = box.entries.findIndex(e => !e.custom && e.secs !== App.group.timerSecs);
            if (at < 0) return win.fail("the group's selector offers no other time");
            win.gtWas = App.group.timerSecs;
            box.activated(at);
            if (!part.hasPending || part.pendingSecs === win.gtWas) return win.fail("the group's selector kept no choice");
            if (!win.findBy(item => item.objectName === "groupTimerBoxChoice" && item.visible)) return win.fail("a choice in the group's selector shows no Apply and Cancel");
            // The page's own `reset()` (the panel calls it at every open and close) forgets the choice by itself. Closing the
            // panel also empties the page (`App.closeGroup()`: the part's conversation changes, which drops a choice too), so
            // the reset is called here with the page still standing: only its wiring to the part can end the choice now.
            let page = part.parent;
            while (page && !(typeof page.reset === "function" && page.membersShown !== undefined)) page = page.parent;
            if (!page) return win.fail("the group's page was not found above its selector");
            page.reset();
            if (part.hasPending) return win.fail("the group page's reset() leaves a choice in its selector");
            box.activated(at);
            if (!part.hasPending) return win.fail("the selector kept no choice the second time");
            // The X: the panel closes and the part forgets the choice.
            const close = win.findBy(item => item.objectName === "detailsClose" && item.visible);
            if (!close) return win.fail("no X on the group's panel");
            close.clicked();
            win.next("gtClosed");
            return;
        }
        case "gtClosed":
            if (win.chat.panelOpen) return win.later("the X closes the group's page");
            win.loads2 = win.loads;
            win.chat.openDetails();
            win.next("gtReopened");
            return;
        case "gtReopened": {
            if (!win.chat.panelOpen || !win.chat.panel || !win.chat.panel.loaded || win.chat.panel.level !== "group" || win.loads <= win.loads2 || win.waits < 5) return win.later("the group's page again");
            const part = win.findBy(item => item.objectName === "groupTimer");
            if (part.hasPending || win.findBy(item => item.objectName === "groupTimerBoxChoice" && item.visible)) return win.fail("a choice outlived the closed panel");
            if (App.group.timerSecs !== win.gtWas) return win.fail("a choice alone changed the group's timer");
            // A change the chat does not take is refused: the line under the selector says why.
            App.setTimerOf(App.group.id, 2147483647);
            win.next("gtFailed");
            return;
        }
        case "gtFailed": {
            if ((App.group.timerFailed ?? "") === "") return win.later("the refusal of the group's timer");
            const line = win.findBy(item => item.objectName === "groupTimerBoxFailed" && item.visible);
            if (!line || line.text !== App.group.timerFailed) {
                if (win.waits < 10) return win.later("the failure line under the group's selector");
                return win.fail("the group's page shows no failure line under its selector");
            }
            win.next("done");
            return;
        }
        case "done": {
            if (Math.max.apply(null, win.means) > THRESHOLD_MS) return win.fail("far over the budget of 16 ms: " + win.means.join(", ") + " ms");
            Qt.exit(0);
            return;
        }
        }
    }

    property int mark: 0

    function findBy(test: var): var {
        const found = [];
        const walk = item => {
            if (test(item)) found.push(item);
            for (const c of item.children) walk(c);
        };
        if (win.chat && win.chat.panel) walk(win.chat.panel);
        return found.length > 0 ? found[0] : null;
    }

    function findList(): var {
        return win.findBy(item => item.objectName === "membersList");
    }

    Timer {
        interval: 120000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn main() {
    support::answer_list("group_load_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            group_members: 1000,
            group_timers: true,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let group = handle
        .fixture()
        .expect("fixture mode")
        .group_conversation
        .expect("the group of the group page");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT
        .replace("THE_GROUP", &group.0.to_string())
        .replace("THRESHOLD_MS", &THRESHOLD_MS.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/group_load_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code != 0 {
        println!(
            "test a group of a thousand members ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    println!("test a group of a thousand members ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
