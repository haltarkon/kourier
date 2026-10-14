//! Picture quality in the timeline (P1, `target/next/picture-quality.md` §6.3): a received
//! picture is drawn from its FILE, decoded to the bubble's physical size, wherever the 400 px
//! thumbnail cannot cover that size. The old rule drew the thumbnail always; it was stretched
//! 1.9 times for a phone photo at scale 2 and 1.6 times for a phone screenshot at scale 1, and
//! nothing noticed, because the fixture's pictures are smooth gradients drawn at scale 1.
//!
//! This test needs what those lacked: pictures of fine detail (stripes of 6 and 8 px, two lines
//! of text, thin diagonals: `detail_image`), sent through the real core so each has its file
//! AND its derived thumbnail, shown by the real `ImageBlock.qml` in a window on a display of
//! scale 1 and 2 (and, by hand, the user's 1.25; `QT_SCALE_FACTOR`, honoured by the offscreen platform), then
//! grabbed and compared with the FILE decoded whole and scaled to the same physical size by
//! Qt's smooth scaling (`picture_probe`): how much of its fine detail the grab keeps (the
//! variance of the luma's Laplacian, whatever the alignment: a picture stretched from a smaller
//! one keeps a fraction of it) and that it is the right picture (the peak signal-to-noise ratio
//! of the luma averaged in blocks). The old rule fails it at scales 1 and 2 by a factor of 5 to
//! 25; scale 1.25 is only reported (see `REPORTED`).
//!
//! The pictures are sent as own messages (`UiCommand::SendFiles` keeps the user's file and
//! derives the 400 px thumbnail at once, like a download does) rather than put into the
//! fixture: no screenshot page changes and no list picture moves. The grab comes AFTER the
//! core derived the thumbnail and the chat is opened after that, so the first request of the
//! picture already finds the thumbnail: before the derive the old rule served the file
//! (picture-quality.md §3 item 1) and would pass. The test checks that the entry has its
//! thumbnail first.
//!
//! What the provider is asked is the item's PHYSICAL size: Qt multiplies an `Image`'s
//! `sourceSize` by the device pixel ratio itself, so the QML sets it in logical pixels
//! (`Math.ceil(width)`). Before the fix `ImageBlock.qml` multiplied it as well and the provider
//! was asked dpr times too much per axis (measured: 1152x1536 for an item of 576x768 pixels at
//! scale 2); the test logs the requests (`log_image_requests`) and checks the last one of each
//! picture against the grab's size.
//!
//! `harness = false`: the parent runs one child per display scale (a process has one scale),
//! each a real core over the fake network, the real `ImageBlock` and the real provider on the
//! offscreen platform. `KOURIER_PICTURE_UI_SHOTS=<dir>` keeps the grabs for a look,
//! `KOURIER_PICTURE_UI_SCALES=2,1.25` runs only those scales.

mod support;

use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::facade::{
    OriginKind, Query, QueryData, RequestId, TimelineAnchor, TimelineQuery, UiCommand, UiEvent,
};
use kourier_app::{App, AppHandle, FixtureOptions, Launch, StartOptions};
use kourier_model::{AttachmentId, Body, ConversationId};
use kourier_qt::media::{Codec, Image, resolve};
use kourier_qt::{UiContext, UiOptions};

/// The display scales that are judged: 1 (where only the tall pictures need the file) and 2
/// (the picture-quality.md table's worst column).
const SCALES: [&str; 2] = ["1", "2"];
/// The display scale of the user's own PC (125 %, 2026-10-13). The sharpness of its pictures is
/// only REPORTED: the item then sits between device pixels, the scaler's loss on fine stripes
/// is as big as the thumbnail's, and no threshold tells the two apart; the rule itself is a unit
/// test of the core (`timeline_blob`). What the provider is asked for and the corner of an avatar
/// are judged at every scale, this one included.
const REPORTED: &str = "1.25";

/// What a picture that comes from its file must keep of the reference's detail: the variance of
/// the Laplacian of the grab over the reference's (the reference is the file decoded whole and
/// scaled to the item's size by Qt's smooth scaling). Measured on the new rule at the two judged
/// scales: 0.74 to 1.53 (a different scaler than the reference's gives more or less moire);
/// on the old rule, the thumbnail stretched: 0.03 to 0.15. Halfway between, rounded.
const MIN_DETAIL: f64 = 0.5;
/// The least coarse peak signal-to-noise ratio (dB, luma after averaging 8 x 8 pixels: the right
/// picture at the right place; the stripes do not count) against the file scaled smoothly.
/// Measured on the new rule: 25.6 dB (the phone screenshot at scale 1, whose crop lands on half
/// a pixel) to 44 dB; the stretched thumbnail is the same picture, only soft, so this guards the
/// picture and its place and `MIN_DETAIL` guards the sharpness.
const MIN_COARSE_PSNR_DB: f64 = 18.0;

/// A picture of the test: `(name, width, height)`. Portrait 3:4 and a phone screenshot are the
/// ones the thumbnail never covered at scale 2 (the screenshot not even at scale 1); the
/// landscape one is covered at the user's scale 1.25 and not at 2.
const PICTURES: [(&str, u32, u32); 3] = [
    ("portrait", 1500, 2000),
    ("screenshot", 1080, 2340),
    ("landscape", 2000, 1500),
];

const SCRIPT: &str = r##"
import QtQuick
import QtQuick.Window
import QtQml
import app.kourier

// The window is the root (a Window inside an Item that has no window never shows, and a window
// that does not show cannot be grabbed).
Window {
    id: stage
    width: 400
    height: 640
    visible: true
    color: "white"

    Item {
    id: test

    property string step: "start"
    property int index: 0
    property var shownBlock: null
    property int settle: 0
    readonly property var wanted: [IDS]
    readonly property var names: [NAMES]

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property var blocks
        }
    }

    // The block of the timeline for an attachment.
    function blockFor(attachment: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (!r) {
                continue;
            }
            for (const b of r.blocks) {
                if (b.type === "image" && b.attachment === attachment) {
                    return b;
                }
            }
        }
        return null;
    }

    // The timeline's own block for a picture (it sizes itself by the screen, as in a bubble).
    Loader {
        id: blockView
        x: 0
        y: 0
        active: false
        sourceComponent: ImageBlock {
            block: test.shownBlock
        }
    }

    // An avatar of a list's size, round (the setting's default) and then with the theme's
    // corners: the corner the provider cuts must be the corner QML asked for, whatever the scale.
    Loader {
        id: avatarView
        x: 0
        y: 0
        active: false
        sourceComponent: Avatar {
            width: 40
            height: 40
            name: "Avatar Test"
            source: App.timeline.avatarUrl
        }
    }

    Timer {
        interval: 50
        repeat: true
        running: true
        onTriggered: {
            switch (test.step) {
            case "start":
                App.openConversation(CONVERSATION);
                test.step = "open";
                break;
            case "open": {
                const b = test.blockFor(test.wanted[test.index]);
                if (!b) {
                    return;
                }
                if (b.state !== "local" || b.url.indexOf("image://kourier/thumb/") !== 0) {
                    return fail("the block of " + test.names[test.index] + ": " + b.state + " " + b.url);
                }
                test.shownBlock = b;
                blockView.active = true;
                test.settle = 0;
                test.step = "shown";
                break;
            }
            case "shown": {
                const item = blockView.item;
                if (!item || !item.shown) {
                    return;
                }
                // One more turn so the scene graph has the finished picture.
                test.settle += 1;
                if (test.settle < 4) {
                    return;
                }
                console.warn("PICTURE " + test.names[test.index] + ": item " + item.width + "x" + item.height + " logical, devicePixelRatio " + item.Screen.devicePixelRatio + " stage.visible " + stage.visible + " window " + item.Window.window + " same " + (item.Window.window === stage) + " vis " + (item.Window.window ? item.Window.window.visible : "-") + " active " + stage.active);
                test.step = "grabbing";
                item.grabToImage(function (result) {
                    if (!result.saveToFile(GRABDIR + "/" + test.names[test.index] + ".png")) {
                        return test.fail("the grab was not saved");
                    }
                    test.step = "grabbed";
                });
                break;
            }
            case "grabbed":
                blockView.active = false;
                test.index += 1;
                test.step = test.index < test.wanted.length ? "open" : "avatar";
                break;
            case "avatar":
                if (App.timeline.avatarUrl === "") {
                    return fail("the chat has no avatar");
                }
                avatarView.active = true;
                test.settle = 0;
                test.step = "avatar shown";
                break;
            case "avatar shown": {
                const item = avatarView.item;
                if (!item || !item.pictureShown) {
                    return;
                }
                test.settle += 1;
                if (test.settle < 4) {
                    return;
                }
                const kind = App.settings.roundAvatars ? "round" : "corner";
                const file = GRABDIR + "/avatar-" + kind + "-" + Math.round(item.cornerRadius * 100) + "-" + item.side + ".png";
                test.step = "avatar grabbing";
                item.grabToImage(function (result) {
                    if (!result.saveToFile(file)) {
                        return test.fail("the avatar grab was not saved");
                    }
                    test.step = "avatar grabbed";
                });
                break;
            }
            case "avatar grabbed":
                avatarView.active = false;
                if (App.settings.roundAvatars) {
                    App.settings.roundAvatars = false;
                    test.step = "avatar";
                } else {
                    test.step = "done";
                }
                break;
            case "done":
                Qt.exit(0);
                break;
            }
        }
    }

    Timer {
        interval: 90000
        running: true
        onTriggered: fail("timeout")
    }
    }
}
"##;

/// The size of the requests the provider logged for `attachment`'s `thumb/` picture, in order.
fn requests_of(log: &[String], attachment: AttachmentId) -> Vec<(i32, i32)> {
    let prefix = format!("thumb/{}?", attachment.0);
    log.iter()
        .filter_map(|line| {
            let (id, size) = line.split_once(' ')?;
            let (w, h) = size.split_once('x')?;
            if !id.starts_with(&prefix) {
                return None;
            }
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .collect()
}

/// The last request for `attachment`'s picture.
fn last_request(log: &[String], attachment: AttachmentId) -> Option<(i32, i32)> {
    requests_of(log, attachment).last().copied()
}

/// The largest side of any request for `attachment`'s picture, as `(width, height)`.
fn largest_request(log: &[String], attachment: AttachmentId) -> (i32, i32) {
    requests_of(log, attachment)
        .into_iter()
        .fold((0, 0), |a, r| (a.0.max(r.0), a.1.max(r.1)))
}

/// Runs `f` until it returns `Some`, for up to `secs` seconds.
fn wait_for<T>(secs: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The attachment of an own picture of `dims` in `conv`.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn own_picture(app: &AppHandle, conv: ConversationId, dims: (u32, u32)) -> Option<AttachmentId> {
    let page = app
        .runtime()
        .block_on(app.query(Query::Timeline(TimelineQuery {
            conv,
            anchor: TimelineAnchor::Newest,
            limit: 50,
        })));
    let Ok(QueryData::Timeline(page)) = page else {
        panic!("the timeline of the chat");
    };
    page.rows
        .iter()
        .filter(|r| r.origin == OriginKind::OwnThisDevice)
        .filter_map(|r| match &r.body {
            Body::Media { attachments, .. } => Some(attachments),
            _ => None,
        })
        .flatten()
        .find(|a| a.dims == Some(dims))
        .and_then(|a| a.id)
}

/// Sends `path` to `conv` on its own (one message, one picture) and waits until the core
/// stored it.
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn send_picture(app: &AppHandle, conv: ConversationId, request: u64, path: PathBuf) {
    let mut events = app.subscribe();
    app.send(UiCommand::SendFiles {
        conv,
        request: RequestId(request),
        paths: vec![path].into(),
        as_file: Vec::new(),
        caption: None,
        reply_to: None,
    })
    .expect("send the picture");
    app.runtime().block_on(async {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                match events.recv().await {
                    Ok(UiEvent::FilesStored { request: r, .. }) if r == RequestId(request) => {
                        break;
                    }
                    Ok(UiEvent::ActionFailed {
                        action: "SendFiles",
                        ..
                    }) => panic!("the core refused the picture"),
                    Ok(_) => {}
                    Err(e) => panic!("events: {e}"),
                }
            }
        })
        .await
        .expect("the picture was stored in time");
    });
}

/// What the case found out about one picture, to be judged after Qt ran.
struct Shown {
    name: &'static str,
    attachment: AttachmentId,
    file: Vec<u8>,
    file_len: usize,
    thumb_len: usize,
}

/// One display scale, in this process (`QT_SCALE_FACTOR` is already in the environment).
#[allow(clippy::disallowed_methods)] // The test's main thread, not the Qt thread.
fn run_case(scale: &str, judged: bool) {
    let dpr: f64 = scale.parse().expect("a display scale");
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let conv = handle
        .fixture()
        .and_then(|f| f.media_conversation)
        .expect("a media conversation");
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    // ---- the pictures, sent through the real core, thumbnails derived ----
    let dir = std::env::temp_dir().join(format!(
        "kourier-picture-ui-{}-{}",
        std::process::id(),
        scale.replace('.', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let grabs = dir.join("grabs");
    std::fs::create_dir_all(&grabs).expect("grab dir");
    let mut shown = Vec::new();
    for (n, (name, w, h)) in PICTURES.into_iter().enumerate() {
        let jpeg = kourier_qt::bridge::qobject::detail_image(w as i32, h as i32, "jpeg");
        assert!(jpeg.starts_with(&[0xff, 0xd8, 0xff]), "{name}: a JPEG");
        let path = dir.join(format!("{name}.jpg"));
        std::fs::write(&path, &jpeg).expect("write the picture");
        send_picture(&handle, conv, 100 + n as u64, path);
        let attachment = wait_for(30, name, || own_picture(&handle, conv, (w, h)));
        // The core made the thumbnail with the import: the entry has both files before the
        // chat is opened, so the first request already meets it (the old rule served the
        // thumbnail from then on).
        let entry = wait_for(30, name, || {
            handle
                .media()
                .resolve_blocking(attachment)
                .ok()
                .flatten()
                .filter(|e| e.file.is_some() && e.thumb.is_some())
        });
        assert_eq!(entry.dims, Some((w, h)), "{name}");
        let media = handle.media();
        let file = media.read(entry.file.as_ref().unwrap()).expect("the file");
        let thumb = media
            .read(entry.thumb.as_ref().unwrap())
            .expect("thumbnail");
        assert_eq!(&file[..], &jpeg[..], "{name}: the file is the picture sent");
        assert!(
            thumb.len() < file.len() / 3,
            "{name}: the thumbnail is small"
        );
        shown.push(Shown {
            name,
            attachment,
            file: file.to_vec(),
            file_len: file.len(),
            thumb_len: thumb.len(),
        });
    }

    // ---- the real ImageBlock in a window, grabbed ----
    let ids = shown
        .iter()
        .map(|s| s.attachment.0.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let names = shown
        .iter()
        .map(|s| format!("{:?}", s.name))
        .collect::<Vec<_>>()
        .join(", ");
    let script = SCRIPT
        .replace("IDS", &ids)
        .replace("NAMES", &names)
        .replace("CONVERSATION", &conv.0.to_string())
        .replace("GRABDIR", &format!("{:?}", grabs.to_string_lossy()));
    let mut engine = QQmlApplicationEngine::new();
    kourier_qt::bridge::qobject::log_image_requests(true);
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/picture_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    let requests = kourier_qt::bridge::qobject::take_image_request_log();
    kourier_qt::bridge::qobject::log_image_requests(false);
    drop(engine);
    kourier_qt::media::set_store(None);
    assert_eq!(code, 0, "scale {scale}: the script failed (exit {code})");

    // ---- judged: which file the timeline asked for, and how sharp it came out ----
    let keep = std::env::var_os("KOURIER_PICTURE_UI_SHOTS").map(PathBuf::from);
    let mut sharp = 0;
    // `hard` problems fail at every scale (what the provider is asked for, the avatar's corner);
    // `problems` only where the scale is judged (the sharpness of the pictures).
    let mut hard = Vec::new();
    let mut problems = Vec::new();
    for s in &shown {
        let grab = std::fs::read(grabs.join(format!("{}.png", s.name))).expect("the grab");
        let probe = kourier_qt::bridge::qobject::picture_probe(
            s.file.clone(),
            Codec::Jpeg as u8,
            grab.clone(),
        );
        assert_eq!(probe.len(), 7, "scale {scale} {}: the probe", s.name);
        let (psnr, got, want, smooth_psnr) = (probe[0], probe[1], probe[2], probe[5]);
        let (w, h) = (probe[3] as i32, probe[4] as i32);
        // What the provider was asked for this picture: the last request is the one of the
        // finished item, and it is the item's physical size (the grab's), not more.
        let asked = last_request(&requests, s.attachment)
            .unwrap_or_else(|| panic!("scale {scale} {}: the provider was never asked", s.name));
        if (asked.0 - w).abs() > 1 || (asked.1 - h).abs() > 1 {
            hard.push(format!(
                "scale {scale} {}: the provider was asked for {}x{} but the item is {w}x{h} device pixels (sourceSize is logical: Qt multiplies it by the device pixel ratio)",
                s.name, asked.0, asked.1
            ));
        }
        let largest = largest_request(&requests, s.attachment);
        if largest.0 > w + 1 || largest.1 > h + 1 {
            hard.push(format!(
                "scale {scale} {}: a request of {}x{} was made for an item of {w}x{h}",
                s.name, largest.0, largest.1
            ));
        }
        let serve = |(w, h): (i32, i32)| match resolve(
            &handle,
            &format!("thumb/{}?v=1", s.attachment.0),
            w,
            h,
        ) {
            Image::Encoded { bytes, .. } => bytes.len(),
            other => panic!("{}: expected a picture, got {other:?}", s.name),
        };
        let served = serve(asked);
        let from_file = served == s.file_len;
        assert!(from_file || served == s.thumb_len, "{}: neither", s.name);
        println!(
            "scale {scale} {:>10}: item {w}x{h}, provider asked {}x{}, served the {}; coarse PSNR vs the provider's answer {psnr:.1} dB, vs the file scaled smoothly {smooth_psnr:.1} dB, detail {:.2} (grab {got:.0} / reference {want:.0}); asked {w}x{h} it would serve the {}",
            s.name,
            asked.0,
            asked.1,
            if from_file { "FILE" } else { "thumbnail" },
            got / want,
            if serve((w, h)) == s.file_len {
                "file"
            } else {
                "thumbnail"
            },
        );
        if let Some(keep) = &keep {
            std::fs::create_dir_all(keep).expect("keep dir");
            let _ = std::fs::copy(
                grabs.join(format!("{}.png", s.name)),
                keep.join(format!("scale-{scale}-{}.png", s.name)),
            );
        }
        // The rule: the file wherever the thumbnail (the long edge 400) cannot cover the ask.
        let (pw, ph) = PICTURES
            .iter()
            .find(|p| p.0 == s.name)
            .map(|p| (f64::from(p.1), f64::from(p.2)))
            .expect("a picture");
        let need = (f64::from(asked.0) / pw).max(f64::from(asked.1) / ph);
        let have = (400.0 / pw.max(ph)).min(1.0);
        if from_file != (need > have) {
            problems.push(format!(
                "scale {scale} {}: asked {}x{} of {pw}x{ph} (needs {need:.3}, the thumbnail keeps {have:.3}) and the {} was served",
                s.name,
                asked.0,
                asked.1,
                if from_file { "file" } else { "thumbnail" },
            ));
        }
        // What is drawn from the file is as sharp as the file scaled to that size.
        if need > have {
            sharp += 1;
            if got / want < MIN_DETAIL {
                problems.push(format!(
                    "scale {scale} {}: only {:.0} % of the detail of the file at {w}x{h} (PSNR {psnr:.1} dB): the picture was stretched from the thumbnail",
                    s.name,
                    100.0 * got / want
                ));
            }
            if smooth_psnr < MIN_COARSE_PSNR_DB {
                problems.push(format!(
                    "scale {scale} {}: coarse PSNR {smooth_psnr:.1} dB against the file at {w}x{h}",
                    s.name
                ));
            }
        }
    }
    // The screenshot always needs the file (its thumbnail is 185 px wide), the portrait from
    // scale 2 on.
    if sharp < 1 {
        problems.push(format!(
            "scale {scale}: no picture needed the file, the phone screenshot always does"
        ));
    }
    // ---- the avatar's corner: the cut the provider made against the corner QML asked for ----
    let mut avatars = 0;
    for entry in std::fs::read_dir(&grabs).expect("the grabs").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name
            .strip_prefix("avatar-")
            .and_then(|r| r.strip_suffix(".png"))
        else {
            continue;
        };
        // avatar-<round|corner>-<corner radius in 1/100 logical px>-<side in logical px>
        let parts: Vec<&str> = rest.split('-').collect();
        let [kind, radius100, side] = parts[..] else {
            panic!("the avatar grab's name: {name}");
        };
        let radius: f64 = radius100.parse::<f64>().expect("a radius") / 100.0;
        let side: f64 = side.parse().expect("a side");
        let grab = std::fs::read(entry.path()).expect("the avatar grab");
        let probe = kourier_qt::bridge::qobject::corner_probe(grab);
        assert_eq!(probe.len(), 3, "scale {scale} {kind} avatar: the probe");
        // The transparent corner of a quarter circle of radius r is r² (1 - pi/4) pixels.
        let measured = (probe[2] / (1.0 - std::f64::consts::FRAC_PI_4)).sqrt();
        let expected = radius * dpr;
        println!(
            "scale {scale} {kind:>6} avatar: {side} logical px = {} device px, corner radius asked {expected:.2} device px, drawn {measured:.2}",
            probe[0]
        );
        if (probe[0] - side * dpr).abs() > 1.0 {
            hard.push(format!(
                "scale {scale} {kind} avatar: grab is {} px wide, not {} device px",
                probe[0],
                side * dpr
            ));
        }
        if (measured - expected).abs() > 0.2 * expected + 0.6 {
            hard.push(format!(
                "scale {scale} {kind} avatar: the corner is drawn with radius {measured:.2} device px, QML asked for {expected:.2} (the provider cut it for another size than the item shows)"
            ));
        }
        avatars += 1;
    }
    if avatars != 2 {
        hard.push(format!("scale {scale}: {avatars} avatar grabs, expected 2"));
    }
    app.shutdown();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(hard.is_empty(), "{}", hard.join("\n"));
    if judged {
        assert!(problems.is_empty(), "{}", problems.join("\n"));
    } else {
        // Reported, not judged (see SCALES).
        for p in &problems {
            println!("scale {scale} (not judged): {p}");
        }
    }
}

/// Starts the case of `scale` in a child process of its own (a process has one display scale).
fn start_child(scale: &str, judged: bool) -> std::process::Child {
    let mut command = Command::new(std::env::current_exe().expect("this binary"));
    command.arg("--scale").arg(scale);
    if !judged {
        command.arg("--report-only");
    }
    command
        .env("QT_SCALE_FACTOR", scale)
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_FORCE_STDERR_LOGGING", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start the case")
}

/// Waits for a case and prints its numbers; `Err` is what went wrong.
fn finish_child(scale: &str, child: std::process::Child) -> Result<(), String> {
    let out = child.wait_with_output().expect("wait for the case");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The numbers of the case, for the test's output.
    for line in text.lines().filter(|l| l.starts_with("scale ")) {
        println!("{line}");
    }
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("scale {scale}: {}\n{text}", out.status))
    }
}

fn main() {
    support::answer_list("picture_ui");
    let args: Vec<String> = std::env::args().collect();
    if let Some(at) = args.iter().position(|a| a == "--scale") {
        let judged = !args.iter().any(|a| a == "--report-only");
        run_case(args.get(at + 1).map_or("2", String::as_str), judged);
        return;
    }
    // Other scales by hand: `KOURIER_PICTURE_UI_SCALES=2,1.25` runs exactly those.
    let only = std::env::var("KOURIER_PICTURE_UI_SCALES").ok();
    let mut cases: Vec<(&str, bool)> = SCALES.iter().map(|s| (*s, true)).collect();
    cases.push((REPORTED, false));
    if let Some(only) = &only {
        cases.retain(|(scale, _)| only.split(',').any(|o| o == *scale));
    }
    // Side by side: each is a core, an app and a window of its own, and the cases wait on
    // timers and the render loop more than they compute.
    let running: Vec<_> = cases
        .iter()
        .map(|(scale, judged)| (*scale, start_child(scale, *judged)))
        .collect();
    let mut failed = Vec::new();
    for (scale, child) in running {
        if let Err(why) = finish_child(scale, child) {
            failed.push(why);
        }
    }
    if failed.is_empty() {
        println!(
            "test pictures are drawn from the file where the thumbnail cannot cover them ... ok\n\ntest result: ok. 1 passed; 0 failed"
        );
    } else {
        for why in &failed {
            eprintln!("{why}");
        }
        println!(
            "test pictures are drawn from the file where the thumbnail cannot cover them ... FAILED\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
}
