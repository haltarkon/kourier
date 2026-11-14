pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "Add Filter" / "Edit Filter": a saved RegExp filter (RegExp filters R2). Three monospace
// pattern fields (message text, sender's name, chat's name), each checked as you type with its
// own line under it, Match Case, Notify with "Also in muted chats", Highlight with eight
// swatches and "Custom…". Save stays disabled while the name is empty, no pattern is filled or a
// pattern is invalid; a refusal of the core shows under the input it names. `openFor(id)` edits
// a saved filter (0: a new one with the defaults); `openWith(...)` opens a new one with a
// prefill (Save as Filter). The viewmodel words every message; nothing here decides.
Kirigami.Dialog {
    id: root

    // The filter being edited (0: a new one).
    property real filterId: 0
    // The colour on the swatches (`#rrggbb`).
    property string colour: "#ffd60a"
    // The core's refusal: under which input, and the sentence.
    property string refusalField: ""
    property string refusalText: ""
    // The state a test reads.
    readonly property alias nameField: nameField
    readonly property alias textField: textInput.field
    readonly property alias senderField: senderInput.field
    readonly property alias chatField: chatInput.field
    readonly property bool valid: App.filters.canSave(nameField.text, textInput.text, senderInput.text, chatInput.text)
    readonly property alias saveAction: saveAction
    // For the window test: the "Custom…" button and the loader of the color chooser.
    readonly property alias customButton: customButton
    readonly property alias colourPicker: colourLoader
    // The lines under the inputs as they stand (tests read them).
    readonly property string textSaid: textInput.said
    readonly property string senderSaid: senderInput.said
    readonly property string chatSaid: chatInput.said

    title: root.filterId === 0 ? L10n.i18nc("@title:dialog", "Add Filter") : L10n.i18nc("@title:dialog", "Edit Filter")
    preferredWidth: Kirigami.Units.gridUnit * 30
    maximumWidth: Kirigami.Units.gridUnit * 30
    standardButtons: Kirigami.Dialog.NoButton
    padding: Kirigami.Units.largeSpacing

    function fill(values: var): void {
        nameField.text = values.name ?? "";
        textInput.text = values.text ?? "";
        senderInput.text = values.sender ?? "";
        chatInput.text = values.chat ?? "";
        matchCase.checked = values.matchCase ?? false;
        notify.checked = values.notify ?? false;
        notifyMuted.checked = values.notifyMuted ?? false;
        highlight.checked = values.highlight ?? true;
        root.colour = values.colour ?? App.filters.defaultColour();
        root.refusalField = "";
        root.refusalText = "";
    }

    // A new filter (`id` 0) or the edit of a saved one.
    function openFor(id: real): void {
        root.filterId = id;
        if (id === 0) {
            root.fill({});
        } else {
            const f = App.filters;
            root.fill({
                "name": f.value(id, "name"),
                "text": f.value(id, "text"),
                "sender": f.value(id, "sender"),
                "chat": f.value(id, "chat"),
                "matchCase": f.value(id, "matchCase"),
                "notify": f.value(id, "notify"),
                "notifyMuted": f.value(id, "notifyMuted"),
                "highlight": f.value(id, "highlight"),
                "colour": f.value(id, "colour")
            });
        }
        root.open();
        nameField.forceActiveFocus();
    }

    // A new filter with a prefill (`values`: the keys of `fill`).
    function openWith(values: var): void {
        root.filterId = 0;
        root.fill(values);
        root.open();
        nameField.forceActiveFocus();
    }

    function save(): void {
        if (!root.valid) {
            return;
        }
        root.refusalField = "";
        root.refusalText = "";
        if (!App.filters.save(root.filterId, nameField.text, textInput.text, senderInput.text, chatInput.text, matchCase.checked, notify.checked, notifyMuted.checked && notify.checked, highlight.checked, root.colour)) {
            root.refusalText = L10n.i18nc("@info:status", "The filter could not be saved.");
        }
    }

    // One pattern input with its own line under it.
    component PatternInput: ColumnLayout {
        id: input

        property string label
        property string key
        property alias text: edit.text
        property alias field: edit
        // The core's refusal that names this input (empty: none).
        property string refusal: ""
        signal submitted
        readonly property string problem: App.filters.checkPattern(edit.text)
        // The check as you type, else the core's refusal of this input.
        readonly property string said: input.problem !== "" ? input.problem : input.refusal

        Layout.fillWidth: true
        spacing: Kirigami.Units.smallSpacing

        QQC2.Label {
            text: input.label
            textFormat: Text.PlainText
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        }
        QQC2.TextField {
            id: edit
            objectName: "filter-" + input.key
            Layout.fillWidth: true
            font: Kirigami.Theme.fixedWidthFont
            placeholderText: L10n.i18nc("@info:placeholder", "Regular expression")
            Accessible.name: input.label
            onAccepted: input.submitted()
        }
        QQC2.Label {
            objectName: "filter-" + input.key + "-error"
            Layout.fillWidth: true
            visible: input.said !== ""
            text: input.said
            textFormat: Text.PlainText
            wrapMode: Text.WordWrap
            color: Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor)
            Accessible.role: Accessible.StaticText
            Accessible.name: input.said
        }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        ColumnLayout {
            Layout.fillWidth: true
            spacing: Kirigami.Units.smallSpacing

            QQC2.Label {
                text: L10n.i18nc("@label", "Name")
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            }
            QQC2.TextField {
                id: nameField
                objectName: "filter-name"
                Layout.fillWidth: true
                maximumLength: 64
                Accessible.name: L10n.i18nc("@label:textbox", "Filter name")
                onAccepted: root.save()
            }
            QQC2.Label {
                objectName: "filter-name-error"
                Layout.fillWidth: true
                visible: root.refusalField === "name"
                text: root.refusalText
                textFormat: Text.PlainText
                wrapMode: Text.WordWrap
                color: Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor)
            }
        }

        PatternInput {
            id: textInput
            label: L10n.i18nc("@label", "Message text")
            key: "text"
            refusal: root.refusalField === "text" ? root.refusalText : ""
            onSubmitted: root.save()
        }
        PatternInput {
            id: senderInput
            label: L10n.i18nc("@label", "Sender's name")
            key: "sender"
            refusal: root.refusalField === "sender" ? root.refusalText : ""
            onSubmitted: root.save()
        }
        PatternInput {
            id: chatInput
            label: L10n.i18nc("@label", "Chat's name")
            key: "chat"
            refusal: root.refusalField === "chat" ? root.refusalText : ""
            onSubmitted: root.save()
        }

        QQC2.Label {
            objectName: "filter-general-error"
            Layout.fillWidth: true
            visible: root.refusalText !== "" && root.refusalField === ""
            text: root.refusalText
            textFormat: Text.PlainText
            wrapMode: Text.WordWrap
            color: Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor)
        }

        QQC2.Label {
            objectName: "filter-help"
            Layout.fillWidth: true
            text: L10n.i18nc("@info", "A message matches when every filled field matches.")
            textFormat: Text.PlainText
            wrapMode: Text.WordWrap
            font: Kirigami.Theme.smallFont
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        }

        QQC2.CheckBox {
            id: matchCase
            objectName: "filter-matchcase"
            text: L10n.i18nc("@option:check", "Match Case")
        }
        QQC2.CheckBox {
            id: notify
            objectName: "filter-notify"
            text: L10n.i18nc("@option:check", "Notify")
        }
        QQC2.CheckBox {
            id: notifyMuted
            objectName: "filter-muted"
            Layout.leftMargin: Kirigami.Units.gridUnit
            enabled: notify.checked
            text: L10n.i18nc("@option:check", "Also in muted chats")
        }
        QQC2.CheckBox {
            id: highlight
            objectName: "filter-highlight"
            text: L10n.i18nc("@option:check", "Highlight")
        }

        // The colour row: eight swatches and Custom…
        RowLayout {
            Layout.leftMargin: Kirigami.Units.gridUnit
            enabled: highlight.checked
            spacing: Kirigami.Units.smallSpacing

            Repeater {
                model: App.filters.swatches()
                delegate: QQC2.AbstractButton {
                    id: swatch

                    required property int index
                    required property string modelData

                    objectName: "filter-swatch-" + swatch.index
                    checkable: true
                    checked: root.colour === swatch.modelData
                    implicitWidth: Kirigami.Units.gridUnit * 1.6
                    implicitHeight: implicitWidth
                    Accessible.name: L10n.i18nc("@info:whatsthis accessible, %1 a number from 1 to 8", "Color %1", swatch.index + 1)
                    onClicked: root.colour = swatch.modelData
                    contentItem: null
                    background: Rectangle {
                        radius: width / 2
                        color: swatch.modelData
                        border.width: swatch.checked ? 3 : 1
                        border.color: swatch.checked ? Kirigami.Theme.highlightColor : Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                        Rectangle {
                            anchors.fill: parent
                            anchors.margins: -(swatch.visualFocus ? 3 : 0)
                            radius: width / 2
                            color: "transparent"
                            border.width: swatch.visualFocus ? 2 : 0
                            border.color: Kirigami.Theme.focusColor
                        }
                    }
                }
            }
            QQC2.Button {
                id: customButton
                objectName: "filter-custom"
                text: L10n.i18nc("@action:button", "Custom…")
                icon.name: "color-management"
                onClicked: colourLoader.source = Qt.resolvedUrl("ColorPickDialog.qml")
            }
            Rectangle {
                // The colour in use when it is none of the swatches.
                visible: App.filters.swatches().indexOf(root.colour) < 0
                implicitWidth: Kirigami.Units.gridUnit * 1.6
                implicitHeight: implicitWidth
                radius: width / 2
                color: root.colour
                border.width: 3
                border.color: Kirigami.Theme.highlightColor
                Accessible.role: Accessible.Graphic
                Accessible.name: L10n.i18nc("@info:whatsthis accessible", "Custom color")
            }
        }

        Connections {
            target: App.filters
            enabled: root.visible
            function onSaved(): void {
                root.close();
            }
            function onRefused(field: string, text: string): void {
                root.refusalField = field;
                root.refusalText = text;
            }
        }

        // The system's color chooser is loaded on first use.
        Loader {
            id: colourLoader
            onLoaded: {
                const dialog = colourLoader.item;
                if (dialog) {
                    dialog.selectedColor = root.colour;
                    dialog.chosen.connect(c => {
                        root.colour = c;
                    });
                    // (A `ColorDialog` has no `closed`: it is gone when it is no longer visible.)
                    dialog.visibleChanged.connect(() => {
                        if (!dialog.visible) {
                            Qt.callLater(() => colourLoader.source = "");
                        }
                    });
                    dialog.open();
                }
            }
        }
    }

    customFooterActions: [
        Kirigami.Action {
            id: saveAction
            objectName: "filter-save"
            text: L10n.i18nc("@action:button", "Save")
            icon.name: "dialog-ok-apply"
            enabled: root.valid
            onTriggered: root.save()
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]
}
