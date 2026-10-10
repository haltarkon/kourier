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
        enabled: App.hasAccounts
        settingsKeys: root.settingsKeys
        onOpenSettings: root.openSettings()
        onAddAccount: root.openWelcome()
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
        onOpenConversation: id => root.openConversation(id)
        onSwitchedByKeyboard: id => root.switchTo(id)
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
        pageStack.clear();
        pageStack.push(listPage);
        if (root.wideScreen) {
            if (later && !root.framed && root.chatPage === null) {
                root.chatPending = true;
                root.standIn = standInComponent.createObject(root) as Kirigami.Page;
                pageStack.push(root.standIn);
            } else {
                pageStack.push(root.ensureChat());
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
            pageStack.replace(root.ensureChat());
            pageStack.currentIndex = 0;
            page.destroy();
            return;
        }
        root.dropStandIn();
        if (pageStack.depth === 1 && pageStack.lastItem === listPage) {
            pageStack.push(root.ensureChat());
            pageStack.currentIndex = 0;
        }
    }

    function openConversation(id: real): void {
        App.openConversation(id);
        // Keep the chat page when it already follows the list. Otherwise rebuild the stack in
        // one go: pushing the chat page back on its own, after it left the stack, re-creates its
        // header while it has no page row (a Kirigami binding warning, ToolBarPageHeader.qml:61).
        const chat = root.ensureChat();
        root.chatPending = false;
        root.dropStandIn();
        if (root.pageStack.depth > 1 && root.pageStack.get(1) === chat) {
            root.popAfter(chat);
        } else {
            pageStack.clear();
            pageStack.push(listPage);
            pageStack.push(chat);
        }
        pageStack.currentIndex = 1;
    }

    // A notification click or a link: open `id` now, or once the start page is up.
    function openFromActivation(id: real): void {
        if (id <= 0) {
            return;
        }
        if (root.started && App.hasAccounts) {
            root.openConversation(id);
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
        pageStack.push(root.ensureWelcome());
    }

    function openLink(network: string): void {
        // Pop an open link page first: that abandons its login (onPageRemoved below).
        root.popAfter(root.ensureWelcome());
        App.login.start(network);
        pageStack.push(root.ensureLink());
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
        pageStack.push(root.ensureSettings());
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
        } else if (page === "transferbanner" || page === "transferfailed") {
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
            root.openConversation(id);
        }
    }

    // The quick switch opened `id`: the composer takes the keyboard once the chat is there.
    function switchTo(id: real): void {
        root.openConversation(id);
        Qt.callLater(() => root.chatPage?.composer.focusInput());
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
        onActivated: root.openSettings()
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
        onActivated: root.closedForGood()
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
            root.popAfter(listPage);
            root.pageStack.push(root.ensureLink());
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
            if (root.replaceDialog !== null) {
                return;
            }
            const component = Qt.createComponent(Qt.resolvedUrl("dialogs/ReplaceAccountDialog.qml"));
            if (component.status !== Component.Ready) {
                console.error("the question could not be made: " + component.errorString());
                App.cancelReplace();
                return;
            }
            const dialog = component.createObject(root, {
                "question": question
            }) as Kirigami.PromptDialog;
            if (dialog === null) {
                App.cancelReplace();
                return;
            }
            root.replaceDialog = dialog;
            dialog.closed.connect(() => {
                root.replaceDialog = null;
            });
            dialog.open();
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
    // The page stack is being rebuilt for a replaced account's new link.
    property bool restacking: false
    // How many passing notices about ended transfers were shown (tests count them).
    property int transferNotices: 0

    // The fixture pages of the message actions: each opens a chat (`App` does that) and, for
    // `setupPages`, one of its popups or composer modes once the rows are there.
    readonly property list<string> actionPages: ["quotes", "menu", "react", "reactions", "edit", "reply", "delete", "timer", "placeholders", "readmore", "calls", "album", "albumviewer", "revisions", "timercustom", "clearchat", "mediamenu", "deleteattachment"]
    readonly property list<string> setupPages: ["menu", "react", "reactions", "edit", "reply", "delete", "timer", "album", "albumviewer", "revisions", "files", "originals", "timercustom", "clearchat", "mediamenu", "deleteattachment", "folder", "folderfiles", "folderzip", "expiry", "transferwait", "transferdownload", "transferread", "transferbanner", "transferfailed", "transferask"]
    // The fixture pages of the link page in a state of a history transfer, with that state.
    readonly property list<string> transferLinkPages: ["transferwait", "transferdownload", "transferread"]
    readonly property list<string> transferPages: ["transferwait", "transferdownload", "transferread", "transferbanner", "transferfailed", "transferask"]
    property bool pageReady: false
    // The page `transferask` asked its question (once).
    property bool transferAsked: false
    // The fixture pages of the history transfer: whether what the page shows is there.
    function transferPageReady(): bool {
        switch (App.startPage) {
        case "transferwait":
            return App.linkTransfer.phase === "waiting";
        case "transferdownload":
            return App.linkTransfer.phase === "downloading";
        case "transferread":
            return App.linkTransfer.phase === "reading";
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

    // --screenshot=<png>: let data arrive (all of the history for `history`; downloads, the
    // own picture's send and the viewer for the media pages), layout and animations settle,
    // grab the whole window, quit.
    readonly property bool mediaShot: ["media", "viewer", "compose", "files", "originals", "folder", "folderfiles", "folderzip", "album", "albumviewer", "mediamenu", "deleteattachment", "quotes", "menu", "react", "edit", "reply", "delete"].indexOf(App.startPage) >= 0
    Timer {
        running: App.screenshot !== "" && root.started && !(App.startPage === "history" && (App.timeline.hasOlder || App.timeline.loading)) && (App.startPage !== "viewer" || root.viewerShown) && (root.setupPages.indexOf(App.startPage) < 0 || root.pageReady)
        interval: root.mediaShot ? 2500 : 1200
        onTriggered: Qt.exit(App.saveScreenshot(root, App.screenshot) ? 0 : 1)
    }
}
