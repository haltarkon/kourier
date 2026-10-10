pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// Who reacted to a message: one line per person, grouped by emoji (most used first). Our own
// reaction can be taken back from here.
QQC2.Popup {
    id: root

    // The message (`TimelineActions` row description).
    property var row: null

    readonly property var chips: root.row ? root.row.reactions : []
    readonly property int total: {
        let n = 0;
        for (const chip of root.chips) {
            n += chip.count;
        }
        return n;
    }
    // One entry per person: the emoji, the name, and whether it is us.
    readonly property var people: {
        const out = [];
        for (const chip of root.chips) {
            for (let i = 0; i < chip.names.length; ++i) {
                out.push({
                    "emoji": chip.emoji,
                    "name": chip.names[i],
                    "mine": i === chip.ownAt
                });
            }
        }
        return out;
    }

    popupType: QQC2.Popup.Item
    padding: Kirigami.Units.largeSpacing
    focus: true

    contentItem: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing

        Kirigami.Heading {
            level: 4
            text: L10n.i18ncp("@title", "%1 Reaction", "%1 Reactions", root.total)
            Layout.fillWidth: true
        }

        ListView {
            id: list

            clip: true
            model: root.people
            spacing: 0
            boundsBehavior: Flickable.StopAtBounds
            implicitWidth: Kirigami.Units.gridUnit * 14
            implicitHeight: Math.min(contentHeight, Kirigami.Units.gridUnit * 14)
            Layout.fillWidth: true

            QQC2.ScrollBar.vertical: QQC2.ScrollBar {}

            delegate: RowLayout {
                id: person

                required property var modelData

                width: ListView.view ? ListView.view.width : implicitWidth
                // At least the usual height, more for a larger font.
                height: Math.max(Math.round(Kirigami.Units.gridUnit * 1.8), Math.ceil(person.implicitHeight) + Kirigami.Units.smallSpacing)
                spacing: Kirigami.Units.largeSpacing

                QQC2.Label {
                    text: person.modelData.emoji
                    textFormat: Text.PlainText
                    font.family: "emoji"
                    font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.3
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 1.6
                    horizontalAlignment: Text.AlignHCenter
                }
                QQC2.Label {
                    text: person.modelData.name
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": person.modelData.mine ? Font.DemiBold : Font.Normal
                    })
                    Layout.fillWidth: true
                }
                QQC2.ToolButton {
                    visible: person.modelData.mine && root.row !== null && root.row.actions.react
                    icon.name: "edit-delete-remove"
                    display: QQC2.AbstractButton.IconOnly
                    text: L10n.i18nc("@action:button", "Remove My Reaction")
                    QQC2.ToolTip.visible: hovered
                    QQC2.ToolTip.text: text
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    onClicked: {
                        App.react(root.row.messageId, person.modelData.emoji, true);
                        root.close();
                    }
                }
            }
        }
    }
}
