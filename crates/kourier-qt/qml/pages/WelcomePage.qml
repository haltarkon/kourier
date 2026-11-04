pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import app.kourier

// First run: choose a network to link.
Kirigami.ScrollablePage {
    id: page

    // The only (last) page: centered and without a toolbar; otherwise a column.
    property bool alone: true

    signal linkNetwork(string network)

    title: App.hasAccounts ? L10n.i18nc("@title", "Add Account") : L10n.i18nc("@title", "Welcome")
    globalToolBarStyle: page.alone ? Kirigami.ApplicationHeaderStyle.None : Kirigami.ApplicationHeaderStyle.ToolBar
    Kirigami.ColumnView.fillWidth: page.alone
    Kirigami.ColumnView.preferredWidth: Kirigami.Units.gridUnit * 24
    Kirigami.ColumnView.minimumWidth: Kirigami.Units.gridUnit * 18

    ColumnLayout {
        spacing: 0

        Item {
            implicitHeight: Kirigami.Units.gridUnit * 2
        }

        Kirigami.Icon {
            source: Style.appIcon
            implicitWidth: Kirigami.Units.gridUnit * 5
            implicitHeight: Kirigami.Units.gridUnit * 5
            Layout.alignment: Qt.AlignHCenter
        }

        Kirigami.Heading {
            text: L10n.i18nc("@title", "Welcome to Kourier")
            level: 1
            horizontalAlignment: Text.AlignHCenter
            wrapMode: Text.Wrap
            Layout.fillWidth: true
            Layout.topMargin: Kirigami.Units.gridUnit
        }

        QQC2.Label {
            text: L10n.i18nc("@info", "Your chats on the desktop, made for KDE Plasma. Link an account from your phone to get started.")
            horizontalAlignment: Text.AlignHCenter
            wrapMode: Text.Wrap
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 26
            Layout.alignment: Qt.AlignHCenter
            Layout.topMargin: Kirigami.Units.smallSpacing
        }

        FormCard.FormHeader {
            title: L10n.i18nc("@title:group", "Choose a Network")
            Layout.topMargin: Kirigami.Units.gridUnit
        }

        FormCard.FormCard {
            Repeater {
                // A network this build cannot log in to is a row of Settings only, never a choice
                // (`missing`: the viewmodel's word for it).
                model: App.networks.filter(n => !n.missing)
                delegate: ColumnLayout {
                    id: entry

                    required property var modelData
                    required property int index

                    spacing: 0
                    Layout.fillWidth: true

                    FormCard.FormDelegateSeparator {
                        visible: entry.index > 0
                    }
                    FormCard.FormButtonDelegate {
                        text: entry.modelData.name
                        description: entry.modelData.experimental && !Style.networkWanted(entry.modelData.kind) ? entry.modelData.offDescription : entry.modelData.description
                        icon.source: Style.icon(entry.modelData.icon)
                        icon.width: Kirigami.Units.iconSizes.medium
                        icon.height: Kirigami.Units.iconSizes.medium
                        enabled: entry.modelData.available
                        Layout.fillWidth: true
                        onClicked: page.linkNetwork(entry.modelData.kind)
                    }
                }
            }
        }

        FormCard.FormSectionText {
            // The viewmodel's choice by what the networks of the list can do (U7).
            text: App.networksNote
        }
    }
}
