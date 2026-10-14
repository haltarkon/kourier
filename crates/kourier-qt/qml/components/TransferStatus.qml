pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The history transfer of a newly linked account while it runs, in the account banner of the
// main window: waiting for the phone, downloading (a bar and the bytes), reading (a bar and
// the messages); after it the contacts and groups the account still fetches (a bar and what
// is left). The link page shows none: the chats are usable as soon as the account is linked.
// It is fed by the viewmodel's view (`App.bannerTransfer`): its line, how far the download
// is, and whether "Continue Without History" is offered (a wait or a download can be
// skipped, a read cannot).
//
// Every text is the application's own, except that the line can carry an account's name: it
// is a plain-text label.
ColumnLayout {
    id: root

    // The transfer to show (a map; its `phase` is empty when there is none).
    property var view: App.bannerTransfer

    readonly property string phase: root.view.phase ?? ""
    readonly property bool waiting: root.phase === "waiting"
    readonly property bool downloading: root.phase === "downloading"
    readonly property bool reading: root.phase === "reading"
    // After the transfer (or without one): the profiles and groups a fresh link still
    // fetches, counted against the most seen (banner only).
    readonly property bool updating: root.phase === "updating"
    // 0 … 1, or below zero while it is not known how far the transfer is.
    readonly property real progress: root.view.progress ?? -1
    readonly property string detail: root.view.detail ?? ""
    readonly property string skipLabel: root.view.skip ?? ""
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
    // The button that ends the wait or the download (tests press it).
    readonly property alias skipButton: skip

    spacing: Kirigami.Units.smallSpacing

    // What is going on: one quiet line.
    RowLayout {
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Accessible.role: Accessible.StatusBar
        Accessible.name: sentence.text

        Spinner {
            visible: root.waiting
            implicitWidth: Kirigami.Units.iconSizes.small
            implicitHeight: Kirigami.Units.iconSizes.small
            Layout.alignment: Qt.AlignTop
        }
        QQC2.Label {
            id: sentence
            text: root.view.status ?? ""
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: root.quiet
            font: Kirigami.Theme.smallFont
            Layout.fillWidth: true
        }
    }

    // The download with its bytes, the read with its messages, the update with what is left
    // (each without a measure until its first count). A count that steps back (the download
    // went on from the file on disk after a restart) is just a shorter bar.
    RowLayout {
        visible: root.downloading || root.reading || root.updating
        spacing: Kirigami.Units.largeSpacing
        Layout.fillWidth: true

        QQC2.ProgressBar {
            from: 0
            to: 1
            value: Math.max(0, Math.min(1, root.progress))
            // An indeterminate bar moves; in a picture taken for comparison (`--screenshot`)
            // it is an empty bar instead, the same picture every time.
            indeterminate: root.progress < 0 && App.screenshot === ""
            Accessible.name: sentence.text
            Layout.fillWidth: true
        }
        QQC2.Label {
            visible: root.detail !== ""
            text: root.detail
            textFormat: Text.PlainText
            color: root.quiet
            font: Kirigami.Theme.smallFont
        }
    }

    QQC2.Button {
        id: skip
        visible: root.skipLabel !== ""
        text: root.skipLabel
        // (An icon that points the way the text runs.)
        icon.name: root.LayoutMirroring.enabled ? "go-previous-skip" : "go-next-skip"
        Accessible.name: skip.text
        Layout.alignment: Qt.AlignRight
        onClicked: App.skipTransfer(root.view.account ?? 0)
    }
}
