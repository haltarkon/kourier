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
//! Reading and re-encoding run on the blocking pool. Every upload of a message is made ready
//! FIRST (a recent pointer looked up, a still picture re-encoded), so the size of each is final
//! before the first byte goes up; then they go one after another. While uploads run, the
//! ticker ([`report_progress`]) reports each attachment's own `AttachmentProgress` (plaintext
//! bytes, `done == total` only once its upload is over) and `TransmitProgress`, the share of
//! blob bytes sent weighted by those final sizes (below 1, never down), and 1 once every upload
//! is done; a file whose upload was over before a tick says so when they are all done
//! ([`Uploader::report_files_full`]), so every file that uploaded ends with `done == total`; a
//! body without uploads reports nothing. A re-encoded picture waits for its upload in
//! a temporary encrypted file ([`tmp`]), never in memory all at once: an album of 31 would hold
//! ~93 MiB.

use std::cell::RefCell;
use std::path::{Component, Path};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use kourier_backend_api::BackendEvent;
use kourier_model::{
    Attachment, AttachmentKind, BackendError, BlobRef, Body, ErrorKind, LocalMsgId, ReplyTo,
    SendPhase, TargetAuthor, Text,
};
use libsignal_core::Aci;
use prost::Message as _;
use zeroize::Zeroizing;

use super::content::{self, QuotedFile};
use super::stop::{Stop, stopped};
use crate::actor::Shared;
use crate::attachment::{POINTER_BLOB_V, Pointer, max_blob_len};
use crate::features::MAX_INLINE_BODY_BYTES;
use crate::net::{AbortOnDrop, SignalTransport};
use crate::proto;
use crate::service_id::{SignalUser, parse_user_id};
use crate::upload::{self, Plaintext, Spec, UploadError};

pub(crate) mod tmp;
use tmp::TempFiles;

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
/// pointer cached for another level is not reused. "-clean" since LOC (N6): a pointer made
/// before `clean_still`, when a picture that did not clean left as it came, is not reused.
const PICTURE_TRANSFORM: &str = "image-level-3-clean";

/// The parts of a `DataMessage` an outgoing body becomes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct PreparedBody {
    /// The body text (trimmed when a long-text attachment carries the whole).
    pub(crate) text: Option<String>,
    /// Ranges over the whole text.
    pub(crate) ranges: Vec<proto::BodyRange>,
    /// The uploaded attachments (the long text first).
    pub(crate) attachments: Vec<proto::AttachmentPointer>,
    /// The shared contact card (`DataMessage.contact`; at most one, P5c).
    pub(crate) contact: Vec<proto::data_message::Contact>,
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
            contact: Vec::new(),
        }
    }
}

/// The largest name of a card Kourier sends, in bytes: Kourier's own cap (architecture §13;
/// no Signal client states one), so a card stays far below the message size limit.
pub(crate) const MAX_CARD_NAME_BYTES: usize = 512;

/// The `DataMessage.Contact` of a card Kourier sends (P5c, the narrow send of BQ-10): the name
/// as `givenName` and each phone number as a MOBILE `Phone`; nothing else (no avatar, no
/// `aciBinary`, BQ-7). The core builds the card (the profile name, the SHOWN number); this only
/// encodes it and refuses one without a name (Android shares no card without one) or without
/// exactly one phone number.
pub(crate) fn contact_card(
    card: &kourier_model::ContactCard,
) -> Result<proto::data_message::Contact, BackendError> {
    use proto::data_message::contact;
    let name = card
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            BackendError::new(ErrorKind::InvalidInput, "a contact card without a name")
        })?;
    if name.len() > MAX_CARD_NAME_BYTES {
        return Err(BackendError::new(
            ErrorKind::TooLarge,
            "the name of the contact card is too long",
        ));
    }
    let number: Vec<_> = card
        .identifiers
        .iter()
        .filter(|i| i.kind == kourier_model::IdentifierKind::Phone)
        .map(|i| contact::Phone {
            value: Some(i.value.clone()),
            r#type: Some(contact::phone::Type::Mobile as i32),
            label: None,
        })
        .collect();
    // ONE number (SC3, SD3): the core builds a card with exactly the shown number; a card with
    // none or several is not one Kourier sends.
    if number.len() != 1 {
        return Err(BackendError::new(
            ErrorKind::InvalidInput,
            "a contact card carries exactly one phone number",
        ));
    }
    Ok(proto::data_message::Contact {
        name: Some(contact::Name {
            given_name: Some(name.to_owned()),
            ..Default::default()
        }),
        number,
        ..Default::default()
    })
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
    /// The attempt's stop state: the preparations end when an abort is accepted, and the send
    /// passes its point of no return through it ([`Self::pass_point_of_no_return`]).
    pub(crate) stop: &'a Stop,
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
    /// The attachment's position in the request's `Body::Media.attachments` (what
    /// `AttachmentProgress.index` names); `None` for a long text's file, which is not reported.
    index: Option<u16>,
}

/// One upload as the progress reports see it, once its size is final.
struct Track {
    /// The position of the attachment in the request, `None`: only counted in the share.
    index: Option<u16>,
    /// Plaintext bytes this attempt uploads for it: after the re-encode, or the size of the
    /// pointer a recent upload left.
    plain: u64,
    /// The blob length those bytes make ([`max_blob_len`]): the unit of `sent`.
    blob: u64,
    /// Blob bytes the requests took ([`upload::upload`] adds to it; a resumed request adds the
    /// resent part again, so it can pass `blob`).
    sent: Arc<AtomicU64>,
    /// The upload is over, or never had to run: only then `done == total`.
    over: AtomicBool,
    /// The `done` of this file the last report said ([`NOT_REPORTED`] before the first): the
    /// ticker reports a change only, and the report that ends the uploads says the files it
    /// has not told full yet.
    reported: AtomicU64,
}

/// [`Track::reported`] before the first report of a file.
const NOT_REPORTED: u64 = u64::MAX;

impl Track {
    fn new(index: Option<u16>, plain: u64, over: bool) -> Self {
        Self {
            index,
            plain,
            blob: max_blob_len(plain),
            sent: Arc::new(AtomicU64::new(0)),
            over: AtomicBool::new(over),
            reported: AtomicU64::new(NOT_REPORTED),
        }
    }

    /// Blob bytes done, never past the blob.
    fn blob_done(&self) -> u64 {
        if self.over.load(Ordering::Acquire) {
            self.blob
        } else {
            self.sent.load(Ordering::Relaxed).min(self.blob)
        }
    }

    /// Plaintext bytes done: the blob's share scaled to the plaintext, and `plain` itself only
    /// once the upload is over (the last chunk is queued for the request before the CDN has
    /// it).
    fn plain_done(&self) -> u64 {
        if self.over.load(Ordering::Acquire) {
            return self.plain;
        }
        let scaled =
            u128::from(self.blob_done()) * u128::from(self.plain) / u128::from(self.blob.max(1));
        u64::try_from(scaled)
            .unwrap_or(self.plain)
            .min(self.plain.saturating_sub(1))
    }
}

/// The uploads of one attempt, in the order they run; filled when every size is final.
type Tracks = Rc<RefCell<Vec<Arc<Track>>>>;

/// One slot after the preparation: its size is final.
enum Ready {
    /// The pointer it arrived with.
    Pointer(Box<proto::AttachmentPointer>),
    /// To upload (or to reuse a recent pointer for).
    Upload(Box<ReadyUpload>),
}

/// An upload whose size is final.
struct ReadyUpload {
    spec: Spec,
    fields: proto::AttachmentPointer,
    /// The look-up's clock: the upload re-reads the cache with the same value.
    now: i64,
    track: Arc<Track>,
}

impl<T: SignalTransport> Uploader<'_, T> {
    /// The `DataMessage` parts of `body`, uploading what needs uploading. Ends with `Err` when
    /// an abort is accepted meanwhile: the uploads in progress are dropped where they stand.
    pub(crate) async fn prepare(&self, body: &Body) -> Result<PreparedBody, BackendError> {
        self.stop
            .until_stopped(self.prepare_body(body, false))
            .await
    }

    /// [`Self::prepare`] for the new revision of an edited message: its attachments are the
    /// message's own, so one that has the pointer it arrived with is sent with it.
    pub(crate) async fn prepare_edit(&self, body: &Body) -> Result<PreparedBody, BackendError> {
        self.stop.until_stopped(self.prepare_body(body, true)).await
    }

    /// The send is built and goes on the wire next: an abort is too late from here. `Err` when
    /// one was accepted first (nothing may be sent then).
    pub(crate) fn pass_point_of_no_return(&self) -> Result<(), BackendError> {
        if self.stop.close() {
            Ok(())
        } else {
            Err(stopped())
        }
    }

    /// `DataMessage.quote` for `reply`: the quoted message's id and author, the snapshot's text
    /// with its ranges, and the quoted attachment with its thumbnail uploaded. Ends with `Err`
    /// when an abort is accepted meanwhile.
    pub(crate) async fn quote(
        &self,
        reply: &ReplyTo,
        own: Aci,
    ) -> Result<proto::data_message::Quote, BackendError> {
        self.stop.until_stopped(self.quote_of(reply, own)).await
    }

    async fn quote_of(
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
            // A card is never a revision of another message.
            Body::Contact(card) if !edit => {
                return Ok(PreparedBody {
                    contact: vec![contact_card(card)?],
                    ..PreparedBody::default()
                });
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
            index: u16::try_from(index).ok(),
        })
    }

    /// Uploads every job and reports progress meanwhile; pointers that came with the message
    /// keep their place.
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
        // The ticker runs from the start and says `Preparing` (share 0, nothing to report per
        // file yet) until every picture is re-encoded and the uploads are shown to it: the line
        // under the message reads "Preparing files" and the X is there while that goes on.
        let tracks: Tracks = Rc::new(RefCell::new(Vec::new()));
        // The ticker ends with this future: an upload that is dropped (the attempt was stopped)
        // must not leave a task waking four times a second for as long as the actor lives.
        let ticker = AbortOnDrop(tokio::task::spawn_local(report_progress(
            self.shared.clone(),
            self.local,
            tracks.clone(),
        )));
        // The re-encoded pictures wait in files of this attempt, removed when it ends, whatever
        // way (a dropped future runs the destructor too).
        let temp = Arc::new(TempFiles::new(self.shared.upload_tmp.clone()));
        let result = self.upload_all(slots, &tracks, &temp).await;
        // Before the final report below: a late tick must not report 0.99 after it.
        drop(ticker);
        if result.is_ok() {
            // An upload that was over before a tick, or since the last one, is told full now:
            // every file that uploaded ends with `done == total`.
            self.report_files_full(&tracks).await;
            // The uploads are over: no step. A 1:1 send reports none after this (the sealed
            // send, the sync transcript), and the line under the message would go on saying
            // "Uploading files"; a group send reports its own next step.
            let _ = self
                .shared
                .emit(BackendEvent::TransmitProgress {
                    local: self.local,
                    progress: 1.0,
                    phase: None,
                })
                .await;
        }
        result
    }

    /// Reports every file whose last report was not "full" as full (the uploads are over).
    async fn report_files_full(&self, tracks: &Tracks) {
        let uploads: Vec<Arc<Track>> = tracks.borrow().clone();
        for track in uploads {
            let Some(index) = track.index else { continue };
            if track.reported.swap(track.plain, Ordering::Relaxed) == track.plain {
                continue;
            }
            let _ = self
                .shared
                .emit(BackendEvent::AttachmentProgress {
                    local: self.local,
                    index,
                    done: track.plain,
                    total: track.plain,
                })
                .await;
        }
    }

    /// Makes every slot ready (a recent pointer looked up, a still picture re-encoded: the
    /// size of each is final then), shows the uploads to the ticker, and uploads them one after
    /// another.
    async fn upload_all(
        &self,
        slots: Vec<Slot>,
        tracks: &Tracks,
        temp: &Arc<TempFiles>,
    ) -> Result<Vec<proto::AttachmentPointer>, BackendError> {
        let shared = self.shared;
        let mut ready = Vec::with_capacity(slots.len());
        for slot in slots {
            let Job {
                mut spec,
                fields,
                reencode,
                index,
            } = match slot {
                Slot::Upload(job) => *job,
                Slot::Pointer(pointer) => {
                    ready.push(Ready::Pointer(pointer));
                    continue;
                }
            };
            let now = shared.now();
            let reused = shared
                .db
                .read(|c| upload::cached(c, &spec.source, now))
                .map_err(|e| e.to_backend_error())?;
            let track = match &reused {
                // Nothing to upload: the pointer's own size is the file's, full at once.
                Some(pointer) => Track::new(
                    index,
                    pointer.size.map_or_else(|| spec.plain.len(), u64::from),
                    true,
                ),
                None => {
                    if reencode {
                        spec = reencoded(spec, temp.clone()).await?;
                    }
                    Track::new(index, spec.plain.len(), false)
                }
            };
            ready.push(Ready::Upload(Box::new(ReadyUpload {
                spec,
                fields,
                now,
                track: Arc::new(track),
            })));
        }
        tracks
            .borrow_mut()
            .extend(ready.iter().filter_map(|r| match r {
                Ready::Upload(upload) => Some(upload.track.clone()),
                Ready::Pointer(_) => None,
            }));
        let mut pointers = Vec::with_capacity(ready.len());
        for slot in ready {
            let ReadyUpload {
                spec,
                fields,
                now,
                track,
            } = match slot {
                Ready::Pointer(pointer) => {
                    pointers.push(*pointer);
                    continue;
                }
                Ready::Upload(upload) => *upload,
            };
            let uploaded = upload::upload(
                &shared.net,
                self.socket,
                &shared.db,
                spec,
                now,
                &track.sent,
                shared.tuning.upload_stall,
            )
            .await
            .map_err(|e: UploadError| {
                tracing::warn!(error = %e, "attachment upload failed");
                e.to_backend_error()
            })?;
            track.over.store(true, Ordering::Release);
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
        index: None,
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

/// A still picture as it is sent: its bytes, MIME type and size in pixels.
type Still = (Vec<u8>, String, Option<(u32, u32)>);

/// What a still picture is sent as: its bytes, MIME type and size in pixels, through
/// [`kourier_media::clean_still`] (LOC, F14: the reason Signal states `cleaned_pictures`): the
/// picture is re-encoded at [`PICTURE_LEVEL`] without any metadata block, or stripped when it
/// does not re-encode. A type that function does not clean (HEIC, by its magic) keeps its bytes
/// and the `mime` and `dims` the core gave it: the core's verdict asked for exactly those. A
/// picture of a cleaned type that cannot be cleaned fails its attachment (log-safe).
fn encode_still(
    original: &[u8],
    mime: &str,
    dims: Option<(u32, u32)>,
) -> Result<Still, BackendError> {
    match kourier_media::clean_still(original, PICTURE_LEVEL) {
        Ok(encoded) => Ok((
            encoded.bytes,
            encoded.mime.to_owned(),
            Some((encoded.width, encoded.height)),
        )),
        Err(kourier_media::NotCleaned::NotMine) => {
            tracing::info!("picture of a type not cleaned here; sent as it is");
            Ok((original.to_vec(), mime.to_owned(), dims))
        }
        Err(e @ kourier_media::NotCleaned::Refused) => {
            tracing::warn!(error = %e, "picture not sent");
            Err(BackendError::new(ErrorKind::InvalidInput, e.to_string()))
        }
    }
}

/// Re-encodes a still image ([`encode_still`]; blocking pool); the result waits for its upload
/// in a temporary encrypted file of `temp` (never plaintext on disk, and one picture in memory
/// at a time).
async fn reencoded(spec: Spec, temp: Arc<TempFiles>) -> Result<Spec, BackendError> {
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
        let encoded = encode_still(&original, &spec.mime, spec.dims);
        drop(original);
        let (bytes, mime, dims) = encoded?;
        let bytes = Zeroizing::new(bytes);
        let (path, key, size) = temp.write(&bytes)?;
        Ok(Spec {
            plain: Plaintext::Blob { path, key, size },
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

/// Reports the uploads of an attempt every [`PROGRESS_EVERY`] while they move (aborted when
/// done): each attachment's `AttachmentProgress` (the first report of every upload comes at the
/// first tick after its size is final; later ones only when its bytes moved; an upload that is
/// over before a tick is told full by [`Uploader::report_files_full`]), then
/// `TransmitProgress` with the share of blob bytes sent over all blob bytes, never 1.0 from
/// here: `run` reports that once every upload is done.
async fn report_progress<T: SignalTransport>(
    shared: Rc<Shared<T>>,
    local: LocalMsgId,
    tracks: Tracks,
) {
    // The files are being made ready from the attempt's start: said at once, not after a tick.
    let mut last: Option<(f32, SendPhase)> = None;
    if tracks.borrow().is_empty() {
        last = Some((0.0, SendPhase::Preparing));
        if shared
            .emit(BackendEvent::TransmitProgress {
                local,
                progress: 0.0,
                phase: Some(SendPhase::Preparing),
            })
            .await
            .is_err()
        {
            return;
        }
    }
    loop {
        tokio::time::sleep(PROGRESS_EVERY).await;
        let uploads: Vec<Arc<Track>> = tracks.borrow().clone();
        for track in &uploads {
            let Some(index) = track.index else { continue };
            let done = track.plain_done();
            if track.reported.swap(done, Ordering::Relaxed) == done {
                continue;
            }
            let event = BackendEvent::AttachmentProgress {
                local,
                index,
                done,
                total: track.plain,
            };
            if shared.emit(event).await.is_err() {
                return;
            }
        }
        let (done, total) = uploads.iter().fold((0u64, 0u64), |(done, total), track| {
            (done + track.blob_done(), total + track.blob)
        });
        let progress = if total == 0 {
            0.0
        } else {
            (done as f32 / total as f32).min(0.99)
        };
        // The phase ends with the preparation: the uploads are known when `tracks` holds them.
        let phase = if uploads.is_empty() {
            SendPhase::Preparing
        } else {
            SendPhase::Uploading
        };
        if last == Some((progress, phase)) {
            continue;
        }
        last = Some((progress, phase));
        if shared
            .emit(BackendEvent::TransmitProgress {
                local,
                progress,
                phase: Some(phase),
            })
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

    use std::path::PathBuf;

    use kourier_model::BlobKey;

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

    /// The ticker's events from `core`: `(AttachmentProgress { index, done, total }, share)`
    /// pairs flattened into two lists, in arrival order.
    fn drain(
        core: &mut tokio::sync::mpsc::Receiver<kourier_backend_api::Envelope>,
        local: LocalMsgId,
        files: &mut Vec<(u16, u64, u64)>,
        shares: &mut Vec<f32>,
    ) {
        let mut phases = Vec::new();
        drain_phases(core, local, files, shares, &mut phases);
    }

    /// [`drain`], and the phase of each `TransmitProgress` in `phases`, in the same order as
    /// `shares`.
    fn drain_phases(
        core: &mut tokio::sync::mpsc::Receiver<kourier_backend_api::Envelope>,
        local: LocalMsgId,
        files: &mut Vec<(u16, u64, u64)>,
        shares: &mut Vec<f32>,
        phases: &mut Vec<Option<SendPhase>>,
    ) {
        while let Ok(envelope) = core.try_recv() {
            assert!(envelope.ack.is_none(), "progress is ephemeral");
            for event in envelope.events {
                match event {
                    BackendEvent::AttachmentProgress {
                        local: l,
                        index,
                        done,
                        total,
                    } => {
                        assert_eq!(l, local);
                        files.push((index, done, total));
                    }
                    BackendEvent::TransmitProgress {
                        local: l,
                        progress,
                        phase,
                    } => {
                        assert_eq!(l, local);
                        shares.push(progress);
                        phases.push(phase);
                    }
                    other => panic!("{other:?}"),
                }
            }
        }
    }

    /// A track by hand: `plain` plaintext bytes that make a blob of `blob` bytes.
    fn track(index: Option<u16>, plain: u64, blob: u64, over: bool) -> Arc<Track> {
        Arc::new(Track {
            index,
            plain,
            blob,
            sent: Arc::new(AtomicU64::new(0)),
            over: AtomicBool::new(over),
            reported: AtomicU64::new(NOT_REPORTED),
        })
    }

    /// `TransmitProgress` follows the blob bytes sent: one event per change, as a share of the
    /// total, and never 1.0 from the ticker (that one marks the finished uploads). Before the
    /// uploads are known (the pictures are being re-encoded) it says `Preparing` at 0 (once, at
    /// the start: the line "Preparing files" and the X wait for it) and no file; the phase
    /// turns to `Uploading` in the very tick in which the per-file reports at 0 begin (upload
    /// progress, fix U3).
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
                let tracks: Tracks = Rc::new(RefCell::new(Vec::new()));
                let ticker =
                    tokio::task::spawn_local(report_progress(shared, local, tracks.clone()));
                let (mut files, mut shares, mut phases) = (Vec::new(), Vec::new(), Vec::new());
                let tick = PROGRESS_EVERY + Duration::from_millis(60);
                // Preparing: no upload known yet. The phase is said at once and not repeated.
                tokio::task::yield_now().await;
                drain_phases(&mut core, local, &mut files, &mut shares, &mut phases);
                assert_eq!(
                    (shares.as_slice(), phases.as_slice()),
                    ([0.0].as_slice(), [Some(SendPhase::Preparing)].as_slice()),
                    "said before the first tick"
                );
                tokio::time::sleep(tick * 2).await;
                drain_phases(&mut core, local, &mut files, &mut shares, &mut phases);
                assert!(files.is_empty(), "{files:?}");
                assert_eq!(shares, [0.0], "and not said again");
                // A blob of 1000 bytes for 800 plaintext bytes.
                let upload = track(Some(0), 800, 1000, false);
                tracks.borrow_mut().push(upload.clone());
                // Unchanged (250 twice) is not reported again; past the total is capped.
                for bytes in [0u64, 250, 250, 900, 5000] {
                    upload.sent.store(bytes, Ordering::Relaxed);
                    tokio::time::sleep(tick).await;
                    drain_phases(&mut core, local, &mut files, &mut shares, &mut phases);
                    if bytes == 0 {
                        assert_eq!(
                            (files.as_slice(), phases.last()),
                            ([(0, 0, 800)].as_slice(), Some(&Some(SendPhase::Uploading))),
                            "the file's first report and the phase turn in one tick"
                        );
                    }
                }
                ticker.abort();
                assert_eq!(shares, [0.0, 0.0, 0.25, 0.9, 0.99]);
                assert_eq!(
                    phases,
                    [
                        Some(SendPhase::Preparing),
                        Some(SendPhase::Uploading),
                        Some(SendPhase::Uploading),
                        Some(SendPhase::Uploading),
                        Some(SendPhase::Uploading)
                    ]
                );
                assert_eq!(
                    files,
                    [(0, 0, 800), (0, 200, 800), (0, 720, 800), (0, 799, 800)],
                    "the plaintext share of the blob; the last byte only when the upload is over"
                );
            })
            .await;
    }

    /// Each attachment reports itself: its position, its own bytes, a total that is final from
    /// the first report; a reused pointer is full in its first report; the long text's file is
    /// never reported but counts in the share; `done == total` only once an upload is over; the
    /// share never goes down and stays below 1.
    #[tokio::test(flavor = "current_thread")]
    async fn each_upload_reports_its_own_file_and_ends_full() {
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
                let local = kourier_model::MessageId(9);
                let tracks: Tracks = Rc::new(RefCell::new(Vec::new()));
                let (text, picture, reused, file) = (
                    track(None, 600, 1000, false),
                    track(Some(0), 4000, 5000, false),
                    track(Some(1), 3000, 4000, true),
                    track(Some(2), 7000, 8000, false),
                );
                // Every size is final when the uploads are shown to the ticker.
                tracks.borrow_mut().extend([
                    text.clone(),
                    picture.clone(),
                    reused.clone(),
                    file.clone(),
                ]);
                let ticker =
                    tokio::task::spawn_local(report_progress(shared, local, tracks.clone()));
                let (mut files, mut shares) = (Vec::new(), Vec::new());
                let tick = PROGRESS_EVERY + Duration::from_millis(60);
                tokio::time::sleep(tick).await;
                drain(&mut core, local, &mut files, &mut shares);
                assert_eq!(
                    files,
                    [(0, 0, 4000), (1, 3000, 3000), (2, 0, 7000)],
                    "every upload at once; the reused pointer is full; the text is not reported"
                );
                // The text, then the picture, then the file go up one after another.
                for (upload, bytes) in [
                    (&text, 1000),
                    (&picture, 2500),
                    (&picture, 5000),
                    (&file, 6000),
                    (&file, 8000),
                ] {
                    upload.sent.store(bytes, Ordering::Relaxed);
                    tokio::time::sleep(tick).await;
                    drain(&mut core, local, &mut files, &mut shares);
                }
                // The blob of the last upload is queued; its pointer is not there yet.
                assert_eq!(files.last(), Some(&(2, 6999, 7000)));
                for upload in [&text, &picture, &file] {
                    upload.over.store(true, Ordering::Release);
                }
                tokio::time::sleep(tick).await;
                drain(&mut core, local, &mut files, &mut shares);
                ticker.abort();
                for index in 0..3u16 {
                    let mine: Vec<(u64, u64)> = files
                        .iter()
                        .filter(|(i, ..)| *i == index)
                        .map(|(_, done, total)| (*done, *total))
                        .collect();
                    let total = mine[0].1;
                    assert!(
                        mine.iter().all(|(_, t)| *t == total),
                        "{index}: the total is final from the first report: {mine:?}"
                    );
                    assert!(
                        mine.windows(2).all(|w| w[0].0 <= w[1].0),
                        "{index}: never down: {mine:?}"
                    );
                    assert_eq!(mine.last(), Some(&(total, total)), "{index}: ends full");
                    assert!(
                        mine[..mine.len() - 1].iter().all(|(done, _)| *done < total),
                        "{index}: full only at its end: {mine:?}"
                    );
                }
                assert!(
                    shares.windows(2).all(|w| w[0] <= w[1]),
                    "never down: {shares:?}"
                );
                assert!(shares.iter().all(|share| *share < 1.0), "{shares:?}");
                assert_eq!(
                    shares.first(),
                    Some(&(4000.0_f32 / 18000.0)),
                    "the reused pointer's blob is 4000 of the 18000 bytes"
                );
                assert_eq!(shares.last(), Some(&0.99));
            })
            .await;
    }

    /// A transport whose chat socket hands out a TUS upload form and whose CDN takes every body,
    /// holding each upload for `hold` after its body was read; it notes what the account's
    /// temporary directory holds when an upload arrives.
    struct StubCdn {
        hold: Duration,
        tmp: PathBuf,
        /// Per upload request: the files of `tmp` as `(length, starts as a kourier-blob)`.
        seen: std::sync::Mutex<Vec<Vec<(u64, bool)>>>,
        /// Told when an upload request has its body and holds.
        arrived: tokio::sync::Notify,
    }

    struct StubSocket;

    impl crate::net::ChatSocket for StubSocket {
        async fn request(
            &self,
            _request: crate::net::ChatRequest,
        ) -> Result<crate::net::ChatResponse, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn send_multi_recipient(
            &self,
            _send: crate::net::MultiRecipientSend,
        ) -> Result<crate::net::MultiRecipientOutcome, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn get_upload_form(
            &self,
            _upload_length: u64,
        ) -> Result<
            Result<crate::net::UploadForm, crate::net::UploadTooLarge>,
            crate::net::TransportError,
        > {
            Ok(Ok(crate::net::UploadForm {
                cdn: 3,
                key: "the-cdn-key".to_owned(),
                headers: Vec::new(),
                signed_upload_url: "https://cdn.example/attachments/".to_owned(),
            }))
        }

        async fn disconnect(&self) {}
    }

    impl SignalTransport for StubCdn {
        type Socket = StubSocket;

        async fn connect_auth(
            &self,
            _credentials: &crate::net::AuthCredentials,
        ) -> Result<
            crate::net::Connected<StubSocket, crate::net::ChatEvent>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn connect_unauth(
            &self,
        ) -> Result<
            crate::net::Connected<StubSocket, crate::net::ChatEvent>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn connect_provisioning(
            &self,
        ) -> Result<
            crate::net::Connected<StubSocket, crate::net::ProvisioningEvent>,
            crate::net::TransportError,
        > {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn https(
            &self,
            _request: crate::net::HttpsRequest,
        ) -> Result<crate::net::HttpsResponse, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn https_download(
            &self,
            _request: crate::net::HttpsRequest,
            _sink: crate::net::DownloadSink,
        ) -> Result<http::StatusCode, crate::net::TransportError> {
            Err(crate::net::TransportError::Disconnected)
        }

        async fn https_upload(
            &self,
            request: crate::net::UploadRequest,
        ) -> Result<crate::net::HttpsResponse, crate::net::TransportError> {
            if let Some(mut body) = request.body {
                while let Some(chunk) = body.chunks.recv().await {
                    chunk.map_err(|_| crate::net::TransportError::Disconnected)?;
                }
            }
            let held: Vec<(u64, bool)> = std::fs::read_dir(&self.tmp)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|entry| {
                            let bytes = std::fs::read(entry.path()).unwrap_or_default();
                            (bytes.len() as u64, bytes.starts_with(&kourier_blob::MAGIC))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.seen.lock().unwrap().push(held);
            self.arrived.notify_one();
            tokio::time::sleep(self.hold).await;
            Ok(crate::net::HttpsResponse {
                status: http::StatusCode::CREATED,
                headers: http::HeaderMap::new(),
                body: bytes::Bytes::new(),
            })
        }

        fn network_changed(&self) {}
    }

    /// What an `Uploader` test stands on: the account's shared state over the stub CDN with
    /// directories of its own, the core's end of the channel, and the files of the message.
    struct UploadWorld {
        _fixture: crate::receive::tests::Fixture,
        _dir: tempfile::TempDir,
        shared: Rc<Shared<StubCdn>>,
        core: tokio::sync::mpsc::Receiver<kourier_backend_api::Envelope>,
        cdn: Arc<StubCdn>,
        tmp: PathBuf,
    }

    fn upload_world(hold: Duration) -> UploadWorld {
        let fixture = crate::receive::tests::fixture();
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("upload-tmp");
        let cdn = Arc::new(StubCdn {
            hold,
            tmp: tmp.clone(),
            seen: std::sync::Mutex::new(Vec::new()),
            arrived: tokio::sync::Notify::new(),
        });
        let mut test = crate::actor::testing::shared_over(
            cdn.clone(),
            fixture.db.clone(),
            fixture.account.clone(),
            Vec::new(),
        );
        test.shared.blob_dir = dir.path().join("blobs");
        test.shared.upload_tmp = tmp.clone();
        UploadWorld {
            _fixture: fixture,
            _dir: dir,
            shared: Rc::new(test.shared),
            core: test.core,
            cdn,
            tmp,
        }
    }

    /// A core attachment: a sealed file of `bytes` in the account's blob directory.
    fn core_attachment(
        world: &UploadWorld,
        rel: &str,
        kind: AttachmentKind,
        mime: &str,
        bytes: &[u8],
    ) -> Attachment {
        let key = kourier_blob::FileKey::generate();
        let path = world.shared.blob_dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        kourier_blob::write_all(&path, &key, bytes).unwrap();
        Attachment {
            kind,
            mime: mime.to_owned(),
            size: bytes.len() as u64,
            local: Some(BlobRef {
                path: rel.into(),
                key: BlobKey::new(*key.as_bytes()),
            }),
            ..Attachment::default()
        }
    }

    /// The files of the account's temporary directory.
    fn tmp_files(dir: &Path) -> usize {
        std::fs::read_dir(dir).map_or(0, Iterator::count)
    }

    /// A message of a picture to re-encode, a picture a recent upload left a pointer for, and a
    /// file: every size is final before the first byte goes up, the re-encoded picture waits in a
    /// sealed temporary file (the only file of the directory when its upload starts), the files
    /// are gone when the attempt ends, and the picture's first report already carries its
    /// RE-ENCODED size (not the size of the file it came from).
    #[tokio::test(flavor = "current_thread")]
    async fn a_picture_is_re_encoded_first_into_a_temporary_sealed_file() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut world = upload_world(Duration::from_millis(400));
                let original =
                    kourier_media::sample_image(2400, 1800, 3, kourier_media::SampleFormat::Png)
                        .unwrap();
                let re_encoded = encode_still(&original, "image/png", None).unwrap().0.len() as u64;
                assert_ne!(re_encoded, original.len() as u64);
                let picture = core_attachment(
                    &world,
                    "core/aa/pic",
                    AttachmentKind::Image,
                    "image/png",
                    &original,
                );
                let file = core_attachment(
                    &world,
                    "core/aa/doc",
                    AttachmentKind::File,
                    "application/pdf",
                    &vec![7u8; 90_000],
                );
                // A picture a recent upload left a pointer for: nothing to re-encode or upload.
                let mut reused = core_attachment(
                    &world,
                    "core/aa/old",
                    AttachmentKind::Image,
                    "image/png",
                    &[1; 10],
                );
                let local = reused.local.as_ref().unwrap();
                let source = upload::source_key(
                    PICTURE_TRANSFORM,
                    &[
                        local.path.as_os_str().as_encoded_bytes(),
                        &local.key.expose()[..],
                    ],
                );
                let remembered = proto::AttachmentPointer {
                    size: Some(123_456),
                    cdn_number: Some(3),
                    digest: Some(vec![5; 32]),
                    key: Some(vec![6; 64]),
                    ..Default::default()
                };
                let now = world.shared.now();
                world
                    .shared
                    .db
                    .zone(|tx| upload::remember(tx, &source, &remembered, now))
                    .unwrap();
                reused.size = 10;
                let body = Body::Media {
                    attachments: vec![picture, reused, file],
                    caption: None,
                };
                let local_id = kourier_model::MessageId(11);
                let socket = Arc::new(StubSocket);
                let stop = Stop::open();
                let uploader = Uploader {
                    shared: &world.shared,
                    socket: &socket,
                    local: local_id,
                    net_local: "1700000000000",
                    stop: &stop,
                };
                let prepared = uploader.prepare(&body).await.expect("prepare");
                assert_eq!(prepared.attachments.len(), 3);
                assert_eq!(
                    prepared.attachments[0].size,
                    Some(re_encoded as u32),
                    "the picture went up as re-encoded"
                );
                assert_eq!(
                    prepared.attachments[1].size,
                    Some(123_456),
                    "the reused pointer"
                );
                assert_eq!(tmp_files(&world.tmp), 0, "the attempt's files went with it");

                // What the CDN saw when the picture's upload arrived: ONE sealed file, the
                // re-encoded picture (the file is read in place and the reused one not at all).
                let seen = world.cdn.seen.lock().unwrap().clone();
                assert_eq!(seen.len(), 2, "two uploads: the picture and the file");
                assert_eq!(
                    seen[0],
                    [(kourier_blob::sealed_len(re_encoded), true)],
                    "a sealed temporary file of the re-encoded picture"
                );
                assert_eq!(seen[1], [(kourier_blob::sealed_len(re_encoded), true)]);

                let mut firsts: Vec<Option<(u64, u64)>> = vec![None; 3];
                let mut ended = None;
                while let Ok(envelope) = world.core.try_recv() {
                    for event in envelope.events {
                        match event {
                            BackendEvent::AttachmentProgress {
                                index, done, total, ..
                            } => {
                                firsts[usize::from(index)].get_or_insert((done, total));
                            }
                            BackendEvent::TransmitProgress {
                                progress, phase, ..
                            } => {
                                ended = Some((progress, phase));
                            }
                            other => panic!("{other:?}"),
                        }
                    }
                }
                // The first report comes at the first tick, when the picture's bytes may be
                // under way already: its TOTAL is what must be final.
                assert_eq!(
                    firsts[0].map(|(_, total)| total),
                    Some(re_encoded),
                    "the picture's total is its re-encoded size from the first report"
                );
                assert_eq!(
                    firsts[1],
                    Some((123_456, 123_456)),
                    "the reused pointer is full at once"
                );
                assert_eq!(firsts[2].map(|(_, total)| total), Some(90_000));
                assert_eq!(ended, Some((1.0, None)), "the uploads are over");
            })
            .await;
    }

    /// An upload that is over before the first tick (250 ms) is still reported: every file that
    /// uploaded ends with `done == total`, said before the uploads are reported over. (The
    /// ticker alone says nothing for a quick upload, and the UI would hear of no file.)
    #[tokio::test(flavor = "current_thread")]
    async fn a_quick_upload_is_told_full_before_the_uploads_are_over() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut world = upload_world(Duration::ZERO);
                let picture = core_attachment(
                    &world,
                    "core/aa/pic",
                    AttachmentKind::Image,
                    "image/png",
                    &kourier_media::sample_image(300, 200, 4, kourier_media::SampleFormat::Png)
                        .unwrap(),
                );
                let file = core_attachment(
                    &world,
                    "core/aa/doc",
                    AttachmentKind::File,
                    "application/pdf",
                    &vec![9u8; 5_000],
                );
                let body = Body::Media {
                    attachments: vec![picture, file],
                    caption: None,
                };
                let socket = Arc::new(StubSocket);
                let stop = Stop::open();
                let uploader = Uploader {
                    shared: &world.shared,
                    socket: &socket,
                    local: kourier_model::MessageId(13),
                    net_local: "1700000000002",
                    stop: &stop,
                };
                let started = tokio::time::Instant::now();
                uploader.prepare(&body).await.expect("prepare");
                assert!(
                    started.elapsed() < PROGRESS_EVERY,
                    "the ticker never ticked"
                );
                let mut events = Vec::new();
                while let Ok(envelope) = world.core.try_recv() {
                    events.extend(envelope.events);
                }
                let full: Vec<(usize, u16, u64, u64)> = events
                    .iter()
                    .enumerate()
                    .filter_map(|(at, e)| match e {
                        BackendEvent::AttachmentProgress {
                            index, done, total, ..
                        } => Some((at, *index, *done, *total)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    full.iter().map(|f| (f.1, f.2 == f.3)).collect::<Vec<_>>(),
                    [(0, true), (1, true)],
                    "each file once, full: {events:?}"
                );
                assert_eq!(full[1].3, 5_000, "the file's total is its size");
                let over = events
                    .iter()
                    .position(|e| {
                        matches!(
                            e,
                            BackendEvent::TransmitProgress {
                                progress, phase: None, ..
                            } if *progress == 1.0
                        )
                    })
                    .expect("the uploads are reported over");
                assert!(
                    full.iter().all(|f| f.0 < over),
                    "the files are told full before the uploads are over: {events:?}"
                );
            })
            .await;
    }

    /// An attempt that is dropped in the middle of an upload (the task is aborted) leaves no
    /// temporary file behind.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dropped_attempt_leaves_no_temporary_file() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let world = upload_world(Duration::from_secs(60));
                let original =
                    kourier_media::sample_image(1200, 900, 5, kourier_media::SampleFormat::Png)
                        .unwrap();
                let picture = core_attachment(
                    &world,
                    "core/aa/pic",
                    AttachmentKind::Image,
                    "image/png",
                    &original,
                );
                let body = Body::Media {
                    attachments: vec![picture],
                    caption: None,
                };
                let socket = Arc::new(StubSocket);
                let stop = Stop::open();
                let uploader = Uploader {
                    shared: &world.shared,
                    socket: &socket,
                    local: kourier_model::MessageId(12),
                    net_local: "1700000000001",
                    stop: &stop,
                };
                let attempt = Box::pin(uploader.prepare(&body));
                let mut attempt = Some(attempt);
                tokio::select! {
                    _ = attempt.as_mut().unwrap() => panic!("the held upload ended"),
                    () = world.cdn.arrived.notified() => {}
                }
                assert_eq!(tmp_files(&world.tmp), 1, "the picture waits for its upload");
                drop(attempt.take());
                assert_eq!(tmp_files(&world.tmp), 0, "dropped with its attempt");
            })
            .await;
    }

    /// The progress ticker ends with the upload it reports: an upload that is dropped (the
    /// attempt was stopped by the user) leaves no task that wakes four times a second for as
    /// long as the actor lives. The ticker holds the actor's shared state, so it is seen there.
    #[tokio::test(flavor = "current_thread")]
    async fn no_ticker_survives_a_dropped_upload() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let world = upload_world(Duration::from_secs(60));
                let doc = core_attachment(
                    &world,
                    "core/aa/doc",
                    AttachmentKind::File,
                    "application/pdf",
                    &[3u8; 90_000],
                );
                let body = Body::Media {
                    attachments: vec![doc],
                    caption: None,
                };
                let socket = Arc::new(StubSocket);
                let stop = Stop::open();
                let uploader = Uploader {
                    shared: &world.shared,
                    socket: &socket,
                    local: kourier_model::MessageId(13),
                    net_local: "1700000000002",
                    stop: &stop,
                };
                assert_eq!(Rc::strong_count(&world.shared), 1);
                let mut attempt = Some(Box::pin(uploader.prepare(&body)));
                tokio::select! {
                    _ = attempt.as_mut().unwrap() => panic!("the held upload ended"),
                    () = world.cdn.arrived.notified() => {}
                }
                assert_eq!(
                    Rc::strong_count(&world.shared),
                    2,
                    "the ticker of the running upload holds the state"
                );
                drop(attempt.take());
                // The aborted task is dropped when the set polls it next.
                tokio::time::sleep(Duration::from_millis(50)).await;
                assert_eq!(
                    Rc::strong_count(&world.shared),
                    1,
                    "no ticker is left behind by the dropped upload"
                );
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
            let (bytes, mime, dims) = encode_still(&photo, "image/png", None).unwrap();
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
        let (bytes, mime, dims) = encode_still(&small, "image/png", None).unwrap();
        assert_eq!((mime.as_str(), dims), ("image/png", Some((320, 200))));
        assert!(kourier_media::decode(&bytes).is_ok());
        // Not a picture Kourier decodes: the bytes, MIME type and size it came with.
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0mif1heic not really a picture".to_vec();
        let (bytes, mime, dims) = encode_still(&heic, "image/heic", Some((4032, 3024))).unwrap();
        assert_eq!(bytes, heic);
        assert_eq!((mime.as_str(), dims), ("image/heic", Some((4032, 3024))));
        // A located HEIC too: `clean_still` does not clean it, the core asked (LOC N7).
        let located = kourier_media::sample_located(kourier_media::SampleLocated::Heic);
        let (bytes, _, _) = encode_still(&located, "image/heic", None).unwrap();
        assert_eq!(bytes, located);
        // A JPEG that neither re-encodes nor strips fails its attachment, log-safe.
        let broken = b"\xff\xd8\xff\xe1\0\x08Exif\0\0not a marker".to_vec();
        let e = encode_still(&broken, "image/jpeg", None).unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidInput);
        assert!(!format!("{e:?}").contains("not a marker"));
        // The upload cache keeps levels apart, and pointers from before `clean_still` (N6).
        assert_eq!(PICTURE_TRANSFORM, "image-level-3-clean");
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
