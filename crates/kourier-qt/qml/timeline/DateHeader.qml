import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// A date separator: a small centered pill with a frame of the theme's frame contrast (the
// fill alone is invisible in the high-contrast schemes).
Item {
    id: root

    required property string text

    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: pill.height + Kirigami.Units.largeSpacing * 2

    Rectangle {
        id: pill
        anchors.centerIn: parent
        width: label.implicitWidth + Kirigami.Units.largeSpacing * 2
        height: label.implicitHeight + Kirigami.Units.smallSpacing * 1.5
        radius: height / 2
        color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.07)
        border.width: 1
        border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)

        QQC2.Label {
            id: label
            anchors.centerIn: parent
            text: root.text
            font: Kirigami.Theme.smallFont
            color: Style.secondary(Kirigami.Theme.textColor, pill.color)
            textFormat: Text.PlainText
        }
    }
}
