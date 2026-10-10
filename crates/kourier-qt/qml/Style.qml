pragma Singleton

import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// Shared constants and color helpers of the Kourier UI.
QtObject {
    id: style

    readonly property string iconBase: "qrc:/qt/qml/app/kourier/icons/"

    // Bubble geometry.
    readonly property real bubbleRadius: Kirigami.Units.cornerRadius * 3
    readonly property real bubbleRadiusTight: Kirigami.Units.cornerRadius
    readonly property real bubbleMaxWidth: Kirigami.Units.gridUnit * 26
    readonly property real avatarSize: Kirigami.Units.gridUnit * 1.8
    readonly property real listAvatarSize: Kirigami.Units.gridUnit * 2.6
    readonly property real imageMaxWidth: Kirigami.Units.gridUnit * 16
    readonly property real stickerSize: Kirigami.Units.gridUnit * 8
    // The conversation list in compact mode (Settings → General).
    readonly property real compactAvatarSize: Kirigami.Units.gridUnit * 1.8

    // A high-contrast color scheme (its frame contrast is full: 1 in ours, the Breeze schemes
    // have 0.2). Whatever a Breeze look gets by with, a faint tint or a grey mix, needs a
    // solid line or the plain text color here. The one switch: nothing below may change a
    // Breeze picture.
    readonly property bool highContrast: Kirigami.Theme.frameContrast >= 0.8

    // Text on a bubble must reach this contrast against it (WCAG AA for normal text).
    readonly property real minContrast: 4.5
    // How much of the accent color tints our own bubbles (§14.1: the normal background tinted
    // with the accent, normal text on it, instead of a solid accent bubble with white text).
    readonly property real ownTint: 0.2
    // How much of a picture an overlay on it (the play and download disc, the size or duration
    // pill, the "+4" of an album) covers: the background of the Complementary color set at this
    // opacity, with that set's text on it.
    readonly property real overlayOpacity: 0.6

    // URL of a bundled icon (`icons/<name>.svg`).
    function icon(name: string): string {
        return iconBase + name + ".svg";
    }

    // The application's logo, for `Kirigami.Icon.source`: the icon theme's own icon of the
    // application id when the theme has one (an installed Kourier, so an icon theme can restyle
    // it), else the bundled file (a run that is not installed has no such icon). QML cannot tell
    // the two apart (`Kirigami.Icon.valid` is true for a missing name): the application asks the
    // theme once, the first time the logo is used.
    readonly property string appIcon: {
        const themed = App.themedAppIcon();
        return themed !== "" ? themed : style.icon("kourier");
    }

    // Two pleasant colors derived from a string (image placeholders before download).
    function hueOf(text: string): real {
        let h = 0;
        for (let i = 0; i < text.length; ++i) {
            h = (h * 31 + text.charCodeAt(i)) % 360;
        }
        return h / 360;
    }

    // `text` for a label that guesses its format (Kirigami's InlineMessage, tooltips, any
    // label without `textFormat: Text.PlainText`): such a label renders what looks like markup
    // as rich text, and a text with anything from outside in it (a name, a reaction, a reason
    // the network gave) must never become markup: an `<img>` would be fetched from the network
    // (§9.4). Without angle brackets nothing in it can be read as a tag.
    function plain(text: string): string {
        return text.replace(/</g, "\u2039").replace(/>/g, "\u203a");
    }

    // The bubble of an own message: `background` tinted with the accent color.
    function ownBubble(background: color, accent: color): color {
        return Kirigami.ColorUtils.tintWithAlpha(background, accent, style.ownTint);
    }

    // The bubble of someone else's message: a step away from `background` towards the text.
    function peerBubble(background: color, text: color): color {
        return Kirigami.ColorUtils.linearInterpolation(background, text, 0.075);
    }

    // A frame (the border of a bubble, a pill or a tile) between `background` and `text` at the
    // theme's frame contrast: a faint line in Breeze, solid in the high-contrast schemes. The
    // caller passes `Kirigami.Theme.frameContrast` of its own item: read from this singleton,
    // which is not an item, the theme would not follow the item's color set.
    function frame(background: color, text: color, contrast: real): color {
        return Kirigami.ColorUtils.linearInterpolation(background, text, contrast);
    }

    // The font `base` (a theme font) with another weight, italics, underline or letter spacing
    // (all in `options`), or scaled (`scale`).
    // A label that sets `font.weight` or `font.italic` by itself loses the style's binding to
    // the theme font, and then keeps the size it was created with when the font setting
    // changes; a label bound to `Style.styled(Kirigami.Theme.defaultFont, …)` follows it.
    function styled(base: font, options: var): font {
        return Qt.font({
            "family": base.family,
            "pointSize": base.pointSize * (options.scale ?? 1),
            "weight": options.weight ?? base.weight,
            "italic": options.italic ?? base.italic,
            "underline": options.underline ?? base.underline,
            "letterSpacing": options.letterSpacing ?? base.letterSpacing
        });
    }

    // Secondary text (time, "edited", details) on `background`: the text color, quieter, but
    // never below the minimum contrast. Not the theme's disabled-text role, which is for
    // controls that are disabled and can be much fainter. In a high-contrast scheme it is
    // not quieter at all: the text color itself.
    function secondary(text: color, background: color): color {
        if (style.highContrast) {
            return text;
        }
        return style.readable(Kirigami.ColorUtils.linearInterpolation(background, text, 0.72), background);
    }

    // sRGB channel (0 … 1) to linear light.
    function linear(channel: real): real {
        return channel <= 0.04045 ? channel / 12.92 : Math.pow((channel + 0.055) / 1.055, 2.4);
    }

    // Relative luminance of a color (WCAG 2).
    function luminance(c: color): real {
        return 0.2126 * style.linear(c.r) + 0.7152 * style.linear(c.g) + 0.0722 * style.linear(c.b);
    }

    // Contrast ratio of two colors (1 … 21).
    function contrast(a: color, b: color): real {
        const la = style.luminance(a);
        const lb = style.luminance(b);
        return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
    }

    // `c` as it can be read on `background`: unchanged when its contrast is enough, else the
    // same hue and saturation made darker (on a light background) or lighter (on a dark one)
    // just far enough. Sender names take their color from the avatar palette, links and
    // accents from the theme: none of those are chosen for the bubble they end up on.
    function readable(c: color, background: color): color {
        if (style.contrast(c, background) >= style.minContrast) {
            return c;
        }
        const darker = style.luminance(background) > 0.18;
        const end = darker ? 0 : 1;
        let from = c.hslLightness;
        let to = end;
        let best = darker ? Qt.rgba(0, 0, 0, 1) : Qt.rgba(1, 1, 1, 1);
        // The lightness nearest to the color's own that reaches the contrast (the contrast
        // grows towards `end`).
        for (let i = 0; i < 8; ++i) {
            const mid = (from + to) / 2;
            const candidate = Qt.hsla(c.hslHue, c.hslSaturation, mid, 1);
            if (style.contrast(candidate, background) >= style.minContrast) {
                best = candidate;
                to = mid;
            } else {
                from = mid;
            }
        }
        return best;
    }
}
