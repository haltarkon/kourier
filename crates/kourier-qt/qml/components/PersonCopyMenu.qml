pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// What can be copied of a person shown in a list: the name, the number or ID their page may
// show, the link to them. Rust finds the number and the link, so a number the page may not show
// is never handed to QML; the window's toast says what came of it.
QQC2.Menu {
    id: root

    required property real account
    required property string user
    required property string name

    // In the window's scene (also on Wayland, and in headless screenshots).
    popupType: QQC2.Popup.Item

    QQC2.MenuItem {
        objectName: "personCopyName"
        icon.name: "edit-copy"
        text: L10n.i18nc("@action:inmenu", "Copy Name")
        onTriggered: App.copyShown(Style.unmarked(root.name))
    }
    QQC2.MenuItem {
        objectName: "personCopyNumber"
        icon.name: "edit-copy"
        text: L10n.i18nc("@action:inmenu", "Copy Number or ID")
        onTriggered: App.copyPerson("number", root.account, root.user)
    }
    QQC2.MenuItem {
        objectName: "personCopyLink"
        icon.name: "edit-copy"
        text: L10n.i18nc("@action:inmenu", "Copy Link")
        onTriggered: App.copyPerson("link", root.account, root.user)
    }
}
