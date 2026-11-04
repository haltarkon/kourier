pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// A photo, GIF, video or sticker (§8.6), laid out in the picture's proportions before anything
// is downloaded: the blurhash placeholder first, then the thumbnail from the image provider
// (decoded at display size), the full picture in the viewer on a tap. A picture or video is
// never cropped (the user's word): the block's BOX (what the bubble measures) holds the
// PICTURE RECT, the picture's own shape fitted, centred; a very thin picture leaves the rest
// of its box to the bubble's color. Both are plain bindings (`boxW`, `picW` …), no item of
// their own: the row is made many times. Not downloaded yet: a
// download button with the size; downloading: progress; failed: retry; gone: a note (a
// sticker that is gone has no picture to put one over: it is a small framed box with the
// note instead). GIFs and animated stickers play from a tmpfs copy (AnimatedImage cannot read
// image providers) over their still, which stays when the copy is no GIF or WebP after all.
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
    // Gone here, and Download asks the user's phone to upload it again (the core decides it).
    readonly property bool ask: root.block.ask === true
    // What a tap fetches: a download, a retry, or the question to the phone.
    readonly property bool downloadable: root.mediaState === "remote" || root.mediaState === "failed" || root.ask
    readonly property bool sticker: root.block.sticker

    // The picture of an own message on its way (upload progress): a ring over it with the X
    // that cancels the sending in its middle (the badge below), the percentage in the note pill.
    // All of it comes from the block, and whether the X is there is the core's word
    // (`uploadCancel`, the row's `cancelSending`), never decided here. An album that draws the
    // ring of its "+N" tile itself takes this tile's own ring away (`ringByAlbum`).
    property bool ringByAlbum: false
    readonly property bool uploading: root.block.uploading === true && !root.ringByAlbum
    readonly property real uploadShare: root.block.uploadShare ?? 0
    readonly property string uploadLabel: root.block.uploadLabel ?? ""
    readonly property bool uploadCancel: root.block.uploadCancel === true
    // The X was pressed: it stays, dimmed and no longer a button, until the row changes (the
    // core's flag has gone by then, and the message goes in a moment, or turns out to be sent)
    // or the core offers the cancel again (a stop that came too late: the flag is back).
    property bool cancelAsked: false
    readonly property bool cancelShown: root.uploading && (root.uploadCancel || root.cancelAsked)
    onUploadingChanged: {
        if (!root.uploading) {
            root.cancelAsked = false;
        }
    }
    onUploadCancelChanged: {
        if (root.uploadCancel) {
            root.cancelAsked = false;
        }
    }
    readonly property bool animated: root.block.animated && root.local
    // A sticker's height / width, clamped (stickers keep their own rule).
    readonly property real ratio: root.block.width > 0 && root.block.height > 0 ? Math.max(0.4, Math.min(1.6, root.block.height / root.block.width)) : 0.75
    // The picture's own height / width: never clamped, it is shown whole (unknown: a 4:3 box).
    readonly property real shape: root.block.width > 0 && root.block.height > 0 ? root.block.height / root.block.width : 0.75
    // Natural width in logical pixels, never wider than the bubble allows, never tiny.
    readonly property real naturalWidth: root.block.width > 0 ? root.block.width / Screen.devicePixelRatio : Style.imageMaxWidth
    readonly property real maxWidth: root.sticker ? Style.stickerSize : Style.imageMaxWidth
    readonly property real minWidth: root.sticker ? Kirigami.Units.gridUnit * 4 : Style.imageMinWidth

    // A single picture (§8.6): W is the picture column; the picture is as large as fits W wide
    // and `imageMaxHeight` tall, never above its own size, raised to the minimum width (or to
    // the tallest that fits) when tiny. The box is at least the minimum width and height.
    readonly property real column: Math.min(Style.imageMaxWidth, root.available)
    readonly property real tallest: Style.imageMaxHeight / root.shape
    readonly property real singleW: Math.max(Math.min(root.column, root.tallest, root.naturalWidth), Math.min(Style.imageMinWidth, root.column, root.tallest))
    readonly property real singleBoxW: Math.round(Math.max(root.singleW, Math.min(Style.imageMinWidth, root.column)))
    readonly property real singleBoxH: Math.round(Math.max(root.singleW * root.shape, Style.imageMinHeight))
    // The box: a tile and a sticker are given theirs; a single picture's is its own, centred in
    // a block a caption or an author name made wider.
    readonly property bool framed: root.tile || root.sticker
    readonly property real boxW: root.framed ? root.width : root.singleBoxW
    readonly property real boxH: root.framed ? root.height : root.singleBoxH
    readonly property real boxX: root.framed ? 0 : Math.round((root.width - root.boxW) / 2)
    // The picture rect inside the box (a sticker fills it; a tile's picture is fitted inside the
    // tile it is given; the rest is the bubble's color).
    readonly property real fitW: root.tile ? Math.min(root.width, root.height / root.shape) : root.singleW
    readonly property real fitH: root.tile ? Math.min(root.height, root.width * root.shape) : root.singleW * root.shape
    readonly property real picW: root.sticker ? root.width : Math.max(1, Math.round(root.fitW))
    readonly property real picH: root.sticker ? root.height : Math.max(1, Math.round(root.fitH))
    readonly property real picX: root.boxX + (root.sticker ? 0 : Math.round((root.boxW - root.fitW) / 2))
    readonly property real picY: root.sticker ? 0 : Math.round((root.boxH - root.fitH) / 2)
    readonly property real hue: Style.hueOf(root.block.blurhash)
    readonly property bool playing: root.animated && animation.status === AnimatedImage.Ready
    // The attachment whose pixels `picture` holds: set when it is decoded, kept while the next
    // URL loads, cleared when there is none or it failed (`paintedWidth` keeps its old value
    // after an error, though nothing is drawn).
    property real heldFor: 0
    // The picture is on screen: decoded, or still the previous pixels of THIS attachment while
    // a new URL loads (`retainWhileLoading`: the URL changes once more after a download, when
    // the core has derived the thumbnail and the blurhash). Neither the stand-in nor the
    // blurhash shows then. Another attachment's pixels (an album's tiles are reused by index)
    // are never held: a tile shows the stand-in until its own picture is there.
    readonly property bool pictureHeld: picture.status === Image.Ready || (picture.status === Image.Loading && root.heldFor === root.attachment && picture.paintedWidth > 0)
    readonly property bool shown: root.playing || root.pictureHeld
    // What activating the picture does now: "download", "open" (a video: another application
    // plays it), "view", or "" (a sticker that is here, a download in progress, no longer
    // available). A sticker that is not here is a download like a picture: an imported one
    // is never fetched by itself, and the tap and the badge already offer it.
    readonly property string mediaAction: {
        if (root.attachment <= 0) {
            return "";
        }
        if (!root.local) {
            return root.downloadable ? "download" : "";
        }
        if (root.sticker) {
            return "";
        }
        return root.block.kind === "video" ? "open" : "view";
    }

    // Whether `p`, a point of this block, is on the round badge (the ring and its X).
    function overBadge(p: point): bool {
        const r = badge.width / 2;
        return Math.hypot(p.x - (badge.x + r), p.y - (badge.y + r)) <= r;
    }

    // The X: "Cancel Sending" for the whole message, as its menu entry (asked once).
    function cancelSending(): void {
        if (root.uploading && root.uploadCancel && !root.cancelAsked) {
            root.cancelAsked = true;
            App.cancelMessage(root.message);
        }
    }

    // A tap, or the row's keyboard action.
    function activate(): void {
        if (root.attachment <= 0) {
            return;
        }
        if (!root.local) {
            if (root.downloadable) {
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

    // A sticker that is gone: a box as high as its note, not a sticker-sized hole (the height
    // follows from `implicitWidth`, never from the width the layout gives the block).
    readonly property bool standIn: root.sticker && root.gone
    // Size or duration, and what went wrong.
    // (Also while the phone is asked, and when it could not help.)
    readonly property bool gone: ["expired", "unavailable", "asking", "noanswer", "nomessage", "nofile"].includes(root.mediaState)
    readonly property string note: {
        switch (root.mediaState) {
        case "failed":
            return L10n.i18nc("@info", "Download failed");
        case "expired":
            return L10n.i18nc("@info", "No longer available");
        case "unavailable":
            return L10n.i18nc("@info", "Not available on this device");
        case "asking":
            return L10n.i18nc("@info", "Asking your phone…");
        case "noanswer":
        case "nomessage":
        case "nofile":
            return MediaActions.askNote(root.mediaState);
        default:
            return root.block.label;
        }
    }

    // The note in the pill over a picture: the endings of an ask in a few words (the whole
    // sentence is the pill's tooltip, `note`).
    readonly property string shortNote: {
        if (root.uploading) {
            // The percentage of the upload (nothing for a file that waits or is prepared).
            return root.uploadLabel;
        }
        const short = MediaActions.askShortNote(root.mediaState);
        return short !== "" ? short : root.note;
    }

    implicitWidth: root.sticker ? Math.round(Math.max(root.minWidth, Math.min(root.maxWidth, root.naturalWidth))) : root.singleBoxW
    implicitHeight: root.standIn && standInLoader.item ? Math.ceil((standInLoader.item as Item).implicitHeight) : (root.sticker ? Math.round(Math.min(root.implicitWidth, root.available) * root.ratio) : root.singleBoxH)

    Accessible.role: Accessible.Graphic
    Accessible.name: root.block.kind === "video" ? L10n.i18nc("@info accessible", "Video") : L10n.i18nc("@info accessible", "Picture")
    // Why it is not shown, whole, though a tile may cut the note on the screen.
    Accessible.description: root.gone ? root.note : ""

    // A sticker that is gone: a framed box in the theme's colors (a step from the page towards
    // the text, a frame at the theme's frame contrast) with a glyph and the note, quiet. A
    // sticker has no bubble, so it stands on the page. Made only for such a sticker: every
    // picture of a chat has this block.
    Loader {
        id: standInLoader
        active: root.standIn
        sourceComponent: Rectangle {
            id: box
            objectName: "standInBox"
            width: root.implicitWidth
            implicitHeight: standInColumn.implicitHeight + Kirigami.Units.largeSpacing * 2
            height: implicitHeight
            radius: Style.bubbleRadiusTight
            color: Style.peerBubble(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor)
            border.width: 1
            border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
            readonly property color ink: Style.secondary(Kirigami.Theme.textColor, box.color)

            Column {
                id: standInColumn
                anchors.centerIn: parent
                width: box.width - Kirigami.Units.largeSpacing * 2
                spacing: Kirigami.Units.smallSpacing
                Kirigami.Icon {
                    anchors.horizontalCenter: parent.horizontalCenter
                    width: Kirigami.Units.iconSizes.smallMedium
                    height: width
                    // The waiting glyph while the phone is asked, the final one otherwise.
                    source: root.mediaState === "asking" ? "appointment-soon" : "dialog-cancel"
                    color: box.ink
                    isMask: true
                }
                QQC2.Label {
                    width: parent.width
                    text: root.note
                    color: box.ink
                    font: Kirigami.Theme.smallFont
                    horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
                // The Download of a sticker the phone can be asked for: inside the box, below
                // the note (the round badge over a picture would cover both).
                QQC2.Button {
                    objectName: "standInDownload"
                    anchors.horizontalCenter: parent.horizontalCenter
                    visible: root.ask
                    flat: true
                    icon.name: "download"
                    text: L10n.i18nc("@action:button", "Download")
                    onClicked: root.activate()
                }
            }
        }
    }

    // Before any blurhash: a soft gradient in a color derived from the attachment.
    Rectangle {
        x: root.picX
        y: root.picY
        width: root.picW
        height: root.picH
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
        x: root.picX
        y: root.picY
        width: root.picW
        height: root.picH
        visible: !root.shown && status === Image.Ready
        source: root.block.placeholderUrl
        asynchronous: true
        smooth: true
        fillMode: Image.Stretch
        // The picture's own shape, its long side 32: stretched into the picture rect it is not
        // distorted.
        sourceSize.width: root.shape <= 1 ? 32 : Math.max(1, Math.round(32 / root.shape))
        sourceSize.height: root.shape <= 1 ? Math.max(1, Math.round(32 * root.shape)) : 32
    }

    Image {
        id: picture
        objectName: "picture"
        x: root.picX
        y: root.picY
        width: root.picW
        height: root.picH
        // Also the poster of an animation until (or unless) it plays.
        visible: root.pictureHeld && !root.playing
        source: root.block.url
        asynchronous: true
        retainWhileLoading: true
        smooth: true
        mipmap: true
        fillMode: Image.PreserveAspectFit
        // (Logical pixels: Qt multiplies `sourceSize` by the device pixel ratio itself, so the
        // provider is asked for the physical size.)
        sourceSize.width: Math.ceil(width)
        sourceSize.height: Math.ceil(height)
        onStatusChanged: {
            if (picture.status === Image.Ready) {
                root.heldFor = root.attachment;
            } else if (picture.status !== Image.Loading) {
                root.heldFor = 0;
            }
        }
    }

    MediaCopy {
        id: copy
        attachment: root.attachment
        active: root.animated
    }

    AnimatedImage {
        id: animation
        x: root.picX
        y: root.picY
        width: root.picW
        height: root.picH
        visible: root.playing
        source: copy.movieUrl
        asynchronous: true
        fillMode: Image.PreserveAspectFit
        playing: visible && root.Window.active
    }

    // (Only inside the box: beside a picture in a wider block is the bubble.) The one handler of
    // the block also serves the X: a tap on the ring while the picture goes up cancels the
    // sending (or does nothing, while there is no X or it was pressed) and never opens the
    // viewer. A tile whose ring the album draws (`ringByAlbum`: the "+N" tile) leaves the whole
    // tile to the album's overlay over it, which takes the X by position and opens the viewer
    // anywhere else (the ring is not in the middle when the number stands beside it).
    TapHandler {
        enabled: root.attachment > 0
        onTapped: eventPoint => {
            const p = eventPoint.position;
            if (root.ringByAlbum) {
                return;
            }
            if (root.uploading && root.overBadge(p)) {
                root.cancelSending();
            } else if (p.x >= root.boxX && p.x < root.boxX + root.boxW && p.y >= 0 && p.y < root.boxH) {
                root.activate();
            }
        }
    }

    // The action in the middle: download, progress, retry, play; while the picture goes up the
    // ring of its upload with the X (the percentage is in the note pill).
    Rectangle {
        id: badge
        objectName: "badge"
        x: root.boxX + Math.round((root.boxW - width) / 2)
        y: Math.round((root.boxH - height) / 2)
        readonly property bool downloading: root.mediaState === "downloading"
        readonly property bool busy: badge.downloading || root.uploading
        readonly property real share: root.uploading ? root.uploadShare : root.block.progress
        visible: !root.standIn && (root.uploading || !root.local && (!root.gone || root.ask) || root.local && root.block.kind === "video")
        // Whole in its tile, whatever the tile: an album's tile can be small (a very thin
        // picture beside wide ones), so the badge keeps a small spacing off the tile's edge,
        // and below the ring's room the percent figure goes.
        readonly property real side: Math.min(root.width, root.height)
        width: root.tile ? Math.min(Kirigami.Units.gridUnit * 2.6, Math.max(Math.min(badge.side, Kirigami.Units.gridUnit * 1.2), badge.side - Kirigami.Units.smallSpacing * 2)) : Kirigami.Units.gridUnit * 2.6
        height: width
        radius: width / 2
        // Overlays on a picture take the Complementary color set: its background over the
        // picture and its text on that, whatever the scheme.
        Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
        Kirigami.Theme.inherit: false
        color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
        // The ring is the X for a screen reader (the row's menu has "Cancel Sending" too); once
        // pressed it is no longer a button, it says how the upload goes.
        Accessible.ignored: !root.cancelShown
        Accessible.role: root.cancelAsked ? Accessible.StaticText : Accessible.Button
        Accessible.name: root.cancelAsked ? (root.uploadLabel !== "" ? L10n.i18nc("@info accessible, %1 the percentage uploaded", "Uploading %1", root.uploadLabel) : L10n.i18nc("@info accessible", "Uploading")) : L10n.i18nc("@action:button", "Cancel Sending")
        Accessible.onPressAction: root.cancelSending()
        Kirigami.Icon {
            anchors.centerIn: parent
            // While it goes up: the X when there is one (dimmed once it was pressed), else
            // only the ring (the file is being prepared, or waits).
            visible: !badge.downloading && (!root.uploading || root.cancelShown)
            width: Math.min(root.uploading ? Kirigami.Units.iconSizes.small : Kirigami.Units.iconSizes.smallMedium, badge.width - 4)
            height: width
            source: root.uploading ? "window-close" : (root.local ? "media-playback-start" : (root.mediaState === "failed" ? "view-refresh" : "download"))
            color: Kirigami.Theme.textColor
            isMask: true
            opacity: root.uploading && (!root.uploadCancel || root.cancelAsked) ? 0.4 : 1
        }
        QQC2.Label {
            anchors.centerIn: parent
            visible: badge.downloading && badge.width >= Kirigami.Units.gridUnit * 2
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
                opacity: index / 24 < badge.share ? 1 : 0.3
            }
        }
    }

    // The note over the picture. A sticker has none (a gone one is the box above). In a small
    // tile the note is cut to the tile's width, its whole text a tooltip away.
    Rectangle {
        id: pill
        objectName: "note"
        // At the start corner of the box: the left, or the right in a right-to-left layout.
        x: root.LayoutMirroring.enabled ? root.boxX + root.boxW - pill.width - Kirigami.Units.smallSpacing : root.boxX + Kirigami.Units.smallSpacing
        y: root.boxH - height - Kirigami.Units.smallSpacing
        // How far the round badge reaches below the note's top edge. In a picture's box a few
        // pixels (the badge's lowest point against the note's corner: a picture 90 px tall)
        // touch nothing that is read, and only more would put the note under the badge; in a
        // small tile any overlap does.
        readonly property real overlap: badge.visible ? badge.y + badge.height - pill.y : 0
        readonly property bool underBadge: pill.overlap > (root.tile ? 0 : Kirigami.Units.smallSpacing * 2)
        // (Not over the badge: a small tile has no room for both, and the badge is the action.
        // An own picture on its way shows its percentage here, a small tile too unless the
        // badge is in the way.)
        visible: label.text !== "" && (!root.local || root.block.kind !== "image" || root.uploading) && !root.sticker && !(root.tile && root.local && !root.uploading) && !(root.tile && pill.underBadge)
        // The room for the note: the box's width, and where a flat picture's box (the minimum
        // height holds the round badge, not the badge and the note under it) would put it under
        // the badge, only what is left of the badge (the badge is centred in the box, so that is
        // the same room on the left and on the right, whichever corner the note stands in).
        readonly property real room: (!root.tile && pill.underBadge ? badge.x - root.boxX : root.boxW) - Kirigami.Units.smallSpacing * 2
        width: Math.max(0, Math.min(label.implicitWidth, pill.room - Kirigami.Units.largeSpacing)) + Kirigami.Units.largeSpacing
        height: label.implicitHeight + Kirigami.Units.smallSpacing
        radius: height / 2
        Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
        Kirigami.Theme.inherit: false
        color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
        QQC2.Label {
            id: label
            objectName: "noteLabel"
            anchors.centerIn: parent
            width: pill.width - Kirigami.Units.largeSpacing
            text: root.shortNote
            color: Kirigami.Theme.textColor
            font: Kirigami.Theme.smallFont
            elide: Text.ElideRight
            textFormat: Text.PlainText
        }
        HoverHandler {
            id: pillHover
        }
        QQC2.ToolTip.visible: pillHover.hovered && !root.uploading && (label.truncated || label.text !== root.note)
        QQC2.ToolTip.text: root.note
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
    }
}
