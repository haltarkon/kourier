//! The history transfer of a newly linked account in the real main window, on the real core
//! over the fake network (architecture §14.2 item 14; offscreen, `harness = false`). Each
//! case runs in a process of its own (one Qt application per process): the test binary
//! starts itself once per case.
//!
//! - `skip`: the link page closes as soon as the core has the account (link feedback L3):
//!   the transfer waits for the phone in the account banner, with "Continue Without
//!   History", and the empty list says the history is on its way. Nothing was cancelled;
//!   the button skips, the banner goes, and nothing is said about the end. The fake received
//!   one skip.
//! - `reading`: a read held in the middle: the banner counts the messages ("Importing
//!   history: 12,400 of 50,000 messages. New messages arrive afterwards.") with a bar at that share and no button, and the
//!   empty list says the history is being imported. Closing the window asks "Quit While
//!   Importing History?" (F8) with the keyboard on Cancel: Enter and Cancel keep the
//!   application open, Ctrl+Q asks the same, the confirming button quits once it can be
//!   read, and a close after that asks nothing more.
//! - `done`: a transfer that walks to its end shows waiting, downloading and reading in the
//!   banner (no button while it reads, the messages counted), says once that the history was
//!   transferred, and the fake received one dismissal.
//! - `tray`: the tray menu's Quit asks nothing, also with the window hidden in the tray
//!   during a held read (the shell says `quitting`; the question would open unseen and the
//!   quit would be refused).
//! - `session`: while the desktop session ends (the case says so) a close during a held read
//!   asks nothing and quits.
//! - `banner`: after a restart in the middle of a download there is no link page: the
//!   account banner shows it with its bytes and the skip button; the
//!   button skips, the banner goes and nothing is said.
//! - `delete`: "Delete Chat" asked while a transfer runs is refused, and the chat keeps what
//!   waits in its composer (the dialog does not watch the transfer); once the transfer is
//!   skipped the same call deletes the chat and the composer's copy goes with it.
//! - `search`: with chats in the list and a transfer running, a search that matches nothing
//!   says so ("No chats match ...") and not that the chats are on their way; clearing the
//!   search brings the list back.
//! - `failed`: a failure found at the start shows a banner that stays; "Dismiss" removes it
//!   and the fake received the dismissal. The account is untouched.
//! - `again`: "Try Again…" on the banner opens the question with the keyboard on "Cancel";
//!   Cancel and Escape change nothing, Enter does not confirm; the confirming button removes
//!   the account, a second activation does nothing more, and the link page opens with a QR
//!   code.

mod support;

use std::process::Command;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{Query, QueryData};
use kourier_app::{
    App, FixtureOptions, FixtureTransfer, HistoryTransfer, HistoryTransferFailure, Launch,
    StartOptions,
};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

// The app's own main window is the root (narrow: one page at a time).
MainWindow {
    id: win

    width: 520
    height: 760
    visible: true

    property string mode: "CASE"
    property string step: "start"
    property int waits: 0
    property int quiet: 0
    property var ev: null
    // The states the banner showed of the transfer, in order.
    property var phases: []
    // The account before it was replaced.
    property real firstAccount: 0
    // The chat the `delete` case works on.
    property real chatId: 0
    // How often the window asked the application to quit (`closedForGood`).
    property int quits: 0
    // (Only the last step may quit: a close that went through ends the application with the
    // window, with 0 whatever `fail` said; that case ends with a code of its own.)
    onClosedForGood: {
        win.quits += 1;
        if (win.step !== "quit-again" && win.step !== "tray-hidden" && win.step !== "session") {
            win.fail("quit without asking");
        }
    }
    // How often the banner showed the read with its counts.
    property int counted: 0
    // How often the question was asked for.
    property int asked: 0
    // How often an account was replaced (removed, and its new link started).
    property int replaced: 0
    // How often the question was seen before its confirming button took input.
    property int early: 0
    // The history transfer in the account banner, by its sort: "transfer" while one runs,
    // "transferFailed" for a failure that waits, "" without one.
    readonly property string transferBanner: App.bannerTransfer.phase === "failed" ? "transferFailed" : (App.bannerTransfer.phase !== "" ? "transfer" : "")

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            fail("never happened: " + what + " (login " + App.login.kind + ", transfer '" + App.bannerTransfer.phase + "', banner '" + win.transferBanner + "', accounts " + accounts.count + ")");
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.quiet = 0;
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

    // The transfer's view in the account banner.
    function bannerView(): Item {
        return win.find(win.contentItem, i => i.skipButton !== undefined);
    }

    // What the empty list says (its heading, then its explanation).
    function emptyList(): var {
        const p = win.find(win.contentItem, i => i.explanation !== undefined && i.phase !== undefined && i.visible);
        return p ? [p.text, p.explanation] : null;
    }

    // The banner of a failed transfer.
    function failure(): Item {
        return win.find(win.contentItem, i => i.retryAction !== undefined && i.dismissAction !== undefined);
    }

    // The link page is what the window shows.
    function linking(): bool {
        return win.linkPage !== null && win.pageStack.lastItem === win.linkPage;
    }

    // The question is on screen.
    function asking(): bool {
        return win.replaceDialog !== null && win.replaceDialog.opened;
    }

    // Nothing happened to the account the question was about.
    function untouched(): bool {
        return accounts.count === 1 && accounts.objectAt(0).accountId === win.firstAccount && win.transferBanner === "transferFailed" && !win.linking();
    }

    Instantiator {
        id: accounts
        model: App.accounts
        delegate: QtObject {
            required property real accountId
        }
    }

    Connections {
        target: App
        function onBannerTransferChanged(): void {
            const phase = App.bannerTransfer.phase;
            if (phase !== "" && phase !== "failed" && (win.phases.length === 0 || win.phases[win.phases.length - 1] !== phase)) {
                win.phases = win.phases.concat([phase]);
            }
        }
        function onReplaceAsked(question: var): void {
            win.asked += 1;
        }
        function onReplaceStarted(): void {
            win.replaced += 1;
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
        // ---- every case that links ----
        case "start":
            if (!win.started) return win.later("the start page");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "transfer_ui_events.qml");
            if (win.mode === "failed" || win.mode === "banner" || win.mode === "delete" || win.mode === "search") {
                if (!App.hasAccounts) return win.later("the fixture account");
                win.next(win.mode === "failed" ? "failure" : (win.mode === "banner" ? "restart" : win.mode));
                return;
            }
            if (App.hasAccounts) return win.fail("a link case starts without an account");
            win.openLink("Fake");
            win.next("input");
            return;
        case "input":
            if (App.login.kind !== "input") return win.later("the device name question");
            App.login.submit();
            win.next(win.mode);
            return;

        // ---- skip ----
        case "skip": {
            if (win.transferBanner !== "transfer" || App.bannerTransfer.phase !== "waiting") return win.later("the banner's transfer waits for the phone");
            if (win.linking()) return win.later("the link page closes once the core has the account");
            const view = win.bannerView();
            if (!view || !view.visible || !view.skipButton.visible) return win.later("the banner's view with its button");
            if (App.bannerTransfer.status !== "Waiting for your phone to prepare the message history…") return win.fail("the banner: " + App.bannerTransfer.status);
            if (view.skipButton.text !== "Continue Without History") return win.fail("the button: " + view.skipButton.text);
            if (view.skipButton.Accessible.name !== "Continue Without History") return win.fail("the button's accessible name");
            // (A transfer is not the state of an account: that banner has nothing to say.)
            if (App.bannerKind !== "" || App.bannerText !== "") return win.fail("the state's banner shows too: " + App.bannerKind);
            const said = win.emptyList();
            if (!said) return win.later("the empty list's text");
            if (said[0] !== "Your history is on its way" || said[1] !== "Chats appear here when your phone sends them.") return win.fail("the empty list says: " + said.join(" / "));
            if (accounts.count !== 1) return win.fail("the account is not in the list: " + accounts.count);
            // It stays there until somebody acts.
            win.quiet += 1;
            if (win.quiet < 12) return;
            view.skipButton.click();
            win.next("skipped");
            return;
        }
        case "skipped":
            if (win.transferBanner !== "" || win.linking() || !App.hasAccounts) return win.later("the banner goes after the skip");
            // The end of our own skip is not announced, and nothing is left of the transfer.
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.transferNotices !== 0) return win.fail("our own skip was announced");
            if (win.transferBanner === "transfer" || win.transferBanner === "transferFailed") return win.fail("a banner is left: " + win.transferBanner);
            if (win.phases.join(",") !== "waiting") return win.fail("the states shown: " + win.phases.join(","));
            if (accounts.count !== 1) return win.fail("the account stays");
            Qt.exit(0);
            return;

        // ---- reading ----
        case "reading": {
            if (win.transferBanner !== "transfer" || App.bannerTransfer.phase !== "reading") return win.later("the banner's transfer reads");
            if (win.linking()) return win.later("the link page closes once the core has the account");
            const view = win.bannerView();
            if (!view || !view.visible) return win.later("the banner's view");
            // The counts come with the delivery's first report.
            if (App.bannerTransfer.status !== "Importing history: 12,400 of 50,000 messages. New messages arrive afterwards.") return win.later("the counted messages (" + App.bannerTransfer.status + ")");
            if (!(view.progress > 0.247 && view.progress < 0.249)) return win.fail("the bar: " + view.progress);
            if (view.skipButton.visible) return win.fail("a read offers a skip");
            const said = win.emptyList();
            if (!said) return win.later("the empty list's text");
            if (said[0] !== "Importing your message history" || said[1] !== "Chats appear here as they are imported.") return win.fail("the empty list says: " + said.join(" / "));
            win.quiet += 1;
            if (win.quiet < 12) return;
            if (win.transferNotices !== 0) return win.fail("a read in the middle was announced");
            // F8: closing the window asks first; the window stays meanwhile.
            win.close();
            win.next("quit-asked");
            return;
        }
        case "quit-asked": {
            const dialog = win.quitDialog;
            if (dialog === null || !dialog.opened) return win.later("the question before quitting");
            if (win.quits !== 0) return win.fail("closing quit without asking");
            if (!win.visible) return win.fail("the window closed while it asks");
            if (dialog.title !== "Quit While Importing History?") return win.fail("the title: " + dialog.title);
            if (dialog.confirmButton.text !== "Quit" || dialog.cancelButton.text !== "Cancel") return win.fail("the buttons: " + dialog.confirmButton.text + ", " + dialog.cancelButton.text);
            if (!dialog.cancelButton.activeFocus) return win.later("the keyboard on Cancel");
            // Enter confirms nothing; Cancel keeps the application open.
            if (!win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1)) return win.fail("no key event");
            if (win.quits !== 0 || dialog.answered) return win.fail("Enter quit");
            dialog.cancelButton.click();
            win.next("quit-kept");
            return;
        }
        case "quit-kept":
            if (win.quitDialog !== null) return win.later("the question closes");
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (win.quits !== 0 || !win.visible) return win.fail("Cancel quit");
            // Ctrl+Q asks the same; the confirming button acts once it can be read.
            if (!win.ev.keyClick(Qt.Key_Q, Qt.ControlModifier, -1)) return win.fail("no key event");
            win.next("quit-again");
            return;
        case "quit-again": {
            const dialog = win.quitDialog;
            if (dialog === null || !dialog.opened) return win.later("the question after Ctrl+Q");
            if (!dialog.armed) {
                dialog.confirmButton.click();
                if (win.quits !== 0) return win.fail("quit before the question could be read");
                return;
            }
            dialog.confirmButton.click();
            if (win.quits !== 1) return win.fail("the confirming button quit " + win.quits + " times");
            // Agreed once: the close that follows asks nothing more.
            win.close();
            if (win.quits !== 2) return win.fail("asked again after the answer: " + win.quits);
            // (Its own code: a window that closed ends the application with 0.)
            Qt.exit(REACHED_THE_END);
            return;
        }

        // ---- tray: the tray menu's Quit never asks (F4) ----
        case "tray": {
            if (win.transferBanner !== "transfer" || App.bannerTransfer.phase !== "reading") return win.later("the banner's transfer reads");
            if (win.linking()) return win.later("the link page closes once the core has the account");
            if (App.bannerTransfer.status !== "Importing history: 12,400 of 50,000 messages. New messages arrive afterwards.") return win.later("the counted messages (" + App.bannerTransfer.status + ")");
            if (!App.quitQuestion()) return win.fail("a read is no reason to ask");
            // The close button hides the window in the tray (where a question could not be read).
            win.trayAvailable = true;
            win.close();
            win.next("tray-hidden");
            return;
        }
        case "tray-hidden":
            if (win.visible) return win.later("the window hides in the tray");
            if (win.quitDialog !== null || win.quits !== 0) return win.fail("the close button asked or quit");
            // The tray menu's Quit: the shell says `quitting` and quits the application, which
            // closes the hidden window too. Nothing may veto that.
            win.quitting = true;
            win.close();
            if (win.quitDialog !== null) return win.fail("the tray's Quit asked");
            if (win.quits !== 1) return win.fail("the tray's Quit did not quit: " + win.quits);
            Qt.exit(REACHED_THE_END);
            return;

        // ---- session: no question while the desktop session ends (the run said so) ----
        case "session": {
            if (win.transferBanner !== "transfer" || App.bannerTransfer.phase !== "reading") return win.later("the banner's transfer reads");
            if (win.linking()) return win.later("the link page closes once the core has the account");
            if (App.bannerTransfer.status !== "Importing history: 12,400 of 50,000 messages. New messages arrive afterwards.") return win.later("the counted messages (" + App.bannerTransfer.status + ")");
            if (App.quitQuestion() !== undefined && App.quitQuestion() !== null) return win.fail("a question while the session ends");
            win.close();
            if (win.quitDialog !== null) return win.fail("the close asked while the session ends");
            if (win.quits !== 1) return win.fail("the close did not quit while the session ends: " + win.quits);
            Qt.exit(REACHED_THE_END);
            return;
        }

        // ---- done ----
        case "done": {
            // (The page may say "Linked" a moment longer; the transfer shows in the banner only.)
            const view = win.bannerView();
            if (view && view.visible && view.phase === "downloading" && view.phase === App.bannerTransfer.phase) {
                if (!view.skipButton.visible) return win.fail("a download can be skipped");
                // "0 B" before the size is known, then "4.1 MB of 45.6 MB".
                if (view.detail === App.bannerTransfer.detail && !/^[0-9.]+ [KM]?B( of 45\.6 MB)?$/.test(view.detail)) return win.fail("the bytes: " + view.detail);
            }
            if (view && view.visible && view.phase === "reading" && view.phase === App.bannerTransfer.phase) {
                if (view.skipButton.visible) return win.fail("a read offers a skip");
                const status = App.bannerTransfer.status;
                // Before the first count the plain line, then the counted one.
                if (status !== "Importing messages… New messages arrive afterwards." && !/^Importing history: [0-9,]+ of 50,000 messages\. New messages arrive afterwards\.$/.test(status)) return win.fail("the line: " + status);
                if (status.indexOf("Importing") === 0) win.counted += 1;
            }
            if (win.transferNotices === 0 || win.linking() || !App.hasAccounts) return win.later("the transfer ends and the link finishes");
            win.next("said");
            return;
        }
        case "said":
            // Once, however often the end is reported, and nothing stays.
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.transferNotices !== 1) return win.fail("the end was announced " + win.transferNotices + " times");
            // "updating" (the link's tail after the import, L3) may show for a moment when the
            // machine is loaded: the fake's login ends right after the transfer, but the
            // banner can catch the step between (failed once that way in the L6 sweep).
            const shown = win.phases.join(",");
            if (shown !== "waiting,downloading,reading" && shown !== "waiting,downloading,reading,updating") return win.fail("the states shown: " + shown);
            if (win.counted === 0) return win.fail("the read was never counted");
            if (win.transferBanner === "transfer" || win.transferBanner === "transferFailed") return win.fail("a banner is left: " + win.transferBanner);
            Qt.exit(0);
            return;

        // ---- banner ----
        case "restart": {
            if (win.transferBanner !== "transfer") return win.later("the banner of the running transfer");
            const view = win.find(win.contentItem, i => i.skipButton !== undefined);
            if (!view || !view.visible || !view.skipButton.visible) return win.later("the banner's view with its button");
            if (win.linkPage !== null) return win.fail("a link page after a restart");
            if (App.bannerTransfer.status !== "Downloading message history…") return win.fail("the line: " + App.bannerTransfer.status);
            if (view.view.status !== App.bannerTransfer.status || view.phase !== "downloading") return win.fail("the view shows another state: " + view.phase);
            if (view.detail !== "12.3 MB of 45.6 MB") return win.fail("the bytes: " + view.detail);
            if (!(view.progress > 0.26 && view.progress < 0.28)) return win.fail("the bar: " + view.progress);
            if (view.skipButton.text !== "Continue Without History") return win.fail("the button: " + view.skipButton.text);
            win.quiet += 1;
            if (win.quiet < 12) return;
            view.skipButton.click();
            win.next("restart-skipped");
            return;
        }
        case "restart-skipped":
            if (win.transferBanner === "transfer") return win.later("the banner goes after the skip");
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.transferBanner === "transfer" || win.transferBanner === "transferFailed") return win.fail("a banner is left: " + win.transferBanner);
            if (win.transferNotices !== 0) return win.fail("our own skip was announced");
            if (accounts.count !== 1) return win.fail("the account is untouched");
            Qt.exit(0);
            return;

        // ---- delete: a refused Delete Chat keeps the composer's files (F2) ----
        case "delete": {
            if (win.transferBanner !== "transfer") return win.later("the banner of the running transfer");
            win.chatId = App.conversationAt(0);
            if (win.chatId === 0) return win.later("the chat list");
            App.openConversation(win.chatId);
            win.next("delete-open");
            return;
        }
        case "delete-open": {
            if (App.timeline.conversationId !== win.chatId) return win.later("the chat opens");
            if (App.timeline.stillFilling === "") return win.later("the chat says it still fills");
            if (!App.composer.addFile(FILE)) return win.fail("the file was not added: " + App.composer.notice);
            if (App.composer.attachmentCount !== 1) return win.fail("strip count " + App.composer.attachmentCount);
            // The question was opened before the transfer began; this is its confirming button.
            App.deleteConversation();
            win.next("delete-refused");
            return;
        }
        case "delete-refused": {
            win.quiet += 1;
            if (win.quiet < 12) return;
            if (App.composer.attachmentCount !== 1) return win.fail("a refused Delete Chat took the files from the composer: " + App.composer.attachmentCount);
            if (App.timeline.conversationId !== win.chatId || App.conversationAt(0) !== win.chatId) return win.fail("a refused Delete Chat removed the chat");
            const view = win.bannerView();
            if (!view || !view.visible || !view.skipButton.visible) return win.later("the banner's button");
            view.skipButton.click();
            win.next("delete-skipped");
            return;
        }
        case "delete-skipped": {
            if (win.transferBanner !== "" || App.timeline.stillFilling !== "") return win.later("the transfer is over");
            App.deleteConversation();
            win.next("delete-gone");
            return;
        }
        case "delete-gone":
            if (App.conversationAt(0) === win.chatId) return win.later("the chat is deleted");
            if (App.composer.attachmentCount !== 0) return win.fail("the composer kept the files of a deleted chat: " + App.composer.attachmentCount);
            Qt.exit(0);
            return;

        // ---- search: a search without a match says so, not that the chats are coming (M1) ----
        case "search": {
            if (win.transferBanner !== "transfer") return win.later("the banner of the running transfer");
            const list = win.pageStack.get(0);
            if (!list || list.searchField === undefined) return win.later("the list page");
            if (App.conversationAt(0) === 0) return win.later("the chat list");
            if (win.emptyList() !== null) return win.fail("an empty list over a full one: " + win.emptyList().join(" / "));
            list.searchField.text = "  zzqx <b>no</b>  ";
            win.next("search-none");
            return;
        }
        case "search-none": {
            const said = win.emptyList();
            if (!said) return win.later("the empty list's text");
            // The words around the search are not part of it, and no markup is made of it.
            if (said[0] !== "No chats match “zzqx ‹b›no‹/b›”") return win.fail("the heading: " + said[0]);
            if (said[1] !== "") return win.fail("the explanation: " + said[1]);
            win.pageStack.get(0).searchField.text = "";
            win.next("search-cleared");
            return;
        }
        case "search-cleared":
            if (App.conversationAt(0) === 0) return win.later("the list comes back");
            if (win.emptyList() !== null) return win.later("the placeholder goes");
            Qt.exit(0);
            return;

        // ---- failed ----
        case "failure": {
            if (win.transferBanner !== "transferFailed") return win.later("the banner of the failure");
            const banner = win.failure();
            if (!banner || !banner.visible) return win.later("the banner's item");
            if (App.bannerTransfer.status !== "The message history arrived damaged and was not imported. You can transfer it again.") return win.fail("the sentence: " + App.bannerTransfer.status);
            if (!banner.retryAction.visible || banner.retryAction.text !== "Try Again…") return win.fail("Try Again…: " + banner.retryAction.text);
            if (!banner.dismissAction.visible || banner.dismissAction.text !== "Dismiss") return win.fail("Dismiss: " + banner.dismissAction.text);
            if (win.transferNotices !== 0) return win.fail("a failure is a banner, not a notice");
            // It stays.
            win.quiet += 1;
            if (win.quiet < 15) return;
            banner.dismissAction.trigger();
            win.next("dismissed");
            return;
        }
        case "dismissed":
            if (win.transferBanner === "transferFailed") return win.later("the banner goes");
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.transferBanner === "transferFailed") return win.fail("the banner came back");
            if (accounts.count !== 1) return win.fail("the account is untouched");
            if (win.asked !== 0 || win.transferNotices !== 0) return win.fail("nothing else happened");
            Qt.exit(0);
            return;

        // ---- again ----
        case "again":
            if (win.linking() || !App.hasAccounts || win.transferBanner !== "transferFailed" || accounts.count !== 1) return win.later("the link finishes and the failure shows");
            if (!win.failure()) return win.later("the banner's item");
            win.firstAccount = accounts.objectAt(0).accountId;
            win.failure().retryAction.trigger();
            win.next("cancel");
            return;
        case "cancel": {
            if (!win.asking()) return win.later("the question");
            const dialog = win.replaceDialog;
            if (win.asked !== 1) return win.fail("asked " + win.asked + " times");
            if (dialog.title !== "Remove and Link Again?") return win.fail("the title: " + dialog.title);
            if (!dialog.confirmButton || dialog.confirmButton.text !== "Remove and Link Again") return win.fail("the confirming button");
            if (!dialog.cancelButton || dialog.cancelButton.text !== "Cancel") return win.fail("the cancelling button");
            // The default is to cancel: the keyboard starts there.
            if (!dialog.cancelButton.activeFocus) return win.later("the keyboard on Cancel");
            if (dialog.confirmButton.activeFocus) return win.fail("the keyboard starts on the confirming button");
            const said = dialog.question.paragraphs.join(" ");
            for (const part of ["removes this account's chats, messages and files", "links the account again", "exists only on this computer is lost", "older attachments may not be transferred", "You need your phone to link again", "the account stays removed"]) {
                if (said.indexOf(part) < 0) return win.fail("the question does not say: " + part);
            }
            dialog.cancelButton.click();
            win.next("cancelled");
            return;
        }
        case "cancelled":
            if (win.replaceDialog !== null) return win.later("the question closes");
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (!win.untouched()) return win.fail("Cancel changed something");
            win.failure().retryAction.trigger();
            win.next("escape");
            return;
        case "escape":
            if (!win.asking()) return win.later("the question, again");
            if (!win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("escaped");
            return;
        case "escaped":
            if (win.replaceDialog !== null) return win.later("Escape closes the question");
            win.quiet += 1;
            if (win.quiet < 10) return;
            if (!win.untouched()) return win.fail("Escape changed something");
            win.failure().retryAction.trigger();
            win.next("enter");
            return;
        case "enter":
            if (!win.asking() || !win.replaceDialog.cancelButton.activeFocus) return win.later("the question, a third time");
            if (!win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1) || !win.ev.keyClick(Qt.Key_Enter, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("entered");
            return;
        case "entered":
            // Enter confirms nothing, whatever else it does.
            win.quiet += 1;
            if (win.quiet < 15) return;
            if (!win.untouched()) return win.fail("Enter confirmed the question");
            if (win.replaceDialog !== null && win.replaceDialog.answered) return win.fail("Enter answered the question");
            if (win.replaceDialog === null) {
                win.failure().retryAction.trigger();
            }
            win.next("confirm");
            return;
        case "confirm": {
            if (!win.asking()) return win.later("the question, to confirm it");
            const dialog = win.replaceDialog;
            // The question was just opened: its confirming button does nothing yet (the
            // second click of a double click must not confirm), by the pointer or by its
            // handler.
            if (!dialog.armed) {
                dialog.confirmButton.click();
                dialog.confirm();
                if (dialog.answered || !win.untouched()) return win.fail("the question was confirmed before it could be read");
                win.early += 1;
                return;
            }
            if (win.early === 0) return win.fail("the question was never seen before its confirming button acted");
            dialog.confirmButton.click();
            // A second activation, of the button's own handler and of the command behind
            // it: nothing more happens.
            dialog.confirm();
            App.confirmReplace(win.firstAccount);
            win.next("replaced");
            return;
        }
        case "replaced":
            if (!win.linking() || App.login.kind !== "input") return win.later("the link page of the new link");
            if (win.replaceDialog !== null && win.replaceDialog.opened) return win.fail("the question stays open");
            App.login.submit();
            win.next("qr");
            return;
        case "qr":
            if (App.login.kind !== "qr" || App.login.qr === "") return win.later("the QR code");
            if (!win.linking()) return win.fail("the link page shows the code");
            // The account the question was about is gone from this computer.
            for (let i = 0; i < accounts.count; ++i) {
                if (accounts.objectAt(i).accountId === win.firstAccount) return win.later("the account is removed");
            }
            // (Counted here: "one account is left" would not see a second removal and link.)
            if (win.replaced !== 1) return win.fail("the account was replaced " + win.replaced + " times");
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 150000
        running: true
        onTriggered: win.fail("timeout")
    }
}
"##;

/// What the `reading` case exits with when it reached its end.
const REACHED_THE_END: i32 = 5;

fn options(case: &str) -> FixtureOptions {
    let base = FixtureOptions {
        messages_per_conversation: 6,
        qr_valid_for: Duration::from_millis(200),
        qr_codes: 1,
        transfer_stage: Duration::from_millis(600),
        ..FixtureOptions::default()
    };
    match case {
        "skip" => FixtureOptions {
            link: true,
            transfer: Some(FixtureTransfer::Hold(HistoryTransfer::Waiting)),
            ..base
        },
        "reading" | "tray" | "session" => FixtureOptions {
            link: true,
            transfer: Some(FixtureTransfer::Hold(HistoryTransfer::Reading)),
            ..base
        },
        "done" => FixtureOptions {
            link: true,
            transfer: Some(FixtureTransfer::Walk(HistoryTransfer::Done)),
            ..base
        },
        "banner" | "delete" | "search" => FixtureOptions {
            transfer: Some(FixtureTransfer::Left(HistoryTransfer::Downloading {
                done: 12_300_000,
                total: Some(45_600_000),
            })),
            ..base
        },
        "failed" => FixtureOptions {
            transfer: Some(FixtureTransfer::Left(HistoryTransfer::Failed(
                HistoryTransferFailure::Corrupt,
            ))),
            ..base
        },
        "again" => FixtureOptions {
            link: true,
            // The second code is the one that is scanned: the first stays on screen.
            qr_codes: 2,
            qr_valid_for: Duration::from_secs(3),
            transfer: Some(FixtureTransfer::Walk(HistoryTransfer::Failed(
                HistoryTransferFailure::Download,
            ))),
            ..base
        },
        other => panic!("unknown case {other}"),
    }
}

/// The accounts the core has.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn accounts(app: &kourier_app::AppHandle) -> usize {
    match app.runtime().block_on(app.query(Query::Accounts)) {
        Ok(QueryData::Accounts(accounts)) => accounts.len(),
        _ => panic!("the accounts"),
    }
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
    // No session manager is there to say it: the case says the session ends.
    kourier_qt::set_session_ending(case == "session");
    // A small file for the composer (the `delete` case).
    let file = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("transfer_ui_sample.txt");
    std::fs::write(&file, b"waiting in the composer").expect("a sample file");
    let script = SCRIPT
        .replace("CASE", case)
        .replace("FILE", &format!("{:?}", kourier_qt::app::file_url(&file)))
        .replace("REACHED_THE_END", &REACHED_THE_END.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/transfer_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    // `reading`, `tray` and `session` close the window: only their own code says they
    // reached their end.
    let code = match (case, code) {
        ("reading" | "tray" | "session", REACHED_THE_END) => 0,
        ("reading" | "tray" | "session", 0) => 1,
        (_, code) => code,
    };
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        app.shutdown();
        return code;
    }

    // What the fake network was told, and what the core holds.
    let commands = handle.fixture_commands();
    let count = |name: &str| commands.iter().filter(|c| **c == name).count();
    let (skips, dismissals) = (
        count("SkipHistoryTransfer"),
        count("DismissHistoryTransfer"),
    );
    let mut problems = Vec::new();
    let mut expect = |what: &str, ok: bool| {
        if !ok {
            problems.push(format!(
                "{what} (skips {skips}, dismissals {dismissals}, accounts {})",
                accounts(&handle)
            ));
        }
    };
    match case {
        "skip" => {
            expect("one skip reached the backend", skips == 1);
            expect("the end of the skip was dismissed", dismissals >= 1);
            expect(
                "no login was cancelled",
                count("CancelLogin") == 0 && count("Logout") == 0,
            );
            expect("the account is linked", accounts(&handle) == 1);
        }
        "reading" | "tray" | "session" => {
            expect("nothing was skipped", skips == 0);
            expect(
                "no login was cancelled",
                count("CancelLogin") == 0 && count("Logout") == 0,
            );
            expect("the account is linked", accounts(&handle) == 1);
        }
        "done" => {
            expect("nothing was skipped", skips == 0);
            expect("one dismissal reached the backend", dismissals == 1);
            expect("the account is linked", accounts(&handle) == 1);
        }
        "search" => {
            expect("nothing was skipped", skips == 0);
            expect("the account is untouched", accounts(&handle) == 1);
        }
        "banner" | "delete" => {
            expect("one skip reached the backend", skips == 1);
            expect("the end of the skip was dismissed", dismissals >= 1);
            expect("the account is untouched", accounts(&handle) == 1);
        }
        "failed" => {
            expect("nothing was skipped", skips == 0);
            expect("one dismissal reached the backend", dismissals == 1);
            expect("the account is untouched", accounts(&handle) == 1);
        }
        "again" => {
            expect("the failure was not dismissed", dismissals == 0);
            // The old account is removed and one new account is being linked: a second
            // activation added no other.
            expect("one account is left", accounts(&handle) == 1);
        }
        _ => {}
    }
    app.shutdown();
    for problem in &problems {
        eprintln!("FAIL [{case}]: {problem}");
    }
    i32::from(!problems.is_empty())
}

fn main() {
    support::answer_list("transfer_ui");
    // Never on the user's desktop, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // A child: one case, then exit with its code.
    if let Ok(case) = std::env::var("KOURIER_TRANSFER_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let cases = [
        "skip", "reading", "tray", "session", "done", "banner", "delete", "search", "failed",
        "again",
    ];
    let mut failed = 0;
    for case in cases {
        let status = Command::new(&exe)
            .env("KOURIER_TRANSFER_UI_CASE", case)
            .status()
            .expect("run a case");
        if status.success() {
            println!("test history transfer: {case} ... ok");
        } else {
            println!("test history transfer: {case} ... FAILED ({status})");
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
