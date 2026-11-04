//! The look: does a file carry where it was taken (or made)? (LOC, docs/architecture.md §5.8.)
//!
//! [`look`] walks a file's metadata containers through a caller's [`ReadSeek`] and answers
//! [`Look::Found`], [`Look::None`] or [`Look::Unreadable`]. It never decodes pixels, never turns a
//! coordinate into a number and never keeps one: the answer names only the container and the kind
//! of proof ([`Evidence`]), so it can go into a log count or a test.
//!
//! FOUND (one rule for every format): a latitude AND a longitude are present and not both zero.
//! A GPS fix marked void (EXIF GPSStatus `V`) still counts: cameras then write the last fix, a
//! real place. Zero placeholders ("GPS off") do not count; an EXIF rational with denominator 0
//! counts as zero.
//!
//! Formats looked at so far: JPEG (APP1 `Exif\0\0`), TIFF-shaped files (TIFF, DNG, Panasonic
//! RW2, Olympus ORF), the JPEG XL container (its `Exif` boxes) and ISO-BMFF files (starting with
//! `ftyp`, or with a box QuickTime files start with): the HEIF items (HEIC, AVIF: the `Exif`
//! items of the top-level `meta`), the movies' `moov` (MP4, MOV, 3GP, M4A: the location atoms,
//! keys and telemetry tracks of [`crate::movie`]) and the top-level XMP `uuid` box; PNG and WebP
//! ([`crate::chunks`]); XMP packets wherever these formats keep them (JPEG APP1 and Extended
//! XMP, PNG `iTXt`, WebP `XMP `, JPEG XL `xml `, HEIF XMP items, MOV `udta/XMP_` and the `uuid`
//! box), searched by [`crate::xmp`]. Anything else is [`Look::None`]. Named gaps:
//! BigTIFF (`II+\0`), a bare JPEG XL codestream (it has no metadata boxes), JPEG XL's
//! Brotli-compressed `brob` boxes and a Samsung trailer after a movie's last box; also (LOC P5)
//! C2PA / JUMBF manifests ("Content Credentials": JPEG APP11, PNG `caBX`, the ISO-BMFF `uuid`
//! or `jumb` box, whose `stds.exif` assertion can hold a GPS position: the look reads none, and
//! `strip_png`/`strip_jpeg` drop them), a location only in a JPEG's secondary image (MPF images
//! after the first EOI: the walk stops at the first SOS), archives other than zip (tar, 7z,
//! rar) and containers that are zips (docx, odt), and `Unreadable` itself (a file not judged
//! within the bounds below leaves unasked; the core counts it in its log).
//!
//! Bounds (each past its bound is [`Look::Unreadable`]): one read at most [`PAYLOAD_MAX`], at
//! most [`FILE_READ_MAX`] read per file, at most [`HEADERS_MAX`] headers walked, depth at most
//! [`DEPTH_MAX`], at most [`IFDS_MAX`] IFDs of at most [`IFD_ENTRIES_MAX`] entries, IFD offsets
//! inside their blob; an IFD already visited ends its chain (nothing new to see there); at most
//! [`TIME_MAX`] per file (the clock is looked at before each read). Nothing is
//! read at or past `len`, no allocation follows a length field before it is checked against the
//! bounds, and no input panics.
//!
//! Where several places hold metadata, Found anywhere wins over a broken place elsewhere.

use std::io::{Read, Seek, SeekFrom};
use std::time::{Duration, Instant};

use crate::{bmff, chunks, heif, movie, tiff, xmp};

/// One read of a payload is at most this many bytes.
pub(crate) const PAYLOAD_MAX: u64 = 1 << 20;
/// At most this many bytes are read from one file.
pub(crate) const FILE_READ_MAX: u64 = 2 << 20;
/// A read costs the budget at least this many bytes: one header (LOC RF1).
const READ_MIN: u64 = 8;
/// At most this many headers (segments, boxes) are walked in one file.
pub(crate) const HEADERS_MAX: u32 = 100_000;
/// Containers nest at most this deep.
pub(crate) const DEPTH_MAX: u32 = 12;
/// At most this many IFDs are read from one TIFF blob (the GPS IFD included).
pub(crate) const IFDS_MAX: usize = 8;
/// An IFD has at most this many entries.
pub(crate) const IFD_ENTRIES_MAX: u16 = 1_000;
/// One file is looked at for at most this long (a cold disk or a network mount makes many
/// small reads slow: critic F12).
pub(crate) const TIME_MAX: Duration = Duration::from_secs(2);

/// What [`look`] reads from: any `Read + Seek` (a file, a cursor over bytes). One `dyn` copy of
/// the walkers is in the binary, whatever the reader.
pub trait ReadSeek: Read + Seek {}

impl<T: Read + Seek + ?Sized> ReadSeek for T {}

/// The answer of [`look`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Look {
    /// The file carries a location.
    Found(Evidence),
    /// No location was found in a format looked at, or the format is not looked at.
    None,
    /// The file could not be judged: malformed, cut short or past a bound.
    Unreadable,
}

/// Where a location was found. It names the container and the kind of proof, never a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evidence {
    /// The container format the look recognised.
    pub format: LookFormat,
    /// What held the location.
    pub kind: EvidenceKind,
}

/// The container formats the look recognises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LookFormat {
    /// JPEG.
    Jpeg,
    /// PNG.
    Png,
    /// WebP.
    WebP,
    /// TIFF and the raw formats shaped like it (DNG, RW2, ORF).
    Tiff,
    /// HEIF, HEIC, AVIF.
    Heif,
    /// The JPEG XL container.
    Jxl,
    /// ISO-BMFF video or audio (MP4, MOV, 3GP, M4A).
    Bmff,
}

/// The kind of proof of a [`Look::Found`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EvidenceKind {
    /// An EXIF GPS IFD with a latitude and a longitude.
    ExifGps,
    /// An XMP packet naming a latitude and a longitude.
    Xmp,
    /// A movie's location atom or key.
    MovieAtom,
    /// A timed-metadata track that carries positions.
    Track,
}

/// Looks at the first `len` bytes of `r` for a location (the module doc has the rules). The
/// reader's position afterwards is unspecified.
pub fn look(r: &mut dyn ReadSeek, len: u64) -> Look {
    look_why(r, len).0
}

/// Why a look was [`Look::Unreadable`], for a log line that carries nothing of the file (LOC P4):
/// whether the time bound was passed, and the container the look recognised before it gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unreadable {
    /// The per-file time bound ([`TIME_MAX`]) was passed; otherwise a size or count bound, a
    /// malformed or a cut-short file.
    pub time: bool,
    /// The format the first bytes named (none when even they could not be read).
    pub format: Option<LookFormat>,
}

impl Unreadable {
    /// `"time"` or `"bound"`: the reason, as a log word.
    pub const fn reason(&self) -> &'static str {
        if self.time { "time" } else { "bound" }
    }

    /// The format as a log word (`"unknown"` when none was recognised).
    pub const fn format_name(&self) -> &'static str {
        match self.format {
            Some(LookFormat::Jpeg) => "jpeg",
            Some(LookFormat::Png) => "png",
            Some(LookFormat::WebP) => "webp",
            Some(LookFormat::Tiff) => "tiff",
            Some(LookFormat::Heif) => "heif",
            Some(LookFormat::Jxl) => "jxl",
            Some(LookFormat::Bmff) => "bmff",
            None => "unknown",
        }
    }
}

/// [`look`] with the reason of an [`Look::Unreadable`] answer (`Some` exactly then).
pub fn look_why(r: &mut dyn ReadSeek, len: u64) -> (Look, Option<Unreadable>) {
    let start = Instant::now();
    look_clocked_why(r, len, &mut || start.elapsed())
}

/// [`look`] with its clock: `elapsed` says how long the look has taken so far.
#[cfg(test)]
pub(crate) fn look_clocked(
    r: &mut dyn ReadSeek,
    len: u64,
    elapsed: &mut dyn FnMut() -> Duration,
) -> Look {
    look_clocked_why(r, len, elapsed).0
}

/// [`look_clocked`] with the reason of an unreadable answer.
pub(crate) fn look_clocked_why(
    r: &mut dyn ReadSeek,
    len: u64,
    elapsed: &mut dyn FnMut() -> Duration,
) -> (Look, Option<Unreadable>) {
    let mut src = FileSource {
        r,
        len,
        spent: 0,
        headers: 0,
        elapsed,
    };
    let head = match src.read(0, len.min(12) as usize) {
        Ok(h) => h,
        Err(Bound) => return unreadable(&mut src, None),
    };
    let mut format = None;
    let verdict = if head.starts_with(&[0xff, 0xd8, 0xff]) {
        format = Some(LookFormat::Jpeg);
        jpeg(&mut src).map(|f| f.map(|kind| (LookFormat::Jpeg, kind)))
    } else if tiff::is_tiff_magic(&head) {
        format = Some(LookFormat::Tiff);
        tiff::gps_in(&mut src).map(|f| f.then_some((LookFormat::Tiff, EvidenceKind::ExifGps)))
    } else if head.starts_with(&chunks::PNG_SIGNATURE) {
        format = Some(LookFormat::Png);
        chunks::png(&mut src).map(|f| f.map(|kind| (LookFormat::Png, kind)))
    } else if head.len() == 12 && head[..4] == *b"RIFF" && head[8..] == *b"WEBP" {
        format = Some(LookFormat::WebP);
        chunks::webp(&mut src).map(|f| f.map(|kind| (LookFormat::WebP, kind)))
    } else if head == JXL_SIGNATURE {
        format = Some(LookFormat::Jxl);
        jxl(&mut src).map(|f| f.map(|kind| (LookFormat::Jxl, kind)))
    } else if head
        .get(4..8)
        .is_some_and(|t| ISO_FIRST.iter().any(|f| f[..] == *t))
    {
        // HEIF or a movie: the walk says which when it finds something; a failed walk is
        // named `Bmff` (the family).
        format = Some(LookFormat::Bmff);
        iso_file(&mut src)
    } else {
        Ok(None)
    };
    match verdict {
        Ok(Some((format, kind))) => (Look::Found(Evidence { format, kind }), None),
        Ok(None) => (Look::None, None),
        Err(Bound) => unreadable(&mut src, format),
    }
}

/// The unreadable answer: with its reason, the clock decides `time`.
fn unreadable(src: &mut FileSource<'_>, format: Option<LookFormat>) -> (Look, Option<Unreadable>) {
    let time = (src.elapsed)() > TIME_MAX;
    (Look::Unreadable, Some(Unreadable { time, format }))
}

/// Past a bound, cut short or malformed: the caller answers [`Look::Unreadable`] (or goes on and
/// remembers it, when another place may still hold a location). Carries nothing of the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bound;

/// Random-access bytes the walkers read from: a file through [`ReadSeek`], a slice, or a window
/// into either. Offsets are relative to the source.
pub(crate) trait Bytes {
    /// How many bytes the source has.
    fn len(&self) -> u64;
    /// Exactly `n` bytes at `off`, or [`Bound`] when they are not all inside the source or a
    /// read bound would be passed.
    fn read(&mut self, off: u64, n: usize) -> Result<Vec<u8>, Bound>;
    /// Counts one walked header against [`HEADERS_MAX`] (only a file counts; bytes in memory are
    /// bounded by their size).
    fn count_header(&mut self) -> Result<(), Bound> {
        Ok(())
    }
}

/// Checks that `n` bytes at `off` lie inside `len` and within one payload read.
fn inside(off: u64, n: usize, len: u64) -> Result<(), Bound> {
    let n = n as u64;
    if n > PAYLOAD_MAX {
        return Err(Bound);
    }
    match off.checked_add(n) {
        Some(end) if end <= len => Ok(()),
        _ => Err(Bound),
    }
}

/// A file (or anything [`ReadSeek`]) with the per-file read budget and the header count.
struct FileSource<'a> {
    r: &'a mut dyn ReadSeek,
    len: u64,
    spent: u64,
    headers: u32,
    elapsed: &'a mut dyn FnMut() -> Duration,
}

impl FileSource<'_> {
    /// Counts one walked header against [`HEADERS_MAX`].
    fn header(&mut self) -> Result<(), Bound> {
        self.headers += 1;
        if self.headers > HEADERS_MAX {
            return Err(Bound);
        }
        Ok(())
    }
}

impl Bytes for FileSource<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn count_header(&mut self) -> Result<(), Bound> {
        self.header()
    }

    fn read(&mut self, off: u64, n: usize) -> Result<Vec<u8>, Bound> {
        inside(off, n, self.len)?;
        // A read costs at least a header's 8 bytes, so a loop of tiny or empty reads (fill bytes
        // in a JPEG, empty HEIF extents) ends at the budget after 2 MiB / 8 reads (LOC RF1).
        self.spent += (n as u64).max(READ_MIN);
        if self.spent > FILE_READ_MAX || (self.elapsed)() > TIME_MAX {
            return Err(Bound);
        }
        let mut buf = vec![0; n];
        self.r.seek(SeekFrom::Start(off)).map_err(|_| Bound)?;
        self.r.read_exact(&mut buf).map_err(|_| Bound)?;
        Ok(buf)
    }
}

/// Bytes in memory.
pub(crate) struct Slice<'a>(pub(crate) &'a [u8]);

impl Bytes for Slice<'_> {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read(&mut self, off: u64, n: usize) -> Result<Vec<u8>, Bound> {
        inside(off, n, self.len())?;
        let start = usize::try_from(off).map_err(|_| Bound)?;
        Ok(self.0[start..start + n].to_vec())
    }
}

/// The part `[base, base + len)` of another source; offsets are relative to `base`.
pub(crate) struct Window<'a> {
    pub(crate) inner: &'a mut dyn Bytes,
    pub(crate) base: u64,
    pub(crate) len: u64,
}

impl Bytes for Window<'_> {
    fn len(&self) -> u64 {
        self.len
    }

    fn read(&mut self, off: u64, n: usize) -> Result<Vec<u8>, Bound> {
        inside(off, n, self.len)?;
        self.inner.read(self.base + off, n)
    }

    fn count_header(&mut self) -> Result<(), Bound> {
        self.inner.count_header()
    }
}

/// APP1 of the main XMP packet.
const XMP_MAIN: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
/// APP1 of an Extended XMP segment: then a 32-byte GUID, the full length (u32), this segment's
/// offset (u32) and the data.
const XMP_EXTENSION: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";

/// The JPEG segment walk from SOI to the first SOS (or EOI): each APP1 `Exif\0\0` is a TIFF blob
/// whose offsets count from its own header. The APP1 XMP packets (the main one and the Extended
/// XMP segments, put together by GUID and offset) go to the XMP search after the walk, when no
/// EXIF location was found. Other segments are seeked over, not read.
fn jpeg(src: &mut FileSource<'_>) -> Result<Option<EvidenceKind>, Bound> {
    let len = src.len;
    let mut pos: u64 = 2;
    let mut broken = false;
    let mut main: Vec<Vec<u8>> = Vec::new();
    let mut extended: Vec<([u8; 32], u32, Vec<u8>)> = Vec::new();
    loop {
        src.header()?;
        if pos >= len {
            // Cut short at a segment boundary: what was there has been seen.
            break;
        }
        if src.read(pos, 1)?[0] != 0xff {
            return Err(Bound);
        }
        // Fill bytes before a marker.
        let mut m = pos + 1;
        let marker = loop {
            let b = src.read(m, 1)?[0];
            if b != 0xff {
                break b;
            }
            m += 1;
        };
        match marker {
            // SOS: the image data starts; EOI: the image ends. Metadata comes before either.
            0xda | 0xd9 => break,
            // Standalone markers without a length.
            0x01 | 0xd0..=0xd8 => pos = m + 1,
            _ => {
                let l = src.read(m + 1, 2)?;
                let seg_len = u64::from(u16::from_be_bytes([l[0], l[1]]));
                if seg_len < 2 {
                    return Err(Bound);
                }
                let payload = m + 3;
                let end = m + 1 + seg_len;
                if end > len {
                    return Err(Bound);
                }
                let payload_len = end - payload;
                let prefix = if marker == 0xe1 {
                    let n = payload_len.min(XMP_EXTENSION.len() as u64);
                    src.read(payload, usize::try_from(n).map_err(|_| Bound)?)?
                } else {
                    Vec::new()
                };
                if prefix.starts_with(XMP_MAIN) {
                    let at = payload + XMP_MAIN.len() as u64;
                    main.push(src.read(at, usize::try_from(end - at).map_err(|_| Bound)?)?);
                } else if prefix.starts_with(XMP_EXTENSION) {
                    let at = payload + XMP_EXTENSION.len() as u64;
                    if end - at < 40 {
                        broken = true;
                    } else {
                        let d = src.read(at, usize::try_from(end - at).map_err(|_| Bound)?)?;
                        let guid: [u8; 32] = d[..32].try_into().map_err(|_| Bound)?;
                        let offset = u32::from_be_bytes([d[36], d[37], d[38], d[39]]);
                        extended.push((guid, offset, d[40..].to_vec()));
                    }
                }
                if marker == 0xe1 && prefix.starts_with(b"Exif\0\0") {
                    let mut blob = Window {
                        inner: src,
                        base: payload + 6,
                        len: payload_len - 6,
                    };
                    match tiff::gps_in(&mut blob) {
                        Ok(true) => return Ok(Some(EvidenceKind::ExifGps)),
                        Ok(false) => {}
                        Err(Bound) => broken = true,
                    }
                }
                pos = end;
            }
        }
    }
    // Extended XMP: each GUID's segments in offset order, searched as one text (a name can be
    // split between two segments, critic F19).
    extended.sort_by_key(|(guid, offset, _)| (*guid, *offset));
    let mut parts = main;
    let mut last: Option<[u8; 32]> = None;
    for (guid, _, data) in extended {
        if last != Some(guid) {
            parts.push(Vec::new());
            last = Some(guid);
        }
        if let Some(p) = parts.last_mut() {
            p.extend(data);
        }
    }
    if xmp::parts_name_gps(&parts) {
        return Ok(Some(EvidenceKind::Xmp));
    }
    if broken { Err(Bound) } else { Ok(None) }
}

/// The JPEG XL container's signature box (ISO/IEC 18181-2): `JXL ` with `0D 0A 87 0A`.
const JXL_SIGNATURE: [u8; 12] = [0, 0, 0, 12, b'J', b'X', b'L', b' ', 0x0d, 0x0a, 0x87, 0x0a];

/// The JPEG XL container's top-level boxes after the signature: each `Exif` box is an EXIF payload
/// (a 32-bit offset, then TIFF); each `xml ` box is an XMP packet. `brob` (Brotli) is a named
/// gap; the codestream (`jxlc`, `jxlp`) is skipped by its header.
fn jxl(src: &mut FileSource<'_>) -> Result<Option<EvidenceKind>, Bound> {
    let mut boxes = bmff::Boxes::top_from(JXL_SIGNATURE.len() as u64, src.len);
    let mut broken = false;
    while let Some(b) = boxes.next(src)? {
        if b.typ == *b"Exif" {
            match bmff::exif_payload(&mut b.window(src)) {
                Ok(true) => return Ok(Some(EvidenceKind::ExifGps)),
                Ok(false) => {}
                Err(Bound) => broken = true,
            }
        } else if b.typ == *b"xml " {
            match xmp_box(src, &b) {
                Ok(true) => return Ok(Some(EvidenceKind::Xmp)),
                Ok(false) => {}
                Err(Bound) => broken = true,
            }
        }
    }
    if broken { Err(Bound) } else { Ok(None) }
}

/// Whether a box whose payload is an XMP packet names a location; a packet over
/// [`PAYLOAD_MAX`] is [`Bound`].
pub(crate) fn xmp_box(src: &mut dyn Bytes, b: &bmff::BoxHeader) -> Result<bool, Bound> {
    let n = b.payload_len().min(PAYLOAD_MAX + 1);
    let packet = src.read(b.payload, usize::try_from(n).map_err(|_| Bound)?)?;
    Ok(xmp::names_gps(&packet))
}

/// The types an ISO-BMFF file can start with: `ftyp`, and the boxes QuickTime files start with
/// when they have none (critic F12: a read-only look does not refuse them).
const ISO_FIRST: [&[u8; 4]; 7] = [
    b"ftyp", b"moov", b"mdat", b"wide", b"free", b"skip", b"pnot",
];

/// An ISO-BMFF file: the top-level boxes. A `meta` box holds HEIF items (HEIC, AVIF), whose
/// `Exif` items are EXIF payloads; `moov` is a movie's ([`movie`]); a `uuid` box of the XMP type
/// is an XMP packet. A broken place is remembered: a location found elsewhere in the file still
/// wins. `mdat`, `moof`, `free` and `skip` are passed over by their headers.
fn iso_file(src: &mut FileSource<'_>) -> Result<Option<(LookFormat, EvidenceKind)>, Bound> {
    let mut boxes = bmff::Boxes::top(src.len);
    let mut broken = false;
    while let Some(b) = boxes.next(src)? {
        match &b.typ {
            b"meta" => match heif_meta(src, &b) {
                Ok(Some(kind)) => return Ok(Some((LookFormat::Heif, kind))),
                Ok(None) => {}
                Err(Bound) => broken = true,
            },
            b"moov" => match movie::moov(src, &b) {
                Ok(Some(kind)) => return Ok(Some((LookFormat::Bmff, kind))),
                Ok(None) => {}
                Err(Bound) => broken = true,
            },
            b"uuid" if b.uuid == Some(movie::XMP_UUID) => match xmp_box(src, &b) {
                Ok(true) => return Ok(Some((LookFormat::Bmff, EvidenceKind::Xmp))),
                Ok(false) => {}
                Err(Bound) => broken = true,
            },
            _ => {}
        }
    }
    if broken { Err(Bound) } else { Ok(None) }
}

/// Whether the HEIF items of a top-level `meta` hold a location: its `Exif` items (EXIF
/// payloads) and its XMP `mime` items.
fn heif_meta(
    src: &mut FileSource<'_>,
    meta: &bmff::BoxHeader,
) -> Result<Option<EvidenceKind>, Bound> {
    let mut broken = false;
    for (kind, bytes) in heif::meta_items(src, meta)? {
        match (kind, bytes) {
            (heif::ItemKind::Exif, Ok(bytes)) => match bmff::exif_payload(&mut Slice(&bytes)) {
                Ok(true) => return Ok(Some(EvidenceKind::ExifGps)),
                Ok(false) => {}
                Err(Bound) => broken = true,
            },
            (heif::ItemKind::Xmp, Ok(bytes)) => {
                if xmp::names_gps(&bytes) {
                    return Ok(Some(EvidenceKind::Xmp));
                }
            }
            (_, Err(Bound)) => broken = true,
        }
    }
    if broken { Err(Bound) } else { Ok(None) }
}

#[cfg(test)]
mod tests;
