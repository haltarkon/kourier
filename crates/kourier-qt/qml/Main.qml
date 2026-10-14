pragma ComponentBehavior: Bound

import QtQuick
import org.kde.kwindowsystem
import app.kourier

// The application shell (§8.2 step 6, §8.8): owns the main window. Closing it hides it to the
// tray when a tray icon is shown; after a minute hidden the window is destroyed (the engine and
// the models stay) and created again on activation. Activation comes from notification clicks,
// a second launch, the tray, or `sgnl://` links, with an XDG activation token so KWin lets the
// window come to the front on Wayland.
QtObject {
    id: shell

    property MainWindow window: null
    property bool quitting: false
    readonly property bool trayAvailable: App.trayStatus === 1 && App.settings.tray

    readonly property Component windowComponent: Component {
        MainWindow {
            trayAvailable: shell.trayAvailable
            quitting: shell.quitting
            onHiddenToTray: shell.releaseTimer.restart()
            onClosedForGood: shell.quit()
        }
    }

    // The main window, created when needed.
    function ensureWindow(): MainWindow {
        shell.releaseTimer.stop();
        if (shell.window === null) {
            shell.window = shell.windowComponent.createObject(null) as MainWindow;
            // What was told while there was no window waits in it until it is shown.
            if (shell.window !== null && shell.pendingLogoutNotices.length > 0) {
                shell.window.logoutNotices += shell.pendingLogoutNotices.length;
                shell.window.pendingLogoutNotices = shell.window.pendingLogoutNotices.concat(shell.pendingLogoutNotices);
                shell.pendingLogoutNotices = [];
                // (Shown once the window is up: on the next turn of the event loop, after the
                // caller has shown it, or when it shows.)
                Qt.callLater(shell.window.tellLogoutNotices);
            }
        }
        return shell.window;
    }

    // Shows and raises the window; opens `conversation` when it is not 0.
    function activate(token: string, conversation: real): void {
        const w = shell.ensureWindow();
        if (token !== "") {
            KWindowSystem.setCurrentXdgActivationToken(token);
        }
        if (w.visibility === Window.Minimized) {
            w.showNormal();
        } else {
            w.show();
        }
        w.raise();
        KWindowSystem.activateWindow(w);
        w.openFromActivation(conversation);
    }

    // Tray click: hide a window the user is looking at, else bring it up.
    function toggle(token: string): void {
        const w = shell.window;
        const looking = w !== null && w.visible && w.visibility !== Window.Minimized && (w.active || Date.now() - w.deactivatedAt < 600);
        if (looking && shell.trayAvailable) {
            w.hideToTray();
        } else {
            shell.activate(token, 0);
        }
    }

    function quit(): void {
        shell.quitting = true;
        Qt.quit();
    }

    // Notices about removed or unlinked accounts told while no window existed (link
    // feedback L5): handed to the next window, which shows them once it is visible.
    property var pendingLogoutNotices: []

    // The window that is being released: set until the event loop has really deleted it.
    property MainWindow releasing: null
    // How many times a hidden window was released (its memory handed back).
    property int releases: 0
    // How many times the core was asked to let go of its caches for a release.
    property int coreReleases: 0

    // Destroys the window after a minute hidden in the tray (§8.2 step 6).
    property Timer releaseTimer: Timer {
        interval: App.releaseDelayMs
        onTriggered: {
            if (shell.window !== null && !shell.window.visible && shell.releasing === null) {
                // Nothing is on screen from here on. Said now and not when the memory is
                // handed back: an activation in between makes a new window, and what that
                // window says about its focus and its chat must stand.
                App.setPresence(false, false);
                // Nothing needs to be fast for a while: the core lets go of what it only
                // keeps for speed. Asked now, so that it is free by the time the heap is
                // trimmed (`settleTimer`); it reads it again when it is next needed.
                if (App.windowReleasing()) {
                    shell.coreReleases += 1;
                }
                // Its notices not shown yet outlive it, and so does the one whose dialog is open
                // (a dialog dies with its window; the user did not answer it): link feedback L5.
                shell.pendingLogoutNotices = shell.pendingLogoutNotices.concat(shell.window.unshownLogoutNotices());
                shell.releasing = shell.window;
                shell.window = null;
                shell.releasing.destroy();
                shell.settleTimer.restart();
            }
        }
    }

    // `destroy()` only schedules the deletion: the window's items are freed when the event
    // loop comes round to it. The memory is handed back to the system after that (and after
    // the JavaScript heap let go of what the window held): a trim before it finds nothing
    // free, and what is freed a moment later stays with the process (§1.3: idle memory while
    // hidden). `releasing` reads null once the window object is gone. Only memory is dealt
    // with here: by now the user may be back, with a new window on screen.
    property Timer settleTimer: Timer {
        interval: 50
        onTriggered: {
            if (shell.releasing !== null) {
                shell.settleTimer.restart();
                return;
            }
            gc();
            App.windowReleased();
            shell.releases += 1;
        }
    }

    property Connections desktop: Connections {
        target: App
        function onActivationRequested(token: string, conversation: real): void {
            shell.activate(token, conversation);
        }
        function onToggleRequested(token: string): void {
            shell.toggle(token);
        }
        function onQuitRequested(): void {
            shell.quit();
        }
        // What became of a removed or unlinked account's device, told while no window exists
        // (released in the tray, or a `--background` start): kept for the next window. A
        // window that exists takes it itself (`MainWindow.qml`).
        function onLogoutNotice(text: string, onPhone: string, title: string): void {
            if (shell.window !== null) {
                return;
            }
            shell.pendingLogoutNotices.push({
                "text": text,
                "onPhone": onPhone,
                "title": title
            });
        }
        function onTrayStatusChanged(): void {
            // Never stay invisible without a tray to come back from.
            if (App.trayStatus === 0 && (shell.window === null || !shell.window.visible)) {
                shell.activate("", 0);
            }
        }
    }

    Component.onCompleted: {
        if (App.startHidden) {
            // `--background`: no window yet; the backends connect anyway (§8.2 step 4).
            App.ready();
        } else {
            shell.ensureWindow().show();
        }
    }
}
