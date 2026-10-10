import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// "Clear the chat history?" and "Delete the chat?": both delete every message of the open chat
// for this account; deleting the chat also takes it out of the list (it returns with its next
// message). The question says what happens elsewhere: the account's other devices follow
// where the network has a message for that (`sync`), other people always keep their copies.
Kirigami.PromptDialog {
    id: root

    // Delete the chat (`true`), or only its messages.
    property bool remove: false
    // The account's other devices are told.
    property bool sync: App.timeline.deletesSync

    title: root.remove ? L10n.i18nc("@title:dialog", "Delete Chat?") : L10n.i18nc("@title:dialog", "Clear Chat History?")
    subtitle: {
        if (root.remove) {
            return root.sync ? L10n.i18nc("@info", "The chat and all of its messages are deleted here and on your other devices. The other people in the chat keep their copies, and the chat comes back with its next message.") : L10n.i18nc("@info", "The chat and all of its messages are deleted on this device only. Your other devices and the other people in the chat keep their copies, and the chat comes back with its next message.");
        }
        return root.sync ? L10n.i18nc("@info", "All messages of this chat are deleted here and on your other devices. The chat stays in your list, and the other people in it keep their copies.") : L10n.i18nc("@info", "All messages of this chat are deleted on this device only. The chat stays in your list; your other devices and the other people in it keep their copies.");
    }
    preferredWidth: Kirigami.Units.gridUnit * 26
    maximumWidth: Kirigami.Units.gridUnit * 26
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            text: root.remove ? L10n.i18nc("@action:button", "Delete Chat") : L10n.i18nc("@action:button", "Clear History")
            icon.name: root.remove ? "edit-delete" : "edit-clear-history"
            onTriggered: {
                if (root.remove) {
                    App.deleteConversation();
                } else {
                    App.clearConversation();
                }
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
