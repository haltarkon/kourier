use super::*;
use crate::{EvidenceKind, SampleFormat, SampleLocated, decode, sample_image, sample_located};

const LEVEL: ScaleLevel = ScaleLevel::THREE;

fn look_of(bytes: &[u8]) -> Look {
    look(&mut Cursor::new(bytes), bytes.len() as u64)
}

fn is_found(bytes: &[u8]) -> bool {
    matches!(look_of(bytes), Look::Found(_))
}

/// A big-endian TIFF: IFD0 with the orientation `o` and the GPS IFD pointer, the GPS IFD with a
/// made-up latitude and longitude (synthetic values).
fn gps_tiff_turned(o: u16) -> Vec<u8> {
    let entry = |tag: u16, typ: u16, count: u32, value: [u8; 4]| {
        let mut e = tag.to_be_bytes().to_vec();
        e.extend(typ.to_be_bytes());
        e.extend(count.to_be_bytes());
        e.extend(value);
        e
    };
    // Header 8; IFD0 at 8 (2 + 2 * 12 + 4 = 30); GPS IFD at 38 (30); values at 68 and 92.
    let mut t = b"MM\0*".to_vec();
    t.extend(8u32.to_be_bytes());
    t.extend(2u16.to_be_bytes());
    let mut ov = o.to_be_bytes().to_vec();
    ov.extend([0, 0]);
    t.extend(entry(0x0112, 3, 1, ov.try_into().unwrap()));
    t.extend(entry(0x8825, 4, 1, 38u32.to_be_bytes()));
    t.extend(0u32.to_be_bytes());
    t.extend(2u16.to_be_bytes());
    t.extend(entry(2, 5, 3, 68u32.to_be_bytes()));
    t.extend(entry(4, 5, 3, 92u32.to_be_bytes()));
    t.extend(0u32.to_be_bytes());
    for (n, d) in [(11u32, 1u32), (22, 1), (33, 1), (44, 1), (55, 1), (6, 1)] {
        t.extend(n.to_be_bytes());
        t.extend(d.to_be_bytes());
    }
    t
}

/// `jpeg` with an APP1 EXIF segment holding `tiff` right after SOI.
fn with_app1(jpeg: &[u8], tiff: &[u8]) -> Vec<u8> {
    let mut payload = b"Exif\0\0".to_vec();
    payload.extend(tiff);
    let mut out = jpeg[..2].to_vec();
    out.extend([0xff, 0xe1]);
    out.extend(u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    out.extend(payload);
    out.extend(&jpeg[2..]);
    out
}

/// `jpeg` with its baseline frame header (SOF0) turned into an arithmetic-coded one (SOF9): no
/// decoder here opens it.
fn arithmetic(jpeg: &[u8]) -> Vec<u8> {
    let mut out = jpeg.to_vec();
    let mut pos = 2;
    loop {
        assert_eq!(out[pos], 0xff, "a marker at {pos}");
        let marker = out[pos + 1];
        assert_ne!(marker, 0xda, "no SOF0 before the scan");
        if marker == 0xc0 {
            out[pos + 1] = 0xc9;
            return out;
        }
        pos += 2 + usize::from(u16::from_be_bytes([out[pos + 2], out[pos + 3]]));
    }
}

/// F11: an arithmetic-coded JPEG with EXIF GPS and orientation 6 leaves without its GPS, with
/// only the orientation kept, and with its display size turned.
#[test]
fn an_arithmetic_jpeg_with_gps_and_orientation_six_leaves_without_gps() {
    let plain = sample_image(64, 48, 3, SampleFormat::Jpeg).unwrap();
    let located = arithmetic(&with_app1(&plain, &gps_tiff_turned(6)));
    assert!(is_found(&located), "the input carries a location");
    assert!(decode(&located).is_err(), "no decoder here opens it");
    let out = clean_still(&located, LEVEL).expect("stripped, not refused");
    assert_eq!(out.mime, "image/jpeg");
    assert!(!is_found(&out.bytes), "no location leaves");
    assert_eq!(
        exif_blob(&out.bytes).and_then(tiff::orientation),
        Some(6),
        "the orientation stays"
    );
    assert_eq!(
        (out.width, out.height),
        (48, 64),
        "the display size, turned"
    );
    // The same through the stripper alone.
    let stripped = strip_metadata(&located).expect("strips");
    assert!(!stripped.reencoded);
    assert!(!is_found(&stripped.bytes));
    assert_eq!(
        exif_blob(&stripped.bytes).and_then(tiff::orientation),
        Some(6)
    );
}

/// A turned PNG that does not re-encode keeps its orientation in an `eXIf` chunk (with a valid
/// CRC: the decoders read it back).
#[test]
fn a_minimal_orientation_exif_is_written_into_a_png_too() {
    let png = sample_image(20, 10, 1, SampleFormat::Png).unwrap();
    let out = crate::strip::with_orientation(png, 8);
    assert_eq!(exif_blob(&out).and_then(tiff::orientation), Some(8));
    let info = probe(&out).expect("still a PNG the decoder reads");
    assert_eq!(
        (info.width, info.height),
        (10, 20),
        "the decoder sees the orientation"
    );
}

/// `clean_still` never returns bytes whose look is Found, over the located samples; what it does
/// not clean is `NotMine`, by the magic.
#[test]
fn clean_still_never_returns_located_bytes() {
    for kind in SampleLocated::ALL {
        let located = sample_located(kind);
        assert!(is_found(&located), "{kind:?} is located");
        match clean_still(&located, LEVEL) {
            Ok(out) => {
                assert_eq!(
                    kind,
                    SampleLocated::Jpeg,
                    "only the JPEG is a picture it cleans"
                );
                assert!(!is_found(&out.bytes), "{kind:?}: no location leaves");
            }
            Err(e) => {
                assert_ne!(kind, SampleLocated::Jpeg, "the JPEG decodes: cleaned");
                assert_eq!(e, NotCleaned::NotMine, "{kind:?}");
            }
        }
    }
    // A located PNG (eXIf) and a turned located JPEG that decodes.
    let png = sample_image(30, 20, 2, SampleFormat::Png).unwrap();
    let png = with_exif_chunk(&png, &gps_tiff_turned(1));
    assert!(matches!(
        look_of(&png),
        Look::Found(e) if e.kind == EvidenceKind::ExifGps
    ));
    let out = clean_still(&png, LEVEL).unwrap();
    assert!(!is_found(&out.bytes));
    let jpeg = with_app1(
        &sample_image(40, 30, 2, SampleFormat::Jpeg).unwrap(),
        &gps_tiff_turned(6),
    );
    assert!(is_found(&jpeg));
    let out = clean_still(&jpeg, LEVEL).unwrap();
    assert!(!is_found(&out.bytes));
    assert_eq!(
        (out.width, out.height),
        (30, 40),
        "turned upright by the re-encode"
    );
    // GIF is not cleaned here (sent as it is).
    let gif = sample_image(8, 8, 1, SampleFormat::Gif).unwrap();
    assert_eq!(clean_still(&gif, LEVEL), Err(NotCleaned::NotMine));
}

/// A located picture of a cleaned type that neither re-encodes nor strips is refused, never
/// returned as it came.
#[test]
fn a_located_picture_that_cannot_be_cleaned_is_refused() {
    // A WebP whose image data is broken, with an EXIF chunk holding GPS.
    let mut chunks = Vec::new();
    let mut vp8x = vec![0x08, 0, 0, 0];
    vp8x.extend([0x0f, 0, 0, 0x0f, 0, 0]);
    for (typ, data) in [
        (b"VP8X", vp8x),
        (b"EXIF", gps_tiff_turned(1)),
        (b"VP8 ", vec![1, 2, 3, 4]),
    ] {
        chunks.extend(typ);
        chunks.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        chunks.extend(&data);
        if data.len() % 2 == 1 {
            chunks.push(0);
        }
    }
    let mut webp = b"RIFF".to_vec();
    webp.extend(u32::try_from(chunks.len() + 4).unwrap().to_le_bytes());
    webp.extend(b"WEBP");
    webp.extend(chunks);
    assert!(is_found(&webp), "the WebP carries a location");
    assert_eq!(clean_still(&webp, LEVEL), Err(NotCleaned::Refused));
    // A JPEG broken after its EXIF: no decode, no strip.
    let mut jpeg = with_app1(&[0xff, 0xd8], &gps_tiff_turned(1));
    jpeg.extend(b"not a marker");
    assert!(is_found(&jpeg));
    assert_eq!(clean_still(&jpeg, LEVEL), Err(NotCleaned::Refused));
    // The errors name no byte of the input.
    assert_eq!(
        NotCleaned::Refused.to_string(),
        "the picture could not be cleaned of its metadata"
    );
}

/// `png` with an `eXIf` chunk of `tiff` after IHDR.
fn with_exif_chunk(png: &[u8], tiff: &[u8]) -> Vec<u8> {
    let mut chunk = u32::try_from(tiff.len()).unwrap().to_be_bytes().to_vec();
    chunk.extend(b"eXIf");
    chunk.extend(tiff);
    let mut crc = flate2::Crc::new();
    crc.update(&chunk[4..]);
    chunk.extend(crc.sum().to_be_bytes());
    let mut out = png[..33].to_vec();
    out.extend(chunk);
    out.extend(&png[33..]);
    out
}
