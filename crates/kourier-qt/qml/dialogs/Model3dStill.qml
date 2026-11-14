import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The 3D viewer where Quick 3D draws nothing (the software Quick backend: the shots, the window
// tests, a machine without a GPU): the model's still full size, tinted with the model colour, under
// the same bar. No Quick 3D is imported here, so nothing of it is loaded on this path.
QQC2.Popup {
    id: root
    objectName: "model3dViewer"

    property real attachment: 0
    property string name: ""
    property bool saveOffered: false
    // The still is a shade picture the model colour tints.
    property bool tint: true
    // The software fallback (a test tells the two viewers apart by this).
    readonly property bool fallback: true
    readonly property alias freeRotation: header.freeRotation

    function save(): void {
        if (root.saveOffered && root.attachment > 0) {
            MediaActions.save([root.attachment]);
        }
    }

    parent: QQC2.Overlay.overlay
    x: 0
    y: 0
    width: parent ? parent.width : 0
    height: parent ? parent.height : 0
    modal: true
    focus: true
    padding: 0
    closePolicy: QQC2.Popup.CloseOnEscape

    Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
    Kirigami.Theme.inherit: false

    background: Rectangle {
        color: Qt.alpha(Kirigami.Theme.backgroundColor, 0.92)
    }

    contentItem: Item {
        id: stage
        focus: true
        Keys.onPressed: event => {
            if (event.matches(StandardKey.Save)) {
                root.save();
                event.accepted = true;
            }
        }

        Image {
            id: still
            objectName: "still"
            anchors.fill: parent
            anchors.margins: Kirigami.Units.gridUnit * 2
            anchors.topMargin: Kirigami.Units.gridUnit * 3
            // The URL names the colour, so a changed colour makes the picture again.
            source: root.attachment > 0 ? "image://kourier/thumb/" + root.attachment + "?v=1" + (root.tint ? "&tint=" + Style.modelTint : "") : ""
            asynchronous: true
            smooth: true
            mipmap: true
            fillMode: Image.PreserveAspectFit
        }

        Model3dHeader {
            id: header
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            name: root.name
            attachment: root.attachment
            saveOffered: root.saveOffered
            threeD: false
            onSaveRequested: root.save()
            onCloseRequested: root.close()
        }
    }

    onOpened: stage.forceActiveFocus()
}
