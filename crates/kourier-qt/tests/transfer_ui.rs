//! The history transfer of a newly linked account in the real main window, on the real core
//! over the fake network (architecture §14.2 item 14; offscreen, `harness = false`). Each
//! case runs in a process of its own (one Qt application per process): the test binary
//! starts itself once per case.
//!
//! - `skip`: a link with a transfer shows "Waiting for Your Phone" on the link page with
//!   "Continue Without History"; the button skips, the link finishes, and nothing is said
//!   about the end. The fake received one skip.
//! - `leave`: the link page is left while the transfer waits (back): nothing is cancelled
//!   (the account is linked), the banner takes the transfer over at once, and its button
//!   skips it.
//! - `done`: a transfer that walks to its end shows waiting, downloading and reading on the
//!   link page (no button while it reads), says once that the history was transferred, and
//!   the fake received one dismissal.
//! - `banner`: after a restart in the middle of a download there is no link page: the
//!   account banner shows the same view, compact, with its bytes and the same button; the
//!   button skips, the banner goes and nothing is said.
//! - `failed`: a failure found at the start shows a banner that stays; "Dismiss" removes it
//!   and the fake received the dismissal. The account is untouched.
//! - `again`: "Try Again…" on the banner opens the question with the keyboard on "Cancel";
//!   Cancel and Escape change nothing, Enter does not confirm; the confirming button removes
//!   the account, a second activation does nothing more, and the link page opens with a QR
//!   code.

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
    // The states the link page showed of the transfer, in order.
    property var phases: []
    // The account before it was replaced.
    property real firstAccount: 0
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
            fail("never happened: " + what + " (login " + App.login.kind + ", transfer '" + App.linkTransfer.phase + "', banner '" + win.transferBanner + "', accounts " + accounts.count + ")");
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
        function onLinkTransferChanged(): void {
            const phase = App.linkTransfer.phase;
            if (phase !== "" && (win.phases.length === 0 || win.phases[win.phases.length - 1] !== phase)) {
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
            if (win.mode === "failed" || win.mode === "banner") {
                if (!App.hasAccounts) return win.later("the fixture account");
                win.next(win.mode === "failed" ? "failure" : "restart");
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
            if (App.linkTransfer.phase !== "waiting") return win.later("the transfer waits for the phone");
            const view = win.linkPage ? win.linkPage.transferItem : null;
            if (!view || !view.skipButton.visible) return win.later("the transfer view with its button");
            if (!win.linking()) return win.fail("the link page stays up during the transfer");
            if (App.login.kind === "done") return win.fail("the login is done before the transfer is");
            if (App.linkTransfer.title !== "Waiting for Your Phone") return win.fail("the title: " + App.linkTransfer.title);
            if (App.linkTransfer.text.indexOf("Your phone is preparing your message history.") !== 0) return win.fail("the sentence: " + App.linkTransfer.text);
            if (view.skipButton.text !== "Continue Without History") return win.fail("the button: " + view.skipButton.text);
            if (view.skipButton.Accessible.name !== "Continue Without History") return win.fail("the button's accessible name");
            if (view.compact) return win.fail("the link page shows the large presentation");
            // The account is linked: there is nothing left to cancel.
            if (win.linkPage.actions[0].visible) return win.fail("Cancel is offered during a transfer");
            // Shown once: not in the banner too while the page is up.
            if (win.transferBanner === "transfer") return win.fail("the banner repeats what the page shows");
            // It stays there until somebody acts.
            win.quiet += 1;
            if (win.quiet < 12) return;
            view.skipButton.click();
            win.next("skipped");
            return;
        }
        case "skipped":
            if (App.linkTransfer.phase !== "" || win.linking() || !App.hasAccounts) return win.later("the link finishes after the skip");
            // The end of our own skip is not announced, and nothing is left of the transfer.
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.transferNotices !== 0) return win.fail("our own skip was announced");
            if (win.transferBanner === "transfer" || win.transferBanner === "transferFailed") return win.fail("a banner is left: " + win.transferBanner);
            if (win.phases.join(",") !== "waiting") return win.fail("the states shown: " + win.phases.join(","));
            Qt.exit(0);
            return;

        // ---- leave ----
        case "leave": {
            if (App.linkTransfer.phase !== "waiting" || !win.linkPage || !win.linkPage.transferItem) return win.later("the transfer waits for the phone");
            if (win.transferBanner === "transfer") return win.fail("the banner repeats what the page shows");
            // Back: what the page's own back button does.
            App.login.cancel();
            win.leaveLink();
            win.next("left");
            return;
        }
        case "left": {
            if (win.linking() || win.transferBanner !== "transfer") return win.later("the banner takes the transfer over");
            const view = win.find(win.contentItem, i => i.skipButton !== undefined && i.compact === true);
            if (!view || !view.visible || !view.skipButton.visible) return win.later("the banner's view with its button");
            if (App.linkTransfer.phase !== "") return win.fail("the link page's transfer outlives the page");
            if (view.phase !== "waiting" || App.bannerTransfer.status !== "Waiting for your phone to prepare the message history…") return win.fail("the banner: " + App.bannerTransfer.status);
            // (A transfer is not the state of an account: that banner has nothing to say.)
            if (App.bannerKind !== "" || App.bannerText !== "") return win.fail("the state's banner shows too: " + App.bannerKind);
            if (accounts.count !== 1) return win.fail("leaving the page removed the account");
            win.quiet += 1;
            if (win.quiet < 12) return;
            view.skipButton.click();
            win.next("left-skipped");
            return;
        }
        case "left-skipped":
            if (win.transferBanner === "transfer") return win.later("the banner goes after the skip");
            win.quiet += 1;
            if (win.quiet < 25) return;
            if (win.transferNotices !== 0) return win.fail("our own skip was announced");
            if (accounts.count !== 1) return win.fail("the account stays");
            Qt.exit(0);
            return;

        // ---- done ----
        case "done": {
            const view = win.linkPage ? win.linkPage.transferItem : null;
            if (view && view.phase === "downloading" && view.phase === App.linkTransfer.phase) {
                if (!view.skipButton.visible) return win.fail("a download can be skipped");
                // "0 B" before the size is known, then "4.1 MB of 45.6 MB".
                if (view.detail === App.linkTransfer.detail && !/^[0-9.]+ [KM]?B( of 45\.6 MB)?$/.test(view.detail)) return win.fail("the bytes: " + view.detail);
            }
            if (view && view.phase === "reading" && view.phase === App.linkTransfer.phase) {
                if (view.skipButton.visible) return win.fail("a read offers a skip");
                if (App.linkTransfer.text.indexOf("Keep Kourier open") !== 0) return win.fail("the sentence: " + App.linkTransfer.text);
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
            if (win.phases.join(",") !== "waiting,downloading,reading") return win.fail("the states shown: " + win.phases.join(","));
            if (win.transferBanner === "transfer" || win.transferBanner === "transferFailed") return win.fail("a banner is left: " + win.transferBanner);
            Qt.exit(0);
            return;

        // ---- banner ----
        case "restart": {
            if (win.transferBanner !== "transfer") return win.later("the banner of the running transfer");
            const view = win.find(win.contentItem, i => i.skipButton !== undefined && i.compact === true);
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
        "leave" => FixtureOptions {
            link: true,
            transfer: Some(FixtureTransfer::Hold(HistoryTransfer::Waiting)),
            ..base
        },
        "done" => FixtureOptions {
            link: true,
            transfer: Some(FixtureTransfer::Walk(HistoryTransfer::Done)),
            ..base
        },
        "banner" => FixtureOptions {
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
    let script = SCRIPT.replace("CASE", case);
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/transfer_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
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
            expect("the account is linked", accounts(&handle) == 1);
        }
        "leave" => {
            expect("one skip reached the backend", skips == 1);
            expect("the end of the skip was dismissed", dismissals >= 1);
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
        "banner" => {
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
    let cases = ["skip", "leave", "done", "banner", "failed", "again"];
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
