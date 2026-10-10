//! The settings page's choice for disappearing messages while the clock is unconfirmed,
//! through the real main window and the real core over the fake network (offscreen,
//! `harness = false`).
//!
//! The fixture account ends with a clock the network calls wrong, so nothing confirms the
//! time, and its chat with a timer holds one message whose time is up a few seconds into
//! the run. What the timeline shows of that message then follows the choice:
//!
//! - Nothing is read for the settings page before it is shown; then it shows the option the
//!   core holds (the default: hide), as three radio buttons with a sentence under each.
//! - By default the message is hidden once its time is up.
//! - "Keep until the time is confirmed", chosen with the keyboard (Space on the option):
//!   the core has the option at once and the message is back in the timeline.
//! - "Hide now, erase once the time is confirmed": it is hidden again.
//! - "Erase by this computer's clock": it is erased, and choosing "keep" afterwards does not
//!   bring it back.
//! - "Round avatars" is on; switching it off makes the avatars on screen ask the image
//!   provider for squares with the theme's corner at once, switching it on for circles again;
//!   the preferences hold the last choice.
//!
//! The core is asked directly at the end: the option it holds, and what its timeline has.

use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{ExpiryPolicy, Query, QueryData, TimelineAnchor, TimelineQuery};
use kourier_app::{App, EXPIRING_TEXT, FixtureOptions, FixtureState, Launch, StartOptions};
use kourier_model::{Body, ConversationId};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

// The app's own main window is the root (narrow: one page at a time).
MainWindow {
    id: win

    width: 520
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property int quiet: 0
    // The avatar the "Round avatars" steps watch, and what it asked for while round.
    property var avatar: null
    property string roundSource: ""
    property int roundRadius: 0

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
        win.quiet = 0;
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

    // The radio button of an option, by the core's name for it.
    function option(name: string): Item {
        return win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "expiry-" + name) : null;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property var blocks
        }
    }

    // Whether the open timeline has a message with `text`.
    function shows(text: string): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r || r.kind !== "message") {
                continue;
            }
            for (const b of r.blocks) {
                if (b.type === "text" && b.plain === text) {
                    return true;
                }
            }
        }
        return false;
    }

    // Exactly `name` is the checked option.
    function onlyChecked(name: string): bool {
        for (const other of ["hide", "keep", "erase"]) {
            if (win.option(other).checked !== (other === name)) {
                return false;
            }
        }
        return true;
    }

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
        {
            switch (win.step) {
            case "start":
                if (!win.started || !App.hasAccounts) return win.later("the start page");
                // Nothing was read for the settings page: it does not exist yet.
                if (win.settingsPage !== null) return win.fail("the settings page exists at the start");
                if (App.settings.expiryPolicy !== "") return win.fail("the option was read before the page was shown: " + App.settings.expiryPolicy);
                win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "settings_ui_events.qml");
                win.openConversation(TIMER);
                win.next("chat");
                return;
            case "chat":
                if (App.timeline.conversationId !== TIMER || App.timeline.loading || !win.shows("This one is gone in a week.")) return win.later("the chat with the timer");
                win.next("hidden");
                return;
            case "hidden":
                // The default: once its time is up, the message is hidden (the clock is
                // unconfirmed: the network calls it wrong).
                if (win.shows(GONE)) return win.later("the message whose time is up is hidden");
                if (!win.shows("This one is gone in a week.")) return win.fail("only the message whose time is up goes");
                win.openSettings();
                win.next("page");
                return;
            case "page": {
                if (win.settingsPage === null || App.settings.expiryPolicy === "") return win.later("the settings page with the core's answer");
                if (App.settings.expiryPolicy !== "hide") return win.fail("the default: " + App.settings.expiryPolicy);
                const hide = win.option("hide");
                const keep = win.option("keep");
                const erase = win.option("erase");
                if (!hide || !keep || !erase) return win.fail("three options");
                if (!win.onlyChecked("hide")) return win.fail("the default is the checked option");
                // What each says, and one sentence under each.
                if (hide.text !== "Hide now, erase once the time is confirmed" || keep.text !== "Keep until the time is confirmed" || erase.text !== "Erase by this computer's clock") return win.fail("the labels: " + hide.text + " / " + keep.text + " / " + erase.text);
                for (const o of [hide, keep, erase]) {
                    if (!o.enabled) return win.fail("an option cannot be chosen: " + o.text);
                    if (o.description.length < 40 || o.description.indexOf(".") !== o.description.length - 1) return win.fail("one sentence under " + o.text + ": " + o.description);
                }
                // ---- chosen with the keyboard ----
                win.settingsPage.showExpiry();
                keep.forceActiveFocus();
                if (!keep.activeFocus) return win.fail("the option takes the keyboard");
                if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
                win.next("keep");
                return;
            }
            case "keep":
                // Applied at once: no button to confirm it.
                if (App.settings.expiryPolicy !== "keep" || !win.onlyChecked("keep")) return win.later("the option is keep");
                // The timeline follows: the message is readable again, as it was.
                if (!win.shows(GONE)) return win.later("the message is shown again");
                win.option("hide").click();
                win.next("hide");
                return;
            case "hide":
                if (App.settings.expiryPolicy !== "hide" || !win.onlyChecked("hide")) return win.later("the option is hide");
                if (win.shows(GONE)) return win.later("the message is hidden again");
                win.option("erase").click();
                win.next("erase");
                return;
            case "erase":
                if (App.settings.expiryPolicy !== "erase" || !win.onlyChecked("erase")) return win.later("the option is erase");
                if (win.shows(GONE)) return win.fail("an erased message shows");
                win.option("keep").click();
                win.next("erased");
                return;
            case "erased":
                if (App.settings.expiryPolicy !== "keep" || !win.onlyChecked("keep")) return win.later("the option is keep again");
                // Erased is erased: it does not come back with "keep".
                if (win.shows(GONE)) return win.fail("an erased message came back");
                win.quiet += 1;
                if (win.quiet < 40) return;
                if (!win.shows("This one is gone in a week.")) return win.fail("the other messages are untouched");
                win.next("avatars");
                return;
            case "avatars": {
                // "Round avatars": on by default, and every avatar on screen is a circle.
                const sw = win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "round-avatars") : null;
                win.avatar = win.find(win.contentItem, i => i.pictureRadius !== undefined && i.source !== undefined && i.source !== "");
                if (!sw || !win.avatar) return win.later("the switch and an avatar with a picture");
                if (sw.text !== "Round avatars") return win.fail("the label: " + sw.text);
                if (!sw.checked || !App.settings.roundAvatars) return win.fail("avatars are round by default");
                if (win.avatar.cornerRadius !== win.avatar.side / 2) return win.fail("a round avatar has half its side as corner: " + win.avatar.cornerRadius);
                // The provider is asked for a circle (a radius of half the side or more).
                if (win.avatar.pictureSource.indexOf("&r=") < 0 && win.avatar.pictureSource.indexOf("?r=") < 0) return win.fail("the request has no radius: " + win.avatar.pictureSource);
                win.roundSource = win.avatar.pictureSource;
                win.roundRadius = win.avatar.pictureRadius;
                sw.click();
                win.next("square");
                return;
            }
            case "square": {
                const sw = win.find(win.settingsPage, i => i.objectName === "round-avatars");
                // At once, in the open window: the avatars on screen are asked for again, with
                // the theme's corner and not a circle.
                if (App.settings.roundAvatars || sw.checked || win.avatar.pictureSource === win.roundSource) return win.later("avatars are not round");
                if (!(win.avatar.cornerRadius < win.avatar.side / 2)) return win.fail("a square avatar keeps a corner of its own: " + win.avatar.cornerRadius);
                if (!(win.avatar.pictureRadius < win.roundRadius)) return win.fail("the request still asks for a circle: " + win.avatar.pictureSource);
                sw.click();
                win.next("round again");
                return;
            }
            case "round again":
                if (!App.settings.roundAvatars || win.avatar.pictureSource !== win.roundSource) return win.later("avatars are round again");
                // Left off for the core's check at the end.
                win.find(win.settingsPage, i => i.objectName === "round-avatars").click();
                win.next("off");
                return;
            case "off":
                if (App.settings.roundAvatars) return win.later("avatars are not round");
                win.quiet += 1;
                if (win.quiet < 10) return;
                Qt.exit(0);
                return;
            }
        }
    }

    Timer {
        interval: 180000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

/// The texts of the timer chat as the core has them.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn texts(app: &kourier_app::AppHandle, conv: ConversationId) -> Vec<String> {
    let page = app
        .runtime()
        .block_on(app.query(Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 200,
        })));
    let Ok(QueryData::Timeline(page)) = page else {
        panic!("the timeline of the chat");
    };
    page.rows
        .iter()
        .filter_map(|r| match &r.body {
            Body::Text(t) => Some(t.body.clone()),
            _ => None,
        })
        .collect()
}

#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn policy(app: &kourier_app::AppHandle) -> ExpiryPolicy {
    match app.runtime().block_on(app.query(Query::ExpiryPolicy)) {
        Ok(QueryData::ExpiryPolicy(policy)) => policy,
        _ => panic!("the option the core holds"),
    }
}

fn main() {
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            // Nothing confirms the time: the network calls this computer's clock wrong.
            state: FixtureState::ClockSkew,
            // One message of the timer chat is due a few seconds into the run.
            expiring: Some(Duration::from_secs(3)),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let timer = fixture.timer_conversation.expect("the timer chat");
    assert_eq!(
        policy(&handle),
        ExpiryPolicy::HideUntilConfirmed,
        "the default"
    );

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT
        .replace("GONE", &format!("{EXPIRING_TEXT:?}"))
        .replace("TIMER", &timer.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/settings_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the choice for disappearing messages ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }

    // The preferences hold the last choice of "Round avatars" (round, square, round, square).
    assert!(!handle.desktop().prefs().round_avatars);
    // The core holds what the page chose last, and the erased message is gone from it.
    assert_eq!(policy(&handle), ExpiryPolicy::KeepUntilConfirmed);
    let left = texts(&handle, timer);
    assert!(
        !left.iter().any(|t| t == EXPIRING_TEXT),
        "the message was erased under \"erase\" and stays erased"
    );
    assert!(
        left.iter().any(|t| t == "This one is gone in a week."),
        "the message whose time is not up is kept"
    );

    app.shutdown();
    println!(
        "test the choice for disappearing messages ... ok\n\ntest result: ok. 1 passed; 0 failed"
    );
}
