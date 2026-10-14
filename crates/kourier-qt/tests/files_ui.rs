//! Many files, albums and the edit history through the real core over the fake network, with
//! the app's own composer, timeline, viewer and dialogs (offscreen, `harness = false`):
//!
//! - 300 files of several kinds are added to the composer in one call (ten times what Signal
//!   Desktop's composer takes). The call returns at once and reads nothing from disk; the list
//!   creates delegates only for the rows on screen; sizes and the core's plan arrive later. The
//!   list then says how the selection will be sent (the fake network packs three pictures into
//!   a message and sends every other file alone) and marks the two things no network can carry
//!   (a socket, a file that is gone) without holding the others back. (A folder is neither:
//!   it is asked about, `folder_ui.rs`.)
//! - The send takes the 298 files that can go; the two stay in the list with their reasons.
//!   The Qt thread keeps ticking while the core copies and encrypts. The timeline then holds
//!   exactly the messages the plan announced, in the user's order, the caption on the first,
//!   each with a state of its own.
//! - Files copied in a file manager are pasted from the clipboard.
//! - An album of nine pictures shows five tiles ("+4"); the viewer opens on the tapped one and
//!   steps through all nine with the keyboard and its buttons.
//! - A click on a message's "edited" mark opens its history: the versions, newest first.
//! - A notice (a system row) has a context menu that removes it from this account.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QList, QQmlApplicationEngine, QString, QUrl};
use kourier_app::facade::{OriginKind, Query, QueryData, TimelineAnchor, TimelineQuery};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_model::{Body, ConversationId, OutgoingState};
use kourier_qt::{UiContext, UiOptions};

/// Pictures, then documents: with the fake's rule (three pictures a message, the rest alone)
/// 290 pictures are 97 messages and 8 documents 8 more.
const PICTURES: usize = 290;
const DOCUMENTS: usize = 8;
const MESSAGES: usize = 97 + DOCUMENTS;

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

// The window is the root: the views need a shown window to lay their rows out.
Window {
    id: test

    width: 640
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property real sentAt: 0
    // The longest the Qt thread went without running a 20 ms timer (wall clock: it also
    // counts the time the system gave to other processes, so it is only reported), and the
    // most CPU time the Qt thread used between two ticks: the longest stretch of work it did
    // without returning to its event loop, which is what must stay short and does not depend
    // on how busy the machine is.
    property real lastTick: 0
    property real longestGap: 0
    property real lastCpu: -1
    property real longestWork: 0
    property real addTook: 0
    property real album: 0
    property real edited: 0
    property real notice: 0
    property var viewer: null

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        test.waits += 1;
        if (test.waits > 2400) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        test.step = step;
        test.waits = 0;
    }

    function key(k: int): void {
        if (!test.ev.keyClick(k, Qt.NoModifier, -1)) {
            fail("the window took no key event");
        }
    }

    // The first item below `root` that `pred` accepts.
    function find(root: Item, pred: var): Item {
        if (pred(root)) {
            return root;
        }
        for (const child of root.children) {
            const hit = test.find(child, pred);
            if (hit) {
                return hit;
            }
        }
        return null;
    }

    function click(item: Item): void {
        if (!test.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, Qt.NoModifier, -1)) {
            fail("the window took no mouse event");
        }
    }

    Timer {
        interval: 20
        repeat: true
        running: true
        onTriggered: {
            const now = Date.now();
            if (test.lastTick > 0) {
                test.longestGap = Math.max(test.longestGap, now - test.lastTick);
            }
            test.lastTick = now;
            const cpu = App.threadCpuMs();
            if (test.lastCpu >= 0) {
                test.longestWork = Math.max(test.longestWork, cpu - test.lastCpu);
            }
            test.lastCpu = cpu;
        }
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property bool own
            required property bool edited
            required property string receipt
            required property var blocks
            required property var actions
        }
    }

    Instantiator {
        id: fileRows
        model: App.composer.files
        delegate: QtObject {
            required property string name
            required property string kind
            required property string size
            required property string reason
            required property string preview
            required property int part
            required property bool partFirst
        }
    }

    function fileRow(name: string): var {
        for (let i = 0; i < fileRows.count; ++i) {
            const r = fileRows.objectAt(i);
            if (r && r.name === name) {
                return r;
            }
        }
        return null;
    }

    function rowWith(pred: var): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && pred(r)) {
                return r;
            }
        }
        return null;
    }

    // The newest album whose pictures are all downloaded, with at least `count` of them.
    function albumRow(count: int): var {
        return test.rowWith(r => r.blocks.some(b => b.type === "album" && b.count >= count && b.items.every(i => i.state === "local" && i.fullUrl !== "")));
    }

    Item {
        id: holder
        anchors.fill: parent

        TimelineView {
            id: view
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.bottom: composerItem.top
        }
        Composer {
            id: composerItem
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.bottom: parent.bottom
        }
    }

    Component {
        id: viewerComponent
        ImageViewer {}
    }
    Connections {
        target: MediaActions
        function onAlbumRequested(items: var, index: int): void {
            test.viewer = viewerComponent.createObject(test.contentItem, {
                "items": items,
                "index": index
            });
            test.viewer.open();
        }
    }

    Loader {
        id: albumView
        active: false
        property var block: null
        sourceComponent: AlbumBlock {
            block: albumView.block
        }
    }

    Timer {
        interval: 25
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                test.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", holder, "files_ui_events.qml");
                App.openConversation(MEDIA);
                test.next("open");
                return;
            case "open": {
                if (App.timeline.attachKinds.indexOf("file") < 0 || rows.count < 5) return later("the chat");
                // ---- 300 files in one go ----
                const files = FILES;
                if (files.length !== 300) return fail("the test's own list: " + files.length);
                const t0 = App.threadCpuMs();
                const added = App.composer.addFiles(files);
                test.addTook = App.threadCpuMs() - t0;
                if (added !== 300 || App.composer.attachmentCount !== 300) return fail("added " + added + " of 300, the list has " + App.composer.attachmentCount);
                // Nothing was read to add them: no sizes, no kinds by content, no previews.
                // (CPU time of the Qt thread: a wall clock also measures the other builds.)
                if (test.addTook > 100) return fail("adding 300 files took " + test.addTook.toFixed(1) + " ms of the Qt thread");
                if (App.composer.planned) return fail("a plan without asking the core");
                if (fileRows.objectAt(0).size !== "" || fileRows.objectAt(0).preview !== "") return fail("a size or a preview at once: the Qt thread read the file");
                if (App.composer.summary !== "300 files") return fail("the summary before the plan: " + App.composer.summary);
                test.next("virtual");
                return;
            }
            case "virtual": {
                // The list is a view on a model: rows off screen have no delegate.
                const list = composerItem.fileList;
                if (list.count !== 300) return later("the list");
                let delegates = 0;
                for (const child of list.contentItem.children) {
                    delegates += child.hasOwnProperty("partFirst") ? 1 : 0;
                }
                if (delegates < 3 || delegates > 40) return fail("delegates for 300 rows: " + delegates);
                if (list.height > test.height / 2) return fail("the list takes the window: " + list.height);
                test.next("planned");
                return;
            }
            case "planned": {
                if (!App.composer.planned || fileRows.objectAt(0).size === "") return later("the core's plan and the sizes");
                // What the core makes of it: 290 pictures in threes, 8 documents alone, and
                // two things that cannot go.
                if (App.composer.refusedCount !== 2) return fail("refused: " + App.composer.refusedCount);
                if (App.composer.sendableCount !== 298) return fail("sendable: " + App.composer.sendableCount);
                if (App.composer.messageCount !== MESSAGES) return fail("messages: " + App.composer.messageCount);
                if (App.composer.summary.indexOf("300 files, " + MESSAGES + " messages · ") !== 0) return fail("the summary: " + App.composer.summary);
                if (App.composer.hint.indexOf("2 files cannot be sent here") !== 0) return fail("the hint: " + App.composer.hint);
                const first = fileRows.objectAt(0);
                if (first.part !== 1 || !first.partFirst || first.kind !== "image" || first.preview.indexOf("file://") !== 0) return fail("the first picture: part " + first.part + " " + first.kind);
                if (fileRows.objectAt(2).part !== 1 || fileRows.objectAt(2).partFirst) return fail("the third picture shares message 1");
                if (fileRows.objectAt(3).part !== 2 || !fileRows.objectAt(3).partFirst) return fail("the fourth picture starts message 2");
                const doc = test.fileRow("Notes 1.txt");
                if (!doc || doc.part !== 98 || !doc.partFirst || doc.kind !== "file") return fail("the first document travels alone as message 98: " + (doc ? doc.part : "missing"));
                const socket = test.fileRow("A socket");
                if (!socket || socket.reason !== "Not a file" || socket.part !== 0) return fail("the socket is marked: " + (socket ? socket.reason : "missing"));
                const gone = test.fileRow("Gone.pdf");
                if (!gone || gone.reason !== "Cannot be read" || gone.part !== 0) return fail("the missing file is marked: " + (gone ? gone.reason : "missing"));
                // ---- the send takes what can go ----
                composerItem.setText("All of it");
                test.next("captioned");
                return;
            }
            case "captioned":
                // (The caption does not change the plan here: the first message carries it.)
                if (!App.composer.planned) return later("the plan with a caption");
                if (App.composer.messageCount !== MESSAGES) return fail("messages with a caption: " + App.composer.messageCount);
                test.longestGap = 0;
                test.longestWork = 0;
                test.sentAt = Date.now();
                composerItem.send();
                if (App.composer.attachmentCount !== 2 || App.composer.sendableCount !== 0) return fail("after the send the two refused files stay: " + App.composer.attachmentCount);
                if (composerItem.text !== "") return fail("the caption went with the send");
                if (App.composer.preparing !== 298) return fail("files in preparation: " + App.composer.preparing);
                if (App.composer.notice.indexOf("2 files could not be sent") !== 0) return fail("the notice: " + App.composer.notice);
                if (!test.fileRow("A socket") || !test.fileRow("Gone.pdf")) return fail("which files stayed");
                test.next("sent");
                return;
            case "sent": {
                // The core copies and encrypts 298 files; their messages show when all are
                // stored. The Qt thread is not part of that.
                if (App.composer.preparing !== 0) return later("the messages of the send");
                const newest = test.rowWith(r => r.own);
                if (!newest || !newest.blocks.some(b => b.type === "file")) return later("the last document as the newest own row");
                // The Qt thread never worked for long in one go while the core copied (and
                // while 105 messages arrived in the timeline). Phase H allowed a wall-clock
                // stall of 1000 ms and still failed once in a full run: the gaps it measured
                // (33 to 466 ms) were mostly the system running other builds. In CPU time
                // the longest stretch is the timeline taking the new rows, a few tens of ms.
                if (test.longestWork > 150) return fail("the Qt thread worked for " + test.longestWork.toFixed(1) + " ms in one go during the send");
                console.warn("files_ui: add " + test.addTook.toFixed(1) + " ms of the Qt thread, longest work in one go " + test.longestWork.toFixed(1) + " ms (longest gap between ticks " + test.longestGap + " ms), " + (Date.now() - test.sentAt) + " ms until the messages showed");
                App.composer.clearAttachments();
                App.composer.clearNotice();
                if (App.composer.attachmentCount !== 0 || App.composer.summary !== "") return fail("clear all");
                // ---- pasting files copied in a file manager ----
                if (!App.composer.paste()) return fail("the clipboard's files were not pasted");
                if (App.composer.attachmentCount !== 2) return fail("pasted files: " + App.composer.attachmentCount);
                if (fileRows.objectAt(0).name !== "Notes 1.txt" || fileRows.objectAt(1).name !== "Notes 2.txt") return fail("which files were pasted");
                App.composer.removeAttachment(0);
                if (App.composer.attachmentCount !== 1 || fileRows.objectAt(0).name !== "Notes 2.txt") return fail("remove one");
                App.composer.clearAttachments();
                // ---- an album ----
                App.openConversation(ALBUMS);
                test.next("albums");
                return;
            }
            case "albums": {
                const r = test.albumRow(9);
                if (!r) return later("the album of nine with its pictures");
                test.album = r.messageId;
                const block = r.blocks.find(b => b.type === "album");
                if (block.count !== 9 || block.items.length !== 9) return fail("the album's pictures: " + block.count);
                // Its grid: five tiles, the last one counting the rest.
                albumView.block = block;
                albumView.active = true;
                test.next("grid");
                return;
            }
            case "grid": {
                const grid = albumView.item;
                if (!grid) return later("the grid");
                if (grid.tiles !== 5 || grid.hidden !== 4) return fail("tiles " + grid.tiles + ", hidden " + grid.hidden);
                if (grid.implicitHeight <= 0 || grid.implicitHeight >= grid.implicitWidth) return fail("the grid of five is wider than tall: " + grid.implicitWidth + "x" + grid.implicitHeight);
                // A tap on the second tile opens the viewer on the second of all nine.
                grid.openAt(1);
                test.next("viewer");
                return;
            }
            case "viewer": {
                const v = test.viewer;
                if (!v || !v.opened || !v.shown) return later("the viewer with its picture");
                if (v.count !== 9 || v.index !== 1) return fail("the viewer opened at " + v.index + " of " + v.count);
                const second = v.url;
                if (second === "" || second !== v.items[1].url) return fail("the second picture");
                // The keyboard steps: Right, Right, Left.
                test.key(Qt.Key_Right);
                if (v.index !== 2 || v.url !== v.items[2].url || v.url === second) return fail("Right: " + v.index);
                test.key(Qt.Key_Right);
                test.key(Qt.Key_Left);
                if (v.index !== 2) return fail("Right, Left: " + v.index);
                // End and Home; the ends hold.
                test.key(Qt.Key_End);
                if (v.index !== 8) return fail("End: " + v.index);
                test.key(Qt.Key_Right);
                if (v.index !== 8) return fail("past the last picture: " + v.index);
                test.key(Qt.Key_Home);
                test.key(Qt.Key_Left);
                if (v.index !== 0) return fail("Home, Left: " + v.index);
                test.next("buttons");
                return;
            }
            case "buttons": {
                const v = test.viewer;
                const named = name => test.find(v.contentItem, c => c.objectName === name);
                const previous = named("viewerPrevious");
                const forward = named("viewerNext");
                const counter = named("viewerCounter");
                if (!previous || !forward || !counter) return fail("the viewer's buttons");
                if (previous.enabled || !forward.enabled) return fail("at the first picture only Next works");
                if (counter.text !== "1 of 9") return fail("the counter: " + counter.text);
                test.click(forward);
                test.click(forward);
                if (v.index !== 2 || counter.text !== "3 of 9") return fail("Next twice: " + v.index + " " + counter.text);
                test.click(previous);
                if (v.index !== 1) return fail("Previous: " + v.index);
                test.next("stepped");
                return;
            }
            case "stepped": {
                // The picture it stepped to is shown, not only numbered.
                const v = test.viewer;
                if (!v.shown) return later("the picture after stepping");
                test.key(Qt.Key_Escape);
                test.next("viewerClosed");
                return;
            }
            case "viewerClosed": {
                if (test.viewer.opened || test.viewer.visible) return later("Escape closes the viewer");
                test.viewer.destroy();
                albumView.active = false;
                // ---- the edit history ----
                const r = test.rowWith(r => r.edited);
                if (!r) return fail("the edited message of the chat");
                test.edited = r.messageId;
                if (App.findMessage("edited") !== r.messageId) return fail("findMessage(edited)");
                // Bring its row on screen: the "edited" mark is what opens the history. (The
                // view must not snap back to the newest message meanwhile.)
                view.followNewest = false;
                for (let i = 0; i < rows.count; ++i) {
                    if (rows.objectAt(i).messageId === r.messageId) {
                        view.rowList.positionViewAtIndex(i, ListView.Center);
                    }
                }
                test.next("editedMark");
                return;
            }
            case "editedMark": {
                const row = view.delegateFor(test.edited);
                if (!row) {
                    for (let i = 0; i < rows.count; ++i) {
                        if (rows.objectAt(i).messageId === test.edited) {
                            view.rowList.positionViewAtIndex(i, ListView.Center);
                        }
                    }
                    return later("the edited message's row");
                }
                const mark = test.find(row, c => c.objectName === "editedMark" && c.visible);
                if (!mark) return later("the edited mark");
                if (view.historyDialog !== null) return fail("a history before the click");
                test.click(mark);
                // Asked again while it opens (a double click): the same history, not a
                // second dialog on top of it.
                const shown = view.historyDialog;
                if (!shown) return fail("the click on the mark opened no history");
                if (!view.historyFor(test.edited) || view.historyDialog !== shown) return fail("a second history for the same message");
                test.next("history");
                return;
            }
            case "history": {
                const dialog = view.historyDialog;
                if (!dialog || !dialog.loaded) return later("the versions");
                const versions = dialog.rows;
                if (versions.length !== 3) return fail("versions: " + versions.length);
                if (!versions[0].current || versions[0].original || versions[0].text.plain !== "Meet at the hangar at 6, and bring the spare props") return fail("the current version: " + versions[0].text.plain);
                if (versions[1].current || versions[1].original || versions[1].text.plain !== "Meet at the hangar at 6") return fail("the middle version: " + versions[1].text.plain);
                if (!versions[2].original || versions[2].text.plain !== "Meet at the hangar at 5") return fail("the original: " + versions[2].text.plain);
                if (versions.some(v => v.when === "" || v.empty)) return fail("every version has its time and text");
                dialog.close();
                // The history of a message that does not exist says so instead of spinning.
                if (!view.historyFor(999999)) return fail("asking for an unknown message");
                test.next("nohistory");
                return;
            }
            case "nohistory": {
                const dialog = view.historyDialog;
                if (!dialog || !dialog.loaded) return later("the answer for an unknown message");
                if (dialog.rows.length !== 0) return fail("versions of nothing: " + dialog.rows.length);
                dialog.close();
                // ---- a notice has a context menu ----
                App.openConversation(TIMER);
                test.next("notices");
                return;
            }
            case "notices": {
                if (App.timeline.conversationId !== TIMER || App.timeline.loading) return later("the chat with notices");
                test.notice = view.newestNotice();
                if (test.notice <= 0) return later("a notice on screen");
                if (!view.menuFor(test.notice)) return fail("the notice's menu did not open");
                test.next("noticeMenu");
                return;
            }
            case "noticeMenu": {
                const menu = view.messageMenu;
                if (!menu.visible) return later("the menu");
                if (!menu.row || menu.row.system !== true || menu.row.messageId !== test.notice) return fail("the menu is about the notice");
                // It offers its removal from this account and nothing a message would.
                if (menu.offers.deleteForMe !== true || menu.offers.reply === true || menu.offers.react === true || menu.offers.deleteForEveryone === true) return fail("what a notice offers");
                menu.close();
                App.deleteMessage(test.notice, false);
                test.next("noticeGone");
                return;
            }
            case "noticeGone": {
                for (let i = 0; i < rows.count; ++i) {
                    const r = rows.objectAt(i);
                    if (r && r.messageId === test.notice) return later("the notice goes");
                }
                test.close();
                test.next("done");
                return;
            }
            case "done":
                Qt.exit(0);
                return;
            }
        }
    }

    Timer {
        interval: 240000
        running: true
        onTriggered: test.fail("timeout")
    }
}
"#;

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

/// The 300 things the user picks: pictures, documents, a socket (no file, and no folder
/// either) and a file that is gone. The socket lives as long as the listener returned.
fn selection(dir: &Path) -> (Vec<PathBuf>, std::os::unix::net::UnixListener) {
    let mut files = Vec::with_capacity(300);
    for n in 0..PICTURES {
        let path = dir.join(format!("Picture {:03}.png", n + 1));
        let hue = i32::try_from(n * 7 % 360).unwrap_or(0);
        let png = kourier_qt::bridge::qobject::sample_png(48, 32, hue);
        std::fs::write(&path, png).expect("write a picture");
        files.push(path);
    }
    for n in 0..DOCUMENTS {
        let path = dir.join(format!("Notes {}.txt", n + 1));
        std::fs::write(&path, format!("notes {n}\n").repeat(40)).expect("write a document");
        files.push(path);
    }
    let socket = dir.join("A socket");
    let listener = std::os::unix::net::UnixListener::bind(&socket).expect("make the socket");
    files.push(socket);
    files.push(dir.join("Gone.pdf"));
    assert_eq!(files.len(), 300);
    (files, listener)
}

/// One own message with files, as the core stored it.
#[derive(Clone, Debug)]
struct Sent {
    /// How many files it carries.
    files: usize,
    caption: Option<String>,
    state: Option<OutgoingState>,
    /// The names of its files, in order.
    names: Vec<String>,
}

/// The own messages with files in `conv`, oldest first.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_file_messages(app: &kourier_app::AppHandle, conv: ConversationId) -> Vec<Sent> {
    let page = app
        .runtime()
        .block_on(app.query(Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 400,
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
            } => Some(Sent {
                files: attachments.len(),
                caption: caption.as_ref().map(|c| c.body.clone()),
                state: r.out_state.clone(),
                names: attachments
                    .iter()
                    .map(|a| a.file_name.clone().unwrap_or_default())
                    .collect(),
            }),
            _ => None,
        })
        .collect()
}

fn main() {
    support::answer_list("files_ui");
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before the app and Qt start.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
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
    let albums = fixture.albums_conversation.expect("the albums chat");
    let timer = fixture.timer_conversation.expect("the timer chat");
    let before = own_file_messages(&handle, media).len();

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let dir = std::env::temp_dir().join(format!("kourier-files-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (files, _socket) = selection(&dir);
    let urls: Vec<String> = files.iter().map(|p| kourier_qt::app::file_url(p)).collect();
    // What a file manager's "Copy" leaves on the clipboard.
    let mut copied = QList::<QString>::default();
    for path in &files[PICTURES..PICTURES + 2] {
        copied.append(QString::from(path.to_string_lossy().as_ref()));
    }
    kourier_qt::bridge::qobject::set_clipboard_files(&cxx_qt_lib::QStringList::from(&copied));

    let script = SCRIPT
        .replace("FILES", &format!("{urls:?}"))
        .replace("MESSAGES", &MESSAGES.to_string())
        .replace("MEDIA", &media.0.to_string())
        .replace("ALBUMS", &albums.0.to_string())
        .replace("TIMER", &timer.0.to_string());
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/files_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    assert_eq!(code, 0, "the QML script failed (see FAIL above)");

    // What the core made of the send: exactly the plan the composer showed.
    let sent = wait_for(60, || {
        let all = own_file_messages(&handle, media);
        let new = all[before..].to_vec();
        // Every message has an answer of its own from the (fake) network.
        let settled = new.iter().all(|m| {
            matches!(
                m.state,
                Some(OutgoingState::Sent { .. } | OutgoingState::Delivered | OutgoingState::Read)
            )
        });
        (new.len() >= MESSAGES && settled).then_some(new)
    });
    assert_eq!(sent.len(), MESSAGES, "as many messages as the plan said");
    let counts: Vec<usize> = sent.iter().map(|m| m.files).collect();
    let mut expected = vec![3; PICTURES / 3];
    expected.push(PICTURES % 3);
    expected.extend(std::iter::repeat_n(1, DOCUMENTS));
    assert_eq!(counts, expected, "pictures in threes, documents alone");
    assert_eq!(
        counts.iter().sum::<usize>(),
        298,
        "every file that could go went"
    );
    // The caption is on the first message only.
    assert_eq!(sent[0].caption.as_deref(), Some("All of it"));
    assert!(sent[1..].iter().all(|m| m.caption.is_none()));
    // The user's order is kept across the messages.
    let names: Vec<String> = sent.iter().flat_map(|m| m.names.clone()).collect();
    let picked: Vec<String> = files[..298]
        .iter()
        .map(|p| p.file_name().expect("name").to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, picked, "sent in the order they were picked");

    let _ = std::fs::remove_dir_all(&dir);
    app.shutdown();
}
