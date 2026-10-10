pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import app.kourier

// ONE instance; App.timeline switches conversations (§8.3).
Kirigami.Page {
    id: page

    readonly property bool open: App.timeline.conversationId !== 0
    // The conversation has a disappearing-message timer, or offers one to set.
    readonly property bool hasTimer: App.timeline.timerSecs > 0
    readonly property bool timerOffered: App.timeline.timerOptions.length > 0
    // The conversation takes timers besides the ones its menu lists.
    readonly property bool timerCustom: App.timeline.timerCustomMax > 0

    // The disappearing-message menu, below the header at the page's trailing edge.
    function openTimerMenu(): void {
        timerMenu.popup(page, Math.max(0, page.width - timerMenu.width - Kirigami.Units.largeSpacing), Kirigami.Units.smallSpacing);
    }

    // The dialogs of the chat's own actions, created when asked for (tests read the last one).
    property TimerDialog timerDialog: null
    property ChatDeleteDialog deleteDialog: null

    // A custom disappearing-message time, within the range the conversation states.
    function openCustomTimer(): void {
        if (!page.timerCustom) {
            return;
        }
        page.timerDialog = timerComponent.createObject(page, {
            "min": App.timeline.timerCustomMin,
            "max": App.timeline.timerCustomMax,
            "step": App.timeline.timerCustomStep,
            "current": App.timeline.timerSecs
        }) as TimerDialog;
        page.timerDialog?.open();
    }

    // Asks before the chat's messages are deleted for this account.
    function askClear(): void {
        page.deleteDialog = chatDeleteComponent.createObject(page, {
            "remove": false
        }) as ChatDeleteDialog;
        page.deleteDialog?.open();
    }

    // Asks before the chat is deleted for this account.
    function askDelete(): void {
        page.deleteDialog = chatDeleteComponent.createObject(page, {
            "remove": true
        }) as ChatDeleteDialog;
        page.deleteDialog?.open();
    }

    Component {
        id: timerComponent
        TimerDialog {
            onClosed: destroy()
        }
    }
    Component {
        id: chatDeleteComponent
        ChatDeleteDialog {
            onClosed: destroy()
        }
    }

    title: App.timeline.title
    padding: 0
    Kirigami.ColumnView.fillWidth: true
    Kirigami.Theme.colorSet: Kirigami.Theme.View
    Kirigami.Theme.inherit: false

    titleDelegate: RowLayout {
        spacing: Kirigami.Units.largeSpacing
        visible: page.open
        Layout.fillWidth: true

        Avatar {
            name: App.timeline.title
            source: App.timeline.avatarUrl
            selfChat: App.timeline.isSelf
            implicitWidth: Kirigami.Units.gridUnit * 1.8
            implicitHeight: Kirigami.Units.gridUnit * 1.8
        }
        ColumnLayout {
            spacing: 0
            Layout.fillWidth: true
            QQC2.Label {
                text: App.timeline.title
                font: Style.styled(Kirigami.Theme.defaultFont, {
                    "weight": Font.DemiBold
                })
                elide: Text.ElideRight
                textFormat: Text.PlainText
                Layout.fillWidth: true
            }
            RowLayout {
                spacing: Kirigami.Units.smallSpacing
                Layout.fillWidth: true

                QQC2.Label {
                    text: App.timeline.subtitle
                    elide: Text.ElideRight
                    font: Kirigami.Theme.smallFont
                    color: App.timeline.typing ? Style.readable(Kirigami.Theme.highlightColor, Kirigami.Theme.backgroundColor) : Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    textFormat: Text.PlainText
                    // As wide as its text when there is room; the marks follow it.
                    Layout.maximumWidth: Math.ceil(implicitWidth)
                    Layout.fillWidth: true
                    Layout.horizontalStretchFactor: 1000
                }
                Kirigami.Icon {
                    visible: App.timeline.muted
                    source: "notifications-disabled"
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: timerLabel.color
                    isMask: true
                    Accessible.name: L10n.i18nc("@info accessible", "Muted")
                }
                // The timer mark: messages here disappear after this long.
                Kirigami.Icon {
                    visible: page.hasTimer
                    source: "chronometer"
                    implicitWidth: Kirigami.Units.iconSizes.small
                    implicitHeight: Kirigami.Units.iconSizes.small
                    color: timerLabel.color
                    isMask: true
                }
                QQC2.Label {
                    id: timerLabel
                    visible: page.hasTimer
                    text: App.timeline.timerLabel
                    font: Kirigami.Theme.smallFont
                    color: Style.secondary(Kirigami.Theme.textColor, Kirigami.Theme.backgroundColor)
                    textFormat: Text.PlainText
                    Accessible.name: L10n.i18nc("@info accessible", "Messages disappear after %1", App.timeline.timerLabel)
                }
                Item {
                    Layout.fillWidth: true
                    Layout.horizontalStretchFactor: 1
                }
            }
        }
    }

    actions: [
        Kirigami.Action {
            icon.name: "chronometer"
            text: L10n.i18nc("@action:button", "Disappearing Messages")
            displayHint: Kirigami.DisplayHint.IconOnly
            visible: page.open && page.timerOffered
            onTriggered: page.openTimerMenu()
        },
        // What is done to the chat as a whole lives in the page's menu; both ask first.
        Kirigami.Action {
            icon.name: "edit-clear-history"
            text: L10n.i18nc("@action:inmenu", "Clear Chat History…")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open
            onTriggered: page.askClear()
        },
        Kirigami.Action {
            icon.name: "edit-delete"
            text: L10n.i18nc("@action:inmenu", "Delete Chat…")
            displayHint: Kirigami.DisplayHint.AlwaysHide
            visible: page.open
            onTriggered: page.askDelete()
        }
    ]

    // The timers this conversation offers; the current one is checked.
    QQC2.Menu {
        id: timerMenu

        // In the window's scene (also on Wayland, and in headless screenshots).
        popupType: QQC2.Popup.Item
        title: L10n.i18nc("@title:menu", "Disappearing Messages")

        QQC2.MenuItem {
            enabled: false
            text: L10n.i18nc("@title:menu", "Messages Disappear After")
        }
        Instantiator {
            model: App.timeline.timerOptions
            delegate: QQC2.MenuItem {
                id: option

                required property var modelData

                text: option.modelData.label
                checkable: true
                autoExclusive: true
                checked: option.modelData.secs === App.timeline.timerSecs
                onTriggered: {
                    if (option.modelData.secs !== App.timeline.timerSecs) {
                        App.setTimer(option.modelData.secs);
                    }
                    // The mark follows what the network confirms, not the click.
                    option.checked = Qt.binding(() => option.modelData.secs === App.timeline.timerSecs);
                }
            }
            onObjectAdded: (index, object) => timerMenu.insertItem(index + 1, object as QQC2.MenuItem)
            onObjectRemoved: (index, object) => timerMenu.removeItem(object as QQC2.MenuItem)
        }
        // Any other time the network takes (the entries above are the ones its apps name).
        QQC2.MenuSeparator {
            visible: page.timerCustom
            height: visible ? implicitHeight : 0
        }
        QQC2.MenuItem {
            objectName: "timerCustom"
            visible: page.timerCustom
            height: visible ? implicitHeight : 0
            icon.name: "chronometer"
            text: L10n.i18nc("@action:inmenu", "Custom Time…")
            onTriggered: page.openCustomTimer()
        }
    }

    background: Rectangle {
        color: Kirigami.Theme.backgroundColor
    }

    // `--page=history`: keep the view at the oldest message while older pages load.
    property alias pinToOldest: timelineView.pinToOldest
    // The timeline's message popups (the fixture pages open them).
    property alias timeline: timelineView
    // The composer (tests drive it).
    readonly property alias composer: composer

    TimelineView {
        id: timelineView
        anchors.fill: parent
        visible: page.open
        onComposerRequested: composer.focusInput()
    }

    ChatEmptyState {
        anchors.centerIn: parent
        width: parent.width - Kirigami.Units.gridUnit * 4
        visible: !page.open
    }

    // Files dragged in (Dolphin, the desktop) are attached to the message being written.
    DropArea {
        id: drop
        anchors.fill: parent
        enabled: page.open && composer.canAttach && !composer.blocked && !composer.editing
        keys: ["text/uri-list"]
        onDropped: event => {
            composer.attach(event.urls);
            event.accept(Qt.CopyAction);
        }

        Rectangle {
            anchors.fill: parent
            anchors.margins: Kirigami.Units.largeSpacing
            visible: drop.containsDrag
            radius: Kirigami.Units.cornerRadius * 2
            color: Qt.alpha(Kirigami.Theme.highlightColor, 0.12)
            border.width: 2
            border.color: Kirigami.Theme.highlightColor
            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                icon.name: "mail-attachment"
                text: L10n.i18nc("@info:placeholder", "Drop files to attach them")
            }
        }
    }

    footer: Composer {
        id: composer
        visible: page.open
    }
}
