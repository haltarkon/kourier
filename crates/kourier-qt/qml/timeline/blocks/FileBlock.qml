pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// A file or voice note (§8.6): a round action button (download, progress, retry, open, play),
// the name and details. Files open with the desktop's default application from a tmpfs copy
// (types that are not pictures, sound or video ask first); voice notes play in place through
// Qt Multimedia, loaded on the first play.
RowLayout {
    id: root

    required property var block
    required property color ink
    required property color subtleInk

    readonly property real attachment: root.block.attachment ?? 0
    readonly property bool voice: root.block.voice
    readonly property string mediaState: root.block.state
    readonly property bool available: root.mediaState === "local"
    readonly property bool busy: root.mediaState === "downloading"
    // Gone here, and Download asks the user's phone to upload it again (the core decides it).
    readonly property bool ask: root.block.ask === true
    // What a click fetches: a download, a retry, or the question to the phone.
    readonly property bool downloadable: root.mediaState === "remote" || root.mediaState === "failed" || root.ask
    // Voice playback: the player exists once asked for (and stays while the row lives).
    property bool playRequested: false
    readonly property var player: playerLoader.item
    // What activating the block does now: "download", "play", "pause", "open", or "" (nothing:
    // it is downloading, or no longer available). The row's keyboard action and its menu
    // entry follow it.
    readonly property string mediaAction: {
        if (root.attachment <= 0) {
            return "";
        }
        if (!root.available) {
            return root.downloadable ? "download" : "";
        }
        if (root.voice) {
            return root.player && root.player.playing ? "pause" : "play";
        }
        return "open";
    }

    spacing: Kirigami.Units.largeSpacing

    // The same as a click on the round button.
    function activate(): void {
        root.act();
    }

    function act(): void {
        if (root.attachment <= 0) {
            return;
        }
        if (!root.available) {
            if (root.downloadable) {
                App.downloadAttachment(root.attachment);
            }
        } else if (root.voice) {
            if (root.player) {
                root.player.toggle();
            } else {
                root.playRequested = true;
            }
        } else {
            MediaActions.open(root.attachment, root.block.name, root.block.media);
        }
    }

    QQC2.AbstractButton {
        id: action
        implicitWidth: Kirigami.Units.gridUnit * 2.2
        implicitHeight: implicitWidth
        // A quiet disc stays enabled, because Kirigami.Icon draws the glyph of a disabled item
        // dimmed and that would undo its contrast; it takes no focus and no hover, says it is
        // not a button, and a click does nothing (`act()` looks at what can be fetched).
        enabled: root.attachment > 0
        hoverEnabled: !action.quiet
        focusPolicy: action.quiet ? Qt.NoFocus : Qt.StrongFocus
        Accessible.role: action.quiet ? Accessible.StaticText : Accessible.Button
        text: {
            if (root.busy) {
                return L10n.i18nc("@info:tooltip", "Downloading");
            }
            if (root.ask) {
                return L10n.i18nc("@action:button", "Download");
            }
            switch (root.mediaState) {
            case "remote":
                return L10n.i18nc("@action:button", "Download");
            case "asking":
                return L10n.i18nc("@info", "Asking your phone…");
            case "noanswer":
            case "nomessage":
            case "nofile":
                return MediaActions.askNote(root.mediaState);
            case "failed":
                return L10n.i18nc("@action:button", "Retry Download");
            case "expired":
                return L10n.i18nc("@info", "No longer available");
            case "unavailable":
                return L10n.i18nc("@info", "Not available on this device");
            default:
                return root.voice ? L10n.i18nc("@action:button", "Play") : L10n.i18nc("@action:button", "Open");
            }
        }
        QQC2.ToolTip.visible: hovered
        QQC2.ToolTip.text: text
        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
        onClicked: root.act()

        // A downloaded file shows its type's (colored) icon on a plain disc; actions are glyphs
        // on the accent, in the theme's selection text color, adjusted until they can be read.
        readonly property bool typeIcon: root.available && !root.voice
        // Nothing to click: waiting for the phone, or a file that cannot be had ("Not on your
        // phone", "No answer from your phone", "No longer available"). Such a disc is a plain
        // one like the downloaded file's, framed, with a quiet glyph that can be read on it:
        // the accent at half opacity under the accent's text colour all but vanished in the
        // dark theme.
        readonly property bool quiet: root.attachment > 0 && !root.available && !root.busy && !root.downloadable
        readonly property color fill: {
            if (action.typeIcon || action.quiet) {
                return Kirigami.Theme.backgroundColor;
            }
            return action.hovered ? Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.highlightColor, Kirigami.Theme.textColor, 0.12) : Kirigami.Theme.highlightColor;
        }
        // On the accent fill: the theme's own text colour for its highlight (white on Breeze's
        // blue; user decision, 2026-10-08: the theme's pairing, not a computed contrast).
        readonly property color glyph: {
            if (action.typeIcon) {
                return Kirigami.Theme.textColor;
            }
            return action.quiet ? Style.secondary(Kirigami.Theme.textColor, action.fill) : Kirigami.Theme.highlightedTextColor;
        }

        background: Rectangle {
            radius: width / 2
            color: action.fill
            border.width: action.quiet ? 1 : 0
            border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
            opacity: action.enabled ? 1 : 0.5
        }
        contentItem: Item {
            Kirigami.Icon {
                anchors.centerIn: parent
                visible: !root.busy
                width: Kirigami.Units.iconSizes.smallMedium
                height: width
                source: {
                    if (root.ask) {
                        return "download";
                    }
                    switch (root.mediaState) {
                    case "remote":
                        return "download";
                    case "asking":
                        return "appointment-soon";
                    case "noanswer":
                    case "nomessage":
                    case "nofile":
                        return "dialog-cancel";
                    case "failed":
                        return "view-refresh";
                    case "expired":
                    case "unavailable":
                        return "dialog-cancel";
                    default:
                        if (root.voice) {
                            return root.player && root.player.playing ? "media-playback-pause" : "media-playback-start";
                        }
                        return root.block.icon;
                    }
                }
                color: action.glyph
                isMask: !action.typeIcon
            }
            QQC2.Label {
                anchors.centerIn: parent
                visible: root.busy
                text: Math.round(root.block.progress * 100) + "%"
                color: action.glyph
                font: Kirigami.Theme.smallFont
            }
        }
    }

    ColumnLayout {
        spacing: 0
        Layout.fillWidth: true

        // Voice notes: the waveform fills with the playback position.
        Row {
            id: wave
            visible: root.voice
            spacing: 2
            Layout.preferredHeight: Kirigami.Units.gridUnit
            readonly property real played: root.player ? root.player.progress : 0
            Repeater {
                id: bars
                model: [3, 5, 8, 6, 10, 7, 4, 9, 12, 8, 5, 7, 11, 6, 4, 8, 10, 5, 3, 6, 9, 7, 4, 3]
                delegate: Rectangle {
                    required property int modelData
                    required property int index
                    width: 2
                    radius: 1
                    height: Math.max(3, modelData * Kirigami.Units.gridUnit / 12)
                    anchors.verticalCenter: parent.verticalCenter
                    color: root.ink
                    opacity: (index + 0.5) / bars.count <= wave.played ? 1 : 0.5
                }
            }
        }
        QQC2.Label {
            visible: !root.voice
            text: root.block.name
            color: root.ink
            font: Style.styled(Kirigami.Theme.defaultFont, {
                "weight": Font.DemiBold
            })
            elide: Text.ElideMiddle
            textFormat: Text.PlainText
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 14
        }
        QQC2.Label {
            text: {
                if (root.voice && root.player && root.player.duration > 0) {
                    return root.player.label;
                }
                if (root.voice && root.player && root.player.failed) {
                    return root.block.detail + " · " + L10n.i18nc("@info", "could not be played");
                }
                if (root.busy) {
                    return root.block.detail + " · " + L10n.i18nc("@info", "downloading");
                }
                if (root.mediaState === "failed") {
                    return root.block.detail + " · " + L10n.i18nc("@info", "download failed");
                }
                if (root.mediaState === "expired") {
                    return L10n.i18nc("@info", "No longer available");
                }
                if (root.mediaState === "unavailable") {
                    return L10n.i18nc("@info", "Not available on this device");
                }
                if (root.mediaState === "asking") {
                    return L10n.i18nc("@info", "Asking your phone…");
                }
                const note = MediaActions.askNote(root.mediaState);
                if (note !== "") {
                    return note;
                }
                return root.block.detail;
            }
            color: root.subtleInk
            font: Kirigami.Theme.smallFont
            textFormat: Text.PlainText
            // A note carries the way on ("Open Signal on your phone and try again."): it
            // wraps inside the bubble at every width, as long as the name above it.
            wrapMode: Text.Wrap
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 14
        }
    }

    // Qt Multimedia only once a voice note is played: the player is loaded by its file name,
    // so its import (the module, and with it PipeWire and the codecs) is not resolved when
    // this block's own type is, which is with the first chat (§1.3: idle memory).
    Loader {
        id: playerLoader
        visible: false
        asynchronous: true
        onStatusChanged: {
            if (playerLoader.status === Loader.Error) {
                console.warn("the voice player could not be loaded");
            }
        }
    }
    // Where the player is loaded from (tests check that it is there to load).
    readonly property url playerUrl: Qt.resolvedUrl("VoicePlayer.qml")
    readonly property bool wantsPlayer: root.voice && root.playRequested && root.available
    onWantsPlayerChanged: {
        if (root.wantsPlayer && playerLoader.status === Loader.Null) {
            playerLoader.setSource(root.playerUrl, {
                "attachment": root.attachment
            });
        }
    }
}
