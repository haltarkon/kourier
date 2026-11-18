pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The main window. `Main.qml` creates it, hides it to the tray, releases it after a minute
// hidden and creates it again on activation (§8.2 step 6, §8.8).
Kirigami.ApplicationWindow {
    id: root

    // The start page is chosen once the first answers arrived (App.loaded).
    property bool started: false
    // A tray icon is shown: closing the window hides it there (§8.8).
    property bool trayAvailable: false
    // Quit was chosen: closing really closes.
    property bool quitting: false
    // When the window last lost focus (ms): a tray click right after it still means "hide".
    property real deactivatedAt: 0
    // A chat was opened before the restore of the last run's chats ran (a notification, a link, a click):
    // the restore then opens nothing.
    property bool chatOpenedEarly: false
    // The restore asked for the chats of the last run once (split view S4).
    property bool restoreAsked: false
    property int restoreTries: 0
    // A conversation to open once the start page is up (activation before App.loaded).
    property real pendingConversation: 0
    // The message of that conversation to open at (0: none).
    property real pendingMessage: 0

    // The window went to the tray (closed while a tray icon is shown).
    signal hiddenToTray()
    // Closing without a tray, or the Quit key: the application should quit.
    signal closedForGood()
    // The chat column is on screen with a conversation (read markers, notifications).
    readonly property bool chatShown: root.activeChat !== null && root.activeChat.chatModel.conversationId !== 0 && root.activeChat.Kirigami.ColumnView.inViewport && !root.activeChat.panelCovers && (root.wideScreen || root.pageStack.currentItem === root.activeChat)
    // What the keys Ctrl+Shift+A and Ctrl+Shift+U need (A10d, H2): the chat is shown AND its page is a page of
    // the stack. A wide window with Settings over the chat keeps the page counted "in the viewport" after it
    // left the stack (`chatShown` stays true; found by `roaming_ui` `keys`), and a key must not archive a
    // chat nobody sees.
    readonly property bool chatKeys: root.chatShown && root.inStack(root.activeChat)
    // The first frame was presented.
    property bool framed: false
    // The chat page takes its column right after the first frame (see `showListWith`).
    property bool chatPending: false
    // What stands in the chat column of a wide window until then.
    property Kirigami.Page standIn: null

    title: root.activeTimeline.conversationId !== 0 && root.activeTimeline.title !== "" ? root.activeTimeline.title + " — Kourier" : "Kourier"
    width: App.windowWidth > 0 ? App.windowWidth : Kirigami.Units.gridUnit * 64
    height: App.windowHeight > 0 ? App.windowHeight : Kirigami.Units.gridUnit * 42
    minimumWidth: Kirigami.Units.gridUnit * 20
    minimumHeight: Kirigami.Units.gridUnit * 22

    // Wide: conversation list and chat side by side (§8.3).
    wideScreen: width >= Kirigami.Units.gridUnit * 46

    // ---- the layout of a split (split view S4) ----
    // A chat column needs at least 20 grid units. With the split on the window has three layouts, by the
    // room the page stack has (the window less a side panel that stands beside it):
    //   "full": the list (21 gu) and two chats fit (61 gu): list | chat | chat;
    //   "rail": the list narrowed to a rail of avatars (4 gu) and two chats fit (44 gu): rail | chat | chat;
    //   "one":  else the active chat alone (with the list beside it when the window is wide, alone when not),
    //           the other chat one press away (F6, the header's "Show Other Chat").
    // ONE rule, here; everything that asked `wideScreen` for the split asks `bothShown` or `splitLayout`.
    readonly property real splitChatMin: Kirigami.Units.gridUnit * 20
    readonly property real splitRoom: root.pageStack.columnView.width > 0 ? root.pageStack.columnView.width : root.width
    readonly property string splitLayout: root.splitRoom >= listPage.fullWidth + 2 * root.splitChatMin ? "full" : (root.splitRoom >= listPage.railWidth + 2 * root.splitChatMin ? "rail" : "one")
    // Both chats stand in the stack (a window without a split shows what `wideScreen` says).
    readonly property bool bothShown: root.wideScreen && (!App.split || root.splitLayout !== "one")
    // The width of the first chat's column: its share of what the list leaves. A share, not pixels: a
    // window resized keeps it.
    // (Whole pixels, rounded down: the two columns of an equal split are the same width, as the page row's own
    // default widths were.)
    readonly property real splitFirstExact: {
        const room = Math.max(0, root.splitRoom - (root.splitLayout === "rail" ? listPage.railWidth : listPage.fullWidth));
        return Math.max(root.splitChatMin, Math.min(room - root.splitChatMin, room * App.splitWidth));
    }
    readonly property real splitFirstWidth: Math.floor(root.splitFirstExact)
    // The second chat's: what is left.
    readonly property real splitSecondWidth: Math.max(root.splitChatMin, Math.floor(root.splitRoom - (root.splitLayout === "rail" ? listPage.railWidth : listPage.fullWidth) - root.splitFirstExact))
    onSplitLayoutChanged: Qt.callLater(root.syncSplit)
    // A drag of the divider ended: the first column's width is stored as a share of the room.
    function dividerReleased(width: real): void {
        const room = Math.max(1, root.splitRoom - (root.splitLayout === "rail" ? listPage.railWidth : listPage.fullWidth));
        App.storeSplitWidth(width / room);
    }
    pageStack.columnView.columnResizeMode: root.wideScreen ? Kirigami.ColumnView.DynamicColumns : Kirigami.ColumnView.SingleColumn
    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.ToolBar
    pageStack.globalToolBar.showNavigationButtons: Kirigami.ApplicationHeaderStyle.ShowBackButton
    // The toolbar is a fixed 46 px high in Kirigami; the chat's title and subtitle (two lines of
    // the theme's default and small fonts) need more once the font is larger.
    pageStack.globalToolBar.preferredHeight: Math.max(46, Math.ceil(headerTitleMetrics.height + headerSubtitleMetrics.height) + Kirigami.Units.smallSpacing * 2)

    FontMetrics {
        id: headerTitleMetrics
        font: Kirigami.Theme.defaultFont
    }
    FontMetrics {
        id: headerSubtitleMetrics
        font: Kirigami.Theme.smallFont
    }

    globalDrawer: AccountsDrawer {
        id: accountsDrawer
        window: root
        // Follows the drawer's `accountsShown`: a moment late when the last account goes
        // (a UI choice, see there).
        enabled: accountsDrawer.accountsShown
        settingsKeys: root.settingsKeys
        onOpenSettings: root.leavingLink(root.openSettings)
        onAddAccount: root.leavingLink(root.openWelcome)
    }

    // Pages are single instances; the stack shows them in columns (wide) or one at a time.
    // The chat page's model switches conversations (§8.3). Only the conversation list is
    // created with the window (§1.3: the first frame shows the list and waits for nothing
    // else); the chat page, the welcome and link pages and the settings are created when
    // they are first shown and stay from then on.
    ConversationListPage {
        id: listPage
        visible: false
        // In a narrow window the list is on screen only while the chat is not.
        rail: App.split && root.splitLayout === "rail"
        showsNotice: !root.wideScreen || root.activeTimeline.conversationId === 0 || root.activeTimeline.cannotSend !== ""
        searchKeys: root.searchKeys
        onOpenConversation: id => root.pickedChat(id)
        onOpenSearchHit: (id, message) => root.pickedHit(id, message)
        choosing: root.choosingBeside
        choosingName: root.choosingName
        onChooseCancelled: {
            root.endChoosing();
            listPage.conversations.forceActiveFocus(Qt.TabFocusReason);
        }
        onOpenBesideAsked: id => root.askedBeside(id)
        onCloseChatAsked: id => root.closeChat(App.paneOf(id))
        onSwapChatsAsked: root.swapChats(false)
        onArchiveAsked: conv => root.archiveChat(conv)
        onUnarchiveAsked: conv => root.unarchiveChat(conv)
        onUntilAsked: (conv, back) => root.askMuteUntil(conv, back)
        onSwitchedByKeyboard: id => root.switchTo(id)
        onBannerActivated: root.bannerPressed()
    }

    property ChatPage chatPage: null
    // The second chat page, the column after the first one: made while `App.split` is on (`syncSplit`).
    property ChatPage sideChatPage: null
    property int pageSetupSplitTries: 0
    // The page `splitpick` started its chooser (once).
    property bool pageSetupPickStarted: false
    // The chat page the window's chat keys act on: the active pane's (`App.activePane`). The first page
    // alone without a split.
    readonly property ChatPage activeChat: App.activePane === 1 && root.sideChatPage !== null ? root.sideChatPage : root.chatPage
    // The models of that chat (pane 0's while no chat page exists yet).
    readonly property TimelineModel activeTimeline: root.activeChat !== null ? root.activeChat.chatModel : App.timeline
    readonly property ComposerController activeComposer: root.activeChat !== null ? root.activeChat.composerModel : App.composer
    property WelcomePage welcomePage: null
    property LinkAccountPage linkPage: null
    property SettingsPage settingsPage: null
    // The warning before a new account of an experimental network (made anew each time).
    property RiskNoticePage riskPage: null

    Component {
        id: chatComponent
        ChatPage {
            visible: false
            chatModel: App.timeline
            composerModel: App.composer
            pane: 0
            active: App.activePane === 0
            beside: App.split && root.bothShown
            columnWidth: root.splitFirstWidth
            single: App.split && !root.bothShown
            otherName: App.sideTimeline.title
            onDividerReleased: width => root.dividerReleased(width)
            // A group in common (or "Message") on the details panel opens that chat.
            onConversationRequested: id => root.pickedChat(id)
            onArchiveAsked: conv => root.archiveChat(conv)
            onUnarchiveAsked: conv => root.unarchiveChat(conv)
            onUntilAsked: (conv, back) => root.askMuteUntil(conv, back)
            onSearchAsked: root.searchInChat()
            onChooseBesideAsked: root.startChoosing()
            onSwapChatsAsked: root.swapChats(true)
            onCloseChatAsked: pane => root.closeChat(pane)
            onForwardRefused: (count, chat) => root.forwardRefused(count, chat)
        }
    }
    // The second chat column of a split: the same page with pane 1's models.
    Component {
        id: sideChatComponent
        ChatPage {
            visible: false
            chatModel: App.sideTimeline
            composerModel: App.sideComposer
            pane: 1
            active: App.activePane === 1
            beside: App.split && root.bothShown
            columnWidth: root.splitSecondWidth
            single: App.split && !root.bothShown
            otherName: App.timeline.title
            onConversationRequested: id => root.pickedChat(id)
            onArchiveAsked: conv => root.archiveChat(conv)
            onUnarchiveAsked: conv => root.unarchiveChat(conv)
            onUntilAsked: (conv, back) => root.askMuteUntil(conv, back)
            onSearchAsked: root.searchInChat()
            onChooseBesideAsked: root.startChoosing()
            onSwapChatsAsked: root.swapChats(true)
            onCloseChatAsked: pane => root.closeChat(pane)
            onForwardRefused: (count, chat) => root.forwardRefused(count, chat)
        }
    }
    // The chat column of the first frame: the chat page's empty state without the chat page
    // (its timeline, composer and menus), so the columns stand where they will stay and the
    // list does not jump when the real page arrives.
    Component {
        id: standInComponent
        Kirigami.Page {
            visible: false
            padding: 0
            Kirigami.ColumnView.fillWidth: true
            Kirigami.Theme.colorSet: Kirigami.Theme.View
            Kirigami.Theme.inherit: false
            background: Rectangle {
                color: Kirigami.Theme.backgroundColor
            }
            ChatEmptyState {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
            }
        }
    }
    Component {
        id: welcomeComponent
        WelcomePage {
            id: welcome
            visible: false
            alone: root.pageStack.lastItem === welcome
            onLinkNetwork: network => root.openLink(network)
        }
    }
    Component {
        id: linkComponent
        LinkAccountPage {
            visible: false
            onCancelled: root.leaveLink()
        }
    }
    Component {
        id: riskComponent
        RiskNoticePage {
            visible: false
            onCancelled: root.leaveRisk()
            onAccepted: network => root.acceptRisk(network)
        }
    }
    Component {
        id: settingsComponent
        SettingsPage {
            visible: false
        }
    }

    function ensureChat(): ChatPage {
        if (root.chatPage === null) {
            root.chatPage = chatComponent.createObject(root) as ChatPage;
        }
        return root.chatPage;
    }

    // ---- the split: list | chat | chat ----
    // Whether `item` is `page` or below it.
    function within(item: Item, page: Item): bool {
        for (let i = item; i !== null && i !== undefined; i = i.parent) {
            if (i === page) {
                return true;
            }
        }
        return false;
    }

    // Where `page` stands in the stack, -1 when it is not there.
    function stackIndex(page: Item): int {
        for (let i = 0; page !== null && i < pageStack.depth; ++i) {
            if (pageStack.get(i) === page) {
                return i;
            }
        }
        return -1;
    }

    // The second chat page stands in the stack right after the first one while the split is on in a wide
    // window; a narrow window shows the active chat alone (`showActive`). The page is destroyed (its
    // composer text saved as the draft first, as `freshChat` does) when the split ends. Called wherever the
    // stack of the chat column is rebuilt.
    function syncSplit(): void {
        if (App.split) {
            root.dragCarrier = true;
        }
        if (!App.split) {
            const shown = !root.wideScreen && root.sideChatPage !== null && pageStack.lastItem === root.sideChatPage;
            root.dropSide(true);
            if (shown && App.timeline.conversationId !== 0) {
                // A narrow window that showed the closed pane shows the chat that stays.
                pageStack.push(root.freshChat());
                pageStack.currentIndex = pageStack.depth - 1;
            }
            return;
        }
        if (!root.bothShown) {
            root.showActive();
            return;
        }
        const chatAt = root.stackIndex(root.chatPage);
        const sideAt = root.stackIndex(root.sideChatPage);
        if (chatAt < 0) {
            if (sideAt >= 0) {
                // Wide again after a narrow window showed the second chat alone: the first goes before it.
                pageStack.insertPage(sideAt, root.freshChat());
            }
            // (No chat column now, as under the stand-in: the split waits for it.)
            return;
        }
        if (sideAt < 0) {
            pageStack.insertPage(chatAt + 1, root.freshSide());
        }
    }

    // The second chat page to push: one that left the stack is made anew (as `freshChat` does).
    function freshSide(): ChatPage {
        if (root.sideChatPage !== null && !root.inStack(root.sideChatPage)) {
            root.sideChatPage.composer.switchTo(0);
            root.sideChatPage.destroy();
            root.sideChatPage = null;
        }
        if (root.sideChatPage === null) {
            root.sideChatPage = sideChatComponent.createObject(root) as ChatPage;
        }
        return root.sideChatPage;
    }

    // The side page leaves the stack; `destroy`: the split is over, the page goes.
    function dropSide(destroy: bool): void {
        const side = root.sideChatPage;
        if (side === null) {
            return;
        }
        if (root.inStack(side)) {
            pageStack.removePage(side);
        }
        if (destroy) {
            side.composer.switchTo(0);
            side.destroy();
            root.sideChatPage = null;
        }
    }

    // A narrow window shows the active chat only: while a chat is what it shows (not the list alone, not a
    // page over the chat), the chat page after the list is the active pane's and the other is not in the stack.
    function showActive(): void {
        if (root.bothShown || root.chatPage === null) {
            return;
        }
        const last = pageStack.lastItem;
        if (pageStack.depth < 2 || last === null || (last !== root.chatPage && last !== root.sideChatPage)) {
            return;
        }
        const second = App.split && App.activePane === 1;
        const other = second ? root.chatPage : root.sideChatPage;
        if (other !== null && root.inStack(other)) {
            pageStack.removePage(other);
        }
        const want = second ? root.freshSide() : root.freshChat();
        if (pageStack.lastItem !== want) {
            pageStack.push(want);
        }
        pageStack.currentIndex = pageStack.depth - 1;
    }

    // F6 and Shift+F6: the keyboard goes list, first chat, second chat (the timeline's rows, the first stop
    // of the chat), and round; without a split the list and the chat take turns.
    function cycleFocus(back: bool): void {
        if (!root.bothShown) {
            // A split that shows one chat: F6 and Shift+F6 switch to the other one.
            if (App.split && root.splitLayout === "one" && root.activeChat !== null && root.activeChat.open) {
                App.activatePane(1 - App.activePane);
                Qt.callLater(() => root.activeChat?.timeline.rowList.forceActiveFocus(Qt.TabFocusReason));
            }
            return;
        }
        const stops = [listPage];
        if (root.chatPage !== null && root.chatPage.open) {
            stops.push(root.chatPage);
        }
        if (App.split && root.sideChatPage !== null && root.sideChatPage.open && root.inStack(root.sideChatPage)) {
            stops.push(root.sideChatPage);
        }
        let at = -1;
        const focused = root.activeFocusItem;
        for (let i = 0; i < stops.length; ++i) {
            if (focused && root.within(focused, stops[i])) {
                at = i;
            }
        }
        const next = stops[(at + (back ? stops.length - 1 : 1) + stops.length) % stops.length];
        if (next === listPage) {
            listPage.conversations.forceActiveFocus(Qt.TabFocusReason);
        } else {
            next.timeline.rowList.forceActiveFocus(Qt.TabFocusReason);
        }
    }

    // The keyboard entering a pane makes it the active one ("focus decides"; a press does it in `ChatPage`).
    // Only in a wide window: a narrow one shows the active pane's page alone (`showActive`), so the page that
    // takes the keyboard there follows the active pane and never decides it (the start pushes the first chat
    // page before the split shows its active one).
    onActiveFocusItemChanged: {
        const item = root.activeFocusItem;
        if (!App.split || item === null || !root.bothShown) {
            return;
        }
        if (root.sideChatPage !== null && root.within(item, root.sideChatPage)) {
            App.activatePane(1);
        } else if (root.chatPage !== null && root.within(item, root.chatPage)) {
            App.activatePane(0);
        }
    }

    Connections {
        target: App
        function onSplitChanged(): void {
            root.syncSplit();
            root.showActive();
            if (App.split) {
                Qt.callLater(() => root.activeChat?.composer.focusInput());
            }
        }
        function onActivePaneChanged(): void {
            root.showActive();
        }
    }

    // Whether the pane that is not active is on screen (the core's notification rule).
    readonly property ChatPage inactiveChat: App.activePane === 1 ? root.chatPage : root.sideChatPage
    readonly property bool inactiveShown: App.split && root.inactiveChat !== null && root.inactiveChat.visible && root.inactiveChat.Kirigami.ColumnView.inViewport
    onInactiveShownChanged: App.setSideShown(root.inactiveShown)

    function ensureWelcome(): WelcomePage {
        if (root.welcomePage === null) {
            root.welcomePage = welcomeComponent.createObject(root) as WelcomePage;
        }
        return root.welcomePage;
    }

    function ensureLink(): LinkAccountPage {
        if (root.linkPage === null) {
            root.linkPage = linkComponent.createObject(root) as LinkAccountPage;
        }
        return root.linkPage;
    }

    function ensureSettings(): SettingsPage {
        if (root.settingsPage === null) {
            root.settingsPage = settingsComponent.createObject(root) as SettingsPage;
        }
        return root.settingsPage;
    }

    // Pops every page after `page` (which stays).
    function popAfter(page: Kirigami.Page): void {
        while (pageStack.depth > 1 && pageStack.lastItem !== page) {
            pageStack.pop();
        }
    }

    // Whether `page` is one of the stack's pages now.
    function inStack(page: Item): bool {
        for (let i = 0; i < pageStack.depth; ++i) {
            if (pageStack.get(i) === page) {
                return true;
            }
        }
        return false;
    }

    // Down to the list, which stays. Popping and not clearing: a clear takes the list out of
    // the stack and puts it back, and Kirigami then evaluates the headers of pages that have
    // no page row (TypeErrors in NavigationButtons, ToolBarPageHeader warnings). Only a stack
    // that does not start with the list is built anew.
    function popToList(): void {
        if (pageStack.depth > 0 && pageStack.get(0) === listPage) {
            while (pageStack.depth > 1) {
                pageStack.pop();
            }
        } else {
            pageStack.clear();
            pageStack.push(listPage);
        }
    }

    // The pages below are pushed through these: a page that left the stack is made anew
    // first, because one that is pushed back makes Kirigami create its header while the page
    // has no page row (`Unable to assign [undefined] to bool`, ToolBarPageHeader.qml:61).
    // (The pages hold no state of their own that the new one lacks, but the chat page's
    // composer text: it is saved as the draft first, as when the conversation changes.)
    function pushWelcome(): void {
        if (root.welcomePage !== null && !root.inStack(root.welcomePage)) {
            root.welcomePage.destroy();
            root.welcomePage = null;
        }
        pageStack.push(root.ensureWelcome());
    }

    function pushLink(): void {
        if (root.linkPage !== null && !root.inStack(root.linkPage)) {
            root.linkPage.destroy();
            root.linkPage = null;
        }
        pageStack.push(root.ensureLink());
    }

    function pushSettings(): void {
        if (root.settingsPage !== null && !root.inStack(root.settingsPage)) {
            root.settingsPage.destroy();
            root.settingsPage = null;
        }
        pageStack.push(root.ensureSettings());
    }

    // The chat page to put into the stack: the existing one, or a new one when it left the
    // stack.
    function freshChat(): ChatPage {
        if (root.chatPage !== null && !root.inStack(root.chatPage)) {
            root.chatPage.composer.switchTo(0);
            root.chatPage.destroy();
            root.chatPage = null;
        }
        return root.ensureChat();
    }

    // The list, and beside it in a wide window the chat column. (Also called by name from
    // the development channel: no arguments.)
    function showList(): void {
        root.showListWith(false);
    }

    // `later`: nothing is to be shown in the chat column yet, so the chat page is created
    // after the first frame, which then comes sooner; until then a page with the same
    // empty state stands in its column (`attachChat`).
    function showListWith(later: bool): void {
        root.dropStandIn();
        // (After a link, the welcome and link pages are popped.)
        root.popToList();
        if (root.wideScreen) {
            if (later && !root.framed && root.chatPage === null) {
                root.chatPending = true;
                root.standIn = standInComponent.createObject(root) as Kirigami.Page;
                pageStack.push(root.standIn);
            } else {
                pageStack.push(root.freshChat());
            }
            pageStack.currentIndex = 0;
        }
        root.syncSplit();
        // In the stack the list has its size: its rows can come (with the first frame).
        listPage.showRows();
    }

    // The stand-in leaves the stack (when it is still the last page) and goes.
    function dropStandIn(): void {
        const page = root.standIn;
        if (page === null) {
            return;
        }
        root.standIn = null;
        if (pageStack.depth > 1 && pageStack.lastItem === page) {
            pageStack.pop();
        }
        page.destroy();
    }

    // The chat page takes its column in a wide window: in place of the stand-in, or beside
    // a list that is still alone.
    function attachChat(): void {
        root.chatPending = false;
        if (!App.hasAccounts || !root.wideScreen) {
            root.dropStandIn();
            return;
        }
        const page = root.standIn;
        if (page !== null && pageStack.depth === 2 && pageStack.lastItem === page) {
            root.standIn = null;
            // (`replace` acts on the current page and drops what follows it: with the list
            // current it would replace the list.)
            pageStack.currentIndex = 1;
            pageStack.replace(root.freshChat());
            pageStack.currentIndex = 0;
            page.destroy();
            root.syncSplit();
            return;
        }
        root.dropStandIn();
        if (pageStack.depth === 1 && pageStack.lastItem === listPage) {
            pageStack.push(root.freshChat());
            pageStack.currentIndex = 0;
        }
        root.syncSplit();
    }

    // Opens chat `id`. ONE argument: the dev control calls it by that signature
    // (`dev/exec.rs`, `invoke_qml_number`; `dev_control` `drives_the_fixture_ui`).
    function openConversation(id: real): void {
        root.openChatAt(id, 0);
    }

    // Opens chat `id`; at `message` when one is given (a hit of the message search): the chat
    // is then read around that message from the start, never its newest page first (which
    // would mark the chat read, decisions B1), and the page stack does what any open does.
    function openChatAt(id: real, message: real): void {
        root.chatOpenedEarly = true;
        // Opening a chat any way that is not a pick of the chooser ends the chooser.
        root.choosingBeside = false;
        if (message !== 0) {
            App.openSearchResult(id, message);
        } else {
            App.openConversation(id);
        }
        root.chatPending = false;
        root.dropStandIn();
        // Keep the chat page when it already follows the list. Otherwise pop down to the list
        // (a clear is what printed the NavigationButtons TypeErrors) and push a chat page:
        // one that left the stack is made anew (`freshChat`).
        const chat = root.chatPage;
        if (chat !== null && root.pageStack.depth > 1 && root.pageStack.get(1) === chat) {
            // (The second chat of a split stays.)
            root.popAfter(root.sideChatPage !== null && root.inStack(root.sideChatPage) ? root.sideChatPage : chat);
        } else {
            root.popToList();
            pageStack.push(root.freshChat());
        }
        root.syncSplit();
        pageStack.currentIndex = 1;
    }

    // The user chose chat `id` (a click on the list, the step keys, the quick switch, a
    // notification): in a wide window the list stands beside a link page, and opening a
    // chat rebuilds the stack, which cancels a link waiting at its QR code. Never silently
    // (F3): `leavingLink` asks first when something would be lost, and acts at once
    // otherwise. `then` runs after the chat is open; `message` (a search hit) is where it opens.
    function openingChat(id: real, then: var, message = 0, pick = false): void {
        // The chooser of "Open Chat Beside" stands: a PICK (list click, Return, the quick switch, a search
        // hit) opens beside; every other way to open a chat (`pick` false) just ends it (`openChatAt`).
        const beside = pick && root.choosingBeside;
        root.choosingBeside = false;
        root.leavingLink(() => {
            if (beside) {
                root.openBeside(id);
            } else {
                root.openChatAt(id, message);
            }
            if (then) {
                then();
            }
        });
    }

    // A chat of the list was picked (a click, Enter on a row): an archived chat that a search
    // found brings the archive view with it, with the chat highlighted in it (Signal
    // Desktop's rule). A notification does not (`openFromActivation`): it opens its chat and
    // leaves the list as it is.
    function pickedChat(id: real): void {
        root.openingChat(id, () => App.revealChat(id), 0, true);
    }

    // A hit of the message search was picked: its chat opens at the message and the list stays
    // as it is, as for a notification (decisions M4): no archive view, the search and its hits
    // stay.
    function pickedHit(id: real, message: real): void {
        root.openingChat(id, null, message, true);
    }

    // ---- the second chat beside the first (split view) ----
    // The chooser of the header's "Open Chat Beside": the list shows a banner and the NEXT chat picked
    // there opens beside the open one (`openingChat`'s `pick`). Escape, the banner's close button, F3 and
    // any other way of opening a chat end it; so does the open chat going away.
    property bool choosingBeside: false
    // The name the banner shows (the open chat's, as it was when the chooser started).
    property string choosingName: ""

    function startChoosing(): void {
        if (App.split || root.activeTimeline.conversationId === 0) {
            return;
        }
        root.choosingName = root.activeTimeline.title;
        root.choosingBeside = true;
        if (!root.wideScreen) {
            // A narrow window shows the list (as Back does) with the banner.
            root.popAfter(listPage);
            pageStack.currentIndex = 0;
        }
        Qt.callLater(() => listPage.conversations.forceActiveFocus(Qt.TabFocusReason));
    }

    function endChoosing(): void {
        root.choosingBeside = false;
    }

    // Chat `id` opens in the second pane (the row menu's "Open in Split View", a pick of the chooser) and
    // that pane is active; a chat a pane shows already is only activated (`App.openInSplit`).
    function openBeside(id: real): void {
        root.chatOpenedEarly = true;
        root.choosingBeside = false;
        App.openInSplit(id);
        root.chatPending = false;
        root.dropStandIn();
        if (!root.wideScreen && root.pageStack.depth > 0 && root.pageStack.get(0) === listPage) {
            // The list stood alone (the chooser): now the active chat alone.
            root.popToList();
            const want = App.split && App.activePane === 1 ? root.freshSide() : root.freshChat();
            pageStack.push(want);
            pageStack.currentIndex = pageStack.depth - 1;
        }
        root.syncSplit();
    }

    // Closes the chat in pane `pane` (the header's Close Chat, the list's hover button and row menu): one chat
    // closes into the placeholder, with the split the other chat stays alone (as the main one when the main
    // closed). The keyboard goes to the conversation list every time, whose current row is the chat that
    // stays (or none); a narrow window shows the list (a chat that stays is not pushed over it).
    function closeChat(pane: int): void {
        if (pane < 0) {
            return;
        }
        root.choosingBeside = false;
        App.closePane(pane);
        root.syncSplit();
        if (!root.wideScreen) {
            root.popAfter(listPage);
            pageStack.currentIndex = 0;
        }
        Qt.callLater(() => listPage.conversations.forceActiveFocus(Qt.TabFocusReason));
    }

    // The two chats exchange places. Each page saves its typed text as its chat's draft first; the chats'
    // pages then show the other draft when their timelines change chat (`Composer.switchTo`). `fromChat`: the
    // press came from a chat header, so the keyboard goes to the composer of the active chat (the pane that
    // had it now shows the other chat); from the list it stays where it is.
    function swapChats(fromChat: bool): void {
        if (!App.split) {
            return;
        }
        root.chatPage?.composer.keepDraft();
        root.sideChatPage?.composer.keepDraft();
        App.swapPanes();
        root.showActive();
        if (fromChat) {
            Qt.callLater(() => root.activeChat?.composer.focusInput());
        }
    }

    function askedBeside(id: real): void {
        root.leavingLink(() => root.openBeside(id));
    }

    // F3: with the split on it closes the inactive pane; off, a list row whose chat is not open opens
    // beside, else the chooser starts (or, standing, ends).
    function splitKey(): void {
        if (App.split) {
            root.choosingBeside = false;
            App.closePane(1 - App.activePane);
            return;
        }
        if (root.choosingBeside) {
            root.endChoosing();
            return;
        }
        if (App.timeline.conversationId === 0) {
            return;
        }
        const row = listPage.conversations.activeFocus ? listPage.conversations.currentItem as ConversationDelegate : null;
        if (row !== null && row.conversationId !== 0 && App.paneOf(row.conversationId) < 0) {
            root.askedBeside(row.conversationId);
            return;
        }
        root.startChoosing();
    }

    // A notification click or a link: open `id` now, or once the start page is up.
    // `message` (not 0): the chat opens at it with the flash, the way a search hit opens
    // (`openingChat`), the list and its search stay as they are.
    function openFromActivation(id: real, message = 0): void {
        if (id <= 0) {
            return;
        }
        if (root.started && App.hasAccounts) {
            root.openingChat(id, null, message);
        } else {
            root.pendingConversation = id;
            root.pendingMessage = message;
        }
    }

    function hideToTray(): void {
        root.hide();
        root.hiddenToTray();
    }

    function openWelcome(): void {
        root.attachOrDrop();
        if (!App.hasAccounts) {
            pageStack.clear();
        } else {
            root.popAfter(listPage);
        }
        root.pushWelcome();
    }

    // `network` was chosen on the welcome page. A network that asks first (WhatsApp) shows its
    // warning page; the link starts when the person accepts it. The window's one entry for a
    // link with the network alone: the welcome page, the tests and the dev control (which
    // invokes it by its signature, `openLink(QString)`) call it.
    function openLink(network: string): void {
        root.openLinkAs(network, false);
    }

    // `warned`: the tick of the warning counted for this link already (a restart that turned
    // the network on opens its link page by itself, "Turn On and Restart").
    function openLinkAs(network: string, warned: bool): void {
        // Pop an open link page first: that abandons its login (onPageRemoved below).
        root.popAfter(root.ensureWelcome());
        const risk = warned ? ({}) : App.networkRisk(network);
        if (risk.title !== undefined) {
            root.pushRisk(network, risk);
            return;
        }
        App.login.start(network);
        root.pushLink();
    }

    // The warning page, made anew (its tick box starts unticked every time).
    function pushRisk(network: string, risk: var): void {
        // (The caller popped every page after the welcome page, the old warning included.)
        if (root.riskPage !== null && !root.inStack(root.riskPage)) {
            root.riskPage.destroy();
            root.riskPage = null;
        }
        root.riskPage = riskComponent.createObject(root, {
            "network": network,
            "view": risk
        }) as RiskNoticePage;
        pageStack.push(root.riskPage);
    }

    // Back from the warning: the welcome page again; nothing was started, so nothing is asked.
    function leaveRisk(): void {
        root.popAfter(root.ensureWelcome());
    }

    // The person ticked the box and went on. A network that is off is turned on first and
    // Kourier starts again (the warning counts for the link that opens by itself after the
    // restart); one that is on starts its link now.
    function acceptRisk(network: string): void {
        for (let i = 0; i < App.networks.length; ++i) {
            if (App.networks[i].kind === network && (!App.networks[i].on || (App.networks[i].experimental && !Style.networkWanted(network)))) {
                root.restartAsking(() => App.turnOnNetworkAndRestart(network, true));
                return;
            }
        }
        root.popAfter(root.ensureWelcome());
        App.login.start(network);
        root.pushLink();
    }

    // Back from linking: the welcome page, or the list when accounts exist.
    function leaveLink(): void {
        if (App.hasAccounts) {
            root.showList();
        } else if (pageStack.depth > 0 && pageStack.get(0) === listPage) {
            // The only account was removed to be linked again, and that link was left: no
            // list of an account that is gone under the welcome page.
            root.openWelcome();
        } else {
            root.popAfter(root.ensureWelcome());
        }
    }

    function openSettings(): void {
        root.attachOrDrop();
        root.popAfter(listPage);
        root.pushSettings();
    }

    // Before another page takes the place after the list: the stand-in does not wait there.
    function attachOrDrop(): void {
        if (root.chatPending || root.standIn !== null) {
            root.chatPending = false;
            root.dropStandIn();
        }
    }

    // A restart that turned a network on asked for its link page, once ("Turn On and
    // Restart"): the warning's tick counted for it.
    function openPendingLink(): void {
        const network = App.takePendingLink();
        if (network === "") {
            return;
        }
        if (root.welcomePage === null || !root.inStack(root.welcomePage)) {
            root.openWelcome();
        }
        root.openLinkAs(network, true);
    }

    function start(): void {
        if (root.started) {
            return;
        }
        root.started = true;
        // After the first page is up.
        Qt.callLater(root.openPendingLink);
        const page = App.startPage;
        if (page === "splitnarrow" && App.fixture && root.wideScreen) {
            // The page of the rail: a wide window too narrow for the full list and two chats (1000 px is
            // between the two limits at any usual size of the theme's font).
            root.width = 1000;
        }
        // The pages that walk a link: the link page, its QR code, and its states of a
        // history transfer.
        const links = page === "link" || page === "qr" || root.transferLinkPages.indexOf(page) >= 0;
        if (!App.hasAccounts || page === "welcome" || links) {
            pageStack.clear();
            pageStack.push(root.ensureWelcome());
            if (links) {
                pageStack.push(root.ensureLink());
            }
            if (root.transferLinkPages.indexOf(page) >= 0) {
                pageSetup.start();
            } else if (page === "warisk") {
                // The warning of a network that is off: the page a click on its row opens.
                root.openLink("WhatsApp");
            } else if (page === "tgwarn") {
                // The same for Telegram, off (`FixtureOptions::telegramOff`: the one page that
                // lists it).
                root.openLink("Telegram");
            } else if (App.fixture && root.whatsappLinkPages.indexOf(page) >= 0) {
                // WhatsApp's link page at its QR code, at the phone-number question or at the
                // code (the warning counted: the person ticked it for this link). Only on the
                // throwaway fixture profile: the counted warning would start a real link
                // without the warning (the argument parser refuses these pages without
                // --fixture too).
                root.openLinkAs("WhatsApp", true);
                whatsappLinkSetup.start();
            } else if (App.fixture && root.threemaLinkPages.indexOf(page) >= 0) {
                // Threema's link page at its QR code or at the emoji (the warning counted, as for
                // WhatsApp's; fixture only for the same reason).
                root.openLinkAs("Threema", true);
                threemaLinkSetup.start();
            } else if (App.fixture && root.telegramLinkPages.indexOf(page) >= 0) {
                // Telegram's link page at its QR code, at the code of the phone-number way or at
                // its password (the warning counted, as for the others; fixture only for the same
                // reason).
                root.openLinkAs("Telegram", true);
                telegramLinkSetup.start();
            }
            return;
        }
        const chatPages = ["chat", "split", "splitnarrow", "history", "media", "viewer", "compose", "files", "originals", "folder", "folderfiles", "folderzip", "location", "locationmany", "locationnever"].concat(root.actionPages);
        const showsChat = chatPages.indexOf(page) >= 0 && App.timeline.conversationId !== 0;
        // An ordinary start opens no chat: the chat column can wait for the first frame.
        root.showListWith(!showsChat && root.pendingConversation <= 0);
        if (root.pendingConversation > 0) {
            const id = root.pendingConversation;
            const message = root.pendingMessage;
            root.pendingConversation = 0;
            root.pendingMessage = 0;
            root.openChatAt(id, message);
            return;
        }
        if (showsChat) {
            const chatPage = root.ensureChat();
            if (!root.wideScreen) {
                pageStack.push(chatPage);
                // (A split shows its active chat.)
                root.syncSplit();
            }
            pageStack.currentIndex = 1;
            chatPage.pinToOldest = page === "history";
            if (page === "viewer") {
                viewerShot.start();
            }
            if (root.setupPages.indexOf(page) >= 0) {
                pageSetup.start();
            }
        } else if (page === "compact" && App.fixture) {
            // Only on the throwaway fixture profile: the setting is saved.
            App.settings.compact = true;
        } else if (page === "settings") {
            root.openSettings();
        } else if (page === "expiry" || page === "networks" || page === "appearance" || page === "filters" || page === "filteredit") {
            // The settings page at "Disappearing messages" or at "Networks", once the core has
            // said what holds.
            root.openSettings();
            expirySetup.start();
        } else if (page === "about") {
            // The page Settings > "About Kourier" opens, on top of the settings page.
            root.openSettings();
            root.openAbout();
        } else if (page === "transferbanner" || page === "transferfailed" || page === "transferupdate" || page === "wahistory" || page === "walost") {
            // The backend says where the transfer stands once it may connect.
            pageSetup.start();
        } else if (page === "transferask") {
            // The settings page at "Accounts", with the question of its action open.
            root.openSettings();
            pageSetup.start();
        } else if (page === "archive") {
            // The list at "Archived Chats", once it says there is one.
            archiveSetup.start();
        } else if (root.searchPages.indexOf(page) >= 0) {
            // The chat list with a word typed into its search field (G1j).
            pageSetup.start();
        } else if (root.rowMenuPages.indexOf(page) >= 0) {
            // The chat list with the menu of one row open (the real path of a right click).
            pageSetup.start();
        } else if (root.accountsPages.indexOf(page) >= 0) {
            // Blended chats: the fixture's three accounts, the second hidden by the page (the
            // choice goes through `App.toggleAccount`, as a click does); `pageSetup` waits for it.
            pageSetup.start();
        } else if (page === "drawer") {
            // The drawer as a sidebar (wide) or opened over the list (narrow).
            if (root.wideScreen) {
                accountsDrawer.collapsed = false;
            } else {
                accountsDrawer.open();
            }
        }
    }

    onWideScreenChanged: {
        // Keep the chat column in wide mode even before a conversation is opened.
        if (root.started && !root.chatPending) {
            root.attachChat();
        }
        Qt.callLater(root.syncSplit);
    }

    onActiveChanged: {
        if (!root.active) {
            root.deactivatedAt = Date.now();
        }
        App.setPresence(root.active, root.chatShown);
    }
    onChatShownChanged: App.setPresence(root.active, root.chatShown)

    // Close button: to the tray when one is shown, else quit (§8.8).
    onClosing: close => {
        if (root.trayAvailable && !root.quitting) {
            close.accepted = false;
            root.hideToTray();
        } else if (!root.quitting && !root.quitAgreed && App.quitQuestion()) {
            // Work still runs (F8): asked first, the window stays meanwhile. Not for the
            // tray's Quit (`quitting`: it was chosen, and the window may be hidden, where a
            // question could not be read) and not while the session ends (`quitQuestion` is
            // empty then).
            close.accepted = false;
            root.quitAsking();
        } else {
            root.closedForGood();
        }
    }

    // ---- Keyboard ----------------------------------------------------------------------
    // The window's own keys. Where Qt has a standard key (`StandardKey`) the platform's
    // binding is used (Plasma: the user's own KDE shortcut settings), with a fixed one beside
    // it for platforms that define none. None of them is a key a text field uses for editing
    // (the field would win those anyway), so the composer and the search field keep every
    // key they take. They stand aside while a dialog, a menu or the picture viewer has the
    // keyboard, and while there is no list to move in.
    readonly property bool keysActive: root.started && App.hasAccounts && !root.inOverlay(root.activeFocusItem)
    // How the platform writes the keys of the actions that have a control (tooltips).
    readonly property string searchKeys: searchKey.nativeText
    readonly property string settingsKeys: settingsKey.nativeText !== "" ? settingsKey.nativeText : "Ctrl+,"

    // `item` is inside a popup (they all sit in the window's overlay).
    function inOverlay(item: Item): bool {
        for (let i = item; i; i = i.parent) {
            if (i === root.overlay) {
                return true;
            }
        }
        return false;
    }

    // The keyboard goes to the conversation list's search field (the archive view has none:
    // the chats come back first, as in Signal Desktop). A narrow window shows the chat or a
    // page over the list: the list comes back first.
    function focusSearch(): void {
        if (pageStack.depth > 0 && pageStack.get(0) === listPage) {
            pageStack.currentIndex = 0;
        }
        listPage.focusSearch();
    }

    // Search in the open chat only (G1i): the same as `focusSearch`, with the chat's chip in the
    // field. A chat that is not open, or is protected (it offers no search), does nothing.
    function searchInChat(): void {
        if (root.activeTimeline.conversationId === 0 || !root.activeTimeline.chatSearchable) {
            return;
        }
        if (pageStack.depth > 0 && pageStack.get(0) === listPage) {
            pageStack.currentIndex = 0;
        }
        listPage.searchInChat();
    }

    // Opens the chat `step` rows below (or above) the open one in the list as it is
    // shown; the ends do not wrap.
    function stepConversation(step: int): void {
        const id = App.adjacentConversation(root.activeTimeline.conversationId, step);
        if (id !== 0 && id !== root.activeTimeline.conversationId) {
            root.openingChat(id, null);
        }
    }

    // The quick switch opened `id`: the composer takes the keyboard once the chat is there.
    function switchTo(id: real): void {
        root.openingChat(id, () => {
            // (An archived chat that the search found opens in the archive view. Before the
            // field is emptied: the search is what lists it.)
            App.revealChat(id);
            listPage.searchField.text = "";
            Qt.callLater(() => root.activeChat?.composer.focusInput());
        }, 0, true);
    }

    // The banner's button. Only "Link Again" of a logged-out account takes the link page's
    // place (a relink cancels the login that waits at its QR code); the other kinds
    // (challenge, restart) leave the link page where it is, so they never ask.
    function bannerPressed(): void {
        if (App.bannerKind === "loggedOut") {
            root.leavingLink(() => App.bannerActivate());
        } else if (App.bannerKind === "networkOff") {
            // "Turn On and Restart": the preference is written and Kourier starts again.
            root.restartAsking(() => App.bannerActivate());
        } else {
            App.bannerActivate();
        }
    }

    Shortcut {
        id: searchKey
        sequences: ["Ctrl+K", StandardKey.Find]
        enabled: root.keysActive
        onActivated: root.focusSearch()
    }
    // Search in the open chat only: a chat page is up and the chat is not protected.
    Shortcut {
        id: chatSearchKey
        sequences: ["Ctrl+Shift+F"]
        enabled: root.keysActive && root.chatKeys && root.activeTimeline.chatSearchable
        onActivated: root.searchInChat()
    }
    // The keyboard round the panes: list, first chat, second chat of a split.
    Shortcut {
        sequences: ["F6"]
        enabled: root.keysActive
        onActivated: root.cycleFocus(false)
    }
    Shortcut {
        sequences: ["Shift+F6"]
        enabled: root.keysActive
        onActivated: root.cycleFocus(true)
    }
    // Split view: closes the inactive pane; off, opens the focused row's chat beside or starts the chooser.
    Shortcut {
        sequences: ["F3"]
        enabled: root.keysActive && (root.chatKeys || root.choosingBeside)
        onActivated: root.splitKey()
    }
    // Escape ends the chooser wherever the keyboard is (a dialog's own Escape comes first: `keysActive`).
    Shortcut {
        sequences: ["Escape"]
        enabled: root.keysActive && root.choosingBeside
        onActivated: {
            root.endChoosing();
            listPage.conversations.forceActiveFocus(Qt.TabFocusReason);
        }
    }
    Shortcut {
        sequences: ["Alt+Down", "Ctrl+PgDown", "Ctrl+Tab"]
        enabled: root.keysActive
        onActivated: root.stepConversation(1)
    }
    Shortcut {
        sequences: ["Alt+Up", "Ctrl+PgUp", "Ctrl+Shift+Tab", "Ctrl+Backtab"]
        enabled: root.keysActive
        onActivated: root.stepConversation(-1)
    }
    // Archive the open chat; take it back into the chats. Ctrl+Shift+U is IBus's Unicode entry
    // on some setups (it then never reaches the window).
    Shortcut {
        sequences: ["Ctrl+Shift+A"]
        enabled: root.keysActive && root.chatKeys
        onActivated: root.keyArchive(true)
    }
    Shortcut {
        sequences: ["Ctrl+Shift+U"]
        enabled: root.keysActive && (root.chatKeys || root.undoStanding !== null)
        onActivated: root.undoStanding !== null ? root.undoArchive(root.undoStanding) : root.keyArchive(false)
    }
    Shortcut {
        id: settingsKey
        sequences: [StandardKey.Preferences, "Ctrl+,"]
        enabled: root.keysActive
        onActivated: root.leavingLink(root.openSettings)
    }
    // Close follows the close button (to the tray, or quit); quit always quits, also from
    // a dialog.
    Shortcut {
        sequences: [StandardKey.Close, "Ctrl+W"]
        enabled: !root.inOverlay(root.activeFocusItem)
        onActivated: root.close()
    }
    Shortcut {
        sequences: [StandardKey.Quit, "Ctrl+Q"]
        onActivated: root.quitAsking()
    }

    // Tab walks a wide window the way the eye does: the search field, the conversations (and the
    // way into the archive after them), the timeline, the composer (Shift+Tab the other way).
    // Kirigami's own order puts the chat's
    // footer and toolbar ahead of the list. While the account banner shows a button the
    // search field keeps the natural order, so that the button stays in reach.
    readonly property Item timelineStop: root.wideScreen && root.activeChat !== null && root.activeChat.open ? root.activeChat.timeline.rowList : null
    readonly property Item composerStop: root.timelineStop !== null ? root.activeChat.composer.firstStop : null
    Binding {
        target: listPage.searchField.KeyNavigation
        property: "tab"
        value: listPage.bannerShown ? null : listPage.conversations
    }
    // The side panel beside the list comes before it (the eye's order: panel, search, chats):
    // Shift+Tab from the search field reaches the panel's one Tab stop (UI-4). A modal drawer
    // over the list is not in that order: there Shift+Tab reaches the toolbar's menu button, and
    // Space on it opens the panel with the keyboard on that stop (V5).
    Binding {
        target: listPage.searchField.KeyNavigation
        property: "backtab"
        value: accountsDrawer.modal ? root.menuButton : accountsDrawer.tabEntry
    }
    // Kirigami's closed menu button of a narrow window: the list page toolbar's private
    // `HandleButton`, the one item there whose `drawer` is ours (null when there is none).
    readonly property Item menuButton: accountsDrawer.modal ? root.handleIn(listPage.globalToolBarItem) : null
    // (`var`: the button's `drawer` is a member of Kirigami's private type, not of `Item`.)
    function handleIn(item: var): Item {
        if (item === null) {
            return null;
        }
        if (item.drawer === accountsDrawer) {
            return item;
        }
        for (const child of item.children) {
            const found = root.handleIn(child);
            if (found !== null) {
                return found;
            }
        }
        return null;
    }
    Connections {
        target: accountsDrawer
        // The keyboard that opened the modal panel goes into it, not behind its dim.
        function onOpened(): void {
            if (accountsDrawer.modal && root.menuButton !== null && root.menuButton.activeFocus) {
                accountsDrawer.tabEntry.forceActiveFocus(Qt.TabFocusReason);
            }
        }
    }
    // (The archive view has no search field: Shift+Tab from its list goes to the Back arrow.)
    Binding {
        target: listPage.conversations.KeyNavigation
        property: "backtab"
        value: App.archiveShown ? listPage.archiveBack : listPage.searchField
    }
    // The list's last row, the way into the archive, is a stop of its own after the list (Down
    // stops at the last chat) while it shows; the timeline's Shift+Tab lands on it. (Its own
    // Shift+Tab, to the list, is the entry's: the list is the focus scope the entry sits in, and
    // a focus scope cannot be given the keyboard from inside.)
    readonly property Item archiveStop: listPage.archiveEntry
    Binding {
        target: listPage.conversations.KeyNavigation
        property: "tab"
        value: root.archiveStop !== null ? root.archiveStop : root.timelineStop
    }
    Binding {
        target: root.archiveStop ? root.archiveStop.KeyNavigation : null
        property: "tab"
        value: root.timelineStop
    }
    Binding {
        target: root.timelineStop ? root.timelineStop.KeyNavigation : null
        property: "backtab"
        value: root.archiveStop !== null ? root.archiveStop : listPage.conversations
    }
    Binding {
        target: root.timelineStop ? root.timelineStop.KeyNavigation : null
        property: "tab"
        value: root.composerStop
    }
    Binding {
        target: root.composerStop ? root.composerStop.KeyNavigation : null
        property: "backtab"
        value: root.timelineStop
    }

    Component.onCompleted: {
        if (App.loaded) {
            root.start();
        }
    }

    Connections {
        target: App
        function onLoadedChanged(): void {
            if (App.loaded) {
                Qt.callLater(root.start);
            }
        }
        function onHasAccountsChanged(): void {
            // The last account went away: back to the welcome page.
            if (root.started && !App.hasAccounts && (root.linkPage === null || root.pageStack.lastItem !== root.linkPage)) {
                Qt.callLater(root.openWelcome);
            }
        }
    }

    // "Restart Now", "Turn On and Restart" and the banner's "Turn On" ask for a start again: the
    // window quits like a quit that was agreed to, and `main` starts the process anew once the
    // core is down.
    Connections {
        target: App
        function onRestartRequested(): void {
            root.quitAgreed = true;
            root.closedForGood();
        }
    }

    // Leaving the link page any way (Cancel, back, the drawer) abandons an unfinished login;
    // after "Linked" or a failure this is a no-op.
    Connections {
        target: root.pageStack
        function onPageRemoved(page: Item): void {
            if (!root.restacking && root.linkPage !== null && page === root.linkPage) {
                App.login.cancel();
            }
        }
    }

    // The open chat went away (the user deleted it, here or on another device): a narrow
    // window goes back to the list instead of showing an empty chat page.
    readonly property real openChat: root.activeTimeline.conversationId
    onOpenChatChanged: {
        if (root.openChat === 0) {
            root.choosingBeside = false;
        }
        if (root.openChat === 0 && !root.wideScreen && root.activeChat !== null && root.pageStack.lastItem === root.activeChat && root.pageStack.depth > 1) {
            Qt.callLater(root.backToList);
        }
    }

    function backToList(): void {
        if (root.activeTimeline.conversationId === 0 && root.activeChat !== null && root.pageStack.lastItem === root.activeChat) {
            root.popAfter(listPage);
        }
    }

    // A finished link shows the conversation list after a moment on "Linked".
    Connections {
        target: App.login
        function onKindChanged(): void {
            if (App.login.kind === "done") {
                linkDone.restart();
            }
        }
    }

    Timer {
        id: linkDone
        interval: 900
        onTriggered: {
            if (root.linkPage !== null && root.pageStack.lastItem === root.linkPage) {
                root.showList();
            }
        }
    }

    // Backends connect after the first frame (§8.2 step 4).
    // The chats of the last run reopen after the first frame of the list (the start budget stays the
    // list's: never before it), once. `App.restoreChats` answers false while the list is not known yet; after
    // 3 s (30 asks) it takes what is there. A start that opened a chat of its own restores nothing.
    Timer {
        interval: 100
        repeat: true
        running: root.framed && root.started && App.hasAccounts && listPage.rowsShown && !root.restoreAsked
        onTriggered: {
            if (root.chatOpenedEarly || root.pendingConversation > 0) {
                root.restoreAsked = true;
                App.restoreSkipped();
            } else if (root.restoreStep()) {
                root.restoreAsked = true;
            }
        }
    }

    // One ask of the restore; true when it is over. A split the restore made (not one a page made) ends with
    // the left chat active.
    function restoreStep(): bool {
        const wasSplit = App.split;
        if (!App.restoreChats(root.restoreTries++ >= 30)) {
            return false;
        }
        if (App.split && !wasSplit) {
            // Both chats reopened: the left one ends active. The split's own start puts the keyboard in the
            // active pane a moment later (`onSplitChanged`) and a focus that lands in the right chat
            // would activate it, so the left chat takes it back after that.
            Qt.callLater(() => {
                App.activatePane(0);
                root.chatPage?.composer.focusInput();
            });
        }
        return true;
    }

    Connections {
        id: firstFrame
        target: root
        function onFrameSwapped(): void {
            if (!root.framed) {
                root.framed = true;
                App.ready();
            }
            // The chat column comes after the frame that shows the list's rows.
            if (root.chatPending && !listPage.rowsShown) {
                return;
            }
            firstFrame.enabled = false;
            if (root.chatPending) {
                Qt.callLater(root.attachChat);
            }
        }
    }

    // Timing marks (`KOURIER_TRACE_STARTUP=1`, architecture §1.3): while a mark waits for the
    // frame that shows its rows, each presented frame is reported with what was on screen.
    // Connected only then: frames cost nothing otherwise.
    Connections {
        target: root
        enabled: App.traceFrame
        function onFrameSwapped(): void {
            App.frameTraced(root.started && listPage.visible && listPage.rowsShown && App.conversations.count > 0, root.chatPage !== null && root.chatPage.visible && root.chatPage.timeline.rowList.count > 0);
        }
    }

    // The scripted session of a measuring build (cargo feature `perf`, `cargo xtask perf`).
    // Every other build has an empty `App.perfScenario` and no such file.
    Loader {
        id: perfDriver
        Component.onCompleted: {
            if (App.perfScenario !== "") {
                // (The driver reads the chat page from the window: it is created later.)
                perfDriver.setSource("qrc:/qt/qml/app/kourier/perf/PerfDriver.qml", {
                    "window": root
                });
            }
        }
    }

    // Typing indicators expire while shown.
    Timer {
        running: App.timersActive
        interval: 1000
        repeat: true
        onTriggered: App.tick()
    }

    // The picture viewer and the "open this file?" question, created when asked for (§8.6).
    Component {
        id: viewerComponent
        ImageViewer {
            id: viewerItem
            onClosed: {
                // A viewer opened from a gallery has its own query (the slot "viewer"): it ends with the
                // viewer, unless another viewer took the slot meanwhile (this one closes after its
                // transition, and the new one may have been opened already).
                if (viewerItem.gallery !== "" && root.imageViewer === viewerItem) {
                    App.closeChatMedia("viewer");
                }
                destroy();
            }
        }
    }

    Component {
        id: confirmOpenComponent
        Kirigami.PromptDialog {
            id: confirm

            property real attachment: 0
            property string name: ""
            // Open in Slicer: the slicer's table id ("other" = the desktop's chooser, "" = the
            // default application) and its name.
            property string slicerId: ""
            property string slicerName: ""
            readonly property bool inSlicer: confirm.slicerId !== "" && confirm.slicerId !== "other"
            // The subtitle label guesses its format (AutoText) and the name comes from the
            // sender: a name such as `<img src="https://…">` would be markup that loads things.
            // Escape it and make the text rich on purpose, so it shows exactly as named.
            readonly property string escapedName: confirm.name.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")

            title: L10n.i18nc("@title:dialog", "Open This File?")
            subtitle: confirm.inSlicer ? "<p>" + L10n.i18nc("@info, %1 a file name, %2 the name of a slicer program", "“%1” opens in %2. Files can be harmful: open only what you expect from this sender.", confirm.escapedName, confirm.slicerName) + "</p>" : "<p>" + L10n.i18nc("@info", "“%1” opens in another application. Files can be harmful: open only what you expect from this sender.", confirm.escapedName) + "</p>"
            standardButtons: Kirigami.Dialog.NoButton
            customFooterActions: [
                Kirigami.Action {
                    text: L10n.i18nc("@action:button", "Open")
                    icon.name: "document-open"
                    onTriggered: {
                        if (confirm.slicerId === "") {
                            App.openExternally(confirm.attachment);
                        } else {
                            App.openInSlicer(confirm.attachment, confirm.slicerId);
                        }
                        confirm.close();
                    }
                },
                Kirigami.Action {
                    text: L10n.i18nc("@action:button", "Cancel")
                    icon.name: "dialog-cancel"
                    onTriggered: confirm.close()
                }
            ]
            onClosed: destroy()
        }
    }

    // The viewer that was opened last (fixture pages and tests step through it).
    property ImageViewer imageViewer: null
    // How many viewers were opened (a test counts them: one click opens one).
    property int viewersOpened: 0

    // Opens the viewer on `items` (one picture, or the pictures of an album) at `index`;
    // returns it. With `message` (the message they belong to) the viewer follows that
    // message's row from then on: a download that finishes, a picture that is deleted.
    // With `gallery` (the slot of "Media and Files" the picture came from: "group", "person" or
    // "media") the viewer follows the message through its OWN query (`App.showGalleryViewer`, made by
    // the caller before: the whole album, read at the message) instead of the open timeline's rows.
    function openAlbumViewer(items: var, index: int, message: real, gallery: var): ImageViewer {
        // One viewer at a time: one that is still open is closed before the new one opens.
        if (root.imageViewer !== null && root.imageViewer.visible) {
            root.imageViewer.close();
        }
        if (typeof gallery !== "string" || gallery === "") {
            // (A viewer of the timeline's: nothing of a gallery's own query is left behind.)
            App.closeChatMedia("viewer");
        }
        const viewer = viewerComponent.createObject(root, {
            "items": items,
            "index": index,
            "message": message,
            "gallery": typeof gallery === "string" ? gallery : ""
        }) as ImageViewer;
        viewer?.open();
        root.imageViewer = viewer;
        root.viewersOpened += 1;
        return viewer;
    }

    // The 3D viewer that was opened last, and how many were opened (tests).
    property var modelViewer: null
    property int modelViewersOpened: 0
    // Whether Quick 3D can draw: the software Quick backend (the shots, the window tests, a machine
    // without a GPU) cannot, and the model's still is shown instead. Read from an item of the window.
    readonly property bool software: graphicsProbe.GraphicsInfo.api === GraphicsInfo.Software
    Item {
        id: graphicsProbe
        visible: false
    }

    // The question before a 3D model goes to a slicer (or to the desktop's chooser): the same
    // "Open This File?" as any file, with the slicer named.
    function askOpenInSlicer(attachment: real, name: string, slicerId: string): void {
        let slicerName = "";
        for (const row of JSON.parse(App.slicers)) {
            if (row.id === slicerId) {
                slicerName = row.name;
            }
        }
        const dialog = confirmOpenComponent.createObject(root, {
            "attachment": attachment,
            "name": name,
            "slicerId": slicerId,
            "slicerName": slicerName
        }) as Kirigami.PromptDialog;
        root.slicerAsk = dialog;
        dialog?.open();
    }
    // The question "Open This File?" of the last Open in Slicer (null once it is gone; window tests).
    property var slicerAsk: null

    // Opens the 3D viewer on the model `attachment`. Quick 3D is loaded HERE, by its file name, on
    // the first open: neither the module nor the viewer exists before (§1.3: idle memory). On the
    // software backend the small still viewer opens instead and Quick 3D stays unloaded.
    function openModelViewer(attachment: real, name: string, save: bool, tint: bool): var {
        if (root.modelViewer !== null && root.modelViewer.visible) {
            root.modelViewer.close();
        }
        const file = root.software ? "dialogs/Model3dStill.qml" : "dialogs/Model3dViewer.qml";
        const component = Qt.createComponent(Qt.resolvedUrl(file));
        if (component.status !== Component.Ready) {
            console.warn("the 3D viewer could not be loaded: " + component.errorString());
            return null;
        }
        const viewer = component.createObject(root, {
            "attachment": attachment,
            "name": name,
            "saveOffered": save,
            "tint": tint
        });
        if (viewer === null) {
            return null;
        }
        viewer.closed.connect(() => {
            if (root.modelViewer === viewer) {
                root.modelViewer = null;
            }
            viewer.destroy();
        });
        viewer.open();
        root.modelViewer = viewer;
        root.modelViewersOpened += 1;
        return viewer;
    }

    // Opens the viewer on one picture; returns it (fixture screenshots).
    function openImageViewer(attachment: real, url: string, animated: bool, media: bool): ImageViewer {
        return root.openAlbumViewer([
            {
                "attachment": attachment,
                "url": url,
                "animated": animated,
                "media": media,
                "kind": "image",
                "state": "local",
                "progress": 0,
                "placeholder": ""
            }
        ], 0, 0);
    }

    Connections {
        target: MediaActions
        function onViewRequested(attachment: real, url: string, animated: bool, media: bool, message: real): void {
            // With its message the viewer shows the row as it is and follows it.
            const items = message > 0 ? App.viewerItems(message) : [];
            for (let i = 0; i < items.length; ++i) {
                if (items[i].attachment === attachment) {
                    root.openAlbumViewer(items, i, message);
                    return;
                }
            }
            root.openImageViewer(attachment, url, animated, media);
        }
        function onAlbumRequested(items: var, index: int, message: real): void {
            root.openAlbumViewer(items, index, message);
        }
        // A square of "Media and Files": the viewer opens at once with the slot's rows of the message
        // (the open timeline need not hold it), and its own query of the message (the whole album: a
        // section holds six squares, a level the pages it loaded) takes over when it answers.
        function onGalleryRequested(slot: string, attachment: real, message: real): void {
            const items = App.galleryViewerItems(slot, message);
            for (let i = 0; i < items.length; ++i) {
                if (items[i].attachment === attachment) {
                    App.showGalleryViewer(slot, message);
                    root.openAlbumViewer(items, i, message, slot);
                    return;
                }
            }
        }
        function onModelRequested(attachment: real, name: string, save: bool, tint: bool): void {
            root.openModelViewer(attachment, name, save, tint);
        }
        function onSlicerRequested(attachment: real, name: string, slicerId: string): void {
            root.askOpenInSlicer(attachment, name, slicerId);
        }
        function onOpenRequested(attachment: real, name: string, media: bool): void {
            if (media) {
                App.openExternally(attachment);
                return;
            }
            const dialog = confirmOpenComponent.createObject(root, {
                "attachment": attachment,
                "name": name
            }) as Kirigami.PromptDialog;
            dialog?.open();
        }
    }

    // A copy action is done: the toast says what came of it (it holds no number and no link).
    Connections {
        target: App
        function onCopyTold(text: string): void {
            root.showPassiveNotification(Style.plain(text), "short");
            root.Accessible.announce(text, Accessible.Polite);
        }
    }

    // A slicer was started with the copy: it is the one the main button of the viewer starts next
    // ("Other Application…" is never remembered, and a refused or failed start writes nothing).
    Connections {
        target: App
        function onSlicerStarted(attachment: real, id: string): void {
            App.settings.modelSlicer = id;
        }
    }

    // The copy for another application is ready.
    Connections {
        target: App
        function onExternalReady(attachment: real, url: string): void {
            App.openExternalUrl(url);
        }
    }

    // A passing notice with, when `actionText` is not empty, one button (`callback` runs on a click):
    // THE way the window tells the user something that ended ("Saved …", F3b; the undo of an archive,
    // A10d, comes through it too). The text goes through `Style.plain` (it can hold a name or a file
    // name from the network), is announced to a screen reader politely (a confirmation needs no
    // interruption; Kirigami's passive notification has no announcement of its own), and is recorded
    // for the tests.
    function notify(text: string, actionText: string, callback: var): void {
        const plain = Style.plain(text);
        root.noticeCount += 1;
        root.noticeText = plain;
        root.noticeAction = actionText;
        root.noticeCallback = callback ?? null;
        root.showPassiveNotification(plain, "long", actionText, callback ?? (() => {}));
        root.announce(plain);
    }

    // Tells a screen reader `text`, politely, and records what was told.
    function announce(text: string): void {
        root.noticeAnnounced = text;
        root.noticeAnnouncedPoliteness = Accessible.Polite;
        noticeSpeaker.Accessible.announce(text, Accessible.Polite);
    }

    // How many notices `notify` showed, the last one's text and button, what was announced and how.
    property int noticeCount: 0
    property string noticeText: ""
    property string noticeAction: ""
    property var noticeCallback: null
    property string noticeAnnounced: ""
    property int noticeAnnouncedPoliteness: -1

    // (Not hidden: an item that is not visible has no accessible object to announce from.)
    Item {
        id: noticeSpeaker
        width: 0
        height: 0
        Accessible.role: Accessible.StaticText
        Accessible.name: root.noticeText
    }

    // The progress of a running export and its Cancel (Export Selected): the window's own
    // footer, so a chat switch neither hides it nor lets another chat's bar cancel it. Shown
    // only while `App.exportBusy`.
    footer: ColumnLayout {
        spacing: 0

        ExportStrip {
            id: exportStrip

            Layout.fillWidth: true
        }
        // The progress of a forward that downloads its files first (forward line F6).
        ForwardStrip {
            id: forwardStrip

            Layout.fillWidth: true
        }
    }

    // Save As… and Save All… (F3b): the dialog is made on first use, from the file that
    // `App.saveDialogSource()` names (empty: the built-in one), so the dialogs module is not loaded
    // by the start. One attachment asks Rust for the name it is offered under (`App.askSave`), then
    // the dialog starts there; several ask for a folder.
    // The "As picture" look of a forward draws here, outside the visible area (forward line F7).
    ForwardPicture {
        id: forwardPicture
    }
    // The picture look's item (tests).
    readonly property alias forwardPictureItem: forwardPicture

    // S5: the one carrier of messages dragged from one chat of a split to the other (the timelines start it,
    // the chat pages' drop areas take it). Made with the window's first split (`syncSplit` sets `dragCarrier`) and
    // kept after it, so a drag that runs while the split ends keeps its item; a window that never splits makes none.
    property bool dragCarrier: false
    Loader {
        id: messageDragLoader

        active: root.dragCarrier
        z: 1000
        sourceComponent: MessageDragProxy {}
    }
    readonly property MessageDragProxy messageDragItem: messageDragLoader.item as MessageDragProxy
    Binding {
        target: TimelineActions
        property: "messageDrag"
        value: messageDragLoader.item
    }

    // A chat could not take messages dropped on it: says so, by its name.
    function forwardRefused(count: int, chat: string): void {
        root.notify(count === 1 ? L10n.i18nc("@info:status a dragged message was dropped on the other chat, %1 its name", "This message cannot be forwarded to %1.", chat) : L10n.i18nc("@info:status dragged messages were dropped on the other chat, %1 its name", "These messages cannot be forwarded to %1.", chat), "", null);
    }

    Loader {
        id: saveDialogLoader
        objectName: "saveDialogLoader"
    }

    // What the dialog says (the real one or a test's stand-in with the same signals): the file or the
    // folder it was given, and that it is over (it is made anew the next time). The answer belongs
    // to the attachments the DIALOG was opened for (`attachments`), not to `saveWanted`, which
    // may have been set for another Save meanwhile.
    Connections {
        target: saveDialogLoader.item
        ignoreUnknownSignals: true
        function onPicked(chosen: url): void {
            // The save dialog of an export answers the export, not a save of attachments.
            if (root.exportSaving !== null && root.exportSaving === saveDialogLoader.item) {
                root.exportSaved = true;
                App.exportPicked(chosen);
                return;
            }
            root.savePicked(root.saveDialog.attachments, chosen);
        }
        function onClosed(): void {
            // Only the dialog that closed goes: another Save may have made a new one by the time
            // the call runs.
            const closed = saveDialogLoader.item;
            if (root.exportSaving !== null && root.exportSaving === closed) {
                root.exportSaving = null;
                if (!root.exportSaved) {
                    App.exportAbandon();
                }
            }
            Qt.callLater(() => {
                if (saveDialogLoader.item === closed) {
                    saveDialogLoader.source = "";
                }
            });
        }
    }

    // Any other way the export's save dialog goes (its item replaced or cleared without its
    // `closed`): the export's plan is dropped, so Export Selected… never stays dead.
    Connections {
        target: saveDialogLoader
        function onItemChanged(): void {
            if (root.exportSaving !== null && saveDialogLoader.item !== root.exportSaving) {
                root.exportSaving = null;
                if (!root.exportSaved) {
                    App.exportAbandon();
                }
            }
        }
    }

    // The save dialog an export opened (null while none stands) and whether it answered: a close
    // with no answer drops the export's plan.
    property var exportSaving: null
    property bool exportSaved: false
    // The forward dialog, loaded by URL the first time a plan comes and kept (one instance): a plan that comes while
    // it stands renews its lines, else it opens (forward line F6).
    property var forwardDialog: null
    // The timeline the forward dialog's messages came from (the active pane's when it opened).
    property TimelineModel forwardFrom: null
    // The job whose files are gone, while its question stands.
    property real forwardAsked: 0
    function openForwardDialog(json: string): void {
        if (root.forwardDialog !== null && root.forwardDialog.opened) {
            root.forwardDialog.update(json);
            return;
        }
        if (root.forwardDialog === null) {
            const component = Qt.createComponent(Qt.resolvedUrl("dialogs/ForwardDialog.qml"));
            if (component.status !== Component.Ready) {
                console.error("ForwardDialog: " + component.errorString());
                App.forwardAbandon();
                return;
            }
            root.forwardDialog = component.createObject(root);
            root.forwardDialog.forwarded.connect(() => {
                // A forward that goes ends the selection it was made from.
                if (root.forwardDialog.fromSelection && root.forwardFrom !== null) {
                    root.forwardFrom.selectClear();
                }
            });
        }
        if (root.forwardDialog !== null) {
            // The selection is the active pane's (split view): the one `App.forwardOpen` took the messages from.
            root.forwardFrom = root.activeTimeline;
            root.forwardDialog.fromSelection = root.activeTimeline.selecting;
            root.forwardDialog.show(json);
        }
    }

    // "Some files are no longer available. Send without them?" (a job whose files the network no longer has).
    Kirigami.PromptDialog {
        id: forwardAskDialog

        objectName: "forwardAskDialog"
        property bool answered: false

        title: L10n.i18nc("@title:dialog", "Files Not Available")
        subtitle: L10n.i18nc("@info", "Some files are no longer available. Send without them?")
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                objectName: "forwardAskSend"
                text: L10n.i18nc("@action:button", "Send Without Them")
                icon.name: "mail-forward"
                onTriggered: {
                    forwardAskDialog.answered = true;
                    App.forwardWithout(root.forwardAsked, true);
                    forwardAskDialog.close();
                }
            },
            Kirigami.Action {
                objectName: "forwardAskCancel"
                text: L10n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: forwardAskDialog.close()
            }
        ]
        // Any close without "Send Without Them" ends the job.
        onClosed: {
            if (!forwardAskDialog.answered) {
                App.forwardWithout(root.forwardAsked, false);
            }
        }
    }

    // The export dialog, loaded by URL the first time a plan comes and kept (one instance).
    property var exportDialog: null
    function openExportDialog(json: string): void {
        if (root.exportDialog === null) {
            const component = Qt.createComponent(Qt.resolvedUrl("dialogs/ExportDialog.qml"));
            if (component.status !== Component.Ready) {
                console.error("ExportDialog: " + component.errorString());
                App.exportAbandon();
                return;
            }
            root.exportDialog = component.createObject(root);
        }
        if (root.exportDialog !== null) {
            root.exportDialog.show(json);
        }
    }

    // The attachments the next dialog saves (their numbers): the Save that was asked last, until
    // its dialog is made.
    property var saveWanted: []
    // The start folder of a folder dialog ("Save All…") is being looked for.
    property bool saveFolderAsked: false
    // How many dialogs were opened (a test counts them).
    property int saveDialogsOpened: 0

    // The dialog the window made (null while none stands).
    readonly property var saveDialog: saveDialogLoader.item

    function startSave(attachments: var): void {
        if (!attachments || attachments.length === 0) {
            return;
        }
        // The export's save dialog stands: a second dialog would replace it without its `closed`.
        if (root.exportSaving !== null) {
            root.notify(L10n.i18nc("@info:status", "Finish or cancel the export first."), "", null);
            return;
        }
        root.saveWanted = attachments;
        if (attachments.length === 1) {
            App.askSave(attachments[0]);
        } else if (!root.saveFolderAsked) {
            // The folder is looked for off the Qt thread; `saveFolderReady` opens the dialog.
            root.saveFolderAsked = true;
            App.askSaveFolder();
        }
    }

    function openSaveDialog(properties: var): void {
        const override = App.saveDialogSource();
        root.saveDialogsOpened += 1;
        saveDialogLoader.setSource(override !== "" ? override : Qt.resolvedUrl("dialogs/SaveDialog.qml"), properties);
    }

    function sameNumbers(a: var, b: var): bool {
        return a.length === b.length && a.every((n, i) => n === b[i]);
    }

    // A dialog answered with a file (one attachment) or a folder (several): `attachments` are the
    // ones that dialog was opened for. A Save asked meanwhile (`saveWanted` is another list) keeps
    // its place.
    function savePicked(attachments: var, target: url): void {
        if (attachments.length === 1) {
            App.saveTo(attachments[0], target);
        } else if (attachments.length > 1) {
            App.saveAll(attachments.join(","), target);
        }
        if (root.sameNumbers(root.saveWanted, attachments)) {
            root.saveWanted = [];
        }
    }

    // What a save or a batch ended in, in words (the same form for a failure: nothing but the class
    // of the reason, never a path).
    function saveNoticeText(saved: int, failed: int, name: string, reason: string): string {
        if (failed === 0) {
            return saved === 1 ? L10n.i18nc("@info:status, %1 the name of the saved file", "Saved “%1”", name) : L10n.i18ncp("@info:status, %1 how many files", "Saved %1 file", "Saved %1 files", saved);
        }
        if (saved > 0) {
            return L10n.i18nc("@info:status, %1 how many files were saved, %2 how many were to be", "Some files could not be saved (%1 of %2 saved).", saved, saved + failed);
        }
        const what = failed === 1 ? L10n.i18nc("@info:status", "Could not save the file") : L10n.i18ncp("@info:status", "Could not save %1 file", "Could not save %1 files", failed);
        let why = "";
        switch (reason) {
        case "nospace":
            why = L10n.i18nc("@info:status why a file could not be saved", "there is no space left on the disk");
            break;
        case "notwritable":
            why = L10n.i18nc("@info:status why a file could not be saved", "the folder is not writable");
            break;
        case "nofolder":
            why = L10n.i18nc("@info:status why a file could not be saved", "the folder is gone");
            break;
        case "gone":
            why = L10n.i18nc("@info:status why a file could not be saved", "the message is gone");
            break;
        case "nothere":
            why = L10n.i18nc("@info:status why a file could not be saved", "the file is not on this computer");
            break;
        default:
            return L10n.i18nc("@info:status, %1 what could not be saved", "%1.", what);
        }
        return L10n.i18nc("@info:status, %1 what could not be saved, %2 why", "%1: %2.", what, why);
    }

    Connections {
        target: MediaActions
        function onSaveRequested(attachments: var): void {
            root.startSave(attachments);
        }
    }

    Connections {
        target: App
        // The name is here: the dialog starts at the folder and the name Rust built.
        function onSaveSuggested(attachment: real, url: string): void {
            if (root.saveWanted.length === 1 && root.saveWanted[0] === attachment) {
                root.openSaveDialog({
                    "folder": false,
                    "suggested": url,
                    "attachments": root.saveWanted
                });
            }
        }
        // Forward…: the plan is here, the dialog opens or renews its lines.
        function onForwardDialogReady(json: string): void {
            root.openForwardDialog(json);
        }
        function onForwardAsk(request: real, count: int): void {
            root.forwardAsked = request;
            forwardAskDialog.answered = false;
            forwardAskDialog.open();
        }
        // The picture look: the rows are in `App.forwardRows`, the item outside the window draws and cuts them.
        function onForwardRender(request: real, group: bool): void {
            forwardPicture.start(request, group);
        }
        function onForwardRenderStopped(request: real): void {
            forwardPicture.stop(request);
        }
        // A forward ended or was refused: the usual passive notice.
        function onForwardNotice(text: string): void {
            root.notify(text, "", null);
        }
        // Export Selected: the plan is here, the dialog opens (loaded by URL on first use).
        function onExportDialogReady(json: string): void {
            root.openExportDialog(json);
        }
        // Export Selected: the dialog's Export… was pressed; the save dialog starts at the
        // folder and name Rust built. Its answer goes to `App.exportPicked` (`attachments` is
        // empty: no attachment is saved).
        function onExportSaveReady(url: string): void {
            // Another save dialog stands: it is not replaced, the export is dropped.
            if (saveDialogLoader.item !== null) {
                root.notify(L10n.i18nc("@info:status", "Close the other save dialog first."), "", null);
                App.exportAbandon();
                return;
            }
            root.exportSaved = false;
            root.openSaveDialog({
                "folder": false,
                "suggested": url,
                "attachments": []
            });
            root.exportSaving = saveDialogLoader.item;
        }
        // Export Selected ended (or was refused): the notice, with "Show in Folder" after a
        // written archive; a finished export ends the selection and the composer comes back.
        function onExportFinished(text: string, url: string, selectionEnds: bool): void {
            if (url !== "") {
                root.notify(text, L10n.i18nc("@action:button", "Show in Folder"), () => App.showInFolder(url));
            } else {
                root.notify(text, "", null);
            }
            if (selectionEnds) {
                root.activeTimeline.selectClear();
            }
        }
        // The folder is here: the folder dialog of "Save All…" starts there.
        function onSaveFolderReady(url: string): void {
            if (!root.saveFolderAsked) {
                return;
            }
            root.saveFolderAsked = false;
            if (root.saveWanted.length > 1) {
                root.openSaveDialog({
                    "folder": true,
                    "startFolder": url,
                    "attachments": root.saveWanted
                });
            }
        }
        function onSaveFinished(saved: int, failed: int, name: string, url: string, reason: string): void {
            const text = root.saveNoticeText(saved, failed, name, reason);
            if (saved > 0) {
                root.notify(text, L10n.i18nc("@action:button", "Show in Folder"), () => App.showInFolder(url));
            } else {
                root.notify(text, "", null);
            }
        }
    }

    // Link again (the banner of a logged-out account): the link page shows that login. A
    // challenge to solve. And the history transfer of a newly linked account: its passing
    // notice, the question before an account is replaced, and the link that follows.
    Connections {
        target: App
        function onRelinkStarted(): void {
            root.attachOrDrop();
            // A link page in the stack shows the login the relink has just replaced (and
            // cancelled, after the question of `leavingLink`): its leaving the stack must
            // not cancel the relink that just began. A new page object, as in
            // `onReplaceStarted`.
            root.restacking = true;
            root.popAfter(listPage);
            root.restacking = false;
            root.pushLink();
        }
        function onChallengeRequested(account: real, url: string): void {
            const dialog = challengeComponent.createObject(root, {
                "account": account,
                "url": url
            }) as ChallengeDialog;
            dialog?.open();
        }
        // A transfer ended (done, or not transferred without this UI having skipped it).
        function onTransferNotice(text: string): void {
            root.transferNotices += 1;
            // (The text can carry an account's name.)
            root.showPassiveNotification(Style.plain(text), "long");
        }
        // A change of a chat the person made was made (archived, back in the inbox, marked
        // unread: told once the row says so, never at the click), or the sync switch of Settings
        // was refused, "The setting could not be saved.", or a change of a chat was refused
        // ("refused": the pin limit, or that it was not made; a toast, because the click came from the
        // list as often as from the open chat) (A10d).
        function onChatTold(kind: string, conversation: var, wasPinned: bool, text: string): void {
            if (kind === "archived") {
                root.tellArchived(conversation, wasPinned, text);
            } else if (kind === "unarchived" || kind === "marked-unread") {
                root.tellChat(text, "short");
            } else if (kind === "switch-refused" || kind === "refused") {
                root.tellChat(text, "long");
            }
        }
        // "Transfer History from Phone…", or "Try Again…" of a failed transfer.
        function onReplaceAsked(question: var): void {
            root.askAboutAccount(question, () => App.cancelReplace());
        }
        // "Remove Account…" or "Unlink…" of an account (Settings, the drawer's menu).
        function onAccountActionAsked(question: var): void {
            root.askAboutAccount(question, () => App.cancelAccountAction());
        }
        // What became of a removed or unlinked account's device, told once: in passing when
        // nothing is left to do, else in a notice that stays (what to do on the phone).
        function onLogoutNotice(text: string, onPhone: string, title: string): void {
            root.takeLogoutNotice(text, onPhone, title);
        }
        // The account was removed and its new link started: the link page, over the
        // welcome page as for a first link (the account may have been the only one).
        function onReplaceStarted(): void {
            root.attachOrDrop();
            // The stack is built anew. A link page that is in it shows another login, which
            // the replacement has ended already: its leaving the stack must not cancel the
            // login that just began.
            root.restacking = true;
            root.pageStack.clear();
            root.restacking = false;
            // New pages: one that left the stack and is pushed back makes Kirigami
            // re-create its header while it has no page row (a binding warning).
            if (root.welcomePage !== null) {
                root.welcomePage.destroy();
                root.welcomePage = null;
            }
            if (root.linkPage !== null) {
                root.linkPage.destroy();
                root.linkPage = null;
            }
            root.pageStack.push(listPage);
            root.pageStack.push(root.ensureWelcome());
            root.pageStack.push(root.ensureLink());
        }
    }

    Component {
        id: challengeComponent
        ChallengeDialog {
            onClosed: destroy()
        }
    }

    // ---- History transfer of a newly linked account ----------------------------------
    // The question before an account is removed and linked again, while it is open. Made
    // when it is asked (by name: nothing of it is read at the start); it destroys itself
    // when it closes.
    property Kirigami.PromptDialog replaceDialog: null
    // The notice that stays after a removal or an unlink the network did not confirm.
    property Kirigami.PromptDialog logoutDialog: null
    // How many notices about removed or unlinked accounts were told (tests count them).
    property int logoutNotices: 0
    // The notices not shown yet (link feedback L5): one that stays waits for the one open
    // before it to close (two at once would stack, and `logoutDialog` would lose the first),
    // and none is shown while the window is hidden (in the tray): a passing one would pass
    // unseen. Told when the window shows again.
    property var pendingLogoutNotices: []
    // The notice whose dialog is open (a notice leaves the queue when its dialog opens): a
    // window released with its dialog open hands this one over too (`unshownLogoutNotices`).
    property var openLogoutNotice: null

    // What this window told no one yet or not to the end: the open dialog's notice, then the
    // waiting ones. The shell takes them over when it releases the window (link feedback L5).
    function unshownLogoutNotices(): var {
        return root.openLogoutNotice !== null ? [root.openLogoutNotice].concat(root.pendingLogoutNotices) : root.pendingLogoutNotices;
    }

    onVisibleChanged: {
        if (root.visible) {
            root.tellLogoutNotices();
        }
    }

    // A notice about a removed or unlinked account (`App.logoutNotice`; tests call it too).
    function takeLogoutNotice(text: string, onPhone: string, title: string): void {
        root.logoutNotices += 1;
        root.pendingLogoutNotices.push({
            "text": text,
            "onPhone": onPhone,
            "title": title
        });
        root.tellLogoutNotices();
    }

    // Shows the waiting notices: the passing ones at once, those that stay one at a time.
    function tellLogoutNotices(): void {
        while (root.visible && root.logoutDialog === null && root.pendingLogoutNotices.length > 0) {
            const n = root.pendingLogoutNotices.shift();
            if (n.onPhone === "") {
                root.showPassiveNotification(Style.plain(n.text), "long");
                continue;
            }
            const component = Qt.createComponent(Qt.resolvedUrl("dialogs/LogoutNoticeDialog.qml"));
            const dialog = component.status === Component.Ready ? component.createObject(root, {
                "title": n.title,
                "text": n.text,
                "onPhone": n.onPhone
            }) as Kirigami.PromptDialog : null;
            if (dialog === null) {
                // (Never silent: the instruction goes out in passing at least.)
                root.showPassiveNotification(Style.plain(n.text + " " + n.onPhone), "long");
                continue;
            }
            root.logoutDialog = dialog;
            root.openLogoutNotice = n;
            dialog.closed.connect(() => {
                if (root.logoutDialog === dialog) {
                    root.logoutDialog = null;
                    root.openLogoutNotice = null;
                }
                // (After this dialog is gone: the next one opens on its own.)
                Qt.callLater(root.tellLogoutNotices);
            });
            dialog.open();
        }
    }

    // Puts a question about an account (`ReplaceAccountDialog.qml`: "Remove and Link
    // Again?", "Remove Account?", "Unlink This Computer?"); one at a time. `cancel` tells
    // the viewmodel when it cannot be shown.
    function askAboutAccount(question: var, cancel: var): void {
        if (root.replaceDialog !== null) {
            // (A modal question is open: a second trigger is dropped, as before.)
            return;
        }
        const component = Qt.createComponent(Qt.resolvedUrl("dialogs/ReplaceAccountDialog.qml"));
        if (component.status !== Component.Ready) {
            console.error("the question could not be made: " + component.errorString());
            cancel();
            return;
        }
        const dialog = component.createObject(root, {
            "question": question
        }) as Kirigami.PromptDialog;
        if (dialog === null) {
            cancel();
            return;
        }
        root.replaceDialog = dialog;
        dialog.closed.connect(() => {
            root.replaceDialog = null;
        });
        dialog.open();
    }
    // The page stack is being rebuilt for a replaced account's new link.
    property bool restacking: false

    // ---- Quitting while work runs asks first (F8) ------------------------------------
    // The question "Quit While …?" while it is open (tests read it).
    property Kirigami.PromptDialog quitDialog: null
    // The user agreed to quit: the close that follows asks nothing more.
    property bool quitAgreed: false

    // Quits (`closedForGood`): at once, or after the user agreed when a network is still
    // pushing the history of a new link (WhatsApp: what a quit cuts off may never arrive), a
    // history transfer or the update after a new link still runs (those two go on at the next
    // start). The default answer keeps the application open. (The tray's
    // own Quit asks nothing: the window may be hidden or not exist then.)
    function quitAsking(): void {
        const question = root.quitAgreed ? undefined : App.quitQuestion();
        if (question === undefined || question === null) {
            root.closedForGood();
            return;
        }
        if (root.quitDialog !== null) {
            return;
        }
        const component = Qt.createComponent(Qt.resolvedUrl("dialogs/StopLinkingDialog.qml"));
        if (component.status !== Component.Ready) {
            console.error("the question could not be made: " + component.errorString());
            return;
        }
        const dialog = component.createObject(root, {
            "question": question,
            "confirmIcon": "application-exit",
            "action": () => {
                root.quitAgreed = true;
                root.closedForGood();
            }
        }) as Kirigami.PromptDialog;
        if (dialog === null) {
            return;
        }
        root.quitDialog = dialog;
        dialog.closed.connect(() => {
            root.quitDialog = null;
        });
        dialog.open();
    }

    // The question "Restart Kourier?" while it is open (tests read it).
    property Kirigami.PromptDialog restartDialog: null

    // Runs `action`, which restarts Kourier ("Restart Now", "Turn On and Restart", the banner's
    // button of a turned-off network), after the user agreed when work still runs: a restart
    // ends the application like a quit (K-A fix U4), so the same question about the history a
    // network is pushing (it does not go on at the next start), a history transfer or the update
    // after a link comes first, with "Restart" as its answer. Nothing is
    // written before the answer: `action` writes the preference and the one-shot link, and a
    // "Cancel" leaves no trace of the restart.
    function restartAsking(action: var): void {
        const question = root.quitAgreed ? undefined : App.quitQuestion();
        if (question === undefined || question === null) {
            action();
            return;
        }
        if (root.restartDialog !== null) {
            return;
        }
        const component = Qt.createComponent(Qt.resolvedUrl("dialogs/StopLinkingDialog.qml"));
        if (component.status !== Component.Ready) {
            console.error("the question could not be made: " + component.errorString());
            return;
        }
        const dialog = component.createObject(root, {
            "question": {
                "title": L10n.i18nc("@title:window question before the application restarts while work runs", "Restart Kourier?"),
                "text": question.text,
                "confirm": L10n.i18nc("@action:button restart the application", "Restart"),
                "cancel": question.cancel
            },
            "confirmIcon": "view-refresh",
            "action": action
        }) as Kirigami.PromptDialog;
        if (dialog === null) {
            return;
        }
        root.restartDialog = dialog;
        dialog.closed.connect(() => {
            root.restartDialog = null;
        });
        dialog.open();
    }

    // ---- A link in progress is never cancelled silently (F3) -------------------------
    // The question "Stop Linking?" while it is open (tests read it).
    property Kirigami.PromptDialog stopLinkingDialog: null

    // Runs `action`, which would leave the link page and so cancel a link that has not
    // finished (opening Settings, "Add Account", "Link Again" of the banner, a chat from
    // the list beside the link page), after the user agreed to stop that link. Nothing to
    // lose (no login, or one that is done, or an account the core has taken: Active, or
    // its history transfer runs): at once.
    function leavingLink(action: var): void {
        const question = App.abandonQuestion();
        if (question === undefined || question === null) {
            action();
            return;
        }
        // Asked already: the newest thing the user asked for (another chat, a notification,
        // a shortcut) is what "Stop Linking" then opens; one question stays.
        if (root.stopLinkingDialog !== null) {
            root.stopLinkingDialog.action = action;
            return;
        }
        const component = Qt.createComponent(Qt.resolvedUrl("dialogs/StopLinkingDialog.qml"));
        if (component.status !== Component.Ready) {
            console.error("the question could not be made: " + component.errorString());
            return;
        }
        const dialog = component.createObject(root, {
            "question": question,
            "action": action
        }) as Kirigami.PromptDialog;
        if (dialog === null) {
            return;
        }
        root.stopLinkingDialog = dialog;
        dialog.closed.connect(() => {
            root.stopLinkingDialog = null;
        });
        dialog.open();
    }
    // How many passing notices about ended transfers were shown (tests count them).
    property int transferNotices: 0

    // What the last chat notice told a screen reader and how (tests read both).
    property string chatToldAnnounced: ""
    property int chatToldPoliteness: -1

    // A chat notice (A10d): a passing notice that stays as long as the others of its kind, and
    // the same words told to a screen reader politely, because the focus stays where the
    // person's action left it and nothing else says that it did not work. The text is the
    // viewmodel's own sentence; it goes through `Style.plain` like every other notice.
    function tellChat(text: string, duration: var): void {
        root.chatToldAnnounced = text;
        root.chatToldPoliteness = Accessible.Polite;
        root.showPassiveNotification(Style.plain(text), duration);
        root.contentItem.Accessible.announce(text, Accessible.Polite);
    }

    // "Chat archived" with its Undo, for Desktop's 8 seconds (Kirigami's "short" is 4). What
    // undoing needs stands in `undoStanding` for as long as the notice does: the chat, whether
    // it was pinned (archiving unpinned it) and whether the archive closed it (the Undo opens
    // it again then, as a chat he chose: through `openingChat`).
    property var undoStanding: null
    // The chat the last archive asked for, if it was the open one (`archiveChat` sets it,
    // the notice takes it: the archive closes the open chat before the notice comes).
    property real archivedOpen: 0
    Timer {
        id: undoTimer
        interval: 8000
        onTriggered: root.undoStanding = null
    }

    // Each toast has its OWN standing `s`: its Undo undoes its own archive (Kirigami keeps up to
    // four toasts, each with its callback) and works for as long as the toast stands, however
    // long the pointer rests on it. `undoStanding` is the newest one, for Ctrl+Shift+U, and
    // `undoTimer` only bounds the key's window.
    function tellArchived(conv: real, wasPinned: bool, text: string): void {
        const s = {
            "conv": conv,
            "wasPinned": wasPinned,
            "reopen": root.archivedOpen === conv,
            "done": false
        };
        root.undoStanding = s;
        root.archivedOpen = 0;
        undoTimer.restart();
        root.chatToldAnnounced = text;
        root.chatToldPoliteness = Accessible.Polite;
        root.showPassiveNotification(Style.plain(text), 8000, L10n.i18nc("@action:button", "Undo"), () => root.undoArchive(s));
        root.contentItem.Accessible.announce(text, Accessible.Polite);
    }

    // Undo of the archive `standing` (a toast's own, or the newest for the key): once (a notice
    // left standing does nothing on a second press). The chat opens again only when it was
    // the open one and nothing else has been opened since (L1).
    function undoArchive(standing: var): void {
        if (standing === null || standing === undefined || standing.done) {
            return;
        }
        standing.done = true;
        if (root.undoStanding === standing) {
            root.undoStanding = null;
            undoTimer.stop();
        }
        App.undoArchive(standing.conv, standing.wasPinned);
        if (standing.reopen && root.activeTimeline.conversationId === 0) {
            root.openingChat(standing.conv, null);
        }
    }

    // The ONE path of the menu's Archive and the key: the notice comes when the row says so.
    function archiveChat(conv: real): void {
        root.archivedOpen = conv === root.activeTimeline.conversationId ? conv : 0;
        App.setArchived(conv, true);
    }

    function unarchiveChat(conv: real): void {
        App.setArchived(conv, false);
    }

    // "Mute Until": the dialog is loaded by URL the first time it is asked for and kept (a file a
    // page names loads its imports with the page; this one needs KDE's calendar and clock, which
    // nothing before it does). One instance; it starts at the day after and the core's 8:00.
    property var muteUntilDialog: null
    // `returnTo`: the item that had the keyboard when the question was asked (the list, when
    // the row menu's Until… was chosen by key); it gets the keyboard back when the dialog closes.
    function askMuteUntil(conv: real, returnTo: var): void {
        if (root.muteUntilDialog === null) {
            const component = Qt.createComponent(Qt.resolvedUrl("dialogs/MuteUntilDialog.qml"));
            if (component.status !== Component.Ready) {
                console.error("MuteUntilDialog: " + component.errorString());
                return;
            }
            root.muteUntilDialog = component.createObject(root);
        }
        if (root.muteUntilDialog !== null) {
            root.muteUntilDialog.conversation = conv;
            root.muteUntilDialog.returnTo = returnTo ? returnTo : null;
            // The fields are right before the first frame of the fade-in (M6).
            root.muteUntilDialog.startAgain();
            root.muteUntilDialog.open();
        }
    }

    // Ctrl+Shift+A (`archive`) and Ctrl+Shift+U on the open chat, as Signal Desktop's: nothing
    // when it already is so. With the sync switch off the greyed line's words are the answer.
    function keyArchive(archive: bool): void {
        const conv = root.activeTimeline.conversationId;
        if (conv === 0) {
            return;
        }
        const offer = root.activeTimeline.chatOffer;
        if (!offer.shown) {
            return;
        }
        if (!offer.on) {
            root.tellChat(offer.why, "long");
            return;
        }
        if (offer.archived === archive) {
            return;
        }
        if (archive) {
            root.archiveChat(conv);
        } else {
            root.unarchiveChat(conv);
        }
    }

    // The fixture pages of the message actions: each opens a chat (`App` does that) and, for
    // `setupPages`, one of its popups or composer modes once the rows are there.
    readonly property list<string> actionPages: ["quotes", "split", "splitnarrow", "splitpick", "menu", "react", "reactions", "edit", "reply", "delete", "timer", "placeholders", "readmore", "calls", "unavailable", "album", "albumviewer", "viewerzoom", "revisions", "timercustom", "clearchat", "mediamenu", "deleteattachment", "wafailed", "uploading", "uploadfile", "person", "group", "chatmedia", "safety", "safetychanged", "share", "timerpending", "save", "muteuntil", "chatmenu", "chatmenuoff", "groupadmin", "searchchat", "emojipicker", "emojisearch", "hoverpins", "highlight", "model3d", "model3dviewer", "select", "exportdialog", "exportdone", "forward", "forwardcard"]
    readonly property list<string> setupPages: ["split", "splitnarrow", "splitpick", "menu", "react", "reactions", "edit", "reply", "delete", "timer", "album", "albumviewer", "viewerzoom", "revisions", "files", "originals", "timercustom", "clearchat", "mediamenu", "deleteattachment", "folder", "folderfiles", "folderzip", "expiry", "networks", "appearance", "filters", "filteredit", "waqr", "waphone", "wacode", "thqr", "themoji", "tgqr", "tgcode", "tgpassword", "wahistory", "walost", "transferwait", "transferdownload", "transferread", "transferupdate", "transferbanner", "transferfailed", "transferask", "archive", "person", "group", "chatmedia", "safety", "safetychanged", "share", "timerpending", "save", "location", "locationmany", "locationnever", "accounts", "accountsdrawer", "media", "rowmenu", "rowmenuoff", "rowmenumuted", "mutemenu", "muteuntil", "chatmenu", "chatmenuoff", "groupadmin", "searchresults", "searchchat", "searchindexing", "emojipicker", "emojisearch", "hoverpins", "highlight", "regexsearch", "model3d", "model3dviewer", "select", "exportdialog", "exportdone", "forward", "forwardcard"]
    // The fixture pages of blended chats: no chat opens, the list stands with its accounts.
    readonly property list<string> accountsPages: ["accounts", "accountsdrawer"]
    // The fixture pages of the chat list's own menus (A10d): no chat opens; `rowMenuPageReady` opens the
    // menu of a fixed row (the first ordinary chat) through the list's own `openMenu`.
    readonly property list<string> rowMenuPages: ["rowmenu", "rowmenuoff", "rowmenumuted", "mutemenu"]
    // How far that page has come (0: nothing done yet) and the chat it stands on.
    property int rowMenuStage: 0
    // The `select` page picked its rows.
    property bool selectPicked: false
    property bool exportShown: false
    property bool forwardShown: false
    property bool forwardTicked: false
    property bool forwardStaged: false
    // The page `chatmenuoff` asked for the switch off (once).
    property bool chatMenuOffAsked: false
    property real rowMenuChat: 0
    // The fixture pages of the message search (G1j): `searchresults` and `searchindexing` type the word
    // into the list's field (no chat opens); `searchchat` opens the busiest chat and starts the
    // one-chat search there. The word is one of the fixture's own (a group's name holds it too).
    readonly property list<string> searchPages: ["searchresults", "searchindexing", "searchchat", "regexsearch"]
    readonly property string searchPageWord: "garden"
    // `regexsearch` (R5): the `.*` toggle is on and the pattern typed matches the same word.
    readonly property string regexPageWord: "\\bgard\\w+"
    property int searchPageStage: 0
    // `searchresults` and `searchchat`: the word was typed again once the index was whole.
    property bool searchPageFresh: false
    // The fixture pages of a link in a state of a history transfer, with that state.
    readonly property list<string> transferLinkPages: ["transferwait", "transferdownload", "transferread"]
    readonly property list<string> transferPages: ["transferwait", "transferdownload", "transferread", "transferupdate", "transferbanner", "transferfailed", "transferask", "wahistory", "walost"]
    // The fixture pages at a step of WhatsApp's link page (K-A): the link walks to the step.
    readonly property list<string> whatsappLinkPages: ["waqr", "waphone", "wacode"]
    // The fixture pages at a step of Threema's link page (TH4 KH1c): the QR code, and the three emoji
    // after the scan.
    readonly property list<string> threemaLinkPages: ["thqr", "themoji"]
    // The fixture pages at a step of Telegram's link page (KT-1b): the QR code, the code of the
    // phone-number way, and the two-step password after it.
    readonly property list<string> telegramLinkPages: ["tgqr", "tgcode", "tgpassword"]
    property bool pageReady: false
    // The page `transferask` asked its question (once).
    property bool transferAsked: false
    // The fixture pages of the history transfer: whether what the page shows is there.
    function transferPageReady(): bool {
        switch (App.startPage) {
        // (A link's transfer is in the banner: the link page closed when the core took the
        // account.)
        case "transferwait":
            return App.bannerTransfer.phase === "waiting";
        case "transferdownload":
            return App.bannerTransfer.phase === "downloading" && App.bannerTransfer.progress > 0;
        case "transferread":
            return App.bannerTransfer.phase === "reading" && App.bannerTransfer.progress > 0;
        case "transferupdate":
            return App.bannerTransfer.phase === "updating" && App.bannerTransfer.progress > 0;
        case "transferbanner":
            // (The backend says where the transfer stands once it may connect.)
            return App.bannerTransfer.phase === "downloading";
        case "transferfailed":
            return App.bannerTransfer.phase === "failed";
        case "wahistory":
            // (The phone's history still comes: the quiet line of the account banner.)
            return App.bannerTransfer.phase === "history";
        case "walost":
            // (Part of it never arrived: the quiet line that stays, in the state banner.)
            return App.bannerKind === "historyLost";
        case "transferask":
            if (!root.transferAsked) {
                if (root.settingsPage !== null && App.historyAccounts.length > 0) {
                    root.settingsPage.showAccounts();
                    root.transferAsked = true;
                    App.askReplace(App.historyAccounts[0].account);
                }
                return false;
            }
            return root.replaceDialog !== null && root.replaceDialog.opened;
        }
        return false;
    }
    // The folder pages answered the question (once).
    property bool folderAnswered: false
    // The location pages pressed Send (once).
    property bool locationPressed: false
    // The media page sent its photo (once), when the timeline had this many rows.
    property int mediaSentAt: -1
    Timer {
        id: expirySetup
        property int tries: 0
        property bool dialogShown: false
        interval: 150
        repeat: true
        onTriggered: {
            expirySetup.tries += 1;
            if (expirySetup.tries > 80) {
                console.error("page " + App.startPage + ": the core never said what holds");
                Qt.exit(1);
                return;
            }
            if (root.settingsPage === null || App.settings.expiryPolicy === "") {
                return;
            }
            // The filter pages wait for the core's list too.
            if ((App.startPage === "filters" || App.startPage === "filteredit") && !App.filters.loaded) {
                return;
            }
            if (App.startPage === "filters" || App.startPage === "filteredit") {
                root.settingsPage.showFilters();
                if (App.startPage === "filteredit") {
                    if (expirySetup.dialogShown !== true) {
                        expirySetup.dialogShown = true;
                        root.settingsPage.showFilterDialog();
                        return;
                    }
                    if (!root.settingsPage.filterDialogOpened()) {
                        return;
                    }
                }
            } else if (App.startPage === "networks") {
                root.settingsPage.showNetworks();
            } else if (App.startPage === "appearance") {
                root.settingsPage.showAppearance();
            } else {
                root.settingsPage.showExpiry();
            }
            expirySetup.stop();
            root.pageReady = true;
        }
    }
    // The pages `waqr`, `waphone` and `wacode`: the link of WhatsApp stands at its QR code, takes
    // the phone-number way (the button the page offers) and types a number; each stops at its
    // step and the picture is taken there.
    Timer {
        id: whatsappLinkSetup
        property int tries: 0
        interval: 150
        repeat: true
        onTriggered: {
            whatsappLinkSetup.tries += 1;
            if (whatsappLinkSetup.tries > 80) {
                console.error("page " + App.startPage + ": the link never got there (" + App.login.kind + ")");
                Qt.exit(1);
                return;
            }
            const kind = App.login.kind;
            if (App.startPage === "waqr") {
                if (kind === "qr") {
                    whatsappLinkSetup.stop();
                    root.pageReady = true;
                }
                return;
            }
            if (kind === "qr" && App.login.otherFlow !== "") {
                // (A second call while the new login is starting is ignored by the core's
                // debounce.)
                App.login.switchFlow();
                return;
            }
            if (kind === "input" && App.login.flow === "pair-code") {
                if (App.startPage === "waphone") {
                    whatsappLinkSetup.stop();
                    root.pageReady = true;
                    return;
                }
                App.login.setField("phone", "+380501234567");
                App.login.submit();
                return;
            }
            if (kind === "code" && App.startPage === "wacode") {
                whatsappLinkSetup.stop();
                root.pageReady = true;
            }
        }
    }
    // The pages `thqr` and `themoji`: the link of Threema stands at its QR code, or one step on at the
    // three emoji the fake's phone shows after the scan; the picture is taken there.
    Timer {
        id: threemaLinkSetup
        property int tries: 0
        interval: 150
        repeat: true
        onTriggered: {
            threemaLinkSetup.tries += 1;
            if (threemaLinkSetup.tries > 80) {
                console.error("page " + App.startPage + ": the link never got there (" + App.login.kind + ")");
                Qt.exit(1);
                return;
            }
            if (App.login.kind === (App.startPage === "thqr" ? "qr" : "emoji")) {
                threemaLinkSetup.stop();
                root.pageReady = true;
            }
        }
    }
    // The pages `tgqr`, `tgcode` and `tgpassword`: the link of Telegram stands at its QR code,
    // takes the phone-number way (the button the page offers), types the fixture's number
    // (`FixtureOptions::TELEGRAM_NUMBER`) and, for the password page, the fixture's code
    // (`FixtureOptions::TELEGRAM_CODE`); each stops at its step and the picture is taken there.
    Timer {
        id: telegramLinkSetup
        property int tries: 0
        interval: 150
        repeat: true
        onTriggered: {
            telegramLinkSetup.tries += 1;
            if (telegramLinkSetup.tries > 80) {
                console.error("page " + App.startPage + ": the link never got there (" + App.login.kind + ")");
                Qt.exit(1);
                return;
            }
            const kind = App.login.kind;
            if (App.startPage === "tgqr") {
                if (kind === "qr") {
                    telegramLinkSetup.stop();
                    root.pageReady = true;
                }
                return;
            }
            if (kind === "qr" && App.login.otherFlow !== "") {
                // (A second call while the new login is starting is ignored by the core's
                // debounce.)
                App.login.switchFlow();
                return;
            }
            if (kind !== "input" || App.login.fields.length === 0) {
                return;
            }
            const field = App.login.fields[0].id;
            if (field === "phone") {
                App.login.setField("phone", "+999000510284");
                App.login.submit();
            } else if (field === "code") {
                if (App.startPage === "tgcode") {
                    // (With the button the page offers for a new code: the picture shows it.)
                    if (App.login.resendLabel !== "") {
                        telegramLinkSetup.stop();
                        root.pageReady = true;
                    }
                    return;
                }
                App.login.setField("code", "12345");
                App.login.submit();
            } else if (field === "password" && App.startPage === "tgpassword") {
                telegramLinkSetup.stop();
                root.pageReady = true;
            }
        }
    }
    // The fixture page `about`: the About page on the page stack. Created by URL, when asked.
    function openAbout(): void {
        const component = Qt.createComponent(Qt.resolvedUrl("settings/AboutKourierPage.qml"));
        if (component.status !== Component.Ready) {
            console.error("page about: " + component.errorString());
            Qt.exit(1);
            return;
        }
        pageStack.push(component.createObject(root));
    }
    // The pages `accounts` and `accountsdrawer`: waits for the three accounts and their chats,
    // hides the second account once (the same call a click on its checkbox makes), waits for
    // the list to follow and for its unread count on the panel; `accountsdrawer` then shows the
    // side panel (wide: expanded; narrow: opened over the list). True when the page is final.
    // The first ordinary chat of the list that stands whole on screen: nothing unread, not
    // marked, pinned, muted or Note to Self. 0 while there is none.
    // (A row of the list as an untyped value: its delegate's own properties are not on `Item`.)
    function listRow(l: var, i: int): var {
        return l.itemAtIndex(i);
    }

    function plainRowId(): real {
        const l = listPage.conversations;
        for (let i = 0; i < l.count; ++i) {
            const r = root.listRow(l, i);
            if (r && !r.isSelf && r.unread === 0 && !r.markedUnread && !r.pinned && !r.muted && r.y >= l.contentY && r.y + r.height <= l.contentY + l.height) {
                return r.conversationId;
            }
        }
        return 0;
    }

    function rowOfConversation(id: real): var {
        const l = listPage.conversations;
        for (let i = 0; i < l.count; ++i) {
            const r = root.listRow(l, i);
            if (r && r.conversationId === id) {
                return r;
            }
        }
        return null;
    }

    // The pages `rowmenu`, `rowmenuoff`, `rowmenumuted` and `mutemenu`: one step per call (the
    // timer calls again while this says false). `rowmenuoff` turns the first account's switch off
    // and `rowmenumuted` mutes the row for an hour, both through the real invokables, and wait for
    // the echo before the menu opens; `mutemenu` opens the Mute sub-menu of the open menu.
    function rowMenuPageReady(): bool {
        const page = App.startPage;
        if (!listPage.rowsShown || App.accountActions.length === 0) {
            return false;
        }
        if (root.rowMenuChat === 0) {
            root.rowMenuChat = root.plainRowId();
            return false;
        }
        const id = root.rowMenuChat;
        const offer = App.chatOffer(id);
        if (root.rowMenuStage === 0) {
            if (page === "rowmenuoff") {
                App.setRoamingWrites(App.accountActions[0].account, false);
            } else if (page === "rowmenumuted") {
                App.muteFor(id, "hour");
            }
            root.rowMenuStage = 1;
            return false;
        }
        if (root.rowMenuStage === 1) {
            if (page === "rowmenuoff" && offer.on !== false) {
                return false;
            }
            if (page === "rowmenumuted" && offer.muted !== true) {
                return false;
            }
            const row = root.rowOfConversation(id);
            if (row === null || (page === "rowmenumuted" && !row.muted)) {
                return false;
            }
            listPage.openMenu(row, -1, -1);
            root.rowMenuStage = 2;
            return false;
        }
        if (!listPage.rowMenu.opened) {
            return false;
        }
        if (page === "mutemenu") {
            if (!listPage.muteSub.opened) {
                listPage.rowMenu.currentIndex = 0;
                for (let i = 0; i < listPage.rowMenu.count; ++i) {
                    if (listPage.rowMenu.itemAt(i) === listPage.rowMenu.muteItem) {
                        listPage.rowMenu.currentIndex = i;
                    }
                }
                listPage.muteSub.open();
                return false;
            }
        }
        return true;
    }

    // The pages `searchresults` and `searchindexing`: the word typed into the list's own field (so the
    // field's own delay and the App's own search run), then the wait for what the page shows: the
    // hits answered and, for `searchresults`, the index ready; for `searchindexing` its line
    // standing under the header. The first hit row then holds the keyboard (its frame).
    function searchPageReady(): bool {
        const indexing = App.startPage === "searchindexing";
        const inChat = App.startPage === "searchchat";
        if (!listPage.rowsShown) {
            return false;
        }
        if (root.searchPageStage === 0) {
            // `regexsearch`: the toggle first (what the person clicks), then the pattern is typed.
            if (App.startPage === "regexsearch") {
                listPage.regexOn = true;
                listPage.modeChanged();
            }
            // `searchchat`: the busiest chat is open (the App opened it); the chip names it.
            if (inChat) {
                if (App.timeline.chatSearchable !== true) {
                    return false;
                }
                root.searchInChat();
            }
            root.searchPageStage = 1;
            return false;
        }
        const r = App.searchResults;
        if (root.searchPageStage === 1) {
            // (After a clear: once the search has ended, so the field's text is a new search.)
            if (r.active) {
                return false;
            }
            listPage.searchField.text = App.startPage === "regexsearch" ? root.regexPageWord : root.searchPageWord;
            root.searchPageStage = 2;
            return false;
        }
        if (!r.active || !r.answered || r.loading || r.count === 0 || !listPage.hitsShown || r.indexReady === indexing || (inChat && r.chat !== listPage.searchChat)) {
            return false;
        }
        // `searchresults` and `searchchat`: the word once more, now that the index is whole. How
        // many hits the first search ends with depends on how far the index had come when it was
        // answered (pages chained while it filled are read again whole once it is ready), and in a
        // render where the index won the race the list held one page: the scroll bar's thumb
        // differed (1 render in 3 under load, 2026-11-12). A search of a whole index answers one
        // page, every time. The field is emptied (the search ends at once), then typed again.
        if (!indexing && !root.searchPageFresh) {
            root.searchPageFresh = true;
            listPage.searchField.text = "";
            root.searchPageStage = 1;
            return false;
        }
        // (A few quiet ticks after the hits came: the rows settle before the first one takes the keyboard.)
        if (root.searchPageStage < 8) {
            root.searchPageStage += 1;
            return false;
        }
        return listPage.focusHit(0);
    }

    // The chat page's overflow menu: the shown toolbar button that holds actions (found as
    // `handleIn` finds the drawer's button; every toolbar button has a `menuActions` list, the
    // timer's is empty), its menu in the window's scene, opened. True once it is open.
    function menuButtonIn(item: var): var {
        if (item === null) {
            return null;
        }
        if (item.menuActions !== undefined && item.visible && item.menuActions.length > 0) {
            return item;
        }
        for (const child of item.children) {
            const found = root.menuButtonIn(child);
            if (found !== null) {
                return found;
            }
        }
        return null;
    }

    function openChatMenu(chatPage: var): bool {
        const button = root.menuButtonIn(chatPage.globalToolBarItem);
        if (button === null || !button.menu) {
            return false;
        }
        button.menu.popupType = QQC2.Popup.Item;
        if (!button.checked) {
            // `toggle()` does not emit `toggled`, and the button opens its menu only there: do
            // what a click does.
            button.checked = true;
            button.toggled();
        }
        return button.menu.opened;
    }

    function accountsPageReady(): bool {
        const actions = App.accountActions;
        if (actions.length < 3 || App.conversations.count === 0 || App.shownCount === 0) {
            return false;
        }
        if (actions[1].shown) {
            App.toggleAccount(actions[1].account);
            return false;
        }
        // The narrow open drawer leaves ONE account shown, so its greyed last box is in the
        // pictures (UI-12).
        const oneLeft = App.startPage === "accountsdrawer" && !root.wideScreen;
        if (oneLeft && actions[2].shown) {
            App.toggleAccount(actions[2].account);
            return false;
        }
        if (App.shownCount !== (oneLeft ? 1 : actions.length - 1) || App.hiddenUnread <= 0) {
            return false;
        }
        if (App.startPage === "accountsdrawer") {
            if (root.wideScreen) {
                accountsDrawer.collapsed = false;
                return !accountsDrawer.collapsed;
            }
            accountsDrawer.open();
            return accountsDrawer.drawerOpen;
        }
        return true;
    }
    Timer {
        id: pageSetup
        property int tries: 0
        property int settled: 0
        interval: 150
        repeat: true
        onTriggered: {
            pageSetup.tries += 1;
            if (pageSetup.tries > 80) {
                // The fixture lacks what the page shows: fail loudly, not with a wrong picture.
                console.error("page " + App.startPage + ": nothing to show it on");
                Qt.exit(1);
                return;
            }
            // The pages of blended chats need no chat either: three accounts and their chats
            // are there, the second account is hidden (its unread count is on its entry), and
            // for `accountsdrawer` the side panel is shown.
            if (root.accountsPages.indexOf(App.startPage) >= 0) {
                if (root.accountsPageReady()) {
                    pageSetup.stop();
                    root.pageReady = true;
                }
                return;
            }
            // The pages of the list's own menus need no chat either.
            if (root.rowMenuPages.indexOf(App.startPage) >= 0) {
                if (root.rowMenuPageReady()) {
                    pageSetup.stop();
                    root.pageReady = true;
                }
                return;
            }
            // The pages of the message search type into the list's field: no chat opens.
            if (root.searchPages.indexOf(App.startPage) >= 0) {
                if (root.searchPageReady()) {
                    pageSetup.stop();
                    root.pageReady = true;
                }
                return;
            }
            // The pages of the history transfer need no chat.
            if (root.transferPages.indexOf(App.startPage) >= 0) {
                if (root.transferPageReady()) {
                    pageSetup.stop();
                    root.pageReady = true;
                }
                return;
            }
            if (App.timeline.count === 0 || App.timeline.loading) {
                return;
            }
            const conv = App.timeline.conversationId;
            const chatPage = root.ensureChat();
            const view = chatPage.timeline;
            let done = true;
            switch (App.startPage) {
            case "menu":
                done = view.menuFor(App.findMessage("editable"));
                break;
            case "react":
                done = view.pickerFor(App.findMessage("photo"), true);
                break;
            case "reactions":
                done = view.reactionsFor(App.findMessage("reacted"));
                break;
            case "hoverpins":
                done = view.toolbarFor(App.findMessage("photo"));
                break;
            case "split":
            case "splitnarrow":
                // Both panes show their rows (the second page is made by `syncSplit` with the split; a narrow
                // window shows the second alone); the keyboard is in the second one, which stays the active pane.
                if (root.sideChatPage !== null && App.activePane !== 1 && root.pageSetupSplitTries++ < 3) {
                    root.sideChatPage.composer.focusInput();
                }
                done = App.activePane === 1 && App.split && App.sideTimeline.count > 0 && !App.sideTimeline.loading && root.sideChatPage !== null && root.inStack(root.sideChatPage) && root.sideChatPage.timeline.rowList.count > 0 && (!root.bothShown || chatPage.timeline.rowList.count > 0);
                break;
            case "splitpick":
                // The `chat` page's chat open and the chooser of "Open Chat Beside" started, once the
                // chat's rows are there (a narrow window then shows the list with the banner).
                if (!root.pageSetupPickStarted) {
                    if (chatPage.timeline.rowList.count > 0 && !App.timeline.loading) {
                        root.pageSetupPickStarted = true;
                        root.startChoosing();
                    }
                    done = false;
                } else {
                    done = root.choosingBeside && listPage.chooseBanner !== null && listPage.chooseBanner.visible && (!root.wideScreen || chatPage.timeline.rowList.count > 0);
                }
                break;
            case "highlight":
                // The four filters are saved and listed; the rows were marked in the same change
                // that listed them (the viewmodel re-reads the timeline with it).
                pageSetup.settled += 1;
                done = App.filters.loaded && App.filters.count === 4 && pageSetup.settled > 6;
                break;
            case "emojipicker":
            case "emojisearch":
                // Some use and a star, so the Favorites tab has both of its parts (the lists are the
                // fixture's own: nothing of anyone's).
                if (App.emojiLists.recent.length === 0) {
                    for (const e of ["😀", "🎉", "🙏", "🔥", "👀", "🚀", "☕", "🌈"]) {
                        App.emojiLists.used(e);
                    }
                    for (const e of ["🐶", "🍕", "🎸"]) {
                        App.emojiLists.toggleFavourite(e);
                    }
                }
                chatPage.composer.toggleEmoji(App.startPage === "emojisearch" ? "heart" : "");
                break;
            case "delete":
                done = view.deleteFor(App.findMessage("deletable"));
                break;
            case "edit":
                done = App.composer.editLast(conv);
                break;
            case "reply":
                done = App.composer.replyTo(conv, App.findMessage("photo"));
                break;
            case "timer":
                done = App.timeline.timerOptions.length > 0;
                if (done) {
                    chatPage.openTimerMenu();
                }
                break;
            case "revisions":
                // (Once the chat behind the dialog shows its pictures.)
                done = view.albumsShown() && view.historyFor(App.findMessage("edited"));
                break;
            case "album":
                // Every picture of the albums on screen is downloaded.
                done = view.albumsShown();
                break;
            case "model3d":
                // The albums chat, with the two models' stills derived and shown.
                done = view.albumsShown();
                break;
            case "model3dviewer":
                // The 3D viewer open on the coloured 3MF: the still fallback under the software
                // backend (the shots), the real scene under GL (a run on the PC also saves its
                // turned, zoomed and panned pictures: `modelScript`).
                done = root.model3dPageReady(view);
                break;
            case "albumviewer":
                // The newest album, at its second picture, once its tiles are downloaded.
                done = view.albumFor(App.findMessage("album"), 1);
                break;
            case "viewerzoom": {
                // The media chat's photo in the viewer (with its dimensions: the zoom levels need the natural
                // size), zoomed to 200 % around the viewport's centre; done once the zoomed decode is drawn.
                // The photo is 640 x 480: the wide window's Fit already shows it above its natural size,
                // so the viewer asks no zoomed decode there (`sharpAsked` stays false) and `still` is it.
                const photo = App.findMessage("photo");
                const shown = root.imageViewer;
                if (photo <= 0) {
                    done = false;
                } else if (root.viewersOpened === 0) {
                    const items = App.viewerItems(photo);
                    if (items.length > 0 && items[0].url !== "" && items[0].state === "local") {
                        root.openAlbumViewer(items, 0, photo);
                    }
                    done = false;
                } else if (shown === null || !shown.opened || !shown.shown || shown.fitW <= 0) {
                    done = false;
                } else if (shown.fitted) {
                    shown.zoomLevel(200);
                    done = false;
                } else {
                    done = shown.zoomPercent === 200 && (shown.sharpReady || !shown.sharpAsked);
                }
                break;
            }
            case "files":
            case "originals":
                // The core has looked at the files: the list says how they will be sent.
                done = App.composer.attachmentCount > 0 && App.composer.planned;
                break;
            case "folder":
                // Counted: the question is on screen.
                done = App.composer.folderAsking && chatPage.composer.folderDialog !== null && chatPage.composer.folderDialog.opened;
                break;
            case "folderfiles":
                // "Add the files inside": the folder's files stand in the list.
                if (!root.folderAnswered) {
                    root.folderAnswered = App.fixtureFolders("add");
                    done = false;
                } else {
                    done = !App.composer.folderAsking && App.composer.sendableCount > 1 && App.composer.planned;
                }
                break;
            case "folderzip":
                // "Send as .zip files": one archive is written, the other is being packed.
                if (!root.folderAnswered) {
                    root.folderAnswered = App.fixtureFolders("zip");
                    done = false;
                } else {
                    done = !App.composer.folderAsking && App.composer.sendableCount === 2 && App.composer.planned;
                }
                break;
            case "media":
                // A photo with a caption, sent once the core has looked at it (Send waits for
                // that); done when the message stands in the timeline, the list is empty and the
                // newest own message has left the outbox (the server took it): a picture taken
                // while it is queued or sending would show the clock icon and "Sending...".
                // A send that never ends is the page's own limit (80 tries) and fails loudly.
                if (root.mediaSentAt < 0) {
                    if (App.composer.attachmentCount > 0 && App.composer.planned) {
                        const before = App.timeline.count;
                        if (App.composer.send(conv, "Sent from Kourier")) {
                            root.mediaSentAt = before;
                        }
                    }
                    done = false;
                } else {
                    done = App.composer.attachmentCount === 0 && App.timeline.count > root.mediaSentAt && App.timeline.newestOwnLeftTheOutbox();
                }
                break;
            case "location":
            case "locationmany":
                // Send, once the core has looked at the files (Send waits for that): the
                // question about the located files is on screen.
                if (!root.locationPressed) {
                    if (App.composer.attachmentCount > 0 && App.composer.planned && chatPage.composer.canSend) {
                        root.locationPressed = true;
                        chatPage.composer.send();
                    }
                    done = false;
                } else {
                    done = App.composer.locationAsking && chatPage.composer.locationDialog !== null && chatPage.composer.locationDialog.opened;
                }
                break;
            case "locationnever":
                // "Never send" set (through Settings, its one owner): the located video stays
                // in the list with the reason that names the setting; the plain file goes.
                // Only on the throwaway fixture profile: the setting is saved (like "compact").
                if (App.fixture && App.settings.locationFiles !== 2) {
                    App.settings.locationFiles = 2;
                }
                done = App.composer.attachmentCount === 2 && App.composer.planned && App.composer.sendableCount === 1;
                break;
            case "timercustom":
                done = App.timeline.timerCustomMax > 0;
                if (done) {
                    chatPage.openCustomTimer();
                }
                break;
            case "clearchat":
                chatPage.askClear();
                break;
            case "mediamenu":
                // The menu of a message with pictures, once they are downloaded.
                done = view.albumsShown() && view.menuFor(App.findMessage("removable"));
                break;
            case "deleteattachment":
                done = view.albumsShown() && view.attachmentFor(App.findMessage("removable"));
                break;
            case "select":
            case "exportdialog":
            case "forward":
                // The media chat in the selection mode: the newest picture, file, text and notice
                // are picked (what the chat has), the bar stands in the composer's place.
                if (!root.selectPicked) {
                    const list = view.rowList;
                    const picks = {
                        "image": 0,
                        "file": 0,
                        "text": 0,
                        "notice": 0
                    };
                    for (let i = 0; i < list.count; ++i) {
                        const item = list.itemAtIndex(i);
                        const message = item as MessageDelegate;
                        const id = message?.messageId ?? (item as SystemRow)?.messageId ?? 0;
                        if (!(id > 0)) {
                            continue;
                        }
                        if (message === null) {
                            picks.notice = picks.notice || id;
                            continue;
                        }
                        for (const b of message.blocks) {
                            if (b.type === "image" || b.type === "album") {
                                picks.image = picks.image || id;
                            } else if (b.type === "file") {
                                picks.file = picks.file || id;
                            } else if (b.type === "text") {
                                picks.text = picks.text || id;
                            }
                        }
                    }
                    const ids = [picks.image, picks.file, picks.notice, picks.text].filter(id => id > 0);
                    if (ids.length >= 2 && picks.image > 0) {
                        App.timeline.selectStart(ids[0]);
                        for (let i = 1; i < ids.length && i < 3; ++i) {
                            App.timeline.selectToggle(ids[i]);
                        }
                        root.selectPicked = true;
                    }
                    done = false;
                } else if (App.startPage === "forward") {
                    // The forward dialog over the selection (the real plan of the core): the selection goes in, two
                    // chats are ticked, the first of the source's account and the first of another one (the Look
                    // shows for it), and the page waits for the answer to the newest ticks.
                    if (App.timeline.selecting && App.timeline.selectedCount >= 2) {
                        if (!root.forwardShown) {
                            root.forwardShown = true;
                            App.forwardOpen(App.timeline.selectedIds());
                        }
                        const dialog = root.forwardDialog;
                        if (dialog !== null && dialog.opened && dialog.view.title !== "" && App.forwardTargets.count >= 2 && !root.forwardTicked) {
                            let own = 0;
                            let other = 0;
                            for (let i = 0; i < App.forwardTargets.count; ++i) {
                                const id = App.forwardTargets.conversationAt(i);
                                if (App.forwardTargets.isNative(id)) {
                                    own = own || id;
                                } else {
                                    other = other || id;
                                }
                            }
                            if (own > 0 && other > 0) {
                                root.forwardTicked = true;
                                dialog.toggle(own);
                                dialog.toggle(other);
                            }
                        }
                        done = dialog !== null && root.forwardTicked && !dialog.planPending && dialog.view.lookShown === true;
                    } else {
                        done = false;
                    }
                } else if (App.startPage === "exportdialog") {
                    // The export dialog over the selection. The media chat holds no video, no
                    // disappearing and no view-once message, so the numbers are made up
                    // (`MainWindow.exportDialog.show`, what the plan's answer calls): every kind
                    // line, the disappearing tick with its warning and the left-out lines. No plan
                    // stands, so `App.exportNumbers` answers "" and the dialog keeps these.
                    if (App.timeline.selecting && App.timeline.selectedCount >= 2) {
                        if (!root.exportShown) {
                            root.exportShown = true;
                            root.openExportDialog(JSON.stringify({
                                "messages": 12,
                                "disappearing": 2,
                                "viewOnce": 1,
                                "gone": 0,
                                "pictures": {
                                    "count": 5,
                                    "bytes": 6710886,
                                    "size": "6.4 MB"
                                },
                                "videos": {
                                    "count": 1,
                                    "bytes": 48234496,
                                    "size": "46 MB"
                                },
                                "other": {
                                    "count": 2,
                                    "bytes": 183500,
                                    "size": "179 kB"
                                },
                                "totalSize": "52.6 MB",
                                "notHere": 1
                            }));
                        }
                        done = root.exportDialog !== null && root.exportDialog.opened;
                    } else {
                        done = false;
                    }
                } else {
                    done = App.timeline.selecting && App.timeline.selectedCount >= 2;
                }
                break;
            case "forwardcard":
                // A forward staged into a chat of ANOTHER account: the first two messages of the media chat, staged into
                // the first chat of the list whose account chip differs from the media chat's, which then opens (the
                // card with its Look, the text area, Send on).
                if (!root.forwardStaged) {
                    const list = view.rowList;
                    const ids = [];
                    for (let i = 0; i < list.count && ids.length < 2; ++i) {
                        const id = (list.itemAtIndex(i) as MessageDelegate)?.messageId ?? 0;
                        if (id > 0) {
                            ids.push(String(id));
                        }
                    }
                    const rows = listPage.conversations;
                    const source = App.timeline.conversationId;
                    let chip = "";
                    for (let i = 0; i < rows.count; ++i) {
                        const row = rows.itemAtIndex(i) as ConversationDelegate;
                        if (row !== null && row.conversationId === source) {
                            chip = row.accountBadge;
                        }
                    }
                    let target = 0;
                    for (let i = 0; i < rows.count && chip !== "" && target === 0; ++i) {
                        const row = rows.itemAtIndex(i) as ConversationDelegate;
                        if (row !== null && !row.isSelf && row.accountBadge !== "" && row.accountBadge !== chip) {
                            target = row.conversationId;
                        }
                    }
                    if (ids.length === 2 && target !== 0 && App.composer.stageForward(target, source, ids)) {
                        root.forwardStaged = true;
                        root.openConversation(target);
                    }
                    done = false;
                } else {
                    const card = chatPage.composer.forwardCard;
                    done = App.timeline.conversationId !== 0 && card.standing && card.staged.native === false && card.lookBox.visible;
                }
                break;
            case "exportdone":
                // The notice a written archive ends with, without a real export (the words are
                // `flow::end_of`'s for three messages, with the same contexts).
                if (root.noticeCount === 0) {
                    root.notify(L10n.i18ncp("@info:status, %1 how many messages were exported", "Exported %1 message", "Exported %1 messages", 3), L10n.i18nc("@action:button", "Show in Folder"), () => {});
                }
                done = root.noticeCount > 0;
                break;
            case "save":
                // The menu of an album message whose pictures are downloaded: "Save All…" beside the
                // toolbar that stays open under it, with its Save button.
                done = view.albumsShown() && view.menuFor(App.findMessage("removable"));
                break;
            case "person":
                // The details panel, once the core has answered with the person's page.
                if (!chatPage.panelOpen) {
                    chatPage.openPerson();
                    done = false;
                } else {
                    done = chatPage.panel !== null && chatPage.panel.loaded;
                }
                break;
            case "group":
            case "groupadmin":
                // The details panel on the group's page, once the core has answered: the group
                // level's own flag (`App.group`), not the person's.
                if (!chatPage.panelOpen) {
                    chatPage.openDetails();
                    done = false;
                } else {
                    done = chatPage.panel !== null && chatPage.panel.level === "group" && chatPage.panel.loaded;
                }
                break;
            case "chatmedia":
                // The details panel at "All Media and Files": the person's page first (the level opens on
                // top of it), then both lists of the level, once the core has answered.
                if (!chatPage.panelOpen) {
                    chatPage.openPerson();
                    done = false;
                } else if (chatPage.panel !== null && chatPage.panel.level === "person") {
                    if (chatPage.panel.loaded) {
                        chatPage.panel.showMedia(App.person.account ?? 0, App.person.chat ?? 0);
                    }
                    done = false;
                } else {
                    done = chatPage.panel !== null && chatPage.panel.level === "media" && chatPage.panel.loaded && App.mediaGrid.count > 0;
                }
                break;
            case "safety":
            case "safetychanged":
                // The level of the person's safety number (P5a): her page first (the level opens on
                // top of it, as "View Safety Number" does), then the backend's answer.
                if (!chatPage.panelOpen) {
                    chatPage.openPerson();
                    done = false;
                } else if (chatPage.panel !== null && chatPage.panel.level === "person") {
                    if (chatPage.panel.loaded && (App.person.safetyShown ?? false)) {
                        chatPage.panel.showSafety();
                    }
                    done = false;
                } else {
                    // (The buttons are dead for a second after the number appeared, X1: the picture waits for them.)
                    done = chatPage.panel !== null && chatPage.panel.level === "safety" && chatPage.panel.loaded && !chatPage.panel.safetyCooling;
                }
                break;
            case "muteuntil":
                // "Mute Until": the dialog the chat's menu opens, once it says it is open.
                if (App.timeline.chatOffer.shown !== true || App.timeline.chatOffer.on !== true) {
                    done = false;
                } else {
                    if (root.muteUntilDialog === null || !root.muteUntilDialog.opened) {
                        root.askMuteUntil(conv);
                    }
                    done = root.muteUntilDialog !== null && root.muteUntilDialog.opened;
                }
                break;
            case "chatmenu":
                // The chat page's overflow menu, open (the toolbar button that holds `menuActions`).
                if (App.timeline.chatOffer.shown !== true || App.timeline.chatOffer.on !== true) {
                    done = false;
                } else {
                    done = root.openChatMenu(root.activeChat);
                }
                break;
            case "chatmenuoff":
                // The same menu after the account's sync switch was turned off through the real invokable
                // (once; the page waits for the echo): every entry greyed, the reason wrapped under Archive.
                if (App.timeline.chatOffer.shown !== true) {
                    done = false;
                } else if (App.timeline.chatOffer.on !== false) {
                    if (!root.chatMenuOffAsked) {
                        root.chatMenuOffAsked = true;
                        App.setRoamingWrites(App.accountActions[0].account, false);
                    }
                    done = false;
                } else {
                    done = root.openChatMenu(root.activeChat);
                }
                break;
            case "timerpending":
                // Her page with a choice made in the Disappearing Messages selector (F4): "Apply" and "Cancel" stand
                // under the combo, nothing is sent.
                if (!chatPage.panelOpen) {
                    chatPage.openPerson();
                    done = false;
                } else if (chatPage.panel !== null && chatPage.panel.level === "person" && chatPage.panel.loaded) {
                    const other = (App.person.timers ?? []).find(t => t.secs > 0 && t.secs !== (App.person.timerSecs ?? 0));
                    done = other !== undefined && chatPage.panel.chooseTime(other.secs);
                } else {
                    done = false;
                }
                break;
            case "share":
                // The share picker of the person's page (P5c), over her page, once its rows have come, with the third row
                // chosen (SD8: the one cue of where the card goes is in the picture, also under the high-contrast looks).
                if (!chatPage.panelOpen) {
                    chatPage.openPerson();
                    done = false;
                } else if (chatPage.panel !== null && chatPage.panel.level === "person" && chatPage.panel.shareDialog === null) {
                    if (chatPage.panel.loaded && (App.person.shareShown ?? false)) {
                        chatPage.panel.openShare();
                    }
                    done = false;
                } else {
                    const picker = chatPage.panel?.shareDialog ?? null;
                    if (picker !== null && picker.opened && App.shareTargets.count > 2 && !picker.chosen) {
                        picker.choose(2);
                    }
                    done = picker !== null && picker.opened && picker.chosen;
                }
                break;
            }
            if (done) {
                pageSetup.stop();
                root.pageReady = true;
            }
        }
    }

    // `--page=archive`: open "Archived Chats" once the list says there is an archived chat, and
    // wait for its row. The fixture seeds its archived chat with the rest, so a list that never
    // says so fails loudly (no wrong picture).
    Timer {
        id: archiveSetup
        property int tries: 0
        interval: 150
        repeat: true
        onTriggered: {
            archiveSetup.tries += 1;
            if (archiveSetup.tries > 80) {
                console.error("page archive: the list never had an archived chat");
                Qt.exit(1);
                return;
            }
            if (!App.archiveShown) {
                if (App.archivedCount > 0 && listPage.rowsShown) {
                    listPage.openArchive();
                }
                return;
            }
            if (App.conversations.count > 0) {
                archiveSetup.stop();
                root.pageReady = true;
            }
        }
    }

    // `--page=model3dviewer`: opens the 3D viewer on a model of the albums chat by a tap on its
    // bubble (the real path), then, under GL only, walks the viewer through its states and saves a
    // picture per state beside the screenshot (`<name>-<state>.png`); the page is ready after.
    property bool modelPageDone: false
    property int modelStep: 0
    property int modelWait: 0
    readonly property string modelBase: App.screenshot.replace(/\.png$/, "")
    function openModelOf(view: var, which: string): bool {
        const delegate = view.delegateFor(App.findMessage(which));
        return delegate !== null && delegate.activateMedia();
    }
    function model3dPageReady(view: var): bool {
        if (root.modelPageDone) {
            return true;
        }
        if (root.software) {
            if (root.modelViewer === null) {
                root.openModelOf(view, "3mf");
                return false;
            }
            root.modelPageDone = root.modelViewer.opened;
            return root.modelPageDone;
        }
        if (!modelScript.running) {
            modelScript.start();
        }
        // (The walk takes longer than the setup's 80 tries.)
        pageSetup.tries = 0;
        return false;
    }
    // One step per tick: [what to do, picture name or ""]. A picture is taken a few ticks after the
    // state was set (llvmpipe needs the frames).
    Timer {
        id: modelScript
        interval: 400
        repeat: true
        property int lastStep: -1
        function shot(name: string, ticks: int): bool {
            root.modelWait += 1;
            if (root.modelWait < ticks) {
                return false;
            }
            root.modelWait = 0;
            App.saveScreenshot(root, root.modelBase + "-" + name + ".png");
            return true;
        }
        function openOn(which: string): bool {
            const v = root.modelViewer;
            if (v === null) {
                root.openModelOf(chatTimeline(), which);
                return false;
            }
            return v.opened && v.meshData.ready;
        }
        function chatTimeline(): var {
            return root.ensureChat().timeline;
        }
        onTriggered: {
            const v = root.modelViewer;
            const sphere = App.findMessage("sphere") !== 0;
            const aspect = v ? v.width / Math.max(1, v.height - 60) : 1;
            if (root.modelStep !== modelScript.lastStep) {
                modelScript.lastStep = root.modelStep;
                console.warn("model3d walk: step " + root.modelStep);
            }
            switch (root.modelStep) {
            case 0:
                // (A file still waiting for its derive: the debug sphere of ~100 k triangles.)
                if (!sphere && App.findMessage("file") !== 0 && root.modelWait < 120) {
                    root.modelWait += 1;
                    break;
                }
                if (modelScript.openOn(sphere ? "sphere" : "stl")) {
                    root.modelStep = 1;
                    root.modelWait = 0;
                }
                break;
            case 1:
                if (modelScript.shot(sphere ? "sphere-solid" : "fit-stl", sphere ? 10 : 6)) {
                    if (sphere) {
                        v.wireframe = true;
                        root.modelStep = 20;
                    } else {
                        v.close();
                        root.modelStep = 2;
                    }
                }
                break;
            case 2:
                if (modelScript.openOn("3mf")) {
                    root.modelStep = 3;
                    root.modelWait = 0;
                }
                break;
            case 3:
                if (modelScript.shot("fit-3mf", 6)) {
                    v.rotate(120, -40);
                    root.modelStep = 4;
                }
                break;
            case 4:
                if (modelScript.shot("turned", 4)) {
                    v.fit();
                    v.zoom(500, 0.5, 0.4, aspect);
                    root.modelStep = 5;
                }
                break;
            case 5:
                if (modelScript.shot("zoomed", 4)) {
                    v.fit();
                    v.pan(120, -70, v.height - 60);
                    root.modelStep = 6;
                }
                break;
            case 6:
                if (modelScript.shot("panned", 4)) {
                    v.showView("front");
                    root.modelStep = 7;
                }
                break;
            case 7:
                if (modelScript.shot("view-front", 4)) {
                    v.showView("top");
                    root.modelStep = 8;
                }
                break;
            case 8:
                if (modelScript.shot("view-top", 4)) {
                    v.showView("right");
                    root.modelStep = 9;
                }
                break;
            case 9:
                if (modelScript.shot("view-right", 4)) {
                    v.fit();
                    v.setFreeRotation(true);
                    v.rotate(150, 90);
                    root.modelStep = 10;
                }
                break;
            case 10:
                if (modelScript.shot("free", 4)) {
                    v.setFreeRotation(false);
                    v.wireframe = true;
                    v.fit();
                    root.modelStep = 11;
                }
                break;
            case 11:
                if (modelScript.shot("wire-3mf", 5)) {
                    v.wireframe = false;
                    v.fit();
                    // The Bambu A1's box (256 x 256 x 256) around the coloured 3MF.
                    App.settings.modelBox = "256x256x256";
                    root.modelStep = 12;
                }
                break;
            case 12:
                if (modelScript.shot("box-3mf", 6)) {
                    // The A1 mini's smaller box, from the front: where the model gets out shows.
                    App.settings.modelBox = "180x180x180";
                    v.showView("front");
                    root.modelStep = 13;
                }
                break;
            case 13:
                if (modelScript.shot("box-3mf-mini-front", 6)) {
                    App.settings.modelBox = "";
                    v.fit();
                    root.modelStep = 30;
                }
                break;
            case 20:
                if (modelScript.shot("sphere-wire", 10)) {
                    v.zoom(700, 0, 0, aspect);
                    root.modelStep = 21;
                }
                break;
            case 21:
                if (modelScript.shot("sphere-wire-zoom", 10)) {
                    v.wireframe = false;
                    v.fit();
                    App.settings.modelBox = "256x256x256";
                    root.modelStep = 22;
                }
                break;
            case 22:
                if (modelScript.shot("box-sphere", 10)) {
                    App.settings.modelBox = "";
                    v.close();
                    // On to the coloured 3MF (steps 2 to 13).
                    root.modelStep = 2;
                }
                break;
            case 30:
                modelScript.stop();
                root.modelPageDone = true;
                break;
            }
        }
    }

    // `--page=viewer`: open the viewer on the newest picture once it is downloaded.
    property bool viewerShown: false
    Timer {
        id: viewerShot
        interval: 200
        repeat: true
        onTriggered: {
            const url = App.latestImage();
            if (url !== "") {
                viewerShot.stop();
                // `image://kourier/att/<attachment>?v=<rev>`
                const attachment = Number(url.replace(/^image:\/\/kourier\/att\//, "").replace(/\?.*$/, "")) || 0;
                root.openImageViewer(attachment, url, false, true);
                root.viewerShown = true;
            }
        }
    }

    // --screenshot=<png>: the picture is taken when it is FINAL, not after a fixed wait. From the
    // page's ready moment (what must be on screen: the menu open, the viewer at picture 2, the
    // banner's phase), every 50 ms after a floor of 300 ms (delayed one-shots get to start), the
    // grab waits while anything holds it: pictures loading (the Images, the provider's
    // requests), thumbnails the core still derives while a picture of an attachment is shown
    // (they change the URLs once more), the events and reads that bring those URLs to the
    // models, downloads running, the page row sliding, the link page not gone yet (the link
    // pages of a transfer). When nothing holds it, the window is grabbed twice in a row (50 ms
    // apart): two equal frames are saved, so no animation is still going. At 30 s from the
    // first start of the timer the frame is saved as it is and Rust says
    // `shot: unsettled <what held>` (a failed render for `cargo xtask shots`). The 30 s run
    // from the FIRST start: when the start condition flips (a history that loads more), the
    // cap does not begin again. Spinners stand still in these runs (`Spinner.qml`).
    Timer {
        id: shotTimer

        // The first ready moment, in ms: the 30 s cap counts from it, whatever the timer does
        // after.
        property double began: 0
        // The latest ready moment, in ms: the floor of 300 ms counts from it.
        property double resumed: 0
        property bool done: false

        running: !shotTimer.done && App.screenshot !== "" && root.started && !(App.startPage === "history" && (App.timeline.hasOlder || App.timeline.loading)) && (App.startPage !== "viewer" || root.viewerShown) && (root.setupPages.indexOf(App.startPage) < 0 || root.pageReady)
        interval: 50
        repeat: true
        onRunningChanged: {
            if (shotTimer.running) {
                shotTimer.resumed = Date.now();
                if (shotTimer.began === 0) {
                    shotTimer.began = shotTimer.resumed;
                }
            }
        }
        onTriggered: {
            const waited = Math.round(Date.now() - shotTimer.began);
            if (Date.now() - shotTimer.resumed < 300) {
                return;
            }
            const holds = [];
            const loading = App.screenshotHolds(root);
            if (loading !== "") {
                holds.push(loading);
            }
            const downloading = App.timeline.downloadingAttachments() + (App.split ? App.sideTimeline.downloadingAttachments() : 0);
            if (downloading > 0) {
                holds.push("downloading " + downloading);
            }
            if (root.pageStack.columnView.moving) {
                holds.push("moving");
            }
            // The upload pages: the fake says the shares once a second; the picture waits for the
            // first figure to be on a ring.
            if ((App.startPage === "uploading" || App.startPage === "uploadfile") && App.timeline.uploadsReported() === 0) {
                holds.push("upload figures");
            }
            if (root.transferLinkPages.indexOf(App.startPage) >= 0 && root.linkPage !== null && root.inStack(root.linkPage)) {
                holds.push("link page");
            }
            if (holds.length === 0) {
                const luma = App.saveScreenshotIfStable(root, App.screenshot, waited);
                if (luma >= 0 || luma === -1) {
                    shotTimer.done = true;
                    Qt.exit(luma >= 0 ? 0 : 1);
                    return;
                }
                holds.push("frames differ");
            }
            if (waited >= 30000) {
                shotTimer.done = true;
                Qt.exit(App.saveUnsettledScreenshot(root, App.screenshot, holds.join(", "), waited) >= 0 ? 0 : 1);
            }
        }
    }
}
