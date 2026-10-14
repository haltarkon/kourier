pragma ComponentBehavior: Bound

import QtQuick
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
    // A conversation to open once the start page is up (activation before App.loaded).
    property real pendingConversation: 0

    // The window went to the tray (closed while a tray icon is shown).
    signal hiddenToTray()
    // Closing without a tray, or the Quit key: the application should quit.
    signal closedForGood()
    // The chat column is on screen with a conversation (read markers, notifications).
    readonly property bool chatShown: App.timeline.conversationId !== 0 && root.chatPage !== null && root.chatPage.Kirigami.ColumnView.inViewport && (root.wideScreen || root.pageStack.currentItem === root.chatPage)
    // The first frame was presented.
    property bool framed: false
    // The chat page takes its column right after the first frame (see `showListWith`).
    property bool chatPending: false
    // What stands in the chat column of a wide window until then.
    property Kirigami.Page standIn: null

    title: App.timeline.conversationId !== 0 && App.timeline.title !== "" ? App.timeline.title + " — Kourier" : "Kourier"
    width: App.windowWidth > 0 ? App.windowWidth : Kirigami.Units.gridUnit * 64
    height: App.windowHeight > 0 ? App.windowHeight : Kirigami.Units.gridUnit * 42
    minimumWidth: Kirigami.Units.gridUnit * 20
    minimumHeight: Kirigami.Units.gridUnit * 22

    // Wide: conversation list and chat side by side (§8.3).
    wideScreen: width >= Kirigami.Units.gridUnit * 46

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
        // A moment late when the last account goes (see `accountsShown`).
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
        showsNotice: !root.wideScreen || App.timeline.conversationId === 0 || App.timeline.cannotSend !== ""
        searchKeys: root.searchKeys
        onOpenConversation: id => root.openingChat(id)
        onSwitchedByKeyboard: id => root.switchTo(id)
        onBannerActivated: root.bannerPressed()
    }

    property ChatPage chatPage: null
    property WelcomePage welcomePage: null
    property LinkAccountPage linkPage: null
    property SettingsPage settingsPage: null

    Component {
        id: chatComponent
        ChatPage {
            visible: false
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
            return;
        }
        root.dropStandIn();
        if (pageStack.depth === 1 && pageStack.lastItem === listPage) {
            pageStack.push(root.freshChat());
            pageStack.currentIndex = 0;
        }
    }

    function openConversation(id: real): void {
        App.openConversation(id);
        root.chatPending = false;
        root.dropStandIn();
        // Keep the chat page when it already follows the list. Otherwise pop down to the list
        // (a clear is what printed the NavigationButtons TypeErrors) and push a chat page:
        // one that left the stack is made anew (`freshChat`).
        const chat = root.chatPage;
        if (chat !== null && root.pageStack.depth > 1 && root.pageStack.get(1) === chat) {
            root.popAfter(chat);
        } else {
            root.popToList();
            pageStack.push(root.freshChat());
        }
        pageStack.currentIndex = 1;
    }

    // The user chose chat `id` (a click on the list, the step keys, the quick switch, a
    // notification): in a wide window the list stands beside a link page, and opening a
    // chat rebuilds the stack, which cancels a link waiting at its QR code. Never silently
    // (F3): `leavingLink` asks first when something would be lost, and acts at once
    // otherwise. `then` runs after the chat is open.
    function openingChat(id: real, then: var): void {
        root.leavingLink(() => {
            root.openConversation(id);
            if (then) {
                then();
            }
        });
    }

    // A notification click or a link: open `id` now, or once the start page is up.
    function openFromActivation(id: real): void {
        if (id <= 0) {
            return;
        }
        if (root.started && App.hasAccounts) {
            root.openingChat(id, null);
        } else {
            root.pendingConversation = id;
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

    function openLink(network: string): void {
        // Pop an open link page first: that abandons its login (onPageRemoved below).
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

    function start(): void {
        if (root.started) {
            return;
        }
        root.started = true;
        const page = App.startPage;
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
            }
            return;
        }
        const chatPages = ["chat", "history", "media", "viewer", "compose", "files", "originals", "folder", "folderfiles", "folderzip"].concat(root.actionPages);
        const showsChat = chatPages.indexOf(page) >= 0 && App.timeline.conversationId !== 0;
        // An ordinary start opens no chat: the chat column can wait for the first frame.
        root.showListWith(!showsChat && root.pendingConversation <= 0);
        if (root.pendingConversation > 0) {
            const id = root.pendingConversation;
            root.pendingConversation = 0;
            root.openConversation(id);
            return;
        }
        if (showsChat) {
            const chatPage = root.ensureChat();
            if (!root.wideScreen) {
                pageStack.push(chatPage);
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
        } else if (page === "expiry") {
            // The settings page at "Disappearing messages", once the core has said what holds.
            root.openSettings();
            expirySetup.start();
        } else if (page === "about") {
            // The page Settings > "About Kourier" opens, on top of the settings page.
            root.openSettings();
            root.openAbout();
        } else if (page === "transferbanner" || page === "transferfailed" || page === "transferupdate") {
            // The backend says where the transfer stands once it may connect.
            pageSetup.start();
        } else if (page === "transferask") {
            // The settings page at "Accounts", with the question of its action open.
            root.openSettings();
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

    // The keyboard goes to the conversation list's search field. A narrow window shows the
    // chat or a page over the list: the list comes back first.
    function focusSearch(): void {
        if (pageStack.depth > 0 && pageStack.get(0) === listPage) {
            pageStack.currentIndex = 0;
        }
        listPage.focusSearch();
    }

    // Opens the chat `step` rows below (or above) the open one in the list as it is
    // shown; the ends do not wrap.
    function stepConversation(step: int): void {
        const id = App.adjacentConversation(App.timeline.conversationId, step);
        if (id !== 0 && id !== App.timeline.conversationId) {
            root.openingChat(id, null);
        }
    }

    // The quick switch opened `id`: the composer takes the keyboard once the chat is there.
    function switchTo(id: real): void {
        root.openingChat(id, () => {
            listPage.searchField.text = "";
            Qt.callLater(() => root.chatPage?.composer.focusInput());
        });
    }

    // The banner's button. Only "Link Again" of a logged-out account takes the link page's
    // place (a relink cancels the login that waits at its QR code); the other kinds
    // (challenge, restart) leave the link page where it is, so they never ask.
    function bannerPressed(): void {
        if (App.bannerKind === "loggedOut") {
            root.leavingLink(() => App.bannerActivate());
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

    // Tab walks a wide window the way the eye does: the search field, the conversations, the
    // timeline, the composer (Shift+Tab the other way). Kirigami's own order puts the chat's
    // footer and toolbar ahead of the list. While the account banner shows a button the
    // search field keeps the natural order, so that the button stays in reach.
    readonly property Item timelineStop: root.wideScreen && root.chatPage !== null && root.chatPage.open ? root.chatPage.timeline.rowList : null
    readonly property Item composerStop: root.timelineStop !== null ? root.chatPage.composer.firstStop : null
    Binding {
        target: listPage.searchField.KeyNavigation
        property: "tab"
        value: listPage.bannerShown ? null : listPage.conversations
    }
    Binding {
        target: listPage.conversations.KeyNavigation
        property: "backtab"
        value: listPage.searchField
    }
    Binding {
        target: listPage.conversations.KeyNavigation
        property: "tab"
        value: root.timelineStop
    }
    Binding {
        target: root.timelineStop ? root.timelineStop.KeyNavigation : null
        property: "backtab"
        value: listPage.conversations
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
    readonly property real openChat: App.timeline.conversationId
    onOpenChatChanged: {
        if (root.openChat === 0 && !root.wideScreen && root.chatPage !== null && root.pageStack.lastItem === root.chatPage && root.pageStack.depth > 1) {
            Qt.callLater(root.backToList);
        }
    }

    function backToList(): void {
        if (App.timeline.conversationId === 0 && root.chatPage !== null && root.pageStack.lastItem === root.chatPage) {
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
            onClosed: destroy()
        }
    }

    Component {
        id: confirmOpenComponent
        Kirigami.PromptDialog {
            id: confirm

            property real attachment: 0
            property string name: ""
            // The subtitle label guesses its format (AutoText) and the name comes from the
            // sender: a name such as `<img src="https://…">` would be markup that loads things.
            // Escape it and make the text rich on purpose, so it shows exactly as named.
            readonly property string escapedName: confirm.name.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")

            title: L10n.i18nc("@title:dialog", "Open This File?")
            subtitle: "<p>" + L10n.i18nc("@info", "“%1” opens in another application. Files can be harmful: open only what you expect from this sender.", confirm.escapedName) + "</p>"
            standardButtons: Kirigami.Dialog.NoButton
            customFooterActions: [
                Kirigami.Action {
                    text: L10n.i18nc("@action:button", "Open")
                    icon.name: "document-open"
                    onTriggered: {
                        App.openExternally(confirm.attachment);
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

    // Opens the viewer on `items` (one picture, or the pictures of an album) at `index`;
    // returns it. With `message` (the message they belong to) the viewer follows that
    // message's row from then on: a download that finishes, a picture that is deleted.
    function openAlbumViewer(items: var, index: int, message: real): ImageViewer {
        const viewer = viewerComponent.createObject(root, {
            "items": items,
            "index": index,
            "message": message
        }) as ImageViewer;
        viewer?.open();
        root.imageViewer = viewer;
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

    // The copy for another application is ready.
    Connections {
        target: App
        function onExternalReady(attachment: real, url: string): void {
            Qt.openUrlExternally(url);
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

    // Quits (`closedForGood`): at once, or after the user agreed when a history transfer or
    // the update after a new link still runs (they go on at the next start). The default
    // answer keeps the application open. (The tray's own Quit asks nothing: the window may
    // be hidden or not exist then.)
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

    // The fixture pages of the message actions: each opens a chat (`App` does that) and, for
    // `setupPages`, one of its popups or composer modes once the rows are there.
    readonly property list<string> actionPages: ["quotes", "menu", "react", "reactions", "edit", "reply", "delete", "timer", "placeholders", "readmore", "calls", "unavailable", "album", "albumviewer", "revisions", "timercustom", "clearchat", "mediamenu", "deleteattachment"]
    readonly property list<string> setupPages: ["menu", "react", "reactions", "edit", "reply", "delete", "timer", "album", "albumviewer", "revisions", "files", "originals", "timercustom", "clearchat", "mediamenu", "deleteattachment", "folder", "folderfiles", "folderzip", "expiry", "transferwait", "transferdownload", "transferread", "transferupdate", "transferbanner", "transferfailed", "transferask"]
    // The fixture pages of a link in a state of a history transfer, with that state.
    readonly property list<string> transferLinkPages: ["transferwait", "transferdownload", "transferread"]
    readonly property list<string> transferPages: ["transferwait", "transferdownload", "transferread", "transferupdate", "transferbanner", "transferfailed", "transferask"]
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
    Timer {
        id: expirySetup
        property int tries: 0
        interval: 150
        repeat: true
        onTriggered: {
            expirySetup.tries += 1;
            if (expirySetup.tries > 80) {
                console.error("page expiry: the core never said what holds");
                Qt.exit(1);
                return;
            }
            if (root.settingsPage === null || App.settings.expiryPolicy === "") {
                return;
            }
            root.settingsPage.showExpiry();
            expirySetup.stop();
            root.pageReady = true;
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
    Timer {
        id: pageSetup
        property int tries: 0
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
            case "albumviewer":
                // The newest album, at its second picture, once its tiles are downloaded.
                done = view.albumFor(App.findMessage("album"), 1);
                break;
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
            }
            if (done) {
                pageSetup.stop();
                root.pageReady = true;
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
            const downloading = App.timeline.downloadingAttachments();
            if (downloading > 0) {
                holds.push("downloading " + downloading);
            }
            if (root.pageStack.columnView.moving) {
                holds.push("moving");
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
