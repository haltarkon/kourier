//! Tests of the box walker on boxes written here (no binary fixtures).

use super::*;
use crate::location::Slice;

/// A box with a 32-bit size.
fn bx(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    b.extend_from_slice(typ);
    b.extend_from_slice(payload);
    b
}

/// A box with a 64-bit size.
fn bx64(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = 1u32.to_be_bytes().to_vec();
    b.extend_from_slice(typ);
    b.extend_from_slice(&((payload.len() + 16) as u64).to_be_bytes());
    b.extend_from_slice(payload);
    b
}

/// Every header of `boxes` in `data`.
fn all(data: &[u8], mut boxes: Boxes) -> Result<Vec<BoxHeader>, Bound> {
    let mut src = Slice(data);
    let mut out = Vec::new();
    while let Some(b) = boxes.next(&mut src)? {
        out.push(b);
    }
    Ok(out)
}

fn types(data: &[u8], boxes: Boxes) -> Result<Vec<[u8; 4]>, Bound> {
    Ok(all(data, boxes)?.iter().map(|b| b.typ).collect())
}

fn top(data: &[u8]) -> Result<Vec<BoxHeader>, Bound> {
    all(data, Boxes::top(data.len() as u64))
}

#[test]
fn sizes_of_32_and_64_bits_and_zero_walk_the_top_level() {
    let mut f = bx(b"ftyp", b"jxl \0\0\0\0");
    f.extend(bx64(b"abcd", &[7; 20]));
    f.extend(bx(b"wxyz", &[]));
    let tail = f.len() as u64;
    // Size 0: to the end of the container.
    f.extend_from_slice(&0u32.to_be_bytes());
    f.extend_from_slice(b"mdat");
    f.extend_from_slice(&[9; 33]);
    let h = top(&f).unwrap();
    let t: Vec<_> = h.iter().map(|b| b.typ).collect();
    assert_eq!(t, [*b"ftyp", *b"abcd", *b"wxyz", *b"mdat"]);
    assert_eq!((h[1].payload - h[1].start, h[1].payload_len()), (16, 20));
    assert_eq!(h[2].payload_len(), 0);
    assert_eq!(
        (h[3].start, h[3].payload, h[3].end),
        (tail, tail + 8, f.len() as u64)
    );
    assert!(h.iter().all(|b| b.depth == 0 && b.uuid.is_none()));
}

#[test]
fn a_uuid_box_carries_its_extended_type() {
    let id = [0xbe; 16];
    let mut p = id.to_vec();
    p.extend_from_slice(b"<x:xmpmeta/>");
    let f = bx(b"uuid", &p);
    let h = top(&f).unwrap();
    assert_eq!(h[0].uuid, Some(id));
    assert_eq!((h[0].payload, h[0].payload_len()), (24, 12));
    // A uuid box too short for its extended type.
    let mut short = 20u32.to_be_bytes().to_vec();
    short.extend_from_slice(b"uuid");
    short.extend_from_slice(&[0; 16]);
    assert_eq!(top(&short), Err(Bound));
}

#[test]
fn sizes_of_two_to_seven_are_refused_without_a_loop() {
    for size in 2u32..8 {
        let mut f = bx(b"free", &[0; 4]);
        f.extend_from_slice(&size.to_be_bytes());
        f.extend_from_slice(b"abcd");
        f.extend_from_slice(&[0; 32]);
        assert_eq!(top(&f), Err(Bound), "size {size}");
    }
    // A 64-bit size below its 16-byte header.
    for large in [0u64, 8, 15] {
        let mut f = 1u32.to_be_bytes().to_vec();
        f.extend_from_slice(b"abcd");
        f.extend_from_slice(&large.to_be_bytes());
        f.extend_from_slice(&[0; 32]);
        assert_eq!(top(&f), Err(Bound), "large size {large}");
    }
}

#[test]
fn a_size_that_overflows_is_refused_and_one_past_its_container_is_cut() {
    let mut f = bx(b"free", &[]);
    f.extend_from_slice(&1u32.to_be_bytes());
    f.extend_from_slice(b"mdat");
    f.extend_from_slice(&u64::MAX.to_be_bytes());
    assert_eq!(top(&f), Err(Bound), "8 + u64::MAX overflows");

    // A recording cut short: its mdat claims more than the file has.
    let mut f = bx(b"moov", &bx(b"udta", &[1; 12]));
    f.extend_from_slice(&5000u32.to_be_bytes());
    f.extend_from_slice(b"mdat");
    f.extend_from_slice(&[0; 100]);
    let h = top(&f).unwrap();
    assert_eq!(h[1].end, f.len() as u64);
    // A child past its parent ends at the parent's end.
    let mut inner = 4000u32.to_be_bytes().to_vec();
    inner.extend_from_slice(b"udta");
    inner.extend_from_slice(&[0; 8]);
    let f = bx(b"moov", &inner);
    let moov = top(&f).unwrap()[0];
    let kids = all(&f, moov.children().unwrap()).unwrap();
    assert_eq!((kids[0].typ, kids[0].end), (*b"udta", moov.end));
    // A header cut by its container's end.
    let f = bx(b"moov", &[0, 0, 0, 1, b'a', b'b', b'c', b'd', 0, 0]);
    let moov = top(&f).unwrap()[0];
    assert_eq!(all(&f, moov.children().unwrap()), Err(Bound));
}

#[test]
fn fewer_than_eight_bytes_end_a_container() {
    // QuickTime ends some `udta` with a 32-bit zero.
    let mut p = bx(b"\xa9xyz", b"\0\x0a\0\0+30.1-040.2/");
    p.extend_from_slice(&[0; 4]);
    let f = bx(b"udta", &p);
    let udta = top(&f).unwrap()[0];
    assert_eq!(types(&f, udta.children().unwrap()), Ok(vec![*b"\xa9xyz"]));
}

#[test]
fn nesting_deeper_than_the_bound_is_refused() {
    // 13 boxes inside each other: depths 0..=12.
    let mut f = bx(b"leaf", &[]);
    for _ in 0..12 {
        f = bx(b"nest", &f);
    }
    let mut src = Slice(&f);
    let mut b = Boxes::top(f.len() as u64).next(&mut src).unwrap().unwrap();
    for depth in 1..=DEPTH_MAX {
        b = b.children().unwrap().next(&mut src).unwrap().unwrap();
        assert_eq!(b.depth, depth);
    }
    assert_eq!(b.typ, *b"leaf");
    assert_eq!(b.children().map(|_| ()), Err(Bound));
}

/// QuickTime's `moov/meta`: the children follow the header (no version and flags).
fn quicktime_meta() -> Vec<u8> {
    let mut p = bx(b"hdlr", &[0; 25]);
    p.extend(bx(b"keys", &[0; 8]));
    p.extend(bx(b"ilst", &[]));
    bx(b"meta", &p)
}

/// ISO's `meta` (HEIF, iTunes `udta/meta`): a FullBox.
fn iso_meta() -> Vec<u8> {
    let mut p = vec![0, 0, 0, 0];
    p.extend(bx(b"hdlr", &[0; 25]));
    p.extend(bx(b"iinf", &[0; 6]));
    bx(b"meta", &p)
}

#[test]
fn a_quicktime_meta_has_no_version_and_flags() {
    let f = quicktime_meta();
    let meta = top(&f).unwrap()[0];
    let kids = meta.meta_children(&mut Slice(&f)).unwrap();
    assert_eq!(types(&f, kids), Ok(vec![*b"hdlr", *b"keys", *b"ilst"]));
}

#[test]
fn an_iso_meta_is_a_full_box() {
    let f = iso_meta();
    let meta = top(&f).unwrap()[0];
    let kids = meta.meta_children(&mut Slice(&f)).unwrap();
    assert_eq!(types(&f, kids), Ok(vec![*b"hdlr", *b"iinf"]));
    let iinf = all(&f, kids).unwrap()[1];
    assert_eq!(iinf.depth, 1);
    let mut v = bx(b"iloc", &[2, 0, 0, 7]);
    v.extend(bx(b"tiny", &[1, 2]));
    let h = top(&v).unwrap();
    assert_eq!(h[0].full(&mut Slice(&v)), Ok((2, 7)));
    assert_eq!(h[1].full(&mut Slice(&v)), Err(Bound));
}

#[test]
fn skipped_boxes_are_not_counted_as_headers() {
    assert!(SKIPPED.contains(b"mdat") && SKIPPED.contains(b"moof"));
    assert!(SKIPPED.contains(b"free") && SKIPPED.contains(b"skip"));
    assert!(!SKIPPED.contains(b"moov") && !SKIPPED.contains(b"Exif"));
}
