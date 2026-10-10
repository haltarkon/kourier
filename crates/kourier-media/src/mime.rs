//! Content types: magic-byte sniffing and file-name extensions.

/// The MIME type of a file from its first bytes (16 are enough), for the formats a messenger
/// sends: images, audio, video, PDF and ZIP. `None` when unknown.
pub fn sniff_mime(head: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| head.starts_with(sig);
    let at = |offset: usize, sig: &[u8]| head.get(offset..offset + sig.len()) == Some(sig);
    if starts(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if starts(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("image/png");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"RIFF") && at(8, b"WEBP") {
        return Some("image/webp");
    }
    if starts(b"RIFF") && at(8, b"WAVE") {
        return Some("audio/wav");
    }
    // "BM" alone starts plenty of text files: also require the zero reserved field and a known
    // DIB header size (BITMAPCOREHEADER … BITMAPV5HEADER).
    if starts(b"BM")
        && at(6, &[0, 0, 0, 0])
        && head
            .get(14..18)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .is_some_and(|dib| matches!(dib, 12 | 16 | 40 | 52 | 56 | 64 | 108 | 124))
    {
        return Some("image/bmp");
    }
    if at(4, b"ftyp") {
        let brand = head.get(8..12)?;
        return Some(match brand {
            b"heic" | b"heix" | b"hevc" | b"heim" | b"heis" => "image/heic",
            b"mif1" | b"msf1" => "image/heif",
            b"avif" | b"avis" => "image/avif",
            b"M4A " | b"M4B " => "audio/mp4",
            b"qt  " => "video/quicktime",
            _ => "video/mp4",
        });
    }
    if starts(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some("video/webm");
    }
    if starts(b"OggS") {
        return Some("audio/ogg");
    }
    if starts(b"fLaC") {
        return Some("audio/flac");
    }
    if starts(b"ID3") || starts(&[0xff, 0xfb]) || starts(&[0xff, 0xf3]) || starts(&[0xff, 0xf2]) {
        return Some("audio/mpeg");
    }
    if starts(&[0xff, 0xf1]) || starts(&[0xff, 0xf9]) {
        return Some("audio/aac");
    }
    if starts(b"%PDF-") {
        return Some("application/pdf");
    }
    if starts(b"PK\x03\x04") {
        return Some("application/zip");
    }
    None
}

const TABLE: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("avif", "image/avif"),
    ("mp4", "video/mp4"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("webm", "video/webm"),
    ("mkv", "video/x-matroska"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("ogg", "audio/ogg"),
    ("oga", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("flac", "audio/flac"),
    ("wav", "audio/wav"),
    ("pdf", "application/pdf"),
    ("zip", "application/zip"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("json", "application/json"),
];

/// The MIME type for a file-name extension (case-insensitive, without the dot).
pub fn mime_from_extension(ext: &str) -> Option<&'static str> {
    let ext = ext.to_ascii_lowercase();
    TABLE.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// The usual extension for a MIME type (without the dot), for plaintext media files Qt opens by
/// name.
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    let mime = mime.split(';').next().unwrap_or("").trim();
    if mime.eq_ignore_ascii_case("image/jpg") {
        return Some("jpg");
    }
    TABLE
        .iter()
        .find(|(_, m)| m.eq_ignore_ascii_case(mime))
        .map(|(e, _)| *e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(sniff_mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"\0\0\0\x18ftypisom"), Some("video/mp4"));
        assert_eq!(sniff_mime(b"\0\0\0\x18ftypheic"), Some("image/heic"));
        assert_eq!(sniff_mime(b"\0\0\0\x18ftypM4A "), Some("audio/mp4"));
        assert_eq!(sniff_mime(b"OggS\0"), Some("audio/ogg"));
        assert_eq!(sniff_mime(b"%PDF-1.7"), Some("application/pdf"));
        assert_eq!(sniff_mime(b"hello"), None);
        let mut bmp = Vec::new();
        image::RgbImage::new(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut bmp), image::ImageFormat::Bmp)
            .expect("bmp");
        assert_eq!(sniff_mime(&bmp), Some("image/bmp"));
        assert_eq!(sniff_mime(b"BMI results for the week: fine"), None);
        assert_eq!(sniff_mime(b""), None);
    }

    #[test]
    fn extensions_both_ways() {
        assert_eq!(mime_from_extension("JPG"), Some("image/jpeg"));
        assert_eq!(mime_from_extension("exe"), None);
        assert_eq!(extension_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for_mime("audio/ogg; codecs=opus"), Some("ogg"));
        assert_eq!(extension_for_mime("application/x-unknown"), None);
    }
}
