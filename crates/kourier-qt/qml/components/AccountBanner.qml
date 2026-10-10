import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// What the accounts shown need the user to know or do (`App.bannerKind`): a quiet line while a
// state passes by itself (connecting, getting new messages, offline), a message with its
// action when the user has to act (link again, solve a challenge, set the clock, restart).
//
// The history transfer of a newly linked account shows here too (`App.bannerTransfer`), once
// the link page is gone (after a restart in the middle of one): while it runs, the same view
// the link page shows, compact, with its "Continue Without History"; after a failure, a
// message that stays until it is dismissed. Both are made when they are first needed.
//
// One account shows one thing: its state or its transfer, whichever presses more (the order
// of the viewmodel's `BannerKind`). The transfer is chosen apart from the states of the
// accounts, though, so with several accounts the state of one (logged out for weeks, say)
// does not hide the failed transfer of another, or its skip: then both show, the state above.
ColumnLayout {
    id: root

    readonly property string kind: App.bannerKind
    readonly property bool passing: root.kind === "connecting" || root.kind === "syncing" || root.kind === "offline"
    readonly property bool busy: root.kind === "connecting" || root.kind === "syncing"
    // The history transfer to show: `waiting`, `downloading`, `reading`, `failed`, or empty.
    readonly property string transferPhase: App.bannerTransfer.phase ?? ""
    // A history transfer: running, or failed.
    readonly property bool transfer: root.transferPhase !== ""
    // What shows it (`TransferStatus` or `TransferFailure`; null without a transfer).
    readonly property Item transferItem: transferLoader.item as Item

    visible: root.kind !== "" || root.transfer
    spacing: 0

    // Connecting, syncing, offline.
    RowLayout {
        visible: root.passing
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.smallSpacing
        Layout.rightMargin: Kirigami.Units.smallSpacing
        Accessible.role: Accessible.StatusBar
        Accessible.name: App.bannerText

        QQC2.BusyIndicator {
            visible: root.busy
            running: root.busy && root.visible
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
        }
        Kirigami.Icon {
            visible: !root.busy
            source: "network-disconnect"
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
        }
        QQC2.Label {
            text: App.bannerText
            font: Kirigami.Theme.smallFont
            color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
            wrapMode: Text.Wrap
            textFormat: Text.PlainText
            Layout.fillWidth: true
        }
    }

    // The user has to act.
    Kirigami.InlineMessage {
        visible: root.kind !== "" && !root.passing
        type: root.kind === "loggedOut" || root.kind === "error" ? Kirigami.MessageType.Error : Kirigami.MessageType.Warning
        // The text can carry the account's name and a reason the network gave.
        text: Style.plain(App.bannerText)
        showCloseButton: false
        Layout.fillWidth: true
        actions: [
            Kirigami.Action {
                visible: App.bannerAction !== ""
                text: App.bannerAction
                icon.name: {
                    switch (root.kind) {
                    case "loggedOut":
                        return "link";
                    case "challenge":
                        return "globe";
                    default:
                        return "view-refresh";
                    }
                }
                onTriggered: App.bannerActivate()
            }
        ]
    }

    // The history transfer. Loaded by name when one is first shown: nothing of it is made,
    // or even read, at the start.
    Loader {
        id: transferLoader

        readonly property string wanted: root.transferPhase === "" ? "" : (root.transferPhase === "failed" ? "TransferFailure.qml" : "TransferStatus.qml")
        property string loaded: ""

        function follow(): void {
            if (transferLoader.wanted === transferLoader.loaded) {
                return;
            }
            transferLoader.loaded = transferLoader.wanted;
            if (transferLoader.wanted === "") {
                transferLoader.source = "";
            } else if (transferLoader.wanted === "TransferStatus.qml") {
                transferLoader.setSource(Qt.resolvedUrl(transferLoader.wanted), {
                    "compact": true
                });
            } else {
                transferLoader.setSource(Qt.resolvedUrl(transferLoader.wanted));
            }
        }

        visible: root.transfer
        Layout.fillWidth: true
        // (Under the state of another account, when there is one.)
        Layout.topMargin: root.kind !== "" ? Kirigami.Units.smallSpacing : 0
        Layout.leftMargin: root.transferPhase !== "failed" ? Kirigami.Units.smallSpacing : 0
        Layout.rightMargin: root.transferPhase !== "failed" ? Kirigami.Units.smallSpacing : 0
        onWantedChanged: transferLoader.follow()
        Component.onCompleted: transferLoader.follow()
    }
}
