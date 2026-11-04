//! The items of a HEIF file (HEIC, AVIF; ISO/IEC 23008-12 on ISO-BMFF): the top-level `meta`
//! box names its items in `iinf` (one `infe` each), says where their bytes are in `iloc`, and may
//! hold them in `idat`. The look reads two kinds of item: `Exif` (an EXIF payload: a 32-bit
//! offset, then TIFF) and `mime` items of type `application/rdf+xml` (XMP).
//!
//! Read: `iinf` v0 (16-bit entry count) and v1 (32-bit); `infe` v2 (16-bit item ids) and v3
//! (32-bit), the older versions have no item type and are passed over; `iloc` v0-v2 with
//! offset, length and base-offset sizes of 0, 4 or 8 bytes, `index_size` (v1, v2), construction
//! method 0 (file offsets) and 1 (offsets into `idat`), several extents, an extent length of 0
//! (to the end of its data). Refused ([`Bound`]): construction method 2 (offsets into another
//! item), data in another file (a data reference other than 0), a field size other than 0, 4
//! or 8, an item the look reads without a location, more than [`PAYLOAD_MAX`] for one item, and
//! (the parse is bounded, not only the reads: a size of 0 makes an extent cost no byte) more than
//! 64 extents in one entry, more than 4096 in the whole `iloc`, and a second entry for an item.

use crate::bmff::BoxHeader;
use crate::location::{Bound, Bytes, PAYLOAD_MAX};

/// The kind of item the look reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ItemKind {
    /// An `Exif` item.
    Exif,
    /// A `mime` item of type `application/rdf+xml`.
    Xmp,
}

/// At most this much of an `infe` is read: the item type and the content type come first.
const INFE_READ_MAX: u64 = 4096;
/// At most this many items of the kinds the look reads, per file.
const ITEMS_MAX: usize = 64;
/// At most this many extents in one `iloc` entry (an item has one to a few).
const ITEM_EXTENTS_MAX: u64 = 64;
/// At most this many extents in the whole `iloc`, the entries of other items included. With
/// field sizes of 0 an extent costs no byte, so the file's size does not bound the loop (LOC PR1).
const EXTENTS_MAX: u64 = 4096;

/// One item the look reads and its bytes (or [`Bound`] when they cannot be had).
pub(crate) type Item = (ItemKind, Result<Vec<u8>, Bound>);

/// The `Exif` and XMP items of the HEIF `meta` box `meta` (a top-level box of `src`, whose
/// offsets are file offsets), in the order `iinf` names them. A `meta` without `iinf` has none.
pub(crate) fn meta_items(src: &mut dyn Bytes, meta: &BoxHeader) -> Result<Vec<Item>, Bound> {
    let (mut iinf, mut iloc, mut idat) = (None, None, None);
    let mut children = meta.meta_children(src)?;
    while let Some(b) = children.next(src)? {
        let slot = match &b.typ {
            b"iinf" => &mut iinf,
            b"iloc" => &mut iloc,
            b"idat" => &mut idat,
            _ => continue,
        };
        // The first of each counts (a file has one).
        slot.get_or_insert(b);
    }
    let Some(iinf) = iinf else {
        return Ok(Vec::new());
    };
    let wanted = item_infos(src, &iinf)?;
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let iloc = iloc.ok_or(Bound)?;
    let locations = item_locations(src, &iloc, &wanted)?;
    let mut out = Vec::with_capacity(wanted.len());
    for (id, kind) in wanted {
        let bytes = match locations.iter().find(|l| l.id == id) {
            Some(l) => item_bytes(src, l, idat.as_ref()),
            None => Err(Bound),
        };
        out.push((kind, bytes));
    }
    Ok(out)
}

/// The ids and kinds of the items `iinf` names that the look reads.
fn item_infos(src: &mut dyn Bytes, iinf: &BoxHeader) -> Result<Vec<(u32, ItemKind)>, Bound> {
    let (version, _) = iinf.full(src)?;
    let count_size = if version == 0 { 2 } else { 4 };
    let mut entries = iinf.children_after(4 + count_size)?;
    let mut wanted = Vec::new();
    while let Some(b) = entries.next(src)? {
        if b.typ != *b"infe" {
            continue;
        }
        if let Some(item) = item_info(src, &b)? {
            if wanted.len() == ITEMS_MAX {
                return Err(Bound);
            }
            wanted.push(item);
        }
    }
    Ok(wanted)
}

/// One `infe`: its id and kind when it is an item the look reads.
fn item_info(src: &mut dyn Bytes, infe: &BoxHeader) -> Result<Option<(u32, ItemKind)>, Bound> {
    let (version, _) = infe.full(src)?;
    let id_size = match version {
        2 => 2,
        3 => 4,
        // Versions 0 and 1 have no item type; later ones are unknown.
        _ => return Ok(None),
    };
    let n = infe.payload_len().min(INFE_READ_MAX) as usize;
    let p = infe.window(src).read(0, n)?;
    let mut r = Rd { b: &p, at: 4 };
    let id = r.uint(id_size)?;
    r.uint(2)?; // item_protection_index
    let typ = r.take(4)?;
    if typ == b"Exif" {
        return Ok(Some((id as u32, ItemKind::Exif)));
    }
    if typ != b"mime" {
        return Ok(None);
    }
    // item_name, then content_type, each NUL-terminated (a name past the read: not XMP).
    let (Some(_), Some(content_type)) = (r.cstr(), r.cstr()) else {
        return Ok(None);
    };
    Ok(content_type
        .eq_ignore_ascii_case(b"application/rdf+xml")
        .then_some((id as u32, ItemKind::Xmp)))
}

/// Where one item's bytes are.
#[derive(Debug)]
struct Location {
    id: u32,
    method: u16,
    data_ref: u16,
    base: u64,
    /// (offset, length) per extent; a length of 0 reaches the end of the data.
    extents: Vec<(u64, u64)>,
}

/// The `iloc` entries of the `wanted` items.
fn item_locations(
    src: &mut dyn Bytes,
    iloc: &BoxHeader,
    wanted: &[(u32, ItemKind)],
) -> Result<Vec<Location>, Bound> {
    let len = iloc.payload_len();
    if len > PAYLOAD_MAX {
        return Err(Bound);
    }
    let p = iloc.window(src).read(0, len as usize)?;
    let mut r = Rd { b: &p, at: 0 };
    let version = r.uint(1)?;
    if version > 2 {
        return Err(Bound);
    }
    r.uint(3)?; // flags
    let sizes = r.uint(2)?;
    let offset_size = field_size(sizes >> 12)?;
    let length_size = field_size((sizes >> 8) & 0xf)?;
    let base_size = field_size((sizes >> 4) & 0xf)?;
    let index_size = if version == 0 {
        0
    } else {
        field_size(sizes & 0xf)?
    };
    let id_size = if version == 2 { 4 } else { 2 };
    let count = r.uint(id_size)?;
    let mut out: Vec<Location> = Vec::new();
    let mut extents_in_all = 0u64;
    for _ in 0..count {
        let id = r.uint(id_size)? as u32;
        let method = if version == 0 {
            0
        } else {
            (r.uint(2)? & 0xf) as u16
        };
        let data_ref = r.uint(2)? as u16;
        let base = r.uint(base_size)?;
        let extent_count = r.uint(2)?;
        // The caps come before the loop: an extent can cost 0 bytes (sizes of 0), so the bytes of
        // the box do not bound it (PR1).
        extents_in_all += extent_count;
        if extent_count > ITEM_EXTENTS_MAX || extents_in_all > EXTENTS_MAX {
            return Err(Bound);
        }
        let keep = wanted.iter().any(|&(w, _)| w == id);
        // A second entry for an item is malformed (and would repeat its extents).
        if keep && out.iter().any(|l| l.id == id) {
            return Err(Bound);
        }
        let mut extents = Vec::new();
        for _ in 0..extent_count {
            r.uint(index_size)?; // extent_index (construction method 2 only)
            let offset = r.uint(offset_size)?;
            let length = r.uint(length_size)?;
            if keep {
                extents.push((offset, length));
            }
        }
        if keep {
            out.push(Location {
                id,
                method,
                data_ref,
                base,
                extents,
            });
        }
    }
    Ok(out)
}

/// A field size of `iloc`: 0, 4 or 8 bytes.
fn field_size(v: u64) -> Result<usize, Bound> {
    match v {
        0 => Ok(0),
        4 => Ok(4),
        8 => Ok(8),
        _ => Err(Bound),
    }
}

/// The bytes of one item: its extents one after the other.
fn item_bytes(
    src: &mut dyn Bytes,
    loc: &Location,
    idat: Option<&BoxHeader>,
) -> Result<Vec<u8>, Bound> {
    match (loc.method, loc.data_ref) {
        // Offsets into the file itself.
        (0, 0) => gather(src, loc),
        // Offsets into `idat`.
        (1, _) => gather(&mut idat.ok_or(Bound)?.window(src), loc),
        // Offsets into another item, or data in another file.
        _ => Err(Bound),
    }
}

/// Reads the extents of `loc` from `data`, at most [`PAYLOAD_MAX`] in all.
fn gather(data: &mut dyn Bytes, loc: &Location) -> Result<Vec<u8>, Bound> {
    let mut out = Vec::new();
    for &(offset, length) in &loc.extents {
        let at = loc.base.checked_add(offset).ok_or(Bound)?;
        let n = if length == 0 {
            data.len().checked_sub(at).ok_or(Bound)?
        } else {
            length
        };
        if (out.len() as u64).saturating_add(n) > PAYLOAD_MAX {
            return Err(Bound);
        }
        // An empty extent (one from the data's end to its end) brings nothing: no read (LOC RF1).
        if n == 0 {
            continue;
        }
        out.extend(data.read(at, n as usize)?);
    }
    Ok(out)
}

/// A big-endian reader over bytes in memory.
struct Rd<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Bound> {
        let end = self.at.checked_add(n).ok_or(Bound)?;
        let s = self.b.get(self.at..end).ok_or(Bound)?;
        self.at = end;
        Ok(s)
    }

    /// An unsigned big-endian field of `n` (0 to 8) bytes; 0 bytes read as 0.
    fn uint(&mut self, n: usize) -> Result<u64, Bound> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0, |v, &b| (v << 8) | u64::from(b)))
    }

    /// A NUL-terminated string (without the NUL), or `None` when no NUL follows.
    fn cstr(&mut self) -> Option<&'a [u8]> {
        let rest = self.b.get(self.at..)?;
        let n = rest.iter().position(|&b| b == 0)?;
        self.at += n + 1;
        Some(&rest[..n])
    }
}
