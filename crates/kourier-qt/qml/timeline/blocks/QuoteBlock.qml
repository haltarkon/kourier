pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import app.kourier

// A quoted message: a bar in the author's color, the author, a two-line snippet (the quoted
// text, or what was quoted: "Photo", a file name) and, for a quoted picture or video, its
// thumbnail. Activating it scrolls to the original and highlights it. An original that is not
// on this device or was deleted since says so, in italics, and a missing one leads nowhere.
Rectangle {
    id: root

    required property var block
    required property color ink
    // The bubble the quote sits on.
    required property color surface

    // The original (or its tombstone) is a row of this conversation.
    readonly property bool reachable: root.block.target > 0
    readonly property color fill: Kirigami.ColorUtils.linearInterpolation(root.surface, root.ink, quoteHover.hovered && root.reachable ? 0.14 : 0.08)
    // Own quotes take the accent, others the author's color, each adjusted to be readable on
    // the quote's own background.
    readonly property color barColor: Style.readable(root.block.own ? Kirigami.Theme.highlightColor : Components.NameUtils.colorsFromString(root.block.author), root.fill)
    readonly property color snippetInk: Style.secondary(root.ink, root.fill)
    // A little smaller than the message text (TextBlock uses the default font). Not
    // `Kirigami.Theme.smallFont`: on this plain Item its point size can come out larger than
    // the default font's (12 vs 9 pt seen with the KDE platform theme).
    readonly property real quoteScale: 0.9
    readonly property real thumbSize: Math.round(Kirigami.Units.gridUnit * 2.4)
    readonly property bool hasThumb: root.block.thumbUrl !== "" && thumb.status !== Image.Error

    function activate(): void {
        if (root.reachable) {
            App.jumpTo(root.block.target);
        }
    }

    implicitWidth: Math.min(Kirigami.Units.gridUnit * 18, layout.implicitWidth + Kirigami.Units.largeSpacing + Kirigami.Units.smallSpacing)
    implicitHeight: layout.implicitHeight + Kirigami.Units.smallSpacing * 2
    radius: Kirigami.Units.cornerRadius
    color: root.fill
    clip: true

    Accessible.role: root.reachable ? Accessible.Button : Accessible.StaticText
    Accessible.name: L10n.i18nc("@info accessible, %1 author, %2 text", "Quote of %1: %2", root.block.author, root.block.text)
    Accessible.onPressAction: root.activate()

    Rectangle {
        width: 3
        height: parent.height
        color: root.barColor
    }

    RowLayout {
        id: layout
        anchors.fill: parent
        anchors.leftMargin: Kirigami.Units.largeSpacing
        anchors.rightMargin: Kirigami.Units.smallSpacing
        anchors.topMargin: Kirigami.Units.smallSpacing
        anchors.bottomMargin: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.largeSpacing

        ColumnLayout {
            spacing: 0
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter

            QQC2.Label {
                text: root.block.author
                font: Style.styled(Kirigami.Theme.defaultFont, {
                    "weight": Font.DemiBold,
                    "scale": root.quoteScale
                })
                color: root.barColor
                elide: Text.ElideRight
                textFormat: Text.PlainText
                Layout.fillWidth: true
            }
            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true
                Kirigami.Icon {
                    visible: root.block.icon !== ""
                    source: root.block.icon
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: root.snippetInk
                    isMask: true
                    Layout.alignment: Qt.AlignTop
                }
                QQC2.Label {
                    text: root.block.text
                    color: root.snippetInk
                    // A note in place of the quoted content, not the content.
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "italic": root.block.standIn === true,
                        "scale": root.quoteScale
                    })
                    elide: Text.ElideRight
                    maximumLineCount: 2
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                }
            }
        }

        // The quoted picture, from the image provider like any thumbnail of the timeline.
        Image {
            id: thumb
            visible: root.hasThumb
            source: root.block.thumbUrl
            asynchronous: true
            smooth: true
            fillMode: Image.PreserveAspectCrop
            clip: true
            sourceSize.width: Math.ceil(root.thumbSize)
            sourceSize.height: Math.ceil(root.thumbSize)
            Layout.preferredWidth: root.thumbSize
            Layout.preferredHeight: root.thumbSize
            Layout.alignment: Qt.AlignVCenter
        }
    }

    HoverHandler {
        id: quoteHover
        cursorShape: root.reachable ? Qt.PointingHandCursor : Qt.ArrowCursor
    }
    TapHandler {
        enabled: root.reachable
        onTapped: root.activate()
    }
}
