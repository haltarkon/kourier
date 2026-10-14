//! A folder given to the composer, through the real main window and the real core over the
//! fake network (offscreen, `harness = false`). One session of a user:
//!
//! - The attach button has two entries: files, and a folder.
//! - A folder dropped with a file is not marked as a file that cannot go. It stands in the
//!   list, Kourier counts what it holds and asks: the dialog says how many files and how much
//!   data, and what the walk leaves out (a hidden file, a link to a file and one to a folder).
//! - "Add the Files Inside": the folder's files take its place in the list, in the order of
//!   their paths, subfolders too, without the hidden file and without what the link points
//!   at; the send then carries exactly those files.
//! - Two folders given together are one question. "Send as 2 .zip Files": each becomes one
//!   archive named after it, in the folder's place; the send carries the two archives, and
//!   the archive the core stored holds the folder's files, byte for byte, under relative
//!   paths. Afterwards no archive is left on the disk.
//! - A folder whose files add up to more than the chat takes as one file is not offered as an
//!   archive (checked before anything is packed) and can still be added file by file; Escape
//!   takes it out of the list.
//! - A folder with nothing to send leaves with a notice; a folder with more than a thousand
//!   files is asked about once more before they fill the list; a folder pasted from the
//!   clipboard is asked about like a dropped one; a folder that is removed while Kourier
//!   still looks into it is gone for good.
//! - In a narrow window the note about a file's type is cut at the row's edge, and the
//!   pointer on it shows the whole line in a tooltip.
//!
//! Nothing here opens a file dialog: the picker's two modes are checked by the menu's
//! entries, and what a picked folder does is what a dropped one does.

mod support;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QList, QQmlApplicationEngine, QString, QUrl};
use kourier_app::facade::{
    OriginKind, Query, QueryData, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentId, AttachmentKind, Body, ConversationId};
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
    property real t0: 0
    property int quiet: 0
    property var noteLabel: null

    readonly property var chat: win.chatPage
    readonly property var composerItem: win.chatPage ? win.chatPage.composer : null
    readonly property var dialog: win.composerItem ? win.composerItem.folderDialog : null

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

    // An item of the open question's dialog, by its object name.
    function asked(name: string): Item {
        return win.dialog ? win.find(win.dialog.contentItem, i => i.objectName === name) : null;
    }

    // The question is on screen and counted (every folder that was given).
    function questionShown(): bool {
        return App.composer.folderAsking && win.dialog !== null && win.dialog.opened;
    }

    Instantiator {
        id: fileRows
        model: App.composer.files
        delegate: QtObject {
            required property string name
            required property string kind
            required property string preview
            required property string size
            required property string reason
            required property string note
            required property string stage
            required property real progress
        }
    }

    function names(): string {
        const out = [];
        for (let i = 0; i < fileRows.count; ++i) {
            out.push(fileRows.objectAt(i).name);
        }
        return out.join("|");
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
            case "start":
                if (!win.started || !App.hasAccounts) return win.later("the start page");
                win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "folder_ui_events.qml");
                win.openConversation(MEDIA);
                win.next("chat");
                return;
            case "chat": {
                if (win.chatPage === null || App.timeline.conversationId !== MEDIA || App.timeline.loading || App.timeline.attachKinds.indexOf("file") < 0) return win.later("the media chat");
                // ---- the attach button: files, or a folder ----
                const button = win.find(win.composerItem, i => i.objectName === "attachButton");
                if (!button || !button.visible) return win.fail("the attach button");
                // (The page that was just pushed takes input once it has arrived.)
                if (!button.enabled) return win.later("the composer takes input");
                button.click();
                win.next("menu");
                return;
            }
            case "menu": {
                const menu = win.composerItem.attachMenu;
                if (!menu.opened) return win.later("the attach menu");
                if (menu.count !== 2) return win.fail("entries of the attach menu: " + menu.count);
                if (menu.itemAt(0).text !== "Files…" || menu.itemAt(1).text !== "Folder…") return win.fail("the entries: " + menu.itemAt(0).text + ", " + menu.itemAt(1).text);
                menu.close();
                // ---- a folder dropped together with a file ----
                win.composerItem.attach([COVER, TRIP]);
                // Both stand in the list at once; nothing was read to put them there.
                if (App.composer.attachmentCount !== 2) return win.fail("the list after the drop: " + App.composer.attachmentCount);
                win.next("asked");
                return;
            }
            case "asked": {
                if (!win.questionShown()) return win.later("the question about the folder");
                if (win.composerItem.attachMenu.opened) return win.fail("the menu is still open");
                const cover = fileRows.objectAt(0);
                const trip = fileRows.objectAt(1);
                if (cover.name !== "Cover.txt" || cover.stage !== "" || cover.kind !== "file") return win.fail("the file beside it: " + cover.name + " " + cover.stage);
                // The folder is no file that cannot go: no reason, its own kind, its count.
                if (trip.name !== "Trip" || trip.kind !== "folder" || trip.stage !== "asking") return win.fail("the folder's row: " + trip.kind + " " + trip.stage);
                if (trip.reason !== "") return win.fail("the folder is marked as refused: " + trip.reason);
                if (trip.size.indexOf("3 files · ") !== 0) return win.fail("what the folder holds: " + trip.size);
                if (App.composer.refusedCount !== 0) return win.fail("refused: " + App.composer.refusedCount);
                if (App.composer.summary.indexOf("1 file, 1 folder") !== 0) return win.fail("the summary: " + App.composer.summary);
                if (App.composer.sendableCount !== 1) return win.fail("only the file could go now: " + App.composer.sendableCount);
                // What the dialog says: how many files and how much data, and what is left out.
                if (win.dialog.title !== "Send a Folder") return win.fail("the title: " + win.dialog.title);
                const text = win.asked("folderText").text;
                if (text.indexOf("“Trip” holds 3 files, ") !== 0 || text.indexOf(" in all.") < 0) return win.fail("the question: " + text);
                const skipped = win.asked("folderSkipped");
                if (!skipped.visible || skipped.text !== "Left out: 1 hidden item, 2 links (links are not followed).") return win.fail("what is left out: " + skipped.text);
                const add = win.asked("folderAdd");
                const zip = win.asked("folderZip");
                if (add.text !== "Add the Files Inside" || zip.text !== "Send as One .zip") return win.fail("the answers: " + add.text + " / " + zip.text);
                if (!zip.enabled || win.asked("folderZipHint").text.indexOf("Packed into one file, “Trip.zip”") !== 0) return win.fail("the archive is offered: " + win.asked("folderZipHint").text);
                if (!add.activeFocus) return win.fail("the first answer has the keyboard");
                // ---- "Add the Files Inside" ----
                add.click();
                win.next("added");
                return;
            }
            case "added": {
                if (App.composer.folderAsking || App.composer.attachmentCount !== 4 || !App.composer.planned) return win.later("the folder's files in the list");
                if (win.dialog !== null) return win.later("the dialog goes");
                // In the folder's place, by path, subfolders too; nothing hidden, no link.
                if (win.names() !== "Cover.txt|a.png|b.png|plan.txt") return win.fail("the files: " + win.names());
                for (let i = 0; i < fileRows.count; ++i) {
                    const r = fileRows.objectAt(i);
                    if (r.stage !== "" || r.reason !== "") return win.fail(r.name + ": " + r.stage + " " + r.reason);
                }
                // The walk already knew their sizes and which are pictures.
                const a = fileRows.objectAt(1);
                if (a.size === "" || a.kind !== "image" || a.preview.indexOf("file://") !== 0) return win.fail("a.png: " + a.size + " " + a.kind + " " + a.preview);
                if (App.composer.notice !== "3 files were added from “Trip”. Left out: 1 hidden item, 2 links.") return win.fail("the notice: " + App.composer.notice);
                if (App.composer.sendableCount !== 4 || App.composer.refusedCount !== 0) return win.fail("all four can go");
                win.composerItem.setText("the trip");
                win.composerItem.send();
                if (App.composer.attachmentCount !== 0) return win.fail("the files went with the send");
                win.next("sent");
                return;
            }
            case "sent":
                if (App.composer.preparing !== 0) return win.later("the core stores the send");
                // ---- two folders given together: one question ----
                win.composerItem.attach([TRIP, DOCS]);
                win.next("asked2");
                return;
            case "asked2": {
                if (!win.questionShown()) return win.later("the question about two folders");
                if (win.dialog.title !== "Send 2 Folders") return win.fail("the title: " + win.dialog.title);
                const text = win.asked("folderText").text;
                if (text.indexOf("The 2 folders hold 5 files, ") !== 0) return win.fail("the question: " + text);
                const zip = win.asked("folderZip");
                if (zip.text !== "Send as 2 .zip Files" || !zip.enabled) return win.fail("the archive answer: " + zip.text);
                if (fileRows.objectAt(0).stage !== "asking" || fileRows.objectAt(1).stage !== "asking") return win.fail("both wait for the answer");
                // ---- "Send as .zip" ----
                zip.click();
                win.next("zipped");
                return;
            }
            case "zipped": {
                if (App.composer.folderAsking) return win.later("the answer");
                if (fileRows.count !== 2) return win.fail("two rows while packing: " + fileRows.count);
                const first = fileRows.objectAt(0);
                const second = fileRows.objectAt(1);
                // While a folder is packed its row says so; then the archive stands there.
                for (const r of [first, second]) {
                    if (r.stage !== "" && r.stage !== "packing") return win.fail("a row while packing: " + r.stage);
                    if (r.stage === "packing" && (r.note.indexOf("Packing… ") !== 0 || r.progress < 0 || r.progress > 1)) return win.fail("the packing row: " + r.note + " " + r.progress);
                }
                if (first.stage !== "" || second.stage !== "" || !App.composer.planned) return win.later("both archives");
                if (win.names() !== "Trip.zip|Docs.zip") return win.fail("the archives: " + win.names());
                if (first.kind !== "file" || first.size === "" || first.reason !== "" || second.reason !== "") return win.fail("the archive's row: " + first.kind + " " + first.size + " " + first.reason);
                if (App.composer.sendableCount !== 2) return win.fail("both can go: " + App.composer.sendableCount);
                if (App.composer.notice !== "") return win.fail("a notice without a reason: " + App.composer.notice);
                win.composerItem.setText("zipped");
                win.composerItem.send();
                win.next("sent2");
                return;
            }
            case "sent2":
                if (App.composer.attachmentCount !== 0 || App.composer.preparing !== 0) return win.later("the core stores the archives");
                // ---- too large for one file: the archive is not offered ----
                win.composerItem.attach([BIG]);
                win.next("big");
                return;
            case "big": {
                if (!win.questionShown()) return win.later("the question about the large folder");
                const zip = win.asked("folderZip");
                const hint = win.asked("folderZipHint").text;
                if (zip.enabled) return win.fail("an archive larger than the chat takes is offered");
                if (hint.indexOf("Not possible here: the files are 150.0 MB, and this chat takes files up to 104.9 MB.") !== 0) return win.fail("why not: " + hint);
                // Adding the files is still offered.
                if (!win.asked("folderAdd").enabled) return win.fail("the other answer stands");
                // Asked for anyway: nothing is packed.
                App.composer.answerFolders("zip");
                if (!App.composer.folderAsking || fileRows.objectAt(0).stage !== "asking") return win.fail("the refused answer changed something");
                // Escape: the folder leaves the list.
                if (!win.ev.keyClick(Qt.Key_Escape, Qt.NoModifier, -1)) return win.fail("no key event");
                win.next("escaped");
                return;
            }
            case "escaped":
                if (App.composer.folderAsking || App.composer.attachmentCount !== 0 || win.dialog !== null) return win.later("Escape cancels");
                // ---- nothing to send in it ----
                App.composer.clearNotice();
                win.composerItem.attach([EMPTY]);
                win.next("empty");
                return;
            case "empty":
                if (App.composer.attachmentCount !== 0 || App.composer.notice === "") return win.later("the empty folder leaves");
                if (App.composer.folderAsking) return win.fail("a question about nothing");
                if (App.composer.notice !== "“Empty” holds no files to send. Left out: 1 hidden item.") return win.fail("the notice: " + App.composer.notice);
                // ---- very many files: asked once more ----
                App.composer.clearNotice();
                win.composerItem.attach([MANY]);
                win.next("many");
                return;
            case "many": {
                if (!win.questionShown()) return win.later("the question about many files");
                if (App.composer.folderQuestion.confirming) return win.fail("the second question comes first");
                win.asked("folderAdd").click();
                win.next("confirm");
                return;
            }
            case "confirm": {
                if (!App.composer.folderQuestion.confirming) return win.later("the second question");
                if (App.composer.attachmentCount !== 1) return win.fail("files were added before the second yes");
                const text = win.asked("folderText").text;
                if (text !== "This puts 1001 files into the list. They are sent one after another, each as it is.") return win.fail("the second question: " + text);
                if (win.asked("folderAdd").text !== "Add All 1001 Files") return win.fail("its yes: " + win.asked("folderAdd").text);
                // Back, and forward again.
                App.composer.answerFolders("back");
                if (App.composer.folderQuestion.confirming || !win.questionShown()) return win.fail("back to the first question");
                win.asked("folderAdd").click();
                if (!App.composer.folderQuestion.confirming) return win.fail("the second question again");
                win.t0 = App.threadCpuMs();
                win.asked("folderAdd").click();
                win.next("filled");
                return;
            }
            case "filled": {
                if (App.composer.attachmentCount !== 1001 || App.composer.folderAsking) return win.later("a thousand and one files");
                // (CPU time of the Qt thread from the second yes to the list with every file.)
                console.warn("INFO: 1001 files of a folder in the list after " + (App.threadCpuMs() - win.t0).toFixed(0) + " ms of the Qt thread");
                if (fileRows.objectAt(0).name !== "f0000.txt" || fileRows.objectAt(1000).name !== "f1000.txt") return win.fail("their order: " + fileRows.objectAt(0).name + " … " + fileRows.objectAt(1000).name);
                App.composer.clearAttachments();
                App.composer.clearNotice();
                // ---- pasted as a file ----
                if (!App.composer.paste()) return win.fail("the clipboard's folder was not taken");
                win.next("pasted");
                return;
            }
            case "pasted": {
                if (!win.questionShown()) return win.later("the question about the pasted folder");
                if (fileRows.objectAt(0).name !== "Docs" || fileRows.objectAt(0).stage !== "asking") return win.fail("the pasted folder: " + win.names());
                // "Cancel" (the dialog's button): gone.
                win.dialog.customFooterActions[1].trigger();
                win.next("cancelled");
                return;
            }
            case "cancelled":
                if (App.composer.folderAsking || App.composer.attachmentCount !== 0 || win.dialog !== null) return win.later("Cancel takes the folder out");
                // ---- removed while Kourier still looks into it ----
                win.composerItem.attach([TRIP]);
                App.composer.removeAttachment(0);
                if (App.composer.attachmentCount !== 0) return win.fail("the folder was not removed");
                win.next("removed");
                return;
            case "removed":
                // Nothing of it comes back: no row, no question, no notice.
                if (App.composer.attachmentCount !== 0 || App.composer.folderAsking || App.composer.notice !== "") return win.fail("a removed folder came back: " + win.names() + " " + App.composer.notice);
                win.quiet += 1;
                if (win.quiet < 25) return;
                // ---- a narrow window: the note about a file's type is cut, and the
                // pointer shows all of it ----
                win.width = 380;
                win.composerItem.attach([APK]);
                win.next("note");
                return;
            case "note": {
                if (fileRows.count !== 1 || !App.composer.planned || fileRows.objectAt(0).note === "" || fileRows.objectAt(0).size === "") return win.later("the file with its note");
                const label = win.find(win.composerItem.fileList, i => i.objectName === "fileDetails" && i.visible);
                if (!label) return win.later("the row");
                const full = fileRows.objectAt(0).size + " · " + fileRows.objectAt(0).note;
                if (label.text !== full) return win.fail("the line: " + label.text);
                if (!label.truncated) return win.later("the line is cut at this width (" + label.width + ")");
                if (label.QQC2.ToolTip.visible) return win.fail("a tooltip without the pointer");
                win.noteLabel = label;
                if (!win.ev.mouseMove(label, Math.round(label.width / 2), Math.round(label.height / 2), -1, Qt.NoButton, Qt.NoModifier)) return win.fail("no pointer event");
                win.next("tip");
                return;
            }
            case "tip": {
                const label = win.noteLabel;
                if (!label.QQC2.ToolTip.visible) {
                    // (The pointer rests; a tooltip takes its delay.)
                    win.ev.mouseMove(label, Math.round(label.width / 2) + (win.waits % 2), Math.round(label.height / 2), -1, Qt.NoButton, Qt.NoModifier);
                    return win.later("the tooltip with the whole line");
                }
                if (label.QQC2.ToolTip.text !== "12 B · Some Test network apps cannot save this type of file") return win.fail("the tooltip: " + label.QQC2.ToolTip.text);
                Qt.exit(0);
                return;
            }
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

/// One attachment of an own message: `(name, kind, MIME type, id)`.
type Sent = (String, AttachmentKind, String, Option<AttachmentId>);

/// The attachments of the own messages of `conv`, oldest first, with each message's caption.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_files(app: &kourier_app::AppHandle, conv: ConversationId) -> Vec<(String, Vec<Sent>)> {
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
                    .map(|a| {
                        (
                            a.file_name.clone().unwrap_or_default(),
                            a.kind,
                            a.mime.clone(),
                            a.id,
                        )
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

/// The plaintext of a stored attachment, as the core hands it out for "open with".
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn stored_bytes(app: &kourier_app::AppHandle, attachment: AttachmentId) -> Vec<u8> {
    let mut events = app.subscribe();
    app.send(UiCommand::OpenMedia { attachment })
        .expect("ask for the copy");
    let path = app.runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match events.recv().await {
                    Ok(UiEvent::MediaOpened {
                        attachment: id,
                        path,
                    }) if id == attachment => break path,
                    Ok(UiEvent::ActionFailed {
                        action: "OpenMedia",
                        ..
                    }) => {
                        panic!("the stored archive could not be opened")
                    }
                    Ok(_) => {}
                    Err(e) => panic!("events: {e}"),
                }
            }
        })
        .await
        .expect("the copy in time")
    });
    let bytes = std::fs::read(&path).expect("read the copy");
    app.send(UiCommand::CloseMedia { attachment })
        .expect("close the copy");
    bytes
}

/// What the test's folders hold.
struct Folders {
    dir: PathBuf,
}

impl Folders {
    fn make() -> Self {
        let dir = std::env::temp_dir().join(format!("kourier-folder-ui-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |below: &str, bytes: &[u8]| {
            let path = dir.join(below);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("make a folder");
            std::fs::write(&path, bytes).expect("write a file");
        };
        let png = |hue: i32| kourier_qt::bridge::qobject::sample_png(64, 48, hue);
        write("Cover.txt", b"Two folders follow.\n");
        // A type some apps of the test network will not save (noted in its row).
        write("Ground station.apk", b"PK\x03\x04 nothing");
        // A trip: two pictures, notes in a subfolder, a hidden file, and a link that leads
        // out of the folder to something private.
        write("Trip/a.png", &png(20));
        write("Trip/b.png", &png(200));
        write(
            "Trip/notes/plan.txt",
            b"Day 1: the ridge. Day 2: the lake.\n",
        );
        write("Trip/.secret", b"HIDDEN-IN-THE-FOLDER");
        write("private/diary.txt", b"PRIVATE-OUTSIDE-THE-FOLDER");
        // (One to a file, one to a folder: neither is followed.)
        std::os::unix::fs::symlink(dir.join("private/diary.txt"), dir.join("Trip/shortcut"))
            .expect("make the link");
        std::os::unix::fs::symlink(dir.join("private"), dir.join("Trip/more"))
            .expect("make the link to a folder");
        write("Docs/one.txt", b"one\n");
        write("Docs/two.txt", b"two\n");
        // More than the chat takes as one file (the fake's 100 MiB), without filling the
        // disk: a file with a hole.
        std::fs::create_dir_all(dir.join("Big")).expect("make a folder");
        std::fs::File::create(dir.join("Big/huge.bin"))
            .and_then(|f| f.set_len(150_000_000))
            .expect("make the large file");
        write("Empty/.directory", b"[Desktop Entry]\n");
        for n in 0..=1000 {
            write(&format!("Many/f{n:04}.txt"), b"");
        }
        Self { dir }
    }

    fn url(&self, below: &str) -> String {
        format!("{:?}", kourier_qt::app::file_url(&self.dir.join(below)))
    }

    fn read(&self, below: &str) -> Vec<u8> {
        std::fs::read(self.dir.join(below)).expect("read a test file")
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The entries of an archive: `(name, bytes)`, in its order.
fn entries(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("a ZIP archive");
    (0..zip.len())
        .map(|i| {
            let mut entry = zip.by_index(i).expect("an entry");
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).expect("its bytes");
            (entry.name().to_owned(), bytes)
        })
        .collect()
}

fn archives_left(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, Iterator::count)
}

fn main() {
    support::answer_list("folder_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output.
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
    let before = own_files(&handle, media).len();
    let archives = app.paths().archives_dir();
    assert!(
        archives.is_dir() && archives_left(&archives) == 0,
        "the archive directory starts empty"
    );
    assert!(
        !archives.starts_with(&app.paths().runtime),
        "archives are written to the disk, not into the runtime directory"
    );

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let folders = Folders::make();
    // What a file manager's "Copy" of a folder leaves on the clipboard.
    let mut copied = QList::<QString>::default();
    copied.append(QString::from(
        folders.dir.join("Docs").to_string_lossy().as_ref(),
    ));
    kourier_qt::bridge::qobject::set_clipboard_files(&cxx_qt_lib::QStringList::from(&copied));

    let script = SCRIPT
        .replace("COVER", &folders.url("Cover.txt"))
        .replace("TRIP", &folders.url("Trip"))
        .replace("DOCS", &folders.url("Docs"))
        .replace("BIG", &folders.url("Big"))
        .replace("EMPTY", &folders.url("Empty"))
        .replace("MANY", &folders.url("Many"))
        .replace("APK", &folders.url("Ground station.apk"))
        .replace("MEDIA", &media.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/folder_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test a folder given to the composer ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }

    // ---- what the core made of the two sends ----
    let sent = wait_for(60, || {
        let all = own_files(&handle, media);
        let new = all[before..].to_vec();
        let files: usize = new.iter().map(|(_, files)| files.len()).sum();
        (files >= 6).then_some(new)
    });
    let names: Vec<&str> = sent
        .iter()
        .flat_map(|(_, files)| files.iter().map(|f| f.0.as_str()))
        .collect();
    assert_eq!(
        names,
        [
            "Cover.txt",
            "a.png",
            "b.png",
            "plan.txt",
            "Trip.zip",
            "Docs.zip"
        ],
        "the files inside, in the order of their paths; then one archive per folder"
    );
    assert_eq!(sent[0].0, "the trip", "the caption on the first message");
    let archive = |name: &str| -> &Sent {
        sent.iter()
            .flat_map(|(_, files)| files.iter())
            .find(|f| f.0 == name)
            .expect("the archive's message")
    };
    for name in ["Trip.zip", "Docs.zip"] {
        let (_, kind, mime, _) = archive(name);
        assert_eq!(*kind, AttachmentKind::File, "{name} travels as a file");
        assert_eq!(mime, "application/zip", "{name}");
    }
    // The archive as the core stored it: the folder's files under relative paths, byte for
    // byte, and nothing of the hidden file or of what the link leads to.
    let trip = stored_bytes(&handle, archive("Trip.zip").3.expect("an id"));
    let inside = entries(&trip);
    assert_eq!(
        inside.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        ["Trip/a.png", "Trip/b.png", "Trip/notes/plan.txt"]
    );
    for (name, bytes) in &inside {
        assert_eq!(*bytes, folders.read(name), "{name}");
    }
    assert!(!contains(&trip, b"HIDDEN-IN-THE-FOLDER"));
    assert!(!contains(&trip, b"PRIVATE-OUTSIDE-THE-FOLDER"));
    assert!(!contains(&trip, folders.dir.to_string_lossy().as_bytes()));
    let docs = stored_bytes(&handle, archive("Docs.zip").3.expect("an id"));
    assert_eq!(
        entries(&docs),
        [
            ("Docs/one.txt".to_owned(), b"one\n".to_vec()),
            ("Docs/two.txt".to_owned(), b"two\n".to_vec())
        ]
    );
    // The archives were Kourier's own copies: gone once the core had stored the send (they
    // are deleted on a worker thread, not on the UI's).
    wait_for(20, || (archives_left(&archives) == 0).then_some(()));
    // The folders themselves are untouched.
    assert_eq!(folders.read("Trip/.secret"), b"HIDDEN-IN-THE-FOLDER");
    assert!(folders.dir.join("Trip/shortcut").is_symlink());

    let _ = std::fs::remove_dir_all(&folders.dir);
    app.shutdown();
    assert!(
        !archives.exists() || archives_left(&archives) == 0,
        "nothing is left after the exit"
    );
    println!("test a folder given to the composer ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
