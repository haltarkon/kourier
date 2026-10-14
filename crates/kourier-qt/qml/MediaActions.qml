pragma Singleton

import QtQuick

// What media blocks deep in the timeline ask the window to do: show a picture in the viewer,
// or open a file with another application after the user confirmed (§8.6, §9.4).
QtObject {
    // Show `url` (a full-size `image://kourier/att/…`, or for animated media the attachment
    // only) in the image viewer; `media` = opening it in another application from there needs
    // no confirmation. `message`: the message it belongs to (0 when it has none): the viewer
    // then follows that message's row.
    signal viewRequested(real attachment, string url, bool animated, bool media, real message)
    // Show the pictures of an album in the viewer, starting at `index`. Each of `items` has
    // `attachment`, `url` (full size), `animated`, `media`, `kind`, `state`, `progress` and
    // `placeholder`; `message` is the message they belong to (0 for none).
    signal albumRequested(var items, int index, real message)
    // Open a downloaded file with the desktop's default application; `media` = a well-known
    // picture, sound or video type (no confirmation), else the window asks first.
    signal openRequested(real attachment, string name, bool media)

    function view(attachment: real, url: string, animated: bool, media: bool, message: real): void {
        viewRequested(attachment, url, animated, media, message);
    }

    function viewAlbum(items: var, index: int, message: real): void {
        albumRequested(items, index, message);
    }

    // The note of an attachment the user's phone was asked to upload again and did not
    // ("noanswer", "nomessage", "nofile"); "" for any other state. While asking, the block says
    // "Asking your phone…" itself.
    function askNote(state: string): string {
        switch (state) {
        case "noanswer":
            return L10n.i18nc("@info", "Your phone did not answer. Open Signal on your phone and try again.");
        case "nomessage":
            return L10n.i18nc("@info", "Your phone no longer has this message.");
        case "nofile":
            return L10n.i18nc("@info", "Not on your phone either.");
        default:
            return "";
        }
    }

    // The same endings in a few words, for a pill on a picture or a small tile that cuts what
    // does not fit (the whole sentence is its tooltip and its accessible description); ""
    // for any other state.
    function askShortNote(state: string): string {
        switch (state) {
        case "noanswer":
            return L10n.i18nc("@info", "No answer from your phone");
        case "nomessage":
        case "nofile":
            return L10n.i18nc("@info", "Not on your phone");
        default:
            return "";
        }
    }

    function open(attachment: real, name: string, media: bool): void {
        openRequested(attachment, name, media);
    }
}
