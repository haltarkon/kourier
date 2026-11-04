//! Tests of PNG, WebP and the XMP places of every format. Every file is written here; the place
//! is made up, in the middle of the Atlantic.

use std::io::Write;

use super::*;

pub(super) const GPS_XMP: &[u8] = b"<x:xmpmeta xmlns:x='adobe:ns:meta/'><rdf:RDF><rdf:Description \
    exif:GPSLatitude=\"30,12.576N\" exif:GPSLongitude=\"40,7.148W\"/></rdf:RDF></x:xmpmeta>";
pub(super) const ZERO_XMP: &[u8] = b"<x:xmpmeta><rdf:Description exif:GPSLatitude=\"0,0.0N\" \
    exif:GPSLongitude=\"0,0.0E\"/></x:xmpmeta>";
pub(super) const DJI_XMP: &[u8] = b"<x:xmpmeta><rdf:Description drone-dji:GpsLatitude=\"30.2096\" \
    drone-dji:GpsLongtitude=\"-40.1191\"/></x:xmpmeta>";
const PLAIN_XMP: &[u8] = b"<x:xmpmeta><rdf:Description xmp:CreatorTool=\"Acme 2\"/></x:xmpmeta>";

/// A big-endian TIFF with the made-up location (`gps_tiff` writes the little-endian magic).
fn be_tiff() -> Vec<u8> {
    gps_tiff_with(Bo::Be, b"MM\0*", |w| gps_entries(w, Some(&LAT), Some(&LON)))
}

const fn found(format: LookFormat, kind: EvidenceKind) -> Look {
    Look::Found(Evidence { format, kind })
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

// PNG.

fn chunk(typ: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = (data.len() as u32).to_be_bytes().to_vec();
    c.extend_from_slice(typ);
    c.extend_from_slice(data);
    // The CRC is not checked by the look.
    c.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    c
}

/// A PNG: the signature, IHDR, the given chunks, an IDAT, IEND.
fn png_with(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut f = crate::chunks::PNG_SIGNATURE.to_vec();
    f.extend(chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 2, 0, 0, 0]));
    for c in chunks {
        f.extend_from_slice(c);
    }
    f.extend(chunk(b"IDAT", &[0x78; 30]));
    f.extend(chunk(b"IEND", b""));
    f
}

fn itxt(compressed: bool, text: &[u8]) -> Vec<u8> {
    let mut d = b"XML:com.adobe.xmp\0".to_vec();
    d.extend([u8::from(compressed), 0]);
    d.extend(b"\0\0");
    if compressed {
        d.extend(zlib(text));
    } else {
        d.extend(text);
    }
    chunk(b"iTXt", &d)
}

/// ImageMagick's raw profile text of `bytes`.
fn raw_profile(name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut t = format!("\n{name}\n{:8}\n", bytes.len()).into_bytes();
    for line in bytes.chunks(36) {
        for b in line {
            t.extend(format!("{b:02x}").into_bytes());
        }
        t.push(b'\n');
    }
    t
}

fn ztxt(keyword: &[u8], text: &[u8]) -> Vec<u8> {
    let mut d = keyword.to_vec();
    d.extend([0, 0]);
    d.extend(zlib(text));
    chunk(b"zTXt", &d)
}

fn text(keyword: &[u8], text: &[u8]) -> Vec<u8> {
    let mut d = keyword.to_vec();
    d.push(0);
    d.extend(text);
    chunk(b"tEXt", &d)
}

const PNG_EXIF: Look = found(LookFormat::Png, EvidenceKind::ExifGps);
const PNG_XMP: Look = found(LookFormat::Png, EvidenceKind::Xmp);

#[test]
fn a_png_exif_chunk_with_gps_is_found() {
    let t = be_tiff();
    assert_eq!(look_bytes(&png_with(&[chunk(b"eXIf", &t)])), PNG_EXIF);
    assert_eq!(
        look_bytes(&png_with(&[chunk(b"eXIf", &exif(&t))])),
        PNG_EXIF
    );
    let z = gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO));
    assert_eq!(look_bytes(&png_with(&[chunk(b"eXIf", &z)])), Look::None);
}

#[test]
fn png_xmp_plain_and_compressed_is_found() {
    assert_eq!(look_bytes(&png_with(&[itxt(false, GPS_XMP)])), PNG_XMP);
    assert_eq!(look_bytes(&png_with(&[itxt(true, GPS_XMP)])), PNG_XMP);
    assert_eq!(look_bytes(&png_with(&[itxt(true, DJI_XMP)])), PNG_XMP);
    for none in [ZERO_XMP, PLAIN_XMP] {
        assert_eq!(look_bytes(&png_with(&[itxt(true, none)])), Look::None);
        assert_eq!(look_bytes(&png_with(&[itxt(false, none)])), Look::None);
    }
    // A text chunk of another keyword is nothing, whatever it says.
    assert_eq!(
        look_bytes(&png_with(&[text(b"Comment", GPS_XMP)])),
        Look::None
    );
}

#[test]
fn imagemagick_raw_exif_in_hex_is_found() {
    let e = exif(&gps_tiff(Bo::Le, Some(&LAT), Some(&LON)));
    let profile = raw_profile("exif", &e);
    let kw = b"Raw profile type exif";
    assert_eq!(look_bytes(&png_with(&[ztxt(kw, &profile)])), PNG_EXIF);
    assert_eq!(look_bytes(&png_with(&[text(kw, &profile)])), PNG_EXIF);
    let x = raw_profile("xmp", GPS_XMP);
    assert_eq!(
        look_bytes(&png_with(&[ztxt(b"Raw profile type xmp", &x)])),
        PNG_XMP
    );
    // Zeros; a hex profile cut short or with a bad digit is broken.
    let z = raw_profile("exif", &exif(&gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO))));
    assert_eq!(look_bytes(&png_with(&[ztxt(kw, &z)])), Look::None);
    let cut = &profile[..profile.len() - 20];
    assert_eq!(look_bytes(&png_with(&[ztxt(kw, cut)])), Look::Unreadable);
    let mut bad = profile.clone();
    let at = bad.len() - 4;
    bad[at] = b'g';
    assert_eq!(look_bytes(&png_with(&[ztxt(kw, &bad)])), Look::Unreadable);
}

#[test]
fn an_inflate_past_one_mebibyte_is_unreadable() {
    // 1.5 MiB of spaces before a located packet: a zip bomb by its shape.
    let mut bomb = vec![b' '; 3 << 19];
    bomb.extend(GPS_XMP);
    let f = png_with(&[itxt(true, &bomb)]);
    assert!(f.len() < 10_000, "{}", f.len());
    assert_eq!(look_bytes(&f), Look::Unreadable);
    // The same just under the bound is read.
    let mut under = vec![b' '; (1 << 20) - GPS_XMP.len()];
    under.extend(GPS_XMP);
    assert_eq!(look_bytes(&png_with(&[itxt(true, &under)])), PNG_XMP);
    // A bomb in one chunk does not hide a location in another.
    let t = be_tiff();
    let f = png_with(&[itxt(true, &bomb), chunk(b"eXIf", &t)]);
    assert_eq!(look_bytes(&f), PNG_EXIF);
}

#[test]
fn png_pixels_are_seeked_over_and_broken_chunks_are_unreadable() {
    // 3 MiB of IDAT before the XMP: more than the file's read budget, never read.
    let mut f = crate::chunks::PNG_SIGNATURE.to_vec();
    f.extend(chunk(b"IHDR", &[0; 13]));
    f.extend(chunk(b"IDAT", &vec![0x78; 3 << 20]));
    f.extend(itxt(false, GPS_XMP));
    f.extend(chunk(b"IEND", b""));
    assert_eq!(look_bytes(&f), PNG_XMP);
    // After IEND nothing is looked at.
    let mut f = png_with(&[]);
    f.extend(itxt(false, GPS_XMP));
    assert_eq!(look_bytes(&f), Look::None);
    // A chunk past the file's end; a corrupt compressed stream; an unknown compression.
    let mut f = png_with(&[]);
    f.truncate(f.len() - 20);
    assert_eq!(look_bytes(&f), Look::Unreadable);
    let mut d = b"XML:com.adobe.xmp\0\x01\0\0\0".to_vec();
    d.extend([0x78, 0x9c, 0xff, 0xff, 0xff]);
    assert_eq!(
        look_bytes(&png_with(&[chunk(b"iTXt", &d)])),
        Look::Unreadable
    );
    let mut d = b"XML:com.adobe.xmp\0\x01\x07\0\0".to_vec();
    d.extend(GPS_XMP);
    assert_eq!(
        look_bytes(&png_with(&[chunk(b"iTXt", &d)])),
        Look::Unreadable
    );
    // A PNG without metadata.
    assert_eq!(look_bytes(&png_with(&[])), Look::None);
}

// WebP.

fn riff_chunk(typ: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut c = typ.to_vec();
    c.extend((data.len() as u32).to_le_bytes());
    c.extend_from_slice(data);
    if data.len() % 2 == 1 {
        c.push(0);
    }
    c
}

fn webp_with(chunks: &[Vec<u8>]) -> Vec<u8> {
    let mut body = b"WEBP".to_vec();
    body.extend(riff_chunk(b"VP8X", &[0x0c, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    // An odd-sized picture chunk: its pad byte must be stepped over.
    body.extend(riff_chunk(b"VP8L", &[0x2f; 51]));
    for c in chunks {
        body.extend_from_slice(c);
    }
    let mut f = b"RIFF".to_vec();
    f.extend((body.len() as u32).to_le_bytes());
    f.extend(body);
    f
}

#[test]
fn webp_exif_and_xmp_are_found() {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let ex = found(LookFormat::WebP, EvidenceKind::ExifGps);
    assert_eq!(look_bytes(&webp_with(&[riff_chunk(b"EXIF", &t)])), ex);
    assert_eq!(
        look_bytes(&webp_with(&[riff_chunk(b"EXIF", &exif(&t))])),
        ex
    );
    let x = found(LookFormat::WebP, EvidenceKind::Xmp);
    assert_eq!(look_bytes(&webp_with(&[riff_chunk(b"XMP ", GPS_XMP)])), x);
    assert_eq!(look_bytes(&webp_with(&[riff_chunk(b"XMP ", DJI_XMP)])), x);
    let z = gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO));
    for none in [
        webp_with(&[]),
        webp_with(&[riff_chunk(b"XMP ", ZERO_XMP)]),
        webp_with(&[riff_chunk(b"EXIF", &z)]),
    ] {
        assert_eq!(look_bytes(&none), Look::None);
    }
    // A chunk past the RIFF's end.
    let mut f = webp_with(&[riff_chunk(b"XMP ", GPS_XMP)]);
    f.truncate(f.len() - 10);
    assert_eq!(look_bytes(&f), Look::Unreadable);
}

// JPEG XMP and Extended XMP.

fn app1_xmp(packet: &[u8]) -> Vec<u8> {
    let mut p = b"http://ns.adobe.com/xap/1.0/\0".to_vec();
    p.extend(packet);
    segment(0xe1, &p)
}

fn app1_extended(guid: u8, full: u32, offset: u32, data: &[u8]) -> Vec<u8> {
    let mut p = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
    p.extend([b'A' + guid; 32]);
    p.extend(full.to_be_bytes());
    p.extend(offset.to_be_bytes());
    p.extend(data);
    segment(0xe1, &p)
}

const JPEG_XMP: Look = found(LookFormat::Jpeg, EvidenceKind::Xmp);

#[test]
fn jpeg_xmp_only_and_dji_spelling_are_found() {
    assert_eq!(look_bytes(&jpeg_with(&[app1_xmp(GPS_XMP)])), JPEG_XMP);
    assert_eq!(look_bytes(&jpeg_with(&[app1_xmp(DJI_XMP)])), JPEG_XMP);
    for none in [ZERO_XMP, PLAIN_XMP] {
        assert_eq!(look_bytes(&jpeg_with(&[app1_xmp(none)])), Look::None);
    }
    // EXIF GPS wins over XMP; a zero EXIF and a located XMP is XMP.
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let both = jpeg_with(&[app1_xmp(GPS_XMP), segment(0xe1, &exif(&t))]);
    assert_eq!(look_bytes(&both), JPEG_GPS);
    let z = gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO));
    let f = jpeg_with(&[segment(0xe1, &exif(&z)), app1_xmp(GPS_XMP)]);
    assert_eq!(look_bytes(&f), JPEG_XMP);
    // A UTF-16 packet.
    let wide: Vec<u8> = std::str::from_utf8(GPS_XMP)
        .unwrap()
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    assert_eq!(look_bytes(&jpeg_with(&[app1_xmp(&wide)])), JPEG_XMP);
}

/// LOC P2: a JPEG whose `segments` main-XMP APP1 segments (30 are about 1.9 MiB of the 2 MiB the
/// walk lets through) repeat `unit`, a name with no value end after it. The unbounded value search
/// went to the end of the text for every occurrence (quadratic: 140 s in a debug build); the
/// bounded one is linear. Returns the file and how often `unit` stands in it.
fn hostile_xmp_jpeg(unit: &[u8], segments: usize) -> (Vec<u8>, usize) {
    const SEGMENT: usize = 64_000;
    let mut chunk = Vec::new();
    while chunk.len() + unit.len() <= SEGMENT {
        chunk.extend_from_slice(unit);
    }
    let units = segments * (chunk.len() / unit.len());
    let segments: Vec<Vec<u8>> = (0..segments).map(|_| app1_xmp(&chunk)).collect();
    (jpeg_with(&segments), units)
}

/// Linear by COUNT, not by clock (strike 2, 2026-10-29: "took 103 ms" against a 100 ms bound at a
/// server load of 20): the value search looks at no more than `VALUE_MAX + 1` bytes per occurrence
/// of a name (`xmp::VALUE_BYTES`), whatever the text repeats. Two segments first, so a quadratic
/// search fails in about a second instead of minutes; then the 2 MiB the walk lets through.
/// Mutant: the value search without its `take` (to the end of the text for every occurrence).
#[test]
fn two_mebibytes_of_one_name_are_looked_at_in_linear_time() {
    for unit in [
        &b"GPSLatitude>"[..],
        &b"GPSLatitude=\""[..],
        &b"GPSLongitude='"[..],
    ] {
        for segments in [2, 30] {
            let (jpeg, units) = hostile_xmp_jpeg(unit, segments);
            if segments == 30 {
                assert!(
                    jpeg.len() > 1_800_000 && jpeg.len() < 2 << 20,
                    "{}",
                    jpeg.len()
                );
            }
            crate::xmp::VALUE_BYTES.with(|n| n.set(0));
            // No value, no location (and the whole text was read: not Unreadable).
            assert_eq!(look_bytes(&jpeg), Look::None);
            let looked = crate::xmp::VALUE_BYTES.with(std::cell::Cell::get);
            // Each occurrence is searched for its value once per pass over the text (the look may
            // pass over a packet more than once: the factor 4 leaves room for that, and is still
            // millions of times below the quadratic search's count).
            let bound = 4 * (crate::xmp::VALUE_MAX + 1) * units;
            assert!(
                looked <= bound,
                "{:?} x {units} in {segments} segments: the value search looked at {looked} bytes, \
                 more than {bound}",
                String::from_utf8_lossy(unit)
            );
        }
    }
}

#[test]
fn a_value_over_the_bound_is_no_value_and_one_under_it_is_found() {
    let long = "1".repeat(80);
    let over = format!(
        "<x:xmpmeta><d exif:GPSLatitude=\"{long}\" exif:GPSLongitude=\"{long}\"/></x:xmpmeta>"
    );
    assert_eq!(
        look_bytes(&jpeg_with(&[app1_xmp(over.as_bytes())])),
        Look::None
    );
    // Elements, with a text up to the bound, and a packet cut inside the value.
    let elem = "<x:xmpmeta><exif:GPSLatitude>30,12.576N</exif:GPSLatitude>\
        <exif:GPSLongitude>40,7.148W</exif:GPSLongitude></x:xmpmeta>";
    assert_eq!(
        look_bytes(&jpeg_with(&[app1_xmp(elem.as_bytes())])),
        JPEG_XMP
    );
    let cut = "<x:xmpmeta><exif:GPSLatitude>30,12.576N</exif:GPSLatitude>\
        <exif:GPSLongitude>40,7.148W";
    assert_eq!(
        look_bytes(&jpeg_with(&[app1_xmp(cut.as_bytes())])),
        JPEG_XMP
    );
}

/// LOC P4: an unreadable answer says why (time or bound) and in which format, and nothing else.
#[test]
fn an_unreadable_look_says_its_reason_and_its_format() {
    use std::time::Duration;
    let jpeg = jpeg_with(&[app1_xmp(PLAIN_XMP)]);
    // `secs`: the clock's reading from its second look on (the first read of the head passes).
    let why = |bytes: &[u8], secs: u64| {
        let mut calls = 0;
        let mut clock = move || {
            calls += 1;
            Duration::from_secs(if calls > 1 { secs } else { 0 })
        };
        let (look, why) = look_clocked_why(&mut Cursor::new(bytes), bytes.len() as u64, &mut clock);
        (look, why.map(|w| (w.reason(), w.format_name())))
    };
    // Answered: no reason.
    assert_eq!(why(&jpeg, 0), (Look::None, None));
    // The clock past the bound: time, and the format the first bytes named.
    assert_eq!(why(&jpeg, 3), (Look::Unreadable, Some(("time", "jpeg"))));
    // Cut short in a segment: a bound, in the same format.
    assert_eq!(
        why(&jpeg[..40], 0),
        (Look::Unreadable, Some(("bound", "jpeg")))
    );
    // Not even the first bytes: unknown.
    assert_eq!(
        why(&[], 0),
        (Look::None, None),
        "an empty file is not a located one"
    );
    let png = png_with(&[chunk(b"tEXt", b"x\0y")]);
    assert_eq!(
        why(&png[..20], 0),
        (Look::Unreadable, Some(("bound", "png")))
    );
}

#[test]
fn a_name_split_across_extended_xmp_segments_is_found() {
    let ext = GPS_XMP;
    let cut = ext.windows(8).position(|w| w == b"Latitude").unwrap() + 3;
    let (a, b) = ext.split_at(cut);
    let full = ext.len() as u32;
    // The segments out of order, another GUID's unrelated segment between them.
    let f = jpeg_with(&[
        app1_xmp(PLAIN_XMP),
        app1_extended(0, full, cut as u32, b),
        app1_extended(1, 9, 0, b"<x:x/>123"),
        app1_extended(0, full, 0, a),
    ]);
    assert_eq!(look_bytes(&f), JPEG_XMP);
    // Each half alone holds no whole name.
    for half in [(0, a), (cut as u32, b)] {
        let f = jpeg_with(&[app1_xmp(PLAIN_XMP), app1_extended(0, full, half.0, half.1)]);
        assert_eq!(look_bytes(&f), Look::None);
    }
    // An Extended XMP segment too short for its header is broken.
    let mut p = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
    p.extend([b'A'; 20]);
    assert_eq!(
        look_bytes(&jpeg_with(&[segment(0xe1, &p)])),
        Look::Unreadable
    );
}

// JPEG XL `xml ` and HEIF XMP items.

#[test]
fn jxl_xml_boxes_are_searched() {
    let f = jxl_with(&[bx(b"jxlc", &[0x44; 100]), bx(b"xml ", GPS_XMP)]);
    assert_eq!(look_bytes(&f), found(LookFormat::Jxl, EvidenceKind::Xmp));
    let f = jxl_with(&[bx(b"xml ", ZERO_XMP)]);
    assert_eq!(look_bytes(&f), Look::None);
}

#[test]
fn a_heic_xmp_item_with_zeros_is_none() {
    let n = ZERO_XMP.len() as u64;
    let f = heif_with(
        |data| {
            vec![
                iinf(0, &[infe(2, 1, b"mime", b"application/rdf+xml\0")]),
                iloc(0, [4, 4, 0, 0], &[loc(1, 0, &[(data, n)])]),
            ]
        },
        ZERO_XMP,
    );
    assert_eq!(look_bytes(&f), Look::None);
}

// Fuzz samples for the truncation and bit-flip tests of the parent module.

pub(super) fn png_fuzz_sample() -> Vec<u8> {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let mut padded = vec![b' '; 600];
    padded.extend(GPS_XMP);
    let mut f = crate::chunks::PNG_SIGNATURE.to_vec();
    f.extend(chunk(b"IHDR", &[0; 13]));
    f.extend(chunk(b"IDAT", &[0x78; 1200]));
    f.extend(itxt(true, &padded));
    f.extend(ztxt(
        b"Raw profile type exif",
        &raw_profile("exif", &exif(&t)),
    ));
    f.extend(chunk(b"eXIf", &t));
    f.extend(chunk(b"IEND", b""));
    f
}

pub(super) fn webp_fuzz_sample() -> Vec<u8> {
    let t = be_tiff();
    let mut f = webp_with(&[riff_chunk(b"ICCP", &[0x31; 1300])]);
    let more = [riff_chunk(b"XMP ", GPS_XMP), riff_chunk(b"EXIF", &exif(&t))].concat();
    f.extend(&more);
    let size = (f.len() - 8) as u32;
    f[4..8].copy_from_slice(&size.to_le_bytes());
    f
}

pub(super) fn jpeg_xmp_fuzz_sample() -> Vec<u8> {
    let (a, b) = GPS_XMP.split_at(60);
    let full = GPS_XMP.len() as u32;
    jpeg_with(&[
        segment(0xfe, &[0x20; 1300]),
        app1_xmp(PLAIN_XMP),
        app1_extended(0, full, 60, b),
        app1_extended(0, full, 0, a),
    ])
}

/// LOC PR2: 32 main-XMP parts of 7 bytes (`a\0a\0a\0a`: too short to be taken for UTF-16 one by
/// one, a UTF-16 head all together) before a located ASCII packet. The joined text is searched
/// as it is: a second narrow turned the packet behind them into `0x80` bytes.
#[test]
fn tiny_utf16_looking_parts_before_a_located_packet_do_not_hide_it() {
    let tiny = || app1_xmp(b"a\0a\0a\0a");
    let mut segments: Vec<Vec<u8>> = (0..32).map(|_| tiny()).collect();
    segments.push(app1_xmp(GPS_XMP));
    assert_eq!(look_bytes(&jpeg_with(&segments)), JPEG_XMP);
    // The same parts alone name nothing.
    let alone: Vec<Vec<u8>> = (0..32).map(|_| tiny()).collect();
    assert_eq!(look_bytes(&jpeg_with(&alone)), Look::None);
}
