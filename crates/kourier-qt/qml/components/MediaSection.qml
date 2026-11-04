pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// "Media and Files" of a page of the details panel (people pages P4; architecture §8.3): the six
// newest pictures, videos and GIFs of the page's chat as cropped squares, and the button "All Media
// and Files", which opens the level of all media and files. It is the LAST part of a person's page
// and of a group's page (Signal Desktop's order), and a part with nothing in it is left out: a chat
// with neither pictures nor files shows nothing at all, a chat with files and no pictures shows
// the title and the button. The button is the section's ONE stop of the keyboard (the squares are
// none: the level's grid takes the keys, and a screen reader presses a square through its
// accessible action). The section owns its gallery slot (`App.showChatMedia(slot, account,
// conversation)`): it asks for the chat's gallery when the page has a chat, follows the chat's
// messages while it lives (a picture that arrives or is deleted changes the squares), and asks for
// nothing without a chat. The squares go through the image provider like any picture of the
// timeline, and open the viewer through `MediaActions.viewGallery`, never through the open
// timeline (the chat may be another one than the open chat): the viewer reads the message's whole
// album with a query of its own, not the six squares.
Column {
    id: root

    // Which slot of the gallery this section is: "group" or "person".
    property string slot: "person"
    // The chat whose gallery it shows (0: the page has none) and its account.
    property real conversation: 0
    property real account: 0

    readonly property var gallery: root.slot === "group" ? App.groupMedia : App.personMedia
    readonly property bool shown: root.gallery.count > 0 || root.gallery.other
    // The button that opens the level (the keyboard returns to it).
    readonly property alias allButton: allMedia
    // The width the squares are sized for. The page passes the width its content has WITH its scroll bar, which does
    // not change when the bar comes or goes: a square that grew when the bar went away made a page that just fitted
    // too tall again, the bar came back, the squares shrank, the page fitted, the bar went... a flip every frame, for
    // ever (the wide `person` pictures of P5c, 3 different in 5 renders). Sized this way the page only gets taller
    // when it gets narrower (wrapped text), so the bar settles.
    property real squaresWidth: root.width
    // The squares are three in a row, with the page's margins and a small gap.
    readonly property real side: Math.max(Kirigami.Units.gridUnit * 3, Math.floor((Math.min(root.width, root.squaresWidth) - Kirigami.Units.largeSpacing * 2 - Kirigami.Units.smallSpacing * 2) / 3))

    // "All Media and Files" was chosen: the level of all media opens on top of the page.
    signal showAllRequested

    function sync(): void {
        if (root.conversation > 0) {
            App.showChatMedia(root.slot, root.account, root.conversation);
        } else {
            App.closeChatMedia(root.slot);
        }
    }
    onConversationChanged: root.sync()
    onAccountChanged: root.sync()
    Component.onCompleted: root.sync()

    // The page shows something else in this part's place (the person's page of all groups).
    property bool suppressed: false
    visible: root.shown && !root.suppressed
    spacing: Kirigami.Units.smallSpacing

    QQC2.Label {
        x: Kirigami.Units.largeSpacing
        width: root.width - Kirigami.Units.largeSpacing * 2
        text: L10n.i18nc("@title:group the pictures and files of a chat, on the page of a person or a group", "Media and Files")
        font: Style.styled(Kirigami.Theme.defaultFont, {
            "weight": Font.DemiBold
        })
        elide: Text.ElideRight
        textFormat: Text.PlainText
    }
    Grid {
        id: squares
        objectName: "mediaSquares"
        // (The Grid is as wide as its squares, and a position does not mirror by itself: with one
        // or two squares they stand at the leading edge, the right one under right-to-left.)
        x: LayoutMirroring.enabled ? root.width - width - Kirigami.Units.largeSpacing : Kirigami.Units.largeSpacing
        columns: 3
        spacing: Kirigami.Units.smallSpacing
        Repeater {
            model: root.gallery
            delegate: MediaTile {
                objectName: "mediaSquare"
                gallery: root.slot
                width: root.side
                height: root.side
                // No stop of the keyboard (see the header): one more stop on the person page of the
                // fixture would move where the Tab presses of the `focusCover` step of `chat_ui`
                // end, and there is the one stop below.
                focusPolicy: Qt.NoFocus
            }
        }
    }
    QQC2.Button {
        id: allMedia
        objectName: "mediaAll"
        x: LayoutMirroring.enabled ? root.width - width - Kirigami.Units.largeSpacing : Kirigami.Units.largeSpacing
        flat: true
        text: L10n.i18nc("@action:button open the page with every picture and file of a chat", "All Media and Files")
        Accessible.name: text
        onClicked: root.showAllRequested()
        // (Return and Enter press it like Space: the section's one stop of the keyboard.)
        Keys.onReturnPressed: event => {
            root.showAllRequested();
            event.accepted = true;
        }
        Keys.onEnterPressed: event => {
            root.showAllRequested();
            event.accepted = true;
        }
    }
}
