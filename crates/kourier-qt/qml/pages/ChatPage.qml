pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// ONE instance; App.timeline switches conversations (§8.3).
Kirigami.Page {
    id: page

    readonly property bool open: App.timeline.conversationId !== 0
    // The conversation has a disappearing-message timer, or offers one to set.
    readonly property bool hasTimer: App.timeline.timerSecs > 0
    readonly property bool timerOffered: App.timeline.timerOptions.length > 0
    // The conversation takes timers besides the ones its menu lists.
    readonly property bool timerCustom: App.timeline.timerCustomMax > 0

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
    readonly property bool detailsShown: page.open && App.timeline.hasDetails
    // A 1:1 chat (and Note to Self) has a person to show.
    readonly property bool personShown: page.open && !App.timeline.isGroup

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
        const group = App.timeline.isGroup;
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
    readonly property real openedChat: App.timeline.conversationId
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
            "min": App.timeline.timerCustomMin,
            "max": App.timeline.timerCustomMax,
            "step": App.timeline.timerCustomStep,
            "current": App.timeline.timerSecs
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
            onClosed: destroy()
        }
    }

    title: App.timeline.title
    padding: 0
    Kirigami.ColumnView.fillWidth: true
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
        Accessible.name: page.headerName(App.timeline.title, page.detailsShown)
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
                name: App.timeline.title
                source: App.timeline.avatarUrl
                selfChat: App.timeline.isSelf
                implicitWidth: Kirigami.Units.gridUnit * 1.8
                implicitHeight: Kirigami.Units.gridUnit * 1.8
            }
            ColumnLayout {
                spacing: 0
                Layout.fillWidth: true
                QQC2.Label {
                    text: App.timeline.title
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                }
                RowLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Layout.fillWidth: true

                    QQC2.Label {
                        text: App.timeline.subtitle
                        elide: Text.ElideRight
                        font: Kirigami.Theme.smallFont
                        color: App.timeline.typing ? Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor) : Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        textFormat: Text.PlainText
                        // As wide as its text when there is room; the marks follow it.
                        Layout.maximumWidth: Math.ceil(implicitWidth)
                        Layout.fillWidth: true
                        Layout.horizontalStretchFactor: 1000
                    }
                    Kirigami.Icon {
                        visible: App.timeline.muted
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
                        text: App.timeline.timerLabel
                        font: Kirigami.Theme.smallFont
                        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        textFormat: Text.PlainText
                        Accessible.name: L10n.i18nc("@info accessible", "Messages disappear after %1", App.timeline.timerLabel)
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
        Kirigami.Action {
            icon.name: "chronometer"
            text: L10n.i18nc("@action:button", "Disappearing Messages")
            displayHint: Kirigami.DisplayHint.IconOnly
            visible: page.open && page.timerOffered
            // Off while the account's network holds back what is sent (the core refuses the
            // change then); the tooltip says why.
            enabled: visible && App.timeline.timerHeld === ""
            tooltip: App.timeline.timerHeld
            onTriggered: page.openTimerMenu()
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
            onTriggered: App.setMarkedUnread(App.timeline.conversationId, true)
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
            onTriggered: App.setPinned(App.timeline.conversationId, !pinAction.pinned)
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
                onTriggered: App.muteFor(App.timeline.conversationId, "hour")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "8 Hours")
                onTriggered: App.muteFor(App.timeline.conversationId, "eight")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "1 Day")
                onTriggered: App.muteFor(App.timeline.conversationId, "day")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "1 Week")
                onTriggered: App.muteFor(App.timeline.conversationId, "week")
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "Until…")
                onTriggered: page.untilAsked(App.timeline.conversationId, null)
            }
            Kirigami.Action {
                text: L10n.i18nc("@action:inmenu", "Always")
                onTriggered: App.muteFor(App.timeline.conversationId, "always")
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
            onTriggered: App.unmute(App.timeline.conversationId)
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
            onTriggered: archiveAction.archived ? page.unarchiveAsked(App.timeline.conversationId) : page.archiveAsked(App.timeline.conversationId)
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
        // (`App.timeline.stillFilling`; link feedback L3).
        Kirigami.Action {
            icon.name: "edit-clear-history"
            text: L10n.i18nc("@action:inmenu", "Clear Chat History…")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open
            enabled: App.timeline.stillFilling === ""
            tooltip: App.timeline.stillFilling
            onTriggered: page.askClear()
        },
        Kirigami.Action {
            icon.name: "edit-delete"
            text: L10n.i18nc("@action:inmenu", "Delete Chat…")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open
            enabled: App.timeline.stillFilling === ""
            tooltip: App.timeline.stillFilling
            onTriggered: page.askDelete()
        },
        Kirigami.Action {
            icon.name: "documentinfo"
            text: App.timeline.stillFilling
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open && App.timeline.stillFilling !== ""
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
            model: App.timeline.timerOptions
            delegate: QQC2.MenuItem {
                id: option

                required property var modelData

                text: option.modelData.label
                checkable: true
                autoExclusive: true
                checked: option.modelData.secs === App.timeline.timerSecs
                onTriggered: {
                    if (option.modelData.secs !== App.timeline.timerSecs) {
                        App.setTimer(option.modelData.secs);
                    }
                    // The mark follows what the network confirms, not the click.
                    option.checked = Qt.binding(() => option.modelData.secs === App.timeline.timerSecs);
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
            readonly property string tint: App.timeline.tint

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

    // `--page=history`: keep the view at the oldest message while older pages load.
    property alias pinToOldest: timelineView.pinToOldest
    // The timeline's message popups (the fixture pages open them).
    property alias timeline: timelineView
    // The composer (tests drive it).
    readonly property alias composer: composer

    TimelineView {
        id: timelineView
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

    // What the open chat offers (the core's offer, a map; empty while no chat is open) and the
    // line under the menu's entries: why they are grey, else why Pin is.
    // (Read only while a chat is open: until the model has had a header the property is an invalid
    // variant, which QML must never read.)
    readonly property var offer: page.open ? App.timeline.chatOffer : ({})
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

    footer: Composer {
        id: composer
        visible: page.open
        // A covering panel takes the composer out of the keyboard's reach: nothing typed lands in it.
        enabled: !page.panelCovers
        // Beside the panel the composer stops where the timeline does.
        rightReserved: page.panelReserved
        // The note "Already sent" -> Delete for Everyone: the question of the message menu. The
        // note goes once the question is up (a row that is not on screen keeps the note).
        onDeleteNoteRequested: message => {
            if (timelineView.deleteFor(message)) {
                App.composer.clearNotice();
            }
        }
    }
}
