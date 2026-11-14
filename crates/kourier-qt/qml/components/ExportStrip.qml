import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The progress of a running export (Export Selected), in the window's footer: it belongs to the
// export, not to the chat that is open, so a chat switch does not hide it and its Cancel always
// cancels THE export. Shown only while `App.exportBusy` (the plan is asked for, the documents
// are made or the archive is written). The label, the bar (indeterminate while the total is
// not known) and "Cancel Export".
Rectangle {
    id: root

    readonly property alias label: exportLabel
    readonly property alias bar: exportBar
    readonly property alias cancelButton: cancel

    objectName: "exportStrip"
    visible: App.exportBusy
    color: Kirigami.Theme.backgroundColor
    Kirigami.Theme.colorSet: Kirigami.Theme.Window
    Kirigami.Theme.inherit: false
    // No height while hidden: Kirigami's non-modal side panel subtracts the window footer's
    // height whether or not the footer is shown (`OverlayDrawer.qml`), so a hidden strip of its
    // full height shortened the panel in every wide window.
    implicitHeight: visible ? content.implicitHeight + Kirigami.Units.smallSpacing * 2 : 0
    Accessible.role: Accessible.Grouping
    Accessible.name: L10n.i18nc("@info accessible", "Export progress")

    // A thin line above it, like the composer's.
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
            id: exportLabel

            objectName: "exportStripLabel"
            text: App.exportTotal > 0 ? L10n.i18nc("@info:status, %1 how many percent of the export is written", "Exporting… %1%", Math.floor(App.exportDone * 100 / App.exportTotal)) : L10n.i18nc("@info:status", "Preparing the export…")
            textFormat: Text.PlainText
            elide: Text.ElideRight
            Accessible.name: exportLabel.text
        }
        QQC2.ProgressBar {
            id: exportBar

            objectName: "exportStripBar"
            from: 0
            to: Math.max(1, App.exportTotal)
            value: App.exportDone
            indeterminate: App.exportTotal <= 0
            Accessible.name: exportLabel.text
            Layout.fillWidth: true
        }
        QQC2.Button {
            id: cancel

            objectName: "exportStripCancel"
            icon.name: "dialog-cancel"
            text: L10n.i18nc("@action:button", "Cancel Export")
            Accessible.name: cancel.text
            onClicked: App.exportCancel()
        }
    }
}
