import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.delegates as Delegates
import app.kourier

// One hit of the message search, under the chats of the list (G1h): laid out like a chat row.
// The avatar of the chat, its name with the account chip and the time of the message; then
// one line, the sender and the line of the message that holds the hit, the match in bold.
// A click opens the chat AT the message (the page decides how; the list stays as it is).
// While one chat is searched (`oneChat`, the chip stands) the row's avatar and title are the
// SENDER's (the chat is named by the chip), and the "Name:" before the line is left out (F4).
Delegates.RoundedItemDelegate {
    id: root

    required property int index
    required property real message
    required property real conversationId
    required property string title
    required property string avatarUrl
    required property string author
    // The line with the match in <b>, escaped by the viewmodel (StyledText, never AutoText).
    required property string snippetHtml
    required property string snippetPlain
    // (The role `icon` is read through the model: `icon` is the button's own final property.)
    required property var model
    required property string timeLabel
    required property string accountBadge
    required property string badgeIcon
    required property int badgeNumber
    required property bool isSelf
    // The color of the chat's account, and the sender (whole name, "You" for an own message).
    required property string tint
    required property string senderName
    required property string senderAvatarUrl
    // One chat is searched: the sender is the row's title.
    property bool oneChat: false

    // An icon before the line (a file's name), else empty.
    readonly property string lineIcon: root.model.icon ?? ""
    readonly property bool compact: App.settings.compact
    // What the row stands on, as the chat row's (the account's faint wash while "tint rows" is on).
    readonly property bool tinted: App.settings.tintRows && root.tint !== "" && !Style.highContrast
    readonly property real wash: Style.tintStrength(root.tint, Kirigami.Theme.backgroundColor, 0.08, 0.45)
    readonly property color surface: root.tinted ? Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, root.tint, root.wash) : Kirigami.Theme.backgroundColor
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, root.surface)
    readonly property string shownTitle: root.oneChat ? root.senderName : root.title

    // The row's height is what it holds, at least a chat row's (a larger font grows it).
    implicitHeight: Math.max(root.compact ? Kirigami.Units.gridUnit * 2.5 : Kirigami.Units.gridUnit * 3.6, Math.ceil(root.implicitContentHeight + root.topPadding + root.bottomPadding))
    // A screen reader says the chat, the line that holds the hit (who wrote it first) and the
    // time; the account when the row shows its chip.
    Accessible.role: Accessible.ListItem
    Accessible.name: {
        const line = root.author !== "" && !root.oneChat ? root.author + ": " + root.snippetPlain : root.snippetPlain;
        return [Style.unmarked(root.shownTitle), line, root.timeLabel].filter(p => p !== "").join(". ");
    }
    Accessible.description: root.accountBadge !== "" ? L10n.i18nc("@info accessible, %1 the account of the chat", "Account %1", root.accountBadge) : ""
    Accessible.onPressAction: root.clicked()
    // The list holds the keyboard (one tab stop); the page moves it onto a hit with the arrows.
    focusPolicy: Qt.NoFocus
    activeFocusOnTab: false

    // The hit the keyboard is on: the frame a chat row shows for the current row, and the wash.
    RowFrame {
        tint: root.tint
        tinted: root.tinted
        wash: root.wash
        focused: root.activeFocus
        framed: root.activeFocus
    }

    contentItem: RowLayout {
        spacing: Kirigami.Units.largeSpacing

        Avatar {
            name: root.shownTitle
            source: root.oneChat ? root.senderAvatarUrl : root.avatarUrl
            badge: root.oneChat ? "" : root.badgeIcon
            badgeNumber: root.oneChat ? 0 : root.badgeNumber
            selfChat: !root.oneChat && root.isSelf
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

                // The chat's name, then the account chip right after it.
                Item {
                    implicitHeight: titleLabel.implicitHeight
                    Layout.fillWidth: true

                    Row {
                        id: titleRow
                        width: parent.width
                        spacing: Kirigami.Units.smallSpacing

                        QQC2.Label {
                            id: titleLabel
                            width: Math.min(implicitWidth, titleRow.width - (chip.visible ? chip.implicitWidth + titleRow.spacing : 0))
                            text: root.shownTitle
                            elide: Text.ElideRight
                            textFormat: Text.PlainText
                        }
                        QQC2.Label {
                            id: chip
                            visible: root.accountBadge !== ""
                            text: root.accountBadge
                            textFormat: Text.PlainText
                            font: Kirigami.Theme.smallFont
                            color: root.quiet
                            leftPadding: Kirigami.Units.smallSpacing
                            rightPadding: Kirigami.Units.smallSpacing
                            anchors.verticalCenter: titleLabel.verticalCenter
                            background: Rectangle {
                                radius: height / 2
                                color: "transparent"
                                border.width: 1
                                border.color: Style.frame(root.surface, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
                            }
                        }
                    }
                }
                QQC2.Label {
                    text: root.timeLabel
                    font: Kirigami.Theme.smallFont
                    color: root.quiet
                    textFormat: Text.PlainText
                }
            }

            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                QQC2.Label {
                    visible: root.author !== "" && !root.oneChat
                    text: L10n.i18nc("@item:intext, %1 who wrote the message that the row shows", "%1:", root.author)
                    textFormat: Text.PlainText
                    // A long name never takes the line from the snippet (about 40 % of the row).
                    elide: Text.ElideRight
                    Layout.maximumWidth: root.width * 0.4
                }
                Kirigami.Icon {
                    visible: root.lineIcon !== ""
                    source: root.lineIcon
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: root.quiet
                    isMask: true
                }
                QQC2.Label {
                    objectName: "snippet"
                    text: root.snippetHtml
                    textFormat: Text.StyledText
                    elide: Text.ElideRight
                    maximumLineCount: 1
                    color: root.quiet
                    Layout.fillWidth: true
                }
            }
        }
    }
}
