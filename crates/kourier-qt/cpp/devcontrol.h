// Qt helpers of the development control channel (`kourier --dev-control`, debug builds only;
// build.rs compiles this file only when debug assertions are on). They reach the root window
// the way a user does: through its QML functions and its size. No logic beyond Qt calls.
#pragma once

#include <QtCore/QObject>
#include <QtCore/QString>

#include "rust/cxx.h"

namespace kourier::dev {

// The application's QQuickWindow (the Kirigami root window: the one without a transient
// parent), or null before QML created it.
QObject* mainWindow();

// Calls the QML function `method()` of `object`; false when it has no such function.
bool invokeQml(QObject* object, const QString& method);

// Calls the QML function `method(string)` of `object`.
bool invokeQmlString(QObject* object, const QString& method, const QString& arg);

// Calls the QML function `method(real)` of `object`.
bool invokeQmlNumber(QObject* object, const QString& method, double arg);

// The QObject held by property `name` of `object` (for example `globalDrawer`), or null.
QObject* objectProperty(QObject* object, const QString& name);

// Sets the boolean property `name` of `object`; false when it has no such property.
bool setBoolProperty(QObject* object, const QString& name, bool value);

// Resizes `window` (a QWindow) to `width` x `height` pixels; false when it is not a window.
bool resizeWindow(QObject* window, int width, int height);

// The current size of `window` as width * 65536 + height, or -1 when it is not a window.
int64_t windowSize(QObject* window);

// Presses the left mouse button at (x1, y1) of `window` (a QWindow), moves to (x2, y2) in
// `steps` moves and releases there; false when it is not a window.
bool dragMouse(QObject* window, double x1, double y1, double x2, double y2, int steps);

// Whether `window` (a QWindow) is visible; false when it is not a window.
bool windowVisible(QObject* window);

// Closes `window` (a QWindow) like its close button: a close event QML may refuse.
void closeWindow(QObject* window);

// Leaves the event loop with `code` (QCoreApplication::exit).
void quitApp(int code);

} // namespace kourier::dev
