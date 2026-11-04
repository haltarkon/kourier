//! Blended chats in the side panel (C5): the real main window over the real core with three linked
//! accounts (the fixture account, a second one of the test network and a registered fake playing
//! WhatsApp, `FixtureOptions::accounts`), REAL mouse and key events (`TestEvent`), offscreen,
//! `harness = false`, each case in a process of its own (`KOURIER_ACCOUNTS_SHOWN_ONLY=a,b`).
//!
//! Cases:
//! - `click` (narrow): a click on an account shows only it, tints it, fades the others, and closes
//!   the modal drawer.
//! - `shift`, `ctrl` (narrow): Shift+click / Ctrl+click takes the account out and puts it back;
//!   the two others stay shown (no "only it" behind the toggle: H1's two handlers), the drawer
//!   stays open, "All Chats" is tinted only when all are shown.
//! - `ctrlspace` (wide): ONE Ctrl+Space press toggles once (no click behind it); a stand-in
//!   auto-repeat (`TestEvent` cannot send one) of Ctrl+Space and of Space is taken by the entry's
//!   filter and toggles nothing, a repeat of Tab is not taken.
//! - `space` (wide): Space on an entry is the plain click (only it).
//! - `box` (narrow): the boxes mirror `shown`; a click on a box hides that account; with one
//!   account left its box is greyed, the BOX's own hover says why (the entry's does not, UI-7), a
//!   click on the greyed box changes nothing, keeps the modal drawer open (L8) and tells
//!   (`lastAccountStays`, UI-7); Shift+click on it tells too (P6).
//! - `menu` (wide): the Menu key's "Show in List" is checked for a shown account, unchecked after
//!   it was hidden (set at popup, M6), "Show Only This Account" shows only it, "Show in List" on
//!   the last shown tells (P6).
//! - `off` (narrow): the setting off: no boxes, Shift+click is a click and closes the drawer (L9),
//!   the accounts not shown are not faded (UI-6).
//! - `narrow` (narrow): unread messages of a hidden account: the "Dot with count" (UI-1) on the
//!   CLOSED menu button (Kirigami's toolbar `HandleButton` before the list's title): visible, on
//!   top, its centre over the button's end half and top half, the count and the handle's tooltip
//!   (its accessible name) name it; hidden while the drawer is open; the same on the archive
//!   view's title (NEW-3), hidden there while the drawer is open; gone when nothing hidden is
//!   unread.
//! - `title` (wide, then narrow): two accounts shown: the chats' title delegate (UI-2) names them
//!   while they fit, says "2 accounts" when they do not: at 360 px the fixture's names still fit,
//!   so the case draws the heading three times as large (M7).
//! - `tab` (wide; UI-4, no `forceActiveFocus`): Ctrl+K puts the keyboard in the search field,
//!   Shift+Tab reaches the panel's one Tab stop ("All Chats" while all are shown), Tab leaves the
//!   panel and Shift+Tab comes back to that same entry (the window's own chain), Down moves to
//!   the accounts, Ctrl+Space toggles the focused one, Up goes back; after a click on an account
//!   the Tab stop is that account's entry.
//! - `return` (wide; UI-5): Return on an account entry shows only it, Enter on "All Chats" shows
//!   all again.
//! - `one` (narrow, ONE account; UI-3): Shift+click and Ctrl+Space on the only account are the
//!   plain click (it is current, the drawer closes, nothing tells "stays").
//! - `placeholder`: the fixture account's history transfer waits; with it hidden the empty list's
//!   text reads no transfer (only a SHOWN account's, M2), shown again it does.
//! - `replace` (wide; the critic's M4, fix S2): account 1 hidden, "Transfer History from Phone…"
//!   on it: the new account comes back HIDDEN (the replace keeps the choice), `shownCount` 2;
//!   after the run `hidden-accounts` in the prefs holds the NEW account's uuid alone.
//! - `replacecancel` (wide; fix S1): only account 1 shown, "Transfer History from Phone…" on it,
//!   the link cancelled at the QR code (after the core announced the new account): the replace
//!   ends at once, both accounts left are shown ("All Chats"), the list is not empty, and the
//!   last-shown guard holds again (a toggle of the only shown account is refused).
//! - `drawerkeys` (narrow; fix V5): the modal drawer from the keyboard alone: Ctrl+K, Shift+Tab
//!   back to the toolbar's menu button, Space opens the drawer with the keyboard on the panel's stop
//!   ("All Chats"), Down/Up move between the accounts, Return shows the focused one alone and
//!   closes the drawer; opened again the same way the stop is that account, Escape closes it.
//! - `saved` (wide; fix V3, S4's App half): a choice saved before the start hides account 2: when
//!   the window is made, before the core's accounts answer, `App.currentTitle` is empty and
//!   `allShown` false (no "All Chats"); after the answer two accounts are shown and named.

mod support;

use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{
    App, FixtureOptions, FixtureSecondLink, FixtureTransfer, HistoryTransfer, Launch, StartOptions,
};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import org.kde.kirigami as Kirigami
import app.kourier

MainWindow {
    id: win

    width: win.narrowCase ? 480 : 1100
    height: 760
    visible: true

    property string mode: "CASE"
    readonly property bool narrowCase: ["click", "shift", "ctrl", "box", "off", "narrow", "one", "drawerkeys"].indexOf(win.mode) >= 0
    // The accounts of the case's profile.
    readonly property int accountsWanted: win.mode === "one" ? 1 : 3
    property var ev: null
    property int waits: 0
    property int held: 0
    property int stepAt: 0
    property bool busy: false
    property int stays: 0
    // `replace`, `replacecancel`: the replaced account and the accounts at the start.
    property real first: 0
    property var before: []
    // `saved`: what the App said when the window was made, before the core's accounts answer.
    property var atStart: null
    Component.onCompleted: win.atStart = { title: App.currentTitle, allShown: App.allShown, known: App.accountActions.length }

    readonly property var drawer: win.globalDrawer

    function fail(msg: string): void {
        console.error("FAIL [" + win.mode + " step " + win.stepAt + "]: " + msg);
        Qt.exit(1);
    }

    Connections {
        target: win.globalDrawer
        function onLastAccountStays(): void {
            win.stays += 1;
        }
    }

    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {
            required property real accountId
        }
    }

    function listed(): var {
        const s = new Set();
        for (let i = 0; i < convs.count; i++) s.add(convs.objectAt(i).accountId);
        return s;
    }
    function acc(i: int): real {
        return App.accountActions[i].account;
    }
    function shown(i: int): bool {
        return App.accountActions[i].shown === true;
    }
    function entry(i: int): Item {
        return win.drawer.accountEntry(i);
    }
    function find(root: Item, pred: var): Item {
        if (root === null || root === undefined) return null;
        if (pred(root)) return root;
        for (const child of root.children) {
            const hit = win.find(child, pred);
            if (hit) return hit;
        }
        return null;
    }
    function boxOf(i: int): Item {
        return win.find(win.entry(i), x => x.objectName === "entryBox");
    }
    function guardOf(i: int): Item {
        return win.find(win.entry(i), x => x.objectName === "entryBoxGuard");
    }
    function key(k: int, modifiers: int): void {
        if (!win.ev.keyClick(k, modifiers, -1)) win.fail("the key event was not delivered");
    }
    // The list page (the first page of the stack) and its search field.
    function listPage(): Item {
        return win.pageStack.get(0);
    }
    // The menu button of the list page's toolbar: Kirigami's `HandleButton` of the global drawer.
    function menuButton(): Item {
        const bar = win.listPage().globalToolBarItem;
        return win.find(bar, x => x.drawer === win.globalDrawer && x.visible && x.width > 0);
    }
    function chatsTitle(): Item {
        return win.find(win.listPage().globalToolBarItem, x => x.objectName === "chatsTitle");
    }
    function archiveTitle(): Item {
        return win.find(win.listPage().globalToolBarItem, x => x.objectName === "archiveTitle");
    }
    // "" when `pill` shows the hidden unread count over the closed menu button's top end
    // corner (its centre in the button's end half, or just past its end, and in its top half),
    // painted over it; else what is wrong.
    function pillOnButton(pill: Item): string {
        if (pill.count !== App.hiddenUnread) return "the pill's count";
        if (win.drawer.handleClosedToolTip.indexOf(String(App.hiddenUnread)) < 0) return "the handle's tooltip: " + win.drawer.handleClosedToolTip;
        const button = win.menuButton();
        if (button === null) return "no menu button in the list's toolbar";
        const c = pill.mapToItem(button, pill.width / 2, pill.height / 2);
        console.warn("pill centre on the menu button: " + c.x + "," + c.y + " of " + button.width + "x" + button.height);
        if (c.x < button.width / 2 || c.x > button.width + Kirigami.Units.largeSpacing) return "the pill is not over the button's end (x " + c.x + " of " + button.width + ")";
        if (c.y < -Kirigami.Units.smallSpacing || c.y > button.height / 2) return "the pill is not over the button's top (y " + c.y + " of " + button.height + ")";
        if (!win.paintedOver(pill, button)) return "the pill is not over the menu button (closed)";
        return "";
    }
    function click(item: Item, mods: int): void {
        win.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, mods, -1);
    }
    // The topmost item at the centre of `item`, walking down from the window's root item.
    function onTop(item: Item): bool {
        const root = win.contentItem.parent;
        const pt = item.mapToItem(root, item.width / 2, item.height / 2);
        let cur = root;
        for (;;) {
            const local = root.mapToItem(cur, pt.x, pt.y);
            const c = cur.childAt(local.x, local.y);
            if (c === null) break;
            cur = c;
        }
        for (let p = cur; p !== null; p = p.parent) {
            if (p === item) return true;
        }
        console.warn("on top instead: " + String(cur));
        return false;
    }
    // Whether `top` is painted over `under` (the menu button's pill hangs off the title's start,
    // where `childAt` cannot find it): below their nearest common parent, the branch of `top`
    // has the higher z, or the same z and a later place.
    function paintedOver(top: Item, under: Item): bool {
        const chain = [];
        for (let p = under; p !== null; p = p.parent) chain.push(p);
        let a = top;
        while (a.parent !== null && chain.indexOf(a.parent) < 0) a = a.parent;
        const common = a.parent;
        if (common === null) return false;
        const b = chain[chain.indexOf(common) - 1];
        const kids = Array.from(common.children);
        console.warn("painted over: z " + a.z + "/" + b.z + ", place " + kids.indexOf(a) + "/" + kids.indexOf(b));
        return a.z > b.z || (a.z === b.z && kids.indexOf(a) > kids.indexOf(b));
    }
    function opened(): var {
        if (!win.drawer.drawerOpen || win.drawer.position < 1) return "the drawer opens";
        return true;
    }
    // The drawer stays open for a few ticks.
    function staysOpen(): var {
        if (!win.drawer.drawerOpen) return win.fail("the drawer closed");
        win.held += 1;
        return win.held >= 6 ? true : "the drawer stays open";
    }

    readonly property var common: [
        function () {
            if (!win.pageReadyForTest()) return "the window and its accounts";
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "accounts_shown_ui_events.qml");
            return true;
        },
    ]

    function pageReadyForTest(): bool {
        const n = win.accountsWanted;
        if (App.accountActions.length < n || App.conversations.count === 0) return false;
        // (`saved` starts with account 2 hidden.)
        const shownWanted = win.mode === "saved" ? n - 1 : n;
        if (App.shownCount !== shownWanted || App.allShown !== (win.mode !== "saved") || win.listed().size !== shownWanted) return false;
        win.held += 1;
        return win.held > 30;
    }

    function openDrawer(): var {
        win.drawer.open();
        return true;
    }

    // `drawerkeys`: a step that presses `k` once per tick until `pred()` holds (the press that
    // made it hold included), at most ten presses; it logs where the keyboard went.
    property int presses: 0
    function pressUntil(k: int, mods: int, pred: var, what: string): var {
        return function () {
            if (pred()) {
                win.presses = 0;
                return true;
            }
            if (win.presses >= 10) return win.fail(what + ": not after ten presses (the keyboard is on " + win.activeFocusItem + ")");
            win.presses += 1;
            win.key(k, mods);
            console.warn("drawerkeys: key " + k + " -> " + win.activeFocusItem);
            return "pressing for " + what;
        };
    }
    // Whether the keyboard is on `item` or inside it.
    function focusIn(item: Item): bool {
        for (let f = win.activeFocusItem; f !== null; f = f.parent) {
            if (f === item) return true;
        }
        return false;
    }
    // The keyboard path to the open drawer of a narrow window: Ctrl+K, Shift+Tab back to the
    // toolbar's menu button, Space on it; the panel opens with the keyboard on its stop.
    function openByKeys(): var {
        return [
            function () { win.key(Qt.Key_K, Qt.ControlModifier); return true; },
            function () { return win.listPage().searchField.activeFocus ? true : "Ctrl+K: the search field"; },
            win.pressUntil(Qt.Key_Backtab, Qt.ShiftModifier, () => win.menuButton() !== null && win.focusIn(win.menuButton()), "Shift+Tab to the menu button"),
            function () { win.key(Qt.Key_Space, Qt.NoModifier); return true; },
            win.opened,
            // The keyboard that opened the panel is on its stop at once (no Tab).
            function () { return win.drawer.entryList().some(e => e.activeFocus) ? true : win.fail("the keyboard is not in the opened panel but on " + win.activeFocusItem); },
        ];
    }

    function toggleSteps(mods: int): var {
        return [
            win.openDrawer,
            win.opened,
            function () { win.click(win.entry(1), mods); win.held = 0; return true; },
            function () {
                if (App.shownCount !== 2 || win.shown(1)) return "account 2 is hidden";
                if (!win.shown(0) || !win.shown(2) || App.currentAccount !== -1) return win.fail("a toggle also clicked: shownCount " + App.shownCount + ", current " + App.currentAccount);
                if (win.listed().has(win.acc(1)) || win.listed().size !== 2) return "the list follows";
                if (!win.entry(1).faded || win.entry(0).faded) return win.fail("the fade");
                if (!win.entry(0).highlighted || !win.entry(2).highlighted || win.entry(1).highlighted) return win.fail("the tint");
                return win.staysOpen();
            },
            function () { win.click(win.entry(1), mods); win.held = 0; return true; },
            function () {
                if (App.shownCount !== 3 || !App.allShown) return "all shown again";
                if (win.entry(1).faded || win.entry(0).highlighted) return win.fail("all shown: no fade, no account tint");
                return win.staysOpen();
            },
        ];
    }

    function caseSteps(): var {
        switch (win.mode) {
        case "click":
            return [
                win.openDrawer,
                win.opened,
                function () { win.click(win.entry(1), Qt.NoModifier); return true; },
                function () {
                    if (App.shownCount !== 1 || App.currentAccount !== win.acc(1)) return "only account 2";
                    if (win.drawer.drawerOpen) return "the modal drawer closes";
                    const l = win.listed();
                    if (l.size !== 1 || !l.has(win.acc(1))) return "the list follows";
                    if (!win.entry(1).highlighted || win.entry(0).highlighted || !win.entry(0).faded || win.entry(1).faded) return win.fail("tint and fade");
                    return true;
                },
            ];
        case "shift":
            return win.toggleSteps(Qt.ShiftModifier);
        case "ctrl":
            return win.toggleSteps(Qt.ControlModifier);
        case "ctrlspace":
            return [
                function () { win.entry(1).forceActiveFocus(); return win.entry(1).activeFocus ? true : "the entry takes the keyboard"; },
                function () { win.ev.keyClick(Qt.Key_Space, Qt.ControlModifier, -1); win.held = 0; return true; },
                function () {
                    if (App.shownCount !== 2 || win.shown(1)) return "one toggle";
                    if (App.currentAccount !== -1) return win.fail("a click behind Ctrl+Space");
                    win.held += 1;
                    return win.held >= 6 ? true : "it stays one toggle";
                },
                function () {
                    const e = win.entry(1);
                    const press = { isAutoRepeat: true, accepted: false, key: Qt.Key_Space, modifiers: Qt.ControlModifier };
                    e.pressKey(press);
                    if (!press.accepted) return win.fail("a repeat of Ctrl+Space is not taken");
                    const plain = { isAutoRepeat: true, accepted: false, key: Qt.Key_Space, modifiers: Qt.NoModifier };
                    e.pressKey(plain);
                    if (!plain.accepted) return win.fail("a repeat of Space is not taken");
                    const release = { isAutoRepeat: true, accepted: false, key: Qt.Key_Space, modifiers: Qt.NoModifier };
                    e.releaseKey(release);
                    if (!release.accepted) return win.fail("a repeated release of Space is not taken");
                    const tab = { isAutoRepeat: true, accepted: false, key: Qt.Key_Tab, modifiers: Qt.NoModifier };
                    e.pressKey(tab);
                    if (tab.accepted) return win.fail("a repeat of Tab is taken");
                    const first = { isAutoRepeat: false, accepted: false, key: Qt.Key_Space, modifiers: Qt.NoModifier };
                    e.pressKey(first);
                    if (first.accepted) return win.fail("a first Space is taken");
                    win.held = 0;
                    return true;
                },
                function () {
                    if (App.shownCount !== 2 || win.shown(1) || App.currentAccount !== -1) return win.fail("a repeat toggled or clicked: shownCount " + App.shownCount);
                    win.held += 1;
                    return win.held >= 4 ? true : "nothing moves";
                },
            ];
        case "space":
            return [
                function () { win.entry(2).forceActiveFocus(); return win.entry(2).activeFocus ? true : "the entry takes the keyboard"; },
                function () { win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1); return true; },
                function () { return App.shownCount === 1 && App.currentAccount === win.acc(2) ? true : "Space shows only it"; },
            ];
        case "box":
            return [
                win.openDrawer,
                win.opened,
                function () {
                    for (let i = 0; i < 3; i++) {
                        const b = win.boxOf(i);
                        if (b === null || !b.visible) return "the boxes beside the names";
                        if (b.checked !== win.shown(i) || !b.enabled) return win.fail("box " + i + " does not mirror shown");
                        if (b.focusPolicy !== Qt.NoFocus) return win.fail("a box is a Tab stop");
                    }
                    return true;
                },
                function () { win.click(win.boxOf(1), Qt.NoModifier); win.held = 0; return true; },
                function () {
                    if (win.shown(1) || App.shownCount !== 2) return "the box hides account 2";
                    if (win.boxOf(1).checked) return win.fail("the box stays ticked");
                    return win.staysOpen();
                },
                function () { win.click(win.entry(0), Qt.NoModifier); return true; },
                function () { return App.shownCount === 1 && App.currentAccount === win.acc(0) && !win.drawer.drawerOpen ? true : "only account 1"; },
                win.openDrawer,
                win.opened,
                function () {
                    const b = win.boxOf(0);
                    if (b.enabled || !b.checked || !win.entry(0).lastShownHere) return "the last box greys";
                    if (!win.boxOf(1).enabled || !win.boxOf(2).enabled) return win.fail("another box greys");
                    win.ev.mouseMove(b, b.width / 2, b.height / 2, -1, Qt.NoButton, Qt.NoModifier);
                    return true;
                },
                function () {
                    // (The box's own guard says it: a disabled box gets no hover; the entry
                    // around it says nothing, UI-7.)
                    const g = win.guardOf(0);
                    if (win.entry(0).toolTipShown) return win.fail("the reason shows over the whole entry");
                    return g !== null && g.tipShown && g.tipText === win.drawer.lastStaysText ? true : "the greyed box says why on its hover";
                },
                function () { win.ev.mouseMove(win.entry(0), Kirigami.Units.gridUnit, win.entry(0).height / 2, -1, Qt.NoButton, Qt.NoModifier); return true; },
                function () {
                    if (win.guardOf(0).tipShown) return "the box's tooltip goes with the pointer";
                    return !win.entry(0).toolTipShown ? true : win.fail("the entry's name shows the reason");
                },
                function () { win.click(win.boxOf(0), Qt.NoModifier); win.held = 0; return true; },
                function () {
                    if (App.shownCount !== 1 || App.currentAccount !== win.acc(0)) return win.fail("a click on the greyed box changed something");
                    if (win.stays !== 1) return "a click on the greyed box tells";
                    return win.staysOpen();
                },
                function () { win.click(win.entry(0), Qt.ShiftModifier); return true; },
                function () {
                    if (App.shownCount !== 1 || !win.shown(0)) return win.fail("the last shown account went");
                    return win.stays === 2 ? true : "Shift+click on the last shown tells";
                },
            ];
        case "menu":
            return [
                function () { win.entry(1).forceActiveFocus(); return win.entry(1).activeFocus ? true : "the entry takes the keyboard"; },
                function () { win.ev.keyClick(Qt.Key_Menu, Qt.NoModifier, -1); return true; },
                function () {
                    const m = win.drawer.accountMenu;
                    if (m === null || !m.opened) return "the menu opens";
                    const show = m.itemAt(0);
                    if (show.objectName !== "drawer-show-in-list" || !show.visible || !show.enabled) return win.fail("no 'Show in List' first");
                    if (!show.checked) return win.fail("'Show in List' unchecked for a shown account");
                    if (!m.itemAt(1).visible) return win.fail("no 'Show Only This Account'");
                    win.click(show, Qt.NoModifier);
                    return true;
                },
                function () { return !win.shown(1) && App.shownCount === 2 && !win.drawer.accountMenu.opened ? true : "'Show in List' hides it"; },
                function () { win.drawer.openAccountMenu(win.entry(1), win.acc(1), -1, -1); return true; },
                function () {
                    const m = win.drawer.accountMenu;
                    if (!m.opened) return "the menu opens again";
                    if (m.itemAt(0).checked) return win.fail("'Show in List' checked for a hidden account");
                    win.click(m.itemAt(1), Qt.NoModifier);
                    return true;
                },
                function () { return App.shownCount === 1 && App.currentAccount === win.acc(1) && !win.drawer.accountMenu.opened ? true : "'Show Only This Account'"; },
                function () { win.drawer.openAccountMenu(win.entry(1), win.acc(1), -1, -1); return true; },
                function () {
                    const m = win.drawer.accountMenu;
                    if (!m.opened) return "the menu opens a third time";
                    if (!m.itemAt(0).checked) return win.fail("'Show in List' unchecked for the shown account");
                    win.click(m.itemAt(0), Qt.NoModifier);
                    return true;
                },
                function () {
                    if (!win.shown(1) || App.shownCount !== 1) return win.fail("the last shown account went");
                    return win.stays === 1 ? true : "'Show in List' on the last shown tells";
                },
            ];
        case "off":
            return [
                function () { App.settings.blendAccounts = false; return true; },
                function () { return !App.blend ? true : "the setting reaches the App"; },
                win.openDrawer,
                win.opened,
                function () {
                    const b = win.boxOf(1);
                    if (b !== null && b.visible) return win.fail("a box with the setting off");
                    if (win.entry(1).accessibleNote !== "") return win.fail("the shown/hidden words with the setting off");
                    win.click(win.entry(1), Qt.ShiftModifier);
                    return true;
                },
                function () {
                    if (App.shownCount !== 1 || App.currentAccount !== win.acc(1)) return "Shift+click is a click";
                    if (win.entry(0).faded || win.entry(2).faded) return win.fail("an account is faded with the setting off");
                    return !win.drawer.drawerOpen ? true : "the modal drawer closes";
                },
            ];
        case "narrow":
            return [
                function () { App.toggleAccount(win.acc(1)); return true; },
                function () {
                    const t = win.chatsTitle();
                    const pill = t === null ? null : t.pill;
                    if (App.hiddenUnread <= 0 || pill === null || !pill.visible) return "the pill on the closed menu button (unread " + App.hiddenUnread + ", title " + t + ", pill " + pill + ")";
                    if (win.drawer.drawerOpen) return win.fail("the drawer is open");
                    const wrong = win.pillOnButton(pill);
                    return wrong === "" ? true : win.fail(wrong);
                },
                win.openDrawer,
                win.opened,
                function () {
                    const pill = win.chatsTitle().pill;
                    return !pill.visible ? true : win.fail("the pill shows while the drawer is open");
                },
                function () { win.drawer.close(); return true; },
                function () { return !win.drawer.drawerOpen && win.drawer.position === 0 ? true : "the drawer closes"; },
                function () { return win.chatsTitle().pill.visible ? true : "the pill again with the drawer closed"; },
                // The archive view's title carries the same pill (NEW-3).
                function () { win.listPage().openArchive(); return true; },
                function () {
                    const t = win.archiveTitle();
                    const pill = t === null ? null : t.pill;
                    if (!App.archiveShown || pill === null || !pill.visible) return "the pill on the archive's closed menu button (title " + t + ", pill " + pill + ")";
                    const wrong = win.pillOnButton(pill);
                    return wrong === "" ? true : win.fail("archive: " + wrong);
                },
                win.openDrawer,
                win.opened,
                function () { return !win.archiveTitle().pill.visible ? true : win.fail("the archive's pill shows while the drawer is open"); },
                function () { win.drawer.close(); return true; },
                function () { return !win.drawer.drawerOpen && win.drawer.position === 0 ? true : "the drawer closes"; },
                function () { return win.archiveTitle().pill.visible ? true : "the archive's pill again with the drawer closed"; },
                function () { win.listPage().closeArchive(); return true; },
                function () {
                    const t = win.chatsTitle();
                    return !App.archiveShown && t !== null && t.pill !== null && t.pill.visible ? true : "the chats' title and its pill again";
                },
                function () { App.toggleAccount(win.acc(1)); return true; },
                function () { return App.hiddenUnread === 0 && !win.chatsTitle().pill.visible ? true : "nothing hidden: no pill"; },
            ];
        case "title":
            return [
                function () { App.toggleAccount(win.acc(1)); return true; },
                function () {
                    const t = win.chatsTitle();
                    if (t === null || App.shownCount !== 2 || App.currentTitleShort === "") return "two accounts shown (title " + t + ")";
                    if (App.currentTitle.indexOf(" · ") < 0) return win.fail("the title: " + App.currentTitle);
                    if (t.width < t.fullWidth) return win.fail("the names do not fit a wide window: " + t.width);
                    return t.shownText === App.currentTitle ? true : "the names while they fit (" + t.shownText + ")";
                },
                // (The fixture's two names need about 195 px, which the narrowest window (360 px)
                // still gives them, and the list column does not go below 17 grid units: the case
                // makes the names longer by drawing the heading three times as large.)
                function () {
                    win.width = 360;
                    const h = win.find(win.chatsTitle(), x => x.objectName === "chatsHeadingText");
                    if (h === null) return win.fail("no heading in the title");
                    h.font.pixelSize = Math.round(h.font.pixelSize * 3);
                    return true;
                },
                function () {
                    const t = win.chatsTitle();
                    if (t === null || !t.visible || win.width !== 360) return "the title narrow";
                    console.warn("title narrow: room " + t.width + ", names " + t.fullWidth + ", shown '" + t.shownText + "'");
                    if (t.width >= t.fullWidth) return "the names no longer fit (room " + t.width + ")";
                    return t.shownText === App.currentTitleShort ? true : "the short form when the names do not fit (" + t.shownText + ", " + t.width + ")";
                },
                function () {
                    if (App.currentTitleShort.indexOf("2") < 0) return win.fail("the short form: " + App.currentTitleShort);
                    // The window keeps the full names (its title, the accessible name).
                    return win.listPage().title === App.currentTitle ? true : win.fail("the page's title is not the full names");
                },
            ];
        case "tab":
            return [
                function () { win.key(Qt.Key_K, Qt.ControlModifier); return true; },
                function () { return win.listPage().searchField.activeFocus ? true : "Ctrl+K: the search field"; },
                function () {
                    if (win.drawer.tabEntry !== win.drawer.allChats()) return win.fail("the Tab stop with all shown is not All Chats");
                    // ONE stop in the window's chain: the search field's link and Up/Down force the
                    // focus whatever the policy, so the policy itself is read (the walk below
                    // passed with every entry `Qt.NoFocus`: the first policy mutant lived).
                    const stops = win.drawer.entryList().filter(e => e.activeFocusOnTab);
                    if (stops.length !== 1 || stops[0] !== win.drawer.allChats()) return win.fail("the panel's Tab stops: " + stops.length);
                    win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                    return true;
                },
                function () { return win.drawer.allChats().activeFocus ? true : "Shift+Tab: the panel's Tab stop (All Chats)"; },
                // The panel is ONE stop in the window's own chain (its focus policy, not only the
                // search field's link): Tab leaves it, Shift+Tab comes back to the same entry.
                function () { win.key(Qt.Key_Tab, Qt.NoModifier); return true; },
                function () {
                    const f = win.activeFocusItem;
                    if (f === null || win.drawer.allChats().activeFocus) return "Tab leaves the panel";
                    if (win.drawer.entryList().indexOf(f) >= 0) return win.fail("Tab stops on another entry: " + f.text);
                    return true;
                },
                function () { win.key(Qt.Key_Backtab, Qt.ShiftModifier); return true; },
                function () { return win.drawer.allChats().activeFocus ? true : "Shift+Tab: back on the panel's one stop (" + win.activeFocusItem + ")"; },
                function () { win.key(Qt.Key_Down, Qt.NoModifier); return true; },
                function () { return win.entry(0).activeFocus ? true : "Down: the first account"; },
                function () { win.key(Qt.Key_Down, Qt.NoModifier); return true; },
                function () { return win.entry(1).activeFocus ? true : "Down: the second account"; },
                function () { win.key(Qt.Key_Space, Qt.ControlModifier); return true; },
                function () {
                    if (App.shownCount !== 2 || win.shown(1)) return "Ctrl+Space hides the focused account";
                    return App.currentAccount === -1 ? true : win.fail("a click behind Ctrl+Space");
                },
                function () { win.key(Qt.Key_Up, Qt.NoModifier); return true; },
                function () { return win.entry(0).activeFocus ? true : "Up: the first account"; },
                function () { App.selectAccount(win.acc(2)); return true; },
                function () { return App.currentAccount === win.acc(2) ? true : "account 3 alone"; },
                function () { win.key(Qt.Key_K, Qt.ControlModifier); return true; },
                function () { return win.listPage().searchField.activeFocus ? true : "Ctrl+K again"; },
                function () { win.key(Qt.Key_Backtab, Qt.ShiftModifier); return true; },
                function () { return win.entry(2).activeFocus ? true : "Shift+Tab: the account the list shows is the Tab stop"; },
            ];
        case "return":
            return [
                function () { win.key(Qt.Key_K, Qt.ControlModifier); return true; },
                function () { return win.listPage().searchField.activeFocus ? true : "Ctrl+K: the search field"; },
                function () { win.key(Qt.Key_Backtab, Qt.ShiftModifier); return true; },
                function () { return win.drawer.allChats().activeFocus ? true : "Shift+Tab: All Chats"; },
                function () { win.key(Qt.Key_Down, Qt.NoModifier); return true; },
                function () { return win.entry(0).activeFocus ? true : "Down: the first account"; },
                function () { win.key(Qt.Key_Return, Qt.NoModifier); return true; },
                function () { return App.shownCount === 1 && App.currentAccount === win.acc(0) ? true : "Return shows only it"; },
                function () { win.key(Qt.Key_Up, Qt.NoModifier); return true; },
                function () { return win.drawer.allChats().activeFocus ? true : "Up: All Chats"; },
                function () { win.key(Qt.Key_Enter, Qt.KeypadModifier); return true; },
                function () { return App.shownCount === 3 && App.allShown && App.currentAccount === -1 ? true : "Enter on All Chats shows all"; },
            ];
        case "one":
            return [
                win.openDrawer,
                win.opened,
                function () {
                    if (win.drawer.choosing) return win.fail("the choice is offered with one account");
                    win.click(win.entry(0), Qt.ShiftModifier);
                    return true;
                },
                function () {
                    if (App.currentAccount !== win.acc(0)) return "Shift+click is the plain click";
                    if (win.stays !== 0) return win.fail("'stays' told with one account");
                    return !win.drawer.drawerOpen ? true : "the modal drawer closes";
                },
                function () { App.selectAccount(-1); return true; },
                function () { return App.currentAccount === -1 ? true : "All Chats again"; },
                function () { win.entry(0).pressKey({ isAutoRepeat: false, accepted: false, key: Qt.Key_Space, modifiers: Qt.ControlModifier }); return true; },
                function () {
                    if (win.stays !== 0) return win.fail("'stays' told with one account (Ctrl+Space)");
                    return App.currentAccount === win.acc(0) ? true : "Ctrl+Space is the plain click";
                },
            ];
        case "replace":
            return [
                function () {
                    win.first = win.acc(0);
                    win.before = [win.acc(0), win.acc(1), win.acc(2)];
                    if (App.accountActions[0].transfer !== true) return win.fail("account 1 offers no transfer");
                    App.toggleAccount(win.first);
                    return true;
                },
                function () { return !win.shown(0) && App.shownCount === 2 ? true : "account 1 hidden"; },
                function () { App.askReplace(win.first); App.confirmReplace(win.first); return true; },
                function () {
                    let fresh = -1;
                    for (let i = 0; i < App.accountActions.length; i++) {
                        if (win.before.indexOf(win.acc(i)) < 0) fresh = i;
                    }
                    if (fresh < 0 || App.accountActions.length !== 3) return "the new account (" + App.accountActions.length + " accounts)";
                    if (win.shown(fresh)) return win.fail("the new account of the replace is shown (the choice was not kept)");
                    if (App.shownCount !== 2 || App.allShown) return win.fail("shownCount " + App.shownCount + ", allShown " + App.allShown);
                    win.held = 0;
                    return true;
                },
                function () {
                    // It stays so (no later change shows it).
                    if (App.shownCount !== 2 || App.allShown) return win.fail("the choice moved: shownCount " + App.shownCount);
                    win.held += 1;
                    return win.held >= 10 ? true : "stays hidden";
                },
            ];
        case "replacecancel":
            return [
                function () { win.first = win.acc(0); App.selectAccount(win.first); return true; },
                function () { return App.shownCount === 1 && App.currentAccount === win.first ? true : "only account 1"; },
                function () { App.askReplace(win.first); App.confirmReplace(win.first); return true; },
                function () {
                    // At the QR code the core has announced the new account.
                    if (App.login.kind !== "qr" || App.login.qr === "") return "the QR code";
                    App.login.cancel();
                    return true;
                },
                function () {
                    if (App.accountActions.length !== 2) return "the replaced account is gone (" + App.accountActions.length + ")";
                    if (App.shownCount !== 2 || !App.allShown) return "the replace ends: all shown (shownCount " + App.shownCount + ", allShown " + App.allShown + ")";
                    return App.conversations.count > 0 ? true : "the list is not empty";
                },
                function () { App.selectAccount(win.acc(0)); return true; },
                function () { return App.shownCount === 1 && App.currentAccount === win.acc(0) ? true : "only the first left"; },
                function () {
                    if (App.toggleAccount(win.acc(0))) return win.fail("the only shown account was hidden: the guard is defeated");
                    if (App.shownCount !== 1 || !win.shown(0)) return win.fail("the last shown account went");
                    return true;
                },
            ];
        case "drawerkeys":
            return win.openByKeys().concat([
                function () {
                    // All shown: the panel's stop is "All Chats".
                    if (!win.drawer.allChats().activeFocus) return win.fail("the keyboard enters the panel at " + win.activeFocusItem + ", not All Chats");
                    win.key(Qt.Key_Down, Qt.NoModifier);
                    return true;
                },
                function () { return win.entry(0).activeFocus ? true : "Down: the first account"; },
                function () { win.key(Qt.Key_Down, Qt.NoModifier); return true; },
                function () { return win.entry(1).activeFocus ? true : "Down: the second account"; },
                function () { win.key(Qt.Key_Up, Qt.NoModifier); return true; },
                function () { return win.entry(0).activeFocus ? true : "Up: the first account"; },
                function () { win.key(Qt.Key_Down, Qt.NoModifier); return true; },
                function () { return win.entry(1).activeFocus ? true : "Down again: the second account"; },
                function () { win.key(Qt.Key_Return, Qt.NoModifier); return true; },
                function () {
                    if (App.shownCount !== 1 || App.currentAccount !== win.acc(1)) return "Return shows the second account alone";
                    return !win.drawer.drawerOpen && win.drawer.position === 0 ? true : "Return closes the modal drawer";
                },
            ]).concat(win.openByKeys()).concat([
                function () {
                    // One shown: the panel's stop is that account.
                    if (!win.entry(1).activeFocus) return win.fail("the keyboard enters the panel at " + win.activeFocusItem + ", not the account shown");
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                    return true;
                },
                function () {
                    if (App.shownCount !== 1 || App.currentAccount !== win.acc(1)) return win.fail("Escape changed the choice");
                    return !win.drawer.drawerOpen && win.drawer.position === 0 ? true : "Escape closes the drawer";
                },
            ]);
        case "saved":
            return [
                function () {
                    const a = win.atStart;
                    if (a === null) return win.fail("no reading at the start");
                    console.warn("saved: at the start title '" + a.title + "', allShown " + a.allShown + ", accounts " + a.known);
                    if (a.known !== 0) return win.fail("the accounts had answered before the window was made: the case proves nothing");
                    if (a.allShown || a.title !== "") return win.fail("before the accounts answer a saved hidden set is not 'All Chats' (allShown " + a.allShown + ", title '" + a.title + "')");
                    return true;
                },
                function () {
                    if (win.shown(1) || !win.shown(0) || !win.shown(2)) return win.fail("the saved choice: account 2 hidden, the others shown");
                    return App.currentTitle !== "" && App.currentTitle.indexOf(" · ") >= 0 ? true : "the title names the two shown accounts (" + App.currentTitle + ")";
                },
            ];
        case "placeholder":
            return [
                function () { return (App.bannerShownTransfer.phase ?? "") !== "" ? true : "the fixture account's transfer"; },
                function () { App.toggleAccount(win.acc(0)); return true; },
                function () {
                    if (win.shown(0) || (App.bannerTransfer.phase ?? "") === "") return "account 1 hidden, its transfer in the banner";
                    const empty = win.find(win.contentItem, x => x.objectName === "empty-list");
                    if (empty === null) return win.fail("no empty-list placeholder");
                    if (empty.phase !== "") return win.fail("the placeholder reads a hidden account's transfer: " + empty.phase);
                    App.selectAccount(-1);
                    return true;
                },
                function () {
                    const empty = win.find(win.contentItem, x => x.objectName === "empty-list");
                    return empty.phase !== "" ? true : "a shown account's transfer reaches the placeholder";
                },
            ];
        }
        win.fail("unknown case");
        return [];
    }

    property var steps: []

    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: {
            if (win.busy) return;
            win.busy = true;
            try {
                win.tick();
            } finally {
                win.busy = false;
            }
        }
    }

    function tick(): void {
        if (win.steps.length === 0) {
            win.steps = win.common.concat(win.caseSteps());
        }
        // (Qt.exit is queued: a tick can come after the last step.)
        if (win.stepAt >= win.steps.length) return;
        let r;
        try {
            r = win.steps[win.stepAt]();
        } catch (e) {
            win.fail("threw: " + e);
            return;
        }
        if (r === true) {
            win.stepAt += 1;
            win.waits = 0;
            if (win.stepAt >= win.steps.length) {
                console.warn("accounts shown " + win.mode + ": ok");
                Qt.exit(0);
            }
            return;
        }
        win.waits += 1;
        if (win.waits > 300) {
            win.fail("never happened: " + r + " (shownCount " + App.shownCount + ", current " + App.currentAccount + ", drawer open " + win.drawer.drawerOpen + ")");
        }
    }
}
"##;

const CASES: [&str; 18] = [
    "click",
    "shift",
    "ctrl",
    "ctrlspace",
    "space",
    "box",
    "menu",
    "off",
    "narrow",
    "title",
    "tab",
    "return",
    "one",
    "placeholder",
    "replace",
    "replacecancel",
    "saved",
    "drawerkeys",
];

/// Cases that fail today on an open bug and run only when named in
/// `KOURIER_ACCOUNTS_SHOWN_ONLY` (none today; `drawerkeys` waited here until fix V5's bug was fixed).
const OPEN: [&str; 0] = [];

fn run(case: &str) -> i32 {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 4,
            accounts: if case == "one" { 1 } else { 3 },
            transfer: (case == "placeholder")
                .then_some(FixtureTransfer::Left(HistoryTransfer::Waiting)),
            // The replace cases: the test network offers "Transfer History from Phone…".
            history_on_link: case.starts_with("replace"),
            // The replace's link: scanned at once by another identity (`replace`: the new
            // account comes), or never scanned (`replacecancel`: it waits at its QR code).
            second_link: match case {
                "replace" => Some(FixtureSecondLink::OtherAccount),
                "replacecancel" => Some(FixtureSecondLink::AtQr),
                _ => None,
            },
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let start_rows = account_rows(&handle);
    let start_ids: Vec<i64> = start_rows.iter().map(|a| a.id.0).collect();
    if case == "saved" {
        // A choice saved by an earlier run: account 2 hidden (fix V3, S4's App half).
        let Some(second) = start_rows.get(1) else {
            eprintln!("FAIL [saved]: {} accounts at the start", start_rows.len());
            app.shutdown();
            return 1;
        };
        let mut prefs = handle.desktop().prefs();
        prefs.blend_accounts = true;
        prefs.hidden_accounts = vec![second.uuid.to_string()];
        handle.desktop().set_prefs(prefs);
    }
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext { app: app.handle() });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = SCRIPT.replace("CASE", case);
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/accounts_shown_ui_test.qml"),
        );
    }
    let mut code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if case == "replace" && code == 0 {
        // The stored choice names the NEW account (the replaced uuid is gone with it).
        let rows = account_rows(&handle);
        let fresh: Vec<String> = rows
            .iter()
            .filter(|a| !start_ids.contains(&a.id.0))
            .map(|a| a.uuid.to_string())
            .collect();
        let hidden = handle.desktop().prefs().hidden_accounts;
        if fresh.len() != 1 || hidden != fresh {
            eprintln!(
                "FAIL [replace]: hidden-accounts holds {} entries, the new account's uuid {}",
                hidden.len(),
                if hidden == fresh {
                    "alone"
                } else {
                    "not alone or missing"
                }
            );
            code = 1;
        }
    }
    app.shutdown();
    code
}

/// The accounts the core lists now.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn account_rows(app: &kourier_app::AppHandle) -> Vec<kourier_app::facade::AccountRow> {
    match app
        .runtime()
        .block_on(app.query(kourier_app::facade::Query::Accounts))
    {
        Ok(kourier_app::facade::QueryData::Accounts(rows)) => rows,
        _ => panic!("the accounts"),
    }
}

fn main() {
    support::answer_list("accounts_shown_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_ACCOUNTS_SHOWN_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_ACCOUNTS_SHOWN_ONLY").unwrap_or_default();
    let named = |c: &&str| only.split(',').any(|o| o == *c);
    let cases: Vec<&str> = CASES
        .into_iter()
        .filter(|c| only.is_empty() || named(c))
        .chain(OPEN.into_iter().filter(|c| named(c)))
        .collect();
    let mut failed = 0;
    for case in &cases {
        let out = Command::new(&exe)
            .env("KOURIER_ACCOUNTS_SHOWN_CASE", case)
            .output()
            .expect("run a case");
        let said = String::from_utf8_lossy(&out.stderr);
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{said}");
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        if out.status.success() && errors.is_empty() {
            println!("test accounts shown: {case} ... ok");
        } else {
            println!("test accounts shown: {case} ... FAILED ({})", out.status);
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
