//! Deterministic synthetic pictures for fakes, fixtures and tests (no real photos), and synthetic
//! files that carry a made-up location (mid-Atlantic, never real data) for the LOC tests.

use std::io::Cursor;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

use crate::MediaError;
use crate::decode::encode_jpeg;

/// Container of a [`sample_image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// JPEG, quality 85.
    Jpeg,
    /// Opaque PNG.
    Png,
    /// PNG with a transparent border (a sticker-like image).
    PngAlpha,
    /// A single-frame GIF.
    Gif,
}

impl SampleFormat {
    /// The MIME type of the container.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png | Self::PngAlpha => "image/png",
            Self::Gif => "image/gif",
        }
    }
}

/// SplitMix64.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn byte(v: u64, shift: u32) -> u8 {
    u8::try_from((v >> shift) & 0xff).unwrap_or(0)
}

/// A `width` × `height` picture (each edge clamped to 1…4096): a two-colour gradient with a few
/// discs, all chosen by `seed`. The same arguments always give the same bytes.
pub fn sample_image(
    width: u32,
    height: u32,
    seed: u64,
    format: SampleFormat,
) -> Result<Vec<u8>, MediaError> {
    let (w, h) = (width.clamp(1, 4096), height.clamp(1, 4096));
    let r = mix(seed);
    let (a, b) = (
        [byte(r, 0), byte(r, 8), byte(r, 16)],
        [byte(r, 24), byte(r, 32), byte(r, 40)],
    );
    let discs: Vec<(i64, i64, i64, [u8; 3])> = (0..4u64)
        .map(|i| {
            let d = mix(seed ^ (i + 1).wrapping_mul(0x5851_f42d_4c95_7f2d));
            let cx = i64::try_from(d % u64::from(w)).unwrap_or(0);
            let cy = i64::try_from((d >> 16) % u64::from(h)).unwrap_or(0);
            let rad = i64::from(w.min(h) / 8 + 1) + i64::try_from((d >> 32) % 32).unwrap_or(0);
            (cx, cy, rad, [byte(d, 40), byte(d, 48), byte(d, 56)])
        })
        .collect();
    let mut img = RgbImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let t = u32::try_from((u64::from(x) + u64::from(y)) * 255 / u64::from(w + h).max(1))
            .unwrap_or(0);
        let mut c = [0u8; 3];
        for i in 0..3 {
            let v = (u32::from(a[i]) * (255 - t) + u32::from(b[i]) * t) / 255;
            c[i] = u8::try_from(v).unwrap_or(u8::MAX);
        }
        for (cx, cy, rad, color) in &discs {
            let (dx, dy) = (i64::from(x) - cx, i64::from(y) - cy);
            if dx * dx + dy * dy <= rad * rad {
                c = *color;
            }
        }
        // A little texture so JPEG output is not trivially compressible.
        let n = byte(mix(seed ^ (u64::from(x) << 32 | u64::from(y))), 0) / 16;
        *px = Rgb([
            c[0].saturating_add(n),
            c[1].saturating_add(n),
            c[2].saturating_add(n),
        ]);
    }
    match format {
        SampleFormat::Jpeg => Ok(encode_jpeg(&img, 85)?.bytes),
        SampleFormat::Png => write(&DynamicImage::ImageRgb8(img), ImageFormat::Png),
        SampleFormat::Gif => write(&DynamicImage::ImageRgb8(img), ImageFormat::Gif),
        SampleFormat::PngAlpha => {
            let border = (w.min(h) / 10).max(1);
            let rgba = RgbaImage::from_fn(w, h, |x, y| {
                let p = img.get_pixel(x, y);
                let inside = x >= border && y >= border && x + border < w && y + border < h;
                Rgba([p[0], p[1], p[2], if inside { 255 } else { 0 }])
            });
            write(&DynamicImage::ImageRgba8(rgba), ImageFormat::Png)
        }
    }
}

fn write(img: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>, MediaError> {
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), format)
        .map_err(MediaError::from_image)?;
    Ok(out)
}

/// A synthetic file that carries a made-up location ([`sample_located`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SampleLocated {
    /// A decodable 64 x 48 JPEG with an EXIF GPS IFD (a phone photo).
    Jpeg,
    /// A HEIC whose `Exif` item holds a GPS IFD (Apple's offset of 6); no decodable picture.
    Heic,
    /// An MP4 with `moov/udta/©xyz` (Android's `MediaMuxer`).
    Mp4,
    /// A QuickTime MOV with `moov/meta` keys naming the ISO 6709 key (an iPhone video).
    Mov,
    /// An M4A recording with the iTunes `moov/udta/meta/ilst/©xyz`.
    M4a,
}

impl SampleLocated {
    /// Every kind.
    pub const ALL: [Self; 5] = [Self::Jpeg, Self::Heic, Self::Mp4, Self::Mov, Self::M4a];

    /// The MIME type [`crate::sniff_mime`] gives the file.
    pub fn mime(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Heic => "image/heic",
            Self::Mp4 => "video/mp4",
            Self::Mov => "video/quicktime",
            Self::M4a => "audio/mp4",
        }
    }
}

/// The made-up place: 30°12'34.56" N, 40°7'8.9" W, open ocean.
const LAT: [(u32, u32); 3] = [(30, 1), (12, 1), (3456, 100)];
const LON: [(u32, u32); 3] = [(40, 1), (7, 1), (890, 100)];
/// The same place in ISO 6709.
pub(crate) const PLACE_ISO6709: &[u8] = b"+30.2096-040.1191/";

/// A synthetic file of `kind` that carries a made-up location: [`crate::look`] finds it. The
/// same kind always gives the same bytes.
pub fn sample_located(kind: SampleLocated) -> Vec<u8> {
    match kind {
        SampleLocated::Jpeg => {
            let jpeg = sample_image(64, 48, 0x10c, SampleFormat::Jpeg).unwrap_or_default();
            let mut app1 = b"Exif\0\0".to_vec();
            app1.extend(gps_tiff());
            let mut out = jpeg.get(..2).unwrap_or(&[0xff, 0xd8]).to_vec();
            out.extend([0xff, 0xe1]);
            out.extend(
                u16::try_from(app1.len() + 2)
                    .unwrap_or(u16::MAX)
                    .to_be_bytes(),
            );
            out.extend(app1);
            out.extend(jpeg.get(2..).unwrap_or_default());
            out
        }
        SampleLocated::Heic => {
            let mut exif = 6u32.to_be_bytes().to_vec();
            exif.extend(b"Exif\0\0");
            exif.extend(gps_tiff());
            let ftyp = bmff_box(b"ftyp", b"heic\0\0\0\0mif1heic");
            let meta_with = |data: u32| {
                let mut infe = 1u16.to_be_bytes().to_vec();
                infe.extend([0, 0]);
                infe.extend(b"Exif\0");
                let mut iinf = 1u16.to_be_bytes().to_vec();
                iinf.extend(full_box(b"infe", 2, &infe));
                // iloc v0: offset and length sizes 4, base offset size 0; one item, one extent.
                let mut iloc = vec![0x44, 0x00];
                iloc.extend(1u16.to_be_bytes());
                iloc.extend(1u16.to_be_bytes());
                iloc.extend(0u16.to_be_bytes());
                iloc.extend(1u16.to_be_bytes());
                iloc.extend(data.to_be_bytes());
                iloc.extend(u32::try_from(exif.len()).unwrap_or(0).to_be_bytes());
                let mut hdlr = vec![0; 4];
                hdlr.extend(b"pict");
                hdlr.extend([0; 13]);
                full_box(
                    b"meta",
                    0,
                    &[
                        full_box(b"hdlr", 0, &hdlr),
                        full_box(b"iinf", 0, &iinf),
                        full_box(b"iloc", 0, &iloc),
                    ]
                    .concat(),
                )
            };
            let data = ftyp.len() + meta_with(0).len() + 8;
            let mut out = ftyp;
            out.extend(meta_with(u32::try_from(data).unwrap_or(0)));
            out.extend(bmff_box(b"mdat", &exif));
            out
        }
        SampleLocated::Mp4 => {
            let mut xyz = u16::try_from(PLACE_ISO6709.len())
                .unwrap_or(0)
                .to_be_bytes()
                .to_vec();
            xyz.extend(0x15c7u16.to_be_bytes());
            xyz.extend(PLACE_ISO6709);
            movie(
                b"isom\0\0\x02\0isomiso2mp41",
                &bmff_box(b"udta", &bmff_box(&XYZ, &xyz)),
            )
        }
        SampleLocated::Mov => {
            let mut hdlr = vec![0; 4];
            hdlr.extend(b"mdta");
            hdlr.extend([0; 13]);
            let key = b"com.apple.quicktime.location.ISO6709";
            let mut keys = 1u32.to_be_bytes().to_vec();
            keys.extend(u32::try_from(key.len() + 8).unwrap_or(0).to_be_bytes());
            keys.extend(b"mdta");
            keys.extend(key);
            let item = bmff_box(&1u32.to_be_bytes(), &data_box(PLACE_ISO6709));
            // QuickTime's `meta`: no version and flags.
            let meta = bmff_box(
                b"meta",
                &[
                    full_box(b"hdlr", 0, &hdlr),
                    full_box(b"keys", 0, &keys),
                    bmff_box(b"ilst", &item),
                ]
                .concat(),
            );
            movie(b"qt  \0\0\x02\0qt  ", &meta)
        }
        SampleLocated::M4a => {
            let mut hdlr = vec![0; 4];
            hdlr.extend(b"mdirappl");
            hdlr.extend([0; 9]);
            let ilst = bmff_box(b"ilst", &bmff_box(&XYZ, &data_box(PLACE_ISO6709)));
            let meta = full_box(b"meta", 0, &[full_box(b"hdlr", 0, &hdlr), ilst].concat());
            movie(b"M4A \0\0\x02\0M4A mp42isom", &bmff_box(b"udta", &meta))
        }
    }
}

/// `©xyz`.
const XYZ: [u8; 4] = [0xa9, b'x', b'y', b'z'];

/// A movie: `ftyp` with `brands`, a `moov` holding a `mvhd` and `inside`, then a little `mdat`.
fn movie(brands: &[u8], inside: &[u8]) -> Vec<u8> {
    let mut mvhd = vec![0; 96];
    mvhd[8..12].copy_from_slice(&1000u32.to_be_bytes());
    let mut out = bmff_box(b"ftyp", brands);
    out.extend(bmff_box(
        b"moov",
        &[full_box(b"mvhd", 0, &mvhd), inside.to_vec()].concat(),
    ));
    out.extend(bmff_box(b"mdat", &[0x5a; 256]));
    out
}

/// An iTunes / QuickTime `data` box holding UTF-8 text.
pub(crate) fn data_box(text: &[u8]) -> Vec<u8> {
    let mut d = 1u32.to_be_bytes().to_vec();
    d.extend([0; 4]);
    d.extend(text);
    bmff_box(b"data", &d)
}

/// An ISO-BMFF box with a 32-bit size.
pub(crate) fn bmff_box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = u32::try_from(payload.len() + 8)
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    b.extend(typ);
    b.extend(payload);
    b
}

/// An ISO-BMFF FullBox (version, zero flags).
pub(crate) fn full_box(typ: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![version, 0, 0, 0];
    p.extend(payload);
    bmff_box(typ, &p)
}

/// A little-endian TIFF: IFD0 with only the GPS IFD pointer, the GPS IFD with N/latitude and
/// W/longitude.
fn gps_tiff() -> Vec<u8> {
    let entry = |tag: u16, typ: u16, count: u32, value: [u8; 4]| {
        let mut e = tag.to_le_bytes().to_vec();
        e.extend(typ.to_le_bytes());
        e.extend(count.to_le_bytes());
        e.extend(value);
        e
    };
    // Header 8, IFD0 at 8 (2 + 12 + 4 = 18), GPS IFD at 26 (2 + 4 * 12 + 4 = 54), values at 80.
    let mut t = b"II*\0".to_vec();
    t.extend(8u32.to_le_bytes());
    t.extend(1u16.to_le_bytes());
    t.extend(entry(0x8825, 4, 1, 26u32.to_le_bytes()));
    t.extend(0u32.to_le_bytes());
    t.extend(4u16.to_le_bytes());
    t.extend(entry(1, 2, 2, *b"N\0\0\0"));
    t.extend(entry(2, 5, 3, 80u32.to_le_bytes()));
    t.extend(entry(3, 2, 2, *b"W\0\0\0"));
    t.extend(entry(4, 5, 3, 104u32.to_le_bytes()));
    t.extend(0u32.to_le_bytes());
    for (n, d) in LAT.iter().chain(LON.iter()) {
        t.extend(n.to_le_bytes());
        t.extend(d.to_le_bytes());
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe;

    #[test]
    fn samples_are_deterministic_and_decodable() {
        for f in [
            SampleFormat::Jpeg,
            SampleFormat::Png,
            SampleFormat::PngAlpha,
            SampleFormat::Gif,
        ] {
            let a = sample_image(64, 48, 7, f).unwrap();
            assert_eq!(a, sample_image(64, 48, 7, f).unwrap());
            assert_ne!(a, sample_image(64, 48, 8, f).unwrap());
            let info = probe(&a).unwrap();
            assert_eq!((info.width, info.height, info.mime), (64, 48, f.mime()));
        }
    }

    #[test]
    fn located_samples_are_found_sniffed_and_deterministic() {
        use crate::{Evidence, EvidenceKind as K, Look, LookFormat as F, look, sniff_mime};
        for kind in SampleLocated::ALL {
            let f = sample_located(kind);
            assert_eq!(f, sample_located(kind));
            assert_eq!(sniff_mime(&f), Some(kind.mime()), "{kind:?}");
            let (format, proof) = match kind {
                SampleLocated::Jpeg => (F::Jpeg, K::ExifGps),
                SampleLocated::Heic => (F::Heif, K::ExifGps),
                SampleLocated::Mp4 | SampleLocated::Mov | SampleLocated::M4a => {
                    (F::Bmff, K::MovieAtom)
                }
            };
            assert_eq!(
                look(&mut Cursor::new(&f), f.len() as u64),
                Look::Found(Evidence {
                    format,
                    kind: proof
                }),
                "{kind:?}"
            );
        }
        let info = probe(&sample_located(SampleLocated::Jpeg)).unwrap();
        assert_eq!((info.width, info.height), (64, 48));
    }
}
