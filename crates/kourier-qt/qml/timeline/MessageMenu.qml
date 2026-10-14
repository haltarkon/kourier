pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The context menu of a message: a quick row of reactions, then what the message offers
// (viewing its picture, opening its file, playing its voice note, going to the message it
// quotes; reply, edit, copy, who reacted, its earlier versions, retry, deleting one of its
// attachments, delete). Entries the core does not offer for this message are not shown; a
// notice (a system row) offers only its removal from this account. Everything a pointer can
// do inside a row is here too, so the keyboard reaches it (the timeline is one tab stop).
QQC2.Menu {
    id: root

    // The message (`TimelineActions` row description).
    property var row: null
    // Where the menu was opened (the popups it leads to open there too).
    property Item anchor: null

    readonly property var offers: root.row ? root.row.actions : ({})
    // Our reactions on the message.
    readonly property var mine: {
        const out = [];
        if (root.row) {
            for (const chip of root.row.reactions) {
                if (chip.mine) {
                    out.push(chip.emoji);
                }
            }
        }
        return out;
    }
    readonly property bool hasReactions: root.row !== null && root.row.reactions.length > 0
    readonly property bool canDelete: root.offers.deleteForMe === true || root.offers.deleteForEveryone === true
    // What activating the row's picture, file or voice note does now, and whether it quotes a
    // message that is here.
    readonly property string mediaAction: root.row && root.row.mediaAction ? root.row.mediaAction : ""
    readonly property bool quoted: root.row !== null && root.row.quoted === true
    readonly property bool canDeleteAttachment: root.offers.deleteAttachment === true && root.row !== null && root.row.attachments !== undefined && root.row.attachments.length > 0

    // In the window's scene (also on Wayland, and in headless screenshots).
    popupType: QQC2.Popup.Item

    // An entry that takes no room while it does not apply. It is also disabled then: the
    // menu's keyboard navigation does not skip an item that is only invisible, so a hidden
    // entry could be reached and triggered with Down and Return.
    component Entry: QQC2.MenuItem {
        enabled: visible
        height: visible ? implicitHeight : 0
    }

    // The quick reactions; the last button opens the whole picker.
    Row {
        id: quickRow

        visible: root.offers.react === true
        height: visible ? implicitHeight : 0
        leftPadding: Kirigami.Units.smallSpacing
        rightPadding: Kirigami.Units.smallSpacing
        spacing: 0

        Repeater {
            model: Emoji.quick
            delegate: QQC2.ToolButton {
                id: quickButton

                required property string modelData

                readonly property bool chosen: root.mine.indexOf(quickButton.modelData) >= 0

                implicitWidth: Math.round(Kirigami.Units.gridUnit * 2)
                implicitHeight: implicitWidth
                text: quickButton.modelData
                font.family: "emoji"
                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.35
                checked: quickButton.chosen
                focusPolicy: Qt.NoFocus
                Accessible.name: quickButton.chosen ? L10n.i18nc("@action:button accessible", "Remove reaction %1", quickButton.modelData) : L10n.i18nc("@action:button accessible", "React with %1", quickButton.modelData)
                onClicked: {
                    App.react(root.row.messageId, quickButton.modelData, quickButton.chosen);
                    root.close();
                }
            }
        }
    }

    QQC2.MenuSeparator {
        visible: quickRow.visible
        height: visible ? implicitHeight : 0
    }

    // The row's own action first: it is what Enter does on the row.
    Entry {
        objectName: "menuMedia"
        visible: root.mediaAction !== ""
        icon.name: {
            switch (root.mediaAction) {
            case "download":
                return "download";
            case "play":
                return "media-playback-start";
            case "pause":
                return "media-playback-pause";
            case "open":
                return "document-open";
            default:
                return "view-preview";
            }
        }
        text: root.row !== null && root.row.mediaLabel ? root.row.mediaLabel : ""
        onTriggered: TimelineActions.activate(root.row.messageId)
    }
    Entry {
        objectName: "menuQuote"
        visible: root.quoted
        icon.name: "go-jump"
        text: L10n.i18nc("@action:inmenu", "Go to Quoted Message")
        onTriggered: TimelineActions.quote(root.row.messageId)
    }
    QQC2.MenuSeparator {
        visible: root.mediaAction !== "" || root.quoted
        height: visible ? implicitHeight : 0
    }

    Entry {
        visible: root.offers.react === true
        icon.name: "smiley-add"
        text: L10n.i18nc("@action:inmenu", "Add Reaction…")
        onTriggered: TimelineActions.picker(root.anchor, root.row)
    }
    Entry {
        visible: root.offers.reply === true
        icon.name: "mail-reply-sender"
        text: L10n.i18nc("@action:inmenu", "Reply")
        onTriggered: App.composer.replyTo(App.timeline.conversationId, root.row.messageId)
    }
    Entry {
        visible: root.offers.edit === true
        icon.name: "document-edit"
        text: L10n.i18nc("@action:inmenu", "Edit")
        onTriggered: App.composer.startEdit(App.timeline.conversationId, root.row.messageId)
    }
    Entry {
        visible: root.offers.copy === true
        icon.name: "edit-copy"
        text: L10n.i18nc("@action:inmenu", "Copy Text")
        onTriggered: App.copyText(root.row.copyText)
    }
    Entry {
        visible: root.hasReactions
        icon.name: "view-list-details"
        text: L10n.i18nc("@action:inmenu", "Show Reactions")
        onTriggered: TimelineActions.reactions(root.anchor, root.row)
    }
    Entry {
        visible: root.row !== null && root.row.edited === true
        icon.name: "view-history"
        text: L10n.i18nc("@action:inmenu", "Edit History")
        onTriggered: TimelineActions.history(root.row)
    }

    QQC2.MenuSeparator {
        // (Nothing to separate from in the menu of a notice.)
        visible: (root.offers.retry === true || root.canDelete || root.canDeleteAttachment) && !(root.row !== null && root.row.system === true)
        height: visible ? implicitHeight : 0
    }

    Entry {
        visible: root.offers.retry === true
        icon.name: "view-refresh"
        text: L10n.i18nc("@action:inmenu", "Send Again")
        onTriggered: App.retryMessage(root.row.messageId)
    }
    // Nobody has the message yet: it is simply taken back.
    Entry {
        visible: root.offers.takeBack === true && root.offers.deleteForMe === true
        icon.name: "trash-empty"
        text: root.row !== null && root.row.failed ? L10n.i18nc("@action:inmenu", "Delete") : L10n.i18nc("@action:inmenu", "Cancel Sending")
        onTriggered: App.cancelMessage(root.row.messageId)
    }
    // One attachment goes, the message stays. (The quiet icon of the deletes that are for
    // this account only, as on the question's own button; the red one marks what reaches
    // other people.)
    Entry {
        objectName: "menuDeleteAttachment"
        visible: root.canDeleteAttachment
        icon.name: "trash-empty"
        text: L10n.i18nc("@action:inmenu", "Delete Attachment…")
        onTriggered: TimelineActions.removeAttachment(root.row)
    }
    Entry {
        visible: root.canDelete && root.offers.takeBack !== true
        icon.name: "trash-empty"
        text: L10n.i18nc("@action:inmenu", "Delete…")
        onTriggered: TimelineActions.remove(root.row)
    }
}
