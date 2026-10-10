pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import app.kourier

// One message: a rounded bubble (the normal background tinted with the accent for own
// messages) with the author line in groups, the content blocks, the timer mark, the edited
// marker (it opens the earlier versions), time and receipt; reaction chips underneath. Several
// pictures of one message are one album grid. What the row offers (`actions`, the
// core's answer for this message) is reachable three ways: the context menu (right click, a
// long press on a touch screen, or the Menu key on the row the timeline's keyboard focus is
// on), the toolbar that shows beside the bubble under the pointer, and the chips themselves.
// What a click inside the bubble does (view the picture, open the file, play the voice note,
// go to the quoted message) the keyboard does from the row too: Enter or Space activate its
// picture, file or voice note, Ctrl+Enter goes to the message it quotes, and the menu has an
// entry for each. A long text is shown in part with "Read more" under it; Enter on such a row
// shows more of the text first.
Item {
    id: root

    required property int index
    required property real messageId
    required property bool own
    required property string author
    required property string avatarUrl
    required property bool showAuthor
    required property bool showAvatar
    required property bool groupStart
    required property bool groupEnd
    required property string timeLabel
    required property var blocks
    required property var reactions
    required property bool edited
    required property bool deleted
    required property string receipt
    required property var actions
    required property string expiry

    readonly property bool group: App.timeline.isGroup
    // The row is laid out right to left (the window's layout direction, as anchors and
    // positioners get it): `x` is not mirrored by it, so what is placed by an explicit `x`
    // below goes by `atRight`, the side of the row the bubble leans on: the right for own
    // messages (the left in a mirrored layout), the left, beside the avatar, for the others.
    readonly property bool mirrored: root.LayoutMirroring.enabled
    readonly property bool atRight: root.own !== root.mirrored
    // Big emoji and stickers stand without a bubble.
    readonly property bool jumbo: root.blocks.length === 1 && (root.blocks[0].type === "text" && root.blocks[0].jumbo || root.blocks[0].type === "image" && root.blocks[0].sticker)
    readonly property real sideMargin: root.width > Kirigami.Units.gridUnit * 30 ? Kirigami.Units.gridUnit : Kirigami.Units.largeSpacing
    // Whole pixels, here and for the bubble's width and paddings: at a fractional position or
    // width the outermost pixel column of a picture is only partly covered, and the bubble
    // shows through it as a line of another shade.
    readonly property real avatarColumn: root.group && !root.own ? Math.round(Style.avatarSize + Kirigami.Units.largeSpacing) : 0
    readonly property real maxBubble: Math.floor(Math.min(Style.bubbleMaxWidth, (root.width - root.sideMargin * 2 - root.avatarColumn) * 0.85))
    readonly property real hpad: Math.round(Kirigami.Units.largeSpacing + Kirigami.Units.smallSpacing / 2)
    readonly property real vpad: Math.round(Kirigami.Units.smallSpacing * 1.5)
    readonly property color bubbleColor: root.own ? Style.ownBubble(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor) : Style.peerBubble(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor)
    // What the content stands on: the bubble, or the page for big emoji and stickers.
    readonly property color surface: root.jumbo ? Kirigami.Theme.backgroundColor : root.bubbleColor
    readonly property color inkColor: Kirigami.Theme.textColor
    readonly property color subtleInk: Style.secondary(Kirigami.Theme.textColor, root.surface)
    // Name colors come from the avatar palette and the accent from the theme: neither was
    // chosen for this bubble, so both are adjusted until they can be read on it.
    readonly property color authorColor: Style.readable(Components.NameUtils.colorsFromString(root.author), root.surface)
    readonly property color accentInk: Style.readable(Kirigami.Theme.highlightColor, root.surface)

    // The timeline's keyboard focus is on this row.
    readonly property bool current: root.ListView.isCurrentItem && root.ListView.view !== null && root.ListView.view.activeFocus
    // One of the shared popups is about this row.
    readonly property bool active: TimelineActions.activeMessage === root.messageId
    readonly property bool failed: root.receipt === "failed"
    // A long text of the row is shown only in part ("Read more").
    readonly property bool hasMore: root.blocks.some(b => b.type === "text" && b.more === true)
    // The text "Copy Text" takes: the message text or the caption (all of it, not the part
    // that is shown).
    readonly property string copyText: {
        for (const b of root.blocks) {
            if (b.type === "text") {
                return b.plain;
            }
        }
        return "";
    }

    // Widest natural width of the bubble's content, clamped to the bubble's maximum.
    property real naturalWidth: 0

    function measure(): void {
        let w = Math.max(authorLabel.visible ? authorLabel.implicitWidth : 0, footer.implicitWidth);
        for (let i = 0; i < blockRepeater.count; ++i) {
            const item = blockRepeater.itemAt(i);
            if (item) {
                w = Math.max(w, item.implicitWidth);
            }
        }
        root.naturalWidth = Math.ceil(w);
    }

    // The loaded block items of the row, in order.
    function blockItems(): list<Item> {
        const out = [];
        for (let i = 0; i < blockRepeater.count; ++i) {
            const loader = blockRepeater.itemAt(i) as Loader;
            if (loader !== null && loader.item !== null) {
                out.push(loader.item);
            }
        }
        return out;
    }

    // What activating the row's first picture, album, file or voice note does now: "view",
    // "open", "play", "pause" or "download"; "" when the row has none or it can do nothing
    // at the moment (it is downloading, or no longer available).
    function mediaAction(): string {
        for (const item of root.blockItems()) {
            const image = item as ImageBlock;
            if (image !== null) {
                return image.mediaAction;
            }
            const grid = item as AlbumBlock;
            if (grid !== null) {
                return grid.mediaAction;
            }
            const file = item as FileBlock;
            if (file !== null) {
                return file.mediaAction;
            }
        }
        return "";
    }

    // That action in words, for the menu and for a screen reader.
    function mediaLabel(): string {
        let video = false;
        let pictures = 0;
        for (const b of root.blocks) {
            if (b.type === "image") {
                video = b.kind === "video";
                pictures = 1;
                break;
            }
            if (b.type === "album") {
                pictures = b.count;
                break;
            }
            if (b.type === "file") {
                break;
            }
        }
        switch (root.mediaAction()) {
        case "view":
            return pictures > 1 ? L10n.i18nc("@action:inmenu", "View Pictures") : L10n.i18nc("@action:inmenu", "View Picture");
        case "open":
            return video ? L10n.i18nc("@action:inmenu", "Play Video") : L10n.i18nc("@action:inmenu", "Open File");
        case "play":
            return L10n.i18nc("@action:inmenu", "Play Voice Message");
        case "pause":
            return L10n.i18nc("@action:inmenu", "Pause Voice Message");
        case "download":
            return L10n.i18nc("@action:inmenu", "Download");
        default:
            return "";
        }
    }

    // Does it; `false` when there was nothing to do, and then nothing was done: the caller
    // opens the row's menu instead. (Asked before acting: a sticker has no action of its
    // own here although a tap views it, and Enter must not both view it and open the menu;
    // and what a block offers can change by acting on it.)
    function activateMedia(): bool {
        for (const item of root.blockItems()) {
            const image = item as ImageBlock;
            if (image !== null) {
                if (image.mediaAction === "") {
                    return false;
                }
                image.activate();
                return true;
            }
            const grid = item as AlbumBlock;
            if (grid !== null) {
                if (grid.mediaAction === "") {
                    return false;
                }
                grid.activate();
                return true;
            }
            const file = item as FileBlock;
            if (file !== null) {
                if (file.mediaAction === "") {
                    return false;
                }
                file.activate();
                return true;
            }
        }
        return false;
    }

    // Shows one step more of the row's long text; `false` when it has none to show (and then
    // nothing was done: the caller goes on to the row's other actions).
    function readMore(): bool {
        for (const item of root.blockItems()) {
            const text = item as TextBlock;
            if (text !== null && text.hasMore) {
                text.readMoreText();
                return true;
            }
        }
        return false;
    }

    // The quote of the row, when it leads somewhere.
    function quoteBlock(): QuoteBlock {
        for (const item of root.blockItems()) {
            const quote = item as QuoteBlock;
            if (quote !== null && quote.reachable) {
                return quote;
            }
        }
        return null;
    }

    // Goes to the quoted message; `false` when the row quotes nothing that is here.
    function jumpToQuote(): bool {
        const quote = root.quoteBlock();
        quote?.activate();
        return quote !== null;
    }

    // The pictures and files of the row, as the "delete an attachment" question lists them.
    function attachmentList(): var {
        const out = [];
        const picture = (item, n) => ({
                "attachment": item.attachment ?? 0,
                "name": item.kind === "video" ? L10n.i18nc("@info a video of a message, %1 its number", "Video %1", n) : L10n.i18nc("@info a picture of a message, %1 its number", "Picture %1", n),
                "icon": item.kind === "video" ? "video-x-generic" : "image-x-generic",
                "thumb": item.state === "local" ? item.url : ""
            });
        for (const b of root.blocks) {
            if (b.type === "image") {
                out.push(picture(b, out.length + 1));
            } else if (b.type === "album") {
                for (const item of b.items) {
                    out.push(picture(item, out.length + 1));
                }
            } else if (b.type === "file") {
                out.push({
                    "attachment": b.attachment ?? 0,
                    "name": b.voice ? L10n.i18nc("@info", "Voice message") : b.name,
                    "icon": b.voice ? "audio-x-generic" : b.icon,
                    "thumb": ""
                });
            }
        }
        return out.filter(a => a.attachment > 0);
    }

    // What the shared popups need to know about this row.
    function rowInfo(): var {
        return {
            "messageId": root.messageId,
            "own": root.own,
            "author": root.author,
            "actions": root.actions,
            "copyText": root.copyText,
            "reactions": root.reactions,
            "failed": root.failed,
            "edited": root.edited && !root.deleted,
            "system": false,
            "mediaAction": root.mediaAction(),
            "mediaLabel": root.mediaLabel(),
            "quoted": root.quoteBlock() !== null,
            "attachments": root.deleted ? [] : root.attachmentList()
        };
    }

    // The question which attachment to delete, about this row.
    function openAttachmentDelete(): void {
        TimelineActions.removeAttachment(root.rowInfo());
    }

    // The context menu without a pointer (the Menu key): beside the bubble.
    function openMenu(): void {
        if (root.actions.any) {
            TimelineActions.menu(bubble, root.rowInfo(), -1, 0);
        }
    }

    function reply(): void {
        App.composer.replyTo(App.timeline.conversationId, root.messageId);
    }

    // The other popups of this row, at its bubble (the toolbar and the chips open them at
    // themselves).
    function openPicker(): void {
        TimelineActions.picker(bubble, root.rowInfo());
    }

    // (At the bubble with its chips: the list goes below them, or above the bubble when there
    // is no room below, and leaves the message itself readable.)
    function openReactions(): void {
        TimelineActions.reactions(column, root.rowInfo());
    }

    function openDelete(): void {
        TimelineActions.remove(root.rowInfo());
    }

    // The row's album grid, when it has one.
    function album(): AlbumBlock {
        for (let i = 0; i < blockRepeater.count; ++i) {
            const loader = blockRepeater.itemAt(i) as Loader;
            const block = loader ? loader.item as AlbumBlock : null;
            if (block !== null) {
                return block;
            }
        }
        return null;
    }

    // 0: the row has no album; 1: some of its pictures are not downloaded; 2: all are.
    function albumState(): int {
        const block = root.album();
        if (block === null) {
            return 0;
        }
        for (const item of block.items) {
            if (item.state !== "local") {
                return 1;
            }
        }
        return 2;
    }

    // Opens the viewer on the row's album at picture `index`; `false` while the row has no
    // album whose pictures are all downloaded.
    function openAlbum(index: int): bool {
        if (root.albumState() !== 2) {
            return false;
        }
        root.album().openAt(index);
        return true;
    }

    width: ListView.view ? ListView.view.width : implicitWidth
    implicitHeight: column.y + column.height + (root.groupEnd ? Kirigami.Units.largeSpacing : Kirigami.Units.smallSpacing / 2)

    Accessible.role: Accessible.ListItem
    Accessible.name: root.deleted ? L10n.i18nc("@info accessible, %1 author, %2 time", "%1, %2: deleted message", root.author, root.timeLabel) : L10n.i18nc("@info accessible, %1 author, %2 time, %3 text", "%1, %2: %3", root.author, root.timeLabel, root.copyText)
    Accessible.description: (root.edited ? L10n.i18nc("@info accessible", "Edited.") + " " : "") + (root.failed ? L10n.i18nc("@info accessible", "Not sent.") + " " : "") + root.expiry + (root.current && root.keyHint !== "" ? " " + root.keyHint : "")
    // What the keyboard does on this row, said to a screen reader while the row is current.
    readonly property string keyHint: {
        if (!root.current) {
            return "";
        }
        // (Enter shows more of the text first, then does what the picture or file does.)
        const what = root.hasMore ? L10n.i18nc("@action:button", "Read more") : root.mediaLabel();
        return what !== "" ? L10n.i18nc("@info accessible, %1 an action such as View Picture", "Enter: %1.", what) : "";
    }

    HoverHandler {
        id: hover
    }

    // A quote jumped here: the row lights up once and fades.
    Rectangle {
        id: flash
        anchors.fill: parent
        color: Kirigami.Theme.highlightColor
        opacity: 0

        SequentialAnimation {
            id: flashAnimation
            NumberAnimation {
                target: flash
                property: "opacity"
                to: 0.28
                duration: Kirigami.Units.shortDuration
            }
            PauseAnimation {
                duration: Kirigami.Units.veryLongDuration
            }
            NumberAnimation {
                target: flash
                property: "opacity"
                to: 0
                duration: Kirigami.Units.veryLongDuration * 2
                easing.type: Easing.OutCubic
            }
        }

        Connections {
            target: TimelineActions
            function onFlashSerialChanged(): void {
                if (TimelineActions.flashMessage === root.messageId) {
                    flashAnimation.restart();
                }
            }
        }
        // A row created by the jump itself (its page just loaded).
        Component.onCompleted: {
            if (TimelineActions.flashMessage === root.messageId) {
                flashAnimation.restart();
            }
        }
    }

    Avatar {
        visible: root.showAvatar
        x: root.atRight ? root.width - root.sideMargin - width : root.sideMargin
        y: bubble.y + bubble.height - height
        width: Style.avatarSize
        height: Style.avatarSize
        name: root.author
        source: root.avatarUrl
    }

    Column {
        id: column

        x: Math.round(root.atRight ? root.width - root.sideMargin - root.avatarColumn - width : root.sideMargin + root.avatarColumn)
        y: root.groupStart ? Kirigami.Units.smallSpacing : 0
        width: Math.max(bubble.width, reactionFlow.visible ? reactionFlow.width : 0)
        spacing: 0

        Kirigami.ShadowedRectangle {
            id: bubble

            readonly property real big: Style.bubbleRadius
            readonly property real tight: Style.bubbleRadiusTight

            // (Anchors are mirrored with the layout, so these stay `own` and not `atRight`.)
            anchors.left: root.own ? undefined : parent.left
            anchors.right: root.own ? parent.right : undefined
            width: Math.min(root.maxBubble, root.naturalWidth + root.hpad * 2)
            height: content.height + root.vpad * 2
            color: root.jumbo ? "transparent" : root.bubbleColor
            // The row with the keyboard focus (the focus color), or else a frame at the theme's
            // frame contrast: faint in Breeze, solid in the high-contrast schemes, where the
            // tint alone does not set the bubble off from the page. Big emoji and stickers
            // have no bubble to frame.
            border.width: root.current ? 2 : (root.jumbo ? 0 : 1)
            border.color: root.current ? Kirigami.Theme.focusColor : Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
            // The software renderer (headless screenshots) ignores per-corner radii and
            // draws `radius` on every corner.
            radius: bubble.big
            // (The tight corners are on the side of the avatar, or of the edge for own messages.)
            corners.topLeftRadius: !root.atRight && !root.groupStart ? bubble.tight : bubble.big
            corners.bottomLeftRadius: !root.atRight && !root.groupEnd ? bubble.tight : bubble.big
            corners.topRightRadius: root.atRight && !root.groupStart ? bubble.tight : bubble.big
            corners.bottomRightRadius: root.atRight && !root.groupEnd ? bubble.tight : bubble.big

            // The context menu: right click, or a long press on a touch screen (a held mouse
            // button selects text).
            TapHandler {
                acceptedButtons: Qt.RightButton
                enabled: root.actions.any
                onTapped: point => TimelineActions.menu(bubble, root.rowInfo(), Math.round(point.position.x), Math.round(point.position.y))
            }
            TapHandler {
                id: longPress
                acceptedDevices: PointerDevice.TouchScreen | PointerDevice.Stylus
                enabled: root.actions.any
                onLongPressed: TimelineActions.menu(bubble, root.rowInfo(), Math.round(longPress.point.position.x), Math.round(longPress.point.position.y))
            }

            Column {
                id: content

                x: root.hpad
                y: root.vpad
                width: bubble.width - root.hpad * 2
                spacing: Kirigami.Units.smallSpacing

                QQC2.Label {
                    id: authorLabel
                    visible: root.showAuthor
                    width: parent.width
                    text: root.author
                    elide: Text.ElideRight
                    textFormat: Text.PlainText
                    color: root.authorColor
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "weight": Font.DemiBold
                    })
                    onImplicitWidthChanged: root.measure()
                }

                Repeater {
                    id: blockRepeater
                    model: root.blocks
                    delegate: Loader {
                        id: blockLoader

                        required property var modelData

                        // Text wraps at the maximum width and reports what it uses; other
                        // blocks take the bubble's width.
                        width: blockLoader.modelData.type === "text" ? root.maxBubble - root.hpad * 2 : content.width
                        onImplicitWidthChanged: root.measure()
                        sourceComponent: {
                            switch (blockLoader.modelData.type) {
                            case "text":
                                return textComponent;
                            case "quote":
                                return quoteComponent;
                            case "image":
                                return imageComponent;
                            case "album":
                                return albumComponent;
                            case "file":
                                return fileComponent;
                            default:
                                return placeholderComponent;
                            }
                        }

                        Component {
                            id: textComponent
                            TextBlock {
                                block: blockLoader.modelData
                                ink: root.inkColor
                                surface: root.surface
                                message: root.messageId
                            }
                        }
                        Component {
                            id: quoteComponent
                            QuoteBlock {
                                block: blockLoader.modelData
                                ink: root.inkColor
                                surface: root.surface
                            }
                        }
                        Component {
                            id: imageComponent
                            ImageBlock {
                                block: blockLoader.modelData
                                available: root.maxBubble - root.hpad * 2
                                message: root.messageId
                            }
                        }
                        Component {
                            id: albumComponent
                            AlbumBlock {
                                block: blockLoader.modelData
                                available: root.maxBubble - root.hpad * 2
                                message: root.messageId
                            }
                        }
                        Component {
                            id: fileComponent
                            FileBlock {
                                block: blockLoader.modelData
                                ink: root.inkColor
                                subtleInk: root.subtleInk
                            }
                        }
                        Component {
                            id: placeholderComponent
                            PlaceholderBlock {
                                block: blockLoader.modelData
                                ink: root.subtleInk
                            }
                        }
                    }
                }

                Row {
                    id: footer
                    anchors.right: parent.right
                    spacing: Kirigami.Units.smallSpacing
                    onImplicitWidthChanged: root.measure()

                    // The message disappears: a small timer, with what it counts from.
                    Kirigami.Icon {
                        id: timerMark
                        visible: root.expiry !== ""
                        source: "chronometer"
                        width: Kirigami.Units.iconSizes.small
                        height: Kirigami.Units.iconSizes.small
                        color: root.subtleInk
                        isMask: true
                        anchors.verticalCenter: parent.verticalCenter

                        HoverHandler {
                            id: timerHover
                        }
                        QQC2.ToolTip.visible: timerHover.hovered
                        QQC2.ToolTip.text: root.expiry
                        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    }
                    // "edited": a click shows the earlier versions.
                    QQC2.Label {
                        id: editedMark
                        objectName: "editedMark"
                        visible: root.edited
                        text: L10n.i18nc("@info message was edited", "edited")
                        // The font of the time beside it (a theme font assigned whole does not
                        // always arrive as the size it names, for instance without the KDE
                        // platform theme), underlined while the pointer is on it.
                        font: Style.styled(timeText.font, {
                            "underline": editedHover.hovered
                        })
                        color: root.subtleInk
                        anchors.verticalCenter: parent.verticalCenter
                        Accessible.role: Accessible.Button
                        Accessible.name: L10n.i18nc("@action:button accessible", "Edited. Show earlier versions")
                        Accessible.onPressAction: TimelineActions.history(root.rowInfo())

                        HoverHandler {
                            id: editedHover
                            cursorShape: Qt.PointingHandCursor
                        }
                        TapHandler {
                            onTapped: TimelineActions.history(root.rowInfo())
                        }
                        QQC2.ToolTip.visible: editedHover.hovered
                        QQC2.ToolTip.text: L10n.i18nc("@info:tooltip", "Show earlier versions")
                        QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
                    }
                    QQC2.Label {
                        id: timeText
                        text: root.timeLabel
                        font: Kirigami.Theme.smallFont
                        color: root.subtleInk
                        anchors.verticalCenter: parent.verticalCenter
                    }
                    ReceiptIcon {
                        receipt: root.receipt
                        tint: root.subtleInk
                        readTint: root.accentInk
                        anchors.verticalCenter: parent.verticalCenter
                    }
                }
            }
        }

        Flow {
            id: reactionFlow

            // The chips side by side; a Flow's own implicit width is that of its widest row at
            // its current width, which is one chip while it is still narrow.
            property real natural: 0

            function measure(): void {
                let w = reactionFlow.leftPadding + reactionFlow.rightPadding;
                for (let i = 0; i < chipRepeater.count; ++i) {
                    const item = chipRepeater.itemAt(i);
                    if (item) {
                        w += item.implicitWidth + (i > 0 ? reactionFlow.spacing : 0);
                    }
                }
                reactionFlow.natural = Math.ceil(w);
            }

            visible: root.reactions.length > 0
            anchors.left: root.own ? undefined : parent.left
            anchors.right: root.own ? parent.right : undefined
            width: Math.min(reactionFlow.natural, root.maxBubble)
            // Below the bubble, clear of its edge.
            topPadding: Math.round(Kirigami.Units.smallSpacing / 2)
            spacing: Kirigami.Units.smallSpacing / 2
            layoutDirection: root.own ? Qt.RightToLeft : Qt.LeftToRight
            // (Paddings are not mirrored: the inset is on the side the bubble leans away from.)
            leftPadding: root.atRight ? 0 : Kirigami.Units.largeSpacing
            rightPadding: root.atRight ? Kirigami.Units.largeSpacing : 0

            Repeater {
                id: chipRepeater
                model: root.reactions
                onItemAdded: reactionFlow.measure()
                onItemRemoved: Qt.callLater(reactionFlow.measure)
                delegate: Rectangle {
                    id: chip

                    required property var modelData

                    onImplicitWidthChanged: reactionFlow.measure()

                    readonly property color fill: chip.modelData.mine ? Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, chipHover.hovered ? 0.36 : 0.25) : Kirigami.ColorUtils.linearInterpolation(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, chipHover.hovered ? 0.16 : 0.1)
                    // "Ari, You and Bo": everyone for a few, a count beyond that.
                    readonly property string who: {
                        const names = chip.modelData.names;
                        if (names.length <= 3) {
                            return names.join(", ");
                        }
                        return L10n.i18ncp("@info:tooltip, %2 names, %1 how many more", "%2 and %1 other", "%2 and %1 others", names.length - 2, names.slice(0, 2).join(", "));
                    }

                    implicitWidth: Math.max(implicitHeight, chipRow.implicitWidth + Kirigami.Units.largeSpacing * 1.5)
                    // At least the usual height, more when the emoji and count need it.
                    implicitHeight: Math.max(Math.round(Kirigami.Units.gridUnit * 1.55), Math.ceil(chipRow.implicitHeight) + Kirigami.Units.smallSpacing)
                    radius: height / 2
                    color: chip.fill
                    // Ours: an accent ring; another's: the frame of the theme's frame contrast.
                    border.width: chip.modelData.mine ? 1.5 : 1
                    border.color: chip.modelData.mine ? Kirigami.Theme.highlightColor : Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)

                    Accessible.role: Accessible.Button
                    Accessible.name: L10n.i18ncp("@info accessible, %2 emoji", "%2, %1 reaction", "%2, %1 reactions", chip.modelData.count, chip.modelData.emoji)
                    Accessible.description: chip.who
                    Accessible.onPressAction: chip.toggle()

                    QQC2.ToolTip.visible: chipHover.hovered
                    // Names and the reaction itself come from the network: never markup.
                    QQC2.ToolTip.text: Style.plain(L10n.i18nc("@info:tooltip, %1 names, %2 emoji", "%1 reacted with %2", chip.who, chip.modelData.emoji))
                    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay

                    // A click adds our reaction with this emoji, or takes it back.
                    function toggle(): void {
                        if (root.actions.react) {
                            App.react(root.messageId, chip.modelData.emoji, chip.modelData.mine);
                        } else {
                            root.openReactions();
                        }
                    }

                    HoverHandler {
                        id: chipHover
                        cursorShape: Qt.PointingHandCursor
                    }
                    TapHandler {
                        onTapped: chip.toggle()
                        onLongPressed: root.openReactions()
                    }
                    // Who reacted: a right click (or a long press) on a chip.
                    TapHandler {
                        acceptedButtons: Qt.RightButton
                        onTapped: root.openReactions()
                    }

                    Row {
                        id: chipRow
                        anchors.centerIn: parent
                        spacing: Kirigami.Units.smallSpacing / 2
                        QQC2.Label {
                            text: chip.modelData.emoji
                            textFormat: Text.PlainText
                            font.family: "emoji"
                            font.pointSize: Kirigami.Theme.defaultFont.pointSize * 1.05
                            anchors.verticalCenter: parent.verticalCenter
                        }
                        QQC2.Label {
                            visible: chip.modelData.count > 1
                            text: chip.modelData.count
                            font: Kirigami.Theme.smallFont
                            color: Kirigami.Theme.textColor
                            anchors.verticalCenter: parent.verticalCenter
                        }
                    }
                }
            }
        }
    }

    // The toolbar beside the bubble: react, reply, and the menu with everything else. It shows
    // under the pointer, on the row with the keyboard focus and while one of its popups is
    // open; where the window leaves no room beside the bubble, only the menu button.
    Row {
        id: toolbar

        readonly property real room: root.atRight ? column.x - root.sideMargin : root.width - root.sideMargin - (column.x + column.width)
        readonly property real buttonSize: moreButton.implicitWidth
        readonly property bool compact: toolbar.room < toolbar.buttonSize * 3 + Kirigami.Units.smallSpacing * 2

        visible: root.actions.any && (hover.hovered || root.current || root.active)
        y: Math.round(column.y + bubble.y + (bubble.height - height) / 2)
        x: Math.round(root.atRight ? column.x + column.width - bubble.width - width - Kirigami.Units.smallSpacing : column.x + bubble.width + Kirigami.Units.smallSpacing)
        spacing: 0
        layoutDirection: root.own ? Qt.RightToLeft : Qt.LeftToRight

        // (The buttons never take the keyboard focus: a click on one leaves it where the user
        // is typing, and the timeline is one tab stop. With the keyboard the same actions are
        // in the menu of the current row: Menu, Shift+F10 or Enter.)
        QQC2.ToolButton {
            id: reactButton
            visible: root.actions.react && !toolbar.compact
            focusPolicy: Qt.NoFocus
            icon.name: "smiley-add"
            display: QQC2.AbstractButton.IconOnly
            text: L10n.i18nc("@action:button", "Add Reaction")
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: text
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            onClicked: TimelineActions.picker(reactButton, root.rowInfo())
        }
        QQC2.ToolButton {
            visible: root.actions.reply && !toolbar.compact
            focusPolicy: Qt.NoFocus
            icon.name: "mail-reply-sender"
            display: QQC2.AbstractButton.IconOnly
            text: L10n.i18nc("@action:button", "Reply")
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: text
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            onClicked: root.reply()
        }
        QQC2.ToolButton {
            id: moreButton
            focusPolicy: Qt.NoFocus
            icon.name: "overflow-menu"
            display: QQC2.AbstractButton.IconOnly
            text: L10n.i18nc("@action:button", "More Actions")
            QQC2.ToolTip.visible: hovered
            QQC2.ToolTip.text: text
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            onClicked: TimelineActions.menu(moreButton, root.rowInfo(), 0, moreButton.height)
        }
    }

    Component.onCompleted: root.measure()
}
