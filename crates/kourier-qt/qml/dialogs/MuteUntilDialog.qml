pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.dateandtime as DateTime
import app.kourier

// "Mute Until": a chat is muted until a day and a time the person picks (A10d, his answer to
// question 2: two fields side by side, a Date and a Time, each opening KDE's own calendar or
// clock). It starts at the next day's 8:00 (`App.muteUntilStart`, computed in Rust), "Mute" waits
// for a time that lies at least a minute ahead (`App.muteUntilAllowed`) and the core decides
// again at the click (`App.muteUntil`): a time that has gone by meanwhile keeps the dialog and
// says so. It is loaded by URL when first asked for (`MainWindow.askMuteUntil`): a file a page
// names loads its imports with the page, and `org.kde.kirigamiaddons.dateandtime` is not needed
// before this dialog.
Kirigami.Dialog {
    id: root

    // The chat the mute is for (a chat that goes away closes the dialog).
    property real conversation: 0
    // The day and the time on the fields (month 1-12, hour 0-23).
    property int year: 2000
    property int month: 1
    property int day: 1
    property int hour: 8
    property int minute: 0
    // The click found the time too near (or the network refused): the quiet line says so.
    property bool refused: false
    // The date the fields name (only for showing them: whether it is allowed is the core's).
    readonly property date picked: new Date(root.year, root.month - 1, root.day, root.hour, root.minute)
    readonly property bool allowed: App.muteUntilAllowed(root.year, root.month, root.day, root.hour, root.minute)
    // The two popups of KDE's calendar and clock (tests drive them).
    readonly property alias datePopup: datePopup
    readonly property alias timePopup: timePopup
    readonly property alias dateField: dateField
    readonly property alias timeField: timeField

    // The fields at the start the core names.
    function startAgain(): void {
        const start = App.muteUntilStart();
        root.year = start.year;
        root.month = start.month;
        root.day = start.day;
        root.hour = start.hour;
        root.minute = start.minute;
        root.refused = false;
    }

    // Mute: the core says whether the time held (not the fields' idea of it).
    function accept(): void {
        if (!root.allowed) {
            return;
        }
        if (App.muteUntil(root.conversation, root.year, root.month, root.day, root.hour, root.minute)) {
            root.close();
        } else {
            root.refused = true;
        }
    }

    // Return presses Mute, once: a repeat of a held Return (the key that opened this dialog may
    // still be down) does nothing. The parameter is untyped so that a test can hand over a
    // stand-in event (`TestEvent` sends no auto-repeat).
    function returnKey(event: var): void {
        event.accepted = true;
        if (!event.isAutoRepeat) {
            root.accept();
        }
    }

    title: L10n.i18nc("@title:dialog", "Mute Until")
    preferredWidth: Kirigami.Units.gridUnit * 22
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Mute")
            icon.name: "player-volume-muted"
            enabled: root.allowed
            onTriggered: root.accept()
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]

    ColumnLayout {
        id: body

        spacing: Kirigami.Units.largeSpacing
        focus: true
        Keys.onReturnPressed: event => root.returnKey(event)
        Keys.onEnterPressed: event => root.returnKey(event)

        QQC2.Label {
            text: L10n.i18nc("@label", "Mute notifications from this chat until:")
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            // The two fields: a button each that shows the value and opens the popup.
            QQC2.Button {
                id: dateField

                objectName: "untilDate"
                icon.name: "view-calendar"
                text: root.picked.toLocaleDateString(Qt.locale(), Locale.ShortFormat)
                Accessible.name: L10n.i18nc("@label accessible, followed by the date", "Date: %1", dateField.text)
                Layout.fillWidth: true
                onClicked: datePopup.open()
            }
            QQC2.Button {
                id: timeField

                objectName: "untilTime"
                icon.name: "clock"
                text: root.picked.toLocaleTimeString(Qt.locale(), Locale.ShortFormat)
                Accessible.name: L10n.i18nc("@label accessible, followed by the time", "Time: %1", timeField.text)
                Layout.fillWidth: true
                onClicked: timePopup.open()
            }
        }
        QQC2.Label {
            objectName: "untilNote"
            visible: root.refused || !root.allowed
            text: L10n.i18nc("@info", "Choose a time at least a minute from now.")
            font: Kirigami.Theme.smallFont
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
    }

    // KDE's calendar and clock. They are made with the dialog and open above it (a popup
    // opened later stands over the one before).
    DateTime.DatePopup {
        id: datePopup

        parent: QQC2.Overlay.overlay
        anchors.centerIn: parent
        value: root.picked
        minimumDate: new Date(1970, 0, 1)
        maximumDate: new Date(9999, 11, 31)
        onAccepted: {
            root.year = datePopup.value.getFullYear();
            root.month = datePopup.value.getMonth() + 1;
            root.day = datePopup.value.getDate();
            root.refused = false;
        }
    }
    DateTime.TimePopup {
        id: timePopup

        parent: QQC2.Overlay.overlay
        anchors.centerIn: parent
        value: root.picked
        onAccepted: {
            root.hour = timePopup.value.getHours();
            root.minute = timePopup.value.getMinutes();
            root.refused = false;
        }
    }

    // A chat that goes away (deleted on the phone, its account removed) takes its dialog along.
    Timer {
        interval: 1000
        repeat: true
        running: root.opened
        onTriggered: {
            if (!App.chatOffer(root.conversation).shown) {
                root.close();
            }
        }
    }

    onOpened: body.forceActiveFocus()

    // The item that had the keyboard when the question was asked (the chat list, from the row
    // menu): it has it again once the dialog is closed (M5).
    property Item returnTo: null
    onClosed: {
        if (root.returnTo !== null) {
            root.returnTo.forceActiveFocus();
            root.returnTo = null;
        }
    }
}
