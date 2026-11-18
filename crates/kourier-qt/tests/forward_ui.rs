//! Forward through the real main window over the real core (forward line F6; offscreen, silent,
//! `harness = false`): three accounts, so a chat of another account is a converted target. One window,
//! these cases in order:
//!
//! - the selection bar's Forward… (a real click) opens the dialog with the title "Forward 2 Messages";
//!   Forward is off with nothing ticked; a real click on a row of the source's account ticks it (the Look
//!   stays hidden, the network's own forward), ticking a chat of another account shows the Look and unticking
//!   it hides it again; Forward with a comment starts the job, the dialog and the selection end and the notice
//!   says what was forwarded.
//! - the message menu's Forward… (`actions.forward`) opens the dialog for one message.
//! - the card: `App.composer.stageForward` into chat B shows it, Send is on with no text, the X drops it,
//!   Escape in the text area drops it, Enter with a comment sends it and the card goes.

mod support;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{RequestId, UiCommand, UiEvent};
use kourier_app::{App, AppHandle, FixtureOptions, Launch, StartOptions};
use kourier_model::{ConversationId, MessageId, Text};
use kourier_qt::{UiContext, UiOptions};

const CHILD: &str = "KOURIER_FORWARD_UI_CHILD";

const SCRIPT: &str = r##"
import QtQuick
import QtQuick.Controls as QQC2
import app.kourier

MainWindow {
    id: win

    width: 1100
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property int noticesBefore: 0
    property string afterPick: ""
    property string hitNote: ""

    function fail(msg: string): void {
        console.error("FAIL [" + win.step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 3000) {
            win.fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
    }

    function find(root: Item, pred: var): Item {
        if (pred(root)) {
            return root;
        }
        for (const child of root.children ?? []) {
            const found = win.find(child, pred);
            if (found) {
                return found;
            }
        }
        return null;
    }

    function named(name: string): Item {
        const pred = c => c.objectName === name && c.visible;
        // The window's footer (the export strip) sits beside the content item, not in it.
        return win.find(win.contentItem, pred) ?? (win.footer ? win.find(win.footer, pred) : null);
    }

    function clickItem(item: Item): void {
        if (!win.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
    }

    // The deepest visible item under (x, y) that can take a mouse click: a MouseArea or a
    // control (pictures, separators and plain rectangles take none, a click passes them).
    function takesClick(c: Item): bool {
        if (!c.enabled) {
            return false;
        }
        if ("acceptedButtons" in c) {
            return c.acceptedButtons !== Qt.NoButton;
        }
        return "hoverEnabled" in c;
    }

    function deepest(root: Item, x: real, y: real): Item {
        const kids = root.children ?? [];
        for (let i = kids.length - 1; i >= 0; --i) {
            const c = kids[i];
            if (!c.visible || c.opacity === 0) {
                continue;
            }
            const cx = x - c.x;
            const cy = y - c.y;
            if (cx >= 0 && cy >= 0 && cx < c.width && cy < c.height) {
                const found = win.deepest(c, cx, cy);
                if (found) {
                    return found;
                }
            }
        }
        return win.takesClick(root) ? root : null;
    }

    function hitAt(item: Item): Item {
        const p = item.mapToItem(win.contentItem, item.width / 2, item.height / 2);
        return win.deepest(win.contentItem, p.x, p.y);
    }

    function within(hit: Item, item: Item): bool {
        for (let c = hit; c; c = c.parent) {
            if (c === item) {
                return true;
            }
        }
        return false;
    }

    function select(ids: var): void {
        App.timeline.selectStart(ids[0]);
        for (let i = 1; i < ids.length; ++i) {
            App.timeline.selectToggle(ids[i]);
        }
    }

    property int noticesBefore2: 0
    property int pjRun: 0
    property real pjLimit: 4000
    property real pjTall: 0
    property real pjTotal: 0
    property real own: 0
    property real other: 0

    property bool busy: false
    Timer {
        interval: 20
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

    // The visible item with objectName `name` in the window's popups (the message menu).
    function popupNamed(name: string): Item {
        return win.find(QQC2.Overlay.overlay, c => c.objectName === name && c.visible);
    }

    function tick(): void {
        switch (win.step) {
        case "start": {
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            if (!win.footer || win.footer.height !== 0) return win.fail("the hidden strips are " + (win.footer ? win.footer.height : "no") + " px high");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "forward_ui_events.qml");
            win.openConversation(CONVA);
            return win.next("select1");
        }
        case "select1": {
            if (App.timeline.conversationId !== CONVA) return win.later("the chat");
            win.select(RUNA);
            return win.next("bar1");
        }
        // The bar's Forward… opens the dialog (a real click).
        case "bar1": {
            if (!App.timeline.selecting || App.timeline.selectedCount !== RUNA.length) return win.later("the selection: " + App.timeline.selectedCount);
            const button = win.named("selectionForward");
            if (!button || !button.enabled) return win.later("the Forward… button");
            win.clickItem(button);
            return win.next("dialog1");
        }
        case "dialog1": {
            const d = win.forwardDialog;
            if (!d || !d.opened || d.view.title === "" || App.forwardTargets.count < 2) return win.later("the forward dialog and its chats");
            if (d.view.title !== "Forward 2 Messages") return win.fail("the title: " + d.view.title);
            if (d.canGo) return win.fail("Forward is on with no chat ticked");
            for (let i = 0; i < App.forwardTargets.count; ++i) {
                const id = App.forwardTargets.conversationAt(i);
                if (App.forwardTargets.isNative(id)) { win.own = win.own || id; } else { win.other = win.other || id; }
            }
            if (win.own === 0 || win.other === 0) return win.fail("the list has no chat of the source's account and one of another");
            if (d.lookBox.visible) return win.fail("the Look shows with no chat ticked");
            // A real click on the row ticks it.
            const row = win.find(d.list.contentItem, c => c.objectName === "forwardRow" && c.conversation === win.own);
            if (!row) return win.fail("no row of the own account");
            win.clickItem(row);
            return win.next("tick1");
        }
        case "tick1": {
            const d = win.forwardDialog;
            if (d.ticked.length !== 1 || d.planPending) return win.later("the plan of the first tick");
            if (d.lookBox.visible) return win.fail("the Look shows for a chat of the same account");
            if (!d.canGo) return win.fail("Forward is off for a chat of the same account: " + JSON.stringify(d.view));
            d.toggle(win.other);
            return win.next("tick2");
        }
        case "tick2": {
            const d = win.forwardDialog;
            if (d.ticked.length !== 2 || d.planPending) return win.later("the plan of the second tick");
            if (!d.lookBox.visible) return win.fail("the Look stays hidden with a chat of another account ticked");
            if (!d.canGo) return win.fail("Forward is off: " + JSON.stringify(d.view));
            // Unticking the converted chat hides it again.
            d.toggle(win.other);
            return win.next("tick3");
        }
        case "tick3": {
            const d = win.forwardDialog;
            if (d.ticked.length !== 1 || d.planPending) return win.later("the plan of the untick");
            if (d.lookBox.visible) return win.fail("the Look stays after the converted chat was unticked");
            d.toggle(win.other);
            return win.next("go1");
        }
        case "go1": {
            const d = win.forwardDialog;
            if (d.ticked.length !== 2 || d.planPending || !d.canGo) return win.later("the plan of the last tick");
            d.commentField.text = "Forwarded in a test";
            win.noticesBefore = win.noticeCount;
            d.go();
            return win.next("went1");
        }
        case "went1": {
            const d = win.forwardDialog;
            if (d.sends !== 1) return win.fail("Forward handed " + d.sends + " jobs to Rust");
            if (d.opened || d.visible) return win.later("the dialog to close");
            if (App.timeline.selecting) return win.later("the selection to end");
            if (win.noticeCount <= win.noticesBefore) return win.later("the end notice");
            if (win.noticeText.indexOf("Forwarded") < 0) return win.fail("the notice: " + win.noticeText);
            return win.next("menu1");
        }
        // The message menu's Forward… (the core's `actions.forward`).
        case "menu1": {
            win.openConversation(CONVA);
            return win.next("menu2");
        }
        case "menu2": {
            if (App.timeline.conversationId !== CONVA) return win.later("chat A");
            const item = win.find(win.contentItem, c => c.messageId === RUNA[0] && c.visible && c.width > 50);
            if (!item) return win.later("the message row");
            const bubble = win.find(item, c => c.objectName === "bubble");
            if (!bubble) return win.fail("the row has no bubble");
            win.ev.mouseClick(bubble, bubble.width - 3, bubble.height - 3, Qt.RightButton, Qt.NoModifier, -1);
            return win.next("menu3");
        }
        case "menu3": {
            const menu = win.chatPage.timeline.messageMenu;
            if (!menu || !menu.opened) return win.later("the message menu to open");
            if (menu.offers.forward !== true) return win.fail("the row's actions: " + JSON.stringify(menu.offers));
            let entry = null;
            for (let i = 0; i < menu.count; ++i) {
                const it = menu.itemAt(i);
                if (it && it.objectName === "menuForward") entry = it;
            }
            if (!entry || !entry.visible) return win.fail("the menu has no visible Forward… entry");
            if (entry.text !== "Forward…") return win.fail("the entry's text: " + entry.text);
            entry.triggered();
            return win.next("menu4");
        }
        case "menu4": {
            const d = win.forwardDialog;
            if (!d.opened || d.view.title !== "Forward Message") return win.later("the dialog of one message: " + d.view.title);
            d.close();
            return win.next("card1");
        }
        // The card: stage into chat B, Enter with a comment sends, the X drops.
        case "card1": {
            if (win.forwardDialog.opened || win.forwardDialog.visible) return win.later("the dialog to close");
            win.openConversation(CONVB);
            return win.next("card2");
        }
        case "card2": {
            if (App.timeline.conversationId !== CONVB) return win.later("chat B");
            if (win.chatPage.composer.forwardCard.standing) return win.fail("a card stands before anything was staged");
            if (!App.composer.stageForward(CONVB, CONVA, RUNA)) return win.fail("stageForward answered false");
            return win.next("card3");
        }
        case "card3": {
            const card = win.chatPage.composer.forwardCard;
            if (!card.standing || !card.visible) return win.later("the card");
            if (card.staged.count !== RUNA.length) return win.fail("the card's count: " + JSON.stringify(card.staged));
            if (!win.chatPage.composer.canSend) return win.fail("Send is off with a forward staged and no text");
            win.clickItem(card.cancelButton);
            return win.next("card4");
        }
        case "card4": {
            const card = win.chatPage.composer.forwardCard;
            if (card.standing) return win.later("the X to drop the forward");
            if (win.chatPage.composer.canSend) return win.fail("Send is on after the drop");
            if (!App.composer.stageForward(CONVB, CONVA, RUNA)) return win.fail("stageForward answered false the second time");
            return win.next("card5");
        }
        case "card5": {
            const composer = win.chatPage.composer;
            if (!composer.forwardCard.standing) return win.later("the card again");
            // Escape in the text area drops it, too.
            composer.textArea.forceActiveFocus();
            win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1);
            return win.next("card6");
        }
        case "card6": {
            const composer = win.chatPage.composer;
            if (composer.forwardCard.standing) return win.later("Escape to drop the forward");
            if (!App.composer.stageForward(CONVB, CONVA, RUNA)) return win.fail("stageForward answered false the third time");
            return win.next("card7");
        }
        case "card7": {
            const composer = win.chatPage.composer;
            if (!composer.forwardCard.standing) return win.later("the card for the send");
            composer.textArea.forceActiveFocus();
            composer.textArea.text = "With a comment";
            win.noticesBefore = win.noticeCount;
            win.ev.keyClick(Qt.Key_Return, Qt.NoModifier, -1);
            return win.next("card8");
        }
        case "card8": {
            const composer = win.chatPage.composer;
            if (composer.forwardCard.standing) return win.later("the forward to go");
            if (composer.text !== "") return win.fail("the comment stays in the text area: " + composer.text);
            if (win.noticeCount <= win.noticesBefore) return win.later("the end notice");
            win.pjRun = 0;
            return win.next("pjOpen");
        }
        // The picture look (F7): three jobs with Look "As Picture" to a chat of another account, the picture item
        // draws the rows outside the window and cuts them: the whole at the 4000 px limit, then at a limit just above
        // the tallest row (cut between rows), then at 60 px (rows cut inside themselves).
        case "pjOpen": {
            win.openConversation(CONVA);
            return win.next("pjSelect");
        }
        case "pjSelect": {
            if (App.timeline.conversationId !== CONVA) return win.later("chat A");
            if (win.forwardDialog.opened || win.forwardDialog.visible) return win.later("the dialog to close");
            if (!App.forwardOpen(RUNA)) return win.fail("forwardOpen answered false");
            return win.next("pjTick");
        }
        case "pjTick": {
            const d = win.forwardDialog;
            if (!d.opened || App.forwardTargets.count < 2 || d.planPending) return win.later("the dialog");
            for (let i = 0; i < App.forwardTargets.count; ++i) {
                const id = App.forwardTargets.conversationAt(i);
                if (!App.forwardTargets.isNative(id)) { win.other = id; }
            }
            d.toggle(win.other);
            return win.next("pjGo");
        }
        case "pjGo": {
            const d = win.forwardDialog;
            if (d.ticked.length !== 1 || d.planPending || !d.canGo) return win.later("the plan of the tick");
            if (!d.lookBox.visible) return win.fail("the Look is hidden for a chat of another account");
            d.lookBox.currentIndex = 3;
            const item = win.forwardPictureItem;
            if (item.rendering) return win.fail("the picture item renders before any job");
            if (item.visible) return win.fail("the picture item is visible at rest");
            const limits = [4000, win.pjTall + 8, 60];
            item.maxSlice = limits[win.pjRun];
            win.pjLimit = limits[win.pjRun];
            win.noticesBefore = win.noticeCount;
            d.go();
            return win.next("pjDrawn");
        }
        case "pjDrawn": {
            const item = win.forwardPictureItem;
            if (item.lastPaths.length === 0 || item.rendering) return win.later("the slices");
            const place = item.mapToItem(null, 0, 0);
            if (place.x + item.width > 0) return win.fail("the picture item is inside the window: " + place.x);
            const sizes = item.lastSizes;
            const slices = item.lastSlices;
            const bounds = item.lastBounds;
            if (sizes.length !== item.lastPaths.length || slices.length !== sizes.length) return win.fail("sizes " + JSON.stringify(sizes) + " slices " + JSON.stringify(slices));
            const ratio = item.ratio;
            let sum = 0;
            for (let i = 0; i < sizes.length; ++i) {
                if (sizes[i] > win.pjLimit) return win.fail("slice " + i + " is " + sizes[i] + " px, over " + win.pjLimit);
                if (Math.abs(sizes[i] - slices[i] * ratio) > 1) return win.fail("slice " + i + ": " + sizes[i] + " px for " + slices[i]);
                sum += slices[i];
            }
            if (Math.abs(sum - bounds[bounds.length - 1]) > 1) return win.fail("the slices add up to " + sum + " of " + bounds[bounds.length - 1]);
            if (item.lastPaths[0].slice(-6) !== "01.png") return win.fail("the first file: " + item.lastPaths[0]);
            if (win.pjRun === 0) {
                if (slices.length !== 1) return win.fail("a short selection gives " + slices.length + " slices");
                let tall = 0;
                for (let i = 1; i < bounds.length; ++i) tall = Math.max(tall, bounds[i] - bounds[i - 1]);
                win.pjTall = tall;
                win.pjTotal = bounds[bounds.length - 1];
            } else if (win.pjRun === 1) {
                if (win.pjTotal <= win.pjLimit) return win.fail("the selection is not taller than the limit");
                if (slices.length < 2) return win.fail("no cut at " + win.pjLimit + " for " + win.pjTotal);
                let at = 0;
                for (let i = 0; i < slices.length - 1; ++i) {
                    at += slices[i];
                    if (!bounds.some(b => Math.abs(b - at) <= 1)) return win.fail("a cut at " + at + " is not between rows: " + JSON.stringify(bounds));
                }
            } else {
                if (slices.length < 3) return win.fail("only " + slices.length + " slices at 60 px");
            }
            return win.next("pjEnded");
        }
        case "pjEnded": {
            if (win.noticeCount <= win.noticesBefore) return win.later("the end notice");
            win.pjRun += 1;
            if (win.pjRun < 3) return win.next("pjOpen");
            Qt.exit(0);
            return;
        }
        }
    }

    Timer {
        interval: 280000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn main() -> ExitCode {
    support::answer_list("forward_ui");
    match std::env::var_os(CHILD) {
        Some(base) => child(Path::new(&base)),
        None => parent(),
    }
}

/// Makes the private directories, starts the run in a child process and says how it went.
fn parent() -> ExitCode {
    let base = std::env::temp_dir().join(format!("kourier-forward-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for dir in ["home", "config", "src"] {
        std::fs::create_dir_all(base.join(dir)).expect("a directory");
    }
    std::fs::write(base.join("config/kdeglobals"), "[KDE]\n").expect("kdeglobals");
    let exe = std::env::current_exe().expect("this test's binary");
    let out = std::process::Command::new(exe)
        .args(std::env::args().skip(1))
        .env(CHILD, &base)
        .env("HOME", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("PULSE_SERVER", "unix:/nonexistent/pulse")
        .env("PIPEWIRE_REMOTE", "/nonexistent/pipewire")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run the test again as a child");
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    let errors = support::script_errors(&said);
    let _ = std::fs::remove_dir_all(&base);
    if out.status.success() && errors.is_empty() {
        println!(
            "test Forward through the window (bar, dialog, ticks, look, menu, card) ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "test Forward through the window ... FAILED ({})",
        out.status
    );
    let (why, rest): (Vec<&str>, Vec<&str>) = said
        .lines()
        .partition(|l| l.contains("FAIL") || l.contains("forward_ui:"));
    for line in why.iter().chain(errors.iter()) {
        println!("{line}");
    }
    for line in rest.iter().rev().take(12).rev() {
        println!("    {line}");
    }
    println!("\ntest result: FAILED. 0 passed; 1 failed");
    ExitCode::FAILURE
}

/// Sends `paths` into `conv` as one request and returns the messages it became.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn send(
    app: &AppHandle,
    conv: ConversationId,
    request: u64,
    paths: Vec<PathBuf>,
    caption: Option<&str>,
) -> Vec<MessageId> {
    let mut events = app.subscribe();
    app.send(UiCommand::SendFiles {
        conv,
        request: RequestId(request),
        paths: paths.into(),
        as_file: Vec::new(),
        with_location: Vec::new(),
        caption: caption.map(Text::plain),
        reply_to: None,
    })
    .expect("send the files");
    app.runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(240), async {
            loop {
                match events.recv().await {
                    Ok(UiEvent::FilesStored {
                        request: r,
                        messages,
                        ..
                    }) if r == RequestId(request) => return messages,
                    Ok(UiEvent::ActionFailed {
                        action: "SendFiles",
                        ..
                    }) => panic!("the core refused the files"),
                    Ok(_) => {}
                    Err(e) => panic!("events: {e}"),
                }
            }
        })
        .await
        .expect("the files were stored in time")
    })
}

/// The run itself: the app over the fixture with three accounts, the window, the script.
fn child(base: &Path) -> ExitCode {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            accounts: 3,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let chat = fixture.quotes_conversation.expect("the quotes chat");
    let chat_b = fixture.demo_conversation.expect("the demo chat");
    // Two messages of chat A: a picture with a caption each (synthetic bytes).
    let png = kourier_qt::bridge::qobject::sample_png(320, 200, 90);
    std::fs::write(base.join("src/One.png"), &png).expect("the picture");
    std::fs::write(base.join("src/Two.png"), &png).expect("the picture");
    let one = send(
        &handle,
        chat,
        1,
        vec![base.join("src/One.png")],
        Some("Forward test one"),
    );
    let two = send(
        &handle,
        chat,
        2,
        vec![base.join("src/Two.png")],
        Some("Forward test two"),
    );
    let run: Vec<MessageId> = one.into_iter().chain(two).collect();
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let ids = format!(
        "[{}]",
        run.iter()
            .map(|m| m.0.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let script = SCRIPT
        .replace("CONVA", &chat.0.to_string())
        .replace("CONVB", &chat_b.0.to_string())
        .replace("RUNA", &ids);
    for token in ["CONVA", "CONVB", "RUNA"] {
        assert!(
            !script.contains(token),
            "token {token} is left in the script"
        );
    }
    let watch = watch_pictures(app.paths().media_dir().join("paste"));
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/forward_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    let seen = watch.finish();
    app.shutdown();
    let mut code = code;
    if code == 0 {
        // The picture look's folders: private (0700) while the job runs, files `01.png`, `02.png` …, one folder
        // per request, and none left once the jobs ended.
        let problem = if seen.modes.len() < 3 {
            Some(format!(
                "{} picture folders seen, 3 jobs ran",
                seen.modes.len()
            ))
        } else if seen.modes.iter().any(|m| *m != 0o700) {
            Some(format!("folder modes {:?}", seen.modes))
        } else if !seen.names.contains("01.png") || !seen.names.contains("02.png") {
            Some(format!("file names {:?}", seen.names))
        } else if !seen.left.is_empty() {
            Some(format!("{} folders left after the end", seen.left.len()))
        } else if !seen.copied {
            Some("no slice picture was copied".to_owned())
        } else {
            None
        };
        if let Some(p) = problem {
            eprintln!("FAIL [folders]: {p}");
            code = 1;
        }
    }
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What the watcher of the picture look's folders saw.
struct Seen {
    modes: Vec<u32>,
    names: std::collections::BTreeSet<String>,
    left: Vec<PathBuf>,
    copied: bool,
}

struct Watch {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Seen>,
}

impl Watch {
    fn finish(self) -> Seen {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.thread.join().expect("the watcher")
    }
}

/// Polls the tmpfs cache for the `fwd-*` folders of the picture look while the window runs: their mode, their file
/// names, and a copy of the first complete `01.png` (the slice picture for the report, `target/shots`).
fn watch_pictures(cache: PathBuf) -> Watch {
    use std::os::unix::fs::PermissionsExt;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let out = std::env::var_os("KOURIER_FORWARD_PICTURE_OUT").map_or_else(
            || Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/shots/forward-picture.png"),
            PathBuf::from,
        );
        let folders = |cache: &Path| -> Vec<PathBuf> {
            std::fs::read_dir(cache)
                .map(|d| {
                    d.filter_map(Result::ok)
                        .map(|e| e.path())
                        .filter(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .is_some_and(|n| n.starts_with("fwd-"))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut seen = Seen {
            modes: Vec::new(),
            names: std::collections::BTreeSet::new(),
            left: Vec::new(),
            copied: false,
        };
        let mut known: Vec<PathBuf> = Vec::new();
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            for dir in folders(&cache) {
                if !known.contains(&dir)
                    && let Ok(meta) = std::fs::metadata(&dir)
                {
                    seen.modes.push(meta.permissions().mode() & 0o777);
                    known.push(dir.clone());
                }
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for e in entries.filter_map(Result::ok) {
                        seen.names
                            .insert(e.file_name().to_string_lossy().into_owned());
                    }
                }
                let first = dir.join("01.png");
                if !seen.copied
                    && let Ok(bytes) = std::fs::read(&first)
                    && bytes.ends_with(b"IEND\xaeB`\x82")
                {
                    if let Some(parent) = out.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    seen.copied = std::fs::write(&out, bytes).is_ok();
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        seen.left = folders(&cache);
        seen
    });
    Watch { stop, thread }
}
