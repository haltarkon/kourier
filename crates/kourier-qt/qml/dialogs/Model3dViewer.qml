pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick3D
import QtQuick3D.Helpers
import org.kde.kirigami as Kirigami
import app.kourier

// The 3D viewer (queue item 3): the ONLY file that imports Quick 3D. It is created by URL on the
// first open (`MainWindow.openModelViewer`), never at start, so the module is not resolved before
// a model is looked at (§1.3: idle memory). Full window like the picture viewer: Escape closes.
// The mesh comes from `ModelData` (built off the Qt thread) and is pushed into the
// `ProceduralMesh` from C++; the lists are dropped when the viewer closes.
//
// Turntable by default (left drag = yaw around the vertical axis, pitch clamped to +-89 degrees,
// up stays up), "Free Rotation" (trackball: quaternion); wheel zooms to the cursor; right or
// middle drag pans; double click fits. The model is Z up (the printer convention) and stays put:
// the camera does not move, the model turns.
QQC2.Popup {
    id: root
    objectName: "model3dViewer"

    property real attachment: 0
    property string name: ""
    property bool saveOffered: false
    property bool tint: true
    readonly property bool fallback: false
    readonly property alias freeRotation: header.freeRotation
    // "Edges over the model": the shader of `shaders/wire.frag` instead of the solid material.
    property alias wireframe: header.wireframe
    // The model could not be opened (the core refused it, or there is no core).
    property bool openFailed: false
    readonly property alias meshData: modelData

    // The turntable's angles, the free rotation's quaternion, the pan and the distance.
    property real yaw: -35
    property real pitch: 25
    property quaternion trackball: Quaternion.fromEulerAngles(0, 0, 0)
    property real panX: 0
    property real panY: 0
    property real distance: 100
    readonly property real fov: 35
    readonly property real radius: Math.max(0.001, Math.hypot(modelData.boxX, modelData.boxY, modelData.boxZ) / 2)
    readonly property real halfTan: Math.tan(root.fov * Math.PI / 360)

    // The chosen printer box (the preference `model_box`): its row of the viewmodel's table, or
    // null. Its floor sits on the model's lowest point and it is centred on the model in X and Y;
    // the model never moves. Only this one box is ever drawn.
    readonly property var box: {
        const rows = header.boxRows;
        for (let i = 0; i < rows.length; i++) {
            if (rows[i].id === header.boxId) {
                return rows[i];
            }
        }
        return null;
    }
    // The box's twelve edges as thin bars: [x, y, z, sx, sy, sz] in mm around the box's centre.
    readonly property var boxEdges: {
        const b = root.box;
        if (!b) {
            return [];
        }
        const t = Math.max(0.6, Math.max(b.w, b.d, b.h) * 0.004);
        const out = [];
        for (const sy of [-1, 1]) {
            for (const sz of [-1, 1]) {
                out.push([0, sy * b.d / 2, sz * b.h / 2, b.w + t, t, t]);
            }
        }
        for (const sx of [-1, 1]) {
            for (const sz of [-1, 1]) {
                out.push([sx * b.w / 2, 0, sz * b.h / 2, t, b.d + t, t]);
            }
        }
        for (const sx of [-1, 1]) {
            for (const sy of [-1, 1]) {
                out.push([sx * b.w / 2, sy * b.d / 2, 0, t, t, b.h + t]);
            }
        }
        return out;
    }

    // Turns "Free Rotation" on or off (the page that walks through the viewer's states).
    function setFreeRotation(on: bool): void {
        header.freeRotation = on;
    }

    function save(): void {
        if (root.saveOffered && root.attachment > 0) {
            MediaActions.save([root.attachment]);
        }
    }

    // The distance that shows the whole model, and with a printer box chosen the whole box too
    // (the turn's centre stays the model's: the box's farthest corner from it sets the radius).
    function fitDistance(): real {
        // (Read from the data, not from `radius`: fit() runs in the same readyChanged that updates it.)
        let r = Math.max(0.001, Math.hypot(modelData.boxX, modelData.boxY, modelData.boxZ) / 2);
        const b = root.box;
        if (b) {
            // The box's floor is on the model's lowest point: it spans -boxZ/2 .. h - boxZ/2.
            r = Math.max(r, Math.hypot(b.w / 2, b.d / 2, Math.max(modelData.boxZ / 2, b.h - modelData.boxZ / 2)));
        }
        return r / Math.sin(root.fov * Math.PI / 360) * 1.1;
    }

    // The still's view: the front three quarters from above, the whole model (and box) in view.
    function fit(): void {
        root.yaw = -35;
        root.pitch = 25;
        root.trackball = root.turntableRotation();
        root.panX = 0;
        root.panY = 0;
        root.distance = root.fitDistance();
    }

    // A box chosen or put away: step back (or in) so all of it is in view; the angles and the pan stay.
    onBoxChanged: {
        if (modelData.ready) {
            root.distance = root.fitDistance();
        }
    }

    // A standard view: the model seen along an axis (Z up, X to the right in the front view), or the
    // still's angle (`iso`). The distance and the pan are those of `fit`.
    function showView(which: string): void {
        const angles = {
            "front": [0, 0],
            "back": [180, 0],
            "left": [90, 0],
            "right": [-90, 0],
            "top": [0, 90],
            "bottom": [0, -90],
            "iso": [-35, 25]
        };
        const a = angles[which];
        if (!a) {
            return;
        }
        root.fit();
        root.yaw = a[0];
        root.pitch = a[1];
        root.trackball = root.turntableRotation();
    }

    // Z up -> the scene's Y up, then yaw around the vertical, then the pitch toward the viewer.
    function turntableRotation(): quaternion {
        const up = Quaternion.fromEulerAngles(-90, 0, 0);
        const turn = Quaternion.fromEulerAngles(0, root.yaw, 0);
        const tilt = Quaternion.fromEulerAngles(root.pitch, 0, 0);
        return tilt.times(turn).times(up);
    }

    function rotate(dx: real, dy: real): void {
        if (root.freeRotation) {
            const angle = Math.hypot(dx, dy) * 0.4;
            if (angle > 0) {
                const turn = Quaternion.fromAxisAndAngle(Qt.vector3d(dy, dx, 0).normalized(), angle);
                root.trackball = turn.times(root.trackball);
            }
        } else {
            root.yaw += dx * 0.4;
            root.pitch = Math.max(-89, Math.min(89, root.pitch + dy * 0.4));
            root.trackball = root.turntableRotation();
        }
    }

    // `ndcX`, `ndcY`: the cursor in -1 … 1 over the view; the point under it stays under it.
    function zoom(steps: real, ndcX: real, ndcY: real, aspect: real): void {
        // (The far limit leaves room to see a chosen printer box around a small model.)
        const next = Math.max(root.radius * 0.2, Math.min(Math.max(root.radius * 40, root.fitDistance() * 2), root.distance * Math.pow(0.9985, steps)));
        const k = root.halfTan * (root.distance - next);
        root.panX += ndcX * k * aspect;
        root.panY += ndcY * k;
        root.distance = next;
    }

    function pan(dx: real, dy: real, viewHeight: real): void {
        const perPixel = 2 * root.distance * root.halfTan / Math.max(1, viewHeight);
        root.panX -= dx * perPixel;
        root.panY += dy * perPixel;
    }

    onFreeRotationChanged: {
        // Leaving the trackball for the turntable: keep the angles, derive them anew from where it is.
        if (!root.freeRotation) {
            root.trackball = root.turntableRotation();
        }
    }

    parent: QQC2.Overlay.overlay
    x: 0
    y: 0
    width: parent ? parent.width : 0
    height: parent ? parent.height : 0
    modal: true
    focus: true
    padding: 0
    closePolicy: QQC2.Popup.CloseOnEscape

    Kirigami.Theme.colorSet: Kirigami.Theme.Complementary
    Kirigami.Theme.inherit: false

    background: Rectangle {
        color: Qt.alpha(Kirigami.Theme.backgroundColor, 0.92)
    }

    ModelData {
        id: modelData
        attachment: root.attachment
        fallback: Style.modelColour.toString()
        onReadyChanged: {
            if (modelData.ready) {
                modelData.pushTo(mesh);
                root.fit();
            }
        }
    }

    contentItem: Item {
        id: stage
        focus: true
        Keys.onPressed: event => {
            if (event.matches(StandardKey.Save)) {
                root.save();
                event.accepted = true;
            }
        }

        View3D {
            id: view
            objectName: "view3d"
            anchors.fill: parent
            anchors.topMargin: header.height
            environment: SceneEnvironment {
                backgroundMode: SceneEnvironment.Transparent
                antialiasingMode: SceneEnvironment.MSAA
                antialiasingQuality: SceneEnvironment.High
            }

            Node {
                id: pivot
                rotation: root.trackball
                Model {
                    objectName: "model3d"
                    // The model's box centre sits in the middle of the turn.
                    position: Qt.vector3d(-modelData.centerX, -modelData.centerY, -modelData.centerZ)
                    geometry: ProceduralMesh {
                        id: mesh
                    }
                    // Solid: PrincipledMaterial. Wireframe: the same face shading from the
                    // engine's lights through a CustomMaterial that darkens the triangle edges.
                    materials: root.wireframe ? [wireMaterial] : [solidMaterial]
                }
                // The printer box: faces at low alpha (unlit, both sides) and its edges as thin
                // unlit bars. Z up like the model; the floor on the model's lowest point.
                Node {
                    id: boxNode
                    objectName: "model3dBoxNode"
                    visible: root.box !== null && modelData.ready
                    position: Qt.vector3d(0, 0, root.box ? -modelData.boxZ / 2 + root.box.h / 2 : 0)
                    Model {
                        objectName: "model3dBoxFaces"
                        source: "#Cube"
                        scale: root.box ? Qt.vector3d(root.box.w / 100, root.box.d / 100, root.box.h / 100) : Qt.vector3d(1, 1, 1)
                        materials: PrincipledMaterial {
                            lighting: PrincipledMaterial.NoLighting
                            baseColor: Kirigami.Theme.highlightColor
                            opacity: 0.13
                            alphaMode: PrincipledMaterial.Blend
                            cullMode: Material.NoCulling
                        }
                    }
                    Repeater3D {
                        model: root.boxEdges.length
                        Model {
                            required property int index
                            readonly property var bar: root.boxEdges[index]
                            source: "#Cube"
                            position: Qt.vector3d(bar[0], bar[1], bar[2])
                            scale: Qt.vector3d(bar[3] / 100, bar[4] / 100, bar[5] / 100)
                            materials: PrincipledMaterial {
                                lighting: PrincipledMaterial.NoLighting
                                baseColor: Kirigami.Theme.textColor
                                cullMode: Material.NoCulling
                            }
                        }
                    }
                }
                PrincipledMaterial {
                    id: solidMaterial
                    baseColor: modelData.coloured ? "white" : Style.modelColour
                    vertexColorsEnabled: modelData.coloured
                    roughness: 0.55
                    metalness: 0
                    cullMode: Material.NoCulling
                }
                CustomMaterial {
                    id: wireMaterial
                    property color baseColor: Style.modelColour
                    shadingMode: CustomMaterial.Shaded
                    cullMode: Material.NoCulling
                    fragmentShader: modelData.coloured ? "qrc:/qt/qml/app/kourier/shaders/wirecolor.frag" : "qrc:/qt/qml/app/kourier/shaders/wire.frag"
                }
            }

            PerspectiveCamera {
                position: Qt.vector3d(root.panX, root.panY, root.distance)
                fieldOfView: root.fov
                // (Far out, around a printer box, the near plane moves out too: depth precision.)
                clipNear: Math.max(0.01, root.radius * 0.01, root.distance * 0.002)
                clipFar: (root.distance + root.radius) * 4
            }
            DirectionalLight {
                eulerRotation.x: -35
                eulerRotation.y: -30
                brightness: 1.0
                // A face turned away from both lights stays readable (the still's floor is ~0.45).
                ambientColor: Qt.rgba(0.38, 0.38, 0.38, 1)
            }
            DirectionalLight {
                eulerRotation.x: -10
                eulerRotation.y: 150
                brightness: 0.35
            }
        }

        MouseArea {
            id: mouse
            objectName: "model3dMouse"
            anchors.fill: view
            acceptedButtons: Qt.LeftButton | Qt.RightButton | Qt.MiddleButton
            property point last
            onPressed: event => {
                mouse.last = Qt.point(event.x, event.y);
            }
            onPositionChanged: event => {
                const dx = event.x - mouse.last.x;
                const dy = event.y - mouse.last.y;
                mouse.last = Qt.point(event.x, event.y);
                if (event.buttons & Qt.LeftButton) {
                    root.rotate(dx, dy);
                } else if (event.buttons & (Qt.RightButton | Qt.MiddleButton)) {
                    root.pan(dx, dy, mouse.height);
                }
            }
            onWheel: wheel => {
                root.zoom(wheel.angleDelta.y, (wheel.x / mouse.width) * 2 - 1, 1 - (wheel.y / mouse.height) * 2, mouse.width / Math.max(1, mouse.height));
            }
            onDoubleClicked: root.fit()
        }

        Spinner {
            anchors.centerIn: parent
            running: !modelData.ready && !modelData.failed && !root.openFailed
        }

        Kirigami.PlaceholderMessage {
            anchors.centerIn: parent
            width: parent.width - Kirigami.Units.gridUnit * 8
            visible: modelData.failed || root.openFailed
            icon.name: "image-missing"
            text: L10n.i18nc("@info:placeholder", "The model could not be opened")
        }

        Model3dHeader {
            id: header
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            name: root.name
            attachment: root.attachment
            saveOffered: root.saveOffered
            threeD: true
            sizeText: modelData.ready ? L10n.i18nc("@info size of a 3D model in millimetres: width, depth, height", "%1 × %2 × %3 mm", Number(modelData.boxX.toFixed(modelData.boxX < 10 ? 1 : 0)), Number(modelData.boxY.toFixed(modelData.boxY < 10 ? 1 : 0)), Number(modelData.boxZ.toFixed(modelData.boxZ < 10 ? 1 : 0))) : ""
            detailText: modelData.ready ? L10n.i18ncp("@info:tooltip, %1 the number of triangles", "%1 triangle", "%1 triangles", modelData.triangles) : ""
            onViewSelected: which => root.showView(which)
            onSaveRequested: root.save()
            onCloseRequested: root.close()
        }
    }

    onOpened: {
        stage.forceActiveFocus();
        root.fit();
        if (!App.openModel(root.attachment)) {
            root.openFailed = true;
        }
    }
    // The lists and the core's mesh go with the viewer.
    onClosed: modelData.clear()
}
