pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// ONE instance; chatModel switches conversations (§8.3).
Kirigami.Page {
    id: page
    required property TimelineModel chatModel
    required property ComposerController composerModel
    // Which pane this page is (0: the first chat, 1: the one beside it) and whether it is the active one.
    property int pane: 0
    property bool active: true
    // Split view layout (the window sets them, `MainWindow.splitLayout`): both chats stand side by side
    // (`beside`; the first one has the width `columnWidth` and a divider to drag), or the split shows this chat
    // alone (`single`: the header offers the other one), and the other chat's name for that button.
    property bool beside: false
    property real columnWidth: 0
    property bool single: false
    property string otherName: ""
    // The divider ended a drag: the first column's width.
    signal dividerReleased(real width)

    readonly property bool open: page.chatModel.conversationId !== 0
    // The conversation has a disappearing-message timer, or offers one to set.
    readonly property bool hasTimer: page.chatModel.timerSecs > 0
    readonly property bool timerOffered: page.chatModel.timerOptions.length > 0
    // The conversation takes timers besides the ones its menu lists.
    readonly property bool timerCustom: page.chatModel.timerCustomMax > 0

    // ---- the details panel (people pages P1; architecture §8.3) ----
    // The panel is made when it is first opened (a `Loader`), stands on the page's trailing edge
    // and is either BESIDE the timeline (the timeline keeps at least 20 grid units) or, when
    // there is no room for that, over the whole chat page. In a narrow window the chat page is
    // the window, so "over the chat" is "alone". One rule, no change of the page row.
    property bool panelOpen: false
    readonly property real panelMinWidth: Kirigami.Units.gridUnit * 16
    readonly property real panelMaxWidth: Kirigami.Units.gridUnit * 22
    readonly property real timelineMinWidth: Kirigami.Units.gridUnit * 20
    readonly property bool panelBeside: page.width - page.panelMinWidth >= page.timelineMinWidth
    readonly property real panelWidth: page.panelBeside ? Math.min(page.panelMaxWidth, page.width - page.timelineMinWidth) : page.width
    // The panel hides the chat (the read markers and notifications follow what is on screen).
    readonly property bool panelCovers: page.panelOpen && !page.panelBeside
    // The width the panel takes from the timeline, the composer and the drop area, on the
    // panel's side (the trailing edge: anchors mirror by themselves, the composer's padding and
    // the Loader's `x` do not).
    readonly property real panelReserved: page.panelOpen && page.panelBeside ? page.panelWidth : 0
    // The panel's item, once it was made (tests read it).
    readonly property DetailsPanel panel: panelLoader.item as DetailsPanel
    // The header's clickable items (Kirigami makes one for the page's own toolbar and one for the
    // global toolbar): the keyboard returns to the one on screen.
    property var headerItems: []
    // The header opens the details panel: a 1:1 chat (and Note to Self) shows the person, a
    // group the group. (A channel or a broadcast list has no page: its header stays a title.)
    readonly property bool detailsShown: page.open && page.chatModel.hasDetails
    // A 1:1 chat (and Note to Self) has a person to show.
    readonly property bool personShown: page.open && !page.chatModel.isGroup

    // What a screen reader says for the header: the chat's name WITHOUT WhatsApp's "~" mark
    // (`Style.unmarked`: the mark is for labels, a screen reader would say "tilde"; W6a2 PRIV-1,
    // G1), and "Details of <name>" while the header opens the details. A function of its inputs
    // so `qml_smoke` (`checkMarkedNames`) can hand it a marked name.
    function headerName(title: string, details: bool): string {
        const name = Style.unmarked(title);
        return details ? L10n.i18nc("@action:button the chat's header opens the page of the person or the group, %1 its name", "Details of %1", name) : name;
    }

    // The header item that is on screen now, or null.
    function shownHeader(): Item {
        const w = page.Window.window;
        for (const item of page.headerItems) {
            if (!item || !item.visible || !w) {
                continue;
            }
            const p = item.mapToItem(null, item.width / 2, item.height / 2);
            if (p.x >= 0 && p.x < w.width && p.y >= 0 && p.y < w.height) {
                return item;
            }
        }
        return null;
    }

    // Opens the panel on the page of the open chat: the group's, or the person's.
    function openDetails(): void {
        if (!page.detailsShown) {
            return;
        }
        const group = page.chatModel.isGroup;
        if (!(group ? App.showGroupOfChat() : App.showPersonOfChat())) {
            return;
        }
        page.panelOpener = "";
        panelLoader.active = true;
        // The panel item is the same for every open: nothing of the last one stays.
        page.panel?.reset();
        page.panel?.start(group ? "group" : "person");
        page.panelOpen = true;
        page.panel?.forceActiveFocus();
    }

    // What opened the panel while it was closed, where the keyboard goes back when it closes: ""
    // (the header: the header gets it), "composer" (a click on a sender or a mention in the
    // timeline), "row" (the message menu of a row: the row keeps the keyboard, its current row
    // is kept while the page is open).
    property string panelOpener: ""

    // A sender's name or picture, or a mention, in the timeline: that person's page. With the panel
    // closed the person is its only level (Back and the X close it); with the panel open the page
    // STACKS on what it shows (a person already on top is replaced), so Back returns to the group's
    // page or the members level as they were, search and place kept (`DetailsPanel.pushPerson`).
    function openSender(user: string): void {
        if (!page.open || !App.showPersonInChat(user)) {
            return;
        }
        panelLoader.active = true;
        if (page.panelOpen && page.panel) {
            page.panel.pushPerson();
            return;
        }
        page.panel?.reset();
        page.panel?.start("person");
        page.panelOpener = timelineView.messageMenu.visible ? "row" : "composer";
        timelineView.keepCurrent = page.panelOpener === "row";
        page.panelOpen = true;
        page.panel?.forceActiveFocus();
    }

    // The person `user`'s page as a sender's (the stacking rule of `openSender`) with the level of their safety number
    // on top (P5a: the message menu's entry of a paused send, BQ-3).
    function openSafety(user: string): void {
        page.openSender(user);
        // (Her page was asked for just now: the level opens when it has come, and only if it is hers.)
        if (page.panelOpen && page.panel && page.panel.level === "person") {
            page.panel.safetyWanted = user;
        }
    }

    // Closes the panel at any level.
    function closeDetails(returnFocus: bool): void {
        if (!page.panelOpen) {
            return;
        }
        page.panelOpen = false;
        page.panel?.reset();
        App.closePerson();
        App.closeGroup();
        // (The gallery slots of the pages and of the level of all media: nothing is asked about
        // once no page is on screen.)
        App.closeChatMedia("all");
        const opener = page.panelOpener;
        page.panelOpener = "";
        timelineView.keepCurrent = false;
        if (returnFocus) {
            if (opener === "row") {
                timelineView.rowList.forceActiveFocus();
            } else if (opener === "composer") {
                composer.focusInput();
            } else {
                page.shownHeader()?.forceActiveFocus();
            }
        } else if (opener === "row") {
            timelineView.rowList.dropCurrent();
        }
    }

    function toggleDetails(): void {
        if (page.panelOpen) {
            page.closeDetails(true);
        } else {
            page.openDetails();
        }
    }

    // (The person page's first names, kept as they are: the fixture page and the tests of the
    // person page call them.)
    function openPerson(): void {
        page.openDetails();
    }
    function closePerson(returnFocus: bool): void {
        page.closeDetails(returnFocus);
    }
    function togglePerson(): void {
        page.toggleDetails();
    }

    // Back (the toolbar's arrow, Alt+Left) is the panel's while it is open: it goes one level
    // back (the page of all groups to the person's, a member's page to the group's), and at the
    // bottom level the panel closes.
    onBackRequested: event => {
        if (page.panelOpen) {
            event.accepted = true;
            if (!(page.panel?.back() ?? false)) {
                page.closeDetails(true);
            }
        }
    }

    Connections {
        target: App
        function onPersonEnded(): void {
            page.closeDetails(false);
        }
        function onGroupEnded(): void {
            page.closeDetails(false);
        }
    }
    // Another chat opens (list, notification, quick switch, step keys, a group in common) or
    // this one is gone: the panel was about the person or the group of the chat it was opened
    // from.
    readonly property real openedChat: page.chatModel.conversationId
    onOpenedChatChanged: page.closeDetails(false)

    // The disappearing-message menu, below the header at the page's trailing edge.
    function openTimerMenu(): void {
        timerMenu.popup(page, Math.max(0, page.width - timerMenu.width - Kirigami.Units.largeSpacing), Kirigami.Units.smallSpacing);
    }

    // The dialogs of the chat's own actions, created when asked for (tests read the last one).
    property TimerDialog timerDialog: null
    property ChatDeleteDialog deleteDialog: null

    // A custom disappearing-message time, within the range the conversation states.
    function openCustomTimer(): void {
        if (!page.timerCustom) {
            return;
        }
        page.timerDialog = timerComponent.createObject(page, {
            "min": page.chatModel.timerCustomMin,
            "max": page.chatModel.timerCustomMax,
            "step": page.chatModel.timerCustomStep,
            "current": page.chatModel.timerSecs
        }) as TimerDialog;
        page.timerDialog?.open();
    }

    // The same for another chat's timer (a page of the details panel names its own chat and its
    // range, which need not be the open chat's: a member's 1:1 chat opened from a group).
    function openCustomTimerFor(conversation: real, min: int, max: int, step: int, current: int): void {
        if (conversation <= 0 || max <= 0) {
            return;
        }
        page.timerDialog = timerComponent.createObject(page, {
            "conversation": conversation,
            "min": min,
            "max": max,
            "step": step,
            "current": current
        }) as TimerDialog;
        page.timerDialog?.open();
    }

    // Asks before the chat's messages are deleted for this account.
    function askClear(): void {
        page.deleteDialog = chatDeleteComponent.createObject(page, {
            "remove": false
        }) as ChatDeleteDialog;
        page.deleteDialog?.open();
    }

    // Asks before the chat is deleted for this account.
    function askDelete(): void {
        page.deleteDialog = chatDeleteComponent.createObject(page, {
            "remove": true
        }) as ChatDeleteDialog;
        page.deleteDialog?.open();
    }

    Component {
        id: timerComponent
        TimerDialog {
            onClosed: destroy()
        }
    }
    Component {
        id: chatDeleteComponent
        ChatDeleteDialog {
            chatModel: page.chatModel
            onClosed: destroy()
        }
    }

    title: page.chatModel.title
    padding: 0
    // A split with both chats shown: each column has the width the window gives it (the first its share of the
    // room, a divider to drag; the second the rest: a fill column of Kirigami ignores the list's width and runs
    // past the window); a chat alone fills.
    Kirigami.ColumnView.fillWidth: !page.beside
    Kirigami.ColumnView.preferredWidth: page.beside ? page.columnWidth : 0
    Kirigami.ColumnView.minimumWidth: page.beside ? Kirigami.Units.gridUnit * 20 : 0
    // (A maximum is stated too: the list page does, and a column's width is bounded by it.)
    Kirigami.ColumnView.maximumWidth: page.beside ? Kirigami.Units.gridUnit * 400 : 0
    Kirigami.ColumnView.interactiveResizeEnabled: page.beside && page.pane === 0
    Kirigami.ColumnView.onInteractiveResizingChanged: {
        if (!page.Kirigami.ColumnView.interactiveResizing && page.beside && page.pane === 0) {
            page.dividerReleased(page.width);
            // (A drag writes the column's width itself, which ends the binding: the share is the rule again.)
            page.Kirigami.ColumnView.preferredWidth = Qt.binding(() => page.beside ? page.columnWidth : 0);
        }
    }
    Kirigami.Theme.colorSet: Kirigami.Theme.View
    Kirigami.Theme.inherit: false

    titleDelegate: Item {
        id: headerButton
        visible: page.open
        Layout.fillWidth: true
        implicitWidth: headerRow.implicitWidth
        implicitHeight: headerRow.implicitHeight
        activeFocusOnTab: page.detailsShown
        Accessible.role: Accessible.Button
        Accessible.name: page.headerName(page.chatModel.title, page.detailsShown)
        Accessible.focusable: page.detailsShown
        Accessible.onPressAction: page.toggleDetails()
        objectName: "chatHeader"
        Component.onCompleted: page.headerItems = page.headerItems.concat([headerButton])
        Component.onDestruction: page.headerItems = page.headerItems.filter(i => i !== headerButton)
        Keys.onSpacePressed: event => {
            if (page.detailsShown) {
                page.toggleDetails();
                event.accepted = true;
            }
        }
        Keys.onReturnPressed: event => {
            if (page.detailsShown) {
                page.toggleDetails();
                event.accepted = true;
            }
        }
        // The keypad's Enter is another key than Return.
        Keys.onEnterPressed: event => {
            if (page.detailsShown) {
                page.toggleDetails();
                event.accepted = true;
            }
        }
        HoverHandler {
            enabled: page.detailsShown
            cursorShape: Qt.PointingHandCursor
        }
        TapHandler {
            enabled: page.detailsShown
            onTapped: page.toggleDetails()
        }
        Rectangle {
            anchors.fill: parent
            anchors.margins: -Kirigami.Units.smallSpacing
            visible: headerButton.activeFocus
            color: "transparent"
            radius: Kirigami.Units.cornerRadius
            border.width: 1
            border.color: Kirigami.Theme.highlightColor
        }

        RowLayout {
            id: headerRow
            anchors.fill: parent
            spacing: Kirigami.Units.largeSpacing

            Avatar {
                name: page.chatModel.title
                source: page.chatModel.avatarUrl
                selfChat: page.chatModel.isSelf
                implicitWidth: Kirigami.Units.gridUnit * 1.8
                implicitHeight: Kirigami.Units.gridUnit * 1.8
            }
            ColumnLayout {
                spacing: 0
                Layout.fillWidth: true
                QQC2.Label {
                    id: headerTitle
                    // A marked title is a `RichText` label cut by `headerFit` (`Style.markedName`).
                    readonly property var titleMarks: page.chatModel.titleMarks ?? []
                    readonly property bool marked: headerTitle.titleMarks.length > 0
                    text: headerTitle.marked ? Style.markedName(headerFit.elidedText, page.chatModel.title, headerTitle.titleMarks, Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor) : page.chatModel.title
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    elide: Text.ElideRight
                    textFormat: headerTitle.marked ? Text.RichText : Text.PlainText
                    Accessible.name: page.chatModel.title
                    Layout.fillWidth: true

                    TextMetrics {
                        id: headerFit
                        font: headerTitle.font
                        text: headerTitle.marked ? page.chatModel.title : ""
                        elide: Qt.ElideRight
                        elideWidth: headerTitle.width
                    }
                }
                RowLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Layout.fillWidth: true

                    QQC2.Label {
                        text: page.chatModel.subtitle
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        color: page.chatModel.typing ? Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor) : Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        textFormat: Text.PlainText
                        // As wide as its text when there is room; the marks follow it.
                        Layout.maximumWidth: Math.ceil(implicitWidth)
                        Layout.fillWidth: true
                        Layout.horizontalStretchFactor: 1000
                    }
                    Kirigami.Icon {
                        visible: page.chatModel.muted
                        source: "notifications-disabled"
                        implicitWidth: Kirigami.Units.iconSizes.small
                        implicitHeight: Kirigami.Units.iconSizes.small
                        color: timerLabel.color
                        isMask: true
                        Accessible.name: L10n.i18nc("@info accessible", "Muted")
                    }
                    // The timer mark: messages here disappear after this long.
                    Kirigami.Icon {
                        visible: page.hasTimer
                        source: "chronometer"
                        implicitWidth: Kirigami.Units.iconSizes.small
                        implicitHeight: Kirigami.Units.iconSizes.small
                        color: timerLabel.color
                        isMask: true
                    }
                    QQC2.Label {
                        id: timerLabel
                        visible: page.hasTimer
                        text: page.chatModel.timerLabel
                        font: Kirigami.Theme.smallFont
                        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        textFormat: Text.PlainText
                        Accessible.name: L10n.i18nc("@info accessible", "Messages disappear after %1", page.chatModel.timerLabel)
                    }
                    Item {
                        Layout.fillWidth: true
                        Layout.horizontalStretchFactor: 1
                    }
                }
            }
    
        }
    }

    actions: [
        // A split that shows one chat only (the window is too narrow for two): the other chat, one press away
        // (F6 does the same). `go-next-view`: it reads as "the next view", which is what it is.
        Kirigami.Action {
            id: otherAction

            objectName: "otherChatAction"
            icon.name: "go-next-view"
            text: L10n.i18nc("@action:button", "Show Other Chat")
            displayHint: Kirigami.DisplayHint.IconOnly | Kirigami.DisplayHint.KeepVisible
            visible: page.open && App.split && page.single
            enabled: visible
            tooltip: L10n.i18nc("@info:tooltip, %1 the other chat's name", "Show %1", Style.plain(page.otherName))
            onTriggered: App.activatePane(1 - page.pane)
        },
        Kirigami.Action {
            icon.name: "chronometer"
            text: L10n.i18nc("@action:button", "Disappearing Messages")
            displayHint: Kirigami.DisplayHint.IconOnly
            visible: page.open && page.timerOffered
            // Off while the account's network holds back what is sent (the core refuses the
            // change then); the tooltip says why.
            enabled: visible && page.chatModel.timerHeld === ""
            tooltip: page.chatModel.timerHeld
            onTriggered: page.openTimerMenu()
        },
        // Beside another chat (split view): starts the chooser (the window asks which chat). Not shown
        // while the split is on: the chats are beside each other then.
        Kirigami.Action {
            id: besideAction

            objectName: "besideAction"
            icon.name: "view-split-left-right"
            text: L10n.i18nc("@action:button", "Open Chat Beside")
            // Kept in the header like Swap and Close below (his "Chat header icon"); it and Swap never show together.
            displayHint: Kirigami.DisplayHint.IconOnly | Kirigami.DisplayHint.KeepVisible
            visible: page.open && !App.split
            enabled: visible
            onTriggered: page.chooseBesideAsked()
        },
        // The two chats exchange places (the window does it: drafts and files follow their chats). Swap and Close
        // keep their place in the header (his buttons): a split chat's header is narrow, and the title would take the
        // room and put them into the overflow menu; the timer goes there first.
        Kirigami.Action {
            id: swapAction

            objectName: "swapAction"
            icon.name: "exchange-positions"
            text: L10n.i18nc("@action:button", "Swap Chats")
            displayHint: Kirigami.DisplayHint.IconOnly | Kirigami.DisplayHint.KeepVisible
            visible: page.open && App.split
            enabled: visible
            onTriggered: page.swapChatsAsked()
        },
        // Closes this chat, in both states (one chat: the placeholder shows; two: the other stays).
        Kirigami.Action {
            id: closeAction

            objectName: "closeAction"
            icon.name: "view-close"
            text: L10n.i18nc("@action:button", "Close Chat")
            displayHint: Kirigami.DisplayHint.IconOnly | Kirigami.DisplayHint.KeepVisible
            visible: page.open
            enabled: visible
            onTriggered: page.closeChatAsked(page.pane)
        },
        // Search in this chat only (G1i): the list's search field takes the keyboard with a chip
        // naming the chat. Not shown (not merely greyed) for a protected chat: it offers no search.
        Kirigami.Action {
            icon.name: "edit-find"
            text: L10n.i18nc("@action:inmenu", "Search in Chat")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.chatModel.chatSearchable
            enabled: visible
            // (The window's key is Ctrl+Shift+F; an action's own shortcut would be ambiguous with it.)
            tooltip: L10n.i18nc("@info:tooltip, %1 a keyboard shortcut", "Search in Chat (%1)", "Ctrl+Shift+F")
            onTriggered: page.searchAsked()
        },
        // Keeps this pane and closes the other one.
        Kirigami.Action {
            objectName: "closeSplitAction"
            icon.name: "view-close"
            text: L10n.i18nc("@action:inmenu", "Close Split View")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && App.split
            enabled: visible
            onTriggered: App.closePane(1 - page.pane)
        },
        // What the chat offers of the settings the phone shares (A10d), from the core's offer for
        // the open chat: Mark as Unread, Pin, Mute (with its times), Archive. All in the overflow
        // menu. An entry the chat does not offer is not shown; one the sync switch greys says why
        // in its tooltip and in the line below the entries.
        Kirigami.Action {
            icon.name: "mail-mark-unread"
            text: L10n.i18nc("@action:inmenu", "Mark as Unread")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.shown === true && page.offer.hasUnread !== true
            enabled: page.offer.on === true
            tooltip: page.offer.on === true ? "" : Style.plain(page.offer.why)
            onTriggered: App.setMarkedUnread(page.chatModel.conversationId, true)
        },
        Kirigami.Action {
            id: pinAction

            readonly property bool pinned: page.offer.pinned === true
            icon.name: pinAction.pinned ? "window-unpin" : "pin"
            text: pinAction.pinned ? L10n.i18nc("@action:inmenu", "Unpin") : L10n.i18nc("@action:inmenu", "Pin")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.shown === true
            enabled: page.offer.on === true && (pinAction.pinned || page.offer.pinAtLimit !== true)
            tooltip: pinAction.enabled ? "" : Style.plain(page.offer.on === true ? page.offer.pinWhy : page.offer.why)
            onTriggered: App.setPinned(page.chatModel.conversationId, !pinAction.pinned)
        },
        // Mute with its times (Signal Desktop's order) while the chat can be muted and the sync is
        // on; the greyed flat entry below stands for the off state (a sub-menu's entries do not
        // inherit a parent's `enabled`).
        Kirigami.Action {
            icon.name: "player-volume-muted"
            text: L10n.i18nc("@title:menu", "Mute")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.on === true && page.offer.muteOffered === true && page.offer.muted !== true
            enabled: visible
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "1 Hour")
                onTriggered: App.muteFor(page.chatModel.conversationId, "hour")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "8 Hours")
                onTriggered: App.muteFor(page.chatModel.conversationId, "eight")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "1 Day")
                onTriggered: App.muteFor(page.chatModel.conversationId, "day")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "1 Week")
                onTriggered: App.muteFor(page.chatModel.conversationId, "week")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "Until…")
                onTriggered: page.untilAsked(page.chatModel.conversationId, null)
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "Always")
                onTriggered: App.muteFor(page.chatModel.conversationId, "always")
            }
        },
        Kirigami.Action {
            icon.name: "player-volume-muted"
            text: L10n.i18nc("@action:inmenu", "Mute")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.shown === true && page.offer.on !== true && page.offer.muteOffered === true && page.offer.muted !== true
            enabled: false
            tooltip: Style.plain(page.offer.why)
        },
        // When a mute ends (his answer to question 1): a quiet line above Unmute.
        Kirigami.Action {
            text: page.offer.mutedLabel ? page.offer.mutedLabel : ""
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.muted === true && page.offer.mutedLabel !== undefined && page.offer.mutedLabel !== ""
            enabled: visible
            // An item of 18 grid units with the wrapping label inside (as the row menu's): an
            // unwrapped label would make the menu as wide as the whole sentence (M3).
            displayComponent: Item {
                implicitWidth: Kirigami.Units.gridUnit * 18
                implicitHeight: mutedText.implicitHeight + Kirigami.Units.smallSpacing * 2
                Accessible.role: Accessible.StaticText
                Accessible.name: mutedText.text

                QQC2.Label {
                    id: mutedText

                    anchors.fill: parent
                    anchors.leftMargin: Kirigami.Units.largeSpacing
                    anchors.rightMargin: Kirigami.Units.largeSpacing
                    verticalAlignment: Text.AlignVCenter
                    text: page.offer.mutedLabel ? page.offer.mutedLabel : ""
                    wrapMode: Text.WordWrap
                    textFormat: Text.PlainText
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                }
            }
        },
        Kirigami.Action {
            icon.name: "player-volume"
            text: L10n.i18nc("@action:inmenu", "Unmute")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.shown === true && page.offer.muteOffered === true && page.offer.muted === true
            enabled: page.offer.on === true
            tooltip: page.offer.on === true ? "" : Style.plain(page.offer.why)
            onTriggered: App.unmute(page.chatModel.conversationId)
        },
        Kirigami.Action {
            id: archiveAction

            readonly property bool archived: page.offer.archived === true
            icon.name: archiveAction.archived ? "archive-remove" : "archive-insert"
            text: archiveAction.archived ? L10n.i18nc("@action:inmenu", "Unarchive") : L10n.i18nc("@action:inmenu", "Archive")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.shown === true
            enabled: page.offer.on === true
            tooltip: page.offer.on === true ? "" : Style.plain(page.offer.why)
            onTriggered: archiveAction.archived ? page.unarchiveAsked(page.chatModel.conversationId) : page.archiveAsked(page.chatModel.conversationId)
        },
        // Why the entries above are grey, or why Pin is: right under Archive, as in the row menu
        // (a wrapping label: the sentence is long). Known limit: Kirigami's overflow gives the page
        // no hook when the menu opens, so a screen reader is not told it (the row menu announces it).
        Kirigami.Action {
            text: page.offerLine
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offerLine !== ""
            enabled: visible
            // An item of 18 grid units with the wrapping label inside (as the row menu's): an
            // unwrapped label would make the menu as wide as the whole sentence (M3).
            displayComponent: Item {
                implicitWidth: Kirigami.Units.gridUnit * 18
                implicitHeight: offerText.implicitHeight + Kirigami.Units.smallSpacing * 2
                Accessible.role: Accessible.StaticText
                Accessible.name: offerText.text

                QQC2.Label {
                    id: offerText

                    anchors.fill: parent
                    anchors.leftMargin: Kirigami.Units.largeSpacing
                    anchors.rightMargin: Kirigami.Units.largeSpacing
                    verticalAlignment: Text.AlignVCenter
                    text: page.offerLine
                    wrapMode: Text.WordWrap
                    textFormat: Text.PlainText
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                }
            }
        },
        Kirigami.Action {
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.offer.shown === true
            enabled: visible
            separator: true
        },
        // What is done to the chat as a whole lives in the page's menu; both ask first.
        // While the chat's message history still arrives both wait, and the menu says why
        // (`chatModel.stillFilling`; link feedback L3).
        Kirigami.Action {
            icon.name: "edit-clear-history"
            text: L10n.i18nc("@action:inmenu", "Clear Chat History…")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open
            enabled: page.chatModel.stillFilling === ""
            tooltip: page.chatModel.stillFilling
            onTriggered: page.askClear()
        },
        Kirigami.Action {
            icon.name: "edit-delete"
            text: L10n.i18nc("@action:inmenu", "Delete Chat…")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open
            enabled: page.chatModel.stillFilling === ""
            tooltip: page.chatModel.stillFilling
            onTriggered: page.askDelete()
        },
        Kirigami.Action {
            icon.name: "documentinfo"
            text: page.chatModel.stillFilling
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && page.chatModel.stillFilling !== ""
            enabled: false
        }
    ]

    // The timers this conversation offers; the current one is checked.
    QQC2.Menu {
        id: timerMenu

        // In the window's scene (also on Wayland, and in headless screenshots).
        popupType: QQC2.Popup.Item
        title: L10n.i18nc("@title:menu", "Disappearing Messages")

        QQC2.MenuItem {
            enabled: false
            text: L10n.i18nc("@title:menu", "Messages Disappear After")
        }
        Instantiator {
            model: page.chatModel.timerOptions
            delegate: QQC2.MenuItem {
                id: option

                required property var modelData

                text: option.modelData.label
                checkable: true
                autoExclusive: true
                checked: option.modelData.secs === page.chatModel.timerSecs
                onTriggered: {
                    if (option.modelData.secs !== page.chatModel.timerSecs) {
                        App.setTimer(option.modelData.secs);
                    }
                    // The mark follows what the network confirms, not the click.
                    option.checked = Qt.binding(() => option.modelData.secs === page.chatModel.timerSecs);
                }
            }
            onObjectAdded: (index, object) => timerMenu.insertItem(index + 1, object as QQC2.MenuItem)
            onObjectRemoved: (index, object) => timerMenu.removeItem(object as QQC2.MenuItem)
        }
        // Any other time the network takes (the entries above are the ones its apps name).
        QQC2.MenuSeparator {
            visible: page.timerCustom
            height: visible ? implicitHeight : 0
        }
        QQC2.MenuItem {
            objectName: "timerCustom"
            visible: page.timerCustom
            // Disabled while hidden: the keyboard would reach an invisible item otherwise.
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "chronometer"
            text: L10n.i18nc("@action:inmenu", "Custom Time…")
            onTriggered: page.openCustomTimer()
        }
    }

    background: Rectangle {
        color: Kirigami.Theme.backgroundColor

        // The drawings behind the messages, in the color of the chat's account (Settings →
        // General). One tile per color, made the first time it is asked for: not at the start.
        Image {
            readonly property string tint: page.chatModel.tint

            anchors.fill: parent
            visible: page.open && App.settings.chatWallpaper && !Style.highContrast && tint !== ""
            source: visible ? "image://kourierart/doodle/" + tint.slice(1) : ""
            sourceSize: Qt.size(Style.doodleSide, Style.doodleSide)
            fillMode: Image.Tile
            horizontalAlignment: Image.AlignLeft
            verticalAlignment: Image.AlignTop
            opacity: visible ? Style.tintStrength(tint, Kirigami.Theme.backgroundColor, 0.12, 0.6) : 0
        }
    }

    // "Focus decides": a press anywhere in this pane (the page with its composer, and its header in the
    // window's toolbar) makes it the active one. The handlers are passive (`PointHandler` takes only a
    // passive grab), so the press still reaches what is under it; activating moves no focus and no scroll.
    component PaneTouch: PointHandler {
        acceptedButtons: Qt.AllButtons
        onActiveChanged: {
            if (active && !page.active) {
                App.activatePane(page.pane);
            }
        }
    }
    PaneTouch {
        parent: page
    }
    PaneTouch {
        parent: page.globalToolBarItem
    }
    // The inactive pane's header (title and actions) is drawn at 70 % opacity: a veil of the header's own
    // background colour at 30 % over it. (The header's `opacity` would let the timeline that scrolls under
    // the toolbar show through.)
    readonly property alias headerDim: headerDim
    Rectangle {
        id: headerDim
        readonly property Item header: page.globalToolBarItem
        parent: header
        z: 1000
        visible: header !== null && !page.active
        width: header ? header.width : 0
        height: header ? header.height : 0
        color: header ? Qt.alpha(header.Kirigami.Theme.backgroundColor, 0.3) : "transparent"
    }

    // `--page=history`: keep the view at the oldest message while older pages load.
    property alias pinToOldest: timelineView.pinToOldest
    // The timeline's message popups (the fixture pages open them).
    property alias timeline: timelineView
    // The composer (tests drive it).
    readonly property alias composer: composer

    TimelineView {
        id: timelineView
        chatModel: page.chatModel
        composerModel: page.composerModel
        pane: page.pane
        anchors.fill: parent
        anchors.rightMargin: page.panelReserved
        // A covering panel takes the timeline out of the keyboard's reach (Tab, Shift+Tab).
        enabled: !page.panelCovers
        visible: page.open
        onComposerRequested: composer.focusInput()
        onPersonRequested: user => page.openSender(user)
        onSafetyRequested: user => page.openSafety(user)
    }

    ChatEmptyState {
        anchors.centerIn: parent
        width: parent.width - Kirigami.Units.gridUnit * 4
        visible: !page.open
    }

    // The details panel: made when first asked for; nothing of it exists at the start. It stands
    // in the page itself (over the composer's strip too) so its height is the page's.
    Loader {
        id: panelLoader
        parent: page
        active: false
        z: 10
        visible: page.panelOpen
        // (Not anchors: the Loader is reparented to the page after it is made, and an anchor
        // to the page before that is refused. An explicit `x` does not mirror: the panel stands
        // on the trailing edge, the left one when the layout is mirrored.)
        x: page.LayoutMirroring.enabled ? 0 : page.width - width
        y: 0
        height: page.height
        width: page.panelWidth
        sourceComponent: DetailsPanel {
            chatModel: page.chatModel
            beside: page.panelBeside
            onCloseRequested: page.closeDetails(true)
            onChatRequested: id => page.conversationRequested(id)
            onCustomTimerRequested: (conversation, min, max, step, current) => page.openCustomTimerFor(conversation, min, max, step, current)
        }
    }

    // A chat the panel offers (a group in common, Message) is to be opened.
    signal conversationRequested(real id)
    // The menu asks: Archive, Unarchive or Mute "Until…" for the open chat `conv` (the window does
    // the work: `MainWindow`).
    signal archiveAsked(real conv)
    signal unarchiveAsked(real conv)
    signal untilAsked(real conv, var back)
    // The menu's "Search in Chat": the window brings the list's search field forward.
    signal searchAsked
    // The header's "Open Chat Beside": the window starts its chooser.
    signal chooseBesideAsked
    // The header's "Swap Chats" and "Close Chat" (pane `pane`): the window does them and moves the keyboard.
    signal swapChatsAsked
    signal closeChatAsked(int pane)

    // What the open chat offers (the core's offer, a map; empty while no chat is open) and the
    // line under the menu's entries: why they are grey, else why Pin is.
    // (Read only while a chat is open: until the model has had a header the property is an invalid
    // variant, which QML must never read.)
    readonly property var offer: page.open ? page.chatModel.chatOffer : ({})
    readonly property string offerLine: page.offer.why ? page.offer.why : (page.offer.pinAtLimit === true && page.offer.pinned !== true && page.offer.pinWhy ? page.offer.pinWhy : "")

    // Files dragged in (Dolphin, the desktop) are attached to the message being written.
    DropArea {
        id: drop
        anchors.fill: parent
        // Not over the panel: a drop there would go to the composer under or beside it.
        anchors.rightMargin: page.panelReserved
        enabled: page.open && !page.panelCovers && composer.canAttach && !composer.blocked && !composer.editing
        keys: ["text/uri-list"]
        onDropped: event => {
            composer.attach(event.urls);
            event.accept(Qt.CopyAction);
        }

        Rectangle {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.largeSpacing
            visible: drop.containsDrag
            radius: Kirigami.Units.cornerRadius * 2
            color: Qt.alpha(Kirigami.Theme.highlightColor, 0.12)
            border.width: 2
            border.color: Kirigami.Theme.highlightColor
            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                icon.name: "mail-attachment"
                text: L10n.i18nc("@info:placeholder", "Drop files to attach them")
            }
        }
    }

    // S5: messages dragged from the OTHER chat of a split (`MessageDragProxy`) stage a forward into this
    // chat's composer: the card shows there, Enter sends it, its X takes it back. A drag from this chat's own
    // pane is not taken. The chat becomes the active one and its composer gets the keyboard.
    DropArea {
        id: forwardDrop
        objectName: "forwardDrop"

        anchors.fill: parent
        anchors.rightMargin: page.panelReserved
        enabled: page.open && !page.panelCovers && !composer.blocked && !composer.editing
        keys: ["kourier/messages"]
        onEntered: drag => {
            const from = drag.source as MessageDragProxy;
            drag.accepted = from !== null && from.fromPane !== page.pane && from.messages.length > 0;
        }
        onDropped: drop => {
            const from = drop.source as MessageDragProxy;
            if (from === null || from.fromPane === page.pane) {
                return;
            }
            const count = from.messages.length;
            if (!page.composerModel.stageForward(page.chatModel.conversationId, from.fromConversation, from.messages)) {
                page.forwardRefused(count, page.chatModel.title);
                return;
            }
            drop.accept(Qt.CopyAction);
            if (from.fromModel !== null && from.fromModel.selecting) {
                from.fromModel.selectClear();
            }
            App.activatePane(page.pane);
            Qt.callLater(() => composer.focusInput());
        }

        Rectangle {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.largeSpacing
            visible: forwardDrop.containsDrag
            radius: Kirigami.Units.cornerRadius * 2
            color: Qt.alpha(Kirigami.Theme.highlightColor, 0.12)
            border.width: 2
            border.color: Kirigami.Theme.highlightColor
            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                icon.name: "mail-forward"
                text: (forwardDrop.drag.source as MessageDragProxy)?.messages.length > 1 ? L10n.i18nc("@info:placeholder", "Drop the messages to forward them to this chat") : L10n.i18nc("@info:placeholder", "Drop the message to forward it to this chat")
            }
        }
    }
    // A drop of `count` messages that this chat (titled `chat`) could not take: the forward could not be staged.
    signal forwardRefused(int count, string chat)

    footer: Composer {
        id: composer
        chatModel: page.chatModel
        composerModel: page.composerModel
        active: page.active
        visible: page.open
        // A covering panel takes the composer out of the keyboard's reach: nothing typed lands in it.
        enabled: !page.panelCovers
        // Beside the panel the composer stops where the timeline does.
        rightReserved: page.panelReserved
        // The note "Already sent" -> Delete for Everyone: the question of the message menu. The
        // note goes once the question is up (a row that is not on screen keeps the note).
        onDeleteNoteRequested: message => {
            if (timelineView.deleteFor(message)) {
                page.composerModel.clearNotice();
            }
        }
    }
}
