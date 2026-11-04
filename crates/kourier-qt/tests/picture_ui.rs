//! Picture quality in the timeline (P1, `target/next/picture-quality.md` §6.3): a received
//! picture is drawn from its FILE, decoded to the bubble's physical size, wherever the 400 px
//! thumbnail cannot cover that size. The old rule drew the thumbnail always; it was stretched
//! 1.9 times for a phone photo at scale 2 and 1.6 times for a phone screenshot at scale 1, and
//! nothing noticed, because the fixture's pictures are smooth gradients drawn at scale 1.
//!
//! This test needs what those lacked: pictures of fine detail (stripes of 6 and 8 px, two lines
//! of text, thin diagonals: `detail_image`), sent through the real core so each has its file
//! AND its derived thumbnail, shown by the real `ImageBlock.qml` in a window on a display of
//! scale 1 and 2 (and, by hand, the user's 1.25; `QT_SCALE_FACTOR`, honoured by the offscreen platform), then
//! grabbed and compared with the FILE decoded whole and scaled to the same physical size by
//! Qt's smooth scaling (`picture_probe`): how much of its fine detail the grab keeps (the
//! variance of the luma's Laplacian, whatever the alignment: a picture stretched from a smaller
//! one keeps a fraction of it) and that it is the right picture (the peak signal-to-noise ratio
//! of the luma averaged in blocks). The old rule fails it at scales 1 and 2 by a factor of 5 to
//! 25; scale 1.25 is only reported (see `REPORTED`).
//!
//! The pictures are sent as own messages (`UiCommand::SendFiles` keeps the user's file and
//! derives the 400 px thumbnail at once, like a download does) rather than put into the
//! fixture: no screenshot page changes and no list picture moves. The grab comes AFTER the
//! core derived the thumbnail and the chat is opened after that, so the first request of the
//! picture already finds the thumbnail: before the derive the old rule served the file
//! (picture-quality.md §3 item 1) and would pass. The test checks that the entry has its
//! thumbnail first.
//!
//! What the provider is asked is the item's PHYSICAL size: Qt multiplies an `Image`'s
//! `sourceSize` by the device pixel ratio itself, so the QML sets it in logical pixels
//! (`Math.ceil(width)`). Before the fix `ImageBlock.qml` multiplied it as well and the provider
//! was asked dpr times too much per axis (measured: 1152x1536 for an item of 576x768 pixels at
//! scale 2); the test logs the requests (`log_image_requests`) and checks the last one of each
//! picture against the grab's size.
//!
//! Since "pictures shown whole" (`target/next/nocrop-brief.md`) the test grabs the block's
//! `picture` child, not the block: a thin picture sits whole in a box that is larger than it
//! (the bubble's own color around it), and the probe's reference is cut to the grab's size "as
//! `PreserveAspectCrop` does", which is a no-op only because the grab has the picture's own
//! shape within a pixel. The same script judges the geometry of every picture in logical
//! pixels (`judge`: `Image.PreserveAspectFit`, the rect within one pixel of the picture's
//! shape, the box within the limits, the picture centred, and the pinned sizes of the phone
//! screenshot, the strip, the panorama, the tiny and the narrow one) and `picture_ui` checks
//! that the provider is asked for the rect's physical size and that it is answered with it
//! (the cover within a pixel of the request is the request: `imageprovider.cpp`).
//!
//! `harness = false`: the parent runs one child per display scale (a process has one scale),
//! each a real core over the fake network, the real `ImageBlock` and the real provider on the
//! offscreen platform. `KOURIER_PICTURE_UI_SHOTS=<dir>` keeps the grabs for a look,
//! `KOURIER_PICTURE_UI_SCALES=2,1.25` runs only those scales.

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{
    OriginKind, Query, QueryData, RequestId, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_app::{App, AppHandle, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentId, Body, ConversationId};
use kourier_qt::media::{Codec, Image, resolve};
use kourier_qt::{UiContext, UiOptions};
use kourier_viewmodel::media_layout::{TOTAL_HARD, album_rows, tile_aspect};

/// The display scales that are judged: 1 (where only the tall pictures need the file) and 2
/// (the picture-quality.md table's worst column).
const SCALES: [&str; 2] = ["1", "2"];
/// The display scale of the user's own PC (125 %, 2026-10-13). The sharpness of its pictures is
/// only REPORTED: the item then sits between device pixels, the scaler's loss on fine stripes
/// is as big as the thumbnail's, and no threshold tells the two apart; the rule itself is a unit
/// test of the core (`timeline_blob`). What the provider is asked for and the corner of an avatar
/// are judged at every scale, this one included.
const REPORTED: &str = "1.25";

/// What a picture that comes from its file must keep of the reference's detail: the variance of
/// the Laplacian of the grab over the reference's (the reference is the file decoded whole and
/// scaled to the item's size by Qt's smooth scaling). Measured on the new rule at the two judged
/// scales: 0.74 to 1.53 (a different scaler than the reference's gives more or less moire);
/// on the old rule, the thumbnail stretched: 0.03 to 0.15. Halfway between, rounded.
const MIN_DETAIL: f64 = 0.5;
/// The least coarse peak signal-to-noise ratio (dB, luma after averaging 8 x 8 pixels: the right
/// picture at the right place; the stripes do not count) against the file scaled smoothly.
/// Measured on the new rule: 25.6 dB (the phone screenshot at scale 1, whose crop lands on half
/// a pixel) to 44 dB; the stretched thumbnail is the same picture, only soft, so this guards the
/// picture and its place and `MIN_DETAIL` guards the sharpness.
const MIN_COARSE_PSNR_DB: f64 = 18.0;

/// A picture of the test: `(name, width, height)`. Portrait 3:4 and a phone screenshot are the
/// ones the thumbnail never covered at scale 2 (the screenshot not even at scale 1); the
/// landscape one is covered at the user's scale 1.25 and not at 2.
const PICTURES: [(&str, u32, u32); 3] = [
    ("portrait", 1500, 2000),
    ("screenshot", 1080, 2340),
    ("landscape", 2000, 1500),
];

/// Shapes the box and the picture rect are judged on (a picture is shown whole, never cropped:
/// `ImageBlock.qml`), not the sharpness: a thin strip, a panorama, a tiny picture and a tiny
/// tall one (40 x 400, which is raised to 43 x 432, not to the minimum width). What the
/// provider is asked for is judged for them as for the others.
const SHAPES: [(&str, u32, u32); 4] = [
    ("strip", 300, 6000),
    ("panorama", 6000, 300),
    ("tiny", 40, 40),
    ("narrow", 40, 400),
];

/// Every picture of the test with whether its sharpness is judged: the three of `PICTURES`.
fn all_pictures() -> Vec<(&'static str, u32, u32, bool)> {
    PICTURES
        .iter()
        .map(|p| (p.0, p.1, p.2, true))
        .chain(SHAPES.iter().map(|p| (p.0, p.1, p.2, false)))
        .collect()
}

const SCRIPT: &str = r##"
import QtQuick
import QtQuick.Window
import QtQml
import org.kde.kirigami as Kirigami
import app.kourier

// The window is the root (a Window inside an Item that has no window never shows, and a window
// that does not show cannot be grabbed).
Window {
    id: stage
    width: 400
    height: 640
    visible: true
    color: "white"

    Item {
    id: test

    property string step: "start"
    property int index: 0
    property var shownBlock: null
    property int settle: 0
    readonly property var wanted: [IDS]
    readonly property var names: [NAMES]
    readonly property var dims: [DIMS]
    // The rows `kourier_viewmodel::media_layout` chose for the first five of them.
    readonly property var albumRows: [ALBUMROWS]
    // The album is shown three times: the pictures here, the same not downloaded, and not
    // downloaded in the narrowest bubble (`avail` 0 is the width limit).
    readonly property var variants: [{ "remote": false, "avail": 0 }, { "remote": true, "avail": 0 }, { "remote": true, "avail": 120 }]
    property int variant: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    // The first descendant of `item` with this objectName (the block's `picture` child).
    function child(item: Item, name: string): var {
        for (let i = 0; i < item.children.length; ++i) {
            const c = item.children[i];
            if (c.objectName === name) {
                return c;
            }
            const d = child(c, name);
            if (d) {
                return d;
            }
        }
        return null;
    }

    // Whether `w` x `h` is within one pixel of a box of the ratio `a` = width / height: some
    // height in [h - 1, h + 1] has |a * height - w| <= 1 (rounding a side to a pixel costs
    // up to a pixel on each side; a thin picture's rounding costs more in the other axis).
    function nearShape(w: real, h: real, a: real): bool {
        return a * (h - 1) <= w + 1 && a * (h + 1) >= w - 1;
    }

    function near(a: real, b: real, tolerance: real): bool {
        return Math.abs(a - b) <= tolerance;
    }

    // The block of picture `index` in logical pixels: the picture is shown whole (fitted,
    // never cropped) in a box within the limits, centred in it; what the user's words and
    // the brief pin for the special shapes.
    function judge(item: Item, pic: Item, index: int) {
        const name = names[index];
        const a = dims[index][0] / dims[index][1];
        const what = name + ": box " + item.width + "x" + item.height + ", picture at " + pic.x + "," + pic.y + " " + pic.width + "x" + pic.height;
        if (pic.fillMode !== Image.PreserveAspectFit) {
            return fail(what + ": fillMode " + pic.fillMode + ", not PreserveAspectFit (the picture is cropped)");
        }
        if (!nearShape(pic.width, pic.height, a)) {
            return fail(what + ": the picture rect is not of the picture's own shape " + a);
        }
        if (item.width > Style.imageMaxWidth + 0.5 || item.height > Style.imageMaxHeight + 0.5 || item.width < Style.imageMinWidth - 0.5 || item.height < Style.imageMinHeight - 0.5) {
            return fail(what + ": the box is outside " + Style.imageMinWidth + "x" + Style.imageMinHeight + " .. " + Style.imageMaxWidth + "x" + Style.imageMaxHeight);
        }
        if (pic.x < -0.5 || pic.y < -0.5 || pic.x + pic.width > item.width + 0.5 || pic.y + pic.height > item.height + 0.5) {
            return fail(what + ": the picture is not inside its box");
        }
        if (!near(pic.x + pic.width / 2, item.width / 2, 1) || !near(pic.y + pic.height / 2, item.height / 2, 1)) {
            return fail(what + ": the picture is not centred in its box");
        }
        const tall = Style.imageMaxHeight;
        switch (name) {
        case "portrait":
        case "landscape":
            // Within both limits: the box is the picture, 16 grid units wide.
            if (!near(item.width, pic.width, 1) || !near(item.height, pic.height, 1) || !near(item.width, Style.imageMaxWidth, 1)) {
                return fail(what + ": expected the box to be the picture, as wide as the limit");
            }
            break;
        case "screenshot":
            // A phone screenshot is shown whole at the height limit (199 x 432 at 18 px grid
            // units; today it is cut out of 288 x 461).
            if (!near(item.height, tall, 1) || !near(item.width, tall * 1080 / 2340, 1) || !near(item.width, pic.width, 1) || !near(item.height, pic.height, 1)) {
                return fail(what + ": expected " + tall * 1080 / 2340 + "x" + tall + ", the box being the picture");
            }
            break;
        case "strip":
            // Narrower than the minimum width: whole, on the bubble's own colour around it.
            if (!near(item.height, tall, 1) || !near(item.width, Style.imageMinWidth, 1) || item.width - pic.width < 40) {
                return fail(what + ": expected a minimum-width box holding the thin strip");
            }
            break;
        case "panorama":
            // Lower than the minimum height: whole in a box of the minimum height.
            if (!near(item.width, Style.imageMaxWidth, 1) || !near(item.height, Style.imageMinHeight, 1) || item.height - pic.height < 20) {
                return fail(what + ": expected a minimum-height box holding the flat panorama");
            }
            break;
        case "tiny":
            // Raised to the minimum width only.
            if (!near(item.width, Style.imageMinWidth, 1) || !near(item.height, Style.imageMinWidth, 1) || !near(pic.width, item.width, 1)) {
                return fail(what + ": expected the minimum width, square");
            }
            break;
        case "narrow":
            // 40 x 400 is raised until it is 1.5 L tall (43 x 432), not to the minimum width.
            if (!near(pic.height, tall, 1) || !near(pic.width, tall / 10, 1) || !near(item.width, Style.imageMinWidth, 1) || !near(item.height, tall, 1)) {
                return fail(what + ": expected " + tall / 10 + "x" + tall + " in a minimum-width box");
            }
            break;
        }
        console.warn("SHAPE " + what + " ok");
    }

    // Whether two rectangles share area (touching edges do not).
    function overlap(a: var, b: var): bool {
        return a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
    }

    // The album of seven pictures (five tiles and "+2") in justified rows: every tile has the
    // shape of its picture within a pixel (clamped to 0.4 .. 5; a thinner picture sits whole
    // and centred in a tile of the clamped shape), every picture is shown whole and centred in
    // its tile, the tiles lie inside the grid and do not touch each other, the grid is at most
    // `TOTAL_HARD` times its width tall and the "+2" lies inside the fifth tile. Not downloaded, the
    // round badge lies whole inside its tile, even in the narrowest bubble, and the note
    // is not over it.
    function judgeAlbum(grid: Item, v: var) {
        const where = "album (" + (v.remote ? "remote" : "local") + ", " + (v.avail > 0 ? v.avail : "limit") + "): ";
        const W = grid.natural;
        const ts = [];
        for (const c of grid.children) {
            if (c.clip) {
                ts.push(c);
            }
        }
        if (grid.tiles !== 5 || grid.hidden !== 2 || ts.length !== 5) {
            return fail(where + "tiles " + grid.tiles + ", hidden " + grid.hidden + ", tile items " + ts.length);
        }
        let cells = "";
        let bottom = 0;
        for (let i = 0; i < 5; ++i) {
            const t = ts[i];
            const own = dims[i][0] / dims[i][1];
            const a = Math.min(5, Math.max(0.4, own));
            cells += " [" + t.x + "," + t.y + " " + t.width + "x" + t.height + "]";
            bottom = Math.max(bottom, t.y + t.height);
            if (!nearShape(t.width, t.height, a)) {
                return fail(where + "tile " + i + " is " + t.width + "x" + t.height + ", not of the shape " + a + " of its picture (" + dims[i] + ")");
            }
            if (t.x < 0 || t.y < 0 || t.x + t.width > W || t.y + t.height > grid.implicitHeight) {
                return fail(where + "tile " + i + " is outside the grid " + W + "x" + grid.implicitHeight + ":" + cells);
            }
            for (let j = 0; j < i; ++j) {
                if (overlap(ts[j], t)) {
                    return fail(where + "tiles " + j + " and " + i + " overlap:" + cells);
                }
            }
            const pic = child(t, "picture");
            if (!pic || pic.fillMode !== Image.PreserveAspectFit) {
                return fail(where + "tile " + i + ": the picture is cropped");
            }
            if (!nearShape(pic.width, pic.height, own)) {
                return fail(where + "tile " + i + ": the picture " + pic.width + "x" + pic.height + " is not of its own shape " + own);
            }
            if (pic.x < -0.5 || pic.y < -0.5 || pic.x + pic.width > t.width + 0.5 || pic.y + pic.height > t.height + 0.5 || !near(pic.x + pic.width / 2, t.width / 2, 1) || !near(pic.y + pic.height / 2, t.height / 2, 1)) {
                return fail(where + "tile " + i + ": the picture is not whole and centred in its tile");
            }
            if (own === a && (t.width - pic.width > 3 || t.height - pic.height > 3)) {
                return fail(where + "tile " + i + ": " + t.width + "x" + t.height + " has a bar around the picture " + pic.width + "x" + pic.height);
            }
            if (v.remote) {
                const badge = child(t, "badge");
                if (!badge || !badge.visible) {
                    return fail(where + "tile " + i + ": no badge on a picture that is not here");
                }
                if (badge.x < -0.5 || badge.y < -0.5 || badge.x + badge.width > t.width + 0.5 || badge.y + badge.height > t.height + 0.5) {
                    return fail(where + "tile " + i + " " + t.width + "x" + t.height + ": the badge " + badge.x + "," + badge.y + " " + badge.width + "x" + badge.height + " is not inside it");
                }
                const pill = child(t, "note");
                if (pill && pill.visible && (pill.x < -0.5 || pill.y < -0.5 || pill.x + pill.width > t.width + 0.5 || pill.y + pill.height > t.height + 0.5 || overlap(pill, badge))) {
                    return fail(where + "tile " + i + " " + t.width + "x" + t.height + ": the note " + pill.x + "," + pill.y + " " + pill.width + "x" + pill.height + " is outside the tile or over the badge");
                }
            }
        }
        if (bottom !== grid.implicitHeight) {
            return fail(where + "the grid is " + grid.implicitHeight + " tall, its tiles end at " + bottom);
        }
        // These five are an extreme mix (a strip, a panorama): no cut keeps both the 1.5 limit
        // and a badge-sized tile, so the layout takes the fallback, within `TOTAL_HARD`.
        if (grid.implicitHeight > TOTALHARD * W + 4 * grid.gap + 0.5) {
            return fail(where + "the grid is " + grid.implicitWidth + "x" + grid.implicitHeight + ", more than " + TOTALHARD + " times its width:" + cells);
        }
        const more = child(ts[4], "more");
        if (!more || !more.parent.visible || more.text !== "+2") {
            return fail(where + "the fifth tile does not say +2");
        }
        const at = more.mapToItem(ts[4], 0, 0);
        if (at.x < -0.5 || at.y < -0.5 || at.x + more.width > ts[4].width + 0.5 || at.y + more.height > ts[4].height + 0.5 || more.contentWidth > more.width + 0.5) {
            return fail(where + "the +2 (" + more.contentWidth + " wide) is not inside the fifth tile " + ts[4].width + "x" + ts[4].height);
        }
        console.warn("ALBUM " + where + grid.implicitWidth + "x" + grid.implicitHeight + ":" + cells + " ok");
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property var blocks
        }
    }

    // The block of the timeline for an attachment.
    function blockFor(attachment: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r) {
                continue;
            }
            for (const b of r.blocks) {
                if (b.type === "image" && b.attachment === attachment) {
                    return b;
                }
            }
        }
        return null;
    }

    // The timeline's own block for a picture (it sizes itself by the screen, as in a bubble).
    Loader {
        id: blockView
        x: 0
        y: 0
        active: false
        sourceComponent: ImageBlock {
            block: test.shownBlock
        }
    }

    // The seven pictures as one album message (the fake network packs three to a message, so
    // the block is made here from the real blocks of the pictures; its rows are Rust's). It lies
    // on a peer's bubble colour with a margin, as in the app, and THAT is grabbed: a grab of the
    // album alone saves its transparent places (beside a thin picture, the 2 px gaps) as black,
    // which looks like letterboxing when the app shows the bubble's color there.
    Rectangle {
        id: albumBackdrop
        x: 0
        y: 0
        visible: albumView.active
        width: albumView.width + 2 * albumBackdrop.margin
        height: albumView.height + 2 * albumBackdrop.margin
        readonly property real margin: 12
        color: Style.peerBubble(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor)

        Loader {
            id: albumView
            x: albumBackdrop.margin
            y: albumBackdrop.margin
            active: false
            property var block: null
            sourceComponent: AlbumBlock {
                block: albumView.block
            }
        }
    }

    // An avatar of a list's size, round (the setting's default) and then with the theme's
    // corners: the corner the provider cuts must be the corner QML asked for, whatever the scale.
    Loader {
        id: avatarView
        x: 0
        y: 0
        active: false
        sourceComponent: Avatar {
            width: 40
            height: 40
            name: "Avatar Test"
            source: App.timeline.avatarUrl
        }
    }

    Timer {
        interval: 50
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                App.openConversation(CONVERSATION);
                test.step = "open";
                break;
            case "open": {
                const b = test.blockFor(test.wanted[test.index]);
                if (!b) {
                    return;
                }
                if (b.state !== "local" || b.url.indexOf("image://kourier/thumb/") !== 0) {
                    return fail("the block of " + test.names[test.index] + ": " + b.state + " " + b.url);
                }
                test.shownBlock = b;
                blockView.active = true;
                test.settle = 0;
                test.step = "shown";
                break;
            }
            case "shown": {
                const item = blockView.item;
                if (!item || !item.shown) {
                    return;
                }
                // One more turn so the scene graph has the finished picture.
                test.settle += 1;
                if (test.settle < 4) {
                    return;
                }
                console.warn("PICTURE " + test.names[test.index] + ": item " + item.width + "x" + item.height + " logical, devicePixelRatio " + item.Screen.devicePixelRatio + " stage.visible " + stage.visible + " window " + item.Window.window + " same " + (item.Window.window === stage) + " vis " + (item.Window.window ? item.Window.window.visible : "-") + " active " + stage.active);
                // The grab is the `picture` child, not the block: the block's box holds the
                // bubble's color beside a thin picture, and the picture is what the provider is
                // asked for (its physical size) and what the probe compares with the file.
                const pic = test.child(item, "picture");
                if (!pic) {
                    return fail("the block of " + test.names[test.index] + " has no picture child");
                }
                test.judge(item, pic, test.index);
                test.step = "grabbing";
                pic.grabToImage(function (result) {
                    if (!result.saveToFile(GRABDIR + "/" + test.names[test.index] + ".png")) {
                        return test.fail("the grab was not saved");
                    }
                    test.step = "grabbed";
                });
                break;
            }
            case "grabbed":
                blockView.active = false;
                test.index += 1;
                test.step = test.index < test.wanted.length ? "open" : "album";
                break;
            case "album": {
                const blocks = [];
                for (const id of test.wanted) {
                    const b = test.blockFor(id);
                    if (!b) {
                        return;
                    }
                    blocks.push(b);
                }
                const items = blocks.map(b => Object.assign({}, b, test.variants[test.variant].remote ? { "state": "remote", "url": "", "fullUrl": "", "placeholderUrl": "", "label": "1.2 MB", "progress": 0, "ask": false } : { "url": b.url + "ALBUMMARK" }));
                albumView.block = { "type": "album", "items": items, "count": items.length, "rows": test.albumRows };
                albumView.active = true;
                test.settle = 0;
                test.step = "album shown";
                break;
            }
            case "album shown": {
                const grid = albumView.item;
                if (!grid) {
                    return;
                }
                const v = test.variants[test.variant];
                if (v.avail > 0 && grid.available !== v.avail) {
                    grid.available = v.avail;
                    return;
                }
                test.settle += 1;
                if (test.settle < 4) {
                    return;
                }
                test.judgeAlbum(grid, v);
                // (For a look: the album of the seven test pictures, five shown and "+2".)
                test.step = "album grabbing";
                albumBackdrop.grabToImage(function (result) {
                    if (!result.saveToFile(GRABDIR + "/album-" + test.variant + ".png")) {
                        return test.fail("the album grab was not saved");
                    }
                    test.step = "album grabbed";
                });
                break;
            }
            case "album grabbed":
                albumView.active = false;
                test.variant += 1;
                test.step = test.variant < test.variants.length ? "album" : "avatar";
                break;
            case "avatar":
                if (App.timeline.avatarUrl === "") {
                    return fail("the chat has no avatar");
                }
                avatarView.active = true;
                test.settle = 0;
                test.step = "avatar shown";
                break;
            case "avatar shown": {
                const item = avatarView.item;
                if (!item || !item.pictureShown) {
                    return;
                }
                test.settle += 1;
                if (test.settle < 4) {
                    return;
                }
                const kind = App.settings.roundAvatars ? "round" : "corner";
                const file = GRABDIR + "/avatar-" + kind + "-" + Math.round(item.cornerRadius * 100) + "-" + item.side + ".png";
                test.step = "avatar grabbing";
                item.grabToImage(function (result) {
                    if (!result.saveToFile(file)) {
                        return test.fail("the avatar grab was not saved");
                    }
                    test.step = "avatar grabbed";
                });
                break;
            }
            case "avatar grabbed":
                avatarView.active = false;
                if (App.settings.roundAvatars) {
                    App.settings.roundAvatars = false;
                    test.step = "avatar";
                } else {
                    test.step = "done";
                }
                break;
            case "done":
                Qt.exit(0);
                break;
            }
        }
    }

    Timer {
        interval: 90000
        running: true
        onTriggered: fail("timeout")
    }
    }
}
"##;

/// The album's copies of the blocks carry this in their picture URL (the provider ignores it):
/// the requests of the album's tiles are told apart from those of the single block.
const ALBUM_MARK: &str = "&album=1";

/// The size of the requests the provider logged for `attachment`'s `thumb/` picture, in order
/// (`album`: those of the album's tiles, else those of the single block).
fn requests_of(log: &[String], attachment: AttachmentId, album: bool) -> Vec<(i32, i32)> {
    let prefix = format!("thumb/{}?", attachment.0);
    log.iter()
        .filter_map(|line| {
            let (id, size) = line.split_once(' ')?;
            let (w, h) = size.split_once('x')?;
            if !id.starts_with(&prefix) || id.contains(ALBUM_MARK) != album {
                return None;
            }
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .collect()
}

/// The last request for `attachment`'s picture.
fn last_request(log: &[String], attachment: AttachmentId) -> Option<(i32, i32)> {
    requests_of(log, attachment, false).last().copied()
}

/// The largest side of any request for `attachment`'s picture, as `(width, height)`.
fn largest_request(log: &[String], attachment: AttachmentId) -> (i32, i32) {
    requests_of(log, attachment, false)
        .into_iter()
        .fold((0, 0), |a, r| (a.0.max(r.0), a.1.max(r.1)))
}

/// Runs `f` until it returns `Some`, for up to `secs` seconds.
fn wait_for<T>(secs: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The attachment of an own picture of `dims` in `conv`.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_picture(app: &AppHandle, conv: ConversationId, dims: (u32, u32)) -> Option<AttachmentId> {
    let page = app
        .runtime()
        .block_on(app.query(Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 50,
        })));
    let Ok(QueryData::Timeline(page)) = page else {
        panic!("the timeline of the chat");
    };
    page.rows
        .iter()
        .filter(|r| r.origin == OriginKind::OwnThisDevice)
        .filter_map(|r| match &r.body {
            Body::Media { attachments, .. } => Some(attachments),
            _ => None,
        })
        .flatten()
        .find(|a| a.dims == Some(dims))
        .and_then(|a| a.id)
}

/// Sends `path` to `conv` on its own (one message, one picture) and waits until the core
/// stored it.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn send_picture(app: &AppHandle, conv: ConversationId, request: u64, path: PathBuf) {
    let mut events = app.subscribe();
    app.send(UiCommand::SendFiles {
        conv,
        request: RequestId(request),
        paths: vec![path].into(),
        as_file: Vec::new(),
        with_location: Vec::new(),
        caption: None,
        reply_to: None,
    })
    .expect("send the picture");
    app.runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await {
                    Ok(UiEvent::FilesStored { request: r, .. }) if r == RequestId(request) => {
                        break;
                    }
                    Ok(UiEvent::ActionFailed {
                        action: "SendFiles",
                        ..
                    }) => panic!("the core refused the picture"),
                    Ok(_) => {}
                    Err(e) => panic!("events: {e}"),
                }
            }
        })
        .await
        .expect("the picture was stored in time");
    });
}

/// What the case found out about one picture, to be judged after Qt ran.
struct Shown {
    name: &'static str,
    /// Whether the sharpness and the file-or-thumbnail rule are judged (the three of
    /// `PICTURES`); the others are judged on their box and on what the provider is asked for.
    fine: bool,
    attachment: AttachmentId,
    dims: (u32, u32),
    file: Vec<u8>,
    file_len: usize,
    /// 0 when the core made no thumbnail (a picture below its size).
    thumb_len: usize,
}

/// One display scale, in this process (`QT_SCALE_FACTOR` is already in the environment).
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn run_case(scale: &str, judged: bool) {
    let dpr: f64 = scale.parse().expect("a display scale");
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let conv = handle
        .fixture()
        .and_then(|f| f.media_conversation)
        .expect("a media conversation");
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    // ---- the pictures, sent through the real core, thumbnails derived ----
    let dir = std::env::temp_dir().join(format!(
        "kourier-picture-ui-{}-{}",
        std::process::id(),
        scale.replace('.', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let grabs = dir.join("grabs");
    std::fs::create_dir_all(&grabs).expect("grab dir");
    let mut shown = Vec::new();
    for (n, (name, w, h, fine)) in all_pictures().into_iter().enumerate() {
        let jpeg = kourier_qt::bridge::qobject::detail_image(w as i32, h as i32, "jpeg");
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]), "{name}: a JPEG");
        let path = dir.join(format!("{name}.jpg"));
        std::fs::write(&path, &jpeg).expect("write the picture");
        send_picture(&handle, conv, 100 + n as u64, path);
        let attachment = wait_for(30, name, || own_picture(&handle, conv, (w, h)));
        // The core made the thumbnail with the import: the entry has both files before the
        // chat is opened, so the first request already meets it (the old rule served the
        // thumbnail from then on).
        // (A picture below the thumbnail's size may get none: only the three that are judged
        // for sharpness need one.)
        let entry = wait_for(30, name, || {
            handle
                .media()
                .resolve_blocking(attachment)
                .ok()
                .flatten()
                .filter(|e| e.file.is_some() && (!fine || e.thumb.is_some()))
        });
        assert_eq!(entry.dims, Some((w, h)), "{name}");
        let media = handle.media();
        let file = media.read(entry.file.as_ref().unwrap()).expect("the file");
        let thumb_len = entry.thumb.as_ref().map_or(0, |t| {
            let thumb = media.read(t).expect("thumbnail");
            if fine {
                assert!(
                    thumb.len() < file.len() / 3,
                    "{name}: the thumbnail is small"
                );
            }
            thumb.len()
        });
        assert_eq!(&file[..], &jpeg[..], "{name}: the file is the picture sent");
        shown.push(Shown {
            name,
            fine,
            attachment,
            dims: (w, h),
            file: file.to_vec(),
            file_len: file.len(),
            thumb_len,
        });
    }

    // ---- the real ImageBlock in a window, grabbed ----
    let ids = shown
        .iter()
        .map(|s| s.attachment.0.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let names = shown
        .iter()
        .map(|s| format!("{:?}", s.name))
        .collect::<Vec<_>>()
        .join(", ");
    let dims = all_pictures()
        .iter()
        .map(|p| format!("[{}, {}]", p.1, p.2))
        .collect::<Vec<_>>()
        .join(", ");
    // The rows of the album's five shown tiles, as `models/timeline.rs` hands them to QML.
    let aspects: Vec<f64> = all_pictures()
        .iter()
        .take(5)
        .map(|p| tile_aspect(p.1, p.2))
        .collect();
    let album_rows_js = album_rows(&aspects)
        .iter()
        .map(|r| {
            let prefix = r
                .prefix
                .iter()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>();
            format!(
                "{{ \"count\": {}, \"aspect\": {:?}, \"prefix\": [{}] }}",
                r.count,
                r.aspect,
                prefix.join(", ")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!("scale {scale} album rows {album_rows_js}");
    let script = SCRIPT
        .replace("ALBUMROWS", &album_rows_js)
        .replace("ALBUMMARK", ALBUM_MARK)
        .replace("TOTALHARD", &format!("{:?}", TOTAL_HARD))
        .replace("IDS", &ids)
        .replace("NAMES", &names)
        .replace("DIMS", &dims)
        .replace("CONVERSATION", &conv.0.to_string())
        .replace("GRABDIR", &format!("{:?}", grabs.to_string_lossy()));
    let mut engine = QQmlApplicationEngine::new();
    kourier_qt::bridge::qobject::log_image_requests(true);
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/picture_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    let requests = kourier_qt::bridge::qobject::take_image_request_log();
    kourier_qt::bridge::qobject::log_image_requests(false);
    drop(engine);
    kourier_qt::media::set_store(None);
    assert_eq!(code, 0, "scale {scale}: the script failed (exit {code})");

    // ---- judged: which file the timeline asked for, and how sharp it came out ----
    let keep = std::env::var_os("KOURIER_PICTURE_UI_SHOTS").map(PathBuf::from);
    if let Some(keep) = &keep {
        // The album grabs (local, remote, remote at 120 px), for a look.
        std::fs::create_dir_all(keep).expect("keep dir");
        for n in 0..3 {
            let _ = std::fs::copy(
                grabs.join(format!("album-{n}.png")),
                keep.join(format!("scale-{scale}-album-{n}.png")),
            );
        }
    }
    let mut sharp = 0;
    // `hard` problems fail at every scale (what the provider is asked for, the avatar's corner);
    // `problems` only where the scale is judged (the sharpness of the pictures).
    let mut hard = Vec::new();
    let mut problems = Vec::new();
    for s in &shown {
        let grab = std::fs::read(grabs.join(format!("{}.png", s.name))).expect("the grab");
        let probe = kourier_qt::bridge::qobject::picture_probe(
            s.file.clone(),
            Codec::Jpeg as u8,
            grab.clone(),
        );
        assert_eq!(probe.len(), 7, "scale {scale} {}: the probe", s.name);
        let (psnr, got, want, smooth_psnr) = (probe[0], probe[1], probe[2], probe[5]);
        let (w, h) = (probe[3] as i32, probe[4] as i32);
        // What the provider was asked for this picture: the last request is the one of the
        // finished item, and it is the item's physical size (the grab's), not more.
        let asked = last_request(&requests, s.attachment)
            .unwrap_or_else(|| panic!("scale {scale} {}: the provider was never asked", s.name));
        if (asked.0 - w).abs() > 1 || (asked.1 - h).abs() > 1 {
            hard.push(format!(
                "scale {scale} {}: the provider was asked for {}x{} but the item is {w}x{h} device pixels (sourceSize is logical: Qt multiplies it by the device pixel ratio)",
                s.name, asked.0, asked.1
            ));
        }
        let largest = largest_request(&requests, s.attachment);
        if largest.0 > w + 1 || largest.1 > h + 1 {
            hard.push(format!(
                "scale {scale} {}: a request of {}x{} was made for an item of {w}x{h}",
                s.name, largest.0, largest.1
            ));
        }
        // The album's tiles (the pictures are shown whole there too): every request has the
        // picture's own shape and is no larger than the grid is wide (a request of dpr squared
        // would be).
        let tile_requests = requests_of(&requests, s.attachment, true);
        println!(
            "scale {scale} {:>10}: the album's tile asked for {tile_requests:?}",
            s.name
        );
        // One request per tile, for the finished size (anchors are applied after a picture
        // first asks, so a tile whose `ImageBlock` is anchored asks once for the size the block
        // has before them: a decode wasted on every first tile of a row).
        if tile_requests.len() > 1 {
            hard.push(format!(
                "scale {scale} {}: the album's tile asked the provider {} times: {tile_requests:?}",
                s.name,
                tile_requests.len()
            ));
        }
        for (aw, ah) in tile_requests {
            let own = f64::from(s.dims.0) / f64::from(s.dims.1);
            let (fw, fh) = (f64::from(aw), f64::from(ah));
            if own * (fh - 1.0) > fw + 1.0 || own * (fh + 1.0) < fw - 1.0 {
                hard.push(format!(
                    "scale {scale} {}: the album's tile asked for {aw}x{ah}, not the shape {own:.3} of the picture",
                    s.name
                ));
            }
            if f64::from(aw) > 288.0 * dpr + 1.0 {
                hard.push(format!(
                    "scale {scale} {}: the album's tile asked for {aw}x{ah}, more than the grid is wide ({} device pixels)",
                    s.name,
                    288.0 * dpr
                ));
            }
        }
        let serve = |(w, h): (i32, i32)| match resolve(
            &handle,
            &format!("thumb/{}?v=1", s.attachment.0),
            w,
            h,
        ) {
            Image::Encoded { bytes, .. } => bytes.len(),
            other => panic!("{}: expected a picture, got {other:?}", s.name),
        };
        let served = serve(asked);
        let from_file = served == s.file_len;
        assert!(
            from_file || (s.thumb_len > 0 && served == s.thumb_len),
            "{}: neither",
            s.name
        );
        println!(
            "scale {scale} {:>10}: item {w}x{h}, provider asked {}x{}, served the {}; coarse PSNR vs the provider's answer {psnr:.1} dB, vs the file scaled smoothly {smooth_psnr:.1} dB, detail {:.2} (grab {got:.0} / reference {want:.0}); asked {w}x{h} it would serve the {}",
            s.name,
            asked.0,
            asked.1,
            if from_file { "FILE" } else { "thumbnail" },
            got / want,
            if serve((w, h)) == s.file_len {
                "file"
            } else {
                "thumbnail"
            },
        );
        if let Some(keep) = &keep {
            std::fs::create_dir_all(keep).expect("keep dir");
            let _ = std::fs::copy(
                grabs.join(format!("{}.png", s.name)),
                keep.join(format!("scale-{scale}-{}.png", s.name)),
            );
        }
        if !s.fine {
            // Judged on its box in the script and on what the provider was asked for, above.
            continue;
        }
        // The rule: the file wherever the thumbnail (the long edge 400) cannot cover the ask.
        let (pw, ph) = PICTURES
            .iter()
            .find(|p| p.0 == s.name)
            .map(|p| (f64::from(p.1), f64::from(p.2)))
            .expect("a picture");
        let need = (f64::from(asked.0) / pw).max(f64::from(asked.1) / ph);
        let have = (400.0 / pw.max(ph)).min(1.0);
        if from_file != (need > have) {
            problems.push(format!(
                "scale {scale} {}: asked {}x{} of {pw}x{ph} (needs {need:.3}, the thumbnail keeps {have:.3}) and the {} was served",
                s.name,
                asked.0,
                asked.1,
                if from_file { "file" } else { "thumbnail" },
            ));
        }
        // What is drawn from the file is as sharp as the file scaled to that size.
        if need > have {
            sharp += 1;
            if got / want < MIN_DETAIL {
                problems.push(format!(
                    "scale {scale} {}: only {:.0} % of the detail of the file at {w}x{h} (PSNR {psnr:.1} dB): the picture was stretched from the thumbnail",
                    s.name,
                    100.0 * got / want
                ));
            }
            if smooth_psnr < MIN_COARSE_PSNR_DB {
                problems.push(format!(
                    "scale {scale} {}: coarse PSNR {smooth_psnr:.1} dB against the file at {w}x{h}",
                    s.name
                ));
            }
        }
    }
    // The screenshot always needs the file (its thumbnail is 185 px wide), the portrait from
    // scale 2 on.
    if sharp < 1 {
        problems.push(format!(
            "scale {scale}: no picture needed the file, the phone screenshot always does"
        ));
    }
    // ---- the avatar's corner: the cut the provider made against the corner QML asked for ----
    let mut avatars = 0;
    for entry in std::fs::read_dir(&grabs).expect("the grabs").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name
            .strip_prefix("avatar-")
            .and_then(|r| r.strip_suffix(".png"))
        else {
            continue;
        };
        // avatar-<round|corner>-<corner radius in 1/100 logical px>-<side in logical px>
        let parts: Vec<&str> = rest.split('-').collect();
        let [kind, radius100, side] = parts[..] else {
            panic!("the avatar grab's name: {name}");
        };
        let radius: f64 = radius100.parse::<f64>().expect("a radius") / 100.0;
        let side: f64 = side.parse().expect("a side");
        let grab = std::fs::read(entry.path()).expect("the avatar grab");
        let probe = kourier_qt::bridge::qobject::corner_probe(grab);
        assert_eq!(probe.len(), 3, "scale {scale} {kind} avatar: the probe");
        // The transparent corner of a quarter circle of radius r is r² (1 - pi/4) pixels.
        let measured = (probe[2] / (1.0 - std::f64::consts::FRAC_PI_4)).sqrt();
        let expected = radius * dpr;
        println!(
            "scale {scale} {kind:>6} avatar: {side} logical px = {} device px, corner radius asked {expected:.2} device px, drawn {measured:.2}",
            probe[0]
        );
        if (probe[0] - side * dpr).abs() > 1.0 {
            hard.push(format!(
                "scale {scale} {kind} avatar: grab is {} px wide, not {} device px",
                probe[0],
                side * dpr
            ));
        }
        if (measured - expected).abs() > 0.2 * expected + 0.6 {
            hard.push(format!(
                "scale {scale} {kind} avatar: the corner is drawn with radius {measured:.2} device px, QML asked for {expected:.2} (the provider cut it for another size than the item shows)"
            ));
        }
        avatars += 1;
    }
    if avatars != 2 {
        hard.push(format!("scale {scale}: {avatars} avatar grabs, expected 2"));
    }
    app.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(hard.is_empty(), "{}", hard.join("\n"));
    if judged {
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    } else {
        // Reported, not judged (see SCALES).
        for p in &problems {
            println!("scale {scale} (not judged): {p}");
        }
    }
}

/// Starts the case of `scale` in a child process of its own (a process has one display scale).
fn start_child(scale: &str, judged: bool) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().expect("this binary"));
    command.arg("--scale").arg(scale);
    if !judged {
        command.arg("--report-only");
    }
    command
        .env("QT_SCALE_FACTOR", scale)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start the case")
}

/// Waits for a case and prints its numbers; `Err` is what went wrong.
fn finish_child(scale: &str, child: std::process::Child) -> Result<(), String> {
    let out = child.wait_with_output().expect("wait for the case");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The numbers of the case, for the test's output.
    for line in text.lines().filter(|l| l.starts_with("scale ")) {
        println!("{line}");
    }
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("scale {scale}: {}\n{text}", out.status))
    }
}

fn main() {
    support::answer_list("picture_ui");
    let args: Vec<String> = std::env::args().collect();
    if let Some(at) = args.iter().position(|a| a == "--scale") {
        let judged = !args.iter().any(|a| a == "--report-only");
        run_case(args.get(at + 1).map_or("2", String::as_str), judged);
        return;
    }
    // Other scales by hand: `KOURIER_PICTURE_UI_SCALES=2,1.25` runs exactly those.
    let only = std::env::var("KOURIER_PICTURE_UI_SCALES").ok();
    let mut cases: Vec<(&str, bool)> = SCALES.iter().map(|s| (*s, true)).collect();
    cases.push((REPORTED, false));
    if let Some(only) = &only {
        cases.retain(|(scale, _)| only.split(',').any(|o| o == *scale));
    }
    // Side by side: each is a core, an app and a window of its own, and the cases wait on
    // timers and the render loop more than they compute.
    let running: Vec<_> = cases
        .iter()
        .map(|(scale, judged)| (*scale, start_child(scale, *judged)))
        .collect();
    let mut failed = Vec::new();
    for (scale, child) in running {
        if let Err(why) = finish_child(scale, child) {
            failed.push(why);
        }
    }
    if failed.is_empty() {
        println!(
            "test pictures are drawn from the file where the thumbnail cannot cover them ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
    } else {
        for why in &failed {
            eprintln!("{why}");
        }
        println!(
            "test pictures are drawn from the file where the thumbnail cannot cover them ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
