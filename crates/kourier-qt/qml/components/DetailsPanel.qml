pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The details panel of the chat page (people pages P1, P2; architecture §8.3): a small stack of
// pages about the open chat's person or group. A 1:1 chat opens at the person's page
// (`PersonPage`, with its list of all groups in common as a state of its own); a group opens at
// the group's page (`GroupPage`), from which "Show All" opens the members (`MembersPage`) and a
// member's row opens that person's page on top. Both pages end with "Media and Files", whose
// "Show All" opens the level of all media (`MediaPage`) on top of the page it was chosen on. The
// levels are items made with the panel and kept ALIVE while they are in `levels` (hidden when
// another is on top): the place in a page,
// a search text and the keyboard's row survive a page opened on top, and Back shows the page
// as it was, kept current meanwhile. No `StackView` (its transitions would animate, and a popped
// item is destroyed, which loses exactly that place).
//
// The panel owns what every level shares: the plate (opaque, with the pointer events of its whole
// area, so a click or the wheel over a part with nothing in it never reaches the chat under a
// panel that covers it), the close button (it closes the whole panel at any level) and the
// keys: Escape and Back go ONE level back (a search text in the members' field has the first
// Escape), at the bottom level they close the panel. The ChatPage places this item, beside the
// timeline or over the chat. Every text from the network is plain text.
FocusScope {
    id: root

    readonly property var person: App.person
    readonly property var group: App.group
    // The levels, bottom to top: "group", "members", "person", "media", "safety" (empty: the panel is closed).
    property var levels: []
    // The level on top (not `top`: Item has a FINAL member of that name).
    readonly property string level: root.levels.length > 0 ? root.levels[root.levels.length - 1] : ""
    // The page on top has arrived (each level's own flag: an empty map before).
    readonly property bool loaded: root.level === "person" ? personPage.loaded : (root.level === "group" || root.level === "members") ? groupPage.loaded : root.level === "media" ? mediaPage.loaded : root.level === "safety" ? safetyPage.loaded : false
    // The page of the groups in common shows instead of the person's (after "Show All"): the
    // person level's state, said here for the tests that ask the panel.
    property alias allGroups: personPage.allGroups
    // The panel sits beside the timeline (a line on its leading edge), or covers the chat.
    property bool beside: false
    // The person whose safety-number level opens as soon as her page has come (the message menu's entry of a paused
    // send, BQ-3: the page was asked for in the same moment, so it is not there yet); "" for none. Only that person's
    // page opens it: another answer, a reset or another level forgets it.
    property string safetyWanted: ""
    // The safety level's buttons are dead for a second after a number appears (X1). Read by the fixture pages
    // (`--page=safety`), which are taken once the buttons are awake, so the pictures show the buttons as they are for a
    // person who looks at the number for a while.
    readonly property bool safetyCooling: safetyPage.cooling
    // The share picker of the person's page while it is open (P5c; read by `--page=share` and `chat_ui`).
    readonly property var shareDialog: personPage.shareDialog

    // The keyboard and a screen reader land on a named pane (the subject of the page on top).
    Accessible.role: Accessible.Pane
    Accessible.name: root.spokenName(root.level, root.person.name, root.group.name)

    // What a screen reader says for the pane on top: the person's or the group's name WITHOUT WhatsApp's "~"
    // mark (`Style.unmarked`: the mark is for labels, a screen reader would say "tilde"; W6a2 PRIV-1, G1), the
    // safety level's own sentence (Signal only: no WhatsApp person has that level) or the media level's title. A
    // function of its inputs so `qml_smoke` (`checkMarkedNames`) can hand it a marked name.
    function spokenName(level: string, personName: var, groupName: var): string {
        if (level === "safety") {
            return App.safety.accessibleTitle ?? "";
        }
        if (level === "media") {
            return L10n.i18nc("@title:group the level with every picture and file of a chat", "All Media and Files");
        }
        return Style.unmarked((level === "person" ? personName : groupName) ?? "");
    }

    signal closeRequested
    // A chat the panel offers (a group in common, Message) is to be opened.
    signal chatRequested(real id)
    // "Custom Time…" of the timer of `conversation` was chosen.
    signal customTimerRequested(real conversation, int min, int max, int step, int current)

    // Opens the panel at `level` ("group" or "person"): the page was asked for (`App.group`,
    // `App.person`).
    function start(level: string): void {
        root.levels = [level];
    }

    // What belongs to one open alone ("Show All", the opened texts, every place in a page, the
    // search, the levels) is forgotten at every open and close: the same item serves every later
    // open.
    function reset(): void {
        root.levels = [];
        root.safetyWanted = "";
        root.closeMediaLevel();
        App.closeSafety();
        safetyPage.reset();
        personPage.reset();
        groupPage.reset();
        membersPage.reset();
    }

    // The level of all media goes: its two lists are not asked about any more and are emptied.
    function closeMediaLevel(): void {
        App.closeChatMedia("media");
        App.closeChatMedia("files");
        mediaPage.reset();
    }

    // One level back. False when the panel is at its bottom level (there is nothing to go back
    // to: the caller closes it).
    function back(): bool {
        if (root.level === "person" && personPage.leaveGroups()) {
            return true;
        }
        if (root.levels.length <= 1) {
            return false;
        }
        const left = root.level;
        root.levels = root.levels.slice(0, -1);
        if (left === "person") {
            App.closePerson();
        } else if (left === "media") {
            root.closeMediaLevel();
        } else if (left === "safety") {
            App.closeSafety();
        }
        Qt.callLater(root.focusTop);
        return true;
    }

    // The keyboard returns to what opened the level that is on top again.
    function focusTop(): void {
        if (root.level === "members") {
            membersPage.focusOpener();
        } else if (root.level === "group") {
            groupPage.focusOpener();
        } else if (root.level === "media") {
            mediaPage.focusOpener();
        } else if (root.level === "safety") {
            safetyPage.focusFirst();
        } else if (!(root.level === "person" && personPage.focusOpener())) {
            root.forceActiveFocus();
        }
    }

    // The page of a person the TIMELINE asked for (a sender's name or picture, a mention; the page
    // was asked for with `App.showPersonInChat`) goes on top of the BASE levels the panel has, the
    // group's page and the members level (with its search and its place), which stay alive below
    // and which Back returns to as they were. EVERYTHING ABOVE THEM goes: a person already there
    // (it is replaced), and a level of all media, which belongs to the page it was opened from
    // (`group,person,media` + a sender -> `group,person`; `group,media` + a sender -> `group,person`;
    // there is never more than one "media" level, and never a media level that lost its page).
    // The timeline is not a row of a page, so no row is remembered as the opener of this page: Back
    // puts the keyboard where a level without an opener has it (the first row).
    function pushPerson(): void {
        root.safetyWanted = "";
        groupPage.opener = "";
        membersPage.opener = "";
        personPage.reset();
        const base = [];
        for (const level of root.levels) {
            if (level !== "group" && level !== "members") {
                break;
            }
            base.push(level);
        }
        if (root.levels.indexOf("media") >= 0) {
            root.closeMediaLevel();
        }
        // (The safety-number level belongs to the page it was opened from, as a media level does.)
        if (root.levels.indexOf("safety") >= 0) {
            App.closeSafety();
        }
        root.levels = base.concat(["person"]);
        // (The keyboard goes to the page itself: the click came from the timeline, so the focus may
        // still be in the field of a level that is now hidden, the members' search, where Escape
        // would only empty its text.)
        personPage.forceActiveFocus();
    }

    // A member's page opens on top of the level `from` (a member's row, or "You").
    function openMember(user: string): void {
        if (root.level !== "group" && root.level !== "members") {
            return;
        }
        if (!App.showPerson(root.group.account, user)) {
            return;
        }
        personPage.reset();
        root.levels = root.levels.concat(["person"]);
        root.forceActiveFocus();
    }

    // The level of all media and files of `conversation` (the chat of the page below, of `account`)
    // opens on top of the page it was chosen on.
    function showMedia(account: real, conversation: real): void {
        // One media level at most: the two slots belong to the page below it.
        if (root.levels.indexOf("media") >= 0 || conversation <= 0) {
            return;
        }
        if (!App.showChatMedia("media", account, conversation)) {
            return;
        }
        App.showChatMedia("files", account, conversation);
        mediaPage.reset();
        root.levels = root.levels.concat(["media"]);
        root.forceActiveFocus();
    }

    // The level of the safety number of the person on top (P5a, "View Safety Number") opens above that page. One
    // safety level at most, and only above a person's page that offers it.
    function showSafety(): void {
        if (root.level !== "person" || !(root.person.safetyShown ?? false) || root.levels.indexOf("safety") >= 0) {
            return;
        }
        if (!App.showSafety(root.person.account, root.person.user)) {
            return;
        }
        personPage.opener = "safety";
        safetyPage.reset();
        // The keyboard waits on the back arrow until the answer shows the number; then it goes to the digits, never to a
        // button (W1).
        safetyPage.pending = "open";
        root.levels = root.levels.concat(["safety"]);
        Qt.callLater(safetyPage.focusFirst);
    }

    // "Share Contact…" of the person on top (P5c): the picker opens over the window; false when the page offers no card.
    function openShare(): bool {
        return root.level === "person" && personPage.openShare();
    }

    // A time chosen in her page's Disappearing Messages selector, as its popup chooses one, and not applied (the
    // fixture page `timerpending`); false when her page does not offer that time.
    function chooseTime(secs: int): bool {
        return root.level === "person" && personPage.chooseTime(secs);
    }

    // An answer of the safety level has come (its buttons may have been made anew): the level puts the keyboard where it
    // belongs, a moment later, once the old buttons are gone. `swapped`: the answer is another number than the one on
    // screen (P5a U2). The buttons of a number that was not on screen just before (the first of the level, one after an
    // answer without a number, a swap) and those that replace an approval (Y2) are dead for a second at once, in this
    // very call, so nothing can reach them before the keyboard has been moved (V1, X1: `SafetyPage.arrived`).
    function safetyAnswered(swapped: bool): void {
        if (root.level !== "safety") {
            return;
        }
        safetyPage.arrived(swapped, safetyPage.hasNumber);
        Qt.callLater(() => safetyPage.answered(swapped, root.Window.activeFocusItem === root));
    }

    function showAll(): void {
        if (root.level !== "group") {
            return;
        }
        root.levels = root.levels.concat(["members"]);
        root.forceActiveFocus();
    }

    // A chat of the panel's pages was chosen. The group the panel was opened from is already
    // open: that goes back to its page instead of doing nothing.
    function openChat(id: real): void {
        if (id === App.timeline.conversationId) {
            // Nothing lies below the page (it was opened from the timeline of this very chat): the
            // chat asked for is the one behind the panel, so the panel closes.
            if (root.levels.length <= 1) {
                root.closeRequested();
                return;
            }
            while (root.levels.length > 1) {
                root.back();
            }
            return;
        }
        root.chatRequested(id);
    }

    // The plate: opaque, with the pointer events of its whole area.
    Rectangle {
        anchors.fill: parent
        color: Kirigami.Theme.backgroundColor
    }
    Kirigami.Separator {
        visible: root.beside
        anchors.top: parent.top
        anchors.bottom: parent.bottom
        anchors.left: parent.left
    }
    MouseArea {
        anchors.fill: parent
        acceptedButtons: Qt.AllButtons
        hoverEnabled: true
        onWheel: wheel => {
            wheel.accepted = true;
        }
    }

    // A new answer of the safety level makes its buttons anew (the state changed: Mark became Clear): a keyboard that was
    // on a button is left on the level's scroll view (a focus scope) or on the panel, so the level puts it back on a
    // button after a Mark or a Clear, else on its back arrow (`safetyAnswered`, W2).
    Connections {
        target: App
        function onPersonLoaded(): void {
            if (root.safetyWanted === "" || root.level !== "person" || !personPage.loaded) {
                return;
            }
            const wanted = root.safetyWanted;
            root.safetyWanted = "";
            if ((root.person.user ?? "") === wanted) {
                root.showSafety();
            }
        }
        function onSafetyLoaded(): void {
            root.safetyAnswered(App.safety.swapped ?? false);
        }
    }

    Keys.onEscapePressed: event => {
        if (!root.back()) {
            root.closeRequested();
        }
        event.accepted = true;
    }

    // The close button comes first in the Tab order (it stands over the pages: `z`), as it
    // always did: the order of the panel's items is a thing the tests of the keys depend on.
    QQC2.ToolButton {
        id: closeButton
        objectName: "detailsClose"
        anchors.top: parent.top
        anchors.right: parent.right
        anchors.margins: Kirigami.Units.smallSpacing
        z: 1
        icon.name: "window-close"
        display: QQC2.AbstractButton.IconOnly
        text: L10n.i18nc("@action:button", "Close")
        QQC2.ToolTip.text: text
        QQC2.ToolTip.visible: hovered
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        Accessible.name: text
        onClicked: root.closeRequested()
    }

    PersonPage {
        id: personPage
        anchors.fill: parent
        visible: root.level === "person"
        beside: root.beside
        stacked: root.levels.length > 1
        barTop: closeButton.y + closeButton.height + Kirigami.Units.smallSpacing
        onBackRequested: root.back()
        onChatRequested: id => root.openChat(id)
        onMediaRequested: (account, conversation) => root.showMedia(account, conversation)
        onSafetyRequested: root.showSafety()
        onCustomTimerRequested: (conversation, min, max, step, current) => root.customTimerRequested(conversation, min, max, step, current)
    }

    SafetyPage {
        id: safetyPage
        anchors.fill: parent
        visible: root.level === "safety"
        beside: root.beside
        trailingGap: closeButton.width + Kirigami.Units.smallSpacing * 2
        barTop: closeButton.y + closeButton.height + Kirigami.Units.smallSpacing
        onBackRequested: root.back()
    }

    GroupPage {
        id: groupPage
        anchors.fill: parent
        visible: root.level === "group"
        beside: root.beside
        barTop: closeButton.y + closeButton.height + Kirigami.Units.smallSpacing
        onMemberRequested: user => root.openMember(user)
        onShowAllRequested: root.showAll()
        onMediaRequested: (account, conversation) => root.showMedia(account, conversation)
        onCustomTimerRequested: (conversation, min, max, step, current) => root.customTimerRequested(conversation, min, max, step, current)
    }

    MediaPage {
        id: mediaPage
        anchors.fill: parent
        visible: root.level === "media"
        active: root.levels.indexOf("media") >= 0
        beside: root.beside
        trailingGap: closeButton.width + Kirigami.Units.smallSpacing * 2
        onBackRequested: root.back()
    }

    MembersPage {
        id: membersPage
        anchors.fill: parent
        visible: root.level === "members"
        active: root.levels.indexOf("members") >= 0
        beside: root.beside
        trailingGap: closeButton.width + Kirigami.Units.smallSpacing * 2
        onMemberRequested: user => root.openMember(user)
        onBackRequested: root.back()
    }
}
