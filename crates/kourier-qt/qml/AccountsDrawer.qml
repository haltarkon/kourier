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

    // Whether the drawer acts as one that has accounts: `App.hasAccounts`, but a moment
    // LATE when it turns false. The last account leaving deletes its entry from the
    // Repeater below; flipping the rail into a modal drawer in that same moment made
    // Kirigami's GlobalDrawer read the size of a layout that still held the freed entry, a
    // use-after-free in Qt Quick Layouts that crashed the application (wide window, threaded
    // render loop, many chats: `transfer_review_ui` case `wide-many`). MainWindow's
    // `enabled` follows this too. An account arriving is followed at once.
    // It is set once when the drawer is made and then only by the handler below: a binding
    // to `App.hasAccounts` would stay alive and could re-evaluate at the false edge in the
    // very moment the entry is deleted, whatever the start order (a drawer made while the
    // accounts are already there never saw the true edge that broke it).
    property bool accountsShown: false
    Component.onCompleted: drawer.accountsShown = App.hasAccounts
    Connections {
        target: App
        function onHasAccountsChanged(): void {
            if (App.hasAccounts) {
                accountsFollow.stop();
                drawer.accountsShown = true;
            } else {
                accountsFollow.restart();
            }
        }
    }
    Timer {
        id: accountsFollow
        interval: 300
        onTriggered: drawer.accountsShown = App.hasAccounts
    }

    // The context menu of an account's entry (link feedback L4): what Settings → "Accounts"
    // offers for it. Made once an account entry is hovered, focused or pressed.
    property bool menuWanted: false
    property QQC2.Menu accountMenu: null
    onMenuWantedChanged: {
        if (drawer.menuWanted && drawer.accountMenu === null) {
            drawer.accountMenu = accountMenuComponent.createObject(drawer) as QQC2.Menu;
        }
    }
    // (A Component, not a Loader: an Item here would go into the drawer's layout.)
    Component {
        id: accountMenuComponent

        QQC2.Menu {
            id: accountMenu

            // The account the menu was opened for, and what it offers
            // (`App.accountActions`).
            property real account: 0
            property var actions: ({})

            popupType: QQC2.Popup.Item

            // (A hidden item is disabled too: the menu's keyboard navigation does not skip an
            // item that is only invisible, and "Transfer History from Phone…" must not be
            // reachable with Down and Return while an import runs.)
            QQC2.MenuItem {
                visible: accountMenu.actions.transfer === true
                enabled: visible
                height: visible ? implicitHeight : 0
                icon.name: "document-import"
                text: accountMenu.actions.transferLabel ?? ""
                onTriggered: App.askReplace(accountMenu.account)
            }
            QQC2.MenuItem {
                visible: accountMenu.actions.unlink === true
                enabled: visible
                height: visible ? implicitHeight : 0
                icon.name: "network-disconnect"
                text: accountMenu.actions.unlinkLabel ?? ""
                onTriggered: App.askAccountAction(accountMenu.account, "unlink")
            }
            QQC2.MenuItem {
                objectName: "drawer-remove-account"
                icon.name: "list-remove-user"
                text: accountMenu.actions.removeLabel ?? ""
                onTriggered: App.askAccountAction(accountMenu.account, "remove")
            }
        }
    }

    // Opens the menu of `account` at (`x`, `y`) of `entry`, or under the entry (keyboard). A
    // menu that does not exist yet (the pointer never hovered an entry, nothing was focused or
    // pressed) is made now and shown in the next turn of the event loop: a menu popped up in
    // the moment it was made can come out the size of its padding and never open.
    function openAccountMenu(entry: Item, account: real, x: real, y: real): void {
        const actions = App.accountActions.find(a => a.account === account);
        if (actions === undefined) {
            return;
        }
        const existed = drawer.accountMenu !== null;
        drawer.menuWanted = true;
        if (drawer.accountMenu === null) {
            return;
        }
        if (existed) {
            drawer.popupAccountMenu(entry, account, x, y);
        } else {
            Qt.callLater(drawer.popupAccountMenu, entry, account, x, y);
        }
    }

    function popupAccountMenu(entry: Item, account: real, x: real, y: real): void {
        const actions = App.accountActions.find(a => a.account === account);
        const menu = drawer.accountMenu;
        if (actions === undefined || menu === null || entry === null) {
            return;
        }
        menu.account = account;
        menu.actions = actions;
        if (x < 0) {
            menu.popup(entry, 0, entry.height);
        } else {
            menu.popup(entry, x, y);
        }
    }

    // Without accounts there is nothing to switch between: no rail, no handle.
    modal: !drawer.window.wideScreen || !drawer.accountsShown
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
        // The entry of an account: it has a context menu (`accountMenu`).
        property real menuAccount: 0

        // Right click, a long press on a touch screen, the Menu key and Shift+F10.
        signal menuRequested(real x, real y)

        TapHandler {
            enabled: entry.menuAccount > 0
            acceptedButtons: Qt.RightButton
            onTapped: point => entry.menuRequested(point.position.x, point.position.y)
        }
        TapHandler {
            id: entryLongPress
            enabled: entry.menuAccount > 0
            acceptedDevices: PointerDevice.TouchScreen | PointerDevice.Stylus
            onPressedChanged: {
                if (entryLongPress.pressed) {
                    drawer.menuWanted = true;
                }
            }
            onLongPressed: entry.menuRequested(entryLongPress.point.position.x, entryLongPress.point.position.y)
        }
        Keys.onMenuPressed: entry.menuRequested(-1, -1)
        Keys.onPressed: event => {
            if (entry.menuAccount > 0 && event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
                entry.menuRequested(-1, -1);
                event.accepted = true;
            }
        }
        // The menu is made before it is first asked for (a menu opened in the moment it
        // was made can come out the size of its padding), but not at the start.
        onHoveredChanged: {
            if (entry.hovered && entry.menuAccount > 0) {
                drawer.menuWanted = true;
            }
        }
        onActiveFocusChanged: {
            if (entry.activeFocus && entry.menuAccount > 0) {
                drawer.menuWanted = true;
            }
        }

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
                // Which device of its identity it is, when another account looks the same
                // (two links of one identity from an older profile); empty otherwise.
                required property string device

                label: name
                // Which account it is: its number where the network has one (the badge on
                // the picture says which network), then its state.
                detail: (number !== "" ? number : networkName) + (device !== "" ? " · " + device : "") + " · " + statusLabel
                avatarName: name
                badgeIcon: networkIcon
                stateName: status
                unreadCount: unread
                highlighted: App.currentAccount === accountId
                menuAccount: accountId
                onMenuRequested: (x, y) => drawer.openAccountMenu(this, accountId, x, y)
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
