import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// A centered notice: member changes, safety numbers, timers, calls. The icon sits right before
// the text's widest line, also when the text wraps in a narrow window. What the core offers for
// it (removing it from this account) is in its context menu: a right click, a long press on a
// touch screen, or the Menu key when the timeline's keyboard focus is on it.
//
// A call line (`tone`): while the call rings the icon is in the accent colour and fades in and
// out, the text in the normal colour; a missed or declined call is in the theme's negative text
// colour; every other call is quiet like the other notices. The row changes in place when the
// core changes the line (a ring becomes a missed call), so everything here follows `tone`.
Item {
    id: root

    required property int index
    required property real messageId
    required property string text
    required property string icon
    // "", "ringing" or "negative" (`NoticeTone` of the viewmodel).
    required property string tone
    required property string timeLabel
    required property var actions

    readonly property real iconRoom: glyph.visible ? glyph.width + Kirigami.Units.smallSpacing : 0
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
    readonly property bool ringing: root.tone === "ringing"
    readonly property color textTone: root.tone === "negative" ? Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor) : root.ringing ? Kirigami.Theme.textColor : root.quiet
    readonly property color iconTone: root.ringing ? Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor) : root.textTone
    // The icon of a ringing call fades in and out, unless the person turned animations off (the
    // theme's durations are 0), the window is hidden or is not the active one (as animated
    // pictures: nothing moves in a window nobody works in), or this is a screenshot run (a
    // picture must not depend on the moment it is taken).
    readonly property bool pulsing: root.ringing && root.visible && root.Window.active && Kirigami.Units.longDuration > 0 && App.screenshot === ""
    // The fade is running (what a test looks at).
    readonly property bool animating: pulse.status === Loader.Ready
    // The timeline's keyboard focus is on this row.
    readonly property bool current: root.ListView.isCurrentItem && root.ListView.view !== null && root.ListView.view.activeFocus
    readonly property bool offers: root.actions !== undefined && root.actions.any === true

    // What the shared popups need to know about this row (`TimelineActions`).
    function rowInfo(): var {
        return {
            "messageId": root.messageId,
            "own": false,
            "author": "",
            "actions": root.actions,
            "copyText": "",
            "reactions": [],
            "failed": false,
            "edited": false,
            "system": true
        };
    }

    // The context menu without a pointer (the Menu key): beside the notice.
    function openMenu(): void {
        if (root.offers) {
            TimelineActions.menu(label, root.rowInfo(), -1, 0);
        }
    }

    function openDelete(): void {
        TimelineActions.remove(root.rowInfo());
    }

    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: label.implicitHeight + Kirigami.Units.largeSpacing * 2

    Accessible.role: Accessible.ListItem
    Accessible.name: root.text + ", " + root.timeLabel

    TapHandler {
        acceptedButtons: Qt.RightButton
        enabled: root.offers
        onTapped: point => TimelineActions.menu(root, root.rowInfo(), Math.round(point.position.x), Math.round(point.position.y))
    }
    TapHandler {
        id: longPress
        acceptedDevices: PointerDevice.TouchScreen | PointerDevice.Stylus
        enabled: root.offers
        onLongPressed: TimelineActions.menu(root, root.rowInfo(), Math.round(longPress.point.position.x), Math.round(longPress.point.position.y))
    }

    // The row with the keyboard focus, or the one a popup is about.
    Rectangle {
        visible: root.current || TimelineActions.activeMessage === root.messageId
        x: Math.round(Math.min(label.x, glyph.visible ? glyph.x : label.x) - Kirigami.Units.largeSpacing)
        y: Math.round(label.y - Kirigami.Units.smallSpacing)
        width: Math.round(label.x + label.width - x + Kirigami.Units.largeSpacing)
        height: Math.round(label.height + Kirigami.Units.smallSpacing * 2)
        radius: height / 2
        color: "transparent"
        border.width: root.current ? 2 : 1
        border.color: Kirigami.Theme.focusColor
    }

    QQC2.Label {
        id: label

        // Centre icon + text as one group; the label never exceeds the side margins.
        width: Math.min(implicitWidth, root.width - Kirigami.Units.gridUnit * 2 - root.iconRoom)
        x: Math.round((root.width - width + root.iconRoom) / 2)
        y: Kirigami.Units.largeSpacing
        text: root.text + " · " + root.timeLabel
        font: Kirigami.Theme.smallFont
        color: root.textTone
        wrapMode: Text.Wrap
        horizontalAlignment: Text.AlignHCenter
        textFormat: Text.PlainText
    }

    Kirigami.Icon {
        id: glyph

        visible: root.icon !== ""
        source: root.icon
        width: Kirigami.Units.iconSizes.small
        height: Kirigami.Units.iconSizes.small
        x: Math.round(label.x + (label.width - label.contentWidth) / 2 - root.iconRoom)
        // Centred on the first line.
        y: Math.round(label.y + (label.contentHeight / Math.max(1, label.lineCount) - height) / 2)
        color: root.iconTone
        isMask: true
        // Calls: the theme may lack the finer names (the freedesktop set has call-start).
        fallback: root.icon.indexOf("call-") === 0 ? "call-start" : ""
        opacity: pulse.item !== null ? (pulse.item as Fade).level : 1
    }

    // What the fade is made of: an item whose `level` runs between 1 and 0.3 and back.
    component Fade: Item {
        id: fade

        property real level: 1

        SequentialAnimation {
            running: true
            loops: Animation.Infinite
            NumberAnimation {
                target: fade
                property: "level"
                to: 0.3
                duration: Kirigami.Units.veryLongDuration * 2
                easing.type: Easing.InOutSine
            }
            NumberAnimation {
                target: fade
                property: "level"
                to: 1
                duration: Kirigami.Units.veryLongDuration * 2
                easing.type: Easing.InOutSine
            }
        }
    }

    // The fade of a ringing call, made only while it rings (an inactive Loader costs a notice
    // row almost nothing; an animation of its own would be made for every one of them).
    Loader {
        id: pulse

        active: root.pulsing
        sourceComponent: Fade {}
    }
}
