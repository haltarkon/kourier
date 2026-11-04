pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigami.dialogs as KDialogs
import app.kourier

// "Share Contact…" of a person's page (people pages P5c; architecture §8.3): the chats of the page's account
// a contact card may go to (`App.shareTargets`, filled by `App.prepareShare()`: the account's chats the user can
// write in, Note to Self too, archived ones too, in the list's order; the search runs in Rust). His answer
// "Pick, then Send" (SC2): a click or a double click on a row only CHOOSES it; only "Send", or Return on the
// chosen row, sends. Rust holds whose card it is and the core builds it: this dialog names only the chat.
// Typing in the search drops the choice (a row that moved under the pointer is never sent to).
Kirigami.Dialog {
    id: root

    // The page's name of the person whose card is sent (for the title only; the card carries their profile name).
    property string personName: ""
    // What the card holds ("The card holds <profile name> and <number>."): the core's card worded by the viewmodel
    // (`App.person.shareCard`; SD9). Empty: no line. The dialog never builds it.
    property string cardLine: ""
    // A card went to the chat called `title` (the page shows the toast).
    signal sent(string title)

    // How many times "Send" handed a card to Rust (read by `chat_ui`).
    property int sends: 0

    // A row is chosen: "Send" can send.
    readonly property bool chosen: list.currentIndex >= 0 && list.currentIndex < list.count

    // Chooses the row at `index` (what a click does) and puts the keyboard on the list; nothing is sent.
    function choose(index: int): void {
        if (index < 0 || index >= list.count) {
            return;
        }
        list.currentIndex = index;
        list.forceActiveFocus();
    }

    // Sends the card to the chosen chat (only one the picker lists) and closes.
    function send(): void {
        if (!root.chosen) {
            return;
        }
        const conversation = App.shareTargets.conversationAt(list.currentIndex);
        if (conversation === 0 || !App.shareTargets.isListed(conversation)) {
            return;
        }
        // (Read before the send: the chat's name for the toast.)
        const title = App.shareTargets.titleOf(conversation);
        root.sends += 1;
        const queued = App.shareContact(conversation);
        root.close();
        if (queued) {
            root.sent(title);
        }
    }

    title: L10n.i18nc("@title:dialog, %1 the person whose contact card is sent", "Send %1's Contact Card", root.personName)
    // The title wraps (SD9): Kirigami's own header is one line that elides, and in a narrow window or at a large font
    // it would cut the NAME of the person whose number is about to go out (in Ukrainian the name ends the sentence).
    // The header's close button goes with it: "Cancel" and Escape close the picker.
    header: KDialogs.DialogHeader {
        dialog: root
        // (No width of its own: the dialog's width is the widest of its parts, and a long name in one line would make
        // the dialog wider than its list; the label takes the dialog's width and wraps.)
        implicitWidth: 0
        contentItem: Kirigami.Heading {
            objectName: "shareTitle"
            text: root.title
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
        }
    }
    preferredWidth: Kirigami.Units.gridUnit * 24
    padding: Kirigami.Units.largeSpacing
    // Escape closes the picker from every stop (the search, the list, "Send", "Cancel": the popup's own Escape), but not
    // a press outside it (SD7). The search field takes Escape first while it has text.
    closePolicy: QQC2.Popup.CloseOnEscape
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button send a contact card to the chosen chat", "Send")
            icon.name: "document-send"
            enabled: root.chosen
            onTriggered: root.send()
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]

    ColumnLayout {
        id: content
        spacing: Kirigami.Units.largeSpacing

        // One quiet line under the title: exactly what the card carries (SD9), so the user sees whose number goes out.
        QQC2.Label {
            id: cardLabel
            objectName: "shareCard"
            Layout.fillWidth: true
            // (No width of its own: like the title it wraps in the dialog's width and its height follows the text.)
            visible: root.cardLine !== ""
            text: root.cardLine
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Accessible.role: Accessible.StaticText
            Accessible.name: cardLabel.text
        }

        Kirigami.SearchField {
            id: search
            objectName: "shareSearch"
            Layout.fillWidth: true
            placeholderText: L10n.i18nc("@info:placeholder the search field of the chats a contact card can go to", "Search chats…")
            autoAccept: false
            // Ctrl+F is the window's search of the chats.
            focusSequences: []
            Accessible.name: search.placeholderText
            onTextChanged: {
                list.currentIndex = -1;
                App.shareTargets.filter = search.text;
            }
            // Escape empties a search that has text (the first Escape; an empty one lets the popup close, the second).
            // The popup would take Escape as a shortcut: the field says it wants the key while it has text.
            Keys.onShortcutOverride: event => {
                event.accepted = event.key === Qt.Key_Escape && search.text !== "";
            }
            Keys.onEscapePressed: event => {
                if (search.text !== "") {
                    search.text = "";
                    event.accepted = true;
                } else {
                    event.accepted = false;
                }
            }
            Keys.onDownPressed: event => {
                if (list.count > 0) {
                    list.currentIndex = Math.max(0, list.currentIndex);
                    list.forceActiveFocus();
                }
                event.accepted = true;
            }
        }

        ListView {
            id: list
            objectName: "shareList"
            Layout.fillWidth: true
            // A fixed height: the dialog never scrolls around the list.
            Layout.preferredHeight: Kirigami.Units.gridUnit * 16
            clip: true
            model: App.shareTargets
            currentIndex: -1
            // (A view that gets rows makes the first one current: no row is chosen until the user chooses.)
            onModelChanged: list.currentIndex = -1
            boundsBehavior: Flickable.StopAtBounds
            // The list is the tab stop; the rows are not.
            activeFocusOnTab: true
            keyNavigationEnabled: true
            Accessible.role: Accessible.List
            Accessible.name: L10n.i18nc("@info:accessible the chats a contact card can go to", "Chats")
            QQC2.ScrollBar.vertical: QQC2.ScrollBar {}
            // Up on the first row goes back to the search field.
            Keys.onUpPressed: event => {
                if (list.currentIndex <= 0) {
                    search.forceActiveFocus();
                    event.accepted = true;
                } else {
                    event.accepted = false;
                }
            }
            // Return on the chosen row is a deliberate key: it sends (SC2). A held key acts once (SE1): the repeats are
            // taken and do nothing. The parameter is untyped so a test can hand over a stand-in for the key event.
            function activateKey(event: var): void {
                if (event.isAutoRepeat) {
                    event.accepted = true;
                    return;
                }
                root.send();
                event.accepted = true;
            }
            Keys.onReturnPressed: event => list.activateKey(event)
            Keys.onEnterPressed: event => list.activateKey(event)
            delegate: QQC2.ItemDelegate {
                id: row
                objectName: "shareRow"
                required property int index
                required property real conversation
                required property string title
                required property string avatarUrl
                required property bool isSelf
                width: list.width
                focusPolicy: Qt.NoFocus
                highlighted: ListView.isCurrentItem
                // The chosen row is the one cue of WHERE the card goes (SD8): a screen reader is told which row is chosen,
                // as in the chat list, and its name takes the theme's text colour for a selection (the desktop style
                // fills the row with the selection colour but leaves a custom label in the normal text colour: white on
                // yellow in the high-contrast dark scheme).
                Accessible.role: Accessible.ListItem
                Accessible.name: Style.unmarked(row.title)
                Accessible.selected: row.highlighted
                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing
                    Avatar {
                        name: row.title
                        source: row.avatarUrl
                        selfChat: row.isSelf
                        implicitWidth: Kirigami.Units.gridUnit * 2
                        implicitHeight: Kirigami.Units.gridUnit * 2
                    }
                    QQC2.Label {
                        objectName: "shareRowTitle"
                        Layout.fillWidth: true
                        text: row.title
                        color: row.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
                        elide: Text.ElideRight
                        textFormat: Text.PlainText
                    }
                }
                // A click only chooses the row (SC2), and so does a double click: never a send.
                onClicked: root.choose(row.index)
                onDoubleClicked: root.choose(row.index)
            }

            // The account has no chat a card can go to (SD11: not an empty box with no word).
            QQC2.Label {
                objectName: "shareEmpty"
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                visible: list.count === 0 && search.text === ""
                text: L10n.i18nc("@info:placeholder no chat of the account can take a contact card", "No chat of this account can take a message.")
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }

            // Nothing matches what was typed.
            QQC2.Label {
                objectName: "shareNone"
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                visible: list.count === 0 && search.text !== ""
                text: L10n.i18nc("@info:placeholder nothing in the chats a contact card can go to matches the search", "No chats match.")
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }
        }
    }

    onOpened: search.forceActiveFocus()
}
