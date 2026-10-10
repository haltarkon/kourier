pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// A custom disappearing-message time: a number and a unit, within the range the conversation's
// network takes (`min` … `max` seconds in steps of `step`; the menu's own entries are only the
// values its clients offer by name). A time outside the range cannot be set: the range is
// said, and "Set" waits for a time inside it. Where the network takes only every `step`-th
// second, a time between two steps cannot be set either, and the note says why and names the
// nearest times that can.
Kirigami.Dialog {
    id: root

    // The range, in seconds.
    property int min: 1
    property int max: 1
    property int step: 1
    // The timer that is set now (0 = off): where the fields start.
    property int current: 0

    readonly property var units: {
        const all = [
            {
                "secs": 1,
                "label": L10n.i18nc("@item:inlistbox unit of time", "Seconds")
            },
            {
                "secs": 60,
                "label": L10n.i18nc("@item:inlistbox unit of time", "Minutes")
            },
            {
                "secs": 3600,
                "label": L10n.i18nc("@item:inlistbox unit of time", "Hours")
            },
            {
                "secs": 86400,
                "label": L10n.i18nc("@item:inlistbox unit of time", "Days")
            },
            {
                "secs": 604800,
                "label": L10n.i18nc("@item:inlistbox unit of time", "Weeks")
            }
        ];
        // Only units the range reaches.
        return all.filter(u => u.secs <= root.max);
    }
    readonly property int unitSecs: unit.currentIndex >= 0 && unit.currentIndex < root.units.length ? root.units[unit.currentIndex].secs : 1
    // The time the fields say, in seconds.
    readonly property real seconds: amount.value * root.unitSecs
    readonly property bool inRange: root.seconds >= root.min && root.seconds <= root.max
    readonly property int stride: Math.max(1, root.step)
    // Inside the range, between two of the times the network takes.
    readonly property bool offStep: root.inRange && (root.seconds - root.min) % root.stride !== 0
    readonly property bool valid: root.inRange && !root.offStep
    // The times it takes on either side of what the fields say.
    readonly property int stepBelow: root.min + Math.floor((root.seconds - root.min) / root.stride) * root.stride
    readonly property int stepAbove: root.stepBelow + root.stride
    // Why "Set" waits, or the range while it does not.
    readonly property string note: {
        if (!root.offStep) {
            return L10n.i18nc("@info, %1 shortest and %2 longest time", "Between %1 and %2.", root.spell(root.min), root.spell(root.max));
        }
        if (root.stepAbove > root.max) {
            return L10n.i18nc("@info, %1 the step, %2 the nearest time that can be set", "Times go in steps of %1 here: the nearest is %2.", root.spell(root.stride), root.spell(root.stepBelow));
        }
        return L10n.i18nc("@info, %1 the step, %2 and %3 the nearest times that can be set", "Times go in steps of %1 here: the nearest are %2 and %3.", root.spell(root.stride), root.spell(root.stepBelow), root.spell(root.stepAbove));
    }

    // "45 seconds", "3 days": the largest unit that divides the time.
    function spell(secs: int): string {
        if (secs % 604800 === 0) {
            return L10n.i18ncp("@info a length of time", "%1 week", "%1 weeks", secs / 604800);
        }
        if (secs % 86400 === 0) {
            return L10n.i18ncp("@info a length of time", "%1 day", "%1 days", secs / 86400);
        }
        if (secs % 3600 === 0) {
            return L10n.i18ncp("@info a length of time", "%1 hour", "%1 hours", secs / 3600);
        }
        if (secs % 60 === 0) {
            return L10n.i18ncp("@info a length of time", "%1 minute", "%1 minutes", secs / 60);
        }
        return L10n.i18ncp("@info a length of time", "%1 second", "%1 seconds", secs);
    }

    // The fields show `secs` in the largest unit that divides it.
    function show(secs: int): void {
        let at = 0;
        for (let i = 0; i < root.units.length; ++i) {
            if (secs % root.units[i].secs === 0) {
                at = i;
            }
        }
        unit.currentIndex = at;
        amount.value = Math.max(1, Math.round(secs / root.units[at].secs));
    }

    function accept(): void {
        if (!root.valid) {
            return;
        }
        App.setTimer(root.seconds);
        root.close();
    }

    title: L10n.i18nc("@title:dialog", "Custom Time")
    preferredWidth: Kirigami.Units.gridUnit * 22
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Set")
            icon.name: "dialog-ok-apply"
            enabled: root.valid
            onTriggered: root.accept()
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        QQC2.Label {
            text: L10n.i18nc("@label", "Messages disappear after:")
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            Layout.fillWidth: true
        }
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            QQC2.SpinBox {
                id: amount
                objectName: "timerAmount"
                from: 1
                // Every time of the range can be typed, in seconds if need be: the field has
                // no bound of its own below the range's end.
                to: Math.max(1, root.max)
                editable: true
                // The number counts as it is typed. A spin box otherwise keeps its old value
                // until Enter is released or the field loses the focus: "Set" (which does
                // not take the focus) and Enter (handled when it is pressed) would then set
                // the time the field held before, and the range note would lag behind.
                live: true
                Accessible.name: L10n.i18nc("@label accessible", "How long")
                Layout.fillWidth: true
                Keys.onReturnPressed: root.accept()
                Keys.onEnterPressed: root.accept()
            }
            QQC2.ComboBox {
                id: unit
                objectName: "timerUnit"
                model: root.units
                textRole: "label"
                Accessible.name: L10n.i18nc("@label accessible", "Unit of time")
                Layout.fillWidth: true
            }
        }
        // The range, quietly; in the warning color while the time is outside it, and with
        // the reason while it lies between two times the network takes.
        QQC2.Label {
            objectName: "timerRange"
            text: root.note
            font: Kirigami.Theme.smallFont
            color: root.valid ? Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor) : Style.readable(Kirigami.Theme.neutralTextColor, Kirigami.Theme.backgroundColor)
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }
    }

    onOpened: {
        root.show(root.current > 0 ? root.current : Math.min(root.max, Math.max(root.min, 3600)));
        amount.forceActiveFocus();
    }
}
