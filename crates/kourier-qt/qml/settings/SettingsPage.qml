pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
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

    // Every account with what it offers here (`App.accountActions`).
    readonly property var accountActions: App.accountActions

    // The experimental networks this build holds (WhatsApp), on or off at this start.
    readonly property var experimentalNetworks: App.networks.filter(n => n.experimental)
    // The ones with a switch, and the ones this build cannot log in to (Telegram without its API
    // key or library): those are listed with their reason and no switch.
    readonly property var switchNetworks: page.experimentalNetworks.filter(n => !n.missing)
    readonly property var missingNetworks: page.experimentalNetworks.filter(n => n.missing)
    // The switches show the PREFERENCE (what the next start does); `n.on` is what this start
    // did. A difference between the two is what "Restart Now" is for.
    function networkPref(kind: string): bool {
        return Style.networkWanted(kind);
    }
    readonly property bool restartNeeded: {
        // (A network this build cannot log in to has no switch and is never on: a restart would
        // not change that.)
        for (let i = 0; i < page.switchNetworks.length; ++i) {
            const n = page.switchNetworks[i];
            if (page.networkPref(n.kind) !== n.on) {
                return true;
            }
        }
        return false;
    }
    // Whether a link of `kind` is being made now (the switch waits for it).
    function linking(kind: string): bool {
        return App.login.network === kind && ["input", "qr", "code", "waiting", "progress"].indexOf(App.login.kind) >= 0;
    }
    // The question "Turn Off WhatsApp?" while it is open (tests read it).
    property Kirigami.PromptDialog turnOffDialog: null

    // The account whose "Sync chat changes with my phone" row has the keyboard (0 = none). A caps
    // echo remakes EVERY account card (the comment at the accounts' Repeater), so the row that is
    // clicked or toggled by key says who it was before it asks, and the new row of that account
    // takes the keyboard when it is made, then clears this. A refusal clears it too: nothing
    // changes, so no card is remade for it to be taken by (A10d D5, critic L4).
    property real roamingFocus: 0

    Connections {
        target: App
        // The switch was refused ("The setting could not be saved." is MainWindow's toast).
        function onChatTold(kind: string, conversation: var, wasPinned: bool, text: string): void {
            if (kind === "switch-refused") {
                page.roamingFocus = 0;
            }
        }
    }

    // Turning a network off with an account of it asks first: the account stays linked and
    // readable, nothing is sent or received until the network is on again.
    function askTurnOff(kind: string, name: string, text: string): void {
        if (page.turnOffDialog !== null) {
            return;
        }
        const component = Qt.createComponent(Qt.resolvedUrl("../dialogs/StopLinkingDialog.qml"));
        if (component.status !== Component.Ready) {
            console.error("the question could not be made: " + component.errorString());
            return;
        }
        const dialog = component.createObject(page, {
            "question": {
                "title": L10n.i18nc("@title:window, %1 the network", "Turn Off %1?", Style.plain(name)),
                "text": Style.plain(text),
                "confirm": L10n.i18nc("@action:button", "Turn Off"),
                "cancel": L10n.i18nc("@action:button", "Cancel")
            },
            "confirmIcon": "network-disconnect",
            "action": () => App.settings.setNetworkOn(kind, false)
        }) as Kirigami.PromptDialog;
        if (dialog === null) {
            return;
        }
        page.turnOffDialog = dialog;
        dialog.closed.connect(() => {
            page.turnOffDialog = null;
        });
        dialog.open();
    }

    // Brings "Accounts" on screen (a fixture page shows it).
    function showAccounts(): void {
        page.flickable.contentY = Math.max(0, Math.min(accountsHeader.y, page.flickable.contentHeight - page.flickable.height));
    }

    // Brings "Disappearing messages" on screen (a fixture page shows it), a little below the top
    // of the view. The old rule (its heading AT the top, clamped to the end) left the page's
    // header blank in the pictures once "Networks" made the page longer: the heading then sat
    // above the clamp, and with the style of `shots` (org.kde.desktop, software rendering) the
    // header's content was not drawn at that scroll position (K-A, chunk 10).
    function showExpiry(): void {
        page.flickable.contentY = Math.max(0, Math.min(expiryHeader.y - page.flickable.height * 0.4, page.flickable.contentHeight - page.flickable.height));
    }

    // Brings "Networks" on screen (a fixture page shows it).
    function showNetworks(): void {
        page.flickable.contentY = Math.max(0, Math.min(networksHeader.y, page.flickable.contentHeight - page.flickable.height));
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
            objectName: "blend-accounts"
            text: L10n.i18nc("@option:check", "Show chats of several accounts together")
            description: L10n.i18nc("@info", "Shift+click or Ctrl+click an account in the side panel to add it to the list.")
            checked: App.settings.blendAccounts
            onToggled: App.settings.blendAccounts = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "quiet-notices"
            text: L10n.i18nc("@option:check", "Show minor notices in the conversation list")
            description: L10n.i18nc("@info", "Safety number changes, refreshed sessions, changed phone numbers and accepted message requests. They always stay in the chat itself.")
            // Nothing to switch before the core has said what holds.
            enabled: App.settings.quietNotices !== ""
            checked: App.settings.quietNotices === "show"
            // Applied at once. The mark follows what the core holds, not the click.
            onToggled: {
                App.settings.quietNotices = checked ? "show" : "hide";
                checked = Qt.binding(() => App.settings.quietNotices === "show");
            }
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "round-avatars"
            text: L10n.i18nc("@option:check", "Round avatars")
            description: L10n.i18nc("@info", "Off: squares with your theme's corners.")
            checked: App.settings.roundAvatars
            onToggled: App.settings.roundAvatars = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "tint-rows"
            text: L10n.i18nc("@option:check", "Tint chats with their account's color")
            description: L10n.i18nc("@info", "Right-click an account in the side panel to choose its color.")
            checked: App.settings.tintRows
            onToggled: App.settings.tintRows = checked
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormSwitchDelegate {
            objectName: "chat-wallpaper"
            text: L10n.i18nc("@option:check", "Drawings behind messages")
            description: L10n.i18nc("@info", "A faint pattern in the color of the chat's account.")
            checked: App.settings.chatWallpaper
            onToggled: App.settings.chatWallpaper = checked
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
        FormCard.FormComboBoxDelegate {
            objectName: "locationFiles"
            text: L10n.i18nc("@label:listbox", "Files with location data")
            description: [L10n.i18nc("@info", "When Kourier finds where a photo, video or file was taken, it asks before sending it unchanged."), L10n.i18nc("@info", "Such files are sent with their location, without a question."), L10n.i18nc("@info", "Such files stay in the list; everything else is sent.")][Math.max(0, Math.min(2, App.settings.locationFiles))]
            model: [L10n.i18nc("@item:inlistbox", "Ask each time"), L10n.i18nc("@item:inlistbox", "Always send"), L10n.i18nc("@item:inlistbox", "Never send")]
            currentIndex: App.settings.locationFiles
            onActivated: index => App.settings.locationFiles = index
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            objectName: "readReceipts"
            text: L10n.i18nc("@label", "Read receipts")
            // A second line for a network that is on and always sends them ("Always sent for
            // Telegram."): the viewmodel's sentence, empty when there is none.
            description: L10n.i18nc("@info synced setting", "On · change on your phone") + (App.privacyNote !== "" ? "\n" + App.privacyNote : "")
        }
        FormCard.FormDelegateSeparator {}
        FormCard.FormTextDelegate {
            objectName: "typingIndicators"
            text: L10n.i18nc("@label", "Typing indicators")
            description: L10n.i18nc("@info synced setting", "On · change on your phone") + (App.privacyNote !== "" ? "\n" + App.privacyNote : "")
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

    // The experimental networks (WhatsApp): the switch is a preference; the core has no runtime
    // switch, so a change takes effect when Kourier starts again ("Restart Now").
    FormCard.FormHeader {
        id: networksHeader
        visible: page.experimentalNetworks.length > 0
        title: L10n.i18nc("@title:group", "Networks")
    }
    Card {
        id: networksCard

        visible: page.experimentalNetworks.length > 0

        Repeater {
            model: page.switchNetworks
            delegate: FormCard.FormSwitchDelegate {
                id: networkSwitch

                required property var modelData

                objectName: "network-" + networkSwitch.modelData.kind
                text: Style.plain(networkSwitch.modelData.name)
                // Off for a reason the person can change: the description says which.
                enabled: !page.linking(networkSwitch.modelData.kind)
                // (Not `networkSwitch.enabled`: it is false while any page above is disabled.)
                description: page.linking(networkSwitch.modelData.kind) ? L10n.i18nc("@info", "Finish or stop the link first.") : L10n.i18nc("@info, %1 the network", "Experimental. Kourier is not made or approved by %1, and %1 may restrict or ban an account it finds linked to Kourier.", Style.plain(networkSwitch.modelData.name))
                checked: page.networkPref(networkSwitch.modelData.kind)
                // The mark follows the preference, not the click: turning off with an account
                // asks first and leaves it on until the question is answered.
                onToggled: {
                    if (!networkSwitch.checked && App.settings.accountsOf(networkSwitch.modelData.kind) > 0) {
                        page.askTurnOff(networkSwitch.modelData.kind, networkSwitch.modelData.name, networkSwitch.modelData.turnOffText);
                    } else {
                        App.settings.setNetworkOn(networkSwitch.modelData.kind, networkSwitch.checked);
                    }
                    networkSwitch.checked = Qt.binding(() => page.networkPref(networkSwitch.modelData.kind));
                }
            }
        }
        // A network this build cannot log in to: its name and why, no switch (turning it on
        // would not help). The viewmodel's words, never the backend's.
        Repeater {
            model: page.missingNetworks
            delegate: ColumnLayout {
                id: missingRow

                required property var modelData
                required property int index

                spacing: 0
                Layout.fillWidth: true

                FormCard.FormDelegateSeparator {
                    visible: missingRow.index > 0 || page.switchNetworks.length > 0
                }
                FormCard.FormTextDelegate {
                    objectName: "network-missing-" + missingRow.modelData.kind
                    text: Style.plain(missingRow.modelData.name)
                    description: missingRow.modelData.description
                    Layout.fillWidth: true
                }
            }
        }
        FormCard.FormDelegateSeparator {
            visible: page.restartNeeded
        }
        FormCard.FormTextDelegate {
            visible: page.restartNeeded
            objectName: "networks-restart-note"
            text: L10n.i18nc("@info", "Takes effect when Kourier starts again.")
        }
        FormCard.FormButtonDelegate {
            visible: page.restartNeeded
            objectName: "networks-restart"
            icon.name: "system-reboot"
            text: L10n.i18nc("@action:button", "Restart Now")
            onClicked: {
                // The window asks first when work still runs (a history transfer).
                const window = Kirigami.ApplicationWindow.window;
                if (window && window.restartAsking) {
                    window.restartAsking(() => App.restartNow());
                } else {
                    App.restartNow();
                }
            }
        }
    }

    // What can be done with each account (link feedback L4): a group per account with its
    // line (name and number), "Transfer History from Phone…" for an account whose network
    // hands a newly linked device its history (hidden while that account still links),
    // "Remove Account…" and, for a linked account, "Unlink…". Every action only asks: the
    // question says what would happen, and "Cancel" is its default.
    FormCard.FormHeader {
        id: accountsHeader
        visible: page.accountActions.length > 0
        title: L10n.i18nc("@title:group", "Accounts")
    }
    // These cards sit straight in FormCardPage's `internalLayout`, a ColumnLayout, which is the
    // shape that crashed the accounts drawer (AccountsDrawer.qml, F1): a Qt Quick Layout keeps
    // a removed child in its engine until it polishes, a Repeater deletes the card before that,
    // and a hidden window never polishes. The model is a JS array, so EVERY change of
    // `App.accountActions` removes and remakes all the cards, also while the window is hidden.
    // It is safe today (Qt 6.11.1, Kirigami Add-ons as installed; checked in F1) only because
    // nothing reads `Layout.minimumHeight`, `Layout.minimumWidth` or another size hint of
    // `internalLayout` or of a layout above it: `internalLayout` is the page's top-level
    // layout (its parent is a plain Item of the ScrollablePage, no layout) and the page reads
    // its `implicitHeight` only, which does not walk the engine's items. It would become
    // unsafe the moment a `Layout.*` attached read reaches `internalLayout`, the page is put
    // inside another layout, or Kirigami starts to read such a size hint. If so, wrap the
    // Repeater in a `Column` (a positioner drops a removed child at once), as the drawer does.
    Repeater {
        model: page.accountActions
        delegate: Card {
            id: accountCard

            required property var modelData
            readonly property real account: accountCard.modelData.account

            objectName: "account-" + accountCard.account

            // Which account: its name and number (the account's own text).
            FormCard.FormTextDelegate {
                text: Style.plain(accountCard.modelData.about)
                textItem.textFormat: Text.PlainText
                icon.name: "im-user"
            }
            // "Sync chat changes with my phone" (A10d D5): whether Archive, Pin, Mute and Mark as
            // Unread made here are also written to the phone and the other devices. The row is
            // there only for an account whose network writes them (`roaming` is "" otherwise:
            // a hidden row has no height and takes no key). The mark follows the echo, not the
            // click; it flips at once, no question (the lead's call M8). While the account cannot
            // flip it, `roamingNote` says why and the row is off.
            FormCard.FormDelegateSeparator {
                visible: accountCard.modelData.roaming !== ""
            }
            FormCard.FormSwitchDelegate {
                id: roamingRow
                visible: accountCard.modelData.roaming !== ""
                objectName: "roaming-" + accountCard.account
                text: L10n.i18nc("@option:check", "Sync chat changes with my phone")
                description: accountCard.modelData.roamingNote !== "" ? accountCard.modelData.roamingNote : L10n.i18nc("@info", "Archive, pin, mute and unread marks made here also change on your phone and your other devices.")
                enabled: accountCard.modelData.roamingNote === ""
                // The clicked value is held (`pendingOn`) until the core's echo, the switch's
                // refusal or 5 s, so the handle does not spring back to the old state while the
                // answer is on its way, which looked like a refusal (L5).
                property var pendingOn: null
                readonly property bool coreOn: accountCard.modelData.roaming === "on"
                onCoreOnChanged: roamingRow.pendingOn = null
                checked: roamingRow.pendingOn !== null ? roamingRow.pendingOn : roamingRow.coreOn
                Timer {
                    id: pendingTimer

                    interval: 5000
                    onTriggered: roamingRow.pendingOn = null
                }
                Connections {
                    target: App
                    function onChatTold(kind: string, conversation: var, wasPinned: bool, text: string): void {
                        if (kind === "switch-refused") {
                            roamingRow.pendingOn = null;
                        }
                    }
                }
                // With several accounts the rows have the same name: the delegate's own
                // description (what the switch does) would be all a screen reader hears, so the
                // account's line goes in front of it (L1: FormSwitchDelegate binds
                // `Accessible.description` to its description alone).
                Accessible.description: accountCard.modelData.about + ". " + roamingRow.description
                // The keyboard goes to the row of the NEW card after the echo (see `roamingFocus`).
                Component.onCompleted: {
                    if (page.roamingFocus === accountCard.account && accountCard.modelData.roaming !== "") {
                        Qt.callLater(roamingRow.takeKeyboard);
                    }
                }
                function takeKeyboard(): void {
                    // (The echo can remake the card again before this runs; then the next row
                    // of the account takes it, and `roamingFocus` is still set.)
                    if (roamingRow === null || page.roamingFocus !== accountCard.account) {
                        return;
                    }
                    // FormCardPage scrolls a newly focused item into view by the place it has NOW
                    // (its `onActiveFocusItemChanged`, run inside the call), and the cards that were
                    // just made are not laid out yet: the row stands at the top and the view would
                    // jump there (seen in `settings_ui` `roaming`, one flip in two). The person
                    // left the view where it is, and the row was in it: it stays.
                    const kept = page.flickable.contentY;
                    roamingRow.forceActiveFocus(Qt.TabFocusReason);
                    page.flickable.contentY = kept;
                    page.roamingFocus = 0;
                }
                // A held Space toggles once, not again with every repeat (qml.md: "A held key
                // and a button"; L5). The repeats of EVERY key are taken here, Tab and Backtab
                // excepted; the first press and the real release reach the switch. The parameter
                // is untyped so a test can hand over a stand-in event.
                function holdKey(event: var): void {
                    if (event.isAutoRepeat && event.key !== Qt.Key_Tab && event.key !== Qt.Key_Backtab) {
                        event.accepted = true;
                    }
                }
                Keys.onPressed: event => roamingRow.holdKey(event)
                Keys.onReleased: event => roamingRow.holdKey(event)
                // Applied at once; the handle stays at the clicked value until the core's echo (the
                // mark then follows what the core holds) or the refusal ("The setting could not
                // be saved."), which puts the old state back.
                onToggled: {
                    page.roamingFocus = accountCard.account;
                    const want = roamingRow.checked;
                    roamingRow.pendingOn = want;
                    pendingTimer.restart();
                    roamingRow.checked = Qt.binding(() => roamingRow.pendingOn !== null ? roamingRow.pendingOn : roamingRow.coreOn);
                    App.setRoamingWrites(accountCard.account, want);
                }
            }
            FormCard.FormDelegateSeparator {
                visible: accountCard.modelData.transfer
            }
            FormCard.FormButtonDelegate {
                id: transferRow
                visible: accountCard.modelData.transfer
                objectName: "transfer-history-" + accountCard.account
                icon.name: "document-import"
                text: accountCard.modelData.transferLabel
                // With several accounts the rows have the same name: a screen reader is
                // told which account this is.
                Accessible.description: accountCard.modelData.about
                onClicked: App.askReplace(accountCard.account)
            }
            FormCard.FormDelegateSeparator {
                visible: accountCard.modelData.unlink
            }
            FormCard.FormButtonDelegate {
                id: unlinkRow
                visible: accountCard.modelData.unlink
                objectName: "unlink-" + accountCard.account
                icon.name: "network-disconnect"
                text: accountCard.modelData.unlinkLabel
                Accessible.description: accountCard.modelData.about
                onClicked: App.askAccountAction(accountCard.account, "unlink")
            }
            FormCard.FormDelegateSeparator {}
            FormCard.FormButtonDelegate {
                id: removeRow
                objectName: "remove-account-" + accountCard.account
                icon.name: "list-remove-user"
                text: accountCard.modelData.removeLabel
                Accessible.description: accountCard.modelData.about
                onClicked: App.askAccountAction(accountCard.account, "remove")
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
