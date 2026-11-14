//! Binary and ASCII STL, from a byte slice. Non-finite triangles are dropped.

use crate::{Bound, Format, MAX_ASCII_LINE, MAX_TRIANGLES, Mesh, MeshError};

/// `84 + 50 n == len` for the `n` the header states.
pub(crate) fn size_rule_holds(bytes: &[u8]) -> bool {
    if bytes.len() < 84 {
        return false;
    }
    let n = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as u64;
    84 + 50 * n == bytes.len() as u64
}

pub(crate) fn parse_binary(bytes: &[u8]) -> Result<Mesh, MeshError> {
    if bytes.len() < 84 {
        return Err(MeshError::Malformed("short binary STL"));
    }
    let n = u32::from_le_bytes([bytes[80], bytes[81], bytes[82], bytes[83]]) as u64;
    if 84 + 50 * n != bytes.len() as u64 {
        return Err(MeshError::Malformed("size does not match the count"));
    }
    if n > MAX_TRIANGLES as u64 {
        return Err(MeshError::Refused(Bound::Triangles));
    }
    let mut positions = Vec::with_capacity(n as usize * 9);
    for rec in bytes[84..].as_chunks::<50>().0.iter() {
        let mut t = [0f32; 9];
        for (k, c) in t.iter_mut().enumerate() {
            let o = 12 + 4 * k;
            *c = f32::from_le_bytes([rec[o], rec[o + 1], rec[o + 2], rec[o + 3]]);
        }
        if t.iter().all(|c| c.is_finite()) {
            positions.extend_from_slice(&t);
        }
    }
    let n = positions.len() / 9;
    Mesh::new(positions, vec![0; n], Format::StlBinary)
}

pub(crate) fn parse_ascii(bytes: &[u8]) -> Result<Mesh, MeshError> {
    let mut positions: Vec<f32> = Vec::new();
    let mut tris = 0usize;
    let mut cur = [0f32; 9];
    let mut k = 0usize;
    let mut rest = bytes;
    while !rest.is_empty() {
        // A line ends at LF, CR or CRLF (counted once).
        let end = rest.iter().position(|&b| b == b'\n' || b == b'\r');
        let (line, next) = match end {
            Some(i) => {
                let skip = if rest[i] == b'\r' && rest.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                (&rest[..i], &rest[i + skip..])
            }
            None => (rest, &rest[rest.len()..]),
        };
        if line.len() > MAX_ASCII_LINE {
            return Err(MeshError::Refused(Bound::AsciiLine));
        }
        rest = next;
        let s = std::str::from_utf8(line).map_err(|_| MeshError::Malformed("not UTF-8"))?;
        let mut it = s.split_ascii_whitespace();
        let Some(word) = it.next() else { continue };
        if word.eq_ignore_ascii_case("vertex") {
            if k >= 9 {
                return Err(MeshError::Malformed("more than 3 vertices in a facet"));
            }
            for _ in 0..3 {
                cur[k] = it
                    .next()
                    .ok_or(MeshError::Malformed("short vertex"))?
                    .parse()
                    .map_err(|_| MeshError::Malformed("bad number"))?;
                k += 1;
            }
        } else if word.eq_ignore_ascii_case("endloop") {
            if k == 9 {
                if tris >= MAX_TRIANGLES {
                    return Err(MeshError::Refused(Bound::Triangles));
                }
                tris += 1;
                if cur.iter().all(|c| c.is_finite()) {
                    positions.extend_from_slice(&cur);
                }
            }
            k = 0;
        }
    }
    let n = positions.len() / 9;
    Mesh::new(positions, vec![0; n], Format::StlAscii)
}
