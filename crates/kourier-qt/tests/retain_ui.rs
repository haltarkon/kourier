//! A picture keeps its pixels while its URL changes, and only for the same picture (P1/S1 fix
//! round F2, F3), through the real main window and the real core over the fake network
//! (offscreen, `harness = false`).
//!
//! A downloaded picture is drawn from `thumb/<id>?v=<a>`; when the core has derived its
//! thumbnail and blurhash the row's blocks change and the URL becomes `?v=<b>`. The timeline
//! must not show the gradient stand-in or the blurhash in between, so
//! `Image.retainWhileLoading` has to hold something: the block delegates of a row live on
//! while the row's content changes (the Repeater's model is the NUMBER of blocks), and the
//! `ImageBlock` is the very same item before and after.
//!
//! - F2: the fixture's chat of imported attachments has a picture that downloads on a click.
//!   The test holds the core's derive tasks (`MediaHandles::hold_derives`), clicks it, waits
//!   until it is on screen from its file, then lets the core derive (a request to a local
//!   port releases the hold) and judges EVERY frame (`frameSwapped`) until the new URL is
//!   drawn: the block is the same item, and it holds pixels.
//!   The same run judges what a screenshot waits for (F4): the pump is slowed down for it
//!   (`kourier_qt::pump::set_test_delay`), and from the release until the new URL is applied
//!   `App.screenshotHolds` is never empty, though the core's derive count is 0 long before.
//!   And what it waits for in the first place (F5): the derive only while a picture of an
//!   attachment is on screen.
//! - Held row (the whole-picture step): the chat is read a little above its newest message
//!   (the view does not follow and holds on to the row at the top of the viewport), and the
//!   picture below it downloads with a size that differs from the sender's claim
//!   (`synthetic_image_claiming`: the row is laid out by 200 x 90 and becomes 90 x 200 tall).
//!   Every frame from the click until the picture is on screen from its file: the row at the
//!   top of the viewport stays where it is (within a pixel) and the view does not start
//!   following the newest message.
//! - Followed newest row (the whole-picture step, decision 6): the view follows the newest
//!   message and the row at the newest end grows (its picture's shape changes, the way a
//!   download corrects a claimed size). Every frame from the change until the row is taller:
//!   the viewport stays at the newest end (within a pixel) and the view keeps following.
//! - F3: pixels are held only for the SAME attachment. In the block: another attachment's URL
//!   loading right after a picture shows no held pixels (an album's tiles are reused by
//!   index), and neither does a URL after a failed one (`paintedWidth` keeps its old value
//!   after an error). In the viewer: stepping to the next picture shows the spinner, not the
//!   previous picture under the new counter, and the same after a failed load.

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{Query, QueryData};
use kourier_app::{App, AppHandle, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentId, AttachmentState};
use kourier_qt::{UiContext, UiOptions};

/// How long the pump waits before it reads each event after the derive was released: the time
/// in which a screenshot must not be taken though the core is done.
const PUMP_DELAY: Duration = Duration::from_millis(250);

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

// The app's own main window is the root (narrow: one page at a time), low enough for the chat
// to be taller than its viewport: the newest row, the picture, is on screen at the bottom, and
// the view can be moved up from it.
MainWindow {
    id: win

    width: 520
    height: 560
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    // The picture that downloads on a click, and its block as it was when it was on screen
    // from its file.
    property real pic: 0
    property var blk: null
    property string urlA: ""
    // Every frame from the release of the derive until the new URL is drawn.
    property bool watching: false
    property int frames: 0
    property var urls: []
    // The samples of `App.screenshotHolds` taken while the old URL was still on screen and it
    // said nothing held the picture.
    property var holes: []
    property var seenHolds: ({})
    // The held row: where the row at the top of the viewport was when the picture below it was
    // clicked, the picture's height then, and the largest distance a frame showed the row from
    // there until the picture was on screen from its file.
    property bool holding: false
    property real heldAt0: 0
    property real blockBefore: 0
    property real worst: 0
    property int heldFrames: 0
    // The followed newest row: the largest distance of the viewport from the newest end that
    // a frame showed while the newest row grew, the frames judged, whether the view ever
    // stopped following, and the row's height before.
    property bool followWatch: false
    property real followWorst: 0
    property int followFrames: 0
    property bool followLost: false
    property real followBefore: 0
    property real followBase: 0

    // The pictures that nobody has drawn yet (their ids), for the viewer and the guards.
    readonly property var spare: [SPARE]

    readonly property var view: win.chatPage ? win.chatPage.timeline : null

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

    // The picture blocks on screen.
    function blocks(): var {
        return win.collect(win.view.rowList.contentItem, c => c.mediaState !== undefined && c.pictureHeld !== undefined && c.attachment > 0 && c.visible, []);
    }

    function block(id: real): var {
        const all = win.blocks().filter(b => b.attachment === id);
        return all.length > 0 ? all[0] : null;
    }

    function pictureOf(b: Item): var {
        return win.find(b, c => c.objectName === "picture");
    }

    function click(item: Item): bool {
        return win.ev.mouseClick(item, item.width / 2, item.height / 2, Qt.LeftButton, Qt.NoModifier, -1);
    }

    // Lets the core derive the thumbnail (the test's listener on a local port releases it).
    function release(): void {
        const request = new XMLHttpRequest();
        request.open("GET", "http://127.0.0.1:PORT/release");
        request.send();
    }

    // The pump is quick again.
    function done(): void {
        const request = new XMLHttpRequest();
        request.open("GET", "http://127.0.0.1:PORT/done");
        request.send();
    }

    function viewerItem(id: real, url: string): var {
        return {
            "attachment": id,
            "url": url,
            "animated": false,
            "media": true,
            "kind": "image",
            "state": "local",
            "progress": 0,
            "placeholder": "",
            "ask": false
        };
    }

    // What a screenshot would wait for now. While the old URL is on screen, after the release,
    // something must hold it: the derive, the events on their way, the queries.
    function holdsNow(b: Item): void {
        const holds = App.screenshotHolds(win);
        for (const what of holds.split(", ")) {
            const name = what.split(" ")[0];
            win.seenHolds[name] = (win.seenHolds[name] ?? 0) + 1;
        }
        if (holds === "" && b.block.url === win.urlA) {
            win.holes.push(win.frames);
        }
    }

    // Where the row the timeline holds on to is in the viewport now (`NaN`: none).
    function heldAtNow(): real {
        const list = win.view.rowList;
        return list.held === null ? NaN : list.held.y - list.contentY;
    }

    // One frame of the held row.
    Connections {
        target: win
        function onFrameSwapped(): void {
            if (!win.holding) {
                return;
            }
            win.heldFrames += 1;
            const at = win.heldAtNow();
            if (isNaN(at)) {
                return win.fail("frame " + win.heldFrames + ": the view lost the row it holds on to");
            }
            win.worst = Math.max(win.worst, Math.abs(at - win.heldAt0));
        }
    }

    // One frame of the followed newest row.
    Connections {
        target: win
        function onFrameSwapped(): void {
            if (!win.followWatch) {
                return;
            }
            win.followFrames += 1;
            win.followWorst = Math.max(win.followWorst, Math.abs(win.view.rowList.below() - win.followBase));
            if (!win.view.followNewest) {
                win.followLost = true;
            }
        }
    }

    // One frame of the watch: the block of the picture is the one that was there and holds
    // pixels.
    Connections {
        target: win
        function onFrameSwapped(): void {
            if (!win.watching) {
                return;
            }
            win.frames += 1;
            const b = win.block(win.pic);
            if (b === null) {
                return win.fail("frame " + win.frames + ": the picture has no block on screen");
            }
            if (b !== win.blk) {
                return win.fail("frame " + win.frames + ": the picture's block was built anew (the row's blocks changed and its delegates were replaced)");
            }
            const p = win.pictureOf(b);
            if (!b.pictureHeld || !p.visible) {
                return win.fail("frame " + win.frames + ": the picture is not held (held " + b.pictureHeld + ", visible " + p.visible + ", status " + p.status + ", url " + b.block.url + ")");
            }
            if (win.urls[win.urls.length - 1] !== b.block.url) {
                win.urls.push(b.block.url);
            }
            win.holdsNow(b);
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
        case "start": {
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            const list = win.pageStack.get(0);
            if (!list || !list.rowsShown || list.flickable.count < 15) return win.later("the list with its rows");
            win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "retain_ui_events.qml");
            win.openConversation(IMPORTED_CHAT);
            return win.next("shown");
        }
        case "shown": {
            if (win.chatPage === null || App.timeline.conversationId !== IMPORTED_CHAT || App.timeline.loading) return win.later("the chat");
            if (!win.chatPage.enabled) return win.later("the chat page to take input");
            const all = win.blocks().filter(b => b.mediaState === "remote" && b.mediaAction === "download" && b.tile === false && b.sticker === false);
            if (all.length === 0) return win.later("the picture that downloads on a click");
            win.pic = all[0].attachment;
            // The view is moved up from the newest message by a little (the reader's move): it
            // stops following and holds on to the row at the top of the viewport.
            const list = win.view.rowList;
            if (list.contentHeight < list.height + 400) return win.fail("precondition: the chat is not taller than its viewport (" + list.contentHeight + " in " + list.height + ")");
            list.contentY -= 40;
            return win.next("held");
        }
        case "held": {
            const list = win.view.rowList;
            if (win.view.followNewest || list.held === null) return win.later("the view holding on to a row");
            const b = win.block(win.pic);
            if (b === null) return win.fail("the picture left the screen");
            const centre = b.mapToItem(list, b.width / 2, b.height / 2);
            if (centre.y < 0 || centre.y > list.height) return win.fail("precondition: the centre of the picture is outside the viewport (" + centre.y + " of " + list.height + ")");
            win.heldAt0 = win.heldAtNow();
            win.blockBefore = b.height;
            win.holding = true;
            if (!win.click(b)) return win.fail("the window took no mouse event");
            return win.next("downloading");
        }
        case "downloading": {
            // On screen from its file (the derive is held): decoded, the thumbnail's URL.
            const b = win.block(win.pic);
            if (b === null || b.mediaState !== "local") return win.later("the picture local");
            const p = win.pictureOf(b);
            if (p === null || p.status !== Image.Ready || !b.pictureHeld || b.block.url.indexOf("image://kourier/thumb/") !== 0) return win.later("the picture drawn");
            // The download corrected the picture's size: the row is taller than the claim made
            // it, and the row at the top of the viewport has not moved.
            win.holding = false;
            // (The content's height is no measure: the list estimates the rows it has not made.)
            const grown = b.height - win.blockBefore;
            if (grown < 50) return win.fail("precondition: the download did not change the picture's height (" + win.blockBefore + " -> " + b.height + ")");
            if (win.view.followNewest) return win.fail("the view followed the newest message while a row below the reader's grew");
            const at = win.heldAtNow();
            if (isNaN(at) || Math.abs(at - win.heldAt0) > 1) return win.fail("the row at the top of the viewport moved from " + win.heldAt0 + " to " + at + " when the row below it grew by " + grown);
            if (win.worst > 1) return win.fail("a frame showed the row at the top of the viewport " + win.worst + " px from where it was (" + win.heldFrames + " frames)");
            console.log("held row: the picture grew by " + grown + " px, worst distance " + win.worst + " px over " + win.heldFrames + " frames");
            win.blk = b;
            win.urlA = b.block.url;
            win.urls = [b.block.url];
            // A screenshot waits for the core's thumbnail derive (it is held: it runs) only
            // while a picture of an attachment is on screen: here one is, and with the chat
            // page hidden none is.
            const shown = App.screenshotHolds(win);
            if (shown.indexOf("derive") < 0) return win.fail("a picture is on screen and the core derives its thumbnail, but a screenshot would not wait for it: '" + shown + "'");
            win.chatPage.visible = false;
            const hidden = App.screenshotHolds(win);
            win.chatPage.visible = true;
            if (hidden.indexOf("derive") >= 0) return win.fail("no picture is on screen, but a screenshot would wait for the derive: '" + hidden + "'");
            win.watching = true;
            win.release();
            return win.next("derived");
        }
        case "derived": {
            const b = win.block(win.pic);
            if (b === null) return win.later("the block");
            // (Every tick, not only every frame: the picture on screen does not change meanwhile.)
            win.holdsNow(b);
            const p = win.pictureOf(b);
            if (b.block.url === win.urlA || p.status !== Image.Ready) return win.later("the new URL drawn");
            // The list of URLs is filled by the frames (onFrameSwapped), not by this tick: a tick
            // that reads the new URL before the frame that draws it has run finds one URL in the
            // list although the URL did change (6 copies side by side: 2 of 18 runs). Wait for the
            // frame; a URL that no frame ever shows fails as "never happened".
            if (win.urls.length < 2) return win.later("a frame that draws the new URL: " + JSON.stringify(win.urls));
            if (win.frames < 3) return win.later("three frames");
            win.watching = false;
            win.done();
            // (For a look at what held it, with --no-capture.)
            console.log("screenshotHolds seen after the release, in samples: " + JSON.stringify(win.seenHolds));
            // The derive count is 0 as soon as the core has published; the events behind it
            // (the pump was slowed down) and the reads of the models held the screenshot.
            if (win.holes.length > 0) return win.fail("a screenshot would have been taken with the old picture on screen (frames " + JSON.stringify(win.holes) + "; holds seen " + JSON.stringify(win.seenHolds) + ")");
            if (!win.seenHolds["events"] || !win.seenHolds["queries"]) return win.fail("the slowed-down pump never showed as events and queries: " + JSON.stringify(win.seenHolds));
            return win.next("follow");
        }
        case "follow": {
            // The reader goes back to the newest message: the view follows it again.
            win.view.rowList.positionViewAtBeginning();
            return win.next("follow-set");
        }
        case "follow-set": {
            const list = win.view.rowList;
            const b = win.blk;
            if (!win.view.followNewest || !list.atNewest()) return win.later("the view following the newest message (follows " + win.view.followNewest + ", " + list.below() + " px from the newest end)");
            // The picture is the newest row: its bottom edge is the viewport's (minus the margin).
            const bottom = b.mapToItem(list, 0, b.height).y;
            if (Math.abs(list.height - list.bottomMargin - bottom) > 40) return win.fail("precondition: the picture is not the newest row at the bottom of the viewport (its bottom is at " + bottom + " of " + list.height + ")");
            win.followBefore = b.height;
            win.followBase = list.below();
            win.followWatch = true;
            // The row grows by a lot: the shape of its picture changes (as a download that
            // corrects a claimed size does).
            b.block = Object.assign({}, b.block, {
                "width": 90,
                "height": 300
            });
            return win.next("follow-grown");
        }
        case "follow-grown": {
            const list = win.view.rowList;
            const b = win.blk;
            const grown = b.height - win.followBefore;
            if (grown < 50 || win.followFrames < 2) return win.later("the newest row taller (" + win.followBefore + " -> " + b.height + ", " + win.followFrames + " frames)");
            win.followWatch = false;
            if (win.followLost || !win.view.followNewest) return win.fail("the view stopped following the newest message while its row grew");
            if (win.followWorst > 1) return win.fail("a frame showed the viewport " + win.followWorst + " px from the newest end while the newest row grew by " + grown + " (" + win.followFrames + " frames)");
            if (Math.abs(list.below() - win.followBase) > 1) return win.fail("the view is " + (list.below() - win.followBase) + " px from where it was at the newest end after the newest row grew by " + grown);
            console.log("followed newest row: it grew by " + grown + " px, worst distance " + win.followWorst + " px over " + win.followFrames + " frames (at the newest end: " + win.followBase + ")");
            return win.next("other");
        }
        case "other": {
            // A tile is reused by index: the same item gets ANOTHER attachment's picture. It
            // must not hold this one's pixels while that one loads.
            const b = win.blk;
            const p = win.pictureOf(b);
            if (!b || !p || p.status !== Image.Ready) return win.later("the block");
            const first = win.spare[0];
            b.block = Object.assign({}, b.block, {
                "attachment": first,
                "url": "image://kourier/thumb/" + first + "?v=1"
            });
            if (p.status !== Image.Loading) return win.fail("precondition: the next picture is not loading (status " + p.status + ")");
            if (b.pictureHeld) return win.fail("another attachment's picture holds the previous one's pixels");
            return win.next("other-ready");
        }
        case "other-ready": {
            const p = win.pictureOf(win.blk);
            if (p.status !== Image.Ready) return win.later("the other picture drawn");
            if (!win.blk.pictureHeld) return win.fail("a decoded picture is not shown");
            // The same attachment, a URL that fails: the pixels are gone (nothing is drawn),
            // and the next URL does not count as held though `paintedWidth` is still set.
            const b = win.blk;
            b.block = Object.assign({}, b.block, {
                "url": "image://kourier/nonsense/" + win.spare[0]
            });
            return win.next("failed");
        }
        case "failed": {
            const b = win.blk;
            const p = win.pictureOf(b);
            if (p.status !== Image.Error) return win.later("the failed load");
            b.block = Object.assign({}, b.block, {
                "url": "image://kourier/thumb/" + win.spare[0] + "?v=2"
            });
            if (p.status !== Image.Loading) return win.fail("precondition: the URL after the failed one is not loading (status " + p.status + ")");
            if (b.pictureHeld) return win.fail("a URL after a failed one counts as held (the old paintedWidth " + p.paintedWidth + ")");
            return win.next("viewer");
        }
        case "viewer": {
            const a = win.spare[1];
            const b = win.spare[2];
            const v = win.openAlbumViewer([win.viewerItem(a, "image://kourier/att/" + a + "?v=1"), win.viewerItem(b, "image://kourier/att/" + b + "?v=1")], 0, 0);
            if (!v) return win.fail("no viewer");
            win.viewer = v;
            return win.next("viewer-first");
        }
        case "viewer-first": {
            const v = win.viewer;
            const still = win.find(v.contentItem, c => c.objectName === "still");
            if (!v.opened || still === null || still.status !== Image.Ready || !v.stillHeld) return win.later("the first picture of the album");
            // The next picture: the counter moves and the spinner shows, the previous picture
            // is not kept under it.
            v.step(1);
            if (v.index !== 1) return win.fail("the viewer did not step");
            if (still.status !== Image.Loading) return win.fail("precondition: the next picture is not loading (status " + still.status + ")");
            if (v.stillHeld) return win.fail("the viewer holds the previous picture under the next one's counter");
            const spinner = win.find(v.contentItem, c => c.running !== undefined && c.still !== undefined);
            if (spinner === null || !spinner.running) return win.fail("no spinner while the next picture loads");
            return win.next("viewer-second");
        }
        case "viewer-second": {
            const v = win.viewer;
            const still = win.find(v.contentItem, c => c.objectName === "still");
            if (still.status !== Image.Ready || !v.stillHeld) return win.later("the second picture");
            // The same picture, its URL fails, then another URL of it loads: not held.
            const b = win.spare[2];
            v.items = [v.items[0], win.viewerItem(b, "image://kourier/nonsense/" + b)];
            return win.next("viewer-failed");
        }
        case "viewer-failed": {
            const v = win.viewer;
            const still = win.find(v.contentItem, c => c.objectName === "still");
            if (still.status !== Image.Error) return win.later("the failed load in the viewer");
            const b = win.spare[2];
            v.items = [v.items[0], win.viewerItem(b, "image://kourier/att/" + b + "?v=2")];
            if (still.status !== Image.Loading) return win.fail("precondition: the URL after the failed one is not loading in the viewer (status " + still.status + ")");
            if (v.stillHeld) return win.fail("the viewer holds a picture after a failed load (the old paintedWidth " + still.paintedWidth + ")");
            Qt.exit(0);
        }
        }
    }

    property var viewer: null
}
"##;

/// Runs `f` until it returns `Some`, for up to `secs` seconds.
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

/// Three downloaded pictures of the fixture that no window has drawn yet.
#[allow(clippy::disallowed_methods)] // The test's main thread before Qt starts, not the Qt thread.
fn spare_pictures(app: &AppHandle) -> Vec<AttachmentId> {
    wait_for(60, || {
        let mut found = Vec::new();
        for n in 1..=60 {
            let id = AttachmentId(n);
            let answer = app.runtime().block_on(app.query(Query::Attachment(id)));
            if let Ok(QueryData::Attachment(Some(a))) = answer
                && a.state == AttachmentState::Local
                && a.mime.as_deref().is_some_and(|m| m == "image/jpeg")
                && a.thumb.is_some()
            {
                found.push(id);
            }
        }
        (found.len() >= 3).then_some(found)
    })
}

fn main() {
    support::answer_list("retain_ui");
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
    let imported = fixture
        .unavailable_conversation
        .expect("the chat of attachments that cannot be had");
    let spare = spare_pictures(&handle);

    // The core derives nothing more until the script asks for it with a request to this
    // port: the time between "downloaded" and "thumbnail published" is the script's.
    let media = handle.media();
    // (After the fixture's own pictures were derived: the only job left is the one the script
    // starts, so nothing else keeps a query in flight or the derive count above 0.)
    wait_for(60, || (media.deriving() == 0).then_some(()));
    media.hold_derives();
    let listener = TcpListener::bind("127.0.0.1:0").expect("a local port");
    let port = listener.local_addr().expect("its address").port();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut request = [0u8; 1024];
            let n = stream.read(&mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            if request.starts_with("GET /release") {
                // The pump is slow from now on: the events of the derive wait in the channel.
                kourier_qt::pump::set_test_delay(PUMP_DELAY);
                media.release_derives();
            } else if request.starts_with("GET /done") {
                kourier_qt::pump::set_test_delay(Duration::ZERO);
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    let script = SCRIPT
        .replace("IMPORTED_CHAT", &imported.0.to_string())
        .replace("PORT", &port.to_string())
        .replace(
            "SPARE",
            &spare
                .iter()
                .take(3)
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/retain_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test a picture keeps its pixels while its URL changes ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    app.shutdown();
    println!(
        "test a picture keeps its pixels while its URL changes ... ok\n\ntest result: ok. 1 passed; 0 failed"
    );
}
