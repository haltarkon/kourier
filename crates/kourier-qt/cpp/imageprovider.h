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

// How many requests of the provider are not finished (queued or being decoded). A screenshot
// waits for 0: a view that was re-created while its request was in flight shows no "loading"
// of its own, but the request is still counted here.
int pendingImageRequests();

// Test probe: while on, every request the provider gets is logged as `id WxH` (the size Qt asked
// for: `sourceSize` times the device pixel ratio). Turning it on or off clears the log.
void logImageRequests(bool on);

// The logged requests, in the order they came; empties the log.
rust::Vec<rust::String> takeImageRequestLog();

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

// A picture made of fine detail, for tests of how sharp the timeline draws a picture: bands
// of black and white stripes (8 px and 6 px wide, vertical, then horizontal), a big and a
// small line of text, and thin diagonals, all in the picture's native pixels (the bands
// follow its height). A smooth gradient looks the same stretched 1.5 times; this does not. `format` is
// a Qt format name (`jpeg` is written at quality 92, `png`); empty when it cannot be written
// or a side is not 1 … 8192.
rust::Vec<uint8_t> detailImage(int32_t width, int32_t height, rust::Str format);

// How close the picture the app drew is to the file it comes from (tests of picture quality).
// `grab` is a PNG of the item as the app showed it, in physical pixels; `file` is decoded by the
// decoder of `codec` (the payload's codec byte). Two references are made of the file, each cut
// in the middle to the grab's size: the provider's own answer for a request of that size
// (`decodeMediaPayload`: what the item draws, one to one, when the FILE is served), and the
// file decoded whole and scaled to cover it by Qt's smooth scaling (nothing of the provider in
// it). Returns, in this order: the COARSE peak signal-to-noise ratio (dB, luma, after averaging
// in blocks of 8 x 8 pixels: it tells the right picture at the right place from a wrong one
// and ignores the stripes, which a half-pixel crop moves; 99 for identical) of the grab
// against the provider's answer, the variance of the luma's Laplacian of the grab and of the
// smooth reference (their ratio says how much of the file's detail the grab keeps whatever
// the alignment: a picture stretched from a smaller one keeps a fraction), the grab's width
// and height, the coarse PSNR of the grab against the smooth reference, and the variance of
// the Laplacian of the provider's answer. Empty when either does not decode.
rust::Vec<double> pictureProbe(rust::Vec<uint8_t> file, uint8_t codec, rust::Vec<uint8_t> grab);


// A JPEG of `width` x `height` pixels as stored, left half red and right half blue, carrying the
// EXIF orientation tag `orientation` (1 to 8: 6 = shown rotated by 90 degrees, 8 = by 270) with
// the pixels as stored, as a phone writes it; empty when it cannot be written (tests of the
// decoder).
rust::Vec<uint8_t> orientedImage(int32_t width, int32_t height, int32_t orientation);

// Decodes `file` (codec byte as in `src/media.rs`) as the provider does for a request of
// `width` x `height`, cut to a square when `cornered`: `[width, height, pictures scaled after
// the decoder had decoded them whole (0 when the decoder scaled), the colour in the middle of
// the top tenth, in the middle of the bottom tenth, the pixels the decoder made]`, or empty
// (tests of the decoder).
rust::Vec<int32_t> decodeProbe(rust::Vec<uint8_t> file, uint8_t codec, int32_t width, int32_t height, bool cornered);

// `[width, height, transparent area of the top left quarter in pixels]` of the PNG `grab` of a
// cornered avatar (the sum of 1 - alpha/255: a quarter circle of radius r has r^2 (1 - pi/4));
// empty when it does not decode (tests of picture quality).
rust::Vec<double> cornerProbe(rust::Vec<uint8_t> grab);

} // namespace kourier
