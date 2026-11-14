import QtQuick
import QtQuick.Dialogs
import app.kourier

// "Custom Color": the system's color chooser for a filter's own color. It is loaded by URL when
// "Custom…" is first chosen (`FilterDialog`), so `QtQuick.Dialogs` is not needed before.
ColorDialog {
    id: root

    title: L10n.i18nc("@title:window", "Custom Color")
    // The chosen color as `#rrggbb` (opaque: a filter's alpha is chosen per background).
    signal chosen(string color)
    // (Built from the channels: a color with alpha prints as `#aarrggbb`, so a cut of its text would
    // be the alpha and two channels.)
    onAccepted: root.chosen(root.rgbOf(root.selectedColor))

    function rgbOf(color: color): string {
        const hex = v => ("0" + Math.round(v * 255).toString(16)).slice(-2);
        return "#" + hex(color.r) + hex(color.g) + hex(color.b);
    }
}
