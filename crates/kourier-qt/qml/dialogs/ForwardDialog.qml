pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "Forward…" (forward line F6; architecture §8.3): the chats a message or a selection goes to. The plan comes from
// Rust: `App.forwardOpen(ids)` fills `App.forwardTargets` (the shown accounts' chats the user can write in, the
// search runs in Rust) and `App.forwardDialogReady(json)` hands the view (`ForwardDialogView::to_json`: the title,
// what is left out, the refusal of each ticked chat, the changes a conversion makes, the limits, whether the Look
// applies and whether Forward may go). Every sentence is Rust's; this file only shows them. Ticking a chat asks for a
// new plan (`App.forwardTick`) and Forward stays off until the answer to the newest ticks is in. [Forward] starts
// the job (`App.forwardSend`), Cancel, Escape and a click outside end the dialog (`App.forwardAbandon`). Loaded by URL
// on first use (`MainWindow.openForwardDialog`).
Kirigami.Dialog {
    id: root

    // The newest view of the plan (`ForwardDialogView::to_json`).
    property var view: ({
            "title": "",
            "count": 0,
            "leftOut": [],
            "targets": [],
            "changes": [],
            "limits": [],
            "lookShown": false,
            "canForward": false,
            "downloads": 0,
            "lost": 0
        })
    // The ticked chats (conversation ids, in the order they were ticked).
    property var ticked: []
    // A tick was sent and its plan is not in yet: Forward stays off.
    property bool planPending: false
    // Forward started a job (a close after it is no abandon).
    property bool sent: false
    // How many times Forward handed a job to Rust (read by tests).
    property int sends: 0
    // The selection bar opened the dialog: a forward that goes ends the selection.
    property bool fromSelection: false
    readonly property alias list: list
    readonly property alias lookBox: lookBox
    readonly property alias commentField: comment
    readonly property bool canGo: root.ticked.length > 0 && root.view.canForward === true && !root.planPending

    signal forwarded

    function isTicked(conversation: real): bool {
        return root.ticked.indexOf(conversation) >= 0;
    }

    function refusalOf(conversation: real): string {
        for (const t of root.view.targets) {
            if (t.id === conversation) {
                return t.refusal ?? "";
            }
        }
        return "";
    }

    // Ticks or unticks `conversation` and asks for the plan of the new ticks.
    function toggle(conversation: real): void {
        const at = root.ticked.indexOf(conversation);
        const next = root.ticked.slice();
        if (at >= 0) {
            next.splice(at, 1);
        } else {
            next.push(conversation);
        }
        root.ticked = next;
        root.planPending = true;
        App.forwardTick(next.map(c => String(c)));
    }

    function show(json: string): void {
        root.view = JSON.parse(json);
        root.ticked = [];
        root.planPending = false;
        root.sent = false;
        search.text = "";
        comment.text = "";
        lookBox.currentIndex = Math.max(0, Math.min(3, App.settings.forwardLook));
        list.currentIndex = -1;
        root.open();
    }

    // A newer plan: replaces the lines, Forward may go again.
    function update(json: string): void {
        root.view = JSON.parse(json);
        root.planPending = false;
    }

    function go(): void {
        if (!root.canGo) {
            return;
        }
        if (App.forwardSend(lookBox.currentIndex, comment.text)) {
            root.sent = true;
            root.sends += 1;
            root.close();
            root.forwarded();
        }
    }

    objectName: "forwardDialog"
    title: root.view.title
    // The title wraps (as the share picker's): a long count in a narrow window never cuts.
    preferredWidth: Kirigami.Units.gridUnit * 26
    padding: Kirigami.Units.largeSpacing
    closePolicy: QQC2.Popup.CloseOnEscape
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            objectName: "forwardGo"
            text: L10n.i18nc("@action:button", "Forward")
            icon.name: "mail-forward"
            enabled: root.canGo
            onTriggered: root.go()
        },
        Kirigami.Action {
            objectName: "forwardCancel"
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]
    onClosed: {
        if (!root.sent) {
            App.forwardAbandon();
        }
    }
    onOpened: search.forceActiveFocus()

    ColumnLayout {
        spacing: Kirigami.Units.smallSpacing

        // What is left out of the selection and why (one line each; Rust's words).
        Repeater {
            model: root.view.leftOut
            QQC2.Label {
                required property string modelData

                objectName: "forwardLeftOut"
                Layout.fillWidth: true
                text: modelData
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            }
        }

        Kirigami.SearchField {
            id: search

            objectName: "forwardSearch"
            Layout.fillWidth: true
            placeholderText: L10n.i18nc("@info:placeholder the search field of the chats a message can be forwarded to", "Search chats…")
            autoAccept: false
            focusSequences: []
            Accessible.name: search.placeholderText
            onTextChanged: App.forwardTargets.filter = search.text
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

            objectName: "forwardList"
            Layout.fillWidth: true
            Layout.preferredHeight: Kirigami.Units.gridUnit * 14
            clip: true
            model: App.forwardTargets
            currentIndex: -1
            boundsBehavior: Flickable.StopAtBounds
            activeFocusOnTab: true
            keyNavigationEnabled: true
            Accessible.role: Accessible.List
            Accessible.name: L10n.i18nc("@info:accessible the chats a message can be forwarded to", "Chats")
            QQC2.ScrollBar.vertical: QQC2.ScrollBar {}
            Keys.onUpPressed: event => {
                if (list.currentIndex <= 0) {
                    search.forceActiveFocus();
                    event.accepted = true;
                } else {
                    event.accepted = false;
                }
            }
            // Space ticks the current row (a held key acts once); Return is Forward.
            Keys.onSpacePressed: event => {
                if (!event.isAutoRepeat && list.currentIndex >= 0) {
                    root.toggle(App.forwardTargets.conversationAt(list.currentIndex));
                }
                event.accepted = true;
            }
            function activateKey(event: var): void {
                if (!event.isAutoRepeat) {
                    root.go();
                }
                event.accepted = true;
            }
            Keys.onReturnPressed: event => list.activateKey(event)
            Keys.onEnterPressed: event => list.activateKey(event)
            delegate: QQC2.ItemDelegate {
                id: row

                objectName: "forwardRow"
                required property int index
                required property real conversation
                required property string title
                required property string avatarUrl
                required property bool isSelf
                required property string accountTag
                required property string tint
                required property bool sameAccount
                readonly property bool ticked: root.isTicked(row.conversation)
                readonly property string refusal: root.refusalOf(row.conversation)

                width: list.width
                focusPolicy: Qt.NoFocus
                highlighted: ListView.isCurrentItem
                Accessible.role: Accessible.CheckBox
                Accessible.name: Style.unmarked(row.title)
                Accessible.checked: row.ticked
                Accessible.description: row.refusal
                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing
                    QQC2.CheckBox {
                        objectName: "forwardCheck"
                        checked: row.ticked
                        focusPolicy: Qt.NoFocus
                        // A click on the box is a click on the row.
                        onToggled: root.toggle(row.conversation)
                        Accessible.ignored: true
                    }
                    Avatar {
                        name: row.title
                        source: row.avatarUrl
                        selfChat: row.isSelf
                        implicitWidth: Kirigami.Units.gridUnit * 2
                        implicitHeight: Kirigami.Units.gridUnit * 2
                    }
                    ColumnLayout {
                        spacing: 0
                        Layout.fillWidth: true
                        QQC2.Label {
                            objectName: "forwardRowTitle"
                            Layout.fillWidth: true
                            text: row.title
                            color: row.highlighted ? Kirigami.Theme.highlightedTextColor : Kirigami.Theme.textColor
                            elide: Text.ElideRight
                            textFormat: Text.PlainText
                        }
                        // Why this chat will not get it (only a ticked chat has a plan).
                        QQC2.Label {
                            objectName: "forwardRowRefusal"
                            Layout.fillWidth: true
                            visible: row.ticked && row.refusal !== ""
                            text: row.refusal
                            color: Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor)
                            wrapMode: Text.Wrap
                            textFormat: Text.PlainText
                        }
                    }
                    // The account's tag, where several accounts are shown.
                    QQC2.Label {
                        objectName: "forwardRowTag"
                        visible: row.accountTag !== ""
                        text: row.accountTag
                        textFormat: Text.PlainText
                        font.pointSize: Kirigami.Theme.smallFont.pointSize
                        color: Style.readable(row.tint, Kirigami.Theme.backgroundColor)
                    }
                }
                onClicked: {
                    list.currentIndex = row.index;
                    root.toggle(row.conversation);
                }
            }

            QQC2.Label {
                objectName: "forwardEmpty"
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                visible: list.count === 0
                text: search.text === "" ? L10n.i18nc("@info:placeholder no chat can take a forwarded message", "No chat can take a message.") : L10n.i18nc("@info:placeholder nothing in the chats a message can be forwarded to matches the search", "No chats match.")
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }
        }

        // The Look: only when a chat of another account is ticked (the network's own forward ignores it).
        RowLayout {
            objectName: "forwardLookRow"
            visible: root.view.lookShown === true
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            QQC2.Label {
                text: L10n.i18nc("@label:listbox the look of a message forwarded to another account", "Look:")
                textFormat: Text.PlainText
            }
            QQC2.ComboBox {
                id: lookBox

                objectName: "forwardLook"
                Layout.fillWidth: true
                model: [L10n.i18nc("@item:inlistbox forward look", "Plain copy"), L10n.i18nc("@item:inlistbox forward look", "Forwarded header"), L10n.i18nc("@item:inlistbox forward look", "Header + quoted body"), L10n.i18nc("@item:inlistbox forward look", "As picture")]
                Accessible.name: L10n.i18nc("@label:listbox accessible", "Look of the forwarded messages")
            }
        }
        QQC2.Label {
            objectName: "forwardLookNote"
            Layout.fillWidth: true
            visible: root.view.lookShown === true
            text: L10n.i18nc("@info", "Chats of the same account get the network's own forward")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        }

        // What the conversion changes, and the limits of the networks (Rust's words).
        Repeater {
            model: root.view.changes
            QQC2.Label {
                required property string modelData

                objectName: "forwardChange"
                Layout.fillWidth: true
                text: modelData
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }
        }
        Repeater {
            model: root.view.limits
            QQC2.Label {
                required property string modelData

                objectName: "forwardLimit"
                Layout.fillWidth: true
                text: modelData
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
                color: Style.readable(Kirigami.Theme.neutralTextColor, Kirigami.Theme.backgroundColor)
            }
        }

        QQC2.TextField {
            id: comment

            objectName: "forwardComment"
            Layout.fillWidth: true
            placeholderText: L10n.i18nc("@info:placeholder", "Add a comment…")
            Accessible.name: comment.placeholderText
            onAccepted: root.go()
        }
    }
}
