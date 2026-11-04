//! The settings page's choice for disappearing messages while the clock is unconfirmed,
//! through the real main window and the real core over the fake network (offscreen,
//! `harness = false`).
//!
//! The fixture account ends with a clock the network calls wrong, so nothing confirms the
//! time, and its chat with a timer holds one message whose time is up a few seconds into
//! the run. What the timeline shows of that message then follows the choice:
//!
//! - Nothing is read for the settings page before it is shown; then it shows the option the
//!   core holds (the default: hide), as three radio buttons with a sentence under each.
//! - By default the message is hidden once its time is up.
//! - "Keep until the time is confirmed", chosen with the keyboard (Space on the option):
//!   the core has the option at once and the message is back in the timeline.
//! - "Hide now, erase once the time is confirmed": it is hidden again.
//! - "Erase by this computer's clock": it is erased, and choosing "keep" afterwards does not
//!   bring it back.
//! - Blended chats: a second account (another identity, no chats) is linked at the start and
//!   hidden with `toggleAccount`; the last shown account cannot be hidden. "Show chats of
//!   several accounts together" is on with its hint, and the hidden account is still in the
//!   preferences after every toggle of the page (the App writes it, the page does not).
//! - "Round avatars" is on; switching it off makes the avatars on screen ask the image
//!   provider for squares with the theme's corner at once, switching it on for circles again;
//!   the preferences hold the last choice.
//! - "Show minor notices in the conversation list" (quiet notices): off by default, so no
//!   chat of the list previews a safety number notice; switching it on makes the two fixture
//!   chats that END in such a notice (a direct chat and a group, dated just after the next
//!   chat's newest message) show it as their preview and move one place up, above the chat
//!   that stood directly above them, and switching it off puts the list back as it was.
//!
//! - "Files with location data", the first row of the Privacy card: "Ask each time" by
//!   default with its sentence; "Never send" chosen in the combo's own list with the keyboard
//!   (Down, Down, Return) is applied at once, its sentence shows, and the preferences hold it.
//!
//! - The Privacy card's second line (KT-1b, three more processes of their own): with the fixture
//!   account on Telegram (`tgprivacy`) "Read receipts" and "Typing indicators" read "On · change on
//!   your phone" and, on a second line, "Always sent for Telegram."; with the plain fixture
//!   (`plainprivacy`: the line is the one it always was, with no second line and no blank one) and
//!   with Telegram listed but OFF (`tgoff`) there is no second line: the sentence follows the
//!   networks that are on.
//!
//! The core is asked directly at the end: the option it holds, whether it shows quiet notices
//! (not), and what its timeline has.

mod support;

use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{ExpiryPolicy, Query, QueryData, TimelineAnchor, TimelineQuery};
use kourier_app::{
    App, EXPIRING_TEXT, FixtureNetwork, FixtureOptions, FixtureSecondLink, FixtureState, Launch,
    StartOptions,
};
use kourier_model::{Body, ConversationId};
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

    property string step: "start"
    property int waits: 0
    property var ev: null
    property int quiet: 0
    // The avatar the "Round avatars" steps watch, and what it asked for while round.
    property var avatar: null
    property string roundSource: ""
    property int roundRadius: 0
    // The list with the quiet notices hidden (the default), for the steps after it.
    property var listBefore: null
    // The fixture account, and the second account linked at the start (blended chats).
    property real first: 0
    property real other: 0

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

    // The radio button of an option, by the core's name for it.
    function option(name: string): Item {
        return win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "expiry-" + name) : null;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property var blocks
        }
    }

    // The conversation list as the model has it now.
    Instantiator {
        id: chats
        model: App.conversations
        delegate: QtObject {
            required property real conversationId
            required property string title
            required property string preview
        }
    }

    // The ids of the list in its order (null while the model is being rebuilt).
    function listed(): var {
        const ids = [];
        for (let i = 0; i < chats.count; ++i) {
            const c = chats.objectAt(i);
            if (!c) {
                return null;
            }
            ids.push(c.conversationId);
        }
        return ids;
    }

    // The ids of the chats whose preview is a safety number notice.
    function noticeChats(): var {
        const ids = [];
        for (let i = 0; i < chats.count; ++i) {
            const c = chats.objectAt(i);
            if (c && c.preview.indexOf("safety number") >= 0) {
                ids.push(c.conversationId);
            }
        }
        return ids;
    }

    // Whether the open timeline has a message with `text`.
    function shows(text: string): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r || r.kind !== "message") {
                continue;
            }
            for (const b of r.blocks) {
                if (b.type === "text" && b.plain === text) {
                    return true;
                }
            }
        }
        return false;
    }

    // Exactly `name` is the checked option.
    function onlyChecked(name: string): bool {
        for (const other of ["hide", "keep", "erase"]) {
            if (win.option(other).checked !== (other === name)) {
                return false;
            }
        }
        return true;
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
        {
            switch (win.step) {
            case "start":
                if (!win.started || !App.hasAccounts) return win.later("the start page");
                // Nothing was read for the settings page: it does not exist yet.
                if (win.settingsPage !== null) return win.fail("the settings page exists at the start");
                if (App.settings.expiryPolicy !== "") return win.fail("the option was read before the page was shown: " + App.settings.expiryPolicy);
                win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "settings_ui_events.qml");
                if (App.accountActions.length !== 1) return win.later("the fixture account");
                win.first = App.accountActions[0].account;
                // A second account, linked at once (another identity, no chats).
                win.openWelcome();
                win.openLink("Fake");
                win.next("second");
                return;
            case "second": {
                if (App.accountActions.length !== 2) return win.later("the second account");
                if (win.linkPage !== null && win.pageStack.lastItem === win.linkPage) return win.later("the link page closes");
                // Blending is on by default: the new account joined, every account is shown.
                if (!App.blend || !App.allShown || App.shownCount !== 2) return win.later("both accounts shown: " + App.blend + " " + App.allShown + " " + App.shownCount);
                const other = App.accountActions.find(a => a.account !== win.first);
                if (!other || !other.shown || other.lastShown) return win.fail("the second account is shown and not the last");
                win.other = other.account;
                // Hide it (Shift+click's path); the preferences hold the hidden account.
                if (!App.toggleAccount(win.other)) return win.fail("the toggle was refused");
                win.next("hidden account");
                return;
            }
            case "hidden account": {
                const mine = App.accountActions.find(a => a.account === win.first);
                const other = App.accountActions.find(a => a.account === win.other);
                if (App.allShown || App.shownCount !== 1 || other.shown || !mine.shown || !mine.lastShown) return win.later("the second account hidden");
                if (App.currentAccount !== win.first) return win.fail("the one shown account is the current one: " + App.currentAccount);
                // The last shown account stays.
                if (App.toggleAccount(win.first)) return win.fail("the last shown account was hidden");
                if (App.shownCount !== 1) return win.fail("a refused toggle changed the choice");
                win.openConversation(TIMER);
                win.next("chat");
                return;
            }
            case "chat":
                if (App.timeline.conversationId !== TIMER || App.timeline.loading || !win.shows("This one is gone in a week.")) return win.later("the chat with the timer");
                win.next("hidden");
                return;
            case "hidden":
                // The default: once its time is up, the message is hidden (the clock is
                // unconfirmed: the network calls it wrong).
                if (win.shows(GONE)) return win.later("the message whose time is up is hidden");
                if (!win.shows("This one is gone in a week.")) return win.fail("only the message whose time is up goes");
                win.openSettings();
                win.next("page");
                return;
            case "page": {
                if (win.settingsPage === null || App.settings.expiryPolicy === "") return win.later("the settings page with the core's answer");
                if (App.settings.expiryPolicy !== "hide") return win.fail("the default: " + App.settings.expiryPolicy);
                const hide = win.option("hide");
                const keep = win.option("keep");
                const erase = win.option("erase");
                if (!hide || !keep || !erase) return win.fail("three options");
                if (!win.onlyChecked("hide")) return win.fail("the default is the checked option");
                // What each says, and one sentence under each.
                if (hide.text !== "Hide now, erase once the time is confirmed" || keep.text !== "Keep until the time is confirmed" || erase.text !== "Erase by this computer's clock") return win.fail("the labels: " + hide.text + " / " + keep.text + " / " + erase.text);
                for (const o of [hide, keep, erase]) {
                    if (!o.enabled) return win.fail("an option cannot be chosen: " + o.text);
                    if (o.description.length < 40 || o.description.indexOf(".") !== o.description.length - 1) return win.fail("one sentence under " + o.text + ": " + o.description);
                }
                // ---- chosen with the keyboard ----
                win.settingsPage.showExpiry();
                keep.forceActiveFocus();
                if (!keep.activeFocus) return win.fail("the option takes the keyboard");
                if (!win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1)) return win.fail("no key event");
                win.next("keep");
                return;
            }
            case "keep":
                // Applied at once: no button to confirm it.
                if (App.settings.expiryPolicy !== "keep" || !win.onlyChecked("keep")) return win.later("the option is keep");
                // The timeline follows: the message is readable again, as it was.
                if (!win.shows(GONE)) return win.later("the message is shown again");
                win.option("hide").click();
                win.next("hide");
                return;
            case "hide":
                if (App.settings.expiryPolicy !== "hide" || !win.onlyChecked("hide")) return win.later("the option is hide");
                if (win.shows(GONE)) return win.later("the message is hidden again");
                win.option("erase").click();
                win.next("erase");
                return;
            case "erase":
                if (App.settings.expiryPolicy !== "erase" || !win.onlyChecked("erase")) return win.later("the option is erase");
                if (win.shows(GONE)) return win.fail("an erased message shows");
                win.option("keep").click();
                win.next("erased");
                return;
            case "erased":
                if (App.settings.expiryPolicy !== "keep" || !win.onlyChecked("keep")) return win.later("the option is keep again");
                // Erased is erased: it does not come back with "keep".
                if (win.shows(GONE)) return win.fail("an erased message came back");
                win.quiet += 1;
                if (win.quiet < 40) return;
                if (!win.shows("This one is gone in a week.")) return win.fail("the other messages are untouched");
                win.next("avatars");
                return;
            case "avatars": {
                // "Round avatars": on by default, and every avatar on screen is a circle.
                const sw = win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "round-avatars") : null;
                win.avatar = win.find(win.contentItem, i => i.pictureRadius !== undefined && i.source !== undefined && i.source !== "");
                if (!sw || !win.avatar) return win.later("the switch and an avatar with a picture");
                if (sw.text !== "Round avatars") return win.fail("the label: " + sw.text);
                // "Show chats of several accounts together": on by default, with its hint.
                const blend = win.find(win.settingsPage, i => i.objectName === "blend-accounts");
                if (!blend || blend.text !== "Show chats of several accounts together") return win.fail("the blend switch: " + (blend ? blend.text : "none"));
                if (!blend.checked || !App.settings.blendAccounts || !App.blend) return win.fail("blending is on by default");
                if (blend.description !== "Shift+click or Ctrl+click an account in the side panel to add it to the list.") return win.fail("the blend hint: " + blend.description);
                if (!sw.checked || !App.settings.roundAvatars) return win.fail("avatars are round by default");
                if (win.avatar.cornerRadius !== win.avatar.side / 2) return win.fail("a round avatar has half its side as corner: " + win.avatar.cornerRadius);
                // The provider is asked for a circle (a radius of half the side or more).
                if (win.avatar.pictureSource.indexOf("&r=") < 0 && win.avatar.pictureSource.indexOf("?r=") < 0) return win.fail("the request has no radius: " + win.avatar.pictureSource);
                win.roundSource = win.avatar.pictureSource;
                win.roundRadius = win.avatar.pictureRadius;
                sw.click();
                win.next("square");
                return;
            }
            case "square": {
                const sw = win.find(win.settingsPage, i => i.objectName === "round-avatars");
                // At once, in the open window: the avatars on screen are asked for again, with
                // the theme's corner and not a circle.
                if (App.settings.roundAvatars || sw.checked || win.avatar.pictureSource === win.roundSource) return win.later("avatars are not round");
                if (!(win.avatar.cornerRadius < win.avatar.side / 2)) return win.fail("a square avatar keeps a corner of its own: " + win.avatar.cornerRadius);
                if (!(win.avatar.pictureRadius < win.roundRadius)) return win.fail("the request still asks for a circle: " + win.avatar.pictureSource);
                sw.click();
                win.next("round again");
                return;
            }
            case "round again":
                if (!App.settings.roundAvatars || win.avatar.pictureSource !== win.roundSource) return win.later("avatars are round again");
                // Left off for the core's check at the end.
                win.find(win.settingsPage, i => i.objectName === "round-avatars").click();
                win.next("off");
                return;
            case "off":
                if (App.settings.roundAvatars) return win.later("avatars are not round");
                win.quiet += 1;
                if (win.quiet < 10) return;
                win.next("minor");
                return;
            case "minor": {
                // "Show minor notices in the conversation list": off by default.
                const sw = win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "quiet-notices") : null;
                const ids = win.listed();
                if (!sw || App.settings.quietNotices === "" || !ids || ids.length < 12) return win.later("the switch, the core's answer and the list");
                if (sw.text !== "Show minor notices in the conversation list") return win.fail("the label: " + sw.text);
                if (sw.description.indexOf("Safety number changes") !== 0 || sw.description.indexOf("always stay in the chat") < 0) return win.fail("the kinds are named under it: " + sw.description);
                if (!sw.enabled) return win.fail("the switch cannot be used once the core has answered");
                if (sw.checked || App.settings.quietNotices !== "hide") return win.fail("off by default: " + App.settings.quietNotices);
                // The default: no chat of the list shows a notice as its last message.
                if (win.noticeChats().length !== 0) return win.fail("a chat previews a safety number notice by default: " + win.noticeChats());
                win.listBefore = ids;
                sw.click();
                win.next("minor on");
                return;
            }
            case "minor on": {
                const sw = win.find(win.settingsPage, i => i.objectName === "quiet-notices");
                if (App.settings.quietNotices !== "show" || !sw.checked) return win.later("the choice is show");
                // The two chats that end in such a notice show it, and each stands one place
                // higher: above the chat that stood directly above it.
                const shown = win.noticeChats();
                const after = win.listed();
                if (shown.length !== 2 || !after) return win.later("two chats preview a notice: " + shown);
                for (const id of shown) {
                    const was = win.listBefore.indexOf(id);
                    const now = after.indexOf(id);
                    if (was < 1 || now !== was - 1 || after[now + 1] !== win.listBefore[was - 1]) return win.later("the chat " + id + " moves above the chat " + win.listBefore[was - 1] + " (was " + was + ", is " + now + ")");
                }
                sw.click();
                win.next("minor off");
                return;
            }
            case "minor off": {
                const sw = win.find(win.settingsPage, i => i.objectName === "quiet-notices");
                if (App.settings.quietNotices !== "hide" || sw.checked) return win.later("the choice is hide");
                // The list as it was: the same order, no notice as a preview.
                const after = win.listed();
                if (!after || JSON.stringify(after) !== JSON.stringify(win.listBefore) || win.noticeChats().length !== 0) return win.later("the list is as it was: " + JSON.stringify(after) + " against " + JSON.stringify(win.listBefore));
                win.quiet += 1;
                if (win.quiet < 10) return;
                win.next("location");
                return;
            }
            case "location": {
                // "Files with location data": Ask each time by default, with its sentence.
                const combo = win.settingsPage ? win.find(win.settingsPage, i => i.objectName === "locationFiles") : null;
                if (!combo) return win.later("the location files combo");
                if (combo.text !== "Files with location data") return win.fail("the label: " + combo.text);
                if (combo.count !== 3 || combo.currentIndex !== 0 || App.settings.locationFiles !== 0) return win.fail("ask each time by default: " + combo.currentIndex + " of " + combo.count);
                if (combo.currentText !== "Ask each time") return win.fail("the shown choice: " + combo.currentText);
                if (combo.description.indexOf("When Kourier finds where") !== 0) return win.fail("the ask sentence: " + combo.description);
                // The first row of the Privacy card: above "Read receipts".
                const receipts = win.find(win.settingsPage, i => i.text === "Read receipts" && i.description !== undefined);
                if (!receipts || !(combo.mapToItem(null, 0, 0).y < receipts.mapToItem(null, 0, 0).y)) return win.fail("the combo stands above Read receipts");
                // The plain fixture has no network that always sends them: the rows are the
                // one line they always were (no second line, no blank one).
                const typing = win.find(win.settingsPage, i => i.text === "Typing indicators" && i.description !== undefined);
                if (receipts.description !== "On · change on your phone" || !typing || typing.description !== "On · change on your phone") return win.fail("the privacy rows: '" + receipts.description + "' / '" + (typing ? typing.description : "none") + "'");
                // Opened like a click does; the choice is made in its list with the keyboard.
                combo.clicked();
                win.next("location open");
                return;
            }
            case "location open":
                win.quiet += 1;
                if (win.quiet < 5) return;
                win.ev.keyClick(Qt.Key_Down, Qt.NoModifier, -1);
                win.ev.keyClick(Qt.Key_Down, Qt.NoModifier, -1);
                win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
                win.next("location never");
                return;
            case "location never": {
                const combo = win.find(win.settingsPage, i => i.objectName === "locationFiles");
                if (App.settings.locationFiles !== 2 || combo.currentIndex !== 2) return win.later("the choice is never send (" + App.settings.locationFiles + ")");
                if (combo.currentText !== "Never send") return win.fail("the shown choice: " + combo.currentText);
                if (combo.description !== "Such files stay in the list; everything else is sent.") return win.fail("the never sentence: " + combo.description);
                win.quiet += 1;
                if (win.quiet < 10) return;
                Qt.exit(0);
                return;
            }
            }
        }
    }

    Timer {
        interval: 180000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

/// The Privacy card's rows ("Read receipts", "Typing indicators") on the settings page, in a
/// process of its own per case (`CASE`: `tgprivacy`, `plainprivacy`, `tgoff`; `SECOND` is the
/// second line the case expects, empty for none).
const PRIVACY_SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

MainWindow {
    id: win

    width: 520
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property int quiet: 0

    function fail(msg: string): void {
        console.error("FAIL [CASE/" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            fail("never happened: " + what);
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

    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: {
            if (win.step === "start") {
                if (!win.started || !App.hasAccounts) return win.later("the start page and the fixture account");
                win.openSettings();
                win.step = "page";
                return;
            }
            const page = win.settingsPage;
            const receipts = page ? win.find(page, i => i.objectName === "readReceipts") : null;
            const typing = page ? win.find(page, i => i.objectName === "typingIndicators") : null;
            if (!receipts || !typing) return win.later("the privacy rows");
            // The viewmodel's choice, computed once (`App.privacyNote`), is what the rows add.
            const second = "SECOND";
            if (App.privacyNote !== second) return win.fail("App.privacyNote is '" + App.privacyNote + "'");
            const want = "On · change on your phone" + (second === "" ? "" : "\n" + second);
            if (receipts.text !== "Read receipts" || receipts.description !== want) return win.fail("Read receipts: '" + receipts.description + "'");
            if (typing.text !== "Typing indicators" || typing.description !== want) return win.fail("Typing indicators: '" + typing.description + "'");
            // Both rows say the same, and the second line is no part of the row's own words: the
            // row is as tall as its lines need (a second line grows it, nothing is cut).
            if (second !== "" && !(receipts.height > 0 && typing.height > 0)) return win.fail("a row has no height");
            win.quiet += 1;
            if (win.quiet < 10) return;
            Qt.exit(0);
        }
    }

    Timer {
        interval: 60000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

/// The texts of the timer chat as the core has them.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn texts(app: &kourier_app::AppHandle, conv: ConversationId) -> Vec<String> {
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
        .filter_map(|r| match &r.body {
            Body::Text(t) => Some(t.body.clone()),
            _ => None,
        })
        .collect()
}

#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn quiet_notices_shown(app: &kourier_app::AppHandle) -> bool {
    match app.runtime().block_on(app.query(Query::QuietNotices)) {
        Ok(QueryData::QuietNotices(shown)) => shown,
        _ => panic!("the choice the core holds"),
    }
}

#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn policy(app: &kourier_app::AppHandle) -> ExpiryPolicy {
    match app.runtime().block_on(app.query(Query::ExpiryPolicy)) {
        Ok(QueryData::ExpiryPolicy(policy)) => policy,
        _ => panic!("the option the core holds"),
    }
}

/// One case of the Privacy card's second line in this process: the exit code.
fn run_privacy(case: &str) -> i32 {
    let (network, telegram_off, second) = match case {
        "tgprivacy" => (FixtureNetwork::Telegram, false, "Always sent for Telegram."),
        "plainprivacy" => (FixtureNetwork::Fake, false, ""),
        // Telegram is listed and OFF: it does not send anything for this start.
        "tgoff" => (FixtureNetwork::Fake, true, ""),
        other => panic!("unknown case {other}"),
    };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 6,
            network,
            telegram_off,
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
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = PRIVACY_SCRIPT
        .replace("CASE", case)
        .replace("SECOND", second);
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/settings_ui_privacy_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    code
}

fn main() {
    support::answer_list("settings_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // The Privacy card's second line: each case in a process of its own.
    if let Ok(case) = std::env::var("KOURIER_SETTINGS_UI_CASE") {
        std::process::exit(run_privacy(&case));
    }
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            // Nothing confirms the time: the network calls this computer's clock wrong.
            state: FixtureState::ClockSkew,
            // One message of the timer chat is due a few seconds into the run.
            expiring: Some(Duration::from_secs(3)),
            // A second account (no chats), whose hiding must outlive the page's toggles.
            second_link: Some(FixtureSecondLink::OtherAccount),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let timer = fixture.timer_conversation.expect("the timer chat");
    assert_eq!(
        policy(&handle),
        ExpiryPolicy::HideUntilConfirmed,
        "the default"
    );
    assert!(!quiet_notices_shown(&handle), "hidden by default");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT
        .replace("GONE", &format!("{EXPIRING_TEXT:?}"))
        .replace("TIMER", &timer.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/settings_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test the choice for disappearing messages ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }

    // The preferences hold the last choice of "Round avatars" (round, square, round, square).
    assert!(!handle.desktop().prefs().round_avatars);
    // ... and the hidden second account through all of the page's toggles: the App writes
    // `hidden-accounts`, the page does not list it (M3), so its toggles keep it.
    let prefs = handle.desktop().prefs();
    assert_eq!(prefs.hidden_accounts.len(), 1, "one hidden account kept");
    assert!(prefs.blend_accounts);
    // ... and "Never send" for files with location data, chosen in the combo.
    assert_eq!(
        handle.desktop().prefs().location_files,
        kourier_app::LocationFiles::Never
    );
    // The core holds what the page chose last, and the erased message is gone from it.
    assert_eq!(policy(&handle), ExpiryPolicy::KeepUntilConfirmed);
    // The quiet notices were shown and hidden again.
    assert!(!quiet_notices_shown(&handle));
    let left = texts(&handle, timer);
    assert!(
        !left.iter().any(|t| t == EXPIRING_TEXT),
        "the message was erased under \"erase\" and stays erased"
    );
    assert!(
        left.iter().any(|t| t == "This one is gone in a week."),
        "the message whose time is not up is kept"
    );

    app.shutdown();
    println!("test the choice for disappearing messages ... ok");

    // The Privacy card's second line, each case in a process of its own.
    let exe = std::env::current_exe().expect("the test binary");
    let mut failed = 0;
    let cases = ["tgprivacy", "plainprivacy", "tgoff"];
    for case in cases {
        let out = std::process::Command::new(&exe)
            .env("KOURIER_SETTINGS_UI_CASE", case)
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
            println!("test the privacy card: {case} ... ok");
        } else {
            println!("test the privacy card: {case} ... FAILED ({})", out.status);
            failed += 1;
        }
    }
    let passed = 1 + cases.len() - failed;
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed");
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed");
        std::process::exit(1);
    }
}
