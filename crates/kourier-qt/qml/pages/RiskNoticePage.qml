pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The warning before a NEW account of an experimental network is linked (WhatsApp): what
// the network's terms say, what can happen, what Kourier never does. The words are the
// viewmodel's (`App.networkRisk(network)`), never the network's. The person ticks a box to
// go on: the box is never ticked beforehand and is not remembered, so every new account
// asks again. Nothing is sent to the core before the accepting button: no account, no key,
// no connection, so leaving this page (back, Settings, a chat) asks nothing.
//
// The accepting button reads "Continue" when the network is on; when it is off it reads "Turn
// On and Restart": the preference is written, Kourier starts again and opens the link page
// of this network by itself (the tick counts for that link).
Kirigami.ScrollablePage {
    id: page

    // The network ("WhatsApp") and the warning written for it (empty map: nothing to ask).
    property string network: ""
    property var view: ({})

    // The person ticked the box and pressed the accepting button; or went back.
    signal accepted(string network)
    signal cancelled

    // (Tests press and read these.)
    readonly property alias confirmBox: confirm
    readonly property alias acceptButton: accept
    readonly property alias cancelButton: cancel
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)

    title: page.view.title ?? ""
    Kirigami.ColumnView.fillWidth: true

    onBackRequested: event => {
        event.accepted = true;
        page.cancelled();
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Kirigami.Icon {
            source: "dialog-warning"
            implicitWidth: Kirigami.Units.gridUnit * 3
            implicitHeight: Kirigami.Units.gridUnit * 3
            Layout.alignment: Qt.AlignHCenter
            Layout.topMargin: Kirigami.Units.gridUnit
        }

        Repeater {
            model: page.view.paragraphs ?? []
            delegate: QQC2.Label {
                required property string modelData

                text: modelData
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 30
                Layout.alignment: Qt.AlignHCenter
            }
        }

        QQC2.CheckBox {
            id: confirm
            objectName: "riskConfirm"
            visible: (page.view.confirm ?? "") !== ""
            text: page.view.confirm ?? ""
            // Never ticked beforehand, never remembered.
            checked: false
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 30
            Layout.alignment: Qt.AlignHCenter
            Layout.topMargin: Kirigami.Units.largeSpacing
        }

        // Going on first turns the network on, which restarts Kourier.
        QQC2.Label {
            visible: text !== ""
            text: page.view.restartNote ?? ""
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: page.quiet
            font: Kirigami.Theme.smallFont
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 30
            Layout.alignment: Qt.AlignHCenter
        }

        // Side by side while both fit in the page, else one under the other (K-A fix U1: in
        // Ukrainian at 14 pt in the narrow window the row of two was wider than the page and
        // widened every paragraph above it). The column never grows with its buttons.
        GridLayout {
            id: buttons
            objectName: "riskButtons"

            readonly property bool stacked: cancel.implicitWidth + accept.implicitWidth + columnSpacing > page.availableWidth - Kirigami.Units.gridUnit * 2

            columns: buttons.stacked ? 1 : 2
            columnSpacing: Kirigami.Units.largeSpacing
            rowSpacing: Kirigami.Units.smallSpacing
            Layout.alignment: Qt.AlignHCenter
            Layout.fillWidth: buttons.stacked
            Layout.maximumWidth: Kirigami.Units.gridUnit * 30
            Layout.topMargin: Kirigami.Units.largeSpacing
            Layout.bottomMargin: Kirigami.Units.gridUnit

            QQC2.Button {
                id: cancel
                objectName: "riskCancel"
                text: page.view.cancel ?? ""
                icon.name: "dialog-cancel"
                Accessible.name: cancel.text
                Layout.fillWidth: buttons.stacked
                onClicked: page.cancelled()
            }
            QQC2.Button {
                id: accept
                objectName: "riskAccept"
                Layout.fillWidth: buttons.stacked
                text: page.view.accept ?? ""
                icon.name: "go-next"
                // Off for a reason the person can change: the screen reader is told which.
                enabled: confirm.checked || !confirm.visible
                Accessible.name: accept.text
                Accessible.description: accept.enabled ? "" : (page.view.acceptHint ?? "")
                onClicked: page.accepted(page.network)
            }
        }
    }
}
