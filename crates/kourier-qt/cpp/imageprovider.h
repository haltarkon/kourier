// The `image://kourier/…` image provider (architecture §8.6): attachments, thumbnails, avatars
// and blurhash placeholders, decrypted and decoded off the Qt thread. Each request runs on the
// provider's own thread pool: the Rust reader resolves the handle (a miss waits for the core's
// media read connection), decrypts the `kourier-blob` file into memory and hands the bytes back;
// QImageReader decodes them at the requested size. No logic beyond Qt calls lives here.
#pragma once

#include <QtCore/QString>
#include <QtCore/QStringList>
#include <QtGui/QImage>
#include <QtQml/QQmlApplicationEngine>

#include "rust/cxx.h"

namespace kourier {

// The Rust reader: `(id, width, height)` → a header plus payload (see `src/media.rs`):
// byte 0 format (0 nothing, 1 encoded image, 2 RGBA8888 pixels), byte 1 flags (1 = cut to a
// square with rounded corners); encoded: byte 2 the codec Rust told from the content (the only
// decoder used), bytes 3 and 4 the corner radius in pixels of the requested size (u16 LE), then
// the file; RGBA: bytes 2..10 width and height (u32 LE), then the pixels.
using MediaReader = rust::Fn<rust::Vec<uint8_t>(rust::Str, int32_t, int32_t)>;

// Registers the provider as `image://kourier` on `engine` (which takes ownership).
void installImageProvider(QQmlApplicationEngine& engine, MediaReader reader);

// Decodes what `reader` returned for one request at `requested` (an empty size keeps the
// natural size): encoded images are decoded by the payload's codec alone (never by detecting
// the format, which would let any installed image plugin read bytes from the network), scaled
// to cover the requested size without upscaling and with their EXIF orientation applied;
// avatars are cropped to a square with the payload's corner radius (half the side or more is a
// circle, 0 a plain square). Null on failure. Zeroes the payload once decoded. Exposed for
// tests.
QImage decodeMediaPayload(rust::Vec<uint8_t>& payload, const QSize& requested);

// Whether the clipboard holds an image; when it does, encodes it as PNG on a worker thread and
// calls `done(png)` there (Qt thread only).
bool pasteClipboardImage(rust::Fn<void(rust::Vec<uint8_t>)> done);

// Puts `png` on the clipboard as an image; false when it does not decode (tests of pasting).
bool setClipboardImagePng(rust::Vec<uint8_t> png);

// The local files the clipboard holds (what "Copy" in a file manager puts there), as absolute
// paths; empty when it holds none. Qt thread only.
QStringList clipboardFiles();

// Puts local files on the clipboard as a list of URLs (tests of pasting files).
void setClipboardFiles(const QStringList& paths);

// Puts `text` on the clipboard ("Copy Text" of a message). Qt thread only.
void setClipboardText(const QString& text);

// The clipboard's text (tests of copying).
QString clipboardText();

// Decodes `bytes` as an avatar request with the payload codec byte `codec` at `side` x `side`
// and returns the alpha of its corner pixel * 1000 + the alpha of its center pixel (round
// avatars: 255), or -1 when nothing was decoded (tests).
int32_t roundAvatarProbe(rust::Vec<uint8_t> bytes, uint8_t codec, int32_t side);

// The same with a corner radius of `radius` pixels: the alpha of the corner pixel << 16, of the
// pixel a tenth of the side in from the corner on the diagonal << 8, and of the center pixel
// (or -1). A circle: 0, 0, 255; a square with small corners: 0, 255, 255; a plain square:
// 255, 255, 255 (tests).
int32_t avatarCornerProbe(rust::Vec<uint8_t> bytes, uint8_t codec, int32_t side, int32_t radius);

// A gradient PNG of `width` x `height` (fixture screenshots of the composer).
rust::Vec<uint8_t> samplePng(int32_t width, int32_t height, int32_t hue);

// The same picture in another format Qt can write (`jpeg`, `webp`, `bmp`, `avif`, `jxl`, …);
// empty when it cannot (tests of the decoders).
rust::Vec<uint8_t> sampleImage(int32_t width, int32_t height, int32_t hue, rust::Str format);

} // namespace kourier
