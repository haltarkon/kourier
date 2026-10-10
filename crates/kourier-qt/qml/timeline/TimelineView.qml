pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQml.Models
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import app.kourier

// The timeline: newest message at the bottom (row 0, BottomToTop), a delegate per row kind.
// Reaching the top loads older pages (keyset paging in Rust); a window that dropped its newest
// rows loads them back at the bottom. The message popups live here, one of each for all rows:
// the context menu, the reaction picker, the list of who reacted, the delete question and the
// edit history.
// With the keyboard the timeline is one tab stop: Tab focuses it and makes the newest message
// current, Up and Down move between messages and notices. Enter (or Space) does what a click
// on the current row's picture, file or voice note does (view, open, play, download), and on
// a row without one opens its menu; Ctrl+Enter goes to the message the row quotes; the Menu
// key (or Shift+F10) opens the menu, which has everything the row offers. Escape returns to
// the composer, and the next Tab leaves the timeline. A click never makes a row current (the
// view stays where it is). On a row whose long text is shown in part, Enter shows more of it
// first ("Read more": the view stops following the newest message and keeps the line where
// the text was cut where it is).
QQC2.ScrollView {
    id: root

    // The view shows the oldest loaded row, or the newest message, give or take a line. By
    // distance (see `fromTop` below): a programmatic jump to the newest row lands one margin
    // short of the very end, where the list's own `atYEnd` is still false.
    readonly property bool atTop: root.fromTop <= list.topMargin + Kirigami.Units.gridUnit
    readonly property bool atBottom: root.fromBottom <= list.bottomMargin + Kirigami.Units.gridUnit
    // Keep the view at the oldest loaded row (the top) as pages arrive (`--page=history`).
    property bool pinToOldest: false
    // Follow the newest message (new arrivals, own sends, a resize). The list sets it from
    // where the user leaves the view (see `moved` there): false once the view is moved away
    // from the newest message, true again when it rests there.
    property bool followNewest: true
    // Distance of the viewport from the top (oldest) and the bottom (newest) of the content.
    // `atYBeginning` is not reliable here: with BottomToTop and a top margin, a programmatic
    // jump to the top lands one margin short of it.
    readonly property real fromTop: list.contentY - list.originY + list.topMargin
    readonly property real fromBottom: list.originY + list.contentHeight + list.bottomMargin - list.contentY - list.height
    // The reaction picker is made when it is first asked for, and stays from then on (`null`
    // until then): with the desktop style it costs the Qt thread a third of what the whole
    // chat page costs (§1.3: opening the first chat), and a chat is read far more often
    // than a reaction is picked. The context menu and the list of who reacted are made with
    // the page. The menu must be: the style's menu lays its entries out when a frame is
    // prepared, and one that is opened in the moment it was made can stay as small as its
    // padding, with every entry on top of the other (seen in about every third first
    // opening when the menu was made on demand; forcing the layout did not help).
    property ReactionPicker pickerPopup: null
    // One of the message popups is open.
    readonly property bool popupOpen: menu.visible || whoReacted.visible || (root.pickerPopup !== null && root.pickerPopup.visible)
    // The list of rows and the context menu (tests drive them with input events).
    readonly property alias rowList: list
    readonly property alias messageMenu: menu

    function ensurePicker(): ReactionPicker {
        if (root.pickerPopup === null) {
            root.pickerPopup = pickerComponent.createObject(root) as ReactionPicker;
        }
        return root.pickerPopup;
    }

    // Escape in the timeline: the keyboard focus goes back to the composer.
    signal composerRequested

    QQC2.ScrollBar.horizontal.policy: QQC2.ScrollBar.AlwaysOff

    onPopupOpenChanged: {
        if (!root.popupOpen) {
            TimelineActions.activeMessage = 0;
            // A menu entry moved the keyboard on (Reply and Edit go to the composer).
            if (!list.activeFocus) {
                list.dropCurrent();
            }
        }
    }

    // Where a popup for `anchor` goes: a point of this view, which stays while rows are
    // rebuilt under the popup (a reaction changes the row it was opened from).
    function place(anchor: Item, x: real, y: real): point {
        return anchor ? anchor.mapToItem(root, x, y) : Qt.point(root.width / 2, root.height / 2);
    }

    // The context menu of `row` at (`x`, `y`) of `anchor`; with a negative `x`, beside
    // `anchor` (on the side the window has room), so the message stays in view.
    function openMenu(anchor: Item, row: var, x: real, y: real): void {
        menu.row = row;
        menu.anchor = anchor;
        TimelineActions.activeMessage = row.messageId;
        if (x >= 0 || !anchor) {
            const at = root.place(anchor, x, y);
            menu.popup(root, at.x, at.y);
            return;
        }
        const gap = Kirigami.Units.smallSpacing;
        const corner = root.place(anchor, 0, 0);
        const before = corner.x - menu.implicitWidth - gap;
        const after = corner.x + anchor.width + gap;
        const fits = after + menu.implicitWidth <= root.width;
        // Own messages sit at the trailing edge: their menu opens before them.
        const left = row.own ? (before >= 0 ? before : after) : (fits ? after : Math.max(0, before));
        const top = Math.max(gap, Math.min(corner.y, root.height - menu.implicitHeight - gap));
        menu.popup(root, left, top);
    }

    // Where the picker was asked for (a point of this view) and how tall its anchor is.
    property point pickerAt: Qt.point(0, 0)
    property real pickerAnchorHeight: 0

    // Above the anchor when the picker fits there, else below it; never over the anchor while
    // the window has room elsewhere.
    function placePicker(): void {
        const picker = root.pickerPopup;
        if (picker === null) {
            return;
        }
        const gap = Kirigami.Units.smallSpacing;
        const above = root.pickerAt.y - picker.implicitHeight - gap;
        const below = root.pickerAt.y + root.pickerAnchorHeight + gap;
        picker.x = Math.round(Math.max(gap, Math.min(root.pickerAt.x, root.width - picker.implicitWidth - gap)));
        picker.y = Math.round(above >= gap || below + picker.implicitHeight > root.height ? Math.max(gap, above) : below);
    }

    function openPicker(anchor: Item, row: var): void {
        root.pickerAt = root.place(anchor, 0, 0);
        root.pickerAnchorHeight = anchor ? anchor.height : 0;
        const picker = root.ensurePicker();
        picker.row = row;
        TimelineActions.activeMessage = row.messageId;
        picker.expanded = false;
        root.placePicker();
        picker.open();
    }

    // Where the list of who reacted was asked for, and how tall its anchor is.
    property point reactionsAt: Qt.point(0, 0)
    property real reactionsAnchorHeight: 0

    // Below the chips when the list fits there, else above them.
    function placeReactions(): void {
        const gap = Kirigami.Units.smallSpacing;
        const below = root.reactionsAt.y + root.reactionsAnchorHeight + gap;
        whoReacted.x = Math.round(Math.max(gap, Math.min(root.reactionsAt.x, root.width - whoReacted.implicitWidth - gap)));
        whoReacted.y = Math.round(below + whoReacted.implicitHeight <= root.height ? below : Math.max(gap, root.reactionsAt.y - whoReacted.implicitHeight - gap));
    }

    function openReactions(anchor: Item, row: var): void {
        root.reactionsAt = root.place(anchor, 0, 0);
        root.reactionsAnchorHeight = anchor ? anchor.height : 0;
        whoReacted.row = row;
        TimelineActions.activeMessage = row.messageId;
        root.placeReactions();
        whoReacted.open();
    }

    function openDelete(row: var): void {
        const dialog = deleteComponent.createObject(root, {
            "row": row
        }) as DeleteDialog;
        dialog?.open();
    }

    // The question which attachment of `row` to delete; `false` when it has none to offer.
    function openAttachmentDelete(row: var): bool {
        if (!row || !row.attachments || row.attachments.length === 0 || row.actions.deleteAttachment !== true) {
            return false;
        }
        const dialog = attachmentComponent.createObject(root, {
            "row": row
        }) as AttachmentDeleteDialog;
        dialog?.open();
        root.attachmentDialog = dialog;
        return dialog !== null;
    }

    // That question for a message, by its id (the fixture pages and tests).
    function attachmentFor(message: real): bool {
        const item = root.delegateFor(message);
        return item !== null && root.openAttachmentDelete(item.rowInfo());
    }

    // The attachment question that was opened last (tests).
    property AttachmentDeleteDialog attachmentDialog: null

    // The row of a message, when it has a delegate (it is on screen or near it).
    function delegateFor(message: real): MessageDelegate {
        for (let i = 0; i < list.count; ++i) {
            const item = list.itemAtIndex(i) as MessageDelegate;
            if (item !== null && item.messageId === message) {
                return item;
            }
        }
        return null;
    }

    // The row of a notice (a system row), when it has a delegate.
    function noticeFor(message: real): SystemRow {
        for (let i = 0; i < list.count; ++i) {
            const item = list.itemAtIndex(i) as SystemRow;
            if (item !== null && item.messageId === message) {
                return item;
            }
        }
        return null;
    }

    // The newest notice on screen that offers something; 0 when there is none (tests).
    function newestNotice(): real {
        for (let i = 0; i < list.count; ++i) {
            const item = list.itemAtIndex(i) as SystemRow;
            if (item !== null && item.offers) {
                return item.messageId;
            }
        }
        return 0;
    }

    // Shows the earlier versions of a message; `false` when there is nothing to ask.
    function openHistory(row: var): bool {
        // (A double click on the mark: the history that is opening is the one asked for.)
        if (root.historyDialog !== null && root.historyDialog.visible && root.historyDialog.message === row.messageId) {
            return true;
        }
        if (!App.showRevisions(row.messageId)) {
            return false;
        }
        const dialog = historyComponent.createObject(root, {
            "message": row.messageId
        }) as RevisionsDialog;
        dialog?.open();
        root.historyDialog = dialog;
        return dialog !== null;
    }

    // The edit history of a message, by its id (the fixture pages and tests).
    function historyFor(message: real): bool {
        return message > 0 && root.openHistory({
            "messageId": message
        });
    }

    // Opens the viewer on the album of a message at picture `index` (the fixture pages and
    // tests); `false` while the message has no row on screen or its pictures are not there.
    function albumFor(message: real, index: int): bool {
        const item = root.delegateFor(message);
        return item !== null && item.openAlbum(index);
    }

    // Whether the albums on screen show all their pictures (at least one album, none still
    // loading): the fixture page waits for it.
    function albumsShown(): bool {
        let shown = 0;
        for (let i = 0; i < list.count; ++i) {
            const item = list.itemAtIndex(i) as MessageDelegate;
            const state = item !== null ? item.albumState() : 0;
            if (state === 1) {
                return false;
            }
            shown += state === 2 ? 1 : 0;
        }
        return shown > 0;
    }

    // The edit history that was opened last (tests).
    property RevisionsDialog historyDialog: null

    // The popups of one message, by its id (the fixture pages and tests open them this way).
    // `false` when the message has no row on screen.
    function menuFor(message: real): bool {
        const item = root.delegateFor(message);
        item?.openMenu();
        const notice = item === null ? root.noticeFor(message) : null;
        notice?.openMenu();
        return item !== null || notice !== null;
    }

    function pickerFor(message: real, expanded: bool): bool {
        const item = root.delegateFor(message);
        item?.openPicker();
        // Opening shows the quick row; the whole picker on request.
        if (root.pickerPopup !== null) {
            root.pickerPopup.expanded = expanded && item !== null;
        }
        return item !== null;
    }

    function reactionsFor(message: real): bool {
        const item = root.delegateFor(message);
        item?.openReactions();
        return item !== null;
    }

    function deleteFor(message: real): bool {
        const item = root.delegateFor(message);
        item?.openDelete();
        return item !== null;
    }

    ListView {
        id: list

        model: App.timeline
        verticalLayoutDirection: ListView.BottomToTop
        spacing: 0
        topMargin: Kirigami.Units.largeSpacing
        bottomMargin: Kirigami.Units.largeSpacing
        boundsBehavior: Flickable.StopAtBounds
        // Keep a few screens of delegates around while scrolling.
        cacheBuffer: Kirigami.Units.gridUnit * 30
        // No row is current until the keyboard moves into the timeline.
        currentIndex: -1
        activeFocusOnTab: true
        keyNavigationEnabled: false
        Accessible.role: Accessible.List
        Accessible.name: L10n.i18nc("@info accessible", "Messages")

        // A new conversation starts at its newest message.
        readonly property real conversation: App.timeline.conversationId
        onConversationChanged: {
            root.followNewest = true;
            list.held = null;
            list.currentIndex = -1;
            menu.close();
            root.pickerPopup?.close();
            whoReacted.close();
            list.toNewest();
        }

        // What the view holds on to while its content changes under it (rows arrive, go and
        // change their height, delegates are made and settle their wrapped height, the
        // window is resized): the newest message while it follows, the oldest loaded row
        // with `pinToOldest`, else the row at the top of the viewport (`held`), so what is
        // being read stays where it is. Without that a row inserted at the newest end pushes
        // the rows up whenever the newest row reaches into the viewport (one taller than
        // the window always does), and a row below the viewport that changes its height
        // moves everything above it.
        //
        // Who moved the view decides which of the two it is: every change of the view's
        // position that the view did not make itself (`own`) is the user's, whatever moved
        // it. The wheel and the touchpad do not go through the list: the desktop style's
        // ScrollView has a `Kirigami.WheelHandler` that takes their events and writes or
        // animates `contentY`, so the list never reports `moving`; the scroll bar writes
        // `contentY` too. A drag, a flick and a key that makes a row current move the list
        // itself. A move away from the newest message stops following at once; a view left
        // at the newest message follows again (`rest`).

        // The view is moving itself.
        property int own: 0
        // The bottom edge of the viewport in content coordinates, as last seen, and the
        // height the list had when it last said that it changed.
        property real edge: 0
        property real tall: 0
        // The row the view holds on to while it does not follow, and where its top is in
        // the viewport. (A delegate that is destroyed leaves `null` here.)
        property Item held: null
        property real heldAt: 0

        // Distance of the viewport from the newest end of the content, read from the list:
        // inside a handler of one of these properties a binding on them (`root.fromBottom`)
        // may still hold its old value.
        function below(): real {
            return list.originY + list.contentHeight + list.bottomMargin - list.contentY - list.height;
        }

        function atNewest(): bool {
            return list.below() <= list.bottomMargin + Kirigami.Units.gridUnit;
        }

        function following(): bool {
            return root.followNewest && !root.pinToOldest;
        }

        // The position of the view changed. A resize is no move of the user's: a
        // BottomToTop list changes `contentY` with its height to keep its bottom edge
        // where it is, before it says that its height changed (and at the very end of the
        // content it does not get there: the bounds it checks against are still those of
        // the old height).
        function moved(): void {
            const edge = list.contentY + list.height;
            if (Math.abs(edge - list.edge) < 0.01) {
                return;
            }
            const up = edge < list.edge;
            list.edge = edge;
            if (list.own > 0 || list.height !== list.tall) {
                return;
            }
            list.held = null;
            if (up && !list.atNewest()) {
                root.followNewest = false;
            }
            Qt.callLater(list.rest);
        }

        // Where the user left the view: at the newest message it follows again, anywhere
        // else it holds on to the row at its top. (Called a moment after a move, and when a
        // drag or flick ends: the list's sizes are settled then.) The newest row of a window
        // that ends before the newest message is not a place to follow from: the pages
        // that load below it would carry the view away.
        function rest(): void {
            if (list.moving || list.count === 0) {
                return;
            }
            if (!App.timeline.hasNewer && list.atNewest()) {
                root.followNewest = true;
            }
            if (!list.following() && list.held === null) {
                list.hold();
            }
        }

        // Holds on to the topmost row in the viewport (its top edge may be above it).
        function hold(): void {
            list.held = null;
            const x = list.width / 2;
            const end = list.contentY + list.height;
            for (let y = list.contentY + 1; y < end; y += Kirigami.Units.gridUnit) {
                const item = list.itemAt(x, y);
                if (item !== null) {
                    list.heldAt = item.y - list.contentY;
                    list.held = item;
                    return;
                }
            }
        }

        // The content changed: back to what the view holds on to, once the change is laid
        // out.
        function keep(): void {
            Qt.callLater(list.settle);
        }

        function settle(): void {
            // (A drag or flick is not interrupted; `onMovementEnded` comes here again.)
            if (list.count === 0 || list.moving) {
                return;
            }
            // The list lays the change out now. Where it moves the view while it does (it
            // keeps the rows in view in place when one of them goes), that is its own move.
            list.own += 1;
            list.forceLayout();
            list.own -= 1;
            if (root.pinToOldest) {
                list.toOldest();
            } else if (list.following()) {
                list.toNewest();
            } else if (list.held === null) {
                list.rest();
            } else {
                const y = list.bounded(list.held.y - list.heldAt);
                if (Math.abs(y - list.contentY) >= 0.5) {
                    list.own += 1;
                    list.contentY = y;
                    list.own -= 1;
                }
            }
        }

        // `y` as a position of the view, inside the content.
        function bounded(y: real): real {
            const top = list.originY - list.topMargin;
            const bottom = Math.max(top, list.originY + list.contentHeight + list.bottomMargin - list.height);
            return Math.max(top, Math.min(bottom, y));
        }

        function toNewest(): void {
            list.own += 1;
            list.forceLayout();
            list.positionViewAtBeginning();
            list.own -= 1;
        }

        function toOldest(): void {
            list.own += 1;
            list.forceLayout();
            list.positionViewAtEnd();
            list.own -= 1;
        }

        // To a row of the list, which the view then holds on to (a quoted message).
        function toRow(row: int): void {
            list.own += 1;
            list.forceLayout();
            list.positionViewAtIndex(row, ListView.Center);
            list.own -= 1;
            list.hold();
        }

        onCountChanged: list.keep()
        onContentHeightChanged: list.keep()
        onContentYChanged: list.moved()
        onHeightChanged: {
            list.tall = list.height;
            list.edge = list.contentY + list.height;
            list.keep();
        }
        onMovementEnded: {
            list.rest();
            list.keep();
        }
        // The keyboard in the timeline. Tab brings it here and makes the newest message
        // current (`Keys.onReleased` below: the release of the Tab that moved the focus
        // arrives here). A click into a message's text (to select it) or on a button of a row
        // also puts the focus inside the list, but makes no row current: the newest row made
        // current would pull the view down to it, away from what was clicked. Once the focus
        // leaves (not for one of the timeline's own popups) no row is current any more.
        onActiveFocusChanged: {
            if (list.activeFocus) {
                // (Once the window knows which item has the focus.)
                Qt.callLater(list.noteInner);
            } else if (!root.popupOpen) {
                list.dropCurrent();
                // The list forgets what a click focused inside it: the next Tab comes to the
                // list itself, not back into an old message.
                if (list.inner !== null) {
                    list.inner.focus = false;
                    list.inner = null;
                }
            }
        }

        // What a click inside a row gave the focus to (a message's text, a button).
        property Item inner: null

        function noteInner(): void {
            const item = list.Window.activeFocusItem;
            if (list.activeFocus && item !== null && item !== list && !(item instanceof MessageDelegate)) {
                list.inner = item;
            }
        }

        // The row the focus is inside, if it is inside one; else -1.
        function focusedRow(): int {
            let item = list.Window.activeFocusItem;
            while (item !== null && item !== list) {
                const row = item as MessageDelegate;
                if (row !== null) {
                    return row.index;
                }
                item = item.parent;
            }
            return -1;
        }

        // No current row: a list view keeps its current item in view, which would move the
        // timeline under the pointer whenever rows change.
        function dropCurrent(): void {
            list.currentIndex = -1;
        }

        // Up is older (a higher row), Down newer. The first arrow key makes a row current:
        // the one the focus is in (after a click into its text), else the newest message.
        function step(delta: int): void {
            if (list.currentIndex < 0) {
                const row = list.focusedRow();
                if (row >= 0) {
                    list.currentIndex = row;
                    return;
                }
            }
            list.moveCurrent(delta);
        }

        // The next message, or notice with a menu, from the current row in the direction of
        // `delta` becomes current (date headers and the unread marker are passed over);
        // without a current row, the newest one.
        function moveCurrent(delta: int): void {
            let i = list.currentIndex;
            if (i < 0) {
                delta = 1;
            }
            for (let n = 0; n < 6; ++n) {
                const next = i + delta;
                if (next < 0 || next >= list.count) {
                    break;
                }
                i = next;
                const item = list.itemAtIndex(i);
                if (item === null || item instanceof MessageDelegate || (item as SystemRow)?.offers === true) {
                    break;
                }
            }
            if (i >= 0 && i !== list.currentIndex) {
                list.currentIndex = i;
            }
        }

        function currentMenu(): void {
            if (list.currentIndex < 0) {
                list.step(1);
            }
            const item = list.currentItem as MessageDelegate;
            item?.openMenu();
            const notice = list.currentItem as SystemRow;
            notice?.openMenu();
        }

        // Enter on the current row: more of its text when it is shown in part; else what a
        // click on its picture, file or voice note does; on a row that has none (or whose
        // file is on its way), its menu.
        function currentActivate(): void {
            if (list.currentIndex < 0) {
                list.step(1);
            }
            const item = list.currentItem as MessageDelegate;
            if (item === null || !(item.readMore() || item.activateMedia())) {
                list.currentMenu();
            }
        }

        // "Read more": the text of a row is about to grow. The view stops following the newest
        // message and holds on to the row at its top, before the text changes, so the line
        // where the text was cut stays where it is and the new text continues below it.
        function readMore(message: real): void {
            root.followNewest = false;
            list.hold();
            App.timeline.readMore(message);
        }

        // Ctrl+Enter: to the message the current row quotes.
        function currentQuote(): void {
            const item = list.currentItem as MessageDelegate;
            item?.jumpToQuote();
        }

        function enterPressed(event: var): void {
            if (event.modifiers & Qt.ControlModifier) {
                list.currentQuote();
            } else {
                list.currentActivate();
            }
        }

        Keys.onUpPressed: list.step(1)
        Keys.onDownPressed: list.step(-1)
        Keys.onMenuPressed: list.currentMenu()
        // The rows hold no tab stops of their own: Enter acts on the current row.
        Keys.onReturnPressed: event => list.enterPressed(event)
        Keys.onEnterPressed: event => list.enterPressed(event)
        Keys.onSpacePressed: list.currentActivate()
        Keys.onEscapePressed: root.composerRequested()
        Keys.onPressed: event => {
            if (event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
                list.currentMenu();
                event.accepted = true;
            }
        }
        // Tab (or Shift+Tab) brought the keyboard here: its release arrives at the list, and
        // the newest message becomes current, wherever in the list the focus landed.
        Keys.onReleased: event => {
            if ((event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) && list.currentIndex < 0) {
                list.moveCurrent(1);
            }
        }

        // Rows came, went or changed (an arrival, an own send, a page at either end, a
        // message that disappeared, a receipt or a reaction): told here before the list
        // lays the change out, so the view is back in place in the same frame. (The model
        // replaces all its rows only when none of them stays, `diff::diff_keeping`: a list
        // that is reset starts again at its newest row.)
        Connections {
            target: list.model
            function onRowsInserted(parent: var, first: int, last: int): void {
                list.keep();
            }
            function onRowsRemoved(parent: var, first: int, last: int): void {
                list.keep();
            }
            function onDataChanged(): void {
                list.keep();
            }
            function onModelReset(): void {
                list.keep();
            }
        }
        // The held row moved in the content while the view stood still.
        Connections {
            target: list.held
            function onYChanged(): void {
                list.keep();
            }
        }

        delegate: DelegateChooser {
            role: "kind"
            DelegateChoice {
                roleValue: "message"
                MessageDelegate {}
            }
            DelegateChoice {
                roleValue: "date"
                DateHeader {}
            }
            DelegateChoice {
                roleValue: "unread"
                UnreadMarker {}
            }
            DelegateChoice {
                roleValue: "system"
                SystemRow {}
            }
            DelegateChoice {
                roleValue: "loading"
                LoadingRow {}
            }
        }
    }

    // The popups come after the list: a ScrollView takes its first child as the view to scroll.
    MessageMenu {
        id: menu
    }
    // (Made on first use, with this view as its parent: see `pickerPopup`.)
    Component {
        id: pickerComponent
        ReactionPicker {
            id: picker
            parent: root
            margins: Kirigami.Units.smallSpacing
            // The whole picker is taller: it may have to move to the other side of the message.
            onImplicitHeightChanged: {
                if (picker.visible) {
                    root.placePicker();
                }
            }
        }
    }
    ReactionsPopup {
        id: whoReacted
        parent: root
        margins: Kirigami.Units.smallSpacing
        // Its height follows the people listed, known only once the row is set.
        onImplicitHeightChanged: {
            if (whoReacted.visible) {
                root.placeReactions();
            }
        }
    }
    Component {
        id: deleteComponent
        DeleteDialog {
            onClosed: destroy()
        }
    }
    Component {
        id: historyComponent
        RevisionsDialog {
            onClosed: destroy()
        }
    }
    Component {
        id: attachmentComponent
        AttachmentDeleteDialog {
            onClosed: destroy()
        }
    }

    Connections {
        target: TimelineActions
        enabled: root.visible
        function onMenuRequested(anchor: Item, row: var, x: real, y: real): void {
            root.openMenu(anchor, row, x, y);
        }
        function onPickerRequested(anchor: Item, row: var): void {
            root.openPicker(anchor, row);
        }
        function onReactionsRequested(anchor: Item, row: var): void {
            root.openReactions(anchor, row);
        }
        function onDeleteRequested(row: var): void {
            root.openDelete(row);
        }
        function onHistoryRequested(row: var): void {
            root.openHistory(row);
        }
        function onActivateRequested(message: real): void {
            root.delegateFor(message)?.activateMedia();
        }
        function onQuoteRequested(message: real): void {
            root.delegateFor(message)?.jumpToQuote();
        }
        function onAttachmentDeleteRequested(row: var): void {
            root.openAttachmentDelete(row);
        }
        function onReadMoreRequested(message: real): void {
            list.readMore(message);
        }
    }

    // A quote was activated and its original is among the rows now: scroll to it, light it up.
    Connections {
        // (Through the view's `model`: the lint does not know the model class is a QObject.)
        target: list.model
        function onJumpRequested(message: real, row: int): void {
            // Rows settle their height after the jump: the view must not snap back to the
            // newest message meanwhile.
            root.followNewest = false;
            list.toRow(row);
            jumpSettle.row = row;
            jumpSettle.restart();
            TimelineActions.flash(message);
        }
    }
    Timer {
        id: jumpSettle
        property int row: -1
        interval: 120
        onTriggered: {
            if (jumpSettle.row >= 0 && jumpSettle.row < list.count && !list.moving) {
                list.toRow(jumpSettle.row);
            }
            // (At the newest message the view follows it.)
            list.rest();
        }
    }

    // Fetch older pages while the top is less than a screen away (the source keeps one page
    // in flight, so repeats are free); the same at the bottom of a window that ends before the
    // newest message.
    Timer {
        interval: 150
        repeat: true
        triggeredOnStart: true
        running: root.visible && list.count > 0 && App.timeline.hasOlder && root.fromTop < list.height
        onTriggered: App.fetchOlder()
    }
    Timer {
        interval: 150
        repeat: true
        triggeredOnStart: true
        running: root.visible && list.count > 0 && App.timeline.hasNewer && root.fromBottom < list.height
        onTriggered: App.fetchNewer()
    }

    Components.FloatingButton {
        id: toLatest

        // Over the view, not in it: a ScrollView puts what is declared inside it into the
        // list's content, where the button would scroll away with the newest message, out of
        // sight exactly when it is needed.
        parent: root
        z: 1
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.margins: Kirigami.Units.largeSpacing
        // Clear of the scroll bar.
        anchors.rightMargin: Kirigami.Units.largeSpacing + (root.QQC2.ScrollBar.vertical && root.QQC2.ScrollBar.vertical.visible ? root.QQC2.ScrollBar.vertical.width : 0)
        // A good way from the newest message (not for a line or two), or the window ends
        // before it (a quote jumped back).
        visible: root.fromBottom > Kirigami.Units.gridUnit * 6 || App.timeline.hasNewer
        icon.name: "go-down"
        text: L10n.i18nc("@action:button", "Jump to Latest")
        onClicked: {
            root.followNewest = true;
            list.held = null;
            if (App.timeline.hasNewer) {
                // The newest page replaces the window; the view follows it.
                App.jumpToLatest();
            } else {
                list.toNewest();
            }
        }
    }
}
