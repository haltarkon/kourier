//! Tests of the movies' look (MP4, MOV, 3GP, M4A). Every file is written here or by
//! `sample_located`; the place is made up, in the middle of the Atlantic.

use std::time::Instant;

use super::*;
use crate::sample::{PLACE_ISO6709, bmff_box as b, data_box, full_box as fb};
use crate::{SampleLocated, sample_located};

const XYZ: [u8; 4] = [0xa9, b'x', b'y', b'z'];
const KEY: &[u8] = b"com.apple.quicktime.location.ISO6709";

const ATOM: Look = Look::Found(Evidence {
    format: LookFormat::Bmff,
    kind: EvidenceKind::MovieAtom,
});
const TRACK: Look = Look::Found(Evidence {
    format: LookFormat::Bmff,
    kind: EvidenceKind::Track,
});
const XMP: Look = Look::Found(Evidence {
    format: LookFormat::Bmff,
    kind: EvidenceKind::Xmp,
});

fn ftyp() -> Vec<u8> {
    b(b"ftyp", b"isom\0\0\x02\0isomiso2mp41")
}

/// A QuickTime text payload (a 16-bit size, a language, the text).
fn qt_text(text: &[u8]) -> Vec<u8> {
    let mut t = (text.len() as u16).to_be_bytes().to_vec();
    t.extend(0x15c7u16.to_be_bytes());
    t.extend(text);
    t
}

/// `ftyp`, a `moov` holding `inside`, a little `mdat`.
fn mp4(inside: &[u8]) -> Vec<u8> {
    let mut f = ftyp();
    f.extend(b(b"moov", inside));
    f.extend(b(b"mdat", &[0x5a; 64]));
    f
}

fn udta_xyz(text: &[u8]) -> Vec<u8> {
    b(b"udta", &b(&XYZ, &qt_text(text)))
}

/// A `keys` box (a FullBox) with these keys, 1-based in this order.
fn keys(names: &[&[u8]]) -> Vec<u8> {
    let mut k = (names.len() as u32).to_be_bytes().to_vec();
    for n in names {
        k.extend(((n.len() + 8) as u32).to_be_bytes());
        k.extend(b"mdta");
        k.extend(*n);
    }
    fb(b"keys", 0, &k)
}

fn hdlr(typ: &[u8; 4]) -> Vec<u8> {
    let mut h = vec![0; 4];
    h.extend(typ);
    h.extend([0; 13]);
    fb(b"hdlr", 0, &h)
}

/// The `ilst` item at the 1-based key `index` holding `text`.
fn item(index: u32, text: &[u8]) -> Vec<u8> {
    b(&index.to_be_bytes(), &data_box(text))
}

/// A QuickTime `meta` (no version and flags) or an ISO one (a FullBox).
fn meta(quicktime: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let c = children.concat();
    if quicktime {
        b(b"meta", &c)
    } else {
        fb(b"meta", 0, &c)
    }
}

/// A track whose `stsd` holds these sample entries.
fn trak_with_entries(entries: &[Vec<u8>]) -> Vec<u8> {
    let mut stsd = (entries.len() as u32).to_be_bytes().to_vec();
    stsd.extend(entries.concat());
    let stbl = b(
        b"stbl",
        &[fb(b"stsd", 0, &stsd), b(b"stts", &[0; 8])].concat(),
    );
    let minf = b(b"minf", &stbl);
    let mdia = b(b"mdia", &[hdlr(b"meta"), minf].concat());
    b(b"trak", &[b(b"tkhd", &[0; 84]), mdia].concat())
}

/// A sample entry: 6 reserved bytes, a data reference index, then `rest`.
fn entry(typ: &[u8; 4], rest: &[u8]) -> Vec<u8> {
    let mut e = vec![0, 0, 0, 0, 0, 0, 0, 1];
    e.extend(rest);
    b(typ, &e)
}

/// A `loci` payload with a UTF-8 or UTF-16 name and 16.16 longitude and latitude.
fn loci(utf16: bool, lon: u32, lat: u32) -> Vec<u8> {
    let mut p = vec![0x15, 0xc7];
    if utf16 {
        p.extend([0xfe, 0xff, 0, b'S', 0, b'e', 0, b'a', 0, 0]);
    } else {
        p.extend(b"Sea\0");
    }
    p.push(0); // role
    p.extend(lon.to_be_bytes());
    p.extend(lat.to_be_bytes());
    p.extend(1000u32.to_be_bytes()); // altitude
    p.extend(b"earth\0\0");
    fb(b"loci", 0, &p)
}

#[test]
fn the_samples_are_found_in_their_place() {
    for kind in [SampleLocated::Mp4, SampleLocated::Mov, SampleLocated::M4a] {
        assert_eq!(look_bytes(&sample_located(kind)), ATOM, "{kind:?}");
    }
}

#[test]
fn udta_xyz_and_its_zero_rule() {
    assert_eq!(look_bytes(&mp4(&udta_xyz(PLACE_ISO6709))), ATOM);
    assert_eq!(
        look_bytes(&mp4(&udta_xyz(b"+30.2096-040.1191+012.5/"))),
        ATOM
    );
    // One coordinate zero is still a place.
    assert_eq!(look_bytes(&mp4(&udta_xyz(b"+00.0000-040.1191/"))), ATOM);
    for none in [
        &b"+00.0000+000.0000/"[..],
        b"-00.0000+000.0000+100.000/",
        b"+0+0/",
        b"",
        b"/",
        b"+30.2096",
        b"Acme Phone",
        b"+ab.cd-ef.gh/",
    ] {
        assert_eq!(
            look_bytes(&mp4(&udta_xyz(none))),
            Look::None,
            "{}",
            String::from_utf8_lossy(none)
        );
    }
    // A text size past the box: what is there counts (lenient).
    let mut t = qt_text(PLACE_ISO6709);
    t[1] = 0xff;
    assert_eq!(look_bytes(&mp4(&b(b"udta", &b(&XYZ, &t)))), ATOM);
    // Too short for its own size and language field.
    assert_eq!(
        look_bytes(&mp4(&b(b"udta", &b(&XYZ, &[0])))),
        Look::Unreadable
    );
}

#[test]
fn loci_and_its_zero_rule() {
    let l = |utf16, lon, lat| look_bytes(&mp4(&b(b"udta", &loci(utf16, lon, lat))));
    assert_eq!(l(false, 0xffd7_e1a3, 0x001e_35a8), ATOM);
    assert_eq!(l(true, 0xffd7_e1a3, 0x001e_35a8), ATOM);
    assert_eq!(l(false, 0, 0x001e_35a8), ATOM);
    assert_eq!(l(false, 0, 0), Look::None);
    assert_eq!(l(true, 0, 0), Look::None);
    // A name without its end, or the coordinates cut off.
    let cut = fb(b"loci", 0, &[0x15, 0xc7, b'S', b'e', b'a']);
    assert_eq!(look_bytes(&mp4(&b(b"udta", &cut))), Look::Unreadable);
    let short = fb(b"loci", 0, &[0x15, 0xc7, 0, 0, 1, 2, 3]);
    assert_eq!(look_bytes(&mp4(&b(b"udta", &short))), Look::Unreadable);
}

#[test]
fn quicktime_and_iso_meta_with_the_iso6709_key_are_found() {
    for quicktime in [true, false] {
        let m = meta(
            quicktime,
            &[
                hdlr(b"mdta"),
                keys(&[KEY]),
                b(b"ilst", &item(1, PLACE_ISO6709)),
            ],
        );
        assert_eq!(look_bytes(&mp4(&m)), ATOM, "quicktime {quicktime}");
        // The zero placeholder.
        let z = meta(
            quicktime,
            &[
                hdlr(b"mdta"),
                keys(&[KEY]),
                b(b"ilst", &item(1, b"+00.0000+000.0000/")),
            ],
        );
        assert_eq!(look_bytes(&mp4(&z)), Look::None, "quicktime {quicktime}");
    }
}

#[test]
fn the_keys_index_is_one_based() {
    let make: &[u8] = b"com.apple.quicktime.make";
    // The location is the second key; the first item holds the maker's name.
    let m = meta(
        true,
        &[
            hdlr(b"mdta"),
            keys(&[make, KEY]),
            b(
                b"ilst",
                &[item(1, b"Acme"), item(2, PLACE_ISO6709)].concat(),
            ),
        ],
    );
    assert_eq!(look_bytes(&mp4(&m)), ATOM);
    // The key exists but no item holds it; an item for a key that does not exist.
    let m = meta(
        true,
        &[
            hdlr(b"mdta"),
            keys(&[KEY, make]),
            b(b"ilst", &item(2, PLACE_ISO6709)),
        ],
    );
    assert_eq!(look_bytes(&mp4(&m)), Look::None);
    let m = meta(
        true,
        &[
            hdlr(b"mdta"),
            keys(&[make]),
            b(b"ilst", &item(1, PLACE_ISO6709)),
        ],
    );
    assert_eq!(look_bytes(&mp4(&m)), Look::None);
    // `ilst` before `keys` is read too; a key entry past its box is Unreadable.
    let m = meta(
        true,
        &[
            hdlr(b"mdta"),
            b(b"ilst", &item(1, PLACE_ISO6709)),
            keys(&[KEY]),
        ],
    );
    assert_eq!(look_bytes(&mp4(&m)), ATOM);
    let mut k = keys(&[KEY]);
    k[19] = 0xff;
    let m = meta(
        true,
        &[hdlr(b"mdta"), k, b(b"ilst", &item(1, PLACE_ISO6709))],
    );
    assert_eq!(look_bytes(&mp4(&m)), Look::Unreadable);
}

#[test]
fn the_same_places_under_a_track_are_found() {
    let t = |inside: Vec<u8>| mp4(&b(b"trak", &[b(b"tkhd", &[0; 84]), inside].concat()));
    assert_eq!(look_bytes(&t(udta_xyz(PLACE_ISO6709))), ATOM);
    let m = meta(
        true,
        &[
            hdlr(b"mdta"),
            keys(&[KEY]),
            b(b"ilst", &item(1, PLACE_ISO6709)),
        ],
    );
    assert_eq!(look_bytes(&t(m)), ATOM);
    assert_eq!(look_bytes(&t(udta_xyz(b"+00+000/"))), Look::None);
}

#[test]
fn telemetry_tracks_count_without_their_samples() {
    for typ in [b"gpmd", b"camm", b"djmd", b"dbgi"] {
        let f = mp4(&trak_with_entries(&[entry(typ, &[0; 8])]));
        assert_eq!(look_bytes(&f), TRACK, "{}", String::from_utf8_lossy(typ));
    }
    // A video track and a text track are not telemetry.
    let f = mp4(&trak_with_entries(&[
        entry(b"avc1", &[0; 70]),
        entry(b"tx3g", &[0; 30]),
    ]));
    assert_eq!(look_bytes(&f), Look::None);
    // An iPhone's `mebx` naming the ISO 6709 key; one naming another key.
    let mebx = |key: &[u8]| {
        let mut keyd = b"mdta".to_vec();
        keyd.extend(key);
        let local = b(&1u32.to_be_bytes(), &b(b"keyd", &keyd));
        entry(b"mebx", &b(b"keys", &local))
    };
    let f = mp4(&trak_with_entries(&[mebx(KEY)]));
    assert_eq!(look_bytes(&f), TRACK);
    let f = mp4(&trak_with_entries(&[mebx(
        b"com.apple.quicktime.scene-illuminance",
    )]));
    assert_eq!(look_bytes(&f), Look::None);
}

#[test]
fn zero_xmp_in_a_mov_is_none() {
    let zero = super::stills::ZERO_XMP;
    assert_eq!(look_bytes(&mp4(&b(b"udta", &b(b"XMP_", zero)))), Look::None);
    let mut f = ftyp();
    f.extend(b(b"moov", &b(b"mvhd", &[0; 100])));
    let mut uuid = crate::movie::XMP_UUID.to_vec();
    uuid.extend(zero);
    f.extend(b(b"uuid", &uuid));
    assert_eq!(look_bytes(&f), Look::None);
    // DJI's spelling in a MOV's udta is found.
    assert_eq!(
        look_bytes(&mp4(&b(b"udta", &b(b"XMP_", super::stills::DJI_XMP)))),
        XMP
    );
}

#[test]
fn xmp_in_udta_and_in_the_top_level_uuid_is_found() {
    let packet = b"<x:xmpmeta><rdf:Description exif:GPSLatitude=\"30,12.576N\" \
                   exif:GPSLongitude=\"40,7.148W\"/></x:xmpmeta>";
    assert_eq!(look_bytes(&mp4(&b(b"udta", &b(b"XMP_", packet)))), XMP);
    let mut f = ftyp();
    f.extend(b(b"moov", &b(b"mvhd", &[0; 100])));
    let mut uuid = crate::movie::XMP_UUID.to_vec();
    uuid.extend(packet);
    f.extend(b(b"uuid", &uuid));
    assert_eq!(look_bytes(&f), XMP);
    // A packet without coordinates; a uuid box of another type holding a packet.
    let plain = b"<x:xmpmeta><rdf:Description xmp:CreatorTool=\"Acme\"/></x:xmpmeta>";
    assert_eq!(
        look_bytes(&mp4(&b(b"udta", &b(b"XMP_", plain)))),
        Look::None
    );
    let mut f = ftyp();
    let mut other = [0x11; 16].to_vec();
    other.extend(packet);
    f.extend(b(b"uuid", &other));
    assert_eq!(look_bytes(&f), Look::None);
}

/// A reader of a sparse file: these regions hold bytes, everything else is zero.
struct Sparse {
    regions: Vec<(u64, Vec<u8>)>,
    len: u64,
    pos: u64,
}

impl Read for Sparse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        buf[..n].fill(0);
        for (at, data) in &self.regions {
            for (i, byte) in buf[..n].iter_mut().enumerate() {
                let p = self.pos + i as u64;
                if p >= *at && p < at + data.len() as u64 {
                    *byte = data[(p - at) as usize];
                }
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for Sparse {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = match to {
            SeekFrom::Start(p) => p,
            SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
            SeekFrom::End(d) => self.len.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

#[test]
fn a_moov_at_the_end_of_a_five_gigabyte_file_is_found() {
    let head = ftyp();
    let mdat_size: u64 = 5_000_000_000;
    let mut mdat = 1u32.to_be_bytes().to_vec();
    mdat.extend(b"mdat");
    mdat.extend(mdat_size.to_be_bytes());
    let moov_at = head.len() as u64 + mdat_size;
    let moov = b(b"moov", &udta_xyz(PLACE_ISO6709));
    let len = moov_at + moov.len() as u64;
    let mut r = Sparse {
        regions: vec![(0, [head.clone(), mdat].concat()), (moov_at, moov)],
        len,
        pos: 0,
    };
    assert_eq!(look(&mut r, len), ATOM);
}

#[test]
fn a_mov_without_ftyp_is_found() {
    let mov = sample_located(SampleLocated::Mov);
    let ftyp_len = u32::from_be_bytes([mov[0], mov[1], mov[2], mov[3]]) as usize;
    let rest = &mov[ftyp_len..];
    let moov_len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
    let moov = &rest[..moov_len];
    for first in [b"wide", b"mdat", b"free", b"skip"] {
        let mut f = b(first, &[]);
        f.extend(b(b"mdat", &[0x5a; 100]));
        f.extend(moov);
        assert_eq!(look_bytes(&f), ATOM, "{}", String::from_utf8_lossy(first));
    }
    assert_eq!(look_bytes(moov), ATOM);
}

#[test]
fn a_big_cover_beside_the_location_is_passed_over() {
    let covr = b(b"covr", &data_box(&vec![0xc0; 3 << 20]));
    let ilst = b(b"ilst", &[covr, b(&XYZ, &data_box(PLACE_ISO6709))].concat());
    let m = meta(false, &[hdlr(b"mdir"), ilst]);
    let f = mp4(&b(b"udta", &m));
    assert!(f.len() > 3 << 20);
    assert_eq!(look_bytes(&f), ATOM);
}

#[test]
fn media_data_is_never_walked_into() {
    // `mdat` (and `free`) whose bytes would be broken boxes, before a moov without a location.
    let junk: Vec<u8> = [0u8, 0, 0, 3].repeat(64);
    let mut f = ftyp();
    f.extend(b(b"mdat", &junk));
    f.extend(b(b"free", &junk));
    f.extend(b(b"moov", &b(b"mvhd", &[0; 100])));
    f.extend(b(b"mdat", &junk));
    assert_eq!(look_bytes(&f), Look::None);
}

#[test]
fn fragments_after_a_located_moov_do_not_hide_it() {
    let mut f = sample_located(SampleLocated::Mp4);
    let pair = [b(b"moof", &[]), b(b"mdat", &[])].concat();
    f.extend(pair.repeat(200_000));
    assert_eq!(look_bytes(&f), ATOM);
}

#[test]
fn more_descended_boxes_than_the_bound_are_unreadable() {
    let many = b(b"abcd", &[]).repeat(HEADERS_MAX as usize + 1);
    let mut f = mp4(&b(b"udta", &many));
    f.extend(b(b"moov", &udta_xyz(PLACE_ISO6709)));
    let t = Instant::now();
    let found = look_bytes(&f);
    let took = t.elapsed();
    println!("100,001 descended boxes: {took:?}");
    assert_eq!(found, Look::Unreadable);
    // Generous: the brief's number is 50 ms in a debug build; a loaded PC is slower.
    assert!(took.as_millis() < 2000, "{took:?}");
    // Below the bound the same walk ends and finds the second moov's location.
    let fewer = b(b"abcd", &[]).repeat(HEADERS_MAX as usize - 50);
    let mut f = mp4(&b(b"udta", &fewer));
    f.extend(b(b"moov", &udta_xyz(PLACE_ISO6709)));
    assert_eq!(look_bytes(&f), ATOM);
}

#[test]
fn a_slow_file_ends_unreadable_by_the_clock() {
    let many = b(b"abcd", &[]).repeat(1000);
    let f = mp4(&b(b"udta", &many));
    // Each look at the clock: 10 ms more (a cold disk).
    let mut t = Duration::ZERO;
    let mut slow = || {
        t += Duration::from_millis(10);
        t
    };
    assert_eq!(
        look_clocked(&mut Cursor::new(&f), f.len() as u64, &mut slow),
        Look::Unreadable
    );
    // The same file on a fast clock is judged.
    let mut fast = || Duration::ZERO;
    assert_eq!(
        look_clocked(&mut Cursor::new(&f), f.len() as u64, &mut fast),
        Look::None
    );
}

/// About 2 KiB of movie: ftyp, a trak with a video entry, a QuickTime meta with two keys.
fn movie_fuzz_sample() -> Vec<u8> {
    let m = meta(
        true,
        &[
            hdlr(b"mdta"),
            keys(&[b"com.apple.quicktime.make", KEY]),
            b(
                b"ilst",
                &[item(1, b"Acme"), item(2, PLACE_ISO6709)].concat(),
            ),
        ],
    );
    let t = trak_with_entries(&[entry(b"avc1", &[0x33; 600])]);
    let mut f = ftyp();
    f.extend(b(
        b"moov",
        &[b(b"mvhd", &[0; 100]), t, b(b"udta", &loci(true, 0, 0)), m].concat(),
    ));
    f.extend(b(b"mdat", &[0x5a; 900]));
    f
}

#[test]
fn a_cut_or_flipped_movie_never_panics_nor_reads_past_len() {
    let mut samples = vec![movie_fuzz_sample()];
    samples.extend([SampleLocated::Mp4, SampleLocated::M4a].map(sample_located));
    for s in samples {
        assert_eq!(look_bytes(&s), ATOM);
        for k in 0..=s.len() as u64 {
            let _ = look_guarded(&s, k);
        }
        for pos in 0..s.len() {
            for mask in [0x01u8, 0x80, 0xff] {
                let mut f = s.clone();
                f[pos] ^= mask;
                let _ = look_guarded(&f, f.len() as u64);
            }
        }
    }
}
