pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.prison as Prison
import app.kourier

// "Verify Safety Number" (P5a; architecture §8.3): the level of the details panel that "View
// Safety Number" of a person's page opens. The QR code of our side, the 60 digits in 12 groups
// of 5, the hint, the state line and the buttons Mark as Verified / Clear Verification / Confirm
// New Safety Number, all from `App.safety` (a map from the viewmodel's `SafetyView`). A button
// passes only its action: Rust holds the person and the KEY of the number on screen and acts on
// that key alone (the backend refuses another; the refusal comes back as `notice`). Without a
// number the page says why, with no QR code and no button. The panel owns the plate, the close
// button and the keys; this item is kept alive (hidden) while it is in the panel's stack. Rows sit
// in Columns, not layouts.
Item {
    id: root
    objectName: "safetyPage"

    readonly property var safety: App.safety
    // The level has its answer (an empty map before).
    readonly property bool loaded: root.safety.title !== undefined
    readonly property bool hasNumber: root.loaded && (root.safety.hasNumber ?? false)
    // The panel sits beside the timeline (a line on its leading edge), or covers the chat.
    property bool beside: false
    // Room the panel's close button takes at the trailing end of the title row.
    property real trailingGap: 0
    // Where the vertical scroll bar may start (below the panel's close button; set by the panel).
    property real barTop: 0

    // The level's own back arrow.
    signal backRequested

    // What the next answer of the backend should do for the keyboard (P5a U3, W1): "open" (the level was just opened:
    // the keyboard goes to the digits once the answer is here), or the action of the button that was just pressed;
    // "" nothing in particular.
    property string pending: ""
    // The action of the LAST button pressed (W2). Unlike `pending` an answer does not use it up: only an answer that
    // takes the keyboard off its button (the buttons were made anew) does, so an answer in between that rebuilt nothing
    // (an approval's first echo) cannot make the next one forget that the press was a Confirm.
    property string pressed: ""
    // The notice the screen reader heard last (a notice is announced once, and again after every click that
    // brings one).
    property string announced: ""
    // What the last announcement was told and where the keyboard was when it was made: the keyboard is moved FIRST and
    // the words come after (a screen reader drops queued speech when the focus moves), and the notice of a change or a
    // refusal is announced assertively (V4). Read by the tests.
    property string announcedOn: ""
    property int announcedPoliteness: -1
    // The buttons stay dead for a second whenever they APPEAR for a number that was not on screen just before (X1,
    // generalises V1): the first number after the level opened, a number after an answer without one, and a number that
    // swapped under the level. A double click, a key that is still down, a menu entry whose second click lands where a
    // button now stands must not reach a number nobody has compared yet.
    property bool cooling: false
    readonly property int coolingMs: 1000
    // Whether a number was on screen with the last answer: set by an answer that has one, cleared by `reset()` and by an
    // answer without one (X1). The answer that finds it false and brings a number is the number's first sight.
    property bool numberShown: false

    // Forgets the place in the page: the same item serves every later open.
    function reset(): void {
        const flick = scroll.contentItem as Flickable;
        if (flick) {
            flick.contentY = 0;
        }
        root.pending = "";
        root.pressed = "";
        root.announced = "";
        root.announcedOn = "";
        root.announcedPoliteness = -1;
        root.cooling = false;
        root.numberShown = false;
        coolTimer.stop();
    }

    // The buttons are dead for `coolingMs`.
    function cool(): void {
        root.cooling = true;
        coolTimer.restart();
    }

    // Whether the answer on screen offers a button that does `action`.
    function offers(action: string): bool {
        return (root.safety.buttons ?? []).some(b => b.action === action);
    }

    // An answer is here (called by the panel the moment it comes, before anything else can reach the buttons it brings;
    // `numberNow`: the answer has a number): when it shows a number that was not on screen just before (the first of the
    // level, one after an answer without a number) or another number than the one on screen (`swapped`), the buttons are
    // dead for a second (X1, V1) and no event can reach them before the keyboard has been moved off them. So are the
    // buttons that replace an approval (Y2): the last press was Confirm and Confirm is gone, the state line with it, so
    // Mark has moved up under the pointer: the second click of a double click on Confirm must not become a Mark of a
    // number nobody has compared (V2 for the pointer). The same number again (a state that changed: Mark became Clear)
    // wakes nothing and sleeps nothing.
    function arrived(swapped: bool, numberNow: bool): void {
        const appears = numberNow && !root.numberShown;
        root.numberShown = numberNow;
        const approved = numberNow && root.pressed === "confirm" && !root.offers("confirm");
        if (swapped || appears || approved) {
            root.cool();
        }
    }

    Timer {
        id: coolTimer
        interval: root.coolingMs
        onTriggered: root.cooling = false
    }

    // The keyboard goes to the button of `action` (Mark or Clear, the last press: the first button when that action is no
    // longer offered, because Mark became Clear and back; the back arrow when there is no button). The delegates follow
    // the order of the answer's `buttons`. Never called after a Confirm or for a level just opened (`answered`).
    function focusButton(action: string): void {
        const offered = root.safety.buttons ?? [];
        let target = null;
        for (let i = 0; i < offered.length; i++) {
            const b = buttons.itemAt(i);
            if (!b || !b.visible) {
                continue;
            }
            if (target === null) {
                target = b;
            }
            if (offered[i].action === action) {
                target = b;
                break;
            }
        }
        (target ?? backArrow).forceActiveFocus();
    }

    // The keyboard lands on the level (W1): on the DIGITS, the number to read first (a screen reader reads them as the
    // box's accessible name), NEVER on a button: a press that comes right behind the one that opened the level (a double
    // press, a key that bounces) must not mark or confirm a number nobody has read. Tab reaches the buttons. The back
    // arrow when there is no number yet (the answer has not come, or it has none).
    function focusFirst(): void {
        (digitsBox.visible ? digitsBox : backArrow).forceActiveFocus();
    }

    // An answer has come. The keyboard (P5a U3, V1, W1, W2):
    //  - a level just opened (`pending` "open") puts the keyboard on the digits once they are here (it waited on the back
    //    arrow); never on a button (W1);
    //  - the answer shows ANOTHER number than the one on screen (`swapped`, U2): the buttons act on a number nobody has
    //    compared, so the keyboard LEAVES every button of the level, also one that survived because the answer offers
    //    the same buttons (V1: a refused click keeps the button's focus, and one more Return would mark the new
    //    number), and goes to the back arrow, which acts on nothing; the buttons are dead for a second (`arrived`, called
    //    by the panel the moment the answer is here, which does the same for any number that was not on screen just
    //    before: X1);
    //  - a keyboard that lost its button (the buttons were made anew under it, the active focus fell to the scroll view,
    //    the panel or nowhere) goes to a button only when the LAST PRESS was Mark or Clear (W2): the button of the same
    //    action, else the first (Mark became Clear and back); after a Confirm, or when no button was pressed, it goes to
    //    the back arrow (V2: Confirm is an approval, and one more press must not turn it into a verification of a number
    //    nobody compared).
    // A notice is announced to a screen reader AFTER the keyboard has been placed (once; again after a click that
    // brings it).
    function answered(swapped: bool, panelFocused: bool): void {
        const want = root.pending;
        root.pending = "";
        const f = root.Window.activeFocusItem;
        const lost = panelFocused || f === null || f === scroll || f === content;
        const onButton = f !== null && f.objectName === "safetyButton";
        if (swapped) {
            root.pressed = "";
            if (lost || onButton) {
                backArrow.forceActiveFocus();
            }
        } else if (want === "open") {
            if (lost || f === backArrow) {
                root.focusFirst();
            }
        } else if (lost) {
            const press = root.pressed;
            root.pressed = "";
            if (press === "mark" || press === "clear") {
                root.focusButton(press);
            } else {
                backArrow.forceActiveFocus();
            }
        }
        const notice = root.safety.notice ?? "";
        const fresh = notice !== "" && (notice !== root.announced || want !== "");
        root.announced = notice;
        if (fresh) {
            Qt.callLater(() => root.announce(notice));
        }
    }

    // Tells a screen reader `text`, assertively, and records where the keyboard was at that moment.
    function announce(text: string): void {
        const f = root.Window.activeFocusItem;
        const politeness = Accessible.Assertive;
        root.announcedOn = f ? f.objectName : "";
        root.announcedPoliteness = politeness;
        safetyNotice.Accessible.announce(text, politeness);
    }

    // The title row: the back arrow (it mirrors), the title, room for the panel's close button.
    Item {
        id: header
        objectName: "safetyHeader"
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.leftMargin: root.beside ? 1 : 0
        height: Math.max(backArrow.implicitHeight, title.implicitHeight) + Kirigami.Units.smallSpacing * 2
        QQC2.ToolButton {
            id: backArrow
            objectName: "safetyBack"
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            anchors.leftMargin: Kirigami.Units.smallSpacing
            icon.name: LayoutMirroring.enabled ? "go-next" : "go-previous"
            display: QQC2.AbstractButton.IconOnly
            text: L10n.i18nc("@action:button", "Back")
            Accessible.name: text
            onClicked: root.backRequested()
        }
        QQC2.Label {
            id: title
            objectName: "safetyTitle"
            anchors.left: backArrow.right
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            anchors.leftMargin: Kirigami.Units.smallSpacing
            anchors.rightMargin: root.trailingGap
            text: root.safety.title ?? ""
            font: Style.styled(Kirigami.Theme.defaultFont, {
                "weight": Font.DemiBold
            })
            elide: Text.ElideRight
            textFormat: Text.PlainText
        }
    }

    QQC2.ScrollView {
        id: scroll
        objectName: "safetyScroll"
        anchors.top: header.bottom
        anchors.bottom: parent.bottom
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.leftMargin: root.beside ? 1 : 0
        contentWidth: availableWidth
        QQC2.ScrollBar.horizontal.policy: QQC2.ScrollBar.AlwaysOff
        QQC2.ScrollBar.vertical.topPadding: Math.max(0, root.barTop - header.height)

        Column {
            id: content
            width: scroll.availableWidth
            spacing: Kirigami.Units.largeSpacing
            topPadding: Kirigami.Units.largeSpacing
            bottomPadding: Kirigami.Units.largeSpacing * 2

            // The QR code of our side on a white card so a phone scans it in a dark scheme too: the
            // black and white are on purpose (the QR card is the one exception to theme colours).
            Rectangle {
                id: card
                objectName: "safetyQrCard"
                visible: root.hasNumber
                anchors.horizontalCenter: parent.horizontalCenter
                color: "white"
                radius: Kirigami.Units.cornerRadius * 3
                width: Math.min(Kirigami.Units.gridUnit * 12, content.width - Kirigami.Units.largeSpacing * 2)
                height: width
                border.width: 1
                border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                Accessible.ignored: true
                Prison.Barcode {
                    id: qr
                    objectName: "safetyQr"
                    anchors.fill: parent
                    anchors.margins: Kirigami.Units.gridUnit
                    barcodeType: Prison.Barcode.QRCode
                    // The scannable BYTES (a QByteArray from Rust, never a string: critic M6).
                    content: root.safety.scannable ?? ""
                    foregroundColor: "black"
                    backgroundColor: "white"
                }
            }

            // The 60 digits: 12 groups of 5 in rows of 4 (2 when the panel is narrow). Numbers read left
            // to right in every language, so the grid itself does not mirror (critic F9); a screen reader
            // reads them as one text with pauses (`accessibleDigits`). The box is a tab stop and the place where
            // the keyboard lands when the level opens (W1): the number comes first, the buttons after a Tab. A
            // frame shows the keyboard's place after a Tab, as on a button.
            QQC2.Control {
                id: digitsBox
                objectName: "safetyDigits"
                visible: root.hasNumber
                anchors.horizontalCenter: parent.horizontalCenter
                // No padding at all: the desktop style gives a Control 6 px above and below by itself, which would move the
                // digits and everything under them (`padding: 0` does not reach those two).
                topPadding: 0
                bottomPadding: 0
                leftPadding: 0
                rightPadding: 0
                focusPolicy: Qt.TabFocus
                Accessible.role: Accessible.StaticText
                Accessible.name: root.safety.accessibleDigits ?? ""
                Accessible.focusable: true
                background: Rectangle {
                    x: -Kirigami.Units.smallSpacing
                    y: -Kirigami.Units.smallSpacing
                    width: digitsBox.width + Kirigami.Units.smallSpacing * 2
                    height: digitsBox.height + Kirigami.Units.smallSpacing * 2
                    color: "transparent"
                    radius: Kirigami.Units.cornerRadius
                    border.width: digitsBox.visualFocus ? 2 : 0
                    border.color: Kirigami.Theme.focusColor
                }
                contentItem: Grid {
                    id: digits
                    LayoutMirroring.enabled: false
                    LayoutMirroring.childrenInherit: true
                    columns: content.width >= groupWidth.width * 4 + columnSpacing * 3 + Kirigami.Units.largeSpacing * 2 ? 4 : 2
                    columnSpacing: Kirigami.Units.largeSpacing
                    rowSpacing: Kirigami.Units.smallSpacing
                    TextMetrics {
                        id: groupWidth
                        font: Style.styled(Kirigami.Theme.fixedWidthFont, {
                            "scale": 1.2 * Kirigami.Theme.defaultFont.pointSize / Kirigami.Theme.fixedWidthFont.pointSize
                        })
                        text: "00000"
                    }
                    Repeater {
                        model: root.safety.groups ?? []
                        delegate: QQC2.Label {
                            required property string modelData
                            objectName: "safetyGroup"
                            text: modelData
                            font: groupWidth.font
                            textFormat: Text.PlainText
                            Accessible.ignored: true
                        }
                    }
                }
            }

            QQC2.Label {
                objectName: "safetyHint"
                visible: text !== ""
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                text: root.safety.hint ?? ""
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }

            QQC2.Label {
                objectName: "safetyState"
                visible: text !== ""
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                text: root.safety.stateLine ?? ""
                font: Style.styled(Kirigami.Theme.defaultFont, {
                    "weight": Font.DemiBold
                })
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }

            // Why there is no number (no key yet, the account not connected ...): the only text then.
            QQC2.Label {
                objectName: "safetyReason"
                visible: text !== ""
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                text: root.safety.reason ?? ""
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }

            // The buttons, the primary one first. A click acts on the number on screen (Rust holds its key).
            Repeater {
                id: buttons
                model: root.safety.buttons ?? []
                delegate: QQC2.Button {
                    id: button
                    required property var modelData
                    objectName: "safetyButton"
                    property string safetyAction: button.modelData.action
                    x: Kirigami.Units.largeSpacing
                    width: content.width - Kirigami.Units.largeSpacing * 2
                    text: button.modelData.label
                    highlighted: button.modelData.primary
                    // The style draws nothing for `highlighted` on a push button: the primary one gets an icon so Confirm
                    // stands apart from Mark (U8). Only when the primary action is Confirm or Mark: Clear Verification
                    // never carries a check mark (V6).
                    icon.name: button.modelData.primary && (button.safetyAction === "confirm" || button.safetyAction === "mark") ? "dialog-ok-apply" : ""
                    enabled: root.loaded && !root.cooling
                    Accessible.name: text
                    // The state a sighted user reads above the buttons ("changed since you verified it") (U9).
                    Accessible.description: root.safety.stateLine ?? ""
                    // A key that is held acts once: the first press, never its auto-repeats (V1). The parameter is
                    // untyped so a test can hand over a stand-in for the key event.
                    function activateKey(event: var): void {
                        if (!event.isAutoRepeat) {
                            button.clicked();
                        }
                        event.accepted = true;
                    }
                    Keys.onReturnPressed: event => button.activateKey(event)
                    Keys.onEnterPressed: event => button.activateKey(event)
                    // No held key acts twice (X2, Y3): the repeats of EVERY key are taken here, before the button sees them.
                    // The style clicks a button with the RELEASE of Space and of Select (the platform theme's button keys,
                    // `ButtonPressKeys`), and the platform sends a release and a press, both with `isAutoRepeat`, for every
                    // repeat of a held key, so a held key would click again and again (Mark, Clear, Mark ... of the same
                    // number). Not named by key: a key the next Qt adds to that list is covered too. Tab and Backtab are the
                    // exception, they act on nothing and a held Tab goes on through the page. The generic handlers
                    // run for what the one-key handlers (Return, Enter: `activateKey`) leave unaccepted; the first press and
                    // the real release are left alone (`accepted` stays false: a generic handler starts not accepted).
                    function holdKey(event: var): void {
                        if (event.isAutoRepeat && event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab) {
                            event.accepted = true;
                        }
                    }
                    Keys.onPressed: event => button.holdKey(event)
                    Keys.onReleased: event => button.holdKey(event)
                    onClicked: {
                        root.pending = button.safetyAction;
                        root.pressed = button.safetyAction;
                        App.safetyAction(button.safetyAction);
                    }
                }
            }

            // A refused click or a number that changed under the level ("The safety number changed. ..."): plain text
            // under the buttons, announced to a screen reader when it comes (`answered`).
            QQC2.Label {
                id: safetyNotice
                objectName: "safetyNotice"
                visible: text !== ""
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                horizontalAlignment: Text.AlignHCenter
                text: root.safety.notice ?? ""
                color: Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor)
                wrapMode: Text.Wrap
                textFormat: Text.PlainText
            }
        }
    }
}
