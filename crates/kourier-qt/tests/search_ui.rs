//! The Qt side of the message search through the real main window and the real core over the
//! fake network (G1m; offscreen, `harness = false`, silent). G1h extends this binary with the QML
//! cases.
//!
//! `leaves`: a hit on screen (`App.searchResults` holds its row) and the message deleted for
//! everyone: the row is GONE within 1.5 s with no other input (no typing, no call of
//! `search_due` by the test): the App's own wake-up at `search_deadline()` reads the hits again
//! (privacy review MEDIUM-1).
//!
//! `opens`: an OLD hit in a chat with unread messages, opened with `App.openSearchResult`, sends
//! no read receipt and no read marker (an open of the newest page first would mark the chat
//! read, critic B1) and the timeline jumps to the hit. A control opens another unread chat the
//! ordinary way first and must be heard marking it read, so the count the test reads can move.
//! The first timeline query itself (`Around`) is not visible at the App level without new
//! plumbing: the viewmodel's `open_at_an_old_search_hit_reads_around_it_and_marks_nothing_read`
//! holds it; this case holds the App's one open body for a hit.
//!
//! G1h, the list in the window (all real keys through `TestEvent`, a real mouse click on a hit):
//! - `opens` (B1) clicks the oldest hit ROW of the list's footer (a one-chat search) instead of
//!   calling the App: the chat opens at the hit (the jump seen), the chat page shows, the row's
//!   unread count is unchanged, nothing is heard marking it read, and the open chat's first
//!   timeline read is `around` (`App.timelineFetches()`, a test-only accessor of the viewmodel's
//!   `CoreSource`: no `reload` or `newest` before it).
//! - `lone` (L13): a word found in messages but in no chat's name or preview, typed key by key:
//!   no key opens anything or moves the keyboard, the messages are searched only after the 200 ms
//!   delay, every hit row has an accessible name (its line and time), Enter does nothing, Down
//!   enters the hits, Down/Up move between them, Up from the first goes back to the field,
//!   Escape on a hit gives the keyboard back to the field (text and hits stay), the next Escape
//!   in the field clears field and search (G1i: G1h's case kept the hits here). Emptying the
//!   field ends the search at once.
//! - `both`: a word in a chat's preview and in messages: Down from the field reaches the first
//!   chat, Down past the last chat enters the hits (no chat row current), Up comes back to the
//!   last chat; Enter in the field still opens the first chat (the quick switch).
//! - `archived` (M4): a hit in the ARCHIVED chat (a unique word sent there; the chat's name does
//!   not hold it) opens that chat at the hit; the list stays: no archive view, the field keeps its
//!   text, the hits stay.
//!
//! G1i: `noRes` (the placeholder says nothing wrong from the first key: never "No chats match", then
//! "No results for ..." once the search answered, with the index line as its explanation while the
//! index fills; the dev hook holds the backfill back), `found`/`idxGone` (the index line under the
//! "Messages" header while it fills, gone by itself when it is ready: no key), the Escape cases
//! (`escHit`, `escField`: a hit gives the keyboard to the field, the field's Escape clears everything)
//! and `chip*` (Ctrl+Shift+F and the menu entry start a one-chat search with a chip, the X and
//! Backspace take it off, another chat ends it, a hit of the chat keeps it, Escape ends it) and
//! `prot` (a protected chat offers no search).

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;

use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

MainWindow {
    id: win

    width: 1000
    height: 700
    visible: true

    property string step: "start"
    property int waits: 0
    property double mark: 0
    property int freeChat: 0
    property int controlChat: 0
    property int hitChat: 0
    property var cands: []
    property int candIx: 0
    property int hitMessage: 0
    property int deletedMessage: 0
    property int jumpedTo: 0
    property int receipts: 0
    property int quiet: 0
    property int research: 0
    readonly property var results: App.searchResults
    readonly property string word: "zebrafinch"
    // G1h.
    property var ev: null
    property int unreadBefore: 0
    property var inboxRows: []
    property real archivedChat: 0
    property string lone: ""
    property string both: ""
    property string query: ""
    property string typed: ""
    property real before: 0
    readonly property string unique: "quokkahit"
    // G1i.
    readonly property string nothing: "qzxvq"
    property double mark2: 0
    property real chipChat: 0
    property real otherChat: 0
    property real protChat: 0
    property string chipTitleBefore: ""
    // KOURIER_SEARCH_ONLY_B1=1: the run ends after the B1 case (the mutants of the open).
    readonly property bool onlyB1: ONLYB1

    function page(): var {
        return win.pageStack.get(0);
    }
    function field(): var {
        return win.page().searchField;
    }
    function hitRow(i: int): var {
        const rows = win.page().hitRows;
        return rows === null ? null : rows.itemAt(i);
    }
    function hitCount(): int {
        const rows = win.page().hitRows;
        return rows === null ? 0 : rows.count;
    }
    function findHit(message: real): var {
        for (let i = 0; i < win.hitCount(); i++) {
            const r = win.hitRow(i);
            if (r && r.message === message) return r;
        }
        return null;
    }
    // The row comes into the list's view (it sits in the list's footer).
    function showRow(row: var): void {
        const view = win.page().conversations;
        const at = row.mapToItem(view.contentItem, 0, 0);
        if (at.y < view.contentY) view.contentY = at.y;
        else if (at.y + row.height > view.contentY + view.height) view.contentY = at.y + row.height - view.height;
    }
    function click(row: var): void {
        if (!win.ev.mouseClick(row, row.width / 2, row.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) win.fail("the click was not delivered");
    }
    function key(k: int): void {
        if (!win.ev.keyClick(k, Qt.NoModifier, -1)) win.fail("the key event was not delivered");
    }
    function ctrlShiftF(): void {
        if (!win.ev.keyClick(Qt.Key_F, Qt.ControlModifier | Qt.ShiftModifier, -1)) win.fail("the key event was not delivered");
    }
    function named(item: var, name: string): var {
        if (!item) return null;
        if (item.objectName === name) return item;
        for (let i = 0; i < item.children.length; i++) {
            const r = win.named(item.children[i], name);
            if (r) return r;
        }
        return null;
    }
    function indexLineItem(): var {
        return win.named(win.page().conversations.footerItem, "search-index-line");
    }
    function menuAction(text: string): var {
        const acts = win.chatPage.actions;
        for (let i = 0; i < acts.length; i++) {
            if (acts[i].text === text) return acts[i];
        }
        return null;
    }
    function distinctChats(): int {
        const seen = {};
        for (let i = 0; i < win.hitCount(); i++) seen[win.hitRow(i).conversationId] = true;
        return Object.keys(seen).length;
    }
    function unreadOf(conv: real): int {
        for (let i = 0; i < chats.count; i++) {
            const c = chats.objectAt(i);
            if (c && c.conversationId === conv) return c.unread;
        }
        return -1;
    }
    function rowsNow(): var {
        const rows = [];
        for (let i = 0; i < chats.count; i++) {
            const c = chats.objectAt(i);
            if (c) rows.push((c.title + " " + c.preview).toLowerCase());
        }
        return rows;
    }
    // One key of the query per tick: none may open a chat or take the keyboard from the field.
    // True once the whole query is typed.
    function typeOn(): bool {
        if (!win.field().activeFocus) win.fail("a typed letter took the keyboard from the field");
        if (App.timeline.conversationId !== win.before) win.fail("a typed letter opened a chat");
        if (win.typed.length < win.query.length) {
            const ch = win.query[win.typed.length];
            if (!win.ev.keyClickChar(ch, Qt.NoModifier, -1)) win.fail("the key was not delivered");
            win.typed += ch;
            if (win.typed.length === win.query.length) {
                win.mark = Date.now();
                win.quiet = 0;
                // The messages are searched 200 ms after the last key, not at once.
                if (win.results.active) win.fail("the messages were searched before the delay");
            }
            return false;
        }
        if (win.field().text !== win.query) win.fail("the field holds " + win.field().text);
        return true;
    }
    function startTyping(q: string): void {
        win.field().text = "";
        if (win.results.active) win.fail("an empty field did not end the search");
        win.field().forceActiveFocus();
        win.query = q;
        win.typed = "";
        win.before = App.timeline.conversationId;
    }

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 4000) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
    }

    function ask(path: string): string {
        const r = new XMLHttpRequest();
        r.open("GET", "http://127.0.0.1:PORT" + path, false);
        r.send();
        return r.responseText;
    }

    // What the fake network heard of the commands that say "this chat was read".
    function heardReads(): int {
        return parseInt(win.ask("/heard?name=SendReceipts")) + parseInt(win.ask("/heard?name=SyncReadMarker"));
    }

    Instantiator {
        id: chats
        model: App.conversations
        delegate: QtObject {
            required property var conversationId
            required property var unread
            required property string title
            required property string preview
        }
    }

    // The chats of the list: the first with nothing unread, then the ones with unread messages.
    function askHits(): void {
        App.closeSearch();
        App.searchMessages("the", win.cands[win.candIx]);
    }

    function pickChats(): bool {
        const unread = [];
        let free = 0;
        for (let i = 0; i < chats.count; i++) {
            const c = chats.objectAt(i);
            if (!c) continue;
            if (c.unread > 0) unread.push(c.conversationId);
            else if (free === 0) free = c.conversationId;
        }
        if (free === 0 || unread.length < 2) return false;
        win.freeChat = free;
        win.controlChat = unread[0];
        win.cands = unread.slice(1);
        win.hitChat = unread[1];
        return true;
    }

    Connections {
        target: App.timeline
        function onJumpRequested(message: real, row: int): void {
            win.jumpedTo = message;
        }
    }

    property bool busy: false
    Timer {
        interval: 20
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

    // A search that finds nothing yet (the index follows the sends by a moment) is asked again
    // from the start once a second.
    function searchAgain(text: string, chat: real): void {
        win.research += 1;
        if (win.research % 50 === 1) {
            App.closeSearch();
            // The words of the fixture's messages: a chat that has none of the first one
            // is asked for the next.
            const words = ["the", "meet", "good", "plan", "music", "park", "train", "idea"];
            App.searchMessages(text === "the" ? words[Math.floor(win.research / 50) % words.length] : text, chat);
        }
    }

    function tick(): void {
        // G1i: sampled on every tick from the first key on: no "No chats match" ever shows (the
        // line that flipped to hits a moment later).
        if (win.step !== "start" && win.page()) {
            const ph = win.page().placeholder;
            if (ph && ph.visible && ph.text.indexOf("No chats match") >= 0) return win.fail("the placeholder said \"" + ph.text + "\"");
        }
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts || !win.pickChats()) return win.later("the start page and its chats");
            if (win.results.count !== 0 || win.results.active) return win.fail("a search is shown before one was asked");
            win.ev = Qt.createQmlObject("import QtQuick\nimport QtTest\nTestEvent {}", win.contentItem, "search_ui_events.qml");
            win.startTyping(win.nothing);
            win.next("noRes");
            return;
        // G1i: nothing found. The index is held back by the dev hook, so its line is the placeholder's
        // explanation; Escape then clears the field and the search.
        case "noRes": {
            if (!win.typeOn()) return;
            if (!win.page().noResults) return win.later("the answer: no results");
            const ph = win.page().placeholder;
            if (!ph.visible || ph.text.indexOf("No results") !== 0 || ph.text.indexOf(win.nothing) < 0) return win.fail("no 'No results for' line: \"" + ph.text + "\"");
            if (win.results.indexReady) return win.fail("the index was ready before the backfill's delay: the case proves nothing");
            if (ph.explanation.indexOf("%") < 0 || ph.explanation.indexOf("prepared") < 0) return win.fail("the index line is not the placeholder's explanation: \"" + ph.explanation + "\"");
            if (win.page().conversations.count !== 0 || win.hitCount() !== 0) return win.fail("a chat or a hit shows for a word nothing holds");
            win.key(Qt.Key_Escape);
            win.next("noResEsc");
            return;
        }
        case "noResEsc":
            if (win.field().text !== "" || win.results.active || win.page().hitsShown || win.page().chipShown) return win.fail("Escape did not clear the field and the search");
            if (!win.field().activeFocus) return win.fail("Escape took the keyboard from the field");
            win.openConversation(win.freeChat);
            win.next("opened");
            return;
        case "opened":
            if (App.timeline.conversationId !== win.freeChat || App.timeline.loading) return win.later("the free chat");
            App.composer.send(win.freeChat, "hello " + win.word + " here");
            App.searchMessages(win.word, 0);
            win.research = 0;
            win.next("found");
            return;
        case "found":
            if (!win.results.active) return win.fail("the search did not start");
            if (win.results.count < 1) return win.searchAgain(win.word, 0);
            if (win.results.count !== 1 || win.results.conversationAt(0) !== win.freeChat) return win.fail("the hit is not the one message: " + win.results.count);
            // G1i: the index still fills, so the line shows under the "Messages" header.
            if (win.results.indexReady) return win.fail("the index was ready before the backfill's delay: the case proves nothing");
            {
                const line = win.indexLineItem();
                if (!line || !line.visible || line.text.indexOf("%") < 0 || line.text.indexOf("prepared") < 0) return win.fail("hits show while the index fills, but not its line");
            }
            win.mark2 = Date.now();
            win.next("idxGone");
            return;
        case "idxGone": {
            // No key, no call: the re-read of a filling index moves the line and ends it.
            if (!win.results.indexReady) {
                if (Date.now() - win.mark2 > 60000) return win.fail("the index line never ended: the percent does not move without a key");
                return;
            }
            const line = win.indexLineItem();
            if (!line || line.visible) return win.fail("the index line stays when the index is ready");
            if (win.results.count !== 1) return win.fail("the hit is gone");
            win.deletedMessage = win.results.messageAt(0);
            // The hit is on screen; the message goes for everyone. From here on the test does
            // NOTHING: no typing, no search_due: the row must leave by the App's own wake-up.
            App.deleteMessage(win.deletedMessage, true);
            win.mark = Date.now();
            win.next("leaves");
            return;
        }
        case "leaves":
            if (win.results.count === 0) {
                const took = Date.now() - win.mark;
                if (took > 1500) return win.fail("the deleted message's hit left after " + took + " ms");
                if (!win.results.active) return win.fail("the search ended by itself");
                App.closeSearch();
                win.next("closed");
                return;
            }
            if (Date.now() - win.mark > 1500) return win.fail("the deleted message's hit is still listed after 1.5 s");
            return;
        case "closed":
            if (win.results.active || win.results.count !== 0) return win.later("the search closed");
            App.setPresence(true, true);
            win.receipts = win.heardReads();
            // The control: the ordinary open of an unread chat is heard marking it read.
            win.openConversation(win.controlChat);
            win.next("control");
            return;
        case "control":
            if (App.timeline.conversationId !== win.controlChat || App.timeline.loading) return win.later("the control chat");
            if (win.heardReads() <= win.receipts) return win.later("the control chat's read marks");
            win.receipts = win.heardReads();
            win.quiet = 0;
            win.next("controlDone");
            return;
        case "controlDone":
            // The marks of the control chat may come in several commands: the baseline is taken
            // once none has come for a second.
            if (win.heardReads() !== win.receipts) {
                win.receipts = win.heardReads();
                win.quiet = 0;
                return;
            }
            win.quiet += 1;
            if (win.quiet < 50) return;
            win.candIx = 0;
            win.askHits();
            win.next("hits");
            return;
        case "hits":
            // A chat with a few hits only has no OLD hit among them: the next candidate is
            // asked (the fixture's chats are not all as long).
            win.hitChat = win.cands[win.candIx];
            if (!win.results.indexReady || win.results.loading || win.results.chat !== win.hitChat) {
                win.later("the index and the hits of " + win.hitChat);
                if (win.waits % 50 === 49) win.askHits();
                return;
            }
            if (win.results.count < 3) {
                win.candIx += 1;
                if (win.candIx >= win.cands.length) return win.fail("no unread chat has three hits");
                win.askHits();
                return;
            }
            {
                // The oldest hit's ROW in the list's footer.
                const row = win.hitRow(win.hitCount() - 1);
                if (!row || win.hitCount() !== win.results.count) return win.later("the hit rows under the chats");
                if (row.conversationId !== win.hitChat) return win.fail("a hit of another chat in a chat's search");
                win.hitMessage = row.message;
                win.unreadBefore = win.unreadOf(win.hitChat);
                if (win.unreadBefore <= 0) return win.fail("the hit's chat has nothing unread");
                win.showRow(row);
            }
            win.next("clickHit");
            return;
        case "clickHit": {
            const row = win.findHit(win.hitMessage);
            if (!row) return win.fail("the hit's row went away");
            App.setPresence(true, true);
            win.jumpedTo = 0;
            win.click(row);
            win.mark = Date.now();
            win.next("hitOpen");
            return;
        }
        case "hitOpen": {
            if (App.timeline.conversationId !== win.hitChat || App.timeline.loading || win.jumpedTo === 0) return win.later("the hit's chat around the message");
            if (win.jumpedTo !== win.hitMessage) return win.fail("the view jumped to another message");
            if (!App.timeline.hasNewer) return win.fail("the hit is among the newest messages (" + win.results.count + " hits): the case proves nothing");
            if (win.chatPage === null || !win.chatShown) return win.fail("the chat page is not shown");
            const fetches = App.timelineFetches();
            if (fetches.split(" ")[0] !== "around") return win.fail("the hit's chat was read \"" + fetches + "\" first, not around the hit");
            win.quiet = 0;
            win.next("stays");
            return;
        }
        case "stays":
            // The chat is open and shown, with unread messages newer than the hit: for a
            // second and a half nothing may be heard saying it was read.
            if (win.heardReads() !== win.receipts) return win.fail("opening a hit marked the chat read (" + win.ask("/heard?name=SendReceipts") + " receipts, " + win.ask("/heard?name=SyncReadMarker") + " markers, " + win.receipts + " before)");
            if (win.unreadOf(win.hitChat) !== win.unreadBefore) return win.fail("the hit's chat's unread count moved: " + win.unreadOf(win.hitChat) + ", was " + win.unreadBefore);
            win.quiet += 1;
            if (win.quiet < 75) return;
            if (win.onlyB1) {
                Qt.exit(0);
                return;
            }
            App.closeSearch();
            win.next("inbox");
            return;
        // The words: one in messages but in no chat's name or preview (inbox or archive), one in
        // an inbox chat's name or preview.
        case "inbox":
            if (win.results.active || chats.count === 0) return win.later("the search closed");
            win.inboxRows = win.rowsNow();
            App.showArchive(true);
            win.next("archive");
            return;
        case "archive": {
            if (!App.archiveShown || chats.count === 0 || chats.count !== App.archivedCount) return win.later("the archived chats");
            win.archivedChat = chats.objectAt(0).conversationId;
            const all = win.inboxRows.concat(win.rowsNow());
            const words = ["coffee", "train", "music", "weekend", "plan", "maybe", "sounds", "good", "great", "idea", "photos", "yesterday", "running", "sorry", "dinner", "seven", "bring", "snacks", "trail", "weather", "looks", "fine", "tomorrow", "could", "meet", "park"];
            for (const w of words) {
                const inAny = all.some(r => r.indexOf(w) >= 0);
                const inInbox = win.inboxRows.some(r => r.indexOf(w) >= 0);
                if (!inAny && win.lone === "") win.lone = w;
                if (inInbox && win.both === "") win.both = w;
            }
            if (win.lone === "" || win.both === "") return win.fail("no word for the cases: lone " + win.lone + ", both " + win.both);
            App.showArchive(false);
            win.next("lone");
            return;
        }
        case "lone":
            if (App.archiveShown || chats.count !== win.inboxRows.length) return win.later("the inbox again");
            win.startTyping(win.lone);
            win.next("typeLone");
            return;
        case "typeLone":
            if (!win.typeOn()) return;
            if (!win.page().hitsShown || win.hitCount() < 2 || win.page().conversations.count !== 0) return win.later("the hits of \"" + win.lone + "\" under no chat");
            if (Date.now() - win.mark < 200) return win.fail("hits within the delay");
            win.next("names");
            return;
        case "names":
            for (let i = 0; i < win.hitCount(); i++) {
                const r = win.hitRow(i);
                const name = r ? r.Accessible.name : "";
                if (!r || name === "" || name.indexOf(r.snippetPlain) < 0 || name.indexOf(r.timeLabel) < 0 || r.Accessible.role !== Accessible.ListItem) return win.fail("hit row " + i + " has no accessible name of its line and time: \"" + name + "\"");
            }
            if (win.page().conversations.footerItem === null) return win.fail("the hits are not the list's footer");
            // L13: Enter with no matching chat does nothing, even with hits.
            win.key(Qt.Key_Return);
            win.quiet = 0;
            win.next("l13");
            return;
        case "l13":
            if (App.timeline.conversationId !== win.before) return win.fail("Enter with no matching chat opened a chat");
            if (win.field().text !== win.lone || !win.field().activeFocus) return win.fail("Enter changed the field");
            win.quiet += 1;
            if (win.quiet < 15) return;
            if (!win.page().hitsShown) return win.fail("the hits went away");
            win.key(Qt.Key_Down);
            win.next("downLone");
            return;
        case "downLone":
            if (!win.hitRow(0).activeFocus) return win.fail("Down from the field with no chat did not enter the hits");
            win.key(Qt.Key_Down);
            win.next("down2");
            return;
        case "down2":
            if (!win.hitRow(1).activeFocus) return win.fail("Down on a hit did not go to the next one");
            win.key(Qt.Key_Up);
            win.next("up1");
            return;
        case "up1":
            if (!win.hitRow(0).activeFocus) return win.fail("Up on a hit did not go to the one before");
            win.key(Qt.Key_Up);
            win.next("upField");
            return;
        case "upField":
            if (!win.field().activeFocus) return win.fail("Up from the first hit with no chat did not go back to the field");
            win.key(Qt.Key_Down);
            win.next("downAgain");
            return;
        case "downAgain":
            if (!win.hitRow(0).activeFocus) return win.fail("Down did not enter the hits again");
            // G1i: Escape on a hit gives the keyboard back to the field; text and hits stay.
            win.key(Qt.Key_Escape);
            win.quiet = 0;
            win.next("escHit");
            return;
        case "escHit":
            if (App.timeline.conversationId !== win.before || App.archiveShown) return win.fail("Escape opened something");
            if (win.field().text !== win.lone || !win.page().hitsShown) return win.fail("Escape on a hit took the search or its hits away");
            if (!win.field().activeFocus || win.hitRow(0).activeFocus) return win.fail("Escape on a hit did not give the keyboard to the field");
            win.quiet += 1;
            if (win.quiet < 10) return;
            // The next Escape clears the field, the search and the hits.
            win.key(Qt.Key_Escape);
            win.quiet = 0;
            win.next("escField");
            return;
        case "escField":
            if (win.field().text !== "" || win.results.active || win.page().hitsShown || win.page().chipShown) return win.fail("Escape in the field did not clear the search");
            if (App.timeline.conversationId !== win.before || App.archiveShown) return win.fail("Escape opened something");
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (!win.field().activeFocus) return win.fail("Escape took the keyboard from the field");
            win.startTyping(win.both);
            win.next("typeBoth");
            return;
        case "typeBoth":
            if (!win.typeOn()) return;
            if (!win.page().hitsShown || win.page().conversations.count === 0) return win.later("the chats and the hits of \"" + win.both + "\"");
            win.quiet += 1;
            if (win.quiet < 10) return;
            win.key(Qt.Key_Down);
            win.next("firstChat");
            return;
        case "firstChat": {
            const view = win.page().conversations;
            if (view.currentIndex !== 0 || !view.activeFocus) return win.fail("Down from the field did not reach the first chat");
            win.next("toLast");
            return;
        }
        case "toLast": {
            const view = win.page().conversations;
            if (view.currentIndex < view.count - 1) {
                win.key(Qt.Key_Down);
                return win.later("the last chat");
            }
            if (view.currentIndex !== view.count - 1) return win.fail("the list passed its last chat");
            win.key(Qt.Key_Down);
            win.next("intoHits");
            return;
        }
        case "intoHits": {
            const view = win.page().conversations;
            if (!win.hitRow(0).activeFocus) return win.fail("Down past the last chat did not enter the hits");
            if (view.currentIndex !== -1) return win.fail("a chat row stays current while a hit has the keyboard");
            win.key(Qt.Key_Up);
            win.next("backToChats");
            return;
        }
        case "backToChats": {
            const view = win.page().conversations;
            if (view.currentIndex !== view.count - 1 || !view.activeFocus || win.hitRow(0).activeFocus) return win.fail("Up from the first hit did not come back to the last chat");
            if (App.timeline.conversationId !== win.before) return win.fail("the keys opened a chat");
            // Enter in the field is still the quick switch: the first chat that matches opens
            // (another chat is open first, so the switch is seen).
            win.before = App.conversationAt(0);
            if (win.before === 0) return win.fail("no first chat");
            if (win.before === App.timeline.conversationId) win.openConversation(win.before === win.freeChat ? win.controlChat : win.freeChat);
            win.next("switchKey");
            return;
        }
        case "switchKey":
            if (App.timeline.conversationId === win.before || App.timeline.loading) return win.later("another chat open");
            if (win.field().text !== win.both) return win.fail("the field lost its text");
            win.field().forceActiveFocus();
            win.key(Qt.Key_Return);
            win.next("switched");
            return;
        case "switched":
            if (App.timeline.conversationId !== win.before || win.field().text !== "") return win.later("the quick switch to the first chat");
            if (win.results.active) return win.fail("the emptied field kept the search");
            if (!App.composer.send(win.archivedChat, "see the " + win.unique + " here")) return win.fail("the send into the archived chat was refused");
            win.research = 0;
            win.startTyping(win.unique);
            win.next("typeUnique");
            return;
        // M4: a hit in an archived chat opens it at the hit and leaves the list as it is.
        case "typeUnique":
            if (!win.typeOn()) return;
            if (!win.page().hitsShown) {
                // (The index follows the send by a moment: asked again once a second.)
                win.research += 1;
                if (win.research % 50 === 0) App.searchMessages(win.unique, 0);
                return win.later("the hit in the archived chat");
            }
            {
                const row = win.hitRow(0);
                if (!row || row.conversationId !== win.archivedChat) return win.fail("the unique word's hit is not the archived chat's");
                win.hitMessage = row.message;
                win.showRow(row);
            }
            win.next("clickArchived");
            return;
        case "clickArchived": {
            const row = win.findHit(win.hitMessage);
            if (!row) return win.fail("the archived chat's hit went away");
            win.jumpedTo = 0;
            win.click(row);
            win.next("archivedOpen");
            return;
        }
        case "archivedOpen": {
            if (App.timeline.conversationId !== win.archivedChat || App.timeline.loading) return win.later("the archived chat open");
            const fetches = App.timelineFetches();
            if (fetches.split(" ")[0] !== "around") return win.fail("the archived chat was read \"" + fetches + "\" first");
            win.quiet = 0;
            win.next("listStays");
            return;
        }
        case "listStays":
            if (App.archiveShown) return win.fail("a hit in an archived chat brought the archive view");
            if (win.field().text !== win.unique) return win.fail("a hit's open emptied the search");
            if (!win.results.active || !win.page().hitsShown || win.findHit(win.hitMessage) === null) return win.fail("a hit's open took the hits away");
            if (App.timeline.conversationId !== win.archivedChat) return win.fail("another chat opened");
            win.quiet += 1;
            if (win.quiet < 25) return;
            win.startTyping(win.both);
            win.next("chipPick");
            return;
        // G1i: search in one chat. A word found in two chats; the first hit's chat is opened.
        case "chipPick":
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== 0 || win.results.loading || win.hitCount() < 2 || win.hitCount() !== win.results.count) return win.later("the hits of \"" + win.both + "\" in every chat");
            if (win.distinctChats() < 2) return win.fail("the word is found in one chat only: the case proves nothing");
            win.chipChat = win.hitRow(0).conversationId;
            for (let i = 0; i < win.hitCount(); i++) {
                if (win.hitRow(i).conversationId !== win.chipChat) { win.otherChat = win.hitRow(i).conversationId; break; }
            }
            win.openConversation(win.chipChat);
            win.next("chipOpened");
            return;
        case "chipOpened":
            if (App.timeline.conversationId !== win.chipChat || App.timeline.loading || !App.timeline.chatSearchable) return win.later("the chat to search in");
            win.field().text = "";
            win.contentItem.forceActiveFocus();
            win.ctrlShiftF();
            win.next("chipShown");
            return;
        case "chipShown": {
            if (!win.page().chipShown) return win.later("the chip");
            if (win.page().searchChat !== win.chipChat) return win.fail("the chip names another chat");
            if (!win.field().activeFocus) return win.fail("Ctrl+Shift+F did not give the keyboard to the field");
            const chip = win.page().searchChip;
            const label = win.named(chip, "search-chip-label");
            const x = win.named(chip, "search-chip-x");
            if (!chip.visible || !label || !x) return win.fail("the chip is not complete");
            if (label.text !== "In: " + App.timeline.title || label.textFormat !== Text.PlainText || label.elide !== Text.ElideRight) return win.fail("the chip's label is \"" + label.text + "\"");
            if (chip.Accessible.name.indexOf(App.timeline.title) < 0 || x.Accessible.name === "" || x.Accessible.name !== x.text) return win.fail("the chip has no accessible name");
            if (chip.x + chip.width > win.field().x) return win.fail("the chip overlaps the field: chip " + chip.x + "+" + chip.width + ", field at " + win.field().x);
            win.startTyping(win.both);
            win.next("chipTyped");
            return;
        }
        case "chipTyped":
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== win.chipChat || win.results.loading || win.results.count < 1 || win.hitCount() !== win.results.count) return win.later("the hits of the open chat");
            for (let i = 0; i < win.hitCount(); i++) {
                if (win.hitRow(i).conversationId !== win.chipChat) return win.fail("a hit of another chat in the one-chat search");
            }
            if (win.page().conversations.count !== 0 || win.page().conversations.model !== null) return win.fail("a chat row shows while the chip stands");
            win.key(Qt.Key_Return);
            win.quiet = 0;
            win.next("chipEnter");
            return;
        case "chipEnter":
            if (App.timeline.conversationId !== win.chipChat || !win.page().chipShown) return win.fail("Enter opened a chat or ended the search");
            win.quiet += 1;
            if (win.quiet < 10) return;
            win.click(win.named(win.page().searchChip, "search-chip-x"));
            win.next("chipX");
            return;
        case "chipX":
            if (win.page().chipShown) return win.later("the chip to go");
            if (win.field().text !== win.both) return win.fail("the X changed the text");
            if (win.results.chat !== 0 || win.results.loading || win.distinctChats() < 2) return win.later("the hits of every chat after the X");
            if (win.page().conversations.count === 0) return win.fail("the chats that match do not show after the X");
            win.contentItem.forceActiveFocus();
            win.ctrlShiftF();
            win.next("chipAgain");
            return;
        case "chipAgain":
            if (!win.page().chipShown || !win.field().activeFocus) return win.later("the chip again");
            // The text stays: it searches the chat alone now.
            if (win.field().text !== win.both) return win.fail("the chip took the text");
            win.field().text = "";
            win.next("chipEmpty");
            return;
        case "chipEmpty":
            if (win.results.active) return win.later("the search to end");
            if (!win.page().chipShown) return win.fail("an emptied field took the chip");
            win.key(Qt.Key_Backspace);
            win.next("chipBackspace");
            return;
        case "chipBackspace":
            if (win.page().chipShown) return win.later("Backspace to take the chip off");
            if (win.results.active) return win.fail("a search shows with no text");
            win.ctrlShiftF();
            win.next("chipEnds");
            return;
        // Another chat opens: the one-chat search ends (chip, field, search).
        case "chipEnds":
            if (!win.page().chipShown) return win.later("the chip");
            win.startTyping(win.both);
            win.next("chipEnds2");
            return;
        case "chipEnds2":
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== win.chipChat || win.results.loading || win.results.count < 1) return win.later("the hits of the open chat");
            win.pickedChat(win.otherChat);
            win.next("chipEnded");
            return;
        case "chipEnded":
            if (App.timeline.conversationId !== win.otherChat || App.timeline.loading) return win.later("the other chat");
            if (win.page().chipShown || win.field().text !== "" || win.results.active) return win.fail("another chat opened and the one-chat search stands");
            win.openConversation(win.chipChat);
            win.next("chipHit");
            return;
        // A hit of the chat keeps the chip and the hits.
        case "chipHit":
            if (App.timeline.conversationId !== win.chipChat || App.timeline.loading) return win.later("the chat");
            win.contentItem.forceActiveFocus();
            win.ctrlShiftF();
            win.next("chipHit2");
            return;
        case "chipHit2":
            if (!win.page().chipShown) return win.later("the chip");
            win.startTyping(win.both);
            win.next("chipHit3");
            return;
        case "chipHit3": {
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== win.chipChat || win.results.loading || win.results.count < 1 || win.hitCount() !== win.results.count) return win.later("the hits of the open chat");
            const row = win.hitRow(0);
            win.showRow(row);
            win.click(row);
            win.quiet = 0;
            win.next("chipHit4");
            return;
        }
        case "chipHit4":
            if (App.timeline.conversationId !== win.chipChat || !win.page().chipShown || win.field().text !== win.both || !win.results.active || win.hitCount() < 1) return win.fail("a hit of the chat ended its one-chat search");
            win.quiet += 1;
            if (win.quiet < 25) return;
            // Escape with text and a chip clears both and closes the search.
            win.field().forceActiveFocus();
            win.key(Qt.Key_Escape);
            win.next("chipEsc");
            return;
        case "chipEsc": {
            if (win.page().chipShown || win.field().text !== "" || win.results.active || win.page().hitsShown) return win.fail("Escape did not end the one-chat search");
            // The overflow menu's entry does the same as the key.
            const act = win.menuAction("Search in Chat");
            if (!act || !act.visible) return win.fail("the chat's menu has no \"Search in Chat\"");
            win.contentItem.forceActiveFocus();
            act.trigger();
            win.next("chipMenu");
            return;
        }
        case "chipMenu":
            if (!win.page().chipShown) return win.later("the chip from the menu");
            if (!win.field().activeFocus || win.page().searchChat !== win.chipChat) return win.fail("the menu entry did not give the field the keyboard with the chip");
            win.startTyping(win.both);
            win.next("chipMenu2");
            return;
        case "chipMenu2":
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== win.chipChat || win.results.loading || win.results.count < 1) return win.later("the hits of the open chat");
            for (let i = 0; i < win.hitCount(); i++) {
                if (win.hitRow(i).conversationId !== win.chipChat) return win.fail("the menu's search shows a hit of another chat");
            }
            // An empty field with the chip: Escape ends it.
            win.field().text = "";
            win.key(Qt.Key_Escape);
            win.next("chipMenu3");
            return;
        case "chipMenu3": {
            if (win.page().chipShown || win.results.active) return win.fail("Escape in an empty field did not take the chip off");
            // A protected chat offers no search.
            let prot = 0;
            for (let i = 0; i < chats.count; i++) {
                const c = chats.objectAt(i);
                if (c && c.preview === "Protected chat") prot = c.conversationId;
            }
            if (prot === 0) return win.fail("the fixture has no protected chat");
            win.protChat = prot;
            win.openConversation(prot);
            win.next("prot");
            return;
        }
        case "prot": {
            if (App.timeline.conversationId !== win.protChat || App.timeline.loading) return win.later("the protected chat");
            if (App.timeline.chatSearchable || !App.timeline.chatProtected) return win.fail("a protected chat is searchable");
            const act = win.menuAction("Search in Chat");
            if (act && act.visible) return win.fail("a protected chat offers \"Search in Chat\"");
            win.contentItem.forceActiveFocus();
            win.ctrlShiftF();
            win.quiet = 0;
            win.next("protKey");
            return;
        }
        case "protKey":
            if (win.page().chipShown || win.results.active || win.field().text !== "") return win.fail("Ctrl+Shift+F in a protected chat started a search");
            win.quiet += 1;
            if (win.quiet < 25) return;
            win.next("r3start");
            return;
        // R3 (F1, F6, F7, F8): the word found in chats AND messages, typed key by key.
        case "r3start":
            win.startTyping(win.both);
            win.next("r3typed");
            return;
        case "r3typed": {
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== 0 || win.results.loading || !win.results.answered || win.hitCount() < 2 || win.hitCount() !== win.results.count || win.page().conversations.count < 1) return win.later("the chats and the hits of \"" + win.both + "\"");
            // F1: every hit row shows a bold word, at most 16 characters (and the ellipsis) into its line.
            for (let i = 0; i < win.hitCount(); i++) {
                const html = win.hitRow(i).snippetHtml;
                const at = html.indexOf("<b>");
                if (at < 0) return win.fail("hit row " + i + " shows no bold word: " + html);
                const lead = html.slice(0, at).replace(/&[a-z]+;/g, "#").length;
                if (lead > 17) return win.fail("hit row " + i + " has " + lead + " characters before its match: " + html);
                if (win.hitRow(i).oneChat) return win.fail("a row outside the chip is a one-chat row");
            }
            // F8 (C12): the section is a list named "Messages", announced once per answered text.
            const section = win.named(win.page().conversations.footerItem, "search-hits") || win.page().conversations.footerItem;
            if (!section || section.Accessible.role !== Accessible.List || section.Accessible.name !== "Messages") return win.fail("the hits section has no accessible name");
            if (win.page().searchAnnounced.indexOf("found") < 0) return win.fail("no announcement of the answer: \"" + win.page().searchAnnounced + "\"");
            if (win.page().conversations.headerItem === null) return win.fail("no \"Chats\" header above the chats while \"Messages\" shows");
            // F7: Down from the field to the first chat; End: the last hit; Home: the first chat;
            // Page Down moves on by rows.
            win.key(Qt.Key_Down);
            win.next("r3keys");
            return;
        }
        case "r3keys": {
            const list = win.page().conversations;
            if (!list.activeFocus || list.currentIndex !== 0) return win.later("the first chat is current");
            const loaded = win.hitCount();
            win.key(Qt.Key_End);
            // (The last hit loaded when the key came: the end of the list then asks for more.)
            if (win.page().focusedHit() < loaded - 1) return win.fail("End did not reach the last hit: " + win.page().focusedHit() + " of " + loaded);
            win.key(Qt.Key_Home);
            if (list.currentIndex !== 0 || win.page().focusedHit() !== -1) return win.fail("Home did not reach the first chat");
            win.key(Qt.Key_PageDown);
            if (win.page().currentRow() <= 0) return win.fail("Page Down did not move");
            win.key(Qt.Key_PageUp);
            win.key(Qt.Key_End);
            if (win.page().focusedHit() < loaded - 1) return win.fail("End again");
            // F8 (C14): the focused hit is deleted: the keyboard stays in the list's hits
            // (the previous one, as it was the last) or goes to the field.
            win.deletedMessage = win.hitRow(win.page().focusedHit()).message;
            win.before = win.hitCount();
            // (For me: the hit's message is a received one.)
            App.deleteMessage(win.deletedMessage, false);
            win.mark = Date.now();
            win.next("r3gone");
            return;
        }
        case "r3gone":
            // (By its message: the end of the list may have loaded a page more meanwhile.)
            if (win.findHit(win.deletedMessage) !== null) {
                if (Date.now() - win.mark > 4000) return win.fail("the deleted hit stays listed");
                return;
            }
            if (win.page().focusedHit() < 0 && !win.field().activeFocus) return win.later("the keyboard after the focused hit went");
            win.next("r3chip");
            return;
        // F6: a refinement keeps the section; F8 (C9): the chip keeps its title and the search
        // goes back to every chat when its chat is deleted.
        case "r3chip":
            win.page().endSearch();
            win.contentItem.forceActiveFocus();
            win.openConversation(win.chipChat);
            win.next("r3chip2");
            return;
        case "r3chip2":
            if (App.timeline.conversationId !== win.chipChat || App.timeline.loading) return win.later("the chat of the chip");
            win.ctrlShiftF();
            win.next("r3chip3");
            return;
        case "r3chip3": {
            if (!win.page().chipShown) return win.later("the chip");
            const x = win.named(win.page().searchChip, "search-chip-x");
            if (x.focusPolicy !== Qt.TabFocus) return win.fail("the chip's X is not a Tab stop");
            const act = win.menuAction("Search in Chat");
            if (!act || act.tooltip.indexOf("Ctrl+Shift+F") < 0) return win.fail("the entry does not show its key");
            if (win.page().chipTitle !== App.timeline.title) return win.fail("the chip does not hold its title");
            win.startTyping(win.both);
            win.next("r3chip4");
            return;
        }
        case "r3chip4": {
            if (!win.typeOn()) return;
            if (!win.results.active || win.results.chat !== win.chipChat || win.results.loading || !win.results.answered || win.hitCount() < 1 || win.hitCount() !== win.results.count) return win.later("the hits of the one chat");
            // F4: the sender is the title; no "Name:" before the line.
            for (let i = 0; i < win.hitCount(); i++) {
                const r = win.hitRow(i);
                if (!r.oneChat || r.senderName === "" || r.shownTitle !== r.senderName) return win.fail("hit row " + i + " does not show its sender as the title");
                if (r.Accessible.name.indexOf(r.senderName) !== 0) return win.fail("hit row " + i + " is not named by its sender");
            }
            // F6: one more letter: the section stays until the new text is answered.
            win.ev.keyClickChar("x", Qt.NoModifier, -1);
            win.next("r3more");
            return;
        }
        case "r3more": {
            const asked = win.field().text.trim();
            const answered = win.results.answered && win.page().askedText === asked && !win.results.loading;
            if (!answered) {
                if (win.results.count > 0 && !win.page().hitsShown) return win.fail("the Messages section went while the new text is asked");
                return win.later("the answer of the refinement");
            }
            win.chipTitleBefore = win.page().chipTitle;
            App.deleteConversation();
            win.mark = Date.now();
            win.next("r3deleted");
            return;
        }
        case "r3deleted":
            if (win.page().chipShown) {
                if (Date.now() - win.mark > 6000) return win.fail("the chip stays when its chat is gone");
                return;
            }
            if (win.results.chat !== 0 || win.field().text.trim() === "") return win.fail("the search did not go back to every chat with its text");
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 240000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn main() {
    support::answer_list("search_ui");
    // A private config home: an empty `kdeglobals`, so nothing of the user's desktop is read.
    let home = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("search_ui-{}", std::process::id()));
    let config = home.join("config");
    std::fs::create_dir_all(&config).expect("a private config home");
    std::fs::write(config.join("kdeglobals"), "").expect("an empty kdeglobals");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &config);
        std::env::set_var("QT_QPA_PLATFORM", "offscreen");
        std::env::set_var("QT_FORCE_STDERR_LOGGING", "1");
        // G1i: the index fills for a while (the backfill starts 10 s after the start), so its
        // line can be seen without racing the default.
        std::env::set_var("KOURIER_DEV_SEARCH_BACKFILL_MS", "10000");
    }
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            // More messages than one page, so an old hit is not in the newest page.
            messages_per_conversation: 300,
            // One plain chat is protected: it offers no search in it.
            protected_chat: true,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    // What the fake network heard: `GET /heard?name=<command>` answers the count.
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
            let body = target
                .strip_prefix("/heard?name=")
                .map(|name| {
                    heard
                        .fixture_commands()
                        .iter()
                        .filter(|c| **c == name)
                        .count()
                        .to_string()
                })
                .unwrap_or_default();
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let only_b1 = std::env::var_os("KOURIER_SEARCH_ONLY_B1").is_some();
    let script = SCRIPT
        .replace("PORT", &port.to_string())
        .replace("ONLYB1", if only_b1 { "true" } else { "false" });
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/search_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    let _ = std::fs::remove_dir_all(&home);
    if code != 0 {
        println!(
            "test the message search in the window ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    println!("test the message search in the window ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
