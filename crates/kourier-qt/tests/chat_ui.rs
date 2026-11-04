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
//! - The viewer over the picture of a bubble, with real pointer events: one click on the
//!   bubble's picture opens one viewer; a click on the viewer's picture at the point where the
//!   bubble's lies under it keeps it open, a click beside it closes it, and neither opens a
//!   second viewer through the bubble (a modal popup's tap reached the passive `TapHandler`
//!   under it in Qt 6.11); asked twice, one viewer is left; a click on the blank corner of the
//!   reaction picker, the list of who reacted, the emoji picker and the question before a
//!   link, put over that picture, does not reach it either.
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
//!   steps of the session; the ring is short and counted from the start of the core's clock,
//!   so a slow start finds it over: then the script takes the missed form as right exactly
//!   when the clock says the ring ran out, checks that form, and says in the log that the
//!   pulse and the fade were not fully checked).
//! - The group's page (people pages P2): the header opens it, the members in their order, the
//!   timer read-only with its reason, a click and the wheel that reach nothing under the panel,
//!   a place kept while a member's page lies on top (and found again by key after a refresh),
//!   "Show All" with the search (markup names shown as text), a member's combo and "Custom
//!   Time…" acting on THEIR chat, Escape one level at a time, the X, Message, and the
//!   group's chat deleted with a member's page on top.
//!
//! - Senders and mentions open the person (people pages P3): a real click on a sender's name or
//!   picture in the group's chat, and on a mention in a text, opens that person in the panel (the
//!   only level when the panel was closed: Back closes it; STACKED on the group page, the members
//!   level or a member's page when it was open: Back returns to them as they were, the members'
//!   search kept); a click beside the name does not; the message menu, the reaction picker and the
//!   emoji picker put over the name or the picture take the click; a mention of the user opens
//!   his own page; a mention under a spoiler, inside a link and the forged text
//!   `kourier:mention/0` open no page. The message menu of a row (the Menu key on the row with
//!   the keyboard) offers the same pages, "Show Sender's Page" and one per mentioned person, and
//!   the keyboard goes back to the row when the page closes; the menu has no leading or doubled
//!   separator and an "&" in a name is literal.
//!
//! - Media and Files (people pages P4): the person's page of the albums chat ends with six squares
//!   (three in a row, no Tab stops) and the button "All Media and Files", the section's one stop; a
//!   real click on a square opens ONE viewer on that picture and a click on the viewer's picture
//!   over the square opens no second one; the button opens the level (the grid starts with the
//!   section's first picture, the Files tab says there is none, a cell opens the viewer, Back empties
//!   its lists and puts the keyboard on the button); the keyboard does the same (Tab to the button,
//!   Return, Tab to the grid, the arrows, Return, Escape twice, the X from the level); three
//!   pictures sent into the group fill the group page's section (the button is there for three
//!   pictures too), a file sent meanwhile appears LIVE, the level lists the file with the timeline's
//!   round button, Back puts the keyboard on the button, and fifty more pictures make the level ask
//!   for its second page when the grid is scrolled to the end. The viewer of a square reads the
//!   WHOLE album of its message with a query of its own: from the group page's last square it
//!   stays open, on the same picture with every picture of the message, when one more picture
//!   arrives in the chat (the section's squares move on), and closes when the message is deleted;
//!   a member's page opened from the group page (another chat than the open one) opens ONE viewer,
//!   shows her one square at the right edge when mirrored, opens her level's oldest picture, and a
//!   deleted picture closes the viewer; a sender opened from the timeline replaces every level
//!   above the base ones (`group,person,media` and `group,media` both become `group,person`).
//!
//! Nothing here opens a file in another application or plays sound: the file's and the voice
//! note's actions are checked by what they would do.

mod support;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{OriginKind, Query, QueryData, TimelineAnchor, TimelineQuery};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentKind, Body, ConversationId};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
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
    // The viewer over a bubble's picture (steps `photo` …): the photo's message, how many
    // viewers had been opened before the first click, the points to click and the window
    // points where the bubble's picture lies.
    property real photo: 0
    property int viewersBefore: 0
    property var viewerAt: null
    // The popups' blank areas over the photo (steps `popup` …): which one, the ones that let
    // the click through, where the popup stood on the previous tick and for how many ticks.
    property int popupIndex: 0
    property var leaks: []
    property string popupSeen: ""
    property int popupRest: 0
    property bool popupMoved: false
    property int timerBefore: 0
    // The call that rings, and the lowest opacity its icon showed while it did.
    property real ringing: 0
    property real faded: 1
    // The core's clock reads the system clock plus `clockShift` (ms); the call stops ringing on it at
    // `ringEnd`. The ring is counted from the start of that clock, not from the moment this window is up.
    readonly property real clockShift: CLOCK_SHIFT
    readonly property real ringEnd: RING_END
    // When the pulse was first seen running in the window that is watched, and the last tick it still was.
    property real pulseSince: 0
    property real pulseLast: 0
    // What the ring-dependent steps are, one line for the log.
    property string ringProofs: "all"
    // Another window, to take the focus away from this one for a moment.
    property var other: null
    // The details panel (steps `person` …): a pointer probe under it, the sizes walked, the
    // group a click opened and which way another chat is opened.
    property var probe: null
    property int sizeIndex: 0
    property int how: 0
    property real groupId: 0
    // The group page (steps group, member, via, keys …): who she is, how many answers came, what was typed and
    // picked, and the step that follows the way to her page.
    property string herUser: ""
    property string herName: ""
    // Every member of the group when the members level opened (before anything was typed into its search).
    property var allMembers: []
    property real herChat: 0
    property int personLoads: 0
    property int groupLoads: 0
    property int loadsBefore: 0
    property int viewersMark: 0
    property string after: ""
    property string typed: ""
    property string pickedUser: ""
    property bool markupDone: false
    property int downs: 0
    property real scrollSpot: 0
    property int herTimerWas: 0
    property int groupTimerWas: 0
    property int groupPageTimerWas: 0
    property int timerPick: 0
    // F4 (steps `tp…`): the selector of her page applies only on Apply. The time her chat had, the number of
    // notices her timeline had, the first and the second choice (seconds), the second one's label, and her page as
    // it was before a step changed it by script.
    property int tpWas: 0
    property int tpRows: 0
    property int tpPick: 0
    property int tpOther: 0
    property int tpTries: 0
    property string tpLabel: ""
    // F1 (steps `dx…`): a person's about text and a group's description are selectable, with links in an accepted
    // chat. The address of the link in her about text, and the group's page as it was before a step changed it by script.
    property string dxLink: ""
    property var dxGroup: ({})
    // P3 (steps `sn…`): the senders and the mentions of the group's added messages. `snIds` are their
    // rows by the word they start with; `sender` is the person a click should open; `snCase` walks the
    // popups (0 the message menu, 1 the reaction picker, 2 the emoji picker) and `snTarget` what lies
    // under them (0 the sender's name, 1 the sender's picture).
    property var snIds: ({})
    property var sender: null
    property int snCase: 0
    property int snTarget: 0
    property var snLeaks: []
    property bool snMoved: false
    property string snSeen: ""
    property int snRest: 0
    property int snPersonLoads: 0
    property var snMention: null
    // The Qt thread's CPU when the group's chat was asked for (what its rows cost, printed once).
    property real snCpu: 0
    // The rows the members' search left, and which end of the mirrored name line the text is drawn at.
    property int snWant: 0
    property bool snRight: false
    // The menu's pages (steps sm*): which entry is chosen, whose page it should open, whether it is the user's own,
    // the row asked about, the entries it should have, and whether the chat is the 1:1 one.
    property int snPick: 0
    property string snExpect: ""
    property bool snSelf: false
    property real snRow: 0
    property var snWantPages: []
    property bool snDirect: false
    // The menu's shape (steps smShape): which row of the cases, the entries wanted and whether a shape is wanted.
    property int snShape: 0
    property string snShapeWant: ""
    property bool snWantShape: false
    // P4 (steps md*): the newest picture of the chat the section starts with, how many pictures the
    // chat holds, and the squares' number of the group.
    property real mdFirst: 0
    property int mdExpected: 0
    // Tab presses and the names the keyboard went through, the member's square (its message and full picture), the
    // columns of the level's grid.
    property int mdTabs: 0
    // The safety-number level (P5a steps sv*): her chat's place in the list and the timeline's system rows before a
    // click, and the presses of Tab mdKeys took.
    property int svIndex: -1
    property int svSystem: 0
    // The steps of a key change (svChanged ..): the number the level showed first (compared, never printed), the own
    // message that waits for the Confirm, whether the grab of the QR code was saved.
    property string svDigits: ""
    // The time just before the buttons were deadened (W4), or just before the click or key that opened the level (X1).
    property real svCoolAt: 0
    // The system rows before a click on a button that must be dead (X1), and whether the menu entry of the Default
    // peer was already picked by a click (svDefPick: the second round picks it by the keyboard).
    property int svRows: 0
    property bool svDefMouseDone: false
    // When the level of the person without a key was first seen without a number (Y1; 0: not yet).
    property real svLateAt: 0
    // The window point of a click on the top edge of Confirm (Y2).
    property var svConfirmPoint: null
    // P5b "Copy Link": the person page's count of copies before a press, and what the clipboard held.
    property int lkCopies: 0
    property string lkPasted: ""
    // P5c "Share Contact…": the page's count of cards before a send, the picker's sends, the target chat's title.
    property int shCards: 0
    property string shTitle: ""
    property string shName: ""
    property var shPerson: ({})
    // The person page's scroll bar settles on a page that just fits (P5c flap): the window's height before the steps, and
    // what the page looked like at each tick (its content's width and height, a square's side).
    property real pjHeight: 0
    property var pjSeen: []
    // Frames the window drew (a tick of the step timer is not a frame: a loaded machine ticks several times for one), and
    // the count when the window took its new height.
    property int frames: 0
    property int pjFrames: 0
    onFrameSwapped: win.frames += 1
    // What the app's style does with the page's scroll bar, played in this test's style (see `pjReserve`).
    property var pjReserve: null

    // The share picker of the person's page while it is open (P5c), or null.
    function sharePicker(): var {
        return win.chat.panel ? win.chat.panel.shareDialog : null;
    }
    // An item of the open picker by object name (the dialog lives in the window's overlay), or null.
    function inPicker(name: string): var {
        return win.find(win.overlay, c => c.objectName === name && c.visible);
    }
    // The picker's footer button with this text (its Kirigami actions), or null.
    function pickerButton(text: string): var {
        return win.find(win.overlay, c => c.text === text && c.visible && c.clicked !== undefined && c.objectName !== "shareRow");
    }
    // The row index of chat `conv` in the picker's rows (-1: not listed).
    function shareIndex(conv: real): int {
        for (let i = 0; i < App.shareTargets.count; ++i) {
            if (App.shareTargets.conversationAt(i) === conv) return i;
        }
        return -1;
    }
    // The person page item (counts of copies and cards).
    function personPageItem(): var {
        return win.find(win.chat.panel, c => c.cardsSent !== undefined && c.visible);
    }
    property bool svSeen300: false
    property bool svSeen900: false
    property real svPaused: 0
    property real svDefault: 0
    property bool qrSaved: false
    property string mdNames: ""
    property real mdMessage: 0
    property string mdFull: ""
    property int mdColumns: 0
    // A member of the grown group whose profile name is markup: shown as the text it is.
    readonly property string markup: "<b>Bold</b> name"
    readonly property real gu: Kirigami.Units.gridUnit

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
            required property string receipt
            required property string wait
            required property var pages
        }
    }

    Instantiator {
        id: senders
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property string authorId
            required property string author
            required property bool showAuthor
            required property bool showAvatar
            required property var blocks
        }
    }

    // The row whose first text holds `part` (the messages the group of the P3 steps was given).
    function rowWith(part: string): real {
        for (let i = 0; i < senders.count; ++i) {
            const r = senders.objectAt(i);
            if (r && r.kind === "message" && r.blocks.length > 0 && r.blocks[0].type === "text" && r.blocks[0].plain.indexOf(part) >= 0) {
                return r.messageId;
            }
        }
        return 0;
    }

    function rowInfoOf(id: real): var {
        for (let i = 0; i < senders.count; ++i) {
            const r = senders.objectAt(i);
            if (r && r.messageId === id) {
                return r;
            }
        }
        return null;
    }

    // The sender's name and picture of a row's delegate (null: not shown).
    function nameOf(d: var): var {
        return win.find(d, c => c.objectName === "senderName" && c.visible);
    }
    function pictureOf(d: var): var {
        return win.find(d, c => c.objectName === "senderPicture" && c.visible);
    }
    // The text of a row's first text block.
    function editOf(d: var): var {
        return win.find(d, c => c.hoveredLink !== undefined && c.selectByMouse === true);
    }
    function textBlockOf(d: var): var {
        return win.find(d, c => c.spoilerRects !== undefined && c.revealed !== undefined);
    }

    // What stands in the overlay (class names only).
    function overlayInfo(): string {
        const out = [];
        for (const c of QQC2.Overlay.overlay.children) {
            out.push(String(c).split("(")[0] + (c.visible ? "+" : "-") + Math.round(c.width) + "x" + Math.round(c.height));
        }
        return out.join(",");
    }

    // The row lies wholly inside the timeline's viewport.
    function shownInTimeline(row: var): bool {
        const p = row.mapToItem(win.view, 0, 0);
        return p.y >= 0 && p.y + row.height <= win.view.height;
    }

    // A real click `dx`, `dy` pixels into an item.
    function clickAt(item: var, dx: real, dy: real): void {
        const p = item.mapToItem(null, dx, dy);
        win.clickWindow(p.x, p.y);
    }

    // The window point of the characters at `index` of a text (a little into the glyph).
    function pointInText(edit: var, index: int): var {
        const r = edit.positionToRectangle(index);
        return edit.mapToItem(null, r.x + 3, r.y + r.height / 2);
    }

    // The window point `dx` pixels from the caret's place at `index` of a text.
    function pointNear(edit: var, index: int, dx: real): var {
        const r = edit.positionToRectangle(index);
        return edit.mapToItem(null, r.x + dx, r.y + r.height / 2);
    }

    // A real click with the right button at a point of the window.
    function rightClickWindow(x: real, y: real): void {
        const p = win.contentItem.mapFromItem(null, x, y);
        if (!win.ev.mouseClick(win.contentItem, p.x, p.y, Qt.RightButton, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
    }

    // What the clipboard holds, read back by a paste into an editor of the test's own (compared, never printed).
    function clipboardText(): string {
        const e = Qt.createQmlObject("import QtQuick; TextEdit { visible: false }", win, "dxPaste");
        e.paste();
        const t = e.text;
        e.destroy();
        return t;
    }

    // The visible entry of an open menu in the overlay, or null.
    function menuEntry(name: string): var {
        return win.find(QQC2.Overlay.overlay, c => c.objectName === name && c.visible);
    }

    // The label of the "Open Link?" question for `url`, or null.
    function linkQuestion(url: string): var {
        return win.find(QQC2.Overlay.overlay, c => c.text === win.linkText(url) && c.visible);
    }

    // A button of the question for `url` ("open" or "cancel"), or null.
    function linkButton(url: string, which: string): var {
        const label = win.linkQuestion(url);
        if (!label) {
            return null;
        }
        const box = win.find(label.parent, c => c.standardButton !== undefined);
        return box ? box.standardButton(which === "open" ? QQC2.DialogButtonBox.Open : QQC2.DialogButtonBox.Cancel) : null;
    }

    // The window point on the `which`-th mention of a text (it is shown as "@Name").
    function mentionPoint(d: var, which: int): var {
        const edit = win.editOf(d);
        const plain = edit.getText(0, edit.length);
        let at = -1;
        for (let n = 0; n <= which; ++n) {
            at = plain.indexOf("@", at + 1);
            if (at < 0) {
                return null;
            }
        }
        return win.pointInText(edit, at + 2);
    }

    // The page entries of the open message menu (shown ones, in the menu's order).
    function menuPages(m: var): var {
        const out = [];
        for (let i = 0; i < m.count; ++i) {
            const it = m.itemAt(i);
            if (it && it.objectName === "menuPage" && it.visible) {
                out.push(it);
            }
        }
        return out;
    }

    // One key towards `target` in the open menu: Return once it is the highlighted entry, else Down; true after the Return.
    function menuPick(m: var, target: var): bool {
        if (m.currentIndex >= 0 && m.itemAt(m.currentIndex) === target) {
            win.key(Qt.Key_Return, Qt.NoModifier);
            return true;
        }
        win.key(Qt.Key_Down, Qt.NoModifier);
        return false;
    }

    function snPopupFor(i: int): var {
        return i === 0 ? win.view.messageMenu : i === 1 ? win.view.pickerPopup : win.composerItem.emojiPopup;
    }
    readonly property var snPopupNames: ["the message menu", "the reaction picker", "the emoji picker"]

    function snOpenPopup(i: int, d: var): void {
        if (i === 0) {
            TimelineActions.menu(win.nameOf(d), d.rowInfo(), 0, 0);
        } else if (i === 1) {
            d.openPicker();
        } else {
            const button = win.find(win.composerItem, c => c.icon !== undefined && c.icon !== null && c.icon.name === "smiley");
            if (!button) {
                return win.fail("the composer's emoji button");
            }
            win.ev.mouseClick(button, button.width / 2, button.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
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

    // How long the last call has left to ring (ms; below 0 once its time is out), on the core's clock.
    function ringLeft(): real {
        return win.ringEnd - (Date.now() + win.clockShift);
    }

    // Where the ring stands in the row `ringing` of the open chat: "rings"; "over" (the row says missed and the
    // time is out); "early" (the row says missed with time left: a fault; the deadline task settles a ring at
    // its deadline and never before, and the clock is read after the row, 100 ms of slack for the shift's error);
    // "gone" (no such row).
    function ringState(): string {
        const r = win.row(win.ringing);
        if (r === null) return "gone";
        if (r.text === "Incoming video call…") return "rings";
        return win.ringLeft() > 100 ? "early" : "over";
    }

    // The ring ran out before the steps could look at it: the proofs that need a ringing row (the pulse, the
    // fade) are not made in this run. Said in the log, so it shows how often a start was that slow.
    function ringRanOut(why: string): void {
        win.ringProofs = "skipped";
        console.warn("chat_ui: the ring had run out (" + Math.round(win.ringLeft()) + " ms) " + why + ": the pulse and the fade were not fully checked");
    }

    // A step that proved the ring found it over: right when the row says missed and the time is out, a fault
    // otherwise. The other window goes, and the step `missed` checks the row.
    function ringEndedOrFail(why: string): void {
        const state = win.ringState();
        if (state !== "over") return win.fail("the ring's row is '" + state + "' " + why + ", " + Math.round(win.ringLeft()) + " ms left to ring");
        if (win.other !== null) {
            win.other.destroy();
            win.other = null;
            win.requestActivate();
        }
        win.ringRanOut(why);
        win.next("missed");
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

    // The ImageBlock of the single picture in the row of `photo` (null: not made yet).
    function photoBlock(): var {
        const d = win.view.delegateFor(win.photo);
        if (d === null) {
            return null;
        }
        for (const b of d.blockItems()) {
            if (b.picW !== undefined && b.mediaAction !== undefined) {
                return b;
            }
        }
        return null;
    }

    // A rectangle of `item`, given in its coordinates, as the window's {x, y, w, h}.
    function inWindow(item: Item, x: real, y: real, w: real, h: real): var {
        const p = item.mapToItem(null, x, y);
        return {
            "x": p.x,
            "y": p.y,
            "w": w,
            "h": h
        };
    }

    // The point (window coordinates) is inside `r`, `margin` px in from its edges (negative:
    // out by that much).
    function inside(r: var, x: real, y: real, margin: real): bool {
        return x >= r.x + margin && x <= r.x + r.w - margin && y >= r.y + margin && y <= r.y + r.h - margin;
    }

    // What the open viewer paints of its picture, in the window's coordinates.
    function viewerPicture(v: var): var {
        const still = win.find(v.contentItem, c => c.objectName === "still");
        return win.inWindow(still, (still.width - still.paintedWidth) / 2, (still.height - still.paintedHeight) / 2, still.paintedWidth, still.paintedHeight);
    }

    // A real click (press and release through the window, as the platform delivers them) at
    // a point of the window.
    function clickWindow(x: real, y: real): void {
        const p = win.contentItem.mapFromItem(null, x, y);
        if (!win.ev.mouseClick(win.contentItem, p.x, p.y, Qt.LeftButton, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
    }

    // The rows of `App.groupMembers` (P2b: a Rust list model, listed in the page's order and narrowed by its
    // filter): `{user, name, avatarUrl, admin, isSelf}` each. The role ids are `FIRST_ROLE + index`.
    function members(): var {
        const m = App.groupMembers;
        const out = [];
        for (let i = 0; i < m.count; i++) {
            const at = m.index(i, 0);
            out.push({
                "user": m.data(at, 257),
                "name": m.data(at, 258),
                "avatarUrl": m.data(at, 259),
                "admin": m.data(at, 260),
                "isSelf": m.data(at, 261)
            });
        }
        return out;
    }

    // Every item below `root` that `pred` accepts.
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

    // The item lies inside the page stack (a pixel of rounding allowed).
    function within(item: Item): bool {
        const p = item.mapToItem(win.pageStack, 0, 0);
        return p.x >= -1 && p.y >= -1 && p.x + item.width <= win.pageStack.width + 1 && p.y + item.height <= win.pageStack.height + 1;
    }

    // The panel's item named `name`, or null.
    function inPanel(name: string): var {
        return win.chat.panel ? win.find(win.chat.panel, c => c.objectName === name) : null;
    }

    // The first VISIBLE item of the panel with this object name (the levels under the one on
    // top are kept alive, hidden: inPanel may find them).
    function shown(name: string): var {
        return win.chat.panel ? win.find(win.chat.panel, c => c.objectName === name && c.visible) : null;
    }

    // The place of the chat `id` in the conversation list (-1: not there).
    function listIndex(id: real): int {
        for (let i = 0; i < convs.count; ++i) {
            const c = convs.objectAt(i);
            if (c && c.conversationId === id) return i;
        }
        return -1;
    }

    // How many system rows (notices) the open timeline has.
    function systemRows(): int {
        let n = 0;
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            n += r && r.kind === "system" ? 1 : 0;
        }
        return n;
    }

    // The text of the newest system row of the open timeline (the one with the highest message id), "" without one.
    function newestSystemText(): string {
        let id = -1;
        let text = "";
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "system" && r.messageId > id) {
                id = r.messageId;
                text = r.text;
            }
        }
        return text;
    }

    // The keyboard as it is after a rebuilt button was destroyed: on the level's scroll view (a focus scope) and on no
    // button (a scope hands its active focus to the child that has `focus`, so the buttons give theirs up first).
    function parkOnScroll(): bool {
        for (const b of win.findAll(win.chat.panel, c => c.objectName === "safetyButton")) {
            b.focus = false;
        }
        win.shown("safetyScroll").forceActiveFocus();
        return win.focusName() === "safetyScroll";
    }

    // The safety-number level's own item (its `pending`, `cooling` and the record of its last announcement).
    function safetyPageItem(): var {
        return win.find(win.chat.panel, c => c.objectName === "safetyPage");
    }

    // The actions of the visible buttons of the safety-number level, in order.
    function safetyActions(): string {
        return win.findAll(win.chat.panel, c => c.objectName === "safetyButton" && c.visible).map(b => b.safetyAction).join();
    }

    // The visible button of the safety-number level that does `action` (null: none).
    function safetyButton(action: string): var {
        return win.findAll(win.chat.panel, c => c.objectName === "safetyButton" && c.visible && c.safetyAction === action)[0] ?? null;
    }

    // The digits of the safety-number level as one string (compared, never printed).
    function safetyDigits(): string {
        return win.findAll(win.chat.panel, c => c.objectName === "safetyGroup" && c.visible).map(g => g.text).join(" ");
    }

    // The newest own message of the open timeline (the highest id; null: none).
    function newestOwn(): var {
        let best = null;
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.own && (best === null || r.messageId > best.messageId)) best = r;
        }
        return best;
    }
    function rowOf(id: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) return r;
        }
        return null;
    }

    // What is wrong with the name label of a member row that shows markup, or "".
    function labelProblem(row: var): string {
        const label = win.find(row, c => c.objectName === "memberName");
        if (!label) return "the row has no name label";
        if (label.text !== win.markup) return "the row does not show the profile name as it is";
        if (label.textFormat !== Text.PlainText) return "the row's name is not plain text";
        return "";
    }

    // The panel is open and has the person's page.
    function panelReady(): bool {
        return win.chat.panelOpen && win.chat.panel !== null && win.chat.panel.loaded;
    }

    // The item lies in the covered chat: the timeline or the composer.
    function inChat(item: var): bool {
        for (let p = item; p; p = p.parent) {
            if (p === win.view || p === win.composerItem) {
                return true;
            }
        }
        return false;
    }

    // What has the keyboard, by its object name (never a text).
    function focusName(): string {
        const f = win.activeFocusItem;
        return f ? (f.objectName !== "" ? f.objectName : "(unnamed)") : "(nothing)";
    }

    // A real click on an item.
    function clickItem(item: var): void {
        const p = item.mapToItem(null, item.width / 2, item.height / 2);
        win.clickWindow(p.x, p.y);
    }

    // The page's drop area (files dragged in).
    function dropArea(): var {
        return win.find(win.chat, c => c.keys !== undefined && c.containsDrag !== undefined);
    }

    // A real click on the chat's header (the way into the panel).
    function clickHeader(): void {
        const h = win.chat.shownHeader();
        if (!h) {
            return win.fail("the chat's header is not on screen");
        }
        const p = h.mapToItem(null, h.width / 2, h.height / 2);
        win.clickWindow(p.x, p.y);
    }

    // A probe over the whole timeline, UNDER the panel: it counts the presses and wheel
    // events that get through.
    function installProbe(): void {
        win.probe = Qt.createQmlObject("import QtQuick\nMouseArea {\n property int presses: 0\n property int wheels: 0\n anchors.fill: parent\n acceptedButtons: Qt.AllButtons\n onPressed: presses += 1\n onWheel: wheel => { wheels += 1; wheel.accepted = true }\n}", win.view, "chat_ui_probe.qml");
    }

    // The sizes the panel is checked at: the window, the rail, and what the page does.
    function sizes(): var {
        return [
            {
                "w": 1200,
                "h": 800,
                "rail": false,
                "mode": "beside"
            },
            {
                "w": 1200,
                "h": 800,
                "rail": true,
                "mode": "over"
            },
            {
                "w": Math.round(win.gu * 64),
                "h": Math.round(win.gu * 42),
                "rail": false,
                "mode": "beside"
            },
            {
                "w": 600,
                "h": 800,
                "rail": false,
                "mode": "alone"
            }
        ];
    }

    // The popups that cover part of a bubble: the reaction picker, the list of who reacted,
    // the composer's emoji picker and the question before a link opens.
    readonly property var popupNames: ["the reaction picker", "the list of who reacted", "the emoji picker", "the question before a link"]
    readonly property string linkUrl: "https://example.invalid/kourier-test"

    // The text of the question's address label: the address between a left-to-right isolate and its
    // pop (U+2066 .. U+2069, H1: the label always reads left to right). The checks that look for the
    // question, or for its absence, compare with this, so they keep their bite.
    function linkText(url: string): string {
        return "\u2066" + url + "\u2069";
    }

    function popupFor(i: int): var {
        switch (i) {
        case 0:
            return win.view.pickerPopup;
        case 1:
            return win.view.reactionsPopup;
        case 2:
            return win.composerItem.emojiPopup;
        }
        return null;
    }

    // The popup's own item (background, header and content), once it exists.
    function popupItemFor(i: int): var {
        if (i === 3) {
            const label = win.find(QQC2.Overlay.overlay, c => c.text === win.linkText(win.linkUrl));
            return label ? label.parent : null;
        }
        const popup = win.popupFor(i);
        return popup && popup.contentItem ? popup.contentItem.parent : null;
    }

    function openPopup(i: int): void {
        const d = win.view.delegateFor(win.photo);
        if (i === 0) {
            d.openPicker();
        } else if (i === 1) {
            d.openReactions();
        } else if (i === 2) {
            const button = win.find(win.composerItem, c => c.icon !== undefined && c.icon !== null && c.icon.name === "smiley");
            if (!button) {
                return win.fail("the composer's emoji button");
            }
            win.ev.mouseClick(button, button.width / 2, button.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
        } else {
            const text = win.find(win.view, c => c.selectByMouse === true && c.linkActivated !== undefined);
            if (!text) {
                return win.fail("a text of the timeline to follow a link from");
            }
            text.linkActivated(win.linkUrl);
        }
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
        target: App
        function onPersonLoaded(): void {
            win.personLoads += 1;
        }
        function onGroupLoaded(): void {
            win.groupLoads += 1;
        }
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
                // The list says what the newest line is, and counts the missed call. The call rings from the
                // start of the core's clock (`ringEnd`), not from this moment: a window that came up after
                // the ring ran out (a loaded machine) finds it missed, and that is right exactly then.
                const call = win.listRow(CALLS);
                if (call === null) return win.fail("the chat with calls is not listed");
                const rings = call.preview === "Incoming video call\u2026" && call.previewIcon === "call-incoming";
                const missed = call.preview === "Missed video call" && call.previewIcon === "call-missed";
                if (!rings && !missed) return win.fail("the list's preview of the call: '" + call.preview + "' with " + call.previewIcon);
                // (The clock is read after the row: a deadline task settles a ring at its deadline, never before.)
                if (missed && win.ringLeft() > 100) return win.fail("the call is missed in the list with " + Math.round(win.ringLeft()) + " ms left to ring");
                // The earlier missed call counts as unread. The ring that ran out counts too when the deadline
                // task settled it after the ingest (a ring that was past its time at the ingest keeps its read mark).
                if (call.unread < 1 || call.unread > (missed ? 2 : 1)) return win.fail("the missed call counts as one unread message in the list" + (missed ? " (two when the ring settled later)" : "") + ": " + call.unread);
                if (missed) win.ringRanOut("when the window was up");
                win.openConversation(CALLS);
                win.next("calls");
                return;
            }
            case "calls": {
                if (App.timeline.conversationId !== CALLS || App.timeline.loading || rows.count < 10) return win.later("the chat with calls");
                const lines = win.callLines();
                // The newest line is the call of the ring: ringing, or missed once its time is out.
                const rings = lines.length > 0 && lines[0] === "Incoming video call\u2026|call-incoming|ringing";
                const expected = [
                    rings ? "Incoming video call\u2026|call-incoming|ringing" : "Missed video call|call-missed|negative",
                    "Missed call|call-missed|negative",
                    "Unanswered voice call|call-outgoing|",
                    "Outgoing video call|call-outgoing|",
                    "Voice call \u00b7 answered on another device|call-start|",
                    "Declined video call|call-stop|negative",
                    "Missed voice call|call-missed|negative"
                ];
                if (lines.join("\n") !== expected.join("\n")) return win.fail("the call lines:\n" + lines.join("\n"));
                // The one that rings (or rang): its row, its icon fading.
                let ring = null;
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "system" && r.text === (rings ? "Incoming video call\u2026" : "Missed video call")) ring = r;
                }
                if (ring === null) return win.fail("the row of the call that rings is not there");
                if (rings && ring.tone !== "ringing") return win.fail("the ringing row's tone: " + ring.tone);
                win.ringing = ring.messageId;
                const d = win.view.noticeFor(win.ringing);
                if (d === null) return win.later("the ringing row on screen");
                if (!rings) {
                    // The ring ran out before this step looked: only the missed form is left to check (step `missed`).
                    if (win.ringLeft() > 100) return win.fail("the call is missed in its chat with " + Math.round(win.ringLeft()) + " ms left to ring");
                    if (win.ringProofs !== "skipped") win.ringRanOut("when its chat opened");
                    win.next("missed");
                    return;
                }
                // The ring may run out while this step waits: then there is nothing left to pulse.
                if (win.ringState() !== "rings") return win.ringEndedOrFail("while its chat opened");
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
                // (It may have run out meanwhile: right when its time is out, a fault before.)
                if (win.ringState() !== "rings") return win.ringEndedOrFail("while another window was the active one");
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
                    // How long the pulse has been seen running (the fade down takes 0.8 s of it).
                    if (win.pulseSince === 0) win.pulseSince = Date.now();
                    win.pulseLast = Date.now();
                    return win.later("the ring to run out");
                }
                // Out of the ring: the same row says what the call became (right when its time is out).
                if (win.ringState() === "early") return win.fail("the call is missed with " + Math.round(win.ringLeft()) + " ms left to ring");
                if (win.faded > 0.9) {
                    // A ring that ran out soon after the pulse began was not watched long enough to show a fade.
                    const watched = win.pulseSince === 0 ? 0 : win.pulseLast - win.pulseSince;
                    if (watched >= 2000) return win.fail("the icon never faded while the call rang: " + win.faded + " in " + watched + " ms");
                    win.ringRanOut("after " + watched + " ms of pulse");
                }
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
                win.next("photo");
                return;

            // ---- the viewer over the picture of a bubble (real pointer events) ----
            case "photo": {
                win.photo = App.findMessage("photo");
                if (win.photo <= 0) return win.later("the chat's photo");
                if (win.view.delegateFor(win.photo) === null) {
                    win.focusRow(win.photo);
                    return win.later("the photo's row on screen");
                }
                const b = win.photoBlock();
                if (b === null || !b.local || !b.shown || b.mediaAction !== "view") return win.later("the photo is downloaded and shown");
                if (win.imageViewer !== null && win.imageViewer.visible) return win.fail("a viewer before the click");
                win.viewersBefore = win.viewersOpened;
                // A real click in the middle of the bubble's picture opens the viewer.
                const at = b.mapToItem(null, b.picX + b.picW / 2, b.picY + b.picH / 2);
                const room = win.view.mapToItem(null, 0, 0);
                if (at.y < room.y + 4 || at.y > room.y + win.view.height - 4) return win.fail("the photo is outside the timeline: " + at.y);
                win.clickWindow(at.x, at.y);
                win.next("photoOpened");
                return;
            }
            case "photoOpened": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("a click on the bubble's picture opens the viewer");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows the picture");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("one click opened " + (win.viewersOpened - win.viewersBefore) + " viewers");
                // The viewer's picture and the bubble's picture under it, in the window.
                const b = win.photoBlock();
                const bubble = win.inWindow(b, b.picX, b.picY, b.picW, b.picH);
                const painted = win.viewerPicture(v);
                // Two points of the bubble's picture under the viewer: one the viewer's picture
                // covers, one beside it.
                const left = Math.max(bubble.x, painted.x);
                const right = Math.min(bubble.x + bubble.w, painted.x + painted.w);
                const top = Math.max(bubble.y, painted.y);
                const bottom = Math.min(bubble.y + bubble.h, painted.y + painted.h);
                if (right - left < 20 || bottom - top < 20) return win.fail("the viewer's picture covers none of the bubble's: " + JSON.stringify(bubble) + " under " + JSON.stringify(painted));
                const on = Qt.point((left + right) / 2, (top + bottom) / 2);
                let beside = null;
                for (const c of [Qt.point(bubble.x + 6, bubble.y + bubble.h / 2), Qt.point(bubble.x + bubble.w - 6, bubble.y + bubble.h / 2), Qt.point(bubble.x + bubble.w / 2, bubble.y + 6), Qt.point(bubble.x + bubble.w / 2, bubble.y + bubble.h - 6)]) {
                    if (beside === null && !win.inside(painted, c.x, c.y, -4)) {
                        beside = c;
                    }
                }
                if (beside === null) return win.fail("the bubble's picture lies wholly under the viewer's: " + JSON.stringify(bubble) + " under " + JSON.stringify(painted));
                win.viewerAt = {
                    "viewer": v,
                    "beside": beside
                };
                // A click on the viewer's picture, over the bubble's: the viewer stays, and
                // the bubble under it does not open another.
                win.clickWindow(on.x, on.y);
                win.next("photoStays");
                return;
            }
            case "photoStays": {
                if (win.waits < 10) return win.later("the click settles");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("a click on the viewer's picture opened " + (win.viewersOpened - win.viewersBefore - 1) + " more viewer(s) (the bubble's picture under it took the tap)");
                if (win.imageViewer !== win.viewerAt.viewer || !win.imageViewer.opened) return win.fail("a click on the viewer's picture closed it or replaced it");
                // A click beside the picture, over the bubble's picture again: the viewer
                // closes, and the bubble under it does not open another.
                win.clickWindow(win.viewerAt.beside.x, win.viewerAt.beside.y);
                win.next("photoCloses");
                return;
            }
            case "photoCloses": {
                if (win.waits < 10) return win.later("the click settles");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("a click beside the viewer's picture opened another viewer (the bubble's picture under it took the tap): " + (win.viewersOpened - win.viewersBefore - 1));
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("a click beside the picture closes the viewer");
                win.next("photoClosed");
                return;
            }
            case "photoClosed": {
                if (win.waits < 10) return win.later("the click settles");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("a viewer opened after the click beside the picture closed the viewer");
                // One viewer at a time: asked twice, the first is closed for the second.
                const items = App.viewerItems(win.photo);
                win.viewerAt = win.openAlbumViewer(items, 0, win.photo);
                win.openAlbumViewer(items, 0, win.photo);
                win.next("twice");
                return;
            }
            case "twice": {
                const second = win.imageViewer;
                if (!second || !second.opened) return win.later("the second viewer opens");
                // (A closed viewer is deleted once it has faded; the reference then reads null.)
                if (win.viewerAt !== null && win.viewerAt !== undefined && win.viewerAt.visible) {
                    if (win.waits > 150) return win.fail("the first viewer stays open beside the second");
                    return win.later("the first viewer closes for the second");
                }
                if (!second.visible) return win.fail("the second viewer was closed with the first");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("twiceClosed");
                return;
            }
            case "twiceClosed":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                win.popupIndex = 0;
                win.next("popup");
                return;

            // ---- the blank part of a popup over the bubble's picture ----
            case "popup": {
                if (win.popupIndex >= win.popupNames.length) {
                    if (win.leaks.length > 0) return win.fail("a click on a blank part of a popup reached the picture under it: " + win.leaks.join(", "));
                    win.openConversation(MEDIA);
                    win.next("file");
                    return;
                }
                if (win.view.popupOpen || win.imageViewer !== null && win.imageViewer.visible) return win.later("nothing is open");
                win.openPopup(win.popupIndex);
                win.popupSeen = "";
                win.popupRest = 0;
                win.next("popupOpened");
                return;
            }
            case "popupOpened": {
                const i = win.popupIndex;
                const item = win.popupItemFor(i);
                const popup = win.popupFor(i);
                if (!item || item.width <= 0 || (popup && !popup.opened)) return win.later(win.popupNames[i] + " opens");
                // At rest: the same place on three ticks (it fades or grows in).
                const at = item.mapToItem(null, 0, 0);
                const seen = at.x + "," + at.y + "," + item.width + "," + item.height;
                if (seen !== win.popupSeen) {
                    win.popupSeen = seen;
                    win.popupRest = 0;
                    return win.later(win.popupNames[i] + " comes to rest");
                }
                win.popupRest += 1;
                if (win.popupRest < 3) return win.later(win.popupNames[i] + " stays at rest");
                const b = win.photoBlock();
                const bubble = win.inWindow(b, b.picX, b.picY, b.picW, b.picH);
                // The corner of the popup's own area, a few pixels in: its padding.
                const blank = Qt.point(at.x + 3, at.y + 3);
                if (popup && !win.popupMoved) {
                    // A popup that is not modal can be put where it covers the picture's middle.
                    win.popupMoved = true;
                    popup.x += bubble.x + bubble.w / 2 - blank.x;
                    popup.y += bubble.y + bubble.h / 2 - blank.y;
                    win.popupSeen = "";
                    win.popupRest = 0;
                    return win.later(win.popupNames[i] + " moved over the picture");
                }
                win.popupMoved = false;
                if (!win.inside(bubble, blank.x, blank.y, 2)) return win.fail(win.popupNames[i] + " does not cover the bubble's picture at its corner: " + JSON.stringify(blank) + " in " + JSON.stringify(bubble));
                win.viewersBefore = win.viewersOpened;
                win.clickWindow(blank.x, blank.y);
                win.next("popupClicked");
                return;
            }
            case "popupClicked": {
                if (win.waits < 10) return win.later("the click settles");
                const i = win.popupIndex;
                if (win.viewersOpened !== win.viewersBefore) {
                    win.leaks.push(win.popupNames[i]);
                }
                // Put everything away for the next one.
                if (win.imageViewer !== null && win.imageViewer.visible) {
                    win.imageViewer.close();
                }
                const popup = win.popupFor(i);
                if (popup) {
                    popup.close();
                } else {
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                }
                win.popupIndex += 1;
                win.next("popup");
                return;
            }

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
                // ---- the details panel of a person ----
                // (The custom time's dialog is still up: a modal popup takes the clicks.)
                if (win.chat.timerDialog) win.chat.timerDialog.close();
                // Tall enough for the whole page with seven groups in common.
                win.height = 1100;
                win.next("personClosing");
                return;
            case "personClosing":
                if (win.chat.timerDialog && win.chat.timerDialog.visible) return win.later("the custom time's dialog to close");
                win.openConversation(PERSON);
                win.next("personLoad");
                return;
            case "personLoad": {
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || !win.chat.open) return win.later("the chat of the person");
                // Nothing of the panel exists before it is first asked for.
                if (win.chat.panel !== null || win.chat.panelOpen) return win.fail("the panel was made before it was asked for");
                if (!win.chat.personShown) return win.fail("a 1:1 chat has a person to show");
                win.installProbe();
                // The header is the way in: a real click on it.
                win.clickHeader();
                win.next("personNarrow");
                return;
            }
            case "personNarrow": {
                if (!win.panelReady()) return win.later("the person's page (open " + win.chat.panelOpen + ", panel " + (win.chat.panel !== null) + ", loaded " + (win.chat.panel !== null && win.chat.panel.loaded) + ", shown " + win.chat.personShown + ")");
                // 520 px wide: the chat page is the window, so the panel is alone over the chat.
                if (win.chat.panelBeside || !win.chat.panelCovers) return win.fail("a narrow window shows the panel over the chat");
                if (win.chatShown) return win.fail("the chat counts as shown while the panel covers it");
                const panel = win.chat.panel;
                if (!win.within(panel) || Math.abs(panel.width - win.chat.width) > 1) return win.fail("the panel does not fill the chat page: " + panel.width + " of " + win.chat.width);
                if (!panel.activeFocus) return win.fail("the keyboard is not in the panel");
                // The page: her name (the chat's title), the about text, the number, the timer
                // and the groups; texts are compared, never printed.
                const name = win.inPanel("detailsName");
                if (!name || name.text !== App.timeline.title) return win.fail("the panel's name is not the chat's title");
                const about = win.inPanel("detailsAbout");
                if (!about || !about.visible || about.text.length === 0) return win.fail("the about text is not shown");
                const number = win.inPanel("detailsNumber");
                if (!number || !number.visible || number.text.length === 0) return win.fail("the number she shares is not shown");
                if (!win.inPanel("detailsTimer").visible) return win.fail("the disappearing-message part is not shown");
                const groups = win.findAll(panel, c => c.objectName === "detailsGroup" && c.visible);
                if (groups.length < 2 || groups.length !== Math.min(5, App.person.groups.length)) return win.fail("the groups in common: " + groups.length + " of " + App.person.groups.length);
                // A click and the wheel on a part of the panel with nothing in it reach nothing
                // under it (the probe lies under the panel, over the whole timeline).
                const pr = panel.mapToItem(null, 0, 0);
                const content = win.inPanel("detailsGroups");
                const bottom = content.mapToItem(null, 0, content.height).y;
                const y = pr.y + panel.height - Kirigami.Units.gridUnit;
                if (y <= bottom + Kirigami.Units.gridUnit) return win.fail("no empty part of the panel to click on");
                const x = pr.x + panel.width / 2;
                win.clickWindow(x, y);
                const q = win.contentItem.mapFromItem(null, x, y);
                win.ev.mouseWheel(win.contentItem, q.x, q.y, 0, -120, Qt.NoButton, Qt.NoModifier, -1);
                win.next("personProbe");
                return;
            }
            case "personProbe":
                if (win.waits < 3) return win.later("the events to arrive");
                if (win.probe.presses !== 0) return win.fail("a click on the panel reached the chat under it");
                if (win.probe.wheels !== 0) return win.fail("the wheel over the panel reached the chat under it");
                // The panel is a named pane (the person's name) for a screen reader.
                if (win.chat.panel.Accessible.role !== Accessible.Pane || win.chat.panel.Accessible.name !== App.person.name) return win.fail("the panel has no accessible role and name");
                win.next("focusCover");
                return;
            // ---- the covering panel takes the chat under it out of the keyboard's reach ----
            case "focusCover": {
                // (P4: the person page has its media part, and its stop "All Media and Files", only once the gallery's
                // answer has come, a core query after the person's: the count of Tab presses below is made for the cycle with
                // that stop, so the step waits for it.)
                if (!win.shown("mediaAll")) return win.later("the person page's media part (All Media and Files), whose stop the Tab count is made for");
                // (P5a: and "View Safety Number", one more stop, shown with the page.)
                if (!win.shown("detailsSafety")) return win.later("the person page's View Safety Number, whose stop the Tab count is made for");
                // (P5b: and "Copy Link", one more stop: she shares her number, so the button is enabled.)
                if (!win.shown("detailsLink") || !win.shown("detailsLink").enabled) return win.later("the person page's Copy Link, whose stop the Tab count is made for");
                // (P5c: and "Share Contact…", one more stop: she has a profile name and a shown number, so it is enabled.)
                if (!win.shown("detailsShare") || !win.shown("detailsShare").enabled) return win.later("the person page's Share Contact, whose stop the Tab count is made for");
                if (!win.chat.panelCovers) return win.fail("the panel should cover the chat");
                if (win.view.enabled || win.composerItem.enabled) return win.fail("the covered timeline or composer can still take the keyboard");
                const drop = win.dropArea();
                if (!drop || drop.enabled) return win.fail("files can be dropped on a covering panel");
                const before = win.composerItem.text;
                // (F1: and the about text, one more stop: the keyboard selects in it and opens the menu of its links. The
                // fixture's about text is short enough to need no "More", which would be another stop.)
                if (!win.shown("detailsAbout") || win.shown("detailsAboutMore")) return win.fail("the person page needs an about text that fits its three lines, whose one stop the Tab count is made for");
                win.inPanel("detailsClose").forceActiveFocus();
                // (18 presses, one whole cycle of the page since F1's about text (17 with P5c's "Share Contact…", 16 with
                // P5b's "Copy Link", 15 with P5a's "View Safety Number", 14 in P4): the walk ends on the close button again,
                // where the count of Tab presses below starts.)
                for (let i = 0; i < 18; i++) {
                    win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                    if (win.inChat(win.activeFocusItem)) return win.fail("Shift+Tab reached the covered chat (" + win.focusName() + ", press " + (i + 1) + ")");
                }
                win.chat.panel.forceActiveFocus();
                // (34 presses, not 24: the media part of the person page has one more stop, "All Media and Files" (P4: 26),
                // P5a adds "View Safety Number" (28), P5b "Copy Link" (30), P5c "Share Contact…" (32) and F1 the about text,
                // so the cycle is 18 stops and 34 ends on the same item as 24 did in 13 (count = 2 x cycle - 2): the first
                // unnamed toolbar item after the header, where the Space typed below is harmless.)
                const names = [];
                for (let i = 0; i < 34; i++) {
                    win.key(Qt.Key_Tab, Qt.NoModifier);
                    names.push(win.focusName());
                    if (win.inChat(win.activeFocusItem)) return win.fail("Tab reached the covered chat (" + win.focusName() + ", press " + (i + 1) + ")");
                }
                // The count is made for that item: a changed cycle (another stop on the page) fails here, by name, and not
                // later in `groupTabs` after the typed Space closed the panel.
                const endsOn = names[names.length - 1];
                const after = names[names.length - 2];
                if (names.length !== 34 || after !== "chatHeader" || endsOn !== "(unnamed)") return win.fail(names.length + " presses of Tab end on '" + endsOn + "' after '" + after + "', not on the unnamed item right after chatHeader, where the count of 34 was made; the cycle of the page is: " + names.slice(0, 20).join(", "));
                // Typing wherever the keyboard is does not write into the composer under the panel.
                for (const k of [Qt.Key_H, Qt.Key_I, Qt.Key_Space]) win.key(k, Qt.NoModifier);
                if (win.composerItem.text !== before) return win.fail("typed text landed in the covered composer");
                win.next("groupTabs");
                return;
            }
            // ---- the rows of the groups in common take the keyboard ----
            case "groupTabs": {
                const panel = win.chat.panel;
                const shown = Math.min(5, App.person.groups.length);
                if (App.person.groups.length < 6) return win.fail("the fixture person needs six groups or more: " + App.person.groups.length);
                // (P5a, BQ-11: "View Safety Number" stands between the timer and the groups; P5b puts "Copy Link" right
                // under it and P5c "Share Contact…" under that, so the walk starts on Share Contact, the last stop before
                // the groups.)
                win.inPanel("detailsShare").forceActiveFocus();
                for (let i = 0; i < shown; i++) {
                    win.key(Qt.Key_Tab, Qt.NoModifier);
                    const f = win.activeFocusItem;
                    if (!f || f.objectName !== "detailsGroup") return win.fail("Tab did not reach group row " + i + " (" + win.focusName() + ")");
                    if (f.text !== App.person.groups[i].title) return win.fail("group row " + i + " is not the " + i + ". group in common");
                    if (!f.visualFocus) return win.fail("the focused group row shows no focus");
                }
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsShowAll") return win.fail("Tab after the rows reaches " + win.focusName() + ", not Show All");
                // Space on it opens the page of all the groups.
                win.key(Qt.Key_Space, Qt.NoModifier);
                win.next("allGroups");
                return;
            }
            case "allGroups": {
                const panel = win.chat.panel;
                if (!panel.allGroups) return win.later("Show All");
                if (win.waits < 3) return win.later("the page of all groups");
                const rows = win.findAll(panel, c => c.objectName === "detailsGroup" && c.visible);
                if (rows.length !== App.person.groups.length) return win.fail("all groups: " + rows.length + " of " + App.person.groups.length);
                if (win.inPanel("detailsName").visible) return win.fail("the person's head stays above the list of all groups");
                // Back (a real click) is the person's page again.
                win.clickItem(win.inPanel("detailsGroupsBack"));
                win.next("allBack");
                return;
            }
            case "allBack": {
                if (win.chat.panel.allGroups) return win.later("Back");
                if (win.waits < 3) return win.later("the person's page");
                if (!win.inPanel("detailsName").visible || !win.inPanel("detailsTimer").visible) return win.fail("Back did not show the person's page");
                win.clickItem(win.inPanel("detailsShowAll"));
                win.next("allAgain");
                return;
            }
            case "allAgain": {
                if (!win.chat.panel.allGroups) return win.later("Show All again");
                // Escape leaves the list first; the panel stays.
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("allEsc");
                return;
            }
            case "allEsc": {
                if (win.chat.panel.allGroups) return win.later("Escape to leave the list");
                if (!win.chat.panelOpen) return win.fail("Escape closed the panel from the list of all groups");
                if (win.waits < 3) return win.later("the person's page");
                // The about text opened and the list of all groups open, then the X.
                win.inPanel("detailsAbout").expanded = true;
                win.clickItem(win.inPanel("detailsShowAll"));
                win.next("allX");
                return;
            }
            case "allX": {
                if (!win.chat.panel.allGroups) return win.later("Show All before the X");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("allReopen");
                return;
            }
            case "allReopen":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.clickHeader();
                win.next("allReopened");
                return;
            case "allReopened": {
                if (!win.panelReady() || win.waits < 3) return win.later("the panel again");
                if (win.chat.panel.allGroups) return win.fail("the panel reopens on the list of all groups");
                for (const n of ["detailsAvatar", "detailsName", "detailsTimer", "detailsGroups"]) {
                    if (!win.inPanel(n).visible) return win.fail("after the X and a reopen the page lacks " + n);
                }
                if (win.inPanel("detailsAbout").expanded) return win.fail("the about text stays opened for the next person");
                // The same through another way out: a chat opened from the list closes the panel
                // with the list of all groups up.
                win.clickItem(win.inPanel("detailsShowAll"));
                win.next("allChat");
                return;
            }
            case "allChat":
                if (!win.chat.panel.allGroups) return win.later("Show All before another chat");
                win.pickedChat(MEDIA);
                win.next("allChatGone");
                return;
            case "allChatGone":
                if (App.timeline.conversationId !== MEDIA || win.chat.panelOpen) return win.later("another chat to open");
                win.openConversation(PERSON);
                win.next("allChatBack");
                return;
            case "allChatBack":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.waits < 6) return win.later("the person's chat");
                win.clickHeader();
                win.next("allChatShown");
                return;
            case "allChatShown": {
                if (!win.panelReady() || win.waits < 3) return win.later("the panel once more");
                if (win.chat.panel.allGroups || !win.inPanel("detailsName").visible || !win.inPanel("detailsTimer").visible) return win.fail("the panel reopens on the list of all groups after another chat");
                // Escape closes it and the keyboard goes back to the header.
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("personEsc");
                return;
            }
            case "personEsc":
                if (win.chat.panelOpen) return win.later("Escape to close the panel");
                if (win.chat.panel.visible) return win.fail("a closed panel is still visible");
                if (!win.chat.shownHeader().activeFocus) return win.fail("the keyboard did not go back to the header");
                if (!win.chatShown) return win.fail("the chat is not shown again");
                if (App.timeline.conversationId !== PERSON) return win.fail("Escape left the chat");
                // From the keyboard: Return on the header opens it again.
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("personKey");
                return;
            case "personKey":
                if (!win.panelReady()) return win.later("the panel from the keyboard");
                // Alt+Left is the page stack's Back: it closes the panel, it does not leave the chat.
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("personAlt");
                return;
            case "personAlt":
                if (win.chat.panelOpen) return win.later("Alt+Left to close the panel");
                if (App.timeline.conversationId !== PERSON || win.pageStack.currentIndex !== 1 || win.pageStack.lastItem !== win.chat) return win.fail("Back left the chat instead of closing the panel");
                // The keypad's Enter is another key than Return, and opens it as well.
                if (!win.chat.shownHeader().activeFocus) return win.fail("the keyboard is not on the header after Back");
                win.key(Qt.Key_Enter, Qt.KeypadModifier);
                win.next("personEnter");
                return;
            case "personEnter":
                if (!win.panelReady()) return win.later("the panel from the keypad's Enter");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("personEnterEsc");
                return;
            case "personEnterEsc":
                if (win.chat.panelOpen) return win.later("Escape after the keypad's Enter");
                win.clickHeader();
                win.next("personBack");
                return;
            case "personBack": {
                if (!win.panelReady()) return win.later("the panel for the toolbar's Back");
                // The toolbar's arrow.
                const arrows = win.findAll(win.contentItem, c => c.icon !== undefined && c.icon !== null && typeof c.icon.name === "string" && c.icon.name.indexOf("go-previous") === 0 && c.visible && c.width > 0 && c.clicked !== undefined && !win.chat.panel.contains(c.mapToItem(win.chat.panel, 0, 0)));
                if (arrows.length === 0) return win.fail("the toolbar has no Back button");
                const b = arrows[0];
                win.ev.mouseClick(b, b.width / 2, b.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
                win.next("personBackDone");
                return;
            }
            case "personBackDone":
                if (win.chat.panelOpen) return win.later("the toolbar's Back to close the panel");
                if (App.timeline.conversationId !== PERSON || win.pageStack.currentIndex !== 1) return win.fail("the toolbar's Back left the chat");
                // A group in common: a click opens that group and the panel goes.
                win.clickHeader();
                win.next("personGroup");
                return;
            case "personGroup": {
                // (A panel that has just arrived is not laid out yet: a click on its rows is lost.)
                if (!win.panelReady() || win.waits < 3) return win.later("the panel for the groups");
                const groups = win.findAll(win.chat.panel, c => c.objectName === "detailsGroup" && c.visible);
                if (groups.length === 0) return win.fail("no group in common to click");
                win.groupId = App.person.groups[0].id;
                const g = groups[0];
                win.ev.mouseClick(g, g.width / 2, g.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
                win.next("personGroupDone");
                return;
            }
            case "personGroupDone":
                if (App.timeline.conversationId !== win.groupId) return win.later("the group to open");
                if (win.chat.panelOpen) return win.fail("the panel stays open over another chat");
                // (The chat's id is set before its header: a group is waited for, not assumed.)
                if (!App.timeline.isGroup) return win.later("the header of the group in common");
                // The same with the keyboard: Tab onto a row, Return opens that group.
                win.openConversation(PERSON);
                win.next("keyGroupOpen");
                return;
            case "keyGroupOpen":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("the person's chat for the keyboard");
                win.clickHeader();
                win.next("keyGroup");
                return;
            case "keyGroup": {
                if (!win.panelReady() || win.waits < 3) return win.later("the panel for the keyboard");
                win.groupId = App.person.groups[1].id;
                // (P5a, BQ-11: from the last stop before the groups: "View Safety Number" in P5a, "Copy Link" in P5b,
                // "Share Contact…" since P5c.)
                win.inPanel("detailsShare").forceActiveFocus();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsGroup") return win.fail("Tab did not reach the second group row (" + win.focusName() + ")");
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("keyGroupDone");
                return;
            }
            case "keyGroupDone":
                if (App.timeline.conversationId !== win.groupId) return win.later("the group the keyboard opened");
                if (win.chat.panelOpen) return win.fail("the panel stays open over the group the keyboard opened");
                // (The chat's id is set before its header: a group is waited for, not assumed.)
                if (!App.timeline.isGroup) return win.later("the header of the group in common");
                // The sizes: the same person in a wide window, with the rail collapsed and open.
                win.openConversation(PERSON);
                win.sizeIndex = 0;
                win.next("sizeOpen");
                return;
            case "sizeOpen": {
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen) return win.later("the person's chat again");
                const size = win.sizes()[0];
                win.width = size.w;
                win.height = size.h;
                win.globalDrawer.collapsed = !size.rail;
                win.next("sizeOpened");
                return;
            }
            case "sizeOpened":
                if (win.waits < 6) return win.later("the layout");
                win.clickHeader();
                win.next("sizeCheck");
                return;
            case "sizeCheck": {
                if (!win.panelReady()) return win.later("the panel at the size");
                const size = win.sizes()[win.sizeIndex];
                if (win.waits < 6) return win.later("the layout at the size");
                const panel = win.chat.panel;
                const chatBox = win.chat.mapToItem(win.pageStack, 0, 0);
                const list = win.pageStack.get(0);
                if (size.mode !== "alone") {
                    if (!win.wideScreen) return win.fail("the window should be wide at " + size.w);
                    if (!win.within(list)) return win.fail("the list leaves the window at " + size.w);
                }
                if (!win.within(win.chat) || !win.within(panel)) return win.fail("the chat page or the panel leaves the window at " + size.w + " (rail " + size.rail + ")");
                if (size.mode === "beside") {
                    if (!win.chat.panelBeside || win.chat.panelCovers) return win.fail("the panel should stand beside the timeline at " + size.w + ": the page is " + win.chat.width + " wide");
                    const left = panel.mapToItem(null, 0, 0).x;
                    const timelineRight = win.view.mapToItem(null, win.view.width, 0).x;
                    // The timeline keeps at least 20 grid units.
                    if (win.view.width < win.gu * 20 - 1) return win.fail("the timeline is " + win.view.width + " wide");
                    const composer = win.composerItem.contentItem.mapToItem(null, win.composerItem.contentItem.width, 0).x;
                    if (timelineRight > left + 1 || composer > left + 1) return win.fail("the timeline or the composer runs under the panel");
                    if (!win.chatShown) return win.fail("the chat is shown beside the panel");
                    const drop = win.dropArea();
                    const dropRight = drop.mapToItem(null, drop.width, 0).x;
                    if (!drop.enabled || dropRight > left + 1) return win.fail("the drop area runs under the panel");
                } else {
                    if (win.chat.panelBeside || !win.chat.panelCovers) return win.fail("the panel should cover the chat at " + size.w + ": the page is " + win.chat.width + " wide");
                    if (Math.abs(panel.width - win.chat.width) > 1) return win.fail("the covering panel is " + panel.width + " of " + win.chat.width);
                    if (win.chatShown) return win.fail("the chat counts as shown under the panel");
                }
                // Walk on; the panel stays open across the threshold.
                win.sizeIndex += 1;
                if (win.sizeIndex >= win.sizes().length) {
                    win.next("mirrorSet");
                    return;
                }
                const next = win.sizes()[win.sizeIndex];
                win.width = next.w;
                win.height = next.h;
                win.globalDrawer.collapsed = !next.rail;
                win.waits = 0;
                return;
            }
            // The same beside-the-timeline layout under mirroring (a right-to-left language): the
            // panel stands on the trailing side, which is the left one, and the
            // timeline and the composer keep clear of it there.
            case "mirrorSet":
                win.width = 1200;
                win.height = 800;
                win.globalDrawer.collapsed = true;
                win.LayoutMirroring.enabled = true;
                win.LayoutMirroring.childrenInherit = true;
                win.waits = 0;
                win.next("mirrorCheck");
                return;
            case "mirrorCheck": {
                if (win.waits < 8) return win.later("the mirrored layout");
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || !win.chat.panelBeside) return win.fail("the panel should stand beside the timeline when mirrored");
                if (!panel.LayoutMirroring.enabled) return win.fail("the panel is not mirrored: the test does not mirror");
                const page = win.chat.mapToItem(null, 0, 0);
                const left = panel.mapToItem(null, 0, 0).x;
                const right = left + panel.width;
                if (Math.abs(left - page.x) > 1) return win.fail("the mirrored panel does not stand at the page's left edge: " + (left - page.x));
                const tl = win.view.mapToItem(null, 0, 0).x;
                const tr = tl + win.view.width;
                const cl = win.composerItem.contentItem.mapToItem(null, 0, 0).x;
                const cr = cl + win.composerItem.contentItem.width;
                const pageRight = page.x + win.chat.width;
                if (tl < right - 1 || cl < right - 1) return win.fail("the timeline or the composer runs under the mirrored panel (timeline " + (tl - right) + ", composer " + (cl - right) + ")");
                if (tr > pageRight + 1 || cr > pageRight + 1) return win.fail("the timeline or the composer leaves the page");
                if (win.view.width < win.gu * 20 - 1) return win.fail("the mirrored timeline is " + win.view.width + " wide");
                const drop = win.dropArea();
                const dl = drop.mapToItem(null, 0, 0).x;
                if (dl < right - 1) return win.fail("the drop area runs under the mirrored panel");
                win.LayoutMirroring.enabled = false;
                win.next("otherOpen");
                return;
            }
            // Another chat opened any way closes the panel: the list, the step keys, a
            // notification.
            case "otherOpen":
                if (win.chat.panelOpen && win.waits < 2) return win.later("the panel");
                win.width = 1200;
                win.height = 800;
                win.globalDrawer.collapsed = true;
                win.openConversation(PERSON);
                win.next("otherShow");
                return;
            case "otherShow":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.waits < 6) return win.later("the person's chat");
                if (!win.chat.panelOpen) win.clickHeader();
                win.next("otherDo");
                return;
            case "otherDo":
                if (!win.panelReady()) return win.later("the panel before another chat opens");
                if (win.how === 0) win.pickedChat(MEDIA);
                else if (win.how === 1) win.stepConversation(1);
                else win.openFromActivation(MEDIA);
                win.next("otherDone");
                return;
            case "otherDone":
                if (App.timeline.conversationId === PERSON) return win.later("another chat to open");
                if (win.chat.panelOpen) return win.fail("the panel stays open over another chat (way " + win.how + ")");
                win.how += 1;
                if (win.how < 3) {
                    win.openConversation(PERSON);
                    win.next("otherShow");
                    return;
                }
                // ---- her own page ----
                win.openConversation(NOTE);
                win.next("noteLoad");
                return;
            case "noteLoad":
                if (App.timeline.conversationId !== NOTE || App.timeline.loading || win.waits < 6) return win.later("Note to Self");
                if (!App.timeline.isSelf) return win.fail("the chat is not Note to Self");
                win.clickHeader();
                win.next("noteShown");
                return;
            case "noteShown": {
                if (!win.panelReady()) return win.later("the own page");
                if (!App.person.isSelf) return win.fail("Note to Self shows another person");
                const name = win.inPanel("detailsName");
                if (!name || name.text.length === 0) return win.fail("the own page has no name");
                // The chat is the open one: no "Message" button.
                if (win.inPanel("detailsMessage").visible) return win.fail("Message on the page of the open chat");
                // The account is in groups, but its own page has none in common.
                if (App.person.groups.length !== 0 || App.person.groupsMore !== "" || win.inPanel("detailsGroups").visible) return win.fail("the own page lists groups in common");
                win.next("herOpen");
                return;
            }
            // ---- the details panel of a group (people pages P2) ----
            // Who she is (the person the fixture page shows), learnt from her own page.
            case "herOpen":
                win.openConversation(PERSON);
                win.next("herLoad");
                return;
            case "herLoad":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.waits < 6) return win.later("her chat, to learn who she is");
                win.clickHeader();
                win.next("herShown");
                return;
            case "herShown": {
                if (!win.panelReady() || win.waits < 3) return win.later("her page");
                win.herUser = App.person.user;
                win.herName = App.person.name;
                win.herChat = App.person.chat;
                if (win.herUser === "" || win.herChat === 0 || !App.person.timerCustom) return win.fail("the fixture person has a chat that takes custom times");
                if (!/^[A-Za-z]{3}/.test(win.herName)) return win.fail("her name starts with three letters (the search is typed with them)");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("groupOpen");
                return;
            }
            // 1. The group's header opens the group's page (short window: the page scrolls).
            case "groupOpen":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.height = 520;
                win.openConversation(THE_GROUP);
                win.next("groupHeader");
                return;
            case "groupHeader": {
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 6) return win.later("the group's chat");
                if (!win.chat.detailsShown || win.chat.personShown) return win.fail("a group's header opens a page, and it is not a person's");
                if (win.chat.panelOpen) return win.fail("the panel stays open over another chat");
                if (win.probe) win.probe.destroy();
                win.installProbe();
                win.loadsBefore = win.groupLoads;
                win.viewersMark = win.viewersOpened;
                win.clickHeader();
                win.next("groupShown");
                return;
            }
            case "groupShown": {
                if (!win.panelReady() || win.waits < 3) return win.later("the group's page");
                // A NEW answer came for this open: a page that was never asked for would show the last one.
                if (win.groupLoads <= win.loadsBefore) return win.later("a new groupLoaded");
                const panel = win.chat.panel;
                if (panel.level !== "group" || App.group.id !== THE_GROUP) return win.fail("the panel does not show the group");
                const name = win.shown("groupName");
                if (!name || name.text !== App.timeline.title) return win.fail("the panel's name is not the chat's title");
                if (panel.Accessible.name !== App.timeline.title) return win.fail("the panel's accessible name is not the group's");
                const d = win.shown("groupDescription");
                if (!d || d.text.length === 0) return win.fail("the description is not shown");
                const ms = win.members();
                if (ms.length !== App.group.memberCount || App.groupPreview.count !== 5) return win.fail("the member models hold " + ms.length + " and " + App.groupPreview.count + " rows of " + App.group.memberCount);
                if (ms.length < 12 || App.group.membersPreview !== 5 || App.group.membersMore === "") return win.fail("the grown group: " + ms.length + " members, preview " + App.group.membersPreview);
                // The order: you, the administrators, the others.
                if (!ms[0].isSelf) return win.fail("the first member is not you");
                let others = false;
                for (let i = 1; i < ms.length; i++) {
                    if (ms[i].isSelf) return win.fail("you are in the list twice");
                    if (!ms[i].admin) others = true;
                    else if (others) return win.fail("an administrator after the others");
                }
                const rows = win.findAll(panel, c => c.objectName === "groupMember" && c.visible);
                if (rows.length !== 5) return win.fail("the group page shows " + rows.length + " member rows, not 5");
                for (let i = 0; i < 5; i++) {
                    if (rows[i].member.user !== ms[i].user) return win.fail("row " + i + " is not member " + i + " of the page");
                    if (rows[i].member.name === win.markup) {
                        const bad = win.labelProblem(rows[i]);
                        if (bad !== "") return win.fail("the group level: " + bad);
                    }
                }
                // 2. The timer is read-only, with the viewmodel's reason.
                if (win.shown("groupTimerBox")) return win.fail("the group's timer has a combo");
                const value = win.shown("groupTimerBoxValue");
                const note = win.shown("groupTimerBoxNote");
                if (!value || value.text === "") return win.fail("the group's timer has no value line");
                if (!note || note.text === "" || note.text !== App.group.timerNote) return win.fail("the group's timer has no reason");
                // 3. A click over a part of the page that takes none reaches nothing under it.
                win.clickItem(name);
                win.next("groupPlate");
                return;
            }
            case "groupPlate": {
                if (win.waits < 3) return win.later("the click to arrive");
                if (win.probe.presses !== 0) return win.fail("a click on the group page reached the chat under it");
                // 4. The page scrolls here (a place kept on an unscrolled page proves nothing).
                const scroll = win.shown("groupScroll");
                const flick = scroll.contentItem;
                if (flick.contentHeight <= flick.height + 4 * win.gu) return win.fail("the group page does not scroll at this size: " + flick.contentHeight + " in " + flick.height);
                const p = scroll.mapToItem(null, scroll.width / 2, scroll.height / 2);
                const q = win.contentItem.mapFromItem(null, p.x, p.y);
                for (let i = 0; i < 40; i++) {
                    win.ev.mouseWheel(win.contentItem, q.x, q.y, 0, -120, Qt.NoButton, Qt.NoModifier, -1);
                }
                win.next("groupScrolled");
                return;
            }
            case "groupScrolled": {
                const scroll = win.shown("groupScroll");
                const flick = scroll.contentItem;
                if (flick.moving || win.waits < 4) return win.later("the page to come to rest");
                if (win.probe.wheels !== 0) return win.fail("the wheel over the group page reached the chat under it");
                // (The wheel events of the step before do not move this Flickable in the test's
                // default style; the page is taken to its end by its own position.)
                flick.contentY = flick.contentHeight - flick.height;
                win.scrollSpot = flick.contentY;
                if (win.scrollSpot < 2 * win.gu) return win.fail("the group page has too little to scroll: " + win.scrollSpot);
                win.next("groupAtEnd");
                return;
            }
            case "groupAtEnd": {
                if (win.waits < 3) return win.later("the page at its end");
                const scroll = win.shown("groupScroll");
                // 5. A real click on a member row (one on screen, not yours) opens that person's page on top.
                const rows = win.findAll(win.chat.panel, c => c.objectName === "groupMember" && c.visible && !c.member.isSelf);
                const row = rows.find(r => {
                    const at = r.mapToItem(scroll, 0, 0);
                    return at.y >= 0 && at.y + r.height <= scroll.height;
                });
                if (!row) return win.fail("no member row is on screen at the end of the group page");
                win.pickedUser = row.member.user;
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("memberShown");
                return;
            }
            case "memberShown": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded) return win.later("the member's page (a new personLoaded)");
                if (win.waits < 3) return win.later("the member's page");
                if (App.person.user !== win.pickedUser) return win.fail("the page on top is not the member's");
                if (win.shown("groupScroll")) return win.fail("the group page still shows under the member's");
                const kept = win.find(panel, c => c.objectName === "groupScroll");
                if (!kept) return win.fail("the group page was thrown away when a page opened on top of it");
                // (P2b: the rows are a Rust list model updated in place by user id, so a refresh no longer rebuilds the
                // delegates and the old write of renamed rows into the page has no counterpart: the fixture fake cannot
                // rename a contact. A rename is one dataChanged of the name role, proven in tests/models.rs and in the
                // viewmodel's member_list_tests; the opener is still found by its key in `focusOpener()`.)
                // 7. Alt+Left goes one level back.
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("memberBack");
                return;
            }
            case "memberBack": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen) return win.fail("Back closed the whole panel instead of one level");
                if (panel.level !== "group" || win.waits < 3) return win.later("Back to the group page");
                const flick = win.shown("groupScroll").contentItem;
                if (Math.abs(flick.contentY - win.scrollSpot) > 1) return win.fail("the group page lost its place: " + flick.contentY + " of " + win.scrollSpot);
                const f = win.activeFocusItem;
                if (!f || f.objectName !== "groupMember" || f.member.user !== win.pickedUser) return win.fail("the keyboard is not on the member's row (" + win.focusName() + ")");
                // 8. "You" opens your own page, which has no groups; Escape goes back.
                flick.contentY = 0;
                win.next("youClick");
                return;
            }
            case "youClick": {
                if (win.waits < 3) return win.later("the page at its top");
                const row = win.findAll(win.chat.panel, c => c.objectName === "groupMember" && c.visible)[0];
                if (!row || !row.member.isSelf) return win.fail("the first row is not yours");
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("youShown");
                return;
            }
            case "youShown": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("your own page");
                if (!App.person.isSelf || App.person.groups.length !== 0) return win.fail("the 'You' row does not open the own page");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("youBack");
                return;
            }
            case "youBack":
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Escape to the group page");
                // Room for her whole page from here on.
                win.height = 900;
                win.after = "memberTimer";
                win.markupDone = false;
                win.next("viaAll");
                return;
            // ---- the way to her page through "Show All" and the search (`win.after` is the step after) ----
            case "viaAll": {
                if (win.chat.panel.level !== "group" || win.waits < 6) return win.later("the group page at its size");
                const flick = win.shown("groupScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                win.next("viaClick");
                return;
            }
            case "viaClick": {
                if (win.waits < 3) return win.later("the page to scroll");
                const b = win.shown("groupShowAll");
                if (!b) return win.fail("the group page has no Show All");
                win.clickItem(b);
                win.next("viaAllShown");
                return;
            }
            case "viaAllShown": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level");
                const list = win.shown("membersList");
                win.allMembers = win.members();
                if (!list || list.count !== App.group.memberCount || win.allMembers.length !== App.group.memberCount) return win.fail("the members level lists " + (list ? list.count : -1) + " of " + App.group.memberCount);
                win.clickItem(win.shown("membersSearch"));
                // Markup first (F11), then her name.
                win.typed = win.markupDone ? win.herName.slice(0, 3).toLowerCase() : "bold";
                for (const ch of win.typed) {
                    win.key(ch.toUpperCase().charCodeAt(0), Qt.NoModifier);
                }
                win.next("viaTyped");
                return;
            }
            case "viaTyped": {
                const search = win.shown("membersSearch");
                if (search.text.toLowerCase() !== win.typed) return win.later("the typed letters in the field");
                const list = win.shown("membersList");
                const want = win.allMembers.filter(m => App.memberMatches(m.name, search.text)).length;
                if (want === 0) return win.fail("what was typed matches no member");
                if (list.count !== want || win.waits < 2) return win.later("the filtered rows");
                const listed = win.members();
                for (let i = 0; i < list.count; i++) {
                    if (!App.memberMatches(listed[i].name, search.text)) return win.fail("a row that does not match the search is listed");
                }
                if (!win.markupDone) {
                    const hit = win.findAll(win.chat.panel, c => c.objectName === "membersMember" && c.visible).find(r => r.member.name === win.markup);
                    if (!hit) return win.fail("the member whose name is markup is not in the members list");
                    const bad = win.labelProblem(hit);
                    if (bad !== "") return win.fail("the members level: " + bad);
                    win.markupDone = true;
                    // The field's own Escape empties the text and stays.
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                    win.next("viaCleared");
                    return;
                }
                if (!win.allMembers.some(m => m.user === win.herUser && App.memberMatches(m.name, search.text))) return win.fail("her name is not matched by its own first letters");
                win.downs = 0;
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.next("viaDown");
                return;
            }
            case "viaCleared": {
                const search = win.shown("membersSearch");
                if (search.text !== "" || win.shown("membersList").count !== win.allMembers.length) return win.later("Escape to empty the search");
                if (win.chat.panel.level !== "members") return win.fail("Escape in the field left the level");
                win.typed = win.herName.slice(0, 3).toLowerCase();
                for (const ch of win.typed) {
                    win.key(ch.toUpperCase().charCodeAt(0), Qt.NoModifier);
                }
                win.next("viaTyped");
                return;
            }
            case "viaDown": {
                const list = win.shown("membersList");
                if (!list.activeFocus || list.currentIndex < 0) return win.later("Down to reach the list");
                if (list.model.userAt(list.currentIndex) !== win.herUser) {
                    win.downs += 1;
                    if (win.downs > list.count) return win.fail("her row is not in the list");
                    win.key(Qt.Key_Down, Qt.NoModifier);
                    return;
                }
                win.loadsBefore = win.personLoads;
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("viaHer");
                return;
            }
            case "viaHer": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("her page (a new personLoaded)");
                if (App.person.user !== win.herUser) return win.fail("the page on top is not hers");
                win.next(win.after);
                return;
            }
            // 6. Her combo acts on HER chat, never on the open group.
            case "memberTimer": {
                const box = win.shown("detailsTimerBox");
                if (!box) return win.fail("her page has no timer combo");
                win.herTimerWas = App.person.timerSecs;
                win.groupTimerWas = App.timeline.timerSecs;
                win.groupPageTimerWas = App.group.timerSecs;
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== App.person.timerSecs);
                if (at < 0) return win.fail("her combo offers no other time");
                win.timerPick = box.entries[at].secs;
                box.activated(at);
                win.next("memberTimerApply");
                return;
            }
            // F4: a choice changes nothing until Apply; the click on Apply sends it.
            case "memberTimerApply": {
                if (App.person.timerSecs !== win.herTimerWas) return win.fail("her combo changed her chat's timer before Apply");
                const apply = win.shown("detailsTimerBoxApply");
                if (!apply || apply.width <= 0) return win.later("Apply under her combo");
                win.clickItem(apply);
                win.next("memberTimerSet");
                return;
            }
            case "memberTimerSet": {
                if (App.person.timerSecs !== win.timerPick) return win.later("her chat's timer");
                if (App.timeline.timerSecs !== win.groupTimerWas || App.group.timerSecs !== win.groupPageTimerWas) return win.fail("her combo changed the timer of the open group");
                if (App.composer.notice !== "") return win.fail("a notice without a failure: " + App.composer.notice);
                // F4: "Custom Time…" on her page (opened from a group that takes no custom time) is hers.
                if (win.chat.timerCustom) return win.fail("the fixture group takes custom times: the check would prove nothing");
                const box = win.shown("detailsTimerBox");
                box.activated(box.entries.length - 1);
                win.next("memberCustom");
                return;
            }
            case "memberCustom": {
                const d = win.chat.timerDialog;
                if (!d || !d.opened) {
                    if (win.waits > 50) return win.fail("Custom Time on a member's page opened no dialog");
                    return win.later("the custom time dialog");
                }
                if (d.conversation !== win.herChat) return win.fail("the dialog is not for her chat");
                if (d.max !== App.person.timerMax || d.min !== App.person.timerMin || d.max <= 0) return win.fail("the dialog's range is not her chat's: " + d.min + " to " + d.max);
                const amount = win.find(d.contentItem, c => c.objectName === "timerAmount");
                const unit = win.find(d.contentItem, c => c.objectName === "timerUnit");
                unit.currentIndex = 0;
                amount.value = 45;
                if (!d.valid || d.seconds !== 45) return win.fail("45 seconds is not valid in her range");
                d.accept();
                win.next("memberCustomSet");
                return;
            }
            case "memberCustomSet": {
                if (App.person.timerSecs !== 45) return win.later("her custom timer");
                if (App.timeline.timerSecs !== win.groupTimerWas || App.group.timerSecs !== win.groupPageTimerWas) return win.fail("her custom time changed the open group's timer");
                // The dialog is gone before the next real input (a modal popup takes it), and gone for good: Qt hands the keyboard
                // back to what had it before the dialog opened (since F4 the combo: Apply returns the keyboard to it) when its
                // closing transition ends, which would take it from her row after the Escape below has put it there.
                if (win.chat.timerDialog && win.chat.timerDialog.visible) return win.later("the dialog to close");
                // 9. Escape from her page: the members level, the keyboard on her row, the text kept.
                if (!win.chat.panel.activeFocus) win.chat.panel.forceActiveFocus();
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("herEsc");
                return;
            }
            case "herEsc": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("Escape to the members level");
                // The timer dialog of the step before is modal: its `opened` turns false before its overlay is gone, and an
                // overlay that is still there takes the click below without a word (failed three times, 2026-10-22 to 24).
                if (win.chat.timerDialog && win.chat.timerDialog.visible) return win.later("the timer dialog and its overlay to go");
                const search = win.shown("membersSearch");
                if (search.text.toLowerCase() !== win.typed) return win.fail("the search text was not kept");
                const list = win.shown("membersList");
                if (!list.activeFocus || list.currentIndex < 0 || list.model.userAt(list.currentIndex) !== win.herUser) return win.fail("the keyboard is not on her row (" + win.focusName() + ")");
                // A click into the field, then Escape: the text goes, the level stays.
                win.clickItem(search);
                win.next("herField");
                return;
            }
            case "herField": {
                const search = win.shown("membersSearch");
                if (!search.activeFocus) {
                    // No second click: a field that does not take the focus is a finding, not something to try
                    // again. After one second the place of the keyboard is printed once, then the step waits on.
                    if (win.waits === 25) console.warn("herField: the field has no focus after 1 s; the keyboard is on " + win.focusName());
                    return win.later("the field's focus");
                }
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("herFieldEsc");
                return;
            }
            case "herFieldEsc": {
                const search = win.shown("membersSearch");
                if (search.text !== "" || win.waits < 3) return win.later("Escape to empty the field");
                if (win.chat.panel.level !== "members") return win.fail("the first Escape in the field left the level");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("herEscGroup");
                return;
            }
            case "herEscGroup":
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Escape to the group page");
                if (win.focusName() !== "groupShowAll") return win.fail("the keyboard is not on Show All (" + win.focusName() + ")");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("herEscClosed");
                return;
            case "herEscClosed":
                if (win.chat.panelOpen) return win.later("Escape to close the panel");
                if (!win.chat.shownHeader().activeFocus) return win.fail("the keyboard is not on the header after the last Escape");
                if (win.viewersOpened !== win.viewersMark) return win.fail("a popup opened");
                // 10. From the header into the panel by key: Tab reaches the member rows, Return opens one.
                win.loadsBefore = win.groupLoads;
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("keysShown");
                return;
            case "keysShown": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page from the keyboard");
                win.inPanel("detailsClose").forceActiveFocus();
                const seen = [];
                let f = null;
                for (let i = 0; i < 16 && !f; i++) {
                    win.key(Qt.Key_Tab, Qt.NoModifier);
                    seen.push(win.focusName());
                    if (win.focusName() === "groupMember") f = win.activeFocusItem;
                }
                if (!f) return win.fail("Tab did not reach a member row: " + seen.join(", "));
                if (!f.visualFocus) return win.fail("the focused member row shows no focus");
                win.pickedUser = f.member.user;
                win.loadsBefore = win.personLoads;
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("keysOpened");
                return;
            }
            case "keysOpened": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the page of the member Return opened");
                if (App.person.user !== win.pickedUser) return win.fail("Return opened another member");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("xAll");
                return;
            }
            // 11. The X closes the whole panel at any level, and nothing is kept.
            case "xAll": {
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Escape to the group page");
                const flick = win.shown("groupScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                win.next("xClick");
                return;
            }
            case "xClick":
                if (win.waits < 3) return win.later("the page to scroll");
                win.clickItem(win.shown("groupShowAll"));
                win.next("xRow");
                return;
            case "xRow": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level");
                const row = win.findAll(win.chat.panel, c => c.objectName === "membersMember" && c.visible)[0];
                if (!row) return win.fail("the members level shows no row");
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("xPerson");
                return;
            }
            case "xPerson": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("a member's page over the members");
                if (panel.levels.length !== 3) return win.fail("the stack has " + panel.levels.length + " levels, not 3");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("xClosed");
                return;
            }
            case "xClosed":
                if (win.chat.panelOpen) return win.later("the X to close the whole panel");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("xReopened");
                return;
            case "xReopened": {
                const panel = win.chat.panel;
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.waits < 3) return win.later("the panel again");
                if (panel.level !== "group" || panel.levels.length !== 1) return win.fail("the panel reopens with " + panel.levels.length + " levels, on " + panel.level);
                const search = win.find(panel, c => c.objectName === "membersSearch");
                if (!search || search.text !== "") return win.fail("the search text is kept for the next open");
                // 12. Message, from her page opened by the group, opens her chat.
                win.after = "messageClick";
                win.next("viaAll");
                return;
            }
            case "messageClick": {
                const m = win.shown("detailsMessage");
                if (!m) return win.fail("her page, opened from the group, has no Message");
                win.clickItem(m);
                win.next("messageDone");
                return;
            }
            case "messageDone":
                if (App.timeline.conversationId !== PERSON) return win.later("her chat to open");
                if (win.chat.panelOpen) return win.fail("the panel stays open over her chat");
                win.next("bwOpen");
                return;
            // 12b. In the WIDE window (no toolbar arrow) a member's page has its own back arrow for the
            // mouse; Up on the members list's first row returns to the search field.
            case "bwOpen":
                win.width = 1200;
                win.height = 900;
                win.globalDrawer.collapsed = true;
                win.openConversation(THE_GROUP);
                win.waits = 0;
                win.next("bwHeader");
                return;
            case "bwHeader":
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 8) return win.later("the group's chat, wide");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("bwGroup");
                return;
            case "bwGroup": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page, wide");
                if (!win.chat.panelBeside) return win.fail("the panel should stand beside the timeline in the wide window");
                const flick = win.shown("groupScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                win.next("bwShowAll");
                return;
            }
            case "bwShowAll":
                if (win.waits < 3) return win.later("the page to scroll");
                win.clickItem(win.shown("groupShowAll"));
                win.next("bwMembers");
                return;
            case "bwMembers": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level, wide");
                const row = win.findAll(win.chat.panel, c => c.objectName === "membersMember" && c.visible)[0];
                if (!row) return win.fail("the members level shows no row");
                win.pickedUser = row.member.user;
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("bwPerson");
                return;
            }
            case "bwPerson": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("a member's page over the members, wide");
                const arrow = win.shown("personBack");
                if (!arrow) return win.fail("a member's page on top of the stack has no back arrow");
                if (arrow.Accessible.name !== win.shown("membersBack")?.Accessible.name && arrow.Accessible.name !== "Back") return win.fail("the arrow's accessible name: " + arrow.Accessible.name);
                win.clickItem(arrow);
                win.next("bwMembersBack");
                return;
            }
            case "bwMembersBack": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the arrow to go back to the members");
                const list = win.shown("membersList");
                if (!list.activeFocus || list.model.userAt(list.currentIndex) !== win.pickedUser) return win.fail("the keyboard is not on the opener row (" + win.focusName() + ")");
                // Up on the first row (it is the one that was opened) goes to the search field.
                if (list.currentIndex !== 0) return win.fail("the first row was the opener");
                win.key(Qt.Key_Up, Qt.NoModifier);
                win.next("bwUp");
                return;
            }
            case "bwUp":
                if (!win.shown("membersSearch").activeFocus) return win.later("Up to reach the search field");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("bwGroupBack");
                return;
            case "bwGroupBack": {
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Escape to the group page");
                const row = win.findAll(win.chat.panel, c => c.objectName === "groupMember" && c.visible && !c.member.isSelf)[0];
                if (!row) return win.fail("no member row to open");
                win.pickedUser = row.member.user;
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("bwPerson2");
                return;
            }
            case "bwPerson2": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("a member's page over the group, wide");
                const arrow = win.shown("personBack");
                if (!arrow) return win.fail("a member's page over the group has no back arrow");
                win.clickItem(arrow);
                win.next("bwGroupAgain");
                return;
            }
            case "bwGroupAgain": {
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("the arrow to go back to the group");
                const f = win.activeFocusItem;
                if (!f || f.objectName !== "groupMember" || f.member.user !== win.pickedUser) return win.fail("the keyboard is not on the opener row (" + win.focusName() + ")");
                if (win.shown("personBack")) return win.fail("the arrow stays on a hidden page");
                // 12c. Her page (7 groups in common) opened through Show All and the search.
                win.after = "saHer";
                win.markupDone = true;
                win.next("viaAll");
                return;
            }
            // One Back arrow at a time: in the page of all groups only the groups' own.
            case "saHer": {
                const panel = win.chat.panel;
                if (panel.levels.length < 2) return win.fail("her page is not stacked");
                if (App.person.groupsMore === "") return win.fail("the fixture person shares too few groups for Show All");
                if (!win.shown("personBack")) return win.fail("her stacked page shows no back arrow");
                win.clickItem(win.shown("detailsShowAll"));
                win.next("saAll");
                return;
            }
            case "saAll": {
                const panel = win.chat.panel;
                if (!panel.allGroups || win.waits < 3) return win.later("Show All of her groups");
                if (panel.level !== "person") return win.fail("Show All left the person level");
                const arrows = win.findAll(panel, c => (c.objectName === "personBack" || c.objectName === "detailsGroupsBack" || c.objectName === "membersBack") && c.visible);
                if (arrows.length !== 1 || arrows[0].objectName !== "detailsGroupsBack") return win.fail("the Back arrows shown in the groups list: " + arrows.map(a => a.objectName).join(", "));
                win.clickItem(arrows[0]);
                win.next("saBack");
                return;
            }
            case "saBack": {
                const panel = win.chat.panel;
                if (panel.allGroups || win.waits < 3) return win.later("Back from the groups list");
                if (panel.level !== "person" || !win.shown("personBack")) return win.fail("after the groups list her page has no stacked arrow, level " + panel.level);
                // 12d. Under mirroring the arrow stands at the panel's right (its leading side), "Show All" too.
                win.LayoutMirroring.enabled = true;
                win.LayoutMirroring.childrenInherit = true;
                win.waits = 0;
                win.next("saMirror");
                return;
            }
            case "saMirror": {
                if (win.waits < 8) return win.later("the mirrored layout");
                const panel = win.chat.panel;
                const arrow = win.shown("personBack");
                const at = arrow.mapToItem(panel, arrow.width / 2, 0).x;
                if (at < panel.width / 2) return win.fail("the mirrored back arrow is not on the panel's right half: " + at + " of " + panel.width);
                const x = win.inPanel("detailsClose").mapToItem(panel, 0, 0).x;
                if (x > panel.width / 2) return win.fail("the mirrored close button is not on the panel's left half");
                win.clickItem(arrow);
                win.next("saMembers");
                return;
            }
            case "saMembers":
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("saGroup");
                return;
            case "saGroup": {
                const panel = win.chat.panel;
                if (panel.level !== "group" || win.waits < 3) return win.later("Escape to the group page");
                const b = win.shown("groupShowAll");
                if (!b) return win.fail("the group page has no Show All");
                const right = b.mapToItem(panel, b.width, 0).x;
                if (right < panel.width - win.gu * 3) return win.fail("the mirrored Show All is not at the trailing edge: right edge " + right + " of " + panel.width);
                win.LayoutMirroring.enabled = false;
                win.clickItem(win.inPanel("detailsClose"));
                win.next("bwClosed");
                return;
            }
            case "bwClosed":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.next("mlOpen");
                return;
            // P2b: the members level is a Rust list model (`App.groupMembers`). Its opener is found again BY KEY after a
            // member's page was open (not row 0), and the next "Show All" starts at the top with no current row.
            case "mlOpen":
                win.height = 420;
                win.openConversation(THE_GROUP);
                win.next("mlHeader");
                return;
            case "mlHeader":
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 8) return win.later("the group's chat, short window");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("mlGroup");
                return;
            case "mlGroup": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page, short window");
                const flick = win.shown("groupScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                win.next("mlShowAll");
                return;
            }
            case "mlShowAll":
                if (win.waits < 3) return win.later("the page to scroll");
                win.clickItem(win.shown("groupShowAll"));
                win.next("mlMembers");
                return;
            case "mlMembers": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level, short window");
                const list = win.shown("membersList");
                if (list.count !== App.group.memberCount || list.currentIndex !== -1) return win.fail("the members level opens with " + list.count + " rows and current row " + list.currentIndex);
                win.clickItem(win.shown("membersSearch"));
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.next("mlDown1");
                return;
            }
            case "mlDown1": {
                const list = win.shown("membersList");
                if (!list.activeFocus || list.currentIndex !== 0) return win.later("Down from the search to the first row");
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.next("mlDown2");
                return;
            }
            case "mlDown2": {
                const list = win.shown("membersList");
                if (list.currentIndex !== 1) return win.later("Down to the second row");
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.next("mlReturn");
                return;
            }
            case "mlReturn": {
                const list = win.shown("membersList");
                if (list.currentIndex !== 2) return win.later("Down to the third row");
                win.pickedUser = list.model.userAt(2);
                win.loadsBefore = win.personLoads;
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("mlPerson");
                return;
            }
            case "mlPerson":
                if (win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || !win.chat.panel.loaded || win.waits < 3) return win.later("the page of the third member");
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mlBack");
                return;
            case "mlBack": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("Back to the members");
                const list = win.shown("membersList");
                if (!list.activeFocus) return win.fail("the keyboard is not in the members list (" + win.focusName() + ")");
                if (list.model.rowOf(win.pickedUser) !== 2 || list.currentIndex !== 2) return win.fail("the opener is not found by its key: current row " + list.currentIndex + ", the member is row " + list.model.rowOf(win.pickedUser));
                // Far down the list, then back to the group page and "Show All" again: a new view at the top.
                list.currentIndex = list.count - 1;
                list.positionViewAtEnd();
                if (list.contentY <= list.originY + 1) return win.fail("the members list does not scroll in the short window");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mlGroup2");
                return;
            }
            case "mlGroup2": {
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Escape to the group page");
                win.clickItem(win.shown("groupShowAll"));
                win.next("mlAgain");
                return;
            }
            case "mlAgain": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level again");
                const list = win.shown("membersList");
                if (list.count !== App.group.memberCount) return win.later("the rows of the members level again");
                if (list.currentIndex !== -1) return win.fail("the members level reopens with current row " + list.currentIndex);
                if (Math.abs(list.contentY - list.originY) > 1) return win.fail("the members level reopens scrolled: " + list.contentY);
                win.clickItem(win.shown("membersSearch"));
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.next("mlFirst");
                return;
            }
            case "mlFirst": {
                const list = win.shown("membersList");
                if (!list.activeFocus || list.currentIndex !== 0) return win.later("Down to give the first row");
                win.height = 900;
                win.clickItem(win.inPanel("detailsClose"));
                win.next("mlClosed");
                return;
            }
            case "mlClosed":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.next("snOpen");
                return;
            // ---- P3: a sender's name and picture, and a mention, open the person ----
            // The group's chat in the wide window, with the ten messages the fixture added (`group_mentions`; three of LD's come
            // first ("P3 bare", "P3 turned", "P3 below": `ldOpen`), then "P3 lead" (B), "P3 run 1" and "P3 run 2" (A, a run of two),
            // then four with mentions):
            // "P3 welcome" (written by B, mentions A and you), "P3 forged" (A; a mention of B and a forged link text),
            // "P3 hidden" (B; a mention of A under a spoiler) and "P3 linked" (A; a mention of B inside a link).
            case "snOpen":
                // (The pointer probe of the panel's steps lies over the whole timeline: it takes the clicks.)
                if (win.probe) {
                    win.probe.destroy();
                    win.probe = null;
                }
                win.width = 1200;
                win.height = 900;
                win.globalDrawer.collapsed = true;
                win.snCpu = App.threadCpuMs();
                win.openConversation(THE_GROUP);
                win.next("snRows");
                return;
            case "snRows": {
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 8) return win.later("the group's chat, for the senders");
                if (win.chat.panelOpen) return win.fail("the panel stays open over another chat");
                const ids = {
                    "welcome": win.rowWith("P3 welcome"),
                    "forged": win.rowWith("P3 forged"),
                    "hidden": win.rowWith("P3 hidden"),
                    "linked": win.rowWith("P3 linked")
                };
                for (const k in ids) {
                    if (ids[k] <= 0) return win.later("the added message " + k);
                    const row = win.view.delegateFor(ids[k]);
                    if (row === null || !win.shownInTimeline(row)) {
                        win.focusRow(ids[k]);
                        return win.later("the row " + k + " on screen (" + (row === null ? "no delegate" : JSON.stringify(row.mapToItem(win.view, 0, 0)) + " in a view of " + win.view.height) + ")");
                    }
                }
                win.snIds = ids;
                // What opening a chat of this many rows costs the Qt thread (the row's ONE sender handler is in it).
                console.warn("INFO: the group's chat opened in " + (App.threadCpuMs() - win.snCpu).toFixed(0) + " ms of the Qt thread, " + senders.count + " rows");
                // The roles say who wrote what: A wrote two, B two, and A is the person the other pages know.
                const a = win.rowInfoOf(ids.forged);
                const b = win.rowInfoOf(ids.welcome);
                if (a.authorId !== win.herUser || b.authorId === "" || b.authorId === a.authorId) return win.fail("the senders of the added messages: authorId says " + (a.authorId === win.herUser ? "A" : "not A") + " and " + (b.authorId === "" ? "nothing" : "B"));
                if (win.rowInfoOf(ids.hidden).authorId !== b.authorId || win.rowInfoOf(ids.linked).authorId !== a.authorId) return win.fail("the alternating senders of the added messages");
                // The markup: the mention outside a link is a link of the scheme, the forged text is none.
                const html = r => r.blocks[0].html;
                const anchors = s => (s.match(/<a /g) || []).length;
                if (html(win.rowInfoOf(ids.welcome)).indexOf('href="kourier:mention/0"') < 0 || html(win.rowInfoOf(ids.welcome)).indexOf('href="kourier:mention/1"') < 0) return win.fail("the mentions of the first message are not links");
                if (anchors(html(a)) !== 1 || html(a).indexOf('href="kourier:mention/0"') < 0) return win.fail("the forged link text is a link, or the real mention is none: " + anchors(html(a)));
                if (anchors(html(win.rowInfoOf(ids.linked))) !== 1 || html(win.rowInfoOf(ids.linked)).indexOf("kourier:mention") >= 0) return win.fail("a mention inside a link is not that link alone");
                // 1. A click on the sender's NAME beside the messages opens that person in the panel.
                const d = win.view.delegateFor(ids.welcome);
                const name = win.nameOf(d);
                if (!name || name.text !== d.author) return win.fail("the sender's name is not on the row");
                win.sender = { "user": b.authorId, "name": d.author };
                win.viewersMark = win.viewersOpened;
                win.loadsBefore = win.personLoads;
                win.clickAt(name, 4, name.height / 2);
                win.next("snNameOpened");
                return;
            }
            case "snNameOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || !panel || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page (open " + win.chat.panelOpen + ", loads " + (win.personLoads - win.loadsBefore) + ", level " + (win.chat.panel ? win.chat.panel.level : "-") + ", loaded " + (win.chat.panel ? win.chat.panel.loaded : "-") + ", overlay " + win.overlayInfo() + ")");
                if (App.person.user !== win.sender.user || App.person.name !== win.sender.name) return win.fail("the page is not the sender's");
                if (panel.levels.length !== 1) return win.fail("the page from the timeline is not the only level: " + panel.levels.join());
                if (win.viewersOpened !== win.viewersMark) return win.fail("a click on a name opened a viewer");
                if (win.shown("personBack")) return win.fail("the page from the timeline has a back arrow with nothing behind it");
                // Back (Escape in the panel) closes it: it was opened from the timeline.
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("snNameBack");
                return;
            }
            case "snNameBack": {
                if (win.chat.panelOpen) return win.later("Escape to close the page from the timeline");
                // Beside the name (the empty rest of its line, the bubble's text, the time) opens nothing.
                const d = win.view.delegateFor(win.snIds.welcome);
                const name = win.nameOf(d);
                if (name.implicitWidth > name.width - 8 * 3) return win.fail("the sender's name fills its line: there is no empty part to click");
                win.loadsBefore = win.personLoads;
                win.clickAt(name, name.width - 4, name.height / 2);
                const edit = win.editOf(d);
                win.clickAt(edit, 2, edit.height - 2);
                win.next("snBeside");
                return;
            }
            case "snBeside": {
                if (win.waits < 10) return win.later("the clicks settle");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("a click beside the sender's name opened a page");
                // 2. The PICTURE beside the messages opens the same person.
                const d = win.view.delegateFor(win.snIds.welcome);
                const pic = win.pictureOf(d);
                if (!pic) return win.fail("the sender's picture is not on the row");
                win.loadsBefore = win.personLoads;
                win.clickAt(pic, pic.width / 2, pic.height / 2);
                win.next("snPictureOpened");
                return;
            }
            case "snPictureOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page from the picture");
                if (App.person.user !== win.sender.user) return win.fail("the picture opened another person");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("snPictureClosed");
                return;
            }
            // 3. A's page from a name acts on HER chat: her combo changes her timer, never the open group's.
            case "snPictureClosed": {
                if (win.chat.panelOpen) return win.later("the X to close the page");
                const d = win.view.delegateFor(win.snIds.forged);
                const name = win.nameOf(d);
                win.sender = { "user": win.herUser, "name": d.author };
                win.loadsBefore = win.personLoads;
                win.clickAt(name, 4, name.height / 2);
                win.next("snHer");
                return;
            }
            case "snHer": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("her page from her name");
                if (App.person.user !== win.herUser) return win.fail("her name opened another person");
                const box = win.shown("detailsTimerBox");
                if (!box) return win.fail("her page, opened from her name, has no timer combo");
                win.groupTimerWas = App.timeline.timerSecs;
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== App.person.timerSecs);
                if (at < 0) return win.fail("her combo offers no other time");
                win.timerPick = box.entries[at].secs;
                box.activated(at);
                win.next("snHerApply");
                return;
            }
            case "snHerApply": {
                const apply = win.shown("detailsTimerBoxApply");
                if (!apply || apply.width <= 0) return win.later("Apply under her combo");
                win.clickItem(apply);
                win.next("snHerTimer");
                return;
            }
            case "snHerTimer": {
                if (App.person.timerSecs !== win.timerPick) return win.later("her chat's timer");
                if (App.timeline.timerSecs !== win.groupTimerWas) return win.fail("her combo, opened from a name, changed the open group's timer");
                if (App.composer.notice !== "") return win.fail("a notice without a failure: " + App.composer.notice);
                win.clickItem(win.inPanel("detailsClose"));
                win.snCase = 0;
                win.snTarget = 0;
                win.next("snPopup");
                return;
            }
            // 4. Real clicks at the point where a sender's name or picture lies under the message menu, the
            // reaction picker and the emoji picker: no page, no viewer.
            case "snPopup": {
                if (win.chat.panelOpen || win.view.popupOpen) return win.later("nothing is open");
                if (win.snCase >= 3) {
                    if (win.snLeaks.length > 0) return win.fail("a click on a popup reached the sender under it: " + win.snLeaks.join(", "));
                    win.next("snGroupOpen");
                    return;
                }
                const d = win.view.delegateFor(win.snIds.welcome);
                win.snOpenPopup(win.snCase, d);
                win.snSeen = "";
                win.snRest = 0;
                win.snMoved = false;
                win.next("snPopupOpened");
                return;
            }
            case "snPopupOpened": {
                const i = win.snCase;
                const popup = win.snPopupFor(i);
                const item = popup && popup.contentItem ? popup.contentItem.parent : null;
                if (!popup || !item || item.width <= 0 || !popup.opened) return win.later(win.snPopupNames[i] + " opens");
                const at = item.mapToItem(null, 0, 0);
                const seen = at.x + "," + at.y + "," + item.width + "," + item.height;
                if (seen !== win.snSeen) {
                    win.snSeen = seen;
                    win.snRest = 0;
                    return win.later(win.snPopupNames[i] + " comes to rest");
                }
                win.snRest += 1;
                if (win.snRest < 3) return win.later(win.snPopupNames[i] + " stays at rest");
                const d = win.view.delegateFor(win.snIds.welcome);
                const target = win.snTarget === 0 ? win.nameOf(d) : win.pictureOf(d);
                const rect = win.inWindow(target, 0, 0, win.snTarget === 0 ? Math.min(target.implicitWidth, target.width) : target.width, target.height);
                // The corner of the popup's own area, a few pixels in: its padding.
                const blank = Qt.point(at.x + 3, at.y + 3);
                if (!win.snMoved) {
                    // Put it where its corner lies on the middle of the name (or of the picture).
                    win.snMoved = true;
                    popup.x += rect.x + rect.w / 2 - blank.x;
                    popup.y += rect.y + rect.h / 2 - blank.y;
                    win.snSeen = "";
                    win.snRest = 0;
                    return win.later(win.snPopupNames[i] + " moved over the sender");
                }
                if (!win.inside(rect, blank.x, blank.y, 1)) return win.fail(win.snPopupNames[i] + " does not cover the sender's " + (win.snTarget === 0 ? "name" : "picture") + " at its corner: " + JSON.stringify(blank) + " in " + JSON.stringify(rect));
                win.viewersMark = win.viewersOpened;
                win.loadsBefore = win.personLoads;
                win.clickWindow(blank.x, blank.y);
                win.next("snPopupClicked");
                return;
            }
            case "snPopupClicked": {
                if (win.waits < 10) return win.later("the click settles");
                const i = win.snCase;
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore || win.viewersOpened !== win.viewersMark) {
                    win.snLeaks.push(win.snPopupNames[i] + " over the " + (win.snTarget === 0 ? "name" : "picture"));
                    if (win.chat.panelOpen) {
                        win.chat.closeDetails(false);
                    }
                }
                if (win.imageViewer !== null && win.imageViewer.visible) {
                    win.imageViewer.close();
                }
                win.snPopupFor(i).close();
                if (win.snTarget === 0) {
                    win.snTarget = 1;
                } else {
                    win.snTarget = 0;
                    win.snCase += 1;
                }
                win.next("snPopup");
                return;
            }
            // 5. The stack: a click on a sender while the panel is open puts that person's page on top of the levels it
            // has (a person already on top is replaced); Back returns to the level below as it was.
            case "snGroupOpen":
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("snGroupShown");
                return;
            case "snGroupShown": {
                const panel = win.chat.panel;
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || panel.level !== "group" || win.waits < 3) return win.later("the group page, for the senders");
                const d = win.view.delegateFor(win.snIds.hidden);
                const pic = win.pictureOf(d);
                win.sender = { "user": win.rowInfoOf(win.snIds.hidden).authorId, "name": d.author };
                win.loadsBefore = win.personLoads;
                win.clickAt(pic, pic.width / 2, pic.height / 2);
                win.next("snReplaced");
                return;
            }
            case "snReplaced": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page over the group page");
                if (App.person.user !== win.sender.user) return win.fail("another person than the sender");
                if (panel.levels.join() !== "group,person") return win.fail("the page from the timeline did not stack on the group page: " + panel.levels.join());
                // Back returns to the group page, which was kept (and kept up to date), the panel stays open.
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("snReplacedBack");
                return;
            }
            case "snReplacedBack": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "group" || win.waits < 3) return win.later("Back to the group page");
                if (panel.levels.join() !== "group" || App.group.id !== THE_GROUP) return win.fail("Back did not return to the group page: " + panel.levels.join());
                win.clickItem(win.inPanel("detailsClose"));
                win.next("snReplacedClosed");
                return;
            }
            case "snReplacedClosed":
                if (win.chat.panelOpen) return win.later("the X to close the group page");
                // The same over a member's page that lies on top of the group page.
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("snMemberOpen");
                return;
            case "snMemberOpen": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page, again");
                const row = win.findAll(win.chat.panel, c => c.objectName === "groupMember" && c.visible && !c.member.isSelf && c.member.user !== win.sender.user)[0];
                if (!row) return win.fail("no member row to open");
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("snMemberShown");
                return;
            }
            case "snMemberShown": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || panel.levels.length !== 2 || win.waits < 3) return win.later("a member's page over the group page");
                const d = win.view.delegateFor(win.snIds.welcome);
                const name = win.nameOf(d);
                win.sender = { "user": win.rowInfoOf(win.snIds.welcome).authorId, "name": d.author };
                win.loadsBefore = win.personLoads;
                win.clickAt(name, 4, name.height / 2);
                win.next("snMemberReplaced");
                return;
            }
            case "snMemberReplaced": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page over the member's");
                if (App.person.user !== win.sender.user) return win.fail("another person than the sender");
                // The member's page was replaced by the sender's: still two levels, the group page below.
                if (panel.levels.join() !== "group,person") return win.fail("the page from the timeline did not replace the member's page on the group page: " + panel.levels.join());
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("snMemberBack");
                return;
            }
            case "snMemberBack": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "group" || win.waits < 3) return win.later("Back to the group page from the sender's page");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("snMentions");
                return;
            }
            // 6. Mentions. The first of "Welcome" is A (her), the second is you.
            case "snMentions": {
                if (win.chat.panelOpen) return win.later("the X to close the page");
                const d = win.view.delegateFor(win.snIds.welcome);
                const at = win.mentionPoint(d, 0);
                if (!at) return win.fail("the first mention is not in the text");
                const edit = win.editOf(d);
                const p = edit.mapFromItem(null, at.x, at.y);
                win.ev.mouseMove(edit, p.x, p.y, -1, Qt.NoButton, Qt.NoModifier);
                win.next("snHover");
                return;
            }
            case "snHover": {
                const d = win.view.delegateFor(win.snIds.welcome);
                const edit = win.editOf(d);
                if (edit.hoveredLink !== "kourier:mention/0") return win.later("the pointer on a mention hovers its link (" + edit.hoveredLink + ")");
                const at = win.mentionPoint(d, 0);
                win.loadsBefore = win.personLoads;
                win.clickWindow(at.x, at.y);
                win.next("snMentionOpened");
                return;
            }
            case "snMentionOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the page of the mention");
                if (App.person.user !== win.herUser) return win.fail("the first mention did not open A");
                if (panel.levels.join() !== "person") return win.fail("the mention's page is not the only level");
                if (win.find(QQC2.Overlay.overlay, c => c.text === win.linkText("kourier:mention/0"))) return win.fail("a mention asked the link question");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("snYou");
                return;
            }
            // A mention of you opens your own page.
            case "snYou": {
                if (win.chat.panelOpen) return win.later("Back to close the page of the mention");
                const d = win.view.delegateFor(win.snIds.welcome);
                const at = win.mentionPoint(d, 1);
                if (!at) return win.fail("the second mention is not in the text");
                win.loadsBefore = win.personLoads;
                win.clickWindow(at.x, at.y);
                win.next("snYouOpened");
                return;
            }
            case "snYouOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("your own page from a mention");
                if (!App.person.isSelf) return win.fail("a mention of you did not open your own page");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("snHidden");
                return;
            }
            // A mention under a spoiler does not open while it is hidden; the click reveals it, the next opens.
            case "snHidden": {
                if (win.chat.panelOpen) return win.later("the X to close your page");
                const d = win.view.delegateFor(win.snIds.hidden);
                const block = win.textBlockOf(d);
                if (!block || block.revealed || block.spoilerRects.length === 0) return win.fail("the spoiler is not hiding the mention");
                if (win.rowInfoOf(win.snIds.hidden).blocks[0].mentions[0].spoiler !== true) return win.fail("the viewmodel does not say the mention lies under a spoiler");
                const at = win.mentionPoint(d, 0);
                win.loadsBefore = win.personLoads;
                // The text's own rule, asked directly (the spoiler's rectangle takes a real click first, so the
                // click proves the page but not this rule), and then a real click.
                block.openMention("kourier:mention/0");
                win.clickWindow(at.x, at.y);
                win.next("snHiddenClicked");
                return;
            }
            case "snHiddenClicked": {
                if (win.waits < 10) return win.later("the click settles");
                const d = win.view.delegateFor(win.snIds.hidden);
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("a mention under a spoiler opened a page while hidden");
                if (!win.textBlockOf(d).revealed) return win.fail("the click did not reveal the spoiler");
                const at = win.mentionPoint(d, 0);
                win.clickWindow(at.x, at.y);
                win.next("snHiddenOpened");
                return;
            }
            case "snHiddenOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the page of the revealed mention");
                if (App.person.user !== win.herUser) return win.fail("the revealed mention did not open A");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("snLinked");
                return;
            }
            // A mention inside a link stays that link: the question before it opens, no page.
            case "snLinked": {
                if (win.chat.panelOpen) return win.later("the X to close the page");
                const d = win.view.delegateFor(win.snIds.linked);
                const at = win.mentionPoint(d, 0);
                win.loadsBefore = win.personLoads;
                win.clickWindow(at.x, at.y);
                win.next("snLinkedAsked");
                return;
            }
            case "snLinkedAsked": {
                const label = win.find(QQC2.Overlay.overlay, c => c.text === win.linkText("https://example.invalid/mention-link"));
                if (!label) return win.later("the question before the link");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("a mention inside a link opened a page");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("snLinkedClosed");
                return;
            }
            // The forged text "kourier:mention/0" of a message is no link: a click on it does nothing (and a mention
            // asked of a text with no message does nothing either).
            case "snLinkedClosed": {
                if (win.find(QQC2.Overlay.overlay, c => c.text === win.linkText("https://example.invalid/mention-link") && c.visible)) return win.later("the question to close");
                const d = win.view.delegateFor(win.snIds.forged);
                const edit = win.editOf(d);
                const plain = edit.getText(0, edit.length);
                const at = plain.indexOf("kourier:mention/0");
                if (at < 0) return win.fail("the forged text is not shown as text");
                const p = win.pointInText(edit, at + 8);
                win.loadsBefore = win.personLoads;
                // A text outside the timeline (the list of earlier versions has no message) opens nothing.
                const block = win.textBlockOf(d);
                const message = block.message;
                block.message = 0;
                block.openMention("kourier:mention/0");
                block.message = message;
                win.clickWindow(p.x, p.y);
                win.next("snForged");
                return;
            }
            case "snForged": {
                if (win.waits < 10) return win.later("the click settles");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("a page opened after the forged link text was clicked or a mention was asked of a text with no message");
                if (win.find(QQC2.Overlay.overlay, c => c.text !== undefined && String(c.text).replace("\u2066", "").indexOf("kourier:") === 0)) return win.fail("a forged link text asked the link question");
                win.next("ldOpen");
                return;
            }
            // ---- LD: an address without a scheme is a link that asks with its whole https:// address, an e-mail address is
            // none, and a bidi override left open before a link does not turn the link around in the bubble ----
            case "ldOpen": {
                const id = win.rowWith("P3 bare");
                if (id <= 0) return win.later("the added message with the bare domain");
                const d = win.view.delegateFor(id);
                if (d === null || !win.shownInTimeline(d)) {
                    win.focusRow(id);
                    return win.later("the row with the bare domain on screen");
                }
                const edit = win.editOf(d);
                const plain = edit.getText(0, edit.length);
                const at = plain.indexOf("example.org/fpv");
                if (at < 0) return win.fail("the bare domain is not shown as he wrote it");
                const p = win.pointInText(edit, at + 3);
                win.clickWindow(p.x, p.y);
                win.next("ldAsked");
                return;
            }
            case "ldAsked": {
                if (!win.find(QQC2.Overlay.overlay, c => c.text === win.linkText("https://example.org/fpv") && c.visible)) return win.later("the question before the bare domain, with its https:// address");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("ldClosed");
                return;
            }
            case "ldClosed": {
                if (win.find(QQC2.Overlay.overlay, c => c.text === win.linkText("https://example.org/fpv") && c.visible)) return win.later("the question to close");
                const d = win.view.delegateFor(win.rowWith("P3 bare"));
                const edit = win.editOf(d);
                const plain = edit.getText(0, edit.length);
                const domain = plain.indexOf("example.net");
                if (plain.indexOf("pilot@example.net") < 0) return win.fail("the e-mail address is not shown as he wrote it");
                const p = win.pointInText(edit, domain + 3);
                win.clickWindow(p.x, p.y);
                win.next("ldNoMail");
                return;
            }
            case "ldNoMail": {
                if (win.waits < 10) return win.later("the click on the e-mail address settles");
                if (win.find(QQC2.Overlay.overlay, c => c.text !== undefined && String(c.text).indexOf("\u2066") === 0 && c.visible)) return win.fail("an e-mail address asked the link question");
                win.next("ldTurned");
                return;
            }
            // The override stands before the link, and nothing closes it: each character of the address stands to the right
            // of the one before it (an override would draw them from right to left).
            case "ldTurned":
            case "ldBelow": {
                const below = win.step === "ldBelow";
                const id = win.rowWith(below ? "P3 below" : "P3 turned");
                if (id <= 0) return win.later("the added message with the override");
                const d = win.view.delegateFor(id);
                if (d === null || !win.shownInTimeline(d)) {
                    win.focusRow(id);
                    return win.later("the row with the override on screen");
                }
                const edit = win.editOf(d);
                const plain = edit.getText(0, edit.length);
                const url = below ? "https://example.invalid/below" : "https://example.invalid/turned";
                const at = plain.indexOf(url);
                if (at < 0) return win.fail("the link behind the override is not shown");
                const first = edit.positionToRectangle(at);
                let last = first;
                for (let i = 1; i <= url.length; ++i) {
                    const r = edit.positionToRectangle(at + i);
                    if (r.y !== last.y || r.x <= last.x) return win.fail("the address is turned around in the bubble (" + (below ? "the override on the line above" : "the override on its line") + ") at " + i + ": " + last.x + "," + last.y + " then " + r.x + "," + r.y);
                    last = r;
                }
                if (last.x - first.x < 60) return win.fail("the address is hardly drawn: " + (last.x - first.x) + " px");
                if (below) {
                    win.focusRow(win.snIds.linked);
                    win.next("ldBack");
                } else {
                    win.next("ldBelow");
                }
                return;
            }
            // The rows of the next steps stand on screen again.
            case "ldBack": {
                for (const k in win.snIds) {
                    const row = win.view.delegateFor(win.snIds[k]);
                    if (row === null || !win.shownInTimeline(row)) {
                        win.focusRow(win.snIds[k]);
                        return win.later("the row " + k + " on screen again");
                    }
                }
                win.next("sxGroupsOpen");
                return;
            }
            // ---- P3 fix round (sx*): the open group in a sender's groups, the keyboard's way back, the stack with a search,
            // a row of a run, right to left, an index that is no mention, the cursor ----
            // 7. The sender's page, panel closed, "Show All" groups, the row of the OPEN group: the panel closes (nothing lies
            // below the page, the chat asked for is the one behind the panel).
            case "sxGroupsOpen": {
                if (win.chat.panelOpen) return win.later("the panel to be closed");
                const d = win.view.delegateFor(win.snIds.forged);
                const name = win.nameOf(d);
                win.loadsBefore = win.personLoads;
                win.clickAt(name, 4, name.height / 2);
                win.next("sxGroupsShown");
                return;
            }
            case "sxGroupsShown": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("her page from her name");
                if (panel.levels.join() !== "person") return win.fail("the panel opened from the timeline has more than one level: " + panel.levels.join());
                const all = win.shown("detailsShowAll");
                if (!all) return win.fail("her page has no Show All");
                win.clickItem(all);
                win.next("sxGroupsAll");
                return;
            }
            case "sxGroupsAll": {
                if (!win.chat.panel.allGroups || win.waits < 3) return win.later("her groups in full");
                const row = win.findAll(win.chat.panel, c => c.objectName === "detailsGroup" && c.visible && c.modelData.id === THE_GROUP)[0];
                if (!row) return win.fail("the open group is not among her groups in common");
                win.clickItem(row);
                win.next("sxGroupsDone");
                return;
            }
            case "sxGroupsDone":
                if (win.chat.panelOpen) return win.later("the open group's row to close the panel");
                if (App.timeline.conversationId !== THE_GROUP) return win.fail("the chat changed");
                win.next("sxFocus");
                return;
            // 8. A page opened from the timeline with the panel closed gives the keyboard back to the composer when it closes
            // (the header was not what opened it).
            case "sxFocus":
                win.composerItem.focusInput();
                win.next("sxFocusClick");
                return;
            case "sxFocusClick": {
                if (!win.find(win.composerItem, c => c === win.activeFocusItem)) return win.later("the composer to have the keyboard");
                const d = win.view.delegateFor(win.snIds.welcome);
                const name = win.nameOf(d);
                win.loadsBefore = win.personLoads;
                win.clickAt(name, 4, name.height / 2);
                win.next("sxFocusOpened");
                return;
            }
            case "sxFocusOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page, for the keyboard");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("sxFocusBack");
                return;
            }
            case "sxFocusBack":
                if (win.chat.panelOpen || win.waits < 3) return win.later("Escape to close the page");
                if (!win.find(win.composerItem, c => c === win.activeFocusItem)) return win.fail("the keyboard did not return to the composer: " + win.focusName());
                win.next("sxMembersOpen");
                return;
            // 9. The stack with the members level and its search: a sender's page on top, Back returns to the members level
            // with the search as it was.
            case "sxMembersOpen":
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("sxMembersGroup");
                return;
            case "sxMembersGroup": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page, for the search");
                const flick = win.shown("groupScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                win.next("sxMembersClick");
                return;
            }
            case "sxMembersClick": {
                if (win.waits < 3) return win.later("the page to scroll");
                const b = win.shown("groupShowAll");
                if (!b) return win.fail("the group page has no Show All");
                win.clickItem(b);
                win.next("sxMembersShown");
                return;
            }
            case "sxMembersShown": {
                if (win.chat.panel.level !== "members" || win.waits < 3) return win.later("the members level");
                win.allMembers = win.members();
                win.clickItem(win.shown("membersSearch"));
                win.typed = win.herName.slice(0, 3).toLowerCase();
                for (const ch of win.typed) {
                    win.key(ch.toUpperCase().charCodeAt(0), Qt.NoModifier);
                }
                win.next("sxMembersTyped");
                return;
            }
            case "sxMembersTyped": {
                const search = win.shown("membersSearch");
                if (search.text.toLowerCase() !== win.typed) return win.later("the typed letters in the field");
                const list = win.shown("membersList");
                win.snWant = win.allMembers.filter(m => App.memberMatches(m.name, search.text)).length;
                if (win.snWant === 0 || win.snWant >= win.allMembers.length) return win.fail("the search does not narrow the list: " + win.snWant + " of " + win.allMembers.length);
                if (list.count !== win.snWant || win.waits < 2) return win.later("the filtered rows");
                const d = win.view.delegateFor(win.snIds.welcome);
                const name = win.nameOf(d);
                win.sender = { "user": win.rowInfoOf(win.snIds.welcome).authorId, "name": d.author };
                win.loadsBefore = win.personLoads;
                win.clickAt(name, 4, name.height / 2);
                win.next("sxMembersPerson");
                return;
            }
            case "sxMembersPerson": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page over the members level");
                if (panel.levels.join() !== "group,members,person") return win.fail("the page from the timeline did not stack on the members level: " + panel.levels.join());
                if (App.person.user !== win.sender.user) return win.fail("another person than the sender");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("sxMembersBack");
                return;
            }
            case "sxMembersBack": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "members" || win.waits < 3) return win.later("Back to the members level");
                const search = win.shown("membersSearch");
                if (search.text.toLowerCase() !== win.typed) return win.fail("the search text was lost on the way");
                if (win.shown("membersList").count !== win.snWant) return win.fail("the filtered list was lost on the way: " + win.shown("membersList").count + " rows, not " + win.snWant);
                win.clickItem(win.inPanel("detailsClose"));
                win.next("sxGrouped");
                return;
            }
            // 10. A run of two rows by one author (the name on the first, the picture on the second): the row of a run
            // has a hidden part, and a click there opens nothing, a click on the shown part opens her.
            case "sxGrouped": {
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                const ids = { "run1": win.rowWith("P3 run 1"), "run2": win.rowWith("P3 run 2") };
                for (const k in ids) {
                    if (ids[k] <= 0) return win.later("the added message " + k);
                    const row = win.view.delegateFor(ids[k]);
                    if (row === null || !win.shownInTimeline(row)) {
                        win.focusRow(ids[k]);
                        return win.later("the row " + k + " on screen");
                    }
                }
                win.snIds = Object.assign({}, win.snIds, ids);
                const r1 = win.rowInfoOf(ids.run1);
                const r2 = win.rowInfoOf(ids.run2);
                if (r1.authorId !== win.herUser || r2.authorId !== win.herUser) return win.fail("the run is not written by A");
                if (!r1.showAuthor || r1.showAvatar || r2.showAuthor || !r2.showAvatar) return win.fail("the run does not show the name on the first and the picture on the second: " + r1.showAuthor + r1.showAvatar + r2.showAuthor + r2.showAvatar);
                win.loadsBefore = win.personLoads;
                // The first row's hidden picture column (its picture item is there, not shown) and the second row's hidden
                // name (its label is there, not shown): the rule says no, and so does a real click on each.
                const d1 = win.view.delegateFor(ids.run1);
                const hiddenPicture = win.find(d1, c => c.objectName === "senderPicture");
                if (d1.hitsSender(hiddenPicture.mapToItem(d1, hiddenPicture.width / 2, hiddenPicture.height / 2))) return win.fail("the rule says a hidden picture is a sender");
                const d2 = win.view.delegateFor(ids.run2);
                const hiddenName = win.find(d2, c => c.objectName === "senderName");
                if (d2.hitsSender(hiddenName.mapToItem(d2, 4, hiddenName.height / 2))) return win.fail("the rule says a hidden name is a sender");
                win.clickAt(hiddenPicture, hiddenPicture.width / 2, hiddenPicture.height / 2);
                win.clickAt(hiddenName, 4, hiddenName.height / 2);
                win.next("sxGroupedClicked");
                return;
            }
            case "sxGroupedClicked": {
                if (win.waits < 10) return win.later("the clicks settle");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("a click where no name or picture is shown opened a page");
                const d2 = win.view.delegateFor(win.snIds.run2);
                const pic = win.pictureOf(d2);
                win.loadsBefore = win.personLoads;
                win.clickAt(pic, pic.width / 2, pic.height / 2);
                win.next("sxGroupedOpened");
                return;
            }
            case "sxGroupedOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("A's page from the picture of her second row");
                if (App.person.user !== win.herUser) return win.fail("the picture of the second row opened another person");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("sxRtl");
                return;
            }
            // 11. Right to left: the picture is at the row's trailing side and opens; the name is where its label DRAWS it (the style
            // decides the alignment, not the layout), the empty rest of its line and the leading column open nothing.
            case "sxRtl":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.LayoutMirroring.enabled = true;
                win.LayoutMirroring.childrenInherit = true;
                win.waits = 0;
                win.next("sxRtlSet");
                return;
            case "sxRtlSet": {
                if (win.waits < 8) return win.later("the mirrored layout");
                const d = win.view.delegateFor(win.snIds.welcome);
                if (d === null || !win.shownInTimeline(d)) {
                    win.focusRow(win.snIds.welcome);
                    return win.later("the row on screen, mirrored");
                }
                const name = win.nameOf(d);
                const pic = win.pictureOf(d);
                if (pic.mapToItem(d, pic.width / 2, 0).x < d.width / 2) return win.fail("the picture is not at the trailing side of the mirrored row");
                if (name.contentWidth > name.width - 16) return win.fail("the name fills its line: no empty side");
                // Where the text is drawn: the label's alignment as it is, not the layout's direction.
                win.snRight = name.effectiveHorizontalAlignment === Text.AlignRight;
                win.loadsBefore = win.personLoads;
                // The empty end of the line first (nothing), and the leading column, where a left-to-right row has its picture.
                win.clickAt(name, win.snRight ? 4 : name.width - 4, name.height / 2);
                win.clickAt(d, d.sideMargin + 4, pic.mapToItem(d, 0, pic.height / 2).y);
                win.next("sxRtlBeside");
                return;
            }
            case "sxRtlBeside": {
                if (win.waits < 10) return win.later("the clicks settle");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("a click beside the name, or on the leading column, of a mirrored row opened a page");
                const d = win.view.delegateFor(win.snIds.welcome);
                const name = win.nameOf(d);
                win.loadsBefore = win.personLoads;
                win.clickAt(name, win.snRight ? name.width - 4 : 4, name.height / 2);
                win.next("sxRtlName");
                return;
            }
            case "sxRtlName": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page from the name of a mirrored row");
                if (App.person.user !== win.rowInfoOf(win.snIds.welcome).authorId) return win.fail("another person than the sender");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("sxRtlPicture");
                return;
            }
            case "sxRtlPicture": {
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                const d = win.view.delegateFor(win.snIds.welcome);
                const pic = win.pictureOf(d);
                win.loadsBefore = win.personLoads;
                win.clickAt(pic, pic.width / 2, pic.height / 2);
                win.next("sxRtlPictureOpened");
                return;
            }
            case "sxRtlPictureOpened": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page from the picture of a mirrored row");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("sxRtlDone");
                return;
            }
            case "sxRtlDone":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.LayoutMirroring.enabled = false;
                win.next("sxIndex");
                return;
            // 12. A link of the mention scheme whose index is no mention opens nothing (and throws nothing: the script's errors
            // fail the test).
            case "sxIndex": {
                const block = win.textBlockOf(win.view.delegateFor(win.snIds.linked));
                win.loadsBefore = win.personLoads;
                for (const link of ["kourier:mention/7", "kourier:mention/-1", "kourier:mention/99999999999999999999", "kourier:mention/", "kourier:mention/x"]) {
                    block.openMention(link);
                }
                win.next("sxIndexDone");
                return;
            }
            case "sxIndexDone":
                if (win.waits < 10) return win.later("the asks settle");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("an index that is no mention opened a page");
                win.next("sxCursor");
                return;
            // 13. The pointer cursor: a hand over a mention in the timeline; over the same mention in a text with no message (the list
            // of earlier versions) none.
            case "sxCursor": {
                const d = win.view.delegateFor(win.snIds.welcome);
                const at = win.mentionPoint(d, 0);
                const edit = win.editOf(d);
                const p = edit.mapFromItem(null, at.x, at.y);
                win.ev.mouseMove(edit, p.x, p.y, -1, Qt.NoButton, Qt.NoModifier);
                win.next("sxCursorHover");
                return;
            }
            case "sxCursorHover": {
                const d = win.view.delegateFor(win.snIds.welcome);
                const edit = win.editOf(d);
                if (edit.hoveredLink !== "kourier:mention/0") return win.later("the pointer on a mention hovers its link");
                let hover = null;
                for (let i = 0; i < edit.data.length; ++i) {
                    if (edit.data[i].cursorShape !== undefined) {
                        hover = edit.data[i];
                    }
                }
                if (!hover) return win.fail("the text has no hover handler");
                if (hover.cursorShape !== Qt.PointingHandCursor) return win.fail("no hand over a mention in the timeline: " + hover.cursorShape);
                const block = win.textBlockOf(d);
                const message = block.message;
                block.message = 0;
                const shape = hover.cursorShape;
                block.message = message;
                if (shape !== Qt.IBeamCursor) return win.fail("a hand over a mention that opens nothing: " + shape);
                win.next("smOpen");
                return;
            }
            // ---- P3 round 2 (sm*): the message menu's pages, from the keyboard (the core states the entries) ----
            // 14. The row with the keyboard, the Menu key: "Show Sender's Page", one entry per person the text mentions
            // ("Show Your Page" for the user himself); the entry chosen with the keys opens that page.
            case "smOpen": {
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("smMenu");
                return;
            }
            case "smMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the Menu key opens the row's menu");
                if (m.row.messageId !== win.snIds.welcome) return win.fail("the menu is about another row");
                const pages = win.menuPages(m);
                const labels = pages.map(p => p.text);
                const her = "Show " + win.herName + "'s Page";
                if (labels.join("|") !== "Show Sender's Page|" + her + "|Show Your Page") return win.fail("the entries: " + labels.length + " of them, the texts differ from the sender's, the mentioned person's and the user's");
                for (const p of pages) {
                    if (p.Accessible.name !== p.text) return win.fail("an entry's accessible name is not its text");
                }
                // The entries sit together after their own separator, and the menu is as tall as its entries.
                const at = [];
                for (let i = 0; i < m.count; ++i) {
                    if (pages.indexOf(m.itemAt(i)) >= 0) {
                        at.push(i);
                    }
                }
                if (at.length !== 3 || at[2] - at[0] !== 2) return win.fail("the page entries are not together: " + at.join());
                const before = m.itemAt(at[0] - 1);
                if (!before || String(before).indexOf("MenuSeparator") < 0 || !before.visible) return win.fail("the page entries have no separator before them");
                if (m.height < pages.length * pages[0].height) return win.fail("the menu is shorter than its page entries");
                win.snPick = 0;
                win.snExpect = win.rowInfoOf(win.snIds.welcome).authorId;
                win.snSelf = false;
                win.loadsBefore = win.personLoads;
                win.next("smPick");
                return;
            }
            case "smPick": {
                const m = win.view.messageMenu;
                if (!m.visible) return win.later("the menu");
                if (!win.menuPick(m, win.menuPages(m)[win.snPick])) return win.later("the entry to be highlighted");
                win.next("smPicked");
                return;
            }
            case "smPicked": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the page of the chosen entry");
                if (App.person.isSelf !== win.snSelf || (!win.snSelf && App.person.user !== win.snExpect)) return win.fail("the entry opened another page: self " + App.person.isSelf);
                if (!panel.activeFocus) return win.fail("the keyboard is not on the page: " + win.focusName());
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("smPickedBack");
                return;
            }
            // The page the menu opened closes: the keyboard is back on the row the menu was opened on, whose current row was
            // kept (checked BEFORE a step puts it there again with `rowWithKeyboard`).
            case "smPickedBack": {
                if (win.chat.panelOpen || win.waits < 3) return win.later("Escape to close the page");
                const list = win.view.rowList;
                if (!list.activeFocus || list.currentItem !== win.view.delegateFor(win.snIds.welcome)) return win.fail("the keyboard did not return to the row of the menu: " + win.focusName() + ", current row " + list.currentIndex);
                win.next(win.snPick === 0 ? "smMention" : win.snPick === 1 ? "smYou" : "smRun");
                return;
            }
            case "smMention": {
                if (win.chat.panelOpen) return win.later("Escape to close the page");
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.snPick = 1;
                win.snExpect = win.herUser;
                win.snSelf = false;
                win.loadsBefore = win.personLoads;
                win.next("smPick");
                return;
            }
            case "smYou": {
                if (win.chat.panelOpen) return win.later("Escape to close the page");
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.snPick = 2;
                win.snExpect = "";
                win.snSelf = true;
                win.loadsBefore = win.personLoads;
                win.next("smPick");
                return;
            }
            // A row of the same sender without a mention has the sender's entry alone; a mention under a spoiler has none.
            case "smRun": {
                if (win.chat.panelOpen) return win.later("Escape to close the page");
                win.snRow = win.snIds.run1;
                win.snWantPages = ["Show Sender's Page"];
                win.next("smOnly");
                return;
            }
            case "smOnly": {
                const d = win.rowWithKeyboard(win.snRow);
                if (d === null) return win.later("the keyboard on the row");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("smOnlyMenu");
                return;
            }
            case "smOnlyMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the Menu key opens the row's menu");
                const labels = win.menuPages(m).map(p => p.text);
                if (labels.join("|") !== win.snWantPages.join("|")) return win.fail("the entries of this row: " + labels.length + " of them, wanted " + win.snWantPages.length);
                m.close();
                win.next(win.snRow === win.snIds.run1 ? "smHidden" : win.snRow === win.snIds.hidden ? "smOwn" : win.snDirect ? "mdOpen" : "smOwnDone");
                return;
            }
            case "smHidden":
                if (win.view.messageMenu.visible) return win.later("the menu to close");
                win.snRow = win.snIds.hidden;
                win.snWantPages = ["Show Sender's Page"];
                win.next("smOnly");
                return;
            // Our own message: no sender entry.
            case "smOwn": {
                if (win.view.messageMenu.visible) return win.later("the menu to close");
                let own = 0;
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "message" && r.own) {
                        own = r.messageId;
                        break;
                    }
                }
                if (own <= 0) return win.fail("the group's chat has no own message");
                // (LD's three rows stand between the newest rows and the older ones: it may lie above the view.)
                if (win.view.delegateFor(own) === null) {
                    win.focusRow(own);
                    return win.later("the own message on screen");
                }
                win.snRow = own;
                win.snWantPages = [];
                win.next("smOnly");
                return;
            }
            case "smOwnDone":
                if (win.view.messageMenu.visible) return win.later("the menu to close");
                win.next("smLong");
                return;
            // A very long name is elided by the menu (whole in the accessible name), as plain text, and the menu stays as wide as a menu.
            case "smLong": {
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                const info = d.rowInfo();
                info.pages = [{ "user": win.herUser, "label": "Show <b>x</b>" + " N".repeat(200) + "'s Page" }];
                TimelineActions.menu(win.nameOf(d), info, 0, 0);
                win.next("smLongMenu");
                return;
            }
            case "smLongMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the menu with a long name");
                const pages = win.menuPages(m);
                if (pages.length !== 1) return win.fail("the long entry is not there");
                const e = pages[0];
                if (e.text.indexOf("<") >= 0 || e.text.indexOf("‹b›") < 0) return win.fail("the name is not plain text in the entry");
                if (e.text.charAt(e.text.length - 1) !== "…" || e.text.length > 200) return win.fail("the entry is not elided: " + e.text.length + " characters");
                if (e.Accessible.name.length < 400) return win.fail("the accessible name is cut");
                if (m.width > win.gu * 40) return win.fail("the menu grew with the name: " + m.width);
                m.close();
                win.next("smAmp");
                return;
            }
            // An "&" in a name is literal in the entry: the menu reads "&x" as a mnemonic (the "&" vanishes, the letter triggers).
            case "smAmp": {
                if (win.view.messageMenu.visible) return win.later("the menu to close");
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                const info = d.rowInfo();
                info.pages = [{ "user": win.herUser, "label": "Show Bo&b's Page" }];
                TimelineActions.menu(win.nameOf(d), info, 0, 0);
                win.next("smAmpMenu");
                return;
            }
            case "smAmpMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the menu with an ampersand");
                const pages = win.menuPages(m);
                if (pages.length !== 1) return win.fail("the entry is not there");
                if (pages[0].text !== "Show Bo&&b's Page") return win.fail("the entry does not double the ampersand: " + pages[0].text);
                if (pages[0].Accessible.name !== "Show Bo&b's Page") return win.fail("the accessible name is not the name as it is: " + pages[0].Accessible.name);
                win.loadsBefore = win.personLoads;
                win.key(Qt.Key_B, Qt.NoModifier);
                win.next("smAmpKey");
                return;
            }
            case "smAmpKey":
                if (win.waits < 6) return win.later("the key settles");
                if (win.chat.panelOpen || win.personLoads !== win.loadsBefore) return win.fail("the letter after an ampersand in a name triggered the entry");
                win.view.messageMenu.close();
                win.snShape = 0;
                win.next("smShape");
                return;
            // Separators stand only between groups that are not empty: the menu of someone else's deleted message in a group is
            // "Show Sender's Page", a separator, "Delete…"; a notice has "Delete…" alone; pages alone have no separator; whatever
            // the row, the menu does not start or end with a separator and never has two.
            case "smShape": {
                if (win.view.messageMenu.visible) return win.later("the menu to close");
                if (win.snShape >= 4) {
                    win.next("smStack");
                    return;
                }
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                const info = d.rowInfo();
                const sender = { "user": win.herUser, "label": "Show Sender's Page" };
                const ari = { "user": win.herUser, "label": "Show Ari's Page" };
                const bare = { "messageId": info.messageId, "own": false, "author": info.author, "pages": [], "reactions": [], "failed": false, "edited": false, "mediaAction": "", "mediaLabel": "", "quoted": false, "attachments": [], "copyText": "" };
                let want = null;
                let shown = info;
                if (win.snShape === 0) {
                    shown = Object.assign({}, bare, { "system": false, "pages": [sender], "actions": { "any": true, "deleteForMe": true } });
                    want = "Show Sender's Page|-|Delete…";
                } else if (win.snShape === 1) {
                    shown = Object.assign({}, bare, { "system": true, "actions": { "any": true, "deleteForMe": true } });
                    want = "Delete…";
                } else if (win.snShape === 2) {
                    shown = Object.assign({}, bare, { "system": false, "pages": [sender, ari], "actions": { "any": true } });
                    want = "Show Sender's Page|Show Ari's Page";
                }
                TimelineActions.menu(win.nameOf(d), shown, 0, 0);
                win.snShapeWant = want === null ? "" : want;
                win.snWantShape = want !== null;
                win.next("smShapeMenu");
                return;
            }
            case "smShapeMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the menu to read");
                const seq = [];
                for (let i = 0; i < m.count; ++i) {
                    const it = m.itemAt(i);
                    if (!it || !it.visible) continue;
                    seq.push(String(it).indexOf("MenuSeparator") >= 0 ? "-" : (it.text === undefined ? "quick" : it.text));
                }
                const shape = seq.join("|");
                if (seq.length === 0 || seq[0] === "-" || seq[seq.length - 1] === "-" || shape.indexOf("-|-") >= 0) return win.fail("a separator at an end or two in a row: " + shape);
                if (win.snWantShape && shape !== win.snShapeWant) return win.fail("the menu reads " + shape + ", not " + win.snShapeWant);
                if (!win.snWantShape && seq[0] !== "quick") return win.fail("the menu of a row that can be reacted to does not start with the reactions: " + shape);
                m.close();
                win.snShape += 1;
                win.next("smShape");
                return;
            }
            // The entry opens the page by the click's own path: with the group page open it stacks on it, and the keyboard is on the page.
            case "smStack": {
                if (win.view.messageMenu.visible) return win.later("the menu to close");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("smStackGroup");
                return;
            }
            case "smStackGroup": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page, for the menu");
                const d = win.rowWithKeyboard(win.snIds.welcome);
                if (d === null) return win.later("the keyboard on the welcome row");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.snPick = 0;
                win.snExpect = win.rowInfoOf(win.snIds.welcome).authorId;
                win.snSelf = false;
                win.loadsBefore = win.personLoads;
                win.next("smStackPick");
                return;
            }
            case "smStackPick": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the menu");
                if (!win.menuPick(m, win.menuPages(m)[0])) return win.later("the entry to be highlighted");
                win.next("smStacked");
                return;
            }
            case "smStacked": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page over the group page, from the menu");
                if (panel.levels.join() !== "group,person") return win.fail("the menu's page did not stack on the group page: " + panel.levels.join());
                if (App.person.user !== win.snExpect) return win.fail("another person than the sender");
                if (!panel.activeFocus) return win.fail("the keyboard is not on the page: " + win.focusName());
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("smStackBack");
                return;
            }
            case "smStackBack": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "group" || win.waits < 3) return win.later("Back to the group page");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("smStackClosed");
                return;
            }
            // The panel the HEADER opened, with a sender stacked on it, closed by the X: the keyboard is on the header, as for any
            // panel the header opened (only a panel opened from the timeline gives it to the composer or the row).
            case "smStackClosed":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X to close the panel");
                if (!win.chat.shownHeader().activeFocus) return win.fail("the keyboard is not on the header after the panel it opened closes: " + win.focusName());
                win.next("smDirect");
                return;
            // A 1:1 chat has no sender entry.
            case "smDirect":
                if (win.chat.panelOpen) return win.later("the X to close the panel");
                win.openConversation(PERSON);
                win.next("smDirectRows");
                return;
            case "smDirectRows": {
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.waits < 6) return win.later("her chat, for the menu");
                let theirs = 0;
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "message" && !r.own) {
                        theirs = r.messageId;
                        break;
                    }
                }
                if (theirs <= 0) return win.fail("her chat has no message of hers");
                win.snRow = theirs;
                win.snWantPages = [];
                win.snDirect = true;
                win.next("smOnly");
                return;
            }
            // ---- P4: "Media and Files" of a chat (the section of a page and the level of all media) ----
            // The albums chat (a 1:1 chat with many pictures): the person's page ends with six squares and
            // "Show All"; a click on a square opens ONE viewer, and a click on the viewer's picture over the
            // square opens no second one; "Show All" opens the level, whose grid starts with the same picture,
            // whose Files tab says there is none; Back returns to the page.
            case "mdOpen":
                win.width = 1200;
                win.height = 900;
                win.globalDrawer.collapsed = true;
                win.openConversation(ALBUMS);
                win.next("mdHeader");
                return;
            case "mdHeader": {
                if (App.timeline.conversationId !== ALBUMS || App.timeline.loading || win.waits < 8) return win.later("the albums chat, for its media");
                if (win.chat.panelOpen) return win.fail("the panel stays open over another chat");
                // How many pictures the chat holds (a sticker is no picture of the gallery).
                win.mdExpected = 0;
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.kind === "message") win.mdExpected += win.picturesOf(r).filter(p => !p.sticker).length;
                }
                if (win.mdExpected < 8) return win.fail("the albums chat has " + win.mdExpected + " pictures");
                win.loadsBefore = win.personLoads;
                win.clickHeader();
                win.next("mdPerson");
                return;
            }
            case "mdPerson": {
                if (!win.panelReady() || win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || win.waits < 3) return win.later("the person's page of the albums chat");
                const part = win.shown("personMedia");
                if (!part) return win.later("the media part of the page");
                if (!App.personMedia.loaded || App.personMedia.count !== 6) return win.later("six squares: " + App.personMedia.count);
                if (!App.personMedia.more) return win.fail("the section does not say that more pictures follow");
                const squares = win.findAll(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible);
                if (squares.length !== 6) return win.fail("the section shows " + squares.length + " squares");
                if (squares.some(q => q.mediaState !== "local" || q.url.indexOf("image://kourier/thumb/") !== 0)) return win.later("the squares' pictures are downloaded");
                // Squares: three in a row, as wide as high, inside the panel.
                const panel = win.chat.panel;
                const at = q => q.mapToItem(panel, 0, 0);
                if (Math.abs(at(squares[0]).y - at(squares[2]).y) > 1 || at(squares[3]).y <= at(squares[0]).y + 1) return win.fail("the squares are not three in a row");
                for (const q of squares) {
                    if (Math.abs(q.width - q.height) > 1 || at(q).x < 0 || at(q).x + q.width > panel.width + 1) return win.fail("a square is not square or leaves the panel");
                    // (A square is no Tab stop: one more stop on the person page of the fixture, who has a photo, moves
                    // where P1's 24 Tab presses end, and the Space typed after them would close the panel.)
                    if (q.focusPolicy !== Qt.NoFocus) return win.fail("a square is a stop of the keyboard");
                }
                // The part is the LAST of the page (Signal Desktop's order).
                for (const name of ["detailsMessage", "detailsTimer", "detailsGroups"]) {
                    const other = win.shown(name);
                    if (other && at(other).y >= at(part).y) return win.fail("the media part is not after " + name);
                }
                const all = win.shown("mediaAll");
                if (!all) return win.fail("the section has no All Media and Files");
                if ((all.focusPolicy & Qt.TabFocus) === 0 || all.Accessible.name !== "All Media and Files") return win.fail("All Media and Files is no stop of the keyboard with its name: " + all.focusPolicy + ", " + all.Accessible.name);
                // A real click on a square opens the viewer, once.
                win.mdFirst = squares[0].attachment;
                win.viewersBefore = win.viewersOpened;
                win.clickItem(squares[0]);
                win.next("mdViewer");
                return;
            }
            case "mdViewer": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("a click on a square opens the viewer");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows the square's picture");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("one click opened " + (win.viewersOpened - win.viewersBefore) + " viewers");
                if (v.attachment !== win.mdFirst) return win.fail("the viewer shows another picture than the square");
                // The square lies under the viewer's picture: a click there keeps the viewer open and opens nothing more.
                const square = win.findAll(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible)[0];
                const at = square.mapToItem(null, square.width / 2, square.height / 2);
                if (!win.inside(win.viewerPicture(v), at.x, at.y, 0)) return win.fail("the square does not lie under the viewer's picture: " + JSON.stringify(win.viewerPicture(v)));
                win.clickWindow(at.x, at.y);
                win.next("mdStays");
                return;
            }
            case "mdStays":
                if (win.waits < 10) return win.later("the click settles");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("a click on the viewer's picture opened " + (win.viewersOpened - win.viewersBefore - 1) + " more viewer(s) (the square under it took the click)");
                if (!win.imageViewer || !win.imageViewer.opened) return win.fail("a click on the viewer's picture closed it");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mdViewerGone");
                return;
            case "mdViewerGone":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                if (!win.chat.panelOpen) return win.fail("the Escape that closed the viewer closed the panel too");
                win.clickItem(win.shown("mediaAll"));
                win.next("mdLevel");
                return;
            case "mdLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("the level of all media");
                if (panel.levels.join() !== "person,media") return win.fail("the levels: " + panel.levels.join());
                if (App.mediaGrid.count !== win.mdExpected) return win.later("every picture in the grid: " + App.mediaGrid.count + " of " + win.mdExpected);
                if (App.mediaGrid.more) return win.fail("more pictures behind every picture");
                const grid = win.shown("mediaGrid");
                if (!grid) return win.fail("the media tab shows no grid");
                const first = grid.itemAtIndex(0);
                if (!first || first.attachment !== win.mdFirst) return win.later("the grid starts with the picture the section starts with");
                if (win.shown("mediaTitle").text !== "All Media and Files") return win.fail("the title: " + win.shown("mediaTitle").text);
                if (win.shown("mediaNone")) return win.fail("a placeholder over a grid of pictures");
                if (!win.shown("mediaBack")) return win.fail("the level has no back arrow");
                // The Files tab: this chat has no file.
                win.clickItem(win.shown("mediaTabFiles"));
                win.next("mdFilesNone");
                return;
            }
            case "mdFilesNone": {
                if (!win.shown("mediaFilesNone") || win.waits < 3) return win.later("the Files tab says the chat has no file");
                if (win.shown("mediaGrid")) return win.fail("the grid still shows under the Files tab");
                win.clickItem(win.shown("mediaTabMedia"));
                win.next("mdCell");
                return;
            }
            case "mdCell": {
                const grid = win.shown("mediaGrid");
                if (!grid || win.waits < 3) return win.later("the Media tab again");
                const cell = grid.itemAtIndex(2);
                if (!cell || cell.mediaState !== "local") return win.later("a cell of the grid, downloaded");
                win.viewersBefore = win.viewersOpened;
                win.mdFirst = cell.attachment;
                win.clickItem(cell);
                win.next("mdCellViewer");
                return;
            }
            case "mdCellViewer": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("a click on a cell opens the viewer");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows the cell's picture");
                if (win.viewersOpened !== win.viewersBefore + 1 || v.attachment !== win.mdFirst) return win.fail("a cell opened " + (win.viewersOpened - win.viewersBefore) + " viewers, showing the picture it names: " + (v.attachment === win.mdFirst));
                if (v.gallery !== "media") return win.fail("a cell opened the viewer through '" + v.gallery + "', not through the level's own rows");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mdBack");
                return;
            }
            case "mdBack":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                if (win.chat.panel.level !== "media") return win.fail("the Escape that closed the viewer went a level back");
                // Back goes one level back; the level's two lists are closed and empty.
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mdBackDone");
                return;
            case "mdBackDone": {
                if (win.chat.panel.level !== "person" || win.waits < 3) return win.later("Back to the person's page");
                if (win.chat.panel.levels.join() !== "person") return win.fail("the levels: " + win.chat.panel.levels.join());
                if (App.mediaGrid.count !== 0 || App.mediaFiles.count !== 0 || App.mediaGrid.loaded || App.mediaFiles.loaded) return win.fail("the level's lists stay filled after Back");
                if (App.personMedia.count !== 6 || !win.shown("mediaAll")) return win.fail("the page's section is not as it was");
                // Back puts the keyboard on the button that opened the level; and Tab from the close button (the first stop of
                // the panel) reaches the section's one stop, where Return opens the level.
                if (win.focusName() !== "mediaAll") return win.fail("Back left the keyboard on '" + win.focusName() + "', not on All Media and Files");
                win.inPanel("detailsClose").forceActiveFocus();
                win.mdTabs = 0;
                win.mdNames = "";
                win.next("mdKeys");
                return;
            }
            // ---- the keyboard: Tab to "All Media and Files", Return, Tab to the grid, the arrow keys, Return ----
            case "mdKeys": {
                if (win.focusName() === "mediaAll") {
                    // (BQ-5, P5a: the walk is re-counted from the page: the X, "More" of the about text when shown, the
                    // timer, "View Safety Number" (P5a's one more stop), "Copy Link" when enabled (P5b), each shown group row and "Show All" when shown,
                    // then the section's button; a changed cycle fails here with the stops it went through.)
                    const want = ["detailsClose"];
                    // (F1: the about text is one stop, before its "More".)
                    if (win.shown("detailsAbout")) want.push("detailsAboutText");
                    if (win.shown("detailsAboutMore")) want.push("detailsAboutMore");
                    if (win.shown("detailsTimerBox")) want.push("detailsTimerBox");
                    want.push("detailsSafety");
                    // (P5b: "Copy Link" right under it, a stop when it is enabled: a disabled button takes no Tab.)
                    const link = win.shown("detailsLink");
                    if (link && link.enabled) want.push("detailsLink");
                    // (P5c: "Share Contact…" right under Copy Link, a stop when it is enabled.)
                    const share = win.shown("detailsShare");
                    if (share && share.enabled) want.push("detailsShare");
                    for (const g of win.findAll(win.chat.panel, c => c.objectName === "detailsGroup" && c.visible)) want.push("detailsGroup");
                    if (win.shown("detailsShowAll")) want.push("detailsShowAll");
                    if (win.mdNames !== want.join(", ")) return win.fail(win.mdTabs + " presses of Tab reached All Media and Files, not " + want.length + "; the keyboard went through: " + win.mdNames + "; the page's stops are: " + want.join(", "));
                    const all = win.shown("mediaAll");
                    if (!all || !all.activeFocus || all.Accessible.name !== "All Media and Files") return win.fail("the keyboard is on a button that is not the shown All Media and Files");
                    win.key(Qt.Key_Return, Qt.NoModifier);
                    win.next("mdKeysLevel");
                    return;
                }
                if (win.mdTabs >= 20) return win.fail("20 presses of Tab did not reach All Media and Files; the keyboard went through: " + win.mdNames);
                win.mdTabs += 1;
                win.mdNames += (win.mdNames === "" ? "" : ", ") + win.focusName();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                return;
            }
            case "mdKeysLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("Return on All Media and Files opens the level");
                if (panel.levels.join() !== "person,media") return win.fail("the levels: " + panel.levels.join());
                if (App.mediaGrid.count !== win.mdExpected) return win.later("the grid: " + App.mediaGrid.count + " of " + win.mdExpected);
                win.mdTabs = 0;
                win.mdNames = "";
                win.next("mdGridTab");
                return;
            }
            case "mdGridTab": {
                if (win.focusName() === "mediaGrid") {
                    win.next("mdGridKeys");
                    return;
                }
                if (win.mdTabs >= 8) return win.fail("8 presses of Tab did not reach the grid; the keyboard went through: " + win.mdNames);
                win.mdTabs += 1;
                win.mdNames += (win.mdNames === "" ? "" : ", ") + win.focusName();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                return;
            }
            // The arrow keys move the current cell: Right goes to a cell, Right again to the next, Down one row.
            case "mdGridKeys": {
                const grid = win.shown("mediaGrid");
                win.key(Qt.Key_Right, Qt.NoModifier);
                const first = grid.currentIndex;
                if (first < 0) return win.fail("Right did not move the current cell of the grid");
                win.key(Qt.Key_Right, Qt.NoModifier);
                if (grid.currentIndex !== first + 1) return win.fail("Right moved from " + first + " to " + grid.currentIndex);
                win.key(Qt.Key_Down, Qt.NoModifier);
                if (grid.currentIndex !== first + 1 + grid.columns) return win.fail("Down moved to " + grid.currentIndex + ", not one row (" + grid.columns + " columns) from " + (first + 1));
                const cell = grid.currentItem;
                if (!cell || cell.mediaState !== "local") return win.fail("the current cell is not a downloaded picture");
                win.mdFirst = cell.attachment;
                win.viewersBefore = win.viewersOpened;
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("mdKeysViewer");
                return;
            }
            case "mdKeysViewer": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("Return on the current cell opens the viewer");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows the cell's picture");
                if (win.viewersOpened !== win.viewersBefore + 1 || v.attachment !== win.mdFirst) return win.fail("Return opened " + (win.viewersOpened - win.viewersBefore) + " viewers, showing the current cell's picture: " + (v.attachment === win.mdFirst));
                if (v.gallery !== "media") return win.fail("the viewer follows the slot '" + v.gallery + "', not the level's");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mdKeysGone");
                return;
            }
            // Escape closes the viewer only; with the keyboard in the grid, Escape goes a level back (the lists close).
            case "mdKeysGone":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                if (win.chat.panel.level !== "media") return win.fail("the Escape that closed the viewer went a level back");
                win.shown("mediaGrid").forceActiveFocus();
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mdEscaped");
                return;
            case "mdEscaped": {
                if (win.chat.panel.level !== "person" || win.waits < 3) return win.later("Escape in the level goes back to the page");
                if (!win.chat.panelOpen) return win.fail("Escape at the media level closed the panel");
                if (win.chat.panel.levels.join() !== "person") return win.fail("the levels: " + win.chat.panel.levels.join());
                if (App.mediaGrid.loaded || App.mediaFiles.loaded || App.mediaGrid.count !== 0) return win.fail("the level's lists stay open after Escape");
                // Open the level again, and close the panel with the X while the level is on top.
                win.clickItem(win.shown("mediaAll"));
                win.next("mdXLevel");
                return;
            }
            case "mdXLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("the level again");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("mdXClosed");
                return;
            }
            case "mdXClosed":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes the panel from the media level");
                if (App.mediaGrid.loaded || App.mediaFiles.loaded || App.mediaGrid.count !== 0 || App.mediaFiles.count !== 0) return win.fail("the level's lists stay open after the X");
                win.next("mdPanelClosed");
                return;
            // The group's chat: three pictures are sent, the group page's section shows them (and the one stop "All Media
            // and Files", which every section has), a file sent meanwhile appears in it live, the level lists the
            // pictures and, under Files, the file; Back puts the keyboard on All Media and Files.
            case "mdPanelClosed":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes the panel");
                if (App.personMedia.count !== 0 || App.personMedia.loaded) return win.fail("the section's model is filled although no page is on screen");
                win.openConversation(THE_GROUP);
                win.next("mdGroupSend");
                return;
            case "mdGroupSend":
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 8) return win.later("the group's chat, for its pictures");
                if (App.composer.addFiles([FIRST_PICTURE, SECOND_PICTURE, THIRD_PICTURE]) !== 3) return win.fail("three pictures are added");
                win.next("mdGroupPlanned");
                return;
            case "mdGroupPlanned":
                // Send waits for the core's look at the files (their location verdicts).
                if (!App.composer.planned) return win.later("the plan of the three pictures");
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the pictures went with the send: " + App.composer.attachmentCount);
                win.next("mdGroupSent");
                return;
            case "mdGroupSent":
                if (App.composer.preparing !== 0) return win.later("the core stores the pictures");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("mdGroup");
                return;
            case "mdGroup": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group page, for its pictures");
                if (!App.groupMedia.loaded || App.groupMedia.count !== 3) return win.later("three squares on the group page: " + App.groupMedia.count);
                if (App.groupMedia.more || App.groupMedia.other) return win.fail("the section says more follows (more " + App.groupMedia.more + ", other " + App.groupMedia.other + ")");
                if (!win.shown("mediaAll")) return win.fail("the section is shown and its stop All Media and Files is not");
                const part = win.shown("groupMedia");
                if (!part) return win.fail("the group page has no media part");
                const panel = win.chat.panel;
                const y = item => item.mapToItem(panel, 0, 0).y;
                for (const name of ["groupMembers", "groupRules", "groupTimer"]) {
                    const other = win.shown(name);
                    if (other && y(other) >= y(part)) return win.fail("the group's media part is not after " + name);
                }
                if (App.composer.addFiles([THE_APP]) !== 1) return win.fail("a file is added");
                win.next("mdGroupFilePlanned");
                return;
            }
            case "mdGroupFilePlanned":
                if (!App.composer.planned) return win.later("the plan of the file");
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the file went with the send: " + App.composer.attachmentCount);
                win.next("mdGroupFile");
                return;
            case "mdGroupFile": {
                if (App.composer.preparing !== 0) return win.later("the core stores the file");
                if (!App.groupMedia.other) return win.later("the section learns that the chat has a file now");
                const show = win.shown("mediaAll");
                if (!show) return win.fail("All Media and Files is gone once the chat has a file");
                if (App.groupMedia.count !== 3) return win.fail("the squares changed with a file: " + App.groupMedia.count);
                win.clickItem(show);
                win.next("mdGroupLevel");
                return;
            }
            case "mdGroupLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("the level of the group's media");
                if (panel.levels.join() !== "group,media") return win.fail("the levels: " + panel.levels.join());
                if (App.mediaGrid.count !== 3 || App.mediaFiles.count !== 1) return win.later("the group's pictures and its file: " + App.mediaGrid.count + ", " + App.mediaFiles.count);
                win.clickItem(win.shown("mediaTabFiles"));
                win.next("mdGroupFiles");
                return;
            }
            case "mdGroupFiles": {
                const list = win.shown("mediaFilesList");
                if (!list || win.waits < 3) return win.later("the Files tab");
                const row = list.itemAtIndex(0);
                if (!row) return win.later("the file's row");
                if (row.fileName !== "Ground station.apk") return win.fail("the file's name");
                if (!win.find(row, c => c.objectName === "disc")) return win.fail("the row has no round action button");
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mdGroupBack");
                return;
            }
            case "mdGroupBack": {
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Back to the group page");
                const show = win.shown("mediaAll");
                if (!show || !show.activeFocus) return win.later("the keyboard on All Media and Files (" + win.focusName() + ")");
                // Fifty more pictures (one picture added fifty times): more than the level's first page of 48.
                if (App.composer.addFiles(Array(50).fill(FIRST_PICTURE)) !== 50) return win.fail("fifty pictures are added");
                win.next("mdManyPlanned");
                return;
            }
            case "mdManyPlanned":
                if (!App.composer.planned) return win.later("the plan of the fifty pictures");
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the fifty pictures went with the send: " + App.composer.attachmentCount);
                win.next("mdMany");
                return;
            case "mdMany":
                if (App.composer.preparing !== 0) return win.later("the core stores the fifty pictures");
                // The section still shows six squares, and now says that more follow.
                if (App.groupMedia.count !== 6 || !App.groupMedia.more) return win.later("six squares and more behind them: " + App.groupMedia.count + ", more " + App.groupMedia.more);
                if (!win.shown("mediaAll")) return win.later("All Media and Files");
                // (The page is taller than the window now: the part is at its end.)
                {
                    const flick = win.shown("groupScroll").contentItem;
                    flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                }
                win.next("mdManyScrolled");
                return;
            case "mdManyScrolled": {
                // (One run of 2026-10-25, in the P5a round 3, lost this click and never made it again: the level stayed on
                // the group's page for the 60 s of the step. The page may still be growing under the scroll while the fifty
                // pictures are stored, so the end is asked for again and the click waits until the button lies inside the
                // window. There is NO second click in `mdManyLevel`: a retry would hide a real lost click, and a second
                // failure of this step is the second strike of the flaky-test rule.)
                const flick = win.shown("groupScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                const all = win.shown("mediaAll");
                const at = all ? all.mapToItem(null, all.width / 2, all.height / 2) : null;
                if (win.waits < 3 || !at || at.y < 0 || at.y > win.height) return win.later("the page to scroll");
                win.clickItem(all);
                win.next("mdManyLevel");
                return;
            }
            case "mdManyLevel": {
                if (win.chat.panel.level !== "media" || !win.chat.panel.loaded || win.waits < 3) return win.later("the level of the group's media, with many pictures (level " + win.chat.panel.level + ", loaded " + win.chat.panel.loaded + ")");
                // The first page: 48 of the 53, and the grid goes on from its last row when it is scrolled to the end.
                if (App.mediaGrid.count !== 48 || !App.mediaGrid.more) return win.later("the first page of 48: " + App.mediaGrid.count + ", more " + App.mediaGrid.more);
                win.waits = 0;
                win.shown("mediaGrid").positionViewAtEnd();
                win.next("mdPaged");
                return;
            }
            case "mdPaged": {
                if (App.mediaGrid.count < 53 || win.waits < 3) {
                    // (A view that is still short of its end asks again when it is moved.)
                    win.shown("mediaGrid").positionViewAtEnd();
                    return win.later("the next page: " + App.mediaGrid.count + " of 53");
                }
                if (App.mediaGrid.count !== 53 || App.mediaGrid.more) return win.fail("the last page: " + App.mediaGrid.count + ", more " + App.mediaGrid.more);
                const grid = win.shown("mediaGrid");
                if (grid.contentY <= grid.originY + 1) return win.fail("the grid was reset to its top by the new page");
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mdManyBack");
                return;
            }
            // ---- a square of ANOTHER chat than the open one: a member's page opened from the group page ----
            // Her chat is not the open chat: her square's message is not in the open timeline, and the viewer opens
            // with the section's own rows, once, and follows the section (it closes when its message is gone).
            case "mdManyBack":
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Back to the group page");
                win.loadsBefore = win.personLoads;
                win.chat.panel.openMember(win.herUser);
                win.next("mdMember");
                return;
            case "mdMember": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("her page, opened from the group page");
                if (panel.levels.join() !== "group,person" || App.person.user !== win.herUser || App.person.chat !== win.herChat) return win.fail("the levels: " + panel.levels.join());
                if (!App.personMedia.loaded || App.personMedia.count < 1) return win.later("her square: " + App.personMedia.count);
                const squares = win.findAll(panel, c => c.objectName === "mediaSquare" && c.visible);
                if (squares.length !== App.personMedia.count) return win.fail("her section shows " + squares.length + " squares of " + App.personMedia.count);
                if (App.viewerMessageLoaded(squares[0].message)) return win.fail("her picture's message is in the open timeline: the test proves nothing");
                if (squares[0].mediaState !== "local") return win.later("her square is downloaded");
                win.mdFirst = squares[0].attachment;
                win.mdMessage = squares[0].message;
                win.mdFull = squares[0].fullUrl;
                win.viewersBefore = win.viewersOpened;
                win.clickItem(squares[0]);
                win.next("mdMemberViewer");
                return;
            }
            case "mdMemberViewer": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("a click on her square opens the viewer");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows her picture");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("one click opened " + (win.viewersOpened - win.viewersBefore) + " viewers");
                if (v.attachment !== win.mdFirst || v.message !== win.mdMessage || v.gallery !== "person") return win.fail("the viewer: her picture " + (v.attachment === win.mdFirst) + ", her message " + (v.message === win.mdMessage) + ", follows '" + v.gallery + "'");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mdMemberGone");
                return;
            }
            // The old way into the viewer, with a message the open timeline does not hold (a picture of another chat):
            // no script error, the picture alone.
            case "mdMemberGone":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                if (!win.chat.panelOpen) return win.fail("the Escape that closed the viewer closed the panel too");
                win.viewersBefore = win.viewersOpened;
                MediaActions.view(win.mdFirst, win.mdFull, false, true, win.mdMessage + 1000000);
                win.next("mdUnknown");
                return;
            case "mdUnknown": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("a message the timeline does not hold still opens the picture");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows the picture");
                if (win.viewersOpened !== win.viewersBefore + 1 || v.attachment !== win.mdFirst) return win.fail("the picture of an unknown message: " + (win.viewersOpened - win.viewersBefore) + " viewers");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("mdUnknownGone");
                return;
            }
            // Mirrored (a right-to-left language): one or two squares stand at the LEADING edge, the right one.
            case "mdUnknownGone":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("Escape closes the viewer");
                win.LayoutMirroring.enabled = true;
                win.LayoutMirroring.childrenInherit = true;
                win.waits = 0;
                win.next("mdMirror");
                return;
            case "mdMirror": {
                if (win.waits < 8) return win.later("the mirrored layout");
                const panel = win.chat.panel;
                const squares = win.findAll(panel, c => c.objectName === "mediaSquare" && c.visible);
                if (squares.length < 1 || squares.length > 2) return win.fail("her section shows " + squares.length + " squares; the test needs one or two");
                const margin = Kirigami.Units.largeSpacing;
                const q = squares[0].mapToItem(panel, 0, 0);
                if (!panel.LayoutMirroring.enabled) return win.fail("the panel is not mirrored: the test does not mirror");
                const right = q.x + squares[0].width;
                if (Math.abs(right - (panel.width - margin)) > 2) return win.fail("the first square ends " + (panel.width - margin - right) + " px from the leading (right) edge");
                const all = win.shown("mediaAll");
                const a = all.mapToItem(panel, 0, 0);
                if (Math.abs(a.x + all.width - (panel.width - margin)) > 2) return win.fail("All Media and Files is not at the leading (right) edge");
                win.LayoutMirroring.enabled = false;
                win.waits = 0;
                win.next("mdMirrorBack");
                return;
            }
            // Her level: the oldest picture of her chat (not in the open timeline) opens the viewer, and a picture that is
            // deleted takes the viewer with it.
            case "mdMirrorBack": {
                if (win.waits < 8) return win.later("the layout back");
                const squares = win.findAll(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible);
                if (Math.abs(squares[0].mapToItem(win.chat.panel, 0, 0).x - Kirigami.Units.largeSpacing) > 2) return win.fail("the square is not at the left edge again");
                // (P5c: with "Share Contact…" the member's page is taller than the panel here: the button is scrolled into
                // the window before the real click, as `mdManyScrolled` does.)
                const flick = win.shown("detailsScroll").contentItem;
                flick.contentY = Math.max(0, flick.contentHeight - flick.height);
                const all = win.shown("mediaAll");
                const at = all ? all.mapToItem(null, all.width / 2, all.height / 2) : null;
                if (!at || at.y < 0 || at.y > win.height) return win.later("her page scrolled to All Media and Files");
                win.clickItem(all);
                win.next("mdMemberLevel");
                return;
            }
            case "mdMemberLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("her level of all media");
                if (panel.levels.join() !== "group,person,media") return win.fail("the levels: " + panel.levels.join());
                const grid = win.shown("mediaGrid");
                if (App.mediaGrid.count < 1 || !grid) return win.later("her pictures in the grid: " + App.mediaGrid.count);
                const cell = grid.itemAtIndex(App.mediaGrid.count - 1);
                if (!cell || cell.mediaState !== "local") return win.later("her oldest picture's cell, downloaded");
                if (App.viewerMessageLoaded(cell.message)) return win.fail("her oldest picture's message is in the open timeline");
                win.mdFirst = cell.attachment;
                win.mdMessage = cell.message;
                win.viewersBefore = win.viewersOpened;
                win.clickItem(cell);
                win.next("mdMemberCell");
                return;
            }
            case "mdMemberCell": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("a click on her oldest picture opens the viewer");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows her oldest picture");
                if (win.viewersOpened !== win.viewersBefore + 1) return win.fail("one click opened " + (win.viewersOpened - win.viewersBefore) + " viewers");
                if (v.attachment !== win.mdFirst || v.gallery !== "media") return win.fail("the viewer: her picture " + (v.attachment === win.mdFirst) + ", follows '" + v.gallery + "'");
                // The shown picture is deleted: the viewer follows the gallery and closes, and the grid empties.
                App.deleteAttachment(win.mdFirst);
                win.next("mdMemberDeleted");
                return;
            }
            case "mdMemberDeleted":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("the viewer closes when its picture leaves the gallery");
                if (App.mediaGrid.count !== 0) return win.later("the grid empties: " + App.mediaGrid.count);
                if (!win.chat.panelOpen || win.chat.panel.level !== "media") return win.fail("the delete closed the panel or the level: " + win.chat.panel.level);
                // ---- the stacking rule: a sender of the timeline replaces everything above the base levels ----
                win.loadsBefore = win.personLoads;
                win.chat.openSender(win.herUser);
                win.next("mdStack");
                return;
            case "mdStack": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page on top of the media level");
                if (panel.levels.join() !== "group,person") return win.fail("group,person,media + a sender gave " + panel.levels.join());
                if (App.mediaGrid.loaded || App.mediaFiles.loaded || App.mediaGrid.count !== 0) return win.fail("the media level's lists stay open under the new page");
                if (App.person.user !== win.herUser) return win.fail("the page on top is not the sender's");
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mdStackBack");
                return;
            }
            case "mdStackBack": {
                const panel = win.chat.panel;
                if (panel.level !== "group" || win.waits < 3) return win.later("Back from the sender's page to the group's");
                if (panel.levels.join() !== "group") return win.fail("the levels: " + panel.levels.join());
                // "All Media and Files" of the group, then a sender: group,media -> group,person.
                const all = win.shown("mediaAll");
                if (!all) return win.later("the group's All Media and Files");
                win.clickItem(all);
                win.next("mdStackLevel");
                return;
            }
            case "mdStackLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("the group's level of all media");
                if (panel.levels.join() !== "group,media") return win.fail("the levels: " + panel.levels.join());
                if (App.mediaGrid.count !== 48) return win.later("the group's first page: " + App.mediaGrid.count);
                win.loadsBefore = win.personLoads;
                win.chat.openSender(win.herUser);
                win.next("mdStackSender");
                return;
            }
            case "mdStackSender": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page on top of the group's media level");
                if (panel.levels.join() !== "group,person") return win.fail("group,media + a sender gave " + panel.levels.join());
                if (App.mediaGrid.loaded || App.mediaFiles.loaded || App.mediaGrid.count !== 0) return win.fail("the media level's lists stay open under the new page");
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mdStackAgain");
                return;
            }
            // Back to the group, and "All Media and Files" once more: ONE media level, with its rows again.
            case "mdStackAgain": {
                const panel = win.chat.panel;
                if (panel.level !== "group" || win.waits < 3) return win.later("Back to the group page");
                const all = win.shown("mediaAll");
                if (!all) return win.later("the group's All Media and Files");
                win.clickItem(all);
                win.next("mdStackAgainLevel");
                return;
            }
            case "mdStackAgainLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "media" || !panel.loaded || win.waits < 3) return win.later("the group's level of all media again");
                if (panel.levels.join() !== "group,media") return win.fail("the levels: " + panel.levels.join());
                if (panel.levels.filter(l => l === "media").length !== 1) return win.fail("more than one media level: " + panel.levels.join());
                if (App.mediaGrid.count !== 48 || !App.mediaGrid.loaded) return win.later("the rows again: " + App.mediaGrid.count);
                win.key(Qt.Key_Left, Qt.AltModifier);
                win.next("mdAlbumSend");
                return;
            }
            // ---- the viewer of a section's square follows its MESSAGE, with a query of its own ----
            // Eight pictures sent into the group (the composer makes albums of them: the sixth square is the first
            // picture of an album of three, of which the section shows ONE). The viewer opened on that square steps
            // through the whole album (the section cuts it), stays open on that picture when one more picture arrives
            // in the chat (the section moves on, the picture is no square any more), and closes only when its message
            // is deleted.
            case "mdAlbumSend": {
                if (win.chat.panel.level !== "group" || win.waits < 3) return win.later("Back to the group page, for the album");
                if (App.composer.addFiles(Array(8).fill(FIRST_PICTURE)) !== 8) return win.fail("eight pictures are added");
                win.next("mdAlbumPlanned");
                return;
            }
            case "mdAlbumPlanned":
                if (!App.composer.planned) return win.later("the plan of the eight pictures");
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the eight pictures went with the send: " + App.composer.attachmentCount);
                win.next("mdAlbumSent");
                return;
            case "mdAlbumSent": {
                if (App.composer.preparing !== 0) return win.later("the core stores the eight pictures");
                const sq = win.findAll(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible);
                if (sq.length !== 6 || win.waits < 5) return win.later("six squares of the album: " + sq.length);
                if (sq.some(q => q.mediaState !== "local")) return win.later("the album's squares are downloaded");
                const squaresOf = sq.filter(q => q.message === sq[5].message).length;
                const album = App.viewerItems(sq[5].message).length;
                if (album <= squaresOf) return win.fail("the sixth square's album is not cut by the section: " + squaresOf + " squares of " + album + " pictures");
                win.mdColumns = album;
                win.mdFirst = sq[5].attachment;
                win.mdMessage = sq[5].message;
                win.viewersBefore = win.viewersOpened;
                MediaActions.viewGallery("group", sq[5].attachment, sq[5].message);
                win.next("mdAlbumViewer");
                return;
            }
            case "mdAlbumViewer": {
                const v = win.imageViewer;
                if (win.viewersOpened === win.viewersBefore) return win.later("the viewer opens on the sixth square");
                if (!v || !v.opened || !v.shown) return win.later("the viewer shows the picture");
                if (win.viewersOpened !== win.viewersBefore + 1 || v.attachment !== win.mdFirst || v.gallery !== "group") return win.fail("the viewer: one " + (win.viewersOpened === win.viewersBefore + 1) + ", the square's picture " + (v.attachment === win.mdFirst) + ", from '" + v.gallery + "'");
                if (v.count !== win.mdColumns) return win.later("the viewer's own query brings the whole album: " + v.count + " of " + win.mdColumns);
                if (v.attachment !== win.mdFirst) return win.fail("the album's arrival moved the viewer off its picture");
                // The panel closing ends the pages' places and leaves the viewer's own query alone (the section is asked
                // for again at once: the next step needs its squares).
                App.closeChatMedia("all");
                if (!App.galleryLoaded("viewer")) return win.fail("closeChatMedia('all') ended the viewer's own query");
                win.inPanel("groupMedia").sync();
                if (App.composer.addFiles([SECOND_PICTURE]) !== 1) return win.fail("one more picture is added");
                win.next("mdAlbumMorePlanned");
                return;
            }
            case "mdAlbumMorePlanned":
                if (!App.composer.planned) return win.later("the plan of the one more picture");
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the picture went with the send: " + App.composer.attachmentCount);
                win.next("mdAlbumMore");
                return;
            case "mdAlbumMore": {
                if (App.composer.preparing !== 0) return win.later("the core stores the picture");
                const sq = win.findAll(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible);
                // The section moved on (the new picture is the first square, the sixth picture of the album is none any
                // more), and a second passes for a viewer that would close on it.
                if (sq.some(q => q.attachment === win.mdFirst) || sq.length !== 6 || win.waits < 25) return win.later("the section moves on without the viewer's picture");
                const v = win.imageViewer;
                if (!v || !v.visible) return win.fail("the viewer closed when a picture arrived in the chat: its message still has its pictures");
                if (v.count !== win.mdColumns || v.attachment !== win.mdFirst) return win.fail("the viewer's album changed: " + v.count + " pictures, on its picture: " + (v.attachment === win.mdFirst));
                App.deleteMessage(win.mdMessage, false);
                win.next("mdAlbumDeleted");
                return;
            }
            case "mdAlbumDeleted":
                if (win.imageViewer !== null && win.imageViewer.visible) return win.later("the viewer closes when its message is deleted");
                if (App.galleryLoaded("viewer")) return win.later("the viewer's own query ends with the viewer");
                if (!win.chat.panelOpen) return win.fail("the delete closed the panel");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("mdDone");
                return;
            case "mdDone":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes the panel");
                if (App.groupMedia.count !== 0 || App.groupMedia.loaded) return win.fail("the group's section stays filled after the panel closed");
                win.next("svOpen");
                return;
            // ---- P5a: the safety number of a person (the level "safety" above her page) ----
            case "svOpen":
                win.openConversation(PERSON);
                win.next("svHeader");
                return;
            case "svHeader": {
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("her chat, for her safety number");
                // Her chat is NOT the newest of the list (critic F18): "the quiet notice does not move it" proves
                // something only then.
                win.svIndex = win.listIndex(PERSON);
                if (win.svIndex <= 0) return win.fail("her chat is not in the list or is its newest row: the place proves nothing (" + win.svIndex + ")");
                win.svSystem = win.systemRows();
                win.clickHeader();
                win.next("svRow");
                return;
            }
            case "svRow": {
                if (!win.panelReady() || win.waits < 3) return win.later("her page");
                const row = win.shown("detailsSafety");
                if (!row) return win.fail("her page offers no View Safety Number");
                if (row.Accessible.name !== "View Safety Number") return win.fail("the button has no accessible name");
                // A screen reader that Tabs hears the state on the button itself (U9): nothing yet for a person never
                // verified, "Verified" after Mark (svMark), "Safety number changed" while her sends wait.
                if (row.Accessible.description !== "") return win.fail("the button describes a state for a person never verified");
                // The place: after Disappearing Messages, before Groups in Common (Tab from the timer reaches it; since
                // P5b Tab again reaches Copy Link right under it, since P5c Share Contact under that, and once more the
                // first group row).
                win.inPanel("detailsTimerBox").forceActiveFocus();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsSafety") return win.fail("Tab after the timer reaches '" + win.focusName() + "', not View Safety Number");
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsLink") return win.fail("Tab after View Safety Number reaches '" + win.focusName() + "', not Copy Link");
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsShare") return win.fail("Tab after Copy Link reaches '" + win.focusName() + "', not Share Contact");
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsGroup") return win.fail("Tab after Share Contact reaches '" + win.focusName() + "', not the first group row");
                win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                win.svCoolAt = Date.now();
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("svDouble");
                return;
            }
            // A SECOND Return right behind the one that opened the level (a double press, a key that bounces), at the first
            // tick that has the answer and the level's keyboard placed (W1): the keyboard is on the digits, so it marks
            // nothing; a number nobody has read is never verified by a key press.
            case "svDouble": {
                const panel = win.chat.panel;
                const page = win.safetyPageItem();
                if (panel.level !== "safety" || !panel.loaded || win.safetyActions() === "" || page.pending !== "") return win.later("the level has its answer and the keyboard");
                if (win.focusName() !== "safetyDigits") return win.fail("the answer put the keyboard on '" + win.focusName() + "', not on the digits");
                // The buttons of a number that has just appeared are dead for a second (X1): at this first tick they are
                // (counted from before the Return of svRow, so the time here is never less than the page's own).
                const dt = Date.now() - win.svCoolAt;
                if (dt < 900 && (!page.cooling || win.safetyButton("mark").enabled)) return win.fail("the first button of a number that appeared " + dt + " ms ago is alive");
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.key(Qt.Key_Enter, Qt.NoModifier);
                win.next("svLevel");
                return;
            }
            case "svLevel": {
                const panel = win.chat.panel;
                // (Eight ticks, 320 ms: the answer to a press of svDouble that did act would be here by then.)
                if (panel.level !== "safety" || !panel.loaded || win.waits < 8) return win.later("Return opens her safety number");
                // (The buttons of a number that has just appeared are dead for a second, X1: a disabled button is no tab stop,
                // so Tab from the digits finds the first button only once they are awake.)
                if (win.safetyPageItem().cooling) return win.later("the buttons of her number wake up");
                if (panel.levels.join() !== "person,safety") return win.fail("the levels: " + panel.levels.join());
                // The number from the fake: 12 groups of 5 digits (compared, never printed), the QR code, the names a
                // screen reader hears (the pane's, critic F8, and the digits' as one text).
                const groups = win.findAll(panel, c => c.objectName === "safetyGroup" && c.visible);
                if (groups.length !== 12 || groups.some(g => !/^[0-9]{5}$/.test(g.text))) return win.fail("the number is not 12 groups of 5 digits: " + groups.length + " groups");
                if (groups.some(g => g.textFormat !== Text.PlainText)) return win.fail("a digit group is not plain text");
                if (!win.shown("safetyQr")) return win.fail("no QR code");
                if (panel.Accessible.name === "" || panel.Accessible.name !== App.safety.accessibleTitle) return win.fail("the pane is not named after her safety number");
                if (win.shown("safetyDigits").Accessible.name !== App.safety.accessibleDigits || App.safety.accessibleDigits === "") return win.fail("the digits have no accessible text");
                if (win.safetyActions() !== "mark") return win.fail("a person never verified offers: " + win.safetyActions());
                if (win.shown("safetyState") !== null) return win.fail("a state line for a person never verified");
                // The keyboard is on the DIGITS (W1), where the number is read first, never on a button: the Return and the
                // Enter of svDouble marked nothing (the level still offers Mark only, and no quiet notice came: below).
                if (win.focusName() !== "safetyDigits") return win.fail("opening the level left the keyboard on '" + win.focusName() + "', not on the digits");
                if (win.shown("safetyDigits").visualFocus) return win.fail("the digits show a keyboard frame before any Tab");
                // Tab reaches the first button (and Shift+Tab comes back to the digits).
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "safetyButton" || !win.safetyButton("mark").activeFocus) return win.fail("Tab from the digits reaches '" + win.focusName() + "', not the first button");
                win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                if (win.focusName() !== "safetyDigits" || !win.shown("safetyDigits").visualFocus) return win.fail("Shift+Tab from the first button reaches '" + win.focusName() + "', not the digits with their frame");
                // The bridge acts only with a button the level on screen offers (U11): only Mark is offered, so Confirm,
                // Clear and a name that is none send nothing.
                if (App.safetyAction("confirm") !== false || App.safetyAction("clear") !== false || App.safetyAction("nonsense") !== false) return win.fail("the bridge acted with a button the level does not offer");
                if (win.systemRows() !== win.svSystem) return win.fail("an action that is not offered changed her chat");
                win.clickItem(win.findAll(panel, c => c.objectName === "safetyButton" && c.visible)[0]);
                win.next("svMark");
                return;
            }
            // Mark: the fake hears SetVerified with the shown key and answers Verified; the quiet notice lands in her chat
            // and the chat keeps its place in the list.
            case "svMark": {
                if (win.safetyActions() !== "clear" || win.waits < 3) return win.later("the level after Mark: " + win.safetyActions());
                const line = win.shown("safetyState");
                if (!line || line.text !== "Verified") return win.fail("the level's state line after Mark");
                const under = win.inPanel("detailsSafetyState");
                if (!under || under.text !== "Verified") return win.later("her page's line under the button says Verified");
                if (win.systemRows() !== win.svSystem + 1) return win.later("the quiet notice in her chat: " + (win.systemRows() - win.svSystem));
                if (win.listIndex(PERSON) !== win.svIndex) return win.fail("the quiet notice moved her chat in the list: " + win.svIndex + " -> " + win.listIndex(PERSON));
                // The buttons were made anew and the one that had the keyboard is gone: it is on a button again (U3), and
                // the state is heard on the row and on the button (U9).
                if (win.focusName() !== "safetyButton") return win.fail("after Mark the keyboard is on '" + win.focusName() + "', not on a button of the level");
                if (win.inPanel("detailsSafety").Accessible.description !== "Verified") return win.fail("her page's button does not describe 'Verified'");
                if (win.safetyButton("clear").Accessible.description !== "Verified") return win.fail("the level's button does not describe 'Verified'");
                // Clear Verification is the primary button now and never carries the check mark (V6).
                if (!win.safetyButton("clear").highlighted || win.safetyButton("clear").icon.name !== "") return win.fail("Clear Verification has an icon or is not the primary button: '" + win.safetyButton("clear").icon.name + "'");
                // Clear with the KEYBOARD (Return on the button the keyboard is on).
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("svClear");
                return;
            }
            case "svClear": {
                if (win.safetyActions() !== "mark" || win.waits < 3) return win.later("the level after Clear: " + win.safetyActions());
                if (win.shown("safetyState") !== null) return win.fail("a state line after Clear");
                if (win.inPanel("detailsSafetyState").visible) return win.later("her page's line goes after Clear");
                if (win.systemRows() !== win.svSystem + 2) return win.later("the second quiet notice: " + (win.systemRows() - win.svSystem));
                if (win.listIndex(PERSON) !== win.svIndex) return win.fail("the quiet notice moved her chat in the list");
                if (win.focusName() !== "safetyButton") return win.fail("after Clear (by the keyboard) the keyboard is on '" + win.focusName() + "', not on a button of the level");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("svBack");
                return;
            }
            // Escape goes one level back: her page, the keyboard on View Safety Number; the level is closed.
            case "svBack": {
                const panel = win.chat.panel;
                if (panel.level !== "person" || win.waits < 3) return win.later("Escape back to her page");
                if (panel.levels.join() !== "person" || !win.chat.panelOpen) return win.fail("the levels after Escape: " + panel.levels.join());
                if (win.focusName() !== "detailsSafety") return win.fail("Back left the keyboard on '" + win.focusName() + "', not on View Safety Number");
                if (App.safety.title !== undefined) return win.fail("the level's map stays filled after Back");
                // No level on screen, no held view: nothing a button could act on (U11).
                const rowsBefore = win.systemRows();
                if (App.safetyAction("mark") !== false || App.safetyAction("confirm") !== false) return win.fail("the bridge acted with no level on screen");
                if (win.systemRows() !== rowsBefore) return win.fail("an action with no level changed her chat");
                win.next("svRowClick");
                return;
            }
            // The MOUSE twin of svRow / svDouble (X1): the row "View Safety Number" picked by a REAL click, and a real click
            // on the first button at the first tick that button exists. The number was on screen for a few milliseconds, so
            // the button is dead and the click acts on nothing (a Mark of a number nobody has read); the buttons wake up
            // after a second.
            case "svRowClick": {
                const row = win.shown("detailsSafety");
                if (!row) return win.fail("her page offers no View Safety Number to click");
                win.svCoolAt = Date.now();
                win.clickItem(row);
                win.next("svRowMouse");
                return;
            }
            case "svRowMouse": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "safety" || !panel.loaded || win.safetyActions() === "") return win.later("the clicked row opens her number");
                const page = win.safetyPageItem();
                const mark = win.safetyButton("mark");
                if (!mark) return win.fail("the level offers: " + win.safetyActions());
                const dt = Date.now() - win.svCoolAt;
                if (dt < 900 && (!page.cooling || mark.enabled)) return win.fail("the first button of a number that appeared " + dt + " ms after a click on the row is alive");
                // (A first tick that came after 900 ms says nothing about a dead button, and a click on an awake one would mark the
                // number: on a machine so busy the click is left out.)
                if (dt >= 900) console.warn("svRowMouse: the first tick came " + dt + " ms after the click on the row: a busy machine");
                win.svRows = win.systemRows();
                if (dt < 900) win.clickItem(mark);
                win.next("svRowQuiet");
                return;
            }
            case "svRowQuiet": {
                if (win.waits < 8) return win.later("a click on a dead button acts on nothing");
                const page = win.safetyPageItem();
                // Nothing was marked: the level still offers Mark only, no quiet notice came, nothing was pending.
                if (win.safetyActions() !== "mark" || win.systemRows() !== win.svRows || page.pending !== "" || page.pressed !== "") return win.fail("a click on the first button of a number that had just appeared acted (offers '" + win.safetyActions() + "', quiet notices " + (win.systemRows() - win.svRows) + ", pending '" + page.pending + "')");
                if (page.cooling) return win.later("the buttons wake up after a second");
                if (!win.safetyButton("mark").enabled) return win.fail("the buttons stay dead after the second");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svGroup");
                return;
            }
            // The stacking rule: a sender clicked with group,person,safety on top gives group,person.
            case "svGroup":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.openConversation(THE_GROUP);
                win.next("svGroupHeader");
                return;
            case "svGroupHeader":
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 6) return win.later("the group, for the stacking rule");
                win.clickHeader();
                win.next("svGroupPage");
                return;
            case "svGroupPage":
                if (!win.panelReady() || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group's page");
                win.loadsBefore = win.personLoads;
                win.chat.panel.openMember(win.herUser);
                win.next("svMember");
                return;
            case "svMember": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("her page above the group's");
                panel.showSafety();
                win.next("svMemberLevel");
                return;
            }
            case "svMemberLevel": {
                const panel = win.chat.panel;
                if (panel.level !== "safety" || !panel.loaded || win.waits < 3) return win.later("her safety number above the group's page");
                if (panel.levels.join() !== "group,person,safety") return win.fail("the levels: " + panel.levels.join());
                win.loadsBefore = win.personLoads;
                win.chat.openSender(win.herUser);
                win.next("svStack");
                return;
            }
            case "svStack": {
                const panel = win.chat.panel;
                if (win.personLoads <= win.loadsBefore || panel.level !== "person" || !panel.loaded || win.waits < 3) return win.later("the sender's page replaces the safety level");
                if (panel.levels.join() !== "group,person") return win.fail("group,person,safety + a sender gave " + panel.levels.join());
                if (App.safety.title !== undefined) return win.fail("the safety level stays asked for under the sender's page");
                panel.showSafety();
                win.next("svX");
                return;
            }
            // The X closes everything with the level on top.
            case "svX": {
                const panel = win.chat.panel;
                if (panel.level !== "safety" || !panel.loaded || win.waits < 3) return win.later("her safety number again");
                if (panel.levels.filter(l => l === "safety").length !== 1) return win.fail("more than one safety level: " + panel.levels.join());
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svXClosed");
                return;
            }
            case "svXClosed":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes the panel with the safety level on top");
                if (App.safety.title !== undefined) return win.fail("the safety level stays filled after the X");
                if (win.chat.panel.levels.length !== 0) return win.fail("levels after the X: " + win.chat.panel.levels.join());
                win.next("svQrOpen");
                return;
            // The QR code carries the scannable BYTES of the number on screen (critic M6, F11): the item's content is
            // those bytes (not a string), and the grabbed card is decoded by the Rust side after the run (quircs).
            case "svQrOpen":
                win.openConversation(PERSON);
                win.next("svQrHeader");
                return;
            case "svQrHeader":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("her chat, for the QR code");
                win.clickHeader();
                win.next("svQrPage");
                return;
            case "svQrPage":
                if (!win.panelReady() || win.waits < 3) return win.later("her page, for the QR code");
                win.chat.panel.showSafety();
                win.next("svQr");
                return;
            case "svQr": {
                const panel = win.chat.panel;
                if (panel.level !== "safety" || !panel.loaded || win.waits < 3) return win.later("her safety number, for the QR code");
                const qr = win.shown("safetyQr");
                if (!qr) return win.fail("no QR code");
                const want = new Uint8Array(App.safety.scannable);
                if (want.length !== 74) return win.fail("the scannable is not 74 bytes: " + want.length);
                // (A byte of 0x80 or more is what a string would carry as two bytes of UTF-8.)
                if (!want.some(b => b >= 0x80)) return win.fail("the fixture's scannable has no byte of 0x80 or more: the check would prove nothing");
                if (!(qr.content instanceof ArrayBuffer)) return win.fail("the QR code's content is not bytes");
                const got = new Uint8Array(qr.content);
                if (got.length !== want.length || got.some((b, i) => b !== want[i])) return win.fail("the QR code's content is not the scannable of the number on screen (" + got.length + " bytes)");
                win.qrSaved = false;
                win.shown("safetyQrCard").grabToImage(r => {
                    win.qrSaved = r.saveToFile(QR_PATH);
                });
                win.next("svQrSaved");
                return;
            }
            case "svQrSaved":
                if (!win.qrSaved) return win.later("the grab of the QR code is saved");
                win.LayoutMirroring.enabled = true;
                win.next("svMirror");
                return;
            // Right to left: the digit groups keep their order (numbers are left to right in Hebrew too, critic F9),
            // the back arrow mirrors.
            case "svMirror": {
                if (win.waits < 3) return win.later("the mirrored level");
                const groups = win.findAll(win.chat.panel, c => c.objectName === "safetyGroup" && c.visible);
                if (groups.length !== 12) return win.fail("mirrored: " + groups.length + " digit groups");
                const a = groups[0].mapToItem(null, 0, 0);
                const b = groups[1].mapToItem(null, 0, 0);
                if (Math.abs(a.y - b.y) > 1 || !(a.x < b.x)) return win.fail("mirrored: the first digit group is not left of the second on its row");
                const back = win.shown("safetyBack");
                if (!back || back.icon.name !== "go-next") return win.fail("mirrored: the back arrow does not point the other way");
                win.LayoutMirroring.enabled = false;
                win.next("svMirrorBack");
                return;
            }
            case "svMirrorBack": {
                if (win.waits < 3) return win.later("the level left to right again");
                const groups = win.findAll(win.chat.panel, c => c.objectName === "safetyGroup" && c.visible);
                if (!(groups[0].mapToItem(null, 0, 0).x < groups[1].mapToItem(null, 0, 0).x)) return win.fail("left to right: the digit groups are out of order");
                if (win.shown("safetyBack").icon.name !== "go-previous") return win.fail("left to right: the back arrow");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svChanged");
                return;
            }
            // A key change (critic F15, scripted in the fixture: her number was verified, then changed, and the fake
            // holds a second new key once the level has asked): a text to her pauses; the message menu's entry opens
            // her number; Mark of that (now stale) number is refused and the level shows the new one; Confirm of the
            // new one lets the text go.
            case "svChanged":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.openConversation(KEYPEER);
                win.next("svChangedChat");
                return;
            case "svChangedChat":
                if (App.timeline.conversationId !== KEYPEER || App.timeline.loading || win.waits < 6) return win.later("the chat of the person whose key changed");
                win.composerItem.setText("after her key changed");
                win.composerItem.send();
                win.next("svPaused");
                return;
            case "svPaused": {
                const r = win.newestOwn();
                // (The head of a paused queue is its message whose send met the changed key: the core offers its
                // person's safety number on it, MessageActions.safety_number.)
                if (!r || !r.pages.some(p => p.safety)) return win.later("the text waits for the new safety number" + (r ? " (receipt " + r.receipt + ", a wait line " + (r.wait !== "") + ", " + r.pages.length + " page entries)" : ""));
                if (r.receipt !== "pending" && r.receipt !== "failed") return win.fail("the waiting text is " + r.receipt);
                // The paused head shows as failed and says whose number it waits for (U4); nothing of the name is printed.
                if (r.receipt === "failed" && !/^Not sent: the safety number with .+ changed$/.test(r.wait)) return win.later("the failed head names the person (a line of " + r.wait.length + " characters)");
                win.svPaused = r.messageId;
                win.next("svPausedRow");
                return;
            }
            case "svPausedRow":
                if (win.rowWithKeyboard(win.svPaused) === null) return win.later("the keyboard on the waiting text");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("svMenu");
                return;
            case "svMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the Menu key opens the waiting text's menu");
                const pages = win.menuPages(m);
                const entry = pages.find(p => /^Show .+'s Safety Number$/.test(p.text));
                if (!entry) return win.fail("the waiting text's menu offers no safety number (" + pages.length + " page entries)");
                if (pages[0] !== entry) return win.fail("the safety number is not the menu's first page entry");
                if (entry.Accessible.name !== entry.text) return win.fail("the entry's accessible name is not its text");
                win.next("svMenuPick");
                return;
            }
            case "svMenuPick": {
                const m = win.view.messageMenu;
                const entry = win.menuPages(m).find(p => /^Show .+'s Safety Number$/.test(p.text));
                if (!m.visible || !entry) return win.fail("the menu closed before its entry was chosen");
                if (win.menuPick(m, entry)) win.next("svChangedLevel");
                return;
            }
            case "svChangedLevel": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "safety" || !panel.loaded || win.waits < 3) return win.later("the entry opens her safety number");
                // (The buttons of a number that has just appeared are dead for a second, X1: Mark takes the keyboard below only
                // once it is awake.)
                if (win.safetyPageItem().cooling) return win.later("the buttons of her changed number wake up");
                if (panel.levels.join() !== "person,safety") return win.fail("the levels: " + panel.levels.join());
                if (win.safetyActions() !== "confirm,mark") return win.fail("a changed number offers: " + win.safetyActions());
                const line = win.shown("safetyState");
                if (!line || line.text.indexOf("changed since you verified it") < 0) return win.fail("the level does not say her number changed since it was verified");
                if (line.textFormat !== Text.PlainText) return win.fail("the state line is not plain text");
                // Opened from the menu: the keyboard is on the digits, not on Confirm, the first button (W1), and the buttons
                // carry the state line for a screen reader (U9).
                if (win.focusName() !== "safetyDigits") return win.fail("the menu's entry left the keyboard on '" + win.focusName() + "', not on the digits");
                if (win.safetyButton("confirm").Accessible.description !== line.text || win.safetyButton("mark").Accessible.description !== line.text) return win.fail("the buttons do not describe the state line");
                if (win.safetyButton("confirm").icon.name !== "dialog-ok-apply" || win.safetyButton("mark").icon.name !== "") return win.fail("only the primary button has the icon (U8)");
                if (App.safety.swapped !== false) return win.fail("a first answer is a swap");
                win.svDigits = win.safetyDigits();
                // The keyboard on Mark (V1): a key that is still down acts once. A stand-in for its auto-repeat (the
                // handler's own function, the event stood in for) does nothing; the real Return marks, and the backend
                // refuses the old key (svStale).
                const mark = win.safetyButton("mark");
                const page = win.safetyPageItem();
                mark.forceActiveFocus();
                if (win.focusName() !== "safetyButton" || !mark.activeFocus) return win.fail("the keyboard could not be put on Mark: '" + win.focusName() + "'");
                const held = { isAutoRepeat: true, accepted: false };
                mark.activateKey(held);
                if (!held.accepted || page.pending !== "") return win.fail("an auto-repeat of Return acted (pending '" + page.pending + "') or was not taken");
                // The same for every other key (X2, Y3): the button's generic key handler (`holdKey`) takes the auto-repeat of
                // ANY key but Tab and Backtab and leaves the first press and the real release to the button (`accepted` stays
                // false: the generic handlers start not accepted). TestEvent cannot send an auto-repeat, so the handler's own
                // function gets stand-ins (Space and Select are the keys that click a button, the others show that no key is
                // named); the real Space is pressed in svKeyless. The two `Keys` lines that call it are run by no test.
                for (const k of [Qt.Key_Space, Qt.Key_Select, Qt.Key_Return, Qt.Key_Enter, Qt.Key_A]) {
                    const heldKey = { isAutoRepeat: true, accepted: false, key: k };
                    const firstKey = { isAutoRepeat: false, accepted: false, key: k };
                    mark.holdKey(heldKey);
                    mark.holdKey(firstKey);
                    if (!heldKey.accepted) return win.fail("an auto-repeat of the key " + k + " is not taken by the button's handler");
                    if (firstKey.accepted) return win.fail("the first press of the key " + k + " is taken from the button");
                }
                for (const k of [Qt.Key_Tab, Qt.Key_Backtab]) {
                    const heldTab = { isAutoRepeat: true, accepted: false, key: k };
                    mark.holdKey(heldTab);
                    if (heldTab.accepted) return win.fail("a held Tab is taken by the button: the keyboard would stop on it");
                }
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("svStale");
                return;
            }
            // The page was stale (the fake holds another key since it answered): Mark is refused, nothing is marked,
            // the inline line says why and the level shows the new number; the text still waits.
            case "svStale": {
                const notice = win.shown("safetyNotice");
                if (!notice || win.waits < 3) return win.later("the refusal of a stale Mark");
                if (notice.text !== "The safety number changed. Look at the new one before you mark it.") return win.fail("the refusal's line");
                if (notice.textFormat !== Text.PlainText) return win.fail("the refusal's line is not plain text");
                if (win.safetyDigits() === win.svDigits) return win.later("the level shows the new number");
                if (win.safetyActions() !== "confirm,mark") return win.fail("after the refusal the level offers: " + win.safetyActions());
                const r = win.rowOf(win.svPaused);
                if (!r || !r.pages.some(p => p.safety) || (r.receipt !== "pending" && r.receipt !== "failed")) return win.fail("the text left without a Confirm");
                // The answer that came after the refusal is ANOTHER number than the one on screen (U2): the map says so.
                if (App.safety.swapped !== true) return win.fail("the new number after a stale Mark is not marked as swapped");
                // The REAL swap path (onSafetyLoaded -> answered): the buttons are the same ones (confirm,mark), nothing was
                // rebuilt, and the keyboard that pressed Mark is OFF every button, on the back arrow (V1): one more Return
                // must not mark the number nobody compared.
                const page = win.safetyPageItem();
                // (The level has placed the keyboard and told the notice a moment after the answer.)
                if (page.announcedOn === "") return win.later("the level tells the notice");
                if (win.focusName() !== "safetyBack") return win.fail("after the swap the keyboard is on '" + win.focusName() + "', not on the back arrow");
                // The notice is told AFTER the keyboard was placed, and assertively (V4).
                if (page.announcedOn !== "safetyBack") return win.fail("the notice was announced with the keyboard on '" + page.announcedOn + "', not on the back arrow");
                if (Accessible.Assertive === undefined || page.announcedPoliteness !== Accessible.Assertive) return win.fail("the notice was not announced assertively (" + page.announcedPoliteness + ")");
                win.key(Qt.Key_Return, Qt.NoModifier);
                if (page.pending !== "" || !win.chat.panelOpen || win.chat.panel.level !== "safety") return win.fail("a second Return acted (pending '" + page.pending + "', level '" + win.chat.panel.level + "')");
                // The line's colour is readable on its background in a scheme where the raw negative colour is not (U5):
                // Breeze's own #da4453 on white is 4.3:1.
                const panel = win.chat.panel;
                panel.Kirigami.Theme.inherit = false;
                panel.Kirigami.Theme.backgroundColor = "white";
                panel.Kirigami.Theme.negativeTextColor = "#da4453";
                win.next("svColour");
                return;
            }
            case "svColour": {
                const notice = win.shown("safetyNotice");
                if (!notice || win.waits < 3) return win.later("the refusal line in the test colours");
                const ok = Style.contrast(notice.color, notice.Kirigami.Theme.backgroundColor) >= Style.minContrast;
                const raw = Qt.colorEqual(notice.color, "#da4453");
                win.chat.panel.Kirigami.Theme.inherit = true;
                if (!ok || raw) return win.fail("the refusal line is not drawn in a readable colour (contrast ok " + ok + ", the raw negative colour " + raw + ")");
                // The keyboard rule of a swapped number (U2, U3): a keyboard that fell to the scroll view goes to the back
                // arrow, never onto a button, when the number on screen turned into another; for the same number it goes
                // back to a button.
                if (!win.parkOnScroll()) return win.fail("the keyboard could not be parked on the level's scroll view: '" + win.focusName() + "'");
                win.svCoolAt = Date.now();
                win.chat.panel.safetyAnswered(true);
                // The buttons are dead the moment a swapped answer is here, before anything else can reach them (V1: the
                // check is made in the same turn as the call), stay dead for a second and wake up then (svSwapped, W4).
                if (win.safetyButton("confirm").enabled || win.safetyButton("mark").enabled || !win.safetyPageItem().cooling) return win.fail("the buttons stay alive right after a swapped number");
                win.next("svSwapped");
                return;
            }
            case "svSwapped": {
                if (win.waits < 3) return win.later("the keyboard after a swapped number");
                if (win.focusName() !== "safetyBack") return win.fail("a swapped number left the keyboard on '" + win.focusName() + "', not on the back arrow");
                // The second of dead buttons (V1, W4), counted from before the swapped answer was handed over, so the time
                // here is never less than the page's own: dead at every tick up to 900 ms (the ticks at about 300 ms and
                // about 900 ms are the ones the run should see), awake before 5 s. A timer of 1 ms, or one that restarts
                // at once, wakes them at the first tick; one that never ends stays dead past 5 s. The upper bound is loose
                // on purpose: the 1 s timer fires late on a loaded machine (1.9 s under `ci --quick`, 2026-10-27, against a
                // bound of 1.5 s then). (On a machine so busy that a tick skips a whole window the run says so on the console
                // and does not fail: a test that flaps under load is no proof.)
                const page = win.safetyPageItem();
                const dt = Date.now() - win.svCoolAt;
                if (page.coolingMs < 1000) return win.fail("the buttons are dead for " + page.coolingMs + " ms, not a second");
                if (dt < 900) {
                    if (!page.cooling || win.safetyButton("confirm").enabled || win.safetyButton("mark").enabled) return win.fail("the buttons woke up " + dt + " ms after a swapped number, before the second was over");
                    if (dt >= 200 && dt < 600) win.svSeen300 = true;
                    if (dt >= 700) win.svSeen900 = true;
                    return win.later("the buttons are dead for a second");
                }
                if (page.cooling) {
                    if (dt >= 5000) return win.fail("the buttons are still dead " + dt + " ms after a swapped number");
                    return win.later("the buttons wake up after a second");
                }
                if (!win.svSeen300 || !win.svSeen900) console.warn("svSwapped: the dead buttons were not seen in both windows (" + win.svSeen300 + ", " + win.svSeen900 + "): a busy machine");
                if (!win.safetyButton("confirm").enabled || !win.safetyButton("mark").enabled) return win.fail("the buttons stay dead after the second");
                // The rule itself, apart from the dead buttons (a button that is dead loses the keyboard on its own): a swapped
                // answer takes the keyboard off a button that is ALIVE, also one the answer did not rebuild (V1). The page's
                // own `answered` is called, not the panel's `safetyAnswered`, which would also deaden the buttons.
                win.safetyButton("mark").forceActiveFocus();
                if (win.focusName() !== "safetyButton") return win.fail("the keyboard could not be put on Mark: '" + win.focusName() + "'");
                page.answered(true, false);
                if (win.focusName() !== "safetyBack") return win.fail("a swapped answer left the keyboard on '" + win.focusName() + "', not on the back arrow");
                if (!win.parkOnScroll()) return win.fail("the keyboard could not be parked again: '" + win.focusName() + "'");
                win.chat.panel.safetyAnswered(false);
                win.next("svUnswapped");
                return;
            }
            case "svUnswapped": {
                if (win.waits < 3) return win.later("the keyboard after the same number");
                // The keyboard was lost and NO button was pressed since the swap (W2): it goes to the back arrow, not to a
                // button (the answer after a Mark or a Clear puts it on a button: svMark, svClear).
                if (win.focusName() !== "safetyBack") return win.fail("the same number, with no press, left the keyboard on '" + win.focusName() + "', not on the back arrow");
                // W2: the LAST PRESS survives an answer that rebuilt nothing. Confirm is pressed (the click sets it; the test
                // sets it without sending the command), an answer with the same buttons comes and the keyboard stays on
                // Confirm; then the answer that makes the buttons anew takes the keyboard off: it goes to the back arrow, not
                // to the first button (the Mark that is the only one left after an approval, R2).
                const page = win.safetyPageItem();
                win.safetyButton("confirm").forceActiveFocus();
                page.pressed = "confirm";
                page.answered(false, false);
                if (win.focusName() !== "safetyButton" || page.pressed !== "confirm") return win.fail("an answer that rebuilt nothing moved the keyboard to '" + win.focusName() + "' or used up the press ('" + page.pressed + "')");
                if (!win.parkOnScroll()) return win.fail("the keyboard could not be parked: '" + win.focusName() + "'");
                page.answered(false, false);
                if (win.focusName() !== "safetyBack" || page.pressed !== "") return win.fail("after a Confirm the keyboard went to '" + win.focusName() + "' (press '" + page.pressed + "'), not to the back arrow");
                // ... and after a Mark the press is kept the same way, and the keyboard returns to a button.
                win.safetyButton("mark").forceActiveFocus();
                page.pressed = "mark";
                page.answered(false, false);
                if (win.focusName() !== "safetyButton" || page.pressed !== "mark") return win.fail("an answer that rebuilt nothing moved the keyboard to '" + win.focusName() + "' or used up the press of Mark ('" + page.pressed + "')");
                if (!win.parkOnScroll()) return win.fail("the keyboard could not be parked: '" + win.focusName() + "'");
                page.answered(false, false);
                if (win.focusName() !== "safetyButton" || page.pressed !== "") return win.fail("after a Mark the keyboard went to '" + win.focusName() + "', not to a button");
                // X1, the page's flag of a number on screen: an answer WITHOUT a number clears it, and the number that follows is a
                // first sight (the buttons are dead for a second); the same number again is not. (An answer without a number cannot
                // be made here, so the page's own function is handed both.)
                page.arrived(false, false);
                if (page.numberShown) return win.fail("an answer without a number leaves the flag of a number on screen set");
                page.arrived(false, true);
                if (!page.cooling || win.safetyButton("confirm").enabled || win.safetyButton("mark").enabled) return win.fail("the number that followed an answer without one has live buttons");
                win.svCoolAt = Date.now();
                win.next("svKeyless");
                return;
            }
            case "svKeyless": {
                const page = win.safetyPageItem();
                const dt = Date.now() - win.svCoolAt;
                if (page.cooling) {
                    // Loose like svSwapped's: the 1 s timer fires late on a loaded machine.
                    if (dt >= 5000) return win.fail("the buttons are still dead " + dt + " ms after a number that followed an answer without one");
                    return win.later("the buttons wake up after a second");
                }
                if (dt < 900) return win.fail("the buttons woke up " + dt + " ms after a number that followed an answer without one");
                // The same number again wakes nothing and sleeps nothing.
                page.arrived(false, true);
                if (page.cooling || !win.safetyButton("confirm").enabled) return win.fail("the same number again put the buttons to sleep");
                // A real Space (one press, one release) on the button the keyboard is on presses it once (X2: the handler of the
                // held key leaves the first press and the real release to the button).
                win.safetyButton("confirm").forceActiveFocus();
                if (win.focusName() !== "safetyButton") return win.fail("the keyboard could not be put on Confirm: '" + win.focusName() + "'");
                win.key(Qt.Key_Space, Qt.NoModifier);
                if (page.pending !== "confirm") return win.fail("a real Space on Confirm did not press it (pending '" + page.pending + "')");
                win.next("svConfirm");
                return;
            }
            // Confirm of the number on screen: the text goes, the level offers Mark (state Default, nothing waits).
            case "svConfirm": {
                const r = win.rowOf(win.svPaused);
                if (!r || ["sent", "delivered", "read"].indexOf(r.receipt) < 0 || win.waits < 3) return win.later("Confirm lets the text go" + (r ? " (receipt " + r.receipt + ")" : ""));
                if (r.wait !== "") return win.fail("the sent text keeps a wait line");
                if (r.pages.some(p => p.safety)) return win.fail("the sent text still offers the safety number");
                if (win.safetyActions() !== "mark") return win.later("the level after Confirm: " + win.safetyActions());
                // Confirm is an approval and its button is gone; Mark is the only one left, so the keyboard waits on the back
                // arrow: one more press must not verify a number nobody compared (V2).
                if (win.focusName() !== "safetyBack") return win.fail("after Confirm the keyboard is on '" + win.focusName() + "', not on the back arrow");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("svConfirmBack");
                return;
            }
            case "svConfirmBack":
                if (win.chat.panel.level !== "person" || win.waits < 3) return win.later("Escape back to her page");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svDefOpen");
                return;
            // A person NEVER verified whose send pauses (critic F2 (4), the DEFAULT branch of BQ-2b; U6): the menu entry
            // opens her number, the level offers Confirm and Mark with "Safety number changed" and her page says so under
            // its row; Confirm (an approval of the number on screen) lets the text go and the level offers Mark only.
            case "svDefOpen":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.openConversation(DEFPEER);
                win.next("svDefChat");
                return;
            case "svDefChat":
                if (App.timeline.conversationId !== DEFPEER || App.timeline.loading || win.waits < 6) return win.later("the chat of the person never verified");
                // First (Y1): her page and the level BEFORE anything is sent. The fake has no key of hers yet and answers
                // "no key yet" to the first ask: a REAL answer without a number (svLate*).
                win.clickHeader();
                win.next("svLatePage");
                return;
            case "svLatePage":
                if (!win.panelReady() || win.waits < 3) return win.later("her page, before she has a key");
                win.chat.panel.showSafety();
                win.next("svLateKeyless");
                return;
            // Y1: the level's REAL answer without a number (the fake's first ask of a key it learns late): the reason, no
            // digits and no button. The number that follows it (below) must be a first sight again: dead for a second.
            case "svLateKeyless": {
                const page = win.safetyPageItem();
                if (win.chat.panel.level !== "safety" || !page.loaded) return win.later("the level's answer without a number");
                if (page.hasNumber || win.safetyActions() !== "" || win.shown("safetyDigits") !== null) return win.fail("the level of a person without a key shows a number or a button (offers '" + win.safetyActions() + "')");
                const reason = win.shown("safetyReason");
                if (!reason || reason.text === "") return win.fail("the level of a person without a key says no reason");
                if (page.numberShown) return win.fail("an answer without a number sets the flag of a number on screen");
                if (win.svLateAt === 0) win.svLateAt = Date.now();
                // A second passes (the buttons of the number below are dead for a second of their own: a flag that a
                // key-less answer set would deaden the KEY-LESS answer and end its second in this wait, so it is waited out).
                if (page.cooling || Date.now() - win.svLateAt < 1200) return win.later("a second with no number on screen");
                win.composerItem.setText("to a person whose key came late");
                // Her first send is refused as untrusted and waits: the level asks again and the fake has her key now.
                win.svCoolAt = Date.now();
                win.composerItem.send();
                win.next("svLateNumber");
                return;
            }
            // The number comes after an answer without one: it is a number that was not on screen just before, so its buttons
            // are dead at the first tick, and a real click on Confirm (an approval of a number nobody has read) acts on
            // nothing.
            case "svLateNumber": {
                if (win.safetyActions() === "") return win.later("the number after the answer without one");
                const page = win.safetyPageItem();
                const first = win.safetyButton("confirm");
                if (!first) return win.fail("the level of a person whose key came late and whose send waits offers: " + win.safetyActions());
                const dt = Date.now() - win.svCoolAt;
                if (dt < 900 && (!page.cooling || first.enabled || win.safetyButton("mark").enabled)) return win.fail("the buttons of a number that came " + dt + " ms after an answer without one are alive");
                if (dt >= 900) console.warn("svLateNumber: the first tick came " + dt + " ms after the send: a busy machine");
                if (dt < 900) win.clickItem(first);
                win.next("svLateQuiet");
                return;
            }
            case "svLateQuiet": {
                if (win.waits < 8) return win.later("a click on the dead Confirm of a late number acts on nothing");
                const page = win.safetyPageItem();
                const waiting = win.newestOwn();
                if (!waiting || !waiting.pages.some(p => p.safety)) return win.fail("a click on Confirm of a number that came after an answer without one confirmed it: the text left");
                if (win.safetyActions() !== "confirm,mark" || page.pending !== "" || page.pressed !== "") return win.fail("a click on the first button of a late number acted (offers '" + win.safetyActions() + "', pending '" + page.pending + "')");
                // (The keyboard waited on the back arrow through both answers: never on a button, W1.)
                if (win.focusName() === "safetyButton") return win.fail("the late number took the keyboard onto a button");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svLateClosed");
                return;
            }
            // The panel is closed, her text still waits: the steps of the Default peer go on from here.
            case "svLateClosed":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.next("svDefPaused");
                return;
            case "svDefPaused": {
                const r = win.newestOwn();
                if (!r || !r.pages.some(p => p.safety)) return win.later("the text to a person never verified waits" + (r ? " (receipt " + r.receipt + ", " + r.pages.length + " page entries)" : ""));
                if (r.receipt !== "pending" && r.receipt !== "failed") return win.fail("the waiting text is " + r.receipt);
                win.svDefault = r.messageId;
                win.next("svDefRow");
                return;
            }
            case "svDefRow":
                if (win.rowWithKeyboard(win.svDefault) === null) return win.later("the keyboard on the waiting text");
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("svDefMenu");
                return;
            case "svDefMenu": {
                const m = win.view.messageMenu;
                if (!m.visible || win.waits < 3) return win.later("the Menu key opens the waiting text's menu");
                if (!win.menuPages(m).some(p => /^Show .+'s Safety Number$/.test(p.text))) return win.fail("the menu of a text to a person never verified offers no safety number");
                win.next("svDefPick");
                return;
            }
            // Two rounds over the same waiting text: the first picks the entry with a REAL click and clicks the first button
            // right behind it (X1, the mouse twin of the second), the second picks it by the keyboard (svDefDouble).
            case "svDefPick": {
                const m = win.view.messageMenu;
                const entry = win.menuPages(m).find(p => /^Show .+'s Safety Number$/.test(p.text));
                if (!m.visible || !entry) return win.fail("the menu closed before its entry was chosen");
                if (!win.svDefMouseDone) {
                    win.svCoolAt = Date.now();
                    win.clickItem(entry);
                    win.next("svDefMouse");
                    return;
                }
                if (win.menuPick(m, entry)) win.next("svDefDouble");
                return;
            }
            // A click on the first button, Confirm (an approval of a number nobody has read), at the first tick it exists: it
            // is dead and acts on nothing, so the text still waits (X1).
            case "svDefMouse": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "safety" || !panel.loaded || win.safetyActions() === "") return win.later("the clicked entry opens the number of a person never verified");
                const page = win.safetyPageItem();
                const first = win.safetyButton("confirm");
                if (!first) return win.fail("the level of a person never verified whose send waits offers: " + win.safetyActions());
                const dt = Date.now() - win.svCoolAt;
                if (dt < 900 && (!page.cooling || first.enabled)) return win.fail("Confirm of a number that appeared " + dt + " ms after a click on the menu's entry is alive");
                if (dt >= 900) console.warn("svDefMouse: the first tick came " + dt + " ms after the click on the entry: a busy machine");
                if (dt < 900) win.clickItem(first);
                win.next("svDefMouseQuiet");
                return;
            }
            case "svDefMouseQuiet": {
                if (win.waits < 8) return win.later("a click on a dead Confirm acts on nothing");
                const page = win.safetyPageItem();
                const waiting = win.rowOf(win.svDefault);
                if (!waiting || !waiting.pages.some(p => p.safety)) return win.fail("a click on Confirm right behind the menu's entry confirmed a number nobody had read: the text left");
                if (win.safetyActions() !== "confirm,mark" || page.pending !== "" || page.pressed !== "") return win.fail("a click on the first button of a number that had just appeared acted (offers '" + win.safetyActions() + "', pending '" + page.pending + "')");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svDefMouseClosed");
                return;
            }
            // The panel closed, the same text still waits: the second round picks the entry by the keyboard.
            case "svDefMouseClosed":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.svDefMouseDone = true;
                win.next("svDefRow");
                return;
            // The menu's entry picked by a Return, and a second Return right behind it, at the first tick that has the answer
            // (W1): here the first button is CONFIRM, an approval of a number nobody has read; the keyboard is on the digits,
            // so the text stays paused (svDefLevel checks that the level still offers Confirm and the text still waits).
            case "svDefDouble": {
                if (!win.chat.panelOpen || win.chat.panel.level !== "safety" || !win.chat.panel.loaded) return win.later("the level of the person never verified");
                const page = win.safetyPageItem();
                if (win.safetyActions() === "" || !page || page.pending !== "") return win.later("the level of the person never verified has its answer and the keyboard");
                if (win.focusName() !== "safetyDigits") return win.fail("the answer put the keyboard on '" + win.focusName() + "', not on the digits");
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.key(Qt.Key_Enter, Qt.NoModifier);
                win.next("svDefLevel");
                return;
            }
            case "svDefLevel": {
                const panel = win.chat.panel;
                if (!win.chat.panelOpen || panel.level !== "safety" || !panel.loaded || win.waits < 8) return win.later("the entry opens the number of a person never verified");
                // (The buttons of a number that has just appeared are dead for a second, X1: the click on Confirm below waits.)
                if (win.safetyPageItem().cooling) return win.later("the buttons of the number of a person never verified wake up");
                if (win.safetyActions() === "mark") return win.fail("a second Return confirmed the number of a person never verified before it was read");
                if (win.safetyActions() !== "confirm,mark") return win.later("a person never verified whose send waits offers: " + win.safetyActions());
                const line = win.shown("safetyState");
                if (!line || line.text !== "Safety number changed") return win.fail("the level does not say that the number changed");
                if (win.focusName() !== "safetyDigits") return win.fail("the keyboard is on '" + win.focusName() + "', not on the digits");
                const waiting = win.rowOf(win.svDefault);
                if (!waiting || !waiting.pages.some(p => p.safety)) return win.fail("a second Return confirmed a number nobody had read: the text left");
                // Her page says it under the row, from the wait the level gets (U7), and the button describes it (U9).
                const under = win.inPanel("detailsSafetyState");
                if (!under || under.text !== "Safety number changed") return win.later("her page's line under the button says the number changed");
                if (win.inPanel("detailsSafety").Accessible.description !== "Safety number changed") return win.fail("her page's button does not describe it");
                // Confirm is clicked at its TOP edge: the first click of a double click (Y2).
                const confirm = win.safetyButton("confirm");
                win.svConfirmPoint = confirm.mapToItem(null, confirm.width / 2, 2);
                win.clickWindow(win.svConfirmPoint.x, win.svConfirmPoint.y);
                win.next("svDefApproved");
                return;
            }
            // The approval's answer drops Confirm AND the state line, so Mark moves up under the pointer, where the second
            // click of a double click on Confirm lands (Y2, the pointer twin of V2): at the first tick that shows Mark alone
            // the buttons are dead again for a second, a second real click at the same point and a real click on Mark
            // itself act on nothing, and the number is not marked.
            case "svDefApproved": {
                if (win.safetyActions() !== "mark") return win.later("the approval's answer drops Confirm");
                const page = win.safetyPageItem();
                const mark = win.safetyButton("mark");
                if (!page.cooling || mark.enabled) return win.fail("Mark that replaced an approved Confirm is alive (cooling " + page.cooling + ")");
                win.clickWindow(win.svConfirmPoint.x, win.svConfirmPoint.y);
                win.clickItem(mark);
                win.next("svDefConfirm");
                return;
            }
            case "svDefConfirm": {
                // Nothing marked the number behind the approval (the clicks above): Mark is still what the level offers.
                if (win.safetyActions() === "clear") return win.fail("a click right behind an approval marked the number");
                const r = win.rowOf(win.svDefault);
                if (!r || ["sent", "delivered", "read"].indexOf(r.receipt) < 0 || win.waits < 3) return win.later("Confirm lets the text go" + (r ? " (receipt " + r.receipt + ")" : ""));
                if (r.wait !== "" || r.pages.some(p => p.safety)) return win.fail("the sent text keeps its line or its safety entry");
                if (win.safetyActions() !== "mark") return win.later("the level after Confirm: " + win.safetyActions());
                if (win.shown("safetyState") !== null) return win.fail("a state line after the approval");
                if (win.focusName() !== "safetyBack") return win.fail("after Confirm the keyboard is on '" + win.focusName() + "', not on the back arrow (V2)");
                const under = win.inPanel("detailsSafetyState");
                if (under && under.text !== "") return win.later("her page's line goes with the wait");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("svDefBack");
                return;
            }
            case "svDefBack":
                if (win.chat.panel.level !== "person" || win.waits < 3) return win.later("Escape back to her page");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("svDone");
                return;
            case "svDone":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.next("lkOpen");
                return;
            // ---- P5b: Copy Link (right under View Safety Number); the URL never passes through QML ----
            case "lkOpen":
                win.openConversation(PERSON);
                win.next("lkHeader");
                return;
            case "lkHeader":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("her chat, for her link");
                win.clickHeader();
                win.next("lkCopy");
                return;
            case "lkCopy": {
                if (!win.panelReady() || win.waits < 3 || !win.shown("detailsLink")) return win.later("her page with Copy Link");
                const link = win.shown("detailsLink");
                if (!link.enabled) return win.fail("Copy Link is disabled for a person who shares her number");
                if (link.Accessible.name !== "Copy Link" || link.Accessible.description !== "") return win.fail("Copy Link's accessible name or description is wrong");
                if (win.shown("detailsLinkNote") !== null) return win.fail("a reason line under an enabled Copy Link");
                // His answer (2): right under "View Safety Number" (nothing between the two parts).
                const safety = win.inPanel("detailsSafetyPart");
                const part = win.inPanel("detailsLinkPart");
                if (part.y < safety.y + safety.height || part.y > safety.y + safety.height + Kirigami.Units.largeSpacing * 2) return win.fail("Copy Link does not stand right under View Safety Number");
                // The page's map has no link: Rust holds it (privacy rule).
                if (JSON.stringify(App.person).indexOf("signal.me") >= 0) return win.fail("the link reached QML's map of the page");
                if (App.person.number === "") return win.fail("the fixture person shows no number to build the expected link from");
                const page = win.find(win.chat.panel, c => c.linksCopied !== undefined && c.visible);
                win.lkCopies = page.linksCopied;
                // The keyboard's way: Return on the button.
                link.forceActiveFocus();
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("lkCopied");
                return;
            }
            case "lkCopied": {
                const page = win.find(win.chat.panel, c => c.linksCopied !== undefined && c.visible);
                if (page.linksCopied !== win.lkCopies + 1) return win.later("Return on Copy Link copies the link");
                // What the clipboard holds, read back by a paste into an editor of the test's own (compared, never printed).
                const edit = Qt.createQmlObject("import QtQuick; TextEdit { visible: false }", win, "lkPaste");
                edit.paste();
                win.lkPasted = edit.text;
                edit.destroy();
                if (win.lkPasted !== "https://signal.me/#p/" + App.person.number) return win.fail("the clipboard does not hold her link (" + win.lkPasted.length + " characters)");
                // The toast "Link copied", with no number in it.
                const toasts = win.findAll(win.contentItem, c => c.text === "Link copied" && c.visible).concat(win.findAll(win.overlay, c => c.text === "Link copied" && c.visible));
                if (toasts.length === 0) return win.later("the toast Link copied");
                if (win.focusName() !== "detailsLink") return win.fail("the keyboard left Copy Link after the copy (" + win.focusName() + ")");
                // Z4: a screen reader hears the same words, politely (the toast is not announced by Kirigami).
                if (page.linkAnnounced !== "Link copied") return win.fail("a screen reader was not told 'Link copied' (told " + page.linkAnnounced.length + " characters)");
                if (Accessible.Polite === undefined || page.linkAnnouncedPoliteness !== Accessible.Polite) return win.fail("'Link copied' was not announced politely (" + page.linkAnnouncedPoliteness + ")");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("lkHiddenOpen");
                return;
            }
            // A person whose number Kourier may not use (the never-verified person of the safety steps: no number was
            // ever said to be shown): the button is disabled, the quiet line says why, and a click copies nothing.
            case "lkHiddenOpen":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.openConversation(DEFPEER);
                win.next("lkHiddenHeader");
                return;
            case "lkHiddenHeader":
                if (App.timeline.conversationId !== DEFPEER || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("the chat of a person without a usable number");
                win.clickHeader();
                win.next("lkHidden");
                return;
            case "lkHidden": {
                if (!win.panelReady() || win.waits < 3 || !win.shown("detailsLink")) return win.later("her page with Copy Link");
                const link = win.shown("detailsLink");
                // SD11: Copy Link and Share Contact… say the same sentence for her; it stands ONCE, under the pair (the Share
                // part's line), and Copy Link keeps it as its description.
                const note = win.shown("detailsShareNote");
                if (link.enabled) return win.fail("Copy Link is enabled for a person whose number the page may not use");
                if (win.shown("detailsLinkNote") !== null) return win.fail("the same reason stands twice, under Copy Link and under Share Contact…");
                if (!note || note.text === "" || note.text !== App.person.linkNote || note.text.indexOf(App.person.name) < 0) return win.fail("the line under the disabled pair does not say why with her name");
                if (link.Accessible.description !== note.text) return win.fail("the disabled Copy Link does not describe why to a screen reader");
                if (App.person.number !== "") return win.fail("a number is shown for a person whose link is disabled");
                const page = win.find(win.chat.panel, c => c.linksCopied !== undefined && c.visible);
                win.lkCopies = page.linksCopied;
                page.linkAnnounced = "";
                win.clickItem(link);
                win.next("lkHiddenQuiet");
                return;
            }
            case "lkHiddenQuiet": {
                if (win.waits < 3) return win.later("a click on the disabled Copy Link");
                const page = win.find(win.chat.panel, c => c.linksCopied !== undefined && c.visible);
                if (page.linksCopied !== win.lkCopies) return win.fail("a click on the disabled Copy Link copied something");
                if (page.linkAnnounced !== "") return win.fail("a click on the disabled Copy Link was announced as a copy");
                const edit = Qt.createQmlObject("import QtQuick; TextEdit { visible: false }", win, "lkPaste2");
                edit.paste();
                const same = edit.text === win.lkPasted;
                edit.destroy();
                if (!same) return win.fail("the clipboard changed after a click on the disabled Copy Link");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("lkDone");
                return;
            }
            case "lkDone":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.next("shOpen");
                return;
            // ---- P5c: Share Contact… (right under Copy Link); "Pick, then Send" (SC2); Rust holds whose card it is ----
            case "shOpen":
                win.openConversation(PERSON);
                win.next("shHeader");
                return;
            case "shHeader":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("her chat, for her card");
                win.clickHeader();
                win.next("shButton");
                return;
            case "shButton": {
                if (!win.panelReady() || win.waits < 3 || !win.shown("detailsShare")) return win.later("her page with Share Contact");
                const share = win.shown("detailsShare");
                if (!share.enabled) return win.fail("Share Contact is disabled for a person with a profile name and a shown number");
                if (share.Accessible.name !== "Share Contact…" || share.Accessible.description !== "") return win.fail("Share Contact's accessible name or description is wrong");
                if (win.shown("detailsShareNote") !== null) return win.fail("a reason line under an enabled Share Contact");
                // His answer (2): right under "Copy Link" (nothing between the two parts).
                const link = win.inPanel("detailsLinkPart");
                const part = win.inPanel("detailsSharePart");
                if (part.y < link.y + link.height || part.y > link.y + link.height + Kirigami.Units.largeSpacing * 2) return win.fail("Share Contact does not stand right under Copy Link");
                if (win.sharePicker() !== null) return win.fail("a picker is open before Share Contact was pressed");
                win.shCards = win.personPageItem().cardsSent;
                // SE1: a held Return acts once. `TestEvent` has no auto-repeat, so the button's own handler is handed a
                // stand-in event with `isAutoRepeat`: it is taken and the picker does not open (the keyboard returns to
                // this button after a send, and a repeat of the key would press it again).
                const held = { isAutoRepeat: true, accepted: false };
                share.activateKey(held);
                if (!held.accepted) return win.fail("the auto-repeat of Return was not taken by Share Contact");
                if (win.sharePicker() !== null) return win.fail("an auto-repeat of Return opened the picker");
                // The keyboard's way: Return on the button.
                share.forceActiveFocus();
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("shOpened");
                return;
            }
            case "shOpened": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3) return win.later("Return on Share Contact opens the picker");
                if (d.title !== "Send " + App.person.name + "'s Contact Card") return win.fail("the picker's title does not name her card");
                if (win.focusName() !== "shareSearch") return win.fail("the picker put the keyboard on '" + win.focusName() + "', not on its search");
                const list = win.inPicker("shareList");
                if (!list || list.count === 0 || list.count !== App.shareTargets.count) return win.later("the picker's rows");
                if (list.currentIndex !== -1) return win.fail("a row is chosen before the user chose one");
                // Note to Self is listed (a card to oneself is a way to keep it); her chat with media is the target below.
                if (!App.shareTargets.isListed(NOTE)) return win.fail("Note to Self is not listed");
                if (!App.shareTargets.isListed(MEDIA)) return win.fail("the chat with media is not listed");
                const send = win.pickerButton("Send");
                if (!send || send.enabled) return win.fail("Send is not there, or enabled before a row is chosen");
                win.shTitle = App.shareTargets.titleOf(MEDIA);
                if (win.shTitle === "") return win.fail("the target chat has no title in the picker");
                // SD9: ONE quiet line right under the title says what the card holds: her PROFILE name and the ONE number, the
                // two values the core puts into the card (`person_card_parts`, the same ones the sent cards are compared with
                // after the window), worded by the viewmodel (`App.person.shareCard`). Never printed.
                const card = win.inPicker("shareCard");
                const wantLine = "The card holds " + CARD_NAME + " and " + CARD_NUMBER + ".";
                if (!card || !card.visible || card.text !== wantLine) return win.fail("the picker has no line saying what the card holds, or it is not the core's card");
                if (App.person.shareCard !== wantLine) return win.fail("the page's card line is not the line of the picker");
                if (card.textFormat !== Text.PlainText || card.wrapMode !== Text.Wrap || card.Accessible.name !== wantLine) return win.fail("the card line is not a wrapping plain label with its accessible name");
                const searchField = win.inPicker("shareSearch");
                if (card.mapToItem(null, 0, 0).y < win.inPicker("shareTitle").mapToItem(null, 0, win.inPicker("shareTitle").height).y - 1 || card.mapToItem(null, 0, card.height).y > searchField.mapToItem(null, 0, 0).y) return win.fail("the card line does not stand between the title and the search");
                if (card.height < card.contentHeight - 1) return win.fail("the card line is shorter than its text: the height does not follow the content");
                if (Style.contrast(card.color, card.Kirigami.Theme.backgroundColor) < Style.minContrast) return win.fail("the card line is too faint to read");
                // SD9: the title is a label that wraps (Kirigami's own header is one line that elides: a long name, or a
                // large font, cut the NAME of the person whose number goes out). A long name takes more lines, nothing elides.
                const title = win.inPicker("shareTitle");
                if (!title || title.wrapMode !== Text.Wrap || title.text !== d.title) return win.fail("the picker's title is not a wrapping label of its title");
                win.shName = d.personName;
                d.personName = "Aleksandrina Konstantina Wilhelmina Featherstonehaugh-Cholmondeley of the Seven Hills";
                win.next("shLongTitle");
                return;
            }
            case "shLongTitle": {
                const d = win.sharePicker();
                const title = win.inPicker("shareTitle");
                if (!d || !title || win.waits < 2) return win.later("the long title");
                if (title.text.indexOf("Featherstonehaugh-Cholmondeley of the Seven Hills") < 0) return win.fail("the long name is not in the title");
                if (title.lineCount < 2 || title.truncated) return win.fail("a long name does not wrap in the title (" + title.lineCount + " lines, truncated " + title.truncated + ")");
                if (title.contentHeight > title.height + 1) return win.fail("the wrapped title is taller than its label: the lines are cut");
                d.personName = win.shName;
                // A picker with no chat to offer and no search says why (SD11): the model emptied with the search field empty.
                App.shareTargets.filter = "zzqx no chat";
                win.next("shEmpty");
                return;
            }
            case "shEmpty": {
                const none = win.inPicker("shareNone");
                const empty = win.inPicker("shareEmpty");
                if (App.shareTargets.count !== 0 || win.waits < 2) return win.later("the emptied rows");
                if (win.inPicker("shareSearch").text !== "") return win.fail("the search field has text in the empty-picker step");
                if (none) return win.fail("the no-match line shows for an empty search field");
                if (!empty || empty.text !== "No chat of this account can take a message.") return win.fail("an empty picker with no search does not say why");
                App.shareTargets.filter = "";
                // SD11: Note to Self's row draws the account's own picture (`selfChat`), as in the chat list: the row alone.
                win.inPicker("shareSearch").text = App.shareTargets.titleOf(NOTE);
                win.next("shSelf");
                return;
            }
            case "shSelf": {
                const list = win.inPicker("shareList");
                const row = list ? win.find(list, c => c.objectName === "shareRow" && c.isSelf !== undefined) : null;
                if (!row || win.waits < 2 || App.shareTargets.count < 1) return win.later("Note to Self's row");
                const avatar = win.find(row, c => c.selfChat !== undefined);
                if (!row.isSelf || !avatar || !avatar.selfChat) return win.fail("Note to Self's row does not draw the own picture");
                for (const r of win.findAll(list, c => c.objectName === "shareRow" && !c.isSelf)) {
                    if (win.find(r, c => c.selfChat === true)) return win.fail("another row draws the own picture");
                }
                // A search that matches nothing says so.
                win.inPicker("shareSearch").text = "zzqx no chat";
                win.next("shNone");
                return;
            }
            case "shNone": {
                const none = win.inPicker("shareNone");
                if (App.shareTargets.count !== 0 || !none) return win.later("a search that matches nothing empties the rows and says so");
                if (none.text !== "No chats match.") return win.fail("the empty search's line is wrong");
                if (win.inPicker("shareEmpty")) return win.fail("the empty-picker line shows beside the no-match line");
                // Escape in a search with text empties it and keeps the picker.
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("shCleared");
                return;
            }
            case "shCleared": {
                const d = win.sharePicker();
                if (!d || !d.opened) return win.fail("Escape in a search with text closed the picker");
                if (win.inPicker("shareSearch").text !== "" || App.shareTargets.count === 0) return win.later("Escape empties the search");
                // The search narrows to the target's title (case does not matter; the filter runs in Rust).
                const typed = win.shTitle.slice(0, Math.min(4, win.shTitle.length)).toUpperCase();
                win.inPicker("shareSearch").text = typed;
                win.next("shSearch");
                return;
            }
            case "shSearch": {
                const typed = win.inPicker("shareSearch").text.toLowerCase();
                if (win.waits < 2 || win.shareIndex(MEDIA) < 0) return win.later("the search lists the target");
                for (let i = 0; i < App.shareTargets.count; ++i) {
                    const t = App.shareTargets.titleOf(App.shareTargets.conversationAt(i));
                    if (t.toLowerCase().indexOf(typed) < 0) return win.fail("row " + i + " does not match the search");
                }
                // Down from the field goes to the rows (the first one current), Up on the first row comes back.
                win.key(Qt.Key_Down, Qt.NoModifier);
                const list = win.inPicker("shareList");
                if (!list.activeFocus || list.currentIndex !== 0) return win.fail("Down from the search does not put the keyboard on the first row");
                win.key(Qt.Key_Up, Qt.NoModifier);
                if (win.focusName() !== "shareSearch") return win.fail("Up on the first row does not return to the search");
                // Typing drops the choice (a row that moved is never sent to): the search emptied, nothing is chosen.
                win.inPicker("shareSearch").text = "";
                if (list.currentIndex !== -1) return win.fail("a changed search kept a chosen row");
                win.next("shDouble");
                return;
            }
            // SC2: a DOUBLE click on a row only chooses it; nothing is sent.
            case "shDouble": {
                if (win.waits < 2) return win.later("the rows after the search");
                const at = win.shareIndex(MEDIA);
                const list = win.inPicker("shareList");
                list.positionViewAtIndex(at, ListView.Center);
                const row = win.find(list, c => c.objectName === "shareRow" && c.index === at);
                if (!row) return win.later("the target's row in view");
                const p = row.mapToItem(win.contentItem, row.width / 2, row.height / 2);
                if (!win.ev.mouseDoubleClickSequence(win.contentItem, p.x, p.y, Qt.LeftButton, Qt.NoModifier, -1)) return win.fail("the window took no double click");
                win.next("shDoubled");
                return;
            }
            case "shDoubled": {
                if (win.waits < 8) return win.later("a double click on a row");
                const d = win.sharePicker();
                if (!d || !d.opened || d.sends !== 0 || win.personPageItem().cardsSent !== win.shCards) return win.fail("a double click on a row sent the card (Pick, then Send)");
                const list = win.inPicker("shareList");
                if (list.currentIndex !== win.shareIndex(MEDIA)) return win.fail("the double click did not choose the target row");
                if (!list.activeFocus) return win.fail("the chosen row does not have the keyboard");
                // SD8: the chosen row is the one cue of WHERE the card goes: a screen reader is told which row is chosen, and
                // its name takes the theme's selection text colour (the style fills the row, a custom label stays in the normal
                // colour: white on yellow in the high-contrast dark scheme). No other row is selected or recoloured.
                const rows = win.findAll(list, c => c.objectName === "shareRow");
                const chosen = rows.find(r => r.index === list.currentIndex);
                if (!chosen) return win.later("the chosen row in view");
                if (chosen.Accessible.role !== Accessible.ListItem || !chosen.Accessible.selected) return win.fail("the chosen row is not told to a screen reader as a selected list item");
                const nameOf = r => win.find(r, c => c.objectName === "shareRowTitle");
                if (!Qt.colorEqual(nameOf(chosen).color, nameOf(chosen).Kirigami.Theme.highlightedTextColor)) return win.fail("the chosen row's name is not in the selection's text colour");
                for (const r of rows) {
                    if (r !== chosen && (r.Accessible.selected || !Qt.colorEqual(nameOf(r).color, nameOf(r).Kirigami.Theme.textColor))) return win.fail("row " + r.index + " looks chosen though it is not");
                }
                const send = win.pickerButton("Send");
                if (!send || !send.enabled) return win.fail("Send stays disabled with a row chosen");
                // The deliberate click (SD10): a REAL click on "Send" sends (an empty `onTriggered` ships green otherwise).
                win.clickItem(send);
                win.next("shSent");
                return;
            }
            case "shSent": {
                const page = win.personPageItem();
                if (win.sharePicker() !== null) {
                    if (win.waits > 25) return win.fail("a click on Send sent nothing");
                    return win.later("a click on Send sends and closes the picker");
                }
                if (page.cardsSent !== win.shCards + 1) return win.later("the card goes out");
                // (The toast says "Sending": it appears when the command left, before the core's checks, SD11.)
                const told = "Sending the contact card to " + win.shTitle;
                const toasts = win.findAll(win.contentItem, c => c.text === told && c.visible).concat(win.findAll(win.overlay, c => c.text === told && c.visible));
                if (toasts.length === 0) return win.later("the toast naming the chat");
                if (page.shareAnnounced !== told) return win.fail("a screen reader was not told which chat got the card");
                if (!win.chat.panelOpen || win.chat.panel.level !== "person") return win.fail("the panel did not stay on her page after the send");
                if (win.focusName() !== "detailsShare") return win.fail("after the send the keyboard is on '" + win.focusName() + "', not on Share Contact");
                // Escape: the picker opened again by a click, a search typed, Escape empties it, a second closes.
                win.clickItem(win.shown("detailsShare"));
                win.next("shEscOpen");
                return;
            }
            case "shEscOpen": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3) return win.later("a click on Share Contact opens the picker");
                if (win.inPicker("shareList").currentIndex !== -1) return win.fail("the picker opened again with a row chosen");
                if (win.inPicker("shareSearch").text !== "") return win.fail("the picker opened again with the old search");
                win.inPicker("shareSearch").text = "a";
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("shEsc");
                return;
            }
            case "shEsc": {
                const d = win.sharePicker();
                if (!d || !d.opened) return win.fail("Escape in a search with text closed the picker");
                if (win.inPicker("shareSearch").text !== "") return win.later("Escape empties the search");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("shEscClosed");
                return;
            }
            case "shEscClosed": {
                if (win.sharePicker() !== null) return win.later("a second Escape closes the picker");
                if (win.personPageItem().cardsSent !== win.shCards + 1) return win.fail("closing the picker sent a card");
                if (!win.chat.panelOpen) return win.fail("Escape in the picker closed the panel too");
                if (win.focusName() !== "detailsShare" || win.waits < 3) return win.later("the keyboard back on Share Contact (" + win.focusName() + ")");
                // SD10: "Cancel" by a real click: a row chosen (Send enabled), the picker closes, nothing is sent, the
                // keyboard is back on Share Contact.
                win.clickItem(win.shown("detailsShare"));
                win.next("shCancel");
                return;
            }
            case "shCancel": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3 || win.focusName() !== "shareSearch") return win.later("the picker, opened again for Cancel");
                win.key(Qt.Key_Down, Qt.NoModifier);
                const send = win.pickerButton("Send");
                const cancel = win.pickerButton("Cancel");
                if (win.inPicker("shareList").currentIndex < 0 || !send || !send.enabled) return win.fail("Down from the search did not choose a row for the Cancel step");
                if (!cancel) return win.fail("the picker has no Cancel button");
                win.clickItem(cancel);
                win.next("shCancelled");
                return;
            }
            case "shCancelled": {
                if (win.sharePicker() !== null) return win.later("Cancel closes the picker");
                if (win.personPageItem().cardsSent !== win.shCards + 1) return win.fail("Cancel sent a card");
                if (!win.chat.panelOpen) return win.fail("Cancel closed the panel too");
                if (win.focusName() !== "detailsShare" || win.waits < 3) return win.later("the keyboard back on Share Contact after Cancel (" + win.focusName() + ")");
                win.clickItem(win.shown("detailsShare"));
                win.next("shTabEsc");
                return;
            }
            // SD7: Escape closes the picker from every control, not only from the search: Tab to "Cancel", Escape.
            case "shTabEsc": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3 || win.focusName() !== "shareSearch") return win.later("the picker, opened again for Escape on Cancel");
                // search -> list -> (no row is chosen: no Send) -> Cancel.
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "shareList") return win.fail("Tab from the search went to '" + win.focusName() + "', not to the list");
                win.key(Qt.Key_Tab, Qt.NoModifier);
                const f = win.activeFocusItem;
                if (!f || f.text !== "Cancel") return win.fail("Tab from the list did not reach Cancel ('" + win.focusName() + "')");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("shTabEscClosed");
                return;
            }
            case "shTabEscClosed": {
                if (win.sharePicker() !== null) {
                    if (win.waits > 25) return win.fail("Escape on Cancel did not close the picker");
                    return win.later("Escape on Cancel closes the picker");
                }
                if (win.personPageItem().cardsSent !== win.shCards + 1) return win.fail("Escape on Cancel sent a card");
                if (!win.chat.panelOpen) return win.fail("Escape in the picker closed the panel too");
                if (win.focusName() !== "detailsShare" || win.waits < 3) return win.later("the keyboard back on Share Contact after Escape on Cancel (" + win.focusName() + ")");
                win.clickItem(win.shown("detailsShare"));
                win.next("shReturn");
                return;
            }
            // SC2's other deliberate key: Return on the chosen row sends (a second card into the chat with media).
            case "shReturn": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3 || win.focusName() !== "shareSearch") return win.later("the picker, opened again for Return");
                win.inPicker("shareSearch").text = win.shTitle.slice(0, Math.min(4, win.shTitle.length)).toUpperCase();
                win.next("shReturnChoose");
                return;
            }
            case "shReturnChoose": {
                if (win.waits < 2 || win.shareIndex(MEDIA) < 0) return win.later("the search lists the target");
                // Down puts the first row current; further Downs walk to the target's row.
                win.key(Qt.Key_Down, Qt.NoModifier);
                const list = win.inPicker("shareList");
                for (let i = list.currentIndex; i < win.shareIndex(MEDIA); ++i) win.key(Qt.Key_Down, Qt.NoModifier);
                if (list.currentIndex < 0 || App.shareTargets.conversationAt(list.currentIndex) !== MEDIA) return win.fail("the keyboard did not reach the target chat's row");
                // SE1: the auto-repeat of Return on the chosen row sends nothing (a stand-in event, as for the button).
                const held = { isAutoRepeat: true, accepted: false };
                list.activateKey(held);
                const d = win.sharePicker();
                if (!held.accepted) return win.fail("the auto-repeat of Return was not taken by the picker's list");
                if (!d || d.sends !== 0 || win.personPageItem().cardsSent !== win.shCards + 1) return win.fail("an auto-repeat of Return on the chosen row sent the card");
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("shReturnSent");
                return;
            }
            case "shReturnSent": {
                if (win.sharePicker() !== null) {
                    if (win.waits > 25) return win.fail("Return on the chosen row sent nothing");
                    return win.later("Return on the chosen row sends and closes the picker");
                }
                if (win.personPageItem().cardsSent !== win.shCards + 2) return win.later("the second card goes out");
                if (win.focusName() !== "detailsShare" || win.waits < 3) return win.later("the keyboard back on Share Contact after Return (" + win.focusName() + ")");
                // SE2: the picker belongs to the person and the account it opened for.
                win.clickItem(win.shown("detailsShare"));
                win.next("shSwapOpen");
                return;
            }
            // SE2: a page answer for ANOTHER person (or account) while the picker is open closes it (its title was copied at
            // the open, and Rust sends the card of whoever the page holds). A refresh of the same person keeps it. The
            // answers are put into `App.person` by script (the property is writable): a real swap goes through `show_person`,
            // which empties the page first and so closes the picker by the old rule alone, and this is the check that stands
            // when an answer comes for another person without that.
            case "shSwapOpen": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3) return win.later("the picker, opened again for a page swap");
                win.shPerson = Object.assign({}, App.person);
                if (win.shPerson.user === undefined || win.shPerson.account === undefined || win.shPerson.name === undefined) return win.fail("the page has no user, account or name to swap");
                // (The refresh also carries no card line: the picker stays, its line goes; the next step puts it back.)
                App.person = Object.assign({}, win.shPerson, { "about": win.shPerson.about + " (refreshed)", "shareCard": "" });
                win.next("shSwapLine");
                return;
            }
            case "shSwapLine": {
                if (win.waits < 6) return win.later("a refresh without a card line");
                const d = win.sharePicker();
                if (!d || !d.opened) return win.fail("an answer for the same person closed the picker");
                // (`inPicker` finds only what is visible: a hidden line is null.)
                if (win.inPicker("shareCard") !== null || d.cardLine !== "") return win.fail("the card line stays under a page without one (visible only when it is not empty)");
                App.person = win.shPerson;
                win.next("shSwapSame");
                return;
            }
            case "shSwapSame": {
                if (win.waits < 6) return win.later("a refresh of the same person");
                const d = win.sharePicker();
                if (!d || !d.opened) return win.fail("an answer for the same person closed the picker");
                const card = win.inPicker("shareCard");
                if (!card || !card.visible || card.text !== App.person.shareCard || card.text === "") return win.fail("the card line does not follow the page of the same person");
                App.person = Object.assign({}, win.shPerson, { "user": win.shPerson.user + "x" });
                win.next("shSwapUser");
                return;
            }
            case "shSwapUser": {
                if (win.sharePicker() !== null) {
                    if (win.waits > 25) return win.fail("the page of another person did not close the picker");
                    return win.later("the picker closes under the page of another person");
                }
                App.person = win.shPerson;
                if (win.waits < 3) return win.later("the page of the person is back");
                win.clickItem(win.shown("detailsShare"));
                win.next("shSwapAccountOpen");
                return;
            }
            case "shSwapAccountOpen": {
                const d = win.sharePicker();
                if (!d || !d.opened || win.waits < 3) return win.later("the picker, opened again for an account swap");
                App.person = Object.assign({}, win.shPerson, { "account": win.shPerson.account + 1 });
                win.next("shSwapAccount");
                return;
            }
            case "shSwapAccount": {
                if (win.sharePicker() !== null) {
                    if (win.waits > 25) return win.fail("the page of another account did not close the picker");
                    return win.later("the picker closes under the page of another account");
                }
                App.person = win.shPerson;
                if (win.personPageItem().cardsSent !== win.shCards + 2) return win.fail("a page swap sent a card");
                if (win.waits < 3) return win.later("the page of the person is back");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("shHiddenOpen");
                return;
            }
            // A person whose number the page may not show (DEFPEER, never said): disabled, the reason, no picker.
            case "shHiddenOpen":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.openConversation(DEFPEER);
                win.next("shHiddenHeader");
                return;
            case "shHiddenHeader":
                if (App.timeline.conversationId !== DEFPEER || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("the chat of a person without a usable number");
                win.clickHeader();
                win.next("shHidden");
                return;
            case "shHidden": {
                if (!win.panelReady() || win.waits < 3 || !win.shown("detailsShare")) return win.later("her page with Share Contact");
                const share = win.shown("detailsShare");
                const note = win.shown("detailsShareNote");
                if (share.enabled) return win.fail("Share Contact is enabled for a person whose number the page may not show");
                if (!note || note.text === "" || note.text !== App.person.shareNote || note.text.indexOf(App.person.name) < 0) return win.fail("the line under the disabled Share Contact does not say why with her name");
                // SD11: the one line stands under the pair, below Share Contact (Copy Link's own line is not shown).
                if (win.shown("detailsLinkNote") !== null || App.person.linkNote !== note.text) return win.fail("the same reason stands twice, or the two buttons say different things");
                const noteTop = note.mapToItem(null, 0, 0).y;
                if (noteTop < share.mapToItem(null, 0, share.height).y || share.mapToItem(null, 0, 0).y < win.shown("detailsLink").mapToItem(null, 0, win.shown("detailsLink").height).y) return win.fail("the one reason does not stand under the pair");
                if (share.Accessible.description !== note.text) return win.fail("the disabled Share Contact does not describe why to a screen reader");
                win.clickItem(share);
                win.next("shHiddenQuiet");
                return;
            }
            case "shHiddenQuiet":
                if (win.waits < 4) return win.later("a click on the disabled Share Contact");
                if (win.sharePicker() !== null) return win.fail("a click on the disabled Share Contact opened the picker");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("shDone");
                return;
            case "shDone":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.next("pjOpen");
                return;
            // The person page's scroll bar SETTLES on a page that just fits (the P5c flap): the media squares followed the
            // width the bar leaves, so a page that fitted with the bar was too tall without it, the bar came back, the
            // squares shrank... a flip every frame for ever (the wide `person` pictures: 3 different in 5 renders). The
            // window is made as high as puts the page where it flipped: exactly fitting with the bar, or one pixel too tall
            // without it; then the page must look the same at every tick and frame. This test's style lays its bar OVER the
            // content, where nothing can flip: `pjReserve` does what the app's style does (`org.kde.desktop`'s ScrollView
            // keeps the bar's width beside the content while the bar shows, and its bar shows, one turn of the event loop
            // late, while the content is taller than the view). Without the fix in `MediaSection.squaresWidth` this step
            // fails with two looks.
            case "pjOpen":
                win.pjHeight = win.height;
                win.openConversation(ALBUMS);
                win.next("pjHeader");
                return;
            case "pjHeader":
                if (App.timeline.conversationId !== ALBUMS || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("the albums chat, for a page with two rows of squares");
                win.loadsBefore = win.personLoads;
                win.clickHeader();
                win.next("pjFit");
                return;
            case "pjFit": {
                if (!win.panelReady() || win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || win.waits < 3) return win.later("the person's page of the albums chat");
                if (win.findAll(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible).length !== 6) return win.later("her six squares");
                const scroll = win.shown("detailsScroll");
                if (!scroll || win.waits < 8) return win.later("her page laid out");
                if (scroll.rightPadding !== 0 || scroll.leftPadding !== 0) return win.fail("this style keeps room beside the page's content: the step's stand-in for the app's style would add to it");
                const reserve = Qt.createQmlObject("import QtQuick\nBinding { delayed: true; restoreMode: Binding.RestoreBindingOrValue }", win);
                reserve.target = scroll;
                reserve.property = "rightPadding";
                reserve.value = Qt.binding(() => {
                    const bar = scroll.QQC2.ScrollBar.vertical;
                    return bar.size > 0 && bar.size < 1 ? bar.width : 0;
                });
                win.pjReserve = reserve;
                win.next("pjBand");
                return;
            }
            case "pjBand": {
                const scroll = win.shown("detailsScroll");
                if (!scroll) return win.fail("her page went away");
                win.update();
                if (win.waits < 8) return win.later("her page laid out with the room for the bar");
                const barShown = scroll.rightPadding > 0;
                win.height = Math.round(win.height + scroll.contentHeight - (barShown ? 0 : 1) - scroll.availableHeight);
                win.pjSeen = [];
                win.pjFrames = win.frames;
                win.next("pjWatch");
                return;
            }
            case "pjWatch": {
                const scroll = win.shown("detailsScroll");
                const square = win.find(win.chat.panel, c => c.objectName === "mediaSquare" && c.visible);
                if (!scroll || !square) return win.fail("her page or its squares went away");
                // Three frames for the new height to be laid out (the bar may come or go ONCE), then a look at every tick
                // until both 16 ticks and 8 more frames went by: the old page flipped at every frame. (A settled window
                // draws nothing: every tick asks for a frame.)
                win.update();
                if (win.frames < win.pjFrames + 3) return win.later("her page drawn at the new height");
                const look = scroll.availableWidth + "/" + scroll.contentHeight + "/" + square.width + "/" + scroll.availableHeight;
                win.pjSeen = win.pjSeen.concat([look]);
                if (win.pjSeen.length < 16 || win.frames < win.pjFrames + 11) return win.later("sixteen looks and eight frames of her page");
                const kinds = win.pjSeen.filter((v, i, a) => a.indexOf(v) === i);
                if (kinds.length !== 1) return win.fail("her page's scroll bar never settles (content width/height/square side/view height over " + (win.frames - win.pjFrames - 3) + " frames): " + kinds.join(", "));
                // Room for the bar only beside a page taller than its view.
                if (scroll.rightPadding !== 0 && scroll.contentHeight <= scroll.availableHeight) return win.fail("room for a bar beside a page that fits");
                win.pjReserve.when = false;
                win.pjReserve.destroy();
                win.pjReserve = null;
                win.height = win.pjHeight;
                win.clickItem(win.inPanel("detailsClose"));
                win.next("pjDone");
                return;
            }
            case "pjDone":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.next("tpOpen");
                return;
            // ---- F4: the selector of her page changes what it shows, and only Apply sends (his answer "Apply and
            // Cancel"). Her chat is the open chat here, so its notices are the timeline's rows: the negative checks are
            // ORDERED (the core runs the commands of a chat in order: a stray command sent before the one we wait for
            // would be a second notice by the time the last one is there), not timed.
            case "tpOpen":
                win.openConversation(PERSON);
                win.next("tpHeader");
                return;
            case "tpHeader":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("her chat, for her selector");
                win.loadsBefore = win.personLoads;
                win.clickHeader();
                win.next("tpPage");
                return;
            case "tpPage": {
                if (!win.panelReady() || win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || win.waits < 3) return win.later("her page");
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (!part || !box) return win.fail("her page has no selector");
                if (win.shown("detailsTimerBoxChoice") || win.shown("detailsTimerBoxApplying") || win.shown("detailsTimerBoxFailed")) return win.fail("a choice, an 'Applying' line or a failure on a page at rest");
                if (box.Accessible.description !== "") return win.fail("the combo describes a choice at rest");
                // This line proves NOTHING about the guard: the default style of this test (Fusion) has the wheel off
                // whatever the page says, so a mutant that removes `wheelEnabled: false` from `TimerPart` survives it. The
                // guard is a property of the desktop style's combo, which turns the wheel on (a tick over the combo, while
                // the panel is scrolled with the pointer resting on it, would pick a time); a proof needs a binary that
                // runs `org.kde.desktop` (`transfer_review_ui.rs` has the set-up). A tick only chooses, and a choice sends
                // nothing, so the behaviour is safe twice over.
                if (box.wheelEnabled !== false) return win.fail("a wheel tick over the combo would pick a time");
                win.tpWas = App.person.timerSecs;
                win.tpRows = win.systemRows();
                const at = box.shownIndex;
                if (at < 0) return win.fail("her time is not one of the combo's entries");
                const down = at + 1 < box.entries.length && !box.entries[at + 1].custom;
                const to = down ? at + 1 : at - 1;
                if (to < 0 || box.entries[to].custom) return win.fail("her combo has no neighbouring time");
                win.tpPick = box.entries[to].secs;
                // The combo shows her time (a new list of times once left it on the first entry).
                if (box.currentIndex !== at) return win.fail("the combo shows entry " + box.currentIndex + ", her time is entry " + at);
                // An arrow key on the closed combo fires `activated` in every style: it chooses, it sends nothing.
                box.forceActiveFocus();
                win.key(down ? Qt.Key_Down : Qt.Key_Up, Qt.NoModifier);
                win.next("tpKeyed");
                return;
            }
            case "tpKeyed": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (!part.hasPending) return win.later("the choice the arrow key made (the keyboard is on '" + win.focusName() + "', the combo shows " + box.currentIndex + " of " + box.entries.length + ", its time is " + part.timerSecs + ", enabled " + box.enabled + ")");
                if (part.pendingSecs !== win.tpPick) return win.fail("the arrow key chose " + part.pendingSecs + ", not the neighbour " + win.tpPick);
                if (box.currentIndex !== box.entries.findIndex(e => !e.custom && e.secs === win.tpPick)) return win.fail("the combo does not show the choice");
                const apply = win.shown("detailsTimerBoxApply");
                const cancel = win.shown("detailsTimerBoxCancel");
                if (!apply || !cancel || apply.width <= 0 || cancel.width <= 0) return win.later("Apply and Cancel under the combo");
                if (apply.mapToItem(null, 0, 0).y < box.mapToItem(null, 0, box.height).y - 1) return win.fail("Apply is not under the combo");
                if (box.Accessible.description === "") return win.fail("the combo says nothing about the choice that waits");
                if (win.waits < 10) return win.later("time for a command to come back, if one went out");
                if (App.person.timerSecs !== win.tpWas || win.systemRows() !== win.tpRows) return win.fail("a choice alone changed her chat's timer");
                win.clickItem(cancel);
                win.next("tpCancelled");
                return;
            }
            // Cancel takes the choice back, the keyboard is on the combo; the ORDERED check: another time is chosen
            // and applied, and when ITS notice is the newest there is exactly one new notice (the cancelled choice
            // would be a second).
            case "tpCancelled": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (part.hasPending) return win.later("Cancel");
                if (win.shown("detailsTimerBoxChoice")) return win.fail("the buttons stay after Cancel");
                if (box.currentIndex !== box.shownIndex || App.person.timerSecs !== win.tpWas) return win.fail("Cancel left the combo on the choice");
                if (!box.activeFocus) return win.fail("the keyboard did not return to the combo after Cancel (" + win.focusName() + ")");
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== win.tpWas && e.secs !== win.tpPick && !win.systemRow(e.label));
                if (at < 0) return win.fail("her combo offers no time her chat has no notice of");
                win.tpOther = box.entries[at].secs;
                win.tpLabel = box.entries[at].label;
                box.activated(at);
                win.next("tpOtherApply");
                return;
            }
            case "tpOtherApply": {
                const part = win.shown("detailsTimer");
                if (!part.hasPending || part.pendingSecs !== win.tpOther) return win.fail("the second choice is not kept");
                const apply = win.shown("detailsTimerBoxApply");
                if (!apply || apply.width <= 0) return win.later("Apply");
                win.clickItem(apply);
                win.next("tpApplied");
                return;
            }
            case "tpApplied": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (App.person.timerSecs !== win.tpOther) return win.later("her chat's new time");
                if (win.newestSystemText().indexOf(win.tpLabel) < 0) return win.later("the notice of her new time, the newest in her chat");
                const fresh = win.systemRows() - win.tpRows;
                if (fresh !== 1) return win.fail(fresh + " timer notices after one Apply: the choice that was cancelled went out");
                if (part.applying || part.hasPending || win.shown("detailsTimerBoxChoice") || win.shown("detailsTimerBoxApplying")) return win.later("the part to rest after Apply");
                if (!box.enabled || box.currentIndex !== box.shownIndex) return win.fail("the combo does not show her new time");
                if (!box.activeFocus) return win.fail("the keyboard is not on the combo after Apply (" + win.focusName() + ")");
                win.next("tpSettle");
                return;
            }
            // M1: the popup of the desktop style opens right where the row of Apply and Cancel appears, so the second click
            // of a double click on an entry would press Apply. A choice made while the popup is open wakes the buttons only
            // after the double click interval: the choice is made as the combo makes it (`activated` while the popup is
            // visible), Apply is off at once, a real click on it sends nothing (the ordered count), and the same click
            // after the interval sends once.
            case "tpSettle": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                win.tpWas = App.person.timerSecs;
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== win.tpWas && !win.systemRow(e.label));
                if (at < 0) return win.fail("her combo offers no time her chat has no notice of");
                win.tpPick = box.entries[at].secs;
                win.tpLabel = box.entries[at].label;
                win.tpRows = win.systemRows();
                box.popup.open();
                if (!box.popup.visible) return win.fail("the combo's popup is not visible after open()");
                box.activated(at);
                // (As the combo does: the popup closes after `activated`.)
                box.popup.close();
                if (!part.hasPending || part.pendingSecs !== win.tpPick) return win.fail("the choice of the popup is not kept");
                const apply = win.shown("detailsTimerBoxApply");
                const cancel = win.shown("detailsTimerBoxCancel");
                if (!apply || !cancel) return win.fail("a choice from the popup shows no Apply and Cancel");
                if (apply.enabled || cancel.enabled) return win.fail("Apply or Cancel is on at once after a choice from the popup: the second click of a double click can press it");
                win.next("tpSettled");
                return;
            }
            case "tpSettled": {
                const part = win.shown("detailsTimer");
                const apply = win.shown("detailsTimerBoxApply");
                if (!apply || apply.width <= 0) return win.later("Apply");
                if (apply.enabled) {
                    // The interval ran out before the click (a stalled machine): choose again, a few times.
                    win.tpTries += 1;
                    if (win.tpTries > 5) return win.fail("Apply woke before the click five times");
                    part.cancel();
                    win.next("tpSettle");
                    return;
                }
                win.clickItem(apply);
                win.next("tpWake");
                return;
            }
            case "tpWake": {
                const part = win.shown("detailsTimer");
                const apply = win.shown("detailsTimerBoxApply");
                if (!part.hasPending || part.applying || !apply) return win.fail("a click on the sleeping Apply sent the time");
                if (!apply.enabled) return win.later("Apply to wake after the double click interval");
                win.clickItem(apply);
                win.next("tpWoke");
                return;
            }
            case "tpWoke": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (App.person.timerSecs !== win.tpPick) return win.later("her chat's time after the woken Apply");
                if (win.newestSystemText().indexOf(win.tpLabel) < 0) return win.later("the notice of the time the woken Apply sent");
                const fresh = win.systemRows() - win.tpRows;
                if (fresh !== 1) return win.fail(fresh + " timer notices for one choice from the popup: the click on the sleeping Apply sent it");
                if (part.applying || part.hasPending || win.shown("detailsTimerBoxChoice")) return win.later("the part to rest");
                if (!box.enabled || !box.activeFocus) return win.fail("the combo is not back after the woken Apply (" + win.focusName() + ")");
                win.next("tpEsc");
                return;
            }
            // Escape on the combo takes a waiting choice back and leaves the panel; the second Escape is the panel's Back.
            case "tpEsc": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                const at = box.entries.findIndex(e => !e.custom && e.secs !== App.person.timerSecs);
                box.activated(at);
                if (!part.hasPending) return win.fail("no choice to take back");
                box.forceActiveFocus();
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("tpEscDone");
                return;
            }
            case "tpEscDone": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (!win.chat.panelOpen || !part || !box) return win.fail("the first Escape closed the panel instead of taking the choice back");
                if (part.hasPending || win.shown("detailsTimerBoxChoice")) return win.fail("Escape did not take the choice back");
                if (!box.activeFocus || win.waits < 3) return win.later("the keyboard on the combo");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("tpEscClosed");
                return;
            }
            case "tpEscClosed":
                if (win.chat.panelOpen) return win.later("the second Escape: the panel's own Back");
                win.loadsBefore = win.personLoads;
                win.clickHeader();
                win.next("tpClose");
                return;
            // The panel closing drops a choice: the same page item serves the next open.
            case "tpClose": {
                if (!win.panelReady() || win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || win.waits < 3) return win.later("her page again");
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                box.activated(box.entries.findIndex(e => !e.custom && e.secs !== App.person.timerSecs));
                if (!part.hasPending) return win.fail("no choice to lose");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("tpClosed");
                return;
            }
            case "tpClosed":
                if (win.chat.panelOpen) return win.later("the X closes her page");
                win.loadsBefore = win.personLoads;
                win.clickHeader();
                win.next("tpBack");
                return;
            case "tpBack": {
                if (!win.panelReady() || win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || win.waits < 3) return win.later("her page a third time");
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (part.hasPending || win.shown("detailsTimerBoxChoice")) return win.fail("a choice outlived the closed panel");
                if (box.currentIndex !== box.shownIndex) return win.fail("the combo shows a choice of the panel that was closed");
                win.next("tpScript");
                return;
            }
            // What the page does under a choice, by script (the pages' own answers set `App.person`): a peer's change
            // keeps it and Apply shows only while it differs, a time the chat has now is nothing to apply, the offered
            // times, the hold and the conversation drop it.
            case "tpScript": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                const page = App.person;
                const others = page.timers.filter(t => t.secs > 0 && t.secs !== page.timerSecs);
                if (others.length < 3) return win.fail("her combo offers too few times for the script");
                const pick = others[0].secs;
                const peer = others[1].secs;
                const choose = secs => box.activated(box.entries.findIndex(e => !e.custom && e.secs === secs));
                const choice = () => win.shown("detailsTimerBoxChoice");
                choose(pick);
                if (!part.hasPending) return win.fail("no choice to keep");
                App.person = Object.assign({}, page, { "timerSecs": peer });
                if (part.timerSecs !== peer) return win.fail("the page did not take the peer's change");
                if (!part.hasPending || part.pendingSecs !== pick || !choice()) return win.fail("a peer's change dropped the choice or its buttons");
                if (box.currentIndex !== box.entries.findIndex(e => !e.custom && e.secs === pick)) return win.fail("a peer's change moved the combo off the choice");
                App.person = Object.assign({}, page, { "timerSecs": pick });
                if (part.hasPending || choice()) return win.fail("Apply stays although the chat has the time that was chosen");
                App.person = Object.assign({}, page, { "timerSecs": peer });
                if (part.hasPending || choice()) return win.fail("a choice that was met came back");
                App.person = page;
                // L8: a keyboard on Apply goes back to the combo when the choice goes by itself (the peer met it).
                choose(pick);
                box.forceActiveFocus();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsTimerBoxApply") return win.fail("Tab after the combo reaches '" + win.focusName() + "', not Apply");
                App.person = Object.assign({}, page, { "timerSecs": pick });
                if (part.hasPending || choice()) return win.fail("Apply stays although the peer met the choice");
                if (!box.activeFocus) return win.fail("the keyboard is left on '" + win.focusName() + "' when the choice went by itself");
                App.person = page;
                // M2: a wait for Apply ends on ANY change of the chat's time (a peer's, or ours dropped by the core's
                // "an older one loses"), not only on ours: the combo is the chat's again, not grey on a time it does not have.
                part.appliedSecs = pick;
                part.applying = true;
                if (box.enabled) return win.fail("the combo is not grey while it waits for the chat");
                App.person = Object.assign({}, page, { "timerSecs": peer });
                if (part.applying || !box.enabled || box.currentIndex !== box.shownIndex) return win.fail("a third value left the combo grey on a time the chat does not have");
                if (box.entries[box.currentIndex].secs !== peer) return win.fail("the combo does not show the third value");
                App.person = page;
                // L6: a failure of the timer (the header's clock menu, say) ends a wait but keeps a choice that waits.
                choose(pick);
                App.person = Object.assign({}, page, { "timerFailed": "Failed for this step." });
                if (!part.hasPending || !choice()) return win.fail("a failure of the chat's timer dropped a choice that waits");
                App.person = page;
                part.cancel();
                choose(pick);
                App.person = Object.assign({}, page, { "timers": page.timers.filter(t => t.secs !== pick) });
                if (part.hasPending || choice()) return win.fail("a choice outlived the offered times");
                App.person = page;
                choose(pick);
                App.timeline.timerHeld = "Held for this step.";
                if (part.hasPending || choice() || box.enabled || !win.shown("detailsTimerHeld")) return win.fail("a choice outlived the hold, or the combo is not grey with its reason");
                App.timeline.timerHeld = "";
                if (!box.enabled || win.shown("detailsTimerHeld")) return win.fail("the combo stays grey after the hold");
                choose(pick);
                const conversation = part.conversation;
                part.conversation = conversation + 1;
                if (part.hasPending || choice()) return win.fail("a choice outlived the conversation");
                part.conversation = Qt.binding(() => App.person.chat ?? 0);
                if (part.conversation !== conversation) return win.fail("the conversation is not hers again");
                win.next("tpKeys");
                return;
            }
            // The keyboard: Tab from the combo reaches Apply, then Cancel; a held key acts once (people.md P5a: the
            // guards take a stand-in event, TestEvent sends no auto-repeat); Space on Apply sends.
            case "tpKeys": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                win.tpWas = App.person.timerSecs;
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== win.tpWas);
                win.tpPick = box.entries[at].secs;
                win.tpLabel = box.entries[at].label;
                box.activated(at);
                box.forceActiveFocus();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsTimerBoxApply") return win.fail("Tab after the combo reaches '" + win.focusName() + "', not Apply");
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() !== "detailsTimerBoxCancel") return win.fail("Tab after Apply reaches '" + win.focusName() + "', not Cancel");
                win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                if (win.focusName() !== "detailsTimerBoxApply") return win.fail("Shift+Tab does not come back to Apply");
                const apply = win.shown("detailsTimerBoxApply");
                const cancel = win.shown("detailsTimerBoxCancel");
                const repeats = [{ "isAutoRepeat": true, "accepted": false, "key": Qt.Key_Return }, { "isAutoRepeat": true, "accepted": false, "key": Qt.Key_Return }];
                apply.activateKey(repeats[0]);
                cancel.activateKey(repeats[1]);
                if (!part.hasPending || !repeats[0].accepted || !repeats[1].accepted) return win.fail("a held Return acted on Apply or Cancel");
                const space = { "isAutoRepeat": true, "accepted": false, "key": Qt.Key_Space };
                const tab = { "isAutoRepeat": true, "accepted": false, "key": Qt.Key_Tab };
                apply.holdKey(space);
                cancel.holdKey(space);
                apply.holdKey(tab);
                if (!space.accepted || tab.accepted) return win.fail("the guard of a held key takes a held Tab or lets a held Space through");
                win.tpRows = win.systemRows();
                win.key(Qt.Key_Space, Qt.NoModifier);
                win.next("tpKeysDone");
                return;
            }
            case "tpKeysDone": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (App.person.timerSecs !== win.tpPick) return win.later("Space on Apply");
                if (win.newestSystemText().indexOf(win.tpLabel) < 0) return win.later("the notice of the time Space applied");
                if (win.systemRows() !== win.tpRows + 1) return win.fail((win.systemRows() - win.tpRows) + " notices for one Space on Apply");
                if (part.applying || part.hasPending || win.shown("detailsTimerBoxChoice")) return win.later("the part to rest");
                if (!box.activeFocus) return win.fail("the keyboard is not on the combo after Space on Apply (" + win.focusName() + ")");
                win.next("tpHold");
                return;
            }
            // After Apply the applied time stays shown, the combo is grey and "Applying…" says so until the chat has
            // that time (a network that confirms only after its ack: WhatsApp, up to 75 s). The core here confirms at
            // once, so the page's value is HELD where it was by script (the part does not hear the confirmation);
            // no viewmodel test holds this state: it is the part's own (a backend that confirms late is not played).
            case "tpHold": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                part.timerSecs = part.timerSecs;
                win.tpWas = part.timerSecs;
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== win.tpWas);
                win.tpPick = box.entries[at].secs;
                win.tpLabel = box.entries[at].label;
                box.activated(at);
                win.next("tpHoldApply");
                return;
            }
            case "tpHoldApply": {
                const apply = win.shown("detailsTimerBoxApply");
                if (!apply || apply.width <= 0) return win.later("Apply");
                win.clickItem(apply);
                win.next("tpHeld");
                return;
            }
            case "tpHeld": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                // The core did confirm (the page it hands over has the time), the held part has not heard.
                if (App.person.timerSecs !== win.tpPick) return win.later("the core's confirmation");
                if (win.waits < 8) return win.later("a few ticks of the held ack");
                if (part.timerSecs !== win.tpWas) return win.fail("the part was not held");
                if (!part.applying || box.enabled) return win.fail("the combo is not grey while the ack is out");
                if (box.currentIndex !== box.entries.findIndex(e => !e.custom && e.secs === win.tpPick) || box.displayText !== win.tpLabel) return win.fail("the combo snapped back to the old time while the ack is out (" + box.displayText + ")");
                const line = win.shown("detailsTimerBoxApplying");
                if (!line || line.text === "") return win.fail("no 'Applying' line while the ack is out");
                if (win.shown("detailsTimerBoxChoice")) return win.fail("Apply and Cancel stand while the ack is out");
                // The change fails (the core refuses a time the chat does not take): the line says why, the combo is
                // the chat's again and the Applying line goes.
                App.setTimerOf(App.person.chat, 2147483647);
                win.next("tpFailed");
                return;
            }
            case "tpFailed": {
                const part = win.shown("detailsTimer");
                const box = win.shown("detailsTimerBox");
                if (App.person.timerFailed === "") return win.later("the failure of the change");
                const line = win.shown("detailsTimerBoxFailed");
                if (!line || line.text !== App.person.timerFailed || line.text === "") return win.later("the failure line under the combo");
                if (part.applying || win.shown("detailsTimerBoxApplying") || !box.enabled) return win.fail("the combo stays applying after the failure");
                if (box.currentIndex !== box.shownIndex || box.currentIndex < 0 || box.entries[box.currentIndex].secs !== part.timerSecs) return win.fail("the combo does not show the page's own time after the failure");
                // L7: a screen reader hears why Apply failed (the keyboard is on the combo).
                if (box.Accessible.description !== line.text) return win.fail("the combo does not describe the failure");
                // The held page hears again; the next change forgets the failure at once.
                part.timerSecs = Qt.binding(() => App.person.timerSecs ?? 0);
                if (part.timerSecs !== win.tpPick) return win.fail("the page did not catch up");
                const at = box.entries.findIndex(e => !e.custom && e.secs > 0 && e.secs !== win.tpPick);
                win.tpOther = box.entries[at].secs;
                box.activated(at);
                win.next("tpAgain");
                return;
            }
            case "tpAgain": {
                const apply = win.shown("detailsTimerBoxApply");
                if (!apply || apply.width <= 0) return win.later("Apply");
                win.clickItem(apply);
                if (App.person.timerFailed !== "" || win.shown("detailsTimerBoxFailed")) return win.fail("the failure outlived the next change");
                win.next("tpDone");
                return;
            }
            case "tpDone": {
                const part = win.shown("detailsTimer");
                if (App.person.timerSecs !== win.tpOther) return win.later("her last time");
                if (part.applying || part.hasPending || win.shown("detailsTimerBoxApplying") || win.shown("detailsTimerBoxFailed")) return win.later("the part to rest");
                win.clickItem(win.inPanel("detailsClose"));
                win.next("tpEnd");
                return;
            }
            case "tpEnd":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.next("dxOpen");
                return;
            // ---- F1: a person's about text and a group's description are selectable, with links in an accepted chat ----
            case "dxOpen":
                win.openConversation(PERSON);
                win.next("dxHeader");
                return;
            case "dxHeader":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.chat.panelOpen || win.waits < 6) return win.later("her chat, for her about text");
                win.loadsBefore = win.personLoads;
                win.clickHeader();
                win.next("dxPage");
                return;
            case "dxPage": {
                if (!win.panelReady() || win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || win.waits < 3) return win.later("her page");
                const about = win.shown("detailsAbout");
                if (!about || about.edit.length === 0 || win.shown("detailsAboutMore")) return win.fail("her short about text is not shown, or it needs a More");
                const edit = about.edit;
                if (edit.getText(0, edit.length) !== App.person.aboutEmoji + " " + App.person.about) return win.fail("the about text on the page is not her emoji and her text (" + edit.length + " characters)");
                if (!edit.readOnly || !edit.selectByMouse || !edit.selectByKeyboard) return win.fail("the about text is not a read-only text that selects by mouse and keyboard");
                if (edit.textFormat !== TextEdit.RichText || edit.horizontalAlignment !== TextEdit.AlignHCenter) return win.fail("the about text is not the viewmodel's HTML, centred");
                if (edit.Accessible.name !== "About text") return win.fail("the about text has no accessible name");
                // Her chat is accepted: the address is a link (the viewmodel's table is a unit test).
                if (!App.person.aboutLinks || App.person.aboutHrefs.length !== 1) return win.fail("her accepted chat's about text has no link");
                win.dxLink = App.person.aboutHrefs[0];
                if (win.dxLink !== "https://example.org/pilot") return win.fail("the link of her about text is another address");
                const at = edit.getText(0, edit.length).indexOf(win.dxLink);
                if (at < 0) return win.fail("the address is not in the text");
                const p = win.pointNear(edit, at + 8, 3);
                const q = about.edit.mapFromItem(null, p.x, p.y);
                if (edit.linkAt(q.x, q.y) !== win.dxLink) return win.fail("the address in the text is not a link");
                // ONE Tab stop: from the X, one Tab reaches the text, the next goes on, Shift+Tab comes back to it.
                win.inPanel("detailsClose").forceActiveFocus();
                win.key(Qt.Key_Tab, Qt.NoModifier);
                win.next("dxTab");
                return;
            }
            case "dxTab": {
                if (win.focusName() !== "detailsAboutText") return win.fail("Tab from the X reached '" + win.focusName() + "', not the about text");
                win.key(Qt.Key_Tab, Qt.NoModifier);
                if (win.focusName() === "detailsAboutText" || win.focusName() === "detailsClose") return win.fail("a second Tab went to '" + win.focusName() + "': the text is not ONE stop");
                win.key(Qt.Key_Backtab, Qt.ShiftModifier);
                if (win.focusName() !== "detailsAboutText") return win.fail("Shift+Tab did not come back to the about text (" + win.focusName() + ")");
                // Shift+arrows select, Ctrl+C copies.
                const edit = win.shown("detailsAbout").edit;
                App.copyText("before");
                edit.cursorPosition = 3;
                for (let i = 0; i < 6; i++) win.key(Qt.Key_Right, Qt.ShiftModifier);
                if (edit.selectedText.length !== 6 || edit.selectedText !== edit.getText(3, 9)) return win.fail("Shift+Right selected " + edit.selectedText.length + " characters, not the first word");
                win.key(Qt.Key_C, Qt.ControlModifier);
                win.next("dxKeyCopied");
                return;
            }
            case "dxKeyCopied": {
                const edit = win.shown("detailsAbout").edit;
                if (win.clipboardText() !== edit.getText(3, 9)) return win.fail("Ctrl+C did not put the selection on the clipboard");
                // A drag selects a word.
                const plain = edit.getText(0, edit.length);
                const at = plain.indexOf("building");
                const a = win.pointNear(edit, at, 1);
                const b = win.pointNear(edit, at + 8, -1);
                const pa = win.contentItem.mapFromItem(null, a.x, a.y);
                const pb = win.contentItem.mapFromItem(null, b.x, b.y);
                win.ev.mousePress(win.contentItem, pa.x, pa.y, Qt.LeftButton, Qt.NoModifier, -1);
                win.ev.mouseMove(win.contentItem, (pa.x + pb.x) / 2, pb.y, -1, Qt.LeftButton, Qt.NoModifier);
                win.ev.mouseMove(win.contentItem, pb.x, pb.y, -1, Qt.LeftButton, Qt.NoModifier);
                win.ev.mouseRelease(win.contentItem, pb.x, pb.y, Qt.LeftButton, Qt.NoModifier, -1);
                win.next("dxDragged");
                return;
            }
            case "dxDragged": {
                const edit = win.shown("detailsAbout").edit;
                if (edit.selectedText !== "building") return win.fail("the drag selected " + edit.selectedText.length + " characters, not the word it crossed");
                // A double click selects the word under it.
                const at = edit.getText(0, edit.length).indexOf("view");
                const p = win.pointNear(edit, at + 2, 0);
                const q = win.contentItem.mapFromItem(null, p.x, p.y);
                win.ev.mouseDoubleClickSequence(win.contentItem, q.x, q.y, Qt.LeftButton, Qt.NoModifier, -1);
                win.next("dxDouble");
                return;
            }
            case "dxDouble": {
                const edit = win.shown("detailsAbout").edit;
                if (edit.selectedText !== "view") return win.fail("a double click selected " + edit.selectedText.length + " characters, not the word");
                // The right button opens the menu on the selected word; the selection stays while it is open.
                const at = edit.getText(0, edit.length).indexOf("view");
                const p = win.pointNear(edit, at + 2, 0);
                win.rightClickWindow(p.x, p.y);
                win.next("dxMenu");
                return;
            }
            case "dxMenu": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu of the about text");
                const copy = win.menuEntry("textCopy");
                const all = win.menuEntry("textSelectAll");
                if (!copy || !all) return win.fail("the menu lacks Copy or Select All");
                if (win.menuEntry("textOpenLink") || win.menuEntry("textCopyLink")) return win.fail("link entries on a word that is no link");
                if (about.edit.selectedText !== "view" || !copy.enabled) return win.fail("the menu took the selection away (" + about.edit.selectedText.length + " characters selected, Copy enabled " + copy.enabled + ")");
                App.copyText("before");
                win.clickItem(copy);
                win.next("dxMenuCopied");
                return;
            }
            case "dxMenuCopied": {
                const about = win.shown("detailsAbout");
                if (about.menu.visible || win.focusName() !== "detailsAboutText") return win.later("the menu to close and give the keyboard back to the text (" + win.focusName() + ")");
                if (win.clipboardText() !== "view") return win.fail("Copy in the menu did not put the selected word on the clipboard");
                // Select All.
                const edit = about.edit;
                const p = win.pointNear(edit, 5, 0);
                win.rightClickWindow(p.x, p.y);
                win.next("dxAllMenu");
                return;
            }
            case "dxAllMenu": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu again");
                const all = win.menuEntry("textSelectAll");
                win.clickItem(all);
                win.next("dxAll");
                return;
            }
            case "dxAll": {
                const about = win.shown("detailsAbout");
                if (about.menu.visible || win.focusName() !== "detailsAboutText") return win.later("the menu to close");
                if (about.edit.selectedText !== about.edit.getText(0, about.edit.length)) return win.fail("Select All selected " + about.edit.selectedText.length + " of " + about.edit.length + " characters");
                // The selection is kept only while the menu is open: with the keyboard elsewhere it is gone.
                win.inPanel("detailsClose").forceActiveFocus();
                win.next("dxBlur");
                return;
            }
            case "dxBlur": {
                const about = win.shown("detailsAbout");
                if (about.edit.selectedText !== "") return win.fail("the selection stays after the keyboard left the text (" + about.edit.selectedText.length + " characters)");
                // A right click on a link: Open Link… and Copy Link Address, and Copy is off with nothing selected.
                // (3 pixels into the glyph, as the other steps click a link: `linkAt` is an exact hit test, and the
                // window's events are whole pixels, so the point at a glyph's edge can fall just outside it.)
                win.next("dxLinkAt");
                return;
            }
            case "dxLinkAt": {
                const about = win.shown("detailsAbout");
                const at = about.edit.getText(0, about.edit.length).indexOf(win.dxLink);
                const p = win.pointNear(about.edit, at + 8, 3);
                win.rightClickWindow(p.x, p.y);
                win.next("dxLinkMenu");
                return;
            }
            case "dxLinkMenu": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu on the link");
                const copy = win.menuEntry("textCopy");
                if (!copy || copy.enabled) return win.fail("Copy is enabled with nothing selected");
                if (about.menu.link !== win.dxLink) return win.fail("the menu's link is not the address under the pointer (" + about.menu.link.length + " characters)");
                const open = win.menuEntry("textOpenLink");
                const copyLink = win.menuEntry("textCopyLink");
                if (!open || !copyLink) return win.fail("the menu on a link lacks Open Link… or Copy Link Address");
                if (open.text !== "Open Link…" || copyLink.text !== "Copy Link Address") return win.fail("the link entries are named '" + open.text.length + "' and '" + copyLink.text.length + "' characters");
                win.clickItem(copyLink);
                win.next("dxLinkCopied");
                return;
            }
            case "dxLinkCopied": {
                const about = win.shown("detailsAbout");
                if (about.menu.visible) return win.later("the menu to close");
                if (win.clipboardText() !== win.dxLink) return win.fail("Copy Link Address did not put exactly the address on the clipboard");
                if (win.linkQuestion(win.dxLink)) return win.fail("Copy Link Address asked the link question");
                // Open Link… asks first.
                const at = about.edit.getText(0, about.edit.length).indexOf(win.dxLink);
                const p = win.pointNear(about.edit, at + 8, 3);
                win.rightClickWindow(p.x, p.y);
                win.next("dxOpenMenu");
                return;
            }
            case "dxOpenMenu": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu on the link again");
                win.clickItem(win.menuEntry("textOpenLink"));
                win.next("dxAsked");
                return;
            }
            case "dxAsked": {
                const about = win.shown("detailsAbout");
                const dialog = about.textBlock.question;
                if (!win.linkQuestion(win.dxLink) || !dialog || !dialog.opened) return win.later("the question before the link, opened");
                // The question's buttons act on what was pressed while it was open, never on a repeat or on a release whose
                // press came before (it opens inside the key that chose Open Link… in the menu). TestEvent sends no
                // auto-repeat, so the filters get stand-in events.
                const repeat = { isAutoRepeat: true, accepted: false, key: Qt.Key_Space };
                dialog.filterPress(repeat);
                if (!repeat.accepted) return win.fail("a repeat of a held key reached the question's buttons");
                const early = { isAutoRepeat: false, accepted: false, key: Qt.Key_Space };
                dialog.filterRelease(early);
                if (!early.accepted) return win.fail("the release of a key pressed before the question opened reached its buttons");
                const press = { isAutoRepeat: false, accepted: false, key: Qt.Key_Space };
                dialog.filterPress(press);
                const release = { isAutoRepeat: false, accepted: false, key: Qt.Key_Space };
                dialog.filterRelease(release);
                if (press.accepted || release.accepted) return win.fail("a key pressed and released while the question is open is refused");
                for (const k of [Qt.Key_Tab, Qt.Key_Backtab]) {
                    const held = { isAutoRepeat: true, accepted: false, key: k };
                    dialog.filterPress(held);
                    if (held.accepted) return win.fail("a held Tab is stopped on the question's button");
                }
                // Cancel starts nothing.
                const cancel = win.linkButton(win.dxLink, "cancel");
                if (!cancel) return win.fail("the question has no Cancel");
                win.clickItem(cancel);
                win.next("dxCancelled");
                return;
            }
            case "dxCancelled": {
                if (win.linkQuestion(win.dxLink)) return win.later("the question to close");
                const about = win.shown("detailsAbout");
                // A left click on the link asks the same question (the hand is over it: `hoveredLink`).
                const at = about.edit.getText(0, about.edit.length).indexOf(win.dxLink);
                const p = win.pointNear(about.edit, at + 8, 3);
                const q = win.contentItem.mapFromItem(null, p.x, p.y);
                win.ev.mouseMove(win.contentItem, q.x, q.y, -1, Qt.NoButton, Qt.NoModifier);
                win.next("dxHover");
                return;
            }
            case "dxHover": {
                const about = win.shown("detailsAbout");
                if (about.edit.hoveredLink !== win.dxLink) return win.later("the pointer on the link hovers it");
                const at = about.edit.getText(0, about.edit.length).indexOf(win.dxLink);
                const p = win.pointNear(about.edit, at + 8, 3);
                win.clickWindow(p.x, p.y);
                win.next("dxClicked");
                return;
            }
            case "dxClicked": {
                const open = win.linkButton(win.dxLink, "open");
                if (!open || !win.shown("detailsAbout").textBlock.question.opened) return win.later("the question before the clicked link");
                // Open hands the address to the launcher (the test's recorder starts nothing; Rust compares it at the end).
                win.clickItem(open);
                win.next("dxOpened");
                return;
            }
            case "dxOpened": {
                if (win.linkQuestion(win.dxLink)) return win.later("the question to close");
                // The Menu key opens the menu at the caret; with the caret on the link it offers that link.
                const about = win.shown("detailsAbout");
                const at = about.edit.getText(0, about.edit.length).indexOf(win.dxLink);
                about.edit.forceActiveFocus();
                about.edit.cursorPosition = at + 10;
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("dxKeyMenu");
                return;
            }
            case "dxKeyMenu": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu opened by the Menu key");
                if (about.menu.link !== win.dxLink || !win.menuEntry("textOpenLink") || !win.menuEntry("textCopyLink")) return win.fail("the Menu key with the caret on the link did not offer the link");
                // Escape closes it and the keyboard is back on the text.
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("dxKeyMenuClosed");
                return;
            }
            case "dxKeyMenuClosed": {
                const about = win.shown("detailsAbout");
                if (about.menu.visible || win.focusName() !== "detailsAboutText") return win.later("Escape to close the menu and give the keyboard back (" + win.focusName() + ")");
                // Shift+F10 is the same key; Down, Down, Return chooses Open Link… (Copy is off with nothing selected).
                win.key(Qt.Key_F10, Qt.ShiftModifier);
                win.next("dxKeyMenuAgain");
                return;
            }
            case "dxKeyMenuAgain": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu opened by Shift+F10");
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.key(Qt.Key_Down, Qt.NoModifier);
                win.key(Qt.Key_Return, Qt.NoModifier);
                win.next("dxKeyAsked");
                return;
            }
            case "dxKeyAsked": {
                const about = win.shown("detailsAbout");
                const dialog = about.textBlock.question;
                if (!win.linkQuestion(win.dxLink) || !dialog || !dialog.opened) return win.later("the question opened by the keyboard");
                // It stays: the key that chose the entry does not answer it.
                if (win.waits < 8) return win.later("the question to stay open after the key");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("dxKeyClosed");
                return;
            }
            case "dxKeyClosed": {
                if (win.linkQuestion(win.dxLink)) return win.later("Escape to close the question");
                // The question's close gave the keyboard back to the text (the Connections of DescriptionText), unforced.
                if (win.focusName() !== "detailsAboutText") {
                    if (win.waits < 8) return win.later("the keyboard back on the about text");
                    return win.fail("the closed question left the keyboard on '" + win.focusName() + "', not on the about text");
                }
                // With the caret on no link, a text with ONE link offers that link; with several, one entry each.
                const about = win.shown("detailsAbout");
                about.edit.forceActiveFocus();
                about.edit.cursorPosition = 0;
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("dxOneLink");
                return;
            }
            case "dxOneLink": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu with the caret on no link");
                if (about.menu.link !== win.dxLink || about.menu.listed.length !== 0) return win.fail("a text with one link did not offer it with the caret elsewhere");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("dxOneClosed");
                return;
            }
            case "dxOneClosed": {
                const about = win.shown("detailsAbout");
                if (about.menu.visible || win.focusName() !== "detailsAboutText") return win.later("the menu to close");
                // Two links (by script: the page's own list is put back below).
                about.hrefs = ["https://example.org/one", "https://example.org/two"];
                about.edit.cursorPosition = 0;
                win.key(Qt.Key_Menu, Qt.NoModifier);
                win.next("dxMany");
                return;
            }
            case "dxMany": {
                const about = win.shown("detailsAbout");
                if (!about.menu.opened) return win.later("the menu of a text with two links");
                const entries = win.findAll(QQC2.Overlay.overlay, c => c.objectName === "textOpenListed" && c.visible);
                if (about.menu.link !== "" || entries.length !== 2) return win.fail("two links did not make two entries (" + entries.length + ")");
                if (entries[0].Accessible.name !== "Open https://example.org/one…" || entries[1].Accessible.name !== "Open https://example.org/two…") return win.fail("the entries do not name their addresses");
                if (win.menuEntry("textOpenLink") || win.menuEntry("textCopyLink")) return win.fail("the single link's entries stand beside the list");
                win.key(Qt.Key_Escape, Qt.NoModifier);
                win.next("dxManyClosed");
                return;
            }
            case "dxManyClosed": {
                const about = win.shown("detailsAbout");
                if (about.menu.visible) return win.later("the menu to close");
                about.hrefs = Qt.binding(() => App.person.aboutHrefs ?? []);
                win.clickItem(win.inPanel("detailsClose"));
                win.next("dxGroupOpen");
                return;
            }
            // The group's description: the same component, the cut at three lines, and plain text where the chat is not
            // accepted.
            case "dxGroupOpen":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes her page");
                win.openConversation(THE_GROUP);
                win.next("dxGroupHeader");
                return;
            case "dxGroupHeader":
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.chat.panelOpen || win.waits < 6) return win.later("the group's chat");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("dxGroupPage");
                return;
            case "dxGroupPage": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.chat.panel.level !== "group" || win.waits < 3) return win.later("the group's page");
                const d = win.shown("groupDescription");
                if (!d || d.edit.length === 0) return win.fail("the description is not shown");
                if (d.edit.getText(0, d.edit.length) !== App.group.description) return win.fail("the description on the page is not the group's");
                if (d.edit.Accessible.name !== "Group description" || d.edit.horizontalAlignment !== TextEdit.AlignHCenter) return win.fail("the description has no accessible name, or it is not centred");
                if (!App.group.descriptionLinks || App.group.descriptionHrefs.length !== 1 || App.group.descriptionHrefs[0] !== "https://example.org/regulars") return win.fail("the description of an accepted group has no link");
                // Three lines until More: six lines (by script) are cut at the bottom of the third, More opens the rest.
                d.html = "one<br/>two<br/>three<br/>four<br/>five<br/>six";
                d.text = "one two three four five six";
                win.next("dxClip");
                return;
            }
            case "dxClip": {
                const d = win.shown("groupDescription");
                const more = win.shown("groupDescriptionMore");
                if (d.fullHeight <= 0 || !more) return win.later("the six lines and More");
                if (!d.truncated || d.expanded) return win.fail("six lines are not cut");
                const fourth = d.edit.positionToRectangle(d.edit.getText(0, d.edit.length).indexOf("four"));
                if (Math.abs(d.height - fourth.y) > 2 || d.height >= d.fullHeight) return win.fail("the collapsed text is " + d.height + " high: the third line ends at " + fourth.y + " of " + d.fullHeight);
                win.clickItem(more);
                win.next("dxClipOpen");
                return;
            }
            case "dxClipOpen": {
                const d = win.shown("groupDescription");
                if (!d.expanded || d.height !== d.fullHeight) return win.later("More shows every line");
                if (win.shown("groupDescriptionMore").text !== "Less") return win.fail("the button does not say Less");
                win.clickItem(win.shown("groupDescriptionMore"));
                win.next("dxClipShut");
                return;
            }
            case "dxClipShut": {
                const d = win.shown("groupDescription");
                if (d.expanded || d.height >= d.fullHeight) return win.later("Less cuts the text again");
                // L13: a wrapped right-to-left paragraph (x = 0 of a line is its logical END there): the cut is still the bottom of
                // the third VISUAL line. (A synthetic phrase, written as escapes.)
                const hebrew = "\u05E9\u05DC\u05D5\u05DD \u05E2\u05D5\u05DC\u05DD ".repeat(40).trim();
                d.html = hebrew;
                d.text = hebrew;
                win.next("dxClipHebrew");
                return;
            }
            case "dxClipHebrew": {
                const d = win.shown("groupDescription");
                if (d.edit.length < 100 || d.fullHeight <= 0 || win.waits < 3) return win.later("the wrapped Hebrew paragraph");
                // The visual lines, found from the characters (the tops of their rectangles), not from the cut's own code.
                const tops = [];
                for (let i = 0; i < d.edit.length; ++i) {
                    const y = d.edit.positionToRectangle(i).y;
                    if (tops.indexOf(y) < 0) tops.push(y);
                }
                tops.sort((a, b) => a - b);
                if (tops.length < 5) return win.fail("the Hebrew paragraph wraps into " + tops.length + " lines: too few to test the cut");
                if (!d.truncated || d.expanded) return win.fail("a Hebrew paragraph of " + tops.length + " lines is not cut");
                if (Math.abs(d.height - tops[3]) > 2 || d.height >= d.fullHeight) return win.fail("the collapsed Hebrew paragraph is " + d.height + " high: its third line ends at " + tops[3] + " of " + d.fullHeight + " (" + tops.length + " lines)");
                win.next("dxClipReset");
                return;
            }
            case "dxClipReset": {
                const d = win.shown("groupDescription");
                d.expanded = true;
                d.reset();
                if (d.expanded) return win.fail("reset() leaves the description opened");
                d.html = Qt.binding(() => App.group.descriptionHtml ?? "");
                d.text = Qt.binding(() => App.group.description ?? "");
                // A group that is not accepted: the page's map has the text without links (the viewmodel's table is a unit
                // test); it is still selectable and copiable, and its menu has no link entries.
                win.dxGroup = Object.assign({}, App.group);
                const plain = String(App.group.description);
                const html = plain.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
                App.group = Object.assign({}, win.dxGroup, { "descriptionHtml": html, "descriptionLinks": false, "descriptionHrefs": [] });
                win.next("dxPlain");
                return;
            }
            case "dxPlain": {
                const d = win.shown("groupDescription");
                if (win.waits < 4 || d.edit.getText(0, d.edit.length) !== App.group.description) return win.later("the description of the group that is not accepted");
                const at = d.edit.getText(0, d.edit.length).indexOf("https://example.org/regulars");
                const p = win.pointNear(d.edit, at + 8, 3);
                const q = d.edit.mapFromItem(null, p.x, p.y);
                if (d.edit.linkAt(q.x, q.y) !== "") return win.fail("the address is a link in a group that is not accepted");
                win.clickWindow(p.x, p.y);
                win.rightClickWindow(p.x, p.y);
                win.next("dxPlainMenu");
                return;
            }
            case "dxPlainMenu": {
                const d = win.shown("groupDescription");
                if (!d.menu.opened) return win.later("the menu of the plain description");
                if (win.linkQuestion("https://example.org/regulars")) return win.fail("a click on plain text asked the link question");
                if (win.menuEntry("textOpenLink") || win.menuEntry("textCopyLink") || !win.menuEntry("textSelectAll")) return win.fail("the menu of a text without links lists a link, or lacks Select All");
                win.clickItem(win.menuEntry("textSelectAll"));
                win.next("dxPlainAll");
                return;
            }
            case "dxPlainAll": {
                const d = win.shown("groupDescription");
                if (d.menu.visible || win.focusName() !== "groupDescriptionText") return win.later("the menu to close (" + win.focusName() + ")");
                if (d.edit.selectedText !== App.group.description) return win.fail("Select All selected " + d.edit.selectedText.length + " characters of " + String(App.group.description).length);
                App.copyText("before");
                win.key(Qt.Key_C, Qt.ControlModifier);
                win.next("dxPlainCopied");
                return;
            }
            case "dxPlainCopied": {
                if (win.clipboardText() !== App.group.description) return win.fail("Ctrl+C on the plain description did not copy it");
                // M5: a text that did not have the keyboard (a right press gives it none): right click, Select All, and the
                // keyboard is on the text when the menu closes, so Ctrl+C copies what was selected.
                const d = win.shown("groupDescription");
                win.inPanel("detailsClose").forceActiveFocus();
                if (d.edit.activeFocus) return win.fail("the description had the keyboard before the right click");
                const p = win.pointNear(d.edit, 5, 0);
                win.rightClickWindow(p.x, p.y);
                win.next("dxRightMenu");
                return;
            }
            case "dxRightMenu": {
                const d = win.shown("groupDescription");
                if (!d.menu.opened) return win.later("the menu of the description without the keyboard");
                win.clickItem(win.menuEntry("textSelectAll"));
                win.next("dxRightAll");
                return;
            }
            case "dxRightAll": {
                const d = win.shown("groupDescription");
                if (d.menu.visible || win.waits < 8) return win.later("the menu to close");
                if (win.focusName() !== "groupDescriptionText") return win.fail("Select All left the keyboard on '" + win.focusName() + "': Ctrl+C would copy nothing of it");
                if (d.edit.selectedText !== App.group.description) return win.fail("Select All selected " + d.edit.selectedText.length + " characters of " + String(App.group.description).length);
                App.copyText("before");
                win.key(Qt.Key_C, Qt.ControlModifier);
                win.next("dxRightCopied");
                return;
            }
            case "dxRightCopied": {
                if (win.clipboardText() !== App.group.description) return win.fail("Ctrl+C after right click and Select All did not copy the description");
                App.group = win.dxGroup;
                win.clickItem(win.inPanel("detailsClose"));
                win.next("dxEnd");
                return;
            }
            case "dxEnd":
                if (win.chat.panelOpen || win.waits < 3) return win.later("the X closes the group's page");
                win.next("delOpen");
                return;
            // 13. The group's chat deleted with a member's page on top: the panel goes, and no script error.
            case "delOpen":
                win.openConversation(THE_GROUP);
                win.next("delHeader");
                return;
            case "delHeader":
                if (App.timeline.conversationId !== THE_GROUP || App.timeline.loading || !App.timeline.isGroup || win.waits < 6) return win.later("the group's chat to delete");
                win.loadsBefore = win.groupLoads;
                win.clickHeader();
                win.next("delShown");
                return;
            case "delShown": {
                if (!win.panelReady() || win.groupLoads <= win.loadsBefore || win.waits < 3) return win.later("the group page before the delete");
                const row = win.findAll(win.chat.panel, c => c.objectName === "groupMember" && c.visible && !c.member.isSelf)[0];
                if (!row) return win.fail("no member row to open");
                win.loadsBefore = win.personLoads;
                win.clickItem(row);
                win.next("delMember");
                return;
            }
            case "delMember":
                if (win.personLoads <= win.loadsBefore || win.chat.panel.level !== "person" || !win.chat.panel.loaded || win.waits < 3) return win.later("a member's page before the delete");
                win.chat.askDelete();
                win.next("delConfirm");
                return;
            case "delConfirm": {
                const d = win.chat.deleteDialog;
                if (!d || !d.opened) return win.later("the delete question");
                d.customFooterActions[0].trigger();
                win.next("deletedGroup");
                return;
            }
            case "deletedGroup":
                if (App.timeline.conversationId !== 0 || win.listed(THE_GROUP)) return win.later("the group to go");
                if (win.chat.panelOpen) return win.fail("the panel stays open over a deleted group");
                win.next("deleteOpen");
                return;
            // The chat deleted while its panel is open takes the panel away (and with it the page).
            case "deleteOpen":
                win.openConversation(PERSON);
                win.next("deleteShow");
                return;
            case "deleteShow":
                if (App.timeline.conversationId !== PERSON || App.timeline.loading || win.waits < 6) return win.later("the person's chat to delete");
                if (!win.chat.panelOpen) win.clickHeader();
                win.next("deleteAsk1");
                return;
            case "deleteAsk1":
                if (!win.panelReady()) return win.later("the panel before the delete");
                win.chat.askDelete();
                win.next("deleteConfirm");
                return;
            case "deleteConfirm": {
                const d = win.chat.deleteDialog;
                if (!d || !d.opened) return win.later("the delete question");
                d.customFooterActions[0].trigger();
                win.next("deletedPerson");
                return;
            }
            case "deletedPerson":
                if (App.timeline.conversationId !== 0 || win.listed(PERSON)) return win.later("the chat to go");
                if (win.chat.panelOpen) return win.fail("the panel stays open over a deleted chat");
                // ---- back to the narrow window and the timer chat ----
                win.width = 520;
                win.height = 760;
                win.globalDrawer.collapsed = true;
                win.openConversation(TIMER);
                win.next("personRestore");
                return;
            case "personRestore":
                if (App.timeline.conversationId !== TIMER || App.timeline.loading || win.waits < 6) return win.later("the timer chat again");
                if (win.pageStack.depth !== 2) return win.later("the narrow window");
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
/// The bytes of the one QR code in the picture at `path` (8-bit mode, raw: not read as text).
fn decode_qr(path: &std::path::Path) -> Vec<u8> {
    let picture = image::open(path)
        .expect("the grabbed QR code of the safety number")
        .to_luma8();
    let (w, h) = picture.dimensions();
    let mut decoder = quircs::Quirc::default();
    let codes: Vec<_> = decoder
        .identify(w as usize, h as usize, picture.as_raw())
        .collect();
    assert_eq!(codes.len(), 1, "one QR code in the grab of the card");
    let code = codes
        .into_iter()
        .next()
        .and_then(Result::ok)
        .expect("the QR code of the safety number is found");
    code.decode()
        .expect("the QR code of the safety number decodes")
        .payload
}

/// The scannable bytes of the safety number of the person of the 1:1 chat `conv`, as the backend gives
/// them.
/// Her profile name and the number her page shows (the parts a card of hers may carry), from the core's page.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn person_card_parts(app: &kourier_app::AppHandle, conv: ConversationId) -> (String, String) {
    let Ok(QueryData::Conversation(Some(detail))) =
        app.runtime().block_on(app.query(Query::Conversation(conv)))
    else {
        panic!("her chat");
    };
    let user = detail.row.peer.clone().expect("her chat's person");
    let Ok(QueryData::Person(Some(page))) = app.runtime().block_on(app.query(Query::Person {
        account: detail.row.account,
        user,
    })) else {
        panic!("her page");
    };
    let name = page
        .contact
        .as_ref()
        .and_then(|c| c.profile_name.as_deref())
        .map(|n| n.trim().to_owned())
        .expect("her profile name");
    let number = page.shown_number().expect("her shown number");
    (name, number)
}

/// The contact cards sent from this device in chat `conv`.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_cards(
    app: &kourier_app::AppHandle,
    conv: ConversationId,
) -> Vec<kourier_model::ContactCard> {
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
            Body::Contact(card) => Some(card.clone()),
            _ => None,
        })
        .collect()
}

#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn person_scannable(app: &kourier_app::AppHandle, conv: ConversationId) -> Vec<u8> {
    let Ok(QueryData::Conversation(Some(detail))) =
        app.runtime().block_on(app.query(Query::Conversation(conv)))
    else {
        panic!("her chat");
    };
    let user = detail.row.peer.clone().expect("her chat's person");
    let Ok(QueryData::SafetyNumber { answer, .. }) =
        app.runtime().block_on(app.query(Query::SafetyNumber {
            account: detail.row.account,
            user,
        }))
    else {
        panic!("the safety-number query answers");
    };
    answer
        .expect("her safety number")
        .scannable
        .expose()
        .clone()
}

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

/// Runs this binary again as a child with its stderr read, and fails when the QML printed a
/// script error ("TypeError", "Unable to assign": Kirigami evaluates a header or a button of a
/// page that is not in the stack, and the panel's handlers must not).
fn run_in_child() -> ! {
    let exe = std::env::current_exe().expect("this test binary");
    let child = std::process::Command::new(exe)
        .args(std::env::args().skip(1))
        .env("KOURIER_CHAT_UI_CHILD", "1")
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start the test again as a child");
    let out = child.wait_with_output().expect("wait for the child");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    eprint!("{said}");
    let errors = support::script_errors(&said);
    if !errors.is_empty() {
        println!(
            "test the chat's actions on the real core ... FAILED (script errors)\n{}\n\ntest result: FAILED. 0 passed; 1 failed",
            errors.join("\n")
        );
        std::process::exit(1);
    }
    std::process::exit(out.status.code().unwrap_or(1));
}

/// How long the fixture's last call rings, counted on the core's clock from the fixture's "now"
/// (the clock starts there when the app is set up, before the fixture data is ingested), NOT from
/// the moment the window is up: a start that takes longer than this (the server runs other
/// lanes' tests beside it; one start took 16 s) finds the call already missed. The script reads
/// when the ring ends on that clock (`ringEnd`, `ringLeft()` in `SCRIPT`) and takes the missed
/// form as right exactly when the ring has run out, so no step depends on the speed of the start.
/// What the length buys is how often the proofs that need a ringing row (the pulse, the fade) run:
/// the window is up 9 s after the clock started on the server when it is quiet (12 s left 3 s of
/// ring, too little for the fade), 20 s leaves 11 s and costs the step `ringing` that much waiting.
const RING_FOR: Duration = Duration::from_secs(20);

/// The system clock, in milliseconds since the Unix epoch (what a script's `Date.now()` reads).
fn system_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .expect("the system clock")
}

/// What `App.openExternalUrl` started in place of `xdg-open` (a recorder, nothing starts): the addresses,
/// in order. The window test answers the "Open Link?" question "Open" once, for the address of the about
/// text, and nothing else may open.
static OPENED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn recording_launcher(command: std::process::Command) -> bool {
    assert_eq!(command.get_program(), "xdg-open");
    let url = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    OPENED.lock().expect("the record").push(url);
    true
}

fn main() {
    support::answer_list("chat_ui");
    if std::env::var_os("KOURIER_CHAT_UI_CHILD").is_none() {
        run_in_child();
    }
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
            calls_ring_for: Some(RING_FOR),
            // The own page of the account: a Note to Self chat.
            note_to_self: true,
            // The person shares seven groups: "Show All" has a page to open.
            person_groups: 7,
            // The group page: twelve members or more, two of them made up (one named in markup).
            group_members: 12,
            // Four messages in that group that mention people (the senders and mentions that open one).
            group_mentions: true,
            // A second person whose safety number changed, her first send refused as untrusted (the steps of a
            // key change, P5a).
            safety_peer: true,
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
    let person = fixture
        .person_conversation
        .expect("the chat of a person in groups");
    let group = fixture
        .group_conversation
        .expect("the group of the group page");
    let note = fixture
        .note_to_self_conversation
        .expect("the Note to Self chat");
    let keypeer = fixture
        .safety_conversation
        .expect("the chat of the person whose safety number changed");
    let defpeer = fixture
        .safety_default_conversation
        .expect("the chat of the person never verified");
    let before = own_files(&handle, media).len();
    // The core's clock runs in real time from the fixture's "now": it reads the system clock plus
    // this shift (the midpoint of two system readings around the core's, so the error is a
    // millisecond or two), and the last call stops ringing on it at `ring_end`.
    let (sys_before, core_now, sys_after) = (system_ms(), handle.core_now_ms(), system_ms());
    let clock_shift = core_now - (sys_before + sys_after) / 2;
    let ring_end = fixture.now_ms + i64::try_from(RING_FOR.as_millis()).expect("ms");

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
    // Where `svQr` saves the grabbed QR code of the safety number (decoded below).
    let qr_path = dir.join("safety-qr.png");
    // Her profile name and shown number from the core's page: the picker's line (`shOpened`) is compared with them in the
    // script, and the cards that went out below (never printed).
    let (card_name, card_number) = person_card_parts(&handle, person);
    let script = SCRIPT
        .replace("FIRST_PICTURE", &file("A.png", &png(20)))
        .replace("SECOND_PICTURE", &file("B.png", &png(200)))
        .replace("THIRD_PICTURE", &file("C.png", &png(300)))
        .replace(
            "THE_APP",
            &file("Ground station.apk", b"PK\x03\x04 not really"),
        )
        .replace("CLOCK_SHIFT", &clock_shift.to_string())
        .replace("RING_END", &ring_end.to_string())
        .replace("CALLS", &calls.0.to_string())
        .replace("MEDIA", &media.0.to_string())
        .replace("ALBUMS", &albums.0.to_string())
        .replace("QUOTES", &quotes.0.to_string())
        .replace("PERSON", &person.0.to_string())
        .replace("KEYPEER", &keypeer.0.to_string())
        .replace("DEFPEER", &defpeer.0.to_string())
        .replace("QR_PATH", &format!("{:?}", qr_path.to_string_lossy()))
        .replace("CARD_NAME", &format!("{card_name:?}"))
        .replace("CARD_NUMBER", &format!("{card_number:?}"))
        .replace("THE_GROUP", &group.0.to_string())
        .replace("NOTE", &note.0.to_string())
        .replace("TIMER", &timer.0.to_string());
    // No link opens a program here: the recorder takes `App.openExternalUrl`'s command.
    kourier_qt::app::set_url_launcher(recording_launcher);
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
    // F1: the one address that was answered "Open" (the about text's), and none other: a link asks first, Cancel,
    // Escape and the keys that chose Open Link… start nothing.
    assert_eq!(
        *OPENED.lock().expect("the record"),
        ["https://example.org/pilot"],
        "what the link questions opened"
    );

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

    // `svQr` (P5a critic F11): the QR code on the screen decodes to the scannable bytes of the number the
    // backend gives for her, byte for byte (compared, never printed).
    let decoded = decode_qr(&qr_path);
    let wanted = person_scannable(&handle, person);
    assert!(
        decoded == wanted,
        "the QR code of her safety number does not decode to its scannable bytes ({} decoded, {} wanted)",
        decoded.len(),
        wanted.len()
    );

    // `shSent`, `shReturnSent` (P5c): the cards the picker sent to the chat with media are own messages, each with her PROFILE name and the
    // ONE number her page shows, nothing else (SC1, SC3; compared, never printed).
    let (want_name, want_number) = (card_name, card_number);
    // Two: the click on "Send" (`shSent`) and Return on the chosen row (`shReturnSent`); Cancel and Escape sent none.
    let cards = wait_for(30, || {
        let all = own_cards(&handle, media);
        (all.len() >= 2).then_some(all)
    });
    assert_eq!(
        cards.len(),
        2,
        "two contact cards in the chat with media (Send, then Return)"
    );
    assert!(cards[0] == cards[1], "the two cards differ");
    let card = &cards[0];
    assert!(
        card.name.as_deref() == Some(want_name.as_str()),
        "the card does not carry her profile name"
    );
    assert!(
        card.identifiers.len() == 1
            && card.identifiers[0].kind == kourier_model::IdentifierKind::Phone
            && card.identifiers[0].value == want_number,
        "the card does not carry exactly her shown number ({} identifiers)",
        card.identifiers.len()
    );
    assert!(
        card.organization.is_none() && card.avatar.is_none(),
        "the card carries more than the name and the number"
    );

    let _ = std::fs::remove_dir_all(&dir);
    app.shutdown();
    println!(
        "test the chat's actions on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed"
    );
}
