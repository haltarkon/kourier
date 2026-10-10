//! Attachments of incoming messages (§6.11, §6.13; `SD/ts/textsecure/processDataMessage.preload.ts`
//! `processAttachment`, `SD/ts/util/Attachment.std.ts` `partitionBodyAndNormalAttachments`).
//!
//! An `AttachmentPointer` becomes a core [`Attachment`] in state `Remote` whose `remote` is the
//! encoded pointer (`BackendBlob` v2, what `DownloadAttachment` hands back); a pointer without
//! the key, digest, size or CDN key it needs is kept visible as `Failed{retryable: false}`.
//! Kinds follow the pointer's flags and content type: `VOICE_MESSAGE` → voice note, `GIF` or
//! `image/gif` → GIF, then image, video, audio, file. A sticker's image is a `Body::Sticker`.
//!
//! Each attachment carries the names Signal has for it (`Attachment::net_ids`: the pointer's
//! client UUID and the digest of its encrypted blob), by which our phone later names one it
//! deleted (`SyncMessage.deleteForMe.attachmentDeletes`, [`net_id_uuid`], [`net_id_digest`]).
//!
//! Long texts: a `text/x-signal-plain` attachment carries the whole body of a text longer than
//! 2 KiB (the `body` holds its first 2 KiB). Stage 2 downloads it in the lane's network step
//! ([`long_texts`], within the step's time box; a download the network broke off is tried
//! again there) and maps the message with the whole text; when the download fails, the message
//! keeps the trimmed body and the attachment stays with it, marked `long_text`
//! (`dispatch::message_body`): the core downloads it and joins it into the body.

use std::collections::{HashMap, HashSet};

use kourier_model::{Attachment, AttachmentKind, AttachmentState, BackendBlob, Text};
use prost::Message as _;

use super::KIND_CONTENT;
use super::stage2::{NETWORK_STEP_TIMEOUT, Row};
use crate::actor::Shared;
use crate::attachment::{self, DownloadError, POINTER_BLOB_V, Pointer, max_blob_len};
use crate::net::{Net, SignalTransport};
use crate::proto::{self, content::Content as ContentKind};
use crate::send::media::{FLAG_GIF, FLAG_VOICE, LONG_TEXT_MIME, MAX_TEXT_BYTES};

/// Longest file name kept from a pointer (bytes).
const MAX_FILE_NAME: usize = 255;
/// Longest blurhash kept (Desktop's are ~30 characters).
const MAX_BLURHASH: usize = 128;
/// Most waveform bars kept.
const MAX_WAVEFORM: usize = 100;
/// Downloads tried per long text within the step's time box: a connection that broke off once
/// must not cost the message its text.
const LONG_TEXT_ATTEMPTS: u32 = 3;
/// Pause before a long text is tried again.
const LONG_TEXT_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(500);

/// Lower-case hex.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The name of an attachment by its `clientUuid` (16 bytes), as it stands in
/// `Attachment::net_ids` and in `AttachmentDeleted`.
pub(crate) fn net_id_uuid(client_uuid: &[u8]) -> Option<String> {
    (client_uuid.len() == 16).then(|| format!("uuid:{}", hex(client_uuid)))
}

/// The name of an attachment by the SHA-256 of its encrypted blob (the pointer's `digest`,
/// `AttachmentDelete.fallbackDigest`).
pub(crate) fn net_id_digest(digest: &[u8]) -> Option<String> {
    (digest.len() == 32).then(|| format!("digest:{}", hex(digest)))
}

/// The `len` bytes a name of the form `<prefix><hex>` among `net_ids` states.
fn named_bytes(net_ids: &[String], prefix: &str, len: usize) -> Option<Vec<u8>> {
    net_ids.iter().find_map(|name| {
        let hex = name.strip_prefix(prefix)?;
        if hex.len() != len * 2 || !hex.is_ascii() {
            return None;
        }
        (0..len)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
            .collect()
    })
}

/// The client UUID among an attachment's names ([`net_id_uuid`] read back).
pub(crate) fn uuid_of_net_ids(net_ids: &[String]) -> Option<Vec<u8>> {
    named_bytes(net_ids, "uuid:", 16)
}

/// The blob digest among an attachment's names ([`net_id_digest`] read back).
pub(crate) fn digest_of_net_ids(net_ids: &[String]) -> Option<Vec<u8>> {
    named_bytes(net_ids, "digest:", 32)
}

/// The names of `pointer` ([`net_id_uuid`], [`net_id_digest`]).
fn net_ids(pointer: &proto::AttachmentPointer) -> Vec<String> {
    pointer
        .client_uuid
        .as_deref()
        .and_then(net_id_uuid)
        .into_iter()
        .chain(pointer.digest.as_deref().and_then(net_id_digest))
        .collect()
}

/// Whether `pointer` carries a long text body.
pub(crate) fn is_long_text(pointer: &proto::AttachmentPointer) -> bool {
    pointer
        .content_type
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case(LONG_TEXT_MIME))
}

/// Whether `pointer` is a long text the lane's network step can download ([`long_texts`]): a
/// complete pointer of at most [`MAX_TEXT_BYTES`].
pub(crate) fn is_fetchable_long_text(pointer: &proto::AttachmentPointer) -> bool {
    is_long_text(pointer)
        && Pointer::from_proto(pointer).is_ok_and(|p| p.size <= MAX_TEXT_BYTES as u64)
}

/// The kind of an attachment from its flags and content type.
pub(crate) fn kind_of(mime: &str, flags: u32) -> AttachmentKind {
    let mime = mime.to_ascii_lowercase();
    if flags & FLAG_VOICE != 0 {
        AttachmentKind::Voice
    } else if flags & FLAG_GIF != 0 || mime == "image/gif" {
        AttachmentKind::Gif
    } else if mime.starts_with("image/") && !mime.starts_with("image/svg") {
        AttachmentKind::Image
    } else if mime.starts_with("video/") {
        AttachmentKind::Video
    } else if mime.starts_with("audio/") {
        AttachmentKind::Audio
    } else {
        AttachmentKind::File
    }
}

/// `pointer` as a core attachment of the given kind (`None`: from its flags and type).
pub(crate) fn attachment(
    pointer: &proto::AttachmentPointer,
    kind: Option<AttachmentKind>,
) -> Attachment {
    let mime = pointer
        .content_type
        .clone()
        .filter(|t| !t.is_empty() && t.len() <= 255)
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    let kind = kind.unwrap_or_else(|| kind_of(&mime, pointer.flags.unwrap_or(0)));
    let valid = Pointer::from_proto(pointer).is_ok();
    Attachment {
        id: None,
        kind,
        size: u64::from(pointer.size.unwrap_or(0)),
        file_name: pointer
            .file_name
            .clone()
            .filter(|n| !n.is_empty() && n.len() <= MAX_FILE_NAME),
        dims: pointer
            .width
            .zip(pointer.height)
            .filter(|(w, h)| *w > 0 && *h > 0),
        duration_ms: pointer
            .audio_duration_seconds
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(|s| (f64::from(s) * 1000.0).min(f64::from(u32::MAX)) as u32),
        blurhash: pointer
            .blur_hash
            .clone()
            .filter(|b| !b.is_empty() && b.len() <= MAX_BLURHASH),
        waveform: pointer
            .audio_waveform
            .clone()
            .filter(|w| !w.is_empty())
            .map(|mut w| {
                w.truncate(MAX_WAVEFORM);
                w
            }),
        caption: pointer
            .caption
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(Text::plain),
        state: if valid {
            AttachmentState::Remote
        } else {
            AttachmentState::Failed { retryable: false }
        },
        remote: valid.then(|| BackendBlob::new(POINTER_BLOB_V, pointer.encode_to_vec())),
        local: None,
        thumb: None,
        mime,
        // Set by `dispatch::message_body` for a long text the lane could not fetch: the core
        // then downloads it and joins it into the body (`Attachment::long_text`).
        long_text: false,
        net_ids: net_ids(pointer),
    }
}

/// The sticker of a message as an image attachment.
pub(crate) fn sticker(sticker: &proto::data_message::Sticker) -> Option<Attachment> {
    let data = sticker.data.as_ref()?;
    let mut image = attachment(data, Some(AttachmentKind::Image));
    if data.content_type.as_deref().is_none_or(str::is_empty) {
        "image/webp".clone_into(&mut image.mime);
    }
    Some(image)
}

/// The `DataMessage` a row's content shows: a message, an edit's new revision, or our phone's
/// transcript of either.
fn shown_message(content: &proto::Content) -> Option<&proto::DataMessage> {
    match content.content.as_ref()? {
        ContentKind::DataMessage(message) => Some(message),
        ContentKind::EditMessage(edit) => edit.data_message.as_ref(),
        ContentKind::SyncMessage(sync) => match sync.content.as_ref()? {
            proto::sync_message::Content::Sent(sent) => sent.message.as_ref().or(sent
                .edit_message
                .as_ref()
                .and_then(|e| e.data_message.as_ref())),
            _ => None,
        },
        _ => None,
    }
}

/// The long-text pointer of each row's message that has one. Rows of the `blocked` senders
/// (service id strings) are left out: nothing is fetched for a blocked contact.
fn long_text_pointers(rows: &[Row], blocked: &HashSet<String>) -> Vec<(u64, Pointer)> {
    rows.iter()
        .filter(|row| row.kind == KIND_CONTENT && !blocked.contains(&row.sender))
        .filter_map(|row| {
            let content = proto::Content::decode(row.content.as_deref()?).ok()?;
            let message = shown_message(&content)?;
            let pointer = message
                .attachments
                .iter()
                .find(|p| is_fetchable_long_text(p))?;
            Some((row.arrival, Pointer::from_proto(pointer).ok()?))
        })
        .collect()
}

/// Downloads the long texts of `rows` (by arrival), all within [`NETWORK_STEP_TIMEOUT`]; a row
/// whose download fails is left out (its message keeps the trimmed body and the attachment).
pub(crate) async fn long_texts<T: SignalTransport>(
    shared: &Shared<T>,
    rows: &[Row],
) -> HashMap<u64, String> {
    // The zone drops whatever a blocked sender sent (`dispatch`), so nothing is downloaded for
    // one either: no request to the CDN on their behalf, no lane time spent on their texts.
    let wanted = long_text_pointers(rows, &blocked_senders(shared, rows));
    // An unlinked device starts no network work (§4.3 step 5): the trimmed bodies are kept.
    if wanted.is_empty() || shared.unlinking.get() {
        return HashMap::new();
    }
    let deadline = tokio::time::Instant::now() + NETWORK_STEP_TIMEOUT;
    download_long_texts(&shared.net, wanted, deadline).await
}

/// The senders of `rows` that are blocked on our phone. A database error counts as "none": the
/// zone that follows reads the same flag and reports the error.
fn blocked_senders<T: SignalTransport>(shared: &Shared<T>, rows: &[Row]) -> HashSet<String> {
    let senders: HashSet<&str> = rows
        .iter()
        .map(|row| row.sender.as_str())
        .filter(|sender| !sender.is_empty())
        .collect();
    shared
        .db
        .read(|conn| -> crate::error::Result<HashSet<String>> {
            let mut blocked = HashSet::new();
            for sender in senders {
                if crate::peers::load(conn, sender)?.blocked {
                    blocked.insert(sender.to_owned());
                }
            }
            Ok(blocked)
        })
        .unwrap_or_default()
}

/// Downloads each of `wanted` until `deadline`. A download the network broke off is tried
/// again (up to [`LONG_TEXT_ATTEMPTS`] times in all); an answer of the CDN or a blob that does
/// not verify is final.
async fn download_long_texts<T: SignalTransport>(
    net: &Net<T>,
    wanted: Vec<(u64, Pointer)>,
    deadline: tokio::time::Instant,
) -> HashMap<u64, String> {
    let mut texts = HashMap::new();
    'rows: for (arrival, pointer) in wanted {
        for attempt in 1..=LONG_TEXT_ATTEMPTS {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                tracing::warn!("long text downloads ran out of time; trimmed bodies kept");
                break 'rows;
            }
            let download =
                attachment::download_in_memory(net, &pointer, max_blob_len(pointer.size), left);
            match tokio::time::timeout(left, download).await {
                Ok(Ok(bytes)) => {
                    texts.insert(arrival, String::from_utf8_lossy(&bytes).into_owned());
                    break;
                }
                Ok(Err(DownloadError::Transport(e))) if attempt < LONG_TEXT_ATTEMPTS => {
                    tracing::debug!(arrival, attempt, error = %e, "long text download broke off");
                    tokio::time::sleep(LONG_TEXT_RETRY_PAUSE.min(left)).await;
                }
                Ok(Err(e)) => {
                    tracing::warn!(arrival, error = %e, "long text download failed; trimmed body kept");
                    break;
                }
                Err(_) => {
                    tracing::warn!(arrival, "long text download timed out; trimmed body kept");
                    break;
                }
            }
        }
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer(mime: &str, flags: u32) -> proto::AttachmentPointer {
        proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "key-1".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            digest: Some(vec![2; 32]),
            size: Some(1234),
            content_type: Some(mime.to_owned()),
            flags: Some(flags),
            ..Default::default()
        }
    }

    #[test]
    fn kinds_follow_flags_then_content_type() {
        assert_eq!(kind_of("audio/aac", FLAG_VOICE), AttachmentKind::Voice);
        assert_eq!(kind_of("audio/mpeg", 0), AttachmentKind::Audio);
        assert_eq!(kind_of("video/mp4", FLAG_GIF), AttachmentKind::Gif);
        assert_eq!(kind_of("IMAGE/GIF", 0), AttachmentKind::Gif);
        assert_eq!(kind_of("image/jpeg", 0), AttachmentKind::Image);
        assert_eq!(kind_of("image/svg+xml", 0), AttachmentKind::File);
        assert_eq!(kind_of("video/mp4", 0), AttachmentKind::Video);
        assert_eq!(kind_of("application/pdf", 0), AttachmentKind::File);
    }

    #[test]
    fn pointers_map_with_their_metadata_and_the_pointer_as_remote() {
        let mut p = pointer("image/png", 0);
        p.width = Some(640);
        p.height = Some(480);
        p.blur_hash = Some("LEHV6nWB2yk8".to_owned());
        p.file_name = Some("photo.png".to_owned());
        p.caption = Some("look".to_owned());
        let a = attachment(&p, None);
        assert_eq!(a.kind, AttachmentKind::Image);
        assert_eq!(
            (a.mime.as_str(), a.size, a.dims),
            ("image/png", 1234, Some((640, 480)))
        );
        assert_eq!(a.blurhash.as_deref(), Some("LEHV6nWB2yk8"));
        assert_eq!(a.file_name.as_deref(), Some("photo.png"));
        assert_eq!(a.caption.as_ref().map(|c| c.body.as_str()), Some("look"));
        assert_eq!(a.state, AttachmentState::Remote);
        let remote = a.remote.expect("remote pointer");
        assert_eq!(remote.v, POINTER_BLOB_V);
        assert_eq!(Pointer::from_blob(&remote).unwrap().size, 1234);

        let mut voice = pointer("audio/aac", FLAG_VOICE);
        voice.audio_duration_seconds = Some(2.5);
        voice.audio_waveform = Some(vec![7; 150]);
        let v = attachment(&voice, None);
        assert_eq!(v.kind, AttachmentKind::Voice);
        assert_eq!(v.duration_ms, Some(2500));
        assert_eq!(v.waveform.map(|w| w.len()), Some(100));

        // Without key or digest the attachment is shown but cannot be downloaded.
        let broken = proto::AttachmentPointer {
            key: None,
            ..pointer("application/pdf", 0)
        };
        let b = attachment(&broken, None);
        assert_eq!(b.state, AttachmentState::Failed { retryable: false });
        assert!(b.remote.is_none());
        let untyped = proto::AttachmentPointer {
            content_type: None,
            ..pointer("", 0)
        };
        assert_eq!(attachment(&untyped, None).mime, "application/octet-stream");
    }

    #[test]
    fn stickers_are_images_and_long_texts_are_found_in_every_shown_message() {
        let s = proto::data_message::Sticker {
            data: Some(proto::AttachmentPointer {
                content_type: None,
                ..pointer("", 0)
            }),
            ..Default::default()
        };
        let image = sticker(&s).unwrap();
        assert_eq!(
            (image.kind, image.mime.as_str()),
            (AttachmentKind::Image, "image/webp")
        );
        assert!(sticker(&proto::data_message::Sticker::default()).is_none());

        let message = proto::DataMessage {
            body: Some("trimmed".to_owned()),
            attachments: vec![pointer(LONG_TEXT_MIME, 0), pointer("image/png", 0)],
            ..Default::default()
        };
        let direct = proto::Content {
            content: Some(ContentKind::DataMessage(message.clone())),
            ..Default::default()
        };
        let transcript = proto::Content {
            content: Some(ContentKind::SyncMessage(proto::SyncMessage {
                content: Some(proto::sync_message::Content::Sent(
                    proto::sync_message::Sent {
                        message: Some(message),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            })),
            ..Default::default()
        };
        let row = |arrival: u64, content: &proto::Content| Row {
            arrival,
            received_at: 1,
            kind: KIND_CONTENT.to_owned(),
            sender: String::new(),
            sender_device: 1,
            timestamp: 1,
            server_timestamp: None,
            sealed: false,
            content: Some(content.encode_to_vec()),
            attempts: 0,
            decrypt_error: None,
            content_hint: None,
            group_id: None,
            delivered_at: None,
        };
        let rows = [row(1, &direct), row(2, &transcript)];
        let found = long_text_pointers(&rows, &HashSet::new());
        assert_eq!(found.iter().map(|(a, _)| *a).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(found[0].1.content_type.as_deref(), Some(LONG_TEXT_MIME));
        // Nothing is fetched for a blocked sender; the others' texts still are.
        let from = |sender: &str, arrival: u64| Row {
            sender: sender.to_owned(),
            ..row(arrival, &direct)
        };
        let rows = [from("blocked-one", 3), from("someone", 4)];
        let blocked = HashSet::from(["blocked-one".to_owned()]);
        let found = long_text_pointers(&rows, &blocked);
        assert_eq!(found.iter().map(|(a, _)| *a).collect::<Vec<_>>(), [4]);
    }

    /// The step downloads nothing for the senders our phone blocked: they are read from the
    /// peers, and a sender who is not blocked (or unknown) is not among them.
    #[tokio::test(flavor = "current_thread")]
    async fn the_download_step_knows_the_blocked_senders() {
        let fixture = crate::receive::tests::fixture();
        let test =
            crate::actor::testing::shared(fixture.db.clone(), fixture.account.clone(), Vec::new());
        let from = |sender: &str, arrival: u64| Row {
            arrival,
            received_at: 1,
            kind: KIND_CONTENT.to_owned(),
            sender: sender.to_owned(),
            sender_device: 1,
            timestamp: 1,
            server_timestamp: None,
            sealed: false,
            content: None,
            attempts: 0,
            decrypt_error: None,
            content_hint: None,
            group_id: None,
            delivered_at: None,
        };
        let rows = [
            from("alice", 1),
            from("bob", 2),
            from("", 3),
            from("alice", 4),
        ];
        assert!(blocked_senders(&test.shared, &rows).is_empty());
        fixture
            .db
            .zone(|tx| crate::peers::set_blocked(tx, "alice", true, 5))
            .unwrap();
        assert_eq!(
            blocked_senders(&test.shared, &rows),
            HashSet::from(["alice".to_owned()])
        );
        fixture
            .db
            .zone(|tx| crate::peers::set_blocked(tx, "alice", false, 6))
            .unwrap();
        assert!(blocked_senders(&test.shared, &rows).is_empty());
    }

    /// A long text whose download the network breaks off is tried again in the same step; an
    /// answer of the CDN is final, and so is the step's deadline.
    #[tokio::test]
    async fn long_text_downloads_survive_a_broken_connection() {
        use crate::attachment::tests::{ScriptedCdn, encrypt};
        use crate::net::TransportError;
        let key = [4u8; 64];
        let text = "long ".repeat(1000);
        let (blob, digest) = encrypt(&key, [5; 16], text.as_bytes(), true);
        let wanted = |arrival: u64| {
            let pointer = proto::AttachmentPointer {
                key: Some(key.to_vec()),
                digest: Some(digest.to_vec()),
                size: Some(u32::try_from(text.len()).unwrap()),
                ..pointer(LONG_TEXT_MIME, 0)
            };
            (arrival, Pointer::from_proto(&pointer).unwrap())
        };
        let reset = || Err(TransportError::Https("connection reset".to_owned()));
        let in_time = || tokio::time::Instant::now() + NETWORK_STEP_TIMEOUT;
        let net = |cdn: &std::sync::Arc<ScriptedCdn>| {
            Net::new(cdn.clone(), tokio::runtime::Handle::current())
        };

        // Two broken connections, then the blob: the text arrives. The next row's 404 is
        // final and asked for once.
        let cdn = ScriptedCdn::answering(vec![
            reset(),
            reset(),
            Ok((200, blob.clone())),
            Ok((404, Vec::new())),
        ]);
        let texts = download_long_texts(&net(&cdn), vec![wanted(1), wanted(2)], in_time()).await;
        assert_eq!(texts.get(&1), Some(&text));
        assert!(!texts.contains_key(&2));
        assert_eq!(cdn.requests.lock().unwrap().len(), 4);

        // A connection that never holds: three tries, then the trimmed body has to do.
        let cdn = ScriptedCdn::answering(vec![reset(), reset(), reset(), Ok((200, blob.clone()))]);
        let texts = download_long_texts(&net(&cdn), vec![wanted(1)], in_time()).await;
        assert!(texts.is_empty());
        assert_eq!(cdn.requests.lock().unwrap().len(), 3);

        // Past the deadline nothing is asked for.
        let cdn = ScriptedCdn::answering(vec![Ok((200, blob))]);
        let texts =
            download_long_texts(&net(&cdn), vec![wanted(1)], tokio::time::Instant::now()).await;
        assert!(texts.is_empty());
        assert!(cdn.requests.lock().unwrap().is_empty());
    }
}
