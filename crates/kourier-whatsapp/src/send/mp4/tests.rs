//! The MP4 box walk (W5b, G6/G8): generated boxes only (no real video), every odd shape answers "no metadata" and
//! never panics.

use std::io::Cursor;

use super::*;

/// A box: size, four letters, body.
fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = u32::try_from(8 + body.len())
        .expect("small")
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

fn mvhd_v0(timescale: u32, duration: u32) -> Vec<u8> {
    let mut body = vec![0u8; 4 + 8];
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&duration.to_be_bytes());
    body.extend_from_slice(&[0u8; 80]);
    bx(b"mvhd", &body)
}

fn mvhd_v1(timescale: u32, duration: u64) -> Vec<u8> {
    let mut body = vec![1u8, 0, 0, 0];
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&timescale.to_be_bytes());
    body.extend_from_slice(&duration.to_be_bytes());
    body.extend_from_slice(&[0u8; 80]);
    bx(b"mvhd", &body)
}

/// `tkhd` version 0 with the identity matrix, or a quarter turn when `turned`.
fn tkhd(width: u32, height: u32, turned: bool) -> Vec<u8> {
    let mut body = vec![0u8; 4 + 20 + 16];
    let one = 0x0001_0000u32;
    let matrix: [u32; 9] = if turned {
        [0, one, 0, one.wrapping_neg(), 0, 0, 0, 0, 0x4000_0000]
    } else {
        [one, 0, 0, 0, one, 0, 0, 0, 0x4000_0000]
    };
    for m in matrix {
        body.extend_from_slice(&m.to_be_bytes());
    }
    body.extend_from_slice(&(width << 16).to_be_bytes());
    body.extend_from_slice(&(height << 16).to_be_bytes());
    bx(b"tkhd", &body)
}

fn trak(handler: &[u8; 4], codec: &[u8; 4], tkhd_box: Option<Vec<u8>>) -> Vec<u8> {
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(handler);
    hdlr.extend_from_slice(&[0u8; 13]);
    let mut stsd = vec![0u8; 4];
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend_from_slice(&bx(codec, &[0u8; 16]));
    let stbl = bx(b"stbl", &bx(b"stsd", &stsd));
    let minf = bx(b"minf", &stbl);
    let mdia = bx(b"mdia", &cat(&[bx(b"hdlr", &hdlr), minf]));
    let mut parts = Vec::new();
    if let Some(t) = tkhd_box {
        parts.push(t);
    }
    parts.push(mdia);
    bx(b"trak", &cat(&parts))
}

fn ftyp() -> Vec<u8> {
    bx(b"ftyp", b"isom\0\0\0\0isomavc1")
}

fn walked(file: &[u8]) -> Option<Facts> {
    walk(&mut Cursor::new(file), file.len() as u64).expect("an in-memory reader does not fail")
}

/// A video with its sound: `moov` before or after a large `mdat` (also the prepare tests' MP4).
pub(in crate::send) fn video(moov_first: bool) -> Vec<u8> {
    let moov = bx(
        b"moov",
        &cat(&[
            mvhd_v0(1000, 12_400),
            trak(b"vide", b"avc1", Some(tkhd(1280, 720, false))),
            trak(b"soun", b"mp4a", None),
        ]),
    );
    let mdat = bx(b"mdat", &vec![0x5au8; 300_000]);
    if moov_first {
        cat(&[ftyp(), moov, mdat])
    } else {
        cat(&[ftyp(), mdat, moov])
    }
}

#[test]
fn a_video_says_its_seconds_size_and_codecs_wherever_its_moov_is() {
    let want = Facts {
        seconds: Some(12),
        video: Some(VideoTrack {
            codec: Some(*b"avc1"),
            dims: Some((1280, 720)),
        }),
        audio: Some(*b"mp4a"),
    };
    assert_eq!(walked(&video(true)), Some(want));
    assert_eq!(walked(&video(false)), Some(want), "a moov at the end");
}

#[test]
fn the_seconds_are_duration_over_timescale_in_both_versions() {
    let file = |m: Vec<u8>| cat(&[ftyp(), bx(b"moov", &m)]);
    let seconds = |m| walked(&file(m)).and_then(|f| f.seconds);
    assert_eq!(seconds(mvhd_v0(600, 600 * 95)), Some(95));
    assert_eq!(
        seconds(mvhd_v0(48_000, 48_000 * 3 + 24_000)),
        Some(4),
        "rounded"
    );
    assert_eq!(
        seconds(mvhd_v1(90_000, 90_000 * 7_200)),
        Some(7_200),
        "version 1"
    );
    assert_eq!(seconds(mvhd_v0(0, 1234)), None, "a zero timescale");
    assert_eq!(
        seconds(mvhd_v0(1000, u32::MAX)),
        None,
        "an unknown duration"
    );
}

#[test]
fn a_quarter_turn_swaps_width_and_height() {
    let file = cat(&[
        ftyp(),
        bx(
            b"moov",
            &trak(b"vide", b"avc1", Some(tkhd(1920, 1080, true))),
        ),
    ]);
    let dims = walked(&file).and_then(|f| f.video).and_then(|v| v.dims);
    assert_eq!(dims, Some((1080, 1920)));
}

#[test]
fn an_audio_only_mp4_and_a_hevc_video_say_so() {
    let audio = cat(&[
        ftyp(),
        bx(
            b"moov",
            &cat(&[mvhd_v0(44_100, 44_100 * 30), trak(b"soun", b"mp4a", None)]),
        ),
    ]);
    let facts = walked(&audio).expect("reads");
    assert_eq!(
        (facts.video, facts.audio, facts.seconds),
        (None, Some(*b"mp4a"), Some(30))
    );

    let hevc = cat(&[
        ftyp(),
        bx(
            b"moov",
            &trak(b"vide", b"hvc1", Some(tkhd(640, 480, false))),
        ),
    ]);
    let codec = walked(&hevc).and_then(|f| f.video).and_then(|v| v.codec);
    assert_eq!(codec, Some(*b"hvc1"));
}

#[test]
fn odd_files_have_no_metadata_and_never_panic() {
    // No moov at all.
    assert_eq!(walked(&cat(&[ftyp(), bx(b"mdat", &[1u8; 64])])), None);
    // A top-level box longer than the file.
    let mut long = ftyp();
    long.extend_from_slice(&1_000_000u32.to_be_bytes());
    long.extend_from_slice(b"moov");
    long.extend_from_slice(&[0u8; 32]);
    assert_eq!(walked(&long), None);
    // A box shorter than its own header.
    let mut short = ftyp();
    short.extend_from_slice(&4u32.to_be_bytes());
    short.extend_from_slice(b"free");
    assert_eq!(walked(&short), None);
    // A child longer than its parent: the moov does not read.
    let mut inner = mvhd_v0(1000, 5000);
    inner[..4].copy_from_slice(&10_000u32.to_be_bytes());
    assert_eq!(walked(&cat(&[ftyp(), bx(b"moov", &inner)])), None);
    // 70 levels of nesting.
    let mut deep = mvhd_v0(1000, 9000);
    for _ in 0..70 {
        deep = bx(b"moov", &deep);
    }
    let facts = walked(&cat(&[ftyp(), deep]));
    assert_eq!(facts.and_then(|f| f.seconds), None);
    // Too many boxes at the top.
    let many: Vec<u8> = (0..=MAX_BOXES).flat_map(|_| bx(b"free", &[])).collect();
    assert_eq!(walked(&cat(&[many, bx(b"moov", &mvhd_v0(1, 1))])), None);
    // A moov over the bound is not read.
    let big = bx(
        b"moov",
        &vec![0u8; usize::try_from(MOOV_MAX).expect("small") + 1],
    );
    assert_eq!(walked(&big), None);
    // Truncated bodies of every box the walk reads.
    let whole = video(true);
    for cut in (0..whole.len()).step_by(7) {
        let _ = walked(&whole[..cut]);
    }
    // Garbage.
    let noise: Vec<u8> = (0..4096u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let _ = walked(&noise);
}

#[test]
fn a_failing_reader_is_an_error_not_metadata() {
    /// Yields the file's first `left` bytes, then fails (a stop, a broken file): in the header and in a skip.
    struct Fails {
        file: Cursor<Vec<u8>>,
        left: usize,
    }
    impl Read for Fails {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.left == 0 {
                return Err(io::Error::other("stopped"));
            }
            let n = buf.len().min(self.left);
            let n = self.file.read(&mut buf[..n])?;
            self.left -= n;
            Ok(n)
        }
    }
    let file = video(false);
    let len = file.len() as u64;
    for left in [0, 4, ftyp().len() + 8 + 1000] {
        let mut reader = Fails {
            file: Cursor::new(file.clone()),
            left,
        };
        assert!(walk(&mut reader, len).is_err(), "failed after {left} bytes");
    }
}
