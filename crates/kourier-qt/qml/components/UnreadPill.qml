import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The unread count bubble: accent when it matters, quiet when muted; "@" for mentions; a plain
// dot for a conversation marked unread without a count. On the accent the count has the
// color the theme gives its highlighted text (the theme's own pairing); when muted, the pill
// is a step from the background towards the text, with the text color on it.
Rectangle {
    id: root

    property int count: 0
    property bool muted: false
    property bool mention: false
    property bool compact: false
    property bool dot: false

    readonly property string label: root.count > 999 ? "999+" : root.count.toString()
    readonly property color accent: Kirigami.Theme.highlightColor

    // At least the height the theme's small font needs, so the count never clips.
    implicitHeight: root.dot ? Math.round(Kirigami.Units.gridUnit * 0.6) : Math.max(Math.round(Kirigami.Units.gridUnit * (root.compact ? 0.9 : 1.15)), Math.ceil(countLabel.implicitHeight))
    implicitWidth: root.dot ? implicitHeight : Math.max(implicitHeight, countLabel.implicitWidth + Kirigami.Units.smallSpacing * 2.5)
    radius: height / 2
    color: root.muted ? Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.2) : root.accent

    QQC2.Label {
        id: countLabel
        visible: !root.dot
        anchors.centerIn: parent
        text: root.mention ? "@" : root.label
        color: root.muted ? Kirigami.Theme.textColor : Kirigami.Theme.highlightedTextColor
        font: Style.styled(Kirigami.Theme.smallFont, {
            "weight": Font.DemiBold
        })
    }
}
