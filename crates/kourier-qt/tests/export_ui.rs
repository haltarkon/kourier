//! Export Selected through the real main window over the real core (EXP-ui 3; offscreen, silent,
//! `harness = false`). The native file dialog cannot be driven offscreen, so the window loads a
//! stand-in for `dialogs/SaveDialog.qml` (the `save_ui.rs` contract: `KOURIER_DEV_SAVE_DIALOG`, read
//! in a debug build only); the script answers it like the user would. Everything else is the real
//! thing: the selection, the bar's Export Selected…, the plan, the dialog, the documents, the
//! writer and the end notice.
//!
//! The writer waits 2.5 s before each file entry (`FixtureOptions::export_pause`), so an export can
//! be HELD running while the script works. One window, these cases in order:
//!
//! - `export`: a caption, a picture and a video are selected, Videos is unticked (the dialog's
//!   numbers follow the click), Export… is pressed, the stand-in answers a file in a fresh folder;
//!   after the end notice the test reads the .zip back (entries in order: files, `index.html`,
//!   `messages.jsonl`, `manifest.json` last; no video; the manifest's hashes against the bytes;
//!   one record per message; an inert `index.html`); the selection ended and the composer is back.
//! - the second click: a second REAL click on Export Selected… in the same window opens the dialog.
//! - `switch`: an export held running in chat A, chat B opened: the window's strip still shows, B's
//!   bar has its own counter, Copy as Text on, Export Selected… off ("An export is running."); B's
//!   selection survives A's end.
//! - `stripcancel`: the strip's Cancel ends the export with the cancelled notice, A's selection
//!   stays, nothing is written.
//! - `savebusy`: with the export's save dialog standing an attachment's Save is refused with a
//!   notice, the export's dialog still stands and its answer starts the export.

mod support;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{RequestId, UiCommand, UiEvent};
use kourier_app::{App, AppHandle, FixtureOptions, Launch, StartOptions};
use kourier_model::{ConversationId, MessageId, Text};
use kourier_qt::{UiContext, UiOptions};
use sha2::{Digest as _, Sha256};

const CHILD: &str = "KOURIER_EXPORT_UI_CHILD";

/// The stand-in for `dialogs/SaveDialog.qml`: the same properties and signals, nothing on screen.
const DIALOG: &str = r#"import QtQuick

Item {
    property bool folder: false
    property url suggested
    property url startFolder
    property bool autoOpen: true
    property var attachments: []

    signal picked(url target)
    signal closed
}
"#;

const SCRIPT: &str = r##"
import QtQuick
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

    // The stand-in answers the save dialog the export opened with a path, as the user would.
    function answer(path: string): void {
        const dlg = win.exportSaving;
        dlg.picked("file://" + encodeURI(path));
        dlg.closed();
    }

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

    function tick(): void {
        switch (win.step) {
        case "start": {
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            if (!win.footer || win.footer.height !== 0) return win.fail("the hidden export strip is " + (win.footer ? win.footer.height : "no") + " px high: the side panel loses that room");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "export_ui_events.qml");
            win.openConversation(CONVA);
            return win.next("select1");
        }
        case "select1": {
            if (App.timeline.conversationId !== CONVA) return win.later("the chat");
            win.select(RUNA);
            return win.next("bar1");
        }
        case "bar1": {
            if (!App.timeline.selecting || App.timeline.selectedCount !== RUNA.length) return win.later("the selection: " + App.timeline.selectedCount);
            const button = win.named("selectionExport");
            if (!button || !button.enabled) return win.later("the Export Selected… button");
            win.noticesBefore = win.noticeCount;
            win.clickItem(button);
            return win.next("dialog1");
        }
        case "dialog1": {
            const d = win.exportDialog;
            if (!d || !d.opened) return win.later("the export dialog");
            if (d.numbers.pictures.count !== 1 || d.numbers.videos.count !== 1) return win.fail("the numbers: " + JSON.stringify(d.numbers));
            if (!d.videosBox.checked) return win.fail("Videos starts ticked");
            const before = d.numbers.totalSize;
            win.clickItem(d.videosBox);
            if (d.videosBox.checked) return win.fail("the click did not untick Videos");
            if (d.numbers.totalSize === before || d.shownMessages !== 1) return win.fail("the numbers did not follow the click: " + JSON.stringify(d.numbers));
            if (!d.picturesBox.checked) return win.fail("Pictures stays ticked");
            d.accept();
            return win.next("saving1");
        }
        case "saving1": {
            if (win.exportSaving === null) return win.later("the stand-in save dialog");
            const suggested = win.exportSaving.suggested.toString();
            if (!suggested.endsWith(".zip") || win.exportSaving.folder) return win.fail("the save dialog was opened for " + suggested);
            win.answer(OUT1);
            return win.next("ended1");
        }
        case "ended1": {
            if (win.noticeCount <= win.noticesBefore || App.exportBusy) return win.later("the end notice");
            if (win.noticeText.indexOf("Exported ") !== 0) return win.fail("the notice: " + win.noticeText);
            if (win.noticeAction !== "Show in Folder") return win.fail("the notice's button: " + win.noticeAction);
            if (App.timeline.selecting) return win.fail("the selection did not end");
            if (win.named("selectionBar")) return win.fail("the bar is still where the composer belongs");
            win.select(RUNA);
            return win.next("again2");
        }
        // THE SECOND CLICK: the end notice may lie over the bar. The item that takes the click
        // is looked up first; a real click must then open the dialog again.
        case "again2": {
            if (!App.timeline.selecting || App.timeline.selectedCount !== RUNA.length) return win.later("the selection again");
            const button = win.named("selectionExport");
            if (!button || !button.enabled) return win.later("the Export Selected… button again");
            const hit = win.hitAt(button);
            if (!win.within(hit, button)) {
                // Not ours: the notice. It is hidden, as a person waits for it to go.
                win.hidePassiveNotification();
                win.hitNote = "the click went to " + (hit ? hit.objectName + "/" + hit : "nothing");
                return win.next("again2b");
            }
            win.hitNote = "";
            win.clickItem(button);
            return win.next("again3");
        }
        case "again2b": {
            if (win.waits < 15) return win.later("the notice to go");
            const button = win.named("selectionExport");
            const hit = win.hitAt(button);
            if (!win.within(hit, button)) return win.fail(win.hitNote + " and it stays");
            win.clickItem(button);
            return win.next("again3");
        }
        case "again3": {
            const d = win.exportDialog;
            if (!d || !d.opened) return win.later("a second real click opened no dialog (" + win.hitNote + ")");
            d.close();
            return win.next("again4");
        }
        case "again4": {
            if (win.exportDialog.opened || win.exportDialog.visible) return win.later("the dialog to close");
            if (App.exportBusy) return win.fail("the closed dialog left the flow busy");
            win.afterPick = "sw1";
            return win.next("pick");
        }
        // Export Selected… from the bar to the stand-in save dialog (the case answers it).
        case "pick": {
            if (!App.timeline.selecting || App.timeline.selectedCount < 1) return win.later("the selection");
            const button = win.named("selectionExport");
            if (!button || !button.enabled) return win.later("the Export Selected… button");
            win.hidePassiveNotification();
            // The end notice may still lie over the bar: wait until the click would reach it.
            if (!win.within(win.hitAt(button), button)) return win.later("the notice to go off the button");
            win.clickItem(button);
            return win.next("pickdialog");
        }
        case "pickdialog": {
            const d = win.exportDialog;
            if (!d || !d.opened) return win.later("the export dialog");
            if (d.shownMessages !== 1) return win.fail("the dialog's messages: " + JSON.stringify(d.numbers));
            d.accept();
            return win.next("picksave");
        }
        case "picksave": {
            if (win.exportSaving === null) return win.later("the stand-in save dialog");
            return win.next(win.afterPick);
        }
        // SWITCH: an export held running in A, chat B opened.
        case "sw1": {
            win.noticesBefore = win.noticeCount;
            win.answer(OUT2);
            return win.next("sw2");
        }
        case "sw2": {
            if (!App.exportBusy || !win.named("exportStrip")) return win.later("the strip");
            win.openConversation(CONVB);
            return win.next("sw3");
        }
        case "sw3": {
            if (App.timeline.conversationId !== CONVB) return win.later("chat B");
            if (App.timeline.selecting) return win.fail("A's selection followed the person into B");
            win.select(RUNB);
            return win.next("sw4");
        }
        case "sw4": {
            if (!App.timeline.selecting || App.timeline.selectedCount !== 1) return win.later("B's selection");
            if (!App.exportBusy) return win.fail("A's export ended before the checks: the hold is too short");
            for (const name of ["exportStrip", "exportStripLabel", "exportStripBar", "exportStripCancel"]) {
                if (!win.named(name)) return win.fail("no " + name + " in chat B");
            }
            if (win.footer.height <= 0) return win.fail("the running export's strip has no height");
            const counter = win.named("selectionCounter");
            if (!counter || counter.text !== "1 selected") return win.fail("B's counter: " + (counter ? counter.text : "none"));
            const copy = win.named("selectionCopy");
            if (!copy || !copy.enabled) return win.fail("Copy as Text is off in B");
            const button = win.named("selectionExport");
            if (!button || button.enabled) return win.fail("Export Selected… is on in B while A's export runs");
            if (button.toolTipText !== "An export is running.") return win.fail("the tooltip: " + button.toolTipText);
            return win.next("sw5");
        }
        case "sw5": {
            if (win.noticeCount <= win.noticesBefore || App.exportBusy) return win.later("A's end");
            if (win.noticeText.indexOf("Exported ") !== 0) return win.fail("A's notice: " + win.noticeText);
            if (win.named("exportStrip")) return win.fail("the strip stays after the end");
            if (win.footer.height !== 0) return win.fail("the strip keeps " + win.footer.height + " px after the end");
            if (!App.timeline.selecting || App.timeline.selectedCount !== 1) return win.fail("A's end changed B's selection");
            App.timeline.selectClear();
            win.openConversation(CONVA);
            return win.next("sc0");
        }
        // STRIPCANCEL: the strip's Cancel ends the export, A's selection stays.
        case "sc0": {
            if (App.timeline.conversationId !== CONVA) return win.later("chat A");
            win.select(RUNA);
            win.afterPick = "sc1";
            return win.next("pick");
        }
        case "sc1": {
            win.answer(OUT3);
            return win.next("sc2");
        }
        case "sc2": {
            if (!App.exportBusy || !win.named("exportStripCancel")) return win.later("the strip");
            if (win.waits < 25) return win.later("the writer to be waiting");
            win.noticesBefore = win.noticeCount;
            win.clickItem(win.named("exportStripCancel"));
            return win.next("sc3");
        }
        case "sc3": {
            if (win.noticeCount <= win.noticesBefore || App.exportBusy) return win.later("the cancelled notice");
            if (win.noticeText !== "Export cancelled. Nothing was saved.") return win.fail("the notice: " + win.noticeText);
            if (!App.timeline.selecting || App.timeline.selectedCount !== RUNA.length) return win.fail("the cancel ended A's selection");
            if (win.named("exportStrip")) return win.fail("the strip stays after the cancel");
            win.afterPick = "sb1";
            return win.next("pick");
        }
        // SAVEBUSY: the export's save dialog stands, an attachment's Save is refused.
        case "sb1": {
            const dlg = win.exportSaving;
            win.noticesBefore = win.noticeCount;
            win.startSave([1]);
            if (win.noticeCount !== win.noticesBefore + 1 || win.noticeText !== "Finish or cancel the export first.") return win.fail("no refusal: " + win.noticeText);
            if (win.exportSaving !== dlg || win.saveDialog !== dlg) return win.fail("the export's save dialog was replaced");
            win.answer(OUT4);
            return win.next("sb2");
        }
        case "sb2": {
            if (!App.exportBusy || !win.named("exportStripCancel")) return win.later("the export to start");
            if (win.waits < 25) return win.later("the writer to be waiting");
            win.noticesBefore = win.noticeCount;
            win.clickItem(win.named("exportStripCancel"));
            return win.next("sb3");
        }
        case "sb3": {
            if (win.noticeCount <= win.noticesBefore || App.exportBusy) return win.later("the end");
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
    support::answer_list("export_ui");
    match std::env::var_os(CHILD) {
        Some(base) => child(Path::new(&base)),
        None => parent(),
    }
}

/// Makes the private directories, starts the run in a child process and says how it went.
fn parent() -> ExitCode {
    let base = std::env::temp_dir().join(format!("kourier-export-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for dir in ["home", "config", "out1", "out2", "out3", "out4", "src"] {
        std::fs::create_dir_all(base.join(dir)).expect("a directory");
    }
    std::fs::write(base.join("config/kdeglobals"), "[KDE]\n").expect("kdeglobals");
    std::fs::write(base.join("SaveDialog.qml"), DIALOG).expect("the stand-in dialog");
    let exe = std::env::current_exe().expect("this test's binary");
    let out = std::process::Command::new(exe)
        .args(std::env::args().skip(1))
        .env(CHILD, &base)
        .env("HOME", base.join("home"))
        .env("XDG_CONFIG_HOME", base.join("config"))
        .env("KOURIER_DEV_SAVE_DIALOG", base.join("SaveDialog.qml"))
        // This test shows a window: always offscreen, whatever the session's platform is.
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        // No sound, whatever the machine has.
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
            "test Export Selected through the window (export, second click, switch, stripcancel, savebusy) ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "test Export Selected through the window ... FAILED ({})",
        out.status
    );
    let (why, rest): (Vec<&str>, Vec<&str>) = said
        .lines()
        .partition(|l| l.contains("FAIL") || l.contains("export_ui:"));
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

/// The run itself: the app over the fixture, the window, the script; then the folders.
fn child(base: &Path) -> ExitCode {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            // The writer waits before each file entry: an export can be held running.
            export_pause: Some(Duration::from_millis(2500)),
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let chat = fixture.quotes_conversation.expect("the quotes chat");

    // Run 1's messages: a caption, a picture and a video (synthetic bytes: a PNG and an MP4 head).
    let src = base.join("src");
    let png = kourier_qt::bridge::qobject::sample_png(320, 200, 90);
    std::fs::write(src.join("Field.png"), &png).expect("the picture");
    let mut mp4 = b"\0\0\0\x18ftypisom\0\0\x02\0isomiso2".to_vec();
    mp4.extend((0..4096u32).map(|i| (i % 251) as u8));
    std::fs::write(src.join("Clip.mp4"), &mp4).expect("the video");
    let chat_b = fixture.demo_conversation.expect("the demo chat");
    std::fs::write(src.join("Other.png"), &png).expect("the picture of chat B");
    let run_b = send(&handle, chat_b, 2, vec![src.join("Other.png")], None);
    let run1 = send(
        &handle,
        chat,
        1,
        vec![src.join("Field.png"), src.join("Clip.mp4")],
        Some("Export test note"),
    );
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let ids = |v: &[MessageId]| {
        format!(
            "[{}]",
            v.iter()
                .map(|m| m.0.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let path = |name: &str| base.join(name).to_str().expect("a UTF-8 path").to_owned();
    let script = SCRIPT
        .replace("CONVA", &chat.0.to_string())
        .replace("RUNA", &ids(&run1))
        .replace("CONVB", &chat_b.0.to_string())
        .replace("RUNB", &ids(&run_b))
        .replace("OUT1", &format!("{:?}", path("out1/run1.zip")))
        .replace("OUT2", &format!("{:?}", path("out2/run2.zip")))
        .replace("OUT3", &format!("{:?}", path("out3/run3.zip")))
        .replace("OUT4", &format!("{:?}", path("out4/run4.zip")));
    for token in [
        "CONVA", "CONVB", "RUNA", "RUNB", "OUT1", "OUT2", "OUT3", "OUT4",
    ] {
        assert!(
            !script.contains(token),
            "token {token} is left in the script"
        );
    }
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/export_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code != 0 {
        return ExitCode::FAILURE;
    }
    match look_at_the_folders(base, 1) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("FAIL [folders]: export_ui: {why}");
            ExitCode::FAILURE
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The archive and the folders, as another program would see them.
fn look_at_the_folders(base: &Path, run1_messages: usize) -> Result<(), String> {
    // The held export of `switch` wrote its archive (a picture and a video); `stripcancel` and
    // `savebusy` were cancelled and left their folders empty.
    let held = std::fs::read_dir(base.join("out2"))
        .map_err(|e| e.to_string())?
        .count();
    if held != 1 {
        return Err(format!(
            "the folder of the held export holds {held} entries"
        ));
    }
    for dir in ["out3", "out4"] {
        let n = std::fs::read_dir(base.join(dir))
            .map_err(|e| e.to_string())?
            .count();
        if n != 0 {
            return Err(format!("a cancelled export left {n} entries in {dir}"));
        }
    }
    // Run 1: the one archive.
    let names: Vec<String> = std::fs::read_dir(base.join("out1"))
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if names != ["run1.zip"] {
        return Err(format!("the folder of run 1 holds {} entries", names.len()));
    }
    let file = std::fs::File::open(base.join("out1/run1.zip")).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let order: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).map(|f| f.name().unwrap().into_owned()))
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    let tail: Vec<&str> = order
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(String::as_str)
        .collect();
    // The order of docs/export-format.md: files, `index.html`, `messages.jsonl`, `manifest.json`.
    if tail != ["index.html", "messages.jsonl", "manifest.json"] {
        return Err(format!("the last entries are {tail:?}"));
    }
    let files = &order[..order.len() - 3];
    if files.is_empty() || files.iter().any(|n| !n.starts_with("files/")) {
        return Err("the entries before index.html are not all files/".into());
    }
    if files.iter().any(|n| {
        let n = n.to_ascii_lowercase();
        n.ends_with(".mp4") || n.ends_with(".mov") || n.ends_with(".webm")
    }) {
        return Err("a video is in the archive although Videos was unticked".into());
    }
    let mut read = |name: &str| -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        zip.by_name(name)
            .map_err(|e| e.to_string())?
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    };
    // The manifest's hashes against what the archive holds.
    let manifest: serde_json::Value =
        serde_json::from_slice(&read("manifest.json")?).map_err(|e| e.to_string())?;
    let entries = manifest["entries"].as_array().ok_or("no entries")?;
    if entries.len() != order.len() - 1 {
        return Err(format!(
            "the manifest lists {} entries for {} others",
            entries.len(),
            order.len() - 1
        ));
    }
    for entry in entries {
        let path = entry["path"].as_str().ok_or("an entry without a path")?;
        let bytes = read(path)?;
        if entry["size"].as_u64() != Some(bytes.len() as u64) {
            return Err("an entry's size differs from its bytes".into());
        }
        if entry["sha256"].as_str() != Some(hex(&Sha256::digest(&bytes)).as_str()) {
            return Err("an entry's hash differs from its bytes".into());
        }
    }
    // One record per exported message, one JSON object per line.
    let lines = String::from_utf8(read("messages.jsonl")?).map_err(|e| e.to_string())?;
    let records = lines
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // The caption rides on the picture's message: the selection holds ONE message.
    if records.len() != run1_messages {
        return Err(format!(
            "{} records for {run1_messages} selected messages",
            records.len()
        ));
    }
    // An inert page: no script, and a `src` only inside files/.
    let html = String::from_utf8(read("index.html")?).map_err(|e| e.to_string())?;
    let lower = html.to_ascii_lowercase();
    if lower.contains("<script") {
        return Err("index.html holds a script".into());
    }
    for part in lower.split("src=").skip(1) {
        let value = part.trim_start_matches(['"', '\'']);
        if !value.starts_with("files/") {
            return Err("index.html has a src that is not a relative files/ path".into());
        }
    }
    Ok(())
}
