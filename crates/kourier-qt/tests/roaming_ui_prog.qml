import QtQuick
import QtQml
import org.kde.kirigami as Kirigami
import app.kourier

// The cases of D6 and D7 (A10d) of roaming_ui.rs, written as PROGRAMS: a list of named steps, each
// a function that returns true when it is done and false while it waits (`hold`). The tokens CASE,
// WIDTH, PORT, FIXTURE_NOW and SHOTS_DIR are replaced by the Rust side. The listener at PORT tells
// what the fake heard (`/heard?name=ChangeConversation`) and the core's clock (`/now`).
// A failure is a sentence that names the step: never the text it compared (contact names).
MainWindow {
    id: win

    width: WIDTH
    height: 760
    visible: true

    readonly property string mode: "CASE"
    readonly property bool narrow: WIDTH < 700
    property string stepName: "start"
    property var prog: []
    property int pc: 0
    property int waits: 0
    property int quiet: 0
    property int limit: 1500
    property bool busy: false
    property var ev: null
    // What a case remembers between its steps.
    property var m: ({})

    function fail(msg: string): bool {
        console.error("FAIL [" + mode + "/" + stepName + "]: " + msg);
        Qt.exit(1);
        return false;
    }

    function hold(what: string): bool {
        win.waits += 1;
        if (win.waits > win.limit) {
            return win.fail("never happened: " + what);
        }
        return false;
    }

    // True once the state has held for `n` ticks (a "nothing happens" step).
    function stays(n: int): bool {
        win.quiet += 1;
        return win.quiet >= n;
    }

    function add(name: string, fn: var): void {
        win.prog.push({ name: name, fn: fn });
    }

    function shot(name: string): void {
        if ("SHOTS_DIR" !== "") {
            win.contentItem.grabToImage(r => r.saveToFile("SHOTS_DIR/" + name + ".png"));
        }
    }

    function ask(path: string): string {
        const r = new XMLHttpRequest();
        r.open("GET", "http://127.0.0.1:PORT" + path, false);
        r.send();
        return r.responseText;
    }

    // How many commands of this name the fake network has heard.
    function heard(name: string): int {
        return parseInt(win.ask("/heard?name=" + name));
    }

    function coreNow(): real {
        return parseFloat(win.ask("/now"));
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

    function findAll(root: Item, pred: var): var {
        const hits = [];
        const walk = item => {
            if (pred(item)) {
                hits.push(item);
            }
            for (const child of item.children) {
                walk(child);
            }
        };
        walk(root);
        return hits;
    }

    // Items with exactly this text that are on screen: toasts and their buttons.
    function shownText(text: string): var {
        const same = c => c.text === text && c.visible;
        return win.findAll(win.contentItem, same).concat(win.findAll(win.overlay, same));
    }

    function toasted(text: string): bool {
        return win.shownText(text).length > 0;
    }

    function pg(): Item {
        return win.pageStack.get(0);
    }

    function rowsNow(): var {
        const l = win.pg().conversations;
        const out = [];
        for (let i = 0; i < l.count; ++i) {
            const r = l.itemAtIndex(i);
            if (r) {
                out.push(r);
            }
        }
        return out;
    }

    function rowOfId(id: real): Item {
        const hit = win.rowsNow().find(r => r.conversationId === id);
        return hit ? hit : null;
    }

    // Whether the chat is a row of the list at all (the view is moved through its rows).
    function listed(id: real): bool {
        const l = win.pg().conversations;
        for (let i = 0; i < l.count; ++i) {
            l.positionViewAtIndex(i, ListView.Beginning);
            const r = l.itemAtIndex(i);
            if (r && r.conversationId === id) {
                l.positionViewAtBeginning();
                return true;
            }
        }
        l.positionViewAtBeginning();
        return false;
    }

    function offer(id: real): var {
        return App.chatOffer(id);
    }

    function wholeOnScreen(r: Item): bool {
        const l = win.pg().conversations;
        const y = r.mapToItem(l, 0, 0).y;
        return y >= 0 && y + r.height <= l.height;
    }

    // Ordinary chats on screen: nothing unread, not pinned, not muted, not Note to Self.
    function plainIds(): var {
        return win.rowsNow().filter(r => !r.isSelf && r.unread === 0 && !r.markedUnread && !r.pinned && !r.muted && win.wholeOnScreen(r)).map(r => r.conversationId);
    }

    // Every row of the list as a snapshot (the view is moved through them and put back).
    function scanRows(): var {
        const l = win.pg().conversations;
        const out = [];
        for (let i = 0; i < l.count; ++i) {
            l.positionViewAtIndex(i, ListView.Beginning);
            const r = l.itemAtIndex(i);
            if (r) {
                out.push({ id: r.conversationId, unread: r.unread, markedUnread: r.markedUnread, pinned: r.pinned, muted: r.muted, isSelf: r.isSelf, badge: r.accountBadge });
            }
        }
        l.positionViewAtBeginning();
        return out;
    }

    // Brings the row of `id` to the top of the view (false: it is not a row).
    function scrollTo(id: real): bool {
        const l = win.pg().conversations;
        for (let i = 0; i < l.count; ++i) {
            l.positionViewAtIndex(i, ListView.Beginning);
            const r = l.itemAtIndex(i);
            if (r && r.conversationId === id) {
                return true;
            }
        }
        return false;
    }

    function rightClick(id: real): bool {
        let r = win.rowOfId(id);
        if (!r || !win.wholeOnScreen(r)) {
            if (!win.scrollTo(id)) {
                return false;
            }
            r = win.rowOfId(id);
            if (!r || !win.wholeOnScreen(r)) {
                return false;
            }
        }
        return win.ev.mouseClick(r, 20, 10, Qt.RightButton, Qt.NoModifier, -1);
    }

    function click(item: Item): bool {
        return win.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
    }

    // The entries the row menu shows now: visible, with room, with a text.
    function entries(): var {
        const m = win.pg().rowMenu;
        const out = [];
        for (let i = 0; i < m.count; ++i) {
            const it = m.itemAt(i);
            if (it && it.visible && it.height > 0 && it.text !== undefined && it.text !== "" && !String(it.objectName).startsWith("rowCopy")) {
                out.push(it);
            }
        }
        return out;
    }

    function texts(): string {
        return win.entries().map(e => e.text).join("|");
    }

    // The menu's copy entries (main's copy-and-links), which every chat's menu has: `entries()`
    // leaves them out, so the cases here read the roaming part alone.
    function copyTexts(): string {
        const m = win.pg().rowMenu;
        const out = [];
        for (let i = 0; i < m.count; ++i) {
            const it = m.itemAt(i);
            if (it && it.visible && it.height > 0 && String(it.objectName).startsWith("rowCopy")) out.push(it.text);
        }
        return out.join("|");
    }

    function whyLabel(): Item {
        return win.pg().whyLine.children[0];
    }

    function acct(): real {
        return App.accountActions[0].account;
    }

    // The window's account `n`th in the settings page's list, by what it is called.
    function actionOf(account: real): var {
        return App.accountActions.find(a => a.account === account);
    }

    // The steps that open the row menu of the chat `idf()` by a real right click.
    function openFor(idf: var): void {
        win.add("open the menu", () => {
            const id = idf();
            if (!id) return win.hold("a chat to open the menu of");
            win.m.cur = id;
            if (!win.rightClick(id)) return win.hold("the row on screen");
            return true;
        });
        win.add("the menu is up", () => {
            const p = win.pg();
            if (!p.rowMenu.opened) return win.hold("the menu opens");
            if (p.rowMenu.conversationId !== win.m.cur) return win.fail("the menu is for another chat");
            return true;
        });
    }

    function shutMenu(): void {
        win.add("the menus close", () => {
            const p = win.pg();
            if (p.rowMenu.opened || p.muteSub.opened) {
                p.muteSub.close();
                p.rowMenu.close();
                return win.hold("the menus close");
            }
            return (!p.rowMenu.visible && !p.muteSub.visible) ? true : win.hold("the menus are gone");
        });
    }

    // Counts of what the fake heard, before a click (`win.m.base`).
    function mark(): void {
        win.m.base = win.heard("ChangeConversation");
    }

    function sameKey(key: int, mods: int): bool {
        return win.ev.keyClick(key, mods, -1);
    }

    function ctrlShift(key: int): bool {
        return win.ev.keyClick(key, Qt.ControlModifier | Qt.ShiftModifier, -1);
    }

    function offerNow(): var {
        return App.timeline.chatOffer;
    }

    // Chat page action by text; the nth with this text.
    function actionsNamed(text: string): var {
        const out = [];
        const a = win.chatPage.actions;
        for (let i = 0; i < a.length; ++i) {
            if (a[i].text === text) {
                out.push(a[i]);
            }
        }
        return out;
    }

    function shown(list: var): var {
        return list.filter(a => a.visible);
    }

    // The two steps both cases of settings-* need: the account's switch row by a real click.
    function settingsRow(account: real): Item {
        return win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "roaming-" + account) : null;
    }

    function reveal(item: Item): void {
        const flick = win.settingsPage.flickable;
        const y = item.mapToItem(flick.contentItem, 0, 0).y;
        flick.contentY = Math.max(0, Math.min(y - (flick.height - item.height) / 2, flick.contentHeight - flick.height));
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
        if (win.pc >= win.prog.length) {
            Qt.exit(0);
            return;
        }
        const s = win.prog[win.pc];
        win.stepName = s.name;
        if (s.fn() === true) {
            win.pc += 1;
            win.waits = 0;
            win.quiet = 0;
        }
    }

    // ---------------------------------------------------------------------------------------
    // The cases.

    function build(): void {
        win.add("the window is up", () => {
            if (!win.started || !App.hasAccounts || win.pageStack.depth < 1 || !win.pg().conversations) return win.hold("the start page");
            if (win.pg().conversations.count < 4) return win.hold("the chats");
            if (win.ev === null) {
                win.ev = Qt.createQmlObject("import QtQuick\nimport QtTest\nTestEvent {}", win.contentItem, "roaming_ui_prog_events.qml");
                win.requestActivate();
            }
            if (App.accountActions.length < 1 || App.accountActions[0].roaming === undefined) return win.hold("the accounts");
            return true;
        });
        const cases = {
            "menu-on": win.menuOn, "menu-off": win.menuOff, "menu-off-narrow": win.menuOff, "menu-logged-out": win.menuLoggedOut,
            "menu-refused": win.menuRefused, "menu-refused-pin": win.menuRefusedPin, "undo-two": win.undoTwo, "undo-late": win.undoLate, "until-keys": win.untilKeys, "menu-hidden": win.menuHidden, "menu-read": win.menuRead,
            "note-to-self": win.noteToSelf, "chat-menu": win.chatMenu, "chat-archived": win.chatArchived,
            "keys": win.keys, "keys-narrow": win.keys, "keys-off": win.keysOff, "undo": win.undo, "notices": win.notices,
            "undo-pinned": win.undoPinned, "mute": win.mute, "unmute": win.unmute, "until": win.until, "until-past": win.untilPast,
            "pin-limit": win.pinLimit, "blended": win.blended, "settings-switch": win.settingsSwitch, "settings-refused": win.settingsRefused
        };
        cases[win.mode]();
    }
    Component.onCompleted: win.build()

    // Turns the first account's switch off through the real command and waits for the echo.
    function switchOff(): void {
        win.add("the switch goes off", () => {
            App.setRoamingWrites(win.acct(), false);
            return true;
        });
        win.add("the echo: Off", () => {
            if (App.accountActions[0].roaming !== "off") return win.hold("the account states the switch off");
            return true;
        });
    }

    // ---- D6 ----

    function greyedFour(why: string): bool {
        if (win.texts() !== "Mark as Unread|Pin|Mute|Archive") return win.fail("the greyed menu's entries: " + win.entries().length);
        for (const e of win.entries()) {
            if (e.enabled) return win.fail("an entry is not greyed (" + e.text + ")");
            if (e.Accessible.description !== why) return win.fail("an entry's accessible description is not the reason (" + e.text + ")");
        }
        return true;
    }

    function menuOn(): void {
        win.add("pick a chat", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("entries in order, all enabled", () => {
            const p = win.pg();
            if (win.texts() !== "Mark as Unread|Pin|Mute|Archive") return win.fail("the entries or their order: " + win.entries().length);
            for (const e of win.entries()) {
                if (!e.enabled) return win.fail("an entry is greyed with the sync on (" + e.text + ")");
            }
            if (p.rowMenu.offer.on !== true) return win.fail("the menu's offer is not On");
            if (p.whyLine.visible) return win.fail("a reason shows with nothing wrong");
            win.shot("menu-on");
            win.mark();
            win.click(p.pinEntry);
            return true;
        });
        win.add("Pin reaches the fake and the row follows", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.c).pinned !== true) return win.hold("the row says pinned");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.c);
        win.add("Unpin now, then Mark as Unread", () => {
            const p = win.pg();
            if (p.pinEntry.text !== "Unpin") return win.fail("a pinned chat's entry says Pin");
            win.mark();
            win.click(p.markUnreadEntry);
            return true;
        });
        win.add("Mark as Unread reaches the fake and the row follows", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.c).markedUnread !== true) return win.hold("the row says marked unread");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.c);
        win.add("now Mark as Read replaces it; Archive", () => {
            const p = win.pg();
            if (!p.markReadEntry.visible || p.markUnreadEntry.visible) return win.fail("Mark as Read does not replace Mark as Unread");
            win.mark();
            win.click(p.archiveMenuEntry);
            return true;
        });
        win.add("Archive reaches the fake and the row follows", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.c).archived !== true) return win.hold("the row says archived");
            if (win.rowOfId(win.m.c) !== null) return win.hold("the row leaves the list");
            return true;
        });
    }

    function menuOff(): void {
        win.add("pick a chat", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            return true;
        });
        win.switchOff();
        win.add("the chat's offer is Off", () => win.offer(win.m.c).on === false && win.offer(win.m.c).why !== "" ? true : win.hold("the offer says Off with its reason"));
        win.openFor(() => win.m.c);
        win.add("four greyed entries, the reason as description, wrapped", () => {
            const p = win.pg();
            const why = win.offer(win.m.c).why;
            if (!win.greyedFour(why)) return false;
            if (!p.whyLine.visible) return win.fail("no reason line under the greyed entries");
            const label = win.whyLabel();
            if (label.text !== why) return win.fail("the line is not the reason");
            if (label.lineCount < 2) return win.fail("the reason does not wrap (" + label.lineCount + " line)");
            if (p.rowMenu.width >= Kirigami.Units.gridUnit * 20) return win.fail("the menu is as wide as 20 grid units or more");
            if (p.rowMenu.width > win.width) return win.fail("the menu is wider than the window");
            if (p.menuAnnounced !== why || p.menuAnnouncedPoliteness !== Accessible.Polite) return win.fail("the reason was not told politely when nothing is enabled");
            win.shot(win.mode);
            win.mark();
            for (const e of win.entries()) {
                win.click(e);
            }
            return true;
        });
        win.add("a click on a greyed entry sends nothing", () => {
            if (!win.stays(15)) return false;
            if (win.heard("ChangeConversation") !== win.m.base) return win.fail("a greyed entry sent a change");
            const o = win.offer(win.m.c);
            if (o.archived || o.pinned || o.muted || o.markedUnread) return win.fail("a greyed entry changed the chat");
            return true;
        });
    }

    function menuLoggedOut(): void {
        win.add("pick a chat", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("greyed, with the account's reason", () => {
            const p = win.pg();
            const why = "Available while this account is linked.";
            if (win.offer(win.m.c).why !== why) return win.fail("the offer's reason is not the linked sentence");
            if (!win.greyedFour(why)) return false;
            if (!p.whyLine.visible || win.whyLabel().text !== why) return win.fail("the line does not say the account is not linked");
            win.mark();
            for (const e of win.entries()) {
                win.click(e);
            }
            return true;
        });
        win.add("nothing was sent", () => {
            if (!win.stays(15)) return false;
            if (win.heard("ChangeConversation") !== win.m.base) return win.fail("a greyed entry sent a change");
            return true;
        });
    }

    function menuRefused(): void {
        win.add("pick a chat", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("click Archive", () => {
            win.mark();
            win.click(win.pg().archiveMenuEntry);
            return true;
        });
        win.add("the refusal is told and no Archived notice", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears the change");
            // A toast (never the composer, which may be off screen): H1.
            if (!win.toasted("The change could not be made.")) return win.hold("the refusal's toast");
            if (App.composer.notice !== "") return win.fail("the refusal reached the composer");
            if (win.chatToldAnnounced !== "The change could not be made." || win.chatToldPoliteness !== Accessible.Polite) return win.fail("the refusal was not announced politely");
            if (win.toasted("Chat archived") || win.undoStanding !== null) return win.fail("a refused archive told 'Chat archived'");
            return true;
        });
        win.add("and none later", () => {
            if (!win.stays(25)) return false;
            if (win.toasted("Chat archived") || win.undoStanding !== null || win.chatToldAnnounced === "Chat archived") return win.fail("a refused archive told 'Chat archived' later");
            if (win.offer(win.m.c).archived) return win.fail("a refused archive changed the row");
            return true;
        });
    }

    // H1: a refused Pin from the row menu with NO chat open is told by a toast.
    function menuRefusedPin(): void {
        win.add("pick a chat, none open", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            if (App.timeline.conversationId !== 0) return win.fail("a chat is open");
            win.m.c = ids[0];
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("click Pin", () => {
            win.mark();
            win.click(win.pg().pinEntry);
            return true;
        });
        win.add("the refusal is a toast, not a composer notice", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears the change");
            if (!win.toasted("The change could not be made.")) return win.hold("the toast");
            if (App.composer.notice !== "" || App.timeline.conversationId !== 0) return win.fail("the notice went to a composer");
            return true;
        });
    }

    function menuHidden(): void {
        win.add("the account's chats offer nothing", () => {
            const rows = win.rowsNow().filter(r => !r.isSelf);
            if (rows.length < 4) return win.hold("rows");
            for (const r of rows) {
                if (win.offer(r.conversationId).shown !== false) return win.fail("a chat of the WhatsApp account offers the entries");
            }
            const plain = win.plainIds();
            if (plain.length < 1) return win.hold("an ordinary chat");
            win.m.c = plain[0];
            const un = rows.find(r => r.unread > 0 && !r.markedUnread);
            if (!un) return win.fail("the fixture has no chat with unread messages on screen");
            win.m.u = un.conversationId;
            return true;
        });
        // Every chat's menu has the copy entries, so a chat with nothing else still gets a menu:
        // the copy entries alone, no separator, none of the roaming entries.
        const copyOnly = (how) => {
            win.add("the copy entries alone by " + how, () => {
                const p = win.pg();
                if (!p.rowMenu.opened) return win.hold("the menu opens by " + how);
                if (p.rowMenu.conversationId !== win.m.c) return win.fail("the menu is for another chat");
                if (win.texts() !== "") return win.fail("a chat of a network with no entries offers: " + win.texts());
                if (win.copyTexts() !== "Copy Name|Copy Number or ID|Copy Link") return win.fail("the copy entries are not all there: " + win.copyTexts());
                for (let i = 0; i < p.rowMenu.count; ++i) {
                    const it = p.rowMenu.itemAt(i);
                    if (it && it.visible && it.height > 0 && it.text === undefined) return win.fail("a separator shows in a menu of copy entries alone");
                }
                return true;
            });
            win.shutMenu();
        };
        win.add("a right click", () => {
            if (!win.rightClick(win.m.c)) return win.hold("the row on screen");
            return true;
        });
        copyOnly("a right click");
        win.add("the Menu key", () => {
            const p = win.pg();
            const l = p.conversations;
            for (let i = 0; i < l.count; ++i) {
                const r = l.itemAtIndex(i);
                if (r && r.conversationId === win.m.c) {
                    l.currentIndex = i;
                }
            }
            l.forceActiveFocus();
            win.ev.keyClick(Qt.Key_Menu, Qt.NoModifier, -1);
            return true;
        });
        copyOnly("the Menu key");
        win.add("Shift+F10", () => {
            win.pg().conversations.forceActiveFocus();
            win.ev.keyClick(Qt.Key_F10, Qt.ShiftModifier, -1);
            return true;
        });
        copyOnly("Shift+F10");
        win.openFor(() => win.m.u);
        win.add("a chat with unread messages shows only Mark as Read", () => {
            if (win.texts() !== "Mark as Read") return win.fail("the menu of an unread chat of a network with no entries: " + win.texts());
            return true;
        });
        win.shutMenu();
    }

    function menuRead(): void {
        win.add("an unread chat", () => {
            const un = win.rowsNow().find(r => !r.isSelf && r.unread > 0 && !r.markedUnread && win.wholeOnScreen(r));
            if (!un) return win.hold("a chat with unread messages");
            win.m.c = un.conversationId;
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("Mark as Read leads, no Mark as Unread", () => {
            const p = win.pg();
            if (!p.markReadEntry.visible || p.markUnreadEntry.visible) return win.fail("an unread chat does not offer Mark as Read alone");
            if (win.entries()[0].text !== "Mark as Read") return win.fail("Mark as Read is not the first entry");
            return true;
        });
        win.shutMenu();
        win.add("positive control: opening the chat reads it", () => {
            win.openConversation(win.m.c);
            return true;
        });
        win.add("the open chat is read away", () => {
            if (win.offer(win.m.c).hasUnread !== false) return win.hold("the opened chat counted as read (a window that counts as inactive reads nothing)");
            return true;
        });
        win.add("wait for the row", () => win.rowOfId(win.m.c) && win.wholeOnScreen(win.rowOfId(win.m.c)) ? true : win.hold("the row on screen"));
        win.openFor(() => win.m.c);
        win.add("Mark as Unread replaces it; click it on the OPEN chat", () => {
            const p = win.pg();
            if (!p.markUnreadEntry.visible || p.markReadEntry.visible) return win.fail("a read chat does not offer Mark as Unread alone");
            win.mark();
            win.click(p.markUnreadEntry);
            return true;
        });
        win.add("it is marked", () => {
            if (win.offer(win.m.c).markedUnread !== true) return win.hold("the mark");
            return true;
        });
        win.add("and stays marked on the open chat", () => {
            if (!win.stays(30)) return false;
            if (App.timeline.conversationId !== win.m.c) return win.fail("the chat closed");
            if (win.offer(win.m.c).markedUnread !== true) return win.fail("the open chat read its mark away");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.c);
        win.add("Mark as Read clears it", () => {
            const p = win.pg();
            if (!p.markReadEntry.visible || p.markUnreadEntry.visible) return win.fail("a marked chat does not offer Mark as Read alone");
            win.click(p.markReadEntry);
            return true;
        });
        win.add("cleared", () => win.offer(win.m.c).markedUnread === false && win.offer(win.m.c).hasUnread === false ? true : win.hold("the mark clears"));
    }

    function noteToSelf(): void {
        win.add("the two chats", () => {
            const note = win.rowsNow().find(r => r.isSelf && win.wholeOnScreen(r));
            const ids = win.plainIds();
            if (!note || ids.length < 1) return win.hold("Note to Self and an ordinary chat");
            win.m.note = note.conversationId;
            win.m.c = ids[0];
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("an ordinary chat offers Mute (the control)", () => {
            if (win.texts() !== "Mark as Unread|Pin|Mute|Archive") return win.fail("the ordinary chat's entries: " + win.entries().length);
            if (win.offer(win.m.c).muteOffered !== true) return win.fail("an ordinary chat does not offer Mute");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.note);
        win.add("Note to Self: no Mute, the others there and enabled", () => {
            if (win.offer(win.m.note).muteOffered !== false) return win.fail("Note to Self offers Mute");
            if (win.texts() !== "Mark as Unread|Pin|Archive") return win.fail("Note to Self's entries: " + win.entries().length);
            for (const e of win.entries()) {
                if (!e.enabled) return win.fail("an entry of Note to Self is greyed");
            }
            win.mark();
            win.click(win.pg().pinEntry);
            return true;
        });
        win.add("Pin works there", () => win.offer(win.m.note).pinned === true && win.heard("ChangeConversation") === win.m.base + 1 ? true : win.hold("Note to Self is pinned"));
    }

    function chatMenu(): void {
        win.add("two chats", () => {
            const ids = win.plainIds();
            if (ids.length < 2) return win.hold("two ordinary chats");
            win.m.c = ids[0];
            win.m.d = ids[1];
            return true;
        });
        win.add("open the first", () => {
            win.openConversation(win.m.c);
            return true;
        });
        win.add("the page has the chat", () => App.timeline.conversationId === win.m.c && win.chatPage && win.offerNow().shown === true ? true : win.hold("the chat page and its offer"));
        win.add("the entries are there, enabled, with their words", () => {
            for (const t of ["Mark as Unread", "Pin", "Archive"]) {
                const a = win.shown(win.actionsNamed(t));
                if (a.length !== 1 || !a[0].enabled) return win.fail("the chat menu's " + t + " is not shown and enabled");
            }
            const mutes = win.shown(win.actionsNamed("Mute"));
            if (mutes.length !== 1 || !mutes[0].enabled) return win.fail("the chat menu's Mute is not one enabled entry");
            if (win.chatPage.offerLine !== "") return win.fail("a reason shows with nothing wrong");
            win.mark();
            win.actionsNamed("Pin")[0].trigger();
            return true;
        });
        win.add("Pin reaches the fake; the text turns to Unpin", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.actionsNamed("Unpin").length !== 1 || win.shown(win.actionsNamed("Unpin")).length !== 1) return win.hold("the action reads Unpin");
            win.mark();
            win.actionsNamed("Unpin")[0].trigger();
            return true;
        });
        win.add("Unpin works", () => win.offer(win.m.c).pinned === false && win.heard("ChangeConversation") === win.m.base + 1 && win.shown(win.actionsNamed("Pin")).length === 1 ? true : win.hold("Unpin"));
        win.add("Mark as Unread", () => {
            win.mark();
            win.actionsNamed("Mark as Unread")[0].trigger();
            return true;
        });
        win.add("marked", () => win.offer(win.m.c).markedUnread === true && win.heard("ChangeConversation") === win.m.base + 1 ? true : win.hold("the mark"));
        win.add("Archive", () => {
            win.mark();
            win.actionsNamed("Archive")[0].trigger();
            return true;
        });
        win.add("archived, and the notice's Undo stands", () => win.offer(win.m.c).archived === true && win.heard("ChangeConversation") === win.m.base + 1 ? true : win.hold("the archive"));
        win.switchOff();
        win.add("open the second", () => {
            win.openConversation(win.m.d);
            return true;
        });
        win.add("the off state: the flat Mute, greyed, the reason", () => {
            if (App.timeline.conversationId !== win.m.d || !win.chatPage || win.offerNow().shown !== true) return win.hold("the second chat");
            if (win.offerNow().on !== false) return win.hold("the offer says Off");
            const mutes = win.shown(win.actionsNamed("Mute"));
            if (mutes.length !== 1 || mutes[0].enabled) return win.fail("the off state does not show one greyed Mute");
            for (const t of ["Mark as Unread", "Pin", "Archive"]) {
                const a = win.shown(win.actionsNamed(t));
                if (a.length !== 1 || a[0].enabled) return win.fail("the off state's " + t + " is not greyed");
            }
            if (win.chatPage.offerLine === "" || win.chatPage.offerLine !== win.offerNow().why) return win.fail("the page's line is not the reason");
            win.mark();
            for (const t of ["Mark as Unread", "Pin", "Mute", "Archive"]) {
                win.actionsNamed(t)[0].trigger();
            }
            return true;
        });
        win.add("triggering the greyed ones sends nothing", () => win.stays(15) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("a greyed action sent a change")) : false);
    }

    function chatArchived(): void {
        win.add("the archive's chat found and opened", () => {
            if (win.pc < 0) return false;
            App.showArchive(true);
            return true;
        });
        win.add("the archive lists it", () => {
            const rows = win.rowsNow();
            if (rows.length < 1 || !App.archiveShown) return win.hold("the archive view");
            win.m.a = rows[0].conversationId;
            win.pickedChat(win.m.a);
            return true;
        });
        win.add("it is open", () => App.timeline.conversationId === win.m.a && win.chatPage && win.offerNow().shown === true ? true : win.hold("the archived chat opens"));
        win.add("the page says Unarchive", () => {
            if (win.offerNow().archived !== true) return win.fail("the chat is not archived");
            if (win.actionsNamed("Unarchive").length !== 1 || win.actionsNamed("Archive").length !== 0) return win.fail("an archived chat's action does not read Unarchive");
            win.mark();
            win.actionsNamed("Unarchive")[0].trigger();
            return true;
        });
        win.add("it comes back, the chat stays open, the text turns to Archive", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.a).archived !== false) return win.hold("the echo");
            if (App.timeline.conversationId !== win.m.a) return win.fail("the chat closed");
            if (win.actionsNamed("Archive").length !== 1 || win.actionsNamed("Unarchive").length !== 0) return win.hold("the action's text turns to Archive after the echo");
            return true;
        });
        win.add("the list shows the chats and the chat", () => {
            if (App.archiveShown) return win.hold("the list leaves the archive view");
            if (!win.listed(win.m.a)) return win.hold("the chat among the inbox's rows");
            return true;
        });
    }

    // ---- D7 ----

    // The archived chat of the fixture (its id), through the archive view.
    function findArchived(): void {
        win.add("show the archive", () => {
            App.showArchive(true);
            return true;
        });
        win.add("the archived chat", () => {
            const rows = win.rowsNow();
            if (rows.length < 1 || !App.archiveShown) return win.hold("the archive view");
            win.m.arch = rows[0].conversationId;
            App.showArchive(false);
            return true;
        });
        win.add("back at the chats", () => !App.archiveShown && win.plainIds().length > 0 ? true : win.hold("the chats"));
    }

    function keys(): void {
        win.findArchived();
        win.add("no chat open: Ctrl+Shift+A", () => {
            if (App.timeline.conversationId !== 0) return win.fail("a chat is open at the start");
            win.mark();
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("nothing", () => win.stays(12) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("a key with no chat open reached the fake")) : false);
        win.add("open the archived chat", () => {
            win.openConversation(win.m.arch);
            return true;
        });
        win.add("it is open", () => App.timeline.conversationId === win.m.arch && win.chatShown && win.offerNow().archived === true ? true : win.hold("the archived chat is shown"));
        win.add("Ctrl+Shift+A on an archived chat", () => {
            win.mark();
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("nothing: already archived", () => win.stays(12) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("Ctrl+Shift+A on an archived chat reached the fake")) : false);
        if (win.narrow) {
            win.add("back at the list", () => {
                win.pageStack.currentIndex = 0;
                return true;
            });
            win.add("the list is the page", () => win.pageStack.currentItem === win.pg() && !win.chatKeys ? true : win.hold("the list shows alone"));
        } else {
            win.add("Settings over the chat", () => {
                win.openSettings();
                return true;
            });
            win.add("Settings is up and the chat is not shown", () => {
                if (!win.settingsPage) return win.hold("Settings");
                if (win.chatKeys) return win.hold("the chat is no longer what the keys act on under Settings");
                return true;
            });
        }
        win.add("Ctrl+Shift+U under it", () => {
            win.mark();
            win.ctrlShift(Qt.Key_U);
            return true;
        });
        win.add("nothing under it", () => win.stays(12) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("Ctrl+Shift+U reached the fake while the chat was covered")) : false);
        win.add("the chat again", () => {
            if (win.narrow) {
                win.pageStack.currentIndex = win.pageStack.depth - 1;
            } else {
                win.openConversation(win.m.arch);
            }
            return true;
        });
        win.add("it is shown", () => App.timeline.conversationId === win.m.arch && win.chatShown ? true : win.hold("the chat is shown again"));
        win.add("Ctrl+Shift+U brings it back", () => {
            win.mark();
            win.ctrlShift(Qt.Key_U);
            return true;
        });
        win.add("unarchived, the chat stays", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.arch).archived !== false) return win.hold("the echo");
            if (App.timeline.conversationId !== win.m.arch) return win.fail("the chat closed when it came back");
            return true;
        });
        win.add("Ctrl+Shift+U on a chat in the inbox", () => {
            win.mark();
            win.ctrlShift(Qt.Key_U);
            return true;
        });
        win.add("nothing: not archived", () => win.stays(12) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("Ctrl+Shift+U on a chat in the inbox reached the fake")) : false);
        // The covered or listed chat that is not archived: both keys under cover.
        win.add("a second chat", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            win.openConversation(win.m.c);
            return true;
        });
        win.add("it is shown", () => App.timeline.conversationId === win.m.c && win.chatShown ? true : win.hold("the second chat is shown"));
        if (win.narrow) {
            win.add("back at the list again", () => {
                win.pageStack.currentIndex = 0;
                return true;
            });
            win.add("the list alone", () => win.pageStack.currentItem === win.pg() && !win.chatKeys ? true : win.hold("the list shows alone"));
        } else {
            win.add("Settings over it", () => {
                win.openSettings();
                return true;
            });
            win.add("Settings is up", () => win.settingsPage && !win.chatKeys ? true : win.hold("Settings, and the chat not what the keys act on"));
        }
        win.add("Ctrl+Shift+A under it", () => {
            win.mark();
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("nothing", () => win.stays(12) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("Ctrl+Shift+A reached the fake while the chat was covered or not on screen")) : false);
        win.add("the chat shown again", () => {
            if (win.narrow) {
                win.pageStack.currentIndex = win.pageStack.depth - 1;
            } else {
                win.openConversation(win.m.c);
            }
            return true;
        });
        win.add("shown", () => App.timeline.conversationId === win.m.c && win.chatShown ? true : win.hold("the chat is shown"));
        win.add("Ctrl+Shift+A archives the open chat", () => {
            win.mark();
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("archived and the chat closes", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.c).archived !== true) return win.hold("the echo");
            if (App.timeline.conversationId !== 0) return win.hold("the chat closes");
            if (win.narrow && win.pageStack.currentItem !== win.pg()) return win.hold("a narrow window is back at the list");
            return true;
        });
    }

    function keysOff(): void {
        win.add("a chat", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            win.openConversation(win.m.c);
            return true;
        });
        win.switchOff();
        win.add("the open chat's offer is Off", () => App.timeline.conversationId === win.m.c && win.chatShown && win.offerNow().on === false ? true : win.hold("the open chat's offer says Off"));
        win.add("Ctrl+Shift+A", () => {
            win.mark();
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("the greyed line's words, nothing sent", () => {
            const why = win.offerNow().why;
            if (why === "") return win.fail("the offer has no reason");
            if (!win.toasted(why)) return win.hold("the notice with the reason's words");
            if (win.heard("ChangeConversation") !== win.m.base) return win.fail("the key reached the fake with the sync off");
            return true;
        });
        win.add("and stays so", () => {
            if (!win.stays(15)) return false;
            if (win.heard("ChangeConversation") !== win.m.base || win.offer(win.m.c).archived || win.undoStanding !== null) return win.fail("the key changed something with the sync off");
            return true;
        });
    }

    function undoButton(): Item {
        const found = win.shownText("Undo");
        return found.length > 0 ? found[0] : null;
    }

    function undo(): void {
        win.add("a chat, open", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            win.openConversation(win.m.c);
            return true;
        });
        win.add("shown", () => App.timeline.conversationId === win.m.c && win.chatShown ? true : win.hold("the chat is shown"));
        win.add("Ctrl+Shift+A", () => {
            win.mark();
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("closed, archived, the notice with Undo", () => {
            if (win.offer(win.m.c).archived !== true) return win.hold("the echo");
            if (App.timeline.conversationId !== 0) return win.hold("the chat closes");
            if (win.undoStanding === null || win.undoStanding.reopen !== true) return win.hold("the Undo stands for a chat the archive closed");
            if (!win.undoButton()) return win.hold("the Undo button on screen");
            if (win.chatToldAnnounced !== "Chat archived" || win.chatToldPoliteness !== Accessible.Polite) return win.fail("'Chat archived' was not announced politely");
            return true;
        });
        win.add("a real click on Undo", () => {
            win.m.after = win.heard("ChangeConversation");
            if (!win.click(win.undoButton())) return win.fail("the window took no mouse event");
            return true;
        });
        win.add("unarchived, the chat open again", () => {
            if (win.offer(win.m.c).archived !== false) return win.hold("the echo");
            if (App.timeline.conversationId !== win.m.c) return win.hold("the chat opens again");
            if (win.heard("ChangeConversation") !== win.m.after + 1) return win.fail("Undo sent more than one change");
            return true;
        });
        win.add("no second notice", () => {
            if (!win.stays(25)) return false;
            if (win.toasted("Chat returned to inbox") || win.chatToldAnnounced !== "Chat archived") return win.fail("Undo told 'Chat returned to inbox'");
            return true;
        });
        // Again, and now the key with no chat open.
        // (The click on the toast's Undo leaves the keyboard on the toast's button, in the overlay, where the
        // window's keys are off: `keysActive` false until the keyboard moves. The person's next key needs a
        // click or a Tab first; open item. The test puts the keyboard on the list.)
        win.add("the chat is shown again", () => {
            if (!win.chatShown) return win.hold("the reopened chat is shown");
            win.pg().conversations.forceActiveFocus();
            return true;
        });
        win.add("the keys are on", () => win.keysActive ? true : win.hold("the window's keys are on"));
        win.add("archive again by Ctrl+Shift+A", () => {
            win.ctrlShift(Qt.Key_A);
            return true;
        });
        win.add("closed and archived, the Undo stands", () => {
            if (win.offer(win.m.c).archived === true && App.timeline.conversationId === 0 && win.undoStanding !== null) return true;
            if (win.waits > 100) return win.fail("the second archive by key did not come (archived " + win.offer(win.m.c).archived + ", open " + (App.timeline.conversationId !== 0) + ", undo " + (win.undoStanding !== null) + ", keys active " + win.keysActive + ", chat shown " + win.chatShown + ", changes " + win.heard("ChangeConversation") + ")");
            return win.hold("archived, closed, the Undo stands");
        });
        win.add("Ctrl+Shift+U with no chat open", () => {
            win.m.after = win.heard("ChangeConversation");
            win.ctrlShift(Qt.Key_U);
            return true;
        });
        win.add("the same undo", () => {
            if (win.offer(win.m.c).archived !== false) return win.hold("the echo");
            if (App.timeline.conversationId !== win.m.c) return win.hold("the chat opens again");
            return true;
        });
        win.add("and once", () => {
            if (win.quiet === 0) {
                win.m.after = win.heard("ChangeConversation");
                win.ctrlShift(Qt.Key_U);
            }
            if (!win.stays(20)) return false;
            if (win.heard("ChangeConversation") !== win.m.after) return win.fail("a second Ctrl+Shift+U undid again");
            return true;
        });
    }

    // Archives chats `a` and `b` one after the other, keeping the first one's standing.
    function twoArchives(): void {
        win.add("two chats", () => {
            const ids = win.plainIds();
            if (ids.length < 2) return win.hold("two ordinary chats");
            win.m.a = ids[0];
            win.m.b = ids[1];
            return true;
        });
        win.add("archive the first", () => {
            win.archiveChat(win.m.a);
            return true;
        });
        win.add("its toast stands", () => {
            if (win.offer(win.m.a).archived !== true) return win.hold("the echo");
            if (win.undoStanding === null || win.undoStanding.conv !== win.m.a) return win.hold("the first standing");
            win.m.s1 = win.undoStanding;
            return true;
        });
        win.add("archive the second", () => {
            win.archiveChat(win.m.b);
            return true;
        });
        win.add("two toasts, two standings", () => {
            if (win.offer(win.m.b).archived !== true) return win.hold("the echo");
            if (win.undoStanding === null || win.undoStanding.conv !== win.m.b) return win.hold("the second standing");
            if (win.undoStanding === win.m.s1) return win.fail("the standing is one slot for both");
            if (win.shownText("Undo").length < 2) return win.hold("two Undo buttons");
            return true;
        });
    }

    // H2: the first toast's Undo brings back the FIRST chat and leaves the second archived.
    function undoTwo(): void {
        win.twoArchives();
        win.add("the first toast's Undo", () => {
            win.mark();
            win.undoArchive(win.m.s1);
            return true;
        });
        win.add("the first is back, the second stays archived", () => {
            if (win.offer(win.m.a).archived !== false) return win.hold("the first comes back");
            if (win.offer(win.m.b).archived !== true) return win.fail("the second came back with the first");
            if (win.undoStanding === null || win.undoStanding.conv !== win.m.b) return win.fail("the first Undo cleared the second's standing");
            return true;
        });
        win.add("both toasts' buttons clicked: the first does nothing more, the second undoes its own", () => {
            win.m.after = win.heard("ChangeConversation");
            for (const b of win.shownText("Undo")) {
                win.click(b);
            }
            return true;
        });
        win.add("only the second came back, once", () => {
            if (win.offer(win.m.b).archived !== false) return win.hold("the second comes back");
            if (!win.stays(25)) return false;
            if (win.heard("ChangeConversation") !== win.m.after + 1) return win.fail("the buttons sent more than the second's one change");
            if (win.offer(win.m.a).archived !== false) return win.fail("the first went away again");
            return true;
        });
    }

    // M1: a toast's own Undo works after the key's window of 8 s is over (the pointer rested on it).
    function undoLate(): void {
        win.twoArchives();
        win.add("the key's window runs out", () => {
            if (win.m.t0 === undefined) win.m.t0 = Date.now();
            if (Date.now() - win.m.t0 < 8600) return win.hold("the 8 seconds");
            if (win.undoStanding !== null) return win.hold("the timer clears the key's standing");
            return true;
        });
        win.add("the first toast's Undo still works", () => {
            win.undoArchive(win.m.s1);
            return true;
        });
        win.add("the first is back", () => win.offer(win.m.a).archived === false ? true : win.hold("the first comes back"));
    }

    function notices(): void {
        win.findArchived();
        win.add("open the archived chat", () => {
            win.openConversation(win.m.arch);
            return true;
        });
        win.add("shown", () => App.timeline.conversationId === win.m.arch && win.chatShown && win.offerNow().archived === true ? true : win.hold("the archived chat is shown"));
        win.add("Ctrl+Shift+U", () => {
            win.ctrlShift(Qt.Key_U);
            return true;
        });
        win.add("'Chat returned to inbox' only after the row changed", () => {
            const changed = win.offer(win.m.arch).archived === false;
            const told = win.toasted("Chat returned to inbox");
            if (told && !changed) return win.fail("the notice came before the row changed");
            if (!changed || !told) return win.hold("the row, then the notice");
            return true;
        });
        win.add("pick", () => {
            const ids = win.plainIds().filter(id => id !== win.m.arch);
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("Mark as Unread", () => {
            win.click(win.pg().markUnreadEntry);
            return true;
        });
        win.add("'Chat marked unread' only after the row changed", () => {
            const changed = win.offer(win.m.c).markedUnread === true;
            const told = win.toasted("Chat marked unread");
            if (told && !changed) return win.fail("the notice came before the row changed");
            if (!changed || !told) return win.hold("the row, then the notice");
            return true;
        });
    }

    function undoPinned(): void {
        win.add("a pinned chat", () => {
            const p = win.rowsNow().find(r => r.pinned && r.unread === 0 && !r.markedUnread && win.wholeOnScreen(r));
            if (!p) return win.hold("a pinned chat with nothing unread");
            win.m.c = p.conversationId;
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("Archive it", () => {
            if (win.pg().pinEntry.text !== "Unpin") return win.fail("the chat is not pinned");
            win.click(win.pg().archiveMenuEntry);
            return true;
        });
        win.add("archived and unpinned by the fake's coupling, the Undo stands", () => {
            const o = win.offer(win.m.c);
            if (o.archived !== true) return win.hold("the echo");
            if (o.pinned !== false) return win.fail("archiving left the pin");
            if (win.undoStanding === null || win.undoStanding.wasPinned !== true || !win.undoButton()) return win.hold("the Undo stands and knows it was pinned");
            return true;
        });
        win.add("a real click on Undo", () => {
            if (!win.click(win.undoButton())) return win.fail("the window took no mouse event");
            return true;
        });
        win.add("back, and pinned again", () => {
            if (win.offer(win.m.c).archived !== false) return win.hold("the echo");
            if (win.offer(win.m.c).pinned !== true) return win.hold("the pin comes back");
            return true;
        });
    }

    // Down keys until the sub-menu's entry is the current one, then Right; the sub-menu's first entry
    // takes Return.
    function mute(): void {
        win.add("three chats", () => {
            const ids = win.plainIds();
            if (ids.length < 3) return win.hold("three ordinary chats");
            win.m.a = ids[0];
            win.m.b = ids[1];
            win.m.d = ids[2];
            App.muteFor(win.m.d, "eight");
            return true;
        });
        win.openFor(() => win.m.a);
        win.add("Down to Mute", () => {
            const p = win.pg();
            if (p.rowMenu.itemAt(p.rowMenu.currentIndex) === p.rowMenu.muteItem) return true;
            if (win.waits > 12) return win.fail("Down never reached the Mute entry");
            win.ev.keyClick(Qt.Key_Down, Qt.NoModifier, -1);
            return win.hold("Down reaches Mute");
        });
        win.add("Right opens the sub-menu", () => {
            win.ev.keyClick(Qt.Key_Right, Qt.NoModifier, -1);
            return true;
        });
        win.add("the sub-menu is open, Return on 1 Hour", () => {
            const p = win.pg();
            if (!p.muteSub.opened) return win.hold("the sub-menu opens by Right");
            if (p.muteSub.count !== 6) return win.fail("the sub-menu is not whole");
            win.mark();
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            return true;
        });
        win.add("muted for an hour", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            const o = win.offer(win.m.a);
            if (o.muted !== true) return win.hold("the row shows muted");
            if (o.mutedLabel.indexOf("Muted until ") !== 0) return win.fail("an hour's mute is not 'Muted until …'");
            const eight = win.offer(win.m.d).mutedLabel;
            if (eight === "" || eight === o.mutedLabel) return win.fail("an hour and eight hours end at the same time");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.b);
        win.add("a click on Mute opens the sub-menu", () => {
            win.click(win.pg().rowMenu.muteItem);
            return true;
        });
        win.add("the sub-menu, a click on Always", () => {
            const p = win.pg();
            if (!p.muteSub.opened) return win.hold("the sub-menu opens by a click");
            win.mark();
            win.click(p.muteSub.itemAt(5));
            return true;
        });
        win.add("muted always", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            const o = win.offer(win.m.b);
            if (o.muted !== true) return win.hold("Always mutes the chat");
            if (o.mutedLabel !== "Muted always") return win.fail("Always is not a mute with no end");
            return true;
        });
    }

    // M5/M6: Menu key, Mute, Until… by keys only. The dialog has the keyboard right after it opened
    // (the list did not take it back meanwhile and did not move), its fields are right, and after
    // Cancel the list has the keyboard again.
    function untilKeys(): void {
        win.add("a chat, the list current on it, the Menu key", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            const l = win.pg().conversations;
            for (let i = 0; i < l.count; ++i) {
                const r = l.itemAtIndex(i);
                if (r && r.conversationId === win.m.c) {
                    l.currentIndex = i;
                }
            }
            win.m.index = l.currentIndex;
            l.forceActiveFocus();
            win.ev.keyClick(Qt.Key_Menu, Qt.NoModifier, -1);
            return true;
        });
        win.add("the menu is up", () => {
            const p = win.pg();
            if (!p.rowMenu.opened) return win.hold("the menu opens");
            return true;
        });
        win.add("Down to Mute", () => {
            const p = win.pg();
            if (p.rowMenu.itemAt(p.rowMenu.currentIndex) === p.rowMenu.muteItem) return true;
            if (win.waits > 12) return win.fail("Down never reached the Mute entry");
            win.ev.keyClick(Qt.Key_Down, Qt.NoModifier, -1);
            return win.hold("Down reaches Mute");
        });
        win.add("Right opens the sub-menu", () => {
            win.ev.keyClick(Qt.Key_Right, Qt.NoModifier, -1);
            return true;
        });
        win.add("Down to Until…", () => {
            const p = win.pg();
            if (!p.muteSub.opened) return win.hold("the sub-menu opens by Right");
            if (p.muteSub.currentIndex === 4) return true;
            if (win.waits > 20) return win.fail("Down never reached Until…");
            win.ev.keyClick(Qt.Key_Down, Qt.NoModifier, -1);
            return win.hold("Down reaches Until…");
        });
        win.add("Return on Until…", () => {
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            return true;
        });
        win.add("the dialog has the keyboard right after it opened", () => {
            const d = win.muteUntilDialog;
            if (!d || !d.opened) return win.hold("the dialog opens");
            if (!win.untilStartOk(d)) return win.fail("the fields were not at tomorrow 8:00 when the dialog opened");
            let f = win.activeFocusItem;
            while (f && f !== d.contentItem) f = f.parent;
            if (!f) return win.fail("the dialog does not have the keyboard right after it opened");
            if (win.pg().conversations.activeFocus) return win.fail("the list has the keyboard behind the dialog");
            return true;
        });
        win.add("the list did not move, and stays quiet behind the dialog", () => {
            if (win.pg().conversations.currentIndex !== win.m.index) return win.fail("the list's current row moved");
            if (!win.stays(15)) return false;
            if (win.pg().conversations.activeFocus) return win.fail("the list took the keyboard back under the dialog");
            if (App.timeline.conversationId !== 0) return win.fail("a key behind the dialog opened a chat");
            return true;
        });
        win.add("Escape cancels", () => {
            win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            return true;
        });
        win.add("the dialog is closed and the list has the keyboard again", () => {
            if (win.muteUntilDialog.opened || win.muteUntilDialog.visible) return win.hold("the dialog closes");
            if (!win.pg().conversations.activeFocus) return win.hold("the list takes the keyboard back");
            if (win.pg().conversations.currentIndex !== win.m.index) return win.fail("the list's row changed");
            return true;
        });
    }

    function unmute(): void {
        win.add("a muted chat and a chat to mute for an hour", () => {
            const mu = win.scanRows().find(r => r.muted && !r.isSelf && r.unread === 0 && !r.markedUnread);
            const ids = win.plainIds();
            if (!mu || ids.length < 1) return win.hold("a muted chat and an ordinary one");
            win.m.mu = mu.id;
            win.m.h = ids[0];
            App.muteFor(win.m.h, "hour");
            return true;
        });
        win.openFor(() => win.m.mu);
        win.add("a grey line with the end above Unmute", () => {
            const p = win.pg();
            if (win.offer(win.m.mu).muted !== true) return win.fail("the chat is not muted");
            if (!p.mutedLine.visible || p.mutedLine.children[0].text !== win.offer(win.m.mu).mutedLabel) return win.fail("no line with the end above Unmute");
            if (win.texts() !== "Mark as Unread|Pin|Unmute|Archive") return win.fail("a muted chat's entries: " + win.entries().length);
            if (!p.muteFlat.enabled || p.muteFlat.text !== "Unmute") return win.fail("Unmute is not a flat enabled entry");
            if (p.rowMenu.muteItem.visible) return win.fail("the Mute sub-menu shows for a muted chat");
            win.mark();
            win.click(p.muteFlat);
            return true;
        });
        win.add("Unmute clears it", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.mu).muted !== false) return win.hold("the mute ends");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.mu);
        win.add("Mute is back, the line gone", () => {
            const p = win.pg();
            if (p.mutedLine.visible || !p.rowMenu.muteItem.visible || p.muteFlat.visible) return win.fail("an unmuted chat does not offer the Mute sub-menu alone");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.h);
        win.add("an hour's mute names its end", () => {
            const p = win.pg();
            const label = win.offer(win.m.h).mutedLabel;
            if (!p.mutedLine.visible || label.indexOf("Muted until ") !== 0 || p.mutedLine.children[0].text !== label) return win.fail("the line does not name the end");
            return true;
        });
    }

    // The window's "Mute Until" start: tomorrow 8:00 on the fixture's clock.
    function untilStartOk(d: var): bool {
        // The fixture's calendar is UTC (the pictures do not depend on the machine's zone).
        const t = new Date(FIXTURE_NOW + 86400000);
        return d.year === t.getUTCFullYear() && d.month === t.getUTCMonth() + 1 && d.day === t.getUTCDate() && d.hour === 8 && d.minute === 0;
    }

    function until(): void {
        win.add("two chats", () => {
            const ids = win.plainIds();
            if (ids.length < 2) return win.hold("two ordinary chats");
            win.m.a = ids[0];
            win.m.b = ids[1];
            return true;
        });
        win.openFor(() => win.m.a);
        win.add("Mute opens its sub-menu", () => {
            win.click(win.pg().rowMenu.muteItem);
            return true;
        });
        win.add("Until… from the row menu", () => {
            const p = win.pg();
            if (!p.muteSub.opened) return win.hold("the sub-menu");
            win.click(p.muteSub.itemAt(4));
            return true;
        });
        win.add("the dialog opens for the chat at tomorrow 8:00", () => {
            const d = win.muteUntilDialog;
            if (!d || !d.opened) return win.hold("the dialog");
            if (d.conversation !== win.m.a) return win.fail("the dialog is for another chat");
            if (!win.untilStartOk(d)) return win.fail("the dialog does not start at tomorrow 8:00 on the fixture's clock");
            win.shot("until-menu");
            return true;
        });
        win.add("a real click on the date field", () => {
            const d = win.muteUntilDialog;
            if (!win.click(d.dateField)) return win.fail("the window took no mouse event");
            return true;
        });
        win.add("the calendar is above the dialog", () => {
            const d = win.muteUntilDialog;
            if (!d.datePopup.opened || !d.opened) return win.hold("the calendar opens over the open dialog");
            if (d.datePopup.z < d.z) return win.fail("the calendar is below the dialog");
            // Choose the day after tomorrow, as the calendar's own accept does.
            win.m.start = { y: d.year, m: d.month, d: d.day };
            const v = new Date(d.year, d.month - 1, d.day + 1, 8, 0);
            d.datePopup.value = v;
            d.datePopup.accepted();
            d.datePopup.close();
            return true;
        });
        win.add("the calendar closes, the dialog stays; a held Return presses nothing", () => {
            const d = win.muteUntilDialog;
            if (d.datePopup.opened) return win.hold("the calendar closes");
            if (!d.opened) return win.fail("the calendar's close closed the dialog");
            win.m.chosen = Date.UTC(d.year, d.month - 1, d.day, d.hour, d.minute);
            win.mark();
            d.returnKey({ isAutoRepeat: true, accepted: false });
            if (!d.opened || win.heard("ChangeConversation") !== win.m.base) return win.fail("a stand-in auto-repeat of Return pressed Mute");
            return true;
        });
        win.add("a real Return on Mute", () => {
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            return true;
        });
        win.add("muted until the chosen day", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            const d = win.muteUntilDialog;
            if (d.opened) return win.hold("Mute closes the dialog");
            if (win.offer(win.m.a).muted !== true) return win.hold("the row shows muted");
            if (win.m.chosen <= Date.UTC(win.m.start.y, win.m.start.m - 1, win.m.start.d, 8, 0)) return win.fail("the chosen end is not later than the start");
            return true;
        });
        win.shutMenu();
        // From the chat's menu: its signal reaches the same dialog.
        win.add("open the second chat", () => {
            win.openConversation(win.m.b);
            return true;
        });
        win.add("its page", () => App.timeline.conversationId === win.m.b && win.chatPage && win.offerNow().shown === true ? true : win.hold("the chat page"));
        win.add("Until… from the chat's menu", () => {
            win.chatPage.untilAsked(win.m.b, null);
            return true;
        });
        win.add("the dialog opens for the second chat at tomorrow 8:00", () => {
            const d = win.muteUntilDialog;
            if (!d || !d.opened || d.conversation !== win.m.b) return win.hold("the dialog for the second chat");
            if (!win.untilStartOk(d)) return win.fail("the dialog does not restart at tomorrow 8:00");
            d.close();
            return true;
        });
    }

    function untilPast(): void {
        win.add("a chat; the dialog", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            win.askMuteUntil(win.m.c);
            return true;
        });
        win.add("a time inside the next minute", () => {
            const d = win.muteUntilDialog;
            if (!d || !d.opened) return win.hold("the dialog");
            const t = new Date(win.coreNow() + 30000);
            d.year = t.getUTCFullYear();
            d.month = t.getUTCMonth() + 1;
            d.day = t.getUTCDate();
            d.hour = t.getUTCHours();
            d.minute = t.getUTCMinutes();
            return true;
        });
        win.add("Mute is disabled, the line shows, Return sends nothing", () => {
            const d = win.muteUntilDialog;
            if (d.allowed) return win.fail("a time inside the next minute is allowed (clock " + Math.round(win.coreNow()) + ", picked " + d.picked.getTime() + ", " + d.year + "-" + d.month + "-" + d.day + " " + d.hour + ":" + d.minute + ")");
            if (d.customFooterActions[0].enabled) return win.fail("the Mute button is enabled for a time inside the next minute");
            const note = win.find(d.contentItem, i => i.objectName === "untilNote");
            if (!note || !note.visible) return win.fail("the line that says why is not shown");
            win.mark();
            d.returnKey({ isAutoRepeat: false, accepted: false });
            if (!d.opened || win.heard("ChangeConversation") !== win.m.base) return win.fail("Return muted for a time inside the next minute");
            return true;
        });
        win.add("a time a little more than a minute ahead", () => {
            const d = win.muteUntilDialog;
            win.m.end = Math.ceil((win.coreNow() + 61000) / 60000) * 60000;
            const t = new Date(win.m.end);
            d.year = t.getUTCFullYear();
            d.month = t.getUTCMonth() + 1;
            d.day = t.getUTCDate();
            d.hour = t.getUTCHours();
            d.minute = t.getUTCMinutes();
            return true;
        });
        win.add("it is allowed", () => {
            const d = win.muteUntilDialog;
            if (!d.allowed || !d.customFooterActions[0].enabled) return win.fail("a time more than a minute ahead is not allowed");
            return true;
        });
        win.add("wait until the clock is inside the minute", () => {
            if (win.coreNow() < win.m.end - 59000) return win.hold("the clock gets near the end");
            return true;
        });
        win.add("the click still gets through the dialog: the core refuses", () => {
            const d = win.muteUntilDialog;
            if (!d.allowed) return win.fail("the dialog re-judged by itself");
            win.mark();
            d.accept();
            if (!d.refused) return win.fail("the refusal did not set the dialog's line");
            return true;
        });
        win.add("the dialog stays with its line, nothing sent", () => {
            const d = win.muteUntilDialog;
            if (!win.stays(15)) return false;
            const note = win.find(d.contentItem, i => i.objectName === "untilNote");
            if (!d.opened || !note || !note.visible) return win.fail("the dialog did not stay with its line");
            if (win.heard("ChangeConversation") !== win.m.base || win.offer(win.m.c).muted) return win.fail("a refused time was sent");
            return true;
        });
    }

    function pinLimit(): void {
        win.add("chats to pin up to four", () => {
            const ids = win.plainIds();
            if (ids.length < 4) return win.hold("four ordinary chats");
            win.m.ids = ids;
            const pinned = win.rowsNow().filter(r => r.pinned).length;
            if (pinned !== 2) return win.fail("the fixture does not pin two chats (" + pinned + ")");
            App.setPinned(ids[0], true);
            App.setPinned(ids[1], true);
            return true;
        });
        win.add("four pinned", () => win.offer(win.m.ids[0]).pinned === true && win.offer(win.m.ids[1]).pinned === true ? true : win.hold("two more pins"));
        win.openFor(() => win.m.ids[2]);
        win.add("the fifth: Pin greyed with the sentence", () => {
            const p = win.pg();
            if (!p.pinEntry.visible || p.pinEntry.enabled) return win.fail("Pin is not greyed at the limit");
            const line = "You can pin up to 4 chats.";
            if (p.rowMenu.line !== line || !p.whyLine.visible) return win.fail("the line is not 'You can pin up to 4 chats.'");
            if (p.pinEntry.Accessible.description !== line) return win.fail("Pin's accessible description is not the sentence");
            if (!p.archiveMenuEntry.enabled || !p.markUnreadEntry.enabled) return win.fail("the other entries are greyed by the pin limit");
            win.mark();
            win.click(p.pinEntry);
            return true;
        });
        win.add("a click sends nothing", () => {
            if (!win.stays(15)) return false;
            if (win.heard("ChangeConversation") !== win.m.base || win.offer(win.m.ids[2]).pinned) return win.fail("a greyed Pin sent a change");
            return true;
        });
        win.shutMenu();
        win.openFor(() => win.m.ids[0]);
        win.add("a pinned one still says Unpin and works", () => {
            const p = win.pg();
            if (p.pinEntry.text !== "Unpin" || !p.pinEntry.enabled) return win.fail("a pinned chat's Unpin is not enabled at the limit");
            win.mark();
            win.click(p.pinEntry);
            return true;
        });
        win.add("unpinned", () => win.heard("ChangeConversation") === win.m.base + 1 && win.offer(win.m.ids[0]).pinned === false ? true : win.hold("the unpin"));
    }

    function blended(): void {
        win.add("the two accounts' chats", () => {
            const rows = win.scanRows().filter(r => !r.isSelf);
            const badges = {};
            for (const r of rows) {
                if (r.badge === "") return win.fail("a row has no account mark in a blended list");
                badges[r.badge] = (badges[r.badge] || 0) + 1;
            }
            const names = Object.keys(badges);
            if (names.length !== 2) return win.hold("rows of two accounts");
            win.m.bBadge = names.sort((a, b) => badges[a] - badges[b])[0];
            const work = App.accountActions.filter(a => a.about.startsWith("Demo Work"));
            if (work.length !== 1) return win.fail("the second account is not found");
            win.m.bAcc = work[0].account;
            win.m.aAcc = App.accountActions.find(a => a.account !== win.m.bAcc).account;
            return true;
        });
        // The pin limit counts one account's own pins: the second account is filled to its limit
        // while the first (two pins of its own) can still pin.
        win.add("pin the second account's chats up to its limit", () => {
            const all = win.scanRows().filter(r => r.badge === win.m.bBadge && !r.isSelf);
            const bs = all.filter(r => !r.pinned && win.offer(r.id).archived !== true);
            const pinned = all.filter(r => r.pinned).length;
            if (bs.length + pinned < 5) return win.fail("the second account has too few chats for the limit");
            win.m.pins = bs.slice(0, 4 - pinned).map(r => r.id);
            for (const id of win.m.pins) {
                App.setPinned(id, true);
            }
            return true;
        });
        win.add("they are pinned", () => win.m.pins.every(id => win.offer(id).pinned === true) ? true : win.hold("the pins"));
        win.add("the second account's next chat is at its limit, the first account's is not", () => {
            const rows = win.scanRows().filter(r => !r.isSelf && !r.pinned && win.offer(r.id).archived !== true);
            const b = rows.find(r => r.badge === win.m.bBadge);
            const a = rows.find(r => r.badge !== win.m.bBadge);
            if (!b || !a) return win.fail("a chat of each account is missing");
            if (win.offer(b.id).pinAtLimit !== true) return win.hold("the second account's limit is stated on its own chats");
            if (win.offer(a.id).pinAtLimit === true) return win.fail("the first account's Pin is greyed by the other account's pins");
            return true;
        });
        win.add("turn the first account's switch off", () => {
            App.setRoamingWrites(win.m.aAcc, false);
            return true;
        });
        win.add("the echo", () => win.actionOf(win.m.aAcc).roaming === "off" && win.actionOf(win.m.bAcc).roaming === "on" ? true : win.hold("one account Off, the other On"));
        win.add("one account's chats are greyed, the other's enabled", () => {
            const rows = win.scanRows().filter(r => !r.isSelf);
            let off = 0;
            let on = 0;
            for (const r of rows) {
                const isB = r.badge === win.m.bBadge;
                const o = win.offer(r.id);
                if (isB && o.on !== true) return win.hold("the second account's chats are On");
                if (!isB && o.on !== false) return win.hold("the first account's chats are Off");
                if (isB) on += 1;
                else off += 1;
            }
            if (on < 1 || off < 1) return win.fail("the list lacks a chat of one account");
            const b = rows.filter(r => r.badge === win.m.bBadge && r.pinned);
            const a = rows.filter(r => r.badge !== win.m.bBadge && r.unread === 0 && !r.markedUnread && !r.pinned && !r.muted);
            if (b.length < 1 || a.length < 1) return win.fail("no ordinary chat of one of the accounts");
            win.m.b = b[0].id;
            win.m.a = a[0].id;
            return true;
        });
        win.openFor(() => win.m.a);
        win.add("the first account's row is greyed with the switch's reason", () => {
            const p = win.pg();
            if (!win.greyedFour(win.offer(win.m.a).why)) return false;
            if (!p.whyLine.visible) return win.fail("no line under the greyed entries");
            win.mark();
            win.click(p.pinEntry);
            return true;
        });
        win.add("nothing was sent", () => win.stays(12) ? (win.heard("ChangeConversation") === win.m.base ? true : win.fail("a greyed account's click was sent")) : false);
        win.shutMenu();
        win.openFor(() => win.m.b);
        win.add("the second account's row is enabled; Archive", () => {
            const p = win.pg();
            if (win.entries().length < 4) return win.fail("the enabled account's entries: " + win.entries().length);
            for (const e of win.entries()) {
                if (!e.enabled) return win.fail("an entry of the account that is On is greyed");
            }
            win.mark();
            win.click(p.archiveMenuEntry);
            return true;
        });
        win.add("it reaches that account's fake and the row follows", () => {
            if (win.heard("ChangeConversation") !== win.m.base + 1) return win.hold("the fake hears one change");
            if (win.offer(win.m.b).archived !== true) return win.hold("the right account's chat is archived");
            if (win.offer(win.m.a).archived) return win.fail("the other account's chat changed");
            return true;
        });
    }

    function settingsSwitch(): void {
        win.add("open Settings", () => {
            win.openSettings();
            return true;
        });
        win.add("the account's row", () => {
            const page = win.settingsPage;
            if (!page) return win.hold("Settings");
            win.m.fx = win.acct();
            const row = win.settingsRow(win.m.fx);
            if (!row || !row.visible || !row.enabled || !row.checked) return win.hold("the row shows On");
            win.reveal(row);
            return true;
        });
        win.add("settle, then a real click", () => {
            if (!win.stays(8)) return false;
            const page = win.settingsPage;
            const row = win.settingsRow(win.m.fx);
            win.m.top = page.flickable.contentY;
            win.mark();
            win.m.writes = win.heard("SetRoamingWrites");
            if (!win.click(row)) return win.fail("the window took no mouse event");
            return true;
        });
        win.add("the switch reaches the fake; the row follows the echo; the page did not move", () => {
            const page = win.settingsPage;
            if (Math.abs(page.flickable.contentY - win.m.top) > 0.5) return win.fail("the page moved by " + Math.round(page.flickable.contentY - win.m.top) + " px");
            if (win.heard("SetRoamingWrites") !== win.m.writes + 1) return win.hold("the fake hears the switch");
            if (win.actionOf(win.m.fx).roaming !== "off") return win.hold("the echo");
            const row = win.settingsRow(win.m.fx);
            if (!row || row.checked) return win.hold("the new row says Off");
            return true;
        });
        win.add("the list's menus follow: greyed", () => {
            const ids = win.plainIds();
            if (ids.length < 1) return win.hold("an ordinary chat");
            win.m.c = ids[0];
            if (win.offer(win.m.c).on !== false) return win.hold("the offer follows the echo");
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("the entries are greyed with the reason", () => {
            if (!win.greyedFour(win.offer(win.m.c).why)) return false;
            if (!win.pg().whyLine.visible) return win.fail("no line under the greyed entries");
            return true;
        });
        win.shutMenu();
        win.add("turn it on again by a real click", () => {
            const row = win.settingsRow(win.m.fx);
            if (!row) return win.hold("the row");
            win.reveal(row);
            return true;
        });
        win.add("settle and click", () => {
            if (!win.stays(8)) return false;
            win.m.writes = win.heard("SetRoamingWrites");
            const row = win.settingsRow(win.m.fx);
            if (!win.click(row)) return win.fail("the window took no mouse event");
            return true;
        });
        win.add("On again; the menus follow", () => {
            if (win.heard("SetRoamingWrites") !== win.m.writes + 1) return win.hold("the fake hears the switch");
            if (win.actionOf(win.m.fx).roaming !== "on") return win.hold("the echo");
            if (win.offer(win.m.c).on !== true) return win.hold("the offer follows");
            return true;
        });
        win.openFor(() => win.m.c);
        win.add("enabled again", () => {
            for (const e of win.entries()) {
                if (!e.enabled) return win.fail("an entry is still greyed after the switch came back");
            }
            return true;
        });
    }

    function settingsRefused(): void {
        win.add("open Settings", () => {
            win.openSettings();
            return true;
        });
        win.add("the row", () => {
            if (!win.settingsPage) return win.hold("Settings");
            win.m.fx = win.acct();
            const row = win.settingsRow(win.m.fx);
            if (!row || !row.visible || !row.enabled || !row.checked) return win.hold("the row shows On");
            win.m.row = row;
            win.reveal(row);
            return true;
        });
        win.add("settle, then a real click", () => {
            if (!win.stays(8)) return false;
            win.m.top = win.settingsPage.flickable.contentY;
            win.m.writes = win.heard("SetRoamingWrites");
            if (!win.click(win.m.row)) return win.fail("the window took no mouse event");
            return true;
        });
        win.add("refused: the old state and the toast", () => {
            if (win.heard("SetRoamingWrites") !== win.m.writes + 1) return win.hold("the fake hears the switch");
            if (!win.toasted("The setting could not be saved.")) return win.hold("the toast");
            const row = win.settingsRow(win.m.fx);
            if (row !== win.m.row) return win.fail("a card was made again for a refusal");
            if (!row.checked) return win.fail("the mark followed the click, not the echo");
            if (Math.abs(win.settingsPage.flickable.contentY - win.m.top) > 0.5) return win.fail("the page moved");
            if (App.composer.notice !== "") return win.fail("the refusal put a notice into the composer");
            return true;
        });
        win.add("and for good", () => {
            if (!win.stays(20)) return false;
            if (win.actionOf(win.m.fx).roaming !== "on" || !win.m.row.checked) return win.fail("a refusal changed the switch");
            if (App.composer.notice !== "") return win.fail("a notice reached the composer later");
            return true;
        });
    }

    Timer {
        interval: TIMEOUT
        running: true
        onTriggered: win.fail("timeout in step " + win.stepName)
    }
}
