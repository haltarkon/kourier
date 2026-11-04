pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.delegates as Delegates
import app.kourier

// "All chats", one entry per account (avatar, network badge, state dot), "Add account" and
// "Settings". A slim rail beside the list when the window is wide, a modal drawer when narrow.
//
// Blended chats: which accounts' chats the list shows. A click shows that account alone;
// Shift+click, Ctrl+click, Ctrl+Space, the entry's box (where names show) and the menu's
// "Show in List" add or remove it (with the setting "Show chats of several accounts together"
// off they are a click). Shown accounts are tinted ("All Chats" when every account is shown),
// hidden ones faded; at least one account stays shown. The rules live in the view model
// (`ShownAccounts`); this file only asks `App.selectAccount` / `App.toggleAccount`.
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
    // LATE when it turns false. The last account leaving changes the page behind the
    // drawer (the welcome page replaces the list); a drawer that turned from a rail into a
    // modal, disabled one in that same moment would change its shape under it. A UI choice,
    // no guard against a crash: the account entries sit in a Column below, so that the
    // delay does not decide whether a layout still names a freed entry (it once did: the
    // flip in the moment of the deletion, `transfer_review_ui` case `wide-many`, and the
    // same read in a window hidden in the tray, `account_actions_ui` case `trayed`).
    // MainWindow's `enabled` follows this too. An account arriving is followed at once.
    // It is set once when the drawer is made and then only by the handler below, never a
    // binding to `App.hasAccounts` (`link_actions_ui` case `drawer` holds that: a drawer
    // made while the accounts are already there follows the last one's leaving late too).
    property bool accountsShown: false
    Component.onCompleted: {
        drawer.accountsShown = App.hasAccounts;
        drawer.severalAccounts = drawer.severalNow;
    }
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

    // Two or more accounts: the boxes, the menu's choice items and the shown/hidden words. Late
    // like `accountsShown` when it turns false (the box of the leaving account's neighbour must
    // not change the entries' shape in the moment an entry is deleted), at once when it turns true.
    readonly property bool severalNow: accountEntries.count > 1
    property bool severalAccounts: false
    onSeveralNowChanged: {
        if (drawer.severalNow) {
            severalFollow.stop();
            drawer.severalAccounts = true;
        } else {
            severalFollow.restart();
        }
    }
    Timer {
        id: severalFollow
        interval: 300
        onTriggered: drawer.severalAccounts = drawer.severalNow
    }
    // The choice of several accounts is offered: the setting on and two or more accounts.
    readonly property bool choosing: App.blend && drawer.severalAccounts
    // Boxes beside the accounts where the names show.
    readonly property bool boxes: drawer.choosing && !drawer.rail
    // Said when the last shown account would be taken out (the greyed box's tooltip, the rail's
    // tooltip, the passive notification of Shift/Ctrl+click, Ctrl+Space and the menu item).
    readonly property string lastStaysText: L10n.i18nc("@info", "At least one account stays in the list")
    // The last shown account was asked to go and stays (tests count it).
    signal lastAccountStays

    // A click on an account (and Shift/Ctrl+click with the setting off): only it; a modal
    // drawer closes.
    function pickAccount(account: real): void {
        App.selectAccount(account);
        if (drawer.modal) {
            drawer.close();
        }
    }

    // Shift+click, Ctrl+click, Ctrl+Space, the box, "Show in List": the account joins the list or
    // leaves it, the drawer stays open. With the setting off: a click (L9).
    // With the choice not offered (the setting off, ONE account, or the moment before a second
    // account counts) it is the click: the blend rules start at two accounts (M12, UI-3).
    function toggleEntry(account: real): void {
        if (!drawer.choosing) {
            drawer.pickAccount(account);
            return;
        }
        if (!App.toggleAccount(account)) {
            drawer.tellLastStays();
        }
    }

    // The last shown account stays: said by a passive notification (Shift/Ctrl+click,
    // Ctrl+Space, "Show in List", a click or tap on its greyed box).
    function tellLastStays(): void {
        drawer.lastAccountStays();
        drawer.window.showPassiveNotification(drawer.lastStaysText, "short");
    }

    // The keyboard (KDE's sidebar pattern, UI-4): the panel is ONE Tab stop, the entry the list
    // follows (the first tinted one: "All Chats" when every account is shown), else "All Chats";
    // Up and Down move between the entries ("All Chats", the accounts, "Add Account",
    // "Settings"); Return, Enter and Space click the focused entry.
    readonly property Item tabEntry: {
        if (allChatsEntry.highlighted) {
            return allChatsEntry;
        }
        for (let i = 0; i < accountEntries.count; i++) {
            const e = accountEntries.itemAt(i) as Entry;
            if (e !== null && e.highlighted) {
                return e;
            }
        }
        return allChatsEntry;
    }
    function entryList(): var {
        const list = [allChatsEntry];
        for (let i = 0; i < accountEntries.count; i++) {
            const e = accountEntries.itemAt(i);
            if (e !== null) {
                list.push(e);
            }
        }
        list.push(addAccountEntry, settingsEntry);
        return list;
    }
    function moveFocus(from: Item, step: int): void {
        const list = drawer.entryList();
        const next = list[list.indexOf(from) + step];
        if (next !== undefined && next.visible) {
            next.forceActiveFocus(Qt.TabFocusReason);
        }
    }

    // The account entry at `index` of the panel, and "All Chats" (tests).
    function accountEntry(index: int): Item {
        return accountEntries.itemAt(index);
    }
    function allChats(): Item {
        return allChatsEntry;
    }

    // The narrow window's count of unread messages in hidden accounts sits on the closed menu
    // button, drawn by the list page's title (`ConversationListPage` `chatsHeading`, UI-1); the
    // handle's tooltip says it too.
    handleClosedToolTip: App.hiddenUnread > 0 ? L10n.i18ncp("@info:tooltip, %1 how many", "Open the side panel · %1 unread message in hidden accounts", "Open the side panel · %1 unread messages in hidden accounts", App.hiddenUnread) : L10n.i18nc("@info:tooltip", "Open the side panel")
    handleOpenToolTip: L10n.i18nc("@info:tooltip", "Close the side panel")

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
            // Set at popup (M6): whether the account is shown, and whether the choice items show.
            property bool listed: false
            property bool choosing: false

            popupType: QQC2.Popup.Item

            // A checkable item flips its own `checked` when triggered, which would break a
            // binding: it is set each time the menu opens.
            onAboutToShow: showInList.checked = accountMenu.listed

            QQC2.MenuItem {
                id: showInList
                objectName: "drawer-show-in-list"
                visible: accountMenu.choosing
                enabled: visible
                height: visible ? implicitHeight : 0
                checkable: true
                text: L10n.i18nc("@action:inmenu", "Show in List")
                onTriggered: drawer.toggleEntry(accountMenu.account)
            }
            QQC2.MenuItem {
                objectName: "drawer-show-only"
                visible: accountMenu.choosing
                enabled: visible
                height: visible ? implicitHeight : 0
                text: L10n.i18nc("@action:inmenu", "Show Only This Account")
                onTriggered: drawer.pickAccount(accountMenu.account)
            }
            QQC2.MenuSeparator {
                visible: accountMenu.choosing
                height: visible ? implicitHeight : 0
            }

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
        menu.listed = actions.shown === true;
        menu.choosing = drawer.choosing;
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
        // The panel's one Tab stop (`drawer.tabEntry`); Up and Down reach the others.
        focusPolicy: drawer.tabEntry === entry ? Qt.TabFocus : Qt.NoFocus
        // The key of the action, as the platform writes it (shown in the tooltip).
        property string keys: ""
        // The entry of an account: it has a context menu (`accountMenu`).
        property real menuAccount: 0
        // An account's: whether the list shows its chats, its place among its network's accounts
        // (the number on the badge, 0 = none).
        property bool shownInList: true
        property int badgeNumber: 0
        // A hidden account's picture is faded (its unread pill is not).
        // Only while the choice is offered: with the setting off the panel looks as before (UI-6).
        readonly property bool faded: entry.menuAccount > 0 && drawer.choosing && !entry.shownInList
        // The one account still shown while several could be: it cannot be taken out.
        readonly property bool lastShownHere: entry.menuAccount > 0 && drawer.choosing && entry.shownInList && App.shownCount === 1
        property string accessibleNote: ""
        // A Shift or Ctrl press is under way or just ended: the click the delegate may still make of
        // it is not "only it" (`modifierClick`).
        property bool modifierPress: false
        function notePress(pressed: bool): void {
            if (pressed) {
                entry.modifierPress = true;
            } else {
                Qt.callLater(() => entry.modifierPress = false);
            }
        }
        // True (once) when the click came from a Shift/Ctrl press the handlers took.
        function modifierClick(): bool {
            const was = entry.modifierPress;
            entry.modifierPress = false;
            return was;
        }

        // Right click, a long press on a touch screen, the Menu key and Shift+F10.
        signal menuRequested(real x, real y)

        // Shift+click and Ctrl+click: one handler each, since `acceptedModifiers` is an exact
        // match (H1). PASSIVE (the default drag threshold): measured in `accounts_shown_ui`, a
        // `ReleaseWithinBounds` handler lost its grab to the delegate's own press (the delegate
        // clicked "only it", the handler was cancelled and never tapped). So the handler taps and
        // the delegate's click that follows is dropped (`modifierClick`).
        TapHandler {
            objectName: "entryShiftTap"
            enabled: entry.menuAccount > 0
            acceptedButtons: Qt.LeftButton
            acceptedModifiers: Qt.ShiftModifier
            onPressedChanged: entry.notePress(pressed)
            onTapped: drawer.toggleEntry(entry.menuAccount)
        }
        TapHandler {
            objectName: "entryCtrlTap"
            enabled: entry.menuAccount > 0
            acceptedButtons: Qt.LeftButton
            acceptedModifiers: Qt.ControlModifier
            onPressedChanged: entry.notePress(pressed)
            onTapped: drawer.toggleEntry(entry.menuAccount)
        }
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
        Keys.onPressed: event => entry.pressKey(event)
        Keys.onReleased: event => entry.releaseKey(event)
        // The keys (a function of their own so a test can hand it a stand-in auto-repeat, which
        // `TestEvent` cannot send). Every repeat of a held key is taken (but Tab and Backtab): a
        // held Space, or the Space of a Ctrl+Space whose Ctrl was let go first, would press and
        // click the entry again and again ("only it", qml.md). Ctrl+Space adds or removes the
        // account; Space (the style's own key click), Return and Enter stay the plain click; Up
        // and Down move to the entry before or after (the panel is one Tab stop).
        function pressKey(event: var): void {
            if (event.isAutoRepeat && event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab) {
                event.accepted = true;
                return;
            }
            const plain = (event.modifiers & ~Qt.KeypadModifier) === Qt.NoModifier;
            if (plain && (event.key === Qt.Key_Return || event.key === Qt.Key_Enter)) {
                entry.clicked();
                event.accepted = true;
            } else if (plain && (event.key === Qt.Key_Down || event.key === Qt.Key_Up)) {
                drawer.moveFocus(entry, event.key === Qt.Key_Down ? 1 : -1);
                event.accepted = true;
            } else if (entry.menuAccount > 0 && event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
                entry.menuRequested(-1, -1);
                event.accepted = true;
            } else if (entry.menuAccount > 0 && event.key === Qt.Key_Space && (event.modifiers & Qt.ControlModifier)) {
                drawer.toggleEntry(entry.menuAccount);
                event.accepted = true;
            }
        }
        function releaseKey(event: var): void {
            if (event.isAutoRepeat && event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab) {
                event.accepted = true;
            }
        }
        Accessible.description: entry.accessibleNote
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

        // The rail's tooltip names the entry; on the last shown account it also says why it
        // stays (where names show, the greyed box says it on its own hover, UI-7).
        QQC2.ToolTip.visible: entry.toolTipShown
        QQC2.ToolTip.text: entry.toolTipText
        readonly property bool toolTipShown: (drawer.rail || entry.keys !== "") && entry.hovered
        readonly property string toolTipText: {
            const base = drawer.rail || entry.keys !== "" ? Style.plain((drawer.rail && entry.detail !== "" ? entry.label + " · " + entry.detail : entry.label) + (entry.keys !== "" ? " (" + entry.keys + ")" : "")) : "";
            if (!entry.lastShownHere || !drawer.rail) {
                return base;
            }
            return base + "\n" + drawer.lastStaysText;
        }
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
                    // A hidden account: faded with its badge and dot (plain opacity, as in his
                    // mock "1: tint + fade"); the unread pill beside it keeps full strength.
                    opacity: entry.faded ? 0.4 : 1
                    name: entry.avatarName
                    source: entry.avatarUrl
                    badge: entry.badgeIcon
                    badgeNumber: entry.badgeNumber
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

            // The box (where names show, the setting on, two or more accounts): ticked = shown.
            // Not a Tab stop: the entry is (Ctrl+Space). Made only while it shows (a Loader: an
            // entry that can never show one builds nothing, cost M1). The last shown box is
            // greyed; the guard over it keeps its click from the entry (which would close a modal
            // drawer, L8), says why on its own hover (a disabled box gets none) and tells on a
            // click or tap (UI-7).
            Loader {
                active: entry.menuAccount > 0 && drawer.boxes
                visible: active

                sourceComponent: Item {
                    implicitWidth: entryBox.implicitWidth
                    implicitHeight: entryBox.implicitHeight

                    QQC2.CheckBox {
                        id: entryBox
                        objectName: "entryBox"
                        anchors.centerIn: parent
                        focusPolicy: Qt.NoFocus
                        checked: entry.shownInList
                        enabled: !entry.lastShownHere
                        Accessible.name: L10n.i18nc("@option:check, %1 an account's name", "Show %1 in the list", entry.label)
                        onToggled: {
                            drawer.toggleEntry(entry.menuAccount);
                            entryBox.checked = Qt.binding(() => entry.shownInList);
                        }
                    }
                    MouseArea {
                        id: entryBoxGuard
                        objectName: "entryBoxGuard"
                        // (Tests read these.)
                        readonly property bool tipShown: entryBoxGuard.enabled && entryBoxGuard.containsMouse
                        readonly property string tipText: drawer.lastStaysText
                        anchors.fill: parent
                        enabled: !entryBox.enabled
                        hoverEnabled: true
                        acceptedButtons: Qt.LeftButton
                        onClicked: drawer.tellLastStays()
                        QQC2.ToolTip.visible: entryBoxGuard.tipShown
                        QQC2.ToolTip.text: entryBoxGuard.tipText
                        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    }
                }
            }
        }
    }

    topContent: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing / 2
        Layout.fillWidth: true
        Layout.topMargin: Kirigami.Units.smallSpacing

        Entry {
            id: allChatsEntry
            label: L10n.i18nc("@action:button all conversations", "All Chats")
            iconName: "dialog-messages"
            // Tinted when every account is shown (with one account: as before, M12).
            highlighted: App.currentAccount < 0 && App.allShown
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

        // The accounts sit in a Column of their own, never straight in this layout. A Qt
        // Quick Layout keeps a removed child in its engine until its next polish, and the
        // Repeater deletes the entry before that (a window that is hidden in the tray never
        // polishes): the next size read of the drawer, Kirigami's `Layout.minimumHeight` of
        // its main column, then walks the freed entry (a use-after-free that crashed the
        // application, `account_actions_ui` case `trayed`). A positioner drops a removed
        // child at once. Nothing that the Repeater makes or deletes may be a direct child
        // of a layout here.
        Column {
            visible: accountEntries.count > 0
            spacing: Kirigami.Units.smallSpacing / 2
            Layout.fillWidth: true

            Repeater {
                id: accountEntries
                model: App.accounts
                delegate: Entry {
                    // (The Column does not stretch its children: Entry's own
                    // `Layout.fillWidth` does nothing in it.)
                    width: parent ? parent.width : 0
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
                    required property bool shown
                    required property int place

                    label: name
                    // Which account it is: its number where the network has one (the badge on
                    // the picture says which network), then its state.
                    detail: (number !== "" ? number : networkName) + (device !== "" ? " · " + device : "") + " · " + statusLabel
                    avatarName: name
                    badgeIcon: networkIcon
                    stateName: status
                    unreadCount: unread
                    shownInList: shown
                    badgeNumber: place
                    // The account shown alone (a click), or one of several shown.
                    highlighted: App.currentAccount === accountId || (shown && !App.allShown)
                    menuAccount: accountId
                    accessibleNote: !drawer.choosing ? "" : (shown ? L10n.i18nc("@info accessible", "Shown in the list. Ctrl+Space shows or hides it.") : L10n.i18nc("@info accessible", "Hidden from the list. Ctrl+Space shows or hides it."))
                    onMenuRequested: (x, y) => drawer.openAccountMenu(this, accountId, x, y)
                    onClicked: {
                        if (!modifierClick()) {
                            drawer.pickAccount(accountId);
                        }
                    }
                }
            }
        }

        Entry {
            id: addAccountEntry
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
            id: settingsEntry
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
