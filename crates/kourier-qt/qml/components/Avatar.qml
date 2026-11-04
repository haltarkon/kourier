pragma ComponentBehavior: Bound

import QtQuick
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import app.kourier

// An avatar with an optional network badge (bottom end corner; a pill with the account's
// number when there are several accounts of the network) and an optional connection state
// dot (top start corner). Its shape is a circle, or, with Settings → "Round avatars" off, a square
// whose corners have the theme's radius (`cornerRadius`). Pictures come already cut to that
// shape from the image provider (`image://kourier/avatar/…` takes the radius in the request,
// so every renderer shows them alike and no layer is needed to clip an `Image`); until one
// has loaded, or when it fails, the initials show, drawn here in the same shape. The badge and
// the dot sit on the avatar's corners and stay round.
Item {
    id: root

    property string name
    property string source
    // Network badge icon key (`icons/<badge>.svg`); empty for none.
    property string badge
    // The account's place among the accounts of the badge's network (1, 2, …), shown in the
    // badge; 0 for none.
    property int badgeNumber: 0
    // Account state (`online`, `connecting`, `offline`, `action`, `loggedOut`); empty for none.
    property string status
    property bool selfChat: false
    // A placeholder title without a name ("Signal user ⋯1f2e", a phone number): a person icon
    // instead of meaningless initials, colored by the id's tail.
    readonly property bool anonymous: root.bareName.indexOf("\u22EF") >= 0 || /^\+?[0-9 ()-]+$/.test(root.bareName)
    // The name without WhatsApp's "~" mark (`Style.unmarked`): the monogram, its glyph check, its
    // colour and the accessible name take this one, never the shown string (PRIV-1).
    readonly property string bareName: Style.unmarked(root.name)

    implicitWidth: Style.listAvatarSize
    implicitHeight: Style.listAvatarSize

    readonly property bool pictureShown: picture.status === Image.Ready && !root.selfChat

    // The side of the square the avatar fills.
    readonly property real side: Math.min(root.width, root.height)
    // The corner radius of the shape: half the side (a circle), or the theme's own corner
    // radius when avatars are not round (never more than a circle).
    readonly property real cornerRadius: App.settings.roundAvatars ? root.side / 2 : Math.min(Kirigami.Units.cornerRadius, root.side / 2)
    // The same in pixels of the size the provider is asked for: `sourceSize` below is the item's
    // size in logical pixels, which Qt multiplies by `Screen.devicePixelRatio` itself.
    readonly property int pictureRadius: Math.ceil(root.cornerRadius * Screen.devicePixelRatio)
    // The picture asked for from the provider, with the shape in the request: a change of the
    // setting is a new request, so avatars already on screen are cut again at once.
    readonly property string pictureSource: root.source.indexOf("image://kourier/avatar/") === 0 ? root.source + (root.source.indexOf("?") < 0 ? "?" : "&") + "r=" + root.pictureRadius : root.source
    // Initials, or the person glyph for a placeholder title and for a name that has no
    // initials, in the color of the name (of the id's tail for a placeholder title).
    readonly property bool glyph: root.anonymous || Components.NameUtils.isStringUnsuitableForInitials(root.bareName)
    readonly property color placeholderColor: Components.NameUtils.colorsFromString(root.anonymous ? root.bareName.slice(-4) : root.bareName)

    // The theme's corner as a share of the side of a list avatar: the notification pictures,
    // which are drawn below the UI, take their corners from it.
    Binding {
        target: App.settings
        property: "avatarCorner"
        value: Kirigami.Units.cornerRadius / Style.listAvatarSize
        restoreMode: Binding.RestoreNone
    }

    // Initials or the glyph on a tinted shape, as Kirigami Add-ons' Avatar draws them (its
    // outline is a circle that cannot be changed, so the shape is drawn here).
    Rectangle {
        id: placeholder
        objectName: "avatarShape"
        anchors.centerIn: parent
        width: root.side
        height: root.side
        visible: !root.selfChat && !root.pictureShown
        radius: root.cornerRadius
        border.width: 1.25
        border.color: root.placeholderColor
        color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, border.color, 0.07)
        Accessible.role: Accessible.Graphic
        Accessible.name: root.bareName

        Text {
            objectName: "avatarInitials"
            anchors.fill: parent
            visible: !root.glyph
            text: Components.NameUtils.initialsFromString(root.bareName)
            textFormat: Text.PlainText
            // (The palette's hues are pale ones too, and the tint under them is light: the
            // letters are made as dark, or light, as it takes to be read on it.)
            color: Style.readable(root.placeholderColor, placeholder.color)
            font {
                // (Not both a point and a pixel size.)
                pointSize: -1
                // Padding shrinks with a tiny avatar so the letters stay readable.
                pixelSize: Math.round((root.height - Math.max(0, Math.min(Kirigami.Units.largeSpacing, root.height - Kirigami.Units.largeSpacing * 2))) / 2)
            }
            fontSizeMode: Text.Fit
            verticalAlignment: Text.AlignVCenter
            horizontalAlignment: Text.AlignHCenter
        }

        Kirigami.Icon {
            anchors.centerIn: parent
            width: Math.round(parent.width * 0.5)
            height: width
            visible: root.glyph
            source: "user"
            color: root.placeholderColor
            isMask: true
        }
    }

    Image {
        id: picture
        anchors.fill: parent
        visible: root.pictureShown
        source: root.selfChat ? "" : root.pictureSource
        asynchronous: true
        smooth: true
        mipmap: true
        fillMode: Image.PreserveAspectFit
        sourceSize.width: Math.ceil(width)
        sourceSize.height: Math.ceil(height)
    }

    // Note to self: a bookmark-like glyph on the accent color.
    Rectangle {
        anchors.fill: parent
        visible: root.selfChat
        radius: root.cornerRadius
        color: Kirigami.Theme.highlightColor
        Kirigami.Icon {
            anchors.centerIn: parent
            width: parent.width * 0.5
            height: width
            source: "bookmarks"
            color: Kirigami.Theme.highlightedTextColor
            isMask: true
        }
    }

    // The network badge on the picture's bottom end corner: a disc of the background colour
    // with the network's icon; with `badgeNumber` (the account's place among the accounts of
    // its network) the disc grows into a pill toward the start side, the icon at its start
    // end and the digit at the end (his mock "B"). The whole badge is one `Loader`, so a row
    // without a badge (one account) makes no item for it. Anchors only: they mirror in RTL,
    // an explicit `x` would not.
    Loader {
        id: badgeView
        objectName: "avatarBadge"
        // The badge's side (and the pill's height), and the icon's inset.
        readonly property real side: Math.round(root.width * 0.42)
        readonly property real inset: Math.max(1, Math.round(side * 0.1))
        active: root.badge !== ""
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.rightMargin: -Math.round(side * 0.12)
        anchors.bottomMargin: -Math.round(side * 0.12)
        sourceComponent: Rectangle {
            readonly property bool numbered: root.badgeNumber > 0
            width: numbered ? badgeView.side + digit.implicitWidth + Math.round(badgeView.side * 0.18) : badgeView.side
            height: badgeView.side
            radius: height / 2
            color: Kirigami.Theme.backgroundColor
            // In a high-contrast scheme the pill is set off from the picture by a ring.
            border.width: numbered && Style.highContrast ? 1 : 0
            border.color: Kirigami.Theme.textColor
            // (An icon, not an `Image`: the first `Image` with a file or resource URL makes Qt
            // Quick list every image format it could read, which loads all installed image
            // plugins and their codec libraries at the start, PDF and camera raw included,
            // §1.3 and §8.6. Pictures come from the image provider, bundled icons through
            // `Kirigami.Icon`.)
            Kirigami.Icon {
                anchors.left: parent.left
                anchors.top: parent.top
                anchors.leftMargin: badgeView.inset
                anchors.topMargin: badgeView.inset
                width: badgeView.side - 2 * badgeView.inset
                height: width
                source: Style.icon(root.badge)
                smooth: true
            }
            Text {
                id: digit
                objectName: "avatarBadgeDigit"
                visible: parent.numbered
                anchors.right: parent.right
                anchors.rightMargin: Math.round(badgeView.side * 0.18)
                anchors.verticalCenter: parent.verticalCenter
                text: parent.numbered ? root.badgeNumber : ""
                textFormat: Text.PlainText
                color: Kirigami.Theme.textColor
                font.family: Kirigami.Theme.defaultFont.family
                font.weight: Font.DemiBold
                font.pixelSize: Math.max(1, Math.round(badgeView.side * 0.62))
            }
        }
    }

    // The connection state dot on the picture's top START corner (the rail's unread pill
    // takes the top end corner).
    Rectangle {
        objectName: "avatarStatusDot"
        visible: root.status !== ""
        width: Math.round(root.width * 0.3)
        height: width
        radius: width / 2
        anchors.left: parent.left
        anchors.top: parent.top
        anchors.leftMargin: -Math.round(width * 0.1)
        anchors.topMargin: -Math.round(width * 0.1)
        border.width: Math.max(1.5, Math.round(width * 0.18))
        border.color: Kirigami.Theme.backgroundColor
        color: {
            switch (root.status) {
            case "online":
                return Kirigami.Theme.positiveTextColor;
            case "connecting":
                return Kirigami.Theme.neutralTextColor;
            case "action":
            case "loggedOut":
                return Kirigami.Theme.negativeTextColor;
            default:
                return Kirigami.Theme.disabledTextColor;
            }
        }
    }
}
