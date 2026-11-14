import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The bar of the 3D viewer (both the Quick 3D one and the still of the software fallback): the
// file's name at the left, the toolbar at the right. It imports no Quick 3D.
Item {
    id: root

    property string name: ""
    // "Free Rotation" (trackball) instead of the turntable.
    property alias freeRotation: freeButton.checked
    // "Edges over the model" (Quick 3D only).
    property alias wireframe: wireButton.checked
    property bool saveOffered: false
    // The 3D actions (rotation) work: false in the software fallback, which shows the still.
    property bool threeD: true
    // The size line "W × D × H mm" and the triangle count (tooltip), once the model is read.
    property string sizeText: ""
    property string detailText: ""

    // The chosen printer box's row id ("": none), read from the preference; the menu writes it.
    readonly property string boxId: App.settings.modelBox
    // The printer boxes of the viewmodel's table: [{id, label, w, d, h}].
    readonly property var boxRows: JSON.parse(App.settings.modelBoxes())

    // The attachment the viewer shows: what "Open in Slicer" starts a slicer with.
    property real attachment: 0
    // The slicers of the fixed table, in his order, and whether each is installed:
    // [{id, name, installed}] (the app looks on a worker; the last known list shows meanwhile).
    readonly property var slicerRows: JSON.parse(App.slicers)
    // The slicer the main part starts: the last one that was started, OrcaSlicer by default.
    readonly property string slicerId: {
        const want = App.settings.modelSlicer;
        for (const row of root.slicerRows) {
            if (row.id === want) {
                return want;
            }
        }
        return "orcaslicer";
    }
    readonly property var slicerRow: {
        for (const row of root.slicerRows) {
            if (row.id === root.slicerId) {
                return row;
            }
        }
        return null;
    }
    readonly property bool slicerReady: root.slicerRow !== null && root.slicerRow.installed === true
    // The parts of the split button and its menu (the window tests read them).
    readonly property alias slicerMain: slicerButton
    readonly property alias slicerArrowButton: slicerArrow
    readonly property alias slicerList: slicerMenu
    readonly property string slicerText: root.slicerReady ? L10n.i18nc("@action:button, %1 the name of a slicer program", "Open in %1", root.slicerRow.name) : L10n.i18nc("@action:button", "Open in Slicer")
    // A window too narrow for the buttons' words shows their icons alone (the words stay their
    // accessible names); the Quick 3D viewer has three more buttons, so it needs more room. Below
    // that the size line goes too (the chat's picture shows it). Without this the bar ran past the
    // window's left edge and left the file's name no room at 420 px.
    readonly property bool compact: root.width < Kirigami.Units.gridUnit * (root.threeD ? 60 : 40)
    readonly property bool sizeShown: root.width >= Kirigami.Units.gridUnit * 36
    readonly property int buttonDisplay: root.compact ? QQC2.AbstractButton.IconOnly : QQC2.AbstractButton.TextBesideIcon

    // A standard view was chosen: front, back, left, right, top, bottom or iso.
    signal viewSelected(string which)
    signal saveRequested
    signal closeRequested

    implicitHeight: bar.implicitHeight + Kirigami.Units.smallSpacing * 2

    // The list is looked up again each time a viewer opens (a worker thread; never the Qt thread).
    Component.onCompleted: App.refreshSlicers()

    QQC2.Label {
        objectName: "model3dName"
        anchors.left: parent.left
        anchors.leftMargin: Kirigami.Units.gridUnit
        anchors.right: bar.left
        anchors.rightMargin: Kirigami.Units.gridUnit
        anchors.verticalCenter: parent.verticalCenter
        text: root.name
        color: Kirigami.Theme.textColor
        elide: Text.ElideMiddle
        textFormat: Text.PlainText
        Accessible.role: Accessible.Heading
    }

    Row {
        id: bar
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.margins: Kirigami.Units.smallSpacing
        spacing: Kirigami.Units.smallSpacing

        QQC2.Label {
            objectName: "model3dSize"
            visible: root.sizeText !== "" && root.sizeShown
            anchors.verticalCenter: parent.verticalCenter
            text: root.sizeText
            color: Kirigami.Theme.textColor
            textFormat: Text.PlainText
            QQC2.ToolTip.visible: sizeHover.hovered && root.detailText !== ""
            QQC2.ToolTip.text: root.detailText
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            HoverHandler {
                id: sizeHover
            }
        }
        QQC2.ToolButton {
            id: viewsButton
            objectName: "model3dViews"
            display: root.buttonDisplay
            // The standard views and the wireframe need Quick 3D: not in the still fallback.
            visible: root.threeD
            focusPolicy: Qt.NoFocus
            icon.name: "view-preview"
            text: L10n.i18nc("@action:button", "View")
            QQC2.ToolTip.visible: hovered && !viewsMenu.visible
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Standard views")
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            onClicked: viewsMenu.popup(viewsButton, 0, viewsButton.height)

            QQC2.Menu {
                id: viewsMenu
                Repeater {
                    model: [
                        {
                            "key": "front",
                            "icon": "object-order-front",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model", "Front")
                        },
                        {
                            "key": "back",
                            "icon": "object-order-back",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model", "Back")
                        },
                        {
                            "key": "left",
                            "icon": "go-previous",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model", "Left")
                        },
                        {
                            "key": "right",
                            "icon": "go-next",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model", "Right")
                        },
                        {
                            "key": "top",
                            "icon": "go-up",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model", "Top")
                        },
                        {
                            "key": "bottom",
                            "icon": "go-down",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model", "Bottom")
                        },
                        {
                            "key": "iso",
                            "icon": "zoom-fit-best",
                            "label": L10n.i18nc("@action:inmenu standard view of a 3D model: the angle of the picture in the chat", "Isometric")
                        }
                    ]
                    QQC2.MenuItem {
                        required property var modelData
                        objectName: "model3dView-" + modelData.key
                        icon.name: modelData.icon
                        text: modelData.label
                        onTriggered: root.viewSelected(modelData.key)
                    }
                }
            }
        }
        QQC2.ToolButton {
            id: wireButton
            objectName: "model3dWire"
            display: root.buttonDisplay
            visible: root.threeD
            checkable: true
            focusPolicy: Qt.NoFocus
            icon.name: "draw-polyline"
            text: L10n.i18nc("@action:button", "Wireframe")
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Draw the edges of the triangles over the model")
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
        QQC2.ToolButton {
            id: boxButton
            objectName: "model3dBox"
            display: root.buttonDisplay
            // The box is drawn by Quick 3D: not in the still fallback.
            visible: root.threeD
            focusPolicy: Qt.NoFocus
            icon.name: "draw-cuboid"
            text: L10n.i18nc("@action:button", "Printer Box")
            checked: root.boxId !== ""
            QQC2.ToolTip.visible: hovered && !boxMenu.visible
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Show a translucent box of a printer's build volume around the model")
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            onClicked: boxMenu.popup(boxButton, 0, boxButton.height)

            QQC2.Menu {
                id: boxMenu
                popupType: QQC2.Popup.Item
                QQC2.MenuItem {
                    objectName: "model3dBox-off"
                    checkable: true
                    autoExclusive: true
                    checked: root.boxId === ""
                    text: L10n.i18nc("@item:inmenu no printer box", "Off")
                    onTriggered: App.settings.modelBox = ""
                }
                QQC2.MenuSeparator {}
                Repeater {
                    model: root.boxRows
                    QQC2.MenuItem {
                        required property var modelData
                        objectName: "model3dBox-" + modelData.id
                        checkable: true
                        autoExclusive: true
                        checked: root.boxId === modelData.id
                        // Names are product names and are not translated.
                        text: modelData.label
                        onTriggered: App.settings.modelBox = modelData.id
                    }
                }
            }
        }
        // The swatch is drawn by this button's content, not as its icon: the desktop style draws
        // a tool button's icon from a theme name or a local file only, never from an image
        // provider, so an `image://` icon stays empty (qml.md). The style is told "icon only"
        // and has no icon, so it draws just the frame; the content draws the swatch and, in the
        // wide bar, the text.
        QQC2.ToolButton {
            id: colourButton
            objectName: "model3dColour"
            readonly property bool showsText: root.buttonDisplay !== QQC2.AbstractButton.IconOnly
            display: QQC2.AbstractButton.IconOnly
            horizontalPadding: showsText ? Kirigami.Units.smallSpacing * 2 : 0
            focusPolicy: Qt.NoFocus
            text: L10n.i18nc("@action:button", "Model Color")
            contentItem: Item {
                implicitWidth: colourRow.implicitWidth
                implicitHeight: colourRow.implicitHeight

                Row {
                    id: colourRow
                    anchors.centerIn: parent
                    spacing: Kirigami.Units.smallSpacing

                    Kirigami.Icon {
                        objectName: "model3dColourSwatch"
                        anchors.verticalCenter: parent.verticalCenter
                        width: Kirigami.Units.iconSizes.small
                        height: width
                        source: "image://kourierart/swatch/" + Style.modelTint
                    }
                    QQC2.Label {
                        anchors.verticalCenter: parent.verticalCenter
                        visible: colourButton.showsText
                        text: colourButton.text
                        font: colourButton.font
                    }
                }
            }
            QQC2.ToolTip.visible: hovered && !colourMenu.visible
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "The color of models that have none of their own")
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            onClicked: colourMenu.popup(colourButton, 0, colourButton.height)

            ModelColourMenu {
                id: colourMenu
            }
        }
        QQC2.ToolButton {
            id: freeButton
            objectName: "model3dFree"
            // The still of the software fallback does not turn: an action that does nothing is not shown.
            visible: root.threeD
            display: root.buttonDisplay
            checkable: true
            focusPolicy: Qt.NoFocus
            icon.name: "transform-rotate"
            text: L10n.i18nc("@action:button", "Free Rotation")
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Rotate freely instead of around the vertical axis")
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        }
        // "Open in Slicer": a click on the main part starts the last slicer used (OrcaSlicer by
        // default) when it is installed, else it opens the list; the arrow opens the list. The
        // menu is made with the header (a menu made when it opens can stay tiny, qml.md).
        Row {
            id: slicerButtons
            spacing: 0

            QQC2.ToolButton {
                id: slicerButton
                objectName: "model3dSlicer"
                focusPolicy: Qt.NoFocus
                icon.name: "document-open"
                text: root.slicerText
                // A narrow window: the icon alone, the name is the accessible name.
                display: root.buttonDisplay
                Accessible.name: root.slicerText
                QQC2.ToolTip.visible: hovered && !slicerMenu.visible
                QQC2.ToolTip.text: root.slicerReady ? L10n.i18nc("@info:tooltip", "Open this model in the slicer program") : L10n.i18nc("@info:tooltip", "Choose a slicer program to open this model in")
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: {
                    if (root.slicerReady) {
                        MediaActions.openInSlicer(root.attachment, root.name, root.slicerId);
                    } else {
                        slicerMenu.popup(slicerButton, 0, slicerButton.height);
                    }
                }
            }
            QQC2.ToolButton {
                id: slicerArrow
                objectName: "model3dSlicerMenu"
                focusPolicy: Qt.NoFocus
                icon.name: "arrow-down"
                display: QQC2.AbstractButton.IconOnly
                text: L10n.i18nc("@action:button", "Choose a Slicer")
                Accessible.name: text
                QQC2.ToolTip.visible: hovered && !slicerMenu.visible
                QQC2.ToolTip.text: text
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: slicerMenu.popup(slicerButton, 0, slicerButton.height)
            }

            QQC2.Menu {
                id: slicerMenu
                objectName: "model3dSlicers"
                popupType: QQC2.Popup.Item
                // The table is fixed: four entries made once. (A Repeater over the list itself would
                // make its entries again at every refresh and put them after "Other Application…".)
                Repeater {
                    model: 4
                    QQC2.MenuItem {
                        required property int index
                        readonly property var row: root.slicerRows[index]
                        objectName: "model3dSlicer-" + row.id
                        icon.name: "document-open"
                        // A disabled entry gets no hover, so the reason is in its text. Product
                        // names are not translated.
                        text: row.installed ? row.name : L10n.i18nc("@action:inmenu, %1 the name of a slicer program", "%1 (not installed)", row.name)
                        enabled: row.installed
                        onTriggered: MediaActions.openInSlicer(root.attachment, root.name, row.id)
                    }
                }
                QQC2.MenuSeparator {}
                QQC2.MenuItem {
                    objectName: "model3dSlicer-other"
                    icon.name: "document-open"
                    text: L10n.i18nc("@action:inmenu", "Other Application…")
                    onTriggered: MediaActions.openInSlicer(root.attachment, root.name, "other")
                }
            }
        }
        QQC2.ToolButton {
            objectName: "model3dSave"
            visible: root.saveOffered
            focusPolicy: Qt.NoFocus
            icon.name: "document-save-as"
            text: L10n.i18nc("@action:button", "Save As…")
            display: QQC2.AbstractButton.IconOnly
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Save As… (Ctrl+S)")
            onClicked: root.saveRequested()
        }
        QQC2.ToolButton {
            objectName: "model3dClose"
            focusPolicy: Qt.NoFocus
            icon.name: "dialog-close"
            text: L10n.i18nc("@action:button", "Close")
            display: QQC2.AbstractButton.IconOnly
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: text
            onClicked: root.closeRequested()
        }
    }
}
