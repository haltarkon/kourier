pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami

// "Stop Linking?": the question before an action would cancel a link that has not finished
// (opening Settings, "Add Account", "Link Again" on the banner, or a chat from the list
// beside the link page, while the link page waits for its QR code to be scanned). `question` comes from `App.abandonQuestion()`; the
// action itself is the caller's (`action`). The same dialog asks "Quit While …?" before the
// application quits while a history transfer or a new link's update runs
// (`App.quitQuestion()`, `MainWindow.quitAsking`).
//
// As the question before an account is replaced: the default keeps the link (the keyboard
// starts on "Continue Linking", Enter confirms nothing, Escape, the close button and a
// click outside keep it), the confirming button acts once and only after the question has
// been open for a moment (it can open under the pointer of a double click), and the two
// buttons stack when the dialog is too narrow for both.
Kirigami.PromptDialog {
    id: root

    // What to ask: `title`, `text`, `confirm`, `cancel`.
    property var question: ({})
    property bool answered: false
    property bool armed: false
    readonly property int armDelay: Math.max(1000, 2 * Application.styleHints.mouseDoubleClickInterval)

    // The two buttons (tests press them).
    readonly property Item confirmButton: stopButton
    readonly property Item cancelButton: keepButton

    // What to do once the user chose to stop the link (the caller's action).
    property var action: null
    // The confirming button's icon.
    property string confirmIcon: "dialog-cancel"

    function confirm(): void {
        if (root.answered || !root.armed) {
            return;
        }
        root.answered = true;
        const action = root.action;
        root.close();
        if (action) {
            action();
        }
    }

    title: root.question.title ?? ""
    dialogType: Kirigami.PromptDialog.Warning
    preferredWidth: Kirigami.Units.gridUnit * 26
    maximumWidth: Kirigami.Units.gridUnit * 26
    standardButtons: Kirigami.Dialog.NoButton

    footer: QQC2.Control {
        id: footerBar

        readonly property real sideBySide: stopButton.implicitWidth + keepButton.implicitWidth + buttons.columnSpacing
        readonly property bool stacked: footerBar.availableWidth < footerBar.sideBySide

        padding: Kirigami.Units.largeSpacing
        implicitWidth: Math.max(stopButton.implicitWidth, keepButton.implicitWidth) + footerBar.leftPadding + footerBar.rightPadding
        implicitHeight: buttons.implicitHeight + footerBar.topPadding + footerBar.bottomPadding

        contentItem: GridLayout {
            id: buttons

            columns: footerBar.stacked ? 1 : 3
            columnSpacing: Kirigami.Units.smallSpacing
            rowSpacing: Kirigami.Units.smallSpacing
            enabled: root.opened

            Item {
                visible: !footerBar.stacked
                Layout.fillWidth: true
            }
            QQC2.Button {
                id: stopButton
                text: root.question.confirm ?? ""
                icon.name: root.confirmIcon
                Accessible.name: stopButton.text
                Layout.fillWidth: footerBar.stacked
                onClicked: root.confirm()
            }
            QQC2.Button {
                id: keepButton
                text: root.question.cancel ?? ""
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

    onOpened: {
        keepButton.forceActiveFocus(Qt.TabFocusReason);
        arming.restart();
    }
    onClosed: root.destroy()

    // (A layout, as the question about an account's replacement has: a bare label as the
    // dialog's content kept the text on one line, cut off at the dialog's edge.)
    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        QQC2.Label {
            text: root.question.text ?? ""
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
    }
}
