pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// Several pictures and videos of one message as justified rows (§8.3): every picture whole,
// never cropped (the user's word). The rows come from the viewmodel (`media_layout`: which
// pictures share a row, in message order, so that the pictures of a row share one height and
// their widths follow their shapes); this file only turns them into whole pixels for the width
// the bubble allows (`layout`), with a gap BETWEEN tiles so a tile keeps the shape of its
// picture. A message with more than five shows five tiles, the last one saying how many more
// there are; the viewer steps through all of them. That is a choice of this layout, not a
// limit: nothing a message carries is dropped (Signal Desktop's own grid stops at five tiles
// the same way, `SD/ts/components/conversation/ImageGrid.dom.tsx`, but its composer also
// refuses to send more than 32, which Kourier does not copy).
Item {
    id: root

    required property var block
    // The widest the bubble lets it be.
    property real available: Style.imageMaxWidth
    // The message the album belongs to (0 when it has none): the viewer follows its row.
    property real message: 0

    readonly property var items: root.block.items
    readonly property int count: root.items.length
    // How many tiles are drawn, and how many pictures are behind the last one.
    readonly property int tiles: Math.min(root.count, 5)
    readonly property int hidden: root.count - root.tiles
    readonly property real gap: 2
    // The grid's width. Like a single picture's, its size follows from what the bubble may
    // take (`available`), never from the width the layout gives the block: a height that
    // depends on that width does not settle while the bubble is still measuring its content.
    readonly property real natural: Math.max(0, Math.round(Math.min(Style.imageMaxWidth, root.available)))
    // A bubble made wider by something else (a long caption, a file): the grid stands in its
    // middle.
    readonly property real inset: Math.max(0, Math.round((root.width - root.natural) / 2))

    // The viewmodel's rows of the shown tiles (`count`, `aspect`: the sum of the tiles' shapes,
    // `prefix`: the share of the row's width at which each tile begins).
    readonly property var rows: root.block.rows ?? []
    // The pixel step: the cell [x, y, w, h] of every shown tile and the grid's height, whole
    // pixels, from the width and the rows only (so it is run when one of them changes, never
    // per frame). A row of k tiles spends (k - 1) gaps between its tiles; its height is the
    // inner width over the sum of the shapes, so each cell is the shape of its picture within a
    // pixel; the last tile of a row ends at the grid's edge.
    readonly property var layout: {
        const width = root.natural;
        const cells = [];
        let y = 0;
        for (const row of root.rows) {
            const inner = width - (row.count - 1) * root.gap;
            const height = Math.max(1, Math.round(inner / row.aspect));
            for (let j = 0; j < row.count; ++j) {
                const left = Math.round(row.prefix[j] * inner) + j * root.gap;
                const right = j === row.count - 1 ? width : Math.round(row.prefix[j + 1] * inner) + j * root.gap;
                cells.push({
                    "x": left,
                    "y": y,
                    "w": right - left,
                    "h": height
                });
            }
            y += height + root.gap;
        }
        return {
            "cells": cells,
            "height": Math.max(0, y - root.gap)
        };
    }

    // What the viewer steps through: every picture of the message, not only the tiles.
    function viewerItems(): var {
        const out = [];
        for (const item of root.items) {
            out.push({
                "attachment": item.attachment ?? 0,
                "url": item.fullUrl,
                "animated": item.animated && item.state === "local",
                "media": item.media,
                "kind": item.kind,
                "state": item.state,
                "progress": item.progress,
                "placeholder": item.placeholderUrl,
                "ask": item.ask === true
            });
        }
        return out;
    }

    function openAt(index: int): void {
        MediaActions.viewAlbum(root.viewerItems(), index, root.message);
    }

    // What activating the album does now: "download" while one of its pictures is not here
    // and can be fetched, "view" once all are, "" while the rest is on its way.
    readonly property string mediaAction: {
        let waiting = false;
        for (const item of root.items) {
            if (item.state === "remote" || item.state === "failed" || item.ask === true) {
                return "download";
            }
            waiting = waiting || item.state === "downloading" || item.state === "asking";
        }
        return waiting ? "" : "view";
    }

    // The row's keyboard action: fetch what is missing, else open the viewer at the first
    // picture.
    function activate(): void {
        if (root.mediaAction === "download") {
            for (const item of root.items) {
                if ((item.state === "remote" || item.state === "failed" || item.ask === true) && (item.attachment ?? 0) > 0) {
                    App.downloadAttachment(item.attachment);
                }
            }
        } else if (root.mediaAction === "view") {
            root.openAt(0);
        }
    }

    // The pictures of an own message that go up (upload progress): each tile is an `ImageBlock`
    // with its own ring, and the one behind the others, "+N", has the ring of the pictures it
    // covers (the fifth tile's own and the ones behind it, their bytes together). Every X cancels
    // the sending of the whole message, once.
    readonly property bool moreRing: root.hidden > 0 && root.block.moreUploading === true
    readonly property bool moreCancel: root.block.moreCancel === true
    // The X was pressed: it stays, dimmed and no longer a button, until the core answers (the
    // row changes) or offers the cancel again (a stop that came too late).
    property bool moreAsked: false
    readonly property bool moreXShown: root.moreRing && (root.moreCancel || root.moreAsked)
    onMoreRingChanged: {
        if (!root.moreRing) {
            root.moreAsked = false;
        }
    }
    onMoreCancelChanged: {
        if (root.moreCancel) {
            root.moreAsked = false;
        }
    }

    function cancelMore(): void {
        if (root.moreRing && root.moreCancel && !root.moreAsked) {
            root.moreAsked = true;
            App.cancelMessage(root.message);
        }
    }

    // The "+N" tile: its cell, and its ring (a disc, whole in the tile and at most half of its
    // short side, so the number has room: a tile can be small, a thin picture beside wide ones).
    // With less room above the ring than a line of the theme's small font needs, the number goes
    // BESIDE the ring (the ring at the start of the tile). All of it is plain bindings of the
    // album: the ring is made by ONE Loader below, only while the pictures go up, and a row at
    // rest has no item for it.
    readonly property var moreCell: root.layout.cells[root.tiles - 1] ?? {
        "x": 0,
        "y": 0,
        "w": 0,
        "h": 0
    }
    readonly property real ringSide: Math.min(Kirigami.Units.gridUnit * 2.6, Math.round(Math.min(root.moreCell.w, root.moreCell.h) / 2))
    readonly property bool numberBeside: (root.moreCell.h - root.ringSide) / 2 - Kirigami.Units.smallSpacing < Kirigami.Units.gridUnit
    readonly property real ringLeft: root.numberBeside ? Kirigami.Units.smallSpacing : Math.round((root.moreCell.w - root.ringSide) / 2)
    readonly property real ringTop: Math.round((root.moreCell.h - root.ringSide) / 2)

    // Whether `p`, a point of the "+N" tile, is on its ring.
    function overMore(p: point): bool {
        return root.moreRing && Math.hypot(p.x - (root.ringLeft + root.ringSide / 2), p.y - (root.ringTop + root.ringSide / 2)) <= root.ringSide / 2;
    }

    implicitWidth: root.natural
    implicitHeight: root.layout.height

    Accessible.role: Accessible.Graphic
    Accessible.name: L10n.i18ncp("@info accessible, %1 how many", "Album of %1 picture", "Album of %1 pictures", root.count)

    Repeater {
        model: root.tiles
        delegate: Item {
            id: tile

            required property int index

            // (A block without rows has no cells: nothing is drawn, nothing fails.)
            readonly property var cell: root.layout.cells[tile.index] ?? {
                "x": 0,
                "y": 0,
                "w": 0,
                "h": 0
            }
            readonly property bool more: root.hidden > 0 && tile.index === root.tiles - 1
            // The "+N" tile while the pictures it covers go up: the ring of their upload is drawn
            // over the tile (the album's Loader), the tile's own picture has none.
            readonly property bool ringed: tile.more && root.moreRing

            x: root.inset + tile.cell.x
            y: tile.cell.y
            width: tile.cell.w
            height: tile.cell.h
            clip: true

            ImageBlock {
                // (Bound, not anchored: anchors are applied after the picture asked the
                // provider once, for the size the block has before them.)
                width: tile.width
                height: tile.height
                block: root.items[tile.index]
                message: root.message
                // A tile is the picture's own shape (within a pixel, or a thin picture whole on
                // the bubble's color): no label over small pictures, the badge sized to the tile.
                tile: true
                ringByAlbum: tile.ringed
                onViewRequested: root.openAt(tile.index)
            }

            // "+4": the pictures that have no tile.
            Rectangle {
                anchors.fill: parent
                visible: tile.more
                // An overlay on pictures: the Complementary color set (see ImageBlock).
                Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
                Kirigami.Theme.inherit: false
                color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)

                QQC2.Label {
                    id: label
                    objectName: "more"
                    // The ring of the pictures it covers takes the tile's middle: the number goes
                    // to the room above it, or beside it in a short tile (`numberBeside`). Inside
                    // the tile whatever its size: a small tile takes a smaller number, but never
                    // one below the theme's small font.
                    readonly property bool beside: tile.ringed && root.numberBeside
                    x: label.beside ? root.ringLeft + root.ringSide + Kirigami.Units.smallSpacing : Kirigami.Units.smallSpacing
                    y: tile.ringed && !label.beside ? Kirigami.Units.smallSpacing / 2 : Kirigami.Units.smallSpacing
                    width: Math.max(0, parent.width - label.x - Kirigami.Units.smallSpacing)
                    height: Math.max(0, tile.ringed && !label.beside ? (parent.height - root.ringSide) / 2 - Kirigami.Units.smallSpacing : parent.height - Kirigami.Units.smallSpacing * 2)
                    horizontalAlignment: Text.AlignHCenter
                    verticalAlignment: Text.AlignVCenter
                    text: L10n.i18nc("@info, %1 how many more pictures", "+%1", root.hidden)
                    color: Kirigami.Theme.textColor
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold,
                        "scale": 1.6
                    })
                    fontSizeMode: Text.Fit
                    minimumPointSize: Math.max(5, Kirigami.Theme.smallFont.pointSize)
                    minimumPixelSize: Math.max(5, Kirigami.Theme.smallFont.pixelSize)
                    textFormat: Text.PlainText
                }
                Accessible.name: L10n.i18ncp("@info accessible, %1 how many", "%1 more picture", "%1 more pictures", root.hidden)

                TapHandler {
                    // The X of the ring is tapped here, by position: a ring has no handler of its
                    // own (the tile's picture leaves the whole tile to this handler, `ringByAlbum`).
                    onTapped: eventPoint => {
                        if (root.overMore(eventPoint.position)) {
                            root.cancelMore();
                        } else {
                            root.openAt(tile.index);
                        }
                    }
                }
            }
        }
    }

    // The ring of the pictures "+N" covers, over the tile's overlay: ONE Loader for the whole
    // album, active only while they go up. (No size of its own: a Loader that has one resizes
    // the disc to it, and the disc is a circle only at its own width. The disc has no handler:
    // a tap on it reaches the overlay's, which looks at the position, `overMore`.)
    Loader {
        active: root.moreRing
        sourceComponent: Rectangle {
            id: disc
            objectName: "moreRing"
            x: root.inset + root.moreCell.x + root.ringLeft
            y: root.moreCell.y + root.ringTop
            width: root.ringSide
            height: width
            radius: width / 2
            Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
            Kirigami.Theme.inherit: false
            color: Qt.alpha(Kirigami.Theme.backgroundColor, Style.overlayOpacity)
            // The ring is the X for a screen reader; once pressed it is no longer a button until
            // the core answers.
            Accessible.ignored: !root.moreXShown
            Accessible.role: root.moreAsked ? Accessible.StaticText : Accessible.Button
            Accessible.name: L10n.i18nc("@action:button", "Cancel Sending")
            Accessible.onPressAction: root.cancelMore()
            RingDots {
                model: 24
                side: disc.width
                share: root.block.moreShare ?? 0
                ink: Kirigami.Theme.textColor
            }
            Kirigami.Icon {
                anchors.centerIn: parent
                visible: root.moreXShown
                width: Math.min(Kirigami.Units.iconSizes.small, disc.width - 4)
                height: width
                source: "window-close"
                color: Kirigami.Theme.textColor
                isMask: true
                opacity: root.moreCancel && !root.moreAsked ? 1 : 0.4
            }
        }
    }
}
