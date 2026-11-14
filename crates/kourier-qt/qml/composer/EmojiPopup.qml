import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The composer's emoji picker: search, Favorites and groups; a pick is inserted at the cursor.
QQC2.Popup {
    id: root

    readonly property alias grid: grid

    signal picked(string emoji)

    // In the window's scene (also on Wayland, and in headless screenshots).
    popupType: QQC2.Popup.Item
    padding: Kirigami.Units.smallSpacing
    focus: true
    onOpened: grid.focusGrid()

    contentItem: EmojiGrid {
        id: grid
        onPicked: emoji => {
            App.emojiLists.used(emoji);
            root.picked(emoji);
            root.close();
        }
    }
}
