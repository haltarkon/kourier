//! The menus and dialog of the Signal Storage writes in the real main window, over the real core
//! on the fake network (A10d; offscreen, `harness = false`; each case in a process of its own, no
//! script error allowed, silent and windowless).
//!
//! - `menu-again` (decisions H1): the row menu of an ordinary chat of the fixture's account (sync
//!   on), then of Note to Self, then of an ordinary chat again, all in one process: the sub-menu
//!   "Mute" has its six entries and opens each time it is shown; for Note to Self there is no
//!   Mute at all (neither the sub-menu's entry nor the flat one) and Archive and Pin stay.
//!
//! - `until-opens` (D4): the window's `askMuteUntil` on a real chat makes the "Mute Until" dialog
//!   (loaded by URL, once) and opens it at tomorrow 8:00; the Date and Time fields open KDE's
//!   calendar and clock by a real click and Escape closes only that popup; a held Return does
//!   nothing and a pressed one mutes the chat and closes the dialog.
//!
//! D6 and D7 (`roaming_ui_prog.qml`, cases written as programs of named steps; the listener of this
//! test tells the script what the fake network heard and the core's clock):
//!
//! - `menu-on`, `menu-off`, `menu-off-narrow` (480 px), `menu-logged-out`, `menu-refused`,
//!   `menu-hidden`, `menu-read`, `note-to-self`, `chat-menu`, `chat-archived`: the row menu and the
//!   chat page's actions (D6).
//! - `keys`, `keys-narrow`, `keys-off`, `undo`, `notices`, `undo-pinned`, `mute`, `unmute`, `until`,
//!   `until-past`, `pin-limit`, `blended`, `settings-switch`, `settings-refused` (D7).
//!
//! `KOURIER_ROAMING_ONLY=a,b` picks cases; `KOURIER_ROAMING_SHOTS=<dir>` writes a picture of a
//! case for a look (not a test).

mod support;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureNetwork, FixtureOptions, FixtureState, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

// The app's own main window is the root (wide: the list and the chat side by side).
MainWindow {
    id: win

    width: 1100
    height: 760
    visible: true

    property string mode: "CASE"
    property string step: "start"
    property int waits: 0
    property bool busy: false
    property var ev: null
    // The chats the case uses: two ordinary ones and Note to Self (their ids).
    property real plain: 0
    property real plain2: 0
    property real note: 0
    property int round: 0

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
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

    function shot(name: string): void {
        if ("SHOTS_DIR" !== "") {
            win.contentItem.grabToImage(r => r.saveToFile("SHOTS_DIR/" + name + ".png"));
        }
    }

    // The list's row for the chat `id` (null when it is not on screen).
    function rowOf(id: real): Item {
        const list = win.pageStack.get(0).conversations;
        for (let i = 0; i < list.count; ++i) {
            const row = list.itemAtIndex(i);
            if (row && row.conversationId === id) {
                return row;
            }
        }
        return null;
    }

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
        const page = win.pageStack.depth > 0 ? win.pageStack.get(0) : null;
        switch (win.step) {
        case "start": {
            if (!win.started || !App.hasAccounts || !page || !page.conversations) return win.later("the start page");
            const list = page.conversations;
            if (list.count < 4) return win.later("the chats");
            win.ev = Qt.createQmlObject("import QtQuick\nimport QtTest\nTestEvent {}", win.contentItem, "roaming_ui_events.qml");
            let plain = [];
            for (let i = 0; i < list.count; ++i) {
                const row = list.itemAtIndex(i);
                if (!row) continue;
                if (row.isSelf) win.note = row.conversationId;
                else plain.push(row.conversationId);
            }
            if (win.note === 0) return win.fail("the fixture has no Note to Self row on screen");
            if (plain.length < 2) return win.fail("the fixture has no two ordinary rows on screen");
            win.plain = plain[0];
            win.plain2 = plain[1];
            // The account states its switch (On) when the core has told the caps.
            if (App.accountActions.length !== 1 || App.accountActions[0].roaming !== "on") return win.later("the account's switch states On");
            win.next(win.mode === "until-opens" ? "until-ask" : "open");
            return;
        }
        case "until-ask": {
            // The window loads the dialog by URL on the first ask (the pages never name it).
            if (win.muteUntilDialog !== null) return win.fail("the dialog was made before it was asked for");
            win.askMuteUntil(win.plain);
            win.next("until-wait");
            return;
        }
        case "until-wait": {
            const d = win.muteUntilDialog;
            if (!d || !d.opened) return win.later("the dialog opens");
            if (d.conversation !== win.plain) return win.fail("the dialog is for another chat");
            if (d.title !== "Mute Until") return win.fail("the dialog's title");
            if (d.year < 2000 || d.hour !== 8 || d.minute !== 0) return win.fail("the dialog does not start at tomorrow 8:00 (" + d.hour + ":" + d.minute + ")");
            if (!d.allowed) return win.fail("tomorrow 8:00 is not allowed");
            if (d.dateField.text === "" || d.timeField.text === "") return win.fail("the fields show nothing");
            win.shot("until");
            if (!win.ev.mouseClick(d.dateField, d.dateField.width / 2, d.dateField.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) return win.fail("the window took no mouse event");
            win.next("until-date");
            return;
        }
        case "until-date": {
            const d = win.muteUntilDialog;
            if (!d.datePopup.opened) return win.later("the date field opens KDE's calendar");
            win.shot("until-date");
            win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            win.next("until-date-closed");
            return;
        }
        case "until-date-closed": {
            const d = win.muteUntilDialog;
            if (d.datePopup.opened) return win.later("Escape closes the calendar");
            if (!d.opened) return win.fail("Escape closed the dialog with the calendar");
            if (!win.ev.mouseClick(d.timeField, d.timeField.width / 2, d.timeField.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) return win.fail("the window took no mouse event");
            win.next("until-time");
            return;
        }
        case "until-time": {
            const d = win.muteUntilDialog;
            if (!d.timePopup.opened) return win.later("the time field opens KDE's clock");
            win.shot("until-time");
            win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            win.next("until-time-closed");
            return;
        }
        case "until-time-closed": {
            const d = win.muteUntilDialog;
            if (d.timePopup.opened) return win.later("Escape closes the clock");
            if (!d.opened) return win.fail("Escape closed the dialog with the clock");
            // A repeat of a held Return does nothing; the first press mutes.
            d.returnKey({ isAutoRepeat: true, accepted: false });
            if (!d.opened) return win.fail("a held Return pressed Mute");
            d.returnKey({ isAutoRepeat: false, accepted: false });
            win.next("until-done");
            return;
        }
        case "until-done": {
            const d = win.muteUntilDialog;
            if (d.opened) return win.later("Mute closes the dialog");
            if (App.chatOffer(win.plain).muted !== true) return win.later("the chat is muted");
            Qt.exit(0);
            return;
        }
        case "open": {
            // The three menus in turn: ordinary, Note to Self, ordinary again, then the first
            // chat's once more; `round` says which.
            const ids = [win.plain, win.note, win.plain2, win.plain];
            if (win.round >= ids.length) { Qt.exit(0); return; }
            const row = win.rowOf(ids[win.round]);
            if (!row) return win.later("the row of the chat");
            page.openMenu(row, 20, 10);
            win.next("opened");
            return;
        }
        case "opened": {
            if (!page.rowMenu.opened) return win.later("the menu opens");
            const menu = page.rowMenu;
            const isNote = win.round === 1;
            if (menu.conversationId !== [win.plain, win.note, win.plain2, win.plain][win.round]) return win.fail("the menu is for another chat");
            if (!page.archiveMenuEntry.visible || !page.archiveMenuEntry.enabled) return win.fail("Archive is not offered");
            if (!page.pinEntry.visible || !page.pinEntry.enabled) return win.fail("Pin is not offered");
            if (menu.muteItem === null) return win.fail("the menu did not find its sub-menu's entry");
            if (isNote) {
                if (menu.muteItem.visible || menu.muteItem.height !== 0) return win.fail("Note to Self shows the Mute sub-menu");
                if (page.muteFlat.visible) return win.fail("Note to Self shows a flat Mute or Unmute");
                win.shot("note");
                menu.close();
                win.next("closed");
                return;
            }
            if (!menu.muteItem.visible || !menu.muteItem.enabled || menu.muteItem.height <= 0) return win.fail("the Mute sub-menu's entry is not shown");
            if (page.muteFlat.visible) return win.fail("the flat Mute shows beside the sub-menu");
            if (page.muteSub.count !== 6) return win.fail("the sub-menu has " + page.muteSub.count + " entries, not six");
            win.shot("plain" + win.round);
            if (!win.ev.mouseClick(menu.muteItem, menu.muteItem.width / 2, menu.muteItem.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) return win.fail("the window took no mouse event");
            win.next("sub");
            return;
        }
        case "sub": {
            if (!page.muteSub.opened) return win.later("the sub-menu opens");
            if (page.muteSub.count !== 6) return win.fail("the opened sub-menu has " + page.muteSub.count + " entries");
            const texts = [];
            for (let i = 0; i < page.muteSub.count; ++i) {
                const item = page.muteSub.itemAt(i);
                if (!item || !item.visible || !item.enabled) return win.fail("entry " + i + " of the sub-menu is not usable");
                texts.push(item.text);
            }
            if (texts.join("|") !== "1 Hour|8 Hours|1 Day|1 Week|Until…|Always") return win.fail("the sub-menu's entries or their order: " + texts.length);
            win.shot("sub" + win.round);
            page.rowMenu.close();
            win.next("closed");
            return;
        }
        case "closed": {
            if (page.rowMenu.visible || page.muteSub.visible) return win.later("the menus close");
            win.round += 1;
            win.next("open");
            return;
        }
        }
    }

    Timer {
        interval: 90000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

/// The cases written as programs (`roaming_ui_prog.qml`).
const PROG_CASES: &[&str] = &[
    "menu-on",
    "menu-off",
    "menu-off-narrow",
    "menu-logged-out",
    "menu-refused",
    "menu-refused-pin",
    "menu-hidden",
    "menu-read",
    "note-to-self",
    "chat-menu",
    "chat-archived",
    "keys",
    "keys-narrow",
    "keys-off",
    "undo",
    "notices",
    "undo-pinned",
    "undo-two",
    "undo-late",
    "mute",
    "unmute",
    "until",
    "until-keys",
    "until-past",
    "pin-limit",
    "blended",
    "settings-switch",
    "settings-refused",
];

const PROG_SCRIPT: &str = include_str!("roaming_ui_prog.qml");

fn options(case: &str) -> FixtureOptions {
    let base = FixtureOptions {
        messages_per_conversation: 6,
        ..FixtureOptions::default()
    };
    match case {
        "menu-again" | "until-opens" | "note-to-self" => FixtureOptions {
            note_to_self: true,
            ..base
        },
        "menu-logged-out" => FixtureOptions {
            state: FixtureState::LoggedOut,
            ..base
        },
        "menu-refused" | "menu-refused-pin" | "settings-refused" => FixtureOptions {
            reject_changes: true,
            ..base
        },
        "menu-hidden" => FixtureOptions {
            network: FixtureNetwork::WhatsApp,
            ..base
        },
        "blended" => FixtureOptions {
            accounts: 2,
            ..base
        },
        c if PROG_CASES.contains(&c) => base,
        other => panic!("unknown case {other}"),
    }
}

fn run(case: &str) -> i32 {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(options(case)),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    // The listener: what the fake network heard (`GET /heard?name=<command>` answers the count)
    // and the core's clock (`GET /now`, milliseconds).
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let port = listener.local_addr().expect("its address").port();
    let heard = handle.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 1024];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            let target = request
                .strip_prefix("GET ")
                .and_then(|rest| rest.split_whitespace().next())
                .unwrap_or("");
            let body = if let Some(name) = target.strip_prefix("/heard?name=") {
                heard
                    .fixture_commands()
                    .iter()
                    .filter(|c| **c == name)
                    .count()
                    .to_string()
            } else if target == "/now" {
                heard.core_now_ms().to_string()
            } else {
                String::new()
            };
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });
    let fixture_now = handle.fixture().map_or(0, |f| f.now_ms);
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext { app: handle });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let timeout = if case == "until-past" {
        170_000
    } else {
        90_000
    };
    let width = if case.ends_with("-narrow") { 480 } else { 1100 };
    let script = if PROG_CASES.contains(&case) {
        PROG_SCRIPT
            .replace("FIXTURE_NOW", &fixture_now.to_string())
            .replace("TIMEOUT", &timeout.to_string())
            .replace("WIDTH", &width.to_string())
            .replace("PORT", &port.to_string())
    } else {
        SCRIPT.to_owned()
    }
    .replace("CASE", case)
    .replace(
        "SHOTS_DIR",
        &std::env::var("KOURIER_ROAMING_SHOTS").unwrap_or_default(),
    );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        // Kirigami Add-ons call i18n*() (tests/kde.rs).
        kourier_qt::l10n::setup_engine(engine.as_mut());
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/roaming_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    code
}

fn main() {
    support::answer_list("roaming_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_ROAMING_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_ROAMING_ONLY").unwrap_or_default();
    let cases: Vec<&str> = ["menu-again", "until-opens"]
        .into_iter()
        .chain(PROG_CASES.iter().copied())
        .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
        .collect();
    let mut failed = 0;
    for case in &cases {
        let mut child = Command::new(&exe);
        child.env("KOURIER_ROAMING_UI_CASE", case);
        if *case == "until-past" {
            // The UI's clock runs with the core's (a debug build's knob): time passes under the dialog.
            child.env("KOURIER_DEV_FIXTURE_CLOCK", "run");
        }
        let out = child.output().expect("run a case");
        let said = String::from_utf8_lossy(&out.stderr);
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{said}");
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        if out.status.success() && errors.is_empty() {
            println!("test roaming: {case} ... ok");
        } else {
            println!("test roaming: {case} ... FAILED ({})", out.status);
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
