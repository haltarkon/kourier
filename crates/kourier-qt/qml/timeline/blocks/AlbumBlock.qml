pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// Several pictures and videos of one message as one grid (§8.3): two side by side, three as
// one large and two small, four as two rows of two, five as two over three. A message with
// more shows five tiles, the last one saying how many more there are; the viewer steps through
// all of them. That is a choice of this layout, not a limit: nothing a message carries is
// dropped (Signal Desktop's own grid stops at five tiles the same way, `SD/ts/components/
// conversation/ImageGrid.dom.tsx`, but its composer also refuses to send more than 32, which
// Kourier does not copy).
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
    // Two pictures side by side fill a bubble that a long caption made wider: a strip half
    // as tall as it is wide stays a comfortable size, where a grid of four or five grown
    // the same way would tower over its caption. (The block's own `implicitWidth` stays
    // `natural`, so the bubble's width never depends on this.)
    readonly property real side: root.tiles === 2 ? Math.max(root.natural, Math.round(Math.min(root.width, root.available))) : root.natural

    // Tile rectangles as fractions of the grid: [x, y, w, h] in units of the side.
    readonly property var cells: {
        switch (root.tiles) {
        case 2:
            return [[0, 0, 0.5, 0.5], [0.5, 0, 0.5, 0.5]];
        case 3:
            return [[0, 0, 0.66, 0.66], [0.66, 0, 0.34, 0.33], [0.66, 0.33, 0.34, 0.33]];
        case 4:
            return [[0, 0, 0.5, 0.5], [0.5, 0, 0.5, 0.5], [0, 0.5, 0.5, 0.5], [0.5, 0.5, 0.5, 0.5]];
        default:
            return [[0, 0, 0.5, 0.5], [0.5, 0, 0.5, 0.5], [0, 0.5, 1 / 3, 1 / 3], [1 / 3, 0.5, 1 / 3, 1 / 3], [2 / 3, 0.5, 1 / 3, 1 / 3]];
        }
    }
    // Height of the grid in units of the side.
    readonly property real rows: {
        switch (root.tiles) {
        case 2:
            return 0.5;
        case 3:
            return 0.66;
        case 4:
            return 1;
        default:
            return 0.5 + 1 / 3;
        }
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

    implicitWidth: root.natural
    implicitHeight: Math.round(root.side * root.rows)

    Accessible.role: Accessible.Graphic
    Accessible.name: L10n.i18ncp("@info accessible, %1 how many", "Album of %1 picture", "Album of %1 pictures", root.count)

    Repeater {
        model: root.tiles
        delegate: Item {
            id: tile

            required property int index

            readonly property var cell: root.cells[tile.index]
            readonly property bool more: root.hidden > 0 && tile.index === root.tiles - 1
            // A bubble made wider by a long caption: the grid stands in its middle.
            readonly property real inset: Math.max(0, Math.round((root.width - root.side) / 2))

            // Whole pixels, the gap taken from inside each tile.
            x: tile.inset + Math.round(tile.cell[0] * root.side) + (tile.cell[0] > 0 ? root.gap / 2 : 0)
            y: Math.round(tile.cell[1] * root.side) + (tile.cell[1] > 0 ? root.gap / 2 : 0)
            width: Math.round((tile.cell[0] + tile.cell[2]) * root.side) - Math.round(tile.cell[0] * root.side) - (tile.cell[0] > 0 ? root.gap / 2 : 0) - (tile.cell[0] + tile.cell[2] < 0.999 ? root.gap / 2 : 0)
            height: Math.round((tile.cell[1] + tile.cell[3]) * root.side) - Math.round(tile.cell[1] * root.side) - (tile.cell[1] > 0 ? root.gap / 2 : 0) - (tile.cell[1] + tile.cell[3] < root.rows - 0.001 ? root.gap / 2 : 0)
            clip: true

            ImageBlock {
                anchors.fill: parent
                block: root.items[tile.index]
                message: root.message
                // A tile is cropped to its cell: no label over small pictures.
                tile: true
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
                    anchors.centerIn: parent
                    text: L10n.i18nc("@info, %1 how many more pictures", "+%1", root.hidden)
                    color: Kirigami.Theme.textColor
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold,
                        "scale": 1.6
                    })
                    textFormat: Text.PlainText
                }
                Accessible.name: L10n.i18ncp("@info accessible, %1 how many", "%1 more picture", "%1 more pictures", root.hidden)

                TapHandler {
                    onTapped: root.openAt(tile.index)
                }
            }
        }
    }
}
