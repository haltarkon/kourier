import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.delegates as Delegates
import app.kourier

// One conversation-list row: avatar with network badge, title with pin/mute marks and the
// account chip, time, one-line preview (or typing / draft), unread pill or own receipt. The
// compact list (Settings → General) shows one line per chat: a smaller avatar, the title, the
// time and the unread pill.
Delegates.RoundedItemDelegate {
    id: root

    required property int index
    required property real conversationId
    required property string title
    required property string avatarUrl
    required property string previewAuthor
    required property string previewSource
    required property string preview
    required property string previewIcon
    required property string typing
    required property string timeLabel
    required property int unread
    required property int mentions
    required property bool markedUnread
    required property bool muted
    required property bool pinned
    required property string receipt
    required property string accountBadge
    required property string badgeIcon
    // The account's place among its network's accounts, on the avatar's badge (0: none).
    required property int badgeNumber
    required property bool isSelf
    required property bool isGroup
    // The color of the chat's account.
    required property string tint
    // The RegExp filters' marker backgrounds in the title: `[{start, len, colour}]` (R3).
    required property var titleMarks

    readonly property bool hasUnread: root.unread > 0 || root.markedUnread
    readonly property bool isDraft: root.previewSource === "draft"
    readonly property bool compact: App.settings.compact
    // The split's rail (`ConversationListPage.rail`): only the avatar, with the unread count on it.
    property bool rail: false

    // The row's context menu was asked for, at (`x`, `y`) of the row (the page owns the menu;
    // the keyboard's Menu key reaches it through the list).
    signal menuRequested(real x, real y)
    // The hover button's two meanings: open this chat beside the open one, or close it (it is shown in a pane).
    signal besideRequested
    signal closeRequested

    // What the row's text stands on: the page, or the row's own tint while it is the open
    // chat (RoundedItemDelegate tints the background with the accent by 0.3). The quiet and
    // accent text colors are made readable on that.
    readonly property color surface: root.highlighted ? Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, 0.3) : (root.tinted ? Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, root.tint, root.wash) : Kirigami.Theme.backgroundColor)
    // The faint wash of the account's color (Settings → General). The open chat keeps its own tint.
    readonly property bool tinted: App.settings.tintRows && root.tint !== "" && !root.highlighted && !Style.highContrast
    readonly property real wash: Style.tintStrength(root.tint, Kirigami.Theme.backgroundColor, 0.08, 0.45)
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, root.surface)
    readonly property color accent: Style.readable(Kirigami.Theme.highlightColor, root.surface)

    // The row's height is what it holds (the avatar, two lines of the theme's fonts), at least
    // the usual size, so a larger font grows the row.
    implicitHeight: Math.max(root.compact ? Kirigami.Units.gridUnit * 2.5 : Kirigami.Units.gridUnit * 3.6, Math.ceil(root.implicitContentHeight + root.topPadding + root.bottomPadding))
    // A screen reader says the chat's name, then what the row shows besides it: the news, the
    // marks and the last message.
    Accessible.role: Accessible.ListItem
    Accessible.name: Style.unmarked(root.title)
    Accessible.description: {
        const parts = [];
        // With several accounts shown the row's chip names its account.
        if (root.accountBadge !== "") {
            parts.push(L10n.i18nc("@info accessible, %1 the account of the chat", "Account %1", root.accountBadge));
        }
        if (root.unread > 0) {
            parts.push(L10n.i18ncp("@info accessible, %1 how many", "%1 unread message", "%1 unread messages", root.unread));
        } else if (root.markedUnread) {
            parts.push(L10n.i18nc("@info accessible", "Marked as unread"));
        }
        if (root.mentions > 0) {
            parts.push(L10n.i18nc("@info accessible", "You were mentioned"));
        }
        if (root.muted) {
            parts.push(L10n.i18nc("@info accessible", "Muted"));
        }
        if (root.pinned) {
            parts.push(L10n.i18nc("@info accessible", "Pinned"));
        }
        if (root.typing !== "") {
            parts.push(root.typing);
        } else if (root.preview !== "") {
            parts.push(root.previewAuthor !== "" && (root.previewSource === "peer" || root.isDraft) ? root.previewAuthor + ": " + root.preview : root.preview);
        }
        if (root.timeLabel !== "") {
            parts.push(root.timeLabel);
        }
        return parts.join(". ");
    }
    Accessible.selected: root.highlighted
    // The rail's row has no words: the chat's name is its tooltip (the accessible name is the same).
    QQC2.ToolTip.visible: root.rail && root.hovered
    QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
    QQC2.ToolTip.text: root.rail ? Style.plain(root.title) : ""
    Accessible.onPressAction: root.clicked()
    // The list holds the keyboard for all its rows (one tab stop; see the page).
    focusPolicy: Qt.NoFocus

    // The context menu: right click, or a long press on a touch screen (a held mouse button
    // is no request). Menu and Shift+F10 on the current row are the list's.
    TapHandler {
        acceptedButtons: Qt.RightButton
        onTapped: point => root.menuRequested(point.position.x, point.position.y)
    }
    TapHandler {
        id: longPress
        acceptedDevices: PointerDevice.TouchScreen | PointerDevice.Stylus
        onLongPressed: root.menuRequested(longPress.point.position.x, longPress.point.position.y)
    }

    // The hover button (split view): ONE `Loader`, active only while the row is hovered, a chat is open and the
    // list is not the rail (rows are made many times: no item per row otherwise). The row stays `hovered` while the
    // pointer is on the button (Qt keeps the hover of a hovered item's ancestors), so `active` does not read the
    // button's own `hovered` (that would bind `active` to the item it makes). The click is the button's own
    // (the press goes to the topmost item, so the row is not clicked); the button never takes the keyboard (the
    // row menu is the keyboard's way). The pane test is read while the button shows and follows the panes.
    Loader {
        id: hoverButton

        active: !root.rail && root.hovered && App.timeline.conversationId !== 0
        anchors.right: parent.right
        anchors.rightMargin: Kirigami.Units.smallSpacing
        anchors.verticalCenter: parent.verticalCenter
        sourceComponent: QQC2.ToolButton {
            id: button

            // The panes' chats and the split are what this reads: `paneOf` has no change signal.
            readonly property int pane: {
                App.split;
                App.activePane;
                App.timeline.conversationId;
                App.sideTimeline.conversationId;
                return App.paneOf(root.conversationId);
            }
            readonly property bool closes: button.pane >= 0

            objectName: "rowHoverButton"
            flat: true
            display: QQC2.AbstractButton.IconOnly
            focusPolicy: Qt.NoFocus
            icon.name: button.closes ? "view-close" : "view-split-left-right"
            text: button.closes ? L10n.i18nc("@action:button", "Close Chat") : L10n.i18nc("@action:button", "Open in Split View")
            Accessible.name: button.text
            QQC2.ToolTip.visible: button.hovered
            QQC2.ToolTip.delay: Kirigami.Units.toolTipDelay
            QQC2.ToolTip.text: button.text
            // (A plain fill under the icon: the button lies over the time and the pills.) Its size does not hang on
            // the icon: without the theme's icon the button was its padding alone (12 px), and a scroll bar laid over
            // the row's end took the click.
            background: Rectangle {
                implicitWidth: Kirigami.Units.iconSizes.smallMedium + Kirigami.Units.smallSpacing * 2
                implicitHeight: Kirigami.Units.iconSizes.smallMedium + Kirigami.Units.smallSpacing * 2
                radius: Kirigami.Units.smallSpacing
                color: button.down ? Kirigami.ColorUtils.tintWithAlpha(root.surface, Kirigami.Theme.highlightColor, 0.3) : root.surface
                border.width: 1
                border.color: Style.frame(root.surface, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
            }
            onClicked: {
                if (button.closes) {
                    root.closeRequested();
                } else {
                    root.besideRequested();
                }
            }
        }
    }

    // The row the keyboard is on (the list is one tab stop and holds the focus itself); and
    // in a high-contrast scheme the open chat, whose tint alone does not set the row off
    // from the page: a solid frame in the text color. The wash of the account's color is the same
    // item, under the row's content: one item per row for all three.
    RowFrame {
        tint: root.tint
        tinted: root.tinted
        wash: root.wash
        focused: root.ListView.isCurrentItem && root.ListView.view.activeFocus
        framed: focused || (Style.highContrast && root.highlighted)
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing

        Avatar {
            name: root.title
            source: root.avatarUrl
            badge: root.badgeIcon
            badgeNumber: root.badgeNumber
            selfChat: root.isSelf
            Layout.alignment: Qt.AlignVCenter | Qt.AlignHCenter
            Layout.preferredWidth: root.compact ? Style.compactAvatarSize : Style.listAvatarSize
            Layout.preferredHeight: root.compact ? Style.compactAvatarSize : Style.listAvatarSize
            // (In the rail the avatar's cell takes the row's width and the avatar stands in its middle.)
            Layout.maximumWidth: root.compact ? Style.compactAvatarSize : Style.listAvatarSize
            Layout.fillWidth: root.rail

            // The unread count of the rail on the avatar's top end corner: one `Loader`, active only in the
            // rail and with news (a row of the full list makes no item for it).
            Loader {
                active: root.rail && root.hasUnread
                anchors.right: parent.right
                anchors.top: parent.top
                anchors.rightMargin: -Kirigami.Units.smallSpacing
                anchors.topMargin: -Kirigami.Units.smallSpacing
                sourceComponent: UnreadPill {
                    count: root.unread
                    muted: root.muted
                    dot: root.unread === 0
                }
            }
        }

        ColumnLayout {
            visible: !root.rail
            spacing: Kirigami.Units.smallSpacing / 2
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignVCenter

            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                // Title, then the account chip and the mute mark right after it.
                Item {
                    implicitHeight: titleLabel.implicitHeight
                    Layout.fillWidth: true

                    Row {
                        id: titleRow
                        width: parent.width
                        spacing: Kirigami.Units.smallSpacing

                        QQC2.Label {
                            id: titleLabel
                            // A marked title is a `RichText` label cut by `titleFit` (`Style.markedName`).
                            readonly property bool marked: root.titleMarks.length > 0
                            width: Math.min(implicitWidth, titleRow.width - marks.implicitWidth - (marks.visible ? titleRow.spacing : 0))
                            text: titleLabel.marked ? Style.markedName(titleFit.elidedText, root.title, root.titleMarks, root.surface, Kirigami.Theme.textColor) : root.title
                            elide: Text.ElideRight
                            font: Style.styled(Kirigami.Theme.defaultFont, {
                                "weight": root.hasUnread ? Font.DemiBold : Font.Normal
                            })
                            textFormat: titleLabel.marked ? Text.RichText : Text.PlainText
                            Accessible.name: Style.unmarked(root.title)

                            TextMetrics {
                                id: titleFit
                                font: titleLabel.font
                                text: titleLabel.marked ? root.title : ""
                                elide: Qt.ElideRight
                                // (What the row gives the title, not the label's own width: that one
                                // follows the label's implicit width.)
                                elideWidth: titleRow.width - marks.implicitWidth - (marks.visible ? titleRow.spacing : 0)
                            }
                        }
                        Row {
                            id: marks
                            visible: root.accountBadge !== "" || root.muted
                            spacing: Kirigami.Units.smallSpacing
                            anchors.verticalCenter: titleLabel.verticalCenter

                            QQC2.Label {
                                visible: root.accountBadge !== ""
                                text: root.accountBadge
                                textFormat: Text.PlainText
                                font: Kirigami.Theme.smallFont
                                color: root.quiet
                                leftPadding: Kirigami.Units.smallSpacing
                                rightPadding: Kirigami.Units.smallSpacing
                                anchors.verticalCenter: parent.verticalCenter
                                background: Rectangle {
                                    radius: height / 2
                                    color: "transparent"
                                    border.width: 1
                                    border.color: Style.frame(root.surface, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                                }
                            }
                            Kirigami.Icon {
                                visible: root.muted
                                source: "notifications-disabled"
                                width: Kirigami.Units.iconSizes.small
                                height: Kirigami.Units.iconSizes.small
                                color: root.quiet
                                isMask: true
                                anchors.verticalCenter: parent.verticalCenter
                            }
                        }
                    }
                }
                QQC2.Label {
                    text: root.timeLabel
                    font: Kirigami.Theme.smallFont
                    color: root.unread > 0 && !root.muted ? root.accent : root.quiet
                }
                // Compact rows have no second line: the pin mark moves up here. (Beside the
                // unread pill too: a pinned chat stays marked as pinned when it has news.)
                Kirigami.Icon {
                    visible: root.compact && root.pinned
                    source: "pin"
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: root.quiet
                    isMask: true
                }
                UnreadPill {
                    visible: root.compact && root.mentions > 0
                    mention: true
                    muted: false
                }
                UnreadPill {
                    visible: root.compact && root.hasUnread
                    count: root.unread
                    muted: root.muted
                    dot: root.unread === 0
                }
            }

            RowLayout {
                visible: !root.compact
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                ReceiptIcon {
                    receipt: root.typing === "" && root.previewSource === "own" ? root.receipt : ""
                    tint: root.quiet
                    readTint: root.accent
                }
                QQC2.Label {
                    visible: root.typing === "" && (root.previewSource === "peer" || root.isDraft)
                    text: L10n.i18nc("@item:intext, %1 who wrote the message that the row shows", "%1:", root.previewAuthor)
                    color: root.isDraft ? Kirigami.Theme.negativeTextColor : Kirigami.Theme.textColor
                    textFormat: Text.PlainText
                }
                Kirigami.Icon {
                    visible: root.typing === "" && root.previewIcon !== ""
                    source: root.previewIcon
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: root.quiet
                    isMask: true
                }
                QQC2.Label {
                    text: root.typing !== "" ? root.typing : root.preview
                    elide: Text.ElideRight
                    maximumLineCount: 1
                    textFormat: Text.PlainText
                    font: Style.styled(Kirigami.Theme.defaultFont, {
                        "italic": root.typing !== ""
                    })
                    color: root.typing !== "" ? root.accent : root.quiet
                    Layout.fillWidth: true
                }
                Kirigami.Icon {
                    visible: root.pinned
                    source: "pin"
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: root.quiet
                    isMask: true
                }
                UnreadPill {
                    visible: root.mentions > 0
                    mention: true
                    muted: false
                }
                UnreadPill {
                    visible: root.hasUnread
                    count: root.unread
                    muted: root.muted
                    // Marked unread without a count: a dot.
                    dot: root.unread === 0
                }
            }
        }
    }
}
