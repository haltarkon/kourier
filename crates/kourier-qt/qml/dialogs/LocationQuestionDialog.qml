pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// Send was pressed with files that would leave with where they were taken (a place in a
// photo's, a video's or a recording's data that Kourier cannot take out before sending). The
// setting "Files with location data" is "Ask each time", so Kourier asks, once for all such
// files of that Send: the title asks ("Send It Anyway?"), the line under it says what the
// file holds, by kind, then up to five names and "and N more", and, when other files or the
// text would still go after "Don't Send", a line that says so.
//
// The answers, in his order: "Send with Location" sends everything, those files with their
// location; "Don't Send" keeps them in the list, marked, and sends the rest (or nothing, when
// nothing else would go). "Remember my choice" makes the answer the setting (the composer
// stores it through Settings once the answer applied; a stale one remembers nothing).
// Escape, the close button and a click outside send nothing and remember nothing: the
// composer dismisses the question, the text and the files stay, and the next Send asks
// again. Whichever way the question ends, the composer gives its text area the focus back.
//
// The keyboard: Return and Enter answer NOTHING here (QQC2 buttons click only with Space,
// the platform's press key); Space answers on a focused button. The focus starts on "Don't
// Send" only when that answer sends nothing (Space there holds the files); otherwise on the
// tick box (Space there ticks it, nothing is answered): nothing that sends ever has the
// focus first. The dialog opens while the key that pressed Send may still be down: it takes no
// key before it is open, no release whose press it did not see, and no auto-repeat (a held
// key would click a button again and again; qml.md "A held key and a QQC2.Button").
//
// Everything it says comes from `App.composer.locationQuestion`; the dialog only shows it.
// File names are text from outside: every label is plain text.
Kirigami.Dialog {
    id: root

    readonly property var question: App.composer.locationQuestion
    readonly property bool asking: App.composer.locationAsking
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
    // What the dialog says, as one text (screen readers read it with the focused control).
    readonly property string body: root.say("accessible")
    // The names shown (at most five; the rest is counted in "and N more").
    readonly property var names: root.asking && root.question.names !== undefined ? root.question.names : []

    // The answer, once given: "send" or "hold" (tests read it). A closed dialog gave none.
    signal answered(string answer, bool remember)

    // The controls (tests drive them).
    readonly property alias sendButton: sendButton
    readonly property alias dontSendButton: dontSendButton
    readonly property alias rememberBox: rememberBox

    // Keys pressed while the dialog was open (their release may act).
    property var pressedKeys: ({})

    function say(key: string): string {
        if (!root.asking) {
            return "";
        }
        const value = root.question[key];
        return value === undefined ? "" : value;
    }

    // Lets through what the dialog may act on: a key pressed while it is open, never an
    // auto-repeat, never a release whose press came before (the Return that pressed Send).
    // Tab and Backtab always go on: an accepted repeat of a held Tab would stop the focus
    // chain here. `event` is a KeyEvent or, in tests, a stand-in with the same fields.
    function filterPress(event: var): void {
        if (event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) {
            return;
        }
        if (!root.opened || event.isAutoRepeat) {
            event.accepted = true;
            return;
        }
        const seen = root.pressedKeys;
        seen[event.key] = true;
        root.pressedKeys = seen;
    }

    function filterRelease(event: var): void {
        if (event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) {
            return;
        }
        const seen = root.pressedKeys;
        if (!root.opened || event.isAutoRepeat || seen[event.key] !== true) {
            event.accepted = true;
            return;
        }
        delete seen[event.key];
        root.pressedKeys = seen;
    }

    // `send` or `hold`, with the tick box's state; the composer stores the setting and answers.
    function answer(what: string): void {
        root.answered(what, rememberBox.checked);
    }

    title: root.say("title")
    preferredWidth: Kirigami.Units.gridUnit * 24
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton
    // (A Popup takes no Accessible: the content and every control say the body instead.)

    // His order: "Send with Location", then "Don't Send", at the end of the row. Own buttons
    // (not `customFooterActions`, whose buttons Kirigami makes and no key filter reaches) in
    // a footer of its own, so they STACK, full width and in the same order, when the dialog
    // is too narrow for both side by side (a long label, a large font: U4); the dialog never
    // grows past its width for them.
    footer: QQC2.Control {
        id: footerBar

        // The side-by-side row has THREE cells (the spacer, then the two buttons): two spacings (RF2).
        readonly property real sideBySide: sendButton.implicitWidth + dontSendButton.implicitWidth + 2 * buttons.columnSpacing
        readonly property bool stacked: footerBar.availableWidth < footerBar.sideBySide

        objectName: "locationFooter"
        padding: Kirigami.Units.largeSpacing
        implicitWidth: Math.max(sendButton.implicitWidth, dontSendButton.implicitWidth) + footerBar.leftPadding + footerBar.rightPadding
        implicitHeight: buttons.implicitHeight + footerBar.topPadding + footerBar.bottomPadding

        contentItem: GridLayout {
            id: buttons

            columns: footerBar.stacked ? 1 : 3
            // The desktop DialogButtonBox's spacing (UR2).
            columnSpacing: Kirigami.Units.mediumSpacing
            rowSpacing: Kirigami.Units.mediumSpacing
            // No click while the dialog opens or closes.
            enabled: root.opened

            Item {
                visible: !footerBar.stacked
                Layout.fillWidth: true
            }
            QQC2.Button {
                id: sendButton
                objectName: "locationSend"
                text: root.say("sendLabel")
                icon.name: "mail-send"
                Accessible.name: sendButton.text
                Accessible.description: root.body
                Layout.fillWidth: footerBar.stacked
                Keys.onPressed: event => root.filterPress(event)
                Keys.onReleased: event => root.filterRelease(event)
                onClicked: root.answer("send")
            }
            QQC2.Button {
                id: dontSendButton
                objectName: "locationDontSend"
                text: root.say("holdLabel")
                icon.name: "dialog-cancel"
                Accessible.name: dontSendButton.text
                Accessible.description: root.body
                Layout.fillWidth: footerBar.stacked
                Keys.onPressed: event => root.filterPress(event)
                Keys.onReleased: event => root.filterRelease(event)
                onClicked: root.answer("hold")
            }
        }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing
        Accessible.description: root.body

        // What the files hold, by kind ("This video contains where it was taken.").
        QQC2.Label {
            objectName: "locationText"
            text: root.say("text")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }

        // Which files: up to five names, then how many more.
        ColumnLayout {
            spacing: 0
            visible: root.names.length > 0
            Layout.fillWidth: true

            Repeater {
                model: root.names
                delegate: QQC2.Label {
                    required property string modelData
                    text: modelData
                    textFormat: Text.PlainText
                    elide: Text.ElideMiddle
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                }
            }
            QQC2.Label {
                objectName: "locationMore"
                visible: text !== ""
                text: root.say("more")
                color: root.quiet
                textFormat: Text.PlainText
                elide: Text.ElideRight
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
            }
        }

        // What still goes after "Don't Send" (F2).
        QQC2.Label {
            objectName: "locationStillGoes"
            visible: text !== ""
            text: root.say("stillGoes")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }

        ColumnLayout {
            spacing: Kirigami.Units.smallSpacing / 2
            Layout.fillWidth: true

            // No focus frame of our own (UR5): in an active window Breeze draws the box's own
            // cue, the indicator's frame in the focus colour, as on every check box.
            QQC2.CheckBox {
                id: rememberBox
                objectName: "locationRemember"
                text: root.say("rememberLabel")
                Accessible.name: rememberBox.text
                Accessible.description: root.body
                Layout.fillWidth: true
                Keys.onPressed: event => root.filterPress(event)
                Keys.onReleased: event => root.filterRelease(event)
            }
            QQC2.Label {
                objectName: "locationSettingsHint"
                text: root.say("settingsHint")
                font: Kirigami.Theme.smallFont
                color: root.quiet
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
            }
        }
    }

    onOpened: {
        root.pressedKeys = {};
        if (root.question.focusDontSend === true) {
            dontSendButton.forceActiveFocus(Qt.TabFocusReason);
        } else {
            rememberBox.forceActiveFocus(Qt.TabFocusReason);
        }
    }
}
