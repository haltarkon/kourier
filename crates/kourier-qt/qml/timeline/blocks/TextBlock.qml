pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// Selectable formatted text: a read-only TextEdit fed with whitelisted RichText from Rust
// (§8.4), spoilers drawn as opaque rectangles over their ranges until clicked.
//
// The TextEdit always wraps at the bubble's maximum width; the block reports the width the
// text really uses (`contentWidth`) as its implicit width, so the bubble shrinks to fit
// without a width ↔ implicitWidth binding loop.
//
// A long text arrives cut (`block.more`, from Rust: the text and its ranges are cut before
// the markup is made); "Read more" under it asks for the next step, and the row changes in
// place. Selecting and dragging work on the part that is shown.
Item {
    id: root

    required property var block
    required property color ink
    // What the text stands on (the bubble, or the page for big emoji).
    required property color surface
    // The message the text belongs to ("Read more" asks for more of it); 0 outside the timeline.
    property real message: 0

    property bool revealed: false
    property bool recoloring: false
    // The text of a page (a description, an about text: `DescriptionText`) keeps its selection while a menu of the
    // page takes the keyboard (and is centred, one Tab stop and selectable by the keyboard: `DescriptionText` binds
    // that on the TextEdit, so a message's row pays for none of it). A message's text keeps none.
    property bool keepSelection: false
    // What a screen reader calls the text of a page; a message's text is named by its row.
    property string accessibleName
    // The TextEdit itself: the page's menu reads the selection and the links from it.
    readonly property alias edit: edit
    // The "Open Link?" dialog while one asks (tests hand its key filters stand-in events).
    property var question: null
    // The text goes on beyond what is shown.
    readonly property bool hasMore: root.block.more === true
    // How many words of it are not shown (the hint in "Read more").
    readonly property int moreWords: root.block.moreWords ?? 0
    // The theme's link color, adjusted until it can be read on this bubble.
    readonly property color linkInk: Style.readable(Kirigami.Theme.linkColor, root.surface)

    onLinkInkChanged: Qt.callLater(root.recolor)
    onInkChanged: Qt.callLater(root.recolor)

    implicitWidth: Math.max(Math.ceil(edit.contentWidth), root.hasMore ? Math.ceil(readMore.implicitWidth) : 0)
    implicitHeight: Math.ceil(edit.contentHeight) + (root.hasMore ? Kirigami.Units.smallSpacing + Math.ceil(readMore.implicitHeight) : 0)

    // A mention (`kourier:mention/<i>`, made by the viewmodel; a message cannot make one) opens
    // that person's page. Not while a spoiler hides it: the spoiler's rectangle takes the click that
    // reveals it (the text never sees that click), but the rectangle is one pixel short above and
    // below, and a click on those rows reaches the text's link while the spoiler is still on. Not
    // outside the timeline either (the list of earlier versions has no `message`).
    function openMention(link: string): void {
        const m = root.block.mentions ? root.block.mentions[parseInt(link.slice(16))] : undefined;
        if (m === undefined || root.message <= 0 || (m.spoiler && !root.revealed)) {
            return;
        }
        TimelineActions.person(m.user);
    }

    // Asks whether to open `link` (the one question of the timeline and of the pages' texts): the
    // address opens only when the dialog is answered "Open".
    function openLink(link: string): void {
        const dialog = linkDialogComponent.createObject(root, {
            url: link
        }) as QQC2.Dialog;
        // (Kept for the tests: the question that is asking now; null once it is gone.)
        root.question = dialog;
        dialog?.open();
    }

    // Shows the next step of the text.
    function readMoreText(): void {
        if (root.hasMore && root.message > 0) {
            TimelineActions.readMore(root.message);
        }
    }

    TextEdit {
        id: edit

        width: root.width
        text: root.block.html
        textFormat: TextEdit.RichText
        readOnly: true
        selectByMouse: true
        persistentSelection: root.keepSelection
        wrapMode: TextEdit.Wrap
        Accessible.name: root.accessibleName
        color: root.ink
        // Selected text is read on the selection color, which in Breeze is white on blue at 2.5 : 1.
        selectionColor: Kirigami.Theme.highlightColor
        selectedTextColor: Kirigami.Theme.highlightedTextColor
        // The theme's font (a raw TextEdit would not follow the font setting otherwise); big
        // emoji get the emoji font at three times the size.
        font: root.block.jumbo ? Qt.font({
            "family": "emoji",
            "pointSize": Kirigami.Theme.defaultFont.pointSize * 3.2
        }) : Kirigami.Theme.defaultFont

        onTextChanged: root.recolor()
        Component.onCompleted: root.recolor()

        onLinkActivated: link => {
            if (link.indexOf("kourier:mention/") === 0) {
                root.openMention(link);
                return;
            }
            root.openLink(link);
        }

        HoverHandler {
            // (A mention opens the person only in the timeline: in the list of earlier versions,
            // which has no `message`, it is no button, so no hand.)
            cursorShape: edit.hoveredLink !== "" && (root.message > 0 || edit.hoveredLink.indexOf("kourier:mention/") !== 0) ? Qt.PointingHandCursor : Qt.IBeamCursor
        }
    }

    // Under the text, in the link color the links of the text have.
    QQC2.Label {
        id: readMore

        visible: root.hasMore
        y: Math.ceil(edit.contentHeight) + Kirigami.Units.smallSpacing
        // With how much is left, counted in words.
        text: L10n.i18ncp("@action:button, %1 how many words are not shown", "Read more (%1 word)", "Read more (%1 words)", root.moreWords)
        textFormat: Text.PlainText
        color: root.linkInk
        font: Style.styled(Kirigami.Theme.defaultFont, {
            "underline": readMoreHover.hovered
        })
        Accessible.role: Accessible.Button
        Accessible.name: L10n.i18ncp("@action:button accessible, %1 how many words are not shown", "Read more of this message, %1 word", "Read more of this message, %1 words", root.moreWords)
        Accessible.onPressAction: root.readMoreText()

        HoverHandler {
            id: readMoreHover
            cursorShape: Qt.PointingHandCursor
        }
        TapHandler {
            onTapped: root.readMoreText()
        }
    }

    // Qt would color links from the application palette, which need not match the bubble.
    function recolor(): void {
        // Recoloring edits the document, which emits textChanged again: guard against that.
        if (root.recoloring || root.block.html.indexOf("<a ") < 0) {
            return;
        }
        root.recoloring = true;
        App.colorLinks(edit.textDocument, root.linkInk, root.ink);
        root.recoloring = false;
    }

    // Rectangles covering each spoiler range, one per visual line.
    readonly property var spoilerRects: {
        const spoilers = root.block.spoilers;
        if (root.revealed || !spoilers || spoilers.length === 0 || edit.contentHeight <= 0 || edit.width <= 0) {
            return [];
        }
        const out = [];
        for (const sp of spoilers) {
            const end = Math.min(sp.start + sp.len, edit.length);
            if (sp.start >= end) {
                continue;
            }
            const first = edit.positionToRectangle(sp.start);
            let line = {
                x: first.x,
                y: first.y,
                h: first.height,
                x2: first.x
            };
            for (let p = sp.start + 1; p <= end; ++p) {
                const r = edit.positionToRectangle(p);
                if (r.y > line.y + line.h / 2) {
                    out.push({
                        x: line.x,
                        y: line.y,
                        w: Math.max(line.x2, edit.contentWidth) - line.x,
                        h: line.h
                    });
                    line = {
                        x: r.x,
                        y: r.y,
                        h: r.height,
                        x2: r.x
                    };
                } else {
                    line.x2 = r.x;
                }
            }
            out.push({
                x: line.x,
                y: line.y,
                w: line.x2 - line.x,
                h: line.h
            });
        }
        return out;
    }

    Repeater {
        model: root.spoilerRects
        delegate: Rectangle {
            required property var modelData
            x: modelData.x - 1
            y: modelData.y + 1
            width: modelData.w + 2
            height: modelData.h - 2
            radius: Kirigami.Units.cornerRadius / 2
            color: Kirigami.ColorUtils.linearInterpolation(root.surface, root.ink, 0.6)
            TapHandler {
                onTapped: root.revealed = true
            }
        }
    }

    // Links open only after a confirmation (§8.4); the dialog is created on demand.
    Component {
        id: linkDialogComponent

        QQC2.Dialog {
            id: linkDialog

            property string url
            // Keys pressed while the dialog was open (their release may act).
            property var pressedKeys: ({})

            // Lets through what the dialog's buttons may act on: a key pressed while it is open, never an
            // auto-repeat, never a release whose press came before (the key that chose "Open Link…" in a
            // menu opens this dialog inside its own press). Tab and Backtab always go on: an accepted repeat
            // of a held Tab would stop the focus chain here. `event` is a KeyEvent or, in tests, a stand-in.
            function filterPress(event: var): void {
                if (event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) {
                    return;
                }
                if (!linkDialog.opened || event.isAutoRepeat) {
                    event.accepted = true;
                    return;
                }
                const seen = linkDialog.pressedKeys;
                seen[event.key] = true;
                linkDialog.pressedKeys = seen;
            }

            function filterRelease(event: var): void {
                if (event.key === Qt.Key_Tab || event.key === Qt.Key_Backtab) {
                    return;
                }
                const seen = linkDialog.pressedKeys;
                if (!linkDialog.opened || event.isAutoRepeat || seen[event.key] !== true) {
                    event.accepted = true;
                    return;
                }
                delete seen[event.key];
                linkDialog.pressedKeys = seen;
            }

            parent: QQC2.Overlay.overlay
            anchors.centerIn: parent
            modal: true
            title: L10n.i18nc("@title:dialog", "Open Link?")
            standardButtons: QQC2.Dialog.Open | QQC2.Dialog.Cancel
            onAccepted: App.openExternalUrl(linkDialog.url)
            onOpened: linkDialog.pressedKeys = ({})
            onClosed: {
                root.question = null;
                linkDialog.destroy();
            }

            footer: QQC2.DialogButtonBox {
                standardButtons: linkDialog.standardButtons
                // The buttons filter the keys (the held-key and press/release guard above).
                delegate: QQC2.Button {
                    Keys.onPressed: event => linkDialog.filterPress(event)
                    Keys.onReleased: event => linkDialog.filterRelease(event)
                }
            }

            contentItem: QQC2.Label {
                // The address between a left-to-right isolate (U+2066) and its pop (U+2069): it
                // reads left to right whatever it holds, and nothing in it reorders anything outside
                // it. Qt lays even a plain text out with the bidi algorithm, so a right-to-left
                // character in an address would read as another address (the dialog is the guard
                // that shows what opens). `richtext` ends a link before such a character; this is
                // for the one that gets through.
                text: "\u2066" + linkDialog.url + "\u2069"
                wrapMode: Text.WrapAnywhere
                textFormat: Text.PlainText
            }
        }
    }
}
