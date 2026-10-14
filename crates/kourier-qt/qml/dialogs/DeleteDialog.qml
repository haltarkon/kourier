import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// "Delete this message?": for everyone only where the core allows it for this message (own
// messages inside the network's window, or as a group admin), else on this account only.
Kirigami.PromptDialog {
    id: root

    // The message (`TimelineActions` row description).
    property var row: null

    readonly property bool forEveryone: root.row !== null && root.row.actions.deleteForEveryone === true
    readonly property bool forMe: root.row !== null && root.row.actions.deleteForMe === true
    // A notice (a member joined, a timer changed) rather than a message.
    readonly property bool notice: root.row !== null && root.row.system === true

    title: root.notice ? L10n.i18nc("@title:dialog", "Delete Notice?") : L10n.i18nc("@title:dialog", "Delete Message?")
    subtitle: root.notice ? L10n.i18nc("@info", "The notice is removed from this conversation on your devices. Nothing changes for the other members.") : (root.forEveryone ? L10n.i18nc("@info", "Deleting it for everyone removes it from this conversation for all members. Deleting it for yourself removes it from your devices only.") : L10n.i18nc("@info", "The message is removed from your devices. The other members keep it."))
    // Wide enough for its buttons in one row; the text wraps.
    preferredWidth: Kirigami.Units.gridUnit * 26
    maximumWidth: Kirigami.Units.gridUnit * 26
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            visible: root.forEveryone
            enabled: visible
            text: L10n.i18nc("@action:button", "Delete for Everyone")
            icon.name: "edit-delete"
            onTriggered: {
                App.deleteMessage(root.row.messageId, true);
                root.close();
            }
        },
        Kirigami.Action {
            visible: root.forMe
            enabled: visible
            text: root.notice ? L10n.i18nc("@action:button", "Delete") : L10n.i18nc("@action:button", "Delete for Me")
            icon.name: "trash-empty"
            onTriggered: {
                App.deleteMessage(root.row.messageId, false);
                root.close();
            }
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]
}
