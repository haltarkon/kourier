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
    required property bool isSelf

    readonly property bool hasUnread: root.unread > 0 || root.markedUnread
    readonly property bool isDraft: root.previewSource === "draft"
    readonly property bool compact: App.settings.compact

    // The row's context menu was asked for, at (`x`, `y`) of the row (the page owns the menu;
    // the keyboard's Menu key reaches it through the list).
    signal menuRequested(real x, real y)

    // What the row's text stands on: the page, or the row's own tint while it is the open
    // chat (RoundedItemDelegate tints the background with the accent by 0.3). The quiet and
    // accent text colors are made readable on that.
    readonly property color surface: root.highlighted ? Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, Kirigami.Theme.highlightColor, 0.3) : Kirigami.Theme.backgroundColor
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, root.surface)
    readonly property color accent: Style.readable(Kirigami.Theme.highlightColor, root.surface)

    // The row's height is what it holds (the avatar, two lines of the theme's fonts), at least
    // the usual size, so a larger font grows the row.
    implicitHeight: Math.max(root.compact ? Kirigami.Units.gridUnit * 2.5 : Kirigami.Units.gridUnit * 3.6, Math.ceil(root.implicitContentHeight + root.topPadding + root.bottomPadding))
    // A screen reader says the chat's name, then what the row shows besides it: the news, the
    // marks and the last message.
    Accessible.role: Accessible.ListItem
    Accessible.name: root.title
    Accessible.description: {
        const parts = [];
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

    // The row the keyboard is on (the list is one tab stop and holds the focus itself); and
    // in a high-contrast scheme the open chat, whose tint alone does not set the row off
    // from the page: a solid frame in the text color. One item for both, the focus first.
    Rectangle {
        readonly property bool focused: root.ListView.isCurrentItem && root.ListView.view.activeFocus

        anchors.fill: parent
        visible: focused || (Style.highContrast && root.highlighted)
        radius: Kirigami.Units.cornerRadius
        color: "transparent"
        border.width: 2
        border.color: focused ? Kirigami.Theme.focusColor : Kirigami.Theme.textColor
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing

        Avatar {
            name: root.title
            source: root.avatarUrl
            badge: root.badgeIcon
            selfChat: root.isSelf
            Layout.alignment: Qt.AlignVCenter
            Layout.preferredWidth: root.compact ? Style.compactAvatarSize : Style.listAvatarSize
            Layout.preferredHeight: root.compact ? Style.compactAvatarSize : Style.listAvatarSize
        }

        ColumnLayout {
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
                            width: Math.min(implicitWidth, titleRow.width - marks.implicitWidth - (marks.visible ? titleRow.spacing : 0))
                            text: root.title
                            elide: Text.ElideRight
                            font: Style.styled(Kirigami.Theme.defaultFont, {
                                "weight": root.hasUnread ? Font.DemiBold : Font.Normal
                            })
                            textFormat: Text.PlainText
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
                    text: root.previewAuthor + ":"
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
