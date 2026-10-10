pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import org.kde.kirigamiaddons.formcard as FormCard
import app.kourier

// Settings (§8.9). Network-synced account settings are shown read-only ("change on your
// phone"); app preferences are editable, and every change holds at once.
//
// "Disappearing messages" is the one choice the core keeps (§9.6): what happens to a message
// whose time is up while nobody has confirmed this computer's clock. Three options, each with
// one sentence that says what it means for the reader: who can still read the message when
// the computer is opened without a network, and what a wrong clock can cost.
FormCard.FormCardPage {
    id: page

    title: L10n.i18nc("@title", "Settings")
    Kirigami.ColumnView.fillWidth: true

    // A group of settings: Kirigami Add-ons' card, and in a high-contrast scheme a solid frame
    // over it (the card's own border is a faint mix of its background and text, and in a dark
    // scheme it is the background itself). The frame follows the card's own width, so that a
    // card that does not fill the page is framed as it is drawn.
    component Card: FormCard.FormCard {
        id: card

        Rectangle {
            // (Not in the card's column: it frames the card, it is no row of it.)
            parent: card
            readonly property real inset: card.cardWidthRestricted ? Math.round((card.width - card.maximumWidth) / 2) : 0

            anchors.fill: parent
            anchors.leftMargin: inset
            anchors.rightMargin: inset
            z: 1
            visible: Style.highContrast
            radius: card.cardWidthRestricted ? Kirigami.Units.cornerRadius : 0
            color: "transparent"
            border.width: 1
            border.color: Style.frame(Kirigami.Theme.backgroundColor, Kirigami.Theme.textColor, Kirigami.Theme.frameContrast)
            Kirigami.Theme.colorSet: Kirigami.Theme.View
            Kirigami.Theme.inherit: false
        }
    }

    // The options of "Disappearing messages", by the core's names for them, in its order.
    readonly property var expiryOptions: [
        {
            "name": "hide",
            "text": L10n.i18nc("@option:radio", "Hide now, erase once the time is confirmed"),
            "description": L10n.i18nc("@info", "Nobody who opens this computer offline can read them, and they come back if its clock turns out to be wrong.")
        },
        {
            "name": "keep",
            "text": L10n.i18nc("@option:radio", "Keep until the time is confirmed"),
            "description": L10n.i18nc("@info", "A wrong clock loses nothing, but they can still be read while this computer is offline.")
        },
        {
            "name": "erase",
            "text": L10n.i18nc("@option:radio", "Erase by this computer's clock"),
            "description": L10n.i18nc("@info", "They are gone at once, also offline; a clock that runs ahead erases them too early, for good.")
        }
    ]
    // What the core says holds now ("" until it has answered).
    readonly property string expiryPolicy: App.settings.expiryPolicy

    // The accounts that offer "Transfer History from Phone…": those of a network that hands
    // a newly linked device the account's history.
    readonly property var historyAccounts: App.historyAccounts

    // Brings "Accounts" on screen (a fixture page shows it).
    function showAccounts(): void {
        page.flickable.contentY = Math.max(0, Math.min(accountsHeader.y, page.flickable.contentHeight - page.flickable.height));
    }

    // Brings "Disappearing messages" on screen (a fixture page shows it).
    function showExpiry(): void {
        page.flickable.contentY = Math.max(0, Math.min(expiryHeader.y, page.flickable.contentHeight - page.flickable.height));
    }

    // Nothing is read for this page before it is shown.
    Component.onCompleted: App.settings.load()

    FormCard.FormHeader {
        title: L10n.i18nc("@title:group", "General")
    }
    Card {
        FormCard.FormSwitchDelegate {
            text: L10n.i18nc("@option:check", "Show in system tray")
            // trayStatus: -1 not known yet, 0 no tray, 1 shown.
            description: !App.settings.tray ? L10n.i18nc("@info", "Closing the window quits Kourier.") : (App.trayStatus === 0 ? L10n.i18nc("@info", "No system tray was found: closing the window quits Kourier.") : L10n.i18nc("@info", "Closing the window keeps Kourier running in the tray."))
            checked: App.settings.tray
            onToggled: App.settings.tray = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            text: L10n.i18nc("@option:check", "Start with the system")
            description: L10n.i18nc("@info", "Kourier starts when you log in.")
            checked: App.settings.autostart
            onToggled: App.settings.autostart = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            text: L10n.i18nc("@option:check", "Start hidden in the tray")
            // Off for a reason the user can change: the description says which.
            description: enabled ? L10n.i18nc("@info", "At login, only the tray icon appears; messages still arrive and notify.") : (!App.settings.tray && !App.settings.autostart ? L10n.i18nc("@info", "Needs “Show in system tray” and “Start with the system”.") : (!App.settings.tray ? L10n.i18nc("@info", "Needs “Show in system tray”.") : L10n.i18nc("@info", "Needs “Start with the system”.")))
            enabled: App.settings.tray && App.settings.autostart
            checked: App.settings.startHidden
            onToggled: App.settings.startHidden = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            text: L10n.i18nc("@option:check", "Compact conversation list")
            checked: App.settings.compact
            onToggled: App.settings.compact = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "round-avatars"
            text: L10n.i18nc("@option:check", "Round avatars")
            description: L10n.i18nc("@info", "Off: squares with your theme's corners.")
            checked: App.settings.roundAvatars
            onToggled: App.settings.roundAvatars = checked
        }
    }

    FormCard.FormHeader {
        title: L10n.i18nc("@title:group", "Notifications")
    }
    Card {
        FormCard.FormComboBoxDelegate {
            text: L10n.i18nc("@label:listbox", "Show in notifications")
            description: [L10n.i18nc("@info", "The sender and the message. You can reply from the notification."), L10n.i18nc("@info", "Only who wrote; the message stays hidden."), L10n.i18nc("@info", "Only that a message arrived: no name, no text.")][Math.max(0, Math.min(2, App.settings.notificationContent))]
            model: [L10n.i18nc("@item:inlistbox", "Name and message"), L10n.i18nc("@item:inlistbox", "Name only"), L10n.i18nc("@item:inlistbox", "Nothing")]
            currentIndex: App.settings.notificationContent
            onActivated: index => App.settings.notificationContent = index
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: L10n.i18nc("@label", "While catching up")
            description: L10n.i18nc("@info", "Messages that arrived while Kourier was offline are summed up in one notification.")
        }
    }

    FormCard.FormHeader {
        title: L10n.i18nc("@title:group", "Privacy")
    }
    Card {
        FormCard.FormTextDelegate {
            text: L10n.i18nc("@label", "Read receipts")
            description: L10n.i18nc("@info synced setting", "On · change on your phone")
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: L10n.i18nc("@label", "Typing indicators")
            description: L10n.i18nc("@info synced setting", "On · change on your phone")
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            text: L10n.i18nc("@label", "Link previews")
            description: L10n.i18nc("@info", "Off in this version")
        }
    }

    FormCard.FormHeader {
        id: expiryHeader
        title: L10n.i18nc("@title:group", "Disappearing Messages")
    }
    Card {
        id: expiryCard

        FormCard.FormTextDelegate {
            text: L10n.i18nc("@label", "While the time cannot be confirmed")
            description: L10n.i18nc("@info", "Kourier checks this computer's clock with the network. Until it can (a start without a connection, or a clock the network calls wrong), messages whose time is up are handled like this:")
        }
        QQC2.ButtonGroup {
            id: expiryGroup
        }
        Repeater {
            model: page.expiryOptions
            delegate: FormCard.FormRadioDelegate {
                id: option

                required property var modelData

                objectName: "expiry-" + option.modelData.name
                text: option.modelData.text
                description: option.modelData.description
                QQC2.ButtonGroup.group: expiryGroup
                // Nothing to choose from before the core has said what holds.
                enabled: page.expiryPolicy !== ""
                checked: page.expiryPolicy === option.modelData.name
                Accessible.description: option.modelData.description
                // Applied at once. The mark follows what the core holds, not the click.
                onClicked: {
                    App.settings.expiryPolicy = option.modelData.name;
                    option.checked = Qt.binding(() => page.expiryPolicy === option.modelData.name);
                }
            }
        }
    }

    // What can be done with an account. Today one action, for accounts whose network can
    // send a newly linked device their history; the group is not there without one.
    FormCard.FormHeader {
        id: accountsHeader
        visible: page.historyAccounts.length > 0
        title: L10n.i18nc("@title:group", "Accounts")
    }
    Card {
        visible: page.historyAccounts.length > 0

        Repeater {
            model: page.historyAccounts
            delegate: FormCard.FormButtonDelegate {
                id: transferRow

                required property var modelData

                objectName: "transfer-history-" + transferRow.modelData.account
                icon.name: "document-import"
                text: transferRow.modelData.label
                // Which account (`about`: its name and number, the account's own text). With
                // several accounts the rows have the same name: a screen reader is told
                // which one this is.
                description: Style.plain(transferRow.modelData.about)
                Accessible.description: transferRow.modelData.about
                // It only asks: the question says what would happen, and "Cancel" is its
                // default.
                onClicked: App.askReplace(transferRow.modelData.account)
            }
        }
    }

    FormCard.FormHeader {
        title: L10n.i18nc("@title:group", "About")
    }
    // The About page is Kirigami Add-ons' own; it shows the application's `KAboutData`,
    // which the front-end fills from `kourier_app::about` at its start.
    Card {
        FormCard.FormButtonDelegate {
            objectName: "about-app"
            // (The delegate draws `icon.name` only; its icon takes a URL as well as a name.)
            icon.name: Style.appIcon
            text: L10n.i18nc("@action:button", "About Kourier")
            description: L10n.i18nc("@info the application's version", "Version %1", App.version)
            // (An object with a visual parent, not the Component: the page stack would create
            // it outside the scene and Qt warns.)
            onClicked: page.Kirigami.PageStack.push(aboutPage.createObject(page))
        }
    }

    Component {
        id: aboutPage
        FormCard.AboutPage {}
    }
}
