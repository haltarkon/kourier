//! The route of a file (W5b, decisions G6, G7, G9; brief §2.3): ONE table that decides, from the core's kind, the
//! file's MIME type and size and what Kourier read inside it, how the file goes: the message's SHAPE
//! (`imageMessage`, `videoMessage`, `audioMessage`, `documentMessage`), and with it the upload type (the path
//! `/mms/<kind>`) AND the HKDF type of the encryption, which are therefore the same value ([`Shape::media_type`]) and
//! cannot disagree (a blob encrypted under one type and announced as another is one nobody can decrypt).
//!
//! THE RULE IS THE CONTAINER, NOT THE MIME ALONE (G6): `kourier_media::sniff_mime` calls every MP4 brand it does not
//! know `video/mp4`, and an Ogg is Vorbis, Opus or Theora. So:
//!
//! | Core kind | When | Shape |
//! |---|---|---|
//! | `Image` (not `image/gif`) | at most [`PICTURE_LIMIT`]; cleaned by `kourier_media::clean_still`, its cleaned bytes decode and are at most [`MEDIA_LIMIT`] | image (re-encoded: G7, G7b) |
//! | `Image` | larger (streamed untouched), not a cleaned type (HEIC, AVIF: as it is), only stripped (it does not decode) or a larger cleaned picture | document |
//! | `Video` / `Audio` in MP4 (`video/mp4`, `audio/mp4`) | at most [`MEDIA_LIMIT`]; the box walk finds a video track in `avc1`/`avc3` | video |
//! | the same | no video track, the first sound track `mp4a` | audio (`audio/mp4`) |
//! | the same | any other codec, no track that reads, larger | document |
//! | `Audio` `audio/ogg` | at most [`MEDIA_LIMIT`]; its first packet is `OpusHead` | audio (`audio/ogg; codecs=opus`) |
//! | `Audio` `audio/mpeg` | at most [`MEDIA_LIMIT`]; its first bytes are an `ID3` tag or an MPEG frame sync | audio |
//! | `Video`, `Audio` | any other container (WebM, QuickTime, FLAC, WAV, raw AAC ...) | document |
//! | `Gif` | always (his answer: "As a file") | document |
//! | `File` ("send as file") | always; bytes untouched | document (the caption wrapper when it has a caption: the builder) |
//! | `Voice` | never made by the composer (G9) | not built |
//!
//! A file is ADAPTED, never refused: every row ends in a document when its media form does not fit (the CLAUDE.md
//! limits rule). The limits are WhatsApp Web's defaults [U2] with rows in architecture §13. Pure: what is read inside
//! the file (the re-encode, the box walk, the Ogg head) is the prepare step's, [`look`] says which.

use kourier_model::AttachmentKind;
use wacore::download::MediaType;

use super::mp4::Facts;

/// The largest file sent as a picture, video or audio message, in bytes of plaintext (WhatsApp Web's "16 MB"
/// default for media; whether MB or MiB is unknown [U2], the smaller reading is the safe one). A larger file goes as a
/// document.
pub(crate) const MEDIA_LIMIT: u64 = 16_000_000;

/// The largest file of any kind (G5): `features.files` states every kind at it (a file over [`MEDIA_LIMIT`]
/// is adapted to a document, never refused). Below WhatsApp's 2 GiB constant with room [U2]; a broken upload of a
/// large file restarts from zero (no derived key: a named known limit).
pub(crate) const FILE_LIMIT: u64 = 2_000_000_000;

/// The largest picture read whole and cleaned (G7: the core's own `MAX_PICTURE_BYTES`, above which it imports a
/// picture streamed and undecoded; the bound of what WhatsApp states it cleans, `upload::stated_cleaning`); a larger
/// one is routed as a document and streamed untouched.
pub(crate) const PICTURE_LIMIT: u64 = 64 * 1024 * 1024;

/// Bytes at the start of an Ogg file enough to see its first packet (a page header of 27 bytes, up to 255 lacing
/// values, the eight letters of `OpusHead`).
pub(crate) const OGG_HEAD: usize = 27 + 255 + 8;

/// Bytes at the start of an MP3 file enough to tell an `ID3` tag (three letters) or a frame header (two bytes) apart.
pub(crate) const MPEG_HEAD: usize = 3;

/// The message a file is sent as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// `imageMessage`.
    Image,
    /// `videoMessage`.
    Video,
    /// `audioMessage` (`ptt` false).
    Audio,
    /// `documentMessage` (inside `documentWithCaptionMessage` when it has a caption).
    Document,
}

impl Shape {
    /// The upload type and the HKDF type of the shape: one value for both.
    pub(crate) fn media_type(self) -> MediaType {
        match self {
            Shape::Image => MediaType::Image,
            Shape::Video => MediaType::Video,
            Shape::Audio => MediaType::Audio,
            Shape::Document => MediaType::Document,
        }
    }
}

/// How a file goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    /// The message's shape (and so its upload and HKDF type).
    pub shape: Shape,
    /// The mimetype written instead of the file's own, when the shape needs another (an Opus Ogg, MP4 audio).
    pub mime: Option<&'static str>,
}

impl Route {
    fn plain(shape: Shape) -> Self {
        Self { shape, mime: None }
    }

    /// The upload type and the HKDF type.
    pub(crate) fn media_type(&self) -> MediaType {
        self.shape.media_type()
    }

    /// The mimetype the message carries: the route's, else the file's (a re-encoded picture's is its re-encode's,
    /// which the caller passes).
    pub(crate) fn mimetype<'a>(&self, given: &'a str) -> &'a str {
        self.mime.unwrap_or(given)
    }
}

/// What the prepare step must read inside a file before it can be routed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Look {
    /// Nothing: the route is decided.
    Nothing(Shape),
    /// Read the picture whole and clean it (G7, G7b: `upload::still`): [`Probe::Still`].
    Still,
    /// Walk its MP4 boxes: [`Probe::Mp4`].
    Mp4,
    /// Read its first [`OGG_HEAD`] bytes: [`Probe::Ogg`].
    Ogg,
    /// Read its first [`MPEG_HEAD`] bytes: [`Probe::Mpeg`] (fix F9: an empty or mislabelled `.mp3` is no audio).
    Mpeg,
}

/// What the prepare step found, as [`look`] asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Probe {
    /// Nothing was read.
    Nothing,
    /// The cleaned picture: its length, `None` when it does not decode (`upload::Still::opens`).
    Still(Option<u64>),
    /// The box walk's answer, `None` when the file has no `moov` that reads.
    Mp4(Option<Facts>),
    /// Whether the Ogg's first packet is `OpusHead` ([`opus_head`]).
    Ogg { opus: bool },
    /// Whether the file starts like an MP3 ([`mpeg_head`]).
    Mpeg { mp3: bool },
}

/// The MIME type without its parameters, in lower case.
fn essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// What to read inside a file of this kind, type and size (plaintext bytes) before routing it; `None` for a kind
/// W5b does not build (a voice note: the composer makes none, G9).
pub(crate) fn look(kind: AttachmentKind, mime: &str, size: u64) -> Option<Look> {
    let mime = essence(mime);
    let media = size <= MEDIA_LIMIT;
    Some(match kind {
        AttachmentKind::Voice => return None,
        AttachmentKind::File | AttachmentKind::Gif => Look::Nothing(Shape::Document),
        AttachmentKind::Image if mime == "image/gif" || size > PICTURE_LIMIT => {
            Look::Nothing(Shape::Document)
        }
        AttachmentKind::Image => Look::Still,
        AttachmentKind::Video | AttachmentKind::Audio if !media => Look::Nothing(Shape::Document),
        AttachmentKind::Video | AttachmentKind::Audio => match mime.as_str() {
            "video/mp4" | "audio/mp4" | "audio/x-m4a" | "audio/m4a" => Look::Mp4,
            "audio/ogg" if kind == AttachmentKind::Audio => Look::Ogg,
            "audio/mpeg" | "audio/mp3" if kind == AttachmentKind::Audio => Look::Mpeg,
            _ => Look::Nothing(Shape::Document),
        },
    })
}

/// The route of a file: [`look`]'s answer completed by what the prepare step found. A probe that does not answer
/// the look (or found nothing usable) routes the file as a document.
pub(crate) fn route(kind: AttachmentKind, mime: &str, size: u64, probe: &Probe) -> Option<Route> {
    Some(match (look(kind, mime, size)?, probe) {
        (Look::Nothing(shape), _) => Route::plain(shape),
        (Look::Still, Probe::Still(Some(len))) if *len <= MEDIA_LIMIT => Route::plain(Shape::Image),
        (Look::Mp4, Probe::Mp4(Some(facts))) => mp4_route(facts),
        (Look::Ogg, Probe::Ogg { opus: true }) => Route {
            shape: Shape::Audio,
            mime: Some("audio/ogg; codecs=opus"),
        },
        (Look::Mpeg, Probe::Mpeg { mp3: true }) => Route::plain(Shape::Audio),
        _ => Route::plain(Shape::Document),
    })
}

/// The route of a file sent WITH a caption: an `audioMessage` carries none, so a captioned file whose route is audio
/// (a core `Video` that holds only sound) goes as a document, which keeps both (B10; the core sends an `Audio`
/// file's caption as a text of its own, so this meets only such a video).
pub(crate) fn captioned(route: Route) -> Route {
    if route.shape == Shape::Audio {
        Route::plain(Shape::Document)
    } else {
        route
    }
}

/// An MP4 by its tracks: H.264 video plays on every phone; audio-only AAC is an audio message; anything else (HEVC,
/// AV1, a video track whose codec did not read, no track at all) goes as a document.
fn mp4_route(facts: &Facts) -> Route {
    match facts.video {
        // A video says it is one even when the core typed the file as audio (an `audio/mp4` brand holding an H.264
        // track): a `videoMessage` with an audio mimetype is a combination no phone sends (fix F10).
        Some(track) if matches!(track.codec.as_ref(), Some(b"avc1" | b"avc3")) => Route {
            shape: Shape::Video,
            mime: Some("video/mp4"),
        },
        Some(_) => Route::plain(Shape::Document),
        None if facts.audio.as_ref() == Some(b"mp4a") => Route {
            shape: Shape::Audio,
            mime: Some("audio/mp4"),
        },
        None => Route::plain(Shape::Document),
    }
}

/// Whether an Ogg file's first packet is `OpusHead`: the first page (`OggS`, version 0, the first-page flag) and its
/// first packet's eight bytes after the lacing values.
pub(crate) fn opus_head(head: &[u8]) -> bool {
    if head.len() < 27 || &head[..4] != b"OggS" || head[4] != 0 || head[5] & 0x02 == 0 {
        return false;
    }
    let packet = 27 + usize::from(head[26]);
    head.get(packet..packet + 8) == Some(b"OpusHead".as_slice())
}

/// Whether a file starts like an MP3 (fix F9): an `ID3` tag, or an MPEG audio frame header (eleven bits of sync, a
/// version and a layer that are not the reserved values: a raw AAC stream's ADTS header has layer 0 and is no MP3).
/// An empty file, a text and any other bytes the core typed `audio/mpeg` by its name are no audio message.
pub(crate) fn mpeg_head(head: &[u8]) -> bool {
    if head.starts_with(b"ID3") {
        return true;
    }
    match head {
        [0xFF, second, ..] => {
            second & 0xE0 == 0xE0 && (second >> 3) & 0b11 != 0b01 && (second >> 1) & 0b11 != 0
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
