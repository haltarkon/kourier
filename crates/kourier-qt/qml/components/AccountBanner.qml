import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// What the accounts shown need the user to know or do (`App.bannerKind`): a quiet line while a
// state passes by itself (connecting, getting new messages, offline) or to say that part of
// a pushed history did not arrive (it stays for the session), a message with its action when
// the user has to act (link again, solve a challenge, set the clock, restart).
//
// The history transfer of a newly linked account shows here too (`App.bannerTransfer`): the
// link page ends as soon as the core has the account, and the chats can be used while the
// transfer runs. While it runs, a compact line with its bar and "Continue Without History";
// after a failure, a message that stays until it is dismissed; after it (or without one),
// the contacts and groups the account still fetches, counted. Made when first needed.
//
// One account shows one thing: its state or its transfer, whichever presses more (the order
// of the viewmodel's `BannerKind`). The transfer is chosen apart from the states of the
// accounts, though, so with several accounts the state of one (logged out for weeks, say)
// does not hide the failed transfer of another, or its skip: then both show, the state above.
ColumnLayout {
    id: root

    readonly property string kind: App.bannerKind
    readonly property bool passing: root.kind === "connecting" || root.kind === "syncing" || root.kind === "offline"
    // A quiet line (no button, no colour): a passing state, or the lost part of a history.
    readonly property bool quiet: root.passing || root.kind === "historyLost"
    readonly property bool busy: root.kind === "connecting" || root.kind === "syncing"
    // The history transfer to show: `waiting`, `downloading`, `reading`, `failed`, or empty;
    // `updating` after it, or without one: the contacts and groups a new link still fetches;
    // `history` for a network that sends its history in pieces after the link.
    readonly property string transferPhase: App.bannerTransfer.phase ?? ""
    // A history transfer: running, or failed.
    readonly property bool transfer: root.transferPhase !== ""
    // What shows it (`TransferStatus` or `TransferFailure`; null without a transfer).
    readonly property Item transferItem: transferLoader.item as Item

    // The banner's button was pressed: the window acts (`App.bannerActivate`), after asking
    // when that would cancel a link in progress.
    signal activated

    visible: root.kind !== "" || root.transfer
    spacing: 0

    // Connecting, syncing, offline, and the lost part of a history.
    RowLayout {
        visible: root.quiet
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.smallSpacing
        Layout.rightMargin: Kirigami.Units.smallSpacing
        Accessible.role: Accessible.StatusBar
        Accessible.name: App.bannerText

        Spinner {
            visible: root.busy
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
        }
        Kirigami.Icon {
            visible: !root.busy
            source: root.kind === "historyLost" ? "dialog-information" : "network-disconnect"
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
        visible: root.kind !== "" && !root.quiet
        // A network the person turned off himself is information, not a warning.
        type: root.kind === "loggedOut" || root.kind === "error" ? Kirigami.MessageType.Error : (root.kind === "networkOff" ? Kirigami.MessageType.Information : Kirigami.MessageType.Warning)
        // The text can carry the account's name and a reason the network gave.
        text: Style.plain(App.bannerText)
        showCloseButton: false
        Layout.fillWidth: true
        actions: [
            Kirigami.Action {
                visible: App.bannerAction !== ""
                enabled: visible
                text: App.bannerAction
                icon.name: {
                    switch (root.kind) {
                    case "loggedOut":
                        return "link";
                    case "challenge":
                        return "globe";
                    case "networkOff":
                        return "network-connect";
                    default:
                        return "view-refresh";
                    }
                }
                onTriggered: root.activated()
            },
            // The page the network offers about a ban or a logout (an `https://` address,
            // opened after this click and nowhere else).
            Kirigami.Action {
                id: appealAction
                visible: App.bannerAppealUrl !== ""
                enabled: visible
                text: App.bannerAppealLabel
                icon.name: "globe"
                onTriggered: App.openExternalUrl(App.bannerAppealUrl)
            }
        ]
    }

    // What the network itself said about a ban or a logout: plain text, one line, under
    // Kourier's own sentence (never markup, never logged).
    QQC2.Label {
        id: explanation
        visible: root.kind !== "" && !root.quiet && App.bannerExplanation !== ""
        text: App.bannerExplanation
        textFormat: Text.PlainText
        wrapMode: Text.Wrap
        color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
        font: Kirigami.Theme.smallFont
        Layout.fillWidth: true
        Layout.leftMargin: Kirigami.Units.largeSpacing
        Layout.rightMargin: Kirigami.Units.largeSpacing
        Layout.topMargin: Kirigami.Units.smallSpacing
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
