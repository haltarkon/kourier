//! W5b chunks B14 and B15 (brief §5 WIRE, decisions G6, G18, G20, G21, G22): Kourier's MEDIA SEND end to end over the
//! scripted server and the CDN fake. The core's `Transmit` of a message with one file reaches the actor; the attempt
//! reads the core's sealed file, re-encodes a still picture, routes and encrypts it, uploads it through upstream's
//! `upload_stream` to the CDN fake (`support::cdn`, the clients' HTTP client) after the `w:m` IQ the scripted server
//! answers (`wire::media_conn`), records it, and writes ONE `<message>`; the peer reads the frame and DOWNLOADS what
//! it received the way a receiving phone does (G18: the media part of the RECEIVED message, its blob by
//! `direct_path`, both sha256 values, `file_length`, a mimetype, the key type from the message's own shape).
//!
//! Cases: a picture (re-encoded: the peer's plaintext holds no EXIF block of the original; its size and inline
//! thumbnail; `/mms/image`; the frame written only after the POST's body was read), a captioned document (the bytes
//! unchanged, its name, the caption in the wrapper), a video (seconds and size from the box walk, a streaming
//! sidecar, no thumbnail), a picture in a group (one `skmsg`, the member reads it with the sender key that came
//! along), an Opus Ogg as an `audioMessage` (`ptt` false, `mediatype="audio"`, the sidecar; the peer downloads under
//! the AUDIO keys the received shape names) and an UNCAPTIONED document (`documentMessage`, `/mms/document`; fix F12). The 401-twice and the 500 cases are CHARACTERISATION tests of upstream's host loop (decision G21: one
//! forced `media_conn` refetch after a 401, the next host after a 500); what Kourier adds is the errors table's
//! answer (`send::errors::upload_error`: Transient, no frame) and that the resend then goes. Chunk B15 adds the
//! stop pressed while the CDN reads the POST (Stopping, the reader's error ends the POST, no frame, no record, no
//! file: G1, G2) and the resend after a lost ack (no second upload, the same id and the same media fields: G4).
//!
//! Every body runs in ONE offline child under `unshare -rn` (KOURIER_REQUIRE_UNSHARE=1); the parent checks that
//! nothing the child printed holds a secret, the child that nothing it logged does (D10, G20: the CDN's auth, url,
//! direct path and failure-body markers, the captions, the file name, the group). Every value is SYNTHETIC: pictures
//! from `kourier_media::sample_image`, an MP4 of generated boxes, bytes of a pattern.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use kourier_backend_api::{AbortAnswer, OutgoingKind, OutgoingRequest, TransmitOutcome};
use kourier_model::{
    Attachment, AttachmentKind, Body, ConvKey, ErrorKind, MsgTarget, TargetAuthor, Text,
};
use wacore::download::MediaType;
use wacore_binary::Node;

use crate::receive::{account_dir, count, raw};
use crate::send_wire::{Rig, Script, now_secs, send_secrets, server_of};
use crate::support::capture::Capture;
use crate::support::cdn::{self, BodyEnd, Cdn, Reply};
use crate::support::peer::{self, Peer};
use crate::support::wire::Ack;
use crate::support::{self, wire};

/// The ids the core gives its messages here (none a substring of another or of the other modules' ids).
const PICTURE_ID: &str = "3EB0C0DE5D1A2B3C";
const DOCUMENT_ID: &str = "3EB0C0DE5D2B3C4D";
const VIDEO_ID: &str = "3EB0C0DE5D3C4D5E";
const GROUP_PICTURE_ID: &str = "3EB0C0DE5D4D5E6F";
const AUTH_ID: &str = "3EB0C0DE5D5E6F70";
const FAILED_ID: &str = "3EB0C0DE5D6F7081";
const STOPPED_ID: &str = "3EB0C0DE5D708192";
const RESENT_ID: &str = "3EB0C0DE5D8192A3";
const QUIT_ID: &str = "3EB0C0DE5D92A3B4";
const AUDIO_ID: &str = "3EB0C0DE5DA3B4C5";
const PLAIN_DOCUMENT_ID: &str = "3EB0C0DE5DB4C5D6";
const REVOKED_ID: &str = "3EB0C0DE5DC5D6E7";
const REVOKE_ID: &str = "3EB0C0DE5DD6E7F8";
/// The captions, the document's name and a group (D10: secrets of this module).
const PICTURE_CAPTION: &str = "synthetic picture caption 70461938";
const DOCUMENT_CAPTION: &str = "synthetic document caption 59102837";
const DOCUMENT_NAME: &str = "synthetic-report-28374619.pdf";
const PLAIN_NAME: &str = "synthetic-notes-50293817.txt";
const GROUP: &str = "120363000081726354";
/// Inside the original picture's EXIF block (where a phone writes the place a photo was taken): never sent.
const EXIF_MARKER: &str = "synthetic-exif-gps-64019283";

/// The secrets of this module: the send module's (the fixture's and the CDN fake's) and its own.
fn media_secrets() -> Vec<&'static str> {
    let mut all = send_secrets();
    all.extend([
        PICTURE_CAPTION,
        DOCUMENT_CAPTION,
        DOCUMENT_NAME,
        PLAIN_NAME,
        GROUP,
        EXIF_MARKER,
    ]);
    all
}

/// The core's sealed copy of a file: `bytes` as a kourier-blob in the account's blob directory (`dir/blobs`, the
/// context's), named `name`, under a key made here.
pub(crate) fn core_file(
    dir: &Path,
    name: &str,
    bytes: &[u8],
    kind: AttachmentKind,
    mime: &str,
) -> Attachment {
    let path = dir.join("blobs").join(Path::new("w5").join(name));
    std::fs::create_dir_all(path.parent().expect("a parent")).expect("the blob directory");
    let mut writer =
        kourier_blob::BlobWriter::create(&path, &kourier_blob::FileKey::from_bytes(CORE_KEY))
            .expect("a writer");
    writer.write_all(bytes).expect("written");
    writer.finish_volatile().expect("finished");
    core_attachment(name, bytes.len() as u64, kind, mime)
}

/// The key of every sealed copy [`core_file`] makes.
const CORE_KEY: [u8; 32] = [0x3C; 32];

/// The core's attachment of a sealed copy [`core_file`] made (named `name`, `size` bytes).
pub(crate) fn core_attachment(
    name: &str,
    size: u64,
    kind: AttachmentKind,
    mime: &str,
) -> Attachment {
    Attachment {
        kind,
        mime: mime.into(),
        size,
        local: Some(kourier_model::BlobRef {
            path: Path::new("w5").join(name),
            key: kourier_model::BlobKey::new(CORE_KEY),
        }),
        ..Attachment::default()
    }
}

/// A synthetic JPEG with an EXIF block (an empty TIFF directory followed by [`EXIF_MARKER`]) right after its start.
fn picture_with_exif(width: u32, height: u32, seed: u64) -> Vec<u8> {
    let plain = kourier_media::sample_image(width, height, seed, kourier_media::SampleFormat::Jpeg)
        .expect("a sample picture");
    assert_eq!(&plain[..2], &[0xFF, 0xD8], "a JPEG starts with SOI");
    let mut segment = b"Exif\0\0II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec();
    segment.extend_from_slice(EXIF_MARKER.as_bytes());
    let len = u16::try_from(segment.len() + 2).expect("a small segment");
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&segment);
    out.extend_from_slice(&plain[2..]);
    out
}

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

/// A generated MP4: an H.264 video track of 1280x720 and an AAC sound track, 12.4 seconds, `moov` first.
fn video_mp4() -> Vec<u8> {
    let mut mvhd = vec![0u8; 12];
    mvhd.extend_from_slice(&1000u32.to_be_bytes());
    mvhd.extend_from_slice(&12_400u32.to_be_bytes());
    mvhd.extend_from_slice(&[0u8; 80]);
    let trak = |handler: &[u8; 4], codec: &[u8; 4], size: Option<(u32, u32)>| {
        let mut hdlr = vec![0u8; 8];
        hdlr.extend_from_slice(handler);
        hdlr.extend_from_slice(&[0u8; 13]);
        let mut stsd = vec![0u8; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&bx(codec, &[0u8; 16]));
        let minf = bx(b"minf", &bx(b"stbl", &bx(b"stsd", &stsd)));
        let mdia = bx(b"mdia", &[bx(b"hdlr", &hdlr), minf].concat());
        let mut parts = Vec::new();
        if let Some((w, h)) = size {
            let mut tkhd = vec![0u8; 4 + 20 + 16];
            let one = 0x0001_0000u32;
            for m in [one, 0, 0, 0, one, 0, 0, 0, 0x4000_0000] {
                tkhd.extend_from_slice(&m.to_be_bytes());
            }
            tkhd.extend_from_slice(&(w << 16).to_be_bytes());
            tkhd.extend_from_slice(&(h << 16).to_be_bytes());
            parts.push(bx(b"tkhd", &tkhd));
        }
        parts.push(mdia);
        bx(b"trak", &parts.concat())
    };
    let moov = bx(
        b"moov",
        &[
            bx(b"mvhd", &mvhd),
            trak(b"vide", b"avc1", Some((1280, 720))),
            trak(b"soun", b"mp4a", None),
        ]
        .concat(),
    );
    let mdat = bx(b"mdat", &pattern(300_000, 0x5A));
    [bx(b"ftyp", b"isom\0\0\0\0isomavc1"), moov, mdat].concat()
}

/// `n` bytes of a pattern seeded by `seed`.
fn pattern(n: usize, seed: u8) -> Vec<u8> {
    (0..n)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

/// The core's message with one file and its caption.
pub(crate) fn file_request(
    conv: ConvKey,
    local: &str,
    file: Attachment,
    caption: Option<&str>,
) -> OutgoingRequest {
    OutgoingRequest {
        conv,
        local: local.into(),
        sent_at: i64::try_from(now_secs() * 1000).expect("time"),
        kind: OutgoingKind::Message {
            body: Body::Media {
                attachments: vec![file],
                caption: caption.map(Text::plain),
            },
            reply_to: None,
            expiry: None,
        },
        only: None,
        is_recipient_update: false,
    }
}

/// The 1:1 chat with the peer.
pub(crate) fn chat() -> ConvKey {
    ConvKey::Direct(
        kourier_model::UserId::from_parts("lid", peer::PEER_LID).expect("the peer's id"),
    )
}

/// The scripted server of these cases: the send module's (our devices, the phone's setting, the reach-out lock,
/// every frame acked) with the `w:m` rule (two hosts, five minutes).
pub(crate) fn media_server_of(script: &Script) -> wire::Wire {
    let server = server_of(script);
    server.on_iq(
        "w:m",
        wire::media_conn(cdn::AUTH, &[cdn::HOST_A, cdn::HOST_B], 300),
    );
    server
}

/// [`media_server_of`] over the send module's acked script.
fn media_server() -> wire::Wire {
    media_server_of(&Script::acked())
}

/// The started rig: online, the CDN fake as the HTTP client, the peer has written first (D7).
async fn started(
    dir: &Path,
    device: &wacore::store::Device,
    server: wire::Wire,
    fake: &Cdn,
) -> (Rig, Peer) {
    let rig = Rig::start_with_cdn(dir, server, fake).await;
    let mut peer = Peer::new(device, None).await;
    rig.peer_writes_first(&mut peer).await;
    (rig, peer)
}

fn attr(node: &Node, key: &str) -> Option<String> {
    node.attrs.get(key).map(|v| v.as_str().into_owned())
}

/// The `mediatype` of the `<enc>` the peer's `<to>` carries.
fn to_mediatype(frame: &Node) -> Option<String> {
    let enc = frame
        .get_optional_child("participants")?
        .get_children_by_tag("to")
        .next()?
        .get_optional_child("enc")?;
    attr(enc, "mediatype")
}

fn sent(outcome: &TransmitOutcome) -> bool {
    matches!(outcome, TransmitOutcome::Sent { .. })
}

/// The temporary upload files left in the account's directory (none once every attempt ended).
fn temporary(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("upload-tmp")).map_or(0, |d| d.count())
}

/// The one POST of an upload that went through whole, at `path` on host A.
fn one_whole_post(fake: &Cdn, path: &str) {
    let posts = fake.posts();
    assert_eq!(posts.len(), 1, "one POST for one file");
    let post = &posts[0];
    assert_eq!(post.host, cdn::HOST_A);
    assert_eq!(post.path, path);
    assert!(
        post.token_in_path && post.auth_ok,
        "token or auth not as the media connection said"
    );
    assert_eq!(post.read, BodyEnd::Whole);
    assert_eq!(
        post.received, post.content_length,
        "the body is the declared length"
    );
    assert_eq!(post.status, Some(200));
}

/// A file sealed as a phone seals it, planted on the fake servers, and the `remote` of the message that names it.
fn file_on_the_servers(
    fake: &Cdn,
    direct_path: &str,
    plain: &[u8],
    media_type: MediaType,
    name: &str,
    tamper: bool,
) -> kourier_model::BackendBlob {
    let sealed = wacore::upload::encrypt_media(plain, media_type).expect("sealed");
    // What a message from a phone would carry: the log check of `media_child` looks for these too.
    cdn::learn(Some(&sealed.media_key[..]));
    cdn::learn(Some(&sealed.file_sha256[..]));
    cdn::learn(Some(&sealed.file_enc_sha256[..]));
    let mut data = sealed.data_to_upload.clone();
    if tamper {
        data[0] ^= 1;
    }
    fake.plant(direct_path, data);
    let pointer = serde_json::json!({
        "media_type": name,
        "direct_path": direct_path,
        "media_key": hex(&sealed.media_key),
        "file_sha256": hex(&sealed.file_sha256),
        "file_enc_sha256": hex(&sealed.file_enc_sha256),
        "file_length": plain.len() as u64,
    });
    kourier_model::BackendBlob::new(1, serde_json::to_vec(&pointer).expect("json"))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The core's target of a download in `dir`.
fn target(dir: &Path) -> kourier_backend_api::BlobTarget {
    kourier_backend_api::BlobTarget {
        path: dir.join("blobs/core/ab/synthetic-target"),
        key: kourier_model::BlobKey::new([0x42; 32]),
    }
}

/// W7: a file a phone sent comes through upstream's real download path (a `w:m` IQ, a GET, the MAC, the two hashes, the
/// decryption) into the core's sealed blob, whole, and no temporary file is left.
async fn a_file_from_a_phone_arrives_sealed_and_whole() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, _peer) = started(dir.path(), &device, media_server(), &fake).await;
    let plain = pattern(300_000, 31);
    let remote = file_on_the_servers(
        &fake,
        "/v/t62.7114-24/synthetic-download-58102937",
        &plain,
        MediaType::Document,
        "document",
        false,
    );
    let dest = target(dir.path());
    let meta = rig
        .download(remote, dest.clone())
        .await
        .expect("downloaded");
    assert_eq!(meta.size, plain.len() as u64);
    let key = kourier_blob::FileKey::from_bytes(*dest.key.expose());
    assert!(
        *kourier_blob::read_all(&dest.path, &key).expect("a sealed file") == *plain,
        "the file differs"
    );
    assert_eq!(fake.gets().len(), 1, "one GET for one file");
    let parent = dest.path.parent().expect("a directory");
    let left = std::fs::read_dir(parent).expect("listed").count();
    assert_eq!(left, 1, "only the file is left");
    rig.finish().await;
}

/// W7: a file that fails its hashes never appears at the target, on any host, and can be tried again.
async fn a_file_that_does_not_verify_never_appears() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, _peer) = started(dir.path(), &device, media_server(), &fake).await;
    let remote = file_on_the_servers(
        &fake,
        "/v/t62.7118-24/synthetic-download-69213048",
        &pattern(40_000, 47),
        MediaType::Image,
        "image",
        true,
    );
    let dest = target(dir.path());
    let error = rig
        .download(remote, dest.clone())
        .await
        .expect_err("refused");
    assert!(error.is_retryable(), "{error:?}");
    assert!(!dest.path.exists(), "an unverified file is in place");
    assert_eq!(fake.gets().len(), 2, "both hosts were tried");
    rig.finish().await;
}

/// W7: a reference the servers do not know (any more) is `Expired` at once: a 404 is final, the library asks for no
/// fresh `media_conn`.
async fn a_file_the_servers_do_not_have_is_expired() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, _peer) = started(dir.path(), &device, media_server(), &fake).await;
    let remote = file_on_the_servers(
        &fake,
        "/v/t62.7118-24/synthetic-download-70324159",
        &pattern(4_000, 53),
        MediaType::Image,
        "image",
        false,
    );
    let pointer: serde_json::Value = serde_json::from_slice(&remote.bytes).expect("json");
    let mut moved = pointer.clone();
    moved["direct_path"] = "/v/t62.7118-24/synthetic-elsewhere-81435260".into();
    let remote = kourier_model::BackendBlob::new(1, serde_json::to_vec(&moved).expect("json"));
    let dest = target(dir.path());
    let error = rig.download(remote, dest.clone()).await.expect_err("gone");
    assert!(matches!(error.kind, ErrorKind::Expired), "{error:?}");
    assert!(!dest.path.exists());
    assert_eq!(rig.server.answered("w:m"), 1, "a 404 is final");
    rig.finish().await;
}

/// `a_picture_is_uploaded_then_sent_and_the_peer_decrypts_it`: ONE `w:m` IQ, ONE POST to `/mms/image` with its exact
/// length, no `<message>` while the CDN reads the body, then ONE `type="media"` frame whose `<enc>` says
/// `mediatype="image"`; the peer's download (G18) gives the RE-ENCODE: a JPEG without the original's EXIF block, its
/// size, a JPEG thumbnail of at most 16 KiB, the caption. No temporary file is left.
async fn a_picture_is_uploaded_then_sent_and_the_peer_decrypts_it() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let server = media_server();
    let frames_during_post = Arc::new(AtomicUsize::new(usize::MAX));
    let (seen, watched) = (frames_during_post.clone(), server.clone());
    fake.reply(Reply::CallAt(
        1024,
        Box::new(move || {
            let n = watched
                .frame_nodes()
                .iter()
                .filter(|n| n.tag == "message" && attr(n, "id").as_deref() == Some(PICTURE_ID))
                .count();
            seen.store(n, Ordering::SeqCst);
        }),
    ));
    let (rig, mut peer) = started(dir.path(), &device, server, &fake).await;
    let original = picture_with_exif(320, 240, 47_103_928);
    let file = core_file(
        dir.path(),
        "picture-81920374",
        &original,
        AttachmentKind::Image,
        "image/jpeg",
    );

    let outcome = rig
        .transmit(&file_request(
            chat(),
            PICTURE_ID,
            file,
            Some(PICTURE_CAPTION),
        ))
        .await;
    assert!(sent(&outcome), "the picture is not answered Sent");
    assert_eq!(rig.server.answered("w:m"), 1, "one media connection asked");
    one_whole_post(&fake, "/mms/image");
    assert_eq!(
        frames_during_post.load(Ordering::SeqCst),
        0,
        "a frame was written while the CDN still read the upload"
    );
    let frames = rig.message_frames(PICTURE_ID);
    assert_eq!(frames.len(), 1, "one frame for one picture");
    assert_eq!(attr(&frames[0], "type").as_deref(), Some("media"));
    assert_eq!(to_mediatype(&frames[0]).as_deref(), Some("image"));

    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let image = read.image_message.as_option().expect("a picture message");
    assert!(
        image.caption.as_deref() == Some(PICTURE_CAPTION),
        "another caption"
    );
    assert_eq!((image.width, image.height), (Some(320), Some(240)));
    assert_eq!(image.mimetype.as_deref(), Some("image/jpeg"));
    let thumbnail = image
        .jpeg_thumbnail
        .as_deref()
        .expect("an inline thumbnail");
    assert!(thumbnail.starts_with(&[0xFF, 0xD8]) && thumbnail.len() <= 16 * 1024);
    let (kind, plain) = fake
        .download(&read)
        .expect("the peer downloads the picture");
    assert_eq!(kind, MediaType::Image);
    assert!(plain.starts_with(&[0xFF, 0xD8]), "the picture is no JPEG");
    let marker = EXIF_MARKER.as_bytes();
    assert!(original.windows(marker.len()).any(|w| w == marker));
    assert!(
        !plain.windows(marker.len()).any(|w| w == marker),
        "the original's EXIF block was sent"
    );
    assert!(
        plain != original,
        "the picture was sent as it was, not re-encoded"
    );
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");
    rig.finish().await;
}

/// `a_document_keeps_its_bytes_and_its_name`: a PDF with a caption goes to `/mms/document` and as a
/// `documentWithCaptionMessage` (`mediatype="document"`): the peer's download is byte-identical, the inner document
/// carries the file's name, its type and the caption.
async fn a_document_keeps_its_bytes_and_its_name() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    let mut bytes = b"%PDF-1.4\n".to_vec();
    bytes.extend(pattern(200_000, 0x17));
    let file = Attachment {
        file_name: Some(DOCUMENT_NAME.into()),
        ..core_file(
            dir.path(),
            "document-50918273",
            &bytes,
            AttachmentKind::File,
            "application/pdf",
        )
    };

    let outcome = rig
        .transmit(&file_request(
            chat(),
            DOCUMENT_ID,
            file,
            Some(DOCUMENT_CAPTION),
        ))
        .await;
    assert!(sent(&outcome), "the document is not answered Sent");
    one_whole_post(&fake, "/mms/document");
    let frames = rig.message_frames(DOCUMENT_ID);
    assert_eq!(frames.len(), 1, "one frame for one document");
    assert_eq!(to_mediatype(&frames[0]).as_deref(), Some("document"));
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let document = read
        .document_with_caption_message
        .as_option()
        .and_then(|w| w.message.as_option())
        .and_then(|m| m.document_message.as_option())
        .expect("a document in its caption wrapper");
    assert!(
        document.file_name.as_deref() == Some(DOCUMENT_NAME),
        "another file name"
    );
    assert!(
        document.caption.as_deref() == Some(DOCUMENT_CAPTION),
        "another caption"
    );
    assert_eq!(document.mimetype.as_deref(), Some("application/pdf"));
    let (kind, plain) = fake
        .download(&read)
        .expect("the peer downloads the document");
    assert_eq!(kind, MediaType::Document);
    assert!(plain == bytes, "the document's bytes changed");
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");
    rig.finish().await;
}

/// `a_video_carries_its_seconds_and_sidecar`: an H.264 MP4 goes to `/mms/video` as a `videoMessage`
/// (`mediatype="video"`) with the seconds and the size the box walk read, a streaming sidecar and no thumbnail (G8);
/// the peer's download is the file unchanged.
async fn a_video_carries_its_seconds_and_sidecar() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    let bytes = video_mp4();
    let file = core_file(
        dir.path(),
        "video-63019284",
        &bytes,
        AttachmentKind::Video,
        "video/mp4",
    );

    let outcome = rig
        .transmit(&file_request(chat(), VIDEO_ID, file, None))
        .await;
    assert!(sent(&outcome), "the video is not answered Sent");
    one_whole_post(&fake, "/mms/video");
    let frames = rig.message_frames(VIDEO_ID);
    assert_eq!(frames.len(), 1, "one frame for one video");
    assert_eq!(to_mediatype(&frames[0]).as_deref(), Some("video"));
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let video = read.video_message.as_option().expect("a video message");
    assert_eq!(video.seconds, Some(12));
    assert_eq!((video.width, video.height), (Some(1280), Some(720)));
    assert_eq!(video.mimetype.as_deref(), Some("video/mp4"));
    assert!(
        video
            .streaming_sidecar
            .as_deref()
            .is_some_and(|s| !s.is_empty()),
        "no streaming sidecar"
    );
    assert!(
        video.jpeg_thumbnail.is_none(),
        "a video carries no thumbnail yet"
    );
    let (kind, plain) = fake.download(&read).expect("the peer downloads the video");
    assert_eq!(kind, MediaType::Video);
    assert!(plain == bytes, "the video's bytes changed");
    rig.finish().await;
}

/// `a_group_picture_is_one_sender_key_message` (G22): a picture in a group is uploaded once and written as ONE
/// `type="media"` stanza to the group whose `skmsg` says `mediatype="image"`; the member reads it with the sender key
/// that came along in its `<to>` and downloads the picture.
async fn a_group_picture_is_one_sender_key_message() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let server = media_server();
    let own: &'static str = format!("{}@lid", peer::ACCOUNT_LID).leak();
    let member: &'static str = format!("{}@lid", peer::PEER_LID).leak();
    server.on_iq(
        "w:g2",
        wire::group_info(
            GROUP,
            "lid",
            vec![(own, Some("superadmin")), (member, None)],
        ),
    );
    let (rig, mut peer) = started(dir.path(), &device, server, &fake).await;
    let original = picture_with_exif(200, 160, 81_726_354);
    let file = core_file(
        dir.path(),
        "group-picture-29384756",
        &original,
        AttachmentKind::Image,
        "image/jpeg",
    );
    let group = ConvKey::Group(format!("wag:{GROUP}").into());

    let outcome = rig
        .transmit(&file_request(group, GROUP_PICTURE_ID, file, None))
        .await;
    assert!(sent(&outcome), "the group picture is not answered Sent");
    one_whole_post(&fake, "/mms/image");
    let frames = rig.message_frames(GROUP_PICTURE_ID);
    assert_eq!(frames.len(), 1, "a group picture is not ONE stanza");
    let frame = &frames[0];
    let group_jid = format!("{GROUP}@g.us");
    assert!(
        attr(frame, "to").as_deref() == Some(group_jid.as_str()),
        "not written to the group"
    );
    assert_eq!(attr(frame, "type").as_deref(), Some("media"));
    let skmsg: Vec<&Node> = frame
        .get_children_by_tag("enc")
        .filter(|enc| attr(enc, "type").as_deref() == Some("skmsg"))
        .collect();
    assert_eq!(skmsg.len(), 1, "not one sender-key message");
    assert_eq!(attr(skmsg[0], "mediatype").as_deref(), Some("image"));
    let read = peer
        .read_group(frame)
        .await
        .expect("the member reads the group picture");
    let image = read.image_message.as_option().expect("a picture message");
    assert_eq!((image.width, image.height), (Some(200), Some(160)));
    let (kind, plain) = fake
        .download(&read)
        .expect("the member downloads the picture");
    assert_eq!(kind, MediaType::Image);
    assert!(plain.starts_with(&[0xFF, 0xD8]), "the picture is no JPEG");
    rig.finish().await;
}

/// An Ogg file whose first page holds an `OpusHead` packet (the route's test of an Opus file), then `tail` bytes.
fn opus_ogg(tail: usize) -> Vec<u8> {
    let mut ogg = b"OggS\0\x02".to_vec();
    ogg.extend_from_slice(&[0u8; 20]);
    ogg.push(1);
    ogg.push(19);
    ogg.extend_from_slice(b"OpusHead\x01\x02\0\0\x80\xbb\0\0\0\0\0");
    ogg.extend(pattern(tail, 0x2B));
    ogg
}

/// `an_opus_ogg_is_sent_as_an_audio_message` (fix F12, interop LOW-5): an Opus Ogg goes to `/mms/audio` as an
/// `audioMessage` (`mediatype="audio"`) that is NOT a voice note (`ptt` false) with WhatsApp's Opus mimetype and a
/// streaming sidecar; the peer downloads under the key type its OWN reading of the received shape gives (the audio
/// keys): byte-identical.
async fn an_opus_ogg_is_sent_as_an_audio_message() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    let bytes = opus_ogg(150_000);
    let file = core_file(
        dir.path(),
        "audio-70162839",
        &bytes,
        AttachmentKind::Audio,
        "audio/ogg",
    );

    let outcome = rig
        .transmit(&file_request(chat(), AUDIO_ID, file, None))
        .await;
    assert!(sent(&outcome), "the audio file is not answered Sent");
    one_whole_post(&fake, "/mms/audio");
    let frames = rig.message_frames(AUDIO_ID);
    assert_eq!(frames.len(), 1, "one frame for one audio file");
    assert_eq!(to_mediatype(&frames[0]).as_deref(), Some("audio"));
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    // The oracle first (G18): the peer derives the key type from the RECEIVED shape, whatever the sender used.
    let (kind, plain) = fake.download(&read).expect("the peer downloads the audio");
    assert_eq!(kind, MediaType::Audio);
    assert!(plain == bytes, "the audio file's bytes changed");
    let audio = read.audio_message.as_option().expect("an audio message");
    assert_eq!(audio.ptt, Some(false), "an audio file is no voice note");
    assert_eq!(audio.mimetype.as_deref(), Some("audio/ogg; codecs=opus"));
    assert_eq!(audio.file_length, Some(bytes.len() as u64));
    assert!(
        audio
            .streaming_sidecar
            .as_deref()
            .is_some_and(|s| !s.is_empty()),
        "no streaming sidecar"
    );
    assert!(read.document_message.as_option().is_none());
    assert!(read.document_with_caption_message.as_option().is_none());
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");
    rig.finish().await;
}

/// `an_uncaptioned_document_is_a_document_message` (fix F12): a file with no caption goes to `/mms/document` as a
/// plain `documentMessage` (no caption wrapper), with its name and type; the peer's download is byte-identical.
async fn an_uncaptioned_document_is_a_document_message() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    let bytes = pattern(120_000, 0x6D);
    let file = Attachment {
        file_name: Some(PLAIN_NAME.into()),
        ..core_file(
            dir.path(),
            "plain-document-41928370",
            &bytes,
            AttachmentKind::File,
            "application/octet-stream",
        )
    };

    let outcome = rig
        .transmit(&file_request(chat(), PLAIN_DOCUMENT_ID, file, None))
        .await;
    assert!(sent(&outcome), "the document is not answered Sent");
    one_whole_post(&fake, "/mms/document");
    let frames = rig.message_frames(PLAIN_DOCUMENT_ID);
    assert_eq!(frames.len(), 1, "one frame for one document");
    assert_eq!(to_mediatype(&frames[0]).as_deref(), Some("document"));
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    assert!(
        read.document_with_caption_message.as_option().is_none(),
        "a document with no caption is no wrapper"
    );
    let document = read
        .document_message
        .as_option()
        .expect("a document message");
    assert!(
        document.file_name.as_deref() == Some(PLAIN_NAME),
        "another file name"
    );
    assert!(document.caption.is_none(), "a caption out of nothing");
    assert_eq!(
        document.mimetype.as_deref(),
        Some("application/octet-stream")
    );
    let (kind, plain) = fake
        .download(&read)
        .expect("the peer downloads the document");
    assert_eq!(kind, MediaType::Document);
    assert!(plain == bytes, "the document's bytes changed");
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");
    rig.finish().await;
}

/// The error of a transmit that failed for now.
fn transient(outcome: &TransmitOutcome) -> bool {
    matches!(outcome, TransmitOutcome::Failed(e) if matches!(e.kind, ErrorKind::Transient { .. }))
}

/// `a_401_asks_for_the_media_conn_again_once` (CHARACTERISATION of upstream's loop, G21): a 401 makes upstream ask
/// for the media connection again ONCE and post again; a second 401 ends the upload. Kourier answers Transient
/// (`upload_error`), writes no frame and leaves no file; the resend uploads and goes.
async fn a_401_asks_for_the_media_conn_again_once() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    fake.reply(Reply::Status(401)).reply(Reply::Status(401));
    let bytes = pattern(50_000, 0x2E);
    let file = Attachment {
        file_name: Some(DOCUMENT_NAME.into()),
        ..core_file(
            dir.path(),
            "auth-71829304",
            &bytes,
            AttachmentKind::File,
            "application/pdf",
        )
    };
    let req = file_request(chat(), AUTH_ID, file, None);

    let outcome = rig.transmit(&req).await;
    assert!(
        transient(&outcome),
        "a refused authorisation is not Transient: {outcome:?}"
    );
    assert_eq!(
        rig.server.answered("w:m"),
        2,
        "the media connection asked again once"
    );
    let posts = fake.posts();
    assert_eq!(posts.len(), 2, "one POST per media connection");
    assert!(
        posts
            .iter()
            .all(|p| p.host == cdn::HOST_A && p.status == Some(401))
    );
    assert!(
        rig.message_frames(AUTH_ID).is_empty(),
        "a frame without its file"
    );
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");

    let again = rig.transmit(&req).await;
    assert!(sent(&again), "the resend is not answered Sent");
    let frames = rig.message_frames(AUTH_ID);
    assert_eq!(frames.len(), 1, "one frame for the resend");
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let (_, plain) = fake
        .download(&read)
        .expect("the peer downloads the document");
    assert!(plain == bytes, "the document's bytes changed");
    rig.finish().await;
}

/// `a_500_tries_the_next_host` (CHARACTERISATION of upstream's loop, G20/G21): a 500 (its body holds the CDN's body
/// marker, which no log line may hold) on host A sends the upload to host B; a 500 on both ends the upload: Kourier
/// answers Transient and writes no frame; the resend goes through host B.
async fn a_500_tries_the_next_host() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    fake.reply(Reply::Status(500)).reply(Reply::Status(500));
    let bytes = pattern(80_000, 0x41);
    let file = core_file(
        dir.path(),
        "failed-82930415",
        &bytes,
        AttachmentKind::File,
        "application/octet-stream",
    );
    let req = file_request(chat(), FAILED_ID, file, None);

    let outcome = rig.transmit(&req).await;
    assert!(
        transient(&outcome),
        "a failing media server is not Transient: {outcome:?}"
    );
    let hosts: Vec<(String, Option<u16>)> = fake
        .posts()
        .into_iter()
        .map(|p| (p.host, p.status))
        .collect();
    assert_eq!(
        hosts,
        vec![
            (cdn::HOST_A.to_owned(), Some(500)),
            (cdn::HOST_B.to_owned(), Some(500))
        ],
        "both hosts tried, in order"
    );
    assert!(
        rig.message_frames(FAILED_ID).is_empty(),
        "a frame without its file"
    );
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");

    fake.reply(Reply::Status(500)).reply(Reply::Ok);
    let again = rig.transmit(&req).await;
    assert!(sent(&again), "the resend is not answered Sent");
    let posts = fake.posts();
    assert_eq!(posts.len(), 4);
    assert_eq!(
        (posts[3].host.as_str(), posts[3].status),
        (cdn::HOST_B, Some(200))
    );
    let frames = rig.message_frames(FAILED_ID);
    assert_eq!(frames.len(), 1, "one frame for the resend");
    let read = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let (kind, plain) = fake.download(&read).expect("the peer downloads the file");
    assert_eq!(kind, MediaType::Document);
    assert!(plain == bytes, "the file's bytes changed");
    rig.finish().await;
}

/// `a_stop_during_the_post_writes_no_frame` (brief §5, G1, G2): the person presses X while the CDN reads the
/// POST's body (the hook at 64 KiB holds the CDN until the stop is answered): the stop is answered `Stopping`,
/// the POST ends with the reader's error (no status), the Transmit `Failed(Internal)`, NO `<message>` frame,
/// no upload record, no temporary file.
async fn a_stop_during_the_post_writes_no_frame() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (hit_tx, hit_rx) = tokio::sync::oneshot::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    fake.reply(Reply::CallAt(
        64 * 1024,
        Box::new(move || {
            let _ = hit_tx.send(());
            let _ = done_rx.recv_timeout(std::time::Duration::from_secs(20));
        }),
    ));
    let (rig, _peer) = started(dir.path(), &device, media_server(), &fake).await;
    let bytes = pattern(1_000_000, 0x63);
    let file = core_file(
        dir.path(),
        "stopped-93847102",
        &bytes,
        AttachmentKind::File,
        "application/octet-stream",
    );
    let req = file_request(chat(), STOPPED_ID, file, None);

    let (outcome, answer) = tokio::join!(rig.transmit(&req), async {
        hit_rx.await.expect("the CDN reached the hook");
        let answer = rig.abort().await;
        done_tx.send(()).expect("the hook waits");
        answer
    });
    assert_eq!(
        answer,
        AbortAnswer::Stopping,
        "a stop mid-POST is not Stopping"
    );
    assert!(
        matches!(&outcome, TransmitOutcome::Failed(e) if e.kind == ErrorKind::Internal),
        "a stopped upload is not Failed(Internal)"
    );
    assert!(fake.wait_posts(1, std::time::Duration::from_secs(10)).await);
    let posts = fake.posts();
    assert_eq!(posts.len(), 1, "the stopped upload was posted again");
    assert!(
        matches!(posts[0].read, BodyEnd::Failed(_)) && posts[0].status.is_none(),
        "the POST did not end with the reader's error"
    );
    assert_eq!(fake.finished(), 0, "the CDN kept a whole upload");
    assert!(
        rig.message_frames(STOPPED_ID).is_empty(),
        "a frame of a stopped message"
    );
    // The answer may come before the POST's reader (on upstream's blocking thread) is dropped, as in the attempt's
    // own stop test: the sealed temporary file goes with that reader, within moments.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while temporary(dir.path()) > 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "a temporary file is left"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            &format!("SELECT count(*) FROM media_uploads WHERE local = '{STOPPED_ID}'")
        ),
        0,
        "a stopped upload was recorded"
    );
}

/// `a_quit_while_the_cdn_holds_a_post_ends_within_the_grace` (decision G14): the CDN stops reading the POST's
/// body at 64 KiB and holds it; the account quits meanwhile: the actor exits well inside the network's 20 s
/// `shutdown_grace` (fix F16, Q-B16: the quit takes the attempt, which has written nothing, at once and does not wait
/// `QUIT_ACK_WAIT` for it; after the wait whatever is left is ended BEFORE the client stops, so the held upload does not
/// keep the store into the release wait: the bound is 8 s, the old path took 15.7 s), no `<message>` frame was
/// written, and the attempt's answer, if any, is `Transient`, never `Sent`.
async fn a_quit_while_the_cdn_holds_a_post_ends_within_the_grace() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    fake.reply(Reply::StallAt(64 * 1024));
    let (rig, _peer) = started(dir.path(), &device, media_server(), &fake).await;
    let bytes = pattern(1_000_000, 0x71);
    let file = core_file(
        dir.path(),
        "quit-58203917",
        &bytes,
        AttachmentKind::File,
        "application/octet-stream",
    );
    let req = file_request(chat(), QUIT_ID, file, None);
    let mut answer = rig.transmit_detached(&req).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        tokio::select! {
            stalled = fake.wait_stalled(std::time::Duration::from_secs(30)) => {
                assert!(stalled, "the POST never reached the CDN's hold");
                break;
            }
            outcome = &mut answer => {
                // Only the hold before the first pull may answer first (as `Rig::transmit` retries it).
                let outcome = outcome.expect("an answer");
                assert!(
                    matches!(&outcome, TransmitOutcome::Failed(e)
                        if matches!(e.kind, ErrorKind::Transient { retry_after: Some(_) })),
                    "the file was answered before its upload"
                );
                assert!(std::time::Instant::now() < deadline, "the send stayed held");
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                answer = rig.transmit_detached(&req).await;
            }
        }
    }
    assert!(
        rig.message_frames(QUIT_ID).is_empty(),
        "a frame before the upload ended"
    );
    let took = rig.finish_timed().await;
    eprintln!("quit took {} ms", took.as_millis());
    assert!(
        took < std::time::Duration::from_secs(8),
        "the quit waited for an attempt that had written nothing ({took:?}; the network's grace is 20 s)"
    );
    if let Ok(outcome) = answer.try_recv() {
        assert!(
            matches!(&outcome, TransmitOutcome::Failed(e) if e.is_retryable()),
            "a held upload ended by the quit is answered Transient (the core sends it again): {outcome:?}"
        );
    }
    // The held POST's thread is upstream's: let it end so the child exits cleanly.
    fake.release();
    assert_eq!(fake.finished(), 0, "the CDN kept a whole upload");
}

/// `a_resend_after_a_lost_ack_uploads_nothing_and_sends_the_same_file` (brief §5, G4, D5): the server ends the
/// connection right after the picture's frame, with no ack: Transient; on the NEW connection the core's resend
/// uploads NOTHING (the record is within 6 h) and writes a frame with the SAME id and the SAME media fields;
/// ONE POST in all; the peer reads and downloads the resend; the ledger holds it acked.
async fn a_resend_after_a_lost_ack_uploads_nothing_and_sends_the_same_file() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, mut peer) = started(dir.path(), &device, media_server(), &fake).await;
    let original = picture_with_exif(240, 180, 30_495_817);
    let file = core_file(
        dir.path(),
        "resent-48203917",
        &original,
        AttachmentKind::Image,
        "image/jpeg",
    );
    let req = file_request(chat(), RESENT_ID, file, Some(PICTURE_CAPTION));
    let before = rig.state_count();
    rig.server.ack_next(Ack::DropConnection);

    let outcome = rig.transmit(&req).await;
    assert!(
        matches!(&outcome, TransmitOutcome::Failed(e) if e.kind == ErrorKind::Transient { retry_after: None }),
        "a lost ack is not Transient with no wait"
    );
    assert!(
        rig.server
            .wait_opened(2, std::time::Duration::from_secs(30))
            .await,
        "the client never connected again"
    );
    rig.server
        .send(&wire::success_as_the_server_writes_it(peer::ACCOUNT_LID));
    rig.online_after(before).await;
    let again = rig.transmit(&req).await;
    assert!(sent(&again), "the resend is not answered Sent");

    one_whole_post(&fake, "/mms/image");
    let frames = rig.message_frames(RESENT_ID);
    assert_eq!(
        frames.len(),
        2,
        "the frame and its resend do not both carry the id"
    );
    let first = peer
        .read(&frames[0])
        .await
        .expect("the peer reads the frame");
    let second = peer
        .read(&frames[1])
        .await
        .expect("the peer reads the resend");
    let (a, b) = (
        first.image_message.as_option().expect("a picture"),
        second.image_message.as_option().expect("a picture"),
    );
    assert!(
        a.direct_path == b.direct_path
            && a.media_key == b.media_key
            && a.file_sha256 == b.file_sha256
            && a.file_enc_sha256 == b.file_enc_sha256
            && a.file_length == b.file_length,
        "the resend names another file"
    );
    let (kind, plain) = fake
        .download(&second)
        .expect("the peer downloads the resend");
    assert_eq!(kind, MediaType::Image);
    assert!(plain.starts_with(&[0xFF, 0xD8]), "the picture is no JPEG");
    assert_eq!(temporary(dir.path()), 0, "a temporary file is left");
    rig.finish().await;
    let conn = raw(dir.path());
    assert_eq!(
        count(
            &conn,
            &format!(
                "SELECT count(*) FROM send_outcomes WHERE local = '{RESENT_ID}' AND outcome = 'acked'"
            )
        ),
        1,
        "the resend is not acked in the ledger"
    );
}

/// `a_picture_deleted_for_everyone_leaves_no_upload_record` (fix F13): a picture is sent (its record holds the media
/// key, the paths and the preview), then the person deletes it for everyone and the revoke is acked: the record is
/// gone in the zone that recorded the revoke, not kept for the sweep's 30 days.
async fn a_picture_deleted_for_everyone_leaves_no_upload_record() {
    let (dir, device) = account_dir().await;
    let fake = Cdn::default();
    let (rig, _peer) = started(dir.path(), &device, media_server(), &fake).await;
    let original = picture_with_exif(200, 150, 81_740_295);
    let file = core_file(
        dir.path(),
        "revoked-62059183",
        &original,
        AttachmentKind::Image,
        "image/jpeg",
    );
    let outcome = rig
        .transmit(&file_request(chat(), REVOKED_ID, file, None))
        .await;
    assert!(sent(&outcome), "the picture is not answered Sent");
    let records = |id: &str| {
        count(
            &raw(dir.path()),
            &format!("SELECT count(*) FROM media_uploads WHERE local = '{id}'"),
        )
    };
    assert_eq!(records(REVOKED_ID), 1, "the sent picture has its record");

    let revoke = OutgoingRequest {
        conv: chat(),
        local: REVOKE_ID.into(),
        sent_at: i64::try_from(now_secs() * 1000).expect("time"),
        kind: OutgoingKind::Delete {
            target: MsgTarget {
                conv: Some(chat()),
                author: TargetAuthor::Own,
                local: REVOKED_ID.into(),
                alternates: Vec::new(),
            },
            for_everyone: true,
        },
        only: None,
        is_recipient_update: false,
    };
    let outcome = rig.transmit(&revoke).await;
    assert!(sent(&outcome), "the revoke is not answered Sent");
    assert_eq!(
        count(
            &raw(dir.path()),
            &format!(
                "SELECT count(*) FROM send_outcomes WHERE local = '{REVOKE_ID}' AND outcome = 'acked'"
            )
        ),
        1,
        "the revoke is not acked in the ledger"
    );
    assert_eq!(
        records(REVOKED_ID),
        0,
        "the acked revoke left the deleted picture's upload record"
    );
    rig.finish().await;
}

/// Fix F14: the needles the peer looks for are the value in every form a line could print it in: hex, base64,
/// URL-safe base64 (no padding; upstream puts `file_enc_sha256` into the URL as the last), for the bytes the peer learned
/// of a downloaded message.
#[test]
fn the_peers_needles_are_the_key_and_hashes_in_every_form_a_line_could_print() {
    let [hex, standard, url] = cdn::needles_of(&[0xFB, 0xFF, 0xBE, 0x00, 0x01]);
    assert_eq!(hex, "fbffbe0001");
    assert_eq!(standard, "+/++AAE");
    assert_eq!(url, "-_--AAE");
    let all: Vec<u8> = (0..32).collect();
    let [hex, standard, url] = cdn::needles_of(&all);
    assert_eq!(hex.len(), 64);
    assert_eq!(standard, "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8");
    assert_eq!(url, standard, "no + or / in this one");
    // What the peer learns is what the final grep of a child reads.
    cdn::learn(Some(&[0xFB, 0xFF, 0xBE]));
    cdn::learn(None);
    cdn::learn(Some(&[]));
    let learned = cdn::learned();
    assert!(
        ["fbffbe", "+/++", "-_--"]
            .iter()
            .all(|n| learned.iter().any(|l| l == n))
    );
}

/// Fix F14, the parent: the child checks the recorder itself (a record it is given, not a case).
#[test]
fn the_leak_checks_see_a_host_name_and_a_token_in_upstreams_upload_path_over_the_wire() {
    let output = support::offline::run_child_output(module_path!(), "recorder_child", &[]);
    assert!(
        !output.contains("recorder-child-token"),
        "the child printed its token"
    );
}

/// Fix F14, the child: a recorder with upstream's upload-path modules. `token_shaped` flags a token in a WARN record of
/// each of them (the upload, the media connection, the HTTP client, `ureq`) and in none of another module or below
/// WARN (the app writes none); `leaks` finds the media host's name in any line.
#[test]
fn recorder_child() {
    use tracing_log::log::{self, Level, Record};

    if !support::offline::in_child() {
        return;
    }
    let capture = Capture::default();
    capture.install();
    let token = "recorder-child-token-QwErTyUiOpAsDfGhJkLzXcVbNm-95106284";
    let say = |module: &str, level: Level, text: &str| {
        log::logger().log(
            &Record::builder()
                .args(format_args!("said {text}"))
                .level(level)
                .target("synthetic")
                .module_path(Some(module))
                .build(),
        );
    };
    for module in [
        "whatsapp_rust::upload",
        "whatsapp_rust::mediaconn",
        concat!("whatsapp_rust_", "ureq_http_client"),
        "ureq_proto::util",
    ] {
        say(module, Level::Warn, token);
    }
    say("whatsapp_rust::client", Level::Warn, token);
    say("whatsapp_rust::upload", Level::Info, token);
    assert_eq!(
        capture.token_shaped().len(),
        4,
        "the four upload-path modules are flagged, another module and a record below WARN are not"
    );
    say("whatsapp_rust::client", Level::Warn, cdn::HOST_B);
    assert_eq!(
        capture.leaks(&cdn::secrets()).len(),
        1,
        "a host name is a secret"
    );
}

/// The case the child runs (set by the parent).
const CASE: &str = "WA_MEDIA_WIRE_CASE";

/// One parent per case; nothing the child printed holds a secret (D10, G20).
fn parent(case: &str) {
    let output = support::offline::run_child_output(module_path!(), "media_child", &[(CASE, case)]);
    for secret in media_secrets() {
        assert!(
            !output.contains(secret),
            "the child printed one of the secrets"
        );
    }
}

#[test]
fn a_file_from_a_phone_arrives_sealed_and_whole_over_the_wire() {
    parent("download");
}

#[test]
fn a_file_that_does_not_verify_never_appears_over_the_wire() {
    parent("tampered");
}

#[test]
fn a_file_the_servers_do_not_have_is_expired_over_the_wire() {
    parent("expired");
}

#[test]
fn a_picture_is_uploaded_then_sent_and_the_peer_decrypts_it_over_the_wire() {
    parent("picture");
}

#[test]
fn a_document_keeps_its_bytes_and_its_name_over_the_wire() {
    parent("document");
}

#[test]
fn a_video_carries_its_seconds_and_sidecar_over_the_wire() {
    parent("video");
}

#[test]
fn a_group_picture_is_one_sender_key_message_over_the_wire() {
    parent("group");
}

#[test]
fn an_opus_ogg_is_sent_as_an_audio_message_over_the_wire() {
    parent("audio");
}

#[test]
fn an_uncaptioned_document_is_a_document_message_over_the_wire() {
    parent("plain_document");
}

#[test]
fn a_picture_deleted_for_everyone_leaves_no_upload_record_over_the_wire() {
    parent("revoked");
}

#[test]
fn a_401_asks_for_the_media_conn_again_once_over_the_wire() {
    parent("auth");
}

#[test]
fn a_500_tries_the_next_host_over_the_wire() {
    parent("failed");
}

#[test]
fn a_stop_during_the_post_writes_no_frame_over_the_wire() {
    parent("stopped");
}

#[test]
fn a_quit_while_the_cdn_holds_a_post_ends_within_the_grace_over_the_wire() {
    parent("quit");
}

#[test]
fn a_resend_after_a_lost_ack_uploads_nothing_and_sends_the_same_file_over_the_wire() {
    parent("resent");
}

/// The child (a no-op as an ordinary test): every log line is recorded, and none may hold a secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_child() {
    if !support::offline::in_child() {
        return;
    }
    let log = Capture::default();
    log.install();
    let case = std::env::var(CASE).expect("the parent names the case");
    eprintln!("case {case}");
    match case.as_str() {
        "download" => a_file_from_a_phone_arrives_sealed_and_whole().await,
        "tampered" => a_file_that_does_not_verify_never_appears().await,
        "expired" => a_file_the_servers_do_not_have_is_expired().await,
        "picture" => a_picture_is_uploaded_then_sent_and_the_peer_decrypts_it().await,
        "document" => a_document_keeps_its_bytes_and_its_name().await,
        "video" => a_video_carries_its_seconds_and_sidecar().await,
        "group" => a_group_picture_is_one_sender_key_message().await,
        "audio" => an_opus_ogg_is_sent_as_an_audio_message().await,
        "plain_document" => an_uncaptioned_document_is_a_document_message().await,
        "revoked" => a_picture_deleted_for_everyone_leaves_no_upload_record().await,
        "auth" => a_401_asks_for_the_media_conn_again_once().await,
        "failed" => a_500_tries_the_next_host().await,
        "stopped" => a_stop_during_the_post_writes_no_frame().await,
        "quit" => a_quit_while_the_cdn_holds_a_post_ends_within_the_grace().await,
        "resent" => a_resend_after_a_lost_ack_uploads_nothing_and_sends_the_same_file().await,
        other => panic!("unknown case {other}"),
    }
    assert!(
        log.own_lines() >= 1,
        "the recorder saw no line of the crate"
    );
    assert!(
        log.upstream_records() >= 1,
        "the `log` bridge heard nothing"
    );
    // The fixture's markers and host names, and what the peer learned of the files it downloaded (fix F14): the
    // received media key and both sha256 values, as hex and as base64 (random per run: only the peer knows them).
    let learned = cdn::learned();
    if !matches!(case.as_str(), "stopped" | "quit" | "revoked") {
        // Three values (the key and the two hashes) in three forms: a case that reads a frame must have learned them,
        // or the grep below looks for nothing.
        assert!(learned.len() >= 9, "the peer learned no key or hash");
    }
    let mut targets = media_secrets();
    targets.extend(learned.iter().map(String::as_str));
    if case == "tampered" {
        // Fix F14 holds for what is WRITTEN: upstream's one line for a file that fails its hashes names the host it tried
        // (its `Debug` of the request keeps the URL's host, in quotes, and hides the path, the query and the key: read at
        // the pin), and that line reaches the log only through `redact_upstream`.
        let hosts = [cdn::HOST_A, cdn::HOST_B];
        targets.retain(|target| !hosts.contains(target));
        let lines = log.lines();
        let tried = lines
            .iter()
            .filter(|line| line.target == "log:whatsapp_rust::download:WARN");
        for line in tried {
            let written = kourier_whatsapp::logs::redact_upstream(&line.text);
            assert!(
                !hosts.iter().any(|host| written.contains(host)),
                "the app would write the media host"
            );
        }
    }
    assert_eq!(
        log.leaks(&targets),
        Vec::<String>::new(),
        "a log line holds a secret (the targets are named)"
    );
    assert_eq!(
        log.token_shaped(),
        Vec::<String>::new(),
        "a line of the crate or of upstream's upload path holds a run of token characters (the targets are named)"
    );
}
