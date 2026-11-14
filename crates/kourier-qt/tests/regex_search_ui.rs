//! RegExp search and Save as Filter in the real main window and the real core over the fake
//! network (offscreen, `harness = false`; RegExp filters R5).
//!
//! - The chat list's search field has the `.*` toggle ("Regular Expression") and `Aa` ("Match
//!   Case", greyed until `.*` is on), each with an accessible name.
//! - With `.*` on, an invalid pattern shows the viewmodel's sentence with its place ("at
//!   character") under the field, quotes nothing, asks no search and offers no "Save as Filter".
//! - A valid pattern clears the line, is searched (the page answers with hits), filters the chats'
//!   titles with the core's compiled pattern (fewer chats than before), and offers "Save as
//!   Filter".
//! - "Save as Filter" opens the filter dialog with the pattern in Message text and a name made
//!   from it; Save stores it (the core holds the filter, the dialog closes).
//!
//! Nothing prints a pattern or a name.

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{Query, QueryData};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

MainWindow {
    id: win

    width: 520
    height: 900
    visible: true

    property string step: "start"
    property int waits: 0
    property int quiet: 0
    property int chatsBefore: 0
    readonly property string good: "\\bgard\\w+"

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

    function page(): var {
        return win.pageStack.get(0);
    }

    function child(name: string): Item {
        return win.findIn(win.page(), name);
    }

    function findIn(root: Item, name: string): Item {
        if (root.objectName === name) {
            return root;
        }
        for (const c of root.children) {
            const hit = win.findIn(c, name);
            if (hit) {
                return hit;
            }
        }
        return null;
    }

    Timer {
        interval: 40
        repeat: true
        running: true
        property bool busy: false
        onTriggered: {
            if (busy) {
                return;
            }
            busy = true;
            try {
                win.tick();
            } finally {
                busy = false;
            }
        }
    }

    function tick(): void {
        const p = win.page();
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts || !p || !p.rowsShown || p.conversations.count === 0) return win.later("the list");
            win.chatsBefore = p.conversations.count;
            win.next("buttons");
            return;
        case "buttons": {
            const rb = p.regexButton;
            const mc = p.matchCaseButton;
            if (rb.text !== ".*" || rb.Accessible.name !== "Regular Expression") return win.fail("the .* button: " + rb.text + " / " + rb.Accessible.name);
            if (mc.text !== "Aa" || mc.Accessible.name !== "Match Case") return win.fail("the Aa button");
            if (mc.enabled) return win.fail("Aa is on before .*");
            if (p.saveFilterButton.visible) return win.fail("Save as Filter with nothing typed");
            rb.toggle();
            rb.toggled();
            win.next("regex on");
            return;
        }
        case "regex on":
            if (!p.regexOn || !p.matchCaseButton.enabled) return win.later("the toggle");
            if (p.searchField.placeholderText !== "Search by regular expression") return win.fail("the placeholder: " + p.searchField.placeholderText);
            p.searchField.text = "(open";
            win.next("invalid");
            return;
        case "invalid": {
            const e = win.child("search-regex-error");
            if (!e || !e.visible || e.text.indexOf("at character") < 0) return win.later("the error line");
            if (e.text.indexOf("open") >= 0) return win.fail("the error quotes the pattern");
            if (p.saveFilterButton.visible) return win.fail("Save as Filter for a bad pattern");
            win.quiet += 1;
            if (win.quiet < 15) return;
            if (App.searchResults.active) return win.fail("a bad pattern was searched");
            p.searchField.text = win.good;
            win.next("valid");
            return;
        }
        case "valid": {
            const e = win.child("search-regex-error");
            if (e && e.visible) return win.later("the error line goes: " + e.text);
            const r = App.searchResults;
            if (!r.active || !r.answered || r.loading || r.count === 0) return win.later("the hits of the pattern");
            if (!p.saveFilterButton.visible) return win.fail("no Save as Filter for a valid pattern");
            // The chat titles follow the core's pattern: only the matching chats are listed.
            if (p.conversations.count === 0 || p.conversations.count >= win.chatsBefore) return win.later("the titles: " + p.conversations.count + " of " + win.chatsBefore);
            p.saveFilterButton.clicked();
            win.next("dialog");
            return;
        }
        case "dialog": {
            const d = p.saveFilterDialog;
            if (!d.opened) return win.later("the dialog opens");
            if (d.textField.text !== win.good) return win.fail("the pattern is not in Message text");
            if (d.nameField.text === "") return win.fail("no name made from the pattern");
            if (!d.valid) return win.fail("Save is off for a valid prefill");
            d.saveAction.trigger();
            win.next("saved");
            return;
        }
        case "saved":
            if (App.filters.count !== 1) return win.later("the saved filter");
            if (p.saveFilterDialog.opened) return win.later("the dialog closes");
            win.quiet += 1;
            if (win.quiet < 10) return;
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 60000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn main() {
    support::answer_list("regex_search_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 5,
            search_rows: true,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(SCRIPT),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/regex_search_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the RegExp search ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    // The core holds the filter "Save as Filter" made.
    #[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
    let list = match handle.runtime().block_on(handle.query(Query::Filters)) {
        Ok(QueryData::Filters(list)) => list,
        _ => panic!("the filters the core holds"),
    };
    assert_eq!(list.filters.len(), 1, "one filter was saved");
    assert_eq!(
        list.filters[0]
            .spec
            .text_pattern
            .as_ref()
            .map(|p| p.expose().as_str()),
        Some(r"\bgard\w+")
    );
    app.shutdown();
    println!("test the RegExp search ... ok");
    println!("\ntest result: ok. 1 passed; 0 failed");
}
