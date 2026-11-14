//! The RegExp filters of Settings and the filter dialog, through the real main window and the
//! real core over the fake network (offscreen, `harness = false`; RegExp filters R2).
//!
//! - The settings page asks for the filters when it is shown: no filter yet, so the page says
//!   what a filter does ("No filters yet") and offers "Add Filter…".
//! - "Add Filter…" opens the dialog: an invalid pattern in Message text shows its sentence with
//!   "at character" under the field, and Save is disabled; fixing it enables Save (the name is
//!   filled, the two other fields are empty and not used); Save closes the dialog and the row
//!   appears in the list, with the fields it uses.
//! - A second filter; both rows; "Move Down" of the first makes the second the first row; the
//!   core's list holds the new order.
//! - A save the core refuses (a pattern too large for the compile limit, which the live check
//!   cannot know) keeps the dialog open and says so under the field.
//! - Delete asks first; confirming removes the row, and the core holds one filter.
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
    property var firstId: 0

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

    function named(name: string): Item {
        return win.settingsPage ? win.find(win.settingsPage, i => i.objectName === name) : null;
    }

    // The name of the filter in row `i` (its label), "" when there is no such row.
    function rowName(i: int): string {
        const row = win.named("filter-row-" + i);
        if (!row) {
            return "";
        }
        return row.name;
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
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            win.openSettings();
            win.next("page");
            return;
        case "page": {
            if (win.settingsPage === null || !App.filters.loaded) return win.later("the page and the core's answer");
            const empty = win.named("filters-empty");
            if (!empty || !empty.visible) return win.later("the empty sentence");
            if (App.filters.count !== 0) return win.fail("a filter in a new profile");
            if (empty.text !== "No filters yet") return win.fail("the empty text: " + empty.text);
            const add = win.named("filter-add");
            if (!add || add.text !== "Add Filter…") return win.fail("the Add Filter… row");
            add.clicked();
            win.next("dialog");
            return;
        }
        case "dialog": {
            const d = win.settingsPage.filterDialogItem;
            if (!d.opened) return win.later("the dialog opens");
            if (d.title !== "Add Filter") return win.fail("the title: " + d.title);
            if (d.valid || d.saveAction.enabled) return win.fail("Save is on for an empty dialog");
            d.nameField.text = "Invoices";
            d.textField.text = "(open";
            win.next("invalid");
            return;
        }
        case "invalid": {
            const d = win.settingsPage.filterDialogItem;
            if (d.textSaid.indexOf("at character 1") < 0) return win.later("the message with its place: " + d.textSaid);
            if (d.textSaid.indexOf("open") >= 0) return win.fail("the message quotes the pattern");
            if (d.valid || d.saveAction.enabled) return win.fail("Save is on while a pattern is invalid");
            if (d.senderSaid !== "" || d.chatSaid !== "") return win.fail("an empty field says something");
            d.textField.text = "invoice|receipt";
            win.next("valid");
            return;
        }
        case "valid": {
            const d = win.settingsPage.filterDialogItem;
            if (d.textSaid !== "") return win.later("the message goes");
            if (!d.valid || !d.saveAction.enabled) return win.later("Save is on");
            d.saveAction.trigger();
            win.next("saved");
            return;
        }
        case "saved": {
            const d = win.settingsPage.filterDialogItem;
            if (App.filters.count !== 1 || d.opened) return win.later("the row appears and the dialog closes");
            if (win.rowName(0) !== "Invoices") return win.later("the row's name: " + win.rowName(0));
            const row = win.named("filter-row-0");
            if (!row.visible) return win.fail("the row is hidden");
            if (win.named("filters-empty").visible) return win.fail("the empty sentence stays");
            const dot = win.find(row, i => i.objectName === "filter-dot");
            if (!dot || !dot.visible) return win.fail("Highlight is on for a new filter: it has its colour dot");
            const sw = win.named("filter-notify-0");
            if (!sw || sw.checked) return win.fail("Notify is off for a new filter");
            win.firstId = row.filterId;
            // The second filter, from the dialog's helper (the same path as its Save).
            win.settingsPage.filterDialogItem.openFor(0);
            win.next("second dialog");
            return;
        }
        case "second dialog": {
            const d = win.settingsPage.filterDialogItem;
            if (!d.opened) return win.later("the dialog opens again");
            d.nameField.text = "Second";
            d.senderField.text = "^Alex";
            win.next("second valid");
            return;
        }
        case "second valid": {
            const d = win.settingsPage.filterDialogItem;
            if (!d.valid) return win.later("Save is on");
            // The first filter's colour is used: the second starts with the next swatch.
            if (d.colour === "#ffd60a") return win.fail("the new filter took a colour that is in use");
            d.saveAction.trigger();
            win.next("two rows");
            return;
        }
        case "two rows":
            if (App.filters.count !== 2) return win.later("two rows");
            if (win.rowName(0) !== "Invoices" || win.rowName(1) !== "Second") return win.later("the order: " + win.rowName(0) + ", " + win.rowName(1));
            if (!win.named("filter-row-0") || win.named("filter-row-0").isFirst !== true) return win.later("the first row is the first");
            win.quiet += 1;
            if (win.quiet < 5) return;
            App.filters.moveBy(win.firstId, 1);
            win.next("moved");
            return;
        case "moved":
            if (win.rowName(0) !== "Second" || win.rowName(1) !== "Invoices") return win.later("Move Down: " + win.rowName(0) + ", " + win.rowName(1));
            win.next("refused");
            win.settingsPage.filterDialogItem.openFor(0);
            return;
        case "refused": {
            const d = win.settingsPage.filterDialogItem;
            if (!d.opened) return win.later("the dialog opens for a large pattern");
            d.nameField.text = "Large";
            // Parses, but the compiled program is over the size limit: only the core knows.
            d.textField.text = "\\pL{300}";
            win.next("refused wait");
            return;
        }
        case "refused wait": {
            const d = win.settingsPage.filterDialogItem;
            if (!d.valid) return win.later("the live check lets a pattern that parses through");
            d.saveAction.trigger();
            win.next("refused said");
            return;
        }
        case "refused said": {
            const d = win.settingsPage.filterDialogItem;
            if (d.refusalField !== "text" || d.refusalText === "") return win.later("the core's refusal under the field: " + d.refusalField);
            if (d.textSaid !== d.refusalText) return win.fail("the refusal shows under the field");
            if (!d.opened) return win.fail("the dialog closed on a refusal");
            if (App.filters.count !== 2) return win.fail("a refused filter was added");
            d.customButton.clicked();
            win.next("custom loads");
            return;
        }
        // The system's color chooser loads on the offscreen platform and its choice reaches the
        // dialog as `#rrggbb` (R2's leftover: an alpha colour used to come out as `#aarrggbb` cut).
        case "custom loads": {
            const d = win.settingsPage.filterDialogItem;
            const l = d.colourPicker;
            if (l.status === Loader.Error) return win.fail("the color chooser did not load");
            if (l.status !== Loader.Ready || !l.item || !l.item.visible) return win.later("the chooser opens");
            l.item.selectedColor = Qt.rgba(0.2, 0.4, 0.6, 0.5);
            l.item.accept();
            win.next("custom chosen");
            return;
        }
        case "custom chosen": {
            const d = win.settingsPage.filterDialogItem;
            if (d.colour !== "#336699") return win.later("the dialog's colour: " + d.colour);
            if (!d.opened) return win.fail("the filter dialog closed with the chooser");
            d.close();
            win.next("delete");
            return;
        }
        case "delete": {
            const d = win.settingsPage.filterDeleteItem;
            d.filterId = win.firstId;
            d.filterName = "Invoices";
            d.open();
            win.next("delete asked");
            return;
        }
        case "delete asked": {
            const d = win.settingsPage.filterDeleteItem;
            if (!d.opened) return win.later("the question");
            if (App.filters.count !== 2) return win.fail("the filter went before the answer");
            d.customFooterActions[0].trigger();
            win.next("deleted");
            return;
        }
        case "deleted":
            if (App.filters.count !== 1) return win.later("one row left");
            if (win.rowName(0) !== "Second") return win.later("the remaining row: " + win.rowName(0));
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
    support::answer_list("filters_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 5,
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
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/filters_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the filters page ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    // The core holds what the page left: the second filter only.
    #[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
    let list = match handle.runtime().block_on(handle.query(Query::Filters)) {
        Ok(QueryData::Filters(list)) => list,
        _ => panic!("the filters the core holds"),
    };
    assert_eq!(list.filters.len(), 1, "one filter is left");
    assert_eq!(
        list.filters[0]
            .spec
            .sender_pattern
            .as_ref()
            .map(|p| p.expose().as_str()),
        Some("^Alex")
    );
    app.shutdown();
    println!("test the filters page ... ok");
    println!("\ntest result: ok. 1 passed; 0 failed");
}
