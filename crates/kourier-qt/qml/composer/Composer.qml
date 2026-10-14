pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The composer: the reply or edit banner, the list of files waiting to be sent, attach, text
// area (Enter sends, Shift+Enter is a newline; pasting files or an image attaches them), emoji
// and send buttons; disabled with the reason when the conversation cannot be written to. With
// files waiting the text is their caption.
//
// Files: any number, of any kinds, added in one go from the picker, a drop or a paste. Kourier
// has no limit of its own here; what the network makes of the selection is shown instead: how
// many messages it becomes (the number beside each file, the line above the list) and which
// files it cannot take (marked with the reason; they stay behind and hold nothing back). The
// list is a model view, so hundreds of files cost what is on screen. Attaching follows the
// conversation's features: no button when the network takes no files at all. Like the text,
// the files stay with their chat: looking at another chat and coming back finds them waiting.
// A picture or a video can be sent as the file it is (the button in its row, or Space): not
// re-encoded, not in an album. A file of a type that some apps of the network will not save
// says so in its row, and is sent like any other.
//
// A folder (dropped, pasted, or picked with "Folder…" from the attach button's menu) is not a
// file and is never marked as one that cannot go. It stands in the list where it was given
// while Kourier counts what it holds, then a dialog asks (FolderQuestionDialog): add the files
// inside, or send them as one .zip. Its row says where it stands (counting, packing with a
// bar); removing the row stops the work. The files around it are sent as usual meanwhile.
//
// Edit mode: the banner says what is edited, the text area holds the message's text (or its
// caption), Enter saves it and what was typed before comes back afterwards. Up in an empty
// composer edits the newest own message that can still be edited; Escape leaves edit mode, or
// drops the reply.
QQC2.ToolBar {
    id: root

    readonly property bool replying: App.composer.replyToId > 0
    readonly property bool editing: App.composer.editId > 0
    readonly property bool blocked: App.timeline.cannotSend !== ""
    // Where the keyboard enters the composer: the attach button, else the text.
    readonly property Item firstStop: attachButton.visible ? attachButton : area
    readonly property string kinds: App.timeline.attachKinds
    readonly property bool canAttach: root.kinds !== ""
    readonly property bool hasFiles: App.composer.attachmentCount > 0
    // The conversation whose text the area holds (drafts are saved when it changes).
    property real conversation: 0
    // Set while the text is replaced programmatically (no typing notifications).
    property bool restoring: false
    // What the text area held when an edit began; it comes back when the edit ends.
    property string stash: ""
    // Something can be sent: a text, files the network takes, or an edit (a caption may be
    // emptied).
    readonly property bool canSend: root.editing ? (area.text.trim().length > 0 || App.composer.editCaption) : (area.text.trim().length > 0 || App.composer.sendableCount > 0)
    // The list of waiting files (tests drive it).
    readonly property alias fileList: strip

    // What the text area holds.
    readonly property alias text: area.text
    // The emoji picker, created when it is first asked for.
    property EmojiPopup emojiPopup: null
    // A text this long may be more than one message of the chat carries: with files waiting
    // it is handed to the plan when it rests (in characters; four bytes each at most, and no
    // network's messages are that short).
    readonly property int captionWorthTelling: 500
    // The plan was told a long caption: it is told again when the text shrinks.
    property bool captionTold: false
    // The question about folders that wait in the list, while it is asked (tests drive it).
    property FolderQuestionDialog folderDialog: null
    // The attach button's menu (tests drive it).
    readonly property alias attachMenu: attachMenu

    // The question shows while there is one, and goes when it is answered (or when the chat
    // it is about is no longer the open one: it comes back with that chat). A dialog that is
    // told to go is let go of at once: it closes in its own time (a transition), and a new
    // question that comes meanwhile gets a dialog of its own.
    function syncFolderDialog(): void {
        if (App.composer.folderAsking && root.folderDialog === null && !root.blocked) {
            root.folderDialog = folderComponent.createObject(root) as FolderQuestionDialog;
            root.folderDialog?.open();
        } else if (!App.composer.folderAsking && root.folderDialog !== null) {
            const answered = root.folderDialog;
            root.folderDialog = null;
            answered.close();
        }
    }

    // (A composer that could not be written to asks once it can again.)
    onBlockedChanged: root.syncFolderDialog()

    // Opens the picker: for files, or for one folder.
    function pick(folder: bool): void {
        fileDialogLoader.setSource(Qt.resolvedUrl("AttachDialog.qml"), {
            "kinds": root.kinds,
            "folder": folder
        });
    }

    Component {
        id: folderComponent
        FolderQuestionDialog {
            id: asked
            // Closed without an answer (Escape, the close button): the folders leave the
            // list, as with "Cancel".
            onClosed: {
                // Still the question's dialog: nobody answered, the user closed it.
                if (root.folderDialog === asked) {
                    root.folderDialog = null;
                    if (App.composer.folderAsking) {
                        App.composer.answerFolders("cancel");
                    }
                }
                asked.destroy();
            }
        }
    }

    function tellCaption(): void {
        root.captionTold = area.length > root.captionWorthTelling;
        App.composer.setCaption(area.text);
    }

    onHasFilesChanged: {
        if (root.hasFiles && area.length > root.captionWorthTelling) {
            captionSettle.restart();
        }
    }

    Timer {
        id: captionSettle
        interval: 300
        onTriggered: root.tellCaption()
    }

    // The keyboard focus goes to the text area.
    function focusInput(): void {
        area.forceActiveFocus();
    }

    function setText(text: string): void {
        root.restoring = true;
        area.text = text;
        area.cursorPosition = area.length;
        root.restoring = false;
    }

    position: QQC2.ToolBar.Footer
    padding: Kirigami.Units.smallSpacing
    leftPadding: Kirigami.Units.largeSpacing
    rightPadding: Kirigami.Units.largeSpacing

    // Adds dropped or picked files (URLs) to the list, all of them in one call.
    function attach(urls: list<url>): void {
        const all = [];
        for (const u of urls) {
            all.push(u.toString());
        }
        App.composer.addFiles(all);
        area.forceActiveFocus();
    }

    function send(): void {
        typingStop.stop();
        if (!root.canSend) {
            return;
        }
        // An edit that was saved gives the text area back what it held before (`editFinished`).
        const edit = root.editing;
        if (App.composer.send(App.timeline.conversationId, area.text) && !edit) {
            root.restoring = true;
            area.clear();
            root.restoring = false;
        }
    }

    // Escape: leave edit mode, else drop the reply. `false` when there was neither.
    function cancel(): bool {
        if (root.editing) {
            App.composer.cancelEdit();
            return true;
        }
        if (root.replying) {
            App.composer.cancelReply();
            return true;
        }
        return false;
    }

    // Leaving a conversation saves its draft; entering one restores its draft.
    function switchTo(id: real): void {
        // An edit ends with its conversation: the text typed before it is the draft to keep.
        App.composer.cancelEdit();
        if (root.conversation !== 0) {
            typingStop.stop();
            App.composer.setTyping(root.conversation, false);
            App.composer.saveDraft(root.conversation, area.text);
        }
        root.conversation = id;
        root.restoring = true;
        area.text = id !== 0 ? App.composer.draft(id) : "";
        area.cursorPosition = area.length;
        root.restoring = false;
        App.composer.clearNotice();
    }

    // The open conversation; a change saves the old draft and restores the new one.
    readonly property real openConversation: App.timeline.conversationId
    onOpenConversationChanged: root.switchTo(root.openConversation)
    Component.onCompleted: {
        root.switchTo(root.openConversation);
        root.syncFolderDialog();
    }

    Connections {
        target: App.composer
        function onFolderAskingChanged(): void {
            root.syncFolderDialog();
        }
        function onDraftLoaded(conversation: real): void {
            if (conversation === root.conversation && area.text === "" && !root.editing) {
                root.setText(App.composer.draft(conversation));
            }
        }
        function onEditStarted(text: string): void {
            typingStop.stop();
            App.composer.setTyping(root.conversation, false);
            root.stash = area.text;
            root.setText(text);
            area.forceActiveFocus();
        }
        function onEditFinished(): void {
            root.setText(root.stash);
            root.stash = "";
        }
        function onReplyToIdChanged(): void {
            if (root.replying) {
                area.forceActiveFocus();
            }
        }
    }

    // "Stopped typing" after a pause.
    Timer {
        id: typingStop
        interval: 3000
        onTriggered: App.composer.setTyping(root.conversation, false)
    }

    function insert(text: string): void {
        area.insert(area.cursorPosition, text);
        area.forceActiveFocus();
    }

    contentItem: ColumnLayout {
        spacing: Kirigami.Units.smallSpacing

        // What is being edited, or what the next message replies to.
        RowLayout {
            id: banner

            readonly property string title: {
                if (root.editing) {
                    return App.composer.editCaption ? L10n.i18nc("@info", "Edit Caption") : L10n.i18nc("@info", "Edit Message");
                }
                return L10n.i18nc("@info", "Reply to %1", App.composer.replyAuthor);
            }
            readonly property string snippet: {
                if (!root.editing) {
                    return App.composer.replyText;
                }
                return App.composer.editText !== "" ? App.composer.editText : L10n.i18nc("@info", "No caption yet");
            }
            readonly property color accent: Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor)

            visible: (root.replying || root.editing) && !root.blocked
            spacing: Kirigami.Units.largeSpacing
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.smallSpacing

            Rectangle {
                implicitWidth: 3
                Layout.fillHeight: true
                color: Kirigami.Theme.highlightColor
            }
            Kirigami.Icon {
                source: root.editing ? "document-edit" : "mail-reply-sender"
                implicitWidth: Kirigami.Units.iconSizes.smallMedium
                implicitHeight: Kirigami.Units.iconSizes.smallMedium
                color: banner.accent
                isMask: true
                Layout.alignment: Qt.AlignVCenter
            }
            ColumnLayout {
                spacing: 0
                Layout.fillWidth: true
                QQC2.Label {
                    text: banner.title
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    color: banner.accent
                    textFormat: Text.PlainText
                    elide: Text.ElideRight
                    Layout.fillWidth: true
                }
                RowLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Layout.fillWidth: true
                    Kirigami.Icon {
                        visible: !root.editing && App.composer.replyIcon !== ""
                        source: App.composer.replyIcon
                        implicitWidth: Kirigami.Units.iconSizes.small
                        implicitHeight: Kirigami.Units.iconSizes.small
                        color: snippetLabel.color
                        isMask: true
                    }
                    QQC2.Label {
                        id: snippetLabel
                        text: banner.snippet
                        elide: Text.ElideRight
                        maximumLineCount: 1
                        textFormat: Text.PlainText
                        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                        Layout.fillWidth: true
                    }
                }
            }
            // The quoted picture.
            Image {
                id: replyThumb
                visible: !root.editing && App.composer.replyThumb !== "" && status !== Image.Error
                source: root.editing ? "" : App.composer.replyThumb
                asynchronous: true
                smooth: true
                fillMode: Image.PreserveAspectCrop
                clip: true
                sourceSize.width: Math.ceil(width)
                sourceSize.height: Math.ceil(height)
                Layout.preferredWidth: Math.round(Kirigami.Units.gridUnit * 2.2)
                Layout.preferredHeight: Math.round(Kirigami.Units.gridUnit * 2.2)
            }
            QQC2.ToolButton {
                icon.name: "dialog-close"
                text: root.editing ? L10n.i18nc("@action:button", "Cancel Edit") : L10n.i18nc("@action:button", "Cancel Reply")
                display: QQC2.AbstractButton.IconOnly
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "%1 (Esc)", text)
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                onClicked: root.cancel()
            }
        }

        // Files waiting to be sent: how they will go, then one row per file.
        ColumnLayout {
            id: files

            readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            readonly property color warn: Style.readable(Kirigami.Theme.neutralTextColor, Kirigami.Theme.backgroundColor)
            // A file's row: its name and the line of details under it. At least the usual
            // height, more when a larger font needs it.
            readonly property real rowHeight: Math.max(Math.round(Kirigami.Units.gridUnit * 2.5), Math.ceil(nameMetrics.height + detailMetrics.height) + Kirigami.Units.smallSpacing * 2)
            // The messages are numbered once there is more than one.
            readonly property bool split: App.composer.messageCount > 1

            // An edit changes the text of a message, never its files.
            visible: root.hasFiles && !root.blocked && !root.editing
            spacing: Kirigami.Units.smallSpacing / 2
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.smallSpacing

            FontMetrics {
                id: nameMetrics
                font: Kirigami.Theme.defaultFont
            }
            FontMetrics {
                id: detailMetrics
                font: Kirigami.Theme.smallFont
            }

            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                Kirigami.Icon {
                    source: "mail-attachment"
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: files.quiet
                    isMask: true
                }
                // "7 files, 4 messages · 12.4 MB": Kourier's own words and numbers.
                QQC2.Label {
                    id: summaryLabel
                    objectName: "fileSummary"
                    text: App.composer.summary
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                }
                // The core is still looking at the files.
                Spinner {
                    visible: !App.composer.planned
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                }
                QQC2.ToolButton {
                    objectName: "clearFiles"
                    icon.name: "edit-clear-all"
                    text: L10n.i18nc("@action:button remove every file from the list", "Clear All")
                    display: root.width > Kirigami.Units.gridUnit * 24 ? QQC2.AbstractButton.TextBesideIcon : QQC2.AbstractButton.IconOnly
                    QQC2.ToolTip.visible: hovered && display === QQC2.AbstractButton.IconOnly
                    QQC2.ToolTip.text: text
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    onClicked: App.composer.clearAttachments()
                }
            }

            // What needs saying about the selection: files that stay behind, a text that
            // goes first.
            RowLayout {
                visible: App.composer.hint !== ""
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                Kirigami.Icon {
                    visible: App.composer.refusedCount > 0
                    source: "data-warning"
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    Layout.alignment: Qt.AlignTop
                }
                QQC2.Label {
                    objectName: "fileHint"
                    text: App.composer.hint
                    font: Kirigami.Theme.smallFont
                    color: App.composer.refusedCount > 0 ? files.warn : files.quiet
                    wrapMode: Text.Wrap
                    textFormat: Text.PlainText
                    Layout.fillWidth: true
                }
            }

            QQC2.ScrollView {
                id: fileScroll

                // What a row cut by an edge of the list fades into.
                readonly property color under: Kirigami.Theme.backgroundColor
                readonly property real barWidth: QQC2.ScrollBar.vertical && QQC2.ScrollBar.vertical.visible ? QQC2.ScrollBar.vertical.width : 0

                Layout.fillWidth: true
                // Up to four and a half rows: the half row says there is more.
                Layout.preferredHeight: Math.min(strip.count, 4.5) * files.rowHeight
                QQC2.ScrollBar.horizontal.policy: QQC2.ScrollBar.AlwaysOff

                ListView {
                    id: strip

                    model: App.composer.files
                    clip: true
                    spacing: 0
                    boundsBehavior: Flickable.StopAtBounds
                    // One tab stop for the whole list: Up and Down move, Delete removes.
                    activeFocusOnTab: true
                    keyNavigationEnabled: true
                    highlightFollowsCurrentItem: true
                    currentIndex: -1
                    Accessible.role: Accessible.List
                    Accessible.name: L10n.i18nc("@info accessible", "Files to send")

                    onActiveFocusChanged: {
                        if (strip.activeFocus && strip.currentIndex < 0 && strip.count > 0) {
                            strip.currentIndex = 0;
                        } else if (!strip.activeFocus) {
                            strip.currentIndex = -1;
                        }
                    }
                    Keys.onDeletePressed: strip.removeCurrent()
                    Keys.onPressed: event => {
                        if (event.key === Qt.Key_Backspace) {
                            strip.removeCurrent();
                            event.accepted = true;
                        }
                    }
                    // Space: a picture or a video goes as the file it is, or as a picture again.
                    Keys.onSpacePressed: {
                        const row = strip.currentItem as FileRowItem;
                        if (row !== null && row.canOriginal) {
                            App.composer.setOriginal(row.index, !row.original);
                        }
                    }
                    Keys.onEscapePressed: area.forceActiveFocus()

                    function removeCurrent(): void {
                        const at = strip.currentIndex;
                        if (at < 0) {
                            return;
                        }
                        App.composer.removeAttachment(at);
                        if (strip.count === 0) {
                            area.forceActiveFocus();
                        } else {
                            strip.currentIndex = Math.min(at, strip.count - 1);
                        }
                    }

                    delegate: FileRowItem {
                        id: fileRow

                        // What the second line says of a file that can go: its size, that
                        // it goes as the file it is, a note about its type.
                        readonly property string details: {
                            const parts = [];
                            if (fileRow.size !== "") {
                                parts.push(fileRow.size);
                            }
                            if (fileRow.original) {
                                parts.push(L10n.i18nc("@info a picture sent as the file it is", "original file"));
                            }
                            if (fileRow.note !== "") {
                                parts.push(fileRow.note);
                            }
                            return parts.join(" · ");
                        }

                        readonly property bool refused: fileRow.reason !== ""
                        readonly property bool current: fileRow.ListView.isCurrentItem && strip.activeFocus
                        // Kourier is at work on it: removing the row stops that.
                        readonly property bool working: fileRow.stage === "counting" || fileRow.stage === "listing" || fileRow.stage === "packing"

                        width: strip.width
                        height: files.rowHeight

                        Accessible.role: Accessible.ListItem
                        Accessible.name: fileRow.name
                        Accessible.description: (fileRow.refused ? fileRow.reason : (fileRow.part > 0 && files.split ? L10n.i18nc("@info accessible, %1 details, %2 number", "%1, sent in message %2", fileRow.details, fileRow.part) : fileRow.details)) + (fileRow.canOriginal ? " " + L10n.i18nc("@info accessible", "Space changes whether it is sent as the original file.") : "")

                        Rectangle {
                            anchors.fill: parent
                            anchors.topMargin: 1
                            anchors.bottomMargin: 1
                            radius: Kirigami.Units.cornerRadius
                            color: Kirigami.Theme.highlightColor
                            opacity: fileRow.current ? 0.18 : (rowHover.hovered ? 0.08 : 0)
                        }
                        HoverHandler {
                            id: rowHover
                        }

                        // Which message the file travels in: its number at the first file
                        // of the message, a line down to the last one.
                        Item {
                            id: gutter
                            // (Anchored, not left at x = 0: the anchors of the row mirror, and
                            // an item that does not would leave them without a start.)
                            anchors.left: parent.left
                            visible: files.split || fileRow.refused
                            width: visible ? Math.round(Kirigami.Units.gridUnit * 1.5) : 0
                            height: parent.height

                            // The line that joins the files of one message. It also passes a
                            // file that stays behind between two of them, behind its mark.
                            Rectangle {
                                visible: fileRow.partThrough || (fileRow.part > 0 && !(fileRow.partFirst && fileRow.partLast))
                                width: 2
                                radius: 1
                                x: Math.round((parent.width - width) / 2)
                                y: !fileRow.partThrough && fileRow.partFirst ? parent.height / 2 : 0
                                height: !fileRow.partThrough && (fileRow.partFirst || fileRow.partLast) ? parent.height / 2 : parent.height
                                color: Kirigami.Theme.highlightColor
                                opacity: 0.55
                            }
                            Rectangle {
                                visible: fileRow.refused && fileRow.partThrough
                                anchors.centerIn: parent
                                width: Kirigami.Units.iconSizes.small + 4
                                height: width
                                radius: width / 2
                                color: fileScroll.under
                            }
                            Rectangle {
                                visible: fileRow.part > 0 && fileRow.partFirst
                                anchors.centerIn: parent
                                width: Math.max(height, partLabel.implicitWidth + Kirigami.Units.smallSpacing * 2)
                                height: Math.round(Kirigami.Units.gridUnit * 1.05)
                                radius: height / 2
                                color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, 0.3)
                                border.width: 1
                                border.color: Kirigami.Theme.highlightColor
                                QQC2.Label {
                                    id: partLabel
                                    anchors.centerIn: parent
                                    text: fileRow.part
                                    font: Kirigami.Theme.smallFont
                                    textFormat: Text.PlainText
                                }
                                HoverHandler {
                                    id: partHover
                                }
                                QQC2.ToolTip.visible: partHover.hovered
                                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip, %1 number", "Sent in message %1", fileRow.part)
                                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                            }
                            Kirigami.Icon {
                                visible: fileRow.refused
                                anchors.centerIn: parent
                                width: Kirigami.Units.iconSizes.small
                                height: Kirigami.Units.iconSizes.small
                                source: "data-warning"
                            }
                        }

                        // The picture itself once the core has read that it is one, else the
                        // icon of the type.
                        Rectangle {
                            id: thumb
                            anchors.left: gutter.right
                            anchors.leftMargin: Kirigami.Units.smallSpacing
                            anchors.verticalCenter: parent.verticalCenter
                            width: Math.round(Kirigami.Units.gridUnit * 2)
                            height: width
                            radius: Kirigami.Units.cornerRadius
                            color: Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, 0.08)
                            clip: true
                            opacity: fileRow.refused ? 0.55 : 1

                            Image {
                                id: picture
                                anchors.fill: parent
                                visible: status === Image.Ready
                                source: fileRow.preview
                                asynchronous: true
                                smooth: true
                                fillMode: Image.PreserveAspectCrop
                                sourceSize.width: Math.ceil(width)
                                sourceSize.height: Math.ceil(height)
                            }
                            Kirigami.Icon {
                                anchors.centerIn: parent
                                visible: !picture.visible
                                width: Kirigami.Units.iconSizes.smallMedium
                                height: width
                                source: fileRow.icon
                            }
                            // The frame of a tile that shows an icon; a picture is its own edge
                            // (its square corners would stand out of a rounded frame).
                            Rectangle {
                                visible: !picture.visible
                                anchors.fill: parent
                                radius: parent.radius
                                color: "transparent"
                                border.width: 1
                                border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                            }
                        }

                        Column {
                            anchors.left: thumb.right
                            anchors.leftMargin: Kirigami.Units.largeSpacing
                            anchors.right: originalButton.visible ? originalButton.left : (work.visible ? work.left : removeButton.left)
                            anchors.rightMargin: Kirigami.Units.smallSpacing
                            anchors.verticalCenter: parent.verticalCenter
                            spacing: 0

                            QQC2.Label {
                                width: parent.width
                                text: fileRow.name
                                elide: Text.ElideMiddle
                                textFormat: Text.PlainText
                                opacity: fileRow.refused ? 0.7 : 1
                            }
                            QQC2.Label {
                                id: detailsLabel
                                objectName: "fileDetails"
                                width: parent.width
                                // Why it cannot go; else how big it is and how it goes.
                                text: fileRow.refused ? fileRow.reason : fileRow.details
                                visible: text !== ""
                                font: Kirigami.Theme.smallFont
                                color: fileRow.refused ? files.warn : files.quiet
                                elide: Text.ElideRight
                                textFormat: Text.PlainText

                                // In a narrow window the line is cut (a note about the type
                                // is long): the pointer shows all of it.
                                HoverHandler {
                                    id: detailsHover
                                    enabled: detailsLabel.truncated
                                }
                                QQC2.ToolTip.visible: detailsHover.hovered && detailsLabel.truncated
                                QQC2.ToolTip.text: Style.plain(detailsLabel.text)
                                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                            }
                        }

                        // A folder Kourier is at work on: a bar while it is packed (how far
                        // is known), a spinner while it is counted or listed.
                        Item {
                            id: work
                            objectName: "folderWork"
                            anchors.right: removeButton.left
                            anchors.rightMargin: Kirigami.Units.smallSpacing
                            anchors.verticalCenter: parent.verticalCenter
                            visible: fileRow.working
                            width: fileRow.stage === "packing" ? Math.min(Kirigami.Units.gridUnit * 6, Math.round(fileRow.width * 0.22)) : Kirigami.Units.iconSizes.smallMedium
                            height: Kirigami.Units.iconSizes.smallMedium

                            QQC2.ProgressBar {
                                objectName: "folderProgress"
                                anchors.left: parent.left
                                anchors.right: parent.right
                                anchors.verticalCenter: parent.verticalCenter
                                // (The style's bar has no height of its own while it is
                                // made invisible, and keeps none: it is given one.)
                                height: topInset + bottomInset + Math.round(Kirigami.Units.gridUnit / 3)
                                visible: fileRow.stage === "packing"
                                from: 0
                                to: 1
                                value: fileRow.progress
                            }
                            Spinner {
                                anchors.fill: parent
                                visible: fileRow.working && fileRow.stage !== "packing"
                            }
                        }

                        // A picture or a video: as what it is (shown and scaled by the
                        // receivers' apps), or as the file it is.
                        QQC2.ToolButton {
                            id: originalButton
                            objectName: "originalButton"
                            anchors.right: removeButton.left
                            anchors.verticalCenter: parent.verticalCenter
                            visible: fileRow.canOriginal
                            focusPolicy: Qt.NoFocus
                            checkable: true
                            checked: fileRow.original
                            opacity: fileRow.original || rowHover.hovered || fileRow.current ? 1 : 0.55
                            icon.name: "document-export"
                            icon.width: Kirigami.Units.iconSizes.small
                            icon.height: Kirigami.Units.iconSizes.small
                            text: L10n.i18nc("@action:button, %1 file name", "Send %1 as original file", fileRow.name)
                            display: QQC2.AbstractButton.IconOnly
                            QQC2.ToolTip.visible: hovered
                            QQC2.ToolTip.text: fileRow.original ? L10n.i18nc("@info:tooltip", "Sent as the original file. Click to send it as a picture again.") : L10n.i18nc("@info:tooltip", "Send as original file: unchanged, shown as a file and not in an album")
                            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                            onClicked: {
                                App.composer.setOriginal(fileRow.index, !fileRow.original);
                                // The mark follows what the list holds, not the click.
                                originalButton.checked = Qt.binding(() => fileRow.original);
                            }
                        }

                        QQC2.ToolButton {
                            id: removeButton
                            anchors.right: parent.right
                            anchors.verticalCenter: parent.verticalCenter
                            // The list is one tab stop; Delete removes the current row.
                            focusPolicy: Qt.NoFocus
                            // Quiet until the pointer is on the row: hundreds of them stand
                            // side by side.
                            opacity: rowHover.hovered || fileRow.current ? 1 : 0.55
                            icon.name: "dialog-close"
                            icon.width: Kirigami.Units.iconSizes.small
                            icon.height: Kirigami.Units.iconSizes.small
                            text: L10n.i18nc("@action:button, %1 file name", "Remove %1", fileRow.name)
                            display: QQC2.AbstractButton.IconOnly
                            QQC2.ToolTip.visible: hovered
                            // (Taking a folder out of the list stops the work on it.)
                            QQC2.ToolTip.text: fileRow.working ? L10n.i18nc("@action:button stop packing a folder and take it out of the list", "Stop and Remove") : L10n.i18nc("@action:button", "Remove")
                            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                            onClicked: App.composer.removeAttachment(fileRow.index)
                        }
                    }
                }

                // A row cut by the lower edge fades out instead of ending in a hard line:
                // there is more below (and, once scrolled, above). After the list, with
                // the scroll view as parent: what is declared inside a ScrollView goes into
                // its content otherwise.
                Rectangle {
                    objectName: "fileFadeBottom"
                    parent: fileScroll
                    z: 1
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.bottom: parent.bottom
                    anchors.rightMargin: fileScroll.barWidth
                    height: Math.round(files.rowHeight * 0.6)
                    visible: strip.contentHeight > strip.height + 1 && !strip.atYEnd
                    gradient: Gradient {
                        GradientStop {
                            position: 0
                            color: Qt.alpha(fileScroll.under, 0)
                        }
                        GradientStop {
                            position: 1
                            color: fileScroll.under
                        }
                    }
                }
                Rectangle {
                    objectName: "fileFadeTop"
                    parent: fileScroll
                    z: 1
                    anchors.left: parent.left
                    anchors.right: parent.right
                    anchors.top: parent.top
                    anchors.rightMargin: fileScroll.barWidth
                    height: Math.round(files.rowHeight * 0.4)
                    visible: strip.contentHeight > strip.height + 1 && !strip.atYBeginning
                    gradient: Gradient {
                        GradientStop {
                            position: 0
                            color: fileScroll.under
                        }
                        GradientStop {
                            position: 1
                            color: Qt.alpha(fileScroll.under, 0)
                        }
                    }
                }
            }
        }

        // A send the core is still copying and encrypting: its messages show once all of it
        // is stored.
        RowLayout {
            visible: App.composer.preparing > 0 && !root.blocked
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true
            Layout.leftMargin: Kirigami.Units.smallSpacing

            Spinner {
                implicitWidth: Kirigami.Units.iconSizes.small
                implicitHeight: Kirigami.Units.iconSizes.small
            }
            QQC2.Label {
                objectName: "preparingFiles"
                // The core says how far it is: "Preparing 40 files to send… 12 copied".
                text: App.composer.preparing > 1 && App.composer.preparingDone > 0 ? L10n.i18ncp("@info, %1 how many, %2 how many of them are copied", "Preparing %1 file to send… %2 copied", "Preparing %1 files to send… %2 copied", App.composer.preparing, App.composer.preparingDone) : L10n.i18ncp("@info, %1 how many", "Preparing %1 file to send…", "Preparing %1 files to send…", App.composer.preparing)
                font: Kirigami.Theme.smallFont
                color: files.quiet
                elide: Text.ElideRight
                textFormat: Text.PlainText
                Layout.fillWidth: true
            }
            QQC2.ProgressBar {
                objectName: "preparingProgress"
                visible: App.composer.preparing > 1
                from: 0
                to: Math.max(1, App.composer.preparing)
                value: App.composer.preparingDone
                Layout.preferredWidth: Kirigami.Units.gridUnit * 6
                // (As for a folder's bar below: the style's bar has no height of its own
                // while it is made invisible, and keeps none.)
                Layout.preferredHeight: topInset + bottomInset + Math.round(Kirigami.Units.gridUnit / 3)
            }
        }

        Kirigami.InlineMessage {
            visible: root.blocked || App.composer.error !== ""
            type: root.blocked ? Kirigami.MessageType.Information : Kirigami.MessageType.Error
            text: Style.plain(root.blocked ? App.timeline.cannotSend : App.composer.error)
            Layout.fillWidth: true
        }

        // A small notice from the core, e.g. that sending is not available yet.
        Kirigami.InlineMessage {
            visible: !root.blocked && App.composer.notice !== ""
            type: Kirigami.MessageType.Information
            text: Style.plain(App.composer.notice)
            Layout.fillWidth: true
            actions: [
                Kirigami.Action {
                    icon.name: "dialog-close"
                    text: L10n.i18nc("@action:button", "Dismiss")
                    displayHint: Kirigami.DisplayHint.IconOnly
                    onTriggered: App.composer.clearNotice()
                }
            ]
        }

        RowLayout {
            visible: !root.blocked
            spacing: Kirigami.Units.smallSpacing
            Layout.fillWidth: true

            // Attach: files, or a folder (one picker cannot offer both at once).
            QQC2.ToolButton {
                id: attachButton
                objectName: "attachButton"
                visible: root.canAttach && !root.editing
                icon.name: "mail-attachment"
                text: L10n.i18nc("@action:button", "Attach")
                display: QQC2.AbstractButton.IconOnly
                down: pressed || attachMenu.visible
                Accessible.role: Accessible.ButtonMenu
                QQC2.ToolTip.visible: hovered && !attachMenu.visible
                QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Attach files or a folder")
                QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                Layout.alignment: Qt.AlignBottom
                onClicked: {
                    if (attachMenu.visible) {
                        attachMenu.close();
                    } else {
                        // Above the button: the composer is at the window's lower edge.
                        attachMenu.popup(attachButton, 0, -attachMenu.implicitHeight);
                    }
                }

                // Made with the composer: a menu of this style that is opened in the
                // moment it is created can come up as small as its padding.
                QQC2.Menu {
                    id: attachMenu

                    // In the window's scene (also on Wayland, and in headless screenshots).
                    popupType: QQC2.Popup.Item
                    title: L10n.i18nc("@title:menu", "Attach")

                    QQC2.MenuItem {
                        objectName: "attachFiles"
                        icon.name: "document-open"
                        text: L10n.i18nc("@action:inmenu", "Files…")
                        onTriggered: root.pick(false)
                    }
                    QQC2.MenuItem {
                        objectName: "attachFolder"
                        icon.name: "folder-open"
                        text: L10n.i18nc("@action:inmenu", "Folder…")
                        onTriggered: root.pick(true)
                    }
                }
            }

            QQC2.ScrollView {
                Layout.fillWidth: true
                Layout.maximumHeight: Kirigami.Units.gridUnit * 8
                Layout.alignment: Qt.AlignVCenter

                QQC2.TextArea {
                    id: area

                    placeholderText: root.editing ? (App.composer.editCaption ? L10n.i18nc("@info:placeholder", "Caption") : L10n.i18nc("@info:placeholder", "Message")) : (root.hasFiles ? L10n.i18nc("@info:placeholder", "Add a caption") : L10n.i18nc("@info:placeholder", "Message"))
                    Accessible.name: placeholderText
                    wrapMode: TextEdit.Wrap
                    textFormat: TextEdit.PlainText
                    persistentSelection: true

                    onTextChanged: {
                        // With files waiting the text is their caption: how they are sent
                        // can depend on there being one, and on its length when it is long.
                        App.composer.setHasText(area.text.trim().length > 0);
                        if (root.hasFiles && !root.editing && (area.length > root.captionWorthTelling || root.captionTold)) {
                            captionSettle.restart();
                        }
                        if (root.restoring || root.editing || root.conversation === 0) {
                            return;
                        }
                        App.composer.setTyping(root.conversation, area.text.length > 0);
                        if (area.text.length > 0) {
                            typingStop.restart();
                        } else {
                            typingStop.stop();
                        }
                    }

                    // Pasting files (copied in a file manager) or an image attaches them; text
                    // pastes as usual.
                    Keys.onPressed: event => {
                        if (event.matches(StandardKey.Paste) && root.canAttach && !root.editing && App.composer.paste()) {
                            event.accepted = true;
                        }
                    }
                    // Up in an empty composer edits the last own message (as in most chat
                    // apps); elsewhere it moves the cursor.
                    Keys.onUpPressed: event => {
                        event.accepted = area.text.length === 0 && !root.editing && !root.hasFiles && App.composer.editLast(root.conversation);
                    }
                    Keys.onEscapePressed: event => {
                        event.accepted = root.cancel();
                    }

                    Keys.onReturnPressed: event => {
                        if (event.modifiers & Qt.ShiftModifier) {
                            event.accepted = false;
                        } else {
                            root.send();
                            event.accepted = true;
                        }
                    }
                    Keys.onEnterPressed: event => {
                        if (event.modifiers & Qt.ShiftModifier) {
                            event.accepted = false;
                        } else {
                            root.send();
                            event.accepted = true;
                        }
                    }
                }
            }

            QQC2.ToolButton {
                id: emojiButton
                icon.name: "smiley"
                text: L10n.i18nc("@action:button", "Emoji")
                display: QQC2.AbstractButton.IconOnly
                checkable: true
                checked: root.emojiPopup !== null && root.emojiPopup.visible
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: text
                Layout.alignment: Qt.AlignBottom
                onClicked: {
                    // The picker (categories and a grid of every emoji) exists once asked for.
                    if (root.emojiPopup === null) {
                        root.emojiPopup = emojiComponent.createObject(emojiButton) as EmojiPopup;
                    }
                    if (root.emojiPopup.visible) {
                        root.emojiPopup.close();
                    } else {
                        root.emojiPopup.open();
                    }
                    emojiButton.checked = Qt.binding(() => root.emojiPopup !== null && root.emojiPopup.visible);
                }

                Component {
                    id: emojiComponent
                    EmojiPopup {
                        y: -height - Kirigami.Units.smallSpacing
                        x: emojiButton.width - width
                        onPicked: emoji => root.insert(emoji)
                    }
                }
            }

            QQC2.RoundButton {
                icon.name: root.editing ? "dialog-ok-apply" : "document-send"
                text: root.editing ? L10n.i18nc("@action:button", "Save") : L10n.i18nc("@action:button", "Send")
                display: QQC2.AbstractButton.IconOnly
                enabled: root.canSend
                highlighted: enabled
                QQC2.ToolTip.visible: hovered
                QQC2.ToolTip.text: root.editing ? L10n.i18nc("@info:tooltip", "Save (Enter)") : L10n.i18nc("@info:tooltip", "Send (Enter)")
                Layout.alignment: Qt.AlignBottom
                onClicked: root.send()
            }
        }
    }

    // The file dialog (and the QtQuick.Dialogs import) is loaded on first use.
    Loader {
        id: fileDialogLoader
        onLoaded: {
            const dialog = fileDialogLoader.item as AttachDialog;
            if (dialog) {
                dialog.picked.connect(urls => root.attach(urls));
                dialog.closed.connect(() => Qt.callLater(() => fileDialogLoader.source = ""));
            }
        }
    }
}
