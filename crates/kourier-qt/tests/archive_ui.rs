//! The archive of the conversation list in the real main window, over the real core on the
//! fake network (offscreen, `harness = false`; each case in a process of its own, with real
//! key events from Qt's `TestEvent`). Archived chats can be opened (A1): the list's last row
//! is the way in, the archive view is the same page.
//!
//! - `entry`: the fixture has one archived chat. The list's footer says "Archived Chats (1)"
//!   (the accessible name says the number too), is the last row of the list, and is gone while
//!   a search is typed (a hidden footer would keep an empty band at the list's end); it is
//!   back once the search is emptied.
//! - `entry-none`: a fixture without an archived chat has no footer at all.
//! - `open`: a click on the footer opens the archive view: the title says "Archived Chats", the
//!   list has the one archived chat (and only it: no inbox row), the line that says what the
//!   archive is shows, the search field does not, and the Back arrow stands before the title.
//! - `back`: the Back arrow, Esc with the list focused, Alt+Left with the list focused and Esc
//!   on the arrow itself each return to "All Chats" and the inbox.
//! - `account`: in the archive view the account is picked (the drawer's `selectAccount`): the
//!   title stays "Archived Chats", also while list reads keep coming (the rename refresh of an
//!   account's name is the other writer of the title). Picking "all chats" keeps it too.
//!
//! - `search`: a search lists the archived chat after every inbox match; picking an inbox match
//!   leaves the list on the chats; picking the archived one (a click) opens it AND shows the
//!   archive view with the chat highlighted and the search emptied (Signal Desktop's rule).
//!   `quick`: the same through the quick switch (type its title, Enter).
//! - `hidden` (blended chats, H3): a second account is linked and the fixture account hidden;
//!   a search finds the hidden account's archived chat, and picking it opens it while the list
//!   stays as chosen (the archive view, which lists the shown accounts' archived chats, does
//!   not open).
//! - `saved` (blended chats, M13's apply path): the same two accounts, the fixture account
//!   hidden; then a new window and App over the same core and preferences: no chat of the
//!   saved hidden account is ever listed, not even before the accounts answered.
//! - `quick-inbox`: the quick switch of an inbox chat opens that chat and no other (the Return
//!   the search field passes on must not open the list's current row as well) and leaves the
//!   list on the chats.
//! - `comeback`: the open chat is in the archive; the phone's side moves it to the inbox (a
//!   new message brought it back): the list goes back to the chats, the chat stays open.
//! - `phone`: the open chat is in the inbox; the phone archives it: the chat closes (the chat
//!   column is empty), the list stays on the chats and the entry counts it. `phone-narrow`: in
//!   a narrow window the chat page over the list goes away: back at the list.
//! - `ctrlk`: Ctrl+K in the archive view returns to the chats and puts the keyboard in the
//!   search field (the archive has none).
//! - `tab`: the way into the archive is a stop of the keyboard after the list: Tab from the list
//!   reaches it, Tab again the timeline, Shift+Tab from the timeline lands on it again, Shift+Tab
//!   once more on the list; Return on it opens the archive, where Shift+Tab from the list lands
//!   on the Back arrow (the archive has no search field to go back to).
//! - `allarchived`: every chat is archived (the pinned ones unpinned first): the list has no row
//!   but the way into the archive, which says the count, shows inside the list, and "No
//!   conversations" does not.
//! - `empty`: the archive's only chat comes back to the chats while the archive view shows: the
//!   view says "No archived chats" (and goes on showing), and the Back arrow leads to the chats
//!   with no entry.
//!
//! The phone's side is the real command path: the script asks a listener of this test (a local
//! port, as `retain_ui` releases its derive) to send `ChangeConversation { SetArchived }`
//! (after `SetPinned { false }` when asked: a pinned chat stays in the chats) through the core
//! to the fake network, which confirms it with an upsert as a Storage sync would.
//!
//! `KOURIER_ARCHIVE_ONLY=a,b` picks cases; `KOURIER_ARCHIVE_SHOTS=<dir>` writes a picture of the
//! entry (`entry.png`, the list scrolled to its end), of the archive view (`open.png`), of the
//! archive view that an archived chat picked from a search opens, the chat open and
//! highlighted (`picked.png`), of the list with every chat archived (`allarchived.png`) and of
//! the archive view with nothing left in it (`empty.png`) for a look (not a test). No case may
//! print a script error.

mod support;

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::process::Command;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::UiCommand;
use kourier_app::{App, Arrivals, FixtureOptions, FixtureSecondLink, Launch, StartOptions};
use kourier_model::{ConversationChange, ConversationId};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

// The app's own main window is the root (wide: the list and the chat side by side).
MainWindow {
    id: win

    width: win.mode === "phone-narrow" ? 480 : 1100
    height: 760
    visible: true

    property string mode: "CASE"
    property string step: "start"
    property int waits: 0
    // How long a step waits for what it expects before it fails (ticks of 40 ms): long
    // until the script has moved something, short after it.
    property int limit: 1500
    property real archivedId: 0
    property string archivedTitle: ""
    property real pickedId: 0
    property var ev: null
    property bool busy: false
    property int watched: 0
    property real account: 0
    // Whether the focus checks of the case ran, and how many chats were open before the key.
    property bool spaced: false
    property int openedBefore: 0
    // Changes the chat list's model made since the counter was last zeroed (a row inserted, a reset).
    property int reads: 0

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > win.limit) {
            fail("never happened: " + what);
        }
    }

    // The phone archives (`archived`) or unarchives the chat `conv`: the test's listener sends
    // the command through the core to the fake network. What follows is waited for briefly.
    function phone(conv: real, archived: bool, unpin: bool): void {
        win.limit = 250;
        win.waits = 0;
        const request = new XMLHttpRequest();
        request.open("GET", "http://127.0.0.1:LISTEN_PORT/archive?conv=" + conv + "&to=" + (archived ? 1 : 0) + (unpin ? "&unpin=1" : ""));
        request.send();
    }

    // The row of the list that is the chat `id` (-1 when it is not listed).
    function rowOf(id: real): int {
        for (let i = 0; i < convs.count; ++i) {
            if (convs.objectAt(i).conversationId === id) {
                return i;
            }
        }
        return -1;
    }

    // Clicks the delegate of row `i` (false while the list has not made it yet).
    function clickRow(i: int): bool {
        const view = win.list().conversations;
        view.positionViewAtIndex(i, ListView.Contain);
        const row = view.itemAtIndex(i);
        if (row === null) {
            return false;
        }
        row.click();
        return true;
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
    }

    function key(k: int, modifiers: int): void {
        if (!win.ev.keyClick(k, modifiers, -1)) {
            win.fail("the key event was not delivered");
        }
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

    function list(): Item {
        return win.pageStack.get(0);
    }

    // `KOURIER_ARCHIVE_SHOTS=<dir>`: a picture for a look (not a test).
    function shot(name: string): void {
        if ("SHOTS_DIR" !== "") {
            App.saveScreenshot(win, "SHOTS_DIR/" + name + ".png");
        }
    }

    // The chats the window opened, in order (a failure says which).
    property var opened: []
    Connections {
        target: App.timeline
        function onConversationIdChanged(): void {
            win.opened.push(App.timeline.conversationId);
        }
    }

    Connections {
        target: App.conversations
        function onRowsInserted(): void {
            win.reads += 1;
        }
        function onModelReset(): void {
            win.reads += 1;
        }
    }

    // The list keeps the keyboard and no row is current or focused (the focus frame would
    // sit on a row nobody chose, and Space would open it); "" when that holds.
    function noRowFocused(): string {
        const view = win.list().conversations;
        if (view.currentIndex !== -1) return "currentIndex " + view.currentIndex;
        const row = win.find(view.contentItem, i => i.activeFocus && i.conversationId !== undefined);
        if (row !== null) return "the row of chat " + row.conversationId + " has the focus";
        if (!view.activeFocus) return "the list has no keyboard";
        return "";
    }

    // Space with no row chosen opens nothing. Called every tick until it passes; "" while it
    // waits, a message when a chat opened.
    function spaceOpensNothing(): string {
        if (!win.spaced) {
            win.spaced = true;
            win.openedBefore = win.opened.length;
            win.waits = 0;
            win.key(Qt.Key_Space, Qt.NoModifier);
            return "wait";
        }
        win.waits += 1;
        if (win.opened.length !== win.openedBefore) return "Space opened chat " + win.opened[win.opened.length - 1];
        return win.waits < 12 ? "wait" : "";
    }

    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {
            required property real conversationId
            required property real accountId
            required property string title
            required property bool archived
            required property bool pinned
        }
        // Every account a listed row ever belonged to (the `saved` case).
        onObjectAdded: (index, object) => {
            if (win.seenAccounts.indexOf(object.accountId) < 0) win.seenAccounts.push(object.accountId);
        }
    }
    property var seenAccounts: []

    Instantiator {
        id: accounts
        model: App.accounts
        delegate: QtObject {
            required property real accountId
        }
    }

    function inboxRows(): int {
        let n = 0;
        for (let i = 0; i < convs.count; ++i) {
            if (!convs.objectAt(i).archived) {
                ++n;
            }
        }
        return n;
    }

    // The list has what the fixture delivers (one chat of it archived, or none).
    function loaded(): bool {
        return win.started && App.hasAccounts && convs.count >= (win.mode === "entry-none" ? 3 : 15) && App.archivedCount === (win.mode === "entry-none" ? 0 : 1);
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

    // The archive view is shown: the title, the one archived chat, the line, no search field.
    function archiveShown(): string {
        const page = win.list();
        if (!App.archiveShown) return "App.archiveShown";
        if (App.currentTitle !== "Archived Chats") return "the title: " + App.currentTitle;
        if (convs.count !== 1 || !convs.objectAt(0).archived) return "the rows: " + convs.count;
        if (App.archivedCount !== 1) return "the count in the archive view: " + App.archivedCount;
        if (page.searchField.visible) return "the search field shows in the archive view";
        if (page.archiveEntry !== null || page.conversations.footerItem !== null) return "the entry shows in the archive view";
        const help = win.find(page, i => i.objectName === "archive-help");
        if (!help || !help.visible || help.text.indexOf("come back to your chats") < 0) return "the line that says what the archive is";
        if (page.archiveBack === null || !page.archiveBack.visible || page.archiveBack.Accessible.name !== "Back to Chats") return "the Back arrow";
        return "";
    }

    // The chat list is shown: the title, the inbox without the archived chat, the entry.
    function chatsShown(): string {
        return win.chatsShownWith(true);
    }

    function chatsShownWith(entry: bool): string {
        const page = win.list();
        if (App.archiveShown) return "App.archiveShown";
        if (App.currentTitle !== "All Chats") return "the title: " + App.currentTitle;
        if (win.inboxRows() !== convs.count || convs.count < 15) return "the rows: " + win.inboxRows() + " of " + convs.count;
        if (!page.searchField.visible) return "the search field is gone";
        if (entry && page.archiveEntry === null) return "the entry";
        if (!entry && page.archiveEntry !== null) return "the entry shows with no archived chat";
        return "";
    }

    function tick(): void {
        if (win.mode === "saved-start") {
            // The second start of the `saved` case: the saved hidden account's chats are never
            // listed, not even before the accounts answered (the choice is applied before the
            // first list is built).
            if (!win.started || App.accountActions.length !== 2) return win.later("both accounts");
            const hidden = App.accountActions.find(a => !a.shown);
            if (!hidden || App.shownCount !== 1) return win.later("the saved choice: one account shown");
            if (win.seenAccounts.indexOf(hidden.account) >= 0) return win.fail("the hidden account's chats were listed at the start");
            win.waits += 1;
            if (win.waits < 25) return;
            if (convs.count !== 0) return win.fail("rows listed: " + convs.count);
            Qt.exit(0);
            return;
        }
        switch (win.step) {
        case "start": {
            if (!win.loaded()) return win.later("the list: " + convs.count + " rows, " + App.archivedCount + " archived");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "archive_ui_events.qml");
            const page = win.list();
            if (win.mode === "entry-none") {
                // No archived chat: no row at the list's end, nothing for the keyboard to stop at.
                if (page.archiveEntry !== null || page.conversations.footerItem !== null || page.entryShown) return win.fail("an entry without an archived chat");
                Qt.exit(0);
                return;
            }
            const first = {
                "entry": "entry",
                "account": "account open",
                "search": "search type",
                "hidden": "hidden link",
                "saved": "hidden link",
                "quick": "search type",
                "quick-inbox": "qi type",
                "comeback": "comeback open",
                "phone": "phone open",
                "phone-narrow": "phone open",
                "ctrlk": "ctrlk open",
                "tab": "tab open",
                "allarchived": "all archive",
                "empty": "empty open"
            };
            win.next(first[win.mode] ?? "click");
            return;
        }
        case "entry": {
            const page = win.list();
            const entry = page.archiveEntry;
            if (entry === null || page.conversations.footerItem !== entry) return win.later("the entry as the footer of the list");
            if (!entry.visible || entry.height <= 0 || entry.width !== page.conversations.width) return win.fail("the entry's size: " + entry.width + "x" + entry.height);
            const label = win.find(entry, i => i.text !== undefined && i.text.indexOf("Archived Chats") === 0);
            if (!label || label.text !== "Archived Chats (1)") return win.fail("the entry's words: " + (label ? label.text : "none"));
            if (entry.Accessible.name !== "Archived Chats, 1 chat" || entry.Accessible.role !== Accessible.Button) return win.fail("the entry's accessible name: " + entry.Accessible.name);
            // The last row: below every chat, at the end of the content.
            const at = entry.mapToItem(page.conversations.contentItem, 0, 0).y;
            if (Math.abs(at + entry.height - page.conversations.contentHeight) > 2) return win.fail("the entry is not the last row: y " + at + ", height " + entry.height + ", content " + page.conversations.contentHeight);
            page.conversations.positionViewAtEnd();
            win.next("entry shot");
            return;
        }
        case "entry shot": {
            // (A moment for the picture to be final.)
            win.waits += 1;
            if (win.waits < 15) return;
            win.shot("entry");
            // A search looks in both views: the entry goes with no band left behind.
            win.list().searchField.text = "a";
            win.next("entry searched");
            return;
        }
        case "entry searched": {
            const page = win.list();
            if (page.archiveEntry !== null || page.conversations.footerItem !== null) return win.later("the entry goes while a search is typed");
            page.searchField.text = "";
            win.next("entry back");
            return;
        }
        case "entry back": {
            const page = win.list();
            if (page.archiveEntry === null) return win.later("the entry is back once the search is empty");
            Qt.exit(0);
            return;
        }
        case "click": {
            const page = win.list();
            const entry = page.archiveEntry;
            if (entry === null) return win.later("the entry");
            if (win.chatsShown() !== "") return win.fail("before the click: " + win.chatsShown());
            entry.click();
            win.next("opened");
            return;
        }
        case "opened": {
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view: " + bad);
            if (win.mode === "open") {
                const focus = win.noRowFocused();
                if (focus !== "") return win.fail("after opening the archive: " + focus);
                const space = win.spaceOpensNothing();
                if (space === "wait") return;
                if (space !== "") return win.fail(space);
                win.waits += 1;
                if (win.waits < 15 && "SHOTS_DIR" !== "") return;
                win.shot("open");
                Qt.exit(0);
                return;
            }
            // The arrow goes back.
            win.list().archiveBack.click();
            win.next("back arrow");
            return;
        }
        case "back arrow":
        case "back esc":
        case "back alt":
        case "back arrow esc": {
            if (win.chatsShown() !== "") return win.later("the chats again (" + win.step + "): " + win.chatsShown());
            if (win.mode === "back" && win.step === "back arrow") {
                const focus = win.noRowFocused();
                if (focus !== "") return win.fail("after leaving the archive: " + focus);
                const space = win.spaceOpensNothing();
                if (space === "wait") return;
                if (space !== "") return win.fail(space);
            }
            if (win.step === "back arrow") {
                win.list().openArchive();
                win.next("reopened esc");
            } else if (win.step === "back esc") {
                win.list().openArchive();
                win.next("reopened alt");
            } else if (win.step === "back alt") {
                win.list().openArchive();
                win.next("reopened arrow esc");
            } else {
                Qt.exit(0);
            }
            return;
        }
        case "reopened esc":
        case "reopened alt":
        case "reopened arrow esc": {
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view again (" + win.step + "): " + bad);
            const page = win.list();
            if (win.step === "reopened arrow esc") {
                // Esc on the Back arrow itself.
                page.archiveBack.forceActiveFocus();
                if (!page.archiveBack.activeFocus) return win.fail("the Back arrow takes no keyboard");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("back arrow esc");
                return;
            }
            // Esc, or Alt+Left, with the list focused.
            if (!page.conversations.activeFocus) return win.fail("opening the archive leaves the keyboard outside the list");
            if (win.step === "reopened esc") {
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("back esc");
            } else {
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("back alt");
            }
            return;
        }
        case "account open": {
            win.list().openArchive();
            win.next("account opened");
            return;
        }
        case "account opened": {
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view: " + bad);
            if (accounts.count < 1) return win.later("the accounts");
            win.account = accounts.objectAt(0).accountId;
            App.selectAccount(win.account);
            win.watched = 0;
            win.next("account watched");
            return;
        }
        case "account watched": {
            // Every tick for a few seconds, while the fixture's arrivals make the core's list
            // reads (and so the accounts' refreshes) go on.
            const bad = win.archiveShown();
            if (bad !== "") return win.fail("the archive view after the account was picked: " + bad);
            if (App.currentAccount !== win.account) return win.fail("the account was not picked: " + App.currentAccount);
            win.watched += 1;
            if (win.watched < 120) return;
            App.selectAccount(-1);
            win.next("account all");
            return;
        }
        case "account all": {
            const bad = win.archiveShown();
            if (bad !== "") return win.fail("the archive view after all chats were picked: " + bad);
            win.waits += 1;
            if (win.waits < 10) return;
            Qt.exit(0);
            return;
        }
        case "hidden link": {
            // A second account (another identity, no chats), then the fixture account hidden.
            if (App.accountActions.length !== 1) return win.later("the fixture account");
            win.account = App.accountActions[0].account;
            win.openWelcome();
            win.openLink("Fake");
            win.next("hidden linked");
            return;
        }
        case "hidden linked": {
            if (App.accountActions.length !== 2) return win.later("the second account");
            if (win.linkPage !== null && win.pageStack.lastItem === win.linkPage) return win.later("the link page closes");
            if (!App.toggleAccount(win.account)) return win.fail("hiding the fixture account was refused");
            win.next(win.mode === "saved" ? "saved" : "hidden search");
            return;
        }
        case "saved": {
            // Saved; the window is closed and opened again (the second start, `saved-start`).
            if (App.shownCount !== 1) return win.later("one account shown");
            Qt.exit(0);
            return;
        }
        case "hidden search": {
            if (App.shownCount !== 1 || App.currentAccount === win.account || App.currentAccount < 0) return win.later("only the second account shown: " + App.shownCount + " " + App.currentAccount);
            if (convs.count !== 0) return win.later("no chat of the hidden account listed: " + convs.count);
            // A search looks in every account: the hidden account's archived chat is found.
            win.list().searchField.text = "a";
            win.next("hidden listed");
            return;
        }
        case "hidden listed": {
            let at = -1;
            for (let i = 0; i < convs.count; ++i) {
                if (convs.objectAt(i).archived) at = i;
            }
            if (at < 0) return win.later("the hidden account's archived chat among the matches: " + convs.count + " rows");
            win.archivedId = convs.objectAt(at).conversationId;
            if (!win.clickRow(at)) return win.later("the archived row's delegate");
            win.next("hidden opened");
            return;
        }
        case "hidden opened": {
            if (App.timeline.conversationId !== win.archivedId) return win.later("the hidden account's archived chat opens");
            // (Give a wrong switch time to show.)
            win.waits += 1;
            if (win.waits < 12) return;
            // The archive view lists the shown accounts' archived chats only: the list stays as
            // chosen (H3).
            if (App.archiveShown) return win.fail("a hidden account's archived chat picked from a search opened the archive view");
            if (App.shownCount !== 1) return win.fail("the pick changed the choice");
            Qt.exit(0);
            return;
        }
        case "search type": {
            // "a" is in the titles or previews of inbox chats and of the archived one.
            win.list().searchField.text = "a";
            win.next("search listed");
            return;
        }
        case "search listed": {
            // The inbox's matches come first, the archive's after them (one of each at least).
            let firstArchived = -1;
            let lastInbox = -1;
            for (let i = 0; i < convs.count; ++i) {
                if (convs.objectAt(i).archived) {
                    if (firstArchived < 0) firstArchived = i;
                } else {
                    lastInbox = i;
                }
            }
            if (firstArchived < 0 || lastInbox < 0) return win.later("an inbox match and an archived one: " + convs.count + " rows");
            if (firstArchived < lastInbox) return win.fail("an archived match before an inbox one: archived at " + firstArchived + ", inbox until " + lastInbox);
            win.archivedId = convs.objectAt(firstArchived).conversationId;
            win.archivedTitle = convs.objectAt(firstArchived).title;
            if (win.mode === "quick") {
                // The archived chat alone: its whole title is the query.
                win.list().searchField.text = win.archivedTitle;
                win.next("quick typed");
                return;
            }
            // First an inbox match is picked: the list stays on the chats.
            win.pickedId = convs.objectAt(0).conversationId;
            win.next("search pick inbox");
            return;
        }
        case "search pick inbox": {
            if (!win.clickRow(0)) return win.later("the first row's delegate");
            win.next("search inbox opened");
            return;
        }
        case "search inbox opened": {
            if (App.timeline.conversationId !== win.pickedId) return win.later("the inbox chat opens");
            // (Give a wrong switch time to show.)
            win.waits += 1;
            if (win.waits < 12) return;
            if (App.archiveShown) return win.fail("an inbox chat picked from a search opened the archive view");
            if (win.list().searchField.text !== "a") return win.fail("the search was emptied by the pick: " + win.list().searchField.text);
            win.next("search pick archived");
            return;
        }
        case "search pick archived": {
            const at = win.rowOf(win.archivedId);
            if (at < 0) return win.later("the archived chat among the matches");
            if (!win.clickRow(at)) return win.later("the archived row's delegate");
            win.reads = 0;
            win.next("search archived opened");
            return;
        }
        case "quick typed": {
            // The first match is the archived chat alone (or the quick switch would open another).
            const search = win.list().searchField;
            if (convs.count !== 1 || convs.objectAt(0).conversationId !== win.archivedId) return win.later("the archived chat as the only match: " + convs.count + " rows");
            search.forceActiveFocus();
            if (!search.activeFocus) return win.fail("the search field takes no keyboard");
            win.reads = 0;
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("search archived opened");
            return;
        }
        case "search archived opened": {
            if (App.timeline.conversationId !== win.archivedId) return win.later("the archived chat opens (open: " + App.timeline.conversationId + ", wanted: " + win.archivedId + ", archive view: " + App.archiveShown + ", rows: " + convs.count + ", opened so far: " + win.opened.join(",") + ")");
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view with the chat: " + bad);
            const page = win.list();
            if (page.searchField.text !== "") return win.fail("the search was not emptied: " + page.searchField.text);
            // The list was read once, as the archive: no inbox read in between (that inserts the
            // inbox's rows, or resets the model).
            if (win.reads !== 0) return win.fail("the pick read the list as the inbox first: " + win.reads + " model changes");
            // The chat is highlighted among the archive's rows.
            const row = page.conversations.itemAtIndex(0);
            if (row === null) return win.later("the archive's row");
            if (!row.highlighted) return win.fail("the archived chat's row is not highlighted");
            // (A moment for the picture to be final.)
            win.waits += 1;
            if (win.waits < 15 && "SHOTS_DIR" !== "") return;
            win.shot("picked");
            Qt.exit(0);
            return;
        }
        case "qi type": {
            // A chat of the inbox (neither pinned nor muted): its title is the query.
            if (convs.count < 6) return win.later("the list");
            win.pickedId = convs.objectAt(5).conversationId;
            win.list().searchField.text = convs.objectAt(5).title;
            win.next("qi typed");
            return;
        }
        case "qi typed": {
            const search = win.list().searchField;
            if (convs.count < 1 || convs.objectAt(0).conversationId !== win.pickedId) return win.later("the chat as the first match");
            search.forceActiveFocus();
            if (!search.activeFocus) return win.fail("the search field takes no keyboard");
            win.opened = [];
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.waits = 0;
            win.next("qi opened");
            return;
        }
        case "qi opened": {
            if (App.timeline.conversationId !== win.pickedId) return win.later("the chat opens (open: " + App.timeline.conversationId + ", wanted: " + win.pickedId + ", opened so far: " + win.opened.join(",") + ")");
            // (A moment for a second open to show: the Return that the search field passes
            // on must not open the list's current row as well.)
            win.waits += 1;
            if (win.waits < 25) return;
            if (win.opened.length !== 1 || App.archiveShown) return win.fail("the quick switch opened: " + win.opened.join(",") + ", archive view: " + App.archiveShown);
            Qt.exit(0);
            return;
        }
        case "comeback open": {
            win.list().openArchive();
            win.next("comeback opened");
            return;
        }
        case "comeback opened": {
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view: " + bad);
            win.archivedId = convs.objectAt(0).conversationId;
            if (!win.clickRow(0)) return win.later("the archived row's delegate");
            win.next("comeback chat");
            return;
        }
        case "comeback chat": {
            if (App.timeline.conversationId !== win.archivedId) return win.later("the archived chat opens");
            // Opening a chat of the archive leaves the archive view as it is.
            const bad = win.archiveShown();
            if (bad !== "") return win.fail("the archive view after the chat opened: " + bad);
            win.phone(win.archivedId, false, false);
            win.next("comeback sent");
            return;
        }
        case "comeback sent": {
            // A new message brought the chat back: the list goes to the chats, the chat stays.
            const bad = win.chatsShownWith(false);
            if (bad !== "") return win.later("the chats after the chat came back: " + bad);
            if (App.timeline.conversationId !== win.archivedId) return win.fail("the chat closed: " + App.timeline.conversationId);
            if (App.archivedCount !== 0) return win.fail("the archive's count: " + App.archivedCount);
            if (win.rowOf(win.archivedId) < 0) return win.fail("the chat is not among the inbox's rows");
            Qt.exit(0);
            return;
        }
        case "phone open": {
            // A chat of the inbox that is neither pinned nor muted (the fixture pins the first
            // two and mutes the third).
            if (convs.count < 6) return win.later("the list");
            win.pickedId = convs.objectAt(5).conversationId;
            win.openConversation(win.pickedId);
            win.next("phone chat");
            return;
        }
        case "phone chat": {
            if (App.timeline.conversationId !== win.pickedId) return win.later("the chat opens");
            if (win.mode === "phone-narrow" && win.pageStack.lastItem === win.list()) return win.later("the chat page over the list");
            win.phone(win.pickedId, true, false);
            win.next("phone archived");
            return;
        }
        case "phone archived": {
            // The phone archived the open chat: it closes, the list stays on the chats and
            // the entry counts the chat.
            if (App.timeline.conversationId !== 0) return win.later("the chat closes");
            if (win.mode === "phone-narrow" && win.pageStack.lastItem !== win.list()) return win.later("a narrow window goes back to the list");
            if (App.archivedCount !== 2) return win.later("the archive's count: " + App.archivedCount);
            const bad = win.chatsShown();
            if (bad !== "") return win.fail("the chats after the phone archived the chat: " + bad);
            if (win.rowOf(win.pickedId) >= 0) return win.fail("the archived chat is still among the inbox's rows");
            const entry = win.list().archiveEntry;
            if (entry === null || win.find(entry, i => i.text === "Archived Chats (2)") === null) return win.fail("the entry does not say 2");
            Qt.exit(0);
            return;
        }
        case "ctrlk open": {
            win.list().openArchive();
            win.next("ctrlk opened");
            return;
        }
        case "ctrlk opened": {
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view: " + bad);
            win.key(Qt.Key_K, Qt.ControlModifier);
            win.limit = 250;
            win.waits = 0;
            win.next("ctrlk done");
            return;
        }
        case "ctrlk done": {
            // The chats are back and the keyboard is in the search field.
            const bad = win.chatsShown();
            if (bad !== "") return win.later("the chats after Ctrl+K: " + bad);
            if (!win.list().searchField.activeFocus) return win.later("the keyboard in the search field");
            Qt.exit(0);
            return;
        }
        case "tab open": {
            // A chat of the inbox is open: the timeline is a stop of the keyboard.
            if (convs.count < 6) return win.later("the list");
            win.pickedId = convs.objectAt(5).conversationId;
            win.openConversation(win.pickedId);
            win.next("tab chat");
            return;
        }
        case "tab chat": {
            if (App.timeline.conversationId !== win.pickedId) return win.later("the chat opens");
            if (win.timelineStop === null) return win.later("the timeline as a stop of the keyboard");
            const page = win.list();
            if (page.archiveEntry === null) return win.later("the entry");
            page.conversations.forceActiveFocus();
            if (!page.conversations.activeFocus || page.archiveEntry.activeFocus) return win.fail("the list takes no keyboard");
            win.key(Qt.Key_Tab, Qt.NoModifier);
            win.next("tab footer");
            return;
        }
        case "tab footer": {
            // Tab from the list reaches the entry, which is the list's footer.
            const page = win.list();
            if (!page.archiveEntry.activeFocus) return win.later("Tab from the list reaches the entry");
            win.key(Qt.Key_Tab, Qt.NoModifier);
            win.next("tab timeline");
            return;
        }
        case "tab timeline": {
            if (!win.timelineStop.activeFocus) return win.later("Tab from the entry reaches the timeline");
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            win.next("tab back footer");
            return;
        }
        case "tab back footer": {
            const page = win.list();
            if (!page.archiveEntry.activeFocus) return win.later("Shift+Tab from the timeline lands on the entry");
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            win.next("tab back list");
            return;
        }
        case "tab back list": {
            const page = win.list();
            // (The list is the focus scope the entry sits in: it has the keyboard also while the
            // entry has it.)
            if (!page.conversations.activeFocus || page.archiveEntry.activeFocus) return win.later("Shift+Tab from the entry lands on the list");
            // The keyboard goes into the timeline another way (a click there): the list now
            // remembers itself, not the entry, and Shift+Tab must still land on the entry.
            win.timelineStop.forceActiveFocus();
            if (!win.timelineStop.activeFocus) return win.fail("the timeline takes no keyboard");
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            win.next("tab return");
            return;
        }
        case "tab return": {
            const page = win.list();
            if (!page.archiveEntry.activeFocus) return win.later("Shift+Tab from a timeline entered by a click lands on the entry");
            win.key(Qt.Key_Return, Qt.NoModifier);
            win.next("tab opened");
            return;
        }
        case "tab opened": {
            // (The open chat stays open: it is the archive's own reading that matters here.)
            if (!App.archiveShown) return win.later("Return on the entry opens the archive");
            const page = win.list();
            if (page.archiveBack === null) return win.later("the Back arrow");
            if (!page.conversations.activeFocus) return win.later("the keyboard in the list after the archive opened");
            win.key(Qt.Key_Backtab, Qt.ShiftModifier);
            win.next("tab back arrow");
            return;
        }
        case "tab back arrow": {
            const page = win.list();
            if (!page.archiveBack.activeFocus) return win.later("Shift+Tab from the archive's list lands on the Back arrow");
            Qt.exit(0);
            return;
        }
        case "all archive": {
            // Every chat of the inbox goes to the archive (the pinned ones are unpinned first: a
            // pinned chat stays in the chats).
            let n = 0;
            for (let i = 0; i < convs.count; ++i) {
                const c = convs.objectAt(i);
                if (!c.archived) {
                    win.phone(c.conversationId, true, c.pinned);
                    ++n;
                }
            }
            win.limit = 400;
            win.watched = n + 1;
            win.next("all archived");
            return;
        }
        case "all archived": {
            // Only the entry is left: the count says every chat, there is no row and no
            // "No conversations".
            if (convs.count !== 0 || App.archivedCount !== win.watched) return win.later("every chat archived: " + convs.count + " rows, " + App.archivedCount + " archived, wanted " + win.watched);
            const page = win.list();
            const entry = page.archiveEntry;
            if (entry === null || page.conversations.footerItem !== entry) return win.later("the entry as the footer of the empty list");
            const label = win.find(entry, i => i.text !== undefined && i.text.indexOf("Archived Chats") === 0);
            if (!label || label.text !== "Archived Chats (" + win.watched + ")") return win.fail("the entry's words: " + (label ? label.text : "none"));
            // The entry is in view, inside the list.
            const at = entry.mapToItem(page.conversations, 0, 0).y;
            if (!entry.visible || at < 0 || at + entry.height > page.conversations.height) return win.fail("the entry is not in view: y " + at + ", height " + entry.height + ", list " + page.conversations.height);
            // (A moment for a placeholder that comes late.)
            win.waits += 1;
            if (win.waits < 20) return;
            const placeholder = win.find(page, i => i.objectName === "empty-list");
            if (placeholder === null) return win.fail("the list's placeholder is not there at all");
            if (placeholder.visible) return win.fail("\"" + placeholder.text + "\" shows although the chats are archived");
            win.shot("allarchived");
            // The archive holds them all.
            page.openArchive();
            win.next("all opened");
            return;
        }
        case "all opened": {
            if (!App.archiveShown || convs.count !== win.watched) return win.later("the archive view with every chat: " + convs.count + " rows");
            if (win.find(win.list(), i => i.objectName === "empty-list").visible) return win.fail("the archive's own text shows with its chats");
            Qt.exit(0);
            return;
        }
        case "empty open": {
            win.list().openArchive();
            win.next("empty opened");
            return;
        }
        case "empty opened": {
            const bad = win.archiveShown();
            if (bad !== "") return win.later("the archive view: " + bad);
            const placeholder = win.find(win.list(), i => i.objectName === "empty-list");
            if (placeholder === null || placeholder.visible) return win.fail("the archive's own text shows with its chat");
            // The phone brings the archive's only chat back.
            win.archivedId = convs.objectAt(0).conversationId;
            win.phone(win.archivedId, false, false);
            win.next("empty sent");
            return;
        }
        case "empty sent": {
            const page = win.list();
            if (convs.count !== 0 || App.archivedCount !== 0) return win.later("the archive emptied: " + convs.count + " rows, " + App.archivedCount + " archived");
            // The archive view stays and says so; "No conversations" is the chats' own text.
            if (!App.archiveShown) return win.fail("the archive view went away");
            const placeholder = win.find(page, i => i.objectName === "empty-list");
            if (placeholder === null || !placeholder.visible) return win.later("the archive's own text");
            if (placeholder.text !== "No archived chats") return win.fail("the archive's text: " + placeholder.text);
            // Nothing is archived any more: "these chats" would point at nothing.
            const help = win.find(page, i => i.objectName === "archive-help");
            if (help === null || help.visible) return win.fail("the line about archived chats shows over an empty archive");
            // (A moment for the picture to be final.)
            win.waits += 1;
            if (win.waits < 15 && "SHOTS_DIR" !== "") return;
            win.shot("empty");
            page.archiveBack.click();
            win.next("empty back");
            return;
        }
        case "empty back": {
            // The chats, with no entry (nothing is archived), and not the archive's text.
            const bad = win.chatsShownWith(false);
            if (bad !== "") return win.later("the chats again: " + bad);
            const placeholder = win.find(win.list(), i => i.objectName === "empty-list");
            if (placeholder.visible) return win.fail("\"" + placeholder.text + "\" shows over the chats");
            Qt.exit(0);
            return;
        }
        }
    }
}
"##;

fn options(case: &str) -> FixtureOptions {
    let base = FixtureOptions {
        messages_per_conversation: 6,
        ..FixtureOptions::default()
    };
    match case {
        // Three chats, none of them archived (the fixture archives its fourth).
        "entry-none" => FixtureOptions {
            contacts: 3,
            groups: 0,
            showcase: false,
            ..base
        },
        // Messages keep arriving after the start: the core's list is read again and again.
        "account" => FixtureOptions {
            arrivals: Some(Arrivals {
                count: 8,
                every: Duration::from_millis(500),
                lifetime: None,
            }),
            ..base
        },
        // A second account is linked at once (another identity, no chats).
        "hidden" | "saved" => FixtureOptions {
            second_link: Some(FixtureSecondLink::OtherAccount),
            ..base
        },
        _ => base,
    }
}

fn run(case: &str) -> i32 {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(options(case)),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();

    // The phone's side: a request to this port archives or unarchives a chat the way a Storage
    // sync would, through the core and the fake network (`GET /archive?conv=<id>&to=<0|1>`).
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let port = listener.local_addr().expect("its address").port();
    let phone = handle.clone();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 1024];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            if let Some(query) = request
                .strip_prefix("GET /archive?")
                .and_then(|rest| rest.split_whitespace().next())
            {
                let value = |key: &str| {
                    query
                        .split('&')
                        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
                        .and_then(|v| v.parse::<i64>().ok())
                };
                if let (Some(conv), Some(to)) = (value("conv"), value("to")) {
                    if value("unpin") == Some(1) {
                        let sent = phone.send(UiCommand::ChangeConversation {
                            conv: ConversationId(conv),
                            change: ConversationChange::SetPinned { pinned: false },
                        });
                        assert!(sent.is_ok(), "the core takes the command");
                    }
                    let sent = phone.send(UiCommand::ChangeConversation {
                        conv: ConversationId(conv),
                        change: ConversationChange::SetArchived { archived: to == 1 },
                    });
                    assert!(sent.is_ok(), "the core takes the command");
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext { app: handle });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = SCRIPT
        .replace("CASE", case)
        .replace("LISTEN_PORT", &port.to_string())
        .replace(
            "SHOTS_DIR",
            &std::env::var("KOURIER_ARCHIVE_SHOTS").unwrap_or_default(),
        );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/archive_ui_test.qml"),
        );
    }
    let mut code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    if case == "saved" && code == 0 {
        // The same core and preferences, a new window and App: the start applies the saved
        // choice before its first list.
        assert_eq!(
            app.handle().desktop().prefs().hidden_accounts.len(),
            1,
            "the hidden account is saved"
        );
        kourier_qt::set_context(UiContext { app: app.handle() });
        let again = script.replace(
            "property string mode: \"saved\"",
            "property string mode: \"saved-start\"",
        );
        let mut engine = QQmlApplicationEngine::new();
        if let Some(mut engine) = engine.as_mut() {
            kourier_qt::install_image_provider(engine.as_mut());
            engine.load_data(
                &QByteArray::from(again.as_str()),
                &QUrl::from("qrc:/qt/qml/app/kourier/qml/archive_ui_test2.qml"),
            );
        }
        code = qt.as_mut().map_or(3, |qt| qt.exec());
        drop(engine);
    }
    kourier_qt::media::set_store(None);
    app.shutdown();
    code
}

fn main() {
    support::answer_list("archive_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_ARCHIVE_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_ARCHIVE_ONLY").unwrap_or_default();
    let cases: Vec<&str> = [
        "entry",
        "entry-none",
        "open",
        "back",
        "account",
        "search",
        "hidden",
        "saved",
        "quick",
        "quick-inbox",
        "comeback",
        "phone",
        "phone-narrow",
        "ctrlk",
        "tab",
        "allarchived",
        "empty",
    ]
    .into_iter()
    .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
    .collect();
    let mut failed = 0;
    for case in &cases {
        let out = Command::new(&exe)
            .env("KOURIER_ARCHIVE_UI_CASE", case)
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
            println!("test archive: {case} ... ok");
        } else {
            println!("test archive: {case} ... FAILED ({})", out.status);
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
