//! Attacks on the archive reader, written by a reviewer who did not write the reader. Each test
//! states what the reader SHOULD do. On the reader as first written (2026-10-08) five failed:
//! the four marked DEFECT and `attack_a_broken_header_is_bad_header`; the reader was changed
//! until all pass (the header format is refused, a frame's fields are counted before it is
//! decoded, a frame is read into a buffer of exactly its length). One is ignored because it
//! is a decision, not a defect. The data is synthetic; nothing prints content.

use super::*;

const MIB: usize = 1024 * 1024;

/// libsignal's magic number of the format with an unencrypted, UNAUTHENTICATED metadata header.
const MAGIC: &[u8] = b"SBACKUP\x01";

/// The header (magic number, length, metadata) of libsignal's own `new-account` test archive.
fn libsignal_header() -> Vec<u8> {
    let file = std::fs::read(testdata("new-account.binproto.encrypted")).unwrap();
    assert_eq!(&file[..8], MAGIC);
    let len = usize::from(file[8]);
    assert!(len < 0x80, "a one-byte length");
    file[..8 + 1 + len].to_vec()
}

/// DEFECT 1. A file that starts with the magic number makes libsignal parse a length-delimited
/// metadata message BEFORE any HMAC is checked, with its own `VarintDelimitedReader`, which
/// allocates and zero-fills by the length it reads. Nobody needs a key for this: 13 bytes from
/// the server or the CDN are enough. (256 MiB here; the prefix can claim 4 GiB.)
#[test]
fn attack_an_unauthenticated_header_cannot_make_the_reader_allocate() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut bytes = MAGIC.to_vec();
    bytes.extend(varint(256 * MIB as u64));
    bytes.extend_from_slice(&[0u8; 64]);
    std::fs::write(&path, &bytes).unwrap();

    watermark::reset();
    let started = std::time::Instant::now();
    let error = ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err();
    let largest = watermark::largest();
    let took = started.elapsed();
    assert!(
        largest < 4 * MIB,
        "a {}-byte file without any valid MAC made the reader allocate {largest} bytes at once \
         (error {error:?}, {took:?})",
        bytes.len()
    );
}

/// DEFECT 2. The header is not covered by the MAC: put in front of a real transfer archive it
/// is accepted, and so is any change inside it. A transfer archive never has one.
#[test]
fn attack_a_header_in_front_of_a_transfer_archive_is_refused() {
    let dir = dir();
    let path = dir.path().join("archive");
    write_sample(&path, &EPHEMERAL, aci()).unwrap();
    let archive = std::fs::read(&path).unwrap();
    let header = libsignal_header();

    let mut with_header = header.clone();
    with_header.extend_from_slice(&archive);
    std::fs::write(&path, &with_header).unwrap();
    let plain = read_all(&path, &key(), Limits::default()).map(|(_, frames)| frames.len());

    let mut accepted = 0;
    for position in 9..header.len() {
        let mut bad = with_header.clone();
        bad[position] ^= 0x01;
        std::fs::write(&path, &bad).unwrap();
        if read_all(&path, &key(), Limits::default()).is_ok() {
            accepted += 1;
        }
    }
    assert!(
        plain.is_err() && accepted == 0,
        "an unauthenticated header in front of a transfer archive read as {plain:?}, and {accepted} \
         of {} one-bit changes inside that header were accepted too",
        header.len() - 9
    );
}

/// DEFECT 3. `read_payload` reads through `read_to_end`, which grows the buffer once more after
/// the frame is complete: a frame costs twice its length (three times while the copy is made),
/// and the first buffer, which holds the whole frame, is freed without being wiped.
#[test]
fn attack_one_frame_at_the_limit_costs_one_frame_of_memory() {
    let dir = dir();
    let path = dir.path().join("archive");
    let limits = Limits::default();
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&filler(limits.max_frame_bytes as usize));
    writer.write_with_iv(&path, &key(), IV).unwrap();

    let mut reader = ArchiveReader::open(&path, &key(), limits).unwrap();
    watermark::reset();
    assert_eq!(reader.next_frame(), Ok(Some(Frame::Unknown)));
    let largest = watermark::largest();
    assert!(
        largest <= limits.max_frame_bytes as usize + MIB,
        "a frame of {} bytes was read into an allocation of {largest} bytes",
        limits.max_frame_bytes
    );
}

/// DEFECT 4. The cap on a frame's ENCODED length does not bound what it decodes to: an empty
/// element of a repeated message field is two bytes on the wire and a whole struct in memory.
/// (1 MiB here; the default limit is 16 times that.)
#[test]
fn attack_a_frame_cannot_decode_to_many_times_its_length() {
    let dir = dir();
    let path = dir.path().join("archive");
    // Frame { chatItem (4) { revisions (6): N empty ChatItems } }
    let elements = MIB / 2;
    let mut inner = Vec::with_capacity(MIB);
    for _ in 0..elements {
        inner.extend_from_slice(&[0x32, 0x00]);
    }
    let mut payload = vec![0x22];
    payload.extend(varint(inner.len() as u64));
    payload.extend(inner);
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&payload);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let file_len = std::fs::metadata(&path).unwrap().len();

    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    watermark::reset();
    // (As first written the reader decoded this frame to 524,288 elements of 832 bytes: 436 MB
    // in one allocation. It now counts the fields before it decodes and refuses the frame.)
    let error = reader.next_frame().unwrap_err();
    let largest = watermark::largest();
    assert_eq!(error, ArchiveError::FrameTooComplex { max: 100_000 });
    assert!(
        largest <= 2 * payload.len(),
        "a frame of {} encoded bytes (a file of {file_len}) cost an allocation of {largest} \
         bytes before it was refused",
        payload.len(),
    );
    // A frame of the same shape inside the limit still decodes.
    let mut inner = Vec::new();
    for _ in 0..1000 {
        inner.extend_from_slice(&[0x32, 0x00]);
    }
    let mut payload = vec![0x22];
    payload.extend(varint(inner.len() as u64));
    payload.extend(inner);
    let mut writer = ArchiveWriter::new(&sample_info());
    writer.raw_frame(&payload);
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let mut reader = ArchiveReader::open(&path, &key(), Limits::default()).unwrap();
    let Some(Frame::ChatItem(item)) = reader.next_frame().unwrap() else {
        panic!("a chat item");
    };
    assert_eq!(item.revisions.len(), 1000);
}

/// Files a key holder can make that are almost nothing: none may panic, none is a clean archive.
#[test]
fn attack_tiny_files_with_a_valid_mac_are_refused_without_a_panic() {
    let dir = dir();
    let path = dir.path().join("archive");
    let key = key();
    let sealed = |content: &[u8]| {
        let mut out = content.to_vec();
        out.extend_from_slice(&mac(&key, content));
        out
    };
    let mut results = Vec::new();
    // 8 bytes, the IV alone, the IV and half a block, the IV and one block of garbage.
    for content in [&[7u8; 8][..], &[7u8; 16], &[7u8; 24], &[7u8; 32]] {
        std::fs::write(&path, sealed(content)).unwrap();
        results.push(ArchiveReader::open(&path, &key, Limits::default()).map(|_| ()));
    }
    // An empty plaintext (one block of padding).
    std::fs::write(&path, seal_unpadded_bytes(&key, IV, b"")).unwrap();
    results.push(ArchiveReader::open(&path, &key, Limits::default()).map(|_| ()));
    assert!(
        results.iter().all(Result::is_err),
        "a file with no frame opened: {results:?}"
    );
}

/// What comes after the first gzip member is never decoded: a second member with more frames,
/// or bytes that are not padding, read as a clean end (7 frames, `Ok(None)`). libsignal's
/// validator does the same, and the phone writes one member and zeros, so this is a decision
/// for the lead, not a defect: say in the module docs that it is so, or refuse it.
#[test]
#[ignore = "a decision: bytes after the first gzip member are authenticated but never decoded"]
fn attack_frames_in_a_second_gzip_member_are_not_silently_dropped() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut compressed = sample_writer().compressed();
    let first = compressed.len();
    let second = {
        let mut bytes = Vec::new();
        for frame in sample_frames() {
            let payload = prost::Message::encode_to_vec(&frame.to_proto());
            bytes.extend(varint(payload.len() as u64));
            bytes.extend(payload);
        }
        gzip_of(&bytes)
    };
    compressed.extend_from_slice(&second);
    std::fs::write(&path, seal_unpadded_bytes(&key(), IV, &compressed)).unwrap();
    let two_members = read_all(&path, &key(), Limits::default()).map(|(_, frames)| frames.len());

    let mut garbage = sample_writer().compressed();
    garbage.extend_from_slice(&[0xa5; 4096]);
    std::fs::write(&path, seal_unpadded_bytes(&key(), IV, &garbage)).unwrap();
    let with_garbage = read_all(&path, &key(), Limits::default()).map(|(_, frames)| frames.len());

    assert!(
        two_members != Ok(7) && with_garbage != Ok(7),
        "{} bytes of a second gzip member with 7 more frames read as {two_members:?}; 4096 bytes \
         that are not zero padding read as {with_garbage:?} (first member: {first} bytes)",
        second.len()
    );
}

/// The BackupInfo must be the first message and nothing else may stand in for it; a second one
/// later is not a frame.
#[test]
fn attack_the_backup_info_is_first_and_only_once() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut results = Vec::new();
    // Every sample frame in the place of the BackupInfo.
    for frame in sample_frames() {
        let payload = prost::Message::encode_to_vec(&frame.to_proto());
        let mut bytes = varint(payload.len() as u64);
        bytes.extend(payload);
        seal(&path, &key(), IV, &gzip_of(&bytes)).unwrap();
        results.push(ArchiveReader::open(&path, &key(), Limits::default()).map(|_| ()));
    }
    assert!(results.iter().all(Result::is_err), "{results:?}");

    // A second BackupInfo after the frames.
    let mut writer = sample_writer();
    writer.raw_frame(&prost::Message::encode_to_vec(&sample_info().to_proto()));
    writer.write_with_iv(&path, &key(), IV).unwrap();
    let second = read_all(&path, &key(), Limits::default()).map(|(_, frames)| frames.len());
    assert!(second.is_err(), "a second BackupInfo read as {second:?}");
}

/// Length prefixes: over-long encodings, ten bytes, never-ending, also as the FIRST prefix.
#[test]
fn attack_length_prefixes_of_every_shape_end_in_an_error_or_a_frame() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut results = Vec::new();
    let prefixes: [&[u8]; 6] = [
        &[0xff; 10],
        &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01],
        &[0x80, 0x80, 0x80, 0x80, 0x80],
        &[0xff, 0xff, 0xff, 0xff, 0x0f],
        &[0x80, 0x80, 0x80, 0x80, 0x00],
        &[0x80, 0x00],
    ];
    for prefix in prefixes {
        // As the first prefix of the stream (the BackupInfo's).
        let mut first = prefix.to_vec();
        first.extend_from_slice(&[0u8; 64 * 1024]);
        seal(&path, &key(), IV, &gzip_of(&first)).unwrap();
        watermark::reset();
        let opened = ArchiveReader::open(&path, &key(), Limits::default()).map(|_| ());
        assert!(watermark::largest() < 4 * MIB);
        // And as a later one.
        let mut writer = sample_writer();
        writer.raw(prefix);
        writer.write_with_iv(&path, &key(), IV).unwrap();
        watermark::reset();
        let later = read_all(&path, &key(), Limits::default()).map(|(_, frames)| frames.len());
        assert!(watermark::largest() < 4 * MIB);
        results.push((opened, later));
    }
    // The ten-byte and the never-ending ones are refused in both places.
    for (opened, later) in &results[..3] {
        assert_eq!(*opened, Err(ArchiveError::BadFraming));
        assert_eq!(*later, Err(ArchiveError::BadFraming));
    }
    assert!(matches!(
        results[3],
        (
            Err(ArchiveError::FrameTooLong { .. }),
            Err(ArchiveError::FrameTooLong { .. })
        )
    ));
}

/// A broken unencrypted header is `BadHeader`, whatever is broken in it (and a test reaches
/// that kind at all).
#[test]
fn attack_a_broken_header_is_bad_header() {
    let dir = dir();
    let path = dir.path().join("archive");
    let mut results = Vec::new();
    let bodies: [&[u8]; 4] = [
        // not a protobuf message (field number 0)
        &[0x00, 0x00, 0x00],
        // a message with no pair
        &[0x0a, 0x00],
        // a length-delimited field that runs past the end of the message
        &[0x0a, 0x7f, 0x01],
        // 1,000 unknown fields (field 15, varint 0): one WARN line each in libsignal
        &[0x78, 0x00].repeat(1000),
    ];
    for body in bodies {
        let mut bytes = MAGIC.to_vec();
        bytes.extend(varint(body.len() as u64));
        bytes.extend_from_slice(body);
        bytes.extend_from_slice(&[0u8; 96]);
        std::fs::write(&path, &bytes).unwrap();
        results.push(ArchiveReader::open(&path, &key(), Limits::default()).unwrap_err());
    }
    assert!(
        results
            .iter()
            .all(|error| *error == ArchiveError::BadHeader),
        "broken headers read as {results:?}"
    );
}
