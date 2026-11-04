pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// Every member of the group, with a search (people pages P2; the level of the details panel that
// "Show All" opens). The list is `App.groupMembers`, a Rust list model keyed by the member's user
// id (P2b): a refresh of the page (a member renamed, one joined) is a `dataChanged` or an insert
// in place, so it never resets the place in the list or the keyboard's row. The search writes the
// model's `filter` (the viewmodel's rule, applied in Rust). A group can have a thousand members:
// the list is a bounded `ListView` (only the rows on screen have items), and every row is a
// `MemberRow` (plain text). The panel owns the plate, the close button and the keys; this item
// is kept alive (hidden) while a member's page is open on top of it.
Item {
    id: root

    readonly property var group: App.group
    // The panel sits beside the timeline (a line on its leading edge), or covers the chat.
    property bool beside: false
    // The level is in the panel's stack (shown or covered): only then are the rows kept.
    property bool active: false
    // Room the panel's close button takes at the trailing end of the title row.
    property real trailingGap: 0
    // What opened the member's page on top of this one: "member:<user id>".
    property string opener: ""
    // The search text.
    readonly property alias query: search.text

    // One of the rows was chosen: the member's page opens on top of this one.
    signal memberRequested(string user)
    // The level's own back arrow.
    signal backRequested

    onActiveChanged: {
        if (!root.active) {
            root.reset();
        }
    }

    // Forgets the search and what opened a page on top: the same item serves every later "Show
    // All". (The model's filter is cleared here too: the text may already be empty.)
    function reset(): void {
        root.opener = "";
        search.text = "";
        App.groupMembers.filter = "";
    }

    // The keyboard returns to the row that opened the page on top of this one, found by its
    // key; when that row is gone, to the list's first row, else to the search field.
    function focusOpener(): void {
        let at = -1;
        if (root.opener.startsWith("member:")) {
            at = App.groupMembers.rowOf(root.opener.slice(7));
        }
        if (at < 0 && list.count > 0) {
            at = 0;
        }
        if (at >= 0) {
            list.currentIndex = at;
            list.positionViewAtIndex(at, ListView.Contain);
            list.forceActiveFocus();
        } else {
            search.forceActiveFocus();
        }
    }

    function openCurrent(): void {
        if (list.currentIndex < 0 || list.currentIndex >= list.count) {
            return;
        }
        const user = App.groupMembers.userAt(list.currentIndex);
        root.opener = "member:" + user;
        root.memberRequested(user);
    }

    ColumnLayout {
        anchors.fill: parent
        anchors.leftMargin: root.beside ? 1 : 0
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.largeSpacing
            Layout.topMargin: Kirigami.Units.largeSpacing
            QQC2.ToolButton {
                objectName: "membersBack"
                icon.name: LayoutMirroring.enabled ? "go-next" : "go-previous"
                display: QQC2.AbstractButton.IconOnly
                text: L10n.i18nc("@action:button", "Back")
                Accessible.name: text
                onClicked: root.backRequested()
            }
            QQC2.Label {
                Layout.fillWidth: true
                text: L10n.i18nc("@title:group the members part of a group's page", "Members")
                font: Style.styled(Kirigami.Theme.defaultFont, {
                    "weight": Font.DemiBold
                })
                elide: Text.ElideRight
                textFormat: Text.PlainText
            }
            // (The panel's close button stands over the trailing end of this row.)
            Item {
                implicitWidth: root.trailingGap
            }
        }

        Kirigami.SearchField {
            id: search
            objectName: "membersSearch"
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.largeSpacing
            placeholderText: L10n.i18nc("@info:placeholder the search field of a group's members", "Search members…")
            // The field accepts ~150 ms after each change otherwise (the rows follow the text).
            autoAccept: false
            // Ctrl+F is the window's search of the chats: a second Find shortcut would make both
            // ambiguous.
            focusSequences: []
            Accessible.name: search.placeholderText
            onTextChanged: App.groupMembers.filter = search.text
            // Escape empties a search that has text (while the keyboard is in the field);
            // everywhere else it is the panel's Back.
            Keys.onEscapePressed: event => {
                if (search.text !== "") {
                    search.text = "";
                    event.accepted = true;
                } else {
                    event.accepted = false;
                }
            }
            Keys.onDownPressed: event => {
                if (list.count > 0) {
                    list.currentIndex = Math.max(0, list.currentIndex);
                    list.forceActiveFocus();
                }
                event.accepted = true;
            }
        }

        ListView {
            id: list
            objectName: "membersList"
            Layout.fillWidth: true
            Layout.fillHeight: true
            clip: true
            // Bound only while the level is in the panel's stack: a hidden level holds no 1,000 delegates,
            // and the next "Show All" starts with a new view (top of the list, no current row), as the
            // old list model that `reset()` emptied did.
            model: root.active ? App.groupMembers : null
            // (A view that gets rows makes the first one current: no row is, until the user moves.)
            onModelChanged: list.currentIndex = -1
            boundsBehavior: Flickable.StopAtBounds
            // The list is the tab stop; the rows are not.
            activeFocusOnTab: true
            keyNavigationEnabled: true
            currentIndex: -1
            Accessible.role: Accessible.List
            Accessible.name: L10n.i18nc("@title:group the members part of a group's page", "Members")
            QQC2.ScrollBar.vertical: QQC2.ScrollBar {}
            // Up on the first row goes back to the search field (as KDE's filter lists do).
            Keys.onUpPressed: event => {
                if (list.currentIndex <= 0) {
                    search.forceActiveFocus();
                    event.accepted = true;
                } else {
                    event.accepted = false;
                }
            }
            Keys.onReturnPressed: event => {
                root.openCurrent();
                event.accepted = true;
            }
            Keys.onEnterPressed: event => {
                root.openCurrent();
                event.accepted = true;
            }
            Keys.onSpacePressed: event => {
                root.openCurrent();
                event.accepted = true;
            }
            delegate: MemberRow {
                id: row
                objectName: "membersMember"
                required property int index
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
                width: list.width
                focusPolicy: Qt.NoFocus
                highlighted: list.activeFocus && ListView.isCurrentItem
                onClicked: {
                    list.currentIndex = row.index;
                    root.openCurrent();
                }
            }

            // Nothing matches what was typed.
            QQC2.Label {
                objectName: "membersNone"
                anchors.centerIn: parent
                visible: list.count === 0 && search.text !== ""
                text: L10n.i18nc("@info:placeholder nothing in a group's members matches the search", "No members match.")
                color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                textFormat: Text.PlainText
            }
        }
    }
}
