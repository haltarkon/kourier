pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The emoji picker's content, one component for the composer's popup and the reaction picker: a
// search field, the tabs (Favorites first, then one per Unicode group) and the emoji of the chosen
// tab. The Favorites tab shows "Recently Used" and then the starred emoji; a search replaces the
// tabs' grid with its hits (names and keywords, English and Ukrainian). A right click (or the Menu
// key, Shift+F10) on an emoji opens a menu: pin it to the reactions, star it. Typing while the
// picker is open goes to the search field.
ColumnLayout {
    id: root

    // Emoji per row.
    property int columns: 8
    // Rows shown before the grid scrolls.
    property int rows: 6
    // Emoji shown as chosen already (our own reactions).
    property var chosen: []
    // 0 = Favorites, 1.. = the Unicode groups.
    property int tab: 0

    // Open: nothing of the table is listed while the picker is closed.
    readonly property bool shown: root.visible
    readonly property bool searching: search.text.trim().length > 0
    readonly property var lists: App.emojiLists
    readonly property real cell: Math.round(Kirigami.Units.gridUnit * 2)
    readonly property alias emojis: emojiModel
    readonly property alias searchField: search
    readonly property alias menu: emojiMenu
    readonly property alias favouritesGrid: starredGrid
    readonly property alias recentGrid: recentCells
    readonly property alias groupGrid: groupCells
    readonly property alias hitsGrid: hitCells

    signal picked(string emoji)

    function focusGrid(): void {
        search.forceActiveFocus();
    }

    // The tab names (the group's Unicode name is the key).
    function groupName(group: int): string {
        switch (emojiModel.groupKey(group)) {
        case "Smileys & Emotion":
            return L10n.i18nc("@title:tab emoji category", "Smileys and Emotion");
        case "People & Body":
            return L10n.i18nc("@title:tab emoji category", "People and Body");
        case "Animals & Nature":
            return L10n.i18nc("@title:tab emoji category", "Animals and Nature");
        case "Food & Drink":
            return L10n.i18nc("@title:tab emoji category", "Food and Drink");
        case "Travel & Places":
            return L10n.i18nc("@title:tab emoji category", "Travel and Places");
        case "Activities":
            return L10n.i18nc("@title:tab emoji category", "Activities");
        case "Objects":
            return L10n.i18nc("@title:tab emoji category", "Objects");
        case "Symbols":
            return L10n.i18nc("@title:tab emoji category", "Symbols");
        case "Flags":
            return L10n.i18nc("@title:tab emoji category", "Flags");
        }
        return emojiModel.groupKey(group);
    }

    function choose(glyph: string): void {
        root.picked(glyph);
    }

    // The menu of an emoji, at `x`, `y` of `item`.
    function openMenu(glyph: string, item: Item, x: real, y: real): void {
        emojiMenu.glyph = glyph;
        emojiMenu.popup(item, x, y);
    }

    // A printable key typed anywhere in the picker: into the search field.
    function typeInto(text: string): void {
        search.forceActiveFocus();
        search.text += text;
    }

    onShownChanged: {
        if (root.shown) {
            search.text = "";
            root.tab = 0;
            search.forceActiveFocus();
        }
    }

    spacing: Kirigami.Units.smallSpacing

    // (A creatable type of the module: qmllint does not resolve it, as it does the uncreatable ones.)
    // qmllint disable unresolved-type import
    EmojiModel {
        id: emojiModel

        // Only while the picker shows: a closed picker keeps no rows.
        group: root.shown && !root.searching && root.tab > 0 ? root.tab - 1 : -1
        query: root.shown ? search.text : ""
    }
    // qmllint enable unresolved-type import

    Kirigami.SearchField {
        id: search

        Layout.fillWidth: true
        // Enter picks the first hit; the field's own "accepted after a pause in typing" would
        // pick it while the person is still typing.
        autoAccept: false
        placeholderText: L10n.i18nc("@info:placeholder", "Search emoji…")
        Accessible.name: L10n.i18nc("@label:textbox accessible", "Search emoji")
        onAccepted: {
            if (emojiModel.count > 0) {
                root.choose(emojiModel.glyphAt(0));
            }
        }
        Keys.onDownPressed: {
            if (root.searching) {
                hitCells.forceActiveFocus();
            } else if (root.tab === 0) {
                (recentCells.count > 0 ? recentCells : starredGrid).forceActiveFocus();
            } else {
                groupCells.forceActiveFocus();
            }
        }
    }

    RowLayout {
        spacing: 0
        Layout.fillWidth: true
        visible: !root.searching

        Repeater {
            model: emojiModel.groupCount() + 1
            delegate: QQC2.ToolButton {
                id: tabButton

                required property int index

                // The tabs share the width the grid needs; they do not widen the picker.
                Layout.preferredWidth: 1
                Layout.fillWidth: true
                readonly property string label: tabButton.index === 0 ? L10n.i18nc("@title:tab emoji category", "Favorites") : root.groupName(tabButton.index - 1)
                text: tabButton.index === 0 ? "" : emojiModel.groupIcon(tabButton.index - 1)
                icon.name: tabButton.index === 0 ? "starred-symbolic" : ""
                display: tabButton.index === 0 ? QQC2.AbstractButton.IconOnly : QQC2.AbstractButton.TextOnly
                font.family: "emoji"
                font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.15
                checkable: true
                checked: root.tab === tabButton.index
                autoExclusive: true
                focusPolicy: Qt.NoFocus
                Accessible.name: tabButton.label
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: tabButton.label
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: root.tab = tabButton.index
            }
        }
    }

    Kirigami.Separator {
        Layout.fillWidth: true
    }

    // The Favorites tab: Recently Used, then the starred ones, or a line when there are none.
    ColumnLayout {
        id: favourites

        visible: !root.searching && root.tab === 0
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Layout.preferredHeight: root.cell * root.rows

        QQC2.Label {
            visible: recentCells.count > 0
            text: L10n.i18nc("@title:group emoji", "Recently Used")
            font.bold: true
            Accessible.role: Accessible.Heading
            Layout.fillWidth: true
        }
        EmojiCells {
            id: recentCells

            visible: count > 0
            names: emojiModel
            isList: true
            model: root.shown ? root.lists.recent : []
            chosen: root.chosen
            columns: root.columns
            interactive: false
            Layout.fillWidth: true
            Layout.preferredHeight: Math.ceil(count / root.columns) * root.cell
            onPicked: glyph => root.choose(glyph)
            onMenuRequested: (glyph, item, x, y) => root.openMenu(glyph, item, x, y)
            onEdge: direction => {
                if (direction > 0 && starredGrid.count > 0) {
                    starredGrid.currentIndex = Math.min(recentCells.currentIndex % root.columns, starredGrid.count - 1);
                    starredGrid.forceActiveFocus();
                } else if (direction < 0) {
                    search.forceActiveFocus();
                }
            }
            onTyped: text => root.typeInto(text)
        }
        QQC2.Label {
            visible: starredGrid.count > 0
            text: L10n.i18nc("@title:group emoji", "Favorites")
            font.bold: true
            Accessible.role: Accessible.Heading
            Layout.fillWidth: true
        }
        EmojiCells {
            id: starredGrid

            visible: count > 0
            names: emojiModel
            isList: true
            model: root.shown ? root.lists.favourites : []
            chosen: root.chosen
            columns: root.columns
            Layout.fillWidth: true
            Layout.fillHeight: true
            onPicked: glyph => root.choose(glyph)
            onMenuRequested: (glyph, item, x, y) => root.openMenu(glyph, item, x, y)
            onEdge: direction => {
                if (direction < 0) {
                    (recentCells.count > 0 ? recentCells : search).forceActiveFocus();
                }
            }
            onTyped: text => root.typeInto(text)
        }
        QQC2.Label {
            visible: recentCells.count === 0 && starredGrid.count === 0
            text: L10n.i18nc("@info:placeholder", "Emoji you use and star with a right click show up here.")
            wrapMode: Text.Wrap
            horizontalAlignment: Text.AlignHCenter
            color: Kirigami.Theme.disabledTextColor
            Layout.fillWidth: true
            Layout.topMargin: Kirigami.Units.largeSpacing
        }
        Item {
            Layout.fillHeight: true
            visible: recentCells.count > 0 && starredGrid.count === 0
        }
    }

    // A group's emoji.
    EmojiCells {
        id: groupCells

        visible: !root.searching && root.tab > 0
        names: emojiModel
        model: emojiModel
        chosen: root.chosen
        columns: root.columns
        Layout.fillWidth: true
        Layout.preferredHeight: root.cell * root.rows
        Accessible.name: root.tab > 0 ? root.groupName(root.tab - 1) : ""
        onPicked: glyph => root.choose(glyph)
        onMenuRequested: (glyph, item, x, y) => root.openMenu(glyph, item, x, y)
        onEdge: direction => {
            if (direction < 0) {
                search.forceActiveFocus();
            }
        }
        onTyped: text => root.typeInto(text)
    }

    // The hits of a search.
    EmojiCells {
        id: hitCells

        visible: root.searching && emojiModel.count > 0
        names: emojiModel
        model: emojiModel
        chosen: root.chosen
        columns: root.columns
        Layout.fillWidth: true
        Layout.preferredHeight: root.cell * root.rows
        Accessible.name: L10n.i18nc("@info:status accessible", "Search results")
        onPicked: glyph => root.choose(glyph)
        onMenuRequested: (glyph, item, x, y) => root.openMenu(glyph, item, x, y)
        onEdge: direction => {
            if (direction < 0) {
                search.forceActiveFocus();
            }
        }
        onTyped: text => root.typeInto(text)
    }
    QQC2.Label {
        visible: root.searching && emojiModel.count === 0
        text: L10n.i18nc("@info:status", "No emoji found")
        horizontalAlignment: Text.AlignHCenter
        color: Kirigami.Theme.disabledTextColor
        Layout.fillWidth: true
        Layout.preferredHeight: root.cell * root.rows
        verticalAlignment: Text.AlignVCenter
    }

    // The menu is made with the picker, not when it opens (a menu made on demand can stay as
    // small as its padding).
    QQC2.Menu {
        id: emojiMenu

        property string glyph: ""
        readonly property bool pinned: root.lists.pins.indexOf(emojiMenu.glyph) >= 0
        readonly property bool starred: root.lists.favourites.indexOf(emojiMenu.glyph) >= 0

        popupType: QQC2.Popup.Item

        QQC2.MenuItem {
            objectName: "pinItem"
            text: emojiMenu.pinned ? L10n.i18nc("@action:inmenu", "Unpin from Reactions") : L10n.i18nc("@action:inmenu", "Pin to Reactions")
            enabled: emojiMenu.pinned || root.lists.pins.length < 6
            QQC2.ToolTip.visible: hovered && !enabled
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Six emoji are pinned already: unpin one first.")
            onTriggered: root.lists.togglePin(emojiMenu.glyph)
        }
        QQC2.MenuItem {
            objectName: "starItem"
            text: emojiMenu.starred ? L10n.i18nc("@action:inmenu", "Remove from Favorites") : L10n.i18nc("@action:inmenu", "Add to Favorites")
            onTriggered: root.lists.toggleFavourite(emojiMenu.glyph)
        }
    }
}
