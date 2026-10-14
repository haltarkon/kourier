pragma ComponentBehavior: Bound

import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// Delivery state of an own message: clock, one check, two checks, two bold checks, error.
// Checks are drawn as strokes so they take any color (bundled SVGs cannot be recolored). The
// callers that know what the icon stands on pass colors for it; the defaults are for the
// theme's background.
//
// The strokes are plain rounded rectangles, never QtQuick.Shapes: with the threaded render
// loop Qt's software renderer of a `Shape` reads the item's window from the render thread,
// and an icon whose row had just left the list (hundreds go at once when an account is
// removed) crashed the application there (`target/link-feedback/crash.md`). They are also
// cheaper than a `Shape` in every list row and own message.
Item {
    id: root

    property string receipt
    property color tint: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
    property color readTint: Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor)

    readonly property bool checks: root.receipt === "sent" || root.receipt === "delivered" || root.receipt === "read"
    readonly property bool two: root.receipt === "delivered" || root.receipt === "read"
    // One unit of the 16×16 design grid.
    readonly property real unit: root.width / 16
    readonly property color strokeColor: root.receipt === "read" ? root.readTint : root.tint
    readonly property real strokeWidth: (root.receipt === "read" ? 2 : 1.5) * root.unit

    implicitWidth: Kirigami.Units.iconSizes.small
    implicitHeight: Kirigami.Units.iconSizes.small
    visible: root.receipt !== ""

    Kirigami.Icon {
        anchors.fill: parent
        visible: !root.checks
        source: root.receipt === "pending" ? "clock" : (root.receipt === "failed" ? "dialog-error" : "")
        color: root.tint
        isMask: root.receipt === "pending"
    }

    // One straight stroke with round ends from (x1, y1) to (x2, y2) on the design grid;
    // two of them meeting at a point make a round join.
    component Stroke: Rectangle {
        id: stroke

        property real x1
        property real y1
        property real x2
        property real y2
        readonly property real dx: (stroke.x2 - stroke.x1) * root.unit
        readonly property real dy: (stroke.y2 - stroke.y1) * root.unit

        visible: root.checks
        antialiasing: true
        color: root.strokeColor
        width: Math.sqrt(stroke.dx * stroke.dx + stroke.dy * stroke.dy) + root.strokeWidth
        height: root.strokeWidth
        radius: root.strokeWidth / 2
        x: (stroke.x1 + stroke.x2) / 2 * root.unit - stroke.width / 2
        y: (stroke.y1 + stroke.y2) / 2 * root.unit - stroke.height / 2
        rotation: Math.atan2(stroke.dy, stroke.dx) * 180 / Math.PI
    }

    // One check ("sent"): M3.5 8.5 L6.3 11.3 L12.5 4.7. Two checks: M1.2 8.6 L3.9 11.3
    // L10 4.8 and M7.4 10.6 L8.2 11.3 L14.3 4.8.
    Stroke {
        x1: root.two ? 1.2 : 3.5
        y1: root.two ? 8.6 : 8.5
        x2: root.two ? 3.9 : 6.3
        y2: 11.3
    }
    Stroke {
        x1: root.two ? 3.9 : 6.3
        y1: 11.3
        x2: root.two ? 10 : 12.5
        y2: root.two ? 4.8 : 4.7
    }
    Stroke {
        visible: root.checks && root.two
        x1: 7.4
        y1: 10.6
        x2: 8.2
        y2: 11.3
    }
    Stroke {
        visible: root.checks && root.two
        x1: 8.2
        y1: 11.3
        x2: 14.3
        y2: 4.8
    }
}
