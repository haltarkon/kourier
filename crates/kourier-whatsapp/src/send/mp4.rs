//! The MP4 box walk (W5b, decisions G6 and G8): what an MP4 file says about itself, read from its boxes without a
//! codec, so the route ([`super::route`]) can tell a video the phones play (H.264: `avc1`/`avc3`) from one they may
//! not (HEVC, AV1, ...: sent as a document) and an audio-only MP4 (`mp4a`) from a video, and the video message gets
//! its `seconds`, width and height. Nothing here decodes a frame; no thumbnail is made (G8).
//!
//! WHAT IS READ: the top-level boxes are walked header by header through the plaintext reader (a box that is not
//! `moov` is read past and thrown away in pieces, so an `mdat` before or after `moov` costs no memory); `moov` itself
//! is read whole when it is at most [`MOOV_MAX`] bytes, then `mvhd` (duration / timescale, versions 0 and 1) and, in
//! each `trak`, `tkhd` (width and height as 16.16, the matrix's quarter turn), `mdia/hdlr` (`vide` or `soun`) and
//! `mdia/minf/stbl/stsd` (the first sample entry's four letters). The first video track and the first sound track
//! count.
//!
//! BOUNDS: every box length is checked against its parent (and the file), at most [`MAX_DEPTH`] levels and
//! [`MAX_BOXES`] boxes in all; ANYTHING ODD (a box longer than its parent, a short header, a zero timescale, too many
//! boxes) is "no metadata" for that part or for the whole walk, never a panic. Only files the route may send as media
//! are walked (at most `route::MEDIA_LIMIT` bytes), so the walk reads at most that much once more before encryption.

use std::io::{self, Read};

/// The largest `moov` read into memory (a 16 MB video's is far smaller; a bigger one is "no metadata").
pub(crate) const MOOV_MAX: u64 = 4 * 1024 * 1024;
/// Levels of boxes below the file.
pub(crate) const MAX_DEPTH: u32 = 64;
/// Boxes looked at in all (the top level counted).
pub(crate) const MAX_BOXES: u32 = 4096;

/// What the walk found. `None` fields were not there or did not read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The presentation's length, rounded to whole seconds (`mvhd`).
    pub seconds: Option<u32>,
    /// The first video track.
    pub video: Option<VideoTrack>,
    /// The first sound track's sample entry (`mp4a` for AAC).
    pub audio: Option<[u8; 4]>,
}

/// A video track: its sample entry and its size as shown (a quarter turn in the matrix swaps width and height).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoTrack {
    /// The first sample entry of `stsd` (`avc1`, `avc3`, `hvc1`, `av01`, ...); `None` when it did not read.
    pub codec: Option<[u8; 4]>,
    /// Width and height in pixels, when `tkhd` gave both non-zero.
    pub dims: Option<(u32, u32)>,
}

/// Walks the file `reader` yields (`len` bytes of plaintext) and answers what its `moov` says, or `None` when it
/// has no `moov` that reads. An I/O error of the reader is returned (the caller's stop or a broken file).
pub(crate) fn walk(reader: &mut impl Read, len: u64) -> io::Result<Option<Facts>> {
    let mut at = 0u64;
    let mut boxes = 0u32;
    while len - at >= 8 {
        boxes += 1;
        if boxes > MAX_BOXES {
            return Ok(None);
        }
        let mut head = [0u8; 8];
        reader.read_exact(&mut head)?;
        let mut header = 8u64;
        let mut size = u64::from(u32::from_be_bytes([head[0], head[1], head[2], head[3]]));
        let kind = [head[4], head[5], head[6], head[7]];
        if size == 1 {
            if len - at < 16 {
                return Ok(None);
            }
            let mut large = [0u8; 8];
            reader.read_exact(&mut large)?;
            size = u64::from_be_bytes(large);
            header = 16;
        } else if size == 0 {
            size = len - at;
        }
        if size < header || size > len - at {
            return Ok(None);
        }
        let body = size - header;
        if &kind == b"moov" {
            if body > MOOV_MAX {
                return Ok(None);
            }
            let mut moov = vec![0u8; usize::try_from(body).unwrap_or(usize::MAX)];
            reader.read_exact(&mut moov)?;
            return Ok(parse_moov(&moov, boxes));
        }
        let skipped = io::copy(&mut reader.by_ref().take(body), &mut io::sink())?;
        if skipped != body {
            return Ok(None);
        }
        at += size;
    }
    Ok(None)
}

/// The children of a box body: `(type, body)` pairs, or `None` when a child does not fit.
fn children<'a>(body: &'a [u8], depth: u32, count: &mut u32) -> Option<Vec<([u8; 4], &'a [u8])>> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut out = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        *count += 1;
        if *count > MAX_BOXES || rest.len() < 8 {
            return None;
        }
        let mut size = u64::from(be32(rest, 0)?);
        let kind = [rest[4], rest[5], rest[6], rest[7]];
        let mut header = 8usize;
        if size == 1 {
            size = be64(rest, 8)?;
            header = 16;
        } else if size == 0 {
            size = rest.len() as u64;
        }
        let size = usize::try_from(size).ok()?;
        if size < header || size > rest.len() {
            return None;
        }
        out.push((kind, &rest[header..size]));
        rest = &rest[size..];
    }
    Some(out)
}

/// The first child of a kind.
fn child<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(k, _)| k == kind).map(|(_, body)| *body)
}

fn parse_moov(moov: &[u8], mut count: u32) -> Option<Facts> {
    let top = children(moov, 1, &mut count)?;
    let mut facts = Facts {
        seconds: child(&top, b"mvhd").and_then(mvhd_seconds),
        ..Facts::default()
    };
    for (_, trak) in top.iter().filter(|(k, _)| k == b"trak") {
        let Some(track) = children(trak, 2, &mut count) else {
            continue;
        };
        let mdia = child(&track, b"mdia").and_then(|m| children(m, 3, &mut count));
        let Some(mdia) = mdia else { continue };
        let handler = child(&mdia, b"hdlr").and_then(|h| h.get(8..12));
        let entry = child(&mdia, b"minf")
            .and_then(|m| children(m, 4, &mut count))
            .and_then(|minf| child(&minf, b"stbl"))
            .and_then(|s| children(s, 5, &mut count))
            .and_then(|stbl| child(&stbl, b"stsd"))
            .and_then(first_entry);
        match handler {
            Some(b"vide") if facts.video.is_none() => {
                facts.video = Some(VideoTrack {
                    codec: entry,
                    dims: child(&track, b"tkhd").and_then(tkhd_dims),
                });
            }
            Some(b"soun") if facts.audio.is_none() => facts.audio = entry,
            _ => {}
        }
    }
    Some(facts)
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be64(b: &[u8], at: usize) -> Option<u64> {
    let s = b.get(at..at.checked_add(8)?)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Some(u64::from_be_bytes(a))
}

/// `mvhd`: duration / timescale, rounded; a zero timescale or an unknown duration (all ones) is none.
fn mvhd_seconds(body: &[u8]) -> Option<u32> {
    let (timescale, duration) = match body.first()? {
        0 => {
            let d = be32(body, 16)?;
            (
                be32(body, 12)?,
                if d == u32::MAX {
                    u64::MAX
                } else {
                    u64::from(d)
                },
            )
        }
        1 => (be32(body, 20)?, be64(body, 24)?),
        _ => return None,
    };
    if timescale == 0 || duration == u64::MAX {
        return None;
    }
    let timescale = u64::from(timescale);
    u32::try_from(duration.checked_add(timescale / 2)? / timescale).ok()
}

/// `tkhd`: width and height (16.16, the integer part), swapped when the matrix turns the picture a quarter.
fn tkhd_dims(body: &[u8]) -> Option<(u32, u32)> {
    // After version and flags: times, track id, reserved, duration (20 bytes in version 0, 32 in 1), then
    // reserved (8), layer, alternate group, volume, reserved (2 each), the matrix (36), width, height.
    let fixed = match body.first()? {
        0 => 4 + 20,
        1 => 4 + 32,
        _ => return None,
    };
    let matrix = fixed + 16;
    let width = be32(body, matrix + 36)? >> 16;
    let height = be32(body, matrix + 40)? >> 16;
    if width == 0 || height == 0 {
        return None;
    }
    // The matrix is {a, b, u, c, d, v, x, y, w}: a quarter turn has a == 0 and b != 0.
    let (a, b) = (be32(body, matrix)?, be32(body, matrix + 4)?);
    Some(if a == 0 && b != 0 {
        (height, width)
    } else {
        (width, height)
    })
}

/// `stsd`: the four letters of its first sample entry.
fn first_entry(body: &[u8]) -> Option<[u8; 4]> {
    if be32(body, 4)? == 0 {
        return None;
    }
    let entry = body.get(12..16)?;
    Some([entry[0], entry[1], entry[2], entry[3]])
}

#[cfg(test)]
pub(in crate::send) mod tests;
