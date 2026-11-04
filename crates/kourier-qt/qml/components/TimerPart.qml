pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The "Disappearing Messages" part of a page of the details panel (people pages P1, P2): a
// combo of the times the chat offers (then "Custom Time…" when it takes others), or, where
// the network offers none to change here, the time as a read-only value with the reason (the
// viewmodel's words: `note`). The part acts on ITS OWN chat (`conversation`: a member's 1:1
// chat, the group), never on the one that is open.
//
// A choice in the combo changes only what the combo shows (F4, his answer "Apply and Cancel"):
// "Apply" and "Cancel" stand under it while the choice differs from the chat's time, Escape on
// the combo or a button takes the choice back, and only "Apply" sends it. The choice is kept as
// SECONDS (`pendingSecs`, `hasPending`), never as an index: the entries change under it (the
// chat's own custom time is one of them). After "Apply" the applied time stays shown, the combo
// is grey and a quiet line says "Applying…" until the chat's time changes (to the applied time, or
// to another one: a peer's, or ours dropped by the core's "an older one loses"), the change failed
// (`failed`) or the page is another conversation's: no timer of our own. A backend that answers Ok
// without changing the time leaves "Applying…" until the panel closes or the conversation changes
// (a known limit). A pointer choice in the popup wakes "Apply" and "Cancel" only after a double click
// interval (`settle`): the popup opens right where the row appears. A choice is
// dropped by a change of the conversation, of the offered times (the choice is no longer one of
// them), of the hold, by `reset()` (the panel opening, closing or leaving for another person) and
// by a time that is the choice (a peer's change keeps a choice that still differs).
Column {
    id: root

    // The chat whose timer this is, and the values of its page.
    property real conversation: 0
    // The times offered, `{secs, label}` each; empty: none can be set here.
    property var timers: []
    property int timerSecs: 0
    // The time as a word ("1 week", "Off"): the read-only value.
    property string timerLabel: ""
    // The chat takes other times than the offered ones, within this range.
    property bool custom: false
    property int customMin: 0
    property int customMax: 0
    property int customStep: 1
    // Why the time is read-only; empty when there is nothing to say.
    property string note: ""
    // The object names of the combo and of the hold line (tests find them).
    property string boxName: ""
    property string heldName: ""
    // Why the last change of this chat's time failed (the viewmodel's words, F4); empty when
    // none did. A line under the combo, as the hold's reason is.
    property string failed: ""

    // The choice that waits for "Apply" (seconds; `hasPending` says whether there is one).
    property int pendingSecs: 0
    property bool hasPending: false
    // The time "Apply" sent, shown until the chat says it (or the change failed).
    property int appliedSecs: 0
    property bool applying: false
    // Why the combo is grey: the account's network holds back what is sent (empty: it does not).
    readonly property string holdReason: App.timeline.timerHeld
    // The choice is not the chat's time: "Apply" and "Cancel" are shown.
    readonly property bool differs: root.hasPending && root.pendingSecs !== root.timerSecs
    // What the combo shows.
    readonly property int shownSecs: root.applying ? root.appliedSecs : (root.hasPending ? root.pendingSecs : root.timerSecs)

    // "Custom Time…" was chosen: the page asks for the dialog.
    signal customRequested(real conversation, int min, int max, int step, int current)

    spacing: Kirigami.Units.smallSpacing

    // A choice made in the popup by the pointer: the popup of the desktop style opens right under the combo, where
    // "Apply" appears, and the second click of a double click on an entry would press it (M1). The buttons wake after
    // the double click interval.
    Timer {
        id: settle
        // (`Qt.styleHints` is untyped for qmllint: through a `var`.)
        readonly property var hints: Qt.styleHints
        interval: settle.hints.mouseDoubleClickInterval
    }

    // The choice goes: another conversation, the hold began or ended, the page is reset.
    function dropChoice(): void {
        // (A keyboard on "Apply" or "Cancel" goes back to the combo, as the buttons go.)
        if (applyButton.activeFocus || cancelButton.activeFocus) {
            timerBox.forceActiveFocus();
        }
        settle.stop();
        root.hasPending = false;
        root.applying = false;
    }
    // Chooses `secs` as the combo's popup does (the fixture page `timerpending`); whether a choice waits for "Apply".
    function chooseTime(secs: int): bool {
        const at = timerBox.entries.findIndex(e => !e.custom && e.secs === secs);
        if (at < 0 || !timerBox.visible) {
            return false;
        }
        timerBox.activated(at);
        return root.hasPending;
    }
    // Called by the page's `reset()`: the same item serves every later page.
    function reset(): void {
        root.dropChoice();
    }
    // Takes the choice back; the keyboard stays with the combo.
    function cancel(): void {
        settle.stop();
        root.hasPending = false;
        timerBox.forceActiveFocus();
    }
    // Sends the choice. The keyboard goes to the combo FIRST: Qt keeps a disabled item's place
    // and hands it the keyboard again when it is enabled (the applied time arrived, or the
    // change failed).
    function apply(): void {
        if (!root.differs) {
            return;
        }
        root.appliedSecs = root.pendingSecs;
        root.hasPending = false;
        timerBox.forceActiveFocus();
        root.applying = true;
        App.setTimerOf(root.conversation, root.appliedSecs);
    }
    // Escape takes a waiting choice back and is used up; with none it goes on (the panel's Back).
    function escapeKey(event: var): void {
        if (root.differs) {
            root.cancel();
            event.accepted = true;
        } else {
            event.accepted = false;
        }
    }

    onConversationChanged: root.dropChoice()
    onHoldReasonChanged: root.dropChoice()
    // A failure of this chat's timer ends the wait for it; a choice that waits stays (the failure may be the
    // header's clock menu, not our Apply).
    onFailedChanged: {
        if (root.failed !== "") {
            root.applying = false;
        }
    }
    onTimerSecsChanged: {
        // The chat said its time: ours (the confirmation) or another one (a peer's, or ours dropped by the core's
        // "an older one loses"); the combo follows the chat either way.
        if (root.applying) {
            root.applying = false;
        }
        // The chat now has the time that was chosen (a peer chose it too): nothing waits.
        if (root.hasPending && root.pendingSecs === root.timerSecs) {
            root.dropChoice();
        }
    }

    QQC2.Label {
        width: root.width
        text: L10n.i18nc("@title:group", "Disappearing Messages")
        font: Style.styled(Kirigami.Theme.defaultFont, {
            "weight": Font.DemiBold
        })
        textFormat: Text.PlainText
    }
    QQC2.ComboBox {
        id: timerBox
        objectName: root.boxName
        visible: root.timers.length > 0
        width: root.width
        // The offered times, then "Custom Time…" when the chat takes others.
        readonly property var entries: {
            const all = (root.timers ?? []).map(t => ({
                        "label": t.label,
                        "secs": t.secs,
                        "custom": false
                    }));
            if (root.custom) {
                all.push({
                    "label": L10n.i18nc("@action:inmenu", "Custom Time…"),
                    "secs": -1,
                    "custom": true
                });
            }
            return all;
        }
        readonly property int shownIndex: entries.findIndex(e => !e.custom && e.secs === root.shownSecs)
        enabled: root.holdReason === "" && !root.applying
        // The desktop style turns the wheel on: a tick over the combo, while the panel is
        // scrolled with the pointer resting on it, would pick another time.
        wheelEnabled: false
        model: timerBox.entries
        textRole: "label"
        currentIndex: timerBox.shownIndex
        Accessible.name: L10n.i18nc("@title:group", "Disappearing Messages")
        Accessible.description: root.holdReason !== "" ? root.holdReason : (root.failed !== "" ? root.failed : (root.differs ? L10n.i18nc("@info accessible description of the disappearing messages selector", "Not applied yet. Choose Apply to change it.") : ""))
        onActivated: index => {
            const e = timerBox.entries[index];
            if (e.custom) {
                // The dialog has its own "Set" and "Cancel".
                root.dropChoice();
                root.customRequested(root.conversation, root.customMin, root.customMax, root.customStep, root.timerSecs);
            } else if (e.secs === root.timerSecs) {
                root.hasPending = false;
            } else {
                root.pendingSecs = e.secs;
                root.hasPending = true;
                // (The combo emits `activated` before it closes its popup: a choice from the popup is a pointer's.)
                if (timerBox.popup.visible) {
                    settle.restart();
                }
            }
            // The mark follows the choice kept above, never the click itself.
            timerBox.currentIndex = Qt.binding(() => timerBox.shownIndex);
        }
        // A new list of times resets the combo's own index (to the first entry), and a binding runs again only when
        // what it reads changes (`shownIndex` may stay the same number): after every new model the index is bound
        // again, so the combo shows `shownSecs` and not the first entry.
        onModelChanged: timerBox.currentIndex = Qt.binding(() => timerBox.shownIndex)
        onCountChanged: timerBox.currentIndex = Qt.binding(() => timerBox.shownIndex)
        // The offered times changed: a choice (or an applied time) that is no longer one of them goes.
        onEntriesChanged: {
            const listed = secs => timerBox.entries.some(e => !e.custom && e.secs === secs);
            if (root.hasPending && !listed(root.pendingSecs)) {
                root.dropChoice();
            }
            if (root.applying && !listed(root.appliedSecs)) {
                root.applying = false;
            }
        }
        Keys.onEscapePressed: event => root.escapeKey(event)
    }
    // "Apply" and "Cancel" while the choice differs from the chat's time: side by side while
    // both fit, else stacked full width ("Apply" on top).
    GridLayout {
        id: choice
        objectName: root.boxName === "" ? "" : root.boxName + "Choice"
        visible: root.timers.length > 0 && root.differs && root.holdReason === ""
        width: root.width
        columns: choice.stacked ? 1 : 2
        columnSpacing: Kirigami.Units.mediumSpacing
        rowSpacing: Kirigami.Units.smallSpacing
        uniformCellWidths: true
        readonly property bool stacked: root.width < 2 * Math.max(applyButton.implicitWidth, cancelButton.implicitWidth) + choice.columnSpacing

        QQC2.Button {
            id: applyButton
            objectName: root.boxName === "" ? "" : root.boxName + "Apply"
            Layout.fillWidth: true
            text: L10n.i18nc("@action:button", "Apply")
            icon.name: "dialog-ok-apply"
            enabled: !settle.running
            Accessible.name: text
            // A held key acts once (people.md P5a, qml.md "A held key and a button"): the first press, never its
            // auto-repeats. The parameter is untyped so a test can hand over a stand-in for the key event.
            function activateKey(event: var): void {
                if (!event.isAutoRepeat) {
                    applyButton.clicked();
                }
                event.accepted = true;
            }
            Keys.onReturnPressed: event => applyButton.activateKey(event)
            Keys.onEnterPressed: event => applyButton.activateKey(event)
            // The repeats of EVERY key but Tab and Backtab are taken here (the style clicks with the release of Space).
            function holdKey(event: var): void {
                if (event.isAutoRepeat && event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab) {
                    event.accepted = true;
                }
            }
            Keys.onPressed: event => applyButton.holdKey(event)
            Keys.onReleased: event => applyButton.holdKey(event)
            Keys.onEscapePressed: event => root.escapeKey(event)
            onClicked: root.apply()
        }
        QQC2.Button {
            id: cancelButton
            objectName: root.boxName === "" ? "" : root.boxName + "Cancel"
            Layout.fillWidth: true
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            enabled: !settle.running
            Accessible.name: text
            function activateKey(event: var): void {
                if (!event.isAutoRepeat) {
                    cancelButton.clicked();
                }
                event.accepted = true;
            }
            Keys.onReturnPressed: event => cancelButton.activateKey(event)
            Keys.onEnterPressed: event => cancelButton.activateKey(event)
            function holdKey(event: var): void {
                if (event.isAutoRepeat && event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab) {
                    event.accepted = true;
                }
            }
            Keys.onPressed: event => cancelButton.holdKey(event)
            Keys.onReleased: event => cancelButton.holdKey(event)
            Keys.onEscapePressed: event => root.escapeKey(event)
            onClicked: root.cancel()
        }
    }
    // "Apply" was pressed and the chat does not have the time yet (a network that confirms first).
    QQC2.Label {
        objectName: root.boxName === "" ? "" : root.boxName + "Applying"
        visible: root.timers.length > 0 && root.applying
        width: root.width
        text: L10n.i18nc("@info:status the new time of disappearing messages is being set", "Applying…")
        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        wrapMode: Text.Wrap
        textFormat: Text.PlainText
        font: Kirigami.Theme.smallFont
    }
    // The change failed: the viewmodel's words, where he is looking (the composer's notice may be hidden
    // under a covering panel, or be another chat's).
    QQC2.Label {
        objectName: root.boxName === "" ? "" : root.boxName + "Failed"
        visible: root.timers.length > 0 && root.failed !== ""
        width: root.width
        text: root.failed
        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        wrapMode: Text.Wrap
        textFormat: Text.PlainText
        font: Kirigami.Theme.smallFont
    }
    // Where the time cannot be changed here: its value, as a line.
    QQC2.Label {
        objectName: root.boxName === "" ? "" : root.boxName + "Value"
        visible: root.timers.length === 0
        width: root.width
        text: root.timerLabel
        wrapMode: Text.Wrap
        textFormat: Text.PlainText
    }
    // Why it cannot be changed here.
    QQC2.Label {
        objectName: root.boxName === "" ? "" : root.boxName + "Note"
        visible: root.timers.length === 0 && root.note !== ""
        width: root.width
        text: root.note
        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        wrapMode: Text.Wrap
        textFormat: Text.PlainText
        font: Kirigami.Theme.smallFont
    }
    // Why the combo is grey: the account's network holds back what is sent.
    QQC2.Label {
        objectName: root.heldName
        visible: root.timers.length > 0 && root.holdReason !== ""
        width: root.width
        text: root.holdReason
        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        wrapMode: Text.Wrap
        textFormat: Text.PlainText
        font: Kirigami.Theme.smallFont
    }
}
