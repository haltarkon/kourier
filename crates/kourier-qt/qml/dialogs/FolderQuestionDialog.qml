pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// A folder was given to the composer (dropped, pasted or picked). A folder cannot travel as a
// message, so Kourier asks, every time, and once for all the folders given together: add the
// files inside, or send them as one .zip per folder. The dialog says how many files and how
// much data that is and what a walk through the folder leaves out (hidden files, links).
//
// Each answer is a button with one line under it that says what it does. The archive is not
// offered where the files add up to more than the chat takes as one file, or where the chat
// takes no such files: the line under the button says so, and the other answer stands.
// Adding very many files is asked about once more (`confirming`). Escape, the close button and
// "Cancel" take the folders out of the list again.
//
// What it says comes from `App.composer.folderQuestion`; the dialog only shows it. Folder
// names are text from outside (a name can be anything): every label is plain text.
Kirigami.Dialog {
    id: root

    readonly property var question: App.composer.folderQuestion
    readonly property bool asking: App.composer.folderAsking
    // The second question: this many files, really?
    readonly property bool confirming: root.asking && root.question.confirming === true
    readonly property bool canZip: root.asking && root.question.canZip === true
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)

    function say(key: string): string {
        const value = root.question[key];
        return value === undefined ? "" : value;
    }

    // `add`, `zip`, `back` or `cancel`.
    function answer(what: string): void {
        App.composer.answerFolders(what);
    }

    title: root.say("title")
    preferredWidth: Kirigami.Units.gridUnit * 24
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Back")
            icon.name: "go-previous"
            visible: root.confirming
            enabled: visible
            onTriggered: root.answer("back")
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.answer("cancel")
        }
    ]

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        // What the folders hold.
        QQC2.Label {
            objectName: "folderText"
            text: root.confirming ? root.say("confirm") : root.say("text")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }
        // What a walk leaves out.
        QQC2.Label {
            objectName: "folderSkipped"
            visible: text !== "" && !root.confirming
            text: root.say("skipped")
            font: Kirigami.Theme.smallFont
            color: root.quiet
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }

        // The first answer (in the second question: its "yes").
        ColumnLayout {
            spacing: Kirigami.Units.smallSpacing / 2
            Layout.fillWidth: true

            QQC2.Button {
                id: addButton
                objectName: "folderAdd"
                icon.name: "list-add"
                text: root.confirming ? root.say("confirmLabel") : root.say("addLabel")
                Accessible.description: root.say("addHint")
                Layout.fillWidth: true
                onClicked: root.answer("add")
            }
            QQC2.Label {
                visible: !root.confirming
                text: root.say("addHint")
                font: Kirigami.Theme.smallFont
                color: root.quiet
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
            }
        }

        // The second answer.
        ColumnLayout {
            spacing: Kirigami.Units.smallSpacing / 2
            Layout.fillWidth: true

            QQC2.Button {
                objectName: "folderZip"
                icon.name: "application-zip"
                text: root.say("zipLabel")
                enabled: root.canZip
                Accessible.description: root.say("zipHint")
                Layout.fillWidth: true
                onClicked: root.answer("zip")
            }
            QQC2.Label {
                objectName: "folderZipHint"
                text: root.say("zipHint")
                font: Kirigami.Theme.smallFont
                // Why it is not possible is said in the color of a note, not of an error:
                // nothing went wrong, and the other answer is right above.
                color: root.canZip ? root.quiet : Style.readable(Kirigami.Theme.neutralTextColor, Kirigami.Theme.backgroundColor)
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
            }
        }
    }

    onOpened: addButton.forceActiveFocus()
}
