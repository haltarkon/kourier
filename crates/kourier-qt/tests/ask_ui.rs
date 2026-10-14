//! Asking the phone for lost files (S6), through the real main window and the real core over
//! the fake network (offscreen, `harness = false`; the fixture's chat of imported attachments
//! that cannot be had, whose account can ask its phone). One session of a user:
//!
//! - Every attachment the core lists in its row's `ask_attachments` has the Download: a file,
//!   a picture, a sticker's stand-in; one being asked for, and one the phone said it does not
//!   have, have none, whatever the note says.
//! - A click on a file's button turns the block to "Asking your phone…" and takes the
//!   Download away, the same for a picture's tap and for the Download inside a sticker's
//!   stand-in (reachable: shown, enabled, with a size to click).
//! - The viewer on a picture that can be asked offers the action, enabled.

mod support;

use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

// The app's own main window is the root (narrow: one page at a time), tall enough for every
// row of the chat to have its delegate.
MainWindow {
    id: win

    width: 520
    height: 3200
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    // The attachments the steps click, by id: a row that changes may get new delegates.
    property real file: 0
    property real picture: 0
    property real sticker: 0

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
    }

    // Every item below `root` that `pred` accepts.
    function collect(root: Item, pred: var, out: var): var {
        if (pred(root)) {
            out.push(root);
        }
        for (const child of root.children ?? []) {
            win.collect(child, pred, out);
        }
        return out;
    }

    // The first item below `root` that `pred` accepts.
    function find(root: Item, pred: var): Item {
        const all = win.collect(root, pred, []);
        return all.length > 0 ? all[0] : null;
    }

    // The media blocks on screen: pictures, album tiles and files.
    function blocks(): var {
        return win.collect(win.view.rowList.contentItem, c => c.mediaState !== undefined && c.ask !== undefined && c.attachment > 0 && c.visible, []);
    }

    // The first ancestor of `item` that is disabled (its object name and type).
    function disabledBy(item: Item): string {
        // (`enabled` is the effective one: the cause is the disabled ancestor whose parent is not.)
        for (let p = item.parent; p; p = p.parent) {
            if (!p.enabled && (!p.parent || p.parent.enabled)) {
                return p.objectName + "/" + p.toString();
            }
        }
        return "nothing";
    }

    // The block of attachment `id` on screen (`null` while it has none).
    function block(id: real): var {
        const all = win.blocks().filter(b => b.attachment === id);
        return all.length > 0 ? all[0] : null;
    }

    function click(item: Item): bool {
        return win.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
    }

    function note(block: Item): string {
        const label = win.find(block, c => typeof c.text === "string" && /Asking your phone/.test(c.text) && c.visible);
        return label === null ? "" : label.text;
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
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "ask_ui_events.qml");
            win.openConversation(IMPORTED_CHAT);
            return win.next("shown");
        }
        case "shown": {
            if (win.chatPage === null || App.timeline.conversationId !== IMPORTED_CHAT || App.timeline.loading) return win.later("the chat");
            // (A page takes input once the page row has moved to it.)
            if (!win.chatPage.enabled) return win.later("the chat page to take input");
            const all = win.blocks();
            // An album of four, a picture, a file and a sticker that are not here, a picture and
            // a file that are gone, the ones asked for and the ends of an ask, and one that waits.
            if (all.length < 16) return win.later("the blocks: " + all.length);
            const states = {};
            let sticker = null;
            let file = null;
            let picture = null;
            for (const b of all) {
                states[b.mediaState] = (states[b.mediaState] ?? 0) + 1;
                const want = ["expired", "unavailable", "noanswer"].includes(b.mediaState);
                if (b.ask !== want) return win.fail("a block that is " + b.mediaState + " has ask " + b.ask);
                const gone = ["asking", "nomessage", "nofile"].includes(b.mediaState);
                if ((want || gone) && b.mediaAction !== (want ? "download" : "")) return win.fail("a block that is " + b.mediaState + " does \"" + b.mediaAction + "\"");
                if (b.sticker === true && b.mediaState === "unavailable") sticker = b;
                else if (b.voice !== undefined && b.mediaState === "unavailable" && file === null) file = b;
                else if (b.voice === undefined && b.sticker === false && b.tile === false && b.mediaState === "unavailable" && picture === null) picture = b;
            }
            for (const s of ["unavailable", "expired", "asking", "noanswer", "nomessage", "nofile"]) {
                if (!states[s]) return win.fail("no block is " + s + ": " + JSON.stringify(states));
            }
            if (file === null || picture === null || sticker === null) return win.fail("a file, a picture and a sticker are missing");
            // The sticker's Download is inside its box, reachable.
            const box = win.find(sticker, c => c.objectName === "standInBox");
            const download = win.find(sticker, c => c.objectName === "standInDownload");
            if (box === null || download === null || !download.visible || !download.enabled) return win.fail("the sticker's stand-in has no Download that can be clicked: box " + (box !== null) + ", button " + (download !== null) + (download !== null ? ", visible " + download.visible + ", enabled " + download.enabled + ", disabled by " + win.disabledBy(download) : ""));
            const inside = download.mapToItem(box, 0, 0);
            if (inside.x < 0 || inside.y < 0 || inside.x + download.width > box.width + 1 || inside.y + download.height > box.height + 1) return win.fail("the Download is not inside the stand-in's box");
            if (win.find(sticker, c => c.visible && c.width > 0 && Math.abs(c.width - c.height) < 1 && c.radius !== undefined && c.radius === c.width / 2) !== null) return win.fail("a round badge covers the stand-in");
            win.file = file.attachment;
            win.picture = picture.attachment;
            win.sticker = sticker.attachment;
            // A click on the file's button.
            const button = win.find(file, c => c.hoverEnabled !== undefined && c.clicked !== undefined && c.visible && c.width > 0);
            if (button === null || !button.enabled) return win.fail("the file's button is not there or not enabled");
            if (!win.click(button)) return win.fail("the window took no mouse event");
            return win.next("file");
        }
        case "file": {
            const file = win.block(win.file);
            if (file === null || file.mediaState !== "asking") return win.later("the file asking");
            if (file.ask || file.mediaAction !== "") return win.fail("a file being asked for still offers Download");
            if (win.note(file) === "") return win.fail("the file does not say it asks the phone");
            // A tap on the picture.
            const picture = win.block(win.picture);
            if (picture === null || !win.click(picture)) return win.fail("the window took no mouse event on the picture");
            return win.next("picture");
        }
        case "picture": {
            const picture = win.block(win.picture);
            if (picture === null || picture.mediaState !== "asking") return win.later("the picture asking");
            if (picture.ask || picture.mediaAction !== "") return win.fail("a picture being asked for still offers Download");
            // The Download inside the stand-in.
            const sticker = win.block(win.sticker);
            const download = sticker === null ? null : win.find(sticker, c => c.objectName === "standInDownload");
            if (download === null || !win.click(download)) return win.fail("the window took no mouse event on the stand-in's Download");
            return win.next("sticker");
        }
        case "sticker": {
            const sticker = win.block(win.sticker);
            if (sticker === null || sticker.mediaState !== "asking") return win.later("the sticker asking");
            const download = win.find(sticker, c => c.objectName === "standInDownload");
            if (download !== null && download.visible) return win.fail("the stand-in still offers Download");
            if (sticker.mediaAction !== "") return win.fail("the sticker still offers an action");
            // The viewer on a picture that is gone and can be asked: its action is enabled.
            const gone = win.blocks().filter(b => b.voice === undefined && b.sticker === false && b.tile === false && b.mediaState === "expired");
            if (gone.length === 0) return win.fail("no picture is gone");
            const items = App.viewerItems(gone[0].message);
            if (items.length === 0 || items[0].state !== "expired" || items[0].ask !== true) return win.fail("the viewer's item: " + JSON.stringify(items.map(i => i.state + ":" + i.ask)));
            const v = win.openAlbumViewer(items, 0, gone[0].message);
            if (!v || !v.opened) return win.later("the viewer");
            if (!v.ask || !v.downloadable) return win.fail("the viewer on a gone picture: ask " + v.ask + ", downloadable " + v.downloadable);
            const holder = win.find(v.contentItem, c => c && c.helpfulAction !== undefined && c.helpfulAction !== null && c.helpfulAction.objectName === "viewerAction");
            if (holder === null) return win.fail("the viewer has no action");
            if (!holder.helpfulAction.enabled) return win.fail("the viewer's Download is not enabled");
            Qt.exit(0);
        }
        }
    }
}
"##;

fn main() {
    support::answer_list("ask_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
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
    let fixture = handle.fixture().expect("fixture mode");
    let imported = fixture
        .unavailable_conversation
        .expect("the chat of attachments that cannot be had");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT.replace("IMPORTED_CHAT", &imported.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/ask_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test asking the phone on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    // (What the clicks started is stored by now or on its way: let the core finish.)
    std::thread::sleep(Duration::from_millis(200));
    app.shutdown();
    println!(
        "test asking the phone on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed"
    );
}
