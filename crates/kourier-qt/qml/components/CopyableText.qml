pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// A plain text of a page (a name, a number, an ID) that can be taken away: the mouse selects it,
// Ctrl+C copies it, a right click offers Copy and Select All, and `button` puts a small copy
// button right after it. Centred, as the head of a page is. Plain text only: what comes from the
// network never becomes markup.
Item {
    id: root

    property string text
    property font font: Kirigami.Theme.defaultFont
    property color color: Kirigami.Theme.textColor
    // The small copy button after the text.
    property bool button: false

    readonly property real buttonSpace: root.button ? copyButton.width + Kirigami.Units.smallSpacing : 0

    implicitWidth: Math.ceil(metrics.advanceWidth) + 1 + root.buttonSpace
    implicitHeight: Math.max(edit.contentHeight, root.button ? copyButton.height : 0)

    function copyPicked(): void {
        App.copyShown(edit.selectedText !== "" ? edit.selectedText : root.text);
    }

    TextMetrics {
        id: metrics
        font: root.font
        text: root.text
    }

    TextEdit {
        id: edit

        // As wide as the text, or the whole line that is left: with the button right after the
        // text the two stay together in the middle.
        width: Math.min(Math.ceil(metrics.advanceWidth) + 1, Math.max(0, root.width - root.buttonSpace))
        x: Math.round((root.width - edit.width - root.buttonSpace) / 2)
        y: Math.round((root.height - edit.contentHeight) / 2)
        text: root.text
        font: root.font
        color: root.color
        textFormat: TextEdit.PlainText
        wrapMode: TextEdit.Wrap
        horizontalAlignment: TextEdit.AlignHCenter
        readOnly: true
        selectByMouse: true
        // The selection stays while the menu is open (it takes the keyboard).
        persistentSelection: true
        selectionColor: Kirigami.Theme.highlightColor
        selectedTextColor: Kirigami.Theme.highlightedTextColor
        activeFocusOnTab: false
        Accessible.name: root.text
        Accessible.role: Accessible.StaticText

        onActiveFocusChanged: {
            if (!activeFocus && !menu.visible) {
                edit.deselect();
            }
        }

        HoverHandler {
            cursorShape: Qt.IBeamCursor
        }
        TapHandler {
            acceptedButtons: Qt.RightButton
            onTapped: point => menu.popup(edit, point.position.x, point.position.y)
        }
    }

    QQC2.ToolButton {
        id: copyButton

        objectName: "copyButton"
        visible: root.button
        x: edit.x + edit.width + Kirigami.Units.smallSpacing
        y: Math.round((root.height - copyButton.height) / 2)
        width: Kirigami.Units.iconSizes.small + Kirigami.Units.smallSpacing * 2
        height: copyButton.width
        padding: Kirigami.Units.smallSpacing
        display: QQC2.AbstractButton.IconOnly
        icon.name: "edit-copy"
        icon.width: Kirigami.Units.iconSizes.small
        icon.height: Kirigami.Units.iconSizes.small
        text: L10n.i18nc("@action:button accessible, the small button after a number or an ID", "Copy")
        Accessible.name: copyButton.text
        QQC2.ToolTip.text: copyButton.text
        QQC2.ToolTip.visible: copyButton.hovered
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        onClicked: App.copyShown(root.text)
    }

    QQC2.Menu {
        id: menu

        // In the window's scene (also on Wayland, and in headless screenshots).
        popupType: QQC2.Popup.Item

        onClosed: {
            if (!edit.activeFocus) {
                edit.deselect();
            }
        }

        QQC2.MenuItem {
            objectName: "copyableCopy"
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:inmenu", "Copy")
            onTriggered: root.copyPicked()
        }
        QQC2.MenuItem {
            objectName: "copyableSelectAll"
            icon.name: "edit-select-all"
            text: L10n.i18nc("@action:inmenu", "Select All")
            onTriggered: edit.selectAll()
        }
    }
}
