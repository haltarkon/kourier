import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// A stand-in: deleted, undecryptable, expired, unsupported, contact, location, poll.
RowLayout {
    id: root

    required property var block
    required property color ink

    spacing: Kirigami.Units.smallSpacing

    Kirigami.Icon {
        visible: root.block.icon !== ""
        source: root.block.icon
        implicitWidth: Kirigami.Units.iconSizes.small
        implicitHeight: Kirigami.Units.iconSizes.small
        color: root.ink
        isMask: true
    }
    QQC2.Label {
        text: root.block.text
        color: root.ink
        font: Style.styled(Kirigami.Theme.defaultFont, {
            "italic": true
        })
        wrapMode: Text.Wrap
        textFormat: Text.PlainText
        Layout.fillWidth: true
    }
}
