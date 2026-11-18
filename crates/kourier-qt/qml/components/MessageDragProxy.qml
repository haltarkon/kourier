pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// Messages dragged from one chat of a split to the other (S5, his words: "message forwarding/sharing should work
// on drug-n-drop"). ONE item for the window, made with its first split (`MainWindow.dragCarrier`); while a drag runs it stands at the pointer and its
// `Drag` carries the messages to a chat page's drop area (`ChatPage` `forwardDrop`, the key "kourier/messages"),
// which stages a forward into that chat's composer: the forward card, Enter sends it. A card beside the pointer
// says what is carried. The timeline starts, moves and ends a drag (`TimelineActions.messageDrag`); nothing here
// knows a message, it only carries ids.
Item {
    id: root
    objectName: "messageDrag"

    // Where the drag comes from: the pane, its timeline and its chat; and the messages it carries (ids as
    // strings, oldest first for a selection).
    property int fromPane: -1
    property TimelineModel fromModel: null
    property real fromConversation: 0
    property var messages: []
    readonly property bool dragging: root.Drag.active
    // How many drags a chat took, and how many ended without one (tests).
    property int taken: 0
    property int missed: 0

    width: 0
    height: 0
    z: 1000
    visible: root.dragging

    Drag.keys: ["kourier/messages"]
    Drag.source: root
    Drag.supportedActions: Qt.CopyAction
    Drag.proposedAction: Qt.CopyAction

    // Starts a drag of `ids` of `model`'s chat (pane `pane`) at `scene` (a scene position).
    function start(pane: int, model: TimelineModel, ids: var, scene: point): void {
        if (root.Drag.active || model === null || ids.length === 0) {
            return;
        }
        root.fromPane = pane;
        root.fromModel = model;
        root.fromConversation = model.conversationId;
        root.messages = ids;
        root.move(scene);
        root.Drag.active = true;
    }

    // The pointer moved to `scene`.
    function move(scene: point): void {
        const p = root.parent.mapFromItem(null, scene.x, scene.y);
        root.x = p.x;
        root.y = p.y;
    }

    // Ends the drag: a drop where the pointer is (`drop`), else nothing. True when a chat took it.
    function finish(drop: bool): bool {
        if (!root.Drag.active) {
            return false;
        }
        let took = false;
        if (drop) {
            took = root.Drag.drop() !== Qt.IgnoreAction;
        } else {
            root.Drag.cancel();
        }
        root.Drag.active = false;
        if (took) {
            root.taken += 1;
        } else {
            root.missed += 1;
        }
        root.fromPane = -1;
        root.fromModel = null;
        root.fromConversation = 0;
        root.messages = [];
        return took;
    }

    // The card: beside the pointer, on its trailing side.
    Rectangle {
        id: card
        objectName: "messageDragCard"

        Kirigami.Theme.colorSet: Kirigami.Theme.View
        Kirigami.Theme.inherit: false

        x: root.LayoutMirroring.enabled ? -width - Kirigami.Units.largeSpacing : Kirigami.Units.largeSpacing
        y: Kirigami.Units.largeSpacing
        width: cardRow.implicitWidth + Kirigami.Units.largeSpacing * 2
        height: cardRow.implicitHeight + Kirigami.Units.smallSpacing * 2
        radius: Kirigami.Units.cornerRadius
        color: Kirigami.Theme.backgroundColor
        border.width: 1
        border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)

        Row {
            id: cardRow

            anchors.centerIn: parent
            spacing: Kirigami.Units.smallSpacing

            Kirigami.Icon {
                anchors.verticalCenter: parent.verticalCenter
                width: Kirigami.Units.iconSizes.small
                height: width
                source: "mail-forward"
            }
            QQC2.Label {
                objectName: "messageDragText"
                anchors.verticalCenter: parent.verticalCenter
                textFormat: Text.PlainText
                text: L10n.i18ncp("@info:status the card beside the pointer while messages are dragged to the other chat", "Forward %1 message", "Forward %1 messages", root.messages.length)
            }
        }
    }
}
