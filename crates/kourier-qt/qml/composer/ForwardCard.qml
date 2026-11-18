import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The card of a staged forward, beside the reply banner of the composer (forward line F6): "Forwarding N messages
// from <chat>", the Look (hidden when the target is on the source's account: the network's own forward), the X
// (Cancel Forward; Escape in the composer drops it too) and a progress line while the job runs. Enter in the text area
// sends the forward with the text as its comment (`Composer.send`). `staged` is `composerModel.forwardOf(conversation)`:
// `count`, `from`, `look`, `native`, `waiting`; empty when no forward stands. The words are the card's own, the rules
// are the core's.
RowLayout {
    id: root

    // The pane's composer (split view): the staged forwards are kept by conversation in Rust, and a change is told to
    // both panes' composers.
    required property ComposerController composerModel
    // The conversation the composer shows.
    property real conversation: 0
    // Bumped by `forwardsChanged`: `forwardOf` is a call, not a property.
    property int serial: 0
    readonly property var staged: {
        root.serial;
        return root.conversation !== 0 ? root.composerModel.forwardOf(root.conversation) : ({});
    }
    readonly property bool standing: root.staged.count !== undefined && root.staged.count > 0
    readonly property alias lookBox: look
    readonly property alias cancelButton: cancel
    readonly property color accent: Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor)

    objectName: "forwardCard"
    visible: root.standing
    spacing: Kirigami.Units.largeSpacing
    Accessible.role: Accessible.Grouping
    Accessible.name: L10n.i18nc("@info accessible", "Forward")

    Connections {
        target: root.composerModel
        function onForwardsChanged(): void {
            root.serial += 1;
        }
    }

    Rectangle {
        implicitWidth: 3
        Layout.fillHeight: true
        color: Kirigami.Theme.highlightColor
    }
    Kirigami.Icon {
        source: "mail-forward"
        implicitWidth: Kirigami.Units.iconSizes.smallMedium
        implicitHeight: Kirigami.Units.iconSizes.smallMedium
        color: root.accent
        isMask: true
        Layout.alignment: Qt.AlignVCenter
    }
    ColumnLayout {
        spacing: 0
        Layout.fillWidth: true
        QQC2.Label {
            id: title

            objectName: "forwardCardTitle"
            Layout.fillWidth: true
            text: root.standing ? L10n.i18ncp("@info, %1 how many messages are forwarded, %2 the chat they come from", "Forwarding %1 message from %2", "Forwarding %1 messages from %2", root.staged.count, root.staged.from) : ""
            font: Style.styled(Kirigami.Theme.defaultFont, {
                "weight": Font.DemiBold
            })
            color: root.accent
            textFormat: Text.PlainText
            elide: Text.ElideRight
            Accessible.name: title.text
        }
        // Under the title, so a narrow composer keeps the title's words.
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.topMargin: Kirigami.Units.smallSpacing
            visible: root.standing && root.staged.native !== true
            QQC2.Label {
                text: L10n.i18nc("@label:listbox the look of a message forwarded to another account", "Look:")
                textFormat: Text.PlainText
            }
            QQC2.ComboBox {
                id: look

                objectName: "forwardCardLook"
                enabled: root.staged.waiting !== true
                model: [L10n.i18nc("@item:inlistbox forward look", "Plain copy"), L10n.i18nc("@item:inlistbox forward look", "Forwarded header"), L10n.i18nc("@item:inlistbox forward look", "Header + quoted body"), L10n.i18nc("@item:inlistbox forward look", "As picture")]
                currentIndex: root.standing ? root.staged.look : 1
                Accessible.name: L10n.i18nc("@label:listbox accessible", "Look of the forwarded messages")
                onActivated: index => root.composerModel.setForwardLook(root.conversation, index)
            }
        }
        QQC2.Label {
            objectName: "forwardCardProgress"
            Layout.fillWidth: true
            visible: root.standing && root.staged.waiting === true
            text: L10n.i18nc("@info:status", "Sending the forward…")
            textFormat: Text.PlainText
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            elide: Text.ElideRight
        }
    }
    QQC2.ToolButton {
        id: cancel

        objectName: "forwardCardCancel"
        icon.name: "dialog-close"
        text: L10n.i18nc("@action:button", "Cancel Forward")
        display: QQC2.AbstractButton.IconOnly
        QQC2.ToolTip.visible: hovered
        QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "%1 (Esc)", text)
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        Accessible.name: cancel.text
        onClicked: root.composerModel.dropForward(root.conversation)
    }
}
