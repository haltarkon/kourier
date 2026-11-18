//! Split view in the real main window (S3b1): list | chat | chat over the real core on the fake
//! network, offscreen, `harness = false`, real mouse and key events (`TestEvent`):
//!
//! - `App.openInSplit` makes the second chat page (pane 1, the active one) in the stack after the first;
//!   the inactive pane's header is dimmed;
//! - a press on the other pane's timeline activates it, a click in its composer gives it the keyboard;
//! - the composers keep their own text across activations;
//! - F6 and Shift+F6 go list, left chat, right chat, and the active pane follows;
//! - Escape reaches the active pane's selection only;
//! - closing the split removes the second page;
//! - S4b: the header's "Close Chat" (one chat, the right one, the main one: the keyboard goes to the list), the
//!   hover button of a list row ("Open in Split View" / "Close Chat", by a real mouse move), the row menu's "Close Chat"
//!   and "Swap Chats", the header's "Swap Chats" (chats, drafts and the active chat);
//! - S3b2, opening and closing: the row menu's "Open in Split View" (Menu key, a click; absent for a chat a
//!   pane shows and with no chat open), F3 (closes the inactive pane; on a focused row opens beside; else
//!   starts and ends the chooser), the header's "Open Chat Beside" (the chooser: the
//!   list's banner, Escape, the banner's close button, a real click on a row, another way of opening ends
//!   it), the chat menu's "Close Split View";
//! - a second child process at 600x800 (narrow): the header button shows the list with the banner and a
//!   click on a row shows the new chat alone.
//!
//! S4, the layouts, the divider and the restore (cases `layouts`, `restore`, `restoregone`):
//!
//! - `layouts` (1500 px start, the split on): the window made narrower step by step, full -> rail -> one (wide)
//!   -> one (not wide) -> rail -> full, and the room widened; at each width the stack and the pages shown, the
//!   draft of each composer kept; the rail's rows (avatar only, name as tooltip and accessible name, no search
//!   field), a click and the keyboard on a rail row open that chat; in "one" F6 and the header button "Show
//!   Other Chat" switch the active pane and its tooltip names the other chat; the divider: the first page's
//!   width follows `App.storeSplitWidth`, the share is clamped, an end of a drag (`interactiveResizing`
//!   switched true and then false by the script: Kirigami's separator is not driven with `TestEvent`) stores
//!   the share the page has, and the share follows the window's width;
//! - `restore`: a start with chats stored (the test seed `UiOptions::restore_seed` writes them into the
//!   fixture's own preferences, the app reads them back as a real start does): both chats open after the
//!   first frame, pane 0 stays active, the divider's share is applied, a later change of a pane is written
//!   back; `restoregone`: pane 0's chat is gone, pane 1's opens as the only chat.
//!
//! S5, the drag (case `drag`, 1500 px): a real press, moves and a release from a non-text point of a bubble in the
//! left chat (found by the row's own rule, `MessageDelegate.dragStartAt`) to the right chat: the hover names the
//! message before the press, a move under the drag distance starts nothing, past it the drag carries the message
//! (the card beside the pointer shows), the right chat's drop area holds it, the drop stages ONE message into the
//! right composer (its card shows, nothing is sent, the right pane becomes active, its text area has the keyboard);
//! a drag on a message's text selects text and carries nothing; a drop on the chat the drag came from is missed and
//! stages nothing; with two messages selected, a drag of one of them through the selection's plate carries both and
//! the drop ends the selection; without a split nothing starts. `KOURIER_SPLIT_WIN_SHOTS=<dir>` writes pictures of
//! the card over the right chat and of the staged forward, for one message and for the selection, for a look.
//!
//! The parent runs the cases as children and fails on a script error (`TypeError`, "Unable to assign").

mod support;

use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{RestoreSeed, SeedChat, UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The app's own main window is the root: list | chat | chat.
MainWindow {
    id: win

    width: SPLIT_WIDTH
    height: 800
    visible: true

    readonly property string kase: "SPLIT_CASE"
    property string step: "start"
    property int waits: 0
    property var ev: null
    property real first: 0
    property real second: 0
    property real firstMsg: 0
    property real secondMsg: 0
    property bool busy: false
    property int activePaneWas: 0
    property real third: 0
    property real wantShare: 0
    property int quiet: 0
    property var stableItem: null
    property string stableKey: ""
    property int stableTicks: 0
    property int rowOpened: 0
    property int besideAsked: 0
    property string swapTitleFirst: ""
    property string swapTitleSecond: ""
    property var swapTexts: ({})
    property var dgFrom: null
    property int dgTaken: 0
    property int dgMissed: 0
    property real dgNewest: 0
    property var dgText: null
    property var dgIds: []

    onClosedForGood: {}

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

    // A picture of the window for a look by hand, only when the run names a folder.
    function snap(name: string): void {
        if ("SPLIT_SHOTS" !== "") {
            App.saveScreenshot(win, "SPLIT_SHOTS/" + name + ".png");
        }
    }

    function key(k: int, modifiers: int): void {
        if (!win.ev.keyClick(k, modifiers, -1)) {
            win.fail("the key event was not delivered");
        }
    }

    function click(item: Item, x: real, y: real): void {
        if (!win.ev.mouseClick(item, x, y, Qt.LeftButton, Qt.NoModifier, -1)) {
            win.fail("the mouse event was not delivered");
        }
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

    // The conversation list page (the first page of the stack).
    function lp(): Item {
        return win.pageStack.get(0);
    }

    // The chooser's banner is shown (the list makes it only while the chooser stands: null otherwise).
    function bannerShown(): bool {
        const bn = win.lp().chooseBanner;
        return bn !== null && bn.visible;
    }

    // The list's row of chat `id` takes the keyboard and is the current one (false: no such row yet).
    function focusRow(id: real): bool {
        const l = win.lp().conversations;
        for (let i = 0; i < l.count; ++i) {
            const r = win.listRow(l, i);
            if (r && r.conversationId === id) {
                l.forceActiveFocus(Qt.TabFocusReason);
                l.currentIndex = i;
                return l.activeFocus && l.currentIndex === i;
            }
        }
        return false;
    }

    // The action of `page` with this object name (null: none).
    function act(page: Item, name: string): var {
        for (const a of page.actions) {
            if (a.objectName === name) {
                return a;
            }
        }
        return null;
    }

    // The header's "Open Chat Beside" / "Close This Chat" button of `page` (null: not drawn yet).
    // `item` has stood still (place and size) for three ticks in a row: a toolbar button that is still being
    // laid out takes a click on whatever lies at its old place.
    function stable(item: Item): bool {
        const at = item.mapToItem(win.contentItem, 0, 0);
        const key = at.x + "," + at.y + "," + item.width + "," + item.height;
        if (win.stableItem !== item || win.stableKey !== key) {
            win.stableItem = item;
            win.stableKey = key;
            win.stableTicks = 0;
        } else {
            win.stableTicks += 1;
        }
        return win.stableTicks >= 3;
    }

    function actionButton(page: Item, name: string): Item {
        const top = page !== null && page.globalToolBarItem !== null && page.globalToolBarItem !== undefined ? page.globalToolBarItem : win.contentItem;
        const b = win.find(top, i => i.visible && i.width > 0 && ((i.kirigamiAction !== undefined && i.kirigamiAction !== null && i.kirigamiAction.objectName === name) || (i.action !== undefined && i.action !== null && i.action.objectName === name)));
        return b !== null && win.stable(b) ? b : null;
    }

    // What a page's header shows (for a failure text): each visible item that carries an action, by the action's
    // object name or text, with its x and width, and the header's width.
    function headerSays(page: Item): string {
        if (page === null || page.globalToolBarItem === null || page.globalToolBarItem === undefined) return "no header";
        const top = page.globalToolBarItem;
        const said = [];
        const walk = item => {
            for (const c of item.children) {
                const a = c.kirigamiAction !== undefined && c.kirigamiAction !== null ? c.kirigamiAction : (c.action !== undefined && c.action !== null ? c.action : null);
                if (a !== null && c.visible) said.push((a.objectName !== "" ? a.objectName : a.text) + "@" + Math.round(c.mapToItem(top, 0, 0).x) + "+" + Math.round(c.width));
                walk(c);
            }
        };
        walk(top);
        return "header " + Math.round(top.width) + " px: " + said.join(", ");
    }

    function besideButton(page: Item): Item {
        return win.actionButton(page, "besideAction");
    }

    // The text typed in the composer of the page that shows chat `id` ("" when no page shows it).
    function textOf(id: real): string {
        const l = win.chatPage;
        const r = win.sideChatPage;
        if (l !== null && l.chatModel.conversationId === id && win.area(l) !== null) return win.area(l).text;
        if (r !== null && r.chatModel.conversationId === id && win.area(r) !== null) return win.area(r).text;
        return "";
    }

    // The hover button of a list row (null: none made).
    function hoverButtonOf(row: Item): Item {
        return row === null ? null : win.find(row, i => i.objectName === "rowHoverButton");
    }

    // The pointer moves onto the middle of the row of chat `id`.
    function hoverRow(id: real): bool {
        const row = win.rowOfConversation(id);
        if (row === null) return false;
        return win.ev.mouseMove(row, row.width / 2, row.height / 2, -1, Qt.NoButton, Qt.NoModifier);
    }

    function gu(n: real): real {
        return Kirigami.Units.gridUnit * n;
    }

    // The window width that gives the page stack `room` (the stack is the window less the account sidebar).
    function widthFor(room: real): real {
        return room + (win.width - win.splitRoom);
    }
    // The room's limits of the rule: the list (21 gu) or the rail (4 gu) and two chat columns.
    function fullLimit(): real {
        return win.lp().fullWidth + 2 * win.splitChatMin;
    }
    function railLimit(): real {
        return win.lp().railWidth + 2 * win.splitChatMin;
    }

    // The room the two chats share (what the window gives them beside the list or the rail).
    function chatRoom(): real {
        return win.splitRoom - (win.splitLayout === "rail" ? win.lp().railWidth : win.lp().fullWidth);
    }

    // What the stack shows in the layout `layout` ("full", "rail": list, chat, chat; "one": list or not, and
    // the active chat alone). "" when it is right, else what is wrong (or "wait").
    function stackProblem(layout: string): string {
        const l = win.chatPage;
        const r = win.sideChatPage;
        if (win.splitLayout !== layout) return "wait: the layout is " + win.splitLayout + ", want " + layout + " (room " + win.splitRoom + ", gu " + Kirigami.Units.gridUnit + ")";
        if (layout !== "one") {
            if (l === null || r === null || !win.inStack(l) || !win.inStack(r)) return "wait: both chats in the stack";
            if (win.pageStack.depth !== 3 || win.pageStack.get(0) !== win.lp() || win.pageStack.get(1) !== l || win.pageStack.get(2) !== r) return "wait: the stack is list, chat, chat (depth " + win.pageStack.depth + ")";
            const listWide = layout === "full" ? win.lp().fullWidth : win.lp().railWidth;
            if (Math.abs(win.lp().width - listWide) > 2) return "wait: the list is " + win.lp().width + " wide, want " + listWide;
            if (win.lp().rail !== (layout === "rail")) return "the rail flag follows the layout";
            if (l.width < win.splitChatMin - 2 || r.width < win.splitChatMin - 2) return "wait: a chat column is 20 gu at least: " + l.width + " / " + r.width;
            if (r.x + r.width > win.width + 1) return "wait: the second chat ends at " + (r.x + r.width) + " in a window of " + win.width;
            return "";
        }
        const active = win.activeChat;
        const other = App.activePane === 1 ? l : r;
        if (active === null || !win.inStack(active) || win.pageStack.lastItem !== active) return "wait: the active chat is the last page";
        if (other !== null && win.inStack(other)) return "wait: the other chat left the stack";
        if (win.pageStack.depth !== (win.wideScreen ? 2 : 2) || win.pageStack.get(0) !== win.lp()) return "wait: the stack is the list and the active chat (depth " + win.pageStack.depth + ")";
        if (win.lp().rail) return "no rail in the one-chat layout";
        return "";
    }

    function draftsKept(): bool {
        const l = win.chatPage;
        const r = win.sideChatPage;
        return l !== null && r !== null && win.area(l) !== null && win.area(r) !== null && win.area(l).text === "ab" && win.area(r).text === "cd";
    }

    // The composer's own text area (Forward's card stands first in the composer, and a combo's text field would match a search).
    function area(page: Item): Item {
        return page.composer?.textArea ?? null;
    }

    function newestMessage(page: Item): real {
        const list = page.timeline.rowList;
        for (let i = 0; i < list.count; ++i) {
            const id = list.itemAtIndex(i)?.messageId ?? 0;
            if (id > 0) {
                return id;
            }
        }
        return 0;
    }

    function bothReady(): bool {
        const l = win.chatPage;
        const r = win.sideChatPage;
        return l !== null && r !== null && win.inStack(l) && win.inStack(r)
            && App.timeline.conversationId === win.first && App.sideTimeline.conversationId === win.second
            && App.timeline.count > 0 && App.sideTimeline.count > 0 && !App.timeline.loading && !App.sideTimeline.loading
            && l.timeline.rowList.count > 0 && r.timeline.rowList.count > 0 && win.area(l) !== null && win.area(r) !== null;
    }

    // Both pages show their chats (whichever pane each is in) with rows.
    function bothReady2(): bool {
        const l = win.chatPage;
        const r = win.sideChatPage;
        return l !== null && r !== null && win.inStack(l) && win.inStack(r) && App.timeline.count > 0 && App.sideTimeline.count > 0
            && !App.timeline.loading && !App.sideTimeline.loading && l.timeline.rowList.count > 0 && r.timeline.rowList.count > 0
            && win.area(l) !== null && win.area(r) !== null;
    }

    // S5, the drag. The pointer in the window's content: a move with `buttons` held, a press and a release of the
    // left button.
    function dgMove(at: point, buttons: int): void {
        if (!win.ev.mouseMove(win.contentItem, at.x, at.y, -1, buttons, Qt.NoModifier)) win.fail("the mouse move was not delivered");
    }
    function dgPress(at: point): void {
        if (!win.ev.mousePress(win.contentItem, at.x, at.y, Qt.LeftButton, Qt.NoModifier, -1)) win.fail("the mouse press was not delivered");
    }
    function dgRelease(at: point): void {
        if (!win.ev.mouseRelease(win.contentItem, at.x, at.y, Qt.LeftButton, Qt.NoModifier, -1)) win.fail("the mouse release was not delivered");
    }
    function dgShift(at: point, dx: real, dy: real): point {
        return Qt.point(at.x + dx, at.y + dy);
    }
    // The middle of `page` in the window's content.
    function dgCentre(page: Item): point {
        return page.mapToItem(win.contentItem, page.width / 2, page.height / 2);
    }
    // A point of a message in `page`'s timeline where a drag starts (the row's own rule, `dragStartAt`), well inside
    // the view and with room for a move to the right: `{ id, at }` (`at` in the window's content); null: none yet.
    function dgPoint(page: Item): var {
        const view = page.timeline;
        const list = view.rowList;
        for (let i = 0; i < list.count; ++i) {
            const d = list.itemAtIndex(i);
            if (d === null || d.dragStartAt === undefined || (d.messageId ?? 0) <= 0) continue;
            const b = win.find(d, c => c.objectName === "bubble");
            if (b === null) continue;
            for (let y = 4; y < b.height - 2; y += 4) {
                for (let x = 4; x < b.width - 2; x += 4) {
                    const p = b.mapToItem(d, x, y);
                    const v = d.mapToItem(view, p.x, p.y);
                    if (v.x < 30 || v.y < 30 || v.x > view.width - 70 || v.y > view.height - 30) continue;
                    if (d.dragStartAt(p)) return { id: d.messageId, at: d.mapToItem(win.contentItem, p.x, p.y) };
                }
            }
        }
        return null;
    }
    // A message's text in `page`'s timeline that selects by the mouse, of 12 characters or more, whose first
    // character lies well inside the view with room for a move to the right (null: none yet).
    function dgTextItem(page: Item): var {
        const view = page.timeline;
        const list = view.rowList;
        for (let i = 0; i < list.count; ++i) {
            const d = list.itemAtIndex(i);
            if (d === null || (d.messageId ?? 0) <= 0) continue;
            const t = win.find(d, c => c.visible && c.selectByMouse === true && c.length !== undefined && c.length >= 12 && c.width >= 70);
            if (t === null) continue;
            const r = t.positionToRectangle(0);
            const v = t.mapToItem(view, r.x + 3, r.y + r.height / 2);
            if (v.x < 10 || v.y < 30 || v.y > view.height - 30 || v.x + 80 > view.width - 20) continue;
            return t;
        }
        return null;
    }
    // Two messages of `page`'s timeline that can be forwarded, their rows' middles inside the view (ids; [] for
    // none yet).
    function dgTwo(page: Item): var {
        const view = page.timeline;
        const list = view.rowList;
        const ids = [];
        for (let i = 0; i < list.count && ids.length < 2; ++i) {
            const d = list.itemAtIndex(i);
            if (d === null || d.dragStartAt === undefined || (d.messageId ?? 0) <= 0 || d.actions?.forward !== true) continue;
            const v = d.mapToItem(view, d.width / 2, d.height / 2);
            if (v.y < 30 || v.y > view.height - 30) continue;
            ids.push(d.messageId);
        }
        return ids.length === 2 ? ids : [];
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
        const left = win.chatPage;
        const right = win.sideChatPage;
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts || App.conversationAt(1) === 0) return win.later("the list");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "split_win_ui_events.qml");
            win.first = App.conversationAt(0);
            win.second = App.conversationAt(1);
            if (win.sideChatPage !== null || App.split) return win.fail("no split at the start");
            win.third = App.conversationAt(2);
            if (win.kase.indexOf("restore") === 0) {
                // The seed stores rows 1 and 2 of the list (`restore_seed`): nothing is opened here, the window
                // restores them.
                win.first = App.conversationAt(1);
                win.second = App.conversationAt(2);
            } else {
                win.openConversation(win.first);
            }
            win.next(win.kase === "narrow" ? "n-open" : win.kase === "layouts" ? "ly-open" : win.kase === "drag" ? "dg-open" : win.kase.indexOf("restore") === 0 ? "rs-wait" : "opened");
            return;
        case "opened":
            if (App.timeline.conversationId !== win.first || win.chatPage === null) return win.later("the first chat");
            win.next("opened2");
            return;
        case "opened2":
            if (App.timeline.count === 0 || App.timeline.loading) return win.later("the first chat's rows");
            App.openInSplit(win.second);
            win.next("split");
            return;
        case "split":
            if (!win.bothReady()) return win.later("both panes");
            if (App.activePane !== 1 || win.activeChat !== right) return win.fail("the new pane is the active one");
            if (!right.active || left.active) return win.fail("the pages' active flags follow the pane");
            if (win.pageStack.depth !== 3 || win.pageStack.get(1) !== left || win.pageStack.get(2) !== right) return win.fail("the stack is list, chat, chat");
            if (right.x <= left.x) return win.fail("the second chat stands right of the first");
            if (Math.abs(left.width - right.width) > 60 || right.x + right.width > win.width + 1) return win.later("the chats share the room: " + left.width + " and " + right.width + ", window " + win.width + ", list " + win.pageStack.get(0).width + ", right ends at " + (right.x + right.width));
            if (left.globalToolBarItem === null || right.globalToolBarItem === null) return win.fail("the headers exist");
            if (!left.headerDim.visible || right.headerDim.visible) return win.fail("only the inactive header is dimmed: " + left.headerDim.visible + " / " + right.headerDim.visible);
            win.next("press-left");
            return;
        case "press-left": {
            // A real press on the inactive pane's timeline activates it, and what is under the press still gets it.
            win.click(left.timeline, left.timeline.width / 2, 60);
            win.next("press-left-done");
            return;
        }
        case "press-left-done":
            if (App.activePane !== 0) return win.later("a press on the left pane activates it");
            if (win.activeChat !== left || !left.active || right.active) return win.fail("activeChat follows the press");
            if (!right.headerDim.visible || left.headerDim.visible) return win.later("the headers follow");
            win.click(right.timeline, right.timeline.width / 2, 60);
            win.next("press-right-done");
            return;
        case "press-right-done":
            if (App.activePane !== 1) return win.later("a press on the right pane activates it: right at " + right.x + " w " + right.width + " tl " + right.timeline.width + " x" + right.timeline.mapToItem(win.contentItem, 0, 0).x + " contentX " + win.pageStack.columnView.contentX + " win " + win.width + " list " + win.lp().width + " left " + left.x + "/" + left.width);
            // A press in the right pane's composer, then in the left one's: the focus follows too.
            win.click(win.area(left), 10, 10);
            win.next("typing-left");
            return;
        case "typing-left":
            if (App.activePane !== 0 || !win.area(left).activeFocus) return win.later("the left composer has the keyboard and the pane");
            win.key(Qt.Key_A, Qt.NoModifier);
            win.key(Qt.Key_B, Qt.NoModifier);
            if (win.area(left).text !== "ab") return win.fail("typed text lands in the left composer: " + win.area(left).text);
            win.click(win.area(right), 10, 10);
            win.next("typing-right");
            return;
        case "typing-right":
            if (App.activePane !== 1 || !win.area(right).activeFocus) return win.later("the right composer has the keyboard and the pane");
            win.key(Qt.Key_C, Qt.NoModifier);
            win.key(Qt.Key_D, Qt.NoModifier);
            if (win.area(right).text !== "cd") return win.fail("typed text lands in the right composer: " + win.area(right).text);
            if (win.area(left).text !== "ab") return win.fail("the left text stays: " + win.area(left).text);
            win.click(win.area(left), 10, 10);
            win.next("back-left");
            return;
        case "back-left":
            if (App.activePane !== 0 || !win.area(left).activeFocus) return win.later("back to the left composer");
            // (A click into the composer selects what it holds: the cursor goes to the end first.)
            win.key(Qt.Key_End, Qt.NoModifier);
            win.key(Qt.Key_E, Qt.NoModifier);
            if (win.area(left).text !== "abe" || win.area(right).text !== "cd") return win.fail("each text stays with its pane: " + win.area(left).text + " / " + win.area(right).text);
            win.next("f6-list");
            return;
        case "f6-list":
            // F6: list, left chat, right chat, list; Shift+F6 the other way.
            win.pageStack.get(0).conversations.forceActiveFocus(Qt.TabFocusReason);
            if (!win.within(win.activeFocusItem, win.pageStack.get(0))) return win.fail("the list has the keyboard");
            win.key(Qt.Key_F6, Qt.NoModifier);
            if (!left.timeline.rowList.activeFocus) return win.fail("F6 from the list reaches the left timeline");
            if (App.activePane !== 0) return win.fail("the left pane is active after F6");
            win.key(Qt.Key_F6, Qt.NoModifier);
            if (!right.timeline.rowList.activeFocus) return win.fail("F6 reaches the right timeline");
            if (App.activePane !== 1 || win.activeChat !== right) return win.fail("the right pane is active after F6");
            win.key(Qt.Key_F6, Qt.NoModifier);
            if (!win.within(win.activeFocusItem, win.pageStack.get(0))) return win.fail("F6 from the right chat reaches the list");
            win.key(Qt.Key_F6, Qt.ShiftModifier);
            if (!right.timeline.rowList.activeFocus || App.activePane !== 1) return win.fail("Shift+F6 from the list reaches the right chat");
            win.key(Qt.Key_F6, Qt.ShiftModifier);
            if (!left.timeline.rowList.activeFocus || App.activePane !== 0) return win.fail("Shift+F6 reaches the left chat");
            win.firstMsg = win.newestMessage(left);
            win.secondMsg = win.newestMessage(right);
            if (win.firstMsg === 0 || win.secondMsg === 0) return win.later("a message in each pane");
            win.next("select");
            return;
        case "select":
            // The selection mode is per pane.
            left.chatModel.selectStart(win.firstMsg);
            right.chatModel.selectStart(win.secondMsg);
            if (!left.chatModel.selecting || !right.chatModel.selecting) return win.fail("both panes select");
            right.timeline.rowList.forceActiveFocus(Qt.TabFocusReason);
            if (App.activePane !== 1) return win.fail("the keyboard entering the right pane activates it");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("escaped-right");
            return;
        case "escaped-right":
            if (right.chatModel.selecting) return win.later("Escape ends the active pane's selection");
            if (!left.chatModel.selecting) return win.fail("Escape reached the inactive pane");
            left.timeline.rowList.forceActiveFocus(Qt.TabFocusReason);
            if (App.activePane !== 0) return win.fail("the keyboard entering the left pane activates it");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("escaped-left");
            return;
        case "escaped-left":
            if (left.chatModel.selecting) return win.later("Escape ends the left selection");
            App.closePane(1);
            win.next("closed");
            return;
        case "closed":
            if (App.split) return win.fail("the split ended");
            if (win.sideChatPage !== null) return win.later("the second page is gone");
            if (win.pageStack.depth !== 2 || win.activeChat !== win.chatPage) return win.fail("list and chat remain, the first page is the active chat");
            if (win.chatPage.headerDim.visible) return win.fail("a lone chat's header is not dimmed");
            win.next("pm-open-row");
            return;

        // ---- the row menu's "Open in Split View" ----
        case "pm-open-row":
            // The open chat's own row: no entry.
            if (!win.focusRow(win.first)) return win.later("the open chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("pm-open-menu");
            return;
        case "pm-open-menu":
            if (!win.lp().rowMenu.opened) return win.later("the row menu of the open chat");
            if (win.lp().openBesideEntry.visible) return win.fail("no Open in Split View for the open chat");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("pm-open-closed");
            return;
        case "pm-open-closed":
            if (win.lp().rowMenu.visible) return win.later("the menu closes");
            if (!win.focusRow(win.second)) return win.later("the other chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("pm-second-menu");
            return;
        case "pm-second-menu": {
            if (!win.lp().rowMenu.opened) return win.later("the row menu of the other chat");
            const e = win.lp().openBesideEntry;
            if (!e.visible || !e.enabled) return win.fail("Open in Split View is offered for a chat no pane shows");
            if (e.text !== "Open in Split View") return win.fail("its words: " + e.text);
            if (win.lp().rowMenu.itemAt(0) !== e) return win.fail("it is the first entry");
            win.click(e, e.width / 2, e.height / 2);
            win.next("pm-second-done");
            return;
        }
        case "pm-second-done":
            if (!App.split) return win.later("the click opens the split");
            if (App.paneOf(win.second) !== 1 || App.activePane !== 1) return win.fail("the chat opened in pane 1, active");
            if (!win.bothReady()) return win.later("both panes");
            win.next("pm-shown-row");
            return;
        case "pm-shown-row":
            // A chat a pane shows has no entry (the second one, the first one).
            if (!win.focusRow(win.second)) return win.later("the shown chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("pm-shown-menu");
            return;
        case "pm-shown-menu":
            if (!win.lp().rowMenu.opened) return win.later("the row menu of a shown chat");
            if (win.lp().openBesideEntry.visible) return win.fail("no Open in Split View for a chat a pane shows");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("pm-shown-closed");
            return;
        case "pm-shown-closed":
            if (win.lp().rowMenu.visible) return win.later("the menu closes");
            win.next("f3-close");
            return;

        // ---- F3 with the split on closes the inactive pane ----
        case "f3-close":
            if (!win.focusRow(win.second)) return win.later("a row has the keyboard");
            win.key(Qt.Key_F3, Qt.NoModifier);
            win.next("f3-closed");
            return;
        case "f3-closed":
            if (App.split || win.sideChatPage !== null) return win.later("F3 closes the split");
            if (App.timeline.conversationId !== win.second) return win.later("the active (right) chat stays, now on the left");
            win.next("cl-split");
            return;

        // ---- "Close Chat" in the LEFT header ----
        case "cl-split":
            App.openInSplit(win.first);
            win.next("cl-click");
            return;
        case "cl-click": {
            if (!App.split || win.sideChatPage === null || !win.inStack(win.sideChatPage) || App.timeline.conversationId !== win.second || App.sideTimeline.conversationId !== win.first || App.sideTimeline.count === 0 || App.sideTimeline.loading) return win.later("both panes (second | first)");
            const l = win.chatPage;
            const r = win.sideChatPage;
            for (const page of [l, r]) {
                const c = win.act(page, "closeAction");
                if (c === null || !c.visible || c.text !== "Close Chat" || c.icon.name !== "view-close") return win.fail("both headers offer Close Chat while the split is on");
                if (win.act(page, "besideAction").visible) return win.fail("Open Chat Beside is not offered while the split is on");
                const w = win.act(page, "swapAction");
                if (w === null || !w.visible || w.text !== "Swap Chats" || w.icon.name !== "exchange-positions") return win.fail("both headers offer Swap Chats while the split is on");
            }
            if (win.act(l, "closeSplitAction") === null || !win.act(l, "closeSplitAction").visible) return win.fail("the chat menu offers Close Split View");
            const b = win.actionButton(l, "closeAction");
            if (b === null) return win.later("the left header's Close Chat (" + win.headerSays(l) + ")");
            win.click(b, b.width / 2, b.height / 2);
            win.next("cl-closed");
            return;
        }
        case "cl-closed":
            if (App.split || win.sideChatPage !== null) return win.later("the left chat closed and the split ended");
            if (App.timeline.conversationId !== win.first) return win.later("the right chat moved left");
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard after the main chat closed");
            if (win.act(win.chatPage, "besideAction").text !== "Open Chat Beside" || !win.act(win.chatPage, "besideAction").visible) return win.fail("the button offers Open Chat Beside again");
            if (!win.act(win.chatPage, "closeAction").visible || win.act(win.chatPage, "swapAction").visible) return win.fail("Close Chat stays, Swap Chats goes with the split");
            if (win.act(win.chatPage, "closeSplitAction").visible) return win.fail("no Close Split View without a split");
            win.next("ch-click");
            return;

        // ---- "Open Chat Beside": the chooser ----
        case "ch-click": {
            const b = win.besideButton(win.chatPage);
            if (b === null) return win.later("the header's Open Chat Beside");
            win.click(b, b.width / 2, b.height / 2);
            win.next("ch-banner");
            return;
        }
        case "ch-banner": {
            const bn = win.lp().chooseBanner;
            if (!win.choosingBeside || bn === null || !bn.visible) return win.later("the chooser and its banner");
            const title = win.chatPage.chatModel.title;
            if (bn.text.indexOf(title) < 0 || win.choosingName !== title) return win.fail("the banner names the open chat: " + bn.text);
            win.next("ch-focus");
            return;
        }
        case "ch-focus":
            if (!win.within(win.activeFocusItem, win.lp())) return win.later("the list has the keyboard");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("ch-esc");
            return;
        case "ch-esc":
            if (win.choosingBeside || win.bannerShown()) return win.later("Escape ends the chooser");
            if (App.split) return win.fail("nothing opened beside");
            win.next("ch-click2");
            return;
        case "ch-click2": {
            const b = win.besideButton(win.chatPage);
            if (b === null) return win.later("the header's button");
            win.click(b, b.width / 2, b.height / 2);
            win.next("ch-pick");
            return;
        }
        case "ch-pick": {
            const row = win.rowOfConversation(win.second);
            if (!win.choosingBeside || !win.bannerShown() || row === null) return win.later("the chooser and a row");
            win.click(row, row.width / 2, row.height / 2);
            win.next("ch-picked");
            return;
        }
        case "ch-picked":
            if (!App.split) return win.later("the click opens beside");
            if (win.choosingBeside || win.bannerShown()) return win.fail("the chooser ended with the pick");
            if (App.paneOf(win.second) !== 1 || App.activePane !== 1 || App.paneOf(win.first) !== 0) return win.fail("the picked chat is pane 1, active");
            if (!win.bothReady()) return win.later("both panes");
            win.next("ch-x-split");
            return;
        case "ch-x-split":
            App.closePane(1);
            win.next("ch-x-click");
            return;
        case "ch-x-click": {
            if (App.split || win.sideChatPage !== null) return win.later("the split ended");
            const b = win.besideButton(win.chatPage);
            if (b === null) return win.later("the header's button");
            win.click(b, b.width / 2, b.height / 2);
            win.next("ch-x-banner");
            return;
        }
        case "ch-x-banner": {
            const bn = win.lp().chooseBanner;
            if (!win.choosingBeside || bn === null || !bn.visible) return win.later("the chooser");
            const x = win.find(bn, i => i.visible && i.width > 0 && i.icon !== undefined && i.icon.name === "dialog-close");
            if (x === null) return win.later("the banner's close button");
            // (The banner grows open: the click waits until the button lies inside it.)
            const at = x.mapToItem(bn, 0, 0);
            if (at.y < 0 || at.y + x.height > bn.height + 0.5 || !win.stable(x)) return win.later("the banner is open round its button");
            win.click(x, x.width / 2, x.height / 2);
            win.next("ch-x-done");
            return;
        }
        case "ch-x-done":
            if (win.choosingBeside || win.bannerShown()) return win.later("the close button ends the chooser");
            if (App.split) return win.fail("nothing opened beside");
            win.next("ch-o-click");
            return;
        case "ch-o-click": {
            const b = win.besideButton(win.chatPage);
            if (b === null) return win.later("the header's button");
            win.click(b, b.width / 2, b.height / 2);
            win.next("ch-o-open");
            return;
        }
        case "ch-o-open":
            // Any other way of opening a chat (the dev control's call) ends the chooser and replaces.
            if (!win.choosingBeside) return win.later("the chooser");
            win.openConversation(win.second);
            win.next("ch-o-done");
            return;
        case "ch-o-done":
            if (win.choosingBeside) return win.later("another open ends the chooser");
            if (App.split) return win.fail("it did not open beside");
            if (App.timeline.conversationId !== win.second) return win.later("it replaced the open chat");
            win.openConversation(win.first);
            win.next("cs-open");
            return;

        // ---- "Close Split View" from the right page's menu ----
        case "cs-open":
            if (App.timeline.conversationId !== win.first) return win.later("the first chat");
            App.openInSplit(win.second);
            win.next("cs-menu");
            return;
        case "cs-menu": {
            if (!win.bothReady()) return win.later("both panes");
            const a = win.act(win.sideChatPage, "closeSplitAction");
            if (a === null || !a.visible || !a.enabled || a.text !== "Close Split View") return win.fail("the right page's menu offers Close Split View");
            a.trigger();
            win.next("cs-done");
            return;
        }
        case "cs-done":
            if (App.split || win.sideChatPage !== null) return win.later("the split ended");
            if (App.timeline.conversationId !== win.second) return win.later("the right chat stays alone");
            win.next("f3-row");
            return;

        // ---- F3 ----
        case "f3-row":
            // On a focused row whose chat is not open: opens it beside.
            if (!win.focusRow(win.first)) return win.later("a row has the keyboard");
            win.key(Qt.Key_F3, Qt.NoModifier);
            win.next("f3-row-done");
            return;
        case "f3-row-done":
            if (!App.split) return win.later("F3 on a row opens beside");
            if (App.paneOf(win.first) !== 1 || App.activePane !== 1) return win.fail("the row's chat is pane 1, active");
            App.closePane(1);
            win.next("f3-choose");
            return;
        case "f3-choose":
            // Elsewhere with a chat open: the chooser starts; F3 again ends it.
            if (App.split || win.sideChatPage !== null) return win.later("the split ended");
            win.area(win.chatPage).forceActiveFocus(Qt.TabFocusReason);
            if (win.lp().conversations.activeFocus) return win.fail("the list gave the keyboard up");
            win.key(Qt.Key_F3, Qt.NoModifier);
            win.next("f3-chooser");
            return;
        case "f3-chooser":
            if (!win.choosingBeside) return win.later("F3 starts the chooser");
            if (!win.within(win.activeFocusItem, win.lp())) return win.later("the keyboard goes to the list");
            win.key(Qt.Key_F3, Qt.NoModifier);
            win.next("f3-ended");
            return;
        case "f3-ended":
            if (win.choosingBeside) return win.later("F3 ends the chooser");
            if (App.split) return win.fail("nothing opened beside");
            win.next("x-one");
            return;

        // ---- S4b: Close Chat with one chat open ----
        case "x-one": {
            if (App.timeline.conversationId !== win.second || win.chatPage === null) return win.later("the one chat");
            const b = win.actionButton(win.chatPage, "closeAction");
            if (b === null) return win.later("the header's Close Chat");
            win.click(b, b.width / 2, b.height / 2);
            win.next("x-one-done");
            return;
        }
        case "x-one-done":
            if (App.timeline.conversationId !== 0) return win.later("the chat closed");
            if (win.chatPage.open) return win.fail("the placeholder shows: no chat open");
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard after the close");
            win.next("x-nohover");
            return;
        case "x-nohover": {
            // No chat open: no hover button, though the row is hovered.
            const row = win.rowOfConversation(win.first);
            if (row === null) return win.later("a row");
            if (!win.hoverRow(win.first)) return win.fail("the mouse move was not delivered");
            win.quiet = 0;
            win.next("x-nohover2");
            return;
        }
        case "x-nohover2": {
            const row = win.rowOfConversation(win.first);
            if (!row.hovered) return win.later("the row is hovered");
            if (++win.quiet < 10) return;
            if (win.hoverButtonOf(row) !== null) return win.fail("no hover button while no chat is open");
            win.openConversation(win.first);
            win.next("x-open1");
            return;
        }
        case "x-open1":
            if (App.timeline.conversationId !== win.first || App.timeline.count === 0 || App.timeline.loading || win.chatPage === null) return win.later("the first chat");
            win.lp().openConversation.connect(id => { win.rowOpened += 1; });
            win.lp().openBesideAsked.connect(id => { win.besideAsked += 1; });
            if (!win.hoverRow(win.second)) return win.fail("the mouse move was not delivered");
            win.next("x-hover-open");
            return;

        // ---- the hover button: not shown -> "Open in Split View"; shown -> "Close Chat" ----
        case "x-hover-open": {
            const row = win.rowOfConversation(win.second);
            const b = win.hoverButtonOf(row);
            if (b === null || !b.visible || !b.width || !row.hovered) return win.later("the hover button of a row that is not shown");
            if (b.text !== "Open in Split View" || b.icon.name !== "view-split-left-right" || b.Accessible.name !== "Open in Split View" || b.QQC2.ToolTip.text !== "Open in Split View") return win.fail("an unshown row offers Open in Split View: " + b.text + " / " + b.icon.name);
            if (b.focusPolicy !== Qt.NoFocus) return win.fail("the button never takes the keyboard");
            // Its size does not hang on the theme's icon (here there is none): 12 px put its middle on the scroll bar.
            if (b.width < Kirigami.Units.iconSizes.smallMedium || b.height < Kirigami.Units.iconSizes.smallMedium) return win.fail("the hover button is " + b.width + "x" + b.height + " px");
            if (!win.stable(b)) return win.later("the button stands still");
            win.click(b, b.width / 2, b.height / 2);
            win.next("x-hover-opened");
            return;
        }
        case "x-hover-opened": {
            if (!App.split) {
                const row = win.rowOfConversation(win.second);
                return win.later("the button opens beside (the list asked " + win.besideAsked + " time(s), row hovered " + (row !== null && row.hovered) + ", button " + (win.hoverButtonOf(row) !== null) + ", rows opened " + win.rowOpened + ")");
            }
            if (App.paneOf(win.second) !== 1 || App.paneOf(win.first) !== 0 || App.activePane !== 1) return win.fail("the chat opened in pane 1, active");
            if (win.rowOpened !== 0) return win.fail("the click did not also open the row: " + win.rowOpened);
            // The button stays under the pointer and its meaning follows the change: Close Chat now.
            const b = win.hoverButtonOf(win.rowOfConversation(win.second));
            if (b === null || b.text !== "Close Chat") return win.later("the icon flips to Close Chat under the pointer");
            if (b.icon.name !== "view-close" || b.Accessible.name !== "Close Chat" || b.QQC2.ToolTip.text !== "Close Chat") return win.fail("Close Chat's icon, name and tooltip");
            if (!win.bothReady()) return win.later("both panes");
            win.next("x-hover-close-right");
            return;
        }
        case "x-hover-close-right": {
            const b = win.hoverButtonOf(win.rowOfConversation(win.second));
            if (b === null || !win.stable(b)) return win.later("the button of the shown row");
            win.click(b, b.width / 2, b.height / 2);
            win.next("x-hover-closed-right");
            return;
        }
        case "x-hover-closed-right":
            if (App.split) return win.later("the button closes the right chat");
            if (App.timeline.conversationId !== win.first) return win.fail("the left chat stays alone");
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard");
            if (win.rowOpened !== 0) return win.fail("the click did not open the row: " + win.rowOpened);
            App.openInSplit(win.second);
            win.next("x-hover-left");
            return;
        case "x-hover-left": {
            if (!win.bothReady()) return win.later("both panes");
            if (!win.hoverRow(win.first)) return win.fail("the mouse move was not delivered");
            win.next("x-hover-left2");
            return;
        }
        case "x-hover-left2": {
            // The main (left) chat's row offers Close Chat too; it closes the main chat: the right one is the single chat.
            const row = win.rowOfConversation(win.first);
            const b = win.hoverButtonOf(row);
            if (b === null || !b.visible || !row.hovered || b.text !== "Close Chat" || !win.stable(b)) return win.later("Close Chat on the main chat's row");
            win.click(b, b.width / 2, b.height / 2);
            win.next("x-hover-left-done");
            return;
        }
        case "x-hover-left-done":
            if (App.split || win.sideChatPage !== null) return win.later("the split ended");
            if (App.timeline.conversationId !== win.second) return win.later("the right chat is the single chat");
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard");
            if (win.rowOpened !== 0) return win.fail("no row was opened by a button: " + win.rowOpened);
            win.next("x-menu-one");
            return;

        // ---- the row menu with one chat open ----
        case "x-menu-one":
            if (!win.focusRow(win.second)) return win.later("the shown chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("x-menu-one-open");
            return;
        case "x-menu-one-open": {
            const m = win.lp();
            if (!m.rowMenu.opened) return win.later("the menu of the open chat");
            if (!m.closeChatEntry.visible || m.closeChatEntry.text !== "Close Chat" || m.closeChatEntry.icon.name !== "view-close") return win.fail("a shown row offers Close Chat");
            if (m.swapChatsEntry.visible || m.openBesideEntry.visible) return win.fail("no Swap Chats without a split, no Open in Split View for a shown chat");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("x-menu-one-closed");
            return;
        }
        case "x-menu-one-closed":
            if (win.lp().rowMenu.visible) return win.later("the menu closes");
            if (!win.focusRow(win.first)) return win.later("the other chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("x-menu-one-other");
            return;
        case "x-menu-one-other": {
            const m = win.lp();
            if (!m.rowMenu.opened) return win.later("the menu of a chat that is not shown");
            if (m.closeChatEntry.visible || m.swapChatsEntry.visible) return win.fail("neither Close Chat nor Swap Chats for a chat that is not shown");
            if (!m.openBesideEntry.visible) return win.fail("Open in Split View stays");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("x-menu-close");
            return;
        }
        case "x-menu-close": {
            if (win.lp().rowMenu.visible) return win.later("the menu closes");
            if (!win.focusRow(win.second)) return win.later("the shown chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("x-menu-close-click");
            return;
        }
        case "x-menu-close-click": {
            const m = win.lp();
            if (!m.rowMenu.opened || !m.closeChatEntry.visible) return win.later("the menu of the shown chat");
            win.click(m.closeChatEntry, m.closeChatEntry.width / 2, m.closeChatEntry.height / 2);
            win.next("x-menu-close-done");
            return;
        }
        case "x-menu-close-done":
            if (App.timeline.conversationId !== 0) return win.later("the entry closes the chat");
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard");
            win.openConversation(win.first);
            win.next("x-sw-open");
            return;

        // ---- the swap: by the header icon and by the row menu ----
        case "x-sw-open":
            if (App.timeline.conversationId !== win.first || App.timeline.count === 0 || App.timeline.loading) return win.later("the first chat");
            App.openInSplit(win.second);
            win.next("x-sw-both");
            return;
        case "x-sw-both":
            if (!win.bothReady()) return win.later("both panes");
            win.click(win.area(left), 10, 10);
            win.next("x-sw-tl");
            return;
        case "x-sw-tl":
            if (App.activePane !== 0 || !win.area(left).activeFocus) return win.later("the left composer");
            win.key(Qt.Key_A, Qt.NoModifier);
            win.key(Qt.Key_B, Qt.NoModifier);
            win.click(win.area(right), 10, 10);
            win.next("x-sw-tr");
            return;
        case "x-sw-tr":
            if (App.activePane !== 1 || !win.area(right).activeFocus) return win.later("the right composer");
            win.key(Qt.Key_C, Qt.NoModifier);
            win.key(Qt.Key_D, Qt.NoModifier);
            if (win.textOf(win.first) !== "ab" || win.textOf(win.second) !== "cd") return win.fail("the typed texts: " + win.textOf(win.first) + " / " + win.textOf(win.second));
            win.next("x-sw-click");
            return;
        case "x-sw-click": {
            // The right chat (second) is active; the header's Swap Chats is a real click.
            const b = win.actionButton(left, "swapAction");
            if (b === null) return win.later("the header's Swap Chats");
            win.swapTitleFirst = left.chatModel.title;
            win.swapTitleSecond = right.chatModel.title;
            win.click(b, b.width / 2, b.height / 2);
            win.next("x-sw-done");
            return;
        }
        case "x-sw-done":
            if (App.paneOf(win.second) !== 0 || App.paneOf(win.first) !== 1) return win.later("the chats exchanged places");
            if (!win.bothReady2()) return win.later("both panes show their new chats");
            if (App.activePane !== 0 || win.activeChat !== left) return win.fail("the active chat (second) stays active: pane " + App.activePane);
            if (left.chatModel.title !== win.swapTitleSecond || right.chatModel.title !== win.swapTitleFirst) return win.fail("the pages' titles follow: " + left.chatModel.title + " / " + right.chatModel.title);
            if (win.area(left).text !== "cd" || win.area(right).text !== "ab") return win.later("each draft follows its chat: " + win.area(left).text + " / " + win.area(right).text);
            if (!win.area(left).activeFocus) return win.later("the keyboard is in the active chat's composer");
            if (!left.active || right.active) return win.fail("the active flags follow");
            win.next("x-sw-menu");
            return;
        case "x-sw-menu":
            if (!win.focusRow(win.first)) return win.later("a shown row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("x-sw-menu-open");
            return;
        case "x-sw-menu-open": {
            const m = win.lp();
            if (!m.rowMenu.opened) return win.later("the menu of a shown chat");
            if (!m.closeChatEntry.visible || !m.swapChatsEntry.visible || m.swapChatsEntry.text !== "Swap Chats" || m.swapChatsEntry.icon.name !== "exchange-positions") return win.fail("a shown row offers Close Chat and Swap Chats with the split on");
            if (m.openBesideEntry.visible) return win.fail("no Open in Split View for a shown chat");
            win.key(Qt.Key_Down, Qt.NoModifier);
            win.key(Qt.Key_Down, Qt.NoModifier);
            if (m.rowMenu.itemAt(m.rowMenu.currentIndex) !== m.swapChatsEntry) return win.fail("Down, Down reaches Swap Chats: index " + m.rowMenu.currentIndex);
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("x-sw-menu-done");
            return;
        }
        case "x-sw-menu-done":
            if (App.paneOf(win.first) !== 0 || App.paneOf(win.second) !== 1) return win.later("the menu's swap exchanged them again");
            if (App.activePane !== 1) return win.fail("the active chat (second) follows to pane 1: " + App.activePane);
            if (!win.bothReady2()) return win.later("both panes");
            if (win.area(left).text !== "ab" || win.area(right).text !== "cd") return win.later("the drafts follow again: " + win.area(left).text + " / " + win.area(right).text);
            if (!win.lp().conversations.activeFocus) return win.later("the keyboard stays on the list");
            win.next("x-sw-menu-other");
            return;
        case "x-sw-menu-other":
            // A row whose chat is not shown has neither entry (the third chat).
            if (!win.focusRow(win.third)) return win.later("the third chat's row");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("x-sw-menu-other-open");
            return;
        case "x-sw-menu-other-open": {
            const m = win.lp();
            if (!m.rowMenu.opened) return win.later("the menu of a chat that is not shown");
            if (m.closeChatEntry.visible || m.swapChatsEntry.visible) return win.fail("neither Close Chat nor Swap Chats for a chat no pane shows");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("x-sw-menu-other-closed");
            return;
        }
        case "x-sw-menu-other-closed":
            if (win.lp().rowMenu.visible) return win.later("the menu closes");
            App.closePane(1);
            win.next("x-sw-end");
            return;
        case "x-sw-end":
            if (App.split || win.sideChatPage !== null) return win.later("the split ended");
            win.next("nochat-close");
            return;

        // ---- no chat open: no entry ----
        case "nochat-close":
            App.closePane(0);
            win.next("nochat-row");
            return;
        case "nochat-row":
            if (App.timeline.conversationId !== 0) return win.later("the chat closed");
            if (!win.focusRow(win.first)) return win.later("a row has the keyboard");
            win.key(Qt.Key_Menu, Qt.NoModifier);
            win.next("nochat-menu");
            return;
        case "nochat-menu":
            if (!win.lp().rowMenu.opened) return win.later("the row menu");
            if (win.lp().openBesideEntry.visible) return win.fail("no Open in Split View with no chat open");
            win.key(Qt.Key_Escape, Qt.NoModifier);
            win.next("nochat-closed");
            return;
        case "nochat-closed":
            if (win.lp().rowMenu.visible) return win.later("the menu closes");
            console.warn("PASS");
            Qt.exit(0);
            return;


        // ---- S4: the layouts, the rail, the divider ----
        case "ly-open":
            win.openConversation(win.first);
            win.next("ly-open2");
            return;
        case "ly-open2":
            if (App.timeline.conversationId !== win.first || win.chatPage === null || App.timeline.count === 0 || App.timeline.loading) return win.later("the first chat");
            App.openInSplit(win.second);
            win.next("ly-both");
            return;
        case "ly-both":
            if (!win.bothReady()) return win.later("both panes");
            if (win.stackProblem("full") !== "") return win.later(win.stackProblem("full"));
            win.click(win.area(left), 10, 10);
            win.next("ly-type-l");
            return;
        case "ly-type-l":
            if (App.activePane !== 0 || !win.area(left).activeFocus) return win.later("the left composer");
            win.key(Qt.Key_A, Qt.NoModifier);
            win.key(Qt.Key_B, Qt.NoModifier);
            win.click(win.area(right), 10, 10);
            win.next("ly-type-r");
            return;
        case "ly-type-r":
            if (App.activePane !== 1 || !win.area(right).activeFocus) return win.later("the right composer");
            win.key(Qt.Key_C, Qt.NoModifier);
            win.key(Qt.Key_D, Qt.NoModifier);
            if (!win.draftsKept()) return win.fail("the typed texts: " + win.area(left).text + " / " + win.area(right).text);
            win.width = win.widthFor((win.railLimit() + win.fullLimit()) / 2);
            win.next("ly-rail");
            return;
        case "ly-rail": {
            const p = win.stackProblem("rail");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            // The rail: no search field, rows with the avatar only; the name is the tooltip and the accessible name.
            if (win.lp().header.visible) return win.fail("the rail has no search field or banner");
            const l = win.lp().conversations;
            const row = win.listRow(l, 0);
            if (row === null || row === undefined || !row.rail) return win.later("a rail row");
            if (row.Accessible.name !== Style.unmarked(row.title) || row.Accessible.name === "") return win.fail("the accessible name is the chat's name: " + row.Accessible.name);
            if (row.QQC2.ToolTip.text !== Style.plain(row.title) || row.QQC2.ToolTip.text === "") return win.fail("the tooltip is the chat's name: " + row.QQC2.ToolTip.text);
            if (row.width > win.lp().railWidth + 1) return win.fail("a rail row is as wide as the rail: " + row.width);
            win.next("ly-rail-click");
            return;
        }
        case "ly-rail-click": {
            // A click on a rail row opens that chat in the active pane (as the list does).
            const row = win.rowOfConversation(win.third);
            if (row === null) return win.later("the third chat's row");
            win.activePaneWas = App.activePane;
            win.click(row, row.width / 2, row.height / 2);
            win.next("ly-rail-clicked");
            return;
        }
        case "ly-rail-clicked":
            if (App.paneOf(win.third) !== win.activePaneWas) return win.later("the clicked chat is in the active pane: pane of third " + App.paneOf(win.third) + " active " + App.activePane + " first " + App.paneOf(win.first) + " second " + App.paneOf(win.second) + " panes " + App.timeline.conversationId + "/" + App.sideTimeline.conversationId + " third " + win.third);
            // The keyboard: Down four times from no row and Return open the fourth chat.
            win.lp().conversations.forceActiveFocus(Qt.TabFocusReason);
            win.lp().conversations.currentIndex = -1;
            for (let i = 0; i < 4; ++i) win.key(Qt.Key_Down, Qt.NoModifier);
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("ly-rail-keys");
            return;
        case "ly-rail-keys": {
            const id = App.conversationAt(3);
            if (App.paneOf(id) < 0) return win.later("Return on a rail row opens its chat");
            // (Back to the two chats the steps below know.)
            App.openConversation(App.activePane === 1 ? win.second : win.first);
            win.next("ly-rail-back");
            return;
        }
        case "ly-rail-back":
            if (App.timeline.conversationId !== win.first || App.sideTimeline.conversationId !== win.second) {
                // The two chats as they were: pane 0 first, pane 1 second.
                if (App.paneOf(win.first) < 0) App.openConversation(win.first);
                if (App.paneOf(win.second) < 0) App.openInSplit(win.second);
                return win.later("the two chats again");
            }
            if (!win.bothReady()) return win.later("both panes");
            if (!win.draftsKept()) return win.later("the drafts after the clicks");
            {
                // A wide window (46 gu) whose room is under the rail's limit: it exists only in the band between
                // 46 gu and the rail limit plus the sidebar; the test takes the middle of it, and skips wide
                // "one" when the band is under half a grid unit (the narrow one below is tested anyway).
                const lo = win.gu(46);
                const hi = win.railLimit() + (win.width - win.splitRoom);
                if (hi - lo < win.gu(0.5)) {
                    console.warn("no wide window is under the rail limit: " + lo + " .. " + hi);
                    win.width = win.gu(40);
                    win.next("ly-narrow");
                    return;
                }
                win.width = (lo + hi) / 2;
            }
            win.next("ly-one");
            return;
        case "ly-one": {
            // Wide (46 gu) but too narrow for the rail and two chats: the list and the active chat.
            const p = win.stackProblem("one");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            if (!win.wideScreen) return win.fail("47 gu is a wide window");
            const a = win.act(win.activeChat, "otherChatAction");
            if (a === null || !a.visible || a.text !== "Show Other Chat") return win.fail("the header offers Show Other Chat");
            const otherTitle = App.activePane === 1 ? App.timeline.title : App.sideTimeline.title;
            if (a.tooltip !== "Show " + Style.plain(otherTitle)) return win.fail("the tooltip names the other chat: " + a.tooltip);
            win.activePaneWas = App.activePane;
            win.key(Qt.Key_F6, Qt.NoModifier);
            win.next("ly-f6");
            return;
        }
        case "ly-f6": {
            if (App.activePane === win.activePaneWas) return win.later("F6 switches the active pane");
            const p = win.stackProblem("one");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            const a = win.act(win.activeChat, "otherChatAction");
            const otherTitle = App.activePane === 1 ? App.timeline.title : App.sideTimeline.title;
            if (!a.visible || a.tooltip !== "Show " + Style.plain(otherTitle)) return win.fail("the tooltip follows: " + a.tooltip);
            win.activePaneWas = App.activePane;
            win.next("ly-btn");
            return;
        }
        case "ly-btn": {
            const b = win.actionButton(win.activeChat, "otherChatAction");
            if (b === null) return win.later("the header's Show Other Chat button");
            win.click(b, b.width / 2, b.height / 2);
            win.next("ly-btn-done");
            return;
        }
        case "ly-btn-done": {
            if (App.activePane === win.activePaneWas) return win.later("the button switches the active pane");
            const p = win.stackProblem("one");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            win.width = win.gu(40);
            win.next("ly-narrow");
            return;
        }
        case "ly-narrow": {
            if (win.wideScreen) return win.later("40 gu is not a wide window");
            const p = win.stackProblem("one");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            win.activePaneWas = App.activePane;
            win.key(Qt.Key_F6, Qt.ShiftModifier);
            win.next("ly-narrow-f6");
            return;
        }
        case "ly-narrow-f6": {
            if (App.activePane === win.activePaneWas) return win.later("Shift+F6 switches the active pane");
            const p = win.stackProblem("one");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            win.width = win.widthFor((win.railLimit() + win.fullLimit()) / 2);
            win.next("ly-rail2");
            return;
        }
        case "ly-rail2": {
            const p = win.stackProblem("rail");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            if (!win.draftsKept()) return win.later("the drafts after the window was narrow: " + (win.area(left) ? win.area(left).text : "?") + " / " + (win.area(right) ? win.area(right).text : "?"));
            win.width = win.widthFor(win.fullLimit() + win.gu(4));
            win.next("ly-full");
            return;
        }
        case "ly-full": {
            const p = win.stackProblem("full");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            if (!win.draftsKept()) return win.later("the drafts after the window was wide again");
            // The share 0.5 of the room: both chats equal.
            if (Math.abs(left.width - win.chatRoom() * 0.5) > 3) return win.later("the first chat has half of the room: " + left.width + " of " + win.chatRoom());
            // (A room where 0.25 of it is still a whole column: the divider steps below.)
            win.width = win.gu(100);
            win.next("ly-full100");
            return;
        }
        case "ly-full100": {
            const p = win.stackProblem("full");
            if (p !== "") return p.indexOf("wait") === 0 ? win.later(p) : win.fail(p);
            if (Math.abs(left.width - win.chatRoom() * 0.5) > 3) return win.later("half of the wide room: " + left.width + " of " + win.chatRoom());
            App.storeSplitWidth(0.4);
            win.next("ly-share");
            return;
        }
        case "ly-share":
            if (Math.abs(left.width - win.chatRoom() * 0.4) > 3) return win.later("the first chat follows the share 0.4: " + left.width + " of " + win.chatRoom());
            // The end of a drag stores the share the page has: the page is made 0.6 of the room while a drag
            // "stands", then the drag ends.
            left.Kirigami.ColumnView.interactiveResizing = true;
            left.Kirigami.ColumnView.preferredWidth = win.chatRoom() * 0.6;
            win.next("ly-drag");
            return;
        case "ly-drag":
            if (Math.abs(left.width - win.chatRoom() * 0.6) > 3) return win.later("the dragged column: " + left.width + " of " + win.chatRoom());
            if (Math.abs(App.splitWidth - 0.4) > 0.001) return win.fail("nothing is stored while the drag goes on: " + App.splitWidth);
            left.Kirigami.ColumnView.interactiveResizing = false;
            win.next("ly-dragged");
            return;
        case "ly-dragged":
            if (Math.abs(App.splitWidth - 0.6) > 0.02) return win.later("the end of the drag stored the share: " + App.splitWidth);
            // The share follows the window.
            win.width = win.gu(120);
            win.next("ly-wider");
            return;
        case "ly-wider":
            if (Math.abs(left.width - win.chatRoom() * App.splitWidth) > 3 || win.splitRoom < win.gu(115)) return win.later("the share follows a wider window: " + left.width + " of " + win.chatRoom());
            // Clamped: 0.25 .. 0.75, and a column never under 20 gu.
            App.storeSplitWidth(0.95);
            win.next("ly-clamp-hi");
            return;
        case "ly-clamp-hi":
            if (Math.abs(App.splitWidth - 0.75) > 0.0001) return win.fail("the share is at most 0.75: " + App.splitWidth);
            if (Math.abs(left.width - win.chatRoom() * 0.75) > 3) return win.later("0.75 of the room");
            App.storeSplitWidth(0.05);
            win.next("ly-clamp-lo");
            return;
        case "ly-clamp-lo":
            if (Math.abs(App.splitWidth - 0.25) > 0.0001) return win.fail("the share is at least 0.25: " + App.splitWidth);
            if (Math.abs(left.width - Math.max(win.splitChatMin, win.chatRoom() * 0.25)) > 3) return win.later("0.25 of the room, a whole column at least: " + left.width);
            console.warn("PASS");
            Qt.exit(0);
            return;

        // ---- S4: the restore ----
        case "rs-wait": {
            // Nothing opened by the script: the window restores the stored chats after the first frame.
            const gone = win.kase === "restoregone";
            if (gone) {
                if (App.split) return win.fail("pane 0's chat is gone: no split");
                if (App.timeline.conversationId !== win.second || win.chatPage === null || App.timeline.count === 0 || App.timeline.loading) return win.later("pane 1's chat opens as the only chat");
                if (win.sideChatPage !== null) return win.fail("no second page");
                win.next("rs-stored");
                return;
            }
            if (!win.bothReady()) return win.later("both restored chats");
            if (App.activePane !== 0 || !left.active || right.active) return win.later("pane 0 ends active");
            if (win.stackProblem("full") !== "") return win.later(win.stackProblem("full"));
            if (Math.abs(left.width - win.chatRoom() * 0.375) > 3 || Math.abs(App.splitWidth - 0.375) > 0.0001) return win.later("the stored share is applied: " + left.width + " of " + win.chatRoom());
            win.quiet = 0;
            win.next("rs-quiet");
            return;
        }
        case "rs-quiet":
            // Nothing moves the active pane afterwards (a focus that lands in the second chat would).
            if (App.activePane !== 0) return win.fail("pane 0 stays active");
            if (++win.quiet < 25) return;
            win.next("rs-stored");
            return;
        case "rs-stored": {
            const gone = win.kase === "restoregone";
            const want = gone ? win.second + ";0;0.5000" : win.first + ";" + win.second + ";0.3750";
            // (The test's seed wrote ids 1 and 2 of the rows; what the app keeps now is the open state.)
            if (!gone && App.storedChats() !== want) return win.later("the preferences hold " + App.storedChats() + ", want " + want);
            if (gone && App.storedChats() !== want) return win.later("the preferences were corrected to " + want + ", now " + App.storedChats());
            if (gone) {
                console.warn("PASS");
                Qt.exit(0);
                return;
            }
            // A swap is written back: the stored chats name pane 0's and pane 1's chats again.
            App.swapPanes();
            win.next("rs-swapped");
            return;
        }
        case "rs-swapped":
            if (App.storedChats() !== win.second + ";" + win.first + ";0.3750") return win.later("the swap is written: " + App.storedChats());
            if (App.activePane !== 1 || App.paneOf(win.first) !== 1) return win.fail("the active chat (first) follows to pane 1");
            // A change of a pane is written back: the split ends, only the first chat is kept.
            App.closePane(1);
            win.next("rs-closed");
            return;
        case "rs-closed":
            if (App.split) return win.later("the split ended");
            if (App.storedChats() !== win.second + ";0;0.3750") return win.later("the closed split is written: " + App.storedChats());
            console.warn("PASS");
            Qt.exit(0);
            return;

        // ---- S5: messages dragged to the other chat stage a forward there ----
        case "dg-open":
            if (App.timeline.conversationId !== win.first || win.chatPage === null) return win.later("the first chat");
            if (App.timeline.count === 0 || App.timeline.loading) return win.later("its rows");
            App.openInSplit(win.second);
            win.next("dg-split");
            return;
        case "dg-split": {
            if (!win.bothReady()) return win.later("both panes");
            if (win.messageDragItem.dragging) return win.fail("no drag at the start");
            const from = win.dgPoint(left);
            if (from === null) return win.later("a message of the first chat that can be dragged");
            win.dgFrom = from;
            win.dgTaken = win.messageDragItem.taken;
            win.dgMissed = win.messageDragItem.missed;
            win.dgNewest = win.newestMessage(right);
            if (App.sideComposer.forwardOf(win.second).count !== undefined) return win.fail("nothing is staged into the second chat yet");
            // The pointer rests on the bubble, no button held: the timeline decides there that a drag may start.
            win.dgMove(from.at, Qt.NoButton);
            win.next("dg-hovered");
            return;
        }
        case "dg-hovered":
            if (left.timeline.dragFrom !== win.dgFrom.id) return win.later("the hover names the message under the pointer: " + left.timeline.dragFrom);
            win.dgPress(win.dgFrom.at);
            win.next("dg-pressed");
            return;
        case "dg-pressed":
            // A press on the other pane's timeline activates it.
            if (App.activePane !== 0) return win.later("the press activates the first pane");
            win.dgMove(win.dgShift(win.dgFrom.at, 2, 0), Qt.LeftButton);
            if (win.messageDragItem.dragging) return win.fail("a move under the drag distance starts no drag");
            win.dgMove(win.dgShift(win.dgFrom.at, 30, 0), Qt.LeftButton);
            win.next("dg-dragging");
            return;
        case "dg-dragging": {
            const d = win.messageDragItem;
            if (!d.dragging) return win.fail("a move past the drag distance starts the drag");
            if (d.fromPane !== 0 || d.fromConversation !== win.first || d.messages.length !== 1 || d.messages[0] !== String(win.dgFrom.id)) return win.fail("the drag carries the pressed message of the first chat");
            const card = win.find(d, i => i.objectName === "messageDragCard");
            if (card === null || !card.visible) return win.fail("the card beside the pointer shows");
            const target = win.dgCentre(right);
            win.dgMove(Qt.point((win.dgFrom.at.x + target.x) / 2, (win.dgFrom.at.y + target.y) / 2), Qt.LeftButton);
            win.dgMove(target, Qt.LeftButton);
            win.next("dg-over");
            return;
        }
        case "dg-over": {
            const drop = win.find(right, i => i.objectName === "forwardDrop");
            if (drop === null || !drop.containsDrag) return win.later("the second chat's drop area holds the drag");
            if (win.find(left, i => i.objectName === "forwardDrop").containsDrag) return win.fail("the first chat's drop area does not");
            win.snap("drag-over");
            win.dgRelease(win.dgCentre(right));
            win.next("dg-dropped");
            return;
        }
        case "dg-dropped": {
            const d = win.messageDragItem;
            if (d.dragging || d.taken !== win.dgTaken + 1) return win.later("the second chat takes the drop");
            if (d.missed !== win.dgMissed) return win.fail("a drop that was taken is not missed");
            if (App.sideComposer.forwardOf(win.second).count !== 1) return win.fail("one message is staged into the second chat");
            if (App.composer.forwardOf(win.first).count !== undefined) return win.fail("nothing is staged into the first chat");
            if (!right.composer.forwarding || !right.composer.forwardCard.visible) return win.later("the second chat's composer shows the card");
            if (App.activePane !== 1) return win.later("the drop activates the second pane");
            if (!win.area(right).activeFocus) return win.later("the second chat's text area has the keyboard");
            if (win.newestMessage(right) !== win.dgNewest) return win.fail("the drop sends nothing");
            win.snap("drag-dropped");
            win.next("dg-text");
            return;
        }
        case "dg-text": {
            // A press on a message's text selects text, as before the split: no drag.
            const t = win.dgTextItem(left);
            if (t === null) return win.later("a text in the first chat");
            win.dgText = t;
            win.dgTaken = win.messageDragItem.taken;
            win.dgMissed = win.messageDragItem.missed;
            const r = t.positionToRectangle(0);
            const at = t.mapToItem(win.contentItem, r.x + 3, r.y + r.height / 2);
            win.dgFrom = { id: 0, at: at };
            win.dgMove(at, Qt.NoButton);
            win.next("dg-text-hovered");
            return;
        }
        case "dg-text-hovered":
            if (left.timeline.dragFrom !== 0) return win.later("the hover on text offers no drag: " + left.timeline.dragFrom);
            win.dgPress(win.dgFrom.at);
            win.dgMove(win.dgShift(win.dgFrom.at, 2, 0), Qt.LeftButton);
            win.dgMove(win.dgShift(win.dgFrom.at, 30, 0), Qt.LeftButton);
            win.dgMove(win.dgShift(win.dgFrom.at, 60, 0), Qt.LeftButton);
            if (win.messageDragItem.dragging) return win.fail("a drag on text starts no message drag");
            win.dgRelease(win.dgShift(win.dgFrom.at, 60, 0));
            win.next("dg-text-done");
            return;
        case "dg-text-done": {
            const d = win.messageDragItem;
            if (d.dragging || d.taken !== win.dgTaken || d.missed !== win.dgMissed) return win.fail("a drag on text carries nothing");
            if (win.dgText.selectedText.length === 0) return win.fail("a drag on text selects it");
            if (App.sideComposer.forwardOf(win.second).count !== 1) return win.fail("the staged forward stays as it was");
            // A drop on the chat the drag came from is no drop: nothing is staged, the drag is missed.
            const from = win.dgPoint(left);
            if (from === null) return win.later("a message of the first chat that can be dragged");
            win.dgFrom = from;
            win.dgMove(from.at, Qt.NoButton);
            win.next("dg-same-hovered");
            return;
        }
        case "dg-same-hovered":
            if (left.timeline.dragFrom !== win.dgFrom.id) return win.later("the hover names the message under the pointer");
            win.dgPress(win.dgFrom.at);
            win.dgMove(win.dgShift(win.dgFrom.at, 2, 0), Qt.LeftButton);
            win.dgMove(win.dgShift(win.dgFrom.at, 30, 0), Qt.LeftButton);
            win.dgMove(win.dgShift(win.dgFrom.at, 50, 0), Qt.LeftButton);
            if (!win.messageDragItem.dragging) return win.fail("a move past the drag distance starts the drag");
            if (win.find(left, i => i.objectName === "forwardDrop").containsDrag) return win.fail("the chat a drag comes from does not take it");
            win.dgRelease(win.dgShift(win.dgFrom.at, 50, 0));
            win.next("dg-same-done");
            return;
        case "dg-same-done": {
            const d = win.messageDragItem;
            if (d.dragging || d.missed !== win.dgMissed + 1) return win.later("the drag ends missed");
            if (d.taken !== win.dgTaken) return win.fail("a drop on its own chat is not taken");
            if (App.composer.forwardOf(win.first).count !== undefined) return win.fail("nothing is staged into the first chat");
            // A selection: a drag of a selected message carries every selected message.
            App.sideComposer.dropForward(win.second);
            const ids = win.dgTwo(left);
            if (ids.length !== 2) return win.later("two messages of the first chat that can be forwarded");
            win.dgIds = ids;
            App.timeline.selectStart(ids[0]);
            App.timeline.selectToggle(ids[1]);
            win.next("dg-selected");
            return;
        }
        case "dg-selected": {
            if (!App.timeline.selecting || App.timeline.selectedCount !== 2) return win.later("two messages selected");
            if (App.sideComposer.forwardOf(win.second).count !== undefined) return win.later("the staged forward dropped");
            const d = left.timeline.delegateFor(win.dgIds[0]);
            if (d === null) return win.later("the selected row");
            const view = left.timeline;
            const v = d.mapToItem(view, d.width / 2, d.height / 2);
            if (v.y < 10 || v.y > view.height - 10) return win.fail("the selected row stays in view");
            win.dgTaken = win.messageDragItem.taken;
            win.dgMissed = win.messageDragItem.missed;
            win.dgFrom = { id: win.dgIds[0], at: d.mapToItem(win.contentItem, d.width / 2, d.height / 2) };
            win.dgPress(win.dgFrom.at);
            win.dgMove(win.dgShift(win.dgFrom.at, 2, 0), Qt.LeftButton);
            if (win.messageDragItem.dragging) return win.fail("a move under the drag distance starts no drag");
            win.dgMove(win.dgShift(win.dgFrom.at, 30, 0), Qt.LeftButton);
            win.next("dg-sel-dragging");
            return;
        }
        case "dg-sel-dragging": {
            const d = win.messageDragItem;
            if (!d.dragging) return win.fail("a drag of a selected message starts");
            if (d.messages.length !== 2 || d.messages.indexOf(String(win.dgIds[0])) < 0 || d.messages.indexOf(String(win.dgIds[1])) < 0) return win.fail("the drag carries both selected messages");
            const target = win.dgCentre(right);
            win.dgMove(Qt.point((win.dgFrom.at.x + target.x) / 2, (win.dgFrom.at.y + target.y) / 2), Qt.LeftButton);
            win.dgMove(target, Qt.LeftButton);
            win.next("dg-sel-over");
            return;
        }
        case "dg-sel-over":
            if (!win.find(right, i => i.objectName === "forwardDrop").containsDrag) return win.later("the second chat's drop area holds the drag");
            win.snap("drag-selection-over");
            win.dgRelease(win.dgCentre(right));
            win.next("dg-sel-dropped");
            return;
        case "dg-sel-dropped":
            if (win.messageDragItem.dragging || win.messageDragItem.taken !== win.dgTaken + 1) return win.later("the second chat takes the drop");
            if (App.sideComposer.forwardOf(win.second).count !== 2) return win.fail("both messages are staged into the second chat");
            if (App.timeline.selecting) return win.fail("the drop ends the selection it carried");
            if (App.activePane !== 1) return win.later("the drop activates the second pane");
            win.snap("drag-selection-dropped");
            // Without a split no drag starts.
            App.closePane(1);
            win.next("dg-closed");
            return;
        case "dg-closed": {
            if (App.split || win.sideChatPage !== null && win.inStack(win.sideChatPage)) return win.later("the split ended");
            const page = win.chatPage;
            if (page === null || page.timeline.rowList.count === 0) return win.later("the first chat");
            const from = win.dgPoint(page);
            if (from === null) return win.later("a message of the first chat");
            win.dgFrom = from;
            win.dgTaken = win.messageDragItem.taken;
            win.dgMissed = win.messageDragItem.missed;
            win.dgMove(from.at, Qt.NoButton);
            win.next("dg-off");
            return;
        }
        case "dg-off":
            win.dgPress(win.dgFrom.at);
            win.dgMove(win.dgShift(win.dgFrom.at, 2, 0), Qt.LeftButton);
            win.dgMove(win.dgShift(win.dgFrom.at, 30, 0), Qt.LeftButton);
            win.dgMove(win.dgShift(win.dgFrom.at, 50, 0), Qt.LeftButton);
            if (win.messageDragItem.dragging) return win.fail("no drag starts without a split");
            win.dgRelease(win.dgShift(win.dgFrom.at, 50, 0));
            win.next("dg-off-done");
            return;
        case "dg-off-done":
            if (win.messageDragItem.dragging || win.messageDragItem.taken !== win.dgTaken || win.messageDragItem.missed !== win.dgMissed) return win.fail("nothing is carried without a split");
            console.warn("PASS");
            Qt.exit(0);
            return;

        // ---- a narrow window ----
        case "n-open":
            if (win.wideScreen) return win.fail("the window is narrow");
            if (App.timeline.conversationId !== win.first || win.chatPage === null || win.pageStack.lastItem !== win.chatPage) return win.later("the chat alone");
            if (App.timeline.count === 0 || App.timeline.loading) return win.later("its rows");
            win.next("n-click");
            return;
        case "n-click": {
            const b = win.besideButton(win.chatPage);
            if (b === null) return win.later("the header's button");
            win.click(b, b.width / 2, b.height / 2);
            win.next("n-banner");
            return;
        }
        case "n-banner":
            if (!win.choosingBeside) return win.later("the chooser");
            if (win.pageStack.lastItem !== win.lp() || !win.bannerShown()) return win.later("the list with the banner");
            win.next("n-pick");
            return;
        case "n-pick": {
            const row = win.rowOfConversation(win.second);
            if (row === null) return win.later("a row");
            win.click(row, row.width / 2, row.height / 2);
            win.next("n-picked");
            return;
        }
        case "n-picked":
            if (!App.split) return win.later("the pick opens beside");
            if (win.choosingBeside) return win.fail("the chooser ended");
            if (App.activePane !== 1 || App.paneOf(win.second) !== 1) return win.fail("the new chat is the active pane");
            if (win.sideChatPage === null || win.pageStack.lastItem !== win.sideChatPage) return win.later("the new chat shows");
            if (win.inStack(win.chatPage) || win.pageStack.depth !== 2) return win.fail("only the new chat is shown");
            win.next("n-close");
            return;
        case "n-close": {
            // Close Chat on the active chat of the split: the list shows, with the keyboard; the chat that stays
            // is not pushed over it.
            const a = win.act(win.sideChatPage, "closeAction");
            if (a === null || !a.visible) return win.later("Close Chat is offered");
            const b = win.actionButton(win.sideChatPage, "closeAction");
            if (b === null && win.waits < 40) return win.later("the header's Close Chat");
            if (b !== null) win.click(b, b.width / 2, b.height / 2); else a.trigger();
            win.next("n-closed");
            return;
        }
        case "n-closed":
            if (App.split) return win.later("the split ended");
            if (App.timeline.conversationId !== win.first) return win.fail("the chat that stays is the first");
            if (win.pageStack.lastItem !== win.lp() || win.pageStack.depth !== 1) return win.later("the list shows alone: depth " + win.pageStack.depth);
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard");
            win.openConversation(win.second);
            win.next("n-one");
            return;
        case "n-one":
            if (App.timeline.conversationId !== win.second || win.chatPage === null || win.pageStack.lastItem !== win.chatPage) return win.later("one chat alone");
            if (App.timeline.count === 0 || App.timeline.loading) return win.later("its rows");
            {
                const a = win.act(win.chatPage, "closeAction");
                const b = win.actionButton(win.chatPage, "closeAction");
                if (b === null && win.waits < 40) return win.later("the header's Close Chat");
                if (b !== null) win.click(b, b.width / 2, b.height / 2); else a.trigger();
            }
            win.next("n-one-closed");
            return;
        case "n-one-closed":
            if (App.timeline.conversationId !== 0) return win.later("the chat closed");
            if (win.pageStack.lastItem !== win.lp()) return win.later("the list shows");
            if (!win.lp().conversations.activeFocus) return win.later("the list has the keyboard");
            console.warn("PASS");
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 120000
        running: true
        onTriggered: win.fail("timeout")
    }
}
"##;

/// Runs one case in this process: its window, its script; returns the exit code.
fn run(case: &str) -> i32 {
    let width = if case == "narrow" { "600" } else { "1500" };
    let script = SCRIPT
        .replace("SPLIT_CASE", case)
        .replace("SPLIT_WIDTH", width)
        .replace(
            "SPLIT_SHOTS",
            &std::env::var("KOURIER_SPLIT_WIN_SHOTS").unwrap_or_default(),
        );
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
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
    // The restore cases start as a normal run would, with these chats stored (rows 1 and 2 of the list; pane
    // 0's chat does not exist in `restoregone`) and the divider at 0.375 (`restoregone`: the default).
    let restore_seed = match case {
        "restore" => Some(RestoreSeed {
            open: SeedChat::Row(1),
            split: SeedChat::Row(2),
            width_permille: 375,
        }),
        "restoregone" => Some(RestoreSeed {
            open: SeedChat::Id(999_999),
            split: SeedChat::Row(2),
            width_permille: 500,
        }),
        _ => None,
    };
    kourier_qt::set_options(UiOptions {
        restore_seed,
        ..UiOptions::default()
    });
    let mut qt = QGuiApplication::new();

    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/split_win_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code == 0 {
        app.shutdown();
    }
    code
}

fn main() {
    support::answer_list("split_win_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_SPLIT_WIN_CASE") {
        std::process::exit(run(&case));
    }
    // The parent: each case in a child of its own, whose stderr is read for script errors.
    let exe = std::env::current_exe().expect("the test binary");
    let mut failed = 0;
    for case in [
        "wide",
        "narrow",
        "layouts",
        "restore",
        "restoregone",
        "drag",
    ] {
        let out = Command::new(&exe)
            .env("KOURIER_SPLIT_WIN_CASE", case)
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
            println!("split view in the main window: {case} ... ok");
        } else {
            println!(
                "split view in the main window: {case} ... FAILED ({})",
                out.status
            );
            failed += 1;
        }
    }
    if failed == 0 {
        println!("\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!("\ntest result: FAILED. 0 passed; 1 failed");
        std::process::exit(1);
    }
}
