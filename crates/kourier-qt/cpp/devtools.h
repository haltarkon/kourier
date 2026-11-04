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

// How many images of `window` (a QQuickWindow) are loading: the items below its content item,
// popups and the viewer included (they live in the overlay), that are a QQuickImageBase (Image,
// AnimatedImage, BorderImage) with status Loading. -1 when the object is not a window.
int imagesLoading(QObject* window);

// What `imagesLoading` counted, in words: one entry per loading image (its name, its parent's,
// visible or hidden, its size, `source` and `sourceSize`), joined by "; ". Empty when none loads
// or the object is not a window. For the line `shot: unsettled images N [...]`.
QString loadingImages(QObject* window);

// How many visible images of `window` (a QQuickWindow) draw an attachment: a QQuickImageBase
// whose source is `image://kourier/thumb/…` or `image://kourier/att/…`, whatever its status.
// What `--screenshot` asks before it waits for the core's thumbnail derive: a window that shows
// none has nothing the derive could change. -1 when the object is not a window.
int attachmentPictures(QObject* window);

// Grabs `window` and compares the grab with the previous call's (kept here, one per process):
// equal -> saves it as a PNG at `path` and returns its mean luminance (as grabWindowToFile);
// different or the first -> keeps it and returns -2; -1 when the object is not a window or
// saving fails. `--screenshot` runs: two equal frames in a row are the picture that is final.
int grabIfStable(QObject* window, const QString& path);

// The plain text QTextDocument makes of `html`: what a read-only TextEdit displays, with line
// separators as '\n' and non-breaking spaces as ' '.
QString richTextPlain(const QString& html);

// Moves the mouse pointer far outside every window, so headless screenshots show no hover
// state or tooltip.
void parkCursor();

// Recolors every link in a TextEdit's document (`textDocument`, a QQuickTextDocument): links
// in own bubbles take the bubble's ink instead of the palette's link color.
// A mention (the link `kourier:mention/<i>`) is not a link to look at: it keeps the look of the
// text (`ink`, bold from its markup), without the underline an anchor gets.
void colorLinks(QObject* textDocument, const QColor& color, const QColor& ink);

// How the links of `html` look after `colorLinks`, one line per link ("href fg underline bold"):
// what the tests of the mention's look read.
QString richTextLinkLooks(const QString& html, const QColor& color, const QColor& ink);

// Installs a Qt message handler that hands every message to `sink(level, category, text)`
// (level: 0 debug, 1 warning, 2 critical, 3 fatal, 4 info). Messages are UTF-8; the file and
// line of QML warnings are already part of their text.
void installMessageHandler(rust::Fn<void(int32_t, rust::Str, rust::Str)> sink);

} // namespace kourier
