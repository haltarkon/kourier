//! The 3D model viewer, front-end side (V3 of `viewer-3d.md`): the model's bubble, the viewer by
//! URL, the lists Quick 3D is fed with, and the tint the image provider applies. Offscreen,
//! `harness = false`, in the real main window over the fixture core:
//!
//! - the bubble (`ImageBlock.qml`) of a model shows its still with the size pill, asks the
//!   provider for the model colour in the URL (`&tint=RRGGBB`, only for a shade picture) and a tap
//!   opens the 3D viewer (`MediaActions.modelRequested`), never the picture viewer;
//! - the viewer opens by URL on the first tap: nothing of Quick 3D is loaded at start
//!   (`/proc/self/maps` holds no `libQt6Quick3D`), and on the software Quick backend (this test's)
//!   the still viewer opens and Quick 3D stays out of the process;
//! - "Free Rotation" toggles, and like the views and the wireframe it is hidden in the still
//!   fallback (which does not turn); the "Model Color" button draws its swatch itself and the
//!   swatch follows the colour; Escape closes the viewer;
//! - `ModelData` takes the lists a worker thread built (positions, face normals, barycentrics,
//!   colours with the fallback colour for the triangles that have none, the box), a failed open
//!   marks it failed, `clear()` drops the lists and a destroyed object leaves nothing behind;
//! - the provider multiplies a shade picture by the colour in its request, keeps the premultiplied
//!   alpha, and leaves every other request alone (`tint_probe`).
//!
//! What no test here can see: the real GL scene (the software Quick backend draws no Quick 3D);
//! that is looked at by hand on the PC.

mod support;

use cxx_qt_lib::{QByteArray, QGuiApplication, QQmlApplicationEngine, QUrl};
use kourier_app::{App, FixtureOptions, Launch, StartOptions};
use kourier_qt::bridge::qobject;
use kourier_qt::{UiContext, UiOptions};

const SCRIPT: &str = r##"
import QtQuick
import QtQml
import app.kourier

MainWindow {
    id: win

    width: 1100
    height: 760
    visible: true

    property string step: "start"
    property int waits: 0
    property var ev: null
    property var bubble: null
    property var asked: null
    property int pictureViewers: 0
    property var data5: null
    property var data6: null
    property real msg: 0
    property bool albumsAsked: false

    function fail(msg: string): void {
        console.error("FAIL [" + step + "]: " + msg);
        Qt.exit(1);
    }

    function later(what: string): void {
        win.waits += 1;
        if (win.waits > 400) {
            fail("never happened: " + what);
        }
    }

    function next(step: string): void {
        win.step = step;
        win.waits = 0;
    }

    function find(root: Item, name: string): Item {
        if (root.objectName === name) {
            return root;
        }
        for (const child of root.children) {
            const hit = win.find(child, name);
            if (hit) {
                return hit;
            }
        }
        return null;
    }

    function near(a: real, b: real): bool {
        return Math.abs(a - b) < 0.01;
    }

    Connections {
        target: MediaActions
        function onModelRequested(attachment: real, name: string, save: bool, tint: bool): void {
            win.asked = {
                "attachment": attachment,
                "name": name,
                "save": save,
                "tint": tint
            };
        }
        function onViewRequested(): void {
            win.pictureViewers += 1;
        }
    }

    Component {
        id: bubbleComponent
        ImageBlock {}
    }

    function modelBlock(tint: bool): var {
        return {
            "type": "image",
            "attachment": 7,
            "state": "local",
            "sticker": false,
            "animated": false,
            "kind": "file",
            "width": 400,
            "height": 300,
            "blurhash": "",
            "placeholderUrl": "",
            "url": "image://kourier/thumb/7?v=1",
            "fullUrl": "",
            "media": false,
            "label": "60 × 31 × 48 mm",
            "progress": 0,
            "save": true,
            "isModel": true,
            "modelTint": tint,
            "modelName": "bracket.stl"
        };
    }

    Timer {
        interval: 50
        repeat: true
        running: true
        onTriggered: {
            switch (win.step) {
            case "start":
                if (!win.visible || !App.loaded) {
                    return win.later("the window");
                }
                if (win.modelViewersOpened !== 0 || win.modelViewer !== null) {
                    return win.fail("a 3D viewer exists at start");
                }
                if (App.libraryLoaded("libQt6Quick3D")) {
                    return win.fail("Quick 3D is loaded at start");
                }
                win.ev = Qt.createQmlObject("import QtTest\nTestEvent {}", win.contentItem, "model3d_ui_events.qml");
                // The lists the worker thread delivers, and a failed open: the Rust side of the
                // test feeds them a moment from now.
                win.data5 = Qt.createQmlObject("import app.kourier\nModelData { attachment: 5; fallback: \"#102030\" }", win, "model3d_ui_data5.qml");
                win.data6 = Qt.createQmlObject("import app.kourier\nModelData { attachment: 6 }", win, "model3d_ui_data6.qml");
                win.bubble = bubbleComponent.createObject(win.contentItem, {
                    "x": 20,
                    "y": 20,
                    "block": win.modelBlock(true)
                });
                win.next("bubble");
                break;
            case "bubble": {
                const pic = win.find(win.bubble, "picture");
                if (!pic) {
                    return win.fail("the bubble has no picture");
                }
                const want = "image://kourier/thumb/7?v=1&tint=" + Style.modelTint;
                if (pic.source.toString() !== want) {
                    return win.fail("the tinted URL is " + pic.source + ", not " + want);
                }
                const note = win.find(win.bubble, "noteLabel");
                if (!note || note.text !== "60 × 31 × 48 mm") {
                    return win.fail("the size pill says " + (note ? note.text : "nothing"));
                }
                if (!win.find(win.bubble, "note").visible) {
                    return win.fail("the size pill is hidden");
                }
                if (win.bubble.mediaAction !== "model") {
                    return win.fail("the media action is " + win.bubble.mediaAction);
                }
                // A coloured model: its own colours, the URL asks for no tint.
                win.bubble.block = win.modelBlock(false);
                if (win.find(win.bubble, "picture").source.toString() !== "image://kourier/thumb/7?v=1") {
                    return win.fail("a coloured model's URL carries a tint: " + win.find(win.bubble, "picture").source);
                }
                win.bubble.block = win.modelBlock(true);
                win.bubble.activate();
                next("opened");
                break;
            }
            case "opened": {
                if (!win.asked) {
                    return win.later("the model request");
                }
                if (win.asked.attachment !== 7 || win.asked.name !== "bracket.stl" || !win.asked.save || !win.asked.tint) {
                    return win.fail("the request is " + JSON.stringify(win.asked));
                }
                if (win.pictureViewers !== 0) {
                    return win.fail("the picture viewer was asked for a model");
                }
                const v = win.modelViewer;
                if (!v || win.modelViewersOpened !== 1) {
                    return win.later("the viewer");
                }
                if (v.objectName !== "model3dViewer") {
                    return win.fail("the viewer is " + v.objectName);
                }
                if (v.fallback !== win.software) {
                    return win.fail("fallback " + v.fallback + " with software " + win.software);
                }
                if (App.libraryLoaded("libQt6Quick3D") && win.software) {
                    return win.fail("Quick 3D was loaded for the software fallback");
                }
                if (!v.opened) {
                    return win.later("the viewer to open");
                }
                const free = win.find(v.contentItem, "model3dFree");
                if (!free || v.freeRotation) {
                    return win.fail("the Free Rotation button is missing or checked");
                }
                // The still of the fallback does not turn, so the button is not shown there.
                if (free.visible === v.fallback) {
                    return win.fail("Free Rotation shown " + free.visible + " with fallback " + v.fallback);
                }
                free.toggle();
                if (!v.freeRotation) {
                    return win.fail("Free Rotation did not turn on");
                }
                free.toggle();
                if (v.freeRotation) {
                    return win.fail("Free Rotation did not turn off");
                }
                // The standard views and the wireframe need Quick 3D: the still fallback has neither.
                const views = win.find(v.contentItem, "model3dViews");
                const wire = win.find(v.contentItem, "model3dWire");
                if (!views || !wire) {
                    return win.fail("the views or wireframe button is missing from the tree");
                }
                if (v.fallback && (views.visible || wire.visible)) {
                    return win.fail("the fallback shows the views (" + views.visible + ") or the wireframe (" + wire.visible + ")");
                }
                if (!v.fallback && (!views.visible || !wire.visible)) {
                    return win.fail("the Quick 3D viewer hides the views or the wireframe");
                }
                if (v.fallback) {
                    const still = win.find(v.contentItem, "still");
                    if (!still || still.source.toString() !== "image://kourier/thumb/7?v=1&tint=" + Style.modelTint) {
                        return win.fail("the fallback's still is " + (still ? still.source : "missing"));
                    }
                }
                if (win.find(v.contentItem, "model3dName").text !== "bracket.stl") {
                    return win.fail("the header does not name the file");
                }
                // The box menu: only where Quick 3D draws; the table has its 18 rows and the
                // preference takes a row id and nothing else.
                const boxButton = win.find(v.contentItem, "model3dBox");
                if (!boxButton) {
                    return win.fail("the box button is missing from the tree");
                }
                if (v.fallback === boxButton.visible) {
                    return win.fail("the box button's visibility is " + boxButton.visible + " with fallback " + v.fallback);
                }
                const header = win.find(v.contentItem, "model3dName").parent;
                if (header.boxRows.length !== 18 || header.boxRows[9].id !== "256x256x256" || header.boxRows[9].label.indexOf("256 × 256 × 256 mm — Bambu Lab A1,") !== 0) {
                    return win.fail("the box rows are " + JSON.stringify(header.boxRows.slice(8, 10)));
                }
                if (App.settings.modelBox !== "") {
                    return win.fail("a box is chosen at start: " + App.settings.modelBox);
                }
                App.settings.modelBox = "9x9x9";
                if (App.settings.modelBox !== "") {
                    return win.fail("an unknown box was taken");
                }
                App.settings.modelBox = "180x180x180";
                if (header.boxId !== "180x180x180") {
                    return win.fail("the header does not follow the box preference");
                }
                App.settings.modelBox = "";
                // The colour: the viewer's button is on both paths and writes the Settings
                // preference; the bubble's URL and the still follow; "Use Accent Color" returns.
                const colourButton = win.find(v.contentItem, "model3dColour");
                if (!colourButton || !colourButton.visible) {
                    return win.fail("the colour button is missing or hidden");
                }
                const accentTint = Style.modelTint;
                if (App.settings.modelColour !== "") {
                    return win.fail("a colour is set at start");
                }
                let menu = null;
                for (const o of colourButton.data) {
                    if (o.objectName === "model3dColourMenu") {
                        menu = o;
                    }
                }
                if (!menu) {
                    return win.fail("the colour menu is missing");
                }
                const entry = name => {
                    for (let i = 0; i < menu.count; i++) {
                        if (menu.itemAt(i).objectName === name) {
                            return menu.itemAt(i);
                        }
                    }
                    return null;
                };
                if (!entry("model3dColour-e0529c") || !entry("model3dColour-accent")) {
                    return win.fail("the colour menu lacks an entry");
                }
                entry("model3dColour-e0529c").triggered();
                if (App.settings.modelColour !== "#e0529c" || Style.modelTint !== "e0529c") {
                    return win.fail("the viewer's button wrote " + App.settings.modelColour + ", tint " + Style.modelTint);
                }
                // The button draws its swatch itself (the desktop style draws no `image://` icon).
                const swatch = win.find(colourButton.contentItem, "model3dColourSwatch");
                if (!swatch || !swatch.visible || swatch.source.toString() !== "image://kourierart/swatch/e0529c") {
                    return win.fail("the button's swatch does not follow the colour: " + (swatch ? swatch.source : "none"));
                }
                App.settings.modelColour = "red";
                if (App.settings.modelColour !== "#e0529c") {
                    return win.fail("a bad colour was taken: " + App.settings.modelColour);
                }
                const picture = win.find(win.bubble, "picture");
                if (picture.source.toString() !== "image://kourier/thumb/7?v=1&tint=e0529c") {
                    return win.fail("the bubble's URL did not follow the colour: " + picture.source);
                }
                if (v.fallback && win.find(v.contentItem, "still").source.toString() !== "image://kourier/thumb/7?v=1&tint=e0529c") {
                    return win.fail("the still did not follow the colour");
                }
                entry("model3dColour-accent").triggered();
                if (App.settings.modelColour !== "" || Style.modelTint !== accentTint) {
                    return win.fail("Use Accent Color left " + App.settings.modelColour + ", tint " + Style.modelTint);
                }
                if (picture.source.toString() !== "image://kourier/thumb/7?v=1&tint=" + accentTint) {
                    return win.fail("the bubble's URL did not go back to the accent: " + picture.source);
                }
                win.next("slicerlist");
                break;
            }
            case "slicerlist": {
                // "Open in Slicer" on the viewer of the stand-in model (its slicers are the fake
                // OrcaSlicer entry of KOURIER_DEV_APPLICATIONS; the other three are not installed).
                const v = win.modelViewer;
                const header = win.find(v.contentItem, "model3dName").parent;
                if (!header.slicerRows[0].installed) {
                    return win.later("the scan of the slicers");
                }
                const ids = header.slicerRows.map(r => r.id + ":" + r.name + ":" + r.installed).join(",");
                if (ids !== "orcaslicer:OrcaSlicer:true,bambustudio:Bambu Studio:false,prusaslicer:PrusaSlicer:false,cura:UltiMaker Cura:false") {
                    return win.fail("the slicer rows are " + ids);
                }
                const main = header.slicerMain;
                const arrow = header.slicerArrowButton;
                if (!main.visible || main.objectName !== "model3dSlicer" || main.text !== "Open in OrcaSlicer" || main.Accessible.name !== "Open in OrcaSlicer") {
                    return win.fail("the main button says " + main.text + " / " + main.Accessible.name + " (visible " + main.visible + ")");
                }
                if (!arrow.visible || arrow.objectName !== "model3dSlicerMenu" || arrow.icon.name !== "arrow-down" || arrow.Accessible.name !== "Choose a Slicer") {
                    return win.fail("the arrow is " + arrow.objectName + " / " + arrow.icon.name + " / " + arrow.Accessible.name);
                }
                const menu = header.slicerList;
                if (menu.objectName !== "model3dSlicers") {
                    return win.fail("the menu is " + menu.objectName);
                }
                const entries = {};
                for (let i = 0; i < menu.count; ++i) {
                    const it = menu.itemAt(i);
                    entries[it.objectName] = it;
                }
                if (!entries["model3dSlicer-orcaslicer"].enabled || entries["model3dSlicer-orcaslicer"].text !== "OrcaSlicer") {
                    return win.fail("the Orca entry: " + entries["model3dSlicer-orcaslicer"].text);
                }
                for (const [id, text] of [["bambustudio", "Bambu Studio (not installed)"], ["prusaslicer", "PrusaSlicer (not installed)"], ["cura", "UltiMaker Cura (not installed)"]]) {
                    const e = entries["model3dSlicer-" + id];
                    if (!e || e.enabled || e.text !== text) {
                        return win.fail("the entry of " + id + " is " + (e ? e.enabled + " " + e.text : "missing"));
                    }
                }
                if (!entries["model3dSlicer-other"] || !entries["model3dSlicer-other"].enabled || entries["model3dSlicer-other"].text !== "Other Application…") {
                    return win.fail("the Other Application entry is wrong");
                }
                // The order: the four of the table, a separator, Other Application.
                if (menu.itemAt(0).objectName !== "model3dSlicer-orcaslicer" || menu.itemAt(3).objectName !== "model3dSlicer-cura" || menu.itemAt(5).objectName !== "model3dSlicer-other") {
                    return win.fail("the menu order is wrong");
                }
                // A remembered slicer that is not installed: the main part says "Open in Slicer"
                // and a click opens the list instead of starting anything.
                App.settings.modelSlicer = "bambustudio";
                if (main.text !== "Open in Slicer") {
                    return win.fail("a missing slicer's main button says " + main.text);
                }
                main.clicked();
                win.next("slicermenu");
                break;
            }
            case "slicermenu": {
                const v = win.modelViewer;
                const header = win.find(v.contentItem, "model3dName").parent;
                if (!header.slicerList.visible) {
                    return win.later("the slicer list to open");
                }
                if (win.slicerAsk !== null) {
                    return win.fail("a question was asked for a slicer that is not installed");
                }
                header.slicerList.close();
                App.settings.modelSlicer = "";
                if (App.settings.modelSlicer !== "") {
                    return win.fail("the slicer preference did not clear");
                }
                win.ev.keyClick(Qt.Key_Escape, 0, -1);
                win.next("closed");
                break;
            }
            case "closed":
                if (win.modelViewer !== null) {
                    return win.later("the viewer to close");
                }
                if (win.modelViewersOpened !== 1) {
                    return win.fail("viewers opened " + win.modelViewersOpened);
                }
                win.bubble.destroy();
                win.next("lists");
                break;
            case "lists": {
                if (!win.data5.ready || !win.data6.failed) {
                    return win.later("the lists of the worker");
                }
                const d = win.data5;
                if (d.triangles !== 2 || !d.coloured) {
                    return win.fail("triangles " + d.triangles + " coloured " + d.coloured);
                }
                if (d.positions.length !== 6 || d.normals.length !== 6 || d.uv1s.length !== 6 || d.colors.length !== 6) {
                    return win.fail("list lengths " + d.positions.length + " " + d.normals.length + " " + d.uv1s.length + " " + d.colors.length);
                }
                if (!win.near(d.normals[0].z, 1) || !win.near(d.normals[3].z, -1)) {
                    return win.fail("face normals " + d.normals[0] + " " + d.normals[3]);
                }
                if (!win.near(d.uv1s[0].x, 1) || !win.near(d.uv1s[1].y, 1) || !win.near(d.uv1s[2].x, 0)) {
                    return win.fail("barycentrics " + d.uv1s[0] + " " + d.uv1s[1] + " " + d.uv1s[2]);
                }
                // The first triangle has its own colour (red), the second takes the fallback.
                if (!win.near(d.colors[0].x, 1) || !win.near(d.colors[0].y, 0) || !win.near(d.colors[5].x, 16 / 255) || !win.near(d.colors[5].y, 32 / 255) || !win.near(d.colors[5].z, 48 / 255)) {
                    return win.fail("colours " + d.colors[0] + " " + d.colors[5]);
                }
                if (!win.near(d.boxX, 2) || !win.near(d.boxY, 3) || !win.near(d.boxZ, 4) || !win.near(d.centerX, 1)) {
                    return win.fail("box " + d.boxX + " " + d.boxY + " " + d.boxZ);
                }
                if (App.modelDataAlive() !== 2) {
                    return win.fail("alive " + App.modelDataAlive());
                }
                d.clear();
                if (d.ready || d.positions.length !== 0) {
                    return win.fail("clear() left the lists");
                }
                win.data5.destroy();
                win.data6.destroy();
                win.next("gone");
                break;
            }
            case "gone":
                if (App.modelDataAlive() !== 0) {
                    return win.later("the ModelData objects to go");
                }
                win.next("realopen");
                break;
            case "realopen": {
                // The real model message of the albums chat: its viewer, its menu, and a slicer started
                // through the seam (recorded, never run).
                if (App.timeline.conversationId !== ALBUMS_ID) {
                    if (!win.albumsAsked) {
                        win.albumsAsked = true;
                        win.openConversation(ALBUMS_ID);
                    }
                    return win.later("the albums chat");
                }
                win.msg = App.findMessage("3mf");
                if (win.msg === 0) {
                    return win.later("the model message");
                }
                const view = win.ensureChat().timeline;
                const delegate = view.delegateFor(win.msg);
                if (delegate === null) {
                    return win.later("the model row");
                }
                if (!delegate.activateMedia()) {
                    return win.later("the model's action");
                }
                win.next("realviewer");
                break;
            }
            case "realviewer": {
                const v = win.modelViewer;
                if (!v || !v.opened) {
                    return win.later("the real viewer");
                }
                const header = win.find(v.contentItem, "model3dName").parent;
                if (!header.slicerRows[0].installed || header.slicerMain.text !== "Open in OrcaSlicer") {
                    return win.later("the scan of the slicers");
                }
                if (App.settings.modelSlicer !== "") {
                    return win.fail("a slicer was remembered before any start: " + App.settings.modelSlicer);
                }
                header.slicerMain.clicked();
                const ask = win.slicerAsk;
                if (!ask || ask.slicerId !== "orcaslicer" || ask.slicerName !== "OrcaSlicer") {
                    return win.fail("the question is " + (ask ? ask.slicerId + "/" + ask.slicerName : "missing"));
                }
                win.next("asked");
                break;
            }
            case "asked": {
                const ask = win.slicerAsk;
                if (!ask || !ask.opened) {
                    return win.later("the question to open");
                }
                if (ask.subtitle.indexOf("opens in OrcaSlicer. Files can be harmful") < 0) {
                    return win.fail("the question says " + ask.subtitle);
                }
                if (App.settings.modelSlicer !== "") {
                    return win.fail("the slicer was remembered before the answer");
                }
                ask.customFooterActions[0].trigger();
                win.next("started");
                break;
            }
            case "started": {
                // The copy comes, the seam records the start, then the slicer is remembered.
                if (App.settings.modelSlicer !== "orcaslicer") {
                    return win.later("the slicer to start");
                }
                const v = win.modelViewer;
                const header = win.find(v.contentItem, "model3dName").parent;
                for (let i = 0; i < header.slicerList.count; ++i) {
                    if (header.slicerList.itemAt(i).objectName === "model3dSlicer-other") {
                        header.slicerList.itemAt(i).triggered();
                    }
                }
                win.next("other");
                break;
            }
            case "other": {
                const ask = win.slicerAsk;
                if (!ask || ask.slicerId !== "other" || !ask.opened) {
                    return win.later("the question for another application");
                }
                if (ask.subtitle.indexOf("opens in another application.") < 0) {
                    return win.fail("the question says " + ask.subtitle);
                }
                // Cancel: the portal's chooser is never asked here, and nothing is remembered.
                ask.customFooterActions[1].trigger();
                if (App.settings.modelSlicer !== "orcaslicer") {
                    return win.fail("Other Application changed the remembered slicer: " + App.settings.modelSlicer);
                }
                win.ev.keyClick(Qt.Key_Escape, 0, -1);
                win.next("viewerclosed");
                break;
            }
            case "viewerclosed":
                if (win.modelViewer !== null || win.slicerAsk !== null) {
                    return win.later("the viewer and the question to go");
                }
                win.ensureChat().timeline.menuFor(win.msg);
                win.next("menu");
                break;
            case "menu": {
                const menu = win.ensureChat().timeline.messageMenu;
                if (!menu.opened) {
                    return win.later("the message menu");
                }
                if (!menu.canSlicer || menu.modelAttachment <= 0) {
                    return win.fail("the menu does not offer Open in Slicer: " + menu.canSlicer + " " + menu.modelAttachment);
                }
                const sub = menu.slicerMenu;
                const entries = {};
                for (let i = 0; i < sub.count; ++i) {
                    entries[sub.itemAt(i).objectName] = sub.itemAt(i);
                }
                if (sub.title !== "Open in Slicer" || sub.objectName !== "menuSlicer") {
                    return win.fail("the sub-menu is " + sub.title + " / " + sub.objectName);
                }
                if (!entries["menuSlicer-orcaslicer"] || !entries["menuSlicer-orcaslicer"].enabled || entries["menuSlicer-orcaslicer"].text !== "OrcaSlicer") {
                    return win.fail("the Orca entry of the menu is wrong");
                }
                for (const id of ["bambustudio", "prusaslicer", "cura"]) {
                    const e = entries["menuSlicer-" + id];
                    if (!e || e.enabled || e.text.indexOf("(not installed)") < 0) {
                        return win.fail("the entry of " + id + " is not greyed with its reason");
                    }
                }
                if (!entries["menuSlicer-other"] || !entries["menuSlicer-other"].enabled) {
                    return win.fail("the menu has no Other Application entry");
                }
                entries["menuSlicer-orcaslicer"].triggered();
                const ask = win.slicerAsk;
                if (!ask || ask.slicerId !== "orcaslicer") {
                    return win.fail("the menu entry asked " + (ask ? ask.slicerId : "nothing"));
                }
                menu.close();
                win.next("menuasked");
                break;
            }
            case "menuasked": {
                const ask = win.slicerAsk;
                if (!ask || !ask.opened) {
                    return win.later("the menu's question");
                }
                ask.customFooterActions[1].trigger();
                win.next("done");
                break;
            }
            case "done":
                if (win.slicerAsk !== null) {
                    return win.later("the last question to go");
                }
                console.warn("model3d_ui ok, software " + win.software);
                Qt.exit(0);
                break;
            }
        }
    }

    Timer {
        interval: 120000
        running: true
        onTriggered: win.fail("timeout in step " + win.step)
    }
}
"##;

/// A PNG of 4 x 1 straight-alpha pixels: opaque grey 200, half transparent grey 100, opaque
/// black, fully transparent.
fn shade_png() -> Vec<u8> {
    let mut img = image::RgbaImage::new(4, 1);
    img.put_pixel(0, 0, image::Rgba([200, 200, 200, 255]));
    img.put_pixel(1, 0, image::Rgba([100, 100, 100, 128]));
    img.put_pixel(2, 0, image::Rgba([0, 0, 0, 255]));
    img.put_pixel(3, 0, image::Rgba([0, 0, 0, 0]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

/// The provider's tint step (`imageprovider.cpp`): the RGB of a shade picture times the colour in
/// the request, the (premultiplied) alpha kept; requests without a valid tint, and requests that
/// are no thumbnails, are served as decoded.
fn check_the_tint() {
    let probe = qobject::tint_probe;
    let png = shade_png();
    // [width, height, first, middle (x = 2), last] as premultiplied ARGB; the middle is opaque black.
    let plain = probe("thumb/7?v=1", png.clone());
    assert_eq!(&plain[..3], &[4, 1, 0xffc8_c8c8], "{plain:x?}");
    let argb = |a: u32, r: u32, g: u32, b: u32| (a << 24) | (r << 16) | (g << 8) | b;
    let tinted = probe("thumb/7?v=1&tint=ff8000", png.clone());
    // 200 * 255/255, 200 * 128/255 (rounded), 0: alpha kept.
    assert_eq!(tinted[2], argb(255, 200, 100, 0), "{tinted:x?}");
    assert_eq!(
        tinted[4],
        argb(0, 0, 0, 0),
        "a transparent pixel stays transparent"
    );
    // The half transparent pixel: premultiplied grey 50, then the colour: alpha 128 kept.
    let half = probe("thumb/7?v=1&tint=ff8000", shade_png_half());
    assert_eq!(half[2] >> 24, 128, "{half:x?}");
    assert_eq!(half[2] & 0xff, 0, "no blue in an orange tint: {half:x?}");
    let red = (half[2] >> 16) & 0xff;
    let green = (half[2] >> 8) & 0xff;
    assert!((49..=51).contains(&red), "red {red}");
    assert!((24..=26).contains(&green), "green {green}");
    // Not a tint: a bad digit, a short one, another kind of request.
    for id in [
        "thumb/7?v=1&tint=gg8000",
        "thumb/7?v=1&tint=ff80",
        "att/7?v=1&tint=ff8000",
        "thumb/7",
    ] {
        assert_eq!(probe(id, png.clone()), plain, "{id} is served as it is");
    }
}

/// One half transparent grey pixel (straight 100, alpha 128) in the first place.
fn shade_png_half() -> Vec<u8> {
    let mut img = image::RgbaImage::new(2, 1);
    img.put_pixel(0, 0, image::Rgba([100, 100, 100, 128]));
    img.put_pixel(1, 0, image::Rgba([0, 0, 0, 0]));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png)
        .expect("encode");
    out.into_inner()
}

/// Every start of a slicer the window asked for: (program, arguments). Nothing is started.
static STARTS: std::sync::Mutex<Vec<(String, Vec<String>)>> = std::sync::Mutex::new(Vec::new());

fn record_start(command: std::process::Command) -> bool {
    STARTS.lock().expect("the record").push((
        command.get_program().to_string_lossy().into_owned(),
        command
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect(),
    ));
    true
}

fn main() {
    support::answer_list("model3d_ui");
    // This test shows a window: always offscreen, whatever the session's platform is.
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("QT_QPA_PLATFORM", "offscreen") };
    // SAFETY: as above.
    unsafe { std::env::set_var("QT_FORCE_STDERR_LOGGING", "1") };
    check_the_tint();

    // Open in Slicer: the only applications directory a fixture run reads is this one, holding a
    // fake OrcaSlicer whose program exists (`/bin/true`) and is NEVER run: every start goes
    // through the seam below, which records the command.
    let apps = std::env::temp_dir().join(format!("kourier-model3d-ui-apps-{}", std::process::id()));
    std::fs::create_dir_all(&apps).expect("the applications directory");
    std::fs::write(
        apps.join("com.orcaslicer.OrcaSlicer.desktop"),
        "[Desktop Entry]\nType=Application\nName=OrcaSlicer\nExec=/bin/true %U\nMimeType=model/stl;model/3mf;\n",
    )
    .expect("the fake OrcaSlicer entry");
    // SAFETY: single-threaded test binary, before the app and Qt start.
    unsafe { std::env::set_var("KOURIER_DEV_APPLICATIONS", &apps) };
    kourier_qt::app::set_slicer_launcher(record_start);

    let app = App::start(StartOptions {
        launch: Launch::Fixture(FixtureOptions {
            messages_per_conversation: 6,
            models: true,
            ..FixtureOptions::default()
        }),
        ..StartOptions::default()
    })
    .expect("start the app");
    let handle = app.handle();
    let albums = handle
        .fixture()
        .and_then(|f| f.albums_conversation)
        .expect("the albums chat")
        .0;

    cxx_qt::init_crate!(kourier_qt);
    kourier_qt::set_context(UiContext {
        app: handle.clone(),
    });
    kourier_qt::set_options(UiOptions::default());
    let mut qt = QGuiApplication::new();

    // What the core's mesh gives the worker: two triangles, the first red and the second with no
    // colour of its own (it takes the object's fallback), in a box 2 x 3 x 4 mm.
    let feeder = std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let positions: [f32; 18] = [
            0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0, // counter-clockwise: up
            0.0, 0.0, 4.0, 0.0, 1.0, 4.0, 2.0, 3.0, 4.0, // clockwise: down
        ];
        let colors: [u32; 2] = [0xff00_00ff, 0];
        let built = qobject::build_model_buffers(
            &positions,
            &colors,
            0x8fa3_bfff,
            &[0.0, 0.0, 0.0],
            &[2.0, 3.0, 4.0],
        );
        // The first waits with a fallback of its own (#102030 in the QML), the worker hands the
        // lists over whichever colour it was given: the fallback is part of the build.
        let own = qobject::build_model_buffers(
            &positions,
            &colors,
            0x1020_30ff,
            &[0.0, 0.0, 0.0],
            &[2.0, 3.0, 4.0],
        );
        drop(built);
        qobject::deliver_model(5, own);
        qobject::fail_model(6);
    });

    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        kourier_qt::install_image_provider(engine.as_mut());
        engine.load_data(
            &QByteArray::from(SCRIPT.replace("ALBUMS_ID", &albums.to_string()).as_str()),
            &QUrl::from("qrc:/qt/qml/app/kourier/qml/model3d_ui_test.qml"),
        );
    }
    let code = qt.as_mut().map_or(1, |q| q.exec());
    drop(engine);
    feeder.join().expect("the feeder thread");
    kourier_qt::media::set_store(None);
    if code != 0 {
        println!(
            "test model3d_ui ... FAILED (exit {code})\n\ntest result: FAILED. 0 passed; 1 failed"
        );
        std::process::exit(1);
    }
    // The seam heard ONE start: the fake OrcaSlicer's program with the copy of the 3MF as its one
    // argument (an absolute path ending `.3mf`; "Other Application…" and the cancelled questions
    // started nothing), and the preference was written after it.
    let started = STARTS.lock().expect("the record").clone();
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0].0, "/bin/true");
    assert_eq!(started[0].1.len(), 1, "{started:?}");
    let copy = std::path::Path::new(&started[0].1[0]);
    assert!(copy.is_absolute(), "{started:?}");
    assert!(
        copy.extension().is_some_and(|e| e == "3mf"),
        "the copy is named by its type: {started:?}"
    );
    assert_eq!(handle.desktop().prefs().model_slicer, "orcaslicer");
    let _ = std::fs::remove_dir_all(&apps);
    app.shutdown();
    println!("test model3d_ui ... ok\n\ntest result: ok. 1 passed; 0 failed");
}
