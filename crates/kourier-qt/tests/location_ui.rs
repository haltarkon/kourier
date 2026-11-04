//! The question before files that would leave with where they were taken, through the real
//! main window and the real core over the Signal-like fake (offscreen, `harness = false`).
//! Files from `sample_located` (a made-up place in the open ocean). One session of a user:
//!
//! - The first question (a video with a text file) answered by keys only, Tab from the tick
//!   box and Space on "Send with Location": all goes, the focus is back in the text area (UR7).
//! - A phone JPEG to the Signal-like chat does not ask (Signal cleans its pictures).
//! - A video with a text file: Send (Return in the text) asks. The dialog says his sentence,
//!   the focus is on the tick box (other files would still go) and Space there ticks it and
//!   answers nothing (F2; Return answers nothing anywhere in this dialog, U6); the body is every control's `Accessible.description` (F15); the key filter
//!   lets no auto-repeat and no release of an unseen press through; `send()` while it asks is
//!   a no-op.
//! - The footer one pixel wider than the width where the answers stack: side by side, neither
//!   answer smaller than its size or past the footer's edge (RF2).
//! - Escape: the dialog goes and the question with it (L3), nothing is sent, Remember is not
//!   applied (F23), the text and the files stay, and the dialog does not come back by itself;
//!   the next Send asks again and the answer sends that press's text. Don't Send keeps the
//!   video, marked, and sends the rest with the text.
//! - A lone located file with a caption: the focus is on Don't Send; Return there answers
//!   nothing, Space there is Don't Send and sends nothing (U6).
//! - A lone located video with no text, Don't Send: Send stays on (a held file counts under
//!   Ask), the next Send asks again, the row says the way back; under Always the press sends
//!   it without a question (U3).
//! - A question that goes stale (a second located file joins while it is open) with Remember
//!   ticked: nothing is remembered, exactly one more question opens (U1).
//! - Remember + Send with Location, by keys only (Space ticks, Tab, Space): the setting
//!   becomes Always (through Settings, F1), the focus is back in the text area (UR7), and the
//!   next located file goes without a question. Remember + Don't Send: Never; the tray toggle
//!   keeps it (F1).
//! - The setting changed while files wait: the rows and the counts follow at once (L5).
//! - A chat switch closes the question; the text stays there as the draft (F6).
//! - A caption over 500 characters with Return within 300 ms: asked once, the whole caption
//!   goes (BQ-R1).

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{
    OriginKind, Query, QueryData, SampleLocated, TimelineAnchor, TimelineQuery, sample_located,
};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_model::{Body, ConversationId};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

MainWindow {
    id: win

    width: 520
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property int quiet: 0
    property int askedTimes: 0
    property var shown: null
    property string longText: ""

    readonly property var composerItem: win.chatPage ? win.chatPage.composer : null
    readonly property var dialog: win.composerItem ? win.composerItem.locationDialog : null
    readonly property var question: App.composer.locationQuestion

    readonly property string located: "Location and device data stay in this file"
    readonly property string willNot: "Will not be sent: it holds where it was taken (Settings: Files with location data)"
    readonly property string notSent: "Not sent: it holds where it was taken (Settings: Files with location data)"
    readonly property string heldAsk: "Not sent: it holds where it was taken. Send asks again."
    readonly property string heldAlways: "Not sent: it holds where it was taken. Send now sends it."

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

    // An item of the open dialog, by its object name.
    function asked(name: string): Item {
        return win.dialog ? win.find(win.dialog.contentItem, i => i.objectName === name) : null;
    }

    function shownOpen(): bool {
        return App.composer.locationAsking && win.dialog !== null && win.dialog.opened;
    }

    Connections {
        target: App.composer
        function onLocationAskingChanged(): void {
            if (App.composer.locationAsking) {
                win.askedTimes += 1;
            }
        }
    }

    Instantiator {
        id: fileRows
        model: App.composer.files
        delegate: QtObject {
            required property string name
            required property string reason
            required property string note
        }
    }

    function row(i: int): var {
        return fileRows.objectAt(i);
    }

    // `n` files in the list, planned, each verdict known, `sendable` of them sendable now.
    function settled(n: int, sendable: int): bool {
        return App.composer.attachmentCount === n && fileRows.count === n && App.composer.planned && App.composer.sendableCount === sendable;
    }

    // Starts a press of Send with `text` in the text area (Return, as he types it).
    function pressSend(text: string): void {
        win.composerItem.setText(text);
        win.composerItem.focusInput();
        win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
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
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "location_ui_events.qml");
            win.openConversation(MEDIA);
            win.next("chat");
            return;
        case "chat": {
            if (win.chatPage === null || App.timeline.conversationId !== MEDIA || App.timeline.loading || App.timeline.attachKinds.indexOf("file") < 0) return win.later("the media chat");
            const button = win.find(win.composerItem, i => i.objectName === "attachButton");
            if (!button || !button.enabled) return win.later("the composer takes input");
            if (App.settings.locationFiles !== 0) return win.fail("the setting starts at Ask: " + App.settings.locationFiles);
            // ---- the first question, answered by keys only: Tab + Space on "Send with
            // Location"; the focus is back in the text area after it (UR7) ----
            App.composer.addFiles([MP4, TXT]);
            win.next("keysLooked");
            return;
        }
        case "keysLooked":
            if (!win.settled(2, 2)) return win.later("the files for the keys");
            win.pressSend("keys");
            win.next("keysAsked");
            return;
        case "keysAsked":
            if (!win.shownOpen()) return win.later("the question for the keys");
            if (!win.dialog.rememberBox.activeFocus) return win.fail("the focus is not on the tick box");
            if (!win.ev.keyClick(Qt.Key_Tab, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("keysTabbed");
            return;
        case "keysTabbed":
            if (!win.dialog.sendButton.activeFocus) return win.fail("Tab from the tick box is not on Send with Location");
            if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("keysSent");
            return;
        case "keysSent":
            if (App.composer.attachmentCount !== 0 || win.composerItem.text !== "") return win.later("Send with Location sends");
            if (win.dialog !== null || App.composer.locationAsking) return win.later("the answered dialog goes");
            // At once, not only once the dialog's closing transition gave the focus back (Qt
            // does that about 160 ms later: what he types meanwhile went nowhere).
            if (!win.composerItem.textArea.activeFocus) return win.fail("after Send with Location the focus is not in the text area: " + win.activeFocusItem);
            if (App.settings.locationFiles !== 0) return win.fail("an answer without Remember changed the setting");
            win.askedTimes = 0;
            // ---- a phone JPEG to the Signal-like chat: Signal cleans it, no question ----
            App.composer.addFiles([PHONE]);
            win.next("phone");
            return;
        case "phone":
            if (!win.settled(1, 1)) return win.later("the phone photo is looked at");
            if (win.row(0).note !== "" || win.row(0).reason !== "") return win.fail("the phone photo's row: " + win.row(0).note + win.row(0).reason);
            win.pressSend("a phone photo");
            win.next("phoneSent");
            return;
        case "phoneSent":
            if (App.composer.locationAsking || win.dialog !== null) return win.fail("the phone photo was asked about");
            if (App.composer.attachmentCount !== 0) return win.later("the phone photo goes");
            if (win.askedTimes !== 0) return win.fail("asked: " + win.askedTimes);
            // ---- a video and a text file ----
            App.composer.addFiles([MP4, TXT]);
            win.next("videoLooked");
            return;
        case "videoLooked":
            if (!win.settled(2, 2)) return win.later("the video is looked at");
            if (win.row(0).note !== win.located || win.row(0).reason !== "") return win.fail("the video's row: " + win.row(0).note + " / " + win.row(0).reason);
            if (win.row(1).note !== "") return win.fail("the text file's row: " + win.row(1).note);
            win.pressSend("first text");
            win.next("asked");
            return;
        case "asked": {
            if (!win.shownOpen()) return win.later("the question");
            if (win.askedTimes !== 1) return win.fail("asked: " + win.askedTimes);
            win.shown = win.dialog;
            // His sentence, by kind; the name; what still goes; the hint.
            if (win.dialog.title !== "Send It Anyway?") return win.fail("the title: " + win.dialog.title);
            if (win.asked("locationText").text !== "This video contains where it was taken.") return win.fail("the text: " + win.asked("locationText").text);
            if (win.asked("locationStillGoes").text !== "The other files and the text are sent without it.") return win.fail("still goes: " + win.asked("locationStillGoes").text);
            if (win.asked("locationMore").visible) return win.fail("'and more' for one file");
            if (win.asked("locationSettingsHint").text !== "You can change this in Settings.") return win.fail("the hint");
            if (win.dialog.names.length !== 1 || win.dialog.names[0] !== "trip.mp4") return win.fail("the names: " + win.dialog.names);
            if (win.dialog.sendButton.text !== "Send with Location" || win.dialog.dontSendButton.text !== "Don't Send") return win.fail("the buttons");
            // Other files would still go: the focus on the tick box, not on an answer (F2).
            if (win.question.focusDontSend !== false) return win.fail("focusDontSend with other files");
            if (!win.dialog.rememberBox.activeFocus) return win.fail("the focus is not on the tick box");
            // ... and it shows there: the style's own cue (UR5; Breeze draws it in an active
            // window, offscreen draws none).
            if (!win.dialog.rememberBox.visualFocus) return win.fail("no visual focus on the tick box");
            // The body for screen readers (F15).
            const body = win.question.accessible;
            if (body === "" || body.indexOf("This video contains where it was taken.") < 0) return win.fail("the accessible body: " + body);
            if (win.dialog.rememberBox.Accessible.description !== body || win.dialog.dontSendButton.Accessible.description !== body || win.dialog.sendButton.Accessible.description !== body) return win.fail("Accessible.description");
            // The text that pressed Send stays until an answer sends it.
            if (win.composerItem.text !== "first text") return win.fail("the text after the press: " + win.composerItem.text);
            // The key filter (qml.md "A held key and a QQC2.Button"), with stand-in events.
            const repeat = { key: Qt.Key_Space, isAutoRepeat: true, accepted: false };
            win.dialog.filterPress(repeat);
            if (!repeat.accepted) return win.fail("an auto-repeat went through");
            const unseen = { key: Qt.Key_A, isAutoRepeat: false, accepted: false };
            win.dialog.filterRelease(unseen);
            if (!unseen.accepted) return win.fail("the release of an unseen press went through");
            const tab = { key: Qt.Key_Tab, isAutoRepeat: true, accepted: false };
            win.dialog.filterPress(tab);
            if (tab.accepted) return win.fail("Tab was stopped");
            const press = { key: Qt.Key_B, isAutoRepeat: false, accepted: false };
            win.dialog.filterPress(press);
            const release = { key: Qt.Key_B, isAutoRepeat: false, accepted: false };
            win.dialog.filterRelease(release);
            if (press.accepted || release.accepted) return win.fail("a seen press and its release were stopped");
            // Return answers nothing in this dialog (QQC2 buttons click only with Space). Space
            // where F2 puts the focus, on the tick box: it ticks the box and answers nothing (U6).
            if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("spaced");
            return;
        }
        case "spaced":
            if (++win.quiet < 8) return;
            if (!win.shownOpen() || win.dialog !== win.shown) return win.fail("Space on the tick box closed the question");
            if (!win.dialog.rememberBox.checked) return win.fail("Space on the tick box did not tick it");
            if (App.composer.attachmentCount !== 2 || win.composerItem.text !== "first text") return win.fail("Space on the tick box sent: " + App.composer.attachmentCount);
            if (App.settings.locationFiles !== 0) return win.fail("Space on the tick box changed the setting");
            win.dialog.rememberBox.checked = false;
            // A second Send while it asks: nothing.
            win.composerItem.send();
            win.next("sentTwice");
            return;
        case "sentTwice":
            if (++win.quiet < 5) return;
            if (!win.shownOpen() || win.dialog !== win.shown || win.askedTimes !== 1) return win.fail("send() while asking did something");
            if (App.composer.attachmentCount !== 2 || win.composerItem.text !== "first text") return win.fail("send() while asking sent");
            win.next("bandSet");
            return;
        case "bandSet": {
            // ---- the footer one pixel above the width where the answers stack (RF2): the row is
            // side by side and each button keeps its size (the spacer takes a spacing too) ----
            const footer = win.dialog.footer;
            const want = footer.sideBySide + 1;
            if (footer.availableWidth !== want) {
                if (++win.quiet > 20) return win.fail("the footer's width cannot be set: " + footer.availableWidth + " / " + want);
                win.dialog.width += want - footer.availableWidth;
                return;
            }
            win.next("bandChecked");
            return;
        }
        case "bandChecked": {
            if (++win.quiet < 3) return;
            const footer = win.dialog.footer;
            if (footer.availableWidth !== footer.sideBySide + 1) return win.fail("the footer's width moved: " + footer.availableWidth);
            if (footer.stacked) return win.fail("stacked at sideBySide + 1");
            const send = win.dialog.sendButton;
            const hold = win.dialog.dontSendButton;
            if (send.width < send.implicitWidth || hold.width < hold.implicitWidth) return win.fail("an answer is squeezed: " + send.width + "/" + send.implicitWidth + ", " + hold.width + "/" + hold.implicitWidth);
            // The layout does not shrink a Breeze button below its size: a row that is too wide
            // runs past the footer's edge instead. Both answers lie inside it (1 px for the pixel snapping).
            const grid = footer.contentItem;
            if (send.mapToItem(grid, 0, 0).x < -1 || hold.mapToItem(grid, 0, 0).x + hold.width > grid.width + 1) return win.fail("an answer runs past the footer: " + hold.mapToItem(grid, 0, 0).x + " + " + hold.width + " > " + grid.width);
            // The dialog's own width again (its default is its implicit width).
            win.dialog.width = Qt.binding(() => win.dialog.implicitWidth);
            win.next("bandDone");
            return;
        }
        case "bandDone":
            if (++win.quiet < 3) return;
            // ---- Escape, with Remember ticked: nothing (F23, L3) ----
            win.dialog.rememberBox.checked = true;
            if (!win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("escaped");
            return;
        case "escaped":
            if (win.dialog !== null || App.composer.locationAsking) return win.later("Escape dismisses the question");
            if (App.settings.locationFiles !== 0) return win.fail("Escape applied Remember: " + App.settings.locationFiles);
            if (App.composer.attachmentCount !== 2 || win.composerItem.text !== "first text") return win.fail("Escape lost the files or the text");
            if (win.row(0).reason !== "") return win.fail("Escape held the video: " + win.row(0).reason);
            // What the composer does when it can be written to again: no dialog comes back.
            win.composerItem.syncLocationDialog();
            win.next("staysGone");
            return;
        case "staysGone":
            if (++win.quiet < 5) return;
            if (win.dialog !== null || App.composer.locationAsking) return win.fail("the dismissed question came back");
            // The next Send asks again, with the new text.
            win.pressSend("second text");
            win.next("askedAgain");
            return;
        case "askedAgain":
            if (!win.shownOpen()) return win.later("the question again");
            if (win.askedTimes !== 2) return win.fail("asked: " + win.askedTimes);
            if (win.dialog.rememberBox.checked) return win.fail("a new question starts unticked");
            win.dialog.dontSendButton.click();
            win.next("held");
            return;
        case "held":
            // Don't Send: the text file and the text go, the video stays, marked.
            if (App.composer.attachmentCount !== 1 || win.composerItem.text !== "") return win.later("Don't Send sends the rest");
            if (App.composer.locationAsking || win.dialog !== null) return win.fail("the question stays");
            if (win.row(0).name !== "trip.mp4" || win.row(0).reason !== win.heldAsk) return win.fail("the held video's row: " + win.row(0).name + " / " + win.row(0).reason);
            // A held file counts under Ask: the next Send asks about it again (U3).
            if (App.composer.sendableCount !== 1) return win.fail("the held video does not count: " + App.composer.sendableCount);
            App.composer.clearAttachments();
            // ---- a lone located file with a caption: Don't Send sends nothing ----
            App.composer.addFiles([MOV]);
            win.next("loneLooked");
            return;
        case "loneLooked":
            if (!win.settled(1, 1)) return win.later("the lone video is looked at");
            win.pressSend("lone caption");
            win.next("loneAsked");
            return;
        case "loneAsked":
            if (!win.shownOpen()) return win.later("the lone question");
            if (win.question.focusDontSend !== true) return win.fail("focusDontSend for a lone file");
            if (!win.dialog.dontSendButton.activeFocus) return win.fail("the focus is not on Don't Send");
            if (win.asked("locationStillGoes").visible) return win.fail("still goes for a lone file");
            // Return on the focused answer answers nothing (U6) ...
            if (!win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("loneReturned");
            return;
        case "loneReturned":
            if (++win.quiet < 8) return;
            if (!win.shownOpen() || App.composer.attachmentCount !== 1 || win.composerItem.text !== "lone caption") return win.fail("Return on Don't Send answered");
            // ... Space on it, where F2 puts the focus, is "Don't Send": nothing goes.
            if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("loneHeld");
            return;
        case "loneHeld":
            if (App.composer.locationAsking || win.dialog !== null) return win.later("the lone answer");
            if (++win.quiet < 5) return;
            if (App.composer.attachmentCount !== 1 || win.composerItem.text !== "lone caption") return win.fail("the lone answer sent: " + App.composer.attachmentCount + " " + win.composerItem.text);
            App.composer.clearAttachments();
            win.composerItem.setText("");
            // ---- U3: a lone located video and no text, answered Don't Send: Send stays on ----
            App.composer.addFiles([MOV]);
            win.next("heldLoneLooked");
            return;
        case "heldLoneLooked":
            if (!win.settled(1, 1)) return win.later("the lone video for the held file");
            win.pressSend("");
            win.next("heldLoneAsked");
            return;
        case "heldLoneAsked":
            if (!win.shownOpen()) return win.later("the question for the held file");
            win.dialog.dontSendButton.click();
            win.next("heldLone");
            return;
        case "heldLone":
            if (App.composer.locationAsking || win.dialog !== null) return win.later("the answer for the held file");
            if (++win.quiet < 5) return;
            if (App.composer.attachmentCount !== 1 || win.composerItem.text !== "") return win.fail("the held answer sent: " + App.composer.attachmentCount);
            if (win.row(0).reason !== win.heldAsk) return win.fail("the held row says: " + win.row(0).reason);
            if (App.composer.sendableCount !== 1) return win.fail("Send is dead for a held file with no text: " + App.composer.sendableCount);
            // The next Send asks again about it.
            win.askedTimes = 0;
            win.pressSend("");
            win.next("heldLoneAgain");
            return;
        case "heldLoneAgain":
            if (!win.shownOpen()) return win.later("Send asks again about the held file");
            if (win.askedTimes !== 1 || win.question.count !== 1) return win.fail("asked: " + win.askedTimes + " / " + win.question.count);
            if (!win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("heldLoneEscaped");
            return;
        case "heldLoneEscaped":
            if (win.dialog !== null || App.composer.locationAsking) return win.later("Escape dismisses the question again");
            if (App.composer.attachmentCount !== 1 || win.row(0).reason !== win.heldAsk) return win.fail("Escape changed the held file: " + win.row(0).reason);
            // Always: the press lets the held file go, without a question.
            App.settings.locationFiles = 1;
            if (win.row(0).reason !== win.heldAlways) return win.fail("the held row under Always: " + win.row(0).reason);
            if (App.composer.sendableCount !== 1) return win.fail("Send is dead under Always: " + App.composer.sendableCount);
            win.pressSend("");
            win.next("heldLoneAlways");
            return;
        case "heldLoneAlways":
            if (App.composer.locationAsking || win.dialog !== null) return win.fail("Always asked about the held file");
            if (App.composer.attachmentCount !== 0) return win.later("Always sends the held file");
            App.settings.locationFiles = 0;
            // ---- Remember + Send with Location, by keys only: Always (F1), the focus back (UR7) ----
            App.composer.addFiles([MP4, TXT]);
            win.next("alwaysLooked");
            return;
        case "alwaysLooked":
            if (!win.settled(2, 2)) return win.later("the files for Always");
            win.pressSend("always");
            win.next("alwaysAsked");
            return;
        case "alwaysAsked":
            if (!win.shownOpen()) return win.later("the question for Always");
            if (!win.dialog.rememberBox.activeFocus) return win.fail("the focus is not on the tick box");
            // Space ticks the box, Tab goes on to "Send with Location".
            if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
            if (!win.dialog.rememberBox.checked) return win.fail("Space did not tick the box");
            if (!win.ev.keyClick(Qt.Key_Tab, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("alwaysTabbed");
            return;
        case "alwaysTabbed":
            if (!win.dialog.sendButton.activeFocus) return win.fail("Tab from the tick box is not on Send with Location");
            if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("always");
            return;
        case "always":
            if (App.composer.attachmentCount !== 0 || win.composerItem.text !== "") return win.later("Send with Location sends");
            if (App.settings.locationFiles !== 1) return win.fail("Remember did not make it Always: " + App.settings.locationFiles);
            // The question ended with the composer shown: the keyboard is in the text area again.
            if (win.dialog !== null) return win.later("the answered dialog goes");
            if (!win.composerItem.textArea.activeFocus) return win.fail("after Send with Location the focus is not in the text area: " + win.activeFocusItem);
            App.composer.addFiles([MOV]);
            win.next("alwaysAgainLooked");
            return;
        case "alwaysAgainLooked":
            if (!win.settled(1, 1)) return win.later("the second video for Always");
            if (win.row(0).note !== win.located) return win.fail("Always's row: " + win.row(0).note);
            win.pressSend("always again");
            win.next("alwaysAgain");
            return;
        case "alwaysAgain":
            if (App.composer.locationAsking || win.dialog !== null) return win.fail("Always asked");
            if (App.composer.attachmentCount !== 0) return win.later("Always sends at once");
            // ---- Remember + Don't Send: Never; the tray toggle keeps it (F1) ----
            App.settings.locationFiles = 0;
            App.composer.addFiles([MOV, TXT]);
            win.next("neverLooked");
            return;
        case "neverLooked":
            if (!win.settled(2, 2)) return win.later("the files for Never");
            win.pressSend("never");
            win.next("neverAsked");
            return;
        case "neverAsked":
            if (!win.shownOpen()) return win.later("the question for Never");
            win.dialog.rememberBox.click();
            win.dialog.dontSendButton.click();
            win.next("never");
            return;
        case "never": {
            if (App.composer.attachmentCount !== 1 || win.composerItem.text !== "") return win.later("Don't Send sends the text file");
            if (App.settings.locationFiles !== 2) return win.fail("Remember did not make it Never: " + App.settings.locationFiles);
            const tray = App.settings.tray;
            App.settings.tray = !tray;
            if (App.settings.locationFiles !== 2) return win.fail("the tray toggle changed the setting: " + App.settings.locationFiles);
            App.settings.tray = tray;
            if (App.settings.locationFiles !== 2) return win.fail("the tray toggle back changed the setting: " + App.settings.locationFiles);
            App.composer.clearAttachments();
            App.settings.locationFiles = 0;
            // ---- the setting changes while files wait: the rows follow at once (L5) ----
            App.composer.addFiles([MP4, TXT]);
            win.next("prefLooked");
            return;
        }
        case "prefLooked":
            if (!win.settled(2, 2)) return win.later("the files for the setting");
            if (win.row(0).reason !== "" || win.row(0).note !== win.located) return win.fail("the row under Ask: " + win.row(0).reason);
            App.settings.locationFiles = 2;
            if (win.row(0).reason !== win.willNot) return win.fail("Never's reason did not show at once: '" + win.row(0).reason + "'");
            if (App.composer.sendableCount !== 1) return win.fail("the count under Never: " + App.composer.sendableCount);
            // Send under Never: no question, the video stays, the rest goes.
            win.pressSend("never again");
            win.next("neverSent");
            return;
        case "neverSent":
            if (App.composer.locationAsking || win.dialog !== null) return win.fail("Never asked");
            if (App.composer.attachmentCount !== 1 || win.composerItem.text !== "") return win.later("Never sends the rest");
            if (win.row(0).reason !== win.notSent) return win.fail("the held row under Never: " + win.row(0).reason);
            // Back to Ask: the held video's reason follows at once too.
            App.settings.locationFiles = 0;
            if (win.row(0).reason === win.notSent) return win.fail("the reason kept Never after the change");
            App.composer.clearAttachments();
            // ---- a chat switch closes the question; the text stays as the draft (F6) ----
            App.composer.addFiles([MP4]);
            win.next("switchLooked");
            return;
        case "switchLooked":
            if (!win.settled(1, 1)) return win.later("the video for the switch");
            win.pressSend("draft text");
            win.next("switchAsked");
            return;
        case "switchAsked":
            if (!win.shownOpen()) return win.later("the question before the switch");
            win.openConversation(OTHER);
            win.next("switched");
            return;
        case "switched":
            if (App.timeline.conversationId !== OTHER || App.timeline.loading) return win.later("the other chat");
            if (win.dialog !== null || App.composer.locationAsking) return win.later("the switch closes the question");
            win.openConversation(MEDIA);
            win.next("back");
            return;
        case "back":
            if (App.timeline.conversationId !== MEDIA || App.timeline.loading || win.composerItem === null) return win.later("back in the media chat");
            if (win.composerItem.text !== "draft text") return win.later("the draft comes back: " + win.composerItem.text);
            if (App.composer.locationAsking || win.dialog !== null) return win.fail("the question came back with the chat");
            App.composer.clearAttachments();
            win.composerItem.setText("");
            // ---- U1: a stale answer with Remember ticked remembers nothing ----
            App.composer.addFiles([MP4]);
            win.next("staleLooked");
            return;
        case "staleLooked":
            if (!win.settled(1, 1)) return win.later("the video for the stale answer");
            win.askedTimes = 0;
            win.pressSend("stale text");
            win.next("staleAsked");
            return;
        case "staleAsked":
            if (!win.shownOpen()) return win.later("the question that goes stale");
            win.shown = win.dialog;
            // A second located file joins the list while the question is open (F3).
            App.composer.addFiles([MOV]);
            win.next("staleMoved");
            return;
        case "staleMoved":
            if (!win.settled(2, 2)) return win.later("the second file is looked at");
            if (!win.shownOpen() || win.dialog !== win.shown) return win.fail("the question moved by itself");
            win.dialog.rememberBox.click();
            if (!win.dialog.rememberBox.checked) return win.fail("the tick box");
            win.dialog.sendButton.click();
            win.next("staleAnswered");
            return;
        case "staleAnswered":
            // A new dialog, about both files; nothing was sent, and Remember was not applied.
            if (!win.shownOpen() || win.dialog === win.shown) return win.later("the question comes again");
            if (App.settings.locationFiles !== 0) return win.fail("a stale answer was remembered: " + App.settings.locationFiles);
            if (win.askedTimes !== 2 || win.question.count !== 2) return win.fail("asked: " + win.askedTimes + " / " + win.question.count);
            if (win.dialog.rememberBox.checked) return win.fail("the new question starts unticked");
            if (App.composer.attachmentCount !== 2 || win.composerItem.text !== "stale text") return win.fail("the stale answer sent");
            if (!win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1)) return win.fail("no key event");
            win.next("staleDone");
            return;
        case "staleDone":
            if (win.dialog !== null || App.composer.locationAsking) return win.later("Escape dismisses the new question");
            if (++win.quiet < 8) return;
            if (win.askedTimes !== 2) return win.fail("more than one more question: " + win.askedTimes);
            if (App.settings.locationFiles !== 0) return win.fail("the setting moved: " + App.settings.locationFiles);
            App.composer.clearAttachments();
            win.composerItem.setText("");
            // ---- a long caption, Return within 300 ms (BQ-R1) ----
            App.composer.addFiles([MP4]);
            win.next("longLooked");
            return;
        case "longLooked": {
            if (!win.settled(1, 1)) return win.later("the video for the long caption");
            let text = "";
            while (text.length < 620) {
                text += "a long caption about the ridge and the lake, ";
            }
            win.longText = text.trim();
            win.askedTimes = 0;
            win.pressSend(win.longText);
            win.next("longAsked");
            return;
        }
        case "longAsked":
            if (!win.shownOpen()) return win.later("the question for the long caption");
            win.shown = win.dialog;
            // The caption's 300 ms pass; the question stays the one asked.
            if (++win.quiet < 15) return;
            if (win.dialog !== win.shown || win.askedTimes !== 1) return win.fail("asked again: " + win.askedTimes);
            win.dialog.sendButton.click();
            win.next("longSent");
            return;
        case "longSent":
            if (App.composer.attachmentCount !== 0 || win.composerItem.text !== "") return win.later("the long caption goes");
            if (win.askedTimes !== 1) return win.fail("asked again: " + win.askedTimes);
            if (App.composer.locationAsking) return win.fail("asked after the answer");
            console.log("LONG " + win.longText.length);
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

fn wait_for<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The own messages of `conv` with files, oldest first: `(caption, file names)`.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_files(app: &kourier_app::AppHandle, conv: ConversationId) -> Vec<(String, Vec<String>)> {
    let page = app
        .runtime()
        .block_on(app.query(Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 200,
        })));
    let Ok(QueryData::Timeline(page)) = page else {
        panic!("the timeline of the chat");
    };
    page.rows
        .iter()
        .filter(|r| r.origin == OriginKind::OwnThisDevice)
        .filter_map(|r| match &r.body {
            Body::Media {
                attachments,
                caption,
            } => Some((
                caption.as_ref().map(|c| c.body.clone()).unwrap_or_default(),
                attachments
                    .iter()
                    .map(|a| a.file_name.clone().unwrap_or_default())
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// The test's files, in a folder of their own.
struct Files {
    dir: PathBuf,
}

impl Files {
    fn make() -> Self {
        let dir = std::env::temp_dir().join(format!("kourier-location-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("make the folder");
        let write = |name: &str, bytes: &[u8]| {
            std::fs::write(dir.join(name), bytes).expect("write a file");
        };
        write("phone.jpg", &sample_located(SampleLocated::Jpeg));
        write("trip.mp4", &sample_located(SampleLocated::Mp4));
        write("tour.mov", &sample_located(SampleLocated::Mov));
        write("notes.txt", b"Day 1: the ridge.\n");
        Self { dir }
    }

    fn url(&self, name: &str) -> String {
        format!("{:?}", kourier_qt::app::file_url(&self.dir.join(name)))
    }
}

fn main() {
    support::answer_list("location_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
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
    let fixture = handle.fixture().expect("fixture mode");
    let media = fixture.media_conversation.expect("a media conversation");
    let other = fixture.demo_conversation.expect("another conversation");
    assert_ne!(media, other);
    let before = own_files(&handle, media).len();

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let files = Files::make();
    let script = SCRIPT
        .replace("PHONE", &files.url("phone.jpg"))
        .replace("MP4", &files.url("trip.mp4"))
        .replace("MOV", &files.url("tour.mov"))
        .replace("TXT", &files.url("notes.txt"))
        .replace("MEDIA", &media.0.to_string())
        .replace("OTHER", &other.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/location_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        let _ = std::fs::remove_dir_all(&files.dir);
        println!(
            "test the location question ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }

    // ---- what the core was given, in order ----
    let long = {
        let mut text = String::new();
        while text.chars().count() < 620 {
            text.push_str("a long caption about the ridge and the lake, ");
        }
        text.trim().to_owned()
    };
    let expected: Vec<(String, Vec<String>)> = [
        ("keys", vec!["trip.mp4"]),
        ("", vec!["notes.txt"]),
        ("a phone photo", vec!["phone.jpg"]),
        ("second text", vec!["notes.txt"]),
        ("", vec!["tour.mov"]),
        ("always", vec!["trip.mp4"]),
        ("", vec!["notes.txt"]),
        ("always again", vec!["tour.mov"]),
        ("never", vec!["notes.txt"]),
        ("never again", vec!["notes.txt"]),
        (long.as_str(), vec!["trip.mp4"]),
    ]
    .into_iter()
    .map(|(c, n)| (c.to_owned(), n.into_iter().map(str::to_owned).collect()))
    .collect();
    let sent = wait_for(60, || {
        let new = own_files(&handle, media).split_off(before);
        (new.len() >= expected.len()).then_some(new)
    });
    assert_eq!(
        sent, expected,
        "each send with the text of its press; nothing from Escape, a lone Don't Send or a switch"
    );

    let _ = std::fs::remove_dir_all(&files.dir);
    app.shutdown();
    println!("test the location question ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
