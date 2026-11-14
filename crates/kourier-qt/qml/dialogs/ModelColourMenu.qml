import QtQuick
import QtQuick.Controls as QQC2
import app.kourier

// The colours a 3D model without colours of its own can take: "Use Accent Color" (the default,
// the desktop's) and a few RGB ones. Used by the viewer's toolbar button and by Settings; both
// write the ONE preference `model_colour` through `App.settings.modelColour`.
QQC2.Menu {
    id: menu
    objectName: "model3dColourMenu"

    popupType: QQC2.Popup.Item

    // One colour ("": the accent).
    component ColourItem: QQC2.MenuItem {
        property string value

        icon.source: value === "" ? "" : "image://kourierart/swatch/" + value.slice(1)
        icon.color: "transparent"
        autoExclusive: true
        checkable: true
        checked: App.settings.modelColour === value
        onTriggered: {
            App.settings.modelColour = value;
            checked = Qt.binding(() => App.settings.modelColour === value);
        }
    }

    ColourItem {
        objectName: "model3dColour-accent"
        text: L10n.i18nc("@item:inmenu 3D model color: the desktop's accent color", "Use Accent Color")
    }
    QQC2.MenuSeparator {}
    ColourItem {
        objectName: "model3dColour-3a76f0"
        value: "#3a76f0"
        text: L10n.i18nc("@item:inmenu 3D model color", "Blue")
    }
    ColourItem {
        objectName: "model3dColour-1da851"
        value: "#1da851"
        text: L10n.i18nc("@item:inmenu 3D model color", "Green")
    }
    ColourItem {
        objectName: "model3dColour-14a3a3"
        value: "#14a3a3"
        text: L10n.i18nc("@item:inmenu 3D model color", "Teal")
    }
    ColourItem {
        objectName: "model3dColour-8b5cf6"
        value: "#8b5cf6"
        text: L10n.i18nc("@item:inmenu 3D model color", "Violet")
    }
    ColourItem {
        objectName: "model3dColour-e0529c"
        value: "#e0529c"
        text: L10n.i18nc("@item:inmenu 3D model color", "Pink")
    }
    ColourItem {
        objectName: "model3dColour-dc4c3e"
        value: "#dc4c3e"
        text: L10n.i18nc("@item:inmenu 3D model color", "Red")
    }
    ColourItem {
        objectName: "model3dColour-ef8a17"
        value: "#ef8a17"
        text: L10n.i18nc("@item:inmenu 3D model color", "Orange")
    }
    ColourItem {
        objectName: "model3dColour-d9b310"
        value: "#d9b310"
        text: L10n.i18nc("@item:inmenu 3D model color", "Yellow")
    }
    ColourItem {
        objectName: "model3dColour-b8c0c4"
        value: "#b8c0c4"
        text: L10n.i18nc("@item:inmenu 3D model color", "Light Gray")
    }
    ColourItem {
        objectName: "model3dColour-7f8c8d"
        value: "#7f8c8d"
        text: L10n.i18nc("@item:inmenu 3D model color", "Gray")
    }
}
