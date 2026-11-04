//! Tests of the look. Every file is written here by small writers (no binary fixtures); the
//! coordinates are made up, in the middle of the Atlantic.

use std::io::{Cursor, Read, Seek, SeekFrom};

use super::*;

#[derive(Clone, Copy)]
enum Bo {
    Le,
    Be,
}

/// One IFD entry to write; `data` is already in the writer's byte order.
struct E {
    tag: u16,
    typ: u16,
    count: u32,
    data: Vec<u8>,
}

/// A TIFF writer: a header, then IFDs placed at chosen offsets.
struct W {
    bo: Bo,
    buf: Vec<u8>,
}

impl W {
    fn new(bo: Bo, magic: &[u8; 4], ifd0: u32) -> Self {
        let mut w = Self {
            bo,
            buf: magic.to_vec(),
        };
        let o = w.u32(ifd0);
        w.buf.extend_from_slice(&o);
        w
    }

    fn u16(&self, v: u16) -> [u8; 2] {
        match self.bo {
            Bo::Le => v.to_le_bytes(),
            Bo::Be => v.to_be_bytes(),
        }
    }

    fn u32(&self, v: u32) -> [u8; 4] {
        match self.bo {
            Bo::Le => v.to_le_bytes(),
            Bo::Be => v.to_be_bytes(),
        }
    }

    fn put(&mut self, at: usize, bytes: &[u8]) {
        if self.buf.len() < at + bytes.len() {
            self.buf.resize(at + bytes.len(), 0);
        }
        self.buf[at..at + bytes.len()].copy_from_slice(bytes);
    }

    fn long(&self, tag: u16, v: u32) -> E {
        E {
            tag,
            typ: 4,
            count: 1,
            data: self.u32(v).to_vec(),
        }
    }

    fn short(&self, tag: u16, v: u16) -> E {
        E {
            tag,
            typ: 3,
            count: 1,
            data: self.u16(v).to_vec(),
        }
    }

    fn ascii(&self, tag: u16, s: &[u8]) -> E {
        E {
            tag,
            typ: 2,
            count: s.len() as u32,
            data: s.to_vec(),
        }
    }

    fn rational(&self, tag: u16, v: &[(u32, u32)]) -> E {
        let mut data = Vec::new();
        for &(n, d) in v {
            data.extend_from_slice(&self.u32(n));
            data.extend_from_slice(&self.u32(d));
        }
        E {
            tag,
            typ: 5,
            count: v.len() as u32,
            data,
        }
    }

    /// An entry whose value field is `field` as it stands (no data written for it).
    fn raw(&self, tag: u16, typ: u16, count: u32, field: u32) -> E {
        E {
            tag,
            typ,
            count,
            data: self.u32(field).to_vec(),
        }
    }

    /// Writes an IFD at `at`; values over 4 bytes go right after it.
    fn ifd(&mut self, at: u32, entries: &[E], next: u32) {
        let at = at as usize;
        let mut data_at = at + 2 + 12 * entries.len() + 4;
        let n = self.u16(entries.len() as u16);
        self.put(at, &n);
        for (i, e) in entries.iter().enumerate() {
            let mut rec = Vec::new();
            rec.extend_from_slice(&self.u16(e.tag));
            rec.extend_from_slice(&self.u16(e.typ));
            rec.extend_from_slice(&self.u32(e.count));
            if e.data.len() <= 4 {
                let mut v = e.data.clone();
                v.resize(4, 0);
                rec.extend_from_slice(&v);
            } else {
                rec.extend_from_slice(&self.u32(data_at as u32));
                self.put(data_at, &e.data);
                data_at += e.data.len();
            }
            self.put(at + 2 + 12 * i, &rec);
        }
        let next = self.u32(next);
        self.put(at + 2 + 12 * entries.len(), &next);
    }
}

/// About 30° N, 40° W: open sea.
const LAT: [(u32, u32); 3] = [(30, 1), (12, 1), (3456, 100)];
const LON: [(u32, u32); 3] = [(40, 1), (7, 1), (890, 100)];
const ZERO: [(u32, u32); 3] = [(0, 1), (0, 1), (0, 1)];

/// The usual GPS IFD entries: refs, latitude, longitude.
fn gps_entries(w: &W, lat: Option<&[(u32, u32)]>, lon: Option<&[(u32, u32)]>) -> Vec<E> {
    let mut e = vec![w.ascii(0x0001, b"N\0")];
    if let Some(lat) = lat {
        e.push(w.rational(0x0002, lat));
    }
    e.push(w.ascii(0x0003, b"W\0"));
    if let Some(lon) = lon {
        e.push(w.rational(0x0004, lon));
    }
    e
}

/// A TIFF with IFD0 at 8 (pointing at the GPS IFD at 200) and the given GPS entries.
fn gps_tiff_with(bo: Bo, magic: &[u8; 4], gps: impl FnOnce(&W) -> Vec<E>) -> Vec<u8> {
    let mut w = W::new(bo, magic, 8);
    let ifd0 = vec![
        w.long(0x0100, 64),
        w.long(0x0101, 48),
        w.short(0x0112, 1),
        w.long(0x8825, 200),
    ];
    w.ifd(8, &ifd0, 0);
    let g = gps(&w);
    w.ifd(200, &g, 0);
    w.buf
}

fn gps_tiff(bo: Bo, lat: Option<&[(u32, u32)]>, lon: Option<&[(u32, u32)]>) -> Vec<u8> {
    gps_tiff_with(bo, b"II*\0", |w| gps_entries(w, lat, lon))
}

fn exif(tiff: &[u8]) -> Vec<u8> {
    let mut p = b"Exif\0\0".to_vec();
    p.extend_from_slice(tiff);
    p
}

fn segment(marker: u8, payload: &[u8]) -> Vec<u8> {
    let mut s = vec![0xff, marker];
    s.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
    s.extend_from_slice(payload);
    s
}

/// A JPEG: SOI, JFIF, the given segments, a quantisation table, SOS, a little data, EOI.
fn jpeg_with(segments: &[Vec<u8>]) -> Vec<u8> {
    let mut j = vec![0xff, 0xd8];
    j.extend(segment(0xe0, b"JFIF\0\x01\x02\0\0\x01\0\x01\0\0"));
    for s in segments {
        j.extend_from_slice(s);
    }
    j.extend(segment(0xdb, &[0x11; 65]));
    j.extend(segment(0xda, &[0x22; 10]));
    j.extend_from_slice(&[0x33; 40]);
    j.extend_from_slice(&[0xff, 0xd9]);
    j
}

fn jpeg_exif(tiff: &[u8]) -> Vec<u8> {
    jpeg_with(&[segment(0xe1, &exif(tiff))])
}

fn look_bytes(b: &[u8]) -> Look {
    look(&mut Cursor::new(b), b.len() as u64)
}

/// A reader over `data` that panics when a read reaches at or past `limit`.
struct Guard<'a> {
    data: &'a [u8],
    pos: u64,
    limit: u64,
}

impl Read for Guard<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let end = self.pos + buf.len() as u64;
        assert!(end <= self.limit, "read past len: {end} > {}", self.limit);
        let start = (self.pos as usize).min(self.data.len());
        let n = buf.len().min(self.data.len() - start);
        buf[..n].copy_from_slice(&self.data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Guard<'_> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = match to {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
            SeekFrom::End(d) => (self.data.len() as u64).saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

/// The look of `data` with `len` = `limit`, the bytes past it still there for the guard.
fn look_guarded(data: &[u8], limit: u64) -> Look {
    let mut g = Guard {
        data,
        pos: 0,
        limit,
    };
    look(&mut g, limit)
}

const JPEG_GPS: Look = Look::Found(Evidence {
    format: LookFormat::Jpeg,
    kind: EvidenceKind::ExifGps,
});
const TIFF_GPS: Look = Look::Found(Evidence {
    format: LookFormat::Tiff,
    kind: EvidenceKind::ExifGps,
});

#[test]
fn a_jpeg_with_exif_gps_is_found_in_both_byte_orders() {
    for bo in [Bo::Le, Bo::Be] {
        let magic = match bo {
            Bo::Le => b"II*\0",
            Bo::Be => b"MM\0*",
        };
        let t = gps_tiff_with(bo, magic, |w| gps_entries(w, Some(&LAT), Some(&LON)));
        assert_eq!(look_bytes(&jpeg_exif(&t)), JPEG_GPS);
    }
}

#[test]
fn a_jpeg_without_a_gps_ifd_is_none() {
    let mut w = W::new(Bo::Le, b"II*\0", 8);
    let ifd0 = vec![w.long(0x0100, 64), w.short(0x0112, 6)];
    w.ifd(8, &ifd0, 0);
    assert_eq!(look_bytes(&jpeg_exif(&w.buf)), Look::None);
    assert_eq!(look_bytes(&jpeg_with(&[])), Look::None);
}

#[test]
fn a_tiff_with_gps_is_found_in_both_byte_orders() {
    assert_eq!(
        look_bytes(&gps_tiff(Bo::Le, Some(&LAT), Some(&LON))),
        TIFF_GPS
    );
    let be = gps_tiff_with(Bo::Be, b"MM\0*", |w| gps_entries(w, Some(&LAT), Some(&LON)));
    assert_eq!(look_bytes(&be), TIFF_GPS);
}

#[test]
fn a_dng_like_file_with_gps_far_in_is_found() {
    let mut w = W::new(Bo::Le, b"II*\0", 8);
    let ifd0 = vec![
        w.long(0x00fe, 1),
        w.long(0x014a, 1000),
        E {
            tag: 0xc612,
            typ: 1,
            count: 4,
            data: vec![1, 4, 0, 0],
        },
        w.long(0x8825, 3000),
    ];
    w.ifd(8, &ifd0, 0);
    let sub = vec![w.long(0x00fe, 0), w.long(0x0100, 4000)];
    w.ifd(1000, &sub, 0);
    let g = gps_entries(&w, Some(&LAT), Some(&LON));
    w.ifd(3000, &g, 0);
    w.put(9000, &[0x5a; 100]);
    assert_eq!(look_bytes(&w.buf), TIFF_GPS);
}

#[test]
fn panasonic_and_olympus_magics_are_tiffs() {
    for magic in [b"IIU\0", b"IIRO", b"IIRS"] {
        let t = gps_tiff_with(Bo::Le, magic, |w| gps_entries(w, Some(&LAT), Some(&LON)));
        assert_eq!(look_bytes(&t), TIFF_GPS, "{magic:?}");
    }
}

#[test]
fn zero_coordinates_are_none() {
    assert_eq!(
        look_bytes(&jpeg_exif(&gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO)))),
        Look::None
    );
    // A rational with denominator 0 counts as zero.
    let zero_den = [(0, 0), (5, 0), (0, 0)];
    assert_eq!(
        look_bytes(&jpeg_exif(&gps_tiff(
            Bo::Le,
            Some(&zero_den),
            Some(&zero_den)
        ))),
        Look::None
    );
    // One of the two is enough to be a place.
    assert_eq!(
        look_bytes(&jpeg_exif(&gps_tiff(Bo::Le, Some(&ZERO), Some(&LON)))),
        JPEG_GPS
    );
    assert_eq!(
        look_bytes(&jpeg_exif(&gps_tiff(Bo::Le, Some(&LAT), Some(&ZERO)))),
        JPEG_GPS
    );
}

#[test]
fn a_void_status_with_coordinates_is_found() {
    let t = gps_tiff_with(Bo::Le, b"II*\0", |w| {
        let mut e = vec![w.ascii(0x0001, b"N\0")];
        e.push(w.rational(0x0002, &LAT));
        e.push(w.ascii(0x0003, b"W\0"));
        e.push(w.rational(0x0004, &LON));
        e.push(w.ascii(0x0009, b"V\0"));
        e
    });
    assert_eq!(look_bytes(&jpeg_exif(&t)), JPEG_GPS);
}

#[test]
fn a_latitude_alone_is_none() {
    assert_eq!(
        look_bytes(&jpeg_exif(&gps_tiff(Bo::Le, Some(&LAT), None))),
        Look::None
    );
    assert_eq!(
        look_bytes(&jpeg_exif(&gps_tiff(Bo::Le, None, Some(&LON)))),
        Look::None
    );
}

#[test]
fn a_gps_ifd_inside_the_thumbnail_data_is_found() {
    let mut w = W::new(Bo::Le, b"II*\0", 8);
    let ifd0 = vec![w.long(0x0100, 64), w.long(0x8825, 400)];
    w.ifd(8, &ifd0, 100);
    let ifd1 = vec![w.long(0x0201, 300), w.long(0x0202, 300)];
    w.ifd(100, &ifd1, 0);
    w.put(300, &[0xab; 300]);
    let g = gps_entries(&w, Some(&LAT), Some(&LON));
    w.ifd(400, &g, 0);
    assert_eq!(look_bytes(&jpeg_exif(&w.buf)), JPEG_GPS);
}

#[test]
fn a_count_times_size_past_u32_is_unreadable() {
    let t = gps_tiff_with(Bo::Le, b"II*\0", |w| {
        vec![
            w.raw(0x0002, 5, 0x2000_0001, 8),
            w.raw(0x0004, 5, 0x2000_0001, 8),
        ]
    });
    assert_eq!(look_bytes(&jpeg_exif(&t)), Look::Unreadable);
}

#[test]
fn an_ifd_loop_ends_its_chain() {
    // IFD0 names itself as the next IFD.
    let mut w = W::new(Bo::Le, b"II*\0", 8);
    let ifd0 = vec![w.long(0x0100, 64)];
    w.ifd(8, &ifd0, 8);
    assert_eq!(look_bytes(&jpeg_exif(&w.buf)), Look::None);
    // IFD1 points back at IFD0; the GPS pointer at IFD0 itself.
    let mut w = W::new(Bo::Be, b"MM\0*", 8);
    let ifd0 = vec![w.long(0x8825, 8)];
    w.ifd(8, &ifd0, 100);
    let ifd1 = vec![w.long(0x0100, 64)];
    w.ifd(100, &ifd1, 8);
    assert_eq!(look_bytes(&w.buf), Look::None);
}

#[test]
fn a_long_ifd_chain_is_unreadable() {
    let mut w = W::new(Bo::Le, b"II*\0", 8);
    for i in 0..20u32 {
        let at = 8 + i * 40;
        let e = vec![w.long(0x0100, i)];
        w.ifd(at, &e, at + 40);
    }
    assert_eq!(look_bytes(&w.buf), Look::Unreadable);
}

#[test]
fn offsets_outside_the_blob_are_unreadable() {
    let mut w = W::new(Bo::Le, b"II*\0", 8);
    let ifd0 = vec![w.long(0x8825, 0xffff_ff00)];
    w.ifd(8, &ifd0, 0);
    assert_eq!(look_bytes(&jpeg_exif(&w.buf)), Look::Unreadable);
    // IFD0 itself outside.
    let w = W::new(Bo::Le, b"II*\0", 70_000);
    assert_eq!(look_bytes(&jpeg_exif(&w.buf)), Look::Unreadable);
    // A latitude whose value lies past the blob's end (but inside the file).
    let t = gps_tiff_with(Bo::Le, b"II*\0", |w| {
        vec![w.raw(0x0002, 5, 3, 5000), w.raw(0x0004, 5, 3, 5000)]
    });
    let mut j = jpeg_exif(&t);
    j.extend_from_slice(&[0x44; 6000]);
    assert_eq!(look_bytes(&j), Look::Unreadable);
}

#[test]
fn ten_thousand_entries_are_unreadable() {
    let mut t = b"II*\0\x08\0\0\0".to_vec();
    t.extend_from_slice(&10_000u16.to_le_bytes());
    t.resize(8 + 2 + 12 * 10_000 + 4, 0);
    assert_eq!(look_bytes(&t), Look::Unreadable);
}

#[test]
fn big_segments_before_the_exif_are_seeked_over() {
    // 2.4 MB of other APP segments: more than one file may read, so they must not be read.
    let mut segs: Vec<Vec<u8>> = (0..40).map(|_| segment(0xe2, &[0x55; 60_000])).collect();
    segs.push(segment(
        0xe1,
        &exif(&gps_tiff(Bo::Le, Some(&LAT), Some(&LON))),
    ));
    assert_eq!(look_bytes(&jpeg_with(&segs)), JPEG_GPS);
}

#[test]
fn a_second_exif_segment_still_counts_after_a_broken_one() {
    let broken = segment(0xe1, b"Exif\0\0II*\0\xff\xff\xff\x7f");
    let good = segment(0xe1, &exif(&gps_tiff(Bo::Le, Some(&LAT), Some(&LON))));
    assert_eq!(look_bytes(&jpeg_with(&[broken.clone(), good])), JPEG_GPS);
    assert_eq!(look_bytes(&jpeg_with(&[broken])), Look::Unreadable);
}

#[test]
fn other_formats_and_short_files_are_none() {
    assert_eq!(look_bytes(b""), Look::None);
    assert_eq!(look_bytes(b"hello, world"), Look::None);
    assert_eq!(
        look_bytes(b"II+\0\x08\0\x2b\0"),
        Look::None,
        "BigTIFF: a named gap"
    );
    // A file shorter than the `len` it is said to have.
    let j = jpeg_exif(&gps_tiff(Bo::Le, Some(&LAT), Some(&LON)));
    assert_eq!(
        look(&mut Cursor::new(&j[..40]), j.len() as u64),
        Look::Unreadable
    );
}

#[test]
fn the_tiff_helpers_read_blobs() {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    assert_eq!(tiff::gps_present(&t), Ok(true));
    assert_eq!(tiff::orientation(&t), Some(1));
    let mut w = W::new(Bo::Be, b"MM\0*", 8);
    let ifd0 = vec![w.short(0x0112, 6)];
    w.ifd(8, &ifd0, 0);
    assert_eq!(tiff::orientation(&w.buf), Some(6));
    assert_eq!(tiff::gps_present(&w.buf), Ok(false));
    assert_eq!(tiff::orientation(b"II*\0"), None);
    assert_eq!(tiff::gps_present(b"nope"), Err(Bound));
}

#[test]
fn debug_names_no_value() {
    let s = format!("{JPEG_GPS:?}");
    assert_eq!(
        s, "Found(Evidence { format: Jpeg, kind: ExifGps })",
        "only the kind of proof"
    );
}

/// About 2 KiB: a JPEG with EXIF GPS behind a comment segment, and a TIFF with GPS and padding.
fn fuzz_samples() -> Vec<Vec<u8>> {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let jpeg = jpeg_with(&[segment(0xfe, &[0x20; 1400]), segment(0xe1, &exif(&t))]);
    let mut tiff = gps_tiff_with(Bo::Be, b"MM\0*", |w| gps_entries(w, Some(&LAT), Some(&LON)));
    tiff.resize(2048, 0x66);
    let mut jxl = jxl_with(&[
        bx(b"jxlc", &[0x44; 1000]),
        bx(b"Exif", &exif_box(6, &exif(&t))),
    ]);
    jxl.extend(bx(b"jxlp", &[0x55; 300]));
    // A HEIC: an idat item and an Exif item split in two extents of mdat, after the picture.
    let x = exif_box(6, &exif(&t));
    let (a, b) = x.split_at(40);
    let (la, lb) = (a.len() as u64, b.len() as u64);
    let mdat = [&[0x77; 1000][..], a, b].concat();
    let heic = heif_with(
        |data| {
            vec![
                iinf(
                    0,
                    &[
                        infe(2, 1, b"hvc1", b""),
                        infe(3, 2, b"Exif", b""),
                        infe(2, 3, b"mime", b"application/rdf+xml\0"),
                    ],
                ),
                iloc(
                    1,
                    [4, 4, 4, 0],
                    &[
                        loc(1, 0, &[(data, 1000)]),
                        loc(2, 0, &[(data + 1000, la), (data + 1000 + la, lb)]),
                        loc(3, 1, &[(0, 4)]),
                    ],
                ),
                bx(b"idat", b"<x/>"),
            ]
        },
        &mdat,
    );
    vec![
        jpeg,
        tiff,
        jxl,
        heic,
        stills::png_fuzz_sample(),
        stills::webp_fuzz_sample(),
        stills::jpeg_xmp_fuzz_sample(),
    ]
}

#[test]
fn truncation_at_every_length_never_panics_nor_reads_past_len() {
    for sample in fuzz_samples() {
        assert!(sample.len() >= 1500, "{}", sample.len());
        assert_ne!(look_bytes(&sample), Look::None);
        for k in 0..=sample.len() as u64 {
            let _ = look_guarded(&sample, k);
        }
    }
}

#[test]
fn a_flipped_byte_at_every_position_never_panics_nor_reads_past_len() {
    for sample in fuzz_samples() {
        for pos in 0..sample.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut s = sample.clone();
                s[pos] ^= mask;
                let _ = look_guarded(&s, s.len() as u64);
            }
        }
    }
}

// The JPEG XL container.

/// A box with a 32-bit size.
fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(typ);
    b.extend_from_slice(payload);
    b
}

/// A JPEG XL container: the signature, `ftyp`, then the given boxes.
fn jxl_with(boxes: &[Vec<u8>]) -> Vec<u8> {
    let mut f = JXL_SIGNATURE.to_vec();
    f.extend(bx(b"ftyp", b"jxl \0\0\0\0jxl "));
    f.extend(bx(b"jxll", &[5]));
    for b in boxes {
        f.extend_from_slice(b);
    }
    f
}

/// An EXIF payload: the 32-bit offset to the TIFF header, then `rest` (which the offset steps
/// into).
fn exif_box(offset: u32, rest: &[u8]) -> Vec<u8> {
    let mut p = offset.to_be_bytes().to_vec();
    p.extend_from_slice(rest);
    p
}

const JXL_GPS: Look = Look::Found(Evidence {
    format: LookFormat::Jxl,
    kind: EvidenceKind::ExifGps,
});

#[test]
fn a_jxl_exif_box_with_gps_is_found() {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    // Offset 0, the TIFF right after it; behind the codestream.
    let f = jxl_with(&[bx(b"jxlc", &[0x44; 64]), bx(b"Exif", &exif_box(0, &t))]);
    assert_eq!(look_bytes(&f), JXL_GPS);
    // Big-endian TIFF, before the codestream, the codestream to the end (size 0).
    let tb = gps_tiff_with(Bo::Be, b"MM\0*", |w| gps_entries(w, Some(&LAT), Some(&LON)));
    let mut f = jxl_with(&[bx(b"Exif", &exif_box(0, &tb))]);
    f.extend_from_slice(&[0, 0, 0, 0, b'j', b'x', b'l', b'c', 0x44, 0x44]);
    assert_eq!(look_bytes(&f), JXL_GPS);
}

#[test]
fn a_jxl_exif_offset_is_honoured() {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    // Apple's form: offset 6, then `Exif\0\0`, then the TIFF.
    let apple = exif_box(6, &exif(&t));
    assert_eq!(&apple[..10], b"\0\0\0\x06Exif\0\0");
    assert_eq!(look_bytes(&jxl_with(&[bx(b"Exif", &apple)])), JXL_GPS);
    // Ten bytes of something else before the TIFF.
    let ten = exif_box(10, &[&b"zzzzzzzzzz"[..], &t].concat());
    assert_eq!(&ten[4..14], b"zzzzzzzzzz");
    assert_eq!(look_bytes(&jxl_with(&[bx(b"Exif", &ten)])), JXL_GPS);
    // Offset 0 although `Exif\0\0` comes first: read too.
    let mut forgot = 0u32.to_be_bytes().to_vec();
    forgot.extend(exif(&t));
    assert_eq!(look_bytes(&jxl_with(&[bx(b"Exif", &forgot)])), JXL_GPS);
    // An offset past the box, and a box too short for the offset.
    let mut past = exif_box(0, &t);
    past[..4].copy_from_slice(&(t.len() as u32 + 1).to_be_bytes());
    assert_eq!(
        look_bytes(&jxl_with(&[bx(b"Exif", &past)])),
        Look::Unreadable
    );
    assert_eq!(
        look_bytes(&jxl_with(&[bx(b"Exif", &[0, 0])])),
        Look::Unreadable
    );
    assert_eq!(bmff::exif_payload(&mut Slice(&exif_box(0, &t))), Ok(true));
    let mut huge = exif_box(0, &t);
    huge[..4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(bmff::exif_payload(&mut Slice(&huge)), Err(Bound));
}

#[test]
fn a_jxl_without_a_location_is_none() {
    let zero = gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO));
    assert_eq!(
        look_bytes(&jxl_with(&[bx(b"Exif", &exif_box(0, &zero))])),
        Look::None
    );
    assert_eq!(look_bytes(&jxl_with(&[bx(b"jxlc", &[1; 40])])), Look::None);
    assert_eq!(look_bytes(&jxl_with(&[])), Look::None);
    // A bare codestream has no boxes, and so no metadata.
    assert_eq!(
        look_bytes(&[0xff, 0x0a, 0xfa, 0x12, 0, 0, 0, 0]),
        Look::None
    );
    // Brotli-compressed EXIF: a named gap.
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let mut brob = b"Exif".to_vec();
    brob.extend_from_slice(&exif_box(0, &t));
    assert_eq!(look_bytes(&jxl_with(&[bx(b"brob", &brob)])), Look::None);
}

#[test]
fn a_broken_jxl_exif_box_is_unreadable_unless_another_is_found() {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let broken = bx(b"Exif", &exif_box(0, b"II*\0\xff\xff\xff\x7f"));
    let good = bx(b"Exif", &exif_box(0, &t));
    assert_eq!(look_bytes(&jxl_with(&[broken.clone(), good])), JXL_GPS);
    assert_eq!(look_bytes(&jxl_with(&[broken])), Look::Unreadable);
    // A box structure that breaks (size 3).
    let mut f = jxl_with(&[bx(b"jxlc", &[1; 8])]);
    f.extend_from_slice(&[0, 0, 0, 3, b'a', b'b', b'c', b'd', 0, 0, 0, 0]);
    assert_eq!(look_bytes(&f), Look::Unreadable);
}

#[test]
fn jxl_boxes_count_against_the_header_bound_but_skipped_ones_do_not() {
    let t = gps_tiff(Bo::Le, Some(&LAT), Some(&LON));
    let exif_gps = bx(b"Exif", &exif_box(0, &t));
    // More small boxes than the bound, then the location: never reached.
    let many: Vec<Vec<u8>> = (0..=HEADERS_MAX).map(|_| bx(b"junk", &[])).collect();
    let mut f = jxl_with(&many);
    f.extend_from_slice(&exif_gps);
    assert_eq!(look_bytes(&f), Look::Unreadable);
    // 150,000 codestream parts are only skipped: the location after them is found.
    let parts: Vec<Vec<u8>> = (0..150_000).map(|_| bx(b"jxlp", &[])).collect();
    let mut f = jxl_with(&parts);
    f.extend_from_slice(&exif_gps);
    assert_eq!(look_bytes(&f), JXL_GPS);
}

// HEIF (HEIC, AVIF): items of the top-level `meta`.

/// A FullBox.
fn fbx(typ: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![version, 0, 0, 0];
    p.extend_from_slice(payload);
    bx(typ, &p)
}

/// An unsigned big-endian field of `n` bytes.
fn be(v: u64, n: usize) -> Vec<u8> {
    v.to_be_bytes()[8 - n..].to_vec()
}

/// An `infe` of `version` (2: 16-bit id, 3: 32-bit), named, with `rest` after the name.
fn infe(version: u8, id: u32, typ: &[u8; 4], rest: &[u8]) -> Vec<u8> {
    let mut p = be(u64::from(id), if version == 3 { 4 } else { 2 });
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(typ);
    p.extend_from_slice(b"item\0");
    p.extend_from_slice(rest);
    fbx(b"infe", version, &p)
}

/// An `iinf` of `version` (0: 16-bit count, 1: 32-bit) over `entries`.
fn iinf(version: u8, entries: &[Vec<u8>]) -> Vec<u8> {
    let mut p = be(entries.len() as u64, if version == 0 { 2 } else { 4 });
    for e in entries {
        p.extend_from_slice(e);
    }
    fbx(b"iinf", version, &p)
}

/// One `iloc` entry.
#[derive(Clone)]
struct Loc {
    id: u32,
    method: u16,
    data_ref: u16,
    base: u64,
    extents: Vec<(u64, u64)>,
}

fn loc(id: u32, method: u16, extents: &[(u64, u64)]) -> Loc {
    Loc {
        id,
        method,
        data_ref: 0,
        base: 0,
        extents: extents.to_vec(),
    }
}

/// An `iloc` of `version` with the field sizes (offset, length, base offset, index).
fn iloc(version: u8, sizes: [usize; 4], items: &[Loc]) -> Vec<u8> {
    let [os, ls, bs, is] = sizes;
    let mut p = vec![((os << 4) | ls) as u8, ((bs << 4) | is) as u8];
    let id_size = if version == 2 { 4 } else { 2 };
    p.extend(be(items.len() as u64, id_size));
    for it in items {
        p.extend(be(u64::from(it.id), id_size));
        if version > 0 {
            p.extend(be(u64::from(it.method), 2));
        }
        p.extend(be(u64::from(it.data_ref), 2));
        p.extend(be(it.base, bs));
        p.extend(be(it.extents.len() as u64, 2));
        for &(o, l) in &it.extents {
            if version > 0 {
                p.extend(be(7, is));
            }
            p.extend(be(o, os));
            p.extend(be(l, ls));
        }
    }
    fbx(b"iloc", version, &p)
}

/// A HEIC: `ftyp`, then `meta` (its `hdlr` and the children `children(data)` gives, where `data`
/// is the file offset of the `mdat` payload), then `mdat` holding `mdat`.
fn heif_with(children: impl Fn(u64) -> Vec<Vec<u8>>, mdat: &[u8]) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    let meta = |data: u64| {
        let mut p = fbx(
            b"hdlr",
            0,
            &[&[0; 4][..], b"pict", &[0; 12], b"\0"].concat(),
        );
        for c in children(data) {
            p.extend(c);
        }
        fbx(b"meta", 0, &p)
    };
    let data = (ftyp.len() + meta(0).len() + 8) as u64;
    let mut f = ftyp;
    f.extend(meta(data));
    f.extend(bx(b"mdat", mdat));
    f
}

/// Apple's EXIF item: offset 6, `Exif\0\0`, a TIFF with the made-up location.
fn heif_exif() -> Vec<u8> {
    exif_box(6, &exif(&gps_tiff(Bo::Le, Some(&LAT), Some(&LON))))
}

/// A HEIC whose Exif item (id 2, beside a picture item 1) is in `mdat` after 100 bytes of picture.
fn heic_simple(infe_v: u8, iinf_v: u8, iloc_v: u8, sizes: [usize; 4], exif_item: &[u8]) -> Vec<u8> {
    let mut mdat = vec![0x77; 100];
    mdat.extend_from_slice(exif_item);
    let n = exif_item.len() as u64;
    heif_with(
        |data| {
            vec![
                iinf(
                    iinf_v,
                    &[infe(infe_v, 1, b"hvc1", b""), infe(infe_v, 2, b"Exif", b"")],
                ),
                iloc(
                    iloc_v,
                    sizes,
                    &[loc(1, 0, &[(data, 100)]), loc(2, 0, &[(data + 100, n)])],
                ),
            ]
        },
        &mdat,
    )
}

const HEIF_GPS: Look = Look::Found(Evidence {
    format: LookFormat::Heif,
    kind: EvidenceKind::ExifGps,
});

#[test]
fn a_heic_exif_item_at_the_file_end_is_found() {
    let f = heic_simple(2, 0, 0, [4, 4, 0, 0], &heif_exif());
    assert_eq!(look_bytes(&f), HEIF_GPS);
    // 32-bit ids everywhere (infe v3, iinf v1, iloc v2).
    let f = heic_simple(3, 1, 2, [4, 4, 0, 0], &heif_exif());
    assert_eq!(look_bytes(&f), HEIF_GPS);
    // An AVIF looks the same.
    let mut avif = heic_simple(2, 0, 1, [4, 4, 0, 0], &heif_exif());
    avif[8..12].copy_from_slice(b"avif");
    assert_eq!(look_bytes(&avif), HEIF_GPS);
}

#[test]
fn iloc_field_sizes_of_zero_four_and_eight_are_read() {
    let x = heif_exif();
    for sizes in [[4, 4, 0, 0], [8, 8, 0, 0], [8, 4, 8, 4], [4, 8, 4, 8]] {
        for v in [0, 1, 2] {
            assert_eq!(
                look_bytes(&heic_simple(2, 0, v, sizes, &x)),
                HEIF_GPS,
                "{sizes:?} v{v}"
            );
        }
    }
    // A base offset plus extent offsets; offset size 0 (offset 0) with the base carrying it.
    let mut mdat = vec![0x77; 40];
    mdat.extend_from_slice(&x);
    let n = x.len() as u64;
    let f = heif_with(
        |data| {
            let mut l = loc(5, 0, &[(0, n)]);
            l.base = data + 40;
            vec![
                iinf(0, &[infe(2, 5, b"Exif", b"")]),
                iloc(1, [0, 4, 8, 0], &[l]),
            ]
        },
        &mdat,
    );
    assert_eq!(look_bytes(&f), HEIF_GPS);
    // A size of 2 is not one of 0, 4, 8.
    assert_eq!(
        look_bytes(&heic_simple(2, 0, 1, [2, 4, 0, 0], &x)),
        Look::Unreadable
    );
}

#[test]
fn a_heic_exif_item_inside_idat_is_found() {
    let x = heif_exif();
    let n = x.len() as u64;
    // Construction method 1: offsets into `idat`'s payload (8 bytes of something first).
    let idat = [&[0x99; 8][..], &x].concat();
    let f = heif_with(
        |_| {
            vec![
                iinf(0, &[infe(2, 1, b"Exif", b"")]),
                iloc(1, [4, 4, 0, 0], &[loc(1, 1, &[(8, n)])]),
                bx(b"idat", &idat),
            ]
        },
        &[0x77; 64],
    );
    assert_eq!(look_bytes(&f), HEIF_GPS);
    // Without `idat`.
    let f = heif_with(
        |_| {
            vec![
                iinf(0, &[infe(2, 1, b"Exif", b"")]),
                iloc(1, [4, 4, 0, 0], &[loc(1, 1, &[(8, n)])]),
            ]
        },
        &[0x77; 64],
    );
    assert_eq!(look_bytes(&f), Look::Unreadable);
}

#[test]
fn a_heic_exif_item_in_two_extents_or_to_the_end_is_found() {
    let x = heif_exif();
    let (a, b) = x.split_at(30);
    // The second half first in `mdat`, 50 bytes of picture between them.
    let mdat = [b, &[0x77; 50][..], a].concat();
    let (la, lb) = (a.len() as u64, b.len() as u64);
    let f = heif_with(
        |data| {
            vec![
                iinf(0, &[infe(2, 1, b"Exif", b"")]),
                iloc(
                    0,
                    [4, 4, 0, 0],
                    &[loc(1, 0, &[(data + lb + 50, la), (data, lb)])],
                ),
            ]
        },
        &mdat,
    );
    assert_eq!(look_bytes(&f), HEIF_GPS);
    // An extent length of 0 reaches the end of the file.
    let f = heif_with(
        |data| {
            vec![
                iinf(0, &[infe(2, 1, b"Exif", b"")]),
                iloc(0, [4, 4, 0, 0], &[loc(1, 0, &[(data, 0)])]),
            ]
        },
        &x,
    );
    assert_eq!(look_bytes(&f), HEIF_GPS);
}

#[test]
fn heic_items_the_look_cannot_reach_are_unreadable() {
    let x = heif_exif();
    let n = x.len() as u64;
    let with = |l: Loc| {
        heif_with(
            move |data| {
                let mut l = l.clone();
                for e in &mut l.extents {
                    e.0 += data;
                }
                vec![
                    iinf(0, &[infe(2, 1, b"Exif", b"")]),
                    iloc(1, [4, 4, 0, 4], &[l]),
                ]
            },
            &x,
        )
    };
    // Construction method 2 (offsets into another item).
    assert_eq!(look_bytes(&with(loc(1, 2, &[(0, n)]))), Look::Unreadable);
    // Data in another file.
    let mut other = loc(1, 0, &[(0, n)]);
    other.data_ref = 1;
    assert_eq!(look_bytes(&with(other)), Look::Unreadable);
    // The right location reads (the closure is sound).
    assert_eq!(look_bytes(&with(loc(1, 0, &[(0, n)]))), HEIF_GPS);
    // Past the file's end.
    assert_eq!(
        look_bytes(&with(loc(1, 0, &[(0, n + 1)]))),
        Look::Unreadable
    );
    // An Exif item without a location, and a `meta` without `iloc`.
    assert_eq!(look_bytes(&with(loc(9, 0, &[(0, n)]))), Look::Unreadable);
    let f = heif_with(|_| vec![iinf(0, &[infe(2, 1, b"Exif", b"")])], &x);
    assert_eq!(look_bytes(&f), Look::Unreadable);
    // An unknown iloc version.
    let mut f = heic_simple(2, 0, 1, [4, 4, 0, 0], &x);
    let at = f.windows(4).position(|w| w == b"iloc").unwrap() + 4;
    f[at] = 3;
    assert_eq!(look_bytes(&f), Look::Unreadable);
}

#[test]
fn a_heic_without_a_location_is_none() {
    let zero = exif_box(6, &exif(&gps_tiff(Bo::Le, Some(&ZERO), Some(&ZERO))));
    assert_eq!(
        look_bytes(&heic_simple(2, 0, 0, [4, 4, 0, 0], &zero)),
        Look::None
    );
    // No Exif item.
    let f = heif_with(
        |data| {
            vec![
                iinf(0, &[infe(2, 1, b"hvc1", b"")]),
                iloc(0, [4, 4, 0, 0], &[loc(1, 0, &[(data, 10)])]),
            ]
        },
        &[0x77; 10],
    );
    assert_eq!(look_bytes(&f), Look::None);
    // `infe` v1 has no item type; a `meta` without `iinf`; an MP4 without `meta`.
    let x = heif_exif();
    assert_eq!(
        look_bytes(&heic_simple(1, 0, 0, [4, 4, 0, 0], &x)),
        Look::None
    );
    assert_eq!(look_bytes(&heif_with(|_| vec![], &x)), Look::None);
    let mut mp4 = bx(b"ftyp", b"isom\0\0\0\0isomiso2");
    mp4.extend(bx(b"free", &[0; 16]));
    mp4.extend(bx(b"mdat", &[0x55; 64]));
    assert_eq!(look_bytes(&mp4), Look::None);
}

#[test]
fn a_broken_heic_exif_item_is_unreadable_unless_another_is_found() {
    let good = heif_exif();
    let broken = exif_box(0, b"II*\0\xff\xff\xff\x7f");
    let mdat = [&broken[..], &good].concat();
    let (b, g) = (broken.len() as u64, good.len() as u64);
    let both = |items: &'static [u32]| {
        heif_with(
            move |data| {
                vec![
                    iinf(
                        0,
                        &items
                            .iter()
                            .map(|&id| infe(2, id, b"Exif", b""))
                            .collect::<Vec<_>>(),
                    ),
                    iloc(
                        0,
                        [4, 4, 0, 0],
                        &[loc(1, 0, &[(data, b)]), loc(2, 0, &[(data + b, g)])],
                    ),
                ]
            },
            &mdat,
        )
    };
    assert_eq!(look_bytes(&both(&[1, 2])), HEIF_GPS);
    assert_eq!(look_bytes(&both(&[1])), Look::Unreadable);
    assert_eq!(look_bytes(&both(&[2])), HEIF_GPS);
}

#[test]
fn more_exif_items_than_the_bound_are_unreadable() {
    let x = heif_exif();
    let n = x.len() as u64;
    let f = |count: u32| {
        heif_with(
            move |data| {
                let entries: Vec<Vec<u8>> =
                    (1..=count).map(|id| infe(2, id, b"Exif", b"")).collect();
                let locs: Vec<Loc> = (1..=count).map(|id| loc(id, 0, &[(data, n)])).collect();
                vec![iinf(0, &entries), iloc(0, [4, 4, 0, 0], &locs)]
            },
            &x,
        )
    };
    assert_eq!(look_bytes(&f(64)), HEIF_GPS);
    assert_eq!(look_bytes(&f(65)), Look::Unreadable);
}

/// LOC PR1: a HEIC whose `iloc` is `iloc_payload` (after the version and flags), beside an
/// `iinf` naming a picture (1) and the Exif item (2), with an `mdat` of a few bytes.
fn heic_with_raw_iloc(version: u8, iloc_payload: &[u8]) -> Vec<u8> {
    heif_with(
        |_| {
            vec![
                iinf(0, &[infe(2, 1, b"hvc1", b""), infe(2, 2, b"Exif", b"")]),
                fbx(b"iloc", version, iloc_payload),
            ]
        },
        &[0x77; 16],
    )
}

/// An `iloc` v1 payload with all four field sizes 0 and `entries` of (id, extent count): an
/// extent costs no byte, an entry 8.
fn zero_size_iloc(entries: &[(u16, u16)]) -> Vec<u8> {
    let mut p = vec![0, 0]; // offset/length sizes 0, base/index sizes 0
    p.extend((entries.len() as u16).to_be_bytes());
    for &(id, extents) in entries {
        p.extend(id.to_be_bytes());
        p.extend([0, 0]); // construction method 0
        p.extend([0, 0]); // data reference 0
        p.extend(extents.to_be_bytes());
    }
    p
}

/// A HEIC whose `iloc` (v1, offset and length sizes 0, base offset 4 bytes: every extent costs
/// no byte, "offset 0, length 0" = from the base to the end of the file) has the picture (1) and
/// the Exif item (2) at the file's end, found as they stand, and after them `rest`: entries of
/// (id, extent count) that name other items.
fn heic_with_free_extents(rest: &[(u16, u16)]) -> Vec<u8> {
    let exif = heif_exif();
    let mut mdat = vec![0x77; 100];
    mdat.extend_from_slice(&exif);
    heif_with(
        |data| {
            let mut p = vec![0x00, 0x40]; // sizes: offset 0, length 0, base 4, index 0
            p.extend(((2 + rest.len()) as u16).to_be_bytes());
            let mut entry = |id: u16, base: u64, extents: u16| {
                p.extend(id.to_be_bytes());
                p.extend([0, 0, 0, 0]); // method 0, data reference 0
                p.extend(be(base, 4));
                p.extend(extents.to_be_bytes());
            };
            entry(1, data, 1);
            entry(2, data + 100, 1);
            for &(id, extents) in rest {
                entry(id, data, extents);
            }
            vec![
                iinf(0, &[infe(2, 1, b"hvc1", b""), infe(2, 2, b"Exif", b"")]),
                fbx(b"iloc", 1, &p),
            ]
        },
        &mdat,
    )
}

/// The best of up to three looks (stopping at the first fast one), as the P2 test does: an
/// Unreadable answer for the `bound` reason, in well under 100 ms.
fn assert_unreadable_by_bound_fast(f: &[u8], what: &str) {
    let mut best = std::time::Duration::MAX;
    for _ in 0..3 {
        let t = std::time::Instant::now();
        let (look, why) = look_why(&mut Cursor::new(f), f.len() as u64);
        best = best.min(t.elapsed());
        assert_eq!(look, Look::Unreadable, "{what}");
        let why = why.expect("an unreadable look has its reason");
        assert_eq!(why.reason(), "bound", "{what}");
        assert_eq!(why.format_name(), "bmff", "{what}");
        if best < std::time::Duration::from_millis(100) || best > std::time::Duration::from_secs(5)
        {
            break;
        }
    }
    assert!(
        best < std::time::Duration::from_millis(100),
        "{what} took {best:?}"
    );
}

/// The two files of the re-check (LOC PR1): `iloc` v1 with field sizes 0, so an extent costs no
/// byte; before the caps the first took 12 s and the second about 1 GiB.
#[test]
fn the_hostile_iloc_files_are_unreadable_by_the_bound_in_no_time() {
    // 65535 entries of 65535 free extents, ids that are not the Exif item's.
    let foreign: Vec<(u16, u16)> = (0..65535u32)
        .map(|i| (1000 + (i % 60000) as u16, 65535))
        .collect();
    let f = heic_with_raw_iloc(1, &zero_size_iloc(&foreign));
    assert_unreadable_by_bound_fast(&f, "65535 entries x 65535 extents");
    // Behind a picture and an Exif item that are found as they stand.
    let f = heic_with_free_extents(&foreign);
    assert_unreadable_by_bound_fast(&f, "the same behind a found item");
    // 1024 entries, all for the Exif item (the file of 8,340 bytes).
    let same: Vec<(u16, u16)> = (0..1024).map(|_| (2, 65535)).collect();
    let f = heic_with_raw_iloc(1, &zero_size_iloc(&same));
    assert_unreadable_by_bound_fast(&f, "1024 entries for the Exif item");
}

/// Each cap alone: a file whose picture and Exif item are found as they stand, and one hostile
/// thing beside them. (A file without a location for the Exif item is Unreadable anyway, so only
/// a found item shows which cap holds.)
#[test]
fn each_iloc_cap_alone_makes_a_found_item_unreadable() {
    assert_eq!(look_bytes(&heic_with_free_extents(&[])), HEIF_GPS);
    // At the caps: 64 extents in an entry, 4096 in all (the item entries take 2).
    let at = |n: usize| -> Vec<(u16, u16)> {
        let mut rest: Vec<(u16, u16)> = (0..n / 64).map(|i| (100 + i as u16, 64)).collect();
        if !n.is_multiple_of(64) {
            rest.push((999, (n % 64) as u16));
        }
        rest
    };
    assert_eq!(look_bytes(&heic_with_free_extents(&at(4094))), HEIF_GPS);
    assert_unreadable_by_bound_fast(
        &heic_with_free_extents(&at(4095)),
        "4097 extents in the whole iloc",
    );
    // One entry of 65 extents (4 in all besides): the per-entry cap.
    assert_unreadable_by_bound_fast(
        &heic_with_free_extents(&[(100, 65)]),
        "65 extents in one entry",
    );
    // Many entries of one extent each: the total cap.
    let ones: Vec<(u16, u16)> = (0..5000).map(|i| (100 + i, 1)).collect();
    assert_unreadable_by_bound_fast(&heic_with_free_extents(&ones), "5000 entries");
    // A second entry for the Exif item (a wanted id): the duplicate check.
    assert_unreadable_by_bound_fast(
        &heic_with_free_extents(&[(2, 1)]),
        "a second entry for the Exif item",
    );
}

#[test]
fn a_valid_heic_still_answers_as_before_the_iloc_caps() {
    let f = heic_simple(2, 0, 1, [4, 4, 0, 0], &heif_exif());
    assert_eq!(look_bytes(&f), HEIF_GPS);
    // A zero-size field file (length 0 = to the end of its data) is read, not refused.
    let f = heic_simple(2, 0, 1, [4, 0, 0, 0], &heif_exif());
    assert_eq!(look_bytes(&f), HEIF_GPS);
}

#[test]
fn a_heic_xmp_item_is_read_as_an_item() {
    let xmp = stills::GPS_XMP.to_vec();
    let n = xmp.len() as u64;
    let f = heif_with(
        |data| {
            vec![
                iinf(
                    0,
                    &[
                        infe(2, 1, b"mime", b"APPLICATION/rdf+xml\0"),
                        infe(2, 2, b"mime", b"image/jpeg\0"),
                        infe(3, 3, b"mime", b"application/rdf+xml"),
                    ],
                ),
                iloc(
                    0,
                    [4, 4, 0, 0],
                    &[loc(1, 0, &[(data, n)]), loc(2, 0, &[(data, 4)])],
                ),
            ]
        },
        &xmp,
    );
    let mut src = Slice(&f);
    let mut top = bmff::Boxes::top(f.len() as u64);
    let meta = loop {
        let b = top.next(&mut src).unwrap().unwrap();
        if b.typ == *b"meta" {
            break b;
        }
    };
    // Only the first: the second is a JPEG, the third's content type has no NUL.
    assert_eq!(
        heif::meta_items(&mut src, &meta),
        Ok(vec![(heif::ItemKind::Xmp, Ok(xmp))])
    );
    // The XMP search reads it.
    assert_eq!(
        look_bytes(&f),
        Look::Found(Evidence {
            format: LookFormat::Heif,
            kind: EvidenceKind::Xmp,
        })
    );
}

#[test]
fn the_brand_does_not_choose_the_walk() {
    // One ISO-BMFF branch: a `meta` with HEIF items is read under a movie brand, and a
    // located `moov` under a HEIF brand.
    let mut f = heic_simple(2, 0, 0, [4, 4, 0, 0], &heif_exif());
    f[8..12].copy_from_slice(b"isom");
    assert_eq!(look_bytes(&f), HEIF_GPS);
    let mp4 = crate::sample::sample_located(crate::SampleLocated::Mp4);
    let ftyp_len = u32::from_be_bytes(mp4[0..4].try_into().unwrap()) as usize;
    let mut heic_movie = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
    heic_movie.extend_from_slice(&mp4[ftyp_len..]);
    assert!(matches!(
        look_bytes(&heic_movie),
        Look::Found(Evidence {
            format: LookFormat::Bmff,
            ..
        })
    ));
}

// The movies (MP4, MOV, 3GP, M4A).
mod movie;

// PNG, WebP and the XMP places of every format.
mod stills;

// The reads a hostile file makes (LOC RF1).
mod reads;
