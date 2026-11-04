pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// "All Media and Files" (people pages P4; the level of the details panel that "Show All" of a
// page's section opens): a grid of every picture, video and GIF of the chat, newest first, and the
// list of its files, in two tabs. Both are Rust list models (`App.mediaGrid`, `App.mediaFiles`)
// that grow page by page: the next page is asked for when the view comes near its end, and the
// rows already there never move (a picture that arrives or is deleted is an insert or a remove).
// A square does what a click on the picture in the chat does (the one viewer, a video in another
// application, a download); a file row is the timeline's own file block, with its round action
// button. The panel owns the plate, the close button and the keys; this item is kept alive
// (hidden) while a page lies on top of it.
Item {
    id: root

    // The level is in the panel's stack (shown or covered): only then are the rows kept.
    property bool active: false
    // The panel sits beside the timeline (a line on its leading edge), or covers the chat.
    property bool beside: false
    // Room the panel's close button takes at the trailing end of the title row.
    property real trailingGap: 0
    // Both lists have their first answer.
    readonly property bool loaded: App.mediaGrid.loaded && App.mediaFiles.loaded
    // The tab: 0 Media, 1 Files. A chat with files and no pictures opens on its files until the
    // user chooses.
    property int tab: 0
    property bool tabChosen: false
    // How many rows each list had when the next page was last asked for (one ask per length).
    property int askedGrid: -1
    property int askedFiles: -1

    // The level's own back arrow.
    signal backRequested

    // Forgets the tab and the places: the same item serves every later open.
    function reset(): void {
        root.tab = 0;
        root.tabChosen = false;
        root.askedGrid = -1;
        root.askedFiles = -1;
        grid.contentY = 0;
        files.contentY = 0;
    }

    // The keyboard returns to the level: the tab bar (the lists take it with a key).
    function focusOpener(): void {
        bar.forceActiveFocus();
    }

    // The next page of a list when its view is near its end (or does not fill itself).
    function fill(): void {
        if (!root.active) {
            return;
        }
        if (root.tab === 0 && App.mediaGrid.more && root.askedGrid !== App.mediaGrid.count && grid.contentHeight - grid.contentY - grid.height < grid.height) {
            root.askedGrid = App.mediaGrid.count;
            App.moreChatMedia("media");
        }
        if (root.tab === 1 && App.mediaFiles.more && root.askedFiles !== App.mediaFiles.count && files.contentHeight - files.contentY - files.height < files.height) {
            root.askedFiles = App.mediaFiles.count;
            App.moreChatMedia("files");
        }
    }

    // No pictures but files: open on the files, once.
    function pickTab(): void {
        if (!root.tabChosen && App.mediaGrid.loaded && App.mediaGrid.count === 0 && App.mediaGrid.other) {
            root.tab = 1;
        }
    }

    // What the two lists say, read as properties of this item so that a change of one runs `fill` or
    // `pickTab` (a `Connections` on a model is a `QObject` binding the linter does not take).
    readonly property int gridCount: App.mediaGrid.count
    readonly property bool gridMore: App.mediaGrid.more
    readonly property bool gridLoaded: App.mediaGrid.loaded
    readonly property bool gridOther: App.mediaGrid.other
    readonly property int filesCount: App.mediaFiles.count
    readonly property bool filesMore: App.mediaFiles.more
    onGridCountChanged: Qt.callLater(root.fill)
    onGridMoreChanged: Qt.callLater(root.fill)
    onGridLoadedChanged: root.pickTab()
    onGridOtherChanged: root.pickTab()
    onFilesCountChanged: Qt.callLater(root.fill)
    onFilesMoreChanged: Qt.callLater(root.fill)
    onTabChanged: Qt.callLater(root.fill)

    ColumnLayout {
        anchors.fill: parent
        anchors.leftMargin: root.beside ? 1 : 0
        spacing: Kirigami.Units.smallSpacing

        RowLayout {
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.largeSpacing
            Layout.topMargin: Kirigami.Units.largeSpacing
            QQC2.ToolButton {
                objectName: "mediaBack"
                icon.name: LayoutMirroring.enabled ? "go-next" : "go-previous"
                display: QQC2.AbstractButton.IconOnly
                text: L10n.i18nc("@action:button", "Back")
                Accessible.name: text
                onClicked: root.backRequested()
            }
            QQC2.Label {
                objectName: "mediaTitle"
                Layout.fillWidth: true
                text: L10n.i18nc("@title:group the level with every picture and file of a chat", "All Media and Files")
                font: Style.styled(Kirigami.Theme.defaultFont, {
                    "weight": Font.DemiBold
                })
                elide: Text.ElideRight
                textFormat: Text.PlainText
            }
            // (The panel's close button stands over the trailing end of this row.)
            Item {
                implicitWidth: root.trailingGap
            }
        }

        QQC2.TabBar {
            id: bar
            objectName: "mediaTabs"
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.largeSpacing
            Layout.rightMargin: Kirigami.Units.largeSpacing
            currentIndex: root.tab
            QQC2.TabButton {
                objectName: "mediaTabMedia"
                text: L10n.i18nc("@title:tab the pictures and videos of a chat", "Media")
                onClicked: {
                    root.tabChosen = true;
                    root.tab = 0;
                }
            }
            QQC2.TabButton {
                objectName: "mediaTabFiles"
                text: L10n.i18nc("@title:tab the files of a chat", "Files")
                onClicked: {
                    root.tabChosen = true;
                    root.tab = 1;
                }
            }
        }

        StackLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            currentIndex: root.tab

            // ---- Media: the squares ----
            Item {
                GridView {
                    id: grid
                    objectName: "mediaGrid"
                    anchors.fill: parent
                    clip: true
                    leftMargin: Kirigami.Units.largeSpacing
                    rightMargin: Kirigami.Units.largeSpacing
                    topMargin: Kirigami.Units.smallSpacing
                    readonly property int columns: Math.max(2, Math.floor((width - leftMargin - rightMargin) / (Kirigami.Units.gridUnit * 7)))
                    cellWidth: Math.floor((width - leftMargin - rightMargin) / columns)
                    cellHeight: cellWidth
                    // Bound only while the level is in the panel's stack.
                    model: root.active ? App.mediaGrid : null
                    boundsBehavior: Flickable.StopAtBounds
                    activeFocusOnTab: true
                    keyNavigationEnabled: true
                    currentIndex: -1
                    onModelChanged: grid.currentIndex = -1
                    onContentYChanged: Qt.callLater(root.fill)
                    onHeightChanged: Qt.callLater(root.fill)
                    Accessible.role: Accessible.List
                    Accessible.name: L10n.i18nc("@title:tab the pictures and videos of a chat", "Media")
                    QQC2.ScrollBar.vertical: QQC2.ScrollBar {}
                    Keys.onReturnPressed: event => {
                        (grid.currentItem as MediaTile)?.activate();
                        event.accepted = true;
                    }
                    Keys.onEnterPressed: event => {
                        (grid.currentItem as MediaTile)?.activate();
                        event.accepted = true;
                    }
                    Keys.onSpacePressed: event => {
                        (grid.currentItem as MediaTile)?.activate();
                        event.accepted = true;
                    }
                    delegate: MediaTile {
                        width: grid.cellWidth - Kirigami.Units.smallSpacing
                        height: grid.cellHeight - Kirigami.Units.smallSpacing
                        objectName: "mediaCell"
                        // The viewer opens with the level's own rows and follows them (never the open timeline's).
                        gallery: "media"
                        focusPolicy: Qt.NoFocus
                        framed: grid.activeFocus && GridView.isCurrentItem
                    }
                }
                Kirigami.PlaceholderMessage {
                    objectName: "mediaNone"
                    anchors.centerIn: parent
                    width: parent.width - Kirigami.Units.gridUnit * 4
                    visible: App.mediaGrid.loaded && App.mediaGrid.count === 0
                    text: L10n.i18nc("@info:placeholder a chat has no pictures or videos", "No media")
                }
            }

            // ---- Files: the rows ----
            Item {
                ListView {
                    id: files
                    objectName: "mediaFilesList"
                    anchors.fill: parent
                    clip: true
                    topMargin: Kirigami.Units.smallSpacing
                    model: root.active ? App.mediaFiles : null
                    boundsBehavior: Flickable.StopAtBounds
                    activeFocusOnTab: true
                    keyNavigationEnabled: true
                    currentIndex: -1
                    onModelChanged: files.currentIndex = -1
                    onContentYChanged: Qt.callLater(root.fill)
                    onHeightChanged: Qt.callLater(root.fill)
                    Accessible.role: Accessible.List
                    Accessible.name: L10n.i18nc("@title:tab the files of a chat", "Files")
                    QQC2.ScrollBar.vertical: QQC2.ScrollBar {}
                    Keys.onReturnPressed: event => {
                        (files.currentItem as QQC2.ItemDelegate)?.clicked();
                        event.accepted = true;
                    }
                    Keys.onEnterPressed: event => {
                        (files.currentItem as QQC2.ItemDelegate)?.clicked();
                        event.accepted = true;
                    }
                    Keys.onSpacePressed: event => {
                        (files.currentItem as QQC2.ItemDelegate)?.clicked();
                        event.accepted = true;
                    }
                    delegate: QQC2.ItemDelegate {
                        id: fileRow
                        objectName: "mediaFile"
                        required property real attachment
                        required property real message
                        required property string mediaState
                        required property real progress
                        required property bool media
                        required property string fileName
                        required property string detail
                        required property string iconName
                        required property bool canSave
                        width: files.width
                        focusPolicy: Qt.NoFocus
                        highlighted: files.activeFocus && ListView.isCurrentItem
                        Accessible.name: fileRow.fileName + ", " + fileRow.detail
                        // The same as a click on the round button.
                        onClicked: fileBlock.activate()
                        contentItem: FileBlock {
                            id: fileBlock
                            ink: Kirigami.Theme.textColor
                            subtleInk: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                            message: fileRow.message
                            block: ({
                                    "attachment": fileRow.attachment,
                                    "voice": false,
                                    "state": fileRow.mediaState,
                                    "ask": false,
                                    "uploading": false,
                                    "progress": fileRow.progress,
                                    "media": fileRow.media,
                                    "name": fileRow.fileName,
                                    "detail": fileRow.detail,
                                    "icon": fileRow.iconName,
                                    "save": fileRow.canSave
                                })
                        }
                    }
                }
                Kirigami.PlaceholderMessage {
                    objectName: "mediaFilesNone"
                    anchors.centerIn: parent
                    width: parent.width - Kirigami.Units.gridUnit * 4
                    visible: App.mediaFiles.loaded && App.mediaFiles.count === 0
                    text: L10n.i18nc("@info:placeholder a chat has no files", "No files")
                }
            }
        }
    }
}
