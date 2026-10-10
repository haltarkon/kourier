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
    // The text goes on beyond what is shown.
    readonly property bool hasMore: root.block.more === true
    // How many words of it are not shown (the hint in "Read more").
    readonly property int moreWords: root.block.moreWords ?? 0
    // The theme's link color, adjusted until it can be read on this bubble.
    readonly property color linkInk: Style.readable(Kirigami.Theme.linkColor, root.surface)

    onLinkInkChanged: Qt.callLater(root.recolor)

    implicitWidth: Math.max(Math.ceil(edit.contentWidth), root.hasMore ? Math.ceil(readMore.implicitWidth) : 0)
    implicitHeight: Math.ceil(edit.contentHeight) + (root.hasMore ? Kirigami.Units.smallSpacing + Math.ceil(readMore.implicitHeight) : 0)

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
        persistentSelection: false
        wrapMode: TextEdit.Wrap
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
            const dialog = linkDialogComponent.createObject(root, {
                url: link
            }) as QQC2.Dialog;
            dialog?.open();
        }

        HoverHandler {
            cursorShape: edit.hoveredLink !== "" ? Qt.PointingHandCursor : Qt.IBeamCursor
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
        App.colorLinks(edit.textDocument, root.linkInk);
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

            parent: QQC2.Overlay.overlay
            anchors.centerIn: parent
            modal: true
            title: L10n.i18nc("@title:dialog", "Open Link?")
            standardButtons: QQC2.Dialog.Open | QQC2.Dialog.Cancel
            onAccepted: Qt.openUrlExternally(linkDialog.url)
            onClosed: linkDialog.destroy()

            contentItem: QQC2.Label {
                text: linkDialog.url
                wrapMode: Text.WrapAnywhere
                textFormat: Text.PlainText
            }
        }
    }
}
