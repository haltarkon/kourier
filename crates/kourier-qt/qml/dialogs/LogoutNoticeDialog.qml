pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// What became of the device of an account that was removed or unlinked, when the network
// could not be told (link feedback L4): what happened here (`text`) and what to do on the
// phone (`onPhone`, which names the device). It stays until the user closes it: the account
// may be gone from this computer, and nothing else would say it again. Both texts come from
// the viewmodel and can carry an account's or a device's name: plain text.
Kirigami.PromptDialog {
    id: root

    property string text: ""
    property string onPhone: ""

    // The button (tests press it).
    readonly property Item closeButton: okButton

    dialogType: Kirigami.PromptDialog.Warning
    preferredWidth: Kirigami.Units.gridUnit * 28
    maximumWidth: Kirigami.Units.gridUnit * 28
    standardButtons: Kirigami.Dialog.NoButton

    footer: QQC2.DialogButtonBox {
        padding: Kirigami.Units.largeSpacing
        QQC2.Button {
            id: okButton
            text: L10n.i18nc("@action:button", "Close")
            icon.name: "dialog-close"
            Accessible.name: okButton.text
            QQC2.DialogButtonBox.buttonRole: QQC2.DialogButtonBox.AcceptRole
            onClicked: root.close()
        }
    }

    onOpened: okButton.forceActiveFocus(Qt.TabFocusReason)
    onClosed: root.destroy()

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        QQC2.Label {
            text: root.text
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
        QQC2.Label {
            text: root.onPhone
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            font: Style.styled(Kirigami.Theme.defaultFont, {
                "weight": Font.DemiBold
            })
            Layout.fillWidth: true
        }
    }
}
