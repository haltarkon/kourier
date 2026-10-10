//! Metadata stripping (EXIF with GPS, XMP, IPTC, comments, text chunks) for JPEG and PNG.

use std::io::Cursor;

use image::metadata::Orientation;
use image::{ImageDecoder, ImageReader};

use crate::MediaError;
use crate::decode::decode;

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
/// ICC profile and the Adobe colour-transform marker) or a PNG (eXIf, tEXt, zTXt, iTXt, tIME).
/// Bytes after the end of the image (JPEG EOI, PNG IEND), such as a motion photo's video, are
/// dropped too. When an EXIF orientation other than "normal" is present, the image is decoded, rotated and
/// re-encoded instead, so it still shows upright. Other formats: [`MediaError::Unsupported`]
/// (use [`scale_to_level`](crate::scale_to_level), which always re-encodes).
pub fn strip_metadata(bytes: &[u8]) -> Result<Stripped, MediaError> {
    let mime = if bytes.starts_with(&[0xff, 0xd8]) {
        "image/jpeg"
    } else if bytes.starts_with(&PNG_SIG) {
        "image/png"
    } else {
        return Err(MediaError::Unsupported);
    };
    if orientation(bytes)? != Orientation::NoTransforms {
        let e = decode(bytes)?.reencode()?;
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
    Ok(Stripped {
        bytes: out,
        mime,
        reencoded: false,
    })
}

fn orientation(bytes: &[u8]) -> Result<Orientation, MediaError> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| MediaError::Decode(e.to_string()))?;
    let mut decoder = reader.into_decoder().map_err(MediaError::from_image)?;
    Ok(decoder.orientation().unwrap_or(Orientation::NoTransforms))
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
        let drop = matches!(kind, b"eXIf" | b"tEXt" | b"zTXt" | b"iTXt" | b"tIME");
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
