//! Metadata stripping (EXIF with GPS, XMP, IPTC, comments, text chunks) for JPEG and PNG.

use crate::MediaError;
use crate::decode::decode;
use crate::tiff;

/// The result of [`strip_metadata`].
#[derive(Clone, PartialEq, Eq)]
pub struct Stripped {
    /// The image without metadata.
    pub bytes: Vec<u8>,
    /// MIME type of `bytes`.
    pub mime: &'static str,
    /// The pixels were re-encoded (to apply an EXIF orientation); otherwise only metadata blocks
    /// were removed and the compressed image data is unchanged.
    pub reencoded: bool,
}

impl std::fmt::Debug for Stripped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stripped")
            .field("len", &self.bytes.len())
            .field("mime", &self.mime)
            .field("reencoded", &self.reencoded)
            .finish()
    }
}

const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// Removes metadata from a JPEG (APP1 EXIF/XMP, APP13 IPTC, other APPn, COM; keeps JFIF, the
/// ICC profile and the Adobe colour-transform marker) or a PNG (eXIf, tEXt, zTXt, iTXt, tIME, and caBX, the C2PA manifest, whose
/// `stds.exif` assertion can hold GPS). Bytes after the end of the image (JPEG EOI, PNG IEND), such as a motion photo's video, are
/// dropped too. When an EXIF orientation other than "normal" is present, the image is decoded, rotated and
/// re-encoded instead, so it still shows upright; when that decode or encode fails (an
/// arithmetic-coded or lossless JPEG, a damaged file), the metadata is stripped all the same and
/// a minimal EXIF holding ONLY the orientation is written back (LOC, F11). The orientation is
/// read with the crate's own TIFF walker, so a file the decoders cannot open is still stripped;
/// an orientation that cannot be read counts as "normal". Other formats:
/// [`MediaError::Unsupported`] (use [`scale_to_level`](crate::scale_to_level), which always
/// re-encodes).
pub fn strip_metadata(bytes: &[u8]) -> Result<Stripped, MediaError> {
    let mime = if bytes.starts_with(&[0xff, 0xd8]) {
        "image/jpeg"
    } else if bytes.starts_with(&PNG_SIG) {
        "image/png"
    } else {
        return Err(MediaError::Unsupported);
    };
    let turned = exif_blob(bytes)
        .and_then(tiff::orientation)
        .filter(|o| (2..=8).contains(o));
    if turned.is_some()
        && let Ok(e) = decode(bytes).and_then(|d| d.reencode())
    {
        return Ok(Stripped {
            bytes: e.bytes,
            mime: e.mime,
            reencoded: true,
        });
    }
    let out = if mime == "image/jpeg" {
        strip_jpeg(bytes)?
    } else {
        strip_png(bytes)?
    };
    let out = match turned {
        Some(o) => with_orientation(out, o),
        None => out,
    };
    Ok(Stripped {
        bytes: out,
        mime,
        reencoded: false,
    })
}

/// The EXIF (TIFF) blob of a JPEG (the first APP1 `Exif\0\0` before the first scan) or a PNG
/// (the `eXIf` chunk before the image data), if there is one.
pub(crate) fn exif_blob(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.starts_with(&PNG_SIG) {
        let mut pos = PNG_SIG.len();
        loop {
            let len = bytes.get(pos..pos + 4)?;
            let len = usize::try_from(u32::from_be_bytes([len[0], len[1], len[2], len[3]])).ok()?;
            let kind = bytes.get(pos + 4..pos + 8)?;
            let data = bytes.get(pos + 8..pos.checked_add(8)?.checked_add(len)?)?;
            match kind {
                b"eXIf" => return Some(data.strip_prefix(b"Exif\0\0").unwrap_or(data)),
                b"IDAT" | b"IEND" => return None,
                _ => pos += 12 + len,
            }
        }
    }
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return None;
    }
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
                if len < 2 {
                    return None;
                }
                let payload = bytes.get(m + 3..m + 1 + len)?;
                if marker == 0xe1
                    && let Some(tiff) = payload.strip_prefix(b"Exif\0\0")
                {
                    return Some(tiff);
                }
                pos = m + 1 + len;
            }
        }
    }
}

/// A big-endian TIFF whose IFD0 holds only the orientation `o`.
fn orientation_tiff(o: u16) -> Vec<u8> {
    let mut t = b"MM\0*".to_vec();
    t.extend(8u32.to_be_bytes());
    t.extend(1u16.to_be_bytes());
    t.extend(0x0112u16.to_be_bytes());
    t.extend(3u16.to_be_bytes());
    t.extend(1u32.to_be_bytes());
    t.extend(o.to_be_bytes());
    t.extend([0, 0]);
    t.extend(0u32.to_be_bytes());
    t
}

/// `stripped` (a JPEG or PNG without metadata) with a minimal EXIF of the orientation `o` put
/// back: a JPEG APP1 after SOI (after JFIF's APP0 when it leads), a PNG `eXIf` after IHDR.
pub(crate) fn with_orientation(stripped: Vec<u8>, o: u16) -> Vec<u8> {
    let tiff = orientation_tiff(o);
    if stripped.starts_with(&PNG_SIG) {
        // IHDR is always the first chunk: 8 + 4 + 4 + 13 + 4.
        let at = PNG_SIG.len() + 25;
        if stripped.get(PNG_SIG.len() + 4..PNG_SIG.len() + 8) != Some(b"IHDR")
            || stripped.len() < at
        {
            return stripped;
        }
        let mut chunk = u32::try_from(tiff.len())
            .unwrap_or(0)
            .to_be_bytes()
            .to_vec();
        chunk.extend(b"eXIf");
        chunk.extend(&tiff);
        let mut crc = flate2::Crc::new();
        crc.update(&chunk[4..]);
        chunk.extend(crc.sum().to_be_bytes());
        let mut out = stripped[..at].to_vec();
        out.extend(chunk);
        out.extend(&stripped[at..]);
        return out;
    }
    let mut at = 2;
    if stripped.get(2..4) == Some(&[0xff, 0xe0])
        && let Some(len) = stripped.get(4..6)
    {
        at = (4 + usize::from(u16::from_be_bytes([len[0], len[1]]))).min(stripped.len());
    }
    let mut seg = vec![0xff, 0xe1];
    seg.extend(u16::try_from(2 + 6 + tiff.len()).unwrap_or(0).to_be_bytes());
    seg.extend(b"Exif\0\0");
    seg.extend(tiff);
    let mut out = stripped[..at].to_vec();
    out.extend(seg);
    out.extend(&stripped[at..]);
    out
}

fn malformed(what: &str) -> MediaError {
    MediaError::Decode(format!("malformed {what}"))
}

fn strip_jpeg(bytes: &[u8]) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&bytes[..2]);
    let mut pos = 2;
    let mut scanned = false;
    loop {
        if scanned && pos >= bytes.len() {
            // Truncated after its last scan (no EOI): decoders accept that, so do we.
            return Ok(out);
        }
        if bytes.get(pos) != Some(&0xff) {
            return Err(malformed("JPEG marker"));
        }
        // Fill bytes before a marker.
        let mut m = pos + 1;
        while bytes.get(m) == Some(&0xff) {
            m += 1;
        }
        let marker = *bytes.get(m).ok_or_else(|| malformed("JPEG"))?;
        let seg_start = m - 1;
        match marker {
            // EOI: the image ends here. Anything appended after it (a motion photo's video,
            // another program's trailer) is not part of the picture and goes too.
            0xd9 => {
                out.extend_from_slice(&bytes[seg_start..=m]);
                return Ok(out);
            }
            // Standalone markers without a length.
            0x01 | 0xd0..=0xd7 => {
                out.extend_from_slice(&bytes[seg_start..=m]);
                pos = m + 1;
            }
            _ => {
                let len = bytes
                    .get(m + 1..m + 3)
                    .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
                    .ok_or_else(|| malformed("JPEG segment"))?;
                if len < 2 {
                    return Err(malformed("JPEG segment length"));
                }
                let end = m + 1 + len;
                let seg = bytes
                    .get(seg_start..end)
                    .ok_or_else(|| malformed("JPEG segment"))?;
                let payload = &bytes[m + 3..end];
                let keep = match marker {
                    0xe0 => payload.starts_with(b"JFIF\0") || payload.starts_with(b"JFXX\0"),
                    0xe2 => payload.starts_with(b"ICC_PROFILE\0"),
                    0xee => payload.starts_with(b"Adobe"),
                    0xe1 | 0xe3..=0xed | 0xef | 0xfe => false,
                    _ => true,
                };
                if keep {
                    out.extend_from_slice(seg);
                }
                pos = end;
                if marker == 0xda {
                    // SOS: the entropy-coded data runs to the next marker (progressive images
                    // have several scans, with tables and maybe metadata between them).
                    let data_end = entropy_end(bytes, pos);
                    out.extend_from_slice(&bytes[pos..data_end]);
                    pos = data_end;
                    scanned = true;
                }
            }
        }
    }
}

/// Where the entropy-coded data that starts at `pos` ends: the first `0xFF` that is neither a
/// stuffed zero byte nor a restart marker (the end of `bytes` when no marker follows).
fn entropy_end(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() {
        if bytes[pos] == 0xff {
            match bytes.get(pos + 1) {
                Some(0x00 | 0xd0..=0xd7) => {
                    pos += 2;
                    continue;
                }
                _ => return pos,
            }
        }
        pos += 1;
    }
    bytes.len()
}

fn strip_png(bytes: &[u8]) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(&PNG_SIG);
    let mut pos = PNG_SIG.len();
    while pos < bytes.len() {
        let len = bytes
            .get(pos..pos + 4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| malformed("PNG chunk"))?;
        let len = usize::try_from(len).map_err(|_| malformed("PNG chunk"))?;
        let end = pos
            .checked_add(12)
            .and_then(|p| p.checked_add(len))
            .ok_or_else(|| malformed("PNG chunk"))?;
        let chunk = bytes.get(pos..end).ok_or_else(|| malformed("PNG chunk"))?;
        let kind = &chunk[4..8];
        let drop = matches!(
            kind,
            b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME" | b"caBX"
        );
        if !drop {
            out.extend_from_slice(chunk);
        }
        pos = end;
        if kind == b"IEND" {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SampleFormat, probe, sample_image};

    /// An APP1 EXIF segment with one IFD0 entry: orientation = `value`, plus a fake GPS marker
    /// string that must not survive.
    fn exif_app1(value: u16) -> Vec<u8> {
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II*\0");
        tiff.extend_from_slice(&8u32.to_le_bytes());
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff.extend_from_slice(b"GPS 48.8584N 2.2945E");
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend_from_slice(&tiff);
        let mut seg = vec![0xff, 0xe1];
        seg.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
        seg.extend_from_slice(&payload);
        seg
    }

    fn with_exif(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&exif_app1(orientation));
        // A comment segment too.
        out.extend_from_slice(&[0xff, 0xfe, 0, 7]);
        out.extend_from_slice(b"hello");
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn jpeg_exif_and_comments_go_without_reencoding() {
        let jpeg = sample_image(80, 40, 5, SampleFormat::Jpeg).unwrap();
        let tagged = with_exif(&jpeg, 1);
        assert!(tagged.windows(3).any(|w| w == b"GPS"));
        let s = strip_metadata(&tagged).unwrap();
        assert!(!s.reencoded);
        assert_eq!(s.mime, "image/jpeg");
        assert!(!s.bytes.windows(4).any(|w| w == b"Exif"));
        assert!(!s.bytes.windows(3).any(|w| w == b"GPS"));
        assert!(!s.bytes.windows(5).any(|w| w == b"hello"));
        // The original had no metadata: stripping the tagged copy gives it back exactly.
        assert_eq!(s.bytes, jpeg);

        // Data appended after EOI (a motion photo's video, a trailer) goes too.
        let mut trailed = tagged.clone();
        trailed.extend_from_slice(b"\0\0\0\x18ftypmp42 GPS 48.8584N video");
        let s = strip_metadata(&trailed).unwrap();
        assert_eq!(s.bytes, jpeg);
        // A file cut off after its scan (no EOI) keeps what it has.
        let cut = &jpeg[..jpeg.len() - 2];
        assert_eq!(strip_metadata(cut).unwrap().bytes, cut);
    }

    #[test]
    fn progressive_scans_keep_their_data_and_lose_metadata_between_them() {
        let jpeg = sample_image(64, 48, 7, SampleFormat::Jpeg).unwrap();
        // Split the scan: a COM segment between SOS data and EOI, as a progressive file can have
        // between scans, and stuffed bytes and a restart marker inside the data.
        let eoi = jpeg.len() - 2;
        let mut tagged = jpeg[..eoi].to_vec();
        tagged.extend_from_slice(&[0xff, 0xfe, 0, 6]);
        tagged.extend_from_slice(b"gps!");
        tagged.extend_from_slice(&jpeg[eoi..]);
        let s = strip_metadata(&tagged).unwrap();
        assert_eq!(s.bytes, jpeg);
        assert_eq!(
            entropy_end(&[1, 0xff, 0x00, 2, 0xff, 0xd3, 3, 0xff, 0xd9], 0),
            7
        );
        assert_eq!(entropy_end(&[1, 2, 3], 0), 3);
    }

    #[test]
    fn a_rotated_jpeg_is_turned_upright_and_reencoded() {
        let jpeg = sample_image(80, 40, 5, SampleFormat::Jpeg).unwrap();
        let tagged = with_exif(&jpeg, 6);
        assert_eq!(probe(&tagged).unwrap().width, 40);
        let s = strip_metadata(&tagged).unwrap();
        assert!(s.reencoded);
        assert!(!s.bytes.windows(3).any(|w| w == b"GPS"));
        let info = probe(&s.bytes).unwrap();
        assert_eq!((info.width, info.height), (40, 80));
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for b in data {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    #[test]
    fn a_png_c2pa_manifest_goes() {
        // LOC P5: `caBX` (C2PA) can hold a GPS assertion; an unknown ancillary chunk passes, this
        // one must not.
        let png = sample_image(20, 10, 2, SampleFormat::Png).unwrap();
        let mut tagged = png[..33].to_vec();
        let data = b"jumb manifest with exif GPSLatitude";
        tagged.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut typed = b"caBX".to_vec();
        typed.extend_from_slice(data);
        tagged.extend_from_slice(&typed);
        tagged.extend_from_slice(&crc32(&typed).to_be_bytes());
        tagged.extend_from_slice(&png[33..]);
        assert!(tagged.windows(4).any(|w| w == b"caBX"));
        let s = strip_metadata(&tagged).unwrap();
        assert!(!s.reencoded);
        assert!(!s.bytes.windows(4).any(|w| w == b"caBX"));
        assert_eq!(s.bytes, png);
    }

    #[test]
    fn png_text_chunks_go() {
        let png = sample_image(20, 10, 2, SampleFormat::Png).unwrap();
        // Insert a tEXt chunk after IHDR (8 + 25 bytes).
        let mut tagged = png[..33].to_vec();
        let data = b"Comment\0secret place";
        tagged.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
        let mut typed = b"tEXt".to_vec();
        typed.extend_from_slice(data);
        tagged.extend_from_slice(&typed);
        tagged.extend_from_slice(&crc32(&typed).to_be_bytes());
        tagged.extend_from_slice(&png[33..]);
        let s = strip_metadata(&tagged).unwrap();
        assert!(!s.reencoded);
        assert_eq!(s.bytes, png);
        assert!(matches!(
            strip_metadata(b"GIF89a"),
            Err(MediaError::Unsupported)
        ));
        assert!(strip_metadata(&[0xff, 0xd8, 0x00]).is_err());
    }
}
