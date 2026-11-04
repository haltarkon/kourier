//! The look's chunk walks for PNG and WebP (LOC). Pixel chunks (`IDAT`, `VP8 `, …) are passed
//! over by their headers, never read.
//!
//! PNG: `eXIf` (TIFF, sometimes behind `Exif\0\0`); `iTXt` with the keyword `XML:com.adobe.xmp`
//! (XMP, compressed when its flag says so); ImageMagick's `Raw profile type exif` / `… xmp` in
//! `tEXt`, `zTXt` or `iTXt` (hex text). A compressed text is inflated to at most
//! [`PAYLOAD_MAX`] bytes: more is [`Bound`] (a zip bomb, critic F17). WebP: the RIFF chunks
//! `EXIF` (TIFF, with or without `Exif\0\0`) and `XMP `.

use std::io::Read;

use crate::location::{Bound, Bytes, EvidenceKind, PAYLOAD_MAX, Window};
use crate::{tiff, xmp};

/// The PNG signature.
pub(crate) const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Found anywhere wins; a broken place is remembered and ends the walk Unreadable only when
/// nothing is found.
#[derive(Default)]
struct Verdict {
    broken: bool,
}

impl Verdict {
    /// Folds one place's answer: `Some(kind)` when it was found.
    fn take(&mut self, r: Result<Option<EvidenceKind>, Bound>) -> Option<EvidenceKind> {
        match r {
            Ok(found) => found,
            Err(Bound) => {
                self.broken = true;
                None
            }
        }
    }

    fn end(self) -> Result<Option<EvidenceKind>, Bound> {
        if self.broken { Err(Bound) } else { Ok(None) }
    }
}

/// A TIFF blob, maybe behind `Exif\0\0`, at `[off, off + len)` of `src`.
fn exif_at(src: &mut dyn Bytes, off: u64, len: u64) -> Result<Option<EvidenceKind>, Bound> {
    let skip = if len >= 6 && src.read(off, 6)? == *b"Exif\0\0" {
        6
    } else {
        0
    };
    let mut blob = Window {
        inner: src,
        base: off + skip,
        len: len - skip,
    };
    Ok(tiff::gps_in(&mut blob)?.then_some(EvidenceKind::ExifGps))
}

/// The PNG chunk walk from the signature to `IEND` (or the end of the file).
pub(crate) fn png(src: &mut dyn Bytes) -> Result<Option<EvidenceKind>, Bound> {
    let len = src.len();
    let mut pos = PNG_SIGNATURE.len() as u64;
    let mut v = Verdict::default();
    while pos < len {
        src.count_header()?;
        let h = src.read(pos, 8)?;
        let n = u64::from(u32::from_be_bytes([h[0], h[1], h[2], h[3]]));
        let typ = [h[4], h[5], h[6], h[7]];
        let data = pos + 8;
        // The data and its 4-byte CRC; a chunk length is at most 2^31 - 1.
        let end = data + n + 4;
        if n > 0x7fff_ffff || end > len {
            return Err(Bound);
        }
        let found = match &typ {
            b"IEND" => break,
            b"eXIf" => v.take(exif_at(src, data, n)),
            b"tEXt" | b"zTXt" | b"iTXt" => {
                let r = read_payload(src, data, n).and_then(|d| text_chunk(&typ, &d));
                v.take(r)
            }
            _ => None,
        };
        if found.is_some() {
            return Ok(found);
        }
        pos = end;
    }
    v.end()
}

/// `n` bytes at `off`, at most [`PAYLOAD_MAX`] (more is [`Bound`]).
fn read_payload(src: &mut dyn Bytes, off: u64, n: u64) -> Result<Vec<u8>, Bound> {
    if n > PAYLOAD_MAX {
        return Err(Bound);
    }
    src.read(off, usize::try_from(n).map_err(|_| Bound)?)
}

/// Splits at the first NUL: the part before and the part after it.
fn cut_nul(d: &[u8]) -> Result<(&[u8], &[u8]), Bound> {
    let at = d.iter().position(|&b| b == 0).ok_or(Bound)?;
    Ok((&d[..at], &d[at + 1..]))
}

/// A PNG text chunk: its keyword says what the text is; other keywords are nothing.
fn text_chunk(typ: &[u8; 4], d: &[u8]) -> Result<Option<EvidenceKind>, Bound> {
    let (keyword, rest) = cut_nul(d)?;
    let what = match keyword {
        b"XML:com.adobe.xmp" | b"Raw profile type xmp" => What::Xmp {
            hex: keyword.starts_with(b"Raw"),
        },
        b"Raw profile type exif" | b"Raw profile type APP1" => What::HexExif,
        _ => return Ok(None),
    };
    let text = match typ {
        b"tEXt" => rest.to_vec(),
        b"zTXt" => {
            // The compression method (0 = zlib), then the compressed text.
            let (&method, z) = rest.split_first().ok_or(Bound)?;
            if method != 0 {
                return Err(Bound);
            }
            inflate(z)?
        }
        _ => {
            // iTXt: the compression flag and method, a language tag, a translated keyword.
            let [flag, method, tail @ ..] = rest else {
                return Err(Bound);
            };
            let (_language, tail) = cut_nul(tail)?;
            let (_translated, t) = cut_nul(tail)?;
            match (flag, method) {
                (0, _) => t.to_vec(),
                (1, 0) => inflate(t)?,
                _ => return Err(Bound),
            }
        }
    };
    match what {
        What::Xmp { hex: false } => Ok(xmp::names_gps(&text).then_some(EvidenceKind::Xmp)),
        What::Xmp { hex: true } => {
            Ok(xmp::names_gps(&raw_profile(&text)?).then_some(EvidenceKind::Xmp))
        }
        What::HexExif => {
            let blob = raw_profile(&text)?;
            let tiff = blob.strip_prefix(b"Exif\0\0").unwrap_or(&blob);
            Ok(tiff::gps_present(tiff)?.then_some(EvidenceKind::ExifGps))
        }
    }
}

enum What {
    Xmp { hex: bool },
    HexExif,
}

/// Inflates a zlib stream to at most [`PAYLOAD_MAX`] bytes; a longer output is [`Bound`].
fn inflate(z: &[u8]) -> Result<Vec<u8>, Bound> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(z)
        .take(PAYLOAD_MAX + 1)
        .read_to_end(&mut out)
        .map_err(|_| Bound)?;
    if out.len() as u64 > PAYLOAD_MAX {
        return Err(Bound);
    }
    Ok(out)
}

/// ImageMagick's raw profile text: a newline, the profile's name, a newline, its byte count
/// in decimal, then the bytes in hex over lines. Returns the bytes.
fn raw_profile(text: &[u8]) -> Result<Vec<u8>, Bound> {
    let mut words = text
        .split(u8::is_ascii_whitespace)
        .filter(|w| !w.is_empty());
    let _name = words.next().ok_or(Bound)?;
    let count = words.next().ok_or(Bound)?;
    if count.is_empty() || count.len() > 8 || !count.iter().all(u8::is_ascii_digit) {
        return Err(Bound);
    }
    let count: usize = std::str::from_utf8(count)
        .map_err(|_| Bound)?
        .parse()
        .map_err(|_| Bound)?;
    if count as u64 > PAYLOAD_MAX {
        return Err(Bound);
    }
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(count.min(text.len() / 2));
    let mut digits = words.flatten().copied();
    while out.len() < count {
        let (Some(a), Some(b)) = (digits.next(), digits.next()) else {
            return Err(Bound);
        };
        out.push(hex(a).ok_or(Bound)? << 4 | hex(b).ok_or(Bound)?);
    }
    Ok(out)
}

/// The WebP RIFF chunk walk (`RIFF`, size, `WEBP`, then chunks with a little-endian size,
/// padded to an even length).
pub(crate) fn webp(src: &mut dyn Bytes) -> Result<Option<EvidenceKind>, Bound> {
    let h = src.read(0, 12)?;
    let riff = u64::from(u32::from_le_bytes([h[4], h[5], h[6], h[7]]));
    // A RIFF size past the file: the file is what is there.
    let len = (riff + 8).min(src.len());
    let mut pos = 12;
    let mut v = Verdict::default();
    while pos + 8 <= len {
        src.count_header()?;
        let c = src.read(pos, 8)?;
        let n = u64::from(u32::from_le_bytes([c[4], c[5], c[6], c[7]]));
        let data = pos + 8;
        if data + n > len {
            return Err(Bound);
        }
        let found = match &c[..4] {
            b"EXIF" => v.take(exif_at(src, data, n)),
            b"XMP " => {
                let r = read_payload(src, data, n)
                    .map(|p| xmp::names_gps(&p).then_some(EvidenceKind::Xmp));
                v.take(r)
            }
            _ => None,
        };
        if found.is_some() {
            return Ok(found);
        }
        pos = data + n + (n & 1);
    }
    v.end()
}
