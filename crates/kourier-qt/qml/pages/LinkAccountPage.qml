pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import org.kde.prison as Prison
import app.kourier

// Generic login-step renderer (§8.3): input fields, QR code (rotating), pairing code, emoji,
// progress, error with retry.
//
// After the code was scanned the phone may transfer the account's message history. The page
// does not wait for it: it says "Linked" as soon as the core has the account, and the
// transfer goes on in the account banner of the main window.
Kirigami.ScrollablePage {
    id: page

    readonly property string kind: App.login.kind
    // Secondary text on the page.
    readonly property color quiet: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)

    // The user left the page (Cancel or back): the login is abandoned.
    signal cancelled

    title: App.login.title !== "" ? App.login.title : L10n.i18nc("@title", "Link Account")
    Kirigami.ColumnView.fillWidth: true

    actions: [
        Kirigami.Action {
            text: L10n.i18nc("@action:button", "Cancel")
            icon.name: "dialog-cancel"
            visible: page.kind !== "done"
            enabled: visible
            onTriggered: {
                App.login.cancel();
                page.cancelled();
            }
        }
    ]

    onBackRequested: event => {
        App.login.cancel();
        event.accepted = true;
        page.cancelled();
    }

    ColumnLayout {
        spacing: Kirigami.Units.largeSpacing

        Item {
            implicitHeight: Kirigami.Units.gridUnit
        }

        // Input step (device name, phone number): one field per input, then Continue.
        ColumnLayout {
            visible: page.kind === "input"
            spacing: 0
            Layout.fillWidth: true

            Kirigami.Icon {
                source: App.login.network === "Signal" ? Style.icon("signal") : Style.appIcon
                implicitWidth: Kirigami.Units.gridUnit * 4
                implicitHeight: Kirigami.Units.gridUnit * 4
                Layout.alignment: Qt.AlignHCenter
                Layout.bottomMargin: Kirigami.Units.largeSpacing
            }

            QQC2.Label {
                text: L10n.i18nc("@info", "Your phone shows this name in its list of linked devices.")
                wrapMode: Text.Wrap
                horizontalAlignment: Text.AlignHCenter
                color: page.quiet
                Layout.fillWidth: true
                Layout.maximumWidth: Kirigami.Units.gridUnit * 24
                Layout.alignment: Qt.AlignHCenter
            }

            FormCard.FormCard {
                Layout.topMargin: Kirigami.Units.largeSpacing
                Repeater {
                    model: page.kind === "input" ? App.login.fields : []
                    // A plain field: FormTextFieldDelegate needs the KI18n global context,
                    // which Kourier does not install (architecture §13, phase B).
                    delegate: FormCard.AbstractFormDelegate {
                        id: field
                        required property var modelData
                        background: null
                        Layout.fillWidth: true
                        contentItem: ColumnLayout {
                            spacing: Kirigami.Units.smallSpacing
                            QQC2.Label {
                                text: field.modelData.label
                                textFormat: Text.PlainText
                                elide: Text.ElideRight
                                Layout.fillWidth: true
                            }
                            QQC2.TextField {
                                text: field.modelData.value
                                echoMode: field.modelData.kind === "secret" ? TextInput.Password : TextInput.Normal
                                Layout.fillWidth: true
                                onTextChanged: App.login.setField(field.modelData.id, text)
                                onAccepted: continueButton.clicked()
                            }
                        }
                    }
                }
                FormCard.FormDelegateSeparator {}
                FormCard.FormButtonDelegate {
                    id: continueButton
                    // The delegate draws its own trailing arrow; no leading icon.
                    text: L10n.i18nc("@action:button", "Continue")
                    onClicked: App.login.submit()
                }
            }
        }

        // QR code on a white card so phones scan it in dark mode too. The black and white here
        // are on purpose and the only literal colors of the pages: a scanner needs dark on
        // light whatever the scheme is.
        Rectangle {
            visible: page.kind === "qr"
            color: "white"
            radius: Kirigami.Units.cornerRadius * 3
            implicitWidth: Kirigami.Units.gridUnit * 15
            implicitHeight: implicitWidth
            border.width: 1
            border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
            Layout.alignment: Qt.AlignHCenter

            Prison.Barcode {
                anchors.fill: parent
                anchors.margins: Kirigami.Units.gridUnit
                barcodeType: Prison.Barcode.QRCode
                content: App.login.qr
                foregroundColor: "black"
                backgroundColor: "white"
            }
            Rectangle {
                anchors.centerIn: parent
                width: Kirigami.Units.gridUnit * 2.6
                height: width
                radius: Kirigami.Units.cornerRadius * 2
                color: "white"
                Image {
                    anchors.fill: parent
                    anchors.margins: Kirigami.Units.smallSpacing
                    source: Style.icon(App.login.network === "Signal" ? "signal" : "kourier")
                    sourceSize.width: width * 2
                    sourceSize.height: height * 2
                }
            }
        }

        QQC2.Label {
            visible: page.kind === "code"
            text: App.login.code
            textFormat: Text.PlainText
            // The theme's fixed-width font, 2.4 times the size of the default font, with a gap
            // between the characters in proportion to the text.
            font: Style.styled(Kirigami.Theme.fixedWidthFont, {
                "scale": 2.4 * Kirigami.Theme.defaultFont.pointSize / Kirigami.Theme.fixedWidthFont.pointSize,
                "letterSpacing": Kirigami.Theme.defaultFont.pointSize * 0.4
            })
            Layout.alignment: Qt.AlignHCenter
        }

        Kirigami.Heading {
            visible: page.kind !== "input"
            text: App.login.title
            textFormat: Text.PlainText
            level: 2
            horizontalAlignment: Text.AlignHCenter
            Layout.fillWidth: true
        }

        // Steps for Signal's QR flow (other networks show their hint below).
        ColumnLayout {
            visible: page.kind === "qr" && App.login.network === "Signal"
            spacing: Kirigami.Units.smallSpacing
            Layout.alignment: Qt.AlignHCenter
            Layout.maximumWidth: Kirigami.Units.gridUnit * 24

            Repeater {
                model: [L10n.i18nc("@info linking step, %1 is the network", "Open %1 on your phone", App.login.network), L10n.i18nc("@info linking step", "Go to Settings → Linked devices"), L10n.i18nc("@info linking step", "Tap “Link new device” and scan this code")]
                delegate: RowLayout {
                    id: step
                    required property string modelData
                    required property int index
                    spacing: Kirigami.Units.largeSpacing
                    Rectangle {
                        // A disc around the number that grows with the font.
                        implicitWidth: Math.max(Kirigami.Units.gridUnit * 1.3, Math.ceil(stepNumber.implicitHeight) + Kirigami.Units.smallSpacing)
                        implicitHeight: implicitWidth
                        radius: width / 2
                        color: Kirigami.Theme.highlightColor
                        QQC2.Label {
                            id: stepNumber
                            anchors.centerIn: parent
                            text: step.index + 1
                            color: Kirigami.Theme.highlightedTextColor
                            font: Kirigami.Theme.smallFont
                        }
                    }
                    QQC2.Label {
                        text: step.modelData
                        wrapMode: Text.Wrap
                        Layout.fillWidth: true
                    }
                }
            }
        }

        QQC2.Label {
            visible: (page.kind !== "qr" || App.login.network !== "Signal") && page.kind !== "input" && App.login.hint !== ""
            text: App.login.hint
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            horizontalAlignment: Text.AlignHCenter
            color: page.quiet
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 24
            Layout.alignment: Qt.AlignHCenter
        }

        QQC2.Label {
            visible: page.kind === "qr"
            text: L10n.i18nc("@info", "The code changes every few seconds; scan the one shown.")
            wrapMode: Text.Wrap
            horizontalAlignment: Text.AlignHCenter
            color: page.quiet
            font: Kirigami.Theme.smallFont
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 24
            Layout.alignment: Qt.AlignHCenter
        }

        RowLayout {
            visible: page.kind === "qr" || page.kind === "waiting" || page.kind === "progress"
            spacing: Kirigami.Units.smallSpacing
            Layout.alignment: Qt.AlignHCenter
            Layout.topMargin: Kirigami.Units.largeSpacing
            Spinner {
                implicitWidth: Kirigami.Units.iconSizes.medium
                implicitHeight: Kirigami.Units.iconSizes.medium
            }
            QQC2.Label {
                text: page.kind === "progress" ? L10n.i18nc("@info", "Linking…") : L10n.i18nc("@info", "Waiting for your phone…")
                color: page.quiet
            }
        }

        QQC2.ProgressBar {
            visible: page.kind === "progress" && App.login.progress >= 0
            from: 0
            to: 1
            value: Math.max(0, App.login.progress)
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 20
            Layout.alignment: Qt.AlignHCenter
        }

        Kirigami.InlineMessage {
            visible: page.kind === "failed"
            type: Kirigami.MessageType.Error
            // The backend's words for the failure: never markup.
            text: Style.plain(App.login.error)
            Layout.fillWidth: true
            Layout.maximumWidth: Kirigami.Units.gridUnit * 24
            Layout.alignment: Qt.AlignHCenter
            actions: [
                Kirigami.Action {
                    text: L10n.i18nc("@action:button", "Try Again")
                    icon.name: "view-refresh"
                    visible: App.login.retry
                    enabled: visible
                    onTriggered: App.login.start(App.login.network)
                },
                // A link that would only fail again (the account is here already) offers
                // the way to the list instead, where the account and its banner are.
                Kirigami.Action {
                    text: App.login.leave
                    visible: App.login.leave !== ""
                    enabled: visible
                    onTriggered: page.cancelled()
                }
            ]
        }

        Kirigami.PlaceholderMessage {
            visible: page.kind === "done"
            icon.name: "checkmark"
            text: L10n.i18nc("@info", "Linked")
            explanation: L10n.i18nc("@info", "Your chats are syncing.")
            Layout.fillWidth: true
        }
    }
}
