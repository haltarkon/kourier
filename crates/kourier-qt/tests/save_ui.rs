//! Save As, Save All and Show in Folder through the real main window over the real core (F3b,
//! `target/next/archive-findings/F3-download.md`; offscreen, `harness = false`). The native file
//! dialog cannot be driven offscreen, so the window loads a stand-in for `dialogs/SaveDialog.qml`
//! (a file of this test with the same properties and signals, named by the debug-only environment
//! variable `KOURIER_DEV_SAVE_DIALOG` that `App.saveDialogSource()` reads in a debug build and
//! never in a release build): the script reads what the window asked it (the name offered, the
//! folder it starts in) and answers like the user would. Everything else is the real thing: the
//! menu, the hover bar, the viewer, the core's command that writes the file, the notice, its
//! announcement and the button that shows the file in its folder (the desktop's call is replaced by
//! a recorder, `kourier_qt::app::set_folder_shower`).
//!
//! The child process runs with a private home, a private configuration directory (its
//! `user-dirs.dirs` names the Downloads folder, its `kdeglobals` is empty so nothing of the user's
//! desktop leaks in) and two folders the dialog answers with. What it checks, in order:
//!
//! - a downloaded file ("Flight plan.pdf"): its row offers the file-row icon, the hover bar's Save
//!   (the bar stays up while the menu is open) and "Save As…" in the menu; the dialog starts in
//!   Downloads (the first time), offers the sender's file name, and the answer writes the file; the
//!   notice says `Saved “Flight plan.pdf”` with "Show in Folder", it is announced politely, and the
//!   button calls the desktop seam with the file's path;
//! - a picture or file that is NOT downloaded (the chat of attachments that cannot be had) shows
//!   none of the four: not the flag, not the menu entry, not the hover bar's button, not the
//!   file-row icon, and its viewer has no Save button and Ctrl+S does nothing;
//! - an album: "Save All…" asks for a folder, which starts where the last save went; the pictures
//!   land under `<sender> <date> <time>`, ` 2`, ` 3` …; the second run into the same folder never
//!   replaces a file (` (1)`); "Saved %n files";
//! - the viewer of the album saves the picture it shows: Ctrl+S (a key of the viewer) and the
//!   button, each on its own picture;
//! - a Save asked while a dialog is open (`swap*`): the dialog's answer saves the file the DIALOG was
//!   opened for (its `attachments`), and the Save asked meanwhile still gets its own dialog;
//! - the folder of every dialog is looked for off the Qt thread (`App.askSaveFolder`, and the
//!   offered name's `saveSuggested`): the steps that wait for a dialog take that wait;
//! - after the run the files are looked at from outside: names, sizes, no executable bit, nothing
//!   in Downloads.
//!
//! NOT here (said once): the view-once rule. The fixture plays Signal, which maps a view-once
//! message without an attachment, so a window test could not see the rule at work (critic M9); the
//! core's table (`actions::tests`) and `kourier-core/tests/save.rs` (on the fake that plays
//! WhatsApp, where a view-once picture keeps its attachment) carry it.

mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Mutex;

use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

const CHILD: &str = "KOURIER_SAVE_UI_CHILD";

/// A file already in `out3`: the typed name `report:v2.jpg` cleans to its name, but is saved as typed.
const MINE: &str = "report_v2.jpg";
const MINE_BYTES: &[u8] = b"not a picture, the user's own file";

/// The stand-in for `dialogs/SaveDialog.qml`: the same properties and signals, nothing on screen.
const DIALOG: &str = r#"import QtQuick

Item {
    property bool folder: false
    property url suggested
    property url startFolder
    property bool autoOpen: true
    // The attachments the window opened this dialog for.
    property var attachments: []

    signal picked(url target)
    signal closed
}
"#;

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
    property real target: 0
    property int pictures: 0
    property int rounds: 0
    property int dialogsBefore: 0
    property real swapA: 0
    property real swapB: 0

    function fail(msg: string): void {
        console.error("FAIL [" + win.step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            win.fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
    }

    function collect(root: Item, pred: var, out: var): var {
        if (pred(root)) {
            out.push(root);
        }
        for (const child of root.children ?? []) {
            win.collect(child, pred, out);
        }
        return out;
    }

    function find(root: Item, pred: var): Item {
        const all = win.collect(root, pred, []);
        return all.length > 0 ? all[0] : null;
    }

    readonly property var view: win.chatPage ? win.chatPage.timeline : null

    // The visible delegate of a message.
    function delegate(id: real): var {
        return win.view ? win.view.delegateFor(id) : null;
    }

    // The entry of the open message menu, by its object name (a hidden one counts as none).
    function menuEntry(name: string): var {
        const m = win.view.messageMenu;
        for (let i = 0; i < m.count; ++i) {
            const item = m.itemAt(i);
            if (item && item.objectName === name && item.visible) {
                return item;
            }
        }
        return null;
    }

    // The stand-in dialog the window has made, or null.
    function dialog(): var {
        return win.saveDialog;
    }

    // The path a `file:` URL stands for.
    function pathOf(url: string): string {
        return decodeURIComponent(url.toString().replace(/^file:\/\//, ""));
    }

    function pressSave(): void {
        if (!win.ev.keyClick(Qt.Key_S, Qt.ControlModifier, -1)) {
            win.fail("the window took no key event");
        }
    }

    function clickItem(item: Item): void {
        if (!win.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) {
            win.fail("the window took no mouse event");
        }
    }

    // The question the dialog was opened with is answered with `target` (a path), as the user would.
    function answer(dlg: var, path: string): void {
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
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "save_ui_events.qml");
            win.openConversation(MEDIA);
            return win.next("media");
        }
        // ---- a downloaded file: the row's icon, the bar, the menu, the dialog, the notice ----
        case "media": {
            if (win.chatPage === null || App.timeline.conversationId !== MEDIA || App.timeline.loading) return win.later("the media chat");
            const id = App.findMessage("file");
            const d = win.delegate(id);
            if (id <= 0 || !d) return win.later("the row of the file");
            if (d.actions.saveAttachments !== true || d.saveList().length !== 1) return win.fail("a downloaded file row does not offer Save: " + JSON.stringify(d.actions));
            if (!win.find(d, c => c.objectName === "fileSave" && c.visible)) return win.later("the file row's save icon");
            win.target = id;
            win.view.menuFor(id);
            return win.next("fileMenu");
        }
        case "fileMenu": {
            const m = win.view.messageMenu;
            if (!m.visible) return win.later("the row's menu");
            const entry = win.menuEntry("menuSave");
            if (!entry) return win.fail("the menu of a downloaded file offers no Save As…");
            if (entry.text !== "Save As…") return win.fail("the entry says " + entry.text);
            // The bar beside the bubble stays up while the menu is open: its Save button is there.
            const d = win.delegate(win.target);
            const button = win.find(d, c => c.objectName === "saveButton" && c.visible);
            if (!button) return win.later("the hover bar's Save button");
            if (button.text !== "Save As…") return win.fail("the bar's button says " + button.text);
            // The bar's own button asks as well as the menu's entry: this time the BUTTON. (Pressed
            // while the menu is open, which keeps the bar up; the button goes with the bar.)
            win.dialogsBefore = win.saveDialogsOpened;
            button.clicked();
            m.close();
            return win.next("fileDialog");
        }
        case "fileDialog": {
            const dlg = win.dialog();
            if (!dlg || win.saveDialogsOpened <= win.dialogsBefore) return win.later("the dialog (dialogs opened " + win.saveDialogsOpened + ", before " + win.dialogsBefore + ", notices " + win.noticeCount + ", last " + win.noticeText + ", wanted " + JSON.stringify(win.saveWanted) + ", source " + App.saveDialogSource() + ")");
            if (dlg.folder) return win.fail("one file asks for a file, not a folder");
            // The first time: Downloads (the private user-dirs.dirs), the sender's name for the file.
            if (win.pathOf(dlg.suggested) !== DOWNLOADS + "/Flight plan.pdf") return win.fail("the dialog starts at " + win.pathOf(dlg.suggested));
            win.noticesBefore = win.noticeCount;
            win.answer(dlg, OUT1 + "/Flight plan.pdf");
            return win.next("fileSaved");
        }
        case "fileSaved": {
            if (win.noticeCount <= win.noticesBefore) return win.later("the notice after the save");
            if (win.noticeText !== "Saved “Flight plan.pdf”") return win.fail("the notice says " + win.noticeText);
            if (win.noticeAction !== "Show in Folder") return win.fail("the notice's button says " + win.noticeAction);
            if (win.noticeAnnounced !== win.noticeText || win.noticeAnnouncedPoliteness !== Accessible.Polite) return win.fail("the notice was not announced politely: " + win.noticeAnnounced + " / " + win.noticeAnnouncedPoliteness);
            if (typeof win.noticeCallback !== "function") return win.fail("the notice has no action");
            // "Show in Folder": the desktop seam (the test's recorder) gets the saved file.
            win.noticeCallback();
            win.openConversation(REMOTE);
            return win.next("remote");
        }
        // ---- what is not downloaded offers no Save anywhere ----
        case "remote": {
            if (win.chatPage === null || App.timeline.conversationId !== REMOTE || App.timeline.loading) return win.later("the chat of attachments that cannot be had");
            const delegates = win.collect(win.view.rowList.contentItem, c => c.messageId !== undefined && c.saveList !== undefined && c.blocks !== undefined && c.visible, []);
            const remote = delegates.filter(d => d.blocks.some(b => (b.type === "image" || b.type === "file") && b.state === "remote"));
            if (remote.length === 0) return win.later("a row with a picture that is not downloaded");
            for (const d of remote) {
                if (d.actions.saveAttachments === true || d.saveList().length !== 0) return win.fail("a row that is not downloaded offers Save: " + JSON.stringify(d.actions));
            }
            const d = remote[0];
            if (win.find(d, c => c.objectName === "fileSave")) return win.fail("a file that is not downloaded has a save icon");
            win.target = d.messageId;
            win.view.menuFor(d.messageId);
            return win.next("remoteMenu");
        }
        case "remoteMenu": {
            const m = win.view.messageMenu;
            if (!m.visible) return win.later("the menu of the row that is not downloaded");
            if (win.menuEntry("menuSave")) return win.fail("Save As… in the menu of a picture that is not downloaded");
            const d = win.delegate(win.target);
            if (win.find(d, c => c.objectName === "saveButton")) return win.fail("the hover bar offers Save for a picture that is not downloaded");
            m.close();
            // Its viewer: no button, and Ctrl+S saves nothing.
            const items = App.viewerItems(win.target);
            if (items.length === 0 || items[0].save === true) return win.fail("the viewer's items offer Save: " + JSON.stringify(items));
            win.dialogsBefore = win.saveDialogsOpened;
            win.openAlbumViewer(items, 0, win.target);
            return win.next("remoteViewer");
        }
        case "remoteViewer": {
            const v = win.imageViewer;
            if (!v || !v.opened) return win.later("the viewer");
            if (win.find(v.contentItem, c => c.objectName === "viewerSave" && c.visible)) return win.fail("the viewer of a picture that is not downloaded has a Save button");
            win.pressSave();
            return win.next("remoteKey");
        }
        case "remoteKey": {
            // A few ticks for a dialog that would come late.
            if (win.waits < 25) return win.later("");
            if (win.saveDialogsOpened !== win.dialogsBefore) return win.fail("Ctrl+S in the viewer of a picture that is not downloaded opened the dialog");
            win.imageViewer.close();
            win.openConversation(ALBUMS);
            return win.next("albums");
        }
        // ---- an album: Save All… into a folder, twice ----
        case "albums": {
            if (win.chatPage === null || App.timeline.conversationId !== ALBUMS || App.timeline.loading) return win.later("the albums chat");
            if (!win.view.albumsShown()) return win.later("the albums with their pictures");
            const id = App.findMessage("removable");
            const d = win.delegate(id);
            if (id <= 0 || !d) return win.later("the album's row");
            win.pictures = d.saveList().length;
            if (win.pictures < 2 || d.actions.saveAttachments !== true) return win.fail("the album offers " + win.pictures + " pictures to save");
            win.target = id;
            win.rounds = 0;
            win.view.menuFor(id);
            return win.next("allMenu");
        }
        case "allMenu": {
            const m = win.view.messageMenu;
            if (!m.visible) return win.later("the album's menu");
            const entry = win.menuEntry("menuSave");
            if (!entry) return win.fail("the album's menu offers no Save All…");
            if (entry.text !== "Save All…") return win.fail("the entry says " + entry.text);
            const button = win.find(win.delegate(win.target), c => c.objectName === "saveButton" && c.visible);
            if (!button || button.text !== "Save All…") return win.fail("the bar's button for an album: " + (button ? button.text : "none"));
            m.close();
            win.dialogsBefore = win.saveDialogsOpened;
            entry.triggered();
            return win.next("allDialog");
        }
        case "allDialog": {
            const dlg = win.dialog();
            if (!dlg || win.saveDialogsOpened <= win.dialogsBefore) return win.later("the folder dialog");
            if (!dlg.folder) return win.fail("several files ask for a folder");
            // Where the last save went (the file from before), then twice the same folder.
            const want = win.rounds === 0 ? OUT1 : OUT2;
            if (win.pathOf(dlg.startFolder) !== want) return win.fail("the folder dialog starts at " + win.pathOf(dlg.startFolder) + ", not in the last folder used");
            win.noticesBefore = win.noticeCount;
            win.answer(dlg, OUT2);
            return win.next("allSaved");
        }
        case "allSaved": {
            if (win.noticeCount <= win.noticesBefore) return win.later("the notice after Save All…");
            if (win.noticeText !== "Saved " + win.pictures + " files") return win.fail("the notice says " + win.noticeText);
            if (win.noticeAction !== "Show in Folder") return win.fail("the notice's button says " + win.noticeAction);
            win.rounds += 1;
            if (win.rounds < 2) {
                win.view.menuFor(win.target);
                return win.next("allMenu");
            }
            return win.next("viewerOpen");
        }
        // ---- the viewer saves the picture it shows ----
        case "viewerOpen": {
            const items = App.viewerItems(win.target);
            if (items.length < 2 || items[1].save !== true) return win.fail("the album's viewer items: " + JSON.stringify(items));
            win.openAlbumViewer(items, 1, win.target);
            return win.next("viewerShown");
        }
        case "viewerShown": {
            const v = win.imageViewer;
            if (!v || !v.opened) return win.later("the viewer");
            const button = win.find(v.contentItem, c => c.objectName === "viewerSave" && c.visible);
            if (!button) return win.fail("the viewer has no Save button");
            if (button.QQC2.ToolTip.text.indexOf("Ctrl+S") < 0) return win.fail("the tooltip does not name the key: " + button.QQC2.ToolTip.text);
            win.dialogsBefore = win.saveDialogsOpened;
            win.pressSave();
            return win.next("viewerDialog");
        }
        case "viewerDialog": {
            const dlg = win.dialog();
            if (!dlg || win.saveDialogsOpened <= win.dialogsBefore) return win.later("the dialog after Ctrl+S");
            const name = win.pathOf(dlg.suggested).split("/").pop();
            // The second picture of the album: " 2" before the extension, from the sender.
            if (!/^Sol Vireo \d{4}-\d{2}-\d{2} \d{2}\.\d{2}\.\d{2} 2\.[a-z]+$/.test(name)) return win.fail("the second picture is offered as " + name);
            win.noticesBefore = win.noticeCount;
            win.answer(dlg, OUT1 + "/" + name);
            return win.next("viewerSaved");
        }
        case "viewerSaved": {
            if (win.noticeCount <= win.noticesBefore) return win.later("the notice after Ctrl+S");
            if (win.noticeText.indexOf("Saved “") !== 0) return win.fail("the notice says " + win.noticeText);
            // Then the button, on the first picture.
            win.imageViewer.step(-1);
            return win.next("viewerFirst");
        }
        case "viewerFirst": {
            const v = win.imageViewer;
            if (v.index !== 0) return win.later("the first picture");
            const button = win.find(v.contentItem, c => c.objectName === "viewerSave" && c.visible);
            if (!button) return win.fail("the viewer has no Save button on the first picture");
            win.dialogsBefore = win.saveDialogsOpened;
            win.clickItem(button);
            return win.next("viewerButton");
        }
        case "viewerButton": {
            const dlg = win.dialog();
            if (!dlg || win.saveDialogsOpened <= win.dialogsBefore) return win.later("the dialog after the button");
            const name = win.pathOf(dlg.suggested).split("/").pop();
            if (!/^Sol Vireo \d{4}-\d{2}-\d{2} \d{2}\.\d{2}\.\d{2}\.[a-z]+$/.test(name)) return win.fail("the first picture is offered as " + name);
            win.noticesBefore = win.noticeCount;
            win.answer(dlg, OUT1 + "/" + name);
            return win.next("buttonSaved");
        }
        case "buttonSaved": {
            if (win.noticeCount <= win.noticesBefore) return win.later("the notice after the button");
            win.imageViewer.close();
            return win.next("swapStart");
        }
        // ---- a second Save asked while a dialog is open: the answer is for the dialog's own file ----
        case "swapStart": {
            if (win.imageViewer && win.imageViewer.visible) return win.later("the viewer to close");
            const d = win.delegate(win.target);
            if (!d) return win.later("the album's row");
            const ids = d.saveList();
            if (ids.length < 2) return win.fail("the album offers " + ids.length + " pictures");
            win.swapA = ids[0];
            win.swapB = ids[1];
            win.dialogsBefore = win.saveDialogsOpened;
            win.startSave([win.swapA]);
            return win.next("swapFirst");
        }
        case "swapFirst": {
            const dlg = win.dialog();
            if (!dlg || win.saveDialogsOpened <= win.dialogsBefore) return win.later("the dialog for the first picture");
            if (JSON.stringify(dlg.attachments) !== JSON.stringify([win.swapA])) return win.fail("the dialog was not told which file it is for: " + JSON.stringify(dlg.attachments));
            // He asks for another picture's Save before he answers this dialog, then he answers it.
            win.dialogsBefore = win.saveDialogsOpened;
            win.noticesBefore = win.noticeCount;
            win.startSave([win.swapB]);
            win.answer(dlg, OUT3 + "/first.jpg");
            return win.next("swapSaved");
        }
        case "swapSaved": {
            // The answer saved the file its dialog was opened for ...
            if (win.noticeCount <= win.noticesBefore) return win.later("the notice for the first picture");
            if (win.noticeText !== "Saved “first.jpg”") return win.fail("the notice says " + win.noticeText);
            // ... and the second ask was not used up by it: its own dialog opens.
            const dlg = win.dialog();
            if (!dlg || win.saveDialogsOpened <= win.dialogsBefore) return win.later("the dialog for the second picture");
            if (JSON.stringify(dlg.attachments) !== JSON.stringify([win.swapB])) return win.fail("the second dialog is for " + JSON.stringify(dlg.attachments));
            // He types a name with a colon while a file of the cleaned name is in the folder: the
            // name is saved as he typed it (the dialog asked about THAT name) and the other file,
            // the one the dialog never asked about, is as it was (looked at after the window).
            win.noticesBefore = win.noticeCount;
            win.answer(dlg, OUT3 + "/report:v2.jpg");
            return win.next("swapDone");
        }
        case "swapDone": {
            if (win.noticeCount <= win.noticesBefore) return win.later("the notice for the second picture");
            if (win.noticeText !== "Saved “report:v2.jpg”") return win.fail("the notice says " + win.noticeText);
            return win.next("done");
        }
        case "done":
            if (win.imageViewer && win.imageViewer.visible) return win.later("the viewer to close");
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

/// What "Show in Folder" was asked to show (a recorder in place of the file manager's call).
static SHOWN: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn record_shown(path: &Path) {
    SHOWN.lock().expect("the record").push(path.to_path_buf());
}

fn main() -> ExitCode {
    support::answer_list("save_ui");
    match std::env::var_os(CHILD) {
        Some(base) => child(Path::new(&base)),
        None => parent(),
    }
}

/// Makes the private directories, starts the run in a child process and says how it went.
fn parent() -> ExitCode {
    let base = std::env::temp_dir().join(format!("kourier-save-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for dir in ["home", "config", "Downloads", "out1", "out2", "out3"] {
        std::fs::create_dir_all(base.join(dir)).expect("a directory");
    }
    std::fs::write(base.join("config/kdeglobals"), "[KDE]\n").expect("kdeglobals");
    // A file of the user's that the cleaned form of a typed name would be (swap step).
    std::fs::write(base.join("out3").join(MINE), MINE_BYTES).expect("the user's own file");
    std::fs::write(
        base.join("config/user-dirs.dirs"),
        format!(
            "XDG_DOWNLOAD_DIR=\"{}\"\n",
            base.join("Downloads").to_str().expect("a UTF-8 path")
        ),
    )
    .expect("user-dirs.dirs");
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
        // What the script says when it fails must reach the test's output.
        .env("QT_FORCE_STDERR_LOGGING", "1")
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
            "test saving attachments through the window ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "test saving attachments through the window ... FAILED ({})",
        out.status
    );
    let (why, rest): (Vec<&str>, Vec<&str>) = said
        .lines()
        .partition(|l| l.contains("FAIL") || l.contains("save_ui:"));
    for line in why.iter().chain(errors.iter()) {
        println!("{line}");
    }
    for line in rest.iter().rev().take(12).rev() {
        println!("    {line}");
    }
    println!("\ntest result: FAILED. 0 passed; 1 failed");
    ExitCode::FAILURE
}

/// The run itself: the app over the fixture, the window, the script; then the folders.
fn child(base: &Path) -> ExitCode {
    kourier_qt::app::set_folder_shower(record_shown);
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
    let media = fixture.media_conversation.expect("the media chat");
    let albums = fixture.albums_conversation.expect("the albums chat");
    let remote = fixture
        .unavailable_conversation
        .expect("the chat of attachments that cannot be had");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let path = |name: &str| base.join(name).to_str().expect("a UTF-8 path").to_owned();
    let script = SCRIPT
        .replace("MEDIA", &media.0.to_string())
        .replace("ALBUMS", &albums.0.to_string())
        .replace("REMOTE", &remote.0.to_string())
        .replace("DOWNLOADS", &format!("{:?}", path("Downloads")))
        .replace("OUT1", &format!("{:?}", path("out1")))
        .replace("OUT2", &format!("{:?}", path("out2")))
        .replace("OUT3", &format!("{:?}", path("out3")));
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/save_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    if code != 0 {
        return ExitCode::FAILURE;
    }
    match look_at_the_folders(base) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("FAIL [folders]: save_ui: {why}");
            ExitCode::FAILURE
        }
    }
}

/// The files, as another program would see them.
fn look_at_the_folders(base: &Path) -> Result<(), String> {
    let list = |dir: &str| -> Vec<(String, std::fs::Metadata)> {
        let mut out: Vec<_> = std::fs::read_dir(base.join(dir))
            .expect("a folder")
            .flatten()
            .map(|e| {
                (
                    e.file_name().to_string_lossy().into_owned(),
                    e.metadata().expect("metadata"),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    let names =
        |v: &[(String, std::fs::Metadata)]| v.iter().map(|f| f.0.clone()).collect::<Vec<_>>();
    if !list("Downloads").is_empty() {
        return Err("a save wrote into Downloads, where no one asked".into());
    }
    let out1 = list("out1");
    let out2 = list("out2");
    for (name, meta) in out1.iter().chain(out2.iter()) {
        if meta.permissions().mode() & 0o111 != 0 {
            return Err(format!("a saved file has an executable bit: {name}"));
        }
        if meta.len() < 100 {
            return Err(format!("a saved file is {} bytes: {name}", meta.len()));
        }
        if name.starts_with('.') {
            return Err(format!("a hidden file stayed in the folder: {name}"));
        }
    }
    // The file: the sender's file name, all of its 182,404 bytes (the fixture's synthetic bytes, not
    // zeros).
    let pdf = base.join("out1/Flight plan.pdf");
    let bytes = std::fs::read(&pdf).map_err(|e| e.to_string())?;
    if bytes.len() != 182_404 || bytes.iter().all(|b| *b == 0) {
        return Err(format!(
            "the saved file is {} bytes of the sender's file",
            bytes.len()
        ));
    }
    // The viewer's two pictures: the first without a number, the second with " 2".
    let in_out1: Vec<&String> = out1
        .iter()
        .map(|f| &f.0)
        .filter(|n| n.starts_with("Sol Vireo"))
        .collect();
    if in_out1.len() != 2 || !in_out1.iter().any(|n| n.contains(" 2.")) {
        return Err(format!("the viewer saved {:?}", names(&out1)));
    }
    // The album, twice: n files numbered 1..n, then n more that never replaced them.
    let first: Vec<&String> = out2
        .iter()
        .map(|f| &f.0)
        .filter(|n| !n.contains("(1)"))
        .collect();
    let second: Vec<&String> = out2
        .iter()
        .map(|f| &f.0)
        .filter(|n| n.contains("(1)"))
        .collect();
    if first.len() < 2 || first.len() != second.len() {
        return Err(format!("Save All twice wrote {:?}", names(&out2)));
    }
    for n in 1..=first.len() {
        let suffix = if n == 1 {
            String::new()
        } else {
            format!(" {n}")
        };
        let wanted = |names: &[&String], tail: &str| {
            names.iter().any(|f| {
                f.starts_with("Sol Vireo ") && f.contains(&format!("{suffix}{tail}.")) && {
                    // (the number stands right before the extension: no other digit group follows)
                    f.rsplit_once('.')
                        .is_some_and(|(stem, _)| stem.ends_with(&format!("{suffix}{tail}")))
                }
            })
        };
        if !wanted(&first, "") {
            return Err(format!(
                "picture {n} of the album has no file under its number: {:?}",
                names(&out2)
            ));
        }
        if !wanted(&second, " (1)") {
            return Err(format!(
                "the second Save All did not keep both of picture {n}: {:?}",
                names(&out2)
            ));
        }
    }
    // The two pictures of the swap step: each file holds the picture its own dialog was for (the
    // album's pictures are different images), and nothing else was written there.
    if names(&list("out3")) != ["first.jpg", "report:v2.jpg", MINE] {
        return Err(format!("the swap step wrote {:?}", names(&list("out3"))));
    }
    let first = std::fs::read(base.join("out3/first.jpg")).map_err(|e| e.to_string())?;
    let second = std::fs::read(base.join("out3/report:v2.jpg")).map_err(|e| e.to_string())?;
    if first == second {
        return Err("the first picture's dialog saved the second picture's bytes".into());
    }
    // The name as typed was saved; the file of its cleaned form, which the dialog never asked
    // about, was not replaced (H1 of the review).
    if std::fs::read(base.join("out3").join(MINE)).map_err(|e| e.to_string())? != MINE_BYTES {
        return Err("a typed name replaced the file of its cleaned form".into());
    }
    // "Show in Folder" reached the seam with the file.
    let shown = SHOWN.lock().expect("the record").clone();
    if shown != vec![pdf] {
        return Err("Show in Folder did not ask the desktop to show the saved file".into());
    }
    Ok(())
}
