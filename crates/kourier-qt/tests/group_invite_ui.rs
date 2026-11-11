//! The invite link on a group's page, through the real main window and the real core over the
//! fake network (offscreen, `harness = false`): the account is an administrator and the link
//! starts off. The buttons follow the link's state, Copy and Reset put the link on the clipboard
//! while the toast never holds it, and what cuts people off is asked first.

mod support;

use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

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
    property int tolds: 0
    property int marked: 0
    property string told: ""
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
        function onCopyTold(text: string): void {
            win.told = text;
            win.tolds += 1;
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

    function findBy(test: var): var {
        const found = [];
        const walk = item => {
            if (test(item)) found.push(item);
            for (const c of item.children) walk(c);
        };
        if (win.chat && win.chat.panel) walk(win.chat.panel);
        return found.length > 0 ? found[0] : null;
    }

    function button(name: string): var {
        return win.findBy(item => item.objectName === name && item.visible);
    }

    // The question of a reset or a turning off: a popup, so it is among the page's data.
    function question(): var {
        const walk = item => {
            if (!item || !item.data) return null;
            for (const d of item.data) {
                if (d.objectName === "groupInviteDialog") return d;
                const found = walk(d);
                if (found) return found;
            }
            return null;
        };
        return win.chat ? walk(win.chat.panel) : null;
    }

    function pasted(): string {
        const edit = Qt.createQmlObject("import QtQuick; TextEdit { visible: false }", win, "invitePaste");
        edit.paste();
        const text = edit.text;
        edit.destroy();
        return text;
    }

    function click(name: string, then: string): void {
        win.marked = win.tolds;
        win.button(name).clicked();
        win.next(then);
    }

    // The toast of the last action is `want`, without a link in it.
    function toldIs(want: string): void {
        if (win.told !== want) win.fail("the toast says '" + win.told + "', not '" + want + "'");
        if (win.told.indexOf("invite.example") >= 0) win.fail("the toast holds the link");
    }

    function onShows(on: bool): bool {
        return !!win.button("groupInviteCopy") === on && !!win.button("groupInviteReset") === on
            && !!win.button("groupInviteOff") === on && !!win.button("groupInviteOn") === !on;
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
            if (!win.chat.panelOpen || !win.chat.panel || !win.chat.panel.loaded || !win.button("groupInviteOn")) return win.later("the invite link's part of the page");
            if (!win.onShows(false)) return win.fail("a link that is off offers only Turn On");
            win.click("groupInviteOn", "on");
            return;
        case "on":
            if (win.tolds <= win.marked || !win.onShows(true)) return win.later("the link on");
            win.toldIs("The invite link is on");
            if (win.pasted().indexOf("invite.example") >= 0) return win.fail("turning on put the link on the clipboard");
            win.click("groupInviteCopy", "copied");
            return;
        case "copied":
            if (win.tolds <= win.marked) return win.later("the toast of Copy");
            win.toldIs("Invite link copied");
            if (win.pasted() !== "https://invite.example/1") return win.fail("the clipboard holds '" + win.pasted() + "'");
            win.click("groupInviteReset", "resetAsked");
            return;
        case "resetAsked": {
            const d = win.question();
            if (!d || !d.opened) return win.later("the question before a reset");
            if (d.action !== "reset") return win.fail("the question is about '" + d.action + "'");
            if (win.tolds !== win.marked) return win.fail("the question did not wait for the answer");
            d.customFooterActions[0].trigger();
            win.next("reset");
            return;
        }
        case "reset":
            if (win.tolds <= win.marked) return win.later("the toast of Reset");
            win.toldIs("New invite link copied");
            if (win.pasted() !== "https://invite.example/2") return win.fail("the clipboard holds the old link");
            win.next("resetClosed");
            return;
        case "resetClosed":
            if (win.question().visible) return win.later("the question closed");
            win.click("groupInviteOff", "cancelAsked");
            return;
        case "cancelAsked": {
            const d = win.question();
            if (!d || !d.opened) return win.later("the question before turning off");
            if (d.action !== "off") return win.fail("the question is about '" + d.action + "'");
            d.customFooterActions[1].trigger();
            win.next("cancelled");
            return;
        }
        case "cancelled":
            if (win.question().visible) return win.later("Cancel closes the question");
            if (win.tolds !== win.marked || !win.onShows(true)) return win.fail("Cancel changed the link");
            win.click("groupInviteOff", "offAsked");
            return;
        case "offAsked": {
            const d = win.question();
            if (!d || !d.opened) return win.later("the question before turning off, again");
            d.customFooterActions[0].trigger();
            win.next("off");
            return;
        }
        case "off":
            if (win.tolds <= win.marked || !win.onShows(false)) return win.later("the link off");
            win.toldIs("The invite link is off");
            if (win.pasted() !== "https://invite.example/2") return win.fail("turning off changed the clipboard");
            win.click("groupInviteOn", "onAgain");
            return;
        case "onAgain":
            if (win.tolds <= win.marked || !win.onShows(true)) return win.later("the link on again");
            win.click("groupInviteCopy", "sameLink");
            return;
        case "sameLink":
            if (win.tolds <= win.marked) return win.later("the toast of the last Copy");
            if (win.pasted() !== "https://invite.example/2") return win.fail("turning on again did not bring the same link back");
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 120000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn main() {
    support::answer_list("group_invite_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            group_admin: true,
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

    let script = SCRIPT.replace("THE_GROUP", &group.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/group_invite_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code != 0 {
        println!(
            "test the invite link of a group ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    println!("test the invite link of a group ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
