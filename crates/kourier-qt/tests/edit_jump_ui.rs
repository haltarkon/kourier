//! The timeline never draws a frame in the wrong place when the composer (or the window) changes
//! height, judged at EVERY frame (his report on the archive `af4f0bf`: "In case of message
//! editing there is short jump of height of viewport"; `target/next/archive-findings/F2-*.md`).
//!
//! The composer is the page's footer, so every change of its height (an edit starts, a new line,
//! save, cancel, the reply banner, a line that wraps) is a change of the timeline's height. The
//! list reacts to a change of its height inside the layout of that frame, wrongly in both modes
//! (following: it stops at the bounds of the old height and leaves the newest row under the
//! composer; holding a row: it keeps its bottom edge and moves the row being read), so
//! `TimelineView.qml` puts the view back in the SAME frame (`settle()` in `onHeightChanged`, like
//! the held row's own `y` path). A correction queued one event later is drawn one frame too late:
//! at scale 1 a frame with the newest message under the composer, at scale 1.25 (the user's) worse:
//! the list's sub-pixel write in that frame is taken for his own scroll, following is lost for
//! good and his next message lands below the view.
//!
//! `scroll_ui` cannot prove this (it tolerates the lag by design: its arrivals push the newest end
//! for a frame, `staysNewest` allows six frames), so this test reads the state at
//! `Window.afterAnimating` of EVERY frame (after the polish, right before the sync: what that
//! frame draws) and fails on the first frame that is off. The real main window (narrow) over the
//! fixture's quotes chat in the app's own style (`org.kde.desktop`, KDE platform theme), no
//! arrivals, one run per display scale, 1 and 1.25 (two tests for nextest, `edit_jump_ui` and
//! `edit_jump_ui_scale125`; a plain `cargo test` runs both):
//!
//! - At the newest message (the view follows): an own message is sent; edit of the newest own
//!   message started (short text), a new line, letters, saved; started again and cancelled; a long
//!   message sent (it is in view) and its edit started (the composer grows by more than 100 px),
//!   saved; an older own message edited (start, new line, save; started again and cancelled); the
//!   reply banner on and off; a new message typed until it wraps and cleared; the window made
//!   smaller and larger by the script. In every frame: the newest row ends where the timeline ends (1 px), `followNewest`
//!   stays true, "Jump to Latest" never shows; after a send the new row is in view.
//! - In the middle (a row is held): the same moments (an edit started, a new line, saved with a
//!   real change; started again and cancelled; a long edit started and cancelled; the reply
//!   banner; a wrapping line; the window resized): the held row's window `y` never moves (0.5 px), in any frame.
//!
//! - A move of the user's that is still going on when the composer changes (review LOW-3: the line
//!   `list.settle()` in `onHeightChanged` must leave it alone). In the middle, ONE wheel notch (the
//!   desktop style's `Kirigami.WheelHandler` animates `contentY`, the private `kdeglobals` turn
//!   smooth scrolling on): the edit of a long message is started on the frame after the animation
//!   began, the composer grows by more than 100 px, and the held row keeps moving the wheel's way in
//!   every frame (never back, never more than the wheel's whole distance in one frame) and ends the
//!   distance a control notch without a change moved it (1.5 px). The step fails when the
//!   composer's change was not drawn while the animation was running (a move that had not begun, or
//!   was over). Then a finger (touch events, `list.moving` and `dragging`) drags the list while the
//!   reply banner comes and goes: the drag is not interrupted, the moves of the finger after a change
//!   are carried one to one, the view moves no more than the composer's own change, nothing moves at
//!   the release, the row under the finger ends where the finger left it and is the row held.
//!
//! Every moment must have been seen by at least one frame that shows another height of the list or
//! the composer than the frame before (the test fails when a change was never drawn).

mod support;

use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use cxx_qt_lib::{QByteArray, QQmlApplicationEngine, QUrl};
use cxx_qt_lib_extras::QApplication;
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::{UiContext, UiOptions};

/// The environment variable that makes a process the session of one display scale.
const RUN: &str = "KOURIER_EDIT_JUMP_UI_SCALE";

/// The tests nextest lists and the display scale (`QT_SCALE_FACTOR`) each one runs at.
const RUNS: [(&str, &str); 2] = [("edit_jump_ui", "1"), ("edit_jump_ui_scale125", "1.25")];

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import QtQuick.Controls as QQC2
import org.kde.kirigami as Kirigami
import app.kourier

// The app's own main window is the root (narrow: one page at a time).
MainWindow {
    id: win

    width: 520
    height: 620
    visible: true

    // What the script is doing: the step's name, the plan and the step that runs now.
    property string step: "start"
    property var plan: []
    property int upNext: 0
    property var cur: null
    property int waits: 0
    property int rested: 0
    property bool failed: false
    property var ev: null

    // Frames the window drew (counted at afterAnimating), the frames checked in this step, and the
    // frames of this step that showed another height of the list or the composer than the frame
    // before.
    property int frames: 0
    property int checked: 0
    property int changes: 0
    // The same over the whole session.
    property int allChecked: 0
    property int allChanges: 0
    property string lastSig: ""
    // The check of every frame: "" (none), "follow" (the newest end) or "held" (a row is held).
    property string watch: ""

    property real shortId: 0
    property real longId: 0
    property real h0: 0
    property string t0: ""
    // The held row of the middle moments and the window y it stood at.
    property var ref: null
    property real refY: 0
    property var jb: null

    readonly property var view: win.chatPage ? win.chatPage.timeline : null
    readonly property var list: win.view ? win.view.rowList : null
    readonly property var comp: win.chatPage ? win.chatPage.composer : null

    function r1(v: real): real {
        return Math.round(v * 10) / 10;
    }

    function facts(): string {
        const l = win.list;
        const c = win.comp;
        return "scale " + Screen.devicePixelRatio + ", frame " + win.frames + ", checked " + win.checked
            + ", changes " + win.changes
            + (l && c ? ", list h=" + win.r1(l.height) + " cy=" + win.r1(l.contentY) + ", composer h=" + win.r1(c.height)
                + ", follow " + win.view.followNewest : "");
    }

    function fail(msg: string): void {
        if (win.failed) {
            return;
        }
        win.failed = true;
        console.error("FAIL [" + win.step + "]: " + msg + " (" + win.facts() + ")");
        Qt.exit(1);
    }

    Instantiator {
        id: rows
        model: App.timeline
        delegate: QtObject {
            required property string kind
            required property real messageId
            required property var blocks
        }
    }

    function textOf(r: var): string {
        if (!r) {
            return "";
        }
        for (const b of r.blocks) {
            if (b.type === "text") {
                return b.plain;
            }
        }
        return "";
    }

    function rowOf(id: real): var {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
                return r;
            }
        }
        return null;
    }

    function idWith(prefix: string): real {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.kind === "message" && win.textOf(r).indexOf(prefix) === 0) {
                return r.messageId;
            }
        }
        return 0;
    }

    function indexOf(id: real): int {
        for (let i = 0; i < rows.count; ++i) {
            const r = rows.objectAt(i);
            if (r && r.messageId === id) {
                return i;
            }
        }
        return -1;
    }

    function find(root: Item, pred: var): Item {
        if (!root) {
            return null;
        }
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

    // The floating button (always made, shown by its `visible`).
    function jumpButton(): Item {
        if (win.jb === null && win.view) {
            win.jb = win.find(win.view, c => c.text === "Jump to Latest");
        }
        return win.jb;
    }

    function key(k: int, m: int): void {
        if (!win.ev.keyClick(k, m, -1)) {
            win.fail("the window took no key event");
        }
    }

    function typeSome(): void {
        win.ev.keyClick(Qt.Key_A, Qt.NoModifier, -1);
        win.ev.keyClick(Qt.Key_B, Qt.NoModifier, -1);
        win.ev.keyClick(Qt.Key_Space, Qt.NoModifier, -1);
    }

    // What is wrong with the newest end, or "": the newest row ends where the timeline ends, the
    // view follows, "Jump to Latest" is hidden; `inView`: the row also starts inside the timeline
    // (it is wholly in view).
    function newestProblem(inView: bool): string {
        const l = win.list;
        const lp = l.mapToItem(null, 0, 0);
        const end = lp.y + l.height;
        const bad = [];
        const n = l.itemAtIndex(0);
        if (n === null) {
            bad.push("the newest row has no delegate");
        } else {
            const np = n.mapToItem(null, 0, 0);
            const off = np.y + n.height - end;
            if (Math.abs(off) > 1) {
                bad.push("the newest row ends " + win.r1(Math.abs(off)) + " px " + (off > 0 ? "below" : "above") + " the timeline's end");
            }
            if (inView && np.y < lp.y - 0.5) {
                bad.push("the newest row starts " + win.r1(lp.y - np.y) + " px above the timeline");
            }
        }
        if (!win.view.followNewest) {
            bad.push("followNewest is false");
        }
        const jb = win.jumpButton();
        if (jb === null) {
            bad.push("no Jump to Latest button");
        } else if (jb.visible) {
            bad.push("Jump to Latest is shown");
        }
        return bad.join("; ");
    }

    // What is wrong with the held row of the middle moments, or "": its window y is the one it had.
    function heldProblem(): string {
        if (!win.ref) {
            return "the held row is gone";
        }
        const y = win.ref.mapToItem(null, 0, 0).y;
        if (Math.abs(y - win.refY) > 0.5) {
            return "the held row moved " + win.r1(y - win.refY) + " px (from " + win.r1(win.refY) + " to " + win.r1(y) + ")";
        }
        return "";
    }

    // The view rests in the middle: it holds a row, and that row is the reference of the frames.
    function holdHere(): string {
        const h = win.list.held;
        if (win.view.followNewest || !h) {
            return "the view holds no row in the middle (follow " + win.view.followNewest + ")";
        }
        win.ref = h;
        win.refY = h.mapToItem(null, 0, 0).y;
        return "";
    }

    // ---- a move of the user's that is still going on (the wheel's animation, a finger) ----
    // One record per step: the window y of the reference row (`win.ref`) and the state of every frame
    // from the moment the move began, where the move began, the frames in which the list or the
    // composer had another height than the frame before (`hcs`), and how many frames in a row the
    // row stood still after it had moved (`quiet`). `trigger` runs once, in the turn after the first
    // frame in which the row had moved `trigAt` px (a call from inside the frame itself would be
    // drawn in the same frame, before the polish).
    property var fly: null
    // The finger's distance from where it touched, the touch, the tick of the drag, and the
    // distance the control notch moved the row.
    property real fingerDy: 0
    property var touch: null
    property int dragN: 0
    property real wheelD: 0
    // What the steps measured, for the line the test prints at its end.
    property var notes: []

    function flyStart(trigger: var, trigAt: real): void {
        win.fly = {
            y0: win.ref.mapToItem(null, 0, 0).y,
            ys: [], cys: [], lhs: [], chs: [], mvs: [], drs: [], fds: [], fls: [],
            hcs: [], moved: false, quiet: 0, fired: false, trigger: trigger, trigAt: trigAt, releaseAt: -1
        };
    }

    function flySample(): void {
        const f = win.fly;
        const y = win.ref.mapToItem(null, 0, 0).y;
        f.ys.push(y);
        f.cys.push(win.list.contentY);
        f.lhs.push(win.list.height);
        f.chs.push(win.comp.height);
        f.mvs.push(win.list.moving);
        f.drs.push(win.list.dragging);
        f.fds.push(win.fingerDy);
        f.fls.push(win.view.followNewest);
        const n = f.ys.length;
        if (!f.moved && Math.abs(y - f.y0) > 0.01) {
            f.moved = true;
        }
        if (n > 1) {
            if (f.moved && Math.abs(y - f.ys[n - 2]) < 0.01) {
                f.quiet += 1;
            } else {
                f.quiet = 0;
            }
            if (f.lhs[n - 1] !== f.lhs[n - 2] || f.chs[n - 1] !== f.chs[n - 2]) {
                f.hcs.push(n - 1);
            }
        }
        if (!f.fired && f.trigger && Math.abs(y - f.y0) >= f.trigAt) {
            f.fired = true;
            Qt.callLater(f.trigger);
        }
    }

    function flyFacts(i: int): string {
        const f = win.fly;
        return "flight frame " + i + " of " + f.ys.length + ": y " + win.r1(f.ys[i]) + " (began at " + win.r1(f.y0) + "), contentY " + win.r1(f.cys[i])
            + ", list h=" + win.r1(f.lhs[i]) + ", composer h=" + win.r1(f.chs[i]) + ", moving " + f.mvs[i] + ", dragging " + f.drs[i];
    }

    // The desktop style's wheel handler takes the wheel and animates `contentY` (the list never
    // reports `moving` for it): without it, or without smooth scrolling, there is no move in flight.
    function wheelReady(): string {
        let handler = null;
        for (const d of win.view.data) {
            if (d instanceof Kirigami.WheelHandler) {
                handler = d;
            }
        }
        if (handler === null) return "the timeline's ScrollView has no Kirigami.WheelHandler: this is not the org.kde.desktop style";
        if (handler.target !== win.list) return "the wheel handler is not the timeline's";
        if (!Kirigami.Settings.smoothScroll) return "smooth scrolling is off: the wheel does not animate";
        if (Kirigami.Units.longDuration <= 0) return "no time for the scroll animation";
        return "";
    }

    function notch(delta: int): void {
        if (!win.ev.mouseWheel(win.list, win.list.width / 2, win.list.height / 2, Qt.NoButton, Qt.NoModifier, 0, delta, -1)) {
            win.fail("the wheel event was not delivered");
        }
    }

    // What is wrong with a wheel notch that moved the held row, or "". `control`: nothing else
    // happened (it measures the notch's distance); else the composer changed once on the way.
    function wheelProblem(control: bool): string {
        const f = win.fly;
        const n = f.ys.length;
        for (let i = 0; i < n; ++i) {
            if (f.mvs[i]) return "the list reports moving: the notch did not go through the style's wheel handler alone (" + win.flyFacts(i) + ")";
            if (f.fls[i]) return "the view follows the newest message (" + win.flyFacts(i) + ")";
        }
        const d = win.wheelD;
        for (let i = 1; i < n; ++i) {
            const by = f.ys[i] - f.ys[i - 1];
            if (by < -0.5) return "the held row went back by " + win.r1(-by) + " px: the move of the user's was taken away (" + win.flyFacts(i) + ")";
            if (!control && by > d + 0.5) return "the held row moved " + win.r1(by) + " px in one frame, more than the whole notch (" + win.r1(d) + " px) (" + win.flyFacts(i) + ")";
        }
        const end = f.ys[n - 1] - f.y0;
        if (control) {
            if (end < 20 || end > 120) return "a notch moved the view by " + win.r1(end) + " px: not a step of the wheel handler";
            win.wheelD = end;
            return "";
        }
        if (f.hcs.length !== 1) return "the composer or the list changed height " + f.hcs.length + " times, not once (frames " + f.hcs.join(", ") + ")";
        const hc = f.hcs[0];
        const at = f.ys[hc] - f.y0;
        if (at < 2) return "the notch had not begun to move the view when the composer changed (" + win.r1(at) + " px of " + win.r1(d) + ") (" + win.flyFacts(hc) + ")";
        if (at > d - 3) return "the notch was over when the composer changed (" + win.r1(at) + " px of " + win.r1(d) + "): the change was not drawn in flight (" + win.flyFacts(hc) + ")";
        if (f.ys[n - 1] - f.ys[hc] < 3) return "the notch did not go on after the composer changed (" + win.flyFacts(hc) + ")";
        if (Math.abs(end - d) > 1.5) return "the view ended " + win.r1(end) + " px from where it began, the notch alone moved it " + win.r1(d) + " px (" + win.flyFacts(n - 1) + ")";
        win.notes.push("wheel: the composer changed " + win.r1(f.chs[hc] - f.chs[hc - 1]) + " px at " + win.r1(at) + " of " + win.r1(d) + " px, the view moved " + win.r1(end) + " px");
        return "";
    }

    // What is wrong with a drag during which the composer changed twice (the banner on, then off),
    // or "". `k` of a frame is the row's y minus the finger's distance: the place of the row under
    // the finger, which no change of the composer should alter. The first and the last change may
    // move it by the composer's own change at most (HEAD's `settle()` returns while the list is
    // `moving`, so the list's own keep of its bottom edge shows: that is the product's limit, the
    // test does not ask for more than "no more than that").
    function dragProblem(): string {
        const f = win.fly;
        const n = f.ys.length;
        if (f.hcs.length !== 2) return "the composer changed height " + f.hcs.length + " times, not twice (frames " + f.hcs.join(", ") + ")";
        const hc1 = f.hcs[0];
        const hc2 = f.hcs[1];
        const rel = f.releaseAt;
        if (rel < 0) return "no release was sent";
        let first = -1;
        for (let i = 0; i < rel; ++i) {
            if (f.drs[i]) {
                first = i;
                break;
            }
        }
        if (first < 0 || first >= hc1 - 1) return "the finger dragged the list too late or not at all (first dragging frame " + first + ", first change " + hc1 + ")";
        for (let i = first; i < rel; ++i) {
            if (!f.drs[i] || !f.mvs[i]) return "the drag was interrupted before the finger let go (" + win.flyFacts(i) + ")";
            if (f.fls[i]) return "the view follows the newest message in a drag in the middle (" + win.flyFacts(i) + ")";
        }
        const k = i => f.ys[i] - f.fds[i];
        const k0 = k(hc1 - 1);
        const up = f.chs[hc1] - f.chs[hc1 - 1];
        const shift1 = k(hc1) - k0;
        if (Math.abs(shift1) > Math.abs(up) + 1) return "the composer changed by " + win.r1(up) + " px and the row under the finger moved " + win.r1(shift1) + " px (" + win.flyFacts(hc1) + ")";
        for (let i = hc1 + 1; i < hc2; ++i) {
            if (Math.abs(k(i) - k(hc1)) > 0.5) return "a move of the finger after the change was not carried one to one: the row under the finger moved " + win.r1(k(i) - k(hc1)) + " px (" + win.flyFacts(i) + ")";
        }
        const down = f.chs[hc2] - f.chs[hc2 - 1];
        const shift2 = k(hc2) - k(hc2 - 1);
        if (Math.abs(shift2) > Math.abs(down) + 1) return "the composer changed by " + win.r1(down) + " px and the row under the finger moved " + win.r1(shift2) + " px (" + win.flyFacts(hc2) + ")";
        for (let i = hc2 + 1; i < rel; ++i) {
            if (Math.abs(k(i) - k(hc2)) > 0.5) return "the row under the finger moved " + win.r1(k(i) - k(hc2)) + " px with the finger still (" + win.flyFacts(i) + ")";
        }
        for (let i = rel; i < n; ++i) {
            if (Math.abs(f.ys[i] - f.ys[rel - 1]) > 0.5) return "the view moved " + win.r1(f.ys[i] - f.ys[rel - 1]) + " px when the finger let go (" + win.flyFacts(i) + ")";
        }
        const net = k(n - 1) - k0;
        if (Math.abs(net) > 0.5) return "after the banner came and went the row under the finger is " + win.r1(net) + " px from where the finger left it (" + win.flyFacts(n - 1) + ")";
        // The view settled where the user left it: it holds the row at its place.
        const h = win.list.held;
        if (!h || win.view.followNewest) return "after the drag the view holds no row (follow " + win.view.followNewest + ")";
        const at = h.y - win.list.contentY;
        if (Math.abs(at - win.list.heldAt) > 0.5) return "the held row stands " + win.r1(at) + " px in the view, the view recorded " + win.r1(win.list.heldAt);
        win.notes.push("drag: the banner moved the row under the finger " + win.r1(shift1) + " px (composer " + win.r1(up) + " px), its going " + win.r1(shift2) + " px (composer " + win.r1(down) + " px), the end " + win.r1(net) + " px from where the finger left it");
        return "";
    }

    // The state each frame draws: read after the polish, right before the sync.
    Connections {
        target: win
        function onAfterAnimating(): void {
            win.frames += 1;
            if (win.failed || !win.list || !win.comp) {
                return;
            }
            if (win.fly && win.ref) {
                win.flySample();
            }
            const sig = win.r1(win.list.height) + "/" + win.r1(win.comp.height);
            if (sig !== win.lastSig) {
                win.lastSig = sig;
                win.changes += 1;
            }
            if (win.watch === "") {
                return;
            }
            win.checked += 1;
            const why = win.watch === "follow" ? win.newestProblem(false) : win.heldProblem();
            if (why !== "") {
                win.fail("this frame is off: " + why);
            }
        }
    }

    // A step: `act` starts it (the frame check `watch` is on before it), `until` is what it waits
    // for (`poll` is called each tick it is not true yet), `changes` the frames that must have
    // shown another height, `rest` the ticks to wait after that, `check` a last look at the
    // settled state (a text, "" for none).
    function must(ok: bool, why: string): void {
        if (!ok) {
            win.fail(why);
        }
    }

    function grew(by: real): string {
        return win.comp.height - win.h0 > by ? "" : "the composer grew by " + win.r1(win.comp.height - win.h0) + " px, not by more than " + by;
    }

    function buildPlan(): var {
        const q = QUOTES;
        return [
            {
                name: "start",
                what: "the start page",
                until: () => {
                    if (!win.started || !App.hasAccounts) return false;
                    const lp = win.pageStack.get(0);
                    return !!lp && lp.rowsShown && lp.flickable.count >= 15;
                },
                check: () => {
                    win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "edit_jump_ui_events.qml");
                    win.openConversation(q);
                    return "";
                }
            },
            {
                name: "open",
                what: "the chat",
                until: () => win.chatPage !== null && App.timeline.conversationId === q && !App.timeline.loading && !!win.list && win.list.count >= 3,
                rest: 25,
                check: () => {
                    if (Math.abs(Screen.devicePixelRatio - SCALE) > 0.01) return "the display scale is " + Screen.devicePixelRatio + ", not SCALE";
                    if (win.jumpButton() === null) return "the Jump to Latest button was not found";
                    return win.newestProblem(false);
                }
            },
            // ---- the newest end: the view follows ----
            {
                name: "send-short",
                what: "the short message",
                act: () => App.composer.send(q, "Short probe text"),
                until: () => win.idWith("Short probe text") !== 0,
                rest: 30,
                check: () => {
                    win.shortId = win.idWith("Short probe text");
                    return win.newestProblem(true);
                }
            },
            {
                name: "a-start-short-newest",
                what: "the edit's composer",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.editLast(q), "editLast refused");
                },
                changes: 1,
                check: () => win.grew(20)
            },
            {
                name: "b-newline-newest",
                what: "the new line",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.comp.focusInput();
                    win.key(Qt.Key_Return, Qt.ShiftModifier);
                },
                changes: 1,
                check: () => win.grew(5)
            },
            {
                name: "b-type-newest",
                what: "the letters",
                watch: "follow",
                act: () => {
                    win.key(Qt.Key_X, Qt.NoModifier);
                    win.key(Qt.Key_X, Qt.NoModifier);
                },
                rest: 8
            },
            {
                name: "c-save-short-newest",
                what: "the saved edit",
                watch: "follow",
                act: () => {
                    win.t0 = win.textOf(win.rowOf(win.shortId));
                    win.key(Qt.Key_Return, Qt.NoModifier);
                },
                until: () => win.textOf(win.rowOf(win.shortId)) !== win.t0,
                changes: 1,
                check: () => win.newestProblem(true)
            },
            {
                name: "a-start-short-newest-2",
                what: "the second edit's composer",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.editLast(q), "editLast refused (2)");
                },
                changes: 1,
                check: () => win.grew(20)
            },
            {
                name: "c-cancel-short-newest",
                what: "the cancelled edit",
                watch: "follow",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1,
                check: () => win.newestProblem(true)
            },
            // An own message sent after the save is in view (at scale 1.25 it landed 134 px below the view).
            {
                name: "send-long",
                what: "the long message",
                act: () => App.composer.send(q, win.longText),
                until: () => win.idWith("Long probe line 1") !== 0,
                rest: 30,
                check: () => {
                    win.longId = win.idWith("Long probe line 1");
                    return win.newestProblem(true);
                }
            },
            {
                name: "a-start-long-newest",
                what: "the long edit's composer",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.editLast(q), "editLast refused (long)");
                },
                changes: 1,
                check: () => win.grew(100)
            },
            {
                name: "c-save-long-newest",
                what: "the saved long edit",
                watch: "follow",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Y, Qt.NoModifier);
                    win.t0 = win.textOf(win.rowOf(win.longId));
                    win.key(Qt.Key_Return, Qt.NoModifier);
                },
                until: () => win.textOf(win.rowOf(win.longId)) !== win.t0,
                changes: 1,
                check: () => win.newestProblem(true)
            },
            {
                name: "e-older-start-newest",
                what: "the older row's edit",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.startEdit(q, win.shortId), "startEdit refused (older)");
                },
                changes: 1,
                check: () => win.grew(20)
            },
            {
                name: "e-older-newline-newest",
                what: "the older row's new line",
                watch: "follow",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Return, Qt.ShiftModifier);
                    win.key(Qt.Key_W, Qt.NoModifier);
                },
                changes: 1
            },
            {
                name: "e-older-save-newest",
                what: "the older row saved",
                watch: "follow",
                act: () => {
                    win.t0 = win.textOf(win.rowOf(win.shortId));
                    win.key(Qt.Key_Return, Qt.NoModifier);
                },
                until: () => win.textOf(win.rowOf(win.shortId)) !== win.t0,
                changes: 1,
                rest: 12,
                check: () => win.newestProblem(true)
            },
            {
                name: "e-older-start-newest-2",
                what: "the older row's second edit",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.startEdit(q, win.shortId), "startEdit refused (older, 2)");
                },
                changes: 1,
                check: () => win.grew(20)
            },
            {
                name: "e-older-cancel-newest",
                what: "the older row's cancelled edit",
                watch: "follow",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1,
                check: () => win.newestProblem(true)
            },
            {
                name: "r-reply-newest",
                what: "the reply banner",
                watch: "follow",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.replyTo(q, win.longId), "replyTo refused");
                },
                changes: 1,
                check: () => win.grew(5)
            },
            {
                name: "r-cancel-reply-newest",
                what: "the banner gone",
                watch: "follow",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1,
                check: () => win.newestProblem(true)
            },
            {
                name: "w-wrap-newest",
                what: "a new message that wraps",
                watch: "follow",
                act: () => {
                    win.comp.focusInput();
                    win.h0 = win.comp.height;
                },
                poll: () => win.typeSome(),
                until: () => win.comp.height - win.h0 > 1,
                changes: 1
            },
            {
                name: "w-clear-newest",
                what: "the text cleared",
                watch: "follow",
                act: () => win.comp.textArea.clear(),
                changes: 1,
                check: () => win.newestProblem(true)
            },
            // The window made smaller and larger by the script (L1): the same height path, at the newest end.
            {
                name: "z-resize-smaller-newest",
                what: "the smaller window",
                watch: "follow",
                act: () => {
                    win.h0 = win.height;
                    win.height = win.h0 - 53;
                },
                changes: 1
            },
            {
                name: "z-resize-smaller-still-newest",
                what: "the window smaller still",
                watch: "follow",
                act: () => {
                    win.height = win.h0 - 161;
                },
                changes: 1
            },
            {
                name: "z-resize-larger-newest",
                what: "the window as it was",
                watch: "follow",
                act: () => {
                    win.height = win.h0;
                },
                changes: 1,
                check: () => win.newestProblem(true)
            },
            // ---- the middle: a row is held ----
            {
                name: "fillers",
                what: "the filler messages",
                act: () => {
                    for (let i = 1; i <= 14; ++i) {
                        App.composer.send(q, "Filler " + i);
                    }
                },
                until: () => win.idWith("Filler 14") !== 0,
                rest: 40,
                check: () => win.newestProblem(true)
            },
            {
                name: "m-scroll-short",
                what: "the view in the middle",
                act: () => {
                    win.view.followNewest = false;
                    win.list.positionViewAtIndex(win.indexOf(win.shortId), ListView.Center);
                },
                rest: 25,
                check: () => win.holdHere()
            },
            {
                name: "m-a-start-short-middle",
                what: "the edit's composer",
                watch: "held",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.startEdit(q, win.shortId), "startEdit refused");
                },
                changes: 1,
                check: () => win.grew(20)
            },
            {
                name: "m-b-newline-middle",
                what: "the new line",
                watch: "held",
                act: () => {
                    win.h0 = win.comp.height;
                    win.comp.focusInput();
                    win.key(Qt.Key_Return, Qt.ShiftModifier);
                },
                changes: 1,
                check: () => win.grew(5)
            },
            {
                name: "m-c-save-short-middle",
                what: "the saved edit",
                watch: "held",
                act: () => {
                    win.key(Qt.Key_Z, Qt.NoModifier);
                    win.t0 = win.textOf(win.rowOf(win.shortId));
                    win.key(Qt.Key_Return, Qt.NoModifier);
                },
                until: () => win.textOf(win.rowOf(win.shortId)) !== win.t0,
                changes: 1,
                rest: 12
            },
            {
                name: "m-d-start-short-middle-2",
                what: "the second edit's composer",
                watch: "held",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.startEdit(q, win.shortId), "startEdit refused (2)");
                },
                changes: 1,
                check: () => win.grew(20)
            },
            {
                name: "m-d-cancel-short-middle",
                what: "the cancelled edit",
                watch: "held",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1
            },
            {
                name: "m2-scroll-long",
                what: "the view at the long message",
                act: () => {
                    win.view.followNewest = false;
                    win.list.positionViewAtIndex(win.indexOf(win.longId), ListView.Center);
                },
                rest: 25,
                check: () => win.holdHere()
            },
            {
                name: "m2-a-start-long-middle",
                what: "the long edit's composer",
                watch: "held",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.startEdit(q, win.longId), "startEdit refused (long)");
                },
                changes: 1,
                check: () => win.grew(100)
            },
            {
                name: "m2-c-cancel-long-middle",
                what: "the cancelled long edit",
                watch: "held",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1
            },
            {
                name: "m3-reply-middle",
                what: "the reply banner",
                watch: "held",
                act: () => {
                    win.h0 = win.comp.height;
                    win.must(App.composer.replyTo(q, win.longId), "replyTo refused (middle)");
                },
                changes: 1,
                check: () => win.grew(5)
            },
            {
                name: "m3-cancel-reply-middle",
                what: "the banner gone",
                watch: "held",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1
            },
            {
                name: "m4-wrap-middle",
                what: "a new message that wraps",
                watch: "held",
                act: () => {
                    win.comp.focusInput();
                    win.h0 = win.comp.height;
                },
                poll: () => win.typeSome(),
                until: () => win.comp.height - win.h0 > 1,
                changes: 1
            },
            {
                name: "m4-clear-middle",
                what: "the text cleared",
                watch: "held",
                act: () => win.comp.textArea.clear(),
                changes: 1
            },
            {
                name: "m5-resize-smaller-middle",
                what: "the smaller window",
                watch: "held",
                act: () => {
                    win.h0 = win.height;
                    win.height = win.h0 - 97;
                },
                changes: 1
            },
            {
                name: "m5-resize-larger-middle",
                what: "the window as it was",
                watch: "held",
                act: () => {
                    win.height = win.h0;
                },
                changes: 1,
                check: () => win.heldProblem()
            },
            // ---- a move of the user's still going on: ONE wheel notch (the style animates `contentY`) ----
            {
                name: "x-scroll-control",
                what: "the view in the middle",
                act: () => {
                    win.fly = null;
                    win.view.followNewest = false;
                    win.list.positionViewAtIndex(win.indexOf(win.shortId), ListView.Center);
                },
                rest: 25,
                check: () => win.wheelReady() || win.holdHere()
            },
            // The control: the same notch with no change of the composer says how far one notch moves the row.
            {
                name: "x-wheel-control",
                what: "the control notch",
                act: () => {
                    win.flyStart(null, 0);
                    win.notch(120);
                },
                until: () => win.fly.ys.length > 4 && win.fly.quiet >= 10,
                check: () => win.wheelProblem(true)
            },
            {
                name: "x-scroll-edit",
                what: "the view in the middle again",
                act: () => {
                    win.fly = null;
                    win.view.followNewest = false;
                    win.list.positionViewAtIndex(win.indexOf(win.shortId), ListView.Center);
                },
                rest: 25,
                check: () => win.holdHere()
            },
            // The notch, and on the frame after the animation began the edit of the long message starts.
            {
                name: "x-wheel-edit",
                what: "the notch and the long edit",
                act: () => {
                    win.h0 = win.comp.height;
                    win.flyStart(() => win.must(App.composer.startEdit(q, win.longId), "startEdit refused (wheel)"), 2);
                    win.notch(120);
                },
                until: () => win.fly.ys.length > 4 && win.fly.hcs.length > 0 && win.fly.quiet >= 10,
                check: () => win.wheelProblem(false) || win.grew(100)
            },
            {
                name: "x-wheel-cancel",
                what: "the cancelled edit",
                act: () => {
                    win.fly = null;
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1,
                rest: 12
            },
            // ---- a finger drags the list (`moving`, `dragging`) while the banner comes and goes ----
            {
                name: "x-scroll-drag",
                what: "the view in the middle",
                act: () => {
                    win.fly = null;
                    win.view.followNewest = false;
                    win.list.positionViewAtIndex(win.indexOf(win.shortId), ListView.Center);
                },
                rest: 25,
                check: () => win.holdHere()
            },
            {
                name: "x-drag",
                what: "the finger drags and the banner comes and goes",
                act: () => {
                    win.h0 = win.comp.height;
                    win.dragN = 0;
                    win.fingerDy = 0;
                    win.flyStart(null, 0);
                    win.touch = win.ev.touchEvent(win.list);
                    win.touch.press(0, win.list, win.list.width / 2, 300);
                    win.touch.commit();
                },
                // One finger move per tick (12 px; the first 24 px only pass Qt's drag threshold), the
                // banner on in the turn of tick 6 and off in the turn of tick 11 (no move with it), then
                // the finger waits 8 ticks so it lets go with no speed.
                poll: () => {
                    const t = win.dragN;
                    win.dragN += 1;
                    if (t === 6) {
                        win.must(App.composer.replyTo(q, win.longId), "replyTo refused (drag)");
                    } else if (t === 11) {
                        win.comp.focusInput();
                        win.key(Qt.Key_Escape, Qt.NoModifier);
                    } else if (t < 12) {
                        win.fingerDy += 12;
                        win.touch.move(0, win.list, win.list.width / 2, 300 + win.fingerDy);
                        win.touch.commit();
                    }
                },
                until: () => win.dragN >= 20,
                rest: 2
            },
            {
                name: "x-release",
                what: "the finger lets go",
                act: () => {
                    win.fly.releaseAt = win.fly.ys.length;
                    win.touch.release(0, win.list, win.list.width / 2, 300 + win.fingerDy);
                    win.touch.commit();
                },
                until: () => !win.list.moving && win.fly.ys.length - win.fly.releaseAt >= 12,
                check: () => {
                    const why = win.dragProblem();
                    win.fly = null;
                    return why;
                }
            },
            // After the release the row the view holds is the one under the user: a banner now leaves it where it is.
            {
                name: "x-after-reply",
                what: "the banner after the drag",
                watch: "held",
                act: () => {
                    win.must(win.holdHere() === "", "the view holds no row after the drag");
                    win.h0 = win.comp.height;
                    win.must(App.composer.replyTo(q, win.longId), "replyTo refused (after the drag)");
                },
                changes: 1,
                check: () => win.grew(5)
            },
            {
                name: "x-after-cancel",
                what: "the banner gone after the drag",
                watch: "held",
                act: () => {
                    win.comp.focusInput();
                    win.key(Qt.Key_Escape, Qt.NoModifier);
                },
                changes: 1,
                check: () => win.heldProblem()
            }
        ];
    }

    readonly property string longText: "Long probe line 1\nLong probe line 2\nLong probe line 3\nLong probe line 4\nLong probe line 5\nLong probe line 6"

    Component.onCompleted: win.plan = win.buildPlan()

    // An input event can spin the event loop while it is delivered: the steps never nest.
    property bool busy: false
    Timer {
        interval: 40
        repeat: true
        running: true
        onTriggered: {
            if (win.busy) {
                return;
            }
            win.busy = true;
            try {
                win.tick();
            } finally {
                win.busy = false;
            }
        }
    }

    // The whole session has a time of its own (a hung step names itself before nextest's timeout).
    Timer {
        interval: 240000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }

    function tick(): void {
        if (win.failed) {
            return;
        }
        if (win.cur === null) {
            if (win.upNext >= win.plan.length) {
                console.warn("edit_jump_ui: done, " + win.frames + " frames, " + win.allChecked + " judged, " + win.allChanges + " with another height");
                for (const note of win.notes) {
                    console.warn("edit_jump_ui: in flight, " + note);
                }
                Qt.exit(0);
                return;
            }
            const s = win.plan[win.upNext];
            win.upNext += 1;
            win.cur = s;
            win.step = s.name;
            win.waits = 0;
            win.rested = 0;
            win.changes = 0;
            win.checked = 0;
            win.watch = s.watch ?? "";
            if (s.act) {
                s.act();
            }
            return;
        }
        const s = win.cur;
        win.waits += 1;
        if (win.waits > 400) {
            win.fail("never happened: " + (s.what ?? s.name));
            return;
        }
        if (s.until && !s.until()) {
            if (s.poll) {
                s.poll();
            }
            return;
        }
        if (win.changes < (s.changes ?? 0)) {
            return;
        }
        win.rested += 1;
        if (win.rested < (s.rest ?? 8)) {
            return;
        }
        if (s.watch && win.checked === 0) {
            win.fail("no frame was checked");
            return;
        }
        if (s.check) {
            const why = s.check();
            if (why) {
                win.fail(why);
                return;
            }
        }
        win.allChecked += win.checked;
        win.allChanges += win.changes;
        win.cur = null;
    }
}
"##;

fn main() -> ExitCode {
    let names: Vec<&str> = RUNS.iter().map(|(name, _)| *name).collect();
    support::answer_list_of(&names);
    match std::env::var(RUN) {
        Ok(scale) => session(&scale),
        Err(_) => both(),
    }
}

/// Runs the session of each scale this run names (nextest: `--exact <name>`; a plain `cargo test`
/// names none and runs both), each in a child process with a configuration directory of its own
/// (the settings and the display scale are read once per process).
fn both() -> ExitCode {
    let wanted: Vec<&(&str, &str)> = RUNS
        .iter()
        .filter(|(name, _)| support::named(name))
        .collect();
    let wanted = if wanted.is_empty() {
        RUNS.iter().collect()
    } else {
        wanted
    };
    let exe = std::env::current_exe().expect("this test's binary");
    let base = std::env::temp_dir().join(format!("kourier-edit-jump-ui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let children: Vec<_> = wanted
        .iter()
        .map(|(name, scale)| {
            let config = base.join(name);
            std::fs::create_dir_all(&config).expect("a configuration directory");
            std::fs::write(config.join("kdeglobals"), "[KDE]\nSmoothScroll=true\n")
                .expect("kdeglobals");
            let child = Command::new(&exe)
                .env(RUN, scale)
                .env("XDG_CONFIG_HOME", &config)
                // This test shows a window: always offscreen, whatever the session's
                // platform is, and in the style and with the platform theme of the desktop.
                .env("QT_QPA_PLATFORM", "offscreen")
                .env("QT_QPA_PLATFORMTHEME", "kde")
                .env("QT_QUICK_CONTROLS_STYLE", "org.kde.desktop")
                .env("QT_SCALE_FACTOR", scale)
                // What the script says when it fails must reach the test's output.
                .env("QT_FORCE_STDERR_LOGGING", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start the run");
            (*name, child)
        })
        .collect();
    let mut failed = 0;
    for (name, child) in children {
        let out = child.wait_with_output().expect("the run ends");
        let said = String::from_utf8_lossy(&out.stderr);
        // A script error is a failure even when the script went on to its end.
        let errors = support::script_errors(&said);
        if out.status.success() && errors.is_empty() {
            // (How many frames the script judged: `--no-capture` shows it.)
            for line in said.lines().filter(|l| {
                l.contains("edit_jump_ui: done") || l.contains("edit_jump_ui: in flight")
            }) {
                println!("{line}");
            }
            println!("test the timeline is in place in every frame ({name}) ... ok");
        } else {
            failed += 1;
            println!(
                "test the timeline is in place in every frame ({name}) ... FAILED ({})",
                out.status
            );
            // Why: the script's own line, then the end of what else was said.
            let (why, rest): (Vec<&str>, Vec<&str>) =
                said.lines().partition(|l| l.contains("FAIL"));
            for line in why.iter().chain(errors.iter()) {
                println!("{line}");
            }
            for line in rest.iter().rev().take(12).rev() {
                println!("    {line}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    let passed = wanted.len() - failed;
    if failed == 0 {
        println!("\ntest result: ok. {passed} passed; 0 failed");
        ExitCode::SUCCESS
    } else {
        println!("\ntest result: FAILED. {passed} passed; {failed} failed");
        ExitCode::FAILURE
    }
}

/// One session, in this process, at the display scale `scale` (already in the environment).
fn session(scale: &str) -> ExitCode {
    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions::default()),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let fixture = handle.fixture().expect("fixture mode");
    let quotes = fixture.quotes_conversation.expect("the quotes chat");

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    // QApplication, as the app: the org.kde.desktop style draws with a widget style.
    let mut qt = QApplication::new();

    let script = SCRIPT
        .replace("QUOTES", &quotes.0.to_string())
        .replace("SCALE", scale);
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(script.as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/edit_jump_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    kourier_qt::media::set_store(None);
    // (The messages the test sent are stored by now or on their way: let the core finish.)
    std::thread::sleep(Duration::from_millis(200));
    app.shutdown();
    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
