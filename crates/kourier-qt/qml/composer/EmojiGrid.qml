pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The emoji picker's content: a row of category tabs and the grid of the chosen category.
// Used by the composer's popup and by the reaction picker. Arrow keys move in the grid, Enter
// or Space picks.
ColumnLayout {
    id: root

    // Emoji per row.
    property int columns: 8
    // Rows shown before the grid scrolls.
    property int rows: 6
    // Emoji shown as chosen already (our own reactions).
    property var chosen: []
    property int category: 0

    readonly property real cell: Math.round(Kirigami.Units.gridUnit * 2)

    signal picked(string emoji)

    function focusGrid(): void {
        grid.forceActiveFocus();
    }

    spacing: Kirigami.Units.smallSpacing

    RowLayout {
        spacing: 0
        Layout.fillWidth: true

        Repeater {
            model: Emoji.categories
            delegate: QQC2.ToolButton {
                id: tab

                required property var modelData
                required property int index

                // The tabs share the width the grid needs; they do not widen the picker.
                Layout.preferredWidth: 1
                text: tab.modelData.icon
                font.family: "emoji"
                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.15
                checkable: true
                checked: root.category === tab.index
                autoExclusive: true
                Accessible.name: tab.modelData.name
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: tab.modelData.name
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                Layout.fillWidth: true
                onClicked: root.category = tab.index
            }
        }
    }

    Kirigami.Separator {
        Layout.fillWidth: true
    }

    GridView {
        id: grid

        clip: true
        model: Emoji.categories[root.category].emoji
        cellWidth: root.cell
        cellHeight: root.cell
        activeFocusOnTab: true
        keyNavigationEnabled: true
        boundsBehavior: Flickable.StopAtBounds
        // Room for the scroll bar beside the last column.
        implicitWidth: root.cell * root.columns + bar.width
        implicitHeight: root.cell * root.rows
        Layout.fillWidth: true
        Layout.preferredHeight: implicitHeight
        Accessible.name: Emoji.categories[root.category].name

        Keys.onReturnPressed: grid.pick()
        Keys.onEnterPressed: grid.pick()
        Keys.onSpacePressed: grid.pick()

        function pick(): void {
            if (grid.currentIndex >= 0 && grid.currentIndex < grid.count) {
                root.picked(Emoji.categories[root.category].emoji[grid.currentIndex]);
            }
        }

        QQC2.ScrollBar.vertical: QQC2.ScrollBar {
            id: bar
        }

        delegate: QQC2.ToolButton {
            id: key

            required property string modelData
            required property int index

            width: grid.cellWidth
            height: grid.cellHeight
            text: key.modelData
            font.family: "emoji"
            font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.4
            focusPolicy: Qt.NoFocus
            checked: root.chosen.indexOf(key.modelData) >= 0
            highlighted: grid.activeFocus && grid.currentIndex === key.index
            onClicked: root.picked(key.modelData)
        }
    }
}
