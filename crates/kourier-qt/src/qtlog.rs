//! Qt and QML messages into `tracing` (§9.1): one handler for every thread, installed by
//! [`crate::run`] once logging is up. Targets are `qt` (and the Qt category as a field), so
//! `KOURIER_LOG=qt=debug` shows Qt's debug output. Known noise is dropped.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::bridge::qobject;

/// The UI has ended ([`end`]): Qt's messages are dropped from here on.
static ENDED: AtomicBool = AtomicBool::new(false);

/// Routes Qt's messages into `tracing` from now on.
pub fn install() {
    ENDED.store(false, Ordering::Relaxed);
    qobject::install_message_handler(on_message);
}

/// The Qt event loop is over and the UI is gone: messages Qt still writes are dropped.
///
/// Qt goes on talking while the process exits (a plugin is unloaded, a thread store is
/// cleaned up), from static destructors that run after the main thread's thread-local storage
/// is gone. `tracing` keeps its dispatcher there, so a message at that point panicked inside
/// the handler, and a panic in a function C++ calls aborts the process: an exit that had
/// already succeeded ended in an abort and, in a debug build, a core dump.
pub fn end() {
    ENDED.store(true, Ordering::Relaxed);
}

/// Messages that are expected and harmless: KF 6.30's `KI18nContext` installs an event filter
/// across threads once per process (§13, phase B).
pub fn is_noise(text: &str) -> bool {
    text.contains("installEventFilter") && text.contains("different thread")
}

fn on_message(level: i32, category: &str, text: &str) {
    if ENDED.load(Ordering::Relaxed) || is_noise(text) {
        return;
    }
    match level {
        0 => tracing::debug!(target: "qt", category, "{text}"),
        4 => tracing::info!(target: "qt", category, "{text}"),
        1 => tracing::warn!(target: "qt", category, "{text}"),
        _ => tracing::error!(target: "qt", category, "{text}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn nothing_is_logged_once_the_ui_has_ended() {
        use std::sync::atomic::Ordering;
        // (The handler itself needs no Qt: it is a plain function Qt is given.)
        super::ENDED.store(false, Ordering::Relaxed);
        super::on_message(1, "default", "a warning while the UI runs");
        super::end();
        assert!(super::ENDED.load(Ordering::Relaxed));
        // Reaches no logger (at process exit that would be a panic inside a C++ callback).
        super::on_message(1, "default", "a message from a static destructor");
        super::ENDED.store(false, Ordering::Relaxed);
    }

    #[test]
    fn ki18n_thread_warning_is_noise() {
        assert!(super::is_noise(
            "QObject::installEventFilter(): Cannot filter events for objects in a different thread."
        ));
        assert!(!super::is_noise(
            "qrc:/qt/qml/app/kourier/qml/Main.qml:3:5: Unable to assign"
        ));
    }
}
