pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// One square of "Media" (people pages P4; architecture §8.3): the picture, video or GIF of a chat
// as a cropped square (the small previews stay cropped on purpose; the timeline shows a picture
// whole), through the image provider's thumbnail like any picture of the timeline. A picture that
// is not here shows its blurhash and the glyph that fetches it; a video shows a play glyph and
// its length; a click does what a click on the picture in the chat does: fetch it, open the
// viewer (one viewer at a time; a square of a gallery slot opens it through `MediaActions.viewGallery`:
// the viewer reads the message's whole album with a query of its own), or hand a video to another application.
// The roles are those of `MediaListModel`.
QQC2.AbstractButton {
    id: root

    required property real attachment
    required property real message
    required property string kind
    required property string url
    required property string fullUrl
    required property string placeholderUrl
    required property bool animated
    required property bool media
    required property int pixelWidth
    required property int pixelHeight
    required property string mediaState
    required property real progress
    required property string sizeLabel

    // The gallery slot the square belongs to ("group", "person" or "media"): the viewer it opens
    // starts with that slot's rows of the message and follows the message through its own query.
    // Empty: the picture is the timeline's (`MediaActions.view`).
    property string gallery: ""

    // The keyboard's place is drawn by the view that owns the tile (a grid's current item), or by
    // the tile's own focus when it is a tab stop.
    property bool framed: root.visualFocus

    readonly property bool local: root.mediaState === "local"
    readonly property bool video: root.kind === "video"
    readonly property bool busy: root.mediaState === "downloading"
    readonly property bool downloadable: root.mediaState === "remote" || root.mediaState === "failed"
    // Nothing to show or fetch yet: the phone is being asked.
    readonly property bool waiting: root.mediaState === "asking"

    // What a click does now: "download", "open" (a video, another application), "view", or "".
    readonly property string mediaAction: {
        if (root.attachment <= 0) {
            return "";
        }
        if (!root.local) {
            return root.downloadable ? "download" : "";
        }
        if (root.video) {
            return "open";
        }
        return root.fullUrl !== "" || root.animated ? "view" : "";
    }

    function activate(): void {
        switch (root.mediaAction) {
        case "download":
            App.downloadAttachment(root.attachment);
            break;
        case "open":
            MediaActions.open(root.attachment, L10n.i18nc("@info", "Video"), root.media);
            break;
        case "view":
            if (root.gallery !== "") {
                MediaActions.viewGallery(root.gallery, root.attachment, root.message);
            } else {
                MediaActions.view(root.attachment, root.fullUrl, root.animated, root.media, root.message);
            }
            break;
        }
    }

    // A hue of its own from the attachment while nothing else shows.
    readonly property real hue: ((root.attachment * 0.137) % 1 + 1) % 1

    implicitWidth: Kirigami.Units.gridUnit * 6
    implicitHeight: implicitWidth
    hoverEnabled: true
    Accessible.name: (root.video ? L10n.i18nc("@info accessible", "Video") : L10n.i18nc("@info accessible", "Picture")) + (root.sizeLabel !== "" ? ", " + root.sizeLabel : "")
    Accessible.description: root.downloadable ? L10n.i18nc("@info accessible a picture that is not downloaded yet", "Not downloaded") : ""
    QQC2.ToolTip.visible: root.hovered && root.mediaAction === "download"
    QQC2.ToolTip.text: L10n.i18nc("@action:button", "Download")
    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
    onClicked: root.activate()
    Keys.onReturnPressed: event => {
        root.activate();
        event.accepted = true;
    }
    Keys.onEnterPressed: event => {
        root.activate();
        event.accepted = true;
    }

    background: Rectangle {
        color: Kirigami.Theme.alternateBackgroundColor
        border.width: root.framed ? 2 : 0
        border.color: Kirigami.Theme.highlightColor
    }

    contentItem: Item {
        id: face
        // The picture lies inside the keyboard's frame, so that the frame is never over it.
        readonly property real inset: root.framed ? 2 : 0

        // Before any blurhash: a soft gradient in a color derived from the attachment.
        Rectangle {
            anchors.fill: parent
            anchors.margins: face.inset
            visible: blur.status !== Image.Ready && picture.status !== Image.Ready
            gradient: Gradient {
                orientation: Gradient.Vertical
                GradientStop {
                    position: 0
                    color: Qt.hsla(root.hue, 0.35, 0.72, 1)
                }
                GradientStop {
                    position: 1
                    color: Qt.hsla((root.hue + 0.14) % 1, 0.3, 0.4, 1)
                }
            }
        }
        Image {
            id: blur
            anchors.fill: parent
            anchors.margins: face.inset
            visible: picture.status !== Image.Ready && status === Image.Ready
            source: root.placeholderUrl
            asynchronous: true
            smooth: true
            fillMode: Image.PreserveAspectCrop
            sourceSize.width: 32
            sourceSize.height: 32
        }
        Image {
            id: picture
            objectName: "mediaPicture"
            anchors.fill: parent
            anchors.margins: face.inset
            source: root.url
            asynchronous: true
            retainWhileLoading: true
            smooth: true
            mipmap: true
            fillMode: Image.PreserveAspectCrop
            // (Logical pixels: Qt multiplies `sourceSize` by the device pixel ratio itself.)
            sourceSize.width: Math.ceil(width)
            sourceSize.height: Math.ceil(height)
        }

        // The glyph in the middle: fetch it, wait, play. Over a picture, so the Complementary
        // set's colors (as every overlay on a picture).
        Rectangle {
            id: badge
            anchors.centerIn: parent
            width: Math.min(Kirigami.Units.gridUnit * 2.2, Math.max(0, face.width - Kirigami.Units.smallSpacing * 2))
            height: width
            radius: width / 2
            visible: !root.local || root.video
            Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
            Kirigami.Theme.inherit: false
            color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
            Kirigami.Icon {
                anchors.centerIn: parent
                visible: !root.busy
                width: Math.min(Kirigami.Units.iconSizes.smallMedium, badge.width - 4)
                height: width
                source: root.waiting ? "appointment-soon" : (root.local ? "media-playback-start" : (root.mediaState === "failed" ? "view-refresh" : "download"))
                color: Kirigami.Theme.textColor
                isMask: true
            }
            QQC2.Label {
                anchors.centerIn: parent
                visible: root.busy
                text: Math.round(root.progress * 100) + "%"
                color: Kirigami.Theme.textColor
                font: Kirigami.Theme.smallFont
            }
        }

        // A video's length at the corner.
        Rectangle {
            visible: root.video && root.sizeLabel !== "" && face.width > Kirigami.Units.gridUnit * 3
            anchors.left: parent.left
            anchors.bottom: parent.bottom
            anchors.margins: Kirigami.Units.smallSpacing + face.inset
            width: lengthLabel.implicitWidth + Kirigami.Units.smallSpacing * 2
            height: lengthLabel.implicitHeight + Kirigami.Units.smallSpacing
            radius: height / 2
            Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
            Kirigami.Theme.inherit: false
            color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
            QQC2.Label {
                id: lengthLabel
                anchors.centerIn: parent
                text: root.sizeLabel
                color: Kirigami.Theme.textColor
                font: Kirigami.Theme.smallFont
                textFormat: Text.PlainText
            }
        }
    }
}
