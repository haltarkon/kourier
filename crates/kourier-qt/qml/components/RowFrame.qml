import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// What a list row of the conversation list draws under its content, shared by the chat row
// (`ConversationDelegate`) and the hit row of the message search (`SearchResultDelegate`, R3 F2):
// the wash of the account's color (Settings → General, "tint rows"), and the frame of the row the
// keyboard is on (and, in a high-contrast scheme, of the open chat), in the row's insets. One item
// per row for both; the parent is the `RoundedItemDelegate` whose insets it follows.
Rectangle {
    id: frame

    // The row's account color ("" for none) and whether the wash shows on this row.
    required property string tint
    required property bool tinted
    required property real wash
    // The row has the keyboard; or the row is framed for another reason (high contrast).
    required property bool focused
    required property bool framed

    // The row it belongs to (its insets are the card's gaps).
    property var row: parent

    z: -1
    anchors.fill: parent
    anchors.leftMargin: framed ? 0 : frame.row.leftInset
    anchors.rightMargin: framed ? 0 : frame.row.rightInset
    anchors.topMargin: framed ? 0 : frame.row.topInset
    anchors.bottomMargin: framed ? 0 : frame.row.bottomInset
    visible: framed || tinted
    radius: Kirigami.Units.cornerRadius
    color: tinted ? Qt.alpha(tint, wash) : "transparent"
    border.width: framed ? 2 : 0
    border.color: focused ? Kirigami.Theme.focusColor : Kirigami.Theme.textColor
}
