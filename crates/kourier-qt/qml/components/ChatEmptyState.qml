import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// What the chat column says while no conversation is open. One piece, because two things show
// it: the chat page, and the page that stands in for it in the first frame of a wide window.
Kirigami.PlaceholderMessage {
    icon.source: Style.appIcon
    text: L10n.i18nc("@info:placeholder", "Select a conversation")
    explanation: L10n.i18nc("@info:placeholder", "Pick a chat from the list to start reading.")
}
