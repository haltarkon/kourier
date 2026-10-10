pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// A photo, GIF, video or sticker (§8.6), laid out in the picture's proportions before anything
// is downloaded: the blurhash placeholder first, then the thumbnail from the image provider
// (decoded at display size), the full picture in the viewer on a tap. Not downloaded yet: a
// download button with the size; downloading: progress; failed: retry; gone: a note. GIFs and
// animated stickers play from a tmpfs copy (AnimatedImage cannot read image providers) over
// their still, which stays when the copy is no GIF or WebP after all.
Item {
    id: root

    required property var block
    // The widest the bubble lets it be (its height follows from this, not from the width the
    // layout gives it, which would be a binding loop).
    property real available: Style.imageMaxWidth
    // A tile of an album: its size is the grid's, a tap asks the album to open the viewer
    // (`viewRequested`), and the size label stays off the small picture.
    property bool tile: false

    // The message the picture belongs to (0 when it has none): the viewer follows its row.
    property real message: 0

    // The picture was tapped as a tile of an album.
    signal viewRequested

    readonly property real attachment: root.block.attachment ?? 0
    readonly property string mediaState: root.block.state
    readonly property bool local: root.mediaState === "local"
    readonly property bool sticker: root.block.sticker
    readonly property bool animated: root.block.animated && root.local
    // Height / width, clamped so very tall or wide pictures stay readable (they are cropped).
    readonly property real ratio: root.block.width > 0 && root.block.height > 0 ? Math.max(0.4, Math.min(1.6, root.block.height / root.block.width)) : 0.75
    // Natural width in logical pixels, never wider than the bubble allows, never tiny.
    readonly property real naturalWidth: root.block.width > 0 ? root.block.width / Screen.devicePixelRatio : Style.imageMaxWidth
    readonly property real maxWidth: root.sticker ? Style.stickerSize : Style.imageMaxWidth
    readonly property real minWidth: root.sticker ? Kirigami.Units.gridUnit * 4 : Kirigami.Units.gridUnit * 7
    readonly property real hue: Style.hueOf(root.block.blurhash)
    readonly property bool playing: root.animated && animation.status === AnimatedImage.Ready
    readonly property bool shown: root.playing || picture.status === Image.Ready
    // What activating the picture does now: "download", "open" (a video: another application
    // plays it), "view", or "" (a sticker, a download in progress, no longer available).
    readonly property string mediaAction: {
        if (root.attachment <= 0 || root.sticker) {
            return "";
        }
        if (!root.local) {
            return root.mediaState === "remote" || root.mediaState === "failed" ? "download" : "";
        }
        return root.block.kind === "video" ? "open" : "view";
    }

    // A tap, or the row's keyboard action.
    function activate(): void {
        if (root.attachment <= 0) {
            return;
        }
        if (!root.local) {
            if (root.mediaState === "remote" || root.mediaState === "failed") {
                App.downloadAttachment(root.attachment);
            }
        } else if (root.block.kind === "video") {
            // Another application plays it; an unusual type asks first (the copy keeps the
            // sender's file-name extension).
            MediaActions.open(root.attachment, L10n.i18nc("@info", "Video"), root.block.media);
        } else if (root.tile) {
            // The album opens the viewer on all of its pictures, at this one.
            root.viewRequested();
        } else {
            // The viewer plays what plays here; anything else it shows as a still.
            const movie = root.animated && !copy.notMovie;
            if (movie || root.block.fullUrl !== "") {
                MediaActions.view(root.attachment, root.block.fullUrl, movie, root.block.media, root.message);
            }
        }
    }

    implicitWidth: Math.round(Math.max(root.minWidth, Math.min(root.maxWidth, root.naturalWidth)))
    implicitHeight: Math.round(Math.min(root.implicitWidth, root.available) * root.ratio)

    Accessible.role: Accessible.Graphic
    Accessible.name: root.block.kind === "video" ? L10n.i18nc("@info accessible", "Video") : L10n.i18nc("@info accessible", "Picture")

    // Before any blurhash: a soft gradient in a color derived from the attachment.
    Rectangle {
        anchors.fill: parent
        visible: !root.shown && blur.status !== Image.Ready && !root.sticker
        radius: root.tile ? 0 : Style.bubbleRadiusTight
        gradient: Gradient {
            orientation: Gradient.Vertical
            GradientStop {
                position: 0
                color: Qt.hsla(root.hue, 0.35, 0.72, 1)
            }
            GradientStop {
                position: 0.62
                color: Qt.hsla((root.hue + 0.08) % 1, 0.32, 0.52, 1)
            }
            GradientStop {
                position: 1
                color: Qt.hsla((root.hue + 0.14) % 1, 0.3, 0.36, 1)
            }
        }
    }

    Image {
        id: blur
        anchors.fill: parent
        visible: !root.shown && status === Image.Ready
        source: root.block.placeholderUrl
        asynchronous: true
        smooth: true
        fillMode: Image.Stretch
        sourceSize.width: 32
        sourceSize.height: Math.max(1, Math.round(32 * root.ratio))
    }

    Image {
        id: picture
        anchors.fill: parent
        // Also the poster of an animation until (or unless) it plays.
        visible: status === Image.Ready && !root.playing
        source: root.block.url
        asynchronous: true
        smooth: true
        mipmap: true
        fillMode: root.sticker ? Image.PreserveAspectFit : Image.PreserveAspectCrop
        sourceSize.width: Math.ceil(width * Screen.devicePixelRatio)
        sourceSize.height: Math.ceil(height * Screen.devicePixelRatio)
    }

    MediaCopy {
        id: copy
        attachment: root.attachment
        active: root.animated
    }

    AnimatedImage {
        id: animation
        anchors.fill: parent
        visible: root.playing
        source: copy.movieUrl
        asynchronous: true
        fillMode: root.sticker ? Image.PreserveAspectFit : Image.PreserveAspectCrop
        playing: visible && root.Window.active
    }

    TapHandler {
        enabled: root.attachment > 0
        onTapped: root.activate()
    }

    // The action in the middle: download, progress, retry, play.
    Rectangle {
        id: badge
        anchors.centerIn: parent
        readonly property bool busy: root.mediaState === "downloading"
        visible: !root.local && root.mediaState !== "expired" || root.local && root.block.kind === "video"
        width: Kirigami.Units.gridUnit * 2.6
        height: width
        radius: width / 2
        // Overlays on a picture take the Complementary color set: its background over the
        // picture and its text on that, whatever the scheme.
        Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
        Kirigami.Theme.inherit: false
        color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
        Kirigami.Icon {
            anchors.centerIn: parent
            visible: !badge.busy
            width: Kirigami.Units.iconSizes.smallMedium
            height: width
            source: root.local ? "media-playback-start" : (root.mediaState === "failed" ? "view-refresh" : "download")
            color: Kirigami.Theme.textColor
            isMask: true
        }
        QQC2.Label {
            anchors.centerIn: parent
            visible: badge.busy
            text: Math.round(root.block.progress * 100) + "%"
            color: Kirigami.Theme.textColor
            font: Kirigami.Theme.smallFont
        }
        // Progress ring.
        Repeater {
            model: badge.busy ? 24 : 0
            delegate: Rectangle {
                required property int index
                width: 3
                height: 3
                radius: 1.5
                x: badge.width / 2 - 1.5 + (badge.width / 2 - 4) * Math.sin(index / 24 * 2 * Math.PI)
                y: badge.height / 2 - 1.5 - (badge.height / 2 - 4) * Math.cos(index / 24 * 2 * Math.PI)
                color: Kirigami.Theme.textColor
                opacity: index / 24 < root.block.progress ? 1 : 0.3
            }
        }
    }

    // Size or duration, and what went wrong.
    Rectangle {
        anchors.left: parent.left
        anchors.bottom: parent.bottom
        anchors.margins: Kirigami.Units.smallSpacing
        visible: label.text !== "" && (!root.local || root.block.kind !== "image") && !root.sticker && !(root.tile && root.local)
        width: label.implicitWidth + Kirigami.Units.largeSpacing
        height: label.implicitHeight + Kirigami.Units.smallSpacing
        radius: height / 2
        Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
        Kirigami.Theme.inherit: false
        color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
        QQC2.Label {
            id: label
            anchors.centerIn: parent
            text: {
                switch (root.mediaState) {
                case "failed":
                    return L10n.i18nc("@info", "Download failed");
                case "expired":
                    return L10n.i18nc("@info", "No longer available");
                default:
                    return root.block.label;
                }
            }
            color: Kirigami.Theme.textColor
            font: Kirigami.Theme.smallFont
        }
    }
}
