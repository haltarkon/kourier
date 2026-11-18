import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The bar of the selection mode, in the composer's place while messages are being picked (the
// composer holds it: its page footer keeps its frame-exact height rules, and the text area, the
// draft and the typing state stay alive under it): how many messages are selected, "Copy as
// Text" (off at none, with the reason), "Export Selected…" (off at none, in a chat that may not
// be exported and while an export runs, with the reason) and "Cancel". The bar always keeps its
// counter and its three buttons (icons only when the row is too narrow for their words): the
// progress of a running export and the Cancel that stops it are the window's own strip
// (`ExportStrip.qml`, the footer), not the bar's. Cancel and Escape here always end the
// selection mode, never an export.
RowLayout {
    id: root
    required property TimelineModel chatModel

    readonly property int count: root.chatModel.selectedCount
    // The bar's own tab stops, for the keyboard (tests).
    readonly property alias copyButton: copy
    readonly property alias exportButton: exportSelected
    readonly property alias forwardButton: forwardSelected
    readonly property alias cancelButton: cancel

    spacing: Kirigami.Units.smallSpacing
    Accessible.role: Accessible.Grouping
    Accessible.name: L10n.i18nc("@info accessible", "Selected messages")

    // A row too narrow for the counter and the three buttons' words (a narrow window) shows the
    // buttons as icons, each with its text as a tooltip, so the counter keeps its words. The width
    // the words need is measured from the texts, never from the buttons' implicit widths: those
    // change with `display`, and the switch would feed itself.
    TextMetrics {
        id: words

        font: copy.font
        text: counter.text + copy.text + forwardSelected.text + exportSelected.text + cancel.text
    }
    readonly property bool compact: root.width < words.advanceWidth + 4 * (Kirigami.Units.iconSizes.small + 4 * Kirigami.Units.largeSpacing) + 4 * root.spacing

    QQC2.Label {
        id: counter

        objectName: "selectionCounter"
        Layout.fillWidth: true
        elide: Text.ElideRight
        text: L10n.i18ncp("@info:status, %1 how many messages are selected", "%1 selected", "%1 selected", root.count)
        textFormat: Text.PlainText
        Accessible.name: counter.text
    }

    // The button and, while it is off, the guard over it that says why: a disabled control gets no
    // hover, so its own tooltip would never show (`AccountsDrawer.qml` `entryBoxGuard`).
    Item {
        implicitWidth: copy.implicitWidth
        implicitHeight: copy.implicitHeight

        QQC2.Button {
            id: copy

            objectName: "selectionCopy"
            anchors.fill: parent
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:button", "Copy as Text")
            display: root.compact ? QQC2.AbstractButton.IconOnly : QQC2.AbstractButton.TextBesideIcon
            QQC2.ToolTip.visible: root.compact && copy.hovered
            QQC2.ToolTip.text: copy.text
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            enabled: root.count > 0
            Accessible.name: copy.text
            Accessible.description: copy.enabled ? "" : copy.toolTipText
            // The reason it is off (a control that is off for a reason the user can change says which).
            readonly property string toolTipText: L10n.i18nc("@info:tooltip", "Select at least one message to copy.")
            onClicked: root.chatModel.copySelected()
            Keys.onEscapePressed: root.chatModel.selectClear()
        }
        MouseArea {
            id: copyGuard

            objectName: "selectionCopyGuard"
            // (Tests read this.)
            readonly property bool tipShown: copyGuard.enabled && copyGuard.containsMouse
            anchors.fill: parent
            enabled: !copy.enabled
            hoverEnabled: true
            acceptedButtons: Qt.NoButton
            QQC2.ToolTip.visible: copyGuard.tipShown
            QQC2.ToolTip.text: copy.toolTipText
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
    }

    // Forward…: the same guard as Copy as Text; the core's verdict per message is in the dialog it opens.
    Item {
        implicitWidth: forwardSelected.implicitWidth
        implicitHeight: forwardSelected.implicitHeight

        QQC2.Button {
            id: forwardSelected

            objectName: "selectionForward"
            anchors.fill: parent
            icon.name: "mail-forward"
            text: L10n.i18nc("@action:button", "Forward…")
            display: root.compact ? QQC2.AbstractButton.IconOnly : QQC2.AbstractButton.TextBesideIcon
            QQC2.ToolTip.visible: root.compact && forwardSelected.hovered
            QQC2.ToolTip.text: forwardSelected.text
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            enabled: root.count > 0
            Accessible.name: forwardSelected.text
            Accessible.description: forwardSelected.enabled ? "" : forwardSelected.toolTipText
            readonly property string toolTipText: L10n.i18nc("@info:tooltip", "Select at least one message to forward.")
            onClicked: App.forwardOpen(root.chatModel.selectedIds())
            Keys.onEscapePressed: root.chatModel.selectClear()
        }
        MouseArea {
            id: forwardGuard

            objectName: "selectionForwardGuard"
            readonly property bool tipShown: forwardGuard.enabled && forwardGuard.containsMouse
            anchors.fill: parent
            enabled: !forwardSelected.enabled
            hoverEnabled: true
            acceptedButtons: Qt.NoButton
            QQC2.ToolTip.visible: forwardGuard.tipShown
            QQC2.ToolTip.text: forwardSelected.toolTipText
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
    }

    // Export Selected…: the same guard as Copy as Text, with the reason of the case.
    Item {
        implicitWidth: exportSelected.implicitWidth
        implicitHeight: exportSelected.implicitHeight

        QQC2.Button {
            id: exportSelected

            objectName: "selectionExport"
            anchors.fill: parent
            icon.name: "document-export"
            text: L10n.i18nc("@action:button", "Export Selected…")
            display: root.compact ? QQC2.AbstractButton.IconOnly : QQC2.AbstractButton.TextBesideIcon
            QQC2.ToolTip.visible: root.compact && exportSelected.hovered
            QQC2.ToolTip.text: exportSelected.text
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            enabled:root.count > 0 && root.chatModel.exportOff === "" && !App.exportBusy
            Accessible.name: exportSelected.text
            Accessible.description: exportSelected.enabled ? "" : exportSelected.toolTipText
            // Why it is off: nothing selected, or the chat's refusal (text from the view model,
            // shown through the guard's plain-text tooltip).
            readonly property string toolTipText: root.count <= 0 ? L10n.i18nc("@info:tooltip", "Select at least one message to export.") : App.exportBusy ? L10n.i18nc("@info:tooltip", "An export is running.") : Style.plain(root.chatModel.exportOff)
            onClicked: root.chatModel.exportSelected()
            Keys.onEscapePressed: root.chatModel.selectClear()
        }
        MouseArea {
            id: exportGuard

            objectName: "selectionExportGuard"
            readonly property bool tipShown: exportGuard.enabled && exportGuard.containsMouse
            anchors.fill: parent
            enabled: !exportSelected.enabled
            hoverEnabled: true
            acceptedButtons: Qt.NoButton
            QQC2.ToolTip.visible: exportGuard.tipShown
            QQC2.ToolTip.text: exportSelected.toolTipText
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
    }

    QQC2.Button {
        id: cancel

        objectName: "selectionCancel"
        icon.name: "dialog-cancel"
        text: L10n.i18nc("@action:button", "Cancel")
        display: root.compact ? QQC2.AbstractButton.IconOnly : QQC2.AbstractButton.TextBesideIcon
        QQC2.ToolTip.visible: root.compact && cancel.hovered
        QQC2.ToolTip.text: cancel.text
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        Accessible.name: cancel.text
        // Always ends the mode; a running export has its own Cancel in the window's strip.
        onClicked: root.chatModel.selectClear()
        Keys.onEscapePressed: root.chatModel.selectClear()
    }

    Keys.onEscapePressed: root.chatModel.selectClear()
}
