//! What the map knows about the media a message carries (W3b, chunks C4a and C4b). PURE: no I/O, no clock, no
//! logging.
//!
//! C4a: the glimpse a quote shows of a quoted file. C4b: the attachments themselves, as REMOTE POINTERS: a
//! picture, video, video note, voice message, audio file, document and sticker become an
//! [`Attachment`] in state `Remote` whose `remote` is a [`MediaPointer`] (nothing is downloaded before W7).
//! The pointer holds exactly what upstream's `DownloadParams` needs (W7 rebuilds it from the pointer) plus
//! the inline thumbnail, which would be lost otherwise: the inbox content is wiped at stage 2. A file with
//! no `directPath` or no `mediaKey` cannot be downloaded: it is `Unavailable` with no pointer.
//!
//! The limits are Signal's (`kourier-signal/src/receive/media.rs`), so the core meets the same bounds
//! from both networks: a content type or a file name that is longer than the core takes is as good as
//! none, never a failed row.

use std::fmt;

use kourier_model::{Attachment, AttachmentKind, AttachmentState, BackendBlob};
use serde::{Deserialize, Serialize};
use waproto::buffa::MessageField;
use waproto::whatsapp as wa;

use crate::map::class::Content;

/// The version of the pointer's payload (`BackendBlob::v`).
pub(crate) const POINTER_V: u16 = 1;
/// Most waveform bars kept (WhatsApp sends 64).
pub(crate) const MAX_WAVEFORM: usize = 100;
/// The largest inline thumbnail kept (WhatsApp sends a few kilobytes).
const MAX_THUMB: usize = 64 * 1024;
/// The longest `directPath` kept.
const MAX_DIRECT_PATH: usize = 2048;
/// The longest media key kept (WhatsApp's are 32 bytes).
const MAX_MEDIA_KEY: usize = 64;
/// The longest static URL kept.
const MAX_STATIC_URL: usize = 2048;

/// The longest content type kept (RFC 6838 allows 255 bytes in all).
pub(crate) const MAX_MIME: usize = 255;
/// The longest file name kept (the file systems' limit).
pub(crate) const MAX_FILE_NAME: usize = 255;

/// A content type as a message states it: kept when it is not empty, not too long and free of control
/// characters (it is shown and compared as text), else none.
pub(crate) fn kept_mime(mime: Option<&str>) -> Option<String> {
    mime.filter(|m| !m.is_empty() && m.len() <= MAX_MIME && !m.chars().any(char::is_control))
        .map(str::to_owned)
}

/// A file name as a message states it: kept when it is not empty and not too long. Not log-safe.
pub(crate) fn kept_file_name(name: Option<&str>) -> Option<String> {
    name.filter(|n| !n.is_empty() && n.len() <= MAX_FILE_NAME)
        .map(str::to_owned)
}

/// What a quote shows of a quoted message's file: its kind, content type and name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Glimpse {
    pub(crate) kind: AttachmentKind,
    pub(crate) mime: Option<String>,
    /// Not log-safe.
    pub(crate) file_name: Option<String>,
}

/// The file of a (peeled) message, when it carries one: a picture, a video or video note, a GIF, a
/// voice message or audio file, a document, a sticker. A contact, a location and a poll carry none.
pub(crate) fn glimpse(base: &wa::Message) -> Option<Glimpse> {
    let glimpse = |kind, mime: Option<&str>, file_name: Option<&str>| Glimpse {
        kind,
        mime: kept_mime(mime),
        file_name: kept_file_name(file_name),
    };
    if let Some(m) = base.image_message.as_option() {
        return Some(glimpse(AttachmentKind::Image, m.mimetype.as_deref(), None));
    }
    if let Some(m) = base.video_message.as_option() {
        let kind = if m.gif_playback == Some(true) {
            AttachmentKind::Gif
        } else {
            AttachmentKind::Video
        };
        return Some(glimpse(kind, m.mimetype.as_deref(), None));
    }
    if let Some(m) = base.ptv_message.as_option() {
        return Some(glimpse(AttachmentKind::Video, m.mimetype.as_deref(), None));
    }
    if let Some(m) = base.audio_message.as_option() {
        let kind = if m.ptt == Some(true) {
            AttachmentKind::Voice
        } else {
            AttachmentKind::Audio
        };
        return Some(glimpse(kind, m.mimetype.as_deref(), None));
    }
    if let Some(m) = base.document_message.as_option() {
        return Some(glimpse(
            AttachmentKind::File,
            m.mimetype.as_deref(),
            m.file_name.as_deref(),
        ));
    }
    if let Some(m) = base.sticker_message.as_option() {
        return Some(glimpse(AttachmentKind::Image, m.mimetype.as_deref(), None));
    }
    None
}

/// Lower-case hex of `bytes` (the pointer's and the names' form; no new dependency).
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// What a download of the file needs (W7 builds upstream's `DownloadParams` from it), kept in the
/// attachment's `remote` as a [`BackendBlob`] of version [`POINTER_V`] holding this struct as JSON.
/// Binary fields are lower-case HEX strings (the crate has no base64 dependency). `Debug` prints the
/// media type and the length only: the media key is a secret.
///
/// W5b: the pointer of a file KOURIER sent (`send::upload::Prepared::pointer`, kept in the upload record of
/// backend.db, `send::record`) also holds what the sent message said that the core's attachment does not know: its
/// `url`, `streaming_sidecar`, `mimetype` (a re-encoded picture's), `width`/`height`, `seconds` and `shape`, so a
/// resend and a caption edit write exactly the sent file ([`file_message`]). Every one is optional: a pointer of
/// W3b (a file from the phone) reads as before, and `POINTER_V` stays 1.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct MediaPointer {
    /// `image`, `video`, `ptv` (a video note), `audio`, `document` or `sticker`: upstream's media type.
    pub(crate) media_type: String,
    pub(crate) direct_path: String,
    /// Hex of the media key.
    pub(crate) media_key: String,
    /// Hex of the SHA-256 of the plaintext file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) file_sha256: Option<String>,
    /// Hex of the SHA-256 of the encrypted file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) file_enc_sha256: Option<String>,
    pub(crate) file_length: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) media_key_timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) static_url: Option<String>,
    /// Hex of the inline JPEG (a sticker: PNG) thumbnail; W7 turns it into the attachment's thumbnail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thumb_jpeg: Option<String>,
    /// The file's URL as the upload answered it (W5b; never logged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
    /// Hex of the streaming sidecar of a video or audio file (W5b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) streaming_sidecar: Option<String>,
    /// The mimetype the sent message carried (W5b: a re-encoded picture's differs from the core's file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mimetype: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) height: Option<u32>,
    /// Whole seconds (W5b: an MP4's box walk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) seconds: Option<u32>,
    /// How a file Kourier sent was written (W5b): `image`, `video`, `audio`, `document` or [`CAPTIONED_DOCUMENT`]
    /// (a document inside `documentWithCaptionMessage`). None for a file from the phone: its `media_type` decides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) shape: Option<String>,
}

/// The [`MediaPointer::shape`] of a document sent with a caption: inside `documentWithCaptionMessage`, as the
/// phones send it [U7]. Also set on receive for a document the phone sent that way (fix F11).
pub(crate) const CAPTIONED_DOCUMENT: &str = "document_captioned";

impl fmt::Debug for MediaPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "MediaPointer {{ media_type: {}, file_length: {} }}",
            self.media_type, self.file_length
        )
    }
}

impl MediaPointer {
    /// The pointer as the attachment's `remote`.
    fn blob(&self) -> BackendBlob {
        // Strings and numbers only: the encoding cannot fail.
        BackendBlob::new(POINTER_V, serde_json::to_vec(self).unwrap_or_default())
    }

    /// Reads a pointer back (a caption edit, W5a; W7 reads it the same way).
    pub(crate) fn from_blob(blob: &BackendBlob) -> Option<Self> {
        (blob.v == POINTER_V)
            .then(|| serde_json::from_slice(&blob.bytes).ok())
            .flatten()
    }
}

/// Lower-case (or upper-case) hex back to bytes; `None` for anything else.
fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// What the core says about a file: what [`file_message`] writes where the pointer holds nothing (a file from the
/// phone: W3b's pointer has no mimetype, no dims, no seconds).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileFacts<'a> {
    pub(crate) kind: AttachmentKind,
    pub(crate) mime: &'a str,
    /// Not log-safe.
    pub(crate) file_name: Option<&'a str>,
    pub(crate) dims: Option<(u32, u32)>,
    pub(crate) duration_ms: Option<u32>,
}

impl<'a> FileFacts<'a> {
    /// The facts of the core's attachment.
    pub(crate) fn of(attachment: &'a Attachment) -> Self {
        Self {
            kind: attachment.kind,
            mime: &attachment.mime,
            file_name: attachment.file_name.as_deref(),
            dims: attachment.dims,
            duration_ms: attachment.duration_ms,
        }
    }
}

/// THE ONE BUILDER of a file message (W5b, brief §2.3), for the send AND the caption edit: the message that carries
/// the file `p` names, with `caption` (a caption edit's new content; an edit never changes the file, so the file is
/// written as its pointer names it, and a resend under the same id writes the same message: every time in it is the
/// pointer's). The inverse of [`file_of`] (`media::tests::every_built_file_maps_back`). The pointer's own fields win
/// over the core's `facts` (a sent picture's mimetype and size are its re-encode's). The media fields are copied by
/// hand: upstream's builders are plain field copies that take no stored record.
///
/// `None` for a pointer whose key does not read, a kind with no message here (a sticker), and an audio file WITH a
/// caption (an `audioMessage` has none: the route sends a captioned file that would be audio as a document).
pub(crate) fn file_message(
    p: &MediaPointer,
    facts: &FileFacts<'_>,
    caption: Option<String>,
) -> Option<wa::Message> {
    let media_key = Some(unhex(&p.media_key)?);
    let sha = |hash: &Option<String>| hash.as_deref().and_then(unhex);
    let thumb = p.thumb_jpeg.as_deref().and_then(unhex);
    let sidecar = p.streaming_sidecar.as_deref().and_then(unhex);
    let mime = Some(p.mimetype.clone().unwrap_or_else(|| facts.mime.to_owned()));
    let (width, height) = match (p.width, p.height) {
        (Some(w), Some(h)) => (Some(w), Some(h)),
        _ => facts.dims.unzip(),
    };
    let seconds = p.seconds.or_else(|| facts.duration_ms.map(|ms| ms / 1000));
    let direct_path = Some(p.direct_path.clone());
    let mut message = wa::Message::default();
    match p.shape.as_deref().unwrap_or(&p.media_type) {
        "image" => {
            let mut m = wa::message::ImageMessage::default();
            m.url.clone_from(&p.url);
            m.mimetype = mime;
            m.caption = caption;
            m.direct_path = direct_path;
            m.media_key = media_key;
            m.file_sha256 = sha(&p.file_sha256);
            m.file_enc_sha256 = sha(&p.file_enc_sha256);
            m.file_length = Some(p.file_length);
            m.media_key_timestamp = p.media_key_timestamp;
            m.static_url.clone_from(&p.static_url);
            m.jpeg_thumbnail = thumb;
            m.width = width;
            m.height = height;
            message.image_message = MessageField::some(m);
        }
        kind @ ("video" | "ptv") => {
            let mut m = wa::message::VideoMessage::default();
            m.url.clone_from(&p.url);
            m.mimetype = mime;
            m.caption = caption;
            m.direct_path = direct_path;
            m.media_key = media_key;
            m.file_sha256 = sha(&p.file_sha256);
            m.file_enc_sha256 = sha(&p.file_enc_sha256);
            m.file_length = Some(p.file_length);
            m.media_key_timestamp = p.media_key_timestamp;
            m.static_url.clone_from(&p.static_url);
            m.jpeg_thumbnail = thumb;
            m.streaming_sidecar = sidecar;
            m.width = width;
            m.height = height;
            m.seconds = seconds;
            if facts.kind == AttachmentKind::Gif {
                m.gif_playback = Some(true);
            }
            if kind == "ptv" {
                message.ptv_message = MessageField::some(m);
            } else {
                message.video_message = MessageField::some(m);
            }
        }
        "audio" => {
            if caption.is_some() {
                return None;
            }
            let mut m = wa::message::AudioMessage::default();
            m.url.clone_from(&p.url);
            m.mimetype = mime;
            m.direct_path = direct_path;
            m.media_key = media_key;
            m.file_sha256 = sha(&p.file_sha256);
            m.file_enc_sha256 = sha(&p.file_enc_sha256);
            m.file_length = Some(p.file_length);
            m.media_key_timestamp = p.media_key_timestamp;
            m.streaming_sidecar = sidecar;
            m.seconds = seconds;
            m.ptt = Some(false);
            message.audio_message = MessageField::some(m);
        }
        shape @ ("document" | CAPTIONED_DOCUMENT) => {
            let mut m = wa::message::DocumentMessage::default();
            m.url.clone_from(&p.url);
            m.mimetype = mime;
            m.caption = caption;
            m.file_name = facts.file_name.map(str::to_owned);
            // A file Kourier sent names its title as the phones do; a phone's file is written as W3b read it (its
            // pointer has no mimetype, W3b reads none, even when F11 gave it the wrapper's shape).
            if p.shape.is_some() && p.mimetype.is_some() {
                m.title.clone_from(&m.file_name);
            }
            m.direct_path = direct_path;
            m.media_key = media_key;
            m.file_sha256 = sha(&p.file_sha256);
            m.file_enc_sha256 = sha(&p.file_enc_sha256);
            m.file_length = Some(p.file_length);
            m.media_key_timestamp = p.media_key_timestamp;
            m.jpeg_thumbnail = thumb;
            if shape == CAPTIONED_DOCUMENT {
                let mut inner = wa::Message::default();
                inner.document_message = MessageField::some(m);
                let mut wrapper = wa::message::FutureProofMessage::default();
                wrapper.message = MessageField::some(inner);
                message.document_with_caption_message = MessageField::some(wrapper);
            } else {
                message.document_message = MessageField::some(m);
            }
        }
        _ => return None,
    }
    Some(message)
}

/// The context info of a file message built by [`file_message`] (a quote and mentions go there), inside the caption
/// wrapper of a document.
pub(crate) fn file_context(
    message: &mut wa::Message,
) -> Option<&mut MessageField<wa::ContextInfo>> {
    if let Some(m) = message.image_message.as_option_mut() {
        return Some(&mut m.context_info);
    }
    if let Some(m) = message.video_message.as_option_mut() {
        return Some(&mut m.context_info);
    }
    if let Some(m) = message.ptv_message.as_option_mut() {
        return Some(&mut m.context_info);
    }
    if let Some(m) = message.audio_message.as_option_mut() {
        return Some(&mut m.context_info);
    }
    if let Some(m) = message.document_message.as_option_mut() {
        return Some(&mut m.context_info);
    }
    let inner = message
        .document_with_caption_message
        .as_option_mut()?
        .message
        .as_option_mut()?;
    Some(&mut inner.document_message.as_option_mut()?.context_info)
}

/// Writes the chat's timer into the document inside a caption wrapper (fix F8): upstream's
/// `set_ephemeral_expiration` looks at the top-level carriers of `contextInfo` only and answers `false` (with a warning
/// line) for `documentWithCaptionMessage`, so the message would go out without its timer; WhatsApp Web puts the timer
/// in the inner document's context. `false` for any other message (upstream's call, `SendOptions`, handles those).
pub(crate) fn put_timer_inside_wrapper(message: &mut wa::Message, seconds: u32) -> bool {
    if message.document_with_caption_message.as_option().is_none() {
        return false;
    }
    let Some(context) = file_context(message) else {
        return false;
    };
    context.get_or_insert_default().expiration = Some(seconds);
    true
}

/// One file of a message, read off the wire message.
#[derive(Default)]
struct Fields<'a> {
    kind: AttachmentKind,
    media_type: &'static str,
    mime: Option<&'a str>,
    file_name: Option<&'a str>,
    direct_path: Option<&'a str>,
    media_key: Option<&'a [u8]>,
    file_sha256: Option<&'a [u8]>,
    file_enc_sha256: Option<&'a [u8]>,
    file_length: Option<u64>,
    media_key_timestamp: Option<i64>,
    static_url: Option<&'a str>,
    thumb: Option<&'a [u8]>,
    width: Option<u32>,
    height: Option<u32>,
    seconds: Option<u32>,
    waveform: Option<&'a [u8]>,
    /// The shape a phone's file came in, when the pointer must remember it (fix F11: a captioned document).
    shape: Option<&'static str>,
}

fn octet_stream() -> String {
    "application/octet-stream".to_owned()
}

/// A hash is kept when it has the length of a SHA-256 (a shorter or longer one is no hash).
fn sha256(bytes: Option<&[u8]>) -> Option<&[u8]> {
    bytes.filter(|b| b.len() == 32)
}

impl Fields<'_> {
    /// The pointer, when a download can be made of the file.
    fn pointer(&self) -> Option<MediaPointer> {
        let direct_path = self
            .direct_path
            .filter(|p| !p.is_empty() && p.len() <= MAX_DIRECT_PATH)?;
        let media_key = self
            .media_key
            .filter(|k| !k.is_empty() && k.len() <= MAX_MEDIA_KEY)?;
        Some(MediaPointer {
            media_type: self.media_type.to_owned(),
            direct_path: direct_path.to_owned(),
            media_key: hex(media_key),
            file_sha256: sha256(self.file_sha256).map(hex),
            file_enc_sha256: sha256(self.file_enc_sha256).map(hex),
            file_length: self.file_length.unwrap_or(0),
            media_key_timestamp: self.media_key_timestamp.filter(|t| *t > 0),
            static_url: self
                .static_url
                .filter(|u| !u.is_empty() && u.len() <= MAX_STATIC_URL)
                .map(str::to_owned),
            thumb_jpeg: self
                .thumb
                .filter(|t| !t.is_empty() && t.len() <= MAX_THUMB)
                .map(hex),
            shape: self.shape.map(str::to_owned),
            ..MediaPointer::default()
        })
    }

    fn attachment(&self) -> Attachment {
        let pointer = self.pointer();
        Attachment {
            kind: self.kind,
            mime: kept_mime(self.mime).unwrap_or_else(octet_stream),
            size: self.file_length.unwrap_or(0),
            file_name: kept_file_name(self.file_name),
            dims: self
                .width
                .zip(self.height)
                .filter(|(w, h)| *w > 0 && *h > 0),
            duration_ms: self
                .seconds
                .filter(|s| *s > 0)
                .map(|s| s.saturating_mul(1000)),
            waveform: self
                .waveform
                .filter(|w| !w.is_empty())
                .map(|w| w[..w.len().min(MAX_WAVEFORM)].to_vec()),
            // A file no download can be made of is not available on this device (no dead "try again").
            state: if pointer.is_some() {
                AttachmentState::Remote
            } else {
                AttachmentState::Unavailable
            },
            remote: pointer.as_ref().map(MediaPointer::blob),
            // The names the network has for the file: the hash of its plaintext, prefixed so a later
            // delete or W7's check can tell the names of one attachment apart (critic L9).
            net_ids: sha256(self.file_sha256)
                .map(|h| format!("sha256:{}", hex(h)))
                .into_iter()
                .collect(),
            ..Attachment::default()
        }
    }
}

/// The file of a message and the caption beside it.
pub(crate) struct File {
    pub(crate) attachment: Attachment,
    /// The caption as typed (WhatsApp's markup not read yet).
    pub(crate) caption: Option<String>,
}

/// The file of a (peeled) message of the given content kind, as a remote pointer. `None` for a kind that has
/// no file or a message that lacks the field its class names.
pub(crate) fn file_of(base: &wa::Message, content: Content) -> Option<File> {
    file_in(base, content, false)
}

/// [`file_of`] of a message that may have come inside a caption wrapper: `wrapped` (a document the phone sent with a
/// caption, fix F11) is kept in the pointer's `shape`, so a caption edit writes the wrapper the phone sent.
pub(crate) fn file_in(base: &wa::Message, content: Content, wrapped: bool) -> Option<File> {
    let (fields, caption) = match content {
        Content::Image => {
            let m = base.image_message.as_option()?;
            (
                Fields {
                    kind: AttachmentKind::Image,
                    media_type: "image",
                    mime: m.mimetype.as_deref(),
                    direct_path: m.direct_path.as_deref(),
                    media_key: m.media_key.as_deref(),
                    file_sha256: m.file_sha256.as_deref(),
                    file_enc_sha256: m.file_enc_sha256.as_deref(),
                    file_length: m.file_length,
                    media_key_timestamp: m.media_key_timestamp,
                    static_url: m.static_url.as_deref(),
                    thumb: m.jpeg_thumbnail.as_deref(),
                    width: m.width,
                    height: m.height,
                    ..Fields::default()
                },
                m.caption.as_deref(),
            )
        }
        Content::Video | Content::VideoNote => {
            let note = content == Content::VideoNote;
            let m = if note {
                base.ptv_message.as_option()?
            } else {
                base.video_message.as_option()?
            };
            let kind = if !note && m.gif_playback == Some(true) {
                AttachmentKind::Gif
            } else {
                AttachmentKind::Video
            };
            (
                Fields {
                    kind,
                    media_type: if note { "ptv" } else { "video" },
                    mime: m.mimetype.as_deref(),
                    direct_path: m.direct_path.as_deref(),
                    media_key: m.media_key.as_deref(),
                    file_sha256: m.file_sha256.as_deref(),
                    file_enc_sha256: m.file_enc_sha256.as_deref(),
                    file_length: m.file_length,
                    media_key_timestamp: m.media_key_timestamp,
                    static_url: m.static_url.as_deref(),
                    thumb: m.jpeg_thumbnail.as_deref(),
                    width: m.width,
                    height: m.height,
                    seconds: m.seconds,
                    ..Fields::default()
                },
                m.caption.as_deref(),
            )
        }
        Content::Audio => {
            let m = base.audio_message.as_option()?;
            let voice = m.ptt == Some(true);
            (
                Fields {
                    kind: if voice {
                        AttachmentKind::Voice
                    } else {
                        AttachmentKind::Audio
                    },
                    media_type: "audio",
                    mime: m.mimetype.as_deref(),
                    direct_path: m.direct_path.as_deref(),
                    media_key: m.media_key.as_deref(),
                    file_sha256: m.file_sha256.as_deref(),
                    file_enc_sha256: m.file_enc_sha256.as_deref(),
                    file_length: m.file_length,
                    media_key_timestamp: m.media_key_timestamp,
                    seconds: m.seconds,
                    // The waveform is a voice message's; an audio file has none worth drawing.
                    waveform: voice.then_some(m.waveform.as_deref()).flatten(),
                    ..Fields::default()
                },
                None,
            )
        }
        Content::Document => {
            let m = base.document_message.as_option()?;
            (
                Fields {
                    kind: AttachmentKind::File,
                    media_type: "document",
                    mime: m.mimetype.as_deref(),
                    file_name: m.file_name.as_deref(),
                    direct_path: m.direct_path.as_deref(),
                    media_key: m.media_key.as_deref(),
                    file_sha256: m.file_sha256.as_deref(),
                    file_enc_sha256: m.file_enc_sha256.as_deref(),
                    file_length: m.file_length,
                    media_key_timestamp: m.media_key_timestamp,
                    thumb: m.jpeg_thumbnail.as_deref(),
                    shape: wrapped.then_some(CAPTIONED_DOCUMENT),
                    ..Fields::default()
                },
                m.caption.as_deref(),
            )
        }
        Content::Sticker => {
            let m = base.sticker_message.as_option()?;
            (
                Fields {
                    kind: AttachmentKind::Image,
                    media_type: "sticker",
                    // Stickers are WebP; a message that states none is still one.
                    mime: m
                        .mimetype
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .or(Some("image/webp")),
                    direct_path: m.direct_path.as_deref(),
                    media_key: m.media_key.as_deref(),
                    file_sha256: m.file_sha256.as_deref(),
                    file_enc_sha256: m.file_enc_sha256.as_deref(),
                    file_length: m.file_length,
                    media_key_timestamp: m.media_key_timestamp,
                    thumb: m.png_thumbnail.as_deref(),
                    width: m.width,
                    height: m.height,
                    ..Fields::default()
                },
                None,
            )
        }
        Content::Contact | Content::Location | Content::LiveLocation | Content::Poll => {
            return None;
        }
    };
    Some(File {
        attachment: fields.attachment(),
        caption: caption.filter(|c| !c.is_empty()).map(str::to_owned),
    })
}

#[cfg(test)]
mod tests;
