import QtQuick
import app.kourier

// A plaintext copy of a downloaded attachment in the tmpfs media cache, for AnimatedImage and
// Qt Multimedia (they cannot read image providers, §8.6). Asks the core while `active`; the
// copy is released when this object goes or `active` turns off. AnimatedImage gets `movieUrl`,
// never `url`: it detects formats by content with every installed image plugin, so only what
// the app found to be a GIF or WebP goes to it.
QtObject {
    id: root

    property real attachment: 0
    property bool active: false
    // `file://` URL once the copy is ready.
    property string url: ""
    // The copy is a GIF or WebP by content.
    property bool movie: false
    // The URL for AnimatedImage: empty unless the copy is a movie.
    readonly property string movieUrl: root.movie ? root.url : ""
    // The copy is there but nothing AnimatedImage may play.
    readonly property bool notMovie: root.url !== "" && !root.movie
    property bool failed: false
    // The pending or answered request (0: none).
    property real token: 0

    function request(): void {
        if (root.attachment > 0 && root.active && root.token === 0) {
            root.failed = false;
            root.token = App.openMedia(root.attachment);
        }
    }

    function release(): void {
        if (root.token !== 0) {
            App.releaseMedia(root.token);
        }
        root.token = 0;
        root.url = "";
        root.movie = false;
    }

    onActiveChanged: root.active ? root.request() : root.release()
    onAttachmentChanged: {
        root.release();
        root.request();
    }
    Component.onCompleted: root.request()
    Component.onDestruction: root.release()

    property Connections answers: Connections {
        target: App
        function onMediaOpened(token: real, url: string, movie: bool): void {
            if (token === root.token) {
                root.movie = movie;
                root.url = url;
            }
        }
        function onMediaFailed(token: real): void {
            if (token === root.token) {
                root.token = 0;
                root.failed = true;
            }
        }
    }
}
