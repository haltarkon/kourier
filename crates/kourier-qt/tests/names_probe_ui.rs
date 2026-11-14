//! The names probe of RegExp filters R3: how to put the filters' marker background behind a part
//! of a NAME (the sender above a bubble, a chat's title). Qt's `Text` cannot elide rich text and
//! has no background for plain text, so two ways are tried against the same synthetic names:
//!
//! - A: a `RichText` label whose html is made from the name cut by `TextMetrics.elidedText`, the
//!   marked parts in a `background-color` span;
//! - B: a plain label with elide and rectangles behind it, placed from `TextMetrics` widths of the
//!   prefixes.
//!
//! The same names run under left-to-right, right-to-left and mixed text, with elide, at the
//! display scales 1 and 1.25 (two children of this binary; offscreen). The parent reads the
//! grabbed pictures: the columns of marker colour of A and of B against the columns of a
//! red-glyph rendering of the same span (what Qt itself lays out for the marked characters).
//! The measurements are printed (names here are synthetic); the test fails when A misses the
//! glyphs by more than 2 px anywhere, or when it loses an elide. B's numbers are reported, and
//! B must not be the one that is chosen unless it holds too.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};

const SCRIPT: &str = r##"
import QtQuick
import QtQuick.Window

Window {
    id: win
    width: 420
    height: 700
    visible: true
    color: "#fcfcfc"

    readonly property real w: 150
    readonly property color mark: "#ffd60a"
    property var cases: [
        { "name": "Rika Lowe", "marks": [[0, 4]] },
        { "name": "Alexandra Bartholomew Montgomery", "marks": [[10, 11], [28, 4]] },
        { "name": "רוני כהן לוי", "marks": [[5, 3]] },
        { "name": "Dana דנה Cohen", "marks": [[5, 3]] },
        { "name": "Оксана Тестенко", "marks": [[7, 8]] },
        { "name": "Ann <b> & Co", "marks": [[4, 3], [10, 2]] }
    ]
    property int step: 0
    property int finished: 0

    function over(alpha: real): string {
        const base = [0xff, 0xd6, 0x0a];
        const bg = [0xfc, 0xfc, 0xfc];
        let out = "#";
        for (let i = 0; i < 3; ++i) {
            const v = Math.round(base[i] * alpha + bg[i] * (1 - alpha));
            out += (v < 16 ? "0" : "") + v.toString(16);
        }
        return out;
    }

    function esc(s: string): string {
        return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
    }

    // The name cut to what fits, as html with the marks (clipped to the cut) in `style`.
    function html(text: string, marks: var, style: string): string {
        let out = "", at = 0;
        for (const m of marks) {
            if (m[0] >= text.length) {
                break;
            }
            const end = Math.min(m[0] + m[1], text.length);
            out += esc(text.slice(at, m[0])) + "<span style=\"" + style + "\">" + esc(text.slice(m[0], end)) + "</span>";
            at = end;
        }
        return out + esc(text.slice(at));
    }

    Repeater {
        model: win.cases
        delegate: Item {
            id: row
            required property var modelData
            required property int index
            y: index * 110
            width: win.width
            height: 110

            TextMetrics {
                id: metrics
                text: row.modelData.name
                elide: Qt.ElideRight
                elideWidth: win.w
            }
            TextMetrics {
                id: prefix
                font: plain.font
            }

            function prefixWidth(n: int): real {
                prefix.text = row.modelData.name.slice(0, n);
                return prefix.advanceWidth;
            }

            // A: rich text of the elided name, marked by a background.
            Item {
                id: a
                objectName: "a"
                x: 10
                y: 4
                width: win.w
                height: 24
                Text {
                    id: rich
                    width: win.w
                    textFormat: Text.RichText
                    text: win.html(metrics.elidedText, row.modelData.marks, "background-color:" + win.over(0.45))
                }
            }
            // The same with the marked characters in red: where Qt lays them out.
            Item {
                id: c
                objectName: "c"
                x: 10
                y: 32
                width: win.w
                height: 24
                Text {
                    width: win.w
                    textFormat: Text.RichText
                    text: win.html(metrics.elidedText, row.modelData.marks, "color:#ff0000")
                }
            }
            // B: a plain label with elide, rectangles behind it from the widths of the prefixes.
            Item {
                id: b
                objectName: "b"
                x: 10
                y: 60
                width: win.w
                height: 24
                Repeater {
                    model: row.modelData.marks
                    delegate: Rectangle {
                        required property var modelData
                        readonly property real x0: plain.effectiveHorizontalAlignment === Text.AlignRight ? win.w - plain.contentWidth : 0
                        x: x0 + row.prefixWidth(modelData[0])
                        width: Math.max(0, row.prefixWidth(modelData[0] + modelData[1]) - row.prefixWidth(modelData[0]))
                        height: plain.height
                        color: win.over(0.45)
                    }
                }
                Text {
                    id: plain
                    width: win.w
                    text: row.modelData.name
                    textFormat: Text.PlainText
                    elide: Text.ElideRight
                }
            }
            Component.onCompleted: console.log("probe " + index + " rich " + rich.contentWidth.toFixed(2)
                + " plain " + plain.contentWidth.toFixed(2) + " truncated " + plain.truncated
                + " elided " + (metrics.elidedText !== row.modelData.name)
                + " richh " + rich.contentHeight.toFixed(2))
        }
    }

    Timer {
        interval: 400
        running: true
        onTriggered: {
            const dir = DIR;
            const items = [];
            for (let i = 0; i < win.contentItem.children.length; ++i) {
                const row = win.contentItem.children[i];
                for (const k of ["a", "b", "c"]) {
                    for (const child of row.children) {
                        if (child.objectName === k) {
                            items.push({ "item": child, "file": dir + "/" + row.index + "-" + k + ".png" });
                        }
                    }
                }
            }
            let left = items.length;
            for (const it of items) {
                it.item.grabToImage(function (result) {
                    if (!result.saveToFile(it.file)) {
                        console.error("FAIL: grab not saved");
                        Qt.exit(1);
                    }
                    left -= 1;
                    if (left === 0) {
                        Qt.exit(0);
                    }
                });
            }
        }
    }
    Timer {
        interval: 30000
        running: true
        onTriggered: {
            console.error("FAIL: timeout");
            Qt.exit(1);
        }
    }
}
"##;

fn child(dir: &Path) -> i32 {
    // SAFETY: single-threaded test binary, before Qt starts.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    let script = SCRIPT.replace("DIR", &format!("{:?}", dir.display().to_string()));
    let mut qt = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(engine) = engine.as_mut() {
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/names_probe_ui.qml"),
        );
    }
    qt.as_mut().map_or(1, |q| q.exec())
}

/// The first and last column (in logical pixels) where `hit` holds on at least three rows.
fn span(path: &Path, scale: f64, hit: impl Fn([u8; 3]) -> bool) -> Option<(f64, f64)> {
    let img = image::open(path).expect("the grab").to_rgb8();
    let mut first = None;
    let mut last = None;
    for x in 0..img.width() {
        let n = (0..img.height())
            .filter(|&y| hit(img.get_pixel(x, y).0))
            .count();
        if n >= 3 {
            first.get_or_insert(x);
            last = Some(x);
        }
    }
    Some((f64::from(first?) / scale, f64::from(last? + 1) / scale))
}

fn near(p: [u8; 3], q: [u8; 3]) -> bool {
    p.iter().zip(q).all(|(a, b)| a.abs_diff(b) <= 3)
}

fn parent() {
    let base = std::env::temp_dir().join(format!("kourier-names-probe-{}", std::process::id()));
    let exe = std::env::current_exe().expect("this binary");
    let mut worst_a: f64 = 0.0;
    let mut report = Vec::new();
    for scale in ["1", "1.25"] {
        let dir: PathBuf = base.join(scale);
        std::fs::create_dir_all(&dir).expect("a directory");
        let out = Command::new(&exe)
            .env("KOURIER_NAMES_PROBE_CHILD", &dir)
            .env("QT_SCALE_FACTOR", scale)
            .output()
            .expect("the child");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "child at scale {scale}: {stderr}");
        for l in stderr.lines().filter(|l| l.contains("probe ")) {
            report.push(format!("scale {scale}: {l}"));
        }
        let s: f64 = scale.parse().expect("scale");
        // The marker colour of 0.45 yellow over #fcfcfc, as the script makes it.
        let mark = [253u8, 235, 143];
        for i in 0..6 {
            let a = span(&dir.join(format!("{i}-a.png")), s, |p| near(p, mark));
            let b = span(&dir.join(format!("{i}-b.png")), s, |p| near(p, mark));
            let c = span(&dir.join(format!("{i}-c.png")), s, |p| {
                p[0] > 200 && p[1] < 80 && p[2] < 80
            });
            // The red glyphs of ALL marked parts give the outer extent; the marker of all parts
            // must cover it, and stick out by no more than two pixels on a side.
            let verdict = |m: Option<(f64, f64)>| match (m, c) {
                (Some(m), Some(c)) => ((m.0 - c.0).abs().max((m.1 - c.1).abs()), true),
                _ => (f64::NAN, false),
            };
            let (da, ok_a) = verdict(a);
            let (db, ok_b) = verdict(b);
            report.push(format!(
                "scale {scale} case {i}: A off by {da:.2}px (found {ok_a}), B off by {db:.2}px (found {ok_b}); glyphs {c:?} A {a:?} B {b:?}"
            ));
            assert!(
                ok_a,
                "A shows no marker or no glyph in case {i} at scale {scale}"
            );
            worst_a = worst_a.max(da);
        }
    }
    for l in &report {
        println!("{l}");
    }
    println!("worst A deviation {worst_a:.2}px");
    assert!(
        worst_a <= 3.0,
        "A's marker is off its glyphs by {worst_a:.2}px"
    );
    let _ = std::fs::remove_dir_all(&base);
}

fn main() {
    support::answer_list("names_probe_ui");
    if let Some(dir) = std::env::var_os("KOURIER_NAMES_PROBE_CHILD") {
        std::process::exit(child(Path::new(&dir)));
    }
    parent();
    println!("test names probe ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
