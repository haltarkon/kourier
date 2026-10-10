//! The C++ side of the development control channel ([`crate::dev`]) (`cpp/devcontrol.cpp`): the root window, its
//! QML functions, its size, and leaving the event loop. build.rs compiles this bridge only when
//! debug assertions are on (it lives next to `bridge.rs`: cxx-qt wants every bridge of a QML
//! module in one directory).

// cxx drops the `# Safety` sections of extern functions from the code it generates (the same
// false positive the cxx-qt examples allow).
#![allow(clippy::missing_safety_doc)]

#[cxx_qt::bridge]
pub mod qt {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        /// Qt string.
        type QString = cxx_qt_lib::QString;
        include!(<QtCore/QObject>);
        /// Qt's object base class (a bridge without QObjects of its own must import it).
        #[namespace = ""]
        type QObject = cxx_qt::QObject;
    }

    #[namespace = "kourier::dev"]
    unsafe extern "C++" {
        include!("kourier-qt/cpp/devcontrol.h");

        /// The Kirigami root window, or null before QML created it.
        #[cxx_name = "mainWindow"]
        fn main_window() -> *mut QObject;

        /// Calls the QML function `method()` of `object`.
        ///
        /// # Safety
        ///
        /// `object` must be null or a live `QObject`.
        #[cxx_name = "invokeQml"]
        unsafe fn invoke_qml(object: *mut QObject, method: &QString) -> bool;

        /// Calls the QML function `method(string)` of `object`.
        ///
        /// # Safety
        ///
        /// `object` must be null or a live `QObject`.
        #[cxx_name = "invokeQmlString"]
        unsafe fn invoke_qml_string(object: *mut QObject, method: &QString, arg: &QString) -> bool;

        /// Calls the QML function `method(real)` of `object`.
        ///
        /// # Safety
        ///
        /// `object` must be null or a live `QObject`.
        #[cxx_name = "invokeQmlNumber"]
        unsafe fn invoke_qml_number(object: *mut QObject, method: &QString, arg: f64) -> bool;

        /// The QObject held by property `name` of `object`, or null.
        ///
        /// # Safety
        ///
        /// `object` must be null or a live `QObject`.
        #[cxx_name = "objectProperty"]
        unsafe fn object_property(object: *mut QObject, name: &QString) -> *mut QObject;

        /// Sets the boolean property `name` of `object`.
        ///
        /// # Safety
        ///
        /// `object` must be null or a live `QObject`.
        #[cxx_name = "setBoolProperty"]
        unsafe fn set_bool_property(object: *mut QObject, name: &QString, value: bool) -> bool;

        /// Resizes `window` (a `QWindow`).
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "resizeWindow"]
        unsafe fn resize_window(window: *mut QObject, width: i32, height: i32) -> bool;

        /// The size of `window` as `width * 65536 + height`, or -1.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "windowSize"]
        unsafe fn window_size(window: *mut QObject) -> i64;

        /// Presses the left button at `(x1, y1)` of `window`, moves to `(x2, y2)` in `steps`
        /// moves and releases there.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "dragMouse"]
        unsafe fn drag_mouse(
            window: *mut QObject,
            x1: f64,
            y1: f64,
            x2: f64,
            y2: f64,
            steps: i32,
        ) -> bool;

        /// Whether `window` (a `QWindow`) is visible.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "windowVisible"]
        unsafe fn window_visible(window: *mut QObject) -> bool;

        /// Closes `window` (a `QWindow`) like its close button.
        ///
        /// # Safety
        ///
        /// `window` must be null or a live `QObject`.
        #[cxx_name = "closeWindow"]
        unsafe fn close_window(window: *mut QObject);

        /// Leaves the Qt event loop with `code`.
        #[cxx_name = "quitApp"]
        fn quit_app(code: i32);
    }
}
