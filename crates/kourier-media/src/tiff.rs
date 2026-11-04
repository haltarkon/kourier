//! A bounded TIFF IFD walker: the GPS IFD (is a location there?) and the orientation tag. Used
//! for EXIF blobs (JPEG APP1, and the containers of the later steps) and for files that are a
//! TIFF (TIFF, DNG, RW2, ORF). Offsets count from the start of the TIFF header, which is offset 0
//! of the source it is given. Never panics; every bound of [`crate::location`] holds.

use crate::location::{Bound, Bytes, IFD_ENTRIES_MAX, IFDS_MAX, Slice};

/// IFD0's pointer to the GPS IFD.
const GPS_IFD: u16 = 0x8825;
/// The orientation tag of IFD0.
const ORIENTATION: u16 = 0x0112;
/// GPSLatitude and GPSLongitude in the GPS IFD.
const GPS_LATITUDE: u16 = 0x0002;
const GPS_LONGITUDE: u16 = 0x0004;

/// A value larger than this is not read (a latitude is 24 bytes).
const VALUE_MAX: u64 = 4096;

/// Whether `head` (at least 4 bytes) starts a TIFF-shaped file: TIFF `II*\0` / `MM\0*`, Panasonic
/// RW2 `IIU\0`, Olympus ORF `IIRO` / `IIRS`.
pub(crate) fn is_tiff_magic(head: &[u8]) -> bool {
    matches!(
        head.get(..4),
        Some(b"II*\0" | b"MM\0*" | b"IIU\0" | b"IIRO" | b"IIRS")
    )
}

/// Whether the TIFF blob `blob` holds a GPS IFD with a location (the rule of
/// [`crate::location`]).
pub(crate) fn gps_present(blob: &[u8]) -> Result<bool, Bound> {
    gps_in(&mut Slice(blob))
}

/// The EXIF orientation (tag 0x0112 of IFD0) of the TIFF blob `blob`, or `None` when it has
/// none or cannot be read.
pub(crate) fn orientation(blob: &[u8]) -> Option<u16> {
    let mut src = Slice(blob);
    let order = header(&mut src).ok()?;
    let ifd0 = order.u32(&src.read(4, 4).ok()?);
    let entries = entries(&mut src, order, u64::from(ifd0)).ok()?;
    let e = entries.iter().find(|e| e.tag == ORIENTATION)?;
    match e.typ {
        3 if e.count >= 1 => Some(order.u16(&e.value[..2])),
        _ => None,
    }
}

/// [`gps_present`] over any source: IFD0 and the chain of IFDs after it, each IFD's GPS pointer
/// followed.
pub(crate) fn gps_in(src: &mut dyn Bytes) -> Result<bool, Bound> {
    let order = header(src)?;
    let mut next = u64::from(order.u32(&src.read(4, 4)?));
    let mut visited: Vec<u64> = Vec::new();
    while next != 0 {
        if visited.contains(&next) {
            // A loop: that IFD was seen already.
            break;
        }
        if visited.len() >= IFDS_MAX {
            return Err(Bound);
        }
        visited.push(next);
        let ifd = entries(src, order, next)?;
        if let Some(gps) = ifd.iter().find(|e| e.tag == GPS_IFD) {
            let at = u64::from(match gps.typ {
                3 => u32::from(order.u16(&gps.value[..2])),
                _ => order.u32(&gps.value),
            });
            if !visited.contains(&at) {
                if visited.len() >= IFDS_MAX {
                    return Err(Bound);
                }
                visited.push(at);
                if gps_ifd_located(src, order, at)? {
                    return Ok(true);
                }
            }
        }
        next = u64::from(order.u32(&src.read(next + 2 + 12 * ifd.len() as u64, 4)?));
    }
    Ok(false)
}

/// Whether the GPS IFD at `at` has a latitude and a longitude that are not both zero.
fn gps_ifd_located(src: &mut dyn Bytes, order: Order, at: u64) -> Result<bool, Bound> {
    let ifd = entries(src, order, at)?;
    let lat = ifd.iter().find(|e| e.tag == GPS_LATITUDE);
    let lon = ifd.iter().find(|e| e.tag == GPS_LONGITUDE);
    let (Some(lat), Some(lon)) = (lat, lon) else {
        return Ok(false);
    };
    Ok(!(is_zero(src, order, lat)? && is_zero(src, order, lon)?))
}

/// Whether an entry's value is zero: no value, every rational with numerator 0 or denominator 0,
/// or (other types) every byte 0.
fn is_zero(src: &mut dyn Bytes, order: Order, e: &Entry) -> Result<bool, Bound> {
    let unit = unit_size(e.typ);
    let size = u64::from(e.count) * unit;
    if size > VALUE_MAX {
        return Err(Bound);
    }
    let value = if size <= 4 {
        e.value[..size as usize].to_vec()
    } else {
        src.read(u64::from(order.u32(&e.value)), size as usize)?
    };
    Ok(match e.typ {
        // RATIONAL, SRATIONAL: num / den, each 32 bits.
        5 | 10 => value
            .as_chunks::<8>()
            .0
            .iter()
            .all(|r| order.u32(&r[..4]) == 0 || order.u32(&r[4..]) == 0),
        _ => value.iter().all(|&b| b == 0),
    })
}

/// The size of one value of a TIFF field type (unknown types count as bytes).
fn unit_size(typ: u16) -> u64 {
    match typ {
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    }
}

/// One IFD entry: the value field is kept raw (4 bytes, the value itself or its offset).
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    value: [u8; 4],
}

/// The entries of the IFD at `at`.
fn entries(src: &mut dyn Bytes, order: Order, at: u64) -> Result<Vec<Entry>, Bound> {
    let n = order.u16(&src.read(at, 2)?);
    if n > IFD_ENTRIES_MAX {
        return Err(Bound);
    }
    let raw = src.read(at + 2, usize::from(n) * 12)?;
    Ok(raw
        .as_chunks::<12>()
        .0
        .iter()
        .map(|e| Entry {
            tag: order.u16(&e[..2]),
            typ: order.u16(&e[2..4]),
            count: order.u32(&e[4..8]),
            value: [e[8], e[9], e[10], e[11]],
        })
        .collect())
}

/// The byte order of the TIFF header at offset 0 (any magic of [`is_tiff_magic`]).
fn header(src: &mut dyn Bytes) -> Result<Order, Bound> {
    let h = src.read(0, 4)?;
    if !is_tiff_magic(&h) {
        return Err(Bound);
    }
    Ok(if h[0] == b'I' { Order::Le } else { Order::Be })
}

#[derive(Clone, Copy)]
enum Order {
    Le,
    Be,
}

impl Order {
    /// `b` holds at least 2 bytes.
    fn u16(self, b: &[u8]) -> u16 {
        let b = [b[0], b[1]];
        match self {
            Self::Le => u16::from_le_bytes(b),
            Self::Be => u16::from_be_bytes(b),
        }
    }

    /// `b` holds at least 4 bytes.
    fn u32(self, b: &[u8]) -> u32 {
        let b = [b[0], b[1], b[2], b[3]];
        match self {
            Self::Le => u32::from_le_bytes(b),
            Self::Be => u32::from_be_bytes(b),
        }
    }
}
