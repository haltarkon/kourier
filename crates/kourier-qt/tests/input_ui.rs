//! Message actions by keyboard and pointer (S13 UI, §8.3), with real input events on the app's
//! own timeline and composer over the real core on the fake network: Up in an empty composer
//! edits the newest own message and Enter saves it, Up in a composer with text only moves the
//! cursor, Escape leaves edit mode and drops a reply, a right click on a bubble opens its
//! context menu whose entries act on that message, the timeline takes the keyboard focus (Up
//! and Down move between messages, the Menu key, Shift+F10 and Enter open the menu of the
//! current one, Escape closes it and then returns to the composer; the timeline is one tab stop:
//! Tab leaves it and Shift+Tab comes straight back), the toolbar under the pointer opens the
//! reaction picker and a click in it reacts, a click on the reaction's chip takes our
//! reaction back, a click on a quote jumps to its original, a click into the text of an older
//! message leaves the view where it is (no row becomes current; the next arrow key starts at the
//! clicked one), and "Jump to Latest" shows over the view away from the newest message and
//! returns to it.
//!
//! The events come from Qt's own test helper (`TestEvent` of the `QtTest` QML module, Debian
//! package `qml6-module-qttest`), delivered to the window like the platform's. Offscreen,
//! `harness = false`.

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

// The window is the root: the timeline needs a shown window to lay its rows out.
Window {
    id: test

    width: 560
    height: 720
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property real target: 0
    property string original: ""
    property real other: 0
    property real jumpedTo: 0
    property real quoted: 0
    property real chipAt: -1
    property real awayBy: 0
    // Where the view was when it was last seen to move, and for how many ticks it has not.
    property real restY: 0
    property int restTicks: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    // Waits a few ticks for something that settles a moment after its cause (a layout pass,
    // a popup's transition); fails with `what` when it never does.
    function later(what: string): void {
        test.waits += 1;
        // A liveness limit, not a time budget: a minute of ticks. (It was 7.5 s, and the
        // whole test had 60 s: on a machine busy with other builds the fixture's pictures
        // alone can take longer to arrive.)
        if (test.waits > 2000) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        test.waits = 0;
        test.step = step;
    }

    function key(k: int, modifiers: int): void {
        if (!test.ev.keyClick(k, modifiers, -1)) {
            fail("the key event was not delivered");
        }
    }

    function click(item: Item, x: real, y: real, button: int): void {
        if (!test.ev.mouseClick(item, x, y, button, Qt.NoModifier, -1)) {
            fail("the mouse event was not delivered");
        }
    }

    function row(id: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.messageId === id) {
                return r;
            }
        }
        return null;
    }

    function textOf(r: var): string {
        for (const b of r.blocks) {
            if (b.type === "text") {
                return b.plain;
            }
        }
        return "";
    }

    // The first visual descendant of `item` that `fits`.
    function find(item: Item, fits: var): Item {
        if (!item) {
            return null;
        }
        for (let i = 0; i < item.children.length; ++i) {
            const child = item.children[i];
            if (fits(child)) {
                return child;
            }
            const deeper = test.find(child, fits);
            if (deeper) {
                return deeper;
            }
        }
        return null;
    }

    // The timeline's context menu.
    function menu(): var {
        return view.messageMenu;
    }

    // The visible entry of the open menu with this text.
    function entry(text: string): Item {
        const m = test.menu();
        if (!m) {
            return null;
        }
        for (let i = 0; i < m.count; ++i) {
            const item = m.itemAt(i);
            if (item && item.visible && item.text === text) {
                return item;
            }
        }
        return null;
    }

    // The text area has the keyboard focus.
    function typing(): bool {
        const item = test.activeFocusItem;
        return item !== null && item.placeholderText !== undefined && item.cursorPosition !== undefined;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property bool own
            required property var blocks
            required property var reactions
            required property bool edited
            required property var actions
        }
    }

    Connections {
        target: App.timeline
        function onJumpRequested(message: real, row: int): void {
            test.jumpedTo = message;
        }
    }

    Item {
        id: holder
    }

    // The app's own timeline and composer, as the chat page holds them.
    TimelineView {
        id: view
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.bottom: composerItem.top
        onComposerRequested: composerItem.focusInput()
    }
    Composer {
        id: composerItem
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
    }

    // An input event can spin the event loop while it is delivered: the steps never nest.
    property bool busy: false
    Timer {
        interval: 30
        repeat: true
        running: true
        onTriggered: {
            if (test.busy) {
                return;
            }
            test.busy = true;
            try {
                test.tick();
            } finally {
                test.busy = false;
            }
        }
    }

    function tick(): void {
        {
            switch (test.step) {
            case "start":
                if (!App.loaded || !App.hasAccounts) return;
                try {
                    test.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", holder, "input_ui_events.qml");
                } catch (e) {
                    return fail("the QtTest QML module is needed for input events (package qml6-module-qttest): " + e);
                }
                App.ready();
                App.openConversation(QUOTES);
                test.requestActivate();
                return next("open");
            case "open": {
                if (App.timeline.loading || rows.count < 6) return;
                test.target = App.findMessage("editable");
                if (test.target <= 0) return fail("no own message that can be edited");
                test.original = test.textOf(test.row(test.target));
                if (test.original === "") return fail("its text");
                // The rows are laid out once the window is shown.
                if (!view.delegateFor(test.target)) return later("the row has a delegate");
                // A chat opens at its newest message: nothing to jump to.
                if (!view.atBottom) return later("the view rests at the newest message");
                const toLatest = test.find(view, c => c.text === "Jump to Latest");
                if (!toLatest) return fail("the Jump to Latest button exists");
                if (toLatest.visible) return later("no Jump to Latest at the newest message");
                composerItem.focusInput();
                return next("up");
            }

            // ---- the composer's keys ----
            case "up": {
                if (!test.typing()) {
                    composerItem.focusInput();
                    return later("the text area takes the keyboard focus");
                }
                // Up in an empty composer: the newest own message is edited.
                test.key(Qt.Key_Up, Qt.NoModifier);
                if (!composerItem.editing || App.composer.editId !== test.target) return fail("Up did not start the edit");
                if (composerItem.text !== test.original) return fail("the composer holds the message: " + composerItem.text);
                // Up again moves the cursor: it is no second edit, and the text stays.
                test.key(Qt.Key_Up, Qt.NoModifier);
                if (!composerItem.editing || composerItem.text !== test.original) return fail("Up inside an edit");
                // Escape leaves edit mode; the composer is empty again.
                test.key(Qt.Key_Escape, Qt.NoModifier);
                if (composerItem.editing || composerItem.text !== "") return fail("Escape did not leave the edit");
                // With text in the composer Up belongs to the cursor.
                test.ev.keyClickChar("d", Qt.NoModifier, -1);
                if (composerItem.text !== "d") return fail("typing reaches the text area: " + composerItem.text);
                test.key(Qt.Key_Up, Qt.NoModifier);
                if (composerItem.editing) return fail("Up with a draft started an edit");
                composerItem.setText("");
                // Escape drops a reply, and then is nobody's.
                if (!App.composer.replyTo(QUOTES, test.target) || !composerItem.replying) return fail("replyTo");
                test.key(Qt.Key_Escape, Qt.NoModifier);
                if (composerItem.replying) return fail("Escape did not drop the reply");
                // Edit by keyboard alone: Up, a character, Enter.
                test.key(Qt.Key_Up, Qt.NoModifier);
                if (!composerItem.editing) return fail("Up after Escape");
                test.ev.keyClickChar("!", Qt.NoModifier, -1);
                if (composerItem.text !== test.original + "!") return fail("typed into the edit: " + composerItem.text);
                test.key(Qt.Key_Return, Qt.NoModifier);
                if (composerItem.editing || composerItem.text !== "") return fail("Enter did not save the edit");
                return next("edited");
            }
            case "edited": {
                const r = test.row(test.target);
                if (!r || test.textOf(r) !== test.original + "!" || !r.edited) return later("the edited text shows");
                return next("rightclick");
            }

            // ---- the pointer: the context menu and its entries ----
            case "rightclick": {
                const d = view.delegateFor(test.target);
                if (!d) return later("the edited row has a delegate");
                if (view.popupOpen) return fail("a popup is open already");
                // An own bubble ends at the trailing margin.
                test.click(d, d.width - d.sideMargin - 12, d.height / 2, Qt.RightButton);
                return next("menu");
            }
            case "menu": {
                if (!view.popupOpen) return later("the right click opens the context menu");
                // (Its entries have their places once it has finished opening.)
                if (!view.messageMenu.opened) return later("the menu has finished opening");
                if (TimelineActions.activeMessage !== test.target) return fail("the menu is about the clicked message");
                // What the core offers for a recent own text, and nothing else.
                for (const text of ["Add Reaction…", "Reply", "Edit", "Copy Text", "Delete…"]) {
                    if (!test.entry(text)) return later("the entry " + text);
                }
                for (const text of ["Send Again", "Cancel Sending", "Show Reactions"]) {
                    if (test.entry(text)) return fail("an entry that does not apply: " + text);
                }
                const reply = test.entry("Reply");
                test.click(reply, reply.width / 2, reply.height / 2, Qt.LeftButton);
                return next("menureply");
            }
            case "menureply":
                if (view.popupOpen) return later("the menu closes with its entry");
                if (!composerItem.replying || App.composer.replyToId !== test.target) return fail("Reply from the menu");
                if (!test.typing()) return later("a reply moves the keyboard focus to the text area");
                test.key(Qt.Key_Escape, Qt.NoModifier);
                if (composerItem.replying) return fail("Escape after the menu's Reply");
                return next("focus");

            // ---- the timeline's keys ----
            case "focus": {
                // Shift+Tab from the text area walks back to the timeline: past the attach
                // button, and no further (no row holds a tab stop of its own).
                if (!view.rowList.activeFocus) {
                    if (test.waits > 3) return fail("Shift+Tab does not reach the timeline in two steps");
                    test.key(Qt.Key_Backtab, Qt.ShiftModifier);
                    return later("the timeline takes the keyboard focus");
                }
                const newest = view.rowList.currentItem;
                if (!newest || !newest.current) return later("the newest message is current once Tab reached the timeline");
                if (view.rowList.currentIndex !== 0 && view.rowList.itemAtIndex(0) instanceof MessageDelegate) return fail("the newest message, not another: " + view.rowList.currentIndex);
                test.other = newest.messageId;
                test.key(Qt.Key_Menu, Qt.NoModifier);
                return next("menukey");
            }
            case "menukey":
                if (!view.popupOpen) return later("the Menu key opens the menu of the current message");
                if (TimelineActions.activeMessage !== test.other) return fail("the menu of the current message");
                // Escape closes the menu, not the timeline's focus.
                test.key(Qt.Key_Escape, Qt.NoModifier);
                return next("menuclosed");
            case "menuclosed": {
                if (view.popupOpen) return later("Escape closes the menu");
                if (TimelineActions.activeMessage !== 0) return fail("no message is active without a popup");
                if (!view.rowList.activeFocus) {
                    view.rowList.forceActiveFocus();
                    return later("the timeline has the focus again");
                }
                // Up: an older message; Shift+F10 opens its menu.
                const before = view.rowList.currentIndex;
                test.key(Qt.Key_Up, Qt.NoModifier);
                if (view.rowList.currentIndex <= before) return fail("Up moves to an older message");
                const older = view.rowList.currentItem;
                if (!older || older.messageId === undefined || older.messageId === test.other) return fail("another message is current");
                test.key(Qt.Key_Down, Qt.NoModifier);
                if (view.rowList.currentIndex !== before) return fail("Down moves back");
                test.key(Qt.Key_Up, Qt.NoModifier);
                test.other = view.rowList.currentItem.messageId;
                test.key(Qt.Key_F10, Qt.ShiftModifier);
                return next("shiftf10");
            }
            case "shiftf10":
                if (!view.popupOpen) return later("Shift+F10 opens the menu");
                if (TimelineActions.activeMessage !== test.other) return fail("the menu of the message Up moved to");
                test.key(Qt.Key_Escape, Qt.NoModifier);
                return next("enterkey");
            case "enterkey":
                if (view.popupOpen) return later("Escape closes the second menu");
                if (!view.rowList.activeFocus) {
                    view.rowList.forceActiveFocus();
                    return later("the timeline has the focus");
                }
                // The rows hold no buttons to tab to: Enter opens what the current row offers.
                test.key(Qt.Key_Return, Qt.NoModifier);
                return next("entermenu");
            case "entermenu":
                if (!view.popupOpen) return later("Enter opens the menu of the current message");
                // (Which message is current depends on whether the focus came back to the
                // list by itself after the last menu closed, or had to be given back.)
                if (TimelineActions.activeMessage <= 0) return fail("Enter: a menu about a message");
                test.key(Qt.Key_Escape, Qt.NoModifier);
                return next("onestop");
            case "onestop":
                if (view.popupOpen) return later("Escape closes the third menu");
                if (!view.rowList.activeFocus) {
                    view.rowList.forceActiveFocus();
                    return later("the timeline has the focus");
                }
                // The timeline is one tab stop: Tab leaves it, whatever its rows hold, and
                // Shift+Tab comes straight back.
                test.key(Qt.Key_Tab, Qt.NoModifier);
                if (view.rowList.activeFocus) return fail("Tab stays inside the timeline: a row holds a tab stop");
                test.key(Qt.Key_Backtab, Qt.ShiftModifier);
                if (!view.rowList.activeFocus) return fail("Shift+Tab does not come back to the timeline in one step");
                return next("tocomposer");
            case "tocomposer":
                if (view.popupOpen) return later("Escape closes the second menu");
                if (!view.rowList.activeFocus) {
                    view.rowList.forceActiveFocus();
                    return later("the timeline has the focus");
                }
                // Escape in the timeline: back to writing.
                test.key(Qt.Key_Escape, Qt.NoModifier);
                if (!test.typing()) return fail("Escape returns to the composer");
                return next("hover");

            // ---- the toolbar under the pointer and the reaction picker ----
            case "hover": {
                const d = view.delegateFor(test.target);
                if (!d) return later("the row has a delegate");
                if (test.find(d, c => c.text === "Add Reaction" && c.visible) && test.waits === 0) return fail("the toolbar shows without the pointer");
                // The pointer over the row: its toolbar shows beside the bubble.
                test.ev.mouseMove(d, d.width / 2 + test.waits % 2, d.height / 2, -1, Qt.NoButton, Qt.NoModifier);
                const react = test.find(d, c => c.text === "Add Reaction" && c.visible);
                if (!react) return later("the toolbar shows under the pointer");
                test.click(react, react.width / 2, react.height / 2, Qt.LeftButton);
                return next("picker");
            }
            case "picker": {
                if (!view.popupOpen) return later("the toolbar's button opens the reaction picker");
                // (The picker is made on first use: its buttons have their places once it
                // has finished opening.)
                if (!view.pickerPopup || !view.pickerPopup.opened) return later("the picker has finished opening");
                if (TimelineActions.activeMessage !== test.target) return fail("the picker is about the message under the pointer");
                // One of the quick reactions.
                const thumb = test.find(test.contentItem, c => c.text === "👍" && c.visible && c.checked !== undefined && c.width > 0);
                if (!thumb) return later("the quick reactions show");
                test.click(thumb, thumb.width / 2, thumb.height / 2, Qt.LeftButton);
                return next("chip");
            }

            // ---- a reaction chip and a quote ----
            case "chip": {
                if (view.popupOpen) return later("the picker closes with the pick");
                const r = test.row(test.target);
                if (!r.reactions.some(c => c.emoji === "👍" && c.mine)) return later("our reaction shows");
                const d = view.delegateFor(test.target);
                const chip = test.find(d, c => c.modelData !== undefined && c.modelData !== null && c.modelData.emoji === "👍" && c.toggle !== undefined);
                if (!chip || chip.width <= 0) return later("the chip is drawn");
                // The row lays its chips out a moment after they appear: click once they rest.
                const at = chip.mapToItem(null, 0, 0).y;
                if (at !== test.chipAt) {
                    test.chipAt = at;
                    return later("the chip rests");
                }
                test.click(chip, chip.width / 2, chip.height / 2, Qt.LeftButton);
                return next("unreacted");
            }
            case "unreacted": {
                const r = test.row(test.target);
                if (r.reactions.some(c => c.emoji === "👍")) return later("a click on our chip takes the reaction back");
                // A quote whose original is here.
                let quote = null;
                for (let i = 0; i < rows.count && !quote; ++i) {
                    const candidate = rows.objectAt(i);
                    if (candidate && candidate.kind === "message") {
                        const d = view.delegateFor(candidate.messageId);
                        quote = test.find(d, c => c.reachable === true && c.block !== undefined && c.block.type === "quote");
                    }
                }
                if (!quote) return later("a quote of a known message is on screen");
                test.quoted = quote.block.target;
                test.click(quote, quote.width / 2, quote.height / 2, Qt.LeftButton);
                return next("jumped");
            }
            case "jumped":
                if (test.jumpedTo !== test.quoted) return later("a click on the quote jumps to its original");
                if (TimelineActions.flashMessage !== test.quoted) return fail("the original lights up");
                if (!view.delegateFor(test.quoted)) return later("the original is on screen");
                return next("settled");
            case "settled":
                // (The view centers the original once more a moment after the jump.)
                if (test.waits < 10) return later("the jump settles");
                // Away from the newest message: scroll to the oldest one. The rows up there get
                // their delegates and real heights now, and the view its final place: the
                // jump lands a margin short of the end until the list has laid them out.
                view.followNewest = false;
                view.rowList.positionViewAtEnd();
                view.rowList.forceLayout();
                view.rowList.positionViewAtEnd();
                view.rowList.returnToBounds();
                test.restY = view.rowList.contentY;
                test.restTicks = 0;
                return next("away");

            // ---- "Jump to Latest" ----
            case "away": {
                // The view is at rest before anything is measured against it. (This step
                // used to note where a text was one tick after the jump; on a busy machine
                // the list finished settling after that, by the 8 px of its margin, and the
                // click that followed was blamed for the move.)
                if (view.rowList.moving || view.rowList.contentY !== test.restY) {
                    test.restY = view.rowList.contentY;
                    test.restTicks = 0;
                    return later("the view comes to rest");
                }
                test.restTicks += 1;
                if (test.restTicks < 8) return later("the view stays at rest");
                if (view.atBottom) return fail("the chat is too short to scroll away from its newest message");
                const button = test.find(view, c => c.text === "Jump to Latest");
                if (!button || !button.visible) return later("Jump to Latest shows away from the newest message");
                // Over the view, inside what is visible of it: not somewhere in the content.
                const at = button.mapToItem(view, 0, 0);
                if (at.x < 0 || at.y < 0 || at.x + button.width > view.width || at.y + button.height > view.height) {
                    return later("Jump to Latest is inside the view: " + at.x + "," + at.y + " in " + view.width + "x" + view.height);
                }
                if (at.y + button.height < view.height - 80) return fail("Jump to Latest sits at the bottom edge: " + at.y);
                // A click into the text of a message up here (to select it, say) and on the
                // row's toolbar leaves the view where it is: the keyboard focus a click moves
                // must not make the newest message current and pull the view down to it.
                let text = null;
                for (let i = rows.count - 1; i >= 0 && !text; --i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "message") {
                        text = test.find(view.delegateFor(r.messageId), c => c.selectByMouse === true && c.visible && c.height > 0);
                    }
                }
                if (!text) return later("a message text is on screen");
                const where = text.mapToItem(view, 0, 0);
                if (where.y < 0 || where.y + text.height > view.height) return later("the text is inside the view");
                // Where the clicked text is in the view: it must stay there. (The distance to
                // the newest message is an estimate up here, while rows below are not built.)
                test.awayBy = where.y;
                test.click(text, Math.min(8, text.width / 2), text.height / 2, Qt.LeftButton);
                if (test.activeFocusItem !== text) return fail("a click into a text gives it the focus (to select and copy)");
                return next("clicked");
            }
            case "clicked": {
                // (Any pull to the newest message would have happened by now.)
                if (test.waits < 12) return later("the view had time to move");
                const text = test.activeFocusItem;
                if (!text || text.selectByMouse !== true) return fail("the clicked text kept the focus: " + text);
                const now = text.mapToItem(view, 0, 0).y;
                if (Math.abs(now - test.awayBy) > 2) {
                    return fail("a click into a message moved the view by " + (now - test.awayBy) + " (current row " + view.rowList.currentIndex + ", following " + view.followNewest + ")");
                }
                if (view.atBottom) return fail("a click into an older message brought the view to the newest one");
                if (view.rowList.currentIndex >= 0) return fail("a click made a row current: " + view.rowList.currentIndex);
                // The keyboard takes over from there: Up makes the clicked message current,
                // not the newest one, and the view stays.
                let clicked = test.activeFocusItem;
                while (clicked && !(clicked instanceof MessageDelegate)) {
                    clicked = clicked.parent;
                }
                if (!clicked) return fail("the focus is inside the clicked row");
                test.key(Qt.Key_Up, Qt.NoModifier);
                if (view.rowList.currentItem !== clicked || !clicked.current) return fail("Up after a click starts at the clicked message");
                return next("keyedfromclick");
            }
            case "keyedfromclick": {
                if (test.waits < 12) return later("the view had time to move");
                // Still up here, at the clicked message (the view only brings it in whole).
                if (view.atBottom) return fail("the first key after a click brought the view to the newest message");
                const current = view.rowList.currentItem;
                if (!current || !current.current) return fail("the clicked message stays current");
                const top = current.mapToItem(view, 0, 0).y;
                if (top + current.height < 0 || top > view.height) return fail("the current message is on screen: " + top);
                // Escape: back to writing; no row stays current.
                test.key(Qt.Key_Escape, Qt.NoModifier);
                if (!test.typing()) return fail("Escape returns to the composer from a clicked row");
                if (view.rowList.currentIndex >= 0) return fail("a row stayed current without the focus");
                const button = test.find(view, c => c.text === "Jump to Latest");
                test.click(button, button.width / 2, button.height / 2, Qt.LeftButton);
                return next("latest");
            }
            case "latest": {
                if (!view.atBottom) return later("Jump to Latest returns to the newest message");
                const button = test.find(view, c => c.text === "Jump to Latest");
                if (button && button.visible) return later("the button goes at the newest message");
                Qt.exit(0);
                return;
            }
            }
        }
    }

    Timer {
        interval: 240000
        running: true
        onTriggered: fail("timeout in step " + test.step)
    }
}
"#;

fn main() {
    support::answer_list("input_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
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
    let fixture = handle.fixture().expect("fixture data");
    let quotes = fixture.quotes_conversation.expect("the quotes chat");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT.replace("QUOTES", &quotes.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/input_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    app.shutdown();
    if code == 0 {
        println!(
            "test message actions by keyboard and pointer ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
    } else {
        println!(
            "test message actions by keyboard and pointer ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
