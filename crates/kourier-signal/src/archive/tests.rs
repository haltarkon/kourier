//! Proof, both ways: our reader reads what our writer makes and what libsignal's own test
//! archives hold, libsignal's validator accepts what our writer makes, and every limit and every
//! error fires on a file built to trigger it. The data is synthetic.

use std::path::{Path, PathBuf};

use libsignal_account_keys::{AccountEntropyPool, BackupKey};
use libsignal_core::Aci;
use libsignal_message_backup::BackupReader;
use libsignal_message_backup::backup::Purpose;
use libsignal_message_backup::frame::FileReaderFactory;

use super::writer::{
    ArchiveWriter, mac, sample_frames, sample_info, seal, seal_bytes, seal_unpadded_bytes, varint,
    write_sample,
};
use super::{
    ArchiveError, ArchiveInfo, ArchiveKey, ArchiveReader, Frame, Limits, SUPPORTED_VERSION,
};
use crate::proto::backup;

const EPHEMERAL: [u8; 32] = [0x42; 32];
const IV: [u8; 16] = [9; 16];

fn aci() -> Aci {
    Aci::from_uuid_bytes([0x11; 16])
}

fn key() -> ArchiveKey {
    ArchiveKey::for_transfer(&EPHEMERAL, aci())
}

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary directory")
}

/// Opens `path` and reads every frame; the first error, from the open or from a frame, wins.
fn read_all(
    path: &Path,
    key: &ArchiveKey,
    limits: Limits,
) -> Result<(ArchiveInfo, Vec<Frame>), ArchiveError> {
    let mut reader = ArchiveReader::open(path, key, limits)?;
    let mut frames = Vec::new();
    while let Some(frame) = reader.next_frame()? {
        frames.push(frame);
    }
    Ok((reader.info().clone(), frames))
}

/// A payload of exactly `total` bytes that decodes as a `Frame` with an unset item: an unknown
/// length-delimited field 15 holding zeros.
fn filler(total: usize) -> Vec<u8> {
    filler_with(total, || 0)
}

/// [`filler`] with the bytes after the header taken from `fill`.
fn filler_with(total: usize, mut fill: impl FnMut() -> u8) -> Vec<u8> {
    for prefix in 1..=4 {
        let body = total - 1 - prefix;
        if varint(body as u64).len() == prefix {
            let mut out = vec![0x7a];
            out.extend(varint(body as u64));
            out.extend(std::iter::repeat_with(&mut fill).take(body));
            return out;
        }
    }
    panic!("no filler of {total} bytes");
}

/// A [`filler`] of bytes that do not compress (xorshift64*).
fn noise(total: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    filler_with(total, move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 56) as u8
    })
}

fn sample_writer() -> ArchiveWriter {
    let mut writer = ArchiveWriter::new(&sample_info());
    for frame in sample_frames() {
        writer.frame(&frame);
    }
    writer
}

fn small_limits() -> Limits {
    Limits {
        max_frame_bytes: 4096,
        max_frame_fields: 1000,
        max_decompressed_bytes: 1 << 20,
        max_frames: 100,
    }
}

/// Instruments the allocator of this test binary with the largest single allocation of each
/// thread, to prove that a lying length prefix allocates nothing.
mod attack;

mod watermark {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static LARGEST: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) struct Watermark;

    fn note(size: usize) {
        let _ = LARGEST.try_with(|largest| largest.set(largest.get().max(size)));
    }

    // SAFETY: every call is forwarded to the system allocator unchanged; the thread-local is a
    // `Cell<usize>` with a const initialiser, which never allocates.
    unsafe impl GlobalAlloc for Watermark {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc(layout) }
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note(layout.size());
            unsafe { System.alloc_zeroed(layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note(new_size);
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Watermark = Watermark;

    /// Forgets the largest allocation of this thread so far.
    pub(super) fn reset() {
        LARGEST.with(|largest| largest.set(0));
    }

    /// The largest single allocation this thread asked for since `reset`.
    pub(super) fn largest() -> usize {
        LARGEST.with(Cell::get)
    }
}

// ---- the round trip, and the validator ----

#[test]
fn archive_round_trip_gives_back_the_same_frames() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();

    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    assert_eq!(reader.info(), &sample_info());
    assert_eq!(reader.info().version, SUPPORTED_VERSION);
    let mut frames = Vec::new();
    while let Some(frame) = reader.next_frame().unwrap() {
        frames.push(frame);
    }
    let expected = sample_frames();
    assert_eq!(frames.len(), 7);
    assert_eq!(reader.frames_read(), 7);
    assert!(reader.decompressed_bytes() > 100);
    // `Debug` of a frame prints only its kind, so compare the messages themselves.
    for (got, want) in frames.iter().zip(&expected) {
        assert!(got == want, "a {} frame came back different", want.kind());
    }
    let kinds: Vec<_> = frames.iter().map(Frame::kind).collect();
    assert_eq!(
        kinds,
        [
            "account",
            "recipient",
            "recipient",
            "chat",
            "chat_item",
            "chat_item",
            "chat_item"
        ]
    );
    // The end is final and stays final.
    assert_eq!(reader.next_frame(), Ok(None));
    assert_eq!(reader.next_frame(), Ok(None));
}

#[test]
fn archive_without_padding_after_the_gzip_stream_reads_too() {
    let dir = dir();
    let path = dir.path().join("archive");
    let bytes = seal_unpadded_bytes(&key(), IV, &sample_writer().compressed());
    std::fs::write(&path, bytes).unwrap();
    let (info, frames) = read_all(&path, &key(), Limits::default()).unwrap();
    assert_eq!(info, sample_info());
    assert_eq!(frames.len(), 7);
}

#[test]
fn archive_writer_uses_a_random_iv_and_the_reader_does_not_care() {
    let dir = dir();
    let (a, b) = (dir.path().join("a"), dir.path().join("b"));
    write_sample(&a, &EPHEMERAL, aci()).unwrap();
    write_sample(&b, &EPHEMERAL, aci()).unwrap();
    let (a_bytes, b_bytes) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    assert_eq!(a_bytes.len(), b_bytes.len());
    assert_ne!(a_bytes[..16], b_bytes[..16], "two archives share an IV");
    assert!(read_all(&a, &key(), Limits::default()).is_ok());
    assert!(read_all(&b, &key(), Limits::default()).is_ok());
}

#[test]
fn archive_made_by_our_writer_passes_libsignals_validator() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    let key = key();

    let reader = futures::executor::block_on(BackupReader::new_encrypted_compressed(
        key.message_backup_key(),
        FileReaderFactory { path: &path },
        Purpose::DeviceTransfer,
    ))
    .expect("libsignal opens our archive (HMAC, AES, gzip)");
    let result = futures::executor::block_on(reader.read_all());
    if let Err(error) = &result.result {
        panic!("libsignal's validator refused our archive: {error}");
    }
    assert!(
        result.found_unknown_fields.is_empty(),
        "the validator found fields it does not know in our frames"
    );

    // A different key is refused by libsignal too, which is what our reader says.
    let other = ArchiveKey::for_transfer(&[0x43; 32], aci());
    assert!(
        futures::executor::block_on(BackupReader::new_encrypted_compressed(
            other.message_backup_key(),
            FileReaderFactory { path: &path },
            Purpose::DeviceTransfer,
        ))
        .is_err()
    );
}

#[test]
fn archive_made_with_padding_and_without_both_pass_libsignals_validator() {
    let dir = dir();
    let path = dir.path().join("archive");
    let key = key();
    std::fs::write(
        &path,
        seal_unpadded_bytes(&key, IV, &sample_writer().compressed()),
    )
    .unwrap();
    let reader = futures::executor::block_on(BackupReader::new_encrypted_compressed(
        key.message_backup_key(),
        FileReaderFactory { path: &path },
        Purpose::DeviceTransfer,
    ))
    .unwrap();
    assert!(
        futures::executor::block_on(reader.read_all())
            .result
            .is_ok()
    );
}

// ---- libsignal's own archives ----

/// libsignal's test archives (`rust/message-backup/tests/res/test-cases/valid-encrypted/`, copied
/// into `testdata/libsignal/`): made by libsignal's test tooling from this account entropy pool
/// and ACI; the same three frames after the `BackupInfo` in both.
const LIBSIGNAL_ENTROPY_POOL: &str =
    "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";

fn libsignal_backup_key() -> [u8; 32] {
    let pool: AccountEntropyPool = LIBSIGNAL_ENTROPY_POOL.parse().expect("a valid pool");
    BackupKey::derive_from_account_entropy_pool(&pool).0
}

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata/libsignal")
        .join(name)
}

fn check_libsignal_frames(info: &ArchiveInfo, frames: &[Frame]) {
    assert_eq!(info.version, 1);
    assert_eq!(info.backup_time_ms, 1_705_692_409_729);
    assert_eq!(info.media_root_backup_key.as_deref(), Some(&[0xab; 32]));
    let kinds: Vec<_> = frames.iter().map(Frame::kind).collect();
    assert_eq!(kinds, ["account", "recipient", "chat"]);
    let Frame::Account(account) = &frames[0] else {
        unreachable!()
    };
    assert_eq!(account.profile_key.len(), 32);
    let Frame::Recipient(recipient) = &frames[1] else {
        unreachable!()
    };
    assert_eq!(recipient.id, 2);
    assert!(matches!(
        recipient.destination,
        Some(backup::recipient::Destination::Self_(_))
    ));
    let Frame::Chat(chat) = &frames[2] else {
        unreachable!()
    };
    assert_eq!((chat.id, chat.recipient_id), (1, 2));
}

#[test]
fn archive_reads_libsignals_legacy_encrypted_test_archive() {
    // No magic number, no forward-secrecy token: the format a phone writes for a transfer, so
    // the key is exactly `ArchiveKey::for_transfer`.
    let key = ArchiveKey::for_transfer(&libsignal_backup_key(), Aci::from_uuid_bytes([0x11; 16]));
    let (info, frames) = read_all(
        &testdata("legacy-account.binproto.encrypted"),
        &key,
        Limits::default(),
    )
    .unwrap();
    check_libsignal_frames(&info, &frames);
}

#[test]
fn archive_refuses_the_header_format_of_remote_backups() {
    // libsignal's own test archive of the OTHER format: a magic number and an unencrypted
    // metadata header that no MAC covers. A transfer archive never has one; it is refused
    // before anything of it is parsed, whatever the key (see `HEADER_FORMAT_MAGIC`).
    for key in [
        ArchiveKey::for_transfer(&libsignal_backup_key(), Aci::from_uuid_bytes([0x11; 16])),
        key(),
    ] {
        assert_eq!(
            read_all(
                &testdata("new-account.binproto.encrypted"),
                &key,
                Limits::default()
            )
            .unwrap_err(),
            ArchiveError::BadHeader
        );
    }
}

// ---- integrity ----

#[test]
fn archive_a_wrong_key_fails_the_hmac() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    let other_key = ArchiveKey::for_transfer(&[0x43; 32], aci());
    let other_account = ArchiveKey::for_transfer(&EPHEMERAL, Aci::from_uuid_bytes([0x12; 16]));
    for wrong in [other_key, other_account] {
        assert_eq!(
            ArchiveReader::open(&path, &wrong, Limits::default()).unwrap_err(),
            ArchiveError::Hmac
        );
    }
}

#[test]
fn archive_a_flipped_byte_anywhere_fails_the_hmac() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    let good = std::fs::read(&path).unwrap();
    assert!(
        good.len() > 500,
        "the sample archive is {} bytes",
        good.len()
    );
    for position in 0..good.len() {
        let mut bad = good.clone();
        bad[position] ^= 0x01;
        std::fs::write(&path, &bad).unwrap();
        assert_eq!(
            ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err(),
            ArchiveError::Hmac,
            "a flipped bit at byte {position} of {} was not an HMAC failure",
            good.len()
        );
    }
}

#[test]
fn archive_a_file_cut_short_is_refused_whatever_the_cut() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    let good = std::fs::read(&path).unwrap();
    let n = good.len();
    for cut in [
        0,
        1,
        7,
        8,
        15,
        16,
        39,
        40,
        47,
        48,
        64,
        100,
        n / 2,
        n - 33,
        n - 32,
        n - 1,
    ] {
        std::fs::write(&path, &good[..cut]).unwrap();
        let error = ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err();
        // The MAC is the last 32 bytes, so a cut file fails its MAC like a corrupted one; only a
        // file too short to hold a MAC after its first eight bytes is "cut short".
        let expected = if cut < 40 {
            ArchiveError::Truncated
        } else {
            ArchiveError::Hmac
        };
        assert_eq!(error, expected, "a file cut at {cut} of {n} bytes");
    }
    // A missing file is the operating system's error, not a content error.
    assert_eq!(
        ArchiveReader::open(&dir.path().join("nothing"), &key(), Limits::default()).unwrap_err(),
        ArchiveError::Io(std::io::ErrorKind::NotFound)
    );
}

#[test]
fn archive_a_stream_that_ends_inside_a_frame_is_truncated() {
    // A valid MAC over a stream whose last frame promises 100 bytes and has 10.
    let dir = dir();
    let path = dir.path().join("archive");
    let mut writer = sample_writer();
    writer.raw(&varint(100)).raw(&[0x7a; 10]);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    for _ in 0..7 {
        assert!(reader.next_frame().unwrap().is_some());
    }
    assert_eq!(reader.next_frame().unwrap_err(), ArchiveError::Truncated);
    // The reader is finished: it says the same again, and never "the end".
    assert_eq!(reader.next_frame().unwrap_err(), ArchiveError::Truncated);

    // Inside a length prefix: a continuation byte and then nothing.
    let mut writer = sample_writer();
    writer.raw(&[0x80]);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::Truncated
    );
}

#[test]
fn archive_a_gzip_stream_that_ends_early_is_truncated() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut writer = ArchiveWriter::new(&sample_info());
    for _ in 0..200 {
        writer.raw_frame(&filler(300));
    }
    let compressed = writer.compressed();
    // Cut the gzip stream (no padding after the cut: the cipher's own padding is valid).
    let cut = &compressed[..compressed.len() / 2];
    std::fs::write(&path, seal_unpadded_bytes(&key(), IV, cut)).unwrap();
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::Truncated
    );
}

#[test]
fn archive_bad_padding_is_its_own_error() {
    // A valid MAC over a ciphertext whose last block decrypts to garbage: change the last cipher
    // block and compute the MAC again.
    let dir = dir();
    let path = dir.path().join("archive");
    let key = key();
    let mut bytes = seal_unpadded_bytes(&key, IV, &sample_writer().compressed());
    let end = bytes.len() - 32;
    bytes[end - 5] ^= 0x55;
    let new_mac = mac(&key, &bytes[..end]);
    bytes[end..].copy_from_slice(&new_mac);
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        read_all(&path, &key, Limits::default()).unwrap_err(),
        ArchiveError::BadPadding
    );
}

#[test]
fn archive_bad_gzip_is_its_own_error() {
    let dir = dir();
    let path = dir.path().join("archive");
    // Not gzip at all, under a valid MAC and valid padding.
    seal(
        &path,
        &key(),
        IV,
        b"this is a perfectly fine file but not gzip",
    )
    .unwrap();
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::BadGzip
    );
    // Gzip whose trailer (CRC) is wrong: every frame decodes, then the end fails.
    let mut compressed = sample_writer().compressed();
    let crc = compressed.len() - 8;
    compressed[crc] ^= 0xff;
    std::fs::write(&path, seal_unpadded_bytes(&key(), IV, &compressed)).unwrap();
    // The error may come with the first read or after some frames, but never as a clean end.
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::BadGzip
    );
}

#[test]
fn archive_default_limits_are_the_documented_ones() {
    assert_eq!(
        Limits::default(),
        Limits {
            max_frame_bytes: 4 * 1024 * 1024,
            max_frame_fields: 100_000,
            max_decompressed_bytes: 8 * 1024 * 1024 * 1024,
            max_frames: 20_000_000,
        }
    );
}

#[test]
fn archive_errors_say_nothing_of_the_content() {
    // Every error's text, for every kind: a sentence about the file, numbers only where the
    // number is a limit or a version.
    let all = [
        ArchiveError::Io(std::io::ErrorKind::NotFound),
        ArchiveError::Hmac,
        ArchiveError::Truncated,
        ArchiveError::BadHeader,
        ArchiveError::BadPadding,
        ArchiveError::BadGzip,
        ArchiveError::BadFraming,
        ArchiveError::BadProtobuf,
        ArchiveError::MissingInfo,
        ArchiveError::UnsupportedVersion { found: 2 },
        ArchiveError::FrameTooLong { len: 9, max: 8 },
        ArchiveError::TooLarge { max: 7 },
        ArchiveError::TooManyFrames { max: 6 },
    ];
    let texts: std::collections::HashSet<_> = all.iter().map(ToString::to_string).collect();
    assert_eq!(texts.len(), all.len(), "two kinds say the same thing");
    // Frames, keys and the info print no content either.
    let frame = Frame::Account(Box::new(backup::AccountData {
        svr_pin: "123456-secret-pin".to_owned(),
        ..Default::default()
    }));
    assert_eq!(format!("{frame:?}"), "Frame::account");
    assert_eq!(format!("{:?}", Some(&frame)), "Some(Frame::account)");
    assert_eq!(format!("{:?}", key()), "ArchiveKey(<redacted>)");
    let debug = format!("{:?}", sample_info());
    assert!(
        !debug.contains("8.30.2") && !debug.contains("media"),
        "{debug}"
    );
}

#[test]
fn archive_a_file_that_changes_after_the_first_pass_fails_the_final_hmac_check() {
    // `open` checks the HMAC over the whole file; the frames are then read in a second pass over
    // the same file. A byte that changes between the two passes, in the zero padding after the
    // gzip stream (so the frames and the gzip decoder see nothing wrong), must still not read as
    // a clean end.
    let dir = dir();
    let path = dir.path().join("archive");
    let key = key();
    // ~300 KiB of noise, so the padding zone has room and most of the file is still unread when
    // the reader has opened it; grow it until the zone after the gzip stream is 512 bytes or more.
    let compressed = (0..)
        .map(|extra| {
            let mut writer = ArchiveWriter::new(&sample_info());
            for n in 0..30 {
                writer.raw_frame(&noise(10_000 + extra * 997, n));
            }
            let compressed = writer.compressed();
            let padded = crate::attachment::log_pad_size(compressed.len() as u64) as usize;
            (compressed, padded)
        })
        .find(|(compressed, padded)| padded - compressed.len() >= 512)
        .unwrap()
        .0;
    assert!(
        compressed.len() > 200_000,
        "{} bytes of gzip",
        compressed.len()
    );
    seal(&path, &key, IV, &compressed).unwrap();

    // Control: untouched, it ends cleanly.
    assert_eq!(
        read_all(&path, &key, Limits::default()).unwrap().1.len(),
        30
    );

    let mut reader = ArchiveReader::open(&path, &key, Limits::default()).unwrap();
    {
        // In place, into the same inode that the reader holds open: 16 bytes of IV, then the
        // plaintext, so this byte is 256 bytes into the zero padding.
        use std::io::{Seek as _, SeekFrom, Write as _};
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(16 + compressed.len() as u64 + 256))
            .unwrap();
        file.write_all(&[0x5a]).unwrap();
    }
    let mut frames = 0;
    let error = loop {
        match reader.next_frame() {
            Ok(Some(_)) => frames += 1,
            Ok(None) => panic!("a file that changed after the first pass read as a clean end"),
            Err(error) => break error,
        }
    };
    assert_eq!(frames, 30, "every frame is read before the end is checked");
    assert_eq!(error, ArchiveError::Hmac);
}

#[test]
fn archive_is_read_as_a_stream_and_never_held_in_memory() {
    // 12 frames of 200 KB of noise: a file of 2.4 MB that does not compress. The largest single
    // allocation of the whole read (the HMAC pass, the decryption, the decompression, the
    // frames) is a frame's worth, not the file's.
    let dir = dir();
    let path = dir.path().join("archive");
    let mut writer = ArchiveWriter::new(&sample_info());
    for n in 0..12 {
        writer.raw_frame(&noise(200_000, n));
    }
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let file_len = std::fs::metadata(&path).unwrap().len();
    assert!(file_len > 2_300_000, "{file_len} bytes");

    watermark::reset();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    let mut frames = 0;
    while let Some(frame) = reader.next_frame().unwrap() {
        assert_eq!(frame, Frame::Unknown);
        frames += 1;
    }
    let largest = watermark::largest();
    assert_eq!(frames, 12);
    assert!(reader.decompressed_bytes() > 2_300_000);
    assert!(
        largest < 600_000,
        "the largest single allocation was {largest} bytes for a file of {file_len}"
    );
}

#[cfg(feature = "testing")]
#[test]
fn archive_the_public_sample_writer_makes_an_archive_the_reader_takes() {
    let dir = dir();
    let path = dir.path().join("archive");
    crate::archive_testing::write_sample_archive(
        &path,
        &EPHEMERAL,
        uuid::Uuid::from_bytes([0x11; 16]),
    )
    .unwrap();
    let (info, frames) = read_all(&path, &key(), Limits::default()).unwrap();
    assert_eq!(info, sample_info());
    assert_eq!(frames.len(), 7);
}

// ---- limits ----

#[test]
fn archive_a_frame_length_of_4_gib_fails_fast_without_allocating() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw(&varint(u64::from(u32::MAX))).raw(b"a few bytes");
    writer.write_with_iv(&path, &key(), IV).unwrap();

    // The instrument sees a big allocation (so a pass means something).
    watermark::reset();
    drop(std::hint::black_box(Vec::<u8>::with_capacity(
        32 * 1024 * 1024,
    )));
    assert!(watermark::largest() >= 32 * 1024 * 1024);

    watermark::reset();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    let error = reader.next_frame().unwrap_err();
    let largest = watermark::largest();
    assert_eq!(
        error,
        ArchiveError::FrameTooLong {
            len: u64::from(u32::MAX),
            max: 4 * 1024 * 1024,
        }
    );
    assert!(
        largest < 4 * 1024 * 1024,
        "the reader asked for {largest} bytes at once; a 4 GiB prefix must allocate nothing"
    );
    // And it stays failed.
    assert!(matches!(
        reader.next_frame(),
        Err(ArchiveError::FrameTooLong { .. })
    ));

    // A length that fits the limit but outruns the stream costs only what arrived.
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw(&varint(4 * 1024 * 1024)).raw(b"a few bytes");
    writer.write_with_iv(&path, &key(), IV).unwrap();
    watermark::reset();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    assert_eq!(reader.next_frame().unwrap_err(), ArchiveError::Truncated);
    let largest = watermark::largest();
    assert!(
        largest < 2 * 1024 * 1024,
        "a prefix that lies allocated {largest} bytes before the data came"
    );
}

/// A frame `Frame { chatItem { revisions: n empty items } }`: 1 + n fields.
fn frame_of_fields(n: usize) -> Vec<u8> {
    let mut inner = Vec::with_capacity(n * 2);
    for _ in 0..n {
        inner.extend_from_slice(&[0x32, 0x00]);
    }
    let mut payload = vec![0x22];
    payload.extend(varint(inner.len() as u64));
    payload.extend(inner);
    payload
}

#[test]
fn archive_the_field_limit_is_exact_and_counts_through_nested_messages() {
    let dir = dir();
    let path = dir.path().join("archive");
    let limits = small_limits();
    let max = usize::try_from(limits.max_frame_fields).unwrap();
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&frame_of_fields(max - 1));
    writer.raw_frame(&frame_of_fields(max));
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    let Some(Frame::ChatItem(item)) = reader.next_frame().unwrap() else {
        panic!("a chat item");
    };
    assert_eq!(item.revisions.len(), max - 1, "exactly the limit is fine");
    assert_eq!(
        reader.next_frame().unwrap_err(),
        ArchiveError::FrameTooComplex {
            max: limits.max_frame_fields
        }
    );
    // The error stays.
    assert!(matches!(
        reader.next_frame(),
        Err(ArchiveError::FrameTooComplex { .. })
    ));
}

#[test]
fn archive_a_long_text_is_one_field_whatever_it_looks_like() {
    // A body is a string: its bytes are not fields, also when they happen to read as a message
    // (here 1,500 times "field 6, empty", which as a message would be 1,500 fields).
    let dir = dir();
    let path = dir.path().join("archive");
    let limits = small_limits();
    let looks_like_fields = "2\0".repeat(1500);
    assert!(looks_like_fields.len() > usize::try_from(limits.max_frame_fields).unwrap());
    // Frame { account (1) { givenName (4): the text } }: a string field of a real message.
    let mut account = vec![0x22];
    account.extend(varint(looks_like_fields.len() as u64));
    account.extend_from_slice(looks_like_fields.as_bytes());
    let mut payload = vec![0x0a];
    payload.extend(varint(account.len() as u64));
    payload.extend(account);
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&payload);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    // It parses as a message of 1,500 fields, so the count takes it for one and refuses: the
    // count is an upper bound without a schema, and the default limit (100,000 fields in a
    // frame of at most 4 MiB) leaves room for a text of 128 KiB that reads like this one.
    assert!(matches!(
        reader.next_frame(),
        Err(ArchiveError::FrameTooComplex { .. })
    ));
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    let Some(Frame::Account(account)) = reader.next_frame().unwrap() else {
        panic!("the account data");
    };
    assert_eq!(account.given_name, looks_like_fields);
}

#[test]
fn archive_a_frame_nested_deeper_than_a_decoder_goes_is_refused() {
    let dir = dir();
    let path = dir.path().join("archive");
    // chatItem.revisions.revisions… 150 deep, built from the inside out.
    let mut payload = Vec::new();
    for _ in 0..150 {
        let mut outer = vec![0x32];
        outer.extend(varint(payload.len() as u64));
        outer.extend(payload);
        payload = outer;
    }
    let mut frame = vec![0x22];
    frame.extend(varint(payload.len() as u64));
    frame.extend(payload);
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&frame);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    assert!(matches!(
        reader.next_frame(),
        Err(ArchiveError::FrameTooComplex { .. })
    ));
}

#[test]
fn archive_the_frame_length_limit_is_exact() {
    let dir = dir();
    let path = dir.path().join("archive");
    let limits = small_limits();
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&filler(limits.max_frame_bytes as usize));
    writer.raw_frame(&filler(limits.max_frame_bytes as usize + 1));
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    assert_eq!(reader.next_frame(), Ok(Some(Frame::Unknown)));
    assert_eq!(
        reader.next_frame().unwrap_err(),
        ArchiveError::FrameTooLong {
            len: 4097,
            max: 4096
        }
    );
    // The same limit applies to the BackupInfo.
    let tiny = Limits {
        max_frame_bytes: 4,
        ..limits
    };
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    assert!(matches!(
        ArchiveReader::open(&path, &key(), tiny),
        Err(ArchiveError::FrameTooLong { max: 4, .. })
    ));
}

#[test]
fn archive_more_frames_than_the_cap_is_an_error_and_exactly_the_cap_is_not() {
    let dir = dir();
    let path = dir.path().join("archive");
    let limits = Limits {
        max_frames: 7,
        ..small_limits()
    };
    sample_writer().write_with_iv(&path, &key(), IV).unwrap();
    // Seven frames under a cap of seven: fine, and the end is a clean end.
    let (_, frames) = read_all(&path, &key(), limits).unwrap();
    assert_eq!(frames.len(), 7);
    // An eighth is refused.
    let mut writer = sample_writer();
    writer.frame(&Frame::Unknown);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    for _ in 0..7 {
        assert!(reader.next_frame().unwrap().is_some());
    }
    assert_eq!(
        reader.next_frame().unwrap_err(),
        ArchiveError::TooManyFrames { max: 7 }
    );
    assert_eq!(reader.frames_read(), 7);

    // A million empty frames (one zero byte each) inside the byte cap stop at the frame cap.
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw(&vec![0u8; 1_000_000]);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    let mut seen = 0;
    let error = loop {
        match reader.next_frame() {
            Ok(Some(frame)) => {
                assert_eq!(frame, Frame::Unknown);
                seen += 1;
            }
            Ok(None) => panic!("a million frames read as a clean end"),
            Err(error) => break error,
        }
    };
    assert_eq!(error, ArchiveError::TooManyFrames { max: 7 });
    assert_eq!(seen, 7);
}

#[test]
fn archive_a_gzip_bomb_stops_at_the_byte_cap() {
    let dir = dir();
    let path = dir.path().join("archive");
    // 256 frames of 512 KiB of zeros: 128 MiB of output from a file of a few hundred KiB.
    let payload = filler(512 * 1024);
    let mut writer = ArchiveWriter::new(&sample_info());
    for _ in 0..256 {
        writer.raw_frame(&payload);
    }
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let file_len = std::fs::metadata(&path).unwrap().len();
    assert!(file_len < 2 * 1024 * 1024, "the bomb is {file_len} bytes");

    let limits = Limits {
        max_frame_bytes: 1024 * 1024,
        max_frame_fields: 100_000,
        max_decompressed_bytes: 4 * 1024 * 1024,
        max_frames: 1000,
    };
    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    let mut frames = 0;
    let error = loop {
        match reader.next_frame() {
            Ok(Some(_)) => frames += 1,
            Ok(None) => panic!("a 128 MiB stream read as a clean end"),
            Err(error) => break error,
        }
    };
    assert_eq!(
        error,
        ArchiveError::TooLarge {
            max: 4 * 1024 * 1024
        }
    );
    assert!(frames <= 8, "{frames} frames of 512 KiB under a 4 MiB cap");
    // The decoder was stopped at the cap: it produced the cap and the one byte that proved there
    // was more, not the 128 MiB of the bomb.
    assert_eq!(
        reader.decompressed_bytes(),
        limits.max_decompressed_bytes + 1,
        "the decoder produced a different number of bytes than the cap allows"
    );

    // An archive of exactly the cap is not a bomb; one byte under it is.
    sample_writer().write_with_iv(&path, &key(), IV).unwrap();
    let plain = {
        let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
        while reader.next_frame().unwrap().is_some() {}
        reader.decompressed_bytes()
    };
    let at_cap = Limits {
        max_decompressed_bytes: plain,
        ..limits
    };
    assert_eq!(read_all(&path, &key(), at_cap).unwrap().1.len(), 7);
    let under_cap = Limits {
        max_decompressed_bytes: plain - 1,
        ..limits
    };
    assert_eq!(
        read_all(&path, &key(), under_cap).unwrap_err(),
        ArchiveError::TooLarge { max: plain - 1 }
    );
}

// ---- the BackupInfo ----

#[test]
fn archive_an_unknown_backup_info_version_is_its_own_error() {
    let dir = dir();
    let path = dir.path().join("archive");
    for version in [0, 2, 3, u64::MAX] {
        let info = ArchiveInfo {
            version,
            ..sample_info()
        };
        let mut writer = ArchiveWriter::new(&info);
        for frame in sample_frames() {
            writer.frame(&frame);
        }
        writer.write_with_iv(&path, &key(), IV).unwrap();
        assert_eq!(
            ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err(),
            ArchiveError::UnsupportedVersion { found: version },
            "version {version}"
        );
    }
}

#[test]
fn archive_without_any_frame_has_no_info() {
    let dir = dir();
    let path = dir.path().join("archive");
    seal(&path, &key(), IV, &gzip_of(b"")).unwrap();
    assert_eq!(
        ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::MissingInfo
    );
}

fn gzip_of(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(bytes).unwrap();
    gzip.finish().unwrap()
}

// ---- unknown frames and bad framing ----

#[test]
fn archive_a_frame_with_an_unset_or_unknown_item_is_reported_not_fatal() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.frame(&sample_frames()[0]);
    // 1. `item` unset: an empty message.
    writer.raw_frame(&[]);
    // 2. a frame kind from the future: field 15 holding a message.
    writer.raw_frame(&[0x7a, 0x03, 0x08, 0x01, 0x10]);
    // 3. the same field with a bigger payload.
    writer.raw_frame(&filler(300));
    // 4. a known kind with an unknown field inside (a newer phone added a field): still known.
    let mut recipient = backup::Recipient {
        id: 5,
        destination: Some(backup::recipient::Destination::Self_(
            backup::Self_::default(),
        )),
    };
    let mut bytes = prost::Message::encode_to_vec(&backup::Frame {
        item: Some(backup::frame::Item::Recipient(recipient.clone())),
    });
    // field 99, varint 1, appended to the Frame (an unknown field of the Frame itself)
    bytes.extend_from_slice(&[0x98, 0x06, 0x01]);
    writer.raw_frame(&bytes);
    // 5. a real frame after all of them.
    recipient.id = 6;
    writer.frame(&Frame::Recipient(recipient.clone()));
    writer.write_with_iv(&path, &key(), IV).unwrap();

    let (_, frames) = read_all(&path, &key(), Limits::default()).unwrap();
    let kinds: Vec<_> = frames.iter().map(Frame::kind).collect();
    assert_eq!(
        kinds,
        [
            "account",
            "unknown",
            "unknown",
            "unknown",
            "recipient",
            "recipient"
        ]
    );
    let Frame::Recipient(first) = &frames[4] else {
        unreachable!()
    };
    assert_eq!(first.id, 5);
    let Frame::Recipient(last) = &frames[5] else {
        unreachable!()
    };
    assert_eq!(last.id, 6);

    // The validator, by contrast, refuses such an archive: that is the difference the module
    // documents.
    let key = key();
    let reader = futures::executor::block_on(BackupReader::new_encrypted_compressed(
        key.message_backup_key(),
        FileReaderFactory { path: &path },
        Purpose::DeviceTransfer,
    ))
    .unwrap();
    assert!(
        futures::executor::block_on(reader.read_all())
            .result
            .is_err()
    );
}

#[test]
fn archive_bad_framing_and_bad_protobuf_are_their_own_errors() {
    let dir = dir();
    let path = dir.path().join("archive");
    // A length that needs six varint bytes.
    let mut writer = sample_writer();
    writer.raw(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::BadFraming
    );
    // Five bytes, but over 32 bits.
    let mut writer = sample_writer();
    writer.raw(&[0xff, 0xff, 0xff, 0xff, 0x7f]);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::BadFraming
    );
    // A frame that is not a protobuf message (field number 0).
    let mut writer = sample_writer();
    writer.raw_frame(&[0x00, 0x00, 0x00]);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    assert_eq!(
        read_all(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::BadProtobuf
    );
    // A BackupInfo that is not a message.
    let mut compressed = Vec::new();
    {
        use std::io::Write as _;
        let mut gzip = flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::fast());
        gzip.write_all(&varint(3)).unwrap();
        gzip.write_all(&[0x00, 0x00, 0x00]).unwrap();
        gzip.finish().unwrap();
    }
    std::fs::write(&path, seal_bytes(&key(), IV, &compressed).unwrap()).unwrap();
    assert_eq!(
        ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err(),
        ArchiveError::BadProtobuf
    );
}

#[test]
fn archive_an_error_ends_the_reader() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut writer = sample_writer();
    writer.raw_frame(&[0x00, 0x00, 0x00]);
    writer.frame(&Frame::Unknown);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    for _ in 0..7 {
        assert!(reader.next_frame().unwrap().is_some());
    }
    assert_eq!(reader.next_frame().unwrap_err(), ArchiveError::BadProtobuf);
    // The valid frame after the bad one is NOT delivered: the stream's position is no longer
    // trusted once a frame failed.
    assert_eq!(reader.next_frame().unwrap_err(), ArchiveError::BadProtobuf);
}

// ---- the key, the thread ----

#[test]
fn archive_the_key_depends_on_the_ephemeral_key_and_the_aci() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    assert!(read_all(&path, &key(), Limits::default()).is_ok());
    let mut other = EPHEMERAL;
    other[31] ^= 1;
    assert_eq!(
        ArchiveReader::open(
            &path,
            &ArchiveKey::for_transfer(&other, aci()),
            Limits::default()
        )
        .unwrap_err(),
        ArchiveError::Hmac
    );
}

#[test]
fn archive_frames_and_errors_can_cross_threads_and_the_reader_is_drained_on_one() {
    fn is_send<T: Send>() {}
    is_send::<Frame>();
    is_send::<ArchiveError>();
    is_send::<ArchiveInfo>();
    is_send::<Limits>();

    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    // The way the import will call it: open and drain inside one blocking closure, send the
    // frames out.
    let frames = std::thread::spawn(move || {
        let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
        let mut frames = Vec::new();
        while let Some(frame) = reader.next_frame().unwrap() {
            frames.push(frame);
        }
        frames
    })
    .join()
    .unwrap();
    assert_eq!(frames.len(), 7);
}
