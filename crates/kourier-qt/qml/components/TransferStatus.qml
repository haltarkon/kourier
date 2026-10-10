pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// The history transfer of a newly linked account while it runs: waiting for the phone,
// downloading (a bar and the bytes), reading. One presentation in two sizes: large on the
// link page, in place of its "linking" step, and compact in the account banner of the main
// window, where it shows after a restart in the middle of a transfer. Both are fed by the
// same view of the viewmodel (`App.linkTransfer`, `App.bannerTransfer`): its texts, how far
// the download is, and whether "Continue Without History" is offered (a wait or a download
// can be skipped, a read cannot).
//
// Every text is the application's own, except that the compact line can carry an account's
// name: it is a plain-text label.
ColumnLayout {
    id: root

    // The small presentation of the account banner.
    property bool compact: false
    // The transfer to show (a map; its `phase` is empty when there is none): the banner's by
    // itself, the link page hands in the one of the account it links.
    property var view: root.compact ? App.bannerTransfer : ({})

    readonly property string phase: root.view.phase ?? ""
    readonly property bool waiting: root.phase === "waiting"
    readonly property bool downloading: root.phase === "downloading"
    readonly property bool reading: root.phase === "reading"
    // 0 … 1, or below zero while it is not known how far the transfer is.
    readonly property real progress: root.view.progress ?? -1
    readonly property string detail: root.view.detail ?? ""
    readonly property string skipLabel: root.view.skip ?? ""
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
    // The button that ends the wait or the download (tests press it).
    readonly property alias skipButton: skip

    // "Busy" of the size `side`. In a picture taken for comparison (`--screenshot`) it stands
    // still: the same picture every time.
    component Spinner: Item {
        id: spinner

        property real side: Kirigami.Units.iconSizes.small
        readonly property bool still: App.screenshot !== ""

        implicitWidth: spinner.side
        implicitHeight: spinner.side

        QQC2.BusyIndicator {
            anchors.fill: parent
            visible: !spinner.still
            running: spinner.visible && !spinner.still
        }
        Kirigami.Icon {
            anchors.fill: parent
            visible: spinner.still
            source: "process-working-symbolic"
        }
    }

    spacing: root.compact ? Kirigami.Units.smallSpacing : Kirigami.Units.largeSpacing

    Kirigami.Icon {
        visible: !root.compact
        source: root.waiting ? "smartphone-symbolic" : (root.downloading ? "download" : "document-import")
        implicitWidth: Kirigami.Units.gridUnit * 4
        implicitHeight: Kirigami.Units.gridUnit * 4
        Layout.alignment: Qt.AlignHCenter
    }

    Kirigami.Heading {
        visible: !root.compact
        text: root.view.title ?? ""
        textFormat: Text.PlainText
        level: 2
        wrapMode: Text.Wrap
        horizontalAlignment: Text.AlignHCenter
        Layout.fillWidth: true
    }

    // What is going on: a sentence on the link page, one quiet line in the banner.
    RowLayout {
        spacing: Kirigami.Units.smallSpacing
        Layout.fillWidth: true
        Accessible.role: Accessible.StatusBar
        Accessible.name: sentence.text

        Spinner {
            visible: root.compact && root.waiting
            Layout.alignment: Qt.AlignTop
        }
        QQC2.Label {
            id: sentence
            text: (root.compact ? root.view.status : root.view.text) ?? ""
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            horizontalAlignment: root.compact ? Text.AlignLeft : Text.AlignHCenter
            color: root.quiet
            font: root.compact ? Kirigami.Theme.smallFont : Kirigami.Theme.defaultFont
            Layout.fillWidth: true
        }
    }

    Spinner {
        visible: !root.compact && root.waiting
        side: Kirigami.Units.iconSizes.medium
        Layout.alignment: Qt.AlignHCenter
    }

    // The download with its bytes; the read has no measure. A count that steps back (the
    // download went on from the file on disk after a restart) is just a shorter bar.
    RowLayout {
        visible: root.downloading || root.reading
        spacing: Kirigami.Units.largeSpacing
        Layout.fillWidth: true
        Layout.maximumWidth: root.compact ? -1 : Kirigami.Units.gridUnit * 20
        Layout.alignment: Qt.AlignHCenter

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
            visible: root.compact && root.detail !== ""
            text: root.detail
            textFormat: Text.PlainText
            color: root.quiet
            font: Kirigami.Theme.smallFont
        }
    }

    QQC2.Label {
        visible: !root.compact && root.detail !== ""
        text: root.detail
        textFormat: Text.PlainText
        horizontalAlignment: Text.AlignHCenter
        color: root.quiet
        Layout.fillWidth: true
    }

    QQC2.Button {
        id: skip
        visible: root.skipLabel !== ""
        text: root.skipLabel
        // (An icon that points the way the text runs.)
        icon.name: root.LayoutMirroring.enabled ? "go-previous-skip" : "go-next-skip"
        Accessible.name: skip.text
        Layout.alignment: root.compact ? Qt.AlignRight : Qt.AlignHCenter
        Layout.topMargin: root.compact ? 0 : Kirigami.Units.largeSpacing
        onClicked: App.skipTransfer(root.view.account ?? 0)
    }
}
