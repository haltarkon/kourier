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
