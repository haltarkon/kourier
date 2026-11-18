import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The progress of a forward job that downloads the files it needs first (forward line F6), in the window's footer
// like the export's strip: it belongs to the job, not to the chat that is open. Shown only while `App.forwardBusy`.
// Its Cancel is always there: a download that fails again keeps the job waiting until it is cancelled.
Rectangle {
    id: root

    readonly property alias label: forwardLabel
    readonly property alias cancelButton: cancel

    objectName: "forwardStrip"
    visible: App.forwardBusy
    color: Kirigami.Theme.backgroundColor
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    // No height while hidden (see ExportStrip).
    implicitHeight: visible ? content.implicitHeight + Kirigami.Units.smallSpacing * 2 : 0
    Accessible.role: Accessible.Grouping
    Accessible.name: L10n.i18nc("@info accessible", "Forward progress")

    Kirigami.Separator {
        anchors.top: parent.top
        width: parent.width
    }

    RowLayout {
        id: content

        anchors.fill: parent
        anchors.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.smallSpacing

        QQC2.Label {
            id: forwardLabel

            objectName: "forwardStripLabel"
            text: L10n.i18nc("@info:status, %1 files here, %2 files needed", "Forwarding: downloading %1 of %2 files…", App.forwardDone, App.forwardTotal)
            textFormat: Text.PlainText
            elide: Text.ElideRight
            Layout.fillWidth: true
            Accessible.name: forwardLabel.text
        }
        QQC2.Button {
            id: cancel

            objectName: "forwardStripCancel"
            icon.name: "dialog-cancel"
            text: L10n.i18nc("@action:button", "Cancel")
            Accessible.name: cancel.text
            onClicked: App.forwardCancel()
        }
    }
}
