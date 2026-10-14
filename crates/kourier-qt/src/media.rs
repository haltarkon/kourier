//! The Rust half of the `image://kourier/…` image provider (§8.6; the C++ half is
//! `cpp/imageprovider.cpp`). The provider's worker threads call [`read`] with the URL's id
//! (`att/12?v=3`, `thumb/12?v=3`, `blurhash/12?v=3`, `avatar/5?v=0&r=8`) and the requested size.
//!
//! An attachment is looked up in the core's media handle table; a miss waits for the core's
//! media read connection (`MediaHandles::resolve_blocking`, allowed on these threads because
//! they are not runtime threads, never on the Qt thread). The `kourier-blob` file is decrypted
//! into memory and handed to Qt still encoded; Qt decodes it at the requested size. Nothing
//! plaintext is written anywhere.
//!
//! The bytes come from other people, so Qt never chooses the decoder: left alone it picks any
//! installed image plugin by content (SVG, PDF, PostScript through Ghostscript, camera raw, PSD
//! and dozens more). [`Codec::sniff`] tells one of a few mainstream raster formats from the
//! first bytes and the C++ side gives Qt exactly that decoder; anything else is not decoded
//! (§9.4). The same check decides whether a plaintext copy may go to `AnimatedImage`
//! ([`note_copy`]).
//!
//! Failures never crash and never block the UI: an attachment that does not decrypt (wrong key,
//! truncated or tampered file), is not downloaded yet or is no picture of a known format shows
//! its blurhash, or a neutral grey tile; an avatar that fails reports an error so QML falls
//! back to initials.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use kourier_app::AppHandle;
use kourier_app::facade::{MediaBlob, MediaEntry, MediaVariant};
use kourier_model::AttachmentId;
use zeroize::Zeroizing;

/// Largest blurhash placeholder side (they are scaled up smoothly in QML).
pub const BLURHASH_MAX: u32 = 32;

/// Side of the neutral tile shown for an attachment that cannot be shown.
const TILE: u32 = 8;

/// Payload format byte: nothing to show (the request fails).
pub const FORMAT_NONE: u8 = 0;
/// Payload format byte: encoded image bytes for `QImageReader`.
pub const FORMAT_ENCODED: u8 = 1;
/// Payload format byte: `width` × `height` RGBA8888 pixels.
pub const FORMAT_RGBA: u8 = 2;
/// Payload flag: crop to a square with rounded corners of the header's radius (avatars).
pub const FLAG_CORNER: u8 = 1;
/// Bytes before the data of an encoded image: format, flags, codec ([`Codec`] as `u8`), the
/// corner radius (`u16`, little endian; 0 without [`FLAG_CORNER`]).
pub const ENCODED_HEADER: usize = 5;
/// A corner radius that is a circle at any size: what an avatar request without `r=` asks for.
pub const CIRCLE: u16 = u16::MAX;

/// A picture format the provider lets Qt decode, told from the file's first bytes (never from
/// the sender's MIME type or file name). The numbers are the payload's codec byte
/// (`cpp/imageprovider.cpp` maps them to Qt's format names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Codec {
    /// JPEG.
    Jpeg = 1,
    /// PNG (and APNG, shown as a still).
    Png = 2,
    /// GIF.
    Gif = 3,
    /// WebP.
    Webp = 4,
    /// Windows bitmap.
    Bmp = 5,
    /// HEIF/HEIC (decoded where the system has the plugin).
    Heif = 6,
    /// AVIF.
    Avif = 7,
    /// JPEG XL.
    Jxl = 8,
}

impl Codec {
    /// Every codec.
    pub const ALL: [Codec; 8] = [
        Self::Jpeg,
        Self::Png,
        Self::Gif,
        Self::Webp,
        Self::Bmp,
        Self::Heif,
        Self::Avif,
        Self::Jxl,
    ];

    /// The format of `bytes` by its magic number; `None` for everything else (it is not
    /// decoded).
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        let at = |offset: usize, sig: &[u8]| bytes.get(offset..offset + sig.len()) == Some(sig);
        if at(0, &[0xff, 0xd8, 0xff]) {
            return Some(Self::Jpeg);
        }
        if at(0, &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
            return Some(Self::Png);
        }
        if at(0, b"GIF87a") || at(0, b"GIF89a") {
            return Some(Self::Gif);
        }
        if at(0, b"RIFF") && at(8, b"WEBP") {
            return Some(Self::Webp);
        }
        // "BM" alone starts plenty of other files: also the zero reserved field and a known
        // DIB header size (BITMAPCOREHEADER … BITMAPV5HEADER).
        if at(0, b"BM")
            && at(6, &[0, 0, 0, 0])
            && bytes
                .get(14..18)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .is_some_and(|dib| matches!(dib, 12 | 16 | 40 | 52 | 56 | 64 | 108 | 124))
        {
            return Some(Self::Bmp);
        }
        // JPEG XL: a bare codestream or its container.
        if at(0, &[0xff, 0x0a])
            || at(
                0,
                &[
                    0, 0, 0, 0x0c, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a,
                ],
            )
        {
            return Some(Self::Jxl);
        }
        // ISO media: the brands of the `ftyp` box (videos and audio have none of these).
        if at(4, b"ftyp") {
            let size = bytes
                .get(..4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)?;
            let end = size.clamp(16, 256).min(bytes.len());
            // The major brand, then the compatible ones (after the minor version).
            let major = bytes.get(8..12).unwrap_or_default();
            let compatible = bytes
                .get(16..end)
                .map(|b| b.as_chunks::<4>().0)
                .unwrap_or_default();
            let has = |wanted: &[&[u8; 4]]| {
                wanted
                    .iter()
                    .any(|w| major == w.as_slice() || compatible.contains(*w))
            };
            if has(&[b"avif", b"avis"]) {
                return Some(Self::Avif);
            }
            if has(&[
                b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"mif1", b"msf1",
            ]) {
                return Some(Self::Heif);
            }
        }
        None
    }

    /// Whether `AnimatedImage` may play a file of this format (GIF and WebP only).
    pub fn animates(self) -> bool {
        matches!(self, Self::Gif | Self::Webp)
    }

    /// Stable lower-case name (`gif`, `webp`, …) for QML.
    pub fn name(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Bmp => "bmp",
            Self::Heif => "heif",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }
}

/// Most bytes [`Codec::sniff`] looks at.
const SNIFF_BYTES: u64 = 256;

/// The format of the file at `path` by its first bytes (blocking; never on the Qt thread).
pub fn sniff_file(path: &Path) -> Option<Codec> {
    let mut head = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(SNIFF_BYTES)
        .read_to_end(&mut head)
        .ok()?;
    Codec::sniff(&head)
}

/// What the plaintext copies handed out by the core really are, by content, with the number of
/// `MediaOpened` events not delivered yet (two views of one attachment share a file): written
/// by the event pump (off the Qt thread) before the UI hears of a copy, taken when the event is
/// delivered.
static COPIES: Mutex<Option<HashMap<PathBuf, (Codec, u32)>>> = Mutex::new(None);

/// Records what the plaintext copy at `path` is (blocking: reads its first bytes). The event
/// pump calls it once for every `MediaOpened` before forwarding it. Copies that are no picture
/// of a known format are not noted.
pub fn note_copy(path: &Path) {
    let Some(codec) = sniff_file(path) else {
        return;
    };
    let mut copies = COPIES.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = copies
        .get_or_insert_with(HashMap::new)
        .entry(path.to_owned())
        .or_insert((codec, 0));
    entry.1 += 1;
}

/// Takes what [`note_copy`] found for one `MediaOpened` of the copy at `path` (`None`: no
/// picture, so nothing the UI may hand to `AnimatedImage`).
pub fn take_copy(path: &Path) -> Option<Codec> {
    let mut copies = COPIES.lock().unwrap_or_else(PoisonError::into_inner);
    let map = copies.as_mut()?;
    let (codec, left) = map.get_mut(path)?;
    let codec = *codec;
    *left -= 1;
    if *left == 0 {
        map.remove(path);
    }
    Some(codec)
}

/// Why the store could not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadError {
    /// No such attachment or avatar.
    #[error("unknown media")]
    Missing,
    /// It exists but could not be read (not downloaded, wrong key, damaged file, core stopped).
    #[error("media unreadable")]
    Failed,
}

/// Where the provider gets media from: the running app, or a fake in tests. Every method may
/// block; they run on the provider's worker threads only.
pub trait MediaStore: Send + Sync {
    /// The handle-table entry of an attachment (a miss is resolved on the media read
    /// connection).
    fn entry(&self, attachment: AttachmentId) -> Result<Option<MediaEntry>, ReadError>;
    /// Decrypts one file of an entry.
    fn read(&self, blob: &MediaBlob) -> Result<Zeroizing<Vec<u8>>, ReadError>;
    /// RGBA pixels of a blurhash at `width` × `height`.
    fn blurhash(&self, hash: &str, width: u32, height: u32) -> Option<Vec<u8>>;
    /// Decrypts an avatar.
    fn avatar(&self, handle: u64) -> Result<Zeroizing<Vec<u8>>, ReadError>;
}

impl MediaStore for AppHandle {
    fn entry(&self, attachment: AttachmentId) -> Result<Option<MediaEntry>, ReadError> {
        let media = self.media();
        if let Some(e) = media.get(attachment) {
            return Ok(Some(e));
        }
        media.resolve_blocking(attachment).map_err(|e| {
            tracing::debug!(error = %e, "a media handle did not resolve");
            ReadError::Failed
        })
    }

    fn read(&self, blob: &MediaBlob) -> Result<Zeroizing<Vec<u8>>, ReadError> {
        self.media().read(blob).map_err(|e| {
            tracing::debug!(error = %e, "an attachment did not decrypt");
            ReadError::Failed
        })
    }

    fn blurhash(&self, hash: &str, width: u32, height: u32) -> Option<Vec<u8>> {
        self.media().placeholder(hash, width, height).ok()
    }

    fn avatar(&self, handle: u64) -> Result<Zeroizing<Vec<u8>>, ReadError> {
        self.read_avatar(handle).map_err(|e| match e {
            kourier_app::media::MediaError::UnknownHandle => ReadError::Missing,
            other => {
                tracing::debug!(error = %other, "an avatar did not decrypt");
                ReadError::Failed
            }
        })
    }
}

/// One parsed request id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// `att/<id>`: the full picture.
    Full(AttachmentId),
    /// `thumb/<id>`: the picture of the timeline: the file when the 400 px thumbnail cannot
    /// cover the size asked for, else the thumbnail (or the file when there is none), see
    /// [`MediaEntry::timeline_blob`].
    Thumb(AttachmentId),
    /// `blurhash/<id>`: the placeholder.
    Blurhash(AttachmentId),
    /// `avatar/<handle>[?v=<rev>][&r=<radius>]`: an avatar cut to a square with rounded corners.
    Avatar {
        /// The avatar's handle.
        handle: u64,
        /// The corner radius in pixels of the requested size; half the side or more is a
        /// circle ([`CIRCLE`] without `r=`).
        radius: u16,
    },
}

/// The corner radius of a request's query (`v=3&r=8`): `r=<pixels>`, a circle without it, and
/// `None` for a value that is no number. A number too big for any picture is a circle.
fn corner_radius(query: &str) -> Option<u16> {
    match query.split('&').find_map(|part| part.strip_prefix("r=")) {
        None => Some(CIRCLE),
        Some(v) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            Some(v.parse().unwrap_or(CIRCLE))
        }
        Some(_) => None,
    }
}

impl Request {
    /// Parses `kind/<number>[?v=<rev>]` (avatars also `&r=<radius>`); anything else is `None`.
    pub fn parse(id: &str) -> Option<Self> {
        let (path, query) = id.split_once('?').unwrap_or((id, ""));
        let (kind, n) = path.split_once('/')?;
        if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match kind {
            "att" => n.parse().ok().map(|n| Self::Full(AttachmentId(n))),
            "thumb" => n.parse().ok().map(|n| Self::Thumb(AttachmentId(n))),
            "blurhash" => n.parse().ok().map(|n| Self::Blurhash(AttachmentId(n))),
            "avatar" => Some(Self::Avatar {
                handle: n.parse().ok()?,
                radius: corner_radius(query)?,
            }),
            _ => None,
        }
    }
}

/// What a request yields, before it is framed for C++.
#[derive(PartialEq, Eq)]
pub enum Image {
    /// Nothing: the request fails (QML shows its own fallback).
    None,
    /// Encoded bytes of a known picture format; `corner` crops to a square with rounded corners.
    Encoded {
        /// The file's bytes (zeroized on drop; C++ zeroes its copy after decoding).
        bytes: Zeroizing<Vec<u8>>,
        /// What the bytes are ([`Codec::sniff`]): the one decoder Qt may use.
        codec: Codec,
        /// Crop to a square whose corners have this radius in pixels of the requested size
        /// (half the side or more: a circle); `None` keeps the picture as it is.
        corner: Option<u16>,
    },
    /// Raw pixels.
    Rgba {
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// RGBA8888, `width * height * 4` bytes.
        pixels: Vec<u8>,
    },
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => f.write_str("None"),
            Self::Encoded {
                bytes,
                codec,
                corner,
            } => f
                .debug_struct("Encoded")
                .field("len", &bytes.len())
                .field("codec", codec)
                .field("corner", corner)
                .finish(),
            Self::Rgba { width, height, .. } => f
                .debug_struct("Rgba")
                .field("width", width)
                .field("height", height)
                .finish_non_exhaustive(),
        }
    }
}

impl Image {
    /// The header + payload the C++ provider decodes (see `cpp/imageprovider.h`).
    pub fn into_payload(self) -> Vec<u8> {
        match self {
            Self::None => vec![FORMAT_NONE, 0],
            Self::Encoded {
                bytes,
                codec,
                corner,
            } => {
                let mut out = Vec::with_capacity(bytes.len() + ENCODED_HEADER);
                out.push(FORMAT_ENCODED);
                out.push(if corner.is_some() { FLAG_CORNER } else { 0 });
                out.push(codec as u8);
                out.extend_from_slice(&corner.unwrap_or(0).to_le_bytes());
                out.extend_from_slice(&bytes);
                out
            }
            Self::Rgba {
                width,
                height,
                pixels,
            } => {
                let mut out = Vec::with_capacity(pixels.len() + 10);
                out.extend_from_slice(&[FORMAT_RGBA, 0]);
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                out.extend_from_slice(&pixels);
                out
            }
        }
    }
}

/// Placeholder size for a requested size: the request's aspect ratio (else the picture's),
/// at most [`BLURHASH_MAX`] on the long side.
fn placeholder_size(req: (i32, i32), dims: Option<(u32, u32)>) -> (u32, u32) {
    let (w, h) = match req {
        (w, h) if w > 0 && h > 0 => (w.unsigned_abs(), h.unsigned_abs()),
        _ => dims.filter(|(w, h)| *w > 0 && *h > 0).unwrap_or((4, 3)),
    };
    let long = w.max(h);
    let scale = f64::from(BLURHASH_MAX) / f64::from(long);
    let fit = |v: u32| ((f64::from(v) * scale).round() as u32).clamp(1, BLURHASH_MAX);
    if long <= BLURHASH_MAX {
        (w.max(1), h.max(1))
    } else {
        (fit(w), fit(h))
    }
}

/// The stand-in for an attachment that cannot be shown: its blurhash, else a grey tile.
fn stand_in(store: &dyn MediaStore, entry: Option<&MediaEntry>, req: (i32, i32)) -> Image {
    if let Some(e) = entry
        && let Some(hash) = e.blurhash.as_deref()
    {
        let (w, h) = placeholder_size(req, e.dims);
        if let Some(pixels) = store.blurhash(hash, w, h) {
            return Image::Rgba {
                width: w,
                height: h,
                pixels,
            };
        }
    }
    Image::Rgba {
        width: TILE,
        height: TILE,
        pixels: [128u8, 128, 128, 255].repeat((TILE * TILE) as usize),
    }
}

/// Answers one provider request (blocking; worker threads only).
pub fn resolve(store: &dyn MediaStore, id: &str, width: i32, height: i32) -> Image {
    let Some(req) = Request::parse(id) else {
        return Image::None;
    };
    let size = (width, height);
    match req {
        // An avatar that is no picture of a known format fails like a missing one (initials).
        Request::Avatar { handle, radius } => match store.avatar(handle) {
            Ok(bytes) => match Codec::sniff(&bytes) {
                Some(codec) => Image::Encoded {
                    bytes,
                    codec,
                    corner: Some(radius),
                },
                None => Image::None,
            },
            Err(_) => Image::None,
        },
        Request::Full(id) | Request::Thumb(id) | Request::Blurhash(id) => {
            let entry = match store.entry(id) {
                Ok(Some(e)) => e,
                Ok(None) => return Image::None,
                Err(_) => return stand_in(store, None, size),
            };
            let blob = match req {
                Request::Full(_) => entry.blob(MediaVariant::Full),
                // The size is the item's PHYSICAL size (QML's `sourceSize`); an unset side
                // arrives as 0 or less.
                Request::Thumb(_) => entry.timeline_blob((
                    u32::try_from(width).unwrap_or(0),
                    u32::try_from(height).unwrap_or(0),
                )),
                _ => return stand_in(store, Some(&entry), size),
            };
            let mut read = blob.map(|b| store.read(b));
            // The timeline does not decode a huge picture of a format that Qt decodes whole
            // (`MAX_TIMELINE_PIXELS`): the format is the file's bytes', not the sender's MIME
            // type. A thumbnail exists then (the file was chosen over it), so it is read.
            if matches!(req, Request::Thumb(_))
                && let Some(Ok(bytes)) = &read
                && Codec::sniff(bytes).is_some_and(|codec| codec != Codec::Jpeg)
                && entry.exceeds_timeline_pixels()
                && let Some(thumb) = &entry.thumb
                && blob != Some(thumb)
            {
                read = Some(store.read(thumb));
            }
            match read {
                Some(Ok(bytes)) => match Codec::sniff(&bytes) {
                    Some(codec) => Image::Encoded {
                        bytes,
                        codec,
                        corner: None,
                    },
                    // Not a picture Qt may decode, whatever its sender called it.
                    None => stand_in(store, Some(&entry), size),
                },
                // Not downloaded yet, or unreadable: the placeholder.
                _ => stand_in(store, Some(&entry), size),
            }
        }
    }
}

static STORE: RwLock<Option<Arc<dyn MediaStore>>> = RwLock::new(None);

/// Sets the store the provider reads from (the running app); `None` detaches it.
pub fn set_store(store: Option<Arc<dyn MediaStore>>) {
    *STORE.write().unwrap_or_else(PoisonError::into_inner) = store;
}

/// The provider's reader (`MediaReader` in `cpp/imageprovider.h`): called on its worker threads.
/// A panic while reading (untrusted media) fails the request instead of unwinding into C++,
/// where cxx would abort the whole app.
pub fn read(id: &str, width: i32, height: i32) -> Vec<u8> {
    let store = STORE.read().unwrap_or_else(PoisonError::into_inner).clone();
    let Some(store) = store else {
        return Image::None.into_payload();
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resolve(store.as_ref(), id, width, height).into_payload()
    }))
    .unwrap_or_else(|_| {
        tracing::error!("reading media for the image provider panicked");
        Image::None.into_payload()
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use kourier_model::{AccountId, AttachmentKind, AttachmentState, BlobKey};

    use super::*;

    /// A fake store: one image entry (id 1) whose file decrypts only with key `[1; 32]`, and
    /// one avatar (handle 5). The files hold `magic`, then a tag naming them.
    struct Fake {
        key: BlobKey,
        thumb: bool,
        magic: &'static [u8],
        /// The picture's display size (`MediaEntry::dims`).
        dims: (u32, u32),
        /// What the sender called the file (its MIME type).
        label: &'static str,
        reads: Mutex<Vec<&'static str>>,
    }

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"8\" height=\"8\"/>";
    const EPS: &[u8] = b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 10 10\nshowpage\n";

    /// A file's bytes: `magic`, then `tag`.
    fn file(magic: &[u8], tag: &str) -> Vec<u8> {
        [magic, tag.as_bytes()].concat()
    }

    fn blob(key: BlobKey, mime: &str) -> MediaBlob {
        MediaBlob {
            path: PathBuf::from("/blobs/x"),
            key,
            mime: Some(mime.into()),
        }
    }

    impl MediaStore for Fake {
        fn entry(&self, attachment: AttachmentId) -> Result<Option<MediaEntry>, ReadError> {
            self.reads.lock().unwrap().push("entry");
            Ok((attachment == AttachmentId(1)).then(|| MediaEntry {
                attachment,
                account: AccountId(1),
                kind: AttachmentKind::Image,
                mime: Some(self.label.into()),
                state: AttachmentState::Local,
                file: Some(blob(self.key.clone(), self.label)),
                thumb: self.thumb.then(|| blob(self.key.clone(), "image/webp")),
                blurhash: Some("LKO2?U%2Tw=w]~RBVZRi};RPxuwH".into()),
                dims: Some(self.dims),
            }))
        }

        fn read(&self, blob: &MediaBlob) -> Result<Zeroizing<Vec<u8>>, ReadError> {
            if blob.key.expose() != &[1; 32] {
                return Err(ReadError::Failed);
            }
            let tag = if blob.mime.as_deref() == Some("image/webp") {
                "thumb"
            } else {
                "full"
            };
            self.reads.lock().unwrap().push(tag);
            Ok(Zeroizing::new(file(self.magic, tag)))
        }

        fn blurhash(&self, _hash: &str, width: u32, height: u32) -> Option<Vec<u8>> {
            Some(vec![7; (width * height * 4) as usize])
        }

        fn avatar(&self, handle: u64) -> Result<Zeroizing<Vec<u8>>, ReadError> {
            match handle {
                5 => Ok(Zeroizing::new(file(self.magic, "avatar"))),
                6 => Err(ReadError::Failed),
                _ => Err(ReadError::Missing),
            }
        }
    }

    fn fake(key: [u8; 32], thumb: bool) -> Fake {
        Fake {
            key: BlobKey::new(key),
            thumb,
            magic: PNG,
            dims: (640, 480),
            label: "image/jpeg",
            reads: Mutex::new(Vec::new()),
        }
    }

    /// The bytes after the PNG magic, and `corner`, of a PNG handed to Qt.
    fn encoded(img: &Image) -> Option<(&[u8], Option<u16>)> {
        match img {
            Image::Encoded {
                bytes,
                codec: Codec::Png,
                corner,
            } => Some((bytes.strip_prefix(PNG)?, *corner)),
            _ => None,
        }
    }

    #[test]
    fn ids_parse_strictly() {
        assert_eq!(
            Request::parse("thumb/12?v=3"),
            Some(Request::Thumb(AttachmentId(12)))
        );
        assert_eq!(
            Request::parse("att/7"),
            Some(Request::Full(AttachmentId(7)))
        );
        assert_eq!(
            Request::parse("avatar/5?v=0"),
            Some(Request::Avatar {
                handle: 5,
                radius: CIRCLE
            })
        );
        assert_eq!(
            Request::parse("blurhash/1?v=9"),
            Some(Request::Blurhash(AttachmentId(1)))
        );
        for bad in [
            "",
            "thumb",
            "thumb/",
            "thumb/-1",
            "thumb/1x",
            "file/1",
            "avatar/../1",
            "avatar/5?r=",
            "avatar/5?v=0&r=-3",
            "avatar/5?r=8px",
        ] {
            assert_eq!(Request::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_timeline_gets_the_file_where_its_thumbnail_would_be_stretched() {
        let tag = |store: &Fake, id: &str, w, h| match encoded(&resolve(store, id, w, h)) {
            Some((bytes, None)) => String::from_utf8_lossy(bytes).into_owned(),
            other => panic!("{id} {w}x{h}: {other:?}"),
        };
        // A phone photo, 3000 × 4000: the thumbnail is 300 × 400.
        let mut store = fake([1; 32], true);
        store.dims = (3000, 4000);
        // Scale 2: the bubble is 576 × 768 pixels, the thumbnail would be stretched 1.9 times.
        assert_eq!(tag(&store, "thumb/1?v=1", 576, 768), "full");
        // Scale 1.25 (the user's display): 360 × 480 against 300 × 400.
        assert_eq!(tag(&store, "thumb/1?v=1", 360, 480), "full");
        // What the thumbnail covers is still drawn from it: a 100 × 75 request (a quote's
        // picture, an album's small tile), and 200 × 267 (the thumbnail halved).
        assert_eq!(tag(&store, "thumb/1?v=1", 100, 75), "thumb");
        assert_eq!(tag(&store, "thumb/1?v=1", 200, 267), "thumb");
        // No size asked (QML's unset `sourceSize` is 0 or -1): the thumbnail, as before.
        assert_eq!(tag(&store, "thumb/1?v=1", 0, 0), "thumb");
        assert_eq!(tag(&store, "thumb/1?v=1", -1, -1), "thumb");
        // The viewer's request is the file whatever the size.
        assert_eq!(tag(&store, "att/1?v=1", 100, 75), "full");
        // A landscape 4000 × 3000 at scale 1 (288 × 216) is covered by 400 × 300.
        store.dims = (4000, 3000);
        assert_eq!(tag(&store, "thumb/1?v=1", 288, 216), "thumb");
        assert_eq!(tag(&store, "thumb/1?v=1", 576, 432), "full");
        // Without a thumbnail every size gets the file.
        let mut no_thumb = fake([1; 32], false);
        no_thumb.dims = (3000, 4000);
        assert_eq!(tag(&no_thumb, "thumb/1", 100, 75), "full");
    }

    #[test]
    fn a_huge_picture_is_drawn_from_its_thumbnail_unless_its_bytes_are_a_jpeg() {
        // What the provider is handed for `thumb/1` asked at 576 × 432: the codec and the tag
        // of the file it came from.
        let served = |store: &Fake| match resolve(store, "thumb/1?v=1", 576, 432) {
            Image::Encoded { bytes, codec, .. } => {
                let tag = bytes.strip_prefix(store.magic).unwrap_or(&bytes[..]);
                (codec, String::from_utf8_lossy(tag).into_owned())
            }
            _ => panic!("not an encoded picture"),
        };
        let huge = (5000, 4000);
        // A PNG that the sender called a JPEG: the thumbnail (its MIME type decides nothing).
        let mut png = fake([1; 32], true);
        png.dims = huge;
        png.label = "image/jpeg";
        assert_eq!(served(&png), (Codec::Png, "thumb".into()));
        png.label = "image/png";
        assert_eq!(served(&png), (Codec::Png, "thumb".into()));
        // The same size and a JPEG's bytes: the file, even when called a PNG.
        let mut jpeg = fake([1; 32], true);
        jpeg.magic = &[0xff, 0xd8, 0xff, 0xe0, 0, 16];
        jpeg.dims = huge;
        jpeg.label = "image/png";
        assert_eq!(served(&jpeg), (Codec::Jpeg, "full".into()));
        // At the limit a PNG is still drawn from its file; a row above it is not.
        png.dims = (4096, 4096);
        assert_eq!(served(&png), (Codec::Png, "full".into()));
        png.dims = (4096, 4097);
        assert_eq!(served(&png), (Codec::Png, "thumb".into()));
        // Without a thumbnail there is nothing to fall back to: the file.
        let mut alone = fake([1; 32], false);
        alone.dims = huge;
        assert_eq!(served(&alone), (Codec::Png, "full".into()));
        // The viewer's request is the file, whatever its size.
        png.dims = huge;
        assert!(matches!(
            resolve(&png, "att/1?v=1", 0, 0),
            Image::Encoded { codec: Codec::Png, ref bytes, .. } if bytes.ends_with(b"full")
        ));
    }

    #[test]
    fn an_avatar_request_carries_its_corner_radius() {
        let avatar = |id| match Request::parse(id) {
            Some(Request::Avatar { handle, radius }) => Some((handle, radius)),
            _ => None,
        };
        assert_eq!(avatar("avatar/5?v=0&r=8"), Some((5, 8)));
        assert_eq!(avatar("avatar/5?r=8&v=0"), Some((5, 8)));
        // A square avatar, as an old theme with sharp corners has it.
        assert_eq!(avatar("avatar/5?v=1&r=0"), Some((5, 0)));
        // No radius, or one no picture reaches: a circle.
        assert_eq!(avatar("avatar/5"), Some((5, CIRCLE)));
        assert_eq!(avatar("avatar/5?v=2"), Some((5, CIRCLE)));
        assert_eq!(avatar("avatar/5?r=99999999999"), Some((5, CIRCLE)));
        // Only an avatar has corners.
        assert_eq!(
            Request::parse("thumb/5?r=8"),
            Some(Request::Thumb(AttachmentId(5)))
        );
    }

    #[test]
    fn thumbnails_fall_back_to_the_file_and_avatars_are_cut_to_their_corners() {
        let store = fake([1; 32], true);
        assert_eq!(
            encoded(&resolve(&store, "thumb/1?v=1", 200, 150)),
            Some((&b"thumb"[..], None))
        );
        assert_eq!(
            encoded(&resolve(&store, "att/1?v=1", 0, 0)),
            Some((&b"full"[..], None))
        );
        let no_thumb = fake([1; 32], false);
        assert_eq!(
            encoded(&resolve(&no_thumb, "thumb/1", 64, 48)),
            Some((&b"full"[..], None))
        );
        assert_eq!(
            encoded(&resolve(&store, "avatar/5?v=0", 40, 40)),
            Some((&b"avatar"[..], Some(CIRCLE)))
        );
        assert_eq!(
            encoded(&resolve(&store, "avatar/5?v=0&r=6", 40, 40)),
            Some((&b"avatar"[..], Some(6)))
        );
        assert_eq!(
            encoded(&resolve(&store, "avatar/5?v=0&r=0", 40, 40)),
            Some((&b"avatar"[..], Some(0)))
        );
        // The radius is in the header, little endian, after the flag that says it counts.
        let payload = resolve(&store, "avatar/5?r=300", 40, 40).into_payload();
        assert_eq!(
            payload[..ENCODED_HEADER],
            [FORMAT_ENCODED, FLAG_CORNER, Codec::Png as u8, 44, 1]
        );
        // Avatars that fail leave the initials to QML.
        assert_eq!(resolve(&store, "avatar/6", 40, 40), Image::None);
        assert_eq!(resolve(&store, "avatar/99", 40, 40), Image::None);
        assert_eq!(resolve(&store, "nonsense", 40, 40), Image::None);
        assert_eq!(resolve(&store, "thumb/2", 40, 40), Image::None, "unknown");
    }

    #[test]
    fn a_wrong_key_shows_the_placeholder_instead_of_failing() {
        let store = fake([2; 32], true);
        match resolve(&store, "thumb/1?v=1", 64, 48) {
            Image::Rgba {
                width,
                height,
                pixels,
            } => {
                assert_eq!((width, height), (32, 24), "blurhash at the request's ratio");
                assert_eq!(pixels.len(), 32 * 24 * 4);
            }
            other => panic!("expected a placeholder, got {other:?}"),
        }
        match resolve(&store, "blurhash/1", 0, 0) {
            Image::Rgba { width, height, .. } => assert_eq!((width, height), (32, 24)),
            other => panic!("{other:?}"),
        }
        let payload = resolve(&store, "blurhash/1", 8, 8).into_payload();
        assert_eq!(payload[0], FORMAT_RGBA);
        assert_eq!(&payload[2..6], &8u32.to_le_bytes());
        assert_eq!(payload.len(), 10 + 8 * 8 * 4);
    }

    #[test]
    fn the_decoder_is_chosen_from_the_content_and_only_for_known_pictures() {
        let ftyp = |brands: &[&[u8; 4]]| {
            let mut b = Vec::new();
            b.extend_from_slice(&(8 + 8 + 4 * (brands.len() as u32 - 1)).to_be_bytes());
            b.extend_from_slice(b"ftyp");
            b.extend_from_slice(brands[0]);
            b.extend_from_slice(&[0; 4]);
            for brand in &brands[1..] {
                b.extend_from_slice(*brand);
            }
            b.extend_from_slice(&[0; 32]);
            b
        };
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[70, 0, 0, 0, 0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0]);
        let known: [(Vec<u8>, Codec); 11] = [
            (vec![0xff, 0xd8, 0xff, 0xe0, 0, 16], Codec::Jpeg),
            (file(PNG, "…"), Codec::Png),
            (b"GIF89a\x01\x00".to_vec(), Codec::Gif),
            (b"GIF87a\x01\x00".to_vec(), Codec::Gif),
            (b"RIFF\x10\x00\x00\x00WEBPVP8 ".to_vec(), Codec::Webp),
            (bmp, Codec::Bmp),
            (vec![0xff, 0x0a, 0x00], Codec::Jxl),
            (b"\0\0\0\x0cJXL \r\n\x87\n".to_vec(), Codec::Jxl),
            (ftyp(&[b"avif", b"mif1", b"miaf"]), Codec::Avif),
            // The AVIF brand may be a compatible brand only.
            (ftyp(&[b"mif1", b"avif"]), Codec::Avif),
            (ftyp(&[b"heic", b"mif1"]), Codec::Heif),
        ];
        for (bytes, codec) in &known {
            assert_eq!(Codec::sniff(bytes), Some(*codec), "{codec:?}");
        }
        // What Qt would also decode by content if it were asked: documents, vector graphics,
        // PostScript (through Ghostscript), camera raw, layered formats, videos.
        let refused: [&[u8]; 14] = [
            SVG,
            b"<?xml version=\"1.0\"?><svg/>",
            EPS,
            b"%PDF-1.7\n",
            b"8BPS\x00\x01",
            b"II*\x00\x08\x00\x00\x00",
            b"MM\x00*\x00\x00\x00\x08",
            b"gimp xcf file\0",
            b"\x00\x00\x01\x00\x01\x00\x10\x10",
            b"BM, a text that starts like a bitmap",
            b"RIFF\x10\x00\x00\x00WAVEfmt ",
            &ftyp(&[b"isom", b"mp42"]),
            b"GIF",
            b"",
        ];
        for bytes in refused {
            assert_eq!(
                Codec::sniff(bytes),
                None,
                "{:?}",
                &bytes[..bytes.len().min(8)]
            );
        }
        assert_eq!(Codec::ALL.len(), 8);
        for codec in Codec::ALL {
            assert_eq!(codec.animates(), matches!(codec, Codec::Gif | Codec::Webp));
            assert!(!codec.name().is_empty());
        }
    }

    #[test]
    fn what_is_no_known_picture_is_never_handed_to_qt() {
        for content in [SVG, EPS, b"%PDF-1.7\n".as_slice()] {
            let mut store = fake([1; 32], true);
            store.magic = content;
            // An attachment shows its placeholder, whichever variant is asked for.
            for id in ["thumb/1?v=1", "att/1?v=1"] {
                match resolve(&store, id, 64, 48) {
                    Image::Rgba { width, height, .. } => assert_eq!((width, height), (32, 24)),
                    other => panic!("{id}: expected the placeholder, got {other:?}"),
                }
            }
            // An avatar fails, so the initials stay.
            assert_eq!(resolve(&store, "avatar/5", 40, 40), Image::None);
            // The file was read (it is the content that is refused, not the request).
            assert!(store.reads.lock().unwrap().contains(&"full"));
        }
    }

    #[test]
    fn plaintext_copies_are_noted_by_content_for_animated_image() {
        let dir = std::env::temp_dir().join(format!("kourier-qt-copies-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A GIF and a WebP may be played; a picture of another format is noted as what it is.
        let gif = dir.join("a.webp");
        std::fs::write(&gif, b"GIF89a\x01\x00\x01\x00").unwrap();
        let png = dir.join("b.gif");
        std::fs::write(&png, file(PNG, "still")).unwrap();
        // PostScript named like a GIF (the name follows the sender's MIME type): not noted.
        let eps = dir.join("c.gif");
        std::fs::write(&eps, EPS).unwrap();
        for path in [&gif, &png, &eps, &dir.join("missing.gif")] {
            note_copy(path);
        }
        // Two views opened the same copy: each `MediaOpened` is noted and taken once.
        note_copy(&gif);
        assert_eq!(take_copy(&gif), Some(Codec::Gif));
        assert_eq!(take_copy(&gif), Some(Codec::Gif));
        assert_eq!(take_copy(&gif), None, "each note is taken once");
        assert_eq!(take_copy(&png).map(Codec::animates), Some(false));
        assert_eq!(take_copy(&eps), None);
        assert_eq!(take_copy(&dir.join("missing.gif")), None);
        assert_eq!(sniff_file(&gif), Some(Codec::Gif));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_reader_without_an_app_answers_nothing() {
        set_store(None);
        assert_eq!(read("thumb/1", 10, 10), vec![FORMAT_NONE, 0]);
        let store: Arc<dyn MediaStore> = Arc::new(fake([1; 32], true));
        set_store(Some(store));
        let payload = read("att/1", 0, 0);
        assert_eq!(
            payload[..ENCODED_HEADER],
            [FORMAT_ENCODED, 0, Codec::Png as u8, 0, 0]
        );
        assert_eq!(payload[ENCODED_HEADER..], file(PNG, "full"));
        // A store that panics fails the request; nothing unwinds into C++.
        struct Panics;
        impl MediaStore for Panics {
            fn entry(&self, _: AttachmentId) -> Result<Option<MediaEntry>, ReadError> {
                panic!("broken store")
            }
            fn read(&self, _: &MediaBlob) -> Result<Zeroizing<Vec<u8>>, ReadError> {
                Err(ReadError::Failed)
            }
            fn blurhash(&self, _: &str, _: u32, _: u32) -> Option<Vec<u8>> {
                None
            }
            fn avatar(&self, _: u64) -> Result<Zeroizing<Vec<u8>>, ReadError> {
                Err(ReadError::Missing)
            }
        }
        set_store(Some(Arc::new(Panics)));
        assert_eq!(read("thumb/1", 10, 10), vec![FORMAT_NONE, 0]);
        set_store(None);
        assert!(placeholder_size((0, 0), None) == (4, 3));
        assert_eq!(placeholder_size((1200, 300), None), (32, 8));
    }
}
