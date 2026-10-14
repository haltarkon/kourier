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

    // The message's row changed: show it as it is now, staying on the same picture.
    function refresh(): void {
        const fresh = App.viewerItems(root.message);
        // Not among the loaded rows (the window of the timeline moved on): what is shown stays.
        if (fresh === undefined || fresh === null) {
            return;
        }
        // Deleted, or no picture is left of it.
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
            root.refresh();
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
        Keys.onLeftPressed: root.step(-1)
        Keys.onRightPressed: root.step(1)
        Keys.onPressed: event => {
            switch (event.key) {
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

        // A click beside the picture closes; one on the picture keeps it open.
        TapHandler {
            onTapped: (eventPoint, button) => {
                const shown = root.movie ? moving : still;
                const w = shown.paintedWidth;
                const h = shown.paintedHeight;
                const left = shown.x + (shown.width - w) / 2;
                const top = shown.y + (shown.height - h) / 2;
                const p = eventPoint.position;
                if (w > 0 && h > 0 && p.x >= left && p.x <= left + w && p.y >= top && p.y <= top + h) {
                    return;
                }
                root.close();
            }
        }

        Image {
            id: still
            objectName: "still"
            anchors.fill: parent
            anchors.margins: Kirigami.Units.gridUnit * 2
            anchors.topMargin: Kirigami.Units.gridUnit * 3
            anchors.leftMargin: root.count > 1 ? Kirigami.Units.gridUnit * 3.5 : Kirigami.Units.gridUnit * 2
            anchors.rightMargin: anchors.leftMargin
            visible: !root.movie && !root.absent
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

        MediaCopy {
            id: copy
            attachment: root.attachment
            active: root.animated && root.opened
        }

        AnimatedImage {
            id: moving
            anchors.fill: still
            visible: root.movie
            source: copy.movieUrl
            asynchronous: true
            fillMode: Image.PreserveAspectFit
            playing: root.opened
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
                focusPolicy: Qt.NoFocus
                icon.name: "dialog-close"
                text: L10n.i18nc("@action:button", "Close")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
                onClicked: root.close()
            }
        }
    }

    onOpened: stage.forceActiveFocus()
}
