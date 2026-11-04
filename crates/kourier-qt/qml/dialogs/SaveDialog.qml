import QtQuick
import QtQuick.Dialogs
import app.kourier

// The "Save As" dialog (the KDE dialog or the portal through the platform theme), loaded by URL
// on first use like the attachment picker. One file: `suggested` is the whole `file:` URL it starts
// at (the start folder and the offered name, built in Rust, never joined here), no name filter, the
// dialog's own question about replacing. `folder`: it picks one folder instead ("Save All…");
// `startFolder` is where it starts. `picked` carries the file's or the folder's URL; `attachments`
// says which files that answer is for.
//
// A window test replaces this file with a stand-in that has the same properties and signals (the
// native dialog cannot be driven offscreen): the window loads whatever `App.saveDialogSource()`
// names, which a debug build lets the environment choose.
Item {
    id: root

    property bool folder: false
    property url suggested
    property url startFolder
    // The attachments (their numbers) this dialog was opened for: its answer saves these.
    property var attachments: []
    // The dialog opens as soon as it exists.
    property bool autoOpen: true

    signal picked(url target)
    signal closed

    FileDialog {
        id: fileDialog
        title: L10n.i18nc("@title:window", "Save As")
        fileMode: FileDialog.SaveFile
        currentFile: root.suggested
        onAccepted: {
            root.picked(fileDialog.selectedFile);
            root.closed();
        }
        onRejected: root.closed()
    }

    FolderDialog {
        id: folderDialog
        title: L10n.i18nc("@title:window", "Save All")
        currentFolder: root.startFolder
        onAccepted: {
            root.picked(folderDialog.selectedFolder);
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
            fileDialog.open();
        }
    }
}
