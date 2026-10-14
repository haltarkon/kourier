import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

// The history transfer of a newly linked account failed (the `phase` of
// `App.bannerTransfer` is `failed`): what happened and what can be done, in one sentence per
// kind of failure. It stays, also across restarts, until the user acts: "Dismiss" makes the backend
// forget the failure; "Try Again…" opens the question about removing the account from this
// computer and linking it again (not offered where only a newer Kourier can help).
//
// The sentence can carry an account's name: it goes through `Style.plain()`.
Kirigami.InlineMessage {
    id: root

    // The failed transfer (a map, as `App.bannerTransfer` gives it).
    property var view: App.bannerTransfer

    readonly property real account: root.view.account ?? 0
    readonly property string retryLabel: root.view.retry ?? ""
    readonly property string dismissLabel: root.view.dismiss ?? ""
    // The two actions (tests trigger them).
    readonly property alias retryAction: retry
    readonly property alias dismissAction: dismiss

    visible: true
    type: Kirigami.MessageType.Warning
    text: Style.plain(root.view.status ?? "")
    showCloseButton: false
    actions: [
        Kirigami.Action {
            id: retry
            visible: root.retryLabel !== ""
            enabled: visible
            text: root.retryLabel
            icon.name: "view-refresh"
            onTriggered: App.askReplace(root.account)
        },
        Kirigami.Action {
            id: dismiss
            visible: root.dismissLabel !== ""
            enabled: visible
            text: root.dismissLabel
            icon.name: "dialog-close"
            onTriggered: App.dismissTransfer(root.account)
        }
    ]
}
