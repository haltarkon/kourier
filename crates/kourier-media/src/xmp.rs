//! The XMP search of the look (LOC): a bounded byte search, no XML parser.
//!
//! A packet names a location when it holds a latitude AND a longitude with a value, and not both
//! values are zero (the one zero rule, critic F20: `0,0.0N` is "GPS off"). Names are matched
//! case-insensitively: Adobe's `exif:GPSLatitude` / `exif:GPSLongitude` and DJI's
//! `drone-dji:GpsLatitude` / `GpsLongitude` / `GpsLongtitude` (sic). A value is an attribute
//! (`name="…"` or `name='…'`) or an element's text (`<name>…</name>`); a closing tag, a `…Ref`
//! name or a name without a value does not count. UTF-16 packets (either byte order, with or
//! without a byte-order mark) are narrowed to their ASCII first (critic F19). Values are never
//! turned into numbers: a value is zero when it holds no digit other than `0`, and a value
//! without any digit is no value. A value is short: one that does not end within [`VALUE_MAX`]
//! bytes is no value, which keeps the whole search linear in the packet (P2).

/// What a search over one or more packets has seen so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Seen {
    lat: bool,
    lon: bool,
    nonzero: bool,
}

/// Whether the XMP text `packet` names a location (the module doc has the rule).
pub(crate) fn names_gps(packet: &[u8]) -> bool {
    located(&narrow(packet))
}

/// Whether the already narrowed `text` names a location.
fn located(text: &[u8]) -> bool {
    let s = scan(text);
    s.lat && s.lon && s.nonzero
}

/// Whether the concatenation of `parts` names a location: the main packet of a JPEG and its
/// Extended XMP, whose segments can split a name or a value (critic F19). Each part is narrowed
/// on its own (an Extended XMP part is already the concatenation of its segments), and the
/// joined text is searched as it is: narrowing it again could take a run of tiny parts for
/// UTF-16 and turn a located packet behind them into nothing (LOC PR2).
pub(crate) fn parts_name_gps(parts: &[Vec<u8>]) -> bool {
    let mut text = Vec::new();
    for p in parts {
        text.extend(narrow(p));
        text.push(b'\n');
    }
    located(&text)
}

/// The longest value looked at: a coordinate is a few dozen bytes (`30,12.576N`, `-40.1191`),
/// so a value is searched for its end in at most this many bytes after the quote or the `>`;
/// further on there is no value. It keeps the search linear, whatever a hostile packet repeats
/// (LOC P2: a name followed by a megabyte without a closing quote or `<` was searched to the end
/// of the text for every occurrence).
pub(crate) const VALUE_MAX: usize = 64;

#[cfg(test)]
thread_local! {
    /// The bytes the value search looked at on this thread: the tests prove the search linear by
    /// this count (at most `VALUE_MAX + 1` per occurrence), not by a clock.
    pub(crate) static VALUE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One byte of a value looked at (counted in tests only; nothing in a normal build).
#[inline(always)]
fn looked_at() {
    #[cfg(test)]
    VALUE_BYTES.with(|n| n.set(n.get() + 1));
}

const LAT: &[&[u8]] = &[b"gpslatitude"];
const LON: &[&[u8]] = &[b"gpslongitude", b"gpslongtitude"];

fn scan(text: &[u8]) -> Seen {
    let mut seen = Seen::default();
    for (names, is_lat) in [(LAT, true), (LON, false)] {
        for name in names {
            let mut from = 0;
            while let Some(at) = find_ci(&text[from..], name) {
                let start = from + at;
                from = start + name.len();
                if let Some(zero) = value_at(text, start, from) {
                    if is_lat {
                        seen.lat = true;
                    } else {
                        seen.lon = true;
                    }
                    seen.nonzero |= !zero;
                }
            }
        }
    }
    seen
}

/// The first case-insensitive occurrence of `needle` (lower case) in `hay`.
fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle))
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_' | b'.')
}

/// The value after a name found at `start..end`: `Some(true)` for a zero value, `Some(false)`
/// for a non-zero one, `None` when this occurrence carries no value.
fn value_at(text: &[u8], start: usize, end: usize) -> Option<bool> {
    // The whole name (its prefix included) must end here: `GPSLatitudeRef` is another name.
    if text.get(end).is_some_and(|&b| is_name_byte(b)) {
        return None;
    }
    // A closing tag `</exif:GPSLatitude>` carries no value.
    let mut p = start;
    while p > 0 && is_name_byte(text[p - 1]) {
        p -= 1;
    }
    if p > 0 && text[p - 1] == b'/' {
        return None;
    }
    let mut i = end;
    let skip_space = |i: &mut usize| {
        while text.get(*i).is_some_and(u8::is_ascii_whitespace) {
            *i += 1;
        }
    };
    skip_space(&mut i);
    let value: &[u8] = match text.get(i)? {
        b'=' => {
            i += 1;
            skip_space(&mut i);
            let q = *text.get(i)?;
            if q != b'"' && q != b'\'' {
                return None;
            }
            let rest = &text[i + 1..];
            let end = rest
                .iter()
                .take(VALUE_MAX + 1)
                .inspect(|_| looked_at())
                .position(|&b| b == q)?;
            &rest[..end]
        }
        b'>' => {
            let rest = &text[i + 1..];
            match rest
                .iter()
                .take(VALUE_MAX + 1)
                .inspect(|_| looked_at())
                .position(|&b| b == b'<')
            {
                Some(end) => &rest[..end],
                // The text ends inside the value (a cut packet): that is its end.
                None if rest.len() <= VALUE_MAX => rest,
                None => return None,
            }
        }
        _ => return None,
    };
    if !value.iter().any(u8::is_ascii_digit) {
        return None;
    }
    Some(!value.iter().any(|b| (b'1'..=b'9').contains(b)))
}

/// A UTF-16 packet narrowed to one byte per code unit (ASCII kept, everything else `0x80`); any
/// other packet as it is.
fn narrow(p: &[u8]) -> Vec<u8> {
    let le = match p {
        [0xff, 0xfe, ..] => Some(true),
        [0xfe, 0xff, ..] => Some(false),
        _ => utf16_guess(p),
    };
    let Some(le) = le else {
        return p.to_vec();
    };
    let (body, _) = p.as_chunks::<2>();
    body.iter()
        .map(|&[a, b]| {
            let unit = if le {
                u16::from_le_bytes([a, b])
            } else {
                u16::from_be_bytes([a, b])
            };
            u8::try_from(unit).ok().filter(u8::is_ascii).unwrap_or(0x80)
        })
        .collect()
}

/// Guesses UTF-16 without a byte-order mark from the NUL bytes of the first 256 bytes: ASCII
/// text in UTF-16 has a NUL in every other byte. `Some(true)` = little endian.
fn utf16_guess(p: &[u8]) -> Option<bool> {
    let head = &p[..p.len().min(256)];
    let pairs = head.len() / 2;
    if pairs < 4 {
        return None;
    }
    let (chunks, _) = head.as_chunks::<2>();
    let even0 = chunks.iter().filter(|c| c[0] == 0).count();
    let odd0 = chunks.iter().filter(|c| c[1] == 0).count();
    if odd0 * 4 >= pairs * 3 && even0 * 4 < pairs {
        Some(true)
    } else if even0 * 4 >= pairs * 3 && odd0 * 4 < pairs {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16(s: &str, le: bool, bom: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if bom {
            v.extend(if le { [0xff, 0xfe] } else { [0xfe, 0xff] });
        }
        for u in s.encode_utf16() {
            v.extend(if le { u.to_le_bytes() } else { u.to_be_bytes() });
        }
        v
    }

    const ATTRS: &str = "<x:xmpmeta><rdf:Description exif:GPSLatitude=\"30,12.576N\" \
                         exif:GPSLongitude=\"40,7.148W\"/></x:xmpmeta>";

    #[test]
    fn attributes_and_elements_are_found() {
        assert!(names_gps(ATTRS.as_bytes()));
        let el = "<rdf:Description><exif:GPSLatitude>30,12.576N</exif:GPSLatitude>\
                  <exif:GPSLongitude>40,7.148W</exif:GPSLongitude></rdf:Description>";
        assert!(names_gps(el.as_bytes()));
        let single = "<d exif:GPSLatitude = '30,12.576N' exif:gpslongitude='40,7.148W'/>";
        assert!(names_gps(single.as_bytes()));
    }

    #[test]
    fn dji_spellings_are_found() {
        for lon in ["GpsLongitude", "GpsLongtitude"] {
            let p = format!(
                "<rdf:Description drone-dji:GpsLatitude=\"+30.209600\" drone-dji:{lon}=\"-40.119100\"/>"
            );
            assert!(names_gps(p.as_bytes()), "{lon}");
        }
    }

    #[test]
    fn zeros_and_names_without_values_are_not_a_location() {
        for p in [
            "<d exif:GPSLatitude=\"0,0.0N\" exif:GPSLongitude=\"0,0.0E\"/>",
            "<d drone-dji:GpsLatitude=\"+0.000000\" drone-dji:GpsLongitude=\"-0.000000\"/>",
            "<d exif:GPSLatitude=\"\" exif:GPSLongitude=\"\"/>",
            "<d exif:GPSLatitudeRef=\"N1\" exif:GPSLongitudeRef=\"W2\"/>",
            "GPSLatitude GPSLongitude are names in a description",
            "<d exif:GPSLatitude=\"30,12.576N\"/>",
            "<d exif:GPSLongitude=\"40,7.148W\"/>",
            "<d xmp:CreatorTool=\"Acme 1\"/>",
            "</exif:GPSLatitude>12</exif:GPSLongitude>34",
        ] {
            assert!(!names_gps(p.as_bytes()), "{p}");
        }
        // One coordinate zero, the other not: a real place on the equator.
        assert!(names_gps(
            b"<d exif:GPSLatitude=\"0,0.0N\" exif:GPSLongitude=\"40,7.148W\"/>"
        ));
    }

    #[test]
    fn utf16_packets_are_found() {
        for le in [true, false] {
            for bom in [true, false] {
                assert!(names_gps(&utf16(ATTRS, le, bom)), "le {le} bom {bom}");
                let zero = "<d exif:GPSLatitude=\"0,0.0N\" exif:GPSLongitude=\"0,0.0E\"/>";
                assert!(!names_gps(&utf16(zero, le, bom)));
            }
        }
    }

    #[test]
    fn a_name_split_between_parts_is_found() {
        let (a, b) = ATTRS
            .as_bytes()
            .split_at(ATTRS.find("Latitude").unwrap() + 3);
        assert!(!names_gps(a) && !names_gps(b));
        let joined = [a, b].concat();
        assert!(parts_name_gps(&[b"<x:xmpmeta/>".to_vec(), joined]));
    }
}
