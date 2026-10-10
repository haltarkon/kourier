import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "N unread messages": accent line across the timeline. The lines are the accent itself; the
// words are the accent adjusted until they can be read on the page.
Item {
    id: root

    required property string text

    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: row.implicitHeight + Kirigami.Units.largeSpacing * 2

    RowLayout {
        id: row
        anchors.verticalCenter: parent.verticalCenter
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.leftMargin: Kirigami.Units.gridUnit
        anchors.rightMargin: Kirigami.Units.gridUnit
        spacing: Kirigami.Units.largeSpacing

        Rectangle {
            implicitHeight: 1
            color: Kirigami.Theme.highlightColor
            Layout.fillWidth: true
        }
        QQC2.Label {
            text: root.text
            font: Kirigami.Theme.smallFont
            color: Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor)
            textFormat: Text.PlainText
        }
        Rectangle {
            implicitHeight: 1
            color: Kirigami.Theme.highlightColor
            Layout.fillWidth: true
        }
    }
}
