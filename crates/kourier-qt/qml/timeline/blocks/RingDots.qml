pragma ComponentBehavior: Bound

import QtQuick
import org.kde.kirigami as Kirigami

// The dots of a progress ring: 24 small dots on a circle of diameter `side` around the middle
// of the item they are declared in, opaque up to `share` (0 … 1), faint after it. It IS a
// Repeater, so a ring nobody wants has `model: 0` and costs no item (a row is made many
// times); `ImageBlock`'s own ring (a download) has the same shape.
Repeater {
    id: root

    property real side: 0
    property real share: 0
    property color ink: Kirigami.Theme.textColor

    delegate: Rectangle {
        required property int index

        width: 3
        height: 3
        radius: 1.5
        x: root.side / 2 - 1.5 + (root.side / 2 - 4) * Math.sin(index / 24 * 2 * Math.PI)
        y: root.side / 2 - 1.5 - (root.side / 2 - 4) * Math.cos(index / 24 * 2 * Math.PI)
        color: root.ink
        opacity: index / 24 < root.share ? 1 : 0.3
    }
}
