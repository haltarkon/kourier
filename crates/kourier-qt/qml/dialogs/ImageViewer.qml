pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The full-window picture viewer (§8.3 dialogs): the full-size picture from the image provider,
// decoded at the window's size (a GIF plays from its tmpfs copy). With the pictures of an album
// it steps through them: Left and Right (or Page Up and Page Down), Home and End, the buttons
// at the sides; the counter says where it is. Escape, the close button or a click beside the
// picture close it; the open button hands the picture to the desktop's default application for
// its type (after a confirmation for unusual types). With its message (`message`) the viewer
// follows that message's row: a picture that is not downloaded is fetched from here and shows
// when it arrives, progress is shown meanwhile, and a picture that is deleted leaves the album
// (the viewer closes with the last one).
QQC2.Popup {
    id: root

    // What is shown: one picture, or every picture of an album. Each item has `attachment`,
    // `url` (full size; empty while it is not downloaded), `animated`, `media` (a well-known
    // picture type: opening it in another application needs no confirmation), `kind`,
    // `state` and `placeholder`.
    property var items: []
    property int index: 0
    // The message the pictures belong to; 0: what was handed over is shown as it is.
    property real message: 0
    // The gallery slot ("group", "person", "media") the pictures came from, when a square of "Media
    // and Files" opened the viewer; empty: the timeline's. The message is then followed through the
    // viewer's OWN query (the slot "viewer", opened by the window with `App.showGalleryViewer`): the
    // whole album, read at the message, not the six squares of a section or the pages of a level, and
    // not the open timeline (the square may belong to another chat). Until its first answer what the
    // viewer opened with is shown.
    property string gallery: ""

    // The message's row changed: show it as it is now, staying on the same picture.
    function refresh(): void {
        let fresh;
        if (root.gallery !== "") {
            if (!App.galleryLoaded("viewer")) {
                return;
            }
            fresh = App.galleryViewerItems("viewer", root.message);
        } else {
            // Not among the loaded rows (the window of the timeline moved on): what is shown stays.
            if (!App.viewerMessageLoaded(root.message)) {
                return;
            }
            fresh = App.viewerItems(root.message);
        }
        // Deleted (the message has no picture left in the viewer's query, or in the timeline), or no
        // picture is left of it.
        if (fresh.length === 0) {
            root.close();
            return;
        }
        const shown = root.attachment;
        let at = -1;
        for (let i = 0; i < fresh.length; ++i) {
            if (fresh[i].attachment === shown) {
                at = i;
            }
        }
        root.items = fresh;
        // The picture that was shown is gone: its neighbour takes its place.
        root.index = at >= 0 ? at : Math.min(root.index, fresh.length - 1);
    }

    Connections {
        target: App
        enabled: root.message > 0 && root.visible
        function onTimelineRowsChanged(): void {
            if (root.gallery === "") {
                root.refresh();
            }
        }
        // The rows of the viewer's own query changed (a deletion, a timer that ran out, a download, a
        // picture added to the album): the same refresh, from its rows.
        function onGalleryChanged(slot: string): void {
            if (root.gallery !== "" && slot === "viewer") {
                root.refresh();
            }
        }
    }

    readonly property int count: root.items.length
    readonly property var current: root.index >= 0 && root.index < root.count ? root.items[root.index] : null
    readonly property real attachment: root.current ? root.current.attachment : 0
    readonly property string url: root.current ? root.current.url : ""
    readonly property bool animated: root.current ? root.current.animated === true : false
    readonly property bool media: root.current ? root.current.media === true : false
    readonly property bool video: root.current ? root.current.kind === "video" : false
    readonly property string mediaState: root.current ? root.current.state : ""
    // Gone here, and Download asks the user's phone to upload it again (the core decides it).
    readonly property bool ask: root.current ? root.current.ask === true : false
    // The core lists this picture as one it may save (it is on this PC, and not a view-once one).
    readonly property bool saveOffered: root.current ? root.current.save === true : false
    // What the button fetches: a download, a retry, or the question to the phone.
    readonly property bool downloadable: root.mediaState === "remote" || root.mediaState === "failed" || root.ask
    // How far its download is (0 … 1).
    readonly property real progress: root.current && root.current.progress ? root.current.progress : 0
    // Nothing to show full size yet: not downloaded, or a clip another application plays.
    readonly property bool absent: root.current !== null && !root.animated && root.url === ""
    // The current picture is on screen (decoded, or playing).
    // It plays: animated by its type, unless its copy turned out not to be a movie by content
    // (the type is the sender's claim). Such a picture shows as its still, as it does in the
    // timeline; a viewer that follows its message is told `animated` by the row, which
    // cannot know.
    readonly property bool movie: root.animated && !copy.notMovie
    readonly property bool shown: root.movie ? moving.status === AnimatedImage.Ready : (!root.absent && root.stillHeld)
    // The attachment whose pixels `still` holds (set when decoded, kept while the next URL
    // loads, cleared when there is none or it failed: `paintedWidth` keeps its old value after
    // an error, though nothing is drawn).
    property real heldFor: 0
    // The still is decoded, or still the previous pixels of THIS picture while a new URL of
    // it loads (`retainWhileLoading`: the URL changes once more after its download, when the
    // core has derived the thumbnail): the spinner does not flash over it. Stepping to another
    // picture of an album shows the spinner, never the previous picture under the new counter.
    readonly property bool stillHeld: still.status === Image.Ready || (still.status === Image.Loading && root.heldFor === root.attachment && still.paintedWidth > 0)

    // ---- Zoom and pan (picture zoom) ----
    // The picture area (`viewport`, clipped) is today's box. The STATE is `fitMode` (the picture is
    // fitted, today's look) or, when zoomed, `zoomW` (the shown width in logical pixels) and the
    // picture point at the viewport's centre (`cx`, `cy`, fractions of the picture). Everything
    // drawn is derived: the shown rectangle is centred on an axis where it is smaller than the
    // viewport and stops at the viewport's edges where it is larger. 100 % is one picture pixel per
    // DEVICE pixel. The decode `still` holds is today's (its box); a zoom beyond it asks `sharp`
    // once, at the natural size under the provider's cap.
    property bool fitMode: true
    property real zoomW: 0
    property real cx: 0.5
    property real cy: 0.5
    // `sharp` (the second, zoomed decode) is wanted; set when the zoom passes what `still` holds.
    property bool sharpAsked: false
    // How many times `sharp` was asked (a source set), for the tests.
    property int sharpRequests: 0
    readonly property var zoomLadder: [10, 15, 25, 33, 50, 67, 100, 150, 200, 300, 400, 600, 800]
    readonly property real vw: viewport.width
    readonly property real vh: viewport.height
    // Today's Fit rectangle (what the decoded picture paints in the box).
    readonly property real fitW: root.movie ? moving.paintedWidth : still.paintedWidth
    readonly property real fitH: root.movie ? moving.paintedHeight : still.paintedHeight
    readonly property real shownW: root.fitMode ? root.fitW : root.zoomW
    readonly property real shownH: root.fitW > 0 ? root.shownW * root.fitH / root.fitW : 0
    readonly property bool fitted: root.fitMode
    // The natural size in pixels (0 = unknown): the core's dims; else, for a picture decoded smaller
    // than its box on both axes, the decode itself; else what `sharp` decoded (capped).
    readonly property int itemWidth: root.current && root.current.width > 0 && root.current.height > 0 ? root.current.width : 0
    readonly property int itemHeight: root.current && root.current.width > 0 && root.current.height > 0 ? root.current.height : 0
    readonly property int naturalWidth: {
        if (root.itemWidth > 0) {
            return root.itemWidth;
        }
        if (root.movie) {
            return moving.status === AnimatedImage.Ready ? Math.round(moving.implicitWidth) : 0;
        }
        if (sharp.status === Image.Ready) {
            return Math.round(sharp.implicitWidth * stage.dpr);
        }
        if (still.status === Image.Ready && still.implicitWidth * stage.dpr < root.vw * stage.dpr - 1 && still.implicitHeight * stage.dpr < root.vh * stage.dpr - 1) {
            return Math.round(still.implicitWidth * stage.dpr);
        }
        return 0;
    }
    readonly property int naturalHeight: root.naturalWidth > 0 && root.fitW > 0 ? Math.round(root.naturalWidth * root.fitH / root.fitW) : 0
    // Shown logical pixels per picture pixel (0 = natural size unknown).
    readonly property real zoomScale: root.naturalWidth > 0 ? root.shownW / root.naturalWidth : 0
    readonly property int zoomPercent: Math.round(root.zoomScale * stage.dpr * 100)
    readonly property real fitPercent: root.naturalWidth > 0 ? root.fitW / root.naturalWidth * stage.dpr * 100 : 0
    // The unsnapped top-left of the shown rectangle in the viewport.
    readonly property real rawX: root.clampAxis(root.vw / 2 - root.cx * root.shownW, root.shownW, root.vw)
    readonly property real rawY: root.clampAxis(root.vh / 2 - root.cy * root.shownH, root.shownH, root.vh)
    // At 100 % and above the top-left lies on a whole DEVICE pixel (window coordinates), so a
    // display scale of 125 % still shows real pixels 1:1.
    readonly property bool realPixels: !root.fitMode && root.naturalWidth > 0 && root.shownW * stage.dpr >= root.naturalWidth - 0.5
    readonly property real rectX: root.realPixels ? root.snapDevice(root.rawX, 0) : root.rawX
    readonly property real rectY: root.realPixels ? root.snapDevice(root.rawY, 1) : root.rawY
    readonly property var shownRect: ({
            "x": viewport.x + root.rectX,
            "y": viewport.y + root.rectY,
            "w": root.shownW,
            "h": root.shownH
        })
    readonly property var viewportRect: ({
            "x": viewport.x,
            "y": viewport.y,
            "w": root.vw,
            "h": root.vh
        })
    // The level menu (for the tests).
    readonly property var zoomMenu: zoomMenu
    readonly property bool sharpReady: sharp.status === Image.Ready
    readonly property bool canPan: root.shown && !root.fitMode && (root.shownW > root.vw + 0.5 || root.shownH > root.vh + 0.5)
    readonly property bool canZoomIn: root.shown && root.fitW > 0 && root.shownW < root.maxW() - 0.5
    readonly property bool canZoomOut: root.shown && root.fitW > 0 && root.shownW > root.minW() + 0.5

    function clampAxis(r: real, size: real, box: real): real {
        return size <= box ? (box - size) / 2 : Math.max(box - size, Math.min(0, r));
    }

    function snapDevice(r: real, axis: int): real {
        const o = viewport.mapToItem(null, 0, 0);
        const base = axis === 0 ? o.x : o.y;
        return Math.round((base + r) * stage.dpr) / stage.dpr - base;
    }

    function minW(): real {
        if (root.fitW <= 0) {
            return 0;
        }
        return root.naturalWidth > 0 ? Math.min(root.fitW, 0.5 * root.naturalWidth / stage.dpr) : root.fitW;
    }

    function maxW(): real {
        return root.naturalWidth > 0 ? Math.max(root.fitW, 8 * root.naturalWidth / stage.dpr) : root.fitW * 8;
    }

    function resetZoom(): void {
        root.fitMode = true;
        root.sharpAsked = false;
        root.cx = 0.5;
        root.cy = 0.5;
    }

    // Put the shown rectangle's top-left at (x, y) of the viewport, within the edges.
    function placeRect(x: real, y: real, w: real, h: real): void {
        root.cx = (root.vw / 2 - root.clampAxis(x, w, root.vw)) / w;
        root.cy = (root.vh / 2 - root.clampAxis(y, h, root.vh)) / h;
    }

    // Show the picture `target` logical pixels wide, keeping the picture point under (px, py) of
    // the viewport. Within the bounds; at Fit it IS the fitted state.
    function zoomAbout(target: real, px: real, py: real): void {
        if (!root.shown || root.fitW <= 0) {
            return;
        }
        const w = Math.max(root.minW(), Math.min(root.maxW(), target));
        if (Math.abs(w - root.fitW) < 0.5) {
            root.resetZoom();
            return;
        }
        const u = (px - root.rawX) / root.shownW;
        const v = (py - root.rawY) / root.shownH;
        const h = w * root.fitH / root.fitW;
        root.fitMode = false;
        root.zoomW = w;
        root.placeRect(px - u * w, py - v * h, w, h);
        // Beyond what `still` holds, and the file has more pixels: ask for them, once.
        if (!root.sharpAsked && w > root.fitW + 0.5 && (root.naturalWidth <= 0 || root.naturalWidth > root.fitW * stage.dpr + 1)) {
            root.sharpAsked = true;
        }
    }

    // A wheel tick: a factor around the pointer; a step that crosses Fit stops at Fit.
    function zoomBy(factor: real, px: real, py: real): void {
        if (!root.shown || root.fitW <= 0) {
            return;
        }
        let w = root.shownW * factor;
        if ((root.shownW - root.fitW) * (w - root.fitW) < 0) {
            w = root.fitW;
        }
        root.zoomAbout(w, px, py);
    }

    // The next level of the ladder (and Fit) above or below, around the viewport's centre.
    function stepLevel(dir: int): void {
        if (!root.shown || root.fitW <= 0) {
            return;
        }
        const px = root.vw / 2;
        const py = root.vh / 2;
        if (root.naturalWidth <= 0) {
            root.zoomAbout(root.shownW * (dir > 0 ? 1.25 : 0.8), px, py);
            return;
        }
        const k = root.naturalWidth / stage.dpr / 100;
        const levels = root.zoomLadder.concat([root.fitW / k]).sort((a, b) => a - b);
        const lo = root.minW();
        const hi = root.maxW();
        let pick = -1;
        if (dir > 0) {
            for (let i = 0; i < levels.length; ++i) {
                if (levels[i] * k > root.shownW + 0.75 && levels[i] * k <= hi + 0.5) {
                    pick = levels[i];
                    break;
                }
            }
        } else {
            for (let i = levels.length - 1; i >= 0; --i) {
                if (levels[i] * k < root.shownW - 0.75 && levels[i] * k >= lo - 0.5) {
                    pick = levels[i];
                    break;
                }
            }
        }
        if (pick > 0) {
            root.zoomAbout(pick * k, px, py);
        }
    }

    // A level of the menu (percent of device pixels), or Fit (0).
    function zoomLevel(percent: real): void {
        if (percent <= 0) {
            root.resetZoom();
        } else if (root.naturalWidth > 0) {
            root.zoomAbout(percent / 100 * root.naturalWidth / stage.dpr, root.vw / 2, root.vh / 2);
        }
    }

    function levelIs(percent: real): bool {
        return percent <= 0 ? root.fitMode : (!root.fitMode && root.naturalWidth > 0 && root.zoomPercent === percent);
    }

    // The double click: Fit <-> real pixels at the point (from another level: real pixels).
    function toggleAt(px: real, py: real): void {
        if (root.naturalWidth <= 0) {
            if (root.fitMode) {
                root.zoomAbout(root.fitW * 2, px, py);
            } else {
                root.resetZoom();
            }
            return;
        }
        const real = root.naturalWidth / stage.dpr;
        if (!root.fitMode && Math.abs(root.shownW - real) < 0.5) {
            root.resetZoom();
        } else {
            root.zoomAbout(real, px, py);
        }
    }

    // Pan by a distance of the viewport (the keyboard).
    function panBy(dx: real, dy: real): void {
        if (root.canPan) {
            root.placeRect(root.rawX + dx, root.rawY + dy, root.shownW, root.shownH);
        }
    }

    // The point (stage coordinates) is on the VISIBLE part of the shown picture.
    function hitsPicture(x: real, y: real): bool {
        if (root.fitW <= 0 || root.fitH <= 0) {
            return false;
        }
        const r = root.shownRect;
        const x0 = Math.max(viewport.x, r.x);
        const x1 = Math.min(viewport.x + root.vw, r.x + r.w);
        const y0 = Math.max(viewport.y, r.y);
        const y1 = Math.min(viewport.y + root.vh, r.y + r.h);
        return x >= x0 && x <= x1 && y >= y0 && y <= y1;
    }

    onAttachmentChanged: root.resetZoom()

    // What the button in the middle does: fetch a picture that is not here (the viewer stays
    // and shows it when it arrives), or hand a video to another application.
    function act(): void {
        if (root.current === null) {
            return;
        }
        if (root.mediaState === "local") {
            if (root.video) {
                MediaActions.open(root.attachment, L10n.i18nc("@info", "Video"), root.media);
            }
        } else if (root.downloadable) {
            App.downloadAttachment(root.attachment);
        }
    }

    // Save As… for the picture on screen (the button and Ctrl+S).
    function save(): void {
        if (root.saveOffered && root.attachment > 0) {
            MediaActions.save([root.attachment]);
        }
    }

    function step(delta: int): void {
        const next = Math.max(0, Math.min(root.count - 1, root.index + delta));
        if (next !== root.index) {
            root.index = next;
        }
    }

    parent: QQC2.Overlay.overlay
    x: 0
    y: 0
    width: parent ? parent.width : 0
    height: parent ? parent.height : 0
    modal: true
    focus: true
    padding: 0
    closePolicy: QQC2.Popup.CloseOnEscape

    Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
    Kirigami.Theme.inherit: false

    // The Complementary set's background (dark in most schemes), so its text, icons and the
    // placeholder messages below are on the color they were chosen for.
    background: Rectangle {
        color: Qt.alpha(Kirigami.Theme.backgroundColor, 0.92)
    }

    contentItem: Item {
        id: stage

        focus: true
        readonly property real dpr: Screen.devicePixelRatio
        // Qt.styleHints is typed QObject for qmllint (qml.md).
        readonly property var hints: Qt.styleHints
        Keys.onPressed: event => {
            // Ctrl+S: a key of the viewer, not a window shortcut (it cannot fire behind the viewer).
            if (event.matches(StandardKey.Save)) {
                root.save();
                event.accepted = true;
                return;
            }
            const mods = event.modifiers & ~Qt.KeypadModifier;
            const ctrl = (mods & Qt.ControlModifier) !== 0 && (mods & ~(Qt.ControlModifier | Qt.ShiftModifier)) === 0;
            if (root.shown && (event.matches(StandardKey.ZoomIn) || (ctrl && (event.key === Qt.Key_Equal || event.key === Qt.Key_Plus)))) {
                root.stepLevel(1);
                event.accepted = true;
                return;
            }
            if (root.shown && (event.matches(StandardKey.ZoomOut) || (ctrl && event.key === Qt.Key_Minus))) {
                root.stepLevel(-1);
                event.accepted = true;
                return;
            }
            if (root.shown && mods === Qt.ControlModifier && event.key === Qt.Key_0) {
                root.resetZoom();
                event.accepted = true;
                return;
            }
            if (root.shown && mods === Qt.ControlModifier && event.key === Qt.Key_1) {
                root.zoomLevel(100);
                event.accepted = true;
                return;
            }
            // Shift+arrows pan a zoomed picture by a tenth of the viewport; the plain arrows
            // (and the arrows of a picture that cannot pan) step the album.
            const panning = mods === Qt.ShiftModifier && root.canPan;
            switch (event.key) {
            case Qt.Key_Left:
                if (panning) {
                    root.panBy(root.vw / 10, 0);
                } else {
                    root.step(-1);
                }
                break;
            case Qt.Key_Right:
                if (panning) {
                    root.panBy(-root.vw / 10, 0);
                } else {
                    root.step(1);
                }
                break;
            case Qt.Key_Up:
                if (!panning) {
                    return;
                }
                root.panBy(0, root.vh / 10);
                break;
            case Qt.Key_Down:
                if (!panning) {
                    return;
                }
                root.panBy(0, -root.vh / 10);
                break;
            case Qt.Key_PageUp:
                root.step(-1);
                break;
            case Qt.Key_PageDown:
            case Qt.Key_Space:
                root.step(1);
                break;
            case Qt.Key_Home:
                root.step(-root.count);
                break;
            case Qt.Key_Return:
            case Qt.Key_Enter:
                // The button in the middle (Download, Play), when there is one.
                if (!root.absent) {
                    return;
                }
                root.act();
                break;
            case Qt.Key_End:
                root.step(root.count);
                break;
            default:
                return;
            }
            event.accepted = true;
        }

        // A click beside the picture closes; one on the picture keeps it open. The exclusive
        // grab on press keeps the tap here: a passive handler would let a tap on this modal
        // popup reach the TapHandler of an item under it (Qt 6.11), which opens a second
        // viewer from the picture underneath. A release farther than the drag distance from the
        // press is a drag, never a tap (at Fit the DragHandler is off, and WithinBounds would
        // still tap).
        TapHandler {
            id: tapper
            gesturePolicy: TapHandler.WithinBounds
            // The second tap of a double click is told apart here (a tap within the system's double
            // click interval and distance of the one before), not by the handler's own count: a
            // synthetic double click event of the window tests never counted for it.
            property double lastTapAt: 0
            property point lastTapAtPoint: Qt.point(0, 0)
            onTapped: (eventPoint, button) => {
                const p = eventPoint.position;
                const q = eventPoint.pressPosition;
                if (Math.hypot(p.x - q.x, p.y - q.y) > stage.hints.startDragDistance) {
                    tapper.lastTapAt = 0;
                    return;
                }
                if (!root.hitsPicture(p.x, p.y)) {
                    root.close();
                    return;
                }
                const now = Date.now();
                if (tapper.lastTapAt > 0 && now - tapper.lastTapAt <= stage.hints.mouseDoubleClickInterval && Math.hypot(p.x - tapper.lastTapAtPoint.x, p.y - tapper.lastTapAtPoint.y) <= stage.hints.startDragDistance * 2) {
                    tapper.lastTapAt = 0;
                    root.toggleAt(Math.max(0, Math.min(root.vw, p.x - viewport.x)), Math.max(0, Math.min(root.vh, p.y - viewport.y)));
                } else {
                    tapper.lastTapAt = now;
                    tapper.lastTapAtPoint = p;
                }
            }
        }

        // The wheel (and a touchpad's two-finger scroll, which reaches Qt as a wheel event) zooms
        // around the pointer; album steps stay on the keys and the side buttons.
        WheelHandler {
            target: null
            enabled: root.shown
            onWheel: event => {
                const d = event.angleDelta.y !== 0 ? event.angleDelta.y : event.pixelDelta.y;
                if (d === 0) {
                    return;
                }
                const at = viewport.mapFromItem(stage, event.x, event.y);
                root.zoomBy(Math.pow(1.25, d / 120), Math.max(0, Math.min(root.vw, at.x)), Math.max(0, Math.min(root.vh, at.y)));
                event.accepted = true;
            }
        }

        // Dragging pans a picture larger than the viewport; the edges stop at the viewport's.
        DragHandler {
            id: panner
            target: null
            enabled: root.canPan
            maximumPointCount: 1
            cursorShape: root.canPan ? (panner.active ? Qt.ClosedHandCursor : Qt.OpenHandCursor) : Qt.ArrowCursor
            property real startX: 0
            property real startY: 0
            onActiveChanged: {
                if (panner.active) {
                    panner.startX = root.rawX;
                    panner.startY = root.rawY;
                }
            }
            onTranslationChanged: {
                if (panner.active) {
                    root.placeRect(panner.startX + panner.translation.x, panner.startY + panner.translation.y, root.shownW, root.shownH);
                }
            }
        }

        // A pinch (touchpad or touchscreen) zooms around its centroid.
        PinchHandler {
            id: pincher
            target: null
            enabled: root.shown
            acceptedDevices: PointerDevice.TouchScreen | PointerDevice.TouchPad
            property real startW: 0
            onActiveChanged: {
                if (pincher.active) {
                    pincher.startW = root.shownW;
                }
            }
            onActiveScaleChanged: {
                if (pincher.active && pincher.startW > 0) {
                    const at = viewport.mapFromItem(stage, pincher.centroid.position.x, pincher.centroid.position.y);
                    root.zoomAbout(pincher.startW * pincher.activeScale, Math.max(0, Math.min(root.vw, at.x)), Math.max(0, Math.min(root.vh, at.y)));
                }
            }
        }

        // The picture area: today's box, clipped. The pictures live in `layer`, which is moved and
        // scaled to the shown rectangle; at Fit it is exactly the box (scale 1, no offset).
        Item {
            id: viewport
            objectName: "viewerViewport"
            anchors.fill: parent
            anchors.margins: Kirigami.Units.gridUnit * 2
            anchors.topMargin: Kirigami.Units.gridUnit * 3
            anchors.leftMargin: root.count > 1 ? Kirigami.Units.gridUnit * 3.5 : Kirigami.Units.gridUnit * 2
            anchors.rightMargin: anchors.leftMargin
            clip: true

            Item {
                id: layer
                width: viewport.width
                height: viewport.height
                scale: root.fitMode || root.fitW <= 0 ? 1 : root.shownW / root.fitW
                x: root.fitMode || root.fitW <= 0 ? 0 : root.rectX + root.shownW / 2 - viewport.width / 2
                y: root.fitMode || root.fitW <= 0 ? 0 : root.rectY + root.shownH / 2 - viewport.height / 2

                Image {
                    id: still
                    objectName: "still"
                    anchors.fill: parent
                    visible: !root.movie && !root.absent && sharp.status !== Image.Ready
                    source: root.movie ? "" : root.url
                    asynchronous: true
                    retainWhileLoading: true
                    smooth: true
                    mipmap: true
                    fillMode: Image.PreserveAspectFit
                    sourceSize.width: Math.ceil(width)
                    sourceSize.height: Math.ceil(height)
                    onStatusChanged: {
                        if (still.status === Image.Ready) {
                            root.heldFor = root.attachment;
                        } else if (still.status !== Image.Loading) {
                            root.heldFor = 0;
                        }
                    }
                }

                // The zoomed decode: the file at its natural size under the provider's cap (the long
                // side only is asked: the provider covers the LARGER of the two scales), not cached.
                Image {
                    id: sharp
                    objectName: "sharp"
                    anchors.fill: parent
                    visible: sharp.status === Image.Ready
                    source: root.sharpAsked && !root.movie && !root.absent ? root.url : ""
                    asynchronous: true
                    cache: false
                    retainWhileLoading: true
                    smooth: true
                    mipmap: true
                    fillMode: Image.PreserveAspectFit
                    readonly property real longSide: Math.min(root.itemWidth > 0 ? Math.max(root.itemWidth, root.itemHeight) : 8192, 8192) / stage.dpr
                    readonly property bool landscape: root.fitW >= root.fitH
                    sourceSize.width: sharp.landscape ? sharp.longSide : 0
                    sourceSize.height: sharp.landscape ? 0 : sharp.longSide
                    onSourceChanged: {
                        if (sharp.source.toString() !== "") {
                            ++root.sharpRequests;
                        }
                    }
                }

                MediaCopy {
                    id: copy
                    attachment: root.attachment
                    active: root.animated && root.opened
                }

                AnimatedImage {
                    id: moving
                    anchors.fill: parent
                    visible: root.movie
                    source: copy.movieUrl
                    asynchronous: true
                    fillMode: Image.PreserveAspectFit
                    playing: root.opened
                }
            }
        }

        Spinner {
            anchors.centerIn: parent
            running: !root.absent && ((still.status === Image.Loading && !root.stillHeld) || (root.movie && copy.url === "" && !copy.failed))
        }

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 8
            visible: !root.absent && (still.status === Image.Error || (root.movie && (copy.failed || moving.status === AnimatedImage.Error)))
            icon.name: "image-missing"
            text: L10n.i18nc("@info:placeholder", "The picture could not be shown")
        }

        // A video, or a picture that is not downloaded: what it is, and what to do.
        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 8
            visible: root.absent
            icon.name: root.video ? "video-x-generic" : "image-x-generic"
            text: {
                if (!root.current) {
                    return "";
                }
                if (root.mediaState === "downloading") {
                    return L10n.i18nc("@info:placeholder, %1 percent", "Downloading… %1%", Math.round(root.progress * 100));
                }
                if (root.mediaState === "expired") {
                    return L10n.i18nc("@info:placeholder", "No longer available");
                }
                if (root.mediaState === "unavailable") {
                    return L10n.i18nc("@info:placeholder", "Not available on this device");
                }
                if (root.mediaState === "failed") {
                    return L10n.i18nc("@info:placeholder", "The download failed");
                }
                if (root.mediaState === "asking") {
                    return L10n.i18nc("@info", "Asking your phone…");
                }
                const note = MediaActions.askNote(root.mediaState);
                if (note !== "") {
                    return note;
                }
                if (root.mediaState !== "local") {
                    return L10n.i18nc("@info:placeholder", "Not downloaded yet");
                }
                return root.video ? L10n.i18nc("@info:placeholder", "Videos play in another application") : L10n.i18nc("@info:placeholder", "The picture could not be shown");
            }
            helpfulAction: Kirigami.Action {
                objectName: "viewerAction"
                enabled: root.current !== null && (root.mediaState === "local" ? root.video : root.downloadable)
                icon.name: root.mediaState === "local" ? "media-playback-start" : (root.mediaState === "failed" ? "view-refresh" : "download")
                text: root.mediaState === "local" ? L10n.i18nc("@action:button", "Play") : (root.mediaState === "failed" ? L10n.i18nc("@action:button", "Try Again") : L10n.i18nc("@action:button", "Download"))
                onTriggered: root.act()
            }
        }

        // Stepping through an album.
        QQC2.RoundButton {
            id: previousButton
            objectName: "viewerPrevious"
            visible: root.count > 1
            enabled: root.index > 0
            anchors.left: parent.left
            anchors.leftMargin: Kirigami.Units.largeSpacing
            anchors.verticalCenter: parent.verticalCenter
            focusPolicy: Qt.NoFocus
            icon.name: "go-previous"
            text: L10n.i18nc("@action:button", "Previous Picture")
            display: QQC2.AbstractButton.IconOnly
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Previous (Left)")
            onClicked: root.step(-1)
        }
        QQC2.RoundButton {
            id: nextButton
            objectName: "viewerNext"
            visible: root.count > 1
            enabled: root.index < root.count - 1
            anchors.right: parent.right
            anchors.rightMargin: Kirigami.Units.largeSpacing
            anchors.verticalCenter: parent.verticalCenter
            focusPolicy: Qt.NoFocus
            icon.name: "go-next"
            text: L10n.i18nc("@action:button", "Next Picture")
            display: QQC2.AbstractButton.IconOnly
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Next (Right)")
            onClicked: root.step(1)
        }

        // "3 of 9".
        QQC2.Label {
            objectName: "viewerCounter"
            visible: root.count > 1
            anchors.top: parent.top
            anchors.topMargin: Kirigami.Units.largeSpacing
            anchors.horizontalCenter: parent.horizontalCenter
            text: L10n.i18nc("@info position in an album, %1 number, %2 count", "%1 of %2", root.index + 1, root.count)
            color: Kirigami.Theme.textColor
            textFormat: Text.PlainText
        }

        Row {
            anchors.top: parent.top
            anchors.right: parent.right
            anchors.margins: Kirigami.Units.smallSpacing
            spacing: Kirigami.Units.smallSpacing

            QQC2.ToolButton {
                objectName: "viewerZoomOut"
                visible: root.shown
                enabled: root.canZoomOut
                focusPolicy: Qt.NoFocus
                icon.name: "zoom-out"
                text: L10n.i18nc("@action:button", "Zoom Out")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Zoom Out (Ctrl+-)")
                onClicked: root.stepLevel(-1)
            }
            QQC2.ToolButton {
                id: levelButton
                objectName: "viewerZoomLevel"
                visible: root.shown
                focusPolicy: Qt.NoFocus
                text: root.fitMode ? L10n.i18nc("@item:inmenu zoom level", "Fit") : (root.naturalWidth > 0 ? L10n.i18nc("@item:inmenu zoom level, %1 is a number", "%1%", root.zoomPercent) : "…")
                Accessible.name: L10n.i18nc("@info:tooltip", "Zoom level")
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Zoom level")
                onClicked: zoomMenu.popup(levelButton, 0, levelButton.height)
            }
            QQC2.ToolButton {
                objectName: "viewerZoomIn"
                visible: root.shown
                enabled: root.canZoomIn
                focusPolicy: Qt.NoFocus
                icon.name: "zoom-in"
                text: L10n.i18nc("@action:button", "Zoom In")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Zoom In (Ctrl+=)")
                onClicked: root.stepLevel(1)
            }

            QQC2.ToolButton {
                visible: root.attachment > 0 && !root.absent
                focusPolicy: Qt.NoFocus
                icon.name: "document-open"
                text: L10n.i18nc("@action:button", "Open in Another Application")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
                onClicked: MediaActions.open(root.attachment, L10n.i18nc("@info", "Picture"), root.media)
            }
            QQC2.ToolButton {
                objectName: "viewerSave"
                visible: root.saveOffered && root.attachment > 0
                focusPolicy: Qt.NoFocus
                icon.name: "document-save-as"
                text: L10n.i18nc("@action:button", "Save As…")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Save As… (Ctrl+S)")
                onClicked: root.save()
            }
            QQC2.ToolButton {
                focusPolicy: Qt.NoFocus
                icon.name: "dialog-close"
                text: L10n.i18nc("@action:button", "Close")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
                onClicked: root.close()
            }
        }

        // The zoom levels. Declared with the viewer (a menu made when it opens can stay as small as
        // its padding); a closed menu leaves the keyboard nowhere, so the stage takes it back.
        QQC2.Menu {
            id: zoomMenu
            objectName: "viewerZoomMenu"
            popupType: QQC2.Popup.Item
            onClosed: stage.forceActiveFocus()

            QQC2.MenuItem {
                objectName: "viewerZoomFit"
                text: L10n.i18nc("@item:inmenu zoom level", "Fit")
                checkable: true
                checked: root.levelIs(0)
                onTriggered: {
                    root.zoomLevel(0);
                    checked = Qt.binding(() => root.levelIs(0));
                }
            }
            QQC2.MenuItem {
                objectName: "viewerZoom50"
                // TRANSLATORS: a zoom level as a percentage, e.g. "100%"; %1 is the number.
                text: L10n.i18nc("@item:inmenu zoom level, %1 is a number", "%1%", 50)
                enabled: root.naturalWidth > 0
                checkable: true
                checked: root.levelIs(50)
                onTriggered: {
                    root.zoomLevel(50);
                    checked = Qt.binding(() => root.levelIs(50));
                }
            }
            QQC2.MenuItem {
                objectName: "viewerZoom100"
                // TRANSLATORS: a zoom level as a percentage, e.g. "100%"; %1 is the number.
                text: L10n.i18nc("@item:inmenu zoom level, %1 is a number", "%1%", 100)
                enabled: root.naturalWidth > 0
                checkable: true
                checked: root.levelIs(100)
                onTriggered: {
                    root.zoomLevel(100);
                    checked = Qt.binding(() => root.levelIs(100));
                }
            }
            QQC2.MenuItem {
                objectName: "viewerZoom200"
                // TRANSLATORS: a zoom level as a percentage, e.g. "100%"; %1 is the number.
                text: L10n.i18nc("@item:inmenu zoom level, %1 is a number", "%1%", 200)
                enabled: root.naturalWidth > 0
                checkable: true
                checked: root.levelIs(200)
                onTriggered: {
                    root.zoomLevel(200);
                    checked = Qt.binding(() => root.levelIs(200));
                }
            }
            QQC2.MenuItem {
                objectName: "viewerZoom400"
                // TRANSLATORS: a zoom level as a percentage, e.g. "100%"; %1 is the number.
                text: L10n.i18nc("@item:inmenu zoom level, %1 is a number", "%1%", 400)
                enabled: root.naturalWidth > 0
                checkable: true
                checked: root.levelIs(400)
                onTriggered: {
                    root.zoomLevel(400);
                    checked = Qt.binding(() => root.levelIs(400));
                }
            }
            QQC2.MenuItem {
                objectName: "viewerZoom800"
                // TRANSLATORS: a zoom level as a percentage, e.g. "100%"; %1 is the number.
                text: L10n.i18nc("@item:inmenu zoom level, %1 is a number", "%1%", 800)
                enabled: root.naturalWidth > 0
                checkable: true
                checked: root.levelIs(800)
                onTriggered: {
                    root.zoomLevel(800);
                    checked = Qt.binding(() => root.levelIs(800));
                }
            }
        }
    }

    onClosed: root.resetZoom()

    onOpened: {
        root.resetZoom();
        stage.forceActiveFocus();
        // (An answer of the viewer's own query that came before the viewer was open.)
        if (root.gallery !== "") {
            root.refresh();
        }
    }
}
