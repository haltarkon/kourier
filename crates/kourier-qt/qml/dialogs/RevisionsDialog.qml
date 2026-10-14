pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The earlier versions of an edited message, newest first: when each was written and what it
// said. The versions come from the core (`App.showRevisions`, then `App.revisions`); texts are
// shown like in the timeline (formatting, links after a confirmation, spoilers covered). While
// the dialog is open the list is kept up to date: another edit that arrives shows as a new
// first entry, and a message that is deleted meanwhile empties it.
Kirigami.Dialog {
    id: root

    // The message whose versions are shown.
    property real message: 0
    // Its versions once they arrived.
    property var rows: []
    property bool loaded: false

    title: L10n.i18nc("@title:dialog", "Edit History")
    preferredWidth: Kirigami.Units.gridUnit * 26
    maximumHeight: parent ? Math.round(parent.height * 0.85) : Kirigami.Units.gridUnit * 30
    padding: Kirigami.Units.largeSpacing
    standardButtons: Kirigami.Dialog.Close
    // (No longer shown: the core stops being asked about this message.)
    onClosed: App.closeRevisions()

    Connections {
        target: App
        function onRevisionsLoaded(message: real): void {
            if (message === root.message) {
                root.rows = App.revisions;
                root.loaded = true;
            }
        }
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Spinner {
            visible: !root.loaded
            Layout.alignment: Qt.AlignHCenter
        }

        // The message went away meanwhile (deleted, expired).
        QQC2.Label {
            visible: root.loaded && root.rows.length === 0
            text: L10n.i18nc("@info", "This message is no longer here.")
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }

        Repeater {
            model: root.rows
            delegate: ColumnLayout {
                id: version

                required property var modelData
                required property int index

                readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)

                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                Kirigami.Separator {
                    visible: version.index > 0
                    Layout.fillWidth: true
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                }
                RowLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Layout.fillWidth: true

                    QQC2.Label {
                        text: version.modelData.when
                        font: Kirigami.Theme.smallFont
                        color: version.quiet
                        textFormat: Text.PlainText
                    }
                    QQC2.Label {
                        visible: version.modelData.current || version.modelData.original
                        text: version.modelData.current ? L10n.i18nc("@info the version of a message shown now", "Current version") : L10n.i18nc("@info the message as first sent", "Original")
                        font: Kirigami.Theme.smallFont
                        color: version.modelData.current ? Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor) : version.quiet
                        textFormat: Text.PlainText
                    }
                    Item {
                        Layout.fillWidth: true
                    }
                }
                TextBlock {
                    visible: !version.modelData.empty
                    block: version.modelData.text
                    ink: Kirigami.Theme.textColor
                    surface: Kirigami.Theme.backgroundColor
                    Layout.fillWidth: true
                    Layout.preferredHeight: implicitHeight
                }
                QQC2.Label {
                    visible: version.modelData.empty
                    text: L10n.i18nc("@info a version of a message with files that had no text", "No caption")
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "italic": true
                    })
                    color: version.quiet
                    textFormat: Text.PlainText
                }
            }
        }
    }
}
