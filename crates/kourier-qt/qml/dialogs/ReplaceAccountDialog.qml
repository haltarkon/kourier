pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "Remove and Link Again?": the question before an account is removed from this computer
// and linked again so that the phone can transfer its message history ("Transfer History
// from Phone…" in the settings, "Try Again…" on the banner of a failed transfer). It says
// in plain words what is removed, what is lost and what happens when linking is cancelled
// afterwards (`question`, from the viewmodel).
//
// The default is to cancel: the keyboard starts on "Cancel", Enter confirms nothing, and
// Escape, the close button and a click outside all cancel. The confirming button is named
// by what it does and acts once.
//
// The question opens in the middle of the window, which can be right under the pointer
// that asked for it: the second click of a double click on "Transfer History from Phone…"
// would land on a button of the question before a word of it was read. So the confirming
// button does nothing until the question has been open for a moment (`armed`): a press
// before that is ignored and the question stays. (The button is not shown disabled for
// that second: it would change its look under the eyes of someone reading, and with the
// software renderer a button that repaints over a Kirigami dialog leaves a sliver of the
// page behind it.)
//
// The same dialog asks before "Remove Account…" and "Unlink…" of an account (link feedback
// L4): then the question carries its `action` ("remove", "unlink") and the answer goes to
// `App.confirmAccountAction` / `App.cancelAccountAction`.
//
// The footer is the dialog's own: two buttons side by side at the trailing edge, as KDE's
// dialogs have them, and one above the other when the dialog is too narrow for both (a
// long translation, a large font, a narrow window).
Kirigami.PromptDialog {
    id: root

    // What to ask (`App.replaceAsked`, `App.accountActionAsked`): `account`, `title`,
    // `about`, `paragraphs`, `confirm`, `cancel`, and for an account action its `action`.
    property var question: ({})
    // The question was confirmed: a second activation does nothing.
    property bool answered: false
    // The question has been open long enough to have been seen: the confirming button
    // acts. Longer than a double click, however slowly the user has set that up.
    property bool armed: false
    // "remove" or "unlink" for an account action; empty for "Remove and Link Again?".
    readonly property string action: root.question.action ?? ""
    readonly property int armDelay: Math.max(1000, 2 * Application.styleHints.mouseDoubleClickInterval)

    // The two buttons (tests press them).
    readonly property Item confirmButton: removeButton
    readonly property Item cancelButton: keepButton
    // The buttons stand one above the other.
    readonly property bool stacked: footerBar.stacked

    function confirm(): void {
        if (root.answered || !root.armed) {
            return;
        }
        root.answered = true;
        if (root.action === "") {
            App.confirmReplace(root.question.account ?? 0);
        } else {
            App.confirmAccountAction(root.question.account ?? 0, root.action);
        }
        root.close();
    }

    title: root.question.title ?? ""
    dialogType: Kirigami.PromptDialog.Warning
    preferredWidth: Kirigami.Units.gridUnit * 28
    maximumWidth: Kirigami.Units.gridUnit * 28
    standardButtons: Kirigami.Dialog.NoButton

    footer: QQC2.Control {
        id: footerBar

        // What the two buttons need side by side.
        readonly property real sideBySide: removeButton.implicitWidth + keepButton.implicitWidth + buttons.columnSpacing
        readonly property bool stacked: footerBar.availableWidth < footerBar.sideBySide

        padding: Kirigami.Units.largeSpacing
        // (Not the width of the row of buttons: the dialog must not grow past its limits
        // for them, they stack.)
        implicitWidth: Math.max(removeButton.implicitWidth, keepButton.implicitWidth) + footerBar.leftPadding + footerBar.rightPadding
        implicitHeight: buttons.implicitHeight + footerBar.topPadding + footerBar.bottomPadding

        contentItem: GridLayout {
            id: buttons

            columns: footerBar.stacked ? 1 : 3
            columnSpacing: Kirigami.Units.smallSpacing
            rowSpacing: Kirigami.Units.smallSpacing
            // No input while the dialog fades in or out, as in Kirigami's own footer.
            enabled: root.opened

            // Side by side the buttons stand at the trailing edge.
            Item {
                visible: !footerBar.stacked
                Layout.fillWidth: true
            }
            QQC2.Button {
                id: removeButton
                text: root.question.confirm ?? ""
                // The icon of an action that removes something; unlinking keeps the chats.
                icon.name: root.action === "unlink" ? "network-disconnect" : "edit-delete"
                Accessible.name: removeButton.text
                Layout.fillWidth: footerBar.stacked
                onClicked: root.confirm()
            }
            QQC2.Button {
                id: keepButton
                text: root.question.cancel ?? ""
                icon.name: "dialog-cancel"
                Accessible.name: keepButton.text
                Layout.fillWidth: footerBar.stacked
                onClicked: root.close()
            }
        }
    }

    Timer {
        id: arming
        interval: root.armDelay
        onTriggered: root.armed = true
    }

    // The keyboard starts on the answer that changes nothing, and the confirming button
    // starts to act a moment later.
    onOpened: {
        keepButton.forceActiveFocus(Qt.TabFocusReason);
        arming.restart();
    }
    onClosed: {
        if (!root.answered) {
            if (root.action === "") {
                App.cancelReplace();
            } else {
                App.cancelAccountAction();
            }
        }
        root.destroy();
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        // Which account: its name and number, text of the account's own.
        QQC2.Label {
            text: root.question.about ?? ""
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            font: Style.styled(Kirigami.Theme.defaultFont, {
                "weight": Font.DemiBold
            })
            Layout.fillWidth: true
        }
        Repeater {
            model: root.question.paragraphs ?? []
            delegate: QQC2.Label {
                required property string modelData
                text: modelData
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                Layout.fillWidth: true
            }
        }
    }
}
