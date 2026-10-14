//! QML smoke test: the `App` singleton with the fixture, driven from QML through the same
//! invokables the pages use (open a chat, send, switch accounts, search); the color helpers
//! (contrast on both bubbles), `Style.plain` (text from outside never becomes markup in a
//! label that guesses its format), the list page's notice, the file picker's filters and
//! folder mode, what the custom timer dialog says about a time between two steps, and which
//! logo the UI takes (the icon theme's icon of the application when the theme has one, else
//! the bundled file). Offscreen, `harness = false`.

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_qt::{APP_ID, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import app.kourier

Item {
    property int before: 0

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
            App.selectAccount(-1);
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
