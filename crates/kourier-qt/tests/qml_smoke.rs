//! QML smoke test: the `App` singleton with the fixture, driven from QML through the same
//! invokables the pages use (open a chat, send, switch accounts, search); the color helpers
//! (contrast on both bubbles), `Style.plain` (text from outside never becomes markup in a
//! label that guesses its format), the list page's notice, the file picker's filters and
//! folder mode, the "~" mark of a chosen name that never reaches a monogram, a colour or an accessible name (the chat header's and the details panel's too), what the custom timer dialog says about a time between two steps, and which
//! logo the UI takes (the icon theme's icon of the application when the theme has one, else
//! the bundled file). Offscreen, `harness = false`.

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_qt::{APP_ID, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import org.kde.kirigami as Kirigami
import app.kourier

Item {
    property int before: 0

    // The list's rows and the panel's accounts as the roles give them (blended chats).
    Instantiator {
        id: convRows
        model: App.conversations
        delegate: QtObject {
            required property real accountId
            required property real conversationId
            required property string accountBadge
            required property int badgeNumber
        }
    }
    // The real rows (ConversationDelegate) and two avatars with a state dot, one of them
    // mirrored, in a window of their own: the badge's digit and the dot's corner.
    Window {
        visible: true
        width: 500
        height: 3000
        ListView {
            id: rowsView
            width: 500
            height: 3000
            model: App.conversations
            delegate: ConversationDelegate {
                width: ListView.view.width
            }
        }
        Avatar {
            id: dotLtr
            y: 10
            x: 10
            width: 40
            height: 40
            name: "Dot Test"
            status: "online"
        }
        Item {
            x: 100
            width: 60
            height: 60
            LayoutMirroring.enabled: true
            LayoutMirroring.childrenInherit: true
            Avatar {
                id: dotRtl
                y: 10
                x: 10
                width: 40
                height: 40
                name: "Dot Test"
                status: "online"
            }
        }
    }
    // The digit each row's avatar shows ("" for none), by conversation id.
    function digits(): var {
        const out = {};
        for (const row of rowsView.contentItem.children) {
            if (row.conversationId === undefined) continue;
            const digit = findNamed(row, "avatarBadgeDigit");
            // (A shown digit without text still widens the badge into a pill.)
            out[row.conversationId] = digit && digit.visible ? (digit.text === "" ? "(an empty digit)" : digit.text) : "";
        }
        return out;
    }
    // The rows of `account` and the digits their avatars show, or a reason.
    function checkDigits(account: real, want: string): string {
        const shown = digits();
        const rows = listed().filter(r => r.accountId === account);
        if (rows.length === 0) return "no row of account " + account;
        for (const r of rows) {
            if (shown[r.conversationId] === undefined) return "no delegate for a row of account " + account + " (" + r.conversationId + "; delegates " + Object.keys(shown).join(",") + "; view " + rowsView.count + "/" + rowsView.height + ")";
            if (shown[r.conversationId] !== want) return "account " + account + " shows '" + shown[r.conversationId] + "', not '" + want + "'";
        }
        return "";
    }
    // A row whose chip shows names its account in its accessible description; a row without
    // a chip does not.
    function checkNamed(): string {
        let chips = 0;
        for (const row of rowsView.contentItem.children) {
            if (row.conversationId === undefined) continue;
            const said = row.Accessible.description.indexOf("Account ") === 0;
            if (row.accountBadge !== "") {
                chips++;
                if (row.Accessible.description.indexOf("Account " + row.accountBadge) !== 0) return "a row with the chip '" + row.accountBadge + "' says: " + row.Accessible.description;
            } else if (said) {
                return "a row without a chip names an account: " + row.Accessible.description;
            }
        }
        return chips > 0 ? "" : "no row with a chip";
    }
    Component {
        id: avatarComponent
        Avatar {
            width: 40
            height: 40
        }
    }
    Component {
        id: quoteComponent
        QuoteBlock {
            ink: "#232629"
            surface: "#ffffff"
        }
    }
    Component {
        id: headerPageComponent
        ChatPage {}
    }
    Component {
        id: detailsPanelComponent
        DetailsPanel {}
    }
    // The "~" mark of a name a person chose for themself (WhatsApp, W6a2 PRIV-1) is part of the
    // shown string but never of what hashes, splits or speaks the name: the monogram's letters
    // and colour, the author's colour, the quote bar's colour and the accessible names are those
    // of the unmarked name, and a name with no mark is untouched.
    function checkMarkedNames(): string {
        if (Style.unmarked("~Rika Lowe") !== "Rika Lowe") return "one mark is dropped: " + Style.unmarked("~Rika Lowe");
        if (Style.unmarked("~~Rika") !== "~Rika") return "only one mark is dropped: " + Style.unmarked("~~Rika");
        if (Style.unmarked("Rika Lowe") !== "Rika Lowe" || Style.unmarked("") !== "") return "a name with no mark stays";
        const bare = avatarComponent.createObject(this, {
            "name": "Rika Lowe"
        });
        const marked = avatarComponent.createObject(this, {
            "name": "~Rika Lowe"
        });
        const letters = findNamed(marked, "avatarInitials").text;
        const colorKept = Qt.colorEqual(bare.placeholderColor, marked.placeholderColor);
        const glyphKept = bare.glyph === marked.glyph;
        const spoken = findNamed(marked, "avatarShape").Accessible.name;
        bare.destroy();
        marked.destroy();
        if (letters !== "RL") return "the monogram of a marked name is '" + letters + "'";
        if (!colorKept) return "the monogram's colour moved with the mark";
        if (!glyphKept) return "the glyph check moved with the mark";
        if (spoken !== "Rika Lowe") return "the avatar says '" + spoken + "'";
        // A marked placeholder title is still a placeholder (the number is no push name, but the rule is one).
        const number = avatarComponent.createObject(this, {
            "name": "~+49 151 2345"
        });
        const anonymous = number.anonymous;
        number.destroy();
        if (!anonymous) return "a marked number is no placeholder";
        // The author's colour and the quote bar's colour, as the timeline makes them.
        const plainRow = messageWith([textOf(20)], "Rika Lowe", true);
        const markedRow = messageWith([textOf(20)], "~Rika Lowe", true);
        const rowColorKept = Qt.colorEqual(plainRow.authorColor, markedRow.authorColor);
        const rowSpoken = markedRow.Accessible.name;
        const rowSpokenBare = plainRow.Accessible.name;
        plainRow.destroy();
        markedRow.destroy();
        if (!rowColorKept) return "the author's colour moved with the mark";
        if (rowSpoken !== rowSpokenBare) return "the row says '" + rowSpoken + "', not '" + rowSpokenBare + "'";
        const quoteOf = author => quoteComponent.createObject(this, {
                "block": {
                    "type": "quote",
                    "target": 5,
                    "own": false,
                    "author": author,
                    "text": "a quoted text",
                    "icon": "",
                    "thumbUrl": "",
                    "standIn": false
                }
            });
        const plainQuote = quoteOf("Rika Lowe");
        const markedQuote = quoteOf("~Rika Lowe");
        const barKept = Qt.colorEqual(plainQuote.barColor, markedQuote.barColor);
        const quoteSpoken = markedQuote.Accessible.name;
        const quoteSpokenBare = plainQuote.Accessible.name;
        plainQuote.destroy();
        markedQuote.destroy();
        if (!barKept) return "the quote bar's colour moved with the mark";
        if (quoteSpoken !== quoteSpokenBare) return "the quote says '" + quoteSpoken + "'";
        // The chat header's and the details panel's accessible names (G1): what a screen reader is
        // handed, with the header open on the details and without.
        const page = headerPageComponent.createObject(this);
        const header = page ? [page.headerName("~Rika Lowe", false), page.headerName("~Rika Lowe", true), page.headerName("Rika Lowe", false), page.headerName("Rika Lowe", true)] : null;
        page?.destroy();
        if (!header) return "no chat page";
        if (header[0] !== "Rika Lowe" || header[0] !== header[2]) return "the header says '" + header[0] + "'";
        if (header[1] !== "Details of Rika Lowe" || header[1] !== header[3]) return "the header on the details says '" + header[1] + "'";
        const panel = detailsPanelComponent.createObject(this);
        const pane = panel ? [panel.spokenName("person", "~Rika Lowe", ""), panel.spokenName("group", "", "~Rika Lowe"), panel.spokenName("members", "", "~Rika Lowe"), panel.spokenName("person", "Rika Lowe", ""), panel.spokenName("person", undefined, undefined), panel.spokenName("media", "~Rika Lowe", "~Rika Lowe")] : null;
        panel?.destroy();
        if (!pane) return "no details panel";
        if (pane[0] !== "Rika Lowe" || pane[0] !== pane[3]) return "the person's pane says '" + pane[0] + "'";
        if (pane[1] !== "Rika Lowe" || pane[2] !== "Rika Lowe") return "the group's pane says '" + pane[1] + "' / '" + pane[2] + "'";
        if (pane[4] !== "") return "a pane with no name says '" + pane[4] + "'";
        if (pane[5] !== "All Media and Files") return "the media pane says '" + pane[5] + "'";
        return "";
    }
    function checkDot(): string {
        const ltr = findNamed(dotLtr, "avatarStatusDot");
        const rtl = findNamed(dotRtl, "avatarStatusDot");
        if (!ltr || !rtl || !ltr.visible) return "no dot";
        // An avatar without a badge makes no badge item (one Loader, inactive).
        const badge = findNamed(dotLtr, "avatarBadge");
        if (!badge || badge.active || badge.item !== null) return "an avatar without a badge made its badge";
        if (ltr.x + ltr.width / 2 >= dotLtr.width / 2 || ltr.y > 0) return "the dot is not at the top start (left) corner: x " + ltr.x;
        if (rtl.x + rtl.width / 2 <= dotRtl.width / 2) return "the dot does not mirror: x " + rtl.x;
        return "";
    }
    Instantiator {
        id: accountRows
        model: App.accounts
        delegate: QtObject {
            required property real accountId
            required property bool shown
            required property int place
        }
    }
    function listed(): var {
        const out = [];
        for (let i = 0; i < convRows.count; i++) out.push(convRows.objectAt(i));
        return out;
    }
    function panel(): string {
        const out = [];
        for (let i = 0; i < accountRows.count; i++) {
            const a = accountRows.objectAt(i);
            out.push(a.accountId + ":" + a.shown + ":" + a.place);
        }
        return out.join(" ");
    }
    // Which accounts' chats the list holds, sorted.
    function accountsListed(): string {
        return [...new Set(listed().map(r => r.accountId))].sort().join(",");
    }

    function fail(msg: string): void {
        console.error("FAIL: " + msg);
        Qt.exit(1);
    }

    // Sender names (the avatar palette), links and accents are readable on both bubbles, in
    // a light and a dark scheme: at least 4.5:1, by a change of lightness only, and colors
    // that are readable already stay as they are.
    function checkContrast(): string {
        const schemes = [
            // Breeze Light and Breeze Dark: background, text, accent.
            [Qt.color("#ffffff"), Qt.color("#232629"), Qt.color("#3daee9")],
            [Qt.color("#1b1e20"), Qt.color("#fcfcfc"), Qt.color("#3daee9")]
        ];
        const names = ["#c7f20c", "#f2e30c", "#f29b0c", "#0cf2d3", "#f20c9b", "#e93d58", "#3daee9", "#2980b9", "#7f8c8d", "#ffffff", "#000000"];
        for (const [bg, text, accent] of schemes) {
            const bubbles = [Style.ownBubble(bg, accent), Style.peerBubble(bg, text), bg];
            // Own bubbles are a tint of the page, not the accent itself: normal text reads on them.
            if (Style.contrast(text, bubbles[0]) < 7) return "text on the own bubble " + Style.contrast(text, bubbles[0]);
            if (Style.contrast(bubbles[0], bg) < 1.05) return "the own bubble does not stand out";
            for (const bubble of bubbles) {
                for (const name of names) {
                    const c = Qt.color(name);
                    const shown = Style.readable(c, bubble);
                    const ratio = Style.contrast(shown, bubble);
                    if (ratio < Style.minContrast) return name + " on " + bubble + ": " + ratio;
                    if (Style.contrast(c, bubble) >= Style.minContrast && !Qt.colorEqual(shown, c)) return name + " was changed needlessly";
                    if (c.hslSaturation > 0.2 && Math.abs(shown.hslHue - c.hslHue) > 0.02) return name + " lost its hue";
                }
                const quiet = Style.secondary(text, bubble);
                if (Style.contrast(quiet, bubble) < Style.minContrast) return "secondary text on " + bubble;
                if (Style.contrast(quiet, bubble) >= Style.contrast(text, bubble)) return "secondary text is not quieter";
            }
        }
        if (Math.abs(Style.contrast(Qt.color("#000000"), Qt.color("#ffffff")) - 21) > 0.01) return "the contrast formula";
        return "";
    }

    // Text with parts from outside (a name, a reaction, a reason the network gave) in a label
    // that guesses its format: through `Style.plain` it is laid out exactly like plain text,
    // while the same markup unguarded is rendered (the control that the check can tell).
    readonly property string markup: "<b>Ari</b><br><i>reacted</i> &amp; left"
    Text {
        id: guessed
        textFormat: Text.AutoText
        text: Style.plain(parent.markup)
    }
    Text {
        id: literal
        textFormat: Text.PlainText
        text: Style.plain(parent.markup)
    }
    Text {
        id: unguarded
        textFormat: Text.AutoText
        text: parent.markup
    }
    Text {
        id: unguardedLiteral
        textFormat: Text.PlainText
        text: parent.markup
    }

    function checkPlain(): string {
        if (Style.plain("a<b>&c") !== "a\u2039b\u203a&c") return "angle brackets are replaced: " + Style.plain("a<b>&c");
        if (Style.plain("Offline. Nothing to change.") !== "Offline. Nothing to change.") return "plain words stay";
        if (guessed.implicitWidth <= 0 || guessed.implicitWidth !== literal.implicitWidth || guessed.implicitHeight !== literal.implicitHeight) {
            return "guarded text was read as markup: " + guessed.implicitWidth + "x" + guessed.implicitHeight + " vs " + literal.implicitWidth + "x" + literal.implicitHeight;
        }
        if (unguarded.implicitWidth === unguardedLiteral.implicitWidth) return "the control did not render its markup";
        return "";
    }

    // The conversation list as the window holds it: it shows the core's notice while no
    // chat's composer can.
    ConversationListPage {
        id: listPage
        width: 400
        height: 600
    }

    // The visible notice of the list page with this text, if any.
    function listNotice(item: Item, text: string): bool {
        if (!item) {
            return false;
        }
        for (let i = 0; i < item.children.length; ++i) {
            const child = item.children[i];
            if (child.visible && child.type !== undefined && child.showCloseButton !== undefined && child.text === text) {
                return true;
            }
            if (child.visible && listNotice(child, text)) {
                return true;
            }
        }
        return false;
    }

    function checkListNotice(): string {
        const notice = "The verification was not accepted. Try it again from the banner.";
        if (App.timeline.conversationId !== 0) return "no chat is open at the start";
        if (listNotice(listPage.header, notice)) return "a notice before there is one";
        App.composer.notice = notice;
        if (!listPage.showsNotice || !listNotice(listPage.header, notice)) return "without an open chat the list shows the notice";
        // With a chat open (and writable) its composer shows the notice: the list does not.
        App.openConversation(2);
        if (App.timeline.cannotSend !== "") return "the fixture chat can be written to";
        if (listPage.showsNotice || listNotice(listPage.header, notice)) return "the notice shows twice";
        // The window says when the list covers the chat (narrow): then it is the list's again.
        listPage.showsNotice = true;
        if (!listNotice(listPage.header, notice)) return "the list shows the notice when the chat is not on screen";
        App.composer.clearNotice();
        if (listNotice(listPage.header, notice)) return "a dismissed notice stays";
        App.closeConversation();
        return "";
    }

    Component {
        id: pickerComponent
        AttachDialog {
            autoOpen: false
        }
    }

    // The file picker's filter is a convenience, never a gate: where a chat takes only some
    // kinds of files the dialog starts on them, and "All files" is the other choice (the core
    // reads what a file is from its content, not from its name).
    function checkPicker(): string {
        const some = pickerComponent.createObject(this, {
            "kinds": "image,gif,video"
        });
        const filters = some.filters();
        some.destroy();
        if (filters.length !== 2) return "filters for a chat that takes media only: " + filters.length;
        if (filters[0].indexOf("*.jpg") < 0 || filters[0].indexOf("*.gif") < 0 || filters[0].indexOf("*.mp4") < 0) return "the media filter: " + filters[0];
        if (filters[0].indexOf("*.mp3") >= 0) return "a kind the chat does not take is in the filter";
        if (!filters[1].endsWith("(*)")) return "no way to pick a file the filter does not know: " + filters[1];
        const any = pickerComponent.createObject(this, {
            "kinds": "image,file"
        });
        const none = any.filters();
        // The same picker asks for a folder instead when told to (the attach button's
        // second entry); by default it asks for files.
        if (any.folder !== false) return "the picker asks for files unless told otherwise";
        any.destroy();
        if (none.length !== 0) return "a chat that takes any file needs no filter";
        const folder = pickerComponent.createObject(this, {
            "kinds": "image,file",
            "folder": true
        });
        if (folder.folder !== true) return "the picker's folder mode";
        folder.destroy();
        return "";
    }

    Component {
        id: timerComponent
        TimerDialog {}
    }

    // The custom timer dialog says why "Set" waits. Outside the range: the range. Between
    // two of the times a network takes (it takes every `step`-th second only): that, and the
    // nearest times that can be set.
    function checkTimerSteps(): string {
        const note = d => {
            for (const child of d.contentItem.children) {
                const hit = findNamed(child, "timerRange");
                if (hit) {
                    return hit.text;
                }
            }
            return "";
        };
        // One minute to one hour, in steps of five minutes from the first minute.
        const d = timerComponent.createObject(this, {
            "min": 60,
            "max": 3600,
            "step": 300
        });
        d.show(360);
        if (!d.valid || d.offStep) return "6 minutes is a step: " + d.seconds;
        if (note(d) !== "Between 1 minute and 1 hour.") return "the range: " + note(d);
        d.show(420);
        if (d.valid || !d.offStep) return "7 minutes lies between two steps";
        if (note(d) !== "Times go in steps of 5 minutes here: the nearest are 6 minutes and 11 minutes.") return "why Set waits: " + note(d);
        // Above the last step there is only the one below.
        d.show(3540);
        if (d.valid) return "59 minutes lies behind the last step";
        if (note(d) !== "Times go in steps of 5 minutes here: the nearest is 56 minutes.") return "at the end of the range: " + note(d);
        // Outside the range it is the range that is said, not the step.
        d.show(7200);
        if (d.valid || d.offStep) return "two hours is outside the range";
        if (note(d) !== "Between 1 minute and 1 hour.") return "outside the range: " + note(d);
        d.destroy();
        // A network that takes every second has no steps to speak of.
        const any = timerComponent.createObject(this, {
            "min": 1,
            "max": 2419200,
            "step": 1
        });
        any.show(95);
        if (!any.valid || any.offStep) return "95 seconds where every second is taken";
        if (note(any) !== "Between 1 second and 4 weeks.") return "the plain range: " + note(any);
        any.destroy();
        return "";
    }

    // The logo is the icon theme's own icon of the application id when the theme has one (an
    // installed Kourier), else the bundled file (a test run, and every screenshot of the
    // fixture). Which of the two it is depends on the machine: the answer must be consistent.
    function checkLogo(): string {
        const themed = App.themedAppIcon();
        if (themed !== "" && themed !== "@APP_ID@") return "the theme's name is " + themed;
        const want = themed !== "" ? themed : Style.icon("kourier");
        if (Style.appIcon !== want) return "Style.appIcon is " + Style.appIcon + ", not " + want;
        if (themed === "" && Style.appIcon.indexOf("/icons/kourier.svg") < 0) return "the bundled file is " + Style.appIcon;
        return "";
    }

    Component {
        id: imageBlockComponent
        ImageBlock {}
    }

    // A block as the viewmodel hands one to the timeline.
    function pictureBlock(sticker: bool, state: string): var {
        return {
            "attachment": 7,
            "state": state,
            "sticker": sticker,
            "animated": false,
            "kind": "image",
            "width": 512,
            "height": 512,
            "blurhash": "",
            "placeholderUrl": "",
            "url": "",
            "fullUrl": "",
            "media": "",
            "label": "",
            "progress": 0
        };
    }

    // A sticker that cannot be had ("No longer available", "Not available on this device") is
    // a framed box as high as its note, not a hole of the size of a sticker with a pill in its
    // corner; one that can be had keeps its size. In a small tile of an album the note is cut
    // to the tile's width (its whole text is the block's accessible description), and in a
    // wide one it is not.
    function checkGoneMedia(): string {
        for (const state of ["unavailable", "expired"]) {
            const gone = imageBlockComponent.createObject(this, {
                "block": pictureBlock(true, state)
            });
            const box = findNamed(gone, "standInBox");
            const is = gone.standIn;
            const shown = box !== null && box.visible;
            const height = gone.implicitHeight;
            gone.destroy();
            if (!is || !shown) return "no stand-in is shown for a sticker that is " + state;
            if (height <= 0 || height >= Style.stickerSize * 0.6) return "the stand-in is not as high as its note: " + height + " of " + Style.stickerSize;
        }
        const here = imageBlockComponent.createObject(this, {
            "block": pictureBlock(true, "remote")
        });
        const hereBox = findNamed(here, "standInBox");
        const hereShown = hereBox !== null && hereBox.visible;
        const size = here.implicitHeight;
        here.destroy();
        if (hereShown || size < Style.stickerSize * 0.9) return "a sticker that can be had keeps its size: " + size;
        for (const [width, cut] of [[80, true], [400, false]]) {
            const tile = imageBlockComponent.createObject(this, {
                "block": pictureBlock(false, "unavailable"),
                "tile": true,
                "width": width,
                "height": width
            });
            const pill = findNamed(tile, "note");
            const label = findNamed(tile, "noteLabel");
            const summary = tile.Accessible.description + " | " + pill.visible + " " + pill.width + " " + label.truncated;
            const ok = [pill.visible, pill.width <= width, label.truncated === cut];
            tile.destroy();
            if (!ok[0]) return "no note on a picture that is not available: " + summary;
            if (!ok[1]) return "the note is wider than its " + width + " px tile: " + summary;
            if (!ok[2]) return "the note of a " + width + " px tile is " + (cut ? "not cut" : "cut") + ": " + summary;
        }
        return "";
    }

    // A picture that cannot be had and can be asked for shows the round badge in the middle of
    // its box and the note at the bottom left. A flat picture's box is as high as the badge
    // (the minimum height), so the note takes the room left of the badge instead of lying under
    // it; a tall picture's note keeps all its words, and so does a picture of an ordinary wide
    // shape (200 x 90, a box 90 px tall: the badge's lowest point and the note's corner meet,
    // and that is no reason to cut the words to the little that fits beside the badge).
    //
    // In a right-to-left layout the note stands at the box's right corner (the start of the line)
    // and takes the room right of the badge.
    function checkFlatPicture(): string {
        const small = Kirigami.Units.smallSpacing;
        for (const mirrored of [false, true]) {
            for (const [width, height, flat] of [[6000, 300, true], [1080, 2340, false], [200, 90, false]]) {
                const block = Object.assign({}, pictureBlock(false, "unavailable"), {
                    "width": width,
                    "height": height,
                    "ask": true
                });
                const item = imageBlockComponent.createObject(mirrored ? mirroredHost : this, {
                    "block": block
                });
                const badge = findNamed(item, "badge");
                const pill = findNamed(item, "note");
                const label = findNamed(item, "noteLabel");
                const summary = (mirrored ? "right-to-left " : "") + width + "x" + height + ": box " + item.width + "x" + item.height + ", badge " + badge.x + "+" + badge.width + ", note " + pill.x + "+" + pill.width + " truncated " + label.truncated;
                const clear = mirrored ? pill.x >= badge.x + badge.width : pill.x + pill.width <= badge.x;
                // (The corner: the start of the line, a small spacing off the box's edge.)
                const corner = mirrored ? Math.abs(pill.x + pill.width + small - item.width) <= 1 : Math.abs(pill.x - small) <= 1;
                const ok = [badge.visible && pill.visible, !flat || item.height < badge.height + pill.height, !flat || clear, flat === label.truncated, pill.x + pill.width <= item.width && pill.x >= 0, corner];
                item.destroy();
                if (!ok[0]) return "badge or note missing: " + summary;
                if (!ok[1]) return "the box was meant to be too flat for both: " + summary;
                if (!ok[2]) return "the note lies under the badge: " + summary;
                if (!ok[3]) return "the note of a " + (flat ? "flat" : "tall") + " picture is " + (flat ? "not cut" : "cut") + ": " + summary;
                if (!ok[4]) return "the note is outside the box: " + summary;
                if (!ok[5]) return "the note is not at the " + (mirrored ? "right" : "left") + " corner of the box: " + summary;
            }
        }
        return "";
    }

    // The host of a right-to-left block: its children lay out from the right.
    Item {
        id: mirroredHost
        LayoutMirroring.enabled: true
        LayoutMirroring.childrenInherit: true
    }

    Component {
        id: messageComponent
        MessageDelegate {}
    }

    // A message row with these blocks, as the timeline makes one.
    function messageWith(blocks: var, author: string, showAuthor: bool): var {
        return messageComponent.createObject(this, {
            "width": 800,
            "index": 0,
            "messageId": 1,
            "own": false,
            "author": author,
            "authorId": "",
            "pages": [],
            "avatarUrl": "",
            "showAuthor": showAuthor,
            "showAvatar": false,
            "groupStart": true,
            "groupEnd": true,
            "timeLabel": "12:00",
            "blocks": blocks,
            "reactions": [],
            "edited": false,
            "deleted": false,
            "receipt": "",
            "actions": ({
                    "any": false
                }),
            "expiry": "",
            "wait": "",
            "waitTimer": ""
        });
    }

    function textOf(chars: int): var {
        let text = "";
        while (text.length < chars) {
            text += "a caption that is long enough to wrap, ";
        }
        return {
            "type": "text",
            "html": text,
            "plain": text,
            "jumbo": false,
            "more": false,
            "moreWords": 0,
            "spoilers": []
        };
    }

    function photoOf(width: int, height: int): var {
        return Object.assign({}, pictureBlock(false, "local"), {
            "type": "image",
            "width": width,
            "height": height
        });
    }

    // A message with a picture is as wide as the picture (a caption wraps at the picture's
    // width, at least 11 grid units; a long author name counts up to that too), and a picture
    // narrower than its bubble stands in the middle of it, not at its left (a file in the same
    // message, a caption under a strip of 126 px).
    function checkPictureBubble(): string {
        const floor = Kirigami.Units.gridUnit * 11;
        const file = {
            "type": "file",
            "attachment": 9,
            "media": "",
            "name": "a-rather-long-name-of-a-file-that-goes-with-the-picture-and-makes-the-row-wide.pdf",
            "detail": "1.2 MB",
            "icon": "application-pdf",
            "state": "local",
            "progress": 0,
            "voice": false,
            "ask": false
        };
        const longName = "A group member with a very long display name that no bubble holds in one line, really";
        // A reply to a long message: the quote is as wide as it gets (18 grid units).
        const quote = {
            "type": "quote",
            "target": 5,
            "own": false,
            "author": "Bob",
            "text": "a quoted message that is long enough to fill the whole line of the quote and go on to a second one",
            "icon": "",
            "thumbUrl": "",
            "standIn": false
        };
        // [name, blocks, a long author name, the bubble's content width at least, at most, the
        // caption fills the content width] (a wrapped caption is as wide as its longest line: under
        // a strip it is up to the floor, not exactly it; where a quote or a file makes the bubble
        // wider than the picture, the caption wraps at that width and stands under the picture's
        // left edge as wide as the bubble, not in a narrow column hugging the left)
        const cases = [["a 4:3 photo with a long caption", [photoOf(4000, 3000), textOf(400)], false, 288, 288, true], ["a phone screenshot with it", [photoOf(1080, 2340), textOf(400)], false, 199, 199, true], ["a thin strip with it", [photoOf(300, 6000), textOf(400)], false, 126, floor, false], ["a photo, a short caption, a long author name", [photoOf(4000, 3000), textOf(2)], true, 288, 288, false], ["a screenshot beside a file", [photoOf(1080, 2340), file], false, 230, 1000, false], ["a quote, a screenshot and a long caption", [quote, photoOf(1080, 2340), textOf(400)], false, 230, 1000, true], ["a file, a screenshot and a long caption", [file, photoOf(1080, 2340), textOf(400)], false, 230, 1000, true]];
        for (const [name, blocks, author, least, most, fills] of cases) {
            const row = messageWith(blocks, author ? longName : "Ann", author);
            const bubble = findNamed(row, "bubble");
            const picture = findNamed(row, "picture");
            const content = bubble.width - row.hpad * 2;
            const middle = picture.mapToItem(bubble, 0, 0).x + picture.width / 2;
            let caption = -1;
            for (const item of row.blockItems()) {
                if (item.block && item.block.type === "text") {
                    caption = item.width;
                }
            }
            const summary = name + ": bubble " + bubble.width + " (content " + content + "), picture " + picture.width + "x" + picture.height + " at " + middle + " of " + bubble.width / 2 + ", text width " + row.textWidth + ", caption " + caption + ", picture width " + row.pictureWidth + ", other width " + row.otherWidth;
            row.destroy();
            if (content < least - 1 || content > most + 1) return "the bubble is not " + least + ".." + most + " px of content: " + summary;
            if (Math.abs(middle - bubble.width / 2) > 1) return "the picture is not in the middle of the bubble: " + summary;
            if (fills && Math.abs(caption - content) > 1) return "the caption is not as wide as the bubble's content: " + summary;
        }
        return "";
    }

    function findNamed(item: Item, name: string): Item {
        if (item.objectName === name) {
            return item;
        }
        for (const child of item.children) {
            const hit = findNamed(child, name);
            if (hit) {
                return hit;
            }
        }
        return null;
    }

    Timer {
        id: start
        interval: 10
        running: true
        onTriggered: {
            const contrast = checkContrast();
            if (contrast !== "") return fail("contrast: " + contrast);
            const picker = checkPicker();
            if (picker !== "") return fail("file picker: " + picker);
            const steps = checkTimerSteps();
            if (steps !== "") return fail("timer steps: " + steps);
            const plain = checkPlain();
            if (plain !== "") return fail("plain text: " + plain);
            const logo = checkLogo();
            if (logo !== "") return fail("logo: " + logo);
            const goneMedia = checkGoneMedia();
            if (goneMedia !== "") return fail("media that cannot be had: " + goneMedia);
            const bubbles = checkPictureBubble();
            if (bubbles !== "") return fail("picture bubbles: " + bubbles);
            const flatPicture = checkFlatPicture();
            if (flatPicture !== "") return fail("flat picture that cannot be had: " + flatPicture);
            if (!App.fixture || !App.hasAccounts) return fail("fixture not loaded");
            const listNote = checkListNotice();
            if (listNote !== "") return fail("list notice: " + listNote);
            if (!App.fixture || !App.hasAccounts) return fail("fixture not loaded");
            if (App.accounts.count !== 3) return fail("accounts " + App.accounts.count);
            if (App.conversations.count < 12) return fail("conversations " + App.conversations.count);
            App.openConversation(2);
            if (App.timeline.conversationId !== 2) return fail("open");
            if (App.timeline.title !== "FPV Squad") return fail("title " + App.timeline.title);
            if (!App.timeline.isGroup || !App.timeline.typing) return fail("header flags");
            before = App.timeline.count;
            if (before < 25) return fail("timeline rows " + before);
            // The message actions of the pure fixture: reply (the banner takes author and
            // text from the row) and edit mode (entered and left; nothing is edited here).
            const own = App.findMessage("own");
            if (own <= 0) return fail("an own message");
            if (App.composer.replyTo(2, 999999)) return fail("a reply to nothing");
            if (!App.composer.replyTo(2, own)) return fail("reply refused");
            if (App.composer.replyToId !== own || App.composer.replyAuthor !== "You" || App.composer.replyText === "") return fail("reply banner");
            if (!App.composer.startEdit(2, own) || App.composer.editId !== own) return fail("edit mode");
            if (App.composer.replyToId !== 0) return fail("an edit drops the reply");
            if (App.composer.editText === "") return fail("the edit starts from the text");
            App.composer.cancelEdit();
            if (App.composer.editId !== 0) return fail("edit not left");
            if (!App.composer.replyTo(2, own)) return fail("reply refused");
            if (!App.composer.send(2, "smoke test")) return fail("send refused");
            if (App.composer.replyToId !== 0) return fail("reply not cleared");
            if (App.bannerKind !== "") return fail("no banner without account trouble");
            if (App.composer.send(2, "   ")) return fail("empty text sent");
            check.start();
        }
    }

    // The refresh is queued (re-entrancy rule): check after the event loop ran it.
    Timer {
        id: check
        interval: 50
        onTriggered: {
            if (App.timeline.count !== before + 1) return fail("rows after send " + App.timeline.count);
            App.selectAccount(2);
            if (App.currentTitle === "All Chats") return fail("account title");
            if (App.conversations.count !== 2) return fail("work chats " + App.conversations.count);
            // ---- blended chats (the refresh's list) ----
            if (App.allShown || App.shownCount !== 1 || App.currentAccount !== 2) return fail("one shown: " + App.allShown + " " + App.shownCount + " " + App.currentAccount);
            if (panel() !== "1:false:1 2:true:2 3:false:0") return fail("panel after a click: " + panel());
            // The number stays with one shown account; the chip needs two.
            if (!listed().every(r => r.badgeNumber === 2 && r.accountBadge === "")) return fail("rows of one account: " + listed().map(r => r.badgeNumber + "/" + r.accountBadge));
            // Shift+click adds account 1: both accounts' chats, newest first, with numbers.
            if (!App.toggleAccount(1)) return fail("toggle 1 refused");
            if (App.shownCount !== 2 || App.allShown || App.currentAccount !== -1) return fail("two shown: " + App.shownCount);
            if (accountsListed() !== "1,2") return fail("accounts listed " + accountsListed());
            if (!listed().every(r => r.badgeNumber === r.accountId)) return fail("numbers 1/2");
            if (!listed().some(r => r.accountBadge === "Work")) return fail("chips with two shown");
            if (App.currentTitle !== "Andrii Melnyk · Andrii (Work)") return fail("title of two: " + App.currentTitle);
            if (App.currentTitleShort !== "2 accounts") return fail("short title: " + App.currentTitleShort);
            if (panel() !== "1:true:1 2:true:2 3:false:0") return fail("panel with two: " + panel());
            // The rows' avatars show the numbers (after the list view's next layout).
            avatarCheck.start();
        }
    }

    Timer {
        id: avatarCheck
        interval: 50
        onTriggered: {
            const second = checkDigits(2, "2");
            if (second !== "") return fail("badge digits: " + second);
            const first = checkDigits(1, "1");
            if (first !== "") return fail("badge digits: " + first);
            const named = checkNamed();
            if (named !== "") return fail("accessible account: " + named);
            const dot = checkDot();
            if (dot !== "") return fail("state dot: " + dot);
            const marks = checkMarkedNames();
            if (marks !== "") return fail("the '~' mark: " + marks);
            // Both off: the last shown account stays.
            if (!App.toggleAccount(2)) return fail("toggle 2 refused");
            if (App.toggleAccount(1)) return fail("the last shown account was hidden");
            if (App.shownCount !== 1 || accountsListed() !== "1") return fail("the last stays: " + accountsListed());
            // A hidden account's unread counts on the narrow window's dot.
            if (App.hiddenUnread < 0) return fail("hidden unread " + App.hiddenUnread);
            App.selectAccount(-1);
            if (!App.allShown || App.shownCount !== 3 || App.currentTitle !== "All Chats" || App.currentTitleShort !== "") return fail("all chats");
            if (App.conversations.count < 12) return fail("all chats listed " + App.conversations.count);
            if (App.hiddenUnread !== 0) return fail("nothing hidden, no hidden unread");
            if (listed().some(r => r.accountId === 3 && r.badgeNumber !== 0)) return fail("Threema alone has no number");
            // Blending off (the setting; applied after the call): a toggle is a click.
            App.settings.blendAccounts = false;
            blendCheck.start();
        }
    }

    Timer {
        id: blendCheck
        interval: 50
        onTriggered: {
            if (App.blend) return fail("blending is still on");
            // Every account shown: the lone Threema account's rows show no digit.
            const threema = checkDigits(3, "");
            if (threema !== "") return fail("badge digits: " + threema);
            const signal = checkDigits(2, "2");
            if (signal !== "") return fail("badge digits with all shown: " + signal);
            if (!App.toggleAccount(1)) return fail("toggle refused with blending off");
            if (App.shownCount !== 1 || App.currentAccount !== 1 || accountsListed() !== "1") return fail("with blending off a toggle is a click: " + accountsListed());
            App.selectAccount(-1);
            App.settings.blendAccounts = true;
            App.setSearch("olena");
            if (App.conversations.count < 1) return fail("search");
            App.setSearch("");
            App.closeConversation();
            if (App.timeline.count !== 0 || App.timeline.conversationId !== 0) return fail("close");
            App.login.start("Signal");
            if (App.login.kind !== "qr" || App.login.qr.indexOf("sgnl://linkdevice") !== 0) return fail("login");
            Qt.exit(0);
        }
    }

    Timer {
        interval: 10000
        running: true
        onTriggered: fail("timeout")
    }
}
"##;

fn main() {
    support::answer_list("qml_smoke");
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before Qt starts.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_options(UiOptions {
        fixture: true,
        ..UiOptions::default()
    });
    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load_data(
            &QByteArray::from(SCRIPT.replace("@APP_ID@", APP_ID).as_str()),
            &QUrl::from("qrc:/kourier-tests/smoke.qml"),
        );
    }
    let code = app.as_mut().map_or(3, |app| app.exec());
    if code == 0 {
        println!("test qml smoke ... ok\n\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!(
            "test qml smoke ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
