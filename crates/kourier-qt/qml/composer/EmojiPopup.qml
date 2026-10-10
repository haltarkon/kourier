import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami

// The composer's emoji picker: categories and a grid; a pick is inserted at the cursor.
QQC2.Popup {
    id: root

    signal picked(string emoji)

    // In the window's scene (also on Wayland, and in headless screenshots).
    popupType: QQC2.Popup.Item
    padding: Kirigami.Units.smallSpacing
    focus: true
    onOpened: grid.focusGrid()

    contentItem: EmojiGrid {
        id: grid
        onPicked: emoji => {
            root.picked(emoji);
            root.close();
        }
    }
}
