pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The group's page of the details panel (people pages P2; architecture §8.3): picture, name and
// description; Disappearing Messages (read-only where the network offers no timer to this
// client, with the reason); the members (the first few, "Show All" for the rest, a click opens
// the member's page); and the rules of the group that are on. A part with nothing in it is left
// out. The page lives in `App.group` (a map from `GroupView` without its member rows; the order,
// the counts and every text are the viewmodel's); the first member rows are the list model
// `App.groupPreview` (P2b), so a refresh that changes one member touches that model only. It is a level of the panel's stack: the panel owns the plate, the
// close button and the keys, and keeps this item alive (hidden) while a page is open on top of
// it, so the place in the page and the keyboard's row survive. Every text from the network is
// plain text.
Item {
    id: root

    // A button of the invite link's part; Return and Enter press it as Space does.
    component InviteButton: QQC2.Button {
        id: inviteButton
        width: parent ? parent.width : 0
        flat: true
        Accessible.name: inviteButton.text
        Keys.onReturnPressed: event => {
            inviteButton.clicked();
            event.accepted = true;
        }
        Keys.onEnterPressed: event => {
            inviteButton.clicked();
            event.accepted = true;
        }
    }

    // Replacing or turning off a link is asked first: people who hold it are cut off.
    Kirigami.PromptDialog {
        id: inviteDialog
        objectName: "groupInviteDialog"
        // "reset" or "off".
        property string action: ""
        parent: QQC2.Overlay.overlay
        title: inviteDialog.action === "reset" ? L10n.i18nc("@title:dialog", "Reset the Invite Link?") : L10n.i18nc("@title:dialog", "Turn Off the Invite Link?")
        subtitle: inviteDialog.action === "reset" ? L10n.i18nc("@info", "The current link stops working. Nobody can join with it any more.") : L10n.i18nc("@info", "Nobody can join with the link while it is off. Turning it on again brings the same link back.")
        preferredWidth: Kirigami.Units.gridUnit * 24
        maximumWidth: Kirigami.Units.gridUnit * 24
        standardButtons: Kirigami.Dialog.NoButton
        customFooterActions: [
            Kirigami.Action {
                text: inviteDialog.action === "reset" ? L10n.i18nc("@action:button", "Reset Link") : L10n.i18nc("@action:button", "Turn Off")
                icon.name: inviteDialog.action === "reset" ? "view-refresh" : "process-stop"
                onTriggered: {
                    App.groupInvite(inviteDialog.action, root.group.id ?? 0);
                    inviteDialog.close();
                }
            },
            Kirigami.Action {
                text: L10n.i18nc("@action:button", "Cancel")
                icon.name: "dialog-cancel"
                onTriggered: inviteDialog.close()
            }
        ]
    }

    readonly property var group: App.group
    // The page has arrived (an empty map before).
    readonly property bool loaded: root.group.name !== undefined
    // The panel sits beside the timeline (a line on its leading edge), or covers the chat.
    property bool beside: false
    // Where the vertical scroll bar may start (below the panel's close button; set by the panel).
    property real barTop: 0
    // What opened the page on top of this one, so the keyboard can return to it by key (a
    // refresh may rebuild the rows meanwhile): "showAll" or "member:<user id>".
    property string opener: ""

    readonly property bool membersShown: root.loaded && ((root.group.memberCount ?? 0) > 0 || (root.group.invited ?? "") !== "" || (root.group.requests ?? "") !== "")

    // A member's row was chosen: their page opens on top of this one.
    signal memberRequested(string user)
    // "Show All" was chosen: the members level opens on top of this one.
    signal showAllRequested
    // "All Media and Files" was chosen: the level of all media opens on top of this page for
    // `conversation` (the group's chat) of `account`.
    signal mediaRequested(real account, real conversation)
    // "Custom Time…" of the group's timer was chosen.
    signal customTimerRequested(real conversation, int min, int max, int step, int current)

    // What belongs to one group alone (the opened description, the place in the page, what
    // opened a page on top) is forgotten whenever the panel is opened or closed: the same item
    // serves every later page.
    function reset(): void {
        timerPart.reset();
        description.reset();
        root.opener = "";
        const flick = scroll.contentItem as Flickable;
        if (flick) {
            flick.contentY = 0;
        }
    }

    // The keyboard returns to what opened the page on top of this one, found by its key; when
    // that row is gone, to the first row, else to the panel.
    function focusOpener(): void {
        let target = null;
        if (root.opener === "showAll" && showAll.visible) {
            target = showAll;
        } else if (root.opener === "media" && mediaPart.allButton.visible) {
            target = mediaPart.allButton;
        } else if (root.opener.startsWith("member:")) {
            const user = root.opener.slice(7);
            for (let i = 0; i < memberRepeater.count; ++i) {
                const row = memberRepeater.itemAt(i) as MemberRow;
                if (row && row.member.user === user) {
                    target = row;
                    break;
                }
            }
        }
        if (!target && memberRepeater.count > 0) {
            target = memberRepeater.itemAt(0);
        }
        (target ?? root.parent)?.forceActiveFocus();
    }

    QQC2.ScrollView {
        id: scroll
        objectName: "groupScroll"
        anchors.fill: parent
        anchors.leftMargin: root.beside ? 1 : 0
        contentWidth: availableWidth
        QQC2.ScrollBar.horizontal.policy: QQC2.ScrollBar.AlwaysOff
        // The panel's close button stands over the top right corner: the scroll bar starts below it.
        QQC2.ScrollBar.vertical.topPadding: root.barTop
        // The content's width while the scroll bar shows (the style keeps the bar's width beside the content, unless the
        // bar lies over it): the same whether the bar shows or not, for what must not follow the bar (the media squares).
        readonly property real steadyWidth: scroll.width - (scroll.QQC2.ScrollBar.vertical.interactive ? scroll.QQC2.ScrollBar.vertical.width : 0)

        Column {
            id: content
            width: scroll.availableWidth
            spacing: Kirigami.Units.largeSpacing
            topPadding: Kirigami.Units.largeSpacing * 2
            bottomPadding: Kirigami.Units.largeSpacing * 2

            // ---- the group ----
            Column {
                id: head
                visible: root.loaded
                width: parent.width - Kirigami.Units.largeSpacing * 2
                x: Kirigami.Units.largeSpacing
                spacing: Kirigami.Units.smallSpacing

                Avatar {
                    objectName: "groupAvatar"
                    anchors.horizontalCenter: parent.horizontalCenter
                    name: root.group.name ?? ""
                    source: root.group.avatarUrl ?? ""
                    implicitWidth: Kirigami.Units.gridUnit * 6
                    implicitHeight: Kirigami.Units.gridUnit * 6
                    width: implicitWidth
                    height: implicitHeight
                }
                CopyableText {
                    objectName: "groupName"
                    width: parent.width
                    text: root.group.name ?? ""
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold,
                        "pointSize": Kirigami.Theme.defaultFont.pointSize * 1.3
                    })
                }
                // The description: three lines, "More" for the rest; selectable, with links in an accepted chat.
                DescriptionText {
                    id: description
                    objectName: "groupDescription"
                    width: parent.width
                    html: root.group.descriptionHtml ?? ""
                    text: root.group.description ?? ""
                    hrefs: root.group.descriptionHrefs ?? []
                    accessibleName: L10n.i18nc("@info:accessible the group's description", "Group description")
                }
                QQC2.Button {
                    objectName: "groupDescriptionMore"
                    visible: description.visible && (description.truncated || description.expanded)
                    anchors.horizontalCenter: parent.horizontalCenter
                    flat: true
                    text: description.expanded ? L10n.i18nc("@action:button show less of the group's description", "Less") : L10n.i18nc("@action:button show the rest of the group's description", "More")
                    onClicked: description.expanded = !description.expanded
                }
                Repeater {
                    model: root.group.notes ?? []
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

            // ---- Disappearing Messages: the group's timer ----
            TimerPart {
                id: timerPart
                objectName: "groupTimer"
                visible: root.loaded
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                conversation: root.group.id ?? 0
                timers: root.group.timers ?? []
                timerSecs: root.group.timerSecs ?? 0
                timerLabel: root.group.timer ?? ""
                custom: root.group.timerCustom ?? false
                customMin: root.group.timerMin ?? 0
                customMax: root.group.timerMax ?? 0
                customStep: root.group.timerStep ?? 1
                note: root.group.timerNote ?? ""
                failed: root.group.timerFailed ?? ""
                boxName: "groupTimerBox"
                heldName: "groupTimerHeld"
                onCustomRequested: (conversation, min, max, step, current) => root.customTimerRequested(conversation, min, max, step, current)
            }

            // ---- Members ----
            Column {
                id: membersPart
                objectName: "groupMembers"
                visible: root.membersShown
                width: parent.width
                spacing: 0

                QQC2.Label {
                    x: Kirigami.Units.largeSpacing
                    width: parent.width - Kirigami.Units.largeSpacing * 2
                    text: L10n.i18nc("@title:group the members part of a group's page", "Members")
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                }
                // How many there are, and the people not in yet (quiet lines).
                QQC2.Label {
                    objectName: "groupMembersCount"
                    visible: text !== ""
                    x: Kirigami.Units.largeSpacing
                    width: parent.width - Kirigami.Units.largeSpacing * 2
                    text: root.group.membersTitle ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                }
                QQC2.Label {
                    objectName: "groupInvited"
                    visible: text !== ""
                    x: Kirigami.Units.largeSpacing
                    width: parent.width - Kirigami.Units.largeSpacing * 2
                    text: root.group.invited ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                }
                QQC2.Label {
                    objectName: "groupRequests"
                    visible: text !== ""
                    x: Kirigami.Units.largeSpacing
                    width: parent.width - Kirigami.Units.largeSpacing * 2
                    text: root.group.requests ?? ""
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                }
                Item {
                    width: 1
                    height: Kirigami.Units.smallSpacing
                }
                Repeater {
                    id: memberRepeater
                    model: App.groupPreview
                    delegate: MemberRow {
                        id: row
                        objectName: "groupMember"
                        required property string user
                        required property string name
                        required property string avatarUrl
                        required property bool admin
                        required property bool isSelf
                        member: ({
                                "user": row.user,
                                "name": row.name,
                                "avatarUrl": row.avatarUrl,
                                "admin": row.admin,
                                "isSelf": row.isSelf
                            })
                        adminLabel: root.group.adminLabel ?? ""
                        account: root.group.account ?? 0
                        width: membersPart.width
                        // A row of the keyboard: Tab reaches it, Return, Enter and Space open the
                        // member's page, and the frame of the keyboard's focus shows which.
                        focusPolicy: Qt.StrongFocus
                        highlighted: row.visualFocus
                        Keys.onReturnPressed: event => {
                            row.clicked();
                            event.accepted = true;
                        }
                        Keys.onEnterPressed: event => {
                            row.clicked();
                            event.accepted = true;
                        }
                        onClicked: {
                            root.opener = "member:" + row.member.user;
                            root.memberRequested(row.member.user);
                        }
                    }
                }
                QQC2.Button {
                    id: showAll
                    objectName: "groupShowAll"
                    visible: (root.group.membersMore ?? "") !== ""
                    x: LayoutMirroring.enabled ? parent.width - width - Kirigami.Units.largeSpacing : Kirigami.Units.largeSpacing
                    flat: true
                    text: root.group.membersMore ?? ""
                    onClicked: {
                        root.opener = "showAll";
                        root.showAllRequested();
                    }
                }
            }

            // ---- the invite link: an administrator, on a network that has links. The link
            // itself stays in Rust (`App.groupInvite`); `App.copyTold` says what came of a click. ----
            Column {
                id: invitePart
                objectName: "groupInvite"
                readonly property string kind: root.loaded ? (root.group.invites ?? "") : ""
                // WhatsApp's link always stands; Signal's can be off.
                readonly property bool linkOn: invitePart.kind === "reset" || (root.group.inviteOn ?? false)
                visible: invitePart.kind !== ""
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    width: parent.width
                    text: L10n.i18nc("@title:group the invite link part of a group's page", "Invite Link")
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                }
                InviteButton {
                    objectName: "groupInviteCopy"
                    visible: invitePart.linkOn
                    icon.name: "edit-copy"
                    text: L10n.i18nc("@action:button", "Copy Invite Link")
                    onClicked: App.groupInvite("read", root.group.id ?? 0)
                }
                InviteButton {
                    objectName: "groupInviteReset"
                    visible: invitePart.linkOn
                    icon.name: "view-refresh"
                    text: L10n.i18nc("@action:button", "Reset Invite Link…")
                    onClicked: {
                        inviteDialog.action = "reset";
                        inviteDialog.open();
                    }
                }
                InviteButton {
                    objectName: "groupInviteOff"
                    visible: invitePart.kind === "switch" && invitePart.linkOn
                    icon.name: "process-stop"
                    text: L10n.i18nc("@action:button", "Turn Off Invite Link…")
                    onClicked: {
                        inviteDialog.action = "off";
                        inviteDialog.open();
                    }
                }
                InviteButton {
                    objectName: "groupInviteOn"
                    visible: invitePart.kind === "switch" && !invitePart.linkOn
                    icon.name: "insert-link"
                    text: L10n.i18nc("@action:button", "Turn On Invite Link")
                    onClicked: App.groupInvite("on", root.group.id ?? 0)
                }
            }

            // ---- the group's rules, read-only ----
            Column {
                id: rulesPart
                objectName: "groupRules"
                visible: root.loaded && (root.group.rules ?? []).length > 0
                x: Kirigami.Units.largeSpacing
                width: parent.width - Kirigami.Units.largeSpacing * 2
                spacing: Kirigami.Units.smallSpacing

                Repeater {
                    model: root.group.rules ?? []
                    delegate: QQC2.Label {
                        id: rule
                        required property string modelData
                        width: rulesPart.width
                        text: rule.modelData
                        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        wrapMode: Text.Wrap
                        textFormat: Text.PlainText
                    }
                }
            }

            // ---- Media and Files: the group's pictures and files (LAST, as Signal Desktop) ----
            MediaSection {
                id: mediaPart
                objectName: "groupMedia"
                slot: "group"
                conversation: root.loaded ? (root.group.id ?? 0) : 0
                account: root.loaded ? (root.group.account ?? 0) : 0
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
