import QtQuick
import QtQuick.Shapes
import org.kde.kirigami as Kirigami
import app.kourier

// Delivery state of an own message: clock, one check, two checks, two bold checks, error.
// Checks are drawn as paths so they take any color (bundled SVGs cannot be recolored). The
// callers that know what the icon stands on pass colors for it; the defaults are for the
// theme's background.
Item {
    id: root

    property string receipt
    property color tint: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
    property color readTint: Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor)

    readonly property bool checks: root.receipt === "sent" || root.receipt === "delivered" || root.receipt === "read"

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

    Shape {
        visible: root.checks
        anchors.fill: parent
        preferredRendererType: Shape.CurveRenderer

        // 16×16 design grid.
        transform: Scale {
            xScale: root.width / 16
            yScale: root.height / 16
        }

        ShapePath {
            strokeColor: root.receipt === "read" ? root.readTint : root.tint
            strokeWidth: root.receipt === "read" ? 2 : 1.5
            fillColor: "transparent"
            capStyle: ShapePath.RoundCap
            joinStyle: ShapePath.RoundJoin
            PathSvg {
                path: root.receipt === "sent" ? "M3.5 8.5 L6.3 11.3 L12.5 4.7" : "M1.2 8.6 L3.9 11.3 L10 4.8 M7.4 10.6 L8.2 11.3 L14.3 4.8"
            }
        }
    }
}
