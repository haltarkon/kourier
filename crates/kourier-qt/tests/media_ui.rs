//! Media through the real core over the fake network (§8.6, §5.8): the image provider serves
//! thumbnails and placeholders from `core.db` and the encrypted blob store (a handle-table miss
//! resolves on a worker thread; on a runtime thread it degrades to the placeholder instead of
//! stalling), unknown ids fail cleanly, a picture attached in the composer reaches `SendFiles`
//! and comes back as an own message with a thumbnail, a pasted clipboard image lands in the
//! strip as a tmpfs file, a refused `OpenMedia` fails only its own request while a voice note's
//! copy opens (and goes with its view), Qt decodes only the formats the content shows (a GIF's
//! copy may be played; PostScript sent as a GIF gets the neutral tile and no player), and
//! nothing plaintext lands in the profile outside its runtime directory. Offscreen,
//! `harness = false`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{Query, QueryData};
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
                    AttachmentState::Remote | AttachmentState::Downloading { .. } => pending = true,
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

fn main() {
    if std::env::var_os("QT_QPA_PLATFORM").is_none() {
        // SAFETY: single-threaded test binary, before the app and Qt start.
        unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    }
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
    assert_eq!(
        kourier_viewmodel::media::kind_for_file_name("Field sample.png"),
        AttachmentKind::Image
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
    if code == 0 {
        println!("test media on the real core ... ok\n\ntest result: ok. 1 passed; 0 failed");
    } else {
        println!(
            "test media on the real core ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
