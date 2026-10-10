// Desktop integration helpers for the Kourier UI (architecture §8.7, §8.8): the taskbar badge,
// quit-on-last-window, tray icon and notification avatar pixels, and returning freed heap
// memory to the system after the window is released. No logic beyond Qt calls.
#pragma once

#include <QtCore/QString>

#include <cstdint>

#include "rust/cxx.h"

namespace kourier {

// The taskbar badge (QGuiApplication::setBadgeNumber; the Unity LauncherEntry API on Linux,
// shown by Plasma's task manager). 0 hides it.
void setBadgeNumber(int64_t number);

// Whether closing the last window quits the application (Kourier quits explicitly instead,
// so a window hidden to the tray or released never ends the process).
void setQuitOnLastWindowClosed(bool quit);

// Renders the bundled icon at `path` (a qrc URL or path of an SVG) as a `size`×`size` image,
// with a red attention dot when `dot` is set. Returns ARGB32
// bytes in network order (A, R, G, B per pixel), as StatusNotifierItem pixmaps want them;
// empty on failure. Qt thread only (QIcon, QPixmap).
rust::Vec<uint8_t> renderIconArgb(const QString& path, int32_t size, bool dot);

// Renders an avatar for `name` like Kirigami Add-ons' Avatar does in the app: the NameUtils
// color of the name (of its last four characters when `anonymous`) with the name's initials in
// white, or a person glyph for anonymous names and names unsuitable for initials. The shape is
// a square with corners of `corner` pixels; half the side or more is a circle. Returns
// `size`×`size` RGBA8888 bytes. Uses only QImage and QPainter (Qt 6 renders text in any
// thread), so it may run off the Qt thread.
rust::Vec<uint8_t> renderAvatarRgba(const QString& name, bool anonymous, int32_t size, int32_t corner);

// Returns free heap memory to the system (glibc malloc_trim(0)).
void trimMemory();

} // namespace kourier
