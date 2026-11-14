pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "Export Selected…": what the archive will hold, before the person picks where it goes (Export
// Selected, EXP-ui 2). The numbers come from Rust (`App.exportDialogReady` opens it, then
// `App.exportNumbers(ticks)` answers on every tick change: a JSON text of numbers and sizes only,
// the plan's `DialogView`); every sentence is here, no number is added up here. Where no plan
// stands (a fixture page) `exportNumbers` answers "" and the numbers `show(json)` gave stay.
// Three kind ticks (all on), the disappearing tick only when
// the selection has such messages (off), what is left out and why, and the warning each time.
// [Export…] opens the save dialog (`App.exportChoose`); Cancel and Escape drop the plan
// (`App.exportAbandon`). Loaded by URL on first use (`MainWindow.openExportDialog`), like
// `MuteUntilDialog`.
Kirigami.Dialog {
    id: root

    // The plan's numbers for the ticks (`DialogView::to_json_for`): set by `show`, renewed by
    // `refresh` on every tick change. `messages` and `totalSize` are what goes in with the
    // ticks; the kind lines hold what each kind has.
    property var numbers: ({
            "messages": 0,
            "disappearing": 0,
            "viewOnce": 0,
            "gone": 0,
            "pictures": {
                "count": 0,
                "size": ""
            },
            "videos": {
                "count": 0,
                "size": ""
            },
            "other": {
                "count": 0,
                "size": ""
            },
            "totalSize": "",
            "notHere": 0
        })
    // Export… was pressed (Cancel, Escape and a click outside end the plan instead).
    property bool chosen: false
    readonly property alias picturesBox: picturesBox
    readonly property alias videosBox: videosBox
    readonly property alias otherBox: otherBox
    readonly property alias disappearingBox: disappearingBox
    readonly property bool hasDisappearing: root.numbers.disappearing > 0
    // The messages that go in and the files not on this computer, with the ticks (from Rust).
    readonly property int shownMessages: root.numbers.messages
    readonly property int notHereShown: root.numbers.notHere

    // Asks Rust for the numbers of the ticks now; "" (no plan) keeps what there is.
    function refresh(): void {
        const json = App.exportNumbers(picturesBox.checked, videosBox.checked, otherBox.checked, disappearingBox.checked);
        if (json !== "") {
            root.numbers = JSON.parse(json);
        }
    }

    function show(json: string): void {
        root.numbers = JSON.parse(json);
        picturesBox.checked = true;
        videosBox.checked = true;
        otherBox.checked = true;
        disappearingBox.checked = false;
        root.refresh();
        root.chosen = false;
        root.open();
    }

    // Export…: the ticks go to Rust, which asks for the save dialog.
    function accept(): void {
        if (root.shownMessages <= 0) {
            return;
        }
        root.chosen = true;
        App.exportChoose(picturesBox.checked, videosBox.checked, otherBox.checked, disappearingBox.checked);
        root.close();
    }

    objectName: "exportDialog"
    title: L10n.i18nc("@title:dialog", "Export Selected Messages")
    preferredWidth: Kirigami.Units.gridUnit * 26
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            objectName: "exportGo"
            text: L10n.i18nc("@action:button", "Export…")
            icon.name: "document-export"
            enabled: root.shownMessages > 0
            onTriggered: root.accept()
        },
        Kirigami.Action {
            objectName: "exportCancel"
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]
    onClosed: {
        if (!root.chosen) {
            App.exportAbandon();
        }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        QQC2.Label {
            id: summary

            objectName: "exportSummary"
            text: L10n.i18ncp("@info, %1 how many messages are exported", "%1 message will be exported.", "%1 messages will be exported.", root.shownMessages)
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            Accessible.name: summary.text
            Layout.fillWidth: true
        }
        QQC2.Label {
            id: sizeLine

            objectName: "exportSize"
            text: L10n.i18nc("@info, %1 the size of all the files together", "All the files together: %1", root.numbers.totalSize)
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            Accessible.name: sizeLine.text
            Layout.fillWidth: true
        }

        ColumnLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            QQC2.Label {
                id: kindsLabel

                text: L10n.i18nc("@label, followed by ticks for kinds of files", "Files to include:")
                textFormat: Text.PlainText
                Accessible.name: kindsLabel.text
            }
            QQC2.CheckBox {
                id: picturesBox

                objectName: "exportPictures"
                checked: true
                onToggled: root.refresh()
                text: L10n.i18nc("@option:check, %1 how many pictures, %2 their size", "Pictures (%1, %2)", root.numbers.pictures.count, root.numbers.pictures.size)
                Accessible.name: picturesBox.text
            }
            QQC2.CheckBox {
                id: videosBox

                objectName: "exportVideos"
                checked: true
                onToggled: root.refresh()
                text: L10n.i18nc("@option:check, %1 how many videos, %2 their size", "Videos (%1, %2)", root.numbers.videos.count, root.numbers.videos.size)
                Accessible.name: videosBox.text
            }
            QQC2.CheckBox {
                id: otherBox

                objectName: "exportOther"
                checked: true
                onToggled: root.refresh()
                text: L10n.i18nc("@option:check, %1 how many files, %2 their size", "Other files (%1, %2)", root.numbers.other.count, root.numbers.other.size)
                Accessible.name: otherBox.text
            }
        }

        // Only when the selection has disappearing messages: off, with its warning.
        ColumnLayout {
            visible: root.hasDisappearing
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            QQC2.CheckBox {
                id: disappearingBox

                objectName: "exportDisappearing"
                checked: false
                onToggled: root.refresh()
                text: L10n.i18ncp("@option:check, %1 how many disappearing messages", "Include %1 disappearing message", "Include %1 disappearing messages", root.numbers.disappearing)
                Accessible.name: disappearingBox.text
            }
            QQC2.Label {
                id: disappearingWarning

                objectName: "exportDisappearingWarning"
                text: L10n.i18nc("@info", "The copy does not disappear: it stays in the archive after the timer has deleted the message here.")
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                leftPadding: Kirigami.Units.gridUnit * 1.5
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                Accessible.name: disappearingWarning.text
                Layout.fillWidth: true
            }
        }

        // What is left out, and why.
        ColumnLayout {
            id: leftOut

            readonly property bool any: root.numbers.viewOnce > 0 || root.numbers.gone > 0 || root.notHereShown > 0

            objectName: "exportLeftOut"
            visible: leftOut.any
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            QQC2.Label {
                id: viewOnceLine

                visible: root.numbers.viewOnce > 0
                text: L10n.i18ncp("@info, %1 how many view-once messages", "%1 view-once message is never exported.", "%1 view-once messages are never exported.", root.numbers.viewOnce)
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                Accessible.name: viewOnceLine.text
                Layout.fillWidth: true
            }
            QQC2.Label {
                id: goneLine

                visible: root.numbers.gone > 0
                text: L10n.i18ncp("@info, %1 how many messages", "%1 message is gone and is left out.", "%1 messages are gone and are left out.", root.numbers.gone)
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                Accessible.name: goneLine.text
                Layout.fillWidth: true
            }
            QQC2.Label {
                id: notHereLine

                objectName: "exportNotHere"
                visible: root.notHereShown > 0
                text: L10n.i18ncp("@info, %1 how many files", "%1 file is not on this computer: it is listed without its file.", "%1 files are not on this computer: they are listed without their files.", root.notHereShown)
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                Accessible.name: notHereLine.text
                Layout.fillWidth: true
            }
        }

        // The warning, each time.
        RowLayout {
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            Kirigami.Icon {
                source: "dialog-warning"
                Layout.alignment: Qt.AlignTop
                Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
            }
            ColumnLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                QQC2.Label {
                    id: warning

                    objectName: "exportWarning"
                    text: L10n.i18nc("@info", "The archive is not encrypted. Other programs on this computer can read it, and so can the provider of any agent you give it to.")
                    textFormat: Text.PlainText
                    wrapMode: Text.Wrap
                    Accessible.name: warning.text
                    Layout.fillWidth: true
                }
            }
        }
    }
}
