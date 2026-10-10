pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.delegates as Delegates
import app.kourier

// "All chats", one entry per account (avatar, network badge, state dot), "Add account" and
// "Settings". A slim rail beside the list when the window is wide, a modal drawer when narrow.
Kirigami.GlobalDrawer {
    id: drawer

    required property Kirigami.ApplicationWindow window

    signal openSettings
    signal addAccount

    // How the platform writes the key that opens the settings (for the entry's tooltip).
    property string settingsKeys: ""

    // Collapsed sidebar: avatars only, names in tooltips.
    readonly property bool rail: drawer.collapsed

    // Without accounts there is nothing to switch between: no rail, no handle.
    modal: !drawer.window.wideScreen || !App.hasAccounts
    collapsible: !drawer.modal
    collapsed: !drawer.modal
    collapseButtonVisible: !drawer.modal
    showTopContentWhenCollapsed: true
    showHeaderWhenCollapsed: true
    collapsedSize: Kirigami.Units.gridUnit * 3.5
    width: drawer.collapsed ? drawer.collapsedSize : Kirigami.Units.gridUnit * 17

    // Kirigami's OverlayDrawer assigns `drawerOpen` and `collapsible` (and through it
    // `collapsed`) when `modal` turns true, which breaks the bindings above: a window that was
    // narrow once kept the hamburger and lost its rail when widened again. Restore them.
    onModalChanged: {
        drawer.collapsible = Qt.binding(() => !drawer.modal);
        drawer.collapsed = Qt.binding(() => !drawer.modal);
        drawer.drawerOpen = !drawer.modal;
    }
    leftPadding: 0
    rightPadding: 0
    topPadding: 0
    bottomPadding: 0

    header: Kirigami.AbstractApplicationHeader {
        visible: !drawer.rail
        contentItem: RowLayout {
            spacing: Kirigami.Units.smallSpacing
            anchors.fill: parent
            anchors.leftMargin: Kirigami.Units.largeSpacing
            Kirigami.Icon {
                source: Style.appIcon
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            Kirigami.Heading {
                level: 3
                text: "Kourier"
                Layout.fillWidth: true
            }
        }
    }

    component Entry: Delegates.RoundedItemDelegate {
        id: entry

        property string label
        property string detail
        property string iconName
        property string avatarName
        property string avatarUrl
        property string badgeIcon
        property string stateName
        property int unreadCount: 0
        // The key of the action, as the platform writes it (shown in the tooltip).
        property string keys: ""

        Layout.fillWidth: true
        // At least the usual height, more when the two lines of text need it.
        implicitHeight: Math.max(Kirigami.Units.gridUnit * 2.8, Math.ceil(entry.implicitContentHeight + entry.topPadding + entry.bottomPadding))
        text: entry.label

        // The active account in a high-contrast scheme: a solid frame as well as the tint,
        // which alone does not set the entry off from the page.
        Rectangle {
            anchors.fill: parent
            visible: Style.highContrast && entry.highlighted
            radius: Kirigami.Units.cornerRadius
            color: "transparent"
            border.width: 2
            border.color: Kirigami.Theme.textColor
        }

        QQC2.ToolTip.visible: (drawer.rail || entry.keys !== "") && entry.hovered
        QQC2.ToolTip.text: Style.plain((drawer.rail && entry.detail !== "" ? entry.label + " · " + entry.detail : entry.label) + (entry.keys !== "" ? " (" + entry.keys + ")" : ""))
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay

        contentItem: RowLayout {
            spacing: Kirigami.Units.largeSpacing

            Item {
                implicitWidth: Kirigami.Units.gridUnit * 1.9
                implicitHeight: implicitWidth
                Layout.alignment: drawer.rail ? Qt.AlignHCenter : Qt.AlignLeft

                Kirigami.Icon {
                    anchors.centerIn: parent
                    visible: entry.avatarName === ""
                    source: entry.iconName
                    implicitWidth: Kirigami.Units.iconSizes.smallMedium
                    implicitHeight: Kirigami.Units.iconSizes.smallMedium
                }
                Avatar {
                    anchors.fill: parent
                    visible: entry.avatarName !== ""
                    name: entry.avatarName
                    source: entry.avatarUrl
                    badge: entry.badgeIcon
                    status: entry.stateName
                }
                UnreadPill {
                    visible: drawer.rail && entry.unreadCount > 0
                    count: entry.unreadCount
                    compact: true
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.rightMargin: -Kirigami.Units.smallSpacing
                    anchors.topMargin: -Kirigami.Units.smallSpacing
                }
            }

            ColumnLayout {
                visible: !drawer.rail
                spacing: 0
                Layout.fillWidth: true
                QQC2.Label {
                    text: entry.label
                    textFormat: Text.PlainText
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
                QQC2.Label {
                    visible: entry.detail !== ""
                    text: entry.detail
                    textFormat: Text.PlainText
                    elide: Text.ElideRight
                    color: Style.secondary(Kirigami.Theme.textColor, entry.highlighted ? Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, 0.3) : Kirigami.Theme.backgroundColor)
                    font: Kirigami.Theme.smallFont
                    Layout.fillWidth: true
                }
            }

            UnreadPill {
                visible: !drawer.rail && entry.unreadCount > 0
                count: entry.unreadCount
            }
        }
    }

    topContent: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing / 2
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.smallSpacing

        Entry {
            label: L10n.i18nc("@action:button all conversations", "All Chats")
            iconName: "dialog-messages"
            highlighted: App.currentAccount < 0
            onClicked: {
                App.selectAccount(-1);
                if (drawer.modal) {
                    drawer.close();
                }
            }
        }

        Kirigami.Separator {
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.largeSpacing
        }

        Repeater {
            model: App.accounts
            delegate: Entry {
                required property real accountId
                required property string name
                // The model's avatar fills Entry's own property.
                required avatarUrl
                required property string networkName
                required property string networkIcon
                required property string status
                required property string statusLabel
                required property int unread
                // The account's own number or address on its network (empty when unknown).
                required property string number

                label: name
                // Which account it is: its number where the network has one (the badge on
                // the picture says which network), then its state.
                detail: (number !== "" ? number : networkName) + " · " + statusLabel
                avatarName: name
                badgeIcon: networkIcon
                stateName: status
                unreadCount: unread
                highlighted: App.currentAccount === accountId
                onClicked: {
                    App.selectAccount(accountId);
                    if (drawer.modal) {
                        drawer.close();
                    }
                }
            }
        }

        Entry {
            label: L10n.i18nc("@action:button", "Add Account")
            iconName: "list-add"
            onClicked: {
                drawer.addAccount();
                if (drawer.modal) {
                    drawer.close();
                }
            }
        }
    }

    footer: ColumnLayout {
        spacing: 0
        Kirigami.Separator {
            Layout.fillWidth: true
        }
        Entry {
            Layout.margins: Kirigami.Units.smallSpacing
            label: L10n.i18nc("@action:button", "Settings")
            keys: drawer.settingsKeys
            iconName: "configure"
            onClicked: {
                drawer.openSettings();
                if (drawer.modal) {
                    drawer.close();
                }
            }
        }
    }
}
