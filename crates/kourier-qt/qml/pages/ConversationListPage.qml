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
    readonly property bool entryShown: !App.archiveShown && App.archivedCount > 0 && search.text.trim() === "" && page.searchChat === 0
    // The Back arrow of the archive view's title (null while that title is not shown).
    property Item archiveBack: null
    // Kirigami's own title delegate, taken at completion (see `titleDelegate` below).
    property Component kirigamiTitle: null
    // The list's last row, the way into the archive (null while it is not shown; tests read it).
    property Item archiveEntry: null
    // The hits of the message search, under the chats: a header "Messages" and a row per hit,
    // newest first, in the list's footer while a search has any (none without a query, so a
    // picture of the list without one cannot move).
    readonly property bool hitsShown: App.searchResults.active && App.searchResults.count > 0 && !App.archiveShown
    // The hits' rows (null while they are not shown; tests read it).
    property Repeater hitRows: null
    // Search in ONE chat (G1i): the chat the chip names, 0 for none. While it is set the chats
    // part of the list is empty (`list.model` is null: the chats' model, `conversationAt(0)` and
    // the list without a query stay as they are) and only the hits show. It ends when another
    // chat opens, the chat turns protected, the X or Backspace in the empty field takes the chip
    // off (then every chat is searched), or Escape.
    property real searchChat: 0
    readonly property bool chipShown: page.searchChat !== 0
    // What the chip names, kept as it was when the one-chat search began: the chat may close or
    // go while the chip stands (R3 F8, C9).
    property string chipTitle: ""
    property string chipAvatar: ""
    property bool chipSelf: false
    // The chat the chip names left the list (its account went, it was removed): the search goes
    // back to every chat with the same text.
    readonly property bool searchChatGone: page.searchChat !== 0 && App.searchResults.chat === page.searchChat && App.searchResults.chatGone
    // (Later: the chip's own state is part of the binding above.)
    onSearchChatGoneChanged: Qt.callLater(page.dropGoneChip)
    function dropGoneChip(): void {
        if (page.searchChatGone) {
            page.removeChip();
        }
    }
    // What a screen reader was told of the last answer (tests read it), and for which text.
    property string searchAnnounced: ""
    property string announcedText: ""
    // The text (trimmed) the last message search was asked for: "No results" is only for a text
    // the core answered (never for the one typed a moment ago).
    property string askedText: ""
    // RegExp search (R5): the field's `.*` toggle, and `Aa` (Match Case, only with `.*`). They
    // live for the window's life and are not saved. The pattern is checked as you type
    // (`regexError`, the viewmodel's words); a bad one asks nothing.
    property bool regexOn: false
    property bool matchCase: false
    readonly property string syntaxError: page.regexOn ? App.filters.checkPattern(search.text) : ""
    // What shows under the field: the syntax problem, else the core's refusal of a pattern the
    // check could not judge (too large to compile).
    readonly property string regexError: page.syntaxError !== "" ? page.syntaxError : (page.regexOn ? App.searchResults.refusal : "")
    // "Save as Filter" is offered for a pattern that is typed and valid.
    readonly property bool canSaveFilter: page.regexOn && search.text.trim() !== "" && page.regexError === ""
    readonly property alias regexButton: regexButton
    readonly property alias matchCaseButton: matchCaseButton
    readonly property alias saveFilterButton: saveFilterButton
    readonly property alias saveFilterDialog: saveFilterDialog
    // Asks the core for the typed text in the mode the toggles say (a pattern keeps its spaces;
    // words are trimmed); a bad pattern ends the search instead.
    function runSearch(): void {
        const text = search.text.trim();
        if (text === "") {
            return;
        }
        if (page.syntaxError !== "") {
            App.closeSearch();
            return;
        }
        page.askedText = text;
        App.searchMessages(page.regexOn ? search.text : text, page.searchChat);
    }
    // The toggles changed: the core asks in the new mode (a search that runs starts again) and the
    // chat titles follow the pattern.
    function modeChanged(): void {
        App.setSearchMode(page.regexOn, page.matchCase);
        searchDelay.stop();
        page.runSearch();
    }
    // The pattern becomes a filter: the filter dialog opens with it in Message text.
    function saveAsFilter(): void {
        if (!page.canSaveFilter) {
            return;
        }
        saveFilterDialog.openWith({
            "name": App.filters.nameFromPattern(search.text),
            "text": search.text,
            "matchCase": page.matchCase
        });
    }
    // The quiet line while the search index is still being built (G1i, call 3): under the
    // "Messages" header of the hits, or the explanation of "No results".
    readonly property string indexLine: L10n.i18nc("@info:status, %1 how many percent of the old messages are indexed", "Older messages are still being prepared for search (%1)", L10n.i18nc("@item:intext, %1 a percentage number", "%1%", App.searchResults.indexPercent))
    // The chip and the placeholder (tests read them).
    readonly property alias searchChip: chipItem
    readonly property alias placeholder: emptyList
    // Nothing is found for the text asked: the message search answered it (not capped: a search
    // that stopped early may have missed hits), no chat matches and no hit shows.
    // The search looked through all it may (capped) and found nothing yet: it says so and offers
    // another round (R3 F6).
    readonly property bool cappedEmpty: !App.archiveShown && search.text.trim() !== "" && page.askedText === search.text.trim() && App.searchResults.active && App.searchResults.answered && App.searchResults.capped && App.searchResults.chat === page.searchChat && App.searchResults.count === 0 && list.count === 0
    // "Chats" above the chats that match, only while "Messages" shows (R3 F8, V10).
    readonly property bool chatsHeaderShown: page.hitsShown && !page.chipShown && list.count > 0
    readonly property bool noResults: !App.archiveShown && search.text.trim() !== "" && page.askedText === search.text.trim() && App.searchResults.active && App.searchResults.answered && !App.searchResults.capped && App.searchResults.chat === page.searchChat && App.searchResults.count === 0 && list.count === 0

    // What the menu told a screen reader when it opened with nothing to choose, and how (tests
    // read both; "" and -1 while it did not).
    property string menuAnnounced: ""
    property int menuAnnouncedPoliteness: -1

    signal openConversation(real id)
    // The chooser of "Open Chat Beside" (the window's state `choosingBeside`): while it stands a banner
    // names the chat it opens beside, and the banner's close button, Escape in the list or in the
    // search field ask the window to end it.
    property bool choosing: false
    property string choosingName: ""
    readonly property Item chooseBanner: chooseLoader.item
    readonly property alias openBesideEntry: openBesideEntry
    readonly property alias closeChatEntry: closeChatEntry
    readonly property alias swapChatsEntry: swapChatsEntry
    signal chooseCancelled()
    // The row menu's "Open in Split View" for chat `id`.
    signal openBesideAsked(real id)
    // The row menu's and the hover button's "Close Chat" for chat `id`, and the menu's "Swap Chats".
    signal closeChatAsked(real id)
    signal swapChatsAsked
    // A hit of the message search was chosen: its chat opens at `message`, and the list stays
    // as it is (the search, its hits, the view).
    signal openSearchHit(real id, real message)
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

    // Search in the open chat only (G1i): the chip names the chat and the keyboard goes to the field.
    // What is typed already searches that chat alone. A chat that is not open or is protected does
    // nothing (it offers no search).
    function searchInChat(): void {
        if (App.timeline.conversationId === 0 || !App.timeline.chatSearchable) {
            return;
        }
        if (page.searchChat !== App.timeline.conversationId) {
            page.chipTitle = App.timeline.title;
            page.chipAvatar = App.timeline.avatarUrl;
            page.chipSelf = App.timeline.isSelf;
            page.searchChat = App.timeline.conversationId;
            // The chats part goes empty: the list is shown as without a query.
            App.setSearch("");
            if (search.text.trim() !== "") {
                searchDelay.stop();
                page.runSearch();
            } else {
                App.closeSearch();
            }
        }
        page.focusSearch();
    }

    // The chip's X and Backspace in an empty field: the chip goes and the same text searches every
    // chat (the chats that match, and the hits of all).
    function removeChip(): void {
        if (page.searchChat === 0) {
            return;
        }
        page.searchChat = 0;
        const text = search.text.trim();
        App.setSearch(search.text);
        if (text !== "") {
            searchDelay.stop();
            page.askedText = text;
            page.runSearch();
        } else {
            App.closeSearch();
        }
        search.forceActiveFocus();
    }

    // Escape: the field is emptied, the chip goes and the search closes (the list as without a
    // query). True when there was something to end.
    function endSearch(): bool {
        if (search.text === "" && page.searchChat === 0) {
            return false;
        }
        searchDelay.stop();
        page.searchChat = 0;
        search.text = "";
        App.setSearch("");
        App.closeSearch();
        page.askedText = "";
        return true;
    }

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

    // Down from the search field: the first chat that matches, else the first hit (false: there
    // is neither, the key goes on as before).
    function enterList(): bool {
        if (list.count > 0) {
            list.currentIndex = 0;
            list.forceActiveFocus(Qt.TabFocusReason);
            return true;
        }
        return page.hitsShown && page.focusHit(0);
    }

    // The hit at `index` takes the keyboard (inside the list, which stays its one tab stop) and
    // comes into view. No chat row is current meanwhile, so no chat shows the focus frame.
    function focusHit(index: int): bool {
        const rows = page.hitRows;
        if (rows === null || index < 0 || index >= rows.count) {
            return false;
        }
        const row = rows.itemAt(index) as SearchResultDelegate;
        if (row === null) {
            return false;
        }
        list.currentIndex = -1;
        if (!list.activeFocus) {
            list.forceActiveFocus(Qt.TabFocusReason);
        }
        row.forceActiveFocus(Qt.TabFocusReason);
        const at = row.mapToItem(list.contentItem, 0, 0);
        if (at.y < list.contentY) {
            list.contentY = at.y;
        } else if (at.y + row.height > list.contentY + list.height) {
            list.contentY = at.y + row.height - list.height;
        }
        return true;
    }

    // Up from the first hit: back to the last chat (the field when no chat matches).
    function leaveHits(row: Item): void {
        row.focus = false;
        if (list.count > 0) {
            list.currentIndex = list.count - 1;
            list.forceActiveFocus(Qt.BacktabFocusReason);
            list.positionViewAtIndex(list.currentIndex, ListView.Contain);
        } else {
            search.forceActiveFocus(Qt.BacktabFocusReason);
        }
    }

    // The hit that has the keyboard (its place among the hits), -1 when none.
    function focusedHit(): int {
        const rows = page.hitRows;
        for (let i = 0; rows !== null && i < rows.count; i++) {
            const row = rows.itemAt(i);
            if (row !== null && row.activeFocus) {
                return i;
            }
        }
        return -1;
    }

    // The current row over the chats AND the hits (the hits come after the chats): the keyboard's
    // place for Page Up, Page Down, Home and End (R3 F7). -1: none.
    function currentRow(): int {
        const hit = page.focusedHit();
        return hit >= 0 ? list.count + hit : list.currentIndex;
    }

    // Goes to row `row` of that one sequence (clamped): a chat becomes current, a hit takes the keyboard.
    function goRow(row: int): void {
        const hits = page.hitsShown && page.hitRows !== null ? page.hitRows.count : 0;
        const last = list.count + hits - 1;
        if (last < 0) {
            return;
        }
        const target = Math.max(0, Math.min(last, row));
        if (target < list.count) {
            const held = page.focusedHit();
            if (held >= 0) {
                (page.hitRows.itemAt(held) as Item).focus = false;
            }
            list.currentIndex = target;
            list.forceActiveFocus(Qt.TabFocusReason);
            list.positionViewAtIndex(target, ListView.Contain);
        } else {
            page.focusHit(target - list.count);
        }
    }

    // How many rows a screen holds (at least one row's move).
    function screenRows(): int {
        return Math.max(1, Math.floor(list.height / (Kirigami.Units.gridUnit * 3.6)) - 1);
    }

    // Page Up, Page Down, Home and End on the list and on a hit; true when the key was one of them.
    function pageKey(event: var): bool {
        if (event.modifiers & (Qt.ControlModifier | Qt.AltModifier | Qt.MetaModifier | Qt.ShiftModifier)) {
            return false;
        }
        const at = page.currentRow();
        switch (event.key) {
        case Qt.Key_PageDown:
            page.goRow(at < 0 ? page.screenRows() - 1 : at + page.screenRows());
            return true;
        case Qt.Key_PageUp:
            page.goRow(at < 0 ? 0 : at - page.screenRows());
            return true;
        case Qt.Key_Home:
            page.goRow(0);
            return true;
        case Qt.Key_End:
            page.goRow(list.count + (page.hitsShown && page.hitRows !== null ? page.hitRows.count : 0) - 1);
            return true;
        }
        return false;
    }

    // The hit that held the keyboard went (deleted, expired): the next hit takes it, else the
    // previous one, else the field (R3 F8, C14). `at`: where it stood.
    property int refocusAt: -1
    function refocusHit(): void {
        const at = page.refocusAt;
        page.refocusAt = -1;
        if (at < 0 || page.focusedHit() >= 0 || list.currentIndex >= 0) {
            return;
        }
        const count = page.hitRows !== null ? page.hitRows.count : 0;
        if (count > 0) {
            page.focusHit(Math.min(at, count - 1));
        } else {
            search.forceActiveFocus();
        }
    }
    Connections {
        target: App.searchResults
        function onRowsAboutToBeRemoved(parent: var, first: int, last: int): void {
            const held = page.focusedHit();
            if (held >= first && held <= last) {
                page.refocusAt = first;
            }
        }
        function onRowsRemoved(): void {
            if (page.refocusAt >= 0) {
                Qt.callLater(page.refocusHit);
            }
        }
        // One announcement for each answered text: how many messages it found.
        function onAnsweredChanged(): void {
            page.announceAnswer();
        }
    }
    function announceAnswer(): void {
        const hits = App.searchResults;
        if (!hits.answered || hits.count === 0 || page.announcedText === page.askedText) {
            return;
        }
        page.announcedText = page.askedText;
        const text = L10n.i18ncp("@info accessible, %1 how many", "%1 message found", "%1 messages found", hits.count);
        page.searchAnnounced = text;
        page.Accessible.announce(text, Accessible.Polite);
    }

    // The next page of hits once the end of the list is near (one request at a time).
    function moreHitsSoon(): void {
        const hits = App.searchResults;
        if (!page.hitsShown || !hits.more || hits.loading) {
            return;
        }
        if (list.contentY + list.height >= list.originY + list.contentHeight - Kirigami.Units.gridUnit * 10) {
            App.moreSearch();
        }
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
        // "Open in Split View": a chat is open and this one is shown in no pane.
        rowMenu.beside = App.timeline.conversationId !== 0 && App.paneOf(row.conversationId) < 0;
        // "Close Chat" / "Swap Chats": this chat is shown in a pane (the swap needs the split).
        rowMenu.shownInPane = App.paneOf(row.conversationId) >= 0;
        rowMenu.swappable = rowMenu.shownInPane && App.split;
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
                page.searchChat = 0;
                search.text = "";
            }
        }
    }
    // The one-chat search follows its chat: another chat opening (a chat row, the quick switch,
    // the step keys, a notification) ends it, and so does the chat turning protected. A hit of
    // the chat opens the same chat: the chip and the hits stay. (Properties of the page, not a
    // `Connections` on the timeline: qmllint types its target as a plain QObject.)
    readonly property real openChatId: App.timeline.conversationId
    readonly property bool openChatProtected: App.timeline.chatProtected
    onOpenChatIdChanged: {
        if (page.searchChat !== 0 && page.openChatId !== 0 && page.openChatId !== page.searchChat) {
            page.endSearch();
        }
    }
    onOpenChatProtectedChanged: {
        if (page.searchChat !== 0 && page.openChatProtected && page.openChatId === page.searchChat) {
            page.endSearch();
        }
    }
    leftPadding: 0
    rightPadding: 0
    topPadding: 0
    bottomPadding: 0

    // The split's layout asks for the rail (`MainWindow.splitLayout`): the list narrowed to a column of avatars
    // with their unread counts, a chat's name as its tooltip; no search field. A click opens a chat as ever.
    property bool rail: false
    readonly property real fullWidth: Kirigami.Units.gridUnit * 21
    readonly property real railWidth: Kirigami.Units.gridUnit * 4
    Kirigami.ColumnView.minimumWidth: page.rail ? page.railWidth : Kirigami.Units.gridUnit * 17
    Kirigami.ColumnView.maximumWidth: page.rail ? page.railWidth : Kirigami.Units.gridUnit * 24
    Kirigami.ColumnView.preferredWidth: page.rail ? page.railWidth : page.fullWidth

    header: QQC2.Control {
        // (The rail has no room for the search, the banners and the account's notice.)
        visible: !page.rail
        topPadding: Kirigami.Units.smallSpacing
        bottomPadding: Kirigami.Units.smallSpacing
        leftPadding: Kirigami.Units.largeSpacing
        rightPadding: Kirigami.Units.largeSpacing
        contentItem: ColumnLayout {
            spacing: Kirigami.Units.smallSpacing

            // The chooser of "Open Chat Beside": the next chat picked here opens beside the open one. The banner is
            // made only while the chooser stands (the list page is made at the start, and an inline message with its
            // action bar is no small item; `chooseBanner` is null otherwise).
            Loader {
                id: chooseLoader

                active: page.choosing
                visible: chooseLoader.active
                Layout.fillWidth: true
                sourceComponent: Kirigami.InlineMessage {
                    objectName: "choose-banner"
                    visible: true
                    type: Kirigami.MessageType.Information
                    text: L10n.i18nc("@info, %1 the open chat's name", "Choose a chat to open beside %1", Style.plain(Style.unmarked(page.choosingName)))
                    actions: [
                        Kirigami.Action {
                            icon.name: "dialog-close"
                            text: L10n.i18nc("@action:button", "Cancel")
                            displayHint: Kirigami.DisplayHint.IconOnly
                            onTriggered: page.chooseCancelled()
                        }
                    ]
                }
            }

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

            // The search field, with the one-chat search's chip BEFORE it (G1i).
            RowLayout {
                // (Not in the archive view: the row would keep a gap there, which the field alone never left.)
                visible: !App.archiveShown
                Layout.fillWidth: true
                spacing: Kirigami.Units.smallSpacing

                    // The chip: the chat's picture, "In: <name>" and an X that goes back to every chat. A
                    // part of the search field's line: the field's height, a button's corner and the
                    // frame every other frame takes (R3 F3); the picture grows with the font.
                    Item {
                        id: chipItem

                        objectName: "search-chip"
                        visible: page.chipShown
                        Layout.alignment: Qt.AlignVCenter
                        Layout.preferredWidth: chipRow.implicitWidth + Kirigami.Units.smallSpacing * 2
                        // (Not tied to the field's width: the field takes what the chip leaves.)
                        Layout.maximumWidth: page.width * 0.5
                        Layout.preferredHeight: search.implicitHeight
                        Accessible.role: Accessible.StaticText
                        Accessible.name: L10n.i18nc("@info:status, the search is limited to this chat, %1 the chat's name", "In: %1", Style.unmarked(page.chipTitle))

                        Rectangle {
                            anchors.fill: parent
                            radius: Kirigami.Units.cornerRadius
                            color: Kirigami.Theme.backgroundColor
                            border.width: 1
                            border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                        }
                        RowLayout {
                            id: chipRow

                            anchors.fill: parent
                            anchors.leftMargin: Kirigami.Units.smallSpacing
                            anchors.rightMargin: Kirigami.Units.smallSpacing
                            spacing: Kirigami.Units.smallSpacing

                            Avatar {
                                name: page.chipTitle
                                source: page.chipAvatar
                                selfChat: page.chipSelf
                                implicitWidth: chipLabel.implicitHeight
                                implicitHeight: chipLabel.implicitHeight
                                Layout.preferredWidth: chipLabel.implicitHeight
                                Layout.preferredHeight: chipLabel.implicitHeight
                                Layout.alignment: Qt.AlignVCenter
                            }
                            QQC2.Label {
                                id: chipLabel

                                objectName: "search-chip-label"
                                text: L10n.i18nc("@info:status, the search is limited to this chat, %1 the chat's name", "In: %1", Style.plain(page.chipTitle))
                                textFormat: Text.PlainText
                                color: Style.readable(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                                elide: Text.ElideRight
                                Layout.fillWidth: true
                                Layout.minimumWidth: 0
                                Layout.alignment: Qt.AlignVCenter
                            }
                            QQC2.ToolButton {
                                id: chipX

                                objectName: "search-chip-x"
                                icon.name: "dialog-close"
                                display: QQC2.AbstractButton.IconOnly
                                // A Tab stop (Space or Enter takes the chip off); Backspace in the empty
                                // field and Escape do the same.
                                focusPolicy: Qt.TabFocus
                                padding: 0
                                implicitWidth: Kirigami.Units.iconSizes.small + Kirigami.Units.smallSpacing
                                implicitHeight: implicitWidth
                                Layout.alignment: Qt.AlignVCenter
                                text: L10n.i18nc("@action:button", "Search All Chats")
                                Accessible.name: chipX.text
                                QQC2.ToolTip.visible: chipX.hovered
                                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                                QQC2.ToolTip.text: chipX.text
                                onClicked: page.removeChip()
                            }
                        }
                    }

                Kirigami.SearchField {
                    id: search

                    visible: !App.archiveShown
                    placeholderText: page.regexOn ? L10n.i18nc("@info:placeholder", "Search by regular expression") : L10n.i18nc("@info:placeholder", "Search")
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
                    // The chats are filtered at once; the messages are searched 200 ms after the
                    // last key (an empty field ends the search at once).
                    onTextChanged: {
                        // (While the chip stands the chats are not filtered: their part is empty.)
                        App.setSearch(page.chipShown ? "" : text);
                        if (text.trim() === "") {
                            searchDelay.stop();
                            App.closeSearch();
                        } else {
                            searchDelay.restart();
                        }
                    }
                    // Down goes from a query into the list: its first chat, or the first hit when no
                    // chat matches (Enter is the quick switch and does nothing then).
                    Keys.onDownPressed: event => {
                        event.accepted = search.text.trim() !== "" && page.enterList();
                    }
                    // Escape empties the field, takes the chip off and closes the search (the list as
                    // without a query); with nothing to end it passes on as before. Backspace in an
                    // empty field takes the chip off.
                    Keys.onEscapePressed: event => {
                        if (page.choosing) {
                            page.chooseCancelled();
                            event.accepted = true;
                            return;
                        }
                        event.accepted = page.endSearch();
                    }
                    Keys.onPressed: event => {
                        if (event.key === Qt.Key_Backspace && search.text === "" && page.chipShown && !(event.modifiers & (Qt.ControlModifier | Qt.AltModifier | Qt.MetaModifier))) {
                            page.removeChip();
                            event.accepted = true;
                        }
                    }
                    // The quick switch: type a name, Enter opens the first chat that matches, and
                    // the keyboard goes on to the message. The window empties the field once the
                    // chat opens (`MainWindow.switchTo`): kept when the switch is not made (it
                    // asks "Stop Linking?" first and the answer is "Continue Linking").
                    onAccepted: {
                        // (No quick switch in a one-chat search: no chat shows.)
                        const id = page.chipShown ? 0 : App.conversationAt(0);
                        if (id !== 0) {
                            page.switchedByKeyboard(id);
                        }
                    }
                    Layout.fillWidth: true

                    Timer {
                        id: searchDelay

                        interval: 200
                        onTriggered: page.runSearch()
                    }
                }
                // `.*`: the text is a regular expression. `Aa`: case matters (only with `.*`).
                QQC2.ToolButton {
                    id: regexButton

                    objectName: "search-regex"
                    checkable: true
                    checked: page.regexOn
                    text: ".*"
                    font: Kirigami.Theme.fixedWidthFont
                    Accessible.name: L10n.i18nc("@info accessible", "Regular Expression")
                    Accessible.description: L10n.i18nc("@info accessible", "Search messages by a regular expression")
                    QQC2.ToolTip.visible: regexButton.hovered
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Regular Expression")
                    onToggled: {
                        page.regexOn = regexButton.checked;
                        page.modeChanged();
                    }
                }
                QQC2.ToolButton {
                    id: matchCaseButton

                    objectName: "search-match-case"
                    checkable: true
                    checked: page.matchCase
                    enabled: page.regexOn
                    text: "Aa"
                    Accessible.name: L10n.i18nc("@info accessible", "Match Case")
                    QQC2.ToolTip.visible: matchCaseButton.hovered
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    QQC2.ToolTip.text: page.regexOn ? L10n.i18nc("@info:tooltip", "Match Case") : L10n.i18nc("@info:tooltip", "Match Case (turn on Regular Expression first)")
                    onToggled: {
                        page.matchCase = matchCaseButton.checked;
                        page.modeChanged();
                    }
                }
            }
            // The pattern's problem and where, under the field; "Save as Filter" for a valid one.
            QQC2.Label {
                objectName: "search-regex-error"
                visible: !App.archiveShown && page.regexError !== ""
                text: page.regexError
                textFormat: Text.PlainText
                wrapMode: Text.WordWrap
                color: Style.readable(Kirigami.Theme.negativeTextColor, Kirigami.Theme.backgroundColor)
                Accessible.role: Accessible.StaticText
                Accessible.name: page.regexError
                Layout.fillWidth: true
                Layout.leftMargin: Kirigami.Units.smallSpacing
            }
            QQC2.Button {
                id: saveFilterButton

                objectName: "search-save-filter"
                visible: !App.archiveShown && page.canSaveFilter
                text: L10n.i18nc("@action:button", "Save as Filter")
                icon.name: "view-filter"
                Accessible.name: L10n.i18nc("@action:button", "Save as Filter")
                Layout.alignment: Qt.AlignRight
                onClicked: page.saveAsFilter()
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

    // Save as Filter (R5): the filter dialog, opened with the search pattern in Message text.
    FilterDialog {
        id: saveFilterDialog

        objectName: "saveFilterDialog"
        parent: QQC2.Overlay.overlay
    }

    ListView {
        id: list

        // The list is one tab stop (not a stop per chat): Up and Down move the current row (the
        // list's own keys), Page Up, Page Down, Home and End move it over the chats AND the hits
        // (`pageKey`: a ListView handles the arrows only), Enter or Space opens it. No row is
        // current until the keyboard moves into the list.
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

        // (No chats while a one-chat search stands: only its hits show.)
        model: page.rowsShown && !page.chipShown ? App.conversations : null
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
        // Down past the last chat (or with no chat at all) goes on into the hits of the
        // message search; the hits take Up and Down themselves and Up from the first comes back.
        Keys.onDownPressed: event => {
            event.accepted = page.hitsShown && (list.count === 0 || list.currentIndex === list.count - 1) && page.focusHit(0);
        }
        // Esc and Alt+Left leave the archive view while the list has the keyboard.
        Keys.onEscapePressed: event => {
            if (page.choosing) {
                page.chooseCancelled();
                event.accepted = true;
                return;
            }
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
            if (page.pageKey(event)) {
                event.accepted = true;
            } else if (event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
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

        // More hits of the message search as the end of the list comes near.
        // (Later, outside the change that caused it: a page asked inside a layout pass changed
        // `loading` under the bindings that were being worked out.)
        onContentYChanged: Qt.callLater(page.moreHitsSoon)
        onContentHeightChanged: Qt.callLater(page.moreHitsSoon)

        // The way into the archive: the last row, its own stop of the keyboard (below). While a
        // search has hits, the footer holds them instead (the entry never shows with a query).
        header: page.chatsHeaderShown ? chatsHeaderComponent : null
        footer: page.rail ? null : (page.hitsShown ? hitsComponent : (page.entryShown ? archiveEntryComponent : null))

        delegate: ConversationDelegate {
            id: row

            width: ListView.view.width
            rail: page.rail
            highlighted: App.conversations.currentId === conversationId
            onClicked: page.openConversation(conversationId)
            onMenuRequested: (x, y) => page.openMenu(row, x, y)
            onBesideRequested: page.openBesideAsked(conversationId)
            onCloseRequested: page.closeChatAsked(conversationId)
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
            // which is no reason to say there is no conversation. Nor while the search found
            // messages: the hits are the list then.
            visible: page.rowsShown && list.count === 0 && !page.entryShown && !page.hitsShown && emptyList.text !== ""
            icon.name: App.archiveShown ? "archive-insert" : "dialog-messages"
            // After a capped search with no hit: another round of the search.
            helpfulAction: Kirigami.Action {
                visible: page.cappedEmpty
                enabled: visible
                text: L10n.i18nc("@action:button", "Search Further")
                icon.name: "edit-find"
                onTriggered: App.moreSearch()
            }
            text: {
                if (App.archiveShown) {
                    return L10n.i18nc("@info:placeholder", "No archived chats");
                }
                if (emptyList.query !== "" || page.chipShown) {
                    // The messages are searched too: nothing is said until that search answered
                    // this text (no "No chats match" that turns into hits a moment later).
                    if (page.cappedEmpty) {
                        return L10n.i18nc("@info:placeholder", "Kourier looked through many messages and found none yet.");
                    }
                    return page.noResults ? L10n.i18nc("@info:placeholder, %1 what was searched for", "No results for “%1”", Style.plain(emptyList.query)) : "";
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
                if (App.archiveShown) {
                    return "";
                }
                if (emptyList.query !== "" || page.chipShown) {
                    // While the index fills, why there may be more later.
                    return page.noResults && !App.searchResults.indexReady ? page.indexLine : "";
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

    // "Chats" above the chats that match while "Messages" shows (R3 F8, V10).
    Component {
        id: chatsHeaderComponent

        Kirigami.ListSectionHeader {
            width: list.width
            text: L10n.i18nc("@title:group the chats that match a search", "Chats")
            focusPolicy: Qt.NoFocus
        }
    }

    // The footer of the list while a search has hits: "Messages" and the hits, newest first. A
    // hit opens its chat at the message; the list stays as it is.
    Component {
        id: hitsComponent

        Column {
            id: hitsColumn

            width: list.width
            objectName: "search-hits"
            // The section is a list of its own for a screen reader (the footer of the chats' list).
            Accessible.role: Accessible.List
            Accessible.name: L10n.i18nc("@info accessible, the hits of a search in the messages", "Messages")

            Kirigami.ListSectionHeader {
                width: hitsColumn.width
                text: L10n.i18nc("@title:group the hits of a search in the messages", "Messages")
                focusPolicy: Qt.NoFocus
            }
            // The index still fills: older messages may add hits later (the line moves by itself).
            QQC2.Label {
                objectName: "search-index-line"
                width: hitsColumn.width - Kirigami.Units.largeSpacing * 2
                x: Kirigami.Units.largeSpacing
                visible: !App.searchResults.indexReady
                text: page.indexLine
                textFormat: Text.PlainText
                wrapMode: Text.WordWrap
                bottomPadding: Kirigami.Units.smallSpacing
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                Accessible.role: Accessible.StaticText
                Accessible.name: text
            }
            Repeater {
                id: hitRepeater

                model: App.searchResults
                Component.onCompleted: page.hitRows = hitRepeater
                Component.onDestruction: {
                    if (page.hitRows === hitRepeater) {
                        page.hitRows = null;
                    }
                }

                delegate: SearchResultDelegate {
                    id: hit

                    width: hitsColumn.width
                    oneChat: page.chipShown
                    onClicked: page.openSearchHit(hit.conversationId, hit.message)
                    Keys.onPressed: event => {
                        event.accepted = page.pageKey(event);
                    }
                    // The hits are rows of the list for the keyboard: Up and Down move between
                    // them (Up from the first goes back to the chats), Enter opens one as Space does.
                    Keys.onDownPressed: page.focusHit(hit.index + 1)
                    Keys.onUpPressed: {
                        if (hit.index === 0) {
                            page.leaveHits(hit);
                        } else {
                            page.focusHit(hit.index - 1);
                        }
                    }
                    Keys.onReturnPressed: hit.clicked()
                    Keys.onEnterPressed: hit.clicked()
                    // Escape gives the keyboard back to the field (the next Escape clears).
                    Keys.onEscapePressed: event => {
                        hit.focus = false;
                        search.forceActiveFocus(Qt.BacktabFocusReason);
                        event.accepted = true;
                    }
                }
            }
            // (A page that came while the end was in view: the next one once it is read.)
            readonly property bool idle: !App.searchResults.loading
            onIdleChanged: Qt.callLater(page.moreHitsSoon)
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
            // (The rail has no room for a title: none is shown, the sidebar beside it has its own buttons.)
            implicitWidth: page.rail ? 0 : Math.ceil(chatsFull.advanceWidth + chatsTitleItem.lead)
            implicitHeight: Math.ceil(chatsHeadingText.implicitHeight)

            TextMetrics {
                id: chatsFull
                font: chatsHeadingText.font
                text: page.title
            }
            Kirigami.Heading {
                id: chatsHeadingText
                objectName: "chatsHeadingText"
                visible: !page.rail
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
        // Whether "Open in Split View" is offered for that chat, when the menu opened.
        property bool beside: false
        // Whether the chat is shown in a pane, and whether the split is on too (when the menu opened).
        property bool shownInPane: false
        property bool swappable: false
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
            id: openBesideEntry

            objectName: "rowOpenBeside"
            // An action that does nothing is not shown: only with a chat open that this chat is not.
            visible: rowMenu.beside
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "view-split-left-right"
            text: L10n.i18nc("@action:inmenu", "Open in Split View")
            onTriggered: page.openBesideAsked(rowMenu.conversationId)
        }

        QQC2.MenuItem {
            id: closeChatEntry

            objectName: "rowCloseChat"
            visible: rowMenu.shownInPane
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "view-close"
            text: L10n.i18nc("@action:inmenu", "Close Chat")
            onTriggered: page.closeChatAsked(rowMenu.conversationId)
        }

        QQC2.MenuItem {
            id: swapChatsEntry

            objectName: "rowSwapChats"
            visible: rowMenu.swappable
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "exchange-positions"
            text: L10n.i18nc("@action:inmenu", "Swap Chats")
            onTriggered: page.swapChatsAsked()
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
            visible: openBesideEntry.visible || closeChatEntry.visible || swapChatsEntry.visible || markReadEntry.visible || rowMenu.offer.shown === true
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
