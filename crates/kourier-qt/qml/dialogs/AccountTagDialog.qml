import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// "Chat List Tag": a short name for an account that only the chat list shows, in place of the
// account's name (up to 8 characters; empty: the name again). It is kept on this computer and
// never sent to a network.
Kirigami.PromptDialog {
    id: root

    // The account (`App.accountActions`' id) and the tag it has now.
    property real account: 0
    property string current: ""

    title: L10n.i18nc("@title:dialog", "Chat List Tag")
    subtitle: L10n.i18nc("@info", "A short tag, up to 8 characters, shown on this account's chats in the list instead of its name. It stays on this computer. Leave it empty to show the name again.")
    preferredWidth: Kirigami.Units.gridUnit * 24
    maximumWidth: Kirigami.Units.gridUnit * 24
    standardButtons: Kirigami.Dialog.NoButton

    function openFor(account: real, current: string): void {
        root.account = account;
        root.current = current;
        field.text = current;
        root.open();
        field.forceActiveFocus();
        field.selectAll();
    }

    function save(): void {
        App.setAccountTag(root.account, field.text);
        root.close();
    }

    QQC2.TextField {
        id: field
        objectName: "account-tag-field"
        maximumLength: 8
        placeholderText: L10n.i18nc("@info:placeholder", "Tag")
        Accessible.name: L10n.i18nc("@label:textbox", "Chat list tag")
        onAccepted: root.save()
    }

    customFooterActions: [
        Kirigami.Action {
            objectName: "account-tag-save"
            text: L10n.i18nc("@action:button", "Save")
            icon.name: "dialog-ok-apply"
            onTriggered: root.save()
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]
}
