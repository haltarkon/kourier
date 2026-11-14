import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

// One emoji of a picker: the glyph on a flat button, its name (in the interface language) as the
// accessible name and the tooltip. A right click asks for the pin and favourite menu; the Menu key
// and Shift+F10 on the current cell do the same through the grid.
QQC2.ToolButton {
    id: root

    property string glyph: ""
    // The emoji's name in the interface language (CLDR).
    property string label: ""
    // The keyboard's current cell.
    property bool current: false

    // Where in this cell the menu was asked for.
    signal menuRequested(real x, real y)

    text: root.glyph
    font.family: "emoji"
    font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.4
    focusPolicy: Qt.NoFocus
    highlighted: root.current
    Accessible.name: root.label.length > 0 ? root.label : root.glyph
    QQC2.ToolTip.visible: root.hovered && root.label.length > 0
    QQC2.ToolTip.text: root.label
    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay

    TapHandler {
        acceptedButtons: Qt.RightButton
        onTapped: point => root.menuRequested(point.position.x, point.position.y)
    }
}
