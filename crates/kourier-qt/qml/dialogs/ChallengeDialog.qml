import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The network wants a check that a person is sending (a captcha) before it takes more
// messages. The check runs in the browser; what the browser hands back afterwards reaches
// Kourier as a link it is registered for, or is pasted here by hand.
Kirigami.Dialog {
    id: root

    property real account: 0
    // Where the check is done. Handed to the browser only when the user asks for it.
    property string url: ""

    title: L10n.i18nc("@title:dialog", "Verify to Keep Sending")
    preferredWidth: Kirigami.Units.gridUnit * 24
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton

    customFooterActions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Submit")
            icon.name: "dialog-ok-apply"
            enabled: answer.text.trim().length > 0
            onTriggered: {
                App.submitChallenge(root.account, answer.text);
                root.close();
            }
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Close")
            icon.name: "dialog-close"
            onTriggered: root.close()
        }
    ]

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        QQC2.Label {
            text: L10n.i18nc("@info", "The network asks for a quick check that a person is sending these messages. It opens in your browser; the messages that wait are sent once it is done.")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }
        QQC2.Button {
            text: L10n.i18nc("@action:button", "Open in Browser")
            icon.name: "globe"
            enabled: root.url !== ""
            Layout.alignment: Qt.AlignLeft
            onClicked: Qt.openUrlExternally(root.url)
        }
        QQC2.Label {
            text: L10n.i18nc("@info", "If the browser ends on a link that does not open Kourier, copy that link and paste it here:")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }
        QQC2.TextField {
            id: answer
            placeholderText: L10n.i18nc("@info:placeholder", "signalcaptcha://…")
            Accessible.name: L10n.i18nc("@info accessible", "Link from the browser")
            Layout.fillWidth: true
            onAccepted: {
                if (answer.text.trim().length > 0) {
                    App.submitChallenge(root.account, answer.text);
                    root.close();
                }
            }
        }
    }
}
