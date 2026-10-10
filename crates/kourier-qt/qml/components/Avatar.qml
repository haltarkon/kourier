import QtQuick
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.components as Components
import app.kourier

// An avatar with an optional network badge (bottom right) and an optional connection state
// dot (top right). Its shape is a circle, or, with Settings → "Round avatars" off, a square
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
    // Account state (`online`, `connecting`, `offline`, `action`, `loggedOut`); empty for none.
    property string status
    property bool selfChat: false
    // A placeholder title without a name ("Signal user ⋯1f2e", a phone number): a person icon
    // instead of meaningless initials, colored by the id's tail.
    readonly property bool anonymous: root.name.indexOf("\u22EF") >= 0 || /^\+?[0-9 ()-]+$/.test(root.name)

    implicitWidth: Style.listAvatarSize
    implicitHeight: Style.listAvatarSize

    readonly property bool pictureShown: picture.status === Image.Ready && !root.selfChat

    // The side of the square the avatar fills.
    readonly property real side: Math.min(root.width, root.height)
    // The corner radius of the shape: half the side (a circle), or the theme's own corner
    // radius when avatars are not round (never more than a circle).
    readonly property real cornerRadius: App.settings.roundAvatars ? root.side / 2 : Math.min(Kirigami.Units.cornerRadius, root.side / 2)
    // The same in pixels of the picture's requested size (`sourceSize` below), which is
    // `Screen.devicePixelRatio` times the item's size.
    readonly property int pictureRadius: Math.ceil(root.cornerRadius * Screen.devicePixelRatio)
    // The picture asked for from the provider, with the shape in the request: a change of the
    // setting is a new request, so avatars already on screen are cut again at once.
    readonly property string pictureSource: root.source.indexOf("image://kourier/avatar/") === 0 ? root.source + (root.source.indexOf("?") < 0 ? "?" : "&") + "r=" + root.pictureRadius : root.source
    // Initials, or the person glyph for a placeholder title and for a name that has no
    // initials, in the color of the name (of the id's tail for a placeholder title).
    readonly property bool glyph: root.anonymous || Components.NameUtils.isStringUnsuitableForInitials(root.name)
    readonly property color placeholderColor: Components.NameUtils.colorsFromString(root.anonymous ? root.name.slice(-4) : root.name)

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
        anchors.centerIn: parent
        width: root.side
        height: root.side
        visible: !root.selfChat && !root.pictureShown
        radius: root.cornerRadius
        border.width: 1.25
        border.color: root.placeholderColor
        color: Kirigami.ColorUtils.tintWithAlpha(Kirigami.Theme.backgroundColor, border.color, 0.07)
        Accessible.role: Accessible.Graphic
        Accessible.name: root.name

        Text {
            anchors.fill: parent
            visible: !root.glyph
            text: Components.NameUtils.initialsFromString(root.name)
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
        sourceSize.width: Math.ceil(width * Screen.devicePixelRatio)
        sourceSize.height: Math.ceil(height * Screen.devicePixelRatio)
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

    Rectangle {
        visible: root.badge !== ""
        width: Math.round(root.width * 0.42)
        height: width
        radius: width / 2
        anchors.right: parent.right
        anchors.bottom: parent.bottom
        anchors.rightMargin: -Math.round(width * 0.12)
        anchors.bottomMargin: -Math.round(width * 0.12)
        color: Kirigami.Theme.backgroundColor
        // (An icon, not an `Image`: the first `Image` with a file or resource URL makes Qt
        // Quick list every image format it could read, which loads all installed image
        // plugins and their codec libraries at the start, PDF and camera raw included,
        // §1.3 and §8.6. Pictures come from the image provider, bundled icons through
        // `Kirigami.Icon`.)
        Kirigami.Icon {
            anchors.fill: parent
            anchors.margins: Math.max(1, Math.round(parent.width * 0.1))
            source: root.badge !== "" ? Style.icon(root.badge) : ""
            smooth: true
        }
    }

    Rectangle {
        visible: root.status !== ""
        width: Math.round(root.width * 0.3)
        height: width
        radius: width / 2
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.rightMargin: -Math.round(width * 0.1)
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
