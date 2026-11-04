//! The file work (W5b, B6): prepare (the core's file -> WhatsApp's ciphertext in the sealed temporary file, by the
//! route) and the source the upload reads. Synthetic files only (generated pictures, boxes and bytes).

use std::io::Write as _;
use std::path::PathBuf;

use kourier_blob::BlobWriter;
use kourier_model::{AttachmentKind, BlobKey, BlobRef};
use sha2::{Digest as _, Sha256};
use wacore::download::{DownloadUtils, MediaType};

use super::*;
use crate::send::route::Shape;
use crate::send::stop;

/// A metadata marker no output may carry (an EXIF block's synthetic content).
const EXIF_MARKER: &[u8] = b"synthetic-gps-40719263-exif";

struct Dirs {
    _root: tempfile::TempDir,
    blobs: PathBuf,
    tmp: PathBuf,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().expect("tempdir");
    let blobs = root.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let tmp = tmp::dir_beside(&root.path().join("backend.db"));
    Dirs {
        _root: root,
        blobs,
        tmp,
    }
}

/// The core's file of `bytes`: a kourier-blob under a synthetic key in the blob directory.
fn core_attachment(dirs: &Dirs, kind: AttachmentKind, mime: &str, bytes: &[u8]) -> Attachment {
    let key = [0x3C; 32];
    let rel = PathBuf::from("ab").join("core-file-61830274");
    std::fs::create_dir_all(dirs.blobs.join("ab")).expect("subdir");
    let mut writer =
        BlobWriter::create(&dirs.blobs.join(&rel), &FileKey::from_bytes(key)).expect("writer");
    writer.write_all(bytes).expect("write");
    writer.finish_volatile().expect("finish");
    Attachment {
        kind,
        mime: mime.to_owned(),
        size: bytes.len() as u64,
        local: Some(BlobRef {
            path: rel,
            key: BlobKey::new(key),
        }),
        ..Attachment::default()
    }
}

fn halt() -> Halt {
    Halt::new(Stop::open())
}

/// What the receiver gets: the sealed file's ciphertext, decrypted under the PATH's type of the route.
fn received(prepared: &Prepared) -> Vec<u8> {
    let mut ciphertext = Vec::new();
    prepared
        .file
        .reader()
        .expect("reader")
        .read_to_end(&mut ciphertext)
        .expect("read");
    assert_eq!(ciphertext.len() as u64, prepared.len);
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(&ciphertext)),
        prepared.info.file_enc_sha256
    );
    let plain = DownloadUtils::verify_and_decrypt(
        &ciphertext,
        &prepared.info.media_key,
        prepared.route.media_type(),
    )
    .expect("decrypts under the route's type");
    assert_eq!(
        <[u8; 32]>::from(Sha256::digest(&plain)),
        prepared.info.file_sha256
    );
    assert_eq!(plain.len() as u64, prepared.info.file_length);
    plain
}

/// A JPEG with an APP1 "Exif" segment holding the marker right after its start.
fn jpeg_with_exif(width: u32, height: u32) -> Vec<u8> {
    let jpeg =
        kourier_media::sample_image(width, height, 71_402_953, kourier_media::SampleFormat::Jpeg)
            .expect("sample");
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(EXIF_MARKER);
    let len = u16::try_from(app1.len() + 2).expect("small");
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// G7 + critic L1: a still picture goes as `imageMessage` RE-ENCODED (its EXIF block gone), with the re-encode's
/// size and type, and an inline JPEG thumbnail of at most 100 px and 16 KiB, a picture with alpha too (a PNG
/// thumbnail would not show). Mutants: the original bytes sent; the thumbnail by `Decoded::thumbnail`.
#[test]
fn a_picture_is_sent_reencoded_without_its_metadata_with_a_jpeg_thumbnail() {
    let dirs = dirs();
    let original = jpeg_with_exif(640, 480);
    assert!(contains(&original, EXIF_MARKER));
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/jpeg", &original);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Image);
    assert_eq!(p.route.media_type(), MediaType::Image);
    assert_eq!(p.mime, "image/jpeg");
    assert_eq!(p.dims, Some((640, 480)));
    let plain = received(&p);
    assert!(!contains(&plain, EXIF_MARKER), "the EXIF block went");
    assert_ne!(plain, original, "re-encoded");
    let thumb = p.thumbnail.as_ref().expect("a thumbnail");
    assert_eq!(&thumb[..2], &[0xFF, 0xD8], "a JPEG");
    assert!(thumb.len() <= THUMB_MAX);
    let t = kourier_media::decode(thumb).expect("decodes");
    assert_eq!(t.width().max(t.height()), THUMB_EDGE);

    let alpha =
        kourier_media::sample_image(300, 200, 52_960_318, kourier_media::SampleFormat::PngAlpha)
            .expect("sample");
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/png", &alpha);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Image);
    let thumb = p.thumbnail.as_ref().expect("a thumbnail");
    assert_eq!(
        &thumb[..2],
        &[0xFF, 0xD8],
        "a JPEG for a picture with alpha too"
    );
    received(&p);
}

/// Whether the location look finds a location in `bytes` (LOC's look, what the core asks by).
fn located(bytes: &[u8]) -> bool {
    matches!(
        kourier_media::look(&mut std::io::Cursor::new(bytes), bytes.len() as u64),
        kourier_media::Look::Found(_)
    )
}

/// `jpeg` with its baseline frame header (SOF0) turned into an arithmetic-coded one (SOF9): no decoder here opens it.
fn arithmetic(jpeg: &[u8]) -> Vec<u8> {
    let mut out = jpeg.to_vec();
    let mut pos = 2;
    loop {
        assert_eq!(out[pos], 0xFF, "a marker at {pos}");
        assert_ne!(out[pos + 1], 0xDA, "no SOF0 before the scan");
        if out[pos + 1] == 0xC0 {
            out[pos + 1] = 0xC9;
            return out;
        }
        pos += 2 + usize::from(u16::from_be_bytes([out[pos + 2], out[pos + 3]]));
    }
}

/// W5b G7b (LOC F14, the reason WhatsApp states `cleaned_pictures`): a located picture leaves without its location or
/// not at all. A located JPEG that opens goes as a picture message, re-encoded; one no decoder here opens
/// (arithmetic-coded) is stripped and goes as a document; one that neither re-encodes nor strips fails its attachment
/// with fixed words and nothing of it is sent; a HEIC (not a cleaned type, so the core asked first) goes as it is.
/// Mutants: the code before the rebase (`scale_to_level`, then `strip_metadata` with the original as its fallback); a
/// refused picture sent as it is; a stripped picture sent as a picture message; the HEIC refused.
#[test]
fn a_located_picture_leaves_cleaned_or_not_at_all_and_one_not_cleaned_here_as_it_is() {
    let jpeg = kourier_media::sample_located(kourier_media::SampleLocated::Jpeg);
    assert!(located(&jpeg), "the sample carries a location");
    let dirs = dirs();
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/jpeg", &jpeg);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Image);
    assert!(p.thumbnail.is_some());
    assert!(!located(&received(&p)), "re-encoded without its location");

    let stripped_only = arithmetic(&jpeg);
    assert!(located(&stripped_only));
    assert!(
        kourier_media::decode(&stripped_only).is_err(),
        "no decoder here opens it"
    );
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/jpeg", &stripped_only);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(
        p.route.shape,
        Shape::Document,
        "a picture that does not open is no picture message"
    );
    assert_eq!(
        (p.mime.as_str(), p.thumbnail.as_ref()),
        ("image/jpeg", None)
    );
    let sent = received(&p);
    assert!(!located(&sent), "stripped of its location");
    assert_ne!(sent, stripped_only);

    // SOI, the GPS block, and no picture after it: nothing re-encodes or strips.
    let app1_end = 4 + usize::from(u16::from_be_bytes([jpeg[4], jpeg[5]]));
    let mut broken = jpeg[..app1_end].to_vec();
    broken.extend(b"not a marker");
    assert!(located(&broken));
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/jpeg", &broken);
    let Err(e) = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()) else {
        panic!("refused, never sent as it is");
    };
    assert_eq!(e.kind, ErrorKind::InvalidInput);
    assert_eq!(e.detail, "the picture could not be cleaned of its metadata");
    assert!(
        std::fs::read_dir(&dirs.tmp).map_or(true, |mut d| d.next().is_none()),
        "no temporary file"
    );

    let heic = kourier_media::sample_located(kourier_media::SampleLocated::Heic);
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/heic", &heic);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(
        (p.route.shape, p.mime.as_str()),
        (Shape::Document, "image/heic")
    );
    assert_eq!(
        received(&p),
        heic,
        "as it is: the core asked before it let this leave"
    );
}

/// G7 + critic H2: a picture over [`route::PICTURE_LIMIT`] is never read whole: it is streamed as a document,
/// its bytes untouched; a GIF goes as a document too (his answer). Mutant: every picture read whole.
#[test]
fn a_picture_over_the_limit_and_a_gif_are_streamed_as_documents_never_read_whole() {
    let dirs = dirs();
    let original = jpeg_with_exif(64, 48);
    let mut a = core_attachment(&dirs, AttachmentKind::Image, "image/jpeg", &original);
    a.size = route::PICTURE_LIMIT + 1;
    WHOLE_READS.with(|n| n.set(0));
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(
        WHOLE_READS.with(std::cell::Cell::get),
        0,
        "never read whole"
    );
    assert_eq!(p.route.shape, Shape::Document);
    assert_eq!(p.mime, "image/jpeg");
    assert_eq!(p.thumbnail, None);
    assert_eq!(received(&p), original, "the bytes untouched");

    let gif = kourier_media::sample_image(32, 32, 83_015_627, kourier_media::SampleFormat::Gif)
        .expect("sample");
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::Gif, "image/gif", &gif);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(WHOLE_READS.with(std::cell::Cell::get), 0);
    assert_eq!(p.route.shape, Shape::Document);
    assert_eq!(received(&p), gif);
}

/// A file sent as a file keeps its bytes and its type, under the document's keys (the path and the HKDF type are
/// one value). Mutant: the encryption under a fixed type.
#[test]
fn a_document_keeps_its_bytes_and_its_type() {
    let dirs = dirs();
    let bytes: Vec<u8> = (0..200_003u32).map(|i| (i * 7 + i / 251) as u8).collect();
    let a = core_attachment(&dirs, AttachmentKind::File, "application/pdf", &bytes);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Document);
    assert_eq!(p.mime, "application/pdf");
    assert_eq!(p.info.streaming_sidecar, None);
    assert_eq!(received(&p), bytes);
    assert_eq!(
        p.len,
        wacore::upload::encrypted_len(bytes.len()) as u64,
        "the length the upload announces"
    );
}

/// G6/G8: an H.264 MP4 is walked BEFORE it is encrypted: a video message with its seconds, its size and a
/// streaming sidecar; an Opus Ogg is an audio message with WhatsApp's mimetype. Mutant: the walk's answer dropped.
#[test]
fn an_mp4_and_an_opus_ogg_are_routed_by_what_is_inside() {
    let dirs = dirs();
    let video = crate::send::mp4::tests::video(false);
    let a = core_attachment(&dirs, AttachmentKind::Video, "video/mp4", &video);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Video);
    assert_eq!(p.seconds, Some(12));
    assert_eq!(p.dims, Some((1280, 720)));
    assert_eq!(p.mime, "video/mp4");
    assert!(p.info.streaming_sidecar.is_some());
    assert_eq!(received(&p), video);

    let mut ogg = b"OggS\0\x02".to_vec();
    ogg.extend_from_slice(&[0u8; 20]);
    ogg.push(1);
    ogg.push(19);
    ogg.extend_from_slice(b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0");
    ogg.extend_from_slice(&[0x44; 4000]);
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::Audio, "audio/ogg", &ogg);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Audio);
    assert_eq!(p.mime, "audio/ogg; codecs=opus");
    assert_eq!(received(&p), ogg);
}

/// Fix F9 (interop LOW-1): an `audio/mpeg` file is an audio message only when its first bytes are an MP3's; a
/// zero-byte `.mp3` and a text typed by its name go as documents (every other empty file does). Fix F10: an MP4 the
/// core typed as audio that holds an H.264 track goes as a video with a video type. Mutants: the sniff removed;
/// the video arm's mime back to `None`.
#[test]
fn an_mp3_is_known_by_its_bytes_and_an_h264_mp4_typed_audio_is_a_video() {
    let dirs = dirs();
    for (case, bytes, shape) in [
        (
            "id3",
            [&b"ID3\x04\0\0\0\0\0\x21"[..], &[0x55; 3000]].concat(),
            Shape::Audio,
        ),
        (
            "frame",
            [&[0xFF, 0xFB, 0x90, 0x00][..], &[0x55; 3000]].concat(),
            Shape::Audio,
        ),
        ("empty", Vec::new(), Shape::Document),
        (
            "text",
            b"synthetic text in a file called song.mp3".to_vec(),
            Shape::Document,
        ),
    ] {
        let a = core_attachment(&dirs, AttachmentKind::Audio, "audio/mpeg", &bytes);
        let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect(case);
        assert_eq!(p.route.shape, shape, "{case}");
        assert_eq!(p.mime, "audio/mpeg", "{case}");
        assert_eq!(received(&p), bytes, "{case}");
    }
    let video = crate::send::mp4::tests::video(false);
    let a = core_attachment(&dirs, AttachmentKind::Audio, "audio/mp4", &video);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Video);
    assert_eq!(p.mime, "video/mp4");
}

fn temp_files(dirs: &Dirs) -> usize {
    std::fs::read_dir(&dirs.tmp).map_or(0, Iterator::count)
}

/// G1/G2: a stop accepted before or during the prepare ends it as `stopped` (Signal's words), and no temporary file
/// stays. Mutant: the reader that does not look at the stop.
#[test]
fn a_stop_ends_the_prepare_and_leaves_no_file() {
    let dirs = dirs();
    let bytes = vec![0x61u8; 300_000];
    let a = core_attachment(&dirs, AttachmentKind::File, "application/zip", &bytes);
    let stop = Stop::open();
    let halt = Halt::new(stop.clone());
    let mut reader = Halted::new(&bytes[..], halt.clone());
    let mut first = [0u8; 10];
    assert_eq!(reader.read(&mut first).expect("open"), 10);
    stop.request();
    assert!(reader.read(&mut first).is_err(), "the next read fails");

    let err = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt).expect_err("stopped");
    assert_eq!(err.kind, stop::stopped().kind);
    assert_eq!(err.detail, stop::stopped().detail);
    assert_eq!(temp_files(&dirs), 0);

    // The attempt gone another way (its guard): the same.
    let dirs = self::dirs();
    let a = core_attachment(&dirs, AttachmentKind::File, "application/zip", &bytes);
    let gone = Halt::new(Stop::open());
    gone.end();
    assert!(prepare(&dirs.blobs, &dirs.tmp, &a, false, &gone).is_err());
    assert_eq!(temp_files(&dirs), 0);
}

/// G16 + the scout (2): the source reads the sealed file from any offset (a resume), `sent` is the furthest
/// position reached (offset + bytes read, never summed over readers), and a stop fails the next `read` AND the next
/// `reader_from`, on another thread too. The file goes when the last reader and the attempt let go. Mutants: the
/// offset not skipped; the stop not looked at in `read`.
#[test]
fn the_source_reads_from_an_offset_counts_the_furthest_position_and_stops() {
    let dirs = dirs();
    let bytes: Vec<u8> = (0..700_001u32).map(|i| (i % 253) as u8).collect();
    let a = core_attachment(
        &dirs,
        AttachmentKind::File,
        "application/octet-stream",
        &bytes,
    );
    let stop = Stop::open();
    let halt = Halt::new(stop.clone());
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt).expect("prepared");
    let mut whole = Vec::new();
    p.file
        .reader()
        .expect("reader")
        .read_to_end(&mut whole)
        .expect("read");
    let source = SealedSource::new(&p, halt.clone());
    assert_eq!(UploadSource::len(&source), p.len);
    let sent = source.sent();

    let mut from = source.reader_from(300_000).expect("from an offset");
    let mut rest = Vec::new();
    from.read_to_end(&mut rest).expect("read");
    assert_eq!(rest, whole[300_000..], "the bytes after the offset");
    assert_eq!(sent.load(Ordering::Acquire), p.len);

    let mut again = source.reader_from(0).expect("a second host");
    let mut some = [0u8; 4096];
    again.read_exact(&mut some).expect("read");
    assert_eq!(some[..], whole[..4096]);
    assert_eq!(
        sent.load(Ordering::Acquire),
        p.len,
        "the furthest position stays"
    );

    stop.request();
    let after = std::thread::spawn(move || again.read(&mut [0u8; 4096]).map(|_| ()))
        .join()
        .expect("thread");
    assert_eq!(
        after.expect_err("the next read fails").to_string(),
        "the upload was stopped"
    );
    assert!(
        source.reader_from(0).is_err(),
        "reader_from fails after a stop"
    );

    let path = p.file.path().to_path_buf();
    drop(from);
    drop(p);
    assert!(path.exists(), "the source still holds the file");
    drop(source);
    assert!(!path.exists(), "the last holder removed it");
}

// ---- B8: the upload, its reports and the stall watch (paused clock, `FakeSession`) ----

use crate::session::fake::{FAKE_MEDIA_KEY_TIMESTAMP, FakeSession, UploadHold, fake_direct_path};

/// A prepared document of `size` synthetic bytes, its stop, and the reports' receiver.
struct Upload {
    _dirs: Dirs,
    prepared: Prepared,
    stop: Stop,
    halt: Halt,
    reports: Reports,
    events: mpsc::Receiver<Envelope>,
    local: LocalMsgId,
}

fn upload_of(size: usize) -> Upload {
    let dirs = dirs();
    let bytes: Vec<u8> = (0..size).map(|i| (i % 249) as u8).collect();
    let a = core_attachment(&dirs, AttachmentKind::File, "application/x-tar", &bytes);
    let stop = Stop::open();
    let halt = Halt::new(stop.clone());
    let prepared = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt).expect("prepared");
    let (tx, events) = mpsc::channel(1024);
    let local = kourier_model::MessageId(70_351_628);
    Upload {
        _dirs: dirs,
        prepared,
        stop,
        halt,
        reports: Reports::new(tx, local),
        events,
        local,
    }
}

/// A report as the test reads it.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    File { done: u64, total: u64 },
    Share(f32, Option<SendPhase>),
}

fn seen(events: &mut mpsc::Receiver<Envelope>, local: LocalMsgId) -> Vec<Seen> {
    let mut out = Vec::new();
    while let Ok(envelope) = events.try_recv() {
        assert!(envelope.ack.is_none(), "progress is ephemeral");
        for event in envelope.events {
            match event {
                BackendEvent::AttachmentProgress {
                    local: l,
                    index,
                    done,
                    total,
                } => {
                    assert_eq!((l, index), (local, 0));
                    out.push(Seen::File { done, total });
                }
                BackendEvent::TransmitProgress {
                    local: l,
                    progress,
                    phase,
                } => {
                    assert_eq!(l, local);
                    out.push(Seen::Share(progress, phase));
                }
                other => panic!("an unexpected event {}", other.name()),
            }
        }
    }
    out
}

/// The brief §2.2 REPORTS: while the body is read the file's bytes are reported at the ticks, proportional and never
/// whole (at most `total - 1`, the share at most 0.99, `Uploading`), and ONLY once the CDN answered: the file whole,
/// then the share 1.0 with no phase, before `send_file` returns; the session got the route's upload type and the
/// sealed ciphertext. Mutants: `done` not held below the total; the final reports left out.
#[tokio::test(start_paused = true)]
async fn reports_follow_the_bytes_and_end_full() {
    let mut u = upload_of(400_000);
    let session = FakeSession::new();
    let gate = session.hold_uploads(UploadHold::AfterBytes(100_000));
    let total = u.prepared.info.file_length;
    let up = send_file(&session, &u.prepared, &u.halt, &u.reports);
    tokio::pin!(up);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), &mut up)
            .await
            .is_err()
    );
    let mid = seen(&mut u.events, u.local);
    let files: Vec<u64> = mid
        .iter()
        .filter_map(|s| match s {
            Seen::File { done, total: t } => {
                assert_eq!(*t, total, "the final total from the first report");
                Some(*done)
            }
            Seen::Share(..) => None,
        })
        .collect();
    assert!(
        !files.is_empty(),
        "reported while the body is read: {mid:?}"
    );
    assert!(files.windows(2).all(|w| w[0] < w[1]), "{files:?}");
    let last = *files.last().expect("one");
    let expected = 100_000 * u128::from(total) / u128::from(u.prepared.len);
    assert_eq!(u128::from(last), expected, "the bytes read, in plaintext");
    assert!(mid.contains(&Seen::Share(
        (last as f64 / total as f64) as f32,
        Some(SendPhase::Uploading)
    )));

    // The rest of the body is read, and the answer waits (the CDN hashes the body).
    session.move_upload_hold(UploadHold::AfterRead);
    gate.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), &mut up)
            .await
            .is_err()
    );
    let late = seen(&mut u.events, u.local);
    assert!(
        late.contains(&Seen::File {
            done: total - 1,
            total
        }),
        "the whole body read is not the file whole: {late:?}"
    );
    assert!(late.contains(&Seen::Share(0.99, Some(SendPhase::Uploading))));
    gate.notify_one();
    let uploaded = up.await.expect("uploaded");
    assert_eq!(uploaded.direct_path, fake_direct_path(1));
    assert_eq!(uploaded.media_key_timestamp, FAKE_MEDIA_KEY_TIMESTAMP);
    assert_eq!(
        seen(&mut u.events, u.local),
        vec![Seen::File { done: total, total }, Seen::Share(1.0, None)]
    );
    let uploads = session.uploads();
    assert_eq!(uploads.len(), 1);
    assert_eq!(uploads[0].shape, Shape::Document);
    let mut ciphertext = Vec::new();
    u.prepared
        .file
        .reader()
        .expect("reader")
        .read_to_end(&mut ciphertext)
        .expect("read");
    assert_eq!(uploads[0].bytes, ciphertext);
    drop(u.stop);
}

/// An upload that ends before the first tick moved anything is reported once, whole. Mutant: the final reports left
/// out.
#[tokio::test(start_paused = true)]
async fn a_quick_upload_is_reported_once_full() {
    let mut u = upload_of(70_001);
    let session = FakeSession::new();
    send_file(&session, &u.prepared, &u.halt, &u.reports)
        .await
        .expect("uploaded");
    let total = u.prepared.info.file_length;
    assert_eq!(
        seen(&mut u.events, u.local),
        vec![Seen::File { done: total, total }, Seen::Share(1.0, None)]
    );
}

/// THE STALL RULE (G12): no byte of the body read for 60 s ends the upload `Transient` with the 30 s floor, and the
/// source's next read fails (the attempt is gone). Mutant: no stall watch.
#[tokio::test(start_paused = true)]
async fn a_stalled_upload_ends_transient_after_60_s() {
    let mut u = upload_of(300_000);
    let session = FakeSession::new();
    let _gate = session.hold_uploads(UploadHold::AfterBytes(65_536));
    let start = tokio::time::Instant::now();
    // Bounded, so a missing watch fails here instead of waiting for ever on the paused clock.
    let error = tokio::time::timeout(
        UPLOAD_STALL * 2,
        send_file(&session, &u.prepared, &u.halt, &u.reports),
    )
    .await
    .expect("the watch ended it")
    .expect_err("stalled");
    let after = start.elapsed();
    assert!(
        after >= UPLOAD_STALL && after <= UPLOAD_STALL + Duration::from_secs(1),
        "{after:?}"
    );
    assert_eq!(
        error.kind,
        ErrorKind::Transient {
            retry_after: Some(STALL_RETRY)
        }
    );
    assert!(u.halt.halted(), "the reads end with the attempt");
    assert!(!u.stop.is_requested());
    assert!(
        !seen(&mut u.events, u.local).contains(&Seen::Share(1.0, None)),
        "never reported whole"
    );
    assert!(session.uploads().is_empty());
}

/// G12 (critic H1): after the body's last byte the answer may take FIVE stalls (the socket's buffer, the CDN's
/// hashing): no stall at 61 s, a stall at 300 s. The double reads EXACTLY the source's length through `take(len)`,
/// with its reader open and no final read, as ureq does with a sized body (fix F6; review HIGH: the tail never started
/// over real HTTP). Mutants: no tail allowance; the end found by an EOF read only.
#[tokio::test(start_paused = true)]
async fn after_the_last_byte_the_answer_may_take_five_stalls() {
    let u = upload_of(300_000);
    let session = FakeSession::new();
    let _gate = session.hold_uploads(UploadHold::AfterRead);
    let start = tokio::time::Instant::now();
    let up = send_file(&session, &u.prepared, &u.halt, &u.reports);
    tokio::pin!(up);
    assert!(
        tokio::time::timeout(Duration::from_secs(61), &mut up)
            .await
            .is_err(),
        "a stall at 61 s after the last byte"
    );
    let error = tokio::time::timeout(UPLOAD_STALL * UPLOAD_TAIL_STALLS, up)
        .await
        .expect("the watch ended it")
        .expect_err("stalled at last");
    let after = start.elapsed();
    assert!(
        after >= UPLOAD_STALL * UPLOAD_TAIL_STALLS
            && after <= UPLOAD_STALL * UPLOAD_TAIL_STALLS + Duration::from_secs(1),
        "{after:?}"
    );
    assert!(error.is_retryable());
}

/// Fix F6 (review HIGH/LOW): the reader is DROPPED when the request has its answer's head (ureq holds it until then),
/// and the next step (the next host's resume check, the 401's `w:m` IQ) is no stall, however long: ten minutes after a
/// dropped reader that had read the whole body there is none, and the upload is answered. Mutant: the drop leaves the
/// tail armed (`closed` only from `READING`).
#[tokio::test(start_paused = true)]
async fn a_wait_after_the_reader_is_dropped_is_no_stall() {
    let u = upload_of(300_000);
    let session = Arc::new(FakeSession::new());
    let gate = session.hold_uploads(UploadHold::AfterDrop);
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        gate.notify_one();
    });
    send_file(session.as_ref(), &u.prepared, &u.halt, &u.reports)
        .await
        .expect("uploaded after the wait");
    release.await.expect("released");
    assert_eq!(session.uploads().len(), 1);
}

/// Fix F6: the watch finds the body's end by POSITION, the resume offset included (a second host reads from its
/// offset and its reader's position is absolute), needs no read that answers 0, and goes idle when the reader is
/// dropped; a read of a non-empty buffer that answers 0 still ends the body (a client that reads to EOF). Mutants:
/// the position not compared; the offset not counted; the drop leaving the tail armed.
#[test]
fn the_watch_finds_the_end_by_position_and_goes_idle_with_the_reader() {
    let dirs = dirs();
    let bytes: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    let a = core_attachment(&dirs, AttachmentKind::File, "application/x-tar", &bytes);
    let halt = Halt::new(Stop::open());
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt).expect("prepared");
    let source = SealedSource::new(&p, halt);
    let watch = source.watch();
    let tail = Some(UPLOAD_STALL * UPLOAD_TAIL_STALLS);
    assert_eq!(watch.allowed(), None, "no reader yet");

    // Exactly `len` bytes through `take(len)`: the reader is asked nothing after the last byte.
    let mut reader = source.reader_from(0).expect("reader");
    assert_eq!(watch.allowed(), Some(UPLOAD_STALL));
    let mut some = Vec::new();
    (&mut reader)
        .take(p.len - 1)
        .read_to_end(&mut some)
        .expect("read");
    assert_eq!(watch.allowed(), Some(UPLOAD_STALL), "one byte short");
    (&mut reader).take(1).read_to_end(&mut some).expect("read");
    assert_eq!(watch.allowed(), tail, "the last byte read");
    drop(reader);
    assert_eq!(watch.allowed(), None, "the dropped reader disarms the tail");

    // A resumed host: the offset is skipped through the reader and counts in its position.
    let mut resumed = source.reader_from(250_000).expect("from an offset");
    assert_eq!(watch.allowed(), Some(UPLOAD_STALL));
    let mut rest = Vec::new();
    (&mut resumed)
        .take(p.len - 250_000)
        .read_to_end(&mut rest)
        .expect("read");
    assert_eq!(watch.allowed(), tail, "the end by absolute position");
    drop(resumed);
    assert_eq!(watch.allowed(), None);

    // A reader that does not stop at the length is still seen at its 0.
    let plain = Watch::default();
    plain.opened();
    plain.read(10, false, false);
    assert_eq!(plain.allowed(), Some(UPLOAD_STALL));
    plain.read(0, true, false);
    assert_eq!(
        plain.allowed(),
        Some(UPLOAD_STALL),
        "an empty buffer is no end"
    );
    plain.read(0, false, false);
    assert_eq!(
        plain.allowed(),
        tail,
        "a 0 for a non-empty buffer is the end"
    );
}

/// G12: before the first read (the `w:m` IQ, the resume check) nothing is watched: ten minutes there are no stall.
/// Mutant: the watch armed at the call.
#[tokio::test(start_paused = true)]
async fn a_hold_before_the_first_read_is_no_stall() {
    let u = upload_of(90_000);
    let session = Arc::new(FakeSession::new());
    let gate = session.hold_uploads(UploadHold::BeforeRead);
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(600)).await;
        gate.notify_one();
    });
    send_file(session.as_ref(), &u.prepared, &u.halt, &u.reports)
        .await
        .expect("uploaded after the hold");
    release.await.expect("released");
    assert_eq!(session.uploads().len(), 1);
}

/// G1/G2: a stop accepted while the body is read answers `stopped` AT ONCE (not at the stall), never reports the file
/// whole, and the source's next read fails. Mutant: the upload not run under the stop.
#[tokio::test(start_paused = true)]
async fn a_stop_during_the_upload_answers_stopped_at_once() {
    let mut u = upload_of(300_000);
    let session = FakeSession::new();
    let _gate = session.hold_uploads(UploadHold::AfterBytes(65_536));
    let stop = u.stop.clone();
    let start = tokio::time::Instant::now();
    let up = send_file(&session, &u.prepared, &u.halt, &u.reports);
    tokio::pin!(up);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut up)
            .await
            .is_err()
    );
    assert_eq!(stop.request(), kourier_backend_api::AbortAnswer::Stopping);
    let error = up.await.expect_err("stopped");
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(error.detail, stop::stopped().detail);
    assert!(u.halt.halted());
    assert!(!seen(&mut u.events, u.local).contains(&Seen::Share(1.0, None)));
}

/// A refused upload answers by the one error table (brief §2.6): a 413 is not tried again, a 503 is, after its floor
/// (fix F2). Mutant: the session's failure not read by the table.
#[tokio::test(start_paused = true)]
async fn a_refused_upload_answers_by_the_error_table() {
    let u = upload_of(80_000);
    let session = FakeSession::new();
    session.script_upload(Err(crate::session::UploadFailure::Status(413)));
    session.script_upload(Err(crate::session::UploadFailure::Status(503)));
    let refused = send_file(&session, &u.prepared, &u.halt, &u.reports)
        .await
        .expect_err("refused");
    assert_eq!(refused.kind, ErrorKind::Unsupported);
    assert_eq!(
        refused.detail,
        "the media server refused the file (code 413)"
    );
    let u = upload_of(80_000);
    let failed = send_file(&session, &u.prepared, &u.halt, &u.reports)
        .await
        .expect_err("failed");
    assert_eq!(
        failed.kind,
        ErrorKind::Transient {
            retry_after: Some(STALL_RETRY)
        }
    );
}

/// B10: an `audioMessage` carries no caption, so a CAPTIONED file whose route is audio goes as a document (its own
/// mimetype, encrypted as a document), and the pointer of the upload says the caption wrapper; an uncaptioned picture's
/// pointer holds the re-encode's facts and the upload's answer. Mutants: the captioned rule removed; the wrapper shape
/// not written.
#[test]
fn a_captioned_file_that_would_be_audio_goes_as_a_document_and_its_pointer_says_so() {
    let mut ogg = b"OggS\0\x02".to_vec();
    ogg.extend_from_slice(&[0u8; 20]);
    ogg.push(1);
    ogg.push(19);
    ogg.extend_from_slice(b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0");
    ogg.extend_from_slice(&[0x45; 3000]);
    let dirs = dirs();
    let a = core_attachment(&dirs, AttachmentKind::Audio, "audio/ogg", &ogg);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, true, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Document);
    assert_eq!(p.mime, "audio/ogg");
    assert_eq!(received(&p), ogg);
    let uploaded = Uploaded {
        url: "https://synthetic.invalid/upload-63019482".into(),
        direct_path: "/v/t62.7119-24/synthetic-upload-74120593".into(),
        media_key_timestamp: 1_761_400_002,
    };
    let pointer = p.pointer(&uploaded, true);
    assert_eq!(pointer.media_type, "document");
    assert_eq!(
        pointer.shape.as_deref(),
        Some(crate::media::CAPTIONED_DOCUMENT)
    );
    assert_eq!(pointer.direct_path, uploaded.direct_path);
    assert_eq!(pointer.url.as_deref(), Some(uploaded.url.as_str()));
    assert_eq!(pointer.media_key, crate::media::hex(&p.info.media_key));
    assert_eq!(pointer.file_length, p.info.file_length);
    assert_eq!(pointer.mimetype.as_deref(), Some("audio/ogg"));
    assert_eq!(
        p.pointer(&uploaded, false).shape.as_deref(),
        Some("document")
    );

    let dirs = self::dirs();
    let picture = jpeg_with_exif(300, 200);
    let a = core_attachment(&dirs, AttachmentKind::Image, "image/jpeg", &picture);
    let p = prepare(&dirs.blobs, &dirs.tmp, &a, true, &halt()).expect("prepared");
    assert_eq!(p.route.shape, Shape::Image);
    let pointer = p.pointer(&uploaded, true);
    assert_eq!(pointer.shape.as_deref(), Some("image"));
    assert_eq!(pointer.mimetype.as_deref(), Some(p.mime.as_str()));
    assert_eq!(pointer.width.zip(pointer.height), p.dims);
    assert_eq!(
        pointer.thumb_jpeg,
        p.thumbnail.as_deref().map(crate::media::hex)
    );
    assert_eq!(
        pointer.file_sha256,
        Some(crate::media::hex(&p.info.file_sha256))
    );
    assert_eq!(pointer.media_key_timestamp, Some(1_761_400_002));
}

/// Fix F5: a message's entry in `Lingering` lasts as long as ANY reader of its source lives (a second host's reader
/// opens before the first one's thread is gone); another message's entry is its own. Mutant: the entry removed at the
/// first reader's end.
#[test]
fn lingering_lasts_until_the_last_reader_of_the_message_is_gone() {
    let set = Lingering::default();
    let (one, other) = (kourier_model::MessageId(1), kourier_model::MessageId(2));
    assert!(!set.holds(one));
    let first = set.enter(one);
    let second = set.enter(one);
    assert!(set.holds(one) && !set.holds(other));
    drop(first);
    assert!(set.holds(one), "a reader of the message is still open");
    drop(second);
    assert!(!set.holds(one));
    let own = set.enter(other);
    assert!(set.holds(other) && !set.holds(one));
    drop(own);
    assert!(!set.holds(other));
}

/// What the `STALL_RETRY` answer of a moment of the machine looks like (fix F18), and the answer of a file that will
/// not read.
fn is_busy(e: &BackendError) -> bool {
    e.kind
        == ErrorKind::Transient {
            retry_after: Some(STALL_RETRY),
        }
}

/// Fix F18 (robustness LOW), the read of the core's file that fails MID-READ: an I/O error of the machine (out of file
/// descriptors, an I/O error, an interrupted call) is `Transient{STALL_RETRY}` from both [`read_failed`] and
/// [`encrypt_failed`]; a missing file and the blob crate's own verdicts (a tag that does not verify, a truncated file)
/// stay `InvalidInput`. Mutant: every kind back to `InvalidInput`.
#[test]
fn a_moment_of_the_machine_is_tried_again_and_a_file_that_will_not_read_is_refused() {
    struct Failing(fn() -> io::Error);
    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err((self.0)())
        }
    }
    type Case = (&'static str, fn() -> io::Error);
    let machine: [Case; 4] = [
        ("EMFILE", || io::Error::from_raw_os_error(24)),
        ("ENFILE", || io::Error::from_raw_os_error(23)),
        ("EIO", || io::Error::from_raw_os_error(5)),
        ("Interrupted", || io::ErrorKind::Interrupted.into()),
    ];
    let the_file: [Case; 4] = [
        ("missing", || io::ErrorKind::NotFound.into()),
        ("tag", || {
            io::Error::new(io::ErrorKind::InvalidData, kourier_blob::BlobError::Corrupt)
        }),
        ("cut", || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                kourier_blob::BlobError::Truncated,
            )
        }),
        ("foreign", || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                kourier_blob::BlobError::NotABlob,
            )
        }),
    ];
    for (case, make) in machine {
        let mut reader = Halted::new(Failing(make), halt());
        assert!(reader.read(&mut [0u8; 8]).is_err(), "{case}");
        assert!(is_busy(&read_failed(&reader)), "{case}: a read");
        assert!(is_busy(&encrypt_failed(&reader)), "{case}: an encryption");
    }
    for (case, make) in the_file {
        let mut reader = Halted::new(Failing(make), halt());
        assert!(reader.read(&mut [0u8; 8]).is_err(), "{case}");
        assert_eq!(read_failed(&reader).kind, ErrorKind::InvalidInput, "{case}");
        assert_eq!(
            encrypt_failed(&reader).kind,
            ErrorKind::InvalidInput,
            "{case}: an encryption"
        );
    }
}

/// Fix F18, the same through [`prepare`] at the two places that open the core's file (the streamed route and the
/// picture read whole): a directory where the file should be (the operating system's error on the read) is
/// `Transient{STALL_RETRY}`; a file that is not there and a wrong key (the tag does not verify) are `InvalidInput`. No
/// temporary file stays. Mutant: every kind back to `InvalidInput` ([`unreadable_by`] answers `unreadable()`).
#[test]
fn prepare_answers_a_moment_of_the_machine_transient_and_a_missing_or_wrong_file_invalid() {
    let streamed = (
        AttachmentKind::File,
        "application/zip",
        vec![0x61u8; 70_000],
    );
    let picture = (
        AttachmentKind::Image,
        "image/jpeg",
        kourier_media::sample_image(64, 48, 40_618_259, kourier_media::SampleFormat::Jpeg)
            .expect("sample"),
    );
    for (kind, mime, bytes) in [streamed, picture] {
        let case = format!("{kind:?}");
        // The file is a directory: opening works, reading it is the operating system's error.
        let dirs = dirs();
        let mut a = core_attachment(&dirs, kind, mime, &bytes);
        let path = dirs.blobs.join(&a.local.as_ref().expect("a file").path);
        std::fs::remove_file(&path).expect("remove");
        std::fs::create_dir(&path).expect("a directory in its place");
        let e = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect_err("unreadable");
        assert!(is_busy(&e), "{case}: a directory: {:?}", e.kind);
        assert_eq!(temp_files(&dirs), 0, "{case}");
        // The file is not there.
        std::fs::remove_dir(&path).expect("remove the directory");
        let e = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect_err("missing");
        assert_eq!(e.kind, ErrorKind::InvalidInput, "{case}: missing");
        // The key is wrong: the first chunk's tag does not verify.
        let dirs = self::dirs();
        a = core_attachment(&dirs, kind, mime, &bytes);
        a.local.as_mut().expect("a file").key = BlobKey::new([0x4D; 32]);
        let e = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect_err("wrong key");
        assert_eq!(e.kind, ErrorKind::InvalidInput, "{case}: wrong key");
        assert_eq!(temp_files(&dirs), 0, "{case}");
    }
}

/// Re-check N3 (robustness LOW): a temporary file that cannot be written (a full disk, a folder that cannot be made) is
/// `Transient{STALL_RETRY}`, never an immediate retry: every try reads and encrypts the whole file (up to 2 GB) before
/// the disk fails it again. [`prepare`] with a regular file where the temporary folder should be fails at the file's
/// creation (`tmp::failed`); a write that fails after good reads ends in [`encrypt_failed`]'s last arm. Mutants: either
/// answer back to `BackendError::transient` (no floor).
#[test]
fn a_temporary_file_that_cannot_be_written_is_tried_again_after_the_floor() {
    let dirs = dirs();
    let a = core_attachment(
        &dirs,
        AttachmentKind::File,
        "application/zip",
        &[0x61u8; 70_000],
    );
    std::fs::write(&dirs.tmp, b"x").expect("a file where the folder should be");
    let e = prepare(&dirs.blobs, &dirs.tmp, &a, false, &halt()).expect_err("no folder");
    assert!(is_busy(&e), "the folder cannot be made: {:?}", e.kind);
    let reader = Halted::new(&b"synthetic"[..], halt());
    let e = encrypt_failed(&reader);
    assert!(
        is_busy(&e),
        "the write failed after good reads: {:?}",
        e.kind
    );
}
