//! The emoji picker, the pins and the hover bar, through the real main window and the real core over
//! the fake network (offscreen, `harness = false`; one session):
//!
//! - A fresh profile has Signal's six pinned; the first THREE are one-click reactions in the hover bar of
//!   a message (never in the tab order); a press reacts, a second press takes the reaction back; a pick
//!   moves the emoji to the front of the recent list, taking a reaction back does not.
//! - The composer's picker opens on Favorites with a search field that has the keyboard; with nothing used
//!   it says so; a search finds by an English word and by a Ukrainian word, shows "No emoji found" when
//!   nothing matches, and replaces the grid with its hits; closing frees the rows.
//! - A right click on an emoji (and Shift+F10 and the Menu key on the current one) opens the menu: "Add to
//!   Favorites" stars, "Unpin from Reactions" unpins; the seventh pin is refused; the hover bar then holds
//!   the first three pins of the new order.
//! - Enter on a hit picks it (the composer's popup closes and says what was picked).
//! - The preferences hold the lists after the debounce is flushed, and a toggle of a Settings page switch
//!   afterwards leaves them as they were (the lists have ONE writer).

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
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
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property real photo: 0
    property real openedAt: 0
    property string pickedGlyph: ""
    property int pickedCount: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 400) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
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

    function findAll(root: Item, pred: var, out: var): var {
        if (pred(root)) {
            out.push(root);
        }
        for (const child of root.children) {
            win.findAll(child, pred, out);
        }
        return out;
    }

    function click(item: Item, button: int): void {
        win.ev.mouseClick(item, item.width / 2, item.height / 2, button, Qt.NoModifier, -1);
    }

    readonly property var view: win.chatPage ? win.chatPage.timeline : null
    readonly property var composerItem: win.chatPage ? win.chatPage.composer : null
    readonly property var lists: App.emojiLists
    readonly property var popup: win.composerItem ? win.composerItem.emojiPopup : null
    readonly property var grid: win.popup ? win.popup.grid : null

    // The pinned buttons of the photo's row.
    function hoverPins(): var {
        const d = win.view.delegateFor(win.photo);
        return d ? win.findAll(d, c => c.objectName === "hoverPin", []) : [];
    }

    function glyphs(buttons: var): string {
        return buttons.map(b => b.text).join(" ");
    }

    function mine(glyph: string): bool {
        const d = win.view.delegateFor(win.photo);
        if (!d) {
            return false;
        }
        for (const chip of d.reactions) {
            if (chip.mine && chip.emoji === glyph) {
                return true;
            }
        }
        return false;
    }

    // The cell of `glyph` in the hits.
    function hit(glyph: string): var {
        const g = win.grid.hitsGrid;
        for (let i = 0; i < g.count; ++i) {
            if (g.glyphAtIndex(i) === glyph) {
                g.positionViewAtIndex(i, GridView.Contain);
                return g.itemAtIndex(i);
            }
        }
        return null;
    }

    // Why a hit is not there (for the failure message).
    function why(): string {
        const g = win.grid.hitsGrid;
        return g.count + " hits of '" + win.grid.emojis.query + "' (model " + win.grid.emojis.count + "), first " + g.glyphAtIndex(0) + ", visible " + g.visible + ", item0 " + (g.itemAtIndex(0) !== null) + ", height " + g.height + ", popup closed in: " + win.closedIn + " opened " + (win.popup ? win.popup.opened : "none");
    }

    // The time the model takes to answer a typed text (debug build, offscreen): logged for the report.
    function search(text: string): void {
        const t = Date.now();
        win.grid.searchField.text = text;
        console.log("EMOJI_SEARCH_MS " + (Date.now() - t) + " '" + text + "' -> " + win.grid.emojis.count + " hits");
    }

    property string closedIn: ""
    Connections {
        target: win.popup
        function onClosed(): void {
            win.closedIn += win.step + ";";
        }
        function onPicked(emoji: string): void {
            win.pickedGlyph = emoji;
            win.pickedCount += 1;
        }
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
        switch (win.step) {
        case "start": {
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "emoji_ui_events.qml");
            win.openConversation(QUOTES);
            win.next("chat");
            return;
        }
        case "chat": {
            if (App.timeline.conversationId !== QUOTES || App.timeline.loading || App.timeline.count < 3) return win.later("the chat");
            win.photo = App.findMessage("photo");
            // A fresh profile: Signal's six, in Signal's order; the hover bar takes the first three.
            if (win.lists.pins.join(" ") !== "❤️ 👍 👎 😂 😮 😢") return win.fail("the default pins: " + win.lists.pins.join(" "));
            if (win.lists.hoverPins.length !== 3) return win.fail("the hover pins: " + win.lists.hoverPins.length);
            if (win.lists.favourites.length !== 0 || win.lists.recent.length !== 0) return win.fail("a fresh profile has no favourites or recent");
            if (!win.view.toolbarFor(win.photo)) return win.later("the photo's row to have a delegate");
            win.next("hover");
            return;
        }
        case "hover": {
            const pins = win.hoverPins();
            if (pins.length !== 3) return win.later("the three pinned buttons: " + pins.length);
            // The row settles (the list moves it into view) before it is clicked.
            if (win.waits < 15) return win.later("the toolbar to settle");
            if (win.glyphs(pins) !== "❤️ 👍 👎") return win.fail("the hover bar: " + win.glyphs(pins));
            for (const b of pins) {
                if (b.focusPolicy !== Qt.NoFocus) return win.fail("a pinned button takes the keyboard focus");
                if (b.Accessible.name.indexOf("React with") !== 0) return win.fail("its accessible name: " + b.Accessible.name);
            }
            win.click(pins[1], Qt.LeftButton);
            win.next("hover-reacted");
            return;
        }
        case "hover-reacted": {
            if (!win.mine("👍")) return win.later("our reaction on the message");
            const pins = win.hoverPins();
            if (!pins[1].checked) return win.fail("the pinned button of our reaction is not checked");
            if (pins[1].Accessible.name.indexOf("Remove reaction") !== 0) return win.fail("its name: " + pins[1].Accessible.name);
            if (win.lists.recent[0] !== "👍") return win.fail("a reaction is a use: " + win.lists.recent.join(" "));
            // Another reaction takes the first away (one per sender): press the checked one's neighbour, then press ours again to take it back.
            win.click(pins[1], Qt.LeftButton);
            win.next("hover-removed");
            return;
        }
        case "hover-removed": {
            if (win.mine("👍")) return win.later("our reaction taken back");
            if (win.lists.recent.length !== 1) return win.fail("taking a reaction back is no use: " + win.lists.recent.join(" "));
            // The composer's picker.
            win.openedAt = Date.now();
            win.composerItem.toggleEmoji("");
            win.next("picker");
            return;
        }
        case "picker": {
            if (!win.popup || !win.popup.opened || !win.grid.visible) return win.later("the picker");
            console.log("EMOJI_FIRST_OPEN_MS " + (Date.now() - win.openedAt) + " (made, opened, first tick after)");
            const g = win.grid;
            if (g.tab !== 0) return win.fail("the picker does not open on Favorites: " + g.tab);
            if (!g.searchField.activeFocus) return win.fail("the search field has no keyboard focus");
            if (g.recentGrid.count !== 1) return win.fail("Recently Used: " + g.recentGrid.count);
            if (g.favouritesGrid.count !== 0) return win.fail("no favourites yet");
            if (g.emojis.count !== 0) return win.fail("rows listed on the Favorites tab: " + g.emojis.count);
            win.search("grinning");
            win.next("search-en");
            return;
        }
        case "search-en": {
            const g = win.grid;
            if (g.emojis.count < 3) return win.later("the hits of an English word");
            if (!g.hitsGrid.visible || g.groupGrid.visible) return win.fail("the hits do not replace the grid");
            if (g.emojis.glyphAt(0) !== "😀") return win.fail("the first hit: " + g.emojis.glyphAt(0));
            win.search("усміх");
            win.next("search-uk");
            return;
        }
        case "search-uk": {
            const g = win.grid;
            let found = false;
            for (let i = 0; i < g.emojis.count; ++i) found = found || g.emojis.glyphAt(i) === "😀";
            if (!found) return win.later("a Ukrainian word finds the grinning face");
            // The name is in the interface language: English here.
            if (g.emojis.nameOf("😀") !== "grinning face") return win.fail("the name: " + g.emojis.nameOf("😀"));
            win.search("qqqqzzzz");
            win.next("search-none");
            return;
        }
        case "search-none": {
            // A search left alone for a while picks nothing (the field's own auto-accept is off).
            if (win.waits < 40) return win.later("a quiet second");
            if (win.pickedCount !== 0) return win.fail("a search picked an emoji by itself");
            const g = win.grid;
            if (g.emojis.count !== 0) return win.later("no hits");
            if (g.hitsGrid.visible) return win.fail("an empty grid shows");
            if (!win.find(g, c => c.text === "No emoji found" && c.visible)) return win.fail("the line 'No emoji found'");
            win.search("dog face");
            win.next("star");
            return;
        }
        case "star": {
            const cell = win.hit("🐶");
            if (!cell) return win.later("the dog face among the hits: " + win.why());
            win.click(cell, Qt.RightButton);
            win.next("star-menu");
            return;
        }
        case "star-menu": {
            const m = win.grid.menu;
            if (!m.visible) return win.later("the menu");
            const star = win.find(m.contentItem, c => c.objectName === "starItem") || win.find(QQC2.Overlay.overlay, c => c.objectName === "starItem");
            if (!star) return win.fail("the star entry");
            if (star.text !== "Add to Favorites") return win.fail("the star entry says " + star.text);
            const pin = win.find(QQC2.Overlay.overlay, c => c.objectName === "pinItem");
            if (!pin || pin.text !== "Pin to Reactions" || pin.enabled) return win.fail("six are pinned: the pin entry is disabled: " + (pin ? pin.text + " " + pin.enabled : "none"));
            win.click(star, Qt.LeftButton);
            win.next("starred");
            return;
        }
        case "starred": {
            if (win.lists.favourites.join(" ") !== "🐶") return win.later("the star is kept: " + win.lists.favourites.join(" "));
            win.search("red heart");
            win.next("unpin");
            return;
        }
        case "unpin": {
            const cell = win.hit("❤️");
            if (!cell) return win.later("the red heart among the hits: " + win.why());
            win.grid.hitsGrid.currentIndex = win.grid.hitsGrid.indexAt(cell.x, cell.y);
            // The keyboard path: the current cell, Shift+F10.
            win.grid.hitsGrid.forceActiveFocus();
            win.ev.keyClick(Qt.Key_F10, Qt.ShiftModifier, -1);
            win.next("unpin-menu");
            return;
        }
        case "unpin-menu": {
            const m = win.grid.menu;
            if (!m.visible) return win.later("the menu on Shift+F10");
            const pin = win.find(QQC2.Overlay.overlay, c => c.objectName === "pinItem");
            if (!pin || pin.text !== "Unpin from Reactions" || !pin.enabled) return win.fail("the unpin entry: " + (pin ? pin.text : "none"));
            win.click(pin, Qt.LeftButton);
            win.next("unpinned");
            return;
        }
        case "unpinned": {
            if (win.lists.pins.length !== 5) return win.later("one pin fewer: " + win.lists.pins.length);
            if (win.lists.hoverPins.join(" ") !== "👍 👎 😂") return win.fail("the first three of the new order: " + win.lists.hoverPins.join(" "));
            win.search("dog face");
            win.next("pin");
            return;
        }
        case "pin": {
            const cell = win.hit("🐶");
            if (!cell) return win.later("the dog face among the hits: " + win.why());
            win.grid.hitsGrid.currentIndex = win.grid.hitsGrid.indexAt(cell.x, cell.y);
            win.grid.hitsGrid.forceActiveFocus();
            // The Menu key.
            win.ev.keyClick(Qt.Key_Menu, Qt.NoModifier, -1);
            win.next("pin-menu");
            return;
        }
        case "pin-menu": {
            const m = win.grid.menu;
            if (!m.visible) return win.later("the menu on the Menu key");
            const pin = win.find(QQC2.Overlay.overlay, c => c.objectName === "pinItem");
            if (!pin || pin.text !== "Pin to Reactions" || !pin.enabled) return win.fail("the pin entry: " + (pin ? pin.text + " " + pin.enabled : "none"));
            const star = win.find(QQC2.Overlay.overlay, c => c.objectName === "starItem");
            if (star.text !== "Remove from Favorites") return win.fail("the star entry now says " + star.text);
            win.click(pin, Qt.LeftButton);
            win.next("pinned");
            return;
        }
        case "pinned": {
            if (win.lists.pins.length !== 6 || win.lists.pins[5] !== "🐶") return win.later("the dog pinned last: " + win.lists.pins.join(" "));
            // Only the first three are in the hover bar.
            if (win.lists.hoverPins.length !== 3 || win.lists.hoverPins.indexOf("🐶") >= 0) return win.fail("the hover pins: " + win.lists.hoverPins.join(" "));
            // The seventh is refused.
            win.search("cat face");
            win.next("seventh");
            return;
        }
        case "seventh": {
            const cell = win.hit("🐱");
            if (!cell) return win.later("the cat face among the hits: " + win.why());
            win.click(cell, Qt.RightButton);
            win.next("seventh-menu");
            return;
        }
        case "seventh-menu": {
            const m = win.grid.menu;
            if (!m.visible) return win.later("the menu");
            const pin = win.find(QQC2.Overlay.overlay, c => c.objectName === "pinItem");
            if (!pin || pin.enabled) return win.fail("a seventh pin is offered");
            win.lists.togglePin("🐱");
            if (win.lists.pins.length !== 6) return win.fail("a seventh pin was taken: " + win.lists.pins.length);
            m.close();
            // Enter on the first hit picks it.
            win.search("dog face");
            win.next("enter");
            return;
        }
        case "enter": {
            const g = win.grid;
            if (g.emojis.glyphAt(0) !== "🐶") return win.later("the dog first");
            g.hitsGrid.forceActiveFocus();
            g.hitsGrid.currentIndex = 0;
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            win.next("picked");
            return;
        }
        case "picked": {
            if (win.pickedCount !== 1) return win.later("the pick");
            if (win.pickedGlyph !== "🐶") return win.fail("picked " + win.pickedGlyph);
            if (win.popup.visible) return win.later("the popup closes on a pick");
            if (win.lists.recent[0] !== "🐶") return win.fail("a pick is a use: " + win.lists.recent.join(" "));
            // Closed: nothing is listed.
            if (win.grid.emojis.count !== 0) return win.fail("rows kept by a closed picker: " + win.grid.emojis.count);
            // The reaction picker's quick row is the pins, and the whole picker is the same component.
            win.view.pickerFor(win.photo, true);
            win.next("reaction-picker");
            return;
        }
        case "reaction-picker": {
            const p = win.view.pickerPopup;
            if (!p || !p.opened) return win.later("the reaction picker");
            const quick = win.findAll(p.contentItem, c => c.Accessible && c.Accessible.name && c.Accessible.name.indexOf("React with") === 0, []);
            if (quick.length !== 6) return win.fail("the quick row holds " + quick.length + " of the 6 pins");
            const full = win.find(p.contentItem, c => c.searchField !== undefined);
            if (!full || !full.visible) return win.fail("the whole picker");
            if (full.tab !== 0) return win.fail("the reaction picker opens on Favorites too");
            p.close();
            win.next("settings");
            return;
        }
        case "settings": {
            // The lists reach the preferences; a switch of the Settings page then leaves them.
            win.lists.flush();
            App.settings.compact = !App.settings.compact;
            win.next("done");
            return;
        }
        case "done": {
            Qt.exit(0);
            return;
        }
        }
    }
}
"##;

fn main() {
    support::answer_list("emoji_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // The English interface: the names the script compares are English.
    // SAFETY: as above.
    unsafe { std::env::set_var("LANGUAGE", "en") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 12,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let quotes = handle
        .fixture()
        .expect("fixture mode")
        .quotes_conversation
        .expect("the quotes chat");
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
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/emoji_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the emoji picker ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    // The lists reached the preferences, and the Settings page's toggle left them.
    let prefs = handle.desktop().prefs();
    let pins = prefs
        .emoji_pins
        .clone()
        .expect("the pins differ from Signal's six");
    assert_eq!(pins, ["👍", "👎", "😂", "😮", "😢", "🐶"], "the pins");
    assert_eq!(prefs.emoji_favourites, ["🐶"], "the favourites");
    assert_eq!(prefs.emoji_recent.first().map(String::as_str), Some("🐶"));
    assert_eq!(prefs.emoji_recent.get(1).map(String::as_str), Some("👍"));
    app.shutdown();
    println!("test the emoji picker ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
