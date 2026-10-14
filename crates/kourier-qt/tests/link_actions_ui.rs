//! A link in progress is never cancelled without asking (`target/link-feedback/plan.md`
//! L1, `buttons.md` F3), in the real main window on the real core over the fake network
//! (offscreen, `harness = false`; each case in a process of its own).
//!
//! - `keep`: at the QR code of a first link, Settings is asked for the way the drawer asks
//!   (`AccountsDrawer.openSettings`): "Stop Linking?" opens with the keyboard on "Continue
//!   Linking"; Enter and a press of "Stop Linking" in the first moment do nothing; "Continue
//!   Linking" closes it, the link page and its QR code stay, and the fake was told nothing.
//! - `stop`: the same question, "Stop Linking" once it acts: Settings opens and the login
//!   is cancelled (no QR code on any page, the core has no account).
//! - `add`: "Add Account" of the drawer asks the same question.
//! - `cancel2`: Cancel of the link page pressed twice at the QR code (a double click):
//!   one `CancelLogin`, the welcome page alone on the stack, no login left (`buttons.md` §3).
//! - `triple`: three quick presses on the network's entry of the welcome page: one link
//!   page at its code, the two logins before it cancelled once each (also when the core had
//!   not named them yet), at most one account.
//! - `transfer`: a fresh link whose history transfer runs is linked already: its link page
//!   has ended and the transfer waits in the banner; Settings opens at once, without a
//!   question, and nothing is cancelled. While it waits "Transfer History from Phone…" is
//!   offered nowhere (`App.historyAccounts`, the account's actions, the page); once the
//!   transfer is skipped it is back (L5).
//! - `xback` (L5): the import is being read; the link page that says "Linked" for a moment is
//!   left by the page stack's Back: no `CancelLogin`, no skip, the account and the import in
//!   the banner stay.
//!
//! The cases with a fixture account and a second link that waits at its QR code in a WIDE
//! window (the list stands beside the link page):
//!
//! - `chat`: opening a chat the four ways the user has (a click on the list, the quick
//!   switch, the step keys, a notification) asks the same question; "Continue Linking" keeps
//!   the link page, no chat opens and the fake is told no `CancelLogin` (F3).
//! - `chatstop`: the same, and the last time "Stop Linking" opens the chat and cancels
//!   that one link.
//!   The quick switch is typed into the search field: after "Continue Linking" the typed
//!   text is still there (`L1-recheck.md` new 4).
//! - `switchstop`: the quick switch typed while the link waits, "Stop Linking": the chat it
//!   found opens and the field is empty (it is emptied when the chat opens, not before the
//!   question).
//! - `latest`: a chat is opened from the list, and while "Stop Linking?" asks, a
//!   notification of another chat is clicked: one question stays, and "Stop Linking" opens
//!   the chat asked for last (the second trigger is not dropped, `L1-recheck.md` new 4).
//! - `banner`: the banner of a challenge (its button opens the challenge dialog, the link
//!   page stays) asks nothing: only "Link Again" of a logged-out account takes the link
//!   page's place.
//! - `error`: the same for the "error" banner of an account whose connection another session
//!   took (its "Reconnect" restarts that account): no question, the link page and its QR
//!   code stay, nothing is cancelled (only "Link Again" asks: `L1-recheck.md` new 1).
//! - `relink`: the banner of a logged-out account asks; "Continue Linking" keeps the QR
//!   code; "Stop Linking" cancels that link and the relink runs through (the old link page
//!   leaving the stack does not cancel the relink that just began).
//! - `already`: the phone scans a code of the account the profile has: the link fails with
//!   "already in Kourier" and offers "OK" to the list, no "Try Again".
//! - `already-out`: the same for an account the phone unlinked (logged out): the text points
//!   at "Link Again" and the button is "Go to Chats", which goes to the list where that
//!   account's "Link Again" banner is (`L1-recheck.md` new 3).
//! - `draft` (L5): a draft typed in the open chat survives "Add Account" (the chat page leaves
//!   the stack) and the return to the chats (a page that left the stack is made anew, never
//!   pushed back, which printed Kirigami's header warnings): the new composer holds it.
//! - `xadd` (L5): the fixture account's transfer waits for the phone; Add Account asks nothing
//!   (its own link ended), the second link waits at its code, Cancel there cancels that link
//!   only (the banner keeps the transfer, no skip), and Settings offers no second transfer.
//! - `xother` (L5): the same, and the phone of ANOTHER account scans the second code at once:
//!   both accounts stay linked, the first one's transfer goes on in the banner, and only the
//!   new account offers "Transfer History from Phone…".
//! - `drawer`: a second account drawer made while the account exists (what a window made
//!   again after the tray hid it has) follows the LAST account's leaving late, as the
//!   first one does: `AccountsDrawer.accountsShown` is no binding that flips with the
//!   Repeater's entry (the crash of `transfer_review_ui` `wide-many`).

mod support;

use std::process::Command;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{Query, QueryData};
use kourier_app::{
    App, FixtureOptions, FixtureSecondLink, FixtureState, FixtureTransfer, HistoryTransfer, Launch,
    StartOptions,
};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

MainWindow {
    id: win

    width: win.wide ? 1400 : 520
    height: win.wide ? 800 : 760
    visible: true

    property string mode: "CASE"
    // The cases with a fixture account and a second link beside its chat list.
    readonly property bool wide: ["chat", "chatstop", "switchstop", "latest", "banner", "error", "relink", "already", "already-out", "drawer", "draft", "xadd", "xother"].indexOf(win.mode) >= 0
    // The ways the case does the thing that would leave the link page, one per round; the
    // last round of a case that stops is answered with "Stop Linking", the others keep.
    readonly property var rounds: win.mode === "chat" ? ["list", "switch", "step", "activate"] : win.mode === "chatstop" ? ["list", "switch", "step", "activate", "list"] : win.mode === "switchstop" ? ["switch"] : win.mode === "relink" ? ["banner", "banner"] : []
    readonly property bool stopsLast: win.mode === "chatstop" || win.mode === "switchstop" || win.mode === "relink"
    property int round: 0
    // `drawer`: the drawer made late, and when the last account left.
    property var extra: null
    property double lostAt: 0
    property real chatId: 0
    // `latest`: the chat of the notification clicked while the question is open.
    property real otherId: 0
    // What the quick switch types.
    readonly property string typed: "a"
    property real openedBefore: 0
    property string step: "start"
    property int waits: 0
    property int quiet: 0
    property var ev: null
    property int stopAsked: 0
    property bool sawBack: false

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            fail("never happened: " + what + " (login " + App.login.kind + ", accounts " + App.hasAccounts + ")");
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.quiet = 0;
    }

    function linking(): bool {
        return win.linkPage !== null && win.pageStack.lastItem === win.linkPage;
    }

    function asking(): bool {
        return win.stopLinkingDialog !== null && win.stopLinkingDialog.opened;
    }

    // What the user does that would leave the link page, in a wide window beside the list.
    function trigger(what: string): void {
        switch (what) {
        case "list":
            win.pageStack.get(0).openConversation(win.chatId);
            break;
        case "switch": {
            // Typed into the search field, then Enter (`ConversationListPage` quick switch).
            const field = win.pageStack.get(0).searchField;
            field.text = win.typed;
            if (App.conversationAt(0) === 0) return win.fail("the quick switch finds no chat for '" + win.typed + "'");
            field.accepted();
            break;
        }
        case "step":
            win.stepConversation(1);
            break;
        case "activate":
            win.openFromActivation(win.chatId);
            break;
        case "banner":
            win.pageStack.get(0).bannerActivated();
            break;
        }
    }

    // The first visible button with this text below `item`.
    function findButton(item: Item, text: string): Item {
        if (item.visible && item.text === text && typeof item.click === "function") {
            return item;
        }
        const kids = item.children;
        for (let i = 0; i < kids.length; i++) {
            const found = win.findButton(kids[i], text);
            if (found !== null) {
                return found;
            }
        }
        return null;
    }

    // The label with this text below `item`.
    function findText(item: Item, text: string): Item {
        if (item.text === text && item.wrapMode !== undefined) {
            return item;
        }
        const kids = item.children;
        for (let i = 0; i < kids.length; i++) {
            const found = win.findText(kids[i], text);
            if (found !== null) {
                return found;
            }
        }
        return null;
    }

    // The question's text is laid out inside the dialog: wrapped, not one cut-off line.
    function textFits(d: var): string {
        const label = win.findText(d.contentItem, d.question.text);
        if (label === null) return "no label shows the question's text";
        if (label.contentWidth > d.width) return "the text is one line of " + label.contentWidth + " px in a dialog of " + d.width + " px";
        return "";
    }

    // What the drawer does for its entries.
    function drawerAction(): void {
        if (win.mode === "add") {
            win.globalDrawer.addAccount();
        } else {
            win.globalDrawer.openSettings();
        }
    }

    Connections {
        target: App
        function onHasAccountsChanged(): void {
            if (!App.hasAccounts && win.lostAt === 0) {
                win.lostAt = Date.now();
            }
        }
    }

    // The relink's page says "Linked" for a moment before it is left (and its login reset).
    property bool sawDone: false
    Connections {
        target: App.login
        function onKindChanged(): void {
            if (App.login.kind === "done") {
                win.sawDone = true;
            }
        }
    }

    // `KOURIER_LINK_ACTIONS_SHOTS=<dir>`: a picture of the question for a look (not a test).
    function shot(name: string): void {
        if ("SHOTS_DIR" !== "") {
            App.saveScreenshot(win, "SHOTS_DIR/" + name + ".png");
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
        case "start":
            if (!win.started) return win.later("the start page");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "link_actions_ui_events.qml");
            if (win.mode === "drawer") {
                if (!App.hasAccounts || App.historyAccounts.length !== 1) return win.later("the fixture account");
                // A drawer made while the account exists, as a window made again has.
                win.extra = Qt.createQmlObject("import app.kourier\nAccountsDrawer { window: win }", win.contentItem, "link_actions_ui_drawer.qml");
                if (!win.extra.accountsShown) return win.fail("a drawer made with an account does not show it");
                App.askReplace(App.historyAccounts[0].account);
                win.next("drawer-asked");
                return;
            }
            if (win.wide) {
                // The fixture account with its chats, and a second link beside the list.
                if (!App.hasAccounts) return win.later("the fixture account");
                win.chatId = App.conversationAt(0);
                if (win.chatId === 0) return win.later("the chat list");
                if (win.mode === "already-out" && App.bannerKind !== "loggedOut") return win.later("the account is logged out");
                win.openedBefore = App.timeline.conversationId;
                if (win.mode === "xadd" || win.mode === "xother") {
                    // The account's history transfer waits for the phone, in its banner.
                    if (App.bannerTransfer.phase !== "waiting") return win.later("the transfer waits in the banner");
                    // Add Account asks nothing (the account's own link ended), then the
                    // network is chosen on the welcome page.
                    win.globalDrawer.addAccount();
                    if (win.stopLinkingDialog !== null) return win.fail("Add Account asked a question although no link was going on");
                    win.openLink("Fake");
                    win.next("xadd-link");
                    return;
                }
                if (win.mode === "draft") {
                    win.openConversation(win.chatId);
                    win.next("draft-typed");
                    return;
                }
                win.openWelcome();
                win.openLink("Fake");
                win.next("qr");
                return;
            }
            if (App.hasAccounts) return win.fail("a link case starts without an account");
            if (win.mode === "triple") {
                // Three quick presses on the network's entry of the welcome page.
                const welcome = win.ensureWelcome();
                welcome.linkNetwork("Fake");
                welcome.linkNetwork("Fake");
                welcome.linkNetwork("Fake");
                win.next("input");
                return;
            }
            win.openLink("Fake");
            win.next("input");
            return;
        case "input":
            if (App.login.kind !== "input") return win.later("the device name question");
            App.login.submit();
            win.next(win.mode === "transfer" ? "transfer" : win.mode === "xback" ? "xback" : "qr");
            return;

        case "qr":
            if (win.mode === "already" || win.mode === "already-out") {
                // The phone scans at once and the core refuses the account it has.
                if (App.login.kind !== "failed") return win.later("the refusal");
                win.next("already");
                return;
            }
            if (App.login.kind !== "qr" || !win.linking()) return win.later("the QR code");
            if (win.mode === "banner" && App.bannerKind !== "challenge") return win.later("the challenge banner");
            if (win.mode === "error" && App.bannerKind !== "error") return win.later("the error banner");
            if (win.mode === "relink" && App.bannerKind !== "loggedOut") return win.later("the banner of the logged-out account");
            if (win.mode === "banner" || win.mode === "error") {
                win.pageStack.get(0).bannerActivated();
                win.next("banner-after");
                return;
            }
            if (win.rounds.length > 0) {
                win.next("go");
                return;
            }
            if (win.mode === "cancel2") {
                // Cancel pressed twice in one moment (a double click on the action).
                const cancel = win.linkPage.actions[0];
                if (cancel.text !== "Cancel" || !cancel.enabled) return win.fail("no Cancel action at the QR code");
                cancel.trigger();
                cancel.trigger();
                win.next("cancel2-after");
                return;
            }
            if (win.mode === "triple") {
                win.next("triple-after");
                return;
            }
            if (win.mode === "latest") {
                win.otherId = App.conversationAt(1);
                if (win.otherId === 0 || win.otherId === win.chatId) return win.fail("no second chat");
                win.trigger("list");
                win.next("latest-asked");
                return;
            }
            win.drawerAction();
            if (!win.linking()) return win.fail("the link page left before the question was answered");
            win.next("asked");
            return;
        case "asked": {
            if (!win.asking()) return win.later("the question");
            const d = win.stopLinkingDialog;
            if (d.title !== "Stop Linking?") return win.fail("the title: " + d.title);
            if (d.confirmButton.text !== "Stop Linking" || d.cancelButton.text !== "Continue Linking") return win.fail("the buttons: " + d.confirmButton.text + " / " + d.cancelButton.text);
            // "Continue Linking" is no "back": no arrow on it (user review of L1).
            if (d.cancelButton.icon.name !== "") return win.fail("an icon on Continue Linking: " + d.cancelButton.icon.name);
            // The text says what cancels the link, not that going on does (user review of L1).
            if (d.question.text.indexOf("Going on") >= 0 || d.question.text.indexOf("Opening this now cancels the link") < 0) return win.fail("the text: " + d.question.text);
            if (!d.cancelButton.activeFocus) return win.fail("the keyboard is not on Continue Linking");
            win.shot("stop-linking-" + win.mode);
            if (win.textFits(d) !== "") return win.fail(win.textFits(d));
            // A second ask while it is open makes no second question.
            win.drawerAction();
            if (win.stopLinkingDialog !== d) return win.fail("a second question");
            // In the first moment the confirming button does nothing.
            d.confirmButton.click();
            if (!d.opened || !win.linking()) return win.fail("an early press of Stop Linking acted");
            // Enter on the default answer does not confirm.
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            win.next(win.mode === "keep" ? "keep" : "wait-armed");
            return;
        }
        case "keep": {
            // (Enter on "Continue Linking" closes the question: nothing else.)
            if (win.asking()) {
                win.stopLinkingDialog.cancelButton.click();
                return win.later("the question closes");
            }
            if (!win.linking() || App.login.kind !== "qr") return win.fail("the link was left: login " + App.login.kind);
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.stopLinkingDialog !== null) return win.fail("the question is still there");
            if (App.login.kind !== "qr") return win.fail("the login moved: " + App.login.kind);
            Qt.exit(0);
            return;
        }
        case "wait-armed": {
            if (win.asking() && win.stopLinkingDialog.armed) {
                win.stopLinkingDialog.confirmButton.click();
                win.next("stopped");
                return;
            }
            if (!win.asking()) {
                // Enter closed it (the default answer): ask again.
                if (!win.linking()) return win.fail("Enter left the link page");
                win.drawerAction();
            }
            return win.later("the question takes input");
        }
        case "stopped":
            if (win.linking()) return win.later("the link page goes");
            if (win.mode === "add") {
                if (win.pageStack.lastItem !== win.welcomePage) return win.fail("Add Account did not open the welcome page");
            } else if (win.pageStack.lastItem !== win.settingsPage) {
                return win.fail("Settings did not open");
            }
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (App.hasAccounts) return win.fail("an account was left behind");
            Qt.exit(0);
            return;

        case "draft-typed": {
            // A draft typed in the open chat survives the chat page's leaving the stack: a
            // page that left it is made anew when it comes back (a page pushed back makes
            // Kirigami evaluate its header without a page row), and the new composer must
            // hold what the old one did.
            if (App.timeline.conversationId !== win.chatId || win.chatPage === null || !win.inStack(win.chatPage)) return win.later("the chat");
            win.chatPage.composer.setText("a draft kept");
            if (win.chatPage.composer.text !== "a draft kept") return win.fail("the composer did not take the text: " + win.chatPage.composer.text);
            win.extra = win.chatPage;
            // Add Account: the chat page leaves the stack.
            win.openWelcome();
            if (win.inStack(win.extra)) return win.fail("the chat page is still in the stack");
            // Back to the chats.
            win.showList();
            if (win.chatPage === win.extra) return win.fail("the chat page that left the stack was pushed back");
            if (win.chatPage === null || !win.inStack(win.chatPage)) return win.fail("no chat page beside the list");
            win.next("draft-back");
            return;
        }
        case "draft-back": {
            if (win.chatPage.composer.text !== "a draft kept") return win.later("the draft in the new composer (it holds '" + win.chatPage.composer.text + "')");
            if (App.timeline.conversationId !== win.chatId) return win.fail("another chat is open");
            win.quiet += 1;
            if (win.quiet < 10) return;
            Qt.exit(0);
            return;
        }
        case "drawer-asked":
            if (win.replaceDialog === null || !win.replaceDialog.opened || !win.replaceDialog.armed) return win.later("the question about the account");
            win.replaceDialog.confirmButton.click();
            win.next("drawer-removing");
            return;
        case "drawer-removing": {
            if (win.lostAt === 0) return win.later("the last account leaves");
            const age = Date.now() - win.lostAt;
            // The first drawer follows 300 ms late; the late one must too: with a binding it
            // flips at once, in the moment its entry is deleted.
            if (age < 150 && !win.extra.accountsShown) return win.fail("the drawer followed hasAccounts at once (" + age + " ms)");
            if (age < 900) return;
            if (win.extra.accountsShown) return win.fail("the drawer never followed hasAccounts");
            Qt.exit(0);
            return;
        }
        case "go":
            win.trigger(win.rounds[win.round]);
            win.next("ask");
            return;
        case "ask": {
            const what = win.rounds[win.round];
            if (!win.asking()) return win.later("the question after '" + what + "'");
            const d = win.stopLinkingDialog;
            if (d.title !== "Stop Linking?") return win.fail("the title: " + d.title);
            if (!d.cancelButton.activeFocus) return win.fail("the keyboard is not on Continue Linking");
            if (!win.linking() || App.login.kind !== "qr") return win.fail("'" + what + "': the link page left before the answer");
            if (App.timeline.conversationId !== win.openedBefore) return win.fail("'" + what + "': a chat opened before the answer");
            if (win.round === 0) win.shot("stop-linking-" + win.mode);
            if (win.textFits(d) !== "") return win.fail(win.textFits(d));
            if (win.stopsLast && win.round === win.rounds.length - 1) {
                win.next("confirm");
                return;
            }
            d.cancelButton.click();
            win.next("kept");
            return;
        }
        case "latest-asked": {
            if (!win.asking()) return win.later("the question after a click on the list");
            const d = win.stopLinkingDialog;
            win.openFromActivation(win.otherId);
            if (win.stopLinkingDialog !== d || !d.opened) return win.fail("a second trigger made a second question or closed the first");
            if (App.timeline.conversationId !== win.openedBefore) return win.fail("a chat opened before the answer");
            win.next("confirm");
            return;
        }
        case "kept":
            if (win.asking() || win.stopLinkingDialog !== null) return win.later("the question closes");
            if (win.rounds[win.round] === "switch") {
                const field = win.pageStack.get(0).searchField;
                if (field.text !== win.typed) return win.fail("the quick-switch text is gone after Continue Linking: '" + field.text + "'");
                field.text = "";
            }
            if (!win.linking() || App.login.kind !== "qr") return win.fail("the link was left after Continue Linking: " + App.login.kind);
            if (App.timeline.conversationId !== win.openedBefore) return win.fail("a chat opened after Continue Linking");
            win.round += 1;
            if (win.round < win.rounds.length) {
                win.next("go");
                return;
            }
            win.next("kept-quiet");
            return;
        case "kept-quiet":
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.stopLinkingDialog !== null || !win.linking() || App.login.kind !== "qr") return win.fail("the link did not stay");
            Qt.exit(0);
            return;
        case "confirm":
            if (win.asking() && win.stopLinkingDialog.armed) {
                win.stopLinkingDialog.confirmButton.click();
                win.next("after-stop");
                return;
            }
            return win.later("the question takes input");
        case "after-stop":
            if (win.mode === "latest") {
                if (App.timeline.conversationId !== win.otherId) {
                    if (App.timeline.conversationId === win.chatId) return win.fail("the first trigger's chat opened, not the one asked for last");
                    return win.later("the notification's chat opens");
                }
                if (win.linking()) return win.fail("the link page is still up");
            } else if (win.mode === "switchstop") {
                if (App.timeline.conversationId === win.openedBefore || App.timeline.conversationId === 0) return win.later("the quick switch's chat opens");
                if (win.linking()) return win.fail("the link page is still up");
                if (win.pageStack.get(0).searchField.text !== "") return win.fail("the quick-switch text stays after the chat opened");
            } else if (win.mode === "chatstop") {
                if (App.timeline.conversationId !== win.chatId || win.pageStack.depth < 2 || win.pageStack.get(1) !== win.chatPage) return win.later("the chat opens");
                if (win.linking()) return win.fail("the link page is still up");
            } else {
                // The relink runs through: its page said "Linked" (the old page leaving the
                // stack did not cancel it).
                if (!win.sawDone) return win.later("the relink is done (login " + App.login.kind + ")");
            }
            win.quiet += 1;
            if (win.quiet < 25) return;
            Qt.exit(0);
            return;
        case "banner-after":
            if (win.stopLinkingDialog !== null) return win.fail("a banner that leaves the link alone asked 'Stop Linking?'");
            if (!win.linking() || App.login.kind !== "qr") return win.fail("the link was left: login " + App.login.kind);
            win.quiet += 1;
            if (win.quiet < 25) return;
            Qt.exit(0);
            return;
        case "already": {
            const out = win.mode === "already-out";
            const leave = out ? "Go to Chats" : "OK";
            if (App.login.retry) return win.fail("an account that is here offers Try Again");
            if (App.login.leave !== leave) return win.fail("the way back: '" + App.login.leave + "'");
            if (App.login.error.indexOf("already in Kourier") < 0) return win.fail("the text: " + App.login.error);
            if (out !== (App.login.error.indexOf("“Link Again”") >= 0)) return win.fail("the text and Link Again: " + App.login.error);
            if (win.linkPage === null || !win.linking()) return win.fail("the failure is not on the link page");
            if (win.findButton(win.linkPage, "Try Again") !== null) return win.fail("a visible Try Again");
            const ok = win.findButton(win.linkPage, leave);
            if (ok === null) return win.later("the " + leave + " button");
            // (The message slides in: a picture waits for it.)
            win.quiet += 1;
            if (win.quiet < 25) return;
            win.shot("link-failed-" + win.mode);
            ok.click();
            win.next("already-after");
            return;
        }
        case "already-after":
            if (win.linking()) return win.later("OK leaves the link page");
            if (win.pageStack.lastItem === win.welcomePage) return win.fail("OK did not return to the list");
            // The logged-out account's banner with its "Link Again" is what is shown.
            if (win.mode === "already-out" && (App.bannerKind !== "loggedOut" || !win.pageStack.get(0).visible)) return win.fail("no Link Again banner in sight: " + App.bannerKind);
            win.quiet += 1;
            if (win.quiet < 25) return;
            Qt.exit(0);
            return;

        case "cancel2-after":
            if (win.linking()) return win.later("Cancel leaves the link page");
            if (win.pageStack.lastItem !== win.welcomePage) return win.fail("Cancel did not return to the welcome page");
            if (win.pageStack.depth !== 1) return win.fail("the stack has " + win.pageStack.depth + " pages after a double Cancel");
            win.quiet += 1;
            if (win.quiet < 25) return;
            // (A left login shows the empty view, kind "waiting"; the core's side is checked in Rust.)
            if (App.hasAccounts || App.login.kind === "qr") return win.fail("a login stayed: " + App.login.kind);
            Qt.exit(0);
            return;
        case "triple-after":
            // One link page at its code; the two earlier logins are cancelled (in Rust).
            if (!win.linking() || App.login.kind !== "qr") return win.fail("the link left its code: " + App.login.kind);
            if (win.pageStack.depth !== 2) return win.fail("the stack has " + win.pageStack.depth + " pages after three presses");
            win.quiet += 1;
            if (win.quiet < 25) return;
            Qt.exit(0);
            return;

        case "transfer":
            // (Link feedback L3: the link page ends as soon as the core has the account; the
            // transfer waits for the phone in the banner, and the chats can be used.)
            if (App.bannerTransfer.phase !== "waiting" || win.linking()) return win.later("the transfer waits for the phone in the banner, the link page gone");
            win.drawerAction();
            if (win.stopLinkingDialog !== null) return win.fail("a linked account's transfer was asked about");
            if (win.pageStack.lastItem !== win.settingsPage) return win.fail("Settings did not open at once");
            win.next("transfer-settings");
            return;
        case "transfer-settings": {
            // (L5) While the transfer runs no second one is offered: not in the list of the
            // accounts that offer it, not among the account's actions, not as a button on the
            // page. Once it is skipped the action is back.
            win.settingsPage.showAccounts();
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (App.accountActions.length !== 1) return win.fail(App.accountActions.length + " accounts in the settings");
            const label = App.accountActions[0].transferLabel;
            if (App.historyAccounts.length !== 0 || App.accountActions[0].transfer) return win.fail("a second Transfer History is offered while one runs");
            if (win.findButton(win.settingsPage, label) !== null) return win.fail("'" + label + "' is on the settings page while a transfer runs");
            App.skipTransfer(App.accountActions[0].account);
            win.next("transfer-offered");
            return;
        }
        case "transfer-offered": {
            const label = App.accountActions[0].transferLabel;
            if (!App.accountActions[0].transfer || win.findButton(win.settingsPage, label) === null) return win.later("'" + label + "' is offered again once the transfer is over");
            win.next("transfer-after");
            return;
        }

        // ---- xadd / xother (L5): Add Account while the first account's transfer runs ----
        case "xadd-link":
            if (win.mode === "xadd") {
                // The second link waits at its code; the first account's transfer is still
                // in its banner, and nothing about it changed.
                if (App.login.kind !== "qr" || !win.linking()) return win.later("the second link's QR code");
                if (App.bannerTransfer.phase !== "waiting") return win.fail("the first account's transfer left the banner: '" + App.bannerTransfer.phase + "'");
                win.quiet += 1;
                if (win.quiet < 10) return;
                // Back from the link page (its Cancel action).
                const cancel = win.linkPage.actions[0];
                if (cancel.text !== "Cancel" || !cancel.enabled) return win.fail("no Cancel at the second link's QR code");
                cancel.trigger();
                win.next("xadd-back");
                return;
            }
            // xother: the phone scans at once and is another account; both stay linked. The
            // first one's transfer goes on in the banner, and only the new account offers
            // "Transfer History from Phone…".
            if (App.accountActions.length < 2 || win.linking()) return win.later("the second account is linked and its page left");
            if (App.bannerTransfer.phase !== "waiting") return win.fail("the first account's transfer left the banner: '" + App.bannerTransfer.phase + "'");
            if (App.accountActions.length !== 2 || App.accountActions.filter(a => a.transfer).length !== 1) return win.later("only the second account offers a transfer");
            win.next("xadd-after");
            return;
        case "xadd-back":
            if (win.linking() || App.login.kind === "qr") return win.later("the link page is left");
            if (win.pageStack.lastItem === win.linkPage) return win.later("the link page left the stack");
            win.next("xadd-after");
            return;
        case "xadd-after":
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (App.bannerTransfer.phase !== "waiting") return win.fail("the first account's transfer is not in the banner at the end: '" + App.bannerTransfer.phase + "'");
            if (!App.hasAccounts) return win.fail("the first account went");
            if (win.mode === "xadd") {
                // (8d) Settings while the transfer waits: no second one is offered. (A fixture
                // account with a transfer and no link of its own: only the transfer hides it.)
                win.openSettings();
                win.next("xadd-settings");
                return;
            }
            Qt.exit(0);
            return;
        case "xadd-settings": {
            if (win.settingsPage === null || win.pageStack.lastItem !== win.settingsPage) return win.later("the settings page");
            win.settingsPage.showAccounts();
            win.quiet += 1;
            if (win.quiet < 10) return;
            const label = App.accountActions[0].transferLabel;
            if (App.historyAccounts.length !== 0 || App.accountActions[0].transfer) return win.fail("a second Transfer History is offered while one runs");
            if (win.findButton(win.settingsPage, label) !== null) return win.fail("'" + label + "' is on the settings page while a transfer runs");
            Qt.exit(0);
            return;
        }

        // ---- xback (L5): Back from the link page while the import runs ----
        case "xback":
            // The first report of the transfer ends the link, and the page says "Linked" for
            // a moment before it is left. Back in that moment (the page leaves the stack
            // by the user's hand) must not stop the import or cancel anything.
            if (win.sawBack) {
                win.next("xback-after");
                return;
            }
            if (!(win.linking() && App.login.kind === "done" && App.bannerTransfer.phase === "reading")) return win.later("the link page that says Linked, with the import in the banner");
            win.pageStack.pop();
            win.sawBack = true;
            win.next("xback-after");
            return;
        case "xback-after":
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (!App.hasAccounts) return win.fail("the linked account went with Back");
            if (App.bannerTransfer.phase !== "reading") return win.fail("the import left the banner after Back: '" + App.bannerTransfer.phase + "'");
            Qt.exit(0);
            return;

        case "transfer-after":
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (!App.hasAccounts) return win.fail("the linked account went");
            Qt.exit(0);
            return;
        }
    }
}
"##;

fn options(case: &str) -> FixtureOptions {
    let base = FixtureOptions {
        messages_per_conversation: 6,
        link: true,
        transfer_stage: Duration::from_millis(600),
        ..FixtureOptions::default()
    };
    match case {
        // The first code stays on screen for a minute: the second would be scanned.
        "keep" | "stop" | "add" | "cancel2" | "triple" => FixtureOptions {
            qr_codes: 2,
            qr_valid_for: Duration::from_secs(60),
            ..base
        },
        "transfer" => FixtureOptions {
            qr_codes: 1,
            qr_valid_for: Duration::from_millis(200),
            transfer: Some(FixtureTransfer::Hold(HistoryTransfer::Waiting)),
            ..base
        },
        // The import is being read (no skip), and the page that says "Linked" is left by hand.
        "xback" => FixtureOptions {
            qr_codes: 1,
            qr_valid_for: Duration::from_millis(200),
            transfer: Some(FixtureTransfer::Hold(HistoryTransfer::Reading)),
            ..base
        },
        // The fixture account's transfer waits for the phone, and a second link is made.
        "xadd" | "xother" => FixtureOptions {
            link: false,
            transfer: Some(FixtureTransfer::Left(HistoryTransfer::Waiting)),
            second_link: Some(if case == "xadd" {
                FixtureSecondLink::AtQr
            } else {
                FixtureSecondLink::OtherAccount
            }),
            ..base
        },
        // A fixture account with its chats, and a second link beside the list.
        "chat" | "chatstop" | "switchstop" | "latest" => FixtureOptions {
            link: false,
            second_link: Some(FixtureSecondLink::AtQr),
            ..base
        },
        "draft" => FixtureOptions {
            link: false,
            ..base
        },
        "banner" => FixtureOptions {
            link: false,
            state: FixtureState::Challenge,
            second_link: Some(FixtureSecondLink::AtQr),
            ..base
        },
        "error" => FixtureOptions {
            link: false,
            state: FixtureState::Reconnect,
            second_link: Some(FixtureSecondLink::AtQr),
            ..base
        },
        "relink" => FixtureOptions {
            link: false,
            state: FixtureState::LoggedOut,
            second_link: Some(FixtureSecondLink::AtQr),
            ..base
        },
        "already" => FixtureOptions {
            link: false,
            second_link: Some(FixtureSecondLink::SameAccount),
            ..base
        },
        "already-out" => FixtureOptions {
            link: false,
            state: FixtureState::LoggedOut,
            second_link: Some(FixtureSecondLink::SameAccount),
            ..base
        },
        "drawer" => FixtureOptions {
            link: false,
            history_on_link: true,
            second_link: Some(FixtureSecondLink::AtQr),
            ..base
        },
        other => panic!("unknown case {other}"),
    }
}

/// The accounts the core has.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn account_rows(app: &kourier_app::AppHandle) -> Vec<kourier_app::facade::AccountRow> {
    match app.runtime().block_on(app.query(Query::Accounts)) {
        Ok(QueryData::Accounts(accounts)) => accounts,
        _ => panic!("the accounts"),
    }
}

/// How many accounts the core has.
fn accounts(app: &kourier_app::AppHandle) -> usize {
    account_rows(app).len()
}

/// Runs one case in this process; returns its exit code.
fn run(case: &str) -> i32 {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(options(case)),
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
    let script = SCRIPT.replace("CASE", case).replace(
        "SHOTS_DIR",
        &std::env::var("KOURIER_LINK_ACTIONS_SHOTS").unwrap_or_default(),
    );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/link_actions_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        app.shutdown();
        return code;
    }

    let commands = handle.fixture_commands();
    let count = |name: &str| commands.iter().filter(|c| **c == name).count();
    let mut problems = Vec::new();
    let n = accounts(&handle);
    // The cases where the link stays at the end: nothing was told to stop it.
    let kept = matches!(
        case,
        "keep" | "transfer" | "chat" | "banner" | "error" | "xback" | "xother"
    );
    if matches!(case, "transfer" | "xback") && n != 1 {
        problems.push(format!("the account is linked ({n} accounts)"));
    }
    if kept && (count("CancelLogin") != 0 || count("Logout") != 0) {
        problems.push(format!("a login was cancelled: {commands:?}"));
    }
    // The cases with a fixture account: it stays, and the link either waits (two accounts)
    // or was cancelled (one).
    // (`drawer` replaces its only account: the replacement waits at its QR code.)
    let with_account = matches!(
        case,
        "chat"
            | "chatstop"
            | "switchstop"
            | "latest"
            | "banner"
            | "error"
            | "relink"
            | "already"
            | "already-out"
            | "drawer"
            | "draft"
            | "xadd"
            | "xother"
    );
    if with_account {
        let wanted = if matches!(case, "chat" | "banner" | "error" | "xother") {
            2
        } else {
            1
        };
        if n != wanted {
            problems.push(format!("{n} accounts, expected {wanted}"));
        }
    } else if !kept && case != "triple" && n != 0 {
        problems.push(format!("the cancelled link left {n} accounts"));
    }
    // The first account's transfer is never skipped by anything the case did.
    if matches!(case, "xadd" | "xother" | "xback") && count("SkipHistoryTransfer") != 0 {
        problems.push(format!("the transfer was skipped: {commands:?}"));
    }
    if case == "xadd" && (count("CancelLogin") != 1 || n != 1) {
        problems.push(format!(
            "Back from the second link cancels that link only ({n} accounts): {commands:?}"
        ));
    }
    if case == "xother" && (n != 2 || count("Logout") != 0) {
        problems.push(format!(
            "both accounts stay linked ({n} accounts): {commands:?}"
        ));
    }
    if matches!(case, "chatstop" | "switchstop" | "latest" | "relink") && count("CancelLogin") != 1
    {
        problems.push(format!(
            "exactly the abandoned link is cancelled once: {commands:?}"
        ));
    }
    // Cancel pressed twice cancels the one login once; three presses on a network leave one
    // login and cancel the two before it (each once, also before the core named them).
    if case == "cancel2" && count("CancelLogin") != 1 {
        problems.push(format!("a double Cancel is one CancelLogin: {commands:?}"));
    }
    if case == "triple" && (count("CancelLogin") != 2 || n > 1) {
        problems.push(format!(
            "three presses leave one login ({n} accounts): {commands:?}"
        ));
    }
    if case == "relink"
        && !account_rows(&handle)
            .iter()
            .all(|a| matches!(a.state, kourier_model::AccountState::Active))
    {
        problems.push("the relinked account is not active".to_owned());
    }
    if case == "already-out"
        && !account_rows(&handle)
            .iter()
            .all(|a| matches!(a.state, kourier_model::AccountState::LoggedOut { .. }))
    {
        problems.push("the logged-out account did not stay logged out".to_owned());
    }
    if matches!(case, "already" | "already-out") && count("Logout") == 0 {
        problems.push(format!(
            "the refused link was not logged out (the phone would keep a device): {commands:?}"
        ));
    }
    app.shutdown();
    for problem in &problems {
        eprintln!("FAIL [{case}]: {problem}");
    }
    i32::from(!problems.is_empty())
}

fn main() {
    support::answer_list("link_actions_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_LINK_ACTIONS_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_LINK_ACTIONS_ONLY").unwrap_or_default();
    let cases: Vec<&str> = [
        "keep",
        "stop",
        "add",
        "transfer",
        "chat",
        "chatstop",
        "switchstop",
        "latest",
        "banner",
        "error",
        "relink",
        "already",
        "already-out",
        "drawer",
        "cancel2",
        "triple",
        "draft",
        "xadd",
        "xother",
        "xback",
    ]
    .into_iter()
    .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
    .collect();
    let mut failed = 0;
    for case in &cases {
        let out = Command::new(&exe)
            .env("KOURIER_LINK_ACTIONS_UI_CASE", case)
            .output()
            .expect("run a case");
        let said = String::from_utf8_lossy(&out.stderr);
        print!("{}", String::from_utf8_lossy(&out.stdout));
        eprint!("{said}");
        let status = out.status;
        // No script error or failed assignment of Kirigami's or ours on the way (L5: the
        // return to the list or to a chat after "Stop Linking" printed nine and seven).
        let errors = support::script_errors(&said);
        for line in &errors {
            println!("    script error: {line}");
        }
        if status.success() && errors.is_empty() {
            println!("test link actions: {case} ... ok");
        } else {
            println!("test link actions: {case} ... FAILED ({status})");
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
