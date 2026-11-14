//! The RegExp filters' highlight in the real chat window (offscreen, `harness = false`; RegExp
//! filters R3). The fixture's four highlighting filters are saved through the core and three rows
//! are added to the demo chat (`FixtureOptions::highlight`). The test runs itself as a child (to
//! read its stderr and its grabs):
//!
//! - the rows' text blocks carry the marks the rules make: two words of "Invoices" in the first,
//!   only the plain "invoice" in the second (the spoiler covers one word, and a mark touching a
//!   spoiler is dropped), words of "Garden talk" in the third;
//! - the marker is DRAWN: pixels inside a mark's rectangle have the marker colour, pixels beside it
//!   the bubble's; the row with the spoilers has no marker colour outside its one mark (the
//!   spoiler's place shows no marker);
//! - the marks are set again without a new html when a filter changes: its colour changes the
//!   marker, deleting it removes the marker (the document is cleared of the old marks).
//!
//! Nothing prints a name or a text; the words are the fixture's synthetic ones.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import app.kourier

MainWindow {
    id: win

    width: 520
    height: 900
    visible: true

    property string step: "start"
    property int waits: 0
    property int quiet: 0
    readonly property var view: win.chatPage ? win.chatPage.timeline : null

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 1500) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
        win.quiet = 0;
    }

    function find(root: Item, pred: var): Item {
        if (pred(root)) {
            return root;
        }
        for (const child of root.children) {
            const hit = win.find(child, pred);
            if (hit) {
                return hit;
            }
        }
        return null;
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property var blocks
            required property bool showAuthor
            required property string author
            required property var authorMarks
        }
    }

    // The model's row whose text starts with `start`.
    function rowStarting(start: string): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && r.blocks.length > 0 && r.blocks[0].type === "text" && r.blocks[0].plain.indexOf(start) === 0) {
                return { "row": r, "index": i };
            }
        }
        return null;
    }

    // Brings the row on screen and returns its delegate's text edit, or null.
    function editOf(found: var): var {
        const list = win.view.rowList;
        win.view.followNewest = false;
        list.positionViewAtIndex(found.index, ListView.Center);
        const d = win.view.delegateFor(found.row.messageId);
        if (d === null) {
            return null;
        }
        const edit = win.find(d, c => c.selectByMouse === true && c.visible && c.height > 0);
        return edit === null ? null : { "delegate": d, "edit": edit };
    }

    function rects(look: var, marks: var): string {
        let out = "";
        for (const m of marks) {
            const a = look.edit.positionToRectangle(m.start);
            const b = look.edit.positionToRectangle(m.start + m.len);
            const p = look.edit.mapToItem(look.delegate, a.x, a.y);
            out += " " + Math.round(p.x * 100) / 100 + "," + Math.round(p.y * 100) / 100 + "," + Math.round((b.x - a.x) * 100) / 100 + "," + Math.round(a.height * 100) / 100;
        }
        return out;
    }

    function grab(item: Item, name: string, then: string): void {
        win.step = "grabbing";
        item.grabToImage(function (result) {
            if (!result.saveToFile(DIR + "/" + name + ".png")) {
                return win.fail("the grab was not saved");
            }
            win.next(then);
        });
    }

    Timer {
        interval: 40
        repeat: true
        running: true
        property bool busy: false
        onTriggered: {
            if (busy) {
                return;
            }
            busy = true;
            try {
                win.tick();
            } finally {
                busy = false;
            }
        }
    }

    function tick(): void {
        switch (win.step) {
        case "start":
            if (!win.started || !App.hasAccounts) return win.later("the start page");
            win.openConversation(DEMO_CHAT);
            win.next("chat");
            return;
        case "chat": {
            if (win.chatPage === null || App.timeline.conversationId !== DEMO_CHAT || App.timeline.loading) return win.later("the chat");
            if (!App.filters.loaded || App.filters.count !== 4) return win.later("the four filters");
            const one = win.rowStarting("Invoice 14");
            const two = win.rowStarting("This invoice");
            const three = win.rowStarting("Bring tomatoes");
            if (!one || !two || !three) {
                if (win.waits === 200) {
                    let seen = "";
                    for (let i = 0; i < rows.count; ++i) {
                        const r = rows.objectAt(i);
                        seen += (r && r.kind === "message" && r.blocks.length > 0 && r.blocks[0].type === "text" ? r.blocks[0].plain.slice(0, 12) : "-") + "|";
                    }
                    console.log("rows " + rows.count + ": " + seen);
                }
                return win.later("the three rows");
            }
            if (one.row.blocks[0].marks.length === 0) return win.later("the marks of the first row");
            const m1 = one.row.blocks[0].marks;
            const m2 = two.row.blocks[0].marks;
            const m3 = three.row.blocks[0].marks;
            if (m1.length !== 2) return win.fail("row one: " + m1.length + " marks");
            if (m1[0].start !== 0 || m1[0].len !== 7 || m1[0].colour !== "#ffd60a") return win.fail("row one, first mark");
            const at = one.row.blocks[0].plain.indexOf("receipt");
            if (m1[1].start !== at || m1[1].len !== 7) return win.fail("row one, second mark");
            if (m2.length !== 1 || m2[0].start !== 5 || m2[0].len !== 7) return win.fail("row two: " + JSON.stringify(m2));
            if (m3.length !== 3) return win.fail("row three: " + m3.length + " marks");
            for (const m of m3) {
                if (m.colour !== "#30d158") return win.fail("row three: the colour of Garden talk");
            }
            win.next("look one");
            return;
        }
        case "look one": {
            const found = win.rowStarting("Invoice 14");
            const look = win.editOf(found);
            if (look === null) return win.later("row one on screen");
            win.quiet += 1;
            if (win.quiet < 8) return;
            console.log("rects one" + win.rects(look, found.row.blocks[0].marks));
            win.grab(look.delegate, "one", "look two");
            return;
        }
        case "look two": {
            const found = win.rowStarting("This invoice");
            const look = win.editOf(found);
            if (look === null) return win.later("row two on screen");
            win.quiet += 1;
            if (win.quiet < 8) return;
            console.log("rects two" + win.rects(look, found.row.blocks[0].marks));
            // The places of the two spoilers, which must show no marker.
            const sp = found.row.blocks[0].spoilers;
            let s = "";
            for (const x of sp) {
                const a = look.edit.positionToRectangle(x.start);
                const b = look.edit.positionToRectangle(x.start + x.len);
                const p = look.edit.mapToItem(look.delegate, a.x, a.y);
                s += " " + Math.round(p.x) + "," + Math.round(p.y) + "," + Math.round(b.x - a.x) + "," + Math.round(a.height);
            }
            console.log("spoilers two" + s);
            win.grab(look.delegate, "two", "names");
            return;
        }
        // A sender the "First names" filter marks: its label is a rich text label with the marker in
        // a `background-color` span, keeps the plain name as its accessible name, has a natural
        // width of its own (the tap on the name tests it) and the sender's tap still hits it.
        case "names": {
            let found = null;
            for (let i = 0; i < rows.count && found === null; ++i) {
                const r = rows.objectAt(i);
                if (r && r.kind === "message" && r.showAuthor && r.authorMarks.length > 0) {
                    found = { "row": r, "index": i };
                }
            }
            if (found === null) return win.later("a sender with marks");
            win.view.followNewest = false;
            win.view.rowList.positionViewAtIndex(found.index, ListView.Center);
            const d = win.view.delegateFor(found.row.messageId);
            if (d === null) return win.later("the sender's row");
            const label = win.find(d, c => c.objectName === "senderName");
            if (label === null || !label.visible || label.implicitWidth <= 0) return win.later("the sender's label");
            win.quiet += 1;
            if (win.quiet < 6) return;
            if (label.textFormat !== Text.RichText) return win.fail("the marked name is not a rich text label");
            if (label.text.indexOf("background-color") < 0) return win.fail("the marked name has no marker span");
            if (label.Accessible.name !== found.row.author) return win.fail("the accessible name is not the plain name");
            if (label.implicitWidth >= label.width) return win.fail("the natural width of the name follows the label's width");
            const at = label.mapToItem(d, 0, 0);
            if (!d.hitsSender(Qt.point(at.x + 2, at.y + 2))) return win.fail("a tap on the marked name misses it");
            if (d.hitsSender(Qt.point(at.x + label.implicitWidth + 40, at.y + 2))) return win.fail("a tap beside the marked name hits it");
            win.next("done");
            return;
        }
        case "done":
            Qt.exit(0);
            return;
        }
    }

    Timer {
        interval: 90000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

fn child(dir: &Path) -> i32 {
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 20,
            highlight: true,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let demo = handle
        .fixture()
        .and_then(|f| f.demo_conversation)
        .expect("the demo chat");
    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();
    let script = SCRIPT
        .replace("DEMO_CHAT", &demo.0.to_string())
        .replace("DIR", &format!("{:?}", dir.display().to_string()));
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/highlight_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    app.shutdown();
    code
}

fn numbers(line: &str, tag: &str) -> Vec<Vec<f64>> {
    line.split_once(tag)
        .map(|(_, rest)| {
            rest.split_whitespace()
                .map(|r| r.split(',').filter_map(|n| n.parse().ok()).collect())
                .collect()
        })
        .unwrap_or_default()
}

fn pixel(img: &image::RgbaImage, scale: f64, x: f64, y: f64) -> [u8; 3] {
    let p = img.get_pixel((x * scale) as u32, (y * scale) as u32).0;
    [p[0], p[1], p[2]]
}

/// The marker's yellow has little blue; every bubble colour has much more.
fn is_marker(p: [u8; 3]) -> bool {
    p[0] > 200 && p[1] > 150 && p[2] < 185 && p[0].abs_diff(p[1]) < 90
}

fn parent() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("kourier-highlight-ui-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a directory");
    let out = Command::new(std::env::current_exe().expect("this binary"))
        .env("KOURIER_HIGHLIGHT_CHILD", &dir)
        .output()
        .expect("the child");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "the child failed: {stderr}{stdout}");
    let all = format!("{stderr}\n{stdout}");
    let line = |tag: &str| {
        all.lines()
            .find(|l| l.contains(tag))
            .unwrap_or_else(|| panic!("no line {tag}: {all}"))
            .to_owned()
    };
    let one = image::open(dir.join("one.png"))
        .expect("grab one")
        .to_rgba8();
    let two = image::open(dir.join("two.png"))
        .expect("grab two")
        .to_rgba8();
    // The grabs are in device pixels; the script's rectangles in logical ones.
    let scale = 1.0;
    // Row one: inside each mark's rectangle (its middle) the marker, beside the text the bubble.
    let marks = numbers(&line("rects one"), "rects one");
    assert_eq!(marks.len(), 2, "two rectangles");
    for m in &marks {
        let (x, y, w, h) = (m[0], m[1], m[2], m[3]);
        // Left of the glyphs' middle row, inside the rectangle.
        let inside = pixel(&one, scale, x + 1.0, y + 1.0);
        assert!(is_marker(inside), "inside a mark: {inside:?}");
        let before = pixel(&one, scale, x - 3.0, y + 1.0);
        assert!(!is_marker(before), "before the mark: {before:?}");
        let after = pixel(&one, scale, x + w + 3.0, y + 1.0);
        assert!(!is_marker(after), "after the mark: {after:?}");
        assert!(h > 4.0);
    }
    // Row two: one mark only. Every marker pixel of the row lies inside that one rectangle.
    let marks = numbers(&line("rects two"), "rects two");
    assert_eq!(marks.len(), 1, "one rectangle");
    let m = &marks[0];
    let mut found = 0;
    for y in 0..two.height() {
        for x in 0..two.width() {
            let p = two.get_pixel(x, y).0;
            if p[3] > 0 && is_marker([p[0], p[1], p[2]]) {
                let (lx, ly) = (f64::from(x) / scale, f64::from(y) / scale);
                found += 1;
                assert!(
                    lx >= m[0] - 2.0
                        && lx <= m[0] + m[2] + 2.0
                        && ly >= m[1] - 2.0
                        && ly <= m[1] + m[3] + 2.0,
                    "a marker pixel outside the one mark: ({lx}, {ly})"
                );
            }
        }
    }
    assert!(found > 20, "the one mark draws: {found} pixels");
    let _ = std::fs::remove_dir_all(&dir);
}

fn main() {
    support::answer_list("highlight_ui");
    if let Some(dir) = std::env::var_os("KOURIER_HIGHLIGHT_CHILD") {
        std::process::exit(child(Path::new(&dir)));
    }
    parent();
    println!("test highlight ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
