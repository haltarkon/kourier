pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.delegates as Delegates
import app.kourier

// The conversation list: search, the account banner (offline, logged out, a challenge, a wrong
// clock), a notice the chat's composer cannot show, and the chats. The last row of the list is
// the way into the archive ("Archived Chats (N)", only when there are archived chats); the
// archive view is this same page, titled "Archived Chats", with a Back arrow before its title,
// one line that says what the archive is, and no search field (Ctrl+K goes back to the chats).
Kirigami.ScrollablePage {
    id: page

    // The core's notices (a refused action, a relink that could not start, a verification the
    // network did not accept) show in the composer of the open chat. While that composer is
    // not there to show them (no chat is open, the chat cannot be written to, or the list
    // covers it in a narrow window), they show here, so no failure goes unsaid.
    property bool showsNotice: App.timeline.conversationId === 0 || App.timeline.cannotSend !== ""
    // The list has its rows: it is in the page stack and has a size. Until then it has no
    // model. A list view without a height cannot know which rows are in view and makes a
    // delegate for every row it is given (all chats, each with its avatar, only to destroy
    // all but a screenful a moment later): at the start that cost more than everything else
    // together (§1.3). Set once: the list keeps its rows, and its place, when the chat covers
    // it in a narrow window.
    property bool rowsShown: false
    // How many row delegates the list has made so far (the start is measured by it: a list
    // that makes one for every chat is doing the work of all of them for a screenful).
    property int delegatesCreated: 0

    // The stops of the keyboard in the window's tab order (`MainWindow` links them).
    readonly property alias searchField: search
    readonly property alias conversations: list
    // The menu of a chat and its entry (tests read them).
    readonly property alias rowMenu: rowMenu
    readonly property alias markReadEntry: markReadEntry
    readonly property alias markUnreadEntry: markUnreadEntry
    readonly property alias pinEntry: pinEntry
    readonly property alias muteSub: muteSub
    readonly property alias muteFlat: muteFlat
    readonly property alias archiveMenuEntry: archiveMenuEntry
    readonly property alias mutedLine: mutedLine
    readonly property alias whyLine: whyLine
    // The account banner has a button the keyboard must reach: the search field then lets
    // Tab take its natural way.
    readonly property bool bannerShown: banner.visible
    // The keys that put the keyboard in the search field, as the platform writes them (for
    // the tooltip).
    property string searchKeys: ""
    // The way into the archive shows as the list's last row: there are archived chats, the
    // archive is not shown already and nothing is searched (a search looks in both). Not shown
    // means no footer at all, so the list keeps no empty band at its end.
    readonly property bool entryShown: !App.archiveShown && App.archivedCount > 0 && search.text.trim() === ""
    // The Back arrow of the archive view's title (null while that title is not shown).
    property Item archiveBack: null
    // Kirigami's own title delegate, taken at completion (see `titleDelegate` below).
    property Component kirigamiTitle: null
    // The list's last row, the way into the archive (null while it is not shown; tests read it).
    property Item archiveEntry: null

    // What the menu told a screen reader when it opened with nothing to choose, and how (tests
    // read both; "" and -1 while it did not).
    property string menuAnnounced: ""
    property int menuAnnouncedPoliteness: -1

    signal openConversation(real id)
    // The menu or the keys of the window ask: Archive, Unarchive, Mute "Until…" for chat `conv`.
    // The window does the work (the Undo notice and the dialog are its own: `MainWindow`).
    signal archiveAsked(real conv)
    signal unarchiveAsked(real conv)
    // `back`: the item that had the keyboard (the list, from the keyboard's menu), or null.
    signal untilAsked(real conv, var back)
    // The quick switch chose a chat: it opens and the composer takes the keyboard.
    signal switchedByKeyboard(real id)
    // The banner's button (link again, solve the challenge, restart): the window decides
    // whether a link in progress must be asked about first.
    signal bannerActivated

    // The keyboard goes to the search field, with what is typed there selected. The archive
    // view has no search field: the chats come back first (Ctrl+K in the archive searches
    // the chats).
    function focusSearch(): void {
        if (App.archiveShown) {
            App.showArchive(false);
        }
        search.forceActiveFocus();
        search.selectAll();
    }

    // The archive view opens (its chats, no search) and the keyboard stays in the list, with no
    // row current or focused until it moves (the focus frame would sit on a row nobody chose,
    // and Space would open it). The current row goes BEFORE the model changes: the view makes
    // row 0 current when its rows are replaced and gives that row the scope's focus, which a
    // later `currentIndex = -1` does not take away again.
    function openArchive(): void {
        list.currentIndex = -1;
        App.showArchive(true);
        list.forceActiveFocus();
    }

    // The chats again; the keyboard stays in the list, no row current (as above).
    function closeArchive(): void {
        list.currentIndex = -1;
        App.showArchive(false);
        list.forceActiveFocus();
    }

    // The menu of the chat `row` shows, at (`x`, `y`) of the row; with a negative `x` (the
    // keyboard's Menu key), under the row's leading part, so the row stays in view. What the
    // menu holds is read from the core's offer for the chat NOW (`App.chatOffer` has no change
    // signal). Every chat opens a menu (the Copy entries are always there); the A10d entries and
    // Mark as Read come and go above them.
    function openMenu(row: ConversationDelegate, x: real, y: real): void {
        if (row === null) {
            return;
        }
        const offer = App.chatOffer(row.conversationId);
        rowMenu.offer = offer;
        rowMenu.row = row;
        rowMenu.conversationId = row.conversationId;
        // Whatever line the menu shows (the sync switch's reason, or the pin limit's) a screen
        // reader hears too.
        if (rowMenu.line !== "") {
            page.menuAnnounced = rowMenu.line;
            page.menuAnnouncedPoliteness = Accessible.Polite;
            page.Accessible.announce(rowMenu.line, Accessible.Polite);
        }
        // A point of the list, which stays while the list rebuilds rows under the menu.
        const at = row.mapToItem(list, x >= 0 ? x : Kirigami.Units.gridUnit * 2, x >= 0 ? y : row.height);
        // The keyboard comes back to the list when the menu closes if it was there when it opened:
        // with more entries (and a sub-menu) the focus fell to the timeline instead.
        rowMenu.returnFocus = list.activeFocus;
        rowMenu.popup(list, at.x, at.y);
    }

    function showRows(): void {
        if (!page.rowsShown && page.visible && list.height > 0) {
            page.rowsShown = true;
        }
    }

    // (Once the layout pass that gave the list its size is over: rows that arrive inside it
    // change the page's own size while it is being worked out.)
    function showRowsSoon(): void {
        if (!page.rowsShown) {
            Qt.callLater(page.showRows);
        }
    }

    onVisibleChanged: page.showRowsSoon()
    // The title: `chatsHeading` for the chats, `archiveTitle` (a Back arrow before it) for the
    // archive view, but Kirigami's OWN delegate while the page has no page row (its toolbar
    // style is None). Kirigami's page keeps its toolbar loaded whenever the delegate is not its
    // own; when the list leaves the row (`pageStack.clear()`, the last account gone) the row is
    // set to null first, and a ToolBarPageHeader left without its `pageRow` prints "Unable to
    // assign [undefined] to bool" (its line 61, `visible: root.pageRow?.separatorVisible && …`;
    // `link_actions_ui` case `drawer`, UI-2). With its own delegate the toolbar unloads with the
    // style, as on every Kirigami page.
    Component.onCompleted: {
        page.kirigamiTitle = page.titleDelegate;
        page.titleDelegate = Qt.binding(() => page.globalToolBarStyle === Kirigami.ApplicationHeaderStyle.None ? page.kirigamiTitle : (App.archiveShown ? archiveTitle : chatsHeading));
    }

    title: App.currentTitle
    // Another choice of accounts is another list: it starts at its top. (Rows that join above
    // the ones already shown would otherwise push the view far down. Later: the rows of the new
    // choice arrive right after these properties.)
    function listToTop(): void {
        list.positionViewAtBeginning();
    }
    Connections {
        target: App
        function onCurrentAccountChanged(): void {
            Qt.callLater(page.listToTop);
        }
        function onShownCountChanged(): void {
            Qt.callLater(page.listToTop);
        }
        function onAllShownChanged(): void {
            Qt.callLater(page.listToTop);
        }
        function onArchiveShownChanged(): void {
            if (App.archiveShown) {
                search.text = "";
            }
        }
    }
    leftPadding: 0
    rightPadding: 0
    topPadding: 0
    bottomPadding: 0

    Kirigami.ColumnView.minimumWidth: Kirigami.Units.gridUnit * 17
    Kirigami.ColumnView.maximumWidth: Kirigami.Units.gridUnit * 24
    Kirigami.ColumnView.preferredWidth: Kirigami.Units.gridUnit * 21

    header: QQC2.Control {
        topPadding: Kirigami.Units.smallSpacing
        bottomPadding: Kirigami.Units.smallSpacing
        leftPadding: Kirigami.Units.largeSpacing
        rightPadding: Kirigami.Units.largeSpacing
        contentItem: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing

            // What the archive is (the view has no search field).
            QQC2.Label {
                id: archiveHelp

                objectName: "archive-help"
                // (Only over chats: an empty archive says "No archived chats" and no more.)
                visible: App.archiveShown && list.count > 0
                text: L10n.i18nc("@info", "These chats are archived. They come back to your chats when a new message arrives.")
                wrapMode: Text.WordWrap
                textFormat: Text.PlainText
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                Layout.fillWidth: true
                Layout.margins: Kirigami.Units.smallSpacing
            }

            Kirigami.SearchField {
                id: search

                visible: !App.archiveShown
                placeholderText: L10n.i18nc("@info:placeholder", "Search")
                // Find (like Ctrl+K) is the window's key, so that it also works in a narrow
                // window where the chat covers the list; the field's own Find key would
                // make the two ambiguous, and neither would act.
                focusSequences: []
                // Only Enter accepts: Kirigami's field accepts by itself a moment after every
                // change by default, and accepting is the quick switch below, so the first
                // letter typed opened a chat and took the keyboard away (his report,
                // 2026-10-16). The list filters on `onTextChanged`.
                autoAccept: false
                Accessible.name: L10n.i18nc("@info accessible", "Search conversations")
                QQC2.ToolTip.visible: search.hovered && search.text === ""
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                QQC2.ToolTip.text: page.searchKeys !== "" ? L10n.i18nc("@info:tooltip, %1 a keyboard shortcut", "Search conversations (%1)", page.searchKeys) : L10n.i18nc("@info:tooltip", "Search conversations")
                onTextChanged: App.setSearch(text)
                // The quick switch: type a name, Enter opens the first chat that matches, and
                // the keyboard goes on to the message. The window empties the field once the
                // chat opens (`MainWindow.switchTo`): kept when the switch is not made (it
                // asks "Stop Linking?" first and the answer is "Continue Linking").
                onAccepted: {
                    const id = App.conversationAt(0);
                    if (id !== 0) {
                        page.switchedByKeyboard(id);
                    }
                }
                Layout.fillWidth: true
            }
            AccountBanner {
                id: banner
                Layout.fillWidth: true
                onActivated: page.bannerActivated()
            }
            Kirigami.InlineMessage {
                visible: page.showsNotice && App.composer.notice !== ""
                type: Kirigami.MessageType.Information
                text: Style.plain(App.composer.notice)
                Layout.fillWidth: true
                actions: [
                    Kirigami.Action {
                        icon.name: "dialog-close"
                        text: L10n.i18nc("@action:button", "Dismiss")
                        displayHint: Kirigami.DisplayHint.IconOnly
                        onTriggered: App.composer.clearNotice()
                    }
                ]
            }
        }
    }

    ListView {
        id: list

        // The list is one tab stop (not a stop per chat): Up, Down, Page Up, Page Down, Home
        // and End move the current row, Enter or Space opens it. No row is current until
        // the keyboard moves into the list.
        function openCurrent(): void {
            // Only the list's own keyboard: the Return that the search field passes on after
            // the quick switch opened its chat is not the list's, and must not open the
            // current row as well.
            if (!list.activeFocus) {
                return;
            }
            const row = list.currentItem as ConversationDelegate;
            if (row !== null) {
                page.openConversation(row.conversationId);
            }
        }
        // The menu of the current row: the Menu key and Shift+F10.
        function currentMenu(): void {
            page.openMenu(list.currentItem as ConversationDelegate, -1, -1);
        }

        model: page.rowsShown ? App.conversations : null
        currentIndex: -1
        activeFocusOnTab: true
        keyNavigationEnabled: true
        highlightMoveDuration: 0
        Accessible.role: Accessible.List
        Accessible.name: L10n.i18nc("@info accessible", "Conversations")
        Keys.onReturnPressed: list.openCurrent()
        Keys.onEnterPressed: list.openCurrent()
        Keys.onSpacePressed: list.openCurrent()
        Keys.onMenuPressed: list.currentMenu()
        // Esc and Alt+Left leave the archive view while the list has the keyboard.
        Keys.onEscapePressed: event => {
            event.accepted = App.archiveShown;
            if (App.archiveShown) {
                page.closeArchive();
            }
        }
        // (Alt+Left is the page stack's Back shortcut, which would take it first: the list, in
        // the archive view, keeps it.)
        Keys.onShortcutOverride: event => {
            if (App.archiveShown && event.key === Qt.Key_Left && (event.modifiers & Qt.AltModifier)) {
                event.accepted = true;
            }
        }
        Keys.onPressed: event => {
            if (event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
                list.currentMenu();
                event.accepted = true;
            } else if (event.key === Qt.Key_Left && (event.modifiers & Qt.AltModifier) && App.archiveShown) {
                page.closeArchive();
                event.accepted = true;
            }
        }
        spacing: 0
        onHeightChanged: page.showRowsSoon()
        // Rows a little beyond the view are kept ready for scrolling, not the whole list.
        cacheBuffer: Kirigami.Units.gridUnit * 20
        topMargin: Kirigami.Units.smallSpacing
        bottomMargin: Kirigami.Units.smallSpacing

        // The way into the archive: the last row, its own stop of the keyboard (below).
        footer: page.entryShown ? archiveEntryComponent : null

        delegate: ConversationDelegate {
            id: row

            width: ListView.view.width
            highlighted: App.conversations.currentId === conversationId
            onClicked: page.openConversation(conversationId)
            onMenuRequested: (x, y) => page.openMenu(row, x, y)
            Component.onCompleted: page.delegatesCreated += 1
        }

        // An empty list says why while a new link still brings its chats (link feedback L3:
        // the list opens as soon as the account is linked): the history on its way, its
        // import, or the contacts and groups still coming (the banner counts them).
        Kirigami.PlaceholderMessage {
            id: emptyList

            // A SHOWN account's transfer only: a hidden account's banner may show, but its history
            // does not come into this list (M2).
            readonly property string phase: App.bannerShownTransfer.phase ?? ""
            // What the user searches for (the filter ignores the space around it): a search
            // that matches nothing says so, whatever else goes on with the account.
            readonly property string query: search.text.trim()

            objectName: "empty-list"
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 2
            // Not while the list's only row is the way into the archive: every chat is archived,
            // which is no reason to say there is no conversation.
            visible: page.rowsShown && list.count === 0 && !page.entryShown
            icon.name: App.archiveShown ? "archive-insert" : "dialog-messages"
            text: {
                if (App.archiveShown) {
                    return L10n.i18nc("@info:placeholder", "No archived chats");
                }
                if (emptyList.query !== "") {
                    return L10n.i18nc("@info:placeholder, %1 what was searched for", "No chats match “%1”", Style.plain(emptyList.query));
                }
                switch (emptyList.phase) {
                case "waiting":
                case "downloading":
                    return L10n.i18nc("@info:placeholder", "Your history is on its way");
                case "reading":
                    return L10n.i18nc("@info:placeholder", "Importing your message history");
                case "updating":
                    return L10n.i18nc("@info:placeholder", "Getting your chats");
                case "history":
                    return L10n.i18nc("@info:placeholder", "Getting your message history");
                default:
                    return L10n.i18nc("@info:placeholder", "No conversations");
                }
            }
            explanation: {
                if (App.archiveShown || emptyList.query !== "") {
                    return "";
                }
                switch (emptyList.phase) {
                case "waiting":
                case "downloading":
                    return L10n.i18nc("@info:placeholder", "Chats appear here when your phone sends them.");
                case "reading":
                    return L10n.i18nc("@info:placeholder", "Chats appear here as they are imported.");
                case "updating":
                    return L10n.i18nc("@info:placeholder", "Chats appear here as contacts and groups arrive.");
                case "history":
                    return L10n.i18nc("@info:placeholder", "Chats appear here as your phone sends them.");
                default:
                    return L10n.i18nc("@info:placeholder", "Chats from your phone appear here once they sync.");
                }
            }
        }
    }

    // The footer of the list.
    Component {
        id: archiveEntryComponent

        Delegates.RoundedItemDelegate {
            id: entry

            width: list.width
            focusPolicy: Qt.StrongFocus
            Accessible.role: Accessible.Button
            Accessible.name: L10n.i18ncp("@info accessible, %1 how many", "Archived Chats, %1 chat", "Archived Chats, %1 chats", App.archivedCount)
            Accessible.onPressAction: entry.clicked()
            onClicked: page.openArchive()
            // Shift+Tab goes back to the list: the entry sits inside the list, which is a focus
            // scope, so it lets go of the keyboard (the scope then has it) rather than
            // handing it to its own scope.
            Keys.onBacktabPressed: event => {
                entry.focus = false;
                list.forceActiveFocus(Qt.BacktabFocusReason);
                event.accepted = true;
            }
            // (Enter opens it as Space does; the delegate takes Space itself.)
            Keys.onReturnPressed: event => {
                entry.clicked();
                event.accepted = true;
            }
            Keys.onEnterPressed: event => {
                entry.clicked();
                event.accepted = true;
            }
            Component.onCompleted: page.archiveEntry = entry
            Component.onDestruction: {
                if (page.archiveEntry === entry) {
                    page.archiveEntry = null;
                }
            }

            contentItem: RowLayout {
                spacing: Kirigami.Units.largeSpacing

                Kirigami.Icon {
                    source: "archive-insert"
                    implicitWidth: Kirigami.Units.iconSizes.smallMedium
                    implicitHeight: Kirigami.Units.iconSizes.smallMedium
                    Layout.alignment: Qt.AlignVCenter
                }
                QQC2.Label {
                    text: L10n.i18nc("@action:button, %1 how many archived chats there are", "Archived Chats (%1)", App.archivedCount)
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                    Layout.alignment: Qt.AlignVCenter
                }
            }
        }
    }

    // The narrow window's "Dot with count" (UI-1): the unread messages of hidden accounts on the
    // CLOSED menu button. That button is Kirigami's private `HandleButton` of the toolbar, right
    // before the page's title delegate (the drawer's own handle is invisible while the toolbar
    // holds its place); the pill sits over its top end corner (anchored to the title's start, so
    // it mirrors in RTL), only while the drawer is modal and closed, and is made on first need.
    // One for both titles, the chats' and the archive's (NEW-3): a holder of no size inside the
    // title, the pill a child of that title.
    component MenuButtonPill: Item {
        id: pillHost

        // The title item the pill stands before (its start edge, its vertical centre).
        required property Item title
        readonly property Kirigami.GlobalDrawer drawer: ((QQC2.ApplicationWindow.window as Kirigami.ApplicationWindow)?.globalDrawer as Kirigami.GlobalDrawer) ?? null
        readonly property bool handleShown: pillHost.drawer !== null && pillHost.drawer.modal && pillHost.drawer.handleVisible && pillHost.drawer.enabled
        readonly property bool wanted: pillHost.drawer !== null && pillHost.drawer.modal && App.hiddenUnread > 0
        // The pill (null until first needed; tests read it through the title).
        property Item pill: null
        onWantedChanged: pillHost.make()
        Component.onCompleted: pillHost.make()
        function make(): void {
            if (pillHost.wanted && pillHost.pill === null) {
                pillHost.pill = handlePill.createObject(pillHost.title) as Item;
            }
        }
        Component {
            id: handlePill

            UnreadPill {
                objectName: "menu-button-pill"
                compact: true
                count: App.hiddenUnread
                visible: pillHost.wanted && pillHost.handleShown && !pillHost.drawer.drawerOpen
                anchors.right: pillHost.title.left
                // (Like the rail's pill: over the corner, standing out by a few pixels.)
                anchors.bottom: pillHost.title.verticalCenter
                anchors.bottomMargin: -Kirigami.Units.smallSpacing / 2
                z: 1
                // Orca: Kirigami's menu button has no accessible name of its own.
                Accessible.role: Accessible.StaticText
                Accessible.name: pillHost.drawer?.handleClosedToolTip ?? ""
            }
        }
    }

    // The chats' title: Kirigami's default (a heading that shrinks and elides), but with several
    // accounts shown it names them while the names fit ("Andrii · Work") and says how many
    // otherwise ("3 accounts", `App.currentTitleShort`, M7). `title` stays the full text for the
    // window and the accessible name. It carries the menu button's pill (`MenuButtonPill`).
    Component {
        id: chatsHeading

        Item {
            id: chatsTitleItem
            objectName: "chatsTitle"
            // The text shown (tests read it).
            readonly property string shownText: chatsHeadingText.text
            // The width the full title needs (tests read it).
            readonly property real fullWidth: chatsFull.advanceWidth + chatsTitleItem.lead
            // The menu button's pill (null until first needed; tests read it).
            readonly property Item pill: chatsPill.pill

            MenuButtonPill {
                id: chatsPill
                title: chatsTitleItem
            }

            // Kirigami gives ITS default title the toolbar's title padding when no menu button
            // stands before it (`ToolBarPageHeader` `titleLoader.Layout.leftMargin`, only for a
            // `DefaultPageTitleDelegate`): the same room here, so the wide title stays where it was.
            readonly property real lead: chatsTitleItem.handleShown ? 0 : Math.max(0, ((QQC2.ApplicationWindow.window as Kirigami.ApplicationWindow)?.pageStack.globalToolBar.titleLeftPadding ?? Kirigami.Units.gridUnit) - Kirigami.Units.mediumSpacing)
            readonly property bool handleShown: chatsPill.handleShown

            Layout.fillWidth: true
            Layout.minimumWidth: 0
            Layout.maximumWidth: implicitWidth
            implicitWidth: Math.ceil(chatsFull.advanceWidth + chatsTitleItem.lead)
            implicitHeight: Math.ceil(chatsHeadingText.implicitHeight)

            TextMetrics {
                id: chatsFull
                font: chatsHeadingText.font
                text: page.title
            }
            Kirigami.Heading {
                id: chatsHeadingText
                objectName: "chatsHeadingText"
                anchors.fill: parent
                anchors.leftMargin: chatsTitleItem.lead
                maximumLineCount: 1
                elide: Text.ElideRight
                textFormat: Text.PlainText
                text: App.currentTitleShort !== "" && chatsTitleItem.width - chatsTitleItem.lead < chatsFull.advanceWidth ? App.currentTitleShort : page.title
            }
        }
    }

    // The title of the archive view: a Back arrow and the heading, and the menu button's pill
    // as on the chats' title (NEW-3: hidden accounts' unread messages stay counted here too).
    Component {
        id: archiveTitle

        // (An item around the row: the pill is a child of the title, outside the row's layout.)
        Item {
            id: archiveTitleItem
            objectName: "archiveTitle"
            Layout.fillWidth: true
            implicitWidth: archiveRow.implicitWidth
            implicitHeight: archiveRow.implicitHeight
            // The menu button's pill (null until first needed; tests read it).
            readonly property Item pill: archivePill.pill

            MenuButtonPill {
                id: archivePill
                title: archiveTitleItem
            }

            RowLayout {
                id: archiveRow
                anchors.fill: parent
                spacing: Kirigami.Units.smallSpacing

                QQC2.ToolButton {
                    id: back

                    icon.name: LayoutMirroring.enabled ? "go-next" : "go-previous"
                    display: QQC2.AbstractButton.IconOnly
                    Accessible.name: L10n.i18nc("@info accessible", "Back to Chats")
                    QQC2.ToolTip.visible: back.hovered
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Back to Chats")
                    onClicked: page.closeArchive()
                    Keys.onEscapePressed: event => {
                        page.closeArchive();
                        event.accepted = true;
                    }
                    Keys.onShortcutOverride: event => {
                        if (event.key === Qt.Key_Left && (event.modifiers & Qt.AltModifier)) {
                            event.accepted = true;
                        }
                    }
                    Keys.onPressed: event => {
                        if (event.key === Qt.Key_Left && (event.modifiers & Qt.AltModifier)) {
                            page.closeArchive();
                            event.accepted = true;
                        }
                    }
                    Component.onCompleted: page.archiveBack = back
                    Component.onDestruction: {
                        if (page.archiveBack === back) {
                            page.archiveBack = null;
                        }
                    }
                }
                Kirigami.Heading {
                    text: page.title
                    maximumLineCount: 1
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                }
            }
        }
    }

    // The menu of a chat of the list. It is made with the page: a menu made in the moment it
    // opens can stay as small as its padding. (After the list: a scroll view takes its first
    // child as the view to scroll.) It holds what the chat offers (the core's offer for it, read
    // when the menu opens) and the copy actions every chat has. Entries the chat does not offer
    // are hidden (no room taken, no key reaches them); entries the sync switch greys say why
    // (`Accessible.description`) and the line under them says it in words.
    QQC2.Menu {
        id: rowMenu

        // The row the menu was opened for (the list drops rows that scroll away: null then)
        // and its chat.
        property ConversationDelegate row: null
        property real conversationId: 0
        // `App.chatOffer` of that chat, when the menu opened.
        property var offer: ({})
        // The line under the entries: why they are grey, or why Pin is.
        readonly property string why: rowMenu.offer.why ? rowMenu.offer.why : ""
        readonly property string pinWhy: rowMenu.offer.pinWhy ? rowMenu.offer.pinWhy : ""
        readonly property string line: rowMenu.why !== "" ? rowMenu.why : (rowMenu.offer.pinAtLimit === true && rowMenu.offer.pinned !== true ? rowMenu.pinWhy : "")
        // Whether the list had the keyboard when the menu opened (`openMenu`).
        property bool returnFocus: false
        // The sub-menu's own entry of this menu, once the menu has made it (H1: found, never
        // inserted or removed: `removeMenu` would delete the sub-menu).
        property QQC2.MenuItem muteItem: null

        // In the window's scene (also on Wayland, and in headless screenshots).
        popupType: QQC2.Popup.Item

        onClosed: {
            if (rowMenu.returnFocus && !muteSub.visible) {
                rowMenu.returnFocus = false;
                list.forceActiveFocus();
            }
        }

        Component.onCompleted: {
            for (let i = 0; i < rowMenu.count; ++i) {
                const item = rowMenu.itemAt(i) as QQC2.MenuItem;
                if (item !== null && item.subMenu === muteSub) {
                    rowMenu.muteItem = item;
                    break;
                }
            }
            if (rowMenu.muteItem === null) {
                return;
            }
            rowMenu.muteItem.visible = Qt.binding(() => rowMenu.offer.on === true && rowMenu.offer.muteOffered === true && rowMenu.offer.muted !== true);
            rowMenu.muteItem.enabled = Qt.binding(() => rowMenu.muteItem.visible);
            rowMenu.muteItem.height = Qt.binding(() => rowMenu.muteItem.visible ? rowMenu.muteItem.implicitHeight : 0);
            rowMenu.muteItem.icon.name = "player-volume-muted";
        }

        QQC2.MenuItem {
            id: markReadEntry

            // Only while the chat has unread messages or was marked unread on another
            // device; no room taken otherwise.
            visible: rowMenu.offer.hasUnread === true || (rowMenu.row !== null && rowMenu.row.hasUnread)
            // Disabled while hidden: the keyboard would reach an invisible item otherwise.
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "mail-mark-read"
            text: L10n.i18nc("@action:inmenu", "Mark as Read")
            onTriggered: App.markRead(rowMenu.conversationId)
        }

        QQC2.MenuItem {
            id: markUnreadEntry

            visible: rowMenu.offer.shown === true && !markReadEntry.visible
            enabled: visible && rowMenu.offer.on === true
            height: visible ? implicitHeight : 0
            icon.name: "mail-mark-unread"
            text: L10n.i18nc("@action:inmenu", "Mark as Unread")
            Accessible.description: markUnreadEntry.enabled ? "" : rowMenu.why
            onTriggered: App.setMarkedUnread(rowMenu.conversationId, true)
        }

        QQC2.MenuItem {
            id: pinEntry

            readonly property bool pinned: rowMenu.offer.pinned === true
            visible: rowMenu.offer.shown === true
            enabled: visible && rowMenu.offer.on === true && (pinEntry.pinned || rowMenu.offer.pinAtLimit !== true)
            height: visible ? implicitHeight : 0
            icon.name: pinEntry.pinned ? "window-unpin" : "pin"
            text: pinEntry.pinned ? L10n.i18nc("@action:inmenu", "Unpin") : L10n.i18nc("@action:inmenu", "Pin")
            Accessible.description: pinEntry.enabled ? "" : (rowMenu.offer.on === true ? rowMenu.pinWhy : rowMenu.why)
            onTriggered: App.setPinned(rowMenu.conversationId, !pinEntry.pinned)
        }

        // Mute with its times (Signal Desktop's order). A sub-menu declared here, made with the
        // page; `muteItem` above is its entry in this menu.
        QQC2.Menu {
            id: muteSub

            title: L10n.i18nc("@title:menu", "Mute")
            popupType: QQC2.Popup.Item

            QQC2.MenuItem {
                text: L10n.i18nc("@action:inmenu", "1 Hour")
                onTriggered: App.muteFor(rowMenu.conversationId, "hour")
            }
            QQC2.MenuItem {
                text: L10n.i18nc("@action:inmenu", "8 Hours")
                onTriggered: App.muteFor(rowMenu.conversationId, "eight")
            }
            QQC2.MenuItem {
                text: L10n.i18nc("@action:inmenu", "1 Day")
                onTriggered: App.muteFor(rowMenu.conversationId, "day")
            }
            QQC2.MenuItem {
                text: L10n.i18nc("@action:inmenu", "1 Week")
                onTriggered: App.muteFor(rowMenu.conversationId, "week")
            }
            QQC2.MenuItem {
                text: L10n.i18nc("@action:inmenu", "Until…")
                onTriggered: {
                    // The dialog is modal and takes the keyboard when it has opened: the menu
                    // must not hand it to the list meanwhile (M5); the dialog gives it back.
                    const back = rowMenu.returnFocus ? list : null;
                    rowMenu.returnFocus = false;
                    page.untilAsked(rowMenu.conversationId, back);
                }
            }
            QQC2.MenuItem {
                text: L10n.i18nc("@action:inmenu", "Always")
                onTriggered: App.muteFor(rowMenu.conversationId, "always")
            }
        }

        // When a mute ends (his answer to question 1): a quiet line above Unmute.
        Item {
            id: mutedLine

            readonly property bool shown: rowMenu.offer.muted === true && rowMenu.offer.mutedLabel !== undefined && rowMenu.offer.mutedLabel !== ""
            visible: mutedLine.shown
            implicitWidth: Kirigami.Units.gridUnit * 18
            implicitHeight: mutedLine.shown ? mutedLabel.implicitHeight + Kirigami.Units.smallSpacing * 2 : 0
            height: implicitHeight
            Accessible.role: Accessible.StaticText
            Accessible.name: mutedLabel.text

            QQC2.Label {
                id: mutedLabel

                anchors.fill: parent
                anchors.leftMargin: Kirigami.Units.largeSpacing
                anchors.rightMargin: Kirigami.Units.largeSpacing
                verticalAlignment: Text.AlignVCenter
                text: rowMenu.offer.mutedLabel ? rowMenu.offer.mutedLabel : ""
                wrapMode: Text.WordWrap
                textFormat: Text.PlainText
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            }
        }

        // The flat entry beside the sub-menu: "Mute" greyed while the sync is off (a sub-menu's
        // entry would not be reachable to say why), "Unmute" while the chat is muted.
        QQC2.MenuItem {
            id: muteFlat

            readonly property bool muted: rowMenu.offer.muted === true
            visible: rowMenu.offer.shown === true && rowMenu.offer.muteOffered === true && (muteFlat.muted || rowMenu.offer.on !== true)
            enabled: visible && muteFlat.muted && rowMenu.offer.on === true
            height: visible ? implicitHeight : 0
            icon.name: muteFlat.muted ? "player-volume" : "player-volume-muted"
            text: muteFlat.muted ? L10n.i18nc("@action:inmenu", "Unmute") : L10n.i18nc("@action:inmenu", "Mute")
            Accessible.description: muteFlat.enabled ? "" : rowMenu.why
            onTriggered: App.unmute(rowMenu.conversationId)
        }

        QQC2.MenuItem {
            id: archiveMenuEntry

            readonly property bool archived: rowMenu.offer.archived === true
            visible: rowMenu.offer.shown === true
            enabled: visible && rowMenu.offer.on === true
            height: visible ? implicitHeight : 0
            icon.name: archiveMenuEntry.archived ? "archive-remove" : "archive-insert"
            text: archiveMenuEntry.archived ? L10n.i18nc("@action:inmenu", "Unarchive") : L10n.i18nc("@action:inmenu", "Archive")
            Accessible.description: archiveMenuEntry.enabled ? "" : rowMenu.why
            onTriggered: archiveMenuEntry.archived ? page.unarchiveAsked(rowMenu.conversationId) : page.archiveAsked(rowMenu.conversationId)
        }

        // The line: a wrapping label in an item of its own (a menu entry of 105 characters would
        // make the whole menu that wide; this one is as wide as `implicitWidth` says and takes
        // no key).
        Item {
            id: whyLine

            readonly property bool shown: rowMenu.line !== ""
            visible: whyLine.shown
            implicitWidth: Kirigami.Units.gridUnit * 18
            implicitHeight: whyLine.shown ? whyLabel.implicitHeight + Kirigami.Units.smallSpacing * 2 : 0
            height: implicitHeight
            Accessible.role: Accessible.StaticText
            Accessible.name: whyLabel.text

            QQC2.Label {
                id: whyLabel

                anchors.fill: parent
                anchors.leftMargin: Kirigami.Units.largeSpacing
                anchors.rightMargin: Kirigami.Units.largeSpacing
                verticalAlignment: Text.AlignVCenter
                text: rowMenu.line
                wrapMode: Text.WordWrap
                textFormat: Text.PlainText
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            }
        }

        QQC2.MenuSeparator {
            visible: markReadEntry.visible || rowMenu.offer.shown === true
            height: visible ? implicitHeight : 0
        }
        QQC2.MenuItem {
            objectName: "rowCopyName"
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:inmenu", "Copy Name")
            onTriggered: App.copyShown(Style.unmarked(rowMenu.row?.title ?? ""))
        }
        // A group, a channel or a list has no number and no link to a person.
        QQC2.MenuItem {
            objectName: "rowCopyNumber"
            visible: rowMenu.row !== null && !rowMenu.row.isGroup
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:inmenu", "Copy Number or ID")
            onTriggered: App.copyChat("number", rowMenu.conversationId)
        }
        QQC2.MenuItem {
            objectName: "rowCopyLink"
            visible: rowMenu.row !== null && !rowMenu.row.isGroup
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:inmenu", "Copy Link")
            onTriggered: App.copyChat("link", rowMenu.conversationId)
        }
    }
}
