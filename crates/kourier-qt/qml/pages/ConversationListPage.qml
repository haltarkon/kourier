pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The conversation list: search, the account banner (offline, logged out, a challenge, a wrong
// clock), a notice the chat's composer cannot show, and the chats.
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
    // The account banner has a button the keyboard must reach: the search field then lets
    // Tab take its natural way.
    readonly property bool bannerShown: banner.visible
    // The keys that put the keyboard in the search field, as the platform writes them (for
    // the tooltip).
    property string searchKeys: ""

    signal openConversation(real id)
    // The quick switch chose a chat: it opens and the composer takes the keyboard.
    signal switchedByKeyboard(real id)

    // The keyboard goes to the search field, with what is typed there selected.
    function focusSearch(): void {
        search.forceActiveFocus();
        search.selectAll();
    }

    // The menu of the chat `row` shows, at (`x`, `y`) of the row; with a negative `x` (the
    // keyboard's Menu key), under the row's leading part, so the row stays in view. A chat
    // the menu has nothing to offer for gets no menu (an empty popup is no answer).
    function openMenu(row: ConversationDelegate, x: real, y: real): void {
        if (row === null || !row.hasUnread) {
            return;
        }
        rowMenu.row = row;
        rowMenu.conversationId = row.conversationId;
        // A point of the list, which stays while the list rebuilds rows under the menu.
        const at = row.mapToItem(list, x >= 0 ? x : Kirigami.Units.gridUnit * 2, x >= 0 ? y : row.height);
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

    title: App.currentTitle
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

            Kirigami.SearchField {
                id: search

                placeholderText: L10n.i18nc("@info:placeholder", "Search")
                // Find (like Ctrl+K) is the window's key, so that it also works in a narrow
                // window where the chat covers the list; the field's own Find key would
                // make the two ambiguous, and neither would act.
                focusSequences: []
                Accessible.name: L10n.i18nc("@info accessible", "Search conversations")
                QQC2.ToolTip.visible: search.hovered && search.text === ""
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                QQC2.ToolTip.text: page.searchKeys !== "" ? L10n.i18nc("@info:tooltip, %1 a keyboard shortcut", "Search conversations (%1)", page.searchKeys) : L10n.i18nc("@info:tooltip", "Search conversations")
                onTextChanged: App.setSearch(text)
                // The quick switch: type a name, Enter opens the first chat that matches, and
                // the keyboard goes on to the message.
                onAccepted: {
                    const id = App.conversationAt(0);
                    if (id !== 0) {
                        search.text = "";
                        page.switchedByKeyboard(id);
                    }
                }
                Layout.fillWidth: true
            }
            AccountBanner {
                id: banner
                Layout.fillWidth: true
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
        Keys.onPressed: event => {
            if (event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
                list.currentMenu();
                event.accepted = true;
            }
        }
        spacing: 0
        onHeightChanged: page.showRowsSoon()
        // Rows a little beyond the view are kept ready for scrolling, not the whole list.
        cacheBuffer: Kirigami.Units.gridUnit * 20
        topMargin: Kirigami.Units.smallSpacing
        bottomMargin: Kirigami.Units.smallSpacing

        delegate: ConversationDelegate {
            id: row

            width: ListView.view.width
            highlighted: App.conversations.currentId === conversationId
            onClicked: page.openConversation(conversationId)
            onMenuRequested: (x, y) => page.openMenu(row, x, y)
            Component.onCompleted: page.delegatesCreated += 1
        }

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 4
            visible: page.rowsShown && list.count === 0
            icon.name: "dialog-messages"
            text: L10n.i18nc("@info:placeholder", "No conversations")
            explanation: L10n.i18nc("@info:placeholder", "Chats from your phone appear here once they sync.")
        }
    }

    // The menu of a chat of the list. It is made with the page: a menu made in the moment it
    // opens can stay as small as its padding. (After the list: a scroll view takes its first
    // child as the view to scroll.) It holds what the chat offers; a chat that offers
    // nothing gets no menu (`openMenu`).
    QQC2.Menu {
        id: rowMenu

        // The row the menu was opened for (the list drops rows that scroll away: null then)
        // and its chat.
        property ConversationDelegate row: null
        property real conversationId: 0

        // In the window's scene (also on Wayland, and in headless screenshots).
        popupType: QQC2.Popup.Item

        QQC2.MenuItem {
            id: markReadEntry

            // Only while the chat has unread messages or was marked unread on another
            // device; no room taken otherwise.
            visible: rowMenu.row !== null && rowMenu.row.hasUnread
            height: visible ? implicitHeight : 0
            icon.name: "mail-mark-read"
            text: L10n.i18nc("@action:inmenu", "Mark as Read")
            onTriggered: App.markRead(rowMenu.conversationId)
        }
    }
}
