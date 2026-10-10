import QtQuick
import QtQuick.Dialogs
import app.kourier

// The attachment picker (the KDE dialog or the portal through the platform theme). Where the
// conversation takes only some kinds of files (`kinds`, the timeline's `attachKinds`), the
// dialog starts on a filter for them; "All files" is always the other choice. The filter goes
// by file name and is only a convenience: what a file is, and whether the conversation takes
// it, the core reads from its content (a picture without a file name extension is a picture),
// and what it does not take is marked in the composer's list.
//
// `folder`: the dialog picks one folder instead (a file dialog cannot offer files and folders
// at once). What is done with it, the composer asks afterwards.
Item {
    id: root

    property string kinds: ""
    property bool folder: false
    // The dialog opens as soon as it exists (a test that only asks for the filters turns
    // that off).
    property bool autoOpen: true

    signal picked(list<url> urls)
    signal closed

    readonly property var patterns: ({
            "image": "*.jpg *.jpeg *.png *.webp *.heic *.heif *.avif *.bmp *.jxl",
            "gif": "*.gif",
            "video": "*.mp4 *.m4v *.mov *.webm *.mkv",
            "audio": "*.mp3 *.ogg *.oga *.opus *.m4a *.aac *.flac *.wav"
        })

    function filters(): list<string> {
        const kinds = root.kinds.split(",");
        if (kinds.indexOf("file") >= 0) {
            return [];
        }
        const globs = [];
        for (const k of kinds) {
            if (root.patterns[k]) {
                globs.push(root.patterns[k]);
            }
        }
        return globs.length > 0 ? [L10n.i18nc("@item:inlistbox file type", "Media files") + " (" + globs.join(" ") + ")", L10n.i18nc("@item:inlistbox file type", "All files") + " (*)"] : [];
    }

    FileDialog {
        id: dialog
        title: L10n.i18nc("@title:window", "Attach Files")
        fileMode: FileDialog.OpenFiles
        nameFilters: root.filters()
        onAccepted: {
            root.picked(dialog.selectedFiles);
            root.closed();
        }
        onRejected: root.closed()
    }

    FolderDialog {
        id: folderDialog
        title: L10n.i18nc("@title:window", "Attach a Folder")
        onAccepted: {
            root.picked([folderDialog.selectedFolder]);
            root.closed();
        }
        onRejected: root.closed()
    }

    Component.onCompleted: {
        if (!root.autoOpen) {
            return;
        }
        if (root.folder) {
            folderDialog.open();
        } else {
            dialog.open();
        }
    }
}
