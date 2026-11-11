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
//! - `emoji` (KH1d, TH4): the link of a fake that plays Threema stands at the emoji step (the
//!   page `themoji`): three glyph labels in the order the phone shows them, each with its name
//!   under it (the viewmodel's names), all plain text, the glyph three times the size of the
//!   name, each item's `Accessible.name` its name, "Waiting for your phone…" under them, no
//!   button but Cancel (the phone confirms), the rotation line hidden, nothing clipped (each
//!   item inside the row, each name inside its item, the row above the waiting line: U1); then
//!   the window is mirrored (a right-to-left language) and the three keep the phone's order,
//!   first emoji on the left, like the safety number's digits (U2). The welcome page under it
//!   ends its footnote at "keeps your message history." (Threema alone has no history source:
//!   U7; `thqr` checks it too, and `signalqr` the long sentence of the Signal-like fake).
//! - `thqr` (KH1d): the same link one step earlier, at its QR code (the page `thqr`): the three
//!   numbered steps, no emoji, and no line that the code changes (Threema's code is one for the
//!   whole link).
//! - `signalqr` (KH1d): the Signal-like fake's QR code shows that line, and no emoji.
//! - `tgqr` (KT-1b): Telegram's link stands at its QR code (the page `tgqr`): the three numbered steps
//!   of the viewmodel, the line that the code changes (Telegram's code rotates), the button "Link with
//!   Phone Number Instead", no emoji, no field note, no resend button, and a welcome footnote that
//!   promises no copy of the history (Telegram's is `OnDemand`).
//! - `tgcode` (KT-1b): one click on that button takes the phone-number way: the question names Telegram
//!   (not WhatsApp's Chrome sentence), the fixture's number is typed and the CODE step stands (the
//!   page `tgcode`): the field "Code", the explain of the typed delivery ("Telegram sent the code to your
//!   Telegram app on another device."), the button "Send a New Code" shown AT ONCE (the wait is 0),
//!   the other way offered, no note.
//! - `tgresend` (KT-1b, finding 5): at that step half a code is typed and the button is clicked: the core
//!   is told the request ALONE (`resend`, never the typed digits: the fake's `login_inputs`), the step
//!   comes back as the new code's ("Telegram sent the code by SMS.") WITHOUT the button, and the
//!   half-typed digits are not there.
//! - `tgpassword` (KT-1b): the right code is typed and the PASSWORD step stands (the page
//!   `tgpassword`): the field "Password" hidden as typed, the hint "Hint: synthetic hint" under it in a
//!   label of plain text, the two-step explain with the line that the forgotten password is reset in
//!   Telegram, NO other way offered, no button for a new code; a wrong password comes back as the
//!   retry step, the field EMPTY (a password is never refilled) and the hint still there.
//! - `tgwait` (KT-1b, finding 5): the button's wait is counted again whenever the page turns to an
//!   input step: a wait of 700 ms keeps the button away for 700 ms, a turn back to "waiting" and to
//!   "input" at 400 ms starts the count again (the first timer is not left running), a wait of 0 shows
//!   it at once and a step without the button never shows one; a page made anew at an input step
//!   (fix round F4) counts the wait from its own completion (ready at once for 0, after the wait for
//!   500 ms). Driven by writing the controller's own properties, so the wait can be any length.
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
    App, FixtureNetwork, FixtureOptions, FixtureSecondLink, FixtureState, FixtureTransfer,
    HistoryTransfer, Launch, StartOptions,
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
    // The three emoji the fake that plays Threema shows, with the viewmodel's names.
    readonly property var emojiShown: SHOWN_EMOJI

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

    // Every item below `item` (itself included) with this object name, in tree order.
    function named(item: Item, name: string): var {
        let found = item.objectName === name ? [item] : [];
        const kids = item.children;
        for (let i = 0; i < kids.length; i++) {
            found = found.concat(win.named(kids[i], name));
        }
        return found;
    }

    // The text of every visible button below `item`.
    function buttonTexts(item: Item): var {
        let found = item.visible && typeof item.click === "function" ? [item.text] : [];
        const kids = item.children;
        for (let i = 0; i < kids.length; i++) {
            found = found.concat(win.buttonTexts(kids[i]));
        }
        return found;
    }

    // A font's size in whatever unit it is set in.
    function fontSize(f: font): real {
        return f.pointSize > 0 ? f.pointSize : f.pixelSize;
    }

    // What is wrong with the link page of Threema or the Signal-like fake at `want` ("emoji",
    // "qr"), or "" when it is right. `emojiShown` is what the page must show of the emoji.
    function pageProblem(want: string): string {
        const page = win.linkPage;
        const row = win.named(page, "emojiRow")[0];
        const rotation = win.named(page, "rotationLine")[0];
        const waiting = win.named(page, "waitingRow")[0];
        if (!row || !rotation || !waiting) return "a part of the page is missing";
        const emojiStep = want === "emoji";
        if (App.login.kind !== want) return "the step is '" + App.login.kind + "'";
        if (row.visible !== emojiStep) return "the emoji row is " + (row.visible ? "shown" : "hidden");
        // The line that the code changes: only a code that is replaced while the page waits.
        const rotates = win.mode === "signalqr";
        if (App.login.rotates !== rotates) return "login.rotates is " + App.login.rotates;
        if (rotation.visible !== rotates) return "the rotation line is " + (rotation.visible ? "shown" : "hidden");
        // The waiting line stays, with the spinner, at the emoji step too.
        const line = win.findText(page, "Waiting for your phone…");
        if (!waiting.visible || line === null || !line.visible) return "no 'Waiting for your phone…' line";
        // The phone confirms: no button on the page but Cancel.
        const buttons = win.buttonTexts(page);
        for (let i = 0; i < buttons.length; i++) {
            if (buttons[i] !== "Cancel") return "a button '" + buttons[i] + "' on the page";
        }
        let shownActions = 0;
        for (let i = 0; i < page.actions.length; i++) {
            if (page.actions[i].visible) shownActions += 1;
        }
        if (shownActions !== 1 || page.actions[0].text !== "Cancel" || !page.actions[0].visible) return "the page's actions are not Cancel alone";
        if (!emojiStep) {
            if (App.login.emoji.length !== 0) return "emoji at a step without any";
            if (win.mode === "thqr") {
                // The three numbered steps of Threema's QR code.
                if (App.login.steps.length !== 3) return App.login.steps.length + " numbered steps";
                for (let i = 0; i < 3; i++) {
                    const label = win.findText(page, App.login.steps[i]);
                    if (label === null || !label.visible) return "step " + (i + 1) + " is not shown";
                }
            }
            return "";
        }
        if (App.login.title !== "Compare the Emoji") return "the title: " + App.login.title;
        const hint = win.findText(page, "Check that your phone shows the same three emoji, then confirm on your phone.");
        if (hint === null || !hint.visible) return "the line that says where to confirm is not shown";
        const items = win.named(row, "emojiItem");
        const glyphs = win.named(row, "emojiGlyph");
        const names = win.named(row, "emojiName");
        if (items.length !== win.emojiShown.length || glyphs.length !== win.emojiShown.length || names.length !== win.emojiShown.length) return "items " + items.length + ", glyphs " + glyphs.length + ", names " + names.length;
        if (App.login.emoji.length !== win.emojiShown.length) return "login.emoji has " + App.login.emoji.length;
        if (row.width <= 0 || row.width > page.width) return "the row is " + row.width + " px in a page of " + page.width;
        const rowBottom = row.mapToItem(null, 0, row.height).y;
        const waitingTop = waiting.mapToItem(null, 0, 0).y;
        if (row.height <= 0) return "the row is " + row.height + " px high";
        // The row ends above the waiting line (U1).
        if (rowBottom > waitingTop + 0.5) return "the row ends at " + rowBottom + ", under the top of the waiting line at " + waitingTop;
        for (let i = 0; i < win.emojiShown.length; i++) {
            const expect = win.emojiShown[i];
            const g = glyphs[i];
            const n = names[i];
            const at = "emoji " + (i + 1) + " ";
            if (App.login.emoji[i].glyph !== expect.glyph || App.login.emoji[i].name !== expect.name) return at + "login.emoji says " + JSON.stringify(App.login.emoji[i]);
            if (g.text !== expect.glyph) return at + "glyph label: " + g.text;
            if (n.text !== expect.name) return at + "name label: " + n.text;
            if (items[i].Accessible.name !== expect.name) return at + "Accessible.name: '" + items[i].Accessible.name + "'";
            if (!g.visible || !n.visible) return at + "is hidden";
            // The backend's text is never guessed to be markup (L5): the glyph's and the name's.
            if (g.textFormat !== Text.PlainText || n.textFormat !== Text.PlainText) return at + "labels are not plain text";
            // Three times the default font, the name in the default one.
            const ratio = win.fontSize(g.font) / win.fontSize(n.font);
            if (ratio < 2.95 || ratio > 3.05) return at + "the glyph is " + ratio + " times the name";
            if (g.contentWidth <= 0 || g.height <= 0) return at + "the glyph is not laid out";
            // The glyph over its name, the name inside its column and not cut.
            if (g.y >= n.y) return at + "the name is not under the glyph";
            if (n.truncated || n.contentWidth > n.width + 1) return at + "the name is cut: " + n.contentWidth + " px of " + n.width;
            // The three side by side in the phone's order, inside the row.
            if (items[i].x < -0.5 || items[i].x + items[i].width > row.width + 0.5) return at + "is outside the row";
            // Nothing is clipped vertically either (U1): a row or an item that is lower than its content
            // lets the glyph and the name run into what is under them. The item inside the row, the
            // glyph and the name inside the item.
            if (items[i].y < -0.5 || items[i].y + items[i].height > row.height + 0.5) return at + "leaves the row vertically: y " + items[i].y + ", height " + items[i].height + " in a row of " + row.height;
            if (g.y < -0.5 || g.y + g.height > items[i].height + 0.5) return at + "the glyph leaves its item: y " + g.y + ", height " + g.height + " in an item of " + items[i].height;
            if (n.y < -0.5 || n.y + n.height > items[i].height + 0.5) return at + "the name leaves its item: y " + n.y + ", height " + n.height + " in an item of " + items[i].height;
            // A label lower than its own text draws the text over what is under it.
            if (g.contentHeight > g.height + 0.5) return at + "the glyph is lower than its text: " + g.height + " for " + g.contentHeight;
            if (n.contentHeight > n.height + 0.5) return at + "the name is lower than its text: " + n.height + " for " + n.contentHeight;
            if (i > 0 && items[i].x <= items[i - 1].x + items[i - 1].width - 0.5) return at + "overlaps the one before";
        }
        return "";
    }

    // The case `tgwait`'s clock: where it is and when the last turn to an input step was.
    property int waitSub: 0
    property double waitAt: 0
    // `tgwait`, last part (fix round F4): a link page made while the login already stands at an input
    // step (a page made anew; nothing turns `kind` after it, so its own `Component.onCompleted` counts
    // the wait).
    property var madePage: null
    Component {
        id: madeLink
        LinkAccountPage {
            visible: false
        }
    }

    // `tgwait`: the page turns to a code step that states this wait (ms) and this button label,
    // the way the controller shows a step (the label and the wait first, `kind` last, through
    // "waiting" as every answer goes).
    function turnToInput(ms: int, label: string): void {
        App.login.fields = [{
                "id": "code",
                "label": "Code",
                "kind": "text",
                "value": "",
                "required": true,
                "note": ""
            }];
        App.login.resendLabel = label;
        App.login.resendAfterMs = ms;
        App.login.kind = "waiting";
        App.login.kind = "input";
    }

    // What is wrong with the link page of the fake that plays Telegram at `want` ("qr", "phone",
    // "code", "code-resent", "password", "password-again"), or "" when it is right.
    function tgPageProblem(page: Item, want: string): string {
        const buttons = win.buttonTexts(page);
        const hasButton = text => buttons.indexOf(text) >= 0;
        const other = win.named(page, "otherFlowButton")[0];
        const resend = win.named(page, "resendButton")[0];
        const notes = win.named(page, "fieldNote");
        const emoji = win.named(page, "emojiRow")[0];
        if (!other || !resend || !emoji) return "a part of the page is missing";
        if (emoji.visible || App.login.emoji.length !== 0) return "emoji on a Telegram page";
        if (App.login.network !== "Telegram") return "the network is " + App.login.network;
        const explain = (text) => {
            // The viewmodel's line above the fields, shown as plain text; none at the QR step.
            if (App.login.explain !== text) return "explain is '" + App.login.explain + "', not '" + text + "'";
            const label = win.findText(page, text);
            if (label === null || !label.visible) return "the line '" + text + "' is not shown";
            if (label.textFormat !== Text.PlainText) return "the explain label is not plain text";
            return "";
        };
        if (want === "qr") {
            if (App.login.steps.length !== 3) return App.login.steps.length + " numbered steps";
            for (let i = 0; i < 3; i++) {
                const label = win.findText(page, App.login.steps[i]);
                if (label === null || !label.visible) return "step " + (i + 1) + " is not shown";
            }
            // Telegram's code is replaced while the page waits: the page says so.
            if (!App.login.rotates) return "login.rotates is false for Telegram's QR code";
            const rotation = win.named(page, "rotationLine")[0];
            if (!rotation || !rotation.visible) return "the line that the code changes is hidden";
            if (!other.visible || other.text !== App.login.otherFlowLabel || App.login.otherFlowLabel !== "Link with Phone Number Instead") return "the other way: '" + other.text + "' (" + other.visible + ")";
            if (resend.visible || notes.some(n => n.visible)) return "a resend button or a note at the QR code";
            return "";
        }
        if (want === "phone") {
            const text = App.login.explain;
            if (text.indexOf("Telegram") < 0 || text.indexOf("Chrome") >= 0) return "the phone-number line: '" + text + "'";
            const problem = explain(text);
            if (problem !== "") return problem;
            if (App.login.fields[0].label !== "Phone number") return "the field says '" + App.login.fields[0].label + "'";
            if (resend.visible || notes.some(n => n.visible)) return "a resend button or a note at the number";
            if (!other.visible || App.login.otherFlow !== "qr") return "the other way is '" + App.login.otherFlow + "'";
            return "";
        }
        if (want === "code" || want === "code-resent") {
            const sent = want === "code" ? "Telegram sent the code to your Telegram app on another device." : "Telegram sent the code by SMS.";
            const problem = explain(sent);
            if (problem !== "") return problem;
            if (App.login.fields[0].label !== "Code") return "the field says '" + App.login.fields[0].label + "'";
            if (notes.some(n => n.visible) || App.login.fields[0].note !== "") return "a note at the code";
            if (!other.visible || App.login.otherFlow !== "qr") return "the other way is '" + App.login.otherFlow + "'";
            if (want === "code-resent") {
                // The new code came with no further way to ask: no button, and nothing half-typed.
                if (resend.visible || App.login.resendLabel !== "") return "the button is there for a code that allows no new one";
                if (App.login.fields[0].value !== "") return "the half-typed code '" + App.login.fields[0].value + "' came back";
                return "";
            }
            // The wait is 0: the button is there with its words, at once.
            if (App.login.resendAfterMs !== 0 || App.login.resendLabel !== "Send a New Code") return "resend: '" + App.login.resendLabel + "' after " + App.login.resendAfterMs + " ms";
            if (!resend.visible || resend.text !== "Send a New Code" || !hasButton("Send a New Code")) return "no button 'Send a New Code' (" + resend.visible + ")";
            return "";
        }
        // The password and its retry: the field hides what is typed, the hint is plain text under it
        // (the first showing only: the retry keeps it), no other way and no button for a new code.
        const problem = explain(App.login.explain);
        if (problem !== "") return problem;
        // Both showings carry the line about a forgotten password; the retry says the password is
        // wrong first (fix round F5: the way back is wanted most after a wrong one).
        if (want === "password-again" && App.login.explain.indexOf("That password is not right. Try again.") !== 0) {
            return "the retry says '" + App.login.explain + "'";
        }
        if (App.login.explain.indexOf("Forgot it? Reset it in the Telegram app: Kourier cannot.") < 0) {
            return "no line about a forgotten password: '" + App.login.explain + "'";
        }
        const field = App.login.fields[0];
        if (field.label !== "Password" || field.kind !== "secret") return "the field: " + JSON.stringify(field);
        if (field.value !== "") return "the password field holds '" + field.value + "'";
        const shown = notes.filter(n => n.visible);
        if (shown.length !== 1 || shown[0].text !== "Hint: synthetic hint" || field.note !== "Hint: synthetic hint") return "the hint: " + JSON.stringify(shown.map(n => n.text)) + " / '" + field.note + "'";
        if (shown[0].textFormat !== Text.PlainText) return "the hint's label is not plain text";
        // The text field of the step hides what is typed.
        const input = win.firstWith(page, "echoMode");
        if (input === null || input.echoMode !== TextInput.Password) return "the password field shows what is typed";
        if (input.text !== "") return "the password field's text is '" + input.text + "'";
        // A screen reader on the field hears the hint: it is the field's accessible description
        // (the hint's label below it is another item; fix round F8).
        if (input.Accessible.description !== "Hint: synthetic hint") return "the field's accessible description is '" + input.Accessible.description + "'";
        if (App.login.otherFlow !== "" || other.visible || hasButton(App.login.otherFlowLabel) && App.login.otherFlowLabel !== "") return "another way is offered at the password step";
        if (resend.visible || App.login.resendLabel !== "") return "a button for a new code at the password step";
        return "";
    }

    // The first visible item below `item` that has the property `name`.
    function firstWith(item: Item, name: string): Item {
        if (item.visible && item[name] !== undefined) {
            return item;
        }
        const kids = item.children;
        for (let i = 0; i < kids.length; i++) {
            const found = win.firstWith(kids[i], name);
            if (found !== null) {
                return found;
            }
        }
        return null;
    }

    // U7: the footnote under the welcome page's list is the viewmodel's choice (`App.networksNote`), and the
    // page shows it as it is. A list of Threema alone has no history source and ends at "keeps your message
    // history."; at signalqr the plain fixture's off WhatsApp row promises a copy (the Signal-like fake has no
    // history source); the Telegram pages (`tg*`) have Telegram as the only row the person can choose (the
    // fixture's Signal is not in the build): its history stays on the network's servers and it has no main
    // phone, so the footnote says only that Kourier links as an additional device (fix round F3).
    function welcomeNoteProblem(): string {
        const long = "Kourier links as an additional device: your phone stays the main device, keeps your message history and can send a copy of it.";
        const short = "Kourier links as an additional device: your phone stays the main device and keeps your message history.";
        const only = "Kourier links as an additional device.";
        const want = win.mode === "signalqr" ? long : win.mode.indexOf("tg") === 0 ? only : short;
        if (App.networksNote !== want) return "App.networksNote is '" + App.networksNote + "'";
        const label = win.findText(win.ensureWelcome(), want);
        if (label === null) return "the welcome page shows no footnote '" + want + "'";
        return "";
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
            if (win.mode === "emoji" || win.mode === "thqr") {
                // Threema's link page (the warning counted: the person ticked it for this link);
                // no device name is asked for.
                win.openLinkAs("Threema", true);
                win.next("threema");
                return;
            }
            if (win.mode.indexOf("tg") === 0) {
                // Telegram's link page (the warning counted); no device name is asked for.
                win.openLinkAs("Telegram", true);
                win.next("tg-qr");
                return;
            }
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
            win.next(win.mode === "transfer" ? "transfer" : win.mode === "xback" ? "xback" : win.mode === "signalqr" ? "threema" : "qr");
            return;
        case "threema": {
            // The page at Threema's QR code or emoji, or at the Signal-like fake's QR code: once
            // it stands, it is looked at on every tick for a moment, and its picture is taken.
            const want = win.mode === "emoji" ? "emoji" : "qr";
            if (App.login.kind !== want || !win.linking()) return win.later("the " + want + " step");
            // The viewmodel's emoji are set before the kind (`controllers.rs` `show`): at the emoji step
            // the three are there in the very tick it shows, at the QR step none. A missing property is
            // a failure at once, not a 60 s wait for items that never come. (A property that was never
            // set is undefined: reading `.length` of it would throw in every tick and hang to the
            // runner's timeout.)
            const sentEmoji = App.login.emoji;
            const sentCount = sentEmoji === undefined || sentEmoji === null ? -1 : sentEmoji.length;
            if (sentCount !== (want === "emoji" ? win.emojiShown.length : 0)) return win.fail("login.emoji has " + (sentCount < 0 ? "never been set" : sentCount + " entries") + " at the " + want + " step");
            // Shown or hidden is known the moment the step is: no wait for the layout first.
            const shownRow = win.named(win.linkPage, "emojiRow")[0];
            if (!shownRow || shownRow.visible !== (want === "emoji")) return win.fail("the emoji row is " + (shownRow && shownRow.visible ? "shown" : "hidden") + " at the " + want + " step");
            if (win.named(win.linkPage, "emojiItem").length !== (want === "emoji" ? win.emojiShown.length : 0)) return win.later("the emoji items");
            if (want === "emoji" && win.named(win.linkPage, "emojiGlyph")[0].height <= 0) return win.later("the emoji laid out");
            const problem = win.pageProblem(want);
            if (problem !== "") return win.fail(problem);
            const note = win.welcomeNoteProblem();
            if (note !== "") return win.fail(note);
            win.quiet += 1;
            if (win.quiet === 5) win.shot(win.mode);
            if (win.quiet < 10) return;
            // The emoji page goes on to the mirrored look; the others end here.
            if (win.mode === "emoji") {
                win.next("mirror");
                return;
            }
            Qt.exit(0);
            return;
        }
        // U2: the window mirrored, as in a right-to-left language. The emoji row is exempt on purpose
        // (`LayoutMirroring.enabled: false` on it, as the safety number's digits): the person compares
        // the sequence with the phone's, so the first emoji stays on the left.
        case "mirror":
            win.LayoutMirroring.enabled = true;
            win.LayoutMirroring.childrenInherit = true;
            win.next("mirrorCheck");
            return;
        case "mirrorCheck": {
            if (win.waits < 8) return win.later("the mirrored layout");
            const page = win.linkPage;
            const row = win.named(page, "emojiRow")[0];
            const waiting = win.named(page, "waitingRow")[0];
            if (!page.LayoutMirroring.enabled) return win.fail("the page is not mirrored: the test does not mirror");
            // Something that follows the direction does mirror: the waiting line's spinner stands at the
            // RIGHT of its words now.
            const words = win.findText(waiting, "Waiting for your phone…");
            let spinner = null;
            for (let i = 0; i < waiting.children.length; i++) {
                if (waiting.children[i] !== words && waiting.children[i].visible) spinner = waiting.children[i];
            }
            if (words === null || spinner === null) return win.fail("the waiting line's parts are not found");
            if (spinner.mapToItem(null, 0, 0).x <= words.mapToItem(null, 0, 0).x) return win.fail("the mirrored waiting line has its spinner at the left of the words");
            const items = win.named(row, "emojiItem");
            if (items.length !== win.emojiShown.length) return win.fail("items " + items.length);
            for (let i = 1; i < items.length; i++) {
                if (items[i].mapToItem(null, 0, 0).x <= items[i - 1].mapToItem(null, 0, 0).x) return win.fail("the mirrored page shows emoji " + i + " at the left of emoji " + (i + 1) + ": the phone's order is not kept");
            }
            const problem = win.pageProblem("emoji");
            if (problem !== "") return win.fail("mirrored: " + problem);
            win.quiet += 1;
            if (win.quiet === 5) win.shot("emoji-rtl");
            if (win.quiet < 10) return;
            Qt.exit(0);
            return;
        }

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
            if (App.login.leave !== leave) return win.fail("the way back: '" + App.login.leave + "' (kind '" + App.login.kind + "', error '" + App.login.error + "')");
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

        // ---- Telegram (KT-1b): the QR code, the phone-number way, the code, the password ----
        case "tg-qr": {
            if (App.login.kind !== "qr" || !win.linking()) return win.later("Telegram's QR code");
            const page = win.linkPage;
            const problem = win.tgPageProblem(page, "qr");
            if (problem !== "") return win.fail(problem);
            const note = win.welcomeNoteProblem();
            if (note !== "") return win.fail(note);
            win.quiet += 1;
            if (win.quiet === 5) win.shot(win.mode);
            if (win.quiet < 10) return;
            if (win.mode === "tgqr") {
                Qt.exit(0);
                return;
            }
            if (win.mode === "tgwait") {
                win.next("tg-wait");
                return;
            }
            // The other way, with one click on the page's own button.
            win.named(page, "otherFlowButton")[0].click();
            win.next("tg-phone");
            return;
        }
        case "tg-phone": {
            if (App.login.kind !== "input" || App.login.flow !== "phone" || App.login.fields.length !== 1 || App.login.fields[0].id !== "phone") return win.later("the phone-number question");
            const problem = win.tgPageProblem(win.linkPage, "phone");
            if (problem !== "") return win.fail(problem);
            App.login.setField("phone", "TG_NUMBER");
            App.login.submit();
            win.next("tg-code");
            return;
        }
        case "tg-code": {
            if (App.login.kind !== "input" || App.login.fields.length !== 1 || App.login.fields[0].id !== "code") return win.later("the code step");
            // The button is there in the very tick the step shows (the wait is 0: no timer to wait for).
            const problem = win.tgPageProblem(win.linkPage, "code");
            if (problem !== "") return win.fail(problem);
            if (win.mode === "tgpassword") {
                App.login.setField("code", "TG_CODE");
                App.login.submit();
                win.next("tg-password");
                return;
            }
            if (win.mode === "tgresend") {
                // Half a code is typed; the click must not send it.
                App.login.setField("code", "123");
                win.named(win.linkPage, "resendButton")[0].click();
                win.next("tg-resent");
                return;
            }
            win.quiet += 1;
            if (win.quiet === 5) win.shot(win.mode);
            if (win.quiet < 10) return;
            Qt.exit(0);
            return;
        }
        case "tg-resent": {
            // The click went to the core and the code step came back, now the new code's: the one
            // that came by SMS, with no way left to ask for another.
            if (App.login.kind !== "input" || App.login.fields.length !== 1 || App.login.fields[0].id !== "code") return win.later("the code step again");
            const problem = win.tgPageProblem(win.linkPage, "code-resent");
            if (problem !== "") return win.fail(problem);
            win.quiet += 1;
            if (win.quiet < 10) return;
            Qt.exit(0);
            return;
        }
        case "tg-password": {
            if (App.login.kind !== "input" || App.login.fields.length !== 1 || App.login.fields[0].id !== "password" || App.login.explain.indexOf("not right") >= 0) return win.later("the password step");
            const problem = win.tgPageProblem(win.linkPage, "password");
            if (problem !== "") return win.fail(problem);
            win.quiet += 1;
            if (win.quiet === 5) win.shot(win.mode);
            if (win.quiet < 10) return;
            // A wrong password: the retry step comes back with the field EMPTY.
            App.login.setField("password", "not the password");
            App.login.submit();
            win.next("tg-wrong");
            return;
        }
        case "tg-wrong": {
            // (The retry's field is the same field; its step says the last entry was not right.)
            if (App.login.kind !== "input" || App.login.fields.length !== 1 || App.login.fields[0].id !== "password" || App.login.explain.indexOf("not right") < 0) return win.later("the retry step");
            const problem = win.tgPageProblem(win.linkPage, "password-again");
            if (problem !== "") return win.fail(problem);
            win.quiet += 1;
            if (win.quiet < 10) return;
            Qt.exit(0);
            return;
        }
        // The wait of the button, counted by the page whenever it turns to an input step. The case
        // writes the controller's own properties (a Telegram code step with any wait cannot be had
        // from the fake: its fixture row says 0).
        case "tg-wait": {
            const page = win.linkPage;
            const button = win.named(page, "resendButton")[0];
            if (win.waitSub === 0) {
                win.turnToInput(700, "Send a New Code");
                win.waitAt = Date.now();
                win.waitSub = 1;
                return;
            }
            const since = Date.now() - win.waitAt;
            if (win.waitSub === 1) {
                // The wait has not run out yet.
                if (since < 300 && button.visible) return win.fail("the button shows " + since + " ms into a wait of 700 ms");
                if (since < 400) return;
                // Turned away and back at 400 ms: the count starts again.
                win.turnToInput(700, "Send a New Code");
                win.waitAt = Date.now();
                win.waitSub = 2;
                return;
            }
            if (win.waitSub === 2) {
                // 500 ms after the second turn: the first timer would have run out 300 ms ago.
                if (since < 500) {
                    if (button.visible) return win.fail("the button shows " + since + " ms after the second turn to an input step: the first wait was not stopped");
                    return;
                }
                if (button.visible) return win.fail("the button shows at " + since + " ms of a wait of 700 ms");
                win.waitSub = 3;
                return;
            }
            if (win.waitSub === 3) {
                // And it comes once the wait is over (a second and a half is long enough).
                if (!button.visible) {
                    if (since > 2500) return win.fail("the button never came after a wait of 700 ms");
                    return;
                }
                if (since < 650) return win.fail("the button came after " + since + " ms of a wait of 700 ms");
                if (button.text !== "Send a New Code") return win.fail("the button says '" + button.text + "'");
                // A wait of 0 shows it at once, in the same tick the page turns to the step.
                win.turnToInput(0, "Send a New Code");
                if (!button.visible) return win.fail("a wait of 0 does not show the button at once");
                // A step without the button never shows one.
                win.turnToInput(0, "");
                if (button.visible) return win.fail("a step with no button shows one");
                // And a wait does not show it before it is over, whatever came before.
                win.turnToInput(60000, "Send a New Code");
                if (button.visible) return win.fail("a wait of a minute shows the button at once");
                win.shot("tgwait");
                win.waitSub = 4;
                return;
            }
            if (win.waitSub === 4) {
                // A page made anew at a code step whose wait is 0: ready at once, in its own completion
                // (no `kind` turns after it; mutant: the page's `Component.onCompleted` removed).
                win.turnToInput(0, "Send a New Code");
                const zero = madeLink.createObject(win);
                if (!zero.resendReady) return win.fail("a page made at a step with a wait of 0 is not ready");
                zero.destroy();
                // And at a step whose wait is 500 ms: not ready at once, ready when the wait is over.
                win.turnToInput(500, "Send a New Code");
                win.madePage = madeLink.createObject(win);
                win.waitAt = Date.now();
                if (win.madePage.resendReady) return win.fail("a page made at a step with a wait of 500 ms is ready at once");
                win.waitSub = 5;
                return;
            }
            if (win.waitSub === 5) {
                const since = Date.now() - win.waitAt;
                if (!win.madePage.resendReady) {
                    if (since > 2500) return win.fail("a page made at a step with a wait of 500 ms was never ready");
                    return;
                }
                if (since < 450) return win.fail("a page made at a step with a wait of 500 ms was ready after " + since + " ms");
                win.madePage.destroy();
                Qt.exit(0);
                return;
            }
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

/// The three emoji the fake that plays Threema shows (`kourier_fake::FAKE_EMOJI`, spelt out here:
/// this crate has no edge to the fake; the black cat is the sequence with a zero-width joiner),
/// each with the name the viewmodel gives it, as the JSON list the script reads.
fn shown_emoji() -> String {
    let list: Vec<_> = ["\u{1F435}", "\u{1F408}\u{200D}\u{2B1B}", "\u{1F33B}"]
        .into_iter()
        .map(|glyph| {
            let name = kourier_viewmodel::login::emoji_name(glyph).expect("a name for the glyph");
            serde_json::json!({ "glyph": glyph, "name": name })
        })
        .collect();
    serde_json::to_string(&list).expect("the list as JSON")
}

fn options(case: &str) -> FixtureOptions {
    let base = FixtureOptions {
        messages_per_conversation: 6,
        link: true,
        transfer_stage: Duration::from_millis(600),
        ..FixtureOptions::default()
    };
    match case {
        // The first code stays on screen for a minute: the second would be scanned.
        "keep" | "stop" | "add" | "cancel2" | "triple" | "signalqr" => FixtureOptions {
            qr_codes: 2,
            qr_valid_for: Duration::from_secs(60),
            ..base
        },
        // Threema's link stands where the page `thqr` or `themoji` has it: at its QR code, or one
        // step on at the emoji (the fake's phone scans at once and does not confirm for ten minutes).
        "emoji" | "thqr" => FixtureOptions {
            network: FixtureNetwork::Threema,
            hold_link: true,
            hold_emoji: case == "emoji",
            ..base
        },
        // Telegram's link stands where the pages `tgqr`, `tgcode` and `tgpassword` have it (the cases
        // walk to their step by clicks and typing, as the pages do): the fake plays Telegram, the
        // first code is held for ten minutes, the phone-number way has the fixture's row.
        "tgqr" | "tgcode" | "tgresend" | "tgpassword" | "tgwait" => FixtureOptions {
            network: FixtureNetwork::Telegram,
            hold_link: true,
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

/// Says how the login inputs the fake got differ from what the case typed (the values of a
/// password are never printed: only the field ids and how many).
fn problem_inputs(
    problems: &mut Vec<String>,
    got: &[Vec<(String, String)>],
    want: &[Vec<(String, String)>],
) {
    let ids = |lists: &[Vec<(String, String)>]| -> Vec<Vec<String>> {
        lists
            .iter()
            .map(|l| l.iter().map(|(k, _)| k.clone()).collect())
            .collect()
    };
    problems.push(format!(
        "the login inputs are {:?}, wanted {:?} (values differ: {})",
        ids(got),
        ids(want),
        ids(got) == ids(want)
    ));
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

/// How many accounts the core has once a cancelled link's account is gone: the core deletes
/// it when the login's supervisor has logged it out and ends (`login_failed`), a moment after
/// the window stopped showing it (the panel and `hasAccounts` never list a linking account).
/// Asked again until it is `want`, for at most 10 s; the last answer otherwise.
fn accounts_settled(app: &kourier_app::AppHandle, want: usize) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let n = accounts(app);
        if n == want || std::time::Instant::now() >= deadline {
            return n;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
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
    let script = SCRIPT
        .replace("CASE", case)
        .replace(
            "SHOTS_DIR",
            &std::env::var("KOURIER_LINK_ACTIONS_SHOTS").unwrap_or_default(),
        )
        .replace("SHOWN_EMOJI", &shown_emoji())
        .replace("TG_NUMBER", FixtureOptions::TELEGRAM_NUMBER)
        .replace("TG_CODE", FixtureOptions::TELEGRAM_CODE);
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
    // The cases where the link stays at the end: nothing was told to stop it.
    let kept = matches!(
        case,
        "keep"
            | "transfer"
            | "chat"
            | "banner"
            | "error"
            | "xback"
            | "xother"
            | "emoji"
            | "thqr"
            | "signalqr"
            | "tgqr"
            | "tgcode"
            | "tgresend"
            | "tgpassword"
            | "tgwait"
    );
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
    // A cancelled link (no fixture account, not kept) leaves none.
    let gone = !with_account && !kept && case != "triple";
    let wanted = if matches!(case, "chat" | "banner" | "error" | "xother") {
        2
    } else {
        1
    };
    // A cancelled link's account goes a moment after the window let it go, so the count is
    // read once it settled at what the case wants (at once when it is there already).
    let n = if with_account {
        accounts_settled(&handle, wanted)
    } else if gone {
        accounts_settled(&handle, 0)
    } else if matches!(case, "transfer" | "xback") {
        accounts_settled(&handle, 1)
    } else {
        accounts(&handle)
    };
    if matches!(case, "transfer" | "xback") && n != 1 {
        problems.push(format!("the account is linked ({n} accounts)"));
    }
    // (The Telegram cases that take the phone-number way give up the QR login on the way: one
    // cancel and one logout, for that login only, and the new one stays.)
    let switches = matches!(case, "tgcode" | "tgresend" | "tgpassword");
    let gave_up = usize::from(switches);
    if kept && (count("CancelLogin") != gave_up || count("Logout") != gave_up) {
        problems.push(format!("a login was cancelled: {commands:?}"));
    }
    if with_account {
        if n != wanted {
            problems.push(format!("{n} accounts, expected {wanted}"));
        }
    } else if gone && n != 0 {
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
    // The click on "Send a New Code" asks for it ALONE: the core is told the number first, then
    // the request with nothing else in it (never the half-typed code). The typed number and the
    // code of `tgpassword` reached the fake as typed.
    let inputs = handle.fixture_login_inputs();
    let text = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    };
    match case {
        "tgresend" => {
            let want = [
                text(&[("phone", FixtureOptions::TELEGRAM_NUMBER)]),
                text(&[(kourier_model::RESEND_FIELD, "1")]),
            ];
            if inputs != want {
                problems.push(format!(
                    "the login inputs are not the number and then the request alone: {} inputs",
                    inputs.len()
                ));
            }
        }
        "tgpassword" => {
            let want = [
                text(&[("phone", FixtureOptions::TELEGRAM_NUMBER)]),
                text(&[("code", FixtureOptions::TELEGRAM_CODE)]),
                text(&[("password", "not the password")]),
            ];
            if inputs != want {
                problem_inputs(&mut problems, &inputs, &want);
            }
        }
        "tgcode" | "tgqr" | "tgwait" => {
            let sent_code = inputs.iter().any(|i| {
                i.iter()
                    .any(|(k, _)| k == "code" || k == kourier_model::RESEND_FIELD)
            });
            if sent_code {
                problems.push("a code or a request for one was sent".to_owned());
            }
        }
        _ => {}
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
        "emoji",
        "thqr",
        "signalqr",
        "tgqr",
        "tgcode",
        "tgresend",
        "tgpassword",
        "tgwait",
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
