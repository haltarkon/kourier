//! Media through the real core over the fake network (§8.6, §5.8): the image provider serves
//! thumbnails and placeholders from `core.db` and the encrypted blob store (a handle-table miss
//! resolves on a worker thread; on a runtime thread it degrades to the placeholder instead of
//! stalling), unknown ids fail cleanly, a picture attached in the composer reaches `SendFiles`
//! and comes back as an own message with a thumbnail, a pasted clipboard image lands in the
//! strip as a tmpfs file, a refused `OpenMedia` fails only its own request while a voice note's
//! copy opens (and goes with its view), Qt decodes only the formats the content shows (a GIF's
//! copy may be played; PostScript sent as a GIF gets the neutral tile and no player), and
//! nothing plaintext lands in the profile outside its runtime directory, and a provider job done
//! before Qt connected to its response still reaches its Image (`--race-child`, the provider's
//! probe `raceImageFinishes`). Offscreen, `harness = false`.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{Query, QueryData, TimelineAnchor, TimelineQuery};
use kourier_app::{App, AppHandle, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentId, AttachmentKind, AttachmentState};
use kourier_qt::media::{Codec, Image, resolve};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r#"
import QtQuick
import QtQml
import app.kourier

Item {
    id: test

    property string step: "start"
    property int ownImagesBefore: -1

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    // Own rows with a downloaded picture that has a provider URL.
    function ownImages(): var {
        const out = [];
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r || !r.own) {
                continue;
            }
            for (const b of r.blocks) {
                if (b.type === "image" && b.state === "local" && b.url.indexOf("image://kourier/thumb/") === 0) {
                    out.push(b);
                }
            }
        }
        return out;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property bool own
            required property var blocks
        }
    }

    // The composer's files, as its list shows them.
    Instantiator {
        id: fileRows
        model: App.composer.files
        delegate: QtObject {
            required property string name
            required property string kind
            required property string size
            required property string preview
            required property string reason
        }
    }

    Image {
        id: thumb
        asynchronous: true
        sourceSize.width: 64
        sourceSize.height: 48
    }
    Image {
        id: blur
        asynchronous: true
        sourceSize.width: 16
        sourceSize.height: 12
    }
    Image {
        id: missing
        asynchronous: true
    }
    // The "thumbnail" of PostScript that was sent as a GIF.
    Image {
        id: disguised
        asynchronous: true
        sourceSize.width: 64
        sourceSize.height: 64
    }

    // The own animated picture (a GIF by its type) that is not attachment `except`.
    function ownAnimated(except: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r || !r.own) {
                continue;
            }
            for (const b of r.blocks) {
                if (b.type === "image" && b.animated && b.state === "local" && b.attachment !== except) {
                    return b;
                }
            }
        }
        return null;
    }
    property real gifAttachment: 0

    // The timeline's own block for a picture, in a window (it sizes itself by the screen):
    // its MediaCopy asks for the copy and its AnimatedImage plays what it may.
    property var shownBlock: null
    Window {
        id: stage
        width: 400
        height: 400
        visible: true
        Loader {
            id: blockView
            active: false
            sourceComponent: ImageBlock {
                block: test.shownBlock
            }
        }
    }

    // An open the core refuses (an unknown attachment): only its own request is told.
    property real refused: 0
    property int failures: 0
    property real failedToken: 0
    // Copies that arrived, and how many of them AnimatedImage may play.
    property int copies: 0
    property int movies: 0
    Connections {
        target: App
        function onMediaFailed(token: real): void {
            test.failures += 1;
            test.failedToken = token;
        }
        function onMediaOpened(token: real, url: string, movie: bool): void {
            test.copies += 1;
            test.movies += movie ? 1 : 0;
        }
    }

    // A voice note's tmpfs copy for Qt Multimedia (`OpenMedia` → `MediaOpened` → `CloseMedia`).
    Loader {
        id: voiceLoader
        active: false
        sourceComponent: VoicePlayer {
            attachment: VOICE
        }
    }

    Timer {
        id: poll
        interval: 50
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                thumb.source = "image://kourier/thumb/" + IMAGE + "?v=1";
                blur.source = "image://kourier/blurhash/" + IMAGE + "?v=1";
                missing.source = "image://kourier/thumb/999999?v=1";
                test.step = "provider";
                break;
            case "provider":
                if (thumb.status === Image.Error) return fail("thumbnail failed");
                if (blur.status === Image.Error) return fail("blurhash failed");
                if (thumb.status !== Image.Ready || blur.status !== Image.Ready || missing.status === Image.Loading || missing.status === Image.Null) return;
                if (missing.status !== Image.Error) return fail("an unknown id did not fail: " + missing.status);
                if (thumb.implicitWidth < 60 || thumb.implicitWidth > 70 || thumb.implicitHeight < 44) return fail("thumbnail not decoded at the requested size: " + thumb.implicitWidth + "x" + thumb.implicitHeight);
                if (blur.implicitWidth !== 16 || blur.implicitHeight !== 12) return fail("blurhash size " + blur.implicitWidth + "x" + blur.implicitHeight);
                App.openConversation(CONVERSATION);
                test.step = "open";
                break;
            case "open":
                if (App.timeline.attachKinds.indexOf("image") < 0 || rows.count < 5) return;
                test.ownImagesBefore = test.ownImages().length;
                // A link to the web is no file on this computer.
                if (App.composer.addFile("https://example.org/notes.pdf")) return fail("a web link was taken as a file");
                if (App.composer.notice === "") return fail("no notice for what is no local file");
                App.composer.clearNotice();
                if (!App.composer.addFile(SAMPLE)) return fail("the sample was not added");
                if (App.composer.attachmentCount !== 1) return fail("strip count " + App.composer.attachmentCount);
                // Nothing of the file is decoded before the core read what it is.
                if (fileRows.objectAt(0).preview !== "") return fail("a preview before the content was read");
                test.step = "looked";
                break;
            case "looked":
                if (!App.composer.planned || fileRows.objectAt(0).size === "") return;
                if (fileRows.objectAt(0).preview.indexOf("file://") !== 0) return fail("no preview url");
                if (fileRows.objectAt(0).kind !== "image" || fileRows.objectAt(0).reason !== "") return fail("the sample as the core sees it: " + fileRows.objectAt(0).kind);
                if (!App.composer.send(CONVERSATION, "From the test")) return fail("send refused: " + App.composer.error);
                if (App.composer.attachmentCount !== 0) return fail("strip not cleared");
                if (!App.composer.paste()) return fail("the clipboard image was not pasted");
                test.step = "pasted";
                break;
            case "pasted":
                if (App.composer.attachmentCount === 0 || !App.composer.planned || fileRows.objectAt(0).size === "") return;
                if (fileRows.objectAt(0).name !== "Pasted image.png") return fail("pasted name " + fileRows.objectAt(0).name);
                if (fileRows.objectAt(0).preview.indexOf("file://") !== 0) return fail("no preview of the paste");
                App.composer.removeAttachment(0);
                if (App.composer.attachmentCount !== 0) return fail("paste not removed");
                test.step = "sent";
                break;
            case "sent": {
                const images = test.ownImages();
                if (images.length <= test.ownImagesBefore) return;
                thumb.source = images[0].url;
                test.step = "own";
                break;
            }
            case "own":
                if (thumb.status === Image.Error) return fail("own thumbnail failed");
                if (thumb.status !== Image.Ready) return;
                // A refused open right before the voice note's: the core answers in order, so
                // the failure is the refused request's alone.
                test.refused = App.openMedia(999999);
                if (test.refused === 0) return fail("the open was not asked for");
                voiceLoader.active = true;
                test.step = "voice";
                break;
            case "voice": {
                const player = voiceLoader.item;
                if (player && player.failed) return fail("another request's failure failed the voice note");
                if (!player || player.url === "") return;
                if (player.url.indexOf("file://") !== 0 || player.url.indexOf("/media/attachments/") < 0) return fail("voice copy " + player.url);
                if (test.failures !== 1 || test.failedToken !== test.refused) return fail("the refused open was not reported once to its own request: " + test.failures);
                if (test.copies !== 1 || test.movies !== 0) return fail("a voice note is no movie for AnimatedImage: " + test.copies + "/" + test.movies);
                // The copy goes with its view.
                voiceLoader.active = false;
                // Asking again is answered again (nothing stale is left waiting).
                test.refused = App.openMedia(999999);
                test.step = "again";
                break;
            }
            case "again":
                if (test.failures < 2) return;
                if (test.failures !== 2 || test.failedToken !== test.refused) return fail("the second refused open: " + test.failures);
                // A GIF of our own: AnimatedImage may play its copy.
                if (!App.composer.addFile(GIFFILE)) return fail("the GIF was not added");
                test.step = "gifLooked";
                break;
            case "gifLooked":
                // Send waits for the core's look at the file (its location verdict).
                if (!App.composer.planned) return;
                if (!App.composer.send(CONVERSATION, "")) return fail("the GIF was not sent: " + App.composer.error);
                test.step = "gif";
                break;
            case "gif": {
                const b = test.ownAnimated(0);
                if (!b) return;
                test.gifAttachment = b.attachment;
                test.copies = 0;
                test.movies = 0;
                test.shownBlock = b;
                blockView.active = true;
                test.step = "gifcopy";
                break;
            }
            case "gifcopy":
                if (test.copies < 1 || !blockView.item) return;
                if (test.movies !== 1) return fail("a GIF's copy was not offered to AnimatedImage");
                // The block plays it.
                if (!blockView.item.playing) return;
                blockView.active = false;
                // PostScript that calls itself a GIF (the type is the sender's claim; Qt's EPS
                // plugin would run it through Ghostscript): neither decoder gets it.
                if (!App.composer.addFile(PSFILE)) return fail("the disguised file was not added");
                test.step = "disguisedLooked";
                break;
            case "disguisedLooked":
                // The composer's own preview: by its name (and so for the core) the file is a
                // GIF, by its first bytes it is not a picture, and Qt's `Image` never gets it.
                if (!App.composer.planned || fileRows.objectAt(0).size === "") return;
                if (fileRows.objectAt(0).kind !== "gif") return fail("the core takes the name's word: " + fileRows.objectAt(0).kind);
                if (fileRows.objectAt(0).preview !== "") return fail("PostScript named .gif was offered to the composer's Image");
                if (!App.composer.send(CONVERSATION, "")) return fail("the disguised file was not sent: " + App.composer.error);
                test.step = "disguised";
                break;
            case "disguised": {
                const b = test.ownAnimated(test.gifAttachment);
                if (!b) return;
                if (b.url.indexOf("image://kourier/thumb/") !== 0) return fail("no thumbnail url: " + b.url);
                disguised.source = b.url;
                test.copies = 0;
                test.movies = 0;
                test.shownBlock = b;
                blockView.active = true;
                test.step = "disguisedcopy";
                break;
            }
            case "disguisedcopy":
                if (disguised.status === Image.Error) return fail("no stand-in for the disguised file");
                if (test.copies < 1 || disguised.status !== Image.Ready || !blockView.item) return;
                if (test.movies !== 0) return fail("PostScript named .gif was offered to AnimatedImage");
                // The provider answered with its neutral 8 × 8 tile, not a rendering.
                if (disguised.implicitWidth !== 8 || disguised.implicitHeight !== 8) return fail("PostScript named .gif was decoded: " + disguised.implicitWidth + "x" + disguised.implicitHeight);
                // The block shows that tile as its still and plays nothing.
                if (!blockView.item.shown) return;
                if (blockView.item.playing) return fail("the block plays PostScript named .gif");
                blockView.active = false;
                stage.close();
                test.step = "external";
                break;
            case "external":
                // "Open in Another Application" and every link go through the app, which starts
                // the program with the environment the person gave Kourier (the test replaced the
                // launcher by a recorder).
                if (!App.openExternalUrl("https://example.org/seen-by-the-opener")) return fail("openExternalUrl was not taken");
                // Kirigami's About page and link delegates call this themselves: Qt hands the URL
                // to the app's handler of its scheme (the launcher is a recorder here).
                if (!Qt.openUrlExternally("https://example.org/via-qt?q=\u00e9")) return fail("an https URL was not taken");
                if (!Qt.openUrlExternally("http://example.org/plain-http")) return fail("an http URL was not taken");
                if (!Qt.openUrlExternally("mailto:someone@example.org")) return fail("a mailto URL was not taken");
                if (!Qt.openUrlExternally("file:///tmp/kourier-opener-file")) return fail("a file URL was not taken");
                test.step = "closed";
                break;
            case "closed":
                Qt.exit(0);
                break;
            }
        }
    }

    Timer {
        interval: 60000
        running: true
        onTriggered: fail("timeout")
    }
}
"#;

/// Runs `f` until it returns `Some`, for up to `secs` seconds.
fn wait_for<T>(secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The downloaded pictures and the voice note of the fixture, without touching the media
/// handle table (`Query::Attachment` runs on the media read connection and registers nothing).
///
/// The first picture is the one the script asks the provider about by thumbnail and by
/// blurhash, so it must have both. The core derives them after the download, in one update:
/// a picture is `Local` a moment before it has them, and the provider answers a blurhash
/// request with the neutral 8 px tile until then (on a busy machine the test once started in
/// that moment and failed with "blurhash size 8x8").
#[allow(clippy::disallowed_methods)] // The test's main thread before Qt starts, not the Qt thread.
fn downloaded_media(app: &AppHandle) -> (Vec<AttachmentId>, AttachmentId) {
    // The fixture's chat of imported attachments (`--page=unavailable`) keeps a picture that
    // waits for a click: history never downloads by itself, so it is not "on its way".
    let imported: std::collections::HashSet<_> = app
        .fixture()
        .and_then(|f| f.unavailable_conversation)
        .map(|conv| {
            let query = Query::Timeline(TimelineQuery {
                conv,
                anchor: TimelineAnchor::Newest,
                limit: 50,
            });
            match app.runtime().block_on(app.query(query)) {
                Ok(QueryData::Timeline(page)) => page.rows.iter().map(|r| r.id).collect(),
                _ => Default::default(),
            }
        })
        .unwrap_or_default();
    wait_for(30, || {
        let mut images = Vec::new();
        let mut voice = None;
        let mut pending = false;
        let mut first_derived = false;
        // (The fixture's showcase chats bring a few dozen pictures of their own.)
        for n in 1..=60 {
            let id = AttachmentId(n);
            let answer = app.runtime().block_on(app.query(Query::Attachment(id)));
            if let Ok(QueryData::Attachment(Some(a))) = answer {
                let image = a.mime.as_deref().is_some_and(|m| m.starts_with("image/"));
                let audio = a.mime.as_deref().is_some_and(|m| m.starts_with("audio/"));
                match a.state {
                    AttachmentState::Local if image => {
                        if images.is_empty() {
                            first_derived = a.thumb.is_some();
                        }
                        images.push(id);
                    }
                    AttachmentState::Local if audio => voice = Some(id),
                    AttachmentState::Remote | AttachmentState::Downloading { .. }
                        if !imported.contains(&a.message) =>
                    {
                        pending = true;
                    }
                    _ => {}
                }
            }
        }
        match voice {
            Some(v) if !pending && first_derived && images.len() >= 2 => Some((images, v)),
            _ => None,
        }
    })
}

/// Files below `dir` (recursively).
fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.is_dir() {
            files(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// No file of the profile outside its runtime directory holds a plaintext picture (§5.8).
fn assert_no_plaintext(app: &App, sample: &[u8]) {
    let paths = app.paths();
    let mut all = Vec::new();
    for dir in [&paths.data, &paths.config, &paths.state] {
        files(dir, &mut all);
    }
    assert!(all.len() > 3, "the profile has files");
    for f in all {
        let Ok(bytes) = std::fs::read(&f) else {
            continue;
        };
        let magic = bytes.starts_with(b"\x89PNG") || bytes.starts_with(&[0xff, 0xd8, 0xff]);
        let needle = &sample[..sample.len().min(64)];
        let copy = bytes.windows(needle.len()).any(|w| w == needle);
        assert!(!magic && !copy, "plaintext media outside the runtime dir");
    }
}

/// A voice note as a phone sends one: AAC in an MP4 container, 1 s of a 440 Hz tone, mono, 8 kHz
/// (made with ffmpeg's native encoder: `-c:a aac -b:a 6k -movflags +faststart`; 2050 bytes).
const VOICE_AAC: &[u8] = &[
    0x00, 0x00, 0x00, 0x1c, 0x66, 0x74, 0x79, 0x70, 0x4d, 0x34, 0x41, 0x20, 0x00, 0x00, 0x02, 0x00,
    0x4d, 0x34, 0x41, 0x20, 0x69, 0x73, 0x6f, 0x6d, 0x69, 0x73, 0x6f, 0x32, 0x00, 0x00, 0x02, 0xfa,
    0x6d, 0x6f, 0x6f, 0x76, 0x00, 0x00, 0x00, 0x6c, 0x6d, 0x76, 0x68, 0x64, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xe8, 0x00, 0x00, 0x03, 0xe8,
    0x00, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02,
    0x00, 0x00, 0x02, 0x49, 0x74, 0x72, 0x61, 0x6b, 0x00, 0x00, 0x00, 0x5c, 0x74, 0x6b, 0x68, 0x64,
    0x00, 0x00, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xe8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x24, 0x65, 0x64, 0x74, 0x73, 0x00, 0x00, 0x00, 0x1c,
    0x65, 0x6c, 0x73, 0x74, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x03, 0xe8,
    0x00, 0x00, 0x04, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0xc1, 0x6d, 0x64, 0x69, 0x61,
    0x00, 0x00, 0x00, 0x20, 0x6d, 0x64, 0x68, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1f, 0x40, 0x00, 0x00, 0x23, 0x40, 0x55, 0xc4, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x2d, 0x68, 0x64, 0x6c, 0x72, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x73, 0x6f, 0x75, 0x6e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x53, 0x6f, 0x75, 0x6e, 0x64, 0x48, 0x61, 0x6e, 0x64, 0x6c, 0x65, 0x72, 0x00, 0x00, 0x00, 0x01,
    0x6c, 0x6d, 0x69, 0x6e, 0x66, 0x00, 0x00, 0x00, 0x10, 0x73, 0x6d, 0x68, 0x64, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x24, 0x64, 0x69, 0x6e, 0x66, 0x00, 0x00, 0x00,
    0x1c, 0x64, 0x72, 0x65, 0x66, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x0c, 0x75, 0x72, 0x6c, 0x20, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x30, 0x73, 0x74, 0x62,
    0x6c, 0x00, 0x00, 0x00, 0x6a, 0x73, 0x74, 0x73, 0x64, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x5a, 0x6d, 0x70, 0x34, 0x61, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x10, 0x00, 0x00, 0x00,
    0x00, 0x1f, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x65, 0x73, 0x64, 0x73, 0x00, 0x00, 0x00,
    0x00, 0x03, 0x80, 0x80, 0x80, 0x25, 0x00, 0x01, 0x00, 0x04, 0x80, 0x80, 0x80, 0x17, 0x40, 0x15,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x22, 0x76, 0x00, 0x00, 0x22, 0x76, 0x05, 0x80, 0x80, 0x80, 0x05,
    0x15, 0x88, 0x56, 0xe5, 0x00, 0x06, 0x80, 0x80, 0x80, 0x01, 0x02, 0x00, 0x00, 0x00, 0x20, 0x73,
    0x74, 0x74, 0x73, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x08, 0x00,
    0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x03, 0x40, 0x00, 0x00, 0x00, 0x1c, 0x73,
    0x74, 0x73, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x38, 0x73, 0x74, 0x73, 0x7a, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0xc3, 0x00,
    0x00, 0x00, 0xe0, 0x00, 0x00, 0x00, 0x78, 0x00, 0x00, 0x00, 0x5e, 0x00, 0x00, 0x00, 0x5f, 0x00,
    0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x5d, 0x00, 0x00, 0x00, 0xa1, 0x00, 0x00, 0x00, 0xa6, 0x00,
    0x00, 0x00, 0x14, 0x73, 0x74, 0x63, 0x6f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
    0x00, 0x03, 0x26, 0x00, 0x00, 0x00, 0x1a, 0x73, 0x67, 0x70, 0x64, 0x01, 0x00, 0x00, 0x00, 0x72,
    0x6f, 0x6c, 0x6c, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0xff, 0xff, 0x00, 0x00, 0x00,
    0x1c, 0x73, 0x62, 0x67, 0x70, 0x00, 0x00, 0x00, 0x00, 0x72, 0x6f, 0x6c, 0x6c, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x09, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x3d, 0x75, 0x64, 0x74,
    0x61, 0x00, 0x00, 0x00, 0x35, 0x6d, 0x65, 0x74, 0x61, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x21, 0x68, 0x64, 0x6c, 0x72, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x6d, 0x64, 0x69,
    0x72, 0x61, 0x70, 0x70, 0x6c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x08, 0x69, 0x6c, 0x73, 0x74, 0x00, 0x00, 0x00, 0x08, 0x66, 0x72, 0x65, 0x65, 0x00, 0x00,
    0x04, 0xe4, 0x6d, 0x64, 0x61, 0x74, 0x01, 0x1e, 0x52, 0xa8, 0x2c, 0x8a, 0x0a, 0x6d, 0xfd, 0x0a,
    0xaa, 0xde, 0xbb, 0x9d, 0x2e, 0xee, 0x49, 0x12, 0x24, 0x48, 0x89, 0x12, 0x0c, 0xab, 0x13, 0x95,
    0x62, 0x71, 0x59, 0x4b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3a,
    0x3b, 0x3b, 0x3b, 0x3b, 0x3b, 0x3a, 0x3a, 0x3a, 0x3b, 0x3c, 0xd2, 0x63, 0xa9, 0x36, 0x8b, 0xca,
    0xef, 0x6e, 0xdc, 0xff, 0x6e, 0xbd, 0x70, 0x64, 0x48, 0xa5, 0x96, 0x53, 0x72, 0xcb, 0x2c, 0xb2,
    0xcb, 0x29, 0xb9, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65,
    0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96,
    0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59,
    0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65,
    0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96,
    0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59,
    0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x65,
    0x96, 0x59, 0x65, 0x96, 0x59, 0x65, 0x96, 0x59, 0x78, 0x01, 0x00, 0x9c, 0xd9, 0x3d, 0x15, 0x44,
    0xba, 0x19, 0x44, 0xbb, 0x17, 0x64, 0xbe, 0x7d, 0xbe, 0x7f, 0xfe, 0x2f, 0xfc, 0xff, 0x5e, 0xb8,
    0xe3, 0x5a, 0xb9, 0x33, 0xfd, 0x7f, 0x4f, 0xff, 0x0f, 0xfc, 0xff, 0x1e, 0x78, 0xf3, 0x7a, 0x97,
    0x5d, 0xff, 0x7f, 0xcf, 0xfe, 0x9f, 0xfc, 0xfd, 0x75, 0xc7, 0x17, 0xd5, 0x6a, 0x4f, 0xfd, 0x3f,
    0x3f, 0xfa, 0x7f, 0xf7, 0xfa, 0xeb, 0x5c, 0x71, 0xa5, 0xd2, 0x65, 0xd1, 0x8a, 0x3c, 0x0c, 0xba,
    0x31, 0x27, 0x1c, 0xba, 0x31, 0x47, 0x86, 0x99, 0x9b, 0x0c, 0x57, 0x34, 0xd3, 0x2e, 0x65, 0xcc,
    0xf3, 0x4d, 0x32, 0x59, 0xa3, 0x65, 0x97, 0xe4, 0x2e, 0xaa, 0x06, 0xd1, 0x2d, 0x05, 0x18, 0x4e,
    0xb3, 0x9d, 0x15, 0x54, 0x63, 0x1a, 0x83, 0x47, 0x45, 0x11, 0xd0, 0x6e, 0x81, 0xd4, 0x28, 0x37,
    0x41, 0xba, 0xa7, 0xa2, 0x8a, 0xc2, 0x95, 0xb3, 0x64, 0x5b, 0xdb, 0x36, 0x6c, 0x23, 0xbe, 0x91,
    0x70, 0x8d, 0xa2, 0x5c, 0x5f, 0x64, 0xf6, 0x11, 0xd9, 0xb3, 0x64, 0x57, 0xe4, 0x31, 0xbe, 0x6b,
    0x45, 0x23, 0x5e, 0x3c, 0xf3, 0x9c, 0xe7, 0x52, 0x95, 0x1a, 0x94, 0xa9, 0xe7, 0x9e, 0x79, 0xe7,
    0x54, 0xea, 0x9d, 0x53, 0xaa, 0x79, 0xe7, 0x9d, 0x42, 0x75, 0x29, 0x53, 0xcf, 0x3c, 0xfb, 0x27,
    0x51, 0xbe, 0x8d, 0xf4, 0x51, 0x4b, 0xf3, 0xa9, 0x7a, 0xd2, 0x87, 0x63, 0x9f, 0x1b, 0x5d, 0x83,
    0x7c, 0x4e, 0xe1, 0xf8, 0x29, 0xce, 0xa8, 0x79, 0xb9, 0xa7, 0x54, 0xf3, 0xf3, 0x4f, 0xcd, 0xf2,
    0x54, 0xe4, 0x48, 0x41, 0xd1, 0xf2, 0xd8, 0xa1, 0xc0, 0x01, 0x12, 0xf2, 0x8a, 0xe2, 0xe6, 0x65,
    0x67, 0x3c, 0xf7, 0xf9, 0xff, 0xa7, 0xff, 0x6e, 0xb5, 0xc7, 0x17, 0x77, 0x72, 0x5c, 0xce, 0xaa,
    0xe5, 0xf3, 0x9a, 0x52, 0x50, 0x1e, 0xe4, 0x1f, 0xf8, 0x03, 0x3e, 0x80, 0x7f, 0x8c, 0xc0, 0x7b,
    0xe8, 0x07, 0xf8, 0x01, 0xd0, 0x9f, 0x70, 0x43, 0xdf, 0xdf, 0x44, 0x20, 0x7c, 0x00, 0xef, 0xb8,
    0x0f, 0xfc, 0x18, 0x3b, 0xee, 0x40, 0xff, 0x06, 0x09, 0xf7, 0x20, 0x7f, 0x86, 0xc0, 0xf7, 0x20,
    0x33, 0x36, 0x07, 0xbe, 0x80, 0x3e, 0x1b, 0x03, 0xdc, 0x89, 0x92, 0xfe, 0x1e, 0x23, 0xf8, 0xc7,
    0x10, 0x3f, 0x8e, 0x12, 0x5e, 0x23, 0xc9, 0xf6, 0x3f, 0x87, 0x89, 0x1f, 0xe3, 0xe1, 0x01, 0x8c,
    0x01, 0x20, 0xa0, 0x20, 0x11, 0x08, 0x80, 0x01, 0x30, 0xa8, 0x58, 0x36, 0xc4, 0x02, 0x53, 0x01,
    0xc0, 0x01, 0x02, 0x32, 0x87, 0xe6, 0x7f, 0x8f, 0xeb, 0xfa, 0xe7, 0xdb, 0x5e, 0x3f, 0x3f, 0xff,
    0xcb, 0xff, 0xf6, 0xdf, 0x17, 0x9c, 0x73, 0x7f, 0x1f, 0x3c, 0xf1, 0xf5, 0xfb, 0x7e, 0xff, 0xd3,
    0xea, 0x3c, 0x23, 0x35, 0xcd, 0x02, 0xbb, 0xf7, 0xef, 0x51, 0x4d, 0xfb, 0xdb, 0xf7, 0x92, 0x53,
    0x7a, 0x9b, 0xca, 0x53, 0x42, 0x9b, 0xf7, 0xef, 0xbb, 0x7e, 0xfb, 0xef, 0xde, 0x58, 0x2a, 0x19,
    0xc2, 0x57, 0x14, 0x04, 0x02, 0xe1, 0xa8, 0x2c, 0x15, 0x0b, 0x05, 0x00, 0x08, 0x84, 0x02, 0xe1,
    0x30, 0x05, 0x42, 0x40, 0x04, 0x42, 0x00, 0x00, 0x4c, 0x00, 0x0a, 0x00, 0x10, 0x01, 0xc0, 0x00,
    0xfc, 0x32, 0x84, 0x20, 0x11, 0xf9, 0x1b, 0xff, 0x1b, 0xf7, 0x78, 0xfb, 0x7f, 0xfe, 0x8f, 0xff,
    0xeb, 0x7a, 0xe3, 0x74, 0xfb, 0xe7, 0x8c, 0x9c, 0x7c, 0xfe, 0xbf, 0xc7, 0xd7, 0x0f, 0x5d, 0xf5,
    0x52, 0x80, 0x69, 0x58, 0xf4, 0xff, 0x6a, 0x7a, 0x7f, 0xb1, 0x28, 0xbd, 0x27, 0xa4, 0x93, 0x0a,
    0x5f, 0xdb, 0xc1, 0xfd, 0x8c, 0xc9, 0xff, 0x6f, 0xed, 0x99, 0x60, 0xa8, 0x6d, 0x0b, 0x44, 0x50,
    0x10, 0x09, 0x84, 0xc2, 0xc1, 0x50, 0xa8, 0x48, 0x24, 0x12, 0x88, 0x44, 0x01, 0x30, 0x05, 0x82,
    0x81, 0x20, 0x02, 0xe0, 0x09, 0x84, 0xc2, 0xc1, 0x60, 0x90, 0x22, 0x29, 0x70, 0x1c, 0x00, 0xfe,
    0x32, 0x87, 0xe6, 0x7e, 0xff, 0xd3, 0xf7, 0xcf, 0x97, 0x3d, 0xff, 0xfe, 0x1f, 0xff, 0x96, 0xf5,
    0xae, 0xf4, 0x9f, 0x7f, 0x9e, 0xf8, 0xf8, 0xfd, 0xbf, 0xdf, 0xfe, 0x3e, 0x34, 0xf1, 0x56, 0xdd,
    0xf3, 0x40, 0xbd, 0xb6, 0x6c, 0x55, 0x6d, 0x9b, 0x3a, 0x5b, 0x61, 0x48, 0x86, 0xc6, 0xce, 0x88,
    0x37, 0x6c, 0xd9, 0xb3, 0x63, 0x64, 0x76, 0x6c, 0xd8, 0x58, 0x2c, 0x19, 0xc2, 0x9b, 0x36, 0x6c,
    0xd9, 0x40, 0x44, 0x2e, 0x1b, 0x02, 0xc1, 0xe5, 0x0b, 0x00, 0x24, 0x12, 0x04, 0xc0, 0x16, 0x00,
    0x67, 0x0a, 0x80, 0x24, 0x08, 0x80, 0x01, 0x30, 0xa8, 0x02, 0x40, 0x04, 0x40, 0x70, 0x00, 0xfe,
    0x32, 0x87, 0xe6, 0x7e, 0x3f, 0x4f, 0xc6, 0x7f, 0x1e, 0x7b, 0xfc, 0xff, 0xff, 0x2f, 0xff, 0xdb,
    0x7a, 0xd5, 0x6b, 0xbf, 0x6a, 0xe7, 0x35, 0xaf, 0x9f, 0xdf, 0xf4, 0xf3, 0x2f, 0xc6, 0x6a, 0xb7,
    0xa5, 0x02, 0xd0, 0x14, 0x53, 0x7e, 0xfa, 0x6f, 0xde, 0x50, 0xde, 0x6f, 0x29, 0x4b, 0xa9, 0xbf,
    0x7e, 0xfd, 0xed, 0xf7, 0xbe, 0xfd, 0xf7, 0x58, 0x2c, 0x16, 0x0c, 0x50, 0x14, 0x04, 0x42, 0x61,
    0x70, 0xb0, 0x58, 0x33, 0x85, 0x00, 0x08, 0x05, 0xc2, 0xe1, 0x70, 0x05, 0x02, 0xc0, 0x09, 0x02,
    0x00, 0x0b, 0x84, 0xc0, 0x16, 0x0b, 0x44, 0x22, 0x25, 0x30, 0x1c, 0x01, 0x26, 0x32, 0x8b, 0xe5,
    0x10, 0x8a, 0xa6, 0x57, 0x7f, 0xff, 0x67, 0xff, 0xcf, 0x8b, 0xbb, 0xb9, 0x24, 0x92, 0x44, 0x89,
    0x11, 0x13, 0x63, 0x15, 0xab, 0x1e, 0x94, 0x32, 0xbe, 0xe5, 0xab, 0x13, 0xa1, 0x95, 0xf7, 0x5e,
    0xac, 0x70, 0xa9, 0x95, 0x8b, 0xfa, 0x81, 0x85, 0x57, 0x0a, 0x98, 0x6e, 0xe5, 0xab, 0x1c, 0x10,
    0xca, 0xe4, 0x5a, 0xb1, 0xe9, 0xa9, 0x95, 0xc8, 0x15, 0x8a, 0xd0, 0xca, 0xd1, 0x7a, 0xb1, 0x5d,
    0x4c, 0xae, 0x4b, 0xd5, 0x8e, 0x15, 0x5d, 0x5a, 0x2f, 0x8f, 0x15, 0xd5, 0xa6, 0xb1, 0x7c, 0x78,
    0xad, 0x57, 0x56, 0x8b, 0xfa, 0xb1, 0x5a, 0xae, 0xad, 0x17, 0xf5, 0x62, 0xb5, 0x69, 0xad, 0x0c,
    0x56, 0x2b, 0x56, 0x9a, 0xd0, 0xca, 0xfb, 0xaf, 0x56, 0x9a, 0xd0, 0x3a, 0xb1, 0x5a, 0xb4, 0xd6,
    0x86, 0x57, 0x8a, 0xd5, 0x8f, 0x4a, 0x19, 0x5f, 0x72, 0xe7, 0xc7, 0x04, 0x30, 0xd8, 0xad, 0x58,
    0xf4, 0xd4, 0xca, 0xfb, 0x96, 0xac, 0x70, 0x43, 0x27, 0x91, 0x6a, 0xc4, 0x21, 0x95, 0xa1, 0x6a,
    0xc7, 0x0a, 0x99, 0x5c, 0x97, 0xa8, 0x97, 0x53, 0x2b, 0x02, 0x00, 0x38, 0x01, 0x42, 0x32, 0x8b,
    0xe6, 0x14, 0xaa, 0xaf, 0x5f, 0x6f, 0xe7, 0x89, 0xab, 0x92, 0xd2, 0x49, 0x22, 0x24, 0x48, 0x88,
    0x15, 0x04, 0xcb, 0x64, 0xdb, 0x90, 0xa8, 0x0e, 0xb6, 0x4d, 0xb9, 0x0a, 0x83, 0x2a, 0xd9, 0x36,
    0xe7, 0x02, 0x72, 0xbb, 0x08, 0xca, 0xd3, 0x39, 0xb7, 0x21, 0x50, 0x06, 0x99, 0x32, 0xdc, 0x54,
    0x08, 0x69, 0x93, 0x2d, 0xc5, 0x40, 0x86, 0x9b, 0x99, 0x6e, 0x2a, 0x04, 0x34, 0xc9, 0x96, 0xe2,
    0xa0, 0x42, 0x9b, 0x99, 0x6c, 0x54, 0x08, 0x69, 0xb9, 0x96, 0xe4, 0x30, 0x21, 0xae, 0x79, 0x96,
    0xe4, 0x30, 0x21, 0x4d, 0xcc, 0xb7, 0x39, 0xa0, 0x42, 0x9b, 0x99, 0x6c, 0x28, 0x10, 0xae, 0x79,
    0x96, 0xc9, 0xa0, 0x42, 0xb9, 0xe6, 0x5b, 0x26, 0x81, 0x0a, 0x82, 0x65, 0xb2, 0x64, 0xa1, 0x50,
    0x65, 0x5b, 0x26, 0xdc, 0x85, 0x41, 0x95, 0x6c, 0x9b, 0x72, 0x15, 0x06, 0x55, 0xb2, 0x67, 0x50,
    0xa8, 0x1f, 0x5b, 0x26, 0x5b, 0x8a, 0x81, 0xf6, 0x99, 0x33, 0xae, 0x2a, 0x07, 0xda, 0x64, 0xce,
    0xb8, 0xa8, 0x10, 0xd3, 0x26, 0x5b, 0x90, 0xc0, 0xfb, 0x4d, 0xcc, 0xeb, 0x90, 0xc0, 0x86, 0x9b,
    0x9b, 0x80,
];

/// The two lists of hardware device types Qt's FFmpeg backend probes at its first `MediaPlayer`
/// (spelled out here, so a typo in `kourier_qt::QT_ENV_DEFAULTS` is caught).
const DECODING: &str = "QT_FFMPEG_DECODING_HW_DEVICE_TYPES";
const ENCODING: &str = "QT_FFMPEG_ENCODING_HW_DEVICE_TYPES";

/// A 1 × 1 GIF (Qt reads GIFs but cannot write them).
const TINY_GIF: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff,
    0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];
/// Content Qt decodes by itself when it may pick the decoder: vector graphics, and PostScript
/// (which its EPS plugin runs through Ghostscript).
const SVG: &[u8] =
    b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"><rect width=\"8\" height=\"8\" fill=\"red\"/></svg>";
const EPS: &[u8] =
    b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 10 10\n0 0 10 10 rectfill\nshowpage\n%%EOF\n";

/// The provider decodes a picture at the size it is drawn at, not whole (P1 fixes F6 and the
/// avatar's decode size): the reader is told the size to scale to in the FILE's orientation
/// (the transposed one for a picture stored sideways, EXIF 90/270: Qt turns it after decoding),
/// so libjpeg's scaling applies; an avatar is decoded at the size that covers its square.
fn assert_decoder_scales_at_decode() {
    let decode = kourier_qt::bridge::qobject::decode_probe;
    let jpeg = Codec::Jpeg as u8;
    let red = 0xdc1e1e;
    let blue = 0x1e1edc;
    // Stored 1600 x 800, rotated by 90 degrees to display 800 x 1600: the stored left (red)
    // becomes the top. Asked for 200 x 400.
    let rotated = kourier_qt::bridge::qobject::oriented_image(1600, 800, 6);
    assert!(rotated.starts_with(&[0xff, 0xd8]), "a JPEG");
    let got = decode(rotated.clone(), jpeg, 200, 400, false);
    assert_eq!(got.len(), 6, "decoded");
    assert_eq!(
        (got[0], got[1]),
        (200, 400),
        "the displayed size, covering the request"
    );
    assert_eq!(got[2], 0, "scaled by the decoder, not after a whole decode");
    assert!(
        got[5] <= 200 * 400 * 2,
        "a rotated picture is decoded at its scaled size, not whole: {} pixels of 1,280,000",
        got[5]
    );
    let near = |a: i32, b: i32| {
        let c = |x: i32, s: u32| (x >> s) & 0xff;
        [16, 8, 0].iter().all(|s| (c(a, *s) - c(b, *s)).abs() <= 24)
    };
    assert!(
        near(got[3], red) && near(got[4], blue),
        "rotated by 90: {got:x?}"
    );
    // Rotated by 270: the stored left (red) becomes the bottom.
    let got = decode(
        kourier_qt::bridge::qobject::oriented_image(1600, 800, 8),
        jpeg,
        200,
        400,
        false,
    );
    assert_eq!((got[0], got[1], got[2]), (200, 400, 0), "rotated by 270");
    assert!(
        near(got[3], blue) && near(got[4], red),
        "rotated by 270: {got:x?}"
    );
    // Not rotated: scaled to the request by the decoder all the same.
    let flat = decode(
        kourier_qt::bridge::qobject::oriented_image(1600, 800, 1),
        jpeg,
        400,
        200,
        false,
    );
    assert_eq!((flat[0], flat[1], flat[2]), (400, 200, 0), "not rotated");
    // An avatar (cut to a square later) is decoded at the size whose shorter side covers the
    // side asked for: 2000 x 1000 for 100 x 100 is 200 x 100, not 2,000,000 pixels.
    let avatar = decode(
        kourier_qt::bridge::qobject::oriented_image(2000, 1000, 1),
        jpeg,
        100,
        100,
        true,
    );
    assert_eq!((avatar[0], avatar[1]), (100, 100), "the avatar's square");
    assert!(
        avatar[5] <= 200 * 100 * 2,
        "an avatar is decoded at the size that covers its square: {} pixels of 2,000,000",
        avatar[5]
    );
    // A timeline picture is drawn whole into a rectangle of its own shape rounded to pixels and
    // Qt asks for exactly that rectangle: 1080 x 2340 asked 398 x 864 covers to 399 x 864
    // (398.77 rounds up), and drawn into 398 pixels the picture would be resampled by 0.9975
    // (the fine detail halves). A cover within a pixel of the request on both sides is the
    // request itself.
    let shot = kourier_qt::bridge::qobject::oriented_image(1080, 2340, 1);
    let whole = decode(shot.clone(), jpeg, 398, 864, false);
    assert_eq!(
        (whole[0], whole[1]),
        (398, 864),
        "a cover a pixel off is the request"
    );
    let whole = decode(shot.clone(), jpeg, 199, 432, false);
    assert_eq!((whole[0], whole[1]), (199, 432), "the same at scale 1");
    // What is not within a pixel of the request stays a cover: a small square of a quote's
    // thumbnail (the picture keeps its shape, one side is longer), one side only, and an
    // avatar's cut is not touched.
    let cover = decode(shot.clone(), jpeg, 80, 80, false);
    assert_eq!((cover[0], cover[1]), (80, 173), "a square is covered");
    let wide = decode(shot.clone(), jpeg, 200, 0, false);
    assert_eq!((wide[0], wide[1]), (200, 433), "one side asked");
    let cornered = decode(shot, jpeg, 398, 864, true);
    assert_eq!(
        (cornered[0], cornered[1]),
        (864, 864),
        "an avatar is its square, not snapped to the request"
    );
}

/// The provider's decoder (§9.4): Qt decodes a payload with the one decoder its codec byte
/// names and never detects a format by content, so bytes from other people cannot reach the
/// SVG, PDF, PostScript, camera raw and other plugins installed on the system.
fn assert_decoders_are_pinned(png: &[u8]) {
    let probe = kourier_qt::bridge::qobject::round_avatar_probe;
    let sample = kourier_qt::bridge::qobject::sample_image;
    // Every format Qt can write here decodes under its own codec, and under no other.
    for (format, codec, required) in [
        ("jpeg", Codec::Jpeg, true),
        ("webp", Codec::Webp, true),
        ("bmp", Codec::Bmp, true),
        ("avif", Codec::Avif, false),
        ("jxl", Codec::Jxl, false),
        ("heif", Codec::Heif, false),
    ] {
        let bytes = sample(96, 96, 200, format);
        if bytes.is_empty() {
            assert!(!required, "Qt cannot write {format}");
            continue;
        }
        assert_eq!(Codec::sniff(&bytes), Some(codec), "{format} by content");
        assert_eq!(probe(bytes.clone(), codec as u8, 40), 255, "{format}");
        assert_eq!(
            probe(bytes, Codec::Png as u8, 40),
            -1,
            "{format} as PNG: no other decoder steps in"
        );
    }
    assert_eq!(Codec::sniff(TINY_GIF), Some(Codec::Gif));
    assert!(probe(TINY_GIF.to_vec(), Codec::Gif as u8, 1) >= 0, "gif");
    assert_eq!(probe(TINY_GIF.to_vec(), Codec::Webp as u8, 1), -1);
    assert_eq!(probe(png.to_vec(), Codec::Jpeg as u8, 40), -1);
    // What is no known picture is refused before Qt sees it, and Qt refuses it under every
    // codec byte, the unknown ones included.
    for content in [SVG, EPS] {
        assert_eq!(Codec::sniff(content), None);
        for codec in 0..=16u8 {
            assert_eq!(probe(content.to_vec(), codec, 8), -1, "codec byte {codec}");
        }
    }
    assert_eq!(probe(png.to_vec(), 0, 40), -1, "no codec");
    assert_eq!(probe(png.to_vec(), 9, 40), -1, "an unknown codec");
}

/// What the two variables hold in this process: `UNSET`, or the value in quotes.
fn env_state() -> String {
    let show = |name: &str| {
        std::env::var_os(name).map_or_else(|| "UNSET".to_owned(), |v| format!("{v:?}"))
    };
    format!("decoding={} encoding={}", show(DECODING), show(ENCODING))
}

/// The programs Qt's own services or Kourier's launcher could start for a link: scripts that note
/// their name, the URL and the two variables as they find them in a file (`KOURIER_FAKE_OPEN_OUT`),
/// so a leak to a real browser is impossible and visible. Put first on `PATH` by the callers.
fn write_fake_openers(dir: &Path) {
    std::fs::create_dir_all(dir).expect("fake opener dir");
    let body = "#!/bin/sh\n\
        echo \"${0##*/} url=$1 decoding=[${QT_FFMPEG_DECODING_HW_DEVICE_TYPES-UNSET}] \
        encoding=[${QT_FFMPEG_ENCODING_HW_DEVICE_TYPES-UNSET}]\" >> \"$KOURIER_FAKE_OPEN_OUT\"\n";
    for name in [
        "xdg-open",
        "xdg-email",
        "kde-open",
        "kde-open5",
        "kde-open6",
        "gio",
        "gvfs-open",
        "gnome-open",
    ] {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write a fake opener");
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("mode");
    }
}

/// What the fake openers wrote, once there is a line (they are started detached).
fn fake_opener_said(out: &Path) -> String {
    let began = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(out)
            && text.ends_with('\n')
        {
            return text.trim_end().to_owned();
        }
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "no fake opener ran"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `--env-child <dir>`: the environment of a process that prepared it, and of the program its
/// launcher (`kourier_app::opener`, as `App.openExternalUrl` uses it) really starts: a fake
/// `xdg-open` first on `PATH` says what it finds; the process's own environment is unchanged.
fn env_child(dir: &Path) {
    // SAFETY: the first thing this child does; nothing else runs in it yet.
    unsafe { kourier_qt::prepare_environment() };
    println!("prepared: {}", env_state());
    write_fake_openers(&dir.join("bin"));
    let out = dir.join("said.txt");
    // SAFETY: as above; no thread has started.
    unsafe {
        std::env::set_var("PATH", dir.join("bin"));
        std::env::set_var("KOURIER_FAKE_OPEN_OUT", &out);
    }
    let command = kourier_app::opener::command(
        "https://example.org/the-real-launcher",
        &kourier_qt::env_set_by_kourier(),
    )
    .expect("a URL");
    assert!(kourier_app::opener::start(command));
    println!("child: {}", fake_opener_said(&out));
    println!("after: {}", env_state());
}

/// What a child of this binary (`--env-child`) says with `set` in its environment and nothing
/// else of the two.
fn env_child_says(dir: &Path, set: &[(&str, &str)]) -> Vec<String> {
    let mut command = Command::new(std::env::current_exe().expect("this binary"));
    command
        .arg("--env-child")
        .arg(dir)
        .env_remove(DECODING)
        .env_remove(ENCODING);
    for (name, value) in set {
        command.env(name, value);
    }
    let out = command.output().expect("start the child");
    assert!(
        out.status.success(),
        "{:?}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The app sets both lists empty unless the person set them (to any value, the empty one
/// included), and the program its launcher starts gets the person's environment: what the
/// person set, and nothing Kourier set. The app's own environment keeps what it set.
fn assert_the_environment_is_prepared_and_given_back(dir: &Path) {
    assert_eq!(
        env_child_says(&dir.join("none"), &[]),
        [
            "prepared: decoding=\"\" encoding=\"\"",
            "child: xdg-open url=https://example.org/the-real-launcher decoding=[UNSET] encoding=[UNSET]",
            "after: decoding=\"\" encoding=\"\"",
        ],
        "nothing set: both are set empty, the program finds neither, the app keeps both"
    );
    assert_eq!(
        env_child_says(&dir.join("one"), &[(DECODING, "vaapi")]),
        [
            "prepared: decoding=\"vaapi\" encoding=\"\"",
            "child: xdg-open url=https://example.org/the-real-launcher decoding=[vaapi] encoding=[UNSET]",
            "after: decoding=\"vaapi\" encoding=\"\"",
        ],
        "the person's value stays, and so does it for the program"
    );
    assert_eq!(
        env_child_says(&dir.join("both"), &[(DECODING, ""), (ENCODING, "hevc")]),
        [
            "prepared: decoding=\"\" encoding=\"hevc\"",
            "child: xdg-open url=https://example.org/the-real-launcher decoding=[] encoding=[hevc]",
            "after: decoding=\"\" encoding=\"hevc\"",
        ],
        "an empty value the person set counts as set"
    );
}

/// `--voice-child <with|without> <file>`: plays the file with a `MediaPlayer` the way the voice
/// note's player does, and exits 0 at its end. `with` prepares the environment first, as `main`
/// of the application does. It installs no Qt message handler: Qt's own log goes to stderr.
fn voice_child(mode: &str, file: &Path) -> i32 {
    if mode == "with" {
        // SAFETY: the first thing this child does; nothing else runs in it yet.
        unsafe { kourier_qt::prepare_environment() };
    }
    let mut qt = QGuiApplication::new();
    let script = format!(
        r#"
import QtQuick
import QtMultimedia

Item {{
    MediaPlayer {{
        id: player
        source: {url:?}
        // Muted, and the child reaches no audio server (`run_voice_child`): a test never makes a
        // sound on the desktop of the person who runs it.
        audioOutput: AudioOutput {{ muted: true }}
        onMediaStatusChanged: {{
            if (mediaStatus === MediaPlayer.EndOfMedia) {{
                console.log("voice: end of media");
                Qt.exit(0);
            }} else if (mediaStatus === MediaPlayer.InvalidMedia) {{
                console.error("voice: invalid media");
                Qt.exit(1);
            }}
        }}
        Component.onCompleted: play()
    }}
    Timer {{
        interval: 20000
        running: true
        onTriggered: {{
            console.error("voice: timeout");
            Qt.exit(2);
        }}
    }}
}}
"#,
        url = kourier_qt::app::file_url(file)
    );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/voice_child.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    // Which backend played: the plugin of Qt's FFmpeg backend is mapped into the process once a
    // `MediaPlayer` has been made (the evidence that the probe's absence from the log means
    // something).
    let ffmpeg =
        std::fs::read_to_string("/proc/self/maps").is_ok_and(|maps| maps.contains(FFMPEG_PLUGIN));
    println!("{FFMPEG_LINE}{ffmpeg}");
    code
}

/// The file name of Qt's FFmpeg multimedia plugin, and the line the voice child prints about it.
const FFMPEG_PLUGIN: &str = "libffmpegmediaplugin.so";
const FFMPEG_LINE: &str = "ffmpeg backend loaded: ";

/// What the voice child did: its exit code, whether the FFmpeg plugin was loaded in it, and what
/// it wrote to stderr (the hardware probe of Qt's FFmpeg backend logs under
/// `qt.multimedia.ffmpeg.hwaccel`).
struct VoiceRun {
    code: Option<i32>,
    /// What the child said about the FFmpeg plugin; `None` when it said nothing (it died).
    ffmpeg: Option<bool>,
    log: String,
}

/// Runs the voice child.
fn run_voice_child(mode: &str, file: &Path) -> VoiceRun {
    let out = Command::new(std::env::current_exe().expect("this binary"))
        .args(["--voice-child", mode])
        .arg(file)
        .env_remove(DECODING)
        .env_remove(ENCODING)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .env("QT_LOGGING_RULES", "qt.multimedia.ffmpeg.hwaccel*=true")
        // No sound on the desktop: no audio server is reachable from the child (the voice note
        // still plays to its end without one).
        .env("PULSE_SERVER", "unix:/nonexistent/kourier-test-no-audio")
        .env("PIPEWIRE_REMOTE", "kourier-test-no-audio")
        .env_remove("PULSE_RUNTIME_PATH")
        .output()
        .expect("start the child");
    let log = String::from_utf8_lossy(&out.stderr).into_owned();
    let said = String::from_utf8_lossy(&out.stdout);
    let ffmpeg = said
        .lines()
        .find_map(|line| line.strip_prefix(FFMPEG_LINE))
        .map(|loaded| loaded == "true");
    VoiceRun {
        code: out.status.code(),
        ffmpeg,
        log,
    }
}

/// The voice note plays to its end with the hardware probe off: without the call the child's log
/// HAS the probe's lines (the control: the test can see them), with it they are not there. The
/// control is decided from positive evidence: when the FFmpeg plugin is loaded in the child
/// without the call, the probe line must be in its log (a Qt that renames the line fails here
/// instead of passing a test that can no longer see anything); the test is skipped only when the
/// plugin is not there at all (another backend, or none).
fn assert_voice_notes_play_without_the_probe(dir: &Path) {
    let file = dir.join("voice.m4a");
    std::fs::write(&file, VOICE_AAC).expect("write the voice note");
    let without = run_voice_child("without", &file);
    assert_eq!(
        without.code,
        Some(0),
        "the voice note did not play:\n{}",
        without.log
    );
    // A child that played and said nothing about its backend is an error of the test, not a skip.
    let ffmpeg = without
        .ffmpeg
        .unwrap_or_else(|| panic!("the voice child did not say which backend it used"));
    if !ffmpeg {
        println!("voice note probe: skipped, Qt Multimedia does not use FFmpeg here");
        return;
    }
    assert!(
        without.log.contains("Check device types"),
        "the control failed: the FFmpeg plugin is loaded, but its probe is not in the log:\n{}",
        without.log
    );
    let with = run_voice_child("with", &file);
    assert_eq!(
        with.code,
        Some(0),
        "the voice note did not play:\n{}",
        with.log
    );
    assert_eq!(
        with.ffmpeg,
        Some(true),
        "another backend played with the call"
    );
    assert!(
        !with.log.contains("Check device types") && !with.log.contains("Checking HW context"),
        "the hardware probe ran although the lists were set empty:\n{}",
        with.log
    );
}

/// What the replacement of the launcher saw: the URL and the two variables as the program it
/// would start inherits them.
static OPENED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// The two variables as the child of `command` would find them: taken out by the command
/// (`UNSET`), set by it, or inherited from this process.
fn handed_to(command: &Command) -> String {
    let show = |name: &str| {
        command
            .get_envs()
            .find(|(key, _)| *key == name)
            .map_or_else(
                || std::env::var_os(name),
                |(_, value)| value.map(ToOwned::to_owned),
            )
            .map_or_else(|| "UNSET".to_owned(), |v| format!("{v:?}"))
    };
    format!("decoding={} encoding={}", show(DECODING), show(ENCODING))
}

/// Stands in for the launcher in `App.openExternalUrl`: notes the URL and the two variables as
/// the program it would start inherits them. Starts nothing.
fn recording_launcher(command: Command) -> bool {
    assert_eq!(command.get_program(), "xdg-open");
    let url = command
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ");
    OPENED
        .lock()
        .expect("the record")
        .push((url, handed_to(&command)));
    true
}

/// A store that answers every attachment with a blurhash placeholder at once (the fastest job
/// the provider has: microseconds), for the race child.
struct Placeholders;

impl kourier_qt::media::MediaStore for Placeholders {
    fn entry(
        &self,
        attachment: AttachmentId,
    ) -> Result<Option<kourier_app::facade::MediaEntry>, kourier_qt::media::ReadError> {
        Ok(Some(kourier_app::facade::MediaEntry {
            attachment,
            account: kourier_model::AccountId(1),
            kind: AttachmentKind::Image,
            mime: Some("image/jpeg".into()),
            state: AttachmentState::Remote,
            file: None,
            thumb: None,
            blurhash: Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".into()),
            dims: Some((640, 480)),
        }))
    }

    fn read(
        &self,
        _: &kourier_app::facade::MediaBlob,
    ) -> Result<zeroize::Zeroizing<Vec<u8>>, kourier_qt::media::ReadError> {
        Err(kourier_qt::media::ReadError::Failed)
    }

    fn blurhash(&self, _: &str, width: u32, height: u32) -> Option<Vec<u8>> {
        Some([90u8, 120, 160, 255].repeat((width * height) as usize))
    }

    fn avatar(&self, _: u64) -> Result<zeroize::Zeroizing<Vec<u8>>, kourier_qt::media::ReadError> {
        Err(kourier_qt::media::ReadError::Missing)
    }
}

/// How many hidden blurhash Images the race child shows: at most the provider's workers (3), as
/// under probe 2 every worker that emits `finished` itself waits for the reader's event loop, and
/// the reader waits for the worker of each request.
const RACE_IMAGES: usize = 3;

/// The race child: `RACE_IMAGES` hidden Images ask for a blurhash, as `ImageBlock`'s `blur`
/// does, with the provider's probe `raceImageFinishes(how)` on; exit 0 when every one is Ready
/// within 5 s, 2 when any is still Loading then, 3 on an error, 4 when the probe cannot be armed.
fn race_child(how: i32) -> i32 {
    kourier_qt::media::set_store(Some(std::sync::Arc::new(Placeholders)));
    let mut qt = QGuiApplication::new();
    if !kourier_qt::bridge::qobject::race_image_finishes(how) {
        eprintln!("race: the probe {how} cannot be armed (Qt's signal spy hook not found)");
        return 4;
    }
    let script = format!(
        r#"
import QtQuick

Window {{
    id: root
    width: 120
    height: 80
    visible: true
    property int ready: 0

    Repeater {{
        model: {RACE_IMAGES}
        delegate: Image {{
            required property int index
            visible: false
            asynchronous: true
            cache: false
            source: "image://kourier/blurhash/" + (index + 1) + "?v=" + index
            sourceSize.width: 32
            sourceSize.height: 32
            onStatusChanged: {{
                if (status === Image.Ready) {{
                    root.ready += 1;
                    if (root.ready === {RACE_IMAGES}) {{
                        console.log("race: every Image is Ready");
                        Qt.exit(0);
                    }}
                }} else if (status === Image.Error) {{
                    console.error("race: an error for " + source);
                    Qt.exit(3);
                }}
            }}
        }}
    }}
    Timer {{
        interval: 5000
        running: true
        onTriggered: {{
            console.error("race: " + ({RACE_IMAGES} - root.ready) + " of {RACE_IMAGES} Images still Loading after 5 s");
            Qt.exit(2);
        }}
    }}
}}
"#
    );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/race_child.qml"),
        );
    }
    let code = qt.as_mut().map_or(5, |qt| qt.exec());
    kourier_qt::bridge::qobject::race_image_finishes(0);
    code
}

/// A job that is done before Qt's pixmap reader connected to its response still reaches the
/// Image (`cpp/imageprovider.cpp` `announce`). Qt connects to `finished` only after
/// `requestImageResponse` returned, and it catches a `finished` emitted wholly before that (a
/// flag the response sets itself, checked after the connection: probe 1, which passes with a
/// worker that emits on its own thread too); it does NOT catch an emission that began before the
/// connection and set that flag after the check (Qt takes the receivers when an emission starts):
/// probe 2 makes that order certain, and an Image asking for a blurhash (microseconds of work)
/// stays in Loading for ever, nothing pending in the provider (1 of 277 renders of
/// `cargo xtask shots` under load before the fix). The provider announces `finished` queued to the
/// response's own thread, so it is emitted only once the reader is back in its event loop.
fn assert_a_fast_job_reaches_its_image() {
    for how in [1, 2] {
        let out = Command::new(std::env::current_exe().expect("this binary"))
            .args(["--race-child", &how.to_string()])
            .env("QT_QPA_PLATFORM", "offscreen")
            .env("QT_FORCE_STDERR_LOGGING", "1")
            .output()
            .expect("start the race child");
        assert_eq!(
            out.status.code(),
            Some(0),
            "probe {how}: the blurhash Images did not all reach Ready:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn main() {
    support::answer_list("media_ui");
    let args: Vec<String> = std::env::args().collect();
    if let Some(at) = args.iter().position(|a| a == "--env-child") {
        env_child(Path::new(args.get(at + 1).expect("a directory")));
        return;
    }
    if let Some(at) = args.iter().position(|a| a == "--race-child") {
        let how = args
            .get(at + 1)
            .and_then(|how| how.parse().ok())
            .expect("a probe");
        std::process::exit(race_child(how));
    }
    if let Some(at) = args.iter().position(|a| a == "--voice-child") {
        let mode = args.get(at + 1).expect("a mode");
        let file = args.get(at + 2).expect("a file");
        std::process::exit(voice_child(mode, Path::new(file)));
    }
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before the app and Qt start.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
    // What the script says when it fails must reach the test's output: without a terminal
    // Qt writes its messages to the journal, and a failure under load left no reason behind.
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    // The application sets both lists of hardware device types empty before it starts anything
    // (`kourier_qt::prepare_environment`, called first in `main` of the binary), so the voice note
    // below does not pay Qt's probe of every video device. A shell that exports them would change
    // what this test sees: they are taken out first.
    // SAFETY: as above.
    unsafe {
        std::env::remove_var(DECODING);
        std::env::remove_var(ENCODING);
        kourier_qt::prepare_environment();
    }
    // What the preparation does and gives back, and a voice note that plays without the probe:
    // children of this binary, before it starts a thread.
    let voice_dir =
        std::env::temp_dir().join(format!("kourier-media-voice-{}", std::process::id()));
    std::fs::create_dir_all(&voice_dir).expect("temp dir");
    assert_the_environment_is_prepared_and_given_back(&voice_dir);
    assert_voice_notes_play_without_the_probe(&voice_dir);
    assert_a_fast_job_reaches_its_image();
    // Whatever this process starts for a link from here on is a fake that writes a file: first
    // on PATH, under every name Qt's own services try, and no desktop for them to detect. A real
    // browser can never open, and a link that did not go through the app's launcher shows here.
    write_fake_openers(&voice_dir.join("fake-bin"));
    let leaked = voice_dir.join("leaked.txt");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs = vec![voice_dir.join("fake-bin")];
        dirs.extend(std::env::split_paths(&path));
        std::env::set_var("PATH", std::env::join_paths(dirs).expect("a PATH"));
        std::env::set_var("KOURIER_FAKE_OPEN_OUT", &leaked);
        std::env::set_var("XDG_CURRENT_DESKTOP", "kourier-test");
        std::env::remove_var("KDE_FULL_SESSION");
    }
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let conversation = handle
        .fixture()
        .and_then(|f| f.media_conversation)
        .expect("a media conversation");
    let (images, voice) = downloaded_media(&handle);
    let media = handle.media();

    // A handle-table miss resolves on a worker thread (and fills the table).
    let miss = *images
        .iter()
        .find(|id| media.get(**id).is_none())
        .expect("an image the timeline queries did not register");
    let h2 = handle.clone();
    let got = std::thread::spawn(move || resolve(&h2, &format!("thumb/{}?v=1", miss.0), 64, 48))
        .join()
        .expect("worker");
    match got {
        Image::Encoded {
            bytes,
            codec,
            corner,
        } => {
            assert_eq!(corner, None, "an attachment keeps its shape");
            assert!(
                bytes.starts_with(&[0xff, 0xd8, 0xff]) || bytes.starts_with(b"RIFF"),
                "decrypted picture bytes"
            );
            assert!(
                matches!(codec, Codec::Jpeg | Codec::Webp),
                "told from the content: {codec:?}"
            );
        }
        other => panic!("a miss did not resolve: {other:?}"),
    }
    assert!(media.get(miss).is_some(), "the miss was kept");

    // On a runtime thread a miss may not wait: the placeholder shows, nothing panics.
    if let Some(other) = images.iter().copied().find(|id| media.get(*id).is_none()) {
        let h3 = handle.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        handle.runtime().spawn(async move {
            let _ = tx.send(resolve(&h3, &format!("thumb/{}", other.0), 32, 24));
        });
        match rx.recv_timeout(Duration::from_secs(10)).expect("an answer") {
            Image::Rgba { width, height, .. } => assert!(width > 0 && height > 0),
            other => panic!("expected the placeholder, got {other:?}"),
        }
    }

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    // The picture the composer attaches: a PNG outside the profile (a user's file).
    let dir = std::env::temp_dir().join(format!("kourier-media-ui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let sample_path = dir.join("Field sample.png");
    let sample = kourier_qt::bridge::qobject::sample_png(320, 200, 100);
    assert!(sample.starts_with(b"\x89PNG"));
    std::fs::write(&sample_path, &sample).expect("write the sample");
    let sample_url = kourier_qt::app::file_url(&sample_path);
    // A real GIF, and PostScript named like one (the core takes the type of a file it cannot
    // tell by content from its name, as a sender's client could claim any type).
    let gif_path = dir.join("Loop.gif");
    std::fs::write(&gif_path, TINY_GIF).expect("write the GIF");
    let ps_path = dir.join("Disguised.gif");
    std::fs::write(&ps_path, EPS).expect("write the PostScript");
    // Pasting: the clipboard holds a picture.
    assert!(kourier_qt::bridge::qobject::set_clipboard_image_png(
        sample.clone()
    ));
    // Avatars come out of the provider round: transparent corners, an opaque middle.
    let probe = kourier_qt::bridge::qobject::round_avatar_probe;
    assert_eq!(Codec::sniff(&sample), Some(Codec::Png));
    assert_eq!(
        probe(sample.clone(), Codec::Png as u8, 40),
        255,
        "round avatar"
    );
    // The provider cuts an avatar to the corner it is asked for (alpha of the corner pixel,
    // of one a tenth of the side in, and of the center): a circle, a square with small
    // corners (the theme's), a plain square.
    let corner = kourier_qt::bridge::qobject::avatar_corner_probe;
    let alphas = |corner: i32, inset: i32, center: i32| (corner << 16) | (inset << 8) | center;
    for (radius, expected, what) in [
        (20, alphas(0, 0, 255), "half the side: a circle"),
        (65535, alphas(0, 0, 255), "more than half: a circle"),
        (8, alphas(0, 255, 255), "small corners"),
        (0, alphas(255, 255, 255), "no corners: a square"),
    ] {
        assert_eq!(
            corner(sample.clone(), Codec::Png as u8, 40, radius),
            expected,
            "{what}"
        );
    }
    assert_eq!(corner(sample.clone(), Codec::Jpeg as u8, 40, 8), -1);
    assert_decoders_are_pinned(&sample);
    assert_decoder_scales_at_decode();
    assert_eq!(
        kourier_viewmodel::media::kind_for_file_name("Field sample.png"),
        AttachmentKind::Image
    );

    // The launcher of `App.openExternalUrl` (and of every Qt URL handler) is replaced by a recorder
    // that starts nothing. The control: a command built WITHOUT the app's list of names to drop
    // would hand the program both lists, which the app set.
    kourier_qt::app::set_url_launcher(recording_launcher);
    {
        let mut control = Command::new("xdg-open");
        control.arg("control:");
        assert!(recording_launcher(control));
    }
    assert_eq!(
        std::mem::take(&mut *OPENED.lock().expect("the record")),
        [(
            "control:".to_owned(),
            "decoding=\"\" encoding=\"\"".to_owned()
        )],
        "the recorder sees the lists the app set"
    );
    let script = SCRIPT
        .replace("IMAGE", &images[0].0.to_string())
        .replace("VOICE", &voice.0.to_string())
        .replace("CONVERSATION", &conversation.0.to_string())
        .replace("SAMPLE", &format!("{sample_url:?}"))
        .replace(
            "GIFFILE",
            &format!("{:?}", kourier_qt::app::file_url(&gif_path)),
        )
        .replace(
            "PSFILE",
            &format!("{:?}", kourier_qt::app::file_url(&ps_path)),
        );
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/kourier-tests/media_ui.qml"),
        );
    }
    let code = qt.as_mut().map_or(3, |qt| qt.exec());
    drop(engine);
    if code == 0 {
        // The script asked the app to open links, through the app (`App.openExternalUrl`) and the
        // way Kirigami's own components do (`Qt.openUrlExternally`, which Qt hands to the URL
        // handler of its scheme): the program would have started with the person's environment
        // (neither list) every time, and the app's own is unchanged. Nothing reached Qt's own
        // services (the fakes on PATH would have written the file).
        let handed = "decoding=UNSET encoding=UNSET";
        assert_eq!(
            std::mem::take(&mut *OPENED.lock().expect("the record")),
            [
                "https://example.org/seen-by-the-opener",
                "https://example.org/via-qt?q=%C3%A9",
                "http://example.org/plain-http",
                "mailto:someone@example.org",
                "file:///tmp/kourier-opener-file",
            ]
            .map(|url| (url.to_owned(), handed.to_owned())),
            "the URLs the launcher was asked to open, with the environment its child gets"
        );
        assert!(
            !leaked.exists(),
            "a link went to Qt's own services, with this process's environment:\n{}",
            std::fs::read_to_string(&leaked).unwrap_or_default()
        );
        assert_eq!(env_state(), "decoding=\"\" encoding=\"\"");
        assert_no_plaintext(&app, &sample);
        // The voice note's plaintext copy went with its view.
        let copies = app.paths().media_dir().join("attachments");
        wait_for(10, || {
            let mut left = Vec::new();
            files(&copies, &mut left);
            left.is_empty().then_some(())
        });
    }
    app.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&voice_dir);
    if code == 0 {
        println!("test media on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!(
            "test media on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
