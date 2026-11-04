//! The places an ISO-BMFF movie (MP4, MOV, 3GP, M4A) keeps where it was recorded, read from the
//! `moov` box by headers and small leaf values (critic F12: nothing but a leaf of at most
//! [`LEAF_MAX`] bytes, an XMP packet or a key table is read; `mdat`, `covr` and the sample tables
//! are never read).
//!
//! Places: `udta/©xyz` (ISO 6709 text: Android's `MediaMuxer`, QuickTime), `udta/loci` (3GPP TS
//! 26.244), `udta/XMP_` (Adobe), QuickTime's `meta` with `keys` naming
//! `com.apple.quicktime.location.ISO6709` and its `ilst` item (1-based key index), the iTunes
//! `udta/meta/ilst/©xyz`; the same under each `trak` (its `udta` and `meta`); and the timed
//! metadata tracks, whose sample entries in `trak/mdia/minf/stbl/stsd` are `gpmd` (GoPro), `camm`
//! (Google's camera motion metadata), `djmd`/`dbgi` (DJI), or a `mebx` whose key table names the
//! ISO 6709 key (iPhone): a track counts as a location without its samples being read (BQ6).
//! The top-level XMP `uuid` box is read by the caller ([`XMP_UUID`]).
//!
//! The zero rule (critic F20): an ISO 6709 latitude and longitude that are both zero, and a
//! `loci` whose latitude and longitude are both 0, are placeholders (None). A Samsung trailer
//! after the last box is a named gap.

use crate::bmff::{BoxHeader, Boxes};
use crate::location::{Bound, Bytes, EvidenceKind, xmp_box};

/// The extended type of the top-level XMP `uuid` box (BE7ACFCB-97A9-42E8-9C71-999491E3AFAC).
pub(crate) const XMP_UUID: [u8; 16] = [
    0xbe, 0x7a, 0xcf, 0xcb, 0x97, 0xa9, 0x42, 0xe8, 0x9c, 0x71, 0x99, 0x94, 0x91, 0xe3, 0xaf, 0xac,
];

/// At most this much of a leaf value (a `©xyz` text, a `loci`, a `data` item, a key) is read.
pub(crate) const LEAF_MAX: u64 = 4096;

/// QuickTime's key of the recording's location.
const ISO6709_KEY: &[u8] = b"com.apple.quicktime.location.ISO6709";

/// Sample entries of tracks that carry positions.
const TELEMETRY: [[u8; 4]; 4] = [*b"gpmd", *b"camm", *b"djmd", *b"dbgi"];

/// `©xyz`.
const XYZ: [u8; 4] = [0xa9, b'x', b'y', b'z'];

/// What a place answers: the kind of proof, nothing, or [`Bound`].
type Place = Result<Option<EvidenceKind>, Bound>;

/// Walks `boxes`, giving each to `f`: the first Found wins; a broken child is remembered and the
/// walk goes on with its sibling (Found later still wins), a broken walk of `boxes` itself ends it.
fn each(
    src: &mut dyn Bytes,
    mut boxes: Boxes,
    mut f: impl FnMut(&mut dyn Bytes, &BoxHeader) -> Place,
) -> Place {
    let mut broken = false;
    loop {
        let b = match boxes.next(src) {
            Ok(Some(b)) => b,
            Ok(None) => break,
            Err(Bound) => {
                broken = true;
                break;
            }
        };
        match f(src, &b) {
            Ok(Some(kind)) => return Ok(Some(kind)),
            Ok(None) => {}
            Err(Bound) => broken = true,
        }
    }
    if broken { Err(Bound) } else { Ok(None) }
}

/// The `moov` box: its `udta`, `meta` and each `trak`.
pub(crate) fn moov(src: &mut dyn Bytes, moov: &BoxHeader) -> Place {
    each(src, moov.children()?, |src, b| match &b.typ {
        b"udta" => udta(src, b),
        b"meta" => meta(src, b),
        b"trak" => trak(src, b),
        _ => Ok(None),
    })
}

/// A track: its `udta` and `meta`, and its sample entries (`mdia/minf/stbl/stsd`).
fn trak(src: &mut dyn Bytes, trak: &BoxHeader) -> Place {
    each(src, trak.children()?, |src, b| match &b.typ {
        b"udta" => udta(src, b),
        b"meta" => meta(src, b),
        b"mdia" => each(src, b.children()?, |src, b| match &b.typ {
            b"minf" => each(src, b.children()?, |src, b| match &b.typ {
                b"stbl" => each(src, b.children()?, |src, b| match &b.typ {
                    b"stsd" => stsd(src, b),
                    _ => Ok(None),
                }),
                _ => Ok(None),
            }),
            _ => Ok(None),
        }),
        _ => Ok(None),
    })
}

/// User data: `©xyz`, `loci`, `XMP_` and an iTunes `meta`.
fn udta(src: &mut dyn Bytes, udta: &BoxHeader) -> Place {
    each(src, udta.children()?, |src, b| match &b.typ {
        t if *t == XYZ => {
            // A QuickTime text: a 16-bit size, a 16-bit language, the text.
            let v = leaf(src, b)?;
            let size = usize::from(u16::from_be_bytes([
                *v.first().ok_or(Bound)?,
                *v.get(1).ok_or(Bound)?,
            ]));
            let text = v.get(4..).ok_or(Bound)?;
            Ok(iso6709(&text[..size.min(text.len())]).then_some(EvidenceKind::MovieAtom))
        }
        b"loci" => loci(&leaf(src, b)?).map(|f| f.then_some(EvidenceKind::MovieAtom)),
        b"XMP_" => Ok(xmp_box(src, b)?.then_some(EvidenceKind::Xmp)),
        b"meta" => meta(src, b),
        _ => Ok(None),
    })
}

/// A `meta` box, QuickTime's (`hdlr`, `keys`, `ilst` right after the header) or ISO's (a FullBox;
/// iTunes `udta/meta`): the `ilst` item named by the ISO 6709 key, and an iTunes `©xyz` item.
fn meta(src: &mut dyn Bytes, meta: &BoxHeader) -> Place {
    let (mut keys, mut ilst) = (None, None);
    let mut children = meta.meta_children(src)?;
    while let Some(b) = children.next(src)? {
        match &b.typ {
            b"keys" => {
                keys.get_or_insert(b);
            }
            b"ilst" => {
                ilst.get_or_insert(b);
            }
            _ => {}
        }
    }
    let Some(ilst) = ilst else {
        return Ok(None);
    };
    let index = match keys {
        Some(k) => key_index(src, &k)?,
        None => None,
    };
    each(src, ilst.children()?, |src, item| {
        let named = index.is_some_and(|i| u32::from_be_bytes(item.typ) == i);
        if !named && item.typ != XYZ {
            // Every other item (`covr` and its pictures among them) is passed over by its header.
            return Ok(None);
        }
        each(src, item.children()?, |src, d| {
            if d.typ != *b"data" {
                return Ok(None);
            }
            // A type indicator and a locale, then the value.
            let v = leaf(src, d)?;
            let value = v.get(8..).ok_or(Bound)?;
            Ok(iso6709(value).then_some(EvidenceKind::MovieAtom))
        })
    })
}

/// The 1-based index of the ISO 6709 key in a QuickTime `keys` box (a FullBox: an entry count,
/// then entries of a 32-bit size, a namespace and the key), or `None`.
fn key_index(src: &mut dyn Bytes, keys: &BoxHeader) -> Result<Option<u32>, Bound> {
    keys.full(src)?;
    let end = keys.end;
    let mut pos = keys.payload + 8;
    if pos > end {
        return Err(Bound);
    }
    let count = {
        let c = src.read(keys.payload + 4, 4)?;
        u32::from_be_bytes([c[0], c[1], c[2], c[3]])
    };
    for index in 1..=count {
        if end - pos < 8 {
            return Err(Bound);
        }
        src.count_header()?;
        let e = src.read(pos, 8)?;
        let size = u64::from(u32::from_be_bytes([e[0], e[1], e[2], e[3]]));
        if size < 8 || size > end - pos {
            return Err(Bound);
        }
        if size - 8 == ISO6709_KEY.len() as u64
            && src.read(pos + 8, ISO6709_KEY.len())? == ISO6709_KEY
        {
            return Ok(Some(index));
        }
        pos += size;
    }
    Ok(None)
}

/// A track's sample descriptions (a FullBox: an entry count, then one box per entry): a
/// telemetry entry, or a `mebx` whose key table names the ISO 6709 key.
fn stsd(src: &mut dyn Bytes, stsd: &BoxHeader) -> Place {
    each(src, stsd.children_after(8)?, |src, entry| {
        if TELEMETRY.contains(&entry.typ) {
            return Ok(Some(EvidenceKind::Track));
        }
        if entry.typ != *b"mebx" {
            return Ok(None);
        }
        // A sample entry: 6 reserved bytes and a data reference index, then boxes; the `keys`
        // box holds one box per local key, each with a `keyd` (a namespace, then the key).
        each(src, entry.children_after(8)?, |src, keys| {
            if keys.typ != *b"keys" {
                return Ok(None);
            }
            each(src, keys.children()?, |src, key| {
                each(src, key.children()?, |src, d| {
                    if d.typ != *b"keyd" {
                        return Ok(None);
                    }
                    let v = leaf(src, d)?;
                    Ok((v.get(4..) == Some(ISO6709_KEY)).then_some(EvidenceKind::Track))
                })
            })
        })
    })
}

/// The first [`LEAF_MAX`] bytes of a box's payload.
fn leaf(src: &mut dyn Bytes, b: &BoxHeader) -> Result<Vec<u8>, Bound> {
    let n = usize::try_from(b.payload_len().min(LEAF_MAX)).map_err(|_| Bound)?;
    src.read(b.payload, n)
}

/// Whether an ISO 6709 text (`+30.2000-040.1200/`, an altitude and a CRS may follow) names a
/// latitude and a longitude that are not both zero. Only the digits are looked at, never a value.
pub(crate) fn iso6709(text: &[u8]) -> bool {
    let end = text
        .iter()
        .position(|&c| c == b'/' || c == 0)
        .unwrap_or(text.len());
    let text = &text[..end];
    if !text.first().is_some_and(|c| matches!(c, b'+' | b'-')) {
        return false;
    }
    let mut parts = text[1..].split(|&c| c == b'+' || c == b'-');
    let (Some(lat), Some(lon)) = (parts.next(), parts.next()) else {
        return false;
    };
    let number = |p: &[u8]| {
        p.iter().any(u8::is_ascii_digit) && p.iter().all(|c| c.is_ascii_digit() || *c == b'.')
    };
    let nonzero = |p: &[u8]| p.iter().any(|c| (b'1'..=b'9').contains(c));
    number(lat) && number(lon) && (nonzero(lat) || nonzero(lon))
}

/// Whether a `loci` payload (3GPP TS 26.244: version and flags, a language, a name, a role, then
/// a 16.16 longitude and latitude) holds a place that is not 0, 0.
fn loci(v: &[u8]) -> Result<bool, Bound> {
    let name = v.get(6..).ok_or(Bound)?;
    let name_len = if name.starts_with(&[0xfe, 0xff]) || name.starts_with(&[0xff, 0xfe]) {
        // UTF-16 with its byte-order mark, ended by a 16-bit zero.
        name.as_chunks::<2>()
            .0
            .iter()
            .position(|c| *c == [0, 0])
            .map(|i| i * 2 + 2)
    } else {
        name.iter().position(|&c| c == 0).map(|i| i + 1)
    }
    .ok_or(Bound)?;
    // The role byte, then longitude and latitude.
    let at = 6 + name_len + 1;
    let coords = v.get(at..at + 8).ok_or(Bound)?;
    Ok(coords.iter().any(|&b| b != 0))
}
