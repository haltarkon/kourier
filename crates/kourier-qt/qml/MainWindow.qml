pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
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
    readonly property bool chatShown: App.timeline.conversationId !== 0 && root.chatPage !== null && root.chatPage.Kirigami.ColumnView.inViewport && !root.chatPage.panelCovers && (root.wideScreen || root.pageStack.currentItem === root.chatPage)
    // What the keys Ctrl+Shift+A and Ctrl+Shift+U need (A10d, H2): the chat is shown AND its page is a page of
    // the stack. A wide window with Settings over the chat keeps the page counted "in the viewport" after it
    // left the stack (`chatShown` stays true; found by `roaming_ui` `keys`), and a key must not archive a
    // chat nobody sees.
    readonly property bool chatKeys: root.chatShown && root.inStack(root.chatPage)
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
        showsNotice: !root.wideScreen || App.timeline.conversationId === 0 || App.timeline.cannotSend !== ""
        searchKeys: root.searchKeys
        onOpenConversation: id => root.pickedChat(id)
        onArchiveAsked: conv => root.archiveChat(conv)
        onUnarchiveAsked: conv => root.unarchiveChat(conv)
        onUntilAsked: (conv, back) => root.askMuteUntil(conv, back)
        onSwitchedByKeyboard: id => root.switchTo(id)
        onBannerActivated: root.bannerPressed()
    }

    property ChatPage chatPage: null
    property WelcomePage welcomePage: null
    property LinkAccountPage linkPage: null
    property SettingsPage settingsPage: null
    // The warning before a new account of an experimental network (made anew each time).
    property RiskNoticePage riskPage: null

    Component {
        id: chatComponent
        ChatPage {
            visible: false
            // A group in common (or "Message") on the details panel opens that chat.
            onConversationRequested: id => root.pickedChat(id)
            onArchiveAsked: conv => root.archiveChat(conv)
            onUnarchiveAsked: conv => root.unarchiveChat(conv)
            onUntilAsked: (conv, back) => root.askMuteUntil(conv, back)
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

    // A chat of the list was picked (a click, Enter on a row): an archived chat that a search
    // found brings the archive view with it, with the chat highlighted in it (Signal
    // Desktop's rule). A notification does not (`openFromActivation`): it opens its chat and
    // leaves the list as it is.
    function pickedChat(id: real): void {
        root.openingChat(id, () => App.revealChat(id));
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
        const chatPages = ["chat", "history", "media", "viewer", "compose", "files", "originals", "folder", "folderfiles", "folderzip", "location", "locationmany", "locationnever"].concat(root.actionPages);
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
        } else if (page === "expiry" || page === "networks") {
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
            // (An archived chat that the search found opens in the archive view. Before the
            // field is emptied: the search is what lists it.)
            App.revealChat(id);
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
    readonly property Item timelineStop: root.wideScreen && root.chatPage !== null && root.chatPage.open ? root.chatPage.timeline.rowList : null
    readonly property Item composerStop: root.timelineStop !== null ? root.chatPage.composer.firstStop : null
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

    // Save As… and Save All… (F3b): the dialog is made on first use, from the file that
    // `App.saveDialogSource()` names (empty: the built-in one), so the dialogs module is not loaded
    // by the start. One attachment asks Rust for the name it is offered under (`App.askSave`), then
    // the dialog starts there; several ask for a folder.
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
            root.savePicked(root.saveDialog.attachments, chosen);
        }
        function onClosed(): void {
            // Only the dialog that closed goes: another Save may have made a new one by the time
            // the call runs.
            const closed = saveDialogLoader.item;
            Qt.callLater(() => {
                if (saveDialogLoader.item === closed) {
                    saveDialogLoader.source = "";
                }
            });
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
        if (standing.reopen && App.timeline.conversationId === 0) {
            root.openingChat(standing.conv, null);
        }
    }

    // The ONE path of the menu's Archive and the key: the notice comes when the row says so.
    function archiveChat(conv: real): void {
        root.archivedOpen = conv === App.timeline.conversationId ? conv : 0;
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
        const conv = App.timeline.conversationId;
        if (conv === 0) {
            return;
        }
        const offer = App.timeline.chatOffer;
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
    readonly property list<string> actionPages: ["quotes", "menu", "react", "reactions", "edit", "reply", "delete", "timer", "placeholders", "readmore", "calls", "unavailable", "album", "albumviewer", "revisions", "timercustom", "clearchat", "mediamenu", "deleteattachment", "wafailed", "uploading", "uploadfile", "person", "group", "chatmedia", "safety", "safetychanged", "share", "timerpending", "save", "muteuntil", "chatmenu", "chatmenuoff", "groupadmin"]
    readonly property list<string> setupPages: ["menu", "react", "reactions", "edit", "reply", "delete", "timer", "album", "albumviewer", "revisions", "files", "originals", "timercustom", "clearchat", "mediamenu", "deleteattachment", "folder", "folderfiles", "folderzip", "expiry", "networks", "waqr", "waphone", "wacode", "thqr", "themoji", "tgqr", "tgcode", "tgpassword", "wahistory", "walost", "transferwait", "transferdownload", "transferread", "transferupdate", "transferbanner", "transferfailed", "transferask", "archive", "person", "group", "chatmedia", "safety", "safetychanged", "share", "timerpending", "save", "location", "locationmany", "locationnever", "accounts", "accountsdrawer", "media", "rowmenu", "rowmenuoff", "rowmenumuted", "mutemenu", "muteuntil", "chatmenu", "chatmenuoff", "groupadmin"]
    // The fixture pages of blended chats: no chat opens, the list stands with its accounts.
    readonly property list<string> accountsPages: ["accounts", "accountsdrawer"]
    // The fixture pages of the chat list's own menus (A10d): no chat opens; `rowMenuPageReady` opens the
    // menu of a fixed row (the first ordinary chat) through the list's own `openMenu`.
    readonly property list<string> rowMenuPages: ["rowmenu", "rowmenuoff", "rowmenumuted", "mutemenu"]
    // How far that page has come (0: nothing done yet) and the chat it stands on.
    property int rowMenuStage: 0
    // The page `chatmenuoff` asked for the switch off (once).
    property bool chatMenuOffAsked: false
    property real rowMenuChat: 0
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
            if (App.startPage === "networks") {
                root.settingsPage.showNetworks();
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
                    done = root.openChatMenu(chatPage);
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
                    done = root.openChatMenu(chatPage);
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
