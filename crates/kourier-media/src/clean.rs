//! The one cleaning function for still pictures a network sends (LOC, BQ5, F14): a backend that
//! states `NetworkDescriptor::cleaned_pictures` sends every picture of those types through
//! [`clean_still`], which never returns bytes whose look is `Found`.

use std::io::Cursor;

use crate::location::{Look, look};
use crate::strip::{exif_blob, strip_metadata};
use crate::{Encoded, ScaleLevel, probe, scale_to_level, sniff_mime, tiff};

/// The MIME types [`clean_still`] cleans: what [`scale_to_level`] decodes, GIF left out (it is
/// sent as it is).
pub const CLEANED_TYPES: &[&str] = &["image/jpeg", "image/png", "image/webp", "image/bmp"];

/// Why [`clean_still`] gave no cleaned picture. Log-safe: it names no byte of the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NotCleaned {
    /// The bytes are not of a type in [`CLEANED_TYPES`] (by their magic, whatever their name or
    /// stated type says): the caller sends them as they are, because the core's verdict asked
    /// for exactly those bytes (LOC N7).
    #[error("not a picture type this build cleans")]
    NotMine,
    /// A picture of a cleaned type that could not be cleaned (it neither re-encodes nor strips,
    /// or the result still carries a location): that attachment must fail.
    #[error("the picture could not be cleaned of its metadata")]
    Refused,
}

/// Cleans a still picture for sending: re-encoded at `level` (every metadata block dropped);
/// a JPEG or PNG that does not re-encode is stripped instead ([`strip_metadata`], with only
/// the orientation kept, F11). Classified by MAGIC: a type outside [`CLEANED_TYPES`] is
/// [`NotCleaned::NotMine`]. The result is looked at once more: a picture that would still
/// leave with a location is [`NotCleaned::Refused`], never returned.
pub fn clean_still(bytes: &[u8], level: ScaleLevel) -> Result<Encoded, NotCleaned> {
    let mime = sniff_mime(bytes)
        .filter(|m| CLEANED_TYPES.contains(m))
        .ok_or(NotCleaned::NotMine)?;
    let out = match scale_to_level(bytes, level) {
        Ok(encoded) => encoded,
        Err(_) if matches!(mime, "image/jpeg" | "image/png") => {
            let stripped = strip_metadata(bytes).map_err(|_| NotCleaned::Refused)?;
            let (width, height) = dimensions(&stripped.bytes).ok_or(NotCleaned::Refused)?;
            Encoded {
                bytes: stripped.bytes,
                mime: stripped.mime,
                width,
                height,
            }
        }
        Err(_) => return Err(NotCleaned::Refused),
    };
    if located(&out.bytes) {
        return Err(NotCleaned::Refused);
    }
    Ok(out)
}

fn located(bytes: &[u8]) -> bool {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    matches!(look(&mut Cursor::new(bytes), len), Look::Found(_))
}

/// The display size of a stripped JPEG or PNG: what the decoder says, else the frame header
/// (an arithmetic-coded or lossless JPEG has one the decoder does not open), turned by the
/// EXIF orientation.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Ok(info) = probe(bytes) {
        return Some((info.width, info.height));
    }
    let (w, h) = header_dimensions(bytes)?;
    let turned = exif_blob(bytes)
        .and_then(tiff::orientation)
        .is_some_and(|o| (5..=8).contains(&o));
    Some(if turned { (h, w) } else { (w, h) })
}

fn header_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be32 = |b: &[u8]| u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    if bytes.get(12..16) == Some(b"IHDR") {
        let ihdr = bytes.get(16..24)?;
        return Some((be32(&ihdr[..4]), be32(&ihdr[4..])));
    }
    // JPEG: the first SOFn (C0-CF but DHT C4, JPG C8 and DAC CC) before the scan.
    let mut pos = 2;
    loop {
        if bytes.get(pos) != Some(&0xff) {
            return None;
        }
        let mut m = pos + 1;
        while bytes.get(m) == Some(&0xff) {
            m += 1;
        }
        match *bytes.get(m)? {
            0xd9 | 0xda => return None,
            0x01 | 0xd0..=0xd7 => pos = m + 1,
            marker => {
                let len = bytes.get(m + 1..m + 3)?;
                let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
                if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                    // length, precision, height, width.
                    let f = bytes.get(m + 4..m + 8)?;
                    let h = u32::from(u16::from_be_bytes([f[0], f[1]]));
                    let w = u32::from(u16::from_be_bytes([f[2], f[3]]));
                    return (w > 0 && h > 0).then_some((w, h));
                }
                if len < 2 {
                    return None;
                }
                pos = m + 1 + len;
            }
        }
    }
}

#[cfg(test)]
mod tests;
