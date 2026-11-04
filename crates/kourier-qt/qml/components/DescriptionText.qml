pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQml
import org.kde.kirigami as Kirigami
import app.kourier

// A group's description or a person's about text on the details panel (F1): centred, three
// lines until the page's "More" opens the rest, and SELECTABLE: drag, double-click, Shift+arrows,
// Ctrl+C, and a menu (right click, the Menu key, Shift+F10) with Copy, Select All and, where the
// text has links, Open Link… and Copy Link Address. A click on a link asks "Open Link?" first
// (`TextBlock`'s one dialog); the text is one Tab stop, and the menu's "Open Link…" entries are the
// keyboard's way to the links.
//
// The text is the viewmodel's: `html` is whitelisted HTML made by `richtext` from the plain text
// (escaped; a link around an address only in an accepted chat), `text` the same as plain text, and
// `hrefs` the addresses of its links. Nothing here builds a link: the text is shown as a message's
// text is, by the timeline's `TextBlock`. Kirigami's `SelectableLabel` is not used on purpose: its
// `AutoText` format and its tooltip of the hovered link break the rule that backend text has a
// fixed format.
//
// The page owns "More"/"Less" (a button with the page's own words) and flips `expanded`.
Item {
    id: root

    // The description as `richtext` made it, the same as plain text, and the addresses of its links.
    property string html
    property string text
    property var hrefs: []
    // What a screen reader calls the text ("Group description").
    property string accessibleName
    // Every line is shown (the page's "Less" turns it back).
    property bool expanded: false

    // The TextEdit (tests read the selection from it) and the menu.
    readonly property alias edit: block.edit
    readonly property alias textBlock: block
    readonly property alias menu: menu
    readonly property int collapsedLines: 3
    // The text lies over more than the collapsed lines.
    readonly property bool truncated: root.fullHeight > root.collapsedHeight + 1
    // The height of all the lines, and of the first `collapsedLines` of them. The lines can differ in
    // height (an emoji in a fallback font), so the collapsed height is the bottom of the third line, not
    // three times a font height: the cut never lies through a line.
    readonly property real fullHeight: Math.ceil(block.edit.contentHeight)
    readonly property real collapsedHeight: {
        const e = block.edit;
        // (Read so that a new text or width lays the lines out again.)
        if (e.contentHeight <= 0 || e.width <= 0 || e.length < 0) {
            return 0;
        }
        let bottom = 0;
        for (let i = 0; i < root.collapsedLines; ++i) {
            // A point in the MIDDLE of the line: x = 0 is the logical END of a wrapped right-to-left line, and the end of a
            // wrapped line is the start of the next one, so the loop skipped every other line there (L13).
            const at = e.positionAt(e.width / 2, bottom + 1);
            let rect = e.positionToRectangle(at);
            if (rect.y > bottom + 1 && at > 0) {
                // (Past the end of a short wrapped line: its last character is the line.)
                rect = e.positionToRectangle(at - 1);
            }
            const next = rect.y + rect.height;
            if (!(next > bottom)) {
                break;
            }
            bottom = next;
        }
        return Math.ceil(bottom);
    }

    // Forgotten whenever the panel is opened or closed (the same item serves every later page).
    function reset(): void {
        root.expanded = false;
        block.edit.deselect();
    }

    // Opens the menu for the pointer at (`x`, `y`) of the text, or, by the keyboard, at the caret. A link
    // under the pointer (or under the caret) is the menu's link; by the keyboard, with the caret on none,
    // a text with one link offers that one, a text with several offers each of them.
    function openMenu(x: real, y: real, byKey: bool): void {
        const e = block.edit;
        let link = e.linkAt(x, y);
        if (byKey && link === "" && root.hrefs.length === 1) {
            link = root.hrefs[0];
        }
        menu.link = link;
        menu.listed = byKey && link === "" ? root.hrefs : [];
        menu.hadFocus = e.activeFocus;
        menu.popup(e, x, y);
    }

    function openMenuAtCaret(): void {
        const r = block.edit.cursorRectangle;
        root.openMenu(r.x + 1, r.y + r.height / 2, true);
    }

    visible: root.text !== ""
    implicitHeight: view.height
    height: root.implicitHeight

    // The Menu key and Shift+F10 (what the TextEdit does not take comes up to here).
    Keys.onPressed: event => {
        if (event.key === Qt.Key_Menu || (event.key === Qt.Key_F10 && (event.modifiers & Qt.ShiftModifier))) {
            // (An accepted repeat of a held key opens nothing again.)
            event.accepted = true;
            if (!event.isAutoRepeat) {
                root.openMenuAtCaret();
            }
        }
    }

    Item {
        id: view

        width: root.width
        height: root.expanded || !root.truncated ? root.fullHeight : root.collapsedHeight
        clip: true

        TextBlock {
            id: block

            width: view.width
            block: ({
                    "html": root.html,
                    "plain": root.text,
                    "jumbo": false,
                    "more": false,
                    "moreWords": 0,
                    "spoilers": [],
                    "mentions": []
                })
            ink: Kirigami.Theme.textColor
            surface: Kirigami.Theme.backgroundColor
            keepSelection: menu.visible
            accessibleName: root.accessibleName
        }

        // Right click: the menu. (The left button is the TextEdit's own: selecting, and the link.)
        TapHandler {
            acceptedButtons: Qt.RightButton
            onPressedChanged: {
                if (pressed) {
                    root.openMenu(point.position.x, point.position.y, false);
                }
            }
        }
    }

    // The keyboard is on the text.
    Rectangle {
        anchors.fill: view
        anchors.margins: -Kirigami.Units.smallSpacing / 2
        visible: block.edit.activeFocus
        color: "transparent"
        radius: Kirigami.Units.cornerRadius
        border.width: 1
        border.color: Kirigami.Theme.focusColor
    }

    Connections {
        target: block

        // The link question is answered: the keyboard returns to the text that had it before the menu.
        function onQuestionChanged(): void {
            if (block.question === null && menu.hadFocus) {
                menu.hadFocus = false;
                block.edit.forceActiveFocus(Qt.PopupFocusReason);
            }
        }
    }

    Binding {
        target: block.edit
        property: "objectName"
        value: root.objectName + "Text"
    }

    // What makes the page's text a page's text (a message's text has none of it): a read-only TextEdit selects by
    // the keyboard (Shift+arrows, Ctrl+C) only when told to, the text is ONE Tab stop, and it is centred. These
    // Bindings never restore: setting `activeFocusOnTab` false on a text that has the keyboard is refused with a warning.
    Binding {
        target: block.edit
        property: "selectByKeyboard"
        value: true
        restoreMode: Binding.RestoreNone
    }
    Binding {
        target: block.edit
        property: "activeFocusOnTab"
        value: true
        restoreMode: Binding.RestoreNone
    }
    Binding {
        target: block.edit
        property: "horizontalAlignment"
        value: TextEdit.AlignHCenter
        restoreMode: Binding.RestoreNone
    }

    QQC2.Menu {
        id: menu

        // The address under the pointer or the caret ("" = none), the addresses a keyboard menu offers one
        // by one, and whether the text had the keyboard before the menu took it.
        property string link: ""
        property var listed: []
        property bool hadFocus: false
        // How much of an address fits in an entry; the whole address is the entry's accessible name.
        readonly property real linkTextWidth: Kirigami.Units.gridUnit * 20
        readonly property bool linked: menu.link !== ""

        // In the window's scene (also on Wayland, and in headless screenshots).
        popupType: QQC2.Popup.Item

        // The keyboard goes back to the text when the menu closes, unless the entry chosen opened the link
        // question (a modal dialog that must keep the keyboard; the text gets it when the question is gone).
        onClosed: {
            if (menu.hadFocus && block.question === null) {
                menu.hadFocus = false;
                block.edit.forceActiveFocus(Qt.PopupFocusReason);
            }
        }

        QQC2.MenuItem {
            objectName: "textCopy"
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:inmenu", "Copy")
            // Only with something selected.
            enabled: block.edit.selectedText.length > 0
            onTriggered: block.edit.copy()
        }
        QQC2.MenuItem {
            objectName: "textSelectAll"
            icon.name: "edit-select-all"
            text: L10n.i18nc("@action:inmenu", "Select All")
            // (A right press gives the text no keyboard, so `hadFocus` is false: what he selected is what he will copy,
            // so the text gets the keyboard when the menu closes.)
            onTriggered: {
                block.edit.selectAll();
                menu.hadFocus = true;
            }
        }
        QQC2.MenuSeparator {
            visible: menu.linked || menu.listed.length > 0
            height: visible ? implicitHeight : 0
        }
        QQC2.MenuItem {
            objectName: "textOpenLink"
            visible: menu.linked
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "document-open"
            text: L10n.i18nc("@action:inmenu", "Open Link…")
            // (The one question of the timeline: nothing opens without its answer.)
            onTriggered: block.openLink(menu.link)
        }
        QQC2.MenuItem {
            objectName: "textCopyLink"
            visible: menu.linked
            enabled: visible
            height: visible ? implicitHeight : 0
            icon.name: "edit-copy"
            text: L10n.i18nc("@action:inmenu", "Copy Link Address")
            onTriggered: App.copyText(menu.link)
        }

        // The keyboard's menu of a text with several links: one entry for each, after the others.
        Instantiator {
            model: menu.listed
            delegate: QQC2.MenuItem {
                id: entry

                required property string modelData

                objectName: "textOpenListed"
                icon.name: "document-open"
                // (An address may hold "&", which an entry reads as a mnemonic: "&&" is the literal one.)
                text: L10n.i18nc("@action:inmenu, %1 a web address", "Open %1…", linkMetrics.elidedText.replace(/&/g, "&&"))
                Accessible.name: L10n.i18nc("@action:inmenu, %1 a web address", "Open %1…", Style.plain(entry.modelData))
                onTriggered: block.openLink(entry.modelData)

                TextMetrics {
                    id: linkMetrics
                    font: entry.font
                    elide: Text.ElideRight
                    elideWidth: menu.linkTextWidth
                    text: Style.plain(entry.modelData)
                }
            }
            onObjectAdded: (index, object) => menu.addItem(object)
            onObjectRemoved: (index, object) => menu.removeItem(object)
        }
    }
}
