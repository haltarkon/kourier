pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// A grid of emoji cells: the rows of an `EmojiModel` (a group or the hits of a search), or a plain
// list of glyphs (the Recently Used and the starred ones). Arrow keys move, Enter or Space picks,
// the Menu key and Shift+F10 ask for the pin and favourite menu of the current cell.
GridView {
    id: root

    // An `EmojiModel` (names, `nameOf`): the cells' names come from it.
    required property var names
    // The model is a list of glyphs (`modelData`), not an `EmojiModel`.
    property bool isList: false
    // Emoji shown as chosen already (our own reactions).
    property var chosen: []
    property int columns: 8
    readonly property real cell: Math.round(Kirigami.Units.gridUnit * 2)

    signal picked(string glyph)
    // The menu was asked for on `glyph`, at this point of `item`.
    signal menuRequested(string glyph, Item item, real x, real y)
    // Up or Down left the grid at its first or last row.
    signal edge(int direction)
    // A printable key the grid does not use (the picker sends it to its search field).
    signal typed(string text)

    function glyphAtIndex(i: int): string {
        if (i < 0 || i >= root.count) {
            return "";
        }
        return root.isList ? root.model[i] : root.model.glyphAt(i);
    }

    function pick(): void {
        const g = root.glyphAtIndex(root.currentIndex);
        if (g.length > 0) {
            root.picked(g);
        }
    }

    function askMenu(): void {
        const item = root.itemAtIndex(root.currentIndex);
        const g = root.glyphAtIndex(root.currentIndex);
        if (item && g.length > 0) {
            root.menuRequested(g, item, item.width / 2, item.height);
        }
    }

    clip: true
    cellWidth: root.cell
    cellHeight: root.cell
    activeFocusOnTab: true
    keyNavigationEnabled: true
    boundsBehavior: Flickable.StopAtBounds
    implicitWidth: root.cell * root.columns + (bar.visible ? bar.width : 0)
    QQC2.ScrollBar.vertical: QQC2.ScrollBar {
        id: bar
    }

    Keys.onReturnPressed: root.pick()
    Keys.onEnterPressed: root.pick()
    Keys.onSpacePressed: root.pick()
    Keys.onMenuPressed: root.askMenu()
    Keys.onPressed: event => {
        if (event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier)) {
            root.askMenu();
            event.accepted = true;
        } else if (event.key === Qt.Key_Up && root.currentIndex < root.columns) {
            root.edge(-1);
            event.accepted = true;
        } else if (event.key === Qt.Key_Down && root.currentIndex + root.columns >= root.count) {
            root.edge(1);
            event.accepted = true;
        } else if (event.text.length > 0 && event.text.charCodeAt(0) > 32 && !(event.modifiers & (Qt.ControlModifier | Qt.AltModifier | Qt.MetaModifier))) {
            root.typed(event.text);
            event.accepted = true;
        }
    }

    delegate: EmojiCell {
        id: key

        required property var model
        required property int index

        width: root.cellWidth
        height: root.cellHeight
        glyph: root.isList ? key.model.modelData : key.model.glyph
        label: root.isList ? root.names.nameOf(key.glyph) : key.model.name
        checked: root.chosen.indexOf(key.glyph) >= 0
        current: root.activeFocus && root.currentIndex === key.index
        onClicked: root.picked(key.glyph)
        onMenuRequested: (x, y) => root.menuRequested(key.glyph, key, x, y)
    }
}
