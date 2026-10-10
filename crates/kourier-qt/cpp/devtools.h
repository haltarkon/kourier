// Small Qt helpers for the Kourier UI: full-window screenshots (`--screenshot`), link colors
// in message text, a rich-text probe for tests, and the Qt message handler that routes Qt/QML
// messages into Rust `tracing`. No logic beyond Qt calls.
#pragma once

#include <QtCore/QObject>
#include <QtCore/QString>
#include <QtGui/QColor>

#include "rust/cxx.h"

namespace kourier {

// Grabs `window` (must be a QQuickWindow) and saves it as a PNG at `path`. Returns the mean
// luminance (0 … 255) of the image, or -1 when the object is not a window or saving fails.
int grabWindowToFile(QObject* window, const QString& path);

// The plain text QTextDocument makes of `html`: what a read-only TextEdit displays, with line
// separators as '\n' and non-breaking spaces as ' '.
QString richTextPlain(const QString& html);

// Moves the mouse pointer far outside every window, so headless screenshots show no hover
// state or tooltip.
void parkCursor();

// Recolors every link in a TextEdit's document (`textDocument`, a QQuickTextDocument): links
// in own bubbles take the bubble's ink instead of the palette's link color.
void colorLinks(QObject* textDocument, const QColor& color);

// Installs a Qt message handler that hands every message to `sink(level, category, text)`
// (level: 0 debug, 1 warning, 2 critical, 3 fatal, 4 info). Messages are UTF-8; the file and
// line of QML warnings are already part of their text.
void installMessageHandler(rust::Fn<void(int32_t, rust::Str, rust::Str)> sink);

} // namespace kourier
