pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// One member of a group in the details panel (people pages P2): the picture, the name ("You"
// for the user's own row) and, at the row's trailing end, the quiet word "Admin" for an
// administrator. Every text from the network is plain text: the content is made of labels of
// our own with `textFormat: Text.PlainText`, never the style's label (it guesses the format).
// The row only says it was chosen (`clicked`); the page decides what that opens.
QQC2.ItemDelegate {
    id: root

    // The member: `{user, name, avatarUrl, admin, isSelf}`.
    required property var member
    // The word on an administrator's row.
    property string adminLabel: ""

    Accessible.name: Style.unmarked(root.member.name)
    Accessible.description: root.member.admin ? root.adminLabel : ""
    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        Avatar {
            name: root.member.name
            source: root.member.avatarUrl
            implicitWidth: Kirigami.Units.gridUnit * 2
            implicitHeight: Kirigami.Units.gridUnit * 2
        }
        QQC2.Label {
            objectName: "memberName"
            Layout.fillWidth: true
            text: root.member.name
            elide: Text.ElideRight
            textFormat: Text.PlainText
        }
        QQC2.Label {
            objectName: "memberAdmin"
            visible: root.member.admin
            text: root.adminLabel
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            font: Kirigami.Theme.smallFont
            textFormat: Text.PlainText
        }
    }
}
