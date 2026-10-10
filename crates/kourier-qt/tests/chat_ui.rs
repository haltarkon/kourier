//! The chat's own actions through the real main window and the real core over the fake network
//! (offscreen, `harness = false`). One session of a user:
//!
//! - The window starts with the conversation list only: the chat page, the welcome, link and
//!   settings pages do not exist yet, and the list has made delegates for a screenful of rows,
//!   not for every chat.
//! - Files waiting in the composer stay with their chat: another chat has its own (empty)
//!   list, and coming back finds the files, the choice made for one of them and the text.
//! - A picture is sent as the file it is (the button's choice, and Space on its row), a file
//!   of a type some apps of the network will not save is noted and sent all the same; the
//!   count of files in preparation ends when the core says they are stored.
//! - From the keyboard, on a row: Enter opens the viewer on its album, the Menu key opens its
//!   menu, Ctrl+Enter goes to the message it quotes; a file and a voice note name their
//!   actions.
//! - The viewer follows its message: a picture deleted while it is shown leaves the album and
//!   the next one shows; a picture that was still on its way when the viewer opened shows
//!   once the row has it.
//! - A picture whose type claims it moves and whose content does not shows in the viewer as
//!   its still.
//! - "Delete Attachment…" asks which one and what happens on the other devices, then removes
//!   that picture only.
//! - A custom disappearing-message time is set within the range the chat states, typed into
//!   the field and confirmed with Enter; a time outside the range cannot be confirmed, and
//!   one the core is asked for anyway is refused with a notice.
//! - Enter on a row that has nothing to activate (a sticker) opens its menu and does nothing
//!   else.
//! - "Clear Chat History" empties the chat and keeps it in the list; "Delete Chat" takes it
//!   out of the list and the narrow window goes back to the list.
//!
//! - A call is one line of its chat that changes in place: the chat with calls shows a line for
//!   every state of a 1:1 call (the texts, icons and tones of the design), the conversation
//!   list says the same in its preview and counts the missed call as unread, and the call that
//!   rings fades its icon, only while its window is the active one, until its time runs out,
//!   when the same row says "Missed video call" in the negative tone and stops (the first
//!   steps of the session; the ring is short).
//!
//! Nothing here opens a file in another application or plays sound: the file's and the voice
//! note's actions are checked by what they would do.

use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{OriginKind, Query, QueryData, TimelineAnchor, TimelineQuery};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentKind, Body, ConversationId};
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
    height: 460
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property real album: 0
    property int pictures: 0
    property real removed: 0
    property real quoting: 0
    property real quoted: 0
    property real jumpedTo: 0
    property real openAsked: 0
    property bool openMedia: false
    property real sticker: 0
    property int viewAsked: 0
    property int timerBefore: 0
    // The call that rings, and the lowest opacity its icon showed while it did.
    property real ringing: 0
    property real faded: 1
    // Another window, to take the focus away from this one for a moment.
    property var other: null

    readonly property var chat: win.chatPage
    readonly property var view: win.chatPage ? win.chatPage.timeline : null
    readonly property var composerItem: win.chatPage ? win.chatPage.composer : null

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

    function key(k: int, modifiers: int): void {
        if (!win.ev.keyClick(k, modifiers, -1)) {
            fail("the window took no key event");
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

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property bool own
            required property string text
            required property string icon
            required property string tone
            required property var blocks
            required property var actions
        }
    }

    Instantiator {
        id: convs
        model: App.conversations
        delegate: QtObject {
            required property real conversationId
            required property string preview
            required property string previewIcon
            required property int unread
        }
    }

    Instantiator {
        id: fileRows
        model: App.composer.files
        delegate: QtObject {
            required property string name
            required property string reason
            required property int part
            required property bool canOriginal
            required property bool original
            required property string note
        }
    }

    // The list's row of a chat.
    function listRow(conversation: real): var {
        for (let i = 0; i < convs.count; ++i) {
            if (convs.objectAt(i).conversationId === conversation) {
                return convs.objectAt(i);
            }
        }
        return null;
    }

    // The call lines of the open chat, newest first, as "text|icon|tone".
    function callLines(): var {
        const lines = [];
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "system" && r.icon.indexOf("call-") === 0) {
                lines.push(r.text + "|" + r.icon + "|" + r.tone);
            }
        }
        return lines;
    }

    function listed(conversation: real): bool {
        for (let i = 0; i < convs.count; ++i) {
            if (convs.objectAt(i).conversationId === conversation) {
                return true;
            }
        }
        return false;
    }

    function row(id: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
                return r;
            }
        }
        return null;
    }

    function indexOf(id: real): int {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
                return i;
            }
        }
        return -1;
    }

    // How many rows a person or the app wrote (messages and notices).
    function written(): int {
        let n = 0;
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            n += r && (r.kind === "message" || r.kind === "system") ? 1 : 0;
        }
        return n;
    }

    function systemRow(part: string): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "system" && r.text.indexOf(part) >= 0) {
                return true;
            }
        }
        return false;
    }

    // The pictures of a row (of its album or its single image).
    function picturesOf(r: var): var {
        for (const b of r.blocks) {
            if (b.type === "album") {
                return b.items;
            }
            if (b.type === "image") {
                return [b];
            }
        }
        return [];
    }

    // The keyboard is in the timeline, on the row of `id` (which is brought on screen).
    function focusRow(id: real): bool {
        const at = win.indexOf(id);
        if (at < 0) {
            return false;
        }
        const list = win.view.rowList;
        win.view.followNewest = false;
        list.positionViewAtIndex(at, ListView.Center);
        list.forceActiveFocus();
        list.currentIndex = at;
        return true;
    }

    // The row of `id` once the keyboard is in the timeline and on that row; else puts it
    // there and returns null, and the caller looks again a tick later. (The key that follows
    // goes to whatever has the focus in that moment: after a chat was opened or a popup
    // closed, the timeline may have lost it, and its current row with it, since `focusRow`.)
    function rowWithKeyboard(id: real): var {
        const d = win.view.delegateFor(id);
        const list = win.view.rowList;
        if (d === null || !list.activeFocus || list.currentItem !== d) {
            win.focusRow(id);
            return null;
        }
        return d;
    }

    Connections {
        target: App.timeline
        function onJumpRequested(message: real, row: int): void {
            win.jumpedTo = message;
        }
    }
    Connections {
        target: MediaActions
        function onOpenRequested(attachment: real, name: string, media: bool): void {
            win.openAsked = attachment;
            win.openMedia = media;
        }
        function onViewRequested(attachment: real, url: string, animated: bool, media: bool, message: real): void {
            win.viewAsked += 1;
        }
    }

    // An input event can spin the event loop while it is delivered: the steps never nest.
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
        {
            switch (win.step) {
            case "start": {
                if (!win.started || !App.hasAccounts) return win.later("the start page");
                const list = win.pageStack.get(0);
                if (!list || !list.rowsShown || list.flickable.count < 15) return win.later("the list with its rows");
                if (list.flickable.count !== convs.count) return win.fail("the list shows " + list.flickable.count + " of " + convs.count + " chats");
                // Only what is on screen (and a little more) got a delegate: a list that
                // makes one for every chat at the start does the work of all of them.
                if (list.delegatesCreated < 3 || list.delegatesCreated >= convs.count) return win.fail("delegates made for " + convs.count + " chats: " + list.delegatesCreated);
                // Nothing but the list exists yet.
                if (win.chatPage !== null || win.welcomePage !== null || win.linkPage !== null || win.settingsPage !== null) return win.fail("a page was created before it was shown");
                if (win.pageStack.depth !== 1) return win.fail("pages at the start: " + win.pageStack.depth);
                win.height = 760;
                win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "chat_ui_events.qml");
                // ---- a call is one line that changes ----
                // The list says what the newest line is, and counts the missed call.
                const call = win.listRow(CALLS);
                if (call === null) return win.fail("the chat with calls is not listed");
                if (call.preview !== "Incoming video call\u2026" || call.previewIcon !== "call-incoming") return win.fail("the list's preview of the ringing call: '" + call.preview + "' with " + call.previewIcon);
                if (call.unread !== 1) return win.fail("the missed call counts as one unread message in the list: " + call.unread);
                win.openConversation(CALLS);
                win.next("calls");
                return;
            }
            case "calls": {
                if (App.timeline.conversationId !== CALLS || App.timeline.loading || rows.count < 10) return win.later("the chat with calls");
                const lines = win.callLines();
                const expected = [
                    "Incoming video call\u2026|call-incoming|ringing",
                    "Missed call|call-missed|negative",
                    "Unanswered voice call|call-outgoing|",
                    "Outgoing video call|call-outgoing|",
                    "Voice call \u00b7 answered on another device|call-start|",
                    "Declined video call|call-stop|negative",
                    "Missed voice call|call-missed|negative"
                ];
                if (lines.join("\n") !== expected.join("\n")) return win.fail("the call lines:\n" + lines.join("\n"));
                // The one that rings: its row, its icon fading.
                let ring = null;
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "system" && r.tone === "ringing") ring = r;
                }
                win.ringing = ring.messageId;
                const d = win.view.noticeFor(win.ringing);
                if (d === null) return win.later("the ringing row on screen");
                // The pulse runs in the active window only (as animated pictures do).
                if (!win.active) {
                    win.requestActivate();
                    return win.later("this window to be the active one");
                }
                if (!d.pulsing || !d.animating) return win.fail("the ringing call does not pulse (pulsing " + d.pulsing + ", running " + d.animating + ")");
                // (Not a transient child of this window: a window counts as active while one of those is.)
                win.other = Qt.createQmlObject("import QtQuick\nWindow { transientParent: null; width: 120; height: 80; visible: true }", win.contentItem, "chat_ui_other.qml");
                win.other.requestActivate();
                win.next("ringing-unwatched");
                return;
            }
            case "ringing-unwatched": {
                // Another window is the active one: the call still rings, nothing moves here.
                const d = win.view.noticeFor(win.ringing);
                if (d === null) return win.fail("the ringing row went away");
                if (win.row(win.ringing).text !== "Incoming video call\u2026") return win.fail("the ring ran out before the test looked");
                if (win.other !== null) {
                    if (win.active || !win.other.active) return win.later("the other window to be the active one");
                    if (d.pulsing || d.animating) return win.fail("the pulse runs in a window that is not active (pulsing " + d.pulsing + ", running " + d.animating + ")");
                    const icon = win.find(d, c => c.isMask === true && String(c.source) === "call-incoming");
                    if (icon === null || icon.opacity !== 1) return win.fail("the icon of a ring nobody watches is not at rest");
                    win.other.destroy();
                    win.other = null;
                    win.requestActivate();
                    return win.later("this window to be the active one again");
                }
                if (!win.active) {
                    win.requestActivate();
                    return win.later("this window to be the active one again");
                }
                if (!d.pulsing || !d.animating) return win.later("the pulse to run again");
                win.next("ringing");
                return;
            }
            case "ringing": {
                const d = win.view.noticeFor(win.ringing);
                if (d === null) return win.fail("the ringing row went away");
                const r = win.row(win.ringing);
                if (r.text === "Incoming video call\u2026") {
                    // Still ringing: the icon fades in and out and the row says so.
                    if (!d.pulsing || !d.animating) return win.fail("the pulse stopped while the call still rings");
                    const icon = win.find(d, c => c.isMask === true && String(c.source) === "call-incoming");
                    if (icon === null) return win.fail("the ringing row has no icon");
                    win.faded = Math.min(win.faded, icon.opacity);
                    return win.later("the ring to run out");
                }
                // Out of the ring: the same row says what the call became.
                if (win.faded > 0.9) return win.fail("the icon never faded while the call rang: " + win.faded);
                if (r.text !== "Missed video call" || r.icon !== "call-missed" || r.tone !== "negative") return win.fail("the call after its time: '" + r.text + "' " + r.icon + " " + r.tone);
                if (win.callLines().length !== 7) return win.fail("the line changed in place: " + win.callLines().length + " lines");
                win.next("missed");
                return;
            }
            case "missed": {
                const d = win.view.noticeFor(win.ringing);
                if (d === null) return win.fail("the row went away");
                if (d.pulsing || d.animating) return win.fail("the missed call still pulses");
                const icon = win.find(d, c => c.isMask === true && String(c.source) === "call-missed");
                if (icon === null) return win.later("the icon of the missed call");
                if (icon.opacity !== 1) return win.fail("the icon of the missed call is faded: " + icon.opacity);
                const call = win.listRow(CALLS);
                if (call.preview !== "Missed video call" || call.previewIcon !== "call-missed") return win.fail("the list's preview of the missed call: '" + call.preview + "' with " + call.previewIcon);
                win.openConversation(MEDIA);
                win.next("media");
                return;
            }
            case "media": {
                if (win.chatPage === null) return win.fail("opening a chat creates the chat page");
                if (App.timeline.conversationId !== MEDIA || App.timeline.loading || rows.count < 3 || App.timeline.attachKinds.indexOf("file") < 0) return win.later("the media chat");
                if (win.pageStack.depth !== 2) return win.fail("the chat page is pushed");
                // The reaction picker (a third of what the chat page costs the Qt thread)
                // is not part of opening a chat: it is made when it is first asked for.
                if (win.view.pickerPopup !== null) return win.fail("the reaction picker was made with the chat page");
                // ---- files stay with their chat ----
                if (App.composer.addFiles([FIRST_PICTURE, SECOND_PICTURE, THE_APP]) !== 3) return win.fail("three files are added");
                App.composer.setOriginal(1, true);
                win.composerItem.setText("for the first chat");
                win.openConversation(ALBUMS);
                win.next("elsewhere");
                return;
            }
            case "elsewhere":
                if (App.timeline.conversationId !== ALBUMS || App.timeline.loading || rows.count < 3) return win.later("the albums chat");
                // Another chat has its own list and its own text.
                if (App.composer.attachmentCount !== 0) return win.fail("the other chat's files followed: " + App.composer.attachmentCount);
                if (win.composerItem.text !== "") return win.fail("the other chat's text followed: " + win.composerItem.text);
                if (App.composer.addFiles([THIRD_PICTURE]) !== 1) return win.fail("a file for the second chat");
                win.openConversation(MEDIA);
                win.next("back");
                return;
            case "back": {
                if (App.timeline.conversationId !== MEDIA) return win.later("the media chat again");
                // The files, the choice and the text are where they were left.
                if (App.composer.attachmentCount !== 3) return win.fail("the files waited: " + App.composer.attachmentCount);
                if (win.composerItem.text !== "for the first chat") return win.fail("the text waited: " + win.composerItem.text);
                if (!App.composer.planned) return win.later("the plan");
                const a = fileRows.objectAt(0);
                const b = fileRows.objectAt(1);
                const apk = fileRows.objectAt(2);
                if (a.name !== "A.png" || b.name !== "B.png" || apk.name !== "Ground station.apk") return win.fail("which files: " + a.name + ", " + b.name + ", " + apk.name);
                if (!a.canOriginal || a.original) return win.fail("a picture has the choice and goes as a picture");
                if (!b.canOriginal || !b.original) return win.fail("the choice made before the switch is kept");
                if (apk.canOriginal || apk.original) return win.fail("a file has no such choice");
                // The type some apps will not save is noted, and is not a refusal.
                if (apk.note !== "Some Test network apps cannot save this type of file" || apk.reason !== "") return win.fail("the note: '" + apk.note + "', reason '" + apk.reason + "'");
                if (a.note !== "" || b.note !== "") return win.fail("a note on a picture");
                if (App.composer.refusedCount !== 0 || App.composer.sendableCount !== 3) return win.fail("all three can go");
                // A picture, the same picture as a file, the app: three messages.
                if (App.composer.messageCount !== 3) return win.fail("messages: " + App.composer.messageCount);
                if (a.part !== 1 || b.part !== 2 || apk.part !== 3) return win.fail("parts: " + a.part + " " + b.part + " " + apk.part);
                // Space on a row changes the choice, and back.
                const list = win.composerItem.fileList;
                list.forceActiveFocus();
                list.currentIndex = 0;
                win.key(Qt.Key_Space, Qt.NoModifier);
                win.next("spaced");
                return;
            }
            case "spaced":
                if (!fileRows.objectAt(0).original) return win.later("Space sends the first picture as a file");
                if (!App.composer.planned) return win.later("the plan");
                win.key(Qt.Key_Space, Qt.NoModifier);
                win.next("unspaced");
                return;
            case "unspaced":
                if (fileRows.objectAt(0).original) return win.later("Space again sends it as a picture");
                if (!App.composer.planned) return win.later("the plan");
                if (App.composer.messageCount !== 3) return win.fail("messages after the change and back: " + App.composer.messageCount);
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the files went with the send");
                if (App.composer.preparing !== 3) return win.fail("files in preparation: " + App.composer.preparing);
                win.next("sent");
                return;
            case "sent":
                // The count ends when the core says the messages are stored.
                if (App.composer.preparing !== 0) return win.later("the core stores the send");
                if (App.composer.preparingDone !== 0) return win.fail("nothing is being copied any more");
                win.openConversation(ALBUMS);
                win.next("albums");
                return;
            case "albums": {
                if (App.timeline.conversationId !== ALBUMS || App.timeline.loading) return win.later("the albums chat");
                // The file left for this chat is still here.
                if (App.composer.attachmentCount !== 1 || fileRows.objectAt(0).name !== "C.png") return win.fail("this chat's file waited: " + App.composer.attachmentCount);
                App.composer.removeAttachment(0);
                win.next("album");
                return;
            }
            case "album": {
                if (!win.view.albumsShown()) return win.later("the albums with their pictures");
                win.album = App.findMessage("removable");
                if (win.album <= 0) return win.later("an album whose pictures can be removed");
                win.pictures = win.picturesOf(win.row(win.album)).length;
                if (win.pictures < 3) return win.fail("the album has " + win.pictures + " pictures");
                // ---- the keyboard on a row ----
                if (!win.focusRow(win.album)) return win.fail("the album's row");
                win.next("enter");
                return;
            }
            case "enter": {
                const d = win.rowWithKeyboard(win.album);
                if (d === null) return win.later("the keyboard on the album's row");
                if (d.mediaAction() !== "view" || d.mediaLabel() !== "View Pictures") return win.fail("what Enter does on an album: " + d.mediaAction() + " / " + d.mediaLabel());
                if (win.imageViewer !== null) return win.fail("a viewer before Enter");
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("viewer");
                return;
            }
            case "viewer": {
                const v = win.imageViewer;
                if (!v || !v.opened || !v.shown) return win.later("Enter opens the viewer on the album");
                if (v.count !== win.pictures || v.index !== 0 || v.message !== win.album) return win.fail("the viewer: " + v.index + " of " + v.count + " for " + v.message);
                // ---- the viewer follows its message ----
                win.removed = v.attachment;
                App.deleteAttachment(win.removed);
                win.next("follows");
                return;
            }
            case "follows": {
                const v = win.imageViewer;
                if (!v) return win.fail("the viewer closed although pictures are left");
                if (v.count !== win.pictures - 1) return win.later("the deleted picture leaves the viewer");
                if (!v.opened) return win.fail("the viewer stays open");
                if (v.attachment === win.removed || v.attachment <= 0) return win.fail("the next picture takes its place");
                if (!v.shown) return win.later("the next picture shows");
                if (win.picturesOf(win.row(win.album)).length !== win.pictures - 1) return win.fail("the row lost one picture");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("closed");
                return;
            }
            case "closed": {
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                // ---- a download that ends while the viewer is open ----
                // The viewer is opened on the album as it was before its pictures arrived
                // (what a click on a tile that is still downloading hands over).
                const stale = [];
                for (const item of App.viewerItems(win.album)) {
                    stale.push({
                        "attachment": item.attachment,
                        "url": "",
                        "animated": false,
                        "media": item.media,
                        "kind": item.kind,
                        "state": "downloading",
                        "progress": 0.4,
                        "placeholder": item.placeholder
                    });
                }
                if (stale.length !== win.pictures - 1) return win.fail("the album's pictures: " + stale.length);
                const v = win.openAlbumViewer(stale, 0, win.album);
                // (Checked at once: the next change of the row already brings it up to date.)
                if (!v || !v.absent || v.shown || v.mediaState !== "downloading") return win.fail("it says the picture is on its way");
                win.next("waiting");
                return;
            }
            case "waiting": {
                const v = win.imageViewer;
                if (!v || !v.opened) return win.later("the viewer on a picture that is not here yet");
                // The row changes (in life: the download ends) and the viewer shows what the
                // row has now, without being closed and opened again.
                App.react(win.album, "👍", false);
                win.next("arrived");
                return;
            }
            case "arrived": {
                const v = win.imageViewer;
                if (!v || !v.opened) return win.fail("the viewer stays open while it waits");
                if (v.absent || !v.shown) return win.later("the picture shows once the row has it");
                if (v.mediaState !== "local" || v.count !== win.pictures - 1 || v.index !== 0) return win.fail("the viewer after the arrival: " + v.mediaState + ", " + v.index + " of " + v.count);
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("closedAgain");
                return;
            }
            case "closedAgain": {
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                // ---- a picture whose type says it moves and whose content does not ----
                // (The type is the sender's claim. The player only gets what is a GIF or
                // WebP by content; anything else shows as its still, as in the timeline.)
                const real = App.viewerItems(win.album)[0];
                const v = win.openAlbumViewer([
                    {
                        "attachment": real.attachment,
                        "url": real.url,
                        "animated": true,
                        "media": real.media,
                        "kind": "gif",
                        "state": "local",
                        "progress": 1,
                        "placeholder": real.placeholder
                    }
                ], 0, 0);
                if (!v || !v.animated) return win.fail("the viewer on a picture that claims to move");
                win.next("claimed");
                return;
            }
            case "claimed": {
                const v = win.imageViewer;
                if (!v || !v.opened) return win.later("the viewer");
                if (v.movie) return win.later("its copy is looked at: no movie");
                if (!v.shown) return win.later("a picture that only claims to move shows as its still");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("closedThird");
                return;
            }
            case "closedThird":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                if (!win.focusRow(win.album)) return win.fail("the album's row");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("menu");
                return;
            case "menu": {
                const m = win.view.messageMenu;
                if (!m.visible) return win.later("the Menu key opens the row's menu");
                if (m.row.messageId !== win.album) return win.fail("the menu is about the album");
                if (m.mediaAction !== "view" || !m.canDeleteAttachment) return win.fail("the menu offers viewing and deleting an attachment");
                let media = null;
                let remove = null;
                for (let i = 0; i < m.count; ++i) {
                    const item = m.itemAt(i);
                    if (item && item.objectName === "menuMedia") media = item;
                    if (item && item.objectName === "menuDeleteAttachment") remove = item;
                }
                if (!media || !media.visible || media.text !== "View Pictures") return win.fail("the entry for the row's own action");
                if (!remove || !remove.visible) return win.fail("the entry that deletes an attachment");
                m.close();
                remove.triggered();
                win.next("which");
                return;
            }
            case "which": {
                const d = win.view.attachmentDialog;
                if (!d || !d.opened) return win.later("the question which attachment");
                if (!d.several || d.attachments.length !== win.pictures - 1) return win.fail("it lists the pictures that are left: " + d.attachments.length);
                // It says what happens elsewhere (the fake network has other devices to tell).
                if (!d.sync || d.subtitle.indexOf("your other devices") < 0 || d.subtitle.indexOf("keep their copies") < 0) return win.fail("what it says: " + d.subtitle);
                d.chosen = 1;
                win.removed = d.attachment;
                if (win.removed <= 0 || win.removed !== d.attachments[1].attachment) return win.fail("the second one is chosen");
                d.customFooterActions[0].trigger();
                win.next("removed");
                return;
            }
            case "removed": {
                const left = win.picturesOf(win.row(win.album));
                if (left.length !== win.pictures - 2) return win.later("the chosen picture goes");
                if (left.some(p => p.attachment === win.removed)) return win.fail("another picture went instead");
                if (App.composer.notice !== "") return win.fail("a notice without a failure: " + App.composer.notice);
                // ---- a row without an action of its own: a sticker ----
                win.sticker = App.findMessage("sticker");
                if (win.sticker <= 0) return win.fail("the chat's sticker");
                if (!win.focusRow(win.sticker)) return win.fail("the sticker's row");
                win.next("sticker");
                return;
            }
            case "sticker": {
                const d = win.rowWithKeyboard(win.sticker);
                if (d === null) return win.later("the keyboard on the sticker's row");
                const block = d.blockItems()[0];
                if (!block || !block.sticker) return win.fail("the row's block is a sticker");
                if (!block.local || !block.shown) return win.later("the sticker is downloaded and shown");
                if (d.mediaAction() !== "" || d.mediaLabel() !== "") return win.fail("a sticker names no action: " + d.mediaAction());
                if (win.view.messageMenu.visible) return win.fail("a menu before Enter");
                if (win.view.pickerPopup !== null) return win.fail("a reaction picker nobody asked for was made");
                win.viewAsked = 0;
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("stickerMenu");
                return;
            }
            case "stickerMenu": {
                // Enter on a row that has nothing to activate opens its menu, and only that:
                // the sticker is not handed to the viewer on the side.
                const m = win.view.messageMenu;
                if (!m.visible) return win.later("Enter opens the menu of a row without an action");
                if (m.row.messageId !== win.sticker) return win.fail("the menu is about the sticker");
                if (m.mediaAction !== "") return win.fail("the menu offers an action for a sticker: " + m.mediaAction);
                if (win.viewAsked !== 0) return win.fail("Enter also sent the sticker to the viewer");
                if (win.imageViewer !== null && win.imageViewer.visible) return win.fail("a viewer opened behind the menu");
                m.close();
                win.next("stickerDone");
                return;
            }
            case "stickerDone":
                // (The menu hands the keyboard back when it is gone: not while the next
                // chat's row is being made current.)
                if (win.view.messageMenu.visible) return win.later("the menu closes");
                win.openConversation(QUOTES);
                win.next("quotes");
                return;
            case "quotes": {
                if (App.timeline.conversationId !== QUOTES || App.timeline.loading || rows.count < 3) return win.later("the chat with quotes");
                win.quoting = App.findMessage("quote");
                if (win.quoting <= 0) return win.later("a message that quotes another");
                const quote = win.row(win.quoting).blocks.find(b => b.type === "quote");
                win.quoted = quote.target;
                if (!win.focusRow(win.quoting)) return win.fail("the quoting row");
                win.next("quote");
                return;
            }
            case "quote": {
                const d = win.rowWithKeyboard(win.quoting);
                if (d === null) return win.later("the keyboard on the quoting row");
                if (d.quoteBlock() === null) return win.fail("the row's quote leads somewhere");
                win.jumpedTo = 0;
                win.key(Qt.Key_Return, Qt.ControlModifier);
                win.next("jumped");
                return;
            }
            case "jumped":
                if (win.jumpedTo !== win.quoted) return win.later("Ctrl+Enter goes to the quoted message (timeline focus " + win.view.rowList.activeFocus + ", current row " + win.view.rowList.currentIndex + ")");
                win.openConversation(MEDIA);
                win.next("file");
                return;
            case "file": {
                if (App.timeline.conversationId !== MEDIA || App.timeline.loading || rows.count < 3) return win.later("the media chat");
                const file = App.findMessage("file");
                const voice = App.findMessage("voice");
                if (file <= 0 || voice <= 0) return win.later("a file and a voice note");
                const f = win.view.delegateFor(file);
                const v = win.view.delegateFor(voice);
                if (f === null || v === null) {
                    win.focusRow(f === null ? file : voice);
                    return win.later("their rows on screen");
                }
                if (f.mediaAction() === "" || v.mediaAction() === "") return win.later("both are downloaded");
                // What Enter would do (neither is done here: one opens another application,
                // the other plays sound).
                if (f.mediaAction() !== "open" || f.mediaLabel() !== "Open File") return win.fail("a file: " + f.mediaAction() + " / " + f.mediaLabel());
                if (v.mediaAction() !== "play" || v.mediaLabel() !== "Play Voice Message") return win.fail("a voice note: " + v.mediaAction() + " / " + v.mediaLabel());
                const info = f.rowInfo();
                if (info.mediaLabel !== "Open File" || info.attachments.length !== 1) return win.fail("the menu's description of the file row");
                // The voice player is loaded by its file only when a note is played (Qt
                // Multimedia stays out of the start): the file is there to load, and nothing
                // has loaded it yet.
                let voiceBlock = null;
                for (const item of v.blockItems()) {
                    if (item.playerUrl !== undefined) {
                        voiceBlock = item;
                    }
                }
                if (voiceBlock === null) return win.fail("the voice note's block");
                if (voiceBlock.player !== null) return win.fail("a player before anything was played");
                const player = Qt.createComponent(voiceBlock.playerUrl);
                if (player.status !== Component.Ready) return win.fail("the voice player cannot be loaded: " + player.errorString());
                win.openConversation(TIMER);
                win.next("timer");
                return;
            }
            case "timer":
                if (App.timeline.conversationId !== TIMER || App.timeline.loading || App.timeline.timerOptions.length === 0) return win.later("the chat with a timer");
                // ---- a custom time ----
                if (!win.chat.timerCustom) return win.fail("the chat states a range of custom timers");
                win.timerBefore = App.timeline.timerSecs;
                win.chat.openCustomTimer();
                win.next("custom");
                return;
            case "custom": {
                const d = win.chat.timerDialog;
                if (!d || !d.opened) return win.later("the custom time dialog");
                if (d.min !== 1 || d.max !== 2419200) return win.fail("the range: " + d.min + " to " + d.max);
                const amount = win.find(d.contentItem, c => c.objectName === "timerAmount");
                const unit = win.find(d.contentItem, c => c.objectName === "timerUnit");
                const range = win.find(d.contentItem, c => c.objectName === "timerRange");
                if (!amount || !unit || !range) return win.fail("the dialog's fields");
                if (range.text !== "Between 1 second and 4 weeks.") return win.fail("the range it states: " + range.text);
                // Five weeks is more than the chat takes: "Set" does nothing.
                unit.currentIndex = unit.count - 1;
                amount.value = 5;
                if (d.valid) return win.fail("five weeks counts as inside the range");
                d.accept();
                if (!d.opened || App.timeline.timerSecs !== win.timerBefore) return win.fail("a time outside the range was taken");
                // Every time of the range can be said, in seconds if need be: the field has
                // no bound of its own below the range's end (1 day and 1 second here).
                unit.currentIndex = 0;
                amount.value = 86401;
                if (amount.value !== 86401 || d.seconds !== 86401 || !d.valid) return win.fail("86401 seconds is inside the range: the field holds " + amount.value);
                // A time typed into the field counts as it is typed: "Set" and Enter take
                // what the field shows, not the number it held before.
                amount.forceActiveFocus();
                amount.contentItem.selectAll();
                win.key(Qt.Key_4, Qt.NoModifier);
                win.key(Qt.Key_5, Qt.NoModifier);
                win.next("typed");
                return;
            }
            case "typed": {
                const d = win.chat.timerDialog;
                const amount = win.find(d.contentItem, c => c.objectName === "timerAmount");
                if (amount.contentItem.text !== "45") return win.later("the typed number in the field: '" + amount.contentItem.text + "'");
                if (d.seconds !== 45 || !d.valid) return win.fail("the dialog goes by " + d.seconds + " s while the field says 45");
                // Enter in the field sets it.
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("customSet");
                return;
            }
            case "customSet":
                if (App.timeline.timerSecs !== 45 || !win.systemRow("You set the disappearing message timer to 45 seconds")) return win.later("the custom timer is set");
                if (App.timeline.timerLabel !== "45 seconds") return win.fail("the header's mark: " + App.timeline.timerLabel);
                // The menu lists what holds, so it can mark it.
                if (!App.timeline.timerOptions.some(o => o.secs === 45 && o.label === "45 seconds")) return win.fail("the menu lists the custom time");
                if (App.composer.notice !== "") return win.fail("a notice without a failure: " + App.composer.notice);
                // Asked for anyway, a time outside the range is refused by the core.
                App.setTimer(3024000);
                win.next("refused");
                return;
            case "refused":
                if (App.composer.notice === "") return win.later("the refusal");
                if (App.composer.notice !== "This timer is not available in this conversation.") return win.fail("the notice: " + App.composer.notice);
                if (App.timeline.timerSecs !== 45) return win.fail("a refused timer changes nothing");
                App.composer.clearNotice();
                // ---- clearing the history ----
                if (win.written() < 3) return win.fail("the chat has messages to clear");
                win.chat.askClear();
                win.next("clearAsk");
                return;
            case "clearAsk": {
                const d = win.chat.deleteDialog;
                if (!d || !d.opened) return win.later("the question before clearing");
                if (d.remove) return win.fail("it asks about the history, not the chat");
                if (!d.sync || d.subtitle.indexOf("your other devices") < 0 || d.subtitle.indexOf("stays in your list") < 0) return win.fail("what it says: " + d.subtitle);
                d.customFooterActions[0].trigger();
                win.next("cleared");
                return;
            }
            case "cleared":
                if (win.written() !== 0) return win.later("the messages go");
                if (App.timeline.conversationId !== TIMER) return win.fail("the chat stays open");
                if (!win.listed(TIMER)) return win.fail("the chat stays in the list");
                if (App.composer.notice !== "") return win.fail("a notice without a failure: " + App.composer.notice);
                // ---- deleting the chat ----
                win.chat.askDelete();
                win.next("deleteAsk");
                return;
            case "deleteAsk": {
                const d = win.chat.deleteDialog;
                if (!d || !d.opened || !d.remove) return win.later("the question before deleting");
                if (d.subtitle.indexOf("comes back with its next message") < 0) return win.fail("what it says: " + d.subtitle);
                d.customFooterActions[0].trigger();
                win.next("deleted");
                return;
            }
            case "deleted":
                if (App.timeline.conversationId !== 0 || win.listed(TIMER)) return win.later("the chat leaves the list");
                // The narrow window is back at the list, not on an empty chat page.
                if (win.pageStack.depth !== 1) return win.later("back at the list");
                if (convs.count < 15) return win.fail("the other chats are still listed: " + convs.count);
                Qt.exit(0);
                return;
            }
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

/// The attachments of the own messages of `conv`, oldest first: `(name, kind, MIME type)`.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_files(
    app: &kourier_app::AppHandle,
    conv: ConversationId,
) -> Vec<Vec<(String, AttachmentKind, String)>> {
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
            Body::Media { attachments, .. } => Some(
                attachments
                    .iter()
                    .map(|a| {
                        (
                            a.file_name.clone().unwrap_or_default(),
                            a.kind,
                            a.mime.clone(),
                        )
                    })
                    .collect(),
            ),
            _ => None,
        })
        .collect()
}

fn main() {
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
            // The missed call counts in the list, and the last call rings only a short while.
            calls_unread: true,
            calls_ring_for: Some(Duration::from_secs(12)),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let media = fixture.media_conversation.expect("a media conversation");
    let calls = fixture.calls_conversation.expect("the chat with calls");
    let albums = fixture.albums_conversation.expect("the albums chat");
    let quotes = fixture.quotes_conversation.expect("the quotes chat");
    let timer = fixture.timer_conversation.expect("the timer chat");
    let before = own_files(&handle, media).len();

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let dir = std::env::temp_dir().join(format!("kourier-chat-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let file = |name: &str, bytes: &[u8]| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write a file");
        format!("{:?}", kourier_qt::app::file_url(&path))
    };
    let png = |hue: i32| kourier_qt::bridge::qobject::sample_png(64, 48, hue);
    let script = SCRIPT
        .replace("FIRST_PICTURE", &file("A.png", &png(20)))
        .replace("SECOND_PICTURE", &file("B.png", &png(200)))
        .replace("THIRD_PICTURE", &file("C.png", &png(300)))
        .replace(
            "THE_APP",
            &file("Ground station.apk", b"PK\x03\x04 not really"),
        )
        .replace("CALLS", &calls.0.to_string())
        .replace("MEDIA", &media.0.to_string())
        .replace("ALBUMS", &albums.0.to_string())
        .replace("QUOTES", &quotes.0.to_string())
        .replace("TIMER", &timer.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/chat_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the chat's actions on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }

    // What the core made of the send: a picture, the same kind of picture as the file it is,
    // and the app, each a message of its own, in the order they were picked.
    let sent = wait_for(60, || {
        let all = own_files(&handle, media);
        (all.len() >= before + 3).then(|| all[before..].to_vec())
    });
    assert_eq!(sent.len(), 3, "three messages");
    assert_eq!(
        sent[0],
        vec![(
            "A.png".to_owned(),
            AttachmentKind::Image,
            "image/png".to_owned()
        )],
        "a picture is sent as a picture"
    );
    assert_eq!(
        sent[1],
        vec![(
            "B.png".to_owned(),
            AttachmentKind::File,
            "image/png".to_owned()
        )],
        "a picture sent as the original file keeps its name and type and is a file"
    );
    assert_eq!(sent[2].len(), 1);
    assert_eq!(sent[2][0].0, "Ground station.apk");
    assert_eq!(sent[2][0].1, AttachmentKind::File, "noted, and sent");

    let _ = std::fs::remove_dir_all(&dir);
    app.shutdown();
    println!(
        "test the chat's actions on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed"
    );
}
