pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "Delete this attachment?": one picture or file of a message is removed for this account, the
// message and its other attachments stay. A message with several asks which one. The question
// says what happens elsewhere: the account's other devices follow where the network has a
// message for that (`App.timeline.deletesSync`), other people always keep their copies.
Kirigami.PromptDialog {
    id: root

    // The message (`TimelineActions` row description; its `attachments` are listed).
    property var row: null
    // Which of its attachments is chosen.
    property int chosen: 0

    readonly property var attachments: root.row && root.row.attachments ? root.row.attachments : []
    readonly property bool several: root.attachments.length > 1
    readonly property real attachment: root.chosen >= 0 && root.chosen < root.attachments.length ? root.attachments[root.chosen].attachment : 0
    readonly property bool sync: App.timeline.deletesSync

    title: root.several ? L10n.i18nc("@title:dialog", "Delete an Attachment?") : L10n.i18nc("@title:dialog", "Delete Attachment?")
    subtitle: root.sync ? L10n.i18nc("@info", "It is removed from this message here and on your other devices. The message stays, and the other people in the chat keep their copies.") : L10n.i18nc("@info", "It is removed from this message on this device only. The message stays; your other devices and the other people in the chat keep their copies.")
    preferredWidth: Kirigami.Units.gridUnit * 24
    maximumWidth: Kirigami.Units.gridUnit * 24
    standardButtons: Kirigami.Dialog.NoButton
    customFooterActions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Delete Attachment")
            icon.name: "trash-empty"
            enabled: root.attachment > 0
            onTriggered: {
                App.deleteAttachment(root.attachment);
                root.close();
            }
        },
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            onTriggered: root.close()
        }
    ]

    // Which one: Up and Down move, the chosen one is marked.
    ColumnLayout {
        visible: root.several
        spacing: Kirigami.Units.smallSpacing

        QQC2.Label {
            text: L10n.i18nc("@label", "Which one?")
            font: Style.styled(Kirigami.Theme.defaultFont, {
                "weight": Font.DemiBold
            })
            textFormat: Text.PlainText
        }
        QQC2.ScrollView {
            Layout.fillWidth: true
            Layout.preferredHeight: Math.min(choices.contentHeight, Kirigami.Units.gridUnit * 13)

            ListView {
                id: choices
                objectName: "attachmentChoices"
                model: root.several ? root.attachments : []
                clip: true
                currentIndex: root.chosen
                onCurrentIndexChanged: root.chosen = choices.currentIndex
                keyNavigationEnabled: true
                activeFocusOnTab: true
                boundsBehavior: Flickable.StopAtBounds
                Accessible.role: Accessible.List
                Accessible.name: L10n.i18nc("@info accessible", "Attachments of the message")

                delegate: QQC2.RadioDelegate {
                    id: choice

                    required property int index
                    required property var modelData

                    width: ListView.view.width
                    checked: choice.index === root.chosen
                    // The name of a file comes from its sender: shown as it is, never as markup.
                    text: choice.modelData.name
                    Accessible.name: choice.modelData.name
                    onClicked: root.chosen = choice.index

                    contentItem: RowLayout {
                        spacing: Kirigami.Units.largeSpacing

                        Rectangle {
                            implicitWidth: Math.round(Kirigami.Units.gridUnit * 2.2)
                            implicitHeight: implicitWidth
                            radius: Kirigami.Units.cornerRadius
                            color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.08)
                            clip: true

                            Image {
                                id: thumb
                                anchors.fill: parent
                                visible: status === Image.Ready
                                // (The image provider's URL of the row's thumbnail.)
                                source: choice.modelData.thumb
                                asynchronous: true
                                fillMode: Image.PreserveAspectCrop
                                sourceSize.width: Math.ceil(width)
                                sourceSize.height: Math.ceil(height)
                            }
                            Kirigami.Icon {
                                anchors.centerIn: parent
                                visible: !thumb.visible
                                width: Kirigami.Units.iconSizes.smallMedium
                                height: width
                                source: choice.modelData.icon
                            }
                            // The frame of a tile that shows an icon; a picture is its own edge
                            // (its square corners would stand out of a rounded frame).
                            Rectangle {
                                visible: !thumb.visible
                                anchors.fill: parent
                                radius: parent.radius
                                color: "transparent"
                                border.width: 1
                                border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                            }
                        }
                        QQC2.Label {
                            text: choice.modelData.name
                            elide: Text.ElideMiddle
                            textFormat: Text.PlainText
                            Layout.fillWidth: true
                        }
                    }
                }
            }
        }
    }

    onOpened: {
        if (root.several) {
            choices.forceActiveFocus();
        }
    }
}
