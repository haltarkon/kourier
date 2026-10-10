//! Message actions through the real core over the fake network (S13 UI, §8.3): an own message
//! is edited from the composer's edit mode (Up in an empty composer, Escape, the text typed
//! before comes back), reacted to and un-reacted, replied to with text and with a file (the
//! quote names its original and carries its thumbnail), a quote jumps to its original and
//! highlights it, a message is deleted for me (the row goes) and for everyone (a tombstone that
//! offers only its removal), the disappearing-message timer is changed from the conversation's
//! offered values (header mark, system row, the new message's timer mark), our own answer
//! removes the "unread messages" divider, a send the core refuses hands its file, caption and
//! quote back to the composer, a refused action shows as a notice, and "Copy Text" reaches the
//! clipboard. The rows' action flags are the core's; the timeline's popups and the
//! composer are the app's own QML. Offscreen, `harness = false`.

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

// The window is the root: one declared inside an Item is shown only once that Item has a
// window of its own, and the timeline needs a shown window to lay its rows out.
Window {
    id: test

    width: 520
    height: 700
    visible: true

    property string step: "start"
    property real target: 0
    property string original: ""
    property real photo: 0
    property real reply: 0
    property real jumpedTo: 0
    property int jumpRow: -1
    property int waits: 0
    property int editsStarted: 0
    property int editsFinished: 0

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function row(id: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.messageId === id) {
                return r;
            }
        }
        return null;
    }

    // The text (or caption) of a row.
    function textOf(r: var): string {
        for (const b of r.blocks) {
            if (b.type === "text") {
                return b.plain;
            }
        }
        return "";
    }

    function quoteOf(r: var): var {
        for (const b of r.blocks) {
            if (b.type === "quote") {
                return b;
            }
        }
        return null;
    }

    // Our own row with this text.
    function ownWith(text: string): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.own && test.textOf(r) === text) {
                return r;
            }
        }
        return null;
    }

    function hasKind(kind: string): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === kind) {
                return true;
            }
        }
        return false;
    }

    function systemRow(part: string): bool {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "system" && r.text.indexOf(part) >= 0) {
                return true;
            }
        }
        return false;
    }

    // The composer's files, as its list shows them.
    Instantiator {
        id: fileRows
        model: App.composer.files
        delegate: QtObject {
            required property string name
            required property string reason
        }
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property bool own
            required property var blocks
            required property var reactions
            required property bool edited
            required property bool deleted
            required property var actions
            required property string text
            required property string expiry
        }
    }

    Connections {
        target: App.timeline
        function onJumpRequested(message: real, row: int): void {
            test.jumpedTo = message;
            test.jumpRow = row;
        }
    }
    Connections {
        target: App.composer
        function onEditStarted(text: string): void {
            test.editsStarted += 1;
        }
        function onEditFinished(): void {
            test.editsFinished += 1;
        }
    }

    // The app's own timeline and composer, as the chat page holds them.
    TimelineView {
        id: view
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        anchors.bottom: composerItem.top
    }
    Composer {
        id: composerItem
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.bottom: parent.bottom
    }

    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                if (!App.loaded || !App.hasAccounts) return;
                App.ready();
                App.openConversation(QUOTES);
                test.step = "open";
                return;
            case "open": {
                if (App.timeline.loading || rows.count < 6) return;
                test.target = App.findMessage("editable");
                if (test.target <= 0) return fail("no own message that can be edited");
                const r = test.row(test.target);
                if (!r.own || !r.actions.edit || !r.actions.deleteForEveryone || !r.actions.react || !r.actions.reply || !r.actions.copy) return fail("the core's flags of a recent own text");
                test.original = test.textOf(r);
                if (test.original === "") return fail("its text");
                // Up in an empty composer edits the newest own message.
                if (!App.composer.editLast(QUOTES)) return fail("editLast");
                if (App.composer.editId !== test.target || App.composer.editCaption) return fail("the newest own message is edited");
                if (!composerItem.editing || composerItem.text !== test.original) return fail("the composer holds its text: " + composerItem.text);
                // Escape leaves edit mode.
                if (!composerItem.cancel() || composerItem.editing || composerItem.text !== "") return fail("Escape leaves edit mode");
                if (composerItem.cancel()) return fail("nothing left to cancel");
                // What was typed before an edit comes back after it.
                composerItem.setText("typed before the edit");
                if (!App.composer.startEdit(QUOTES, test.target)) return fail("startEdit");
                if (composerItem.text !== test.original) return fail("edit mode replaces the text");
                // An empty text cannot replace a message.
                composerItem.setText("  ");
                composerItem.send();
                if (!composerItem.editing) return fail("an empty edit was taken");
                composerItem.setText(test.original + " (edited)");
                composerItem.send();
                if (composerItem.editing || App.composer.editId !== 0) return fail("the edit is over once sent");
                if (composerItem.text !== "typed before the edit") return fail("the draft came back: " + composerItem.text);
                if (test.editsStarted !== 2 || test.editsFinished !== 2) return fail("edit signals " + test.editsStarted + "/" + test.editsFinished);
                composerItem.setText("");
                test.step = "edited";
                return;
            }
            case "edited": {
                const r = test.row(test.target);
                if (!r || test.textOf(r) !== test.original + " (edited)" || !r.edited) return;
                // The context menu of that row opens in the timeline.
                // (The window lays its rows out a moment after it is shown.)
                if (!view.menuFor(test.target)) {
                    test.waits += 1;
                    if (test.waits > 150) return fail("the row has no delegate in the timeline");
                    return;
                }
                if (!view.popupOpen) return fail("the context menu did not open");
                if (TimelineActions.activeMessage !== test.target) return fail("the row knows its menu is open");
                App.react(test.target, "👍", false);
                test.step = "reacted";
                return;
            }
            case "reacted": {
                const r = test.row(test.target);
                const chip = r.reactions.find(c => c.emoji === "👍");
                if (!chip) return;
                if (!chip.mine || chip.count !== 1 || chip.ownAt !== 0 || chip.names[0] !== "You") return fail("our reaction on the chip");
                if (!view.reactionsFor(test.target)) return fail("the list of who reacted");
                App.react(test.target, "👍", true);
                test.step = "unreacted";
                return;
            }
            case "unreacted": {
                const r = test.row(test.target);
                if (r.reactions.some(c => c.emoji === "👍")) return;
                // A reply to the photo: the banner shows what is quoted.
                test.photo = App.findMessage("photo");
                if (test.photo <= 0) return fail("no photo to reply to");
                // Its download (the core's policy fetched it) ends before the reply.
                if (!test.row(test.photo).blocks.some(b => b.type === "image" && b.state === "local")) return;
                if (App.composer.replyTo(QUOTES, 999999)) return fail("a reply to nothing");
                if (!App.composer.replyTo(QUOTES, test.photo)) return fail("replyTo");
                if (!composerItem.replying || App.composer.replyAuthor === "" || App.composer.replyText === "") return fail("the reply banner");
                if (App.composer.replyThumb.indexOf("image://kourier/thumb/") !== 0) return fail("the quoted photo's thumbnail: " + App.composer.replyThumb);
                composerItem.setText("About the photo");
                composerItem.send();
                if (composerItem.replying || composerItem.text !== "") return fail("the banner goes with the send");
                test.step = "replied";
                return;
            }
            case "replied": {
                const r = test.ownWith("About the photo");
                if (!r) return;
                const q = test.quoteOf(r);
                if (!q) return fail("the reply has no quote block");
                if (q.target !== test.photo || q.state !== "known" || q.standIn) return fail("the quote names its original");
                if (q.thumbUrl.indexOf("image://kourier/thumb/") !== 0) return fail("the quote's thumbnail");
                test.reply = r.messageId;
                // Activating the quote scrolls to the original and highlights it.
                App.jumpTo(test.photo);
                test.step = "jumped";
                return;
            }
            case "jumped":
                if (test.jumpedTo !== test.photo) return;
                if (test.jumpRow < 0) return fail("the row to scroll to");
                if (TimelineActions.flashMessage !== test.photo) return fail("the original is highlighted");
                // Files can be a reply too.
                if (!App.composer.replyTo(QUOTES, test.photo)) return fail("replyTo for files");
                if (!App.composer.addFile(SAMPLE)) return fail("the sample was not added: " + App.composer.notice);
                composerItem.setText("With a picture");
                composerItem.send();
                if (App.composer.attachmentCount !== 0 || composerItem.replying) return fail("strip and banner go with the send");
                test.step = "filereply";
                return;
            case "filereply": {
                const r = test.ownWith("With a picture");
                if (!r) return;
                const q = test.quoteOf(r);
                if (!q || q.target !== test.photo) return fail("files carry the quote");
                if (!r.blocks.some(b => b.type === "image")) return fail("the picture of the reply");
                // A send the core refuses (a file that is gone, sent before the core has
                // looked at it) comes back whole.
                if (!App.composer.replyTo(QUOTES, test.photo)) return fail("replyTo before the refused send");
                if (!App.composer.addFile(GONE)) return fail("the file was not added: " + App.composer.notice);
                composerItem.setText("Not lost");
                composerItem.send();
                if (App.composer.attachmentCount !== 0 || composerItem.text !== "") return fail("the composer empties when it sends");
                test.step = "refusedfiles";
                return;
            }
            case "refusedfiles":
                // The strip, the caption, the quote and a notice: nothing of it was queued.
                if (App.composer.attachmentCount !== 1 || App.composer.notice === "") return;
                if (fileRows.objectAt(0).name !== "Gone.pdf") return fail("the file is back: " + fileRows.objectAt(0).name);
                // It is marked with why it cannot go, and nothing of it can be sent.
                if (fileRows.objectAt(0).reason !== "Cannot be read") return fail("the reason: " + fileRows.objectAt(0).reason);
                if (App.composer.refusedCount !== 1 || App.composer.sendableCount !== 0) return fail("the counts: " + App.composer.refusedCount + "/" + App.composer.sendableCount);
                if (composerItem.text !== "Not lost") return fail("the caption is back: " + composerItem.text);
                if (!composerItem.replying || App.composer.replyToId !== test.photo) return fail("the quote is back");
                if (App.composer.notice.indexOf("cannot be sent here") < 0) return fail("the notice: " + App.composer.notice);
                if (test.ownWith("Not lost")) return fail("a refused send left a message");
                App.composer.removeAttachment(0);
                App.composer.cancelReply();
                App.composer.clearNotice();
                composerItem.setText("");
                // Delete for me: the row goes.
                if (!test.row(test.reply).actions.deleteForMe) return fail("delete for me is offered");
                App.deleteMessage(test.reply, false);
                test.step = "deletedForMe";
                return;
            case "deletedForMe":
                if (test.row(test.reply)) return;
                // Delete for everyone: a tombstone.
                App.deleteMessage(test.target, true);
                test.step = "deletedForEveryone";
                return;
            case "deletedForEveryone": {
                const r = test.row(test.target);
                if (!r || !r.deleted) return;
                const stand = r.blocks[0];
                if (r.blocks.length !== 1 || stand.type !== "placeholder" || stand.text !== "You deleted this message") return fail("the tombstone");
                if (r.actions.edit || r.actions.react || r.actions.reply || r.actions.copy || r.actions.deleteForEveryone || !r.actions.deleteForMe) return fail("a tombstone offers only its removal");
                if (App.composer.startEdit(QUOTES, test.target)) return fail("a tombstone cannot be edited");
                if (App.composer.notice !== "") return fail("no notice without a failure: " + App.composer.notice);
                App.copyText("copied from the test");
                App.openConversation(TIMER);
                test.step = "timer";
                return;
            }
            case "timer":
                if (App.timeline.conversationId !== TIMER || App.timeline.loading || rows.count < 4) return;
                if (App.timeline.timerOptions.length === 0) return;
                if (App.timeline.timerSecs !== 86400 || App.timeline.timerLabel !== "1 day") return fail("the header's timer mark: " + App.timeline.timerLabel);
                if (App.timeline.timerOptions[0].secs !== 0 || !App.timeline.timerOptions.some(o => o.secs === 3600)) return fail("the offered timers");
                if (!test.hasKind("unread")) return fail("the divider above the unread message");
                if (!test.systemRow("1 day")) return fail("the notice of the last timer change");
                App.setTimer(3600);
                test.step = "timerset";
                return;
            case "timerset":
                if (App.timeline.timerSecs !== 3600 || !test.systemRow("You set the disappearing message timer to 1 hour")) return;
                if (App.timeline.timerLabel !== "1 hour") return fail("the new mark: " + App.timeline.timerLabel);
                // Our answer: the divider goes, and the message carries the new timer.
                composerItem.setText("Fine with me");
                composerItem.send();
                test.step = "answered";
                return;
            case "answered": {
                const r = test.ownWith("Fine with me");
                if (!r || test.hasKind("unread")) return;
                if (r.expiry.indexOf("1 hour") < 0) return fail("the message's timer mark: " + r.expiry);
                // A time that is not in the menu is taken where the chat states a range of
                // custom timers (the fake network: one second to four weeks).
                App.setTimer(12345);
                test.step = "custom";
                return;
            }
            case "custom": {
                if (App.timeline.timerSecs !== 12345) return;
                if (App.timeline.timerLabel !== "3 hours 25 minutes 45 seconds") return fail("the custom timer's mark: " + App.timeline.timerLabel);
                if (App.composer.notice !== "") return fail("a notice without a failure: " + App.composer.notice);
                // A timer outside that range is refused, with a notice.
                App.setTimer(3024000);
                test.step = "refused";
                return;
            }
            case "refused":
                if (App.composer.notice === "") return;
                if (App.composer.notice !== "This timer is not available in this conversation.") return fail("the notice: " + App.composer.notice);
                if (App.timeline.timerSecs !== 12345) return fail("a refused timer changes nothing");
                Qt.exit(0);
                return;
            }
        }
    }

    Timer {
        interval: 60000
        running: true
        onTriggered: {
            let notices = "";
            for (let i = 0; i < rows.count; ++i) {
                const r = rows.objectAt(i);
                if (r && r.kind === "system") {
                    notices += " [" + r.text + "]";
                }
            }
            fail("timeout; timer " + App.timeline.timerSecs + ", notice '" + App.composer.notice + "', system rows:" + notices);
        }
    }
}
"#;

fn main() {
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture data");
    let quotes = fixture.quotes_conversation.expect("the quotes chat");
    let timer = fixture.timer_conversation.expect("the timer chat");
    assert!(
        fixture.kinds_conversation.is_some(),
        "the chat with stand-ins"
    );

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    // The picture a reply attaches: a PNG outside the profile (a user's file).
    let dir = std::env::temp_dir().join(format!("kourier-actions-ui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let sample_path = dir.join("Reply sample.png");
    let sample = kourier_qt::bridge::qobject::sample_png(240, 160, 200);
    std::fs::write(&sample_path, &sample).expect("write the sample");
    let sample_url = kourier_qt::app::file_url(&sample_path);

    // Something the composer takes like any file and no network can carry: a file that was
    // deleted after it was picked.
    let gone_url = kourier_qt::app::file_url(&dir.join("Gone.pdf"));

    let script = SCRIPT
        .replace("QUOTES", &quotes.0.to_string())
        .replace("TIMER", &timer.0.to_string())
        .replace("SAMPLE", &format!("{sample_url:?}"))
        .replace("GONE", &format!("{gone_url:?}"));
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/actions_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    // "Copy Text" put the text on the clipboard.
    let copied = kourier_qt::bridge::qobject::clipboard_text().to_string();
    drop(engine);
    app.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    if code == 0 {
        assert_eq!(copied, "copied from the test", "Copy Text");
        println!(
            "test message actions on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
    } else {
        println!(
            "test message actions on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
