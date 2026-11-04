pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The person's page of the details panel (people pages P1; architecture §8.3): picture, name,
// about text and number; then Message, Disappearing Messages, View Safety Number (P5a), Copy Link (P5b),
// Share Contact… (P5c) and Groups in Common, in Signal Desktop's order. A part with nothing in it is left out, and a number the person does not share
// has no line. The page lives in `App.person` (a map from `PersonView`). It is a level of the
// panel's stack: the panel owns the plate, the close button and the keys, and keeps this item
// alive (hidden) while a page is open on top of it. Every text from the network is plain text.
Item {
    id: root

    readonly property var person: App.person
    // The page has arrived (an empty map before).
    readonly property bool loaded: root.person.name !== undefined
    // The page of the groups in common shows instead of the person's (after "Show All").
    property bool allGroups: false
    // What opened the level on top of this page, so the keyboard can return to it: "media" or "safety".
    property string opener: ""
    // The panel sits beside the timeline (a line on its leading edge), or covers the chat.
    property bool beside: false
    // Where the vertical scroll bar may start (below the panel's close button; set by the panel).
    property real barTop: 0
    // The page is not the panel's bottom level (a member's page opened from a group): it shows
    // its own back arrow in a header row (for the mouse in the wide layout; in the narrow one the toolbar's arrow also shows, and both go one level back).
    property bool stacked: false
    // The arrow was clicked: the panel goes one level back.
    signal backRequested
    // How many times "Copy Link" put a link on the clipboard (P5b; read by `chat_ui`).
    property int linksCopied: 0
    // What the last copy told a screen reader and how (Z4: the toast has no announcement of its own, and the focus stays on
    // the button, so nothing else says the copy happened). Read by `chat_ui`.
    property string linkAnnounced: ""
    property int linkAnnouncedPoliteness: -1
    // The share picker while it is open (P5c; read by `chat_ui`), and how many cards it handed to Rust.
    property ShareContactDialog shareDialog: null
    property int cardsSent: 0
    // The person and the account the open picker was made for (SE2): a page answer for another person closes the
    // picker (its title was copied at the open, and Rust sends the card of whoever the page holds now).
    property string shareUser: ""
    property real shareAccount: 0
    // What the last card's toast told a screen reader (as `linkAnnounced`).
    property string shareAnnounced: ""

    // A time chosen in the selector as its popup chooses one, not applied (the fixture page `timerpending`).
    function chooseTime(secs: int): bool {
        return timerPart.visible && timerPart.chooseTime(secs);
    }

    // "Share Contact…" (P5c): the picker of the account's chats over the window; false when the page offers no card
    // (Rust then fills no rows). One picker at a time.
    function openShare(): bool {
        if (root.shareDialog !== null || !shareButton.visible || !shareButton.enabled || !App.prepareShare()) {
            return false;
        }
        root.shareUser = root.person.user ?? "";
        root.shareAccount = root.person.account ?? 0;
        root.shareDialog = shareComponent.createObject(root, {
            "personName": root.person.name ?? ""
        }) as ShareContactDialog;
        root.shareDialog?.open();
        return root.shareDialog !== null;
    }

    // The card left for the chat's queue: the toast names the CHAT (never the number), politely told to a screen reader
    // too. It says "Sending", not "sent": it appears when the command left, before the core's checks (a refusal still
    // shows its own notice, SD11).
    function cardSent(title: string): void {
        const told = L10n.i18nc("@info:status toast after a contact card was handed to the core, %1 the chat's name", "Sending the contact card to %1", title);
        root.cardsSent += 1;
        root.shareAnnounced = told;
        (Kirigami.ApplicationWindow.window as Kirigami.ApplicationWindow)?.showPassiveNotification(told, "short");
        shareButton.Accessible.announce(told, Accessible.Polite);
    }

    Component {
        id: shareComponent
        ShareContactDialog {
            // What the card holds follows the page (the same person's refresh keeps the picker, SE2).
            cardLine: root.person.shareCard ?? ""
            onSent: title => root.cardSent(title)
            onClosed: {
                root.shareDialog = null;
                // The keyboard returns to the button (the panel stays).
                if (shareButton.visible && root.visible) {
                    shareButton.forceActiveFocus();
                }
                destroy();
            }
        }
    }

    // Z4: tells a screen reader the toast's own words, politely (a confirmation needs no interruption), and records
    // what was told for the tests. Kirigami's passive notification has no `Accessible` announcement.
    function announceCopy(text: string): void {
        root.linkAnnounced = text;
        root.linkAnnouncedPoliteness = Accessible.Polite;
        linkButton.Accessible.announce(text, Accessible.Polite);
    }

    // "Share Contact…" stands on the page (the network sends cards, not her own page: BQ-4; not the groups' level).
    readonly property bool shareOffered: root.loaded && (root.person.shareShown ?? false) && !root.allGroups
    // Copy Link and Share Contact… stand together, and for a person whose number the page cannot use both say the SAME
    // sentence: it is said once, under the pair (the second button's line); the first button keeps it as its accessible
    // description (SD11).
    readonly property bool oneNote: root.shareOffered && (root.person.linkNote ?? "") !== "" && (root.person.linkNote ?? "") === (root.person.shareNote ?? "")

    // Message opens her chat, unless it is the open one; her timer is her chat's, open or not
    // (the viewmodel offers none for a chat the user deleted).
    readonly property bool timerShown: root.loaded && root.person.chat !== 0 && root.person.timers.length > 0
    readonly property bool messageShown: root.loaded && root.person.chat !== 0 && root.person.chat !== App.timeline.conversationId
    readonly property bool groupsShown: root.loaded && root.person.groups.length > 0
    // The groups in common as the rows were last made from. `App.person` is a new map at every
    // refresh (a profile, a contact, a group changing), and a model that is a new array each time
    // makes the Repeater throw its rows away: a press on a row, and the keyboard's focus in it,
    // would be lost. The rows are rebuilt only when what they show changed.
    property var groupRows: []
    function syncGroups(): void {
        const next = root.person.groups ?? [];
        if (JSON.stringify(next) !== JSON.stringify(root.groupRows)) {
            root.groupRows = next;
        }
    }
    onPersonChanged: {
        root.syncGroups();
        // A page that no longer offers a card (its state changed under the picker), or the page of ANOTHER person or
        // account (SE2: a refresh of the same person keeps it), closes the picker.
        if (root.shareDialog !== null && (!((root.person.shareShown ?? false) && (root.person.shareNote ?? "") === "") || (root.person.user ?? "") !== root.shareUser || (root.person.account ?? 0) !== root.shareAccount)) {
            root.shareDialog.close();
        }
    }
    Component.onCompleted: root.syncGroups()

    // "All Media and Files" was chosen: the level of all media opens on top of this page for
    // `conversation` (her chat) of `account`.
    signal mediaRequested(real account, real conversation)
    // "View Safety Number" was chosen: the level of the person's safety number opens on top of this page (P5a).
    signal safetyRequested
    // A group row was chosen (its chat opens), or Message was.
    signal chatRequested(real id)
    // "Custom Time…" of her chat's timer was chosen.
    signal customTimerRequested(real conversation, int min, int max, int step, int current)

    // What belongs to one person alone ("Show All", the opened about text, the place in the
    // page) is forgotten whenever the panel is opened, closed or left for another person: the
    // same item serves every later page.
    function reset(): void {
        root.shareDialog?.close();
        timerPart.reset();
        root.allGroups = false;
        root.opener = "";
        about.reset();
        root.scrollToTop();
    }

    // The keyboard returns to what opened the level on top of this page (the section's button);
    // false when nothing did (the panel takes it).
    function focusOpener(): bool {
        if (root.opener === "media" && mediaPart.allButton.visible) {
            mediaPart.allButton.forceActiveFocus();
            return true;
        }
        if (root.opener === "safety" && safetyButton.visible) {
            safetyButton.forceActiveFocus();
            return true;
        }
        return false;
    }

    function scrollToTop(): void {
        const flick = scroll.contentItem as Flickable;
        if (flick) {
            flick.contentY = 0;
        }
    }

    // Back from the page of all groups to the person's; false when it was not open.
    function leaveGroups(): bool {
        if (!root.allGroups) {
            return false;
        }
        root.allGroups = false;
        return true;
    }

    // The header row of a page that is not the panel's bottom level: its own back arrow, in a row
    // above the scroll view (it never lies over the rows). In the narrow layout the toolbar's
    // arrow ALSO shows; both go one level back. The panel's close button stands over the
    // trailing end of this row.
    Item {
        id: header
        objectName: "personHeader"
        visible: root.stacked && !root.allGroups
        anchors.top: parent.top
        anchors.left: parent.left
        anchors.right: parent.right
        height: header.visible ? backArrow.implicitHeight + Kirigami.Units.smallSpacing * 2 : 0
        QQC2.ToolButton {
            id: backArrow
            objectName: "personBack"
            anchors.top: parent.top
            anchors.left: parent.left
            anchors.margins: Kirigami.Units.smallSpacing
            icon.name: LayoutMirroring.enabled ? "go-next" : "go-previous"
            display: QQC2.AbstractButton.IconOnly
            text: L10n.i18nc("@action:button", "Back")
            Accessible.name: text
            onClicked: root.backRequested()
        }
    }

    QQC2.ScrollView {
        id: scroll
        objectName: "detailsScroll"
        anchors.top: header.bottom
        anchors.bottom: parent.bottom
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.leftMargin: root.beside ? 1 : 0
        contentWidth: availableWidth
        QQC2.ScrollBar.horizontal.policy: QQC2.ScrollBar.AlwaysOff
        // The panel's close button stands over the top right corner: the scroll bar starts below it.
        QQC2.ScrollBar.vertical.topPadding: Math.max(0, root.barTop - header.height)
        // The content's width while the scroll bar shows (the style keeps the bar's width beside the content, unless the
        // bar lies over it): the same whether the bar shows or not, for what must not follow the bar (the media squares).
        readonly property real steadyWidth: scroll.width - (scroll.QQC2.ScrollBar.vertical.interactive ? scroll.QQC2.ScrollBar.vertical.width : 0)

        Column {
            id: content
            width: scroll.availableWidth
            spacing: Kirigami.Units.largeSpacing
            topPadding: Kirigami.Units.largeSpacing * 2
            bottomPadding: Kirigami.Units.largeSpacing * 2

            // ---- the person ----
            Column {
                id: head
                visible: root.loaded && !root.allGroups
                width: parent.width - Kirigami.Units.largeSpacing * 2
                x: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Avatar {
                    objectName: "detailsAvatar"
                    anchors.horizontalCenter: parent.horizontalCenter
                    name: root.person.name ?? ""
                    source: root.person.avatarUrl ?? ""
                    implicitWidth: Kirigami.Units.gridUnit * 6
                    implicitHeight: Kirigami.Units.gridUnit * 6
                    width: implicitWidth
                    height: implicitHeight
                }
                QQC2.Label {
                    objectName: "detailsName"
                    width: parent.width
                    horizontalAlignment: Text.AlignHCenter
                    text: root.person.name ?? ""
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold,
                        "pointSize": Kirigami.Theme.defaultFont.pointSize * 1.3
                    })
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
                QQC2.Label {
                    objectName: "detailsProfileName"
                    visible: (root.person.profileName ?? "") !== ""
                    width: parent.width
                    horizontalAlignment: Text.AlignHCenter
                    text: root.person.profileName ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
                // About text: three lines, "More" for the rest; selectable, with links where her chat is accepted.
                DescriptionText {
                    id: about
                    objectName: "detailsAbout"
                    width: parent.width
                    html: root.person.aboutHtml ?? ""
                    text: ((root.person.aboutEmoji ?? "") !== "" ? root.person.aboutEmoji + " " : "") + (root.person.about ?? "")
                    hrefs: root.person.aboutHrefs ?? []
                    accessibleName: L10n.i18nc("@info:accessible a person's about text", "About text")
                }
                QQC2.Button {
                    objectName: "detailsAboutMore"
                    visible: about.visible && (about.truncated || about.expanded)
                    anchors.horizontalCenter: parent.horizontalCenter
                    flat: true
                    text: about.expanded ? L10n.i18nc("@action:button show less of the about text", "Less") : L10n.i18nc("@action:button show the rest of the about text", "More")
                    onClicked: about.expanded = !about.expanded
                }
                QQC2.Label {
                    objectName: "detailsNumber"
                    visible: (root.person.number ?? "") !== ""
                    width: parent.width
                    horizontalAlignment: Text.AlignHCenter
                    text: root.person.number ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    textFormat: Text.PlainText
                }
                Repeater {
                    model: root.person.notes ?? []
                    delegate: QQC2.Label {
                        id: note
                        required property string modelData
                        width: head.width
                        horizontalAlignment: Text.AlignHCenter
                        text: note.modelData
                        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        wrapMode: Text.Wrap
                        textFormat: Text.PlainText
                    }
                }
            }

            // ---- Message ----
            QQC2.Button {
                objectName: "detailsMessage"
                visible: root.messageShown && !root.allGroups
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                icon.name: "mail-message-new"
                text: L10n.i18nc("@action:button open the chat with this person", "Message")
                onClicked: root.chatRequested(root.person.chat)
            }

            // ---- Disappearing Messages: her chat's timer ----
            TimerPart {
                id: timerPart
                objectName: "detailsTimer"
                visible: root.timerShown && !root.allGroups
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                conversation: root.person.chat ?? 0
                timers: root.person.timers ?? []
                timerSecs: root.person.timerSecs ?? 0
                timerLabel: root.person.timer ?? ""
                custom: root.person.timerCustom ?? false
                customMin: root.person.timerMin ?? 0
                customMax: root.person.timerMax ?? 0
                customStep: root.person.timerStep ?? 1
                failed: root.person.timerFailed ?? ""
                boxName: "detailsTimerBox"
                heldName: "detailsTimerHeld"
                onCustomRequested: (conversation, min, max, step, current) => root.customTimerRequested(conversation, min, max, step, current)
            }

            // ---- View Safety Number (P5a: after Disappearing Messages, before Groups in Common, as Signal Desktop) ----
            // Offered when the network can answer for this person (`safetyShown`: not the own page, not a person known by
            // a number alone); a quiet line under it says "Verified" or "Safety number changed".
            Column {
                id: safetyPart
                objectName: "detailsSafetyPart"
                visible: root.loaded && (root.person.safetyShown ?? false) && !root.allGroups
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                spacing: Kirigami.Units.smallSpacing
                QQC2.Button {
                    id: safetyButton
                    objectName: "detailsSafety"
                    width: parent.width
                    flat: true
                    icon.name: "security-high"
                    text: L10n.i18nc("@action:button", "View Safety Number")
                    Accessible.name: text
                    // The quiet line under it is no Tab stop: a screen reader hears the state here (U9).
                    Accessible.description: root.person.safetyState ?? ""
                    Keys.onReturnPressed: event => {
                        safetyButton.clicked();
                        event.accepted = true;
                    }
                    Keys.onEnterPressed: event => {
                        safetyButton.clicked();
                        event.accepted = true;
                    }
                    onClicked: {
                        root.opener = "safety";
                        root.safetyRequested();
                    }
                }
                QQC2.Label {
                    objectName: "detailsSafetyState"
                    visible: text !== ""
                    width: parent.width
                    horizontalAlignment: Text.AlignHCenter
                    text: root.person.safetyState ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
            }

            // ---- Copy Link (P5b: right under View Safety Number, his answer 2) ----
            // Shown when the network has links to people (`linkShown`: never on the own page, BQ-4). Disabled with one
            // quiet line saying why when the page may use no number of theirs. Rust copies the link it holds for this
            // page: the URL never passes through QML; the toast carries no number.
            Column {
                id: linkPart
                objectName: "detailsLinkPart"
                visible: root.loaded && (root.person.linkShown ?? false) && !root.allGroups
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                spacing: Kirigami.Units.smallSpacing
                QQC2.Button {
                    id: linkButton
                    objectName: "detailsLink"
                    width: parent.width
                    flat: true
                    icon.name: "edit-copy"
                    text: L10n.i18nc("@action:button", "Copy Link")
                    enabled: (root.person.linkNote ?? "") === ""
                    Accessible.name: text
                    // The disabled reason (the quiet line under it) for a screen reader.
                    Accessible.description: root.person.linkNote ?? ""
                    Keys.onReturnPressed: event => {
                        linkButton.clicked();
                        event.accepted = true;
                    }
                    Keys.onEnterPressed: event => {
                        linkButton.clicked();
                        event.accepted = true;
                    }
                    onClicked: {
                        if (App.copyPersonLink()) {
                            const told = L10n.i18nc("@info:status toast after Copy Link", "Link copied");
                            root.linksCopied += 1;
                            (Kirigami.ApplicationWindow.window as Kirigami.ApplicationWindow)?.showPassiveNotification(told, "short");
                            root.announceCopy(told);
                        }
                    }
                }
                QQC2.Label {
                    objectName: "detailsLinkNote"
                    visible: text !== "" && !root.oneNote
                    width: parent.width
                    horizontalAlignment: Text.AlignHCenter
                    text: root.person.linkNote ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
            }

            // ---- Share Contact… (P5c: right under Copy Link, his answer 2) ----
            // Shown when the network sends contact cards (`shareShown`: never on the own page, BQ-4). Disabled with one
            // quiet line saying why: no number the page may show, or no profile name (SC1). The picker names only the
            // chat; Rust holds whose card it is and the core builds it.
            Column {
                id: sharePart
                objectName: "detailsSharePart"
                visible: root.shareOffered
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                spacing: Kirigami.Units.smallSpacing
                QQC2.Button {
                    id: shareButton
                    objectName: "detailsShare"
                    width: parent.width
                    flat: true
                    icon.name: "document-share"
                    text: L10n.i18nc("@action:button", "Share Contact…")
                    enabled: (root.person.shareNote ?? "") === ""
                    Accessible.name: text
                    // The disabled reason (the quiet line under it) for a screen reader.
                    Accessible.description: root.person.shareNote ?? ""
                    // A held Return acts once, never its auto-repeats (SE1): after a send the keyboard returns to this
                    // button, and the next repeat of the key would press it and open the picker again. The parameter is
                    // untyped so a test can hand over a stand-in for the key event (`TestEvent` has no auto-repeat).
                    function activateKey(event: var): void {
                        if (event.isAutoRepeat) {
                            event.accepted = true;
                            return;
                        }
                        shareButton.clicked();
                        event.accepted = true;
                    }
                    Keys.onReturnPressed: event => shareButton.activateKey(event)
                    Keys.onEnterPressed: event => shareButton.activateKey(event)
                    onClicked: root.openShare()
                }
                QQC2.Label {
                    objectName: "detailsShareNote"
                    visible: text !== ""
                    width: parent.width
                    horizontalAlignment: Text.AlignHCenter
                    text: root.person.shareNote ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                }
            }

            // ---- Groups in Common ----
            Column {
                id: groupsPart
                objectName: "detailsGroups"
                visible: root.groupsShown
                width: parent.width
                spacing: 0

                RowLayout {
                    x: Kirigami.Units.largeSpacing
                    width: parent.width - Kirigami.Units.largeSpacing * 2
                    QQC2.ToolButton {
                        objectName: "detailsGroupsBack"
                        visible: root.allGroups
                        icon.name: LayoutMirroring.enabled ? "go-next" : "go-previous"
                        display: QQC2.AbstractButton.IconOnly
                        text: L10n.i18nc("@action:button", "Back")
                        Accessible.name: text
                        onClicked: root.leaveGroups()
                    }
                    QQC2.Label {
                        Layout.fillWidth: true
                        text: L10n.i18nc("@title:group", "Groups in Common")
                        font: Style.styled(Kirigami.Theme.defaultFont, {
                            "weight": Font.DemiBold
                        })
                        elide: Text.ElideRight
                        textFormat: Text.PlainText
                    }
                }
                Repeater {
                    model: root.allGroups ? root.groupRows : root.groupRows.slice(0, root.person.groupsPreview ?? 5)
                    delegate: QQC2.ItemDelegate {
                        id: group
                        objectName: "detailsGroup"
                        required property var modelData
                        width: groupsPart.width
                        text: group.modelData.title
                        Accessible.name: group.modelData.title
                        // A row of the keyboard: Tab reaches it, Return, Enter and Space open the
                        // group, and the frame of the keyboard's focus shows which.
                        focusPolicy: Qt.StrongFocus
                        highlighted: group.visualFocus
                        Keys.onReturnPressed: event => {
                            group.clicked();
                            event.accepted = true;
                        }
                        Keys.onEnterPressed: event => {
                            group.clicked();
                            event.accepted = true;
                        }
                        contentItem: RowLayout {
                            spacing: Kirigami.Units.largeSpacing
                            Avatar {
                                name: group.modelData.title
                                source: group.modelData.avatarUrl
                                implicitWidth: Kirigami.Units.gridUnit * 2
                                implicitHeight: Kirigami.Units.gridUnit * 2
                            }
                            QQC2.Label {
                                Layout.fillWidth: true
                                text: group.modelData.title
                                elide: Text.ElideRight
                                textFormat: Text.PlainText
                            }
                        }
                        onClicked: root.chatRequested(group.modelData.id)
                    }
                }
                QQC2.Button {
                    objectName: "detailsShowAll"
                    visible: !root.allGroups && (root.person.groupsMore ?? "") !== ""
                    x: LayoutMirroring.enabled ? parent.width - width - Kirigami.Units.largeSpacing : Kirigami.Units.largeSpacing
                    flat: true
                    text: root.person.groupsMore ?? ""
                    onClicked: root.allGroups = true
                }
            }

            // ---- Media and Files: her chat's pictures and files (LAST, as Signal Desktop) ----
            MediaSection {
                id: mediaPart
                objectName: "personMedia"
                slot: "person"
                // Her 1:1 chat (none, no section); the page of all groups is another state.
                conversation: root.loaded ? (root.person.chat ?? 0) : 0
                account: root.loaded ? (root.person.account ?? 0) : 0
                suppressed: root.allGroups
                width: parent.width
                // (Never the width that follows the scroll bar: see `MediaSection.squaresWidth`.)
                squaresWidth: scroll.steadyWidth
                onShowAllRequested: {
                    root.opener = "media";
                    root.mediaRequested(mediaPart.account, mediaPart.conversation);
                }
            }
        }
    }
}
