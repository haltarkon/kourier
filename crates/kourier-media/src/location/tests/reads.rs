//! LOC RF1: a read costs budget even when it brings nothing, so the number of reads a hostile
//! file makes is bounded by the budget, not only by the time. A counting reader stands for the
//! seek system calls of a real file (`FileSource::read` is one seek and one read).

use super::*;

/// A reader over bytes that counts its seeks and its reads.
struct Counting<'a> {
    inner: Cursor<&'a [u8]>,
    seeks: u64,
    reads: u64,
}

impl<'a> Counting<'a> {
    fn new(data: &'a [u8]) -> Self {
        Counting {
            inner: Cursor::new(data),
            seeks: 0,
            reads: 0,
        }
    }
}

impl Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads += 1;
        self.inner.read(buf)
    }
}

impl Seek for Counting<'_> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.seeks += 1;
        self.inner.seek(to)
    }
}

/// The look of `data` through a counting reader: the answer, the seeks and the reads made.
fn counted_look(data: &[u8]) -> (Look, u64, u64) {
    let mut r = Counting::new(data);
    let answer = look(&mut r, data.len() as u64);
    (answer, r.seeks, r.reads)
}

/// The re-check's A3 file: `metas` top-level `meta` boxes, each with 64 Exif items whose `iloc`
/// entries have 64 extents of (0, 0) from a base at the file's end, so every extent is an empty
/// read there. Field sizes: offset 0, length 0, base 4, index 0 (an extent costs no byte).
fn many_empty_extents(metas: usize) -> Vec<u8> {
    let build = |base: u64| {
        let mut f = bx(b"ftyp", b"heic\0\0\0\0mif1heic");
        for _ in 0..metas {
            let infes: Vec<Vec<u8>> = (1..=64).map(|id| infe(2, id, b"Exif", b"")).collect();
            let mut p = vec![0x00, 0x40];
            p.extend(64u16.to_be_bytes());
            for id in 1..=64u16 {
                p.extend(id.to_be_bytes());
                p.extend([0, 0, 0, 0]); // method 0, data reference 0
                p.extend(be(base, 4));
                p.extend(64u16.to_be_bytes());
            }
            let mut m = fbx(
                b"hdlr",
                0,
                &[&[0; 4][..], b"pict", &[0; 12], b"\0"].concat(),
            );
            m.extend(iinf(0, &infes));
            m.extend(fbx(b"iloc", 1, &p));
            f.extend(fbx(b"meta", 0, &m));
        }
        f
    };
    let len = build(0).len() as u64;
    build(len)
}

/// The empty extents of one `meta` (64 x 64 = 4096 of them) make no read at all: what is left
/// are the reads of the 64 `infe` boxes, about 200 (before RF1 the 4096 came on top).
#[test]
fn an_empty_extent_makes_no_read() {
    let f = many_empty_extents(1);
    let (answer, seeks, reads) = counted_look(&f);
    assert!(seeks < 1_000, "{seeks} seeks");
    assert!(reads < 1_000, "{reads} reads");
    // The items have no bytes, so no Exif is there to be read: unreadable, as before.
    assert_eq!(answer, Look::Unreadable);
}

/// The re-check's A3: 1,000 top-level `meta` boxes. Before RF1 the look made 3.1 million seeks
/// (every empty extent was a read that cost no budget). Now the empty extents read nothing and
/// every other read costs at least 8 bytes, so the look can never make more than 2 MiB / 8 =
/// 262,144; it makes about 142,000 (the `infe` boxes of the metas it reaches before the budget
/// ends it), and the empty extents would add 4,096 per `meta` (the limit separates the two).
#[test]
fn a_thousand_metas_of_empty_extents_make_few_reads() {
    let f = many_empty_extents(1_000);
    assert!(f.len() > 2 << 20, "the file is {} bytes", f.len());
    let (answer, seeks, reads) = counted_look(&f);
    assert!(seeks < 200_000, "{seeks} seeks");
    assert!(reads < 200_000, "{reads} reads");
    assert_eq!(answer, Look::Unreadable);
}

#[test]
fn a_jpeg_of_fill_bytes_makes_few_reads() {
    let mut f = vec![0xff, 0xd8];
    f.resize(2 << 20, 0xff);
    f.extend([0xff, 0xff]);
    let (answer, seeks, reads) = counted_look(&f);
    // A byte read one at a time was a read of 1 byte of 2 MiB (2.1 million reads); now each
    // read costs at least 8 bytes: at most 2 MiB / 8 = 262,144 and the walk ends.
    assert!(seeks < 300_000, "{seeks} seeks");
    assert!(reads < 300_000, "{reads} reads");
    assert_eq!(answer, Look::Unreadable);
}
