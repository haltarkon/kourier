//! An experimental network (WhatsApp) shows up honestly in the real main window, on the real
//! core over the fake network (offscreen, `harness = false`; each case in a process of its own).
//! K-A, C6b: no WhatsApp backend exists in this build; the fake plays it.
//!
//! - `off`: the build holds WhatsApp and the person has not turned it on. Choosing it on the
//!   welcome page opens the warning page (no login starts, nothing to lose): its words, the
//!   accepting button "Turn On and Restart" disabled until the box is ticked, with the reason
//!   for a screen reader; Cancel goes back to the welcome page; the page made again has the
//!   box unticked (not remembered). Ticked, the accepting button writes the preference and the
//!   one-shot link request and asks for a restart.
//! - `settings`: Settings > "Networks" shows the switch off; turning it on writes the
//!   preference, says "Takes effect when Kourier starts again" and offers "Restart Now",
//!   which asks for a restart (the core has no runtime switch).
//! - `ask`: the fixture account IS a WhatsApp account and the network is on. Turning it off
//!   asks "Turn Off WhatsApp?" first: the switch and the preference stay on until the question
//!   is answered, "Cancel" keeps them, "Turn Off" turns the preference off and shows the note.
//! - `prefoff` (U7): as `ask`, then the person goes to Add Account. The preference is off but
//!   this start still has WhatsApp registered (the core has no runtime switch): the Add Account
//!   row says "Off · experimental" and choosing it opens the warning with "Turn On and
//!   Restart", not the link page of a network the next start would pause.
//! - `restartasks` (U4): as `settings`, but the fixture account's history transfer is still
//!   downloading. "Restart Now" asks "Restart Kourier?" first (the quit question's text);
//!   "Cancel" restarts nothing and records no request; asked again and confirmed, it restarts.
//! - `historyasks` (W4, D2): the fixture account IS a WhatsApp account whose phone is still
//!   pushing its message history (`FixtureWhatsApp::Filling`, the page `wahistory`). The quit
//!   question is the pushed history's ("Quit While History Arrives?": the phone sends it once,
//!   so the words do not promise that it goes on at the next start), and "Restart Now" (after
//!   turning the network off) asks "Restart Kourier?" with those words first; Cancel restarts
//!   nothing and records no request, the second answer restarts.
//!
//! - `turnon`: the fixture account IS a WhatsApp account and the network is turned off at this
//!   start (a paused account: the fake is registered for the seed only). Its banner says so and
//!   offers "Turn On and Restart", which writes the preference and asks for a restart, with no link page to
//!   open after it (the account is linked already).
//! - `appeal`: the account is banned for a while and the network's own words come with the ban:
//!   the banner is the restriction's, Kourier's sentence names the end and the reason, the
//!   network's text is plain text beside it, and the one button that opens its page is
//!   offered for an `https://` address (never opened here).
//!
//! - `tgask` (KT-1b): the plain fixture lists a Telegram that is turned off (the page `tgwarn`'s
//!   `telegram_off`). Its entry asks first, the Settings row is a switch that is off with the
//!   experimental sentence and no "Always sent" line anywhere (Telegram is not on), its Add Account
//!   row says "Off · experimental", and the row opens Telegram's warning: its title, the plan's
//!   three paragraphs, the tick text, "Turn On and Restart" disabled until ticked (with the reason),
//!   the restart note; ticked, it writes `networks-on` with Telegram, the one-shot link request and
//!   asks for a restart.
//! - `tgmissing` (KT-1b): the build cannot log in to Telegram (`telegram_missing`). Its entry is a
//!   row of Settings only: "Not available in this build", NO switch, never a choice (the welcome
//!   page and Add Account list the others and not Telegram); a preference that has Telegram on
//!   (written the way the switch would) makes no "Takes effect when Kourier starts again" and no
//!   "Restart Now": a restart would change nothing for a network that cannot start.
//!
//! The preferences are read at the end: the preference of the network, the one-shot link
//! request, and whether a restart was asked for.

mod support;

use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{
    App, FixtureNetwork, FixtureOptions, FixtureState, FixtureTransfer, FixtureWhatsApp,
    HistoryTransfer, Launch, StartOptions, TelegramMissing,
};
use kourier_model::NetworkKind;
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

    property string mode: "CASE"
    property string step: "start"
    property int waits: 0
    property bool closedAsked: false
    property bool busy: false
    // `tgmissing`: how many ticks the settings page was watched after the preference changed.
    property int tgQuiet: 0

    onClosedForGood: win.closedAsked = true

    function fail(msg: string): void {
        console.error("FAIL [" + mode + "/" + step + "]: " + msg);
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

    function named(page: Item, name: string): Item {
        return page ? win.find(page, i => i.objectName === name) : null;
    }

    // The cases whose fixture account is a WhatsApp account on a start that has the network on.
    function asks(): bool {
        return win.mode === "ask" || win.mode === "prefoff" || win.mode === "historyasks";
    }

    function prefOn(): bool {
        for (let i = 0; i < App.settings.networksOn.length; ++i) {
            if (App.settings.networksOn[i] === "WhatsApp") {
                return true;
            }
        }
        return false;
    }

    function whatsapp(): var {
        for (let i = 0; i < App.networks.length; ++i) {
            if (App.networks[i].kind === "WhatsApp") {
                return App.networks[i];
            }
        }
        return null;
    }

    // The entries of Telegram in the list (one, or none).
    function telegrams(): var {
        return App.networks.filter(n => n.kind === "Telegram");
    }

    // Whether the preference has Telegram on.
    function telegramPref(): bool {
        for (let i = 0; i < App.settings.networksOn.length; ++i) {
            if (App.settings.networksOn[i] === "Telegram") {
                return true;
            }
        }
        return false;
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
        switch (win.step) {
        case "start": {
            if (!win.started || !App.hasAccounts) return win.later("the start page and the fixture account");
            if (win.mode.indexOf("tg") === 0) {
                // Telegram's cases: exactly one entry of it, whose state is the case's.
                const list = win.telegrams();
                if (list.length !== 1) return win.fail(list.length + " entries of Telegram");
                const t = list[0];
                const missing = win.mode === "tgmissing";
                if (t.missing !== missing || t.on || !t.experimental) return win.fail("the entry: " + JSON.stringify(t));
                // The off Telegram asks first and can be chosen; a missing one is no choice at all.
                if (t.asksFirst === missing || t.available === missing) return win.fail("asks first or can be chosen: " + JSON.stringify(t));
                if (App.privacyNote !== "") return win.fail("a network that is not on always sends: '" + App.privacyNote + "'");
                if (missing && (t.description !== "Not available in this build" || t.offDescription !== t.description)) return win.fail("the missing row's words: " + JSON.stringify(t));
                if (!missing && (t.offDescription !== "Off · experimental" || t.description !== t.offDescription)) return win.fail("the off row's words: " + JSON.stringify(t));
                win.openSettings();
                win.next("tg-settings");
                return;
            }
            if (win.mode === "restartasks" && !App.quitQuestion()) return win.later("the transfer the fixture account stands at");
            if (win.mode === "historyasks") {
                // The first batch of the push comes a moment after the start.
                const q = App.quitQuestion();
                if (!q) return win.later("the pushed history's first batch");
                if (q.title !== "Quit While History Arrives?" || q.text.indexOf("may not arrive") < 0 || q.text.indexOf("only once") >= 0 || q.text.indexOf("next start") >= 0) return win.fail("the quit question of a pushed history: " + JSON.stringify(q));
            }
            const n = win.whatsapp();
            if (n === null) return win.fail("WhatsApp is always listed");
            if (n.on !== (win.mode === "ask" || win.mode === "appeal" || win.mode === "prefoff" || win.mode === "historyasks") || !n.experimental || !n.asksFirst || !n.available) return win.fail("the entry: " + JSON.stringify(n));
            if (win.mode === "off") {
                win.openWelcome();
                win.next("welcome");
            } else if (win.mode === "turnon" || win.mode === "appeal") {
                win.next("banner");
            } else {
                win.openSettings();
                win.next("page");
            }
            return;
        }
        case "tg-settings": {
            const page = win.settingsPage;
            if (!page) return win.later("the settings page");
            const sw = win.named(page, "network-Telegram");
            const row = win.named(page, "network-missing-Telegram");
            const restartNote = win.named(page, "networks-restart-note");
            const restartButton = win.named(page, "networks-restart");
            // WhatsApp (off) is listed beside it with its switch, in both cases.
            if (!win.named(page, "network-WhatsApp") || !restartNote || !restartButton) return win.later("the networks card");
            if (win.mode === "tgask") {
                if (!sw || row) return win.fail("Telegram off: a switch " + !!sw + ", a missing row " + !!row);
                if (sw.text !== "Telegram" || sw.description.indexOf("Experimental. Kourier is not made or approved by Telegram") !== 0) return win.fail("the row: " + sw.text + " / " + sw.description);
                if (sw.checked || win.telegramPref()) return win.fail("the switch or the preference is on");
                // (A page that is not yet the current one is disabled for a moment.)
                if (!sw.enabled) return win.later("the switch is enabled");
                if (restartNote.visible || restartButton.visible) return win.fail("a restart is offered before anything changed");
                win.openWelcome();
                win.next("tg-welcome");
                return;
            }
            // Missing: a row with its reason and NO switch.
            if (sw || !row) return win.fail("Telegram missing: a switch " + !!sw + ", a row " + !!row);
            if (!row.visible || row.text !== "Telegram" || row.description !== "Not available in this build") return win.fail("the row: " + row.visible + " / " + row.text + " / " + row.description);
            if (restartNote.visible || restartButton.visible) return win.fail("a restart is offered before anything changed");
            // The preference has Telegram on (what the switch of a build that could run it writes):
            // for a network that cannot start, a restart changes nothing, so none is offered.
            App.settings.setNetworkOn("Telegram", true);
            win.next("tg-pref");
            return;
        }
        case "tg-pref": {
            if (!win.telegramPref()) return win.later("the preference is on");
            const page = win.settingsPage;
            win.tgQuiet += 1;
            if (win.named(page, "networks-restart-note").visible || win.named(page, "networks-restart").visible) return win.fail("a restart is offered for a network that cannot start");
            if (win.tgQuiet < 10) return;
            win.openWelcome();
            win.next("tg-welcome");
            return;
        }
        case "tg-welcome": {
            if (win.welcomePage === null || win.pageStack.lastItem !== win.welcomePage) return win.later("the Add Account page");
            // The control: the other networks are rows (WhatsApp, off).
            const whatsapp = win.find(win.welcomePage, i => i.text === "WhatsApp" && i.description !== undefined);
            if (!whatsapp) return win.later("the row of WhatsApp");
            const telegram = win.find(win.welcomePage, i => i.text === "Telegram" && i.description !== undefined);
            if (win.mode === "tgmissing") {
                // Never a choice: no row, not even a disabled one.
                if (telegram) return win.fail("Add Account lists a Telegram this build cannot log in to: '" + telegram.description + "'");
                Qt.exit(0);
                return;
            }
            if (!telegram) return win.later("the row of Telegram");
            if (telegram.description !== "Off · experimental" || !telegram.enabled) return win.fail("the row: '" + telegram.description + "', enabled " + telegram.enabled);
            win.welcomePage.linkNetwork("Telegram");
            win.next("tg-risk");
            return;
        }
        case "tg-risk": {
            const page = win.riskPage;
            if (page === null || win.pageStack.lastItem !== page) return win.later("the warning page");
            // Nothing started: no login, so nothing would be lost.
            if (App.abandonQuestion() !== undefined || win.linkPage !== null && win.inStack(win.linkPage)) return win.fail("a login started before the warning was accepted");
            if (!page.cancelButton.enabled) return win.later("the page is enabled");
            if (page.confirmBox.checked) return win.fail("the box is ticked beforehand");
            if (page.view.title !== "Before You Add Telegram") return win.fail("the title: " + page.view.title);
            if (page.view.paragraphs.length !== 3) return win.fail("three paragraphs: " + page.view.paragraphs.length);
            if (page.view.paragraphs[0].indexOf("Kourier is not made by Telegram") !== 0 || page.view.paragraphs[2].indexOf("always sends read receipts, typing and online status") < 0) return win.fail("the paragraphs: " + JSON.stringify(page.view.paragraphs));
            if (page.confirmBox.text !== "I understand that Telegram may limit or ban this account") return win.fail("the box: " + page.confirmBox.text);
            if (page.acceptButton.text !== "Turn On and Restart" || page.view.restartNote === "") return win.fail("the accepting button: " + page.acceptButton.text);
            if (page.acceptButton.enabled) return win.fail("going on is possible without the tick");
            if (page.acceptButton.Accessible.description !== "Tick the box above to continue.") return win.fail("the reason: " + page.acceptButton.Accessible.description);
            page.confirmBox.toggle();
            win.next("tg-ticked");
            return;
        }
        case "tg-ticked": {
            const page = win.riskPage;
            if (!page.confirmBox.checked || !page.acceptButton.enabled) return win.later("the tick enables going on");
            page.acceptButton.click();
            win.next("restart");
            return;
        }
        case "welcome": {
            if (win.welcomePage === null || win.pageStack.lastItem !== win.welcomePage) return win.later("the welcome page");
            // A click on the row: the page's signal, as the delegate sends it.
            win.welcomePage.linkNetwork("WhatsApp");
            win.next("risk");
            return;
        }
        case "risk":
        case "risk again": {
            const page = win.riskPage;
            if (page === null || win.pageStack.lastItem !== page) return win.later("the warning page");
            // Nothing started: no login, so nothing would be lost.
            if (App.abandonQuestion() !== undefined || win.linkPage !== null && win.inStack(win.linkPage)) return win.fail("a login started before the warning was accepted");
            // (A page that has just been pushed is disabled for a moment.)
            if (!page.cancelButton.enabled) return win.later("the page is enabled");
            if (page.confirmBox.checked) return win.fail("the box is ticked beforehand");
            if (win.step === "risk") {
                if (page.view.title !== "Before You Link WhatsApp") return win.fail("the title: " + page.view.title);
                if (page.view.paragraphs.length !== 5) return win.fail("five paragraphs: " + page.view.paragraphs.length);
                if (page.view.paragraphs[4].indexOf("linked device") < 0 || page.view.paragraphs[4].indexOf("No usage statistics are sent to WhatsApp.") < 0 || page.view.paragraphs[4].indexOf("Kourier (") >= 0) return win.fail("the last paragraph (no promise of the phone's label): " + page.view.paragraphs[4]);
                if (page.acceptButton.text !== "Turn On and Restart") return win.fail("the accepting button: " + page.acceptButton.text);
                if (page.view.restartNote === "") return win.fail("no word that going on restarts Kourier");
                if (page.confirmBox.text !== "I understand that WhatsApp may restrict or ban this account") return win.fail("the box: " + page.confirmBox.text);
                if (page.acceptButton.enabled) return win.fail("going on is possible without the tick");
                if (page.acceptButton.Accessible.description !== "Tick the box above to continue.") return win.fail("the reason: " + page.acceptButton.Accessible.description);
                // A press on the disabled button does nothing.
                page.acceptButton.click();
                if (win.closedAsked) return win.fail("a disabled button restarted Kourier");
                page.cancelButton.click();
                win.next("cancelled");
            } else {
                page.confirmBox.toggle();
                win.next("ticked");
            }
            return;
        }
        case "cancelled":
            if (win.pageStack.lastItem !== win.welcomePage) return win.later("the welcome page again");
            if (App.abandonQuestion() !== undefined) return win.fail("Cancel left a login");
            win.welcomePage.linkNetwork("WhatsApp");
            win.next("risk again");
            return;
        case "ticked": {
            const page = win.riskPage;
            if (!page.confirmBox.checked || !page.acceptButton.enabled) return win.later("the tick enables going on");
            if (page.acceptButton.Accessible.description !== "") return win.fail("a reason for an enabled button");
            page.acceptButton.click();
            win.next("restart");
            return;
        }
        case "page": {
            const sw = win.named(win.settingsPage, "network-WhatsApp");
            if (!sw) return win.later("the switch");
            if (sw.text !== "WhatsApp" || sw.description.indexOf("Experimental. Kourier is not made or approved by WhatsApp") !== 0) return win.fail("the row: " + sw.text + " / " + sw.description);
            if (win.settingsPage.linking("WhatsApp")) return win.fail("no link runs: " + App.login.network + "/" + App.login.kind);
            // (A page that is not yet the current one is disabled for a moment.)
            if (!sw.enabled) return win.later("the switch is enabled");
            if (App.settings.accountsOf("WhatsApp") !== (win.asks() ? 1 : 0)) return win.fail("accounts of WhatsApp: " + App.settings.accountsOf("WhatsApp"));
            if (sw.checked !== win.asks() || win.prefOn() !== sw.checked) return win.fail("the switch follows the preference: " + sw.checked);
            if (win.named(win.settingsPage, "networks-restart-note").visible || win.named(win.settingsPage, "networks-restart").visible) return win.fail("a restart is offered before anything changed");
            sw.click();
            win.next(win.asks() ? "question" : "on");
            return;
        }
        case "on": {
            const sw = win.named(win.settingsPage, "network-WhatsApp");
            if (!win.prefOn() || !sw.checked) return win.later("the preference is on");
            const note = win.named(win.settingsPage, "networks-restart-note");
            const button = win.named(win.settingsPage, "networks-restart");
            if (!note.visible || !button.visible) return win.later("the note and Restart Now");
            if (note.text !== "Takes effect when Kourier starts again." || button.text !== "Restart Now") return win.fail("the note: " + note.text + " / " + button.text);
            button.click();
            win.next(win.mode === "restartasks" ? "restart asked" : "restart");
            return;
        }
        case "restart asked":
        case "restart asked again": {
            // A transfer runs: the restart asks like a quit does, and has done nothing yet.
            const d = win.restartDialog;
            if (d === null || !d.opened || !d.armed) return win.later("the question, armed");
            if (win.closedAsked) return win.fail("the window quit before the answer");
            const words = win.mode === "historyasks" ? "still sending your message history" : "still importing your message history";
            if (d.question.title !== "Restart Kourier?" || d.question.confirm !== "Restart" || d.question.cancel !== "Cancel" || d.question.text.indexOf(words) < 0) return win.fail("the question: " + JSON.stringify(d.question));
            if (win.mode === "historyasks" && d.question.text.indexOf("next start") >= 0) return win.fail("a pushed history does not go on at the next start: " + d.question.text);
            if (win.step === "restart asked") {
                d.cancelButton.click();
                win.next("restart cancelled");
            } else {
                d.confirmButton.click();
                win.next("restart");
            }
            return;
        }
        case "restart cancelled": {
            if (win.restartDialog !== null) return win.later("the question closed");
            if (win.closedAsked) return win.fail("Cancel restarted Kourier");
            // (The Rust side checks at the end that no restart was recorded before the second answer.)
            win.named(win.settingsPage, "networks-restart").click();
            win.next("restart asked again");
            return;
        }
        case "question": {
            const d = win.settingsPage.turnOffDialog;
            if (d === null || !d.opened) return win.later("the question");
            if (d.question.title !== "Turn Off WhatsApp?" || d.question.confirm !== "Turn Off" || d.question.cancel !== "Cancel") return win.fail("the question: " + JSON.stringify(d.question));
            // Nothing changed yet: the switch follows the preference.
            if (!win.prefOn() || !win.named(win.settingsPage, "network-WhatsApp").checked) return win.fail("the switch or the preference changed before the answer");
            d.cancelButton.click();
            win.next("kept");
            return;
        }
        case "kept": {
            if (win.settingsPage.turnOffDialog !== null) return win.later("the question closed");
            if (!win.prefOn() || !win.named(win.settingsPage, "network-WhatsApp").checked) return win.fail("Cancel turned the network off");
            if (win.named(win.settingsPage, "networks-restart-note").visible) return win.fail("a restart is offered after Cancel");
            win.named(win.settingsPage, "network-WhatsApp").click();
            win.next("question again");
            return;
        }
        case "question again": {
            const d = win.settingsPage.turnOffDialog;
            if (d === null || !d.opened || !d.armed) return win.later("the question, armed");
            d.confirmButton.click();
            win.next("off");
            return;
        }
        case "off": {
            const sw = win.named(win.settingsPage, "network-WhatsApp");
            if (win.prefOn() || sw.checked) return win.later("the preference is off");
            const note = win.named(win.settingsPage, "networks-restart-note");
            if (!note.visible) return win.later("the note says it takes effect at the next start");
            if (win.closedAsked) return win.fail("turning it off restarted Kourier by itself");
            if (win.mode === "historyasks") {
                // Turning the network off was answered; "Restart Now" asks like a quit does.
                const button = win.named(win.settingsPage, "networks-restart");
                if (!button.visible) return win.later("Restart Now");
                button.click();
                win.next("restart asked");
                return;
            }
            if (win.mode === "prefoff") {
                // The preference is off; this start still has the network registered.
                const n = win.whatsapp();
                if (n === null || !n.on) return win.fail("the start's registry changed at once: " + JSON.stringify(n));
                win.openWelcome();
                win.next("add account");
                return;
            }
            Qt.exit(0);
            return;
        }
        case "add account": {
            if (win.welcomePage === null || win.pageStack.lastItem !== win.welcomePage) return win.later("the Add Account page");
            const n = win.whatsapp();
            const row = win.find(win.welcomePage, i => i.text === "WhatsApp" && i.description !== undefined);
            if (!row) return win.later("the row of WhatsApp");
            if (n.offDescription === "" || n.offDescription === n.description) return win.fail("the entry has no words of its own for the off state: " + JSON.stringify(n));
            // The row follows the preference (off), not this start's registry (on).
            if (row.description !== n.offDescription) return win.fail("the row of a network the preference has off: " + row.description);
            win.welcomePage.linkNetwork("WhatsApp");
            win.next("off warning");
            return;
        }
        case "off warning": {
            const page = win.riskPage;
            if (page === null || win.pageStack.lastItem !== page) return win.later("the warning page");
            // The warning of a network that is turned off, and no login started behind it.
            if (App.abandonQuestion() !== undefined || win.linkPage !== null && win.inStack(win.linkPage)) return win.fail("a login started for a network the preference has off");
            if (page.acceptButton.text !== "Turn On and Restart" || page.view.restartNote === "") return win.fail("the warning of a network that is on: " + page.acceptButton.text);
            Qt.exit(0);
            return;
        }
        case "banner": {
            if (win.mode === "turnon") {
                // Paused: the banner of a turned-off network, with the one way out.
                if (App.bannerKind !== "networkOff") return win.later("the banner of a turned-off network: " + App.bannerKind);
                if (App.bannerText !== "WhatsApp is turned off. Turn it on to send and receive.") return win.fail("the banner's text: " + App.bannerText);
                if (App.bannerAction !== "Turn On and Restart") return win.fail("the banner's button: " + App.bannerAction);
                // What the banner's button does. (The preference is read at the end: the
                // settings page was never opened, so its controller has not loaded it.)
                App.bannerActivate();
                win.next("turned on");
                return;
            }
            // A ban with the network's own words.
            if (App.bannerKind !== "restricted") return win.later("the banner of a ban: " + App.bannerKind);
            if (App.bannerText.indexOf("WhatsApp banned this account until ") !== 0 || App.bannerText.indexOf("you sent too many messages to people who have not saved your number") < 0) return win.fail("Kourier's sentence: " + App.bannerText);
            if (App.bannerAction !== "") return win.fail("a ban offers a button before it has ended: " + App.bannerAction);
            if (App.bannerExplanation !== "Your account is temporarily banned. You can use WhatsApp again when the time is up.") return win.fail("the network's text: " + App.bannerExplanation);
            if (App.bannerAppealUrl.indexOf("https://") !== 0) return win.fail("the page to appeal on: " + App.bannerAppealUrl);
            if (App.bannerAppealLabel !== "Open WhatsApp's Page") return win.fail("the page's button: " + App.bannerAppealLabel);
            if (win.closedAsked) return win.fail("a ban restarted Kourier");
            Qt.exit(0);
            return;
        }
        case "turned on":
            if (!win.closedAsked) return win.later("the window quits for the restart");
            Qt.exit(0);
            return;
        case "restart":
            if (!win.closedAsked) return win.later("the window quits for the restart");
            Qt.exit(0);
            return;
        }
    }
}
"##;

fn options(case: &str) -> FixtureOptions {
    FixtureOptions {
        messages_per_conversation: 6,
        network: match case {
            "ask" | "appeal" | "prefoff" | "historyasks" => FixtureNetwork::WhatsApp,
            "turnon" => FixtureNetwork::WhatsAppOff,
            _ => FixtureNetwork::Fake,
        },
        // The plain fixture lists a Telegram that is turned off (the page `tgwarn`)...
        telegram_off: case == "tgask",
        // ... or has a build that cannot log in to it (no page: a knob of this test).
        telegram_missing: (case == "tgmissing").then_some(TelegramMissing::ApiKey),
        // A paused account is not connected; a banned one is restricted.
        state: if case == "turnon" {
            FixtureState::Offline
        } else {
            FixtureState::Online
        },
        whatsapp: match case {
            "appeal" => Some(FixtureWhatsApp::Banned),
            // The phone is still sending the chats' history (the page `wahistory`).
            "historyasks" => Some(FixtureWhatsApp::Filling),
            _ => None,
        },
        // The history of the linked account is still coming in.
        transfer: (case == "restartasks").then_some(FixtureTransfer::Left(
            HistoryTransfer::Downloading {
                done: 12_300_000,
                total: Some(45_600_000),
            },
        )),
        ..FixtureOptions::default()
    }
}

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
    let script = SCRIPT.replace("CASE", case);
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/networks_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        app.shutdown();
        return code;
    }

    // What the person's choices left in the preferences and what asked for a restart.
    let prefs = handle.desktop().prefs();
    let mut problems = Vec::new();
    let kind = if case.starts_with("tg") {
        NetworkKind::Telegram
    } else {
        NetworkKind::WhatsApp
    };
    let (on, restart, pending) = match case {
        // Telegram's warning accepted: on, a restart, and the link page for after it.
        "tgask" => (true, true, Some(NetworkKind::Telegram)),
        // The preference was written (as the switch of a build that could run it would); the page
        // offered no restart and asked for none, and no link request stands.
        "tgmissing" => (true, false, None),
        // "Turn On and Restart": on, a restart, and the link page for after it.
        "off" => (true, true, Some(NetworkKind::WhatsApp)),
        // The switch and "Restart Now".
        "settings" | "restartasks" => (true, true, None),
        // The banner's "Turn On and Restart": on and a restart; the account is linked, so no link page.
        "turnon" => (true, true, None),
        // A ban shows its words and touches nothing.
        "appeal" => (true, false, None),
        // The network was turned off (the question answered), then "Restart Now" asked the
        // pushed history's question and the second answer restarted.
        "historyasks" => (false, true, None),
        // Turned off at the end of the question; no restart by itself.
        _ => (false, false, None),
    };
    if prefs.network_on(kind) != on {
        problems.push(format!(
            "the preference of {kind:?}: {:?}",
            prefs.networks_on
        ));
    }
    if prefs.pending_link_kind() != pending {
        problems.push(format!(
            "the one-shot link request: {:?}",
            prefs.pending_link
        ));
    }
    if handle.restart_requested() != restart {
        problems.push(format!(
            "a restart was asked for: {}",
            handle.restart_requested()
        ));
    }
    app.shutdown();
    if problems.is_empty() {
        0
    } else {
        for p in problems {
            eprintln!("FAIL [{case}]: {p}");
        }
        1
    }
}

fn main() {
    support::answer_list("networks_ui");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    if let Ok(case) = std::env::var("KOURIER_NETWORKS_UI_CASE") {
        std::process::exit(run(&case));
    }
    let exe = std::env::current_exe().expect("the test binary");
    let only = std::env::var("KOURIER_NETWORKS_ONLY").unwrap_or_default();
    let cases: Vec<&str> = [
        "off",
        "settings",
        "ask",
        "prefoff",
        "turnon",
        "appeal",
        "restartasks",
        "historyasks",
        "tgask",
        "tgmissing",
    ]
    .into_iter()
    .filter(|c| only.is_empty() || only.split(',').any(|o| o == *c))
    .collect();
    let mut failed = 0;
    for case in &cases {
        let out = Command::new(&exe)
            .env("KOURIER_NETWORKS_UI_CASE", case)
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
            println!("test networks: {case} ... ok");
        } else {
            println!("test networks: {case} ... FAILED ({})", out.status);
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
