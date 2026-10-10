import QtQuick
import QtMultimedia
import app.kourier

// Plays a voice note from its tmpfs copy (Qt Multimedia needs a file URL, §8.6). Starts as soon
// as the copy is ready; the copy goes with this item.
Item {
    id: root

    required property real attachment

    // The tmpfs copy (`file://`), once ready.
    readonly property string url: copy.url
    readonly property bool playing: player.playbackState === MediaPlayer.PlayingState
    readonly property real duration: player.duration
    readonly property real progress: player.duration > 0 ? player.position / player.duration : 0
    // "0:03 / 0:07".
    readonly property string label: root.clock(player.position) + " / " + root.clock(player.duration)

    function clock(ms: real): string {
        const s = Math.floor(ms / 1000);
        const sec = s % 60;
        return Math.floor(s / 60) + ":" + (sec < 10 ? "0" : "") + sec;
    }

    // The copy could not be made (the play button asks again).
    readonly property bool failed: copy.failed

    function toggle(): void {
        if (copy.url === "") {
            // Still on its way, or failed: ask again (a pending request is not repeated).
            copy.request();
            return;
        }
        if (root.playing) {
            player.pause();
        } else {
            player.play();
        }
    }

    MediaCopy {
        id: copy
        attachment: root.attachment
        active: true
        onUrlChanged: {
            if (copy.url !== "") {
                player.source = copy.url;
                player.play();
            }
        }
    }

    MediaPlayer {
        id: player
        audioOutput: AudioOutput {}
        onMediaStatusChanged: {
            if (player.mediaStatus === MediaPlayer.EndOfMedia) {
                player.stop();
            }
        }
    }
}
