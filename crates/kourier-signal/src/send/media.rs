//! The body of an outgoing message (§6.7 steps 1-2; `SD/ts/jobs/helpers/sendNormalMessage.preload.ts`
//! `getMessageSendData`, `SD/ts/util/uploadAttachment.preload.ts`, `SD/ts/util/longAttachment.std.ts`).
//!
//! [`Uploader::prepare`] turns a core [`Body`] into the parts of a `DataMessage`:
//!
//! - a text longer than 2 KiB goes out trimmed to 2 KiB (at a character boundary) with the whole
//!   text as a `text/x-signal-plain` attachment in front of the others (at most 64 KiB); the
//!   ranges always describe the whole text;
//! - every attachment of `Body::Media` is read from the core's blob (never copied to disk in
//!   plaintext), still images are re-encoded ([`encode_still`]: every metadata block dropped,
//!   which protects the sender's privacy, at [`PICTURE_LEVEL`], the best quality an official
//!   client sends; GIFs go out unchanged; an image that does not decode keeps its bytes with
//!   metadata stripped where possible), encrypted and uploaded ([`crate::upload`]);
//! - images and videos lose their file names (Desktop strips them), voice notes carry their
//!   flag, audio its duration and waveform. The GIF flag marks a looping *video* (the clients
//!   play such an attachment in their video player, `SD/ts/util/Attachment.std.ts` `isGIF`), so
//!   only a clip gets it; a GIF file is sent as the `image/gif` it is, like the official
//!   clients send one;
//! - at most [`MAX_INCREMENTAL_MACS`] attachments of a message keep their incremental MAC (up to
//!   8 KiB each; with more, a message of many videos would not fit the server's 256 KiB):
//!   Desktop's `MAX_INCREMENTAL_MAC_ATTACHMENTS` in `SD/ts/textsecure/SendMessage.preload.ts`,
//!   Android's remote key `global.maxIncrementalMacsPerEnvelope`, which Kourier reads too. The
//!   digest and the whole-blob MAC still protect the others. Kourier's own sends carry one
//!   video per message (`features::file_packing`), so today the cap can only bite on an edit
//!   of an album another of our devices sent.
//!
//! An edit ([`Uploader::prepare_edit`]) never changes files: an attachment that still has the
//! pointer it arrived with (our message from another device) goes out with that pointer; one
//! of this device is uploaded again, which within three days reuses the pointer of its first
//! upload (Desktop re-sends the pointers it has the same way,
//! `SD/ts/util/uploadAttachment.preload.ts`).
//!
//! A file sent "as file" arrives here as `AttachmentKind::File` whatever it holds (a picture,
//! a video): it is never decoded, re-encoded, scaled or stripped. Its bytes go out as they
//! are, under its own name and its true content type (`image/jpeg` for a JPEG). The protocol
//! has no marker for "a document": the official clients state the real type of a file picked
//! as one too (Android: `mms/DocumentSlide`), and a receiver needs the type to open or save
//! the file. A client that draws that type shows it as a picture or a video; what it saves
//! are the original bytes. The core packs such a file as a generic file, so it travels in a
//! message of its own (`features::file_packing`).
//!
//! A reply's quote ([`Uploader::quote`]) names the quoted message, its author, the text and
//! ranges the core showed, and the first quoted attachment with its thumbnail uploaded as an
//! attachment of its own (`SD/ts/jobs/helpers/sendNormalMessage.preload.ts` `uploadMessageQuote`).
//!
//! Reading and re-encoding run on the blocking pool. While uploads run, `TransmitProgress`
//! reports the share of blob bytes sent (below 1), and 1 once every upload is done; a body
//! without uploads reports nothing.

use std::path::{Component, Path};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    Attachment, AttachmentKind, BackendError, BlobRef, Body, ErrorKind, LocalMsgId, ReplyTo,
    TargetAuthor, Text,
};
use libsignal_core::Aci;
use prost::Message as _;
use zeroize::Zeroizing;

use super::content::{self, QuotedFile};
use crate::actor::Shared;
use crate::attachment::{POINTER_BLOB_V, Pointer, max_blob_len};
use crate::features::MAX_INLINE_BODY_BYTES;
use crate::net::SignalTransport;
use crate::proto;
use crate::service_id::{SignalUser, parse_user_id};
use crate::upload::{self, Plaintext, Spec, UploadError};

/// Longest text sent at all (`MAX_BODY_ATTACHMENT_BYTE_LENGTH`).
pub(crate) const MAX_TEXT_BYTES: usize = 64 * 1024;
/// Content type of a long text's attachment (`MIME.LONG_MESSAGE`).
pub(crate) const LONG_TEXT_MIME: &str = "text/x-signal-plain";
/// `AttachmentPointer.Flags.VOICE_MESSAGE`.
pub(crate) const FLAG_VOICE: u32 = 1;
/// `AttachmentPointer.Flags.GIF`.
pub(crate) const FLAG_GIF: u32 = 8;
/// Most waveform bars a pointer carries.
const MAX_WAVEFORM: usize = 100;
/// Most attachments of one message that carry an incremental MAC, without a remote config.
pub(crate) const MAX_INCREMENTAL_MACS: usize = 10;
/// The remote-config name of that number (Android's `RemoteConfig.maxIncrementalMacsPerEnvelope`).
const CONFIG_MAX_INCREMENTAL_MACS: &str = "global.maxIncrementalMacsPerEnvelope";
/// How often upload progress goes to the core.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// The quality still pictures are sent at: Signal's level three ("High" in the official
/// composers; `SD/ts/util/scaleImageToLevel.preload.ts` `MEDIA_QUALITY_LEVEL_DATA`): at most
/// 4096 px, a JPEG of at most 3 MiB at quality 0.75 once the file is over 0.4 MiB.
///
/// Desktop's composer sends level one (1600 px, 1 MiB) unless the user picks "High"; that
/// default is its own choice. No server and no receiver checks a picture's size below the
/// attachment limit, and every client shows level three, because the phones and Desktop send
/// it themselves. So Kourier does not copy the smaller default (CLAUDE.md, the limits rule).
/// Larger than level three is not sent: no official client produces it, so no receiver is
/// known to show it. The re-encode itself stays: it drops EXIF, GPS and XMP.
pub(crate) const PICTURE_LEVEL: kourier_media::ScaleLevel = kourier_media::ScaleLevel::THREE;
/// The name of that transform in the key of the upload cache ([`upload::source_key`]): a
/// pointer cached for another level is not reused.
const PICTURE_TRANSFORM: &str = "image-level-3";

/// The parts of a `DataMessage` an outgoing body becomes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PreparedBody {
    /// The body text (trimmed when a long-text attachment carries the whole).
    pub(crate) text: Option<String>,
    /// Ranges over the whole text.
    pub(crate) ranges: Vec<proto::BodyRange>,
    /// The uploaded attachments (the long text first).
    pub(crate) attachments: Vec<proto::AttachmentPointer>,
}

impl PreparedBody {
    /// A text-only body.
    #[cfg(test)]
    pub(crate) fn text(text: &Text) -> Self {
        let (inline, _) = split_long(text);
        Self {
            text: Some(inline),
            ranges: super::content::body_ranges(text),
            attachments: Vec::new(),
        }
    }
}

/// The first at most [`MAX_INLINE_BODY_BYTES`] bytes of `text` (at a character boundary), and
/// whether the text is longer (`trimBody`).
pub(crate) fn split_long(text: &Text) -> (String, bool) {
    let max = MAX_INLINE_BODY_BYTES as usize;
    if text.body.len() <= max {
        return (text.body.clone(), false);
    }
    let mut end = max;
    while !text.body.is_char_boundary(end) {
        end -= 1;
    }
    let trimmed = &text.body[..end];
    // Desktop's failover for degenerate cases: never an empty body.
    let trimmed = if trimmed.is_empty() {
        "\u{FFFE}"
    } else {
        trimmed
    };
    (trimmed.to_owned(), true)
}

/// Prepares the bodies of one `Transmit`: uploads go through the account's authenticated socket.
pub(crate) struct Uploader<'a, T: SignalTransport> {
    /// The account actor's state.
    pub(crate) shared: &'a Rc<Shared<T>>,
    /// The authenticated socket.
    pub(crate) socket: &'a Arc<T::Socket>,
    /// The core's message row (progress events).
    pub(crate) local: LocalMsgId,
    /// The network id of the message (stable per message: client uuids derive from it).
    pub(crate) net_local: &'a str,
}

/// One attachment of a message, before the uploads ran.
enum Slot {
    /// To upload.
    Upload(Box<Job>),
    /// The pointer it arrived with (an edit re-sends it).
    Pointer(Box<proto::AttachmentPointer>),
}

/// One upload of a message, before it ran.
struct Job {
    spec: Spec,
    /// The per-message pointer fields.
    fields: proto::AttachmentPointer,
    /// A still image: re-encoded before its upload.
    reencode: bool,
}

impl<T: SignalTransport> Uploader<'_, T> {
    /// The `DataMessage` parts of `body`, uploading what needs uploading.
    pub(crate) async fn prepare(&self, body: &Body) -> Result<PreparedBody, BackendError> {
        self.prepare_body(body, false).await
    }

    /// [`Self::prepare`] for the new revision of an edited message: its attachments are the
    /// message's own, so one that has the pointer it arrived with is sent with it.
    pub(crate) async fn prepare_edit(&self, body: &Body) -> Result<PreparedBody, BackendError> {
        self.prepare_body(body, true).await
    }

    /// `DataMessage.quote` for `reply`: the quoted message's id and author, the snapshot's text
    /// with its ranges, and the quoted attachment with its thumbnail uploaded.
    pub(crate) async fn quote(
        &self,
        reply: &ReplyTo,
        own: Aci,
    ) -> Result<proto::data_message::Quote, BackendError> {
        let invalid = |what: &'static str| BackendError::new(ErrorKind::InvalidInput, what);
        let id: u64 = reply
            .target
            .local
            .parse()
            .map_err(|_| invalid("the quoted message has no Signal id"))?;
        let author = match &reply.target.author {
            TargetAuthor::Own => own,
            TargetAuthor::User(user) => match parse_user_id(user) {
                Ok(SignalUser::Aci(aci)) => aci,
                _ => return Err(invalid("the quoted author is not a Signal account")),
            },
            TargetAuthor::Unknown => return Err(invalid("the quoted message has no author")),
        };
        let snapshot = reply.snapshot.as_ref();
        let file = match snapshot {
            Some(snapshot) if snapshot.attachment.is_some() => {
                let thumbnail = match snapshot.thumb.as_deref() {
                    Some(thumb) => self.quote_thumbnail(thumb).await?,
                    None => None,
                };
                Some(QuotedFile {
                    // Desktop quotes its GIFs (MP4 clips) as `image/gif`.
                    content_type: if snapshot.attachment == Some(AttachmentKind::Gif) {
                        Some("image/gif".to_owned())
                    } else {
                        snapshot.mime.clone().filter(|m| !m.is_empty())
                    },
                    file_name: snapshot.file_name.clone().filter(|n| !n.is_empty()),
                    thumbnail,
                })
            }
            _ => None,
        };
        Ok(content::quote(
            id,
            author,
            snapshot.and_then(|s| s.text.as_ref()),
            file,
        ))
    }

    /// Uploads the thumbnail of a quoted attachment (a small picture in a core blob). A blob
    /// that cannot be read or is no picture is left out; a failed upload fails the send, which
    /// is tried again.
    async fn quote_thumbnail(
        &self,
        thumb: &BlobRef,
    ) -> Result<Option<proto::AttachmentPointer>, BackendError> {
        if !is_relative_inside(&thumb.path) {
            return Ok(None);
        }
        let path = self.shared.blob_dir.join(&thumb.path);
        let key = thumb.key.clone();
        let source = upload::source_key(
            "quote-thumbnail",
            &[thumb.path.as_os_str().as_encoded_bytes(), &key.expose()[..]],
        );
        let read = tokio::task::spawn_blocking(move || {
            kourier_blob::read_all(&path, &kourier_blob::FileKey::from_bytes(*key.expose())).ok()
        })
        .await
        .map_err(|_| BackendError::internal("the thumbnail task stopped"))?;
        let Some(bytes) = read else {
            tracing::info!("a quote's thumbnail could not be read; sent without it");
            return Ok(None);
        };
        let Some(mime) = kourier_media::sniff_mime(&bytes).filter(|m| m.starts_with("image/"))
        else {
            return Ok(None);
        };
        let dims = kourier_media::decode(&bytes)
            .ok()
            .map(|picture| (picture.width(), picture.height()));
        let spec = Spec {
            plain: Plaintext::Memory(Arc::new(Zeroizing::new(bytes.to_vec()))),
            mime: mime.to_owned(),
            dims,
            source,
        };
        let shared = self.shared;
        let now = shared.now();
        let sent = Arc::new(AtomicU64::new(0));
        let uploaded = upload::upload(
            &shared.net,
            self.socket,
            &shared.db,
            spec,
            now,
            &sent,
            shared.tuning.upload_stall,
        )
        .await
        .map_err(|e: UploadError| {
            tracing::warn!(error = %e, "quote thumbnail upload failed");
            e.to_backend_error()
        })?;
        Ok(Some(merge(
            uploaded,
            proto::AttachmentPointer {
                content_type: Some(mime.to_owned()),
                ..Default::default()
            },
        )))
    }

    async fn prepare_body(&self, body: &Body, edit: bool) -> Result<PreparedBody, BackendError> {
        let (text, attachments): (Option<&Text>, &[Attachment]) = match body {
            Body::Text(text) => (Some(text), &[]),
            Body::Media {
                attachments,
                caption,
            } => (caption.as_ref(), attachments),
            Body::Sticker(_) => {
                return Err(BackendError::unsupported(
                    "sending stickers is not available",
                ));
            }
            _ => {
                return Err(BackendError::unsupported(
                    "this kind of message cannot be sent",
                ));
            }
        };
        let text = text.filter(|t| !t.body.is_empty());
        if attachments.is_empty() && text.is_none() {
            return Err(BackendError::new(ErrorKind::InvalidInput, "empty message"));
        }
        let mut prepared = PreparedBody::default();
        let mut slots = Vec::new();
        if let Some(text) = text {
            if text.body.len() > MAX_TEXT_BYTES {
                return Err(
                    BackendError::new(ErrorKind::TooLarge, "the text is too long")
                        .with_user_message("This message is too long to send."),
                );
            }
            let (inline, long) = split_long(text);
            prepared.text = Some(inline);
            prepared.ranges = super::content::body_ranges(text);
            if long {
                slots.push(Slot::Upload(Box::new(long_text_job(text))));
            }
        }
        for (index, attachment) in attachments.iter().enumerate() {
            // The long text of an earlier revision is not part of the new one.
            if attachment.long_text {
                continue;
            }
            let arrived_with = attachment.remote.as_ref().and_then(remote_pointer);
            slots.push(match arrived_with {
                Some(pointer) if edit || attachment.local.is_none() => {
                    Slot::Pointer(Box::new(pointer))
                }
                _ => Slot::Upload(Box::new(self.attachment_job(index, attachment)?)),
            });
        }
        prepared.attachments = self.run(slots).await?;
        let keep = self
            .shared
            .db
            .read(|c| crate::maintenance::remote_config_value(c, CONFIG_MAX_INCREMENTAL_MACS))
            .map_err(|e| e.to_backend_error())?
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(MAX_INCREMENTAL_MACS);
        cap_incremental_macs(&mut prepared.attachments, keep);
        Ok(prepared)
    }

    /// What to upload for `attachment`, and the pointer fields of this message.
    fn attachment_job(&self, index: usize, attachment: &Attachment) -> Result<Job, BackendError> {
        let local = attachment.local.as_ref().ok_or_else(|| {
            BackendError::new(ErrorKind::InvalidInput, "an attachment without its file")
        })?;
        if !is_relative_inside(&local.path) {
            return Err(BackendError::new(
                ErrorKind::InvalidInput,
                "an attachment file outside the blob directory",
            ));
        }
        let path = self.shared.blob_dir.join(&local.path);
        let still_image = attachment.kind == AttachmentKind::Image
            && !attachment.mime.eq_ignore_ascii_case("image/gif");
        let transform = if still_image {
            PICTURE_TRANSFORM
        } else {
            "as-is"
        };
        let source = upload::source_key(
            transform,
            &[
                local.path.as_os_str().as_encoded_bytes(),
                &local.key.expose()[..],
            ],
        );
        let plain = Plaintext::Blob {
            path,
            key: local.key.clone(),
            size: attachment.size,
        };
        let mut fields = proto::AttachmentPointer {
            content_type: Some(attachment.mime.clone()),
            client_uuid: Some(pointer_client_uuid(attachment, self.net_local, index)),
            blur_hash: attachment.blurhash.clone().filter(|b| !b.is_empty()),
            caption: attachment
                .caption
                .as_ref()
                .map(|c| c.body.clone())
                .filter(|c| !c.is_empty()),
            ..Default::default()
        };
        match attachment.kind {
            AttachmentKind::Image | AttachmentKind::Video => {}
            // The flag means "a video that loops": on an `image/gif` it would send the phones'
            // video players a file they cannot play.
            AttachmentKind::Gif => {
                if is_video(&attachment.mime) {
                    fields.flags = Some(FLAG_GIF);
                }
            }
            AttachmentKind::Voice | AttachmentKind::Audio | AttachmentKind::File => {
                fields.file_name = attachment.file_name.clone().filter(|n| !n.is_empty());
            }
        }
        if attachment.kind == AttachmentKind::Voice {
            fields.flags = Some(FLAG_VOICE);
        }
        if matches!(
            attachment.kind,
            AttachmentKind::Voice | AttachmentKind::Audio
        ) {
            fields.audio_duration_seconds = attachment.duration_ms.map(|ms| ms as f32 / 1000.0);
            fields.audio_waveform = attachment
                .waveform
                .clone()
                .map(|mut w| {
                    w.truncate(MAX_WAVEFORM);
                    w
                })
                .filter(|w| !w.is_empty());
        }
        Ok(Job {
            spec: Spec {
                plain,
                mime: attachment.mime.clone(),
                dims: attachment.dims,
                source,
            },
            fields,
            reencode: still_image,
        })
    }

    /// Uploads every job (re-encoding still images first, unless a recent upload is reused) and
    /// reports progress meanwhile; pointers that came with the message keep their place.
    async fn run(&self, slots: Vec<Slot>) -> Result<Vec<proto::AttachmentPointer>, BackendError> {
        if !slots.iter().any(|slot| matches!(slot, Slot::Upload(_))) {
            return Ok(slots
                .into_iter()
                .filter_map(|slot| match slot {
                    Slot::Pointer(pointer) => Some(*pointer),
                    Slot::Upload(_) => None,
                })
                .collect());
        }
        let total: u64 = slots
            .iter()
            .map(|slot| match slot {
                Slot::Upload(job) => max_blob_len(job.spec.plain.len()),
                Slot::Pointer(_) => 0,
            })
            .sum();
        let sent = Arc::new(AtomicU64::new(0));
        let ticker = tokio::task::spawn_local(report_progress(
            self.shared.clone(),
            self.local,
            sent.clone(),
            total.max(1),
        ));
        let result = self.upload_all(slots, &sent).await;
        ticker.abort();
        if result.is_ok() {
            let _ = self
                .shared
                .emit(BackendEvent::TransmitProgress {
                    local: self.local,
                    progress: 1.0,
                })
                .await;
        }
        result
    }

    async fn upload_all(
        &self,
        slots: Vec<Slot>,
        sent: &Arc<AtomicU64>,
    ) -> Result<Vec<proto::AttachmentPointer>, BackendError> {
        let shared = self.shared;
        let mut pointers = Vec::with_capacity(slots.len());
        for slot in slots {
            let Job {
                mut spec,
                fields,
                reencode,
            } = match slot {
                Slot::Upload(job) => *job,
                Slot::Pointer(pointer) => {
                    pointers.push(*pointer);
                    continue;
                }
            };
            let now = shared.now();
            let reused = shared
                .db
                .read(|c| upload::cached(c, &spec.source, now))
                .map_err(|e| e.to_backend_error())?;
            if reencode && reused.is_none() {
                spec = reencoded(spec).await?;
            }
            let uploaded = upload::upload(
                &shared.net,
                self.socket,
                &shared.db,
                spec,
                now,
                sent,
                shared.tuning.upload_stall,
            )
            .await
            .map_err(|e: UploadError| {
                tracing::warn!(error = %e, "attachment upload failed");
                e.to_backend_error()
            })?;
            pointers.push(merge(uploaded, fields));
        }
        Ok(pointers)
    }
}

/// Whether a MIME type is a video's.
fn is_video(mime: &str) -> bool {
    mime.get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("video/"))
}

/// Leaves the incremental MAC on the first `keep` pointers that have one and takes it (with
/// its chunk size) off the rest, as Desktop's `toProto` does.
pub(crate) fn cap_incremental_macs(pointers: &mut [proto::AttachmentPointer], keep: usize) {
    let mut kept = 0;
    for pointer in pointers {
        if pointer.incremental_mac.is_none() {
            continue;
        }
        if kept < keep {
            kept += 1;
        } else {
            pointer.incremental_mac = None;
            pointer.chunk_size = None;
        }
    }
}

/// The `AttachmentPointer` an attachment arrived with (`Attachment::remote`, what
/// `receive::media::attachment` stored), when it is complete.
fn remote_pointer(remote: &kourier_model::BackendBlob) -> Option<proto::AttachmentPointer> {
    if remote.v != POINTER_BLOB_V {
        return None;
    }
    let pointer = proto::AttachmentPointer::decode(&remote.bytes[..]).ok()?;
    Pointer::from_proto(&pointer).ok()?;
    Some(pointer)
}

/// The upload of a long text (`text/x-signal-plain`, no file name).
fn long_text_job(text: &Text) -> Job {
    let bytes = Arc::new(Zeroizing::new(text.body.as_bytes().to_vec()));
    Job {
        spec: Spec {
            source: upload::source_key("long-text", &[&bytes[..]]),
            plain: Plaintext::Memory(bytes),
            mime: LONG_TEXT_MIME.to_owned(),
            dims: None,
        },
        fields: proto::AttachmentPointer {
            content_type: Some(LONG_TEXT_MIME.to_owned()),
            ..Default::default()
        },
        reencode: false,
    }
}

/// The upload fields of `uploaded` (CDN, key, digest, size, type, dimensions) with this
/// message's fields.
fn merge(
    uploaded: proto::AttachmentPointer,
    fields: proto::AttachmentPointer,
) -> proto::AttachmentPointer {
    proto::AttachmentPointer {
        attachment_identifier: uploaded.attachment_identifier,
        cdn_number: uploaded.cdn_number,
        key: uploaded.key,
        size: uploaded.size,
        digest: uploaded.digest,
        incremental_mac: uploaded.incremental_mac,
        chunk_size: uploaded.chunk_size,
        upload_timestamp: uploaded.upload_timestamp,
        content_type: uploaded.content_type.or(fields.content_type),
        width: uploaded.width.or(fields.width),
        height: uploaded.height.or(fields.height),
        ..fields
    }
}

/// What a still picture is sent as: its bytes, MIME type and size in pixels. The picture is
/// re-encoded at [`PICTURE_LEVEL`] without any metadata block; one that does not decode here
/// (HEIC, a damaged file) keeps its bytes, without metadata where the format allows, and the
/// `mime` and `dims` the core gave it.
fn encode_still(
    original: &[u8],
    mime: &str,
    dims: Option<(u32, u32)>,
) -> (Vec<u8>, String, Option<(u32, u32)>) {
    match kourier_media::scale_to_level(original, PICTURE_LEVEL) {
        Ok(encoded) => (
            encoded.bytes,
            encoded.mime.to_owned(),
            Some((encoded.width, encoded.height)),
        ),
        Err(e) => {
            tracing::info!(error = %e, "image not re-encoded; sent as it is");
            match kourier_media::strip_metadata(original) {
                Ok(stripped) => (stripped.bytes, stripped.mime.to_owned(), dims),
                Err(_) => (original.to_vec(), mime.to_owned(), dims),
            }
        }
    }
}

/// Re-encodes a still image ([`encode_still`]; blocking pool); the plaintext then lives in
/// memory.
async fn reencoded(spec: Spec) -> Result<Spec, BackendError> {
    tokio::task::spawn_blocking(move || {
        let Plaintext::Blob { path, key, .. } = &spec.plain else {
            return Ok(spec);
        };
        let original =
            kourier_blob::read_all(path, &kourier_blob::FileKey::from_bytes(*key.expose()))
                .map_err(|_| {
                    BackendError::new(
                        ErrorKind::InvalidInput,
                        "the image to send could not be read",
                    )
                })?;
        let (bytes, mime, dims) = encode_still(&original, &spec.mime, spec.dims);
        Ok(Spec {
            plain: Plaintext::Memory(Arc::new(Zeroizing::new(bytes))),
            mime,
            dims,
            source: spec.source,
        })
    })
    .await
    .map_err(|_| BackendError::internal("the image task stopped"))?
}

/// A relative path without `..`, root or prefix components (it stays inside the blob dir).
fn is_relative_inside(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// A UUID v4-shaped `clientUuid` derived from the message and the attachment's index (its
/// position in the message's `Body::Media`), so a retried send names its attachments the same
/// way, and the core can be told the name before anything is sent
/// ([`attachment_net_ids`]).
pub(crate) fn client_uuid(net_local: &str, index: usize) -> Vec<u8> {
    use sha2::{Digest as _, Sha256};
    let hash = Sha256::new()
        .chain_update(b"kourier client uuid\0")
        .chain_update(net_local.as_bytes())
        .chain_update((index as u64).to_be_bytes())
        .finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .as_bytes()
        .to_vec()
}

/// The `clientUuid` of the pointer of `attachment`, number `index` of the message or edit
/// `net_local`: the name the core holds for it when it has one (`Attachment::net_ids`, which
/// it got from [`attachment_net_ids`] when the message was queued), else the derived one.
/// So an edit, which has an id of its own, sends its message's attachments under the names
/// they went out with, and a delete of one of them on either device still finds it.
fn pointer_client_uuid(attachment: &Attachment, net_local: &str, index: usize) -> Vec<u8> {
    crate::receive::media::uuid_of_net_ids(&attachment.net_ids)
        .unwrap_or_else(|| client_uuid(net_local, index))
}

/// `Network::attachment_net_ids`: the names attachment `index` of our own message
/// `net_local` has before it is sent, in the form incoming attachments carry them
/// (`receive::media::net_id_uuid`). Our phone names an attachment of ours it deleted by this
/// client UUID (`SyncMessage.deleteForMe.attachmentDeletes`), and Kourier names it the same
/// way when the user deletes it here. The digest of the upload is not known before the send.
pub(crate) fn attachment_net_ids(net_local: &str, index: usize) -> Vec<String> {
    crate::receive::media::net_id_uuid(&client_uuid(net_local, index))
        .into_iter()
        .collect()
}

/// Sends `TransmitProgress` every [`PROGRESS_EVERY`] while it changes (aborted when done).
async fn report_progress<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    local: LocalMsgId,
    sent: Arc<AtomicU64>,
    total: u64,
) {
    let mut last = u64::MAX;
    loop {
        tokio::time::sleep(PROGRESS_EVERY).await;
        let now = sent.load(Ordering::Relaxed).min(total);
        if now == last {
            continue;
        }
        last = now;
        // Never 1.0 from here: `run` reports that once every upload is done.
        let progress = (now as f32 / total as f32).min(0.99);
        if shared
            .emit(BackendEvent::TransmitProgress { local, progress })
            .await
            .is_err()
        {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use kourier_model::{RangeKind, TextRange};

    use super::*;

    #[test]
    fn long_texts_are_trimmed_at_a_character_boundary() {
        let short = Text::plain("hello");
        assert_eq!(split_long(&short), ("hello".to_owned(), false));
        // 2047 ASCII bytes and a 3-byte character across the limit.
        let body = format!("{}€ and more", "a".repeat(2047));
        let mut long = Text::plain(&body);
        long.ranges.push(TextRange {
            start: 2047,
            len: 3,
            kind: RangeKind::Bold,
        });
        let (inline, is_long) = split_long(&long);
        assert!(is_long);
        assert_eq!(inline.len(), 2047);
        assert!(body.starts_with(&inline));
        // The ranges still describe the whole text.
        let prepared = PreparedBody::text(&long);
        assert_eq!(prepared.ranges.len(), 1);
        assert_eq!(prepared.ranges[0].start, Some(2047));
    }

    #[test]
    fn paths_must_stay_inside_and_uuids_are_stable() {
        assert!(is_relative_inside(Path::new("core/ab/abcdef")));
        assert!(!is_relative_inside(Path::new("/etc/passwd")));
        assert!(!is_relative_inside(Path::new("core/../../x")));
        assert!(!is_relative_inside(Path::new("")));
        let a = client_uuid("1700000000000", 0);
        assert_eq!(a, client_uuid("1700000000000", 0));
        assert_ne!(a, client_uuid("1700000000000", 1));
        assert_eq!(a.len(), 16);
        assert_eq!(a[6] >> 4, 4, "version 4 layout");
        // The name the core stores for an own attachment is the one the pointer will carry,
        // in the form a received attachment has it.
        let names = attachment_net_ids("1700000000000", 0);
        assert_eq!(
            names,
            [crate::receive::media::net_id_uuid(&a).unwrap()],
            "the client UUID of the pointer"
        );
        assert!(names[0].starts_with("uuid:") && names[0].len() == 5 + 32);
        assert_ne!(names, attachment_net_ids("1700000000000", 1));
        assert_ne!(names, attachment_net_ids("1700000000001", 0));
        // The pointer carries the name the core holds: the same for the message itself, and
        // still the message's name when an edit (with an id of its own) sends the file again.
        let mut attachment = Attachment {
            net_ids: names.clone(),
            ..Attachment::default()
        };
        assert_eq!(pointer_client_uuid(&attachment, "1700000000000", 0), a);
        assert_eq!(
            pointer_client_uuid(&attachment, "1700000099999", 0),
            a,
            "an edit keeps the attachment's name"
        );
        // An attachment the core holds no name for (queued by an older version) gets the
        // derived one, as before.
        attachment.net_ids.clear();
        assert_eq!(
            pointer_client_uuid(&attachment, "1700000099999", 3),
            client_uuid("1700000099999", 3)
        );
        attachment.net_ids = vec!["digest:00".to_owned(), "uuid:xyz".to_owned()];
        assert_eq!(
            pointer_client_uuid(&attachment, "5", 1),
            client_uuid("5", 1),
            "a malformed name is no name"
        );
    }

    /// `TransmitProgress` follows the blob bytes sent: one event per change, as a share of the
    /// total, and never 1.0 from the ticker (that one marks the finished uploads).
    #[tokio::test(flavor = "current_thread")]
    async fn progress_events_follow_the_bytes_sent() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let fixture = crate::receive::tests::fixture();
                let test = crate::actor::testing::shared(
                    fixture.db.clone(),
                    fixture.account.clone(),
                    Vec::new(),
                );
                let mut core = test.core;
                let shared = Rc::new(test.shared);
                let local = kourier_model::MessageId(7);
                let sent = Arc::new(AtomicU64::new(0));
                let ticker =
                    tokio::task::spawn_local(report_progress(shared, local, sent.clone(), 1000));
                let mut seen = Vec::new();
                // Unchanged (250 twice) is not reported again; past the total is capped.
                for bytes in [0u64, 250, 250, 900, 5000] {
                    sent.store(bytes, Ordering::Relaxed);
                    tokio::time::sleep(PROGRESS_EVERY + Duration::from_millis(60)).await;
                    while let Ok(envelope) = core.try_recv() {
                        assert!(envelope.ack.is_none(), "progress is ephemeral");
                        for event in envelope.events {
                            match event {
                                BackendEvent::TransmitProgress { local: l, progress } => {
                                    assert_eq!(l, local);
                                    seen.push(progress);
                                }
                                other => panic!("{other:?}"),
                            }
                        }
                    }
                }
                ticker.abort();
                assert_eq!(seen, [0.0, 0.25, 0.9, 0.99]);
            })
            .await;
    }

    /// A still picture goes out at the best quality an official client sends (level three),
    /// not at the 1600 px of Desktop's composer default: a 2400 × 1800 photo keeps its size
    /// (the steps of the scale are `kourier_media`'s tests), a small picture keeps its size
    /// and kind, and what does not decode is sent as it is.
    #[test]
    fn still_pictures_are_sent_at_the_high_quality_level() {
        use kourier_media::{SampleFormat, sample_image};
        let level = PICTURE_LEVEL;
        assert_eq!(
            (level.max_dimensions, level.target_size),
            (4096, 3 * 1024 * 1024)
        );
        let size_of = |bytes: &[u8]| u64::try_from(bytes.len()).unwrap();
        let sent = |width: u32, height: u32| {
            let photo = sample_image(width, height, 3, SampleFormat::Png).unwrap();
            assert!(
                size_of(&photo) > level.threshold_size,
                "{width} × {height}: large enough to be re-encoded as a JPEG"
            );
            let (bytes, mime, dims) = encode_still(&photo, "image/png", None);
            assert_eq!(mime, "image/jpeg");
            assert!(size_of(&bytes) <= level.target_size);
            let decoded = kourier_media::decode(&bytes).unwrap();
            assert_eq!(dims, Some((decoded.width(), decoded.height())));
            dims
        };
        assert_eq!(
            sent(2400, 1800),
            Some((2400, 1800)),
            "its own size, not the 1600 px of Desktop's default level"
        );
        // A small picture keeps its size and kind.
        let small = sample_image(320, 200, 4, SampleFormat::Png).unwrap();
        let (bytes, mime, dims) = encode_still(&small, "image/png", None);
        assert_eq!((mime.as_str(), dims), ("image/png", Some((320, 200))));
        assert!(kourier_media::decode(&bytes).is_ok());
        // Not a picture Kourier decodes: the bytes, MIME type and size it came with.
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic not really a picture".to_vec();
        let (bytes, mime, dims) = encode_still(&heic, "image/heic", Some((4032, 3024)));
        assert_eq!(bytes, heic);
        assert_eq!((mime.as_str(), dims), ("image/heic", Some((4032, 3024))));
        // The upload cache keeps levels apart.
        assert_eq!(PICTURE_TRANSFORM, "image-level-3");
    }

    /// A message of many videos (an album our phone sent, edited here): ten keep their
    /// incremental MAC, the rest travel without (the message would not fit the server's limit
    /// otherwise); pointers without one do not count.
    #[test]
    fn at_most_ten_attachments_of_a_message_keep_an_incremental_mac() {
        let video = || proto::AttachmentPointer {
            incremental_mac: Some(vec![7; 8 * 1024]),
            chunk_size: Some(65_536),
            digest: Some(vec![2; 32]),
            ..Default::default()
        };
        let picture = || proto::AttachmentPointer {
            digest: Some(vec![3; 32]),
            ..Default::default()
        };
        let mut pointers: Vec<_> = (0..31)
            .map(|i| if i % 3 == 0 { picture() } else { video() })
            .collect();
        let videos = pointers
            .iter()
            .filter(|p| p.incremental_mac.is_some())
            .count();
        assert_eq!(videos, 20);
        cap_incremental_macs(&mut pointers, MAX_INCREMENTAL_MACS);
        let with_mac: Vec<usize> = pointers
            .iter()
            .enumerate()
            .filter(|(_, p)| p.incremental_mac.is_some())
            .map(|(i, _)| i)
            .collect();
        assert_eq!(with_mac.len(), 10);
        assert_eq!(with_mac[0], 1, "the first ten videos, in order");
        assert!(
            pointers
                .iter()
                .all(|p| p.incremental_mac.is_some() == p.chunk_size.is_some())
        );
        assert!(
            pointers.iter().all(|p| p.digest.is_some()),
            "nothing else is touched"
        );
        // The whole album fits a message with room to spare for the body and the envelope.
        let size: usize = pointers.iter().map(prost::Message::encoded_len).sum();
        assert!(size < 128 * 1024, "{size}");
        assert!(is_video("video/mp4") && is_video("VIDEO/webm"));
        assert!(!is_video("image/gif") && !is_video("vid"));
    }

    #[test]
    fn merged_pointers_keep_upload_fields_and_message_fields() {
        let uploaded = proto::AttachmentPointer {
            attachment_identifier: Some(proto::attachment_pointer::AttachmentIdentifier::CdnKey(
                "k".to_owned(),
            )),
            cdn_number: Some(3),
            key: Some(vec![1; 64]),
            size: Some(10),
            digest: Some(vec![2; 32]),
            content_type: Some("image/jpeg".to_owned()),
            width: Some(1600),
            height: Some(1200),
            upload_timestamp: Some(5),
            ..Default::default()
        };
        let fields = proto::AttachmentPointer {
            content_type: Some("image/png".to_owned()),
            width: Some(2400),
            height: Some(1800),
            blur_hash: Some("LKO2".to_owned()),
            flags: Some(FLAG_GIF),
            ..Default::default()
        };
        let merged = merge(uploaded, fields);
        assert_eq!(merged.content_type.as_deref(), Some("image/jpeg"));
        assert_eq!((merged.width, merged.height), (Some(1600), Some(1200)));
        assert_eq!(merged.blur_hash.as_deref(), Some("LKO2"));
        assert_eq!(merged.flags, Some(FLAG_GIF));
        assert_eq!(merged.cdn_number, Some(3));
    }
}
