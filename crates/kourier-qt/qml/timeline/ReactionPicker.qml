pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// Add a reaction: a quick row of six, and the whole emoji picker behind the last button. Our
// own reactions show as chosen; picking one of them takes it back.
QQC2.Popup {
    id: root

    // The message (`TimelineActions` row description).
    property var row: null
    // The whole picker is open.
    property bool expanded: false

    // Our reactions on the message.
    readonly property var mine: {
        const out = [];
        if (root.row) {
            for (const chip of root.row.reactions) {
                if (chip.mine) {
                    out.push(chip.emoji);
                }
            }
        }
        return out;
    }

    function pick(emoji: string): void {
        if (root.row) {
            App.react(root.row.messageId, emoji, root.mine.indexOf(emoji) >= 0);
        }
        root.close();
    }

    popupType: QQC2.Popup.Item
    padding: Kirigami.Units.smallSpacing
    focus: true
    onClosed: root.expanded = false

    contentItem: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            spacing: 0
            Layout.fillWidth: true

            Repeater {
                model: Emoji.quick
                delegate: QQC2.ToolButton {
                    id: quickButton

                    required property string modelData

                    readonly property bool chosen: root.mine.indexOf(quickButton.modelData) >= 0

                    implicitWidth: Math.round(Kirigami.Units.gridUnit * 2.2)
                    implicitHeight: implicitWidth
                    text: quickButton.modelData
                    font.family: "emoji"
                    font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.5
                    checked: quickButton.chosen
                    Accessible.name: quickButton.chosen ? L10n.i18nc("@action:button accessible", "Remove reaction %1", quickButton.modelData) : L10n.i18nc("@action:button accessible", "React with %1", quickButton.modelData)
                    onClicked: root.pick(quickButton.modelData)
                }
            }

            QQC2.ToolButton {
                implicitWidth: Math.round(Kirigami.Units.gridUnit * 2.2)
                implicitHeight: implicitWidth
                icon.name: "smiley-add"
                display: QQC2.AbstractButton.IconOnly
                text: L10n.i18nc("@action:button", "More Emoji")
                checkable: true
                checked: root.expanded
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: {
                    root.expanded = !root.expanded;
                    if (root.expanded) {
                        full.focusGrid();
                    }
                }
            }
        }

        EmojiGrid {
            id: full
            visible: root.expanded
            // About as wide as the quick row above it (the eight category tabs set the width).
            columns: 7
            rows: 5
            chosen: root.mine
            Layout.fillWidth: true
            onPicked: emoji => root.pick(emoji)
        }
    }
}
