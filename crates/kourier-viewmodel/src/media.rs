//! Media helpers (§8.6): `image://kourier/…` URLs of attachments, what the composer may attach
//! to a conversation, and a file's attachment kind guessed from its name.
//!
//! URLs carry only the core's attachment id and a revision (never a path, key or file name):
//! `image://kourier/{att,thumb,blurhash}/<attachment id>?v=<rev>`. The revision changes whenever
//! what the provider would decode changes (download state, size, type, dimensions, blurhash),
//! so QML's image cache never shows a stale picture.

use std::collections::BTreeMap;
use std::path::Path;

use kourier_model::{
    AskFailure, Attachment, AttachmentId, AttachmentKind, AttachmentState, FileLimits,
};

/// What the provider serves for an attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MediaVariant {
    /// The full file, decoded at the requested size.
    Full,
    /// The timeline's picture: the provider serves the 400 px thumbnail when it covers the
    /// size the item is drawn at, the file when it does not or when there is no thumbnail (the
    /// core decides, `MediaEntry::timeline_blob`).
    Thumb,
    /// The blurhash placeholder.
    Blurhash,
}

impl MediaVariant {
    /// The URL path segment (`att`, `thumb`, `blurhash`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "att",
            Self::Thumb => "thumb",
            Self::Blurhash => "blurhash",
        }
    }
}

/// FNV-1a over `bytes` (stable across runs, unlike `DefaultHasher`).
fn fnv(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(hash, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// The revision of an attachment's URLs: changes whenever the decoded picture can change.
///
/// Whether a thumbnail exists IS in it: a picture drawn from the file before its thumbnail
/// existed is the same picture, but the provider may serve other pixels once it does (the
/// thumbnail where it covers the size asked for, `MediaEntry::timeline_blob`; a decode from
/// the file and one from a thumbnail differ by a grey level or two), so the picture is asked
/// for again when the thumbnail appears. The timeline keeps the old pixels on screen until
/// the new ones are ready (`retainWhileLoading`), so the second request shows no flash.
pub fn revision(att: &Attachment) -> u32 {
    let state: u8 = match att.state {
        AttachmentState::Remote => 0,
        AttachmentState::Downloading { .. } => 1,
        AttachmentState::Local => 2,
        AttachmentState::Failed { .. } => 3,
        AttachmentState::Expired => 4,
        AttachmentState::Unavailable => 5,
        AttachmentState::Asking => 6,
        AttachmentState::AskFailed { why } => match why {
            AskFailure::NoAnswer => 7,
            AskFailure::NoMessage => 8,
            AskFailure::NoFile => 9,
        },
    };
    let (w, h) = att.dims.unwrap_or((0, 0));
    let mut x = fnv(0xcbf2_9ce4_8422_2325, &[state]);
    x = fnv(x, &att.size.to_le_bytes());
    x = fnv(x, &w.to_le_bytes());
    x = fnv(x, &h.to_le_bytes());
    x = fnv(x, att.mime.as_bytes());
    x = fnv(x, att.blurhash.as_deref().unwrap_or("").as_bytes());
    x = fnv(x, &[u8::from(att.thumb.is_some())]);
    // Fold to 32 bits; the value only has to change, not to be unique.
    (x ^ (x >> 32)) as u32
}

/// `image://kourier/<variant>/<id>?v=<rev>`.
pub fn media_url(variant: MediaVariant, id: AttachmentId, rev: u32) -> String {
    format!("image://kourier/{}/{}?v={rev}", variant.as_str(), id.0)
}

/// Picture, sound and video types whose plaintext copy the core names after the type
/// (`kourier-media`'s MIME table): another application then opens the copy as that type,
/// whatever the sender called the file.
const MEDIA_MIMES: &[&str] = &[
    "image/jpeg",
    "image/jpg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/bmp",
    "image/heic",
    "image/heif",
    "image/avif",
    "video/mp4",
    "video/quicktime",
    "video/webm",
    "video/x-matroska",
    "audio/mpeg",
    "audio/mp4",
    "audio/aac",
    "audio/ogg",
    "audio/flac",
    "audio/wav",
];

/// Whether a MIME type is a well-known picture, sound or video type (opened without a
/// confirmation; anything else asks first, §9.4). The type is the sender's claim: any other
/// type, `image/x-…` included, keeps the sender's file-name extension on the plaintext copy (a
/// "picture" called `page.html` would open in a browser), and SVG is a document that can carry
/// scripts and links, so those ask first.
pub fn is_media_mime(mime: &str) -> bool {
    let m = mime.split(';').next().unwrap_or("").trim();
    MEDIA_MIMES.iter().any(|t| t.eq_ignore_ascii_case(m))
}

/// The type and subtype of a MIME type, lower-case, without parameters.
fn essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Whether a sticker or GIF must be shown animated (`AnimatedImage` from a tmpfs copy, §8.6)
/// rather than as a still through the image provider: GIF files, and WebP stickers. A "GIF"
/// that is a video is a clip ([`is_clip`]), not a picture.
pub fn is_animated(att: &Attachment, sticker: bool) -> bool {
    let m = essence(&att.mime);
    (att.kind == AttachmentKind::Gif || sticker) && m == "image/gif" || sticker && m == "image/webp"
}

/// Whether an attachment is a video clip: a video, or a "GIF" that is one (Signal's GIFs are
/// MP4 clips with a GIF flag). No picture decoder plays those: until there is an in-app
/// video player they show their placeholder and open in another application.
pub fn is_clip(att: &Attachment) -> bool {
    att.kind == AttachmentKind::Video
        || att.kind == AttachmentKind::Gif && essence(&att.mime).starts_with("video/")
}

/// The attachment kind the core will most likely give a file, from its name (the core sniffs
/// the content; this is the composer's pre-check against the conversation's features).
pub fn kind_for_file_name(name: &str) -> AttachmentKind {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    match ext.as_str() {
        "gif" => AttachmentKind::Gif,
        "jpg" | "jpeg" | "png" | "webp" | "heic" | "heif" | "avif" | "bmp" | "jxl" => {
            AttachmentKind::Image
        }
        "mp4" | "m4v" | "mov" | "webm" | "mkv" | "avi" | "3gp" => AttachmentKind::Video,
        "mp3" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "flac" | "wav" => AttachmentKind::Audio,
        _ => AttachmentKind::File,
    }
}

/// The Breeze icon of a file to attach, from its name.
pub fn icon_for_file_name(name: &str) -> &'static str {
    let ext = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let mime = match ext.as_str() {
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "7z" => "application/x-7z-compressed",
        "tar" => "application/x-tar",
        "gz" | "tgz" => "application/gzip",
        "txt" | "log" | "md" => "text/plain",
        "csv" | "ods" | "xls" | "xlsx" => "application/vnd.spreadsheet",
        "odp" | "ppt" | "pptx" => "application/vnd.presentation",
        "odt" | "doc" | "docx" | "rtf" => "application/msword",
        _ => match kind_for_file_name(name) {
            AttachmentKind::Image | AttachmentKind::Gif => "image/x",
            AttachmentKind::Video => "video/x",
            AttachmentKind::Audio | AttachmentKind::Voice => "audio/x",
            AttachmentKind::File => "application/octet-stream",
        },
    };
    crate::timeline::mime_icon(mime)
}

/// Stable name of a kind for QML (`image`, `gif`, `video`, `audio`, `voice`, `file`).
pub fn kind_name(kind: AttachmentKind) -> &'static str {
    match kind {
        AttachmentKind::Image => "image",
        AttachmentKind::Gif => "gif",
        AttachmentKind::Video => "video",
        AttachmentKind::Audio => "audio",
        AttachmentKind::Voice => "voice",
        AttachmentKind::File => "file",
    }
}

/// What the composer may attach in a conversation (from `ConversationFeatures.files`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct AttachRules {
    /// Accepted kinds with their limits; empty: no attachments at all.
    pub kinds: BTreeMap<AttachmentKind, FileLimits>,
}

impl AttachRules {
    /// The rules of a conversation's features.
    pub fn new(files: &BTreeMap<AttachmentKind, FileLimits>) -> Self {
        Self {
            kinds: files.clone(),
        }
    }

    /// Whether anything can be attached.
    pub fn any(&self) -> bool {
        !self.kinds.is_empty()
    }

    /// Whether a file of `kind` can be attached.
    pub fn accepts(&self, kind: AttachmentKind) -> bool {
        self.kinds.contains_key(&kind)
    }

    /// Whether files can be sent with a caption (every accepted kind allows one).
    pub fn caption(&self) -> bool {
        self.kinds.values().all(|l| l.caption)
    }

    /// The accepted kind names, comma-separated (`""` = none), for QML.
    pub fn names(&self) -> String {
        self.kinds
            .keys()
            .map(|k| kind_name(*k))
            .collect::<Vec<_>>()
            .join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(state: AttachmentState) -> Attachment {
        Attachment {
            id: Some(AttachmentId(42)),
            kind: AttachmentKind::Image,
            mime: "image/jpeg".into(),
            size: 1000,
            dims: Some((640, 480)),
            state,
            ..Attachment::default()
        }
    }

    #[test]
    fn urls_change_with_what_the_provider_decodes() {
        let remote = image(AttachmentState::Remote);
        let local = image(AttachmentState::Local);
        assert_ne!(revision(&remote), revision(&local));
        assert_eq!(revision(&local), revision(&image(AttachmentState::Local)));
        let mut hashed = local.clone();
        hashed.blurhash = Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".into());
        assert_ne!(revision(&local), revision(&hashed));
        // The thumbnail appearing is a new revision (the picture is asked for again), and it
        // is the only thing that changed.
        let mut derived = local.clone();
        derived.thumb = Some(kourier_model::BlobRef {
            path: "thumb".into(),
            key: kourier_model::BlobKey::new([1; 32]),
        });
        assert_ne!(revision(&local), revision(&derived));
        assert_eq!(revision(&derived), revision(&derived.clone()));
        let url = media_url(MediaVariant::Thumb, AttachmentId(42), 7);
        assert_eq!(url, "image://kourier/thumb/42?v=7");
        assert_eq!(
            media_url(MediaVariant::Full, AttachmentId(1), 0),
            "image://kourier/att/1?v=0"
        );
    }

    #[test]
    fn kinds_from_names_and_rules() {
        assert_eq!(kind_for_file_name("IMG_1.JPG"), AttachmentKind::Image);
        assert_eq!(kind_for_file_name("cat.gif"), AttachmentKind::Gif);
        assert_eq!(kind_for_file_name("talk.opus"), AttachmentKind::Audio);
        assert_eq!(kind_for_file_name("clip.webm"), AttachmentKind::Video);
        assert_eq!(kind_for_file_name("plan.pdf"), AttachmentKind::File);
        assert_eq!(kind_for_file_name("Makefile"), AttachmentKind::File);
        assert_eq!(icon_for_file_name("Plan.PDF"), "application-pdf");
        assert_eq!(icon_for_file_name("a.jpg"), "image-x-generic");
        assert_eq!(icon_for_file_name("table.xlsx"), "x-office-spreadsheet");
        assert_eq!(icon_for_file_name("blob"), "application-octet-stream");
        let limits = FileLimits {
            max_size: 100,
            caption: true,
        };
        let rules = AttachRules::new(&BTreeMap::from([
            (AttachmentKind::Image, limits),
            (AttachmentKind::Gif, limits),
        ]));
        assert!(rules.any() && rules.caption());
        assert!(rules.accepts(AttachmentKind::Image));
        assert!(!rules.accepts(AttachmentKind::File));
        assert_eq!(rules.names(), "image,gif");
        assert!(!AttachRules::default().any());
        assert!(is_media_mime("Image/PNG") && is_media_mime("audio/ogg"));
        assert!(is_media_mime("video/webm; codecs=vp9"));
        assert!(!is_media_mime("application/pdf"));
        assert!(!is_media_mime("image/svg+xml"), "scriptable: asks first");
        // Unknown subtypes keep the sender's extension on the copy: they ask first too.
        assert!(!is_media_mime("image/x-anything") && !is_media_mime("video/x-other"));
        assert!(!is_media_mime("") && !is_media_mime("image/"));
        let mut sticker = image(AttachmentState::Local);
        sticker.mime = "image/webp".into();
        assert!(is_animated(&sticker, true) && !is_animated(&sticker, false));
        assert!(!is_clip(&sticker));
        // A GIF file plays as a picture; Signal's "GIF" is an MP4 clip and no picture at all.
        let mut gif = image(AttachmentState::Local);
        gif.kind = AttachmentKind::Gif;
        gif.mime = "image/gif".into();
        assert!(is_animated(&gif, false) && !is_clip(&gif));
        gif.mime = "Video/MP4; codecs=avc1".into();
        assert!(!is_animated(&gif, false) && is_clip(&gif));
        let mut video = image(AttachmentState::Local);
        video.kind = AttachmentKind::Video;
        video.mime = "video/webm".into();
        assert!(is_clip(&video) && !is_animated(&video, false));
    }
}
