import QtQuick
import QtQml.Models
import org.kde.kirigami as Kirigami
import app.kourier

// The "As picture" look of a forward (forward line F7): the selected messages drawn as Kourier's own bubbles, in the
// current theme, and cut into PNG slices that `App.forwardPicturesDone` hands to the job. The rows come from the
// viewmodel (`App.forwardRows`, the timeline's model type and roles) and are drawn by the timeline's own
// `MessageDelegate`, so a row looks as it does in the chat. The item lives in the main window OUTSIDE its visible area
// (never a window of its own); it is `visible` only while it renders. Pictures come only through Kourier's image
// provider, as in the timeline. Every slice is at most `maxSlice` device pixels tall and is cut BETWEEN rows; a single
// row taller than that is cut inside itself. The files are `01.png`, `02.png` … in the folder Rust made
// (`App.forwardPicturesDir`); Rust deletes it when the job ends.
Item {
    id: root

    objectName: "forwardPicture"

    // The largest slice, in device pixels.
    property int maxSlice: 4000
    // The width of the drawn timeline: the bubble's maximum width plus the margins the delegate keeps.
    readonly property real pictureWidth: Math.round(Style.bubbleMaxWidth / 0.85) + Kirigami.Units.gridUnit * 4
    // The job being drawn (0: none).
    property real request: 0
    // The rows are a group's: a column for the senders' pictures, whichever chat is open.
    property bool group: false
    // How long a render waits for pictures to load before it goes on with what is there (ms).
    property int settleLimit: 15000
    // The slices of the last answer, as written (tests).
    property var lastPaths: []
    // The heights (logical pixels) of the slices of the last render, and the row boundaries it cut at (tests).
    property var lastSlices: []
    property var lastBounds: []
    // Device pixels per logical pixel of the screen the window is on.
    readonly property real ratio: Screen.devicePixelRatio
    readonly property bool rendering: root.request !== 0

    // Outside the window's visible area, never on screen.
    x: -(root.pictureWidth + Kirigami.Units.gridUnit * 20)
    y: 0
    width: root.pictureWidth
    height: 1
    visible: root.rendering
    enabled: false

    property real _waited: 0
    property real _lastHeight: -1
    property int _stable: 0

    function start(request: real, group: bool): void {
        if (root.rendering) {
            root.finish(false, []);
        }
        root.group = group;
        root.lastPaths = [];
        root.lastSlices = [];
        root.lastBounds = [];
        root.lastSizes = [];
        root._waited = 0;
        root._lastHeight = -1;
        root._stable = 0;
        root.request = request;
        settle.restart();
    }

    // The job ended or was cancelled: nothing is written, nothing is answered.
    function stop(request: real): void {
        if (root.rendering && root.request === request) {
            settle.stop();
            root.request = 0;
        }
    }

    function finish(ok: bool, paths: var): void {
        settle.stop();
        const request = root.request;
        root.request = 0;
        root.lastPaths = ok ? paths : [];
        if (request !== 0) {
            App.forwardPicturesDone(request, root.lastPaths);
        }
    }

    // Whether an image below `item` still loads.
    function loading(item: Item): bool {
        for (const child of item.children) {
            if (child.status !== undefined && child.progress !== undefined && child.status === Image.Loading) {
                return true;
            }
            if (root.loading(child)) {
                return true;
            }
        }
        return false;
    }

    // Every row has its delegate and nothing moves any more.
    function settled(): bool {
        if (list.count !== App.forwardRows.rowCount()) {
            return false;
        }
        for (let i = 0; i < list.count; ++i) {
            if (list.itemAtIndex(i) === null) {
                return false;
            }
        }
        if (root.loading(list.contentItem)) {
            return false;
        }
        if (list.contentHeight !== root._lastHeight) {
            root._lastHeight = list.contentHeight;
            root._stable = 0;
            return false;
        }
        root._stable += 1;
        return root._stable >= 3;
    }

    // The cut points (logical pixels from the top) of a column whose rows begin at `bounds` (sorted, with the end last):
    // `[[from, to], …]`, each at most `limit` tall, at a row boundary where there is one in reach.
    function cuts(bounds: var, limit: real): var {
        const end = bounds[bounds.length - 1];
        const out = [];
        let top = 0;
        while (top < end - 0.01) {
            const reach = top + limit;
            if (reach >= end) {
                out.push([top, end]);
                break;
            }
            let cut = -1;
            for (const b of bounds) {
                if (b > top + 0.01 && b <= reach) {
                    cut = b;
                }
            }
            if (cut < 0) {
                cut = reach;
            }
            out.push([top, cut]);
            top = cut;
        }
        return out;
    }

    function render(): void {
        const dir = App.forwardPicturesDir(root.request);
        if (dir === "" || list.count === 0) {
            root.finish(false, []);
            return;
        }
        const bounds = [];
        for (let i = list.count - 1; i >= 0; --i) {
            const item = list.itemAtIndex(i);
            const top = Math.round(item.mapToItem(list, 0, 0).y);
            bounds.push(top);
        }
        bounds.push(Math.round(list.contentHeight));
        const limit = Math.max(1, Math.floor(root.maxSlice / root.ratio));
        const slices = root.cuts(bounds, limit);
        root.lastBounds = bounds;
        root.lastSlices = slices.map(s => s[1] - s[0]);
        root.grab(dir, slices, 0, []);
    }

    function grab(dir: string, slices: var, at: int, done: var): void {
        if (!root.rendering) {
            return;
        }
        if (at >= slices.length) {
            root.finish(true, done);
            return;
        }
        const request = root.request;
        const [from, to] = slices[at];
        view.height = to - from;
        list.y = -from;
        const name = (at + 1 < 10 ? "0" : "") + (at + 1) + ".png";
        const path = dir + "/" + name;
        const ok = view.grabToImage(result => {
            if (!root.rendering || root.request !== request) {
                return;
            }
            if (!result.saveToFile(path)) {
                root.finish(false, []);
                return;
            }
            root.lastSizes[at] = result.image.height;
            done.push(path);
            root.grab(dir, slices, at + 1, done);
        });
        if (!ok) {
            root.finish(false, []);
        }
    }

    // The device-pixel height of every slice written (tests).
    property var lastSizes: []

    Timer {
        id: settle

        interval: 80
        repeat: true
        onTriggered: {
            root._waited += settle.interval;
            if (root.settled() || root._waited >= root.settleLimit) {
                settle.stop();
                root.render();
            }
        }
    }

    // The window the slice is cut from: a clip of the column at the slice's place.
    Item {
        id: view

        objectName: "forwardPictureView"
        width: root.width
        height: 1
        clip: true

        Rectangle {
            anchors.fill: parent
            color: Kirigami.Theme.backgroundColor
        }

        ListView {
            id: list

            objectName: "forwardPictureList"
            width: view.width
            height: list.contentHeight
            interactive: false
            spacing: 0
            // Every row gets its delegate: the whole column is drawn.
            cacheBuffer: 1000000000
            verticalLayoutDirection: ListView.BottomToTop
            model: root.rendering ? App.forwardRows : null

            delegate: DelegateChooser {
                role: "kind"
                DelegateChoice {
                    roleValue: "message"
                    MessageDelegate {
                        // The rows of the picture are the forward's own model; nothing is pressed in them.
                        chatModel: App.forwardRows
                        composerModel: App.composer
                        group: root.group
                    }
                }
                DelegateChoice {
                    roleValue: "date"
                    DateHeader {}
                }
                DelegateChoice {
                    roleValue: "system"
                    SystemRow {}
                }
            }
        }
    }
}
